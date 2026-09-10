//! libfuzzer target: fuzzed bytes -> the real block-admission path.
//!
//! Each input models one hostile-peer message. It is pushed through the genuine
//! `submit_block_to_new_network` doorway of a real, in-process `new_network::sync` loop (see
//! `zebrad::fuzz::Ingest`), so a block that parses but is consensus-invalid runs the actual
//! verify+commit pipeline rather than a reconstruction. A panic anywhere in that pipeline aborts
//! the process (panic = "abort"), which is the crash signal the fuzzer is hunting.
//!
//! Boot is expensive (spawns state + rocksdb + the sync loop), so we boot ONCE for the whole
//! campaign and reuse it: libfuzzer drives this target on a single thread, feeding many inputs.
#![no_main]

use libfuzzer_sys::fuzz_target;
use once_cell::sync::Lazy;
use zebrad::fuzz::Ingest;

static INGEST: Lazy<Ingest> = Lazy::new(Ingest::boot);

fuzz_target!(|data: &[u8]| {
    let _ = INGEST.submit_bytes(data);
});
