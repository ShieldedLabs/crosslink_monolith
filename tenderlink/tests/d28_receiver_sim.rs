//! Paired sender/receiver Guided simulations (Fix #428).
//!
//! All counters are typed-record transmissions or explicitly modelled buffer
//! ownership, NOT codec bytes, allocator/RSS measurements or an STP benchmark.
//! Both arms use the same core, real Ed25519 signatures, clocks and network.
//! No transport, WAL, archive, future-vote bank or bulk broadcast fallback.
//!
//! `frames/records/body_bytes/bulk` count source offers (including later drops);
//! `processed_duplicate_*` count repeats only among admitted processed bulk,
//! not rejected/on-wire copies or evidence already present before measurement.
//! Packet bytes count inline storage and owned Vec capacities, not allocator,
//! core, inbox-capacity or virtual-network overhead. Diagnostic ledgers and the
//! event map are test-only and have no claimed production memory bound.
//! Connections remain in one generation: reconnect/restart authentication and
//! old-session queue rejection are NOT exercised by this simulation. Late-token
//! tests intentionally remain in the SAME authenticated session.
//!
//! Reproduce: CARGO_BUILD_JOBS=2 cargo test --offline --locked --release
//! --manifest-path tenderlink/Cargo.toml --test d28_receiver_sim
//! -- --nocapture --test-threads=1
use ed25519_zebra::{SigningKey, VerificationKeyBytes};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;
use tenderlink::d28::{
    gossip::{receiver::*, *},
    *,
};

const RHO: u64 = 3;
const K: usize = 1;
const GRANT_TIMEOUT: u64 = 12;
const STATUS_TTL: u64 = 120;
const EVENT_LIMIT: u64 = 200_000;

fn key(i: usize) -> SigningKey {
    let mut bytes = [0xa8; 32];
    bytes[..4].copy_from_slice(&(i as u32 + 1).to_le_bytes());
    SigningKey::from(bytes)
}
fn peer(i: usize) -> u32 {
    1009 + 17 * i as u32 // opaque host handles, never roster offsets
}
fn config(weights: &[u64]) -> Config {
    Config::new(
        [0x28; 32],
        weights
            .iter()
            .enumerate()
            .map(|(i, &weight)| Member {
                public_key: VerificationKeyBytes::from(&key(i)).into(),
                weight,
            })
            .collect(),
        1 << 20,
        RHO,
        Timeouts {
            propose: 300,
            prevote: 150,
            precommit: 250,
            inc_propose: 5,
            inc_prevote: 1,
            inc_precommit: 3,
        },
    )
    .unwrap()
}
fn body(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i as u8).wrapping_mul(31)).collect()
}

#[derive(Clone, Debug)]
struct Packet {
    target: Target,
    message: Message,
    outer: Option<Vote>,
    proposal: Option<(Header, Vec<u8>)>,
    groups: Vec<(CertKind, Vec<Vote>)>,
}
impl Packet {
    fn copy(target: Target, frame: Frame<'_>) -> Self {
        Self {
            target,
            message: frame.message,
            outer: frame.outer.copied(),
            proposal: frame.proposal.map(|p| (*p.header, p.body.to_vec())),
            groups: frame
                .groups
                .iter()
                .map(|g| (g.kind, g.votes.iter().copied().collect()))
                .collect(),
        }
    }
    fn bulk(&self) -> bool {
        self.proposal.is_some() || !self.groups.is_empty() || self.message == Message::Dec
    }
    fn records(&self) -> u64 {
        self.outer.is_some() as u64
            + self.proposal.is_some() as u64
            + self.groups.iter().map(|g| g.1.len() as u64).sum::<u64>()
    }
    fn body_bytes(&self) -> u64 {
        self.proposal.as_ref().map_or(0, |p| p.1.len() as u64)
    }
    fn payload_bytes(&self) -> usize {
        // Packet-owned Rust storage: inline fields plus backing Vec capacities.
        // Excludes allocator overhead, inbox capacity, Arc/network allocations,
        // and the core. This is deliberately NOT total process memory or wire size.
        std::mem::size_of::<Self>()
            + self.proposal.as_ref().map_or(0, |p| p.1.capacity())
            + self.groups.capacity() * std::mem::size_of::<(CertKind, Vec<Vote>)>()
            + self
                .groups
                .iter()
                .map(|g| g.1.capacity() * std::mem::size_of::<Vote>())
                .sum::<usize>()
    }
    fn deliver(&self, engine: &mut Engine<MemoryStore>, rec: &mut Rec) {
        let groups: Vec<_> = self
            .groups
            .iter()
            .map(|(kind, votes)| Group {
                kind: *kind,
                votes: Votes::Dense(votes),
            })
            .collect();
        engine
            .receive(
                Frame {
                    message: self.message,
                    outer: self.outer.as_ref(),
                    proposal: self
                        .proposal
                        .as_ref()
                        .map(|(header, body)| Proposal { header, body }),
                    groups: &groups,
                },
                Status::Valid,
                rec,
            )
            .unwrap();
    }
}
#[derive(Default)]
struct Rec {
    packets: Vec<Packet>,
    decisions: Vec<(u64, Id)>,
    acquire: Option<ProposalTag>,
}
impl Sink for Rec {
    fn send(&mut self, target: Target, frame: Frame<'_>) {
        self.packets.push(Packet::copy(target, frame));
    }
    fn decided(&mut self, height: u64, proposal: Proposal<'_>, group: Group<'_>) {
        assert!(
            matches!(group.kind, CertKind::Decision { id, .. } if id == proposal.header.content.id)
        );
        self.decisions.push((height, proposal.header.content.id));
    }
    fn acquire(&mut self, tag: ProposalTag) {
        self.acquire = Some(tag);
    }
}
fn decision(c: &Config, bytes: &[u8]) -> Packet {
    let id = body_id(bytes);
    let signer = c.proposer(0, 0);
    let header = Header::sign(
        c,
        &key(signer),
        ProposalContent {
            height: 0,
            round: 0,
            valid_round: None,
            id,
            signer: signer as u32,
        },
    );
    let mut weight = 0;
    let votes = c
        .members()
        .iter()
        .enumerate()
        .take(c.members().len() - 1)
        .map(|(i, m)| {
            weight += m.weight;
            Vote::sign(
                c,
                &key(i),
                VoteContent {
                    height: 0,
                    round: 0,
                    kind: VoteKind::Precommit,
                    id: Some(id),
                    signer: i as u32,
                },
            )
        })
        .collect::<Vec<_>>();
    assert!(c.quorum(weight));
    Packet {
        target: Target::Peer(0),
        message: Message::Dec,
        outer: None,
        proposal: Some((header, bytes.to_vec())),
        groups: vec![(CertKind::Decision { round: 0, id }, votes)],
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Lane {
    Enter,
    Prevote,
    Precommit,
    Evidence,
    Status,
}
fn lane(packet: &Packet) -> Lane {
    if packet.bulk() {
        return Lane::Evidence;
    }
    match packet.outer.unwrap().content.kind {
        VoteKind::Enter => Lane::Enter,
        VoteKind::Prevote => Lane::Prevote,
        VoteKind::Precommit => Lane::Precommit,
    }
}
fn mix(mut x: u64) -> u64 {
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d049bb133111eb);
    x ^ (x >> 31)
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Delivery {
    drop: bool,
    duplicate: bool,
    delay: u64,
    tie: u64,
}
struct Schedule {
    seed: u64,
    chaos: bool,
    evidence_delay: u64,
    ordinals: BTreeMap<(usize, usize, Lane), u64>,
}
impl Schedule {
    fn new(seed: u64, chaos: bool) -> Self {
        Self {
            seed,
            chaos,
            evidence_delay: 1,
            ordinals: BTreeMap::new(),
        }
    }
    fn next(&mut self, from: usize, to: usize, lane: Lane) -> Delivery {
        let ordinal = self.ordinals.entry((from, to, lane)).or_default();
        *ordinal += 1;
        let hash = mix(self.seed
            ^ mix(from as u64)
            ^ mix((to as u64) + 0x10000)
            ^ mix((lane as u64) + 0x20000)
            ^ mix(*ordinal + 0x30000));
        Delivery {
            drop: self.chaos && hash % 11 == 0,
            duplicate: self.chaos && hash % 7 == 0,
            delay: if lane == Lane::Evidence {
                self.evidence_delay
            } else {
                1
            } + if self.chaos { hash % 3 } else { 0 },
            tie: hash,
        }
    }
}

#[test]
fn lane_schedule_is_reproducible_and_unrelated_traffic_cannot_shift_loss() {
    let mut a = Schedule::new(0xd28, true);
    let mut b = Schedule::new(0xd28, true);
    for _ in 0..100 {
        b.next(9, 8, Lane::Evidence);
        b.next(0, 1, Lane::Status);
        assert_eq!(a.next(0, 1, Lane::Evidence), b.next(0, 1, Lane::Evidence));
    }
}

#[test]
fn signed_decision_fixture_is_admitted_by_the_real_core() {
    let c = config(&[1; 4]);
    let bytes = body(512);
    let mut e = Engine::boot(MemoryStore::default(), key(3), c.clone()).unwrap();
    e.set_emission(Emission::Guided);
    let mut r = Rec::default();
    e.init(&c, 0, 0, &mut r).unwrap();
    decision(&c, &bytes).deliver(&mut e, &mut r);
    assert_eq!(r.decisions, [(0, body_id(&bytes))]);
    assert!(r.packets.iter().all(|p| !p.bulk()));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Sender,
    Receiver,
}

enum Scheduler {
    Sender {
        director: Director,
        tx: AdmissionBudget,
    },
    Receiver {
        receiver: Receiver,
        responder: Responder,
    },
}
impl Scheduler {
    fn new(mode: Mode, n: usize) -> Self {
        match mode {
            Mode::Sender => Self::Sender {
                director: Director::try_new(DirectorConfig {
                    max_peers: n - 1,
                    max_grants: K,
                    grant_timeout: GRANT_TIMEOUT,
                    status_ttl: STATUS_TTL,
                })
                .unwrap(),
                tx: AdmissionBudget::new(K),
            },
            Mode::Receiver => Self::Receiver {
                receiver: Receiver::try_new(ReceiverConfig {
                    max_peers: n - 1,
                    max_requests: K,
                    request_timeout: GRANT_TIMEOUT,
                    status_ttl: STATUS_TTL,
                })
                .unwrap(),
                responder: Responder::try_new(SenderConfig {
                    max_peers: n - 1,
                    max_outgoing: K,
                    request_timeout: GRANT_TIMEOUT,
                    status_ttl: STATUS_TTL,
                })
                .unwrap(),
            },
        }
    }
    fn logical(&self) -> usize {
        match self {
            Self::Sender { director, .. } => director.active_grants(),
            Self::Receiver { receiver, .. } => receiver.active_requests(),
        }
    }
    fn tx_owned(&self) -> usize {
        match self {
            Self::Sender { tx, .. } => tx.in_use(),
            Self::Receiver { responder, .. } => responder.outstanding(),
        }
    }
}
struct Node {
    engine: Engine<MemoryStore>,
    rec: Rec,
    scheduler: Scheduler,
    generation: u64,
    sequence: u64,
    // This budget survives a scheduler/session reset. Each distinct deep copy
    // below owns its own nonclone permit until processing/discard finishes.
    rx: AdmissionBudget,
    inbox: VecDeque<Received>,
}
struct Received {
    ready: u64,
    generation: u64,
    packet: Packet,
    _permit: Permit,
}
struct Transmission {
    packet: Packet,
    token: Option<u64>,
    // Duplicated virtual network events share ONE source allocation and permit.
    // Each admitted receiver copy is separate and acquires its OWN rx permit.
    _permit: Option<Permit>,
}
struct Replies {
    frames: Vec<(u32, Transmission)>,
}
impl ReplySink for Replies {
    fn send(
        &mut self,
        key: RequestKey,
        frame: Frame<'_>,
        permit: Permit,
    ) -> std::result::Result<(), Permit> {
        self.frames.push((
            key.peer,
            Transmission {
                packet: Packet::copy(Target::Peer(key.peer), frame),
                token: Some(key.sequence),
                _permit: Some(permit),
            },
        ));
        Ok(())
    }
}
struct SenderReplies<'a> {
    frames: Vec<(u32, Transmission)>,
    budget: &'a AdmissionBudget,
    dropped: u64,
}
impl Sink for SenderReplies<'_> {
    fn send(&mut self, target: Target, frame: Frame<'_>) {
        let Target::Peer(peer) = target else {
            panic!("directed sender broadcast")
        };
        let Some(permit) = self.budget.try_acquire() else {
            self.dropped += 1;
            return;
        };
        self.frames.push((
            peer,
            Transmission {
                packet: Packet::copy(target, frame),
                token: None,
                _permit: Some(permit),
            },
        ));
    }
    fn decided(&mut self, _: u64, _: Proposal<'_>, _: Group<'_>) {
        unreachable!()
    }
}
#[derive(Clone)]
enum Event {
    Control {
        from: usize,
        to: usize,
        update: StatusUpdate,
        request: Option<u64>,
    },
    Frame {
        from: usize,
        to: usize,
        generation: u64,
        tx: Arc<Transmission>,
    },
}
impl Event {
    fn endpoints(&self) -> (usize, usize) {
        match self {
            Self::Control { from, to, .. } | Self::Frame { from, to, .. } => (*from, *to),
        }
    }
    fn lane(&self) -> Lane {
        match self {
            Self::Control { .. } => Lane::Status,
            Self::Frame { tx, .. } if tx._permit.is_some() => Lane::Evidence,
            Self::Frame { tx, .. } => lane(&tx.packet),
        }
    }
    fn owned_response(&self) -> bool {
        matches!(self, Self::Frame { tx, .. } if tx._permit.is_some())
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Counts {
    frames: u64,
    records: u64,
    body_bytes: u64,
    bulk: u64,
    responses: u64,
    statuses: u64,
    request_signals: u64,
    requests_started: u64,
    wire_drops: u64,
    wire_duplicates: u64,
    tx_cap_drops: u64,
    rx_cap_drops: u64,
    stale_responses: u64,
    stale_statuses: u64,
    processed_bulk: u64,
    processed_duplicate_bulk: u64,
    processed_duplicate_records: u64,
    processed_duplicate_body_bytes: u64,
    peak_logical: usize,
    peak_tx_owned: usize,
    peak_rx_owned: usize,
    peak_rx_packet_bytes: usize,
    peak_wire_responses: usize,
    events: u64,
}
struct Net {
    mode: Mode,
    config: Config,
    body: Vec<u8>,
    nodes: Vec<Node>,
    offline: Vec<bool>,
    unavailable: Option<usize>,
    now: u64,
    hold: u64,
    pause_until: Vec<u64>,
    schedule: Schedule,
    wire: BTreeMap<(u64, u64, u64), Event>,
    serial: u64, // tie collision only; NOT used for loss, delay or duplication
    counts: Counts,
    decisions: Vec<Option<u64>>,
    seen_bodies: BTreeSet<(usize, u64, Id)>,
    seen_records: BTreeSet<(usize, [u8; 64])>,
    seen_bulk: BTreeSet<(usize, [u8; 32])>,
}
impl Net {
    fn new(
        mode: Mode,
        weights: &[u64],
        body_len: usize,
        seed: u64,
        chaos: bool,
        cap: usize,
    ) -> Self {
        let c = config(weights);
        let n = weights.len();
        let nodes = (0..n)
            .map(|i| {
                let mut engine = Engine::boot(MemoryStore::default(), key(i), c.clone()).unwrap();
                engine.set_emission(Emission::Guided);
                let mut rec = Rec::default();
                engine.init(&c, 0, 0, &mut rec).unwrap();
                Node {
                    engine,
                    rec,
                    scheduler: Scheduler::new(mode, n),
                    generation: 1,
                    sequence: 0,
                    rx: AdmissionBudget::new(cap),
                    inbox: VecDeque::new(),
                }
            })
            .collect();
        Self {
            mode,
            config: c,
            body: body(body_len),
            nodes,
            offline: vec![false; n],
            unavailable: None,
            now: 0,
            hold: 0,
            pause_until: vec![0; n],
            schedule: Schedule::new(seed, chaos),
            wire: BTreeMap::new(),
            serial: 0,
            counts: Counts::default(),
            decisions: vec![None; n],
            seen_bodies: BTreeSet::new(),
            seen_records: BTreeSet::new(),
            seen_bulk: BTreeSet::new(),
        }
    }
    fn enqueue(&mut self, event: Event) {
        let (from, to) = event.endpoints();
        let lane = event.lane();
        let d = self.schedule.next(from, to, lane);
        if d.drop || (lane == Lane::Evidence && self.unavailable == Some(from)) {
            self.counts.wire_drops += 1;
            return;
        }
        let at = self.now + d.delay;
        self.serial += 1;
        if d.duplicate {
            self.counts.wire_duplicates += 1;
            self.wire
                .insert((at + 1, d.tie, self.serial), event.clone());
            self.serial += 1;
        }
        self.wire.insert((at, d.tie, self.serial), event);
        let responses = self.wire.values().filter(|e| e.owned_response()).count();
        self.counts.peak_wire_responses = self.counts.peak_wire_responses.max(responses);
    }
    fn send_frame(&mut self, from: usize, to: usize, tx: Arc<Transmission>) {
        self.counts.frames += 1;
        self.counts.records += tx.packet.records();
        self.counts.body_bytes += tx.packet.body_bytes();
        self.counts.bulk += tx.packet.bulk() as u64;
        self.counts.responses += tx._permit.is_some() as u64;
        self.enqueue(Event::Frame {
            from,
            to,
            generation: self.nodes[from].generation,
            tx,
        });
    }
    fn flush(&mut self, i: usize) {
        let packets = std::mem::take(&mut self.nodes[i].rec.packets);
        if self.offline[i] {
            return;
        }
        for packet in packets {
            assert_eq!(packet.target, Target::All, "Guided automatic target");
            assert!(!packet.bulk(), "no automatic body/certificate fallback");
            let tx = Arc::new(Transmission {
                packet,
                token: None,
                _permit: None,
            });
            for to in 0..self.nodes.len() {
                if to != i {
                    self.send_frame(i, to, Arc::clone(&tx));
                }
            }
        }
    }
    fn supply(&mut self, i: usize) {
        let n = &mut self.nodes[i];
        if let Some(tag) = n.rec.acquire.take() {
            if n.engine.proposal_tag() == Some(tag) {
                n.engine
                    .supply_proposal(tag, &self.body, Status::Valid, &mut n.rec)
                    .unwrap();
            }
        }
        self.flush(i);
    }
    fn record_receive(&mut self, to: usize, packet: &Packet) {
        if !packet.bulk() {
            return;
        }
        self.counts.processed_bulk += 1;
        let mut hash = blake3::Hasher::new();
        hash.update(format!("{:?}", packet.message).as_bytes());
        if let Some((header, body)) = &packet.proposal {
            if !self
                .seen_bodies
                .insert((to, header.content.height, header.content.id))
            {
                self.counts.processed_duplicate_body_bytes += body.len() as u64;
            }
            self.counts.processed_duplicate_records +=
                (!self.seen_records.insert((to, header.signature))) as u64;
            hash.update(&header.signature);
        }
        for vote in packet
            .outer
            .iter()
            .chain(packet.groups.iter().flat_map(|g| &g.1))
        {
            self.counts.processed_duplicate_records +=
                (!self.seen_records.insert((to, vote.signature))) as u64;
            hash.update(&vote.signature);
        }
        for (kind, _) in &packet.groups {
            hash.update(format!("{kind:?}").as_bytes());
        }
        self.counts.processed_duplicate_bulk +=
            (!self.seen_bulk.insert((to, *hash.finalize().as_bytes()))) as u64;
    }
    fn deliver_due(&mut self) {
        while self
            .wire
            .first_key_value()
            .is_some_and(|(key, _)| key.0 <= self.now)
        {
            let (_, event) = self.wire.pop_first().unwrap();
            self.counts.events += 1;
            assert!(
                self.counts.events < EVENT_LIMIT,
                "bounded simulation event limit"
            );
            let (_, to) = event.endpoints();
            if self.offline[to] {
                continue;
            }
            match event {
                Event::Control {
                    from,
                    to,
                    update,
                    request,
                } => {
                    assert_eq!(update.peer, peer(from));
                    let n = &mut self.nodes[to];
                    match &mut n.scheduler {
                        Scheduler::Sender { director, .. } => {
                            match director.update(update, self.now) {
                                Ok(()) => {}
                                Err(UpdateError::Stale) => self.counts.stale_statuses += 1,
                                Err(error) => panic!("director update: {error:?}"),
                            }
                        }
                        Scheduler::Receiver {
                            receiver,
                            responder,
                        } => {
                            match receiver.update(update, self.now) {
                                Ok(()) => {}
                                Err(UpdateError::Stale) => self.counts.stale_statuses += 1,
                                Err(error) => panic!("receiver update: {error:?}"),
                            }
                            // Count stale controls once, at the receiver table.
                            match responder.update(RequestStatus { update, request }, self.now) {
                                Ok(()) | Err(RequestUpdateError::Stale) => {}
                                Err(error) => panic!("responder update: {error:?}"),
                            }
                        }
                    }
                }
                Event::Frame {
                    from,
                    to,
                    generation,
                    tx,
                } => {
                    let n = &mut self.nodes[to];
                    if tx._permit.is_none() {
                        tx.packet.deliver(&mut n.engine, &mut n.rec);
                    } else {
                        if let Scheduler::Receiver { receiver, .. } = &n.scheduler {
                            let key = RequestKey {
                                peer: peer(from),
                                generation,
                                sequence: tx.token.unwrap(),
                            };
                            if !receiver.is_current(key, self.now) {
                                // Tokens classify the attempt, never authorize
                                // consensus. Late/unsolicited evidence still
                                // competes for the same physical RX budget.
                                self.counts.stale_responses += 1;
                            }
                        }
                        // Admission precedes the independent deep body/vote copy.
                        let Some(permit) = n.rx.try_acquire() else {
                            self.counts.rx_cap_drops += 1;
                            continue;
                        };
                        n.inbox.push_back(Received {
                            ready: self.now + self.hold,
                            generation: n.generation,
                            packet: tx.packet.clone(),
                            _permit: permit,
                        });
                    }
                }
            }
        }
    }
    fn process(&mut self) {
        for i in 0..self.nodes.len() {
            if self.offline[i] || self.now < self.pause_until[i] {
                continue;
            }
            // Same per-node service rate in both arms: one complete response/tick.
            if self.nodes[i]
                .inbox
                .front()
                .is_some_and(|p| p.ready <= self.now)
            {
                let received = self.nodes[i].inbox.pop_front().unwrap();
                if received.generation == self.nodes[i].generation {
                    self.record_receive(i, &received.packet);
                    let n = &mut self.nodes[i];
                    received.packet.deliver(&mut n.engine, &mut n.rec);
                }
                // owned response/permit is dropped here, NOT at logical timeout
            }
        }
    }
    fn poll(&mut self) {
        for i in 0..self.nodes.len() {
            if self.offline[i] {
                continue;
            }
            let n = &mut self.nodes[i];
            n.sequence += 1;
            let frames = match &mut n.scheduler {
                Scheduler::Sender { director, tx } => {
                    let mut sink = SenderReplies {
                        frames: vec![],
                        budget: tx,
                        dropped: 0,
                    };
                    // Director's legacy Sink cannot report admission failure.
                    // With K=1, gate busy TX just as Responder does, rather than
                    // unfairly manufacturing a failed grant/backoff in this arm.
                    if tx.in_use() < tx.capacity() {
                        director.poll(&n.engine, self.now, &mut sink).unwrap();
                    }
                    assert_eq!(sink.dropped, 0);
                    self.counts.tx_cap_drops += sink.dropped;
                    sink.frames
                }
                Scheduler::Receiver {
                    receiver,
                    responder,
                } => {
                    let started = receiver
                        .poll(n.engine.status().unwrap(), n.sequence, self.now)
                        .unwrap();
                    self.counts.requests_started += started.len() as u64;
                    let mut sink = Replies { frames: vec![] };
                    responder.poll(&n.engine, self.now, &mut sink).unwrap();
                    sink.frames
                }
            };
            // Sample before a network drop can release a newly accepted owner.
            self.counts.peak_tx_owned = self.counts.peak_tx_owned.max(n.scheduler.tx_owned());
            for (handle, tx) in frames {
                let to = (0..self.nodes.len()).find(|&j| peer(j) == handle).unwrap();
                self.send_frame(i, to, Arc::new(tx));
            }
        }
        // IDENTICAL status cadence: every online node advertises every tick in
        // both arms. Receiver request bits piggyback on the same status lane.
        for from in 0..self.nodes.len() {
            if self.offline[from] {
                continue;
            }
            for to in 0..self.nodes.len() {
                if from == to {
                    continue;
                }
                let n = &self.nodes[from];
                let request = match &n.scheduler {
                    Scheduler::Receiver { receiver, .. } => receiver.request_for(peer(to)),
                    Scheduler::Sender { .. } => None,
                };
                let update = StatusUpdate {
                    peer: peer(from),
                    generation: n.generation,
                    sequence: n.sequence,
                    status: n.engine.status().unwrap(),
                };
                self.counts.statuses += 1;
                self.counts.request_signals += request.is_some() as u64;
                self.enqueue(Event::Control {
                    from,
                    to,
                    update,
                    request,
                });
            }
        }
    }
    fn sample(&mut self) {
        for (i, n) in self.nodes.iter().enumerate() {
            let logical = n.scheduler.logical();
            assert!(logical <= K);
            self.counts.peak_logical = self.counts.peak_logical.max(logical);
            self.counts.peak_tx_owned = self.counts.peak_tx_owned.max(n.scheduler.tx_owned());
            self.counts.peak_rx_owned = self.counts.peak_rx_owned.max(n.rx.high_water());
            let bytes: usize = n.inbox.iter().map(|p| p.packet.payload_bytes()).sum();
            self.counts.peak_rx_packet_bytes = self.counts.peak_rx_packet_bytes.max(bytes);
            assert_eq!(n.rx.in_use(), n.inbox.len());
            assert!(n.rx.in_use() <= n.rx.capacity());
            assert!(n.scheduler.tx_owned() <= K);
            for slot in 0..3 {
                assert!(n.engine.storage().body(slot).len() <= self.config.max_body());
            }
            for region in 0..4 {
                assert!(n.engine.storage().region_rows(region).len() <= self.nodes.len());
            }
            assert_eq!(n.engine.prevotes().len(), self.nodes.len());
            assert_eq!(n.engine.precommits().len(), self.nodes.len());
            if let Some(&(height, id)) = n.rec.decisions.first() {
                assert_eq!(height, 0);
                assert_eq!(id, body_id(&self.body), "decision disagreement at node {i}");
                self.decisions[i].get_or_insert(self.now);
            }
        }
    }
    fn tick(&mut self) {
        self.now += 1;
        self.deliver_due();
        self.sample(); // observe ownership BEFORE service can release anything
        self.process();
        for i in 0..self.nodes.len() {
            if self.offline[i] {
                continue;
            }
            let n = &mut self.nodes[i];
            n.engine.elapse(1, &mut n.rec).unwrap();
            self.flush(i);
            self.supply(i);
        }
        self.poll();
        self.sample();
    }
    fn run(&mut self, limit: u64) {
        for _ in 0..limit {
            if self
                .decisions
                .iter()
                .enumerate()
                .all(|(i, t)| self.offline[i] || t.is_some())
            {
                return;
            }
            self.tick();
        }
        panic!(
            "{:?} failed to decide by tick {}: {:?}",
            self.mode,
            self.now,
            self.nodes
                .iter()
                .map(|n| n.engine.status())
                .collect::<Vec<_>>()
        );
    }
    fn star(&mut self) {
        // Setup is outside measurement. N-1 holders have the SAME real signed
        // decision package; the final node has never seen it. Every holder learns
        // that gap through the regular delayed status lane, not an oracle.
        let packet = decision(&self.config, &self.body);
        for i in 0..self.nodes.len() - 1 {
            let n = &mut self.nodes[i];
            packet.deliver(&mut n.engine, &mut n.rec);
            n.rec.packets.clear();
        }
        self.sample();
    }
    fn drain(&mut self, ticks: u64) {
        for _ in 0..ticks {
            self.tick();
        }
    }
    fn report(&self, case: &str) {
        println!(
            "D28_RECEIVER case={case} mode={:?} n={} body={} seed={} chaos={} cap={} hold={} evidence_delay={} decision_ticks={:?} counts={:?} units=typed_records_body_bytes_and_packet_owned_capacity_bytes_NOT_wire_bytes_or_RSS",
            self.mode,
            self.nodes.len(),
            self.body.len(),
            self.schedule.seed,
            self.schedule.chaos,
            self.nodes[0].rx.capacity(),
            self.hold,
            self.schedule.evidence_delay,
            self.decisions,
            self.counts
        );
    }
}

#[test]
fn paired_healthy_guided_modes_decide_with_identical_transport() {
    for n in [4, 8] {
        for mode in [Mode::Sender, Mode::Receiver] {
            let mut net = Net::new(mode, &vec![1; n], 512, 0xd28, false, K);
            net.run(1200);
            net.report("healthy");
        }
    }
}

#[test]
fn paired_star_catchup_reduces_duplicate_offers_not_a_general_dominance_claim() {
    for n in [4, 8] {
        for cap in [K, n] {
            let mut results = vec![];
            for mode in [Mode::Sender, Mode::Receiver] {
                let mut net = Net::new(mode, &vec![1; n], 4096, 0xd28, false, cap);
                net.star();
                // Equal fixed measurement horizon, including late copies after
                // the first decision; no hidden early stop of the cheaper arm.
                net.drain(2 * GRANT_TIMEOUT);
                assert!(net.decisions.iter().all(Option::is_some));
                net.report("star_fixed_window");
                results.push(net.counts);
            }
            let [sender, receiver] = results.as_slice() else {
                unreachable!()
            };
            assert_eq!(sender.bulk, n as u64 - 1);
            assert_eq!(receiver.bulk, 1);
            assert_eq!(sender.body_bytes, (n as u64 - 1) * 4096);
            assert_eq!(receiver.body_bytes, 4096);
            assert_eq!(receiver.peak_rx_owned, 1);
            if cap == K {
                assert_eq!(sender.rx_cap_drops, n as u64 - 2);
                assert_eq!(receiver.rx_cap_drops, 0);
            } else {
                assert_eq!(sender.peak_rx_owned, n - 1);
                assert_eq!(sender.processed_duplicate_bulk, n as u64 - 2);
                assert_eq!(receiver.processed_duplicate_bulk, 0);
            }
        }
    }
}

#[test]
fn paired_loss_duplication_and_reordering_are_reproducible() {
    for seed in [1, 9] {
        for mode in [Mode::Sender, Mode::Receiver] {
            let mut previous = None;
            for repeat in 0..2 {
                let mut net = Net::new(mode, &[1; 4], 512, seed, true, K);
                net.run(1600);
                net.drain(6);
                assert!(net.counts.wire_drops > 0);
                assert!(net.counts.wire_duplicates > 0);
                let result = (net.decisions.clone(), net.counts);
                if let Some(previous) = &previous {
                    assert_eq!(&result, previous);
                }
                if repeat == 0 {
                    net.report("chaos");
                }
                previous = Some(result);
            }
        }
    }
}

#[test]
fn paired_unavailable_first_source_fails_over_without_bulk_broadcast() {
    for mode in [Mode::Sender, Mode::Receiver] {
        let mut net = Net::new(mode, &[1; 4], 512, 0xd28, false, K);
        net.star();
        // Source 0 advertises the real decision but refuses all evidence. It is
        // the receiver's deterministic first (lowest-handle) source. The other
        // sources remain correct. This is a withholding fault, not forged votes.
        net.unavailable = Some(0);
        net.drain(3);
        if mode == Mode::Receiver {
            let Scheduler::Receiver { receiver, .. } = &net.nodes[3].scheduler else {
                unreachable!()
            };
            assert!(receiver.request_for(peer(0)).is_some());
            assert!(net.decisions[3].is_none());
        }
        net.run(200);
        if mode == Mode::Receiver {
            assert!(net.counts.requests_started >= 2);
            assert!(net.decisions[3].unwrap() > GRANT_TIMEOUT);
        }
        assert!(net.counts.wire_drops > 0);
        net.report("withholding_source");
    }
}

#[test]
fn paired_delayed_owned_responses_saturate_without_releasing_on_timeout() {
    for mode in [Mode::Sender, Mode::Receiver] {
        let mut net = Net::new(mode, &[1; 4], 4096, 0xd28, false, K);
        net.star();
        // The first admitted copy lives across several logical leases. New
        // attempts must neither free it nor allocate past the physical cap.
        net.hold = 3 * GRANT_TIMEOUT;
        net.drain(2 * GRANT_TIMEOUT);
        assert_eq!(net.nodes[3].rx.in_use(), 1);
        assert!(net.decisions[3].is_none());
        assert_eq!(net.counts.peak_rx_owned, K);
        assert!(net.counts.rx_cap_drops > 0);
        if mode == Mode::Receiver {
            assert!(net.counts.requests_started >= 2);
        }
        net.run(200);
        assert_eq!(net.counts.peak_rx_owned, K);
        net.report("receive_saturation");
    }
}

#[test]
fn paired_late_same_session_responses_are_valid_even_after_request_expiry() {
    for mode in [Mode::Sender, Mode::Receiver] {
        let mut net = Net::new(mode, &[1; 4], 512, 0xd28, false, K);
        net.star();
        net.schedule.evidence_delay = 2 * GRANT_TIMEOUT;
        net.run(200);
        if mode == Mode::Receiver {
            assert!(
                net.counts.stale_responses > 0,
                "exercise a canceled/expired token"
            );
        }
        assert!(net.decisions[3].unwrap() > GRANT_TIMEOUT);
        assert_eq!(net.counts.peak_rx_owned, K);
        net.report("late_same_session");
    }
}

#[test]
fn paired_weighted_cohort_decides_then_late_proposer_catches_up() {
    for mode in [Mode::Sender, Mode::Receiver] {
        let mut net = Net::new(mode, &[1, 3, 3, 3], 512, 19, true, K);
        net.offline[0] = true;
        net.run(1600);
        assert!(net.decisions[0].is_none());
        assert!(net.nodes[1].engine.view().unwrap().round > 0);
        let healed = net.now;
        net.offline[0] = false;
        net.run(800);
        println!(
            "D28_RECEIVER_CATCHUP mode={mode:?} healed={healed} catchup_ticks={}",
            net.decisions[0].unwrap() - healed
        );
        net.report("weighted_late_proposer");
    }
}
