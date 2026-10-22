//! End of support checking task.

use std::time::Duration;

use color_eyre::Report;

use zebra_chain::{block::Height, chain_tip::ChainTip, parameters::Network};

use crate::application::release_version;

/// The estimated height that this release will be published.
pub const ESTIMATED_RELEASE_HEIGHT: u32 = 93_545;

/// The number of blocks per day on the featurenet, with its 25-second block time.
pub const ESTIMATED_BLOCKS_PER_DAY: u32 = 24 * 60 * 60 / 25;

/// The maximum number of days after `ESTIMATED_RELEASE_HEIGHT` where a Zebra server will run
/// without halting.
///
/// Notes:
///
/// - Zebra will exit with a panic if the current tip height is bigger than the
///   `ESTIMATED_RELEASE_HEIGHT` plus this number of days.
/// - Currently set to 2 weeks.
pub const EOS_PANIC_AFTER: u32 = 14;

/// The number of days after `ESTIMATED_RELEASE_HEIGHT` where Zebra starts displaying warnings.
pub const EOS_WARN_AFTER: u32 = 7;

/// A string which is part of the panic that will be displayed if Zebra halts.
pub const EOS_PANIC_MESSAGE_HEADER: &str = "Zebra refuses to run";

/// A string which is part of the warning that will be displayed if Zebra release is close to halting.
pub const EOS_WARN_MESSAGE_HEADER: &str = "Your Zebra release is too old and it will stop running";

/// The amount of time between end of support checks.
const CHECK_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Wait a few seconds at startup so `best_tip_height` is always `Some`.
const INITIAL_WAIT: Duration = Duration::from_secs(10);

/// Start the end of support checking task. It is not enforced on Regtest.
pub async fn start(
    network: Network,
    latest_chain_tip: impl ChainTip + std::fmt::Debug,
) -> Result<(), Report> {
    info!("Starting end of support task");

    tokio::time::sleep(INITIAL_WAIT).await;

    loop {
        if !network.is_regtest() {
            if let Some(tip_height) = latest_chain_tip.best_tip_height() {
                check(tip_height, &network);
            }
        } else {
            info!("Release always valid in Regtest");
        }
        tokio::time::sleep(CHECK_INTERVAL).await;
    }
}

/// Returns the estimated last supported height for this release, or `None` on networks where
/// end of support is not enforced.
///
/// The node runs up to and including this height, and halts with an end of support panic when
/// the tip goes past it. This matches zcashd, where `end_of_service.block_height` is also the
/// threshold rather than the first halted block.
pub fn end_of_support_height(network: &Network) -> Option<Height> {
    if network.is_regtest() {
        return None;
    }

    Some(Height(
        ESTIMATED_RELEASE_HEIGHT + (EOS_PANIC_AFTER * ESTIMATED_BLOCKS_PER_DAY),
    ))
}

/// Check if the current release is too old and panic if so.
pub fn check(tip_height: Height, _network: &Network) {
    info!("Checking if Zebra release is inside support range ...");

    let panic_height =
        Height(ESTIMATED_RELEASE_HEIGHT + (EOS_PANIC_AFTER * ESTIMATED_BLOCKS_PER_DAY));
    let warn_height =
        Height(ESTIMATED_RELEASE_HEIGHT + (EOS_WARN_AFTER * ESTIMATED_BLOCKS_PER_DAY));

    if tip_height > panic_height {
        panic!(
            "{EOS_PANIC_MESSAGE_HEADER} if the release date is older than {EOS_PANIC_AFTER} days. \
            \nRelease name: {}, Estimated release height: {ESTIMATED_RELEASE_HEIGHT} \
            \nHint: Download and install the latest release from: https://github.com/ShieldedLabs/crosslink_monolith/releases/latest",
            release_version()
        );
    } else if tip_height > warn_height {
        warn!(
            "{EOS_WARN_MESSAGE_HEADER} at block {}. \
            \nRelease name: {}, Estimated release height: {ESTIMATED_RELEASE_HEIGHT} \
            \nHint: Download and install the latest release from: https://github.com/ShieldedLabs/crosslink_monolith/releases/latest", panic_height.0, release_version()
        );
    } else {
        info!("Zebra release is supported until block {}, please report bugs at https://github.com/ShieldedLabs/crosslink_monolith/issues", panic_height.0);
    }
}
