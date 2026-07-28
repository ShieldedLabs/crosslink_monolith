use super::*;

fn round(height: u64, round: u32, valid_round: i64) -> RoundData {
    RoundData {
        height,
        round,
        proposal_valid_round: valid_round,
        ..RoundData::EMPTY
    }
}

#[test]
fn gossip_always_includes_current_and_referenced_round_and_is_bounded() {
    let rounds = vec![
        round(9, 0, -1),
        round(9, 1, -1),
        round(9, 2, -1),
        round(9, 3, -1),
        round(9, 4, 1),
    ];

    for cursor in 0..20 {
        let (selected, _) = round_indices_to_gossip(&rounds, 9, 4, cursor);
        assert!(selected.contains(&4));
        assert!(selected.contains(&1));
        assert!(selected.len() <= 3);
    }
}

#[test]
fn gossip_rotation_eventually_covers_every_other_round() {
    let rounds = vec![
        round(8, 0, -1),
        round(9, 0, -1),
        round(9, 1, -1),
        round(9, 2, -1),
        round(9, 3, -1),
        round(9, 4, 1),
        round(10, 0, -1),
    ];

    let mut cursor = 0;
    let mut seen = std::collections::BTreeSet::new();
    for _ in 0..6 {
        let (selected, next_cursor) = round_indices_to_gossip(&rounds, 9, 4, cursor);
        cursor = next_cursor;
        seen.extend(selected);
    }

    assert_eq!(
        seen,
        std::collections::BTreeSet::from([1usize, 2, 3, 4, 5]),
    );
}

#[test]
fn gossip_missing_current_round_fails_closed() {
    let rounds = vec![round(9, 0, -1), round(9, 1, -1)];
    assert_eq!(round_indices_to_gossip(&rounds, 9, 2, 99), (Vec::new(), 0));
}

#[test]
fn historical_cache_metadata_lookup_is_checked_base_relative_and_exact() {
    let cache = vec![round(40, 7, -1), round(41, 3, -1)];
    assert_eq!(commit_round_cache_entry_at_height(&cache, 40).unwrap().height, 40);
    assert_eq!(commit_round_cache_entry_at_height(&cache, 41).unwrap().height, 41);
    assert!(commit_round_cache_entry_at_height(&cache, 0).is_none());
    assert!(commit_round_cache_entry_at_height(&cache, 39).is_none());
    assert!(commit_round_cache_entry_at_height(&cache, 42).is_none());
    assert!(commit_round_cache_entry_at_height(&cache, u64::MAX).is_none());

    let gapped = vec![round(40, 7, -1), round(42, 3, -1)];
    assert!(commit_round_cache_entry_at_height(&gapped, 41).is_none());

    let mut relayable = round(50, 2, -1);
    relayable.proposal = BlockValue(vec![5; 32]);
    relayable.proposal_id = relayable.proposal.id_from_value(&HashKeys::default());
    relayable.proposal_sigs = vec![TMSig([5; 64])];
    relayable.proposal_sigs_n = 1;
    let mut cache = vec![relayable];
    assert!(cached_commit_round_at_height(&cache, 50).is_some());
    compact_round_proposal_payload(&mut cache[0]);
    assert!(commit_round_cache_entry_at_height(&cache, 50).is_some());
    assert!(cached_commit_round_at_height(&cache, 50).is_none());
}
