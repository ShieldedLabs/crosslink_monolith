//! In-process fuzzing harness for the real block-admission path.
//!
//! Malicious-peer model: hand the node fuzzed bytes and watch what it actually does -- accept,
//! reject, panic, or diverge. A block that *parses* but is consensus-invalid (bad merkle root,
//! wrong subsidy, bad difficulty) is the interesting case, exactly what a hostile peer sends,
//! so the fuzzer must not stop at deserialization.
//!
//! To stay honest we run the *real* pipeline, not a reconstruction: [`Ingest::boot`] stands up
//! the actual `new_network::sync` loop -- the same loop production runs -- against real
//! ephemeral state, and fuzzed bytes go through the genuine
//! [`zebra_state::new_network::submit_block_to_new_network`] doorway, exercising the real
//! header -> body -> crosslink-gate -> expensive -> commit sequence. Networking is left idle
//! (`Config::ephemeral` already binds an ephemeral port with no peers), and we reimplement none
//! of the loop, so the fuzzer cannot drift from the real path and can observe a wrong
//! *acceptance*, not merely a panic.
//!
//! Boot once, then feed many inputs; a panic anywhere in the real pipeline aborts the process,
//! which is the fuzzer's crash signal.
//!
//! # Not yet pinned (reproducibility)
//!
//! Two nondeterminism sources remain on this path and must be controlled before a crash
//! reproduces from the input alone: the wall clock read into `block_check_header`'s time check
//! (`zebra_debug_time::now()`), and `thread_rng()` in Sapling/Orchard batch verification. Both
//! are follow-on work.

use std::sync::Arc;
use std::time::{Duration, Instant};

use zebra_chain::{
    block::{genesis::regtest_genesis_block, Block, Height},
    chain_tip::ChainTip,
    parameters::Network,
    serialization::ZcashDeserialize,
};
use zebra_crosslink::uhh;
use zebra_state::new_network::{submit_block_to_new_network, IngestOutcome, VerifyFns};

/// A booted, in-process instance of the real block-ingest path.
///
/// Holds the Tokio runtime, so the `new_network::sync` loop keeps running for the life of the
/// fuzzer, and the latest-tip handle used to confirm genesis committed. Drop to tear down.
pub struct Ingest {
    // Option so Drop can take the runtime and shut it down in the background. The
    // new_network::sync loop is an infinite blocking task, so a normal Runtime drop blocks
    // forever waiting for it -- which would silently wedge a fuzz campaign on teardown.
    rt: Option<tokio::runtime::Runtime>,
    latest_chain_tip: zebra_state::LatestChainTip,
}

impl Drop for Ingest {
    fn drop(&mut self) {
        if let Some(rt) = self.rt.take() {
            // Deliberately leak the runtime. Dropping it would block forever waiting for the
            // infinite sync loop; shutting it down "in the background" would instead race that
            // still-running loop against a tearing-down runtime (an error/panic at teardown).
            // Leaking lets everything keep running untouched until the process exits -- the only
            // lifetime Ingest is meant to have. Nothing in flight is lost, because every
            // submit_bytes fully commits-or-rejects and returns before we get here.
            std::mem::forget(rt);
        }
    }
}

impl Ingest {
    /// Boot the real ingest once. Returns after genesis has committed through the real path
    /// (which is also after the submission channel is live). Regtest, so PoW is disabled on
    /// the admission path, matching `check_pow = !network.disable_pow()`.
    pub fn boot() -> Ingest {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");

        let latest_chain_tip = rt.block_on(async {
            let network = Network::new_regtest(Default::default());

            // `ephemeral()` already sets network_local_port = 0 and network_initial_peers = [],
            // so the sync loop's STP thread binds an ephemeral port and never connects.
            let config = zebra_state::config::Config::ephemeral();

            // Trivial always-pass fat-pointer gate, matching init_test. The gate only governs
            // PoW<->PoS linkage, which the block-bytes fuzzer is not exercising.
            let gate: zebra_state::ClosureToCallIntoCrosslinkFromState =
                Arc::new(|_, _, _| Some(true));

            // spawn_init is the public constructor (the zebra_state::service module is private).
            // It returns the block_writer that sync() must own -- which the init_test/
            // init_test_services helpers discard, so they can't be used here.
            let (_state, read_state, latest_chain_tip, _tip_change, block_writer) =
                zebra_state::spawn_init(config.clone(), &network, Height::MAX, 0, gate.clone())
                    .await
                    .expect("state init task");

            let genesis = regtest_genesis_block();
            assert_eq!(genesis.hash(), network.genesis_hash(), "regtest genesis mismatch");

            // The real verify entry points, exactly as start.rs wires them (plain fn pointers,
            // because zebra-state cannot depend on zebra-consensus).
            let verify_fns = VerifyFns {
                check_header: zebra_consensus::sync_verify::block_check_header,
                check_body: zebra_consensus::sync_verify::block_check_body,
                check_cheap: zebra_consensus::sync_verify::block_check_cheap,
                verify_expensive: zebra_consensus::sync_verify::block_verify_expensive,
            };

            // Run the real sync loop on the blocking pool with a runtime handle, mirroring
            // start.rs. This SETs BLOCK_SUBMISSION_SENDER and commits genesis; the task is
            // fire-and-forget and lives as long as the runtime does.
            let handle = tokio::runtime::Handle::current();
            tokio::task::spawn_blocking(move || {
                zebra_state::new_network::sync(
                    &config,
                    read_state,
                    handle,
                    verify_fns,
                    gate,
                    block_writer,
                    genesis,
                );
            });

            // `_state` (the tower front-end) is unused for pure admission -- the writer owns the
            // state and read_state reads it -- so let it drop.
            latest_chain_tip
        });

        let ingest = Ingest { rt: Some(rt), latest_chain_tip };
        ingest.wait_for_genesis();
        ingest
    }

    /// Block until genesis is committed (best tip height becomes 0). Because the submission
    /// sender is set just before genesis is committed, this also guarantees `submit_bytes`
    /// will reach the loop rather than the "not running" path.
    fn wait_for_genesis(&self) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while self.latest_chain_tip.best_tip_height().is_none() {
            assert!(Instant::now() < deadline, "ingest did not commit genesis in time");
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// The current best-chain tip height (0 right after boot). Mostly for tests/assertions.
    pub fn best_tip_height(&self) -> Option<Height> {
        self.latest_chain_tip.best_tip_height()
    }

    /// Feed one fuzzed input through the real admission path.
    ///
    /// Unparseable bytes are dropped (`None`) -- a peer that cannot even frame a block is the
    /// boring case, handled below consensus. A parsed block runs the *full* real verify+commit
    /// pipeline and its [`IngestOutcome`] (Committed / Known / Failed) is returned. A panic
    /// inside that pipeline is not caught: it aborts, which is the fuzzer's crash signal.
    pub fn submit_bytes(&self, data: &[u8]) -> Option<IngestOutcome> {
        let block = uhh(Block::zcash_deserialize(data), uhh::LOG).ok()?;
        // Tight timeout: a healthy regtest commit is milliseconds, so a multi-second wait means
        // the pipeline wedged on this input -- a finding, not something to sit on. (libfuzzer's
        // own -timeout is the outer backstop.)
        let outcome = self
            .rt
            .as_ref()
            .expect("ingest runtime present")
            .block_on(async { submit_block_to_new_network(Arc::new(block), Duration::from_secs(2)).await });
        // An `Err` here is the transport-level failure (e.g. the loop stopped); the consensus
        // verdict lives inside `IngestOutcome`.
        uhh(outcome, uhh::LOG).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Proves the harness boots the REAL ingest (genesis committed through the real path) and
    // that feeding bytes through the real doorway neither hangs nor panics. Adversarial bytes
    // are dropped or rejected; a checked-in serialized block, if it still parses on this dev,
    // exercises the full verify+commit and returns an outcome either way -- never a panic.
    #[test]
    fn real_ingest_boots_and_survives_hostile_bytes() {
        let ingest = Ingest::boot();
        assert_eq!(ingest.best_tip_height(), Some(Height(0)), "genesis must commit at boot");

        assert!(ingest.submit_bytes(&[0xff; 64]).is_none(), "garbage must not parse");
        assert!(ingest.submit_bytes(&[0x00; 2048]).is_none(), "garbage must not parse");

        // If this checked-in block still deserializes on current dev it runs the whole
        // pipeline; if not, it is dropped. The invariant under test is: no panic.
        let _ = ingest.submit_bytes(include_bytes!("../../crosslink-test-data/test_pow_block_0.bin"));
    }
}
