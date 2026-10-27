//! Test-format-based tests

#![cfg(test)]

use chrono::{DateTime, Utc};
use std::{path::PathBuf, sync::Arc, time::Duration};

use zcash_keys::address::Address;
use zebra_chain::{
    block::{
        merkle, Block, BftBlock, BftBlockAndFatPointerToIt, ChainHistoryBlockTxAuthCommitmentHash,
        FatPointerSignature, FatPointerToBftBlock, PROTOTYPE_PARAMETERS, PubKeyID,
        Hash as BlockHash, Header as BlockHeader, Height as BlockHeight,
    },
    fmt::HexDebug,
    history_tree::HistoryTree,
    orchard,
    parameters::{Network, NetworkUpgrade},
    sapling,
    serialization::*,
    transaction::{LockTime, Transaction},
    work::{self, difficulty::CompactDifficulty},
};
use zebra_crosslink::test_format::*;
use zebra_state::crosslink::*;
use zebrad::application::CROSSLINK_TEST_CONFIG_OVERRIDE;
use zebrad::config::ZebradConfig;

macro_rules! function_name {
    () => {{
        fn f() {}
        fn type_name_of<T>(_: T) -> &'static str {
            std::any::type_name::<T>()
        }
        let name = type_name_of(f);
        name.strip_suffix("::f")
            .unwrap()
            .split("::")
            .last()
            .unwrap()
    }};
}

/// Set the global state TEST_NAME
pub fn set_test_name(name: &'static str) {
    // Every test body calls this first, before building any blocks, so the time from here to
    // `test_start` is the scenario's setup.
    zebra_crosslink::test_timing::mark_setup_begin();
    *zebra_crosslink::TEST_NAME.lock().unwrap() = name;
}

/// Crosslink Test entrypoint
pub fn test_start() {
    zebra_crosslink::test_timing::mark_boot_begin();
    // init globals
    {
        // Consensus parameters are fixed when the network is built, so they are read from the test
        // file here, before the node boots.
        let bytes = {
            let path = zebra_crosslink::TEST_INSTR_PATH.lock().unwrap().clone();
            match path {
                Some(path) => std::fs::read(path).unwrap_or_default(),
                None => zebra_crosslink::TEST_INSTR_BYTES.lock().unwrap().clone(),
            }
        };
        let crosslink = crosslink_parameters_for_test(&bytes);
        let node_config = node_config_for_test(&bytes);
        *CROSSLINK_TEST_CONFIG_OVERRIDE.lock().unwrap() = {
            let mut base = ZebradConfig::default();
            base.network.network = test_network(&crosslink, &node_config);
            if !node_config.is_empty() {
                let file: ZebradConfig = toml::from_str(&node_config).expect("a test file's node config parses as zebrad.toml");
                base.crosslink.slashforks = file.crosslink.slashforks;
                base.crosslink.disable_shipped_slashforks = file.crosslink.disable_shipped_slashforks;
                base.state.network_checkpoint = file.state.network_checkpoint;
                base.crosslink.explicit_bft_key_seed = file.crosslink.explicit_bft_key_seed;
            }
            // A finalizer identity the test knows, so a scenario can bond to the node and the BFT
            // instructions can find it on the roster.
            if base.crosslink.explicit_bft_key_seed.is_none() {
                base.crosslink.explicit_bft_key_seed = Some(zebra_crosslink::test_format::TEST_NODE_FINALIZER_SEED.to_string());
            }
            base.state.ephemeral = true;
            // getblocktemplate refuses to build a coinbase without one (MINE_FROM_TEMPLATE). The
            // test miner's key, so scenarios can spend what the node's templates pay out.
            base.mining.miner_address = Some(
                TestKey::new(b"crosslink test miner")
                    .address()
                    .encode(&base.network.network)
                    .parse()
                    .expect("an encoded address parses"),
            );

            Some(std::sync::Arc::new(base))
        };
        *zebra_crosslink::TEST_MODE.lock().unwrap() = true;
        *zebra_crosslink::TEST_CHECK_ASSERT.lock().unwrap() = 0;
        *zebra_crosslink::TEST_SHUTDOWN_FN.lock().unwrap() = || {
            zebra_crosslink::dump_test_instrs();
            // APPLICATION.shutdown(abscissa_core::Shutdown::Graceful);
            std::process::exit(*zebra_crosslink::TEST_FAILED.lock().unwrap());
        }
    }

    use zebrad::application::{ZebradApp, APPLICATION};

    // boot
    let os_args: Vec<_> = std::env::args_os().collect();
    // panic!("OS args: {:?}", os_args);
    let args: Vec<std::ffi::OsString> = vec![
        os_args[0].clone(),
        zebrad::commands::EntryPoint::default_cmd_as_str().into(),
    ];
    // println!("args: {:?}", args);

    // Mirrors `zebrad::application::boot`, except that tests are headless unless asked: they
    // run unattended, several at once, and a test thread is not the main thread macOS wants.
    // Any non-empty ZEBRA_TEST_GUI opens the window, and then a failed test stays on screen
    // instead of aborting.
    let headless = std::env::var_os("ZEBRA_TEST_GUI").map_or(true, |v| v.is_empty());
    zebra_crosslink::viz2::run_node(headless, move || ZebradApp::run_command(&APPLICATION, args));
}

/// The harness parameters with the prototype's from-chain bootstrap instead of `Supplied`, for
/// the one test that exercises the bootstrap gate. Sigma stays the harness's pinned value, since
/// the scenes are built for it (see `HARNESS_PARAMETERS`).
const BOOTSTRAP_HARNESS_PARAMETERS: zcash_primitives::bft::ZcashCrosslinkParameters =
    zcash_primitives::bft::ZcashCrosslinkParameters {
        bootstrap: PROTOTYPE_PARAMETERS.bootstrap,
        ..HARNESS_PARAMETERS
    };

/// Run a Crosslink Test from a dynamic byte array.
pub fn test_bytes(bytes: Vec<u8>) {
    *zebra_crosslink::TEST_INSTR_BYTES.lock().unwrap() = bytes;
    test_start();
}

/// Run a Crosslink Test from a file path.
pub fn test_path(path: PathBuf) {
    *zebra_crosslink::TEST_INSTR_PATH.lock().unwrap() = Some(path);
    test_start();
}

const REGTEST_BLOCK_BYTES: &[&[u8]] = &[
    //                                                                     i   h
    include_bytes!("../../crosslink-test-data/test_pow_block_0.bin"), //   0,  1: 02a610...
    include_bytes!("../../crosslink-test-data/test_pow_block_1.bin"), //   1,  2: 02d711...
    include_bytes!("../../crosslink-test-data/test_pow_block_2.bin"), //   2,  fork 3: 098207...
    include_bytes!("../../crosslink-test-data/test_pow_block_3.bin"), //   3,  3: 0032241...
    include_bytes!("../../crosslink-test-data/test_pow_block_4.bin"), //   4,  4: 0247f1a...
    include_bytes!("../../crosslink-test-data/test_pow_block_6.bin"), //   5,  5: 0ab3a5d8...
    include_bytes!("../../crosslink-test-data/test_pow_block_7.bin"), //   6,  6
    include_bytes!("../../crosslink-test-data/test_pow_block_9.bin"), //   7,  7
    include_bytes!("../../crosslink-test-data/test_pow_block_10.bin"), //  8,  8
    include_bytes!("../../crosslink-test-data/test_pow_block_12.bin"), //  9,  9
    include_bytes!("../../crosslink-test-data/test_pow_block_13.bin"), // 10, 10
    include_bytes!("../../crosslink-test-data/test_pow_block_14.bin"), // 11, 11
    include_bytes!("../../crosslink-test-data/test_pow_block_15.bin"), // 12, 12
    include_bytes!("../../crosslink-test-data/test_pow_block_17.bin"), // 13, 13
    include_bytes!("../../crosslink-test-data/test_pow_block_18.bin"), // 14, 14
    include_bytes!("../../crosslink-test-data/test_pow_block_19.bin"), // 15, 15
    include_bytes!("../../crosslink-test-data/test_pow_block_21.bin"), // 16, 16
    include_bytes!("../../crosslink-test-data/test_pow_block_22.bin"), // 17, 17
    include_bytes!("../../crosslink-test-data/test_pow_block_23.bin"), // 18, 18
    include_bytes!("../../crosslink-test-data/test_pow_block_25.bin"), // 19, 19
    include_bytes!("../../crosslink-test-data/test_pow_block_26.bin"), // 20, 20
    include_bytes!("../../crosslink-test-data/test_pow_block_28.bin"), // 21, 21
    include_bytes!("../../crosslink-test-data/test_pow_block_29.bin"), // 22, 22
];
const REGTEST_BLOCK_BYTES_N: usize = REGTEST_BLOCK_BYTES.len();

#[allow(dead_code)]
fn regtest_block_hashes() -> [BlockHash; REGTEST_BLOCK_BYTES_N] {
    let mut hashes = [BlockHash([0; 32]); REGTEST_BLOCK_BYTES_N];
    for i in 0..REGTEST_BLOCK_BYTES_N {
        // ALT: BlockHeader::
        hashes[i] = Block::zcash_deserialize(REGTEST_BLOCK_BYTES[i])
            .unwrap()
            .hash();
    }
    hashes
}

const REGTEST_POS_BLOCK_BYTES: &[&[u8]] = &[
    include_bytes!("../../crosslink-test-data/test_pos_block_5.bin"),
    include_bytes!("../../crosslink-test-data/test_pos_block_8.bin"),
    include_bytes!("../../crosslink-test-data/test_pos_block_11.bin"),
    include_bytes!("../../crosslink-test-data/test_pos_block_16.bin"),
    include_bytes!("../../crosslink-test-data/test_pos_block_20.bin"),
    include_bytes!("../../crosslink-test-data/test_pos_block_24.bin"),
    include_bytes!("../../crosslink-test-data/test_pos_block_27.bin"),
];

const REGTEST_POW_IDX_FINALIZED_BY_POS_BLOCK: &[usize] = &[1, 4, 6, 10, 13, 16, 18];

/// Filename index of each block in `REGTEST_BLOCK_BYTES` / `REGTEST_POS_BLOCK_BYTES`. The two
/// sets interleave as one 0..29 sequence, so the numbering only makes sense together.
const POW_FILE_IDX: [usize; REGTEST_BLOCK_BYTES_N] = [
    0, 1, 2, 3, 4, 6, 7, 9, 10, 12, 13, 14, 15, 17, 18, 19, 21, 22, 23, 25, 26, 28, 29,
];
const POS_FILE_IDX: [usize; 7] = [5, 8, 11, 16, 20, 24, 27];

/// The regtest network a scenario's node runs, for building that scenario's blocks. Consensus
/// rules that depend on the Crosslink parameters must see the same ones on both sides: the staking
/// share of the coinbase, for one, starts at the bootstrap activation height, or at genesis when
/// BFT is supplied.
fn regtest_network(params: &zcash_primitives::bft::ZcashCrosslinkParameters) -> Network {
    test_network(params, "")
}

/// The network a test file's node runs: regtest with the file's Crosslink parameters, and the
/// file's `[network]` parameters (activation heights, funding streams, lockbox payouts,
/// checkpoints, unshielded coinbase spends) when its node config sets any. test_start and
/// scenarios building blocks for that node both call this, so the two cannot drift.
fn test_network(params: &zcash_primitives::bft::ZcashCrosslinkParameters, node_config: &str) -> Network {
    use zebra_chain::parameters::testnet::RegtestParameters;

    let default = || Network::new_regtest(RegtestParameters { crosslink: Some(*params), ..Default::default() });
    if node_config.is_empty() {
        return default();
    }
    // Parsed as the node parses it, so a bad file fails with zebrad's own message.
    let file: ZebradConfig = toml::from_str(node_config).expect("a test file's node config parses as zebrad.toml");
    assert!(file.network.network.is_regtest(), "test files configure a regtest network, not {}", file.network.network);

    // A config can't carry the harness's Crosslink parameters (it can't express supplied BFT), so
    // the network is built again with the file's own on top. The configured values are read from
    // the file, not from the network zebra built: building keeps one upgrade per activation
    // height, so a built network reads back with different heights for upgrades that shared one.
    // This mirrors zebra-network's build_regtest_params, for both ways a config names regtest.
    let value: toml::Value = toml::from_str(node_config).expect("a test file's node config parses as TOML");
    let section = value.get("network");
    let configured = section
        .and_then(|network| network.get("network"))
        .and_then(|network| network.get("params"))
        .or_else(|| section.and_then(|network| network.get("testnet_parameters")));
    let Some(configured) = configured else {
        return default();
    };
    fn field<T: serde::de::DeserializeOwned>(table: &toml::Value, name: &str) -> Option<T> {
        let value = table.get(name)?.clone();
        Some(value.try_into().unwrap_or_else(|err| panic!("node config network parameter {name}: {err}")))
    }
    let mut funding_streams: Vec<zebra_chain::parameters::testnet::ConfiguredFundingStreams> =
        field(configured, "funding_streams").unwrap_or_default();
    if let Some(post_nu6) = field(configured, "post_nu6_funding_streams") {
        funding_streams.insert(0, post_nu6);
    }
    if let Some(pre_nu6) = field(configured, "pre_nu6_funding_streams") {
        funding_streams.insert(0, pre_nu6);
    }
    Network::new_regtest(RegtestParameters {
        activation_heights: field(configured, "activation_heights").unwrap_or_default(),
        funding_streams: Some(funding_streams),
        lockbox_disbursements: field(configured, "lockbox_disbursements"),
        checkpoints: field(configured, "checkpoints"),
        extend_funding_stream_addresses_as_required: field(configured, "extend_funding_stream_addresses_as_required"),
        should_allow_unshielded_coinbase_spends: field(configured, "should_allow_unshielded_coinbase_spends"),
        crosslink: Some(*params),
    })
}

/// A node config fragment configuring regtest with these activation heights, for
/// TF::new_with_node_config. Written from the configured heights: serializing a built network
/// would fold upgrades that share a height (see test_network).
fn node_config_with_activation_heights(heights: zebra_chain::parameters::testnet::ConfiguredActivationHeights) -> String {
    let mut params = toml::map::Map::new();
    params.insert("activation_heights".to_string(), toml::Value::try_from(heights).expect("activation heights serialize"));
    let mut network = toml::map::Map::new();
    network.insert("params".to_string(), toml::Value::Table(params));
    let mut section = toml::map::Map::new();
    section.insert("network".to_string(), toml::Value::Table(network));
    let mut config = toml::map::Map::new();
    config.insert("network".to_string(), toml::Value::Table(section));
    toml::to_string(&config).expect("a TOML table serializes")
}

/// A test file's node config reaches the node unchanged: the network rebuilt from it under the
/// file's Crosslink parameters is the network the config described. Needs no node.
#[test]
fn test_format_node_config_round_trips_the_network() {
    use zebra_chain::parameters::testnet::{ConfiguredActivationHeights, RegtestParameters};

    let plain = test_network(&HARNESS_PARAMETERS, "");
    let defaulted = node_config_with_activation_heights(ConfiguredActivationHeights::default());
    assert_eq!(test_network(&HARNESS_PARAMETERS, &defaulted), plain);

    let heights = ConfiguredActivationHeights { nu6_1: Some(10), ..Default::default() };
    let later_upgrade = Network::new_regtest(RegtestParameters {
        activation_heights: heights,
        crosslink: Some(HARNESS_PARAMETERS),
        ..Default::default()
    });
    assert_ne!(later_upgrade, plain);
    let node_config = node_config_with_activation_heights(heights);
    assert_eq!(test_network(&HARNESS_PARAMETERS, &node_config), later_upgrade);

    let bytes = TF::new_with_node_config(&HARNESS_PARAMETERS, &node_config).write_to_bytes();
    assert_eq!(node_config_for_test(&bytes), node_config);
    assert_eq!(crosslink_parameters_for_test(&bytes), HARNESS_PARAMETERS);
}

/// A test file moves a network upgrade, and the node runs with the moved height.
/// FeaturenetUpgrade1 activates far beyond any scenario by default; here the file's node config
/// puts it at height 4. The node shows it took effect at exactly that height: a block there built
/// for the default schedule, whose coinbase is still the older upgrade's, is refused, and one
/// built for the moved schedule is accepted, as is the node's own next block.
#[test]
fn crosslink_upgrade_height_set_by_the_test_file() {
    use zebra_chain::parameters::testnet::ConfiguredActivationHeights;

    set_test_name(function_name!());
    let params = HARNESS_PARAMETERS;
    let activation = 4;
    let node_config = node_config_with_activation_heights(ConfiguredActivationHeights {
        featurenet_upgrade_1: Some(activation),
        ..Default::default()
    });
    let mut tf = TF::new_with_node_config(&params, &node_config);

    let network = test_network(&params, &node_config);
    let default_schedule = regtest_network(&params);
    assert_ne!(NetworkUpgrade::current(&network, BlockHeight(activation - 1)), NetworkUpgrade::FeaturenetUpgrade1);
    assert_eq!(NetworkUpgrade::current(&network, BlockHeight(activation)), NetworkUpgrade::FeaturenetUpgrade1);
    assert_ne!(NetworkUpgrade::current(&default_schedule, BlockHeight(activation)), NetworkUpgrade::FeaturenetUpgrade1);

    let key = TestKey::new(b"crosslink test miner");
    let miner = key.address();
    let mut gen = BlockGen::init_at_genesis_plus_1(network, BlockGen::REGTEST_GENESIS_HASH, &miner);
    tf.push_instr_load_pow(&gen.tip, 0);
    for _ in 2..activation {
        tf.push_instr_load_pow(&gen.next_block(&miner), 0);
    }

    // the same chain, continued by a generator that thinks the upgrade is still far off
    let mut default_gen = gen.clone();
    default_gen.network = default_schedule;
    tf.push_instr_load_pow(&default_gen.next_block(&miner), SHOULD_FAIL);

    let first_after = gen.next_block(&miner);
    tf.push_instr_load_pow(&first_after, 0);
    tf.push_instr_expect_pow_tip(&first_after.hash(), 0);
    tf.push_instr_mine_from_template_into(1, 0);
    tf.push_instr_expect_pow_tip_slot(1, 0);
    tf.push_instr_expect_pow_chain_length(activation as usize + 2, 0);
    tf.push_instr_expect_node_alive(0);

    test_bytes(tf.write_to_bytes());
}

/// FeaturenetUpgrade2 is unscheduled by default; a test file's node config schedules it at height
/// 4, and the node switches to it at exactly that height, as for FeaturenetUpgrade1 above: a block
/// there built for the default schedule, whose coinbase is still the older upgrade's, is refused,
/// and one built for the moved schedule is accepted, as is the node's own next block.
#[test]
fn crosslink_featurenet_upgrade_2_height_set_by_the_test_file() {
    use zebra_chain::parameters::testnet::ConfiguredActivationHeights;

    set_test_name(function_name!());
    let params = HARNESS_PARAMETERS;
    let activation = 4;
    let node_config = node_config_with_activation_heights(ConfiguredActivationHeights {
        featurenet_upgrade_2: Some(activation),
        ..Default::default()
    });
    let mut tf = TF::new_with_node_config(&params, &node_config);

    let network = test_network(&params, &node_config);
    let default_schedule = regtest_network(&params);
    assert_ne!(NetworkUpgrade::current(&network, BlockHeight(activation - 1)), NetworkUpgrade::FeaturenetUpgrade2);
    assert_eq!(NetworkUpgrade::current(&network, BlockHeight(activation)), NetworkUpgrade::FeaturenetUpgrade2);
    assert_eq!(NetworkUpgrade::FeaturenetUpgrade2.activation_height(&default_schedule), None);
    assert_ne!(NetworkUpgrade::current(&default_schedule, BlockHeight::MAX), NetworkUpgrade::FeaturenetUpgrade2);

    let key = TestKey::new(b"crosslink test miner");
    let miner = key.address();
    let mut gen = BlockGen::init_at_genesis_plus_1(network, BlockGen::REGTEST_GENESIS_HASH, &miner);
    tf.push_instr_load_pow(&gen.tip, 0);
    for _ in 2..activation {
        tf.push_instr_load_pow(&gen.next_block(&miner), 0);
    }

    // the same chain, continued by a generator that does not know the upgrade
    let mut default_gen = gen.clone();
    default_gen.network = default_schedule;
    tf.push_instr_load_pow(&default_gen.next_block(&miner), SHOULD_FAIL);

    let first_after = gen.next_block(&miner);
    tf.push_instr_load_pow(&first_after, 0);
    tf.push_instr_expect_pow_tip(&first_after.hash(), 0);
    tf.push_instr_mine_from_template_into(1, 0);
    tf.push_instr_expect_pow_tip_slot(1, 0);
    tf.push_instr_expect_pow_chain_length(activation as usize + 2, 0);
    tf.push_instr_expect_node_alive(0);

    test_bytes(tf.write_to_bytes());
}

/// The node and the network for a fat pointer binding scenario: FeaturenetUpgrade2 at
/// `activation`, or unscheduled as by default.
fn fat_pointer_binding_node(activation: Option<u32>) -> (TF, Network) {
    use zebra_chain::parameters::testnet::ConfiguredActivationHeights;

    let params = HARNESS_PARAMETERS;
    let Some(activation) = activation else {
        let network = regtest_network(&params);
        assert_eq!(NetworkUpgrade::FeaturenetUpgrade2.activation_height(&network), None);
        return (TF::new(&params), network);
    };
    let node_config = node_config_with_activation_heights(ConfiguredActivationHeights {
        featurenet_upgrade_2: Some(activation),
        ..Default::default()
    });
    let network = test_network(&params, &node_config);
    assert_eq!(
        NetworkUpgrade::FeaturenetUpgrade2.activation_height(&network),
        Some(zebra_chain::block::Height(activation)),
        "the test network schedules the upgrade where the scenario puts it",
    );
    (TF::new_with_node_config(&params, &node_config), network)
}

/// Loads six blocks and two BFT blocks over them, bft0 finalizing P1 and bft1 finalizing P2, and
/// builds P7 citing bft1: the honest block. Then one copy of P7 with the pointer changed and the
/// commitment left as it was: bft0 cited in place of bft1. The copy is a valid block apart from
/// the commitment, which is what the two sides of the upgrade disagree on.
///
/// The scene runs on the nil roster of `HARNESS_PARAMETERS`, under which the node admits exactly
/// one certificate for a BFT block from FeaturenetUpgrade2: no signatures, round 0
/// (`certificate_is_valid`). So the BFT blocks are decided at round 0 with no signatures, and a
/// pointer with a changed height byte or an added signature is not a variant this scene can
/// offer: the certificate check would refuse it before the commitment check ran, proving nothing
/// about the binding. Changed signature lists are covered on a real roster by
/// `fat_pointer_binding_on_a_roster_across_featurenet_upgrade_2`.
fn fat_pointer_binding_scene(tf: &mut TF, network: Network) -> (Arc<Block>, Vec<Arc<Block>>) {
    let miner = Address::decode(&network, "t27eWDgjFYJGVXmzrXeVjnb5J3uXDM9xH9v").unwrap();
    let mut gen = BlockGen::init_at_genesis_plus_1(network, BlockGen::REGTEST_GENESIS_HASH, &miner);
    let mut pow = vec![gen.tip.clone()];
    for _ in 2..=6 {
        pow.push(gen.next_block(&miner));
    }
    for block in &pow {
        tf.push_instr_load_pow(block, 0);
    }

    let (pos_h, fat_ptr) = (&mut 0, &mut FatPointerToBftBlock::null());
    let bft0 = next_pos_at_round(pos_h, fat_ptr, &pow[1..4], 0, &[]);
    tf.push_instr_load_pos(&bft0, 0);
    let bft1 = next_pos_at_round(pos_h, fat_ptr, &pow[2..5], 0, &[]);
    tf.push_instr_load_pos(&bft1, 0);

    gen.next_block(&miner);
    let honest = point_tip_at_bft(&mut gen, &bft1.0.fat_ptr);
    assert_ne!(honest.header.fat_pointer_to_bft_block, FatPointerToBftBlock::null());

    let with_pointer = |pointer: FatPointerToBftBlock| {
        Arc::new(Block {
            header: Arc::new(BlockHeader { fat_pointer_to_bft_block: pointer, ..*honest.header }),
            ..honest.as_ref().clone()
        })
    };
    let tampered = vec![with_pointer(bft0.0.fat_ptr.clone())];
    (honest, tampered)
}

/// From FeaturenetUpgrade2 the block commitment binds the fat pointer, so a block whose pointer
/// was changed after its commitment was computed is refused. Here the pointer names another
/// admissible BFT block, bft0 in place of bft1, so the copy passes the certificate check and every
/// other rule and fails only the commitment. Without the binding this is a free re-roll of the
/// block hash after one Equihash solve. The honest block commits, and the chain neither grows nor
/// moves for the copy.
#[test]
fn crosslink_fat_pointer_changed_after_the_commitment_is_refused_from_featurenet_upgrade_2() {
    set_test_name(function_name!());
    let (mut tf, network) = fat_pointer_binding_node(Some(4));
    let (honest, tampered) = fat_pointer_binding_scene(&mut tf, network);

    tf.push_instr_load_pow(&honest, 0);
    tf.push_instr_expect_pow_chain_length(8, 0);
    tf.push_instr_expect_pow_tip(&honest.hash(), 0);
    for block in &tampered {
        tf.push_instr_load_pow(block, SHOULD_FAIL);
        tf.push_instr_expect_rejection_reason("InvalidChainHistoryBlockTxAuthCommitment", 0);
        tf.push_instr_expect_pow_chain_length(8, 0);
        tf.push_instr_expect_pow_tip(&honest.hash(), 0);
    }
    tf.push_instr_expect_node_alive(0);

    test_bytes(tf.write_to_bytes());
}

/// The other side of the upgrade boundary: with FeaturenetUpgrade2 unscheduled the commitment
/// binds no pointer, so the same changed copy is a valid sibling of the honest block, accepted as
/// it is today.
#[test]
fn crosslink_fat_pointer_changed_after_the_commitment_is_accepted_without_featurenet_upgrade_2() {
    set_test_name(function_name!());
    let (mut tf, network) = fat_pointer_binding_node(None);
    let (honest, tampered) = fat_pointer_binding_scene(&mut tf, network);

    tf.push_instr_load_pow(&honest, 0);
    tf.push_instr_expect_pow_chain_length(8, 0);
    for block in &tampered {
        tf.push_instr_load_pow(block, 0);
        tf.push_instr_expect_pow_chain_length(8, 0);
    }
    tf.push_instr_expect_node_alive(0);

    test_bytes(tf.write_to_bytes());
}

/// With FeaturenetUpgrade2 unscheduled a block's hash is what it was before the pointer was bound:
/// the regtest genesis, and the ten blocks of finality diagram scene 1, the last four of which
/// cite BFT blocks. The hashes were recorded from the scene as the node loaded it before the
/// binding existed (commit 32a059c6). Needs no node.
#[test]
fn crosslink_block_hashes_without_featurenet_upgrade_2_are_unchanged() {
    assert_eq!(
        NetworkUpgrade::FeaturenetUpgrade2.activation_height(&regtest_network(&HARNESS_PARAMETERS)),
        None
    );
    assert_eq!(zebra_chain::block::genesis::regtest_genesis_block().hash(), BlockGen::REGTEST_GENESIS_HASH);
    assert_eq!(
        BlockGen::REGTEST_GENESIS_HASH.to_string(),
        "029f11d80ef9765602235e1bc9727e3eb6ba20839319f761fee920d63401e327"
    );

    let (_, pow) = diagram_scene_1();
    let recorded = [
        "cfb89d521721f3701f91e9694c2af5c771e824151f7c97b43be933fb48edc62f",
        "fdfad1b116eedb950a10c390c90dadf9a1c51474c309e0c908b8bf950a21d0ba",
        "e61f66d735a00551dff49bf1655317fdb6fb339e3e213f14a1e0be90f6c14d13",
        "acddb611e758944eac867a216e35817c1e0dee5ffe59a42b3e94915fe4efe5a6",
        "c94c501de9865044a242971f41384c239fa37edff2f38497e5b72385188429ec",
        "8e675045103ad0a3dc0b53ccbe4db4e59f8b435af62f61490a5e5135470ae915",
        "881a5e9cb5241ff0ded972912cbd82b1d4aeca6ed470a71c844b57774823a782",
        "af5079db84a11e9e45bba032c2222fd1d6d4e218d1c4d6c1644f4b63097c20cd",
        "1e144a99639c95a176252edb320aba4d8e951ea4122f9bb8e0b010bc99a1bd22",
        "d8a09226dbe218be557c69b97badaec15b058cc0c78170fe13f52ac5095ed034",
    ];
    assert_eq!(pow.iter().map(|b| b.hash().to_string()).collect::<Vec<_>>(), recorded);
    assert!(pow[6..].iter().all(|b| b.header.fat_pointer_to_bft_block != FatPointerToBftBlock::null()));
}

/// From FeaturenetUpgrade2 the node's own template carries a commitment over its pointer: the
/// template's `blockcommitmentshash` is built with the pointer's digest, so the block mined from
/// it unchanged commits. Its parent cites a BFT block with the one certificate the nil roster
/// admits (see `fat_pointer_binding_scene`), so the mined block must cite one too (a pointer
/// cannot revert to null), and its commitment therefore binds a real digest.
#[test]
fn crosslink_template_commits_to_its_fat_pointer_from_featurenet_upgrade_2() {
    set_test_name(function_name!());
    let (mut tf, network) = fat_pointer_binding_node(Some(4));
    let (honest, _) = fat_pointer_binding_scene(&mut tf, network);

    tf.push_instr_load_pow(&honest, 0);
    tf.push_instr_mine_from_template_into(1, 0);
    tf.push_instr_expect_pow_tip_slot(1, 0);
    tf.push_instr_expect_pow_chain_length(9, 0);
    tf.push_instr_expect_node_alive(0);

    test_bytes(tf.write_to_bytes());
}

/// Rewrite the checked-in binaries in `crosslink-test-data` from the current block format.
///
/// A tool rather than a test, so it is `#[ignore]`d like `read_from_file` and runs only when
/// named. The checked-in files predate Ironwood v6: the PoS blocks no longer deserialize and
/// the PoW blocks carry pre-rework `hashBlockCommitments`.
///
/// Two invariants the consumers depend on, neither obvious from the data itself:
/// only the clone advances at i == 2, so `pow[2]` and `pow[3]` are siblings at height 3 and
/// the chain must not grow when index 3 arrives, which is what
/// `crosslink_push_example_pow_chain_only`'s `2 + i - (i >= 3)` asserts; and each PoS block
/// finalizes the PoW index named by `REGTEST_POW_IDX_FINALIZED_BY_POS_BLOCK`.
///
///     cargo nextest run -p zebrad --test crosslink regen_test_data --run-ignored only
#[ignore]
#[test]
fn regen_test_data() {
    let dir = PathBuf::from("../crosslink-test-data");
    assert!(dir.is_dir(), "expected {dir:?} relative to the zebrad crate dir");

    let network = regtest_network(&HARNESS_PARAMETERS);
    let miner_addr = Address::decode(&network, "t27eWDgjFYJGVXmzrXeVjnb5J3uXDM9xH9v").unwrap();
    // A second valid P2PKH miner: a different coinbase gives a different block hash, so the
    // sibling at height 3 actually competes.
    let miner_addr2 = Address::Transparent(
        zcash_transparent::address::TransparentAddress::PublicKeyHash([1u8; 20]),
    );
    let mut gen =
        BlockGen::init_at_genesis_plus_1(network, BlockGen::REGTEST_GENESIS_HASH, &miner_addr);

    let mut pow = vec![gen.tip.clone()];
    let mut genb = gen.clone();
    for i in 1..REGTEST_BLOCK_BYTES_N {
        if i == 2 {
            pow.push(genb.next_block(&miner_addr2));
        } else {
            pow.push(gen.next_block(&miner_addr));
        }
        genb = gen.clone();
    }

    for i in 0..REGTEST_BLOCK_BYTES_N {
        let bytes = pow[i].zcash_serialize_to_vec().unwrap();
        std::fs::write(dir.join(format!("test_pow_block_{}.bin", POW_FILE_IDX[i])), bytes).unwrap();
    }

    let (pos_h, fat_ptr) = (&mut 0, &mut FatPointerToBftBlock::null());
    for (i, &link) in REGTEST_POW_IDX_FINALIZED_BY_POS_BLOCK.iter().enumerate() {
        let bft = next_pos(pos_h, fat_ptr, &pow[link + 1..link + 4], &[]);
        let bytes = bft.zcash_serialize_to_vec().unwrap();
        std::fs::write(dir.join(format!("test_pos_block_{}.bin", POS_FILE_IDX[i])), bytes).unwrap();
    }

    println!(
        "regenerated {} pow + {} pos blocks in {dir:?}",
        REGTEST_BLOCK_BYTES_N,
        POS_FILE_IDX.len()
    );
}

#[test]
fn crosslink_expect_pos_height_on_boot() {
    set_test_name(function_name!());
    let mut tf = TF::new(&HARNESS_PARAMETERS);

    tf.push_instr_expect_pos_chain_length(0, 0);

    test_bytes(tf.write_to_bytes());
}

#[test]
fn crosslink_expect_pow_height_on_boot() {
    set_test_name(function_name!());
    let mut tf = TF::new(&HARNESS_PARAMETERS);

    tf.push_instr_expect_pow_chain_length(1, 0);

    test_bytes(tf.write_to_bytes());
}

#[test]
fn crosslink_expect_first_pow_to_not_be_a_no_op() {
    set_test_name(function_name!());
    let mut tf = TF::new(&HARNESS_PARAMETERS);

    tf.push_instr_load_pow_bytes(REGTEST_BLOCK_BYTES[0], 0);
    tf.push_instr_expect_pow_chain_length(2, 0);

    test_bytes(tf.write_to_bytes());
}

#[test]
fn crosslink_push_example_pow_chain_only() {
    set_test_name(function_name!());
    let mut tf = TF::new(&HARNESS_PARAMETERS);

    for i in 0..REGTEST_BLOCK_BYTES.len() {
        tf.push_instr_load_pow_bytes(REGTEST_BLOCK_BYTES[i], 0);
        tf.push_instr_expect_pow_chain_length(2 + i - (i >= 3) as usize, 0);
    }
    tf.push_instr_expect_pow_chain_length(1 - 1 + REGTEST_BLOCK_BYTES.len(), 0);

    test_bytes(tf.write_to_bytes());
}

#[test]
fn crosslink_push_example_pow_chain_each_block_twice() {
    set_test_name(function_name!());
    let mut tf = TF::new(&HARNESS_PARAMETERS);

    for i in 0..REGTEST_BLOCK_BYTES.len() {
        tf.push_instr_load_pow_bytes(REGTEST_BLOCK_BYTES[i], 0);
        // Re-submitting a committed block is idempotent, not an error: ingest answers
        // IngestOutcome::Known, so the load SUCCEEDS. What must not happen is the chain
        // growing, which the length expectation below already asserts.
        tf.push_instr_load_pow_bytes(REGTEST_BLOCK_BYTES[i], 0);
        tf.push_instr_expect_pow_chain_length(2 + i - (i >= 3) as usize, 0);
    }
    tf.push_instr_expect_pow_chain_length(1 - 1 + REGTEST_BLOCK_BYTES.len(), 0);

    test_bytes(tf.write_to_bytes());
}

#[test]
fn crosslink_push_example_pow_chain_again_should_not_change_the_pow_chain_length() {
    set_test_name(function_name!());
    let mut tf = TF::new(&HARNESS_PARAMETERS);

    for i in 0..REGTEST_BLOCK_BYTES.len() {
        tf.push_instr_load_pow_bytes(REGTEST_BLOCK_BYTES[i], 0);
        tf.push_instr_expect_pow_chain_length(2 + i - (i >= 3) as usize, 0);
    }
    tf.push_instr_expect_pow_chain_length(1 - 1 + REGTEST_BLOCK_BYTES.len(), 0);

    // Replaying the entire chain a second time: every block is already known, so each load
    // succeeds idempotently and the length must stay put. See the note above.
    for i in 0..REGTEST_BLOCK_BYTES.len() {
        tf.push_instr_load_pow_bytes(REGTEST_BLOCK_BYTES[i], 0);
        tf.push_instr_expect_pow_chain_length(1 - 1 + REGTEST_BLOCK_BYTES.len(), 0);
    }

    test_bytes(tf.write_to_bytes());
}

#[test]
fn crosslink_expect_pos_not_pushed_if_pow_blocks_not_present() {
    set_test_name(function_name!());
    let mut tf = TF::new(&HARNESS_PARAMETERS);

    tf.push_instr_load_pos_bytes(REGTEST_POS_BLOCK_BYTES[0], SHOULD_FAIL);
    tf.push_instr_expect_pos_chain_length(0, 0);

    test_bytes(tf.write_to_bytes());
}

#[test]
fn crosslink_expect_pos_height_after_push() {
    set_test_name(function_name!());
    let mut tf = TF::new(&HARNESS_PARAMETERS);

    let nw = regtest_network(&HARNESS_PARAMETERS);
    let miner_addr = Address::decode(&nw, "t27eWDgjFYJGVXmzrXeVjnb5J3uXDM9xH9v").unwrap();
    let mut gen = BlockGen::init_at_genesis_plus_1(nw, BlockGen::REGTEST_GENESIS_HASH, &miner_addr);

    let mut pow_common = vec![gen.tip.clone()];
    for _ in 2..8 {
        pow_common.push(gen.next_block(&miner_addr));
    }
    for pow in &pow_common {
        tf.push_instr_load_pow(pow, 0);
    }
    tf.push_instr_expect_pow_chain_length(8, 0);

    let (pos_h, fat_ptr) = (&mut 0, &mut FatPointerToBftBlock::null());
    for i in 1..5 {
        let bft = next_pos(pos_h, fat_ptr, &pow_common[i..i+3], &[]);
        tf.push_instr_load_pos(&bft, 0);
        tf.push_instr_expect_pos_chain_length((*pos_h).try_into().unwrap(), 0);
    }

    // let write_ok = tf.write_to_file(Path::new("crosslink_expect_pos_height_after_push.zeccltf"));
    // assert!(write_ok);
    test_bytes(tf.write_to_bytes());
}

#[test]
fn crosslink_expect_pos_out_of_order() {
    set_test_name(function_name!());
    let mut tf = TF::new(&HARNESS_PARAMETERS);


    let nw = regtest_network(&HARNESS_PARAMETERS);
    let miner_addr = Address::decode(&nw, "t27eWDgjFYJGVXmzrXeVjnb5J3uXDM9xH9v").unwrap();
    let mut gen = BlockGen::init_at_genesis_plus_1(nw, BlockGen::REGTEST_GENESIS_HASH, &miner_addr);

    let mut pow_common = vec![gen.tip.clone()];
    for _ in 2..6 {
        pow_common.push(gen.next_block(&miner_addr));
    }
    for pow in &pow_common {
        tf.push_instr_load_pow(pow, 0);
    }
    tf.push_instr_expect_pow_chain_length(6, 0);

    let (pos_h, fat_ptr) = (&mut 0, &mut FatPointerToBftBlock::null());
    let pos = [
        next_pos(pos_h, fat_ptr, &pow_common[0..=2], &[]),
        next_pos(pos_h, fat_ptr, &pow_common[1..=3], &[]),
        next_pos(pos_h, fat_ptr, &pow_common[2..=4], &[]),
    ];

    tf.push_instr_load_pos(&pos[0], 0);
    tf.push_instr_load_pos(&pos[2], SHOULD_FAIL);
    tf.push_instr_load_pos(&pos[1], 0);
    tf.push_instr_expect_pos_chain_length(2, 0);

    test_bytes(tf.write_to_bytes());
}

#[test]
fn crosslink_expect_pos_push_same_block_twice_only_accepted_once() {
    set_test_name(function_name!());
    let mut tf = TF::new(&HARNESS_PARAMETERS);

    let nw = regtest_network(&HARNESS_PARAMETERS);
    let miner_addr = Address::decode(&nw, "t27eWDgjFYJGVXmzrXeVjnb5J3uXDM9xH9v").unwrap();
    let mut gen = BlockGen::init_at_genesis_plus_1(nw, BlockGen::REGTEST_GENESIS_HASH, &miner_addr);

    let mut pow_common = vec![gen.tip.clone()];
    for _ in 2..4 {
        pow_common.push(gen.next_block(&miner_addr));
    }
    for pow in &pow_common {
        tf.push_instr_load_pow(pow, 0);
    }
    tf.push_instr_expect_pow_chain_length(4, 0);

    let (pos_h, fat_ptr) = (&mut 0, &mut FatPointerToBftBlock::null());
    let pos = next_pos(pos_h, fat_ptr, &pow_common[0..=2], &[]);

    tf.push_instr_load_pos(&pos, 0);
    tf.push_instr_load_pos(&pos, SHOULD_FAIL);
    tf.push_instr_expect_pos_chain_length(1, 0);

    test_bytes(tf.write_to_bytes());
}

#[test]
fn crosslink_reject_pos_with_signature_on_different_data() {
    set_test_name(function_name!());
    let mut tf = TF::new(&HARNESS_PARAMETERS);

    for i in 0..REGTEST_BLOCK_BYTES.len() {
        tf.push_instr_load_pow_bytes(REGTEST_BLOCK_BYTES[i], 0);
    }

    // modify block data so that the signatures are incorrect
    // NOTE: modifying the last as there is no `previous_block_hash` that this invalidates
    let mut bft_block_and_fat_ptr =
        BftBlockAndFatPointerToItWrap::zcash_deserialize(REGTEST_POS_BLOCK_BYTES[0]).unwrap();
    // let mut last_pow_hdr =
    bft_block_and_fat_ptr.0.block.headers.last_mut().unwrap().time += 1;
    let new_bytes = bft_block_and_fat_ptr.zcash_serialize_to_vec().unwrap();

    assert!(
        &new_bytes != REGTEST_POS_BLOCK_BYTES[0],
        "test invalidated if the serialization has not been changed"
    );

    tf.push_instr_load_pos_bytes(&new_bytes, SHOULD_FAIL);
    tf.push_instr_expect_pos_chain_length(0, 0);

    test_bytes(tf.write_to_bytes());
}

/// The finality read path, before and after the first decision (FINALITY.md §7.2).
///
/// Every case of the block-status row is asserted against a populated state through the same
/// [`ReadRequest::CrosslinkBlockFinality`] the RPC issues: a block this node does not hold, a
/// block on the best chain while `fin` is unset, and the same block once `fin` has reached it.
///
/// @Todo: the two finalized-tip RPCs return `fin` itself, which this reads only through the
/// block status above it. Asserting on that request directly needs a test-format instruction
/// stage 3 did not define; a JSON-RPC level test needs a harness that does not exist.
#[test]
fn crosslink_finality_reads_before_and_after_the_first_decision() {
    set_test_name(function_name!());
    let mut tf = TF::new(&HARNESS_PARAMETERS);

    let (pos_h, fat_ptr) = (&mut 0, &mut FatPointerToBftBlock::null());
    let network = regtest_network(&HARNESS_PARAMETERS);
    let miner_addr = Address::decode(&network, "t27eWDgjFYJGVXmzrXeVjnb5J3uXDM9xH9v").unwrap();
    let mut gen =
        BlockGen::init_at_genesis_plus_1(network, BlockGen::REGTEST_GENESIS_HASH, &miner_addr);

    // A hash no block has: the absent case of the block-status row.
    let unknown = zebra_chain::block::Hash([0x11u8; 32]);
    tf.push_instr_expect_pow_block_finality(
        &unknown,
        Some(TFLBlockFinality::CantBeFinalized),
        0,
    );

    let mut pow = vec![gen.tip.clone()];
    for _ in 2..=8 {
        pow.push(gen.next_block(&miner_addr));
    }
    for block in &pow {
        tf.push_instr_load_pow(block, 0);
    }

    // Nothing has decided, so `fin` is unset and every block this node holds is above it.
    for block in &pow {
        tf.push_instr_expect_pow_block_finality(
            &block.hash(),
            Some(TFLBlockFinality::NotYetFinalized),
            0,
        );
    }
    tf.push_instr_expect_pow_block_finality(
        &unknown,
        Some(TFLBlockFinality::CantBeFinalized),
        0,
    );

    // Decide over pow[4..7], whose snapshot is pow[3], and cite it: `fin` reaches pow[3].
    let bft = next_pos(pos_h, fat_ptr, &pow[4..7], &[]);
    tf.push_instr_load_pos(&bft, 0);
    gen.next_block(&miner_addr);
    pow.push(point_tip_at_bft(&mut gen, &bft.0.fat_ptr));
    tf.push_instr_load_pow(pow.last().unwrap(), 0);

    for (i, block) in pow.iter().enumerate() {
        let finality = if i <= 3 {
            TFLBlockFinality::Finalized
        } else {
            TFLBlockFinality::NotYetFinalized
        };
        tf.push_instr_expect_pow_block_finality(&block.hash(), Some(finality), 0);
    }
    tf.push_instr_expect_pow_block_finality(
        &unknown,
        Some(TFLBlockFinality::CantBeFinalized),
        0,
    );

    test_bytes(tf.write_to_bytes());
}

#[test]
fn crosslink_test_basic_finality() {
    set_test_name(function_name!());
    let mut tf = TF::new(&HARNESS_PARAMETERS);

    let (pos_h, fat_ptr) = (&mut 0, &mut FatPointerToBftBlock::null());
    let network = regtest_network(&HARNESS_PARAMETERS);
    let miner_addr = Address::decode(&network, "t27eWDgjFYJGVXmzrXeVjnb5J3uXDM9xH9v").unwrap();
    let mut gen =
        BlockGen::init_at_genesis_plus_1(network, BlockGen::REGTEST_GENESIS_HASH, &miner_addr);

    // A second, distinct, valid transparent P2PKH miner (a different coinbase => different
    // block hashes, so the fork actually competes). Tex addresses are rejected by the
    // Ironwood v6 coinbase builder ("Address not supported for miner rewards").
    let miner_addr2 = zcash_keys::address::Address::Transparent(
        zcash_transparent::address::TransparentAddress::PublicKeyHash([1u8; 20]),
    );
    let n = 18; // TODO: this fails with higher numbers due to block verification
    let mut pow = vec![gen.tip.clone()];
    let _side: Vec<Arc<Block>> = vec![];
    let mut genb = gen.clone();
    let mut side_gen = None;
    for i2 in 1..n+1 {
        if i2 == 2 {
            side_gen = Some(genb.clone());
            pow.push(genb.next_block(&miner_addr2));
        } else {
            pow.push(gen.next_block(&miner_addr));
        }
        genb = gen.clone()
    }

    // pow[2] and pow[3] are siblings of equal work, and the fork choice breaks that tie on the
    // greater tip hash (Chain::cmp). The checks below expect pow[3] to win. Block hashes move with
    // anything in the coinbase, so pick a side miner that makes pow[2] lose instead of relying on
    // how the hashes happen to fall.
    let mut side_miner_byte = 1u8;
    while pow[2].hash().0 > pow[3].hash().0 {
        side_miner_byte += 1;
        let side_miner = zcash_keys::address::Address::Transparent(
            zcash_transparent::address::TransparentAddress::PublicKeyHash([side_miner_byte; 20]),
        );
        pow[2] = side_gen.clone().expect("pow[2] is the side block").next_block(&side_miner);
    }

    // Nothing is loaded yet, so every one of these is a block this node does not hold, which
    // the FINALITY.md §7.2 table answers the same way as a block off the best chain.
    for i2 in 0..n {
        tf.push_instr_expect_pow_block_finality(
            &pow[i2].hash(),
            Some(TFLBlockFinality::CantBeFinalized),
            0,
        );
    }

    for i in 0..n {
        tf.push_instr_load_pow(&pow[i], 0);

        for i2 in 0..n {
            // `pow[2]` and `pow[3]` are the two children of `pow[1]`, and the fork choice takes
            // `pow[3]`, so `pow[2]` is off the best chain from the moment its sibling arrives.
            let finality = if i2 > i || (i2 == 2 && i >= 3) {
                Some(TFLBlockFinality::CantBeFinalized)
            } else {
                Some(TFLBlockFinality::NotYetFinalized)
            };
            tf.push_instr_expect_pow_block_finality(&pow[i2].hash(), finality, 0);
        }
    }

    // The loop above stops at `pow[n - 1]`, but the last window below reaches `pow[n]`. Tail
    // Confirmation needs the block at the topmost header to be present, or validation defers
    // with `NeedsBlock` (FINALITY.md §3.4), so load it here. Nothing asserts on it: the
    // expectation loops cover `pow[0..n]`.
    tf.push_instr_load_pow(&pow[n], 0);

    const LINKS: &[usize] = &[1, 4, 6, 10, 13, 16];//, 18];
    for i in 0..LINKS.len() {
        // `pow[2]` and `pow[3]` are both children of `pow[1]`: the fork this test is built
        // around. A window containing both is not a chain, so it fails Tail Confirmation
        // (FINALITY.md §3.4). The first window is the only one that straddles the fork, and it
        // takes the main-chain blocks either side of the orphan instead. Its snapshot is still
        // `parent(pow[1]) = pow[0]`, so the finality this loop expects is unchanged.
        let window: Vec<Arc<Block>> = if LINKS[i] == 1 {
            vec![pow[1].clone(), pow[3].clone(), pow[4].clone()]
        } else {
            pow[LINKS[i]..LINKS[i] + 3].to_vec()
        };
        let bft = next_pos(pos_h, fat_ptr, &window, &[]);
        tf.push_instr_load_pos(&bft, 0);

        // A decision alone moves nothing (FINALITY.md §4.3): `fin` follows `candidate(bc_best)`,
        // so the chain has to cite the decision before the blocks below its snapshot are final.
        // These blocks sit above `pow[n]`, so they change none of the expectations below.
        gen.next_block(&miner_addr);
        tf.push_instr_load_pow(&point_tip_at_bft(&mut gen, &bft.0.fat_ptr), 0);

        for i2 in 0..n {
            let finality = if i2 == 2 {
                // The branch the fork choice did not take.
                Some(TFLBlockFinality::CantBeFinalized)
            } else if i2 < LINKS[i] {
                // A BFT block over `pow[k..k+3]` finalizes the PARENT of its deepest header --
                // the carried headers are the sigma confirmations above the snapshot -- so the
                // finalized run ends one block below the window, at pow[LINKS[i] - 1].
                Some(TFLBlockFinality::Finalized)
            } else {
                Some(TFLBlockFinality::NotYetFinalized)
            };
            tf.push_instr_expect_pow_block_finality(&pow[i2].hash(), finality, 0);
        }
    }

    test_bytes(tf.write_to_bytes());
}

/// A BFT block assembled straight from PoW blocks, bypassing `BftBlock::try_from` and the header
/// count it enforces, with the fat pointer recomputed from the result so that the only thing wrong
/// with the block is what the test made wrong. The Tail Confirmation tests need blocks
/// `create_pos_and_ptr_to_finalize_pow` will not build.
fn pos_from_headers(
    bft_height: u32,
    parent_fat_ptr: FatPointerToBftBlock,
    pow_blocks: &[Arc<Block>],
) -> BftBlockAndFatPointerToItWrap {
    let block = BftBlock {
        version: 1,
        height: bft_height,
        previous_block_fat_ptr: parent_fat_ptr,
        headers: pow_blocks
            .iter()
            .map(|b| zebra_crosslink::bc_hdr_to_lrz(b.header.as_ref()))
            .collect(),
        slashforks: Vec::new(),
        do_not_include_until_bc_height: 0,
    };
    BftBlockAndFatPointerToItWrap(BftBlockAndFatPointerToIt::from_parts(
        block,
        bft_height.into(),
        1,
        &[],
    ))
}

/// Genesis + `n` blocks from the standard test miner, each loaded into `tf` in order.
/// `pow[i]` is the block at height `i + 1`, so `pow[4]` is P5.
fn pow_chain_for(tf: &mut TF, n: usize) -> (BlockGen, Address, Vec<Arc<Block>>) {
    let network = regtest_network(&HARNESS_PARAMETERS);
    let miner_addr = Address::decode(&network, "t27eWDgjFYJGVXmzrXeVjnb5J3uXDM9xH9v").unwrap();
    let mut gen =
        BlockGen::init_at_genesis_plus_1(network, BlockGen::REGTEST_GENESIS_HASH, &miner_addr);
    let mut pow = vec![gen.tip.clone()];
    for _ in 1..n {
        pow.push(gen.next_block(&miner_addr));
    }
    for block in &pow {
        tf.push_instr_load_pow(block, 0);
    }
    (gen, miner_addr, pow)
}

/// Tail Confirmation (FINALITY.md §3.4): `headers_bc` holds exactly `sigma` headers. One short is
/// not the tail of anything, and the shortfall is the only defect: the headers that are there are
/// consecutive and their blocks are on the chain.
#[test]
fn crosslink_reject_pos_block_with_lt_sigma_headers() {
    set_test_name(function_name!());
    let mut tf = TF::new(&HARNESS_PARAMETERS);
    let sigma = HARNESS_PARAMETERS.bc_confirmation_depth_sigma as usize;
    let (_gen, _miner_addr, pow) = pow_chain_for(&mut tf, 8);

    // P5..P7 would be the sigma-block tail above P4; this stops one header short of it.
    let short = pos_from_headers(0, FatPointerToBftBlock::null(), &pow[4..3 + sigma]);
    assert_eq!(short.0.block.headers.len(), sigma - 1);
    tf.push_instr_load_pos(&short, SHOULD_FAIL);
    tf.push_instr_expect_pos_chain_length(0, 0);

    test_bytes(tf.write_to_bytes());
}

/// Tail Confirmation (FINALITY.md §3.4): the `sigma` headers form a chain, each naming the one
/// below it. These are `sigma` real headers off one chain with a gap in the middle, so the count
/// is right and every block named is on the chain -- only the linkage is broken.
#[test]
fn crosslink_reject_pos_block_with_unlinked_headers() {
    set_test_name(function_name!());
    let mut tf = TF::new(&HARNESS_PARAMETERS);
    assert_eq!(
        HARNESS_PARAMETERS.bc_confirmation_depth_sigma, 3,
        "the header window below is written for sigma = 3"
    );
    let (_gen, _miner_addr, pow) = pow_chain_for(&mut tf, 8);

    // P5, P7, P8: P7 names P6 as its parent, not P5.
    let unlinked = pos_from_headers(
        0,
        FatPointerToBftBlock::null(),
        &[pow[4].clone(), pow[6].clone(), pow[7].clone()],
    );
    tf.push_instr_load_pos(&unlinked, SHOULD_FAIL);
    tf.push_instr_expect_pos_chain_length(0, 0);

    test_bytes(tf.write_to_bytes());
}

/// Linearity (FINALITY.md §3.4): `snapshot(parent(B)) ⪯bc snapshot(B)`. The second block carries
/// the window one block lower than the first, so its snapshot is the first snapshot's parent --
/// below it on the same chain rather than above it.
#[test]
fn crosslink_reject_pos_block_that_regresses_the_snapshot() {
    set_test_name(function_name!());
    let mut tf = TF::new(&HARNESS_PARAMETERS);
    let (_gen, _miner_addr, pow) = pow_chain_for(&mut tf, 8);

    let (pos_h, fat_ptr) = (&mut 0, &mut FatPointerToBftBlock::null());
    // Headers P5..P7, so the snapshot is P4.
    let bft0 = next_pos(pos_h, fat_ptr, &pow[4..7], &[]);
    tf.push_instr_load_pos(&bft0, 0);

    // Headers P4..P6, so the snapshot is P3. Everything else about the block is well formed:
    // the parent pointer, the height and the header window are all what the chain expects.
    let regressed = create_pos_and_ptr_to_finalize_pow(*pos_h, fat_ptr.clone(), &pow[3..6], &[]);
    tf.push_instr_load_pos(&regressed, SHOULD_FAIL);
    tf.push_instr_expect_pos_chain_length(1, 0);

    test_bytes(tf.write_to_bytes());
}

/// With BFT bootstrapped from the chain, a PoW block at or below the activation height may not point
/// at a BFT block. Every other scenario supplies BFT directly (`HARNESS_PARAMETERS`) and so never
/// meets this rule; this one runs the prototype's bootstrap so the rule itself stays tested.
#[test]
fn crosslink_reject_fat_pointer_below_bootstrap_activation() {
    set_test_name(function_name!());
    let mut tf = TF::new(&BOOTSTRAP_HARNESS_PARAMETERS);

    let (pos_h, fat_ptr) = (&mut 0, &mut FatPointerToBftBlock::null());
    let network = regtest_network(&BOOTSTRAP_HARNESS_PARAMETERS);
    let miner_addr = Address::decode(&network, "t27eWDgjFYJGVXmzrXeVjnb5J3uXDM9xH9v").unwrap();
    let mut gen =
        BlockGen::init_at_genesis_plus_1(network, BlockGen::REGTEST_GENESIS_HASH, &miner_addr);
    let mut pow = vec![gen.tip.clone()];
    tf.push_instr_load_pow(&gen.tip, 0);
    for _ in 2..5 {
        pow.push(gen.next_block(&miner_addr));
        tf.push_instr_load_pow(&gen.tip, 0);
    }

    let bft = next_pos(pos_h, fat_ptr, &pow[1..4], &[]);
    tf.push_instr_load_pos(&bft, 0);

    let fat_pointer_to_bft_block = FatPointerToBftBlock {
        vote_for_block_without_finalizer_public_key: bft.0.fat_ptr.vote_for_block_without_finalizer_public_key,
        signatures: bft
            .0
            .fat_ptr
            .signatures
            .iter()
            .map(|sig| FatPointerSignature { pub_key: sig.pub_key, vote_signature: sig.vote_signature })
            .collect(),
    };

    // Height 5 points at that BFT block, far below the prototype's activation height.
    pow.push(gen.next_block(&miner_addr));
    gen.tip = Arc::new(Block {
        header: Arc::new(BlockHeader {
            version: 5,
            fat_pointer_to_bft_block,
            ..*gen.tip.header
        }),
        ..gen.tip.as_ref().clone()
    });
    tf.push_instr_load_pow(&gen.tip, SHOULD_FAIL);
    tf.push_instr_expect_pow_chain_length(5, 0);

    test_bytes(tf.write_to_bytes());
}

#[test]
fn crosslink_test_pow_to_pos_link() {
    set_test_name(function_name!());
    let mut tf = TF::new(&HARNESS_PARAMETERS);

    let (pos_h, fat_ptr) = (&mut 0, &mut FatPointerToBftBlock::null());
    let network = regtest_network(&HARNESS_PARAMETERS);
    let miner_addr = Address::decode(&network, "t27eWDgjFYJGVXmzrXeVjnb5J3uXDM9xH9v").unwrap();
    let mut gen =
        BlockGen::init_at_genesis_plus_1(network, BlockGen::REGTEST_GENESIS_HASH, &miner_addr);
    let mut pow = vec![gen.tip.clone()];
    tf.push_instr_load_pow(&gen.tip, 0);

    for _ in 2..5 {
        pow.push(gen.next_block(&miner_addr));
        tf.push_instr_load_pow(&gen.tip, 0);
    }

    // TODO: push

    let bft = next_pos(pos_h, fat_ptr, &pow[1..4], &[]);
    tf.push_instr_load_pos(&bft, 0);

    let fat_pointer_to_bft_block = FatPointerToBftBlock {
        vote_for_block_without_finalizer_public_key: bft
            .0.fat_ptr
            .vote_for_block_without_finalizer_public_key,
            signatures: bft
                .0.fat_ptr
                .signatures
                .iter()
                .map(|sig| FatPointerSignature {
                    pub_key: sig.pub_key,
                    vote_signature: sig.vote_signature,
                })
        .collect(),
    };

    pow.push(gen.next_block(&miner_addr));
    gen.tip = Arc::new(Block {
        header: Arc::new(BlockHeader {
            version: 5,
            fat_pointer_to_bft_block: fat_pointer_to_bft_block.clone(),
            ..*gen.tip.header
        }),
        ..gen.tip.as_ref().clone()
    });
    tf.push_instr_load_pow(&gen.tip, 0);

    pow.push(gen.next_block(&miner_addr));
    gen.tip = Arc::new(Block {
        header: Arc::new(BlockHeader {
            version: 5,
            fat_pointer_to_bft_block,
            ..*gen.tip.header
        }),
        ..gen.tip.as_ref().clone()
    });
    tf.push_instr_load_pow(&gen.tip, 0);

    test_bytes(tf.write_to_bytes());
}

#[test]
fn crosslink_reject_pow_chain_fork_that_is_competing_against_a_shorter_finalized_pow_chain() {
    set_test_name(function_name!());
    let mut tf = TF::new(&HARNESS_PARAMETERS);

    let (pos_h, fat_ptr) = (&mut 0, &mut FatPointerToBftBlock::null());
    let network = regtest_network(&HARNESS_PARAMETERS);
    let miner_addr = Address::decode(&network, "t27eWDgjFYJGVXmzrXeVjnb5J3uXDM9xH9v").unwrap();
    let mut gen =
        BlockGen::init_at_genesis_plus_1(network, BlockGen::REGTEST_GENESIS_HASH, &miner_addr);
    let mut pow = vec![gen.tip.clone()];
    tf.push_instr_load_pow(&gen.tip, 0);

    for _ in 2..9 {
        pow.push(gen.next_block(&miner_addr));
        tf.push_instr_load_pow(&gen.tip, 0);
    }

    for i in 0..3 {
        let bft = next_pos(pos_h, fat_ptr, &pow[2*i..2*i+3], &[]);
        tf.push_instr_load_pos(&bft, 0);
    }

    for _ in 9..10 {
        pow.push(gen.next_block(&miner_addr));
        tf.push_instr_load_pow(&gen.tip, 0);
    }
    let mut genb = gen.clone(); // fork

    let bft = next_pos(pos_h, fat_ptr, &pow[6..9], &[]);
    tf.push_instr_load_pos(&bft, 0);

    for _ in 10..14 {
        pow.push(gen.next_block(&miner_addr));
        tf.push_instr_load_pow(&gen.tip, 0);
    }

    // Snapshot h10 = pow[9]: the block right above the fork point, so the fork conflicts with
    // the finalized chain. Its three headers are h11..h13.
    let bft = next_pos(pos_h, fat_ptr, &pow[10..13], &[]);
    tf.push_instr_load_pos(&bft, 0);

    // h14 cites that decision. The decision alone finalizes nothing (FINALITY.md §4.3): `fin`
    // reaches h10 only once the best chain carries the fat pointer, and it is `fin` that the
    // fork below has to conflict with.
    gen.next_block(&miner_addr);
    pow.push(point_tip_at_bft(&mut gen, &bft.0.fat_ptr));
    tf.push_instr_load_pow(pow.last().unwrap(), 0);

    // A second, distinct, valid transparent P2PKH miner (a different coinbase => different
    // block hashes, so the fork actually competes). Tex addresses are rejected by the
    // Ironwood v6 coinbase builder ("Address not supported for miner rewards").
    let miner_addr2 = zcash_keys::address::Address::Transparent(
        zcash_transparent::address::TransparentAddress::PublicKeyHash([1u8; 20]),
    );
    // Refused at its first block; the rest wait on a parent that never commits.
    for height in 10..18 {
        let flags = if height == 10 { SHOULD_FAIL } else { SHOULD_DEFER };
        tf.push_instr_load_pow(&genb.next_block(&miner_addr2), flags);
    }
    tf.push_instr_expect_pow_chain_length(15, 0);

    test_bytes(tf.write_to_bytes());
}

// The fork-choice floor of FINALITY.md §4.3: a decision does not move the node, `fin` does, and
// `fin` moves only where the best chain changes. The old shape of this test asserted the
// opposite -- that deciding a snapshot collapsed the node onto that branch -- which is the
// behaviour §4.3 replaces.
#[test]
fn crosslink_pow_follows_the_heaviest_chain_until_fin_moves_to_the_decided_branch() {
    set_test_name(function_name!());
    let mut tf = TF::new(&HARNESS_PARAMETERS);

    let (pos_h, fat_ptr) = (&mut 0, &mut FatPointerToBftBlock::null());
    let network = regtest_network(&HARNESS_PARAMETERS);
    let miner_addr = Address::decode(&network, "t27eWDgjFYJGVXmzrXeVjnb5J3uXDM9xH9v").unwrap();
    // A second, distinct, valid transparent P2PKH miner (a different coinbase => different
    // block hashes, so the fork actually competes). Tex addresses are rejected by the
    // Ironwood v6 coinbase builder ("Address not supported for miner rewards").
    let miner_addr2 = zcash_keys::address::Address::Transparent(
        zcash_transparent::address::TransparentAddress::PublicKeyHash([1u8; 20]),
    );
    let mut gen =
        BlockGen::init_at_genesis_plus_1(network, BlockGen::REGTEST_GENESIS_HASH, &miner_addr);
    let mut pow = vec![gen.tip.clone()];
    tf.push_instr_load_pow(&gen.tip, 0);

    // Heights 1..9, common to both branches.
    for _ in 2..10 {
        pow.push(gen.next_block(&miner_addr));
        tf.push_instr_load_pow(&gen.tip, 0);
    }
    for i in 0..3 {
        let bft = next_pos(pos_h, fat_ptr, &pow[i..i + 3], &[]);
        tf.push_instr_load_pos(&bft, 0);
    }

    // Branch A: heights 10..18, the heaviest chain for most of this test.
    let mut genb = gen.clone(); // fork
    for _ in 10..19 {
        tf.push_instr_load_pow(&gen.next_block(&miner_addr), 0);
    }
    tf.push_instr_expect_pow_chain_length(19, 0);

    // Branch B: heights 10..13, a side chain of four blocks. pow[9..13].
    for _ in 10..14 {
        pow.push(genb.next_block(&miner_addr2));
        tf.push_instr_load_pow(&genb.tip, 0);
    }
    tf.push_instr_expect_pow_chain_length(19, 0);

    // Decide a bft-block whose snapshot is pow[9], branch B's first block: headers 11..13 put
    // the snapshot at their parent. Under §4.3 this moves `bft_final_snapshot` and nothing else
    // -- the node stays on branch A, and the decided block is not final on it.
    let bft = next_pos(pos_h, fat_ptr, &pow[10..13], &[]);
    tf.push_instr_load_pos(&bft, 0);
    tf.push_instr_expect_pow_chain_length(19, 0);
    tf.push_instr_expect_pow_block_finality(
        &pow[9].hash(),
        Some(TFLBlockFinality::CantBeFinalized),
        0,
    );

    // Branch A still extends, because `fin` is below the fork.
    tf.push_instr_load_pow(&gen.next_block(&miner_addr), 0);
    tf.push_instr_expect_pow_chain_length(20, 0);

    let fat_pointer_to_bft_block = FatPointerToBftBlock {
        vote_for_block_without_finalizer_public_key: bft
            .0
            .fat_ptr
            .vote_for_block_without_finalizer_public_key,
        signatures: bft
            .0
            .fat_ptr
            .signatures
            .iter()
            .map(|sig| FatPointerSignature {
                pub_key: sig.pub_key,
                vote_signature: sig.vote_signature,
            })
            .collect(),
    };

    // Branch B overtakes: heights 14..21, the last of them citing the decided bft-block. Only
    // the tip's pointer matters, since `candidate` reads the best tip.
    for _ in 14..21 {
        pow.push(genb.next_block(&miner_addr2));
        tf.push_instr_load_pow(&genb.tip, 0);
    }
    genb.next_block(&miner_addr2);
    genb.tip = Arc::new(Block {
        header: Arc::new(BlockHeader {
            version: 5,
            fat_pointer_to_bft_block,
            ..*genb.tip.header
        }),
        ..genb.tip.as_ref().clone()
    });
    pow.push(genb.tip.clone());
    tf.push_instr_load_pow(&genb.tip, 0);

    // Branch B is now the best chain and its tip cites the decision, so `fin` reaches pow[9].
    tf.push_instr_expect_pow_chain_length(22, 0);
    tf.push_instr_expect_pow_block_finality(&pow[9].hash(), Some(TFLBlockFinality::Finalized), 0);

    // With `fin` on branch B, branch A is gone: a block extending it forks below the finalized
    // tip, which is the fork-choice floor as Zebra enforces it (§4.3, §6.3).
    tf.push_instr_load_pow(&gen.next_block(&miner_addr), SHOULD_DEFER);
    tf.push_instr_expect_pow_chain_length(22, 0);

    test_bytes(tf.write_to_bytes());
}

/// The depth commit runs past a fork from the decided snapshot: the node drops BFT, records why in
/// its database before that commit, and keeps running as proof of work. Afterwards it asks for no
/// block on the dropped branch, decides nothing more, and still mines and commits on its own chain.
#[test]
fn crosslink_dropping_bft_at_the_reorg_depth_records_it_and_keeps_pow_running() {
    set_test_name(function_name!());
    let mut tf = TF::new(&HARNESS_PARAMETERS);

    let (pos_h, fat_ptr) = (&mut 0, &mut FatPointerToBftBlock::null());
    let network = regtest_network(&HARNESS_PARAMETERS);
    let miner_addr = Address::decode(&network, "t27eWDgjFYJGVXmzrXeVjnb5J3uXDM9xH9v").unwrap();
    let miner_addr2 = zcash_keys::address::Address::Transparent(
        zcash_transparent::address::TransparentAddress::PublicKeyHash([1u8; 20]),
    );
    let mut gen =
        BlockGen::init_at_genesis_plus_1(network, BlockGen::REGTEST_GENESIS_HASH, &miner_addr);
    let mut pow = vec![gen.tip.clone()];
    tf.push_instr_load_pow(&gen.tip, 0);

    // Heights 1..9, common to both branches.
    for _ in 2..10 {
        pow.push(gen.next_block(&miner_addr));
        tf.push_instr_load_pow(&gen.tip, 0);
    }
    for i in 0..3 {
        let bft = next_pos(pos_h, fat_ptr, &pow[i..i + 3], &[]);
        tf.push_instr_load_pos(&bft, 0);
    }

    // Branch B, heights 10..14: pow[9..14], a side chain holding the snapshot that gets decided.
    let mut genb = gen.clone();
    for _ in 10..15 {
        pow.push(genb.next_block(&miner_addr2));
        tf.push_instr_load_pow(&genb.tip, 0);
    }
    // Branch A, heights 10..20: the best chain.
    for _ in 10..21 {
        tf.push_instr_load_pow(&gen.next_block(&miner_addr), 0);
    }

    // Decided: snapshot pow[9], branch B's height 10.
    let decided = next_pos(pos_h, fat_ptr, &pow[10..13], &[]);
    tf.push_instr_load_pos(&decided, 0);
    let decided_snapshot = pow[9].hash();
    // The next height on it, snapshot pow[10]. Checked, never stored.
    let next = create_pos_and_ptr_to_finalize_pow(*pos_h, fat_ptr.clone(), &pow[11..14], &[]);

    // Branch A runs on. Height 109 is the first whose commit takes branch A's height 10 into the
    // finalized state, which leaves branch B on no chain this node holds.
    for _ in 21..109 {
        tf.push_instr_load_pow(&gen.next_block(&miner_addr), 0);
    }
    tf.push_instr_expect_bft_dropped(false, None, 0);
    tf.push_instr_check_pos(&next, 0);

    tf.push_instr_load_pow(&gen.next_block(&miner_addr), 0);
    tf.push_instr_expect_bft_dropped(true, Some(&decided_snapshot), 0);

    // Indeterminate, and with no block named: nothing on the dropped branch can arrive.
    tf.push_instr_check_pos(&next, SHOULD_DEFER);
    tf.push_instr_expect_rejection_reason("pending: None", 0);
    tf.push_instr_load_pos(&next, SHOULD_FAIL);
    tf.push_instr_expect_rejection_reason("BFT is dropped", 0);
    tf.push_instr_expect_node_alive(0);

    // Proof of work goes on, from loaded blocks and from the node's own templates.
    tf.push_instr_load_pow(&gen.next_block(&miner_addr), 0);
    tf.push_instr_expect_pow_chain_length(111, 0);
    tf.push_instr_mine_from_template(0);
    tf.push_instr_expect_pow_chain_length(112, 0);

    test_bytes(tf.write_to_bytes());
}

/// A height-0 BFT block whose headers are `tail`'s, relinked onto a snapshot named by `snapshot`
/// that no chain holds, with every header's difficulty set to `bits`.
fn pos_on_a_snapshot_nobody_holds(tail: &[Arc<Block>], snapshot: [u8; 32], bits: Option<u32>) -> BftBlockAndFatPointerToItWrap {
    use zcash_primitives::block::BlockHash as BcBlockHash;
    let mut headers: Vec<_> = tail.iter().map(|b| zebra_crosslink::bc_hdr_to_lrz(b.header.as_ref())).collect();
    for i in 0..headers.len() {
        if let Some(bits) = bits {
            headers[i].bits = bits;
        }
        headers[i].prev_block = if i == 0 { BcBlockHash(snapshot) } else { BcBlockHash::from_header_data(&headers[i - 1]) };
    }
    let mut block = BftBlock::try_from(&HARNESS_PARAMETERS, 0, FatPointerToBftBlock::null(), headers).expect("sigma linked headers");
    block.version = 2;
    BftBlockAndFatPointerToItWrap(BftBlockAndFatPointerToIt::from_parts(block, 0, 0, &[]))
}

/// Proposals naming snapshots that do not exist cannot stall PoW sync. Each one this node
/// validates asks peers for its snapshot by hash; more of them than one peer has download slots
/// used to take every slot, so the next real block was never requested. The wanted set is capped
/// and by-hash requests hold only a share of the budget, so the real block still arrives. A tail
/// whose own headers fail is refused before anything is asked for.
#[test]
fn crosslink_bogus_bft_snapshots_do_not_stall_pow_sync() {
    set_test_name(function_name!());
    let mut tf = TF::new(&HARNESS_PARAMETERS);
    let (mut gen, miner, pow) = pow_chain_for(&mut tf, 10);

    let mut over_the_limit = [0xeeu8; 32];
    over_the_limit[0] = 0;
    tf.push_instr_check_pos(&pos_on_a_snapshot_nobody_holds(&pow[2..5], over_the_limit, Some(0x217f_ffff)), SHOULD_FAIL);
    tf.push_instr_expect_rejection_reason("rejected", 0);

    for n in 0..40u8 {
        let mut snapshot = [n; 32];
        snapshot[31] = 0xa5;
        tf.push_instr_check_pos(&pos_on_a_snapshot_nobody_holds(&pow[2..5], snapshot, None), SHOULD_DEFER);
    }
    tf.push_instr_recv_pow(&gen.next_block(&miner), 0, 0);
    tf.push_instr_expect_pow_chain_length(pow.len() + 2, 0);

    test_bytes(tf.write_to_bytes());
}

/// The bound on wanted snapshots still lets a real one through: a proposal whose snapshot this
/// node lacks asks for it by hash, a peer serves it, and the proposal then validates.
#[test]
fn crosslink_missing_bft_snapshot_is_fetched_by_hash() {
    set_test_name(function_name!());
    let mut tf = TF::new(&HARNESS_PARAMETERS);
    let (gen, _, pow) = pow_chain_for(&mut tf, 10);
    let miner2 = zcash_keys::address::Address::Transparent(
        zcash_transparent::address::TransparentAddress::PublicKeyHash([1u8; 20]),
    );
    // A side branch off the tip that the node has not seen.
    let mut genb = gen.clone();
    let side: Vec<Arc<Block>> = (0..4).map(|_| genb.next_block(&miner2)).collect();
    let proposal = create_pos_and_ptr_to_finalize_pow(0, FatPointerToBftBlock::null(), &side[1..4], &[]);
    assert_eq!(pow.len(), 10);

    tf.push_instr_check_pos(&proposal, SHOULD_DEFER);
    tf.push_instr_expect_rejection_reason("NeedsBlock", 0);
    tf.push_instr_recv_pow(&side[0], 0, 0);
    tf.push_instr_check_pos(&proposal, 0);

    test_bytes(tf.write_to_bytes());
}

// Last Final Snapshot (FINALITY.md §3.1, §6.2): a bc-block may cite a bft-block only if its own
// ancestry contains that bft-block's snapshot. The test needs the decided snapshot to sit on a
// branch the citing block is not on, which is possible only now that deciding does not commit.
#[test]
fn crosslink_reject_pow_block_citing_a_snapshot_off_its_own_chain() {
    set_test_name(function_name!());
    let mut tf = TF::new(&HARNESS_PARAMETERS);

    let (pos_h, fat_ptr) = (&mut 0, &mut FatPointerToBftBlock::null());
    let network = regtest_network(&HARNESS_PARAMETERS);
    let miner_addr = Address::decode(&network, "t27eWDgjFYJGVXmzrXeVjnb5J3uXDM9xH9v").unwrap();
    let miner_addr2 = zcash_keys::address::Address::Transparent(
        zcash_transparent::address::TransparentAddress::PublicKeyHash([1u8; 20]),
    );
    let mut gen =
        BlockGen::init_at_genesis_plus_1(network, BlockGen::REGTEST_GENESIS_HASH, &miner_addr);
    let mut pow = vec![gen.tip.clone()];
    tf.push_instr_load_pow(&gen.tip, 0);

    // Heights 1..9, common to both branches.
    for _ in 2..10 {
        pow.push(gen.next_block(&miner_addr));
        tf.push_instr_load_pow(&gen.tip, 0);
    }
    for i in 0..3 {
        let bft = next_pos(pos_h, fat_ptr, &pow[i..i + 3], &[]);
        tf.push_instr_load_pos(&bft, 0);
    }

    // Branch A, heights 10..13: the best chain, and the one that will cite the bft-block.
    let mut genb = gen.clone(); // fork
    for _ in 10..14 {
        tf.push_instr_load_pow(&gen.next_block(&miner_addr), 0);
    }

    // Branch B, heights 10..13: pow[9..13], holding the snapshot that gets decided.
    for _ in 10..14 {
        pow.push(genb.next_block(&miner_addr2));
        tf.push_instr_load_pow(&genb.tip, 0);
    }

    let bft = next_pos(pos_h, fat_ptr, &pow[10..13], &[]);
    tf.push_instr_load_pos(&bft, 0);
    tf.push_instr_expect_pos_chain_length(4, 0);

    let fat_pointer_to_bft_block = FatPointerToBftBlock {
        vote_for_block_without_finalizer_public_key: bft
            .0
            .fat_ptr
            .vote_for_block_without_finalizer_public_key,
        signatures: bft
            .0
            .fat_ptr
            .signatures
            .iter()
            .map(|sig| FatPointerSignature {
                pub_key: sig.pub_key,
                vote_signature: sig.vote_signature,
            })
            .collect(),
    };

    // Branch A's height 14 cites a bft-block whose snapshot is branch B's height 10. It is σ + 1
    // above that snapshot, so only Last Final Snapshot can refuse it.
    gen.next_block(&miner_addr);
    gen.tip = Arc::new(Block {
        header: Arc::new(BlockHeader {
            version: 5,
            fat_pointer_to_bft_block,
            ..*gen.tip.header
        }),
        ..gen.tip.as_ref().clone()
    });
    tf.push_instr_load_pow(&gen.tip, SHOULD_FAIL);
    tf.push_instr_expect_pow_chain_length(14, 0);

    test_bytes(tf.write_to_bytes());
}

// NOTE: this is very similar to the RPC get_block_template code
#[derive(Clone, Debug)]
struct BlockGen {
    network: Network,
    // NOTE: these roots need updating if we include shielded transactions
    sapling_root: sapling::tree::Root,
    orchard_root: orchard::tree::Root,
    // Ironwood (NU6.3) note-commitment root. Its type is orchard::tree::Root and its value is
    // ignored by the chain-history commitment before NU6.3 (which regtest never reaches), so an
    // empty tree root is correct for every block we generate.
    ironwood_root: orchard::tree::Root,

    history_tree: HistoryTree,

    tip: Arc<Block>,

    // Every output of every generated block not yet spent by a generated block, so a block's
    // coinbase can include its transactions' fees and a test can find coinbase to spend.
    utxos: std::collections::HashMap<zebra_chain::transparent::OutPoint, zebra_chain::transparent::Utxo>,
}

impl BlockGen {
    const REGTEST_GENESIS_HASH: BlockHash = BlockHash([
        0x27, 0xe3, 0x01, 0x34, 0xd6, 0x20, 0xe9, 0xfe, 0x61, 0xf7, 0x19, 0x93, 0x83, 0x20, 0xba,
        0xb6, 0x3e, 0x7e, 0x72, 0xc9, 0x1b, 0x5e, 0x23, 0x02, 0x56, 0x76, 0xf9, 0x0e, 0xd8, 0x11,
        0x9f, 0x02,
    ]);

    #[allow(dead_code)]
    pub fn init_regtest_at_tip(tip: Arc<Block>) -> Self {
        let mut gen = BlockGen {
            network: Network::new_regtest(Default::default()),
            sapling_root: sapling::tree::NoteCommitmentTree::default().root(),
            orchard_root: orchard::tree::NoteCommitmentTree::default().root(),
            ironwood_root: orchard::tree::NoteCommitmentTree::default().root(),
            history_tree: HistoryTree::default(),
            tip: tip.clone(),
            utxos: Default::default(),
        };
        gen.record_block(&tip);
        gen
    }

    pub fn init_at_genesis_plus_1(
        network: Network,
        genesis_hash: BlockHash,
        miner_addr: &Address,
    ) -> Self {
        Self::init_at_genesis_plus_1_with_txs(network, genesis_hash, miner_addr, &[])
    }

    pub fn init_at_genesis_plus_1_with_txs(
        network: Network,
        genesis_hash: BlockHash,
        miner_addr: &Address,
        extra_txs: &[Arc<Transaction>],
    ) -> Self {
        let history_tree = HistoryTree::default();
        let time = chrono::DateTime::<Utc>::from_timestamp(1758127904, 0).expect("valid time");

        // Regtest difficulty is constant; Ironwood v6 expects the same threshold for the
        // first block after genesis as for every later block (the old special-cased value is
        // now rejected as InvalidDifficultyThreshold).
        let difficulty_threshold =
            CompactDifficulty::from_bytes_in_display_order(&[0x20, 0x0f, 0x0f, 0x0f]).unwrap();

        // Nothing precedes this block but genesis, so its transactions spend nothing and pay no fee.
        let tip = BlockGen::create_block(
            &network,
            miner_addr,
            BlockHeight(1),
            genesis_hash,
            time,
            &history_tree,
            difficulty_threshold,
            extra_txs,
            zebra_chain::amount::Amount::zero(),
        );
        let mut gen = BlockGen {
            network,
            history_tree,
            sapling_root: sapling::tree::NoteCommitmentTree::default().root(),
            orchard_root: orchard::tree::NoteCommitmentTree::default().root(),
            ironwood_root: orchard::tree::NoteCommitmentTree::default().root(),
            tip: tip.clone(),
            utxos: Default::default(),
        };
        gen.record_block(&tip);
        gen
    }

    /// Records `block`'s outputs as unspent and removes the outputs it spends.
    fn record_block(&mut self, block: &Block) {
        let height = block.coinbase_height().expect("generated blocks have a height");
        for tx in &block.transactions {
            for outpoint in tx.spent_outpoints() {
                self.utxos.remove(&outpoint);
            }
            let hash = tx.hash();
            for (index, output) in tx.outputs().iter().enumerate() {
                self.utxos.insert(
                    zebra_chain::transparent::OutPoint::from_usize(hash, index),
                    zebra_chain::transparent::Utxo { output: output.clone(), height, from_coinbase: tx.is_coinbase() },
                );
            }
        }
    }

    /// The total fee `txs` pay, which the block's coinbase must include (NU6 onward requires its
    /// outputs to equal the subsidy plus the fees exactly). A transaction whose value doesn't
    /// balance contributes nothing: tests build such transactions to check they are rejected. So
    /// does one that spends an output this generator does not hold, an output already spent for
    /// one: `value_balance` panics on a missing output, it does not return an error.
    fn fees(&self, txs: &[Arc<Transaction>]) -> zebra_chain::amount::Amount<zebra_chain::amount::NonNegative> {
        let mut total = zebra_chain::amount::Amount::zero();
        for tx in txs {
            let spends_an_unheld_output = tx.inputs().iter()
                .filter_map(|input| input.outpoint())
                .any(|outpoint| !self.utxos.contains_key(&outpoint));
            if spends_an_unheld_output { continue; }
            let Ok(value_balance) = tx.value_balance(&self.utxos) else { continue };
            let Ok(fee) = value_balance.remaining_transaction_value() else { continue };
            total = (total + fee).expect("test fees fit in an amount");
        }
        total
    }

    /// The oldest unspent coinbase output paying `key` that the next block may spend.
    fn mature_coinbase_for(&self, key: &TestKey) -> Option<(zebra_chain::transparent::OutPoint, zebra_chain::transparent::Output)> {
        let next_height = self.tip.coinbase_height().expect("generated blocks have a height").0 + 1;
        let lock_script = key.lock_script();
        self.utxos
            .iter()
            .filter(|(_, utxo)| {
                utxo.from_coinbase
                    && utxo.output.lock_script == lock_script
                    && next_height >= utxo.height.0 + zebra_chain::transparent::MIN_TRANSPARENT_COINBASE_MATURITY
            })
            .min_by_key(|(outpoint, utxo)| (utxo.height, outpoint.hash.0, outpoint.index))
            .map(|(outpoint, utxo)| (*outpoint, utxo.output.clone()))
    }

    pub fn create_block(
        network: &Network,
        miner_addr: &Address,
        height: BlockHeight,
        previous_block_hash: BlockHash,
        time: DateTime<Utc>,
        history_tree: &HistoryTree,
        difficulty_threshold: CompactDifficulty,
        extra_txs: &[Arc<Transaction>],
        txs_fee: zebra_chain::amount::Amount<zebra_chain::amount::NonNegative>,
    ) -> Arc<zebra_chain::block::Block> {
        // Build the coinbase the same way the production miner path does. Ironwood v6 removed
        // the old `standard_coinbase_outputs` / `Transaction::new_v*_coinbase` helpers, so we go
        // MinerParams -> TransactionTemplate::new_coinbase -> deserialize. This is
        // network-agnostic (not regtest-specific), which is what the fuzzer needs too. NU6 onward
        // requires coinbase outputs == subsidy + fees exactly, so the caller passes `txs_fee`.
        use zebra_rpc::methods::types::get_block_template::MinerParams;
        use zebra_rpc::methods::types::transaction::TransactionTemplate;
        let mining_config = zebra_rpc::config::mining::Config {
            miner_address: Some(miner_addr.to_zcash_address(network)),
            ..Default::default()
        };
        let miner_params = MinerParams::new(network, mining_config).expect("valid miner params");
        // `data()` is the public derive_getters accessor for the template's serialized bytes; no
        // rpc-crate change is needed to reach the built coinbase transaction.
        let coinbase_tx: Transaction = TransactionTemplate::new_coinbase(
            network,
            height,
            &miner_params,
            txs_fee,
        )
        .expect("valid coinbase template")
        .data()
        .as_ref()
        .zcash_deserialize_into()
        .expect("coinbase template deserializes");

        let mut transactions: Vec<Arc<Transaction>> = Vec::with_capacity(1 + extra_txs.len());
        transactions.push(coinbase_tx.into());
        transactions.extend(extra_txs.iter().cloned());

        // Both roots must cover *every* transaction in the block, not just the coinbase.
        let merkle_root: merkle::Root = transactions.iter().collect();
        let fat_pointer_to_bft_block = FatPointerToBftBlock::null();
        let commitment_bytes =
            BlockGen::commitment_bytes(network, height, history_tree, &transactions, &fat_pointer_to_bft_block);

        Arc::new(Block {
            header: Arc::new(BlockHeader {
                version: 6,
                previous_block_hash,
                merkle_root,
                commitment_bytes: HexDebug(commitment_bytes),
                time,
                difficulty_threshold,
                nonce: HexDebug([0; 32]),
                solution: work::equihash::Solution::Regtest([0; 36]),
                fat_pointer_to_bft_block,
            }),

            transactions,
        })
    }

    /// The commitment a block at `height` with these transactions and this pointer carries, on a
    /// chain whose history up to its parent is `history_tree`. Mirrors the consensus dispatch
    /// (state service check::block_commitment_is_valid_for_chain_history): the Heartwood
    /// activation block carries the reserved all-zero commitment, Heartwood/Canopy carry the raw
    /// history MMR root, and NU5 onward carries the ZIP-244 hashBlockCommitments, which also
    /// commits to the auth data of every transaction and, from FeaturenetUpgrade2, to the pointer.
    fn commitment_bytes(
        network: &Network,
        height: BlockHeight,
        history_tree: &HistoryTree,
        transactions: &[Arc<Transaction>],
        fat_pointer: &FatPointerToBftBlock,
    ) -> [u8; 32] {
        if NetworkUpgrade::Heartwood.activation_height(network) == Some(height) {
            return [0u8; 32];
        }
        match NetworkUpgrade::current(network, height) {
            NetworkUpgrade::Heartwood | NetworkUpgrade::Canopy => {
                <[u8; 32]>::from(history_tree.hash().unwrap_or([0u8; 32].into()))
            }
            _ => {
                let auth_data_root: merkle::AuthDataRoot = transactions.iter().collect();
                ChainHistoryBlockTxAuthCommitmentHash::from_commitments(
                    &history_tree
                        .hash()
                        .expect("history tree is non-empty after the Heartwood activation block"),
                    &auth_data_root,
                    &zebra_chain::block::fat_pointer_digest_at(network, height, fat_pointer),
                )
                .bytes_in_serialized_order()
            }
        }
    }

    pub fn next_block(&mut self, miner_addr: &Address) -> Arc<zebra_chain::block::Block> {
        self.next_block_with_txs(miner_addr, &[])
    }

    /// The network upgrade in effect at the height of the next block, which a transaction for that
    /// block must be signed for.
    fn next_upgrade(&self) -> NetworkUpgrade {
        let height = zebra_chain::block::Height(self.tip.coinbase_height().expect("generated blocks have a height").0 + 1);
        NetworkUpgrade::current(&self.network, height)
    }

    pub fn next_block_with_txs(
        &mut self,
        miner_addr: &Address,
        extra_txs: &[Arc<Transaction>],
    ) -> Arc<zebra_chain::block::Block> {
        // NOTE: it's not completely obvious where this should be done. Having it here allows for
        // tip modification by the user, but means that the history_tree is never visibly
        // up-to-date.
        self.history_tree
            .push(
                &self.network,
                self.tip.clone(),
                zebra_chain::primitives::zcash_history::BlockCommitmentTreeRoots {
                    sapling: &self.sapling_root,
                    orchard: &self.orchard_root,
                    ironwood: &self.ironwood_root,
                },
            )
            .unwrap();

        let height = zebra_chain::block::Height(self.tip.coinbase_height().unwrap().0 + 1);
        let hash = self.tip.header.hash();
        let time = self.tip.header.time + Duration::from_secs(70);

        let difficulty_threshold =
            CompactDifficulty::from_bytes_in_display_order(&[0x20, 0x0f, 0x0f, 0x0f]).unwrap();
        let fees = self.fees(extra_txs);
        self.tip = BlockGen::create_block(
            &self.network,
            miner_addr,
            height,
            hash,
            time,
            &self.history_tree,
            difficulty_threshold,
            extra_txs,
            fees,
        );
        let tip = self.tip.clone();
        self.record_block(&tip);

        self.tip.clone()
    }
}

#[test]
fn crosslink_gen_pow_fork() {
    set_test_name(function_name!());
    let mut tf = TF::new(&HARNESS_PARAMETERS);

    let network = regtest_network(&HARNESS_PARAMETERS);
    let miner_addr = Address::decode(&network, "t27eWDgjFYJGVXmzrXeVjnb5J3uXDM9xH9v").unwrap();
    let mut gen =
        BlockGen::init_at_genesis_plus_1(network, BlockGen::REGTEST_GENESIS_HASH, &miner_addr);
    tf.push_instr_load_pow(&gen.tip, 0);

    for _ in 2..4 {
        tf.push_instr_load_pow(&gen.next_block(&miner_addr), 0);
    }
    let mut genb = gen.clone();

    for _ in 4..7 {
        tf.push_instr_load_pow(&gen.next_block(&miner_addr), 0);
    }
    tf.push_instr_expect_pow_chain_length(7, 0);

    // A second, distinct, valid transparent P2PKH miner (a different coinbase => different
    // block hashes, so the fork actually competes). Tex addresses are rejected by the
    // Ironwood v6 coinbase builder ("Address not supported for miner rewards").
    let miner_addr2 = zcash_keys::address::Address::Transparent(
        zcash_transparent::address::TransparentAddress::PublicKeyHash([1u8; 20]),
    );
    for _ in 4..8 {
        tf.push_instr_load_pow(&genb.next_block(&miner_addr2), 0);
    }
    tf.push_instr_expect_pow_chain_length(8, 0);

    // Result:
    // P  LOAD_POW (1 - f2245bd187293755539ac981d156e0e09a5d5f3e985abae974c71f04d953a2f1, parent: 029f11d80ef9765602235e1bc9727e3eb6ba20839319f761fee920d63401e327)
    // P  LOAD_POW (2 - e01789fab97edbdd127a0162348b1882200ba26043f392bce495362e7c20c354, parent: f2245bd187293755539ac981d156e0e09a5d5f3e985abae974c71f04d953a2f1)
    // P  LOAD_POW (3 - c19351b6534ce047a7da8d3f60f04383206c9f9cd9f6edd51e39c15f32b767ef, parent: e01789fab97edbdd127a0162348b1882200ba26043f392bce495362e7c20c354)
    // P  LOAD_POW (4 - f8214b23b5539cbeb93917d91dd85e7209824429f5aa3905baf273ae7a2d9586, parent: c19351b6534ce047a7da8d3f60f04383206c9f9cd9f6edd51e39c15f32b767ef)
    // P  LOAD_POW (5 - ee3f15e63275d68320e998c148cde1f50c6cb7f61c308cbe65b2fb8bf373b088, parent: f8214b23b5539cbeb93917d91dd85e7209824429f5aa3905baf273ae7a2d9586)
    // P  LOAD_POW (6 - 5360f9cf81a7d40cd4d305efc1a4127ba5fe0e3c7cd3fe9cea952c1186f67709, parent: ee3f15e63275d68320e998c148cde1f50c6cb7f61c308cbe65b2fb8bf373b088)
    // P  EXPECT_POW_CHAIN_LENGTH (7)
    // P  LOAD_POW (4 - 669391145fdbadc09b622657aec6166927efda199b20662e66e5154b5d35ee95, parent: c19351b6534ce047a7da8d3f60f04383206c9f9cd9f6edd51e39c15f32b767ef)
    // P  LOAD_POW (5 - 8fde5bb6d9d7b21795b06da74b574167628fce2cf5f651ddc41d9d33dc76ca8e, parent: 669391145fdbadc09b622657aec6166927efda199b20662e66e5154b5d35ee95)
    // P  LOAD_POW (6 - bbb0e46c50d3730cd217c073b3da08929d33ec8cd5800b5226027c7a48ba016c, parent: 8fde5bb6d9d7b21795b06da74b574167628fce2cf5f651ddc41d9d33dc76ca8e)
    // P  LOAD_POW (7 - 879e7c77dd9c141689ff4f05bb0b80e153639062b5371f2ffced3acaaf563d31, parent: bbb0e46c50d3730cd217c073b3da08929d33ec8cd5800b5226027c7a48ba016c)
    // P  EXPECT_POW_CHAIN_LENGTH (8)

    test_bytes(tf.write_to_bytes());
}

#[test]
fn crosslink_mine_from_template() {
    set_test_name(function_name!());
    let mut tf = TF::new(&HARNESS_PARAMETERS);

    let network = regtest_network(&HARNESS_PARAMETERS);
    let key = TestKey::new(b"crosslink test miner");
    let miner = key.address();
    let mut gen = BlockGen::init_at_genesis_plus_1(network, BlockGen::REGTEST_GENESIS_HASH, &miner);
    tf.push_instr_load_pow(&gen.tip, 0);
    let mut chain_length = 2;
    while gen.mature_coinbase_for(&key).is_none() {
        tf.push_instr_load_pow(&gen.next_block(&miner), 0);
        chain_length += 1;
    }

    // mempool to block the way production does it: the node's own template picks the spend up
    let spend = transparent_spend(gen.next_upgrade(), &key, gen.mature_coinbase_for(&key).expect("matured above"));
    tf.push_instr_submit_tx(&spend, 0);
    tf.push_instr_expect_mempool_contains(&spend, 0);
    tf.push_instr_mine_from_template(0);
    tf.push_instr_expect_mempool_absent(&spend, 0);

    for _ in 0..2 {
        tf.push_instr_mine_from_template(0);
    }
    tf.push_instr_expect_pow_chain_length(chain_length + 3, 0);
    tf.push_instr_expect_node_alive(0);

    test_bytes(tf.write_to_bytes());
}

/// Malformed files and odd blocks are rejected or described, never panicked on: the GUI opens
/// arbitrary files inside a running node, and the instruction dump runs in the panic hook.
/// Needs no node.
#[test]
fn test_format_survives_malformed_files_and_odd_blocks() {
    let mut tf = TF::new(&HARNESS_PARAMETERS);
    let network = regtest_network(&HARNESS_PARAMETERS);
    let miner_addr = Address::decode(&network, "t27eWDgjFYJGVXmzrXeVjnb5J3uXDM9xH9v").unwrap();
    let gen = BlockGen::init_at_genesis_plus_1(network, BlockGen::REGTEST_GENESIS_HASH, &miner_addr);
    let mut no_height = gen.tip.as_ref().clone();
    no_height.transactions.clear();
    tf.push_instr_load_pow(&no_height, SHOULD_FAIL);
    tf.push_instr_ex(9999, 0, &[1, 2, 3], [0; 2]);
    let good = tf.write_to_bytes();

    let parsed = TF::read_from_bytes(&good).expect("a written file reads back");
    let described: Vec<String> = parsed.instrs.iter().map(|instr| TFInstr::string_from_instr(&good, instr)).collect();
    assert!(described.iter().any(|line| line.contains("no height")), "{described:?}");

    // header: magic [0, 8), instrs_o [8, 16), instrs_n [16, 20), instr_size [20, 24);
    // instruction: kind, flags, data.o, data.size at +16, val
    let header = std::mem::size_of::<TFHdr>();
    let instrs_o = u64::from_le_bytes(good[8..16].try_into().unwrap()) as usize;
    let mut wrong_magic = good.clone();
    wrong_magic[0] ^= 1;
    let mut bad_offset = good.clone();
    bad_offset[8..16].copy_from_slice(&u64::MAX.to_le_bytes());
    let mut bad_stride = good.clone();
    bad_stride[20..24].copy_from_slice(&1u32.to_le_bytes());
    let mut data_past_end = good.clone();
    data_past_end[instrs_o + 16..instrs_o + 24].copy_from_slice(&u64::MAX.to_le_bytes());
    for (name, bytes) in [
        ("empty", Vec::new()),
        ("header only", good[..header].to_vec()),
        ("wrong magic", wrong_magic),
        ("instructions outside the file", bad_offset),
        ("wrong instruction size", bad_stride),
        ("data past the end", data_past_end),
        ("truncated", good[..good.len() - 1].to_vec()),
    ] {
        assert!(TF::read_from_bytes(&bytes).is_err(), "{name} was accepted");
    }
}

/// `block` with one byte of its first spend's unlock script flipped. Transaction ids leave out
/// unlock scripts (ZIP-244), so the merkle root and the block hash stay the honest block's;
/// only the auth data commitment, checked at commit, catches it.
fn with_forged_signature(block: &Block) -> Block {
    let mut forged = block.clone();
    let spend_i = forged
        .transactions
        .iter()
        .position(|tx| !tx.is_coinbase() && !tx.inputs().is_empty())
        .expect("the block carries a transparent spend");
    forged.transactions[spend_i] = Arc::new(forged_signature(&forged.transactions[spend_i]));
    forged
}

/// `spend` with one byte of its first unlock script flipped: same txid, invalid signature.
fn forged_signature(spend: &Transaction) -> Transaction {
    let mut tx = spend.clone();
    let Transaction::VCrosslink { inputs, .. } = &mut tx else {
        panic!("post-genesis regtest transactions are VCrosslink");
    };
    let zebra_chain::transparent::Input::PrevOut { unlock_script, .. } = &mut inputs[0] else {
        panic!("a spend's input names a previous output");
    };
    let mut bytes = unlock_script.as_raw_bytes().to_vec();
    bytes[8] ^= 1;
    *unlock_script = zebra_chain::transparent::Script::new(&bytes);
    tx
}

/// Loads blocks mined to `key` until `count` of its coinbase outputs are spendable by the next
/// block, and returns those, oldest first.
fn matured_coinbases(
    tf: &mut TF,
    gen: &mut BlockGen,
    key: &TestKey,
    count: usize,
) -> Vec<(zebra_chain::transparent::OutPoint, zebra_chain::transparent::Output)> {
    let miner = key.address();
    let lock_script = key.lock_script();
    loop {
        let next_height = gen.tip.coinbase_height().expect("generated blocks have a height").0 + 1;
        let mut matured: Vec<_> = gen
            .utxos
            .iter()
            .filter(|(_, utxo)| {
                utxo.from_coinbase
                    && utxo.output.lock_script == lock_script
                    && next_height >= utxo.height.0 + zebra_chain::transparent::MIN_TRANSPARENT_COINBASE_MATURITY
            })
            .map(|(outpoint, utxo)| (utxo.height, *outpoint, utxo.output.clone()))
            .collect();
        if matured.len() >= count {
            matured.sort_by_key(|(height, outpoint, _)| (*height, outpoint.hash.0, outpoint.index));
            return matured.into_iter().take(count).map(|(_, outpoint, output)| (outpoint, output)).collect();
        }
        tf.push_instr_load_pow(&gen.next_block(&miner), 0);
    }
}

/// The mempool verified a transaction while its input was unspent; then a block spends that
/// input with a different transaction. The verdict the mempool reached is stale, and nothing may
/// keep acting on it: the transaction leaves the mempool, a block carrying it is refused, it
/// cannot be submitted again, and the node's next template leaves it out.
#[test]
fn crosslink_mempool_verdict_goes_stale_when_a_block_spends_the_input() {
    set_test_name(function_name!());
    let mut tf = TF::new(&HARNESS_PARAMETERS);

    let network = regtest_network(&HARNESS_PARAMETERS);
    let key = TestKey::new(b"crosslink test miner");
    let miner = key.address();
    let other_key = TestKey::new(b"another key");
    let mut gen = BlockGen::init_at_genesis_plus_1(network, BlockGen::REGTEST_GENESIS_HASH, &miner);
    tf.push_instr_load_pow(&gen.tip, 0);
    let input = matured_coinbases(&mut tf, &mut gen, &key, 1).remove(0);

    let in_mempool = transparent_spend(gen.next_upgrade(), &key, input.clone());
    let fee = mempool_fee(&in_mempool);
    let conflicting = signed_tx(
        gen.next_upgrade(),
        Some(&key),
        &[input.clone()],
        vec![zebra_chain::transparent::Output::new((input.1.value - fee).expect("the input covers the fee"), other_key.lock_script())],
        None,
        0,
    );
    assert_ne!(in_mempool.hash(), conflicting.hash());

    tf.push_instr_submit_tx(&in_mempool, 0);
    tf.push_instr_expect_mempool_contains(&in_mempool, 0);

    let spent_utxo = gen.utxos[&input.0].clone();
    let spends_it_first = gen.next_block_with_txs(&miner, &[conflicting.clone()]);
    tf.push_instr_load_pow(&spends_it_first, 0);
    tf.push_instr_expect_pow_tip(&spends_it_first.hash(), 0);
    tf.push_instr_expect_mempool_absent(&in_mempool, 0);

    // with the input spent, the stale transaction is dead: a block extending the tip with it
    // double-spends, and the mempool refuses it. The generator building that block is told the
    // input is unspent, so the block is well-formed (its coinbase claims the fee) and wrong
    // only in spending it twice.
    let mut double_spend_gen = gen.clone();
    double_spend_gen.utxos.insert(input.0, spent_utxo);
    let double_spend = double_spend_gen.next_block_with_txs(&miner, &[in_mempool.clone()]);
    // Refused, and named for what it is. The node once answered it "already known", taking any
    // commit error whose text contained "Duplicate" for a duplicate block; the state's
    // double-spend errors are DuplicateTransparentSpend and Duplicate*Nullifier.
    tf.push_instr_load_pow_rejected(&double_spend, "DuplicateTransparentSpend");
    tf.push_instr_expect_pow_tip(&spends_it_first.hash(), 0);
    tf.push_instr_expect_pow_tip(&double_spend.hash(), SHOULD_FAIL);
    tf.push_instr_submit_tx(&in_mempool, SHOULD_FAIL);
    tf.push_instr_expect_rejection_reason("inputs were spent", 0);
    tf.push_instr_expect_mempool_absent(&in_mempool, 0);

    tf.push_instr_expect_node_alive(0);

    test_bytes(tf.write_to_bytes());
}

/// A transaction mined on a branch that then loses a reorg is unmined again. The mempool filed
/// it as mined when its block arrived; that record must not outlive the block. It can be
/// submitted again, and the node's own template mines it.
#[test]
fn crosslink_tx_mined_on_a_losing_branch_can_be_mined_again() {
    set_test_name(function_name!());
    let mut tf = TF::new(&HARNESS_PARAMETERS);

    let network = regtest_network(&HARNESS_PARAMETERS);
    let key = TestKey::new(b"crosslink test miner");
    let miner = key.address();
    let other_miner = Address::Transparent(zcash_transparent::address::TransparentAddress::PublicKeyHash([1u8; 20]));
    let mut gen = BlockGen::init_at_genesis_plus_1(network, BlockGen::REGTEST_GENESIS_HASH, &miner);
    tf.push_instr_load_pow(&gen.tip, 0);
    let input = matured_coinbases(&mut tf, &mut gen, &key, 1).remove(0);
    let spend = transparent_spend(gen.next_upgrade(), &key, input);

    tf.push_instr_submit_tx(&spend, 0);
    tf.push_instr_expect_mempool_contains(&spend, 0);
    let mut winning_gen = gen.clone();
    let mined_in = gen.next_block_with_txs(&miner, &[spend.clone()]);
    tf.push_instr_load_pow(&mined_in, 0);
    tf.push_instr_expect_mempool_absent(&spend, 0);
    tf.push_instr_expect_mempool_rejected(&spend, SHOULD_FAIL);

    // a longer branch from the same parent, without the transaction
    tf.push_instr_load_pow(&winning_gen.next_block(&other_miner), 0);
    let winning_tip = winning_gen.next_block(&other_miner);
    tf.push_instr_load_pow(&winning_tip, 0);
    tf.push_instr_expect_pow_tip(&winning_tip.hash(), 0);

    // unmined, so it is wanted again
    tf.push_instr_submit_tx(&spend, 0);
    tf.push_instr_expect_mempool_contains(&spend, 0);
    tf.push_instr_mine_from_template_into(1, 0);
    tf.push_instr_expect_pow_tip_slot(1, 0);
    tf.push_instr_expect_mempool_absent(&spend, 0);
    tf.push_instr_expect_node_alive(0);

    test_bytes(tf.write_to_bytes());
}

/// One transaction spends a chain output, an output still in the mempool, and another chain
/// output, in that order. The outputs it spends must be matched to its inputs by position
/// whether they come from the chain or the mempool (upstream zebra PR 10356 appended the
/// mempool ones last, so every signature check ran against the wrong output). It is verified
/// in the mempool, then again in the block the node's template mines both into.
#[test]
fn crosslink_tx_spending_chain_and_mempool_outputs_in_mixed_order() {
    set_test_name(function_name!());
    let mut tf = TF::new(&HARNESS_PARAMETERS);

    let network = regtest_network(&HARNESS_PARAMETERS);
    let key = TestKey::new(b"crosslink test miner");
    let miner = key.address();
    let mut gen = BlockGen::init_at_genesis_plus_1(network, BlockGen::REGTEST_GENESIS_HASH, &miner);
    tf.push_instr_load_pow(&gen.tip, 0);
    let mut coinbases = matured_coinbases(&mut tf, &mut gen, &key, 3);
    let (second_chain, first_chain, parent_input) = (coinbases.remove(2), coinbases.remove(1), coinbases.remove(0));

    let parent = transparent_spend(gen.next_upgrade(), &key, parent_input);
    let from_mempool = (zebra_chain::transparent::OutPoint::from_usize(parent.hash(), 0), parent.outputs()[0].clone());
    let inputs = [first_chain, from_mempool, second_chain];
    // the three inputs hold different amounts, so a signature checked against the wrong one fails
    assert_ne!(inputs[0].1.value, inputs[1].1.value);
    let total = inputs
        .iter()
        .fold(zebra_chain::amount::Amount::<zebra_chain::amount::NonNegative>::zero(), |sum, (_, output)| {
            (sum + output.value).expect("test amounts fit")
        });
    let outputs = |fee: zebra_chain::amount::Amount<zebra_chain::amount::NonNegative>| {
        vec![zebra_chain::transparent::Output::new((total - fee).expect("the inputs cover the fee"), key.lock_script())]
    };
    let fee = mempool_fee(&signed_tx(gen.next_upgrade(), Some(&key), &inputs, outputs(zebra_chain::amount::Amount::zero()), None, 0));
    let mixed = signed_tx(gen.next_upgrade(), Some(&key), &inputs, outputs(fee), None, 0);

    tf.push_instr_submit_tx(&parent, 0);
    tf.push_instr_expect_mempool_contains(&parent, 0);
    tf.push_instr_submit_tx(&mixed, 0);
    tf.push_instr_expect_mempool_contains(&mixed, 0);

    tf.push_instr_mine_from_template_into(1, 0);
    tf.push_instr_expect_pow_tip_slot(1, 0);
    tf.push_instr_expect_mempool_absent(&parent, 0);
    tf.push_instr_expect_mempool_absent(&mixed, 0);
    tf.push_instr_expect_mempool_rejected(&mixed, SHOULD_FAIL);
    tf.push_instr_expect_node_alive(0);

    test_bytes(tf.write_to_bytes());
}

/// The instructions no scenario used before: RECV_TX, EXPECT_MEMPOOL_REJECTED (including that a
/// mined transaction is not "rejected"), EXPECT_POOL_TOTALS, EXPECT_FINALIZER_BANK, and the
/// load-rejected helper.
#[test]
fn crosslink_mempool_and_staking_state_instructions() {
    set_test_name(function_name!());
    let mut tf = TF::new(&HARNESS_PARAMETERS);

    let network = regtest_network(&HARNESS_PARAMETERS);
    let key = TestKey::new(b"crosslink test miner");
    let miner = key.address();
    let mut gen = BlockGen::init_at_genesis_plus_1(network.clone(), BlockGen::REGTEST_GENESIS_HASH, &miner);
    tf.push_instr_load_pow(&gen.tip, 0);
    let mature = |gen: &mut BlockGen, tf: &mut TF| {
        while gen.mature_coinbase_for(&key).is_none() {
            tf.push_instr_load_pow(&gen.next_block(&miner), 0);
        }
        gen.mature_coinbase_for(&key).expect("matured above")
    };

    // a spend arriving from a peer reaches the mempool and leaves it when mined, and a mined
    // transaction is not a rejected one
    let spend = transparent_spend(gen.next_upgrade(), &key, mature(&mut gen, &mut tf));
    let wire = zebra_network::wire::tx_message_bytes(&network, zebra_chain::transaction::UnminedTx::from(spend.clone()))
        .expect("a transaction frames");
    tf.push_instr_recv_tx_wire(&wire, 0, 0);
    tf.push_instr_expect_mempool_contains(&spend, 0);
    tf.push_instr_load_pow(&gen.next_block_with_txs(&miner, &[spend.clone()]), 0);
    tf.push_instr_expect_mempool_absent(&spend, 0);
    tf.push_instr_expect_mempool_rejected(&spend, SHOULD_FAIL);

    // a forged signature is refused and lands in the rejected set
    let forged = forged_signature(&transparent_spend(gen.next_upgrade(), &key, mature(&mut gen, &mut tf)));
    tf.push_instr_submit_tx(&forged, SHOULD_FAIL);
    tf.push_instr_expect_mempool_rejected(&forged, 0);

    // before any bond the staking pools and the target's bank are empty; a funded bond makes
    // the bonded pool non-empty. The amounts after it depend on the issuance rules, which pay a
    // new bond in its own block, so this checks only that the pool moved.
    let target_key = zebra_crosslink::rng_private_public_key_from_address(b"staking-target").1;
    let target = zcash_primitives::bft::FinalizerAddress::create(&target_key);
    let target_pub_key = target.pub_key.0;
    let input = mature(&mut gen, &mut tf);
    tf.push_instr_expect_pool_totals(0, 0, TEST_STAKE_IGNORED, 0);
    tf.push_instr_expect_finalizer_bank(target_pub_key, 0, 0);
    let bond = staking_tx_create_bond_funded(gen.next_upgrade(), b"funded-bond", target, 100_000_000, &key, input);
    let bond_key = bond.staking_action().expect("a staking transaction").bond_key();
    tf.push_instr_expect_bond(bond_key, TF_BOND_ABSENT, TEST_STAKE_IGNORED, 0);
    tf.push_instr_load_pow(&gen.next_block_with_txs(&miner, &[bond]), 0);
    tf.push_instr_expect_bond(bond_key, TF_BOND_ACTIVE, TEST_STAKE_IGNORED, 0);
    tf.push_instr_expect_pool_totals(0, TEST_STAKE_IGNORED, TEST_STAKE_IGNORED, SHOULD_FAIL);
    // whatever the bond and the target's bank now hold, it is what the chain implies
    tf.push_instr_expect_staking_replay_matches(0);
    tf.push_instr_load_pow(&gen.next_block(&miner), 0);
    tf.push_instr_expect_staking_replay_matches(0);

    // the load-rejected helper: a body that doesn't match the merkle root
    let mut tampered = gen.next_block(&miner).as_ref().clone();
    tampered.transactions.push(tampered.transactions[0].clone());
    tf.push_instr_load_pow_rejected(&tampered, "merkle");

    test_bytes(tf.write_to_bytes());
}

/// RPC methods exist with the parameters clients send. A method that no longer exists answers
/// "Method not found", and one that exists but gets malformed parameters answers "Invalid
/// params", so a method can be checked for existence without its arguments being valid.
/// wallet_staking_action went missing in a merge and nothing failed for two months.
#[test]
fn crosslink_rpc_methods_exist() {
    set_test_name(function_name!());
    let mut tf = TF::new(&HARNESS_PARAMETERS);

    let network = regtest_network(&HARNESS_PARAMETERS);
    let miner_addr = Address::decode(&network, "t27eWDgjFYJGVXmzrXeVjnb5J3uXDM9xH9v").unwrap();
    let mut gen = BlockGen::init_at_genesis_plus_1(network, BlockGen::REGTEST_GENESIS_HASH, &miner_addr);
    tf.push_instr_load_pow(&gen.tip, 0);
    tf.push_instr_load_pow(&gen.next_block(&miner_addr), 0);

    tf.push_instr_call_rpc("getblockcount", "[]", 0);
    tf.push_instr_call_rpc("getbestblockhash", "[]", 0);
    tf.push_instr_call_rpc("no_such_method", "[]", SHOULD_FAIL);
    tf.push_instr_expect_rejection_reason("Method not found", 0);
    tf.push_instr_call_rpc("wallet_staking_action", "[]", SHOULD_FAIL);
    tf.push_instr_expect_rejection_reason("Invalid params", 0);
    tf.push_instr_expect_node_alive(0);

    test_bytes(tf.write_to_bytes());
}

/// A forged body under an honest block's hash is rejected, and must not stop the honest block
/// with that hash from being fetched and committed afterwards (upstream zebra PR 11052: the
/// rejected hash stayed "known" and the honest block was never downloaded).
#[test]
fn crosslink_honest_block_accepted_after_forged_body_with_its_hash() {
    set_test_name(function_name!());
    let mut tf = TF::new(&HARNESS_PARAMETERS);

    let network = regtest_network(&HARNESS_PARAMETERS);
    let key = TestKey::new(b"crosslink test miner");
    let miner = key.address();
    let mut gen = BlockGen::init_at_genesis_plus_1(network, BlockGen::REGTEST_GENESIS_HASH, &miner);
    tf.push_instr_load_pow(&gen.tip, 0);
    let mut chain_length = 2;

    // a body that doesn't match the merkle root: from one peer, then the honest block from another
    let honest = gen.next_block(&miner);
    let mut forged = honest.as_ref().clone();
    forged.transactions.push(forged.transactions[0].clone());
    assert_eq!(forged.hash(), honest.hash());
    tf.push_instr_recv_pow(&forged, 0, SHOULD_FAIL);
    tf.push_instr_expect_rejection_reason("merkle", 0);
    tf.push_instr_recv_pow(&honest, 1, 0);
    chain_length += 1;
    tf.push_instr_expect_pow_chain_length(chain_length, 0);

    // the same, both from one peer
    let honest = gen.next_block(&miner);
    let mut forged = honest.as_ref().clone();
    forged.transactions.push(forged.transactions[0].clone());
    tf.push_instr_recv_pow(&forged, 0, SHOULD_FAIL);
    tf.push_instr_recv_pow(&honest, 0, 0);
    chain_length += 1;
    tf.push_instr_expect_pow_chain_length(chain_length, 0);

    // the same, through the local doorway
    let honest = gen.next_block(&miner);
    let mut forged = honest.as_ref().clone();
    forged.transactions.push(forged.transactions[0].clone());
    tf.push_instr_load_pow(&forged, SHOULD_FAIL);
    tf.push_instr_load_pow(&honest, 0);
    chain_length += 1;
    tf.push_instr_expect_pow_chain_length(chain_length, 0);

    // a forged signature: it passes every check keyed by the hash and fails only late
    for (forged_peer, honest_peer) in [(2, 3), (2, 2)] {
        while gen.mature_coinbase_for(&key).is_none() {
            tf.push_instr_load_pow(&gen.next_block(&miner), 0);
            chain_length += 1;
        }
        let spend = transparent_spend(gen.next_upgrade(), &key, gen.mature_coinbase_for(&key).expect("matured above"));
        let honest = gen.next_block_with_txs(&miner, &[spend]);
        let forged = with_forged_signature(&honest);
        assert_eq!(forged.hash(), honest.hash());
        tf.push_instr_recv_pow(&forged, forged_peer, SHOULD_FAIL);
        tf.push_instr_recv_pow(&honest, honest_peer, 0);
        chain_length += 1;
        tf.push_instr_expect_pow_chain_length(chain_length, 0);
    }

    // and through the local doorway
    while gen.mature_coinbase_for(&key).is_none() {
        tf.push_instr_load_pow(&gen.next_block(&miner), 0);
        chain_length += 1;
    }
    let spend = transparent_spend(gen.next_upgrade(), &key, gen.mature_coinbase_for(&key).expect("matured above"));
    let honest = gen.next_block_with_txs(&miner, &[spend]);
    tf.push_instr_load_pow(&with_forged_signature(&honest), SHOULD_FAIL);
    tf.push_instr_load_pow(&honest, 0);
    chain_length += 1;
    tf.push_instr_expect_pow_chain_length(chain_length, 0);
    tf.push_instr_expect_node_alive(0);

    test_bytes(tf.write_to_bytes());
}

/// The clock is the test's. A block's timestamp is judged against a time the test supplies:
/// asked of the checks directly, which answer at once, and through the node on a supplied clock.
/// Nothing here depends on when the test runs or on how long anything takes.
#[test]
fn crosslink_block_time_is_judged_against_a_supplied_clock() {
    set_test_name(function_name!());
    let mut tf = TF::new(&HARNESS_PARAMETERS);

    let network = regtest_network(&HARNESS_PARAMETERS);
    let miner_addr = Address::decode(&network, "t27eWDgjFYJGVXmzrXeVjnb5J3uXDM9xH9v").unwrap();
    let mut gen = BlockGen::init_at_genesis_plus_1(network, BlockGen::REGTEST_GENESIS_HASH, &miner_addr);
    tf.push_instr_load_pow(&gen.tip, 0);
    let block = gen.next_block(&miner_addr);
    let block_time = block.header.time.timestamp() as u64;
    let three_hours = 3 * 60 * 60;

    // directly: a header more than two hours ahead of the clock is refused, and a body that
    // doesn't match its header is refused by the next stage
    tf.push_instr_check_pow(&block, block_time, 0);
    tf.push_instr_check_pow(&block, block_time - three_hours, SHOULD_FAIL);
    tf.push_instr_expect_rejection_reason("header", 0);
    let mut tampered = block.as_ref().clone();
    tampered.transactions.push(tampered.transactions[0].clone());
    tf.push_instr_check_pow(&tampered, block_time, SHOULD_FAIL);
    tf.push_instr_expect_rejection_reason("body", 0);
    tf.push_instr_expect_pow_chain_length(2, 0);

    // through the node: the same block is refused while the clock is behind it, and accepted
    // once the clock reaches it
    tf.push_instr_set_time(block_time - three_hours, 0);
    tf.push_instr_load_pow(&block, SHOULD_FAIL);
    tf.push_instr_expect_rejection_reason("header", 0);
    tf.push_instr_set_time(block_time, 0);
    tf.push_instr_load_pow(&block, 0);
    tf.push_instr_expect_pow_tip(&block.hash(), 0);
    tf.push_instr_set_time(0, 0);

    test_bytes(tf.write_to_bytes());
}

/// CHECK_POS asks whether a PoS block is valid now and stores nothing: the answer is immediate,
/// and the BFT chain is as it was.
#[test]
fn crosslink_check_pos_answers_without_storing() {
    set_test_name(function_name!());
    let mut tf = TF::new(&HARNESS_PARAMETERS);
    let sigma = HARNESS_PARAMETERS.bc_confirmation_depth_sigma as usize;
    let (_gen, _miner_addr, pow) = pow_chain_for(&mut tf, 8);

    let valid = pos_from_headers(0, FatPointerToBftBlock::null(), &pow[4..4 + sigma]);
    let short = pos_from_headers(0, FatPointerToBftBlock::null(), &pow[4..3 + sigma]);
    tf.push_instr_check_pos(&valid, 0);
    tf.push_instr_check_pos(&short, SHOULD_FAIL);
    tf.push_instr_expect_pos_chain_length(0, 0);
    tf.push_instr_load_pos(&valid, 0);
    tf.push_instr_expect_pos_chain_length(1, 0);

    test_bytes(tf.write_to_bytes());
}

/// Which block is the tip, not just how long the chain is.
///
/// Of two siblings with equal work the node's tip is the one with the greater hash, whichever
/// arrived first (`Chain::cmp` in zebra-state). That departs from the protocol, which prefers the
/// block received first; upstream zebra changed to first-received in PR 11341, which this fork
/// does not have. This pins what the node does today, in both arrival orders, so the day the tie
/// rule changes this fails and says so.
///
/// A block the node mines itself is named by the slot it was mined into.
#[test]
fn crosslink_pow_tip_by_hash() {
    set_test_name(function_name!());
    let mut tf = TF::new(&HARNESS_PARAMETERS);

    let network = regtest_network(&HARNESS_PARAMETERS);
    let key = TestKey::new(b"crosslink test miner");
    let miner = key.address();
    let other_miner = Address::Transparent(zcash_transparent::address::TransparentAddress::PublicKeyHash([1u8; 20]));
    let mut gen = BlockGen::init_at_genesis_plus_1(network, BlockGen::REGTEST_GENESIS_HASH, &miner);
    tf.push_instr_load_pow(&gen.tip, 0);
    tf.push_instr_expect_pow_tip(&gen.tip.hash(), 0);
    tf.push_instr_expect_finalized_tip(None, 0);

    // two sibling pairs: the lesser hash arrives first in one, the greater in the other
    for greater_arrives_first in [false, true] {
        let mut sibling_gen = gen.clone();
        let ours = gen.next_block(&miner);
        let theirs = sibling_gen.next_block(&other_miner);
        let (lesser, greater) = if ours.hash().0 < theirs.hash().0 { (&ours, &theirs) } else { (&theirs, &ours) };
        if greater_arrives_first {
            tf.push_instr_load_pow(greater, 0);
            tf.push_instr_load_pow(lesser, 0);
        } else {
            tf.push_instr_load_pow(lesser, 0);
            tf.push_instr_load_pow(greater, 0);
        }
        tf.push_instr_expect_pow_tip(&greater.hash(), 0);
        tf.push_instr_expect_pow_tip(&lesser.hash(), SHOULD_FAIL);

        // more work ends the tie: a block on our branch makes it the best chain either way
        let heavier = gen.next_block(&miner);
        tf.push_instr_load_pow(&heavier, 0);
        tf.push_instr_expect_pow_tip(&heavier.hash(), 0);
    }

    tf.push_instr_mine_from_template_into(1, 0);
    tf.push_instr_expect_pow_tip_slot(1, 0);
    tf.push_instr_expect_pow_tip(&gen.tip.hash(), SHOULD_FAIL);

    test_bytes(tf.write_to_bytes());
}

#[test]
fn crosslink_recv_pow_over_stp() {
    set_test_name(function_name!());
    let mut tf = TF::new(&HARNESS_PARAMETERS);

    let network = regtest_network(&HARNESS_PARAMETERS);
    let miner_addr = Address::decode(&network, "t27eWDgjFYJGVXmzrXeVjnb5J3uXDM9xH9v").unwrap();
    let mut gen =
        BlockGen::init_at_genesis_plus_1(network, BlockGen::REGTEST_GENESIS_HASH, &miner_addr);
    tf.push_instr_load_pow(&gen.tip, 0);
    for _ in 2..4 {
        tf.push_instr_load_pow(&gen.next_block(&miner_addr), 0);
    }
    let mut fork = gen.clone();

    // served by a peer instead of submitted
    let served = gen.next_block(&miner_addr);
    tf.push_instr_recv_pow(&served, 0, 0);
    tf.push_instr_expect_pow_chain_length(5, 0);

    // a known block is never requested, which is not a failure
    tf.push_instr_recv_pow(&served, 0, 0);

    // a longer fork from a second peer, one block at a time, reorgs the chain
    let miner_addr2 = zcash_keys::address::Address::Transparent(
        zcash_transparent::address::TransparentAddress::PublicKeyHash([1u8; 20]),
    );
    for _ in 4..7 {
        tf.push_instr_recv_pow(&fork.next_block(&miner_addr2), 1, 0);
    }
    tf.push_instr_expect_pow_chain_length(7, 0);

    // the header still hashes as advertised, so the node downloads the body before refusing it
    let mut tampered = fork.next_block(&miner_addr2).as_ref().clone();
    tampered.transactions.push(tampered.transactions[0].clone());
    tf.push_instr_recv_pow(&tampered, 1, SHOULD_FAIL);
    tf.push_instr_expect_pow_chain_length(7, 0);

    // an unknown packet type is ignored; a truncated chunk header gets the peer killed, for good
    tf.push_instr_recv_stp_packet(&[0x63, 1, 2, 3], 2, 0);
    tf.push_instr_recv_stp_packet(&[2, 1, 2], 2, SHOULD_FAIL);
    tf.push_instr_expect_rejection_reason("chunk header", 0);
    tf.push_instr_recv_pow(&fork.next_block(&miner_addr2), 2, SHOULD_FAIL);
    tf.push_instr_expect_rejection_reason("already killed", 0);
    tf.push_instr_expect_node_alive(0);

    test_bytes(tf.write_to_bytes());
}

/// A block whose header is invalid cannot exist under its hash, so the peer serving it is killed.
/// A block whose parent is unknown is answered at once, then committed when the parent arrives.
#[test]
fn crosslink_invalid_header_kills_peer_and_orphans_answer_at_once() {
    set_test_name(function_name!());
    let mut tf = TF::new(&HARNESS_PARAMETERS);

    let network = regtest_network(&HARNESS_PARAMETERS);
    let miner_addr = Address::decode(&network, "t27eWDgjFYJGVXmzrXeVjnb5J3uXDM9xH9v").unwrap();
    let mut gen =
        BlockGen::init_at_genesis_plus_1(network, BlockGen::REGTEST_GENESIS_HASH, &miner_addr);
    tf.push_instr_load_pow(&gen.tip, 0);

    let honest = gen.next_block(&miner_addr);
    let mut bad = honest.as_ref().clone();
    let mut bad_header = BlockHeader::clone(&bad.header);
    bad_header.difficulty_threshold = work::difficulty::INVALID_COMPACT_DIFFICULTY;
    bad.header = Arc::new(bad_header);
    tf.push_instr_recv_pow(&bad, 0, SHOULD_FAIL);
    tf.push_instr_expect_rejection_reason("header", 0);
    tf.push_instr_recv_pow(&honest, 0, SHOULD_FAIL);
    tf.push_instr_expect_rejection_reason("already killed", 0);
    tf.push_instr_recv_pow(&honest, 1, 0);
    tf.push_instr_expect_pow_chain_length(3, 0);

    let parent = gen.next_block(&miner_addr);
    let orphan = gen.next_block(&miner_addr);
    tf.push_instr_load_pow(&orphan, SHOULD_DEFER);
    tf.push_instr_expect_rejection_reason("not committed yet", 0);
    tf.push_instr_load_pow(&parent, 0);
    // The orphan commits right after its parent; a child of it commits only if it did.
    tf.push_instr_load_pow(&gen.next_block(&miner_addr), 0);
    tf.push_instr_expect_pow_chain_length(6, 0);
    tf.push_instr_expect_node_alive(0);

    test_bytes(tf.write_to_bytes());
}

// NOTE: a staking action is the only transaction we can synthesize without spendable
// UTXOs or shielded proofs: `has_inputs_and_outputs` waives the inputs/outputs rule for
// it. The amount must be 0 unless the bond is funded: the per-tx value balance must be
// non-negative, and coinbase outputs can't fund it before they mature (100 blocks).
// `target_finalizer` is a FinalizerAddress (a signed capability), not a bare key: consensus
// verifies its embedded signature (delegation.rs `addr.verify()`), so it must be minted from a
// real signing key via `FinalizerAddress::create`. The bond key is a real key for the same
// reason: consensus verifies the action's signature against it (check::staking_action_signature),
// so `bond_seed` names a key rather than supplying the pubkey bytes directly.
fn staking_tx_create_bond(
    upgrade: NetworkUpgrade,
    bond_seed: &[u8],
    target_finalizer: zcash_primitives::bft::FinalizerAddress,
    amount_zats: u64,
) -> Arc<Transaction> {
    use zcash_primitives::transaction::StakingAction;

    signed_staking_tx(upgrade, bond_seed, 0, |unique_pubkey, signature| StakingAction::CreateNewDelegationBond {
        amount_zats,
        unique_pubkey,
        bond_salt: [0; 32],
        target_finalizer,
        signature,
    })
}

/// A staking-only transaction whose action is signed by the bond key `bond_seed` names. `action`
/// builds the action from the bond's public key and a signature: it is called once with a blank
/// signature to find the sighash, then again with the real one. `expiry_height` only tells apart
/// transactions that would otherwise be identical; 0 means none.
fn signed_staking_tx(
    upgrade: NetworkUpgrade,
    bond_seed: &[u8],
    expiry_height: u32,
    action: impl Fn([u8; 32], [u8; 64]) -> zcash_primitives::transaction::StakingAction,
) -> Arc<Transaction> {
    signed_tx(upgrade, None, &[], Vec::new(), Some((bond_seed, &action)), expiry_height)
}

/// A transparent key a test holds. BlockGen can mine to its address, so the test can spend what
/// the coinbase pays it.
#[derive(Clone)]
struct TestKey {
    secret: secp256k1::SecretKey,
    public: secp256k1::PublicKey,
}

impl TestKey {
    fn new(seed: &[u8]) -> Self {
        use sha2::Digest;

        let secret = secp256k1::SecretKey::from_slice(&sha2::Sha256::digest(seed))
            .expect("a SHA-256 digest is a valid secret key");
        let public = secp256k1::PublicKey::from_secret_key(&secp256k1::Secp256k1::new(), &secret);
        TestKey { secret, public }
    }

    fn pub_key_hash(&self) -> [u8; 20] {
        use ripemd::Digest as _;
        use sha2::Digest as _;

        ripemd::Ripemd160::digest(sha2::Sha256::digest(self.public.serialize())).into()
    }

    /// The address to pass BlockGen as its miner address.
    fn address(&self) -> Address {
        Address::Transparent(zcash_transparent::address::TransparentAddress::PublicKeyHash(self.pub_key_hash()))
    }

    fn lock_script(&self) -> zebra_chain::transparent::Script {
        zebra_chain::transparent::Address::from_pub_key_hash(zebra_chain::parameters::NetworkKind::Regtest, self.pub_key_hash())
            .script()
    }

    /// A P2PKH unlock script: the signature over `sighash` (SIGHASH_ALL), then the public key.
    fn unlock_script(&self, sighash: &[u8]) -> zebra_chain::transparent::Script {
        let message = secp256k1::Message::from_digest_slice(sighash).expect("a sighash is 32 bytes");
        let signature = secp256k1::Secp256k1::new().sign_ecdsa(&message, &self.secret).serialize_der();
        let public = self.public.serialize();

        let mut script = Vec::new();
        script.push(signature.len() as u8 + 1);
        script.extend_from_slice(&signature);
        script.push(0x01);
        script.push(public.len() as u8);
        script.extend_from_slice(&public);
        zebra_chain::transparent::Script::new(&script)
    }
}

/// A VCrosslink transaction spending `inputs`, all locked to `key`, into `outputs`, optionally
/// carrying a staking action signed by the bond key its seed names. The fee is whatever the inputs
/// leave over; the caller picks it by choosing the outputs.
///
/// Signing order doesn't matter: the staking action signs the sighash with no input selected,
/// which commits to the inputs' outpoints and amounts but not their unlock scripts, and each input
/// signs a sighash that covers the staking action without its signature.
///
/// `upgrade` must be the network upgrade at the height of the block the transaction goes in: its
/// consensus branch id is signed over and checked. `BlockGen::next_upgrade` gives it for the next
/// block.
fn signed_tx(
    upgrade: NetworkUpgrade,
    key: Option<&TestKey>,
    inputs: &[(zebra_chain::transparent::OutPoint, zebra_chain::transparent::Output)],
    outputs: Vec<zebra_chain::transparent::Output>,
    staking: Option<(&[u8], &dyn Fn([u8; 32], [u8; 64]) -> zcash_primitives::transaction::StakingAction)>,
    expiry_height: u32,
) -> Arc<Transaction> {
    use zebra_chain::transaction::HashType;

    let network_upgrade = upgrade;
    let build = |unlock_scripts: &[zebra_chain::transparent::Script], staking_signature: [u8; 64]| {
        let inputs = inputs
            .iter()
            .zip(unlock_scripts)
            .map(|((outpoint, _), unlock_script)| zebra_chain::transparent::Input::PrevOut {
                outpoint: *outpoint,
                unlock_script: unlock_script.clone(),
                sequence: u32::MAX,
            })
            .collect();
        let staking_action = staking.map(|(bond_seed, action)| {
            let bond_pub_key = zebra_crosslink::rng_private_public_key_from_address(bond_seed).2;
            action(bond_pub_key.0, staking_signature)
        });
        Arc::new(Transaction::VCrosslink {
            network_upgrade,
            lock_time: LockTime::unlocked(),
            expiry_height: BlockHeight(expiry_height),
            inputs,
            outputs: outputs.clone(),
            sapling_shielded_data: None,
            orchard_shielded_data: None,
            ironwood_shielded_data: None,
            staking_action,
        })
    };

    let blank_scripts = vec![zebra_chain::transparent::Script::new(&[]); inputs.len()];
    let previous_outputs = Arc::new(inputs.iter().map(|(_, output)| output.clone()).collect::<Vec<_>>());

    // The sighash covers the action without its signature (`sa.unsigned().tree_hash()` in
    // sighash.rs), so signing what the unsigned transaction hashes to leaves that hash unchanged.
    let staking_signature = match staking {
        Some((bond_seed, _)) => {
            let bond_signing_key = zebra_crosslink::rng_private_public_key_from_address(bond_seed).1;
            let sighash = build(&blank_scripts, [0; 64])
                .sighash(network_upgrade, HashType::ALL, previous_outputs.clone(), None)
                .expect("a VCrosslink transaction has a sighash");
            bond_signing_key.sign(sighash.as_ref()).into()
        }
        None => [0; 64],
    };

    let unsigned = build(&blank_scripts, staking_signature);
    let unlock_scripts: Vec<_> = inputs
        .iter()
        .enumerate()
        .map(|(index, (_, output))| {
            let key = key.expect("a transaction with inputs needs the key they are locked to");
            let script_code = output.lock_script.as_raw_bytes().to_vec();
            let sighash = unsigned
                .sighash(network_upgrade, HashType::ALL, previous_outputs.clone(), Some((index, script_code)))
                .expect("a VCrosslink transaction has a sighash");
            key.unlock_script(sighash.as_ref())
        })
        .collect();
    build(&unlock_scripts, staking_signature)
}

/// The fee a test transaction pays for the mempool to accept it: the ZIP-317 conventional fee, and
/// at least the cap of the size-based minimum relay fee.
///
/// The floor matters today: `conventional_actions` counts a VCrosslink transaction without a
/// staking action as 0 actions (STAKING_AUDIT S8), so its conventional fee is 0 and only the relay
/// fee rule applies. Once S8 is fixed the conventional fee takes over.
fn mempool_fee(draft: &Transaction) -> zebra_chain::amount::Amount<zebra_chain::amount::NonNegative> {
    use zebra_chain::transaction::zip317;

    let relay_cap = zebra_chain::amount::Amount::<zebra_chain::amount::NonNegative>::try_from(
        zip317::MEMPOOL_TX_FEE_REQUIREMENT_CAP as u64,
    )
    .expect("the relay fee cap is a valid amount");
    std::cmp::max(zip317::conventional_fee(draft), relay_cap)
}

/// Spends `input` back to `key`, paying `mempool_fee`.
fn transparent_spend(
    upgrade: NetworkUpgrade,
    key: &TestKey,
    input: (zebra_chain::transparent::OutPoint, zebra_chain::transparent::Output),
) -> Arc<Transaction> {
    let input_value = input.1.value;
    let change = |fee: zebra_chain::amount::Amount<zebra_chain::amount::NonNegative>| {
        vec![zebra_chain::transparent::Output::new(
            (input_value - fee).expect("the input covers the fee"),
            key.lock_script(),
        )]
    };
    // The conventional fee depends only on the transaction's shape, not its amounts.
    let draft = signed_tx(upgrade, Some(key), &[input.clone()], change(zebra_chain::amount::Amount::zero()), None, 0);
    let fee = mempool_fee(&draft);
    signed_tx(upgrade, Some(key), &[input], change(fee), None, 0)
}

/// Bonds `amount_zats` to `target`, funded by `input` (locked to `key`), which also pays
/// `mempool_fee`; the rest comes back to `key`. Unlike an input-less staking action,
/// this pays a fee, so the mempool admits it.
fn staking_tx_create_bond_funded(
    upgrade: NetworkUpgrade,
    bond_seed: &[u8],
    target_finalizer: zcash_primitives::bft::FinalizerAddress,
    amount_zats: u64,
    key: &TestKey,
    input: (zebra_chain::transparent::OutPoint, zebra_chain::transparent::Output),
) -> Arc<Transaction> {
    use zcash_primitives::transaction::StakingAction;

    let create = move |unique_pubkey, signature| StakingAction::CreateNewDelegationBond {
        amount_zats,
        unique_pubkey,
        bond_salt: [0; 32],
        target_finalizer,
        signature,
    };
    let bonded = zebra_chain::amount::Amount::<zebra_chain::amount::NonNegative>::try_from(amount_zats)
        .expect("a test bond amount is a valid amount");
    let input_value = input.1.value;
    let change = |fee: zebra_chain::amount::Amount<zebra_chain::amount::NonNegative>| {
        let spent = (bonded + fee).expect("bond plus fee is a valid amount");
        vec![zebra_chain::transparent::Output::new(
            (input_value - spent).expect("the input covers the bond and the fee"),
            key.lock_script(),
        )]
    };
    // The conventional fee depends only on the transaction's shape, not its amounts.
    let draft = signed_tx(upgrade, Some(key), &[input.clone()], change(zebra_chain::amount::Amount::zero()), Some((bond_seed, &create)), 0);
    let fee = mempool_fee(&draft);
    signed_tx(upgrade, Some(key), &[input], change(fee), Some((bond_seed, &create)), 0)
}

fn staking_tx_unbond(upgrade: NetworkUpgrade, bond_seed: &[u8]) -> Arc<Transaction> {
    use zcash_primitives::transaction::StakingAction;

    signed_staking_tx(upgrade, bond_seed, 0, |unique_pubkey, signature| StakingAction::BeginDelegationUnbonding {
        unique_pubkey,
        signature,
    })
}

fn staking_tx_withdraw(upgrade: NetworkUpgrade, bond_seed: &[u8], amount_zats: u64, expiry_height: u32) -> Arc<Transaction> {
    use zcash_primitives::transaction::StakingAction;

    signed_staking_tx(upgrade, bond_seed, expiry_height, |unique_pubkey, signature| StakingAction::WithdrawDelegationBond {
        amount_zats,
        unique_pubkey,
        signature,
    })
}

fn staking_tx_retarget(
    upgrade: NetworkUpgrade,
    bond_seed: &[u8],
    from_finalizer: zcash_primitives::bft::FinalizerAddress,
    to_finalizer: zcash_primitives::bft::FinalizerAddress,
) -> Arc<Transaction> {
    use zcash_primitives::transaction::StakingAction;

    signed_staking_tx(upgrade, bond_seed, 0, |unique_pubkey, signature| StakingAction::RetargetDelegationBond {
        unique_pubkey,
        signature,
        from_finalizer,
        to_finalizer,
    })
}

#[test]
fn crosslink_pow_block_with_staking_tx() {
    set_test_name(function_name!());
    let mut tf = TF::new(&HARNESS_PARAMETERS);

    let network = regtest_network(&HARNESS_PARAMETERS);
    let miner_addr = Address::decode(&network, "t27eWDgjFYJGVXmzrXeVjnb5J3uXDM9xH9v").unwrap();
    let mut gen =
        BlockGen::init_at_genesis_plus_1(network, BlockGen::REGTEST_GENESIS_HASH, &miner_addr);
    tf.push_instr_load_pow(&gen.tip, 0);

    // A validly-minted finalizer target (its embedded signature must verify in consensus).
    let target = zcash_primitives::bft::FinalizerAddress::create(
        &zebra_crosslink::rng_private_public_key_from_address(b"staking-target").1,
    );

    // height 2 is the first height whose commitment is the ZIP-244 hashBlockCommitments
    // (height 1 carries the reserved all-zero commitment), so this exercises both the
    // merkle root and the auth-data commitment accounting for the extra transaction
    let block_with_tx =
        gen.next_block_with_txs(&miner_addr, &[staking_tx_create_bond(gen.next_upgrade(), b"staking-bond", target, 0)]);
    assert_eq!(block_with_tx.transactions.len(), 2);
    tf.push_instr_load_pow(&block_with_tx, 0);

    for _ in 3..5 {
        tf.push_instr_load_pow(&gen.next_block(&miner_addr), 0);
    }
    tf.push_instr_expect_pow_chain_length(5, 0);

    // appending a transaction without rebuilding the header must be rejected: the
    // merkle root & commitment no longer match the transaction list
    let mut tampered = gen.next_block(&miner_addr).as_ref().clone();
    tampered
        .transactions
        .push(staking_tx_create_bond(gen.next_upgrade(), b"staking-bond-tampered", target, 0));
    tf.push_instr_load_pow(&tampered, SHOULD_FAIL);
    tf.push_instr_expect_pow_chain_length(5, 0);

    test_bytes(tf.write_to_bytes());
}

/// `create_pos_and_ptr_to_finalize_pow_at_round` at round 1, as the scenes were built: the
/// pointer is header bytes of every PoW block citing it, so the recorded block hashes
/// (`crosslink_block_hashes_without_featurenet_upgrade_2_are_unchanged`) depend on the round.
fn create_pos_and_ptr_to_finalize_pow(
    bft_height: u32,
    parent_fat_ptr: FatPointerToBftBlock,
    pow_blocks: &[Arc<Block>],
    sigs: &[FatPointerSignature],
) -> BftBlockAndFatPointerToItWrap {
    create_pos_and_ptr_to_finalize_pow_at_round(bft_height, parent_fat_ptr, pow_blocks, 1, sigs)
}

/// A BFT block at `bft_height` over `pow_blocks`, with the fat pointer to it decided at `round`
/// with `sigs`. Under the nil roster of `HARNESS_PARAMETERS` the only certificate the node admits
/// from FeaturenetUpgrade2 is an empty one at round 0 (`certificate_is_valid`).
fn create_pos_and_ptr_to_finalize_pow_at_round(
    bft_height: u32,
    parent_fat_ptr: FatPointerToBftBlock,
    pow_blocks: &[Arc<Block>],
    round: u32,
    sigs: &[FatPointerSignature],
) -> BftBlockAndFatPointerToItWrap {
    assert_eq!(
        pow_blocks.len(),
        HARNESS_PARAMETERS.bc_confirmation_depth_sigma as usize
    );

    let mut hdrs = Vec::with_capacity(pow_blocks.len());
    for pow_block in pow_blocks {
        // BftBlock::try_from takes BcBlockHeader (zcash_primitives), not zebra's
        // Header; convert via the same helper production code uses (lib.rs).
        hdrs.push(zebra_crosslink::bc_hdr_to_lrz(pow_block.header.as_ref()));
    }

    // The `snapshot` -- the block this BFT block finalizes -- is the PARENT of the deepest
    // carried header: the sigma carried headers are the confirmations built on top of it, and
    // the snapshot is not carried. The PoW-side fat-pointer check resolves its height from the
    // chain to enforce `pow_height >= snapshot + sigma + 1`.
    let block = BftBlock::try_from(
        &HARNESS_PARAMETERS,
        bft_height,
        parent_fat_ptr,
        hdrs,
    )
    .expect("valid PoS block");

    // TODO:
    let _sig = FatPointerSignature {
        pub_key: PubKeyID([0xabu8; 32]),
        vote_signature: [0xbcu8; 64],
    };

    BftBlockAndFatPointerToItWrap(BftBlockAndFatPointerToIt::from_parts(block, bft_height.into(), round, sigs))
}

/// `next_pos_at_round` at round 1; see `create_pos_and_ptr_to_finalize_pow`.
fn next_pos(
    cur_bft_height: &mut u32,
    cur_fat_ptr: &mut FatPointerToBftBlock,
    pow_blocks: &[Arc<Block>],
    sigs: &[FatPointerSignature],
) -> BftBlockAndFatPointerToItWrap {
    next_pos_at_round(cur_bft_height, cur_fat_ptr, pow_blocks, 1, sigs)
}

fn next_pos_at_round(
    cur_bft_height: &mut u32,
    cur_fat_ptr: &mut FatPointerToBftBlock,
    pow_blocks: &[Arc<Block>],
    round: u32,
    sigs: &[FatPointerSignature],
) -> BftBlockAndFatPointerToItWrap {
    // NOTE: BFT heights are 0-based: a block's height is the chain position it will
    // occupy (validate_bft_block), so the first block is at height 0. Incrementing
    // *after* keeps the counter equal to the resulting chain *length*, which is what
    // EXPECT_POS_CHAIN_LENGTH callers pass it in as.
    let bft = create_pos_and_ptr_to_finalize_pow_at_round(*cur_bft_height, cur_fat_ptr.clone(), pow_blocks, round, sigs);
    *cur_bft_height += 1;
    *cur_fat_ptr = bft.0.fat_ptr.clone();
    bft
}

/// The BFT block a finalizer would propose at `bft_height` on top of `parent`, finalizing the
/// block just below `tail`: `tail` is the sigma PoW blocks above that snapshot, as the node's own
/// proposer takes them. Version 2 and `do_not_include_until_bc_height` carried from the parent, as
/// the node proposes when no slashfork is scheduled at this height.
fn bft_proposal(
    params: &zcash_primitives::bft::ZcashCrosslinkParameters,
    bft_height: u32,
    parent: FatPointerToBftBlock,
    parent_do_not_include: u64,
    tail: &[Arc<Block>],
) -> BftBlock {
    let headers = tail.iter().map(|b| zebra_crosslink::bc_hdr_to_lrz(b.header.as_ref())).collect();
    let mut block = BftBlock::try_from(params, bft_height, parent, headers).expect("a proposal over sigma linked headers");
    block.version = 2;
    block.do_not_include_until_bc_height = parent_do_not_include;
    block
}

/// The bytes a proposer sends for `block`: what tenderlink carries as the proposal value.
fn bft_proposal_value(block: &BftBlock) -> Vec<u8> {
    block.zcash_serialize_to_vec().expect("a BFT block serializes")
}

/// The fat pointer finalizers make by precommitting `block` at BFT `height` and `round`: one
/// signature per finalizer, each over the vote (its key, the block's hash, the height, and the
/// round with the commit bit), with the height's vote namespace appended unless it is nil.
/// Finalizers are named by the seeds their keys come from, as everywhere in these tests.
fn signed_fat_pointer(
    block: &BftBlock,
    height: u64,
    round: u32,
    finalizer_seeds: &[&[u8]],
    vote_namespace: &[u8; 32],
) -> FatPointerToBftBlock {
    let unsigned = FatPointerToBftBlock::from_parts(block.blake3_hash(), height, round, &[]);
    certificate_over(unsigned.vote_for_block_without_finalizer_public_key, finalizer_seeds, vote_namespace)
}

/// A fat pointer whose vote bytes are `template` (block hash, height, round with the commit bit),
/// signed by the finalizers with these seeds in the order given, each over its key and the
/// template, with the namespace appended unless it is nil. For certificates a node would not
/// make: the template is taken as given.
fn certificate_over(template: [u8; 44], finalizer_seeds: &[&[u8]], vote_namespace: &[u8; 32]) -> FatPointerToBftBlock {
    let signatures: Vec<FatPointerSignature> = finalizer_seeds
        .iter()
        .map(|seed| {
            let (_, key, pub_key) = zebra_crosslink::rng_private_public_key_from_address(seed);
            let mut signed = pub_key.0.to_vec();
            signed.extend_from_slice(&template);
            if *vote_namespace != [0; 32] {
                signed.extend_from_slice(vote_namespace);
            }
            FatPointerSignature { pub_key, vote_signature: <[u8; 64]>::from(key.sign(&signed)) }
        })
        .collect();
    FatPointerToBftBlock { vote_for_block_without_finalizer_public_key: template, signatures }
}

/// A fat pointer signed by these helpers verifies as the node verifies one, under the namespace it
/// was signed for and no other. Needs no node.
#[test]
fn signed_fat_pointer_verifies_under_its_own_namespace_only() {
    let (_gen, _miner, pow) = {
        let network = regtest_network(&HARNESS_PARAMETERS);
        let miner = Address::decode(&network, "t27eWDgjFYJGVXmzrXeVjnb5J3uXDM9xH9v").unwrap();
        let mut gen = BlockGen::init_at_genesis_plus_1(network, BlockGen::REGTEST_GENESIS_HASH, &miner);
        let mut pow = vec![gen.tip.clone()];
        for _ in 0..HARNESS_PARAMETERS.bc_confirmation_depth_sigma {
            pow.push(gen.next_block(&miner));
        }
        (gen, miner, pow)
    };
    let sigma = HARNESS_PARAMETERS.bc_confirmation_depth_sigma as usize;
    let block = bft_proposal(&HARNESS_PARAMETERS, 0, FatPointerToBftBlock::null(), 0, &pow[1..1 + sigma]);
    assert_eq!(block.version, 2);
    let value = bft_proposal_value(&block);
    assert_eq!(BftBlock::zcash_deserialize(&value[..]).expect("the value parses as a BFT block"), block);

    let seeds: [&[u8]; 3] = [b"finalizer-0", b"finalizer-1", b"finalizer-2"];
    let namespace = [7u8; 32];
    for (signed_for, checked_with, verifies) in
        [([0u8; 32], [0u8; 32], true), (namespace, namespace, true), (namespace, [0u8; 32], false), ([0u8; 32], namespace, false)]
    {
        let fat_ptr = signed_fat_pointer(&block, 0, 1, &seeds, &signed_for);
        assert_eq!(fat_ptr.signatures.len(), seeds.len());
        assert_eq!(fat_ptr.points_at_block_hash(), block.blake3_hash());
        assert_eq!(fat_ptr.validate_signatures(&checked_with), verifies, "signed for {signed_for:?}, checked with {checked_with:?}");
    }
}

#[test]
fn crosslink_gen_pow_and_no_signature_no_roster_pos() {
    set_test_name(function_name!());
    let mut tf = TF::new(&HARNESS_PARAMETERS);

    let network = regtest_network(&HARNESS_PARAMETERS);
    let miner_addr = Address::decode(&network, "t27eWDgjFYJGVXmzrXeVjnb5J3uXDM9xH9v").unwrap();
    let mut gen =
        BlockGen::init_at_genesis_plus_1(network, BlockGen::REGTEST_GENESIS_HASH, &miner_addr);

    let mut pow_common = vec![gen.tip.clone()];
    for _ in 2..4 {
        pow_common.push(gen.next_block(&miner_addr));
    }
    for block in &pow_common {
        tf.push_instr_load_pow(block, 0);
    }

    let fat_ptr = &mut FatPointerToBftBlock::null();
    let pos_h = &mut 0;
    let bft = next_pos(pos_h, fat_ptr, &pow_common[0..3], &[]);
    tf.push_instr_load_pos(&bft, 0);

    for _ in 4..7 {
        tf.push_instr_load_pow(&gen.next_block(&miner_addr), 0);
        // push bft pointer
    }
    tf.push_instr_expect_pow_chain_length(7, 0);

    test_bytes(tf.write_to_bytes());
}

#[test]
fn crosslink_force_roster() {
    set_test_name(function_name!());
    let mut tf = TF::new(&HARNESS_PARAMETERS);

    tf.push_instr_expect_roster_includes([0xab; 32], 42, SHOULD_FAIL);

    tf.push_instr_roster_force_include([0xab; 32], 42, 0);

    tf.push_instr_expect_roster_includes([0xab; 32], 42, 0);
    tf.push_instr_expect_roster_includes([0xab; 32], TEST_STAKE_IGNORED, 0);
    tf.push_instr_expect_roster_includes([0xab; 32], 43, SHOULD_FAIL);
    tf.push_instr_expect_roster_includes([0xba; 32], 42, SHOULD_FAIL);

    test_bytes(tf.write_to_bytes());
}

#[test]
fn crosslink_add_newcomer_to_roster_via_pow() {
    set_test_name(function_name!());
    let mut tf = TF::new(&HARNESS_PARAMETERS);

    // let (_, prv_key, pub_key) = rng_private_public_key_from_address(&[0]);

    // tf.push_instr_roster_force_include(pub_key, 42000, 0);
    // tf.push_instr_expect_roster_includes(pub_key, 42000, SHOULD_FAIL);

    let network = regtest_network(&HARNESS_PARAMETERS);
    let miner_addr = Address::decode(&network, "t27eWDgjFYJGVXmzrXeVjnb5J3uXDM9xH9v").unwrap();

    let (_, prv_key, pub_key) =
        zebra_crosslink::rng_private_public_key_from_address("some_pub_key".as_bytes());
    // The roster is keyed on the finalizer address's pub_key (target_finalizer_pk), which is
    // pub_key here, so EXPECT_ROSTER_INCLUDES(pub_key) below matches.
    let target = zcash_primitives::bft::FinalizerAddress::create(&prv_key);

    // NOTE: the bond must be in the height-1 block: the BFT block over headers 2..=4 has
    // snapshot height 1, and the roster snapshot taken at finalization only sees bonds
    // already in the finalized state. Amount 0 as the bond can't be funded (see
    // staking_tx_create_bond).
    let height_1_upgrade = NetworkUpgrade::current(&network, zebra_chain::block::Height(1));
    let staking_tx = staking_tx_create_bond(height_1_upgrade, b"newcomer-bond", target, 0);
    let mut gen = BlockGen::init_at_genesis_plus_1_with_txs(
        network,
        BlockGen::REGTEST_GENESIS_HASH,
        &miner_addr,
        &[staking_tx],
    );

    let mut pow_common = vec![gen.tip.clone()];
    for _ in 2..5 {
        pow_common.push(gen.next_block(&miner_addr));
    }
    for block in &pow_common[0..4] {
        tf.push_instr_load_pow(block, 0);
    }

    // The window is [2,3,4] so its snapshot -- the parent of its deepest header, which is what
    // a BFT block finalizes -- is height 1, the block carrying the bond.
    let (pos_h, fat_ptr) = (&mut 0, &mut FatPointerToBftBlock::null());
    let bft = next_pos(pos_h, fat_ptr, &pow_common[1..4], &[]);
    tf.push_instr_load_pos(&bft, 0);

    // NOTE: membership only: the bond is created with 0 stake, but finalizer rewards
    // accrue to it by the time the roster snapshot is taken, so the exact voting power
    // depends on the reward schedule
    tf.push_instr_expect_roster_includes(pub_key.0, TEST_STAKE_IGNORED, 0);

    test_bytes(tf.write_to_bytes());
}


// ---------------------------------------------------------------------------------------
// The scenes drawn in FINALITY_DIAGRAM.html, as test-format files.
//
// These exist to be looked at: load one with the GUI's "View (no consensus)" button and the
// picture is built from the file's own blocks, so the third scene -- a best chain forking
// below the finalized block -- can be seen even though this node refuses it. "Load into
// zebra" runs the first two through the real admission path as well.
//
// Heights are chosen so this node's own finalized marker lands where the diagram writes
// `fin`. The marker is the snapshot of the newest BFT block, `parent(headers[0])`
// (FINALITY.md 3.1), so the BFT chain in each scene stops at the block whose snapshot is the
// diagram's `fin`. The drawing reaches the same block with `LF` two BFT blocks behind the
// one the tip cites, and this tree has neither the LF lag nor the `candidate` clamp, so
// matching the drawn arrows and matching the drawn marker are different files. These match
// the marker. VIZ_GUI_FINALITY_RULES.md records the rest.
// ---------------------------------------------------------------------------------------

/// Where the scene files live: beside the other test-format data.
fn diagram_scene_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../crosslink-test-data")
        .join(name)
}

/// Re-stamps the generator's tip with a BFT pointer, giving that block a `context_bft`.
/// The merkle root still matches the transaction list; the commitment is recomputed, which
/// changes it only from FeaturenetUpgrade2, where it binds the pointer. The block hash does
/// change, which is why this has to happen before the next block is generated from this one.
fn point_tip_at_bft(gen: &mut BlockGen, fat_ptr: &FatPointerToBftBlock) -> Arc<Block> {
    let height = gen.tip.coinbase_height().expect("generated blocks have a height");
    // The generator pushes a block into its history tree only when it builds the next one, so the
    // tree still ends at the tip's parent, which is what the tip's commitment covers.
    let commitment_bytes =
        BlockGen::commitment_bytes(&gen.network, height, &gen.history_tree, &gen.tip.transactions, fat_ptr);
    gen.tip = Arc::new(Block {
        header: Arc::new(BlockHeader {
            commitment_bytes: HexDebug(commitment_bytes),
            fat_pointer_to_bft_block: fat_ptr.clone(),
            ..*gen.tip.header
        }),
        ..gen.tip.as_ref().clone()
    });
    gen.tip.clone()
}

/// A second valid transparent P2PKH miner, so a competing branch has different coinbases
/// and therefore different block hashes. Tex addresses are rejected by the Ironwood v6
/// coinbase builder ("Address not supported for miner rewards").
fn diagram_fork_miner() -> Address {
    Address::Transparent(zcash_transparent::address::TransparentAddress::PublicKeyHash(
        [1u8; 20],
    ))
}

/// Scene 1 of the diagram: one PoW chain of ten blocks, three BFT blocks over sliding
/// sigma-windows, and the finalized marker on P5.
///
/// ```text
///   PoW           BFT             snapshot   file order
///   P1..P6        -                          P1..P6
///   -             bft0 [P4,P5,P6] P3         bft0
///   P7 -> bft0    -                          P7
///   -             bft1 [P5,P6,P7] P4         bft1
///   P8 -> bft1    -                          P8
///   -             bft2 [P6,P7,P8] P5         bft2
///   P9..P10 -> bft2                          P9, P10
/// ```
///
/// Each BFT block finalizes the PARENT of its deepest header -- the carried headers are the
/// sigma confirmations above it -- so bft2 over [P6,P7,P8] is what puts the marker on P5. A
/// certificate may only be carried by a PoW block at `snapshot + sigma + 1` or above (bft0's
/// earliest legal carrier is P7); the interleave cites each one in the first block built after
/// it, which is exactly that minimum.
///
/// A BFT block can only carry headers of PoW blocks that already exist, and a PoW block
/// can only point at a BFT block that already exists, so the two chains have to be
/// interleaved this way; the diagram's `P10.context_bft = S6` with `S6` covering P8..P10
/// is not constructible at all, since the pointer is inside the hashed header.
fn diagram_scene_1() -> (TF, Vec<Arc<Block>>) {
    let mut tf = TF::new(&HARNESS_PARAMETERS);

    let network = regtest_network(&HARNESS_PARAMETERS);
    let miner_addr = Address::decode(&network, "t27eWDgjFYJGVXmzrXeVjnb5J3uXDM9xH9v").unwrap();
    let mut gen =
        BlockGen::init_at_genesis_plus_1(network, BlockGen::REGTEST_GENESIS_HASH, &miner_addr);

    // pow[i] is the block at height i + 1, so pow[4] is the diagram's P5.
    let mut pow: Vec<Arc<Block>> = vec![gen.tip.clone()];
    for _ in 2..=6 {
        pow.push(gen.next_block(&miner_addr));
    }
    for block in &pow {
        tf.push_instr_load_pow(block, 0);
    }

    let (pos_h, fat_ptr) = (&mut 0, &mut FatPointerToBftBlock::null());

    // bft0 over [P4,P5,P6], snapshot P3; P7 then cites it.
    let bft0 = next_pos(pos_h, fat_ptr, &pow[3..6], &[]);
    tf.push_instr_load_pos(&bft0, 0);
    gen.next_block(&miner_addr);
    pow.push(point_tip_at_bft(&mut gen, &bft0.0.fat_ptr));
    tf.push_instr_load_pow(pow.last().unwrap(), 0);

    // bft1 over [P5,P6,P7], snapshot P4; P8 then cites it.
    let bft1 = next_pos(pos_h, fat_ptr, &pow[4..7], &[]);
    tf.push_instr_load_pos(&bft1, 0);
    gen.next_block(&miner_addr);
    pow.push(point_tip_at_bft(&mut gen, &bft1.0.fat_ptr));
    tf.push_instr_load_pow(pow.last().unwrap(), 0);

    // bft2 over [P6,P7,P8]. Its snapshot is P5, so the finalized marker lands there.
    let bft2 = next_pos(pos_h, fat_ptr, &pow[5..8], &[]);
    tf.push_instr_load_pos(&bft2, 0);
    for _ in 9..=10 {
        gen.next_block(&miner_addr);
        pow.push(point_tip_at_bft(&mut gen, &bft2.0.fat_ptr));
        tf.push_instr_load_pow(pow.last().unwrap(), 0);
    }

    assert_eq!(pow.len(), 10);
    assert_eq!(
        bft2.0.block.snapshot_block_hash().0,
        pow[4].hash().0,
        "the newest BFT block's snapshot must be P5, the diagram's fin"
    );

    (tf, pow)
}

/// Scene 2: the benign case, where a reorganization exposes an older BFT context so the
/// derived candidate moves backward while the whole new best chain still contains `fin`.
///
/// Scene 1, then a branch off P8 whose blocks cite `bft1` rather than `bft2`. The diagram
/// replaces P8-P10 with three Q blocks of higher work; regtest difficulty is constant, so
/// here the branch wins by being one block longer instead, Q9..Q11.
fn diagram_scene_2() -> (TF, Vec<Arc<Block>>, Vec<Arc<Block>>) {
    let mut tf = TF::new(&HARNESS_PARAMETERS);

    let network = regtest_network(&HARNESS_PARAMETERS);
    let miner_addr = Address::decode(&network, "t27eWDgjFYJGVXmzrXeVjnb5J3uXDM9xH9v").unwrap();
    let fork_addr = diagram_fork_miner();
    let mut gen =
        BlockGen::init_at_genesis_plus_1(network, BlockGen::REGTEST_GENESIS_HASH, &miner_addr);

    let mut pow: Vec<Arc<Block>> = vec![gen.tip.clone()];
    for _ in 2..=6 {
        pow.push(gen.next_block(&miner_addr));
    }
    for block in &pow {
        tf.push_instr_load_pow(block, 0);
    }

    let (pos_h, fat_ptr) = (&mut 0, &mut FatPointerToBftBlock::null());

    let bft0 = next_pos(pos_h, fat_ptr, &pow[3..6], &[]);
    tf.push_instr_load_pos(&bft0, 0);
    gen.next_block(&miner_addr);
    pow.push(point_tip_at_bft(&mut gen, &bft0.0.fat_ptr));
    tf.push_instr_load_pow(pow.last().unwrap(), 0);

    let bft1 = next_pos(pos_h, fat_ptr, &pow[4..7], &[]);
    tf.push_instr_load_pos(&bft1, 0);
    gen.next_block(&miner_addr);
    pow.push(point_tip_at_bft(&mut gen, &bft1.0.fat_ptr));
    tf.push_instr_load_pow(pow.last().unwrap(), 0);

    // The competing branch starts from P8, which already carries its bft1 pointer, so both
    // branches descend from the same block.
    let mut fork_gen = gen.clone();

    let bft2 = next_pos(pos_h, fat_ptr, &pow[5..8], &[]);
    tf.push_instr_load_pos(&bft2, 0);
    for _ in 9..=10 {
        gen.next_block(&miner_addr);
        pow.push(point_tip_at_bft(&mut gen, &bft2.0.fat_ptr));
        tf.push_instr_load_pow(pow.last().unwrap(), 0);
    }

    // Q9..Q11 cite bft1: an older context than P9..P10's bft2, which is what makes the
    // derived candidate move backward. The Extension rule still holds -- their parent P8
    // cites bft1 too, so the pointer never regresses along the branch.
    let mut fork: Vec<Arc<Block>> = Vec::new();
    for _ in 9..=11 {
        fork_gen.next_block(&fork_addr);
        fork.push(point_tip_at_bft(&mut fork_gen, &bft1.0.fat_ptr));
        tf.push_instr_load_pow(fork.last().unwrap(), 0);
    }

    assert_eq!(pow.len(), 10);
    assert_eq!(fork.len(), 3);
    assert_eq!(
        fork[0].header.previous_block_hash,
        pow[7].hash(),
        "the branch must fork from P8"
    );

    (tf, pow, fork)
}

/// Scene 3: the exceptional case, where the raw best chain forks below `fin`.
///
/// P1..P8 with the BFT chain finalizing P5, then C4..C9 forking from P3 -- one block
/// longer than P4..P8, so it is the heaviest chain -- and citing `bft0`, whose own headers
/// sit on the branch it conflicts with.
///
/// This node will not hold this state: committing `fin` discards the non-finalized
/// branches that do not hold it, and a fork below the finalized block is
/// refused on ingest. It is a view-only scene, and there is no test that runs it through
/// the node, because the behaviour under test would be the refusal rather than the
/// picture.
fn diagram_scene_3(fork_flags: u32) -> (TF, Vec<Arc<Block>>, Vec<Arc<Block>>) {
    let mut tf = TF::new(&HARNESS_PARAMETERS);

    let network = regtest_network(&HARNESS_PARAMETERS);
    let miner_addr = Address::decode(&network, "t27eWDgjFYJGVXmzrXeVjnb5J3uXDM9xH9v").unwrap();
    let fork_addr = diagram_fork_miner();
    let mut gen =
        BlockGen::init_at_genesis_plus_1(network, BlockGen::REGTEST_GENESIS_HASH, &miner_addr);

    let mut pow: Vec<Arc<Block>> = vec![gen.tip.clone()];
    for _ in 2..=3 {
        pow.push(gen.next_block(&miner_addr));
    }
    // The conflicting branch forks from P3, below everything the BFT chain finalizes.
    let mut fork_gen = gen.clone();
    for _ in 4..=6 {
        pow.push(gen.next_block(&miner_addr));
    }
    for block in &pow {
        tf.push_instr_load_pow(block, 0);
    }

    let (pos_h, fat_ptr) = (&mut 0, &mut FatPointerToBftBlock::null());

    let bft0 = next_pos(pos_h, fat_ptr, &pow[3..6], &[]);
    tf.push_instr_load_pos(&bft0, 0);
    gen.next_block(&miner_addr);
    pow.push(point_tip_at_bft(&mut gen, &bft0.0.fat_ptr));
    tf.push_instr_load_pow(pow.last().unwrap(), 0);

    let bft1 = next_pos(pos_h, fat_ptr, &pow[4..7], &[]);
    tf.push_instr_load_pos(&bft1, 0);
    gen.next_block(&miner_addr);
    pow.push(point_tip_at_bft(&mut gen, &bft1.0.fat_ptr));
    tf.push_instr_load_pow(pow.last().unwrap(), 0);

    // Snapshot P5. Everything after this conflicts with it. P9 cites it: a decision alone
    // moves nothing (FINALITY.md §4.3), so without the citation `fin` would stop at P4.
    let bft2 = next_pos(pos_h, fat_ptr, &pow[5..8], &[]);
    tf.push_instr_load_pos(&bft2, 0);
    gen.next_block(&miner_addr);
    pow.push(point_tip_at_bft(&mut gen, &bft2.0.fat_ptr));
    tf.push_instr_load_pow(pow.last().unwrap(), 0);

    // C4..C6 carry no BFT pointer: bft0's snapshot is P3, and the sigma-confirmation rule in
    // the fat-pointer check lets nothing below P3 + sigma + 1 = 7 carry that certificate.
    // C7..C10 then cite bft0, whose headers sit on the branch this one conflicts with -- the
    // point of the scene.
    let mut fork: Vec<Arc<Block>> = Vec::new();
    for height in 4..=10 {
        fork_gen.next_block(&fork_addr);
        if height >= 7 {
            fork.push(point_tip_at_bft(&mut fork_gen, &bft0.0.fat_ptr));
        } else {
            fork.push(fork_gen.tip.clone());
        }
        // A refused fork is refused at its first block; the rest wait on a parent that never
        // commits, so the node answers them pending.
        let flags = if fork_flags != 0 && height > 4 { SHOULD_DEFER } else { fork_flags };
        tf.push_instr_load_pow(fork.last().unwrap(), flags);
    }

    assert_eq!(pow.len(), 9);
    assert_eq!(fork.len(), 7);
    assert_eq!(
        fork[0].header.previous_block_hash,
        pow[2].hash(),
        "the conflicting branch must fork from P3"
    );

    (tf, pow, fork)
}

/// Writes the three diagram scenes to `crosslink-test-data/`. Boots nothing: the files are
/// built from `BlockGen` alone, so this runs alongside anything else.
#[test]
fn crosslink_write_finality_diagram_scenes() {
    let (tf1, pow1) = diagram_scene_1();
    let (tf2, pow2, fork2) = diagram_scene_2();
    let (tf3, pow3, fork3) = diagram_scene_3(0);

    // The first two scenes share their common prefix exactly, so scene 2 really is scene 1
    // plus a branch rather than a differently-generated chain.
    assert_eq!(
        pow1.iter().map(|b| b.hash()).collect::<Vec<_>>(),
        pow2.iter().map(|b| b.hash()).collect::<Vec<_>>()
    );
    assert_eq!(pow3.len() + fork3.len(), 16);
    assert_eq!(fork2.len(), 3);

    for (tf, name) in [
        (tf1, "finality_diagram_1_candidate.zeccltf"),
        (tf2, "finality_diagram_2_benign_reorg.zeccltf"),
        (tf3, "finality_diagram_3_conflicting_fork.zeccltf"),
    ] {
        let path = diagram_scene_path(name);
        assert!(tf.write_to_file(&path), "could not write {}", path.display());
    }
}

/// Scene 1 through the real admission path: the chain reaches P10 and the finalized marker
/// sits on P5.
#[test]
fn crosslink_finality_diagram_1_candidate() {
    set_test_name(function_name!());
    let (mut tf, pow) = diagram_scene_1();

    tf.push_instr_expect_pow_chain_length(11, 0);
    tf.push_instr_expect_pos_chain_length(3, 0);
    tf.push_instr_expect_pow_block_finality(&pow[4].hash(), Some(TFLBlockFinality::Finalized), 0);
    tf.push_instr_expect_pow_block_finality(
        &pow[5].hash(),
        Some(TFLBlockFinality::NotYetFinalized),
        0,
    );

    test_bytes(tf.write_to_bytes());
}

/// Scene 2 through the real admission path: the longer branch off P8 becomes the best
/// chain, and the finalized marker does not move, because no BFT block decided.
#[test]
fn crosslink_finality_diagram_2_benign_reorg() {
    set_test_name(function_name!());
    let (mut tf, pow, _fork) = diagram_scene_2();

    tf.push_instr_expect_pow_chain_length(12, 0);
    tf.push_instr_expect_pos_chain_length(3, 0);
    tf.push_instr_expect_pow_block_finality(&pow[4].hash(), Some(TFLBlockFinality::Finalized), 0);

    test_bytes(tf.write_to_bytes());
}

/// Scene 2's best chain and decided BFT blocks, as `export-chain` would read them from the node
/// that ran it: genesis, P1..P8, Q9..Q11, and bft0..bft2.
fn diagram_scene_2_chain_prefix() -> zebra_state::ChainPrefix {
    let (tf, pow, fork) = diagram_scene_2();
    let bytes = tf.write_to_bytes();
    let decisions = TF::read_from_bytes(&bytes)
        .expect("the scene reads back")
        .instrs
        .iter()
        .filter(|instr| instr.kind == TFInstr::LOAD_POS)
        .map(|instr| {
            let decided = BftBlockAndFatPointerToItWrap::zcash_deserialize(instr.data_slice(&bytes))
                .expect("the scene's BFT blocks parse")
                .0;
            zebra_state::BftDecision { block: decided.block, fat_pointer: decided.fat_ptr, proposal_sigs: Vec::new() }
        })
        .collect();
    let best_chain = std::iter::once(zebra_chain::block::genesis::regtest_genesis_block())
        .chain(pow[..8].iter().cloned())
        .chain(fork)
        .collect();
    zebra_state::ChainPrefix::new(best_chain, decisions).expect("P8 is Q9's parent")
}

/// A chain prefix file of scene 2 loads through the real admission paths. The node ends on the
/// file's best chain and holds the BFT blocks the cutoff kept. Its finalized marker sits where
/// following that chain puts it: on P4, which the branch's own pointer to bft1 finalizes. The
/// scene's node had moved it to P5 while the abandoned P9..P10 were its best chain, and that
/// memory is not part of the chain.
fn chain_prefix_of_scene_2_loads(cutoff: zebra_state::BftCutoff, trusted: bool) {
    let prefix = diagram_scene_2_chain_prefix();
    let mut tf = zebra_crosslink::chain_prefix::prefix_tf(&HARNESS_PARAMETERS, &prefix, cutoff);
    let (_, tip) = prefix.tip().expect("the prefix has blocks");
    let p4 = prefix.blocks[4].hash();
    let p5 = prefix.blocks[5].hash();

    tf.push_instr_expect_pow_chain_length(prefix.blocks.len(), 0);
    tf.push_instr_expect_pow_tip(&tip, 0);
    tf.push_instr_expect_pos_chain_length(prefix.bft_count(cutoff), 0);
    tf.push_instr_expect_finalized_tip(Some(&p4), 0);
    tf.push_instr_expect_pow_block_finality(&p4, Some(TFLBlockFinality::Finalized), 0);
    tf.push_instr_expect_pow_block_finality(&p5, Some(TFLBlockFinality::NotYetFinalized), 0);

    zebra_crosslink::test_format::TRUSTED_LOAD_POW.store(trusted, std::sync::atomic::Ordering::Relaxed);
    test_bytes(tf.write_to_bytes());
}

#[test]
fn crosslink_chain_prefix_cited_loads() {
    set_test_name(function_name!());
    let prefix = diagram_scene_2_chain_prefix();
    // Q9..Q11 and P8 cite bft1; bft2 was cited only by the abandoned P9..P10.
    assert_eq!(prefix.bft_count(zebra_state::BftCutoff::Cited), 2);
    chain_prefix_of_scene_2_loads(zebra_state::BftCutoff::Cited, false);
}

#[test]
fn crosslink_chain_prefix_supported_loads() {
    set_test_name(function_name!());
    let prefix = diagram_scene_2_chain_prefix();
    // bft2's snapshot, P5, is on the best chain, so the prefix can validate it.
    assert_eq!(prefix.bft_count(zebra_state::BftCutoff::Supported), 3);
    chain_prefix_of_scene_2_loads(zebra_state::BftCutoff::Supported, false);
}

#[test]
fn crosslink_chain_prefix_trusted_loads() {
    set_test_name(function_name!());
    chain_prefix_of_scene_2_loads(zebra_state::BftCutoff::Cited, true);
}

// TODO:
// - reject signatures from outside the roster
// - reject pos block with < 2/3rds roster stake
// - reject pos block with signatures from the previous, but not current roster
// > require correctly-signed incorrect data:
//   - reject pos block with > sigma headers
//   - reject pos block with < sigma headers
//   - reject pos block where headers don't form subchain (hdrs[i].hash() != hdrs[i+1].previous_block_hash)
//   - repeat all signature tests but for the *next* pos block's fat pointer back
// - reject pos block that does have the correct fat pointer *hash* to prev block
// ...

/// Scene 3 through the real admission path, which refuses it: the conflicting branch
/// forks below the finalized block, so none of it is admitted and the best chain stays
/// P8. The scene file itself carries no SHOULD_FAIL flags -- it is a picture for the
/// GUI's "View (no consensus)" path, where the branch is drawn as the diagram draws it.
#[test]
fn crosslink_finality_diagram_3_conflicting_fork_is_refused() {
    set_test_name(function_name!());
    let (mut tf, pow, _fork) = diagram_scene_3(SHOULD_FAIL);

    tf.push_instr_expect_pow_chain_length(10, 0);
    tf.push_instr_expect_pos_chain_length(3, 0);
    tf.push_instr_expect_pow_block_finality(&pow[4].hash(), Some(TFLBlockFinality::Finalized), 0);

    test_bytes(tf.write_to_bytes());
}

/// A staking amount above MAX_MONEY is rejected like any invalid block. It used to abort the node:
/// the amount comes off the wire unchecked, converting it to an Amount asserted, and the profiles
/// set panic = "abort". The block is valid in every other way, so nothing rejects it earlier.
#[test]
fn crosslink_reject_pow_block_with_oversized_staking_amount() {
    set_test_name(function_name!());
    let mut tf = TF::new(&HARNESS_PARAMETERS);

    let network = regtest_network(&HARNESS_PARAMETERS);
    let miner_addr = Address::decode(&network, "t27eWDgjFYJGVXmzrXeVjnb5J3uXDM9xH9v").unwrap();
    let mut gen =
        BlockGen::init_at_genesis_plus_1(network, BlockGen::REGTEST_GENESIS_HASH, &miner_addr);
    tf.push_instr_load_pow(&gen.tip, 0);

    let target = zcash_primitives::bft::FinalizerAddress::create(
        &zebra_crosslink::rng_private_public_key_from_address(b"staking-target").1,
    );
    let oversized = zebra_chain::amount::MAX_MONEY as u64 + 1;
    let block = gen.next_block_with_txs(
        &miner_addr,
        &[staking_tx_create_bond(gen.next_upgrade(), b"oversized-bond", target, oversized)],
    );
    tf.push_instr_load_pow(&block, SHOULD_FAIL);

    // Answering this needs the node still running, with its chain unchanged.
    tf.push_instr_expect_pow_chain_length(2, 0);

    test_bytes(tf.write_to_bytes());
}

/// Takes three bonds through a lifecycle in real blocks, and at each stage offers a block with a
/// pair of actions that each apply alone but not together: unbond then retarget (STAKING_AUDIT
/// S3), and one bond withdrawn twice, which also panicked the commit. Both were accepted before
/// 65c7e1b4. Each bad block is built on a copy of the generator, so the good block at the same
/// height follows it; the node must reject the bad one and still accept the good one.
#[test]
fn crosslink_reject_action_on_a_bond_after_it_unbonds_or_withdraws_in_the_block() {
    set_test_name(function_name!());

    // The shortest realistic calendar keeps the lifecycle to a few dozen blocks rather than 300.
    let params = HARNESS_PARAMETERS;
    let staking = short_staking(&params);
    let params = zcash_primitives::bft::ZcashCrosslinkParameters { staking, ..params };
    let mut tf = TF::new(&params);

    let network = regtest_network(&params);
    let miner_addr = Address::decode(&network, "t27eWDgjFYJGVXmzrXeVjnb5J3uXDM9xH9v").unwrap();
    let finalizer = |seed: &[u8]| {
        zcash_primitives::bft::FinalizerAddress::create(&zebra_crosslink::rng_private_public_key_from_address(seed).1)
    };
    let (target, other) = (finalizer(b"finalizer-a"), finalizer(b"finalizer-b"));

    let mut gen =
        BlockGen::init_at_genesis_plus_1(network, BlockGen::REGTEST_GENESIS_HASH, &miner_addr);
    tf.push_instr_load_pow(&gen.tip, 0);
    let height = |gen: &BlockGen| gen.tip.coinbase_height().expect("generated blocks have a height").0;
    let mine_empty_until = |gen: &mut BlockGen, tf: &mut TF, next_height: u32| {
        while height(gen) + 1 < next_height {
            tf.push_instr_load_pow(&gen.next_block(&miner_addr), 0);
        }
    };

    // Unbonding needs action_delay blocks after creation, withdrawal the same after unbonding,
    // and both must land in a staking day window: the start of the next two periods.
    const CREATED: u32 = 2;
    let unbond = staking.period;
    let withdraw = 2 * staking.period;
    for (at, after) in [(unbond, CREATED), (withdraw, unbond)] {
        assert!(at >= after + staking.action_delay && at % staking.period < staking.day_window);
    }

    let block = gen.next_block_with_txs(
        &miner_addr,
        &[
            staking_tx_create_bond(gen.next_upgrade(), b"bond-a", target, 0),
            staking_tx_create_bond(gen.next_upgrade(), b"bond-b", target, 0),
            staking_tx_create_bond(gen.next_upgrade(), b"bond-c", target, 0),
        ],
    );
    assert_eq!(height(&gen), CREATED);
    tf.push_instr_load_pow(&block, 0);

    mine_empty_until(&mut gen, &mut tf, unbond);
    let block = gen.next_block_with_txs(&miner_addr, &[staking_tx_unbond(gen.next_upgrade(), b"bond-a"), staking_tx_unbond(gen.next_upgrade(), b"bond-b")]);
    tf.push_instr_load_pow(&block, 0);

    // Bond C unbonds, then retargets in the same block. Each is valid on its own here.
    let bad = gen
        .clone()
        .next_block_with_txs(&miner_addr, &[staking_tx_unbond(gen.next_upgrade(), b"bond-c"), staking_tx_retarget(gen.next_upgrade(), b"bond-c", target, other)]);
    tf.push_instr_load_pow(&bad, SHOULD_FAIL);
    tf.push_instr_expect_pow_chain_length(unbond as usize + 1, 0);
    let block = gen.next_block_with_txs(&miner_addr, &[staking_tx_retarget(gen.next_upgrade(), b"bond-c", target, other)]);
    tf.push_instr_load_pow(&block, 0);

    mine_empty_until(&mut gen, &mut tf, withdraw);
    let block = gen.next_block_with_txs(&miner_addr, &[staking_tx_withdraw(gen.next_upgrade(), b"bond-b", 0, 0)]);
    tf.push_instr_load_pow(&block, 0);

    // Bond A withdrawn twice in one block: two transactions, told apart by their expiry.
    let bad = gen.clone().next_block_with_txs(
        &miner_addr,
        &[staking_tx_withdraw(gen.next_upgrade(), b"bond-a", 0, 0), staking_tx_withdraw(gen.next_upgrade(), b"bond-a", 0, 10_000)],
    );
    tf.push_instr_load_pow(&bad, SHOULD_FAIL);
    tf.push_instr_expect_pow_chain_length(withdraw as usize + 1, 0);
    let block = gen.next_block_with_txs(&miner_addr, &[staking_tx_withdraw(gen.next_upgrade(), b"bond-a", 0, 0)]);
    tf.push_instr_load_pow(&block, 0);
    tf.push_instr_expect_pow_chain_length(withdraw as usize + 2, 0);

    test_bytes(tf.write_to_bytes());
}

/// A test can mine to a key it holds and spend the matured coinbase with a fee: first a plain
/// transparent spend, then a bond creation funded the same way. Each passes the mempool's ZIP-317
/// rules, and the block that mines it credits the fee to its coinbase (NU6 onward requires coinbase
/// outputs to equal the subsidy, less the staking share, plus the fees exactly), so the node
/// accepts the block and drops the transaction from its mempool.
#[test]
fn crosslink_spend_matured_coinbase_with_a_fee() {
    set_test_name(function_name!());
    let mut tf = TF::new(&HARNESS_PARAMETERS);

    let network = regtest_network(&HARNESS_PARAMETERS);
    let key = TestKey::new(b"crosslink test miner");
    let miner = key.address();
    let mut gen = BlockGen::init_at_genesis_plus_1(network, BlockGen::REGTEST_GENESIS_HASH, &miner);
    tf.push_instr_load_pow(&gen.tip, 0);
    while gen.mature_coinbase_for(&key).is_none() {
        tf.push_instr_load_pow(&gen.next_block(&miner), 0);
    }

    let spend = transparent_spend(gen.next_upgrade(), &key, gen.mature_coinbase_for(&key).expect("matured above"));
    tf.push_instr_submit_tx(&spend, 0);
    tf.push_instr_expect_mempool_contains(&spend, 0);

    let block = gen.next_block_with_txs(&miner, &[spend.clone()]);
    tf.push_instr_load_pow(&block, 0);
    tf.push_instr_expect_mempool_absent(&spend, 0);

    // A bond funded by the next matured coinbase output.
    while gen.mature_coinbase_for(&key).is_none() {
        tf.push_instr_load_pow(&gen.next_block(&miner), 0);
    }
    let target = zcash_primitives::bft::FinalizerAddress::create(
        &zebra_crosslink::rng_private_public_key_from_address(b"staking-target").1,
    );
    let bond_seed = b"funded-bond";
    let bond = staking_tx_create_bond_funded(
        gen.next_upgrade(),
        bond_seed,
        target,
        100_000_000,
        &key,
        gen.mature_coinbase_for(&key).expect("matured above"),
    );
    tf.push_instr_submit_tx(&bond, 0);
    tf.push_instr_expect_mempool_contains(&bond, 0);

    let block = gen.next_block_with_txs(&miner, &[bond.clone()]);
    tf.push_instr_load_pow(&block, 0);
    tf.push_instr_expect_mempool_absent(&bond, 0);
    // Stakers are paid every block, so the bond's amount moves on from the one bonded; the block
    // being accepted is what shows the value balanced.
    let bond_key = zebra_crosslink::rng_private_public_key_from_address(bond_seed).2 .0;
    tf.push_instr_expect_bond(bond_key, TF_BOND_ACTIVE, TEST_STAKE_IGNORED, 0);

    test_bytes(tf.write_to_bytes());
}

/// A regtest network for `params` with FeaturenetUpgrade1 at `height`, and the node config that
/// gives the node the same one.
fn network_with_featurenet_upgrade_at(params: &zcash_primitives::bft::ZcashCrosslinkParameters, height: u32) -> (Network, String) {
    let heights = zebra_chain::parameters::testnet::ConfiguredActivationHeights {
        featurenet_upgrade_1: Some(height),
        ..Default::default()
    };
    let node_config = node_config_with_activation_heights(heights);
    let network = test_network(params, &node_config);
    assert_eq!(
        NetworkUpgrade::FeaturenetUpgrade1.activation_height(&network),
        Some(zebra_chain::block::Height(height)),
        "the test network schedules the upgrade where the scenario puts it",
    );
    (network, node_config)
}

/// The staking state one block's PoS issuance acts on, and the two payout rules, written from
/// their descriptions so the scenario below checks the node against something other than itself.
/// Rounding follows the descriptions too: every share rounds down and the remainder goes to the
/// biggest staker.
///
/// Before FeaturenetUpgrade1 (`pay_per_bond`): each active bond earns its stake-weighted share of
/// the issuance, a roster finalizer's bank counting as stake; one tenth of a bond's share, rounded
/// down, goes to its finalizer's bank if that finalizer is in the roster, and the bond keeps the
/// rest; a bank keeps its whole share. From the upgrade on (`pay_split`): one tenth, rounded down,
/// is the finalizer pool and the rest the bond pool; the bond pool is shared by active bonds and
/// roster banks by amount; the finalizer pool by roster finalizers by stake, bonds plus bank.
#[derive(Clone, Debug, PartialEq)]
struct PayoutModel {
    /// (bond key, amount, target finalizer)
    bonds: Vec<([u8; 32], u64, [u8; 32])>,
    banks: std::collections::BTreeMap<[u8; 32], u64>,
    /// The network's roster cap.
    active_roster_max_n: usize,
}

impl PayoutModel {
    /// Finalizers with stake, biggest first, ties to the larger key; the first
    /// `active_roster_max_n` are the roster.
    fn ranked(&self) -> Vec<(u64, [u8; 32])> {
        let mut stakes = std::collections::BTreeMap::<[u8; 32], u64>::new();
        for &(_, amount, target) in &self.bonds {
            *stakes.entry(target).or_insert(0) += amount;
        }
        for (&finalizer, &bank) in &self.banks {
            if bank != 0 {
                *stakes.entry(finalizer).or_insert(0) += bank;
            }
        }
        let mut ranked: Vec<(u64, [u8; 32])> = stakes.into_iter().map(|(finalizer, stake)| (stake, finalizer)).collect();
        ranked.sort_by(|a, b| b.cmp(a));
        ranked
    }

    fn roster(&self) -> Vec<[u8; 32]> {
        self.ranked().into_iter().take(self.active_roster_max_n).map(|(_, finalizer)| finalizer).collect()
    }

    /// The stakers that share a pool: every bond, then every roster bank with something in it.
    /// The biggest takes the remainder; ties go to a bond over a bank, then to the smaller key.
    fn stakers(&self, roster: &[[u8; 32]]) -> (Vec<(bool, [u8; 32], u64)>, usize) {
        let mut stakers: Vec<(bool, [u8; 32], u64)> = self.bonds.iter().map(|&(key, amount, _)| (false, key, amount)).collect();
        for (&finalizer, &bank) in &self.banks {
            if bank != 0 && roster.contains(&finalizer) {
                stakers.push((true, finalizer, bank));
            }
        }
        let biggest = (0..stakers.len())
            .max_by(|&a, &b| {
                let (a_bank, a_key, a_amount) = stakers[a];
                let (b_bank, b_key, b_amount) = stakers[b];
                a_amount.cmp(&b_amount).then(b_bank.cmp(&a_bank)).then(b_key.cmp(&a_key))
            })
            .expect("the scenario has stake");
        (stakers, biggest)
    }

    fn shares(stakers: &[(bool, [u8; 32], u64)], biggest: usize, pool: u64) -> Vec<u64> {
        let total: u64 = stakers.iter().map(|&(_, _, amount)| amount).sum();
        let mut shares: Vec<u64> = stakers.iter().map(|&(_, _, amount)| (amount as u128 * pool as u128 / total as u128) as u64).collect();
        let paid: u64 = shares.iter().sum();
        shares[biggest] += pool - paid;
        shares
    }

    fn pay_per_bond(&mut self, issuance: u64) {
        let roster = self.roster();
        let (stakers, biggest) = self.stakers(&roster);
        let shares = Self::shares(&stakers, biggest, issuance);
        let mut credits = std::collections::BTreeMap::<[u8; 32], u64>::new();
        for (index, &(is_bank, key, _)) in stakers.iter().enumerate() {
            let share = shares[index];
            if is_bank {
                *credits.entry(key).or_insert(0) += share;
                continue;
            }
            let bond = self.bonds.iter_mut().find(|bond| bond.0 == key).expect("a staker that is not a bank is a bond");
            let commission = if roster.contains(&bond.2) { share / 10 } else { 0 };
            bond.1 += share - commission;
            *credits.entry(bond.2).or_insert(0) += commission;
        }
        for (finalizer, credit) in credits {
            *self.banks.entry(finalizer).or_insert(0) += credit;
        }
    }

    fn pay_split(&mut self, issuance: u64) {
        let finalizer_pool = issuance / 10;
        let bond_pool = issuance - finalizer_pool;
        let ranked: Vec<(u64, [u8; 32])> =
            self.ranked().into_iter().take(self.active_roster_max_n).collect();
        let roster: Vec<[u8; 32]> = ranked.iter().map(|&(_, finalizer)| finalizer).collect();

        let (stakers, biggest) = self.stakers(&roster);
        let shares = Self::shares(&stakers, biggest, bond_pool);
        let mut credits = std::collections::BTreeMap::<[u8; 32], u64>::new();
        for (index, &(is_bank, key, _)) in stakers.iter().enumerate() {
            if is_bank {
                *credits.entry(key).or_insert(0) += shares[index];
            } else {
                self.bonds.iter_mut().find(|bond| bond.0 == key).expect("a staker that is not a bank is a bond").1 += shares[index];
            }
        }

        let roster_stake: u64 = ranked.iter().map(|&(stake, _)| stake).sum();
        let mut paid = 0;
        for &(stake, finalizer) in &ranked {
            let share = (stake as u128 * finalizer_pool as u128 / roster_stake as u128) as u64;
            paid += share;
            *credits.entry(finalizer).or_insert(0) += share;
        }
        *credits.entry(ranked[0].1).or_insert(0) += finalizer_pool - paid;

        for (finalizer, credit) in credits {
            *self.banks.entry(finalizer).or_insert(0) += credit;
        }
    }

    fn expect(&self, tf: &mut TF) {
        for &(key, amount, _) in &self.bonds {
            tf.push_instr_expect_bond(key, TF_BOND_ACTIVE, amount, 0);
        }
        for (&finalizer, &bank) in &self.banks {
            tf.push_instr_expect_finalizer_bank(finalizer, bank, 0);
        }
        let bonded: u64 = self.bonds.iter().map(|&(_, amount, _)| amount).sum();
        let banked: u64 = self.banks.values().sum();
        tf.push_instr_expect_pool_totals(bonded, 0, banked, 0);
    }
}

/// The staking issuance of the block at `height`.
fn pos_issuance(network: &Network, height: u32) -> u64 {
    use zebra_chain::parameters::subsidy::{block_subsidy, pos_subsidy};

    let height = zebra_chain::block::Height(height);
    let subsidy = block_subsidy(height, network).expect("a regtest height has a subsidy");
    u64::from(pos_subsidy(height, network, subsidy).expect("the staking share is a valid amount"))
}

/// FeaturenetUpgrade1 changes how a block's staking issuance is paid out. One more finalizer than
/// the roster has seats has a bond each, so the smallest is outside it. The bonds are made in
/// the block before the upgrade, which pays them under the old rule, and the upgrade block pays
/// under the new one. After each, every bond, every bank and the pool totals are checked.
///
/// The roster finalizer outside the roster is what tells the rules apart: before the upgrade its
/// bond keeps its whole share and the finalizers get less than a tenth; from the upgrade on its bond
/// gets its share of nine tenths and the roster always gets the full tenth. With every finalizer
/// in the roster the two rules pay the same, rounding aside.
#[test]
fn crosslink_payout_changes_rule_at_featurenet_upgrade_1() {
    set_test_name(function_name!());
    const UPGRADE: u32 = 20;
    const FINALIZERS: usize = HARNESS_PARAMETERS.active_roster_max_n + 1;
    let (network, node_config) = network_with_featurenet_upgrade_at(&HARNESS_PARAMETERS, UPGRADE);
    let mut tf = TF::new_with_node_config(&HARNESS_PARAMETERS, &node_config);

    let key = TestKey::new(b"crosslink test miner");
    let miner = key.address();
    let mut gen = BlockGen::init_at_genesis_plus_1(network.clone(), BlockGen::REGTEST_GENESIS_HASH, &miner);
    tf.push_instr_load_pow(&gen.tip, 0);
    let inputs = matured_coinbases(&mut tf, &mut gen, &key, FINALIZERS);
    let height = |gen: &BlockGen| gen.tip.coinbase_height().expect("generated blocks have a height").0;
    assert!(height(&gen) + 1 < UPGRADE - 1, "the coinbases mature before the bond block");
    while height(&gen) + 1 < UPGRADE - 1 {
        tf.push_instr_load_pow(&gen.next_block(&miner), 0);
    }

    // Twelve bonds of 1 ZEC and one of 0.1 ZEC, each to its own finalizer, in the block before the
    // upgrade. Powers of 10, so the bond rule that starts with the upgrade is not in play.
    let mut model = PayoutModel { bonds: Vec::new(), banks: Default::default(), active_roster_max_n: HARNESS_PARAMETERS.active_roster_max_n };
    let mut bonds = Vec::new();
    for (index, input) in inputs.into_iter().enumerate() {
        let finalizer = zcash_primitives::bft::FinalizerAddress::create(
            &zebra_crosslink::rng_private_public_key_from_address(format!("payout-finalizer-{index}").as_bytes()).1,
        );
        let amount = if index + 1 == FINALIZERS { 10_000_000 } else { 100_000_000 };
        let bond = staking_tx_create_bond_funded(
            gen.next_upgrade(),
            format!("payout-bond-{index}").as_bytes(),
            finalizer,
            amount,
            &key,
            input,
        );
        model.bonds.push((bond.staking_action().expect("a staking transaction").bond_key(), amount, finalizer.pub_key.0));
        bonds.push(bond);
    }
    assert_eq!(gen.next_upgrade(), NetworkUpgrade::Nu6_3, "the bond block is before the upgrade");
    tf.push_instr_load_pow(&gen.next_block_with_txs(&miner, &bonds), 0);
    model.pay_per_bond(pos_issuance(&network, UPGRADE - 1));
    model.expect(&mut tf);

    // The upgrade block. The two rules must disagree on it, or this scenario shows nothing.
    assert_eq!(gen.next_upgrade(), NetworkUpgrade::FeaturenetUpgrade1, "the next block is the upgrade's");
    let issuance = pos_issuance(&network, UPGRADE);
    let mut old_rule = model.clone();
    old_rule.pay_per_bond(issuance);
    model.pay_split(issuance);
    assert_ne!(old_rule, model, "the two payout rules pay this block the same, so it cannot tell them apart");
    tf.push_instr_load_pow(&gen.next_block(&miner), 0);
    model.expect(&mut tf);
    // And the replay the wallet scanner uses agrees with the node, across the change of rule.
    tf.push_instr_expect_staking_replay_matches(0);

    test_bytes(tf.write_to_bytes());
}

/// From FeaturenetUpgrade1 on, a new bond must be a power of 10 zats. One that is not is accepted
/// in the block before the upgrade and refused in the upgrade block, where a power of 10 is
/// accepted.
#[test]
fn crosslink_new_bonds_must_be_a_power_of_10_from_featurenet_upgrade_1() {
    set_test_name(function_name!());
    const UPGRADE: u32 = 10;
    let (network, node_config) = network_with_featurenet_upgrade_at(&HARNESS_PARAMETERS, UPGRADE);
    let mut tf = TF::new_with_node_config(&HARNESS_PARAMETERS, &node_config);

    let key = TestKey::new(b"crosslink test miner");
    let miner = key.address();
    let mut gen = BlockGen::init_at_genesis_plus_1(network, BlockGen::REGTEST_GENESIS_HASH, &miner);
    tf.push_instr_load_pow(&gen.tip, 0);
    let mut inputs = matured_coinbases(&mut tf, &mut gen, &key, 3).into_iter();
    let height = |gen: &BlockGen| gen.tip.coinbase_height().expect("generated blocks have a height").0;
    assert!(height(&gen) + 1 < UPGRADE - 1, "the coinbases mature before the last block before the upgrade");
    while height(&gen) + 1 < UPGRADE - 1 {
        tf.push_instr_load_pow(&gen.next_block(&miner), 0);
    }
    let target = zcash_primitives::bft::FinalizerAddress::create(
        &zebra_crosslink::rng_private_public_key_from_address(b"power-of-10-target").1,
    );
    let mut bond = |gen: &BlockGen, seed: &[u8], amount: u64| {
        staking_tx_create_bond_funded(gen.next_upgrade(), seed, target, amount, &key, inputs.next().expect("three inputs matured"))
    };

    assert_eq!(gen.next_upgrade(), NetworkUpgrade::Nu6_3);
    let before = bond(&gen, b"before-the-upgrade", 150_000_000);
    tf.push_instr_load_pow(&gen.next_block_with_txs(&miner, &[before]), 0);

    assert_eq!(gen.next_upgrade(), NetworkUpgrade::FeaturenetUpgrade1);
    let refused = bond(&gen, b"after-the-upgrade", 150_000_000);
    tf.push_instr_load_pow_rejected(&gen.clone().next_block_with_txs(&miner, &[refused]), "power of 10");
    let accepted = bond(&gen, b"after-the-upgrade-power-of-10", 100_000_000);
    tf.push_instr_load_pow(&gen.next_block_with_txs(&miner, &[accepted]), 0);
    tf.push_instr_expect_staking_replay_matches(0);

    test_bytes(tf.write_to_bytes());
}

/// Crosslink parameters whose bootstrap a scenario can reach: the roster is the stakes at height
/// 10, and BFT starts at height 111, the first more than MAX_BLOCK_REORG_HEIGHT above it.
const BFT_ROUNDS_PARAMETERS: zcash_primitives::bft::ZcashCrosslinkParameters = zcash_primitives::bft::ZcashCrosslinkParameters {
    bootstrap: zcash_primitives::bft::BftBootstrap::FromChain { staking_height: 0, roster_height: 10, activation_height: 111 },
    ..HARNESS_PARAMETERS
};

/// More than tenderlink's 250 ms send tick, so every ADVANCE_TIME by it runs exactly one tick.
const BFT_TICK_MILLIS: u64 = 251;

/// Bonds the node and three synthetic finalizers and loads blocks up to BFT activation, so the
/// node runs BFT from height 1 with those four. The synthetic finalizers, and the node's stake (a
/// small multiple of theirs, at most half the total, so no decision is the node's alone), are
/// picked so that the node proposes round r of height 1 exactly when `node_proposes[r]`. Which
/// member proposes follows from the stakes and keys alone, so it is known before the node runs.
fn bft_rounds_scene(tf: &mut TF, node_proposes: &[bool]) -> BftRoundsScene {
    const STAKE: u64 = 100_000_000;
    let network = regtest_network(&BFT_ROUNDS_PARAMETERS);
    let key = TestKey::new(b"crosslink test miner");
    let miner = key.address();
    let mut gen = BlockGen::init_at_genesis_plus_1(network, BlockGen::REGTEST_GENESIS_HASH, &miner);
    tf.push_instr_load_pow(&gen.tip, 0);

    let node = zebra_crosslink::rng_private_public_key_from_address(TEST_NODE_FINALIZER_SEED.as_bytes());
    // Within the indices the harness connects by itself.
    let n = SYNTHETIC_FINALIZERS_MAX_N;
    let pub_keys: Vec<[u8; 32]> = (0..n).map(|k| synthetic_finalizer_pub_key(k.into())).collect();
    let (node_stake, finalizers) = [1, 2, 3]
        .into_iter()
        .flat_map(|factor| {
            (0..n).flat_map(move |a| (a + 1..n).flat_map(move |b| (b + 1..n).map(move |c| (factor * STAKE, [a, b, c]))))
        })
        .find(|(node_stake, finalizers)| {
            let members: Vec<([u8; 32], u64)> = std::iter::once((node.2 .0, *node_stake))
                .chain(finalizers.iter().map(|&k| (pub_keys[usize::from(k)], STAKE)))
                .collect();
            node_proposes.iter().enumerate().all(|(round, &is_node)| (bft_proposer(&members, BFT_ROUNDS_PARAMETERS.active_roster_max_n, 1, round as u32) == Some(node.2 .0)) == is_node)
        })
        .expect("some stakes and three synthetic finalizers put the proposer where asked");

    let members = std::iter::once((node.1, node_stake)).chain(finalizers.iter().map(|&k| {
        (zebra_crosslink::rng_private_public_key_from_address(synthetic_finalizer_seed(k.into()).as_bytes()).1, STAKE)
    }));
    let inputs = matured_coinbases(tf, &mut gen, &key, 4);
    let bonds: Vec<Arc<Transaction>> = inputs
        .into_iter()
        .zip(members)
        .enumerate()
        .map(|(i, (input, (signing_key, stake)))| {
            let finalizer = zcash_primitives::bft::FinalizerAddress::create(&signing_key);
            staking_tx_create_bond_funded(gen.next_upgrade(), format!("bft-rounds-bond-{i}").as_bytes(), finalizer, stake, &key, input)
        })
        .collect();
    let height = |gen: &BlockGen| gen.tip.coinbase_height().expect("generated blocks have a height").0;
    assert!(height(&gen) < 10, "the bonds land by the roster height");
    tf.push_instr_load_pow(&gen.next_block_with_txs(&miner, &bonds), 0);
    let mut pow = Vec::new();
    while height(&gen) < 111 {
        let block = gen.next_block(&miner);
        tf.push_instr_load_pow(&block, 0);
        pow.push(block);
    }
    let member_pub_keys: Vec<([u8; 32], u64)> = std::iter::once((node.2 .0, node_stake))
        .chain(finalizers.iter().map(|&k| (pub_keys[usize::from(k)], STAKE)))
        .collect();
    BftRoundsScene { finalizers, node_stake, total_stake: node_stake + 3 * STAKE, member_pub_keys, active_roster_max_n: BFT_ROUNDS_PARAMETERS.active_roster_max_n, pow, gen, miner }
}

/// What `bft_rounds_scene` built.
struct BftRoundsScene {
    finalizers: [u16; 3],
    node_stake: u64,
    total_stake: u64,
    /// The node first, then `finalizers` in order, as `bft_proposer` takes them.
    member_pub_keys: Vec<([u8; 32], u64)>,
    /// The roster cap the scene's network runs.
    active_roster_max_n: usize,
    /// The blocks loaded after the bonds, which include every height above the roster height.
    pow: Vec<Arc<Block>>,
    /// At the last block loaded, for scenarios that extend the chain.
    gen: BlockGen,
    miner: Address,
}

impl BftRoundsScene {
    /// The synthetic finalizer that proposes at `height` and `round`, or TF_BFT_THE_NODE.
    fn proposer(&self, height: u64, round: u32) -> u64 {
        let proposer = bft_proposer(&self.member_pub_keys, self.active_roster_max_n, height, round).expect("a roster has a proposer");
        match self.member_pub_keys.iter().position(|(pub_key, _)| *pub_key == proposer) {
            Some(0) => TF_BFT_THE_NODE,
            Some(i) => self.finalizers[i - 1].into(),
            None => unreachable!("the proposer is a member"),
        }
    }

    fn pow_at(&self, height: u32) -> &Arc<Block> {
        self.pow
            .iter()
            .find(|b| b.coinbase_height().map(|h| h.0) == Some(height))
            .expect("the scene loaded that height")
    }

    /// The BFT genesis the node decides at activation: version 2, finalizing the roster height.
    fn genesis(&self) -> BftBlock {
        let zcash_primitives::bft::BftBootstrap::FromChain { roster_height, .. } = BFT_ROUNDS_PARAMETERS.bootstrap else { unreachable!() };
        bft_proposal(&BFT_ROUNDS_PARAMETERS, 0, FatPointerToBftBlock::null(), 0, &self.tail_above(roster_height))
    }

    /// The sigma blocks above `snapshot`: the headers of a BFT block finalizing it.
    fn tail_above(&self, snapshot: u32) -> Vec<Arc<Block>> {
        let sigma = BFT_ROUNDS_PARAMETERS.bc_confirmation_depth_sigma as u32;
        (snapshot + 1..=snapshot + sigma).map(|h| self.pow_at(h).clone()).collect()
    }

    /// A valid height-1 proposal, on the genesis, finalizing `snapshot`.
    fn height_1_proposal(&self, snapshot: u32) -> BftBlock {
        let parent = FatPointerToBftBlock::from_parts(self.genesis().blake3_hash(), 0, 0, &[]);
        bft_proposal(&BFT_ROUNDS_PARAMETERS, 1, parent, 0, &self.tail_above(snapshot))
    }
}

/// The node proposes at height 1, round 0; three synthetic finalizers prevote and precommit its
/// proposal, and it decides the height: the whole round through the node's own engine, with the
/// clock moved only by the test.
#[test]
fn crosslink_bft_round_decides_the_nodes_proposal() {
    set_test_name(function_name!());
    let mut tf = TF::new(&BFT_ROUNDS_PARAMETERS);
    let finalizers = bft_rounds_scene(&mut tf, &[true]).finalizers;

    tf.push_instr_advance_time(BFT_TICK_MILLIS, 0);
    tf.push_instr_expect_bft_proposer(TF_BFT_THE_NODE, 1, 0, 0);
    tf.push_instr_expect_bft_step(1, 0, None, 0);
    tf.push_instr_advance_time(BFT_TICK_MILLIS, 0);
    tf.push_instr_expect_bft_sent(2, 1, 0, None, 0);

    for k in finalizers {
        tf.push_instr_bft_recv_vote(k, TF_BFT_FOR_NODE_PROPOSAL, 1, 0, &[0; 32], 0);
    }
    tf.push_instr_advance_time(BFT_TICK_MILLIS, 0);
    tf.push_instr_advance_time(BFT_TICK_MILLIS, 0);
    tf.push_instr_expect_bft_step(1, 0, Some(TMStep::Precommit), 0);

    for k in finalizers {
        tf.push_instr_bft_recv_vote(k, TF_BFT_FOR_NODE_PROPOSAL | TF_BFT_PRECOMMIT, 1, 0, &[0; 32], 0);
    }
    tf.push_instr_advance_time(BFT_TICK_MILLIS, 0);
    tf.push_instr_advance_time(BFT_TICK_MILLIS, 0);
    tf.push_instr_expect_bft_step(2, 0, None, 0);

    test_bytes(tf.write_to_bytes());
}

/// As crosslink_bft_round_decides_the_nodes_proposal, but the synthetic finalizers sign their
/// prevotes under the wrong vote namespace. The node must not count them, so it never reaches the
/// precommit step; without this, a harness signing under the wrong namespace could make every
/// BFT scenario pass for the wrong reason.
#[test]
fn crosslink_bft_votes_under_the_wrong_namespace_do_not_count() {
    set_test_name(function_name!());
    let mut tf = TF::new(&BFT_ROUNDS_PARAMETERS);
    let finalizers = bft_rounds_scene(&mut tf, &[true]).finalizers;

    tf.push_instr_advance_time(BFT_TICK_MILLIS, 0);
    tf.push_instr_advance_time(BFT_TICK_MILLIS, 0);
    tf.push_instr_expect_bft_sent(2, 1, 0, None, 0);
    for k in finalizers {
        tf.push_instr_bft_recv_vote(k, TF_BFT_FOR_NODE_PROPOSAL | TF_BFT_WRONG_NAMESPACE, 1, 0, &[0; 32], 0);
    }
    tf.push_instr_advance_time(BFT_TICK_MILLIS, 0);
    tf.push_instr_advance_time(BFT_TICK_MILLIS, 0);
    tf.push_instr_expect_bft_step(1, 0, Some(TMStep::Precommit), SHOULD_FAIL);
    tf.push_instr_expect_bft_sent(1, 1, 0, None, SHOULD_FAIL);

    test_bytes(tf.write_to_bytes());
}

/// The real-node form of tenderlink's line49 split: the round's proposer gives the node one value,
/// B, and every other finalizer precommits another, A. Those precommits are a quorum without the
/// node, so A is decided whatever the node holds; the node must give up B, take A when it
/// arrives, and decide it. Deciding B instead, or nothing, splits the node off the chain.
#[test]
fn crosslink_bft_node_given_another_value_decides_what_the_rest_precommitted() {
    set_test_name(function_name!());
    let mut tf = TF::new(&BFT_ROUNDS_PARAMETERS);
    let mut scene = bft_rounds_scene(&mut tf, &[false]);
    assert!(
        3 * (scene.total_stake - scene.node_stake) > 2 * scene.total_stake,
        "the synthetic finalizers are a quorum without the node"
    );
    let proposer = scene.proposer(1, 0);
    let proposer = u16::try_from(proposer).expect("the node does not propose");

    let value_a = bft_proposal_value(&scene.height_1_proposal(20));
    let value_b = bft_proposal_value(&scene.height_1_proposal(30));
    let (id_a, id_b) = (bft_value_id(&value_a), bft_value_id(&value_b));

    tf.push_instr_advance_time(BFT_TICK_MILLIS, 0);
    tf.push_instr_expect_bft_proposer(proposer.into(), 1, 0, 0);
    tf.push_instr_bft_recv_proposal(proposer, 1, 0, -1, 0, &value_b, 0);
    tf.push_instr_advance_time(BFT_TICK_MILLIS, 0);
    // B is a valid proposal, so the node holds it: without this, an invalid B would leave nothing
    // to give up.
    tf.push_instr_expect_bft_sent(0, 1, 0, Some(&id_b), 0);

    for k in scene.finalizers {
        tf.push_instr_bft_recv_vote(k, TF_BFT_PRECOMMIT, 1, 0, &id_a, 0);
    }
    tf.push_instr_bft_recv_proposal(proposer, 1, 0, -1, 0, &value_a, 0);
    tf.push_instr_advance_time(BFT_TICK_MILLIS, 0);
    tf.push_instr_advance_time(BFT_TICK_MILLIS, 0);
    tf.push_instr_expect_bft_step(2, 0, None, 0);
    // Which value was decided shows in fin, which moves only once the best chain's tip cites the
    // decision. A tip citing A, under the precommits the synthetic finalizers sent, moves fin to
    // A's snapshot only if the node holds A as decided: a node that decided B has no BFT block
    // with A's hash for that citation to name.
    let seeds: Vec<String> = scene.finalizers.iter().map(|&k| synthetic_finalizer_seed(k.into())).collect();
    let seeds: Vec<&[u8]> = seeds.iter().map(|seed| seed.as_bytes()).collect();
    let decided = signed_fat_pointer(&scene.height_1_proposal(20), 1, 0, &seeds, &[0; 32]);
    let (miner, gen) = (scene.miner.clone(), &mut scene.gen);
    gen.next_block(&miner);
    tf.push_instr_load_pow(&point_tip_at_bft(gen, &decided), 0);
    tf.push_instr_expect_finalized_tip(Some(&scene.pow_at(20).hash()), 0);

    test_bytes(tf.write_to_bytes());
}

/// The decide race: the node validates a proposal as Pass, then a depth commit drops the side
/// chain holding its snapshot, then the rest of the roster precommits it. Tenderlink decides on the
/// Pass it cached, and the node's re-validation is now indeterminate. The decision is parked, not
/// asserted on, so the node stays up; fetching the snapshot then shows it conflicts with the
/// committed chain, and BFT is dropped with the record naming that block.
#[test]
fn crosslink_bft_decision_whose_snapshot_was_dropped_is_parked_not_aborted() {
    set_test_name(function_name!());
    let mut tf = TF::new(&BFT_ROUNDS_PARAMETERS);
    let mut scene = bft_rounds_scene(&mut tf, &[false]);
    assert!(
        3 * (scene.total_stake - scene.node_stake) > 2 * scene.total_stake,
        "the synthetic finalizers are a quorum without the node"
    );
    let proposer = u16::try_from(scene.proposer(1, 0)).expect("the node does not propose");

    // A side chain at heights 112..115 off the scene's tip; its first block is the snapshot.
    let miner2 = zcash_keys::address::Address::Transparent(
        zcash_transparent::address::TransparentAddress::PublicKeyHash([1u8; 20]),
    );
    let mut genb = scene.gen.clone();
    let side: Vec<Arc<Block>> = (0..4).map(|_| genb.next_block(&miner2)).collect();
    for block in &side {
        tf.push_instr_load_pow(block, 0);
    }
    let parent = FatPointerToBftBlock::from_parts(scene.genesis().blake3_hash(), 0, 0, &[]);
    let value = bft_proposal_value(&bft_proposal(&BFT_ROUNDS_PARAMETERS, 1, parent, 0, &side[1..4]));
    let id = bft_value_id(&value);

    tf.push_instr_advance_time(BFT_TICK_MILLIS, 0);
    tf.push_instr_bft_recv_proposal(proposer, 1, 0, -1, 0, &value, 0);
    tf.push_instr_advance_time(BFT_TICK_MILLIS, 0);
    // A prevote for it: the node validated it as Pass, and tenderlink has cached that.
    tf.push_instr_expect_bft_sent(0, 1, 0, Some(&id), 0);

    // The main chain runs on until committing its height 112 drops the side chain. One block
    // more publishes the chains as they are after that commit.
    let miner = scene.miner.clone();
    for _ in 112..=212 {
        tf.push_instr_load_pow(&scene.gen.next_block(&miner), 0);
    }
    tf.push_instr_expect_pow_chain_length(213, 0);

    // The rest of the roster precommits it, which decides it on the engine's next tick. The
    // engine then waits on the node's answer, so from there it never settles.
    for k in scene.finalizers {
        tf.push_instr_bft_recv_vote(k, TF_BFT_PRECOMMIT, 1, 0, &id, 0);
    }
    tf.push_instr_advance_time(BFT_TICK_MILLIS, SHOULD_FAIL);
    tf.push_instr_expect_node_alive(0);
    tf.push_instr_expect_bft_dropped(false, None, 0);

    // The parked decision put its snapshot on the decided chain, so the node asks for it by hash.
    // It sits below the finalized tip on a chain the node did not commit.
    tf.push_instr_recv_pow(&side[0], 0, SHOULD_FAIL);
    tf.push_instr_expect_rejection_reason("already finalized", 0);
    tf.push_instr_expect_bft_dropped(true, Some(&side[0].hash()), 0);
    tf.push_instr_expect_node_alive(0);
    tf.push_instr_load_pow(&scene.gen.next_block(&miner), 0);
    tf.push_instr_expect_pow_chain_length(214, 0);

    test_bytes(tf.write_to_bytes());
}

/// Locks the node on B at height 1, round 0, and moves it to round 1 undecided: the node holds the
/// proposal B and sees every other finalizer prevote it, so it precommits B; the others then
/// precommit nil. Returns A, a different valid height-1 value, and B's id.
fn bft_lock_on_b_then_round_1(tf: &mut TF, scene: &BftRoundsScene) -> (Vec<u8>, [u8; 32]) {
    let proposer_0 = u16::try_from(scene.proposer(1, 0)).expect("the node does not propose round 0");
    let value_a = bft_proposal_value(&scene.height_1_proposal(20));
    let value_b = bft_proposal_value(&scene.height_1_proposal(30));
    let id_b = bft_value_id(&value_b);

    tf.push_instr_advance_time(BFT_TICK_MILLIS, 0);
    tf.push_instr_bft_recv_proposal(proposer_0, 1, 0, -1, 0, &value_b, 0);
    tf.push_instr_advance_time(BFT_TICK_MILLIS, 0);
    tf.push_instr_expect_bft_sent(0, 1, 0, Some(&id_b), 0);

    for k in scene.finalizers {
        tf.push_instr_bft_recv_vote(k, 0, 1, 0, &id_b, 0);
    }
    tf.push_instr_advance_time(BFT_TICK_MILLIS, 0);
    tf.push_instr_advance_time(BFT_TICK_MILLIS, 0);
    tf.push_instr_expect_bft_sent(1, 1, 0, Some(&id_b), 0);

    for k in scene.finalizers {
        tf.push_instr_bft_recv_vote(k, TF_BFT_PRECOMMIT | TF_BFT_NIL, 1, 0, &[0; 32], 0);
    }
    // Round 0's precommit timeout is 500 ms.
    for _ in 0..3 {
        tf.push_instr_advance_time(BFT_TICK_MILLIS, 0);
    }
    tf.push_instr_expect_bft_step(1, 1, None, 0);
    (value_a, id_b)
}

/// Tendermint's lock, through the real node: locked on B in round 0, the node must not prevote a
/// fresh value A in round 1, one whose proposal names no round where A gathered a quorum of
/// prevotes. It prevotes nil. A node that dropped its lock could precommit A in a later round
/// after B was decided elsewhere.
#[test]
fn crosslink_bft_node_locked_on_a_value_does_not_prevote_a_fresh_one() {
    set_test_name(function_name!());
    let mut tf = TF::new(&BFT_ROUNDS_PARAMETERS);
    let scene = bft_rounds_scene(&mut tf, &[false, false]);
    let (value_a, _) = bft_lock_on_b_then_round_1(&mut tf, &scene);
    let id_a = bft_value_id(&value_a);
    let proposer_1 = u16::try_from(scene.proposer(1, 1)).expect("the node does not propose round 1");

    tf.push_instr_bft_recv_proposal(proposer_1, 1, 1, -1, 0, &value_a, 0);
    tf.push_instr_advance_time(BFT_TICK_MILLIS, 0);
    tf.push_instr_expect_bft_sent(0, 1, 1, None, 0);
    tf.push_instr_expect_bft_sent(0, 1, 1, Some(&id_a), SHOULD_FAIL);

    test_bytes(tf.write_to_bytes());
}

/// As crosslink_bft_node_locked_on_a_value_does_not_prevote_a_fresh_one, with the node proposing
/// round 1: a locked node proposes the value it is locked on, B, not a new block of its own.
#[test]
fn crosslink_bft_node_locked_on_a_value_proposes_it_again() {
    set_test_name(function_name!());
    let mut tf = TF::new(&BFT_ROUNDS_PARAMETERS);
    let scene = bft_rounds_scene(&mut tf, &[false, true]);
    let (_, id_b) = bft_lock_on_b_then_round_1(&mut tf, &scene);

    tf.push_instr_advance_time(BFT_TICK_MILLIS, 0);
    tf.push_instr_expect_bft_sent(2, 1, 1, Some(&id_b), 0);

    test_bytes(tf.write_to_bytes());
}

#[path = "crosslink/fuzz.rs"]
mod fuzz;

/// Crosslink parameters for twelve seats, the deployed roster cap: a dozen funded bonds land by
/// the roster height, 20, and BFT starts at 121, the first height more than
/// MAX_BLOCK_REORG_HEIGHT above it.
const TWELVE_SEATS_PARAMETERS: zcash_primitives::bft::ZcashCrosslinkParameters = zcash_primitives::bft::ZcashCrosslinkParameters {
    bootstrap: zcash_primitives::bft::BftBootstrap::FromChain { staking_height: 0, roster_height: 20, activation_height: 121 },
    ..HARNESS_PARAMETERS
};

/// Each seat's stake. Rewards start at the BFT activation height, so every snapshot below it holds
/// exactly this much per seat.
const SEAT_STAKE: u64 = 100_000_000;

/// A regtest network with FeaturenetUpgrade2 at `height`, or unscheduled, and the node config
/// that gives the node the same schedule.
fn network_with_featurenet_upgrade_2_at(params: &zcash_primitives::bft::ZcashCrosslinkParameters, height: Option<u32>) -> (Network, String) {
    let Some(height) = height else {
        return (regtest_network(params), String::new());
    };
    let node_config = node_config_with_activation_heights(zebra_chain::parameters::testnet::ConfiguredActivationHeights {
        featurenet_upgrade_2: Some(height),
        ..Default::default()
    });
    let network = test_network(params, &node_config);
    assert_eq!(
        NetworkUpgrade::FeaturenetUpgrade2.activation_height(&network),
        Some(zebra_chain::block::Height(height)),
        "the test network schedules the upgrade where the scenario puts it",
    );
    (network, node_config)
}

/// What `twelve_seats_scene` built.
struct TwelveSeats {
    /// The eleven synthetic finalizers, in roster order.
    finalizers: Vec<u16>,
    /// The node first, then the synthetic finalizers, as `bft_proposer` takes them.
    member_pub_keys: Vec<([u8; 32], u64)>,
    /// The roster cap the scene's network runs.
    active_roster_max_n: usize,
    /// The blocks loaded after the bonds, which include every height above the roster height.
    pow: Vec<Arc<Block>>,
    /// At the last block loaded, for scenarios that extend the chain.
    gen: BlockGen,
    miner: Address,
}

/// Bonds the node and eleven synthetic finalizers at equal stake on `network` and loads blocks up
/// to BFT activation, so the node runs BFT from height 1 with twelve equal seats: 2f+1 is eight of
/// them, at these stakes, and the strict quorum nine. The synthetic finalizers are picked so that the node proposes
/// at each `(height, round)` in `node_proposes` exactly when its flag says so.
fn twelve_seats_scene(tf: &mut TF, network: Network, node_proposes: &[((u64, u32), bool)]) -> TwelveSeats {
    let key = TestKey::new(b"crosslink test miner");
    let miner = key.address();
    let active_roster_max_n = network.crosslink_parameters().active_roster_max_n;
    let mut gen = BlockGen::init_at_genesis_plus_1(network, BlockGen::REGTEST_GENESIS_HASH, &miner);
    tf.push_instr_load_pow(&gen.tip, 0);

    let node = zebra_crosslink::rng_private_public_key_from_address(TEST_NODE_FINALIZER_SEED.as_bytes());
    let members_with = |finalizers: &[u16]| -> Vec<([u8; 32], u64)> {
        std::iter::once((node.2 .0, SEAT_STAKE))
            .chain(finalizers.iter().map(|&k| (synthetic_finalizer_pub_key(k.into()), SEAT_STAKE)))
            .collect()
    };
    // A fixed sequence of candidate sets, so the scene is the same on every run.
    let mut lcg: u64 = 1;
    let finalizers: Vec<u16> = (0..10_000)
        .find_map(|_| {
            let mut picked: Vec<u16> = Vec::new();
            while picked.len() < 11 {
                lcg = lcg.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
                let k = u16::try_from((lcg >> 33) % u64::from(SYNTHETIC_FINALIZERS_MAX_N)).expect("below the synthetic finalizer count");
                if !picked.contains(&k) {
                    picked.push(k);
                }
            }
            let members = members_with(&picked);
            node_proposes
                .iter()
                .all(|&((height, round), is_node)| (bft_proposer(&members, active_roster_max_n, height, round) == Some(node.2 .0)) == is_node)
                .then_some(picked)
        })
        .expect("some eleven synthetic finalizers put the proposer where asked");
    // Roster order, which is the order a certificate's signatures are in.
    let roster_order: Vec<[u8; 32]> = bft_roster_order(&members_with(&finalizers));
    let mut finalizers = finalizers;
    finalizers.sort_by_key(|&k| roster_order.iter().position(|pub_key| *pub_key == synthetic_finalizer_pub_key(k.into())));

    let signing_keys = std::iter::once(node.1).chain(finalizers.iter().map(|&k| {
        zebra_crosslink::rng_private_public_key_from_address(synthetic_finalizer_seed(k.into()).as_bytes()).1
    }));
    let inputs = matured_coinbases(tf, &mut gen, &key, 12);
    let bonds: Vec<Arc<Transaction>> = inputs
        .into_iter()
        .zip(signing_keys)
        .enumerate()
        .map(|(i, (input, signing_key))| {
            let finalizer = zcash_primitives::bft::FinalizerAddress::create(&signing_key);
            staking_tx_create_bond_funded(gen.next_upgrade(), format!("twelve-seats-bond-{i}").as_bytes(), finalizer, SEAT_STAKE, &key, input)
        })
        .collect();
    let height = |gen: &BlockGen| gen.tip.coinbase_height().expect("generated blocks have a height").0;
    assert!(height(&gen) < 20, "the bonds land by the roster height");
    tf.push_instr_load_pow(&gen.next_block_with_txs(&miner, &bonds), 0);
    let mut pow = Vec::new();
    while height(&gen) < 121 {
        let block = gen.next_block(&miner);
        tf.push_instr_load_pow(&block, 0);
        pow.push(block);
    }
    let member_pub_keys = members_with(&finalizers);
    TwelveSeats { finalizers, member_pub_keys, active_roster_max_n, pow, gen, miner }
}

/// The members' keys in the order the node's roster puts them.
fn bft_roster_order(members: &[([u8; 32], u64)]) -> Vec<[u8; 32]> {
    let members: Vec<zcash_primitives::transaction::RosterMember> = members
        .iter()
        .map(|&(pub_key, voting_power)| zcash_primitives::transaction::RosterMember { pub_key, voting_power, ..Default::default() })
        .collect();
    zebra_state::new_network::bft::tenderlink_roster_from_internal(&members, &Default::default())
        .into_iter()
        .map(|member| member.pub_key.0)
        .collect()
}

impl TwelveSeats {
    /// The synthetic finalizer that proposes at `height` and `round`, or TF_BFT_THE_NODE.
    fn proposer(&self, height: u64, round: u32) -> u64 {
        let proposer = bft_proposer(&self.member_pub_keys, self.active_roster_max_n, height, round).expect("a roster has a proposer");
        match self.member_pub_keys.iter().position(|(pub_key, _)| *pub_key == proposer) {
            Some(0) => TF_BFT_THE_NODE,
            Some(i) => self.finalizers[i - 1].into(),
            None => unreachable!("the proposer is a member"),
        }
    }

    fn pow_at(&self, height: u32) -> &Arc<Block> {
        self.pow
            .iter()
            .find(|b| b.coinbase_height().map(|h| h.0) == Some(height))
            .expect("the scene loaded that height")
    }

    /// The sigma blocks above `snapshot`: the headers of a BFT block finalizing it.
    fn tail_above(&self, snapshot: u32) -> Vec<Arc<Block>> {
        let sigma = TWELVE_SEATS_PARAMETERS.bc_confirmation_depth_sigma as u32;
        (snapshot + 1..=snapshot + sigma).map(|h| self.pow_at(h).clone()).collect()
    }

    /// The BFT genesis the node decides at activation: version 2, finalizing the roster height.
    fn genesis(&self) -> BftBlock {
        bft_proposal(&TWELVE_SEATS_PARAMETERS, 0, FatPointerToBftBlock::null(), 0, &self.tail_above(20))
    }

    /// A valid height-1 proposal, on the genesis, finalizing `snapshot`.
    fn height_1_proposal(&self, snapshot: u32) -> BftBlock {
        let parent = FatPointerToBftBlock::from_parts(self.genesis().blake3_hash(), 0, 0, &[]);
        bft_proposal(&TWELVE_SEATS_PARAMETERS, 1, parent, 0, &self.tail_above(snapshot))
    }

    /// The seeds of the first `n` synthetic finalizers in roster order.
    fn seeds(&self, n: usize) -> Vec<String> {
        self.finalizers[..n].iter().map(|&k| synthetic_finalizer_seed(k.into())).collect()
    }
}

/// Eight precommits decide a height whose roster is the stake below FeaturenetUpgrade2, and do
/// not decide the first height whose roster is the stake above it, where nine do: the quorum
/// turns strict where the roster's snapshot crosses the upgrade, through the node's own engine
/// with twelve equal seats. Without the upgrade eight decide both heights. Synthetic finalizers
/// propose both heights, so the test knows both blocks.
///
/// The quorum is counted in stake. Twelve seats of `SEAT_STAKE` are `n = 12 * SEAT_STAKE`, so
/// 2f+1 is `8 * SEAT_STAKE - 1` and needs eight seats, one unit short of two thirds; the strict
/// quorum is `8 * SEAT_STAKE + 1`, nine seats. Seven of twelve is 2f+1 only for unit stakes.
fn bft_quorum_across_featurenet_upgrade_2(upgrade: Option<u32>) {
    let (network, node_config) = network_with_featurenet_upgrade_2_at(&TWELVE_SEATS_PARAMETERS, upgrade);
    let mut tf = TF::new_with_node_config(&TWELVE_SEATS_PARAMETERS, &node_config);
    let scene = twelve_seats_scene(&mut tf, network, &[((1, 0), false), ((2, 0), false)]);
    let proposer_1 = u16::try_from(scene.proposer(1, 0)).expect("the node does not propose height 1");
    let proposer_2 = u16::try_from(scene.proposer(2, 0)).expect("the node does not propose height 2");

    // Height 1's roster is the stake at height 20, below the upgrade. Its block finalizes 60.
    let block_1 = scene.height_1_proposal(60);
    let value_1 = bft_proposal_value(&block_1);
    let id_1 = bft_value_id(&value_1);
    tf.push_instr_advance_time(BFT_TICK_MILLIS, 0);
    tf.push_instr_bft_recv_proposal(proposer_1, 1, 0, -1, 0, &value_1, 0);
    tf.push_instr_advance_time(BFT_TICK_MILLIS, 0);
    tf.push_instr_expect_bft_sent(0, 1, 0, Some(&id_1), 0);
    for &k in &scene.finalizers[..8] {
        tf.push_instr_bft_recv_vote(k, TF_BFT_PRECOMMIT, 1, 0, &id_1, 0);
    }
    tf.push_instr_advance_time(BFT_TICK_MILLIS, 0);
    tf.push_instr_advance_time(BFT_TICK_MILLIS, 0);
    tf.push_instr_expect_bft_step(2, 0, None, 0);

    // Height 2's roster is the stake at 60, above the upgrade when it is scheduled. Its proposal
    // cites height 1 with the eight precommits that decided it.
    let seeds = scene.seeds(8);
    let seeds: Vec<&[u8]> = seeds.iter().map(|seed| seed.as_bytes()).collect();
    let decided_1 = signed_fat_pointer(&block_1, 1, 0, &seeds, &[0; 32]);
    let block_2 = bft_proposal(&TWELVE_SEATS_PARAMETERS, 2, decided_1, 0, &scene.tail_above(80));
    let value_2 = bft_proposal_value(&block_2);
    let id_2 = bft_value_id(&value_2);
    tf.push_instr_bft_recv_proposal(proposer_2, 2, 0, -1, 0, &value_2, 0);
    tf.push_instr_advance_time(BFT_TICK_MILLIS, 0);
    tf.push_instr_expect_bft_sent(0, 2, 0, Some(&id_2), 0);
    for &k in &scene.finalizers[..8] {
        tf.push_instr_bft_recv_vote(k, TF_BFT_PRECOMMIT, 2, 0, &id_2, 0);
    }
    tf.push_instr_advance_time(BFT_TICK_MILLIS, 0);
    tf.push_instr_advance_time(BFT_TICK_MILLIS, 0);
    let strict = upgrade.is_some();
    tf.push_instr_expect_bft_step(3, 0, None, if strict { SHOULD_FAIL } else { 0 });
    if strict {
        for &k in &scene.finalizers[8..9] {
            tf.push_instr_bft_recv_vote(k, TF_BFT_PRECOMMIT, 2, 0, &id_2, 0);
        }
        tf.push_instr_advance_time(BFT_TICK_MILLIS, 0);
        tf.push_instr_advance_time(BFT_TICK_MILLIS, 0);
        tf.push_instr_expect_bft_step(3, 0, None, 0);
    }

    test_bytes(tf.write_to_bytes());
}

#[test]
fn crosslink_bft_quorum_turns_strict_where_the_roster_snapshot_reaches_featurenet_upgrade_2() {
    set_test_name(function_name!());
    // Between the roster height, 20, and height 1's snapshot, 60.
    bft_quorum_across_featurenet_upgrade_2(Some(50));
}

#[test]
fn crosslink_bft_quorum_stays_2f_plus_1_with_featurenet_upgrade_2_unscheduled() {
    set_test_name(function_name!());
    bft_quorum_across_featurenet_upgrade_2(None);
}

/// From FeaturenetUpgrade2 a PoW block's fat pointer must be a certificate for the BFT block it
/// names, and a BFT proposal's previous-block pointer one for its parent. The node decides BFT
/// height 1 through its own engine on nine of twelve precommits; then one PoW block per malformed
/// certificate for that block is offered at the same height, each refused with the upgrade below
/// the scenario and accepted with it unscheduled, beside an honest certificate accepted either
/// way. A height-2 proposal whose previous-block pointer carries forged signatures is refused and
/// accepted the same way.
fn fat_pointer_certificates_across_featurenet_upgrade_2(upgrade: Option<u32>) {
    let (network, node_config) = network_with_featurenet_upgrade_2_at(&TWELVE_SEATS_PARAMETERS, upgrade);
    let mut tf = TF::new_with_node_config(&TWELVE_SEATS_PARAMETERS, &node_config);
    let mut scene = twelve_seats_scene(&mut tf, network, &[((1, 0), false)]);
    let proposer = u16::try_from(scene.proposer(1, 0)).expect("the node does not propose");

    let block_1 = scene.height_1_proposal(40);
    let value = bft_proposal_value(&block_1);
    let id = bft_value_id(&value);
    tf.push_instr_advance_time(BFT_TICK_MILLIS, 0);
    tf.push_instr_bft_recv_proposal(proposer, 1, 0, -1, 0, &value, 0);
    tf.push_instr_advance_time(BFT_TICK_MILLIS, 0);
    tf.push_instr_expect_bft_sent(0, 1, 0, Some(&id), 0);
    for &k in &scene.finalizers[..9] {
        tf.push_instr_bft_recv_vote(k, TF_BFT_PRECOMMIT, 1, 0, &id, 0);
    }
    tf.push_instr_advance_time(BFT_TICK_MILLIS, 0);
    tf.push_instr_advance_time(BFT_TICK_MILLIS, 0);
    tf.push_instr_expect_bft_step(2, 0, None, 0);

    // Height 1's roster is the stake at height 20, at or above the upgrade when it is scheduled.
    let strict = upgrade.is_some();
    let namespace = zebra_state::new_network::bft::namespace_for_bft_height(&[], 1, strict);
    let seeds = scene.seeds(11);
    let seeds: Vec<&[u8]> = seeds.iter().map(|seed| seed.as_bytes()).collect();
    let template = |height: u64, round_bits: u32| -> [u8; 44] {
        let mut template = [0u8; 44];
        template[..32].copy_from_slice(&block_1.blake3_hash().0);
        template[32..40].copy_from_slice(&height.to_le_bytes());
        template[40..44].copy_from_slice(&round_bits.to_le_bytes());
        template
    };
    let decided = template(1, 0x8000_0000);
    let honest = certificate_over(decided, &seeds[..9], &namespace);

    let outsider = (0..SYNTHETIC_FINALIZERS_MAX_N)
        .find(|k| !scene.finalizers.contains(k))
        .map(|k| synthetic_finalizer_seed(k.into()))
        .expect("more synthetic finalizers than seats");
    let mut with_outsider = honest.clone();
    with_outsider.signatures.push(certificate_over(decided, &[outsider.as_bytes()], &namespace).signatures[0].clone());
    let mut duplicate = honest.clone();
    duplicate.signatures.insert(1, honest.signatures[1].clone());
    let mut out_of_order = honest.clone();
    out_of_order.signatures.reverse();
    let mut other_round = honest.clone();
    other_round.vote_for_block_without_finalizer_public_key[40] = 1;
    let mut bad_signature = honest.clone();
    bad_signature.signatures[4].vote_signature[0] ^= 1;
    let mut forged = honest.clone();
    for signature in &mut forged.signatures {
        signature.vote_signature[0] ^= 1;
    }

    let malformed: [(&str, FatPointerToBftBlock); 10] = [
        ("no signatures", certificate_over(decided, &[], &namespace)),
        ("a signer outside the roster", with_outsider),
        ("a duplicate signer", duplicate),
        ("signers out of roster order", out_of_order),
        ("eight of twelve, below the strict quorum", certificate_over(decided, &seeds[..8], &namespace)),
        ("the wrong height", certificate_over(template(2, 0x8000_0000), &seeds[..9], &namespace)),
        ("a round other than the one signed", other_round),
        ("the commit bit clear", certificate_over(template(1, 0), &seeds[..9], &namespace)),
        ("a bad signature", bad_signature),
        ("the namespace before the upgrade", certificate_over(decided, &seeds[..9], &[0; 32])),
    ];
    let miner = scene.miner.clone();
    for (_defect, certificate) in &malformed {
        let mut sibling = scene.gen.clone();
        sibling.next_block(&miner);
        tf.push_instr_load_pow(&point_tip_at_bft(&mut sibling, certificate), if strict { SHOULD_FAIL } else { 0 });
    }
    scene.gen.next_block(&miner);
    let honest_block = point_tip_at_bft(&mut scene.gen, &honest);
    tf.push_instr_load_pow(&honest_block, 0);
    tf.push_instr_expect_pow_chain_length(123, 0);
    if strict {
        tf.push_instr_expect_pow_tip(&honest_block.hash(), 0);
    }

    // A height-2 proposal citing height 1 by hash, with forged signatures or with a certificate.
    let height_2 = |previous: FatPointerToBftBlock| {
        let block = bft_proposal(&TWELVE_SEATS_PARAMETERS, 2, previous, 0, &scene.tail_above(60));
        BftBlockAndFatPointerToItWrap(BftBlockAndFatPointerToIt::from_parts(block, 2, 0, &[]))
    };
    tf.push_instr_check_pos(&height_2(forged), if strict { SHOULD_FAIL } else { 0 });
    tf.push_instr_check_pos(&height_2(honest), 0);

    test_bytes(tf.write_to_bytes());
}

#[test]
fn crosslink_fat_pointer_certificates_are_checked_from_featurenet_upgrade_2() {
    set_test_name(function_name!());
    // Below the roster height, so every BFT height and every PoW block here is past it.
    fat_pointer_certificates_across_featurenet_upgrade_2(Some(15));
}

#[test]
fn crosslink_fat_pointer_certificates_are_hash_only_with_featurenet_upgrade_2_unscheduled() {
    set_test_name(function_name!());
    fat_pointer_certificates_across_featurenet_upgrade_2(None);
}

/// From FeaturenetUpgrade2 the block commitment binds the whole certificate and not only the BFT
/// block it names. Nine of twelve seats is a strict quorum, so one decided vote has many valid
/// certificates: any nine or more of its signatures, in roster order. Without the binding a miner
/// who has one Equihash solve re-rolls the block hash for free by re-choosing which of those
/// signatures the block carries, since the pointer is header bytes outside the commitment. The
/// node decides BFT height 1 through its own engine on nine precommits; the honest PoW block
/// carries the nine-signer certificate, and two copies keep its header, hence its commitment, but
/// carry a certificate over the same vote from another valid signer set: the nine after the first
/// signer, and all ten. Each copy is a strict quorum in roster order under the right namespace, so
/// it passes the certificate check (`admit_fat_pointer`) and every other rule, and only the
/// commitment check, which runs after it, can refuse it: that is what makes these copies isolate
/// the binding. With the upgrade below the scenario the honest block is accepted and each copy
/// refused with the commitment error; with it unscheduled all three are accepted as siblings.
fn fat_pointer_binding_on_a_roster_across_featurenet_upgrade_2(upgrade: Option<u32>) {
    let (network, node_config) = network_with_featurenet_upgrade_2_at(&TWELVE_SEATS_PARAMETERS, upgrade);
    let mut tf = TF::new_with_node_config(&TWELVE_SEATS_PARAMETERS, &node_config);
    let mut scene = twelve_seats_scene(&mut tf, network, &[((1, 0), false)]);
    let proposer = u16::try_from(scene.proposer(1, 0)).expect("the node does not propose");

    let block_1 = scene.height_1_proposal(40);
    let value = bft_proposal_value(&block_1);
    let id = bft_value_id(&value);
    tf.push_instr_advance_time(BFT_TICK_MILLIS, 0);
    tf.push_instr_bft_recv_proposal(proposer, 1, 0, -1, 0, &value, 0);
    tf.push_instr_advance_time(BFT_TICK_MILLIS, 0);
    tf.push_instr_expect_bft_sent(0, 1, 0, Some(&id), 0);
    for &k in &scene.finalizers[..9] {
        tf.push_instr_bft_recv_vote(k, TF_BFT_PRECOMMIT, 1, 0, &id, 0);
    }
    tf.push_instr_advance_time(BFT_TICK_MILLIS, 0);
    tf.push_instr_advance_time(BFT_TICK_MILLIS, 0);
    tf.push_instr_expect_bft_step(2, 0, None, 0);

    // Height 1's roster is the stake at height 20, at or above the upgrade when it is scheduled.
    let strict = upgrade.is_some();
    let namespace = zebra_state::new_network::bft::namespace_for_bft_height(&[], 1, strict);
    let seeds = scene.seeds(10);
    let seeds: Vec<&[u8]> = seeds.iter().map(|seed| seed.as_bytes()).collect();
    let honest = signed_fat_pointer(&block_1, 1, 0, &seeds[..9], &namespace);
    let other_signers = [
        signed_fat_pointer(&block_1, 1, 0, &seeds[1..10], &namespace),
        signed_fat_pointer(&block_1, 1, 0, &seeds[..10], &namespace),
    ];

    let miner = scene.miner.clone();
    scene.gen.next_block(&miner);
    let honest_block = point_tip_at_bft(&mut scene.gen, &honest);
    let with_pointer = |pointer: FatPointerToBftBlock| {
        Arc::new(Block {
            header: Arc::new(BlockHeader { fat_pointer_to_bft_block: pointer, ..*honest_block.header }),
            ..honest_block.as_ref().clone()
        })
    };
    tf.push_instr_load_pow(&honest_block, 0);
    tf.push_instr_expect_pow_chain_length(123, 0);
    tf.push_instr_expect_pow_tip(&honest_block.hash(), 0);
    for certificate in &other_signers {
        assert_eq!(certificate.vote_for_block_without_finalizer_public_key, honest.vote_for_block_without_finalizer_public_key);
        assert_ne!(certificate.signatures, honest.signatures);
        let copy = with_pointer(certificate.clone());
        assert_eq!(copy.header.commitment_bytes, honest_block.header.commitment_bytes);
        assert_ne!(copy.hash(), honest_block.hash());
        tf.push_instr_load_pow(&copy, if strict { SHOULD_FAIL } else { 0 });
        if strict {
            tf.push_instr_expect_rejection_reason("InvalidChainHistoryBlockTxAuthCommitment", 0);
            tf.push_instr_expect_pow_tip(&honest_block.hash(), 0);
        }
        tf.push_instr_expect_pow_chain_length(123, 0);
    }
    tf.push_instr_expect_node_alive(0);

    test_bytes(tf.write_to_bytes());
}

#[test]
fn crosslink_fat_pointer_binding_covers_the_signature_list_from_featurenet_upgrade_2() {
    set_test_name(function_name!());
    // Below the roster height, so every BFT height and every PoW block here is past it.
    fat_pointer_binding_on_a_roster_across_featurenet_upgrade_2(Some(15));
}

#[test]
fn crosslink_fat_pointer_binding_leaves_the_signature_list_free_with_featurenet_upgrade_2_unscheduled() {
    set_test_name(function_name!());
    fat_pointer_binding_on_a_roster_across_featurenet_upgrade_2(None);
}

/// The lock gives way to a later quorum: locked on B in round 0, the node holds a fresh A in round
/// 1 and prevotes nil, but every other finalizer prevotes A, which is a quorum without the node.
/// The node must then precommit A, locking on it in round 1. A node that held its round-0 lock
/// against a later quorum could never join a decision the rest had reached.
#[test]
fn crosslink_bft_node_locked_on_a_value_precommits_a_later_quorum() {
    set_test_name(function_name!());
    let mut tf = TF::new(&BFT_ROUNDS_PARAMETERS);
    let scene = bft_rounds_scene(&mut tf, &[false, false]);
    assert!(
        3 * (scene.total_stake - scene.node_stake) > 2 * scene.total_stake,
        "the synthetic finalizers are a quorum without the node"
    );
    let (value_a, id_b) = bft_lock_on_b_then_round_1(&mut tf, &scene);
    let id_a = bft_value_id(&value_a);
    let proposer_1 = u16::try_from(scene.proposer(1, 1)).expect("the node does not propose round 1");

    tf.push_instr_bft_recv_proposal(proposer_1, 1, 1, -1, 0, &value_a, 0);
    tf.push_instr_advance_time(BFT_TICK_MILLIS, 0);
    tf.push_instr_expect_bft_sent(0, 1, 1, None, 0);

    for k in scene.finalizers {
        tf.push_instr_bft_recv_vote(k, 0, 1, 1, &id_a, 0);
    }
    tf.push_instr_advance_time(BFT_TICK_MILLIS, 0);
    tf.push_instr_advance_time(BFT_TICK_MILLIS, 0);
    tf.push_instr_expect_bft_sent(1, 1, 1, Some(&id_a), 0);
    tf.push_instr_expect_bft_sent(1, 1, 1, Some(&id_b), SHOULD_FAIL);

    test_bytes(tf.write_to_bytes());
}

/// Height 1 decided by the three synthetic finalizers, its proposal signed by `signer`'s key in
/// `signed_valid_round` and loaded as made in `valid_round`. The proposer at 1.0 is a synthetic
/// finalizer.
fn signed_decision_scene(signer: Option<SignerOf>, signed_valid_round: i64, valid_round: i64, flags: u32) -> TF {
    let mut tf = TF::new(&BFT_ROUNDS_PARAMETERS);
    let scene = bft_rounds_scene(&mut tf, &[false]);
    let proposer = scene.proposer(1, 0);
    assert_ne!(proposer, TF_BFT_THE_NODE, "the scene puts a synthetic finalizer at 1.0");
    // Waits for BFT to have started, so height 1 is the one being decided.
    tf.push_instr_advance_time(BFT_TICK_MILLIS, 0);

    let block = scene.height_1_proposal(20);
    let seeds: Vec<String> = scene.finalizers.iter().map(|&k| synthetic_finalizer_seed(k.into())).collect();
    let seeds: Vec<&[u8]> = seeds.iter().map(|seed| seed.as_bytes()).collect();
    let fat_ptr = signed_fat_pointer(&block, 1, 0, &seeds, &[0; 32]);
    let sigs = match signer {
        None => Vec::new(),
        Some(signer) => {
            let k = match signer {
                SignerOf::Proposer => proposer,
                SignerOf::Another => scene.finalizers.iter().map(|&k| u64::from(k)).find(|&k| k != proposer).unwrap(),
            };
            let key = zebra_crosslink::rng_private_public_key_from_address(synthetic_finalizer_seed(k).as_bytes()).1;
            bft_proposal_sigs(&key, &block, 0, signed_valid_round, &[0; 32])
        }
    };
    let data = BftBlockAndFatPointerToItWrap(BftBlockAndFatPointerToIt { block, fat_ptr });
    tf.push_instr_load_pos_signed(&data, &sigs, valid_round, flags);
    tf
}

#[derive(Clone, Copy)]
enum SignerOf {
    Proposer,
    Another,
}

/// A decided height whose proposal signatures are the round's proposer's is taken.
#[test]
fn crosslink_load_pos_with_the_proposers_signatures() {
    set_test_name(function_name!());
    let mut tf = signed_decision_scene(Some(SignerOf::Proposer), -1, -1, 0);
    tf.push_instr_expect_pos_chain_length(2, 0);
    test_bytes(tf.write_to_bytes());
}

/// Signatures by a finalizer that is not the round's proposer are refused...
#[test]
fn crosslink_load_pos_refuses_another_finalizers_signatures() {
    set_test_name(function_name!());
    let mut tf = signed_decision_scene(Some(SignerOf::Another), -1, -1, SHOULD_FAIL);
    tf.push_instr_expect_rejection_reason("is not signed by", 0);
    tf.push_instr_expect_pos_chain_length(1, 0);
    test_bytes(tf.write_to_bytes());
}

/// ...as are the proposer's own, made in another valid round than the one the load names: the
/// valid round is part of what each chunk signature covers...
#[test]
fn crosslink_load_pos_refuses_signatures_from_another_valid_round() {
    set_test_name(function_name!());
    let mut tf = signed_decision_scene(Some(SignerOf::Proposer), 0, -1, SHOULD_FAIL);
    tf.push_instr_expect_rejection_reason("is not signed by", 0);
    test_bytes(tf.write_to_bytes());
}

/// ...and none at all: above the genesis height, a decision has a proposer who signed it.
#[test]
fn crosslink_load_pos_refuses_a_decision_with_no_signatures() {
    set_test_name(function_name!());
    let mut tf = signed_decision_scene(None, -1, -1, SHOULD_FAIL);
    tf.push_instr_expect_rejection_reason("0 proposal signatures", 0);
    test_bytes(tf.write_to_bytes());
}
