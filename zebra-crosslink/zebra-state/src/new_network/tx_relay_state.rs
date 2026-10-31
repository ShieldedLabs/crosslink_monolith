//! Transaction relay's bookkeeping: what we list, what we want and from whom, what we serve.
//!
//! Pure: every call takes the time, and returns what to do ([`Out`]) instead of doing it, so the
//! bounds below can be tested without sockets or a mempool. The sync loop carries the [`Out`]s
//! to the transport and to the mempool bridge, and feeds the answers back in.
//!
//! Relay is pull-based (no pushes, so nothing marks a transaction's origin). The bounds come
//! from the relay's security reviews:
//! - what we ask a peer for is budgeted by bytes ([`MAX_REQUESTED_BYTES_PER_PEER`]) and count
//!   ([`MAX_IN_FLIGHT_PER_PEER`], counting what awaits the mempool's verdict), each id at the
//!   size *that* peer listed, so a peer lying about sizes only ever affects its own asks; asks
//!   refill every poll;
//! - one peer can make at most [`MAX_SOLE_LISTED_PER_PEER`] ids wanted that nobody else lists,
//!   and its statuses are rate limited, so it cannot fill [`MAX_WANTED`] and crowd out everyone
//!   else's transactions; ids nobody lists any more are forgotten;
//! - a wanted transaction keeps up to [`MAX_LISTERS`] listers, and is asked of the others when
//!   the ask goes unanswered for [`REREQUEST_AFTER`]; once none are left it is forgotten (honest
//!   holders list it again within a heartbeat). Not delivering is no fault (a transaction can be
//!   mined or evicted meanwhile, and STP loses packets): it only demotes the peer, which is then
//!   asked last and loses its place to a better lister (unless the body was dropped on our side,
//!   for a full decode queue). A peer is only ever asked for, and only
//!   ever demoted over, ids it listed itself; the same id is not asked of the same peer again
//!   within [`ASK_COOLDOWN`] (meanwhile the peer does not count as listing it);
//! - strikes back a peer off (it is not asked), 1 s doubling to 64 s, forgotten after
//!   [`STRIKES_FORGOTTEN_AFTER`] without one. A strike is only for what no honest peer does
//!   whatever our state: a body larger than the size *it* listed, or a transaction that fails
//!   verification for a state-independent reason. Malformed bytes are the transport's to kill
//!   for, never ours;
//! - what we serve is limited by a byte bucket per peer, reserved when a request is accepted,
//!   requests themselves are rate limited, and an id is not served to the same peer again
//!   within [`SERVE_REPEAT_SUPPRESS`].

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use tenderlink::stp::ConnectionKey;
use zebra_chain::transaction::UnminedTxId;

use super::tx_relay::{tx_req_packet, tx_status_packet, MAX_TXS_PER_MESSAGE, MAX_TX_IDS_PER_STATUS};

pub(super) const MAX_REQUESTED_BYTES_PER_PEER: u64 = 2 << 20;
/// Asked of one peer and not delivered, plus delivered and awaiting the mempool's verdict. At
/// most the mempool's own cap per STP peer (zebrad's `DEFAULT_MAX_INBOUND_CONCURRENCY_PER_STP_PEER`),
/// so one peer's deliveries are not refused as a full queue.
pub(super) const MAX_IN_FLIGHT_PER_PEER: usize = 32;
pub(super) const MAX_LISTERS: usize = 8;
pub(super) const REREQUEST_AFTER: Duration = Duration::from_secs(4);
/// A wanted transaction nobody delivers is forgotten after this; a later listing starts over.
pub(super) const WANTED_GIVE_UP: Duration = Duration::from_secs(60);
/// Wanted ids one peer may be the only lister of.
pub(super) const MAX_SOLE_LISTED_PER_PEER: usize = 512;
/// Wanted transactions held at most (about 200 bytes each): twice what the inbound connection
/// cap times [`MAX_SOLE_LISTED_PER_PEER`] can fill with ids only one peer lists.
pub(super) const MAX_WANTED: usize = 65536;
/// The same id is not asked of the same peer again within this, whatever became of the ask.
pub(super) const ASK_COOLDOWN: Duration = Duration::from_secs(60);
/// Ids queued to ask one peer at most (a whole mempool's listing); past it the oldest go.
const MAX_TO_ASK_PER_PEER: usize = 8192;
/// Recent asks remembered per peer for [`ASK_COOLDOWN`] at most; past it the oldest go.
const MAX_ASKED_REMEMBERED: usize = 4096;
/// After [`Outcome::Busy`] the delivering peer may be asked for the id again after this.
const BUSY_RETRY_AFTER: Duration = REREQUEST_AFTER;
pub(super) const STATUS_HEARTBEAT: Duration = Duration::from_secs(30);
pub(super) const STATUS_MIN_GAP: Duration = Duration::from_millis(250);
/// A change waits 100..500 ms before it is listed, so a burst goes out as one status.
const STATUS_BATCH_MIN: Duration = Duration::from_millis(100);
const STATUS_BATCH_SPREAD_MS: u64 = 400;
/// Statuses heard from one peer: a burst big enough for a whole `MEMPOOL_REQ` answer, then
/// as often as an honest peer sends them.
const STATUSES_IN_BURST: u64 = 40;
const STATUSES_IN_PER_SEC: u64 = 4;
/// `TX_REQ`s heard from one peer.
const TX_REQS_IN_BURST: u64 = 16;
const TX_REQS_IN_PER_SEC: u64 = 4;
pub(super) const MEMPOOL_REQ_ANSWER_INTERVAL: Duration = Duration::from_secs(600);
pub(super) const SERVE_BURST_BYTES: u64 = 2 << 20;
pub(super) const SERVE_BYTES_PER_SEC: u64 = 256 << 10;
pub(super) const SERVE_REPEAT_SUPPRESS: Duration = Duration::from_secs(10);
/// Ids remembered as served to one peer at most; past it the oldest are forgotten.
const MAX_SERVED_REMEMBERED: usize = 4096;
const STRIKE_BACKOFF_MIN: Duration = Duration::from_secs(1);
const STRIKE_BACKOFF_MAX: Duration = Duration::from_secs(64);
pub(super) const STRIKES_FORGOTTEN_AFTER: Duration = Duration::from_secs(600);
/// A peer's count of undelivered asks halves this often.
const DELIVERY_FAILURES_HALVE_EVERY: Duration = Duration::from_secs(300);

/// What the sync loop does for the relay.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Out {
    /// Send on the peer's control channel (`bulk: false`) or bulk channel.
    Send { to: ConnectionKey, packet: Vec<u8>, bulk: bool },
    /// Send this `TX_REQ` for `ids`; if it cannot be sent now, tell [`TxRelay::on_ask_unsent`].
    Ask { to: ConnectionKey, ids: Vec<UnminedTxId>, packet: Vec<u8> },
    /// Send a `MEMPOOL_REQ`; if it cannot be sent now, tell [`TxRelay::on_mempool_req_unsent`].
    MempoolReq { to: ConnectionKey },
    /// Ask the mempool which of `entries` it neither holds, nor has rejected, nor is verifying;
    /// answer with [`TxRelay::on_filtered`]. May be dropped if the bridge is busy.
    Filter { from: ConnectionKey, entries: Vec<(UnminedTxId, u32)> },
    /// Fetch these from the mempool and send them; their bytes are already reserved.
    Fetch { to: ConnectionKey, ids: Vec<UnminedTxId> },
}

/// What became of a transaction a peer delivered, as the mempool judged it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Outcome {
    /// Accepted, or already known: it will show up in (or is already in) our listing.
    Accepted,
    /// Refused for a reason that may be ours (our tip, mempool contents, a timeout, the
    /// mempool disabled): no fault of the peer.
    NotTaken,
    /// Not looked at: the mempool's queue was full. It stays wanted and is asked for again
    /// shortly, of the same peer or another.
    Busy,
    /// Fails verification whatever our state (a bad proof or signature): the peer is struck.
    Invalid,
}

/// A token bucket, in thousandths of a token.
#[derive(Debug)]
struct Bucket {
    milli: u64,
    at: Instant,
}

impl Bucket {
    fn full(burst: u64, now: Instant) -> Self {
        Self { milli: burst * 1000, at: now }
    }
    fn refill(&mut self, now: Instant, per_sec: u64, burst: u64) {
        let ms = now.saturating_duration_since(self.at).as_millis() as u64;
        // per_sec tokens a second is per_sec thousandths a millisecond.
        self.milli = self.milli.saturating_add(ms.saturating_mul(per_sec)).min(burst * 1000);
        self.at = now;
    }
    fn take(&mut self, cost: u64, now: Instant, per_sec: u64, burst: u64) -> bool {
        self.refill(now, per_sec, burst);
        if self.milli < cost * 1000 {
            return false;
        }
        self.milli -= cost * 1000;
        true
    }
    #[cfg(test)]
    fn tokens(&self) -> u64 {
        self.milli / 1000
    }
}

#[derive(Debug)]
struct Wanted {
    /// The peers that listed it, each with the size it listed.
    listers: Vec<(ConnectionKey, u32)>,
    /// The lister that made it wanted, while it is the only one (counted against its cap).
    sole: Option<ConnectionKey>,
    /// When we asked each peer we are waiting on.
    asked: Vec<(ConnectionKey, Instant)>,
    /// A lister with no failed deliveries to ask at once, alongside doubtful outstanding asks.
    jump: Option<ConnectionKey>,
    first_seen: Instant,
    /// Delivered and handed to the mempool; waiting for its verdict.
    submitted: bool,
}

impl Wanted {
    fn listed_size(&self, peer: ConnectionKey) -> Option<u32> {
        self.listers.iter().find(|(p, _)| *p == peer).map(|(_, size)| *size)
    }
}

#[derive(Debug, Default)]
struct Strikes {
    count: u32,
    last: Option<Instant>,
    until: Option<Instant>,
}

impl Strikes {
    fn strike(&mut self, now: Instant) {
        if self.last.is_some_and(|last| now.saturating_duration_since(last) >= STRIKES_FORGOTTEN_AFTER) {
            self.count = 0;
        }
        self.count = self.count.saturating_add(1);
        self.last = Some(now);
        let backoff = STRIKE_BACKOFF_MIN.saturating_mul(1 << (self.count - 1).min(6)).min(STRIKE_BACKOFF_MAX);
        self.until = Some(now + backoff);
    }
    fn backing_off(&self, now: Instant) -> bool {
        self.until.is_some_and(|until| now < until)
    }
}

/// Undelivered asks, halving every [`DELIVERY_FAILURES_HALVE_EVERY`]: orders whom to ask.
#[derive(Debug)]
struct DeliveryFailures {
    count: u32,
    halved_at: Instant,
}

impl DeliveryFailures {
    fn current(&mut self, now: Instant) -> u32 {
        let halvings = now.saturating_duration_since(self.halved_at).as_secs() / DELIVERY_FAILURES_HALVE_EVERY.as_secs();
        if halvings > 0 {
            self.count >>= halvings.min(31);
            self.halved_at = now;
        }
        self.count
    }
}

#[derive(Debug)]
struct PeerTx {
    /// Ids asked of this peer and not yet delivered, with the size it listed.
    requested: HashMap<UnminedTxId, u32>,
    requested_bytes: u64,
    /// Delivered by this peer and awaiting the mempool's verdict.
    submitted: usize,
    /// Wanted ids this peer is a lister of.
    listing: HashSet<UnminedTxId>,
    /// When a body from this peer was last dropped on our side (the bridge was full): asks made
    /// before it that go unanswered are no fault of the peer.
    local_drop: Option<Instant>,
    /// Wanted ids this peer listed and has not been asked for, oldest first. Stale entries
    /// (resolved, or asked of someone else) are skipped when popped.
    to_ask: VecDeque<UnminedTxId>,
    /// When we last asked it for each id, oldest first, for [`ASK_COOLDOWN`].
    asked_at: HashMap<UnminedTxId, Instant>,
    asked_order: VecDeque<UnminedTxId>,
    /// Wanted ids only this peer lists.
    sole_listed: usize,
    statuses_in: Bucket,
    tx_reqs_in: Bucket,
    /// The listing generation last sent, and when; and when the next is due after a change.
    sent_generation: u64,
    last_status: Option<Instant>,
    status_due: Option<Instant>,
    serve: Bucket,
    /// What we served it, and when, oldest first.
    served: HashMap<UnminedTxId, Instant>,
    served_order: VecDeque<UnminedTxId>,
    strikes: Strikes,
    delivery_failures: DeliveryFailures,
    mempool_req_sent: bool,
    mempool_req_answered: Option<Instant>,
}

impl PeerTx {
    fn new(now: Instant) -> Self {
        Self {
            requested: HashMap::new(),
            requested_bytes: 0,
            submitted: 0,
            listing: HashSet::new(),
            local_drop: None,
            to_ask: VecDeque::new(),
            asked_at: HashMap::new(),
            asked_order: VecDeque::new(),
            sole_listed: 0,
            statuses_in: Bucket::full(STATUSES_IN_BURST, now),
            tx_reqs_in: Bucket::full(TX_REQS_IN_BURST, now),
            sent_generation: 0,
            last_status: None,
            status_due: None,
            serve: Bucket::full(SERVE_BURST_BYTES, now),
            served: HashMap::new(),
            served_order: VecDeque::new(),
            strikes: Strikes::default(),
            delivery_failures: DeliveryFailures { count: 0, halved_at: now },
            mempool_req_sent: false,
            mempool_req_answered: None,
        }
    }

    fn queue_ask(&mut self, id: UnminedTxId) {
        if self.to_ask.len() >= MAX_TO_ASK_PER_PEER {
            self.to_ask.pop_front();
        }
        self.to_ask.push_back(id);
    }

    fn cooling_down(&self, id: &UnminedTxId, now: Instant) -> bool {
        self.asked_at.get(id).is_some_and(|at| now.saturating_duration_since(*at) < ASK_COOLDOWN)
    }

    fn note_asked(&mut self, id: UnminedTxId, now: Instant) {
        if self.asked_at.insert(id, now).is_none() {
            self.asked_order.push_back(id);
            if self.asked_order.len() > MAX_ASKED_REMEMBERED {
                if let Some(oldest) = self.asked_order.pop_front() {
                    self.asked_at.remove(&oldest);
                }
            }
        }
    }

    fn unrequest(&mut self, id: &UnminedTxId) {
        if let Some(size) = self.requested.remove(id) {
            self.requested_bytes -= u64::from(size);
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct TxRelay {
    /// Our mempool's ids and serialized sizes (kept from its change stream), and the order they
    /// arrived in, newest last, for listing the most recent ones.
    mirror: HashMap<UnminedTxId, u32>,
    arrival: VecDeque<UnminedTxId>,
    generation: u64,
    wanted: HashMap<UnminedTxId, Wanted>,
    /// Wanted ids in the order they became wanted, to give up on them in order.
    wanted_order: VecDeque<(Instant, UnminedTxId)>,
    /// Asks in the order they were made, to find the overdue ones in order. Stale entries
    /// (delivered, or re-asked) are skipped when popped.
    asks: VecDeque<(Instant, UnminedTxId)>,
    peers: HashMap<ConnectionKey, PeerTx>,
}

/// 100..500 ms, spread by peer and generation so peers do not all hear a change at once.
fn batch_delay(peer: ConnectionKey, generation: u64) -> Duration {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    peer.hash(&mut hasher);
    generation.hash(&mut hasher);
    STATUS_BATCH_MIN + Duration::from_millis(hasher.finish() % STATUS_BATCH_SPREAD_MS)
}

impl TxRelay {
    // ---- our mempool ----

    /// Transactions entered our mempool, with their serialized sizes. Ones we wanted are no
    /// longer wanted.
    pub fn on_added(&mut self, entries: impl IntoIterator<Item = (UnminedTxId, u32)>) {
        for (id, size) in entries {
            if self.mirror.insert(id, size).is_none() {
                self.arrival.push_back(id);
                self.generation += 1;
            }
            self.forget_wanted(id);
        }
    }

    /// Transactions left our mempool (mined, invalidated, evicted).
    pub fn on_removed(&mut self, ids: impl IntoIterator<Item = UnminedTxId>) {
        for id in ids {
            if self.mirror.remove(&id).is_some() {
                self.generation += 1;
            }
        }
        // Arrival order is pruned lazily: entries no longer in the mirror are skipped and
        // dropped once the queue is twice the mirror's size.
        if self.arrival.len() > 2 * self.mirror.len() + 64 {
            let mirror = &self.mirror;
            self.arrival.retain(|id| mirror.contains_key(id));
        }
    }

    /// Our mempool, all of it (after the change stream lagged): replaces the mirror.
    pub fn on_reset(&mut self, entries: impl IntoIterator<Item = (UnminedTxId, u32)>) {
        self.mirror.clear();
        self.arrival.clear();
        self.generation += 1;
        self.on_added(entries);
    }

    // ---- what peers list ----

    /// A peer's `TX_STATUS`, if its rate allows. Ids we neither hold nor want go to the mempool
    /// to drop what it holds, rejected or is verifying; ids we already want gain the peer as a
    /// lister. A peer backing off is still heard, but not asked.
    pub fn on_status(&mut self, from: ConnectionKey, entries: Vec<(UnminedTxId, u32)>, now: Instant) -> Vec<Out> {
        let peer = self.peers.entry(from).or_insert_with(|| PeerTx::new(now));
        if !peer.statuses_in.take(1, now, STATUSES_IN_PER_SEC, STATUSES_IN_BURST) {
            return Vec::new();
        }
        let mut new = Vec::new();
        for (id, size) in entries {
            if self.mirror.contains_key(&id) {
                continue;
            }
            if self.wanted.contains_key(&id) {
                self.add_lister(id, from, size, now);
            } else {
                new.push((id, size));
            }
        }
        let mut out = Vec::new();
        if !new.is_empty() {
            out.push(Out::Filter { from, entries: new });
        }
        out.extend(self.ask(from, false, now));
        out
    }

    /// `from` lists `id` at `size`, and we want it. A lister with no failed deliveries displaces
    /// the worst one when the list is full, and is asked at once if every outstanding ask is to
    /// a peer that has failed to deliver before.
    fn add_lister(&mut self, id: UnminedTxId, from: ConnectionKey, size: u32, now: Instant) {
        let Some(wanted) = self.wanted.get_mut(&id) else { return };
        if self.peers.get(&from).is_some_and(|peer| peer.cooling_down(&id, now)) {
            return;
        }
        if wanted.listed_size(from).is_some() {
            // Listed again while nobody is asked for it (an ask ended without a delivery):
            // ask this lister.
            if wanted.asked.is_empty() && !wanted.submitted {
                if let Some(peer) = self.peers.get_mut(&from) {
                    peer.queue_ask(id);
                }
            }
            return;
        }
        let peers = &mut self.peers;
        let mut failures = |p: &ConnectionKey| peers.get_mut(p).map_or(u32::MAX, |peer| peer.delivery_failures.current(now));
        let newcomer_clean = failures(&from) == 0;
        let mut displaced = None;
        if wanted.listers.len() >= MAX_LISTERS {
            if !newcomer_clean {
                return;
            }
            // Displace the lister with the most failed deliveries, unless it is being waited on.
            let mut worst: Option<(usize, u32)> = None;
            for (i, (p, _)) in wanted.listers.iter().enumerate() {
                let f = failures(p);
                if f > 0 && !wanted.asked.iter().any(|(asked, _)| asked == p) && worst.is_none_or(|(_, w)| f > w) {
                    worst = Some((i, f));
                }
            }
            let Some((i, _)) = worst else { return };
            displaced = Some(wanted.listers.swap_remove(i).0);
        }
        let jump = newcomer_clean && !wanted.asked.is_empty() && wanted.asked.iter().all(|(p, _)| failures(p) > 0);
        if let Some(peer) = displaced.and_then(|displaced| self.peers.get_mut(&displaced)) {
            peer.listing.remove(&id);
        }
        wanted.listers.push((from, size));
        let sole = wanted.sole.take();
        if jump {
            wanted.jump = Some(from);
        }
        if let Some(peer) = sole.and_then(|sole| self.peers.get_mut(&sole)) {
            peer.sole_listed = peer.sole_listed.saturating_sub(1);
        }
        let peer = self.peers.get_mut(&from).expect("made by on_status");
        peer.listing.insert(id);
        if jump {
            peer.to_ask.push_front(id);
        } else {
            peer.queue_ask(id);
        }
    }

    /// The mempool's answer to [`Out::Filter`]: `keep` are the entries it neither holds, nor
    /// has rejected, nor is verifying. They become wanted, listed by `from`, within its cap.
    pub fn on_filtered(&mut self, from: ConnectionKey, keep: Vec<(UnminedTxId, u32)>, now: Instant) -> Vec<Out> {
        if !self.peers.contains_key(&from) {
            return Vec::new();
        }
        for (id, size) in keep {
            if self.mirror.contains_key(&id) {
                continue;
            }
            if self.wanted.contains_key(&id) {
                self.add_lister(id, from, size, now);
                continue;
            }
            let peer = self.peers.get_mut(&from).expect("checked above");
            if self.wanted.len() >= MAX_WANTED || peer.sole_listed >= MAX_SOLE_LISTED_PER_PEER || peer.cooling_down(&id, now) {
                continue;
            }
            peer.sole_listed += 1;
            peer.listing.insert(id);
            peer.queue_ask(id);
            self.wanted_order.push_back((now, id));
            self.wanted.insert(id, Wanted { listers: vec![(from, size)], sole: Some(from), asked: Vec::new(), jump: None, first_seen: now, submitted: false });
        }
        self.ask(from, false, now)
    }

    /// Ask `peer` for what it listed, within its budgets and at the sizes it listed. Unless
    /// `re_request`, an id already asked of another peer waits (it comes back if that ask goes
    /// unanswered). Only the peer's own queue is walked, and only as far as it asks.
    fn ask(&mut self, peer_key: ConnectionKey, re_request: bool, now: Instant) -> Vec<Out> {
        let Some(peer) = self.peers.get_mut(&peer_key) else { return Vec::new() };
        if peer.strikes.backing_off(now) {
            return Vec::new();
        }
        let mut ids = Vec::new();
        while peer.requested.len() + peer.submitted < MAX_IN_FLIGHT_PER_PEER && ids.len() < MAX_TXS_PER_MESSAGE {
            let Some(id) = peer.to_ask.pop_front() else { break };
            let Some(wanted) = self.wanted.get_mut(&id) else { continue };
            let Some(size) = wanted.listed_size(peer_key) else { continue };
            if wanted.submitted || wanted.asked.iter().any(|(p, _)| *p == peer_key) || peer.cooling_down(&id, now) {
                continue;
            }
            let jumped = wanted.jump == Some(peer_key);
            if !re_request && !jumped && !wanted.asked.is_empty() {
                continue;
            }
            if peer.requested_bytes + u64::from(size) > MAX_REQUESTED_BYTES_PER_PEER {
                peer.to_ask.push_front(id);
                break;
            }
            if jumped {
                wanted.jump = None;
            }
            wanted.asked.push((peer_key, now));
            self.asks.push_back((now, id));
            peer.requested.insert(id, size);
            peer.requested_bytes += u64::from(size);
            peer.note_asked(id, now);
            ids.push(id);
        }
        if ids.is_empty() {
            return Vec::new();
        }
        let packet = tx_req_packet(&ids);
        vec![Out::Ask { to: peer_key, ids, packet }]
    }

    /// An [`Out::Ask`] that could not be sent: the asks are undone, and the ids go back to the
    /// front of the peer's queue (the cooldown does not apply to what never left).
    pub fn on_ask_unsent(&mut self, to: ConnectionKey, ids: &[UnminedTxId]) {
        let Some(peer) = self.peers.get_mut(&to) else { return };
        for id in ids.iter().rev() {
            peer.unrequest(id);
            peer.asked_at.remove(id);
            if let Some(wanted) = self.wanted.get_mut(id) {
                wanted.asked.retain(|(p, _)| *p != to);
                peer.to_ask.push_front(*id);
            }
        }
    }

    /// Forget a wanted id: free the budget its asks hold, and its sole lister's count.
    fn forget_wanted(&mut self, id: UnminedTxId) {
        let Some(wanted) = self.wanted.remove(&id) else { return };
        for (peer_key, _) in wanted.asked {
            if let Some(peer) = self.peers.get_mut(&peer_key) {
                peer.unrequest(&id);
            }
        }
        for (peer_key, _) in wanted.listers {
            if let Some(peer) = self.peers.get_mut(&peer_key) {
                peer.listing.remove(&id);
            }
        }
        if let Some(sole) = wanted.sole {
            if let Some(peer) = self.peers.get_mut(&sole) {
                peer.sole_listed = peer.sole_listed.saturating_sub(1);
            }
        }
    }

    /// A wanted id nobody is asked for or verifying goes back to its listers' queues, or is
    /// forgotten if none are left. Called where an ask or a verification ends without it.
    fn requeue_or_forget(&mut self, id: UnminedTxId) {
        let Some(wanted) = self.wanted.get(&id) else { return };
        if !wanted.asked.is_empty() || wanted.submitted {
            return;
        }
        if wanted.listers.is_empty() {
            self.forget_wanted(id);
            return;
        }
        for (lister, _) in &wanted.listers {
            if let Some(peer) = self.peers.get_mut(lister) {
                peer.to_ask.push_front(id);
            }
        }
    }

    /// A `TX` body from `from` was dropped on our side (the bridge's queue was full), so the
    /// asks it answered are not held against `from`.
    pub fn on_body_dropped(&mut self, from: ConnectionKey, now: Instant) {
        if let Some(peer) = self.peers.get_mut(&from) {
            peer.local_drop = Some(now);
        }
    }

    // ---- what peers deliver ----

    /// Of the transactions `from` sent, with their serialized lengths, those we asked it for,
    /// which the caller then hands to the mempool. One larger than the size *it* listed is a lie
    /// no honest peer tells: it is struck and the body dropped. The rest were not asked of it (a
    /// late answer after a re-request) and are dropped.
    pub fn on_delivered(&mut self, from: ConnectionKey, delivered: &[(UnminedTxId, u32)], now: Instant) -> Vec<UnminedTxId> {
        // What this returns is marked submitted until its outcome arrives.
        let () = super::tx_relay_bridge::ASSUME_ONE_OUTCOME_PER_SUBMITTED;
        let Some(peer) = self.peers.get_mut(&from) else { return Vec::new() };
        let mut take = Vec::new();
        let mut lied = Vec::new();
        for (id, len) in delivered {
            let Some(listed) = peer.requested.get(id).copied() else { continue };
            peer.unrequest(id);
            let Some(wanted) = self.wanted.get_mut(id) else { continue };
            wanted.asked.retain(|(p, _)| *p != from);
            if *len > listed {
                wanted.listers.retain(|(p, _)| *p != from);
                peer.listing.remove(id);
                lied.push(*id);
                continue;
            }
            if !wanted.submitted {
                wanted.submitted = true;
                take.push(*id);
            }
        }
        peer.submitted += take.len();
        if !lied.is_empty() {
            peer.strikes.strike(now);
        }
        for id in lied {
            self.requeue_or_forget(id);
        }
        take
    }

    /// The mempool's verdict on a transaction `from` delivered.
    pub fn on_outcome(&mut self, from: ConnectionKey, id: UnminedTxId, outcome: Outcome, now: Instant) {
        if let Some(peer) = self.peers.get_mut(&from) {
            peer.submitted = peer.submitted.saturating_sub(1);
        }
        // A verdict is about the entry that was submitted. One given up on while it was verified
        // (WANTED_GIVE_UP) may be wanted afresh, and asked for, by the time its verdict lands:
        // that entry's listers and asks are not this verdict's to change.
        let submitted = self.wanted.get(&id).is_some_and(|wanted| wanted.submitted);
        match outcome {
            // It reaches the mirror through the change stream, which forgets it as wanted.
            Outcome::Accepted => {}
            Outcome::NotTaken => {
                if submitted {
                    self.forget_wanted(id);
                }
            }
            Outcome::Busy => {
                let Some(wanted) = self.wanted.get_mut(&id).filter(|_| submitted) else { return };
                wanted.submitted = false;
                // The other listers are asked now; `from` again after BUSY_RETRY_AFTER, when it
                // lists it again (meanwhile it is not a lister).
                wanted.listers.retain(|(p, _)| *p != from);
                if let Some(peer) = self.peers.get_mut(&from) {
                    peer.listing.remove(&id);
                    let retry_at = now.checked_sub(ASK_COOLDOWN.saturating_sub(BUSY_RETRY_AFTER)).unwrap_or(now);
                    if let Some(at) = peer.asked_at.get_mut(&id) {
                        *at = retry_at;
                    }
                }
                self.requeue_or_forget(id);
            }
            Outcome::Invalid => {
                let () = super::tx_relay_bridge::ASSUME_INVALID_IS_STATE_INDEPENDENT;
                self.forget_wanted(id);
                if let Some(peer) = self.peers.get_mut(&from) {
                    peer.strikes.strike(now);
                }
            }
        }
    }

    // ---- what peers ask of us ----

    /// A peer's `TX_REQ`, if its rate allows: of the ids we hold and have not just served it,
    /// as many as its serving bucket covers are fetched to be sent, their bytes reserved now.
    pub fn on_request(&mut self, from: ConnectionKey, ids: Vec<UnminedTxId>, now: Instant) -> Vec<Out> {
        let peer = self.peers.entry(from).or_insert_with(|| PeerTx::new(now));
        if !peer.tx_reqs_in.take(1, now, TX_REQS_IN_PER_SEC, TX_REQS_IN_BURST) {
            return Vec::new();
        }
        peer.serve.refill(now, SERVE_BYTES_PER_SEC, SERVE_BURST_BYTES);
        let mut fetch = Vec::new();
        for id in ids {
            let Some(size) = self.mirror.get(&id) else { continue };
            if peer.served.get(&id).is_some_and(|at| now.saturating_duration_since(*at) < SERVE_REPEAT_SUPPRESS) {
                continue;
            }
            if !peer.serve.take(u64::from(*size), now, SERVE_BYTES_PER_SEC, SERVE_BURST_BYTES) {
                break;
            }
            if peer.served.insert(id, now).is_none() {
                peer.served_order.push_back(id);
                if peer.served_order.len() > MAX_SERVED_REMEMBERED {
                    if let Some(oldest) = peer.served_order.pop_front() {
                        peer.served.remove(&oldest);
                    }
                }
            }
            fetch.push(id);
        }
        if fetch.is_empty() {
            return Vec::new();
        }
        vec![Out::Fetch { to: from, ids: fetch }]
    }

    /// A peer's `MEMPOOL_REQ`: our whole listing, at most once per
    /// [`MEMPOOL_REQ_ANSWER_INTERVAL`] per peer, on the bulk channel.
    pub fn on_mempool_req(&mut self, from: ConnectionKey, now: Instant) -> Vec<Out> {
        let peer = self.peers.entry(from).or_insert_with(|| PeerTx::new(now));
        if peer.mempool_req_answered.is_some_and(|at| now.saturating_duration_since(at) < MEMPOOL_REQ_ANSWER_INTERVAL) {
            return Vec::new();
        }
        peer.mempool_req_answered = Some(now);
        let total = self.mirror.len() as u32;
        let listing: Vec<(&UnminedTxId, u32)> = self.arrival.iter().rev().filter_map(|id| self.mirror.get(id).map(|size| (id, *size))).collect();
        listing
            .chunks(MAX_TX_IDS_PER_STATUS)
            .map(|chunk| Out::Send { to: from, packet: tx_status_packet(total, chunk.iter().copied()), bulk: true })
            .collect()
    }

    /// An [`Out::MempoolReq`] that could not be sent: it is sent again on a later poll.
    pub fn on_mempool_req_unsent(&mut self, to: ConnectionKey) {
        if let Some(peer) = self.peers.get_mut(&to) {
            peer.mempool_req_sent = false;
        }
    }

    // ---- peers coming and going, and time passing ----

    /// A peer disconnected. What only it listed, and nobody is asked for, is forgotten, so
    /// connection churn cannot leave ownerless entries filling the wanted set.
    pub fn on_peer_gone(&mut self, peer: ConnectionKey) {
        // Only listers are asked, and the sole and jump peers are listers, so the peer's own
        // listing covers every wanted entry that names it.
        let Some(gone) = self.peers.remove(&peer) else { return };
        for id in gone.listing {
            let Some(wanted) = self.wanted.get_mut(&id) else { continue };
            wanted.listers.retain(|(p, _)| *p != peer);
            wanted.asked.retain(|(p, _)| *p != peer);
            if wanted.sole == Some(peer) {
                wanted.sole = None;
            }
            if wanted.jump == Some(peer) {
                wanted.jump = None;
            }
            self.requeue_or_forget(id);
        }
    }

    /// Statuses due, asks refilled, re-requests of what was not delivered, giving up on what
    /// nobody delivers, and a `MEMPOOL_REQ` to each peer once per session. `live` are the
    /// connected peers.
    pub fn poll(&mut self, live: &[ConnectionKey], now: Instant) -> Vec<Out> {
        // Per-peer state is made for any key seen and freed only by `on_peer_gone`.
        let () = super::tx_relay_bridge::ASSUME_PEER_GONE_FOR_EVERY_KEY;
        let mut out = Vec::new();
        for key in live {
            self.peers.entry(*key).or_insert_with(|| PeerTx::new(now));
        }

        // Give up, oldest first, on what nobody delivered in time.
        while let Some(&(since, id)) = self.wanted_order.front() {
            if now.saturating_duration_since(since) < WANTED_GIVE_UP {
                break;
            }
            self.wanted_order.pop_front();
            if self.wanted.get(&id).is_some_and(|w| w.first_seen == since) {
                self.forget_wanted(id);
            }
        }
        // Entries for ids already resolved are dropped once they outnumber the live ones.
        if self.wanted_order.len() > 2 * self.wanted.len() + 1024 {
            let wanted = &self.wanted;
            self.wanted_order.retain(|(since, id)| wanted.get(id).is_some_and(|w| w.first_seen == *since));
        }
        if self.asks.len() > 2 * self.wanted.len() + 1024 {
            let wanted = &self.wanted;
            self.asks.retain(|(at, id)| wanted.get(id).is_some_and(|w| w.asked.iter().any(|(_, asked_at)| asked_at == at)));
        }

        // Overdue asks, oldest first. The peers asked (each a lister of it) are demoted and lose
        // their place as listers, and the other listers are asked; with none left the id is
        // forgotten, to be wanted again when an honest holder lists it.
        let mut re_ask: HashSet<ConnectionKey> = HashSet::new();
        let mut forget = Vec::new();
        while let Some(&(at, id)) = self.asks.front() {
            if now.saturating_duration_since(at) < REREQUEST_AFTER {
                break;
            }
            self.asks.pop_front();
            let Some(wanted) = self.wanted.get_mut(&id) else { continue };
            if wanted.submitted || !wanted.asked.iter().any(|(_, asked_at)| *asked_at == at) {
                continue;
            }
            if wanted.asked.iter().any(|(_, asked_at)| now.saturating_duration_since(*asked_at) < REREQUEST_AFTER) {
                continue; // a more recent ask is still within its time
            }
            let overdue: Vec<(ConnectionKey, Instant)> = wanted.asked.drain(..).collect();
            for (peer_key, asked_at) in &overdue {
                let Some(peer) = self.peers.get_mut(peer_key) else { continue };
                peer.unrequest(&id);
                // A body of theirs we dropped since the ask may have held it: no fault, and
                // they stay a lister (asked again once their cooldown passes).
                if peer.local_drop.is_some_and(|dropped| dropped >= *asked_at) {
                    peer.asked_at.remove(&id);
                    continue;
                }
                peer.delivery_failures.current(now);
                peer.delivery_failures.count = peer.delivery_failures.count.saturating_add(1);
                peer.listing.remove(&id);
                wanted.listers.retain(|(p, _)| p != peer_key);
            }
            if wanted.listers.is_empty() {
                forget.push(id);
                continue;
            }
            for (lister, _) in &wanted.listers {
                if let Some(peer) = self.peers.get_mut(lister) {
                    peer.to_ask.push_front(id);
                    re_ask.insert(*lister);
                }
            }
        }
        for id in forget {
            self.forget_wanted(id);
        }
        for peer_key in re_ask {
            out.extend(self.ask(peer_key, true, now));
        }

        // Asks refill as deliveries free budget, peers with fewer failed deliveries first.
        let mut order: Vec<(u32, ConnectionKey)> = live
            .iter()
            .filter_map(|key| self.peers.get_mut(key).map(|peer| (peer.delivery_failures.current(now), *key)))
            .collect();
        order.sort();
        for (_, key) in order {
            out.extend(self.ask(key, false, now));
        }

        let total = self.mirror.len() as u32;
        let generation = self.generation;
        // Built once, and only if some peer is due one: the same listing goes to every peer.
        let mut status: Option<Vec<u8>> = None;
        let (mirror, arrival) = (&self.mirror, &self.arrival);
        for key in live {
            let peer = self.peers.get_mut(key).expect("added above");
            if !peer.mempool_req_sent {
                peer.mempool_req_sent = true;
                out.push(Out::MempoolReq { to: *key });
            }
            let changed = peer.sent_generation != generation;
            if changed && peer.status_due.is_none() {
                peer.status_due = Some(now + batch_delay(*key, generation));
            }
            let gap_ok = peer.last_status.is_none_or(|at| now.saturating_duration_since(at) >= STATUS_MIN_GAP);
            let change_due = changed && peer.status_due.is_some_and(|due| now >= due);
            let heartbeat_due = peer.last_status.is_none_or(|at| now.saturating_duration_since(at) >= STATUS_HEARTBEAT);
            if gap_ok && (change_due || heartbeat_due) {
                peer.sent_generation = generation;
                peer.last_status = Some(now);
                peer.status_due = None;
                let packet = status.get_or_insert_with(|| {
                    let newest_first = arrival.iter().rev().filter_map(|id| mirror.get(id).map(|size| (id, *size)));
                    tx_status_packet(total, newest_first.take(MAX_TX_IDS_PER_STATUS))
                });
                out.push(Out::Send { to: *key, packet: packet.clone(), bulk: false });
            }
        }
        out
    }

    /// Whether we are waiting on `peer` for transactions: only then is a `TX` from it decoded.
    pub fn has_requested(&self, peer: ConnectionKey) -> bool {
        let () = super::tx_relay_bridge::ASSUME_UNASKED_TX_NOT_DECODED;
        self.peers.get(&peer).is_some_and(|p| !p.requested.is_empty())
    }

    /// The bytes we are waiting on from `peer`: what a `TX` from it may decode to, at most.
    pub fn requested_bytes(&self, peer: ConnectionKey) -> u64 {
        self.peers.get(&peer).map_or(0, |p| p.requested_bytes)
    }

    /// Whether `peer` is backed off for relay (not asked for anything).
    #[cfg(test)]
    pub fn backing_off(&self, peer: ConnectionKey, now: Instant) -> bool {
        self.peers.get(&peer).is_some_and(|p| p.strikes.backing_off(now))
    }

    /// Serving bytes left for `peer` (for tests and diagnostics).
    #[cfg(test)]
    fn serve_tokens(&self, peer: ConnectionKey) -> u64 {
        self.peers.get(&peer).map_or(0, |p| p.serve.tokens())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::new_network::tx_relay::read_tx_status;
    use crate::new_network::PACKET_TYPE_TX_STATUS;
    use zebra_chain::transaction::Hash;

    fn peer(n: u16) -> ConnectionKey {
        ConnectionKey { ip: std::net::Ipv6Addr::LOCALHOST, port: n, key_15_bits: 0 }
    }
    fn id(n: u32) -> UnminedTxId {
        let mut bytes = [0u8; 32];
        bytes[..4].copy_from_slice(&n.to_le_bytes());
        UnminedTxId::Legacy(Hash(bytes))
    }
    /// The ids `outs` ask `of` for.
    fn asked(outs: &[Out], of: ConnectionKey) -> Vec<UnminedTxId> {
        outs.iter()
            .filter_map(|o| match o {
                Out::Ask { to, ids, .. } if *to == of => Some(ids.clone()),
                _ => None,
            })
            .flatten()
            .collect()
    }
    /// A peer lists `entries`, and the mempool keeps them all.
    fn listed(relay: &mut TxRelay, from: ConnectionKey, entries: Vec<(UnminedTxId, u32)>, now: Instant) -> Vec<Out> {
        let mut outs = relay.on_status(from, entries, now);
        let keep = outs.iter().find_map(|o| match o {
            Out::Filter { entries, .. } => Some(entries.clone()),
            _ => None,
        });
        if let Some(keep) = keep {
            outs.extend(relay.on_filtered(from, keep, now));
        }
        outs
    }

    #[test]
    fn asks_are_budgeted_by_bytes_and_count_and_refill_as_deliveries_land() {
        let now = Instant::now();
        let mut relay = TxRelay::default();
        relay.poll(&[peer(1)], now);
        // 100 KB each: 2 MiB holds 20 of them, fewer than the in-flight cap.
        let outs = listed(&mut relay, peer(1), (0..100).map(|n| (id(n), 100_000)).collect(), now);
        assert_eq!(asked(&outs, peer(1)).len(), 20);

        // 1 KB each: the in-flight cap binds first; verdicts on the deliveries free it for the
        // next poll, without waiting for another status.
        let mut relay = TxRelay::default();
        relay.poll(&[peer(1)], now);
        let first = asked(&listed(&mut relay, peer(1), (0..100).map(|n| (id(n), 1_000)).collect(), now), peer(1));
        assert_eq!(first.len(), MAX_IN_FLIGHT_PER_PEER);
        let delivered: Vec<(UnminedTxId, u32)> = first.iter().map(|i| (*i, 1_000)).collect();
        assert_eq!(relay.on_delivered(peer(1), &delivered, now).len(), MAX_IN_FLIGHT_PER_PEER);
        for id in &first {
            relay.on_outcome(peer(1), *id, Outcome::Accepted, now);
        }
        assert_eq!(asked(&relay.poll(&[peer(1)], now), peer(1)).len(), MAX_IN_FLIGHT_PER_PEER);
    }

    #[test]
    fn an_undelivered_ask_demotes_its_peer_and_goes_to_the_other_listers() {
        let now = Instant::now();
        let mut relay = TxRelay::default();
        let peers = [peer(1), peer(2), peer(3)];
        relay.poll(&peers, now);
        assert_eq!(asked(&listed(&mut relay, peer(1), vec![(id(7), 500)], now), peer(1)), vec![id(7)]);
        // Later listers wait while the first ask is within its time.
        for p in [peer(2), peer(3)] {
            assert!(asked(&listed(&mut relay, p, vec![(id(7), 500)], now), p).is_empty());
        }
        assert!(asked(&relay.poll(&peers, now + REREQUEST_AFTER / 2), peer(2)).is_empty());

        let later = now + REREQUEST_AFTER;
        let outs = relay.poll(&peers, later);
        assert_eq!(asked(&outs, peer(2)), vec![id(7)]);
        assert_eq!(asked(&outs, peer(3)), vec![id(7)]);
        // Not delivering is no fault: the peer is still heard and still asked.
        assert!(!relay.backing_off(peer(1), later));
        assert!(!relay.on_status(peer(1), vec![(id(8), 500)], later).is_empty());
    }

    #[test]
    fn when_every_lister_fails_it_is_forgotten_and_nobody_else_is_asked_or_demoted() {
        let now = Instant::now();
        let mut relay = TxRelay::default();
        let peers = [peer(1), peer(2), peer(3)];
        relay.poll(&peers, now);
        listed(&mut relay, peer(1), vec![(id(7), 500)], now);
        let outs = relay.poll(&peers, now + REREQUEST_AFTER);
        assert!(asked(&outs, peer(2)).is_empty() && asked(&outs, peer(3)).is_empty());
        assert!(relay.wanted.is_empty());
        for p in [peer(2), peer(3)] {
            assert_eq!(relay.peers.get_mut(&p).unwrap().delivery_failures.current(now), 0);
        }
    }

    #[test]
    fn each_lister_is_judged_by_the_size_it_listed() {
        let now = Instant::now();
        let mut relay = TxRelay::default();
        relay.poll(&[peer(1), peer(2)], now);
        // Peer 1 lists it first, claiming 10 bytes, and never delivers.
        listed(&mut relay, peer(1), vec![(id(1), 10)], now);
        listed(&mut relay, peer(2), vec![(id(1), 5_000)], now);
        let outs = relay.poll(&[peer(1), peer(2)], now + REREQUEST_AFTER);
        assert_eq!(asked(&outs, peer(2)), vec![id(1)]);
        // Peer 2 delivers the real 5000-byte body: no lie, so no strike, and it is taken.
        assert_eq!(relay.on_delivered(peer(2), &[(id(1), 5_000)], now + REREQUEST_AFTER), vec![id(1)]);
        assert!(!relay.backing_off(peer(2), now + REREQUEST_AFTER));
    }

    #[test]
    fn a_delivery_is_taken_once_only_if_asked_and_a_size_lie_strikes() {
        let now = Instant::now();
        let mut relay = TxRelay::default();
        relay.poll(&[peer(1), peer(2)], now);
        listed(&mut relay, peer(1), vec![(id(1), 500), (id(2), 500)], now);
        assert_eq!(relay.on_delivered(peer(2), &[(id(1), 500)], now), vec![], "not asked of peer 2");
        assert_eq!(relay.on_delivered(peer(1), &[(id(1), 500), (id(9), 500)], now), vec![id(1)]);
        assert_eq!(relay.on_delivered(peer(1), &[(id(1), 500)], now), vec![], "already taken");
        assert!(!relay.backing_off(peer(1), now));
        assert_eq!(relay.on_delivered(peer(1), &[(id(2), 501)], now), vec![], "larger than listed");
        assert!(relay.backing_off(peer(1), now));
    }

    #[test]
    fn only_an_invalid_verdict_strikes_and_strikes_are_forgotten() {
        let now = Instant::now();
        let mut relay = TxRelay::default();
        relay.poll(&[peer(1), peer(2)], now);
        for (p, n) in [(peer(1), 1), (peer(2), 2)] {
            listed(&mut relay, p, vec![(id(n), 500)], now);
            relay.on_delivered(p, &[(id(n), 500)], now);
        }
        relay.on_outcome(peer(1), id(1), Outcome::NotTaken, now);
        relay.on_outcome(peer(2), id(2), Outcome::Invalid, now);
        assert!(!relay.backing_off(peer(1), now));
        assert!(relay.backing_off(peer(2), now));
        // A second strike long after the first starts the backoff over (1 s, not 2 s).
        let much_later = now + STRIKES_FORGOTTEN_AFTER;
        relay.on_outcome(peer(2), id(3), Outcome::Invalid, much_later);
        assert!(relay.backing_off(peer(2), much_later + Duration::from_millis(999)));
        assert!(!relay.backing_off(peer(2), much_later + STRIKE_BACKOFF_MIN));
    }

    #[test]
    fn one_peer_cannot_fill_the_wanted_set_nor_leave_it_filled() {
        let now = Instant::now();
        let mut relay = TxRelay::default();
        relay.poll(&[peer(1), peer(2)], now);
        for chunk in 0..8u32 {
            listed(&mut relay, peer(1), (0..256).map(|n| (id(chunk * 256 + n), 10)).collect(), now);
        }
        assert_eq!(relay.wanted.len(), MAX_SOLE_LISTED_PER_PEER);
        // Another peer's listings still get in.
        listed(&mut relay, peer(2), vec![(id(100_000), 10)], now);
        assert!(relay.wanted.contains_key(&id(100_000)));
        // When peer 1 goes, what only it listed and nobody is asked for goes with it.
        relay.on_peer_gone(peer(1));
        assert_eq!(relay.wanted.keys().collect::<Vec<_>>(), vec![&id(100_000)]);
    }

    #[test]
    fn an_ask_that_could_not_be_sent_is_undone_and_retried() {
        let now = Instant::now();
        let mut relay = TxRelay::default();
        relay.poll(&[peer(1)], now);
        let ids = asked(&listed(&mut relay, peer(1), vec![(id(1), 500)], now), peer(1));
        relay.on_ask_unsent(peer(1), &ids);
        assert_eq!(relay.requested_bytes(peer(1)), 0);
        assert_eq!(asked(&relay.poll(&[peer(1)], now), peer(1)), vec![id(1)]);
    }

    #[test]
    fn the_same_id_is_not_asked_of_the_same_peer_again_within_the_cooldown() {
        let now = Instant::now();
        let mut relay = TxRelay::default();
        relay.poll(&[peer(1)], now);
        listed(&mut relay, peer(1), vec![(id(1), 500)], now);
        relay.on_delivered(peer(1), &[(id(1), 500)], now);
        relay.on_outcome(peer(1), id(1), Outcome::NotTaken, now);
        let soon = now + Duration::from_secs(1);
        assert!(asked(&listed(&mut relay, peer(1), vec![(id(1), 500)], soon), peer(1)).is_empty());
        let later = now + ASK_COOLDOWN;
        assert_eq!(asked(&listed(&mut relay, peer(1), vec![(id(1), 500)], later), peer(1)), vec![id(1)]);
    }

    #[test]
    fn a_peers_statuses_are_rate_limited() {
        let now = Instant::now();
        let mut relay = TxRelay::default();
        relay.poll(&[peer(1)], now);
        let heard = (0..STATUSES_IN_BURST + 10).filter(|n| !relay.on_status(peer(1), vec![(id(*n as u32), 10)], now).is_empty()).count();
        assert_eq!(heard as u64, STATUSES_IN_BURST);
        assert!(!relay.on_status(peer(1), vec![(id(999), 10)], now + Duration::from_secs(1)).is_empty());
    }

    #[test]
    fn serving_is_limited_by_bytes_reserved_up_front_and_by_request_rate() {
        let now = Instant::now();
        let mut relay = TxRelay::default();
        relay.on_added((0..40).map(|n| (id(n), 100_000)));
        let ids: Vec<UnminedTxId> = (0..40).map(id).collect();
        let fetched = match &relay.on_request(peer(1), ids.clone(), now)[..] {
            [Out::Fetch { ids, .. }] => ids.clone(),
            other => panic!("{other:?}"),
        };
        assert_eq!(fetched.len() as u64, SERVE_BURST_BYTES / 100_000);
        assert!(relay.serve_tokens(peer(1)) < 100_000);
        // What was just served is not served again, and ids we do not hold are not fetched.
        assert!(relay.on_request(peer(1), fetched, now + Duration::from_secs(1)).is_empty());
        assert!(relay.on_request(peer(1), vec![id(999)], now).is_empty());
        // Requests beyond the rate are dropped unread.
        let mut relay = TxRelay::default();
        relay.on_added([(id(1), 10)]);
        let answered = (0..TX_REQS_IN_BURST + 5).filter(|_| {
            let outs = relay.on_request(peer(1), vec![id(1)], now);
            relay.peers.get_mut(&peer(1)).unwrap().served.clear();
            !outs.is_empty()
        });
        assert_eq!(answered.count() as u64, TX_REQS_IN_BURST);
    }

    #[test]
    fn a_mempool_request_is_answered_in_full_at_most_once_per_interval_and_resent_if_unsent() {
        let now = Instant::now();
        let mut relay = TxRelay::default();
        relay.on_added((0..600).map(|n| (id(n), 300)));
        let outs = relay.on_mempool_req(peer(1), now);
        assert_eq!(outs.len(), 3);
        let listed: usize = outs
            .iter()
            .map(|o| match o {
                Out::Send { packet, bulk: true, .. } => read_tx_status(&packet[1..]).unwrap().1.len(),
                other => panic!("{other:?}"),
            })
            .sum();
        assert_eq!(listed, 600);
        assert!(relay.on_mempool_req(peer(1), now + MEMPOOL_REQ_ANSWER_INTERVAL / 2).is_empty());
        assert_eq!(relay.on_mempool_req(peer(1), now + MEMPOOL_REQ_ANSWER_INTERVAL).len(), 3);

        let mut relay = TxRelay::default();
        assert!(relay.poll(&[peer(2)], now).iter().any(|o| matches!(o, Out::MempoolReq { .. })));
        relay.on_mempool_req_unsent(peer(2));
        assert!(relay.poll(&[peer(2)], now).iter().any(|o| matches!(o, Out::MempoolReq { .. })));
    }

    /// Two peers list `id(7)`; peer 1 is asked first. Returns the relay at `now`.
    fn two_listers(now: Instant) -> TxRelay {
        let mut relay = TxRelay::default();
        relay.poll(&[peer(1), peer(2)], now);
        assert_eq!(asked(&listed(&mut relay, peer(1), vec![(id(7), 500)], now), peer(1)), vec![id(7)]);
        assert!(asked(&listed(&mut relay, peer(2), vec![(id(7), 500)], now), peer(2)).is_empty());
        relay
    }

    #[test]
    fn when_the_asked_peer_lies_or_leaves_the_other_listers_are_asked_at_once() {
        let now = Instant::now();
        let mut relay = two_listers(now);
        relay.on_delivered(peer(1), &[(id(7), 501)], now);
        assert_eq!(asked(&relay.poll(&[peer(1), peer(2)], now), peer(2)), vec![id(7)]);

        let mut relay = two_listers(now);
        relay.on_peer_gone(peer(1));
        assert_eq!(asked(&relay.poll(&[peer(2)], now), peer(2)), vec![id(7)]);
        assert!(relay.peers.get(&peer(2)).unwrap().listing.contains(&id(7)));
    }

    #[test]
    fn a_full_mempool_queue_keeps_it_wanted_and_retries_shortly() {
        let now = Instant::now();
        let mut relay = two_listers(now);
        assert_eq!(relay.on_delivered(peer(1), &[(id(7), 500)], now), vec![id(7)]);
        relay.on_outcome(peer(1), id(7), Outcome::Busy, now);
        assert_eq!(asked(&relay.poll(&[peer(1), peer(2)], now), peer(2)), vec![id(7)], "the other lister now");

        // With one lister, the same peer is asked again once it relists after the short retry.
        let mut relay = TxRelay::default();
        relay.poll(&[peer(1)], now);
        listed(&mut relay, peer(1), vec![(id(7), 500)], now);
        relay.on_delivered(peer(1), &[(id(7), 500)], now);
        relay.on_outcome(peer(1), id(7), Outcome::Busy, now);
        assert!(asked(&listed(&mut relay, peer(1), vec![(id(7), 500)], now), peer(1)).is_empty());
        let retry = now + BUSY_RETRY_AFTER;
        assert_eq!(asked(&listed(&mut relay, peer(1), vec![(id(7), 500)], retry), peer(1)), vec![id(7)]);
    }

    #[test]
    fn a_late_verdict_on_an_entry_given_up_on_leaves_the_one_wanted_afresh_alone() {
        for outcome in [Outcome::Busy, Outcome::NotTaken] {
            let now = Instant::now();
            let mut relay = TxRelay::default();
            relay.poll(&[peer(1)], now);
            listed(&mut relay, peer(1), vec![(id(7), 500)], now);
            assert_eq!(relay.on_delivered(peer(1), &[(id(7), 500)], now), vec![id(7)]);
            // Verified for longer than it had left: it is given up on meanwhile, then listed
            // again and asked for afresh.
            let later = now + WANTED_GIVE_UP;
            relay.poll(&[peer(1)], later);
            assert!(relay.wanted.is_empty());
            assert_eq!(asked(&listed(&mut relay, peer(1), vec![(id(7), 500)], later), peer(1)), vec![id(7)]);
            // The first submission's verdict lands: the new entry keeps its lister and its ask.
            relay.on_outcome(peer(1), id(7), outcome, later);
            let wanted = &relay.wanted[&id(7)];
            assert_eq!((wanted.listers.clone(), wanted.sole), (vec![(peer(1), 500)], Some(peer(1))), "{outcome:?}");
            assert!(wanted.asked.iter().any(|(p, _)| *p == peer(1)));
            assert!(relay.peers[&peer(1)].listing.contains(&id(7)));
            assert_eq!(relay.requested_bytes(peer(1)), 500);
        }
    }

    #[test]
    fn an_ask_lost_to_our_own_full_decode_queue_does_not_demote() {
        let now = Instant::now();
        let mut relay = two_listers(now);
        relay.on_body_dropped(peer(1), now);
        let later = now + REREQUEST_AFTER;
        let outs = relay.poll(&[peer(1), peer(2)], later);
        assert_eq!(relay.peers.get_mut(&peer(1)).unwrap().delivery_failures.current(later), 0);
        assert_eq!(asked(&outs, peer(1)), vec![id(7)], "still a lister, asked again");
    }

    #[test]
    fn what_awaits_a_verdict_counts_against_a_peers_asks() {
        let now = Instant::now();
        let mut relay = TxRelay::default();
        relay.poll(&[peer(1)], now);
        let first = asked(&listed(&mut relay, peer(1), (0..100).map(|n| (id(n), 1_000)).collect(), now), peer(1));
        let delivered: Vec<(UnminedTxId, u32)> = first.iter().map(|i| (*i, 1_000)).collect();
        relay.on_delivered(peer(1), &delivered, now);
        assert!(asked(&relay.poll(&[peer(1)], now), peer(1)).is_empty(), "all {MAX_IN_FLIGHT_PER_PEER} await verdicts");
        relay.on_outcome(peer(1), first[0], Outcome::Accepted, now);
        assert_eq!(asked(&relay.poll(&[peer(1)], now), peer(1)).len(), 1);
    }

    #[test]
    fn statuses_batch_changes_and_keep_a_heartbeat() {
        let now = Instant::now();
        let mut relay = TxRelay::default();
        let statuses = |outs: &[Out]| outs.iter().filter(|o| matches!(o, Out::Send { packet, .. } if packet[0] == PACKET_TYPE_TX_STATUS)).count();
        assert_eq!(statuses(&relay.poll(&[peer(1)], now)), 1, "a first status at once");
        relay.on_added([(id(1), 10)]);
        assert_eq!(statuses(&relay.poll(&[peer(1)], now + STATUS_MIN_GAP)), 0, "a change waits its batching delay");
        let batched = now + STATUS_MIN_GAP + STATUS_BATCH_MIN + Duration::from_millis(STATUS_BATCH_SPREAD_MS);
        assert_eq!(statuses(&relay.poll(&[peer(1)], batched)), 1);
        assert_eq!(statuses(&relay.poll(&[peer(1)], batched + STATUS_MIN_GAP)), 0, "nothing new");
        assert_eq!(statuses(&relay.poll(&[peer(1)], batched + STATUS_HEARTBEAT)), 1, "heartbeat");
    }

    // ---- load: the relay runs on the PoW sync thread, so no call may stall PoW or BFT sync ----
    //
    // Each test drives the relay at its adversarial maxima, times every call, prints a table
    // (run with --nocapture) and fails only past a bound that a complexity regression would
    // cross, not a slow machine: debug builds included.

    /// The slowest a call that handles one message or one tick may be, debug build included.
    const WORST_CALL: Duration = Duration::from_millis(50);
    /// The slowest a call that makes one pass over the mempool or the wanted set may be:
    /// answering a `MEMPOOL_REQ`, taking in a whole mempool, a peer that lists every wanted id
    /// leaving, and the poll that gives up on a whole wanted set that became wanted at once.
    const WORST_BULK_CALL: Duration = Duration::from_secs(1);
    /// A loaded machine preempts the odd call, and a growing map's resize makes a few calls in a
    /// hundred slower (the wanted set's, at 57,344 entries, about 18 ms in a debug build), so a
    /// bound is checked against the median, and the 95th percentile where there are 100 calls or
    /// more; the worst call only against this many times the bound.
    const PREEMPTED: u32 = 5;
    const GIVE_UP_ALL: &str = "poll (gives up on all wanted)";
    const LOAD_PEERS: u16 = 64;
    /// Small enough that the in-flight count, not the byte budget, caps a peer's asks.
    const LOAD_TX_SIZE: u32 = 2_000;

    fn load_peers() -> Vec<ConnectionKey> {
        (1..=LOAD_PEERS).map(peer).collect()
    }

    /// A full status's worth of ids, distinct for every `c`.
    fn chunk(c: u32) -> Vec<(UnminedTxId, u32)> {
        let n = MAX_TX_IDS_PER_STATUS as u32;
        (c * n..(c + 1) * n).map(|i| (id(i), LOAD_TX_SIZE)).collect()
    }

    /// Wall-clock time of each call, by operation, with the operation's bound.
    #[derive(Default)]
    struct Timings(Vec<(&'static str, Duration, Vec<Duration>)>);

    impl Timings {
        fn time<T>(&mut self, op: &'static str, call: impl FnOnce() -> T) -> T {
            let bound = if op == GIVE_UP_ALL { WORST_BULK_CALL } else { WORST_CALL };
            self.time_within(op, bound, call)
        }

        fn time_bulk<T>(&mut self, op: &'static str, call: impl FnOnce() -> T) -> T {
            self.time_within(op, WORST_BULK_CALL, call)
        }

        fn time_within<T>(&mut self, op: &'static str, bound: Duration, call: impl FnOnce() -> T) -> T {
            let start = std::time::Instant::now();
            let out = call();
            let took = start.elapsed();
            match self.0.iter_mut().find(|(name, ..)| *name == op) {
                Some((.., all)) => all.push(took),
                None => self.0.push((op, bound, vec![took])),
            }
            out
        }

        /// Print the table, then check each operation against its bound.
        fn report(&self, scenario: &str, notes: &str) {
            let build = if cfg!(debug_assertions) { "debug" } else { "release" };
            let mut table = format!("\n{scenario} ({build} build)\n  {:<34} {:>7} {:>12} {:>12} {:>12} {:>8}\n", "operation", "calls", "worst", "p95", "mean", "bound");
            let mut over = Vec::new();
            for (op, bound, all) in &self.0 {
                let mut sorted = all.clone();
                sorted.sort();
                let worst = sorted[sorted.len() - 1];
                let p95 = if sorted.len() >= 100 { sorted[sorted.len() * 95 / 100] } else { worst };
                let mean = all.iter().sum::<Duration>() / all.len() as u32;
                table += &format!("  {op:<34} {:>7} {worst:>12.3?} {p95:>12.3?} {mean:>12.3?} {bound:>8?}\n", all.len());
                let median = sorted[sorted.len() / 2];
                if median >= *bound || (sorted.len() >= 100 && p95 >= *bound) || worst >= *bound * PREEMPTED {
                    over.push(format!("{op}: worst {worst:?}, p95 {p95:?}, median {median:?}, bound {bound:?}"));
                }
            }
            table += notes;
            // One print, so parallel tests do not interleave their tables.
            println!("{table}");
            assert!(over.is_empty(), "{scenario}: {over:?}");
        }
    }

    /// The asks made of each peer and not yet answered, as the transport would carry them.
    type Asks = HashMap<ConnectionKey, Vec<UnminedTxId>>;

    fn note_asks(asks: &mut Asks, outs: impl IntoIterator<Item = Out>) {
        for out in outs {
            if let Out::Ask { to, ids, .. } = out {
                asks.entry(to).or_default().extend(ids);
            }
        }
    }

    /// A peer lists `entries`, and the mempool keeps them all (it holds none of them).
    fn list_timed(relay: &mut TxRelay, timings: &mut Timings, asks: &mut Asks, from: ConnectionKey, entries: Vec<(UnminedTxId, u32)>, now: Instant) {
        for out in timings.time("on_status (256 ids)", || relay.on_status(from, entries, now)) {
            match out {
                Out::Filter { from, entries } => {
                    let more = timings.time("on_filtered (all kept)", || relay.on_filtered(from, entries, now));
                    note_asks(asks, more);
                }
                other => note_asks(asks, [other]),
            }
        }
    }

    /// The bounds the design promises, and the books balancing, over the whole state.
    fn check_bounds(relay: &TxRelay) {
        assert!(relay.wanted.len() <= MAX_WANTED);
        let mut sole_of: HashMap<ConnectionKey, usize> = HashMap::new();
        for (id, wanted) in &relay.wanted {
            assert!(wanted.listers.len() <= MAX_LISTERS);
            if let Some(sole) = wanted.sole {
                assert_eq!(wanted.listers.iter().map(|(p, _)| *p).collect::<Vec<_>>(), vec![sole], "a sole lister is the only one");
                *sole_of.entry(sole).or_default() += 1;
            }
            for (lister, _) in &wanted.listers {
                assert!(relay.peers.get(lister).is_none_or(|peer| peer.listing.contains(id)), "a lister's listing names what it lists");
            }
        }
        for (key, peer) in &relay.peers {
            assert!(peer.sole_listed <= MAX_SOLE_LISTED_PER_PEER);
            assert_eq!(peer.sole_listed, sole_of.get(key).copied().unwrap_or(0), "sole_listed counts exactly the ids only it lists");
            assert_eq!(peer.requested_bytes, peer.requested.values().map(|size| u64::from(*size)).sum::<u64>(), "requested_bytes drifted");
            assert!(peer.requested_bytes <= MAX_REQUESTED_BYTES_PER_PEER);
            assert!(peer.requested.len() + peer.submitted <= MAX_IN_FLIGHT_PER_PEER);
            for id in peer.requested.keys() {
                assert!(relay.wanted.get(id).is_some_and(|w| w.asked.iter().any(|(p, _)| p == key)), "a request is an outstanding ask");
            }
            for id in &peer.listing {
                assert!(relay.wanted.get(id).is_some_and(|w| w.listed_size(*key).is_some()), "a listing names only wanted ids it lists");
            }
            assert!(peer.asked_at.len() <= MAX_ASKED_REMEMBERED && peer.asked_order.len() <= MAX_ASKED_REMEMBERED);
            assert!(peer.served.len() <= MAX_SERVED_REMEMBERED && peer.served_order.len() <= MAX_SERVED_REMEMBERED);
        }
    }

    /// The compaction `poll` promises: its ordered queues hold at most twice what was wanted when
    /// it compacted them (no more than `wanted_before`, what was wanted when it was called) plus
    /// slack, and the asks it made after.
    fn check_compacted(relay: &TxRelay, wanted_before: usize) {
        let bound = 2 * wanted_before + 1024;
        assert!(relay.wanted_order.len() <= bound, "wanted_order {} for {wanted_before} wanted", relay.wanted_order.len());
        let asks_made = relay.peers.len() * MAX_IN_FLIGHT_PER_PEER;
        assert!(relay.asks.len() <= bound + asks_made, "asks {} for {wanted_before} wanted", relay.asks.len());
    }

    /// Once everything is delivered or forgotten, nothing is held against anyone.
    fn check_drained(relay: &TxRelay) {
        assert!(relay.wanted.is_empty(), "{} still wanted", relay.wanted.len());
        assert!(relay.wanted_order.is_empty());
        assert!(relay.asks.len() <= 1024);
        for peer in relay.peers.values() {
            assert_eq!(peer.requested_bytes, 0);
            assert!(peer.requested.is_empty() && peer.listing.is_empty());
            assert_eq!(peer.sole_listed, 0);
        }
    }

    /// The largest structures seen, for the notes under each table.
    #[derive(Debug, Default)]
    struct Peak {
        wanted: usize,
        wanted_order: usize,
        asks: usize,
        to_ask_one_peer: usize,
        to_ask_all_peers: usize,
        listing_one_peer: usize,
    }

    impl Peak {
        fn see(&mut self, relay: &TxRelay) {
            self.wanted = self.wanted.max(relay.wanted.len());
            self.wanted_order = self.wanted_order.max(relay.wanted_order.len());
            self.asks = self.asks.max(relay.asks.len());
            self.to_ask_one_peer = self.to_ask_one_peer.max(relay.peers.values().map(|p| p.to_ask.len()).max().unwrap_or(0));
            self.to_ask_all_peers = self.to_ask_all_peers.max(relay.peers.values().map(|p| p.to_ask.len()).sum());
            self.listing_one_peer = self.listing_one_peer.max(relay.peers.values().map(|p| p.listing.len()).max().unwrap_or(0));
        }
    }

    /// Every peer lists full statuses of new ids, each chunk then listed by the next peer too, so
    /// the per-peer cap on sole listings never binds, until [`MAX_WANTED`] are wanted. Peer `p`
    /// ends up listing its own chunks and peer `p - 1`'s.
    fn fill_wanted(relay: &mut TxRelay, peers: &[ConnectionKey], now: &mut Instant, timings: &mut Timings, asks: &mut Asks, peak: &mut Peak) {
        let mut round = 0;
        while relay.wanted.len() < MAX_WANTED {
            for (p, key) in peers.iter().enumerate() {
                let c = (round * peers.len() + p) as u32;
                list_timed(relay, timings, asks, *key, chunk(c), *now);
                list_timed(relay, timings, asks, peers[(p + 1) % peers.len()], chunk(c), *now);
            }
            *now += Duration::from_millis(100);
            let wanted_before = relay.wanted.len();
            let outs = timings.time("poll", || relay.poll(peers, *now));
            note_asks(asks, outs);
            check_bounds(relay);
            check_compacted(relay, wanted_before);
            peak.see(relay);
            round += 1;
            assert!(round <= 8, "wanted stuck at {}", relay.wanted.len());
        }
        assert_eq!(relay.wanted.len(), MAX_WANTED);
    }

    #[test]
    fn sixty_four_peers_listing_full_statuses_fill_the_wanted_set_without_stalling_sync() {
        let peers = load_peers();
        let mut relay = TxRelay::default();
        let mut timings = Timings::default();
        let mut asks = Asks::default();
        let mut peak = Peak::default();
        let mut now = Instant::now();
        note_asks(&mut asks, timings.time("poll", || relay.poll(&peers, now)));
        fill_wanted(&mut relay, &peers, &mut now, &mut timings, &mut asks, &mut peak);

        // Then every peer keeps listing in full: half ids already wanted (each gains listers up
        // to MAX_LISTERS), half new (refused while the set is full). Even peers deliver what
        // they are asked and the mempool accepts it; odd peers never deliver, so their asks go
        // overdue and are re-made of other listers.
        let mut next_new = (MAX_WANTED / MAX_TX_IDS_PER_STATUS) as u32;
        for round in 0..40u32 {
            for (p, key) in peers.iter().enumerate() {
                let mut entries = chunk((p as u32 * 5 + round) % next_new);
                entries.truncate(MAX_TX_IDS_PER_STATUS / 2);
                entries.extend(chunk(next_new).into_iter().take(MAX_TX_IDS_PER_STATUS / 2));
                next_new += 1;
                list_timed(&mut relay, &mut timings, &mut asks, *key, entries, now);
            }
            for key in peers.iter().step_by(2) {
                let Some(ids) = asks.remove(key) else { continue };
                let delivered: Vec<(UnminedTxId, u32)> = ids.iter().map(|id| (*id, LOAD_TX_SIZE)).collect();
                let taken = timings.time("on_delivered", || relay.on_delivered(*key, &delivered, now));
                for id in taken {
                    timings.time("on_outcome (Accepted)", || relay.on_outcome(*key, id, Outcome::Accepted, now));
                    timings.time("on_added (1)", || relay.on_added([(id, LOAD_TX_SIZE)]));
                }
            }
            now += Duration::from_millis(250);
            let wanted_before = relay.wanted.len();
            let outs = timings.time("poll", || relay.poll(&peers, now));
            note_asks(&mut asks, outs);
            check_bounds(&relay);
            check_compacted(&relay, wanted_before);
            peak.see(&relay);
        }

        // Whatever is left is given up on; nothing stays held against anyone.
        now += WANTED_GIVE_UP;
        for _ in 0..2 {
            now += REREQUEST_AFTER;
            timings.time(GIVE_UP_ALL, || relay.poll(&peers, now));
        }
        check_bounds(&relay);
        check_drained(&relay);
        timings.report("64 peers listing full statuses", &format!("  peaks: {peak:?}\n  mirror at the end: {}", relay.mirror.len()));
    }

    #[test]
    fn a_fifty_thousand_transaction_mempool_is_listed_and_answered_without_stalling_sync() {
        const MIRROR: u32 = 50_000;
        let peers = load_peers();
        let mut relay = TxRelay::default();
        let mut timings = Timings::default();
        let mut now = Instant::now();
        // Our own ids, apart from anything a peer lists.
        let ours = |n: u32| id((1 << 31) | n);
        timings.time_bulk("on_added (50,000 at once)", || relay.on_added((0..MIRROR).map(|n| (ours(n), LOAD_TX_SIZE))));

        // Every peer asks for the whole listing at once.
        for key in &peers {
            let outs = timings.time_bulk("on_mempool_req (50,000 listed)", || relay.on_mempool_req(*key, now));
            assert_eq!(outs.len(), (MIRROR as usize).div_ceil(MAX_TX_IDS_PER_STATUS));
            let listed: usize = outs
                .iter()
                .map(|out| match out {
                    Out::Send { packet, bulk: true, .. } => read_tx_status(&packet[1..]).unwrap().1.len(),
                    other => panic!("{other:?}"),
                })
                .sum();
            assert_eq!(listed, MIRROR as usize);
        }

        // The mempool churns while every peer is sent statuses: a hundred in, a hundred out.
        let mut statuses = 0;
        for round in 0..40u32 {
            let gone: Vec<UnminedTxId> = (round * 100..(round + 1) * 100).map(ours).collect();
            timings.time("on_removed (100)", || relay.on_removed(gone));
            let added: Vec<(UnminedTxId, u32)> = (MIRROR + round * 100..MIRROR + (round + 1) * 100).map(|n| (ours(n), LOAD_TX_SIZE)).collect();
            timings.time("on_added (100)", || relay.on_added(added));
            now += STATUS_BATCH_MIN + Duration::from_millis(STATUS_BATCH_SPREAD_MS);
            let outs = timings.time("poll (statuses to 64 peers)", || relay.poll(&peers, now));
            statuses += outs.iter().filter(|o| matches!(o, Out::Send { packet, .. } if packet[0] == PACKET_TYPE_TX_STATUS)).count();
            // Arrival order is pruned once it is twice the mirror.
            assert!(relay.arrival.len() <= 2 * relay.mirror.len() + 64 + 100);
        }
        assert!(statuses >= peers.len() * 20, "{statuses} statuses");

        // Every peer asks for a full TX_REQ of what we hold.
        for (p, key) in peers.iter().enumerate() {
            let ids: Vec<UnminedTxId> = (0..MAX_TXS_PER_MESSAGE as u32).map(|n| ours(10_000 + p as u32 * 64 + n)).collect();
            let outs = timings.time("on_request (64 ids)", || relay.on_request(*key, ids, now));
            assert!(matches!(&outs[..], [Out::Fetch { ids, .. }] if ids.len() == MAX_TXS_PER_MESSAGE));
        }
        timings.time_bulk("on_reset (50,000)", || relay.on_reset((0..MIRROR).map(|n| (ours(n), LOAD_TX_SIZE))));
        assert_eq!(relay.mirror.len(), MIRROR as usize);
        assert_eq!(relay.arrival.len(), MIRROR as usize);
        timings.report("a 50,000-transaction mempool", &format!("  statuses sent: {statuses}"));
    }

    #[test]
    fn peers_with_full_listings_coming_and_going_do_not_stall_sync_or_grow_the_relay() {
        let peers = load_peers();
        let mut relay = TxRelay::default();
        let mut timings = Timings::default();
        let mut asks = Asks::default();
        let mut peak = Peak::default();
        let mut now = Instant::now();
        relay.poll(&peers, now);
        fill_wanted(&mut relay, &peers, &mut now, &mut timings, &mut asks, &mut peak);
        let chunks_per_peer = MAX_WANTED / MAX_TX_IDS_PER_STATUS / peers.len();

        // Half the peers leave (each listing 2048 ids, every one also listed by a peer that
        // stays), come back and list it all again; then the other half. Nobody delivers.
        let mut to_ask_by_round = Vec::new();
        for round in 0..16 {
            let leaving: Vec<usize> = (0..peers.len()).filter(|p| p % 2 == round % 2).collect();
            for &p in &leaving {
                timings.time("on_peer_gone (2048 listed)", || relay.on_peer_gone(peers[p]));
            }
            now += Duration::from_millis(500);
            note_asks(&mut asks, timings.time("poll", || relay.poll(&peers, now)));
            for &p in &leaving {
                // Its own chunks, and its predecessor's.
                let prev = (p + peers.len() - 1) % peers.len();
                for r in 0..chunks_per_peer {
                    for owner in [p, prev] {
                        list_timed(&mut relay, &mut timings, &mut asks, peers[p], chunk((r * peers.len() + owner) as u32), now);
                    }
                }
            }
            now += Duration::from_millis(500);
            let wanted_before = relay.wanted.len();
            note_asks(&mut asks, timings.time("poll", || relay.poll(&peers, now)));
            check_bounds(&relay);
            check_compacted(&relay, wanted_before);
            peak.see(&relay);
            to_ask_by_round.push((relay.wanted.len(), relay.peers.values().map(|p| p.to_ask.len()).max().unwrap_or(0)));
        }

        // Churn does not grow what is queued to ask: each round leaves it where the first did.
        assert!(to_ask_by_round.iter().all(|(_, to_ask)| *to_ask <= to_ask_by_round[0].1), "{to_ask_by_round:?}");

        now += WANTED_GIVE_UP;
        for _ in 0..2 {
            now += REREQUEST_AFTER;
            timings.time(GIVE_UP_ALL, || relay.poll(&peers, now));
        }
        check_bounds(&relay);
        check_drained(&relay);
        timings.report("64 peers with full listings coming and going", &format!("  peaks: {peak:?}\n  (wanted, largest to_ask) after each round: {to_ask_by_round:?}"));
    }

    #[test]
    fn a_peer_listing_every_wanted_id_can_leave_without_stalling_sync() {
        // Joining ids someone else made wanted has no per-peer cap, so one peer's listing is
        // bounded by its status rate and WANTED_GIVE_UP: (40 + 4/s * 60 s) statuses of 256 ids
        // is more than MAX_WANTED. Its leaving then touches every wanted id.
        let peers = load_peers();
        let everywhere = peer(1000);
        let all: Vec<ConnectionKey> = peers.iter().copied().chain([everywhere]).collect();
        let mut relay = TxRelay::default();
        let mut timings = Timings::default();
        let mut asks = Asks::default();
        let mut peak = Peak::default();
        let mut now = Instant::now();
        relay.poll(&all, now);
        let start = now;
        fill_wanted(&mut relay, &peers, &mut now, &mut timings, &mut asks, &mut peak);
        // A status every 225 ms stays within its rate and lists everything inside the minute
        // before the first ids are given up on. Nobody delivers.
        for c in 0..(MAX_WANTED / MAX_TX_IDS_PER_STATUS) as u32 {
            list_timed(&mut relay, &mut timings, &mut asks, everywhere, chunk(c), now);
            now += Duration::from_millis(225);
            if c % 4 == 3 {
                let wanted_before = relay.wanted.len();
                note_asks(&mut asks, timings.time("poll", || relay.poll(&all, now)));
                check_bounds(&relay);
                check_compacted(&relay, wanted_before);
                peak.see(&relay);
            }
        }
        assert!(now < start + WANTED_GIVE_UP);
        let listed = relay.peers[&everywhere].listing.len();
        let wanted = relay.wanted.len();
        assert!(listed >= MAX_WANTED / 2, "listed {listed} of {wanted} wanted");
        let to_ask_before = relay.peers.values().map(|p| p.to_ask.len()).sum::<usize>();
        timings.time_bulk("on_peer_gone (every wanted id)", || relay.on_peer_gone(everywhere));
        let to_ask_after = relay.peers.values().map(|p| p.to_ask.len()).sum::<usize>();
        check_bounds(&relay);
        peak.see(&relay);

        now += WANTED_GIVE_UP;
        for _ in 0..2 {
            now += REREQUEST_AFTER;
            timings.time(GIVE_UP_ALL, || relay.poll(&peers, now));
        }
        check_bounds(&relay);
        check_drained(&relay);
        timings.report(
            "a peer listing every wanted id leaves",
            &format!("  it listed {listed} of {wanted} wanted\n  to_ask, all peers: {to_ask_before} before it left, {to_ask_after} after\n  peaks: {peak:?}"),
        );
    }

    #[test]
    fn many_overdue_asks_are_re_made_and_given_up_on_without_stalling_sync() {
        let peers = load_peers();
        let mut relay = TxRelay::default();
        let mut timings = Timings::default();
        let mut asks = Asks::default();
        let mut peak = Peak::default();
        let mut now = Instant::now();
        relay.poll(&peers, now);
        fill_wanted(&mut relay, &peers, &mut now, &mut timings, &mut asks, &mut peak);
        let in_flight: usize = relay.peers.values().map(|p| p.requested.len()).sum();
        assert_eq!(in_flight, peers.len() * MAX_IN_FLIGHT_PER_PEER);

        // Nobody ever delivers: every REREQUEST_AFTER, every peer's asks are overdue at once,
        // demote it, and go to the other lister, until both have failed and it is forgotten.
        let mut overdue_polls = 0;
        let mut asks_by_poll = Vec::new();
        let give_up_at = now + WANTED_GIVE_UP;
        for _ in 0..(WANTED_GIVE_UP.as_secs() + 10) {
            now += Duration::from_secs(1);
            let op = if now >= give_up_at && !relay.wanted.is_empty() { GIVE_UP_ALL } else { "poll (asks overdue)" };
            let wanted_before = relay.wanted.len();
            let outs = timings.time(op, || relay.poll(&peers, now));
            overdue_polls += usize::from(outs.iter().any(|o| matches!(o, Out::Ask { .. })));
            check_bounds(&relay);
            check_compacted(&relay, wanted_before);
            peak.see(&relay);
            asks_by_poll.push(relay.asks.len());
        }
        assert!(overdue_polls > 0);
        check_drained(&relay);
        timings.report("2048 asks overdue at once, nobody delivering", &format!("  peaks: {peak:?}\n  asks deque after each poll: {asks_by_poll:?}"));
    }

    /// A small xorshift generator: the random traffic test is the same on every run.
    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n
        }
    }

    #[test]
    fn random_relay_traffic_keeps_the_books_balanced_and_drains_to_nothing() {
        let peers: Vec<ConnectionKey> = (1..=8).map(peer).collect();
        let mut relay = TxRelay::default();
        let mut rng = Rng(0x5eed_1234_abcd_0001);
        let mut now = Instant::now();
        let mut asks = Asks::default();
        // Delivered and handed to the mempool: each gets exactly one outcome.
        let mut submitted: Vec<(ConnectionKey, UnminedTxId)> = Vec::new();
        let mut live = peers.clone();
        relay.poll(&live, now);
        for step in 0..20_000 {
            let key = peers[rng.below(peers.len() as u64) as usize];
            match rng.below(12) {
                0..=2 => {
                    // A listing: sizes differ by peer, so some deliveries are lies.
                    let entries: Vec<(UnminedTxId, u32)> = (0..rng.below(40)).map(|_| (id(rng.below(3000) as u32), 500 + rng.below(3) as u32 * 100)).collect();
                    for out in relay.on_status(key, entries, now) {
                        match out {
                            Out::Filter { from, entries } => {
                                let keep: Vec<(UnminedTxId, u32)> = entries.into_iter().filter(|_| rng.below(4) != 0).collect();
                                note_asks(&mut asks, relay.on_filtered(from, keep, now));
                            }
                            other => note_asks(&mut asks, [other]),
                        }
                    }
                }
                3..=4 => {
                    // Some of what it was asked, at sizes that are sometimes larger than listed.
                    let ids = asks.remove(&key).unwrap_or_default();
                    let delivered: Vec<(UnminedTxId, u32)> = ids.iter().filter_map(|id| (rng.below(3) != 0).then(|| (*id, 500 + rng.below(4) as u32 * 100))).collect();
                    submitted.extend(relay.on_delivered(key, &delivered, now).into_iter().map(|id| (key, id)));
                }
                5..=6 if !submitted.is_empty() => {
                    let (from, id) = submitted.swap_remove(rng.below(submitted.len() as u64) as usize);
                    let outcome = [Outcome::Accepted, Outcome::NotTaken, Outcome::Busy, Outcome::Invalid][rng.below(4) as usize];
                    relay.on_outcome(from, id, outcome, now);
                    if outcome == Outcome::Accepted {
                        relay.on_added([(id, 500)]);
                    }
                }
                7 => {
                    // An ask that could not be sent.
                    if let Some(ids) = asks.remove(&key) {
                        relay.on_ask_unsent(key, &ids);
                    }
                }
                8 => relay.on_body_dropped(key, now),
                9 => {
                    if live.contains(&key) && rng.below(4) == 0 {
                        relay.on_peer_gone(key);
                        asks.remove(&key);
                        live.retain(|p| *p != key);
                    } else if !live.contains(&key) {
                        live.push(key);
                    }
                }
                10 => relay.on_removed((0..rng.below(20)).map(|_| id(rng.below(3000) as u32))),
                _ => {
                    now += Duration::from_millis(rng.below(3000));
                    let wanted_before = relay.wanted.len();
                    note_asks(&mut asks, relay.poll(&live, now));
                    check_bounds(&relay);
                    check_compacted(&relay, wanted_before);
                }
            }
            if step % 64 == 0 {
                check_bounds(&relay);
            }
        }

        // Every verdict arrives, every peer leaves or every ask is given up on.
        for (from, id) in submitted.drain(..) {
            relay.on_outcome(from, id, Outcome::NotTaken, now);
        }
        now += WANTED_GIVE_UP;
        for _ in 0..2 {
            now += REREQUEST_AFTER;
            relay.poll(&live, now);
        }
        check_bounds(&relay);
        check_drained(&relay);
        assert!(relay.peers.values().all(|peer| peer.submitted == 0));
    }
}
