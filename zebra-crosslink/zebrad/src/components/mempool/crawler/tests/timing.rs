//! Timing tests for the mempool crawler.

use zebra_chain::parameters::POST_BLOSSOM_POW_TARGET_SPACING;
use zebra_network::constants::{DEFAULT_CRAWL_NEW_PEER_INTERVAL, HANDSHAKE_TIMEOUT};

use crate::components::mempool::crawler::RATE_LIMIT_DELAY;

#[test]
fn ensure_timing_consistent() {
    assert!(
        RATE_LIMIT_DELAY.as_secs() < POST_BLOSSOM_POW_TARGET_SPACING.into(),
        "a mempool crawl should complete before most new blocks"
    );

    // Upstream also requires the default peer crawler interval to be at least
    // `HANDSHAKE_TIMEOUT` lower than this crawler's interval (see
    // `DEFAULT_CRAWL_NEW_PEER_INTERVAL`). With Crosslink's 25 s blocks the mempool crawler
    // runs every 17 s, to stay under the block spacing (checked above), so it is meant to run
    // several times per 61 s peer crawl. What still holds is that a crawl interval leaves
    // room for a peer handshake.
    assert!(
        HANDSHAKE_TIMEOUT.as_secs() < RATE_LIMIT_DELAY.as_secs(),
        "a peer handshake should complete within one mempool crawl interval"
    );
    assert!(
        RATE_LIMIT_DELAY < DEFAULT_CRAWL_NEW_PEER_INTERVAL,
        "the mempool crawler runs more often than the peer address crawler"
    );
}
