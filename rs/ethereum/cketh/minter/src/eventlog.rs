use crate::eth_logs::ReceivedEthEvent;
use crate::eth_rpc::Hash;
use crate::lifecycle::{init::InitArg, upgrade::UpgradeArg};
use crate::numeric::{BlockNumber, LedgerBurnIndex, LedgerMintIndex, LogIndex};
use crate::transactions::EthWithdrawalRequest;
use crate::tx::SignedEip1559TransactionRequest;
use minicbor::{Decode, Encode};

/// The event descibing the ckETH minter state transition.
#[derive(Clone, Debug, Encode, Decode)]
pub enum EventType {
    /// The minter initialization event.
    /// Must be the first event in the log.
    #[n(0)]
    Init(#[n(0)] InitArg),
    /// The minter upgraded with the specified arguments.
    #[n(1)]
    Upgrade(#[n(0)] UpgradeArg),
    /// The minter discovered a ckETH deposit in the helper contract logs.
    #[n(2)]
    AcceptedDeposit(#[n(0)] ReceivedEthEvent),
    /// The minter discovered an invalid ckETH deposit in the helper contract logs.
    #[n(4)]
    InvalidDeposit {
        /// The transaction hash of the deposit.
        #[n(0)]
        txhash: Hash,
        /// The log index of the event within the block.
        #[cbor(n(1))]
        log_index: LogIndex,
        /// The reason why minter considers the deposit invalid.
        #[n(2)]
        reason: String,
    },
    /// The minter minted ckETH in response to a deposit.
    #[n(5)]
    MintedCkEth {
        /// The transaction hash of the deposit event.
        #[n(0)]
        txhash: Hash,
        /// The log index of the deposit event.
        #[cbor(n(1))]
        log_index: LogIndex,
        /// The transaction index on the ckETH ledger.
        #[cbor(n(2), with = "crate::cbor::id")]
        mint_block_index: LedgerMintIndex,
    },
    /// The minter processed the helper smart contract logs up to the specified hight.
    #[n(6)]
    SyncedToBlock {
        /// The last processed block number (inclusive).
        #[n(0)]
        block_number: BlockNumber,
    },
    /// The minter accepted a new ETH withdrawal request.
    #[n(7)]
    AcceptedEthWithdrawalRequest(#[n(0)] EthWithdrawalRequest),
    /// The minter signed a transaction.
    #[n(8)]
    SignedTx {
        /// The withdrawal identifier.
        #[cbor(n(0), with = "crate::cbor::id")]
        withdrawal_id: LedgerBurnIndex,
        /// The signed transaction.
        #[n(1)]
        tx: SignedEip1559TransactionRequest,
    },
    /// The minter sent the transaction to the Ethereum network.
    #[n(9)]
    SentTransaction {
        /// The withdrawal identifier.
        #[cbor(n(0), with = "crate::cbor::id")]
        withdrawal_id: LedgerBurnIndex,
        #[n(1)]
        txhash: Hash,
    },
    /// The minter observed the transaction being included in a finalized Ethereum block.
    #[n(10)]
    FinalizedTransaction {
        /// The withdrawal identifier.
        #[cbor(n(0), with = "crate::cbor::id")]
        withdrawal_id: LedgerBurnIndex,
        #[n(1)]
        txhash: Hash,
    },
}

#[derive(Encode, Decode)]
pub struct Event {
    /// The caniter time at which the minter generated this event.
    #[n(0)]
    pub timestamp: u64,
    /// The event type.
    #[n(1)]
    pub event_type: EventType,
}
