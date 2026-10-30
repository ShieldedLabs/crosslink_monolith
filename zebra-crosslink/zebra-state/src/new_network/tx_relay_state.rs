//! Transaction relay's bookkeeping: what we list, what we want and from whom, what we serve.
//!
//! Pure: every call takes the time, and returns what to do ([`Out`]) instead of doing it, so the
//! bounds below can be tested without sockets or a mempool. The sync loop carries the [`Out`]s
//! to the transport and to the mempool bridge, and feeds the answers back in.
//!
//! Relay is pull-based (no pushes, so nothing marks a transaction's origin). Every bound comes
//! from the relay's security review:
//! - what we ask a peer for is budgeted by bytes ([`MAX_REQUESTED_BYTES_PER_PEER`]) and count
//!   ([`MAX_IN_FLIGHT_PER_PEER`]);
//! - a wanted transaction remembers up to [`MAX_LISTERS`] peers that listed it, and is asked
//!   of all of them once the first ask goes unanswered for [`REREQUEST_AFTER`], so a peer
//!   that lists ids it never delivers cannot hold a transaction back; it gets a strike;
//! - a peer's strikes back it off (it is neither heard nor asked), 1 s doubling to 64 s. A
//!   strike is only for what no honest peer does whatever our state: failing to deliver what
//!   it listed, or sending a transaction that fails verification for a reason independent of
//!   our state. Malformed bytes are the transport's to kill for, not ours.
//! - what we serve a peer is limited by a token bucket ([`SERVE_BURST_BYTES`],
//!   [`SERVE_BYTES_PER_SEC`]) and the same id is not served to it again within
//!   [`SERVE_REPEAT_SUPPRESS`].

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use tenderlink::stp::ConnectionKey;
use zebra_chain::transaction::UnminedTxId;

use super::tx_relay::{mempool_req_packet, tx_req_packet, tx_status_packet, MAX_TXS_PER_MESSAGE, MAX_TX_IDS_PER_STATUS};

pub(super) const MAX_REQUESTED_BYTES_PER_PEER: u64 = 2 << 20;
pub(super) const MAX_IN_FLIGHT_PER_PEER: usize = 32;
pub(super) const MAX_LISTERS: usize = 8;
pub(super) const REREQUEST_AFTER: Duration = Duration::from_secs(4);
/// A wanted transaction no lister delivers is forgotten after this; a later listing starts over.
pub(super) const WANTED_GIVE_UP: Duration = Duration::from_secs(60);
/// Wanted transactions held at most: past it, new listings are ignored until some resolve.
pub(super) const MAX_WANTED: usize = 8192;
/// Ids queued to ask one peer at most; past it the oldest are dropped from its queue.
const MAX_TO_ASK_PER_PEER: usize = 1024;
pub(super) const STATUS_HEARTBEAT: Duration = Duration::from_secs(30);
pub(super) const STATUS_MIN_GAP: Duration = Duration::from_millis(250);
/// A change waits 100..500 ms before it is listed, so a burst goes out as one status.
const STATUS_BATCH_MIN: Duration = Duration::from_millis(100);
const STATUS_BATCH_SPREAD_MS: u64 = 400;
pub(super) const MEMPOOL_REQ_ANSWER_INTERVAL: Duration = Duration::from_secs(600);
pub(super) const SERVE_BURST_BYTES: u64 = 2 << 20;
pub(super) const SERVE_BYTES_PER_SEC: u64 = 256 << 10;
pub(super) const SERVE_REPEAT_SUPPRESS: Duration = Duration::from_secs(10);
const STRIKE_BACKOFF_MIN: Duration = Duration::from_secs(1);
const STRIKE_BACKOFF_MAX: Duration = Duration::from_secs(64);

/// What the sync loop does for the relay.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Out {
    /// Send on the peer's control channel (`bulk: false`) or bulk channel.
    Send { to: ConnectionKey, packet: Vec<u8>, bulk: bool },
    /// Ask the mempool which of `entries` it already holds or has rejected; answer with
    /// [`TxRelay::on_filtered`].
    Filter { from: ConnectionKey, entries: Vec<(UnminedTxId, u32)> },
    /// Fetch these from the mempool and serve them; answer with [`TxRelay::on_bodies`].
    Fetch { to: ConnectionKey, ids: Vec<UnminedTxId> },
}

/// What became of a transaction a peer delivered, as the mempool judged it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Outcome {
    /// Accepted, or already known: it will show up in (or is already in) our listing.
    Accepted,
    /// Refused for a reason that may be ours (our tip, mempool contents, a full queue, a
    /// timeout): no fault of the peer; another lister may be asked later.
    NotTaken,
    /// Fails verification whatever our state (a bad proof or signature): the peer is struck.
    Invalid,
}

#[derive(Debug)]
struct Wanted {
    size: u32,
    listers: Vec<ConnectionKey>,
    /// When we last asked each peer we asked.
    asked: Vec<(ConnectionKey, Instant)>,
    first_seen: Instant,
    /// Delivered and handed to the mempool; waiting for its verdict.
    submitted: bool,
}

#[derive(Debug, Default)]
struct Strikes {
    count: u32,
    until: Option<Instant>,
}

impl Strikes {
    fn strike(&mut self, now: Instant) {
        self.count = self.count.saturating_add(1);
        let backoff = STRIKE_BACKOFF_MIN.saturating_mul(1 << (self.count - 1).min(6)).min(STRIKE_BACKOFF_MAX);
        self.until = Some(now + backoff);
    }
    fn backing_off(&self, now: Instant) -> bool {
        self.until.is_some_and(|until| now < until)
    }
}

#[derive(Debug)]
struct PeerTx {
    /// Ids asked of this peer and not yet delivered, with their listed size.
    requested: HashMap<UnminedTxId, u32>,
    /// Wanted ids this peer listed that it has not been asked for yet, oldest first. Stale
    /// entries (resolved, or asked of someone else) are skipped when popped.
    to_ask: VecDeque<UnminedTxId>,
    requested_bytes: u64,
    /// The listing generation last sent, and when; and when the next is due after a change.
    sent_generation: u64,
    last_status: Option<Instant>,
    status_due: Option<Instant>,
    /// When we last served each id to this peer.
    served: HashMap<UnminedTxId, Instant>,
    serve_tokens: u64,
    serve_refilled: Instant,
    strikes: Strikes,
    mempool_req_sent: bool,
    mempool_req_answered: Option<Instant>,
}

impl PeerTx {
    fn new(now: Instant) -> Self {
        Self {
            requested: HashMap::new(),
            to_ask: VecDeque::new(),
            requested_bytes: 0,
            sent_generation: 0,
            last_status: None,
            status_due: None,
            served: HashMap::new(),
            serve_tokens: SERVE_BURST_BYTES,
            serve_refilled: now,
            strikes: Strikes::default(),
            mempool_req_sent: false,
            mempool_req_answered: None,
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct TxRelay {
    /// Our mempool's ids and sizes (kept from its change stream), and the order they arrived
    /// in, newest last, for listing the most recent ones.
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

fn queue_ask(peer: &mut PeerTx, id: UnminedTxId) {
    if peer.to_ask.len() >= MAX_TO_ASK_PER_PEER {
        peer.to_ask.pop_front();
    }
    peer.to_ask.push_back(id);
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
    fn peer(&mut self, key: ConnectionKey, now: Instant) -> &mut PeerTx {
        self.peers.entry(key).or_insert_with(|| PeerTx::new(now))
    }

    // ---- our mempool ----

    /// Transactions entered our mempool. Ones we wanted are no longer wanted.
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

    fn listing(&self) -> impl Iterator<Item = (&UnminedTxId, u32)> {
        self.arrival.iter().rev().filter_map(|id| self.mirror.get(id).map(|size| (id, *size)))
    }

    // ---- what peers list ----

    /// A peer's `TX_STATUS`. Ids we neither hold nor want are passed to the mempool to drop
    /// what it holds or has rejected; ids we already want gain the peer as a lister.
    pub fn on_status(&mut self, from: ConnectionKey, entries: Vec<(UnminedTxId, u32)>, now: Instant) -> Vec<Out> {
        if self.peer(from, now).strikes.backing_off(now) {
            return Vec::new();
        }
        let mut new = Vec::new();
        for (id, size) in entries {
            if self.mirror.contains_key(&id) {
                continue;
            }
            match self.wanted.get_mut(&id) {
                Some(wanted) => {
                    if wanted.listers.len() < MAX_LISTERS && !wanted.listers.contains(&from) {
                        wanted.listers.push(from);
                        queue_ask(self.peers.get_mut(&from).expect("made above"), id);
                    }
                }
                None => new.push((id, size)),
            }
        }
        let mut out = Vec::new();
        if !new.is_empty() {
            out.push(Out::Filter { from, entries: new });
        }
        out.extend(self.request_from(from, now));
        out
    }

    /// The mempool's answer to [`Out::Filter`]: `keep` are the entries it neither holds nor
    /// has rejected. They become wanted, listed by `from`, and are asked of it.
    pub fn on_filtered(&mut self, from: ConnectionKey, keep: Vec<(UnminedTxId, u32)>, now: Instant) -> Vec<Out> {
        if !self.peers.contains_key(&from) {
            return Vec::new();
        }
        for (id, size) in keep {
            if self.wanted.len() >= MAX_WANTED {
                break;
            }
            if self.mirror.contains_key(&id) {
                continue;
            }
            let order = &mut self.wanted_order;
            let wanted = self.wanted.entry(id).or_insert_with(|| {
                order.push_back((now, id));
                Wanted { size, listers: Vec::new(), asked: Vec::new(), first_seen: now, submitted: false }
            });
            if wanted.listers.len() < MAX_LISTERS && !wanted.listers.contains(&from) {
                wanted.listers.push(from);
                queue_ask(self.peers.get_mut(&from).expect("checked above"), id);
            }
        }
        self.request_from(from, now)
    }

    /// Ask `peer` for what it listed and nobody has been asked for, within its budgets. With
    /// `re_request`, ids already asked of other listers in this round are asked of it too.
    /// Only the peer's own queue is walked, and only as far as it asks.
    fn request_from(&mut self, peer_key: ConnectionKey, now: Instant) -> Vec<Out> {
        self.ask(peer_key, false, now)
    }

    fn ask(&mut self, peer_key: ConnectionKey, re_request: bool, now: Instant) -> Vec<Out> {
        let Some(peer) = self.peers.get_mut(&peer_key) else { return Vec::new() };
        if peer.strikes.backing_off(now) {
            return Vec::new();
        }
        let mut ask = Vec::new();
        while peer.requested.len() < MAX_IN_FLIGHT_PER_PEER && ask.len() < MAX_TXS_PER_MESSAGE {
            let Some(id) = peer.to_ask.pop_front() else { break };
            let Some(wanted) = self.wanted.get_mut(&id) else { continue };
            if wanted.submitted || wanted.asked.iter().any(|(p, _)| *p == peer_key) || !wanted.listers.contains(&peer_key) {
                continue;
            }
            // Asked of another lister and not overdue: not ours to ask. It comes back to this
            // queue if that ask goes unanswered.
            if !re_request && !wanted.asked.is_empty() {
                continue;
            }
            if peer.requested_bytes + u64::from(wanted.size) > MAX_REQUESTED_BYTES_PER_PEER {
                peer.to_ask.push_front(id);
                break;
            }
            wanted.asked.push((peer_key, now));
            self.asks.push_back((now, id));
            peer.requested.insert(id, wanted.size);
            peer.requested_bytes += u64::from(wanted.size);
            ask.push(id);
        }
        if ask.is_empty() {
            return Vec::new();
        }
        vec![Out::Send { to: peer_key, packet: tx_req_packet(&ask), bulk: false }]
    }

    /// Forget a wanted id, and free the budget its asks hold.
    fn forget_wanted(&mut self, id: UnminedTxId) {
        let Some(wanted) = self.wanted.remove(&id) else { return };
        for (peer_key, _) in wanted.asked {
            if let Some(peer) = self.peers.get_mut(&peer_key) {
                if let Some(size) = peer.requested.remove(&id) {
                    peer.requested_bytes -= u64::from(size);
                }
            }
        }
    }

    // ---- what peers deliver ----

    /// Of the transactions `from` sent, those we asked it for, which the caller then hands to
    /// the mempool. The rest were not asked for (a late answer after a re-request, or unasked
    /// pushes) and are dropped.
    pub fn on_delivered(&mut self, from: ConnectionKey, ids: &[UnminedTxId]) -> Vec<UnminedTxId> {
        let Some(peer) = self.peers.get_mut(&from) else { return Vec::new() };
        let mut take = Vec::new();
        for id in ids {
            let Some(size) = peer.requested.remove(id) else { continue };
            peer.requested_bytes -= u64::from(size);
            if let Some(wanted) = self.wanted.get_mut(id) {
                if !wanted.submitted {
                    wanted.submitted = true;
                    take.push(*id);
                }
            }
        }
        take
    }

    /// The mempool's verdict on a transaction `from` delivered.
    pub fn on_outcome(&mut self, from: ConnectionKey, id: UnminedTxId, outcome: Outcome, now: Instant) -> Vec<Out> {
        match outcome {
            // It reaches the mirror through the change stream, which forgets it as wanted.
            Outcome::Accepted => Vec::new(),
            Outcome::NotTaken => {
                self.forget_wanted(id);
                Vec::new()
            }
            Outcome::Invalid => {
                self.forget_wanted(id);
                self.peer(from, now).strikes.strike(now);
                Vec::new()
            }
        }
    }

    // ---- what peers ask of us ----

    /// A peer's `TX_REQ`: what we hold and have not just served it is fetched to be served.
    pub fn on_request(&mut self, from: ConnectionKey, ids: Vec<UnminedTxId>, now: Instant) -> Vec<Out> {
        let mirror = &self.mirror;
        let peer = self.peers.entry(from).or_insert_with(|| PeerTx::new(now));
        peer.served.retain(|_, at| now.saturating_duration_since(*at) < SERVE_REPEAT_SUPPRESS);
        let ids: Vec<UnminedTxId> = ids.into_iter().filter(|id| mirror.contains_key(id) && !peer.served.contains_key(id)).collect();
        if ids.is_empty() {
            return Vec::new();
        }
        vec![Out::Fetch { to: from, ids }]
    }

    /// The bodies the mempool returned for [`Out::Fetch`], with their sizes: those the peer's
    /// serving budget allows are served, in order.
    pub fn on_bodies(&mut self, to: ConnectionKey, bodies: &[(UnminedTxId, u32)], now: Instant) -> Vec<UnminedTxId> {
        let Some(peer) = self.peers.get_mut(&to) else { return Vec::new() };
        let elapsed = now.saturating_duration_since(peer.serve_refilled);
        let refill = (elapsed.as_millis() as u64).saturating_mul(SERVE_BYTES_PER_SEC) / 1000;
        if refill > 0 {
            peer.serve_tokens = (peer.serve_tokens + refill).min(SERVE_BURST_BYTES);
            peer.serve_refilled = now;
        }
        let mut serve = Vec::new();
        for (id, size) in bodies {
            if u64::from(*size) > peer.serve_tokens {
                break;
            }
            peer.serve_tokens -= u64::from(*size);
            peer.served.insert(*id, now);
            serve.push(*id);
        }
        serve
    }

    /// A peer's `MEMPOOL_REQ`: our whole listing, at most once per
    /// [`MEMPOOL_REQ_ANSWER_INTERVAL`] per peer, on the bulk channel.
    pub fn on_mempool_req(&mut self, from: ConnectionKey, now: Instant) -> Vec<Out> {
        let peer = self.peer(from, now);
        if peer.mempool_req_answered.is_some_and(|at| now.saturating_duration_since(at) < MEMPOOL_REQ_ANSWER_INTERVAL) {
            return Vec::new();
        }
        peer.mempool_req_answered = Some(now);
        let total = self.mirror.len() as u32;
        let listing: Vec<(&UnminedTxId, u32)> = self.listing().collect();
        listing
            .chunks(MAX_TX_IDS_PER_STATUS)
            .map(|chunk| Out::Send { to: from, packet: tx_status_packet(total, chunk.iter().copied()), bulk: true })
            .collect()
    }

    // ---- peers coming and going, and time passing ----

    pub fn on_peer_gone(&mut self, peer: ConnectionKey) {
        self.peers.remove(&peer);
        for wanted in self.wanted.values_mut() {
            wanted.listers.retain(|p| *p != peer);
            wanted.asked.retain(|(p, _)| *p != peer);
        }
    }

    /// Statuses due, re-requests of what was not delivered, giving up on what nobody delivers,
    /// and a `MEMPOOL_REQ` to each peer once per session. `live` are the connected peers.
    pub fn poll(&mut self, live: &[ConnectionKey], now: Instant) -> Vec<Out> {
        let mut out = Vec::new();
        for key in live {
            self.peer(*key, now);
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

        // Overdue asks, oldest first: the peers asked are struck (once each per poll) and lose
        // their place as listers, and every other lister is asked at once.
        let mut struck: HashSet<ConnectionKey> = HashSet::new();
        let mut retry_from: HashSet<ConnectionKey> = HashSet::new();
        while let Some(&(at, id)) = self.asks.front() {
            if now.saturating_duration_since(at) < REREQUEST_AFTER {
                break;
            }
            self.asks.pop_front();
            let Some(wanted) = self.wanted.get_mut(&id) else { continue };
            if wanted.submitted || !wanted.asked.iter().any(|(_, asked_at)| *asked_at == at) {
                continue;
            }
            let overdue: Vec<ConnectionKey> = wanted
                .asked
                .iter()
                .filter(|(_, asked_at)| now.saturating_duration_since(*asked_at) >= REREQUEST_AFTER)
                .map(|(p, _)| *p)
                .collect();
            if overdue.len() < wanted.asked.len() {
                continue; // a more recent ask is still within its time
            }
            wanted.listers.retain(|p| !overdue.contains(p));
            wanted.asked.clear();
            for lister in &wanted.listers {
                if let Some(peer) = self.peers.get_mut(lister) {
                    peer.to_ask.push_front(id);
                    retry_from.insert(*lister);
                }
            }
            for peer_key in overdue {
                if let Some(peer) = self.peers.get_mut(&peer_key) {
                    if let Some(size) = peer.requested.remove(&id) {
                        peer.requested_bytes -= u64::from(size);
                    }
                    if struck.insert(peer_key) {
                        peer.strikes.strike(now);
                    }
                }
            }
        }
        for peer_key in retry_from {
            out.extend(self.ask(peer_key, true, now));
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
                out.push(Out::Send { to: *key, packet: mempool_req_packet(), bulk: false });
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

    /// Whether `peer` is backed off for relay.
    pub fn backing_off(&self, peer: ConnectionKey, now: Instant) -> bool {
        self.peers.get(&peer).is_some_and(|p| p.strikes.backing_off(now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::new_network::tx_relay::{read_tx_req, read_tx_status};
    use crate::new_network::{PACKET_TYPE_TX_REQ, PACKET_TYPE_TX_STATUS};
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
                Out::Send { to, packet, .. } if *to == of && packet[0] == PACKET_TYPE_TX_REQ => read_tx_req(&packet[1..]),
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
    fn asks_are_budgeted_by_bytes_and_count() {
        let now = Instant::now();
        let mut relay = TxRelay::default();
        relay.poll(&[peer(1)], now);
        // 100 KB each: 2 MiB holds 20 of them, fewer than the in-flight cap.
        let outs = listed(&mut relay, peer(1), (0..100).map(|n| (id(n), 100_000)).collect(), now);
        assert_eq!(asked(&outs, peer(1)).len(), 20);
        // 1 KB each: the in-flight cap binds first.
        let mut relay = TxRelay::default();
        relay.poll(&[peer(1)], now);
        let outs = listed(&mut relay, peer(1), (0..100).map(|n| (id(n), 1_000)).collect(), now);
        assert_eq!(asked(&outs, peer(1)).len(), MAX_IN_FLIGHT_PER_PEER);
    }

    #[test]
    fn an_undelivered_ask_strikes_its_peer_and_goes_to_every_other_lister() {
        let now = Instant::now();
        let mut relay = TxRelay::default();
        let peers = [peer(1), peer(2), peer(3)];
        relay.poll(&peers, now);
        let outs = listed(&mut relay, peer(1), vec![(id(7), 500)], now);
        assert_eq!(asked(&outs, peer(1)), vec![id(7)]);
        // Later listers wait while the first ask is within its time.
        for p in [peer(2), peer(3)] {
            assert!(asked(&listed(&mut relay, p, vec![(id(7), 500)], now), p).is_empty());
        }
        assert!(asked(&relay.poll(&peers, now + REREQUEST_AFTER / 2), peer(2)).is_empty());

        let later = now + REREQUEST_AFTER;
        let outs = relay.poll(&peers, later);
        assert_eq!(asked(&outs, peer(2)), vec![id(7)]);
        assert_eq!(asked(&outs, peer(3)), vec![id(7)]);
        assert!(relay.backing_off(peer(1), later));
        // A struck peer is neither heard nor asked while it backs off.
        assert!(relay.on_status(peer(1), vec![(id(8), 500)], later).is_empty());
    }

    #[test]
    fn a_delivery_is_taken_once_and_only_if_asked() {
        let now = Instant::now();
        let mut relay = TxRelay::default();
        relay.poll(&[peer(1), peer(2)], now);
        listed(&mut relay, peer(1), vec![(id(1), 500)], now);
        assert_eq!(relay.on_delivered(peer(2), &[id(1)]), vec![], "not asked of peer 2");
        assert_eq!(relay.on_delivered(peer(1), &[id(1), id(9)]), vec![id(1)]);
        assert_eq!(relay.on_delivered(peer(1), &[id(1)]), vec![], "already taken");
    }

    #[test]
    fn only_an_invalid_verdict_strikes() {
        let now = Instant::now();
        let mut relay = TxRelay::default();
        relay.poll(&[peer(1), peer(2)], now);
        for (p, n) in [(peer(1), 1), (peer(2), 2)] {
            listed(&mut relay, p, vec![(id(n), 500)], now);
            relay.on_delivered(p, &[id(n)]);
        }
        relay.on_outcome(peer(1), id(1), Outcome::NotTaken, now);
        relay.on_outcome(peer(2), id(2), Outcome::Invalid, now);
        assert!(!relay.backing_off(peer(1), now));
        assert!(relay.backing_off(peer(2), now));
    }

    #[test]
    fn what_nobody_delivers_is_given_up_and_frees_the_budget() {
        let now = Instant::now();
        let mut relay = TxRelay::default();
        relay.poll(&[peer(1)], now);
        listed(&mut relay, peer(1), vec![(id(1), 100_000)], now);
        relay.poll(&[peer(1)], now + WANTED_GIVE_UP);
        assert!(relay.wanted.is_empty());
        assert_eq!(relay.peers[&peer(1)].requested_bytes, 0);
    }

    #[test]
    fn serving_is_limited_by_a_byte_bucket_and_repeats_are_suppressed() {
        let now = Instant::now();
        let mut relay = TxRelay::default();
        relay.on_added((0..40).map(|n| (id(n), 100_000)));
        let ids: Vec<UnminedTxId> = (0..40).map(id).collect();
        assert!(matches!(&relay.on_request(peer(1), ids.clone(), now)[..], [Out::Fetch { .. }]));
        let bodies: Vec<(UnminedTxId, u32)> = ids.iter().map(|i| (*i, 100_000)).collect();
        let served = relay.on_bodies(peer(1), &bodies, now);
        assert_eq!(served.len() as u64, SERVE_BURST_BYTES / 100_000);
        // What was just served is not served again, and ids we do not hold are not fetched.
        assert!(relay.on_request(peer(1), served, now).is_empty());
        assert!(relay.on_request(peer(1), vec![id(999)], now).is_empty());
    }

    #[test]
    fn a_mempool_request_is_answered_in_full_at_most_once_per_interval() {
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
}
