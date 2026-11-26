//! The decided BFT chain and everything that reads it.
//!
//! The chain is written only by the `new_network` sync thread (see [`BftRunner`]), which also
//! owns the block writer and the best-chain view, so every rule that reads both the bft-chain and
//! the bc-chain -- proposal, validation, fat-pointer admission, the block-template walk -- runs
//! against one consistent view on one thread. Tenderlink's closures that need that thread marshal a
//! request to it: propose and validate await one reply; a decision tenderlink reached waits for the
//! published decided tip (`DECIDED_TIP`) to pass its height instead. Reading the tip and reading a
//! decided round back are done on tenderlink's own task. Other readers (the RPC, the visualizer,
//! the test harness) take the read lock on [`bft_chain`].
#![allow(missing_docs)]

use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::BuildHasherDefault;
use std::sync::{Arc, LazyLock, Mutex, RwLock};

use chrono::{DateTime, Utc};
use tenderlink::{
    addr_string_to_stuff, BftAddressMap, BlockValue, ConsensusCounts, FinalizerPeerAddress,
    RoundData, SortedRosterMember, TMState, TMStatus, TMStatusReason, TMStep, ValueId,
};
use zcash_primitives::bft::{
    BftBlock, BftBootstrap, Blake3Hash, FatPointerToBftBlock, FinalizerRecencyStatus,
    ConflictingQuorum, SlashForkConfig, HashKeys, PubKeyID, TFLRecencyStatus, TMSig, ZcashCrosslinkParameters,
    // FINALITY_LIVENESS_ALLOWANCE, // only used by the disabled variable-reward gate
};
use zcash_primitives::block::{
    BlockHash, BlockHeader as BcBlockHeaderWrap, BlockHeaderData as BcBlockHeader,
};
use zcash_primitives::transaction::RosterMember;
use zcash_protocol::consensus::{BranchId, NetworkUpgrade, Parameters as _};
use zebra_chain::block::{Hash, Header, Height};
use zebra_chain::parameters::Network;
use zebra_chain::serialization::{ZcashDeserialize, ZcashSerialize};
use zebra_chain::work::difficulty::CompactDifficulty;

use super::ReadState;
use crate::service::check::{self, difficulty::POW_ADJUSTMENT_BLOCK_SPAN, AdjustedDifficulty};
use crate::service::finalized_state::bft::StoredDecision;
use crate::service::finalized_state::ZebraDb;
use crate::service::write::WriteBlockWorkerTask;
use crate::CrosslinkVerdict;

/// The highest BFT block version this node understands, and the one it proposes.
pub const BFT_BLOCK_VERSION_LATEST: u32 = 2;

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
    /// Decided blocks by BFT height, of which only the newest are in memory: `decided_block`
    /// reads an older one. A block with no headers is a placeholder left by out-of-order ingest
    /// and is never a valid pointer target.
    pub blocks: HeldBlocks,
    /// Block hash -> 0-based height, for every decided height. Real blocks only; append-only, so
    /// entries are never invalidated. `blake3_hash()` re-serializes the whole block, so
    /// resolving a pointer by scanning costs ~150 MB of hashing at 9.5k blocks.
    pub hash_to_height: HashMap<Blake3Hash, u64, BuildHasherDefault<Blake3HashFold>>,
    /// The newest decided height and what follows from it, replaced whole with each decision so
    /// a reader on any thread sees one decision's state or the next, never a mix. `None` before
    /// genesis is decided.
    pub tip: Option<Arc<DecidedTip>>,
    /// The roster for the next height to decide: the stakes at the tip's snapshot, or the last
    /// non-empty roster when those are empty (the ghost roster; see `apply_decision`).
    pub roster: Vec<RosterMember>,
    /// Who voted at each decided height, indexed like `blocks`: what checking that height's
    /// certificate needs. Heights with the same voting roster share one entry.
    pub voting_rosters: Vec<Arc<VotingRoster>>,
    /// True once tenderlink is running.
    pub is_activated: bool,
    pub peer_strings: Vec<String>,
}

impl BftChain {
    /// A chain with nothing decided.
    pub fn new() -> Self {
        Self {
            blocks: HeldBlocks::new(),
            hash_to_height: HashMap::default(),
            tip: None,
            roster: Vec::new(),
            voting_rosters: Vec::new(),
            is_activated: false,
            peer_strings: Vec::new(),
        }
    }

    /// The certificate of the newest decided block, or the null pointer before genesis.
    pub fn fat_pointer_to_tip(&self) -> FatPointerToBftBlock {
        self.tip.as_ref().map_or_else(FatPointerToBftBlock::null, |tip| tip.certificate.clone())
    }

    /// The newest decided block's hash: what the next block's previous-block pointer names. The
    /// null pointer's before genesis.
    pub fn tip_block_hash(&self) -> Blake3Hash {
        self.tip.as_ref().map_or_else(|| FatPointerToBftBlock::null().points_at_block_hash(), |tip| tip.block_hash)
    }

    /// The snapshot of the last decided bft-block: what `Π_bft` has finalized, which is not
    /// necessarily on this node's best chain and is never the node's own `fin` (FINALITY.md §2).
    pub fn bft_final_snapshot(&self) -> Option<(Height, Hash)> {
        self.tip.as_ref().and_then(|tip| tip.final_snapshot)
    }
}

/// The newest decided BFT height and everything that follows from it. A decision builds one and
/// swaps it in under the same lock that adds the block, so the next height's roster can never be
/// read from a chain that has the block but not yet the stakes at its snapshot.
#[derive(Clone, Debug)]
pub struct DecidedTip {
    pub height: u64,
    pub block_hash: Blake3Hash,
    /// The decided block's certificate: what miners cite for the tip.
    pub certificate: FatPointerToBftBlock,
    /// The PoW block the newest decision with a known snapshot finalizes: a decision whose
    /// snapshot this state no longer holds (a replay with BFT dropped) carries the last one
    /// forward. `None` only when no snapshot was ever known.
    pub final_snapshot: Option<(Height, Hash)>,
    /// The roster tenderlink is handed for the next height, sorted.
    pub next_roster: Vec<SortedRosterMember>,
    /// Who votes on the next height, under which namespace and quorum rule: what a certificate
    /// for it is checked against.
    pub next_voting: Arc<VotingRoster>,
}

/// The finalizers who vote at a BFT height and how their votes are read: the namespace they sign
/// under and whether their stake must reach the strict quorum. A certificate for a block at that
/// height is checked against these, so they are kept per decided height: the stake they come
/// from is the stake at a snapshot a later reader may no longer hold.
#[derive(Debug)]
pub struct VotingRoster {
    /// The active part of the roster, in roster order: at most the network's
    /// `active_roster_max_n` members, cut when the roster is built. Nobody past them votes,
    /// and the whole roster is as long as the bond set.
    pub roster: Vec<SortedRosterMember>,
    pub vote_namespace: [u8; 32],
    pub strict_quorum: bool,
}

impl PartialEq for VotingRoster {
    fn eq(&self, other: &Self) -> bool {
        self.vote_namespace == other.vote_namespace
            && self.strict_quorum == other.strict_quorum
            && self.roster.len() == other.roster.len()
            && self.roster.iter().zip(&other.roster).all(|(a, b)| (a.pub_key, a.stake) == (b.pub_key, b.stake))
    }
}

/// How many of the newest decided BFT blocks stay in memory. A block weighs about 15 KB, and
/// rules about the tip read only the last few; what reads further back (a PoW block that cites
/// an old BFT block during catch-up, the visualizer's history) reads the database.
pub const BFT_BLOCKS_HELD: usize = 1024;

/// The decided BFT blocks, holding the newest `BFT_BLOCKS_HELD` of them. Lengths and heights
/// are those of the whole decided chain, so a height below `base` is decided but not held.
#[derive(Debug)]
pub struct HeldBlocks {
    /// The height of the oldest held block: how many blocks are in the database only.
    base: usize,
    held: VecDeque<BftBlock>,
}

impl HeldBlocks {
    pub const fn new() -> Self {
        Self { base: 0, held: VecDeque::new() }
    }

    /// How many heights are decided, held or not: the next height to decide.
    pub fn len(&self) -> usize {
        self.base + self.held.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The lowest height held in memory.
    pub fn base(&self) -> usize {
        self.base
    }

    /// The newest decided block.
    pub fn last(&self) -> Option<&BftBlock> {
        self.held.back()
    }

    /// The block at `height` if it is held: `None` below `base` as well as past the tip.
    /// `decided_block` also reads the ones below.
    pub fn get(&self, height: usize) -> Option<&BftBlock> {
        self.held.get(height.checked_sub(self.base)?)
    }

    /// The held blocks with their heights, oldest first.
    pub fn held(&self) -> impl DoubleEndedIterator<Item = (usize, &BftBlock)> {
        self.held.iter().enumerate().map(|(i, block)| (self.base + i, block))
    }

    /// Appends the block of the next height, letting go of the oldest held one past the limit.
    pub fn push(&mut self, block: BftBlock) {
        self.held.push_back(block);
        if self.held.len() > BFT_BLOCKS_HELD {
            self.held.pop_front();
            self.base += 1;
        }
    }
}

impl Default for HeldBlocks {
    fn default() -> Self {
        Self::new()
    }
}

/// The database the decided chain is stored in, for reading back what memory no longer holds.
/// Set when the runner is made.
static BFT_DB: RwLock<Option<ZebraDb>> = RwLock::new(None);

/// The decided block at `height`: the one held in memory, or the stored one below those.
pub fn decided_block(chain: &BftChain, height: usize) -> Option<Cow<'_, BftBlock>> {
    match chain.blocks.get(height) {
        Some(block) => Some(Cow::Borrowed(block)),
        None if height < chain.blocks.base() => {
            let db = BFT_DB.read().unwrap();
            db.as_ref()?.bft_decision(u32::try_from(height).ok()?).map(|decision| Cow::Owned(decision.block))
        }
        None => None,
    }
}

/// The decided block with this hash, held or stored.
pub fn decided_block_by_hash(chain: &BftChain, hash: [u8; 32]) -> Option<Cow<'_, BftBlock>> {
    decided_block(chain, *chain.hash_to_height.get(&Blake3Hash(hash))? as usize)
}

/// The decided blocks of `count` heights from `from` on, held or stored: for a reader that walks
/// the history (the visualizer). Shorter than `count` where the chain or the database ends.
pub fn decided_blocks(chain: &BftChain, from: usize, count: usize) -> Vec<BftBlock> {
    let mut blocks = Vec::new();
    let stored_n = chain.blocks.base().saturating_sub(from).min(count);
    if stored_n > 0 {
        let db = BFT_DB.read().unwrap();
        let (Some(db), Ok(from), Ok(stored_n)) = (db.as_ref(), u32::try_from(from), u32::try_from(stored_n)) else {
            return blocks;
        };
        blocks.extend(db.bft_decisions(from, stored_n, usize::MAX).into_iter().map(|decision| decision.block));
        if blocks.len() < stored_n as usize {
            return blocks;
        }
    }
    blocks.extend((from + blocks.len()..from + count).map_while(|height| chain.blocks.get(height).cloned()));
    blocks
}

static BFT_CHAIN: LazyLock<RwLock<BftChain>> = LazyLock::new(|| {
    RwLock::new(BftChain::new())
});

/// The decided bft-chain. Written only by the sync thread; never hold the guard across an await.
pub fn bft_chain() -> &'static RwLock<BftChain> {
    &BFT_CHAIN
}

/// PoW blocks needed to validate a received BFT proposal: its snapshot, or its parent's, each with
/// when a validation last asked for it.
///
/// Tenderlink can deliver a proposal before the PoW synchronizer sees its snapshot. Keep the exact
/// missing hashes here so the sync loop can request them directly instead of waiting for them to
/// happen to appear in a peer's moving near-tip window.
///
/// Any proposer names any hash here, so the set is bounded: an entry expires unless a validation
/// asks for it again within `MISSING_POW_BLOCK_FRESHNESS`, and it never holds more than
/// `MAX_MISSING_POW_BLOCKS`. Tenderlink re-validates an indeterminate proposal on every tick while
/// the network is still voting on it, so a hash stays wanted exactly as long as that. Blocks on
/// the decided chain are kept in `DECIDED_MISSING_POW_BLOCKS` instead, which neither expires nor
/// evicts.
static MISSING_POW_BLOCKS: LazyLock<Mutex<HashMap<Hash, std::time::Instant>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Thirty seconds is many BFT ticks and a few rounds; not measured against a live network.
const MISSING_POW_BLOCK_FRESHNESS: std::time::Duration = std::time::Duration::from_secs(30);

/// Honest validation needs a handful of hashes at once: the snapshot of each proposal in play and
/// the parent's snapshot. Not measured; it only has to sit above that and well below the download
/// budget.
const MAX_MISSING_POW_BLOCKS: usize = 16;

fn needs_pow_block(hash: Hash) -> (TMStatus, TMStatusReason) {
    if bft_is_dropped() {
        return (TMStatus::Indeterminate, TMStatusReason::None);
    }
    note_missing_pow_block(&mut MISSING_POW_BLOCKS.lock().unwrap(), hash, super::clock());
    (TMStatus::Indeterminate, TMStatusReason::NeedsBlock { hash: hash.0 })
}

/// Ask for `hash` as of `now`, evicting the least recently asked-for other hash past the cap.
fn note_missing_pow_block(missing: &mut HashMap<Hash, std::time::Instant>, hash: Hash, now: std::time::Instant) {
    missing.insert(hash, now);
    if missing.len() > MAX_MISSING_POW_BLOCKS {
        let stalest = missing
            .iter()
            .filter(|(other, _)| **other != hash)
            .min_by_key(|(_, asked)| **asked)
            .map(|(other, _)| *other)
            .expect("the map holds more than the cap, so more than the hash just asked for");
        missing.remove(&stalest);
    }
}

/// Drop the hashes that are known now, or that no validation has asked for within the window.
fn retain_missing_pow_blocks(
    missing: &mut HashMap<Hash, std::time::Instant>,
    now: std::time::Instant,
    is_known: impl Fn(Hash) -> bool,
) {
    missing.retain(|hash, asked| now.saturating_duration_since(*asked) < MISSING_POW_BLOCK_FRESHNESS && !is_known(*hash));
}

pub(super) fn missing_pow_blocks(read_state: &ReadState) -> Vec<Hash> {
    // Known conflicting quorums are reported again every DROPPED_BFT_REPORT_INTERVAL.
    {
        let mut last = CONFLICTING_QUORUMS_LAST_REPORTED.lock().unwrap();
        if last.is_some_and(|at| at.elapsed() >= DROPPED_BFT_REPORT_INTERVAL) {
            *last = Some(std::time::Instant::now());
            for (conflict, _) in CONFLICTING_QUORUMS_REPORTED.lock().unwrap().iter() {
                tracing::error!(
                    "crosslink: CONFLICTING BFT QUORUM at height {} is still known: this node stored block {}, \
                     and {} block {}. Report this with both block hashes.",
                    conflict.height, Blake3Hash(conflict.stored), conflict.seen_as, Blake3Hash(conflict.other),
                );
            }
        }
    }
    // A dropped BFT is reported again every DROPPED_BFT_REPORT_INTERVAL.
    if let Some((message, last_report)) = DROPPED_BFT.lock().unwrap().as_mut() {
        if last_report.elapsed() >= DROPPED_BFT_REPORT_INTERVAL {
            tracing::error!("{message}");
            *last_report = std::time::Instant::now();
        }
        return Vec::new();
    }
    let mut missing = MISSING_POW_BLOCKS.lock().unwrap();
    retain_missing_pow_blocks(&mut missing, super::clock(), |hash| read_state.known_block(hash).is_some());
    let mut decided = DECIDED_MISSING_POW_BLOCKS.lock().unwrap();
    decided.retain(|hash| read_state.known_block(*hash).is_none());
    metrics::gauge!("state.crosslink.bft_decided_missing_pow_blocks").set(decided.len() as f64);
    let report = decided_missing_report(&mut DECIDED_MISSING_SINCE.lock().unwrap(), decided.len(), super::clock());
    if let (Some(waited), Some(example)) = (report, decided.iter().next()) {
        tracing::warn!(
            "crosslink: BFT has waited {} min for {} PoW block(s) on the decided chain that no peer has \
             supplied (one is {example}); it finalizes nothing until they arrive. Check that this node \
             has peers holding that chain.",
            waited.as_secs() / 60, decided.len(),
        );
    }
    missing.keys().chain(decided.iter().filter(|hash| !missing.contains_key(*hash))).copied().collect()
}

/// While `DECIDED_MISSING_POW_BLOCKS` is not empty: since when, and when it was last reported.
/// The set as a whole is timed, not each hash, because following previous-block hashes swaps one
/// hash for its parent while the wait goes on.
static DECIDED_MISSING_SINCE: Mutex<Option<(std::time::Instant, std::time::Instant)>> = Mutex::new(None);

/// How long the decided chain has waited, when that is due to be reported again: every
/// `DROPPED_BFT_REPORT_INTERVAL` while `missing` blocks are still missing, as a dropped BFT is.
/// The start of a wait is not reported here; restore and parking say so when it begins.
fn decided_missing_report(
    waiting: &mut Option<(std::time::Instant, std::time::Instant)>,
    missing: usize,
    now: std::time::Instant,
) -> Option<std::time::Duration> {
    if missing == 0 {
        *waiting = None;
        return None;
    }
    let (since, last_report) = waiting.get_or_insert((now, now));
    if now.saturating_duration_since(*last_report) < DROPPED_BFT_REPORT_INTERVAL {
        return None;
    }
    *last_report = now;
    Some(now.saturating_duration_since(*since))
}

/// PoW blocks this node lacks that are a decided snapshot or one of its ancestors.
///
/// A proposal alone puts a hash in `MISSING_POW_BLOCKS`, and one proposer can name any block. A
/// hash enters this set only once the proposal naming it holds a decision's worth of precommits
/// from this node's own roster, and from there only by following previous-block hashes. Every
/// hash here is therefore on the chain that Π_bft decided, which is what makes it safe to drop
/// BFT when one of them turns out to conflict with a committed block.
static DECIDED_MISSING_POW_BLOCKS: LazyLock<Mutex<HashSet<Hash>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

pub(super) fn is_decided_missing_pow_block(hash: Hash) -> bool {
    DECIDED_MISSING_POW_BLOCKS.lock().unwrap().contains(&hash)
}

/// Move the search for where the decided chain joins this node's chains from `hash` to its parent.
pub(super) fn decided_missing_pow_block_needs_parent(hash: Hash, parent: Hash) {
    let mut decided = DECIDED_MISSING_POW_BLOCKS.lock().unwrap();
    decided.remove(&hash);
    decided.insert(parent);
}

/// Why this node stopped taking part in BFT, and when it last said so.
///
/// Following Π_bft's decision would take a reorganization below the committed tip, which one
/// finalized database cannot do. The node stays on its own chain as proof of work with `fin`
/// frozen, and only a resync brings it back (FINALITY.md §4.3).
static DROPPED_BFT: LazyLock<Mutex<Option<(String, std::time::Instant)>>> =
    LazyLock::new(|| Mutex::new(None));

const DROPPED_BFT_REPORT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(600);

/// The running tenderlink task. Aborting it ends its network thread, which closes its socket.
static TENDERLINK_TASK: Mutex<Option<tokio::task::AbortHandle>> = Mutex::new(None);

/// Whether this node has stopped taking part in BFT (see `drop_bft`).
pub fn bft_is_dropped() -> bool {
    DROPPED_BFT.lock().unwrap().is_some()
}

/// Stop BFT on this node because following Π_bft needs `decided`, a block on the decided chain
/// that this node can no longer hold.
///
/// The record is written first, and the depth commit calls this before the commit that leaves
/// the block behind. A restart without it could not tell this state from a decided snapshot that
/// was only lost with the non-finalized state, and would resume BFT waiting for a block no peer
/// can give it.
pub(crate) fn drop_bft(reason: String, decided: (Height, Hash), db: &ZebraDb) {
    if bft_is_dropped() {
        return;
    }
    if let Err(err) = db.write_crosslink_bft_dropped(decided.0, decided.1) {
        // The drop goes ahead: the depth commit is never held back (FINALITY.md §4.3). A restart
        // without the record resumes BFT below the missing snapshot, fetches the decided chain
        // again, finds the same conflict, and drops BFT then.
        tracing::error!(
            "crosslink: could not record that BFT was dropped over block {} at height {}: {err}",
            decided.1, decided.0 .0,
        );
    }
    stop_bft(reason, db.path());
}

/// Stop BFT on this node, with `reason` in the standing report.
fn stop_bft(reason: String, db_path: &std::path::Path) {
    let mut dropped = DROPPED_BFT.lock().unwrap();
    if dropped.is_some() {
        return;
    }
    let message = format!(
        "crosslink: BFT is stopped on this node. {reason} Following that decision needs a \
         reorganization deeper than {} blocks. The node keeps running as proof of work, without \
         finality. Delete {} and resync to rejoin BFT.",
        crate::constants::MAX_BLOCK_REORG_HEIGHT, db_path.display(),
    );
    tracing::error!("{message}");
    *dropped = Some((message, std::time::Instant::now()));
    metrics::gauge!("state.crosslink.bft_dropped").set(1.0);
    MISSING_POW_BLOCKS.lock().unwrap().clear();
    DECIDED_MISSING_POW_BLOCKS.lock().unwrap().clear();
    if let Some(task) = TENDERLINK_TASK.lock().unwrap().take() {
        task.abort();
    }
}

fn note_decided_missing_pow_blocks(bft_state: &TMState) {
    if bft_is_dropped() {
        return;
    }
    for round in &bft_state.rounds_data {
        if round.height != bft_state.height {
            continue;
        }
        let (TMStatus::Indeterminate, TMStatusReason::NeedsBlock { hash }) = round.proposal_checked_validity else {
            continue;
        };
        if round.proposal_sigs_n == 0 || round.proposal_sigs_n != round.proposal_sigs.len() {
            continue;
        }
        // Tenderlink's own decision threshold over the active roster.
        let total_stake: u64 = round.roster.iter().map(|member| member.stake).sum();
        if total_stake == 0 {
            continue;
        }
        let threshold = tenderlink::decision_threshold(total_stake, round.strict_quorum);
        if round.counts.yes_precommits >= threshold && MISSING_POW_BLOCKS.lock().unwrap().contains_key(&Hash(hash)) {
            DECIDED_MISSING_POW_BLOCKS.lock().unwrap().insert(Hash(hash));
        }
    }
}

static RECENCY_STATUS: LazyLock<tokio::sync::watch::Sender<TFLRecencyStatus>> =
    LazyLock::new(|| tokio::sync::watch::channel(TFLRecencyStatus::default()).0);

/// Tenderlink's latest round-state snapshot.
pub fn bft_recency_status() -> TFLRecencyStatus {
    RECENCY_STATUS.borrow().clone()
}

// The sync thread parks a decision while tenderlink waits on its answer, so the engine cannot
// republish the status then: the sync thread publishes this part itself, and the engine copies it.
static PARKED_DECISION_HEIGHT: Mutex<Option<u64>> = Mutex::new(None);

/// How many times `restore` has run since this node started, for the recency status.
static BFT_RESTORES: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Tenderlink is held back while catching up, for the recency status.
static TENDERLINK_HELD: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// How many distinct conflicting quorums this node has reported, for the recency status.
static CONFLICTING_QUORUMS_SEEN: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn publish_tenderlink_held(held: bool) {
    TENDERLINK_HELD.store(held, std::sync::atomic::Ordering::Relaxed);
    RECENCY_STATUS.send_modify(|status| status.tenderlink_held = held);
}

fn publish_parked_decision_height(height: Option<u64>) {
    *PARKED_DECISION_HEIGHT.lock().unwrap() = height;
    RECENCY_STATUS.send_modify(|status| status.parked_decision_height = height);
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
    /// A decision tenderlink reached, to store. Nothing answers it: tenderlink waits for the
    /// published decided tip to pass its height (`DECIDED_TIP`).
    Decided {
        block: BftBlock,
        fat_pointer: FatPointerToBftBlock,
        proposal_sigs: Vec<TMSig>,
    },
    /// A decided block supplied from outside (the test format's `LoadPoS`): validated and stored
    /// exactly as a tenderlink decision, with the proposer's chunk signatures, or none for a round
    /// that has no proposer.
    ForceFeed {
        block: Arc<BftBlock>,
        fat_pointer: FatPointerToBftBlock,
        proposal_sigs: Vec<TMSig>,
        /// The proposal's valid round, which its chunk signatures cover; -1 for none. Checked,
        /// not stored: `restore` reads it back from the signatures (`proposal_valid_round`).
        valid_round: i64,
        /// Require the certificate to carry a quorum of the height's roster even before
        /// FeaturenetUpgrade2: set for decisions fetched from peers, where nothing else vouches
        /// for them. Imports keep the rule of the height they were decided at.
        require_certificate: bool,
        reply: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
}

fn send_request(request: BftRequest) -> bool {
    match super::SYNC_WAKE_SENDER.get() {
        Some(tx) => tx.send(super::SyncWake::Bft(request)).is_ok(),
        None => false,
    }
}

/// Store a decided block supplied from outside, as if tenderlink had decided it, with the
/// proposer's chunk signatures and the valid round they were made in: none for the genesis, which
/// is not proposed, and the round's proposer's, one per chunk, for every later height.
pub async fn force_feed_bft_block(
    block: Arc<BftBlock>,
    fat_pointer: FatPointerToBftBlock,
    proposal_sigs: Vec<TMSig>,
    valid_round: i64,
) -> Result<(), String> {
    let (reply, rx) = tokio::sync::oneshot::channel();
    let request = BftRequest::ForceFeed { block, fat_pointer, proposal_sigs, valid_round, require_certificate: false, reply };
    if !send_request(request) {
        return Err("new_network is not running".to_string());
    }
    rx.await.map_err(|_| "new_network dropped the force-feed reply".to_string())?
}

/// Whether `block` is valid on the current BFT chain: the check a proposal gets, answered at
/// once, with nothing stored or decided.
pub async fn validate_bft_block(block: BftBlock) -> Result<(TMStatus, TMStatusReason), String> {
    let (reply, rx) = tokio::sync::oneshot::channel();
    if !send_request(BftRequest::Validate { block, reply }) {
        return Err("new_network is not running".to_string());
    }
    rx.await.map_err(|_| "new_network dropped the validate reply".to_string())
}

/// The BFT block `bytes` encode, if they are that block's one canonical encoding.
///
/// Tenderlink certifies a hash of a proposal's raw bytes, but `store_decision` checks the hash of the
/// block re-serialized. A proposal that parses but is not canonical (trailing bytes, for one)
/// would be certified and then abort every node that reaches `store_decision`, so validation refuses it.
pub(super) fn bft_block_from_canonical_bytes(bytes: &[u8]) -> Option<BftBlock> {
    let block = BftBlock::zcash_deserialize(bytes).ok()?;
    let canonical = block.zcash_serialize_to_vec().ok()?;
    (canonical == bytes).then_some(block)
}

/// Everything BFT needs from outside the state: the node's finalizer identity, where it listens,
/// and who to connect to. The decided chain is persisted in the finalized database.
pub struct BftLaunch {
    pub signing_key: ed25519_zebra::SigningKey,
    /// `ip:port`. Once `network` is set, tenderlink speaks with the node's key on the node's
    /// port, and this matters only in that it must parse: tenderlink uses its endpoint's port
    /// alone.
    pub public_address: String,
    /// `bft_peers`. Unused once `network` is set, whose seeds are the node's initial peers.
    pub peer_addresses: Vec<String>,
    /// The node's own transport, key, port and initial peers, which tenderlink shares with the
    /// base protocol. Filled in by `new_network::sync`; `None` gives tenderlink a transport of
    /// its own, keyed by `public_address` as before.
    pub network: Option<BftNetwork>,
}

pub struct BftNetwork {
    pub transport: tenderlink::transport::SharedTransport,
    pub keypair: tenderlink::stp::IdentityKeyPair,
    pub port: u16,
    pub seeds: Vec<tenderlink::stp::STPAddress>,
}

pub fn bc_hdr_to_lrz(header: &Header) -> BcBlockHeader {
    let mut bytes = Vec::new();
    header.zcash_serialize(&mut bytes).unwrap();
    BcBlockHeaderWrap::read_data(&*bytes).unwrap()
}

/// The context-free proof-of-work checks for a carried bc-header: a well-formed difficulty
/// threshold within the network's PoWLimit, a proof-of-work hash under that threshold, and a valid
/// Equihash solution. These are the same checks Zebra's block verifier applies to a header before
/// it has the block's ancestry, including its exemption: on a network with PoW disabled it checks
/// only the threshold, so a header here must not have to meet more than its block did. The hash
/// the filter compares depends on the header's height, unknown until the snapshot is, so this
/// lets through a header either hash meets, and [`header_meets_filter_at`] settles it once the
/// height is known.
fn header_pow_is_valid(
    header: &BcBlockHeader,
    network: &zebra_chain::parameters::Network,
) -> Result<(), String> {
    use zebra_chain::work::difficulty::ParameterDifficulty as _;
    let header = header_from_lrz(header)?;
    let hash = header.hash();
    let threshold = header
        .difficulty_threshold
        .to_expanded()
        .ok_or_else(|| format!("invalid difficulty threshold {:?}", header.difficulty_threshold))?;
    if threshold > network.target_difficulty_limit() {
        return Err(format!("difficulty threshold {threshold:?} is above the network limit"));
    }
    if network.disable_pow() {
        return Ok(());
    }
    if hash > threshold && header.hash_without_fat_pointer() > threshold {
        return Err(format!("hash {hash} does not meet difficulty threshold {threshold:?}"));
    }
    header.solution.check(&header).map_err(|e| e.to_string())
}

/// The difficulty filter for a carried bc-header at `height`, its position on its chain, which
/// picks the hash the filter compares (`Header::proof_of_work_hash`). The header's other proof-of-
/// work checks are [`header_pow_is_valid`]'s.
fn header_meets_filter_at(
    header: &BcBlockHeader,
    network: &zebra_chain::parameters::Network,
    height: Height,
) -> Result<(), String> {
    if network.disable_pow() {
        return Ok(());
    }
    let header = header_from_lrz(header)?;
    let threshold = header
        .difficulty_threshold
        .to_expanded()
        .ok_or_else(|| format!("invalid difficulty threshold {:?}", header.difficulty_threshold))?;
    let hash = header.proof_of_work_hash(network, height);
    if hash > threshold {
        return Err(format!("proof-of-work hash {hash} at {height:?} does not meet difficulty threshold {threshold:?}"));
    }
    Ok(())
}

fn header_from_lrz(header: &BcBlockHeader) -> Result<Header, String> {
    let mut bytes = Vec::new();
    BcBlockHeaderWrap::write_data(header, &mut bytes).map_err(|e| e.to_string())?;
    Header::zcash_deserialize(&*bytes).map_err(|e| e.to_string())
}

/// The `(nBits, time)` pairs the difficulty adjustment reads for the block above `snapshot`: the
/// snapshot and its ancestors, nearest first, up to `POW_ADJUSTMENT_BLOCK_SPAN` of them or back to
/// genesis. The walk follows each header's own parent hash, so it reads the snapshot's chain
/// wherever that chain forks from the best one.
fn difficulty_context_above(
    snapshot_hash: Hash,
    snapshot_height: Height,
    mut header_of: impl FnMut(Hash) -> Option<Arc<Header>>,
) -> VecDeque<(CompactDifficulty, DateTime<Utc>)> {
    let mut context = VecDeque::with_capacity(POW_ADJUSTMENT_BLOCK_SPAN + 1);
    let mut cursor = snapshot_hash;
    let mut height = snapshot_height;
    loop {
        let header = header_of(cursor).expect(
            "a held block's ancestors are held: each non-finalized chain carries its history above \
             the finalized tip, and the finalized state holds the rest",
        );
        context.push_back((header.difficulty_threshold, header.time));
        if context.len() == POW_ADJUSTMENT_BLOCK_SPAN || height == Height(0) {
            return context;
        }
        cursor = header.previous_block_hash;
        height = Height(height.0 - 1);
    }
}

/// Whether each carried header could head a block at its position: its `nBits` equals the
/// `ThresholdBits` the chain requires there, and its time is within the median-time-past bounds.
/// `context` is [`difficulty_context_above`] the snapshot, and each accepted header becomes the
/// nearest entry of the next one's context. Without this a header only has to meet the `nBits` it
/// declares for itself, which can be as easy as PoWLimit.
fn headers_meet_required_difficulty(
    network: &Network,
    snapshot_height: Height,
    mut context: VecDeque<(CompactDifficulty, DateTime<Utc>)>,
    headers: &[BcBlockHeader],
) -> Result<(), String> {
    let mut previous_height = snapshot_height;
    for (i, header) in headers.iter().enumerate() {
        let header = header_from_lrz(header)?;
        let Some(height) = previous_height + 1 else {
            return Err(format!("carried header {i} would be above the maximum block height"));
        };
        let adjustment =
            AdjustedDifficulty::new_from_header_time(header.time, previous_height, network, context.iter().copied());
        if let Err(error) = check::difficulty_threshold_and_time_are_valid(header.difficulty_threshold, adjustment) {
            return Err(format!("carried header {i} at height {}: {error}", height.0));
        }
        context.push_front((header.difficulty_threshold, header.time));
        context.truncate(POW_ADJUSTMENT_BLOCK_SPAN);
        previous_height = height;
    }
    Ok(())
}

/// [`headers_meet_required_difficulty`] for headers carried above a block this state holds.
fn carried_headers_meet_required_difficulty(
    read_state: &ReadState,
    snapshot_hash: Hash,
    snapshot_height: Height,
    headers: &[BcBlockHeader],
) -> Result<(), String> {
    let context = difficulty_context_above(snapshot_hash, snapshot_height, |hash| {
        read_state.any_chain_block_header(crate::HashOrHeight::Hash(hash))
    });
    headers_meet_required_difficulty(read_state.network(), snapshot_height, context, headers)
}

/// The set of finalizers terminated (blacklisted) by user-led slashforks at a given point, as a
/// pure function of `(schedule, bft_height, finalized_bc_height)`.
///
/// A finalizer is terminated at BFT height `bft_height` when some scheduled slashfork:
/// - has `bft_certificate_height <= bft_height` -- in effect at this height, **inclusive**, so the
///   finalizers a slashfork terminates are already excluded from the roster that votes on the very
///   block carrying that slashfork (this matches the vote-namespacing inclusiveness); and
/// - has `pow_activation_height > finalized_bc_height` -- the activation has not yet been
///   finalized. Once it is, the bonds are terminated at the source (so the finalizer is gone from
///   the roster anyway) and the key is free to be restaked to, so it must no longer be suppressed
///   here.
pub fn terminated_finalizers_at(
    slashforks: &[SlashForkConfig],
    bft_height: u64,
    finalized_bc_height: u64,
) -> HashSet<PubKeyID> {
    let mut set = HashSet::new();
    for hf in slashforks.iter().filter(|hf| {
        hf.bft_certificate_height <= bft_height && hf.pow_activation_height > finalized_bc_height
    }) {
        for &finalizer in &hf.terminated_finalizers {
            set.insert(finalizer);
        }
    }
    set
}

/// Vote-namespacing domain separator for a BFT height: a flat blake3 hash of the scheduled
/// slashforks whose `bft_certificate_height <= bft_height` (inclusive of a slashfork at
/// `bft_height` itself), concatenated in canonical schedule order. An empty set yields
/// `[0; 32]` (nil), so the no-slashfork case is a backwards-compatible no-op in tenderlink's
/// signing. The real `bft_certificate_height`s in the schedule are non-decreasing (several rules
/// may share one certificate height), but PoW-only rules (`SlashForkConfig::NEVER`) can come
/// between them. The filter therefore selects a subsequence, not a prefix, and must not stop at
/// the first rule that fails it.
///
/// At a height that decides by the strict quorum (`strict_quorum_at`) the FeaturenetUpgrade2
/// branch ID follows the slashforks. A finalizer still deciding on `2f+1` and one deciding on the
/// strict quorum then sign under different namespaces and neither counts the other's votes, so
/// the switch is a clean fork rather than a network whose halves decide on different quorums.
pub fn namespace_for_bft_height(slashforks: &[SlashForkConfig], bft_height: u64, strict_quorum: bool) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    let mut any = false;
    for hf in slashforks.iter().filter(|hf| hf.bft_certificate_height <= bft_height) {
        let mut bytes = Vec::new();
        hf.zcash_serialize(&mut bytes).expect("serializing to a Vec is infallible");
        hasher.update(&bytes);
        any = true;
    }
    if strict_quorum {
        hasher.update(&u32::from(BranchId::FeaturenetUpgrade2).to_le_bytes());
        any = true;
    }
    if any { hasher.finalize().into() } else { [0u8; 32] }
}

/// Whether the BFT height whose roster is the stake at the PoW block at `roster_snapshot_height`
/// decides by the strict quorum and signs under FeaturenetUpgrade2's namespace. The roster for
/// height `h` is the stake at the snapshot of block `h-1`, so every node answers this from
/// decided chain data. Genesis has no roster snapshot: it is decided by construction.
pub fn strict_quorum_at(network: &Network, roster_snapshot_height: Option<Height>) -> bool {
    match roster_snapshot_height {
        Some(height) => network.is_nu_active(NetworkUpgrade::FeaturenetUpgrade2, height.0.into()),
        None => false,
    }
}

/// What votes at `bft_height`: tenderlink's roster built from `stakes`, the stake at the snapshot
/// of the block below, which sits at `roster_snapshot_height` (`None` for genesis); and the
/// voting roster, with the height's namespace and quorum rule, that its certificates are checked
/// against. The snapshot height is also the finalized watermark the slashfork terminations use.
pub fn roster_for_bft_height(
    network: &Network,
    slashforks: &[SlashForkConfig],
    stakes: &[RosterMember],
    bft_height: u64,
    roster_snapshot_height: Option<Height>,
) -> (Vec<SortedRosterMember>, VotingRoster) {
    // u32 to u64 is lossless.
    let watermark = roster_snapshot_height.map_or(0, |height| u64::from(height.0));
    let terminated = terminated_finalizers_at(slashforks, bft_height, watermark);
    let roster = tenderlink_roster_from_internal(stakes, &terminated);
    let strict_quorum = strict_quorum_at(network, roster_snapshot_height);
    let voting = VotingRoster {
        roster: roster[..roster.len().min(network.crosslink_parameters().active_roster_max_n)].to_vec(),
        vote_namespace: namespace_for_bft_height(slashforks, bft_height, strict_quorum),
        strict_quorum,
    };
    (roster, voting)
}

/// Whether `fat_pointer` is a certificate for the block it names at `bft_height`, decided by
/// `voting`: a precommit template at that height, signed by distinct members of the active roster
/// in roster order whose stake reaches the height's quorum, each signature valid under the
/// height's namespace. The nil roster decides only genesis, by construction, with no signatures
/// at round 0.
///
/// The round is not compared with the round this node decided in: one value can gather a quorum
/// of precommits in more than one round, and honest nodes keep the certificate they assembled, so
/// requiring this node's round would refuse another honest node's certificate. The signatures
/// bind whatever round the template names.
///
/// Signer order is the order `round_data_to_fat_pointer` writes, so one pass with strictly
/// increasing roster positions refuses duplicates, strangers and disorder together, and stops
/// within the roster's length whatever the list holds.
pub fn certificate_is_valid(fat_pointer: &FatPointerToBftBlock, bft_height: u64, voting: &VotingRoster) -> bool {
    let vote = fat_pointer.get_vote_template();
    if vote.height != bft_height || !vote.typ {
        return false;
    }
    let active = &voting.roster[..];
    let total_stake: u64 = active.iter().map(|member| member.stake).sum();
    if total_stake == 0 {
        return fat_pointer.signatures.is_empty() && vote.round == 0;
    }
    let mut signed_stake: u64 = 0;
    let mut next = 0;
    for signature in &fat_pointer.signatures {
        let Some(offset) = active[next..].iter().position(|member| member.pub_key == signature.pub_key) else {
            return false;
        };
        signed_stake = signed_stake.saturating_add(active[next + offset].stake);
        next += offset + 1;
    }
    if signed_stake < tenderlink::decision_threshold(total_stake, voting.strict_quorum) {
        return false;
    }
    fat_pointer.validate_signatures(&voting.vote_namespace)
}

/// Build the tenderlink consensus roster from the stored roster, excluding any finalizer in
/// `terminated` (terminated by a user-led slashfork; see [`terminated_finalizers_at`]). Pass an
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
pub fn fat_pointer_to_block_at_height(chain: &BftChain, at_height: u64) -> Option<FatPointerToBftBlock> {
    let decided_n = chain.blocks.len();
    if at_height == 0 || at_height as usize - 1 >= decided_n {
        return None;
    }
    if at_height as usize == decided_n {
        Some(chain.fat_pointer_to_tip())
    } else {
        decided_block(chain, at_height as usize).map(|next| next.previous_block_fat_ptr.clone())
    }
}

/// The valid round a decided height's proposal signatures were made in. The database keeps the
/// signatures but not that round, and tenderlink re-serves each chunk signed over it, so restoring
/// it as -1 re-serves a re-proposed value with signatures that do not verify. With the height's
/// voting roster the proposer and namespace are known exactly, and a valid round lies below the
/// decided round: round 0 and a round with no proposer need no search.
///
/// `None` when no valid round makes every signature verify.
fn proposal_valid_round(
    block: &BftBlock,
    fat_pointer: &FatPointerToBftBlock,
    proposal_sigs: &[TMSig],
    voting: &VotingRoster,
    active_roster_max_n: usize,
) -> Option<i64> {
    let round = fat_pointer.get_vote_template().round;
    if proposal_sigs.is_empty() || round <= 0 {
        return Some(-1);
    }
    // A positive i32 round fits u32.
    let round_u32 = round as u32;
    let height = u64::from(block.height);
    // An empty roster (the genesis height) has no proposer; tenderlink would log asking it for one.
    let proposer_i = if voting.roster.is_empty() { None } else { tenderlink::proposer_index(&HashKeys::default(), &voting.roster, active_roster_max_n, height, round_u32) };
    let Some(proposer_i) = proposer_i else {
        return Some(-1);
    };
    let proposer = voting.roster[proposer_i].pub_key;
    let value = block.zcash_serialize_to_vec().ok()?;
    let value_id = tenderlink::ValueId(fat_pointer.points_at_block_hash().0);
    (-1..i64::from(round)).find(|&valid_round| {
        tenderlink::first_bad_proposal_chunk_sig(proposer, &value, value_id, height, round_u32, valid_round, &voting.vote_namespace, proposal_sigs).is_none()
    })
}

/// A fetched decision's proposal signatures, kept if they verify as the round proposer's over
/// a fresh proposal (valid round -1, which every round-0 decision is), else none. One check,
/// never a search over valid rounds: the database does not store the valid round, and a peer
/// chooses the round. Without them the decision stands, but tenderlink cannot re-serve that
/// height's proposal chunks.
///
/// TODO: store the valid round with each decision and send it in BFT_DECISIONS, so the
/// signatures of re-proposed values are kept too. That is a database format change, deferred
/// while this branch already changes the network format (network merge plan, open items).
fn fresh_proposal_sigs(block: &BftBlock, fat_pointer: &FatPointerToBftBlock, sigs: Vec<TMSig>, voting: &VotingRoster, active_roster_max_n: usize) -> Vec<TMSig> {
    let Ok(round) = u32::try_from(fat_pointer.get_vote_template().round) else { return Vec::new(); };
    if sigs.is_empty() || voting.roster.is_empty() {
        return Vec::new();
    }
    let height = u64::from(block.height);
    let Some(proposer_i) = tenderlink::proposer_index(&HashKeys::default(), &voting.roster, active_roster_max_n, height, round) else {
        return Vec::new();
    };
    let value = block.zcash_serialize_to_vec().expect("a BFT block serializes");
    if sigs.len() != tenderlink::proposal_chunks_n(value.len()) {
        return Vec::new();
    }
    let value_id = tenderlink::ValueId(fat_pointer.points_at_block_hash().0);
    match tenderlink::first_bad_proposal_chunk_sig(voting.roster[proposer_i].pub_key, &value, value_id, height, round, -1, &voting.vote_namespace, &sigs) {
        None => sigs,
        Some(_) => Vec::new(),
    }
}

/// The round data tenderlink keeps for an already-decided height, as the live path would have
/// left it: precommit signatures placed by roster index, counts from the fat pointer.
fn decided_round_data(
    block: &BftBlock,
    fat_pointer: &FatPointerToBftBlock,
    roster: Vec<SortedRosterMember>,
    voting: &VotingRoster,
    proposal_sigs: Vec<TMSig>,
    bft_height: u64,
    active_roster_max_n: usize,
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
    round_data.proposal_valid_round = proposal_valid_round(block, fat_pointer, &proposal_sigs, voting, active_roster_max_n).unwrap_or(-1);
    round_data.proposal_sigs_n = proposal_sigs.len();
    round_data.proposal_sigs = proposal_sigs;
    round_data.proposal = BlockValue(block.zcash_serialize_to_vec().unwrap());
    round_data.proposal_id = ValueId(fat_pointer.points_at_block_hash().0);
    round_data.height = bft_height;
    round_data.round = fat_pointer.get_vote_template().round as u32;
    round_data.vote_namespace = voting.vote_namespace;
    round_data.strict_quorum = voting.strict_quorum;
    round_data.active_roster_max_n = active_roster_max_n;
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
    // The block is read back when it is older than the ones held, as for a PoW block far below
    // the BFT tip. One that resolved and cannot be read defers, like one not here yet.
    let child_block = match child_index {
        Some(h) => match decided_block(chain, h) {
            Some(block) => Some(block),
            None => return None,
        },
        None => None,
    };
    if let Some(child_block) = &child_block {
        let do_not_include = child_block.do_not_include_until_bc_height;
        if (pow_block_height.0 as u64) < do_not_include {
            return Some(CrosslinkVerdict::Reject);
        }
    }

    // The PoW block this block's certificate finalizes -- the snapshot, i.e. the parent of the
    // BFT block's deepest carried header. Placeholder entries from out-of-order BFT ingest carry
    // no headers and are never valid pointer targets, but guard rather than panic.
    let snapshot_hash = match &child_block {
        Some(child_block) if !child_block.headers.is_empty() => {
            Some(Hash(child_block.snapshot_block_hash().0))
        }
        Some(_) => return None,
        None => None,
    };
    drop(child_block);

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

    // Variable reward disabled: stakers and finalizers are paid on every block, so the verdict
    // no longer decides it. The field stays so the gate can be re-enabled by uncommenting below.
    let pos_payout = true;
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

        // VARIABLE REWARD DISABLED: every block pays its staking share (see `Chain::push`).
        // // PoS issuance rides on this check because this is the one place that knows both facts it
        // // needs. A block pays only when it ADVANCES finality (its certificate is a different BFT
        // // block than its parent's -- compared by the cert's identity, the BFT block hash, since
        // // two honest nodes can carry different signature sets for the same decision) and does so
        // // PROMPTLY (`gap <= sigma + FINALITY_LIVENESS_ALLOWANCE`; the check above already put
        // // `gap >= sigma + 1`, so the payable window is exactly those few heights).
        // //
        // // Both facts are objective functions of committed chain data, so every node reaches the
        // // same answer for the same block. See `FINALITY_LIVENESS_ALLOWANCE`.
        // let cert_advanced =
        // child_fat_pointer.points_at_block_hash() != parent_fat_pointer.points_at_block_hash();
        // pos_payout = cert_advanced && gap <= sigma + FINALITY_LIVENESS_ALLOWANCE;
        //
        // // One line per block recording the decision and the facts behind it. `debug!` would be
        // // the natural level, but the release binary is built with `release_max_level_info`, so
        // // anything below `info` is compiled out and would never be seen on a real node.
        // if cert_advanced && !pos_payout {
        // // The notable case: finality DID advance here, but so slowly that the block earns
        // // nothing. It is otherwise indistinguishable from a block that simply carried the
        // // same certificate as its parent, so it is spelled out.
        // tracing::info!(
        // "no PoS issuance at height {}: the certificate finalizes height {} \
        // (gap {}), beyond sigma {} + FINALITY_LIVENESS_ALLOWANCE {}",
        // pow_block_height.0, snapshot_height.0, gap, sigma, FINALITY_LIVENESS_ALLOWANCE,
        // );
        // } else {
        // tracing::info!(
        // "PoS payout decision at height {}: payout={} cert_advanced={} gap={} snapshot={} sigma={}",
        // pow_block_height.0, pos_payout, cert_advanced, gap, snapshot_height.0, sigma,
        // );
        // }
    }

    // From FeaturenetUpgrade2 the pointer is a certificate and not only a hash: the votes that
    // decided the block, checked against the roster, namespace and quorum of its height.
    // PERMANENT: those are fixed by the decided chain, and the pointer by the block. Checked
    // after every deferral above, so a deferred block is not verified again on each retry.
    if let Some(h) = child_index {
        if read_state.network().is_nu_active(NetworkUpgrade::FeaturenetUpgrade2, pow_block_height.0.into()) {
            let voting = chain.voting_rosters.get(h).expect("voting_rosters is indexed like blocks, and h resolved");
            // usize to u64 is lossless on every supported target.
            if !certificate_is_valid(child_fat_pointer, h as u64, voting) {
                return Some(CrosslinkVerdict::Reject);
            }
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
    // Admission checks certificates from FeaturenetUpgrade2 on. A pointer handed out for a block
    // that is not the tip is the next block's previous-block pointer, which was checked only if
    // that block was proposed after the upgrade; one that fails would be refused for good.
    let certificates_checked = u32::try_from(proposed_pow_height)
        .is_ok_and(|height| read_state.network().is_nu_active(NetworkUpgrade::FeaturenetUpgrade2, height.into()));
    let mut suitable_height = None;
    // Only the held blocks are tried. When none of them suits, the template is for a chain far
    // from the BFT tip, and it keeps its parent's pointer, as below.
    for (i, b) in chain.blocks.held().rev() {
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
            if certificates_checked {
                let pointer = fat_pointer_to_block_at_height(chain, i as u64 + 1);
                let voting = chain.voting_rosters.get(i).expect("voting_rosters is indexed like blocks");
                // usize to u64 is lossless on every supported target.
                if !pointer.is_some_and(|pointer| certificate_is_valid(&pointer, i as u64, voting)) {
                    continue;
                }
            }
            suitable_height = Some(i as u64 + 1);
            break;
        }
    }
    if let Some(h) = suitable_height {
        return fat_pointer_to_block_at_height(chain, h).unwrap_or_else(FatPointerToBftBlock::null);
    }
    parent_hash
        .and_then(|parent_hash| read_state.any_chain_block_header(parent_hash.into()))
        .map(|hdr| hdr.fat_pointer_to_bft_block.clone())
        .unwrap_or_else(FatPointerToBftBlock::null)
}

/// How an imported decision is answered: `Ok` once stored, or why it was not. Tenderlink's own
/// decisions are not answered; it follows the published decided tip.
type ImportReply = tokio::sync::oneshot::Sender<Result<(), String>>;

/// What [`BftRunner::check_decided_height`] made of a decision for a height this node decided.
pub(super) enum DecidedCheck {
    /// The height is not decided here, or it is another genesis, which is derived and carries no
    /// quorum to check.
    NotChecked,
    /// The block this node decided there.
    Same,
    /// Another block with a valid certificate for that height: reported as a conflicting quorum.
    Conflict,
    /// Another block whose fat pointer names another block or is not a valid certificate.
    Invalid { why: String },
}

/// What [`BftRunner::feed_fetched`] did with a decision.
pub(super) enum FeedOutcome {
    Fed,
    /// Not the next height, its snapshot is not committed yet, a decision is parked, or
    /// validation cannot settle it yet: handed back to keep.
    NotReady(super::bft_sync::WireDecision),
    /// Not stored. `peer_fault`: the decision is wrong whatever this node's state (its
    /// certificate names another block, or it is labelled with the wrong height), which no honest
    /// peer sends. Otherwise the refusal may be this node's own state disagreeing.
    Refused { why: String, peer_fault: bool },
}

/// Where a decision comes from. It sets which checks `BftRunner::store_decision` gives it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    /// Fetched from a peer: nothing vouches for it but its certificate.
    Fetched,
    /// Supplied from outside (the test format's `LoadPoS`, an import file).
    Imported {
        /// The proposal's valid round, which its chunk signatures cover; -1 for none.
        valid_round: i64,
        /// Require a quorum certificate even before FeaturenetUpgrade2.
        require_certificate: bool,
    },
    /// Decided by this node's tenderlink.
    Tenderlink,
    /// Stored by this node before it last stopped, at or above the height `restore` stopped at
    /// because this state no longer held that decision's snapshot. Taken in once it does.
    Stored,
}

/// What `BftRunner::store_decision` does with a decision, as bits. The low bits are the checks,
/// numbered in the order it runs them; a source starts from all of them and opts out of those an
/// assumption makes unnecessary, naming the assumption where it does. The high bits say how it
/// answers some outcomes.
type Admission = u32;

/// The fat pointer names the block it comes with.
const CHECK_NAMES_BLOCK: Admission = 1 << 0;
/// At a height already decided: the same block is decided already; another is checked as a
/// conflicting quorum (`check_decided_height`).
const CHECK_DECIDED_HEIGHT: Admission = 1 << 1;
/// A decision waiting for its snapshot (parked) holds the next height.
const CHECK_PARKED: Admission = 1 << 2;
/// Genesis is derived from the chain, never fetched, unless BFT is supplied.
const CHECK_GENESIS_NOT_FETCHED: Admission = 1 << 3;
/// It is for the next height.
const CHECK_NEXT_HEIGHT: Admission = 1 << 4;
/// The next height's roster has stake.
const CHECK_ROSTER_STAKE: Admission = 1 << 5;
/// Its certificate is valid before it may wait for its snapshot, checked once per decision.
const CHECK_CERTIFICATE_EARLY: Admission = 1 << 6;
/// Its snapshot is held: nothing else about it settles before.
const CHECK_SNAPSHOT_HELD: Admission = 1 << 7;
/// It links to the decided tip.
const CHECK_LINKS_TO_TIP: Admission = 1 << 8;
/// It carries headers.
const CHECK_HEADERS: Admission = 1 << 9;
/// Its fat pointer has no more signatures than the active roster has seats.
const CHECK_FITS_ROSTER: Admission = 1 << 10;
/// It passes the block rules against this node's chains (`validate_with_hash`).
const CHECK_BLOCK_RULES: Admission = 1 << 11;
/// Every signature its fat pointer carries verifies.
const CHECK_SIGNATURES: Admission = 1 << 12;
/// Its fat pointer is a valid certificate for the height: a quorum of the roster, every
/// signature verifying. Covers `CHECK_SIGNATURES`.
const CHECK_QUORUM: Admission = 1 << 13;
/// Its proposal signatures fit the roster that proposed it: the round's proposer signed each chunk.
const CHECK_PROPOSAL_ROSTER: Admission = 1 << 14;
const CHECK_ALL: Admission = (1 << 15) - 1;

/// A fact that fails is a bug in this node, so it panics rather than refuses.
const FAULTS_ARE_BUGS: Admission = 1 << 16;
/// Block rules that cannot be settled yet park the decision rather than refuse it for now.
const PARK_UNSETTLED: Admission = 1 << 17;
/// Block rules that refuse it are reported: this node's own decision no longer passes them.
const REPORT_REFUSED_RULES: Admission = 1 << 18;
/// A snapshot it lacks is marked to be fetched: it is on the decided chain.
const MARK_MISSING_SNAPSHOT: Admission = 1 << 19;
/// Another block at a height already decided is refused, not taken as decided already.
const CONFLICT_REFUSES: Admission = 1 << 20;
/// Its proposal signatures are kept only if they verify as a fresh proposal's.
const KEEP_ONLY_FRESH_PROPOSAL_SIGS: Admission = 1 << 21;

/// Assumption: tenderlink decides a block only on a quorum of precommits it verified, at the
/// height it is at, with the roster it was given for it, so the checks a peer's decision gets for
/// its height, stake, certificate and proposal are not run. It does run the block rules again,
/// because the chain can move between its validation and its decision.
const ASSUME_TENDERLINK_DECIDES_ON_ITS_OWN_QUORUM: () = ();

/// What `BftRunner::store_decision` did with a decision it did not refuse.
enum Stored {
    Decided,
    AlreadyDecided,
    Parked,
}

/// Why a decision was not stored.
struct Refusal {
    why: String,
    kind: RefusalKind,
}

enum RefusalKind {
    /// The decision is wrong whatever this node's state (its certificate names another block):
    /// no honest peer sends it.
    Fault,
    /// Refused here, but the reason depends on this node's own state (its decided chain, roster,
    /// slashfork schedule), so the sender may be honest and this node the odd one out.
    Refused,
    /// Cannot be settled yet (BFT dropped, a decision parked, validation indeterminate).
    NotYet,
}

impl Refusal {
    fn fault(why: String) -> Self {
        Self { why, kind: RefusalKind::Fault }
    }
    fn refused(why: String) -> Self {
        Self { why, kind: RefusalKind::Refused }
    }
    fn not_yet(why: String) -> Self {
        Self { why, kind: RefusalKind::NotYet }
    }
}

/// Set once the zero-stake roster failure has been reported (`BftRunner::feed_fetched`).
static ZERO_STAKE_REPORTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// How many decided BFT heights this node holds. `validate` admits only the next height, so the
/// decided chain has no gaps and its length is the next height to decide.
pub(super) fn decided_len() -> u64 {
    let chain = BFT_CHAIN.read().unwrap();
    debug_assert_eq!(chain.voting_rosters.len(), chain.blocks.len(), "voting_rosters is indexed like blocks");
    chain.blocks.len() as u64
}

/// The BFT side of the sync thread: answers tenderlink's requests against the writer's chains,
/// bootstraps BFT from the bc-chain, and persists decisions.
pub(super) struct BftRunner {
    rt: tokio::runtime::Handle,
    network: Network,
    params: ZcashCrosslinkParameters,
    slashforks: Arc<zebra_chain::parameters::SlashForkSchedule>,
    /// Held until BFT starts (a loaded chain, or the bootstrap genesis); `None` once tenderlink
    /// is spawned, or when this node runs no BFT at all.
    launch: Option<BftLaunch>,
    /// A bootstrap genesis that has been decided but whose commit has not finished yet, so
    /// tenderlink cannot be started from it. Waiting on the reply here would freeze the sync
    /// thread, which is the only thread that can retry the commit.
    pending_bootstrap: Option<PendingBootstrap>,
    /// A decision whose re-validation came back indeterminate: its snapshot was here when
    /// tenderlink validated the proposal and has been dropped from the non-finalized state since.
    /// The decision is final either way, so it is retried at the start of every tick until the
    /// snapshot is back, and answered only then.
    pub(super) parked_decision: Option<ParkedDecision>,
    /// Set while `tick` retries the parked decision, so only the first park is a warning.
    next_diagnostic: std::time::Instant,
    /// Set by the sync loop while peers hold many more decided heights than this node. It fetches
    /// them as data then, and tenderlink would only crawl after them one height per exchange.
    pub(super) catching_up: bool,
    /// Tenderlink was due to start while catching up. It starts from the stored chain once the
    /// node has caught up.
    pub(super) spawn_deferred: bool,
    /// This node held tenderlink WAL records when it started: what it signed at every height it
    /// may pass while catching up is in them, so tenderlink starts at once, never held back.
    /// Without them (a new node, one that never signed, or one whose state was wiped while its
    /// key was kept) it is held back while catching up.
    has_wal: bool,
    /// The height `restore` stopped at, below stored rows it could not take in because this
    /// state lacked a snapshot: `feed_stored` takes them in, one by one, as they become the next
    /// height with their snapshot held. `None` once no stored row is left above the chain.
    stored_from: Option<u64>,
    /// The height and certificate of the last fetched decision `fetch_window` found valid. A
    /// decision waiting for its snapshot is fed again on every PoW commit, and its signatures need
    /// checking only once there; the quorum check runs again before it is stored.
    screened_certificate: Option<(u64, FatPointerToBftBlock)>,
}

/// The BFT height after the last of `rounds`: where tenderlink starts when it is given them.
fn height_after(rounds: &[RoundData]) -> u64 {
    rounds.last().map_or(0, |round| round.height + 1)
}

/// The round that decided `height`, as tenderlink holds it: the stored decision, voted on by
/// `voting`. Its roster is the active one, which is all of a roster tenderlink reads. `None` when
/// the height is not stored.
fn stored_round(db: &ZebraDb, voting: &VotingRoster, height: u64, active_roster_max_n: usize) -> Option<RoundData> {
    let StoredDecision { block, fat_pointer, proposal_sigs } =
        db.bft_decision(u32::try_from(height).ok()?)?;
    Some(decided_round_data(&block, &fat_pointer, voting.roster.clone(), voting, proposal_sigs, height, active_roster_max_n))
}

/// The round that decided `height`, read back for tenderlink when it no longer holds it: a peer
/// that far behind is still sent the rounds it lacks.
fn fetch_decided_round(height: u64, active_roster_max_n: usize) -> Option<RoundData> {
    let voting = Arc::clone(BFT_CHAIN.read().unwrap().voting_rosters.get(usize::try_from(height).ok()?)?);
    let db = BFT_DB.read().unwrap();
    stored_round(db.as_ref()?, &voting, height, active_roster_max_n)
}

/// The newest decided tip whose decision is stored: what tenderlink follows, read on its own task
/// (`ClosureToReadDecidedTip`). A live decision publishes it after its row is written and never
/// lowers it; `restore` replaces it with the chain it rebuilt, while tenderlink is not running.
static DECIDED_TIP: LazyLock<tokio::sync::watch::Sender<Option<Arc<DecidedTip>>>> =
    LazyLock::new(|| tokio::sync::watch::channel(None).0);

/// Publishes `tip` once its decision's row is written, unless a higher tip is published already.
fn publish_tip(tip: &Arc<DecidedTip>) {
    let () = ASSUME_TIP_PUBLISHED_AFTER_ROW;
    DECIDED_TIP.send_if_modified(|published| {
        if published.as_ref().is_some_and(|published| published.height >= tip.height) {
            return false;
        }
        *published = Some(Arc::clone(tip));
        true
    });
}

/// `tip` as tenderlink takes it.
fn host_tip_of(tip: &DecidedTip) -> tenderlink::HostTip {
    tenderlink::HostTip {
        height: tip.height,
        block_id: tenderlink::ValueId(tip.block_hash.0),
        next_roster: tip.next_roster.clone(),
        next_vote_namespace: tip.next_voting.vote_namespace,
        next_strict_quorum: tip.next_voting.strict_quorum,
    }
}

/// The published decided tip as tenderlink takes it, if it has reached `engine_height`.
fn host_tip(engine_height: u64) -> Option<tenderlink::HostTip> {
    DECIDED_TIP.borrow().as_deref().filter(|tip| tip.height >= engine_height).map(host_tip_of)
}

/// Hands the sync thread a decision as this node's tenderlink hands over one it reached (the test
/// format's `BFT_PUSH_CANDIDATE`). Nothing answers it; what became of it shows in the decided chain
/// and the published tip. False when there is no sync thread to take it.
pub fn push_candidate(block: BftBlock, fat_pointer: FatPointerToBftBlock, proposal_sigs: Vec<TMSig>) -> bool {
    send_request(BftRequest::Decided { block, fat_pointer, proposal_sigs })
}

/// Assumption, kept by `store_decision` and `restore`: a published tip's decision is stored, so
/// tenderlink can read the tip's round back (`fetch_decided_round`) once it takes the tip.
const ASSUME_TIP_PUBLISHED_AFTER_ROW: () = ();

/// Assumption: only the sync thread writes `BFT_CHAIN` and the chain state, so what `store_decision` reads
/// from them before taking the chain lock (the snapshot, the stakes) still holds under it.
const ASSUME_SYNC_THREAD_SOLE_WRITER: () = ();

/// Assumption, kept by `apply_decision` and `restore`: the roster, namespace and quorum rule of the next
/// height to decide are the tip's, set once per decision; nothing derives them again.
const ASSUME_NEXT_ROSTER_IS_THE_TIPS: () = ();

/// Adds an admitted decision to `chain` in one step: the block and `voted`, the roster that voted
/// on it; the stakes at its snapshot, or the previous roster carried forward when they are empty
/// (the ghost roster); and a new tip holding the next height's roster, derived here and nowhere
/// else. Checks nothing cryptographic: the caller admitted the decision.
#[allow(clippy::too_many_arguments)]
fn apply_decision(
    chain: &mut BftChain,
    network: &Network,
    slashforks: &[SlashForkConfig],
    block: BftBlock,
    block_hash: Blake3Hash,
    fat_pointer: FatPointerToBftBlock,
    voted: Arc<VotingRoster>,
    stakes: Vec<RosterMember>,
    final_snapshot: Option<(Height, Hash)>,
) -> Arc<DecidedTip> {
    let () = ASSUME_ADMITTED_BEFORE_APPLY;
    // Unknown only where `restore` replays a chain whose snapshot this state no longer holds (BFT
    // dropped): the last one known stands, for the tip and for the next roster's watermark.
    let final_snapshot = final_snapshot.or_else(|| chain.bft_final_snapshot());
    // `height` is the 0-based canonical height, i.e. the chain index directly.
    let insert_i = block.height as usize;
    assert_eq!(insert_i, chain.blocks.len(), "validate passed, so the block extends the tip");
    // The tip's hash is held, so linking is checked without hashing the previous block again.
    assert!(links_to_tip(chain, &block), "validate passed, so the block links to the tip");
    assert!(insert_i == 0 || block.previous_block_hash() != Blake3Hash([0u8; 32]));
    assert!(!block.headers.is_empty());
    assert_eq!(chain.voting_rosters.len(), insert_i, "voting_rosters is indexed like blocks");
    // Heights with the same voting roster share one entry.
    let voted = match chain.voting_rosters.last() {
        Some(last) if **last == *voted => Arc::clone(last),
        _ => voted,
    };
    chain.voting_rosters.push(voted);
    chain.hash_to_height.insert(block_hash, insert_i as u64);
    chain.blocks.push(block);

    if !stakes.is_empty() {
        chain.roster = stakes;
    } else {
        // Zero aggregated stakes, having previously had real stake.
        //
        // This is legitimately reachable: unbonding every active bond drives total stake to
        // zero, and the chain has to keep running across that gap. Neither adopting the empty
        // set nor dying is right: an empty roster would leave BFT with no validators at all, and
        // that is not what the chain means. The last known-consistent validator set is still the
        // best available answer, so the previous roster is carried forward unchanged -- a "ghost"
        // roster -- until a new bond yields a fresh, consistent stake set, at which point the
        // branch above replaces it wholesale. Carrying over is implicit: `roster` is deliberately
        // NOT written here.
        //
        // A torn stake cache presents identically, so this is still logged loudly. If the roster
        // never recovers once new bonds exist, that is the case to suspect, and
        // `zebrad --fixup-db-stake` remains the repair path.
        if chain.roster.iter().any(|val| val.voting_power > 1) {
            tracing::warn!(
                "No aggregated stakes for BFT height {} (snapshot {:?}); carrying the previous \
                 roster forward as a ghost roster ({} finalizers) until a new bond \
                 establishes a consistent stake set. Expected when every bond has been \
                 unbonded. If it persists once new bonds exist the stake cache may be torn \
                 -- stop the node and run `zebrad --fixup-db-stake` to repair it.",
                insert_i, final_snapshot, chain.roster.len(),
            );
        }
    }

    // The roster for the NEXT height (tenderlink advances to it after this decision): its index is
    // the new chain length. Exclude finalizers terminated at that height, inclusive, so they are
    // already out of the roster that will vote on a slashfork block scheduled there.
    // usize to u64 is lossless on every supported target.
    let next_bft_height = chain.blocks.len() as u64;
    let (next_roster, next_voting) =
        roster_for_bft_height(network, slashforks, &chain.roster, next_bft_height, final_snapshot.map(|(height, _)| height));
    // A decision no longer commits. It advances the final snapshot, which may name a block on a
    // chain that is not `bc_best`; `fin` moves only where the best chain changes, and only by the
    // §3.2 rule (FINALITY.md §4.3).
    let tip = Arc::new(DecidedTip {
        height: insert_i as u64,
        block_hash,
        certificate: fat_pointer,
        final_snapshot,
        next_roster,
        next_voting: Arc::new(next_voting),
    });
    chain.tip = Some(Arc::clone(&tip));
    tip
}

/// The roster that votes on the next height to decide, `chain.blocks.len()`: the one the last
/// decision (or `restore`) stored in the tip. Before genesis, the roster genesis is decided by,
/// built from `chain.roster` as it stands; that is the only place it is built.
fn next_voting(chain: &BftChain, network: &Network, slashforks: &[SlashForkConfig]) -> Arc<VotingRoster> {
    let () = ASSUME_NEXT_ROSTER_IS_THE_TIPS;
    match &chain.tip {
        Some(tip) => Arc::clone(&tip.next_voting),
        None => Arc::new(roster_for_bft_height(network, slashforks, &chain.roster, 0, None).1),
    }
}

/// Whether `block`'s previous-block pointer names `chain`'s decided tip (the null pointer before
/// genesis): what validation requires of every new decision, and replay of every stored one.
fn links_to_tip(chain: &BftChain, block: &BftBlock) -> bool {
    block.previous_block_fat_ptr.points_at_block_hash() == chain.tip_block_hash()
}

/// The stakes at `snapshot`, as the roster of the height after a decision that finalizes it is
/// built from (FINALITY.md §7): from the non-finalized state that holds them, or from the
/// finalized database once `fin` or the depth commit has moved them there. Empty when neither
/// has them.
fn stakes_at(block_writer: &WriteBlockWorkerTask, snapshot: Hash) -> Vec<RosterMember> {
    block_writer
        .non_finalized_state
        .aggregated_stakes_at(snapshot)
        .or_else(|| block_writer.finalized_state.db.aggregated_stakes(&snapshot))
        .unwrap_or_default()
        .into_iter()
        .map(|s| RosterMember { pub_key: s.0, voting_power: s.1, txids: Vec::new(), finalizer_address: None })
        .collect()
}

/// The rounds tenderlink starts with: those that decided the newest
/// `RECENT_COMMIT_ROUND_CACHE_LIMIT` heights below `decided_n`, read back from the store as an
/// older one is read when a peer needs it (`fetch_decided_round`). Those below a round that cannot be read back are left out.
fn startup_rounds(db: &ZebraDb, voting_rosters: &[Arc<VotingRoster>], decided_n: u64, active_roster_max_n: usize) -> Vec<RoundData> {
    let from = decided_n.saturating_sub(tenderlink::RECENT_COMMIT_ROUND_CACHE_LIMIT as u64);
    // Only the unbroken run ending at the tip: tenderlink's cache is contiguous, and it reads a
    // round below it back when a peer needs one.
    let mut rounds: Vec<RoundData> = (from..decided_n)
        .rev()
        .map_while(|height| stored_round(db, voting_rosters.get(usize::try_from(height).ok()?)?, height, active_roster_max_n))
        .collect();
    rounds.reverse();
    rounds
}

/// Assumption: `restore` replays only rows this node's own `store_decision` wrote, so it checks
/// their linkage and nothing else (no signature, certificate or block rule).
const ASSUME_REPLAY_ROWS_ARE_THIS_NODES: () = ();

/// Assumption, kept by `store_decision`: `apply_decision` is given only decisions
/// `store_decision` checked in the same call, so it checks no signature, certificate or block
/// rule itself.
const ASSUME_ADMITTED_BEFORE_APPLY: () = ();

/// The conflicting quorums reported most recently, newest last, with the other block's
/// certificate, so each height and block is reported once, even when two of them alternate.
static CONFLICTING_QUORUMS_REPORTED: Mutex<VecDeque<(ConflictingQuorum, FatPointerToBftBlock)>> = Mutex::new(VecDeque::new());

/// When the known conflicting quorums were last reported, to report them again every
/// `DROPPED_BFT_REPORT_INTERVAL` while any is known.
static CONFLICTING_QUORUMS_LAST_REPORTED: Mutex<Option<std::time::Instant>> = Mutex::new(None);

/// Publishes the known conflicting quorums in the recency status.
fn publish_conflicts(reported: &VecDeque<(ConflictingQuorum, FatPointerToBftBlock)>) {
    let conflicts: Vec<ConflictingQuorum> = reported.iter().map(|(conflict, _)| conflict.clone()).collect();
    RECENCY_STATUS.send_modify(|status| status.conflicts = conflicts);
}

/// How many conflicting quorums `CONFLICTING_QUORUMS_REPORTED` remembers.
const CONFLICTING_QUORUMS_REMEMBERED: usize = 16;

/// Reports, loudly and once per height and block, a quorum for block `other` at a height where
/// this node stored `stored`: finalizers precommitted both. The stored decision stands and BFT
/// keeps running: stopping it would let anyone holding finalizer keys from the past, which cost
/// nothing once unbonded, stop BFT on every node that sees such a quorum. `how` says where the
/// other quorum was seen.
fn report_conflicting_quorum(height: u64, stored: Blake3Hash, certificate: &FatPointerToBftBlock, how: &str) {
    let other = certificate.points_at_block_hash();
    let mut reported = CONFLICTING_QUORUMS_REPORTED.lock().unwrap();
    if reported.iter().any(|(conflict, _)| (conflict.height, conflict.other) == (height, other.0)) {
        return;
    }
    if reported.len() == CONFLICTING_QUORUMS_REMEMBERED {
        reported.pop_front();
    }
    let conflict = ConflictingQuorum {
        height,
        stored: stored.0,
        other: other.0,
        seen_as: how.to_string(),
        other_signers: certificate.signatures.iter().map(|signature| signature.pub_key).collect(),
    };
    reported.push_back((conflict, certificate.clone()));
    publish_conflicts(&reported);
    *CONFLICTING_QUORUMS_LAST_REPORTED.lock().unwrap() = Some(std::time::Instant::now());
    // The evidence outlives a restart. A write that fails loses only the evidence: the conflict
    // is still reported, here and in the status.
    if let (Some(db), Ok(height32)) = (BFT_DB.read().unwrap().as_ref(), u32::try_from(height)) {
        if let Err(err) = db.write_bft_conflicting_certificate(height32, other.0, certificate) {
            tracing::error!("crosslink: could not store the certificate of the conflicting BFT quorum at height {height}: {err}");
        }
    }
    metrics::counter!("state.crosslink.bft_conflicting_quorums").increment(1);
    let seen = CONFLICTING_QUORUMS_SEEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    RECENCY_STATUS.send_modify(|status| status.conflicting_quorums = seen);
    tracing::error!(
        "crosslink: CONFLICTING BFT QUORUM at height {height}. This node stored block {stored} as decided \
         there, and {how} block {other}: at least a third of that height's stake signed both. This node \
         keeps the stored decision and BFT keeps running. Report this with both block hashes.",
    );
}

/// Reports, loudly, a quorum of precommits tenderlink holds at its height for a value other than
/// the decision this node already stored there: finalizers precommitted both. Tenderlink takes
/// the stored decision and moves on, and BFT keeps running: stopping it here would let anyone
/// holding finalizer keys from the past, which cost nothing once unbonded, stop BFT on every node
/// catching up. Called just before tenderlink takes the decided heights, while it still holds its
/// rounds at this height.
fn note_conflicting_quorum(bft_state: &TMState) {
    let height = bft_state.height;
    let stored = {
        let chain = BFT_CHAIN.read().unwrap();
        // usize to u64 is lossless on every supported target.
        if chain.blocks.len() as u64 <= height {
            return;
        }
        let Some(block) = usize::try_from(height).ok().and_then(|h| decided_block(&chain, h)) else { return };
        block.blake3_hash()
    };
    for round in bft_state.rounds_data.iter().filter(|round| round.height == height && round.proposal_is_certified(&round.roster)) {
        if round.proposal_id.0 == stored.0 {
            continue;
        }
        let how = format!("tenderlink holds a quorum of precommits in round {} for", round.round);
        let certificate = tenderlink::round_data_to_fat_pointer(round, &round.roster);
        report_conflicting_quorum(height, stored, &certificate, &how);
    }
}

/// This node's tenderlink WAL records from BFT height `from` up, each with its height, as
/// tenderlink stored them.
fn stored_tenderlink_wal(from: u64) -> Vec<(u64, Vec<u8>)> {
    let db = BFT_DB.read().unwrap();
    let Some(db) = db.as_ref() else {
        tracing::error!("there is no database to read tenderlink's WAL records from");
        return Vec::new();
    };
    let Ok(from) = u32::try_from(from) else { return Vec::new() };
    db.tenderlink_wal(from).into_iter().map(|(height, record)| (u64::from(height), record)).collect()
}

/// Stores tenderlink's WAL record for `height`, synced to disk. `false` when it is not stored,
/// which keeps tenderlink from sending what the record holds.
fn write_tenderlink_wal(height: u64, record: Vec<u8>) -> bool {
    let db = BFT_DB.read().unwrap();
    let (Some(db), Ok(height)) = (db.as_ref(), u32::try_from(height)) else {
        tracing::error!("there is no database to store tenderlink's WAL record for BFT height {height} in");
        return false;
    };
    match db.write_tenderlink_wal(height, record) {
        Ok(()) => true,
        Err(err) => {
            tracing::error!("could not store tenderlink's WAL record for BFT height {height}: {err}");
            false
        }
    }
}

pub(super) struct ParkedDecision {
    source: Source,
    block: BftBlock,
    fat_pointer: FatPointerToBftBlock,
    proposal_sigs: Vec<TMSig>,
    reply: Option<ImportReply>,
}

/// The genesis bootstrap decided, waiting to be stored (it may park on its snapshot) before
/// tenderlink starts on it.
struct PendingBootstrap {
    genesis: BftBlock,
    fat_pointer: FatPointerToBftBlock,
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
        let mut runner = BftRunner {
            rt,
            network: read_state.network().clone(),
            params: read_state.network().crosslink_parameters(),
            slashforks: config.slashfork_schedule.clone(),
            launch,
            pending_bootstrap: None,
            parked_decision: None,
            next_diagnostic: std::time::Instant::now(),
            catching_up: false,
            spawn_deferred: false,
            has_wal: false,
            stored_from: None,
            screened_certificate: None,
        };
        metrics::gauge!("state.crosslink.bft_dropped").set(0.0);
        *BFT_DB.write().unwrap() = Some(block_writer.finalized_state.db.clone());
        crate::new_network::fin::load(&block_writer.finalized_state.db);
        // The WAL keeps its newest record whatever it prunes, so a node that has signed in this
        // state holds at least one.
        runner.has_wal = !block_writer.finalized_state.db.tenderlink_wal(0).is_empty();
        if runner.launch.is_some() {
            // A chain-bootstrapped node restarting with a stored chain and no WAL holds tenderlink
            // back until the sync loop has heard from peers whether it is behind
            // (`set_catching_up`); it is started by `tick` then. One with a WAL, and a
            // supplied-BFT network, starts it here.
            runner.catching_up = !runner.fetches_genesis() && !runner.has_wal;
            runner.restore(read_state, block_writer);
        }
        // Conflicting quorums seen before this start, from the certificates the state keeps.
        {
            let chain = BFT_CHAIN.read().unwrap();
            let mut reported = CONFLICTING_QUORUMS_REPORTED.lock().unwrap();
            for (height, other, certificate) in block_writer.finalized_state.db.bft_conflicting_certificates() {
                let Some(stored) = decided_block(&chain, height as usize).map(|block| block.blake3_hash()) else { continue };
                if reported.len() == CONFLICTING_QUORUMS_REMEMBERED {
                    reported.pop_front();
                }
                let conflict = ConflictingQuorum {
                    height: u64::from(height),
                    stored: stored.0,
                    other,
                    seen_as: "a certificate kept before this start is for".to_string(),
                    other_signers: certificate.signatures.iter().map(|signature| signature.pub_key).collect(),
                };
                reported.push_back((conflict, certificate));
            }
            if !reported.is_empty() {
                let seen = u32::try_from(reported.len()).unwrap_or(u32::MAX);
                CONFLICTING_QUORUMS_SEEN.store(seen, std::sync::atomic::Ordering::Relaxed);
                RECENCY_STATUS.send_modify(|status| status.conflicting_quorums = seen);
                publish_conflicts(&reported);
                // Reported again on the interval, starting now.
                *CONFLICTING_QUORUMS_LAST_REPORTED.lock().unwrap() = Some(std::time::Instant::now() - DROPPED_BFT_REPORT_INTERVAL);
            }
        }
        runner
    }

    /// Whether this network's BFT genesis comes from peers. On a chain-bootstrapped network every
    /// node derives genesis itself (`bootstrap`), so it is never fetched.
    pub(super) fn fetches_genesis(&self) -> bool {
        !matches!(self.params.bootstrap, BftBootstrap::FromChain { .. })
    }

    /// Checks a decision for a height this node has already decided against the roster that voted
    /// there. Another block with a valid certificate means at least a third of that height's stake
    /// signed two blocks: it is reported loudly, and the stored decision stands; BFT never stops
    /// for it, since stopping would let anyone holding old finalizer keys stop BFT on every node
    /// (decisions table, 2026-11-19). `how` says where the other block was seen, for the report.
    /// The same block costs a hash; a conflict reported already is not verified again.
    pub(super) fn check_decided_height(&self, block: &BftBlock, fat_pointer: &FatPointerToBftBlock, how: &str) -> DecidedCheck {
        let height = u64::from(block.height);
        let chain = BFT_CHAIN.read().unwrap();
        // usize to u64 is lossless on every supported target.
        if height >= chain.blocks.len() as u64 {
            return DecidedCheck::NotChecked;
        }
        let block_hash = block.blake3_hash();
        if chain.hash_to_height.get(&block_hash) == Some(&height) {
            return DecidedCheck::Same;
        }
        if height == 0 {
            return DecidedCheck::NotChecked;
        }
        if fat_pointer.points_at_block_hash() != block_hash {
            return DecidedCheck::Invalid { why: format!("the fat pointer for BFT height {height} names another block") };
        }
        // Reported already: its certificate was found valid then, so it need not be verified again.
        if CONFLICTING_QUORUMS_REPORTED.lock().unwrap().iter().any(|(conflict, _)| (conflict.height, conflict.other) == (height, block_hash.0)) {
            return DecidedCheck::Conflict;
        }
        let Some(voting) = chain.voting_rosters.get(height as usize).cloned() else {
            return DecidedCheck::NotChecked;
        };
        let stored = decided_block(&chain, height as usize).map(|stored| stored.blake3_hash());
        drop(chain);
        if !certificate_is_valid(fat_pointer, height, &voting) {
            return DecidedCheck::Invalid { why: format!("the fat pointer is not a valid certificate for BFT height {height}") };
        }
        let Some(stored) = stored else { return DecidedCheck::NotChecked };
        report_conflicting_quorum(height, stored, fat_pointer, how);
        DecidedCheck::Conflict
    }

    /// Takes in the rows `restore` stopped at, in order, each once it is the next height and its
    /// snapshot is held. A height decided meanwhile from a peer rewrote its row, so the next height
    /// is always read from the store. Stops for good at the first row that is missing or refused;
    /// a missing snapshot or a parked decision only waits.
    pub(super) fn feed_stored(&mut self, read_state: &ReadState, block_writer: &mut WriteBlockWorkerTask) -> usize {
        let mut taken = 0;
        while self.stored_from.is_some() {
            let next = decided_len();
            let decision = u32::try_from(next).ok().and_then(|height| block_writer.finalized_state.db.bft_decision(height));
            let Some(StoredDecision { block, fat_pointer, proposal_sigs }) = decision else {
                self.stored_from = None;
                return taken;
            };
            match self.store_decision(Source::Stored, &block, &fat_pointer, &proposal_sigs, None, read_state, block_writer) {
                Ok(Stored::Decided) => taken += 1,
                Ok(Stored::AlreadyDecided | Stored::Parked) => {
                    self.stored_from = None;
                    return taken;
                }
                Err(Refusal { kind: RefusalKind::NotYet, .. }) => return taken,
                Err(refusal) => {
                    tracing::warn!(
                        "crosslink: the stored BFT decision at height {next} cannot be taken in ({}); its height is decided from peers",
                        refusal.why,
                    );
                    self.stored_from = None;
                    return taken;
                }
            }
        }
        taken
    }

    /// Store a decided height fetched from a peer, if it is the next one and its snapshot is
    /// committed, through `store_decision`. Its certificate must carry a quorum of the height's
    /// roster whatever the upgrade: no tenderlink round vouches for it.
    pub(super) fn feed_fetched(&mut self, decision: super::bft_sync::WireDecision, read_state: &ReadState, block_writer: &mut WriteBlockWorkerTask) -> FeedOutcome {
        // The message's own label, before anything about the decision: no honest peer mislabels.
        if decision.block.height as u64 != decision.height {
            return FeedOutcome::Refused {
                why: format!("a block for BFT height {} sent as height {}", decision.block.height, decision.height),
                peer_fault: true,
            };
        }
        let () = super::bft_sync::ASSUME_BFT_BLOCK_HAS_HEADERS;
        debug_assert!(!decision.block.headers.is_empty());
        match self.store_decision(Source::Fetched, &decision.block, &decision.fat_pointer, &decision.proposal_sigs, None, read_state, block_writer) {
            Ok(Stored::Decided | Stored::AlreadyDecided | Stored::Parked) => FeedOutcome::Fed,
            Err(Refusal { kind: RefusalKind::NotYet, .. }) => FeedOutcome::NotReady(decision),
            Err(Refusal { why, kind: RefusalKind::Fault }) => FeedOutcome::Refused { why, peer_fault: true },
            Err(Refusal { why, kind: RefusalKind::Refused }) => FeedOutcome::Refused { why, peer_fault: false },
        }
    }

    /// Work that runs at the start of every tick.
    pub(super) fn tick(&mut self, read_state: &ReadState, block_writer: &mut WriteBlockWorkerTask) {
        // Tenderlink is not running but decisions are stored: held back while catching up, or the
        // genesis (or more) arrived from peers or an import before bootstrap could decide it, so
        // bootstrap never starts it. Start it from everything stored once not catching up. Only
        // on chain-bootstrapped networks: where BFT is supplied, tenderlink starts at
        // construction, and starting it here would race later imports.
        if self.launch.is_some()
            && !self.fetches_genesis()
            && !self.catching_up
            && self.pending_bootstrap.is_none()
            && decided_len() > 0
        {
            self.spawn_deferred = false;
            // The chain is in memory, so tenderlink starts from its tip; rows `restore` stopped at
            // are taken in by `feed_stored`. A start that cannot read the tip's round waits for the
            // next decision.
            self.start_from_tip(block_writer);
        }

        // Crosslink bootstrap: the first accepted PoW block at the activation height (h2)
        // finalizes h1 through a deterministic genesis decision, and BFT starts at height 1 with
        // h1's roster. A network whose BFT is supplied has no activation height and never
        // bootstraps.
        self.finish_bootstrap(block_writer);
        if let (Some(_), Some(activation_height)) = (self.launch.as_ref(), self.params.bootstrap.activation_height()) {
            if read_state.best_tip().is_some_and(|(tip, _)| tip.0 >= activation_height) {
                self.bootstrap(read_state, block_writer);
            }
        }

        if let Some(ParkedDecision { source, block, fat_pointer, proposal_sigs, reply }) = self.parked_decision.take() {
            // With BFT dropped, the refusal answers the reply as it would have been answered.
            let _ = self.store_decision(source, &block, &fat_pointer, &proposal_sigs, reply, read_state, block_writer);
            publish_parked_decision_height(self.parked_decision.as_ref().map(|parked| u64::from(parked.block.height)));
        }

        if self.next_diagnostic.elapsed() >= DIAGNOSTIC_INTERVAL {
            self.next_diagnostic = std::time::Instant::now();
            let latest_final = BFT_CHAIN.read().unwrap().bft_final_snapshot();
            if let (Some((tip_height, _)), Some((final_height, _))) = (read_state.best_tip(), latest_final) {
                if tip_height < final_height {
                    tracing::info!("Our PoW tip is {} blocks away from the latest final block.", final_height - tip_height);
                } else if tip_height - final_height > 512 {
                    tracing::warn!("WARNING! BFT-Finality is falling behind the PoW chain. Current gap to tip is {:?} blocks.", tip_height - final_height);
                }
            }
        }

    }

    /// Serves one of tenderlink's requests, on the sync thread (see `SyncWakes`).
    pub(super) fn handle(&mut self, request: BftRequest, read_state: &ReadState, block_writer: &mut WriteBlockWorkerTask) {
        match request {
            BftRequest::Propose { reply } => {
                let _ = reply.send(if bft_is_dropped() { None } else { self.propose(read_state) });
            }
            BftRequest::Validate { block, reply } => {
                if bft_is_dropped() {
                    let _ = reply.send((TMStatus::Indeterminate, TMStatusReason::None));
                    return;
                }
                let chain = BFT_CHAIN.read().unwrap();
                let _ = reply.send(self.validate(&chain, read_state, &block));
            }
            BftRequest::Decided { block, fat_pointer, proposal_sigs } => {
                let _ = self.store_decision(Source::Tenderlink, &block, &fat_pointer, &proposal_sigs, None, read_state, block_writer);
            }
            BftRequest::ForceFeed { block, fat_pointer, proposal_sigs, valid_round, require_certificate, reply } => {
                let source = Source::Imported { valid_round, require_certificate };
                let _ = self.store_decision(source, &block, &fat_pointer, &proposal_sigs, Some(reply), read_state, block_writer);
            }
        }
    }

    /// The voting roster of the next height to decide, `chain.blocks.len()`: the one the last
    /// decision (or `restore`) stored in the tip and handed tenderlink. Before genesis, the roster
    /// genesis is decided by, built from `chain.roster` as it stands.
    fn next_voting_roster(&self, chain: &BftChain) -> Arc<VotingRoster> {
        next_voting(chain, &self.network, self.slashforks.rules())
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
        let bft_final_snapshot = chain.bft_final_snapshot();
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
        let fat_ptr = chain.fat_pointer_to_tip();
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
        // repeat them is undecided, so this keeps declining until
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
        {
            use zcash_protocol::consensus::{BlockHeight, NetworkUpgrade, Parameters as _};
            let network = read_state.network();
            if network.is_nu_active(NetworkUpgrade::FeaturenetUpgrade2, BlockHeight::from(finality_candidate_height.0)) {
                if let Err(reason) =
                    carried_headers_meet_required_difficulty(read_state, candidate_hash, finality_candidate_height, &headers)
                {
                    tracing::info!("not proposing: the headers above the candidate fail Tail Confirmation: {reason}");
                    return None;
                }
            }
        }

        // The user-led slashforks scheduled at this BFT height (several rules may share one
        // certificate height). The schedule is canonical, so this filter preserves canonical
        // (ascending pow_activation_height) order -- the same order `validate` requires
        // byte-for-byte.
        let scheduled_slashforks: Vec<SlashForkConfig> = self
            .slashforks
            .rules()
            .iter()
            .filter(|hf| hf.bft_certificate_height == bft_height)
            .cloned()
            .collect();

        // A slashfork block sets do_not_include to the greatest activation height it carries
        // (pointing at this block commits to every certificate in it, so the strictest one
        // governs); otherwise the parent's carries forward so it never regresses (both as
        // `validate` requires).
        let do_not_include = scheduled_slashforks.last().map_or(parent_do_not_include, |last| last.pow_activation_height);
        // Always propose the latest version: existing v1 blocks remain v1 (so their hashes and
        // signatures are untouched), and `validate` caps every block at the latest version, so
        // it is >= any parent's.
        match BftBlock::new(params, BFT_BLOCK_VERSION_LATEST, bft_height as u32, fat_ptr, headers, scheduled_slashforks, do_not_include) {
            Ok(block) => Some(block),
            Err(e) => {
                tracing::warn!("Unable to create BftBlock to propose, Error={:?}", e);
                None
            }
        }
    }

    fn validate(&self, chain: &BftChain, read_state: &ReadState, new_block: &BftBlock) -> (TMStatus, TMStatusReason) {
        self.validate_with_hash(chain, read_state, new_block, new_block.blake3_hash())
    }

    /// `validate`, for a caller that already has the block's hash: hashing re-serializes it.
    fn validate_with_hash(&self, chain: &BftChain, read_state: &ReadState, new_block: &BftBlock, new_block_hash: Blake3Hash) -> (TMStatus, TMStatusReason) {
        let fail = (TMStatus::Fail, TMStatusReason::None);

        // A block already decided at its height is valid there. Tenderlink asks this while it
        // catches up behind decisions the sync loop fetched from peers.
        if chain.hash_to_height.get(&new_block_hash) == Some(&u64::from(new_block.height)) {
            return (TMStatus::Pass, TMStatusReason::None);
        }

        // On a chain-bootstrapped network genesis is the one block every node derives from its
        // own chain. Its nil roster lets any empty certificate pass, so nothing else would tell
        // a different height-0 block from it.
        if new_block.height == 0 && !self.fetches_genesis() {
            match self.build_bootstrap_genesis(read_state) {
                None => return (TMStatus::Indeterminate, TMStatusReason::None),
                Some((genesis, _)) if genesis.blake3_hash() != new_block_hash => {
                    tracing::warn!("refusing a BFT genesis {} that is not this chain's {}", new_block_hash, genesis.blake3_hash());
                    return fail;
                }
                Some(_) => {}
            }
        }

        if !new_block.previous_block_fat_ptr.fits_active_roster(&self.params) {
            tracing::warn!(
                "Block's previous block fat pointer has {} signatures, more than the {} seats of the active roster",
                new_block.previous_block_fat_ptr.signatures.len(),
                self.params.active_roster_max_n,
            );
            return fail;
        }

        if !links_to_tip(chain, new_block) {
            tracing::warn!(
                "Block has invalid previous block fat pointer hash: was {} but should be {}",
                new_block.previous_block_fat_ptr.points_at_block_hash(),
                chain.tip_block_hash(),
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

        // From the first height under FeaturenetUpgrade2's rules, the previous-block pointer must
        // be a certificate for the parent and not only its hash: once this block is decided, that
        // pointer is what `fat_pointer_to_block_at_height` hands miners for the parent.
        if let Some(parent_height) = bft_height.checked_sub(1) {
            if self.next_voting_roster(chain).strict_quorum {
                // u64 to usize: the parent is a decided block, so its height indexes `blocks`.
                let parent_voting = chain
                    .voting_rosters
                    .get(parent_height as usize)
                    .expect("voting_rosters is indexed like blocks, and the parent is decided");
                if !certificate_is_valid(&new_block.previous_block_fat_ptr, parent_height, parent_voting) {
                    tracing::warn!("BFT block at height {} carries a previous-block pointer that is not a valid certificate", bft_height);
                    return fail;
                }
            }
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

        // Without this cap one proposer could get a block above the version honest nodes
        // propose decided, after which every honest proposal fails the check above.
        if new_block.version > BFT_BLOCK_VERSION_LATEST {
            tracing::warn!("BFT block version {} is above the latest known version {}", new_block.version, BFT_BLOCK_VERSION_LATEST);
            return fail;
        }

        // The proposal must carry exactly the slashforks this node has scheduled at this BFT
        // height -- byte-for-byte, in canonical (ascending pow_activation_height) order, and
        // none at all when none are scheduled -- and set its do_not_include_until_bc_height to
        // the greatest activation height among them (pointing at this block commits to every
        // certificate in it, so the strictest one governs).
        //
        // This runs before the generic do_not_include_until_bc_height monotonicity check below
        // so that a regression *caused by the slashfork activation* is reported as its own
        // distinct error rather than the generic one.
        let scheduled_slashforks: Vec<&SlashForkConfig> = self
            .slashforks
            .rules()
            .iter()
            .filter(|hf| hf.bft_certificate_height == bft_height)
            .collect();
        {
            let serialize = |hf: &SlashForkConfig| {
                let mut bytes = Vec::new();
                hf.zcash_serialize(&mut bytes).expect("serializing to a Vec is infallible");
                bytes
            };
            let proposal_matches = new_block.slashforks.len() == scheduled_slashforks.len()
                && new_block
                    .slashforks
                    .iter()
                    .zip(scheduled_slashforks.iter())
                    .all(|(carried, scheduled)| serialize(carried) == serialize(scheduled));
            if !proposal_matches {
                tracing::warn!(
                    "BFT block at height {} must carry exactly the {} scheduled slashfork(s) byte-for-byte in schedule order, but does not",
                    bft_height, scheduled_slashforks.len(),
                );
                return fail;
            }
        }

        // Rules are pow-sorted, so the last scheduled rule has the greatest activation.
        if let Some(last_scheduled) = scheduled_slashforks.last() {
            if new_block.do_not_include_until_bc_height != last_scheduled.pow_activation_height {
                tracing::warn!(
                    "BFT slashfork block at height {} must set do_not_include_until_bc_height to the greatest carried pow_activation_height {}, but it is {}",
                    bft_height, last_scheduled.pow_activation_height, new_block.do_not_include_until_bc_height,
                );
                return fail;
            }
            // Separate error: the slashfork's PoW activation height must not regress the
            // monotonic do_not_include_until_bc_height relative to the parent.
            if last_scheduled.pow_activation_height < parent_do_not_include {
                tracing::warn!(
                    "Slashfork pow_activation_height {} at BFT height {} is below the parent's do_not_include_until_bc_height {}; the slashfork activation regresses do_not_include_until_bc_height",
                    last_scheduled.pow_activation_height, bft_height, parent_do_not_include,
                );
                return fail;
            }
        }

        // Without a scheduled slashfork, do_not_include_until_bc_height must equal the parent's,
        // as honest proposers set it. Allowing it only to rise would let one proposer raise it
        // to u64::MAX, after which no PoW block could point at BFT again and the next slashfork
        // block would fail as a regression. (v1 blocks use the implicit value 0, so this holds
        // for existing history.)
        if scheduled_slashforks.is_empty() && new_block.do_not_include_until_bc_height != parent_do_not_include {
            tracing::warn!(
                "BFT block do_not_include_until_bc_height {} differs from its parent's {} with no slashfork scheduled",
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

        // Tail Confirmation (FINALITY.md §3.4): `headers_bc` is a sigma-header proof-of-work
        // tail on the snapshot. The rule is objective -- it says nothing about this validator's
        // own best chain -- and has four parts: the count, the linkage, the snapshot being a
        // block this state holds, and each header's own proof of work.
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
        // The tail itself is checked from the headers alone: each extends the one below it
        // (above), the deepest extends the snapshot (by definition of the snapshot), and each
        // carries its own proof of work. The blocks behind the headers are never required. A
        // tail is the proposer's `bc_best` at proposal time and bc can orphan it afterwards;
        // nodes retain and serve only the blocks on their chains, so a node catching up on a
        // decided bft-block could never download an orphaned tail, and every bc-block whose fat
        // pointer names that bft-block would stay deferred.
        //
        // This runs before the snapshot lookup, which may ask peers for the snapshot: a tail
        // that fails on its own headers is refused without asking for anything.
        let network = read_state.network();
        for (i, header) in new_block.headers.iter().enumerate() {
            if let Err(reason) = header_pow_is_valid(header, network) {
                tracing::warn!("BFT block header {i} fails proof of work: {reason}");
                return fail;
            }
        }

        // The `snapshot` this proposal finalizes: the parent of the deepest carried header.
        let new_final_hash = Hash(new_block.snapshot_block_hash().0);
        let Some(snapshot) = read_state.known_block(new_final_hash) else {
            tracing::warn!("Didn't have hash available for confirmation: {}", new_final_hash);
            return needs_pow_block(new_final_hash);
        };

        for (i, header) in new_block.headers.iter().enumerate() {
            let height = Height(snapshot.height.0 + 1 + i as u32);
            if let Err(reason) = header_meets_filter_at(header, network, height) {
                tracing::warn!("BFT block header {i} fails proof of work: {reason}");
                return fail;
            }
        }

        // From FeaturenetUpgrade2 each header must also carry the difficulty its position
        // requires, read from the snapshot's own ancestry, so the check stays objective.
        {
            use zcash_protocol::consensus::{BlockHeight, NetworkUpgrade, Parameters as _};
            if network.is_nu_active(NetworkUpgrade::FeaturenetUpgrade2, BlockHeight::from(snapshot.height.0)) {
                if let Err(reason) =
                    carried_headers_meet_required_difficulty(read_state, new_final_hash, snapshot.height, &new_block.headers)
                {
                    tracing::warn!("BFT block fails Tail Confirmation: {reason}");
                    return fail;
                }
            }
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
                        return needs_pow_block(parent_snapshot_hash);
                    }
                }
            }
        }

        (TMStatus::Pass, TMStatusReason::None)
    }

    /// Every decision this node stores comes through here: tenderlink's own, one fetched from a
    /// peer, one supplied from outside, or one it stored before it restarted. Its source sets
    /// which checks it gets and how some outcomes are answered (`Admission`); they run as one
    /// pipeline, and nothing is written until they pass. Then it is applied to the chain in one
    /// step (`apply_decision`), written, and its tip published. Its snapshot
    /// is not finalized here: a decision only moves the final snapshot (FINALITY.md §4.3). Only an
    /// import has a `reply`, answered here whatever happens: `Ok`, or why it was refused.
    /// Tenderlink follows the published tip instead. Tenderlink's own decision can still panic
    /// here on a fact only a bug could break; a conflict or a block rule that now refuses it is
    /// reported, never a halt or a crash.
    fn store_decision(
        &mut self,
        source: Source,
        block: &BftBlock,
        fat_pointer: &FatPointerToBftBlock,
        proposal_sigs: &[TMSig],
        reply: Option<ImportReply>,
        read_state: &ReadState,
        block_writer: &mut WriteBlockWorkerTask,
    ) -> Result<Stored, Refusal> {
        let admitted: Result<(Blake3Hash, Arc<VotingRoster>, Vec<TMSig>), Refusal> = 'admit: {
            if bft_is_dropped() {
                break 'admit Err(Refusal::not_yet("BFT is dropped on this node".to_string()));
            }
            let height = u64::from(block.height);
            // Who votes at the next height: the tip's roster for it.
            let voting = self.next_voting_roster(&BFT_CHAIN.read().unwrap());

            let (opt_out, answers, seen_as, valid_round) = match source {
                // Nothing vouches for it but its certificate. Heights below the next are checked by
                // the sync loop (`check_decided_height`); linkage and headers are block rules here.
                Source::Fetched => (
                    CHECK_DECIDED_HEIGHT | CHECK_LINKS_TO_TIP | CHECK_HEADERS | CHECK_PROPOSAL_ROSTER,
                    KEEP_ONLY_FRESH_PROPOSAL_SIGS,
                    "a peer served",
                    -1,
                ),
                // Supplied from outside, for any height and answered at once. Linkage and headers are
                // block rules here. A quorum is needed where the import asks for one or the roster
                // decides by the strict quorum.
                Source::Imported { valid_round, require_certificate } => (
                    CHECK_GENESIS_NOT_FETCHED | CHECK_NEXT_HEIGHT | CHECK_ROSTER_STAKE | CHECK_CERTIFICATE_EARLY
                        | CHECK_SNAPSHOT_HELD | CHECK_LINKS_TO_TIP | CHECK_HEADERS
                        | CHECK_QUORUM * Admission::from(!require_certificate && !voting.strict_quorum),
                    CONFLICT_REFUSES,
                    "an import carried",
                    valid_round,
                ),
                Source::Tenderlink => {
                    let () = ASSUME_TENDERLINK_DECIDES_ON_ITS_OWN_QUORUM;
                    (
                        CHECK_PARKED | CHECK_GENESIS_NOT_FETCHED | CHECK_NEXT_HEIGHT | CHECK_ROSTER_STAKE
                            | CHECK_CERTIFICATE_EARLY | CHECK_SNAPSHOT_HELD | CHECK_LINKS_TO_TIP | CHECK_HEADERS
                            | CHECK_FITS_ROSTER | CHECK_QUORUM | CHECK_PROPOSAL_ROSTER,
                        FAULTS_ARE_BUGS | PARK_UNSETTLED | REPORT_REFUSED_RULES,
                        "this node's tenderlink decided",
                        -1,
                    )
                }
                // A row this node stored: it gets the checks `restore` gives the rows it replays.
                Source::Stored => {
                    let () = ASSUME_REPLAY_ROWS_ARE_THIS_NODES;
                    (
                        CHECK_NAMES_BLOCK | CHECK_DECIDED_HEIGHT | CHECK_GENESIS_NOT_FETCHED | CHECK_ROSTER_STAKE
                            | CHECK_CERTIFICATE_EARLY | CHECK_FITS_ROSTER | CHECK_BLOCK_RULES | CHECK_SIGNATURES
                            | CHECK_QUORUM | CHECK_PROPOSAL_ROSTER,
                        MARK_MISSING_SNAPSHOT,
                        "this node stored",
                        -1,
                    )
                }
            };
            let admission = (CHECK_ALL & !opt_out) | answers;
            let does = |what: Admission| admission & what != 0;
            // A broken fact: the sender's fault, or a bug in this node.
            let fault = |why: String| {
                if does(FAULTS_ARE_BUGS) {
                    panic!("{why}");
                }
                Refusal::fault(why)
            };

            // Hashing re-serializes the block, so it is done once.
            let block_hash = block.blake3_hash();
            if does(CHECK_NAMES_BLOCK) && fat_pointer.points_at_block_hash() != block_hash {
                break 'admit Err(fault(format!(
                    "Fat Pointer hash does not match block hash. fp: {} block: {}",
                    fat_pointer.points_at_block_hash(), block_hash,
                )));
            }
            // Tenderlink decides one height at a time, so it can be at a height the sync loop has
            // decided from peers since; the stored decision stands. Another block there needs a
            // conflicting quorum: finalizers equivocated, which is reported, never a halt.
            if does(CHECK_DECIDED_HEIGHT) && height < decided_len() {
                match self.check_decided_height(block, fat_pointer, seen_as) {
                    DecidedCheck::Invalid { why } if does(FAULTS_ARE_BUGS) => panic!("{why}"),
                    DecidedCheck::Invalid { why } => break 'admit Err(Refusal::refused(why)),
                    DecidedCheck::Conflict if does(CONFLICT_REFUSES) => break 'admit Err(Refusal::refused(format!(
                        "BFT height {height} is decided already with another block; this one's certificate is valid, \
                         so it is reported as a conflicting quorum",
                    ))),
                    DecidedCheck::NotChecked if does(CONFLICT_REFUSES) => {
                        break 'admit Err(Refusal::refused(format!("BFT height {height} is decided already with another block")));
                    }
                    DecidedCheck::Same | DecidedCheck::Conflict | DecidedCheck::NotChecked => {
                        // An import is accepted once; saying so beats a roster check of the wrong
                        // height that would refuse it for a reason that is not true.
                        if let Some(reply) = reply {
                            let _ = reply.send(Err(format!("BFT height {} is decided already with this block", block.height)));
                        }
                        return Ok(Stored::AlreadyDecided);
                    }
                }
            }
            // A decision at the next height is final already and waits only for its snapshot;
            // storing another one there would fork the chain under it.
            if does(CHECK_PARKED) && self.parked_decision.is_some() {
                break 'admit Err(Refusal::not_yet("a decided BFT block is waiting for its snapshot".to_string()));
            }
            // Judged by our bootstrap config, so not the sender's fault.
            if does(CHECK_GENESIS_NOT_FETCHED) && height == 0 && !self.fetches_genesis() {
                break 'admit Err(Refusal::refused("genesis is derived from the chain, never fetched".to_string()));
            }
            if does(CHECK_NEXT_HEIGHT) && height != decided_len() {
                break 'admit Err(Refusal::not_yet(format!("BFT height {height} is not the next height to decide")));
            }
            // Past genesis, a roster with no stake has no honest decision to accept: the chain has
            // failed, and only an operator can move it on (an import). Hold the decision, say so once.
            if does(CHECK_ROSTER_STAKE) && height > 0 && voting.roster.iter().map(|member| member.stake).sum::<u64>() == 0 {
                if !ZERO_STAKE_REPORTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    tracing::error!(
                        "crosslink: BFT height {} has no stake in its roster, so no decision there is accepted from peers. \
                         BFT cannot continue without manual intervention.",
                        height,
                    );
                }
                break 'admit Err(Refusal::not_yet(format!("BFT height {height} has no stake in its roster")));
            }
            // A decision that waits for its snapshot keeps its height in the sync loop's fetch
            // window, and the loop asks nobody else for a height it holds. So before one may wait it
            // must be a valid certificate for this height, which needs only what this node holds;
            // otherwise any peer could hold a height with a block on a made-up snapshot. Checked once
            // per decision, not on every PoW commit while it waits; `CHECK_QUORUM` checks it again
            // before it is stored.
            if does(CHECK_CERTIFICATE_EARLY) && self.screened_certificate.as_ref() != Some(&(height, fat_pointer.clone())) {
                if !certificate_is_valid(fat_pointer, height, &voting) {
                    break 'admit Err(Refusal::refused(format!("the fat pointer is not a valid certificate for BFT height {height}")));
                }
                self.screened_certificate = Some((height, fat_pointer.clone()));
            }
            // Nothing about it settles before its snapshot is committed. Checking that first keeps a
            // decision waiting in the fetch window from being verified again on every PoW commit.
            if does(CHECK_SNAPSHOT_HELD) {
                let snapshot = Hash(block.snapshot_block_hash().0);
                if read_state.known_block(snapshot).is_none() {
                    if does(MARK_MISSING_SNAPSHOT) {
                        // On the decided chain: fetched whatever height a peer alleges, and never expired.
                        DECIDED_MISSING_POW_BLOCKS.lock().unwrap().insert(snapshot);
                    }
                    break 'admit Err(Refusal::not_yet(format!("the snapshot of BFT height {height} is not held yet")));
                }
            }
            if does(CHECK_LINKS_TO_TIP) && !links_to_tip(&BFT_CHAIN.read().unwrap(), block) {
                break 'admit Err(Refusal::refused(format!("the stored decision at BFT height {height} does not link to the decided tip")));
            }
            // `store_decision` stores no block without headers.
            if does(CHECK_HEADERS) && block.headers.is_empty() {
                break 'admit Err(Refusal::refused(format!("BFT height {height} is stored without headers")));
            }
            if does(CHECK_FITS_ROSTER) && !fat_pointer.fits_active_roster(&self.params) {
                break 'admit Err(Refusal::refused(format!(
                    "the fat pointer has {} signatures, more than the {} seats of the active roster",
                    fat_pointer.signatures.len(), self.params.active_roster_max_n,
                )));
            }
            if does(CHECK_BLOCK_RULES) {
                let (status, reason) = self.validate_with_hash(&BFT_CHAIN.read().unwrap(), read_state, block, block_hash);
                match status {
                    TMStatus::Pass => {}
                    // Tenderlink decides only on a Pass from this node, so the block this
                    // re-validation lacks was here then and has been dropped from the non-finalized
                    // state since. The decision is final regardless; it is recorded once that block is
                    // back. Recording it now with the last roster would guess the next height's
                    // validator set, which is the stake at a snapshot this node does not hold.
                    TMStatus::Indeterminate if does(PARK_UNSETTLED) => {
                        // Its height is published while it is parked, so a retry says so quietly.
                        if *PARKED_DECISION_HEIGHT.lock().unwrap() == Some(height) {
                            tracing::debug!("decided BFT block {} at height {} is still parked: {reason:?}", block_hash, block.height);
                        } else {
                            tracing::warn!("parking decided BFT block {} at height {}: {reason:?}", block_hash, block.height);
                        }
                        if let TMStatusReason::NeedsBlock { hash } = &reason {
                            // The proposal holds a decision's worth of precommits, so the block it
                            // needs is on the decided chain: fetched whatever height a peer alleges,
                            // and BFT is dropped if it turns out to conflict with a committed block.
                            DECIDED_MISSING_POW_BLOCKS.lock().unwrap().insert(Hash(*hash));
                        }
                        publish_parked_decision_height(Some(height));
                        self.parked_decision = Some(ParkedDecision {
                            source,
                            block: block.clone(),
                            fat_pointer: fat_pointer.clone(),
                            proposal_sigs: proposal_sigs.to_vec(),
                            reply,
                        });
                        return Ok(Stored::Parked);
                    }
                    // A fetched decision waits in the fetch window; an import is answered at once.
                    TMStatus::Indeterminate => {
                        break 'admit Err(Refusal::not_yet(format!("PoS validation = {status:?}: {reason:?}")));
                    }
                    TMStatus::Fail => {
                        // Tenderlink validated the proposal against this node's chain when it arrived,
                        // so a rule that refuses it now means that chain moved under it. Not stored and
                        // not a crash: tenderlink waits at this height until the tip passes it.
                        if does(REPORT_REFUSED_RULES) {
                            metrics::counter!("state.crosslink.bft_own_decisions_refused").increment(1);
                            tracing::error!(
                                "crosslink: this node's tenderlink decided BFT block {} at height {}, which now fails this \
                                 node's block rules ({reason:?}). It is not stored; tenderlink waits until that height is \
                                 decided from peers.",
                                block_hash, block.height,
                            );
                        }
                        break 'admit Err(Refusal::refused(format!("PoS validation = {status:?}: {reason:?}")));
                    }
                }
            }
            // Vote namespacing: the precommit signatures were made at this block's height with that
            // height's namespace folded in, so they are verified with the same namespace. Without the
            // quorum check, a node would hand miners, as the tip's certificate, a pointer its own
            // admission refuses.
            if does(CHECK_QUORUM) {
                if !certificate_is_valid(fat_pointer, height, &voting) {
                    break 'admit Err(Refusal::refused(format!("the fat pointer is not a valid certificate for BFT height {height}")));
                }
            } else if does(CHECK_SIGNATURES) && !fat_pointer.validate_signatures(&voting.vote_namespace) {
                if does(FAULTS_ARE_BUGS) {
                    panic!("Signatures are not valid. Rejecting block.");
                }
                break 'admit Err(Refusal::refused(format!(
                    "the fat pointer for BFT height {height} carries a signature that does not verify",
                )));
            }
            // A supplied decision against the roster, as a real network has one. The genesis
            // decision is not proposed, so it has no proposal signatures, as the bootstrap decides
            // it; but the stake at its snapshot is the next height's roster, so it must have some,
            // or BFT could never decide again (the bootstrap refuses to start tenderlink on such a
            // roster). Every later decision has the round's proposer's signatures, one per chunk,
            // over each chunk as it was sent in `valid_round`.
            if does(CHECK_PROPOSAL_ROSTER) && height == 0 {
                if !proposal_sigs.is_empty() {
                    break 'admit Err(Refusal::refused(format!(
                        "{} proposal signatures on the genesis decision, which is not proposed", proposal_sigs.len(),
                    )));
                }
                let snapshot = Hash(block.snapshot_block_hash().0);
                if stakes_at(block_writer, snapshot).is_empty() {
                    break 'admit Err(Refusal::refused(format!(
                        "the BFT genesis finalizes {snapshot}, where nothing is bonded, so BFT would have no roster",
                    )));
                }
            } else if does(CHECK_PROPOSAL_ROSTER) {
                let round = fat_pointer.get_vote_template().round;
                let Ok(round) = u32::try_from(round) else {
                    break 'admit Err(Refusal::refused(format!("the fat pointer's round {round} is negative")));
                };
                // An empty roster has no proposer; tenderlink would log asking it for one.
                let proposer_i = if voting.roster.is_empty() {
                    None
                } else {
                    tenderlink::proposer_index(&HashKeys::default(), &voting.roster, self.params.active_roster_max_n, height, round)
                };
                let Some(proposer_i) = proposer_i else {
                    break 'admit Err(Refusal::refused(format!("BFT height {height} has no proposer: its roster has no stake")));
                };
                let value = block.zcash_serialize_to_vec().expect("a BFT block serializes");
                let chunks_n = tenderlink::proposal_chunks_n(value.len());
                if proposal_sigs.len() != chunks_n {
                    break 'admit Err(Refusal::refused(format!(
                        "{} proposal signatures for a proposal of {chunks_n} chunks", proposal_sigs.len(),
                    )));
                }
                let proposer = voting.roster[proposer_i].pub_key;
                let value_id = tenderlink::ValueId(fat_pointer.points_at_block_hash().0);
                let first_bad = tenderlink::first_bad_proposal_chunk_sig(
                    proposer, &value, value_id, height, round, valid_round, &voting.vote_namespace, proposal_sigs,
                );
                if let Some(chunk_i) = first_bad {
                    break 'admit Err(Refusal::refused(format!(
                        "proposal chunk {chunk_i} is not signed by {proposer:?}, the proposer of {height}.{round}",
                    )));
                }
            }
            // The certificate is what makes a fetched decision decided. Its proposal signatures are
            // kept only if they verify as a fresh proposal's.
            let proposal_sigs = if does(KEEP_ONLY_FRESH_PROPOSAL_SIGS) {
                fresh_proposal_sigs(block, fat_pointer, proposal_sigs.to_vec(), &voting, self.params.active_roster_max_n)
            } else {
                proposal_sigs.to_vec()
            };
            Ok((block_hash, voting, proposal_sigs))
        };
        let (block_hash, voted, proposal_sigs) = match admitted {
            Ok(admitted) => admitted,
            Err(refusal) => {
                if let Some(reply) = reply {
                    let _ = reply.send(Err(refusal.why.clone()));
                }
                return Err(refusal);
            }
        };

        // The `snapshot`: the parent of the deepest carried header, i.e. the block being
        // finalized. See `BftBlock::snapshot_block_hash`. The stakes the next height votes with
        // are the stake there (FINALITY.md §7), from the non-finalized state that holds it, or
        // from the finalized database once `fin` or the depth commit has moved it there. Both are
        // read before the chain lock, so the decision becomes visible in one step.
        let new_final_hash = Hash(block.snapshot_block_hash().0);
        let new_final_height = read_state
            .known_block(new_final_hash)
            .expect("the block rules passed, and they require the snapshot to be known")
            .height;
        let stakes = stakes_at(block_writer, new_final_hash);
        // Read before the lock, still true under it; so is what was checked above.
        let () = ASSUME_SYNC_THREAD_SOLE_WRITER;

        let tip = {
            let mut chain = BFT_CHAIN.write().unwrap();
            apply_decision(
                &mut chain, &self.network, self.slashforks.rules(), block.clone(), block_hash, fat_pointer.clone(),
                voted, stakes, Some((new_final_height, new_final_hash)),
            )
        };
        if source == Source::Stored {
            // Its row is the one it was read from.
            tracing::debug!("took in the stored BFT decision at height {}", block.height);
            let () = ASSUME_TIP_PUBLISHED_AFTER_ROW;
            publish_tip(&tip);
            return Ok(Stored::Decided);
        }
        tracing::info!(
            "Successfully decided BFT block at height {} finalizing {}",
            block.height, tip.final_snapshot.map_or_else(|| "nothing".to_string(), |(_, hash)| hash.to_string()),
        );
        // The decision is stored after the chain lock is dropped, so readers never wait on a
        // disk write. A crash before the row lands leaves the BFT chain one height short and
        // that height is decided again on the next run. A crash after it, while the snapshot is
        // still only in the non-finalized state, leaves a row whose snapshot the next run may
        // not hold; `restore` resumes BFT below that row and decides it again.
        match block_writer.finalized_state.db.write_bft_decision(block.height, block, fat_pointer, &proposal_sigs) {
            Ok(()) => publish_tip(&tip),
            // Not published: tenderlink could not read the round back. A later decision publishes
            // a tip above it.
            Err(err) => tracing::error!(
                "could not store BFT decision at height {}: {err}; that height is decided again on the next start",
                block.height,
            ),
        }
        if let Some(reply) = reply {
            let _ = reply.send(Ok(()));
        }
        Ok(Stored::Decided)
    }

    /// Replay the persisted chain into the store, and start tenderlink at its next height if it
    /// holds anything, at startup only. An empty store means no genesis is stored: BFT genesis is
    /// built once the PoW chain is at or above the activation height (see `tick`), which may
    /// already be the case. Each stored decision goes through `store_decision` as
    /// `Source::Stored`; the first whose snapshot this state does not hold stops it, and BFT
    /// resumes there.
    ///
    /// Rosters are recomputed from the bonds at each height's snapshot, never read back as
    /// stored bytes: a roster that disagreed with the stored votes would re-index every one of
    /// them through seats that never voted (FINALITY.md §8.1).
    fn restore(&mut self, read_state: &ReadState, block_writer: &mut WriteBlockWorkerTask) {
        let restores = BFT_RESTORES.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
        RECENCY_STATUS.send_modify(|status| status.bft_restores = restores);
        // The stored chain is taken in one decision at a time, and only its newest blocks are kept
        // (`HeldBlocks`), so it is never all in memory.
        let stored = |height: usize| block_writer.finalized_state.db.bft_decision(u32::try_from(height).ok()?);

        // A tip past the activation height does not mean genesis was stored. Genesis is decided
        // at the start of a tick and the same tick's commit loop can carry the tip past
        // activation, so a node stopped before the next tick has neither; so does one whose
        // genesis row failed to write, or lacks row 0, which hides every later row. Genesis is a
        // pure function of the headers above h1 (`build_bootstrap_genesis`), so the first tick's
        // `bootstrap` derives the same block this node would have stored, and rows above it are
        // rewritten as catch-up decides them again.
        if stored(0).is_none() {
            if let (Some(activation_height), Some((tip, _))) =
                (self.params.bootstrap.activation_height(), read_state.best_tip())
            {
                if tip.0 >= activation_height {
                    tracing::info!(
                        "crosslink restore: tip {} is at or above the BFT activation height {} and no \
                         BFT chain is stored; BFT genesis will be derived from the chain on the first tick",
                        tip.0, activation_height,
                    );
                }
            }
        }

        let db = &block_writer.finalized_state.db;
        // Dropping BFT is permanent for a database (FINALITY.md §4.3).
        if let Some((dropped_height, dropped_hash)) = db.crosslink_bft_dropped() {
            self.load_dropped_chain(read_state, block_writer);
            stop_bft(
                format!(
                    "It was dropped before this node last stopped: following Π_bft needed block \
                     {dropped_hash} at height {}, which conflicts with this node's committed chain.",
                    dropped_height.0,
                ),
                block_writer.finalized_state.db.path(),
            );
            return;
        }

        // Every stored decision comes in through `store_decision`, oldest first, as any other
        // decision does. It stops at the first one whose snapshot this state lacks: the next
        // height's roster is the stake there. `feed_stored` takes it and the rest in later.
        self.stored_from = Some(0);
        self.feed_stored(read_state, block_writer);
        let decided_n = decided_len();
        if self.stored_from.is_some() {
            // A decision's row is written before its snapshot is durable: the snapshot can still
            // be only in the non-finalized state, which a crash, a lost backup or a disabled one
            // does not bring back. The decision is still final; it is taken in once PoW sync has
            // the snapshot again, or decided from peers.
            let snapshot = u32::try_from(decided_n).ok()
                .and_then(|height| block_writer.finalized_state.db.bft_decision(height))
                .map(|decision| Hash(decision.block.snapshot_block_hash().0));
            if let Some(snapshot) = snapshot {
                tracing::warn!(
                    "crosslink restore: BFT height {decided_n} finalizes {snapshot}, which this state does not hold; \
                     resuming BFT at that height",
                );
            }
        }
        let Some(tip) = BFT_CHAIN.read().unwrap().tip.clone() else { return };
        tracing::info!(
            "crosslink restore: resuming BFT at height {}, finalizing bc height {}",
            decided_n, tip.final_snapshot.map_or(0, |(height, _)| height.0),
        );
        self.start_from_tip(block_writer);
    }

    /// Loads a chain whose BFT was dropped before this node last stopped: nothing is decided from
    /// it, but bc-blocks cite it, so it is loaded whole, checking only that each decision links to
    /// the one before. A snapshot this state no longer holds keeps the last one known.
    fn load_dropped_chain(&mut self, read_state: &ReadState, block_writer: &WriteBlockWorkerTask) {
        let slashforks = Arc::clone(&self.slashforks);
        let slashforks = slashforks.rules();
        let db = &block_writer.finalized_state.db;
        let mut chain = BFT_CHAIN.write().unwrap();
        while let Some(StoredDecision { block, fat_pointer, .. }) = u32::try_from(chain.blocks.len()).ok().and_then(|height| db.bft_decision(height)) {
            let () = ASSUME_REPLAY_ROWS_ARE_THIS_NODES;
            if !links_to_tip(&chain, &block) || block.headers.is_empty() {
                break;
            }
            let snapshot = Hash(block.snapshot_block_hash().0);
            let final_snapshot = read_state.known_block(snapshot).map(|known| (known.height, snapshot));
            let stakes = stakes_at(block_writer, snapshot);
            // Voted on by the roster the chain holds for its height, as when it was decided.
            let voted = next_voting(&chain, &self.network, slashforks);
            let block_hash = block.blake3_hash();
            let tip = apply_decision(&mut chain, &self.network, slashforks, block, block_hash, fat_pointer, voted, stakes, final_snapshot);
            let () = ASSUME_TIP_PUBLISHED_AFTER_ROW;
            publish_tip(&tip);
        }
    }

    /// Starts tenderlink at the height after the decided tip held in memory, with the newest
    /// decided rounds that can be read back from the store. False, starting nothing, when there is
    /// no tip or the tip's own round cannot be read back (its row failed to write): a later
    /// decision publishes a tip that can be.
    fn start_from_tip(&mut self, block_writer: &WriteBlockWorkerTask) -> bool {
        let started = {
            let chain = BFT_CHAIN.read().unwrap();
            let Some(tip) = chain.tip.clone() else { return false };
            let db = &block_writer.finalized_state.db;
            (tip.clone(), startup_rounds(db, &chain.voting_rosters, tip.height + 1, self.params.active_roster_max_n))
        };
        let (tip, ingest) = started;
        // Tenderlink starts at the height after its newest round, so the tip's own must be read.
        if ingest.last().map(|round| round.height) != Some(tip.height) {
            tracing::warn!(
                "crosslink: the decided BFT round at the tip, height {}, cannot be read back from the store; \
                 tenderlink starts once a later height is decided",
                tip.height,
            );
            return false;
        }
        self.spawn_tenderlink(tip.next_roster.clone(), ingest, tip.next_voting.vote_namespace, tip.next_voting.strict_quorum);
        true
    }

    /// The deterministic BFT genesis block: the decision that finalizes the bootstrap roster
    /// height (h1), which every node constructs identically from its own PoW chain instead of
    /// receiving.
    ///
    /// It is shaped exactly as a proposer would shape it -- v2, the sigma confirmation headers
    /// above h1, any slashforks scheduled at BFT height 0 -- so `validate` accepts it unchanged.
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
        let scheduled_slashforks: Vec<SlashForkConfig> = self
            .slashforks
            .rules()
            .iter()
            .filter(|hf| hf.bft_certificate_height == 0)
            .cloned()
            .collect();
        let do_not_include = scheduled_slashforks.last().map_or(0, |last| last.pow_activation_height);
        let block = match BftBlock::new(params, BFT_BLOCK_VERSION_LATEST, 0, FatPointerToBftBlock::null(), headers, scheduled_slashforks, do_not_include) {
            Ok(block) => block,
            Err(e) => {
                tracing::error!("Unable to build the bootstrap genesis BFT block: {:?}", e);
                return None;
            }
        };
        let fat_pointer = FatPointerToBftBlock::from_parts(block.blake3_hash(), 0, 0, &[]);
        Some((block, fat_pointer))
    }

    /// Decide genesis (finalizing h1 on the PoW side and taking h1's aggregated stakes as the
    /// roster for height 1) and start tenderlink at height 1.
    fn bootstrap(&mut self, read_state: &ReadState, block_writer: &mut WriteBlockWorkerTask) {
        let BftBootstrap::FromChain { roster_height, activation_height, .. } = self.params.bootstrap else {
            return;
        };
        // Genesis is decided exactly once. `apply_decision` asserts the block it is given validates
        // against the chain, and a genesis already at index 0 fails that assert -- which under
        // `panic = abort` takes the node down.
        // A genesis below the held blocks was decided and stored, so it is a real block.
        let genesis_decided = {
            let chain = BFT_CHAIN.read().unwrap();
            chain.blocks.base() > 0 || chain.blocks.get(0).is_some_and(|b| !b.headers.is_empty())
        };
        if genesis_decided {
            return;
        }
        // A parked genesis is decided already and waits only for its snapshot. With BFT dropped
        // nothing is decided, so trying every tick would only log the refusal every tick.
        if self.parked_decision.is_some() || bft_is_dropped() {
            return;
        }
        let Some((genesis, fat_pointer)) = self.build_bootstrap_genesis(read_state) else {
            return;
        };
        tracing::info!(
            "crosslink bootstrap: PoW reached height {}; deciding BFT genesis {} which finalizes height {}",
            activation_height, genesis.blake3_hash(), roster_height,
        );
        match self.store_decision(Source::Tenderlink, &genesis, &fat_pointer, &[], None, read_state, block_writer) {
            Ok(_) => {
                self.pending_bootstrap = Some(PendingBootstrap { genesis, fat_pointer });
                self.finish_bootstrap(block_writer);
            }
            Err(refusal) => tracing::error!("crosslink bootstrap: genesis could not be finalized: {}", refusal.why),
        }
    }

    /// Start tenderlink once the bootstrap genesis decision has committed. Genesis finalizes a
    /// block every chain at the activation height holds, so this normally succeeds on the tick
    /// that decided it.
    fn finish_bootstrap(&mut self, block_writer: &mut WriteBlockWorkerTask) {
        if self.pending_bootstrap.is_none() {
            return;
        }
        // Stored once the chain has a tip; a parked genesis waits for its snapshot.
        let (tip, genesis_voting) = {
            let chain = BFT_CHAIN.read().unwrap();
            let Some(tip) = chain.tip.clone() else { return };
            // Who decided genesis, as stored with it: the roster `next_voting` gives before
            // genesis, computed there only.
            (tip, Arc::clone(chain.voting_rosters.first().expect("a decided tip means genesis is stored")))
        };
        let PendingBootstrap { genesis, fat_pointer } = self.pending_bootstrap.take().unwrap();
        if tip.height > 0 {
            // More than genesis was decided while it waited: start from the tip as any start does.
            self.start_from_tip(block_writer);
            return;
        }
        let genesis_round = decided_round_data(&genesis, &fat_pointer, genesis_voting.roster.clone(), &genesis_voting, Vec::new(), 0, self.params.active_roster_max_n);
        self.spawn_tenderlink(tip.next_roster.clone(), vec![genesis_round], tip.next_voting.vote_namespace, tip.next_voting.strict_quorum);
    }

    /// Start tenderlink at the height after `ingest` with `roster`, and mark BFT active. Refuses
    /// an empty roster: BFT height 1's roster is fixed by the stakes at h1, so nothing would ever
    /// change. Dropping the launch on that path is what stops the caller retrying: genesis is
    /// already decided and stored, and `store_decision` refuses to store it twice.
    fn spawn_tenderlink(&mut self, roster: Vec<SortedRosterMember>, ingest: Vec<RoundData>, vote_namespace: [u8; 32], strict_quorum: bool) {
        if roster.is_empty() {
            tracing::error!(
                "BFT height {} has an empty roster: no stake was bonded by the bootstrap roster height ({:?}). BFT will not run on this chain.",
                height_after(&ingest), self.params.bootstrap,
            );
            self.launch = None;
            return;
        }
        if bft_is_dropped() {
            self.launch = None;
            return;
        }
        // A node with a WAL is never held back: at every height it passes on the way to the tip,
        // the WAL holds whatever it signed there.
        if self.catching_up && !self.has_wal {
            if !self.spawn_deferred {
                tracing::info!("holding tenderlink back until the decided BFT chain is caught up with peers");
            }
            // `tick` starts it from the decided tip once caught up.
            self.spawn_deferred = true;
            publish_tenderlink_held(true);
            return;
        }
        let Some(launch) = self.launch.take() else { return; };
        publish_tenderlink_held(false);
        tracing::info!("starting tenderlink at BFT height {} with {} finalizer(s)", height_after(&ingest), roster.len());
        BFT_CHAIN.write().unwrap().is_activated = true;

        // The seeds are only addresses to start connections from. A finalizer's BFT key is
        // learned on connect (tenderlink verifies it and rewrites its address map), so the hint
        // here is deliberately nil: the roster comes from the chain, never from this list.
        let (shared_transport, static_keypair, endpoint, seeds) = match launch.network {
            Some(network) => {
                // One key and port per node: tenderlink speaks as the node, at its public IP.
                let ip = addr_string_to_stuff(&launch.public_address).1.ip;
                if !launch.peer_addresses.is_empty() {
                    tracing::warn!("bft_peers is ignored: BFT runs over the node's own connections, seeded from network_initial_peers");
                }
                let endpoint = tenderlink::stp::STPAddress::from(ip, network.port, &network.keypair);
                (Some(network.transport), network.keypair, endpoint, network.seeds)
            }
            None => {
                let (static_keypair, endpoint) = addr_string_to_stuff(&launch.public_address);
                let seeds = launch.peer_addresses.iter().map(|peer| addr_string_to_stuff(peer).1).collect();
                (None, static_keypair, endpoint, seeds)
            }
        };
        let finalizer_peer_addresses: Vec<FinalizerPeerAddress> =
            seeds.into_iter().map(|address| FinalizerPeerAddress { bft_pk: PubKeyID::NIL, address }).collect();
        let wal_records = stored_tenderlink_wal(height_after(&ingest));
        let task = self.rt.spawn(tenderlink::entry_point(
            launch.signing_key,
            shared_transport,
            Some(static_keypair),
            Some(endpoint),
            roster,
            self.params.active_roster_max_n,
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
                let parsed = bft_block_from_canonical_bytes(&block.0);
                Box::pin(async move {
                    let Some(block) = parsed else {
                        tracing::error!("Tenderlink payload is not a canonically encoded BFT block.");
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
                    let height = u64::from(block.height);
                    // Subscribed before the hand-off, so the tip it leads to is not missed.
                    let mut tips = DECIDED_TIP.subscribe();
                    // Failing to send means the sync thread is gone, so no tip will pass this
                    // height; waiting forever lets the runtime's shutdown end the task instead of
                    // the process. A refused or parked decision likewise waits, for the height to
                    // be decided from peers or the snapshot to return.
                    if !send_request(BftRequest::Decided { block, fat_pointer, proposal_sigs }) {
                        return std::future::pending().await;
                    }
                    // The decision's own height when it was stored, or a later tip when the node
                    // had decided past it: either way the published tip passing it is the answer.
                    loop {
                        let reached = tips.borrow_and_update().as_deref().filter(|tip| tip.height >= height).map(host_tip_of);
                        if let Some(tip) = reached {
                            return tip;
                        }
                        if tips.changed().await.is_err() {
                            return std::future::pending().await;
                        }
                    }
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
                note_decided_missing_pow_blocks(bft_state);
                note_conflicting_quorum(bft_state);
                Box::pin(async {})
            })),
            tenderlink::ClosureToFetchDecidedRound(Arc::new({
                let active_roster_max_n = self.params.active_roster_max_n;
                move |height| fetch_decided_round(height, active_roster_max_n)
            })),
            tenderlink::ClosureToWriteWal(Arc::new(move |height, record| {
                let stored = write_tenderlink_wal(height, record);
                Box::pin(async move { stored })
            })),
            tenderlink::ClosureToReadDecidedTip(Arc::new(host_tip)),
            ingest,
            wal_records,
            vote_namespace,
            strict_quorum,
        ));
        *TENDERLINK_TASK.lock().unwrap() = Some(task.abort_handle());
    }
}

fn recency_status_from(bft_state: &TMState, bft_key_address_map: &BftAddressMap) -> TFLRecencyStatus {
    let now_utc = chrono::Utc::now().timestamp();
    let mut finalizer_statuses = Vec::<(PubKeyID, FinalizerRecencyStatus)>::new();

    for round in &bft_state.rounds_data {
        let is_my_height = round.height == bft_state.height;

        // The vote arrays are sized to the *active* roster (the top `active_roster_max_n` by
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
            let cs = ConsensusCounts::from(&(round.msg_val_sigs[roster_i], round.msg_nil_sigs[roster_i], 1, round.proposal_id));
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
        parked_decision_height: *PARKED_DECISION_HEIGHT.lock().unwrap(),
        bft_restores: BFT_RESTORES.load(std::sync::atomic::Ordering::Relaxed),
        tenderlink_held: TENDERLINK_HELD.load(std::sync::atomic::Ordering::Relaxed),
        conflicting_quorums: CONFLICTING_QUORUMS_SEEN.load(std::sync::atomic::Ordering::Relaxed),
        conflicts: CONFLICTING_QUORUMS_REPORTED.lock().unwrap().iter().map(|(conflict, _)| conflict.clone()).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_block(version: u32) -> BftBlock {
        let header = BcBlockHeader {
            version: 4,
            prev_block: BlockHash([1; 32]),
            merkle_root: [2; 32],
            final_sapling_root: [3; 32],
            time: 4,
            bits: 5,
            nonce: [6; 32],
            solution: vec![7; 1344],
            fat_pointer_to_bft_block: FatPointerToBftBlock::null(),
        };
        BftBlock {
            version,
            height: 9,
            previous_block_fat_ptr: FatPointerToBftBlock::null(),
            headers: vec![header; 3],
            slashforks: Vec::new(),
            do_not_include_until_bc_height: if version >= 2 { 11 } else { 0 },
        }
    }

    /// A chain decided through `apply_decision`, each height voted on by the roster `next_voting`
    /// gives, follows the roster rules. Restore takes stored decisions in through the same step
    /// (`Source::Stored`), so there is no separate derivation to drift; one once did (a finalizer
    /// jailed a certificate early at a slashfork boundary). The steps cover changed stakes, empty
    /// stakes (the ghost roster carries the last forward), a watermark crossing FeaturenetUpgrade2
    /// (the strict quorum starts), and an unknown snapshot (the last final snapshot stands).
    #[test]
    fn deciding_a_chain_follows_the_roster_rules() {
        let network = Network::new_regtest(zebra_chain::parameters::testnet::RegtestParameters::from(
            zebra_chain::parameters::testnet::ConfiguredActivationHeights { featurenet_upgrade_2: Some(50), ..Default::default() },
        ));
        let slashforks: &[SlashForkConfig] = &[];
        let member = |k: u8, stake: u64| RosterMember { pub_key: [k; 32], voting_power: stake, txids: Vec::new(), finalizer_address: None };
        let final_at = |height: u32| Some((Height(height), Hash([height as u8; 32])));
        let steps = vec![
            (vec![member(1, 10), member(2, 20)], final_at(40)),
            (Vec::new(), final_at(45)),
            (vec![member(2, 20), member(3, 30), member(4, 5)], final_at(50)),
            (vec![member(3, 30)], None),
            (vec![member(3, 30), member(5, 50)], final_at(55)),
        ];
        let mut chain = BftChain::new();
        let mut previous = FatPointerToBftBlock::null();
        for (height, (stakes, final_snapshot)) in steps.into_iter().enumerate() {
            let mut block = test_block(2);
            block.height = height as u32;
            block.previous_block_fat_ptr = previous;
            let block_hash = block.blake3_hash();
            let fat_pointer = FatPointerToBftBlock::from_parts(block_hash, height as u64, 0, &[]);
            previous = fat_pointer.clone();
            let voted = next_voting(&chain, &network, slashforks);
            apply_decision(&mut chain, &network, slashforks, block, block_hash, fat_pointer, voted, stakes, final_snapshot);
        }

        assert_eq!(chain.voting_rosters.len(), 5);
        let tip = chain.tip.clone().expect("five decisions");
        let keys = |voting: &VotingRoster| voting.roster.iter().map(|m| m.pub_key.0[0]).collect::<Vec<_>>();
        assert!(keys(&chain.voting_rosters[0]).is_empty(), "genesis is decided by the nil roster");
        let mut ghost = keys(&chain.voting_rosters[2]);
        ghost.sort();
        assert_eq!(ghost, vec![1, 2], "empty stakes at height 1's snapshot carry height 0's forward");
        assert!(!chain.voting_rosters[2].strict_quorum, "watermark 45: before FeaturenetUpgrade2");
        assert!(chain.voting_rosters[3].strict_quorum, "watermark 50: FeaturenetUpgrade2");
        assert!(chain.voting_rosters[4].strict_quorum, "an unknown snapshot keeps watermark 50");
        assert_eq!(tip.final_snapshot, final_at(55));
        assert!(*tip.next_voting == *next_voting(&chain, &network, slashforks), "the tip holds the next height's roster");
    }

    /// While decided-chain PoW blocks stay missing, the wait is reported every
    /// `DROPPED_BFT_REPORT_INTERVAL` with how long it has lasted, and a wait that ends starts the
    /// clock again.
    #[test]
    fn a_wait_for_decided_pow_blocks_is_reported_every_interval() {
        let start = std::time::Instant::now();
        let at = |secs: u64| start + std::time::Duration::from_secs(secs);
        let interval = DROPPED_BFT_REPORT_INTERVAL.as_secs();
        let mut waiting = None;
        assert_eq!(decided_missing_report(&mut waiting, 0, at(0)), None, "nothing missing");
        assert_eq!(decided_missing_report(&mut waiting, 2, at(0)), None, "the start of a wait is reported elsewhere");
        assert_eq!(decided_missing_report(&mut waiting, 2, at(interval - 1)), None);
        assert_eq!(decided_missing_report(&mut waiting, 1, at(interval)), Some(std::time::Duration::from_secs(interval)));
        assert_eq!(decided_missing_report(&mut waiting, 1, at(interval + 10)), None, "reported once per interval");
        assert_eq!(decided_missing_report(&mut waiting, 3, at(2 * interval)), Some(std::time::Duration::from_secs(2 * interval)));
        assert_eq!(decided_missing_report(&mut waiting, 0, at(2 * interval + 1)), None, "the wait ended");
        assert_eq!(decided_missing_report(&mut waiting, 1, at(3 * interval)), None, "a new wait starts its own clock");
        assert_eq!(decided_missing_report(&mut waiting, 1, at(4 * interval)), Some(std::time::Duration::from_secs(interval)));
    }

    #[test]
    fn held_blocks_keep_the_newest_and_the_heights_of_the_whole_chain() {
        let mut blocks = HeldBlocks::new();
        assert!(blocks.is_empty() && blocks.last().is_none() && blocks.get(0).is_none());
        let decided_n = BFT_BLOCKS_HELD + 3;
        for height in 0..decided_n {
            // The version stands in for the height, to tell the blocks apart.
            blocks.push(test_block(height as u32));
            assert_eq!(blocks.len(), height + 1);
            assert_eq!(blocks.last().map(|block| block.version), Some(height as u32));
        }
        assert_eq!(blocks.base(), 3);
        assert_eq!(blocks.held().count(), BFT_BLOCKS_HELD);
        assert!(blocks.get(0).is_none() && blocks.get(2).is_none(), "below the held blocks");
        assert!(blocks.get(decided_n).is_none(), "past the tip");
        for height in [3, 4, decided_n - 1] {
            assert_eq!(blocks.get(height).map(|block| block.version), Some(height as u32));
        }
        assert!(blocks.held().map(|(height, block)| (height as u32, block.version)).all(|(height, version)| height == version));
        assert_eq!(blocks.held().next_back().map(|(height, _)| height), Some(decided_n - 1));
    }

    #[test]
    fn only_the_canonical_encoding_of_a_bft_block_is_accepted() {
        for version in [1, 2] {
            let block = test_block(version);
            let bytes = block.zcash_serialize_to_vec().unwrap();
            assert_eq!(bft_block_from_canonical_bytes(&bytes), Some(block.clone()));

            let mut trailing = bytes.clone();
            trailing.push(0);
            assert_eq!(BftBlock::zcash_deserialize(&trailing[..]).unwrap(), block);
            assert_eq!(bft_block_from_canonical_bytes(&trailing), None);

            assert_eq!(bft_block_from_canonical_bytes(&bytes[..bytes.len() - 1]), None);
        }
    }

    fn hash(n: u8) -> Hash {
        Hash([n; 32])
    }

    /// Four finalizers of equal stake, in roster order, with their keys.
    fn four_finalizers(strict_quorum: bool, vote_namespace: [u8; 32]) -> (Vec<ed25519_zebra::SigningKey>, VotingRoster) {
        let keys: Vec<ed25519_zebra::SigningKey> = (0..4).map(|i| ed25519_zebra::SigningKey::from([i + 1; 32])).collect();
        let stakes: Vec<RosterMember> = keys
            .iter()
            .map(|key| RosterMember {
                pub_key: ed25519_zebra::VerificationKeyBytes::from(key).into(),
                voting_power: 10,
                ..Default::default()
            })
            .collect();
        let roster = tenderlink_roster_from_internal(&stakes, &HashSet::new());
        let keys = roster
            .iter()
            .map(|member| keys.iter().find(|key| <[u8; 32]>::from(ed25519_zebra::VerificationKeyBytes::from(*key)) == member.pub_key.0).unwrap().clone())
            .collect();
        (keys, VotingRoster { roster, vote_namespace, strict_quorum })
    }

    /// A certificate over the 44-byte `template`, signed by `signers` (indices into `keys`, in the
    /// order given) under `namespace`.
    fn certificate(template: [u8; 44], keys: &[ed25519_zebra::SigningKey], signers: &[usize], namespace: &[u8; 32]) -> FatPointerToBftBlock {
        let signatures = signers
            .iter()
            .map(|&i| {
                let pub_key = PubKeyID(ed25519_zebra::VerificationKeyBytes::from(&keys[i]).into());
                let mut signed = pub_key.0.to_vec();
                signed.extend_from_slice(&template);
                if *namespace != [0; 32] {
                    signed.extend_from_slice(namespace);
                }
                zcash_primitives::bft::FatPointerSignature { pub_key, vote_signature: <[u8; 64]>::from(keys[i].sign(&signed)) }
            })
            .collect();
        FatPointerToBftBlock { vote_for_block_without_finalizer_public_key: template, signatures }
    }

    fn template(height: u64, round_bits: u32) -> [u8; 44] {
        let mut template = [7u8; 44];
        template[32..40].copy_from_slice(&height.to_le_bytes());
        template[40..44].copy_from_slice(&round_bits.to_le_bytes());
        template
    }

    /// A restored decision gets back the valid round its proposal signatures were made in, which
    /// tenderlink signs every re-served chunk over.
    #[test]
    fn proposal_valid_round_is_read_back_from_the_signatures() {
        let (keys, voting) = four_finalizers(false, [0; 32]);
        let block = test_block(2);
        let height = u64::from(block.height);
        let value = block.zcash_serialize_to_vec().unwrap();
        let fat_pointer = certificate(template(height, 2 | 0x8000_0000), &keys, &[0, 1, 2], &voting.vote_namespace);
        let value_id = tenderlink::ValueId(fat_pointer.points_at_block_hash().0);
        let proposer = tenderlink::proposer_index(&HashKeys::default(), &voting.roster, 4, height, 2).unwrap();
        let sign = |key: &ed25519_zebra::SigningKey, valid_round: i64| {
            tenderlink::proposal_chunk_sigs(key, &value, value_id, height, 2, valid_round, &voting.vote_namespace)
        };

        for valid_round in [-1, 0, 1] {
            let sigs = sign(&keys[proposer], valid_round);
            assert_eq!(proposal_valid_round(&block, &fat_pointer, &sigs, &voting, 4), Some(valid_round));
        }

        // Another finalizer's signatures verify under no valid round.
        let other = (proposer + 1) % keys.len();
        assert_eq!(proposal_valid_round(&block, &fat_pointer, &sign(&keys[other], 1), &voting, 4), None);

        // Round 0 has no earlier round to have locked a value in.
        let round_0 = certificate(template(height, 0x8000_0000), &keys, &[0, 1, 2], &voting.vote_namespace);
        assert_eq!(proposal_valid_round(&block, &round_0, &sign(&keys[proposer], -1), &voting, 4), Some(-1));

        let round_data = decided_round_data(&block, &fat_pointer, voting.roster.clone(), &voting, sign(&keys[proposer], 1), height, 4);
        assert_eq!(round_data.proposal_valid_round, 1);
    }

    /// A fetched decision keeps its proposal signatures only if they verify as a fresh proposal's,
    /// with one check whatever round it names: never a search over valid rounds.
    #[test]
    fn fetched_proposal_sigs_are_kept_only_as_a_fresh_proposals() {
        let (keys, voting) = four_finalizers(false, [0; 32]);
        let block = test_block(2);
        let height = u64::from(block.height);
        let value = block.zcash_serialize_to_vec().unwrap();
        let sign = |fat_pointer: &FatPointerToBftBlock, round: u32, valid_round: i64| {
            let value_id = tenderlink::ValueId(fat_pointer.points_at_block_hash().0);
            let proposer = tenderlink::proposer_index(&HashKeys::default(), &voting.roster, 4, height, round).unwrap();
            tenderlink::proposal_chunk_sigs(&keys[proposer], &value, value_id, height, round, valid_round, &voting.vote_namespace)
        };

        let round_2 = certificate(template(height, 2 | 0x8000_0000), &keys, &[0, 1, 2], &voting.vote_namespace);
        let fresh = sign(&round_2, 2, -1);
        assert_eq!(fresh_proposal_sigs(&block, &round_2, fresh.clone(), &voting, 4), fresh);
        // A re-proposal's signatures would need the valid round: dropped, not searched for.
        assert!(fresh_proposal_sigs(&block, &round_2, sign(&round_2, 2, 1), &voting, 4).is_empty());
        // A wrong count never reaches tenderlink's chunk check.
        assert!(fresh_proposal_sigs(&block, &round_2, fresh[..fresh.len() - 1].to_vec(), &voting, 4).is_empty());

        // The largest round a peer can name costs the same one check.
        let huge = certificate(template(height, 0x7fff_ffff | 0x8000_0000), &keys, &[0, 1, 2], &voting.vote_namespace);
        let started = std::time::Instant::now();
        assert!(fresh_proposal_sigs(&block, &huge, fresh, &voting, 4).is_empty());
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    /// Every way a certificate can fail, each alone on an otherwise valid one, at a height that
    /// decides by the strict quorum (three of four) and at one that decides by 2f+1 (also three
    /// of four here, so the stake case differs only in namespace).
    #[test]
    fn certificate_is_valid_refuses_each_defect_alone() {
        let namespace = namespace_for_bft_height(&[], 5, true);
        let (keys, voting) = four_finalizers(true, namespace);
        let good = template(5, 2 | 0x8000_0000);
        assert!(certificate_is_valid(&certificate(good, &keys, &[0, 1, 2], &namespace), 5, &voting));
        assert!(certificate_is_valid(&certificate(good, &keys, &[0, 1, 2, 3], &namespace), 5, &voting));

        let stranger = ed25519_zebra::SigningKey::from([9; 32]);
        let mut with_stranger = certificate(good, &keys, &[0, 1, 2], &namespace);
        with_stranger.signatures.push(certificate(good, &[stranger], &[0], &namespace).signatures[0].clone());
        let mut bad_signature = certificate(good, &keys, &[0, 1, 2], &namespace);
        bad_signature.signatures[1].vote_signature[0] ^= 1;
        let mut other_round = certificate(good, &keys, &[0, 1, 2], &namespace);
        other_round.vote_for_block_without_finalizer_public_key[40] ^= 1;

        let cases: [(&str, FatPointerToBftBlock); 10] = [
            ("no signatures", certificate(good, &keys, &[], &namespace)),
            ("a signer outside the roster", with_stranger),
            ("a duplicate signer", certificate(good, &keys, &[0, 1, 1, 2], &namespace)),
            ("signers out of roster order", certificate(good, &keys, &[2, 1, 0], &namespace)),
            ("stake below the quorum", certificate(good, &keys, &[0, 1], &namespace)),
            ("the wrong height", certificate(template(6, 2 | 0x8000_0000), &keys, &[0, 1, 2], &namespace)),
            ("the commit bit clear", certificate(template(5, 2), &keys, &[0, 1, 2], &namespace)),
            ("a bad signature", bad_signature),
            ("a round other than the one signed", other_round),
            ("the old namespace", certificate(good, &keys, &[0, 1, 2], &[0; 32])),
        ];
        for (defect, fat_pointer) in cases {
            assert!(!certificate_is_valid(&fat_pointer, 5, &voting), "accepted a certificate with {defect}");
        }
    }

    /// 2f+1 and the strict quorum differ at seven of twelve, the deployed shape.
    #[test]
    fn certificate_is_valid_counts_by_the_heights_quorum() {
        let keys: Vec<ed25519_zebra::SigningKey> = (0..12).map(|i| ed25519_zebra::SigningKey::from([i + 1; 32])).collect();
        let stakes: Vec<RosterMember> = keys
            .iter()
            .map(|key| RosterMember { pub_key: ed25519_zebra::VerificationKeyBytes::from(key).into(), voting_power: 1, ..Default::default() })
            .collect();
        let roster = tenderlink_roster_from_internal(&stakes, &HashSet::new());
        let keys: Vec<ed25519_zebra::SigningKey> = roster
            .iter()
            .map(|member| keys.iter().find(|key| <[u8; 32]>::from(ed25519_zebra::VerificationKeyBytes::from(*key)) == member.pub_key.0).unwrap().clone())
            .collect();
        let good = template(3, 0x8000_0000);
        for (strict_quorum, signers, valid) in [(false, 7, true), (true, 7, false), (true, 8, false), (true, 9, true)] {
            let namespace = namespace_for_bft_height(&[], 3, strict_quorum);
            let voting = VotingRoster { roster: roster.clone(), vote_namespace: namespace, strict_quorum };
            let signers: Vec<usize> = (0..signers).collect();
            assert_eq!(
                certificate_is_valid(&certificate(good, &keys, &signers, &namespace), 3, &voting),
                valid,
                "{} of 12 under the {} quorum", signers.len(), if strict_quorum { "strict" } else { "2f+1" },
            );
        }
    }

    /// The nil roster decides only genesis: no signatures, round 0, commit bit set.
    #[test]
    fn certificate_is_valid_takes_only_an_empty_round_0_certificate_from_the_nil_roster() {
        let nil = VotingRoster { roster: Vec::new(), vote_namespace: [0; 32], strict_quorum: false };
        let genesis = FatPointerToBftBlock::from_parts(Blake3Hash([7; 32]), 0, 0, &[]);
        assert!(certificate_is_valid(&genesis, 0, &nil));
        assert!(!certificate_is_valid(&FatPointerToBftBlock::from_parts(Blake3Hash([7; 32]), 0, 1, &[]), 0, &nil));
        let (keys, _) = four_finalizers(false, [0; 32]);
        assert!(!certificate_is_valid(&certificate(template(0, 0x8000_0000), &keys, &[0], &[0; 32]), 0, &nil));
    }

    /// The rule switches on the height of the roster's snapshot, and with it the namespace: nil
    /// below activation with no slashforks, the branch ID's from it on, and different from the
    /// slashforks' own namespace when there are some.
    #[test]
    fn strict_quorum_and_its_namespace_start_at_the_roster_snapshot_on_featurenet_upgrade_2() {
        let network = Network::new_regtest(zebra_chain::parameters::testnet::RegtestParameters::from(
            zebra_chain::parameters::testnet::ConfiguredActivationHeights { featurenet_upgrade_2: Some(50), ..Default::default() },
        ));
        assert!(!strict_quorum_at(&network, None));
        assert!(!strict_quorum_at(&network, Some(Height(49))));
        assert!(strict_quorum_at(&network, Some(Height(50))));
        let default = Network::new_regtest(Default::default());
        let default_height =
            zcash_protocol::consensus::Parameters::activation_height(&default, NetworkUpgrade::FeaturenetUpgrade2)
                .expect("Regtest schedules FeaturenetUpgrade2 by default");
        assert!(!strict_quorum_at(&default, Some(Height(u32::from(default_height) - 1))));
        assert!(strict_quorum_at(&default, Some(Height(u32::from(default_height)))));

        assert_eq!(namespace_for_bft_height(&[], 7, false), [0; 32]);
        assert_ne!(namespace_for_bft_height(&[], 7, true), [0; 32]);
        let (_, below) = roster_for_bft_height(&network, &[], &[], 7, Some(Height(49)));
        let (_, at) = roster_for_bft_height(&network, &[], &[], 7, Some(Height(50)));
        assert_eq!((below.strict_quorum, below.vote_namespace), (false, [0; 32]));
        assert_eq!((at.strict_quorum, at.vote_namespace), (true, namespace_for_bft_height(&[], 7, true)));
    }

    #[test]
    fn missing_pow_blocks_expire_unless_asked_for_again_and_stay_under_the_cap() {
        let start = std::time::Instant::now();
        let mut missing = HashMap::new();
        note_missing_pow_block(&mut missing, hash(1), start);
        note_missing_pow_block(&mut missing, hash(2), start);

        // Hash 1 is asked for again just before the window closes; hash 2 is not.
        let refreshed = start + MISSING_POW_BLOCK_FRESHNESS - std::time::Duration::from_millis(1);
        note_missing_pow_block(&mut missing, hash(1), refreshed);
        retain_missing_pow_blocks(&mut missing, start + MISSING_POW_BLOCK_FRESHNESS, |_| false);
        assert_eq!(missing.keys().copied().collect::<Vec<_>>(), vec![hash(1)]);

        // A known block leaves at once, fresh or not.
        retain_missing_pow_blocks(&mut missing, refreshed, |known| known == hash(1));
        assert!(missing.is_empty());

        // Past the cap the least recently asked-for goes, never the hash just asked for, even
        // when every entry was asked for at the same instant.
        for n in 0..=u8::try_from(MAX_MISSING_POW_BLOCKS).expect("the cap fits in a byte") {
            note_missing_pow_block(&mut missing, hash(n), start + std::time::Duration::from_secs(u64::from(n)));
        }
        assert_eq!(missing.len(), MAX_MISSING_POW_BLOCKS);
        assert!(!missing.contains_key(&hash(0)));
        let mut same_instant = HashMap::new();
        for n in 0..100 {
            note_missing_pow_block(&mut same_instant, hash(n), start);
            assert!(same_instant.contains_key(&hash(n)));
            assert!(same_instant.len() <= MAX_MISSING_POW_BLOCKS);
        }
    }

    fn time(seconds: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(seconds, 0).expect("test times are in range")
    }

    fn header(previous_block_hash: Hash, seconds: i64, difficulty_threshold: CompactDifficulty) -> Header {
        Header {
            version: 5,
            previous_block_hash,
            merkle_root: zebra_chain::block::merkle::Root([0; 32]),
            commitment_bytes: zebra_chain::fmt::HexDebug([0; 32]),
            time: time(seconds),
            difficulty_threshold,
            nonce: zebra_chain::fmt::HexDebug([0; 32]),
            solution: zebra_chain::work::equihash::Solution::for_proposal(),
            fat_pointer_to_bft_block: FatPointerToBftBlock::null(),
        }
    }

    /// A block id that names its branch and height, so a context can be read back as positions.
    fn block_id(branch: u8, height: u32) -> Hash {
        let mut bytes = [0; 32];
        bytes[0] = branch;
        bytes[1..5].copy_from_slice(&height.to_le_bytes());
        Hash(bytes)
    }

    /// Each block's time encodes its branch and height, and its nBits its height alone.
    fn branch_time(branch: u8, height: u32) -> i64 {
        1_000_000 + i64::from(height) * 1_000 + i64::from(branch)
    }

    #[test]
    fn difficulty_context_follows_the_snapshot_across_a_side_chain() {
        // Branch 1 is the finalized prefix and best chain, heights 0 to 40. Branch 2 is a side
        // chain forking above height 30, heights 31 to 35.
        let mut held: HashMap<Hash, Arc<Header>> = HashMap::new();
        let mut add = |branch: u8, height: u32, parent: Hash| {
            let bits = CompactDifficulty(0x1f00_0000 + height);
            held.insert(block_id(branch, height), Arc::new(header(parent, branch_time(branch, height), bits)));
        };
        add(1, 0, zebra_chain::parameters::GENESIS_PREVIOUS_BLOCK_HASH);
        for height in 1..=40 {
            add(1, height, block_id(1, height - 1));
        }
        add(2, 31, block_id(1, 30));
        for height in 32..=35 {
            add(2, height, block_id(2, height - 1));
        }
        let entry = |branch: u8, height: u32| (CompactDifficulty(0x1f00_0000 + height), time(branch_time(branch, height)));

        // A full node validating the block above side-chain block 35 reads that block and its 27
        // ancestors on its own chain: 35 to 31 on the side chain, then 30 down to 8 below the fork.
        let context = difficulty_context_above(block_id(2, 35), Height(35), |hash| held.get(&hash).cloned());
        let expected: Vec<_> = (31..=35).rev().map(|h| entry(2, h)).chain((8..=30).rev().map(|h| entry(1, h))).collect();
        assert_eq!(context.len(), POW_ADJUSTMENT_BLOCK_SPAN);
        assert_eq!(context.into_iter().collect::<Vec<_>>(), expected);

        // Near genesis the context is shorter and ends at genesis, without asking for its parent.
        let mut asked = Vec::new();
        let context = difficulty_context_above(block_id(1, 5), Height(5), |hash| {
            asked.push(hash);
            held.get(&hash).cloned()
        });
        assert_eq!(context.into_iter().collect::<Vec<_>>(), (0..=5).rev().map(|h| entry(1, h)).collect::<Vec<_>>());
        assert_eq!(asked.last(), Some(&block_id(1, 0)));
    }

    #[test]
    fn carried_headers_must_carry_the_difficulty_their_position_requires() {
        use zebra_chain::work::difficulty::ParameterDifficulty as _;

        // Default Testnet far below its minimum-difficulty start height, so ThresholdBits
        // applies with no exemption. The history runs at the target spacing with nBits well
        // below PoWLimit.
        let network = Network::new_default_testnet();
        let snapshot_height = Height(1_000);
        let spacing = zebra_chain::parameters::NetworkUpgrade::target_spacing_for_height(&network, snapshot_height).num_seconds();
        let pow_limit = network.target_difficulty_limit().to_compact();
        let history_bits = (network.target_difficulty_limit() / 1_000u64).to_compact();
        let snapshot_time = 2_000_000_000;
        let context: VecDeque<_> = (0..POW_ADJUSTMENT_BLOCK_SPAN)
            .map(|depth| (history_bits, time(snapshot_time - spacing * i64::try_from(depth).expect("depth is below 28"))))
            .collect();

        let required = |context: &VecDeque<(CompactDifficulty, DateTime<Utc>)>, previous: Height, seconds: i64| {
            AdjustedDifficulty::new_from_header_time(time(seconds), previous, &network, context.iter().copied())
                .expected_difficulty_threshold()
        };
        let first_time = snapshot_time + spacing;
        let first_bits = required(&context, snapshot_height, first_time);
        assert_ne!(first_bits, pow_limit, "the scenario needs a required difficulty above PoWLimit");
        let first = header(block_id(1, 1_000), first_time, first_bits);
        let mut after_first = context.clone();
        after_first.push_front((first_bits, first.time));
        let second_time = first_time + spacing;
        let second_bits = required(&after_first, Height(1_001), second_time);
        let second = header(first.hash(), second_time, second_bits);

        let tail = |headers: &[&Header]| headers.iter().map(|h| bc_hdr_to_lrz(h)).collect::<Vec<_>>();
        let check = |headers: &[&Header]| headers_meet_required_difficulty(&network, snapshot_height, context.clone(), &tail(headers));

        assert_eq!(check(&[&first, &second]), Ok(()));

        // The same positions declaring PoWLimit, as a forged tail does.
        let forged_first = header(block_id(1, 1_000), first_time, pow_limit);
        let err = check(&[&forged_first]).expect_err("a PoWLimit header where the chain requires more is refused");
        assert!(err.starts_with("carried header 0 at height 1001"), "{err}");
        let forged_second = header(first.hash(), second_time, pow_limit);
        let err = check(&[&first, &forged_second]).expect_err("the second header is checked against a context holding the first");
        assert!(err.starts_with("carried header 1 at height 1002"), "{err}");

        // A time at or below the median-time-past is refused whatever it declares.
        let early_time = snapshot_time - spacing * 20;
        let early = header(block_id(1, 1_000), early_time, required(&context, snapshot_height, early_time));
        assert!(check(&[&early]).is_err());
    }

    /// A carried header declaring PoWLimit more than six spacings after the snapshot, above the
    /// minimum-difficulty start height: a legitimate minimum-difficulty header on stock Testnet
    /// parameters with FeaturenetUpgrade2 moved above it, refused on parameters that switch the
    /// exemption off and once FeaturenetUpgrade2 is active, as it is on stock Testnet itself.
    #[test]
    fn a_minimum_difficulty_carried_header_passes_only_where_the_exemption_holds() {
        use zebra_chain::parameters::testnet::{self, ConfiguredActivationHeights};
        use zebra_chain::work::difficulty::ParameterDifficulty as _;

        let stock = Network::new_default_testnet();
        let Network::Testnet(stock_params) = &stock else {
            unreachable!("the default testnet is a Testnet");
        };
        let mut heights = ConfiguredActivationHeights::from(stock_params.activation_heights());
        let last_scheduled = stock_params.activation_heights().keys().last().expect("Testnet has upgrades").0;
        let snapshot_height = Height(last_scheduled.max(400_000));
        // FeaturenetUpgrade3 to FeaturenetUpgrade5 must follow FeaturenetUpgrade2, so they are
        // unscheduled here.
        heights.featurenet_upgrade_2 = Some(snapshot_height.0 + 2);
        heights.featurenet_upgrade_3 = None;
        heights.featurenet_upgrade_4 = None;
        heights.featurenet_upgrade_5 = None;
        let exempt = testnet::Parameters::build()
            .with_activation_heights(heights)
            .expect("valid activation heights")
            .to_network()
            .expect("valid parameters");
        heights.featurenet_upgrade_2 = Some(snapshot_height.0);
        let upgraded = testnet::Parameters::build()
            .with_activation_heights(heights)
            .expect("valid activation heights")
            .to_network()
            .expect("valid parameters");
        let featurenet = testnet::Parameters::build()
            .with_minimum_difficulty_start_height(None)
            .to_network()
            .expect("valid parameters");

        let check = |network: &Network| {
            let spacing = zebra_chain::parameters::NetworkUpgrade::target_spacing_for_height(network, snapshot_height).num_seconds();
            let history_bits = (network.target_difficulty_limit() / 1_000u64).to_compact();
            let snapshot_time = 2_000_000_000;
            let context: VecDeque<_> = (0..POW_ADJUSTMENT_BLOCK_SPAN)
                .map(|depth| (history_bits, time(snapshot_time - spacing * i64::try_from(depth).expect("depth is below 28"))))
                .collect();
            let gapped = header(block_id(1, snapshot_height.0), snapshot_time + spacing * 6 + 1, network.target_difficulty_limit().to_compact());
            headers_meet_required_difficulty(network, snapshot_height, context, &[bc_hdr_to_lrz(&gapped)])
        };

        assert_eq!(check(&exempt), Ok(()));
        for network in [&stock, &featurenet, &upgraded] {
            let err = check(network).expect_err("no minimum-difficulty exemption applies");
            assert!(err.contains("is not equal to the expected difficulty"), "{err}");
        }
    }
}
