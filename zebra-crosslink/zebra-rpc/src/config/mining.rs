//! Mining config

use serde::{Deserialize, Serialize};
use serde_with::{serde_as, DisplayFromStr};

pub use zcash_address::ZcashAddress;

/// Mining configuration section.
#[serde_as]
#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    /// Address for receiving miner subsidy and tx fees.
    ///
    /// Used in coinbase tx constructed in `getblocktemplate` RPC.
    #[serde_as(as = "Option<DisplayFromStr>")]
    pub miner_address: Option<ZcashAddress>,

    // TODO: Internal miner config code was removed as part of https://github.com/ZcashFoundation/zebra/issues/8180
    // Find the removed code at https://github.com/ZcashFoundation/zebra/blob/v1.5.1/zebra-rpc/src/config/mining.rs#L18-L38
    // Restore the code when conditions are met. https://github.com/ZcashFoundation/zebra/issues/8183
    /// Extra data to include in coinbase transaction inputs.
    /// Limited to around 95 bytes by the consensus rules.
    ///
    /// If this string is hex-encoded, it will be hex-decoded into bytes.
    /// Otherwise, it will be UTF-8 encoded into bytes.
    pub extra_coinbase_data: Option<String>,

    /// Mine blocks using Zebra's internal miner, without an external mining pool or equihash solver.
    ///
    /// This experimental feature is only supported on regtest as it uses null solutions and skips checking
    /// for a valid Proof of Work.
    ///
    /// The internal miner is off by default.
    #[serde(default)]
    pub internal_miner: bool,

    /// The number of internal miner solver threads used by Zebra.
    ///
    /// These threads are scheduled at low priority. The configured count is clamped to the
    /// available parallelism reported by the OS, if that can be detected.
    ///
    /// Defaults to 1 thread, which is the internal miner's original (and, until sibling-solver
    /// cancellation landed, only supported) behaviour.
    #[serde(default = "default_internal_miner_threads")]
    pub internal_miner_threads: usize,
}

/// Returns the default value for [`Config::internal_miner_threads`].
fn default_internal_miner_threads() -> usize {
    1
}

// Written by hand, rather than derived, so `internal_miner_threads` defaults to 1 (matching
// `default_internal_miner_threads()`) instead of a derived `Default`'s 0.
impl Default for Config {
    fn default() -> Self {
        Config {
            miner_address: None,
            extra_coinbase_data: None,
            internal_miner: false,
            internal_miner_threads: default_internal_miner_threads(),
        }
    }
}

impl Config {
    /// Is the internal miner enabled using at least one thread?
    #[cfg(feature = "internal-miner")]
    pub fn is_internal_miner_enabled(&self) -> bool {
        // TODO: Changed to return always false so internal miner is never started. Part of https://github.com/ZcashFoundation/zebra/issues/8180
        // Find the removed code at https://github.com/ZcashFoundation/zebra/blob/v1.5.1/zebra-rpc/src/config/mining.rs#L83
        // Restore the code when conditions are met. https://github.com/ZcashFoundation/zebra/issues/8183
        self.internal_miner
    }
}
