//! Configuration for semantic verification which is run in parallel.

use std::str::FromStr;

use serde::{Deserialize, Serialize};

use zebra_chain::block;

use crate::BoxError;

/// A configured checkpoint that requires a block hash at a specific height.
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CheckpointConfig {
    /// The block height for this checkpoint.
    pub height: u32,

    /// The block hash for this checkpoint, in display order.
    pub hash: String,
}

impl CheckpointConfig {
    /// Convert this config entry into consensus checkpoint types.
    pub fn height_and_hash(&self) -> Result<(block::Height, block::Hash), BoxError> {
        Ok((block::Height(self.height), self.hash.parse()?))
    }
}

impl FromStr for CheckpointConfig {
    type Err = BoxError;

    fn from_str(checkpoint: &str) -> Result<Self, Self::Err> {
        let Some((height, hash)) = checkpoint.split_once(':') else {
            return Err(format!("invalid checkpoint '{checkpoint}': expected HEIGHT:HASH").into());
        };

        let checkpoint = Self {
            height: height.parse()?,
            hash: hash.to_string(),
        };
        checkpoint.height_and_hash()?;

        Ok(checkpoint)
    }
}

/// Configuration for parallel semantic verification:
/// <https://zebra.zfnd.org/dev/rfcs/0002-parallel-verification.html#definitions>
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(
    deny_unknown_fields,
    default,
    from = "InnerConfig",
    into = "InnerConfig"
)]
pub struct Config {
    /// Should Zebra make sure that it follows the consensus chain while syncing?
    /// This is a developer-only option.
    ///
    /// # Security
    ///
    /// Disabling this option leaves your node vulnerable to some kinds of chain-based attacks.
    /// Zebra regularly updates its checkpoints to ensure nodes are following the best chain.
    ///
    /// # Details
    ///
    /// This option is `true` by default, because it prevents some kinds of chain attacks.
    ///
    /// Disabling this option makes Zebra start full validation earlier.
    /// It is slower and less secure.
    ///
    /// Zebra requires some checkpoints to simplify validation of legacy network upgrades.
    /// Required checkpoints are always active, even when this option is `false`.
    ///
    /// # Deprecation
    ///
    /// For security reasons, this option might be deprecated or ignored in a future Zebra
    /// release.
    pub checkpoint_sync: bool,

    /// Additional checkpoints that Zebra must verify by block height and hash.
    ///
    /// These checkpoints are appended to the network's built-in checkpoint list at startup.
    /// They can be used to pin a node to a known fork.
    #[serde(default)]
    pub extra_checkpoints: Vec<CheckpointConfig>,
}

impl From<InnerConfig> for Config {
    fn from(
        InnerConfig {
            checkpoint_sync,
            extra_checkpoints,
            ..
        }: InnerConfig,
    ) -> Self {
        Self {
            checkpoint_sync,
            extra_checkpoints,
        }
    }
}

impl From<Config> for InnerConfig {
    fn from(
        Config {
            checkpoint_sync,
            extra_checkpoints,
        }: Config,
    ) -> Self {
        Self {
            checkpoint_sync,
            extra_checkpoints,
            _debug_skip_parameter_preload: false,
        }
    }
}

/// Inner consensus configuration for backwards compatibility with older `zebrad.toml` files,
/// which contain fields that have been removed.
///
/// Rust API callers should use [`Config`].
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct InnerConfig {
    /// See [`Config`] for more details.
    pub checkpoint_sync: bool,

    /// See [`Config`] for more details.
    #[serde(default)]
    pub extra_checkpoints: Vec<CheckpointConfig>,

    #[serde(skip_serializing, rename = "debug_skip_parameter_preload")]
    /// Unused config field for backwards compatibility.
    pub _debug_skip_parameter_preload: bool,
}

// we like our default configs to be explicit
#[allow(unknown_lints)]
#[allow(clippy::derivable_impls)]
impl Default for Config {
    fn default() -> Self {
        Self {
            checkpoint_sync: true,
            extra_checkpoints: Vec::new(),
        }
    }
}

impl Default for InnerConfig {
    fn default() -> Self {
        Self {
            checkpoint_sync: Config::default().checkpoint_sync,
            extra_checkpoints: Config::default().extra_checkpoints,
            _debug_skip_parameter_preload: false,
        }
    }
}
