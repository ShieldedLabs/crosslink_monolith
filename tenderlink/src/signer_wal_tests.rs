use super::*;

use std::{
    fs::OpenOptions,
    io::{Read, Seek, SeekFrom, Write},
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_TEST_ID: AtomicU64 = AtomicU64::new(0);

#[test]
fn weighted_quorum_is_n_minus_max_faulty_for_every_remainder() {
    assert_eq!(
        (0u64..=10).map(|n| quorum_threshold(n)).collect::<Vec<_>>(),
        vec![0, 1, 2, 3, 3, 4, 5, 5, 6, 7, 7],
    );
    assert_eq!(quorum_threshold(6), 5);
    assert!(2 * quorum_threshold(6) > 6 + ((6 - 1) / 3));
}

#[test]
fn canonical_roster_rejects_ambiguity_and_hashes_the_inactive_tail() {
    let mut keys: Vec<SigningKey> = (1u8..=102)
        .map(|seed| SigningKey::from([seed; 32]))
        .collect();
    keys.sort_by_key(|key| std::cmp::Reverse(PubKeyID(VerificationKeyBytes::from(key).into())));
    let mut cumulative_stake = 0u64;
    let roster: Vec<SortedRosterMember> = keys
        .iter()
        .take(101)
        .enumerate()
        .map(|(index, key)| {
            let stake = 101 - index as u64;
            cumulative_stake += stake;
            SortedRosterMember {
                pub_key: PubKeyID(VerificationKeyBytes::from(key).into()),
                stake,
                cumulative_stake,
            }
        })
        .collect();
    let original_hash = canonical_roster_hash(&roster).unwrap();
    assert_eq!(active_roster_len(&roster), 100);

    let mut duplicate = roster.clone();
    duplicate[1].pub_key = duplicate[0].pub_key;
    assert!(canonical_roster_hash(&duplicate).is_err());

    let mut bad_cumulative = roster.clone();
    bad_cumulative[50].cumulative_stake += 1;
    assert!(canonical_roster_hash(&bad_cumulative).is_err());

    let mut wrong_order = roster.clone();
    wrong_order.swap(0, 1);
    let mut cumulative = 0;
    for member in &mut wrong_order {
        cumulative += member.stake;
        member.cumulative_stake = cumulative;
    }
    assert!(canonical_roster_hash(&wrong_order).is_err());

    let mut changed_tail = roster;
    changed_tail[100].pub_key = PubKeyID(VerificationKeyBytes::from(&keys[101]).into());
    assert_ne!(canonical_roster_hash(&changed_tail).unwrap(), original_hash);
}

#[test]
fn a_new_lock_must_be_the_value_certified_at_its_round() {
    let old = LockValidTransition {
        locked_round: 6,
        locked_value_id: ValueId([1u8; 32]),
        locked_value: vec![1u8; 32],
        valid_round: 6,
        valid_value_id: ValueId([1u8; 32]),
        valid_value: vec![1u8; 32],
        certificate: vec![1u8; 32],
    };
    let uncertified_new_lock = LockValidTransition {
        locked_round: 7,
        locked_value_id: ValueId([2u8; 32]),
        locked_value: vec![2u8; 32],
        valid_round: 8,
        valid_value_id: ValueId([3u8; 32]),
        valid_value: vec![3u8; 32],
        certificate: vec![3u8; 32],
    };
    assert!(matches!(
        validate_transition(Some(&old), &uncertified_new_lock),
        Err(SignerError::Conflict(reason)) if reason.contains("not established")
    ));
}

#[test]
fn six_unit_stake_certificate_requires_five_yes_votes() {
    let mut keys: Vec<SigningKey> = (111u8..=116)
        .map(|seed| SigningKey::from([seed; 32]))
        .collect();
    keys.sort_by_key(|key| std::cmp::Reverse(PubKeyID(VerificationKeyBytes::from(key).into())));
    let mut cumulative_stake = 0;
    let roster: Vec<SortedRosterMember> = keys
        .iter()
        .map(|key| {
            cumulative_stake += 1;
            SortedRosterMember {
                pub_key: PubKeyID(VerificationKeyBytes::from(key).into()),
                stake: 1,
                cumulative_stake,
            }
        })
        .collect();
    let namespace = [117u8; 32];
    let proposal = BlockValue(vec![118u8; 128]);
    let proposal_id = proposal.id_from_value(&HashKeys::default());
    let mut round_data = RoundData {
        height: 0,
        round: 9,
        proposal,
        proposal_id,
        msg_val_sigs: vec![[(ValueId::NIL, TMSig::NIL); 2]; roster.len()],
        roster: roster.clone(),
        vote_namespace: namespace,
        ..RoundData::EMPTY
    };
    let epoch = SignerEpochBinding {
        public_key: roster[0].pub_key,
        chain_id: [119u8; 32],
        height: 0,
        parent_commit: [120u8; 32],
        vote_namespace: namespace,
        consensus_config_hash: [121u8; 32],
        roster_hash: canonical_roster_hash(&roster).unwrap(),
        roster_index: 0,
        active_roster_len: 6,
    };
    for roster_i in 0..4 {
        let signed = make_vote_sign_datas(
            roster[roster_i].pub_key,
            true,
            round_data.height,
            round_data.round,
            proposal_id,
        )[1];
        round_data.msg_val_sigs[roster_i][1] = (
            proposal_id,
            TMSig(sign_with_namespace(&keys[roster_i], &signed, &namespace)),
        );
    }
    let four_yes = canonical_precommit_certificate(&round_data, &roster).unwrap();
    assert!(verify_precommit_certificate(
        &four_yes,
        round_data.round,
        proposal_id,
        &epoch,
        &roster,
    )
    .is_err());

    let signed = make_vote_sign_datas(
        roster[4].pub_key,
        true,
        round_data.height,
        round_data.round,
        proposal_id,
    )[1];
    round_data.msg_val_sigs[4][1] = (
        proposal_id,
        TMSig(sign_with_namespace(&keys[4], &signed, &namespace)),
    );
    let five_yes = canonical_precommit_certificate(&round_data, &roster).unwrap();
    verify_precommit_certificate(&five_yes, round_data.round, proposal_id, &epoch, &roster)
        .unwrap();
}

struct TestPaths {
    dir: PathBuf,
    wal: PathBuf,
    anchor: PathBuf,
}

impl TestPaths {
    fn new(label: &str) -> Self {
        let id = NEXT_TEST_ID.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "tenderlink-signer-wal-{label}-{}-{id}",
            std::process::id(),
        ));
        std::fs::create_dir(&dir).unwrap();
        Self {
            wal: dir.join("signer.wal"),
            anchor: dir.join("signer.anchor"),
            dir,
        }
    }

    fn config(&self, authorized: bool) -> DurableSignerConfig {
        DurableSignerConfig {
            wal_path: self.wal.clone(),
            anchor_path: self.anchor.clone(),
            independent_anchor_authorized: authorized,
            non_genesis_bootstrap_receipt_hash: Some([0x42; 32]),
        }
    }
}

impl Drop for TestPaths {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn one_key_fixture(
    height: u64,
) -> (
    SigningKey,
    Vec<SortedRosterMember>,
    SignerEpochBinding,
    HashKeys,
) {
    let key = SigningKey::from([41u8; 32]);
    let pub_key = PubKeyID(VerificationKeyBytes::from(&key).into());
    let roster = vec![SortedRosterMember {
        pub_key,
        stake: 10,
        cumulative_stake: 10,
    }];
    let hash_keys = HashKeys::default();
    let epoch = SignerEpochBinding {
        public_key: pub_key,
        chain_id: [1u8; 32],
        height,
        parent_commit: [2u8; 32],
        vote_namespace: [3u8; 32],
        consensus_config_hash: [4u8; 32],
        roster_hash: canonical_roster_hash(&roster).unwrap(),
        roster_index: 0,
        active_roster_len: 1,
    };
    (key, roster, epoch, hash_keys)
}

fn vote_bytes(epoch: &SignerEpochBinding, round: u32, precommit: bool, value: ValueId) -> Vec<u8> {
    make_vote_sign_datas(epoch.public_key, precommit, epoch.height, round, value)[1].to_vec()
}

fn proposal_parts(
    epoch: &SignerEpochBinding,
    hash_keys: &HashKeys,
    round: u32,
    valid_round: i64,
    proposal: &[u8],
) -> (ValueId, Vec<Vec<u8>>) {
    let value = BlockValue(proposal.to_vec());
    let proposal_id = value.id_from_value(hash_keys);
    let mut header = PacketProposalChunkHeader {
        height: epoch.height,
        round,
        chunk_i: 0,
        proposal_size: proposal.len().try_into().unwrap(),
        proposal_id,
        valid_round,
    };
    let mut buffer = [0u8; 2048];
    let mut parts = Vec::new();
    for chunk_i in 0..value.chunks_n() {
        header.chunk_i = chunk_i.try_into().unwrap();
        let mut offset = header.write_to(&mut buffer);
        let (chunk_offset, chunk_size) = value.chunk_o_size(chunk_i);
        offset += proposal[chunk_offset..chunk_offset + chunk_size].write_to(&mut buffer[offset..]);
        parts.push(buffer[..offset].to_vec());
    }
    (proposal_id, parts)
}

#[test]
fn exact_vote_replay_is_stable_across_restart() {
    let paths = TestPaths::new("vote-replay");
    let (key, roster, epoch, hash_keys) = one_key_fixture(0);
    let value = ValueId([9u8; 32]);
    let bytes = vote_bytes(&epoch, 7, false, value);
    let expected = {
        let mut signer =
            DurableSigner::open(key.clone(), paths.config(true), epoch.clone()).unwrap();
        assert!(signer.is_active());
        let first = signer
            .sign_vote(&hash_keys, &roster, 7, false, value, &bytes, None)
            .unwrap();
        let replay = signer
            .sign_vote(&hash_keys, &roster, 7, false, value, &bytes, None)
            .unwrap();
        assert_eq!(first, replay);
        first
    };
    let mut reopened = DurableSigner::open(key, paths.config(true), epoch).unwrap();
    assert!(reopened.is_active());
    assert_eq!(
        reopened
            .sign_vote(&hash_keys, &roster, 7, false, value, &bytes, None)
            .unwrap(),
        expected,
    );
}

#[test]
fn conflicting_or_malformed_vote_poison_signing() {
    for malformed in [false, true] {
        let paths = TestPaths::new(if malformed {
            "vote-malformed"
        } else {
            "vote-conflict"
        });
        let (key, roster, epoch, hash_keys) = one_key_fixture(0);
        let first_value = ValueId([11u8; 32]);
        let first_bytes = vote_bytes(&epoch, 3, false, first_value);
        let mut signer = DurableSigner::open(key, paths.config(true), epoch.clone()).unwrap();
        signer
            .sign_vote(
                &hash_keys,
                &roster,
                3,
                false,
                first_value,
                &first_bytes,
                None,
            )
            .unwrap();

        let second_value = if malformed {
            first_value
        } else {
            ValueId([12u8; 32])
        };
        let mut second_bytes = vote_bytes(&epoch, 3, false, second_value);
        if malformed {
            second_bytes[0] ^= 1;
        }
        assert!(signer
            .sign_vote(
                &hash_keys,
                &roster,
                3,
                false,
                second_value,
                &second_bytes,
                None
            )
            .is_err());
        assert!(matches!(signer.status(), SignerStatus::Poisoned(_)));
    }
}

#[test]
fn proposal_manifest_is_exact_and_single_value_per_round() {
    let paths = TestPaths::new("proposal");
    let (key, _roster, epoch, hash_keys) = one_key_fixture(0);
    let proposal = vec![21u8; PROPOSAL_CHUNK_DATA_SIZE + 37];
    let (proposal_id, parts) = proposal_parts(&epoch, &hash_keys, 4, -1, &proposal);
    let mut signer = DurableSigner::open(key, paths.config(true), epoch.clone()).unwrap();
    let first = signer
        .sign_proposal(&hash_keys, &_roster, 4, -1, proposal_id, &proposal, &parts)
        .unwrap();
    assert_eq!(
        signer
            .sign_proposal(&hash_keys, &_roster, 4, -1, proposal_id, &proposal, &parts)
            .unwrap(),
        first,
    );

    let different = vec![22u8; PROPOSAL_CHUNK_DATA_SIZE + 37];
    let (different_id, different_parts) = proposal_parts(&epoch, &hash_keys, 4, -1, &different);
    assert!(signer
        .sign_proposal(
            &hash_keys,
            &_roster,
            4,
            -1,
            different_id,
            &different,
            &different_parts
        )
        .is_err());
    assert!(matches!(signer.status(), SignerStatus::Poisoned(_)));
}

#[test]
fn proposal_and_vote_reject_any_roster_outside_the_epoch_fingerprint() {
    let (key, roster, epoch, hash_keys) = one_key_fixture(0);
    let mut alternate = roster.clone();
    alternate[0].stake += 1;
    alternate[0].cumulative_stake += 1;
    assert_ne!(
        canonical_roster_hash(&alternate).unwrap(),
        epoch.roster_hash
    );

    let proposal_paths = TestPaths::new("proposal-roster-mismatch");
    let proposal = vec![23u8; 512];
    let (proposal_id, parts) = proposal_parts(&epoch, &hash_keys, 1, -1, &proposal);
    let mut proposal_signer =
        DurableSigner::open(key.clone(), proposal_paths.config(true), epoch.clone()).unwrap();
    assert!(proposal_signer
        .sign_proposal(
            &hash_keys,
            &alternate,
            1,
            -1,
            proposal_id,
            &proposal,
            &parts,
        )
        .is_err());
    assert!(matches!(
        proposal_signer.status(),
        SignerStatus::Poisoned(_)
    ));

    let vote_paths = TestPaths::new("vote-roster-mismatch");
    let value = ValueId([24u8; 32]);
    let bytes = vote_bytes(&epoch, 1, false, value);
    let mut vote_signer = DurableSigner::open(key, vote_paths.config(true), epoch).unwrap();
    assert!(vote_signer
        .sign_vote(&hash_keys, &alternate, 1, false, value, &bytes, None,)
        .is_err());
    assert!(matches!(vote_signer.status(), SignerStatus::Poisoned(_)));
}

#[test]
fn signer_index_must_resolve_to_the_epoch_public_key() {
    let paths = TestPaths::new("signer-index-mismatch");
    let (key, roster, mut epoch, hash_keys) = one_key_fixture(0);
    epoch.roster_index = 1;
    let value = ValueId([25u8; 32]);
    let bytes = vote_bytes(&epoch, 1, false, value);
    let mut signer = DurableSigner::open(key, paths.config(true), epoch).unwrap();
    assert!(signer
        .sign_vote(&hash_keys, &roster, 1, false, value, &bytes, None,)
        .is_err());
    assert!(!signer.is_active());
}

#[test]
fn non_genesis_without_receipt_or_unfenced_empty_history_is_observer_only() {
    for (height, authorized) in [(8, true), (0, false)] {
        let paths = TestPaths::new("unknown-history");
        let (key, _roster, epoch, _hash_keys) = one_key_fixture(height);
        let mut config = paths.config(authorized);
        if height > 0 {
            config.non_genesis_bootstrap_receipt_hash = None;
        }
        let signer = DurableSigner::open(key, config, epoch).unwrap();
        assert!(matches!(signer.status(), SignerStatus::ObserverOnly(_)));
        assert!(!paths.wal.exists(), "observer-only startup created a WAL");
        assert!(
            !paths.anchor.exists(),
            "observer-only startup created an anchor"
        );
    }
}

#[test]
fn non_genesis_bootstrap_requires_the_same_sealed_receipt_on_restart() {
    let paths = TestPaths::new("non-genesis-receipt");
    let (key, _roster, epoch, _hash_keys) = one_key_fixture(8);
    let receipt = [0x42; 32];
    let mut config = paths.config(true);
    config.non_genesis_bootstrap_receipt_hash = Some(receipt);
    let signer = DurableSigner::open(key.clone(), config, epoch.clone()).unwrap();
    assert!(signer.is_active());
    drop(signer);

    let mut wrong = paths.config(true);
    wrong.non_genesis_bootstrap_receipt_hash = Some([0x43; 32]);
    let signer = DurableSigner::open(key, wrong, epoch).unwrap();
    assert!(matches!(signer.status(), SignerStatus::ObserverOnly(_)));
}

#[test]
fn high_bit_round_is_rejected_before_encoding() {
    let paths = TestPaths::new("high-round");
    let (key, roster, epoch, hash_keys) = one_key_fixture(0);
    let mut signer = DurableSigner::open(key, paths.config(true), epoch).unwrap();
    assert!(signer
        .sign_vote(
            &hash_keys,
            &roster,
            0x8000_0000,
            false,
            ValueId([1u8; 32]),
            &[],
            None,
        )
        .is_err());
    assert!(matches!(signer.status(), SignerStatus::Poisoned(_)));
}

#[test]
fn a_second_process_cannot_open_the_same_signer_files() {
    let paths = TestPaths::new("exclusive-lock");
    let (key, _roster, epoch, _hash_keys) = one_key_fixture(0);
    let first = DurableSigner::open(key.clone(), paths.config(true), epoch.clone()).unwrap();
    assert!(first.is_active());
    assert!(DurableSigner::open(key, paths.config(true), epoch).is_err());
}

fn seed_vote(
    paths: &TestPaths,
) -> (
    SigningKey,
    Vec<SortedRosterMember>,
    SignerEpochBinding,
    HashKeys,
) {
    let (key, roster, epoch, hash_keys) = one_key_fixture(0);
    let value = ValueId([44u8; 32]);
    let bytes = vote_bytes(&epoch, 1, false, value);
    let mut signer = DurableSigner::open(key.clone(), paths.config(true), epoch.clone()).unwrap();
    signer
        .sign_vote(&hash_keys, &roster, 1, false, value, &bytes, None)
        .unwrap();
    drop(signer);
    (key, roster, epoch, hash_keys)
}

#[test]
fn torn_or_rolled_back_anchor_fails_closed() {
    for clean_frame_rollback in [false, true] {
        let paths = TestPaths::new("anchor-damage");
        let (key, _roster, epoch, _hash_keys) = seed_vote(&paths);
        let anchor_len = std::fs::metadata(&paths.anchor).unwrap().len();
        let new_len = if clean_frame_rollback {
            anchor_len - 116
        } else {
            anchor_len - 1
        };
        OpenOptions::new()
            .write(true)
            .open(&paths.anchor)
            .unwrap()
            .set_len(new_len)
            .unwrap();
        let reopened = DurableSigner::open(key, paths.config(true), epoch).unwrap();
        assert!(matches!(reopened.status(), SignerStatus::ObserverOnly(_)));
    }
}

#[test]
fn torn_wal_fails_closed_and_corruption_is_rejected() {
    for corrupt in [false, true] {
        let paths = TestPaths::new("wal-damage");
        let (key, _roster, epoch, _hash_keys) = seed_vote(&paths);
        let wal_len = std::fs::metadata(&paths.wal).unwrap().len();
        if corrupt {
            let mut file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&paths.wal)
                .unwrap();
            file.seek(SeekFrom::End(-1)).unwrap();
            let mut byte = [0u8; 1];
            file.read_exact(&mut byte).unwrap();
            byte[0] ^= 1;
            file.seek(SeekFrom::End(-1)).unwrap();
            file.write_all(&byte).unwrap();
            file.sync_all().unwrap();
            assert!(DurableSigner::open(key.clone(), paths.config(true), epoch.clone()).is_err());
            let observer = DurableSigner::open_or_observer(key, paths.config(true), epoch);
            assert!(
                matches!(observer.status(), SignerStatus::ObserverOnly(reason)
                if reason.contains("durable signer open failed"))
            );
        } else {
            OpenOptions::new()
                .write(true)
                .open(&paths.wal)
                .unwrap()
                .set_len(wal_len - 1)
                .unwrap();
            let reopened = DurableSigner::open(key, paths.config(true), epoch).unwrap();
            assert!(matches!(reopened.status(), SignerStatus::ObserverOnly(_)));
        }
    }
}

#[test]
fn injected_wal_or_anchor_failure_never_returns_a_signature() {
    for point in [
        WalFailpoint::AfterWalWrite,
        WalFailpoint::AfterWalSync,
        WalFailpoint::AfterAnchorWrite,
        WalFailpoint::AfterAnchorSync,
    ] {
        let paths = TestPaths::new("failpoint");
        let (key, roster, epoch, hash_keys) = one_key_fixture(0);
        let value = ValueId([55u8; 32]);
        let bytes = vote_bytes(&epoch, 2, false, value);
        let mut signer =
            DurableSigner::open(key.clone(), paths.config(true), epoch.clone()).unwrap();
        signer.set_failpoint(point);
        assert!(signer
            .sign_vote(&hash_keys, &roster, 2, false, value, &bytes, None)
            .is_err());
        assert!(matches!(signer.status(), SignerStatus::ObserverOnly(_)));
        drop(signer);

        let reopened = DurableSigner::open(key, paths.config(true), epoch).unwrap();
        match point {
            WalFailpoint::AfterWalWrite | WalFailpoint::AfterWalSync => {
                assert!(matches!(reopened.status(), SignerStatus::ObserverOnly(_)));
            }
            WalFailpoint::AfterAnchorWrite | WalFailpoint::AfterAnchorSync => {
                assert!(reopened.is_active());
                assert_eq!(reopened.replay_intents().len(), 1);
            }
            WalFailpoint::AfterCommitApplied => {
                unreachable!("commit-only failpoint is tested separately")
            }
        }
    }
}

fn quorum_fixture(
    precommit: bool,
) -> (
    Vec<SigningKey>,
    Vec<SortedRosterMember>,
    RoundData,
    SignerEpochBinding,
    HashKeys,
) {
    let mut keys: Vec<SigningKey> = (61u8..=64)
        .map(|seed| SigningKey::from([seed; 32]))
        .collect();
    keys.sort_by_key(|key| std::cmp::Reverse(PubKeyID(VerificationKeyBytes::from(key).into())));
    let mut cumulative_stake = 0;
    let roster: Vec<SortedRosterMember> = keys
        .iter()
        .map(|key| {
            cumulative_stake += 1;
            SortedRosterMember {
                pub_key: PubKeyID(VerificationKeyBytes::from(key).into()),
                stake: 1,
                cumulative_stake,
            }
        })
        .collect();
    let hash_keys = HashKeys::default();
    // Span multiple chunks so WAL recovery proves ordered-manifest survival,
    // not merely preservation of a single signature.
    let proposal = BlockValue(vec![88u8; PROPOSAL_CHUNK_DATA_SIZE * 2 + 17]);
    let proposal_id = proposal.id_from_value(&hash_keys);
    let namespace = [71u8; 32];
    let mut round_data = RoundData {
        height: 0,
        round: 6,
        proposal,
        proposal_id,
        proposal_valid_round: -1,
        msg_val_sigs: vec![[(ValueId::NIL, TMSig::NIL); 2]; roster.len()],
        roster: roster.clone(),
        vote_namespace: namespace,
        ..RoundData::EMPTY
    };
    let vote_i = usize::from(precommit);
    for roster_i in 0..3 {
        let signed = make_vote_sign_datas(
            roster[roster_i].pub_key,
            precommit,
            round_data.height,
            round_data.round,
            proposal_id,
        )[1];
        round_data.msg_val_sigs[roster_i][vote_i] = (
            proposal_id,
            TMSig(sign_with_namespace(&keys[roster_i], &signed, &namespace)),
        );
    }
    populate_proposal_manifest(&keys, &roster, &hash_keys, &mut round_data, -1);
    let epoch = SignerEpochBinding {
        public_key: roster[0].pub_key,
        chain_id: [72u8; 32],
        height: 0,
        parent_commit: [73u8; 32],
        vote_namespace: namespace,
        consensus_config_hash: [74u8; 32],
        roster_hash: canonical_roster_hash(&roster).unwrap(),
        roster_index: 0,
        active_roster_len: roster.len().try_into().unwrap(),
    };
    (keys, roster, round_data, epoch, hash_keys)
}

fn populate_proposal_manifest(
    keys: &[SigningKey],
    roster: &[SortedRosterMember],
    hash_keys: &HashKeys,
    round_data: &mut RoundData,
    valid_round: i64,
) {
    round_data.proposal_valid_round = valid_round;
    let (_, proposer) = TMState::proposer_from_height_round(
        hash_keys,
        roster,
        round_data.height,
        round_data.round,
    );
    let proposer_key = keys
        .iter()
        .find(|key| PubKeyID(VerificationKeyBytes::from(*key).into()) == proposer)
        .expect("fixture contains the selected proposer");
    let mut header = PacketProposalChunkHeader {
        height: round_data.height,
        round: round_data.round,
        chunk_i: 0,
        proposal_size: round_data.proposal.0.len().try_into().unwrap(),
        proposal_id: round_data.proposal_id,
        valid_round,
    };
    round_data.proposal_sigs.clear();
    for chunk_i in 0..round_data.proposal.chunks_n() {
        header.chunk_i = chunk_i.try_into().unwrap();
        let (chunk_offset, chunk_size) = round_data.proposal.chunk_o_size(chunk_i);
        let mut signable = vec![0u8; PacketProposalChunkHeader::SERIALIZED_SIZE + chunk_size];
        let header_len = header.write_to(&mut signable);
        signable[header_len..].copy_from_slice(
            &round_data.proposal.0[chunk_offset..chunk_offset + chunk_size],
        );
        round_data.proposal_sigs.push(TMSig(sign_with_namespace(
            proposer_key,
            &signable,
            &round_data.vote_namespace,
        )));
    }
    round_data.proposal_sigs_n = round_data.proposal_sigs.len();
}

#[test]
fn lock_and_valid_state_requires_a_fresh_exact_quorum_certificate() {
    let (keys, roster, round_data, epoch, hash_keys) = quorum_fixture(false);
    let certificate = canonical_prevote_certificate(&round_data, &roster).unwrap();
    let transition = LockValidTransition {
        locked_round: i64::from(round_data.round),
        locked_value_id: round_data.proposal_id,
        locked_value: round_data.proposal.0.clone(),
        valid_round: i64::from(round_data.round),
        valid_value_id: round_data.proposal_id,
        valid_value: round_data.proposal.0.clone(),
        certificate,
    };
    let paths = TestPaths::new("transition-ok");
    let bytes = vote_bytes(&epoch, round_data.round, true, round_data.proposal_id);
    let mut signer =
        DurableSigner::open(keys[0].clone(), paths.config(true), epoch.clone()).unwrap();
    signer
        .sign_vote(
            &hash_keys,
            &roster,
            round_data.round,
            true,
            round_data.proposal_id,
            &bytes,
            Some(transition.clone()),
        )
        .unwrap();
    assert_eq!(signer.durable_transition(), Some(&transition));
    drop(signer);
    let reopened = DurableSigner::open(keys[0].clone(), paths.config(true), epoch).unwrap();
    assert!(reopened.is_active());
    assert_eq!(reopened.durable_transition(), Some(&transition));

    let bad_paths = TestPaths::new("transition-bad");
    let mut bad = transition;
    *bad.certificate.last_mut().unwrap() ^= 1;
    let mut signer = DurableSigner::open(keys[0].clone(), bad_paths.config(true), {
        let (_, _, _, e, _) = quorum_fixture(false);
        e
    })
    .unwrap();
    assert!(signer
        .sign_vote(
            &hash_keys,
            &roster,
            round_data.round,
            true,
            round_data.proposal_id,
            &bytes,
            Some(bad),
        )
        .is_err());
    assert!(matches!(signer.status(), SignerStatus::Poisoned(_)));
}

#[test]
fn transition_certificate_rejects_high_bit_round_alias() {
    let (_, roster, round_data, epoch, hash_keys) = quorum_fixture(false);
    let mut certificate = canonical_prevote_certificate(&round_data, &roster).unwrap();
    let round_offset = b"tenderlink-prevote-qc-v1".len() + 8;
    certificate[round_offset..round_offset + 4]
        .copy_from_slice(&(MAX_CONSENSUS_ROUND + 1).to_le_bytes());
    let transition = LockValidTransition {
        locked_round: i64::from(MAX_CONSENSUS_ROUND) + 1,
        locked_value_id: round_data.proposal_id,
        locked_value: round_data.proposal.0.clone(),
        valid_round: i64::from(MAX_CONSENSUS_ROUND) + 1,
        valid_value_id: round_data.proposal_id,
        valid_value: round_data.proposal.0,
        certificate,
    };
    assert!(verify_transition_certificate(&transition, &epoch, &hash_keys, &roster).is_err());
}

#[test]
fn off_roster_observer_can_verify_a_valid_decision_certificate() {
    let (_, roster, round_data, mut epoch, _) = quorum_fixture(true);
    let certificate = canonical_precommit_certificate(&round_data, &roster).unwrap();
    epoch.public_key = PubKeyID([199u8; 32]);
    epoch.roster_index = u32::MAX;
    verify_precommit_certificate(
        &certificate,
        round_data.round,
        round_data.proposal_id,
        &epoch,
        &roster,
    )
    .unwrap();
}

#[test]
fn vote_step_and_value_require_the_exact_transition_shape() {
    let (keys, roster, round_data, epoch, hash_keys) = quorum_fixture(false);
    let certificate = canonical_prevote_certificate(&round_data, &roster).unwrap();
    let transition = LockValidTransition {
        locked_round: i64::from(round_data.round),
        locked_value_id: round_data.proposal_id,
        locked_value: round_data.proposal.0.clone(),
        valid_round: i64::from(round_data.round),
        valid_value_id: round_data.proposal_id,
        valid_value: round_data.proposal.0.clone(),
        certificate,
    };

    let missing_paths = TestPaths::new("precommit-missing-transition");
    let precommit = vote_bytes(&epoch, round_data.round, true, round_data.proposal_id);
    let mut missing =
        DurableSigner::open(keys[0].clone(), missing_paths.config(true), epoch.clone()).unwrap();
    assert!(missing
        .sign_vote(
            &hash_keys,
            &roster,
            round_data.round,
            true,
            round_data.proposal_id,
            &precommit,
            None,
        )
        .is_err());
    assert!(matches!(missing.status(), SignerStatus::Poisoned(_)));

    let prevote_paths = TestPaths::new("prevote-with-transition");
    let prevote = vote_bytes(&epoch, round_data.round, false, round_data.proposal_id);
    let mut prevote_signer =
        DurableSigner::open(keys[0].clone(), prevote_paths.config(true), epoch.clone()).unwrap();
    assert!(prevote_signer
        .sign_vote(
            &hash_keys,
            &roster,
            round_data.round,
            false,
            round_data.proposal_id,
            &prevote,
            Some(transition.clone()),
        )
        .is_err());
    assert!(matches!(prevote_signer.status(), SignerStatus::Poisoned(_)));

    let nil_paths = TestPaths::new("nil-precommit-with-transition");
    let nil_precommit = vote_bytes(&epoch, round_data.round, true, ValueId::NIL);
    let mut nil_signer =
        DurableSigner::open(keys[0].clone(), nil_paths.config(true), epoch).unwrap();
    assert!(nil_signer
        .sign_vote(
            &hash_keys,
            &roster,
            round_data.round,
            true,
            ValueId::NIL,
            &nil_precommit,
            Some(transition),
        )
        .is_err());
    assert!(matches!(nil_signer.status(), SignerStatus::Poisoned(_)));
}

#[test]
fn commit_intent_requires_quorum_and_incomplete_commit_resumes_only_exact_decision() {
    let (keys, roster, round_data, epoch, hash_keys) = quorum_fixture(true);
    let certificate = canonical_precommit_certificate(&round_data, &roster).unwrap();
    verify_precommit_certificate(
        &certificate,
        round_data.round,
        round_data.proposal_id,
        &epoch,
        &roster,
    )
    .unwrap();

    let pending_paths = TestPaths::new("commit-pending");
    let mut pending =
        DurableSigner::open(keys[0].clone(), pending_paths.config(true), epoch.clone()).unwrap();
    pending
        .begin_commit(
            &hash_keys,
            round_data.round,
            round_data.proposal_id,
            &round_data.proposal,
            round_data.proposal_valid_round,
            &round_data.proposal_sigs,
            &certificate,
            &roster,
        )
        .unwrap();
    drop(pending);
    let mut reopened =
        DurableSigner::open(keys[0].clone(), pending_paths.config(true), epoch.clone()).unwrap();
    assert!(matches!(
        reopened.status(),
        SignerStatus::ReconciliationRequired(_, _)
    ));
    let recovery = reopened
        .pending_commit_recovery(&hash_keys, &roster)
        .unwrap()
        .expect("pending commit must carry an exact local recovery value");
    assert_eq!(recovery.round_data.proposal, round_data.proposal);
    assert_eq!(
        recovery.round_data.proposal_valid_round,
        round_data.proposal_valid_round
    );
    assert_eq!(recovery.round_data.proposal_sigs, round_data.proposal_sigs);
    assert_eq!(
        recovery.fat_pointer,
        round_data_to_fat_pointer(&round_data, &roster)
    );
    let resumed_digest = reopened
        .begin_or_resume_commit(
            &hash_keys,
            round_data.round,
            round_data.proposal_id,
            &round_data.proposal,
            round_data.proposal_valid_round,
            &round_data.proposal_sigs,
            &certificate,
            &roster,
        )
        .unwrap()
        .expect("the exact pending commit must be resumable");
    reopened
        .complete_commit(
            resumed_digest,
            round_data.proposal_id.0,
            [93u8; 32],
            &roster,
        )
        .unwrap();
    assert!(reopened.is_active());
    assert_eq!(reopened.epoch().height, 1);

    let complete_paths = TestPaths::new("commit-complete");
    let mut complete =
        DurableSigner::open(keys[0].clone(), complete_paths.config(true), epoch).unwrap();
    let digest = complete
        .begin_commit(
            &hash_keys,
            round_data.round,
            round_data.proposal_id,
            &round_data.proposal,
            round_data.proposal_valid_round,
            &round_data.proposal_sigs,
            &certificate,
            &roster,
        )
        .unwrap();
    let durable_parent = round_data.proposal_id.0;
    let next_namespace = [92u8; 32];
    complete
        .complete_commit(digest, durable_parent, next_namespace, &roster)
        .unwrap();
    assert!(complete.is_active());
    assert_eq!(complete.epoch().height, 1);
    assert_eq!(complete.epoch().parent_commit, durable_parent);
    assert_eq!(complete.epoch().vote_namespace, next_namespace);
    let next_epoch = complete.epoch().clone();
    drop(complete);
    let reopened =
        DurableSigner::open(keys[0].clone(), complete_paths.config(true), next_epoch).unwrap();
    assert!(reopened.is_active());

    let unrelated_paths = TestPaths::new("commit-unrelated-readback");
    let (_, _, _, unrelated_epoch, _) = quorum_fixture(true);
    let mut unrelated = DurableSigner::open(
        keys[0].clone(),
        unrelated_paths.config(true),
        unrelated_epoch,
    )
    .unwrap();
    let unrelated_digest = unrelated
        .begin_commit(
            &hash_keys,
            round_data.round,
            round_data.proposal_id,
            &round_data.proposal,
            round_data.proposal_valid_round,
            &round_data.proposal_sigs,
            &certificate,
            &roster,
        )
        .unwrap();
    assert!(unrelated
        .complete_commit(unrelated_digest, [91u8; 32], next_namespace, &roster,)
        .is_err());
    assert!(matches!(unrelated.status(), SignerStatus::Poisoned(_)));

    let forged_paths = TestPaths::new("commit-forged");
    let (_, _, _, forged_epoch, _) = quorum_fixture(true);
    let mut forged =
        DurableSigner::open(keys[0].clone(), forged_paths.config(true), forged_epoch).unwrap();
    let mut bad_certificate = certificate;
    *bad_certificate.last_mut().unwrap() ^= 1;
    assert!(forged
        .begin_commit(
            &hash_keys,
            round_data.round,
            round_data.proposal_id,
            &round_data.proposal,
            round_data.proposal_valid_round,
            &round_data.proposal_sigs,
            &bad_certificate,
            &roster
        )
        .is_err());
    assert!(matches!(forged.status(), SignerStatus::Poisoned(_)));
}

#[test]
fn pending_commit_reconciles_from_local_wal_without_network_redelivery() {
    let (keys, roster, round_data, epoch, hash_keys) = quorum_fixture(true);
    let certificate = canonical_precommit_certificate(&round_data, &roster).unwrap();
    let paths = TestPaths::new("commit-local-recovery");
    let mut signer =
        DurableSigner::open(keys[0].clone(), paths.config(true), epoch.clone()).unwrap();
    signer
        .begin_commit(
            &hash_keys,
            round_data.round,
            round_data.proposal_id,
            &round_data.proposal,
            round_data.proposal_valid_round,
            &round_data.proposal_sigs,
            &certificate,
            &roster,
        )
        .unwrap();
    drop(signer);

    let signer = DurableSigner::open(keys[0].clone(), paths.config(true), epoch.clone()).unwrap();
    assert!(matches!(
        signer.status(),
        SignerStatus::ReconciliationRequired(_, _)
    ));
    let expected_proposal = round_data.proposal.clone();
    let expected_pointer = round_data_to_fat_pointer(&round_data, &roster);
    let expected_valid_round = round_data.proposal_valid_round;
    let expected_proposal_sigs = round_data.proposal_sigs.clone();
    let expected_parent = round_data.proposal_id.0;
    let next_namespace = [96u8; 32];
    let next_roster = roster.clone();
    let calls = Arc::new(AtomicU64::new(0));
    let calls_for_push = calls.clone();
    let push = ClosureToPushDecidedBlock(Arc::new(move |proposal, pointer, valid_round, proposal_sigs| {
        assert_eq!(proposal, expected_proposal);
        assert_eq!(pointer, expected_pointer);
        assert_eq!(valid_round, expected_valid_round);
        assert_eq!(proposal_sigs, expected_proposal_sigs);
        calls_for_push.fetch_add(1, Ordering::SeqCst);
        let next_roster = next_roster.clone();
        Box::pin(async move {
            Ok(DurableDecisionOutcome {
                next_roster,
                next_vote_namespace: next_namespace,
                durable_parent_commit: Some(expected_parent),
            })
        })
    }));
    let public_key = PubKeyID(VerificationKeyBytes::from(&keys[0]).into());
    let mut state = TMState::init(
        signer,
        public_key,
        3032,
        ClosureToProposeNewBlock(Arc::new(|| Box::pin(async { None }))),
        ClosureToValidateProposedBlock(Arc::new(|_| {
            Box::pin(async { (TMStatus::Pass, TMStatusReason::None) })
        })),
        push,
        ClosureToUpdatePeers(Arc::new(|_| Box::pin(async {}))),
        ClosureToAllowBftAccess(Arc::new(|_, _| Box::pin(async {}))),
    );
    state.hash_keys = hash_keys;
    state.height = epoch.height;
    state.vote_namespace = epoch.vote_namespace;
    let mut recovered_roster = roster;

    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(state.reconcile_pending_commit(&mut recovered_roster))
        .unwrap();

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(state.durable_signer.is_active());
    assert_eq!(state.height, epoch.height + 1);
    assert_eq!(state.vote_namespace, next_namespace);
    assert_eq!(state.recent_commit_round_cache.len(), 1);
    let cached = &state.recent_commit_round_cache[0];
    assert_eq!(cached.height, round_data.height);
    assert_eq!(cached.round, round_data.round);
    assert_eq!(cached.proposal.0, round_data.proposal.0);
    assert_eq!(cached.proposal_id, round_data.proposal_id);
    assert_eq!(cached.proposal_valid_round, round_data.proposal_valid_round);
    assert_eq!(cached.proposal_sigs, round_data.proposal_sigs);
    assert_eq!(cached.msg_val_sigs, round_data.msg_val_sigs);
    verify_reconstructed_precommit_quorum(cached, &recovered_roster).unwrap();
}

#[test]
fn transient_decision_apply_latches_exact_reconciliation_without_poison_or_advance() {
    let (keys, roster, mut round_data, epoch, hash_keys) = quorum_fixture(true);
    round_data.counts = round_data
        .msg_val_sigs
        .iter()
        .zip(&roster)
        .fold(ConsensusCounts::ZERO, |counts, (signatures, member)| {
            counts + ConsensusCounts::from(&(*signatures, member.stake))
        });
    let paths = TestPaths::new("decision-transient-reconciliation");
    let signer =
        DurableSigner::open(keys[0].clone(), paths.config(true), epoch.clone()).unwrap();
    let public_key = PubKeyID(VerificationKeyBytes::from(&keys[0]).into());
    let calls = Arc::new(AtomicU64::new(0));
    let calls_for_push = calls.clone();
    let mut state = TMState::init(
        signer,
        public_key,
        3032,
        ClosureToProposeNewBlock(Arc::new(|| Box::pin(async { None }))),
        ClosureToValidateProposedBlock(Arc::new(|_| {
            Box::pin(async { (TMStatus::Pass, TMStatusReason::None) })
        })),
        ClosureToPushDecidedBlock(Arc::new(move |_, _, _, _| {
            calls_for_push.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err("injected transient PoS apply timeout".into()) })
        })),
        ClosureToUpdatePeers(Arc::new(|_| Box::pin(async {}))),
        ClosureToAllowBftAccess(Arc::new(|_, _| Box::pin(async {}))),
    );
    state.hash_keys = hash_keys;
    state.height = epoch.height;
    state.vote_namespace = epoch.vote_namespace;
    state.round = round_data.round;
    state.step = TMStep::Precommit;
    state.rounds_data = vec![round_data];
    let mut live_roster = roster;

    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(state.bft_update(&mut live_roster));

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(matches!(
        state.durable_signer.status(),
        SignerStatus::ReconciliationRequired(_, reason)
            if reason.contains("injected transient PoS apply timeout")
    ));
    assert_eq!(state.height, epoch.height);
    assert!(state.recent_commit_round_cache.is_empty());
    assert!(state.reconciliation_required);

    drop(state);
    let reopened = DurableSigner::open(keys[0].clone(), paths.config(true), epoch).unwrap();
    assert!(matches!(
        reopened.status(),
        SignerStatus::ReconciliationRequired(_, _)
    ));
}

#[test]
fn reproposal_manifest_survives_wal_restart_and_exact_recovery() {
    let (keys, roster, mut round_data, epoch, hash_keys) = quorum_fixture(true);
    populate_proposal_manifest(&keys, &roster, &hash_keys, &mut round_data, 4);
    let certificate = canonical_precommit_certificate(&round_data, &roster).unwrap();
    let paths = TestPaths::new("commit-reproposal-manifest");
    let mut signer =
        DurableSigner::open(keys[0].clone(), paths.config(true), epoch.clone()).unwrap();
    let digest = signer
        .begin_commit(
            &hash_keys,
            round_data.round,
            round_data.proposal_id,
            &round_data.proposal,
            round_data.proposal_valid_round,
            &round_data.proposal_sigs,
            &certificate,
            &roster,
        )
        .unwrap();
    signer
        .require_reconciliation(digest, "injected transient PoS apply failure")
        .unwrap();
    assert!(matches!(
        signer.status(),
        SignerStatus::ReconciliationRequired(found, _) if *found == digest
    ));
    drop(signer);

    let mut reopened =
        DurableSigner::open(keys[0].clone(), paths.config(true), epoch).unwrap();
    assert!(matches!(
        reopened.status(),
        SignerStatus::ReconciliationRequired(found, _) if *found == digest
    ));
    let recovery = reopened
        .pending_commit_recovery(&hash_keys, &roster)
        .unwrap()
        .unwrap();
    assert_eq!(recovery.digest, digest);
    assert_eq!(recovery.round_data.round, round_data.round);
    assert_eq!(recovery.round_data.proposal, round_data.proposal);
    assert_eq!(recovery.round_data.proposal_valid_round, 4);
    assert_eq!(recovery.round_data.proposal_sigs, round_data.proposal_sigs);
    assert_eq!(recovery.round_data.proposal_sigs_n, round_data.proposal_sigs_n);
    verify_reconstructed_precommit_quorum(&recovery.round_data, &roster).unwrap();
    reopened
        .complete_commit(digest, round_data.proposal_id.0, [97u8; 32], &roster)
        .unwrap();
    assert!(reopened.is_active());
}

#[test]
fn reconciliation_digest_conflict_is_poison_but_transient_latch_is_not() {
    let (keys, roster, round_data, epoch, hash_keys) = quorum_fixture(true);
    let certificate = canonical_precommit_certificate(&round_data, &roster).unwrap();
    let paths = TestPaths::new("commit-reconciliation-conflict");
    let mut signer =
        DurableSigner::open(keys[0].clone(), paths.config(true), epoch.clone()).unwrap();
    let digest = signer
        .begin_commit(
            &hash_keys,
            round_data.round,
            round_data.proposal_id,
            &round_data.proposal,
            round_data.proposal_valid_round,
            &round_data.proposal_sigs,
            &certificate,
            &roster,
        )
        .unwrap();
    signer
        .require_reconciliation(digest, "transient apply timeout")
        .unwrap();
    assert!(!matches!(signer.status(), SignerStatus::Poisoned(_)));
    drop(signer);
    let mut reopened =
        DurableSigner::open(keys[0].clone(), paths.config(true), epoch).unwrap();
    assert!(matches!(
        reopened.status(),
        SignerStatus::ReconciliationRequired(_, _)
    ));
    let mut wrong = digest;
    wrong[0] ^= 1;
    assert!(matches!(
        reopened.require_reconciliation(wrong, "wrong digest"),
        Err(SignerError::Conflict(_))
    ));
    assert!(matches!(reopened.status(), SignerStatus::Poisoned(_)));
}

#[test]
fn unfinished_legacy_commit_intent_is_readable_but_not_auto_recoverable() {
    let (keys, roster, round_data, epoch, _) = quorum_fixture(true);
    let certificate = canonical_precommit_certificate(&round_data, &roster).unwrap();
    let paths = TestPaths::new("commit-legacy-pending");
    let mut signer =
        DurableSigner::open(keys[0].clone(), paths.config(true), epoch.clone()).unwrap();
    signer
        .append_legacy_pending_commit_for_test(
            round_data.proposal_id,
            &round_data.proposal,
            &certificate,
        )
        .unwrap();
    drop(signer);

    let legacy = DurableSigner::open(keys[0].clone(), paths.config(true), epoch).unwrap();
    assert!(matches!(
        legacy.status(),
        SignerStatus::ObserverOnly(reason) if reason.contains("legacy pending commit")
    ));
}

#[test]
fn crash_after_commit_applied_rejects_old_epoch_then_recovers_exact_successor() {
    let (keys, roster, round_data, epoch, hash_keys) = quorum_fixture(true);
    let certificate = canonical_precommit_certificate(&round_data, &roster).unwrap();
    let paths = TestPaths::new("commit-applied-crash");
    let mut signer =
        DurableSigner::open(keys[0].clone(), paths.config(true), epoch.clone()).unwrap();
    let digest = signer
        .begin_commit(
            &hash_keys,
            round_data.round,
            round_data.proposal_id,
            &round_data.proposal,
            round_data.proposal_valid_round,
            &round_data.proposal_sigs,
            &certificate,
            &roster,
        )
        .unwrap();
    signer.set_failpoint(WalFailpoint::AfterCommitApplied);
    assert!(signer
        .complete_commit(digest, round_data.proposal_id.0, [92u8; 32], &roster)
        .is_err());
    drop(signer);

    let old_epoch =
        DurableSigner::open(keys[0].clone(), paths.config(true), epoch.clone()).unwrap();
    assert!(
        matches!(old_epoch.status(), SignerStatus::ReconciliationRequired(_, reason)
        if reason.contains("commit recovery is incomplete"))
    );
    drop(old_epoch);

    let mut successor = epoch;
    successor.height += 1;
    successor.parent_commit = round_data.proposal_id.0;
    successor.vote_namespace = [92u8; 32];
    let recovered =
        DurableSigner::open(keys[0].clone(), paths.config(true), successor.clone()).unwrap();
    assert!(recovered.is_active());
    assert_eq!(recovered.epoch(), &successor);
}

#[test]
fn store_ahead_crash_after_commit_intent_recovers_exact_successor_automatically() {
    let (keys, roster, round_data, epoch, hash_keys) = quorum_fixture(true);
    let certificate = canonical_precommit_certificate(&round_data, &roster).unwrap();
    let paths = TestPaths::new("commit-intent-store-ahead");
    let mut signer =
        DurableSigner::open(keys[0].clone(), paths.config(true), epoch.clone()).unwrap();
    signer
        .begin_commit(
            &hash_keys,
            round_data.round,
            round_data.proposal_id,
            &round_data.proposal,
            round_data.proposal_valid_round,
            &round_data.proposal_sigs,
            &certificate,
            &roster,
        )
        .unwrap();
    drop(signer);

    let mut successor = epoch;
    successor.height += 1;
    successor.parent_commit = round_data.proposal_id.0;
    successor.vote_namespace = [93u8; 32];
    let recovered =
        DurableSigner::open(keys[0].clone(), paths.config(true), successor.clone()).unwrap();
    assert!(recovered.is_active());
    assert_eq!(recovered.epoch(), &successor);

    drop(recovered);
    let replayed = DurableSigner::open(keys[0].clone(), paths.config(true), successor).unwrap();
    assert!(replayed.is_active());
}

#[test]
fn store_ahead_recovery_rejects_an_unrelated_successor_without_mutating_history() {
    let (keys, roster, round_data, epoch, hash_keys) = quorum_fixture(true);
    let certificate = canonical_precommit_certificate(&round_data, &roster).unwrap();
    let paths = TestPaths::new("commit-intent-unrelated-successor");
    let mut signer =
        DurableSigner::open(keys[0].clone(), paths.config(true), epoch.clone()).unwrap();
    signer
        .begin_commit(
            &hash_keys,
            round_data.round,
            round_data.proposal_id,
            &round_data.proposal,
            round_data.proposal_valid_round,
            &round_data.proposal_sigs,
            &certificate,
            &roster,
        )
        .unwrap();
    drop(signer);

    let mut unrelated = epoch.clone();
    unrelated.height += 1;
    unrelated.parent_commit = [94u8; 32];
    unrelated.vote_namespace = [95u8; 32];
    let rejected = DurableSigner::open(keys[0].clone(), paths.config(true), unrelated).unwrap();
    assert!(matches!(rejected.status(), SignerStatus::ObserverOnly(_)));
    drop(rejected);

    let mut exact = epoch;
    exact.height += 1;
    exact.parent_commit = round_data.proposal_id.0;
    exact.vote_namespace = [95u8; 32];
    let recovered = DurableSigner::open(keys[0].clone(), paths.config(true), exact).unwrap();
    assert!(recovered.is_active());
}

#[test]
fn quorum_certificates_allow_a_signed_conflicting_minority() {
    for precommit in [false, true] {
        let (keys, roster, mut round_data, epoch, hash_keys) = quorum_fixture(precommit);
        let conflicting = ValueId([101u8; 32]);
        let signed = make_vote_sign_datas(
            roster[3].pub_key,
            precommit,
            round_data.height,
            round_data.round,
            conflicting,
        )[1];
        round_data.msg_val_sigs[3][usize::from(precommit)] = (
            conflicting,
            TMSig(sign_with_namespace(
                &keys[3],
                &signed,
                &epoch.vote_namespace,
            )),
        );
        if precommit {
            let certificate = canonical_precommit_certificate(&round_data, &roster).unwrap();
            verify_precommit_certificate(
                &certificate,
                round_data.round,
                round_data.proposal_id,
                &epoch,
                &roster,
            )
            .unwrap();
        } else {
            let transition = LockValidTransition {
                locked_round: i64::from(round_data.round),
                locked_value_id: round_data.proposal_id,
                locked_value: round_data.proposal.0.clone(),
                valid_round: i64::from(round_data.round),
                valid_value_id: round_data.proposal_id,
                valid_value: round_data.proposal.0.clone(),
                certificate: canonical_prevote_certificate(&round_data, &roster).unwrap(),
            };
            verify_transition_certificate(&transition, &epoch, &hash_keys, &roster).unwrap();
        }
    }
}
