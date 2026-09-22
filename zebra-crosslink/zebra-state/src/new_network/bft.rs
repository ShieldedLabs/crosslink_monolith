//! The decided BFT chain and everything that reads it.
//!
//! The chain is written only by the `new_network` sync thread (see [`BftRunner`]), which also
//! owns the block writer and the best-chain view, so every rule that reads both the bft-chain and
//! the bc-chain -- proposal, validation, fat-pointer admission, the block-template walk -- runs
//! against one consistent view on one thread. Tenderlink's closures marshal a request to that
//! thread and await one reply. Other readers (the RPC, the visualizer, the test harness) take the
//! read lock on [`bft_chain`].
#![allow(missing_docs)]

use std::collections::{HashMap, HashSet};
use std::hash::BuildHasherDefault;
use std::sync::{Arc, LazyLock, OnceLock, RwLock};

use tenderlink::{
    addr_string_to_stuff, BftAddressMap, BlockValue, ConsensusCounts, FinalizerPeerAddress,
    RoundData, SortedRosterMember, TMState, TMStatus, TMStatusReason, TMStep, ValueId,
};
use zcash_primitives::bft::{
    BftBlock, BftBootstrap, Blake3Hash, FatPointerToBftBlock, FinalizerRecencyStatus,
    HardForkConfig, PubKeyID, TFLRecencyStatus, TMSig, ZcashCrosslinkParameters,
    FINALITY_LIVENESS_ALLOWANCE,
};
use zcash_primitives::block::{
    BlockHash, BlockHeader as BcBlockHeaderWrap, BlockHeaderData as BcBlockHeader,
};
use zcash_primitives::transaction::RosterMember;
use zebra_chain::block::{Hash, Header, Height};
use zebra_chain::serialization::ZcashSerialize;

use super::ReadState;
use crate::service::finalized_state::bft::StoredDecision;
use crate::service::write::WriteBlockWorkerTask;
use crate::CrosslinkVerdict;

/// While set, this node proposes no new BFT blocks.
pub static BFT_PAUSE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// How often the sync thread reports the gap between the bc tip and the last final block.
const DIAGNOSTIC_INTERVAL: std::time::Duration = std::time::Duration::from_millis(8000);

// The key is already a blake3 hash -- uniformly distributed -- so re-hashing it (SipHash) is
// wasted work. Xor-fold the written bytes to a u64 instead; the map's full-key Eq still guards
// against fold collisions.
#[derive(Default)]
pub struct Blake3HashFold(u64);
impl std::hash::Hasher for Blake3HashFold {
    fn write(&mut self, bytes: &[u8]) {
        for chunk in bytes.chunks(8) {
            let mut v = [0u8; 8];
            v[..chunk.len()].copy_from_slice(chunk);
            self.0 ^= u64::from_le_bytes(v);
        }
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

/// The decided bft-chain, plus the finality facts derived from it.
#[derive(Debug)]
pub struct BftChain {
    /// Decided blocks by BFT height. A block with no headers is a placeholder left by
    /// out-of-order ingest and is never a valid pointer target.
    pub blocks: Vec<BftBlock>,
    /// Block hash -> 0-based height (index into `blocks`). Real blocks only; append-only, so
    /// entries are never invalidated. `blake3_hash()` re-serializes the whole block, so
    /// resolving a pointer by scanning costs ~150 MB of hashing at 9.5k blocks.
    pub hash_to_height: HashMap<Blake3Hash, u64, BuildHasherDefault<Blake3HashFold>>,
    pub fat_pointer_to_tip: FatPointerToBftBlock,
    /// The roster for the next height to decide: the stakes at the tip's snapshot, or the last
    /// non-empty roster when those are empty (the ghost roster; see `finish_decision`).
    pub roster: Vec<RosterMember>,
    /// True once tenderlink is running.
    pub is_activated: bool,
    /// The snapshot of the last decided bft-block: what `Π_bft` has finalized, which is not
    /// necessarily on this node.s best chain and is never the node.s own `fin` (FINALITY.md §2).
    pub bft_final_snapshot: Option<(Height, Hash)>,
    pub peer_strings: Vec<String>,
}

static BFT_CHAIN: LazyLock<RwLock<BftChain>> = LazyLock::new(|| {
    RwLock::new(BftChain {
        blocks: Vec::new(),
        hash_to_height: HashMap::default(),
        fat_pointer_to_tip: FatPointerToBftBlock::null(),
        roster: Vec::new(),
        is_activated: false,
        bft_final_snapshot: None,
        peer_strings: Vec::new(),
    })
});

/// The decided bft-chain. Written only by the sync thread; never hold the guard across an await.
pub fn bft_chain() -> &'static RwLock<BftChain> {
    &BFT_CHAIN
}

static RECENCY_STATUS: LazyLock<tokio::sync::watch::Sender<TFLRecencyStatus>> =
    LazyLock::new(|| tokio::sync::watch::channel(TFLRecencyStatus::default()).0);

/// Tenderlink's latest round-state snapshot.
pub fn bft_recency_status() -> TFLRecencyStatus {
    RECENCY_STATUS.borrow().clone()
}

/// What tenderlink (and the test harness) ask the sync thread.
pub enum BftRequest {
    Propose {
        reply: tokio::sync::oneshot::Sender<Option<BftBlock>>,
    },
    Validate {
        block: BftBlock,
        reply: tokio::sync::oneshot::Sender<(TMStatus, TMStatusReason)>,
    },
    Decided {
        block: BftBlock,
        fat_pointer: FatPointerToBftBlock,
        proposal_sigs: Vec<TMSig>,
        reply: tokio::sync::oneshot::Sender<(Vec<SortedRosterMember>, [u8; 32])>,
    },
    /// A decided block supplied from outside (the test format's `LoadPoS`): validated and stored
    /// exactly as a tenderlink decision, with no proposal signatures.
    ForceFeed {
        block: Arc<BftBlock>,
        fat_pointer: FatPointerToBftBlock,
        reply: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
}

// A std channel rather than a tokio one: the receiver blocks the sync thread in the idle part of
// its tick (`BftRunner::wait`), which a tokio receiver cannot do.
static BFT_REQUEST_SENDER: OnceLock<std::sync::mpsc::Sender<BftRequest>> = OnceLock::new();

fn send_request(request: BftRequest) -> bool {
    match BFT_REQUEST_SENDER.get() {
        Some(tx) => tx.send(request).is_ok(),
        None => false,
    }
}

/// Store a decided block supplied from outside, as if tenderlink had decided it.
pub async fn force_feed_bft_block(
    block: Arc<BftBlock>,
    fat_pointer: FatPointerToBftBlock,
) -> Result<(), String> {
    let (reply, rx) = tokio::sync::oneshot::channel();
    if !send_request(BftRequest::ForceFeed { block, fat_pointer, reply }) {
        return Err("new_network is not running".to_string());
    }
    rx.await.map_err(|_| "new_network dropped the force-feed reply".to_string())?
}

/// Everything BFT needs from outside the state: the node's finalizer identity, where it listens,
/// and who to connect to. The decided chain is persisted in the finalized database.
pub struct BftLaunch {
    pub signing_key: ed25519_zebra::SigningKey,
    pub public_address: String,
    pub peer_addresses: Vec<String>,
}

pub fn bc_hdr_to_lrz(header: &Header) -> BcBlockHeader {
    let mut bytes = Vec::new();
    header.zcash_serialize(&mut bytes).unwrap();
    BcBlockHeaderWrap::read_data(&*bytes).unwrap()
}

/// The set of finalizers terminated (blacklisted) by user-led hardforks at a given point, as a
/// pure function of `(schedule, bft_height, finalized_bc_height)`.
///
/// A finalizer is terminated at BFT height `bft_height` when some scheduled hardfork:
/// - has `bft_certificate_height <= bft_height` -- in effect at this height, **inclusive**, so the
///   finalizers a hardfork terminates are already excluded from the roster that votes on the very
///   block carrying that hardfork (this matches the vote-namespacing inclusiveness); and
/// - has `pow_activation_height > finalized_bc_height` -- the activation has not yet been
///   finalized. Once it is, the bonds are terminated at the source (so the finalizer is gone from
///   the roster anyway) and the key is free to be restaked to, so it must no longer be suppressed
///   here.
pub fn terminated_finalizers_at(
    hardforks: &[HardForkConfig],
    bft_height: u64,
    finalized_bc_height: u64,
) -> HashSet<PubKeyID> {
    let mut set = HashSet::new();
    for hf in hardforks.iter().filter(|hf| {
        hf.bft_certificate_height <= bft_height && hf.pow_activation_height > finalized_bc_height
    }) {
        for &finalizer in &hf.terminated_finalizers {
            set.insert(finalizer);
        }
    }
    set
}

/// Vote-namespacing domain separator for a BFT height: a flat blake3 hash of the prefix of
/// scheduled hardforks whose `bft_certificate_height <= bft_height` (inclusive of a hardfork at
/// `bft_height` itself), concatenated in canonical schedule order. An empty prefix yields
/// `[0; 32]` (nil), so the no-hardfork case is a backwards-compatible no-op in tenderlink's
/// signing. The schedule is sorted with non-decreasing `bft_certificate_height` (several rules
/// may share one certificate height), so the filtered set is exactly the prefix.
pub fn namespace_for_bft_height(hardforks: &[HardForkConfig], bft_height: u64) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    let mut any = false;
    for hf in hardforks.iter().filter(|hf| hf.bft_certificate_height <= bft_height) {
        let mut bytes = Vec::new();
        hf.zcash_serialize(&mut bytes).expect("serializing to a Vec is infallible");
        hasher.update(&bytes);
        any = true;
    }
    if any { hasher.finalize().into() } else { [0u8; 32] }
}

/// Build the tenderlink consensus roster from the stored roster, excluding any finalizer in
/// `terminated` (terminated by a user-led hardfork; see [`terminated_finalizers_at`]). Pass an
/// empty set to build the roster unfiltered.
pub fn tenderlink_roster_from_internal(
    vals: &[RosterMember],
    terminated: &HashSet<PubKeyID>,
) -> Vec<SortedRosterMember> {
    let mut ret: Vec<SortedRosterMember> = vals
        .iter()
        .map(|v| SortedRosterMember {
            pub_key: PubKeyID(v.pub_key),
            stake: v.voting_power,
            cumulative_stake: 0,
        })
        .filter(|m| !terminated.contains(&m.pub_key))
        .collect();

    // The roster is sorted so that everyone has exactly the same view -- who is under the active
    // cap, and which index represents a member -- regardless of the order members were learned
    // in. Members with the same stake tie-break on pub_key. The cumulative stake makes weighted
    // round-robin easy.
    ret.sort_by_key(|m: &SortedRosterMember| std::cmp::Reverse((m.stake, m.pub_key)));
    debug_assert!(ret.is_sorted_by(|a, b| a.stake >= b.stake));
    let mut cumulative_stake = 0;
    for m in &mut ret {
        cumulative_stake += m.stake;
        m.cumulative_stake = cumulative_stake;
    }
    ret
}

/// The fat pointer to the block at 1-based `at_height`, or `None` when the chain has no such
/// block. The tip's pointer is the stored one; any other is the next block's previous-block
/// pointer.
pub fn fat_pointer_to_block_at_height(
    bft_blocks: &[BftBlock],
    fat_pointer_to_tip: &FatPointerToBftBlock,
    at_height: u64,
) -> Option<FatPointerToBftBlock> {
    if at_height == 0 || at_height as usize - 1 >= bft_blocks.len() {
        return None;
    }
    if at_height as usize == bft_blocks.len() {
        Some(fat_pointer_to_tip.clone())
    } else {
        Some(bft_blocks[at_height as usize].previous_block_fat_ptr.clone())
    }
}

/// The round data tenderlink keeps for an already-decided height, as the live path would have
/// left it: precommit signatures placed by roster index, counts from the fat pointer.
fn decided_round_data(
    hardforks: &[HardForkConfig],
    block: &BftBlock,
    fat_pointer: &FatPointerToBftBlock,
    roster: Vec<SortedRosterMember>,
    proposal_sigs: Vec<TMSig>,
    bft_height: u64,
) -> RoundData {
    let mut round_data = RoundData::EMPTY;
    round_data.msg_val_sigs = roster
        .iter()
        .map(|v| {
            fat_pointer
                .signatures
                .iter()
                .find(|s| s.pub_key == v.pub_key)
                .map(|s| s.vote_signature)
                .unwrap_or([0u8; 64])
        })
        .map(|s| {
            [
                (ValueId::NIL, TMSig::NIL),
                (ValueId(fat_pointer.points_at_block_hash().0), TMSig(s)),
            ]
        })
        .collect();
    round_data.msg_nil_sigs = vec![[TMSig::NIL; 2]; roster.len()];
    round_data.roster = roster;
    round_data.counts.precommits = fat_pointer.signatures.len() as u64;
    round_data.counts.yes_precommits = fat_pointer.signatures.len() as u64;
    round_data.proposal_sigs_n = proposal_sigs.len();
    round_data.proposal_sigs = proposal_sigs;
    round_data.proposal = BlockValue(block.zcash_serialize_to_vec().unwrap());
    round_data.proposal_id = ValueId(fat_pointer.points_at_block_hash().0);
    round_data.height = bft_height;
    round_data.round = fat_pointer.get_vote_template().round as u32;
    round_data.vote_namespace = namespace_for_bft_height(hardforks, bft_height);
    round_data
}

/// Whether a bc-block may carry `child_fat_pointer` given its parent's pointer, and whether it
/// pays PoS issuance.
///
/// Return value:
///   `None`                       => DEFER. Reversible: this node lacks the information to be
///                                   certain the block is invalid, typically an unresolved BFT
///                                   pointer whose block has not entered this node yet.
///   `Some(Reject)`               => REJECT. Permanent and irreversible: the block is dropped and
///                                   every descendant queued behind it is orphaned. Only returned
///                                   on facts that are immutable and view-independent.
///   `Some(Accept { pos_payout })` => ACCEPT; `pos_payout` decides whether this block mints PoS
///                                   issuance (see the tail of this function).
///
/// `parent_hash` is the candidate's parent: the candidate is not committed yet, so its ancestry
/// is its parent's ancestry plus the parent itself. Every chain is searched, because the block
/// being admitted may be extending a side chain.
pub fn admit_fat_pointer(
    chain: &BftChain,
    params: &ZcashCrosslinkParameters,
    read_state: &ReadState,
    parent_hash: Hash,
    parent_fat_pointer: &FatPointerToBftBlock,
    child_fat_pointer: &FatPointerToBftBlock,
    pow_block_height: Height,
) -> Option<CrosslinkVerdict> {
    let parent_is_null = *parent_fat_pointer == FatPointerToBftBlock::null();
    let child_is_null = *child_fat_pointer == FatPointerToBftBlock::null();

    // PERMANENT, from the block's own height: where BFT is bootstrapped from the chain it does
    // not exist at or below the activation height, so a pointer there can never resolve to a
    // legitimate block (see `BftBootstrap`).
    if let Some(activation_height) = params.bootstrap.activation_height() {
        if !child_is_null && pow_block_height.0 <= activation_height {
            return Some(CrosslinkVerdict::Reject);
        }
    }

    // PERMANENT, decided purely from the (immutable) pointer values, without resolving either
    // block: the child reverts to no BFT pointer while its parent had one. A null pointer can
    // never be "as new or newer" than a real one, so this is a certain regression.
    if child_is_null && !parent_is_null {
        return Some(CrosslinkVerdict::Reject);
    }

    // Resolve the child pointer against the bft-chain. A non-null pointer we cannot resolve yet
    // (its BFT block has not entered this node) is NOT a failure -- defer.
    let child_index = if child_is_null {
        None
    } else {
        match chain.hash_to_height.get(&child_fat_pointer.points_at_block_hash()) {
            Some(&h) => Some(h as usize),
            None => return None,
        }
    };

    // PERMANENT, from immutable data: a resolved BFT block carries its own
    // `do_not_include_until_bc_height`, and the PoW block carries its own height. Both are
    // fixed, so a PoW block referencing a BFT block before that BFT block is allowed to be
    // included can never become valid later.
    if let Some(h) = child_index {
        let do_not_include = chain.blocks[h].do_not_include_until_bc_height;
        if (pow_block_height.0 as u64) < do_not_include {
            return Some(CrosslinkVerdict::Reject);
        }
    }

    // The PoW block this block's certificate finalizes -- the snapshot, i.e. the parent of the
    // BFT block's deepest carried header. Placeholder entries from out-of-order BFT ingest carry
    // no headers and are never valid pointer targets, but guard rather than panic.
    let snapshot_hash = match child_index {
        Some(h) if !chain.blocks[h].headers.is_empty() => {
            Some(Hash(chain.blocks[h].snapshot_block_hash().0))
        }
        Some(_) => return None,
        None => None,
    };

    let parent_index = if parent_is_null {
        None
    } else {
        match chain.hash_to_height.get(&parent_fat_pointer.points_at_block_hash()) {
            Some(&h) => Some(h as usize),
            None => return None,
        }
    };

    // Ordering: the child must reference a BFT block at least as new as its parent's. A real
    // pointer ranks as `index + 1`; null ranks as 0 (kept strictly below any real pointer).
    //
    // Once BOTH pointers resolve, this is a PERMANENT fact: the bft-chain is append-only (each
    // decided block is written at the index equal to its own height, strictly in order, and a
    // real block is never overwritten; finalized BFT blocks never revert). So a resolved
    // pointer's index is fixed forever, and a genuine ordering violation can never become valid.
    let child_rank = child_index.map(|h| h + 1).unwrap_or(0);
    let parent_rank = parent_index.map(|h| h + 1).unwrap_or(0);
    if child_rank < parent_rank {
        return Some(CrosslinkVerdict::Reject);
    }

    let mut pos_payout = false;
    if let Some(snapshot_hash) = snapshot_hash {
        // The sigma-confirmation rule. A certificate finalizing PoW height F may only be carried
        // by a PoW block at F + sigma + 1 or above: the sigma carried headers F+1 ..= F+sigma,
        // then the carrier. The carried headers are evidence nobody checks to be on this chain,
        // so without this rule a block at F + 1 could carry a certificate whose "confirmations"
        // are headers from another branch entirely, and the chain would call F final with one
        // block of work above it. Whether F is an ancestor of the carrier is a separate question
        // (the Last Final Snapshot rule); this check only bounds the depth.
        //
        // PERMANENT once resolved, from immutable data: which PoW block a BFT block finalizes is
        // fixed by its bytes, that block's height is fixed by its own coinbase, and so is the
        // height of the block carrying the pointer. An unresolved snapshot is not a failure --
        // this node has simply not seen that PoW block yet -- so it defers, exactly as an
        // unresolved BFT pointer does.
        let snapshot_height = read_state.known_block(snapshot_hash)?.height;
        let sigma = params.bc_confirmation_depth_sigma;
        let gap = (pow_block_height.0 as u64).saturating_sub(snapshot_height.0 as u64);
        if gap < sigma + 1 {
            return Some(CrosslinkVerdict::Reject);
        }

        // The Last Final Snapshot rule: `snapshot(LF(H)) ⪯bc H` (FINALITY.md §3.4). The depth
        // check above bounds only how far below the carrier the snapshot sits; this is what
        // makes those sigma confirmations confirmations of THIS chain. Without it a block at
        // F + sigma + 1 can carry a certificate finalizing a block on another branch, and the
        // chain would call final a block it does not even contain.
        //
        // PERMANENT once answered: a block's ancestry is fixed by its own bytes, so a `false`
        // can never become true later. `None` means the chain cannot place the snapshot on a
        // branch yet, which defers for the same reason as an unresolved pointer.
        match read_state.is_ancestor_of(snapshot_hash, parent_hash) {
            Some(true) => {}
            Some(false) => return Some(CrosslinkVerdict::Reject),
            None => return None,
        }

        // PoS issuance rides on this check because this is the one place that knows both facts it
        // needs. A block pays only when it ADVANCES finality (its certificate is a different BFT
        // block than its parent's -- compared by the cert's identity, the BFT block hash, since
        // two honest nodes can carry different signature sets for the same decision) and does so
        // PROMPTLY (`gap <= sigma + FINALITY_LIVENESS_ALLOWANCE`; the check above already put
        // `gap >= sigma + 1`, so the payable window is exactly those few heights).
        //
        // Both facts are objective functions of committed chain data, so every node reaches the
        // same answer for the same block. See `FINALITY_LIVENESS_ALLOWANCE`.
        let cert_advanced =
            child_fat_pointer.points_at_block_hash() != parent_fat_pointer.points_at_block_hash();
        pos_payout = cert_advanced && gap <= sigma + FINALITY_LIVENESS_ALLOWANCE;

        // One line per block recording the decision and the facts behind it. `debug!` would be
        // the natural level, but the release binary is built with `release_max_level_info`, so
        // anything below `info` is compiled out and would never be seen on a real node.
        if cert_advanced && !pos_payout {
            // The notable case: finality DID advance here, but so slowly that the block earns
            // nothing. It is otherwise indistinguishable from a block that simply carried the
            // same certificate as its parent, so it is spelled out.
            tracing::info!(
                "no PoS issuance at height {}: the certificate finalizes height {} \
                 (gap {}), beyond sigma {} + FINALITY_LIVENESS_ALLOWANCE {}",
                pow_block_height.0, snapshot_height.0, gap, sigma, FINALITY_LIVENESS_ALLOWANCE,
            );
        } else {
            tracing::info!(
                "PoS payout decision at height {}: payout={} cert_advanced={} gap={} snapshot={} sigma={}",
                pow_block_height.0, pos_payout, cert_advanced, gap, snapshot_height.0, sigma,
            );
        }
    }

    Some(CrosslinkVerdict::Accept { pos_payout })
}

/// The fat pointer a block template at `proposed_pow_height` should carry, extending this node's
/// best tip.
///
/// Walks back from the tip to the highest BFT block this PoW height may carry: one whose
/// `do_not_include_until_bc_height` allows it, AND whose snapshot is deep enough to satisfy the
/// sigma-confirmation rule admission enforces. The second condition belongs here as much as in
/// admission: admission rejects a violation PERMANENTLY, so handing the miner a too-new
/// certificate produces a block that can never be committed and is re-mined forever. Being one
/// PoW block behind the proposer is enough to reach that state -- this node can hold a decided
/// BFT block whose snapshot sits sigma below a tip it has not seen yet.
///
/// Honest context selection (FINALITY.md §3.4) adds the third condition: the cited block's
/// snapshot has to lie on the chain the template extends, or Last Final Snapshot rejects the
/// block built from it.
///
/// The parent block's own context always qualifies -- it satisfied Last Final Snapshot against
/// the parent, and the template's chain contains the parent's -- so a template always has one.
/// Falling back to it rather than to the null pointer also keeps the Extension rule satisfied,
/// which reverting to null would not.
pub fn fat_pointer_for_template(
    chain: &BftChain,
    params: &ZcashCrosslinkParameters,
    read_state: &ReadState,
    proposed_pow_height: u64,
) -> FatPointerToBftBlock {
    let sigma = params.bc_confirmation_depth_sigma;
    let parent_hash = read_state.best_tip().map(|(_, hash)| hash);
    let mut suitable_height = None;
    for (i, b) in chain.blocks.iter().enumerate().rev() {
        if b.headers.is_empty() || b.do_not_include_until_bc_height > proposed_pow_height {
            continue;
        }
        let snapshot_hash = Hash(b.snapshot_block_hash().0);
        let Some(known) = read_state.known_block(snapshot_hash) else { continue; };
        if known.height.0 as u64 + sigma + 1 > proposed_pow_height {
            continue;
        }
        let on_the_template_chain = match parent_hash {
            Some(parent_hash) => read_state.is_ancestor_of(snapshot_hash, parent_hash) == Some(true),
            // No tip to judge against, so there is no chain to be off: this is the GUI's display
            // query on an empty state.
            None => true,
        };
        if on_the_template_chain {
            suitable_height = Some(i as u64 + 1);
            break;
        }
    }
    if let Some(h) = suitable_height {
        return fat_pointer_to_block_at_height(&chain.blocks, &chain.fat_pointer_to_tip, h)
            .unwrap_or_else(FatPointerToBftBlock::null);
    }
    parent_hash
        .and_then(|parent_hash| read_state.any_chain_block_header(parent_hash.into()))
        .map(|hdr| hdr.fat_pointer_to_bft_block.clone())
        .unwrap_or_else(FatPointerToBftBlock::null)
}

/// A decided bft-block, on its way to being recorded and answered.
struct DecidedBlock {
    new_final_hash: Hash,
    new_final_height: Height,
    block: BftBlock,
    fat_pointer: FatPointerToBftBlock,
    proposal_sigs: Vec<TMSig>,
    reply: DecisionReply,
}

enum DecisionReply {
    Tenderlink(tokio::sync::oneshot::Sender<(Vec<SortedRosterMember>, [u8; 32])>),
    ForceFeed(tokio::sync::oneshot::Sender<Result<(), String>>),
}

/// The BFT side of the sync thread: answers tenderlink's requests against the writer's chains,
/// bootstraps BFT from the bc-chain, and persists decisions.
pub(super) struct BftRunner {
    rx: std::sync::mpsc::Receiver<BftRequest>,
    rt: tokio::runtime::Handle,
    params: ZcashCrosslinkParameters,
    hardforks: Arc<zebra_chain::parameters::HardForkSchedule>,
    /// Held until BFT starts (a loaded chain, or the bootstrap genesis); `None` once tenderlink
    /// is spawned, or when this node runs no BFT at all.
    launch: Option<BftLaunch>,
    /// A bootstrap genesis that has been decided but whose commit has not finished yet, so
    /// tenderlink cannot be started from it. Waiting on the reply here would freeze the sync
    /// thread, which is the only thread that can retry the commit.
    pending_bootstrap: Option<PendingBootstrap>,
    next_diagnostic: std::time::Instant,
}

struct PendingBootstrap {
    genesis: BftBlock,
    fat_pointer: FatPointerToBftBlock,
    roster_rx: tokio::sync::oneshot::Receiver<(Vec<SortedRosterMember>, [u8; 32])>,
}

impl BftRunner {
    /// Restore the persisted chain and start tenderlink if it holds anything. Must run after
    /// genesis is committed: the restore resolves snapshots against the bc-chain.
    pub(super) fn new(
        launch: Option<BftLaunch>,
        config: &crate::config::Config,
        read_state: &ReadState,
        block_writer: &mut WriteBlockWorkerTask,
        rt: tokio::runtime::Handle,
    ) -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let _ = BFT_REQUEST_SENDER.set(tx);
        let mut runner = BftRunner {
            rx,
            rt,
            params: read_state.network().crosslink_parameters(),
            hardforks: config.hardfork_schedule.clone(),
            launch,
            pending_bootstrap: None,
            next_diagnostic: std::time::Instant::now(),
        };
        crate::new_network::fin::load(&block_writer.finalized_state.db);
        if runner.launch.is_some() {
            runner.restore(read_state, block_writer);
        }
        runner
    }

    /// Work that runs at the start of every tick.
    pub(super) fn tick(&mut self, read_state: &ReadState, block_writer: &mut WriteBlockWorkerTask) {
        // Crosslink bootstrap: the first accepted PoW block at the activation height (h2)
        // finalizes h1 through a deterministic genesis decision, and BFT starts at height 1 with
        // h1's roster. A network whose BFT is supplied has no activation height and never
        // bootstraps.
        self.finish_bootstrap();
        if let (Some(_), Some(activation_height)) = (self.launch.as_ref(), self.params.bootstrap.activation_height()) {
            if read_state.best_tip().is_some_and(|(tip, _)| tip.0 >= activation_height) {
                self.bootstrap(read_state, block_writer);
            }
        }

        if self.next_diagnostic.elapsed() >= DIAGNOSTIC_INTERVAL {
            self.next_diagnostic = std::time::Instant::now();
            let latest_final = BFT_CHAIN.read().unwrap().bft_final_snapshot;
            if let (Some((tip_height, _)), Some((final_height, _))) = (read_state.best_tip(), latest_final) {
                if tip_height < final_height {
                    tracing::info!("Our PoW tip is {} blocks away from the latest final block.", final_height - tip_height);
                } else if tip_height - final_height > 512 {
                    tracing::warn!("WARNING! BFT-Finality is falling behind the PoW chain. Current gap to tip is {:?} blocks.", tip_height - final_height);
                }
            }
        }

        self.drain(read_state, block_writer);
    }

    /// Serve requests until `deadline`: the idle part of the tick, so a BFT round does not wait
    /// out the whole tick for its answer.
    pub(super) fn wait(&mut self, deadline: std::time::Instant, read_state: &ReadState, block_writer: &mut WriteBlockWorkerTask) {
        loop {
            let now = std::time::Instant::now();
            if now >= deadline {
                return;
            }
            match self.rx.recv_timeout(deadline - now) {
                Ok(request) => self.handle(request, read_state, block_writer),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => return,
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    std::thread::sleep(deadline - now);
                    return;
                }
            }
        }
    }

    fn drain(&mut self, read_state: &ReadState, block_writer: &mut WriteBlockWorkerTask) {
        loop {
            match self.rx.try_recv() {
                Ok(request) => self.handle(request, read_state, block_writer),
                Err(_) => break,
            }
        }
    }

    fn handle(&mut self, request: BftRequest, read_state: &ReadState, block_writer: &mut WriteBlockWorkerTask) {
        match request {
            BftRequest::Propose { reply } => {
                let _ = reply.send(self.propose(read_state));
            }
            BftRequest::Validate { block, reply } => {
                let chain = BFT_CHAIN.read().unwrap();
                let _ = reply.send(self.validate(&chain, read_state, &block));
            }
            BftRequest::Decided { block, fat_pointer, proposal_sigs, reply } => {
                self.decide(block, fat_pointer, proposal_sigs, DecisionReply::Tenderlink(reply), read_state, block_writer);
            }
            BftRequest::ForceFeed { block, fat_pointer, reply } => {
                if fat_pointer.points_at_block_hash() != block.blake3_hash() {
                    let _ = reply.send(Err(format!(
                        "Fat Pointer hash does not match block hash. fp: {} block: {}",
                        fat_pointer.points_at_block_hash(), block.blake3_hash(),
                    )));
                    return;
                }
                let (status, reason) = {
                    let chain = BFT_CHAIN.read().unwrap();
                    self.validate(&chain, read_state, &block)
                };
                if status != TMStatus::Pass {
                    let _ = reply.send(Err(format!("PoS validation = {status:?}: {reason:?}")));
                    return;
                }
                let block = Arc::unwrap_or_clone(block);
                self.decide(block, fat_pointer, Vec::new(), DecisionReply::ForceFeed(reply), read_state, block_writer);
            }
        }
    }

    fn propose(&self, read_state: &ReadState) -> Option<BftBlock> {
        if BFT_PAUSE.load(std::sync::atomic::Ordering::Relaxed) {
            return None;
        }
        let params = &self.params;
        let sigma = params.bc_confirmation_depth_sigma;
        let (tip_height, _) = read_state.best_tip()?;
        if (tip_height.0 as u64) < sigma {
            tracing::info!("not enough blocks to enforce finality; tip height: {}", tip_height.0);
            return None;
        }
        // `tip - sigma` is the `snapshot`: the block this proposal finalizes. The certificate
        // carries the sigma headers F+1 ..= tip, the confirmations above the snapshot, and the
        // highest carried header is the tip itself. The snapshot is not carried: it is named by
        // `BftBlock::snapshot_block_hash()` (`parent(headers[0])`), and `find_block_headers`
        // returns the headers starting AFTER its anchor, which is exactly the window above the
        // snapshot.
        //
        // Guarding on the snapshot height keeps one BFT block per PoW block: the tip advancing
        // by one advances the snapshot by one, so each new PoW block is proposable at once.
        let finality_candidate_height = Height(tip_height.0 - sigma as u32);

        let chain = BFT_CHAIN.read().unwrap();
        let bft_final_snapshot = chain.bft_final_snapshot;
        // The parent bft-block's snapshot, for the Linearity check below. A parent carrying no
        // headers is a placeholder from out-of-order ingest and names no snapshot.
        let parent_snapshot_hash = chain
            .blocks
            .last()
            .filter(|b| !b.headers.is_empty())
            .map(|b| Hash(b.snapshot_block_hash().0));
        // 0-based canonical height = the chain index this block will occupy. Serialization
        // re-adds the legacy +1 for v1 blocks (see BftBlock::zcash_serialize).
        let bft_height = chain.blocks.len() as u64;
        let fat_ptr = chain.fat_pointer_to_tip.clone();
        // Parent (current tip) do_not_include, carried forward so the value never regresses.
        let parent_do_not_include = chain.blocks.last().map_or(0, |p| p.do_not_include_until_bc_height);
        drop(chain);

        let is_improved_final = bft_final_snapshot.map_or(true, |(h, _)| finality_candidate_height > h);
        if !is_improved_final {
            tracing::info!(
                "candidate block can't be final: height {}, final height: {:?}",
                finality_candidate_height.0, bft_final_snapshot
            );
            return None;
        }

        // The +40 candidate clamp. This is a Zebra Crosslink design heuristic, not part of the
        // Crosslink 2 specification, and where it binds the proposal departs from honest
        // proposal (FINALITY.md §3.4): `headers_bc` is then a window of `bc_best` ending below
        // its tip rather than its tail. The window still satisfies Tail Confirmation, which is
        // what block validity actually requires, so the departure costs finality speed rather
        // than validity.
        let finality_candidate_height = Height(
            finality_candidate_height.0.min(bft_final_snapshot.map_or(u32::MAX, |(h, _)| h.0 + 40)),
        );

        let Some(candidate_hash) = read_state.best_chain_block_hash(finality_candidate_height) else {
            tracing::info!("not proposing: no best-chain block at height {}", finality_candidate_height.0);
            return None;
        };

        // Linearity (FINALITY.md §3.4): a proposal whose snapshot does not extend the parent
        // bft-block's snapshot is invalid, so there is no point proposing it. Honest proposal
        // says to repeat the parent's `headers_bc` instead of declining; how often a node should
        // repeat them is design question 3 in IMPLEMENTATION.md, so this keeps declining until
        // that is settled. The common cause is a bc reorganization onto a branch that forks
        // below the parent's snapshot, which resolves on its own once a chain containing that
        // snapshot is best again.
        if let Some(parent_snapshot_hash) = parent_snapshot_hash {
            if parent_snapshot_hash != candidate_hash
                && read_state.is_ancestor_of(parent_snapshot_hash, candidate_hash) != Some(true)
            {
                tracing::info!(
                    "not proposing: candidate snapshot {} does not extend the parent bft-block's snapshot {}",
                    candidate_hash, parent_snapshot_hash,
                );
                return None;
            }
        }

        let mut headers: Vec<BcBlockHeader> = read_state
            .find_block_headers(vec![candidate_hash], None)
            .into_iter()
            .map(|ch| bc_hdr_to_lrz(&ch.header))
            .collect();
        headers.truncate(sigma as usize);

        // The tip and the headers are two reads of a chain another commit on this thread
        // cannot have moved, but the reads still see the same branch only if the candidate is
        // on it. Tail Confirmation would reject our own proposal otherwise, so check the same
        // thing here and decline instead.
        if headers.len() != sigma as usize {
            tracing::info!(
                "not proposing: the chain returned {} headers above the candidate, not sigma = {}",
                headers.len(), sigma,
            );
            return None;
        }
        let tail_links_to_candidate = headers[0].prev_block == BlockHash(candidate_hash.0)
            && headers
                .windows(2)
                .all(|w| w[1].prev_block == BlockHash::from_header_data(&w[0]));
        if !tail_links_to_candidate {
            tracing::info!("not proposing: the headers above the candidate do not form a chain");
            return None;
        }

        // The user-led hardforks scheduled at this BFT height (several rules may share one
        // certificate height). The schedule is canonical, so this filter preserves canonical
        // (ascending pow_activation_height) order -- the same order `validate` requires
        // byte-for-byte.
        let scheduled_hardforks: Vec<HardForkConfig> = self
            .hardforks
            .rules()
            .iter()
            .filter(|hf| hf.bft_certificate_height == bft_height)
            .cloned()
            .collect();

        match BftBlock::try_from(params, bft_height as u32, fat_ptr, headers) {
            Ok(mut block) => {
                // Always propose v2 blocks: existing v1 blocks remain v1 (so their hashes and
                // signatures are untouched), and v2 >= any parent's version satisfies the
                // monotonic-version check.
                block.version = 2;
                // A hardfork block sets do_not_include to the greatest activation height it
                // carries (pointing at this block commits to every certificate in it, so the
                // strictest one governs); otherwise the parent's carries forward so it never
                // regresses (both as `validate` requires).
                if let Some(last) = scheduled_hardforks.last() {
                    block.do_not_include_until_bc_height = last.pow_activation_height;
                    block.hardforks = scheduled_hardforks;
                } else {
                    block.do_not_include_until_bc_height = parent_do_not_include;
                }
                Some(block)
            }
            Err(e) => {
                tracing::warn!("Unable to create BftBlock to propose, Error={:?}", e);
                None
            }
        }
    }

    fn validate(&self, chain: &BftChain, read_state: &ReadState, new_block: &BftBlock) -> (TMStatus, TMStatusReason) {
        let fail = (TMStatus::Fail, TMStatusReason::None);

        if new_block.previous_block_fat_ptr.points_at_block_hash() != chain.fat_pointer_to_tip.points_at_block_hash() {
            tracing::warn!(
                "Block has invalid previous block fat pointer hash: was {} but should be {}",
                new_block.previous_block_fat_ptr.points_at_block_hash(),
                chain.fat_pointer_to_tip.points_at_block_hash(),
            );
            return fail;
        }

        // The linkage check above guarantees the parent is the current tip, so this block will
        // occupy the next index -- which is its canonical 0-based height.
        let bft_height = chain.blocks.len() as u64;
        if new_block.height as u64 != bft_height {
            tracing::warn!("BFT block height {} does not match its chain position {}", new_block.height, bft_height);
            return fail;
        }

        let parent = chain.blocks.last();
        let parent_version = parent.map_or(0, |p| p.version);
        let parent_do_not_include = parent.map_or(0, |p| p.do_not_include_until_bc_height);

        // Version must be monotonic non-decreasing along the chain. (All v1 blocks share
        // version 1, so this holds trivially for existing history.)
        if new_block.version < parent_version {
            tracing::warn!("BFT block version {} is below its parent's version {}", new_block.version, parent_version);
            return fail;
        }

        // The proposal must carry exactly the hardforks this node has scheduled at this BFT
        // height -- byte-for-byte, in canonical (ascending pow_activation_height) order, and
        // none at all when none are scheduled -- and set its do_not_include_until_bc_height to
        // the greatest activation height among them (pointing at this block commits to every
        // certificate in it, so the strictest one governs).
        //
        // This runs before the generic do_not_include_until_bc_height monotonicity check below
        // so that a regression *caused by the hardfork activation* is reported as its own
        // distinct error rather than the generic one.
        let scheduled_hardforks: Vec<&HardForkConfig> = self
            .hardforks
            .rules()
            .iter()
            .filter(|hf| hf.bft_certificate_height == bft_height)
            .collect();
        {
            let serialize = |hf: &HardForkConfig| {
                let mut bytes = Vec::new();
                hf.zcash_serialize(&mut bytes).expect("serializing to a Vec is infallible");
                bytes
            };
            let proposal_matches = new_block.hardforks.len() == scheduled_hardforks.len()
                && new_block
                    .hardforks
                    .iter()
                    .zip(scheduled_hardforks.iter())
                    .all(|(carried, scheduled)| serialize(carried) == serialize(scheduled));
            if !proposal_matches {
                tracing::warn!(
                    "BFT block at height {} must carry exactly the {} scheduled hardfork(s) byte-for-byte in schedule order, but does not",
                    bft_height, scheduled_hardforks.len(),
                );
                return fail;
            }
        }

        // Rules are pow-sorted, so the last scheduled rule has the greatest activation.
        if let Some(last_scheduled) = scheduled_hardforks.last() {
            if new_block.do_not_include_until_bc_height != last_scheduled.pow_activation_height {
                tracing::warn!(
                    "BFT hardfork block at height {} must set do_not_include_until_bc_height to the greatest carried pow_activation_height {}, but it is {}",
                    bft_height, last_scheduled.pow_activation_height, new_block.do_not_include_until_bc_height,
                );
                return fail;
            }
            // Separate error: the hardfork's PoW activation height must not regress the
            // monotonic do_not_include_until_bc_height relative to the parent.
            if last_scheduled.pow_activation_height < parent_do_not_include {
                tracing::warn!(
                    "Hardfork pow_activation_height {} at BFT height {} is below the parent's do_not_include_until_bc_height {}; the hardfork activation regresses do_not_include_until_bc_height",
                    last_scheduled.pow_activation_height, bft_height, parent_do_not_include,
                );
                return fail;
            }
        }

        // do_not_include_until_bc_height must be monotonic non-decreasing. (v1 blocks use the
        // implicit value 0, so this holds for existing history.) In the hardfork case the checks
        // above already guarantee this passes.
        if new_block.do_not_include_until_bc_height < parent_do_not_include {
            tracing::warn!(
                "BFT block do_not_include_until_bc_height {} is below its parent's {}",
                new_block.do_not_include_until_bc_height, parent_do_not_include,
            );
            return fail;
        }

        // The parent bft-block's snapshot; Linearity compares it against this block's snapshot
        // below. A parent carrying no headers is a placeholder from out-of-order ingest and
        // names no snapshot.
        let parent_snapshot_hash = parent
            .filter(|p| !p.headers.is_empty())
            .map(|p| Hash(p.snapshot_block_hash().0));

        // Tail Confirmation (FINALITY.md §3.4): `headers_bc` is the sigma-block tail of a
        // bc-valid chain. The rule is objective -- it says nothing about this validator's own
        // best chain -- and has three parts: the count, the linkage, and the bc-validity of the
        // blocks named.
        let sigma = self.params.bc_confirmation_depth_sigma as usize;
        if new_block.headers.len() != sigma {
            tracing::warn!(
                "BFT block carries {} headers; Tail Confirmation requires exactly sigma = {}",
                new_block.headers.len(), sigma,
            );
            return fail;
        }
        for i in 1..new_block.headers.len() {
            let expected = BlockHash::from_header_data(&new_block.headers[i - 1]);
            if new_block.headers[i].prev_block != expected {
                tracing::warn!(
                    "BFT block header {} does not follow header {}: its previous-block hash is {}, not {}",
                    i, i - 1, new_block.headers[i].prev_block, expected,
                );
                return fail;
            }
        }
        // The headers are linked, so the topmost one carries the whole tail with it: a block
        // this state holds has passed bc validation, and its ancestry is exactly these headers
        // and then the snapshot. Checking the rest one by one would add lookups and no
        // information.
        if let Some(top_header) = new_block.headers.last() {
            let top_hash = Hash(BlockHash::from_header_data(top_header).0);
            if read_state.known_block(top_hash).is_none() {
                // Not a violation: this node has simply not seen that bc-block yet.
                return (TMStatus::Indeterminate, TMStatusReason::NeedsBlock { hash: top_hash.0 });
            }
        }

        // The `snapshot` this proposal finalizes: the parent of the deepest carried header.
        let new_final_hash = Hash(new_block.snapshot_block_hash().0);
        if read_state.known_block(new_final_hash).is_none() {
            tracing::warn!("Didn't have hash available for confirmation: {}", new_final_hash);
            return (TMStatus::Indeterminate, TMStatusReason::NeedsBlock { hash: new_final_hash.0 });
        }

        // Linearity (FINALITY.md §3.4): `snapshot(parent(B)) ⪯bc snapshot(B)`. With BFT Final
        // Agreement this is what makes the snapshots of final bft-blocks bc-linear.
        if let Some(parent_snapshot_hash) = parent_snapshot_hash {
            if parent_snapshot_hash != new_final_hash {
                match read_state.is_ancestor_of(parent_snapshot_hash, new_final_hash) {
                    Some(true) => {}
                    Some(false) => {
                        tracing::warn!(
                            "BFT block violates Linearity: its snapshot {} does not extend its parent's snapshot {}",
                            new_final_hash, parent_snapshot_hash,
                        );
                        return fail;
                    }
                    // One of the two is not placed on a chain here yet; ask again rather than
                    // reject, exactly as a missing snapshot does above.
                    None => {
                        return (TMStatus::Indeterminate, TMStatusReason::NeedsBlock { hash: parent_snapshot_hash.0 });
                    }
                }
            }
        }

        (TMStatus::Pass, TMStatusReason::None)
    }

    /// Store a decided block, finalize its snapshot, and answer with the next height's roster.
    fn decide(
        &mut self,
        new_block: BftBlock,
        fat_pointer: FatPointerToBftBlock,
        proposal_sigs: Vec<TMSig>,
        reply: DecisionReply,
        read_state: &ReadState,
        block_writer: &mut WriteBlockWorkerTask,
    ) {
        let hardforks = self.hardforks.rules();
        if fat_pointer.points_at_block_hash() != new_block.blake3_hash() {
            panic!(
                "Fat Pointer hash does not match block hash. fp: {} block: {}",
                fat_pointer.points_at_block_hash(), new_block.blake3_hash(),
            );
        }
        // Vote namespacing: the precommit signatures were made at this block's height with that
        // height's namespace folded in, so verify with the same namespace.
        let vote_namespace = namespace_for_bft_height(hardforks, new_block.height as u64);
        if !fat_pointer.validate_signatures(&vote_namespace) {
            panic!("Signatures are not valid. Rejecting block.");
        }

        let mut chain = BFT_CHAIN.write().unwrap();
        assert_eq!(self.validate(&chain, read_state, &new_block), (TMStatus::Pass, TMStatusReason::None));

        // The `snapshot`: the parent of the deepest carried header, i.e. the block being
        // finalized. See `BftBlock::snapshot_block_hash`.
        let new_final_hash = Hash(new_block.snapshot_block_hash().0);
        let new_final_height = read_state.known_block(new_final_hash).unwrap().height;
        // `height` is the 0-based canonical height, i.e. the chain index directly.
        let insert_i = new_block.height as usize;

        // @Hack: Ensure there are enough blocks to overwrite this at the correct index.
        for i in chain.blocks.len()..=insert_i {
            chain.blocks.push(BftBlock {
                version: 0,
                height: i as u32,
                previous_block_fat_ptr: FatPointerToBftBlock {
                    vote_for_block_without_finalizer_public_key: [0u8; 76 - 32],
                    signatures: Vec::new(),
                },
                headers: Vec::new(),
                hardforks: Vec::new(),
                do_not_include_until_bc_height: 0,
            });
        }
        if insert_i > 0 {
            assert_eq!(
                chain.blocks[insert_i - 1].blake3_hash(),
                new_block.previous_block_fat_ptr.points_at_block_hash()
            );
        }
        assert!(insert_i == 0 || new_block.previous_block_hash() != Blake3Hash([0u8; 32]));
        assert!(chain.blocks[insert_i].headers.is_empty(), "{:?}", chain.blocks[insert_i]);
        assert!(!new_block.headers.is_empty());
        chain.hash_to_height.insert(new_block.blake3_hash(), insert_i as u64);
        chain.blocks[insert_i] = new_block.clone();
        chain.fat_pointer_to_tip = fat_pointer.clone();
        chain.bft_final_snapshot = Some((new_final_height, new_final_hash));
        drop(chain);

        // A decision no longer commits. It advances `bft_final_snapshot`, which may name a block
        // on a chain that is not `bc_best`; `fin` moves only where the best chain changes, and
        // only by the §3.2 rule (FINALITY.md §4.3).
        self.finish_decision(
            DecidedBlock { new_final_hash, new_final_height, block: new_block, fat_pointer, proposal_sigs, reply },
            block_writer,
        );
    }

    fn finish_decision(&mut self, decision: DecidedBlock, block_writer: &mut WriteBlockWorkerTask) {
        let DecidedBlock { new_final_hash, new_final_height, block, fat_pointer, proposal_sigs, reply } = decision;
        let hardforks = self.hardforks.rules();

        // The roster the next height votes with is the stake at this block's snapshot, read from
        // the chain rather than taken from the finalize result (FINALITY.md §7). The finalize
        // above put the snapshot in the finalized database, which is the only place that holds
        // aggregated stakes.
        let got_stakes = block_writer
            .non_finalized_state
            .aggregated_stakes_at(new_final_hash)
            .or_else(|| block_writer.finalized_state.db.aggregated_stakes(&new_final_hash))
            .unwrap_or_default();

        let mut chain = BFT_CHAIN.write().unwrap();
        if !got_stakes.is_empty() {
            chain.roster = got_stakes
                .into_iter()
                .map(|s| RosterMember { pub_key: s.0, voting_power: s.1, txids: Vec::new() })
                .collect();
        } else {
            // Zero aggregated stakes, having previously had real stake.
            //
            // This is legitimately reachable: unbonding every active bond drives total stake to
            // zero, and the chain has to keep running across that gap. Neither adopting the
            // empty set nor dying is right: an empty roster would leave BFT with no validators
            // at all, and that is not what the chain means. The last known-consistent validator
            // set is still the best available answer, so the previous roster is carried forward
            // unchanged -- a "ghost" roster -- until a new bond yields a fresh, consistent stake
            // set, at which point the branch above replaces it wholesale. Carrying over is
            // implicit: `roster` is deliberately NOT written here.
            //
            // A torn stake cache presents identically, so this is still logged loudly. If the
            // roster never recovers once new bonds exist, that is the case to suspect, and
            // `zebrad --fixup-db-stake` remains the repair path.
            if chain.roster.iter().any(|val| val.voting_power > 1) {
                tracing::warn!(
                    "No aggregated stakes for block {} at height {:?}; carrying the previous \
                     roster forward as a ghost roster ({} finalizers) until a new bond \
                     establishes a consistent stake set. Expected when every bond has been \
                     unbonded. If it persists once new bonds exist the stake cache may be torn \
                     -- stop the node and run `zebrad --fixup-db-stake` to repair it.",
                    new_final_hash, new_final_height, chain.roster.len(),
                );
            }
        }

        // The returned roster is for the NEXT height (tenderlink advances to it after this
        // decision): its index is the new chain length. Exclude finalizers terminated at that
        // height, inclusive, so they are already out of the roster that will vote on a hardfork
        // block scheduled there.
        let next_bft_height = chain.blocks.len() as u64;
        let terminated = terminated_finalizers_at(hardforks, next_bft_height, new_final_height.0 as u64);
        let roster = tenderlink_roster_from_internal(&chain.roster, &terminated);
        drop(chain);

        tracing::info!(
            "Successfully decided BFT block at height {} finalizing {}",
            block.height, new_final_hash,
        );

        // The decision is stored after the chain lock is dropped, so readers never wait on a
        // disk write. A crash before the row lands leaves the BFT chain one height short and
        // that height is decided again on the next run.
        if let Err(err) = block_writer.finalized_state.db.write_bft_decision(
            block.height,
            &block,
            &fat_pointer,
            &proposal_sigs,
        ) {
            tracing::error!("could not store BFT decision at height {}: {err}", block.height);
        }

        match reply {
            DecisionReply::Tenderlink(reply) => {
                // Vote namespacing: the next height's namespace is the cumulative hardfork hash
                // inclusive of any hardfork scheduled at that next height.
                let namespace = namespace_for_bft_height(hardforks, block.height as u64 + 1);
                let _ = reply.send((roster, namespace));
            }
            DecisionReply::ForceFeed(reply) => {
                let _ = reply.send(Ok(()));
            }
        }
    }

    /// Replay the persisted chain into the store, and start tenderlink at its next height if it
    /// holds anything. An empty store means this node has not bootstrapped yet: BFT genesis is
    /// built once the PoW chain reaches the activation height (see `tick`).
    ///
    /// Rosters are recomputed from the bonds at each height's snapshot, never read back as
    /// stored bytes: a roster that disagreed with the stored votes would re-index every one of
    /// them through seats that never voted (FINALITY.md §8.1).
    fn restore(&mut self, read_state: &ReadState, block_writer: &mut WriteBlockWorkerTask) {
        let hardforks = self.hardforks.clone();
        let hardforks = hardforks.rules();
        let stored = block_writer.finalized_state.db.bft_chain();

        // A node that has mined past the activation height has decided BFT genesis, so an empty
        // BFT chain there is a database written before the chain was stored. It is not migrated
        // and not re-bootstrapped: re-deriving genesis over a chain that already ran BFT would
        // put this node on a different decided chain than its peers.
        if stored.is_empty() {
            if let (Some(activation_height), Some((tip, _))) =
                (self.params.bootstrap.activation_height(), read_state.best_tip())
            {
                if tip.0 > activation_height && self.launch.is_some() {
                    tracing::error!(
                        "this database is past the BFT activation height ({}) but holds no decided \
                         BFT chain, so it predates BFT storage in the finalized database. Delete \
                         {} and resync.",
                        activation_height,
                        block_writer.finalized_state.db.path().display(),
                    );
                    std::process::exit(1);
                }
            }
        }

        let mut ingest: Vec<RoundData> = Vec::new();
        let mut blocks: Vec<BftBlock> = Vec::new();
        let mut fat_pointer_to_tip = FatPointerToBftBlock::null();
        // The roster that voted on the block about to be replayed. BFT genesis is decided by
        // the nil validator set (see `build_bootstrap_genesis`), so replay starts empty; each
        // later height votes with the stakes at the previous height's snapshot.
        let mut unsorted_roster: Vec<RosterMember> = Vec::new();
        // BC height finalized by the previous replayed block's cert; the roster voting on block
        // N was formed at N-1's decision, so N's replayed roster must use this. Using this
        // block's own height jailed and unjailed finalizers one cert early at a hardfork
        // activation boundary.
        let mut prev_finalized_bc_height: u64 = 0;

        for decision in stored {
            let StoredDecision { block, fat_pointer, proposal_sigs } = decision;
            if block.previous_block_fat_ptr.points_at_block_hash() != fat_pointer_to_tip.points_at_block_hash() {
                break;
            }

            // Historical round replay: filter the roster exactly as the live path did at this
            // height, so it matches the roster that actually voted on this decided block.
            let this_bft_height = ingest.len() as u64;
            let this_terminated = terminated_finalizers_at(hardforks, this_bft_height, prev_finalized_bc_height);
            let roster = tenderlink_roster_from_internal(&unsorted_roster, &this_terminated);

            // The roster the next height votes with, and the watermark that filters it, both
            // come from this block's snapshot -- the same two reads `finish_decision` makes
            // live. An unresolvable snapshot keeps the last known values: monotone, and correct
            // whenever the database is intact.
            if !block.headers.is_empty() {
                let snapshot = Hash(block.snapshot_block_hash().0);
                if let Some(known) = read_state.known_block(snapshot) {
                    prev_finalized_bc_height = known.height.0 as u64;
                }
                let stakes = block_writer
                    .non_finalized_state
                    .aggregated_stakes_at(snapshot)
                    .or_else(|| block_writer.finalized_state.db.aggregated_stakes(&snapshot))
                    .unwrap_or_default();
                // Empty stakes are the ghost roster of `finish_decision`: the previous roster is
                // carried forward rather than replaced, so replay reproduces what voted.
                if !stakes.is_empty() {
                    unsorted_roster = stakes
                        .into_iter()
                        .map(|s| RosterMember { pub_key: s.0, voting_power: s.1, txids: Vec::new() })
                        .collect();
                }
            }

            ingest.push(decided_round_data(hardforks, &block, &fat_pointer, roster, proposal_sigs, this_bft_height));
            blocks.push(block);
            fat_pointer_to_tip = fat_pointer;
        }

        let mut new_final_hash = Hash([0; 32]);
        let mut new_final_height = Height(0);
        if let Some(new_block) = blocks.last() {
            new_final_hash.0 = new_block.snapshot_block_hash().0;
            match read_state.known_block(new_final_hash) {
                Some(known) => new_final_height = known.height,
                None => {
                    // The bc-chain is behind the BFT chain, which the two-stores split used to
                    // make possible. Both now live in the same database and commit together, so
                    // this is a damaged database rather than a configuration mistake.
                    tracing::error!(
                        "the decided BFT chain finalizes block {}, which this database does not \
                         hold. Delete {} and resync.",
                        new_final_hash,
                        block_writer.finalized_state.db.path().display(),
                    );
                    std::process::exit(1);
                }
            }
        }

        // Startup roster is for the next height to decide (the loaded chain length), with the
        // terminated finalizers excluded inclusively at that height.
        let startup_bft_height = blocks.len() as u64;
        let terminated = terminated_finalizers_at(hardforks, startup_bft_height, new_final_height.0 as u64);
        let roster = tenderlink_roster_from_internal(&unsorted_roster, &terminated);

        let loaded_any = !blocks.is_empty();
        {
            let mut chain = BFT_CHAIN.write().unwrap();
            chain.roster = unsorted_roster;
            chain.hash_to_height = blocks.iter().enumerate().map(|(i, b)| (b.blake3_hash(), i as u64)).collect();
            chain.blocks = blocks;
            chain.fat_pointer_to_tip = fat_pointer_to_tip;
            if new_final_hash != Hash([0; 32]) {
                chain.bft_final_snapshot = Some((new_final_height, new_final_hash));
            }
        }

        if loaded_any {
            tracing::info!(
                "crosslink restore: resuming BFT at height {}, finalizing bc height {}",
                startup_bft_height, new_final_height.0,
            );
            self.spawn_tenderlink(roster, ingest);
        }
    }

    /// The deterministic BFT genesis block: the decision that finalizes the bootstrap roster
    /// height (h1), which every node constructs identically from its own PoW chain instead of
    /// receiving.
    ///
    /// It is shaped exactly as a proposer would shape it -- v2, the sigma confirmation headers
    /// above h1, any hardforks scheduled at BFT height 0 -- so `validate` accepts it unchanged.
    /// Its fat pointer names the block at height 0, round 0, and carries no signatures: the
    /// validator set for genesis is nil and the decision is valid by construction. BFT height 1
    /// is the first real decision, and its previous-block pointer is this one.
    ///
    /// None until the chain has the headers (the caller only asks once the tip is at or past the
    /// activation height, where h1 is finalized-by-depth, so the headers are the same on every
    /// node).
    fn build_bootstrap_genesis(&self, read_state: &ReadState) -> Option<(BftBlock, FatPointerToBftBlock)> {
        let params = &self.params;
        let BftBootstrap::FromChain { roster_height, .. } = params.bootstrap else {
            return None;
        };
        // h1 is the snapshot, so the carried headers are the sigma blocks above it:
        // h1+1 ..= h1+sigma (see BftBlock).
        let mut headers: Vec<BcBlockHeader> = Vec::with_capacity(params.bc_confirmation_depth_sigma as usize);
        for h in roster_height + 1..=roster_height + params.bc_confirmation_depth_sigma as u32 {
            let (header, ..) = read_state.block_header(Height(h).into())?;
            headers.push(bc_hdr_to_lrz(&header));
        }
        let mut block = match BftBlock::try_from(params, 0, FatPointerToBftBlock::null(), headers) {
            Ok(block) => block,
            Err(e) => {
                tracing::error!("Unable to build the bootstrap genesis BFT block: {:?}", e);
                return None;
            }
        };
        block.version = 2;
        let scheduled_hardforks: Vec<HardForkConfig> = self
            .hardforks
            .rules()
            .iter()
            .filter(|hf| hf.bft_certificate_height == 0)
            .cloned()
            .collect();
        if let Some(last) = scheduled_hardforks.last() {
            block.do_not_include_until_bc_height = last.pow_activation_height;
            block.hardforks = scheduled_hardforks;
        }
        let fat_pointer = FatPointerToBftBlock::from_parts(block.blake3_hash(), 0, 0, &[]);
        Some((block, fat_pointer))
    }

    /// Decide genesis (finalizing h1 on the PoW side and taking h1's aggregated stakes as the
    /// roster for height 1) and start tenderlink at height 1.
    fn bootstrap(&mut self, read_state: &ReadState, block_writer: &mut WriteBlockWorkerTask) {
        let BftBootstrap::FromChain { roster_height, activation_height } = self.params.bootstrap else {
            return;
        };
        // Genesis is decided exactly once. `decide` asserts the block it is given validates
        // against the chain, and a genesis already at index 0 fails that assert -- which under
        // `panic = abort` takes the node down.
        if BFT_CHAIN.read().unwrap().blocks.first().is_some_and(|b| !b.headers.is_empty()) {
            return;
        }
        let Some((genesis, fat_pointer)) = self.build_bootstrap_genesis(read_state) else {
            return;
        };
        tracing::info!(
            "crosslink bootstrap: PoW reached height {}; deciding BFT genesis {} which finalizes height {}",
            activation_height, genesis.blake3_hash(), roster_height,
        );
        let (reply, roster_rx) = tokio::sync::oneshot::channel();
        self.decide(genesis.clone(), fat_pointer.clone(), Vec::new(), DecisionReply::Tenderlink(reply), read_state, block_writer);
        self.pending_bootstrap = Some(PendingBootstrap { genesis, fat_pointer, roster_rx });
        self.finish_bootstrap();
    }

    /// Start tenderlink once the bootstrap genesis decision has committed. Genesis finalizes a
    /// block every chain at the activation height holds, so this normally succeeds on the tick
    /// that decided it.
    fn finish_bootstrap(&mut self) {
        let Some(pending) = self.pending_bootstrap.as_mut() else { return; };
        let roster = match pending.roster_rx.try_recv() {
            Ok((roster, _)) => roster,
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => return,
            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                tracing::error!("crosslink bootstrap: genesis could not be finalized");
                self.pending_bootstrap = None;
                return;
            }
        };
        let PendingBootstrap { genesis, fat_pointer, .. } = self.pending_bootstrap.take().unwrap();
        let hardforks = self.hardforks.rules();
        let terminated = terminated_finalizers_at(hardforks, 0, 0);
        let genesis_round = decided_round_data(
            hardforks,
            &genesis,
            &fat_pointer,
            tenderlink_roster_from_internal(&[], &terminated),
            Vec::new(),
            0,
        );
        self.spawn_tenderlink(roster, vec![genesis_round]);
    }

    /// Start tenderlink at the height after `ingest` with `roster`, and mark BFT active. Refuses
    /// an empty roster: BFT height 1's roster is fixed by the stakes at h1, so nothing would ever
    /// change. Dropping the launch on that path is what stops the caller retrying: genesis is
    /// already decided and stored, and `decide` refuses to decide it twice.
    fn spawn_tenderlink(&mut self, roster: Vec<SortedRosterMember>, ingest: Vec<RoundData>) {
        if roster.is_empty() {
            tracing::error!(
                "BFT height {} has an empty roster: no stake was bonded by the bootstrap roster height ({:?}). BFT will not run on this chain.",
                ingest.len(), self.params.bootstrap,
            );
            self.launch = None;
            return;
        }
        let Some(launch) = self.launch.take() else { return; };
        tracing::info!("starting tenderlink at BFT height {} with {} finalizer(s)", ingest.len(), roster.len());
        BFT_CHAIN.write().unwrap().is_activated = true;

        let (static_keypair, endpoint) = addr_string_to_stuff(&launch.public_address);
        // `peer_addresses` is only a list of addresses to seed connections from. Keys are
        // learned on connect (tenderlink verifies the peer's key and rewrites its address map),
        // so the hint here is deliberately nil: the roster comes from the chain, never from
        // this list.
        let finalizer_peer_addresses: Vec<FinalizerPeerAddress> = launch
            .peer_addresses
            .iter()
            .map(|peer| FinalizerPeerAddress { bft_pk: PubKeyID::NIL, address: addr_string_to_stuff(peer).1 })
            .collect();
        // Vote namespacing: the startup height is the number of ingested (decided) rounds.
        let initial_vote_namespace = namespace_for_bft_height(self.hardforks.rules(), ingest.len() as u64);

        self.rt.spawn(tenderlink::entry_point(
            launch.signing_key,
            Some(static_keypair),
            Some(endpoint),
            roster,
            finalizer_peer_addresses,
            None,
            tenderlink::ClosureToProposeNewBlock(Arc::new(move || {
                Box::pin(async move {
                    let (reply, rx) = tokio::sync::oneshot::channel();
                    if !send_request(BftRequest::Propose { reply }) {
                        return None;
                    }
                    rx.await.ok().flatten().map(|block| BlockValue(block.zcash_serialize_to_vec().unwrap()))
                })
            })),
            tenderlink::ClosureToValidateProposedBlock(Arc::new(move |block| {
                let parsed = BftBlock::zcash_deserialize(&block.0[..]);
                Box::pin(async move {
                    let Ok(block) = parsed else {
                        tracing::error!("Failed to deserialize Tenderlink payload.");
                        return (TMStatus::Fail, TMStatusReason::None);
                    };
                    let (reply, rx) = tokio::sync::oneshot::channel();
                    if !send_request(BftRequest::Validate { block, reply }) {
                        return (TMStatus::Indeterminate, TMStatusReason::None);
                    }
                    rx.await.unwrap_or((TMStatus::Indeterminate, TMStatusReason::None))
                })
            })),
            tenderlink::ClosureToPushDecidedBlock(Arc::new(move |block, fat_pointer, proposal_sigs| {
                Box::pin(async move {
                    let block = BftBlock::zcash_deserialize(&block.0[..]).unwrap();
                    let (reply, rx) = tokio::sync::oneshot::channel();
                    if !send_request(BftRequest::Decided { block, fat_pointer, proposal_sigs, reply }) {
                        panic!("new_network is not running; cannot store a decided BFT block");
                    }
                    rx.await.expect("new_network dropped a decided BFT block")
                })
            })),
            tenderlink::ClosureToUpdatePeers(Arc::new(move |all_peers| {
                let mut peer_strings = Vec::with_capacity(all_peers.len());
                for peer in &all_peers {
                    peer_strings.push(format!(
                        "{} {} ({})",
                        peer.root_public_bft_key.map_or_else(|| "unknown peer".to_string(), |k| k.to_string()),
                        if peer.connected { "connected" } else { "disconnected" },
                        peer.latest_status_request_height,
                    ));
                }
                BFT_CHAIN.write().unwrap().peer_strings = peer_strings;
                Box::pin(async {})
            })),
            tenderlink::ClosureToAccessBft(Arc::new(move |bft_state: &TMState, bft_key_address_map: &BftAddressMap| {
                RECENCY_STATUS.send_replace(recency_status_from(bft_state, bft_key_address_map));
                Box::pin(async {})
            })),
            ingest,
            initial_vote_namespace,
        ));
    }
}

fn recency_status_from(bft_state: &TMState, bft_key_address_map: &BftAddressMap) -> TFLRecencyStatus {
    let now_utc = chrono::Utc::now().timestamp();
    let mut finalizer_statuses = Vec::<(PubKeyID, FinalizerRecencyStatus)>::new();

    for round in &bft_state.rounds_data {
        let is_my_height = round.height == bft_state.height;

        // The vote arrays are sized to the *active* roster (the top ACTIVE_ROSTER_MAX_N by
        // stake); members past that have no slot.
        let active_n = round.msg_val_sigs.len().min(round.msg_nil_sigs.len());
        for (roster_i, member) in round.roster.iter().take(active_n).enumerate() {
            let st = if let Some(v) = finalizer_statuses.iter_mut().find(|(key, _st)| *key == member.pub_key) {
                v
            } else {
                let last_i = finalizer_statuses.len();
                finalizer_statuses.push((member.pub_key, FinalizerRecencyStatus::default()));
                &mut finalizer_statuses[last_i]
            };

            // Simple (not weighted) counts.
            let cs = ConsensusCounts::from(&(round.msg_val_sigs[roster_i], round.msg_nil_sigs[roster_i], 1));
            if cs.anys > 0 && is_my_height {
                st.1.no_yes_votes_in_my_height[0][0] += cs.nil_prevotes;
                st.1.no_yes_votes_in_my_height[0][1] += cs.yes_prevotes;
                // There are no explicit nil precommits.
                st.1.no_yes_votes_in_my_height[1][0] += cs.precommits.saturating_sub(cs.yes_precommits);
                st.1.no_yes_votes_in_my_height[1][1] += cs.yes_precommits;
                st.1.highest_round_vote = st.1.highest_round_vote.max(round.round);
            }

            if member.pub_key == bft_state.my_pub_key {
                st.1.last_direct_connection_utc = Some(now_utc);
            } else {
                let utc = bft_key_address_map.last_packet_utcs.get(&member.pub_key);
                st.1.last_direct_connection_utc = st.1.last_direct_connection_utc.max(utc.copied());
            }
        }
    }

    TFLRecencyStatus {
        now_utc,
        my_height: bft_state.height,
        my_round: bft_state.round,
        my_step: match bft_state.step {
            TMStep::Propose => 0,
            TMStep::Prevote => 1,
            TMStep::Precommit => 2,
        },
        my_locked_round: bft_state.locked_value_round.1,
        my_valid_round: bft_state.valid_value_round.1,
        finalizer_statuses,
    }
}
