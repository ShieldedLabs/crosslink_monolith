//! The D28 chain boundary uses the unchanged BFT block/vote/fat-pointer formats.
use ed25519_zebra::{Signature, SigningKey, VerificationKey, VerificationKeyBytes};
use tenderlink::d28::{
    chain::{MAX_ROUND, decision_fat_pointer},
    *,
};
use zcash_primitives::bft::{
    BftBlock, Blake3Hash, FatPointerToBftBlock, HashKeys, PubKeyID, Vote as ChainVote,
};

fn key(i: usize) -> SigningKey {
    SigningKey::from([u8::try_from(i + 1).unwrap(); 32])
}
fn config(namespace: Id, weights: &[u64]) -> Config {
    Config::crosslink(
        namespace,
        weights
            .iter()
            .enumerate()
            .map(|(i, &weight)| Member {
                public_key: VerificationKeyBytes::from(&key(i)).into(),
                weight,
            })
            .collect(),
        4096,
        u64::MAX / 2,
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
fn native(c: &Config) -> Config {
    Config::new(
        c.domain(),
        c.members().to_vec(),
        c.max_body(),
        c.rho(),
        c.timeouts(),
    )
    .unwrap()
}
fn block(version: u32, height: u32) -> BftBlock {
    BftBlock {
        version,
        height,
        previous_block_fat_ptr: FatPointerToBftBlock::null(),
        headers: vec![],
        slashforks: vec![],
        do_not_include_until_bc_height: 0,
    }
}
fn header(c: &Config, height: u64, round: Round, body: &[u8]) -> Header {
    let signer = c.proposer(height, round);
    Header::try_sign(
        c,
        &key(signer),
        ProposalContent {
            height,
            round,
            valid_round: None,
            id: c.body_id(body),
            signer: signer as u32,
        },
    )
    .unwrap()
}
fn vote(
    c: &Config,
    signer: usize,
    height: u64,
    round: Round,
    kind: VoteKind,
    id: Option<Id>,
) -> Vote {
    Vote::try_sign(
        c,
        &key(signer),
        VoteContent {
            height,
            round,
            kind,
            id,
            signer: signer as u32,
        },
    )
    .unwrap()
}
fn precommits(c: &Config, h: &Header, signers: &[usize]) -> Vec<Vote> {
    signers
        .iter()
        .map(|&signer| {
            vote(
                c,
                signer,
                h.content.height,
                h.content.round,
                VoteKind::Precommit,
                Some(h.content.id),
            )
        })
        .collect()
}
fn decision<'a>(h: &Header, votes: &'a [Vote]) -> Group<'a> {
    Group {
        kind: CertKind::Decision {
            round: h.content.round,
            id: h.content.id,
        },
        votes: Votes::Dense(votes),
    }
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
fn chain_votes_match_existing_bytes_parser_and_namespaces() {
    for namespace in [[0; 32], [91; 32]] {
        let c = config(namespace, &[1, 1, 1, 1]);
        for round in [0, 17, MAX_ROUND] {
            for kind in [VoteKind::Prevote, VoteKind::Precommit] {
                for id in [None, Some([73; 32])] {
                    let v = vote(&c, 2, 0x0102_0304_0506_0708, round, kind, id);
                    let canonical = ChainVote {
                        validator_address: PubKeyID(c.members()[2].public_key),
                        value: Blake3Hash(id.unwrap_or([0; 32])),
                        height: v.content.height,
                        typ: kind == VoteKind::Precommit,
                        round: i32::try_from(round).unwrap(),
                    };
                    let bytes = canonical.to_bytes();
                    assert_eq!(ChainVote::from_bytes(&bytes), canonical);
                    assert_eq!(&bytes[..32], &c.members()[2].public_key);
                    assert_eq!(&bytes[32..64], &id.unwrap_or([0; 32]));
                    assert_eq!(&bytes[64..72], &v.content.height.to_le_bytes());
                    let encoded_round = u32::try_from(round).unwrap()
                        | if kind == VoteKind::Precommit {
                            0x8000_0000
                        } else {
                            0
                        };
                    assert_eq!(&bytes[72..], &encoded_round.to_le_bytes());
                    let mut signed = bytes.to_vec();
                    if namespace != [0; 32] {
                        signed.extend_from_slice(&namespace);
                    }
                    assert_eq!(v.signature, key(2).sign(&signed).to_bytes());
                    assert!(v.verify(&c));
                    let vk = VerificationKey::try_from(VerificationKeyBytes::from(
                        c.members()[2].public_key,
                    ))
                    .unwrap();
                    assert!(vk.verify(&Signature::from(v.signature), &signed).is_ok());
                    assert!(!v.verify(&config([92; 32], &[1, 1, 1, 1])));
                }
            }
        }
    }
}

#[test]
fn modes_and_internal_signatures_are_separate() {
    let c = config([0; 32], &[1, 1, 1, 1]);
    let n = native(&c);
    let changed = config(c.domain(), &[4, 1, 1, 1]);
    assert!(c.is_crosslink());
    assert!(!n.is_crosslink());
    assert_ne!(c.hash(), n.hash());
    assert_ne!(c.hash(), changed.hash());
    assert_eq!(
        n.body_id(b"opaque native value"),
        body_id(b"opaque native value")
    );
    assert_ne!(
        c.body_id(b"opaque native value"),
        n.body_id(b"opaque native value")
    );
    for kind in [VoteKind::Enter, VoteKind::Prevote, VoteKind::Precommit] {
        let a = vote(&c, 1, 7, 2, kind, None);
        let b = vote(&n, 1, 7, 2, kind, None);
        assert!(a.verify(&c));
        assert!(b.verify(&n));
        assert!(!a.verify(&n));
        assert!(!b.verify(&c));
        // The existing chain contract cannot bind weights/config hashes. The
        // host's agreed chain-state provenance supplies that binding instead.
        assert_eq!(a.verify(&changed), kind != VoteKind::Enter);
    }
    let h = header(&c, 7, 2, b"header body");
    assert!(h.verify(&c));
    assert!(!h.verify(&n));
    assert!(!h.verify(&changed));
    let nzero = vote(&n, 0, 7, u64::MAX, VoteKind::Precommit, Some([0; 32]));
    assert!(nzero.verify(&n));
    assert!(!nzero.verify(&c));
}

#[test]
fn exported_decisions_validate_and_roundtrip_without_resigning_or_reordering() {
    for namespace in [[0; 32], [19; 32]] {
        let c = config(namespace, &[4, 2, 1, 1]);
        for version in [1, 2] {
            for height in [0, 7] {
                let b = block(version, height);
                let bytes = b.zcash_serialize_to_vec().unwrap();
                assert_eq!(c.body_id(&bytes), b.blake3_hash().0);
                assert_eq!(c.body_id(&bytes), HashKeys::default().value_id.hash(&bytes));
                for round in [0, 17, MAX_ROUND] {
                    let h = header(&c, u64::from(height), round, &bytes);
                    let votes = precommits(&c, &h, &[0, 2, 3]);
                    let p = Proposal {
                        header: &h,
                        body: &bytes,
                    };
                    let pointer =
                        decision_fat_pointer(&c, u64::from(height), p, decision(&h, &votes))
                            .unwrap();
                    assert!(pointer.validate_signatures(&namespace));
                    assert_eq!(pointer.points_at_block_hash(), b.blake3_hash());
                    // certificate_is_valid in zebra-state requires strictly
                    // increasing active-roster positions, not just valid sigs.
                    let positions: Vec<_> = pointer
                        .signatures
                        .iter()
                        .map(|signature| {
                            c.members()
                                .iter()
                                .position(|m| m.public_key == signature.pub_key.0)
                                .unwrap()
                        })
                        .collect();
                    assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
                    let template = pointer.get_vote_template();
                    assert!(template.typ);
                    assert_eq!(template.height, u64::from(height));
                    assert_eq!(u64::try_from(template.round).unwrap(), round);
                    for (signature, vote) in pointer.signatures.iter().zip(&votes) {
                        assert_eq!(
                            signature.pub_key.0,
                            c.members()[vote.content.signer as usize].public_key
                        );
                        assert_eq!(signature.vote_signature, vote.signature);
                    }
                    let mut serialized = Vec::new();
                    pointer.zcash_serialize(&mut serialized).unwrap();
                    assert_eq!(serialized, pointer.to_bytes());
                    assert_eq!(
                        FatPointerToBftBlock::try_from_bytes(&serialized),
                        Some(pointer.clone())
                    );
                    let sparse = [Some(votes[0]), None, Some(votes[1]), Some(votes[2])];
                    let g = Group {
                        kind: decision(&h, &votes).kind,
                        votes: Votes::Rows(&sparse),
                    };
                    assert_eq!(
                        decision_fat_pointer(&c, u64::from(height), p, g).unwrap(),
                        pointer
                    );
                    assert!(!pointer.validate_signatures(&[20; 32]));
                }
            }
        }
    }
}

#[test]
fn exporter_rejects_bad_certificates_and_noncanonical_or_wrong_height_bodies() {
    let c = config([12; 32], &[4, 2, 1, 1]);
    let bytes = block(2, 7).zcash_serialize_to_vec().unwrap();
    let h = header(&c, 7, 1, &bytes);
    let votes = precommits(&c, &h, &[0, 1]); // six of eight is a strict quorum
    let p = Proposal {
        header: &h,
        body: &bytes,
    };
    assert!(decision_fat_pointer(&c, 7, p, decision(&h, &votes)).is_ok());
    assert_eq!(
        decision_fat_pointer(&native(&c), 7, p, decision(&h, &votes)),
        Err(Error::Config)
    );
    assert_eq!(
        decision_fat_pointer(&c, 8, p, decision(&h, &votes)),
        Err(Error::Invalid)
    );
    for kind in [
        CertKind::Polka {
            round: 1,
            id: h.content.id,
        },
        CertKind::Timeout { round: 1 },
        CertKind::Decision {
            round: 2,
            id: h.content.id,
        },
        CertKind::Decision {
            round: 1,
            id: [13; 32],
        },
    ] {
        assert_eq!(
            decision_fat_pointer(
                &c,
                7,
                p,
                Group {
                    kind,
                    votes: Votes::Dense(&votes)
                }
            ),
            Err(Error::Invalid)
        );
    }
    for bad in [
        vec![],
        vec![votes[0]],
        vec![votes[1]],
        vec![votes[0], votes[0]],
        vec![votes[1], votes[0]], // genuine quorum, but chain-invalid order
    ] {
        assert_eq!(
            decision_fat_pointer(&c, 7, p, decision(&h, &bad)),
            Err(Error::Invalid)
        );
    }
    for field in 0..7 {
        let mut bad = votes.clone();
        match field {
            0 => bad[1].signature[0] ^= 1,
            1 => bad[1].content.signer = u32::MAX,
            2 => bad[1].content.height += 1,
            3 => bad[1].content.round += 1,
            4 => bad[1].content.kind = VoteKind::Prevote,
            5 => bad[1].content.id = None,
            _ => bad[1].content.id = Some([0; 32]),
        }
        assert_eq!(
            decision_fat_pointer(&c, 7, p, decision(&h, &bad)),
            Err(Error::Invalid)
        );
    }
    let mut bad_header = h;
    bad_header.signature[0] ^= 1;
    assert_eq!(
        decision_fat_pointer(
            &c,
            7,
            Proposal {
                header: &bad_header,
                body: &bytes
            },
            decision(&h, &votes)
        ),
        Err(Error::Invalid)
    );
    let mut noncanonical = bytes.clone();
    // The discarded legacy finalization-candidate field must canonically be 0.
    noncanonical[8 + 44 + 2] = 1;
    let mut trailing = bytes.clone();
    trailing.push(0);
    for bad_body in [
        noncanonical,
        trailing,
        vec![1, 2, 3],
        block(2, 8).zcash_serialize_to_vec().unwrap(),
    ] {
        let bad_header = header(&c, 7, 1, &bad_body);
        let bad_votes = precommits(&c, &bad_header, &[0, 1]);
        assert_eq!(
            decision_fat_pointer(
                &c,
                7,
                Proposal {
                    header: &bad_header,
                    body: &bad_body
                },
                decision(&bad_header, &bad_votes)
            ),
            Err(Error::Invalid)
        );
    }
    let mut mismatched = bytes.clone();
    *mismatched.last_mut().unwrap() ^= 1;
    assert_eq!(
        decision_fat_pointer(
            &c,
            7,
            Proposal {
                header: &h,
                body: &mismatched
            },
            decision(&h, &votes)
        ),
        Err(Error::Invalid)
    );
}

#[derive(Default)]
struct Recorder {
    votes: Vec<Vote>,
    decisions: Vec<FatPointerToBftBlock>,
    config: Option<Config>,
}
impl Sink for Recorder {
    fn send(&mut self, _: Target, f: Frame<'_>) {
        if let Some(v) = f.outer {
            self.votes.push(*v);
        }
    }
    fn decided(&mut self, height: u64, p: Proposal<'_>, g: Group<'_>) {
        self.decisions
            .push(decision_fat_pointer(self.config.as_ref().unwrap(), height, p, g).unwrap());
    }
}
fn engine(c: &Config, height: u64) -> (Engine<MemoryStore>, Recorder) {
    let mut e = Engine::boot(MemoryStore::default(), key(0), c.clone()).unwrap();
    let mut s = Recorder {
        config: Some(c.clone()),
        ..Default::default()
    };
    e.init(c, height, 0, &mut s).unwrap();
    (e, s)
}

#[test]
fn bounds_and_nil_alias_fail_before_signing_and_invalid_peers_do_not_panic() {
    let c = config([3; 32], &[1, 1, 1, 1]);
    let (mut e, mut s) = engine(&c, 0);
    for kind in [VoteKind::Enter, VoteKind::Prevote, VoteKind::Precommit] {
        let good = vote(&c, 1, 0, 0, kind, None);
        for round in [MAX_ROUND + 1, u64::from(u32::MAX), u64::MAX] {
            let content = VoteContent {
                round,
                ..good.content
            };
            assert_eq!(Vote::try_sign(&c, &key(1), content), Err(Error::Exhausted));
            let bad = Vote { content, ..good };
            assert!(!bad.verify(&c));
            e.receive(
                ordinary(Some(&bad), None, &[]),
                Status::Indeterminate,
                &mut s,
            )
            .unwrap();
            assert_eq!(e.view().unwrap().round, 0);
        }
        for content in [
            VoteContent {
                id: Some([0; 32]),
                ..good.content
            },
            VoteContent {
                signer: u32::MAX,
                ..good.content
            },
        ] {
            assert_eq!(Vote::try_sign(&c, &key(1), content), Err(Error::Invalid));
            let bad = Vote { content, ..good };
            assert!(!bad.verify(&c));
            e.receive(
                ordinary(Some(&bad), None, &[]),
                Status::Indeterminate,
                &mut s,
            )
            .unwrap();
        }
        assert_eq!(
            Vote::try_sign(&c, &key(2), good.content),
            Err(Error::Invalid)
        );
    }
    let bytes = block(2, 0).zcash_serialize_to_vec().unwrap();
    let h = header(&c, 0, 0, &bytes);
    for content in [
        ProposalContent {
            round: MAX_ROUND + 1,
            ..h.content
        },
        ProposalContent {
            valid_round: Some(MAX_ROUND + 1),
            ..h.content
        },
    ] {
        assert_eq!(
            Header::try_sign(&c, &key(0), content),
            Err(Error::Exhausted)
        );
        let bad = Header { content, ..h };
        assert!(!bad.verify(&c));
        assert_eq!(
            e.receive(
                ordinary(
                    None,
                    Some(Proposal {
                        header: &bad,
                        body: &bytes
                    }),
                    &[]
                ),
                Status::Valid,
                &mut s
            ),
            Err(Error::Invalid)
        );
    }
    let zero = ProposalContent {
        id: [0; 32],
        ..h.content
    };
    assert_eq!(Header::try_sign(&c, &key(0), zero), Err(Error::Invalid));
    let wrong_hash = Header::try_sign(
        &c,
        &key(0),
        ProposalContent {
            id: body_id(&bytes),
            ..h.content
        },
    )
    .unwrap();
    assert_eq!(
        e.receive(
            ordinary(
                None,
                Some(Proposal {
                    header: &wrong_hash,
                    body: &bytes
                }),
                &[]
            ),
            Status::Valid,
            &mut s
        ),
        Err(Error::Invalid)
    );
}

#[test]
fn maximum_round_exhaustion_never_signs_or_publishes_a_truncated_round() {
    let c = config([8; 32], &[1, 1, 1, 1]);
    let (mut e, mut s) = engine(&c, 0);
    let enters = [
        vote(&c, 1, 0, MAX_ROUND, VoteKind::Enter, None),
        vote(&c, 2, 0, MAX_ROUND, VoteKind::Enter, None),
    ];
    let groups = [Group {
        kind: CertKind::Jump { round: MAX_ROUND },
        votes: Votes::Dense(&enters),
    }];
    e.receive(ordinary(None, None, &groups), Status::Indeterminate, &mut s)
        .unwrap();
    assert_eq!(e.view().unwrap().round, MAX_ROUND);
    let pcs: Vec<_> = (1..4)
        .map(|i| vote(&c, i, 0, MAX_ROUND, VoteKind::Precommit, None))
        .collect();
    let groups = [Group {
        kind: CertKind::Timeout { round: MAX_ROUND },
        votes: Votes::Dense(&pcs),
    }];
    e.receive(ordinary(None, None, &groups), Status::Indeterminate, &mut s)
        .unwrap();
    let before = s.votes.clone();
    let duration = c.timeouts().precommit + c.timeouts().inc_precommit * MAX_ROUND;
    assert_eq!(e.elapse(duration, &mut s), Err(Error::Exhausted));
    assert_eq!(e.view().unwrap().round, MAX_ROUND);
    assert_eq!(s.votes, before);
    assert!(
        s.votes
            .iter()
            .all(|v| v.content.round <= MAX_ROUND && v.verify(&c))
    );
    assert_eq!(e.elapse(0, &mut s), Err(Error::Inactive));
}

#[test]
fn engine_decisions_and_recovery_use_keyed_bodies_and_original_signatures() {
    for namespace in [[0; 32], [32; 32]] {
        let c = config(namespace, &[1]);
        let (mut e, mut s) = engine(&c, 7);
        let bytes = block(2, 7).zcash_serialize_to_vec().unwrap();
        let before = e.storage().clone();
        let first_write = before.writes;
        e.supply_proposal(e.proposal_tag().unwrap(), &bytes, Status::Valid, &mut s)
            .unwrap();
        assert_eq!(e.view().unwrap().step, Step::Decided);
        assert_eq!(s.decisions.len(), 1);
        let pointer = s.decisions[0].clone();
        assert!(pointer.validate_signatures(&namespace));
        let last_write = e.storage().writes;
        let mut out = Recorder {
            config: Some(c.clone()),
            ..Default::default()
        };
        let recovered = Engine::recover(e.into_storage(), key(0), &mut out).unwrap();
        assert!(recovered.storage().config().unwrap().is_crosslink());
        assert_eq!(out.decisions, [pointer.clone()]);
        assert!(
            out.votes.is_empty(),
            "decided recovery must not sign or emit new votes"
        );
        assert_eq!(recovered.storage().writes, last_write);
        // Every crash boundary in local proposal publication/decision must
        // recover with the contextual hash, including unfinished header intent.
        let mut probe_sink = Recorder {
            config: Some(c.clone()),
            ..Default::default()
        };
        let probe = Engine::recover(before.clone(), key(0), &mut probe_sink).unwrap();
        let recovery_writes = probe.storage().writes - first_write;
        for cut in first_write..last_write {
            let mut store = before.clone();
            store.fail_at = Some(cut + recovery_writes);
            let mut sink = Recorder {
                config: Some(c.clone()),
                ..Default::default()
            };
            let mut e = Engine::recover(store, key(0), &mut sink).unwrap();
            assert_eq!(
                e.supply_proposal(e.proposal_tag().unwrap(), &bytes, Status::Valid, &mut sink),
                Err(Error::Storage)
            );
            let mut store = e.into_storage();
            store.clear_fault();
            let mut sink = Recorder {
                config: Some(c.clone()),
                ..Default::default()
            };
            let mut e = Engine::recover(store, key(0), &mut sink).unwrap();
            if let Some(tag) = e.proposal_tag() {
                e.supply_proposal(tag, &bytes, Status::Valid, &mut sink)
                    .unwrap();
            }
            assert_eq!(e.view().unwrap().step, Step::Decided, "cut {cut}");
            assert_eq!(sink.decisions, [pointer.clone()], "cut {cut}");
        }
    }
}

#[test]
fn configuration_banks_rebuild_the_mode_and_recovery_rejects_invalid_rounds() {
    let c = config([0; 32], &[1]);
    let (mut e, mut s) = engine(&c, 0);
    for (height, next) in [native(&c), config([1; 32], &[2, 1]), config([2; 32], &[1])]
        .into_iter()
        .enumerate()
    {
        e.init(&next, height as u64 + 1, 100, &mut s).unwrap();
        assert_eq!(e.storage().config(), Some(&next));
        e = Engine::recover(e.into_storage(), key(0), &mut s).unwrap();
        assert_eq!(
            e.storage().config().unwrap().is_crosslink(),
            next.is_crosslink()
        );
        assert_eq!(e.view().unwrap().config, next.hash());
    }
    let mut store = e.into_storage();
    let mut d = store.descriptor().unwrap().clone();
    d.round = MAX_ROUND + 1;
    store.write_descriptor(&d).unwrap();
    let mut out = Recorder::default();
    assert!(matches!(
        Engine::recover(store, key(0), &mut out),
        Err(Error::Corrupt)
    ));
    assert!(out.votes.is_empty());
    assert!(out.decisions.is_empty());
}

#[test]
fn received_chain_decision_retains_keyed_body_across_pending_recovery() {
    let c = config([43; 32], &[1, 1, 1, 1]);
    let (mut e, mut sink) = engine(&c, 7);
    let bytes = block(2, 7).zcash_serialize_to_vec().unwrap();
    let h = header(&c, 7, 9, &bytes);
    let votes = precommits(&c, &h, &[0, 1, 2]);
    let groups = [decision(&h, &votes)];
    let frame = Frame {
        message: Message::Dec,
        outer: None,
        proposal: Some(Proposal {
            header: &h,
            body: &bytes,
        }),
        groups: &groups,
    };
    e.receive(frame, Status::Indeterminate, &mut sink).unwrap();
    assert!(e.view().unwrap().pending.is_some());
    assert!(sink.decisions.is_empty());
    let mut out = Recorder {
        config: Some(c.clone()),
        ..Default::default()
    };
    let mut e = Engine::recover(e.into_storage(), key(0), &mut out).unwrap();
    assert!(out.decisions.is_empty());
    e.receive(frame, Status::Valid, &mut out).unwrap();
    assert_eq!(e.view().unwrap().step, Step::Decided);
    assert_eq!(out.decisions.len(), 1);
    let pointer = &out.decisions[0];
    assert!(pointer.validate_signatures(&c.domain()));
    assert_eq!(pointer.points_at_block_hash().0, c.body_id(&bytes));
    assert_eq!(
        pointer
            .signatures
            .iter()
            .map(|s| s.vote_signature)
            .collect::<Vec<_>>(),
        votes.iter().map(|v| v.signature).collect::<Vec<_>>()
    );
}
