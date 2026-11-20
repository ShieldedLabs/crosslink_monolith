//! Public-API regressions. Owned test packets are not consensus workspace.
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
#[test]
fn strict_thresholds_and_configuration_bound_signatures() {
    let a = config(&[0, 1, 2, 3], &[6, 2, 1, 1]);
    assert!(!a.quorum(6));
    assert!(a.quorum(7));
    assert!(!a.witness(3));
    assert!(a.witness(4));
    let v = vote(&a, 1, 1, 9, 7, VoteKind::Prevote, None);
    assert!(v.verify(&a));
    assert!(!v.verify(&config(&[0, 1, 2, 3], &[5, 3, 1, 1])));
    let mut changed = v;
    changed.content.id = Some([0; 32]);
    assert!(!changed.verify(&a));
    changed = v;
    changed.content.height += 1;
    assert!(!changed.verify(&a));
}
#[test]
fn future_fragments_do_not_accumulate_but_one_package_jumps() {
    let c = config(&[0, 1, 2, 3], &[1; 4]);
    let (mut e, mut s) = engine(&c, 0, 0);
    let a = vote(&c, 1, 1, 0, 9, VoteKind::Prevote, None);
    let b = vote(&c, 2, 2, 0, 9, VoteKind::Prevote, None);
    for v in [&a, &b] {
        e.receive(ordinary(Some(v), None, &[]), Status::Indeterminate, &mut s)
            .unwrap();
    }
    assert_eq!(e.view().unwrap().round, 0);
    let votes = [a, b];
    let groups = [Group {
        kind: CertKind::Row {
            round: 9,
            kind: VoteKind::Prevote,
        },
        votes: Votes::Dense(&votes),
    }];
    e.receive(
        ordinary(Some(&a), None, &groups),
        Status::Indeterminate,
        &mut s,
    )
    .unwrap();
    assert_eq!(e.view().unwrap().round, 9);
    let entry = e.view().unwrap().entry.unwrap();
    assert_eq!(
        e.storage().region_header(entry.index).unwrap().kind,
        CertKind::Jump { round: 9 }
    );
}
#[test]
fn mixed_round_jump_chooses_the_greatest_weighted_cut() {
    let c = config(&[0, 1, 2, 3], &[6, 2, 1, 1]);
    let (mut e, mut s) = engine(&c, 0, 0);
    let votes = [
        vote(&c, 1, 1, 0, 3, VoteKind::Prevote, None),
        vote(&c, 2, 2, 0, 7, VoteKind::Prevote, None),
        vote(&c, 3, 3, 0, 5, VoteKind::Prevote, None),
    ];
    let groups = [Group {
        kind: CertKind::Jump { round: 3 },
        votes: Votes::Dense(&votes),
    }];
    e.receive(
        ordinary(Some(&votes[0]), None, &groups),
        Status::Indeterminate,
        &mut s,
    )
    .unwrap();
    assert_eq!(e.view().unwrap().round, 3);
    let entry = e.view().unwrap().entry.unwrap();
    assert_eq!(e.storage().region_header(entry.index).unwrap().count, 3);
}
#[test]
fn malformed_group_does_not_discard_valid_outer_vote() {
    let c = config(&[0, 1, 2, 3], &[1; 4]);
    let (mut e, mut s) = engine(&c, 0, 0);
    let a = vote(&c, 1, 1, 0, 0, VoteKind::Prevote, None);
    let duplicate = [a, a];
    let groups = [Group {
        kind: CertKind::Row {
            round: 0,
            kind: VoteKind::Prevote,
        },
        votes: Votes::Dense(&duplicate),
    }];
    e.receive(
        ordinary(Some(&a), None, &groups),
        Status::Indeterminate,
        &mut s,
    )
    .unwrap();
    assert_eq!(e.prevotes()[1], Some(a));
    assert_eq!(e.prevotes().iter().flatten().count(), 1);
}
#[test]
fn complete_quorum_repairs_conflicting_cached_rows() {
    let c = config(&[0, 1, 2, 3], &[1; 4]);
    let (mut e, mut s) = engine(&c, 99, 0);
    let polluted = vote(&c, 1, 1, 0, 0, VoteKind::Prevote, Some([7; 32]));
    e.receive(
        ordinary(Some(&polluted), None, &[]),
        Status::Indeterminate,
        &mut s,
    )
    .unwrap();
    let id = body_id(b"repaired");
    let votes: Vec<_> = (1..4)
        .map(|i| vote(&c, i, i as u8, 0, 0, VoteKind::Prevote, Some(id)))
        .collect();
    let groups = [Group {
        kind: CertKind::Polka { round: 0, id },
        votes: Votes::Dense(&votes),
    }];
    e.receive(
        ordinary(Some(&votes[0]), None, &groups),
        Status::Indeterminate,
        &mut s,
    )
    .unwrap();
    for i in 1..4 {
        assert_eq!(e.prevotes()[i].unwrap().content.id, Some(id));
    }
}
#[test]
fn valid_round_proposal_waits_for_exact_form_a() {
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
    let body = b"authorized";
    let h = header(&c, 1, 0, 1, Some(0), body);
    let p = Proposal { header: &h, body };
    e.receive(ordinary(None, Some(p), &[]), Status::Valid, &mut s)
        .unwrap();
    assert_eq!(e.view().unwrap().step, Step::Propose);
    assert!(e.prevotes()[2].is_none());
    let polka: Vec<_> = [0, 1, 3]
        .into_iter()
        .map(|i| vote(&c, i, i as u8, 0, 0, VoteKind::Prevote, Some(h.content.id)))
        .collect();
    let groups = [Group {
        kind: CertKind::Polka {
            round: 0,
            id: h.content.id,
        },
        votes: Votes::Dense(&polka),
    }];
    e.receive(ordinary(None, Some(p), &groups), Status::Valid, &mut s)
        .unwrap();
    assert!(e.view().unwrap().justified);
    assert_eq!(e.prevotes()[2].unwrap().content.id, Some(h.content.id));
}
#[test]
fn fresh_valid_decision_resolves_exact_indeterminate_slot_before_start() {
    let c = config(&[0, 1, 2, 3], &[1; 4]);
    let (mut e, mut s) = engine(&c, 3, 1000);
    let body = b"pending dependencies";
    let h = header(&c, 0, 0, 0, None, body);
    let p = Proposal { header: &h, body };
    e.receive(ordinary(None, Some(p), &[]), Status::Indeterminate, &mut s)
        .unwrap();
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
    assert_eq!(e.view().unwrap().step, Step::Decided);
    assert_eq!(s.decisions, [(0, h.content.id)]);
    assert!(s.packets.is_empty());
}
#[test]
fn roster_removal_disables_signing_but_preserves_decision_adoption() {
    let old = config(&[0, 1, 2, 3], &[1; 4]);
    let new = config(&[1, 2, 3], &[3, 2, 1]);
    let (mut e, mut s) = engine(&old, 0, 0);
    s.packets.clear();
    assert!(e.init(&new, 1, 0, &mut s).unwrap());
    assert_eq!(e.local_index(), None);
    let body = b"new committee";
    let h = header(&new, 2, 1, 0, None, body);
    let pcs: Vec<_> = (0..3)
        .map(|i| {
            vote(
                &new,
                i,
                (i + 1) as u8,
                1,
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
    e.receive(
        Frame {
            message: Message::Dec,
            outer: None,
            proposal: Some(Proposal { header: &h, body }),
            groups: &groups,
        },
        Status::Valid,
        &mut s,
    )
    .unwrap();
    assert_eq!(s.decisions, [(1, h.content.id)]);
    assert!(s.packets.iter().all(|p| p.outer.is_none()));
    let before = e.storage().writes;
    assert!(!e.init(&old, 0, 0, &mut s).unwrap());
    assert!(!e.init(&old, 1, 0, &mut s).unwrap());
    assert_eq!(e.storage().writes, before);
}
#[test]
fn roster_publication_crash_prefixes_recover_old_or_new_configuration() {
    let old = config(&[0, 1, 2, 3], &[1; 4]);
    let new = config(&[1, 2, 3], &[3, 2, 1]);
    let (e, _) = engine(&old, 3, 1000);
    let original = e.into_storage();
    let mut sink = Recorder::default();
    let mut successful = Engine::recover(original.clone(), key(3), &mut sink).unwrap();
    successful.init(&new, 1, 1000, &mut sink).unwrap();
    let count = successful.storage().writes - original.writes;
    assert!(count >= 5);
    for cut in 0..=count {
        let mut store = original.clone();
        store.fail_at = Some(store.writes + cut);
        let mut sink = Recorder::default();
        let mut interrupted = Engine::recover(store, key(3), &mut sink).unwrap();
        let result = interrupted.init(&new, 1, 1000, &mut sink);
        assert!(result.is_ok() || result == Err(Error::Storage));
        let mut store = interrupted.into_storage();
        store.clear_fault();
        let recovered = Engine::recover(store, key(3), &mut sink).unwrap();
        let d = recovered.view().unwrap();
        assert!(d.height <= 1);
        assert_eq!(
            d.config,
            if d.height == 0 {
                old.hash()
            } else {
                new.hash()
            }
        );
        assert_eq!(recovered.storage().config().unwrap().hash(), d.config);
        assert_eq!(d.step, Step::PreRound);
        assert!(sink.packets.is_empty());
    }
}
#[test]
fn weighted_nodes_decide_with_loss_duplicates_and_reordering() {
    lossy_simulation(config(&[0, 1, 2, 3], &[4, 3, 2, 1]), false);
}
#[test]
fn absent_initial_proposer_is_skipped_and_later_adopts_the_retained_decision() {
    lossy_simulation(config(&[0, 1, 2, 3], &[1, 4, 3, 2]), true);
}
fn lossy_simulation(c: Config, delayed_proposer: bool) {
    let mut nodes: Vec<_> = (0..4).map(|i| engine(&c, i, 0)).collect();
    let mut serial = 0usize;
    let mut attempts = [[[0usize; 5]; 4]; 4];
    for tick in 0..300 {
        for (i, (e, s)) in nodes.iter_mut().enumerate() {
            if delayed_proposer && i == 0 && tick < 120 {
                continue;
            }
            if let Some(tag) = e.proposal_tag() {
                e.supply_proposal(tag, b"deterministic proposal", Status::Valid, s)
                    .unwrap();
            }
            e.elapse(1, s).unwrap();
        }
        let mut deliveries = Vec::new();
        for (from, (_, sink)) in nodes.iter_mut().enumerate() {
            for packet in std::mem::take(&mut sink.packets) {
                for to in 0..4 {
                    if from == to
                        || delayed_proposer && tick < 120 && (from == 0 || to == 0)
                        || packet.target != Target::All && packet.target != Target::Peer(to as u32)
                    {
                        continue;
                    }
                    serial += 1;
                    let class = if packet.message == Message::Dec {
                        4
                    } else {
                        match packet.outer.map(|v| v.content.kind) {
                            Some(VoteKind::Enter) => 0,
                            Some(VoteKind::Prevote) => 2,
                            Some(VoteKind::Precommit) => 3,
                            None => 1,
                        }
                    };
                    attempts[from][to][class] += 1;
                    if tick < 10 || attempts[from][to][class] % 3 == 1 {
                        continue;
                    }
                    deliveries.push((to, packet.clone()));
                    if serial % 7 == 0 {
                        deliveries.push((to, packet.clone()));
                    }
                }
            }
        }
        deliveries.reverse();
        for (to, packet) in deliveries {
            let (e, s) = &mut nodes[to];
            packet.deliver(e, s).unwrap();
        }
        if delayed_proposer && tick == 119 {
            assert!(
                nodes[1..].iter().all(|(_, s)| !s.decisions.is_empty()),
                "available quorum failed to advance without its initial proposer"
            );
            assert!(nodes[0].1.decisions.is_empty());
            assert!(nodes[1..].iter().all(|(e, _)| e.view().unwrap().round > 0));
        }
        if nodes.iter().all(|(_, s)| !s.decisions.is_empty()) {
            break;
        }
    }
    let id = body_id(b"deterministic proposal");
    for (e, s) in &nodes {
        assert_eq!(e.view().unwrap().step, Step::Decided);
        assert_eq!(s.decisions, [(0, id)]);
    }
}

#[test]
fn old_round_fresh_valid_copy_resolves_pending_without_another_dec_packet() {
    let c = config(&[0, 1, 2, 3], &[1; 4]);
    let (mut e, mut s) = engine(&c, 3, 1000);
    let body = b"old-round decision";
    let h = header(&c, 0, 0, 4, None, body);
    let p = Proposal { header: &h, body };
    let pcs: Vec<_> = (0..3)
        .map(|i| {
            vote(
                &c,
                i,
                i as u8,
                0,
                4,
                VoteKind::Precommit,
                Some(h.content.id),
            )
        })
        .collect();
    let groups = [Group {
        kind: CertKind::Decision {
            round: 4,
            id: h.content.id,
        },
        votes: Votes::Dense(&pcs),
    }];
    e.receive(
        Frame {
            message: Message::Dec,
            outer: None,
            proposal: Some(p),
            groups: &groups,
        },
        Status::Indeterminate,
        &mut s,
    )
    .unwrap();
    assert!(s.decisions.is_empty());
    e.receive(ordinary(None, Some(p), &[]), Status::Valid, &mut s)
        .unwrap();
    assert_eq!(s.decisions, [(0, h.content.id)]);
    assert_eq!(
        e.storage()
            .header(e.view().unwrap().pending.unwrap().proposal.index),
        Some(&h)
    );
}

#[test]
fn review_regression_new_height_honors_smaller_retransmission_period() {
    let template = config(&[0, 1, 2, 3], &[1; 4]);
    let old = Config::new(
        template.domain(),
        template.members().to_vec(),
        template.max_body(),
        1000,
        template.timeouts(),
    )
    .unwrap();
    let new = Config::new(
        template.domain(),
        template.members().to_vec(),
        template.max_body(),
        1,
        template.timeouts(),
    )
    .unwrap();
    let (mut e, mut s) = engine(&old, 99, 1000);
    e.elapse(1, &mut s).unwrap();
    assert!(e.init(&new, 1, 0, &mut s).unwrap());
    let body = b"new-height decision";
    let h = header(&new, 1, 1, 0, None, body);
    let pcs: Vec<_> = (0..3)
        .map(|i| {
            vote(
                &new,
                i,
                i as u8,
                1,
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
    e.receive(
        Frame {
            message: Message::Dec,
            outer: None,
            proposal: Some(Proposal { header: &h, body }),
            groups: &groups,
        },
        Status::Valid,
        &mut s,
    )
    .unwrap();
    assert_eq!(s.decisions, [(1, h.content.id)]);
    s.packets.clear();
    e.elapse(1, &mut s).unwrap();
    assert!(s.packets.iter().any(|p| p.message == Message::Dec));
}

#[test]
fn review_regression_no_proposal_reply_after_decision() {
    let c = config(&[0, 1, 2, 3], &[1; 4]);
    let (mut e, mut s) = engine(&c, 0, 0);
    let body = b"proposer body";
    let tag = e.proposal_tag().unwrap();
    e.supply_proposal(tag, body, Status::Valid, &mut s).unwrap();
    let h = s
        .packets
        .iter()
        .find_map(|p| p.proposal.as_ref().map(|(h, _)| *h))
        .unwrap();
    s.packets.clear();
    let pcs: Vec<_> = (1..4)
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
        kind: CertKind::Row {
            round: 0,
            kind: VoteKind::Precommit,
        },
        votes: Votes::Dense(&pcs),
    }];
    let enter = vote(&c, 1, 1, 0, 0, VoteKind::Enter, None);
    e.receive(
        ordinary(Some(&enter), Some(Proposal { header: &h, body }), &groups),
        Status::Valid,
        &mut s,
    )
    .unwrap();
    assert_eq!(e.view().unwrap().step, Step::Decided);
    assert_eq!(s.decisions, [(0, h.content.id)]);
    assert!(
        s.packets.is_empty(),
        "ordinary proposal reply escaped after DECIDED"
    );
}
