use zebra_chain::parameters;

/// Get the default all nu activated at 1, Network
#[must_use]
pub fn current_nus_configured_in_block_one_regtest_net() -> parameters::Network {
    parameters::Network::new_regtest(all_height_one_nus().into())
}

/// Get sequentially activated (1,2,3,4,5,6,7,8) nus network
#[must_use]
pub fn nus_configured_in_sequence_regtest_net() -> parameters::Network {
    parameters::Network::new_regtest(sequential_height_nus().into())
}

#[must_use]
pub fn sequential_height_nus() -> parameters::testnet::ConfiguredActivationHeights {
    parameters::testnet::ConfiguredActivationHeights {
        before_overwinter: Some(1),
        overwinter: Some(2),
        sapling: Some(3),
        blossom: Some(4),
        heartwood: Some(5),
        canopy: Some(6),
        nu5: Some(7),
        nu6: Some(8),
        // see https://zips.z.cash/#nu6-1-candidate-zips for info on NU6.1
        nu6_1: Some(9),
        nu7: None,
    }
}
#[must_use]
pub fn all_height_one_nus() -> parameters::testnet::ConfiguredActivationHeights {
    parameters::testnet::ConfiguredActivationHeights {
        before_overwinter: Some(1),
        overwinter: Some(1),
        sapling: Some(1),
        blossom: Some(1),
        heartwood: Some(1),
        canopy: Some(1),
        nu5: Some(1),
        nu6: Some(1),
        // see https://zips.z.cash/#nu6-1-candidate-zips for info on NU6.1
        nu6_1: Some(1),
        nu7: None,
    }
}
