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
