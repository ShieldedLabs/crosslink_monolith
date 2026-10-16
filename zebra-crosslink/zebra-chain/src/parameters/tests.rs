//! Consensus parameter tests for Zebra.

#![allow(clippy::unwrap_in_result)]

use std::collections::HashSet;

use crate::block;

use super::*;

use Network::*;
use NetworkUpgrade::*;

/// Check that the activation heights and network upgrades are unique.
#[test]
fn activation_bijective() {
    let _init_guard = zebra_test::init();

    let mainnet_activations = Mainnet.activation_list();
    let mainnet_heights: HashSet<&block::Height> = mainnet_activations.keys().collect();
    assert_eq!(MAINNET_ACTIVATION_HEIGHTS.len(), mainnet_heights.len());

    let mainnet_nus: HashSet<&NetworkUpgrade> = mainnet_activations.values().collect();
    assert_eq!(MAINNET_ACTIVATION_HEIGHTS.len(), mainnet_nus.len());

    let testnet_activations = Network::new_default_testnet().activation_list();
    let testnet_heights: HashSet<&block::Height> = testnet_activations.keys().collect();
    assert_eq!(TESTNET_ACTIVATION_HEIGHTS.len(), testnet_heights.len());

    let testnet_nus: HashSet<&NetworkUpgrade> = testnet_activations.values().collect();
    assert_eq!(TESTNET_ACTIVATION_HEIGHTS.len(), testnet_nus.len());
}

#[test]
fn activation_extremes_mainnet() {
    let _init_guard = zebra_test::init();
    activation_extremes(Mainnet)
}

#[test]
#[ignore] // [ACTIVATION HEIGHT PROBLEM]
fn activation_extremes_testnet() {
    let _init_guard = zebra_test::init();
    activation_extremes(Network::new_default_testnet())
}

/// Test the activation_list, activation_height, current, and next functions
/// for `network` with extreme values.
fn activation_extremes(network: Network) {
    // The first three upgrades are Genesis, BeforeOverwinter, and Overwinter
    assert_eq!(
        network.activation_list().get(&block::Height(0)),
        Some(&Genesis)
    );
    assert_eq!(Genesis.activation_height(&network), Some(block::Height(0)));
    assert!(NetworkUpgrade::is_activation_height(
        &network,
        block::Height(0)
    ));

    assert_eq!(NetworkUpgrade::current(&network, block::Height(0)), Genesis);
    assert_eq!(
        NetworkUpgrade::next(&network, block::Height(0)),
        Some(BeforeOverwinter)
    );

    assert_eq!(
        network.activation_list().get(&block::Height(1)),
        Some(&BeforeOverwinter)
    );
    assert_eq!(
        BeforeOverwinter.activation_height(&network),
        Some(block::Height(1))
    );
    assert!(NetworkUpgrade::is_activation_height(
        &network,
        block::Height(1)
    ));

    assert_eq!(
        NetworkUpgrade::current(&network, block::Height(1)),
        BeforeOverwinter
    );
    assert_eq!(
        NetworkUpgrade::next(&network, block::Height(1)),
        Some(Overwinter)
    );

    assert!(!NetworkUpgrade::is_activation_height(
        &network,
        block::Height(2)
    ));

    // We assume that the last upgrade we know about continues forever
    // (even if we suspect that won't be true)
    assert_ne!(
        network.activation_list().get(&block::Height::MAX),
        Some(&Genesis)
    );
    assert!(!NetworkUpgrade::is_activation_height(
        &network,
        block::Height::MAX
    ));

    assert_ne!(
        NetworkUpgrade::current(&network, block::Height::MAX),
        Genesis
    );
    assert_eq!(NetworkUpgrade::next(&network, block::Height::MAX), None);
}

#[test]
fn activation_consistent_mainnet() {
    let _init_guard = zebra_test::init();
    activation_consistent(Mainnet)
}

#[test]
fn activation_consistent_testnet() {
    let _init_guard = zebra_test::init();
    activation_consistent(Network::new_default_testnet())
}

/// Check that the `activation_height`, `is_activation_height`,
/// `current`, and `next` functions are consistent for `network`.
fn activation_consistent(network: Network) {
    let activation_list = network.activation_list();
    let network_upgrades: HashSet<&NetworkUpgrade> = activation_list.values().collect();

    for &network_upgrade in network_upgrades {
        let height = network_upgrade
            .activation_height(&network)
            .expect("activations must have a height");
        assert!(NetworkUpgrade::is_activation_height(&network, height));

        if height > block::Height(0) {
            // Genesis is immediately followed by BeforeOverwinter,
            // but the other network upgrades have multiple blocks between them
            assert!(!NetworkUpgrade::is_activation_height(
                &network,
                (height + 1).unwrap()
            ));
        }

        assert_eq!(NetworkUpgrade::current(&network, height), network_upgrade);
        // Network upgrades don't repeat
        assert_ne!(
            NetworkUpgrade::next(&network, height),
            Some(network_upgrade)
        );
        assert_ne!(
            NetworkUpgrade::next(&network, block::Height(height.0 + 1)),
            Some(network_upgrade)
        );
        assert_ne!(
            NetworkUpgrade::next(&network, block::Height::MAX),
            Some(network_upgrade)
        );
    }
}

/// Check that the network upgrades and branch ids are unique.
#[test]
fn branch_id_bijective() {
    let _init_guard = zebra_test::init();

    let branch_id_list = NetworkUpgrade::branch_id_list();
    let nus: HashSet<&NetworkUpgrade> = branch_id_list.keys().collect();
    assert_eq!(CONSENSUS_BRANCH_IDS.len(), nus.len());

    let branch_ids: HashSet<&ConsensusBranchId> = branch_id_list.values().collect();
    assert_eq!(CONSENSUS_BRANCH_IDS.len(), branch_ids.len());
}

#[test]
fn branch_id_extremes_mainnet() {
    let _init_guard = zebra_test::init();
    branch_id_extremes(Mainnet)
}

#[test]
fn branch_id_extremes_testnet() {
    let _init_guard = zebra_test::init();
    branch_id_extremes(Network::new_default_testnet())
}

/// Test the branch_id_list, branch_id, and current functions for `network` with
/// extreme values.
fn branch_id_extremes(network: Network) {
    // Branch ids were introduced in Overwinter
    assert_eq!(
        NetworkUpgrade::branch_id_list().get(&BeforeOverwinter),
        None
    );
    assert_eq!(ConsensusBranchId::current(&network, block::Height(0)), None);
    assert_eq!(
        NetworkUpgrade::branch_id_list().get(&Overwinter).cloned(),
        Overwinter.branch_id()
    );

    // We assume that the last upgrade we know about continues forever
    // (even if we suspect that won't be true)
    assert_ne!(
        NetworkUpgrade::branch_id_list()
            .get(&NetworkUpgrade::current(&network, block::Height::MAX)),
        None
    );
    assert_ne!(
        ConsensusBranchId::current(&network, block::Height::MAX),
        None
    );
}

#[test]
fn branch_id_consistent_mainnet() {
    let _init_guard = zebra_test::init();
    branch_id_consistent(Mainnet)
}

#[test]
#[ignore] // [ACTIVATION HEIGHT PROBLEM]
fn branch_id_consistent_testnet() {
    let _init_guard = zebra_test::init();
    branch_id_consistent(Network::new_default_testnet())
}

/// Check that the branch_id and current functions are consistent for `network`.
fn branch_id_consistent(network: Network) {
    let branch_id_list = NetworkUpgrade::branch_id_list();
    let network_upgrades: HashSet<&NetworkUpgrade> = branch_id_list.keys().collect();

    for &network_upgrade in network_upgrades {
        let height = network_upgrade.activation_height(&network);

        // Skip network upgrades that don't have activation heights yet
        if let Some(height) = height {
            assert_eq!(
                ConsensusBranchId::current(&network, height),
                network_upgrade.branch_id()
            );
        }
    }
}

// TODO: split this file in unit.rs and prop.rs
use hex::{FromHex, ToHex};
use proptest::prelude::*;

proptest! {
    #[test]
    fn branch_id_hex_roundtrip(nu in any::<NetworkUpgrade>()) {
        let _init_guard = zebra_test::init();

        if let Some(branch) = nu.branch_id() {
            let hex_branch: String = branch.encode_hex();
            let new_branch = ConsensusBranchId::from_hex(hex_branch.clone()).expect("hex branch_id should parse");
            prop_assert_eq!(branch, new_branch);
            prop_assert_eq!(hex_branch, new_branch.to_string());
        }
    }
}

/// A list of network upgrades in the order that they must be activated.
const NETWORK_UPGRADES_IN_ORDER: &[NetworkUpgrade] = &[
    Genesis,
    BeforeOverwinter,
    Overwinter,
    Sapling,
    Blossom,
    Heartwood,
    Canopy,
    Nu5,
    Nu6,
    Nu6_1,
    Nu6_2,
    #[cfg(any(test, feature = "zebra-test"))]
    Nu6_3,
    #[cfg(any(test, feature = "zebra-test"))]
    FeaturenetUpgrade1,
    #[cfg(any(test, feature = "zebra-test"))]
    FeaturenetUpgrade2,
    #[cfg(any(test, feature = "zebra-test"))]
    Nu7,
];

#[test]
fn network_upgrade_iter_matches_order_constant() {
    let iter_upgrades: Vec<NetworkUpgrade> = NetworkUpgrade::iter().collect();
    let expected_upgrades: Vec<NetworkUpgrade> = NETWORK_UPGRADES_IN_ORDER.to_vec();

    assert_eq!(iter_upgrades, expected_upgrades);
}

#[test]
fn full_activation_list_contains_all_upgrades() {
    let network = Network::Mainnet;
    let full_list = network.full_activation_list();

    // The featurenet upgrades and NU7 are unscheduled on Mainnet (no activation height committed),
    // so they are absent from the full activation list even though they are always present in the
    // iter.
    assert_eq!(full_list.len(), NetworkUpgrade::iter().count() - 3);
}

/// FeaturenetUpgrade2's branch ID is the bytes `FTU2`, and Zebra and zcash_protocol both map it
/// back to the upgrade, so a transaction signed under it is read as FeaturenetUpgrade2's.
#[test]
fn featurenet_upgrade_2_branch_id_round_trips() {
    let _init_guard = zebra_test::init();

    let branch_id = FeaturenetUpgrade2
        .branch_id()
        .expect("FeaturenetUpgrade2 has a consensus branch ID");
    assert_eq!(u32::from(branch_id).to_be_bytes(), *b"FTU2");

    let upgrade = NetworkUpgrade::try_from(u32::from(branch_id))
        .expect("Zebra knows the FeaturenetUpgrade2 branch ID");
    assert_eq!(upgrade, FeaturenetUpgrade2);

    let lrz_branch_id = zcash_protocol::consensus::BranchId::try_from(branch_id)
        .expect("zcash_protocol knows the FeaturenetUpgrade2 branch ID");
    assert_eq!(
        lrz_branch_id,
        zcash_protocol::consensus::BranchId::FeaturenetUpgrade2
    );
    assert_eq!(u32::from(lrz_branch_id), u32::from(branch_id));
    let lrz_upgrade = lrz_branch_id
        .network_upgrade()
        .expect("a featurenet branch ID has a network upgrade");
    assert_eq!(NetworkUpgrade::from(lrz_upgrade), FeaturenetUpgrade2);
    assert_eq!(lrz_upgrade.branch_id(), lrz_branch_id);

    let hex_branch_id: String = branch_id.encode_hex();
    assert_eq!(
        ConsensusBranchId::from_hex(hex_branch_id).expect("hex branch ID parses"),
        branch_id
    );
}


/// The Testnet minimum-difficulty exemption: from the configured start height on stock Testnet
/// parameters, never on parameters that switch it off, and never once FeaturenetUpgrade2 is
/// active, whatever the start height.
#[test]
fn minimum_difficulty_exemption_follows_start_height_and_featurenet_upgrade_2() {
    let _init_guard = zebra_test::init();

    let gap = |network: &Network, height: block::Height| {
        NetworkUpgrade::target_spacing_for_height(network, height) * 6
    };
    let gapped = |network: &Network, height: block::Height, extra_seconds: i64| {
        let previous = chrono::DateTime::from_timestamp(1_800_000_000, 0).expect("in range");
        let time = previous + gap(network, height) + chrono::Duration::seconds(extra_seconds);
        NetworkUpgrade::is_testnet_min_difficulty_block(network, height, time, previous)
    };
    let start = block::Height(299_188);
    let above = block::Height(400_000);

    let stock = Network::new_default_testnet();
    let Testnet(stock_params) = &stock else {
        unreachable!("the default testnet is a Testnet");
    };
    assert_eq!(stock_params.minimum_difficulty_start_height(), Some(start));
    assert_eq!(NetworkUpgrade::minimum_difficulty_spacing_for_height(&stock, block::Height(start.0 - 1)), None);
    assert_eq!(NetworkUpgrade::minimum_difficulty_spacing_for_height(&stock, start), Some(gap(&stock, start)));
    assert!(gapped(&stock, above, 1));
    assert!(!gapped(&stock, above, 0), "the gap must be strictly greater than six spacings");

    let featurenet = testnet::Parameters::build()
        .with_minimum_difficulty_start_height(None)
        .to_network()
        .expect("valid parameters");
    for height in [start, above, block::Height::MAX] {
        assert_eq!(NetworkUpgrade::minimum_difficulty_spacing_for_height(&featurenet, height), None);
    }
    assert!(!gapped(&featurenet, above, 1));

    // Stock Testnet parameters with FeaturenetUpgrade2 scheduled above every other upgrade and
    // above the start height.
    let mut heights = testnet::ConfiguredActivationHeights::from(stock_params.activation_heights());
    let last_scheduled = stock_params.activation_heights().keys().last().expect("Testnet has upgrades").0;
    let activation = block::Height(last_scheduled.max(above.0) + 1);
    heights.featurenet_upgrade_2 = Some(activation.0);
    let upgraded = testnet::Parameters::build()
        .with_activation_heights(heights)
        .expect("valid activation heights")
        .to_network()
        .expect("valid parameters");
    let below = block::Height(activation.0 - 1);
    assert_eq!(NetworkUpgrade::current(&upgraded, activation), FeaturenetUpgrade2);
    assert_eq!(NetworkUpgrade::minimum_difficulty_spacing_for_height(&upgraded, below), Some(gap(&upgraded, below)));
    assert!(gapped(&upgraded, below, 1));
    assert_eq!(NetworkUpgrade::minimum_difficulty_spacing_for_height(&upgraded, activation), None);
    assert!(!gapped(&upgraded, activation, 1));
}
