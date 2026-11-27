//! Bounded, deterministic host-side models for the receiver scheduler.
//!
//! These tests use only public APIs. Synthetic statuses remain untrusted hints:
//! nomination is not evidence validation, and a logical cancellation is not a
//! release of a frame that the host still owns. No transport or WAL is modeled.
use ed25519_zebra::{SigningKey, VerificationKeyBytes};
use std::collections::{BTreeMap, BTreeSet};
use tenderlink::d28::{
    gossip::{Gap, StatusSnapshot, StatusUpdate, UpdateError, receiver::*},
    *,
};

// Small fixed-seed generator: no entropy, timing, network, or extra dependency.
struct Schedule(u64);
impl Schedule {
    fn pick(&mut self, limit: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 as usize % limit
    }
}
fn peer(index: usize) -> u32 {
    1009 + 17 * index as u32
}
fn status(phase: usize) -> StatusSnapshot {
    let mut s = StatusSnapshot {
        config: [0x28; 32],
        height: 0,
        round: 0,
        step: Step::PreRound,
        proposal: None,
        polka: None,
        prevote_quorum: false,
        justified: false,
    };
    match phase {
        0 => {}
        1 => s.step = Step::Propose,
        2 => {
            s.step = Step::Prevote;
            s.proposal = Some([1; 32]);
            s.justified = true;
        }
        3 => {
            s.step = Step::Precommit;
            s.proposal = Some([1; 32]);
            s.polka = Some((0, [1; 32]));
            s.prevote_quorum = true;
            s.justified = true;
        }
        4 => s.step = Step::Decided,
        5 => {
            s.height = 1;
            s.step = Step::Propose;
        }
        _ => unreachable!(),
    }
    s
}
fn hint(local: StatusSnapshot, variant: usize) -> StatusSnapshot {
    let mut s = local;
    match variant {
        0 => {}
        1 => s.step = Step::Decided,
        2 => {
            s.round += 1;
            s.step = Step::Propose;
        }
        3 => {
            s.step = Step::Prevote;
            s.proposal = Some([1; 32]);
            s.justified = true;
            s.prevote_quorum = true;
        }
        4 => {
            s.step = Step::Precommit;
            s.proposal = Some([2; 32]);
            s.polka = Some((s.round, [2; 32]));
            s.prevote_quorum = true;
            s.justified = true;
        }
        5 => s.config = [0xff; 32],
        6 => s.height += 1,
        7 => s.step = Step::PreRound,
        _ => unreachable!(),
    }
    s
}
fn update(peer: u32, generation: u64, sequence: u64, status: StatusSnapshot) -> StatusUpdate {
    StatusUpdate {
        peer,
        generation,
        sequence,
        status,
    }
}
fn identity(key: RequestKey) -> (u32, u64, u64) {
    (key.peer, key.generation, key.sequence)
}
#[derive(Clone, Copy)]
struct Latest {
    update: StatusUpdate,
    received: u64,
}

#[test]
fn seeded_receiver_schedules_preserve_capacity_freshness_and_token_origins() {
    const PEERS: usize = 8;
    const EVENTS: usize = 192;
    let mut operations = [0usize; 8];
    let mut total_started = 0;
    let mut stale_rejections = 0;
    let mut full_rejections = 0;
    for seed in 1..=12 {
        let cfg = ReceiverConfig {
            max_peers: 5,
            max_requests: 1 + seed as usize % 3,
            request_timeout: 4,
            status_ttl: 9,
        };
        let mut receiver = Receiver::try_new(cfg).unwrap();
        let mut schedule = Schedule(seed);
        let mut latest: BTreeMap<u32, Latest> = BTreeMap::new();
        let mut generations = [1; PEERS];
        let mut issued: BTreeMap<(u32, u64, u64), u64> = BTreeMap::new();
        let mut now = 0;
        for event in 0..EVENTS {
            let local = status(event / 32);
            now += schedule.pick(3) as u64;
            if event % 16 == 0 {
                now += cfg.request_timeout + 1;
            }
            let index = schedule.pick(PEERS);
            let id = peer(index);
            let operation = schedule.pick(8);
            operations[operation] += 1;
            let previous = latest.get(&id).copied();
            if operation == 0 {
                receiver.remove(id);
                latest.remove(&id);
                // The authenticated host, not a wire hint, assigns reconnect
                // generations. Never replay an old session as a fresh peer.
                generations[index] += 1;
            } else if operation < 7 {
                let mut u = update(id, generations[index], 1, hint(local, schedule.pick(8)));
                if let Some(p) = previous {
                    u.sequence = p.update.sequence + 1;
                    match operation {
                        1 => u = p.update, // duplicate, including its old hint
                        2 => u.sequence = p.update.sequence.saturating_sub(1),
                        3 => {
                            u.generation = p.update.generation - 1;
                            u.sequence = u64::MAX; // sequence cannot repair an old session
                        }
                        4 => {
                            generations[index] += 1;
                            u.generation = generations[index];
                            u.sequence = 1;
                        }
                        5 => u.status = p.update.status, // advancing heartbeat, same hint
                        _ => {}
                    }
                }
                let expected = match previous {
                    Some(p)
                        if (u.generation, u.sequence)
                            <= (p.update.generation, p.update.sequence) =>
                    {
                        stale_rejections += 1;
                        Err(UpdateError::Stale)
                    }
                    None if latest.len() == cfg.max_peers => {
                        full_rejections += 1;
                        Err(UpdateError::Full)
                    }
                    _ => Ok(()),
                };
                assert_eq!(
                    receiver.update(u, now),
                    expected,
                    "seed={seed}, event={event}, op={operation}"
                );
                if expected.is_ok() {
                    latest.insert(
                        id,
                        Latest {
                            update: u,
                            received: now,
                        },
                    );
                }
            }

            let sequence = event as u64 + 1;
            let started = receiver.poll(local, sequence, now).unwrap();
            total_started += started.len();
            assert!(started.len() <= cfg.max_requests);
            for key in &started {
                assert_eq!(
                    key.sequence, sequence,
                    "token must name its originating poll"
                );
                assert!(issued.insert(identity(*key), now).is_none(), "token reused");
            }
            let active: Vec<_> = receiver.requests().collect();
            let identities: BTreeSet<_> = active.iter().copied().map(identity).collect();
            let destinations: BTreeSet<_> = active.iter().map(|key| key.peer).collect();
            assert_eq!(active.len(), receiver.active_requests());
            assert_eq!(identities.len(), active.len());
            assert_eq!(destinations.len(), active.len(), "one request per source");
            assert!(
                active.len() <= cfg.max_requests,
                "seed={seed}, event={event}"
            );
            assert_eq!(receiver.peers(), latest.len());
            assert!(receiver.peers() <= cfg.max_peers);
            for key in &active {
                let p = latest.get(&key.peer).expect("request to a removed peer");
                assert_eq!(key.generation, p.update.generation);
                assert_eq!(p.update.status.config, local.config);
                assert_eq!(p.update.status.height, local.height);
                assert!(now - p.received <= cfg.status_ttl, "expired hint nominated");
                assert!(
                    now - issued[&identity(*key)] < cfg.request_timeout,
                    "heartbeat extended a token's original deadline"
                );
                assert!(receiver.is_current(*key, now));
                assert!(!receiver.is_current(
                    RequestKey {
                        generation: key.generation + 1,
                        ..*key
                    },
                    now
                ));
                assert!(!receiver.is_current(
                    RequestKey {
                        sequence: sequence + 1,
                        ..*key
                    },
                    now
                ));
            }
            for (&(peer, generation, sequence), _) in &issued {
                let key = RequestKey {
                    peer,
                    generation,
                    sequence,
                };
                assert_eq!(
                    receiver.is_current(key, now),
                    identities.contains(&identity(key)),
                    "retired token resurrected: seed={seed}, event={event}"
                );
            }
            for index in 0..PEERS {
                let expected = active
                    .iter()
                    .find(|key| key.peer == peer(index))
                    .map(|key| key.sequence);
                assert_eq!(receiver.request_for(peer(index)), expected);
            }
            if local.step == Step::Decided {
                assert!(
                    active.is_empty(),
                    "decided receiver requests no more evidence"
                );
            }
            // A replay or backwards local advertisement must not start work or
            // mutate the observable nomination set, even with a changed hint.
            if operation == 7 {
                for replay in [sequence, sequence.saturating_sub(1)] {
                    assert!(receiver.poll(hint(local, 1), replay, now).is_err());
                    assert_eq!(receiver.requests().collect::<Vec<_>>(), active);
                }
            }
        }
    }
    assert!(operations.iter().all(|count| *count > 0));
    assert!(
        total_started > 100,
        "schedule failed to exercise nominations"
    );
    assert!(stale_rejections > 100 && full_rejections > 0);
}

#[test]
fn scarce_honest_source_is_attempted_despite_constant_stronger_hints_and_heartbeats() {
    for n in [3, 7, 11] {
        for k in [1, 2, 3] {
            for phase in 0..4 {
                for seed in 1..=4 {
                    let cfg = ReceiverConfig {
                        max_peers: n,
                        max_requests: k,
                        request_timeout: 4,
                        status_ttl: 8,
                    };
                    let mut receiver = Receiver::try_new(cfg).unwrap();
                    let local = status(phase);
                    let useful_hint = match phase {
                        0 => status(1),
                        1 => status(2),
                        2 => StatusSnapshot {
                            prevote_quorum: true,
                            ..local
                        },
                        3 => StatusSnapshot {
                            round: 1,
                            step: Step::Propose,
                            ..local
                        },
                        _ => unreachable!(),
                    };
                    // Half the traces deliberately hide any useful source hint.
                    // Same semantic status does not imply identical custody;
                    // fair exploration must still attempt this compatible peer.
                    let opaque = seed % 2 == 0;
                    let honest_hint = if opaque { local } else { useful_hint };
                    let stronger_hint = StatusSnapshot {
                        step: Step::Decided,
                        ..local
                    };
                    let honest = peer(n - 1);
                    let mut schedule = Schedule(seed);
                    let mut order: Vec<_> = (0..n).collect();
                    for i in (1..n).rev() {
                        order.swap(i, schedule.pick(i + 1));
                    }
                    for &index in &order {
                        receiver
                            .update(
                                update(
                                    peer(index),
                                    1,
                                    1,
                                    if peer(index) == honest {
                                        honest_hint
                                    } else {
                                        stronger_hint
                                    },
                                ),
                                0,
                            )
                            .unwrap();
                    }
                    let mut attempted = false;
                    // A fair slot every second nomination gives a conservative
                    // 2N+2 timeout-epoch bound for a continuously eligible source.
                    // Every stronger claimant is silent; no false hint is treated
                    // as a certificate and there is no all-to-all frame fallback.
                    let bound = 2 * n + 2;
                    for epoch in 0..bound {
                        let now = epoch as u64 * cfg.request_timeout;
                        // Helpful-source traces keep constant stronger hints;
                        // opaque-source traces also exercise stronger-hint churn.
                        let stronger_hint = if opaque && epoch % 2 == 1 {
                            StatusSnapshot {
                                round: local.round + 1,
                                step: Step::Propose,
                                ..local
                            }
                        } else {
                            stronger_hint
                        };
                        for &index in &order {
                            receiver
                                .update(
                                    update(
                                        peer(index),
                                        1,
                                        epoch as u64 + 2,
                                        if peer(index) == honest {
                                            honest_hint
                                        } else {
                                            stronger_hint
                                        },
                                    ),
                                    now,
                                )
                                .unwrap();
                        }
                        let started = receiver.poll(local, epoch as u64 + 1, now).unwrap();
                        assert!(receiver.active_requests() <= k);
                        if started.iter().any(|key| key.peer == honest) {
                            attempted = true;
                            break;
                        }
                    }
                    assert!(
                        attempted,
                        "honest source starved: n={n}, k={k}, phase={phase}, seed={seed}, epochs={bound}"
                    );
                }
            }
        }
    }
}

#[test]
fn seeded_admission_model_counts_owned_frames_not_logical_requests() {
    for capacity in [1, 2, 4] {
        for seed in 1..=12 {
            let budget = AdmissionBudget::new(capacity);
            let mut schedule = Schedule(seed);
            let mut held: Vec<(usize, Permit)> = Vec::new();
            let mut nominated = BTreeSet::new();
            let mut expected_high_water = 0;
            let mut serial = 0;
            for event in 0..256 {
                match schedule.pick(5) {
                    0 | 1 => {
                        // New logical work is allowed while canceled frames are
                        // still owned, but it never manufactures physical space.
                        serial += 1;
                        nominated.insert(serial);
                        let acquired = budget.try_acquire();
                        assert_eq!(
                            acquired.is_some(),
                            held.len() < capacity,
                            "seed={seed}, event={event}, capacity={capacity}"
                        );
                        if let Some(permit) = acquired {
                            held.push((serial, permit));
                        }
                        expected_high_water = expected_high_water.max(held.len());
                    }
                    2 => {
                        if !held.is_empty() {
                            let index = schedule.pick(held.len());
                            nominated.remove(&held[index].0); // cancel, keep the owned frame
                        }
                    }
                    3 => {
                        if !held.is_empty() {
                            let index = schedule.pick(held.len());
                            let (request, permit) = held.swap_remove(index);
                            nominated.remove(&request);
                            drop(permit); // transport completion or abort frees physical space
                        }
                    }
                    _ => {
                        // Ownership may move between host queues without a new
                        // permit, even after every logical nomination was canceled.
                        nominated.clear();
                        if held.len() > 1 {
                            let last = held.len() - 1;
                            held.swap(0, last);
                        }
                    }
                }
                assert_eq!(budget.capacity(), capacity);
                assert_eq!(budget.in_use(), held.len());
                assert_eq!(budget.high_water(), expected_high_water);
                assert!(budget.in_use() <= capacity);
            }
            nominated.clear();
            assert_eq!(budget.in_use(), held.len());
            drop(held);
            assert_eq!(budget.in_use(), 0);
            assert_eq!(budget.high_water(), expected_high_water);
        }
    }
}

struct Discard;
impl Sink for Discard {
    fn send(&mut self, _: Target, _: Frame<'_>) {}
    fn decided(&mut self, _: u64, _: Proposal<'_>, _: Group<'_>) {}
}
fn proposal_source() -> Engine<MemoryStore> {
    let key = |index: u8| SigningKey::from([index + 1; 32]);
    let cfg = Config::new(
        [0x28; 32],
        (0..4)
            .map(|i| Member {
                public_key: VerificationKeyBytes::from(&key(i)).into(),
                weight: 1,
            })
            .collect(),
        1024,
        3,
        Timeouts {
            propose: 30,
            prevote: 15,
            precommit: 25,
            inc_propose: 5,
            inc_prevote: 1,
            inc_precommit: 3,
        },
    )
    .unwrap();
    let mut engine = Engine::boot(MemoryStore::default(), key(3), cfg.clone()).unwrap();
    engine.set_emission(Emission::Guided);
    engine.init(&cfg, 0, 0, &mut Discard).unwrap();
    let body = b"bounded model proposal";
    let proposer = cfg.proposer(0, 0) as u8;
    let header = Header::sign(
        &cfg,
        &key(proposer),
        ProposalContent {
            height: 0,
            round: 0,
            valid_round: None,
            id: body_id(body),
            signer: proposer as u32,
        },
    );
    engine
        .receive(
            Frame {
                message: Message::Ordinary,
                outer: None,
                proposal: Some(Proposal {
                    header: &header,
                    body,
                }),
                groups: &[],
            },
            Status::Valid,
            &mut Discard,
        )
        .unwrap();
    engine
}

struct OwnedReply {
    // For this fixture a complete frame is exactly an ordinary proposal, with
    // no outer vote or groups. The permit accompanies all of its owned bytes.
    _proposal: (Header, Vec<u8>),
    _permit: Permit,
}
#[derive(Default)]
struct HoldingReplies {
    refuse: bool,
    called: Vec<RequestKey>,
    held: Vec<OwnedReply>,
}
impl ReplySink for HoldingReplies {
    fn send(
        &mut self,
        key: RequestKey,
        frame: Frame<'_>,
        permit: Permit,
    ) -> std::result::Result<(), Permit> {
        self.called.push(key);
        if self.refuse {
            return Err(permit);
        }
        assert_eq!(frame.message, Message::Ordinary);
        assert!(frame.outer.is_none() && frame.groups.is_empty());
        let proposal = frame.proposal.expect("servable proposal fixture");
        assert_eq!(proposal.body, b"bounded model proposal");
        self.held.push(OwnedReply {
            _proposal: (*proposal.header, proposal.body.to_vec()),
            _permit: permit,
        });
        Ok(())
    }
}

#[derive(Clone, Copy)]
struct LiveAttempt {
    token: u64,
    deadline: u64,
    offered: bool,
}
struct RequesterModel {
    latest: Latest,
    // Deliberately explicit terminal-token ledger, rather than the production
    // scalar highwater representation. Every retired prefix stays forbidden.
    retired: BTreeSet<u64>,
    live: Option<LiveAttempt>,
}
impl RequesterModel {
    fn close(&mut self, through: u64) {
        self.retired.extend(0..=through);
        self.live = None;
    }
    fn expire(&mut self, now: u64) {
        if let Some(a) = self.live {
            if now >= a.deadline {
                self.close(a.token);
            }
        }
    }
    fn apply(&mut self, message: RequestStatus, now: u64, timeout: u64) {
        if self.latest.update.generation != message.update.generation {
            self.live = None;
            self.retired.clear();
        }
        self.expire(now);
        self.latest = Latest {
            update: message.update,
            received: now,
        };
        match message.request {
            None => self.close(message.update.sequence),
            Some(token) => {
                if let Some(old) = self.live {
                    if old.token != token {
                        self.close(old.token);
                    }
                }
                if self.retired.contains(&token) {
                    self.live = None;
                } else if self.live.is_none() {
                    self.live = Some(LiveAttempt {
                        token,
                        deadline: now + timeout,
                        offered: false,
                    });
                }
            }
        }
    }
}

#[test]
fn seeded_responder_model_retains_terminal_tokens_and_physical_ownership() {
    let mut engine = proposal_source();
    let caught_up = engine.status().unwrap();
    let lag = StatusSnapshot {
        proposal: None,
        step: Step::Propose,
        justified: false,
        ..caught_up
    };
    assert_eq!(engine.gap_for(&lag), Some(Gap::Proposal));
    assert_eq!(engine.gap_for(&caught_up), None);
    let mut accepted_total = 0;
    let mut refused_total = 0;
    let mut replay_total = 0;
    let mut canceled_owned = 0;
    for seed in 1..=12 {
        let cfg = SenderConfig {
            max_peers: 5,
            max_outgoing: 1 + seed as usize % 3,
            request_timeout: 7,
            status_ttl: 4,
        };
        let mut responder = Responder::try_new(cfg).unwrap();
        let mut schedule = Schedule(seed);
        let mut model: BTreeMap<u32, RequesterModel> = BTreeMap::new();
        let mut generations = [1; 8];
        let mut accepted = BTreeSet::new();
        let mut sink = HoldingReplies::default();
        let mut now = 0;
        for event in 0..256 {
            now += schedule.pick(3) as u64;
            if event % 23 == 0 {
                now += cfg.request_timeout;
            }
            let index = schedule.pick(generations.len());
            let id = peer(index);
            let operation = schedule.pick(12);
            if operation == 0 {
                responder.remove(id);
                model.remove(&id);
                generations[index] += 1;
            } else if operation < 10 {
                let previous = model.get(&id);
                let sequence = previous.map_or(1, |p| {
                    p.latest.update.sequence + 1 + schedule.pick(3) as u64
                });
                let snapshot = match schedule.pick(5) {
                    0 | 1 => lag,
                    2 => caught_up,
                    3 => StatusSnapshot {
                        config: [0xee; 32],
                        ..lag
                    },
                    _ => StatusSnapshot {
                        step: Step::Decided,
                        ..lag
                    },
                };
                let mut message = RequestStatus {
                    update: update(id, generations[index], sequence, snapshot),
                    request: Some(sequence),
                };
                match operation {
                    1 => message.request = None,
                    3 => {
                        message.request = previous
                            .and_then(|p| p.live.map(|a| a.token))
                            .or(Some(sequence))
                    }
                    4 if previous.is_some() => message.update = previous.unwrap().latest.update,
                    5 if previous.is_some() => {
                        message.update.generation = previous.unwrap().latest.update.generation - 1;
                        message.update.sequence = u64::MAX;
                    }
                    6 => message.request = Some(sequence + 1),
                    7 | 9 => {
                        message.request = Some(schedule.pick(sequence as usize + 1) as u64);
                        replay_total += 1;
                    }
                    8 => {
                        generations[index] += 1;
                        message.update.generation = generations[index];
                        message.update.sequence = 1;
                        message.request = Some(1);
                    }
                    _ => {}
                }
                // A same-sequence replay must be otherwise well formed, so the
                // freshness check rather than future-token validation rejects it.
                if operation == 4 && previous.is_some() {
                    message.request = Some(message.update.sequence);
                }
                let u = message.update;
                let expected = if message.request.is_some_and(|token| token > u.sequence) {
                    Err(RequestUpdateError::InvalidRequest)
                } else if previous.is_some_and(|p| {
                    (u.generation, u.sequence)
                        <= (p.latest.update.generation, p.latest.update.sequence)
                }) {
                    Err(RequestUpdateError::Stale)
                } else if previous.is_none() && model.len() == cfg.max_peers {
                    Err(RequestUpdateError::Full)
                } else {
                    Ok(())
                };
                assert_eq!(
                    responder.update(message, now),
                    expected,
                    "seed={seed}, event={event}, op={operation}"
                );
                if expected.is_ok() {
                    model
                        .entry(id)
                        .or_insert_with(|| RequesterModel {
                            latest: Latest {
                                update: u,
                                received: now,
                            },
                            retired: BTreeSet::new(),
                            live: None,
                        })
                        .apply(message, now, cfg.request_timeout);
                }
            } else if operation == 10 && !sink.held.is_empty() {
                let index = schedule.pick(sink.held.len());
                drop(sink.held.swap_remove(index));
            }
            // Cancellation/removal/expiry never frees a sink-owned allocation.
            assert_eq!(responder.outstanding(), sink.held.len());
            sink.refuse = schedule.pick(4) == 0;
            sink.called.clear();
            let held_before = sink.held.len();
            for p in model.values_mut() {
                p.expire(now);
            }
            let sent = responder.poll(&mut engine, now, &mut sink).unwrap();
            assert!(sink.called.len() <= cfg.max_outgoing);
            let mut offered_this_poll = BTreeSet::new();
            for &key in &sink.called {
                assert!(offered_this_poll.insert(identity(key)));
                let p = model
                    .get_mut(&key.peer)
                    .expect("offer for removed requester");
                let a = p.live.as_mut().expect("offer resurrected a terminal token");
                assert_eq!(key.generation, p.latest.update.generation);
                assert_eq!(key.sequence, a.token);
                assert!(now < a.deadline && now - p.latest.received <= cfg.status_ttl);
                assert!(!a.offered, "accepted token was offered twice");
                assert!(!p.retired.contains(&key.sequence));
                assert_eq!(engine.gap_for(&p.latest.update.status), Some(Gap::Proposal));
                if !sink.refuse {
                    assert!(
                        accepted.insert(identity(key)),
                        "accepted token was resurrected"
                    );
                    a.offered = true;
                }
            }
            if sink.refuse {
                assert!(sent.is_empty());
                assert_eq!(sink.held.len(), held_before);
                refused_total += sink.called.len();
            } else {
                assert_eq!(
                    sent,
                    sink.called
                        .iter()
                        .map(|key| (*key, Gap::Proposal))
                        .collect::<Vec<_>>()
                );
                assert_eq!(sink.held.len(), held_before + sent.len());
                accepted_total += sent.len();
            }
            let live = model.values().filter(|p| p.live.is_some()).count();
            assert_eq!(responder.peers(), model.len());
            assert_eq!(
                responder.active_requests(),
                live,
                "seed={seed}, event={event}"
            );
            assert!(responder.peers() <= cfg.max_peers);
            assert!(responder.active_requests() <= cfg.max_peers);
            assert_eq!(responder.outstanding(), sink.held.len());
            assert!(responder.outstanding() <= cfg.max_outgoing);
            if live == 0 && !sink.held.is_empty() {
                canceled_owned += 1;
            }
        }
        drop(sink);
        assert_eq!(responder.outstanding(), 0);
    }
    assert!(accepted_total > 20 && refused_total > 0 && replay_total > 100);
    assert!(
        canceled_owned > 0,
        "schedule never separated logical and physical lifetimes"
    );
}

#[test]
fn cloned_budgets_share_one_bound_across_concurrent_owners() {
    use std::sync::{
        Barrier,
        atomic::{AtomicUsize, Ordering},
    };

    const WORKERS: usize = 8;
    const ROUNDS: usize = 16;
    for capacity in [1, 3, 8] {
        let budget = AdmissionBudget::new(capacity);
        let owned = AtomicUsize::new(0);
        let gate = Barrier::new(WORKERS + 1);
        let mut observations = Vec::new();
        std::thread::scope(|scope| {
            for _ in 0..WORKERS {
                let budget = budget.clone();
                let owned = &owned;
                let gate = &gate;
                scope.spawn(move || {
                    for _ in 0..ROUNDS {
                        let permit = budget.try_acquire();
                        if permit.is_some() {
                            owned.fetch_add(1, Ordering::SeqCst);
                        }
                        gate.wait(); // everyone attempted; no owner has dropped
                        gate.wait(); // observer has sampled the held state
                        if permit.is_some() {
                            owned.fetch_sub(1, Ordering::SeqCst);
                        }
                        drop(permit);
                        gate.wait(); // everyone released; next round is blocked
                        gate.wait();
                    }
                });
            }
            for _ in 0..ROUNDS {
                gate.wait();
                observations.push((owned.load(Ordering::SeqCst), budget.in_use(), capacity));
                gate.wait();
                gate.wait();
                observations.push((owned.load(Ordering::SeqCst), budget.in_use(), 0));
                gate.wait();
            }
        });
        // Check only after all workers joined: an accounting regression must
        // fail, not strand other workers behind a test-only assertion/barrier.
        for (owned, counted, expected) in observations {
            assert_eq!(owned, expected);
            assert_eq!(counted, expected);
        }
        assert_eq!(budget.in_use(), 0);
        assert_eq!(budget.high_water(), capacity);
    }
}
