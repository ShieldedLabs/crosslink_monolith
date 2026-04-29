use crate::native_sockets::{Dscp, Ipv6Addr, monotonic_clock_ns};
use nym_sdk::mixnet::MixnetClient;
use nym_ip_packet_requests::IpPair;
use std::cell::UnsafeCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

const NYM_META_RING_SIZE: usize = 256;
const NYM_BYTE_SLAB_SIZE: usize = 512 * 1024; // 500 KiB

#[derive(Clone, Copy)]
struct PacketMeta {
    offset: u32, // offset into the byte slab
    len: u32,    // payload length
}

// SPSC variable-length ring buffer.
// Writer packs payload bytes contiguously into the byte slab and records (offset, len) in the metadata ring.
// Reader reads metadata, copies bytes out, advances.
//
// Invariant: the byte region [meta[tail].offset .. meta[head-1].offset+len) is owned by unread entries.
// Writer must not overwrite that region.
pub struct NymRing {
    slab: UnsafeCell<Box<[u8; NYM_BYTE_SLAB_SIZE]>>,
    meta: [UnsafeCell<PacketMeta>; NYM_META_RING_SIZE],
    head: AtomicU64,       // writer advances after writing meta+slab
    tail: AtomicU64,       // reader advances after reading
    slab_write: AtomicU64, // monotonic byte offset into slab (writer only, mod SLAB_SIZE for actual index)
    slab_read: AtomicU64,  // monotonic byte offset into slab (reader only)
}

// Safety: single writer, single reader on separate threads. UnsafeCell access
// is gated by head/tail atomics ensuring no concurrent access to the same slot.
#[allow(unsafe_code)]
unsafe impl Sync for NymRing {}

#[allow(unsafe_code)]
fn new_nym_ring() -> NymRing {
    NymRing {
        slab: UnsafeCell::new(unsafe { Box::new_uninit().assume_init() }),
        meta: std::array::from_fn(|_| UnsafeCell::new(PacketMeta { offset: 0, len: 0 })),
        head: AtomicU64::new(0),
        tail: AtomicU64::new(0),
        slab_write: AtomicU64::new(0),
        slab_read: AtomicU64::new(0),
    }
}

/// Write a contiguous byte slice into the slab at `offset`, handling wrap-around.
#[allow(unsafe_code)]
fn slab_write(slab: &mut [u8; NYM_BYTE_SLAB_SIZE], mut offset: usize, data: &[u8]) -> usize {
    let first = data.len().min(NYM_BYTE_SLAB_SIZE - offset);
    slab[offset..offset + first].copy_from_slice(&data[..first]);
    if first < data.len() {
        slab[..data.len() - first].copy_from_slice(&data[first..]);
    }
    offset + data.len()
}

/// Try to push a framed packet (header + payload) into the ring. Returns true on success, false if full.
#[allow(unsafe_code)]
fn ring_try_push(ring: &NymRing, header: &[u8], payload: &[u8]) -> bool {
    let head = ring.head.load(Ordering::Relaxed);
    let tail = ring.tail.load(Ordering::Acquire);

    if head.wrapping_sub(tail) >= NYM_META_RING_SIZE as u64 {
        return false;
    }

    let total_len = header.len() + payload.len();
    let slab_w = ring.slab_write.load(Ordering::Relaxed);
    let slab_r = ring.slab_read.load(Ordering::Acquire);

    if (slab_w - slab_r) as usize + total_len > NYM_BYTE_SLAB_SIZE {
        return false;
    }

    let offset = (slab_w % NYM_BYTE_SLAB_SIZE as u64) as usize;
    let slab = unsafe { &mut *ring.slab.get() };

    let mid = slab_write(slab, offset, header);
    slab_write(slab, mid % NYM_BYTE_SLAB_SIZE, payload);

    ring.slab_write.store(slab_w + total_len as u64, Ordering::Relaxed);

    let meta_idx = (head % NYM_META_RING_SIZE as u64) as usize;
    unsafe {
        *ring.meta[meta_idx].get() = PacketMeta {
            offset: offset as u32,
            len: total_len as u32,
        };
    }

    ring.head.store(head + 1, Ordering::Release);
    true
}

/// Try to pop a packet from the ring into buf. Returns Some(len) on success, None if empty.
#[allow(unsafe_code)]
fn ring_try_pop(ring: &NymRing, buf: &mut [u8]) -> Option<usize> {
    let tail = ring.tail.load(Ordering::Relaxed);
    let head = ring.head.load(Ordering::Acquire);

    if tail == head {
        return None;
    }

    let meta_idx = (tail % NYM_META_RING_SIZE as u64) as usize;
    let meta = unsafe { *ring.meta[meta_idx].get() };
    let offset = meta.offset as usize;
    let pkt_len = meta.len as usize;
    let copy_len = pkt_len.min(buf.len());

    let slab = unsafe { &*ring.slab.get() };

    // Read payload, handling wrap-around
    let first = copy_len.min(NYM_BYTE_SLAB_SIZE - offset);
    buf[..first].copy_from_slice(&slab[offset..offset + first]);
    if first < copy_len {
        buf[first..copy_len].copy_from_slice(&slab[..copy_len - first]);
    }

    ring.slab_read.store(ring.slab_read.load(Ordering::Relaxed) + pkt_len as u64, Ordering::Release);
    ring.tail.store(tail + 1, Ordering::Release);

    Some(copy_len)
}

/// Drain all entries from a ring without reading the data.
fn ring_drain(ring: &NymRing) {
    let head = ring.head.load(Ordering::Acquire);
    ring.tail.store(head, Ordering::Release);
    ring.slab_read.store(ring.slab_write.load(Ordering::Acquire), Ordering::Release);
}

// Shared state for allocated IPs, written by Nym thread, read by realtime thread.
// Gated by `ready` flag — realtime thread only reads after ready is true.
pub struct NymSockShared {
    pub send_ring: NymRing,
    pub recv_ring: NymRing,
    pub ready: AtomicBool,
    pub allocated_ipv4: UnsafeCell<Ipv6Addr>, // v4-mapped v6, written once before ready=true
    pub allocated_ipv6: UnsafeCell<Ipv6Addr>, // written once before ready=true
}

#[allow(unsafe_code)]
unsafe impl Sync for NymSockShared {}

impl std::fmt::Debug for NymSockHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NymSockHandle").finish()
    }
}

#[derive(Clone)]
pub struct NymSockHandle {
    pub shared: Arc<NymSockShared>,
}

struct PendingNymSock {
    pub shared: Arc<NymSockShared>,
}

struct ActiveNymSock {
    pub shared: Arc<NymSockShared>,
    pub tunnel: smolmix::Tunnel,
    pub udp: smolmix::UdpSocket,
}

pub struct NymHandle {
    pub pending: Arc<std::sync::Mutex<Vec<PendingNymSock>>>,
}

pub fn nym_setup() -> NymHandle {
    NymHandle {
        pending: Arc::new(std::sync::Mutex::new(Vec::new())),
    }
}

/*
pub struct NymHandle {
    pub pending: Arc<std::sync::Mutex<Vec<PendingNymSock>>>,
    pub _nym_thread: std::thread::JoinHandle<()>,
}

pub fn nym_setup() -> NymHandle {
    nym_bin_common::logging::setup_tracing_logger();

    let pending: Arc<std::sync::Mutex<Vec<PendingNymSock>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let pending_clone = pending.clone();

    let nym_thread = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("Failed to create Nym tokio runtime");

        runtime.block_on(async move {
            let mut client = MixnetClient::connect_new().await
                .expect("Failed to connect to Nym mixnet");

            use crate::native_sockets::ASSUMED_BIGGEST_POSSIBLE_UDP_PAYLOAD_ON_EXISTING_HARDWARE;
            let mut active: Vec<ActiveNymSock> = Vec::new();
            let mut send_buf = vec![0u8; ASSUMED_BIGGEST_POSSIBLE_UDP_PAYLOAD_ON_EXISTING_HARDWARE];
            let mut recv_buf = vec![0u8; ASSUMED_BIGGEST_POSSIBLE_UDP_PAYLOAD_ON_EXISTING_HARDWARE];

            loop {
                // Phase 1: Check for new connection requests
                {
                    let mut queue = pending_clone.lock().unwrap();
                    for pending_sock in queue.drain(..) {
                        match setup_nym_sock(&mut client, pending_sock.shared.clone()).await {
                            Ok(active_sock) => active.push(active_sock),
                            Err(e) => eprintln!("Failed to setup Nym socket: {}", e),
                        }
                    }
                }

                // Phase 2: Drain all send rings
                for sock in active.iter() {
                    while let Some(len) = ring_try_pop(&sock.shared.send_ring, &mut send_buf) {
                        if len > SEND_HEADER_SIZE {
                            let ip = Ipv6Addr::from(<[u8; 16]>::try_from(&send_buf[..16]).unwrap());
                            let port = u16::from_be_bytes([send_buf[16], send_buf[17]]);
                            let payload = &send_buf[SEND_HEADER_SIZE..len];
                            let dst: std::net::SocketAddr = if let Some(v4) = ip.to_ipv4_mapped() {
                                (v4, port).into()
                            } else {
                                (ip, port).into()
                            };
                            match sock.udp.send_to(payload, dst).await {
                                Ok(n) => eprintln!("[nym_thread send] {} bytes -> {}", n, dst),
                                Err(e) => eprintln!("[nym_thread send] ERROR {} -> {}", e, dst),
                            }
                        }
                    }
                }

                // Phase 3: Remove dead connections
                active.retain(|sock| {
                    if Arc::strong_count(&sock.shared) == 1 {
                        let tunnel = &sock.tunnel;
                        tokio::spawn({
                            let tunnel = tunnel.clone();
                            async move { tunnel.shutdown().await; }
                        });
                        false
                    } else {
                        true
                    }
                });

                // Phase 4: Wait for receives or tick timeout, then loop back to drain sends
                let tick = tokio::time::sleep(std::time::Duration::from_millis(1));
                tokio::pin!(tick);

                'wait: loop {
                    if active.is_empty() {
                        tick.as_mut().await;
                        break 'wait;
                    }

                    // Build a future that resolves when any socket has data
                    // We poll each socket's recv in sequence; first one ready wins
                    let mut got_recv = false;
                    for sock in active.iter() {
                        tokio::select! {
                            _ = &mut tick => { break 'wait; }
                            result = sock.udp.recv_from(&mut recv_buf) => {
                                match result {
                                    Ok((len, src)) => {
                                        eprintln!("[nym_thread recv] {} bytes <- {}", len, src);
                                        let (src_ip, src_port) = match src {
                                            std::net::SocketAddr::V4(v4) => (v4.ip().to_ipv6_mapped(), v4.port()),
                                            std::net::SocketAddr::V6(v6) => (*v6.ip(), v6.port()),
                                        };
                                        let mut header = [0u8; RECV_HEADER_SIZE];
                                        header[0..16].copy_from_slice(&src_ip.octets());
                                        header[16..18].copy_from_slice(&src_port.to_be_bytes());
                                        header[18] = 0;
                                        header[19] = 0;
                                        ring_try_push(&sock.shared.recv_ring, &header, &recv_buf[..len]);
                                        got_recv = true;
                                    }
                                    Err(e) => { eprintln!("[nym_thread recv] ERROR {}", e); }
                                }
                                break; // got data from one socket, loop back to check all again
                            }
                        }
                    }
                    if !got_recv {
                        break 'wait; // tick fired
                    }
                    // Got a recv — loop 'wait to try more recvs before tick expires
                }
            }
        });
    });

    NymHandle {
        pending,
        _nym_thread: nym_thread,
    }
}
*/

async fn setup_nym_sock(
    client: &mut MixnetClient,
    shared: Arc<NymSockShared>,
) -> Result<ActiveNymSock, Box<dyn std::error::Error + Send + Sync>> {
    use nym_sdk::ip_packet_client::discovery::{create_nym_api_client, get_best_ipr};
    use nym_ip_packet_requests::v9;
    use nym_ip_packet_requests::v9::response::IpPacketResponse;
    use nym_ip_packet_requests::response_helpers;
    use tokio::io::AsyncWriteExt;

    // Discover best IPR
    let network_defaults = nym_sdk::NymNetworkDetails::new_mainnet();
    let api_urls = network_defaults.nym_api_urls.ok_or("No Nym API URL")?;
    let api_client = create_nym_api_client(api_urls)?;
    let ipr_address = get_best_ipr(api_client).await?;

    // Open stream to IPR on the shared client
    let mut stream = client.open_stream(ipr_address, Some(10)).await?;

    // IPR handshake: request tunnel, get allocated IPs
    let (request, request_id) = v9::new_connect_request(None);
    let request_bytes = request.to_bytes()?;
    stream.write_all(&request_bytes).await.map_err(|_| "Failed to send connect request")?;

    let timeout = tokio::time::sleep(std::time::Duration::from_secs(60));
    tokio::pin!(timeout);

    let allocated_ips: IpPair = loop {
        tokio::select! {
            _ = &mut timeout => {
                return Err("IPR connect response timeout".into());
            }
            result = stream.recv() => {
                let data = result.ok_or("IPR stream closed")?;
                if let Ok(response) = IpPacketResponse::from_bytes(&data) {
                    if response.id() == Some(request_id) {
                        let ips = response_helpers::parse_connect_response(response)
                            .map_err(|e| format!("IPR connect failed: {e}"))?;
                        break ips;
                    }
                }
            }
        }
    };

    // Build smolmix tunnel from our MixnetStream + allocated IPs
    let ipr_stream = nym_sdk::ipr_wrapper::IpMixStream::from_parts(stream, allocated_ips);
    let tunnel = smolmix::Tunnel::from_stream(ipr_stream).await
        .map_err(|e| format!("Failed to create smolmix tunnel: {e}"))?;
    let udp = tunnel.udp_socket().await
        .map_err(|e| format!("Failed to create UDP socket: {e}"))?;

    // Store allocated IPs
    #[allow(unsafe_code)]
    unsafe {
        *shared.allocated_ipv4.get() = allocated_ips.ipv4.to_ipv6_mapped();
        *shared.allocated_ipv6.get() = std::net::Ipv6Addr::from(allocated_ips.ipv6.octets());
    }

    // Drain any packets pushed before we were ready
    ring_drain(&shared.send_ring);

    shared.ready.store(true, Ordering::Release);

    Ok(ActiveNymSock { shared, tunnel, udp })
}

pub fn new_nym_sock(nym_handle: &NymHandle) -> NymSockHandle {
    let shared = Arc::new(NymSockShared {
        send_ring: new_nym_ring(),
        recv_ring: new_nym_ring(),
        ready: AtomicBool::new(false),
        allocated_ipv4: UnsafeCell::new(Ipv6Addr::UNSPECIFIED),
        allocated_ipv6: UnsafeCell::new(Ipv6Addr::UNSPECIFIED),
    });

    nym_handle.pending.lock().unwrap().push(PendingNymSock {
        shared: shared.clone(),
    });

    NymSockHandle { shared }
}

// Send ring format: [16B dst_ip6][2B dst_port BE][1B flags: bit0=ecn_signal][1B dscp][payload...]
const SEND_HEADER_SIZE: usize = 20;
// Recv ring format: [16B src_ip6][2B src_port BE][1B flags: bit0=congested, bit1=ecn_enabled][1B dscp][payload...]
const RECV_HEADER_SIZE: usize = 20;

/// Matching native_sockets API: returns send timestamp in nanoseconds.
pub fn nym_udp_send_with_congestion_and_dscp(
    nym_sock: &NymSockHandle,
    dst_ip6: Ipv6Addr,
    dst_port: u16,
    payload: &[u8],
    dscp: Dscp,
    ecn_signal: bool,
) -> u64 {
panic!("disabled");
    let mut header = [0u8; SEND_HEADER_SIZE];
    header[0..16].copy_from_slice(&dst_ip6.octets());
    header[16..18].copy_from_slice(&dst_port.to_be_bytes());
    header[18] = if ecn_signal { 1 } else { 0 };
    header[19] = dscp as u8;
    ring_try_push(&nym_sock.shared.send_ring, &header, payload);
    monotonic_clock_ns()
}

const MAX_FRAMED_RECV: usize = RECV_HEADER_SIZE + crate::native_sockets::ASSUMED_BIGGEST_POSSIBLE_UDP_PAYLOAD_ON_EXISTING_HARDWARE;

/// Matching native_sockets API: returns (len, src_ip6, src_port, congested, ecn_enabled, dscp, timestamp_ns).
pub fn nym_udp_recv_with_congestion_and_dscp(
    nym_sock: &NymSockHandle,
    buf: &mut [u8],
) -> std::io::Result<(usize, Ipv6Addr, u16, bool, bool, Dscp, u64)> {
panic!("disabled");
    let mut tmp = [0u8; MAX_FRAMED_RECV];
    match ring_try_pop(&nym_sock.shared.recv_ring, &mut tmp) {
        Some(total_len) if total_len > RECV_HEADER_SIZE => {
            let src_ip = Ipv6Addr::from(<[u8; 16]>::try_from(&tmp[..16]).unwrap());
            let src_port = u16::from_be_bytes([tmp[16], tmp[17]]);
            let flags = tmp[18];
            let congested = flags & 1 != 0;
            let ecn_enabled = flags & 2 != 0;
            let dscp = Dscp::from_u8(tmp[19]);
            let payload_len = total_len - RECV_HEADER_SIZE;
            let copy_len = payload_len.min(buf.len());
            buf[..copy_len].copy_from_slice(&tmp[RECV_HEADER_SIZE..RECV_HEADER_SIZE + copy_len]);
            let timestamp_ns = monotonic_clock_ns();
            Ok((copy_len, src_ip, src_port, congested, ecn_enabled, dscp, timestamp_ns))
        }
        _ => Err(std::io::Error::from(std::io::ErrorKind::WouldBlock)),
    }
}

/// Matching native_sockets API: returns (ipv4, ipv6) source addresses.
#[allow(unsafe_code)]
pub fn nym_udp_probe_source_addresses(
    nym_sock: &NymSockHandle,
) -> (Option<Ipv6Addr>, Option<Ipv6Addr>) {
panic!("disabled");
    if !nym_sock.shared.ready.load(Ordering::Acquire) {
        return (None, None);
    }
    unsafe {
        (Some(*nym_sock.shared.allocated_ipv4.get()), Some(*nym_sock.shared.allocated_ipv6.get()))
    }
}
