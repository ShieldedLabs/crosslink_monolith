//! Component-only mock-chain benchmark using the real D28 runtime, crypto,
//! codec, durable-image adapter and localhost STP. No PoW or ledger is run.
//!
//! The in-memory backend is NOT crash durable. Its optional blocking delay is
//! artificial, not an fsync model. The host's all-online-node ACK barrier is a
//! benchmark model limitation: it prevents trusted INIT from retiring evidence
//! before every configured online node independently exports the same decision.
//! Offline identities remain in the full equal-stake roster. No undecided body
//! or certificate is injected into another node's host.
use super::{
    atomic::AtomicBlockStore,
    durable,
    runtime::{self, AccessD28, HostCallbacks, RuntimeConfig},
};
use crate::{
    BlockValue, ClosureToProposeNewBlock, ClosureToPushDecidedBlock, ClosureToReadDecidedTip,
    ClosureToUpdatePeers, ClosureToValidateProposedBlock, HostTip, STPAddress, SortedRosterMember,
    TMStatus, TMStatusReason, ValueId,
    transport::{Consumer, ConsumerHandle, SharedTransport, TransportStats},
};
use ed25519_zebra::{SigningKey, VerificationKeyBytes};
use serde::Serialize;
use std::{
    collections::BTreeMap,
    io,
    net::{Ipv6Addr, UdpSocket},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use zcash_primitives::{
    bft::{BftBlock, FatPointerToBftBlock, HashKeys, PubKeyID},
    block::{BlockHash, BlockHeader, BlockHeaderData},
};

pub mod distributed;

const NAMESPACE: [u8; 32] = [0x28; 32];

/// Opt-in benchmark geometry only; production `PROPOSAL_MAX_SIZE` stays 1 MiB.
pub const BENCH_MAX_BODY: usize = 10 * 1024 * 1024;

/// Admit a bounded complete evidence frame, not just its body. Keep the wrapper's
/// existing defaults for legacy 1 MiB runs; no production transport limit changes.
fn bench_transport_limits(nodes: usize, max_body: usize) -> io::Result<crate::transport::TransportLimits> {
    if !(1..=128).contains(&nodes) || !(1..=BENCH_MAX_BODY).contains(&max_body) {
        return Err(invalid("invalid benchmark transport geometry"));
    }
    let frame = super::wire::Limits { max_members: nodes, max_body }
        .max_frame_bytes()
        .map_err(|_| invalid("invalid benchmark frame bound"))?;
    let mut limits = crate::transport::TransportLimits::default();
    if max_body > crate::PROPOSAL_MAX_SIZE {
        // Completed-message slots are count-bounded, not byte-bounded. Avoid
        // allowing 4096 large frames in the native ring on a 2 GiB test VM.
        limits.rings.receive = 32;
    }
    limits.max_message_size = limits.max_message_size.max(frame);
    // Leave control-message slack beyond a full frame (16 MiB for 10 MiB bodies).
    let queue = frame.next_power_of_two();
    limits.peer_outbox_bytes = limits.peer_outbox_bytes.max(queue);
    limits.consumer_inbox_peer_bytes = limits.consumer_inbox_peer_bytes.max(queue);
    limits.consumer_inbox_bytes = limits.consumer_inbox_bytes.max(queue);
    // STP permits one oversized-to-target message in an empty send queue. Keep
    // its 8 MiB backlog target and 512 KiB high-water gating unchanged.
    Ok(limits)
}

/// Finite work and allocation limits; `heights` excludes `warmup` heights.
#[derive(Clone, Debug, Serialize)]
pub struct BenchConfig {
    pub nodes: usize,
    pub offline: usize,
    pub heights: u32,
    pub warmup: u32,
    pub headers: usize,
    pub timeout_secs: u64,
    pub workers: usize,
    pub host_delay_ms: u64,
    pub store_delay_ms: u64,
    /// Explicit override only: default is the production runtime's 1 MiB.
    pub max_body: usize,
    pub chatter_secs: u64,
}
impl Default for BenchConfig {
    fn default() -> Self {
        Self {
            nodes: 4,
            offline: 0,
            heights: 30,
            warmup: 3,
            headers: 32,
            timeout_secs: 180,
            workers: 4,
            host_delay_ms: 0,
            store_delay_ms: 0,
            max_body: crate::PROPOSAL_MAX_SIZE,
            chatter_secs: 0,
        }
    }
}
impl BenchConfig {
    pub fn validate(&self) -> io::Result<()> {
        if !(1..=128).contains(&self.nodes)
            || self.offline >= self.nodes
            || 3 * (self.nodes - self.offline) <= 2 * self.nodes
        {
            return Err(invalid(
                "nodes must be 1..128 and online stake must strictly exceed two thirds of full roster",
            ));
        }
        if self.heights == 0
            || self
                .heights
                .checked_add(self.warmup)
                .is_none_or(|h| h > 10_000)
            || !(1..=86_400).contains(&self.timeout_secs)
            || !(1..=128).contains(&self.workers)
            || self.host_delay_ms > 60_000
            || self.store_delay_ms > 60_000
            || self.chatter_secs > 3_600
            || !(1..=BENCH_MAX_BODY).contains(&self.max_body)
            || self.headers > 2_048
        {
            return Err(invalid(
                "invalid benchmark bounds: total heights 1..10000, timeout 1..86400s, workers 1..128, delays <=60000ms, chatter <=3600s, max-body 1..10MiB, headers <=2048",
            ));
        }
        // Reserve a full-N parent certificate, not just the likely minimum quorum.
        let worst = mock_body(
            2,
            FatPointerToBftBlock::from_parts(
                zcash_primitives::bft::Blake3Hash([1; 32]),
                1,
                0,
                &vec![
                    zcash_primitives::bft::FatPointerSignature {
                        pub_key: PubKeyID([1; 32]),
                        vote_signature: [0; 64]
                    };
                    self.nodes
                ],
            ),
            self.headers,
        )?;
        if worst.0.len() > self.max_body {
            return Err(invalid(
                "headers plus maximum parent certificate exceed max-body",
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq)]
pub struct Percentiles {
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
}
/// Nearest-rank percentiles; no samples is represented as null, never zero.
pub fn percentiles(samples: &[f64]) -> Option<Percentiles> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let at = |p: usize| sorted[(sorted.len() * p).div_ceil(100) - 1];
    Some(Percentiles {
        p50: at(50),
        p95: at(95),
        p99: at(99),
    })
}
fn latency_summary(heights: &[HeightReport]) -> (LatencySummary, BTreeMap<String, usize>) {
    let samples = |get: fn(&HeightReport) -> Option<f64>| {
        heights
            .iter()
            .filter(|h| h.measured)
            .filter_map(get)
            .collect::<Vec<_>>()
    };
    let first = samples(|h| h.first_latency_ms);
    let quorum = samples(|h| h.quorum_latency_ms);
    let all = samples(|h| h.all_latency_ms);
    (
        LatencySummary {
            first: percentiles(&first),
            quorum: percentiles(&quorum),
            all: percentiles(&all),
        },
        BTreeMap::from([
            ("first".into(), first.len()),
            ("quorum".into(), quorum.len()),
            ("all".into(), all.len()),
        ]),
    )
}
#[derive(Clone, Debug, Default, Serialize)]
pub struct LatencySummary {
    pub first: Option<Percentiles>,
    pub quorum: Option<Percentiles>,
    pub all: Option<Percentiles>,
}
#[derive(Clone, Debug, Serialize)]
pub struct Summary {
    pub measured_heights: usize,
    pub serialized_body_bytes: u64,
    pub synthetic_payload_bytes: u64,
    pub elapsed_seconds: Option<f64>,
    pub committed_heights_per_second: Option<f64>,
    pub committed_body_bytes_per_second: Option<f64>,
    pub committed_payload_bytes_per_second: Option<f64>,
    pub latency_ms: LatencySummary,
    pub latency_samples: BTreeMap<String, usize>,
}
#[derive(Clone, Debug, Serialize)]
pub struct NodeDecision {
    pub node: usize,
    /// Entry to push_decided, before host certificate checks and ACK waiting.
    pub push_decided_at_ms: f64,
    pub latency_ms: f64,
    pub callback_lock_wait_ms: f64,
    pub export_host_check_ms: f64,
    pub certificate_round: u64,
    pub certificate_signatures: usize,
    pub max_observed_round: u64,
    pub rounds_entered: u64,
    pub barrier_wait_ms: Option<f64>,
    /// Time until the callback future actually returns (null at final height).
    pub ack_wait_ms: Option<f64>,
}
#[derive(Clone, Debug, Serialize)]
pub struct ProposalTiming {
    pub node: usize,
    pub requested_at_ms: f64,
    pub body_ready_at_ms: Option<f64>,
}
#[derive(Clone, Debug, Serialize)]
pub struct HeightReport {
    pub height: u64,
    pub measured: bool,
    pub id: Option<String>,
    pub start_at_ms: f64,
    pub first_proposal_at_ms: Option<f64>,
    pub serialized_body_bytes: Option<u64>,
    pub synthetic_payload_bytes: u64,
    pub first_latency_ms: Option<f64>,
    pub quorum_latency_ms: Option<f64>,
    pub all_latency_ms: Option<f64>,
    pub cohort_committed: bool,
    pub proposals: Vec<ProposalTiming>,
    pub nodes: Vec<NodeDecision>,
}
#[derive(Clone, Debug, Serialize)]
pub struct NodeReport {
    pub node: usize,
    pub public_key: String,
    pub online: bool,
    pub measured_decisions: usize,
    pub latency_ms: Option<Percentiles>,
}
#[derive(Clone, Copy, Debug, Default, Serialize, PartialEq, Eq)]
pub struct LoopbackCounters {
    pub tx_bytes: u64,
    pub tx_packets: u64,
}
impl LoopbackCounters {
    fn difference(self, start: Self) -> io::Result<Self> {
        Ok(Self {
            tx_bytes: self
                .tx_bytes
                .checked_sub(start.tx_bytes)
                .ok_or_else(|| invalid("lo byte counter decreased"))?,
            tx_packets: self
                .tx_packets
                .checked_sub(start.tx_packets)
                .ok_or_else(|| invalid("lo packet counter decreased"))?,
        })
    }
}
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
pub struct TcCounters {
    /// tc clsact egress accounting includes the synthetic 14-byte lo L2 header.
    pub pre_impairment_accounted_bytes: u64,
    pub pre_impairment_packets: u64,
    pub netem_drops: u64,
}
impl TcCounters {
    fn difference(self, start: Self) -> Option<Self> {
        Some(Self {
            pre_impairment_accounted_bytes: self
                .pre_impairment_accounted_bytes
                .checked_sub(start.pre_impairment_accounted_bytes)?,
            pre_impairment_packets: self
                .pre_impairment_packets
                .checked_sub(start.pre_impairment_packets)?,
            netem_drops: self.netem_drops.checked_sub(start.netem_drops)?,
        })
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct NetworkWindow {
    pub scope: String,
    pub start: LoopbackCounters,
    pub end: LoopbackCounters,
    pub delta: LoopbackCounters,
    pub tc_start: Option<TcCounters>,
    pub tc_end: Option<TcCounters>,
    pub tc_delta: Option<TcCounters>,
    pub semantics: &'static str,
}
/// Cumulative counters at a serially sampled barrier boundary. Maps retain each
/// underlying counter's exact name; application byte counts are NOT wire bytes.
#[derive(Clone, Debug, Serialize)]
pub struct CounterSnapshot {
    pub at_ms: f64,
    pub sampling_duration_ms: f64,
    pub application: BTreeMap<String, u64>,
    pub transport: BTreeMap<String, u64>,
    pub store_replacements: u64,
    pub store_replacement_bytes: u64,
    pub loopback: Option<LoopbackCounters>,
    pub tc: Option<TcCounters>,
    pub tc_error: Option<String>,
    pub tc_timed_out: bool,
}
#[derive(Clone, Debug, Serialize)]
pub struct CounterWindow {
    pub start: CounterSnapshot,
    pub end: CounterSnapshot,
    pub application_delta: BTreeMap<String, u64>,
    pub transport_delta: BTreeMap<String, u64>,
    pub store_replacements: u64,
    pub store_replacement_bytes: u64,
}
#[derive(Clone, Debug, Serialize)]
pub struct BenchReport {
    pub schema_version: u32,
    pub status: &'static str,
    pub failure: Option<String>,
    pub config: BenchConfig,
    pub quorum_nodes: usize,
    pub online_nodes: Vec<usize>,
    pub offline_nodes: Vec<usize>,
    pub durable_image_bytes_per_node: usize,
    pub total_run_seconds: f64,
    pub summary: Summary,
    pub runtime_settings: BTreeMap<String, u64>,
    pub transport_settings: String,
    pub measured_window: Option<CounterWindow>,
    pub network: Option<NetworkWindow>,
    pub chatter_window: Option<CounterWindow>,
    pub chatter_network: Option<NetworkWindow>,
    pub heights: Vec<HeightReport>,
    pub nodes: Vec<NodeReport>,
    pub methodology: Vec<&'static str>,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}
fn milliseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}
fn seed(kind: &str, index: usize) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"D28 isolated benchmark identities v1");
    hasher.update(kind.as_bytes());
    hasher.update(&(index as u64).to_le_bytes());
    *hasher.finalize().as_bytes()
}
fn key(index: usize) -> SigningKey {
    SigningKey::from(seed("signing", index))
}
fn roster(nodes: usize) -> Vec<SortedRosterMember> {
    (0..nodes)
        .map(|i| SortedRosterMember {
            pub_key: PubKeyID(VerificationKeyBytes::from(&key(i)).into()),
            stake: 1,
            cumulative_stake: i as u64 + 1,
        })
        .collect()
}
fn header(height: u32, index: usize) -> BlockHeaderData {
    let tag = seed("mock-header", index);
    BlockHeaderData {
        version: 4,
        prev_block: BlockHash(tag),
        merkle_root: tag,
        final_sapling_root: [0; 32],
        time: height,
        bits: 0,
        nonce: tag,
        solution: vec![],
        fat_pointer_to_bft_block: FatPointerToBftBlock::null(),
    }
}
fn payload_size(headers: usize) -> io::Result<u64> {
    let mut bytes = Vec::new();
    BlockHeader::write_data(&header(1, 0), &mut bytes)?;
    Ok((bytes.len() * headers) as u64)
}
fn mock_body(height: u32, parent: FatPointerToBftBlock, headers: usize) -> io::Result<BlockValue> {
    Ok(BlockValue(
        BftBlock {
            version: 2,
            height,
            previous_block_fat_ptr: parent,
            headers: (0..headers).map(|i| header(height, i)).collect(),
            slashforks: vec![],
            do_not_include_until_bc_height: 0,
        }
        .zcash_serialize_to_vec()?,
    ))
}
fn verify_pointer(
    pointer: &FatPointerToBftBlock,
    members: &[SortedRosterMember],
    height: u64,
    id: ValueId,
) -> bool {
    let vote = pointer.get_vote_template();
    if vote.height != height || vote.value.0 != id.0 || !vote.typ {
        return false;
    }
    let mut last = None;
    let mut stake = 0;
    for signature in &pointer.signatures {
        let Some(index) = members.iter().position(|m| m.pub_key == signature.pub_key) else {
            return false;
        };
        if last.is_some_and(|old| old >= index) {
            return false;
        }
        last = Some(index);
        stake += members[index].stake;
    }
    stake >= crate::decision_threshold(members.len() as u64, true)
        && pointer.validate_signatures(&NAMESPACE)
}
fn validate_body(
    value: &BlockValue,
    height: u64,
    parent_id: ValueId,
    config: &BenchConfig,
    members: &[SortedRosterMember],
) -> bool {
    if value.0.len() > config.max_body {
        return false;
    }
    let mut bytes = &value.0[..];
    let Ok(block) = BftBlock::zcash_deserialize(&mut bytes) else {
        return false;
    };
    if !bytes.is_empty()
        || block.version != 2
        || u64::from(block.height) != height
        || !block.slashforks.is_empty()
        || block.do_not_include_until_bc_height != 0
        || block.headers.len() != config.headers
        || !block
            .headers
            .iter()
            .enumerate()
            .all(|(i, h)| *h == header(block.height, i))
        || block.zcash_serialize_to_vec().ok().as_deref() != Some(value.0.as_slice())
    {
        return false;
    }
    if height == 1 {
        block.previous_block_fat_ptr == FatPointerToBftBlock::null()
    } else {
        verify_pointer(
            &block.previous_block_fat_ptr,
            members,
            height - 1,
            parent_id,
        )
    }
}

#[derive(Default)]
struct StoreCounts {
    writes: AtomicU64,
    bytes: AtomicU64,
}
/// One allocation reused for every replacement. This deliberately does not
/// implement physical persistence or claim AtomicBlockStore crash durability.
struct MemoryBackend {
    image: Vec<u8>,
    initialized: bool,
    delay: Duration,
    deadline: Instant,
    counts: Arc<StoreCounts>,
}
impl AtomicBlockStore for MemoryBackend {
    fn block_size(&self) -> usize {
        self.image.len()
    }
    fn load(&mut self, block: &mut [u8]) -> super::Result<bool> {
        if block.len() != self.image.len() {
            return Err(super::Error::Storage);
        }
        if self.initialized {
            block.copy_from_slice(&self.image);
        }
        Ok(self.initialized)
    }
    fn replace(&mut self, block: &[u8]) -> super::Result<()> {
        if block.len() != self.image.len() || Instant::now() >= self.deadline {
            return Err(super::Error::Storage);
        }
        if !self.delay.is_zero() {
            std::thread::sleep(
                self.delay
                    .min(self.deadline.saturating_duration_since(Instant::now())),
            );
        }
        if Instant::now() >= self.deadline {
            return Err(super::Error::Storage);
        }
        self.image.copy_from_slice(block);
        self.initialized = true;
        self.counts.writes.fetch_add(1, Ordering::Relaxed);
        self.counts
            .bytes
            .fetch_add(block.len() as u64, Ordering::Relaxed);
        Ok(())
    }
}

fn parse_loopback(text: &str) -> io::Result<LoopbackCounters> {
    let line = text
        .lines()
        .find_map(|line| {
            let (name, rest) = line.split_once(':')?;
            (name.trim() == "lo").then_some(rest)
        })
        .ok_or_else(|| invalid("/proc/net/dev has no lo interface"))?;
    let words: Vec<_> = line.split_whitespace().collect();
    if words.len() != 16 {
        return Err(invalid("unexpected /proc/net/dev lo fields"));
    }
    Ok(LoopbackCounters {
        tx_bytes: words[8]
            .parse()
            .map_err(|_| invalid("invalid lo TX bytes"))?,
        tx_packets: words[9]
            .parse()
            .map_err(|_| invalid("invalid lo TX packets"))?,
    })
}
fn parse_tc(text: &[u8]) -> io::Result<Option<TcCounters>> {
    let entries: serde_json::Value =
        serde_json::from_slice(text).map_err(|e| invalid(e.to_string()))?;
    let entries = entries
        .as_array()
        .ok_or_else(|| invalid("tc qdisc JSON is not an array"))?;
    let Some(clsact) = entries.iter().find(|e| e["kind"] == "clsact") else {
        return Ok(None);
    };
    let field = |e: &serde_json::Value, name: &str| {
        e[name]
            .as_u64()
            .ok_or_else(|| invalid(format!("tc missing {name}")))
    };
    Ok(Some(TcCounters {
        pre_impairment_accounted_bytes: field(clsact, "bytes")?,
        pre_impairment_packets: field(clsact, "packets")?,
        netem_drops: entries
            .iter()
            .find(|e| e["kind"] == "netem")
            .map(|e| field(e, "drops"))
            .transpose()?
            .unwrap_or(0),
    }))
}
fn network_scope() -> io::Result<Option<String>> {
    let Some(parent) = std::env::var_os("D28_BENCH_NETNS_PARENT") else {
        return Ok(None);
    };
    let current = std::fs::read_link("/proc/self/ns/net")?;
    if parent == current.as_os_str() {
        return Err(invalid(
            "D28_BENCH_NETNS_PARENT matches current namespace; refusing misleading isolated counters",
        ));
    }
    if parent.is_empty() || !parent.to_string_lossy().starts_with("net:[") {
        return Err(invalid("invalid D28_BENCH_NETNS_PARENT marker"));
    }
    parse_loopback(&std::fs::read_to_string("/proc/net/dev")?)?;
    Ok(Some(current.to_string_lossy().into_owned()))
}
// tc is read-only, but must not defeat the benchmark's finite deadline.
#[cfg(unix)]
fn bounded_output(
    command: &mut std::process::Command,
    deadline: Instant,
) -> io::Result<std::process::Output> {
    use std::{io::Read, os::fd::AsRawFd, process::Stdio};
    struct OwnedChild(std::process::Child);
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = OwnedChild(
        command
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?,
    );
    let mut stdout = child.0.stdout.take().expect("piped stdout");
    let fd = stdout.as_raw_fd();
    // The fd is our live pipe. Nonblocking reads avoid both full-pipe deadlock
    // and inherited writers keeping EOF away after the direct child exits.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let until = deadline.min(Instant::now() + Duration::from_millis(500));
    let mut output = Vec::new();
    let mut eof = false;
    loop {
        let mut buffer = [0; 4096];
        match stdout.read(&mut buffer) {
            Ok(0) => eof = true,
            Ok(n) => {
                output.extend_from_slice(&buffer[..n]);
                if output.len() > 65_536 {
                    return Err(invalid("tc output exceeds 64KiB"));
                }
            }
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) => {}
            Err(e) => return Err(e),
        }
        if let Some(status) = child.0.try_wait()? {
            if eof {
                return Ok(std::process::Output {
                    status,
                    stdout: output,
                    stderr: vec![],
                });
            }
        }
        if Instant::now() >= until {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "tc boundary sample exceeded deadline/500ms",
            ));
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}
#[cfg(not(unix))]
fn bounded_output(_: &mut std::process::Command, _: Instant) -> io::Result<std::process::Output> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "isolated tc sampling requires Unix",
    ))
}
struct Sampler {
    observers: Vec<ConsumerHandle>,
    stores: Vec<Arc<StoreCounts>>,
    scope: Option<String>,
    deadline: Instant,
    metrics: Vec<Arc<runtime::RuntimeMetrics>>,
}
impl Sampler {
    fn application_snapshot(&self) -> BTreeMap<String, u64> {
        let mut out = BTreeMap::new();
        for metrics in &self.metrics {
            let snapshot = metrics.snapshot();
            for (name, count) in [
                ("offered.hello", snapshot.offered.hello),
                ("offered.status", snapshot.offered.status),
                ("offered.bare_votes", snapshot.offered.bare_votes),
                ("offered.evidence", snapshot.offered.evidence),
                ("received", snapshot.received),
                ("processed", snapshot.processed),
                ("rejected", snapshot.rejected),
                (
                    "scheduler_budget_discarded",
                    snapshot.scheduler_budget_discarded,
                ),
            ] {
                *out.entry(format!("{name}.messages")).or_default() += count.messages;
                *out.entry(format!("{name}.bytes")).or_default() += count.bytes;
            }
            *out.entry("offered.status_request_messages".into())
                .or_default() += snapshot.offered.status_request_messages;
        }
        out
    }
    fn snapshot(&self, origin: Instant) -> io::Result<CounterSnapshot> {
        let at = Instant::now();
        let mut transport = BTreeMap::new();
        for handle in &self.observers {
            add_transport(&mut transport, handle.stats());
        }
        let mut result = CounterSnapshot {
            at_ms: milliseconds(at.duration_since(origin)),
            sampling_duration_ms: 0.0,
            application: self.application_snapshot(),
            transport,
            store_replacements: self
                .stores
                .iter()
                .map(|s| s.writes.load(Ordering::Relaxed))
                .sum(),
            store_replacement_bytes: self
                .stores
                .iter()
                .map(|s| s.bytes.load(Ordering::Relaxed))
                .sum(),
            loopback: None,
            tc: None,
            tc_error: None,
            tc_timed_out: false,
        };
        if self.scope.is_some() {
            result.loopback = Some(parse_loopback(&std::fs::read_to_string("/proc/net/dev")?)?);
            match bounded_output(
                std::process::Command::new("tc").args(["-j", "-s", "qdisc", "show", "dev", "lo"]),
                self.deadline,
            ) {
                Ok(output) if output.status.success() => match parse_tc(&output.stdout) {
                    Ok(counters) => result.tc = counters,
                    Err(error) => result.tc_error = Some(error.to_string()),
                },
                Ok(output) => result.tc_error = Some(format!("tc exited {}", output.status)),
                Err(error) if error.kind() == io::ErrorKind::TimedOut => {
                    result.tc_timed_out = true;
                    result.tc_error = Some(error.to_string());
                }
                Err(error) => result.tc_error = Some(error.to_string()),
            }
        }
        result.sampling_duration_ms = milliseconds(at.elapsed());
        Ok(result)
    }
}
fn add_transport(out: &mut BTreeMap<String, u64>, value: TransportStats) {
    macro_rules! fields { ($($name:ident),* $(,)?) => { $( *out.entry(stringify!($name).into()).or_default() += value.$name; )* }; }
    fields!(
        outbox_dropped_messages,
        outbox_dropped_bytes,
        send_to_unknown_peer_dropped,
        oversized_dropped,
        connects_skipped,
        connects_failed,
        ring_sent,
        ring_received,
        ring_receive_dropped_full,
        ring_inbound_refused_full,
        ring_send_head_blocked,
        ring_send_dropped_unknown,
        ring_send_dropped_oversized,
        stale_session_messages,
        stale_session_sends,
        inbox_dropped_messages,
        inbox_dropped_bytes,
        kills,
        redials_skipped,
        dials_not_allowed,
        inbound_refused
    );
}
fn map_delta(end: &BTreeMap<String, u64>, start: &BTreeMap<String, u64>) -> BTreeMap<String, u64> {
    end.iter()
        .map(|(k, v)| (k.clone(), v.saturating_sub(*start.get(k).unwrap_or(&0))))
        .collect()
}
fn counter_window(start: &CounterSnapshot, end: &CounterSnapshot) -> CounterWindow {
    CounterWindow {
        start: start.clone(),
        end: end.clone(),
        application_delta: map_delta(&end.application, &start.application),
        transport_delta: map_delta(&end.transport, &start.transport),
        store_replacements: end
            .store_replacements
            .saturating_sub(start.store_replacements),
        store_replacement_bytes: end
            .store_replacement_bytes
            .saturating_sub(start.store_replacement_bytes),
    }
}
fn network_window(
    scope: Option<&str>,
    start: &CounterSnapshot,
    end: &CounterSnapshot,
) -> io::Result<Option<NetworkWindow>> {
    let (Some(scope), Some(a), Some(b)) = (scope, start.loopback, end.loopback) else {
        return Ok(None);
    };
    Ok(Some(NetworkWindow {
        scope: scope.into(),
        start: a,
        end: b,
        delta: b.difference(a)?,
        tc_start: start.tc,
        tc_end: end.tc,
        tc_delta: end.tc.zip(start.tc).and_then(|(b, a)| b.difference(a)),
        semantics: "lo TX: aggregate post-netem L3 bytes/packets, no RX double count; drops excluded. Optional tc clsact: pre-impairment bytes including synthetic lo 14-byte L2 header per packet; not physical-link bytes. Serial boundary samples, in-flight final tail excluded.",
    }))
}

struct Cohort {
    config: BenchConfig,
    members: Vec<SortedRosterMember>,
    origin: Instant,
    committed: u64,
    committed_id: ValueId,
    parents: Vec<FatPointerToBftBlock>,
    reports: Vec<HeightReport>,
    rounds: Vec<u64>,
    round_counts: Vec<u64>,
    last_rounds: Vec<Option<u64>>,
    failure: Option<String>,
    completed: bool,
    measured_start: Option<CounterSnapshot>,
    measured_end: Option<CounterSnapshot>,
    sampler: Sampler,
}
impl Cohort {
    fn now(&self) -> f64 {
        milliseconds(self.origin.elapsed())
    }
    fn tip(&self) -> HostTip {
        HostTip {
            height: self.committed,
            block_id: self.committed_id,
            next_roster: self.members.clone(),
            next_vote_namespace: NAMESPACE,
            next_strict_quorum: true,
        }
    }
    fn open_height(&mut self, start: f64) {
        let height = self.committed + 1;
        self.reports.push(HeightReport {
            height,
            measured: height > u64::from(self.config.warmup),
            id: None,
            start_at_ms: start,
            first_proposal_at_ms: None,
            serialized_body_bytes: None,
            synthetic_payload_bytes: payload_size(self.config.headers).expect("validated headers"),
            first_latency_ms: None,
            quorum_latency_ms: None,
            all_latency_ms: None,
            cohort_committed: false,
            proposals: vec![],
            nodes: vec![],
        });
        self.rounds.fill(0);
        self.round_counts.fill(0);
        self.last_rounds.fill(None);
    }
    fn decision(
        &mut self,
        node: usize,
        value: BlockValue,
        pointer: FatPointerToBftBlock,
        at: f64,
        chunks_empty: bool,
    ) {
        if self.failure.is_some() || self.completed {
            return;
        }
        let check_start = self.now();
        let height = self.committed + 1;
        let id = ValueId(HashKeys::default().value_id.hash(&value.0));
        if !chunks_empty
            || !validate_body(
                &value,
                height,
                self.committed_id,
                &self.config,
                &self.members,
            )
            || !verify_pointer(&pointer, &self.members, height, id)
        {
            self.failure = Some(format!(
                "node {node} exported an invalid mock body or chain certificate at height {height}"
            ));
            return;
        }
        let export_host_check_ms = self.now() - check_start;
        let report = self.reports.last_mut().expect("open height");
        if report.nodes.iter().any(|d| d.node == node) {
            self.failure = Some(format!("node {node} duplicated push_decided"));
            return;
        }
        let encoded_id = hex::encode(id.0);
        if report.id.as_ref().is_some_and(|old| old != &encoded_id) {
            self.failure = Some(format!("conflicting decisions at height {height}"));
            return;
        }
        report.id = Some(encoded_id);
        report.serialized_body_bytes = Some(value.0.len() as u64);
        let latency = at - report.start_at_ms;
        report.nodes.push(NodeDecision {
            node,
            push_decided_at_ms: at,
            latency_ms: latency,
            callback_lock_wait_ms: (check_start - at).max(0.0),
            export_host_check_ms,
            certificate_round: pointer.get_vote_template().round as u64,
            certificate_signatures: pointer.signatures.len(),
            max_observed_round: self.rounds[node],
            rounds_entered: self.round_counts[node],
            barrier_wait_ms: None,
            ack_wait_ms: None,
        });
        self.parents[node] = pointer;
        // Callback entry order may differ from lock order, so always sort timestamps.
        let mut times: Vec<_> = report.nodes.iter().map(|n| n.latency_ms).collect();
        times.sort_by(f64::total_cmp);
        report.first_latency_ms = times.first().copied();
        report.quorum_latency_ms = times.get(2 * self.config.nodes / 3).copied();
        if report.nodes.len() != self.config.nodes - self.config.offline {
            return;
        }
        report.all_latency_ms = times.last().copied();
        report.cohort_committed = true;
        let all_at = report.start_at_ms + report.all_latency_ms.unwrap();
        for node in &mut report.nodes {
            node.barrier_wait_ms = Some(all_at - node.push_decided_at_ms);
        }
        let final_height = height == u64::from(self.config.heights + self.config.warmup);
        if height == u64::from(self.config.warmup) || final_height {
            match self.sampler.snapshot(self.origin) {
                Ok(sample) => {
                    if sample.tc_timed_out {
                        self.failure = Some("tc boundary sampling timed out".into());
                    }
                    if final_height {
                        self.measured_end = Some(sample);
                    } else {
                        self.measured_start = Some(sample);
                    }
                }
                Err(error) => {
                    self.failure = Some(format!("boundary sampling: {error}"));
                    return;
                }
            }
        }
        if final_height {
            // Keep final ACK and read_tip unpublished during the optional chatter
            // hold. No next-height INIT or proposals can contaminate the tail.
            self.completed = true;
        } else {
            self.committed = height;
            self.committed_id = id;
            self.open_height(all_at);
        }
    }
}
fn callbacks(cohort: Arc<Mutex<Cohort>>, node: usize) -> HostCallbacks {
    let propose = cohort.clone();
    let validate = cohort.clone();
    let pushed = cohort.clone();
    let tip = cohort.clone();
    HostCallbacks {
        propose: ClosureToProposeNewBlock(Arc::new(move || {
            let cohort = propose.clone();
            let (height, parent, headers, delay, request) = {
                let mut c = cohort.lock().unwrap();
                let at = c.now();
                let height = c.committed + 1;
                let r = c.reports.last_mut().unwrap();
                let request = r.proposals.len();
                r.first_proposal_at_ms.get_or_insert(at);
                r.proposals.push(ProposalTiming {
                    node,
                    requested_at_ms: at,
                    body_ready_at_ms: None,
                });
                (
                    height,
                    c.parents[node].clone(),
                    c.config.headers,
                    c.config.host_delay_ms,
                    request,
                )
            };
            Box::pin(async move {
                if delay > 0 {
                    tokio::time::sleep(Duration::from_millis(delay)).await;
                }
                let body = mock_body(height as u32, parent, headers);
                let mut c = cohort.lock().unwrap();
                let at = c.now();
                if let Some(r) = c.reports.get_mut(height as usize - 1) {
                    if let Some(p) = r.proposals.get_mut(request) {
                        p.body_ready_at_ms = Some(at);
                    }
                }
                match body {
                    Ok(body) => Some(body),
                    Err(error) => {
                        c.failure = Some(error.to_string());
                        None
                    }
                }
            })
        })),
        validate: ClosureToValidateProposedBlock(Arc::new(move |body| {
            let cohort = validate.clone();
            Box::pin(async move {
                let (config, members, height, id) = {
                    let c = cohort.lock().unwrap();
                    (
                        c.config.clone(),
                        c.members.clone(),
                        c.committed + 1,
                        c.committed_id,
                    )
                };
                if config.host_delay_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(config.host_delay_ms)).await;
                }
                let valid = validate_body(body, height, id, &config, &members);
                (
                    if valid {
                        TMStatus::Pass
                    } else {
                        TMStatus::Fail
                    },
                    TMStatusReason::None,
                )
            })
        })),
        push_decided: ClosureToPushDecidedBlock(Arc::new(move |body, pointer, chunks| {
            // Timestamp at actual local callback entry, before certificate checks,
            // shared cohort lock and ACK barrier; never a coordinator-delivered body.
            let entered = Instant::now();
            let cohort = pushed.clone();
            let height = pointer.get_vote_template().height;
            {
                let mut c = cohort.lock().unwrap();
                let at = milliseconds(entered.duration_since(c.origin));
                c.decision(node, body, pointer, at, chunks.is_empty());
            }
            Box::pin(async move {
                loop {
                    let ready = {
                        let mut c = cohort.lock().unwrap();
                        if c.committed >= height {
                            let at = c.now();
                            if let Some(d) = c.reports[height as usize - 1]
                                .nodes
                                .iter_mut()
                                .find(|d| d.node == node)
                            {
                                d.ack_wait_ms = Some(at - d.push_decided_at_ms);
                            }
                            Some(c.tip())
                        } else {
                            None
                        }
                    };
                    if let Some(tip) = ready {
                        return tip;
                    }
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            })
        })),
        read_tip: ClosureToReadDecidedTip(Arc::new(move |height| {
            let c = tip.lock().unwrap();
            (height <= c.committed).then(|| c.tip())
        })),
        update_peers: ClosureToUpdatePeers(Arc::new(|_| Box::pin(async {}))),
        access: AccessD28(Arc::new(move |snapshot, _| {
            let mut c = cohort.lock().unwrap();
            if snapshot.height == c.committed + 1 {
                c.rounds[node] = c.rounds[node].max(snapshot.round);
                if c.last_rounds[node] != Some(snapshot.round) {
                    c.round_counts[node] += 1;
                    c.last_rounds[node] = Some(snapshot.round);
                }
            }
        })),
    }
}

/// Run in an enabled Tokio runtime. `workers` describes the CLI's runtime;
/// library callers are responsible for their own executor size. No external
/// seeds, ports, identities, ledger storage or chain formats are accepted.
pub async fn run(config: BenchConfig) -> io::Result<BenchReport> {
    run_inner(config, |_, _| {}).await
}
async fn run_inner(
    config: BenchConfig,
    started: impl FnOnce(std::sync::Weak<Mutex<Cohort>>, Vec<u16>) + Send,
) -> io::Result<BenchReport> {
    config.validate()?;
    let scope = network_scope()?;
    let online = config.nodes - config.offline;
    let members = roster(config.nodes);
    let image_size = durable::image_size(durable::Limits {
        max_members: config.nodes,
        max_body: config.max_body,
    })
    .map_err(|_| invalid("invalid durable geometry"))?;
    // Keep reservations alive until all ports have been chosen, avoiding reuse.
    let reservations: Vec<_> = (0..online)
        .map(|_| UdpSocket::bind((Ipv6Addr::LOCALHOST, 0)))
        .collect::<io::Result<_>>()?;
    let identities: Vec<_> = (0..online)
        .map(|i| {
            crate::new_keypair_from_connect_magic1_with_seed(
                crate::CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2b,
                seed("transport", i),
            )
            .ok_or_else(|| invalid("STP identity construction failed"))
        })
        .collect::<io::Result<_>>()?;
    let addresses: Vec<_> = identities
        .iter()
        .zip(&reservations)
        .map(|(identity, socket)| {
            Ok(STPAddress::from(
                Ipv6Addr::LOCALHOST,
                socket.local_addr()?.port(),
                identity,
            ))
        })
        .collect::<io::Result<_>>()?;
    let ports: Vec<_> = reservations
        .iter()
        .map(|s| s.local_addr().map(|a| a.port()))
        .collect::<io::Result<_>>()?;
    drop(reservations);
    let transport_limits = bench_transport_limits(config.nodes, config.max_body)?;
    let transports: Vec<_> = identities
        .into_iter()
        .zip(ports.iter().copied())
        .map(|(identity, port)| {
            SharedTransport::with_bind_ip(
                vec![identity],
                port,
                None,
                (1_000_000, 512 * 1024, 8 << 20),
                transport_limits.clone(),
                std::net::IpAddr::V6(Ipv6Addr::LOCALHOST),
            )
        })
        .collect();
    for transport in &transports {
        transport.dial_only(addresses.iter().map(STPAddress::connection_key).collect());
    }
    let stores: Vec<_> = (0..online)
        .map(|_| Arc::new(StoreCounts::default()))
        .collect();
    let origin = Instant::now();
    let deadline = origin + Duration::from_secs(config.timeout_secs);
    let metrics: Vec<_> = (0..online)
        .map(|_| Arc::new(runtime::RuntimeMetrics::default()))
        .collect();
    let sampler = Sampler {
        observers: transports
            .iter()
            .map(|t| t.attach(Consumer::Base))
            .collect(),
        stores: stores.clone(),
        scope,
        deadline,
        metrics: metrics.clone(),
    };
    let mut state = Cohort {
        config: config.clone(),
        members: members.clone(),
        origin,
        committed: 0,
        committed_id: ValueId::NIL,
        parents: vec![FatPointerToBftBlock::null(); online],
        reports: vec![],
        rounds: vec![0; online],
        round_counts: vec![0; online],
        last_rounds: vec![None; online],
        failure: None,
        completed: false,
        measured_start: None,
        measured_end: None,
        sampler,
    };
    if config.warmup == 0 {
        state.measured_start = Some(state.sampler.snapshot(origin)?);
    }
    state.open_height(0.0);
    let cohort = Arc::new(Mutex::new(state));
    // JoinSet aborts owned runtimes if the caller cancels this future.
    let mut tasks = tokio::task::JoinSet::new();
    let mut runtime_settings = BTreeMap::new();
    for (i, transport) in transports.into_iter().enumerate() {
        let mut runtime = RuntimeConfig::new(
            1,
            members.clone(),
            NAMESPACE,
            config.nodes,
            transport,
            addresses
                .iter()
                .enumerate()
                .filter(|(j, _)| *j != i)
                .map(|(_, a)| a.clone())
                .collect(),
        );
        runtime.max_body = config.max_body;
        runtime.metrics = Some(metrics[i].clone());
        if i == 0 {
            macro_rules! settings { ($($field:ident),*) => { $(runtime_settings.insert(stringify!($field).into(), runtime.$field as u64);)* }; }
            settings!(
                initial_height,
                active_roster_max_n,
                max_members,
                max_body,
                rho,
                request_timeout,
                status_ttl,
                max_connections,
                max_requests,
                max_outgoing,
                max_incoming,
                max_receive_per_tick
            );
            runtime_settings.insert(
                "poll_interval_ms".into(),
                runtime.poll_interval.as_millis() as u64,
            );
            runtime_settings.insert(
                "status_interval_ms".into(),
                runtime.status_interval.as_millis() as u64,
            );
            for (name, value) in [
                ("propose", runtime.timeouts.propose),
                ("prevote", runtime.timeouts.prevote),
                ("precommit", runtime.timeouts.precommit),
                ("inc_propose", runtime.timeouts.inc_propose),
                ("inc_prevote", runtime.timeouts.inc_prevote),
                ("inc_precommit", runtime.timeouts.inc_precommit),
            ] {
                runtime_settings.insert(format!("timeouts.{name}_ms"), value);
            }
        }
        let backend = MemoryBackend {
            image: vec![0; image_size],
            initialized: false,
            delay: Duration::from_millis(config.store_delay_ms),
            deadline,
            counts: stores[i].clone(),
        };
        tasks.spawn(runtime::run(
            key(i),
            backend,
            runtime,
            callbacks(cohort.clone(), i),
        ));
    }
    started(Arc::downgrade(&cohort), ports);
    let mut chatter_end = None;
    loop {
        let (complete, failed) = {
            let c = cohort.lock().unwrap();
            (c.completed, c.failure.is_some())
        };
        if failed {
            break;
        }
        if complete {
            if config.chatter_secs > 0 {
                let until = Instant::now() + Duration::from_secs(config.chatter_secs);
                if until > deadline {
                    cohort.lock().unwrap().failure =
                        Some("deadline cannot accommodate requested chatter hold".into());
                } else {
                    tokio::time::sleep_until(tokio::time::Instant::from_std(until)).await;
                    let mut c = cohort.lock().unwrap();
                    match c.sampler.snapshot(origin) {
                        Ok(sample) => chatter_end = Some(sample),
                        Err(e) => c.failure = Some(format!("chatter sampling: {e}")),
                    }
                }
            }
            break;
        }
        if Instant::now() >= deadline {
            cohort.lock().unwrap().failure =
                Some("benchmark deadline reached before all requested cohort decisions".into());
            break;
        }
        if let Some(outcome) = tasks.try_join_next() {
            cohort.lock().unwrap().failure = Some(format!(
                "runtime exited before all requested cohort decisions: {outcome:?}"
            ));
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    {
        let mut c = cohort.lock().unwrap();
        if c.measured_start.is_some() && c.measured_end.is_none() {
            match c.sampler.snapshot(origin) {
                Ok(sample) => c.measured_end = Some(sample),
                Err(e) => {
                    c.failure.get_or_insert(format!("partial sampling: {e}"));
                }
            }
        }
    }
    tasks.abort_all();
    while let Some(outcome) = tasks.join_next().await {
        match outcome {
            Err(error) if error.is_cancelled() => {}
            outcome => {
                let mut c = cohort.lock().unwrap();
                c.failure
                    .get_or_insert(format!("runtime exited: {outcome:?}"));
            }
        }
    }
    let c = cohort.lock().unwrap();
    let measured: Vec<_> = c
        .reports
        .iter()
        .filter(|h| h.measured && h.cohort_committed)
        .collect();
    let elapsed = c
        .measured_start
        .as_ref()
        .zip(c.measured_end.as_ref())
        .map(|(a, b)| (b.at_ms - a.at_ms) / 1000.0)
        .filter(|v| *v > 0.0);
    let body_bytes = measured
        .iter()
        .filter_map(|h| h.serialized_body_bytes)
        .sum::<u64>();
    let payload_bytes = measured
        .iter()
        .map(|h| h.synthetic_payload_bytes)
        .sum::<u64>();
    let (latency_ms, latency_samples) = latency_summary(&c.reports);
    let mut failure = c.failure.clone();
    let measured_window = c
        .measured_start
        .as_ref()
        .zip(c.measured_end.as_ref())
        .map(|(a, b)| counter_window(a, b));
    let network = c
        .measured_start
        .as_ref()
        .zip(c.measured_end.as_ref())
        .map(|(a, b)| network_window(c.sampler.scope.as_deref(), a, b))
        .transpose()
        .unwrap_or_else(|e| {
            failure.get_or_insert(format!("network counters: {e}"));
            None
        })
        .flatten();
    let chatter_window = c
        .measured_end
        .as_ref()
        .zip(chatter_end.as_ref())
        .map(|(a, b)| counter_window(a, b));
    let chatter_network = c
        .measured_end
        .as_ref()
        .zip(chatter_end.as_ref())
        .map(|(a, b)| network_window(c.sampler.scope.as_deref(), a, b))
        .transpose()
        .unwrap_or_else(|e| {
            failure.get_or_insert(format!("chatter counters: {e}"));
            None
        })
        .flatten();
    Ok(BenchReport {
        schema_version: 1,
        status: if c.completed && failure.is_none() {
            "complete"
        } else {
            "partial"
        },
        failure,
        config: config.clone(),
        quorum_nodes: 2 * config.nodes / 3 + 1,
        online_nodes: (0..online).collect(),
        offline_nodes: (online..config.nodes).collect(),
        durable_image_bytes_per_node: image_size,
        total_run_seconds: origin.elapsed().as_secs_f64(),
        summary: Summary {
            measured_heights: measured.len(),
            serialized_body_bytes: body_bytes,
            synthetic_payload_bytes: payload_bytes,
            elapsed_seconds: elapsed,
            committed_heights_per_second: elapsed.map(|s| measured.len() as f64 / s),
            committed_body_bytes_per_second: elapsed.map(|s| body_bytes as f64 / s),
            committed_payload_bytes_per_second: elapsed.map(|s| payload_bytes as f64 / s),
            latency_ms,
            latency_samples,
        },
        measured_window,
        network,
        chatter_window,
        chatter_network,
        runtime_settings,
        transport_settings: format!(
            "send_buffer_params=(1000000,524288,8388608); max_pps=None; limits={:?}; peer_high_water_bytes clamped to min buffer=524288",
            transport_limits
        ),
        nodes: (0..config.nodes)
            .map(|i| {
                let times: Vec<_> = c
                    .reports
                    .iter()
                    .filter(|h| h.measured)
                    .flat_map(|h| &h.nodes)
                    .filter(|n| n.node == i)
                    .map(|n| n.latency_ms)
                    .collect();
                NodeReport {
                    node: i,
                    public_key: hex::encode(members[i].pub_key.0),
                    online: i < online,
                    measured_decisions: times.len(),
                    latency_ms: percentiles(&times),
                }
            })
            .collect(),
        heights: c.reports.clone(),
        methodology: vec![
            "Component-only: real runtime/STP/crypto/codec/durable image; mock host checks height, canonical synthetic headers, and parent id/height/certificate, not PoW or ledger.",
            "Equal stake, deterministic public test keys and online indices [0,N-offline); absent identities stay in full roster, no endpoint/task. Strict >2/3 quorum. Not Byzantine-fault testing.",
            "All-online ACK barrier gates BOTH callback future completion and read_tip publication; every node independently exports matching height/id. Model limitation: slowest online node gates next height; no chain catch-up/sync measurement.",
            "Latency starts at prior cohort's last local push_decided entry (run start for height1), ends at local callback entry before host checks/ACK. Includes INIT, scheduling, unavailable proposer timeouts, validation/storage/network. First/quorum/all are order statistics of node callbacks, not certificate formation timestamps.",
            "Proposal requested/body-ready timestamps are host acquisition events, not first network send. Nearest-rank latency percentiles include every observed measured milestone (sample counts separate); missing decisions are censored, never zero. Partial status and raw incomplete height retained; throughput denominator includes stalled measured time.",
            "Every local callback already exports a full signed quorum certificate. Quorum latency means a quorum of nodes independently exported, not the first certificate. Extra export_host_check_ms is benchmark validation overhead after that node's entry; callback_lock_wait_ms measures coordinator contention.",
            "Application offered.* counts encoded app bytes/messages per recipient at transport handoff, not delivery or UDP. status_request_messages is a subset of status.messages, not additive. RX outcome counters and offered counters are cumulative relaxed independent reads.",
            "Serialized body/payload throughput counts one chain copy per committed height, not per-node replication. Payload is exact serialized synthetic header bytes, no transactions or TPS; parent certificate sizes can vary.",
            "In-memory fixed image reused by copy_from_slice; full durable adapter serialization/hash/write workload. Artificial synchronous replacement delay, no fsync or crash-durability claim. Image bytes=4096+556*Nmax+3*Bmax, not whole-process memory.",
            "Counter boundaries are sampled serially under cohort lock after callback entry/validation; sampling_duration_ms exposes observer skew and next-height perturbation. STP pump threads continue concurrently; counters are not an atomic system snapshot.",
            "Measured counters exclude warmup; success ends at final cohort before shutdown/drain. Final in-flight tail excluded. Optional chatter window keeps final ACK/tip unpublished and measures continued final-height protocol traffic separately, not a quiescent post-commit network.",
            "TransportStats gathered with Base.stats only: no Base service and no second BFT attachment. Ring/message counters are not UDP packet/byte counters. Outside marked isolated netns network counters are null.",
            "Production RuntimeConfig timer defaults unchanged; max_body differs only when explicitly configured. workers is CLI executor size; library caller owns executor sizing. Debug builds are not production-throughput evidence.",
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use zcash_primitives::bft::{Blake3Hash, FatPointerSignature, Vote};
    pub(super) fn pointer(n: usize, signers: &[usize], height: u64, id: ValueId) -> FatPointerToBftBlock {
        let signatures: Vec<_> = signers
            .iter()
            .map(|&i| {
                let vote = Vote {
                    validator_address: roster(n)[i].pub_key,
                    value: Blake3Hash(id.0),
                    height,
                    typ: true,
                    round: 0,
                };
                FatPointerSignature {
                    pub_key: vote.validator_address,
                    vote_signature: crate::sign_with_namespace(
                        &key(i),
                        &vote.to_bytes(),
                        &NAMESPACE,
                    ),
                }
            })
            .collect();
        FatPointerToBftBlock::from_parts(Blake3Hash(id.0), height, 0, &signatures)
    }
    #[test]
    fn benchmark_body_and_transport_caps_leave_production_defaults_unchanged() {
        assert_eq!(BenchConfig::default().max_body, crate::PROPOSAL_MAX_SIZE);
        assert_eq!(crate::PROPOSAL_MAX_SIZE, 1024 * 1024);
        let defaults = crate::transport::TransportLimits::default();
        let legacy = bench_transport_limits(7, crate::PROPOSAL_MAX_SIZE).unwrap();
        assert_eq!(format!("{legacy:?}"), format!("{defaults:?}"));
        let limits = bench_transport_limits(7, BENCH_MAX_BODY).unwrap();
        let frame = super::super::wire::Limits { max_members: 7, max_body: BENCH_MAX_BODY }
            .max_frame_bytes().unwrap();
        assert_eq!(frame, BENCH_MAX_BODY + 1499);
        assert_eq!(limits.max_message_size, frame);
        assert_eq!(limits.peer_outbox_bytes, 16 << 20);
        assert_eq!(limits.consumer_inbox_peer_bytes, 16 << 20);
        assert_eq!(limits.consumer_inbox_bytes, 64 << 20);
        assert_eq!(limits.peer_high_water_bytes, 512 * 1024);
        assert_eq!((limits.rings.send, limits.rings.control, limits.rings.receive), (1024, 256, 32));
        let mut config = BenchConfig { max_body: BENCH_MAX_BODY, ..Default::default() };
        assert!(config.validate().is_ok());
        config.max_body += 1;
        assert!(config.validate().is_err());
        assert!(bench_transport_limits(7, config.max_body).is_err());
    }
    #[test]
    fn bounds_preserve_strict_full_roster_quorum_and_body_limit() {
        let mut c = BenchConfig::default();
        c.offline = 1;
        assert!(c.validate().is_ok());
        c.offline = 2;
        assert!(c.validate().is_err());
        c.offline = 0;
        c.max_body = 16;
        assert!(c.validate().is_err());
        c.max_body = crate::PROPOSAL_MAX_SIZE;
        c.heights = 0;
        assert!(c.validate().is_err());
        assert_eq!(payload_size(32).unwrap(), 187 * 32);
        c.heights = 1;
        c.headers = 2048;
        assert!(c.validate().is_ok());
        let body = mock_body(1, FatPointerToBftBlock::null(), 2048).unwrap();
        assert!(validate_body(&body, 1, ValueId::NIL, &c, &roster(c.nodes)));
        c.headers = 2049;
        assert!(c.validate().is_err());
    }
    #[test]
    fn mock_parent_accepts_different_valid_signer_subsets_not_fake_certificates() {
        let c = BenchConfig {
            headers: 1,
            ..Default::default()
        };
        let members = roster(4);
        let id = ValueId([7; 32]);
        let a = pointer(4, &[0, 1, 2], 1, id);
        let b = pointer(4, &[0, 2, 3], 1, id);
        assert_ne!(a, b);
        for parent in [a, b] {
            let body = mock_body(2, parent, 1).unwrap();
            assert!(validate_body(&body, 2, id, &c, &members));
            assert!(!validate_body(&body, 2, ValueId([8; 32]), &c, &members));
        }
        let short = pointer(4, &[0, 1], 1, id);
        assert!(!verify_pointer(&short, &members, 1, id));
        let mut bad = pointer(4, &[0, 1, 2], 1, id);
        bad.signatures[0].vote_signature[0] ^= 1;
        assert!(!verify_pointer(&bad, &members, 1, id));
        let body = mock_body(1, FatPointerToBftBlock::null(), 1).unwrap();
        assert!(validate_body(&body, 1, ValueId::NIL, &c, &members));
    }
    #[test]
    fn memory_image_reused_and_wrong_size_is_not_committed() {
        let mut store = MemoryBackend {
            image: vec![0; 8],
            initialized: false,
            delay: Duration::ZERO,
            deadline: Instant::now() + Duration::from_secs(1),
            counts: Arc::default(),
        };
        let address = store.image.as_ptr();
        let mut loaded = [0; 8];
        assert!(!store.load(&mut loaded).unwrap());
        store.replace(&[1; 8]).unwrap();
        assert!(store.replace(&[2; 7]).is_err());
        assert!(store.load(&mut loaded).unwrap());
        assert_eq!(loaded, [1; 8]);
        assert_eq!(address, store.image.as_ptr());
        assert_eq!(store.counts.writes.load(Ordering::Relaxed), 1);
    }
    #[test]
    fn counters_and_nearest_rank_have_explicit_units() {
        assert_eq!(percentiles(&[]), None);
        assert_eq!(
            percentiles(&[1., 2., 3., 4.]).unwrap(),
            Percentiles {
                p50: 2.,
                p95: 4.,
                p99: 4.
            }
        );
        let text = "Inter-| Receive | Transmit\n lo: 9999 99 0 0 0 0 0 0 1234 12 0 0 0 0 0 0\n";
        assert_eq!(
            parse_loopback(text).unwrap(),
            LoopbackCounters {
                tx_bytes: 1234,
                tx_packets: 12
            }
        );
        assert!(parse_loopback("lo: 1").is_err());
        assert_eq!(
            parse_tc(br#"[{"kind":"clsact","bytes":162,"packets":1},{"kind":"netem","drops":1}]"#)
                .unwrap()
                .unwrap(),
            TcCounters {
                pre_impairment_accounted_bytes: 162,
                pre_impairment_packets: 1,
                netem_drops: 1
            }
        );
        assert_eq!(parse_tc(b"[]").unwrap(), None);
    }
    #[test]
    fn barrier_hides_tip_until_all_online_independent_exports() {
        let config = BenchConfig {
            heights: 2,
            warmup: 1,
            headers: 0,
            ..Default::default()
        };
        let mut c = Cohort {
            members: roster(4),
            origin: Instant::now(),
            committed: 0,
            committed_id: ValueId::NIL,
            parents: vec![FatPointerToBftBlock::null(); 4],
            reports: vec![],
            rounds: vec![0; 4],
            round_counts: vec![0; 4],
            last_rounds: vec![None; 4],
            failure: None,
            completed: false,
            measured_start: None,
            measured_end: None,
            sampler: Sampler {
                observers: vec![],
                stores: vec![],
                scope: None,
                deadline: Instant::now() + Duration::from_secs(1),
                metrics: vec![],
            },
            config,
        };
        c.open_height(0.0);
        let body = mock_body(1, FatPointerToBftBlock::null(), 0).unwrap();
        let id = ValueId(HashKeys::default().value_id.hash(&body.0));
        for i in 0..3 {
            c.decision(
                i,
                body.clone(),
                pointer(4, &[0, 1, 2], 1, id),
                i as f64 + 1.,
                true,
            );
            assert_eq!(c.tip().height, 0);
        }
        let mut partial = c.reports[0].clone();
        partial.measured = true;
        let (latencies, counts) = latency_summary(&[partial]);
        assert_eq!(
            counts,
            BTreeMap::from([("first".into(), 1), ("quorum".into(), 1), ("all".into(), 0)])
        );
        assert_eq!(latencies.first.unwrap().p50, 1.);
        assert_eq!(latencies.quorum.unwrap().p50, 3.);
        assert!(latencies.all.is_none());
        assert!(c.measured_start.is_none());
        c.decision(3, body, pointer(4, &[0, 2, 3], 1, id), 4., true);
        assert_eq!(c.tip().height, 1);
        assert!(c.measured_start.is_some());
        assert_eq!(c.reports[0].quorum_latency_ms, Some(3.));
        assert_eq!(c.reports[0].all_latency_ms, Some(4.));
        assert_eq!(c.reports[1].start_at_ms, 4.);
    }
    #[test]
    fn cancelling_library_future_releases_owned_runtime_and_loopback_sockets() {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let sentinel = tokio::spawn(std::future::pending::<()>());
                let (send, receive) = tokio::sync::oneshot::channel();
                let config = BenchConfig {
                    heights: 2,
                    warmup: 0,
                    headers: 1,
                    max_body: 4096,
                    timeout_secs: 5,
                    host_delay_ms: 1000,
                    ..Default::default()
                };
                let task = tokio::spawn(run_inner(config, move |weak, ports| {
                    let _ = send.send((weak, ports));
                }));
                let (weak, ports) = receive.await.unwrap();
                tokio::time::timeout(Duration::from_secs(2), async {
                    loop {
                        let booted = weak.upgrade().is_some_and(|c| {
                            c.lock()
                                .unwrap()
                                .sampler
                                .stores
                                .iter()
                                .all(|s| s.writes.load(Ordering::Relaxed) > 0)
                        });
                        if booted {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await
                .expect("owned nodes booted");
                task.abort();
                assert!(task.await.unwrap_err().is_cancelled());
                tokio::time::timeout(Duration::from_secs(2), async {
                    while weak.upgrade().is_some() {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                    loop {
                        let rebound: io::Result<Vec<_>> = ports
                            .iter()
                            .map(|&port| UdpSocket::bind((Ipv6Addr::LOCALHOST, port)))
                            .collect();
                        if rebound.is_ok() {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(5)).await;
                    }
                })
                .await
                .expect("cancelled nodes released cohort, transports and sockets");
                assert!(!sentinel.is_finished(), "unrelated task survived");
                sentinel.abort();
                let _ = sentinel.await;
            });
    }
    #[test]
    #[cfg(unix)]
    fn boundary_subprocess_timeout_kills_and_reaps_only_its_child() {
        let mut sentinel = std::process::Command::new("sleep")
            .arg("10")
            .spawn()
            .unwrap();
        let start = Instant::now();
        let error = bounded_output(
            std::process::Command::new("sleep").arg("10"),
            start + Duration::from_millis(20),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(sentinel.try_wait().unwrap().is_none());
        sentinel.kill().unwrap();
        sentinel.wait().unwrap();
        // An inherited writer outliving the direct child must not block EOF.
        let error = bounded_output(
            std::process::Command::new("sh").args(["-c", "sleep 0.1 & exit 0"]),
            Instant::now() + Duration::from_millis(20),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        // Expired counter sampling still retains app/store/lo samples, not an error.
        let sample = Sampler {
            observers: vec![],
            stores: vec![],
            metrics: vec![],
            scope: Some("test".into()),
            deadline: Instant::now(),
        }
        .snapshot(Instant::now())
        .unwrap();
        assert!(sample.loopback.is_some());
        assert!(sample.tc_error.is_some());
    }
}
