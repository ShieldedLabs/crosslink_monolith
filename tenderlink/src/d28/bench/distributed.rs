//! One real D28 validator per process, with an out-of-band all-online ACK barrier.
//! Predictable benchmark keys and mock headers: private test networks only.
//! The controller never supplies a body, certificate, roster or trusted tip.
use super::*;
use serde::Deserialize;
use std::net::{IpAddr, SocketAddrV6};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    sync::oneshot,
};

const CONTROL_LIMIT: usize = 64 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DistributedConfig {
    pub node: usize,
    pub nodes: usize,
    pub offline: usize,
    pub headers: usize,
    pub heights: u32,
    pub warmup: u32,
    pub timeout_secs: u64,
    /// Tokio executor threads per validator process, not machine vCPUs or total threads.
    pub workers: usize,
    pub max_body: usize,
    pub chatter_secs: u64,
    pub peers: Vec<SocketAddrV6>,
    pub coordinator: SocketAddrV6,
    pub run_id: String,
    /// Legacy per-header padding in the existing Zcash solution field.
    #[serde(default)]
    pub solution_bytes: usize,
    /// Exact serialized BftBlock target, including the actual parent certificate.
    /// Zero retains legacy header/solution sizing. Mock padding is NOT valid PoW.
    #[serde(default)]
    pub body_bytes: usize,
    /// Explicit benchmark-only fallback timers; omitted means unchanged production.
    #[serde(default = "default_timing_profile")]
    pub timing_profile: String,
    #[serde(default = "default_payload_mode")]
    pub payload_mode: String,
    #[serde(default)]
    pub payload_leaf_bytes: usize,
    #[serde(default = "default_poll")]
    pub poll_ms: u64,
    #[serde(default = "default_status")]
    pub status_ms: u64,
    #[serde(default = "default_budget")]
    pub receive_budget: usize,
    #[serde(default = "default_requests")]
    pub max_requests: usize,
    #[serde(default = "default_requests")]
    pub max_outgoing: usize,
}
fn default_poll() -> u64 {
    25
}
fn default_status() -> u64 {
    100
}
fn default_budget() -> usize {
    32
}
fn default_requests() -> usize {
    2
}

fn default_timing_profile() -> String {
    "production".into()
}

impl DistributedConfig {
    fn namespace(&self) -> [u8; 32] {
        payload_namespace(&self.payload_mode, self.payload_leaf_bytes)
    }
    fn validate_body_shape(&self) -> io::Result<()> {
        if !(1..=32).contains(&self.nodes)
            || !(1..=BENCH_MAX_BODY).contains(&self.max_body)
            || self.body_bytes > self.max_body
            || self.body_bytes > BENCH_MAX_BODY
            || self.headers > 2048
            || self.solution_bytes > 1344
            || (self.body_bytes > 0 && (self.headers == 0 || self.solution_bytes != 0))
        {
            return Err(invalid("bounds: nodes 1..32, max_body 1..40MiB, body_bytes <=max_body, headers <=2048; byte mode requires headers 1..2048 and solution_bytes=0; legacy solution_bytes <=1344"));
        }
        Ok(())
    }
    fn apply_timing(
        &self,
        timeouts: &mut super::super::Timeouts,
        request_timeout: &mut u64,
        status_ttl: &mut u64,
    ) {
        let bases = match self.timing_profile.as_str() {
            "wan" => (10_000, 5_000, 7_000),
            "wan_fast" => (2_500, 1_500, 2_000),
            _ => return,
        };
        (timeouts.propose, timeouts.prevote, timeouts.precommit) = bases;
        *request_timeout = 10_000;
        *status_ttl = 15_000;
    }
    fn base(&self) -> BenchConfig {
        BenchConfig {
            nodes: self.nodes,
            offline: self.offline,
            headers: self.headers,
            heights: self.heights,
            warmup: self.warmup,
            timeout_secs: self.timeout_secs,
            workers: self.workers,
            max_body: self.max_body,
            chatter_secs: self.chatter_secs,
            payload_mode: self.payload_mode.clone(),
            payload_leaf_bytes: self.payload_leaf_bytes,
            ..BenchConfig::default()
        }
    }
    pub fn validate(&self) -> io::Result<()> {
        self.validate_body_shape()?;
        if !matches!(self.timing_profile.as_str(), "production" | "wan" | "wan_fast") {
            return Err(invalid("timing_profile must be production, wan or wan_fast"));
        }
        if !(1..=4).contains(&self.workers) {
            return Err(invalid("distributed workers must be 1..4 Tokio executor threads"));
        }
        self.base().validate()?;
        if self.node >= self.nodes - self.offline || self.peers.len() != self.nodes {
            return Err(invalid(
                "node must be online; peers must contain the entire ordered roster",
            ));
        }
        if self.run_id.is_empty()
            || self.run_id.len() > 128
            || !self
                .run_id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
        {
            return Err(invalid(
                "run_id must be 1..128 ASCII letters, digits, dash, dot or underscore",
            ));
        }
        if self.solution_bytes > 1344
            || !(1..=1000).contains(&self.poll_ms)
            || !(1..=5000).contains(&self.status_ms)
            || !(1..=4096).contains(&self.receive_budget)
            || !(1..=128).contains(&self.max_requests)
            || !(1..=128).contains(&self.max_outgoing)
        {
            return Err(invalid(
                "bounds: solution_bytes <=1344, poll_ms 1..1000, status_ms 1..5000, receive_budget 1..4096, requests/outgoing 1..128",
            ));
        }
        for addr in self.peers.iter().chain(std::iter::once(&self.coordinator)) {
            // Numeric loopback or ULA only: no DNS, unspecified, public, multicast,
            // IPv4-mapped or scope-dependent address may expand the test topology.
            let private = addr.ip().is_loopback() || addr.ip().octets()[0] & 0xfe == 0xfc;
            if !private || addr.port() == 0 || addr.flowinfo() != 0 || addr.scope_id() != 0 {
                return Err(invalid(
                    "peer/coordinator addresses must be exact IPv6 loopback or private ULA with nonzero ports",
                ));
            }
        }
        let unique: std::collections::BTreeSet<_> = self.peers.iter().collect();
        if unique.len() != self.peers.len() {
            return Err(invalid("duplicate peer endpoint"));
        }
        let mut parent = FatPointerToBftBlock::from_parts(
            zcash_primitives::bft::Blake3Hash([1; 32]),
            1,
            0,
            &vec![
                zcash_primitives::bft::FatPointerSignature {
                    pub_key: PubKeyID([1; 32]),
                    vote_signature: [0; 64],
                };
                self.nodes
            ],
        );
        // Check size arithmetic before allocating any solution padding. A full-N
        // parent must fit even if the actual chain exports only a quorum.
        solution_lengths(&parent, self)?;
        if self.body_bytes > 0 {
            let quorum = crate::decision_threshold(self.nodes as u64, true) as usize;
            while parent.signatures.len() > quorum {
                parent.signatures.pop();
                solution_lengths(&parent, self)?;
            }
            solution_lengths(&FatPointerToBftBlock::null(), self)?;
        }
        Ok(())
    }
    /// Bounded regular-file input; unknown fields and invalid geometry are errors.
    pub fn load(path: &std::path::Path) -> io::Result<Self> {
        use std::io::Read;
        let file = std::fs::File::open(path)?;
        if !file.metadata()?.is_file() {
            return Err(invalid("distributed configuration must be a regular file"));
        }
        let mut bytes = Vec::new();
        file.take(CONTROL_LIMIT as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > CONTROL_LIMIT {
            return Err(invalid("configuration exceeds 64KiB"));
        }
        let config: Self = serde_json::from_slice(&bytes).map_err(|e| invalid(e.to_string()))?;
        config.validate()?;
        Ok(config)
    }
}

fn padded_header(height: u32, index: usize, bytes: usize) -> BlockHeaderData {
    let mut h = header(height, index);
    // Neutral deterministic payload; no claim of valid Equihash work.
    h.solution = vec![0x28; bytes];
    h
}

/// Largest solution whose encoded growth over an empty solution fits `budget`.
/// The unchanged Vector codec uses CompactSize (1/3/5 bytes here), bounded by
/// 32 MiB per vector. A 40 MiB body fits across the runner's 32 headers; the
/// legacy single-header path must still respect the unchanged vector bound.
fn padding_at_most(budget: usize) -> (usize, usize) {
    match budget {
        0..=252 => (budget, budget),
        253..=254 => (252, 252),
        255..=65537 => (budget - 2, budget),
        65538..=65539 => (65535, 65537),
        _ => (budget - 4, budget),
    }
}

/// Bounded arithmetic/metadata only: no solution Vec is allocated until the
/// whole target (including the actual parent certificate) has been checked.
fn solution_lengths(parent: &FatPointerToBftBlock, c: &DistributedConfig) -> io::Result<Vec<usize>> {
    c.validate_body_shape()?;
    if parent.signatures.len() > c.nodes {
        return Err(invalid("parent exceeds full roster"));
    }
    // Derive fixed overhead from the existing serializers, not a second format.
    let base = mock_body(2, parent.clone(), 0)?.0.len() + payload_size(c.headers)? as usize;
    if c.body_bytes == 0 {
        let prefix_growth = if c.solution_bytes < 253 { 0 } else { 2 };
        if base + c.headers * (c.solution_bytes + prefix_growth) > c.max_body {
            return Err(invalid("padded headers plus full-roster parent certificate exceed max_body"));
        }
        return Ok(vec![c.solution_bytes; c.headers]);
    }
    let extra = c.body_bytes.checked_sub(base)
        .ok_or_else(|| invalid("body_bytes is smaller than headers plus parent certificate"))?;
    if c.headers == 1 {
        let (length, used) = padding_at_most(extra);
        // zcash_encoding::MAX_COMPACT_SIZE is an unchanged 32 MiB per Vector.
        // With >=2 headers, the 40 MiB whole-body bound keeps each below it.
        if length > 32 * 1024 * 1024 {
            return Err(invalid("single-header solution exceeds the 32MiB Vector limit"));
        }
        return if used == extra { Ok(vec![length]) } else {
            Err(invalid("body_bytes falls in a CompactSize gap; use at least two headers"))
        };
    }
    // Balance the first N-2 headers, then solve the last pair. Two headers can
    // bridge either two-byte CompactSize gap without changing the target size.
    let (each, used) = padding_at_most(extra / c.headers);
    let mut lengths = vec![each; c.headers];
    let remaining = extra - used * (c.headers - 2);
    for adjustment in 0..=4 {
        let (a, a_used) = padding_at_most((remaining / 2).saturating_sub(adjustment));
        let (b, b_used) = padding_at_most(remaining - a_used);
        if a_used + b_used == remaining {
            lengths[c.headers - 2] = a;
            lengths[c.headers - 1] = b;
            return Ok(lengths);
        }
    }
    Err(invalid("cannot represent exact padding target"))
}

fn body(
    height: u32,
    parent: FatPointerToBftBlock,
    c: &DistributedConfig,
) -> io::Result<BlockValue> {
    let lengths = solution_lengths(&parent, c)?;
    let value = BlockValue(
        BftBlock {
            version: 2,
            height,
            previous_block_fat_ptr: parent,
            headers: lengths.into_iter().enumerate()
                .map(|(i, bytes)| padded_header(height, i, bytes))
                .collect(),
            slashforks: vec![],
            do_not_include_until_bc_height: 0,
        }
        .zcash_serialize_to_vec()?,
    );
    if value.0.len() > c.max_body || (c.body_bytes > 0 && value.0.len() != c.body_bytes) {
        return Err(invalid("serialized mock body does not match its validated target"));
    }
    Ok(value)
}

fn payload_bytes(block: &BftBlock) -> io::Result<u64> {
    struct Counter(u64);
    impl io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0 += bytes.len() as u64;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> { Ok(()) }
    }
    let mut counter = Counter(0);
    for h in &block.headers {
        BlockHeader::write_data(h, &mut counter)?;
    }
    Ok(counter.0)
}

fn valid_body(
    value: &BlockValue,
    height: u64,
    parent: ValueId,
    c: &DistributedConfig,
    members: &[SortedRosterMember],
) -> bool {
    checked_body_payload(value, height, parent, c, members).is_some()
}

/// Validate once and return actual serialized header bytes, not the target body
/// size. In byte mode this payload varies with the parent certificate's length.
fn checked_body_payload(
    value: &BlockValue,
    height: u64,
    parent: ValueId,
    c: &DistributedConfig,
    members: &[SortedRosterMember],
) -> Option<u64> {
    c.validate_body_shape().ok()?;
    if value.0.len() > c.max_body || (c.body_bytes > 0 && value.0.len() != c.body_bytes) {
        return None;
    }
    let mut bytes = &value.0[..];
    let block = BftBlock::zcash_deserialize(&mut bytes).ok()?;
    let lengths = solution_lengths(&block.previous_block_fat_ptr, c).ok()?;
    if !bytes.is_empty()
        || block.version != 2
        || u64::from(block.height) != height
        || !block.slashforks.is_empty()
        || block.do_not_include_until_bc_height != 0
        || block.headers.len() != c.headers
        || !block.headers.iter().zip(lengths).enumerate()
            .all(|(i, (h, length))| *h == padded_header(block.height, i, length))
        || block.zcash_serialize_to_vec().ok().as_deref() != Some(value.0.as_slice())
    {
        return None;
    }
    let valid_parent = if height == 1 {
        block.previous_block_fat_ptr == FatPointerToBftBlock::null()
    } else {
        verify_pointer(&block.previous_block_fat_ptr, members, height - 1, parent, &c.namespace())
    };
    valid_parent.then(|| payload_bytes(&block).ok()).flatten()
}

#[derive(Clone, Debug, Serialize)]
pub struct LocalHeight {
    pub height: u64,
    pub measured: bool,
    pub id: String,
    pub body_bytes: u64,
    pub payload_bytes: u64,
    pub latency_ms: f64,
    pub round: u64,
    pub at_ms: f64,
    pub start_at_ms: f64,
    pub export_host_check_ms: f64,
    pub ack_wait_ms: Option<f64>,
    pub acknowledged: bool,
}
#[derive(Clone, Debug, Serialize)]
pub struct SystemSnapshot {
    pub at_ms: f64,
    pub sampling_duration_ms: f64,
    /// Cumulative counters, except process.rss_bytes and process.max_rss_bytes (gauges).
    pub counters: BTreeMap<String, u64>,
    pub errors: Vec<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct Snapshot {
    #[serde(flatten)]
    pub counters: CounterSnapshot,
    pub system: SystemSnapshot,
}
#[derive(Clone, Debug, Serialize)]
pub struct Window {
    pub start: Snapshot,
    pub end: Snapshot,
    pub application_delta: BTreeMap<String, u64>,
    pub transport_delta: BTreeMap<String, u64>,
    /// Only monotonic counters present at both boundaries; RSS gauges excluded.
    pub system_delta: BTreeMap<String, u64>,
    pub store_replacements: u64,
    pub store_replacement_bytes: u64,
}
fn window(start: &Snapshot, end: &Snapshot) -> Window {
    let base = counter_window(&start.counters, &end.counters);
    let system_delta = end
        .system
        .counters
        .iter()
        .filter_map(|(key, value)| {
            if key.ends_with("rss_bytes") || key.ends_with("ticks_per_second") {
                return None;
            }
            let delta = value.checked_sub(*start.system.counters.get(key)?)?;
            Some((key.clone(), delta))
        })
        .collect();
    Window {
        start: start.clone(),
        end: end.clone(),
        application_delta: base.application_delta,
        transport_delta: base.transport_delta,
        system_delta,
        store_replacements: base.store_replacements,
        store_replacement_bytes: base.store_replacement_bytes,
    }
}
fn sample(sampler: &Sampler, origin: Instant) -> io::Result<Snapshot> {
    Ok(Snapshot {
        counters: sampler.snapshot(origin)?,
        system: system_snapshot(origin),
    })
}
fn system_snapshot(origin: Instant) -> SystemSnapshot {
    let at = Instant::now();
    let mut out = SystemSnapshot {
        at_ms: milliseconds(at.duration_since(origin)),
        sampling_duration_ms: 0.0,
        counters: BTreeMap::new(),
        errors: vec![],
    };
    #[cfg(target_os = "linux")]
    {
        let mut read = |path: &str| match std::fs::read_to_string(path) {
            Ok(text) => Some(text),
            Err(error) => {
                out.errors.push(format!("{path}: {error}"));
                None
            }
        };
        if let Some(text) = read("/proc/net/dev") {
            for line in text.lines() {
                let Some((name, rest)) = line.split_once(':') else {
                    continue;
                };
                let name = name.trim();
                if name == "lo" {
                    continue;
                }
                let values: Vec<_> = rest.split_whitespace().collect();
                for (label, index) in [
                    ("rx_bytes", 0),
                    ("rx_packets", 1),
                    ("tx_bytes", 8),
                    ("tx_packets", 9),
                ] {
                    if let Some(value) = values.get(index).and_then(|v| v.parse().ok()) {
                        out.counters
                            .insert(format!("network.{name}.{label}"), value);
                    }
                }
            }
        }
        if let Some(text) = read("/proc/stat") {
            if let Some(line) = text.lines().find(|s| s.starts_with("cpu ")) {
                let values: Vec<u64> = line
                    .split_whitespace()
                    .skip(1)
                    .filter_map(|x| x.parse().ok())
                    .collect();
                // Guest/guest_nice already included in user/nice, not counted twice.
                out.counters
                    .insert("cpu.total_ticks".into(), values.iter().take(8).sum());
                for (label, index) in [("user", 0), ("system", 2), ("steal", 7)] {
                    if let Some(value) = values.get(index) {
                        out.counters.insert(format!("cpu.{label}_ticks"), *value);
                    }
                }
            }
        }
        let cgroup_v2 = std::path::Path::new("/sys/fs/cgroup/cpu.stat").is_file();
        let cpu_stat = if cgroup_v2 {
            "/sys/fs/cgroup/cpu.stat"
        } else {
            "/sys/fs/cgroup/cpu,cpuacct/cpu.stat"
        };
        if let Some(text) = read(cpu_stat) {
            for line in text.lines() {
                let mut words = line.split_whitespace();
                if let (Some(k), Some(v)) =
                    (words.next(), words.next().and_then(|v| v.parse().ok()))
                {
                    out.counters.insert(format!("cgroup.{k}"), v);
                }
            }
        }
        if !cgroup_v2 {
            if let Some(text) = read("/sys/fs/cgroup/cpu,cpuacct/cpuacct.usage") {
                if let Ok(ns) = text.trim().parse() {
                    out.counters.insert("cgroup.usage_nsec".into(), ns);
                }
            }
        }
        if let Some(text) = read("/proc/self/stat") {
            if let Some((_, fields)) = text.rsplit_once(')') {
                let fields: Vec<_> = fields.split_whitespace().collect();
                for (label, index) in [("user_ticks", 11), ("system_ticks", 12)] {
                    if let Some(v) = fields.get(index).and_then(|v| v.parse().ok()) {
                        out.counters.insert(format!("process.{label}"), v);
                    }
                }
                // These sysconf queries have no pointers and do not change process state.
                let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
                let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
                if ticks > 0 {
                    out.counters
                        .insert("process.ticks_per_second".into(), ticks as u64);
                }
                if let Some(pages) = fields.get(21).and_then(|v| v.parse::<u64>().ok()) {
                    if page_size > 0 {
                        out.counters
                            .insert("process.rss_bytes".into(), pages * page_size as u64);
                    }
                }
            }
        }
        if let Some(text) = read("/proc/self/status") {
            if let Some(kib) = text.lines().find_map(|l| {
                l.strip_prefix("VmHWM:")?
                    .split_whitespace()
                    .next()?
                    .parse::<u64>()
                    .ok()
            }) {
                out.counters
                    .insert("process.max_rss_bytes".into(), kib * 1024);
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    out.errors
        .push("system counters require Linux procfs/cgroup v2".into());
    out.sampling_duration_ms = milliseconds(at.elapsed());
    out
}

#[derive(Clone, Debug, Serialize)]
pub struct LocalSummary {
    pub measured_heights: usize,
    pub body_bytes: u64,
    pub payload_bytes: u64,
    pub elapsed_seconds: Option<f64>,
    pub latency_ms: Option<Percentiles>,
    pub latency_samples: usize,
    pub ack_wait_ms: Option<Percentiles>,
}
#[derive(Clone, Debug, Serialize)]
pub struct DistributedReport {
    pub schema_version: u32,
    pub mode: &'static str,
    pub config: DistributedConfig,
    pub benchmark_namespace: String,
    pub status: &'static str,
    pub failure: Option<String>,
    pub total_run_seconds: f64,
    pub durable_image_bytes: usize,
    pub heights: Vec<LocalHeight>,
    pub summary: LocalSummary,
    pub measured_start: Option<Snapshot>,
    pub measured_end: Option<Snapshot>,
    pub measured_window: Option<Window>,
    pub chatter_window: Option<Window>,
    pub process_start: SystemSnapshot,
    pub process_end: SystemSnapshot,
    pub runtime_settings: BTreeMap<String, u64>,
    pub transport_settings: String,
    pub methodology: Vec<&'static str>,
}

struct Pending {
    id: ValueId,
    pointer: FatPointerToBftBlock,
    ack: oneshot::Sender<HostTip>,
}
struct LocalState {
    config: DistributedConfig,
    members: Vec<SortedRosterMember>,
    committed: u64,
    id: ValueId,
    parent: FatPointerToBftBlock,
    origin: Instant,
    height_start: Instant,
    reports: Vec<LocalHeight>,
    pending: Option<Pending>,
    failure: Option<String>,
}
impl LocalState {
    fn tip(&self) -> HostTip {
        HostTip {
            height: self.committed,
            block_id: self.id,
            next_roster: self.members.clone(),
            next_vote_namespace: self.config.namespace(),
            next_strict_quorum: true,
        }
    }
}
fn host(state: Arc<Mutex<LocalState>>) -> HostCallbacks {
    let propose = state.clone();
    let validate = state.clone();
    let pushed = state.clone();
    HostCallbacks {
        propose: ClosureToProposeNewBlock(Arc::new(move || {
            let c = propose.lock().unwrap();
            let result = body((c.committed + 1) as u32, c.parent.clone(), &c.config).ok();
            Box::pin(async move { result })
        })),
        validate: ClosureToValidateProposedBlock(Arc::new(move |body| {
            let c = validate.lock().unwrap();
            let valid = valid_body(body, c.committed + 1, c.id, &c.config, &c.members);
            Box::pin(async move {
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
            // True callback entry, before locking, full-body parsing or certificate verification.
            let entered = Instant::now();
            let (tx, rx) = oneshot::channel();
            let mut c = pushed.lock().unwrap();
            let height = c.committed + 1;
            let id = ValueId(HashKeys::default().value_id.hash(&body.0));
            let payload = checked_body_payload(&body, height, c.id, &c.config, &c.members);
            if c.pending.is_some()
                || !chunks.is_empty()
                || payload.is_none()
                || !verify_pointer(&pointer, &c.members, height, id, &c.config.namespace())
            {
                c.failure = Some(format!(
                    "invalid or duplicate local body/certificate at height {height}"
                ));
            } else {
                let report = LocalHeight {
                    height,
                    measured: height > u64::from(c.config.warmup),
                    id: hex::encode(id.0),
                    body_bytes: body.0.len() as u64,
                    payload_bytes: payload.expect("validated shape"),
                    latency_ms: milliseconds(entered.duration_since(c.height_start)),
                    round: pointer.get_vote_template().round as u64,
                    at_ms: milliseconds(entered.duration_since(c.origin)),
                    start_at_ms: milliseconds(c.height_start.duration_since(c.origin)),
                    export_host_check_ms: milliseconds(entered.elapsed()),
                    ack_wait_ms: None,
                    acknowledged: false,
                };
                c.reports.push(report);
                c.pending = Some(Pending {
                    id,
                    pointer,
                    ack: tx,
                });
            }
            Box::pin(async move {
                match rx.await {
                    Ok(tip) => tip,
                    // Aborted/failing benchmark must never manufacture a trusted tip.
                    Err(_) => std::future::pending().await,
                }
            })
        })),
        read_tip: ClosureToReadDecidedTip(Arc::new(move |height| {
            let c = state.lock().unwrap();
            (height <= c.committed).then(|| c.tip())
        })),
        update_peers: ClosureToUpdatePeers(Arc::new(|_| Box::pin(async {}))),
        access: AccessD28(Arc::new(|_, _| {})),
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
enum Control {
    #[serde(rename = "start")]
    Start {},
    #[serde(rename = "ack")]
    Ack { height: u64 },
}
/// The benchmark backend can hit its deadline and return Storage before the
/// outer timeout wins. Add observed deadline context, never replace the cause
/// or infer that a pre-deadline storage error was caused by the deadline.
fn runtime_failure(
    outcome: &Result<io::Result<()>, tokio::task::JoinError>,
    now: Instant,
    deadline: Instant,
) -> String {
    let cause = format!("D28 runtime exited: {outcome:?}");
    if now >= deadline {
        format!("whole-process benchmark deadline reached; {cause}")
    } else {
        cause
    }
}

async fn bounded<T>(
    deadline: Instant,
    future: impl std::future::Future<Output = io::Result<T>>,
) -> io::Result<T> {
    tokio::time::timeout_at(tokio::time::Instant::from_std(deadline), future)
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "whole-process benchmark deadline reached",
            )
        })?
}
async fn read_control(stream: &mut BufReader<TcpStream>, deadline: Instant) -> io::Result<Control> {
    let mut frame = vec![];
    let mut limited = stream.take(CONTROL_LIMIT as u64 + 1);
    bounded(deadline, limited.read_until(b'\n', &mut frame)).await?;
    if frame.len() > CONTROL_LIMIT {
        return Err(invalid("control frame exceeds 64KiB"));
    }
    if frame.last() != Some(&b'\n') {
        return Err(invalid(
            "coordinator closed with missing/truncated control frame",
        ));
    }
    serde_json::from_slice(&frame).map_err(|e| invalid(format!("invalid control frame: {e}")))
}
async fn send(
    stream: &mut BufReader<TcpStream>,
    value: serde_json::Value,
    deadline: Instant,
) -> io::Result<()> {
    let mut frame = serde_json::to_vec(&value).map_err(|e| invalid(e.to_string()))?;
    frame.push(b'\n');
    if frame.len() > CONTROL_LIMIT {
        return Err(invalid("outgoing control frame exceeds 64KiB"));
    }
    bounded(deadline, stream.get_mut().write_all(&frame)).await
}

/// Run one validator; the caller supplies an enabled Tokio runtime with the
/// configured worker count. Valid input always yields a complete/partial report.
pub async fn run(config: DistributedConfig) -> io::Result<DistributedReport> {
    let origin = Instant::now();
    let deadline = origin + Duration::from_secs(config.timeout_secs.min(86_400));
    config.validate()?;
    let process_start = system_snapshot(origin);
    let image_size = durable::image_size(durable::Limits {
        max_members: config.nodes,
        max_body: config.max_body,
    })
    .map_err(|_| invalid("invalid durable geometry"))?;
    let state = Arc::new(Mutex::new(LocalState {
        config: config.clone(),
        members: roster(config.nodes),
        committed: 0,
        id: ValueId::NIL,
        parent: FatPointerToBftBlock::null(),
        origin,
        height_start: origin,
        reports: vec![],
        pending: None,
        failure: None,
    }));
    let mut runtimes = tokio::task::JoinSet::new();
    let mut sampler = Sampler {
        observers: vec![],
        stores: vec![],
        scope: None,
        deadline,
        metrics: vec![],
    };
    let mut measured_start = None;
    let mut measured_end = None;
    let mut chatter_end = None;
    let mut runtime_settings = BTreeMap::new();
    let limits = bench_transport_limits(config.nodes, config.max_body)?;
    let transport_settings = format!("{limits:?}; send_buffer_params=(1000000,524288,8388608)");
    let result = bounded(deadline, async {
        let identities: Vec<_> = (0..config.nodes).map(|i| {
            crate::new_keypair_from_connect_magic1_with_seed(
                crate::CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2b, seed("transport", i))
                .ok_or_else(|| invalid("STP identity construction failed"))
        }).collect::<io::Result<_>>()?;
        let addresses: Vec<_> = identities.iter().zip(&config.peers)
            .map(|(key, addr)| STPAddress::from(*addr.ip(), addr.port(), key)).collect();
        let own = config.peers[config.node];
        // Return ordinary bind errors rather than the underlying STP constructor's
        // panic. There remains a small close/rebind race in that infallible API.
        drop(UdpSocket::bind(own)?);
        let transport = SharedTransport::with_bind_ip(vec![identities[config.node].clone()],
            own.port(), None, (1_000_000, 512 * 1024, 8 << 20), limits, IpAddr::V6(*own.ip()));
        transport.dial_only(addresses.iter().map(STPAddress::connection_key).collect());
        let counts = Arc::new(StoreCounts::default());
        let metrics = Arc::new(runtime::RuntimeMetrics::default());
        sampler.observers.push(transport.attach(Consumer::Base));
        sampler.stores.push(counts.clone());
        sampler.metrics.push(metrics.clone());
        let mut rt = RuntimeConfig::new(1, roster(config.nodes), config.namespace(), config.nodes, transport,
            addresses.into_iter().enumerate().filter_map(|(i, a)| (i != config.node).then_some(a)).collect());
        rt.max_body = config.max_body;
        rt.payload_mode = runtime_payload_mode(&config.payload_mode, config.payload_leaf_bytes)?;
        config.apply_timing(&mut rt.timeouts, &mut rt.request_timeout, &mut rt.status_ttl);
        rt.poll_interval = Duration::from_millis(config.poll_ms);
        rt.status_interval = Duration::from_millis(config.status_ms);
        rt.max_receive_per_tick = config.receive_budget;
        rt.max_requests = config.max_requests;
        rt.max_outgoing = config.max_outgoing;
        rt.metrics = Some(metrics);
        macro_rules! settings { ($($field:ident),*) => { $(runtime_settings.insert(stringify!($field).into(), rt.$field as u64);)* }; }
        settings!(rho, request_timeout, status_ttl, max_connections, max_requests, max_outgoing, max_incoming, max_receive_per_tick, max_body);
        runtime_settings.insert("poll_interval_ms".into(), config.poll_ms);
        runtime_settings.insert("status_interval_ms".into(), config.status_ms);
        for (name, value) in [("propose", rt.timeouts.propose), ("prevote", rt.timeouts.prevote),
            ("precommit", rt.timeouts.precommit), ("inc_propose", rt.timeouts.inc_propose),
            ("inc_prevote", rt.timeouts.inc_prevote), ("inc_precommit", rt.timeouts.inc_precommit)] {
            runtime_settings.insert(format!("timeouts.{name}_ms"), value);
        }
        let stream = bounded(deadline, TcpStream::connect(config.coordinator)).await?;
        stream.set_nodelay(true)?;
        let mut stream = BufReader::new(stream);
        send(&mut stream, serde_json::json!({"type":"ready", "run_id":config.run_id, "node":config.node,
            "payload_mode":config.payload_mode, "payload_leaf_bytes":config.payload_leaf_bytes}), deadline).await?;
        if !matches!(read_control(&mut stream, deadline).await?, Control::Start {}) {
            return Err(invalid("expected start before any ACK"));
        }
        if config.warmup == 0 { measured_start = Some(sample(&sampler, origin)?); }
        state.lock().unwrap().height_start = Instant::now();
        runtimes.spawn(runtime::run(key(config.node), MemoryBackend { image: vec![0; image_size], initialized: false,
            delay: Duration::ZERO, deadline, counts }, rt, host(state.clone())));
        let total = u64::from(config.heights + config.warmup);
        for height in 1..=total {
            let decision = loop {
                {
                    let c = state.lock().unwrap();
                    if let Some(failure) = &c.failure { return Err(invalid(failure.clone())); }
                    if c.pending.is_some() { break c.reports.last().expect("pending report").clone(); }
                }
                if let Some(outcome) = runtimes.try_join_next() {
                    return Err(invalid(runtime_failure(&outcome, Instant::now(), deadline)));
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            };
            if decision.height != height { return Err(invalid("unexpected local decision height")); }
            send(&mut stream, serde_json::json!({"type":"decision", "run_id":config.run_id,
                "node":config.node, "height":height, "id":decision.id, "body_bytes":decision.body_bytes,
                "payload_bytes":decision.payload_bytes, "latency_ms":decision.latency_ms, "round":decision.round}), deadline).await?;
            match read_control(&mut stream, deadline).await? {
                Control::Ack { height: h } if h == height => {},
                _ => return Err(invalid(format!("expected ACK for height {height}"))),
            }
            {
                let mut c = state.lock().unwrap();
                let now = Instant::now();
                let report = c.reports.last_mut().unwrap();
                report.ack_wait_ms = Some(milliseconds(now.duration_since(origin)) - report.at_ms);
                report.acknowledged = true;
            }
            if height == total {
                measured_end = Some(sample(&sampler, origin)?);
                // Final ACK ends measurement but does NOT publish a next tip or
                // release the callback. Keep final-height evidence during chatter.
                if config.chatter_secs > 0 {
                    tokio::time::sleep(Duration::from_secs(config.chatter_secs)).await;
                    chatter_end = Some(sample(&sampler, origin)?);
                }
                return Ok(());
            }
            if height == u64::from(config.warmup) { measured_start = Some(sample(&sampler, origin)?); }
            let mut c = state.lock().unwrap();
            let pending = c.pending.take().expect("locally verified pending decision");
            c.committed = height;
            c.id = pending.id;
            c.parent = pending.pointer;
            c.height_start = Instant::now();
            let _ = pending.ack.send(c.tip());
        }
        unreachable!()
    }).await;
    let mut failure = result.err().map(|e| e.to_string());
    // Preserve duration/counters even when not one decision was possible.
    if measured_start.is_some() && measured_end.is_none() {
        match sample(&sampler, origin) {
            Ok(s) => measured_end = Some(s),
            Err(e) => {
                failure.get_or_insert(format!("partial boundary sampling: {e}"));
            }
        }
    } else if measured_end.is_some() && config.chatter_secs > 0 && chatter_end.is_none() {
        chatter_end = sample(&sampler, origin).ok();
    }
    runtimes.abort_all();
    while let Some(outcome) = runtimes.join_next().await {
        if !matches!(outcome, Err(ref e) if e.is_cancelled()) {
            failure.get_or_insert_with(|| runtime_failure(&outcome, Instant::now(), deadline));
        }
    }
    drop(sampler); // Drop final transport observer: close this process's STP endpoint.
    let process_end = system_snapshot(origin);
    let heights = state.lock().unwrap().reports.clone();
    let committed: Vec<_> = heights
        .iter()
        .filter(|h| h.measured && h.acknowledged)
        .collect();
    let latencies: Vec<_> = heights
        .iter()
        .filter(|h| h.measured)
        .map(|h| h.latency_ms)
        .collect();
    let waits: Vec<_> = committed.iter().filter_map(|h| h.ack_wait_ms).collect();
    let measured_window = measured_start
        .as_ref()
        .zip(measured_end.as_ref())
        .map(|(a, b)| window(a, b));
    let elapsed_seconds = measured_window
        .as_ref()
        .map(|w| (w.end.counters.at_ms - w.start.counters.at_ms) / 1000.0);
    let summary = LocalSummary {
        measured_heights: committed.len(),
        body_bytes: committed.iter().map(|h| h.body_bytes).sum(),
        payload_bytes: committed.iter().map(|h| h.payload_bytes).sum(),
        elapsed_seconds,
        latency_ms: percentiles(&latencies),
        latency_samples: latencies.len(),
        ack_wait_ms: percentiles(&waits),
    };
    let complete = failure.is_none() && summary.measured_heights == config.heights as usize;
    Ok(DistributedReport {
        schema_version: 1,
        mode: "distributed",
        benchmark_namespace: hex::encode(config.namespace()),
        config,
        status: if complete { "complete" } else { "partial" },
        failure,
        total_run_seconds: origin.elapsed().as_secs_f64(),
        durable_image_bytes: image_size,
        heights,
        summary,
        chatter_window: measured_end
            .as_ref()
            .zip(chatter_end.as_ref())
            .map(|(a, b)| window(a, b)),
        measured_start,
        measured_end,
        measured_window,
        process_start,
        process_end,
        runtime_settings,
        transport_settings,
        methodology: vec![
            "Payload mode/leaf size bind readiness and the benchmark signature/wire namespace; HostTip and parent/export certificate validation use that same namespace at every height. Body, PoW and ledger serialization are unchanged.",
            "Payload metrics are application offers, not delivery or wire bytes. Raw payload_bytes includes embedded inline bodies or outlined data and is a subset, not extra offered traffic. outlined_evidence_bytes is already in offered.evidence.bytes. payload.offered_control/data are separate offers; duplicate_payload_bytes is not total wasted network traffic.",
            "One real D28/STP validator per process; fixed full equal-stake roster, predictable isolated benchmark keys/namespace; no PoW, ledger or transaction/TPS claim.",
            "All-online controller ACK barrier is intentional benchmark overhead: both callback return and trusted read_tip wait for ACK. Bodies and certificates come only from local D28 export and unchanged production parsing/signature checks, never from controller.",
            "Node latency uses one local monotonic clock: START to push_decided entry at height1, prior ACK/tip-publication to push_decided entry thereafter. Includes INIT, storage and host polling; excludes local export checks and ACK wait. Never subtract clocks across machines; controller receipt/cycle metrics are separate.",
            "Measured counter window starts before first measured height publication and ends after final ACK: includes global barrier overhead, excludes warmup and optional separate chatter. Partial zero-decision duration is retained. ACK wait is callback entry to ACK receipt, including host validation/control overhead.",
            "Optional chatter retains final-height evidence after final ACK without next-height publication; not idle traffic of a quiescent chain. Shutdown/in-flight tail excluded from measured counters.",
            "Application offered bytes are not wire bytes; transport ring messages are not UDP packets. /proc/net/dev includes TX/RX of every non-lo interface and all VM traffic, including controller/management; not isolated D28 amplification. Serial snapshots have sampling skew.",
            "Host CPU/steal/cgroup counters include other VM processes. Process CPU is in reported clock ticks; RSS/maxRSS are gauges (not deltas). Missing system metrics carry errors, never fabricated zeros. In-memory atomic image is not fsync/crash-durable storage.",
            "Synthetic solution padding uses unchanged BftBlock/header Vector serialization; it is NOT valid Equihash work or a production-valid ledger body. body_bytes targets the exact whole serialized body; payload_bytes counts only actual serialized headers and can vary with parent certificate length.",
            "Production defaults and core formats unchanged. Explicit benchmark timing_profile=wan/wan_fast changes only base consensus/request/status-expiry timers (wan_fast uses 2500/1500/2000ms consensus bases with WAN request/status TTL); rho and round increments stay unchanged. Scheduling knobs, body cap and benchmark transport/ring overrides are recorded. Memory bounds here are not a whole-process RSS guarantee.",
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> DistributedConfig {
        serde_json::from_value(serde_json::json!({"node":0,"nodes":4,"offline":0,
            "headers":32,"heights":2,"warmup":1,"timeout_secs":10,"workers":1,
            "max_body":1048576,"chatter_secs":0,"peers":["[::1]:9000","[::1]:9001","[::1]:9002","[::1]:9003"],
            "coordinator":"[::1]:9100","run_id":"unit-test"})).unwrap()
    }
    #[test]
    fn payload_config_defaults_strict_bounds_and_host_namespace() {
        let legacy = config();
        assert_eq!(legacy.payload_mode, "inline");
        assert_eq!(legacy.payload_leaf_bytes, 0);
        assert_eq!(legacy.namespace(), NAMESPACE);
        for (mode, leaf) in [("inline", 0), ("outlined", 0), ("merkle", 102400)] {
            let mut c = config();
            c.payload_mode = mode.into();
            c.payload_leaf_bytes = leaf;
            c.validate().unwrap();
            let json = serde_json::to_value(&c).unwrap();
            let decoded: DistributedConfig = serde_json::from_value(json).unwrap();
            assert_eq!(decoded.namespace(), c.namespace());
            let id = ValueId([7; 32]);
            let parent = super::super::tests::pointer_with_namespace(4, &[0, 1, 2], 1, id, &c.namespace());
            let value = body(2, parent.clone(), &c).unwrap();
            assert!(valid_body(&value, 2, id, &c, &roster(4)));
            let state = LocalState { config: c.clone(), members: roster(4), committed: 1, id, parent,
                origin: Instant::now(), height_start: Instant::now(), reports: vec![], pending: None, failure: None };
            assert_eq!(state.tip().next_vote_namespace, c.namespace());
            assert_eq!(c.base().namespace(), c.namespace());
            if mode != "inline" {
                assert!(!valid_body(&value, 2, id, &legacy, &roster(4)));
            }
        }
        for (mode, leaf) in [("unknown", 0), ("inline", 102400), ("outlined", 1),
                             ("merkle", 0), ("merkle", 32767), ("merkle", 1048577)] {
            let mut c = config();
            c.payload_mode = mode.into();
            c.payload_leaf_bytes = leaf;
            assert!(c.validate().is_err());
        }
        for (key, value) in [("leaf_bytes", serde_json::json!(102400)),
                             ("payload_leaf_bytes", serde_json::json!(true)),
                             ("payload_mode", serde_json::json!(1))] {
            let mut json = serde_json::to_value(config()).unwrap();
            json[key] = value;
            assert!(serde_json::from_value::<DistributedConfig>(json).is_err());
        }
    }
    #[test]
    fn executor_workers_are_bounded_explicit_and_roundtrip() {
        for workers in 1..=4 {
            let mut c = config();
            c.workers = workers;
            c.validate().unwrap();
            assert_eq!(c.base().workers, workers);
            let json = serde_json::to_value(&c).unwrap();
            assert_eq!(json["workers"], workers);
            let decoded: DistributedConfig = serde_json::from_value(json).unwrap();
            decoded.validate().unwrap();
            assert_eq!(decoded.workers, workers);
        }
        for workers in [0, 5, 128, usize::MAX] {
            let mut c = config();
            c.workers = workers;
            assert!(c.validate().is_err());
        }
        for field in ["workers", "max_body"] {
            let mut json = serde_json::to_value(config()).unwrap();
            json.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<DistributedConfig>(json).is_err());
        }
        for value in [serde_json::json!(true), serde_json::json!(-1),
                      serde_json::json!(1.5), serde_json::json!("2"), serde_json::Value::Null] {
            let mut json = serde_json::to_value(config()).unwrap();
            json["workers"] = value;
            assert!(serde_json::from_value::<DistributedConfig>(json).is_err());
        }
        // Do not narrow the older single-process library/CLI worker range.
        assert!(BenchConfig { workers: 128, ..Default::default() }.validate().is_ok());
    }
    #[test]
    fn forty_mib_merkle_geometry_is_bounded_for_100k_and_32k_leaves() {
        assert_eq!(BENCH_MAX_BODY, 41_943_040);
        for (leaf_bytes, count, bitmap, proof) in [(102400, 410, 52, 288), (32768, 1280, 160, 352)] {
            let mut c = byte_config(BENCH_MAX_BODY);
            c.payload_mode = "merkle".into();
            c.payload_leaf_bytes = leaf_bytes;
            c.validate().unwrap();
            let layout = crate::d28::merkle::Manifest {
                body_id: [0; 32], signed_header_identity: [0; 32], root: [0; 32],
                body_len: c.body_bytes as u64, leaf_bytes: leaf_bytes as u32,
            }.validate(c.max_body).unwrap();
            assert_eq!(layout.leaf_count(), count);
            assert_eq!(layout.bitmap_bytes(), bitmap);
            assert_eq!(layout.proof_bytes(), proof);
            assert!(u32::try_from(layout.leaf_count()).is_ok()); // Runtime wire index bound.
            let json = serde_json::to_value(&c).unwrap();
            let decoded: DistributedConfig = serde_json::from_value(json).unwrap();
            decoded.validate().unwrap();
            assert_eq!((decoded.body_bytes, decoded.max_body), (BENCH_MAX_BODY, BENCH_MAX_BODY));
        }
    }
    #[test]
    fn shape_and_private_topology_bounds() {
        let mut c = config();
        assert!(c.validate().is_ok());
        assert_eq!(
            (
                c.poll_ms,
                c.status_ms,
                c.receive_budget,
                c.max_requests,
                c.max_outgoing
            ),
            (25, 100, 32, 2, 2)
        );
        c.headers = 2048;
        c.solution_bytes = 320;
        assert!(c.validate().is_ok());
        let b = body(1, FatPointerToBftBlock::null(), &c).unwrap();
        assert!(b.0.len() > 1_000_000 && b.0.len() < 1_048_576);
        assert!(valid_body(&b, 1, ValueId::NIL, &c, &roster(4)));
        assert_eq!(checked_body_payload(&b, 1, ValueId::NIL, &c, &roster(4)), Some(509 * 2048));
        c.solution_bytes = 321;
        assert!(!valid_body(&b, 1, ValueId::NIL, &c, &roster(4)));
        c.headers = 2049;
        assert!(c.validate().is_err());
        c = config();
        c.peers[0] = "[2001:db8::1]:9000".parse().unwrap();
        assert!(c.validate().is_err());
        c = config();
        c.peers[0] = c.peers[1];
        assert!(c.validate().is_err());
        c = config();
        c.offline = 1;
        c.node = 3;
        assert!(c.validate().is_err());
        c = config();
        c.receive_budget = 0;
        assert!(c.validate().is_err());
    }
    fn byte_config(target: usize) -> DistributedConfig {
        let mut c = config();
        c.nodes = 7;
        c.peers = (0..7).map(|i| format!("[::1]:{}", 9000 + i).parse().unwrap()).collect();
        c.body_bytes = target;
        c.max_body = BENCH_MAX_BODY;
        c
    }
    #[test]
    fn exact_bodies_roundtrip_with_null_quorum_and_full_roster_parents() {
        use super::super::tests::pointer;
        let members = roster(7);
        for target in [6727, 8192, 65536, 1024 * 1024, 10 * 1024 * 1024, BENCH_MAX_BODY] {
            let c = byte_config(target);
            c.validate().unwrap();
            let first = body(1, FatPointerToBftBlock::null(), &c).unwrap();
            let id = ValueId(HashKeys::default().value_id.hash(&first.0));
            for (height, parent) in [
                (1, FatPointerToBftBlock::null()),
                (2, pointer(7, &[0, 1, 2, 3, 4], 1, id)),
                (2, pointer(7, &[0, 1, 2, 3, 4, 5], 1, id)),
                (2, pointer(7, &[0, 1, 2, 3, 4, 5, 6], 1, id)),
            ] {
                let signatures = parent.signatures.len();
                let value = body(height, parent, &c).unwrap();
                assert_eq!(value.0.len(), target);
                let mut bytes = value.0.as_slice();
                let decoded = BftBlock::zcash_deserialize(&mut bytes).unwrap();
                assert!(bytes.is_empty());
                assert_eq!(decoded.zcash_serialize_to_vec().unwrap(), value.0);
                assert!(valid_body(&value, height.into(), id, &c, &members));
                assert_eq!(payload_bytes(&decoded).unwrap() as usize, target - 71 - 96 * signatures);
                assert_eq!(checked_body_payload(&value, height.into(), id, &c, &members),
                    Some((target - 71 - 96 * signatures) as u64));
                if target == BENCH_MAX_BODY {
                    assert!(decoded.headers.iter().all(|h| h.solution.len() > 1344));
                }
            }
        }
        for headers in [1, 2, 2048] {
            let target = if headers == 1 { 10 * 1024 * 1024 } else { BENCH_MAX_BODY };
            let mut c = byte_config(target);
            c.headers = headers;
            c.validate().unwrap();
            let value = body(1, FatPointerToBftBlock::null(), &c).unwrap();
            assert_eq!(value.0.len(), target);
            assert!(valid_body(&value, 1, ValueId::NIL, &c, &members));
        }
    }
    #[test]
    fn exact_body_cap_never_autogrows_and_single_vector_limit_is_checked_before_allocation() {
        let mut c = byte_config(BENCH_MAX_BODY);
        c.max_body = 10 * 1024 * 1024;
        assert!(c.validate().is_err());
        c.max_body = BENCH_MAX_BODY;
        c.headers = 1;
        let null = FatPointerToBftBlock::null();
        assert!(c.validate().is_err());
        assert!(solution_lengths(&null, &c).is_err());
        assert!(body(1, null.clone(), &c).is_err());
        // Exact unchanged Vector limit accepted, next byte rejected arithmetically.
        c.body_bytes = 71 + 187 + 4 + 32 * 1024 * 1024;
        c.validate().unwrap();
        assert_eq!(solution_lengths(&null, &c).unwrap(), vec![32 * 1024 * 1024]);
        c.body_bytes += 1;
        assert!(c.validate().is_err());
        assert!(solution_lengths(&null, &c).is_err());
    }
    #[test]
    fn exact_padding_bridges_compact_size_gaps_without_format_changes() {
        for headers in [1, 2, 32] {
            let mut c = byte_config(8192);
            c.headers = headers;
            let base = mock_body(1, FatPointerToBftBlock::null(), headers).unwrap().0.len();
            for extra in (0..=520).chain(65500..=65600).chain(32 * 252..=32 * 256) {
                c.body_bytes = base + extra;
                let value = body(1, FatPointerToBftBlock::null(), &c);
                if headers == 1 && matches!(extra, 253 | 254 | 65538 | 65539) {
                    assert!(value.is_err());
                } else {
                    let value = value.unwrap();
                    assert_eq!(value.0.len(), c.body_bytes);
                    assert!(valid_body(&value, 1, ValueId::NIL, &c, &roster(7)));
                }
            }
        }
    }
    #[test]
    fn byte_mode_rejects_bad_bounds_padding_and_parent_signatures() {
        use super::super::tests::pointer;
        for (field, value) in [
            ("body_bytes", BENCH_MAX_BODY + 1), ("body_bytes", usize::MAX),
            ("max_body", BENCH_MAX_BODY + 1), ("max_body", 0),
            ("max_body", 8191), ("headers", 0), ("headers", 2049),
            ("nodes", 0), ("nodes", 33), ("solution_bytes", 1),
            ("body_bytes", 6726),
        ] {
            let mut json = serde_json::to_value(byte_config(8192)).unwrap();
            json[field] = serde_json::json!(value);
            let c: DistributedConfig = serde_json::from_value(json).unwrap();
            assert!(c.validate().is_err(), "{field}={value}");
        }
        let c = byte_config(8192);
        let members = roster(7);
        let first = body(1, FatPointerToBftBlock::null(), &c).unwrap();
        let id = ValueId(HashKeys::default().value_id.hash(&first.0));
        let parent = pointer(7, &[0, 1, 2, 3, 4], 1, id);
        let value = body(2, parent.clone(), &c).unwrap();
        let mut decoded = BftBlock::zcash_deserialize(value.0.as_slice()).unwrap();
        decoded.headers[0].solution[0] ^= 1;
        let bad = BlockValue(decoded.zcash_serialize_to_vec().unwrap());
        assert_eq!(bad.0.len(), c.body_bytes);
        assert!(!valid_body(&bad, 2, id, &c, &members));
        decoded.headers[0].solution[0] ^= 1;
        decoded.headers[0].solution.pop();
        assert!(!valid_body(&BlockValue(decoded.zcash_serialize_to_vec().unwrap()), 2, id, &c, &members));
        let mut bad_parent = parent;
        bad_parent.signatures[0].vote_signature[0] ^= 1;
        assert!(!valid_body(&body(2, bad_parent, &c).unwrap(), 2, id, &c, &members));
        let short = pointer(7, &[0, 1, 2, 3], 1, id);
        assert!(!valid_body(&body(2, short, &c).unwrap(), 2, id, &c, &members));
        assert!(!valid_body(&value, 2, ValueId::NIL, &c, &members));
        assert!(!valid_body(&value, 3, id, &c, &members));
        let mut trailing = value;
        trailing.0.push(0);
        assert!(!valid_body(&trailing, 2, id, &c, &members));
        // Valid at full-roster height2 but not representable at height1: reject
        // before launch rather than returning no proposal later.
        let mut gap = byte_config(71 + 187 + 65538);
        gap.headers = 1;
        assert!(gap.validate().is_err());
    }
    #[test]
    fn timing_profiles_are_explicit_and_preserve_production_defaults() {
        let mut c = config();
        assert_eq!(c.body_bytes, 0);
        assert_eq!(c.timing_profile, "production");
        let original = super::super::super::Timeouts {
            propose: 2000, prevote: 1000, precommit: 1500,
            inc_propose: 900, inc_prevote: 300, inc_precommit: 600,
        };
        let mut timeouts = original;
        let (mut request, mut ttl) = (2000, 5000);
        c.apply_timing(&mut timeouts, &mut request, &mut ttl);
        assert_eq!((timeouts, request, ttl), (original, 2000, 5000));
        c.timing_profile = "wan".into();
        c.validate().unwrap();
        c.apply_timing(&mut timeouts, &mut request, &mut ttl);
        assert_eq!(timeouts, super::super::super::Timeouts {
            propose: 10000, prevote: 5000, precommit: 7000, ..original
        });
        assert_eq!((request, ttl), (10000, 15000));
        let namespace = c.namespace();
        c.timing_profile = "wan_fast".into();
        c.validate().unwrap();
        c.apply_timing(&mut timeouts, &mut request, &mut ttl);
        assert_eq!(timeouts, super::super::super::Timeouts {
            propose: 2500, prevote: 1500, precommit: 2000, ..original
        });
        assert_eq!((request, ttl), (10000, 15000));
        assert_eq!(c.namespace(), namespace); // Timer choice is not a payload/signature domain.
        let json = serde_json::to_value(&c).unwrap();
        assert_eq!(json["timing_profile"], "wan_fast");
        let decoded: DistributedConfig = serde_json::from_value(json).unwrap();
        decoded.validate().unwrap();
        assert_eq!(decoded.timing_profile, "wan_fast");
        c.timing_profile = "other".into();
        assert!(c.validate().is_err());
    }
    #[test]
    fn runtime_exit_reports_deadline_context_only_at_or_after_deadline() {
        let deadline = Instant::now();
        let outcome = Ok::<io::Result<()>, tokio::task::JoinError>(Err(io::Error::other("D28: Storage")));
        let original = format!("D28 runtime exited: {outcome:?}");
        assert_eq!(
            runtime_failure(&outcome, deadline - Duration::from_nanos(1), deadline),
            original,
        );
        for now in [deadline, deadline + Duration::from_nanos(1)] {
            assert_eq!(
                runtime_failure(&outcome, now, deadline),
                format!("whole-process benchmark deadline reached; {original}"),
            );
        }
        // The cleanup path also handles unexpected successful runtime exits;
        // retain their original cause rather than manufacturing a storage error.
        let completed = Ok::<io::Result<()>, tokio::task::JoinError>(Ok(()));
        assert_eq!(
            runtime_failure(&completed, deadline - Duration::from_nanos(1), deadline),
            "D28 runtime exited: Ok(Ok(()))",
        );
    }
    #[tokio::test]
    async fn bounded_deadline_error_retains_its_existing_classification() {
        let error = bounded::<()>(Instant::now(), std::future::pending()).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(error.to_string(), "whole-process benchmark deadline reached");
    }
    #[test]
    fn control_is_only_start_or_exact_ack() {
        assert!(matches!(
            serde_json::from_str::<Control>(r#"{"type":"start"}"#).unwrap(),
            Control::Start {}
        ));
        assert!(matches!(
            serde_json::from_str::<Control>(r#"{"type":"ack","height":1}"#).unwrap(),
            Control::Ack { height: 1 }
        ));
        for text in [
            r#"{"type":"start","body":"x"}"#,
            r#"{"type":"ack","height":1,"node":0}"#,
            r#"{"type":"ack","height":true}"#,
            r#"{"type":"ack","height":-1}"#,
            r#"{"type":"ack","height":1,"height":2}"#,
            r#"{"type":"tip"}"#,
        ] {
            assert!(serde_json::from_str::<Control>(text).is_err(), "{text}");
        }
    }
}
