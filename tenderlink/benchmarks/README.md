# D28 mock-chain performance runner

`d28_perf.py` runs the Tenderlink-only Rust mock BFT chain sequentially in fresh
Linux rootless network namespaces. It uses Python's standard library and existing
`unshare`, `ip`, and `tc`; it never installs tools, uses sudo, changes host network
settings, or falls back to an unisolated run. This is a shared-host kernel-netem
loopback experiment, **not a WAN prediction** or a dedicated-machine benchmark.

From the repository root:

```sh
cargo build --manifest-path tenderlink/Cargo.toml --release --features bench --bin d28_bench
python3 tenderlink/benchmarks/d28_perf.py selftest
# Optional real netns checks using synthetic IPv6 UDP, not the Rust benchmark:
python3 tenderlink/benchmarks/d28_perf.py selftest --netns

# Small smoke run; every --out must name a NEW directory.
python3 tenderlink/benchmarks/d28_perf.py run --out tenderlink/benchmarks/smoke-001 \
  --case clean --nodes 4 --heights 10 --warmup 2 --headers 32 --seed 100

# Five cases, sequential, each repeated three times; no automatic N/body sweep.
python3 tenderlink/benchmarks/d28_perf.py matrix --out tenderlink/benchmarks/matrix-001 \
  --nodes 4 --heights 12 --warmup 2 --headers 512 --repetitions 3 --seed 100 \
  --chatter-secs 1 --host-note 'Shared host; unrelated work was not excluded.'

python3 tenderlink/benchmarks/d28_perf.py summary tenderlink/benchmarks/matrix-001
```

`run` accepts `--delay-ms`, `--jitter-ms`, `--loss-pct`, and **`--rate-mbit`**
(aggregate budget), plus `--offline`, `--nodes`, `--headers`, `--workers`,
`--host-delay-ms`, and `--store-delay-ms`. `--headers` scales real serialized mock
Zcash headers, not assumed 80-byte Bitcoin headers; Rust reports the actual byte
counts. `--max-body` is an explicit optional cap override. A live >2/3 quorum is
required. See `run --help` for bounds. `--binary` overrides the default release
binary above; the runner does not build it.

Defaults are 4 validators, 30 measured heights + 3 warmup heights, 32 headers,
4 workers, a 180-second Rust deadline, MTU 1500, and a separate 1-second final-height
hold (`--chatter-secs`; Rust alone defaults to zero). Production protocol timers
are unchanged. The outer watchdog defaults to `timeout-secs + chatter-secs + 20`;
`--hard-timeout-secs` can override it. It terminates only this trial's new process
group, preserves available evidence, and returns failure. No broad process kill
or host qdisc cleanup is used: the namespace disappears with its processes.

## Network model

Every trial starts with `unshare --user --map-root-user --net --`. Before any
`ip`/`tc` change, the worker compares `/proc/self/ns/net` link and inode against
the supplied parent identity and refuses equality. It sets only private `lo`
MTU (`--mtu`, 1280..65535; default 1500), brings it up, installs a `clsact` egress
`matchall action gact pass` counter, then installs a root `netem` qdisc. A failed
step fails the trial; there is no compatibility fallback that omits impairment.
No mount namespace is used. Network counters come from `/proc/net/dev`, **never**
`/sys/class/net`, whose mount would describe the host.

The matrix cases are clean, 25 ms one-way delay, 25 +/- 5 ms uniform jitter with
1% independent loss, aggregate 10 Mbit/s, and `floor((N-1)/3)` offline validators.
The rate applies to the **shared loopback interface across all validators**, not
to each validator's uplink. Delay/loss apply per IP packet or fragment, not per
application message; nominal RTT is twice the one-way delay before queuing or
retransmission. The queue defaults to 10,000 packets
(`--queue-packets`). A repetition uses `seed + repetition - 1` for every case;
a seeded loss process does not make asynchronous packet ordering deterministic.

## Reading the measurements

- Decision latency is measured by Rust before its host ACK barrier. `first` is
  the first certificate-backed local decision; `quorum` means >2/3 validators
  have locally decided, not when a valid certificate first exists; `all` means
  every online validator. Chain throughput counts a body **once**, not once per
  validator, and is paced by the all-online ACK barrier. Body B/s and synthetic
  header-payload B/s are separate and are not network goodput.
- Small samples do not establish stable tail latency: nearest-rank p99 is the
  maximum with 30 heights, and p95 is the maximum with at most 19. Failed/partial
  trials remain visible; absent measurements print `-`, not zero. Each latency
  cohort has its own sample count: a partial final height can contribute first
  or quorum decisions without contributing to `all` or completed-chain count.
- `report.json` measured `network` counters exclude warmup. Pre-impairment `tc`
  bytes include **14 synthetic loopback link-header bytes per packet**; lo TX
  is post-netem L3 and excludes netem-dropped packets. A neutral UDP100/IPv6
  probe verifies 162 pre-netem bytes versus 148 lo TX bytes when clean, but
  162 pre-netem bytes versus **zero** lo TX bytes at 100% loss. Neither counter
  is application-offered bytes or retransmission-only overhead.
- `chatter_network` is a separate final-height hold: the host withholds final
  ACK/tip publication. It measures continued final-height traffic, **not idle
  traffic of an already quiescent chain**. The summary prints totals, not rates
  divided by the requested hold duration; use actual recorded sample times for
  rates. Counter samples are serial, not atomic; their sampling duration/skew
  matters near boundaries.
- `worker.json` counters cover the **whole Rust process**, including startup,
  warmup, measurement, the hold, and shutdown. Do not mix them with measured
  chain rates. Its explicitly labeled L3 estimate subtracts `14 * packets` from
  pre-netem accounted bytes. Raw qdisc/filter counters are retained. No additional
  queue-drain wait is performed; post-run backlog/qlen may show packets still
  queued. Network/unique-chain byte ratios describe amplification, including
  necessary replication, not purely wasted chatter; subtracting application
  bytes cannot isolate retransmissions, FEC, framing, ACKs, or boundary effects.

Each output has immutable `manifest.json` (CPU, affinity, load, kernel, git,
binary SHA-256, expected build recipe, cases and settings), `run.json`, and one
case/repetition directory. Each trial retains exact argv, namespace identities,
Rust stdout/stderr, **unchanged** Rust `report.json`, setup output, timed raw
`tc -j -s` qdisc/filter snapshots, `/proc/net/dev` deltas, `worker.json`, and the
watchdog/classification `result.json`. The build recipe is provenance guidance,
not proof of the command that produced an externally supplied binary. Trials are
sequential within one invocation; unrelated CPU/disk activity is not excluded.

## Recorded baseline — 2026-11-26

Measured release commit `32e3e94f1efd4921c195236f7cdbd7037c7ca9f8`, built with
Rust 1.94.0. Binary SHA-256:
`ecdb54ee18ac8c0313614565bd447d4644c1b68cc2947fbbfade38be544b655d`.
The shared host was an AMD Ryzen 5 5500U, 12 logical CPUs. All validators shared
one process with **4 Tokio workers**; each STP endpoint also owns its transport
threads. Production timers, receive budget 32, and the 1 MiB body bound were
unchanged. Storage was the in-memory atomic-image backend; these are not fsync
or distributed-machine capacity measurements.

Each row combines three sequential trials, each with 2 warmup and 12 measured
heights, plus a separate 1-second final-height hold. All **21 repeated trials
completed**. Percentiles below pool 36 raw height observations using nearest rank;
rates use total bytes divided by total measured duration, not averaged medians.

Four validators, 512 synthetic headers, **96,103 serialized bytes/body**:

| Network condition | First decision p50 / p95 (ms) | All-online p50 (ms) | Committed body KiB/s | Network / chain bytes |
|---|---:|---:|---:|---:|
| Clean | 470.48 / 521.91 | 501.02 | 186.81 | 25.32× |
| 25 ms one-way delay | 659.10 / 857.50 | 700.20 | 130.24 | 27.75× |
| 25 ± 5 ms, 1% loss | 689.29 / 961.37 | 726.22 | 110.64 | 26.61× |
| Shared aggregate 10 Mbit/s | 2058.03 / 5622.86 | 2257.77 | 31.25 | 33.75× |
| One validator offline | 523.95 / 4008.00 | 552.81 | 67.02 | 21.02× |

Network/chain uses pre-netem accounted bytes, including the synthetic 14-byte
loopback link header. It includes necessary replication, complete evidence,
framing, redundancy and probes: **it is not a pure wasted-chatter ratio**.
Complete evidence alone was 12.42–16.89 times unique chain-body bytes and
95.2–99.1% of application-offered traffic. Application control traffic was
22.50–28.26 KiB/s with four online validators, and 12.78 KiB/s with one offline.
One lossy height took 4.33 seconds to its first decision; with only 36 samples,
p99 is the maximum, not a stable tail estimate.

Clean-link scaling with 32 headers (same 5,984-byte synthetic payload):

| Online validators | Mean body bytes | First decision p50 / p95 (ms) | All-online p50 / p95 (ms) | Body KiB/s | Network / chain bytes | Receive-budget discards |
|---|---:|---:|---:|---:|---:|---:|
| 4 | 6,343 | 498.24 / 548.34 | 532.89 / 616.96 | 11.54 | 80.84× | 0 |
| 16 | 7,111 | 1354.74 / 6029.82 | 1976.44 / 6551.54 | 2.93 | 2193.52× | 19,699 |

The 16-validator run exposes local receive-budget losses on a lossless simulated
link, despite zero recorded wrapper inbox/outbox/ring drops. It is a **contended
single-host, four-worker result**, not a prediction for sixteen separate hosts.
Larger bodies amortize substantial per-height traffic; the defaults are not a
performance-tuned configuration. The separate final-height hold is not idle:
for the four-validator large-body clean case it still emitted 803.9 KiB/s,
including unfinished transfer traffic, control and transport overhead.

A separate 100%-loss trial correctly returned `partial` with no decisions,
retained its stalled duration of at least 3 seconds, and recorded positive
whole-process pre-loss traffic despite zero post-loss loopback TX. Missing
end-of-deadline `tc` samples remained unavailable, not fabricated zero costs.

Raw local evidence (each includes manifests, original reports and counters):

- `/var/tmp/d28-component-matrix-20261126-1538`
- `/var/tmp/d28-component-small-20261126-1542`
- `/var/tmp/d28-component-scale16-20261126-1542`
- `/var/tmp/d28-component-allloss-20261126-1546`

The corresponding code passed 275 release tests (five existing ignored tests;
unbounded legacy `single_rt` excluded), 14 runner selftests including real netns
checks, and the optimized CLI build. No PoW/ledger formats or rules were changed.
