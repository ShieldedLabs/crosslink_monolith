use std::path::Path;

use zcash_primitives::bft::{finalizer_key_from_seed, FinalizerAddress, PubKeyID};
use zebra_chain::parameters::{SlashForkConfig, SlashForkSchedule};
use zebrad::config::ZebradConfig;

// The existing fixture's staking-aligned activation and certificate heights.
const ACTIVATION_HEIGHT: u64 = 20_736;
const CERTIFICATE_HEIGHT: u64 = 10;

fn address(seed: &[u8]) -> FinalizerAddress {
    FinalizerAddress::create(&finalizer_key_from_seed(seed).0)
}

fn parse_rule(entries: &[&str]) -> Result<SlashForkConfig, toml::de::Error> {
    let entries = entries
        .iter()
        .map(|s| format!("{s:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    toml::from_str(&format!(
        "pow_activation_height = {ACTIVATION_HEIGHT}\nbft_certificate_height = {CERTIFICATE_HEIGHT}\nterminated_finalizers = [{entries}]\n",
    ))
}

#[test]
fn slashfork_address_and_legacy_key_are_consensus_equivalent() {
    let address = address(b"slashfork-compatible");
    let encoded = address.encode();
    let legacy = address.pub_key.to_string();
    let signed_rule = parse_rule(&[&encoded]).unwrap();
    let legacy_rule = parse_rule(&[&legacy]).unwrap();
    assert_eq!(signed_rule, legacy_rule);
    assert_eq!(signed_rule.terminated_finalizers, vec![address.pub_key]);

    // Independent expected layout: heights, count, then raw key bytes (no signature).
    let mut expected = ACTIVATION_HEIGHT.to_le_bytes().to_vec();
    expected.extend_from_slice(&CERTIFICATE_HEIGHT.to_le_bytes());
    expected.extend_from_slice(&1u32.to_le_bytes());
    expected.extend_from_slice(&address.pub_key.0);
    for rule in [&signed_rule, &legacy_rule] {
        let mut bytes = Vec::new();
        rule.zcash_serialize(&mut bytes).unwrap();
        assert_eq!(bytes, expected);
        assert_eq!(
            SlashForkConfig::zcash_deserialize(bytes.as_slice()).unwrap(),
            *rule
        );
    }
    let signed_schedule = SlashForkSchedule::new(vec![signed_rule]);
    let legacy_schedule = SlashForkSchedule::new(vec![legacy_rule]);
    assert_eq!(signed_schedule, legacy_schedule);
    for height in [
        CERTIFICATE_HEIGHT - 1,
        CERTIFICATE_HEIGHT,
        CERTIFICATE_HEIGHT + 1,
    ] {
        for strict_quorum in [false, true] {
            assert_eq!(
                zebra_state::new_network::bft::namespace_for_bft_height(
                    signed_schedule.rules(),
                    height,
                    strict_quorum,
                ),
                zebra_state::new_network::bft::namespace_for_bft_height(
                    legacy_schedule.rules(),
                    height,
                    strict_quorum,
                ),
            );
        }
    }
}

#[test]
fn slashfork_legacy_byte_order_and_mixed_formats_are_preserved() {
    // Non-palindromic bytes catch accidental reversal changes.
    let legacy = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
    let mut expected_key = [0u8; 32];
    let last_index = expected_key.len() - 1;
    for (i, byte) in expected_key.iter_mut().enumerate() {
        *byte = (last_index - i) as u8;
    }
    let encoded = address(b"slashfork-mixed").encode();
    let parsed = parse_rule(&[legacy, &encoded]).unwrap();
    assert_eq!(
        parsed.terminated_finalizers,
        vec![
            PubKeyID(expected_key),
            FinalizerAddress::decode_and_verify(&encoded).unwrap(),
        ]
    );
    assert_eq!(
        parse_rule(&[&legacy.to_uppercase()])
            .unwrap()
            .terminated_finalizers,
        vec![PubKeyID(expected_key)]
    );
    let serialized = toml::to_string(&parsed).unwrap();
    assert_eq!(
        toml::from_str::<SlashForkConfig>(&serialized).unwrap(),
        parsed
    );
}

#[test]
fn slashfork_duplicate_keys_rejected_across_formats() {
    let address = address(b"slashfork-duplicate");
    let encoded = address.encode();
    let legacy = address.pub_key.to_string();
    for entries in [
        [&legacy, &encoded],
        [&encoded, &legacy],
        [&encoded, &encoded],
        [&legacy, &legacy],
    ] {
        assert!(parse_rule(&entries.map(String::as_str))
            .unwrap_err()
            .to_string()
            .contains("duplicate finalizer"));
    }
}

#[test]
fn slashfork_invalid_addresses_and_unsupported_lengths_rejected() {
    let valid = address(b"slashfork-invalid");
    let encoded = valid.encode();
    let mut bad_signature = valid;
    bad_signature.sig.0[0] ^= 1;
    let mut bad_key = valid;
    bad_key.pub_key.0[0] ^= 1;
    for entry in [
        bad_signature.encode(),
        bad_key.encode(),
        encoded.replacen("zfinv1", "zfinv2", 1),
        format!("{}=", &encoded[..encoded.len() - 1]),
        format!("{}+", &encoded[..encoded.len() - 1]),
        format!("zfinv1{}", "é".repeat(64)),
    ] {
        assert!(parse_rule(&[&entry])
            .unwrap_err()
            .to_string()
            .contains("invalid finalizer address"));
    }
    for entry in [
        String::new(),
        "a".repeat(63),
        "a".repeat(65),
        encoded[..encoded.len() - 1].to_owned(),
        format!("{encoded}A"),
        "a".repeat(192),
    ] {
        assert!(parse_rule(&[&entry])
            .unwrap_err()
            .to_string()
            .contains("64 hex characters"));
    }
    for entry in ["g".repeat(64), "é".repeat(32)] {
        assert!(parse_rule(&[&entry]).is_err());
    }
    assert!(toml::from_str::<SlashForkConfig>(&format!(
        "pow_activation_height = {ACTIVATION_HEIGHT}\nbft_certificate_height = {CERTIFICATE_HEIGHT}\nterminated_finalizers = [1]\n",
    )).is_err());
}

#[test]
fn slashfork_signed_config_loads_and_merges_like_legacy() {
    let address = address(b"slashfork-config-load");
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("zebrad.toml");
    let mut loaded = Vec::new();
    for entry in [address.encode(), address.pub_key.to_string()] {
        std::fs::write(&path, format!(
            "[[crosslink.slashforks]]\npow_activation_height = {ACTIVATION_HEIGHT}\nbft_certificate_height = {CERTIFICATE_HEIGHT}\nterminated_finalizers = [{entry:?}]\n",
        )).unwrap();
        loaded.push(ZebradConfig::load(Some(path.clone())).unwrap());
    }
    assert_eq!(
        loaded[0].crosslink.slashforks,
        loaded[1].crosslink.slashforks
    );
}

fn fixtures_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/common/slashfork_configs")
}

#[test]
fn slashfork_valid_config_loads() {
    ZebradConfig::load(Some(fixtures_dir().join("valid.toml")))
        .expect("valid slashfork config should load");
}

#[test]
fn slashfork_invalid_configs_rejected() {
    let cases = [
        (
            "misaligned-height.toml",
            "must be a multiple of the staking period",
        ),
        ("zero-height.toml", "must be greater than zero"),
        ("duplicate-finalizers.toml", "duplicate finalizer"),
        ("empty-finalizers.toml", "must list at least one finalizer"),
    ];

    for (file, expected_msg) in cases {
        let error = ZebradConfig::load(Some(fixtures_dir().join(file)))
            .err()
            .unwrap_or_else(|| panic!("{file} should have been rejected at config load"));

        let mut rendered = format!("{error}\n{error:?}");
        let mut source = std::error::Error::source(&error);
        while let Some(inner) = source {
            rendered.push('\n');
            rendered.push_str(&inner.to_string());
            source = inner.source();
        }

        assert!(
            rendered.contains(expected_msg),
            "{file}: expected error containing {expected_msg:?}, got:\n{rendered}",
        );
    }
}
