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
    pub workers: usize,
    pub max_body: usize,
    pub chatter_secs: u64,
    pub peers: Vec<SocketAddrV6>,
    pub coordinator: SocketAddrV6,
    pub run_id: String,
    /// Existing Zcash solution field, not an added ledger/wire format.
    #[serde(default)]
    pub solution_bytes: usize,
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

impl DistributedConfig {
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
            ..BenchConfig::default()
        }
    }
    pub fn validate(&self) -> io::Result<()> {
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
        let parent = FatPointerToBftBlock::from_parts(
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
        if body(2, parent, self)?.0.len() > self.max_body {
            return Err(invalid(
                "padded headers plus full-roster parent certificate exceed max_body",
            ));
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
fn body(
    height: u32,
    parent: FatPointerToBftBlock,
    c: &DistributedConfig,
) -> io::Result<BlockValue> {
    Ok(BlockValue(
        BftBlock {
            version: 2,
            height,
            previous_block_fat_ptr: parent,
            headers: (0..c.headers)
                .map(|i| padded_header(height, i, c.solution_bytes))
                .collect(),
            slashforks: vec![],
            do_not_include_until_bc_height: 0,
        }
        .zcash_serialize_to_vec()?,
    ))
}
fn payload_bytes(c: &DistributedConfig) -> io::Result<u64> {
    let mut bytes = vec![];
    BlockHeader::write_data(&padded_header(1, 0, c.solution_bytes), &mut bytes)?;
    Ok((bytes.len() * c.headers) as u64)
}
fn valid_body(
    value: &BlockValue,
    height: u64,
    parent: ValueId,
    c: &DistributedConfig,
    members: &[SortedRosterMember],
) -> bool {
    if value.0.len() > c.max_body {
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
        || block.headers.len() != c.headers
        || !block
            .headers
            .iter()
            .enumerate()
            .all(|(i, h)| *h == padded_header(block.height, i, c.solution_bytes))
        || block.zcash_serialize_to_vec().ok().as_deref() != Some(value.0.as_slice())
    {
        return false;
    }
    if height == 1 {
        block.previous_block_fat_ptr == FatPointerToBftBlock::null()
    } else {
        verify_pointer(&block.previous_block_fat_ptr, members, height - 1, parent)
    }
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
            next_vote_namespace: NAMESPACE,
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
            if c.pending.is_some()
                || !chunks.is_empty()
                || !valid_body(&body, height, c.id, &c.config, &c.members)
                || !verify_pointer(&pointer, &c.members, height, id)
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
                    payload_bytes: payload_bytes(&c.config).expect("validated shape"),
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
    let limits = crate::transport::TransportLimits::default();
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
        let mut rt = RuntimeConfig::new(1, roster(config.nodes), NAMESPACE, config.nodes, transport,
            addresses.into_iter().enumerate().filter_map(|(i, a)| (i != config.node).then_some(a)).collect());
        rt.max_body = config.max_body;
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
        send(&mut stream, serde_json::json!({"type":"ready", "run_id":config.run_id, "node":config.node}), deadline).await?;
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
                    return Err(invalid(format!("D28 runtime exited: {outcome:?}")));
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
            failure.get_or_insert(format!("D28 runtime exited: {outcome:?}"));
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
            "One real D28/STP validator per process; fixed full equal-stake roster, predictable isolated benchmark keys/namespace; no PoW, ledger or transaction/TPS claim.",
            "All-online controller ACK barrier is intentional benchmark overhead: both callback return and trusted read_tip wait for ACK. Bodies and certificates come only from local D28 export and unchanged production parsing/signature checks, never from controller.",
            "Node latency uses one local monotonic clock: START to push_decided entry at height1, prior ACK/tip-publication to push_decided entry thereafter. Includes INIT, storage and host polling; excludes local export checks and ACK wait. Never subtract clocks across machines; controller receipt/cycle metrics are separate.",
            "Measured counter window starts before first measured height publication and ends after final ACK: includes global barrier overhead, excludes warmup and optional separate chatter. Partial zero-decision duration is retained. ACK wait is callback entry to ACK receipt, including host validation/control overhead.",
            "Optional chatter retains final-height evidence after final ACK without next-height publication; not idle traffic of a quiescent chain. Shutdown/in-flight tail excluded from measured counters.",
            "Application offered bytes are not wire bytes; transport ring messages are not UDP packets. /proc/net/dev includes TX/RX of every non-lo interface and all VM traffic, including controller/management; not isolated D28 amplification. Serial snapshots have sampling skew.",
            "Host CPU/steal/cgroup counters include other VM processes. Process CPU is in reported clock ticks; RSS/maxRSS are gauges (not deltas). Missing system metrics carry errors, never fabricated zeros. In-memory atomic image is not fsync/crash-durable storage.",
            "Production rho/consensus timeouts and core formats unchanged. Only explicit poll/status/receive/request/outgoing scheduling knobs vary; transport defaults and maximum body bound are recorded.",
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
        assert_eq!(payload_bytes(&c).unwrap(), 509 * 2048);
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
