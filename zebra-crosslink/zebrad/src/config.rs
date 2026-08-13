//! Zebrad Config
//!
//! See instructions in `commands.rs` to specify the path to your
//! application's configuration file and/or command-line options
//! for specifying it.

use std::{collections::HashMap, path::PathBuf};

use serde::{Deserialize, Serialize};
use zebra_rpc::config::mining::{default_miner_address, MinerAddressType};

use crate::components::With;

/// Centralized, case-insensitive suffix-based deny-list to ban setting config fields with
/// environment variables if those config field names end with any of these suffixes.
const DENY_CONFIG_KEY_SUFFIX_LIST: [&str; 5] = [
    "password",
    "secret",
    "token",
    // Block raw cookies only if a field is literally named "cookie".
    // (Paths like cookie_dir are not affected.)
    "cookie",
    // Only raw private keys; paths like *_private_key_path are not affected.
    "private_key",
];

/// Returns true if a leaf key name should be considered sensitive and blocked
/// from environment variable overrides.
fn is_sensitive_leaf_key(leaf_key: &str) -> bool {
    let key = leaf_key.to_ascii_lowercase();
    DENY_CONFIG_KEY_SUFFIX_LIST
        .iter()
        .any(|deny_suffix| key.ends_with(deny_suffix))
}

/// Configuration for `zebrad`.
///
/// The `zebrad` config is a TOML-encoded version of this structure. The meaning
/// of each field is described in the documentation, although it may be necessary
/// to click through to the sub-structures for each section.
///
/// The path to the configuration file can also be specified with the `--config` flag when running Zebra.
///
/// The default path to the `zebrad` config is platform dependent, based on
/// [`dirs::preference_dir`](https://docs.rs/dirs/latest/dirs/fn.preference_dir.html):
///
/// | Platform | Value                                 | Example                                        |
/// | -------- | ------------------------------------- | ---------------------------------------------- |
/// | Linux    | `$XDG_CONFIG_HOME` or `$HOME/.config` | `/home/alice/.config/zebrad.toml`              |
/// | macOS    | `$HOME/Library/Preferences`           | `/Users/Alice/Library/Preferences/zebrad.toml` |
/// | Windows  | `{FOLDERID_RoamingAppData}`           | `C:\Users\Alice\AppData\Local\zebrad.toml`     |
#[derive(Clone, Default, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct ZebradConfig {
    /// Consensus configuration
    //
    // These configs use full paths to avoid a rustdoc link bug (#7048).
    pub consensus: zebra_consensus::config::Config,

    /// Metrics configuration
    pub metrics: crate::components::metrics::Config,

    /// Networking configuration
    pub network: zebra_network::config::Config,

    /// State configuration
    pub state: zebra_state::config::Config,

    /// Tracing configuration
    pub tracing: crate::components::tracing::Config,

    /// Sync configuration
    pub sync: crate::components::sync::Config,

    /// Mempool configuration
    pub mempool: crate::components::mempool::Config,

    /// Block notify configuration
    pub notify: crate::components::notify::Config,

    /// RPC configuration
    pub rpc: zebra_rpc::config::rpc::Config,

    /// Mining configuration
    pub mining: zebra_rpc::config::mining::Config,

    /// Health check HTTP server configuration.
    ///
    /// See the Zebra Book for details and examples:
    /// <https://zebra.zfnd.org/user/health.html>
    pub health: crate::components::health::Config,

    /// zcashd-compat mode configuration.
    pub zcashd_compat: crate::components::zcashd_compat::Config,
    /// CrossLink configuration
    pub crosslink: zebra_crosslink::config::Config,
}

impl ZebradConfig {
    /// Loads the configuration from the conventional sources.
    ///
    /// Configuration is loaded from three sources, in order of precedence:
    /// 1. Environment variables with `ZEBRA_` prefix (highest precedence)
    /// 2. TOML configuration file (if provided)
    /// 3. Hard-coded defaults (lowest precedence)
    ///
    /// Environment variables use the format `ZEBRA_SECTION__KEY` where:
    /// - `SECTION` is the configuration section (e.g., `network`, `rpc`)
    /// - `KEY` is the configuration key within that section
    /// - Double underscores (`__`) separate nested keys
    ///
    /// # Security
    ///
    /// Environment variables whose leaf key names end with sensitive suffixes (case-insensitive)
    /// will cause configuration loading to fail with an error: `password`, `secret`, `token`, `cookie`, `private_key`.
    /// This prevents both silent misconfigurations and process table exposure of sensitive values.
    ///
    /// See [`DENY_CONFIG_KEY_SUFFIX_LIST`] and [`is_sensitive_leaf_key()`] above
    ///
    /// # Examples
    /// - `ZEBRA_NETWORK__NETWORK=Testnet` sets `network.network = "Testnet"`
    /// - `ZEBRA_RPC__LISTEN_ADDR=127.0.0.1:8232` sets `rpc.listen_addr = "127.0.0.1:8232"`
    pub fn load(config_path: Option<PathBuf>) -> Result<Self, config::ConfigError> {
        Self::load_with_env(config_path, "ZEBRA")
    }

    /// Loads configuration using a caller-provided environment variable prefix.
    ///
    /// This allows callers that need multiple configs in the same process (e.g.,
    /// the `copy-state` command) to keep overrides separate. For example:
    /// - Source/base config uses `ZEBRA_...` env vars (default prefix)
    /// - Target config uses `ZEBRA_TARGET_...` env vars
    ///
    /// The nested key separator remains `__`, e.g., `ZEBRA_TARGET_STATE__CACHE_DIR`.
    pub fn load_with_env(
        config_path: Option<PathBuf>,
        env_prefix: &str,
    ) -> Result<Self, config::ConfigError> {
        // 1. Start with an empty `config::Config` builder (no pre-populated values).
        // We merge sources, then deserialize into `ZebradConfig`, which uses
        // `ZebradConfig::default()` wherever keys are missing.
        let mut builder = config::Config::builder();

        // 2. Add TOML configuration file as a source if provided
        if let Some(path) = config_path {
            builder = builder.add_source(
                config::File::from(path)
                    .format(config::FileFormat::Toml)
                    .required(true),
            );
        }

        // 3. Load from environment variables (with a sensitive-leaf deny-list)
        // Use the provided prefix and `__` as separator for nested keys.
        // We filter the raw environment first, then let config-rs parse types via try_parsing(true).
        let mut filtered_env: HashMap<String, String> = HashMap::new();
        let required_prefix = format!("{}_", env_prefix);
        for (key, value) in std::env::vars() {
            if let Some(without_prefix) = key.strip_prefix(&required_prefix) {
                // Check for sensitive keys on the stripped key.
                let parts: Vec<&str> = without_prefix.split("__").collect();
                if let Some(leaf) = parts.last() {
                    if is_sensitive_leaf_key(leaf) {
                        return Err(config::ConfigError::Message(format!(
                            "Environment variable '{}' contains sensitive key '{}' which cannot be overridden via environment variables. \
                             Use the configuration file instead to prevent process table exposure.",
                            key, leaf
                        )));
                    }
                }

                // When providing a `source` map, the keys should not have the prefix.
                filtered_env.insert(without_prefix.to_string(), value);
            }
        }

        // When using `source`, we provide a map of already-filtered and processed
        // keys, so we use a default `Environment` without a prefix.
        builder = builder.add_source(
            config::Environment::default()
                .separator("__")
                .try_parsing(true)
                .source(Some(filtered_env)),
        );

        // Build the configuration
        let raw_config = builder.build()?;

        // println!("raw config: {raw_config:#?}");
        // Deserialize into our struct, which will use defaults for any missing fields
        let mut config: Self = raw_config.clone().try_deserialize()?;

        // set crosslink testnet defaults based on whether they were *specified*, which is slightly
        // more precise than whether they are currently Some/None
        {
            let defaults = Self::crosslink_default();

            match raw_config.get("network.network") {
                Err(config::ConfigError::NotFound(_)) |
                    Ok("Testnet") =>
                    config.network.network = defaults.network.network,

                _ => {}
            }

            if let Err(config::ConfigError::NotFound(_)) = raw_config.get_array("crosslink.bft_peers") {
                config.crosslink.bft_peers = defaults.crosslink.bft_peers;
            }
            if let Err(config::ConfigError::NotFound(_)) = raw_config.get_array("network.initial_testnet_peers") {
                config.network.initial_testnet_peers = defaults.network.initial_testnet_peers;
            }
            if let Err(config::ConfigError::NotFound(_)) = raw_config.get_array("state.network_initial_peers") {
                config.state.network_initial_peers = defaults.state.network_initial_peers ;
            }

            if let Err(config::ConfigError::NotFound(_)) = raw_config.get_int("mempool.debug_enable_at_height") {
                config.mempool.debug_enable_at_height = defaults.mempool.debug_enable_at_height;
            }
            if let Err(config::ConfigError::NotFound(_)) = raw_config.get_string("mining.internal_miner") {
                config.mining.internal_miner = defaults.mining.internal_miner;
            }
            if let Err(config::ConfigError::NotFound(_)) = raw_config.get_string("rpc.listen_addr") {
                config.rpc.listen_addr = defaults.rpc.listen_addr;
            }
            if let Err(config::ConfigError::NotFound(_)) = raw_config.get_bool("rpc.enable_cookie_auth") {
                config.rpc.enable_cookie_auth = defaults.rpc.enable_cookie_auth;
            }
        }

        // Namespace the cache directory at load time. This is a transformation applied on
        // top of whatever cache_dir is in effect (the default *or* a user-specified one),
        // not a default value, so it is deliberately NOT serialized by `crosslink_default`
        // / `generate` — load always appends it, which keeps generate-then-load equivalent
        // to running with no config file.
        config.state
            .cache_dir
            .push("zebra_crosslink_workshop_season_one_v3_ehtedht_cache_delete_me");

        // Merge user-led hardforks with the ones shipped in the executable, validate
        // them, and store the canonical (sorted, deduplicated) list back. Building
        // the schedule panics on conflicting/interleaving rules, so a bad config is
        // rejected at load time. `disable_shipped_hardforks` skips the built-in rules
        // so a testnet operator can specify the whole schedule manually.
        let user_hardforks = config.crosslink.hardforks.clone();
        let schedule = if config.crosslink.disable_shipped_hardforks {
            zebra_chain::parameters::HardForkSchedule::new_without_shipped(user_hardforks)
        } else {
            zebra_chain::parameters::HardForkSchedule::new(user_hardforks)
        };
        config.crosslink.hardforks = schedule.rules().to_vec();

        // if let zcash_protocol::consensus::Network::TestNetwork(_) = config.network.network
        Ok(config)
    }

    pub fn crosslink_default() -> Self {
        use zebra_chain::{
            block::Height,
            parameters::{subsidy::FundingStreamReceiver, testnet, Magic},
        };

        Self {
            crosslink: zebra_crosslink::config::Config {
                bft_peers: vec![
                    "70.34.201.146:12301".to_owned(), // @terminator
                    "70.34.209.22:12301".to_owned(),  // @terminator
                    "70.34.195.191:12301".to_owned(), // @terminator
                    "70.34.209.18:12301".to_owned(),  // @terminator
                ],
                ..Default::default()
            },

            state: zebra_state::config::Config {
                network_initial_peers: vec![
                    "[::ffff:70.34.201.146]:12001:1fgEw5Nx:_BA-d-zgMDO3lj5R-FgL3VwJQofnPVZarZSUzx9ZMhs".to_owned(), // @terminator
                    "[::ffff:70.34.209.22]:12001:1fgEw5Nx:2huJ7vzzieTrT_dFMaQwhS0fSGZFatCeBXNFCXTfJCs".to_owned(),  // @terminator
                    "[::ffff:70.34.195.191]:12001:1fgEw5Nx:iezUrR8zwiqzt1__9Ex0OiqQ1O0gbipHuuKwCHwQggo".to_owned(), // @terminator
                    "[::ffff:70.34.209.18]:12001:1fgEw5Nx:9nM4V10MYltC-ShN4OaEQlvDiFHEJtsOYmOroLBanQM".to_owned(),  // @terminator
                ],
                ..Default::default()
            },

            network: zebra_network::config::Config {
                initial_testnet_peers: {
                    let mut peers = indexmap::IndexSet::new();
                    peers.insert("70.34.201.146:8233".to_owned()); // @terminator
                    peers.insert("70.34.209.22:8233".to_owned());  // @terminator
                    peers.insert("70.34.195.191:8233".to_owned()); // @terminator
                    peers.insert("70.34.209.18:8233".to_owned());  // @terminator
                    peers
                },

                network: testnet::Parameters::build()
                    // .with_network_name("Crosslink_Testnet_0")
                    .with_network_magic(Magic([67, 108, 84, 48]))
                    .expect("Crosslink testnet magic is not a reserved value")
                    .with_slow_start_interval(Height(0))
                    .with_genesis_hash("05a60a92d99d85997cce3b87616c089f6124d7342af37106edc76126334a2c38")
                    .expect("Crosslink testnet genesis hash is well-formed")
                    .with_funding_streams(vec![testnet::ConfiguredFundingStreams {
                        height_range: Some(Height(1)..Height(99_999_999)),
                        recipients: Some(vec![testnet::ConfiguredFundingStreamRecipient {
                            receiver: FundingStreamReceiver::MajorGrants,
                            numerator: 20,
                            addresses: Some(vec!["t27tjLaUJZ53JKqWPkgd1XCTNWF636eLQRg".to_string()]),
                        }]),
                    }])
                .to_network()
                .expect("Crosslink testnet parameters are valid"),

                ..Default::default()
            },

            rpc: zebra_rpc::config::rpc::Config {
                listen_addr: Some("127.0.0.1:8232".parse().unwrap()),
                enable_cookie_auth: false,
                ..Default::default()
            },

            mempool: crate::components::mempool::Config {
                debug_enable_at_height: Some(0),
                ..Default::default()
            },

            mining: zebra_rpc::config::mining::Config {
                internal_miner: true,
                ..Default::default()
            },

            ..Default::default()
        }
    }
}

impl With<MinerAddressType> for ZebradConfig {
    fn with(mut self, miner_address_type: MinerAddressType) -> Self {
        self.mining.miner_address = Some(
            default_miner_address(self.network.network.kind(), &miner_address_type)
                .parse()
                .expect("valid hard-coded address"),
        );

        self
    }
}
