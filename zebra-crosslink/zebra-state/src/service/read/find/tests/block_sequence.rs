//! Fixed test vectors for reading a run of blocks as a single chain, and for finding the forks
//! a caller would read that way.

use std::sync::Arc;

use zebra_chain::{
    amount::NonNegative,
    block::{Block, Height},
    parameters::Network,
    value_balance::ValueBalance,
};

use crate::{
    arbitrary::Prepare,
    service::{
        finalized_state::FinalizedState, non_finalized_state::NonFinalizedState,
        read::find::{block_sequence, sidechain_forks},
    },
    tests::FakeChainHelper,
    Config,
};

/// A best chain of `root, b2, b3` plus a side chain that swaps `b3` for `b3_side, b4_side`.
///
/// The side chain is two blocks long so a run can end below its tip and still have to pick
/// between two blocks at the same height.
///
/// Pre-Heartwood blocks, since the brand new `FinalizedState` passes a `None` history tree
/// to the `NonFinalizedState`.
fn fixture() -> (
    NonFinalizedState,
    FinalizedState,
    Vec<Arc<Block>>,
    Vec<Arc<Block>>,
) {
    let network = Network::Mainnet;
    let root: Arc<Block> = Arc::new(network.test_block(653599, 583999).unwrap());
    let b2 = root.make_fake_child().set_work(10);
    let b3 = b2.make_fake_child().set_work(10);
    let b3_side = b2.make_fake_child().set_work(1);
    let b4_side = b3_side.make_fake_child().set_work(1);

    let mut non_finalized_state = NonFinalizedState::new(&network, Default::default());
    let finalized_state = FinalizedState::new(
        &Config::ephemeral(),
        &network,
        #[cfg(feature = "elasticsearch")]
        false,
    );
    finalized_state.set_finalized_value_pool(ValueBalance::<NonNegative>::fake_populated_pool());

    non_finalized_state
        .commit_new_chain(root.clone().prepare(), &finalized_state)
        .unwrap();
    for block in [&b2, &b3, &b3_side, &b4_side] {
        non_finalized_state
            .commit_block(block.clone().prepare(), &finalized_state)
            .unwrap();
    }

    (
        non_finalized_state,
        finalized_state,
        vec![root, b2, b3],
        vec![b3_side, b4_side],
    )
}

/// Every returned run must be one chain, ascending, with no gaps.
fn assert_is_one_chain(seq: &[(Height, zebra_chain::block::Hash, Arc<Block>)]) {
    for i in 1..seq.len() {
        assert_eq!(
            seq[i].2.header.previous_block_hash,
            seq[i - 1].1,
            "block at {:?} must be a child of the block below it",
            seq[i].0
        );
        assert_eq!(seq[i].0 .0, seq[i - 1].0 .0 + 1, "heights must be contiguous");
        assert_eq!(seq[i].1, seq[i].2.hash(), "hash must match the block");
    }
}

fn height_of(block: &Arc<Block>) -> Height {
    block.coinbase_height().unwrap()
}

#[test]
fn sequence_follows_the_best_chain() {
    let _init_guard = zebra_test::init();
    let (non_finalized_state, finalized_state, best, _side) = fixture();

    let seq = block_sequence(
        &non_finalized_state,
        &finalized_state.db,
        best[2].hash(),
        height_of(&best[2]),
        height_of(&best[0]),
        1000,
    );

    assert_is_one_chain(&seq);
    assert_eq!(
        seq.iter().map(|(_, hash, _)| *hash).collect::<Vec<_>>(),
        best.iter().map(|block| block.hash()).collect::<Vec<_>>(),
    );
}

/// The point of anchoring on a hash: a run ending on a side-chain block reports that
/// branch, not the best chain. `FindBlockHashes` cannot express this.
#[test]
fn sequence_follows_a_side_chain() {
    let _init_guard = zebra_test::init();
    let (non_finalized_state, finalized_state, best, side) = fixture();

    let seq = block_sequence(
        &non_finalized_state,
        &finalized_state.db,
        side[1].hash(),
        height_of(&side[1]),
        height_of(&best[0]),
        1000,
    );

    assert_is_one_chain(&seq);
    assert_eq!(seq.len(), 4);
    assert_eq!(seq[3].1, side[1].hash(), "the run must end on the anchor");
}

/// The anchor picks the chain even when the top of the run is below it: at the height where
/// the two branches disagree, the anchor's block wins over the best chain's.
#[test]
fn hi_height_resolves_within_the_anchors_chain() {
    let _init_guard = zebra_test::init();
    let (non_finalized_state, finalized_state, best, side) = fixture();

    assert_eq!(
        height_of(&side[0]),
        height_of(&best[2]),
        "the fixture must have two blocks at this height",
    );

    let seq = block_sequence(
        &non_finalized_state,
        &finalized_state.db,
        side[1].hash(),
        height_of(&side[0]),
        height_of(&best[0]),
        1000,
    );

    assert_is_one_chain(&seq);
    assert_eq!(seq.len(), 3);
    assert_eq!(seq[2].1, side[0].hash(), "the run must stay on the anchor's branch");
    assert_ne!(seq[2].1, best[2].hash(), "and not switch to the best chain");
}

/// A `hi_height` above the anchor is not an error: the run tops out at the anchor, which is
/// what a caller asking for "up to the tip I read" wants when the state has moved on.
#[test]
fn hi_height_clamps_to_the_anchor() {
    let _init_guard = zebra_test::init();
    let (non_finalized_state, finalized_state, best, _side) = fixture();

    let seq = block_sequence(
        &non_finalized_state,
        &finalized_state.db,
        best[1].hash(),
        Height(height_of(&best[2]).0 + 100),
        height_of(&best[0]),
        1000,
    );

    assert_is_one_chain(&seq);
    assert_eq!(seq.len(), 2);
    assert_eq!(seq[1].1, best[1].hash());
}

#[test]
fn max_len_keeps_the_blocks_nearest_the_top() {
    let _init_guard = zebra_test::init();
    let (non_finalized_state, finalized_state, best, _side) = fixture();

    let seq = block_sequence(
        &non_finalized_state,
        &finalized_state.db,
        best[2].hash(),
        height_of(&best[2]),
        height_of(&best[0]),
        2,
    );

    assert_is_one_chain(&seq);
    assert_eq!(seq.len(), 2);
    assert_eq!(seq[0].1, best[1].hash());
    assert_eq!(seq[1].1, best[2].hash());
}

#[test]
fn a_fork_is_reported_with_its_tip_and_the_height_it_leaves_the_best_chain() {
    let _init_guard = zebra_test::init();
    let (non_finalized_state, _finalized_state, best, side) = fixture();

    let forks = sidechain_forks(&non_finalized_state);

    assert_eq!(forks.len(), 1, "the best chain must not be reported as a fork");
    assert_eq!(forks[0].tip_hash, side[1].hash());
    assert_eq!(forks[0].tip_height, height_of(&side[1]));
    assert_eq!(
        forks[0].fork_height,
        height_of(&best[2]),
        "the fork height is where the branches first disagree, not the chain root",
    );
}

/// What the visualizer does: read every reported fork as its own run.
#[test]
fn following_a_reported_fork_gives_exactly_the_divergent_branch() {
    let _init_guard = zebra_test::init();
    let (non_finalized_state, finalized_state, best, side) = fixture();

    let fork = sidechain_forks(&non_finalized_state)[0];
    let seq = block_sequence(
        &non_finalized_state,
        &finalized_state.db,
        fork.tip_hash,
        fork.tip_height,
        fork.fork_height,
        1000,
    );

    assert_is_one_chain(&seq);
    assert_eq!(
        seq.iter().map(|(_, hash, _)| *hash).collect::<Vec<_>>(),
        side.iter().map(|block| block.hash()).collect::<Vec<_>>(),
        "no shared blocks, no missing ones",
    );
    assert_eq!(
        seq[0].2.header.previous_block_hash,
        best[1].hash(),
        "the branch must hang off a best-chain block, so a caller holding the best chain can \
         attach it",
    );
}

/// Forks of forks share their lower blocks, so a caller reading each run separately sees those
/// blocks more than once.
#[test]
fn forks_of_forks_are_reported_separately() {
    let _init_guard = zebra_test::init();
    let (mut non_finalized_state, finalized_state, _best, side) = fixture();

    let sibling = side[0].make_fake_child().set_work(2);
    non_finalized_state
        .commit_block(sibling.clone().prepare(), &finalized_state)
        .unwrap();

    let forks = sidechain_forks(&non_finalized_state);

    assert_eq!(forks.len(), 2);
    assert!(forks
        .iter()
        .all(|fork| fork.fork_height == height_of(&side[0])));
    let tips: Vec<_> = forks.iter().map(|fork| fork.tip_hash).collect();
    assert!(tips.contains(&side[1].hash()));
    assert!(tips.contains(&sibling.hash()));
}

#[test]
fn a_state_with_no_chains_has_no_forks() {
    let _init_guard = zebra_test::init();

    assert!(
        sidechain_forks(&NonFinalizedState::new(&Network::Mainnet, Default::default())).is_empty()
    );
}

#[test]
fn unresolvable_requests_are_empty() {
    let _init_guard = zebra_test::init();
    let (non_finalized_state, finalized_state, best, _side) = fixture();
    let root_height = height_of(&best[0]);
    let tip_height = height_of(&best[2]);

    // A hash the state has never seen: what a stale visualizer anchor looks like after the
    // block behind it is gone.
    let unknown = zebra_chain::block::Hash([0xab; 32]);
    assert!(block_sequence(
        &non_finalized_state,
        &finalized_state.db,
        unknown,
        tip_height,
        root_height,
        1000,
    )
    .is_empty());

    // An anchor below the requested low height.
    assert!(block_sequence(
        &non_finalized_state,
        &finalized_state.db,
        best[0].hash(),
        tip_height,
        tip_height,
        1000,
    )
    .is_empty());

    // A window entirely above the anchor, which clamping collapses to nothing.
    assert!(block_sequence(
        &non_finalized_state,
        &finalized_state.db,
        best[0].hash(),
        Height(tip_height.0 + 100),
        Height(root_height.0 + 1),
        1000,
    )
    .is_empty());

    // No room for even one block.
    assert!(block_sequence(
        &non_finalized_state,
        &finalized_state.db,
        best[2].hash(),
        tip_height,
        root_height,
        0,
    )
    .is_empty());
}
