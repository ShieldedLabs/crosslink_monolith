//! Benchmark-only, bounded protocol observation. Never used by transport service.
//!
//! Native stats do NOT include current_connections (STP's wants_snapshot is
//! reference-only). Wrapper hashes on either side of a request are evidence,
//! not an atomic native session identifier. Keep raw samples, never peer deltas.
use super::*;
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::{TryLockError, Weak};

const PERIOD: Duration = Duration::from_millis(200);
const TIMEOUT: Duration = Duration::from_secs(1);
const SAMPLE_CAP: usize = 128;
const PEER_CAP: usize = 32;
// Split both caps over local endpoints; distributed reports have one endpoint.
const PEER_SAMPLE_CAP: usize = 512;

pub(crate) const SEMANTICS: &str = "STP protocol: benchmark-only 200ms async observer; one final top-level tail, not repeated at boundaries. At most128 samples and512 retained peer-sample cells per report, divided over local endpoints; max32 peers/sample, oldest samples overwritten, omitted peers explicit. Per-endpoint capacity/count/time-range metadata records actual retention; no samples means no observation yet. Raw rolling tail may include warmup/chatter, NOT whole-window coverage or deltas; align timestamps to measured boundaries. Native capture lies between request and collect, exact time unknown; queued bytes/outbox gauges have separate observation times and are not unacknowledged/delivered bytes. Native current_connections is absent: request/collect wrapper hashes only, wrapper_confirmed is not atomic native identity. Peer counters reset on reconnect; thread counters are pump-lifetime. cc_rate_upps is millionths of packets/s (divide by 1000000), rtt_us is microseconds, udp_payload_tu is bytes, cc_app_limited_entries counts transitions not duration. free_bytes=u64::MAX means empty. Send/ACK counters are separate attempts including local errors, not delivery; local drops also include probes. Pacing/loop maxima are lifetime maxima. Pump CPU is copied by STP into each peer but recorded once/endpoint, never summed; CPU/kernel-drop values may be cached up to100ms. Null is unavailable, not zero. Timeout preserves the outstanding request; late responses retain their original bracket.";

fn ms(duration: Duration) -> f64 { duration.as_secs_f64() * 1000.0 }

#[derive(Clone, Debug, Serialize)]
pub struct QueueSnapshot {
    pub at_ms: f64,
    /// Queue order: BftLive, Control, Bulk. Planned sends, not unacked bytes.
    pub queued_send_bytes: [u64; 3],
    pub outbox_bytes: [u64; 3],
}
#[derive(Clone, Debug, Serialize)]
pub struct PeerSnapshot {
    pub ip: std::net::Ipv6Addr,
    pub port: u16,
    pub key_15_bits: u16,
    pub requested_handshake_hash: Option<String>,
    pub collected_handshake_hash: Option<String>,
    pub session_status: &'static str,
    pub queues_at_request: Option<QueueSnapshot>,
    pub queues_at_collect: Option<QueueSnapshot>,
    /// Raw native gauges; u64::MAX means empty, null means not supplied.
    pub unreliable_send_free_bytes: [Option<u64>; 3],
    pub stats: BTreeMap<&'static str, u64>,
    pub last_local_send_error: Option<i32>,
}
#[derive(Clone, Debug, Serialize)]
pub struct ProtocolData {
    pub peers_total: usize,
    pub peers_omitted: usize,
    pub peers: Vec<PeerSnapshot>,
    pub thread_stats: BTreeMap<&'static str, u64>,
    pub kernel_rx_drops: Option<u64>,
    /// Same pump-wide value duplicated in native peer stats; absent without peers.
    pub network_cpu_user_us: Option<u64>,
    pub network_cpu_sys_us: Option<u64>,
}
#[derive(Clone, Debug, Serialize)]
pub struct ProtocolSample {
    pub status: &'static str,
    pub requested_at_ms: Option<f64>,
    pub collected_at_ms: f64,
    /// Upper bound on native snapshot age, NOT its exact age.
    pub request_age_ms: Option<f64>,
    pub collection_duration_ms: f64,
    pub data: Option<ProtocolData>,
}
#[derive(Clone, Debug, Serialize)]
pub struct ProtocolHistory {
    pub snapshot_at_ms: f64,
    pub first_collected_at_ms: Option<f64>,
    pub last_collected_at_ms: Option<f64>,
    pub sample_period_ms: u64,
    pub sample_capacity: usize,
    pub peer_capacity: usize,
    pub peer_sample_capacity: usize,
    pub retained_peer_samples: usize,
    pub observations: u64,
    pub ready_samples: u64,
    pub missing_samples: u64,
    pub overwritten_samples: u64,
    pub samples: VecDeque<ProtocolSample>,
}
impl Default for ProtocolHistory {
    fn default() -> Self { Self::bounded(1) }
}
impl ProtocolHistory {
    fn bounded(endpoints: usize) -> Self {
        let cells = (PEER_SAMPLE_CAP / endpoints.max(1)).max(1);
        Self { snapshot_at_ms: 0.0, first_collected_at_ms: None, last_collected_at_ms: None,
            sample_period_ms: PERIOD.as_millis() as u64, sample_capacity: (SAMPLE_CAP / endpoints.max(1)).max(1),
            peer_capacity: PEER_CAP.min(cells), peer_sample_capacity: cells, retained_peer_samples: 0,
            observations: 0, ready_samples: 0, missing_samples: 0, overwritten_samples: 0, samples: VecDeque::new() }
    }
    fn push(&mut self, mut sample: ProtocolSample) {
        self.observations = self.observations.saturating_add(1);
        if sample.data.is_some() { self.ready_samples = self.ready_samples.saturating_add(1); }
        else { self.missing_samples = self.missing_samples.saturating_add(1); }
        let cells = sample.data.as_mut().map_or(0, |data| {
            data.peers.truncate(self.peer_capacity);
            data.peers_omitted = data.peers_total.saturating_sub(data.peers.len());
            data.peers.len()
        });
        while self.samples.len() >= self.sample_capacity || self.retained_peer_samples + cells > self.peer_sample_capacity {
            let old = self.samples.pop_front().expect("bounded sample fits an empty history");
            self.retained_peer_samples -= old.data.as_ref().map_or(0, |d| d.peers.len());
            self.overwritten_samples = self.overwritten_samples.saturating_add(1);
        }
        self.retained_peer_samples += cells;
        self.last_collected_at_ms = Some(sample.collected_at_ms);
        self.samples.push_back(sample);
        self.first_collected_at_ms = self.samples.front().map(|s| s.collected_at_ms);
    }
}

pub(super) struct Request {
    at: Instant,
    peers: Vec<(ConnectionKey, [u8; 64], QueueSnapshot)>,
}

/// Owned by the benchmark Sampler. The task has only a weak hub reference;
/// abort-on-drop bounds its lifetime even if the benchmark errors or is cancelled.
pub(crate) struct ProtocolObserver {
    handle: ConsumerHandle,
    origin: Instant,
    history: Arc<Mutex<ProtocolHistory>>,
    task: tokio::task::JoinHandle<()>,
}
impl ProtocolObserver {
    pub(crate) fn new(handle: ConsumerHandle, origin: Instant, deadline: Instant, endpoints: usize) -> Self {
        let hub = Arc::downgrade(&handle.hub);
        let (consumer, generation) = (handle.consumer, handle.generation);
        let history = Arc::new(Mutex::new(ProtocolHistory::bounded(endpoints)));
        let output = Arc::clone(&history);
        let task = tokio::spawn(async move {
            while Instant::now() < deadline {
                let sample = observe(&hub, consumer, generation, origin);
                let terminal = matches!(sample.status, "closed" | "replaced");
                output.lock().unwrap_or_else(|e| e.into_inner()).push(sample);
                if terminal { break; }
                // No catch-up bursts after scheduler stalls and no lock held asleep.
                tokio::time::sleep_until(tokio::time::Instant::from_std(
                    (Instant::now() + PERIOD).min(deadline))).await;
            }
        });
        Self { handle, origin, history, task }
    }
    pub(crate) fn stats(&self) -> TransportStats { self.handle.stats() }
    pub(crate) fn snapshot(&self) -> ProtocolHistory {
        let mut history = self.history.lock().unwrap_or_else(|e| e.into_inner()).clone();
        history.snapshot_at_ms = ms(self.origin.elapsed());
        history
    }
}
impl Drop for ProtocolObserver {
    fn drop(&mut self) { self.task.abort(); }
}

fn observe(hub: &Weak<Mutex<Hub>>, consumer: Consumer, generation: u64, origin: Instant) -> ProtocolSample {
    let at = Instant::now();
    let mut sample = ProtocolSample { status: "closed", requested_at_ms: None,
        collected_at_ms: ms(at.saturating_duration_since(origin)), request_age_ms: None,
        collection_duration_ms: 0.0, data: None };
    if let Some(hub) = hub.upgrade() {
        match hub.try_lock() {
            Err(TryLockError::WouldBlock) => sample.status = "busy",
            guard => {
                let mut hub = guard.unwrap_or_else(|e| match e {
                    TryLockError::Poisoned(e) => e.into_inner(),
                    TryLockError::WouldBlock => unreachable!(),
                });
                if hub.consumers[consumer as usize].generation != generation {
                    sample.status = "replaced";
                } else {
                    hub.handle.observe_protocol(origin, at, &mut sample);
                }
            }
        }
    }
    let collected = Instant::now();
    sample.collected_at_ms = ms(collected.saturating_duration_since(origin));
    sample.request_age_ms = sample.requested_at_ms.map(|requested| sample.collected_at_ms - requested);
    sample.collection_duration_ms = ms(collected.saturating_duration_since(at));
    sample
}
impl NetworkThreadHandle {
    fn protocol_queues(&self, key: ConnectionKey, origin: Instant) -> Option<QueueSnapshot> {
        let peer = self.peers.get(&key).filter(|p| p.is_connected())?;
        Some(QueueSnapshot { at_ms: ms(Instant::now().saturating_duration_since(origin)),
            queued_send_bytes: Channel::ALL.map(|q| self.endpoint.queued_send_bytes(key, q.queue())),
            outbox_bytes: Channel::ALL.map(|q| peer.outboxes[q.queue()].bytes as u64) })
    }
    fn request_protocol(&mut self, at: Instant, origin: Instant) {
        let mut keys: Vec<_> = self.peers.iter().filter(|(_, p)| p.is_connected()).map(|(k, _)| *k).collect();
        keys.sort_unstable();
        let peers = keys.into_iter().take(PEER_CAP).map(|key| {
            (key, self.peers[&key].handshake_hash, self.protocol_queues(key, origin).unwrap())
        }).collect();
        self.protocol_request = Some(Request { at, peers });
        self.endpoint.request_protocol_stats();
    }
    fn observe_protocol(&mut self, origin: Instant, at: Instant, sample: &mut ProtocolSample) {
        if self.is_finished() {
            self.protocol_request = None;
            return;
        }
        if self.protocol_request.is_none() { self.request_protocol(at, origin); }
        let request = self.protocol_request.as_ref().unwrap();
        sample.requested_at_ms = Some(ms(request.at.saturating_duration_since(origin)));
        sample.request_age_ms = Some(ms(at.saturating_duration_since(request.at)));
        sample.status = if at.saturating_duration_since(request.at) >= TIMEOUT { "timeout" } else { "pending" };
        if let Some(pull) = self.endpoint.try_protocol_stats() {
            let request = self.protocol_request.take().unwrap();
            sample.status = "ready";
            sample.data = Some(self.protocol_data(pull, &request, origin));
            // Never queue another request until the one response was consumed.
            self.request_protocol(Instant::now(), origin);
        }
    }
    fn protocol_data(&self, mut pull: shielded_transport_pigeon::stp::NetworkThreadPull,
        request: &Request, origin: Instant) -> ProtocolData {
        pull.connection_stats.sort_unstable_by_key(|(key, _)| *key);
        let peers_total = pull.connection_stats.len();
        let cpu = pull.connection_stats.first().map(|(_, s)| (s.network_cpu_user_us, s.network_cpu_sys_us));
        let peers = pull.connection_stats.into_iter().take(PEER_CAP).map(|(key, s)| {
            let before = request.peers.iter().find(|(k, _, _)| *k == key);
            let after = self.peers.get(&key).filter(|p| p.is_connected());
            let confirmed = before.zip(after).is_some_and(|((_, hash, _), p)| *hash == p.handshake_hash);
            let mut stats = BTreeMap::new();
            macro_rules! fields { ($($field:ident),* $(,)?) => { $(stats.insert(stringify!($field), s.$field as u64);)* }; }
            fields!(rtt_us, cc_rate_upps, cc_app_limited_entries, udp_payload_tu,
                packets_sent, bytes_sent, ack_packets_sent, ack_bytes_sent,
                local_send_drops, local_send_drop_bytes, packets_lost, spurious_losses,
                rx_packets, rx_lost, rx_reordered, ack_app_overload_ce_sent, rx_app_overload_ce_marks,
                pacing_batches, pacing_packets, pacing_max_burst, pacing_error_samples,
                pacing_error_abs_sum_ns, pacing_error_max_ns);
            PeerSnapshot { ip: key.ip, port: key.port, key_15_bits: key.key_15_bits,
                requested_handshake_hash: before.map(|(_, hash, _)| hex::encode(hash)),
                collected_handshake_hash: after.map(|p| hex::encode(p.handshake_hash)),
                session_status: if confirmed { "wrapper_confirmed" } else { "ambiguous" },
                queues_at_request: before.map(|(_, _, q)| q.clone()),
                queues_at_collect: self.protocol_queues(key, origin),
                unreliable_send_free_bytes: Channel::ALL.map(|q| pull.unreliable_send_free_bytes.iter()
                    .find(|(k, queue, _)| *k == key && *queue == q.queue()).map(|(_, _, b)| *b)),
                stats, last_local_send_error: s.last_local_send_error }
        }).collect();
        let s = pull.thread_stats;
        ProtocolData { peers_total, peers_omitted: peers_total.saturating_sub(PEER_CAP), peers,
            thread_stats: BTreeMap::from([("loop_iterations", s.loop_iterations),
                ("max_loop_gap_ns", s.max_loop_gap_ns), ("loop_gaps_over_1ms", s.loop_gaps_over_1ms),
                ("loop_gaps_over_10ms", s.loop_gaps_over_10ms), ("loop_gaps_over_100ms", s.loop_gaps_over_100ms),
                ("recv_queue_dropped_messages", s.recv_queue_dropped_messages)]),
            kernel_rx_drops: s.kernel_rx_drops, network_cpu_user_us: cpu.map(|c| c.0), network_cpu_sys_us: cpu.map(|c| c.1) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shielded_transport_pigeon::stp::{ConnectionStats, new_keypair_from_connect_magic1,
        CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2b};
    use std::net::Ipv6Addr;

    fn key() -> IdentityKeyPair {
        new_keypair_from_connect_magic1(CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2b).unwrap()
    }
    fn port() -> u16 {
        std::net::UdpSocket::bind("[::1]:0").unwrap().local_addr().unwrap().port()
    }
    fn fixture() -> (SharedTransport, Pump) {
        let (handle, pump) = NetworkThreadHandle::new_unstarted_on(vec![key()], port(), None,
            (1_000_000, 512 * 1024, 8 << 20), TransportLimits::default(),
            Some(std::net::IpAddr::V6(Ipv6Addr::LOCALHOST)));
        (SharedTransport(Arc::new(Mutex::new(Hub { handle, consumers: Default::default(),
            redial: HashMap::new(), lock_times: Default::default(), dial_only: None }))), pump)
    }
    fn take(handle: &ConsumerHandle, origin: Instant) -> ProtocolSample {
        observe(&Arc::downgrade(&handle.hub), handle.consumer, handle.generation, origin)
    }

    #[test]
    fn pending_timeout_late_ready_and_closed_are_distinct_and_one_request_is_retained() {
        let (transport, mut pump) = fixture();
        let handle = transport.attach(Consumer::Base);
        let origin = Instant::now() - Duration::from_secs(2);
        let first = take(&handle, origin);
        assert_eq!(first.status, "pending");
        assert!(first.data.is_none());
        let first_at = lock(&handle.hub).handle.protocol_request.as_ref().unwrap().at;
        for _ in 0..100 {
            assert!(take(&handle, origin).data.is_none());
            assert_eq!(lock(&handle.hub).handle.protocol_request.as_ref().unwrap().at, first_at);
        }
        // Advance only the request's age; the native pump is deliberately paused.
        lock(&handle.hub).handle.protocol_request.as_mut().unwrap().at = origin;
        let missing = take(&handle, origin);
        assert_eq!(missing.status, "timeout");
        assert_eq!(missing.requested_at_ms, Some(0.0));
        assert!(missing.data.is_none());
        pump.run_for(Duration::from_millis(2));
        let ready = take(&handle, origin);
        assert_eq!(ready.status, "ready");
        assert_eq!(ready.requested_at_ms, Some(0.0));
        assert!(ready.request_age_ms.unwrap() >= 2000.0);
        let data = ready.data.unwrap();
        assert!(data.peers.is_empty()); // Valid zero peers is NOT missing data.
        assert!(data.network_cpu_user_us.is_none());
        assert_eq!(data.peers_total, 0);
        assert_eq!(take(&handle, origin).status, "pending");
        // Paused pump: no extra queued/overwritten response from repeated polls.
        assert!(lock(&handle.hub).handle.endpoint.try_protocol_stats().is_none());
        lock(&handle.hub).handle.endpoint.close();
        drop(pump); // Finished means the native pump has actually stopped.
        let closed = take(&handle, origin);
        assert_eq!(closed.status, "closed");
        assert!(closed.data.is_none() && closed.requested_at_ms.is_none());
        assert!(lock(&handle.hub).handle.protocol_request.is_none());
    }

    #[test]
    fn busy_and_replaced_observers_never_wait_or_service_transport() {
        let (transport, _pump) = fixture();
        let handle = transport.attach(Consumer::Base);
        let origin = Instant::now();
        let guard = lock(&handle.hub);
        assert_eq!(take(&handle, origin).status, "busy");
        assert!(guard.handle.protocol_request.is_none());
        drop(guard);
        let _replacement = transport.attach(Consumer::Base);
        assert_eq!(take(&handle, origin).status, "replaced");
        assert_eq!(observe(&Weak::new(), Consumer::Base, 0, origin).status, "closed");
    }

    #[test]
    fn raw_units_session_ambiguity_and_sample_peer_bounds_are_preserved_in_json() {
        let (transport, _pump) = fixture();
        let origin = Instant::now();
        let mut hub = lock(&transport.0);
        let handle = &mut hub.handle;
        let remote = key();
        let mut pull = shielded_transport_pigeon::stp::NetworkThreadPull::default();
        for i in 0..PEER_CAP + 2 {
            let address = STPAddress::from(Ipv6Addr::LOCALHOST, 40000 + i as u16, &remote);
            let peer = address.connection_key();
            handle.handle_event(Event::Connected { peer, address, handshake_hash: [1; 64] },
                &mut VecDeque::new(), origin);
            handle.enqueue(peer, Channel::Bulk, vec![1; 17]);
            pull.connection_stats.push((peer, ConnectionStats { rtt_us: 123456,
                cc_rate_upps: 987654321, cc_app_limited_entries: 3, udp_payload_tu: 1200,
                bytes_sent: 1000 + i as u64, packets_sent: 5, local_send_drops: 2,
                last_local_send_error: Some(105), pacing_error_abs_sum_ns: 123456789,
                network_cpu_user_us: 90, network_cpu_sys_us: 10, ..Default::default() }));
            pull.unreliable_send_free_bytes.push((peer, Channel::Bulk.queue(), u64::MAX));
        }
        handle.request_protocol(origin, origin);
        let request = handle.protocol_request.take().unwrap();
        assert_eq!(request.peers.len(), PEER_CAP);
        // Native API supplies no current_connections, even for connected stats.
        assert!(pull.current_connections.is_empty());
        let first = request.peers[0].0;
        handle.peers.get_mut(&first).unwrap().handshake_hash = [2; 64];
        let data = handle.protocol_data(pull, &request, origin);
        assert_eq!(data.peers.len(), PEER_CAP);
        assert_eq!(data.peers_omitted, 2);
        assert_eq!(data.network_cpu_user_us, Some(90)); // Not 90 * peer count.
        assert_eq!(data.peers[0].session_status, "ambiguous");
        assert_eq!(data.peers[1].session_status, "wrapper_confirmed");
        assert_ne!(data.peers[0].requested_handshake_hash, data.peers[0].collected_handshake_hash);
        let json = serde_json::to_value(&data).unwrap();
        let peer = &json["peers"][0];
        assert_eq!(peer["stats"]["cc_rate_upps"], 987654321u64);
        assert_eq!(peer["stats"]["rtt_us"], 123456u64);
        assert_eq!(peer["stats"]["pacing_error_abs_sum_ns"], 123456789u64);
        assert_eq!(peer["last_local_send_error"], 105);
        assert_eq!(peer["unreliable_send_free_bytes"][2].as_u64(), Some(u64::MAX));
        assert!(peer["unreliable_send_free_bytes"][0].is_null());
        assert_eq!(peer["queues_at_request"]["outbox_bytes"][2], 17);
        assert_eq!(peer["queues_at_collect"]["outbox_bytes"][2], 17);
        assert!(json["kernel_rx_drops"].is_null());
        assert!(peer["stats"].get("network_cpu_user_us").is_none());
        check_artifact_bounds(&data);
        let empty = ProtocolSample { status: "timeout", requested_at_ms: Some(0.0), collected_at_ms: 2000.0,
            request_age_ms: Some(2000.0), collection_duration_ms: 0.0, data: None };
        let mut history = ProtocolHistory::default();
        history.push(ProtocolSample { status: "ready", data: Some(data), ..empty.clone() });
        for _ in 0..SAMPLE_CAP { history.push(empty.clone()); }
        assert_eq!(history.samples.len(), SAMPLE_CAP);
        assert_eq!(history.overwritten_samples, 1);
        assert_eq!(history.ready_samples, 1);
        assert_eq!(history.missing_samples, SAMPLE_CAP as u64);
        assert_eq!(history.observations, SAMPLE_CAP as u64 + 1);
        assert!(serde_json::to_value(&history).unwrap()["samples"][0]["data"].is_null());
    }

    fn check_artifact_bounds(template: &ProtocolData) {
        // Max-width counters/addresses, full queues/hashes: pretty JSON, the same
        // serializer as the CLI. Leave >=4MiB of the worker's 8MiB report cap for
        // existing height/system/window fields (not a bound on those fields).
        for (endpoints, peer_count) in [(1, 6), (1, 32), (7, 6), (128, 32)] {
            let mut data = template.clone();
            data.peers.truncate(peer_count);
            data.peers_total = peer_count;
            data.peers_omitted = 0;
            for p in &mut data.peers {
                p.ip = Ipv6Addr::from([0xffff; 8]);
                p.port = u16::MAX;
                p.key_15_bits = u16::MAX;
                p.stats.values_mut().for_each(|v| *v = u64::MAX);
                p.session_status = "wrapper_confirmed";
                p.last_local_send_error = Some(i32::MIN);
                p.unreliable_send_free_bytes = [Some(u64::MAX); 3];
                for q in [&mut p.queues_at_request, &mut p.queues_at_collect].into_iter().flatten() {
                    q.at_ms = u64::MAX as f64;
                    q.queued_send_bytes = [u64::MAX; 3];
                    q.outbox_bytes = [u64::MAX; 3];
                }
            }
            data.thread_stats.values_mut().for_each(|v| *v = u64::MAX);
            data.kernel_rx_drops = Some(u64::MAX);
            data.network_cpu_user_us = Some(u64::MAX);
            data.network_cpu_sys_us = Some(u64::MAX);
            let mut histories = Vec::new();
            for _ in 0..endpoints {
                let mut history = ProtocolHistory::bounded(endpoints);
                for i in 0..2 * SAMPLE_CAP {
                    history.push(ProtocolSample { status: "ready", requested_at_ms: Some(i as f64 * 200.0),
                        collected_at_ms: i as f64 * 200.0 + 1.0, request_age_ms: Some(1.0),
                        collection_duration_ms: 0.5, data: Some(data.clone()) });
                }
                assert!(history.samples.len() <= history.sample_capacity);
                assert!(history.retained_peer_samples <= history.peer_sample_capacity);
                assert!(history.overwritten_samples > 0);
                assert_eq!(history.first_collected_at_ms, history.samples.front().map(|s| s.collected_at_ms));
                histories.push(history);
            }
            assert!(histories.iter().map(|h| h.samples.len()).sum::<usize>() <= SAMPLE_CAP);
            assert!(histories.iter().map(|h| h.retained_peer_samples).sum::<usize>() <= PEER_SAMPLE_CAP);
            let bytes = serde_json::to_vec_pretty(&serde_json::json!({"protocol": histories})).unwrap().len();
            eprintln!("protocol pretty JSON: endpoints={endpoints} peers={peer_count} bytes={bytes}");
            assert!(bytes < 4 * 1024 * 1024, "protocol tail leaves insufficient report headroom: {bytes}");
        }
    }

    #[test]
    fn real_local_stp_observer_collects_during_sending_and_stops_on_drop() {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
            let remote = key();
            let remote_port = port();
            let address = STPAddress::from(Ipv6Addr::LOCALHOST, remote_port, &remote);
            let limits = TransportLimits::default();
            let a = SharedTransport::with_bind_ip(vec![key()], port(), Some(100),
                (1_000_000, 512 * 1024, 8 << 20), limits.clone(), Ipv6Addr::LOCALHOST.into());
            let b = SharedTransport::with_bind_ip(vec![remote], remote_port, None,
                (1_000_000, 512 * 1024, 8 << 20), limits, Ipv6Addr::LOCALHOST.into());
            let origin = Instant::now();
            let deadline = origin + Duration::from_secs(10);
            let observer = ProtocolObserver::new(a.attach(Consumer::Base), origin, deadline, 1);
            let mut sender = a.attach(Consumer::Bft);
            let mut receiver = b.attach(Consumer::Bft);
            let mut receiving = NetworkThreadPush::default();
            let mut push = NetworkThreadPush { initiate_connections: vec![address.clone()], ..Default::default() };
            loop {
                let pull = sender.service(std::mem::take(&mut push)).unwrap();
                push.wanted_connections = pull.current_connections;
                let received = receiver.service(std::mem::take(&mut receiving)).unwrap();
                receiving.wanted_connections = received.current_connections;
                if !push.wanted_connections.is_empty() { break; }
                assert!(Instant::now() < deadline);
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            // Deliberately >1s of work at 100pps, to observe active queued sends,
            // not merely an empty post-decision boundary. No delivery-rate claim.
            push.send_bulk = vec![(address.connection_key(), vec![0x80; 2 << 20])];
            sender.service(push).unwrap();
            loop {
                let history = observer.snapshot();
                if history.samples.iter().filter_map(|s| s.data.as_ref()).flat_map(|d| &d.peers)
                    .any(|p| p.stats["packets_sent"] > 0 && p.queues_at_collect.as_ref()
                        .is_some_and(|q| q.queued_send_bytes[2] > 0)) {
                    assert!(history.ready_samples > 0);
                    assert!(history.samples.iter().any(|s| s.request_age_ms.is_some_and(|age| age > 0.0)));
                    break;
                }
                assert!(Instant::now() < deadline, "no active-transfer protocol observation");
                let received = receiver.service(std::mem::take(&mut receiving)).unwrap();
                receiving.wanted_connections = received.current_connections;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let retained = Arc::clone(&observer.history);
            let abort = observer.task.abort_handle();
            drop(observer);
            tokio::task::yield_now().await;
            assert!(abort.is_finished());
            let count = retained.lock().unwrap().observations;
            tokio::time::sleep(PERIOD + Duration::from_millis(20)).await;
            assert_eq!(retained.lock().unwrap().observations, count);
        });
    }
}
