//! Types & commands for crosslink

use std::fmt;

use tokio::sync::broadcast;

use zebra_chain::block::{Hash as BlockHash, Height as BlockHeight};

use serde_with::serde_as;

pub use zcash_primitives::bft::{FinalizerRecencyStatus, TFLRecencyStatus, ScanBond, ScanInfo, WalletSpendableFunds, WalletStakingPositions};
use zcash_primitives::transaction::StakingActionRequest;

/// The finality status of a block
#[derive(Debug, PartialEq, Eq, Clone, serde::Serialize, serde::Deserialize)]
pub enum TFLBlockFinality {
    // TODO: rename?
    /// The block height is above the finalized height, or nothing is finalized yet, so it's
    /// not yet determined whether or not it will be finalized.
    NotYetFinalized,

    /// The block is finalized: it's height is below the finalized height and
    /// it is in the best chain.
    Finalized,

    /// The block cannot be finalized: it's height is below the finalized height and
    /// it is not in the best chain.
    CantBeFinalized,
}

/// Types of requests that can be made to the TFLService.
///
/// These map one to one to the variants of the same name in [`TFLServiceResponse`].
#[derive(Clone, Debug)]
pub enum TFLServiceRequest {
    /// Is the TFL service activated yet?
    IsTFLActivated,
    /// Get the final block hash
    FinalBlockHeightHash,
    /// Get a receiver for the final block hash
    FinalBlockRx,
    /// Get the finality status of a block
    BlockFinalityStatus(BlockHeight, BlockHash),
    /// Get the finality status of a transaction
    TxFinalityStatus(zebra_chain::transaction::Hash),
    /// Send a staking command transaction
    StakingCmd(String),
    /// faucet
    Faucet(String),
    /// For crosslink testnet 1
    TotalIssuanceFromKey(Vec<zcash_keys::keys::UnifiedFullViewingKey>, BlockHeight, BlockHeight),
    /// Get UFVK for wallet
    WalletUfvk,
    /// Send staking action from wallet
    WalletStakingAction(StakingActionRequest),
    /// Query wallet staking positions grouped by finalizer
    WalletStakingPositions,
    /// Query what the wallet can spend now, and its own address
    WalletSpendableFunds,
    /// Send a basic shielded value transfer from the wallet: (value in zatoshis, unified address)
    WalletBasicSend(u64, String),
}

/// Types of responses that can be returned by the TFLService.
///
/// These map one to one to the variants of the same name in [`TFLServiceRequest`].
#[derive(Debug)]
pub enum TFLServiceResponse {
    /// Is the TFL service activated yet?
    IsTFLActivated(bool),
    /// Final block hash
    FinalBlockHeightHash(Option<(BlockHeight, BlockHash)>),
    /// Receiver for the final block hash
    FinalBlockRx(broadcast::Receiver<(BlockHeight, BlockHash)>),
    /// Finality status of a block
    BlockFinalityStatus(Option<TFLBlockFinality>),
    /// Finality status of a transaction
    TxFinalityStatus(Option<TFLBlockFinality>),
    /// Send a staking command transaction
    StakingCmd,
    /// Faucet
    Faucet(Result<u64, String>),
    /// Response to [`ReadRequest::TotalIssuanceFromKey`]
    TotalIssuanceFromKey(Result<Vec<ScanInfo>, String>),
    /// Get UFVK for wallet
    WalletUfvk(Option<String>),
    /// Send staking action from wallet
    WalletStakingAction(Result<String, String>),
    /// Query wallet staking positions grouped by finalizer
    WalletStakingPositions(WalletStakingPositions),
    /// Query what the wallet can spend now; `None` until the wallet's first sync pass completes
    WalletSpendableFunds(Option<WalletSpendableFunds>),
    /// Send a basic shielded value transfer from the wallet
    WalletBasicSend(Result<String, String>),
}

/// Errors that can occur when interacting with the TFLService.
#[derive(Debug)]
pub enum TFLServiceError {
    /// Not implemented error
    NotImplemented,
    /// Arbitrary error
    Misc(String),
}

impl fmt::Display for TFLServiceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TFLServiceError: {:?}", self)
    }
}

use std::error::Error;
impl Error for TFLServiceError {}
