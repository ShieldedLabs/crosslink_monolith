//! Crosslink's owner of the STP 1.0 transport.
//!
//! STP 1.0 starts no threads. This module spawns the one network thread that drives the
//! [`Pump`], and keeps the single-owner [`LockfreeEndpoint`] inside [`NetworkThreadHandle`],
//! which belongs to exactly one sync loop (Tenderlink's, or new_network's). Nothing here is
//! shared through a lock: the handle is `Send`, not `Sync`, and every call takes `&mut self`.
//!
//! The push/pull shape of the 0.x API is kept, so the two sync loops did not have to be
//! restructured: each tick hands over the connections to start, the connections to keep and
//! the messages to send, and gets back the live connections and the messages received.
//!
//! Every queue is bounded:
//! - the three STP rings (send, control, receive) by [`TransportLimits::rings`];
//! - each peer's two local outboxes (one per [`Channel`]) by
//!   [`TransportLimits::peer_outbox_bytes`] and [`TransportLimits::peer_outbox_messages`]
//!   each, so at most twice those per peer in total (16 MiB / 8192 messages by default);
//! - connection attempts in flight, and so queued connect commands, by
//!   [`TransportLimits::max_pending_connects`];
//! - disconnect intents by the peer table itself: closing is a flag on the peer's entry, set
//!   once and cleared when STP's control ring takes the disconnect, so repeated service calls
//!   with the ring full queue nothing new.
//!
//! Two channels, each its own STP send queue per connection: [`Channel::Control`] for small
//! protocol messages (status, requests, votes) and [`Channel::Bulk`] for whole blocks. STP
//! shares a connection's bandwidth between its queues by equal-weight byte service, so a
//! status or request to a peer does not wait behind a block to the same peer. The caller
//! picks the channel per message (`send_unreliable` vs `send_bulk`).
//!
//! Flow control is early, not drop-after-producing: a message leaves a (peer, channel) outbox
//! for the shared send ring only when STP can take it at once (see `flush_outboxes`), control
//! outboxes before bulk ones, and producers ask [`NetworkThreadHandle::send_budget`] before
//! building large messages. Outbox drops are counted in [`TransportStats`]; they are
//! the overflow path, not the steady-state one.
//!
//! STP is best-effort. A message accepted here can still be lost on the wire, so request /
//! response protocols on top keep their own timeouts and retries.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use shielded_transport_pigeon::pump::{Event, LockfreeEndpoint, RingConfig};
use shielded_transport_pigeon::stp::{
    ConnectionKey, Dscp, IdentityKeyPair, NetworkThreadOptions, Pump, STPAddress,
    new_pump_with_options,
};

/// Which STP send queue of a connection a message travels on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    /// Small protocol messages; flushed to STP first.
    Control = 0,
    /// Large payloads such as whole blocks.
    Bulk = 1,
}

impl Channel {
    pub const ALL: [Channel; 2] = [Channel::Control, Channel::Bulk];

    fn queue(self) -> usize {
        self as usize
    }
}

/// The first byte of every message names its family, then its type within the family. Bit 7
/// picks the family:
/// - [`MESSAGE_FAMILY_BASE`] (0x00..=0x7F): the base protocol, PoW sync and transaction relay.
///   It does not depend on BFT, so it can ship without it. Its types are new_network's
///   `PACKET_TYPE_*`.
/// - [`MESSAGE_FAMILY_BFT`] (0x80..=0xFF): tenderlink. Bit 6 flags a status block after the
///   header, and bits 0..=5 are the type.
///
/// One byte routes any message to its consumer; STP cannot, since a received message does not
/// say which send queue it travelled on.
pub const MESSAGE_FAMILY_MASK: u8 = 0x80;
pub const MESSAGE_FAMILY_BASE: u8 = 0x00;
pub const MESSAGE_FAMILY_BFT: u8 = 0x80;

/// The family of a message, from its first byte.
pub fn message_family(first_byte: u8) -> u8 {
    first_byte & MESSAGE_FAMILY_MASK
}

/// Largest message either sync loop sends. A whole Zcash block (2 MB at most) plus its
/// header fits with room to spare; the receive side refuses anything bigger.
pub const MAX_MESSAGE_SIZE: usize = 4 << 20;

#[derive(Debug, Clone)]
pub struct TransportLimits {
    pub max_message_size: usize,
    pub rings: RingConfig,
    /// Bytes one (peer, channel) outbox may hold before new messages to it are dropped.
    pub peer_outbox_bytes: usize,
    /// Messages one (peer, channel) outbox may hold before new messages to it are dropped.
    pub peer_outbox_messages: usize,
    /// A (peer, channel)'s next message enters STP's send ring only if that STP queue has
    /// nothing untransmitted or the message still fits under this many bytes. It is
    /// clamped to the core's minimum per-peer queue capacity
    /// (`min(send_buffer_params.1, send_buffer_params.2)`) so a message in
    /// the shared ring is always admissible (see `flush_outboxes`).
    pub peer_high_water_bytes: u64,
    pub max_pending_connects: usize,
    /// A connection attempt with no outcome after this long is forgotten locally.
    pub connect_timeout: Duration,
    /// A connection the caller stops listing as wanted is closed once it is this old.
    pub unwanted_grace: Duration,
}

impl Default for TransportLimits {
    fn default() -> Self {
        Self {
            max_message_size: MAX_MESSAGE_SIZE,
            rings: RingConfig {
                send: 1024,
                control: 256,
                receive: 4096,
            },
            peer_outbox_bytes: 8 << 20,
            peer_outbox_messages: 4096,
            peer_high_water_bytes: 512 * 1024,
            max_pending_connects: 256,
            connect_timeout: Duration::from_secs(60),
            unwanted_grace: Duration::from_secs(30),
        }
    }
}

#[derive(Default)]
pub struct NetworkThreadPush {
    pub initiate_connections: Vec<STPAddress>,
    pub wanted_connections: Vec<(STPAddress, [u8; 64])>,
    /// Messages for [`Channel::Control`].
    pub send_unreliable: Vec<(ConnectionKey, Vec<u8>)>,
    /// Messages for [`Channel::Bulk`].
    pub send_bulk: Vec<(ConnectionKey, Vec<u8>)>,
}

#[derive(Default)]
pub struct NetworkThreadPull {
    pub current_connections: Vec<(STPAddress, [u8; 64])>,
    pub received_unreliable_messages: VecDeque<(ConnectionKey, Vec<u8>)>,
}

/// Local counters, plus STP's own ring counters at the time of the call.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TransportStats {
    pub outbox_dropped_messages: u64,
    pub outbox_dropped_bytes: u64,
    pub send_to_unknown_peer_dropped: u64,
    pub oversized_dropped: u64,
    pub connects_skipped: u64,
    pub connects_failed: u64,
    pub ring_sent: u64,
    pub ring_received: u64,
    pub ring_receive_dropped_full: u64,
    pub ring_inbound_refused_full: u64,
    pub ring_send_head_blocked: u64,
    pub ring_send_dropped_unknown: u64,
    pub ring_send_dropped_oversized: u64,
}

#[derive(Default)]
struct Outbox {
    messages: VecDeque<Vec<u8>>,
    bytes: usize,
}

impl Outbox {
    fn clear(&mut self) {
        self.messages.clear();
        self.bytes = 0;
    }
}

struct PeerState {
    address: STPAddress,
    handshake_hash: [u8; 64],
    connected_at: Instant,
    outboxes: [Outbox; 2], // indexed by Channel

    /// The caller no longer wants this session; a disconnect is owed to STP. The peer is
    /// hidden from callers meanwhile and leaves the table once STP accepts the disconnect.
    closing: bool,
}

struct PumpThread {
    cancel: Arc<AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl PumpThread {
    fn start(mut pump: Pump, port: u16) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&cancel);
        let join = std::thread::Builder::new()
            .name(format!("stp-pump-{port}"))
            .spawn(move || {
                // run_for parks on the socket/wake when idle, so this does not spin; the
                // slice bounds how long a stop request waits to be noticed.
                while !stop.load(Ordering::Acquire) {
                    pump.run_for(Duration::from_millis(50));
                }
                // Dropping the pump closes the UDP socket.
            })
            .expect("failed to spawn the STP network thread");
        Self {
            cancel,
            join: Some(join),
        }
    }

    fn stop(&mut self) {
        self.cancel.store(true, Ordering::Release);
        if let Some(join) = self.join.take() {
            if join.join().is_err() {
                eprintln!("STP network thread panicked");
            }
        }
    }
}

/// The transport of one sync loop. See the module docs.
pub struct NetworkThreadHandle {
    endpoint: LockfreeEndpoint,
    pump: PumpThread,
    my_public_keys: Vec<Vec<u8>>,
    limits: TransportLimits,
    peers: HashMap<ConnectionKey, PeerState>,
    connecting: HashMap<ConnectionKey, Instant>,
    /// Connects not yet accepted by STP's control ring; each is also in `connecting`.
    pending_connects: VecDeque<STPAddress>,
    stats: TransportStats,
    flush_cursor: usize,
}

impl Drop for NetworkThreadHandle {
    fn drop(&mut self) {
        // Close cancels (does not flush) and wakes the pump; then stop and join its thread.
        self.endpoint.close();
        self.pump.stop();
    }
}

/// Bind `port` and start the network thread. Same arguments as the 0.x function, with the
/// message and queue bounds from [`TransportLimits::default`].
pub fn new_network_thread(
    keypairs: Vec<IdentityKeyPair>,
    port: u16,
    max_pps: Option<u64>,
    send_buffer_params: (u32, u32, u32),
) -> NetworkThreadHandle {
    NetworkThreadHandle::new(
        keypairs,
        port,
        max_pps,
        send_buffer_params,
        TransportLimits::default(),
    )
}

/// One exchange with the network thread, the 0.x call shape. `None` means the network
/// thread has stopped (its socket failed or it panicked), as the 0.x `None` meant a dead or
/// wedged thread; callers keep their previous connection list rather than an empty one.
pub fn service_connections(
    handle: &mut NetworkThreadHandle,
    push: NetworkThreadPush,
) -> Option<NetworkThreadPull> {
    if handle.is_finished() {
        return None;
    }
    Some(handle.service(push))
}

impl NetworkThreadHandle {
    pub fn new(
        keypairs: Vec<IdentityKeyPair>,
        port: u16,
        max_pps: Option<u64>,
        send_buffer_params: (u32, u32, u32),
        limits: TransportLimits,
    ) -> Self {
        let (handle, pump) =
            Self::new_unstarted(keypairs, port, max_pps, send_buffer_params, limits);
        let mut handle = handle;
        handle.pump = PumpThread::start(pump, port);
        handle
    }

    /// The handle with its pump not yet running; tests drive the pump by hand.
    fn new_unstarted(
        keypairs: Vec<IdentityKeyPair>,
        port: u16,
        max_pps: Option<u64>,
        send_buffer_params: (u32, u32, u32),
        mut limits: TransportLimits,
    ) -> (Self, Pump) {
        // The core's per-peer queue capacity never drops below min(min, max) of these bounds.
        let core_floor = send_buffer_params.1.min(send_buffer_params.2) as u64;
        limits.peer_high_water_bytes = limits.peer_high_water_bytes.min(core_floor);
        let my_public_keys = keypairs.iter().map(|kp| kp.public.clone()).collect();
        let (pump, endpoint) = new_pump_with_options(
            keypairs,
            port,
            max_pps,
            send_buffer_params,
            limits.max_message_size,
            NetworkThreadOptions {
                rings: limits.rings.clone(),
                send_queue_count: Channel::ALL.len(),
                ..Default::default()
            },
        );
        let handle = Self {
            endpoint,
            pump: PumpThread {
                cancel: Arc::new(AtomicBool::new(false)),
                join: None,
            },
            my_public_keys,
            limits,
            peers: HashMap::new(),
            connecting: HashMap::new(),
            pending_connects: VecDeque::new(),
            stats: TransportStats::default(),
            flush_cursor: 0,
        };
        (handle, pump)
    }

    pub fn limits(&self) -> &TransportLimits {
        &self.limits
    }

    pub fn is_connected(&self, peer: ConnectionKey) -> bool {
        self.peers.get(&peer).is_some_and(|s| !s.closing)
    }

    /// Bytes handed to us for `peer` on `channel` and not yet transmitted: our outbox plus
    /// STP's queue. `None` if `peer` is not connected.
    pub fn send_backlog_bytes(&self, peer: ConnectionKey, channel: Channel) -> Option<u64> {
        let state = self.peers.get(&peer).filter(|s| !s.closing)?;
        let outbox = &state.outboxes[channel as usize];
        Some(
            (outbox.bytes as u64)
                .saturating_add(self.endpoint.queued_send_bytes(peer, channel.queue())),
        )
    }

    /// How many more bytes may be queued to `peer` on `channel` now without risking an
    /// outbox drop. Ask before building a large message; 0 means wait for the backlog.
    pub fn send_budget(&self, peer: ConnectionKey, channel: Channel) -> u64 {
        let Some(state) = self.peers.get(&peer).filter(|s| !s.closing) else {
            return 0;
        };
        if state.outboxes[channel as usize].messages.len() >= self.limits.peer_outbox_messages {
            return 0;
        }
        let backlog = self.send_backlog_bytes(peer, channel).unwrap_or(u64::MAX);
        (self.limits.peer_outbox_bytes as u64).saturating_sub(backlog)
    }

    pub fn stats(&self) -> TransportStats {
        let ring = self.endpoint.stats();
        TransportStats {
            ring_sent: ring.sent,
            ring_received: ring.received,
            ring_receive_dropped_full: ring.receive_dropped_full,
            ring_inbound_refused_full: ring.inbound_refused_full,
            ring_send_head_blocked: ring.send_head_blocked,
            ring_send_dropped_unknown: ring.send_dropped_unknown,
            ring_send_dropped_oversized: ring.send_dropped_oversized,
            ..self.stats
        }
    }

    /// Whether the network thread has stopped (socket closed or endpoint closed).
    pub fn is_finished(&self) -> bool {
        self.endpoint.is_finished()
    }

    pub fn service(&mut self, push: NetworkThreadPush) -> NetworkThreadPull {
        let now = Instant::now();
        let mut pull = NetworkThreadPull::default();

        self.drain_events(&mut pull.received_unreliable_messages, now);
        self.close_unwanted(&push.wanted_connections, now);
        self.connecting
            .retain(|_, started| now.duration_since(*started) < self.limits.connect_timeout);
        // Invariant: every queued connect is a live attempt and keys are unique, so the queue
        // never exceeds max_pending_connects even when attempts time out and are re-requested
        // while the control ring stays full.
        let connecting = &self.connecting;
        self.pending_connects
            .retain(|a| connecting.contains_key(&a.connection_key()));
        for address in push.initiate_connections {
            self.initiate(address, now);
        }
        self.flush_controls();
        for (peer, message) in push.send_unreliable {
            self.enqueue(peer, Channel::Control, message);
        }
        for (peer, message) in push.send_bulk {
            self.enqueue(peer, Channel::Bulk, message);
        }
        self.flush_outboxes();

        pull.current_connections = self
            .peers
            .values()
            .filter(|state| !state.closing)
            .map(|state| (state.address.clone(), state.handshake_hash))
            .collect();
        pull
    }

    fn drain_events(&mut self, received: &mut VecDeque<(ConnectionKey, Vec<u8>)>, now: Instant) {
        // Bounded: about one ring's worth plus what lands while we drain. The pump keeps
        // running on its own thread regardless of how often this is called.
        let max_events = self.limits.rings.receive.saturating_mul(2).max(64);
        for _ in 0..max_events {
            let Some(event) = self.endpoint.try_recv() else {
                break;
            };
            match event {
                Event::Connected {
                    peer,
                    address,
                    handshake_hash,
                } => {
                    self.connecting.remove(&peer);
                    let state = self.peers.entry(peer).or_insert_with(|| PeerState {
                        address: address.clone(),
                        handshake_hash,
                        connected_at: now,
                        outboxes: Default::default(),
                        closing: false,
                    });
                    if state.handshake_hash != handshake_hash {
                        // A new session to the same key: what was queued was for the old one.
                        state.address = address;
                        state.handshake_hash = handshake_hash;
                        state.connected_at = now;
                        state.outboxes.iter_mut().for_each(Outbox::clear);
                        state.closing = false;
                    }
                }
                Event::ConnectFailed { address, .. } => {
                    self.connecting.remove(&address.connection_key());
                    self.stats.connects_failed += 1;
                }
                Event::Disconnected { peer, .. } => {
                    self.peers.remove(&peer);
                }
                Event::Message { peer, data } => {
                    if self.is_connected(peer) {
                        received.push_back((peer, data));
                    }
                }
            }
        }
    }

    fn close_unwanted(&mut self, wanted: &[(STPAddress, [u8; 64])], now: Instant) {
        let wanted: HashSet<ConnectionKey> =
            wanted.iter().map(|(a, _)| a.connection_key()).collect();
        let grace = self.limits.unwanted_grace;
        for (peer, state) in self.peers.iter_mut() {
            if !state.closing
                && !wanted.contains(peer)
                && now.duration_since(state.connected_at) >= grace
            {
                // Idempotent intent: flagged once, delivered by flush_controls when the
                // control ring has room. What was still queued for it is abandoned.
                state.closing = true;
                state.outboxes.iter_mut().for_each(Outbox::clear);
            }
        }
    }

    fn initiate(&mut self, address: STPAddress, now: Instant) {
        if self.my_public_keys.iter().any(|k| *k == address.key) || address.key.len() < 2 {
            return;
        }
        let peer = address.connection_key();
        // A closing peer is not reconnected until its disconnect has gone out.
        if self.peers.contains_key(&peer) || self.connecting.contains_key(&peer) {
            return;
        }
        if self.connecting.len() >= self.limits.max_pending_connects {
            self.stats.connects_skipped += 1;
            return;
        }
        self.connecting.insert(peer, now);
        self.pending_connects.push_back(address);
    }

    fn flush_controls(&mut self) {
        // Disconnects first, so a teardown always precedes a later connect to the same key.
        let closing: Vec<ConnectionKey> = self
            .peers
            .iter()
            .filter(|(_, s)| s.closing)
            .map(|(k, _)| *k)
            .collect();
        for peer in closing {
            if !self.endpoint.try_disconnect(peer) {
                return; // ring full: the flags stay set, retried next service
            }
            self.peers.remove(&peer);
        }
        while let Some(address) = self.pending_connects.front() {
            if !self.endpoint.try_connect(address.clone()) {
                return; // ring full: retry from here next service, order preserved
            }
            self.pending_connects.pop_front();
        }
    }

    fn enqueue(&mut self, peer: ConnectionKey, channel: Channel, message: Vec<u8>) {
        let len = message.len();
        if len == 0 || len > self.limits.max_message_size {
            self.stats.oversized_dropped += 1;
            return;
        }
        let Some(state) = self.peers.get_mut(&peer).filter(|s| !s.closing) else {
            self.stats.send_to_unknown_peer_dropped += 1;
            return;
        };
        let outbox = &mut state.outboxes[channel as usize];
        if outbox.messages.len() >= self.limits.peer_outbox_messages
            || outbox.bytes + len > self.limits.peer_outbox_bytes
        {
            self.stats.outbox_dropped_messages += 1;
            self.stats.outbox_dropped_bytes += len as u64;
            return;
        }
        outbox.bytes += len;
        outbox.messages.push_back(message);
    }

    fn flush_outboxes(&mut self) {
        let mut keys: Vec<ConnectionKey> = self
            .peers
            .iter()
            .filter(|(_, s)| !s.closing && s.outboxes.iter().any(|o| !o.messages.is_empty()))
            .map(|(k, _)| *k)
            .collect();
        if keys.is_empty() {
            return;
        }
        keys.sort_unstable();
        // Rotate the start so no peer always gets first claim on the send ring.
        self.flush_cursor = self.flush_cursor.wrapping_add(1);
        let rotate = self.flush_cursor % keys.len();
        keys.rotate_left(rotate);

        // Control outboxes first, then bulk; within a channel round-robin, one message per
        // peer per round, until nothing more can move.
        for channel in Channel::ALL {
            loop {
                let mut progressed = false;
                for peer in &keys {
                    let Some(state) = self.peers.get_mut(peer) else {
                        continue;
                    };
                    let outbox = &mut state.outboxes[channel as usize];
                    let Some(front) = outbox.messages.front_mut() else {
                        continue;
                    };
                    let len = front.len();
                    // The send ring is one FIFO for every peer and queue: a message STP cannot
                    // take yet would sit at its head and stall all the others. STP's queue
                    // takes a message when it is empty, or when the message fits its capacity,
                    // which never drops below min(send_buffer_params.1, .2); `queued_send_bytes`
                    // counts at least the queue's fill plus what is still in the ring. With the
                    // high water clamped to that floor, this admission never blocks the head.
                    let queued = self.endpoint.queued_send_bytes(*peer, channel.queue());
                    if queued != 0
                        && queued
                            .checked_add(len as u64)
                            .is_none_or(|q| q > self.limits.peer_high_water_bytes)
                    {
                        continue;
                    }
                    if !self
                        .endpoint
                        .try_send(*peer, channel.queue(), Dscp::BestEffort, front)
                    {
                        return; // send ring full; the buffer was left in place
                    }
                    outbox.messages.pop_front();
                    outbox.bytes -= len;
                    progressed = true;
                }
                if !progressed {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use shielded_transport_pigeon::stp::{
        CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2b, new_keypair_from_connect_magic1,
    };
    use std::net::Ipv6Addr;

    fn free_port() -> u16 {
        let probe = std::net::UdpSocket::bind("[::1]:0").unwrap();
        probe.local_addr().unwrap().port()
    }

    fn node(limits: TransportLimits) -> (NetworkThreadHandle, STPAddress) {
        let key = new_keypair_from_connect_magic1(CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2b)
            .unwrap();
        let port = free_port();
        let address = STPAddress::from(Ipv6Addr::LOCALHOST, port, &key);
        let handle = NetworkThreadHandle::new(
            vec![key],
            port,
            None,
            (1_000_000, 512 * 1024, 32 << 20),
            limits,
        );
        (handle, address)
    }

    fn connect_pair(
        limits: TransportLimits,
    ) -> (
        NetworkThreadHandle,
        NetworkThreadHandle,
        ConnectionKey,
        ConnectionKey,
    ) {
        let (mut a, a_addr) = node(limits.clone());
        let (mut b, b_addr) = node(limits);
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut push = NetworkThreadPush {
            initiate_connections: vec![b_addr.clone()],
            ..Default::default()
        };
        loop {
            let pa = a.service(std::mem::take(&mut push));
            let pb = b.service(NetworkThreadPush::default());
            if !pa.current_connections.is_empty() && !pb.current_connections.is_empty() {
                break;
            }
            assert!(Instant::now() < deadline, "connect timed out");
            std::thread::sleep(Duration::from_millis(5));
        }
        (a, b, b_addr.connection_key(), a_addr.connection_key())
    }

    #[test]
    fn connect_send_receive_and_keep_wanted() {
        let (mut a, mut b, to_b, _) = connect_pair(TransportLimits::default());
        let big = vec![7u8; 2 << 20]; // a whole max-size block in one message
        let mut push = NetworkThreadPush {
            send_unreliable: vec![(to_b, b"hello".to_vec()), (to_b, big.clone())],
            ..Default::default()
        };
        let mut got = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(20);
        while got.len() < 2 {
            let pa = a.service(std::mem::take(&mut push));
            push.wanted_connections = pa.current_connections;
            let pb = b.service(NetworkThreadPush::default());
            got.extend(pb.received_unreliable_messages.into_iter().map(|(_, m)| m));
            assert!(
                Instant::now() < deadline,
                "receive timed out, got {}",
                got.len()
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(got.contains(&b"hello".to_vec()));
        assert!(got.contains(&big));
        assert!(a.is_connected(to_b));
    }

    #[test]
    fn outbox_is_bounded_and_budget_reports_backpressure() {
        let limits = TransportLimits {
            peer_outbox_bytes: 64 * 1024,
            peer_outbox_messages: 8,
            peer_high_water_bytes: 1, // nothing leaves the outbox while STP holds any bytes
            ..Default::default()
        };
        let (mut a, _b, to_b, _) = connect_pair(limits);
        assert_eq!(a.send_budget(to_b, Channel::Control), 64 * 1024);
        let msgs: Vec<_> = (0..32).map(|_| (to_b, vec![1u8; 16 * 1024])).collect();
        a.service(NetworkThreadPush {
            send_unreliable: msgs,
            ..Default::default()
        });
        let stats = a.stats();
        // At most a few were accepted (outbox bound + whatever reached STP), the rest dropped
        // and counted rather than buffered without limit.
        assert!(stats.outbox_dropped_messages >= 32 - 8 - 4, "{stats:?}");
        assert!(a.send_backlog_bytes(to_b, Channel::Control).unwrap() <= 64 * 1024 + 16 * 1024 * 4);
        assert!(a.send_budget(to_b, Channel::Control) < 64 * 1024);
        assert_eq!(
            a.send_budget(to_b, Channel::Bulk),
            64 * 1024,
            "channels are budgeted apart"
        );
        // Unknown peers and oversized messages are refused up front.
        let stranger = ConnectionKey {
            ip: Ipv6Addr::LOCALHOST,
            port: 1,
            key_15_bits: 0,
        };
        a.service(NetworkThreadPush {
            send_unreliable: vec![(stranger, vec![1]), (to_b, vec![0u8; MAX_MESSAGE_SIZE + 1])],
            ..Default::default()
        });
        let stats = a.stats();
        assert_eq!(stats.send_to_unknown_peer_dropped, 1);
        assert_eq!(stats.oversized_dropped, 1);
        assert_eq!(a.send_budget(stranger, Channel::Bulk), 0);
    }

    #[test]
    fn unwanted_connection_is_closed_after_grace() {
        let (mut a, mut b, to_b, to_a) = connect_pair(TransportLimits::default());
        a.limits.unwanted_grace = Duration::from_millis(0);
        let pa = a.service(NetworkThreadPush::default()); // nothing wanted any more
        assert!(pa.current_connections.is_empty());
        assert!(!a.is_connected(to_b));
        // STP 1.0 closes locally without telling the peer, so b still sends for a while;
        // none of it may resurrect the session on a or reach a's caller.
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            b.service(NetworkThreadPush {
                send_unreliable: vec![(to_a, b"late".to_vec())],
                ..Default::default()
            });
            let pa = a.service(NetworkThreadPush::default());
            assert!(pa.current_connections.is_empty());
            assert!(pa.received_unreliable_messages.is_empty());
            std::thread::sleep(Duration::from_millis(10));
        }
        // Sending to the closed peer is refused and counted, not queued.
        a.service(NetworkThreadPush {
            send_unreliable: vec![(to_b, vec![1])],
            ..Default::default()
        });
        assert_eq!(a.stats().send_to_unknown_peer_dropped, 1);
    }

    #[test]
    fn disconnect_intents_stay_bounded_while_the_control_ring_is_full() {
        // Pump not running, so nothing drains the 1-slot control ring until we say so.
        let key = new_keypair_from_connect_magic1(CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2b)
            .unwrap();
        let limits = TransportLimits {
            rings: RingConfig {
                send: 8,
                control: 1,
                receive: 8,
            },
            unwanted_grace: Duration::ZERO,
            max_pending_connects: 2,
            ..Default::default()
        };
        let (mut a, mut pump) = NetworkThreadHandle::new_unstarted(
            vec![key],
            free_port(),
            None,
            (1_000_000, 512 * 1024, 32 << 20),
            limits,
        );
        let peers: Vec<STPAddress> = (0..4)
            .map(|i| {
                let k = new_keypair_from_connect_magic1(
                    CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2b,
                )
                .unwrap();
                STPAddress::from(Ipv6Addr::LOCALHOST, 40_000 + i, &k)
            })
            .collect();
        for (i, address) in peers.iter().enumerate() {
            a.peers.insert(
                address.connection_key(),
                PeerState {
                    address: address.clone(),
                    handshake_hash: [i as u8; 64],
                    connected_at: Instant::now(),
                    outboxes: Default::default(),
                    closing: false,
                },
            );
        }
        let extra: Vec<STPAddress> = (0..5)
            .map(|i| {
                let k = new_keypair_from_connect_magic1(
                    CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2b,
                )
                .unwrap();
                STPAddress::from(Ipv6Addr::LOCALHOST, 41_000 + i, &k)
            })
            .collect();
        // Many services, nothing wanted, connects requested every time, ring never drained.
        for _ in 0..50 {
            let pull = a.service(NetworkThreadPush {
                initiate_connections: extra.clone(),
                ..Default::default()
            });
            assert!(
                pull.current_connections.is_empty(),
                "closing peers are hidden"
            );
            assert!(
                a.peers.len() <= 4 && a.peers.len() >= 3,
                "only one disconnect fits the ring"
            );
            assert!(a.peers.values().all(|s| s.closing));
            assert!(a.pending_connects.len() <= 2 && a.connecting.len() <= 2);
            // a closing peer is not reconnected before its disconnect went out
            a.service(NetworkThreadPush {
                initiate_connections: peers.clone(),
                ..Default::default()
            });
            assert!(
                a.connecting
                    .keys()
                    .all(|k| peers.iter().all(|p| p.connection_key() != *k))
            );
        }
        // Space returns: each pump pass takes the queued control, every teardown is delivered
        // in turn, and only then do the queued connects go out.
        for _ in 0..20 {
            pump.run_for(Duration::from_millis(1));
            a.service(NetworkThreadPush::default());
            if a.peers.is_empty() && a.pending_connects.is_empty() {
                break;
            }
        }
        assert!(a.peers.is_empty(), "{} peers still closing", a.peers.len());
        assert!(a.pending_connects.is_empty());
    }

    #[test]
    fn connect_intents_stay_bounded_across_timeouts_with_the_control_ring_full() {
        let key = new_keypair_from_connect_magic1(CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2b)
            .unwrap();
        let limits = TransportLimits {
            rings: RingConfig {
                send: 8,
                control: 1,
                receive: 8,
            },
            max_pending_connects: 1,
            connect_timeout: Duration::from_millis(1),
            ..Default::default()
        };
        let (mut a, mut pump) = NetworkThreadHandle::new_unstarted(
            vec![key],
            free_port(),
            None,
            (1_000_000, 512 * 1024, 32 << 20),
            limits,
        );
        let mk = |port| {
            let k =
                new_keypair_from_connect_magic1(CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2b)
                    .unwrap();
            STPAddress::from(Ipv6Addr::LOCALHOST, port, &k)
        };
        let (fill, target) = (mk(42_000), mk(42_001));
        // The first connect takes the only ring slot; nothing drains it.
        a.service(NetworkThreadPush {
            initiate_connections: vec![fill],
            ..Default::default()
        });
        for _ in 0..50 {
            std::thread::sleep(Duration::from_millis(2)); // past connect_timeout every time
            a.service(NetworkThreadPush {
                initiate_connections: vec![target.clone()],
                ..Default::default()
            });
            assert!(a.connecting.len() <= 1);
            assert!(
                a.pending_connects.len() <= 1,
                "{} queued connects",
                a.pending_connects.len()
            );
        }
        // Once the ring drains, the live attempt (and only it) goes out.
        a.limits.connect_timeout = Duration::from_secs(60);
        a.service(NetworkThreadPush {
            initiate_connections: vec![target.clone()],
            ..Default::default()
        });
        pump.run_for(Duration::from_millis(1));
        a.service(NetworkThreadPush::default());
        assert!(a.pending_connects.is_empty());
        assert!(a.connecting.contains_key(&target.connection_key()));
    }

    /// The send ring is one FIFO shared by every peer. Two max-size blocks queued for a peer
    /// whose STP queue drains slowly must not let the second block wait at the ring head,
    /// where it would hold back a small message to a healthy peer. Real pumps, real rings.
    #[test]
    fn slow_peer_bulk_does_not_block_healthy_peer() {
        let key = new_keypair_from_connect_magic1(CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2b)
            .unwrap();
        let port = free_port();
        // 400 packets/s: one ~2 MB block alone needs several seconds on the wire.
        let mut s = NetworkThreadHandle::new(
            vec![key],
            port,
            Some(400),
            (1_000_000, 512 * 1024, 32 << 20),
            TransportLimits::default(),
        );
        let (slow_key, slow_port) = (
            new_keypair_from_connect_magic1(CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2b)
                .unwrap(),
            free_port(),
        );
        let slow_addr = STPAddress::from(Ipv6Addr::LOCALHOST, slow_port, &slow_key);
        let (mut slow, mut slow_pump) = NetworkThreadHandle::new_unstarted(
            vec![slow_key],
            slow_port,
            None,
            (1_000_000, 512 * 1024, 32 << 20),
            TransportLimits::default(),
        );
        let (mut healthy, healthy_addr) = node(TransportLimits::default());
        let (to_slow, to_healthy) = (slow_addr.connection_key(), healthy_addr.connection_key());

        let mut push = NetworkThreadPush {
            initiate_connections: vec![slow_addr, healthy_addr],
            ..Default::default()
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while !(s.is_connected(to_slow) && s.is_connected(to_healthy)) {
            let pull = s.service(std::mem::take(&mut push));
            push.wanted_connections = pull.current_connections;
            slow_pump.run_for(Duration::from_millis(2));
            slow.service(NetworkThreadPush::default());
            healthy.service(NetworkThreadPush::default());
            assert!(Instant::now() < deadline, "connect timed out");
        }
        // From here the slow peer's pump is never run again.

        let block = vec![0xb1u8; 2_000_045];
        let started = Instant::now();
        push.send_bulk = vec![(to_slow, block.clone()), (to_slow, block)];
        push.send_unreliable = vec![(to_healthy, b"vote".to_vec())];
        // small messages behind a large one still flow to the healthy peer: several fit the floor
        push.send_unreliable
            .extend((0..8).map(|_| (to_healthy, vec![5u8; 32 * 1024])));
        let deadline = started + Duration::from_secs(3);
        let (mut vote_seen, mut small_received) = (false, 0);
        loop {
            let pull = s.service(std::mem::take(&mut push));
            push.wanted_connections = pull.current_connections;
            // the second block is held in our outbox, not at the shared ring head
            assert_eq!(
                s.peers[&to_slow].outboxes[Channel::Bulk as usize]
                    .messages
                    .len(),
                1
            );
            let got = healthy
                .service(NetworkThreadPush::default())
                .received_unreliable_messages;
            small_received += got.iter().filter(|(_, m)| m.len() == 32 * 1024).count();
            vote_seen |= got.iter().any(|(_, m)| m == b"vote");
            if vote_seen && small_received == 8 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "healthy peer starved behind slow peer's bulk"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        // ...and it was delivered while the first block was still untransmitted.
        assert!(s.endpoint.queued_send_bytes(to_slow, Channel::Bulk.queue()) > 0);
        assert!(s.send_budget(to_slow, Channel::Bulk) < (8 << 20) - 2_000_045);
    }

    #[test]
    fn high_water_never_exceeds_the_core_queue_floor() {
        let mk = |bounds: (u32, u32, u32), hw: u64| {
            let key =
                new_keypair_from_connect_magic1(CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2b)
                    .unwrap();
            let limits = TransportLimits {
                peer_high_water_bytes: hw,
                ..Default::default()
            };
            NetworkThreadHandle::new_unstarted(vec![key], free_port(), None, bounds, limits)
                .0
                .limits
                .peer_high_water_bytes
        };
        assert_eq!(mk((1_000_000, 512 * 1024, 32 << 20), 8 << 20), 512 * 1024);
        assert_eq!(
            mk((1_000_000, 512 * 1024, 32 << 20), 1000),
            1000,
            "lowering is allowed"
        );
        assert_eq!(
            mk((1_000_000, 512 * 1024, 64 * 1024), 8 << 20),
            64 * 1024,
            "min > max clamps to max"
        );
    }

    /// A control message to a peer is not queued behind a whole block to the same peer.
    #[test]
    fn control_overtakes_unfinished_bulk_to_the_same_peer() {
        let key = new_keypair_from_connect_magic1(CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2b)
            .unwrap();
        let port = free_port();
        // 400 packets/s: the block needs several seconds on the wire.
        let mut s = NetworkThreadHandle::new(
            vec![key],
            port,
            Some(400),
            (1_000_000, 512 * 1024, 32 << 20),
            TransportLimits::default(),
        );
        let (mut r, r_addr) = node(TransportLimits::default());
        let to_r = r_addr.connection_key();
        let mut push = NetworkThreadPush {
            initiate_connections: vec![r_addr],
            ..Default::default()
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while !s.is_connected(to_r) {
            let pull = s.service(std::mem::take(&mut push));
            push.wanted_connections = pull.current_connections;
            r.service(NetworkThreadPush::default());
            assert!(Instant::now() < deadline, "connect timed out");
            std::thread::sleep(Duration::from_millis(2));
        }
        let block = vec![0xb2u8; 2_000_045];
        push.send_bulk = vec![(to_r, block.clone())];
        let pull = s.service(std::mem::take(&mut push));
        push.wanted_connections = pull.current_connections;
        std::thread::sleep(Duration::from_millis(100)); // the block is on the wire first
        push.send_unreliable = vec![
            (to_r, b"status".to_vec()),
            (to_r, b"block-request".to_vec()),
        ];
        let started = Instant::now();
        let mut got: Vec<Vec<u8>> = Vec::new();
        while got.len() < 2 {
            let pull = s.service(std::mem::take(&mut push));
            push.wanted_connections = pull.current_connections;
            got.extend(
                r.service(NetworkThreadPush::default())
                    .received_unreliable_messages
                    .into_iter()
                    .map(|(_, m)| m),
            );
            assert!(
                !got.contains(&block),
                "block finished first; test rate too high"
            );
            assert!(
                started.elapsed() < Duration::from_secs(3),
                "control waited behind bulk: {got:?}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(got, vec![b"status".to_vec(), b"block-request".to_vec()]);
        assert!(
            s.endpoint.queued_send_bytes(to_r, Channel::Bulk.queue()) > 0,
            "block still untransmitted"
        );
    }

    #[test]
    fn fresh_addresses_round_trip_and_the_0x_selector_fails_closed() {
        use shielded_transport_pigeon::stp::{
            CONNECT_MAGIC1_PLAIN_TEXT, MAGIC2_APP_CROSSLINK, build_magic2_client_block,
            crypto_string_from_connect_magic1, negotiate_magic2, validate_magic2,
        };
        // STP 2.0 selectors; every peer upgrades together.
        assert_eq!(
            CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2b,
            0xa91a_aca6_f0f9
        );
        assert_eq!(CONNECT_MAGIC1_PLAIN_TEXT, 0x1f34_a6ae_39ab);
        assert_eq!(MAGIC2_APP_CROSSLINK, 0x8944_4182_b302_528e);

        // Crosslink's application magic2 is what the transitive negotiator offers and accepts.
        let block = build_magic2_client_block();
        assert_eq!(block[0], 1);
        assert_eq!(
            block[1..9],
            [0x8e, 0x52, 0x02, 0xb3, 0x82, 0x41, 0x44, 0x89]
        );
        assert_eq!(negotiate_magic2(&block), Some(MAGIC2_APP_CROSSLINK));
        assert!(validate_magic2(MAGIC2_APP_CROSSLINK));
        // The STP 1.0 Crosslink magic2 is neither offered nor accepted.
        assert!(!validate_magic2(0x5d74_7212_5631_44d9));
        let mut old_block = block;
        old_block[1..9].copy_from_slice(&0x5d74_7212_5631_44d9_u64.to_le_bytes());
        assert_eq!(negotiate_magic2(&old_block), None);

        for (magic1, selector) in [
            (CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2b, "-fCmrBqp"),
            (CONNECT_MAGIC1_PLAIN_TEXT, "qzmupjQf"),
        ] {
            let key = new_keypair_from_connect_magic1(magic1).unwrap();
            let address = STPAddress::from(Ipv6Addr::LOCALHOST, 39_210, &key);
            assert_eq!(address.magic1, magic1);
            let text = format!("{address:?}");
            assert!(text.contains(&format!(":{selector}:")), "{text}");
            let back = STPAddress::parse(&text).expect("printed address parses");
            assert_eq!(
                (back.ip, back.port, back.magic1, &back.key),
                (address.ip, address.port, address.magic1, &address.key)
            );
            // The same peer written with the 1.0 or 0.x selector is not a usable 2.0 address.
            for old_selector in ["11Q8bpGo", "1fgEw5Nx", "Gwn4skNt"] {
                let old = text.replace(selector, old_selector);
                assert!(STPAddress::parse(&old).is_none_or(|a| a.magic1 != magic1));
            }
        }
        // Old selectors: 1.0 encrypted, 1.0 plaintext, 0.x encrypted.
        for old in [0xa891_6e3c_54d7, 0x6d43_b2f8_091b, 0x7193_c304_f8d5] {
            assert!(crypto_string_from_connect_magic1(old).is_none(), "{old:#x}");
            assert!(new_keypair_from_connect_magic1(old).is_none(), "{old:#x}");
        }
    }

    #[test]
    fn built_in_peer_configs_use_current_stp_selector() {
        for (source, count) in [
            (
                include_str!("../../zebra-crosslink/zebrad/src/config.rs"),
                3,
            ),
            (
                include_str!("../../zebra-crosslink/dilated_regtest/node0.toml.in"),
                1,
            ),
            (
                include_str!("../../zebra-crosslink/dilated_regtest/node1.toml.in"),
                1,
            ),
        ] {
            let peers: Vec<_> = source
                .lines()
                .filter(|line| line.contains("]:12001:") || line.contains("]:12002:"))
                .map(|line| line.split('"').nth(1).expect("quoted peer address"))
                .collect();
            assert_eq!(peers.len(), count);
            for peer in peers {
                let address = STPAddress::parse(peer).expect("built-in peer must parse");
                assert_eq!(
                    address.magic1, CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2b,
                    "{peer}"
                );
                assert_eq!(format!("{address:?}"), peer);
            }
        }
    }

    #[test]
    fn self_connect_is_skipped_and_drop_joins_promptly() {
        let (mut a, a_addr) = node(TransportLimits::default());
        a.service(NetworkThreadPush {
            initiate_connections: vec![a_addr],
            ..Default::default()
        });
        assert!(a.connecting.is_empty());
        let started = Instant::now();
        drop(a);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "drop took {:?}",
            started.elapsed()
        );
    }
}
