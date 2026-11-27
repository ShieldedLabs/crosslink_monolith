//! Guided-only, deterministic, public-API integration model (real Ed25519).
//!
//! There is deliberately no transport, WAL, codec, round replay journal, or
//! automatic complete-frame broadcast fallback here. The two host lanes are
//! latest status and bare votes. Complete packages are produced ONLY by an
//! explicit per-sender Director, K=1, and routed by opaque host peer handles.
//! Counts below are typed signature-record TRANSMISSIONS and body bytes, NOT
//! encoded wire bytes. Status transmissions are counted separately.
use ed25519_zebra::{SigningKey, VerificationKeyBytes};
use std::collections::VecDeque;
use tenderlink::d28::{gossip::*, *};

const RHO: u64 = 3;
const K: usize = 1;
const EVENT_LIMIT: u64 = 2_000_000;
fn key(i: usize) -> SigningKey {
    let mut b = [0x91; 32];
    b[..4].copy_from_slice(&(i as u32 + 1).to_le_bytes());
    SigningKey::from(b)
}
fn config(ids: &[usize], weights: &[u64]) -> Config {
    Config::new(
        [0x28; 32],
        ids.iter()
            .zip(weights)
            .map(|(&i, &weight)| Member {
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
fn director(n: usize) -> Director {
    Director::new(DirectorConfig {
        max_peers: n,
        max_grants: K,
        grant_timeout: 6,
        status_ttl: 90,
    })
}
// Opaque host handles, deliberately NOT member indices or vector offsets.
fn peer(i: usize) -> u32 {
    1009 + 17 * i as u32
}
#[derive(Clone, Debug)]
struct Packet {
    from: usize,
    target: Target,
    message: Message,
    outer: Option<Vote>,
    proposal: Option<(Header, Vec<u8>)>,
    groups: Vec<(CertKind, Vec<Vote>)>,
    periodic: bool,
    guided_by: bool,
}
impl Packet {
    fn deliver(&self, e: &mut Engine<MemoryStore>, s: &mut Rec) -> Result<()> {
        let groups: Vec<_> = self
            .groups
            .iter()
            .map(|(kind, votes)| Group {
                kind: *kind,
                votes: Votes::Dense(votes),
            })
            .collect();
        e.receive(
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
            s,
        )
    }
    fn records(&self) -> u64 {
        self.outer.is_some() as u64
            + self.proposal.is_some() as u64
            + self.groups.iter().map(|g| g.1.len() as u64).sum::<u64>()
    }
    fn bulk(&self) -> bool {
        self.proposal.is_some() || !self.groups.is_empty() || self.message == Message::Dec
    }
}
#[derive(Default)]
struct Rec {
    from: usize,
    periodic: bool,
    guided_by: bool,
    packets: Vec<Packet>,
    decisions: Vec<(u64, Id)>,
    acquire: Option<ProposalTag>,
}
impl Sink for Rec {
    fn send(&mut self, target: Target, f: Frame<'_>) {
        self.packets.push(Packet {
            from: self.from,
            target,
            message: f.message,
            outer: f.outer.copied(),
            proposal: f.proposal.map(|p| (*p.header, p.body.to_vec())),
            groups: f
                .groups
                .iter()
                .map(|g| (g.kind, g.votes.iter().copied().collect()))
                .collect(),
            periodic: self.periodic,
            guided_by: self.guided_by,
        });
    }
    fn acquire(&mut self, tag: ProposalTag) {
        self.acquire = Some(tag);
    }
    fn decided(&mut self, h: u64, p: Proposal<'_>, g: Group<'_>) {
        assert!(matches!(g.kind, CertKind::Decision { id, .. } if id == p.header.content.id));
        self.decisions.push((h, p.header.content.id));
    }
}
struct Node {
    e: Engine<MemoryStore>,
    r: Rec,
    d: Director,
    generation: u64,
    sequence: u64,
}
#[derive(Clone)]
enum Event {
    Frame {
        to: usize,
        p: Packet,
    },
    Status {
        from: usize,
        to: usize,
        update: StatusUpdate,
    },
}
#[derive(Default, Clone, Copy, Debug)]
struct Counts {
    frames: u64,
    records: u64,
    body_bytes: u64,
    status: u64,
    bulk: u64,
    targeted: u64,
    periodic_bare: u64,
    dropped_packages: u64,
    stale_status: u64,
    events: u64,
}
impl Counts {
    fn since(self, before: Self) -> Self {
        Self {
            frames: self.frames - before.frames,
            records: self.records - before.records,
            body_bytes: self.body_bytes - before.body_bytes,
            status: self.status - before.status,
            bulk: self.bulk - before.bulk,
            targeted: self.targeted - before.targeted,
            periodic_bare: self.periodic_bare - before.periodic_bare,
            dropped_packages: self.dropped_packages - before.dropped_packages,
            stale_status: self.stale_status - before.stale_status,
            events: self.events - before.events,
        }
    }
}
struct Net {
    nodes: Vec<Option<Node>>,
    ids: Vec<usize>,
    offline: Vec<bool>,
    queue: VecDeque<Event>,
    emission: Emission,
    now: u64,
    body: Vec<u8>,
    counts: Counts,
    lose_live: bool,
    chaos: bool,
    serial: u64,
    rng: u64,
    dec_deliveries: u64,
}
// Ignore pure countdowns, but NOT timer arming, rows, intents, locks or step.
fn semantic(e: &Engine<MemoryStore>) -> String {
    let mut d = e.view().unwrap().clone();
    d.grid_remaining = 0;
    d.height_remaining = 0;
    for t in &mut d.timers {
        t.remaining = 0;
    }
    format!("{d:?}/{:?}/{:?}", e.prevotes(), e.precommits())
}
impl Net {
    fn new(ids: &[usize], weights: &[u64], emission: Emission, body_len: usize) -> Self {
        let c = config(ids, weights);
        let top = ids.iter().max().unwrap() + 1;
        let mut nodes = Vec::new();
        nodes.resize_with(top, || None);
        for &i in ids {
            let mut e = Engine::boot(MemoryStore::default(), key(i), c.clone()).unwrap();
            e.set_emission(emission);
            let mut r = Rec {
                from: i,
                ..Rec::default()
            };
            assert!(e.init(&c, 0, 0, &mut r).unwrap());
            nodes[i] = Some(Node {
                e,
                r,
                d: director(ids.len()),
                generation: 1,
                sequence: 0,
            });
        }
        Self {
            nodes,
            ids: ids.to_vec(),
            offline: vec![false; top],
            queue: VecDeque::new(),
            emission,
            now: 0,
            body: (0..body_len).map(|i| (i as u8).wrapping_mul(31)).collect(),
            counts: Counts::default(),
            lose_live: false,
            chaos: false,
            serial: 0,
            rng: 0xd28,
            dec_deliveries: 0,
        }
    }
    fn uniform(n: usize, em: Emission) -> Self {
        Self::new(&(0..n).collect::<Vec<_>>(), &vec![1; n], em, 4096)
    }
    fn enqueue(&mut self, event: Event) {
        self.serial += 1;
        // Each lane suffers deterministic loss and duplication, independently of
        // consensus state. A later periodic offer/status, not an implicit ACK,
        // is needed after a lost complete package.
        if self.chaos && self.serial % 11 == 0 {
            if matches!(&event, Event::Frame { p, .. } if p.bulk()) {
                self.counts.dropped_packages += 1;
            }
            return;
        }
        if self.chaos && self.serial % 7 == 0 {
            self.queue.push_back(event.clone());
        }
        self.queue.push_back(event);
    }
    fn flush(&mut self, i: usize) {
        let out = std::mem::take(&mut self.nodes[i].as_mut().unwrap().r.packets);
        if self.offline[i] {
            return;
        }
        for p in out {
            if self.emission == Emission::Guided {
                match p.target {
                    Target::All => {
                        assert!(!p.bulk(), "Guided bulk broadcast: {p:?}");
                        assert_eq!(p.message, Message::Ordinary);
                        assert!(p.outer.is_some(), "empty guided broadcast");
                    }
                    Target::Peer(_) => assert!(p.guided_by, "non-Director guided send: {p:?}"),
                }
            }
            let recipients = match p.target {
                Target::All => self
                    .ids
                    .iter()
                    .copied()
                    .filter(|&j| j != i)
                    .collect::<Vec<_>>(),
                Target::Peer(handle) if self.emission == Emission::Guided => vec![
                    *self
                        .ids
                        .iter()
                        .find(|&&j| peer(j) == handle)
                        .expect("target must resolve through CURRENT host roster mapping"),
                ],
                // CompactLive's core reactive reply addresses a member index;
                // Director addresses an opaque host handle, a separate namespace.
                Target::Peer(member) => vec![self.ids[member as usize]],
            };
            let tx = recipients.len() as u64;
            self.counts.frames += tx;
            self.counts.records += tx * p.records();
            self.counts.body_bytes += tx * p.proposal.as_ref().map_or(0, |p| p.1.len() as u64);
            self.counts.bulk += tx * p.bulk() as u64;
            self.counts.targeted += tx * matches!(p.target, Target::Peer(_)) as u64;
            self.counts.periodic_bare += tx * (p.periodic && !p.bulk()) as u64;
            if self.lose_live && !p.periodic && !p.guided_by {
                continue;
            }
            for to in recipients {
                self.enqueue(Event::Frame { to, p: p.clone() });
            }
        }
    }
    fn supply(&mut self, i: usize) {
        if self.offline[i] {
            return;
        }
        let n = self.nodes[i].as_mut().unwrap();
        if let Some(tag) = n.r.acquire.take() {
            if n.e.proposal_tag() == Some(tag) {
                n.e.supply_proposal(tag, &self.body, Status::Valid, &mut n.r)
                    .unwrap();
            }
        }
        self.flush(i);
    }
    fn statuses(&mut self) {
        for from in self.ids.clone() {
            if self.offline[from] {
                continue;
            }
            let n = self.nodes[from].as_mut().unwrap();
            n.sequence += 1;
            let update = StatusUpdate {
                peer: peer(from),
                generation: n.generation,
                sequence: n.sequence,
                status: n.e.status().unwrap(),
            };
            for to in self.ids.clone() {
                if to != from {
                    self.counts.status += 1;
                    self.enqueue(Event::Status { from, to, update });
                }
            }
        }
    }
    fn poll(&mut self) {
        if self.emission != Emission::Guided {
            return;
        }
        for i in self.ids.clone() {
            if self.offline[i] {
                continue;
            }
            let n = self.nodes[i].as_mut().unwrap();
            n.r.guided_by = true;
            let grants = n.d.poll(&mut n.e, self.now, &mut n.r).unwrap();
            n.r.guided_by = false;
            assert!(grants.len() <= K);
            assert!(n.d.active_grants() <= K);
            assert!(n.d.peers() <= self.ids.len());
            self.flush(i);
        }
    }
    fn pump(&mut self) {
        while !self.queue.is_empty() {
            self.counts.events += 1;
            assert!(self.counts.events < EVENT_LIMIT);
            let at = if self.chaos {
                self.rng = self.rng.wrapping_mul(6364136223846793005).wrapping_add(1);
                (self.rng as usize >> 16) % self.queue.len()
            } else {
                0
            };
            match self.queue.remove(at).unwrap() {
                Event::Status { from, to, update } => {
                    if self.offline[to] {
                        continue;
                    }
                    assert_eq!(update.peer, peer(from));
                    match self.nodes[to].as_mut().unwrap().d.update(update, self.now) {
                        Ok(()) => {}
                        Err(UpdateError::Stale) => self.counts.stale_status += 1,
                        Err(e) => panic!("unexpected status error {from}->{to}: {e:?}"),
                    }
                }
                Event::Frame { to, p } => {
                    if self.offline[to] {
                        continue;
                    }
                    if let Target::Peer(handle) = p.target {
                        if self.emission == Emission::Guided {
                            assert_eq!(handle, peer(to));
                        } else {
                            assert_eq!(self.ids[handle as usize], to);
                        }
                    }
                    let n = self.nodes[to].as_mut().unwrap();
                    p.deliver(&mut n.e, &mut n.r).unwrap_or_else(|e| {
                        panic!(
                            "unexpected receive error at t={} {}->{to}: {e:?}; {p:?}",
                            self.now, p.from
                        )
                    });
                    self.dec_deliveries += (p.message == Message::Dec) as u64;
                    self.flush(to);
                    self.supply(to);
                }
            }
        }
    }
    fn tick(&mut self) {
        self.now += 1;
        for i in self.ids.clone() {
            if self.offline[i] {
                continue;
            }
            let n = self.nodes[i].as_mut().unwrap();
            let before = semantic(&n.e);
            n.r.periodic = n.e.view().unwrap().grid_remaining == 1;
            n.e.elapse(1, &mut n.r).unwrap();
            n.r.periodic = false;
            // Mixed timer-expiry/grid units are not counted as pure periodic.
            if self.lose_live && before != semantic(&n.e) {
                for p in &mut n.r.packets {
                    p.periodic = false;
                }
            }
            self.flush(i);
            self.supply(i);
        }
        self.pump();
        if self.now % RHO == 0 {
            self.statuses();
            self.pump();
        }
        self.poll();
        self.pump();
        self.assert_consistent_decisions();
    }
    fn assert_consistent_decisions(&self) {
        let mut seen = std::collections::BTreeMap::new();
        for n in self.nodes.iter().flatten() {
            for &(h, id) in &n.r.decisions {
                if let Some(old) = seen.insert(h, id) {
                    assert_eq!(old, id, "decision divergence at height {h}");
                }
            }
        }
    }
    fn decided(&self, i: usize, h: u64) -> Option<Id> {
        self.nodes[i]
            .as_ref()
            .unwrap()
            .r
            .decisions
            .iter()
            .find(|x| x.0 == h)
            .map(|x| x.1)
    }
    fn run(&mut self, h: u64, limit: u64) {
        for _ in 0..limit {
            if self
                .ids
                .iter()
                .all(|&i| self.offline[i] || self.decided(i, h).is_some())
            {
                return;
            }
            self.tick();
        }
        let states: Vec<_> = self
            .ids
            .iter()
            .map(|&i| (i, self.nodes[i].as_ref().unwrap().e.status()))
            .collect();
        panic!(
            "no decision by t={} (bounded test, not a liveness proof): {states:?}",
            self.now
        );
    }
    fn restart(&mut self, i: usize) {
        let n = self.nodes[i].take().unwrap();
        let mut r = n.r;
        r.packets.clear();
        r.acquire = None;
        // Mode must be set INSIDE recovery, before it can emit anything.
        let e = Engine::recover_with_emission(n.e.into_storage(), key(i), self.emission, &mut r)
            .unwrap();
        assert_eq!(e.emission(), self.emission);
        self.nodes[i] = Some(Node {
            e,
            r,
            d: director(self.ids.len()),
            generation: n.generation + 1,
            sequence: 0,
        });
        self.flush(i);
        self.supply(i);
    }
    fn next_height(&mut self, ids: &[usize], h: u64) {
        assert!(self.queue.is_empty());
        let c = config(ids, &vec![1; ids.len()]);
        let top = ids.iter().max().unwrap() + 1;
        self.nodes.resize_with(self.nodes.len().max(top), || None);
        self.offline.resize(self.nodes.len(), false);
        for &i in ids {
            if self.nodes[i].is_none() {
                let mut e = Engine::boot(MemoryStore::default(), key(i), c.clone()).unwrap();
                e.set_emission(self.emission);
                self.nodes[i] = Some(Node {
                    e,
                    r: Rec {
                        from: i,
                        ..Rec::default()
                    },
                    d: director(ids.len()),
                    generation: 1,
                    sequence: 0,
                });
            }
            let n = self.nodes[i].as_mut().unwrap();
            n.d = director(ids.len());
            n.generation += 1;
            n.sequence = 0;
            assert!(n.e.init(&c, h, 0, &mut n.r).unwrap());
        }
        self.ids = ids.to_vec();
        for i in self.ids.clone() {
            self.flush(i);
        }
    }
}

#[test]
fn guided_healthy_and_compact_decisions_agree_without_periodic_bulk() {
    for n in [4, 8] {
        let mut guided = Net::uniform(n, Emission::Guided);
        let mut compact = Net::uniform(n, Emission::CompactLive);
        guided.run(0, 250);
        compact.run(0, 250);
        for i in 0..n {
            assert_eq!(guided.decided(i, 0), compact.decided(i, 0));
            let a = guided.nodes[i].as_ref().unwrap().e.status().unwrap();
            let b = compact.nodes[i].as_ref().unwrap().e.status().unwrap();
            assert_eq!(
                (a.config, a.height, a.round, a.step),
                (b.config, b.height, b.round, b.step),
                "final consensus-state divergence"
            );
            // DEC can pin a pending decision without filling the round slot.
            // Record, rather than conceal or reject, path-dependent evidence.
            if a != b {
                println!(
                    "GUIDED evidence-state divergence n={n} node={i} guided={a:?} compact={b:?}"
                );
            }
        }
        assert!(guided.counts.targeted > 0);
        println!(
            "GUIDED healthy n={n} guided={:?} compact={:?} (typed records/body bytes; NOT wire bytes)",
            guided.counts, compact.counts
        );
        // Drain stale predecision statuses and wait for fresh caught-up status.
        for _ in 0..30 {
            guided.tick();
            compact.tick();
        }
        let g = guided.counts;
        let c = compact.counts;
        for _ in 0..30 {
            guided.tick();
            compact.tick();
        }
        let gd = guided.counts.since(g);
        let cd = compact.counts.since(c);
        assert_eq!(
            (gd.bulk, gd.body_bytes),
            (0, 0),
            "caught-up Guided must have no bulk replay"
        );
        assert!(
            cd.bulk > 0 && cd.body_bytes > 0,
            "CompactLive replays DEC after decision"
        );
        println!("GUIDED caught-up n={n} guided={gd:?} compact={cd:?}");
    }
}

#[test]
fn guided_periodic_bare_and_status_recover_all_lost_initial_outputs() {
    let mut net = Net::uniform(4, Emission::Guided);
    net.lose_live = true;
    net.chaos = true;
    net.run(0, 1500);
    assert!(net.counts.periodic_bare > 0);
    assert!(net.counts.targeted > 0);
    assert!(
        net.counts.dropped_packages > 0,
        "exercise dropped complete packages"
    );
    assert!(
        net.counts.stale_status > 0,
        "exercise duplicate/reordered latest status"
    );
    for i in 0..4 {
        assert_eq!(net.decided(i, 0), Some(body_id(&net.body)));
    }
}

#[test]
fn absent_proposer_weighted_cohort_and_late_node_targeted_dec() {
    // Nodes 1,2,3 carry 9/10 total weight (>2/3); round-0 proposer 0 is absent.
    let mut net = Net::new(&[0, 1, 2, 3], &[1, 3, 3, 3], Emission::Guided, 512);
    net.offline[0] = true;
    net.chaos = true;
    net.run(0, 1800);
    assert!(net.nodes[1].as_ref().unwrap().e.view().unwrap().round > 0);
    assert_eq!(net.decided(0, 0), None);
    net.offline[0] = false;
    let before = net.dec_deliveries;
    net.run(0, 1500);
    assert!(
        net.dec_deliveries > before,
        "late node must receive targeted complete DEC"
    );
    assert_eq!(net.decided(0, 0), net.decided(1, 0));
}

#[test]
fn restart_and_roster_replacement_preserve_guided_routing() {
    let mut net = Net::uniform(4, Emission::Guided);
    // Recovery while active must not transiently emit Reference packages.
    for _ in 0..3 {
        net.tick();
    }
    net.restart(1);
    net.chaos = true;
    net.run(0, 1500);
    net.restart(2); // and recovery of an already decided node
    // New member 4, reordered member indices, removed member 0.
    net.next_height(&[3, 1, 4, 2], 1);
    net.run(1, 1500);
    assert_eq!(net.decided(0, 1), None);
    for i in [3, 1, 4, 2] {
        assert_eq!(net.decided(i, 1), Some(body_id(&net.body)));
    }
    // Remove another old member, reintroduce 0 with a different index.
    net.next_height(&[4, 0, 2, 3], 2);
    net.run(2, 1500);
    assert_eq!(net.decided(1, 2), None);
    for i in [4, 0, 2, 3] {
        assert_eq!(net.decided(i, 2), Some(body_id(&net.body)));
    }
}

fn signed_vote(c: &Config, member: usize, round: u64, kind: VoteKind, id: Option<Id>) -> Vote {
    Vote::sign(
        c,
        &key(member),
        VoteContent {
            height: 0,
            round,
            kind,
            id,
            signer: member as u32,
        },
    )
}
fn frame(v: Vote) -> Packet {
    Packet {
        from: v.content.signer as usize,
        target: Target::All,
        message: Message::Ordinary,
        outer: Some(v),
        proposal: None,
        groups: vec![],
        periodic: false,
        guided_by: false,
    }
}

#[test]
fn future_bare_votes_are_not_banked_and_laggard_needs_complete_entry() {
    let c = config(&[0, 1, 2, 3], &[1; 4]);
    let mut net = Net::uniform(4, Emission::Guided);
    net.tick();
    let n = net.nodes[3].as_mut().unwrap();
    let before = semantic(&n.e);
    for i in 0..3 {
        for kind in [VoteKind::Enter, VoteKind::Prevote, VoteKind::Precommit] {
            frame(signed_vote(&c, i, 7, kind, None))
                .deliver(&mut n.e, &mut n.r)
                .unwrap();
        }
    }
    assert_eq!(
        semantic(&n.e),
        before,
        "future bare votes must not change retained state"
    );
    assert!(
        n.e.prevotes()
            .iter()
            .flatten()
            .all(|v| v.content.round != 7)
    );
    // Drive a separate sender to round 7 using a complete, signed ENTRY bridge.
    let entries: Vec<_> = (0..3)
        .map(|i| signed_vote(&c, i, 7, VoteKind::Enter, None))
        .collect();
    let jump = Packet {
        from: 0,
        target: Target::Peer(peer(2)),
        message: Message::Ordinary,
        outer: Some(entries[0]),
        proposal: None,
        groups: vec![(CertKind::Jump { round: 7 }, entries)],
        periodic: false,
        guided_by: true,
    };
    let sender = net.nodes[2].as_mut().unwrap();
    jump.deliver(&mut sender.e, &mut sender.r).unwrap();
    assert_eq!(sender.e.view().unwrap().round, 7);
    let lag = net.nodes[3].as_ref().unwrap().e.status().unwrap();
    let sender = net.nodes[2].as_mut().unwrap();
    sender.r.packets.clear();
    sender.r.guided_by = true;
    sender
        .d
        .update(
            StatusUpdate {
                peer: peer(3),
                generation: 1,
                sequence: 1,
                status: lag,
            },
            1,
        )
        .unwrap();
    let grants = sender.d.poll(&mut sender.e, 1, &mut sender.r).unwrap();
    sender.r.guided_by = false;
    assert_eq!(grants, vec![(peer(3), Gap::Round)]);
    let package = sender.r.packets.pop().unwrap();
    assert_eq!(package.target, Target::Peer(peer(3)));
    assert!(!package.groups.is_empty());
    let receiver = net.nodes[3].as_mut().unwrap();
    package.deliver(&mut receiver.e, &mut receiver.r).unwrap();
    assert_eq!(receiver.e.view().unwrap().round, 7);
    // The old future votes were not recalled after the jump.
    assert!(receiver.e.prevotes().iter().all(Option::is_none));
    assert!(receiver.e.precommits().iter().all(Option::is_none));
}

fn jump_package(c: &Config, r: u64) -> Packet {
    let votes: Vec<_> = (0..3)
        .map(|i| signed_vote(c, i, r, VoteKind::Enter, None))
        .collect();
    Packet {
        from: 0,
        target: Target::Peer(peer(2)),
        message: Message::Ordinary,
        outer: Some(votes[0]),
        proposal: None,
        groups: vec![(CertKind::Jump { round: r }, votes)],
        periodic: false,
        guided_by: true,
    }
}
fn proposal_package(c: &Config, r: u64, vr: Option<u64>, body: &[u8]) -> Packet {
    let signer = c.proposer(0, r);
    let h = Header::sign(
        c,
        &key(signer),
        ProposalContent {
            height: 0,
            round: r,
            valid_round: vr,
            id: body_id(body),
            signer: signer as u32,
        },
    );
    Packet {
        from: signer,
        target: Target::Peer(peer(2)),
        message: Message::Ordinary,
        outer: None,
        proposal: Some((h, body.to_vec())),
        groups: vec![],
        periodic: false,
        guided_by: true,
    }
}
fn beam(net: &mut Net, from: usize, to: usize, now: u64, sequence: u64) -> Vec<Packet> {
    let status = net.nodes[to].as_ref().unwrap().e.status().unwrap();
    let n = net.nodes[from].as_mut().unwrap();
    n.r.packets.clear();
    n.r.guided_by = true;
    n.d.update(
        StatusUpdate {
            peer: peer(to),
            generation: 1,
            sequence,
            status,
        },
        now,
    )
    .unwrap();
    n.d.poll(&mut n.e, now, &mut n.r).unwrap();
    n.r.guided_by = false;
    assert!(n.d.active_grants() <= K);
    std::mem::take(&mut n.r.packets)
}

#[test]
fn lower_round_gets_current_self_carrying_quorum_not_predecessor_timeout() {
    let c = config(&[0, 1, 2, 3], &[1; 4]);
    let mut net = Net::uniform(4, Emission::Guided);
    net.tick();
    let n = net.nodes[2].as_mut().unwrap();
    jump_package(&c, 7).deliver(&mut n.e, &mut n.r).unwrap();
    let mut p = proposal_package(&c, 7, None, b"round-seven");
    p.groups.push((
        CertKind::Polka {
            round: 7,
            id: body_id(b"round-seven"),
        },
        (0..3)
            .map(|i| signed_vote(&c, i, 7, VoteKind::Prevote, Some(body_id(b"round-seven"))))
            .collect(),
    ));
    p.deliver(&mut n.e, &mut n.r).unwrap();
    assert_eq!(n.e.view().unwrap().step, Step::Precommit);
    let packages = beam(&mut net, 2, 3, 1, 1);
    assert_eq!(packages.len(), 1);
    let p = &packages[0];
    assert_eq!(p.target, Target::Peer(peer(3)));
    assert_eq!(p.outer.as_ref().unwrap().content.kind, VoteKind::Precommit);
    assert!(
        p.groups
            .iter()
            .any(|g| matches!(g.0, CertKind::Polka { round: 7, .. }))
    );
    let n = net.nodes[3].as_mut().unwrap();
    let before = n.e.view().unwrap().round;
    assert!(before < 7);
    p.deliver(&mut n.e, &mut n.r).unwrap();
    assert_eq!(n.e.view().unwrap().round, 7);
    assert_eq!(
        n.e.view().unwrap().step,
        Step::Precommit,
        "one complete current quorum repairs entry and polka without waiting for a predecessor TPC"
    );
}

#[test]
fn held_body_missing_justification_is_a_real_gap_and_loss_is_not_ack() {
    let c = config(&[0, 1, 2, 3], &[1; 4]);
    let mut net = Net::uniform(4, Emission::Guided);
    net.tick();
    // The source must actually RETAIN the old valid-round certificate, not
    // merely have validated an incoming witness after jumping past its round.
    let n = net.nodes[2].as_mut().unwrap();
    jump_package(&c, 6).deliver(&mut n.e, &mut n.r).unwrap();
    let mut old = proposal_package(&c, 6, None, b"held-unjustified");
    old.groups.push((
        CertKind::Polka {
            round: 6,
            id: body_id(b"held-unjustified"),
        },
        (0..3)
            .map(|i| {
                signed_vote(
                    &c,
                    i,
                    6,
                    VoteKind::Prevote,
                    Some(body_id(b"held-unjustified")),
                )
            })
            .collect(),
    ));
    old.deliver(&mut n.e, &mut n.r).unwrap();
    assert_eq!(
        n.e.status().unwrap().polka,
        Some((6, body_id(b"held-unjustified")))
    );
    for i in [2, 3] {
        let n = net.nodes[i].as_mut().unwrap();
        jump_package(&c, 7).deliver(&mut n.e, &mut n.r).unwrap();
    }
    let mut p = proposal_package(&c, 7, Some(6), b"held-unjustified");
    let receiver = net.nodes[3].as_mut().unwrap();
    p.deliver(&mut receiver.e, &mut receiver.r).unwrap();
    let st = receiver.e.status().unwrap();
    assert_eq!(st.proposal, Some(body_id(b"held-unjustified")));
    assert!(!st.justified);
    p.groups.push((
        CertKind::Polka {
            round: 6,
            id: body_id(b"held-unjustified"),
        },
        (0..3)
            .map(|i| {
                signed_vote(
                    &c,
                    i,
                    6,
                    VoteKind::Prevote,
                    Some(body_id(b"held-unjustified")),
                )
            })
            .collect(),
    ));
    let sender = net.nodes[2].as_mut().unwrap();
    p.deliver(&mut sender.e, &mut sender.r).unwrap();
    assert!(sender.e.status().unwrap().justified);
    let first = beam(&mut net, 2, 3, 1, 1);
    assert_eq!(first.len(), 1);
    assert!(first[0].bulk());
    // Drop first package. A newer status that repeats the SAME gap is not ACK.
    assert_eq!(net.nodes[3].as_ref().unwrap().e.status().unwrap(), st);
    assert!(beam(&mut net, 2, 3, 2, 2).is_empty());
    assert_eq!(net.nodes[2].as_ref().unwrap().d.active_grants(), 1);
    assert!(
        beam(&mut net, 2, 3, 7, 3).is_empty(),
        "timeout starts backoff, not success"
    );
    let retry = beam(&mut net, 2, 3, 19, 4);
    assert_eq!(
        retry.len(),
        1,
        "same gap must be retried after loss and bounded backoff"
    );
    let n = net.nodes[3].as_mut().unwrap();
    retry[0].deliver(&mut n.e, &mut n.r).unwrap();
    if !n.e.status().unwrap().justified {
        // A retained current PV quorum has higher priority than the exact
        // older valid-round justification. A second explicit beam repairs it.
        assert!(
            n.e.status().unwrap().prevote_quorum,
            "retry={retry:?}, receiver={:?}",
            n.e.status()
        );
        let next = beam(&mut net, 2, 3, 20, 5);
        assert_eq!(next.len(), 1);
        let n = net.nodes[3].as_mut().unwrap();
        next[0].deliver(&mut n.e, &mut n.r).unwrap();
    }
    let st = net.nodes[3].as_ref().unwrap().e.status().unwrap();
    assert!(st.justified);
    assert!(matches!(st.step, Step::Prevote | Step::Precommit));
}

#[test]
fn status_generation_reordering_and_peer_churn_stay_bounded() {
    let mut net = Net::uniform(4, Emission::Guided);
    net.tick();
    let status = net.nodes[1].as_ref().unwrap().e.status().unwrap();
    let n = net.nodes[2].as_mut().unwrap();
    let u = StatusUpdate {
        peer: peer(1),
        generation: 2,
        sequence: 8,
        status,
    };
    n.d.update(u, 1).unwrap();
    assert_eq!(
        n.d.update(StatusUpdate { sequence: 7, ..u }, 2),
        Err(UpdateError::Stale)
    );
    assert_eq!(
        n.d.update(
            StatusUpdate {
                generation: 1,
                sequence: 99,
                ..u
            },
            3
        ),
        Err(UpdateError::Stale)
    );
    n.d.update(
        StatusUpdate {
            generation: 3,
            sequence: 0,
            ..u
        },
        4,
    )
    .unwrap();
    for i in 10..110 {
        n.d.remove(peer(1));
        n.d.update(
            StatusUpdate {
                peer: peer(1),
                generation: i,
                sequence: 0,
                status,
            },
            i,
        )
        .unwrap();
        assert_eq!(n.d.peers(), 1);
        assert!(n.d.active_grants() <= K);
    }
}

#[test]
fn many_rounds_keep_only_fixed_roles_and_replay_current_votes() {
    let c = config(&[0, 1, 2, 3], &[1; 4]);
    let mut net = Net::uniform(4, Emission::Guided);
    net.tick();
    let n = net.nodes[2].as_mut().unwrap();
    for round in 1..=40 {
        jump_package(&c, round).deliver(&mut n.e, &mut n.r).unwrap();
        assert_eq!(n.e.view().unwrap().round, round);
        assert_eq!(n.e.prevotes().len(), 4);
        assert_eq!(n.e.precommits().len(), 4);
        for region in 0..4 {
            assert!(n.e.storage().region_rows(region).len() <= 4);
        }
        for slot in 0..3 {
            assert!(n.e.storage().body(slot).len() <= c.max_body());
        }
        assert_eq!(
            n.e.storage().own(VoteKind::Enter).unwrap().content.round,
            round
        );
        // The host never archives outputs for replay on behalf of the engine.
        n.r.packets.clear();
    }
    n.e.elapse(RHO, &mut n.r).unwrap();
    assert!(!n.r.packets.is_empty());
    for p in &n.r.packets {
        assert!(!p.bulk());
        assert_eq!(
            p.outer.unwrap().content.round,
            40,
            "no old-round replay history"
        );
    }
}

#[test]
#[ignore = "release benchmark: cargo test --release --test d28_guided -- --ignored --nocapture"]
fn guided_release_benchmark() {
    use std::time::Instant;
    for n in [4, 16, 32, 64] {
        for mode in [Emission::CompactLive, Emission::Guided] {
            let start = Instant::now();
            let mut net = Net::uniform(n, mode);
            net.run(0, 1000);
            let ticks = net.now;
            let rounds: Vec<_> = net
                .ids
                .iter()
                .map(|&i| net.nodes[i].as_ref().unwrap().e.view().unwrap().round)
                .collect();
            let healthy = net.counts;
            let body_copies = healthy.body_bytes / 4096;
            println!(
                "GUIDED_BENCH dissemination n={n} mode={mode:?} decision_ticks={ticks} rounds={rounds:?} body_copies={body_copies} excess_body_copies_over_nminus1={}",
                body_copies.saturating_sub(n as u64 - 1)
            );
            // A fresh status period drains predecision hints; retain two
            // additional rho periods at large N to avoid millions of redundant
            // real-signature DEC checks in this optional scaling benchmark.
            let periods = if n < 16 { 10 } else { 2 };
            for _ in 0..RHO {
                net.tick();
            }
            let before = net.counts;
            for _ in 0..periods * RHO {
                net.tick();
            }
            let caught_up = net.counts.since(before);
            if mode == Emission::Guided {
                assert_eq!((caught_up.bulk, caught_up.body_bytes), (0, 0));
            } else {
                assert!(caught_up.bulk > 0 && caught_up.body_bytes > 0);
            }
            println!(
                "GUIDED_BENCH n={n} body=4096 mode={mode:?} healthy={healthy:?} caught_up_periods={periods} caught_up={caught_up:?} elapsed_ms={} units=typed_signature_record_transmissions_and_body_bytes_NOT_wire_bytes status_count_separate=true",
                start.elapsed().as_millis()
            );
        }
    }
}
