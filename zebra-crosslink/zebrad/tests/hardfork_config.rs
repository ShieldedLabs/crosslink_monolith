use std::path::Path;

use zebrad::config::ZebradConfig;

fn fixtures_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/common/hardfork_configs")
}

#[test]
fn hardfork_valid_config_loads() {
    ZebradConfig::load(Some(fixtures_dir().join("valid.toml")))
        .expect("valid hardfork config should load");
}

#[test]
fn hardfork_invalid_configs_rejected() {
    let cases = [
        ("misaligned-height.toml", "must be a multiple of the staking period"),
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
