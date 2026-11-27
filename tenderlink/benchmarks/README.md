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
