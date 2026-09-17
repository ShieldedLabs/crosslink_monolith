# zebrad-fuzz

Coverage-guided fuzzing of the **real** block-admission path (plan phase P1).

Each input is one hostile-peer message, pushed through the genuine
`submit_block_to_new_network` doorway of a real in-process `new_network::sync` loop
(`zebrad::fuzz::Ingest`). A block that parses but is consensus-invalid runs the actual
verify+commit pipeline; a panic there aborts the process (`panic = "abort"`), which is the
crash the fuzzer hunts.

## Targets

- `ingest` — raw bytes -> `Ingest::submit_bytes` -> real verify+commit. Corpus seeded from
  `../../crosslink-test-data/*.bin` under `corpus/ingest/`.

## Run (Linux/macOS runner)

```
cargo install cargo-fuzz
cd zebra-crosslink/zebrad/fuzz
cargo +nightly fuzz run ingest -- -timeout=10 -rss_limit_mb=4096
```

`-timeout` is libfuzzer's outer backstop; `Ingest::submit_bytes` also self-times-out a wedged
commit at 2s.

## Platform note

libFuzzer / SanitizerCoverage support on `x86_64-pc-windows-msvc` is unreliable, and a cold
build of the full node graph (rocksdb, libzcash_script, secp256k1, tromp-equihash) under
sanitizer instrumentation is memory-heavy. **Build and run this on the Linux CI runner**, not
the Windows dev box.

On Windows, the equivalent crash-oracle runs with no nightly and no libfuzzer as an ordinary
test that replays the whole corpus (plus a deterministic byte-flip sweep) through the same
`Ingest::submit_bytes`:

```
cargo test -p zebrad --lib fuzz::tests::corpus_replay_never_panics -- --nocapture --test-threads 1
```
