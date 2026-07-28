use super::*;

fn fixture() -> ([u8; 32], HashKeys, Vec<RoundData>) {
    let namespace = [7u8; 32];
    let hash_keys = HashKeys::default();
    let signing_keys: Vec<SigningKey> = (1u8..=4)
        .map(|seed| SigningKey::from([seed; 32]))
        .collect();

    let mut cumulative_stake = 0u64;
    let roster: Vec<SortedRosterMember> = signing_keys
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

    let proposal = BlockValue(vec![42u8; 128]);
    let proposal_id = proposal.id_from_value(&hash_keys);
    let mut referenced = RoundData {
        height: 77,
        round: 1,
        proposal: proposal.clone(),
        proposal_valid_round: -1,
        proposal_sigs: vec![TMSig([1u8; 64])],
        proposal_sigs_n: 1,
        proposal_id,
        msg_val_sigs: vec![[(ValueId::NIL, TMSig::NIL); 2]; roster.len()],
        roster: roster.clone(),
        vote_namespace: namespace,
        ..RoundData::EMPTY
    };

    for roster_i in 0..3 {
        let signed_data = make_vote_sign_datas(
            roster[roster_i].pub_key,
            false,
            referenced.height,
            referenced.round,
            proposal_id,
        )[1];
        referenced.msg_val_sigs[roster_i][0] = (
            proposal_id,
            TMSig(sign_with_namespace(
                &signing_keys[roster_i],
                &signed_data,
                &namespace,
            )),
        );
    }

    let current = RoundData {
        height: 77,
        round: 2,
        proposal,
        proposal_valid_round: 1,
        proposal_sigs: vec![TMSig([2u8; 64])],
        proposal_sigs_n: 1,
        proposal_id,
        msg_val_sigs: vec![[(ValueId::NIL, TMSig::NIL); 2]; roster.len()],
        roster,
        vote_namespace: namespace,
        ..RoundData::EMPTY
    };

    (namespace, hash_keys, vec![referenced, current])
}

#[test]
fn accepts_verified_referenced_round_quorum_when_current_yes_is_zero() {
    let (namespace, hash_keys, rounds) = fixture();
    assert_eq!(
        verified_referenced_prevote_certificate(&rounds, 1, &namespace, &hash_keys),
        Some((1, 3, 3)),
    );
    assert_eq!(rounds[1].counts.yes_prevotes, 0);
}

#[test]
fn rejects_referenced_certificate_outside_canonical_round_domain() {
    let (namespace, hash_keys, rounds) = fixture();

    let mut high_current = rounds.clone();
    high_current[1].round = MAX_CONSENSUS_ROUND + 1;
    assert_eq!(
        verified_referenced_prevote_certificate(&high_current, 1, &namespace, &hash_keys),
        None,
    );

    let mut high_referenced = rounds;
    high_referenced[0].round = MAX_CONSENSUS_ROUND + 1;
    high_referenced[1].proposal_valid_round = i64::from(MAX_CONSENSUS_ROUND) + 1;
    assert_eq!(
        verified_referenced_prevote_certificate(&high_referenced, 1, &namespace, &hash_keys),
        None,
    );
}

#[test]
fn rejects_subquorum_forgery_and_cross_domain_evidence() {
    let (namespace, hash_keys, rounds) = fixture();

    let mut subquorum = rounds.clone();
    subquorum[0].msg_val_sigs[2][0] = (ValueId::NIL, TMSig::NIL);
    assert_eq!(
        verified_referenced_prevote_certificate(&subquorum, 1, &namespace, &hash_keys),
        None,
    );

    let mut forged = rounds.clone();
    forged[0].msg_val_sigs[0][0].1 = TMSig([9u8; 64]);
    assert_eq!(
        verified_referenced_prevote_certificate(&forged, 1, &namespace, &hash_keys),
        None,
    );

    let mut wrong_roster = rounds.clone();
    wrong_roster[0].roster[0].stake = 2;
    assert_eq!(
        verified_referenced_prevote_certificate(&wrong_roster, 1, &namespace, &hash_keys),
        None,
    );

    let mut wrong_value = rounds.clone();
    wrong_value[1].proposal.0[0] ^= 1;
    assert_eq!(
        verified_referenced_prevote_certificate(&wrong_value, 1, &namespace, &hash_keys),
        None,
    );

    let mut wrong_namespace = rounds.clone();
    wrong_namespace[0].vote_namespace = [8u8; 32];
    assert_eq!(
        verified_referenced_prevote_certificate(
            &wrong_namespace,
            1,
            &namespace,
            &hash_keys,
        ),
        None,
    );
}

#[test]
fn accepts_quorum_despite_one_correctly_signed_conflicting_minority_vote() {
    let (namespace, hash_keys, mut rounds) = fixture();
    let conflicting = ValueId([99u8; 32]);
    let key = SigningKey::from([4u8; 32]);
    let signed = make_vote_sign_datas(
        rounds[0].roster[3].pub_key,
        false,
        rounds[0].height,
        rounds[0].round,
        conflicting,
    )[1];
    rounds[0].msg_val_sigs[3][0] = (
        conflicting,
        TMSig(sign_with_namespace(&key, &signed, &namespace)),
    );
    assert_eq!(
        verified_referenced_prevote_certificate(&rounds, 1, &namespace, &hash_keys),
        Some((1, 3, 3)),
    );
}

#[test]
fn referenced_quorum_uses_only_the_active_first_hundred_members() {
    let namespace = [31u8; 32];
    let hash_keys = HashKeys::default();
    let signing_keys: Vec<SigningKey> = (1u8..=101)
        .map(|seed| SigningKey::from([seed; 32]))
        .collect();
    let mut cumulative_stake = 0u64;
    let roster: Vec<SortedRosterMember> = signing_keys.iter().map(|key| {
        cumulative_stake += 1;
        SortedRosterMember {
            pub_key: PubKeyID(VerificationKeyBytes::from(key).into()),
            stake: 1,
            cumulative_stake,
        }
    }).collect();
    assert_eq!(active_roster_len(&roster), 100);

    let proposal = BlockValue(vec![32u8; 128]);
    let proposal_id = proposal.id_from_value(&hash_keys);
    let mut referenced = RoundData {
        height: 88,
        round: 3,
        proposal: proposal.clone(),
        proposal_sigs: vec![TMSig([1u8; 64])],
        proposal_sigs_n: 1,
        proposal_id,
        msg_val_sigs: vec![[(ValueId::NIL, TMSig::NIL); 2]; 100],
        roster: roster.clone(),
        vote_namespace: namespace,
        ..RoundData::EMPTY
    };
    for roster_i in 0..67 {
        let signed = make_vote_sign_datas(
            roster[roster_i].pub_key,
            false,
            referenced.height,
            referenced.round,
            proposal_id,
        )[1];
        referenced.msg_val_sigs[roster_i][0] = (
            proposal_id,
            TMSig(sign_with_namespace(&signing_keys[roster_i], &signed, &namespace)),
        );
    }
    let current = RoundData {
        height: 88,
        round: 4,
        proposal,
        proposal_valid_round: 3,
        proposal_sigs: vec![TMSig([2u8; 64])],
        proposal_sigs_n: 1,
        proposal_id,
        msg_val_sigs: vec![[(ValueId::NIL, TMSig::NIL); 2]; 100],
        roster,
        vote_namespace: namespace,
        ..RoundData::EMPTY
    };

    assert_eq!(
        verified_referenced_prevote_certificate(
            &[referenced, current],
            1,
            &namespace,
            &hash_keys,
        ),
        Some((3, 67, 67)),
    );
}

fn state_for_condition_28(mut rounds: Vec<RoundData>, namespace: [u8; 32]) -> (TMState, Vec<SortedRosterMember>) {
    let original_roster = rounds[1].roster.clone();
    let mut order: Vec<usize> = (0..original_roster.len()).collect();
    order.sort_by(|left, right| {
        (original_roster[*right].stake, original_roster[*right].pub_key)
            .cmp(&(original_roster[*left].stake, original_roster[*left].pub_key))
    });
    let mut cumulative_stake = 0u64;
    let canonical_roster: Vec<SortedRosterMember> = order
        .iter()
        .map(|index| {
            let mut member = original_roster[*index].clone();
            cumulative_stake += member.stake;
            member.cumulative_stake = cumulative_stake;
            member
        })
        .collect();
    for round in &mut rounds {
        let previous_signatures = round.msg_val_sigs.clone();
        round.roster = canonical_roster.clone();
        round.msg_val_sigs = order
            .iter()
            .map(|index| previous_signatures[*index])
            .collect();
    }
    let roster = rounds[1].roster.clone();
    let signing_key = (1u8..=4)
        .map(|seed| SigningKey::from([seed; 32]))
        .find(|key| PubKeyID(VerificationKeyBytes::from(key).into()) == roster[0].pub_key)
        .expect("canonical first roster member must be in the fixture key set");
    let public_key = PubKeyID(VerificationKeyBytes::from(&signing_key).into());
    assert_eq!(public_key, roster[0].pub_key);
    let signer = DurableSigner::ephemeral_for_simulation(
        signing_key,
        SignerEpochBinding {
            public_key,
            chain_id: [1u8; 32],
            height: 77,
            parent_commit: [2u8; 32],
            vote_namespace: namespace,
            consensus_config_hash: [3u8; 32],
            roster_hash: canonical_roster_hash(&roster).unwrap(),
            roster_index: 0,
            active_roster_len: roster.len() as u32,
        },
    );
    let mut state = TMState::init(
        signer,
        public_key,
        3032,
        ClosureToProposeNewBlock(Arc::new(|| Box::pin(async { None }))),
        ClosureToValidateProposedBlock(Arc::new(|_| {
            Box::pin(async { (TMStatus::Pass, TMStatusReason::None) })
        })),
        ClosureToPushDecidedBlock(Arc::new(|_, _, _, _| {
            Box::pin(async { Err("decision closure is unused in condition-28 test".into()) })
        })),
        ClosureToUpdatePeers(Arc::new(|_| Box::pin(async {}))),
        ClosureToAllowBftAccess(Arc::new(|_, _| Box::pin(async {}))),
    );
    state.height = 77;
    state.round = 2;
    state.step = TMStep::Propose;
    state.vote_namespace = namespace;
    state.rounds_data = rounds;
    (state, roster)
}

#[test]
fn condition_28_state_machine_prevotes_reproposal_with_historical_qc_and_current_yv_zero() {
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let (namespace, hash_keys, rounds) = fixture();
        assert_eq!(rounds[1].counts.yes_prevotes, 0);
        let proposal_id = rounds[1].proposal_id;
        let (mut state, mut roster) = state_for_condition_28(rounds, namespace);
        state.hash_keys = hash_keys;

        state.bft_update(&mut roster).await;

        assert_eq!(state.step, TMStep::Prevote);
        let current = state
            .rounds_data
            .iter()
            .find(|round| (round.height, round.round) == (77, 2))
            .unwrap();
        assert_eq!(current.msg_val_sigs[0][0].0, proposal_id);
        assert_ne!(current.msg_val_sigs[0][0].1, TMSig::NIL);
    });
}

#[test]
fn condition_28_state_machine_does_not_vote_on_forged_historical_qc() {
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let (namespace, hash_keys, mut rounds) = fixture();
        rounds[0].msg_val_sigs[0][0].1 = TMSig([0x99; 64]);
        let (mut state, mut roster) = state_for_condition_28(rounds, namespace);
        state.hash_keys = hash_keys;

        state.bft_update(&mut roster).await;

        assert_eq!(state.step, TMStep::Propose);
        let current = state
            .rounds_data
            .iter()
            .find(|round| (round.height, round.round) == (77, 2))
            .unwrap();
        assert_eq!(current.msg_val_sigs[0][0], (ValueId::NIL, TMSig::NIL));
    });
}

#[test]
fn condition_28_indeterminate_validation_does_not_suppress_timeout_or_round_advance() {
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let (namespace, hash_keys, rounds) = fixture();
        let (mut state, mut roster) = state_for_condition_28(rounds, namespace);
        state.hash_keys = hash_keys;
        state.validate_closure = ClosureToValidateProposedBlock(Arc::new(|_| {
            Box::pin(async {
                (
                    TMStatus::Indeterminate,
                    TMStatusReason::NeedsBlock { hash: [9u8; 32] },
                )
            })
        }));
        let current = state
            .rounds_data
            .iter_mut()
            .find(|round| (round.height, round.round) == (77, 2))
            .unwrap();
        current.active_timeout = Some(Timeout {
            time: Instant::now() - std::time::Duration::from_secs(1),
            height: 77,
            round: 2,
            step: TMStep::Propose,
        });

        state.bft_update(&mut roster).await;

        assert_eq!(state.step, TMStep::Prevote);
        let current = state
            .rounds_data
            .iter()
            .find(|round| (round.height, round.round) == (77, 2))
            .unwrap();
        assert_eq!(current.msg_val_sigs[0][0].0, ValueId::NIL);
        assert_ne!(current.msg_val_sigs[0][0].1, TMSig::NIL);

        // Supply a NIL-prevote quorum so the node reaches precommit, then prove the
        // precommit timeout still advances to the next round while the proposal's
        // PoW dependency remains unavailable.
        let signing_keys: Vec<SigningKey> = (1u8..=4)
            .map(|seed| SigningKey::from([seed; 32]))
            .collect();
        for roster_i in 0..3 {
            let member = roster[roster_i].pub_key;
            let key = signing_keys
                .iter()
                .find(|key| PubKeyID(VerificationKeyBytes::from(*key).into()) == member)
                .unwrap();
            let signable = make_vote_sign_datas(member, false, 77, 2, ValueId::NIL)[0];
            state.check_and_incorporate_msg(
                77,
                2,
                0,
                ValueId::NIL,
                -2,
                &roster,
                roster_i,
                PACKET_TYPE_PREVOTE_SIGNATURES,
                &signable,
                TMSig(sign_with_namespace(key, &signable, &namespace)),
            );
        }
        state.bft_update(&mut roster).await;
        assert_eq!(state.step, TMStep::Precommit);
        let current = state
            .rounds_data
            .iter_mut()
            .find(|round| (round.height, round.round) == (77, 2))
            .unwrap();
        current.active_timeout = Some(Timeout {
            time: Instant::now() - std::time::Duration::from_secs(1),
            height: 77,
            round: 2,
            step: TMStep::Precommit,
        });
        state.bft_update(&mut roster).await;
        assert_eq!(state.round, 3);
    });
}
