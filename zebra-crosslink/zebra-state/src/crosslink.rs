//! Types & commands for crosslink

use std::fmt;

use tokio::sync::broadcast;

use zebra_chain::block::{Hash as BlockHash, Height as BlockHeight};

use serde_with::serde_as;

pub use zcash_primitives::bft::{FinalizerRecencyStatus, TFLRecencyStatus, ScanInfo};
use zcash_primitives::transaction::StakingActionRequest;

/// The finality status of a block
#[derive(Debug, PartialEq, Eq, Clone, serde::Serialize, serde::Deserialize)]
pub enum TFLBlockFinality {
    // TODO: rename?
    /// The block height is above the finalized height, so it's not yet determined
    /// whether or not it will be finalized.
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
    /// Set final block hash
    SetFinalBlockHash(BlockHash),
    /// Get the finality status of a block
    BlockFinalityStatus(BlockHeight, BlockHash),
    /// Get the finality status of a transaction
    TxFinalityStatus(zebra_chain::transaction::Hash),
    /// Get the finalizer roster
    Roster,
    /// Get the fat pointer to the BFT chain tip, suitable for a PoW block at the given height.
    /// The handler walks back from the tip to find the most recent BFT block whose
    /// `do_not_include_until_bc_height` is <= the proposed PoW block height.
    FatPointerToBFTChainTip(u64),
    /// Send a staking command transaction
    StakingCmd(String),
    /// faucet
    Faucet(String),
    /// For crosslink testnet 1
    TotalIssuanceFromKey(Vec<zcash_keys::keys::UnifiedFullViewingKey>, BlockHeight, BlockHeight),
    /// Finalizer recency status
    FinalizersRecencyStatus,
    /// Get UFVK for wallet
    WalletUfvk,
    /// Send staking action from wallet
    WalletStakingAction(StakingActionRequest),
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
    /// Set final block hash
    SetFinalBlockHash(Option<BlockHeight>),
    /// Finality status of a block
    BlockFinalityStatus(Option<TFLBlockFinality>),
    /// Finality status of a transaction
    TxFinalityStatus(Option<TFLBlockFinality>),
    /// Finalizer roster
    Roster(Vec<zcash_primitives::transaction::RosterMember>),
    /// Fat pointer to the BFT chain tip
    FatPointerToBFTChainTip(zcash_primitives::bft::FatPointerToBftBlock),
    /// Send a staking command transaction
    StakingCmd,
    /// Faucet
    Faucet(Result<u64, String>),
    /// Response to [`ReadRequest::TotalIssuanceFromKey`]
    TotalIssuanceFromKey(Result<Vec<ScanInfo>, String>),
    /// Finalizer recency status + reference UTC
    FinalizersRecencyStatus(TFLRecencyStatus),
    /// Get UFVK for wallet
    WalletUfvk(Option<String>),
    /// Send staking action from wallet
    WalletStakingAction(Result<String, String>),
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
