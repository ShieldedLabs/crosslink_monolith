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
until the unchanged two-second expiry. `economy` changes only baseline's polling
interval, from 25 to 100 ms, to test lower state-replacement frequency. All profiles
keep outgoing handoffs at two. By default, consensus timers, quorum, signatures
and the 1 MiB body cap are unchanged; explicit large-body and WAN overrides are
explained below. These are benchmark overrides, **not changed node defaults**.
Sizes and paired profiles are randomized within each repetition.
`--offline 1` keeps four identities in the roster but launches only three nodes.

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

### Exact large bodies and worldwide timing

`--body-bytes` selects exact **serialized block** sizes, including the previous
fat pointer, through 10 MiB. It uses 32 existing mock headers and deterministic
padding in their solution fields; it is incompatible with explicit `--headers`
or `--solution-bytes`. These are not valid Equihash solutions or real transaction
payloads. The canonical block parser, hash, vote signatures and fat-pointer
format are unchanged. Synthetic-payload counters count the serialized headers,
not the full block or its parent certificate.

Set `--max-body 10485760` explicitly and keep it fixed across a size sweep.
Changing the cap also changes the fixed durable-image work and transport
geometry, so doing it per size would confound the comparison. For seven members
this cap gives a 31,465,268-byte durable image, serialized/hashed/copied on each
replacement. The store is still in memory, not a disk-backed production store.
Large-cap benchmark transport settings admit the bounded frame and use 16-MiB
per-peer byte queues and 32 native receive-ring slots; default-cap settings are
unchanged. This is not a proof that every allocation fits a particular VM size.

`--timing-profile wan` uses propose/prevote/precommit timeouts of 10/5/7 seconds,
a 10-second source-request lifetime and 15-second status TTL. Timeout increments
remain 900/300/600 ms and rho remains 500 ms. These timers do not impose waits on
the successful quorum path, but do change failure recovery; these results must
not be labeled production-default performance. The default timing profile is
`production`. Host-service profiles such as `baseline` remain a separate choice.

Each completed seven-height trial covers a full round-zero proposer rotation.
Partial trials need not cover all seven, and fallback rounds can use other
proposers. For example:

```sh
python3 /app/d28_fly.py campaign --roster /data/roster.json \
  --binary /app/d28_bench --token-file /run/d28-token --out /data/global-grid-001 \
  --body-bytes 131072,524288,1048576,2097152,4194304,8388608,10485760 \
  --max-body 10485760 --timing-profile wan --profiles economy \
  --repetitions 3 --heights 7 --warmup 1 --seed 432 \
  --timeout-secs 600 --chatter-secs 2 --lifetime-secs 5400 \
  --max-trial-tx-bytes 2147483648 --max-campaign-tx-bytes 60000000000
```

Set the same `--max-trial-tx-bytes` on each worker. The per-node watchdog counts
whole-child VM-interface TX, including warmup, hold and control traffic; the
campaign budget sums those counters, not RX or just measured-chain traffic.
Missing or regressing counters fail closed when limits are enabled. These are
software stop thresholds, **not exact billing ceilings**: sampling, termination
and an in-flight trial can overshoot. Keep a separate machine lifetime limit,
collect results, and delete the temporary app. A budget stop is not a successful
completion of the planned campaign. Do not discard partial trials or manufacture
zero counters for missing node reports.

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

#### Near-cap blocks after the long campaign

`iad-nearcap-001` returned to four IAD machines without resetting their CPU state:
2,048 headers plus 320-byte synthetic solutions produced **1,042,791-byte bodies
(1,018.35 KiB)**. Seed 430 randomized three profiles over three repetitions,
12 measured heights plus two warmup, with a 90-second whole-process deadline.
The Rust executable was unchanged; runner SHA-256 was
`f70fcc0643af9c86cac0af9d80b15b1560c42880156a337ccd3955c5bc1d8bc8`.

| Profile | Complete / trials | ACKed / planned measured heights | Chain KiB/s | All receipt p50 / p95 ms |
|---|---:|---:|---:|---:|
| baseline | 0 / 3 | 12 / 36 | 81.73 | 8080 / 17524 |
| fast | 0 / 3 | 3 / 36 | 19.21 | 14369 / 16384 |
| fast_k1 | 0 / 3 | 11 / 36 | 75.96 | 4039 / 33199 |

These are **failed-attempt throughput observations**, including measured stalls,
not successful near-1-MiB capacity measurements. K1 did not make any trial complete.
Chronology and changing CPU conditions prevent attributing the difference from
the first IAD sweep solely to block size. Percentiles have only 12/3/11 observed
heights and omit uncompleted heights, so they are especially weak tail estimates.

There are 17 readable partial node reports, four truncated reports and 15 missing
reports; **no trial has all four usable counter windows**. Therefore no matched
network/evidence/CPU ratio is reported for these arms. The controller records
remain usable for the table. Sixteen readable reports explicitly identify the
whole-process deadline. One reports `D28: Storage` at 90.008 s: the in-memory
`MemoryBackend::replace` deliberately returns that error when its deadline expires
(`bench.rs:483–495`), strongly supporting a deadline race rather than demonstrated
image corruption. The exact failing mutation was not logged. All campaign-owned
children reached terminal states; missing/truncated outputs are not zeros.

#### Slower-polling control

`iad-economy-001` compared baseline with `economy` at 374.35 KiB: three paired,
randomized repetitions, eight measured heights plus one warmup, seed 431.
Only polling changed (25 to 100 ms); status cadence, receive budget and source
concurrency stayed at baseline. All six trials completed. CPU steal had fallen
to 0.47% / 0.34%, so this short run does **not** prove recovery under sustained
credit exhaustion. Runner SHA-256 was
`51de3dd390e91d8a3418b14bbc5b58a9585a357b817132ae476571b4e443725b`.

| Profile | Complete / trials | Measured heights | Chain KiB/s | All receipt p50 / p95 ms | VM TX / body | Offered evidence / body |
|---|---:|---:|---:|---:|---:|---:|
| baseline | 3 / 3 | 24 | 724.0 | 522 / 541 | 25.31× | 18.98× |
| economy | 3 / 3 | 24 | 342.8 | 1075 / 1202 | 24.75× | 18.52× |

All 12 node counter windows are present per profile. Replacements per node-second
fell from 79.90 to 31.96, but summed process CPU per committed block was 0.643 s
baseline versus 0.715 s economy. Lower replacement frequency did **not** produce
better throughput or demonstrated CPU efficiency per block. None of these
experiments changes production defaults or establishes a reliable sustained
maximum on this shared-CPU tier.

#### Final verification and retained artifacts

Across the four measured campaigns: **57 trials, 46 complete, 11 partial, 884
ACKed measured heights**. A separate three-height smoke run is excluded. All
184 report hashes stored by completed trials were checked; partial/missing report
coverage remains explicit. All four downloaded archives match their remote
SHA-256s. Raw reports, original summaries, read-only `*-analysis-v2.json`, logs,
verification receipts and the exact deployed executable are retained under
`/var/tmp/d28-fly-20261126-1600/`. The near-cap and slower-polling directories are
`iad-nearcap-001` and `iad-economy-001`, each with a matching `.tar.gz` archive.

The temporary Fly app `d28-perf-20261126-1600` and its six machines were destroyed;
its absence from the live app list was verified. No public service or production
identity was used. No remotely deployed testnet, disk-backed performance or
transaction/TPS claim is implied. The final code passed **282 Rust release tests**
(five existing ignored; unbounded legacy `single_rt` excluded) and **20 Python
runner checks**, including real four-process baseline/K1/economy campaigns.

### Seven-region, 10-MiB sweep — 2026-11-26

Seven separate `performance-1x`, 2-GiB Fly machines, one validator/executor worker
per machine, ran over private IPv6 in **IAD, SJC, GRU, FRA, JNB, SIN and SYD**.
Node 0/the controller was in IAD. This non-burst-limited tier avoided deliberately
reusing exhausted shared-CPU credits; it does not exclude host contention.
A short pre-run ICMP sample had directed mean RTTs of **82–403 ms** and 124/126
replies. It is not a UDP loss estimate. No probes ran during measured trials.

The cap stayed **10,485,760 bytes** across every size, fixing the durable image
at **31,465,268 bytes** and retaining the same large-cap transport settings.
Bodies use deterministic padding in existing synthetic header solution fields,
not transactions or valid PoW work. The mock store is in memory. These are not
full-node/disk-backed results, and cannot be compared directly with the earlier
shared-CPU, 1-MiB-cap campaigns. No PoW/ledger formats or rules changed.

A separate four-trial pilot compared baseline/economy at 128 KiB and 10 MiB,
with two measured heights plus one warmup each. All four completed. Economy's
10-MiB measured throughput was 348.791 KiB/s versus baseline's 281.486 KiB/s;
both small endpoints also completed. This justified a calibration choice, not a
statistical superiority claim. Pilot observations are excluded from the table.

The frozen primary used **economy** (100-ms polling/status, receive budget 32,
K=2), benchmark WAN timers 10/5/7 seconds, seven measured heights plus one warmup,
seed 432, three planned repetitions per size, a 600-second process deadline and
5,400-second campaign limit. Software guards were 2 GiB TX/node/trial and 60 GB
aggregate whole-child TX. The pilot showed the earlier 180-second/1-GiB limits
would risk censoring the longer eight-height trials. Guards did not trigger.

#### Collected results and shutdown boundary

The user requested shutdown before all **21 trials / 147 measured heights** were
collected. **Eleven trials have finalized controller records: 77 measured
heights**, with complete seven-node counter coverage in each included trial.
An additional 10-MiB trial completed on all nodes but collection was interrupted;
it is reported separately below. Nine trials never started.

The table uses only those eleven controller-complete trials. First/all latency
is IAD-controller receipt time, not synchronized cross-machine event time. Rates
count each chain body once and include the all-seven ACK barrier. Percentiles
pool observed heights; **p95 is the observed maximum with these 7/14 samples**,
not a stable tail estimate. Unequal repetitions and fallback rounds prevent
calling the 10-MiB point a demonstrated optimum or capacity maximum.

| Exact body | Trials / planned | Heights / planned | First receipt p50 / p95 (s) | All receipt p50 / p95 (s) | Unique chain KiB/s | VM TX / body | Offered evidence / body |
|---:|---:|---:|---:|---:|---:|---:|---:|
| 128 KiB | 2 / 3 | 14 / 21 | 20.730 / 46.607 | 21.001 / 46.829 | 7.910 | 247.079× | 46.004× |
| 512 KiB | 1 / 3 | 7 / 21 | 22.373 / 39.987 | 22.632 / 40.215 | 26.034 | 118.094× | 47.812× |
| 1 MiB | 2 / 3 | 14 / 21 | 21.666 / 47.874 | 22.071 / 49.406 | 48.790 | 88.727× | 45.193× |
| 2 MiB | 2 / 3 | 14 / 21 | 23.582 / 46.054 | 24.590 / 46.620 | 83.852 | 82.444× | 48.242× |
| 4 MiB | 2 / 3 | 14 / 21 | 22.891 / 69.534 | 24.084 / 70.317 | 129.241 | 84.415× | 53.016× |
| 8 MiB | 1 / 3 | 7 / 21 | 42.204 / 47.858 | 43.107 / 49.276 | 222.514 | 79.064× | 57.295× |
| 10 MiB | 1 / 3 | 7 / 21 | 23.488 / 41.703 | 25.131 / 43.005 | 368.118 | 63.621× | 48.005× |

Within this completed-controller cohort, 80× larger bodies (128 KiB to 10 MiB)
bought **46.5× throughput**, while average all-ACK chain cycle rose **16.183 to
27.817 seconds**. Median first receipt rose 20.730 to 23.488 seconds. The trend
supports amortizing fixed costs with larger blocks; the irregular 8/10-MiB
latencies do not establish that larger blocks intrinsically reduce latency.

**Recovered trial 12, 10 MiB repetition 2:** all seven Rust reports are complete,
exit zero, cleanup-confirmed, and agree on eight ACKed heights including seven
measured heights. Thus **84/147 measured heights have node-ACK evidence overall**.
SIGINT arrived during collection, so its controller/result and the campaign
`run.json` were not finalized. Existing artifacts were preserved; remaining
worker artifacts were recovered separately, without inventing controller times.
This is **interrupted collection, not a protocol/deadline failure**. Node 0's
local measured window was 242.090 seconds, yielding **296.088 KiB/s** with local
callback median/max **26.484/51.712 seconds**. Its round-5 warmup took 120.113
seconds. Those local measurements are not pooled with controller receipts/rates.

#### What limits this prototype

- **Fallback dominates many decisions:** of the 77 controller-recorded measured
  heights, 17 decided in round 0, 44 in round 1, 14 in round 2 and two in round 3.
  All seven additional recovered heights also used later rounds. No transition,
  timer-expiry or proposal-arrival trace identifies the initiating cause; the
  successful quorum path does not inherently wait out the 10/5/7-second timers.
- Every state replacement processes the entire **31.47-MB image**, even for a
  128-KiB block. Counted replacement work was **35.7–70.2 GiB per committed block**
  summed across seven nodes; process CPU was **32.8–94.1 CPU-seconds/block**.
  This is serialization/hash/copy work, not measured disk or memory-bus traffic.
  Guest steal was 0.30–0.38%; this does not establish an always-saturated CPU or
  prove that image work caused the fallbacks.
- Measured receive-budget drops totaled **4,164 messages**. The transport's
  reported wrapper/ring drop counters were zero; neither establishes UDP loss.
  Critical small-message delays/drops remain a plausible contributor, not a
  demonstrated single cause.
- VM TX amplification was **63.6–247.1×** unique body bytes; evidence offers alone
  were **45.2–57.3×**. This includes necessary replication and different framing
  layers, not exclusively waste or retransmissions. All counter ratios have
  full matched-trial coverage in the completed cohort.
- During the separate two-second final-height holds, there were **zero new
  evidence or bare-vote offers**, yet aggregate VM TX was **1.78–21.27 MB/s**.
  Queued transfers, control, padding, probes and transport redundancy are not
  separately identified. This is not quiescent-chain idle traffic.

Whole-child TX is a separate accounting window: 22,420,830,910 bytes for the
11 finalized primary trials and 6,898,615,893 for recovered trial 12, totaling
**29,319,446,803 bytes**. The pilot added 4,954,105,436 bytes. These interface
counts are not a verified billable-egress invoice and are not used as numerators
in the table's measured-window amplification.

#### Provenance, verification and resource cleanup

Root: `/var/tmp/d28-fly-global-20261126-1708/`.

- Binary source: `b89acd6bafb91307508a4ff1126c433f01d0608f`; integrated Rust sources
  matched the branch. Executable SHA-256:
  `c8481fd2ae8422ea1423e2a1548a3247f73b395e1467d5a862708a72436388ad`.
- Runner SHA-256: `2d46af2d3f7329fe98f5a8d24b3debfa416800fa7940c13644df1bc380c48eec`.
- `final-iad/global-grid-001/` retains original primary files;
  `final-iad/user-stop-recovery/` retains recovered worker outputs.
  `global-grid-001-completed-analysis-v2.json` explicitly excludes the interrupted
  controller collection instead of manufacturing its missing metadata.
- `primary-independent-audit.json` verifies 77 original completed-trial report
  hashes, 616 node-height records, 154 measured/hold windows and 9,856 boundary
  deltas. `pilot-independent-audit-338.json` audits the separate pilot.
- All seven `data-final-REGION.tar.gz` archives were downloaded and hash-verified
  before deletion; receipts are in `final-archives-verified.json`.
- **The temporary app `d28-global-20261126-1708` and all seven machines were
  destroyed**, and its absence from the live app list was verified. All benchmark
  children were confirmed stopped before archiving. See `cleanup-receipt.json`.

The pinned implementation passed **291 release Rust tests** with
`CROSSLINK_FUZZ_SEED=428` (five existing ignored tests; unbounded `single_rt`
excluded), plus **38 Python checks including the real-process tests**. An initial
random-seed run reproduced an inherited legacy coverage-only assertion failure:
seed `2960165695641715445` generated three missing-leading-signature cases versus
its required four, after functional assertions passed (`src/lib.rs:6646`). Both
failure logs are retained; no unrelated legacy code was changed. No additional
performance experiments were started after the shutdown request.

## Outlined-payload component profile — 2026-11-27

The first payload experiment separates the existing signed proposal header and
certificate metadata from the serialized body. A missing body is fetched
separately; later evidence reuses descriptor-retained body bytes. Existing keyed
block IDs, proposal/vote signatures, canonical block parsing and the Host validity
boundary are unchanged. Inline transport remains the production default.

This is **seven separate localhost validator processes**, not a new Fly campaign
or a WAN-capacity result. All six planned trials completed: seven measured heights
and one excluded warmup per trial, **42 measured chain heights** total. The body
sizes were 128 KiB, 1 MiB and 10 MiB, with the **same 10-MiB capacity in every arm**.
Each process used one executor worker; all arms used the economy host profile,
100-ms polling/status, 32-message receive budget, two source requests/two outgoing
leases, and WAN benchmark timers. There were no overlapping builds or benchmark
jobs during this primary profile. Earlier functional gates are excluded.

### Stage-one results

Receipt latency uses the controller's single monotonic clock. Each cell is the
**median / observed maximum** over seven heights, not a stable tail estimate.
Throughput counts one chain and includes the all-online ACK barrier.

| Body | Mode | First receipt, seconds | Chain KiB/s | Raw payload/body | Total offered application/body |
|---|---|---:|---:|---:|---:|
| 128 KiB | inline | 22.185 / 30.566 | 5.426 | 44.000x | 56.026x |
| 128 KiB | outlined | 23.395 / 47.721 | 4.264 | 13.714x | 28.802x |
| 1 MiB | inline | 30.722 / 30.987 | 37.459 | 48.143x | 49.892x |
| 1 MiB | outlined | 24.430 / 48.000 | 30.850 | 14.571x | 16.756x |
| 10 MiB | inline | 24.903 / 45.642 | 353.720 | 48.000x | 48.169x |
| 10 MiB | outlined | 26.388 / 47.443 | 312.749 | 13.714x | 13.917x |

Outlining reduced raw payload offers by **68.8–71.4%**. Total application offers
fell by **48.6%, 66.4% and 71.1%**, respectively. It did **not** improve sustained
chain throughput in this sample: rates fell **21.4%, 17.6% and 11.6%**. The 1-MiB
median improved, but its maximum worsened; the other medians were slower too.
All seven nodes agreed on each final round: **34 round-one and eight round-two
measured decisions, with none in round zero**. These reports do not establish why
round zero failed. The 10/5/7-second WAN timeout settings are fallback timers,
not an unconditional minimum decision time.

The outlined ratios are still above the six-copy ideal for one successful
proposal delivered once to every other validator. They can include different
proposals across fallback rounds and repeated transfers. The bounded
`duplicate_payload_bytes` counter remembers only the most recent body offered to
a peer in a height/session; it is **not** global duplicate detection or total waste.
The 31,465,268-byte atomic state image also remains unchanged in every arm.

### Accounting and reproduction

`d28_fly.py local` retains the same distributed controller/worker reports while
running every validator as its own local process. It checks child ownership and
cleanup, removes only its own token, and suppresses invalid independent-VM
aggregation of shared-host interface, guest-CPU and cgroup counters. No Fly
resources were provisioned for this experiment.

```sh
cargo build --offline --locked --release --features bench \
  --manifest-path tenderlink/Cargo.toml --bin d28_bench
python3 -B tenderlink/benchmarks/d28_fly.py local \
  --binary "$PWD/tenderlink/target/release/d28_bench" \
  --out /var/tmp/d28-outline-new --nodes 7 \
  --body-bytes 131072,1048576,10485760 --max-body 10485760 \
  --payload-modes inline,outlined --profiles economy --timing-profile wan \
  --repetitions 1 --heights 7 --warmup 1 --timeout-secs 600 \
  --chatter-secs 2 --lifetime-secs 4800 --seed 428
```

Raw payload bytes are a **subset**, not an additional traffic category. Total
offered application bytes sum exactly six disjoint categories:
`offered.{hello,status,bare_votes,evidence}.bytes` and
`payload.{offered_control,offered_data}.bytes`. Outlined metadata is already in
evidence; body bytes are already in inline evidence or separate data. Use only
the common trial cohort with all six counters present on every online node.
Divide by controller-ACKed measured body bytes, once per chain, and retain failed
or partial durations. These are offers, **not delivery or STP/UDP wire bytes**.

Campaign completeness comes from `campaign/run.json` and `local-result.json`,
not merely summary rows for surviving trial directories. This run's independent
audit checked 42 original reports, six controllers, all 252 category/node deltas,
matching IDs/sizes/rounds/ACKs, 54 input hashes and worker cleanup. All workers
exited zero; threads stopped and the run token was removed. The separate
final-height chatter windows are not quiescent-idle measurements.

Root: `/var/tmp/d28-payload-20261127/`.

- Primary reports: `outline-primary/campaign/` and retained worker originals.
- Frozen plan/preflight: `outline-primary-plan.json`, `outline-preflight.json`.
- Audited result: `outline-primary-independent-audit.json`, SHA-256
  `ae6dc89d402facee47a987394bbadeb005365cc50445f03aab6190715b116bca`.
- Pinned executable: `d28_bench-outlined-2a01a05e`, built from integrated source
  `35b5d2bc6623c77bda6baf469766e7067adbffe0`, SHA-256
  `2e9d5239c823b09f38809f8f1f1408618fe463e781ce30a4f4d529101f28f74c`.
- Historical runner SHA-256:
  `43af8ea239f57702ede5eaf88e3da66ee09c7c4d4b7c5fd7189f11265e03227b`.

The later certificate-admission hardening in `b3023b31` is not retroactively
included in these measurements. A Merkle comparison must use a fresh outlined
control on the **same final executable** as its leaf-size arms; comparing only
against this earlier stage would also confound code version and time order.

### Merkle functional gate and retained failures

The first seven-process Merkle gate used a 10-MiB body/cap, one warmup height and
one measured height, the same economy/WAN settings for every arm, and a
300-second whole-trial deadline. This is a functional gate, **not** the primary
leaf-size comparison. No Fly machines were provisioned.

| Transport | Warmup | Measured height | Outcome |
|---|---|---|---|
| Outlined whole body | Complete | Complete, 26.77-s all-node cycle | Complete |
| Merkle, 32-KiB leaves | Did not complete | Not reached | Partial at deadline |
| Merkle, 100-KiB leaves | Complete, round 6 | No decision | Partial at deadline |
| Merkle, 256-KiB leaves | Complete, round 5 | No decision | Partial at deadline |
| Merkle, 1-MiB leaves | Complete, round 1 | Complete, 32.06-s all-node cycle | Complete |

This was not a blanket proof-verification failure: during the undecided measured
height the 100-KiB arm completed nine full assemblies across seven nodes and
verified 188,887,040 leaf bytes; the 256-KiB arm completed 27 assemblies and
verified 310,640,640 bytes. The 32-KiB reports contain no measured application
window: its counters are **unavailable, not zero**. Its full stalled duration
still belongs in chain-rate accounting.

Source review identified two interacting risks, not a trace-proven attribution
of these runs: one-shot leaf acknowledgements could leave an eight-leaf sender
window occupied until its ten-second lease expired, and completed/partial
acquisition can be discarded at round or ownership boundaries. STP is best
effort; its send-backlog count retires planned send work, not reliably
acknowledged message delivery. The gate did not record individual ACK-loss or
lease-expiry events.

All original failures are retained under
`/var/tmp/d28-payload-20261127/merkle-seven-node-gate/`. The pinned executable is
`d28_bench-merkle-e365a1a0`, SHA-256
`fafef8158e088f201a9d9661893d43cd70acd05a4f397d8ffabf34cc5b793c30`.
An independent summary failure on null counter windows was repaired in
`eb33e41d3`; the repaired summarizer reconstructs five cases, two complete and
three partial, without fabricating counters or removing failed durations.
All 517 original file hashes were unchanged when the derived
`campaign/summary.json` was added. Its SHA-256 is
`5bdc600be26006484f7fe770371e2ff32bbf2d1ab373f0659b2697b9f2305621`.

## Historical ACK-gated Merkle comparison — 2026-11-27

**These measurements do not isolate the effect of Merkle leaf size.** The old
application protocol held two eight-leaf batches until every leaf was acknowledged
or the batch timed out. That allowed only 512 KiB of leased leaf data at 32-KiB
leaves, versus 16 MiB at 1-MiB leaves: a **32-fold difference in byte allowance**.
It also coupled unrelated unordered messages to batch-wide completion. The
results below are retained as measurements of that implementation, not evidence
against 100-KiB leaves or a properly paced unordered shard design.

Outlining saved bandwidth; the ACK-gated Merkle implementation did not improve
throughput in this configuration. The 1-MiB leaf arm completed and offered fewer
bytes than its fresh outlined control. Smaller-leaf arms were substantially
slower and partial. This is a seven-process **localhost** experiment, not a
distributed/WAN result.

### Frozen primary results

All five arms used the same final executable, 10-MiB serialized bodies and body
capacity, economy polling/status at 100 ms, WAN benchmark timers, receive budget
32, and two incoming/outgoing payload windows. Merkle windows contain at most
eight leaves. Each trial planned one warmup plus seven measured heights with a
600-second **whole-trial** deadline. There were no concurrent builds/tests or
other task benchmarks. The diagnostic trace was disabled. No settings were tuned
between arms, and no failed arm was replaced by a successful retry.

| Transport | Measured heights | First receipt p50 / maximum, s | Chain KiB/s | Raw payload/body | Total offered application/body |
|---|---:|---:|---:|---:|---:|
| Outlined whole body | 7/7 | 25.332 / 46.529 | 318.932 | 13.714x | 13.919x |
| Merkle, 1-MiB leaves | 7/7 | 27.908 / 30.069 | 291.100 | 11.414x | 11.776x |
| Merkle, 256-KiB leaves | 3/7 | 139.162 / 179.689 | 65.593 | 31.600x | 33.673x |
| Merkle, 100-KiB leaves | 2/7 | 209.623 / 247.293 | 37.773 | 36.483x | 41.451x |
| Merkle, 32-KiB leaves | 0/7 | unavailable | 0.000 | undefined | undefined |

All **five planned trials ran**: two complete, three partial; **19/35 measured
heights** completed, plus all five warmup heights. Exit status 1 reports partial
trials, not a runner exception. The 32-KiB arm reached its all-node warmup barrier
only after approximately 521 seconds, leaving 78.733 seconds of measured time
without a decision. It did **not** produce zero blocks during the entire trial.

Latency is controller first receipt on a single monotonic clock, nearest-rank
p50 and observed maximum, with only 7/7/3/2/0 samples in table order. Partial-arm
latencies describe **successful heights only**; they do not represent unfinished
heights. Throughput waits for all seven validators and includes the remaining
stalled measured time. Post-warmup rate durations in table order are
224.750/246.238/468.339/542.193/78.733 seconds. These tiny samples do not establish
reliable tail latencies or a sustained optimum.

The 1-MiB arm reduced raw payload offers by **16.8%** and total application offers
by **15.4%**, but chain throughput was **8.7% lower** than the fresh outlined
control. Its first-receipt maximum was smaller, yet all-node cycles remained
slower. This is not a demonstrated latency/throughput win. The earlier inline
versus outlined profile is a separate experiment and is not pooled here.

All six disjoint application-byte categories have complete 7/7-node coverage in
every primary arm. Ratios include work on unfinished heights, not just committed
bodies. The zero-decision arm still offered **50,711,659 application bytes** in
its measured interval; dividing by zero committed bytes is undefined, not zero.
Raw payload, proofs, availability and known duplicates remain subset counters.
Actual network byte ratios are unavailable on this shared-host run.

The separate final-height holds offered **46.435 KiB/s** of aggregate application
traffic for outlining and **158.336 KiB/s** for 1-MiB leaves, with zero raw payload
bytes in either hold and complete 7/7-node counter coverage. These rates sum each
node's bytes over its actual local hold duration; they are not network-byte rates
or quiescent-idle chatter. Partial arms did not reach a hold window.

### Leaf geometry versus implementation costs

For a 10-MiB body the implemented geometry is:

| Leaf size | Leaves | Availability bitmap | Merkle proof per leaf | Full signed HAVE record |
|---|---:|---:|---:|---:|
| 32 KiB | 320 | 40 B | 288 B | 286 B |
| 100 KiB | 103 | 13 B | 224 B | 259 B |
| 256 KiB | 40 | 5 B | 192 B | 251 B |
| 1 MiB | 10 | 2 B | 128 B | 248 B |

The bitmap is small as intended, and leaf size is independent of STP packet
size. HAVE also carries the reference and holder attestation, so the full record
is larger than the bitmap. A transport Merkle root never replaces the existing
chain body ID: complete reconstruction, canonical encoding, keyed body-ID,
proposal/certificate and Host validation remain mandatory. Neither a bitmap nor
an ACK grants consensus validity. Production defaults still use inline transfer;
no PoW/ledger formats changed.

The measured inefficiency is not explained by large Merkle proofs. In the
100-KiB arm, proof offers were 1,680,672 bytes versus 765,112,320 raw payload
bytes. Its measured interval completed 48 assemblies but only two chain heights.
The 256-KiB arm completed 87 assemblies for three heights; the 1-MiB arm completed
75 for seven. These counters are not a global duplicate detector, but show
substantial acquisition work beyond one copy per recipient.

### Repair and separate diagnostic evidence

Before this primary sweep, `f850f396` replaced one-shot leaf acknowledgements with
one bounded, cumulative eight-index receipt per peer, refreshed on the existing
status cadence. An actual dropped-message regression failed before the repair
and passed afterward. Exact-ticket fencing, receipt expiry, old-token watermarks,
reordered/duplicate leaves and scope retirement have regression coverage. There
were no timer, budget, window-size or body-retention changes.

The separate 100-KiB diagnostic completed one height in 59.450 seconds. Its
traces stayed below their caps (5,074 events total; each process below 4,096) and showed:

- 153 source windows fully acknowledged in 386–1,360 ms on each source's clock.
- One send timeout, with two leaves missing at the receiver despite repeated
  cumulative ACK arrivals—not the original all-leaves-received/ACK-lost pattern.
- Nine WANTs without an accepted-request trace; loss versus rejection is unknown.
- Four accepted requests that offered zero leaves after an old reference became
  unserviceable at a round change; three cost a full receive timeout.
- Four nodes successfully retained a full body in round 1, then reacquired that
  same body from zero in round 2. Retention at completion is not retention across
  rounds. The precise later eviction/handoff event was not instrumented.

These observations identify request/ownership and round-transition limitations;
they are not per-event causal proof for the later untraced primary trials. The
primary 1-MiB arm still discarded 4,735 messages at the runtime receive budget on
localhost. No claim of a complete acquisition-liveness or whole-node memory proof
is made. The original failed gate and diagnostic are retained separately.

### Reproduction, verification and artifacts

```sh
cargo build --offline --locked --release --features bench \
  --manifest-path tenderlink/Cargo.toml --bin d28_bench
unset D28_LEAF_TRACE
python3 -B tenderlink/benchmarks/d28_fly.py local \
  --binary "$PWD/tenderlink/target/release/d28_bench" \
  --out /var/tmp/d28-merkle-new --nodes 7 \
  --body-bytes 10485760 --max-body 10485760 \
  --payload-modes outlined,merkle \
  --payload-leaf-bytes 32768,102400,262144,1048576 \
  --profiles economy --timing-profile wan --repetitions 1 \
  --heights 7 --warmup 1 --timeout-secs 600 --chatter-secs 2 \
  --lifetime-secs 4200 --seed 428
```

Source `fcbb2f635ad405eb82ca119d8c006e6e00dc31d9` passed **350 optimized Rust tests**
with `CROSSLINK_FUZZ_SEED=428`, five existing ignored tests and unbounded legacy
`single_rt` excluded; the non-benchmark release library check; the optimized
benchmark build; and **43 Python checks including real-process tests**. An initial
existing transport test encountered an address-in-use error. Its unchanged
focused retry and the subsequent full suite passed; both logs are retained.

Root: `/var/tmp/d28-payload-20261127/`.

- Primary originals and derived summary: `merkle-primary/campaign/`.
- Frozen plan and preflight: `merkle-primary-plan.json`,
  `merkle-primary-preflight.json`.
- Pinned executable: `d28_bench-merkle-f850f396`, SHA-256
  `6bdfdf6abf5512f79f5eee41f0535d4f3c44376a8de40c78b75ea501afb3cf46`.
- Runner SHA-256:
  `5e2c8a24a28d64c5fcf66ec2dcd4179160c816ff037f540b3c172fd3f7ea6bda`.
- ACK diagnostic: `merkle-ack-diagnostic/`, including its separate binary/patch,
  original reports, bounded traces and `trace-reduction.json`.
- Verification: `merkle-final-verification-retry.log`; initial bind failure and
  focused retry in `merkle-final-verification.log` and `merkle-port-bind-retry.log`.

The local cleanup receipt confirms all owned children/worker threads stopped and
the temporary token removed. No benchmark processes remain and no Fly resources
were created for either payload comparison.

## Unordered authenticated shards — 2026-11-27

This implementation replaces the historical ACK-gated experiment above, rather
than tuning its batch window. Only the proposer proactively pushes: independent
shuffled passes cover each connected peer without replacement. Receivers advertise
verified possession and randomly request missing indices from advertising peers.
Retries have independent 250–500 ms clocks; when alternatives exist, a retry avoids
its previous source. A 200 ms initial-progress grace avoids immediately racing
healthy initial delivery. No remote shard ACK releases application credit, and no
batch-completion or transport-empty barrier gates unrelated shards.

Every shard includes the original proposer's BFT-only commitment to the exact
header, consensus context, Merkle root and geometry, plus its proof path. Its
32-bit index is the first shard field after the two-byte tag/version and is bound
by the leaf hash. Partial receivers can relay verified leaves with the original
proposer proof. The reconstructed body must still pass the existing canonical
encoding, chain body-ID, certificate and Host checks. These transport signatures
are not a replacement for chain-facing signatures or validity.

For an exact 10-MiB body, the **new** wire geometry is:

| Shard size | Leaves | Availability bitmap | Proof per shard | Shard envelope plus proof | Full HAVE record |
|---|---:|---:|---:|---:|---:|
| 32 KiB | 320 | 40 B | 288 B | 663 B | 411 B |
| 100 KiB | 103 | 13 B | 224 B | 599 B | 384 B |
| 256 KiB | 40 | 5 B | 192 B | 567 B | 376 B |
| 1 MiB | 10 | 2 B | 128 B | 503 B | 373 B |

Envelope sizes exclude shard data and STP/UDP/IP overhead. These are not the
historical holder-only HAVE sizes in the preceding experiment.

### Bounds and 100-KiB-only tuning

Partial assemblies and completed caches share two additional body owners, separate
from the consensus core's three physical body slots. Per-peer inventories and
send histories contain bitmaps, not body copies. Useful content can survive round
changes, but finite cache ownership cannot retain every proposal ever observed.
Random selection does not guarantee recovery without an honest available source
and eventual useful delivery.

All shard-size arms use the same 10-MiB body cap, `max_requests=max_outgoing=2`,
**21,397,220-byte** encoded Merkle admission ceiling, 256 staged-message ceiling,
and 24 shard offers per service call. Admission charges observed transport backlog
and already staged frames, then observes native STP byte/slot headroom. These are
local admission/service bounds, not remote-ACK windows, equal byte-rate shaping,
or a whole-runtime/STP memory proof. In particular, a fixed messages-per-call cap
still allows different potential bytes per call at different shard sizes.

Optimization used only 100-KiB shards and seven separate localhost processes.
The initial `economy`, `baseline` and `fast` gates all completed in round zero.
A subsequent isolated receive-budget comparison held polling at 25 ms and status
at 100 ms: raising the receive budget from 32 to 128 (`balanced`) removed **1,312
receive-budget discards** across six measured heights, versus zero with balanced.
Observed throughput was 1.311 versus 1.302 MiB/s; this is not a demonstrated speed
improvement. Balanced was selected to avoid clean-link discards, without a material
observed throughput regression. All other production defaults remain unchanged.

A 49-Hz userspace CPU profile, including startup/warmup, found 37.56% self samples
in BLAKE3's AVX2 hash routine. Incomplete stack unwinding prevents assigning all
those samples to durable storage. The old image encoder nevertheless demonstrably
zeroed bytes immediately before overwriting them. The narrow optimization clears
only the unused tail; full-image hashing and atomic replacement remain unchanged.
Two differential tests compare complete output with the original encoder, including
dirty/reused buffers, shrink/clear transitions and three 10-MiB bodies. This is not
a storage-format redesign, removal of durability, or a benchmark-only bypass.

The 100-KiB A/B/B/A encoder comparison completed all 12 measured heights in round
zero without receive-budget discards. It **did not demonstrate a latency or
throughput improvement**: original versus tail-zero pooled throughput was 1.366
versus 1.278 MiB/s; first-receipt p50 was 6.394 versus 6.915 seconds. Aggregate
process CPU was 52.027 versus 51.885 seconds per height. Only six measured heights
per encoder were collected on a shared host. The tiny byte-identical change is
retained for eliminating provably redundant zero writes, not as an established
speedup. Both executables and every exploratory result are retained.

### Frozen comparison methodology and reproduction

The primary comparison uses seven separate localhost processes, exact 10-MiB
bodies, a fixed 10-MiB capacity, one executor worker per validator, `balanced`
host service and unchanged benchmark `wan` timers. The same executable runs all
five arms. Three repetitions interleave outlined whole-body transfer with the
four shard sizes, using seed 428 for trial ordering only; protocol randomness is
OS-seeded. Each trial has one warmup and seven measured heights, a 300-second
deadline and a separate two-second final-height hold. The plan has 15 trials and
105 measured heights; failures are retained, not retried or excluded.

Latency to the first certificate-backed decision report at the controller and
node-local callback latency are separate quantities. Chain throughput counts
one body per all-online-ACKed height, including stalled measured time. A small
sample does not establish stable p95/p99 tails. Total application bytes sum six
disjoint categories: hello, status, bare votes, evidence, payload control and
payload data. Raw body bytes, proof bytes and HAVE bytes are subsets, never added
again. Local shared-loopback counters cannot be attributed to separate machines:
**there is no aggregate STP/UDP/IP wire-byte measurement or WAN capacity claim**.

```sh
cargo build --offline --locked --release --features bench \
  --manifest-path tenderlink/Cargo.toml --bin d28_bench
unset D28_LEAF_TRACE
python3 -B tenderlink/benchmarks/d28_fly.py local \
  --binary "$PWD/tenderlink/target/release/d28_bench" \
  --out /var/tmp/d28-unordered-new --nodes 7 \
  --body-bytes 10485760 --max-body 10485760 \
  --payload-modes outlined,merkle \
  --payload-leaf-bytes 32768,102400,262144,1048576 \
  --interleave-payload-modes --profiles balanced --timing-profile wan \
  --repetitions 3 --heights 7 --warmup 1 --timeout-secs 300 \
  --chatter-secs 2 --lifetime-secs 6000 --seed 428
```

The selected code is `a0351df70f6e7ccba610e05526cd0cff4bc79c79`. Before measurement,
**359 optimized Rust tests** passed with seed 428 (five existing ignored tests,
unbounded legacy `single_rt` excluded), plus the non-benchmark release library
check, optimized benchmark build, and **46 Python checks including real-process
integration tests**. Production defaults still use inline transfer; PoW/ledger
formats and chain-facing signatures are unchanged.

Artifacts are under `/var/tmp/d28-shards-20261127/`:

- Plans, source/binary pins and verification: `plan.json`, `primary-design.json`,
  `primary-preflight.json`, `final-verification.log`.
- Optimization: `100k-gate-001/`, `100k-host-profiles/`, `100k-perf-001/`,
  `100k-balanced/`, `100k-encoder-{original-a1,tail-zero-b1,tail-zero-b2,original-a2}/`.
- Primary: `frozen-primary/campaign/`; separate availability gate: `100k-offline/`.
- Exact executable: `d28_bench-a0351df70`, SHA-256
  `921a5068a2013c711eaa8f5ca84ad4bf627b6ea4d4fb1461eebe3e13461032b0`.
- Runner SHA-256:
  `e7ca55ab9b99a97200b7a28d49f800a1c8723de6ee3047d52bb4ae6305593305`.

### Frozen primary results

All **12 Merkle trials completed**, covering 84/84 measured heights, every one in
round zero. The outlined control completed one trial and timed out in two, for
17/21 measured heights. Overall, 13/15 trials completed and 101/105 measured
heights were ACKed. Both partials remain in the table and throughput denominator;
no arm was retried. The campaign correctly exited nonzero for those partials.

| Transfer | Complete / trials | Measured heights | First receipt p50 / p95 s | Chain MiB/s | Raw payload / chain bytes | All application / chain bytes |
|---|---:|---:|---:|---:|---:|---:|
| 32-KiB shards | 3/3 | 21/21 | 10.83 / 11.66 | 0.895 | 6.001× | 6.253× |
| **100-KiB shards** | **3/3** | **21/21** | **7.02 / 8.41** | **1.270** | **6.000×** | **6.103×** |
| 256-KiB shards | 3/3 | 21/21 | 8.04 / 8.43 | 1.165 | 6.402× | 6.498× |
| 1-MiB shards | 3/3 | 21/21 | 5.99 / 6.90 | 1.510 | 11.652× | 11.711× |
| Outlined whole body | 1/3 | 17/21 | 46.25 / 72.31 | 0.217 | 16.706× | 16.974× |

Outlined receipt percentiles include only its 17 completed measured heights, not
the four censored heights. Its two partial trials each ACKed five of seven
measured heights after warmup. Measured controller durations were 268.295 and
270.694 seconds, ending at the whole-process 300-second deadline; warmup is not
silently added to measured time. Every completed outlined height used a later
round: eight in round 1, eight in round 2 and one in round 3. This is the current
whole-body implementation's result, not a lower bound for an ideal outlined
protocol.

The **100-KiB arm is the best observed bandwidth tradeoff**, not a universal
optimal leaf size. Its raw payload offers were exactly the six copies needed for
six other validators. Signatures, proofs and all control traffic took the total
to 6.103×, about 1.7% above that raw replication minimum. It made zero leaf repair
requests in the measured windows. This application accounting does not include
STP FEC, retransmissions, framing, UDP/IP or padding.

One-MiB shards bought **19.0% more throughput** than 100-KiB shards, at **91.9% more
application bytes**. They recorded 1,906 requested indices and 2,447 leaf offers,
versus 1,260 offers for one six-recipient pass across 21 bodies. Known repeated
same-peer offers were 430 MiB, but that counter is not a complete cross-peer or
cross-proposal duplicate detector. The fixed 100-KiB-tuned repair policy was not
retuned for larger shards. Counters show redundancy; they do not trace every
request to a particular delayed or lost STP message.

All primary arms reported zero runtime receive-budget discards and zero exposed
wrapper/native-ring drop/refusal counters. Normal receive rejections still
occurred; these counters do not mean zero stale or invalid messages. Process and
storage work remain substantial:

| Transfer | Aggregate process CPU s / height | Image replacement GiB / height | Peak process RSS MiB |
|---|---:|---:|---:|
| 32-KiB shards | 76.28 | 26.92 | 141.97 |
| 100-KiB shards | 53.44 | 16.10 | 144.47 |
| 256-KiB shards | 61.71 | 19.42 | 149.59 |
| 1-MiB shards | 48.52 | 14.76 | 169.22 |
| Outlined whole body | 304.23 | 113.03 | 167.41 |

CPU is summed across seven processes; image work is serialization/hash/copy work,
not disk traffic. RSS is a process-lifetime high-water mark, not seven-process
aggregate or a measured-window allocation delta. The fixed per-call message cap,
CPU contention and repeated durable-image work remain part of this implementation
comparison; the table does not isolate pure Merkle geometry or remote capacity.

Separate final-height holds had no Merkle payload-data offers but continued
application control traffic: **139–152 KiB/s aggregate** across the four shard
arms. These use actual per-node hold durations, not requested seconds. They are
unfinished-height holds, not quiescent idle-wire measurements. Outlined hold
coverage is only 1/3 trials and is not silently pooled as three zero-traffic holds.

### Two-offline availability gate and cleanup

A separate 100-KiB, seven-identity run with two validators offline completed all
seven measured heights, with five online validators agreeing on every decision.
Raw payload offers were 4.015× chain bytes and all application offers were 4.093×,
against a four-other-online-validator replication minimum. It issued 12 requested
leaf indices and recorded zero receive-budget discards. First-receipt p50 was
5.981 seconds; throughput was 0.750 MiB/s. Heights 5 and 6 required rounds 2 and 1,
with first receipts at 41.587 and 22.264 seconds; other measured heights used
round zero. This tests unavailable proposers and fallback, **not random packet
loss or a malicious-validator testnet**. Deterministic unit tests separately cover
dropped/reordered/duplicate leaves, nonresponsive advertisers, partial relaying,
independent retries, authenticated indices and bounded ownership.

All primary and offline-gate local cleanup receipts confirm owned child and
worker-thread termination and temporary-token removal. No `d28_bench` processes
remained after the sweep. No Fly resources were created. Exact originals and
partial trials are retained, with the concise derived table in
`primary-analysis.json`; no benchmark inputs were replaced by summaries.

An independent audit recomputed endpoint deltas, controller/node decisions,
percentiles, matched coverage, timeout denominators and cleanup from originals,
without importing the runner: **41,374 checks, zero errors**, with **1,522 input
SHA-256s unchanged**. It verified all 105 node reports and 90 worker-original
copies, including the two partial trials. This is an artifact/accounting audit,
not a fresh cryptographic or production-security audit. Receipt:
`primary-independent-audit.json`, SHA-256
`0d2494a6d93e08467541a28487464cb9241c6c9e007a9593852b4954d153f221`.

## Sparse durability and immutable bodies — 2026-11-27

The runtime now opens `DurableStore::open_buffered`. Logical mutations stage in
its existing bounded working state rather than encoding, hashing and replacing
an entire image for every mutation. This is not a non-durable benchmark bypass:

- Partial shard assemblies stay volatile. A completed body is committed before
  dependent externally visible work; there is no per-shard persistence protocol.
- Signing intents and their supporting state are committed before signing.
  Publication state is committed before dependent callbacks, evidence service,
  Host scheduling, status publication and Merkle attestations.
- Countdown/rho-only progress uses a fixed-age one-second checkpoint, not a
  sliding debounce that busy traffic can defer forever. Scheduler/storage stalls
  can extend that interval. Already-durable output can replay without forcing
  unchanged authority plus a new countdown to disk on every poll.
- Any mutation, encoding or barrier error fails closed. This includes a backend
  reporting an error after actually replacing the block. Recovery reloads and
  validates the old-or-new durable state, never trusting the discarded RAM copy.

The fixed image format and `AtomicBlockStore` API are unchanged. The measured
image is still **31,465,268 bytes**. Zebra still uses its identity-bound, synced
RocksDB replacement; no PoW/ledger formats or chain-facing signatures changed.
The eager `DurableStore::open` path remains available with its reference crash
cuts. Buffered countdown recovery can repeat elapsed/replay work; it does not
promise identical intermediate crash traces or charge time spent offline.

A completed Merkle owner also retains an immutable, configured-hash-checked body
with a canonical-encoding wrapper. Subsequent evidence can share that allocation
without copying, parsing, re-encoding or hashing its entire payload again. This
uses the existing two additional body owners, not an unbounded proposal cache.
New header/context/certificate checks and Host validity remain necessary; recovery
rechecks actual bytes. Retained-core-only fallback remains unoptimized.

A focused work-counter test delivers 12 certificates for a 280,571-byte body.
Fallback processes **3,366,852 bytes** in each of the body-hash and canonical-check
paths; the completed-owner path adds **zero** such bytes after the single assembly
check. Pointer tests confirm allocation reuse on freeze and partial-owner refresh.
These counters do not cover every hash, Host parse, Merkle operation or allocation.

### Frozen measurement and results

All arms keep **100-KiB shards, exact 10-MiB bodies, seven identities, one executor
worker per validator and benchmark `wan` timers**. The same-profile comparison
uses `balanced` service: 25-ms polling, 100-ms status and a 128-message receive
budget. Two old-code trials preceded the candidate; two candidate trials were
followed by two more old-code trials. Each trial has one warmup and seven measured
heights, a 300-second timeout and a separate two-second final-height hold. A
prespecified two-trial `fast` sensitivity and one two-offline gate follow. No
builds/tests ran concurrently with measurements, and no code was retuned between
arms. This sequential shared-host comparison is not a randomized machine trial
or an ablation separating the two optimizations.

All **nine trials completed**, covering **63/63 measured heights**. The 56 heights
in all-online trials all decided in round zero. No failed or partial runs were
excluded. Here, latency is first certificate-backed receipt at the controller;
throughput counts one chain and waits for every online validator's ACK.

| Code / service | Measured heights | First receipt p50 / p95 s | Chain MiB/s | Application bytes / chain bytes | Receive-budget discards |
|---|---:|---:|---:|---:|---:|
| Old, balanced, before | 14 | 7.44 / 8.17 | 1.221 | 6.104× | 0 |
| **New, balanced** | **14** | **2.71 / 2.81** | **3.460** | **6.101×** | **0** |
| Old, balanced, after | 14 | 7.30 / 10.78 | 1.145 | 6.119× | 23 |
| New, fast | 14 | 2.85 / 3.15 | 3.168 | 6.213× | 435 |
| New, balanced, two offline | 7 | 2.48 / 38.74 | 0.980 | 4.073× | 0 |

The unchanged-settings candidate is **2.83× faster than the before control and
3.02× faster than the after control**. Relative to before, its median first
receipt is 63.5% lower. Quorum/all-receipt medians are 2.790/2.856 seconds, versus
7.740/7.959 before. Small samples do not establish stable tail percentiles.
Application traffic sums six disjoint offered-byte categories, not wire bytes.
The new balanced raw payload ratio is 6.027×, with 39 requested shard indices;
its 6.101× total remains close to the six-copy minimum. It is not a zero-repair run.

| Code / service | Aggregate CPU s / height | Replacements / height | Replaced-image GiB / height | Peak process RSS MiB |
|---|---:|---:|---:|---:|
| Old, balanced, before | 56.97 | 546.00 | 16.000 | 142.80 |
| **New, balanced** | **14.06** | **75.36** | **2.208** | **147.80** |
| Old, balanced, after | 58.42 | 572.21 | 16.768 | 145.09 |
| New, fast | 16.17 | 76.43 | 2.240 | 158.26 |
| New, balanced, two offline | 19.34 | 107.43 | 3.148 | 143.16 |

CPU is summed across the online processes, not wall-clock latency. Replaced-image
bytes are image size times replacement count, not physical disk traffic or a count
of every memory pass. RSS is the largest process-lifetime high-water mark. The
balanced candidate reduces CPU by **75.3%** and replacement volume by **86.2%**
relative to before. It still performs 2.21 GiB of full-image replacement work per
10-MiB block: this change reduces frequency, not the size of each required commit.

The `fast` profile changes both polling (25→5 ms) and status (100→25 ms), not just
one variable. It is slower here and discards 435 messages at the receive budget;
**balanced remains selected**. Separate final-height holds offer 208.4 KiB/s of
aggregate application control traffic with new balanced, versus 141.5 before and
136.3 in the repeated old control. Fast offers 821.7 KiB/s. All have zero payload
data offers during those holds. These are unfinished-height holds, not quiescent
idle or wire-traffic measurements; the increased control rate remains a cost.

With two identities offline, all five online validators agree on seven measured
heights. Heights 5 and 6 require rounds 2 and 1, with first receipts at 38.743 and
19.799 seconds; the other five use round zero. This is unavailable-proposer
fallback, not a Byzantine-node or packet-loss deployment. Raw payload is 4.003×
and total application offers 4.073×, against the four-copy online minimum.

### Verification, reproduction and retained artifacts

Candidate source is `b1792811cc7bce9340e43e2459ab7f392b22e30b`. The integrated
optimized suite passes **390 Tenderlink Rust tests**, with five existing ignored
tests and unbounded legacy `single_rt` excluded. The non-benchmark release library
check, benchmark build, **46 Python checks** including real subprocess integration,
and **four Zebra adapter tests** pass. New crash tests cover required output/signing
barriers, complete-body retention, recovered locks, discarded staged rows, progress
checkpoints, and old/new outcomes at replacement failures. Four generic-Storage
regressions also check non-Storage error normalization and refusal to retry/output.
These are fault-model tests, not physical power-loss/fsync certification.

Artifacts are under `/var/tmp/d28-sparse-20261127/`:

- Frozen inputs: `plan.json`, `candidate-plan.json`, `candidate-preflight.json`.
- Suites: `verification-receipt.json`, `final-verification.log`,
  `zebra-adapter-tests.log`, `sparse-crash-tests.log`.
- Measurements: `baseline-before/`, `candidate-balanced/`, `baseline-after/`,
  `candidate-fast/`, `candidate-offline/`; each retains raw reports and cleanup.
- Derived comparison with input hashes: `performance-analysis.json`.
- Candidate executable: `d28_bench-b1792811c`, SHA-256
  `62f766c07683bc1587873835577cf825cac25208762e7349c35fec85e82147a3`.
- Old executable and unchanged runner hashes are the same as the unordered-shard
  experiment above; the old `b8718d92a` result commit adds documentation beyond
  executable source `a0351df70`, not a different runtime implementation.

```sh
python3 -B tenderlink/benchmarks/d28_fly.py local \
  --binary /var/tmp/d28-sparse-20261127/d28_bench-b1792811c \
  --out /var/tmp/d28-sparse-new --nodes 7 \
  --body-bytes 10485760 --max-body 10485760 \
  --payload-modes merkle --payload-leaf-bytes 102400 \
  --profiles balanced --timing-profile wan --repetitions 2 \
  --heights 7 --warmup 1 --seed 428 --timeout-secs 300 \
  --chatter-secs 2 --lifetime-secs 1200
```

All cleanup receipts confirm owned children and worker threads stopped and tokens
removed; no benchmark processes remained after the sweep. No Fly resources were
created. These are separate localhost processes with a memory backend that still
executes real encoding/hashing and required atomic replacement barriers. They do
**not** establish physical disk throughput, worldwide performance, STP/UDP/IP byte
amplification or operation at the nodes' network-bandwidth limit.

The independent raw-artifact audit completed **29,089 checks with zero errors**:
61 node reports, 52 retained worker-original copies, 488 node-height records
including warmup, and 9,882 endpoint counter deltas. It independently reconstructed
controller latency/throughput and measured CPU, image work and application-byte
results, and checked provenance and cleanup receipts without importing the runner.
All **967 input hashes** were reverified unchanged afterward. No reported result
needed correction. This is an accounting/artifact audit, not new execution,
physical-durability or cryptographic certification. Receipt:
`sparse-independent-audit-399.json`, SHA-256
`3115230fc0c004a07e56eadab990803850cbae4c5f206510fc8b8da0f2106da8`;
its input manifest is `sparse-independent-audit-399-inputs.json`.

## Worldwide dedicated-CPU and throughput experiments — 2026-11-27/28

Seven private Fly machines ran in **IAD, SJC, GRU, FRA, JNB, SIN and SYD**.
RAM stayed at **8192 MiB per machine** while dedicated performance CPUs changed
from one to two to four. Every reported trial used **one Tokio executor thread
per validator**, with STP's separate native threads; a two-executor-thread arm was
planned but **not run**. Shards stayed at **100 KiB** throughout. These are real
D28/signature/Merkle/STP executions with mock bodies and memory-backed atomic
images, not Zcash transaction validation or physical disk/fsync benchmarks.
PoW/ledger formats and consensus rules were not changed by these experiments.

All D28 chain-throughput rates below count unique serialized body bytes **once
per chain**, divided by controller measured-start through final ACK. The benchmark
still waits for **all online validators** before advancing. First/quorum/all latency means the first,
fifth and seventh controller decision receipts, not exact certificate formation
or synchronized cross-machine timestamps. With two validators offline, quorum is
still five of seven, so quorum and all-online receipt times coincide. Percentiles
are nearest rank; **p95 is the observed maximum for both 7- and 14-height samples**.
Warmup, terminal holds and whole-child budget counters are kept separate.

### CPU sizing and block size

The same executable and runner were used for the hardware comparison: executable
SHA-256 `62f766c07683bc1587873835577cf825cac25208762e7349c35fec85e82147a3`,
runner `e7ca55ab9b99a97200b7a28d49f800a1c8723de6ee3047d52bb4ae6305593305`.
Each row has two trials, each with one warmup and seven measured 10-MiB blocks;
body cap is also 10 MiB. `balanced` uses 25-ms polling and 100-ms status cadence.

| Dedicated CPUs | Kernel ordinary buffer ceilings | Chain MiB/s | First receipt p50/p95, s | All receipt p50/p95, s | App offers/body | Measured VM TX/body |
|---|---|---:|---:|---:|---:|---:|
| 1, initial control | 208 KiB | 1.523 | 4.098 / 8.061 | 6.699 / 10.155 | 8.100× | 13.031× |
| 1, matched sizing control | 16 MiB | 1.569 | 4.351 / 7.970 | 6.525 / 9.127 | 8.180× | 13.110× |
| 2 | 16 MiB | 1.538 | 4.195 / 8.737 | 6.275 / 10.695 | 8.128× | 13.105× |
| 4 | 16 MiB | 1.566 | 4.237 / 8.593 | 6.014 / 10.478 | 7.847× | 12.717× |

**More CPUs did not demonstrate higher throughput in this configuration.** This
is not proof that CPU work or a serial critical path cannot matter. CPU seconds
summed over seven processes per committed block were 14.68, 14.35 and 13.92 for
the matched 1/2/4-CPU rows. Full-image replacement volume was 2.899, 2.912 and
2.886 GiB/block respectively; it is memory work, not disk traffic.

The original 212,992-byte (208-KiB) sysctl observation was **not an effective STP
socket-buffer measurement**. STP first tries `SO_RCVBUFFORCE`/`SO_SNDBUFFORCE`; these root VMs
could bypass the ordinary ceiling. An owned disposable socket successfully
forced 32 MiB above the ceiling on every VM (Linux reported 64 MiB). No baseline
undersized-buffer warnings appeared. Therefore the roughly 3% difference between
the first two rows is **not an attributed socket-buffer optimization**.

An instrumented four-CPU 10-MiB/cap-10 control reached 1.598 MiB/s. Its image
checksum span was 1.410 seconds/block summed across seven nodes, about 0.201 per
node, versus an all-node median receipt of 6.291 seconds. Phase spans nest and
must not be added as disjoint CPU costs. The instrumented control is not pooled
with the uninstrumented hardware rows.

The following separate sweep fixes the **body cap at 40 MiB**, keeping code,
hardware, shard size and timers identical within that sweep. Each row again has
two trials and fourteen measured heights. A 10-MiB body under a 40-MiB cap is not
the same storage workload as a 10-MiB/cap-10 case: the atomic image is about
120 MiB instead of 30 MiB.

| Body MiB | Chain MiB/s | First receipt p50/p95, s | Quorum receipt p50/p95, s | All receipt p50/p95, s | App offers/body | VM TX/body | Nonzero-round heights |
|---|---:|---:|---:|---:|---:|---:|---:|
| 10 | 1.343 | 4.916 / 8.820 | 5.110 / 9.132 | 7.532 / 11.113 | 10.458× | 17.268× | 0/14 |
| 20 | 2.105 | 6.610 / 24.967 | 6.883 / 25.228 | 7.276 / 25.300 | 10.479× | 16.422× | 1/14 |
| 40 | 2.800 | 11.161 / 26.487 | 11.415 / 26.694 | 11.663 / 26.883 | 9.158× | 13.150× | 3/14 |

Larger blocks increased throughput and latency together. These short, warming
runs are not evidence of unlimited scaling or stable tail latency. Raw payload
offers are already included in application offers; VM `eth0` TX includes control
and management traffic and is summed once, **never TX+RX**. None of these ratios
is an isolated D28 UDP/IP-wire amplification measurement.

### Polling and bounded receive retention: retained negative results

A separate matched 10-MiB/cap-10 experiment changed **only polling cadence** from
25 to 5 ms (`balanced` versus `fast_poll`), leaving status cadence and consensus
timers unchanged. Across two trials per arm, throughput was 1.646 versus
1.714 MiB/s, but all-receipt p95 worsened from 8.952 to 9.883 seconds and CPU cost
rose from 13.901 to 14.754 CPU-seconds/block. This small observed difference does
not establish a general speedup; `balanced` remained selected.

Runtime commit `ec333b702` retains bounded receive surplus for later service
rather than discarding it at each tick's work limit. Fairness/overflow tests cover
the new queue. The warmed comparison uses **14 warmup plus 14 measured 40-MiB
blocks**, one trial per arm, with the same four CPUs, 8 GiB, one executor worker,
100-KiB shards, 40-MiB cap and `balanced`/`wan` settings.

| Runtime | Chain MiB/s | First receipt p50/p95, s | All receipt p50/p95, s | App offers/body | VM TX/body | Scheduler discarded messages/bytes | Nonzero-round heights |
|---|---:|---:|---:|---:|---:|---:|---:|
| Old drop-surplus control | 2.905 | 10.034 / 31.999 | 11.378 / 33.559 | 7.524× | 9.240× | 860 / 13,688,388 | 2/14 |
| Bounded receive retention | 2.633 | 10.616 / 35.520 | 11.479 / 38.735 | 8.250× | 10.214× | 0 / 0 | 3/14 |

**No demonstrated speedup:** the candidate was 9.34% slower overall. The eleven
same-height observations that stayed in round zero in both runs totaled
120.066245 seconds before and 118.877741 after. The other three heights added
21.046495 seconds, accounting for the net slowdown. This localizes the difference
to round-changing heights but does **not identify which phase expired** or prove
that the queue change caused a regression. Queue occupancy/high-water/residence
counters were not captured; missing retention telemetry is not zero. Two measured
proposer rotations and final bounded STP telemetry tails do not certify steady
state or whole-run network utilization.

### Short timers and unavailable proposers

Four separate, sequential eight-height trials use the receive-retention runtime,
10-MiB body/cap and one warmup. `wan` has proposal/prevote/precommit bases of
10/5/7 seconds; benchmark-only `wan_fast` uses 2.5/1.5/2 seconds. Request timeout
remains 10 seconds and status TTL 15 seconds. Offline identities 5/SIN and 6/SYD
stay in the seven-member roster; this is absence, not an adversarial-node test.

| Online / timers | Chain MiB/s | First receipt p50/p95, s | All-online receipt p50/p95, s | App offers/body | VM TX/body | Final round indices, measured heights 2–8 |
|---|---:|---:|---:|---:|---:|---|
| 7 / wan | 1.666 | 3.985 / 8.022 | 6.351 / 8.692 | 8.685× | 13.757× | 0,0,0,0,0,0,0 |
| 7 / wan_fast | 1.131 | 7.620 / 16.273 | 7.785 / 17.001 | 15.042× | 23.460× | 1,0,1,2,1,0,1 |
| 5 / wan | 0.796 | 7.843 / 39.654 | 8.087 / 39.858 | 5.674× | 9.188× | 0,0,0,2,1,0,0 |
| 5 / wan_fast | 1.045 | 7.893 / 22.590 | 8.110 / 22.783 | 7.469× | 12.094× | 1,1,3,2,1,0,0 |

All four trials completed. Shorter timers reduced healthy throughput by 32.14%
and increased measured VM TX by 70.53%. With two offline validators, throughput
rose 31.34% but TX also rose 31.64%; the median did not improve. The missing
round-zero proposer heights 5/6 changed from 39.858/20.359-second cycles to
14.324/7.680 seconds, while another height worsened from 8.922 to 22.783 seconds
and became the worst stall. This is **not a recommended production default**.
Measured and hold windows had zero receive-budget discards and UDP6 receive-buffer
errors; nonzero UDP6 send-buffer errors remain recorded. Native lifetime counters
in final telemetry tails are not whole-measurement deltas.

### Transport-only diagnostic, not chain capacity

A separately bounded, unmodified pinned STP 4.0.0 sender on IAD sent 8192 messages
of 100 KiB to GRU. The receiver confirmed **8187/8192, 799.5117 MiB**, zero corrupt;
five messages were missing and the client correctly exited 1. Five FEC decode
timeouts were also recorded, but retained counters do not prove matching IDs or
CPU-saturated decoding. The server's outer timeout exit 124 was expected.

Verified goodput was **21.3284 MiB/s** over first submission to mapped last
verified receive (37.485734 s). Including terminal idle/status waiting to the
actual 40.297394-second stop gives **19.8403 MiB/s**. These denominators differ;
there is no per-second receiver-delivery series. After startup ramp and a loss
backoff, the sender had sixteen continuously fed seconds at the configured
**32,000-pps cap**, averaging 39.9485 MiB/s of attempted UDP payload. That is
neither delivered throughput nor NIC saturation. Terminal depletion followed.

D28 uses native queue bounds of 512 KiB–8 MiB and no explicit packet-rate cap;
the diagnostic uses 4–64 MiB and 32,000 pps, one association and no consensus.
D28 also distributes each body to six remote validators. Therefore the ratio of
single-path goodput to chain throughput is **not a same-factor headroom claim**.
The diagnostic's combined endpoint interface TX was 1,034,802,368 bytes. Both
endpoints had zero UDP6 send/receive-buffer error deltas during this diagnostic;
that does not erase send errors observed in the separate D28 trials.

Artifacts for this entire worldwide campaign are under
`/var/tmp/d28-wan-speed-20261127-2256/`. Earlier-stage results above have independent
raw-data checks in `summary-check-567.json` (22 unique trials/154 node reports),
`pair-audit-555.json`, `timing-audit-562.json` (four trials/24 reports/20 remote
originals), and `headroom-audit-564.json`. Their input hashes were reverified by the
parent. Sequential stage order, host replacement during CPU resizing, small
samples, all-online barriers and RAM-backed durability remain limitations.

### Final large-block deadline experiment

Benchmark source `5be09b14c` adds opt-in **`wan_bulk`**. Relative to `wan`, only the
proposal base increases, **10 to 20 seconds**. Prevote/precommit stay 5/7 seconds,
request timeout 10 seconds, status TTL 15 seconds; round increments and all
production defaults remain unchanged. The control is the **same receive-retention
runtime**, not the earlier drop-surplus implementation. Both arms use seven
four-CPU/8-GiB machines, one executor worker, 40-MiB bodies/cap, 100-KiB shards,
`balanced` scheduling, fourteen warmups and fourteen measured heights. One
sequential trial per arm completed; no failed durations were filtered away.

| Metric | `wan`, proposal 10 s | `wan_bulk`, proposal 20 s |
|---|---:|---:|
| Chain throughput, MiB/s | 2.633 | **3.512** |
| First receipt p50 / p95, s | 10.616 / 35.520 | **10.222 / 12.335** |
| Quorum receipt p50 / p95, s | 10.911 / 35.816 | **10.517 / 12.629** |
| All-online receipt p50 / p95, s | 11.479 / 38.735 | **11.032 / 15.421** |
| App offers / chain bytes | 8.250× | 7.152× |
| Raw payload offers / chain bytes | 8.127× | 7.061× |
| Measured VM TX / chain bytes | 10.214× | 8.717× |
| Process CPU-seconds / block, seven nodes summed | 48.268 | 38.494 |
| Image replacement GiB / block, seven nodes summed | 19.647 | 14.967 |
| Peak single-process RSS, MiB | 636.605 | 565.727 |
| Nonzero-round measured heights | 3/14 | **0/14** |

Throughput increased **33.37%** in this comparison. The measured numerator is
587,202,560 bytes in each arm; rate durations are 212.647615080 and 159.441476360
seconds. Of the approximately 53.21-second reduction, 50.73 seconds occurs in the
three heights that previously changed round. The other eleven round-zero heights
improve by only 2.47 seconds in total. This supports the overly-short large-block
proposal-deadline hypothesis, not a claim that every transfer became 33% faster.
The first warmup height still changed round. Measured rotation rates are 3.596
and 3.433 MiB/s; two rotations do not establish steady state. The observed p95 is
still the maximum of fourteen heights, not a reliable production tail estimate.

This is the highest complete arm observed in this campaign, **not proof of an
optimal setting or unlimited throughput scalability**. A longer deadline also
waits longer for a genuinely unavailable proposer; the 40-MiB/two-offline case
was not run. It is not a new production timer recommendation. Shortening all
timers globally had the opposite healthy-network effect in the separate tests.

Remaining costs are visible even in this faster arm:

- **72,770 UDP6 send-buffer errors**, versus 78,625 in the matched control, spread
  across the seven machines. This is local send pressure, not attribution of the
  principal bottleneck. Zero scheduler discards, exposed transport overflow/drop
  counters and UDP receive errors do **not** mean zero packet loss or errors.
- **14.97 GiB of full-image replacement work per 40-MiB block**, still RAM-backed.
  Inclusive image checksumming takes 7.229 seconds/block summed across validators
  (about 1.033 seconds per validator); image encoding is 8.266 seconds/block and
  includes checksum work. These spans must not be added as independent costs.
- Application offers are 7.152× unique body bytes versus the six-copy replication
  minimum; this is about 19.2% above that payload minimum, including metadata.
  VM TX is 8.717×, not an isolated STP-wire measurement or a NIC-utilization proof.
- The all-online benchmark barrier, proposer fanout, repeated small-message work
  and bounded final-only native telemetry prevent a pure-bandwidth-limit claim.
  There is no full-run per-link capacity or phase-critical-path measurement.

### Final verification, reproduction and shutdown

The final executable SHA-256 is
`41162b623f461ee62976efccb0f5e5aba1c04c486f5191b84d904c5aea945fbb`;
runner SHA-256 is
`4c55a0d177fcc78e75f847dde71579c18d0e039452155ddf9bd05d218f7232f0`.
The integrated release suite passes **414 Rust tests**, with five existing ignored
tests and unbounded legacy `single_rt` excluded. The non-benchmark library check,
release benchmark build, **50 Python checks** including real-process tests, and
seven-process 40-MiB `wan_bulk` gate pass. The first full-suite attempt hit an
existing localhost-port reservation/wildcard-bind collision; its log is retained.
The focused unchanged test and entire unchanged suite then passed. This is not a
hidden D28 trial failure (`phase4-initial-suite-failure.json`). No physical fsync
or new full-Zebra integration certification is implied.

The final on-node0 invocation, after preparing matching private workers with the
same executable and 4-GiB per-node guard, was:

```sh
python3 -B /app/d28_fly-phase4.py campaign \
  --roster /data/roster-phase4.json --token-file /run/d28-token \
  --binary /app/d28_bench-phase4 --out /data/cpu4-warmed-bulk \
  --body-bytes 41943040 --max-body 41943040 \
  --payload-modes merkle --payload-leaf-bytes 102400 \
  --profiles balanced --timing-profile wan_bulk --workers 1 \
  --repetitions 1 --heights 14 --warmup 14 --seed 428 \
  --timeout-secs 900 --chatter-secs 2 --lifetime-secs 1000 \
  --max-trial-tx-bytes 4294967296 --max-campaign-tx-bytes 22000000000
```

A fresh deployment and new output directories are required to repeat it; the
original temporary app no longer exists. Exact prospective designs, admissions,
source/binary pins and inventories remain in `bulk-timeout-design.json` and
`phase4-*.json`. Raw final reports are in `final-iad/cpu4-warmed-bulk/`; remote
originals are in `final-{sjc,gru,fra,jnb,sin,syd}/phase4-worker/`. Complete final VM
artifact archives are `final-*.tar.gz`, pinned by `final-archives-verified.json`.

The independent final audit verified **27 unique D28 trials and 185 node reports**,
all complete; both warmed arms' seven-node/28-height agreement; seven final archives
and all **3,279 regular members**; and **3,398 unchanged input hashes**. Its primary
receipt is `bulk-audit-580.json`, SHA-256
`4c3814108a676a4b6f8fa80eb44280516e7341b97f72d01a9417d5dfde237cd7`;
`bulk-audit-580-inputmanifest.json` and `bulk-audit-580-supplement.json` retain the
input set and phase/hold limitations. This is an artifact/accounting check, not
an additional performance repetition or security/durability certification.

**The temporary `d28-speed-20261127-2256` application and all seven machines were
deleted**, after artifact hashes and benchmark-child cleanup were verified. A
successful Fly application inventory confirmed absence; no public services or
persistent volumes were created. `cleanup-receipt.json`, `pre-delete-validation.json`
and `fly-destroy.log` retain the evidence. Accounted D28 whole-child plus separate
transport-diagnostic TX is **89,361,636,231 bytes**; it includes warmup/hold/shutdown,
is not the measured-rate numerator, and omits some setup/upload/archive/idle
traffic. It is **not a Fly bill**. The revised 100-GB soft experiment allowance and
original machine deadline are recorded in the prospective design. All benchmark
processes are stopped; unrelated repository files were left untouched.

## Cached durable-body processing — 2026-11-28

Implementation `678de7bbe`, with migration documentation `1c4d71d2a`, optimizes
local preparation of the private D28 atomic image. Version 2 puts metadata and
its checksum before three fixed body regions, caching each unchanged region's
length and digest. Metadata-only commits no longer recopy, rehash or clear those
body regions. The image size, `AtomicBlockStore` replacement API and required
signing/publication barriers are unchanged. Cold recovery verifies actual body
bytes and canonical padding before trusting the cache. Version 1 opens without a
write and migrates at the next required replacement; **older executables reject
version 2**. This is not downgrade compatibility. PoW/ledger encodings, exported
BFT signatures, consensus rules and production timers are unchanged.

This comparison is **localhost, not a new Fly or worldwide result**. Seven
independent validator processes share one host, with one executor worker each,
real STP/signatures/Merkle proofs, mock bodies and RAM-backed atomic replacement.
The controller still requires **all seven validators to progress**. Body caps
match the 10- or 40-MiB body size; both use 100-KiB shards, `balanced` scheduling
and `wan_bulk` timing. Each trial has seven warmup and seven measured heights.
No builds or test suites ran during measurements; unrelated host activity was
not excluded.

The prospective order was one old-version trial at each size, implementation
and verification, two new-version trials at each size, then another old-version
trial at each size. All eight primary trials completed, with **56/56 measured
heights in round zero** and zero receive-budget discards. A separate seven-node
40-MiB gate (one warmup, two measured heights) also passed and is excluded below.
Rates pool unique chain bytes over controller measured durations; latency
percentiles pool fourteen observations per arm. P95 is the observed maximum,
not an estimate of stable production tails.

| Metric | Old, 10 MiB | New, 10 MiB | Old, 40 MiB | New, 40 MiB |
|---|---:|---:|---:|---:|
| Chain throughput, MiB/s | 4.260 | 4.619 | 4.009 | **4.855** |
| First receipt p50 / p95, s | 2.205 / 2.535 | 1.961 / 2.168 | 9.028 / 10.552 | 7.630 / 8.870 |
| All-online receipt p50 / p95, s | 2.264 / 2.655 | 2.104 / 2.514 | 9.461 / 10.908 | 8.067 / 9.297 |
| Process CPU-seconds/block, seven nodes summed | 11.293 | 9.641 | 55.184 | **40.892** |
| Full-image replacement GiB/block, seven nodes summed | 2.087 | 2.064 | 12.607 | 11.719 |
| Application offers / chain bytes | 6.069× | 6.068× | 6.064× | 6.064× |
| Peak single-process RSS, MiB | 149.777 | 148.734 | 487.473 | 490.738 |

The observed pooled 40-MiB throughput improvement is **21.10%**, with **25.90%
less process CPU time/block**. However, the old-version before/after rates were
4.301/3.754 MiB/s, while the two candidate trials were 5.066/4.661 MiB/s. Both
candidate trials beat both old trials, but baseline drift and this small,
nonrandomized sample prevent a precise causal speedup estimate.

At 10 MiB, the pooled improvement is 8.43%, but old-version before/after rates
were 4.651/3.930 MiB/s and candidate rates were 4.800/4.452 MiB/s. The pooled
candidate is 0.68% slower than the earlier baseline, and its observed all-receipt
p95 is 11.06% worse than that earlier baseline (2.514 versus 2.264 seconds).
**A robust 10-MiB throughput improvement is not established**; the reduction in
encoder work is much clearer.

### Local work removed, and work that remains

The following are measured wall-span seconds/block **summed over seven nodes**.
Checksum is contained within encoding; spans can overlap across nodes and nest
inside runtime phases. They are neither additive CPU costs nor critical-path
latency measurements.

| Durable phase | Old, 10 MiB | New, 10 MiB | Old, 40 MiB | New, 40 MiB |
|---|---:|---:|---:|---:|
| Image encoding, including checksums | 2.110 | 0.087 | 13.818 | 0.426 |
| Checksum subset | 1.646 | 0.045 | 10.559 | 0.213 |
| Backend full-image replacement | 0.793 | 0.992 | 5.710 | 6.424 |

Encoding spans fell about 96–97%, and checksum spans about 97–98%. New encoder
counters report exactly **70 MiB per 10-MiB block** and **280 MiB per 40-MiB
block** of body copying, hashing and zeroing, each separately summed over seven
nodes. Those counters exclude staging copies, initial allocation, recovery and
backend traffic; they do not measure all copies in the process. The old binary
does not export these counters, so its missing entries are not zeros.

The fixed regression also verifies forty metadata/countdown/vote fences with
zero additional body-copy/hash/zero bytes, 120 reused body slots and exactly
`40*(M-32)` metadata bytes hashed, where `M = 4096 + 556*Nmax`.

**Full-image backend replacement is still expensive and has not been removed.**
The candidate 40-MiB run still replaces 11.72 GiB/block across seven nodes, with
6.42 summed wall seconds/block inside the memory-backed replacement calls. Its
per-replacement image is unchanged; total volume varies with checkpoint count.
This experiment demonstrates less local encoding work, not lower physical disk
write volume, worldwide throughput, network saturation or full Zcash validation.
Application offers are not STP/UDP/IP wire bytes. Host network/UDP counters are
shared on localhost and are not summed as independent per-validator traffic.

### Verification and reproduction

The integrated release suite passed **422 Rust tests**, with five existing
ignored tests and unbounded legacy `single_rt` excluded. The non-benchmark
library check, release executable build, **50 Python checks** including real
process tests, and **four Zebra D28 adapter checks** also passed. Migration,
old/new replacement failures, cache invalidation, canonical corruption and
metric export are covered; physical power-loss/fsync behavior is not certified.

Baseline source is `ba3791d27`; baseline executable SHA-256 is
`41162b623f461ee62976efccb0f5e5aba1c04c486f5191b84d904c5aea945fbb`.
Candidate executable SHA-256 is
`f069bad208630fd9bdc0f33c55a59f36af20881a68656fb667f029de6ea2d843`.
The unchanged runner SHA-256 is
`4c55a0d177fcc78e75f847dde71579c18d0e039452155ddf9bd05d218f7232f0`.

After building/freezing the desired executable, a two-trial candidate arm is:

```sh
python3 -B tenderlink/benchmarks/d28_fly.py local \
  --binary /absolute/path/to/frozen/d28_bench --out /absolute/path/to/NEW/results \
  --nodes 7 --body-bytes 41943040 --max-body 41943040 \
  --payload-modes merkle --payload-leaf-bytes 102400 \
  --profiles balanced --timing-profile wan_bulk --workers 1 \
  --repetitions 2 --heights 7 --warmup 7 --seed 428 \
  --timeout-secs 600 --chatter-secs 2 --lifetime-secs 1800
```

Use 10485760 for both body and cap in the 10-MiB arm. Preserve the full
before/candidate/after order for a matched comparison, not just the fastest
trial. Exact commands, prospective plan, raw reports, source/binary pins,
verification logs and aggregate calculations are retained under
`/var/tmp/d28-local-processing-20261128/`, notably `plan.json`,
`candidate-measurement-plan.json`, `candidate-preflight.json`,
`comparison-analysis.json` and `comparison-aggregates.json`.

The independent raw-boundary audit passed **38,156 checks with zero errors**:
nine trials including the gate, 63 node reports, 54 retained worker-original
report copies and 805 node-height decisions. All **1,060 physical input hashes
and 21 Git-blob hashes** were reverified unchanged by the parent, along with the
audit script. `audit-local-v2.json`, `audit-local-v2-inputs.json` and
`audit-parent-verification.json` retain the findings, limitations and input set.
The audit receipt SHA-256 is
`265f1a019b1a1ef7b332030afaee3d6984881db7c0824970533dea638f713921`.
This is an artifact/accounting audit, not a new benchmark repetition or a fresh
execution of the correctness suite.

All seven campaign cleanup receipts confirm stopped workers and removed tokens;
a subsequent owned-process check found no remaining benchmark children.
`cleanup-receipt.json` retains this evidence. **No Fly resources were created.**

## Split atomic body storage — 2026-11-28

Implementation and integration are committed through `62ffe8973`, building on
cached-body baseline `2d9d555df`. The next experiment removes full-image backend
replacement when only small consensus metadata changes. It retains the same
bounded logical v2 image, three body slots, consensus rules, required durable
signing/publication barriers, Host checks, and all-online progress requirement.
PoW/ledger encodings, exported BFT signatures, transport and production timers
are unchanged.

### What is split, and what remains atomic

`AtomicBlockStore::replace_regions` takes the complete next logical image and
an ordered set of changed regions. Backends commit **all selected regions as
one old-or-new transaction**; independent per-region writes are not sufficient.
Existing backends can safely fall back to whole-image replacement. The durable
adapter selects metadata plus changed complete body slots before encoding
updates its cache. First initialization and v1 conversion select everything.

The real Zebra adapter stores metadata and three bodies under four fixed,
BFT-private RocksDB keys and uses **one synced WriteBatch**. For seven members,
metadata is `M = 4096 + 556*7 = 7,988` bytes. With a 40-MiB body cap, a normal
metadata-only update no longer submits the entire **125,837,108-byte** logical
image. It submits metadata; a changed body adds one full 40-MiB slot, including
padding. Zebra additionally writes the metadata root's 40-byte identity envelope.

Existing monolithic v1/v2 storage loads without a write. Its first required
split commit establishes all three body rows, even when a v2 encoder cache says
only metadata changed. Snapshot reads reject missing, mixed, orphaned, oversized,
wrong-identity and corrupt state; value lengths are checked before application
copies, and presence checks do not copy body bytes. The `D28KEY02` envelope makes
older executables fail closed, rather than recover stale state or bootstrap
fresh. **This private storage migration is not downgrade-compatible.**

The benchmark uses the same core/codec/barriers with a RAM backend. It now copies
selected regions and counts one successful batch, not one transaction per region.
The new byte metric is **actual backend RAM-copy volume**, not the former
whole-image-equivalent count, individual changed-byte count, or physical disk
traffic. RocksDB WAL/SST/compaction/device amplification is not measured.

### Matched seven-process results

Seven independent local validator processes, real STP/signatures/Merkle proofs,
100-KiB shards, mock bodies and RAM-backed persistence. Body caps match the 10-
or 40-MiB body size; both use `balanced`, `wan_bulk`, and one executor worker.
Every height still waits for **all seven validators**. No builds or test suites
from this experiment ran during measurements; unrelated shared-host activity,
including other projects' builds, was not controlled or excluded.

After all verification and a separate 40-MiB gate, the prospective primary order
was old/new/new/old at 10 MiB, then new/old/old/new at 40 MiB. Each invocation had
seven warmup and seven measured heights. All eight primary trials completed:
**56/56 measured heights, all round zero, zero receive-budget discards**. The gate
(one warmup, two measured heights) passed and is excluded from pooled comparisons.

| Metric | Old, 10 MiB | Split, 10 MiB | Old, 40 MiB | Split, 40 MiB |
|---|---:|---:|---:|---:|
| Chain throughput, MiB/s | 4.750 | **5.415** | 5.148 | **5.749** |
| First receipt p50 / p95, s | 1.924 / 2.316 | 1.747 / 1.811 | 7.426 / 7.844 | 6.588 / 7.367 |
| All-online receipt p50 / p95, s | 2.023 / 2.497 | 1.840 / 1.983 | 7.721 / 8.178 | 6.856 / 7.535 |
| Process CPU-seconds/block, seven nodes summed | 9.464 | **7.648** | 38.652 | **31.281** |
| Backend RAM-copy GiB/block, seven nodes summed | 2.022 | **0.137** | 11.401 | **0.548** |
| Atomic batches/block, seven nodes summed | 69.000 | 68.143 | 97.286 | 97.286 |
| Application offers / unique chain bytes | 6.066× | 6.065× | 6.062× | 6.061× |
| Peak single-process RSS, MiB | 149.418 | 154.719 | 489.777 | 486.309 |

Observed pooled throughput rose **14.00% at 10 MiB** and **11.67% at 40 MiB**.
CPU work per block fell **19.19%** and **19.07%**, respectively. Backend copy
volume fell **93.21%** and **95.20%**. The 40-MiB arms have exactly the same pooled
batch count: the reduction is not from omitting required checkpoints.

Per-trial throughput, in measurement order:

| Body | Old/new order | Throughput, MiB/s |
|---|---|---|
| 10 MiB | old / split / split / old | 5.061 / 5.357 / 5.474 / 4.474 |
| 40 MiB | split / old / old / split | 5.616 / 5.203 / 5.095 / 5.888 |

Both split trials beat both baseline trials at each size. Nevertheless, baseline
movement and the small sequential sample prevent treating the percentage gains
as precise, repeatable causal estimates. Throughput pools unique chain bytes over
controller durations; latency pools fourteen measured heights per arm. Nearest-
rank p95 is their maximum, not a stable production-tail estimate. RSS is a
single-process lifetime high-water mark and can include warmup.

### Where the saved work went

Wall-span seconds/block below are **summed over seven processes**. Checksum is
inside encoding; spans can overlap across nodes and nest inside runtime phases.
They are not additive CPU costs or critical-path latency measurements.

| Durable phase | Old, 10 MiB | Split, 10 MiB | Old, 40 MiB | Split, 40 MiB |
|---|---:|---:|---:|---:|
| Image encoding, including checksums | 0.097 | 0.072 | 0.431 | 0.458 |
| Checksum subset | 0.048 | 0.041 | 0.206 | 0.228 |
| Backend replacement | 1.138 | **0.043** | 6.726 | **0.363** |

Backend replacement spans fell **96.19%** and **94.61%**. Encoding was already
cached in the baseline and did not improve consistently; its 40-MiB span grew
slightly. Both versions still report seven body-lengths copied, hashed and zeroed
by the encoder per unique block, each separately summed across seven validators.
The split backend copies fourteen full body slots per unique block in these runs
(new bodies and cleared slots), plus metadata. This is about **140 MiB** of body
regions per 10-MiB block or **560 MiB** per 40-MiB block, not zero body work.

The targeted backend cost is now small, but total 40-MiB CPU work remains about
31.3 CPU-seconds/block across seven processes. Candidate body-check, Host-schedule
and payload-receive spans remain substantial (4.25, 8.77 and 4.66 summed wall
seconds/block); those scopes overlap and do not identify an additive remaining
budget or prove a new principal bottleneck. This experiment does not establish
network saturation, worldwide performance, physical-disk durability performance
or full Zcash transaction-validation throughput. Application offers are not
STP/UDP/IP wire bytes.

### Verification, provenance and reproduction

**433 Rust tests, 50 Python checks, ten Zebra D28 adapter tests and one seeded
legacy-WAL property test passed**, along with the non-benchmark library check and
release build. Five existing ignored Rust tests and unbounded legacy `single_rt`
remain excluded. Tests cover all eight dirty-body masks under before-error and
commit-then-error outcomes, poisoning/reload, first initialization, v1/v2 lazy
migration, padding, same-length/copy/shrink/clear updates, forty metadata-only
fences, malformed ranges, row-operation counts and concurrent RocksDB snapshots.
The region-mask fixture is an omitted-byte equivalence oracle; actual partial
copies are separately exercised by MemoryBackend tests. Native process-kill,
physical power-loss and injected RocksDB I/O failures were **not** tested.

Baseline binary SHA-256:
`f069bad208630fd9bdc0f33c55a59f36af20881a68656fb667f029de6ea2d843`.
Candidate binary SHA-256:
`7ed611b4d22d7c1efe122f84d2d367df09f5be8013d07446833f175f3f86fcc4`.
The runner is unchanged:
`4c55a0d177fcc78e75f847dde71579c18d0e039452155ddf9bd05d218f7232f0`.

For each primary invocation, use the earlier reproduction command with
`--repetitions 1 --heights 7 --warmup 7 --timeout-secs 600 --lifetime-secs 900`,
the selected frozen binary, and a new output directory. Preserve the complete
old/new ordering above rather than selecting only the best runs. The gate instead
uses `--heights 2 --warmup 1` and stays outside primary pooling. Group by explicit
binary arm and body size: the runner's generic workload key omits executable
identity and would otherwise mix old/new results.

An independent raw-boundary audit passed **112,501 checks with zero errors**:
nine campaigns, 63 node reports, 805 node-height records and 54 retained
worker-original report copies. It independently reproduced all trial/cohort
figures and relative changes. The parent reverified **649 input-file hashes and
ten Git-blob hashes**, plus the audit script and receipts. `audit-split.py`,
`audit-split.json`, `audit-split-inputs.json` and `audit-parent-verification.json`
retain the checks and coverage. The audit receipt SHA-256 is
`69853969a0aa679156e3d1bb56dd567ada7f53cce3a839a6bfa2bda22591033c`.
This is an artifact/accounting audit, not another benchmark or correctness-test
run. Raw reports contain decision IDs and rounds, not certificate bytes; matching
them does not constitute fresh offline cryptographic certificate verification.

Raw reports, commands, source/binary pins, prospective plans, verification logs
and calculations are retained in `/var/tmp/d28-split-storage-20261128/`:
`measurement-plan.json`, `candidate-preflight.json`, `source-review.json`,
`comparison-analysis.json`, `comparison-aggregates.json`,
`measurements-completed.json` and `cleanup-receipt.json`. All nine campaign
cleanup receipts confirm stopped workers, stopped HTTP threads and removed
tokens; an exact frozen-binary process check found no remaining benchmark
children. **No Fly resources were created.**
