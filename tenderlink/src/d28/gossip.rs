//! Status-directed evidence service: a bounded TARGETING building block.
//!
//! A host learns each peer's latest semantic status (an UNTRUSTED hint, never a
//! vote or certificate), asks [`Engine::serve`] what that peer is missing, and
//! the engine emits at most ONE complete, already-retained reference frame to
//! that peer. [`Director`] is the optional host-side scheduler: it keeps O(1)
//! words per peer, at most `max_grants` (K) outstanding interactions regardless
//! of the peer count N, and prefers state-changing gaps. There is no sweep:
//! a peer is only served when its status shows a concrete gap the local node can
//! fill from retained state.
//!
//! Scope. Under `Emission::Guided` this is the only source of complete frames;
//! otherwise it does not replace the periodic rho replay or `CompactLive`
//! fallback and nothing here claims liveness for guided-only gossip. Receivers
//! still validate every frame as a whole through `Engine::receive`; the helper
//! never fabricates a threshold, copies source votes, or declares an external
//! commit. It cannot promise a transition when the receiver lacks external
//! validity of a body. Status and sequencing are volatile (not persisted): a
//! host restart starts with an empty table, and a fresh transport session must
//! use a fresh, larger `generation`.
use super::*;
use std::collections::HashMap;

pub mod receiver;

/// The state-changing gap a frame was chosen to close, strongest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gap {
    /// Peer has not decided; we hold the complete DEC package.
    Decision,
    /// Peer is in a lower round (or not started); we hold ENTER + entry cert.
    Round,
    /// Peer lacks the current round's proposal body.
    Proposal,
    /// Peer lacks the polka (valid-round) for the current round.
    Polka,
    /// Peer is behind in step; our complete vote carries the row evidence.
    Votes,
}
impl Gap {
    fn rank(self) -> u8 {
        match self {
            Gap::Decision => 5,
            Gap::Round => 4,
            Gap::Polka => 3,
            Gap::Proposal => 2,
            Gap::Votes => 1,
        }
    }
}
/// Why nothing was sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Skip {
    /// Different configuration: fail closed.
    Foreign,
    /// Peer is at a previous height: host archive catch-up, not this service.
    Archive,
    /// Peer is at a later height.
    Ahead,
    /// No gap that retained evidence can close.
    NoGap,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Served {
    /// One complete frame was handed to the sink for the peer. Not a claim that
    /// the peer processed it.
    Sent(Gap),
    Skipped(Skip),
}
/// Constant-size semantic status. Untrusted hint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatusSnapshot {
    pub config: Id,
    pub height: u64,
    pub round: Round,
    pub step: Step,
    /// Body id of the current round's proposal held, if any.
    pub proposal: Option<Id>,
    /// Round and body id of the held valid-round polka, if any.
    pub polka: Option<(Round, Id)>,
    /// A quorum of current-round prevotes (any ids) is held, i.e. the prevote
    /// row is complete enough to arm the prevote timeout. Constant size.
    pub prevote_quorum: bool,
    /// The held current proposal (`proposal`) is justified: its valid-round
    /// polka was seen (or it needs none). Bound to that proposal; false when
    /// no proposal is held.
    pub justified: bool,
}
fn step_rank(s: Step) -> u8 {
    match s {
        Step::PreRound => 0,
        Step::Propose => 1,
        Step::Prevote => 2,
        Step::Precommit => 3,
        Step::Decided => 4,
    }
}
#[derive(Clone, Copy)]
enum Plan {
    Dec,
    Enter,
    Proposal,
    Polka,
    Precommit,
    Prevote,
}
impl<S: Storage> Engine<S> {
    /// The status to advertise to peers (None before init / after sealing).
    pub fn status(&self) -> Option<StatusSnapshot> {
        let d = self.view()?;
        let polka = match (d.valid_round, d.vc) {
            (Some(r), Some(vc)) => Some((r, self.proposal(vc.proposal).header.content.id)),
            _ => None,
        };
        Some(StatusSnapshot {
            config: d.config,
            height: d.height,
            round: d.round,
            step: d.step,
            proposal: d.slot.map(|s| self.proposal(s).header.content.id),
            polka,
            prevote_quorum: self.c().quorum(self.row_weight(VoteKind::Prevote, None)),
            justified: d.slot.is_some_and(|s| {
                self.proposal(s).header.content.valid_round.is_none() || d.justified
            }),
        })
    }
    /// For a lower-round peer: current self-carrying evidence jumps it straight
    /// into our round (no predecessor-round TPC delay); else ENTER + entry
    /// certificate. Guards check the actual retained quorum (recovery rebuilds
    /// only own rows), so a frame is never chosen on a bare vote's say-so.
    fn round_plan(&self) -> Plan {
        let d = self.d();
        let pv_quorum = self.c().quorum(self.row_weight(VoteKind::Prevote, None));
        if let (Some(v), Some(s), Some(vc)) = (self.own_current(VoteKind::Precommit), d.slot, d.vc)
        {
            let id = self.proposal(s).header.content.id;
            if v.content.id == Some(id)
                && d.valid_round == Some(d.round)
                && self.proposal(vc.proposal).header.content.id == id
            {
                return Plan::Precommit;
            }
        }
        // Recovery may have cleared the volatile row after a late polka
        // advanced VC without changing an already-issued nil precommit.
        // The retained certificate is still a complete current-round bridge.
        if d.valid_round == Some(d.round) && d.vc.is_some() {
            return Plan::Polka;
        }
        if pv_quorum && self.own_current(VoteKind::Prevote).is_some() {
            return Plan::Prevote;
        }
        if pv_quorum
            && self
                .own_current(VoteKind::Precommit)
                .is_some_and(|v| v.content.id.is_none())
        {
            return Plan::Precommit;
        }
        Plan::Enter
    }
    fn plan(&self, st: &StatusSnapshot) -> std::result::Result<(Gap, Plan), Skip> {
        let d = self.d();
        if st.config != d.config {
            return Err(Skip::Foreign);
        }
        if st.height < d.height {
            return Err(Skip::Archive);
        }
        if st.height > d.height {
            return Err(Skip::Ahead);
        }
        if st.step == Step::Decided || d.step == Step::PreRound {
            return Err(Skip::NoGap);
        }
        if d.step == Step::Decided {
            return if d.pending.is_some() {
                Ok((Gap::Decision, Plan::Dec))
            } else {
                Err(Skip::NoGap)
            };
        }
        if d.entry.is_some()
            && self.own_current(VoteKind::Enter).is_some()
            && (st.step == Step::PreRound || st.round < d.round)
        {
            return Ok((Gap::Round, self.round_plan()));
        }
        if st.round != d.round || st.step == Step::PreRound {
            return Err(Skip::NoGap);
        }
        let slot = d.slot.map(|s| self.proposal(s));
        // 1. Current-round polka package (body + VC) when the peer does not
        // hold the matching polka, even if it also lacks the body. State
        // changing, so it precedes proposal-only service.
        if let (Some(vr), Some(vc)) = (d.valid_round, d.vc) {
            let id = self.proposal(vc.proposal).header.content.id;
            if vr == d.round && st.polka != Some((vr, id)) {
                let pc = self.own_current(VoteKind::Precommit);
                let slot_id = slot.as_ref().map(|p| p.header.content.id);
                let plan = if pc.is_some_and(|v| v.content.id == Some(id)) && slot_id == Some(id) {
                    Plan::Precommit
                } else {
                    Plan::Polka
                };
                return Ok((Gap::Polka, plan));
            }
        }
        // 2. Our full PV row when the peer lacks a prevote quorum (any step).
        if !st.prevote_quorum
            && self.c().quorum(self.row_weight(VoteKind::Prevote, None))
            && self.own_current(VoteKind::Prevote).is_some()
        {
            return Ok((Gap::Votes, Plan::Prevote));
        }
        // 3. Proposal body, or the exact valid-round certificate of a held but
        // unjustified proposal.
        if let Some(p) = slot {
            let servable = p.header.content.round == d.round;
            if st.proposal.is_none() && servable {
                return Ok((Gap::Proposal, Plan::Proposal));
            }
            if let (Some(vr), Some(vc)) = (p.header.content.valid_round, d.vc) {
                if d.valid_round == Some(vr)
                    && vr < d.round
                    && self.proposal(vc.proposal).header.content.id == p.header.content.id
                    && st.proposal == Some(p.header.content.id)
                    && !st.justified
                {
                    return Ok((Gap::Proposal, Plan::Proposal));
                }
            }
        }
        let mine = step_rank(d.step);
        let theirs = step_rank(st.step);
        if mine >= 3 && theirs < 3 && self.own_current(VoteKind::Precommit).is_some() {
            return Ok((Gap::Votes, Plan::Precommit));
        }
        if mine >= 2 && theirs < 2 && self.own_current(VoteKind::Prevote).is_some() {
            return Ok((Gap::Votes, Plan::Prevote));
        }
        // Same-step missing row: not every individual missing vote, only when
        // we hold a prevote quorum row and the peer (already prevoting) lacks one.
        if mine >= 2
            && theirs >= 2
            && !st.prevote_quorum
            && self.c().quorum(self.row_weight(VoteKind::Prevote, None))
            && self.own_current(VoteKind::Prevote).is_some()
        {
            return Ok((Gap::Votes, Plan::Prevote));
        }
        Err(Skip::NoGap)
    }
    /// The certificate worth attaching to the current proposal: the retained
    /// polka for the SAME body, and only when its round either justifies the
    /// proposal (its valid round) or is the current round (repairs rows and
    /// conflicting cached bodies). A polka of another round would be a valid
    /// but useless group on a form-B proposal.
    fn proposal_group(&self, p: &Proposal<'_>) -> Option<Group<'_>> {
        let d = self.d();
        let vc = d.vc?;
        if self.proposal(vc.proposal).header.content.id != p.header.content.id {
            return None;
        }
        let r = d.valid_round?;
        if r == d.round || p.header.content.valid_round == Some(r) {
            Some(self.group(vc.certificate))
        } else {
            None
        }
    }
    /// Side-effect-free: the gap `serve` would attempt for this status.
    pub fn gap_for(&self, status: &StatusSnapshot) -> Option<Gap> {
        if self.check().is_err() {
            return None;
        }
        self.plan(status).ok().map(|(g, _)| g)
    }
    /// Emit at most ONE complete retained frame to `peer`, chosen from the
    /// peer's reported status. The status is only a hint; nothing is marked
    /// delivered or convincing.
    pub fn serve(
        &self,
        peer: u32,
        status: &StatusSnapshot,
        sink: &mut impl Sink,
    ) -> Result<Served> {
        self.check()?;
        let (gap, plan) = match self.plan(status) {
            Ok(x) => x,
            Err(s) => return Ok(Served::Skipped(s)),
        };
        let t = Target::Peer(peer);
        match plan {
            Plan::Dec => {
                let p = self.d().pending.ok_or(Error::Corrupt)?;
                let groups = [self.group(p.certificate)];
                sink.send(
                    t,
                    Frame {
                        message: Message::Dec,
                        outer: None,
                        proposal: Some(self.proposal(p.proposal)),
                        groups: &groups,
                    },
                );
            }
            Plan::Enter => self.emit_vote_complete(VoteKind::Enter, t, sink),
            Plan::Precommit => self.emit_vote_complete(VoteKind::Precommit, t, sink),
            Plan::Prevote => self.emit_vote_complete(VoteKind::Prevote, t, sink),
            Plan::Polka => {
                let vc = self.d().vc.ok_or(Error::Corrupt)?;
                let mut p = self.proposal(vc.proposal);
                if let Some(sl) = self.d().slot {
                    if self.proposal(sl).header.content.id == p.header.content.id {
                        p = self.proposal(sl);
                    }
                }
                let groups = [self.group(vc.certificate)];
                sink.send(
                    t,
                    Frame {
                        message: Message::Ordinary,
                        outer: None,
                        proposal: Some(p),
                        groups: &groups,
                    },
                );
            }
            Plan::Proposal => {
                let s = self.d().slot.ok_or(Error::Corrupt)?;
                let p = self.proposal(s);
                let group = self.proposal_group(&p);
                sink.send(
                    t,
                    Frame {
                        message: Message::Ordinary,
                        outer: None,
                        proposal: Some(p),
                        groups: group.as_slice(),
                    },
                );
            }
        }
        Ok(Served::Sent(gap))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirectorConfig {
    /// Peer table capacity N (fixed; further peers are refused).
    pub max_peers: usize,
    /// Maximum simultaneously outstanding interactions K (independent of N).
    pub max_grants: usize,
    /// Host-clock units before an unanswered interaction is abandoned.
    pub grant_timeout: u64,
    /// Host-clock units after which a received status is ignored.
    pub status_ttl: u64,
}
/// Host-assigned update. `generation` identifies the transport session
/// (fresh and larger on reconnect, assigned by the host, never peer-claimed);
/// `sequence` is the SENDER's per-session status sequence carried on the wire,
/// not a receiver arrival counter, so reordered or replayed statuses are stale.
/// Latest wins.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StatusUpdate {
    pub peer: u32,
    pub generation: u64,
    pub sequence: u64,
    pub status: StatusSnapshot,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UpdateError {
    /// Old or duplicated (generation, sequence).
    Stale,
    /// Peer table full; no state grows.
    Full,
}
struct PeerState {
    id: u32,
    generation: u64,
    sequence: u64,
    status: StatusSnapshot,
    received: u64,
    retry_at: u64,
    /// Service ordinal (not a clock) of the last send; None = never served.
    last_served: Option<u64>,
    failures: u32,
    granted: bool,
}
struct Grant {
    peer: u32,
    gap: Gap,
    deadline: u64,
    generation: u64,
    sequence: u64,
}
/// Bounded scheduler. State: N fixed-size peer records and K grants; no frame
/// is copied or queued.
pub struct Director {
    cfg: DirectorConfig,
    peers: Vec<PeerState>,
    index: HashMap<u32, usize>,
    grants: Vec<Grant>,
    clock: u64,
    /// Alternates priority-first and oldest-unserved-first selection; kept
    /// across polls and status updates so churn cannot reset it.
    fair_next: bool,
    /// Monotone count of sends; orders "oldest served" even when many polls
    /// share one host-clock value. Overflow fails closed before sending.
    serial: u64,
}
impl Director {
    pub fn new(cfg: DirectorConfig) -> Self {
        Self {
            cfg,
            peers: Vec::with_capacity(cfg.max_peers),
            index: HashMap::with_capacity(cfg.max_peers),
            grants: Vec::with_capacity(cfg.max_grants),
            clock: 0,
            fair_next: false,
            serial: 0,
        }
    }
    /// Checked constructor: `max_grants`, `grant_timeout` and `status_ttl`
    /// must be positive and `max_peers` nonzero. `new` keeps accepting any
    /// config; liveness claims exclude degenerate ones.
    pub fn try_new(cfg: DirectorConfig) -> Option<Self> {
        (cfg.max_peers > 0 && cfg.max_grants > 0 && cfg.grant_timeout > 0 && cfg.status_ttl > 0)
            .then(|| Self::new(cfg))
    }
    pub fn peers(&self) -> usize {
        self.peers.len()
    }
    pub fn active_grants(&self) -> usize {
        self.grants.len()
    }
    fn tick(&mut self, now: u64) -> u64 {
        self.clock = self.clock.max(now);
        self.clock
    }
    pub fn update(&mut self, u: StatusUpdate, now: u64) -> std::result::Result<(), UpdateError> {
        let now = self.tick(now);
        if let Some(&i) = self.index.get(&u.peer) {
            let p = &mut self.peers[i];
            if (u.generation, u.sequence) <= (p.generation, p.sequence) {
                return Err(UpdateError::Stale);
            }
            p.generation = u.generation;
            p.sequence = u.sequence;
            p.status = u.status;
            p.received = now;
            return Ok(());
        }
        if self.peers.len() >= self.cfg.max_peers {
            return Err(UpdateError::Full);
        }
        self.index.insert(u.peer, self.peers.len());
        self.peers.push(PeerState {
            id: u.peer,
            generation: u.generation,
            sequence: u.sequence,
            status: u.status,
            received: now,
            retry_at: 0,
            last_served: None,
            failures: 0,
            granted: false,
        });
        Ok(())
    }
    pub fn remove(&mut self, peer: u32) {
        let Some(i) = self.index.remove(&peer) else {
            return;
        };
        self.grants.retain(|g| g.peer != peer);
        self.peers.swap_remove(i);
        if let Some(moved) = self.peers.get(i) {
            self.index.insert(moved.id, i);
        }
    }
    /// `now` must be a host clock that never decreases (it is clamped, never
    /// trusted backwards); fairness uses a send ordinal, not `now`.
    /// Release finished or timed-out grants, then serve up to the free grant
    /// slots, best gap first. Returns the interactions started this call
    /// (at most K).
    pub fn poll<S: Storage>(
        &mut self,
        engine: &Engine<S>,
        now: u64,
        sink: &mut impl Sink,
    ) -> Result<Vec<(u32, Gap)>> {
        engine.check()?;
        let now = self.tick(now);
        let mut kept = Vec::with_capacity(self.cfg.max_grants);
        for g in std::mem::take(&mut self.grants) {
            let Some(&i) = self.index.get(&g.peer) else {
                continue;
            };
            let p = &mut self.peers[i];
            let newer = (p.generation, p.sequence) != (g.generation, g.sequence);
            let open = engine.gap_for(&p.status) == Some(g.gap);
            if newer && !open {
                p.failures = 0; // observed change: the gap closed
                p.granted = false;
            } else if now >= g.deadline {
                p.failures = p.failures.saturating_add(1);
                let backoff = self
                    .cfg
                    .grant_timeout
                    .saturating_mul(1 << p.failures.min(6));
                p.retry_at = now.saturating_add(backoff);
                p.granted = false;
            } else if !open {
                p.granted = false; // our own state moved on; no blame
            } else {
                kept.push(g);
            }
        }
        self.grants = kept;
        let mut started = Vec::new();
        while self.grants.len() < self.cfg.max_grants {
            let mut best: Option<(usize, Gap)> = None;
            for (i, p) in self.peers.iter().enumerate() {
                if p.granted
                    || now < p.retry_at
                    || now.saturating_sub(p.received) > self.cfg.status_ttl
                {
                    continue;
                }
                let Some(gap) = engine.gap_for(&p.status) else {
                    continue;
                };
                let better = match best {
                    None => true,
                    Some((b, bg)) => {
                        let q = &self.peers[b];
                        // Preserve strict service age, but diversify equal-age
                        // targets across senders instead of making every holder
                        // aim at the same lowest peer id. This is a tie-break,
                        // not random probing or a trust/ownership assignment.
                        let fair = self.fair_next;
                        let key = |p: &PeerState, g: Gap| {
                            let mut rank =
                                (p.id as u64) ^ (engine.me as u64).wrapping_mul(0x9e3779b97f4a7c15);
                            rank = (rank ^ (rank >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
                            rank = (rank ^ (rank >> 27)).wrapping_mul(0x94d049bb133111eb);
                            rank ^= rank >> 31;
                            (
                                std::cmp::Reverse(if fair { 0 } else { g.rank() }),
                                if fair { 0 } else { p.failures },
                                p.last_served.unwrap_or(0),
                                p.failures,
                                rank,
                                p.id,
                            )
                        };
                        key(p, gap) < key(q, bg)
                    }
                };
                if better {
                    best = Some((i, gap));
                }
            }
            let Some((i, gap)) = best else { break };
            let next = self.serial.checked_add(1).ok_or(Error::Exhausted)?;
            let p = &mut self.peers[i];
            match engine.serve(p.id, &p.status, sink)? {
                Served::Sent(g) => {
                    self.serial = next;
                    p.granted = true;
                    p.last_served = Some(next);
                    self.fair_next = !self.fair_next;
                    self.grants.push(Grant {
                        peer: p.id,
                        gap: g,
                        deadline: now.saturating_add(self.cfg.grant_timeout),
                        generation: p.generation,
                        sequence: p.sequence,
                    });
                    started.push((p.id, g));
                }
                Served::Skipped(_) => {
                    let _ = gap;
                    break;
                }
            }
        }
        Ok(started)
    }
}
