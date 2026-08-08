//! Types & commands for crosslink

use std::fmt;

use tokio::sync::broadcast;

use zebra_chain::block::{Hash as BlockHash, Height as BlockHeight};

use serde_with::serde_as;

pub use zcash_primitives::bft::{FinalizerRecencyStatus, TFLRecencyStatus, ScanInfo};
use zcash_primitives::transaction::StakingActionRequest;

/// A cheap, point-in-time snapshot of the headless wallet's own note-scan progress and
/// balances. Updated as a handful of plain field assignments once per wallet loop iteration
/// (see `wallet::WALLET_SYNC_STATUS`), so reading it via RPC costs nothing beyond what the
/// wallet already computes for its own internal use -- unlike deriving this information from
/// the `DUMP_NOTES` debug log, which reprints the *entire* notes list from scratch every
/// iteration and does not scale as that list grows.
#[derive(Debug, Default, PartialEq, Eq, Clone, serde::Serialize, serde::Deserialize)]
pub struct WalletSyncStatusSnapshot {
    /// The highest block height the headless wallet's own note-scan has processed so far.
    /// Distinct from the raw node's chain-sync height (see `getblockchaininfo`) -- this can
    /// lag behind it, particularly right after a restart, since wallet note-scanning is
    /// in-memory only and always restarts from genesis.
    pub sync_height: u32,
    /// The chain tip height the wallet is scanning towards, as of the same snapshot.
    pub tip_height: u32,
    /// The user wallet's confirmed, spendable (not staked, not pending) balance, in zatoshis.
    pub user_shielded_spendable_zats: u64,
    /// The user wallet's unconfirmed/pending shielded balance, in zatoshis.
    pub user_shielded_pending_zats: u64,
    /// The user wallet's transparent balance, in zatoshis.
    pub user_unshielded_zats: u64,
    /// Total currently staked (bonded) balance, in zatoshis.
    pub staked_zats: u64,
    /// Balance available to withdraw from completed unbonding, in zatoshis.
    pub withdrawable_zats: u64,
}

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
    /// Get the headless wallet's own note-scan progress and balances
    WalletSyncStatus,
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
    /// The headless wallet's own note-scan progress and balances
    WalletSyncStatus(WalletSyncStatusSnapshot),
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
