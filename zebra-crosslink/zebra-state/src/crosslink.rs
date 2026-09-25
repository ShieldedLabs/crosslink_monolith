//! Types & commands for crosslink

use std::fmt;

use tokio::sync::broadcast;

use zebra_chain::block::{Hash as BlockHash, Height as BlockHeight};

use serde_with::serde_as;

pub use zcash_primitives::bft::{FinalizerRecencyStatus, TFLRecencyStatus, ScanBond, ScanInfo, WalletStakingPositions};
use zcash_primitives::transaction::StakingActionRequest;

/// How this node's BFT (finality) layer is doing, in one call.
///
/// Answers the operator question "is finality working, and if not, why not?" without
/// scraping the node log. Everything here is read from the live tenderlink state
/// snapshot the BFT loop publishes each tick, plus the node's own PoW tip, so it is
/// current to within one BFT tick.
///
/// ## Public keys
/// Finalizer keys in this and the other `get_tfl_*` diagnostic responses are hex in
/// **display order** -- the same orientation the node's log lines use (`Pub{093e}...`),
/// and the same as [`TFLRecencyStatus`]. Note that `get_tfl_roster_zats` reports the
/// *opposite* (raw) orientation for the same keys; these endpoints do the roster join
/// server-side precisely so that nobody has to reconcile the two by hand.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct TFLFinalityStatus {
    /// Reference time for every other absolute timestamp in this response.
    pub now_utc: i64,

    /// BFT height this node is currently trying to decide (the height *not yet* decided).
    pub bft_height: u64,
    /// Round within `bft_height`. A round above 0 means earlier rounds failed to decide.
    pub bft_round: u32,
    /// `"propose"`, `"prevote"` or `"precommit"`.
    pub bft_step: String,
    /// Round this node is locked on, or -1.
    pub bft_locked_round: i64,
    /// Round of this node's latest valid value, or -1.
    pub bft_valid_round: i64,
    /// Number of decided BFT blocks this node holds.
    pub bft_chain_len: u64,
    /// When this node last appended a decided BFT block.
    pub last_decision_utc: Option<i64>,
    /// Age of the last decision. Climbing without bound is the signature of a stall.
    pub secs_since_last_decision: Option<i64>,

    /// This node's PoW best-chain tip height.
    pub pow_tip_height: Option<u32>,
    /// Highest PoW height covered by BFT finality.
    pub finalized_height: Option<u32>,
    /// `pow_tip_height - finalized_height`: how far finality trails the PoW chain.
    pub finality_gap: Option<u32>,

    /// Active roster size.
    pub roster_n: usize,
    /// Total voting power in the active roster.
    pub total_power: u64,
    /// Voting power seen directly within the liveness window.
    pub online_power: u64,
    /// `online_power` as a percentage of `total_power`.
    pub online_power_pct: f64,
    /// Power needed to decide: `2f+1`, computed with the same `f` consensus uses.
    pub quorum_threshold: u64,
    /// Whether the power currently seen online could reach `quorum_threshold` at all.
    pub quorum_online: bool,

    /// True when nothing below flagged a problem.
    pub healthy: bool,
    /// Human-readable findings, most important first. Empty when `healthy`.
    pub diagnosis: Vec<String>,
}

/// One active-roster finalizer, with its liveness and its votes at this node's current
/// BFT height already joined together. See [`TFLFinalityStatus`] on key orientation.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct TFLQuorumMember {
    /// Finalizer public key, display order.
    pub pub_key: zcash_primitives::bft::PubKeyID,
    /// This finalizer's stake-weighted voting power.
    pub voting_power: u64,
    /// `voting_power` as a percentage of the active roster's total.
    pub power_pct: f64,
    /// Whether this is us.
    pub is_me: bool,
    /// Whether we have heard from this finalizer inside the liveness window.
    pub online: bool,
    /// When we last had a direct connection to this finalizer.
    pub last_seen_utc: Option<i64>,
    /// Age of `last_seen_utc`.
    pub secs_since_seen: Option<i64>,
    /// Whether it has prevoted for a value at our current height.
    pub prevoted: bool,
    /// Whether it has precommitted for a value at our current height.
    pub precommitted: bool,
    /// Highest round we have seen it vote in at our current height.
    pub highest_round_vote: u32,
}

/// Why one round at the current BFT height has not decided.
///
/// This is the structured form of the node's `DECIDE_WAIT` log line: it distinguishes a
/// round short of precommit power (finalizers missing) from one whose proposal cannot be
/// validated (typically this node not yet holding the candidate PoW block).
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct TFLRoundDiagnosis {
    /// BFT height of this round.
    pub height: u64,
    /// Round index.
    pub round: u32,

    /// Whether a proposal has been received for this round.
    pub proposal_present: bool,
    /// Proposal chunk signatures received, of those expected.
    pub proposal_sigs_have: usize,
    /// Proposal chunk signatures expected.
    pub proposal_sigs_want: usize,
    /// `"Pass"`, `"Fail"` or `"Indeterminate"`.
    pub proposal_validity: String,
    /// Set when validity is blocked on a PoW block this node does not hold: the hash it
    /// is waiting for, display order. This is the RPC-visible form of the
    /// "Didn't have hash available for confirmation" log warning.
    pub proposal_blocked_on_block: Option<String>,
    /// Whether the proposal was rejected as faulty.
    pub proposal_is_faulty: bool,

    /// Total active-roster power for this round.
    pub total_power: u64,
    /// The consensus `f` for this round's power.
    pub f: u64,
    /// Power needed to decide: `2f+1`.
    pub quorum_threshold: u64,
    /// Power that has prevoted for the value.
    pub yes_prevote_power: u64,
    /// Power that has precommitted for the value.
    pub yes_precommit_power: u64,
    /// Additional precommit power still needed to reach `quorum_threshold`.
    pub precommit_power_short_by: u64,

    /// One character per active roster index, same encoding as the `DECIDE_WAIT` log
    /// line: `C` yes-precommit, `n` nil-precommit, `p` prevote only, `.` silent.
    pub votes: String,
    /// Finalizers that have not voted at all in this round -- who to chase.
    pub silent: Vec<zcash_primitives::bft::PubKeyID>,
}

/// Sizes of the BFT layer's in-memory structures.
///
/// `rounds_data` and `recent_commit_round_cache` are the two that grow with uptime;
/// `recent_commit_round_cache` in particular currently retains every completed height.
/// Sampling these alongside RSS is what separates "the BFT layer is accumulating state"
/// from "something else on the node is".
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct TFLBftInternalStats {
    /// Rounds held for the height(s) in progress.
    pub rounds_data_len: usize,
    /// Completed-height rounds retained in the commit cache.
    pub recent_commit_round_cache_len: usize,
    /// Decided BFT blocks held in memory.
    pub bft_blocks_len: usize,
    /// Entries in the BFT block-hash index.
    pub bft_block_index_len: usize,
    /// Size of the on-disk `pos.chain` store, in bytes.
    pub pos_chain_bytes: Option<u64>,
    /// BFT peer connection lines, as the node reports them.
    pub peers: Vec<String>,
}

/// A decided BFT block, as this node holds it.
#[derive(Debug, Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct TFLBftBlockInfo {
    /// 0-based BFT chain height.
    pub height: u64,
    /// Block hash, display order.
    pub hash: String,
    /// Block version.
    pub version: u32,
    /// PoW headers carried by this block.
    pub header_count: usize,
    /// Hash of the finalization candidate -- the PoW block this BFT block finalizes to.
    /// Display order, so it can be passed straight to `getblock`.
    pub candidate_hash: Option<String>,
    /// PoW height before which this BFT block must not be included.
    pub do_not_include_until_bc_height: u64,
    /// Finalizer signatures on this block.
    pub signature_count: usize,
    /// Hardfork rules activated by this block.
    pub hardfork_count: usize,
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
    /// Query wallet staking positions grouped by finalizer
    WalletStakingPositions,
    /// Summary of BFT finality health
    FinalityStatus,
    /// Per-finalizer liveness and votes at the current BFT height
    QuorumStatus,
    /// Why the rounds at the current BFT height have not decided
    RoundDiagnosis,
    /// Sizes of the BFT layer's in-memory structures
    BftInternalStats,
    /// A decided BFT block by 0-based height (`None` => chain tip)
    BftBlockInfo(Option<u64>),
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
    /// Query wallet staking positions grouped by finalizer
    WalletStakingPositions(WalletStakingPositions),
    /// Summary of BFT finality health
    FinalityStatus(TFLFinalityStatus),
    /// Per-finalizer liveness and votes at the current BFT height
    QuorumStatus(Vec<TFLQuorumMember>),
    /// Why the rounds at the current BFT height have not decided
    RoundDiagnosis(Vec<TFLRoundDiagnosis>),
    /// Sizes of the BFT layer's in-memory structures
    BftInternalStats(TFLBftInternalStats),
    /// A decided BFT block by height
    BftBlockInfo(Option<TFLBftBlockInfo>),
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
