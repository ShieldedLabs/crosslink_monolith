//! Receiver-selected evidence service for the standalone guided prototype.
//!
//! A peer-specific `Some(first_status_sequence)` allows at most one ACCEPTED
//! complete-frame handoff. Refused handoffs may be retried within that lease.
//! Subsequent status heartbeats keep the token; `None` cancels it. The
//! host authenticates peers and supplies connection generations locally. Only
//! the token is echoed on the wire: endpoint-local peer handles/generations
//! need not agree. Requests and statuses are scheduling hints, never evidence.
//!
//! There are THREE independent bounds: receiver nominations, owned receive
//! slots, and owned outgoing slots. Cancellation, expiry and reconnect change
//! scheduling metadata only. An admitted late/unsolicited reply is still fed
//! through ordinary `Engine::receive` validation, without requiring a current
//! request. Neither an ACK nor `receive` returning `Ok` establishes progress.
//!
//! `AdmissionBudget` bounds only allocations whose ownership retains a permit.
//! Acquire before allocating/reassembling, retain through processing and the
//! last owned copy, and charge each additional copy separately. Slots are NOT
//! a byte/RSS bound: a host must also cap aggregate decoded backing storage
//! (including sparse rows), serialization, queues and transport copies. This
//! module implements no codec, STP integration, durable state or wire admission.
//!
//! The host admits a fixed authenticated roster, preserves status sequences,
//! polls fairly and uses fresh larger generations on reconnect. `remove`
//! forgets a peer's replay/fairness record; old-session traffic must thereafter
//! be rejected by the host, and unbounded remove/reinsert churn is outside the
//! fairness contract. Source selection never infers actual certificate custody:
//! priority uses hints, while oldest-attempt turns explore ALL fresh compatible
//! peers. Complete frames still require the sender's actual retained-state gap.
use super::{Gap, StatusSnapshot, StatusUpdate, UpdateError, step_rank};
use crate::d28::{Engine, Error, Frame, Group, Proposal, Result, Sink, Step, Storage, Target};
use std::cmp::Reverse;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

/// Inbound control event. `update.peer/generation` are supplied by the host,
/// not trusted fields decoded from a remote status payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RequestStatus {
    pub update: StatusUpdate,
    /// First status sequence of this attempt, stable across newer heartbeats.
    pub request: Option<u64>,
}
/// Local association key. At the receiver, `peer` is the source; at the sender,
/// it is the requester. A transport echoes only `sequence` and rebinds the key
/// to its authenticated source/current local generation on delivery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RequestKey {
    pub peer: u32,
    pub generation: u64,
    pub sequence: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReceiverConfig {
    pub max_peers: usize,
    pub max_requests: usize,
    pub request_timeout: u64,
    pub status_ttl: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SenderConfig {
    pub max_peers: usize,
    pub max_outgoing: usize,
    pub request_timeout: u64,
    pub status_ttl: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequestUpdateError {
    Stale,
    Full,
    /// A request cannot originate in a future status sequence.
    InvalidRequest,
    Exhausted,
}

#[derive(Debug)]
struct BudgetState {
    capacity: usize,
    used: AtomicUsize,
    peak: AtomicUsize,
}
/// Shareable counter, not a payload queue. Cloning the budget shares its limit.
#[derive(Clone, Debug)]
pub struct AdmissionBudget(Arc<BudgetState>);
/// One physical ownership slot. Not Clone; dropping releases exactly this slot,
/// independently of all request lifetimes and even of the scheduler's lifetime.
#[derive(Debug)]
pub struct Permit(Arc<BudgetState>);
impl AdmissionBudget {
    pub fn new(capacity: usize) -> Self {
        Self(Arc::new(BudgetState {
            capacity,
            used: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        }))
    }
    pub fn try_acquire(&self) -> Option<Permit> {
        let old = self
            .0
            .used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < self.0.capacity).then(|| n + 1)
            })
            .ok()?;
        self.0.peak.fetch_max(old + 1, Ordering::Relaxed);
        Some(Permit(Arc::clone(&self.0)))
    }
    pub fn capacity(&self) -> usize {
        self.0.capacity
    }
    pub fn in_use(&self) -> usize {
        self.0.used.load(Ordering::Acquire)
    }
    pub fn high_water(&self) -> usize {
        self.0.peak.load(Ordering::Relaxed)
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        self.0.used.fetch_sub(1, Ordering::AcqRel);
    }
}

#[derive(Clone, Copy)]
struct Selection {
    key: RequestKey,
    baseline: StatusSnapshot,
    deadline: u64,
}
struct Source {
    update: StatusUpdate,
    received: u64,
    last_attempt: u64,
    failures: u32,
    retry_at: u64,
    selection: Option<Selection>,
}
/// O(1) scalar words per source and at most `max_requests` logical selections.
/// This owns no payloads and does not reserve physical receive slots.
pub struct Receiver {
    cfg: ReceiverConfig,
    sources: Vec<Source>,
    clock: u64,
    sequence: Option<u64>,
    serial: u64,
    fair_next: bool,
}
impl Receiver {
    pub fn try_new(cfg: ReceiverConfig) -> Option<Self> {
        (cfg.max_peers > 0 && cfg.max_requests > 0 && cfg.request_timeout > 0 && cfg.status_ttl > 0)
            .then(|| Self {
                cfg,
                sources: Vec::with_capacity(cfg.max_peers),
                clock: 0,
                sequence: None,
                serial: 0,
                fair_next: false,
            })
    }
    pub fn peers(&self) -> usize {
        self.sources.len()
    }
    pub fn active_requests(&self) -> usize {
        self.sources
            .iter()
            .filter(|p| p.selection.is_some())
            .count()
    }
    pub fn requests(&self) -> impl Iterator<Item = RequestKey> + '_ {
        self.sources
            .iter()
            .filter_map(|p| p.selection.map(|s| s.key))
    }
    pub fn request_for(&self, peer: u32) -> Option<u64> {
        self.sources
            .iter()
            .find(|p| p.update.peer == peer)?
            .selection
            .map(|s| s.key.sequence)
    }
    /// Classification only, never a receive permission or a progress signal.
    pub fn is_current(&self, key: RequestKey, now: u64) -> bool {
        self.sources.iter().any(|p| {
            p.selection
                .is_some_and(|s| s.key == key && self.clock.max(now) < s.deadline)
        })
    }
    pub fn remove(&mut self, peer: u32) {
        self.sources.retain(|p| p.update.peer != peer);
    }
    pub fn update(&mut self, u: StatusUpdate, now: u64) -> std::result::Result<(), UpdateError> {
        self.clock = self.clock.max(now);
        if let Some(p) = self.sources.iter_mut().find(|p| p.update.peer == u.peer) {
            if (u.generation, u.sequence) <= (p.update.generation, p.update.sequence) {
                return Err(UpdateError::Stale);
            }
            if u.generation != p.update.generation {
                // Fairness age survives a trusted reconnect, unlike its request.
                p.selection = None;
            }
            p.update = u;
            p.received = self.clock;
            return Ok(());
        }
        if self.sources.len() >= self.cfg.max_peers {
            return Err(UpdateError::Full);
        }
        self.sources.push(Source {
            update: u,
            received: self.clock,
            last_attempt: 0,
            failures: 0,
            retry_at: 0,
            selection: None,
        });
        Ok(())
    }
    /// Supply a fresh, strictly increasing LOCAL outbound status sequence each
    /// call, then advertise `local` with that sequence and `request_for(peer)`
    /// separately to each destination. The host stamps the REQUESTER identity
    /// on receipt; a destination handle is not the advertisement's source.
    ///
    /// Returns only newly started selections. Changes in local semantic facts
    /// may cancel old requests; broadcast the resulting flags even when this
    /// vector is empty. Repeated equal status/ACKs do not extend any deadline.
    pub fn poll(
        &mut self,
        local: StatusSnapshot,
        sequence: u64,
        now: u64,
    ) -> Result<Vec<RequestKey>> {
        if self.sequence.is_some_and(|old| sequence <= old) {
            return Err(Error::Stale);
        }
        self.clock = self.clock.max(now);
        let now = self.clock;
        // Check before producing any request. Never mint an already-expired
        // lease at the edge of the host's finite clock domain.
        let deadline = now
            .checked_add(self.cfg.request_timeout)
            .ok_or(Error::Exhausted)?;
        self.sequence = Some(sequence);
        for p in &mut self.sources {
            let Some(s) = p.selection else { continue };
            if semantic_progress(s.baseline, local) {
                p.selection = None;
                p.failures = 0;
                p.retry_at = 0;
            } else if now >= s.deadline {
                let failures = p.failures.saturating_add(1);
                let delay = self
                    .cfg
                    .request_timeout
                    .checked_mul(1 << failures.min(6))
                    .ok_or(Error::Exhausted)?;
                let retry_at = now.checked_add(delay).ok_or(Error::Exhausted)?;
                p.selection = None;
                p.failures = failures;
                p.retry_at = retry_at;
            } else if !compatible(local, p.update.status)
                || now.saturating_sub(p.received) > self.cfg.status_ttl
            {
                p.selection = None;
            }
        }
        let mut started = Vec::new();
        if local.step == Step::Decided {
            return Ok(started);
        }
        while self.active_requests() < self.cfg.max_requests {
            let next = self
                .sources
                .iter()
                .enumerate()
                .filter(|(_, p)| {
                    p.selection.is_none()
                        && now >= p.retry_at
                        && now.saturating_sub(p.received) <= self.cfg.status_ttl
                        && compatible(local, p.update.status)
                })
                .min_by_key(|(_, p)| {
                    let rank = source_hint(local, p.update.status).map_or(0, Gap::rank);
                    (
                        Reverse(if self.fair_next { 0 } else { rank }),
                        if self.fair_next { 0 } else { p.failures },
                        p.last_attempt,
                        p.failures,
                        p.update.peer,
                    )
                })
                .map(|(i, _)| i);
            let Some(i) = next else { break };
            let serial = self.serial.checked_add(1).ok_or(Error::Exhausted)?;
            let p = &mut self.sources[i];
            let key = RequestKey {
                peer: p.update.peer,
                generation: p.update.generation,
                sequence,
            };
            p.selection = Some(Selection {
                key,
                baseline: local,
                deadline,
            });
            p.last_attempt = serial;
            self.serial = serial;
            self.fair_next = !self.fair_next;
            started.push(key);
        }
        Ok(started)
    }
}
fn compatible(a: StatusSnapshot, b: StatusSnapshot) -> bool {
    a.config == b.config && a.height == b.height
}
// Only locally observed semantic gains (or retirement of the entire scope).
// This is not attribution of success to a chosen source: unsolicited evidence,
// local votes and validity completions can also make these facts true.
fn semantic_progress(old: StatusSnapshot, new: StatusSnapshot) -> bool {
    !compatible(old, new)
        || new.step == Step::Decided
        || new.round > old.round
        || new.round == old.round
            && (step_rank(new.step) > step_rank(old.step)
                || old.proposal.is_none() && new.proposal.is_some()
                || !old.prevote_quorum && new.prevote_quorum
                || !old.justified && new.justified && old.proposal == new.proposal
                || new.polka.is_some() && new.polka != old.polka)
}
fn source_hint(local: StatusSnapshot, source: StatusSnapshot) -> Option<Gap> {
    if source.step == Step::Decided {
        Some(Gap::Decision)
    } else if source.round > local.round
        || local.step == Step::PreRound && source.step != Step::PreRound
    {
        Some(Gap::Round)
    } else if source.round != local.round {
        None
    } else if source.polka.is_some_and(|(r, _)| r == source.round) && source.polka != local.polka {
        Some(Gap::Polka)
    } else if !local.prevote_quorum && source.prevote_quorum {
        Some(Gap::Votes)
    } else if local.proposal.is_none() && source.proposal.is_some()
        || local.proposal.is_some()
            && local.proposal == source.proposal
            && !local.justified
            && (source.justified || source.polka.is_some())
    {
        Some(Gap::Proposal)
    } else if step_rank(source.step) > step_rank(local.step) {
        Some(Gap::Votes)
    } else {
        None
    }
}

#[derive(Clone, Copy)]
struct Attempt {
    token: u64,
    deadline: u64,
    offered: bool,
}
struct Requester {
    update: StatusUpdate,
    received: u64,
    /// Tokens <= this sequence may not start another attempt. A false status
    /// closes even unseen older tokens, not just one previously observed id.
    closed_through: Option<u64>,
    attempt: Option<Attempt>,
    last_attempt: u64,
}
/// Synchronous handoff. On `Ok`, the host owns the permit and retains it with
/// every charged outgoing allocation until those bytes are gone. On refusal,
/// return the SAME permit and retain no payload; the responder drops it. The
/// borrowed `Frame` must not escape this call. This is not a transport ACK.
pub trait ReplySink {
    fn send(
        &mut self,
        key: RequestKey,
        frame: Frame<'_>,
        permit: Permit,
    ) -> std::result::Result<(), Permit>;
}
/// Request-gated sender. One accepted offer per token; no frame queue. Fresh
/// status snapshots update the actual serve plan without refreshing the lease.
///
/// `try_new` creates a NEW outgoing budget. Before replacing/dropping this
/// scheduler and constructing another, drain all prior outgoing payload owners.
/// Old permits remain charged to their original pool, not to the replacement.
/// Reconnects and `remove` on this SAME instance preserve its outgoing pool.
/// A handoff permits one owned complete-frame copy, not uncharged duplicates.
pub struct Responder {
    cfg: SenderConfig,
    peers: Vec<Requester>,
    clock: u64,
    serial: u64,
    fair_next: bool,
    outgoing: AdmissionBudget,
}
impl Responder {
    pub fn try_new(cfg: SenderConfig) -> Option<Self> {
        (cfg.max_peers > 0 && cfg.max_outgoing > 0 && cfg.request_timeout > 0 && cfg.status_ttl > 0)
            .then(|| Self {
                cfg,
                peers: Vec::with_capacity(cfg.max_peers),
                clock: 0,
                serial: 0,
                fair_next: false,
                outgoing: AdmissionBudget::new(cfg.max_outgoing),
            })
    }
    pub fn peers(&self) -> usize {
        self.peers.len()
    }
    pub fn active_requests(&self) -> usize {
        self.peers.iter().filter(|p| p.attempt.is_some()).count()
    }
    pub fn outstanding(&self) -> usize {
        self.outgoing.in_use()
    }
    pub fn remove(&mut self, peer: u32) {
        self.peers.retain(|p| p.update.peer != peer);
    }
    pub fn update(
        &mut self,
        message: RequestStatus,
        now: u64,
    ) -> std::result::Result<(), RequestUpdateError> {
        let u = message.update;
        if message.request.is_some_and(|token| token > u.sequence) {
            return Err(RequestUpdateError::InvalidRequest);
        }
        self.clock = self.clock.max(now);
        let now = self.clock;
        let existing = self.peers.iter().position(|p| p.update.peer == u.peer);
        if let Some(i) = existing {
            let p = &self.peers[i];
            if (u.generation, u.sequence) <= (p.update.generation, p.update.sequence) {
                return Err(RequestUpdateError::Stale);
            }
        } else if self.peers.len() >= self.cfg.max_peers {
            return Err(RequestUpdateError::Full);
        }
        // Validate finite lifetime before changing a peer record. Same-token
        // heartbeats need no new deadline, even near clock exhaustion.
        let needs_deadline = message.request.is_some_and(|token| {
            existing.is_none_or(|i| {
                let p = &self.peers[i];
                u.generation != p.update.generation
                    || !p.closed_through.is_some_and(|closed| token <= closed)
                        && p.attempt.is_none_or(|a| a.token != token)
            })
        });
        let deadline = if needs_deadline {
            Some(
                now.checked_add(self.cfg.request_timeout)
                    .ok_or(RequestUpdateError::Exhausted)?,
            )
        } else {
            None
        };
        let i = if let Some(i) = existing {
            i
        } else {
            self.peers.push(Requester {
                update: u,
                received: now,
                closed_through: None,
                attempt: None,
                last_attempt: 0,
            });
            self.peers.len() - 1
        };
        let p = &mut self.peers[i];
        if u.generation != p.update.generation {
            p.attempt = None;
            p.closed_through = None;
        }
        expire(p, now);
        p.update = u;
        p.received = now;
        match message.request {
            None => {
                p.closed_through = Some(p.closed_through.map_or(u.sequence, |x| x.max(u.sequence)));
                p.attempt = None;
            }
            Some(token) => {
                if p.closed_through.is_some_and(|closed| token <= closed) {
                    // Accept the newer semantic status, but never resurrect an
                    // ended token. Retiring a newer live token also closes it,
                    // or a subsequent heartbeat could reopen its offered bit.
                    if let Some(old) = p.attempt.take() {
                        p.closed_through = Some(p.closed_through.unwrap().max(old.token));
                    }
                } else if p.attempt.is_none_or(|a| a.token != token) {
                    if let Some(old) = p.attempt {
                        if token < old.token {
                            p.closed_through = Some(old.token);
                            p.attempt = None;
                            return Ok(());
                        }
                        p.closed_through = Some(old.token);
                    }
                    p.attempt = Some(Attempt {
                        token,
                        deadline: deadline.expect("new attempt deadline"),
                        offered: false,
                    });
                }
            }
        }
        Ok(())
    }
    pub fn poll<S: Storage>(
        &mut self,
        engine: &mut Engine<S>,
        now: u64,
        sink: &mut impl ReplySink,
    ) -> Result<Vec<(RequestKey, Gap)>> {
        engine.check()?;
        self.clock = self.clock.max(now);
        let now = self.clock;
        for p in &mut self.peers {
            expire(p, now);
        }
        let mut tried = Vec::with_capacity(self.cfg.max_outgoing);
        let mut started = Vec::new();
        // Even a refusing sink sees at most max_outgoing attempts per poll.
        while tried.len() < self.cfg.max_outgoing {
            let best = self
                .peers
                .iter()
                .enumerate()
                .filter_map(|(i, p)| {
                    let a = p.attempt?;
                    if a.offered
                        || tried.contains(&i)
                        || now.saturating_sub(p.received) > self.cfg.status_ttl
                    {
                        return None;
                    }
                    let gap = engine.gap_for(&p.update.status)?;
                    Some((i, gap))
                })
                .min_by_key(|(i, gap)| {
                    let p = &self.peers[*i];
                    (
                        Reverse(if self.fair_next { 0 } else { gap.rank() }),
                        p.last_attempt,
                        p.update.peer,
                    )
                });
            let Some((i, gap)) = best else { break };
            let serial = self.serial.checked_add(1).ok_or(Error::Exhausted)?;
            let Some(permit) = self.outgoing.try_acquire() else {
                break;
            };
            tried.push(i);
            let p = &mut self.peers[i];
            let key = RequestKey {
                peer: p.update.peer,
                generation: p.update.generation,
                sequence: p.attempt.expect("selected attempt").token,
            };
            p.last_attempt = serial;
            self.serial = serial;
            self.fair_next = !self.fair_next;
            let mut adapter = Handoff {
                key,
                permit: Some(permit),
                sink,
                accepted: false,
            };
            let result = engine.serve(p.update.peer, &p.update.status, &mut adapter);
            if adapter.accepted {
                p.attempt.as_mut().expect("selected attempt").offered = true;
                started.push((key, gap));
            }
            result?;
        }
        Ok(started)
    }
}
fn expire(p: &mut Requester, now: u64) {
    if let Some(a) = p.attempt {
        if now >= a.deadline {
            p.closed_through = Some(p.closed_through.map_or(a.token, |x| x.max(a.token)));
            p.attempt = None;
        }
    }
}
struct Handoff<'a, S: ReplySink> {
    key: RequestKey,
    permit: Option<Permit>,
    sink: &'a mut S,
    accepted: bool,
}
impl<S: ReplySink> Sink for Handoff<'_, S> {
    fn send(&mut self, _target: Target, frame: Frame<'_>) {
        if let Some(permit) = self.permit.take() {
            match self.sink.send(self.key, frame, permit) {
                Ok(()) => self.accepted = true,
                Err(permit) => drop(permit),
            }
        }
    }
    fn decided(&mut self, _: u64, _: Proposal<'_>, _: Group<'_>) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    fn status() -> StatusSnapshot {
        StatusSnapshot {
            config: [3; 32],
            height: 4,
            round: 0,
            step: Step::Propose,
            proposal: None,
            polka: None,
            prevote_quorum: false,
            justified: false,
        }
    }
    fn update(peer: u32, sequence: u64) -> StatusUpdate {
        StatusUpdate {
            peer,
            generation: 1,
            sequence,
            status: status(),
        }
    }
    fn receiver() -> Receiver {
        Receiver::try_new(ReceiverConfig {
            max_peers: 8,
            max_requests: 1,
            request_timeout: 2,
            status_ttl: 1000,
        })
        .unwrap()
    }
    #[test]
    fn budgets_share_capacity_and_outlive_handles() {
        let budget = AdmissionBudget::new(1);
        let clone = budget.clone();
        let permit = budget.try_acquire().unwrap();
        assert!(clone.try_acquire().is_none());
        drop(budget);
        assert_eq!(clone.in_use(), 1);
        drop(permit);
        assert_eq!(clone.in_use(), 0);
        assert_eq!(clone.high_water(), 1);
    }
    #[test]
    fn heartbeats_expire_and_all_sources_get_fair_attempts() {
        let mut r = receiver();
        for i in 0..4 {
            r.update(update(i, 0), 0).unwrap();
        }
        let first = r.poll(status(), 1, 0).unwrap()[0];
        assert_eq!(first.peer, 0);
        assert!(r.poll(status(), 2, 1).unwrap().is_empty());
        assert_eq!(r.request_for(0), Some(1));
        let second = r.poll(status(), 3, 2).unwrap()[0];
        assert_ne!(first.peer, second.peer);
        assert!(!r.is_current(first, 2));
        let mut seen = vec![first.peer, second.peer];
        for tick in 3..12 {
            for i in 0..4 {
                r.update(update(i, tick), tick).unwrap();
            }
            seen.extend(
                r.poll(status(), tick + 1, tick)
                    .unwrap()
                    .iter()
                    .map(|k| k.peer),
            );
        }
        for i in 0..4 {
            assert!(seen.contains(&i));
        }
    }
    #[test]
    fn ended_tokens_cannot_be_resurrected_by_newer_statuses() {
        let mut sender = Responder::try_new(SenderConfig {
            max_peers: 2,
            max_outgoing: 1,
            request_timeout: 5,
            status_ttl: 20,
        })
        .unwrap();
        let put = |s: &mut Responder, seq, token, now| {
            s.update(
                RequestStatus {
                    update: update(1, seq),
                    request: token,
                },
                now,
            )
            .unwrap()
        };
        put(&mut sender, 7, Some(7), 0);
        put(&mut sender, 8, None, 1);
        put(&mut sender, 9, Some(7), 2);
        assert_eq!(sender.active_requests(), 0);
        put(&mut sender, 10, Some(10), 3);
        put(&mut sender, 11, Some(10), 7);
        assert_eq!(sender.peers[0].attempt.unwrap().deadline, 8);
        put(&mut sender, 12, Some(10), 8);
        assert_eq!(sender.active_requests(), 0);
        put(&mut sender, 13, Some(10), 9);
        assert_eq!(sender.active_requests(), 0);
        put(&mut sender, 14, Some(14), 9);
        assert_eq!(sender.active_requests(), 1);
    }
    #[test]
    fn replayed_closed_token_also_retires_the_newer_live_token() {
        let mut sender = Responder::try_new(SenderConfig {
            max_peers: 2,
            max_outgoing: 1,
            request_timeout: 20,
            status_ttl: 30,
        })
        .unwrap();
        let put = |s: &mut Responder, seq, token| {
            s.update(
                RequestStatus {
                    update: update(1, seq),
                    request: token,
                },
                0,
            )
            .unwrap()
        };
        put(&mut sender, 7, Some(7));
        put(&mut sender, 8, None);
        put(&mut sender, 20, Some(20));
        sender.peers[0].attempt.as_mut().unwrap().offered = true;
        put(&mut sender, 21, Some(7));
        assert_eq!(sender.active_requests(), 0);
        put(&mut sender, 22, Some(20));
        assert_eq!(
            sender.active_requests(),
            0,
            "a retired offered token must never reopen"
        );
    }
    #[test]
    fn local_progress_and_reconnect_cancel_only_metadata() {
        let mut r = receiver();
        r.update(update(1, 0), 0).unwrap();
        let old = r.poll(status(), 1, 0).unwrap()[0];
        let budget = AdmissionBudget::new(1);
        let permit = budget.try_acquire().unwrap();
        let mut advanced = status();
        advanced.prevote_quorum = true;
        let fresh = r.poll(advanced, 2, 1).unwrap()[0];
        assert_ne!(old, fresh);
        assert_eq!(budget.in_use(), 1);
        r.update(
            StatusUpdate {
                generation: 2,
                ..update(1, 0)
            },
            1,
        )
        .unwrap();
        assert_eq!(r.active_requests(), 0);
        assert_eq!(budget.in_use(), 1);
        drop(permit);
        assert_eq!(budget.in_use(), 0);
    }
    #[test]
    fn sequence_clock_and_finite_arithmetic_fail_closed() {
        let mut r = receiver();
        r.update(update(1, 0), 5).unwrap();
        r.poll(status(), 0, 4).unwrap();
        assert_eq!(r.sources[0].selection.unwrap().deadline, 7);
        assert_eq!(r.poll(status(), 0, 6), Err(Error::Stale));
        assert_eq!(r.poll(status(), 1, u64::MAX), Err(Error::Exhausted));
        let mut r = receiver();
        r.update(update(1, 0), 0).unwrap();
        r.serial = u64::MAX;
        assert_eq!(r.poll(status(), 1, 0), Err(Error::Exhausted));
        assert_eq!(r.active_requests(), 0);
    }
}
