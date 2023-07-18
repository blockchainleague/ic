use std::{
    fs::File,
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{anyhow, Context, Error};
use axum::{
    body::Body,
    extract::MatchedPath,
    handler::Handler,
    http::{Request, Response, StatusCode, Uri},
    middleware::{self, Next},
    response::IntoResponse,
    routing::{delete, get, post, put},
    Extension, Router, Server,
};
use candid::Principal;
use chacha20poly1305::{KeyInit, XChaCha20Poly1305};
use clap::Parser;
use futures::future::TryFutureExt;
use ic_agent::{
    agent::http_transport::ReqwestHttpReplicaV2Transport, identity::Secp256k1Identity, Agent,
};
use instant_acme::{Account, AccountCredentials};
use opentelemetry::{
    global,
    metrics::{Counter, Histogram},
    sdk::{
        export::metrics::aggregation,
        metrics::{controllers, processors, selectors},
        Resource,
    },
    KeyValue,
};
use opentelemetry_prometheus::{ExporterBuilder, PrometheusExporter};
use prometheus::{Encoder as PrometheusEncoder, TextEncoder};
use tokio::{sync::Semaphore, task, time::sleep};
use tower::ServiceBuilder;
use tracing::info;
use trust_dns_resolver::{
    config::{ResolverConfig, ResolverOpts},
    TokioAsyncResolver,
};

use crate::{
    acme::Acme,
    acme_idna::WithIDNA,
    certificate::{
        CanisterCertGetter, CanisterExporter, CanisterUploader, Export, WithDecode, WithPagination,
        WithRetries, WithVerify,
    },
    check::{Check, Checker},
    cloudflare::Cloudflare,
    dns::Resolver,
    encode::{Decoder, Encoder},
    metrics::{MetricParams, WithMetrics},
    registration::{Create, Get, Remove, State, Update, UpdateType},
    verification::CertificateVerifier,
    work::{Dispense, DispenseError, Peek, PeekError, Process, Queue, WithDetectRenewal},
};

mod acme;
mod acme_idna;
mod api;
mod certificate;
mod check;
mod cloudflare;
mod dns;
mod encode;
mod metrics;
mod registration;
mod verification;
mod work;

const SERVICE_NAME: &str = "certificate-issuer";

#[derive(Parser)]
#[command(name = SERVICE_NAME)]
struct Cli {
    #[arg(long, default_value = "127.0.0.1:3000")]
    api_addr: SocketAddr,

    /// NNS public key
    #[clap(long)]
    root_key_path: Option<PathBuf>,

    #[clap(long, default_value = "identity.pem")]
    identity_path: PathBuf,

    #[arg(long, default_value = "http://127.0.0.1:8080/")]
    orchestrator_uri: Uri,

    #[arg(long)]
    orchestrator_canister_id: Principal,

    /// A symmetric key used to encrypt and/or decrypt certificates
    #[clap(long, default_value = "key.pem")]
    key_path: PathBuf,

    /// A domain clients are required to delegate their DNS-01 challenge to.
    #[arg(long)]
    delegation_domain: String,

    #[arg(long)]
    acme_account_id: String,

    #[arg(long)]
    acme_account_key: String,

    #[arg(long, default_value = "https://acme-v02.api.letsencrypt.org")]
    acme_provider_url: String,

    #[arg(long)]
    cloudflare_api_key: String,

    #[arg(long, default_value = "60")]
    peek_sleep_sec: u64,

    #[arg(long, default_value = "127.0.0.1:9090")]
    metrics_addr: SocketAddr,
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let cli = Cli::parse();

    // Logging
    let subscriber = tracing_subscriber::fmt()
        .json()
        .flatten_event(true)
        .finish();

    tracing::subscriber::set_global_default(subscriber)
        .context("failed to set global subscriber")?;

    // Metrics
    let exporter = ExporterBuilder::new(
        controllers::basic(
            processors::factory(
                selectors::simple::histogram([]),
                aggregation::cumulative_temporality_selector(),
            )
            .with_memory(true),
        )
        .with_resource(Resource::new(vec![KeyValue::new("service", SERVICE_NAME)]))
        .build(),
    )
    .init();

    let meter = global::meter(SERVICE_NAME);

    let metrics_handler = metrics_handler.layer(Extension(MetricsHandlerArgs { exporter }));
    let metrics_router = Router::new().route("/metrics", get(metrics_handler));

    // Orchestrator
    let agent = {
        static USER_AGENT: &str = "Ic-Certificate-Issuer";
        let client = reqwest::Client::builder().user_agent(USER_AGENT).build()?;

        let transport = ReqwestHttpReplicaV2Transport::create_with_client(
            cli.orchestrator_uri.to_string(),
            client,
        )?;

        let f = File::open(cli.identity_path).context("failed to open identity file")?;
        let identity = Secp256k1Identity::from_pem(f).context("failed to create basic identity")?;

        let agent = Agent::builder()
            .with_identity(identity)
            .with_transport(transport)
            .build()?;

        let root_key = cli
            .root_key_path
            .map(std::fs::read)
            .transpose()
            .context("failed to open root key")?;

        if let Some(root_key) = &root_key {
            agent.set_root_key(root_key.clone());
        }

        Arc::new(agent)
    };

    // DNS
    let resolver = Resolver(TokioAsyncResolver::tokio(
        ResolverConfig::default(),
        ResolverOpts::default(),
    )?);

    let resolver = WithMetrics(resolver, MetricParams::new(&meter, SERVICE_NAME, "resolve"));

    // Encryption
    let cipher = Arc::new({
        let f = std::fs::read(cli.key_path).context("failed to open key file")?;
        let p = pem::parse(f).context("failed to parse pem file")?;
        XChaCha20Poly1305::new_from_slice(&p.contents).context("failed to init symmetric key")?
    });

    let encoder = Encoder::new(cipher.clone());
    let encoder = WithMetrics(encoder, MetricParams::new(&meter, SERVICE_NAME, "encrypt"));
    let encoder = Arc::new(encoder);

    let decoder = Decoder::new(cipher.clone());
    let decoder = WithMetrics(decoder, MetricParams::new(&meter, SERVICE_NAME, "decrypt"));
    let decoder = Arc::new(decoder);

    // Registration
    let registration_checker = Checker::new(
        cli.delegation_domain.clone(),
        Box::new(resolver.clone()),
        agent.clone(),
    );
    let registration_checker = WithMetrics(
        registration_checker,
        MetricParams::new(&meter, SERVICE_NAME, "check_registration"),
    );
    let registration_checker = Arc::new(registration_checker);

    let registration_creator =
        registration::CanisterCreator(agent.clone(), cli.orchestrator_canister_id);
    let registration_creator = WithMetrics(
        registration_creator,
        MetricParams::new(&meter, SERVICE_NAME, "create_registration"),
    );
    let registration_creator = Arc::new(registration_creator);

    let registration_updater =
        registration::CanisterUpdater(agent.clone(), cli.orchestrator_canister_id);
    let registration_updater = WithMetrics(
        registration_updater,
        MetricParams::new(&meter, SERVICE_NAME, "update_registration"),
    );
    let registration_updater = Arc::new(registration_updater);

    let registration_remover =
        registration::CanisterRemover(agent.clone(), cli.orchestrator_canister_id);
    let registration_remover = WithMetrics(
        registration_remover,
        MetricParams::new(&meter, SERVICE_NAME, "remove_registration"),
    );
    let registration_remover = Arc::new(registration_remover);

    let registration_getter =
        registration::CanisterGetter(agent.clone(), cli.orchestrator_canister_id);
    let registration_getter = WithMetrics(
        registration_getter,
        MetricParams::new(&meter, SERVICE_NAME, "get_registration"),
    );
    let registration_getter = Arc::new(registration_getter);

    // Verifier
    let certificate_verifier =
        CertificateVerifier::new(agent.clone(), cli.orchestrator_canister_id);
    let certificate_verifier = WithMetrics(
        certificate_verifier,
        MetricParams::new(&meter, SERVICE_NAME, "verify_certificates"),
    );
    let certificate_verifier = Arc::new(certificate_verifier);

    // Certificates
    let certificate_getter =
        CanisterCertGetter::new(agent.clone(), cli.orchestrator_canister_id, decoder.clone());
    let certificate_getter = WithMetrics(
        certificate_getter,
        MetricParams::new(&meter, SERVICE_NAME, "get_certificate"),
    );
    let certificate_getter = Arc::new(certificate_getter);

    let certificate_exporter = CanisterExporter::new(agent.clone(), cli.orchestrator_canister_id);
    let certificate_exporter = WithVerify(certificate_exporter, certificate_verifier);
    let certificate_exporter = WithRetries(
        certificate_exporter,
        20, // Number of retries
    );
    let certificate_exporter = WithDecode(certificate_exporter, decoder);
    let certificate_exporter = WithMetrics(
        certificate_exporter,
        MetricParams::new(&meter, SERVICE_NAME, "export_certificates"),
    );
    let certificate_exporter = WithPagination(
        certificate_exporter,
        50, // Page Size
    );
    let certificate_exporter = Arc::new(certificate_exporter);

    let certificate_uploader =
        CanisterUploader::new(agent.clone(), cli.orchestrator_canister_id, encoder);
    let certificate_uploader = WithMetrics(
        certificate_uploader,
        MetricParams::new(&meter, SERVICE_NAME, "upload_certificate"),
    );

    // Work
    let queuer = work::CanisterQueuer(agent.clone(), cli.orchestrator_canister_id);
    let queuer = WithMetrics(queuer, MetricParams::new(&meter, SERVICE_NAME, "queue"));
    let queuer = Arc::new(queuer);

    // API
    let create_registration_handler = api::create_handler.layer(Extension({
        let v: (Arc<dyn Check>, Arc<dyn Create>, Arc<dyn Queue>) = (
            registration_checker.clone(), // checker
            registration_creator.clone(), // creator
            queuer.clone(),               // queuer
        );
        v
    }));

    let get_registration_handler = api::get_handler.layer(Extension({
        let v: Arc<dyn Get> = registration_getter.clone();
        v
    }));

    let update_registration_handler = api::update_handler.layer(Extension({
        let v: (Arc<dyn Check>, Arc<dyn Get>, Arc<dyn Update>) = (
            registration_checker.clone(), // checker
            registration_getter.clone(),  // getter
            registration_updater.clone(), // updater
        );
        v
    }));

    let remove_registration_handler = api::remove_handler.layer(Extension({
        let v: (Arc<dyn Check>, Arc<dyn Get>, Arc<dyn Remove>) = (
            registration_checker.clone(), // checker
            registration_getter.clone(),  // getter
            registration_remover.clone(), // remover
        );
        v
    }));

    let export_handler = api::export_handler.layer(Extension({
        let v: Arc<dyn Export> = certificate_exporter;
        v
    }));

    let api_router = Router::new()
        .route("/registrations", post(create_registration_handler))
        .route("/registrations/:id", get(get_registration_handler))
        .route("/registrations/:id", put(update_registration_handler))
        .route("/registrations/:id", delete(remove_registration_handler))
        .route("/certificates", get(export_handler));

    // API (Instrument)
    let api_router = api_router.layer(
        ServiceBuilder::new()
            .layer(Extension(MetricsMiddlewareArgs {
                counter: meter
                    .u64_counter("requests_total")
                    .with_description("Counts occurences of requests")
                    .init(),
                recorder: meter
                    .f64_histogram("request_duration")
                    .with_description("Duration of requests")
                    .init(),
            }))
            .layer(middleware::from_fn(metrics_mw)),
    );

    // ACME
    let Cli {
        acme_account_id,
        acme_account_key,
        acme_provider_url,
        ..
    } = cli;

    let acme_credentials: AccountCredentials = serde_json::from_str(&format!(
        r#"{{
            "id": "{acme_provider_url}/acme/acct/{acme_account_id}",
            "key_pkcs8": "{acme_account_key}",
            "urls": {{
                "newNonce": "{acme_provider_url}/acme/new-nonce",
                "newAccount": "{acme_provider_url}/acme/new-acct",
                "newOrder": "{acme_provider_url}/acme/new-order"
            }}
        }}"#,
    ))?;

    let acme_account = Account::from_credentials(acme_credentials)
        .context("failed to create acme account from credentials")?;

    let acme_client = Acme::new(acme_account);

    let acme_order = WithIDNA(acme_client.clone());
    let acme_order = WithMetrics(
        acme_order,
        MetricParams::new(&meter, SERVICE_NAME, "acme_create_order"),
    );

    let acme_ready = WithIDNA(acme_client.clone());
    let acme_ready = WithMetrics(
        acme_ready,
        MetricParams::new(&meter, SERVICE_NAME, "acme_ready_order"),
    );

    let acme_finalize = WithIDNA(acme_client.clone());
    let acme_finalize = WithMetrics(
        acme_finalize,
        MetricParams::new(&meter, SERVICE_NAME, "acme_finalize_order"),
    );

    // Cloudflare
    let dns_creator = Cloudflare::new(&cli.cloudflare_api_key)?;
    let dns_creator = WithMetrics(
        dns_creator,
        MetricParams::new(&meter, SERVICE_NAME, "dns_create"),
    );

    let dns_deleter = Cloudflare::new(&cli.cloudflare_api_key)?;
    let dns_deleter = WithMetrics(
        dns_deleter,
        MetricParams::new(&meter, SERVICE_NAME, "dns_delete"),
    );

    // Work
    let peeker = work::CanisterPeeker(agent.clone(), cli.orchestrator_canister_id);
    let peeker = WithMetrics(peeker, MetricParams::new(&meter, SERVICE_NAME, "peek"));

    let dispenser = work::CanisterDispenser(agent.clone(), cli.orchestrator_canister_id);
    let dispenser = WithMetrics(
        dispenser,
        MetricParams::new(&meter, SERVICE_NAME, "dispense"),
    );

    let processor = work::Processor::new(
        cli.delegation_domain,
        registration_checker.clone(),
        Box::new(resolver),
        Box::new(acme_order),
        Box::new(acme_ready),
        Box::new(acme_finalize),
        Box::new(dns_creator),
        Box::new(dns_deleter),
        Box::new(certificate_uploader),
    );
    let processor = WithMetrics(
        processor,
        MetricParams::new(&meter, SERVICE_NAME, "process"),
    );
    let processor = WithDetectRenewal::new(processor, certificate_getter.clone());
    let processor = Arc::new(processor);

    let sem = Arc::new(Semaphore::new(10));

    // Service
    info!(
        msg = format!("starting {SERVICE_NAME}").as_str(),
        metrics_addr = cli.metrics_addr.to_string().as_str(),
    );

    let _ = tokio::try_join!(
        task::spawn(async move {
            loop {
                let _permit = sem.clone().acquire_owned().await.unwrap();

                let processor = processor.clone();
                let queuer = queuer.clone();
                let registration_updater = registration_updater.clone();

                // First check with a query call if there's anything to dispense
                if let Err(err) = peeker.peek().await {
                    match err {
                        PeekError::NoTasksAvailable => {
                            sleep(Duration::from_secs(cli.peek_sleep_sec)).await;
                            continue;
                        }
                        PeekError::UnexpectedError(_) => {
                            sleep(Duration::from_secs(10)).await;
                            continue;
                        }
                    }
                };

                let (id, task) = match dispenser.dispense().await {
                    Ok((id, task)) => (id, task),
                    Err(DispenseError::NoTasksAvailable) => {
                        sleep(Duration::from_secs(cli.peek_sleep_sec)).await;
                        continue;
                    }
                    Err(DispenseError::UnexpectedError(_)) => {
                        sleep(Duration::from_secs(10)).await;
                        continue;
                    }
                };

                task::spawn(async move {
                    let _permit = _permit;

                    match processor.process(&id, &task).await {
                        Ok(()) => {
                            let d: Duration = Duration::from_secs(60 * 24 * 3600); // 60 days
                            let t = SystemTime::now().duration_since(UNIX_EPOCH)? + d;
                            let t = t.as_nanos() as u64;

                            // Schedule renewal
                            queuer
                                .queue(&id, t)
                                .await
                                .context("failed to queue task {id}")?;

                            registration_updater
                                .update(&id, &UpdateType::State(State::Available))
                                .await
                                .context("failed to update registration {id}")?;
                        }
                        Err(err) => {
                            let d: Duration = (&err).into();
                            let t = SystemTime::now().duration_since(UNIX_EPOCH)? + d;
                            let t = t.as_nanos() as u64;

                            // Schedule retry
                            queuer
                                .queue(&id, t)
                                .await
                                .context("failed to queue task {id}")?;

                            registration_updater
                                .update(&id, &UpdateType::State(err.into()))
                                .await
                                .context("failed to update registration {id}")?;
                        }
                    }

                    Ok::<_, Error>(())
                });
            }
        }),
        task::spawn(
            Server::bind(&cli.api_addr)
                .serve(api_router.into_make_service())
                .map_err(|err| anyhow!("server failed: {:?}", err))
        ),
        task::spawn(
            Server::bind(&cli.metrics_addr)
                .serve(metrics_router.into_make_service())
                .map_err(|err| anyhow!("server failed: {:?}", err))
        ),
    )
    .context(format!("{SERVICE_NAME} failed to run"))?;

    Ok(())
}

#[derive(Clone)]
struct MetricsHandlerArgs {
    exporter: PrometheusExporter,
}

async fn metrics_handler(
    Extension(MetricsHandlerArgs { exporter }): Extension<MetricsHandlerArgs>,
) -> Response<Body> {
    let metric_families = exporter.registry().gather();

    let encoder = TextEncoder::new();

    let mut metrics_text = Vec::new();
    if encoder.encode(&metric_families, &mut metrics_text).is_err() {
        return Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .body("Internal Server Error".into())
            .unwrap();
    };

    Response::builder()
        .status(200)
        .body(metrics_text.into())
        .unwrap()
}

#[derive(Clone)]
struct MetricsMiddlewareArgs {
    counter: Counter<u64>,
    recorder: Histogram<f64>,
}

async fn metrics_mw<B>(req: Request<B>, next: Next<B>) -> impl IntoResponse {
    let cx = opentelemetry::Context::current();

    let MetricsMiddlewareArgs { counter, recorder } = req
        .extensions()
        .get::<MetricsMiddlewareArgs>()
        .expect("missing metrics middleware args")
        .to_owned();

    let path = req
        .extensions()
        .get::<MatchedPath>()
        .expect("missing matched-path")
        .as_str()
        .to_owned();

    let method = req.method().as_str().to_owned();

    let start_time = Instant::now();

    let response = next.run(req).await;

    let request_duration = start_time.elapsed().as_secs_f64();
    let status_code = response.status().as_str().to_owned();

    info!(path, method, status_code, request_duration);

    let labels = &[
        KeyValue::new("path", path),
        KeyValue::new("method", method),
        KeyValue::new("status_code", status_code),
    ];

    counter.add(&cx, 1, labels);
    recorder.record(&cx, request_duration, labels);

    response
}
