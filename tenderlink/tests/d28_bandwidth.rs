//! Deterministic public-API bandwidth simulation: `Emission::Reference` vs
//! `Emission::CompactLive` (Fix #412).
//!
//! Units are TRANSMISSIONS of typed records, not wire bytes (there is no codec
//! here): a `Target::All` emission is counted as N-1 transmissions, a
//! `Target::Peer` emission as one. "sig records" = outer vote + every vote in
//! every group + the proposal header signature; "body bytes" = proposal body
//! length per transmission. Real ed25519 signatures; non-empty bodies.
//! Optional release benchmark (ignored):
//!   D28_BW_N=32,64 D28_BW_BODY=65536 cargo test --locked --release \
//!   --manifest-path tenderlink/Cargo.toml --test d28_bandwidth -- --ignored --nocapture
use ed25519_zebra::{SigningKey, VerificationKeyBytes};
use std::collections::VecDeque;
use tenderlink::d28::*;

const EVENT_LIMIT: usize = 2_000_000;
const RHO: u64 = 3;

fn key(i: usize) -> SigningKey {
    let mut b = [0x77; 32];
    b[..4].copy_from_slice(&(i as u32 + 1).to_le_bytes());
    SigningKey::from(b)
}
fn timeouts() -> Timeouts {
    Timeouts {
        propose: 30,
        prevote: 15,
        precommit: 25,
        inc_propose: 5,
        inc_prevote: 1,
        inc_precommit: 3,
    }
}
fn config(ids: &[usize], weights: &[u64]) -> Config {
    Config::new(
        [28; 32],
        ids.iter()
            .zip(weights)
            .map(|(&i, &weight)| Member {
                public_key: VerificationKeyBytes::from(&key(i)).into(),
                weight,
            })
            .collect(),
        1 << 20,
        RHO,
        timeouts(),
    )
    .unwrap()
}
fn body(len: usize, seed: u8) -> Vec<u8> {
    (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed))
        .collect()
}

#[derive(Clone)]
struct Packet {
    from: usize,
    target: Target,
    message: Message,
    outer: Option<Vote>,
    proposal: Option<(Header, Vec<u8>)>,
    groups: Vec<(CertKind, Vec<Vote>)>,
    periodic: bool,
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
}
#[derive(Default)]
struct Rec {
    from: usize,
    periodic: bool,
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
        });
    }
    fn decided(&mut self, h: u64, p: Proposal<'_>, _g: Group<'_>) {
        self.decisions.push((h, p.header.content.id));
    }
    fn acquire(&mut self, tag: ProposalTag) {
        self.acquire = Some(tag);
    }
}

#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
struct Stats {
    tx: u64,
    records: u64,
    body_bytes: u64,
    bare_initial_votes: u64,
    /// Transmissions / signature records per frame class:
    /// [ENTER, PREVOTE, PRECOMMIT, proposal-only, DEC].
    by_tx: [u64; 5],
    by_records: [u64; 5],
    relabelled_units: u64,
    errors: u64,
    events: u64,
}
#[derive(Clone, Copy, PartialEq)]
enum Loss {
    None,
    /// Drop every packet emitted outside a periodic rho replay (all initial,
    /// state-transition, reactive and proposer one-shot effects).
    AllLive,
}
struct Net {
    nodes: Vec<Option<(Engine<MemoryStore>, Rec)>>,
    ids: Vec<usize>, // member index -> node index
    queue: VecDeque<(usize, Packet)>,
    loss: Loss,
    shuffle: Option<u64>,
    offline: Vec<bool>,
    stats: Stats,
    body: Vec<u8>,
    elapsed: u64,
    /// Immutable owned copies of every PERIODIC emission (labelled by proof,
    /// see `tick_all`), whatever the loss policy did to delivery.
    plog: Vec<Packet>,
}
fn class(p: &Packet) -> usize {
    if p.message == Message::Dec {
        return 4;
    }
    match p.outer.as_ref().map(|v| v.content.kind) {
        Some(VoteKind::Enter) => 0,
        Some(VoteKind::Prevote) => 1,
        Some(VoteKind::Precommit) => 2,
        None => 3,
    }
}
impl Net {
    fn new(c: &Config, ids: &[usize], emission: Emission, body_len: usize) -> Self {
        let mut nodes: Vec<Option<(Engine<MemoryStore>, Rec)>> = vec![];
        let top = ids.iter().max().unwrap() + 1;
        nodes.resize_with(top, || None);
        for &i in ids {
            let mut e = Engine::boot(MemoryStore::default(), key(i), c.clone()).unwrap();
            e.set_emission(emission);
            let mut r = Rec {
                from: i,
                ..Rec::default()
            };
            assert!(e.init(c, 0, 0, &mut r).unwrap());
            nodes[i] = Some((e, r));
        }
        Net {
            offline: vec![false; top],
            nodes,
            ids: ids.to_vec(),
            queue: VecDeque::new(),
            loss: Loss::None,
            shuffle: None,
            stats: Stats::default(),
            body: body(body_len, 7),
            elapsed: 0,
            plog: vec![],
        }
    }
    fn flush(&mut self, i: usize) {
        let n = self.nodes.iter().flatten().count() as u64;
        let (_, r) = self.nodes[i].as_mut().unwrap();
        let out = std::mem::take(&mut r.packets);
        if self.offline[i] {
            return; // a silent node emits nothing at all
        }
        for p in out {
            let tx = match p.target {
                Target::All => n - 1,
                Target::Peer(_) => 1,
            };
            self.stats.tx += tx;
            self.stats.records += tx * p.records();
            self.stats.by_tx[class(&p)] += tx;
            self.stats.by_records[class(&p)] += tx * p.records();
            if p.periodic {
                self.plog.push(p.clone());
            }
            self.stats.body_bytes += tx * p.proposal.as_ref().map_or(0, |x| x.1.len() as u64);
            if !p.periodic
                && p.message == Message::Ordinary
                && p.outer.is_some()
                && p.proposal.is_none()
                && p.groups.is_empty()
            {
                self.stats.bare_initial_votes += tx;
            }
            if self.loss == Loss::AllLive && !p.periodic {
                continue;
            }
            match p.target {
                Target::All => {
                    for j in 0..self.nodes.len() {
                        if j != i && self.nodes[j].is_some() {
                            self.queue.push_back((j, p.clone()));
                        }
                    }
                }
                Target::Peer(m) => self.queue.push_back((self.ids[m as usize], p)),
            }
        }
    }
    fn supply(&mut self, i: usize) {
        let body = self.body.clone();
        if self.offline[i] {
            return;
        }
        let (e, r) = self.nodes[i].as_mut().unwrap();
        if let Some(tag) = r.acquire.take() {
            r.periodic = false;
            if e.proposal_tag() == Some(tag) {
                e.supply_proposal(tag, &body, Status::Valid, r).unwrap();
            }
        }
        self.flush(i);
    }
    fn elapse(&mut self, i: usize, d: u64, periodic: bool) {
        let (e, r) = self.nodes[i].as_mut().unwrap();
        r.periodic = periodic;
        e.elapse(d, r).unwrap();
        r.periodic = false;
        self.flush(i);
        self.supply(i);
    }
    /// Advance every online node by `d` units, one unit at a time. A unit is
    /// PERIODIC iff the node's rho grid fires in it (grid_remaining == 1 before
    /// it); this is read from the public descriptor, so grids that are not
    /// aligned across nodes (late joiners, new heights) are labelled exactly.
    /// Under `Loss::AllLive` we additionally assert the consensus state is
    /// unchanged across a periodic unit, so everything emitted inside it is a
    /// replay and nothing live can hide in it.
    fn tick_all(&mut self, d: u64) {
        self.elapsed += d;
        for i in 0..self.nodes.len() {
            if self.nodes[i].is_none() || self.offline[i] {
                continue;
            }
            for _ in 0..d {
                let fires = self.nodes[i]
                    .as_ref()
                    .unwrap()
                    .0
                    .view()
                    .unwrap()
                    .grid_remaining
                    == 1;
                if fires && self.loss == Loss::AllLive {
                    let before = normalized(&self.nodes[i].as_ref().unwrap().0);
                    let (e, r) = self.nodes[i].as_mut().unwrap();
                    r.periodic = true;
                    e.elapse(1, r).unwrap();
                    r.periodic = false;
                    // If the grid unit also changed semantic state (a timer
                    // expiry, arming, step, round, lock, ...), its sends are
                    // not provably pure replays: label ALL of them live, so
                    // AllLive drops them (this may drop a genuine replay too;
                    // the next stable grid unit services it).
                    if before != normalized(&e) {
                        for p in &mut r.packets {
                            p.periodic = false;
                        }
                        self.stats.relabelled_units += 1;
                    }
                    self.flush(i);
                    self.supply(i);
                } else {
                    self.elapse(i, 1, fires);
                }
            }
        }
    }
    fn pump(&mut self) {
        let mut rng = self.shuffle;
        while !self.queue.is_empty() {
            self.stats.events += 1;
            assert!((self.stats.events as usize) < EVENT_LIMIT, "event limit");
            let k = if let Some(s) = rng.as_mut() {
                *s = s
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                ((*s >> 33) as usize) % self.queue.len()
            } else {
                0
            };
            let (to, p) = self.queue.remove(k).unwrap();
            if self.offline[to] {
                continue;
            }
            let (e, r) = self.nodes[to].as_mut().unwrap();
            if let Err(error) = p.deliver(e, r) {
                self.stats.errors += 1;
                panic!(
                    "correct frame rejected: {}->{to}, message={:?}, outer={:?}: {error:?}",
                    p.from,
                    p.message,
                    p.outer.map(|v| v.content),
                );
            }
            self.flush(to);
            self.supply(to);
        }
    }
    fn settle(&mut self) {
        for i in 0..self.nodes.len() {
            if self.nodes[i].is_some() {
                self.flush(i);
                self.supply(i);
            }
        }
        self.pump();
    }
    fn decided(&self, h: u64) -> Vec<Option<Id>> {
        self.nodes
            .iter()
            .flatten()
            .map(|(_, r)| r.decisions.iter().find(|d| d.0 == h).map(|d| d.1))
            .collect()
    }
    fn all_decided(&self, h: u64) -> Option<Id> {
        let d = self.decided(h);
        let first = d[0]?;
        d.iter().all(|x| *x == Some(first)).then_some(first)
    }
    /// Healthy path: one start tick, then pump until quiescent.
    fn healthy(&mut self) {
        self.tick_all(1);
        self.settle();
    }
    /// Lossy path: after the start tick, only periodic rho offers get through.
    /// Returns the periods needed.
    fn periodic_until_decided(&mut self, h: u64, max_periods: u64) -> u64 {
        self.tick_all(1);
        self.settle();
        let mut periods = 0;
        // First tick of RHO-1 ends the first period (grid started at RHO, 1 used).
        let mut step = RHO - 1;
        while self.all_decided(h).is_none() {
            assert!(
                periods < max_periods,
                "no decision in {max_periods} periods"
            );
            self.tick_all(step);
            step = RHO;
            periods += 1;
            self.settle();
        }
        periods
    }
}
/// Full descriptor and vote rows, minus only the pure countdown values
/// (remaining durations); keeps timer active flags, locks, vc refs, intents.
fn normalized(e: &Engine<MemoryStore>) -> String {
    let mut d = e.view().unwrap().clone();
    d.grid_remaining = 0;
    d.height_remaining = 0;
    for t in &mut d.timers {
        t.remaining = 0;
    }
    format!("{d:?}/{}", snapshot(e))
}
fn snapshot(e: &Engine<MemoryStore>) -> String {
    let d = e.view().unwrap();
    let rows = |r: &[Option<Vote>]| {
        r.iter()
            .map(|v| v.map(|v| (v.content.round, v.content.id)))
            .collect::<Vec<_>>()
    };
    format!(
        "{:?}/{}/{:?}/{:?}/{:?}",
        d.step,
        d.round,
        rows(e.prevotes()),
        rows(e.precommits()),
        d.intents
    )
}
fn uniform(n: usize) -> (Config, Vec<usize>) {
    let ids: Vec<usize> = (0..n).collect();
    (config(&ids, &vec![1; n]), ids)
}
fn run_healthy(n: usize, em: Emission, body_len: usize) -> (Stats, u64) {
    let (c, ids) = uniform(n);
    let mut net = Net::new(&c, &ids, em, body_len);
    net.healthy();
    assert_eq!(net.stats.errors, 0, "{em:?} N={n}");
    let id = net.all_decided(0).expect("all decide in round 0");
    assert_eq!(id, body_id(&net.body));
    for (e, _) in net.nodes.iter().flatten() {
        assert_eq!(e.view().unwrap().round, 0);
    }
    // Zero mandatory timer wait: only the single start tick of 1 unit elapsed,
    // far below the propose timeout (30).
    assert_eq!(net.elapsed, 1);
    (net.stats, net.elapsed)
}

#[test]
fn default_emission_is_reference_and_recover_restores_it() {
    let (c, ids) = uniform(4);
    let mut net = Net::new(&c, &ids, Emission::CompactLive, 64);
    assert_eq!(
        net.nodes[1].as_ref().unwrap().0.emission(),
        Emission::CompactLive
    );
    let e = Engine::boot(MemoryStore::default(), key(0), c.clone()).unwrap();
    assert_eq!(e.emission(), Emission::Reference);
    net.healthy();
    let (e, mut r) = net.nodes[1].take().unwrap();
    let e = Engine::recover(e.into_storage(), key(1), &mut r).unwrap();
    assert_eq!(e.emission(), Emission::Reference);
}

#[test]
fn compact_live_initial_votes_are_bare_and_replay_is_complete() {
    let (c, ids) = uniform(4);
    let mut bares = vec![];
    for em in [Emission::Reference, Emission::CompactLive] {
        let mut net = Net::new(&c, &ids, em, 4096);
        net.healthy();
        bares.push(net.stats.bare_initial_votes);
    }
    // Reference only sends naturally evidence-free votes bare (round-0 ENTER,
    // prevotes for a missing/other value); CompactLive strictly more.
    assert!(bares[1] > bares[0], "{bares:?}");
    // Periodic replay from a node that has not decided: take a fresh network,
    // advance to the grid with no delivery, and inspect the complete frames.
    let mut net = Net::new(&c, &ids, Emission::CompactLive, 4096);
    net.loss = Loss::AllLive;
    net.tick_all(1);
    net.settle();
    let proposer = c.proposer(0, 0);
    // Live: proposer emitted a proposal, no bare vote carries a body.
    net.tick_all(RHO - 1);
    let kind = |p: &Packet| p.outer.as_ref().map(|v| v.content.kind);
    assert!(net.plog.iter().all(|p| p.periodic));
    assert!(net.plog.iter().any(|p| kind(p) == Some(VoteKind::Enter)));
    let prop_only: Vec<_> = net
        .plog
        .iter()
        .filter(|p| p.outer.is_none() && p.proposal.is_some())
        .collect();
    assert!(!prop_only.is_empty(), "periodic proposer proposal replayed");
    assert!(prop_only.iter().all(|p| p.from == proposer));
    // The proposer's own periodic PREVOTE carries the body and a current row.
    let pv = net
        .plog
        .iter()
        .find(|p| p.from == proposer && kind(p) == Some(VoteKind::Prevote))
        .expect("own prevote replayed");
    assert!(pv.proposal.is_some(), "prevote replay carries the body");
    assert!(
        !pv.groups.is_empty(),
        "prevote replay carries the current row"
    );
}

#[test]
fn healthy_round0_reduction_is_exact_and_needs_no_timer_wait() {
    for (n, body_len) in [
        (4usize, 4096usize),
        (8, 4096),
        (16, 4096),
        (4, 65536),
        (16, 65536),
    ] {
        let (r, _) = run_healthy(n, Emission::Reference, body_len);
        let (k, _) = run_healthy(n, Emission::CompactLive, body_len);
        println!(
            "D28_BW healthy n={n} body={body_len} ref_tx={} ref_records={} ref_body={} compact_tx={} compact_records={} compact_body={}",
            r.tx, r.records, r.body_bytes, k.tx, k.records, k.body_bytes
        );
        println!(
            "D28_BW classes n={n} [ENTER,PV,PC,proposal-only,DEC] ref_tx={:?} ref_rec={:?} compact_tx={:?} compact_rec={:?} (sig records and body bytes are typed-record counts, NOT wire bytes)",
            r.by_tx, r.by_records, k.by_tx, k.by_records
        );
        assert!(k.records < r.records, "n={n}");
        assert!(k.body_bytes < r.body_bytes, "n={n}");
        // Body: proposer's one broadcast (N-1) plus at most one reactive reply
        // per peer; Reference additionally re-sends it inside prevotes.
        assert!(
            k.body_bytes <= 2 * (n as u64 - 1) * body_len as u64,
            "n={n}"
        );
        assert!(k.tx <= r.tx);
    }
}

#[test]
fn compact_healthy_reduction_with_reordering_and_weights() {
    // Weighted roster, reordered delivery, both policies decide in round 0.
    let ids = [0usize, 1, 2, 3, 4];
    let c = config(&ids, &[5, 3, 2, 2, 1]);
    for seed in [1u64, 2, 3, 99] {
        for em in [Emission::Reference, Emission::CompactLive] {
            let mut net = Net::new(&c, &ids, em, 4096);
            net.shuffle = Some(seed);
            net.healthy();
            assert!(net.all_decided(0).is_some(), "{em:?} seed={seed}");
            assert_eq!(net.elapsed, 1);
        }
    }
}

#[test]
fn all_live_effects_dropped_still_decides_through_periodic_replay() {
    for n in [4usize, 7] {
        let mut periods = [0u64; 2];
        for (k, em) in [Emission::Reference, Emission::CompactLive]
            .into_iter()
            .enumerate()
        {
            let (c, ids) = uniform(n);
            let mut net = Net::new(&c, &ids, em, 4096);
            net.loss = Loss::AllLive;
            periods[k] = net.periodic_until_decided(0, 12);
            assert_eq!(net.all_decided(0), Some(body_id(&net.body)));
            // One more period so decided nodes replay their DEC package.
            net.tick_all(RHO);
            net.settle();
            // Owned periodic snapshots contain the full evidence even though
            // the engines have since advanced (and every live effect was lost).
            let has = |f: &dyn Fn(&Packet) -> bool| net.plog.iter().any(|p| f(p));
            let kind = |p: &Packet| p.outer.as_ref().map(|v| v.content.kind);
            assert!(has(&|p| kind(p) == Some(VoteKind::Enter)));
            assert!(has(&|p| p.outer.is_none()
                && p.proposal.is_some()
                && p.message == Message::Ordinary));
            assert!(has(&|p| kind(p) == Some(VoteKind::Prevote)
                && p.proposal.is_some()
                && p.groups.iter().any(
                    |g| matches!(g.0, CertKind::Row { .. }) && !g.1.is_empty()
                )));
            assert!(has(&|p| kind(p) == Some(VoteKind::Precommit)
                && p.proposal.is_some()
                && p.groups
                    .iter()
                    .any(
                        |g| matches!(g.0, CertKind::Polka { .. }) && !g.1.is_empty()
                    )));
            assert!(has(&|p| p.message == Message::Dec
                && p.proposal.is_some()
                && p.groups
                    .iter()
                    .any(|g| matches!(g.0, CertKind::Decision { .. }))));
            // No timer fired: total elapsed stays below the propose timeout.
            assert!(net.elapsed < 30, "{em:?} elapsed={}", net.elapsed);
            for (e, _) in net.nodes.iter().flatten() {
                assert_eq!(e.view().unwrap().round, 0);
            }
        }
        println!(
            "D28_BW loss n={n} ref_periods={} compact_periods={}",
            periods[0], periods[1]
        );
        assert_eq!(
            periods[0], periods[1],
            "periodic replay is policy independent"
        );
    }
}

#[test]
fn late_joiner_recovers_from_periodic_offers_under_total_live_loss() {
    let (c, ids) = uniform(4);
    let mut net = Net::new(&c, &ids, Emission::CompactLive, 4096);
    net.loss = Loss::AllLive;
    net.offline[3] = true; // weight 1 of 4: the other three are a quorum
    net.periodic_until_decided_subset(0, 12, 3);
    assert!(net.nodes[3].as_ref().unwrap().1.decisions.is_empty());
    net.offline[3] = false;
    let mut periods = 0;
    while net.nodes[3].as_ref().unwrap().1.decisions.is_empty() {
        assert!(periods < 12);
        net.tick_all(RHO);
        net.settle();
        periods += 1;
    }
    assert_eq!(net.all_decided(0), Some(body_id(&net.body)));
}
impl Net {
    fn periodic_until_decided_subset(&mut self, h: u64, max: u64, upto: usize) {
        self.tick_all(1);
        self.settle();
        let mut step = RHO - 1;
        let mut periods = 0;
        while !self.decided(h)[..upto].iter().all(|d| d.is_some()) {
            assert!(periods < max);
            self.tick_all(step);
            step = RHO;
            periods += 1;
            self.settle();
        }
    }
}

#[test]
fn roster_rotation_recovers_under_compact_live_and_total_live_loss() {
    let ids_a = [0usize, 1, 2, 3];
    let a = config(&ids_a, &[1, 1, 1, 1]);
    let ids_b = [4usize, 2, 1, 3, 0]; // rotated order, new member 4, weights
    let b = config(&ids_b, &[1, 4, 2, 2, 2]);
    let mut net = Net::new(&a, &ids_a, Emission::CompactLive, 4096);
    net.healthy();
    assert!(net.all_decided(0).is_some());
    // New roster at height 1; node 4 joins fresh. Only periodic offers pass.
    let top = 5;
    net.nodes.resize_with(top, || None);
    net.offline.resize(top, false);
    let mut e = Engine::boot(MemoryStore::default(), key(4), b.clone()).unwrap();
    e.set_emission(Emission::CompactLive);
    net.nodes[4] = Some((
        e,
        Rec {
            from: 4,
            ..Rec::default()
        },
    ));
    net.ids = ids_b.to_vec();
    net.body = body(4096, 9);
    net.loss = Loss::AllLive;
    for i in 0..top {
        let (e, r) = net.nodes[i].as_mut().unwrap();
        let first = i == 4;
        assert!(e.init(&b, 1, 0, r).unwrap(), "node {i} first={first}");
    }
    net.tick_all(1);
    net.settle();
    let mut periods = 0;
    let mut step = RHO - 1;
    while net.all_decided(1).is_none() {
        assert!(periods < 12, "rotated roster did not decide");
        net.tick_all(step);
        step = RHO;
        periods += 1;
        net.settle();
    }
    assert_eq!(net.all_decided(1), Some(body_id(&net.body)));
}

#[test]
fn weighted_offline_proposer_node0_silent_until_tick_120() {
    // Analogue of d28_public's weighted [1,4,3,2]: member 0 (weight 1, the
    // round-0 proposer) sends/receives nothing until tick 120. Weight 9/10
    // is a quorum, so 1..3 decide without it; after healing it adopts the DEC.
    let ids = [0usize, 1, 2, 3];
    let c = config(&ids, &[1, 4, 3, 2]);
    for em in [Emission::Reference, Emission::CompactLive] {
        let mut net = Net::new(&c, &ids, em, 4096);
        net.offline[0] = true;
        let mut t = 0;
        while !net.decided(0)[1..].iter().all(|d| d.is_some()) {
            assert!(t < 120, "{em:?}: 1..3 must decide before tick 120");
            net.tick_all(1);
            net.settle();
            t += 1;
        }
        assert!(net.nodes[0].as_ref().unwrap().1.decisions.is_empty());
        assert!(net.nodes[0].as_ref().unwrap().1.packets.is_empty());
        let decided = net.decided(0)[1].unwrap();
        while net.elapsed < 120 {
            net.tick_all(1);
            net.settle();
            assert!(net.nodes[0].as_ref().unwrap().1.decisions.is_empty());
        }
        assert_eq!(net.elapsed, 120);
        net.offline[0] = false;
        let mut periods = 0;
        while net.nodes[0].as_ref().unwrap().1.decisions.is_empty() {
            assert!(periods < 30, "{em:?}: node 0 did not adopt");
            net.tick_all(RHO);
            net.settle();
            periods += 1;
        }
        assert_eq!(net.all_decided(0), Some(decided));
        println!("D28_BW offline_proposer {em:?} decided_at_tick={t} adopt_periods={periods}");
    }
}

#[test]
#[ignore]
fn bandwidth_benchmark() {
    let ns: Vec<usize> = std::env::var("D28_BW_N")
        .unwrap_or("4,8,16,32,64".into())
        .split(',')
        .map(|x| x.parse().unwrap())
        .collect();
    let body_len: usize = std::env::var("D28_BW_BODY")
        .ok()
        .and_then(|x| x.parse().ok())
        .unwrap_or(4096);
    for n in ns {
        let (r, _) = run_healthy(n, Emission::Reference, body_len);
        let (k, _) = run_healthy(n, Emission::CompactLive, body_len);
        println!(
            "D28_BW n={n} body={body_len} ref_tx={} ref_records={} ref_body={} compact_tx={} compact_records={} compact_body={}",
            r.tx, r.records, r.body_bytes, k.tx, k.records, k.body_bytes
        );
    }
}
