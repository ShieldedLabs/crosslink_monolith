use super::*;
fn keys() -> [SigningKey; 4] {
    std::array::from_fn(|i| SigningKey::from([i as u8 + 1; 32]))
}
fn config(weights: [u64; 4]) -> Config {
    let k = keys();
    Config::new(
        [42; 32],
        k.iter()
            .enumerate()
            .map(|(i, k)| Member {
                public_key: VerificationKeyBytes::from(k).into(),
                weight: weights[i],
            })
            .collect(),
        1024,
        10,
        Timeouts {
            propose: 30,
            prevote: 5,
            precommit: 10,
            inc_propose: 3,
            inc_prevote: 1,
            inc_precommit: 2,
        },
    )
    .unwrap()
}
#[derive(Default)]
struct Out {
    sent: Vec<(Target, Option<Vote>, Option<Header>, Message)>,
    decisions: Vec<Header>,
    acquire: usize,
    retries: usize,
}
impl Sink for Out {
    fn send(&mut self, t: Target, f: Frame<'_>) {
        self.sent.push((
            t,
            f.outer.copied(),
            f.proposal.map(|p| *p.header),
            f.message,
        ));
    }
    fn decided(&mut self, _h: u64, p: Proposal<'_>, _c: Group<'_>) {
        self.decisions.push(*p.header);
    }
    fn acquire(&mut self, _: ProposalTag) {
        self.acquire += 1;
    }
    fn retry(&mut self, _: BodyTag, _: Proposal<'_>) {
        self.retries += 1;
    }
}
fn engine(c: &Config, me: usize, h: u64, tau: u64) -> (Engine<MemoryStore>, Out) {
    let mut o = Out::default();
    let mut e = Engine::boot(MemoryStore::default(), keys()[me].clone(), c.clone()).unwrap();
    e.init(c, h, tau, &mut o).unwrap();
    (e, o)
}
fn vote(c: &Config, s: usize, r: u64, k: VoteKind, id: Option<Id>) -> Vote {
    Vote::sign(
        c,
        &keys()[s],
        VoteContent {
            height: 4,
            round: r,
            kind: k,
            id,
            signer: s as u32,
        },
    )
}
fn header(c: &Config, r: u64, vr: Option<u64>, body: &[u8]) -> Header {
    let s = c.proposer(4, r);
    Header::sign(
        c,
        &keys()[s],
        ProposalContent {
            height: 4,
            round: r,
            valid_round: vr,
            id: body_id(body),
            signer: s as u32,
        },
    )
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
fn strict_weight_boundaries() {
    let c = config([1, 1, 1, 3]);
    assert!(!c.quorum(4));
    assert!(c.quorum(5));
    assert!(!c.witness(2));
    assert!(c.witness(3));
}
#[test]
fn signed_header_binds_every_field() {
    let c = config([1; 4]);
    let h = header(&c, 3, Some(1), b"body");
    assert!(h.verify(&c));
    for changed in 0..5 {
        let mut x = h;
        match changed {
            0 => x.content.id[31] ^= 1,
            1 => x.content.height += 1,
            2 => x.content.round += 4,
            3 => x.content.valid_round = Some(0),
            _ => x.content.signer = 0,
        };
        assert!(!x.verify(&c));
    }
    let other = Config::new([43; 32], c.members().to_vec(), 1024, 10, c.timeouts()).unwrap();
    assert!(!h.verify(&other));
    assert!(!header(&c, 2, Some(2), b"body").verify(&c));
}
#[test]
fn packaged_jump_weighted_max_and_self_exclusion() {
    let c = config([6, 2, 1, 1]);
    let (mut e, mut o) = engine(&c, 0, 4, 0);
    let pv = [vote(&c, 1, 3, VoteKind::Prevote, None)];
    let pc = [vote(&c, 2, 7, VoteKind::Precommit, None)];
    let outer = vote(&c, 3, 5, VoteKind::Enter, None);
    let g = [
        Group {
            kind: CertKind::Row {
                round: 3,
                kind: VoteKind::Prevote,
            },
            votes: Votes::Dense(&pv),
        },
        Group {
            kind: CertKind::Row {
                round: 7,
                kind: VoteKind::Precommit,
            },
            votes: Votes::Dense(&pc),
        },
    ];
    e.receive(
        ordinary(Some(&outer), None, &g),
        Status::Indeterminate,
        &mut o,
    )
    .unwrap();
    assert_eq!(e.d().round, 3);
    assert_eq!(e.group(e.d().entry.unwrap()).votes.len(), 3);
    let own = vote(&c, 0, 99, VoteKind::Enter, None);
    e.receive(
        ordinary(Some(&own), None, &[]),
        Status::Indeterminate,
        &mut o,
    )
    .unwrap();
    assert_eq!(e.d().round, 3);
}
#[test]
fn separate_frames_do_not_accumulate_future() {
    let c = config([1; 4]);
    let (mut e, mut o) = engine(&c, 0, 4, 0);
    for s in 1..4 {
        let v = vote(&c, s, 20, VoteKind::Enter, None);
        e.receive(ordinary(Some(&v), None, &[]), Status::Indeterminate, &mut o)
            .unwrap();
        assert_eq!(e.d().round, 0);
    }
    assert!(e.d().entry.is_none());
}
#[test]
fn heavy_future_and_same_frame_rows() {
    let c = config([1, 1, 1, 7]);
    let (mut e, mut o) = engine(&c, 0, 4, 0);
    let v = vote(&c, 3, 17, VoteKind::Prevote, None);
    let g = [Group {
        kind: CertKind::Row {
            round: 17,
            kind: VoteKind::Prevote,
        },
        votes: Votes::Dense(std::slice::from_ref(&v)),
    }];
    e.receive(ordinary(Some(&v), None, &g), Status::Indeterminate, &mut o)
        .unwrap();
    assert_eq!(e.d().round, 17);
    assert_eq!(e.pv[3], Some(v));
}
#[test]
fn lone_heavy_self_insertion_decides() {
    let c = config([7, 1, 1, 1]);
    let (mut e, mut o) = engine(&c, 0, 4, 0);
    assert_eq!(o.acquire, 1);
    e.supply_proposal(e.proposal_tag().unwrap(), b"chosen", Status::Valid, &mut o)
        .unwrap();
    assert_eq!(e.d().step, Step::Decided);
    assert_eq!(o.decisions.len(), 1);
    let before = o.sent.len();
    e.elapse(10, &mut o).unwrap();
    assert_eq!(o.sent.len(), before + 1);
    assert_eq!(o.sent.last().unwrap().3, Message::Dec);
}
#[test]
fn any_round_dec_pending_exact_header_and_local_event() {
    let c = config([1; 4]);
    let (mut e, mut o) = engine(&c, 1, 4, 100);
    let h = header(&c, 12, Some(3), b"pending");
    let vs = (0..3)
        .map(|s| vote(&c, s, 12, VoteKind::Precommit, Some(h.content.id)))
        .collect::<Vec<_>>();
    let groups = [Group {
        kind: CertKind::Decision {
            round: 12,
            id: h.content.id,
        },
        votes: Votes::Dense(&vs),
    }];
    e.receive(
        Frame {
            message: Message::Dec,
            outer: None,
            proposal: Some(Proposal {
                header: &h,
                body: b"pending",
            }),
            groups: &groups,
        },
        Status::Indeterminate,
        &mut o,
    )
    .unwrap();
    assert_eq!(e.d().step, Step::PreRound);
    assert!(o.sent.is_empty());
    let p = e.d().pending.unwrap();
    assert_eq!(*e.proposal(p.proposal).header, h);
    e.validity(e.body_tag(p.proposal), Status::Valid, &mut o)
        .unwrap();
    assert_eq!(o.decisions, vec![h]);
}
#[test]
fn preround_raw_zero_decides_without_signing() {
    let c = config([1; 4]);
    let (mut e, mut o) = engine(&c, 3, 4, 100);
    let h = header(&c, 0, None, b"zero");
    let vs = (0..3)
        .map(|s| vote(&c, s, 0, VoteKind::Precommit, Some(h.content.id)))
        .collect::<Vec<_>>();
    let groups = [Group {
        kind: CertKind::Row {
            round: 0,
            kind: VoteKind::Precommit,
        },
        votes: Votes::Dense(&vs),
    }];
    e.receive(
        ordinary(
            None,
            Some(Proposal {
                header: &h,
                body: b"zero",
            }),
            &groups,
        ),
        Status::Valid,
        &mut o,
    )
    .unwrap();
    assert_eq!(e.d().step, Step::Decided);
    assert!(o.sent.is_empty());
    assert!(e.d().intents.iter().all(Option::is_none));
}
#[test]
fn malformed_group_does_not_poison_outer() {
    let c = config([1; 4]);
    let (mut e, mut o) = engine(&c, 0, 4, 0);
    let v = vote(&c, 1, 0, VoteKind::Prevote, None);
    let bad = [v, v];
    let g = [Group {
        kind: CertKind::Row {
            round: 0,
            kind: VoteKind::Prevote,
        },
        votes: Votes::Dense(&bad),
    }];
    e.receive(ordinary(Some(&v), None, &g), Status::Indeterminate, &mut o)
        .unwrap();
    assert_eq!(e.pv[1], Some(v));
}
#[test]
fn propose_quorum_fallback_and_ordinary_resend() {
    let c = config([1; 4]);
    let (mut e, mut o) = engine(&c, 3, 4, 0);
    let vs = (0..3)
        .map(|s| vote(&c, s, 0, VoteKind::Prevote, None))
        .collect::<Vec<_>>();
    let g = [Group {
        kind: CertKind::Row {
            round: 0,
            kind: VoteKind::Prevote,
        },
        votes: Votes::Dense(&vs),
    }];
    e.receive(ordinary(None, None, &g), Status::Indeterminate, &mut o)
        .unwrap();
    assert!(e.d().timers[1].active);
    e.elapse(5, &mut o).unwrap();
    assert_eq!(e.d().step, Step::Precommit);
    assert!(e.own_current(VoteKind::Prevote).is_some());
}
#[test]
fn sticky_expired_pc_requires_actual_evidence_after_recovery() {
    let c = config([1; 4]);
    let (mut e, mut o) = engine(&c, 0, 4, 0);
    let vs = (1..4)
        .map(|s| vote(&c, s, 0, VoteKind::Precommit, None))
        .collect::<Vec<_>>();
    let g = [Group {
        kind: CertKind::Row {
            round: 0,
            kind: VoteKind::Precommit,
        },
        votes: Votes::Dense(&vs),
    }];
    e.receive(ordinary(None, None, &g), Status::Indeterminate, &mut o)
        .unwrap();
    let mut st = e.into_storage();
    st.descriptor.as_mut().unwrap().timers[3].remaining = 0;
    let mut e = Engine::recover(st, keys()[0].clone(), &mut o).unwrap();
    assert_eq!(e.d().round, 0);
    assert_eq!(e.d().timers[3].remaining, 0);
    e.receive(ordinary(None, None, &g), Status::Indeterminate, &mut o)
        .unwrap();
    assert_eq!(e.d().round, 1);
    assert_eq!(
        e.group(e.d().entry.unwrap()).kind,
        CertKind::Timeout { round: 0 }
    );
}
#[test]
fn config_rotation_retires_authority_and_preserves_grid() {
    let c = config([1; 4]);
    let (mut e, mut o) = engine(&c, 0, 4, 0);
    e.elapse(3, &mut o).unwrap();
    let mut roster = c.members().to_vec();
    roster.remove(0);
    let next = Config::new([99; 32], roster, 512, 10, c.timeouts()).unwrap();
    assert!(e.init(&next, 5, 0, &mut o).unwrap());
    assert_eq!(e.local_index(), None);
    assert_eq!(e.d().grid_remaining, 7);
    let before = o.sent.len();
    e.elapse(30, &mut o).unwrap();
    assert_eq!(o.sent.len(), before);
    assert!(!e.init(&c, 5, 0, &mut o).unwrap());
    let e = Engine::recover(e.into_storage(), keys()[0].clone(), &mut o).unwrap();
    assert_eq!(e.local_index(), None);
    assert_eq!(e.c().hash(), next.hash());
}

#[test]
fn crash_cut_every_proposal_lock_and_decision_write() {
    let c = config([7, 1, 1, 1]);
    let (base, _) = engine(&c, 0, 4, 0);
    let base = base.into_storage();
    for cut in 0..45 {
        let mut o = Out::default();
        let mut e = Engine::recover(base.clone(), keys()[0].clone(), &mut o).unwrap();
        o.sent.clear();
        e.store.fail_at = Some(e.store.writes + cut);
        let result = e.supply_proposal(e.proposal_tag().unwrap(), b"first", Status::Valid, &mut o);
        if result.is_err() {
            assert!(!e.is_active());
            let sent = o.sent.len();
            assert_eq!(e.elapse(1, &mut o), Err(Error::Inactive));
            assert_eq!(sent, o.sent.len());
        }
        let committed = e.d().proposal_intent.map(|i| i.content.id);
        // Every signed output is licensed by the still-persisted exact intent.
        for (_, vote, header, _) in &o.sent {
            if let Some(v) = vote {
                assert_eq!(e.d().intents[v.content.kind.index()], Some(v.content));
            }
            if let Some(h) = header {
                assert_eq!(e.d().proposal_intent.unwrap().content, h.content);
            }
        }
        let mut store = e.into_storage();
        store.clear_fault();
        let mut e = Engine::recover(store, keys()[0].clone(), &mut o).unwrap();
        if let Some(tag) = e.proposal_tag() {
            e.supply_proposal(tag, b"second", Status::Valid, &mut o)
                .unwrap();
        }
        assert_eq!(e.d().step, Step::Decided, "cut={cut}");
        assert_eq!(
            e.proposal(e.d().pending.unwrap().proposal)
                .header
                .content
                .id,
            committed.unwrap_or(body_id(b"second")),
            "cut={cut}"
        );
    }
}
#[test]
fn crash_cut_height_entry_never_loses_enter_intent() {
    let c = config([1; 4]);
    let (base, _) = engine(&c, 0, 4, 1);
    let base = base.into_storage();
    for cut in 0..12 {
        let mut o = Out::default();
        let mut e = Engine::recover(base.clone(), keys()[0].clone(), &mut o).unwrap();
        e.store.fail_at = Some(e.store.writes + cut);
        let _ = e.elapse(1, &mut o);
        let mut s = e.into_storage();
        s.clear_fault();
        let mut e = Engine::recover(s, keys()[0].clone(), &mut o).unwrap();
        if e.d().step == Step::PreRound {
            e.elapse(1, &mut o).unwrap();
        }
        assert!(e.own_current(VoteKind::Enter).is_some(), "cut={cut}");
    }
}
#[test]
fn crash_cut_roster_staging_keeps_old_or_new_whole_authority() {
    let c = config([7, 1, 1, 1]);
    let (mut base, mut out) = engine(&c, 0, 4, 0);
    base.supply_proposal(
        base.proposal_tag().unwrap(),
        b"old",
        Status::Valid,
        &mut out,
    )
    .unwrap();
    let base = base.into_storage();
    let next = config([1, 7, 1, 1]);
    for cut in 0..15 {
        let mut o = Out::default();
        let mut e = Engine::recover(base.clone(), keys()[0].clone(), &mut o).unwrap();
        e.store.fail_at = Some(e.store.writes + cut);
        let _ = e.init(&next, 5, 5, &mut o);
        let mut s = e.into_storage();
        s.clear_fault();
        let e = Engine::recover(s, keys()[0].clone(), &mut o).unwrap();
        match e.d().height {
            4 => {
                assert_eq!(e.c().hash(), c.hash());
                assert_eq!(e.d().step, Step::Decided);
                assert_eq!(e.proposal(e.d().pending.unwrap().proposal).body, b"old");
            }
            5 => {
                assert_eq!(e.c().hash(), next.hash());
                assert!(e.d().pending.is_none());
                assert!(e.own_current(VoteKind::Precommit).is_none());
            }
            _ => panic!("mixed authority"),
        }
    }
}
#[test]
fn corrupted_published_intent_fails_closed_without_output() {
    let c = config([1; 4]);
    let (e, _) = engine(&c, 0, 4, 0);
    let mut store = e.into_storage();
    store.descriptor.as_mut().unwrap().intents[0]
        .as_mut()
        .unwrap()
        .signer = 1;
    let mut out = Out::default();
    assert!(matches!(
        Engine::recover(store, keys()[0].clone(), &mut out),
        Err(Error::Corrupt)
    ));
    assert!(out.sent.is_empty());
    assert!(out.decisions.is_empty());
}
#[test]
fn same_id_different_header_pending_never_aliases() {
    let c = config([1; 4]);
    let (mut e, mut o) = engine(&c, 3, 4, 100);
    let first = header(&c, 0, None, b"same");
    e.receive(
        ordinary(
            None,
            Some(Proposal {
                header: &first,
                body: b"same",
            }),
            &[],
        ),
        Status::Indeterminate,
        &mut o,
    )
    .unwrap();
    let second = header(&c, 8, Some(0), b"same");
    let vs = (0..3)
        .map(|s| vote(&c, s, 8, VoteKind::Precommit, Some(second.content.id)))
        .collect::<Vec<_>>();
    let g = [Group {
        kind: CertKind::Decision {
            round: 8,
            id: second.content.id,
        },
        votes: Votes::Dense(&vs),
    }];
    e.receive(
        Frame {
            message: Message::Dec,
            outer: None,
            proposal: Some(Proposal {
                header: &second,
                body: b"same",
            }),
            groups: &g,
        },
        Status::Indeterminate,
        &mut o,
    )
    .unwrap();
    let d = e.d();
    assert_ne!(d.slot.unwrap().index, d.pending.unwrap().proposal.index);
    assert_eq!(e.proposal(d.pending.unwrap().proposal).header, &second);
}
#[test]
fn form_b_waits_then_late_polka_updates_valid_without_second_precommit() {
    let c = config([1; 4]);
    let (mut e, mut o) = engine(&c, 3, 4, 0);
    let enter = vote(&c, 1, 2, VoteKind::Enter, None);
    let other = [vote(&c, 2, 2, VoteKind::Enter, None)];
    let jump = [Group {
        kind: CertKind::Jump { round: 2 },
        votes: Votes::Dense(&other),
    }]; // Package two valid votes using outer + ROW carrier instead.
    let _ = jump;
    let pv = [vote(&c, 2, 2, VoteKind::Prevote, None)];
    let g = [Group {
        kind: CertKind::Row {
            round: 2,
            kind: VoteKind::Prevote,
        },
        votes: Votes::Dense(&pv),
    }];
    e.receive(
        ordinary(Some(&enter), None, &g),
        Status::Indeterminate,
        &mut o,
    )
    .unwrap();
    assert_eq!(e.d().round, 2);
    let h = header(&c, 2, Some(0), b"form-b");
    let vs = (0..3)
        .map(|s| vote(&c, s, 2, VoteKind::Prevote, Some(h.content.id)))
        .collect::<Vec<_>>();
    let g = [Group {
        kind: CertKind::Polka {
            round: 2,
            id: h.content.id,
        },
        votes: Votes::Dense(&vs),
    }];
    e.receive(
        ordinary(
            None,
            Some(Proposal {
                header: &h,
                body: b"form-b",
            }),
            &g,
        ),
        Status::Valid,
        &mut o,
    )
    .unwrap();
    assert_eq!(e.d().step, Step::Propose);
    assert!(!e.d().justified);
    e.elapse(7, &mut o).unwrap();
    assert_eq!(e.d().valid_round, Some(2));
    assert_eq!(e.own_current(VoteKind::Prevote).unwrap().content.id, None);
    assert_eq!(
        e.own_current(VoteKind::Precommit).unwrap().content.id,
        Some(h.content.id)
    );
}

#[test]
fn complete_group_repairs_conflict_but_never_own_row() {
    let c = config([1; 4]);
    let (mut e, mut o) = engine(&c, 3, 4, 0);
    let stale = vote(&c, 0, 0, VoteKind::Prevote, None);
    e.receive(
        ordinary(Some(&stale), None, &[]),
        Status::Indeterminate,
        &mut o,
    )
    .unwrap();
    e.elapse(30, &mut o).unwrap();
    let own = e.own_current(VoteKind::Prevote).copied().unwrap();
    let id = [9; 32];
    let votes = (0..4)
        .map(|s| vote(&c, s, 0, VoteKind::Prevote, Some(id)))
        .collect::<Vec<_>>();
    let groups = [Group {
        kind: CertKind::Polka { round: 0, id },
        votes: Votes::Dense(&votes),
    }];
    e.receive(ordinary(None, None, &groups), Status::Indeterminate, &mut o)
        .unwrap();
    assert_eq!(e.pv[0].unwrap().content.id, Some(id));
    assert_eq!(e.pv[3], Some(own));
    assert_eq!(e.own_current(VoteKind::Prevote), Some(&own));
}
#[test]
fn jump_deterministic_phase_then_nil_first_ties() {
    let c = config([1, 1, 1, 7]);
    let (mut e, mut o) = engine(&c, 0, 4, 0);
    let high = vote(&c, 3, 5, VoteKind::Prevote, Some([8; 32]));
    let low = vote(&c, 3, 5, VoteKind::Prevote, None);
    let groups = [
        Group {
            kind: CertKind::Row {
                round: 5,
                kind: VoteKind::Prevote,
            },
            votes: Votes::Dense(std::slice::from_ref(&high)),
        },
        Group {
            kind: CertKind::Row {
                round: 5,
                kind: VoteKind::Prevote,
            },
            votes: Votes::Dense(std::slice::from_ref(&low)),
        },
    ];
    e.receive(ordinary(None, None, &groups), Status::Indeterminate, &mut o)
        .unwrap();
    let entry = e.group(e.d().entry.unwrap());
    assert_eq!(entry.votes.len(), 1);
    assert_eq!(entry.votes.iter().next().unwrap(), &low);
    let high_phase = vote(&c, 3, 7, VoteKind::Precommit, Some([9; 32]));
    let low_phase = vote(&c, 3, 7, VoteKind::Prevote, None);
    let groups = [Group {
        kind: CertKind::Row {
            round: 7,
            kind: VoteKind::Precommit,
        },
        votes: Votes::Dense(std::slice::from_ref(&high_phase)),
    }];
    e.receive(
        ordinary(Some(&low_phase), None, &groups),
        Status::Indeterminate,
        &mut o,
    )
    .unwrap();
    assert_eq!(
        e.group(e.d().entry.unwrap()).votes.iter().next().unwrap(),
        &high_phase
    );
}
#[test]
fn stale_validity_event_cannot_resolve_reacquired_copy() {
    let c = config([1; 4]);
    let (mut e, mut o) = engine(&c, 3, 4, 100);
    let h = header(&c, 0, None, b"copy");
    let f = ordinary(
        None,
        Some(Proposal {
            header: &h,
            body: b"copy",
        }),
        &[],
    );
    e.receive(f, Status::Indeterminate, &mut o).unwrap();
    let old = e.body_tag(e.d().slot.unwrap());
    let mut d = e.d().clone();
    d.slot = None;
    e.commit(d).unwrap();
    e.receive(f, Status::Indeterminate, &mut o).unwrap();
    let new = e.body_tag(e.d().slot.unwrap());
    assert_ne!(old, new);
    assert_eq!(e.validity(old, Status::Valid, &mut o), Err(Error::Stale));
    assert_eq!(
        e.d().statuses[e.d().slot.unwrap().index],
        Status::Indeterminate
    );
}
#[test]
fn exact_alias_fresh_valid_dec_resolves_retained_bottom() {
    let c = config([1; 4]);
    let (mut e, mut o) = engine(&c, 3, 4, 100);
    let h = header(&c, 0, None, b"alias");
    let p = Proposal {
        header: &h,
        body: b"alias",
    };
    e.receive(ordinary(None, Some(p), &[]), Status::Indeterminate, &mut o)
        .unwrap();
    let slot = e.d().slot.unwrap();
    let votes = (0..3)
        .map(|s| vote(&c, s, 0, VoteKind::Precommit, Some(h.content.id)))
        .collect::<Vec<_>>();
    let groups = [Group {
        kind: CertKind::Decision {
            round: 0,
            id: h.content.id,
        },
        votes: Votes::Dense(&votes),
    }];
    e.receive(
        Frame {
            message: Message::Dec,
            outer: None,
            proposal: Some(p),
            groups: &groups,
        },
        Status::Valid,
        &mut o,
    )
    .unwrap();
    assert_eq!(e.d().step, Step::Decided);
    assert_eq!(e.d().pending.unwrap().proposal, slot);
}
#[test]
fn oversized_sparse_rows_rejected_before_scan() {
    let c = config([1; 4]);
    let (mut e, mut o) = engine(&c, 0, 4, 0);
    let sparse = [None; 100];
    let groups = [Group {
        kind: CertKind::Row {
            round: 0,
            kind: VoteKind::Prevote,
        },
        votes: Votes::Rows(&sparse),
    }];
    assert_eq!(
        e.receive(ordinary(None, None, &groups), Status::Indeterminate, &mut o),
        Err(Error::Invalid)
    );
}
#[test]
fn round_time_arithmetic_exhaustion_fails_closed() {
    let c = config([1; 4]);
    assert_eq!(c.timeouts().at(0, u64::MAX), Err(Error::Exhausted));
    let (mut e, mut o) = engine(&c, 0, 4, 0);
    let future = [
        vote(&c, 1, u64::MAX, VoteKind::Prevote, None),
        vote(&c, 2, u64::MAX, VoteKind::Prevote, None),
    ];
    let groups = [Group {
        kind: CertKind::Row {
            round: u64::MAX,
            kind: VoteKind::Prevote,
        },
        votes: Votes::Dense(&future),
    }];
    assert_eq!(
        e.receive(ordinary(None, None, &groups), Status::Indeterminate, &mut o),
        Err(Error::Exhausted)
    );
    assert!(!e.is_active());
}
