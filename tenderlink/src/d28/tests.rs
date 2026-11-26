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

// Original receive scans retained as a deliberately slow, independent oracle.
// In particular, a whole malformed group vanishes, while duplicate signers in
// DIFFERENT valid groups are legal and count once for each matching tuple.
fn slow_group_valid(e: &Engine<MemoryStore>, g: Group<'_>) -> bool {
    if g.votes.is_empty() || g.votes.len() > e.c().members.len() {
        return false;
    }
    let mut weight = 0;
    for (i, v) in g.votes.iter().enumerate() {
        let c = v.content;
        if c.height != e.d().height
            || !v.verify(e.c())
            || g.votes.iter().take(i).any(|v| v.content.signer == c.signer)
        {
            return false;
        }
        if !match g.kind {
            CertKind::Polka { round, id } => {
                c.kind == VoteKind::Prevote && c.round == round && c.id == Some(id)
            }
            CertKind::Decision { round, id } => {
                c.kind == VoteKind::Precommit && c.round == round && c.id == Some(id)
            }
            CertKind::Timeout { round } => c.kind == VoteKind::Precommit && c.round == round,
            CertKind::Jump { round } => c.round >= round,
            CertKind::Row { round, kind } => {
                kind != VoteKind::Enter && c.kind == kind && c.round == round
            }
        } {
            return false;
        }
        weight += e.c().members[c.signer as usize].weight;
    }
    match g.kind {
        CertKind::Polka { .. } | CertKind::Decision { .. } | CertKind::Timeout { .. } => {
            e.c().quorum(weight)
        }
        CertKind::Jump { .. } => e.c().witness(weight),
        CertKind::Row { .. } => true,
    }
}
fn slow_accepted<'a>(e: &Engine<MemoryStore>, f: Frame<'a>, position: usize) -> Option<&'a Vote> {
    if let Some(v) = f.outer {
        if position == 0 {
            return (v.content.height == e.d().height && v.verify(e.c())).then_some(v);
        }
    }
    let mut i = usize::from(f.outer.is_some());
    for g in f.groups {
        let valid = slow_group_valid(e, *g);
        for v in g.votes.iter() {
            if position == i {
                return valid.then_some(v);
            }
            i += 1;
        }
    }
    None
}
fn slow_weight(e: &Engine<MemoryStore>, accepted: &[Option<Vote>], c: VoteContent) -> u64 {
    (0..e.c().members.len())
        .filter(|s| {
            accepted.iter().flatten().any(|v| {
                let x = v.content;
                x.signer as usize == *s && (x.round, x.kind, x.id) == (c.round, c.kind, c.id)
            })
        })
        .map(|s| e.c().members[s].weight)
        .sum()
}
#[test]
fn indexed_receive_matches_slow_scans_and_row_order() {
    let c = config([4, 2, 1, 3]);
    let mut seed = 0x28_u64;
    for case in 0..128 {
        let (mut e, mut out) = engine(&c, 3, 4, 100);
        let mut next = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            seed >> 32
        };
        // Every frame is admitted: <=5 occupied positions and <=4 physical
        // rows per group. Physical row order intentionally differs from signer.
        let mut votes = (0..5)
            .map(|_| {
                let signer = next() as usize % 4;
                let round = next() % 3;
                let kind =
                    [VoteKind::Enter, VoteKind::Prevote, VoteKind::Precommit][next() as usize % 3];
                let id = if kind == VoteKind::Enter || next() % 2 == 0 {
                    None
                } else {
                    Some([next() as u8 % 3; 32])
                };
                vote(&c, signer, round, kind, id)
            })
            .collect::<Vec<_>>();
        // Deliberate legal across-group duplicates, illegal within-group
        // duplicates, malformed signatures/heights, and valid matching rows.
        if case % 3 == 0 {
            votes[1] = votes[0];
        }
        if case % 4 == 0 {
            votes[2] = votes[1];
        }
        if case % 7 == 0 {
            votes[3].signature[0] ^= 1;
        }
        if case % 11 == 0 {
            votes[4].content.height += 1;
        }
        let sparse = [Some(votes[3]), None, None, Some(votes[4])];
        let row_kind = |v: Vote| CertKind::Row {
            round: v.content.round,
            kind: v.content.kind,
        };
        let groups = [
            Group {
                kind: row_kind(votes[1]),
                votes: Votes::Dense(&votes[1..3]),
            },
            Group {
                kind: if case % 2 == 0 {
                    CertKind::Jump { round: 0 }
                } else {
                    row_kind(votes[3])
                },
                votes: Votes::Rows(&sparse),
            },
        ];
        let f = ordinary(Some(&votes[0]), None, &groups);
        let accepted = (0..5)
            .map(|i| slow_accepted(&e, f, i).copied())
            .collect::<Vec<_>>();
        e.index_frame(f);
        assert_eq!(e.rx_positions.len(), 5);
        for (position, expected) in accepted.iter().enumerate() {
            assert_eq!(
                e.accepted_vote(f, position).copied(),
                *expected,
                "case={case} pos={position}"
            );
            if let Some(v) = expected {
                assert_eq!(
                    e.rx_weight(f, position),
                    slow_weight(&e, &accepted, v.content),
                    "case={case} pos={position}"
                );
            }
        }
        assert!(e.accepted_vote(f, 5).is_none());
        // Retained rows take precedence over the FIRST matching RX PC, even
        // when different valid groups contain conflicting ids for one signer.
        e.pc[0] = Some(vote(&c, 0, 0, VoteKind::Precommit, Some([8; 32])));
        e.index_timeout(f);
        for signer in 0..4 {
            let expected = e.pc[signer].or_else(|| {
                accepted
                    .iter()
                    .flatten()
                    .find(|v| {
                        v.content.signer as usize == signer
                            && v.content.round == 0
                            && v.content.kind == VoteKind::Precommit
                    })
                    .copied()
            });
            assert_eq!(
                e.timeout_vote(f, signer),
                expected,
                "case={case} signer={signer}"
            );
        }
        // Compare the entire repair pass, including its sequential overwrites,
        // against the old algorithm; pre-round prevents subsequent signing.
        for signer in 0..4 {
            e.pv[signer] = Some(vote(&c, signer, 0, VoteKind::Prevote, Some([9; 32])));
            e.pc[signer] = Some(vote(&c, signer, 0, VoteKind::Precommit, Some([9; 32])));
        }
        let mut pv = e.pv.clone();
        let mut pc = e.pc.clone();
        for v in accepted
            .iter()
            .flatten()
            .filter(|v| v.content.round == 0 && v.content.kind != VoteKind::Enter)
        {
            let row = if v.content.kind == VoteKind::Prevote {
                &mut pv
            } else {
                &mut pc
            };
            let signer = v.content.signer as usize;
            if row[signer].is_none_or(|old| old.content == v.content)
                || c.quorum(slow_weight(&e, &accepted, v.content))
            {
                row[signer] = Some(*v);
            }
        }
        e.receive(f, Status::Indeterminate, &mut out).unwrap();
        assert_eq!(e.pv, pv, "case={case}");
        assert_eq!(e.pc, pc, "case={case}");
        assert!(e.rx_positions.is_empty());
        assert!(e.rx_weights.is_empty());
        assert!(e.selected.iter().all(Option::is_none));
    }
}
#[test]
fn rx_quorum_deduplicates_across_groups_and_excludes_whole_malformed_group() {
    let c = config([4, 2, 1, 3]);
    let (mut e, mut out) = engine(&c, 3, 4, 100);
    let id = Some([7; 32]);
    let old = vote(&c, 1, 0, VoteKind::Prevote, None);
    e.pv[1] = Some(old);
    let outer = vote(&c, 0, 0, VoteKind::Prevote, id);
    let repeated = [outer, vote(&c, 1, 0, VoteKind::Prevote, id)];
    let malformed = [
        vote(&c, 2, 0, VoteKind::Prevote, id),
        vote(&c, 3, 1, VoteKind::Prevote, id),
    ];
    let groups = [
        Group {
            kind: CertKind::Row {
                round: 0,
                kind: VoteKind::Prevote,
            },
            votes: Votes::Dense(&repeated),
        },
        Group {
            kind: CertKind::Row {
                round: 0,
                kind: VoteKind::Prevote,
            },
            votes: Votes::Dense(&malformed),
        },
    ];
    e.receive(
        ordinary(Some(&outer), None, &groups),
        Status::Indeterminate,
        &mut out,
    )
    .unwrap();
    // Only weights 4+2 count, not repeated 4 and not the otherwise-valid vote
    // of signer 2 in the malformed group: six is not a quorum of ten.
    assert_eq!(e.pv[1], Some(old));
    assert!(e.pv[2].is_none());
    let extra = [None, None, Some(malformed[0]), None];
    let groups = [
        groups[0],
        Group {
            kind: groups[1].kind,
            votes: Votes::Rows(&extra),
        },
    ];
    e.receive(
        ordinary(Some(&outer), None, &groups),
        Status::Indeterminate,
        &mut out,
    )
    .unwrap();
    assert_eq!(e.pv[1], Some(repeated[1])); // 4+2+1 is a strict quorum.
    // A later frame must not inherit that cached seven-weight tuple.
    e.pv[1] = Some(old);
    e.receive(
        ordinary(Some(&repeated[1]), None, &[]),
        Status::Indeterminate,
        &mut out,
    )
    .unwrap();
    assert_eq!(e.pv[1], Some(old));
}
#[test]
fn same_frame_jump_preserves_sparse_positions_and_sequential_conflict_choices() {
    let c = config([1, 1, 1, 7]);
    let (mut e, mut out) = engine(&c, 0, 4, 0);
    let first = vote(&c, 3, 5, VoteKind::Prevote, Some([8; 32]));
    let second = vote(&c, 3, 5, VoteKind::Prevote, Some([9; 32]));
    let last = vote(&c, 3, 5, VoteKind::Prevote, None);
    let pc = vote(&c, 3, 5, VoteKind::Precommit, Some([6; 32]));
    let sparse = [None, None, Some(second), None];
    let groups = [
        Group {
            kind: CertKind::Row {
                round: 5,
                kind: VoteKind::Prevote,
            },
            votes: Votes::Rows(&sparse),
        },
        Group {
            kind: CertKind::Row {
                round: 5,
                kind: VoteKind::Precommit,
            },
            votes: Votes::Dense(std::slice::from_ref(&pc)),
        },
        Group {
            kind: CertKind::Row {
                round: 5,
                kind: VoteKind::Prevote,
            },
            votes: Votes::Dense(std::slice::from_ref(&last)),
        },
    ];
    e.receive(
        ordinary(Some(&first), None, &groups),
        Status::Indeterminate,
        &mut out,
    )
    .unwrap();
    assert_eq!(e.d().round, 5);
    // Jump picks greatest kind, independent of row repair's later frame order.
    assert_eq!(
        e.group(e.d().entry.unwrap())
            .votes
            .iter()
            .copied()
            .collect::<Vec<_>>(),
        vec![pc]
    );
    assert_eq!(e.pv[3], Some(last));
    assert_eq!(e.pc[3], Some(pc));
    assert!(e.rx_positions.is_empty());
    assert!(e.rx_weights.is_empty());
}
#[test]
fn indexed_jump_matches_original_max_cut_and_ties() {
    let c = config([2, 3, 1, 4]);
    let mut seed = 0x412_u64;
    for case in 0..64 {
        let (mut e, mut out) = engine(&c, 0, 4, 0);
        let mut next = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            seed >> 32
        };
        let mut votes = (0..5)
            .map(|_| {
                let signer = next() as usize % 4;
                let round = 1 + next() % 8;
                let kind =
                    [VoteKind::Enter, VoteKind::Prevote, VoteKind::Precommit][next() as usize % 3];
                let id = if kind == VoteKind::Enter || next() % 2 == 0 {
                    None
                } else {
                    Some([next() as u8 % 3; 32])
                };
                vote(&c, signer, round, kind, id)
            })
            .collect::<Vec<_>>();
        if case % 3 == 0 {
            votes[2] = votes[0];
        }
        if case % 7 == 0 {
            votes[3].signature[0] ^= 1;
        }
        let sparse = [None, Some(votes[1]), None, None];
        let groups = (1..5)
            .map(|i| Group {
                kind: if votes[i].content.kind == VoteKind::Enter {
                    CertKind::Jump { round: 0 }
                } else {
                    CertKind::Row {
                        round: votes[i].content.round,
                        kind: votes[i].content.kind,
                    }
                },
                votes: if i == 1 {
                    Votes::Rows(&sparse)
                } else {
                    Votes::Dense(&votes[i..i + 1])
                },
            })
            .collect::<Vec<_>>();
        let f = ordinary(Some(&votes[0]), None, &groups);
        let accepted = (0..5)
            .filter_map(|i| slow_accepted(&e, f, i).copied())
            .collect::<Vec<_>>();
        let mut selected: [Option<Vote>; 4] = [None; 4];
        for v in accepted {
            let x = v.content;
            let signer = x.signer as usize;
            if signer == e.me || x.round <= e.d().round {
                continue;
            }
            if selected[signer].is_none_or(|v| {
                let old = v.content;
                (x.round, x.kind) > (old.round, old.kind)
                    || ((x.round, x.kind) == (old.round, old.kind) && x.id < old.id)
            }) {
                selected[signer] = Some(v);
            }
        }
        let target = selected
            .iter()
            .flatten()
            .map(|v| v.content.round)
            .filter(|round| {
                let weight = selected
                    .iter()
                    .enumerate()
                    .filter(|(_, v)| v.is_some_and(|v| v.content.round >= *round))
                    .map(|(s, _)| c.members[s].weight)
                    .sum();
                c.witness(weight)
            })
            .max();
        e.receive(f, Status::Indeterminate, &mut out).unwrap();
        assert_eq!(e.d().round, target.unwrap_or(0), "case={case}");
        if let Some(round) = target {
            let expected = selected
                .into_iter()
                .flatten()
                .filter(|v| v.content.round >= round)
                .collect::<Vec<_>>();
            let entry = e.group(e.d().entry.unwrap());
            assert_eq!(entry.kind, CertKind::Jump { round });
            assert_eq!(
                entry.votes.iter().copied().collect::<Vec<_>>(),
                expected,
                "case={case}"
            );
        } else {
            assert!(e.d().entry.is_none());
        }
    }
}
#[test]
fn timeout_index_keeps_first_matching_vote_and_rx_scratch_is_not_evidence() {
    let c = config([1; 4]);
    let (mut e, mut out) = engine(&c, 0, 4, 100);
    let first = vote(&c, 1, 0, VoteKind::Precommit, Some([1; 32]));
    let second = vote(&c, 1, 0, VoteKind::Precommit, None);
    let future = vote(&c, 2, 1, VoteKind::Precommit, None);
    let sparse = [None, None, Some(second), None];
    let groups = [
        Group {
            kind: CertKind::Row {
                round: 0,
                kind: VoteKind::Precommit,
            },
            votes: Votes::Rows(&sparse),
        },
        Group {
            kind: CertKind::Row {
                round: 1,
                kind: VoteKind::Precommit,
            },
            votes: Votes::Dense(std::slice::from_ref(&future)),
        },
    ];
    let f = ordinary(Some(&first), None, &groups);
    e.index_frame(f);
    e.index_timeout(f);
    assert_eq!(e.timeout_vote(f, 1), Some(first));
    assert_eq!(e.timeout_vote(f, 2), None);
    e.pc[1] = Some(second);
    assert_eq!(e.timeout_vote(f, 1), Some(second));
    let oversized = [None; 5];
    let bad = [Group {
        kind: groups[0].kind,
        votes: Votes::Rows(&oversized),
    }];
    assert_eq!(
        e.receive(ordinary(None, None, &bad), Status::Indeterminate, &mut out),
        Err(Error::Invalid)
    );
    assert!(e.rx_positions.is_empty());
    assert!(e.rx_weights.is_empty());
    assert!(!e.outer_ok);
    assert!(!e.group_ok.iter().any(|x| *x));
    e.index_frame(f);
    e.rx_weight(f, 0);
    let next = Config::new([99; 32], c.members()[..3].to_vec(), 512, 10, c.timeouts()).unwrap();
    e.init(&next, 5, 100, &mut out).unwrap();
    assert!(e.rx_positions.is_empty());
    assert!(e.rx_weights.is_empty());
    assert_eq!(e.group_ok.len(), 4);
    assert_eq!(e.selected.len(), 3);
    let recovered = Engine::recover(e.into_storage(), keys()[0].clone(), &mut out).unwrap();
    assert!(recovered.rx_positions.is_empty());
    assert!(recovered.rx_weights.is_empty());
    assert!(recovered.selected.iter().all(Option::is_none));
}

#[test]
fn verified_outer_reuse_requires_exact_bytes_and_all_group_guards() {
    let c = config([1; 4]);
    let (mut e, _) = engine(&c, 0, 4, 100);
    let valid = vote(&c, 1, 0, VoteKind::Prevote, Some([1; 32]));
    let other = vote(&c, 2, 0, VoteKind::Prevote, Some([1; 32]));
    let mut bad_signature = valid;
    bad_signature.signature[0] ^= 1;
    let mut bad_content = valid;
    bad_content.content.id = Some([2; 32]);
    let mut wrong_height = valid;
    wrong_height.content.height += 1;
    let variants = [valid, bad_signature, bad_content, wrong_height];
    for outer in &variants {
        for inner in &variants {
            let votes = [*inner, other];
            let duplicate = [*inner, *inner];
            for kind in [
                CertKind::Row {
                    round: 0,
                    kind: VoteKind::Prevote,
                },
                CertKind::Row {
                    round: 1,
                    kind: VoteKind::Prevote,
                },
                CertKind::Polka {
                    round: 0,
                    id: [1; 32],
                },
            ] {
                for rows in [&votes[..], &duplicate[..]] {
                    let groups = [Group {
                        kind,
                        votes: Votes::Dense(rows),
                    }];
                    let frame = ordinary(Some(outer), None, &groups);
                    let expected = (0..3)
                        .map(|i| slow_accepted(&e, frame, i).copied())
                        .collect::<Vec<_>>();
                    e.index_frame(frame);
                    assert_eq!(e.group_ok[0], slow_group_valid(&e, groups[0]));
                    for (position, expected) in expected.iter().enumerate() {
                        assert_eq!(e.accepted_vote(frame, position).copied(), *expected);
                    }
                }
            }
        }
    }
}

mod coalesced {
    use super::*;

    // The runtime's bounded Events reduction, including retry insertion order.
    // Consensus callbacks are not coalesced and must match exactly.
    #[derive(Debug, Default, PartialEq, Eq)]
    struct Events {
        votes: [Option<Vote>; 3],
        retries: [Option<BodyTag>; 2],
        decisions: Vec<Header>,
        acquisitions: Vec<ProposalTag>,
    }
    impl Sink for Events {
        fn send(&mut self, target: Target, frame: Frame<'_>) {
            assert_eq!(target, Target::All);
            assert!(frame.proposal.is_none() && frame.groups.is_empty());
            if let Some(vote) = frame.outer {
                self.votes[vote.content.kind.index()] = Some(*vote);
            }
        }
        fn decided(&mut self, _: u64, proposal: Proposal<'_>, _: Group<'_>) {
            self.decisions.push(*proposal.header);
        }
        fn acquire(&mut self, tag: ProposalTag) {
            self.acquisitions.push(tag);
        }
        fn retry(&mut self, tag: BodyTag, _: Proposal<'_>) {
            if !self.retries.contains(&Some(tag)) {
                if let Some(slot) = self.retries.iter_mut().find(|slot| slot.is_none()) {
                    *slot = Some(tag);
                }
            }
        }
    }

    fn clone_engine(e: &Engine<MemoryStore>) -> Engine<MemoryStore> {
        Engine {
            emission: Emission::Guided,
            store: e.store.clone(),
            key: e.key.clone(),
            me: e.me,
            active: e.active,
            pv: e.pv.clone(),
            pc: e.pc.clone(),
            replied: e.replied.clone(),
            selected: e.selected.clone(),
            group_ok: e.group_ok.clone(),
            outer_ok: e.outer_ok,
            rx_positions: e.rx_positions.clone(),
            rx_weights: e.rx_weights.clone(),
            acquiring: e.acquiring,
        }
    }
    fn assert_same(a: &Engine<MemoryStore>, b: &Engine<MemoryStore>) {
        assert_eq!(a.d(), b.d(), "descriptor");
        assert_eq!(a.store.own, b.store.own, "signed OWN rows");
        assert_eq!(a.store.own_header, b.store.own_header);
        assert_eq!(a.store.headers, b.store.headers);
        assert_eq!(a.store.bodies, b.store.bodies);
        assert_eq!(a.store.region_headers, b.store.region_headers);
        assert_eq!(a.store.regions, b.store.regions);
        assert_eq!(a.store.configs, b.store.configs);
        assert_eq!(a.store.sealed, b.store.sealed);
        assert_eq!(a.pv, b.pv);
        assert_eq!(a.pc, b.pc);
        assert_eq!(a.replied, b.replied);
        assert_eq!(a.acquiring, b.acquiring);
        assert_eq!(a.active, b.active);
    }
    fn compare_elapsed(base: &Engine<MemoryStore>, online: u64) {
        let mut reference = clone_engine(base);
        let mut coalesced = clone_engine(base);
        let mut expected = Events::default();
        let mut actual = Events::default();
        reference.elapse(online, &mut expected).unwrap();
        coalesced.elapse_coalesced(online, &mut actual).unwrap();
        assert_same(&reference, &coalesced);
        assert_eq!(expected, actual, "effective guided events");
        let mut expected = Events::default();
        let mut actual = Events::default();
        let reference = Engine::recover_with_emission(
            reference.store,
            base.key.clone(),
            Emission::Guided,
            &mut expected,
        )
        .unwrap();
        let coalesced = Engine::recover_with_emission(
            coalesced.store,
            base.key.clone(),
            Emission::Guided,
            &mut actual,
        )
        .unwrap();
        assert_same(&reference, &coalesced);
        assert_eq!(expected, actual, "recovery events");
    }
    fn rows(e: &mut Engine<MemoryStore>, kind: VoteKind, id: Option<Id>) {
        let votes = (0..3)
            .map(|s| vote(e.c(), s, 0, kind, id))
            .collect::<Vec<_>>();
        let groups = [Group {
            kind: CertKind::Row { round: 0, kind },
            votes: Votes::Dense(&votes),
        }];
        e.receive(
            ordinary(None, None, &groups),
            Status::Indeterminate,
            &mut Out::default(),
        )
        .unwrap();
    }
    fn snapshots(c: &Config) -> Vec<Engine<MemoryStore>> {
        let (pre, _) = engine(c, 1, 4, 37);
        let (propose, _) = engine(c, 3, 4, 0);
        let mut prevote = clone_engine(&propose);
        prevote.elapse(30, &mut Events::default()).unwrap();
        assert_eq!(prevote.d().step, Step::Prevote);
        let mut fallback = clone_engine(&propose);
        rows(&mut fallback, VoteKind::Prevote, Some([7; 32]));
        rows(&mut fallback, VoteKind::Precommit, None);
        assert!(fallback.d().timers[1].active && fallback.d().timers[3].active);
        let mut precommit = clone_engine(&propose);
        rows(&mut precommit, VoteKind::Prevote, None);
        precommit.elapse(5, &mut Events::default()).unwrap();
        assert_eq!(precommit.d().step, Step::Precommit);

        let mut retry = clone_engine(&propose);
        let h = header(c, 0, None, b"retry-slot");
        retry
            .receive(
                ordinary(
                    None,
                    Some(Proposal {
                        header: &h,
                        body: b"retry-slot",
                    }),
                    &[],
                ),
                Status::Indeterminate,
                &mut Events::default(),
            )
            .unwrap();
        let h = header(c, 7, None, b"retry-pending");
        let votes = (0..3)
            .map(|s| vote(c, s, 7, VoteKind::Precommit, Some(h.content.id)))
            .collect::<Vec<_>>();
        let groups = [Group {
            kind: CertKind::Decision {
                round: 7,
                id: h.content.id,
            },
            votes: Votes::Dense(&votes),
        }];
        let pending = Frame {
            message: Message::Dec,
            outer: None,
            proposal: Some(Proposal {
                header: &h,
                body: b"retry-pending",
            }),
            groups: &groups,
        };
        retry
            .receive(pending, Status::Indeterminate, &mut Events::default())
            .unwrap();
        let mut pre_retry = clone_engine(&pre);
        pre_retry
            .receive(pending, Status::Indeterminate, &mut Events::default())
            .unwrap();

        // Carry a locked body into the next round, where this node is proposer:
        // timeout-driven start must retain every intent/header/body/cert write.
        let (mut locked, _) = engine(c, 1, 4, 0);
        locked.set_emission(Emission::Guided);
        let h = header(c, 0, None, b"locked");
        locked
            .receive(
                ordinary(
                    None,
                    Some(Proposal {
                        header: &h,
                        body: b"locked",
                    }),
                    &[],
                ),
                Status::Valid,
                &mut Events::default(),
            )
            .unwrap();
        rows(&mut locked, VoteKind::Prevote, Some(h.content.id));
        rows(&mut locked, VoteKind::Precommit, None);
        assert_eq!(locked.d().step, Step::Precommit);
        assert!(locked.d().vc.is_some() && locked.d().timers[3].active);
        let mut decided = clone_engine(&locked);
        rows(&mut decided, VoteKind::Precommit, Some(h.content.id));
        assert_eq!(decided.d().step, Step::Decided);
        vec![
            pre, propose, prevote, fallback, precommit, retry, pre_retry, locked, decided,
        ]
    }

    #[test]
    fn differential_all_steps_grid_phases_expiries_and_recovery() {
        for rho in [3, 10, 17] {
            let c = config([1; 4]);
            let c = Config::new(
                c.domain(),
                c.members().to_vec(),
                c.max_body(),
                rho,
                c.timeouts(),
            )
            .unwrap();
            for (scenario, base) in snapshots(&c).iter().enumerate() {
                for grid in [0, 1, rho - 1, rho] {
                    let mut base = clone_engine(base);
                    base.store.descriptor.as_mut().unwrap().grid_remaining = grid;
                    // Zero is the legitimate crash cut between a reference tick's
                    // decrement publication and its grid-reset publication.
                    if grid == 0 {
                        base = Engine::recover_with_emission(
                            base.store,
                            base.key,
                            Emission::Guided,
                            &mut Events::default(),
                        )
                        .unwrap();
                    }
                    for online in [0, 1, 5, 9, 10, 11, 29, 30, 31, 37, 101] {
                        let result = std::panic::catch_unwind(|| compare_elapsed(&base, online));
                        assert!(
                            result.is_ok(),
                            "rho={rho} scenario={scenario} grid={grid} online={online}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn differential_multiple_rounds_heights_and_configurations() {
        let c = config([1, 7, 1, 1]);
        let (base, _) = engine(&c, 1, 4, 7);
        compare_elapsed(&base, 1_001); // Heavy own nil quorums advance many rounds.
        let mut base = clone_engine(&base);
        base.elapse_coalesced(3, &mut Events::default()).unwrap();
        let next = Config::new([99; 32], c.members()[2..].to_vec(), 512, 7, c.timeouts()).unwrap();
        base.init(&next, 5, 11, &mut Events::default()).unwrap();
        assert_eq!(base.local_index(), None);
        assert_eq!(base.d().grid_remaining, 7);
        compare_elapsed(&base, 113);
        let next =
            Config::crosslink([100; 32], c.members().to_vec(), 1024, 13, c.timeouts()).unwrap();
        base.init(&next, 8, 0, &mut Events::default()).unwrap();
        assert_eq!(base.local_index(), Some(1));
        compare_elapsed(&base, 1_003);
    }

    #[test]
    fn full_elapsed_and_write_bound_independent_of_missed_grid_ticks() {
        let c = config([1; 4]);
        let c = Config::new(
            c.domain(),
            c.members().to_vec(),
            c.max_body(),
            1,
            c.timeouts(),
        )
        .unwrap();
        let states = snapshots(&c);
        for index in [2, 8] {
            // Stable Prevote and Decided have no future deadline.
            for online in [1, 10_003, u64::MAX] {
                let mut e = clone_engine(&states[index]);
                let before = e.store.writes;
                e.elapse_coalesced(online, &mut Events::default()).unwrap();
                assert_eq!(e.store.writes - before, 1);
                assert_eq!(e.d(), states[index].d());
            }
        }
        let mut costs = Vec::new();
        for online in [31, 10_003, u64::MAX] {
            let mut e = clone_engine(&states[1]);
            let before = e.store.writes;
            e.elapse_coalesced(online, &mut Events::default()).unwrap();
            assert_eq!(e.d().step, Step::Prevote);
            assert_eq!(e.d().timers[0].remaining, 0);
            assert_eq!(e.d().grid_remaining, 1);
            costs.push(e.store.writes - before);
        }
        assert!(costs[0] <= 6);
        assert_eq!(costs, vec![costs[0]; 3]);
        let (mut pre, _) = engine(&c, 0, 4, u64::MAX);
        pre.set_emission(Emission::Guided);
        let before = pre.store.writes;
        pre.elapse_coalesced(u64::MAX - 1, &mut Events::default())
            .unwrap();
        assert_eq!(pre.store.writes - before, 1);
        assert_eq!(pre.d().height_remaining, 1);
        pre.elapse_coalesced(1, &mut Events::default()).unwrap();
        assert_eq!(pre.d().step, Step::Propose);
        assert_eq!(pre.d().timers[0].remaining, 30);
    }

    #[test]
    fn guarded_policy_and_extreme_grid_arithmetic() {
        let c = config([1; 4]);
        let (mut e, _) = engine(&c, 0, 4, 0);
        let before = e.store.writes;
        for emission in [Emission::Reference, Emission::CompactLive] {
            e.set_emission(emission);
            let mut out = Out::default();
            assert_eq!(e.elapse_coalesced(100, &mut out), Err(Error::Invalid));
            assert_eq!(e.store.writes, before);
            assert!(e.is_active() && out.sent.is_empty() && out.acquire == 0);
        }
        let c = Config::new(
            c.domain(),
            c.members().to_vec(),
            c.max_body(),
            u64::MAX,
            c.timeouts(),
        )
        .unwrap();
        let (base, _) = engine(&c, 0, 4, 37);
        compare_elapsed(&base, u64::MAX);
    }

    use super::super::durable::{DurableStore, Limits, image_size};
    use std::{cell::RefCell, rc::Rc};
    const LIMITS: Limits = Limits {
        max_members: 4,
        max_body: 1024,
    };

    // Atomic old-or-new fault model; not a claim of physical fsync durability.
    #[derive(Clone, Default)]
    struct Disk(Rc<RefCell<DiskState>>);
    #[derive(Default)]
    struct DiskState {
        block: Option<Vec<u8>>,
        writes: usize,
        fail: Option<(usize, bool)>,
        failed: bool,
    }
    impl AtomicBlockStore for Disk {
        fn block_size(&self) -> usize {
            image_size(LIMITS).unwrap()
        }
        fn load(&mut self, block: &mut [u8]) -> Result<bool> {
            assert_eq!(block.len(), self.block_size());
            let state = self.0.borrow();
            if let Some(saved) = &state.block {
                block.copy_from_slice(saved);
                Ok(true)
            } else {
                Ok(false)
            }
        }
        fn replace(&mut self, block: &[u8]) -> Result<()> {
            assert_eq!(block.len(), self.block_size());
            let mut state = self.0.borrow_mut();
            let fail = state.fail.filter(|(at, _)| *at == state.writes);
            state.writes += 1;
            if fail.is_none_or(|(_, commit_new)| commit_new) {
                state.block = Some(block.to_vec());
            }
            state.failed = fail.is_some();
            if fail.is_some() {
                Err(Error::Storage)
            } else {
                Ok(())
            }
        }
    }
    struct Published {
        disk: Disk,
        calls: usize,
        votes: Vec<Vote>,
    }
    impl Published {
        fn load(&mut self) -> DurableStore<Disk> {
            assert!(
                !self.disk.0.borrow().failed,
                "callback after failed replacement"
            );
            self.calls += 1;
            DurableStore::open(self.disk.clone(), LIMITS).unwrap()
        }
    }
    impl Sink for Published {
        fn send(&mut self, _: Target, frame: Frame<'_>) {
            assert!(frame.proposal.is_none() && frame.groups.is_empty());
            let store = self.load();
            let d = store.descriptor().unwrap();
            assert!(
                d.grid_remaining > 0,
                "replay before normalized grid publication"
            );
            let vote = frame.outer.unwrap();
            assert_eq!(d.intents[vote.content.kind.index()], Some(vote.content));
            assert_eq!(store.own(vote.content.kind), Some(vote));
            if d.proposal_published {
                let intent = d.proposal_intent.unwrap();
                let header = store.own_header().unwrap();
                assert_eq!(header.content, intent.content);
                assert_eq!(store.header(intent.slot), Some(header));
                assert_eq!(
                    store.config().unwrap().body_id(store.body(intent.slot)),
                    header.content.id
                );
            }
            for region in [d.entry, d.vc.map(|p| p.certificate)].into_iter().flatten() {
                let header = store.region_header(region.index).unwrap();
                assert_eq!(header.digest, region.digest);
                assert_eq!(
                    store.region_rows(region.index).iter().flatten().count(),
                    header.count
                );
            }
            self.votes.push(*vote);
        }
        fn decided(&mut self, _: u64, proposal: Proposal<'_>, _: Group<'_>) {
            let store = self.load();
            let d = store.descriptor().unwrap();
            assert_eq!(d.step, Step::Decided);
            let slot = d.pending.unwrap().proposal.index;
            assert_eq!(store.header(slot), Some(proposal.header));
            assert_eq!(store.body(slot), proposal.body);
        }
        fn acquire(&mut self, tag: ProposalTag) {
            let store = self.load();
            let d = store.descriptor().unwrap();
            assert_eq!((d.height, d.round), (tag.height, tag.round));
        }
        fn retry(&mut self, tag: BodyTag, _: Proposal<'_>) {
            let store = self.load();
            let d = store.descriptor().unwrap();
            assert!(
                [d.slot, d.pending.map(|p| p.proposal)]
                    .into_iter()
                    .flatten()
                    .any(
                        |s| s.header == tag.header && d.body_generations[s.index] == tag.generation
                    )
            );
        }
    }
    fn durable_locked() -> (Engine<DurableStore<Disk>>, Published) {
        let c = config([1; 4]);
        let c = Config::new(
            c.domain(),
            c.members().to_vec(),
            c.max_body(),
            3,
            c.timeouts(),
        )
        .unwrap();
        let disk = Disk::default();
        let store = DurableStore::open(disk.clone(), LIMITS).unwrap();
        let mut e = Engine::boot(store, keys()[1].clone(), c.clone()).unwrap();
        e.set_emission(Emission::Guided);
        let mut out = Published {
            disk,
            calls: 0,
            votes: Vec::new(),
        };
        e.init(&c, 4, 0, &mut out).unwrap();
        let h = header(&c, 0, None, b"durable-lock");
        e.receive(
            ordinary(
                None,
                Some(Proposal {
                    header: &h,
                    body: b"durable-lock",
                }),
                &[],
            ),
            Status::Valid,
            &mut out,
        )
        .unwrap();
        for (kind, id) in [
            (VoteKind::Prevote, Some(h.content.id)),
            (VoteKind::Precommit, None),
        ] {
            let votes = (0..3).map(|s| vote(&c, s, 0, kind, id)).collect::<Vec<_>>();
            let groups = [Group {
                kind: CertKind::Row { round: 0, kind },
                votes: Votes::Dense(&votes),
            }];
            e.receive(
                ordinary(None, None, &groups),
                Status::Indeterminate,
                &mut out,
            )
            .unwrap();
        }
        assert_eq!(e.d().step, Step::Precommit);
        assert!(e.d().vc.is_some() && e.d().timers[3].active);
        out.calls = 0;
        out.votes.clear();
        (e, out)
    }

    #[test]
    fn durable_every_elapsed_cut_keeps_publication_and_fails_closed() {
        let (mut baseline, mut out) = durable_locked();
        let before = out.disk.0.borrow().writes;
        baseline.elapse_coalesced(37, &mut out).unwrap();
        let writes = out.disk.0.borrow().writes - before;
        assert!(writes > 10); // Timeout certificate, new ENTER, copied proposal/PV.
        assert_eq!(baseline.d().round, 1);
        assert!(baseline.d().proposal_published);
        for commit_new in [false, true] {
            for cut in 0..writes {
                let (mut e, mut out) = durable_locked();
                let before = out.disk.0.borrow().writes;
                out.disk.0.borrow_mut().fail = Some((before + cut, commit_new));
                assert_eq!(
                    e.elapse_coalesced(37, &mut out),
                    Err(Error::Storage),
                    "cut={cut}"
                );
                assert!(!e.is_active() && e.storage().is_poisoned());
                if cut == 0 {
                    assert_eq!(out.calls, 0);
                }
                let calls = out.calls;
                assert_eq!(e.elapse_coalesced(1, &mut out), Err(Error::Inactive));
                assert_eq!(out.calls, calls);
                out.disk.0.borrow_mut().fail = None;
                out.disk.0.borrow_mut().failed = false;
                let store = DurableStore::open(e.into_storage().into_backend(), LIMITS).unwrap();
                let mut e = Engine::recover_with_emission(
                    store,
                    keys()[1].clone(),
                    Emission::Guided,
                    &mut out,
                )
                .unwrap();
                e.elapse_coalesced(37, &mut out).unwrap();
                for v in &out.votes {
                    for after in &out.votes {
                        if (v.content.height, v.content.round, v.content.kind)
                            == (
                                after.content.height,
                                after.content.round,
                                after.content.kind,
                            )
                        {
                            assert_eq!(v, after, "double sign at cut={cut} new={commit_new}");
                        }
                    }
                }
            }
        }
        baseline.elapse_coalesced(6, &mut out).unwrap(); // Last positive timer.
        for online in [1, 10_003, u64::MAX] {
            let before = out.disk.0.borrow().writes;
            baseline.elapse_coalesced(online, &mut out).unwrap();
            assert_eq!(out.disk.0.borrow().writes - before, 1);
        }
    }
}
