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

## Distributed component runner on Fly

`d28_bench --distributed CONFIG.json --json REPORT.json` runs **one actual D28
validator per process**. `d28_fly.py` coordinates separate machines through a
private IPv6 network. It never provisions machines or changes host networking.
Use temporary machines, test identities only, no public service/IP, and collect
artifacts before deleting the temporary app. All output directories must be new.

Build and test from the repository root:

```sh
cargo build --locked --release --features bench \
  --manifest-path tenderlink/Cargo.toml --bin d28_bench
python3 tenderlink/benchmarks/d28_fly.py selftest \
  --binary tenderlink/target/release/d28_bench
```

Install the same binary and `d28_fly.py` at `/app/` on every machine, plus Python
3. Generate one private token with `d28_fly.py token --out TOKEN` and install it
as `/run/d28-token` with mode 0600 on each machine. No production keys are used.
Start a worker on each machine except node 0:

```sh
python3 /app/d28_fly.py worker --bind "[$PRIVATE_IPV6]:9200" \
  --binary /app/d28_bench --token-file /run/d28-token \
  --root /data/worker-001 --lifetime-secs 7200
```

The roster JSON contains `peers` (ordered full-roster `[IPv6]:9000` addresses),
`workers` (`null` for node 0, then `[IPv6]:9200` for each other node),
`coordinator` (`[node-0-IPv6]:9100`) and `interface` (`eth0`). Only literal private
ULA IPv6 or loopback addresses are accepted. Run the campaign **on node 0**, not
on the laptop, to avoid putting the laptop's network hop in the ACK barrier:

```sh
python3 /app/d28_fly.py campaign --roster /data/roster.json \
  --binary /app/d28_bench --token-file /run/d28-token --out /data/sweep-001 \
  --headers 32,128,512,2048 --profiles baseline,fast \
  --repetitions 3 --heights 20 --warmup 3 --seed 428 --chatter-secs 1
```

The baseline uses runtime poll/status intervals of 25/100 ms and a receive budget
of 32; `fast` uses 5/25 ms and 128. Both allow two concurrent source requests.
`fast_k1` keeps the fast settings but permits only one source request; it is not
an end-to-end RX/memory cap, and an unhelpful source can occupy that sole lease
until the unchanged two-second expiry. All profiles keep outgoing handoffs at
two. Consensus timers, quorum, signatures and the 1 MiB body cap are unchanged.
These are benchmark overrides, **not changed node defaults**. Sizes and paired
profiles are randomized within each repetition. `--offline 1` keeps four
identities in the roster but launches only three nodes.

Reprocess a downloaded campaign without changing its original `summary.json`:

```sh
python3 tenderlink/benchmarks/d28_fly.py summary /path/to/campaign
```

The Python `summarize(path, quiet=True)` API returns a JSON-ready analysis with
`aggregation_version: 2`. Each metric has matched-trial coverage, missing-counter
reasons and its own matched denominator; unavailable values are null, not zero.
Measured and final-height-hold windows stay separate. Summaries show CPU seconds,
guest steal and process-lifetime peak-RSS gauges alongside application and VM TX
counters. Reprocessing does not change which runner version performed a trial.

The unchanged production block parser caps header count at 2,048. For a separate
near-1-MiB payload arm, use `--headers 2048 --solution-bytes 320`. This fills an
existing synthetic header field, not a new format or a relaxed parser. These
headers do not represent valid PoW work. Reports record actual serialized sizes,
including the previous certificate; the full-roster parent must fit the cap.

The controller carries **only readiness, decision metadata and ACKs**. Every node
obtains and validates its own block and certificate through D28/STP. Both trusted
tip publication and callback completion wait for matching local decisions from
all online validators, preserving old-height evidence. This remains an explicit
all-online-barrier component model, not an independently advancing full ledger.

Read the separate measurements correctly:

- Node-local latency runs from START/prior ACK tip publication to local decision
  callback entry. It includes scheduling and INIT, but is not an exact INIT timer.
- Controller first/quorum/all receipt latencies and chain cycles use only its own
  monotonic clock; they include reporting/control-network overhead. Never subtract
  wall clocks across machines or order differently started local durations.
- Throughput counts one chain's bytes once. Failed/partial trials retain their
  stalled durations. Measured node counters exclude warmup; worker process-window
  counters do not. Interface TX includes controller/management traffic as well as
  BFT/STP; it is not retransmission-only or pure wasted-chatter bandwidth.
- Each node retains application/transport counters, CPU usage/steal, available
  cgroup counters and RSS. Cheap shared CPU burst balances and host contention
  are uncontrolled; guest steal does not distinguish those causes.

Raw reports, exact configurations, executable/script hashes, worker metadata and
controller traces are retained per trial. Workers run one fixed executable with
bounded input and owned-child lifetimes; uncertain cleanup stops the campaign.
Stop and delete only the temporary app you created after retrieving its results.

### Recorded Fly results — 2026-11-26

Four separate `shared-cpu-1x`, 512-MiB machines, one validator/executor worker per
machine, over private Fly IPv6. The same-region campaign used four IAD machines.
The mock host used an in-memory atomic store, not RocksDB/fsync. Production
consensus timers, signatures, block parser and 1-MiB body admission cap were
unchanged. `baseline` and `fast` are the host-service profiles above: fast changes
polling, status cadence **and** receive budget together, not one isolated knob.

Each row below pools three trials and 60 measured heights, excluding three warmup
heights per trial. Throughput counts a single chain, including slow decisions.
Latencies are first/all decision **receipts at the controller**, p50/p95, in ms;
node-local callback latency is retained separately. All-online ACK pacing remains
part of this experiment. These are short shared-CPU observations, not sustained
capacity guarantees or distributed-machine synchronized decision timestamps.

| Serialized body | Profile | Chain KiB/s | First receipt p50 / p95 ms | All receipt p50 / p95 ms | VM TX / body | Offered evidence / body |
|---:|---|---:|---:|---:|---:|---:|
| 6.19 KiB | baseline | 13.2 | 448 / 475 | 468 / 497 | 92.64× | 20.64× |
| 6.19 KiB | fast | 37.9 | 154 / 162 | 163 / 171 | 85.96× | 21.43× |
| 23.73 KiB | baseline | 50.4 | 452 / 479 | 476 / 504 | 41.95× | 19.12× |
| 23.73 KiB | fast | 140.6 | 158 / 170 | 167 / 184 | 43.69× | 20.41× |
| 93.85 KiB | baseline | 195.1 | 455 / 473 | 484 / 510 | 28.33× | 18.56× |
| 93.85 KiB | fast | 522.8 | 166 / 181 | 178 / 192 | 30.76× | 20.44× |
| 374.35 KiB | baseline | 728.9 | 486 / 517 | 518 / 546 | 22.93× | 19.59× |
| 374.35 KiB | fast | 1040.4 | 220 / 250 | 244 / 274 | 21.26× | 17.91× |

All 24 trials and 480 measured heights completed. Two of the 60 largest-block
fast heights required round 1: their all-receipt latencies were 3,772 and 3,719 ms.
Those two cycles consumed 34.7% of that arm's measured time; its 244-ms median
therefore does **not** imply a sustained 1.57 MB/s chain. They are included in the
reported 1,065,334 B/s. Per-trial CPU/store counters cannot establish their cause.

VM TX is summed once across validators in each node's measured window; RX is not
added. It includes STP, TCP control and other VM activity, not just BFT packets or
wasted/retransmitted bytes. Application offers are a different counter layer.
All online nodes supplied valid windows. Measured receive-budget and reported
transport wrapper/ring drops were zero; this does not prove zero UDP loss.

The separate one-second final-height hold produced aggregate VM TX rates of
0.51–1.35 MB/s for baseline and 1.50–2.68 MB/s for fast, using actual node window
durations. This holds final-height evidence without advancing the tip: it is
**not** the idle traffic of a quiescent chain. Fast improves latency but increases
this hold traffic.

Measured guest steal was 0.46–0.85% across these arms. Shared CPU credit balances
were not available, so neither low steal nor zero reported cgroup throttling
establishes steady-state headroom. All body sizes retain the same 3,152,048-byte
store image; replacement byte counts are in-memory serialization/hash/copy work,
not disk I/O. Observed end-boundary process-lifetime RSS high-water marks were
15.2–22.0 MiB, not a whole-node memory bound.

Raw same-region reports and archive:
`/var/tmp/d28-fly-20261126-1600/iad-grid-001{,.tar.gz}`. The deployed executable's
SHA-256 was `ea688cd47c257c53d2465a78dd3765f0d56b81e6a93d6fb62c897ce2a2efa738`;
the first campaign runner's SHA-256 was
`1769242a6ee78bf626e3a5494c85dd75a382fe60fe5acf1acf2cd19e31fe51e1`.
Original report hashes, controller/node height agreement and snapshot deltas were
independently checked, with no missing/invalid reports or excluded trials.

#### Cross-region run and shared-CPU limitation

The subsequent `wan-grid-001` campaign used two IAD and two FRA machines, the
same executable/first runner, seed 429, and the same 20 measured / three warmup
heights per trial. It took about 17 minutes. This was **not** a matched experiment
isolating network distance: IAD machines had already served the earlier campaign,
and shared CPU credit/host scheduling conditions changed during the run.

| Serialized body | Profile | Complete / trials | ACKed / planned measured heights | Chain KiB/s | First receipt p50 / p95 ms | All receipt p50 / p95 ms |
|---:|---|---:|---:|---:|---:|---:|
| 6.19 KiB | baseline | 3 / 3 | 60 / 60 | 3.96 | 925 / 4305 | 1280 / 4800 |
| 6.19 KiB | fast | 3 / 3 | 60 / 60 | 5.43 | 465 / 4492 | 561 / 4670 |
| 93.85 KiB | baseline | 3 / 3 | 60 / 60 | 48.23 | 783 / 6090 | 865 / 6402 |
| 93.85 KiB | fast | 3 / 3 | 60 / 60 | 36.43 | 543 / 7280 | 598 / 7521 |
| 374.35 KiB | baseline | 2 / 3 | 48 / 60 | 94.11 | 823 / 11680 | 898 / 12642 |
| 374.35 KiB | fast | 2 / 3 | 42 / 60 | 172.87 | 530 / 717 | 591 / 806 |

Percentiles describe observed receipts only: a stalled height with **no** decision
cannot appear in them. The largest fast arm's low p95 is not a successful or
robust tail-latency result. Throughput includes each partial trial's measured
stall duration (156.214 s baseline; 66.961 s fast), not just successful heights.
Their whole-process lifetimes were 180 s, with the remainder mostly warmup;
failed warmup time is not part of these measured-window rates.

Both failures occurred in repetition three. Retained Rust reports say
`whole-process benchmark deadline reached`; the controller then reports
`node disconnected before final ACK`. Three other child processes were cancelled
with SIGTERM during cleanup and produced no report. Local decision callbacks
already took seconds to tens of seconds before these failures; this is not
merely delayed controller reporting. Available records show no conflicting block
IDs or panic, but do not establish a single root cause for the slowdown.

All three trials' CPU windows exist for each 6-/94-KiB arm: pooled guest steal was
34.0% / 34.3% for baseline/fast at 6 KiB, and 35.4% / 48.4% at 94 KiB. This is
substantially worse than the earlier IAD campaign. Steal alone does not distinguish
shared-credit throttling from host contention. Largest-block CPU/TX/evidence
ratios have only **two-of-three matched-trial coverage**, because the failed trial
is missing some node reports; their low complete-subset steal cannot describe
the failed cohort. No failed controller duration was removed from chain rates.

Raw reports: `/var/tmp/d28-fly-20261126-1600/wan-grid-001{,.tar.gz}`. Read-only
postprocessing for both campaigns is retained as `*-analysis-v2.json` alongside
those directories, preserving original `summary.json` and runner provenance.

#### What the hold traffic does, and does not, show

In the fast 6-/374-KiB hold windows there were **zero new evidence or bare-vote
offers**, yet about 400 status and 12 hello messages/s across the four nodes
(roughly 59 KB/s of offered application bytes). Consequently, the 1.50/2.68 MB/s
VM TX cannot all be explained as new repeated evidence packages. STP 4.0 pads
ordinary datagrams to its current transfer unit and also sends ACKs/probes,
conditional copies and proactive parity. Previously queued work can cross the
measurement boundary. The retained counters do not identify these contributions
separately: neither TX/body nor TX minus offered bytes measures retransmissions
or packet loss. A final-height hold still sends statuses/hellos; it is not silent
protocol-only idle traffic.
