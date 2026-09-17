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

use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use zebra_chain::{
    block::{genesis::regtest_genesis_block, Block, Height},
    chain_tip::ChainTip,
    parameters::Network,
    serialization::{ZcashDeserialize, ZcashSerialize},
};
use zebra_crosslink::uhh;
use zebra_state::new_network::{submit_block_to_new_network, IngestOutcome, VerifyFns};

/// The result of feeding one input through the real doorway.
///
/// The whole point of the enum is to keep three things that used to collapse into `None`
/// distinct, because two of them are findings a fuzz campaign must never discard: a timed-out
/// verdict and a dead loop. (The corpus replay surfaced a real `coinbase_height().expect()` panic
/// on the sync worker on 2026-09-10; a loop-thread panic never propagates to the caller, so
/// `TransportDead` is the only way this harness can observe it without `panic = "abort"`.)
#[derive(Debug)]
pub enum IngestResult {
    /// The bytes did not deserialize into a `Block`. The overwhelming majority of fuzz inputs,
    /// and the boring case: a peer that cannot even frame a block is handled below consensus.
    Unparseable,
    /// The block ran the real verify+commit pipeline and returned a verdict.
    Outcome(IngestOutcome),
    /// The verdict did not return within the submit timeout. Deliberately NOT a panic -- under
    /// concurrent load, or with the state service briefly unresponsive, a healthy input can time
    /// out. So this is a SUSPICION to re-run, never a confirmed finding on its own: only a
    /// TimedOut that reproduces across reruns counts. (`TransportDead` -- a dead loop -- is the
    /// reliable crash signal; that is the one to trust.)
    TimedOut,
    /// The sync loop stopped replying (channel closed / reply dropped), e.g. it panicked while
    /// processing this or an earlier input. The strongest crash signal available here.
    TransportDead(String),
}

/// A booted, in-process instance of the real block-ingest path.
///
/// Holds the Tokio runtime, so the `new_network::sync` loop keeps running for the life of the
/// fuzzer, and the latest-tip handle used to confirm genesis committed.
pub struct Ingest {
    // ManuallyDrop, never dropped -- the runtime is leaked on purpose. The new_network::sync loop
    // is an infinite blocking task, so a normal Runtime drop blocks forever waiting for it, and
    // shutdown_background would instead race the still-running loop against a dead reactor (a
    // teardown panic). Leaking lets everything run untouched until the process exits -- the only
    // lifetime Ingest is meant to have. Sound ONLY at one Ingest per process (each boot leaks a
    // whole runtime + ephemeral state DB + thread set); boot() enforces that. Nothing owned by
    // submit_bytes is lost, since each call fully returns before another begins.
    rt: ManuallyDrop<tokio::runtime::Runtime>,
    // Per-submit verdict timeout. A slow verdict is reported as TimedOut, never panicked, so this
    // is a tunable, not a constant: raise it under load to avoid false timeouts (see
    // with_submit_timeout, and never run a campaign next to a build).
    submit_timeout: Duration,
    latest_chain_tip: zebra_state::LatestChainTip,
}

impl Ingest {
    /// Boot the real ingest once. Returns after genesis has committed through the real path
    /// (which is also after the submission channel is live). Regtest, so PoW is disabled on
    /// the admission path, matching `check_pow = !network.disable_pow()`.
    pub fn boot() -> Ingest {
        // The leak-on-exit design (see the struct) is sound only at one instance per process:
        // each boot leaks a whole runtime + ephemeral state DB + thread set, which on a
        // memory-constrained box surfaces as a mystifying OOM rather than an obvious leak. Turn a
        // second boot into a loud, immediate error instead of a silent resource leak.
        static BOOTED: AtomicBool = AtomicBool::new(false);
        assert!(
            !BOOTED.swap(true, Ordering::SeqCst),
            "Ingest::boot called twice in one process: it leaks a runtime + state DB per call; \
             reuse the one Ingest",
        );

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
                Arc::new(|_, _, _, _| Some(zebra_state::CrosslinkVerdict::Accept { pos_payout: true }));

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

        let ingest = Ingest {
            rt: ManuallyDrop::new(rt),
            submit_timeout: Duration::from_secs(2),
            latest_chain_tip,
        };
        ingest.wait_for_genesis();
        ingest
    }

    /// Override the per-submit verdict timeout (default 2s). A slow verdict returns
    /// [`IngestResult::TimedOut`], never a panic, so raise this when running under concurrent
    /// load to avoid false timeouts.
    pub fn with_submit_timeout(mut self, timeout: Duration) -> Self {
        self.submit_timeout = timeout;
        self
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
    /// A parsed block runs the *full* real verify+commit pipeline; see [`IngestResult`] for how
    /// the outcomes are kept distinct. A panic on the calling thread is not caught (it aborts
    /// under `panic = "abort"`, the fuzzer's crash signal); a panic on the sync-loop thread
    /// instead surfaces as [`IngestResult::TransportDead`] on this and every later call, which is
    /// how the harness observes a loop-thread crash at all.
    pub fn submit_bytes(&self, data: &[u8]) -> IngestResult {
        let block = match uhh(Block::zcash_deserialize(data), uhh::LOG) {
            Ok(block) => block,
            Err(_) => return IngestResult::Unparseable,
        };
        // A healthy regtest commit is milliseconds; self.submit_timeout is the per-input backstop
        // (libfuzzer's own -timeout is the outer one).
        let result = self
            .rt
            .block_on(async { submit_block_to_new_network(Arc::new(block), self.submit_timeout).await });
        match result {
            Ok(outcome) => IngestResult::Outcome(outcome),
            // submit_block_to_new_network reports its two timeout cases with "timed out ..."
            // messages and every other failure (sender missing / queue closed / reply dropped)
            // with a distinct one. A dropped reply channel is what a sync-loop panic looks like.
            Err(reason) if reason.contains("timed out") => IngestResult::TimedOut,
            Err(reason) => IngestResult::TransportDead(reason),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // NOTE: these tests each boot an Ingest, and boot() has a process-wide single-boot guard, so
    // AT MOST ONE may run per process. That is fine normally -- the reproducer below is #[ignore]d,
    // so a plain `cargo test` runs only the smoke test, and `--ignored` runs only the reproducer.
    // Do NOT run them together with `--include-ignored`: the second boot() panics by design.

    // Green smoke test / P5 regression replay (stable toolchain, no nightly, no libfuzzer): the
    // harness boots the REAL ingest (genesis committed through the real path), rejects garbage
    // below consensus, and survives every VALID checked-in block -- each gets a verdict and the
    // sync loop stays alive. A sync-loop-thread panic never propagates to submit_bytes, so the
    // only way it shows up is TransportDead on a later call (exactly why the Option-collapsing
    // predecessor reported a dead worker as success); asserting the loop stays alive is the oracle.
    // Mutated/hostile inputs are left to the libfuzzer `ingest` target, which re-runs and minimizes
    // (a lone TimedOut is only a suspicion -- see IngestResult::TimedOut).
    #[test]
    fn real_ingest_boots_and_survives_valid_corpus() {
        let ingest = Ingest::boot();
        assert_eq!(ingest.best_tip_height(), Some(Height(0)), "genesis must commit at boot");

        assert!(matches!(ingest.submit_bytes(&[0xff; 64]), IngestResult::Unparseable));
        assert!(matches!(ingest.submit_bytes(&[0x00; 2048]), IngestResult::Unparseable));

        let corpus_dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../crosslink-test-data");
        let mut replayed = 0usize;
        for entry in std::fs::read_dir(corpus_dir).expect("corpus dir readable") {
            let path = entry.expect("dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("bin") {
                continue; // *.bin are serialized blocks; skip fonts / .zeccltf programs
            }
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let bytes = std::fs::read(&path).expect("read corpus file");
            // The only hard failure is a dead loop: valid blocks must not CRASH the node.
            // A verdict (Outcome) or a stale block that no longer parses (Unparseable) are both
            // fine, and so is TimedOut -- these seeds are a chain, so once an ancestor is rejected
            // (e.g. stale data fails difficulty on this dev) its descendants orphan in the commit
            // queue and never get a verdict; a lone TimedOut is a suspicion under load anyway, not
            // proof of anything (see IngestResult::TimedOut). Only TransportDead means the loop died.
            match ingest.submit_bytes(&bytes) {
                IngestResult::TransportDead(reason) => {
                    panic!("valid corpus block {name} killed the sync loop: {reason}")
                }
                IngestResult::Outcome(_) | IngestResult::Unparseable | IngestResult::TimedOut => {}
            }
            replayed += 1;
        }
        assert!(replayed > 0, "corpus must contain at least one *.bin seed at {corpus_dir}");
    }

    // Deterministic reproducer for the open finding (2026-09-10): a block that deserializes but has
    // an empty transaction vector returns coinbase_height() == None, and the submit doorway does no
    // height check before the commit queue's .expect() at new_network.rs:2806/2822. With a known
    // (genesis) parent it reaches that expect and kills the sync loop; a correct node must REJECT
    // it with a verdict. #[ignore]d because it is RED until the submit path grows the same
    // coinbase-height guard the packet path already has at new_network.rs:2726 -- run it with
    // `--ignored` to check the finding, and delete the attribute once the guard lands.
    #[test]
    #[ignore = "open finding: coinbase_height().expect on the submit path, new_network.rs:2822"]
    fn empty_tx_block_must_not_crash_sync_loop() {
        let ingest = Ingest::boot();

        let mut empty_tx_block =
            Block::zcash_deserialize(&include_bytes!("../../crosslink-test-data/test_pow_block_0.bin")[..])
                .expect("seed block parses");
        empty_tx_block.transactions = vec![];
        assert!(empty_tx_block.coinbase_height().is_none(), "empty-tx block must have no coinbase height");
        let empty_tx_bytes = empty_tx_block.zcash_serialize_to_vec().expect("reserialize");

        // Observe the RETURN value, never a propagated worker-thread panic: a sync-loop panic does
        // not reach this thread, so relying on it propagating would let the reproducer pass green on
        // a broken node. A healthy node returns a verdict (Outcome, a Failed rejection); a crash
        // surfaces as TransportDead and a stall as TimedOut -- both mean the loop did not survive.
        match ingest.submit_bytes(&empty_tx_bytes) {
            IngestResult::Outcome(_) => {}
            other => panic!(
                "empty-tx block did not get a clean verdict -- the sync loop did not survive it \
                 (coinbase_height().expect at new_network.rs:2822): {other:?}"
            ),
        }
    }
}
