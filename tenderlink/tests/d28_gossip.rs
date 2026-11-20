//! Status-directed evidence service (d28::gossip) public-API tests.
use ed25519_zebra::{SigningKey, VerificationKeyBytes};
use tenderlink::d28::*;
fn key(i: u8) -> SigningKey {
    SigningKey::from([i.wrapping_add(1); 32])
}
fn config(ids: &[u8], weights: &[u64]) -> Config {
    Config::new(
        [42; 32],
        ids.iter()
            .zip(weights)
            .map(|(&i, &weight)| Member {
                public_key: VerificationKeyBytes::from(&key(i)).into(),
                weight,
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
    .unwrap()
}
fn vote(c: &Config, i: usize, k: u8, h: u64, r: u64, kind: VoteKind, id: Option<Id>) -> Vote {
    Vote::sign(
        c,
        &key(k),
        VoteContent {
            height: h,
            round: r,
            kind,
            id,
            signer: i as u32,
        },
    )
}
fn header(c: &Config, k: u8, h: u64, r: u64, vr: Option<u64>, body: &[u8]) -> Header {
    Header::sign(
        c,
        &key(k),
        ProposalContent {
            height: h,
            round: r,
            valid_round: vr,
            id: body_id(body),
            signer: c.proposer(h, r) as u32,
        },
    )
}
#[derive(Clone)]
struct Packet {
    target: Target,
    message: Message,
    outer: Option<Vote>,
    proposal: Option<(Header, Vec<u8>)>,
    groups: Vec<(CertKind, Vec<Vote>)>,
}
impl Packet {
    fn deliver(&self, e: &mut Engine<MemoryStore>, s: &mut Recorder) -> Result<()> {
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
}
#[derive(Default)]
struct Recorder {
    packets: Vec<Packet>,
    decisions: Vec<(u64, Id)>,
    acquired: Vec<ProposalTag>,
}
impl Sink for Recorder {
    fn send(&mut self, target: Target, f: Frame<'_>) {
        self.packets.push(Packet {
            target,
            message: f.message,
            outer: f.outer.copied(),
            proposal: f.proposal.map(|p| (*p.header, p.body.to_vec())),
            groups: f
                .groups
                .iter()
                .map(|g| (g.kind, g.votes.iter().copied().collect()))
                .collect(),
        });
    }
    fn acquire(&mut self, tag: ProposalTag) {
        self.acquired.push(tag);
    }
    fn decided(&mut self, h: u64, p: Proposal<'_>, g: Group<'_>) {
        assert_eq!(
            g.kind,
            CertKind::Decision {
                round: p.header.content.round,
                id: p.header.content.id
            }
        );
        self.decisions.push((h, p.header.content.id));
    }
}
fn engine(c: &Config, k: u8, tau: u64) -> (Engine<MemoryStore>, Recorder) {
    let mut e = Engine::boot(MemoryStore::default(), key(k), c.clone()).unwrap();
    let mut s = Recorder::default();
    assert!(e.init(c, 0, tau, &mut s).unwrap());
    (e, s)
}
fn ordinary<'a>(
    outer: Option<&'a Vote>,
    proposal: Option<Proposal<'a>>,
    groups: &'a [Group<'a>],
) -> Frame<'a> {
    Frame {
        message: Message::Ordinary,
        outer,
        proposal,
        groups,
    }
}
use tenderlink::d28::gossip::*;

fn st(e: &Engine<MemoryStore>) -> StatusSnapshot {
    e.status().unwrap()
}
/// Engine for member k that has accepted the round-0 proposal and prevoted.
fn with_proposal(c: &Config, k: u8, body: &[u8]) -> (Engine<MemoryStore>, Recorder, Header) {
    let (mut e, mut s) = engine(c, k, 0);
    let h = header(c, c.proposer(0, 0) as u8, 0, 0, None, body);
    e.receive(
        ordinary(None, Some(Proposal { header: &h, body }), &[]),
        Status::Valid,
        &mut s,
    )
    .unwrap();
    s.packets.clear();
    (e, s, h)
}
fn polka_votes(c: &Config, id: Id) -> Vec<Vote> {
    (0..3)
        .map(|i| vote(c, i, i as u8, 0, 0, VoteKind::Prevote, Some(id)))
        .collect()
}
fn dcfg(n: usize, k: usize) -> DirectorConfig {
    DirectorConfig {
        max_peers: n,
        max_grants: k,
        grant_timeout: 10,
        status_ttl: 1000,
    }
}
fn upd(peer: u32, generation: u64, sequence: u64, status: StatusSnapshot) -> StatusUpdate {
    StatusUpdate {
        peer,
        generation,
        sequence,
        status,
    }
}

#[test]
fn lagging_peer_is_served_one_proposal_and_uptodate_peer_is_not() {
    let c = config(&[0, 1, 2, 3], &[1; 4]);
    let (e, mut s, h) = with_proposal(&c, 3, b"body");
    let (mut r, mut rs) = engine(&c, 2, 0);
    let mine = st(&e);
    assert_eq!(mine.proposal, Some(h.content.id));
    let lagging = StatusSnapshot {
        proposal: None,
        ..st(&r)
    };
    assert_eq!(lagging.round, 0);
    // Up-to-date peer: nothing sent.
    assert_eq!(
        e.serve(9, &mine, &mut s).unwrap(),
        Served::Skipped(Skip::NoGap)
    );
    assert!(s.packets.is_empty());
    // Lagging peer: exactly one complete frame, targeted.
    assert_eq!(
        e.serve(7, &lagging, &mut s).unwrap(),
        Served::Sent(Gap::Proposal)
    );
    assert_eq!(s.packets.len(), 1);
    assert_eq!(s.packets[0].target, Target::Peer(7));
    assert!(r.view().unwrap().slot.is_none());
    s.packets[0].deliver(&mut r, &mut rs).unwrap();
    assert!(r.view().unwrap().slot.is_some());
    assert_eq!(st(&r).proposal, Some(h.content.id));
    // The requested state change is visible in the peer's next status.
    assert_eq!(
        e.serve(7, &st(&r), &mut s).unwrap(),
        Served::Skipped(Skip::NoGap)
    );
}

#[test]
fn decision_package_decides_receiver_and_decided_claims_get_nothing() {
    let c = config(&[0, 1, 2, 3], &[1; 4]);
    let (mut e, mut s, h) = with_proposal(&c, 3, b"decide me");
    let pcs: Vec<_> = (0..3)
        .map(|i| {
            vote(
                &c,
                i,
                i as u8,
                0,
                0,
                VoteKind::Precommit,
                Some(h.content.id),
            )
        })
        .collect();
    let groups = [Group {
        kind: CertKind::Decision {
            round: 0,
            id: h.content.id,
        },
        votes: Votes::Dense(&pcs),
    }];
    let p = Proposal {
        header: &h,
        body: b"decide me",
    };
    e.receive(
        Frame {
            message: Message::Dec,
            outer: None,
            proposal: Some(p),
            groups: &groups,
        },
        Status::Valid,
        &mut s,
    )
    .unwrap();
    assert_eq!(s.decisions.len(), 1);
    s.packets.clear();
    let (mut r, mut rs) = engine(&c, 2, 0);
    let behind = st(&r);
    assert_eq!(
        e.serve(5, &behind, &mut s).unwrap(),
        Served::Sent(Gap::Decision)
    );
    assert_eq!(s.packets.len(), 1);
    assert_eq!(s.packets[0].message, Message::Dec);
    s.packets[0].deliver(&mut r, &mut rs).unwrap();
    assert_eq!(rs.decisions, [(0, h.content.id)]);
    // A peer that claims to be decided is not served (and we never trust it
    // as a certificate: nothing about our own state changes).
    s.packets.clear();
    assert_eq!(
        e.serve(5, &st(&r), &mut s).unwrap(),
        Served::Skipped(Skip::NoGap)
    );
    assert!(s.packets.is_empty());
}

#[test]
fn round_entry_package_catches_up_lower_round() {
    let c = config(&[0, 1, 2, 3], &[1; 4]);
    let (mut e, mut s) = engine(&c, 2, 0);
    let future = [
        vote(&c, 0, 0, 0, 1, VoteKind::Prevote, None),
        vote(&c, 1, 1, 0, 1, VoteKind::Prevote, None),
    ];
    let jump = [Group {
        kind: CertKind::Row {
            round: 1,
            kind: VoteKind::Prevote,
        },
        votes: Votes::Dense(&future),
    }];
    e.receive(
        ordinary(Some(&future[0]), None, &jump),
        Status::Indeterminate,
        &mut s,
    )
    .unwrap();
    assert_eq!(st(&e).round, 1);
    s.packets.clear();
    let (mut r, mut rs) = engine(&c, 3, 0);
    assert_eq!(st(&r).round, 0);
    assert_eq!(
        e.serve(1, &st(&r), &mut s).unwrap(),
        Served::Sent(Gap::Round)
    );
    assert_eq!(s.packets.len(), 1);
    s.packets[0].deliver(&mut r, &mut rs).unwrap();
    assert_eq!(st(&r).round, 1);
}

#[test]
fn polka_package_repairs_polluted_rows_and_moves_lock() {
    let c = config(&[0, 1, 2, 3], &[1; 4]);
    let body = b"polka body";
    let (mut e, mut s, h) = with_proposal(&c, 3, body);
    let id = h.content.id;
    let votes = polka_votes(&c, id);
    let g = [Group {
        kind: CertKind::Polka { round: 0, id },
        votes: Votes::Dense(&votes),
    }];
    e.receive(ordinary(Some(&votes[0]), None, &g), Status::Valid, &mut s)
        .unwrap();
    assert_eq!(e.view().unwrap().locked_round, Some(0));
    s.packets.clear();
    // Receiver holds the proposal, a polluted row, and no polka.
    let (mut r, mut rs) = engine(&c, 2, 0);
    r.receive(
        ordinary(None, Some(Proposal { header: &h, body }), &[]),
        Status::Valid,
        &mut rs,
    )
    .unwrap();
    let polluted = vote(&c, 1, 1, 0, 0, VoteKind::Prevote, Some([7; 32]));
    r.receive(ordinary(Some(&polluted), None, &[]), Status::Valid, &mut rs)
        .unwrap();
    assert_eq!(r.prevotes()[1].unwrap().content.id, Some([7; 32]));
    assert_eq!(
        e.serve(4, &st(&r), &mut s).unwrap(),
        Served::Sent(Gap::Polka)
    );
    assert_eq!(s.packets.len(), 1);
    assert!(s.packets[0].proposal.is_some());
    s.packets[0].deliver(&mut r, &mut rs).unwrap();
    assert_eq!(r.prevotes()[1].unwrap().content.id, Some(id));
    assert_eq!(r.view().unwrap().locked_round, Some(0));
    assert_eq!(st(&r).polka, Some((0, id)));
    assert_eq!(
        e.serve(4, &st(&r), &mut s).unwrap(),
        Served::Skipped(Skip::NoGap)
    );
}

#[test]
fn configuration_and_height_are_separated_and_fail_closed() {
    let c = config(&[0, 1, 2, 3], &[1; 4]);
    let other = config(&[0, 1, 2], &[1; 3]);
    let (e, mut s, _) = with_proposal(&c, 3, b"x");
    let mut lag = StatusSnapshot {
        proposal: None,
        ..st(&e)
    };
    lag.config = [9; 32];
    assert_eq!(
        e.serve(1, &lag, &mut s).unwrap(),
        Served::Skipped(Skip::Foreign)
    );
    lag.config = st(&e).config;
    lag.height = 1;
    assert_eq!(
        e.serve(1, &lag, &mut s).unwrap(),
        Served::Skipped(Skip::Ahead)
    );
    let (e1, _) = engine(&other, 2, 0);
    assert_ne!(st(&e1).config, st(&e).config);
    let mut s1 = Recorder::default();
    assert_eq!(
        e.serve(1, &st(&e1), &mut s1).unwrap(),
        Served::Skipped(Skip::Foreign)
    );
    assert!(s.packets.is_empty() && s1.packets.is_empty());
}

#[test]
fn latest_status_wins_and_old_or_duplicate_are_rejected() {
    let c = config(&[0, 1, 2, 3], &[1; 4]);
    let (e, _, _) = with_proposal(&c, 3, b"x");
    let a = st(&e);
    let mut d = Director::new(dcfg(4, 2));
    d.update(upd(1, 5, 10, a), 0).unwrap();
    assert_eq!(d.update(upd(1, 5, 10, a), 0), Err(UpdateError::Stale));
    assert_eq!(d.update(upd(1, 5, 9, a), 0), Err(UpdateError::Stale));
    assert_eq!(d.update(upd(1, 4, 99, a), 0), Err(UpdateError::Stale));
    d.update(upd(1, 5, 11, a), 0).unwrap();
    d.update(upd(1, 6, 0, a), 0).unwrap(); // new transport session
    assert_eq!(d.peers(), 1);
    // Latest status controls service: lagging then caught up.
    let lag = StatusSnapshot {
        proposal: None,
        ..a
    };
    let mut d = Director::new(dcfg(4, 2));
    let mut s = Recorder::default();
    d.update(upd(1, 1, 1, lag), 0).unwrap();
    d.update(upd(1, 1, 2, a), 1).unwrap();
    assert!(d.poll(&e, 1, &mut s).unwrap().is_empty());
    assert!(s.packets.is_empty());
    assert_eq!(d.update(upd(1, 1, 1, lag), 1), Err(UpdateError::Stale));
    assert!(d.poll(&e, 2, &mut s).unwrap().is_empty());
}

#[test]
fn bounded_grants_timeout_and_fair_ties() {
    let c = config(&[0, 1, 2, 3], &[1; 4]);
    let (e, mut s, _) = with_proposal(&c, 3, b"x");
    let lag = StatusSnapshot {
        proposal: None,
        ..st(&e)
    };
    let mut d = Director::new(dcfg(50, 3));
    for p in 0..50u32 {
        d.update(upd(p, 1, 1, lag), 0).unwrap();
    }
    assert_eq!(d.update(upd(50, 1, 1, lag), 0), Err(UpdateError::Full));
    let first = d.poll(&e, 0, &mut s).unwrap();
    assert_eq!(first.len(), 3);
    assert_eq!(s.packets.len(), 3);
    assert_eq!(d.active_grants(), 3);
    // Outstanding grants block more sends; no growth with N.
    assert!(d.poll(&e, 5, &mut s).unwrap().is_empty());
    assert_eq!(s.packets.len(), 3);
    // After the timeout the unresponsive peers are not retried ahead of fresh ones.
    let second = d.poll(&e, 10, &mut s).unwrap();
    assert_eq!(second.len(), 3);
    for (p, _) in &second {
        assert!(!first.iter().any(|(q, _)| q == p), "pinned to {p}");
    }
    // Eventually every peer was tried before an unresponsive one repeats.
    let mut seen: std::collections::HashSet<u32> = HashSetFrom::from(&first, &second);
    let mut now = 10;
    while seen.len() < 50 {
        now += 10;
        for (p, _) in d.poll(&e, now, &mut s).unwrap() {
            assert!(seen.insert(p) || seen.len() == 50);
        }
        assert!(now < 1000);
    }
    assert!(d.active_grants() <= 3);
}
struct HashSetFrom;
impl HashSetFrom {
    fn from(a: &[(u32, Gap)], b: &[(u32, Gap)]) -> std::collections::HashSet<u32> {
        a.iter().chain(b).map(|x| x.0).collect()
    }
}

#[test]
fn state_changing_gap_has_priority_and_progress_releases_grant() {
    let c = config(&[0, 1, 2, 3], &[1; 4]);
    let (e, mut s, _) = with_proposal(&c, 3, b"x");
    let (r, _) = engine(&c, 2, 0);
    let a = st(&e);
    let no_prop = StatusSnapshot {
        proposal: None,
        ..a
    };
    let early = StatusSnapshot {
        step: Step::Propose,
        ..a
    };
    let mut d = Director::new(dcfg(8, 1));
    d.update(upd(1, 1, 1, early), 0).unwrap();
    d.update(upd(2, 1, 1, no_prop), 0).unwrap();
    d.update(upd(3, 1, 1, a), 0).unwrap();
    let _ = r;
    let got = d.poll(&e, 0, &mut s).unwrap();
    assert_eq!(got, [(2, Gap::Proposal)]);
    // Sending alone does not convince: still granted, nothing else sent.
    assert!(d.poll(&e, 1, &mut s).unwrap().is_empty());
    // A newer status showing the gap closed releases the grant.
    d.update(upd(2, 1, 2, a), 2).unwrap();
    let got = d.poll(&e, 2, &mut s).unwrap();
    assert!(got.iter().all(|(p, _)| *p != 2));
}

/// Local node: polka for X at round 0 retained, then moved to round 1.
fn round1_with_polka() -> (Config, Engine<MemoryStore>, Recorder, Id) {
    let c = config(&[0, 1, 2, 3], &[1; 4]);
    let (mut e, mut s, h) = with_proposal(&c, 3, b"x");
    let id = h.content.id;
    let votes = polka_votes(&c, id);
    let g = [Group {
        kind: CertKind::Polka { round: 0, id },
        votes: Votes::Dense(&votes),
    }];
    e.receive(ordinary(Some(&votes[0]), None, &g), Status::Valid, &mut s)
        .unwrap();
    let future = [
        vote(&c, 0, 0, 0, 1, VoteKind::Prevote, None),
        vote(&c, 1, 1, 0, 1, VoteKind::Prevote, None),
    ];
    let jump = [Group {
        kind: CertKind::Row {
            round: 1,
            kind: VoteKind::Prevote,
        },
        votes: Votes::Dense(&future),
    }];
    e.receive(
        ordinary(Some(&future[0]), None, &jump),
        Status::Indeterminate,
        &mut s,
    )
    .unwrap();
    assert_eq!(st(&e).round, 1);
    s.packets.clear();
    (c, e, s, id)
}

#[test]
fn form_b_proposal_is_served_with_its_justifying_polka_and_after_vc_advanced() {
    let (c, mut e, mut s, id) = round1_with_polka();
    let body = b"x";
    let h = header(&c, c.proposer(0, 1) as u8, 0, 1, Some(0), body);
    assert_eq!(h.content.id, id);
    let g0v = polka_votes(&c, id);
    let g0 = [Group {
        kind: CertKind::Polka { round: 0, id },
        votes: Votes::Dense(&g0v),
    }];
    e.receive(
        ordinary(None, Some(Proposal { header: &h, body }), &g0),
        Status::Valid,
        &mut s,
    )
    .unwrap();
    s.packets.clear();
    let lag = StatusSnapshot {
        proposal: None,
        polka: None,
        ..st(&e)
    };
    assert_eq!(
        e.serve(5, &lag, &mut s).unwrap(),
        Served::Sent(Gap::Proposal)
    );
    let kinds: Vec<_> = s.packets[0].groups.iter().map(|g| g.0).collect();
    assert_eq!(kinds, [CertKind::Polka { round: 0, id }]);
    assert_eq!(s.packets[0].proposal.as_ref().unwrap().0, h);
    // The receiver validates the whole frame and is justified.
    let (mut r, mut rs) = engine(&c, 2, 0);
    let rj = [
        vote(&c, 0, 0, 0, 1, VoteKind::Prevote, None),
        vote(&c, 1, 1, 0, 1, VoteKind::Prevote, None),
    ];
    let rg = [Group {
        kind: CertKind::Row {
            round: 1,
            kind: VoteKind::Prevote,
        },
        votes: Votes::Dense(&rj),
    }];
    r.receive(
        ordinary(Some(&rj[0]), None, &rg),
        Status::Indeterminate,
        &mut rs,
    )
    .unwrap();
    s.packets[0].deliver(&mut r, &mut rs).unwrap();
    assert_eq!(st(&r).proposal, Some(id));
    assert!(r.view().unwrap().justified);
    assert_eq!(st(&r).step, Step::Prevote);

    // Advance the sender's VC to this same body's CURRENT round. The old
    // proposal header still names valid_round=0; its current-round polka is
    // nevertheless useful form-B evidence, not a malformed justification.
    let current_votes: Vec<_> = (0..3)
        .map(|i| vote(&c, i, i as u8, 0, 1, VoteKind::Prevote, Some(id)))
        .collect();
    let current = [Group {
        kind: CertKind::Polka { round: 1, id },
        votes: Votes::Dense(&current_votes),
    }];
    e.receive(ordinary(None, None, &current), Status::Valid, &mut s)
        .unwrap();
    assert_eq!(e.view().unwrap().valid_round, Some(1));
    s.packets.clear();

    let (mut b, mut bs) = engine(&c, 2, 0);
    b.receive(
        ordinary(Some(&rj[0]), None, &rg),
        Status::Indeterminate,
        &mut bs,
    )
    .unwrap();
    assert_eq!(st(&b).round, 1);
    assert_eq!(st(&b).proposal, None);
    assert_eq!(
        e.serve(5, &st(&b), &mut s).unwrap(),
        Served::Sent(Gap::Polka)
    );
    assert_eq!(s.packets.len(), 1);
    assert_eq!(s.packets[0].groups[0].0, CertKind::Polka { round: 1, id });
    s.packets[0].deliver(&mut b, &mut bs).unwrap();
    assert_eq!(st(&b).proposal, Some(id));
    assert!(
        !b.view().unwrap().justified,
        "form B does not fabricate an old-vr justification"
    );
    assert!(st(&b).prevote_quorum);
    assert!(b.view().unwrap().timers[1].active);
    let remaining = b.view().unwrap().timers[1].remaining;
    assert!(remaining > 0);
    b.elapse(remaining, &mut bs).unwrap();
    assert_eq!(b.view().unwrap().locked_round, Some(1));
    assert_eq!(st(&b).step, Step::Precommit);
    assert_eq!(b.precommits()[2].unwrap().content.id, Some(id));
}

#[test]
fn form_b_body_with_unrelated_vc_is_still_served_alone() {
    let (c, mut e, mut s, _) = round1_with_polka();
    let body = b"y";
    let h = header(&c, c.proposer(0, 1) as u8, 0, 1, Some(0), body);
    e.receive(
        ordinary(None, Some(Proposal { header: &h, body }), &[]),
        Status::Valid,
        &mut s,
    )
    .unwrap();
    s.packets.clear();
    let status = st(&e);
    assert_eq!(status.proposal, Some(h.content.id));
    let lag = StatusSnapshot {
        proposal: None,
        ..status
    };
    assert_eq!(
        e.serve(5, &lag, &mut s).unwrap(),
        Served::Sent(Gap::Proposal)
    );
    assert!(s.packets[0].groups.is_empty());
    assert_eq!(s.packets[0].proposal.as_ref().unwrap().0, h);
}

#[test]
fn certified_conflict_is_repaired_not_nogap() {
    // Peer caches a conflicting proposal; we hold the current-round polka.
    let c = config(&[0, 1, 2, 3], &[1; 4]);
    let (mut e, mut s, h) = with_proposal(&c, 3, b"x");
    let id = h.content.id;
    let votes = polka_votes(&c, id);
    let g = [Group {
        kind: CertKind::Polka { round: 0, id },
        votes: Votes::Dense(&votes),
    }];
    e.receive(ordinary(Some(&votes[0]), None, &g), Status::Valid, &mut s)
        .unwrap();
    let lag = StatusSnapshot {
        proposal: Some([9; 32]),
        polka: None,
        prevote_quorum: false,
        justified: false,
        ..st(&e)
    };
    assert!(matches!(
        e.serve(5, &lag, &mut s).unwrap(),
        Served::Sent(Gap::Polka)
    ));
    assert!(s.packets[0].proposal.is_some());
    assert!(
        s.packets[0]
            .groups
            .iter()
            .any(|g| g.0 == CertKind::Polka { round: 0, id })
    );
}

#[test]
fn same_step_peer_lacking_prevote_quorum_gets_full_row_and_arms_timer() {
    let c = config(&[0, 1, 2, 3], &[1; 4]);
    let (mut e, mut s, _) = with_proposal(&c, 3, b"x");
    let (mut r, mut rs, _) = with_proposal(&c, 2, b"x");
    let a = vote(&c, 0, 0, 0, 0, VoteKind::Prevote, None);
    let b = vote(&c, 1, 1, 0, 0, VoteKind::Prevote, None);
    e.receive(ordinary(Some(&a), None, &[]), Status::Valid, &mut s)
        .unwrap();
    e.receive(ordinary(Some(&b), None, &[]), Status::Valid, &mut s)
        .unwrap();
    s.packets.clear();
    assert!(st(&e).prevote_quorum);
    assert!(!st(&r).prevote_quorum);
    assert_eq!(st(&e).step, st(&r).step);
    assert!(!r.view().unwrap().timers[2].active);
    // A single missing vote alone never triggers a package.
    let near = StatusSnapshot {
        prevote_quorum: true,
        justified: false,
        ..st(&r)
    };
    assert_eq!(
        e.serve(6, &near, &mut s).unwrap(),
        Served::Skipped(Skip::NoGap)
    );
    assert_eq!(
        e.serve(6, &st(&r), &mut s).unwrap(),
        Served::Sent(Gap::Votes)
    );
    s.packets[0].deliver(&mut r, &mut rs).unwrap();
    assert!(st(&r).prevote_quorum);
    assert!(r.view().unwrap().timers[2].active);
}

#[test]
fn high_priority_churn_cannot_starve_lower_gap() {
    let (c, mut e, mut s, id) = round1_with_polka();
    let _ = id;
    let body = b"z";
    let h = header(&c, c.proposer(0, 1) as u8, 0, 1, None, body);
    e.receive(
        ordinary(None, Some(Proposal { header: &h, body }), &[]),
        Status::Valid,
        &mut s,
    )
    .unwrap();
    s.packets.clear();
    let mine = st(&e);
    let low = StatusSnapshot {
        step: Step::Propose,
        ..mine
    };
    let hi_round = StatusSnapshot { round: 0, ..mine };
    let hi_prop = StatusSnapshot {
        proposal: None,
        ..mine
    };
    assert_eq!(e.gap_for(&low), Some(Gap::Votes));
    assert_eq!(e.gap_for(&hi_round), Some(Gap::Round));
    assert_eq!(e.gap_for(&hi_prop), Some(Gap::Proposal));
    let mut d = Director::new(dcfg(4, 1));
    d.update(upd(1, 1, 1, low), 0).unwrap();
    d.update(upd(2, 1, 1, hi_round), 0).unwrap();
    let mut seq = 1;
    let mut served_low = false;
    for now in 0..12u64 {
        // The high peer's status flips every tick, releasing its grant.
        seq += 1;
        let flip = if seq % 2 == 0 { hi_prop } else { hi_round };
        d.update(upd(2, 1, seq, flip), now).unwrap();
        for (p, _) in d.poll(&e, now, &mut s).unwrap() {
            if p == 1 {
                served_low = true;
            }
        }
        if served_low {
            break;
        }
    }
    assert!(served_low, "low-priority eligible peer starved");
}

#[test]
fn poll_on_healthy_engine_with_no_peers_is_empty() {
    let c = config(&[0, 1, 2, 3], &[1; 4]);
    let (e, mut s, _) = with_proposal(&c, 3, b"x");
    let mut d = Director::new(dcfg(2, 1));
    assert!(d.poll(&e, 0, &mut s).unwrap().is_empty());
}

fn assert_bare(packets: &[Packet]) {
    for p in packets {
        assert_eq!(p.target, Target::All, "no targeted blind reply");
        assert_eq!(p.message, Message::Ordinary, "no DEC");
        assert!(p.outer.is_some(), "bare signed vote");
        assert!(p.proposal.is_none(), "no proposal/body");
        assert!(p.groups.is_empty(), "no groups");
    }
}

#[test]
fn guided_automatic_emissions_are_only_bare_votes_and_serve_is_explicit() {
    let c = config(&[0, 1, 2, 3], &[1; 4]);
    let p = c.proposer(0, 0);
    let mut e = Engine::boot(MemoryStore::default(), key(p as u8), c.clone()).unwrap();
    e.set_emission(Emission::Guided);
    let mut s = Recorder::default();
    assert!(e.init(&c, 0, 0, &mut s).unwrap());
    assert_eq!(s.acquired.len(), 1);
    let tag = s.acquired[0];
    e.supply_proposal(tag, b"guided", Status::Valid, &mut s)
        .unwrap();
    let id = body_id(b"guided");
    assert_eq!(st(&e).proposal, Some(id), "proposal retained");
    assert!(!s.packets.is_empty());
    // Reactive reply to another member's ENTER stays silent.
    let others: Vec<usize> = (0..4).filter(|i| *i != p).collect();
    let enter = vote(&c, others[0], others[0] as u8, 0, 0, VoteKind::Enter, None);
    e.receive(ordinary(Some(&enter), None, &[]), Status::Valid, &mut s)
        .unwrap();
    // Two more prevotes: polka, own precommit.
    for &i in &others[..2] {
        let v = vote(&c, i, i as u8, 0, 0, VoteKind::Prevote, Some(id));
        e.receive(ordinary(Some(&v), None, &[]), Status::Valid, &mut s)
            .unwrap();
    }
    assert_eq!(st(&e).step, Step::Precommit);
    // Several periodic rho replays, independent of the current step.
    e.elapse(3 * 4, &mut s).unwrap();
    assert!(s.packets.len() >= 6);
    assert_bare(&s.packets);
    // Recovery selects Guided before any output.
    let mut rs = Recorder::default();
    let store = e.into_storage();
    let mut e =
        Engine::recover_with_emission(store, key(p as u8), Emission::Guided, &mut rs).unwrap();
    assert!(!rs.packets.is_empty());
    assert_bare(&rs.packets);
    // Decide, then no DEC replay.
    for &i in &others[..2] {
        let v = vote(&c, i, i as u8, 0, 0, VoteKind::Precommit, Some(id));
        e.receive(ordinary(Some(&v), None, &[]), Status::Valid, &mut rs)
            .unwrap();
    }
    assert_eq!(rs.decisions, [(0, id)]);
    rs.packets.clear();
    e.elapse(9, &mut rs).unwrap();
    assert!(rs.packets.is_empty(), "no DEC in guided");
    // Explicit serve is the only source of the full package.
    let mut sv = Recorder::default();
    let (fresh, _) = engine(&c, others[0] as u8, 0);
    let lag = StatusSnapshot {
        proposal: None,
        ..st(&fresh)
    };
    assert_eq!(
        e.serve(4, &lag, &mut sv).unwrap(),
        Served::Sent(Gap::Decision)
    );
    assert_eq!(sv.packets[0].target, Target::Peer(4));
    assert_eq!(sv.packets[0].message, Message::Dec);
    assert!(sv.packets[0].proposal.is_some());
}

#[test]
fn reference_recovery_still_replays_full_frames() {
    let c = config(&[0, 1, 2, 3], &[1; 4]);
    let p = c.proposer(0, 0);
    let (mut e, mut s) = engine(&c, p as u8, 0);
    e.supply_proposal(s.acquired[0], b"ref", Status::Valid, &mut s)
        .unwrap();
    let mut rs = Recorder::default();
    let _e = Engine::recover(e.into_storage(), key(p as u8), &mut rs).unwrap();
    assert!(rs.packets.iter().any(|p| p.proposal.is_some()));
}

#[test]
fn held_unjustified_form_b_body_gets_exact_vr_witness_and_progresses() {
    let (c, mut e, mut s, id) = round1_with_polka();
    let body = b"x";
    let h = header(&c, c.proposer(0, 1) as u8, 0, 1, Some(0), body);
    let (mut r, mut rs) = engine(&c, 2, 0);
    let rj = [
        vote(&c, 0, 0, 0, 1, VoteKind::Prevote, None),
        vote(&c, 1, 1, 0, 1, VoteKind::Prevote, None),
    ];
    let rg = [Group {
        kind: CertKind::Row {
            round: 1,
            kind: VoteKind::Prevote,
        },
        votes: Votes::Dense(&rj),
    }];
    r.receive(
        ordinary(Some(&rj[0]), None, &rg),
        Status::Indeterminate,
        &mut rs,
    )
    .unwrap();
    // Held body, no justification.
    r.receive(
        ordinary(None, Some(Proposal { header: &h, body }), &[]),
        Status::Valid,
        &mut rs,
    )
    .unwrap();
    assert_eq!(st(&r).proposal, Some(id));
    assert!(!st(&r).justified);
    assert_eq!(st(&r).step, Step::Propose);
    // Sender holds the proposal and the exact vr=0 polka.
    let g0v = polka_votes(&c, id);
    let g0 = [Group {
        kind: CertKind::Polka { round: 0, id },
        votes: Votes::Dense(&g0v),
    }];
    e.receive(
        ordinary(None, Some(Proposal { header: &h, body }), &g0),
        Status::Valid,
        &mut s,
    )
    .unwrap();
    s.packets.clear();
    // State-changing quorum row first (peer lacks a prevote quorum)...
    assert_eq!(
        e.serve(5, &st(&r), &mut s).unwrap(),
        Served::Sent(Gap::Votes)
    );
    s.packets[0].deliver(&mut r, &mut rs).unwrap();
    assert!(st(&r).prevote_quorum && !st(&r).justified);
    s.packets.clear();
    // ...then the exact valid-round certificate for the held body.
    assert_eq!(
        e.serve(5, &st(&r), &mut s).unwrap(),
        Served::Sent(Gap::Proposal)
    );
    assert_eq!(s.packets[0].groups[0].0, CertKind::Polka { round: 0, id });
    s.packets[0].deliver(&mut r, &mut rs).unwrap();
    assert!(st(&r).justified);
    assert_eq!(st(&r).step, Step::Prevote);
    // A justified peer is no longer served.
    assert_eq!(
        e.serve(5, &st(&r), &mut s).unwrap(),
        Served::Skipped(Skip::NoGap)
    );
}

#[test]
fn lagging_round_receiver_jumps_directly_on_self_carrying_prevote_row() {
    let (c, mut e, mut s, _) = round1_with_polka();
    e.elapse(35, &mut s).unwrap();
    assert!(st(&e).prevote_quorum);
    assert_eq!(e.prevotes()[3].unwrap().content.round, 1);
    s.packets.clear();
    let (mut r, mut rs) = engine(&c, 2, 0);
    assert_eq!(st(&r).round, 0);
    assert_eq!(
        e.serve(1, &st(&r), &mut s).unwrap(),
        Served::Sent(Gap::Round)
    );
    assert_eq!(s.packets[0].outer.unwrap().content.kind, VoteKind::Prevote);
    s.packets[0].deliver(&mut r, &mut rs).unwrap();
    assert_eq!(st(&r).round, 1, "no predecessor-round TPC wait");
    assert!(st(&r).prevote_quorum, "quorum row merged");
}

#[test]
fn director_checked_constructor_rejects_degenerate_config() {
    assert!(Director::try_new(dcfg(2, 0)).is_none());
    assert!(Director::try_new(dcfg(2, 1)).is_some());
}

#[test]
fn recovered_late_polka_bridges_lower_round_despite_nil_own_precommit() {
    let c = config(&[0, 1, 2, 3], &[1; 4]);
    let (mut e, mut s) = engine(&c, 3, 0);
    e.set_emission(Emission::Guided);
    let entrants = [
        vote(&c, 0, 0, 0, 1, VoteKind::Enter, None),
        vote(&c, 1, 1, 0, 1, VoteKind::Enter, None),
    ];
    let entry = [Group {
        kind: CertKind::Jump { round: 1 },
        votes: Votes::Dense(&entrants),
    }];
    e.receive(
        ordinary(Some(&entrants[0]), None, &entry),
        Status::Valid,
        &mut s,
    )
    .unwrap();
    assert_eq!(st(&e).round, 1);
    let body = b"late certificate";
    let h = header(&c, c.proposer(0, 1) as u8, 0, 1, None, body);
    let id = h.content.id;
    e.receive(
        ordinary(None, Some(Proposal { header: &h, body }), &[]),
        Status::Valid,
        &mut s,
    )
    .unwrap();
    // One Byzantine signer first supplies nil, then equivocates to the value.
    // The correct sender's own prevote stays for the value throughout.
    let nil = vote(&c, 0, 0, 0, 1, VoteKind::Prevote, None);
    let yes = vote(&c, 1, 1, 0, 1, VoteKind::Prevote, Some(id));
    for v in [&nil, &yes] {
        e.receive(ordinary(Some(v), None, &[]), Status::Valid, &mut s)
            .unwrap();
    }
    assert!(e.view().unwrap().timers[2].active);
    e.elapse(e.view().unwrap().timers[2].remaining, &mut s)
        .unwrap();
    assert_eq!(e.precommits()[3].unwrap().content.id, None);
    let votes: Vec<_> = [0usize, 1, 3]
        .into_iter()
        .map(|i| vote(&c, i, i as u8, 0, 1, VoteKind::Prevote, Some(id)))
        .collect();
    let polka = [Group {
        kind: CertKind::Polka { round: 1, id },
        votes: Votes::Dense(&votes),
    }];
    e.receive(ordinary(None, None, &polka), Status::Valid, &mut s)
        .unwrap();
    assert_eq!(st(&e).polka, Some((1, id)));
    let e =
        Engine::recover_with_emission(e.into_storage(), key(3), Emission::Guided, &mut s).unwrap();
    assert!(!st(&e).prevote_quorum, "volatile quorum row was cleared");
    assert_eq!(e.precommits()[3].unwrap().content.id, None);
    s.packets.clear();
    let (mut r, mut rs) = engine(&c, 2, 0);
    assert_eq!(
        e.serve(2, &st(&r), &mut s).unwrap(),
        Served::Sent(Gap::Round)
    );
    assert_eq!(s.packets.len(), 1);
    assert_eq!(s.packets[0].outer, None);
    assert_eq!(s.packets[0].groups[0].0, CertKind::Polka { round: 1, id });
    s.packets[0].deliver(&mut r, &mut rs).unwrap();
    assert_eq!(st(&r).round, 1);
    assert_eq!(r.view().unwrap().locked_round, Some(1));
    assert_eq!(st(&r).step, Step::Precommit);
}
