//! The wire boundary checks resources and canonical structure, not consensus.
use ed25519_zebra::{SigningKey, VerificationKeyBytes};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use tenderlink::d28::gossip::StatusSnapshot;
use tenderlink::d28::wire::*;
use tenderlink::d28::*;

struct CountingAllocator;
thread_local! {
    // Thread-local counting keeps parallel test allocations out of the result.
    static ALLOCATIONS: Cell<Option<(usize, usize)>> = const { Cell::new(None) };
}
fn record_allocation(size: usize) {
    let _ = ALLOCATIONS.try_with(|stats| {
        if let Some((count, bytes)) = stats.get() {
            stats.set(Some((count + 1, bytes + size)));
        }
    });
}
// SAFETY: All operations are forwarded to System with their original arguments.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record_allocation(layout.size());
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record_allocation(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        record_allocation(size);
        unsafe { System.realloc(ptr, layout, size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}
#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn allocations<R>(f: impl FnOnce() -> R) -> (R, usize, usize) {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            ALLOCATIONS.with(|stats| stats.set(None));
        }
    }
    ALLOCATIONS.with(|stats| {
        assert!(stats.get().is_none());
        stats.set(Some((0, 0)));
    });
    let reset = Reset;
    let result = f();
    let (count, bytes) = ALLOCATIONS.with(|stats| stats.get().unwrap());
    drop(reset);
    (result, count, bytes)
}

const LIMITS: Limits = Limits {
    max_members: 4,
    max_body: 64,
};

fn vote(kind: VoteKind, id: Option<Id>) -> Vote {
    Vote {
        content: VoteContent {
            height: u64::MAX - 1,
            round: u64::MAX,
            kind,
            id,
            signer: u32::MAX,
        },
        signature: std::array::from_fn(|i| i as u8),
    }
}
fn header(valid_round: Option<u64>) -> Header {
    Header {
        content: ProposalContent {
            height: u64::MAX,
            round: 1 << 31,
            valid_round,
            id: std::array::from_fn(|i| 255 - i as u8),
            signer: u32::MAX - 1,
        },
        signature: std::array::from_fn(|i| 200 - i as u8),
    }
}
fn empty_frame() -> Frame<'static> {
    Frame {
        message: Message::Ordinary,
        outer: None,
        proposal: None,
        groups: &[],
    }
}
fn status() -> WireStatus {
    WireStatus {
        sequence: u64::MAX,
        request: None,
        snapshot: StatusSnapshot {
            config: std::array::from_fn(|i| (i * 7) as u8),
            height: u64::MAX - 1,
            round: 1 << 31,
            step: Step::PreRound,
            proposal: None,
            polka: None,
            prevote_quorum: false,
            justified: false,
        },
    }
}
fn roundtrip(frame: Frame<'_>, request: Option<u64>, limits: Limits) -> Vec<u8> {
    let bytes = encode_frame(frame, request, limits).unwrap();
    assert_eq!(&bytes[..2], &[FRAME_TAG, VERSION]);
    assert!(bytes.len() <= limits.max_frame_bytes().unwrap());
    let decoded = decode_frame(&bytes, limits).unwrap();
    assert_eq!(decoded.request, request);
    decoded.with_frame(|actual| {
        assert_eq!(actual.message, frame.message);
        assert_eq!(actual.outer, frame.outer);
        match (actual.proposal, frame.proposal) {
            (Some(a), Some(b)) => {
                assert_eq!(a.header, b.header);
                assert_eq!(a.body, b.body);
                let offset = 14 + usize::from(frame.outer.is_some()) * 118 + 125 + 8;
                assert_eq!(a.body.as_ptr(), bytes[offset..].as_ptr());
            }
            (None, None) => {}
            _ => panic!("proposal presence changed"),
        }
        assert_eq!(actual.groups.len(), frame.groups.len());
        for (a, b) in actual.groups.iter().zip(frame.groups) {
            assert_eq!(a.kind, b.kind);
            assert_eq!(a.votes.len(), b.votes.len());
            assert!(matches!(a.votes, Votes::Dense(_)));
            assert!(a.votes.iter().eq(b.votes.iter()));
        }
        assert_eq!(
            encode_frame(actual, decoded.request, limits).unwrap(),
            bytes
        );
    });
    bytes
}
fn rejected_frame(bytes: &[u8], limits: Limits) {
    let (result, count, _) = allocations(|| decode_frame(bytes, limits));
    assert!(result.is_err(), "malformed frame accepted: {bytes:?}");
    assert_eq!(count, 0, "rejected frame allocated before admission");
}
fn rejected_encode(frame: Frame<'_>, limits: Limits) {
    let (result, count, _) = allocations(|| encode_frame(frame, None, limits));
    assert!(result.is_err());
    assert_eq!(count, 0, "rejected encoding allocated before admission");
}
fn rejected_status(bytes: &[u8]) {
    let (result, count, _) = allocations(|| decode_status(bytes));
    assert!(result.is_err(), "malformed status accepted");
    assert_eq!(count, 0);
}

#[test]
fn status_roundtrips_exact_hash_steps_flags_and_optional_zero_values() {
    assert_ne!(HELLO_TAG, STATUS_TAG);
    assert_ne!(STATUS_TAG, FRAME_TAG);
    for tag in [HELLO_TAG, STATUS_TAG, FRAME_TAG] {
        assert_eq!(tag & BFT_FAMILY, BFT_FAMILY);
    }
    for step in [
        Step::PreRound,
        Step::Propose,
        Step::Prevote,
        Step::Precommit,
        Step::Decided,
    ] {
        for request in [None, Some(0), Some(u64::MAX)] {
            for proposal in [None, Some([0; 32]), Some([255; 32])] {
                for polka in [None, Some((0, [0; 32])), Some((u64::MAX, [255; 32]))] {
                    for prevote_quorum in [false, true] {
                        for justified in [false, true] {
                            let mut s = status();
                            s.request = request;
                            s.snapshot = StatusSnapshot {
                                step,
                                proposal,
                                polka,
                                prevote_quorum,
                                justified,
                                ..s.snapshot
                            };
                            let bytes = encode_status(s).unwrap();
                            assert_eq!(bytes.len(), STATUS_BYTES);
                            assert_eq!(&bytes[..2], &[STATUS_TAG, VERSION]);
                            assert_eq!(&bytes[19..51], &s.snapshot.config);
                            assert_eq!(decode_status(&bytes).unwrap(), s);
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn all_messages_votes_and_certificates_roundtrip_without_semantic_rewriting() {
    let kinds = [
        CertKind::Polka {
            round: u64::MAX,
            id: [0; 32],
        },
        CertKind::Decision {
            round: 1 << 31,
            id: [255; 32],
        },
        CertKind::Timeout { round: 0 },
        CertKind::Jump {
            round: u64::MAX - 1,
        },
        CertKind::Row {
            round: 17,
            kind: VoteKind::Enter,
        },
        CertKind::Row {
            round: 18,
            kind: VoteKind::Prevote,
        },
        CertKind::Row {
            round: 19,
            kind: VoteKind::Precommit,
        },
    ];
    for message in [Message::Ordinary, Message::Dec] {
        for kind in kinds {
            for vote_kind in [VoteKind::Enter, VoteKind::Prevote, VoteKind::Precommit] {
                for id in [None, Some([0; 32]), Some([255; 32])] {
                    let v = vote(vote_kind, id);
                    let votes = [v];
                    let groups = [Group {
                        kind,
                        votes: Votes::Dense(&votes),
                    }];
                    for valid_round in [None, Some(0), Some(u64::MAX)] {
                        let h = header(valid_round);
                        for body in [&[][..], &[0, 1, 2, 255][..]] {
                            roundtrip(
                                Frame {
                                    message,
                                    outer: Some(&v),
                                    proposal: Some(Proposal { header: &h, body }),
                                    groups: &groups,
                                },
                                Some(u64::MAX),
                                LIMITS,
                            );
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn bare_votes_empty_frames_and_empty_groups_remain_structural() {
    for message in [Message::Ordinary, Message::Dec] {
        roundtrip(
            Frame {
                message,
                ..empty_frame()
            },
            None,
            LIMITS,
        );
    }
    for kind in [VoteKind::Enter, VoteKind::Prevote, VoteKind::Precommit] {
        for id in [None, Some([0; 32])] {
            let v = vote(kind, id);
            roundtrip(
                Frame {
                    outer: Some(&v),
                    ..empty_frame()
                },
                Some(0),
                LIMITS,
            );
        }
    }
    let groups = [Group {
        kind: CertKind::Jump { round: 5 },
        votes: Votes::Dense(&[]),
    }];
    roundtrip(
        Frame {
            groups: &groups,
            ..empty_frame()
        },
        None,
        LIMITS,
    );
}

#[test]
fn sparse_local_rows_become_dense_and_keep_vote_order() {
    let rows = [
        None,
        Some(vote(VoteKind::Enter, None)),
        None,
        Some(vote(VoteKind::Precommit, Some([7; 32]))),
    ];
    let groups = [Group {
        kind: CertKind::Jump { round: 8 },
        votes: Votes::Rows(&rows),
    }];
    let bytes = roundtrip(
        Frame {
            groups: &groups,
            ..empty_frame()
        },
        None,
        LIMITS,
    );
    decode_frame(&bytes, LIMITS).unwrap().with_frame(|frame| {
        assert_eq!(frame.groups[0].votes.storage_len(), 2);
    });
}

#[test]
fn maximum_package_and_outer_position_budget_roundtrip() {
    let limits = Limits {
        max_members: 8,
        max_body: 64,
    };
    let v = vote(VoteKind::Prevote, Some([3; 32]));
    let votes = [v];
    let groups = vec![
        Group {
            kind: CertKind::Polka {
                round: 0,
                id: [3; 32]
            },
            votes: Votes::Dense(&votes),
        };
        limits.max_members + 1
    ];
    let h = header(Some(0));
    let body = vec![5; limits.max_body];
    let bytes = roundtrip(
        Frame {
            message: Message::Dec,
            outer: None,
            proposal: Some(Proposal {
                header: &h,
                body: &body,
            }),
            groups: &groups,
        },
        Some(1),
        limits,
    );
    assert_eq!(bytes.len(), limits.max_frame_bytes().unwrap());

    let full_row = vec![v; limits.max_members];
    let group = [Group {
        kind: CertKind::Timeout { round: 0 },
        votes: Votes::Dense(&full_row),
    }];
    roundtrip(
        Frame {
            outer: Some(&v),
            groups: &group,
            ..empty_frame()
        },
        None,
        limits,
    );
}

#[test]
fn encoding_admits_all_bounds_before_any_allocation() {
    let v = vote(VoteKind::Enter, None);
    let h = header(None);
    let body = [0; 65];
    rejected_encode(
        Frame {
            proposal: Some(Proposal {
                header: &h,
                body: &body,
            }),
            ..empty_frame()
        },
        LIMITS,
    );
    let empty_group = Group {
        kind: CertKind::Jump { round: 0 },
        votes: Votes::Dense(&[]),
    };
    let groups = [empty_group; 6];
    rejected_encode(
        Frame {
            groups: &groups,
            ..empty_frame()
        },
        LIMITS,
    );
    let too_many = [v; 5];
    let group = [Group {
        votes: Votes::Dense(&too_many),
        ..empty_group
    }];
    rejected_encode(
        Frame {
            groups: &group,
            ..empty_frame()
        },
        LIMITS,
    );
    let sparse_too_long = [None; 5];
    let group = [Group {
        votes: Votes::Rows(&sparse_too_long),
        ..empty_group
    }];
    rejected_encode(
        Frame {
            groups: &group,
            ..empty_frame()
        },
        LIMITS,
    );
    let row = [v; 4];
    let groups = [Group {
        votes: Votes::Dense(&row),
        ..empty_group
    }; 2];
    rejected_encode(
        Frame {
            groups: &groups,
            ..empty_frame()
        },
        LIMITS,
    );
    let single = [v];
    let groups = [Group {
        votes: Votes::Dense(&single),
        ..empty_group
    }; 5];
    rejected_encode(
        Frame {
            outer: Some(&v),
            groups: &groups,
            ..empty_frame()
        },
        LIMITS,
    );
}

#[test]
fn limit_and_count_overflow_fail_without_allocation() {
    for limits in [
        Limits {
            max_members: usize::MAX,
            max_body: 0,
        },
        Limits {
            max_members: usize::MAX / 2,
            max_body: 1,
        },
        Limits {
            max_members: 1,
            max_body: usize::MAX,
        },
        Limits {
            max_members: 1,
            max_body: isize::MAX as usize,
        },
    ] {
        assert_eq!(limits.max_frame_bytes(), Err(Error::Exhausted));
        rejected_encode(empty_frame(), limits);
        rejected_frame(&[], limits);
    }
    let mut frame = encode_frame(empty_frame(), None, LIMITS).unwrap();
    frame[14..22].copy_from_slice(&u64::MAX.to_le_bytes());
    rejected_frame(&frame, LIMITS);
    let group = [Group {
        kind: CertKind::Jump { round: 0 },
        votes: Votes::Dense(&[]),
    }];
    let mut frame = encode_frame(
        Frame {
            groups: &group,
            ..empty_frame()
        },
        None,
        LIMITS,
    )
    .unwrap();
    frame[64..72].copy_from_slice(&u64::MAX.to_le_bytes());
    rejected_frame(&frame, LIMITS);
    let h = header(None);
    let mut frame = encode_frame(
        Frame {
            proposal: Some(Proposal {
                header: &h,
                body: &[],
            }),
            ..empty_frame()
        },
        None,
        LIMITS,
    )
    .unwrap();
    frame[139..147].copy_from_slice(&u64::MAX.to_le_bytes());
    rejected_frame(&frame, LIMITS);
}

#[test]
fn decoding_checks_total_size_and_each_independent_budget() {
    rejected_frame(&vec![0; LIMITS.max_frame_bytes().unwrap() + 1], LIMITS);
    let large = Limits {
        max_members: 8,
        max_body: 128,
    };
    let v = vote(VoteKind::Enter, None);
    let h = header(None);
    let body = [0; 65];
    let bytes = encode_frame(
        Frame {
            proposal: Some(Proposal {
                header: &h,
                body: &body,
            }),
            ..empty_frame()
        },
        None,
        large,
    )
    .unwrap();
    rejected_frame(&bytes, LIMITS);
    let empty_group = Group {
        kind: CertKind::Jump { round: 0 },
        votes: Votes::Dense(&[]),
    };
    let groups = [empty_group; 6];
    let bytes = encode_frame(
        Frame {
            groups: &groups,
            ..empty_frame()
        },
        None,
        large,
    )
    .unwrap();
    rejected_frame(&bytes, LIMITS);
    let row = [v; 5];
    let group = [Group {
        votes: Votes::Dense(&row),
        ..empty_group
    }];
    let bytes = encode_frame(
        Frame {
            groups: &group,
            ..empty_frame()
        },
        None,
        large,
    )
    .unwrap();
    rejected_frame(&bytes, LIMITS);
    let row = [v];
    let groups = [Group {
        votes: Votes::Dense(&row),
        ..empty_group
    }; 5];
    let bytes = encode_frame(
        Frame {
            outer: Some(&v),
            groups: &groups,
            ..empty_frame()
        },
        None,
        large,
    )
    .unwrap();
    rejected_frame(&bytes, LIMITS);
    let rows = [v; 3];
    let groups = [Group {
        votes: Votes::Dense(&rows),
        ..empty_group
    }; 2];
    let bytes = encode_frame(
        Frame {
            groups: &groups,
            ..empty_frame()
        },
        None,
        large,
    )
    .unwrap();
    rejected_frame(&bytes, LIMITS);
}

#[test]
fn all_truncations_and_trailing_bytes_are_rejected_before_allocation() {
    let v = vote(VoteKind::Precommit, Some([5; 32]));
    let h = header(Some(9));
    let row = [v; 4];
    let groups = [Group {
        kind: CertKind::Decision {
            round: 10,
            id: [5; 32],
        },
        votes: Votes::Dense(&row),
    }];
    let mut bytes = encode_frame(
        Frame {
            message: Message::Dec,
            outer: Some(&v),
            proposal: Some(Proposal {
                header: &h,
                body: &[0; 64],
            }),
            groups: &groups,
        },
        Some(7),
        LIMITS,
    )
    .unwrap();
    for end in 0..bytes.len() {
        rejected_frame(&bytes[..end], LIMITS);
    }
    bytes.push(0);
    rejected_frame(&bytes, LIMITS);
    let mut bytes = encode_status(status()).unwrap();
    for end in 0..bytes.len() {
        rejected_status(&bytes[..end]);
    }
    bytes.push(0);
    rejected_status(&bytes);
}

#[test]
fn tags_options_booleans_and_unused_certificate_payloads_are_canonical() {
    let bytes = encode_frame(empty_frame(), None, LIMITS).unwrap();
    for (offset, value) in [
        (0, FRAME_TAG & !BFT_FAMILY),
        (0, HELLO_TAG),
        (0, STATUS_TAG),
        (1, VERSION + 1),
        (2, 2),
        (3, 1),
        (11, 2),
        (12, 2),
        (13, 2),
    ] {
        let mut bad = bytes.clone();
        bad[offset] = value;
        rejected_frame(&bad, LIMITS);
    }
    let v = vote(VoteKind::Enter, None);
    let bytes = encode_frame(
        Frame {
            outer: Some(&v),
            ..empty_frame()
        },
        None,
        LIMITS,
    )
    .unwrap();
    for (offset, value) in [(29, 3), (30, 2), (31, 1)] {
        let mut bad = bytes.clone();
        bad[offset] = value;
        rejected_frame(&bad, LIMITS);
    }
    let h = header(None);
    let bytes = encode_frame(
        Frame {
            proposal: Some(Proposal {
                header: &h,
                body: &[],
            }),
            ..empty_frame()
        },
        None,
        LIMITS,
    )
    .unwrap();
    for (offset, value) in [(30, 2), (31, 1)] {
        let mut bad = bytes.clone();
        bad[offset] = value;
        rejected_frame(&bad, LIMITS);
    }
    for kind in [
        CertKind::Polka {
            round: 3,
            id: [0; 32],
        },
        CertKind::Decision {
            round: 3,
            id: [0; 32],
        },
        CertKind::Timeout { round: 3 },
        CertKind::Jump { round: 3 },
        CertKind::Row {
            round: 3,
            kind: VoteKind::Prevote,
        },
    ] {
        let groups = [Group {
            kind,
            votes: Votes::Dense(&[]),
        }];
        let bytes = encode_frame(
            Frame {
                groups: &groups,
                ..empty_frame()
            },
            None,
            LIMITS,
        )
        .unwrap();
        for (offset, value) in [(22, 5), (63, 3)] {
            let mut bad = bytes.clone();
            bad[offset] = value;
            rejected_frame(&bad, LIMITS);
        }
        if !matches!(kind, CertKind::Row { .. }) {
            let mut bad = bytes.clone();
            bad[63] = 1;
            rejected_frame(&bad, LIMITS);
        }
        if !matches!(kind, CertKind::Polka { .. } | CertKind::Decision { .. }) {
            let mut bad = bytes.clone();
            bad[31] = 1;
            rejected_frame(&bad, LIMITS);
        }
    }
    let bytes = encode_status(status()).unwrap();
    for (offset, value) in [
        (0, STATUS_TAG & !BFT_FAMILY),
        (0, HELLO_TAG),
        (0, FRAME_TAG),
        (1, VERSION + 1),
        (10, 2),
        (11, 1),
        (67, 5),
        (68, 2),
        (69, 1),
        (101, 2),
        (102, 1),
        (110, 1),
        (142, 2),
        (143, 2),
    ] {
        let mut bad = bytes.clone();
        bad[offset] = value;
        rejected_status(&bad);
    }
}

#[test]
fn malformed_last_group_is_rejected_before_allocating_valid_prefix_metadata() {
    let votes = [vote(VoteKind::Enter, None)];
    let groups = [Group {
        kind: CertKind::Jump { round: 0 },
        votes: Votes::Dense(&votes),
    }; 4];
    let bytes = encode_frame(
        Frame {
            groups: &groups,
            ..empty_frame()
        },
        None,
        LIMITS,
    )
    .unwrap();
    // Three complete groups precede the final vote: each group occupies
    // 50 descriptor/count bytes and 118 vote bytes, following a 22-byte prefix.
    let final_vote = 22 + 3 * (50 + 118) + 50;
    for (offset, value) in [
        (final_vote + 16, 3),
        (final_vote + 17, 2),
        (final_vote + 18, 1),
    ] {
        let mut bad = bytes.clone();
        bad[offset] = value;
        rejected_frame(&bad, LIMITS);
    }
}

#[test]
fn dense_decode_backing_is_linear_and_does_not_copy_large_body() {
    for n in [4, 32, 256] {
        let limits = Limits {
            max_members: n,
            max_body: 1024 * 1024,
        };
        let v = [vote(VoteKind::Enter, None)];
        let groups = vec![
            Group {
                kind: CertKind::Jump { round: 5 },
                votes: Votes::Dense(&v)
            };
            n + 1
        ];
        let h = header(None);
        let body = vec![17; limits.max_body];
        let bytes = encode_frame(
            Frame {
                proposal: Some(Proposal {
                    header: &h,
                    body: &body,
                }),
                groups: &groups,
                ..empty_frame()
            },
            None,
            limits,
        )
        .unwrap();
        let (decoded, calls, allocated_bytes) = allocations(|| decode_frame(&bytes, limits));
        let decoded = decoded.unwrap();
        assert_eq!(
            calls, 2,
            "only dense votes and group metadata are allocated"
        );
        assert!(allocated_bytes <= 256 * (n + 1));
        let (_, calls, allocated_bytes) = allocations(|| {
            decoded.with_frame(|f| {
                assert_eq!(f.proposal.unwrap().body.as_ptr(), bytes[147..].as_ptr());
                assert_eq!(f.groups.len(), n + 1);
                assert!(f.groups.iter().all(|g| g.votes.storage_len() == 1));
            })
        });
        assert_eq!(calls, 1, "only the borrowed group view is allocated");
        assert!(allocated_bytes <= 128 * (n + 1));
    }
}

fn config() -> Config {
    Config::new(
        [42; 32],
        (0..4)
            .map(|i| Member {
                public_key: VerificationKeyBytes::from(&SigningKey::from([i + 1; 32])).into(),
                weight: 1,
            })
            .collect(),
        64,
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
#[derive(Default)]
struct SinkCount {
    decisions: usize,
}
impl Sink for SinkCount {
    fn send(&mut self, _target: Target, _frame: Frame<'_>) {}
    fn decided(&mut self, _height: u64, _proposal: Proposal<'_>, _certificate: Group<'_>) {
        self.decisions += 1;
    }
}

#[test]
fn signatures_survive_but_duplicate_signers_and_bad_signatures_remain_core_checks() {
    let c = config();
    let body = b"exact signed proposal";
    let round = 2;
    let signer = c.proposer(0, round) as u32;
    let h = Header::sign(
        &c,
        &SigningKey::from([signer as u8 + 1; 32]),
        ProposalContent {
            height: 0,
            round,
            valid_round: None,
            id: body_id(body),
            signer,
        },
    );
    let votes: Vec<_> = (0..4)
        .map(|signer| {
            Vote::sign(
                &c,
                &SigningKey::from([signer as u8 + 1; 32]),
                VoteContent {
                    height: 0,
                    round,
                    kind: VoteKind::Precommit,
                    id: Some(h.content.id),
                    signer,
                },
            )
        })
        .collect();
    let duplicated = vec![votes[0]; 4];
    let mut bad_signature = votes.clone();
    bad_signature[0].signature[0] ^= 1;
    for (row, valid) in [
        (&duplicated, false),
        (&bad_signature, false),
        (&votes, true),
    ] {
        let groups = [Group {
            kind: CertKind::Decision {
                round,
                id: h.content.id,
            },
            votes: Votes::Dense(row),
        }];
        let bytes = roundtrip(
            Frame {
                message: Message::Dec,
                outer: None,
                proposal: Some(Proposal { header: &h, body }),
                groups: &groups,
            },
            Some(9),
            LIMITS,
        );
        let decoded = decode_frame(&bytes, LIMITS).unwrap();
        let mut sink = SinkCount::default();
        let mut engine =
            Engine::boot(MemoryStore::default(), SigningKey::from([1; 32]), c.clone()).unwrap();
        engine.init(&c, 0, 0, &mut sink).unwrap();
        let result = decoded.with_frame(|frame| {
            assert!(frame.proposal.unwrap().header.verify(&c));
            if valid {
                assert!(frame.groups[0].votes.iter().all(|v| v.verify(&c)));
            }
            engine.receive(frame, Status::Valid, &mut sink)
        });
        if valid {
            assert_eq!(result, Ok(()));
            assert_eq!(sink.decisions, 1);
        } else {
            assert_eq!(result, Err(Error::Invalid));
            assert_eq!(sink.decisions, 0);
        }
    }
    let mut s = status();
    s.snapshot.config = c.hash();
    assert_eq!(
        decode_status(&encode_status(s).unwrap())
            .unwrap()
            .snapshot
            .config,
        c.hash()
    );
}

#[test]
fn deterministic_malformed_corpus_is_nonpanicking_and_accepted_bytes_are_canonical() {
    let v = [vote(VoteKind::Precommit, Some([3; 32]))];
    let h = header(Some(0));
    let groups = [Group {
        kind: CertKind::Decision {
            round: 1,
            id: [3; 32],
        },
        votes: Votes::Dense(&v),
    }];
    let seed_frame = encode_frame(
        Frame {
            message: Message::Dec,
            outer: Some(&v[0]),
            proposal: Some(Proposal {
                header: &h,
                body: &[1, 2, 3],
            }),
            groups: &groups,
        },
        Some(0),
        LIMITS,
    )
    .unwrap();
    let seed_status = encode_status(status()).unwrap();
    let mut random = 0x33b0_b1b2_77ab_98d1u64;
    let mut next = || {
        random ^= random << 13;
        random ^= random >> 7;
        random ^= random << 17;
        random
    };
    for iteration in 0..4000 {
        let mut bytes = if iteration % 3 == 0 {
            seed_frame.clone()
        } else if iteration % 3 == 1 {
            seed_status.clone()
        } else {
            vec![0; (next() as usize) % 1100]
        };
        for _ in 0..8 {
            if !bytes.is_empty() {
                let position = next() as usize % bytes.len();
                bytes[position] = next() as u8;
            }
        }
        let result = std::panic::catch_unwind(|| {
            if let Ok(decoded) = decode_frame(&bytes, LIMITS) {
                decoded.with_frame(|frame| {
                    assert_eq!(encode_frame(frame, decoded.request, LIMITS).unwrap(), bytes);
                });
            }
            if let Ok(decoded) = decode_status(&bytes) {
                assert_eq!(encode_status(decoded).unwrap(), bytes);
            }
        });
        assert!(
            result.is_ok(),
            "decoder panicked at corpus item {iteration}"
        );
    }
}
