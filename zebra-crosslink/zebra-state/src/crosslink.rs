//! Types & commands for crosslink

use std::fmt;

use zebra_chain::block::Height as BlockHeight;

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
    /// Write the best chain from genesis to a height (the best tip if `None`) to a chain prefix
    /// file in this node's export directory, with the BFT blocks `bft` selects.
    ExportChainPrefix {
        /// The last PoW height to include; the best tip if `None`.
        max_height: Option<BlockHeight>,
        /// Which decided BFT blocks to include.
        bft: crate::BftCutoff,
    },
}

/// Types of responses that can be returned by the TFLService.
///
/// These map one to one to the variants of the same name in [`TFLServiceRequest`].
#[derive(Debug)]
pub enum TFLServiceResponse {
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
    /// What was written for [`TFLServiceRequest::ExportChainPrefix`], or why nothing was
    ExportChainPrefix(Result<ChainPrefixExport, String>),
}

/// A chain prefix file that was written.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChainPrefixExport {
    /// Where the file is, on the node's machine.
    pub path: String,
    /// The height of the last PoW block in the file.
    pub tip_height: u32,
    /// The hash of the last PoW block in the file.
    pub tip_hash: String,
    /// How many PoW blocks the file holds, genesis included.
    pub pow_blocks: usize,
    /// How many decided BFT blocks the file holds.
    pub bft_blocks: usize,
    /// The rule that chose the BFT blocks: `cited`, `supported`, `none`, or a count.
    pub bft_cutoff: String,
    /// The file's size.
    pub bytes: usize,
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
