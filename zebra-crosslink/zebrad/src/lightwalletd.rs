
// The FFI glue to nghttp2 is the one place in zebrad that needs unsafe.
#![allow(unsafe_code)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};

use hex::ToHex;
use libnghttp2_sys as ng;
use prost::Message;
use tokio::task::JoinHandle;
use tower::{util::BoxService, Service, ServiceExt};

use zcash_client_backend::proto::compact_formats::{
    CompactBlock, CompactOrchardAction, CompactSaplingOutput, CompactSaplingSpend, CompactTx,
};
use zcash_client_backend::proto::service::{
    Address, AddressList, Balance, BlockId, BlockRange, BondInfoRequest, BondInfoResponse, Bytes,
    Duration, Exclude, FaucetRequest, FaucetResponse, GetAddressUtxosArg, GetAddressUtxosReply,
    GetAddressUtxosReplyList, GetSubtreeRootsArg, LightdInfo, PingResponse, RawTransaction,
    SendResponse, SubtreeRoot, TransparentAddressBlockFilter, TreeState, TxFilter,
};

use zebra_chain::block::{self, Block, Height};
use zebra_chain::chain_tip::ChainTip;
use zebra_chain::parameters::{Network, NetworkUpgrade};
use zebra_chain::serialization::{ZcashDeserialize, ZcashSerialize};
use zebra_chain::subtree::NoteCommitmentSubtreeIndex;
use zebra_chain::transaction::{self, Transaction, UnminedTxId};
use zebra_chain::transparent;

use zebra_node_services::mempool;
use zebra_node_services::mempool::{MempoolChange, MempoolChangeKind, MempoolTxSubscriber};
use zebra_state::crosslink::{TFLServiceError, TFLServiceRequest, TFLServiceResponse};
use zebra_state::{HashOrHeight, LatestChainTip, ReadRequest, ReadResponse, ReadStateService};

type BoxError = Box<dyn std::error::Error + Send + Sync>;
type MempoolSvc =
    tower::buffer::Buffer<BoxService<mempool::Request, mempool::Response, BoxError>, mempool::Request>;
type TflSvc = tower::buffer::Buffer<
    BoxService<TFLServiceRequest, TFLServiceResponse, TFLServiceError>,
    TFLServiceRequest,
>;

/// Everything the handlers need. All handles are cheap clones.
#[derive(Clone)]
#[allow(missing_docs)]
pub struct Ctx {
    pub rt: tokio::runtime::Handle,
    pub read_state: ReadStateService,
    pub mempool: MempoolSvc,
    pub tfl: TflSvc,
    pub tip: LatestChainTip,
    pub mempool_events: MempoolTxSubscriber,
    pub network: Network,
}

/// Where lightwallet_server listens, and what callers from other hosts must present.
#[derive(Clone)]
pub struct Listen {
    /// The gRPC address the operator chose. Loopback on its port is served beside it.
    pub grpc: SocketAddr,
    /// The readiness endpoint's port, always on loopback.
    pub ready_port: u16,
    /// What a caller from another host sends as `authorization: Bearer <token>`.
    pub token: Option<String>,
    /// A public lightwalletd: callers from other hosts may use every method but the faucet
    /// without the token.
    pub public: bool,
}

/// Shows whether a token is set, never the token.
impl std::fmt::Debug for Listen {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Listen")
            .field("grpc", &self.grpc)
            .field("ready_port", &self.ready_port)
            .field("token", &self.token.as_ref().map(|_| "[REDACTED]"))
            .field("public", &self.public)
            .finish()
    }
}

impl Listen {
    /// The gRPC addresses to bind: the chosen one first, then each loopback address it does
    /// not cover, so the node's own wallet reaches the server however `localhost` resolves.
    /// `[::]` is dual-stack on Linux and macOS, where the 0.0.0.0 bind after it fails
    /// harmlessly with AddrInUse, but v6-only on Windows, where it is needed.
    fn grpc_binds(&self) -> Vec<SocketAddr> {
        let port = self.grpc.port();
        let v6_loopback = SocketAddr::from((Ipv6Addr::LOCALHOST, port));
        let v4_loopback = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let mut binds = vec![self.grpc];
        match self.grpc.ip() {
            IpAddr::V6(ip) if ip.is_unspecified() => binds.push(SocketAddr::from((Ipv4Addr::UNSPECIFIED, port))),
            IpAddr::V4(ip) if ip.is_unspecified() => binds.push(v6_loopback),
            ip if ip == v6_loopback.ip() => binds.push(v4_loopback),
            ip if ip == v4_loopback.ip() => binds.push(v6_loopback),
            _ => binds.extend([v6_loopback, v4_loopback]),
        }
        binds
    }
}

// -------------------------------------------------------------------------
// gRPC basics

const GRPC_OK: u32 = 0;
const GRPC_UNKNOWN: u32 = 2;
const GRPC_INVALID: u32 = 3;
const GRPC_NOT_FOUND: u32 = 5;
const GRPC_RESOURCE: u32 = 8;
const GRPC_OUT_OF_RANGE: u32 = 11;
const GRPC_UNIMPLEMENTED: u32 = 12;
const GRPC_INTERNAL: u32 = 13;
const GRPC_UNAVAILABLE: u32 = 14;
const GRPC_PERMISSION_DENIED: u32 = 7;
const GRPC_UNAUTHENTICATED: u32 = 16;

/// gRPC error: (status code, message).
type Grr = (u32, String);

const REQ_MAX: usize = 4 * 1024 * 1024;
const OUT_BUDGET: usize = 256 * 1024;
/// Backend reads per spawned task for the streaming generators.
const CHUNK: i64 = 32;
const PATH_PREFIX: &[u8] = b"/cash.z.wallet.sdk.rpc.CompactTxStreamer/";

/// Connections from other hosts, in all and from any one prefix (`peer_prefix`). Loopback
/// connections, the node's own wallet's among them, are not counted, so other hosts cannot crowd
/// them out.
const MAX_REMOTE_CONNS: usize = 256;
const MAX_CONNS_PER_PREFIX: usize = 16;
/// A connection from another host with no request in progress and no new request for this
/// long is closed. Tests use seconds, so they can wait the timers out.
#[cfg(not(test))]
const IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
#[cfg(test)]
const IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(4);
/// A connection that has sent nothing for KEEPALIVE_INTERVAL is sent a PING, and closed if
/// the ACK takes longer than KEEPALIVE_TIMEOUT.
#[cfg(not(test))]
const KEEPALIVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);
#[cfg(test)]
const KEEPALIVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);
#[cfg(not(test))]
const KEEPALIVE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
#[cfg(test)]
const KEEPALIVE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);
/// Readiness-probe connections at once, and how long one may take to send its request.
const MAX_READY_CONNS: usize = 32;
const READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// The longest `authorization` header kept; a longer one cannot match the token.
const AUTHORIZATION_MAX: usize = 1024;

/// Transparent addresses one request may name.
const MAX_ADDRESSES: usize = 256;
/// UTXOs one GetAddressUtxos(Stream) answer may carry, whatever `max_entries` asks.
const MAX_UTXO_ENTRIES: usize = 10_000;
/// Blocks one GetBlockRange(Nullifiers) request may span. The node's own wallet asks for
/// 1,024 at a time.
const MAX_BLOCK_RANGE: u32 = 10_000;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Method {
    GetLatestBlock,
    GetBlock,
    GetBlockNullifiers,
    GetBlockRange,
    GetBlockRangeNullifiers,
    GetTransaction,
    GetRoster,
    SendTransaction,
    GetTaddressTransactions,
    GetTaddressBalance,
    GetTaddressBalanceStream,
    GetMempoolTx,
    GetMempoolStream,
    GetTreeState,
    GetLatestTreeState,
    GetSubtreeRoots,
    GetAddressUtxos,
    GetAddressUtxosStream,
    GetBondInfo,
    RequestFaucetDonation,
    GetLightdInfo,
    Ping,
    Unknown,
}

/// Frame one protobuf message with the 5-byte gRPC prefix.
fn enc<M: Message>(m: &M) -> Vec<u8> {
    let body = m.encode_to_vec();
    let mut v = Vec::with_capacity(5 + body.len());
    v.push(0); // not compressed
    v.extend_from_slice(&(body.len() as u32).to_be_bytes());
    v.extend_from_slice(&body);
    v
}

/// The single message of a request body (gRPC 5-byte-prefix framing).
fn one_msg(body: &[u8]) -> Result<&[u8], Grr> {
    // split_msgs
    let mut msgs = Vec::new();
    let mut p = 0usize;
    while p < body.len() {
        if body.len() - p < 5 {
            return Err((GRPC_INVALID, "truncated grpc frame".into()));
        }
        if body[p] != 0 {
            return Err((GRPC_UNIMPLEMENTED, "compressed messages not supported".into()));
        }
        let len = u32::from_be_bytes(body[p + 1..p + 5].try_into().unwrap()) as usize;
        if len > REQ_MAX || body.len() - p - 5 < len {
            return Err((GRPC_RESOURCE, "message too large or truncated".into()));
        }
        msgs.push(&body[p + 5..p + 5 + len]);
        p += 5 + len;
    }

    match msgs.len() {
        1 => Ok(msgs[0]),
        0 => Ok(&[]), // an absent message decodes as all-defaults, matching tonic
        _ => Err((GRPC_INVALID, "expected exactly one request message".into())),
    }
}

fn dec<M: Message + Default>(bytes: &[u8]) -> Result<M, Grr> {
    M::decode(bytes).map_err(|e| (GRPC_INVALID, format!("bad request proto: {e}")))
}

/// Refuses a request naming more than MAX_ADDRESSES transparent addresses.
fn too_many_addresses(count: usize) -> Result<(), Grr> {
    if count > MAX_ADDRESSES {
        return Err((GRPC_INVALID, format!("a request may name at most {MAX_ADDRESSES} addresses")));
    }
    Ok(())
}

fn internal(e: impl std::fmt::Display) -> Grr {
    (GRPC_INTERNAL, e.to_string())
}

/// Collect a finished runtime task's result; a JoinError (panic/abort) becomes
/// INTERNAL. Only call once `is_finished()` — then this returns instantly.
fn reap<T>(rt: &tokio::runtime::Handle, task: JoinHandle<Result<T, Grr>>) -> Result<T, Grr> {
    match rt.block_on(task) {
        Ok(r) => r,
        Err(e) => Err((GRPC_INTERNAL, format!("task failed: {e}"))),
    }
}

// -------------------------------------------------------------------------
// Per-stream state
//
// All backend I/O runs as tasks spawned onto the tokio runtime; the server
// thread only ever polls `is_finished()` and reaps. It never blocks, so one
// slow backend call (e.g. the TFL roster) cannot stall other streams.

enum Work {
    None,
    /// Pre-encoded messages, drained under the out-buffer budget.
    Items(VecDeque<Vec<u8>>),
    /// A spawned handler running on the runtime; resolves to the next Work.
    /// Unary methods resolve to `Items` with their one response frame.
    Pending(JoinHandle<Result<Work, Grr>>),
    /// Compact blocks over a height range (inclusive), CHUNK reads per task.
    /// The task result's bool is "hit a height past the best chain".
    Blocks {
        next: u32,
        end: u32,
        step: i64,
        nulls: bool,
        inflight: Option<(i64, JoinHandle<Result<(Vec<Vec<u8>>, bool), Grr>>)>,
    },
    /// Full transactions by txid, CHUNK reads per task.
    Txs {
        hashes: Vec<transaction::Hash>,
        i: usize,
        inflight: Option<(usize, JoinHandle<Result<Vec<Vec<u8>>, Grr>>)>,
    },
    /// Live mempool feed; closes when the chain tip moves.
    Mempool {
        backlog: VecDeque<Vec<u8>>,
        rx: tokio::sync::broadcast::Receiver<MempoolChange>,
        seen: HashSet<UnminedTxId>,
        tip0: block::Hash,
        inflight: Option<JoinHandle<Result<Vec<Vec<u8>>, Grr>>>,
    },
}

struct Stream {
    path: Vec<u8>,
    /// The `authorization` header, empty if absent or over AUTHORIZATION_MAX.
    authorization: Vec<u8>,
    req: Vec<u8>,
    req_done: bool,
    dispatched: bool,
    out: VecDeque<u8>,
    done: bool,
    status: u32,
    status_msg: String,
    deferred: bool,
    work: Work,
}

impl Stream {
    fn push(&mut self, framed: Vec<u8>) {
        self.out.extend(framed);
    }

    fn finish(&mut self, status: u32, msg: impl Into<String>) {
        self.done = true;
        self.status = status;
        self.status_msg = msg.into();
        self.work = Work::None;
    }
}

struct Conn {
    session: *mut ng::nghttp2_session,
    sock: TcpStream,
    streams: HashMap<i32, Stream>,
    /// Bytes produced by mem_send that the socket wouldn't take yet.
    pending: Vec<u8>,
    pending_off: usize,
    dead: bool,
    /// The prefix the peer's connections are counted under (`peer_prefix`).
    peer_prefix: IpAddr,
    /// The peer is on this host, as the node's own wallet is: trusted and not counted.
    loopback: bool,
    /// When the peer last sent anything.
    last_recv: std::time::Instant,
    /// When the peer last began or closed a request.
    last_used: std::time::Instant,
    /// When the keepalive PING still unanswered was sent.
    ping_sent: Option<std::time::Instant>,
    /// GOAWAY is queued; the connection closes once it is sent.
    closing: bool,
}

/// The prefix a peer's connections are counted under for MAX_CONNS_PER_PREFIX: an IPv4 address's
/// /16, as Bitcoin groups peers, so one holder of many addresses in a block counts once, and an
/// IPv6 address's /64, since one host or site is routinely given a whole /64 and could otherwise
/// open MAX_CONNS_PER_PREFIX from each of its addresses. `ip` is canonical, so a v4-mapped peer
/// counts as IPv4.
fn peer_prefix(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, _, _] = v4.octets();
            IpAddr::V4(Ipv4Addr::new(a, b, 0, 0))
        }
        IpAddr::V6(v6) => {
            let mut segments = v6.segments();
            segments[4..].fill(0);
            IpAddr::V6(Ipv6Addr::from(segments))
        }
    }
}

/// Whether `authorization` is `Bearer <token>`, compared in time that depends only on lengths.
fn bearer_matches(authorization: &[u8], token: &str) -> bool {
    let expected = [b"Bearer ".as_slice(), token.as_bytes()].concat();
    authorization.len() == expected.len()
        && authorization.iter().zip(&expected).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

impl Drop for Conn {
    fn drop(&mut self) {
        // Sessions retain stream callbacks pointing into Conn; destroy them
        // before streams and socket, including the server's shutdown path.
        unsafe { ng::nghttp2_session_del(self.session) };
    }
}

// -------------------------------------------------------------------------
// Server loop

/// Spawn the server thread. Serves gRPC (h2c plaintext) on `listen.grpc` and on loopback at its
/// port. Also answers any HTTP/1.1 POST on `listen.ready_port` (lightwallet_server's legacy
/// JSON-RPC port, always loopback) with a 200, which is what the wallet's
/// `wait_for_lightwalletd()` readiness probe needs.
pub fn lightwalletd_spawn(ctx: Ctx, listen: Listen) -> std::thread::JoinHandle<()> {
    spawn_server(ctx, listen, true, Default::default())
}

/// [`lightwalletd_spawn`], with loopback peers treated as other hosts unless
/// `loopback_trusted`, which lets tests reach the paths other hosts take. The server also
/// stops when `stop` is set, so a test can end it before its runtime goes away.
fn spawn_server(
    ctx: Ctx,
    listen: Listen,
    loopback_trusted: bool,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("lightwallet_server-grpc".into())
        .spawn(move || {
            // serve
            let port = listen.grpc.port();
            let ready_port = listen.ready_port;
            let mut listeners: Vec<TcpListener> = Vec::new();
            for (i, addr) in listen.grpc_binds().into_iter().enumerate() {
                match TcpListener::bind(addr) {
                    Ok(l) => {
                        l.set_nonblocking(true).expect("nonblocking listener");
                        tracing::info!("lightwallet_server gRPC serving on {addr}");
                        listeners.push(l);
                    }
                    // The rest are loopback addresses the chosen one may already cover.
                    Err(e) if i == 0 => tracing::error!("lightwallet_server: cannot serve gRPC on {addr}: {e}"),
                    Err(_) => {}
                }
            }
            if listeners.is_empty() {
                tracing::error!("lightwallet_server: cannot bind port {port} on any stack");
                return;
            }

            let mut ready_listeners: Vec<TcpListener> = Vec::new();
            for addr in [format!("[::1]:{ready_port}"), format!("127.0.0.1:{ready_port}")] {
                if let Ok(l) = TcpListener::bind(&*addr) {
                    l.set_nonblocking(true).expect("nonblocking listener");
                    ready_listeners.push(l);
                }
            }
            // (sock, request bytes so far, accepted at)
            let mut ready_conns: Vec<(TcpStream, Vec<u8>, std::time::Instant)> = Vec::new();

            let mut conns: Vec<Box<Conn>> = Vec::new();
            let token = listen.token.clone();
            let public = listen.public;
            // Connections refused for the caps since the last warning, which goes out at most
            // once a minute.
            let mut refused: u64 = 0;
            let mut refused_warned: Option<std::time::Instant> = None;

            while !zebra_chain::shutdown::is_shutting_down() && !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let mut progress = false;
                let now = std::time::Instant::now();

                // readiness probe endpoint: 200 any complete HTTP request
                for l in &ready_listeners {
                    loop {
                        match l.accept() {
                            Ok((sock, _)) => {
                                // dropping the socket closes it
                                if ready_conns.len() < MAX_READY_CONNS && sock.set_nonblocking(true).is_ok() {
                                    ready_conns.push((sock, Vec::new(), now));
                                    progress = true;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                }
                ready_conns.retain_mut(|(sock, buf, accepted)| {
                    if now.duration_since(*accepted) > READY_TIMEOUT {
                        return false;
                    }
                    let mut tmp = [0u8; 4096];
                    loop {
                        match sock.read(&mut tmp) {
                            Ok(0) => return false,
                            Ok(n) => {
                                buf.extend_from_slice(&tmp[..n]);
                                progress = true;
                            }
                            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                            Err(_) => return false,
                        }
                    }
                    if buf.len() > 64 * 1024 {
                        return false;
                    }
                    let Some(hdr_end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
                        return true;
                    };
                    let content_length = std::str::from_utf8(&buf[..hdr_end])
                        .ok()
                        .and_then(|hdrs| {
                            hdrs.lines().find_map(|ln| {
                                let (name, value) = ln.split_once(':')?;
                                name.eq_ignore_ascii_case("content-length")
                                    .then(|| value.trim().parse::<usize>().ok())?
                            })
                        })
                        .unwrap_or(0);
                    if buf.len() < hdr_end + 4 + content_length {
                        return true;
                    }
                    // the probe only checks for a 2xx status; the tiny response
                    // fits in the socket buffer, so blocking-free write_all is fine
                    let body = br#"{"jsonrpc":"2.0","result":{"ready":true},"id":1}"#;
                    let head = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = sock.write_all(head.as_bytes());
                    let _ = sock.write_all(body);
                    progress = true;
                    false
                });

                for listener in &listeners {
                    loop {
                        match listener.accept() {
                            Ok((sock, peer)) => {
                                let peer_ip = peer.ip().to_canonical();
                                let peer_prefix = peer_prefix(peer_ip);
                                let loopback = loopback_trusted && peer_ip.is_loopback();
                                if !loopback {
                                    let remote = conns.iter().filter(|c| !c.loopback).count();
                                    let from_prefix = conns.iter().filter(|c| c.peer_prefix == peer_prefix).count();
                                    if remote >= MAX_REMOTE_CONNS || from_prefix >= MAX_CONNS_PER_PREFIX {
                                        // dropping the socket closes it
                                        refused += 1;
                                        if refused_warned.is_none_or(|warned| now.duration_since(warned) >= std::time::Duration::from_secs(60)) {
                                            tracing::warn!(
                                                "lightwallet_server: refused {refused} connection(s) over the caps \
                                                 ({MAX_REMOTE_CONNS} from other hosts, {MAX_CONNS_PER_PREFIX} from one IPv4 /16 \
                                                 or IPv6 /64); \
                                                 the last from {peer_ip}",
                                            );
                                            refused = 0;
                                            refused_warned = Some(now);
                                        }
                                        continue;
                                    }
                                }
                                // new_conn
                                let conn = 'mk: {
                                    if sock.set_nonblocking(true).is_err() {
                                        break 'mk None;
                                    }
                                    sock.set_nodelay(true).ok();

                                    let mut conn = Box::new(Conn {
                                        session: std::ptr::null_mut(),
                                        sock,
                                        streams: HashMap::new(),
                                        pending: Vec::new(),
                                        pending_off: 0,
                                        dead: false,
                                        peer_prefix,
                                        loopback,
                                        last_recv: now,
                                        last_used: now,
                                        ping_sent: None,
                                        closing: false,
                                    });
                                    let user_data: *mut Conn = &mut *conn;

                                    unsafe {
                                        let mut cbs: *mut ng::nghttp2_session_callbacks =
                                            std::ptr::null_mut();
                                        if ng::nghttp2_session_callbacks_new(&mut cbs) != 0 {
                                            break 'mk None;
                                        }
                                        ng::nghttp2_session_callbacks_set_on_begin_headers_callback(
                                            cbs,
                                            Some(cb_begin_headers),
                                        );
                                        ng::nghttp2_session_callbacks_set_on_header_callback(
                                            cbs,
                                            Some(cb_header),
                                        );
                                        ng::nghttp2_session_callbacks_set_on_data_chunk_recv_callback(
                                            cbs,
                                            Some(cb_data_chunk),
                                        );
                                        ng::nghttp2_session_callbacks_set_on_frame_recv_callback(
                                            cbs,
                                            Some(cb_frame_recv),
                                        );
                                        ng::nghttp2_session_callbacks_set_on_stream_close_callback(
                                            cbs,
                                            Some(cb_stream_close),
                                        );

                                        let mut session: *mut ng::nghttp2_session = std::ptr::null_mut();
                                        let rc = ng::nghttp2_session_server_new(
                                            &mut session,
                                            cbs,
                                            user_data as *mut _,
                                        );
                                        ng::nghttp2_session_callbacks_del(cbs);
                                        if rc != 0 {
                                            break 'mk None;
                                        }
                                        conn.session = session;

                                        let settings = [ng::nghttp2_settings_entry {
                                            settings_id: ng::NGHTTP2_SETTINGS_MAX_CONCURRENT_STREAMS
                                                as i32,
                                            value: 128,
                                        }];
                                        ng::nghttp2_submit_settings(
                                            session,
                                            ng::NGHTTP2_FLAG_NONE as u8,
                                            settings.as_ptr(),
                                            settings.len(),
                                        );
                                    }
                                    Some(conn)
                                };
                                if let Some(conn) = conn {
                                    conns.push(conn);
                                    progress = true;
                                }
                            }
                            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                            Err(e) => {
                                tracing::warn!("lightwallet_server: accept error: {e}");
                                break;
                            }
                        }
                    }
                }

                for conn in conns.iter_mut() {
                    let c: *mut Conn = &mut **conn;

                    // pump: one lap for one connection: flush, read, dispatch,
                    // advance, send. Callbacks fire synchronously inside
                    // `mem_recv`/`mem_send` on this thread, so no Rust borrow of
                    // `Conn` fields may be held across those FFI calls.
                    progress |= unsafe {
                        'pump: {
                            let mut lap = false;
                            let session = (*c).session;

                            // 0. keepalive, and closing what is dead or idle. A request in
                            // progress, long-polls included, keeps a connection open; one that
                            // never finished arriving does not. Loopback connections, the
                            // node's own wallet's among them, are never closed as idle.
                            if let Some(sent) = (*c).ping_sent {
                                if now.duration_since(sent) > KEEPALIVE_TIMEOUT {
                                    (*c).dead = true;
                                    break 'pump true;
                                }
                            } else if now.duration_since((*c).last_recv) > KEEPALIVE_INTERVAL {
                                ng::nghttp2_submit_ping(session, ng::NGHTTP2_FLAG_NONE as u8, std::ptr::null());
                                (*c).ping_sent = Some(now);
                                lap = true;
                            }
                            if !(*c).closing
                                && !(*c).loopback
                                && !(*c).streams.values().any(|s| s.dispatched)
                                && now.duration_since((*c).last_used) > IDLE_TIMEOUT
                            {
                                // GOAWAY goes out below; the session then wants neither read
                                // nor write, and the connection is dropped.
                                ng::nghttp2_session_terminate_session(session, ng::NGHTTP2_NO_ERROR);
                                (*c).closing = true;
                                lap = true;
                            }

                            // 1. flush bytes the socket wouldn't take last lap
                            if !(*c).pending.is_empty() {
                                // flush_pending
                                let off = (*c).pending_off;
                                let pending: &Vec<u8> = &(*c).pending;
                                let chunk: Vec<u8> = pending[off..].to_vec();

                                // write_some
                                let mut written = 0usize;
                                let mut broken = false;
                                {
                                    let mut data = &chunk[..];
                                    while !data.is_empty() {
                                        match (*c).sock.write(data) {
                                            Ok(0) => {
                                                broken = true;
                                                break;
                                            }
                                            Ok(n) => {
                                                written += n;
                                                data = &data[n..];
                                            }
                                            Err(ref e)
                                                if e.kind() == std::io::ErrorKind::WouldBlock =>
                                            {
                                                break
                                            }
                                            Err(_) => {
                                                broken = true;
                                                break;
                                            }
                                        }
                                    }
                                }
                                if broken {
                                    (*c).dead = true;
                                    break 'pump lap;
                                }
                                if written == chunk.len() {
                                    (*c).pending.clear();
                                    (*c).pending_off = 0;
                                } else {
                                    (*c).pending_off = off + written;
                                    break 'pump lap;
                                }
                            }

                            // 2. read + feed the session
                            let mut buf = [0u8; 16 * 1024];
                            loop {
                                let r = (*c).sock.read(&mut buf);
                                match r {
                                    Ok(0) => {
                                        (*c).dead = true;
                                        break 'pump true;
                                    }
                                    Ok(n) => {
                                        lap = true;
                                        (*c).last_recv = now;
                                        let rc =
                                            ng::nghttp2_session_mem_recv(session, buf.as_ptr(), n);
                                        if rc < 0 {
                                            (*c).dead = true;
                                            break 'pump true;
                                        }
                                    }
                                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                                        break
                                    }
                                    Err(_) => {
                                        (*c).dead = true;
                                        break 'pump true;
                                    }
                                }
                            }

                            // 3. dispatch requests that completed
                            let ready: Vec<i32> = (*c)
                                .streams
                                .iter()
                                .filter(|(_, s)| s.req_done && !s.dispatched)
                                .map(|(id, _)| *id)
                                .collect();
                            for id in ready {
                                lap = true;
                                {
                                    // submit_response_headers
                                    let nva = [
                                        nv(b":status", b"200"),
                                        nv(b"content-type", b"application/grpc"),
                                    ];
                                    let provider = ng::nghttp2_data_provider {
                                        source: ng::nghttp2_data_source {
                                            ptr: std::ptr::null_mut(),
                                        },
                                        read_callback: Some(cb_data_read),
                                    };
                                    ng::nghttp2_submit_response(
                                        session,
                                        id,
                                        nva.as_ptr(),
                                        nva.len(),
                                        &provider,
                                    );
                                }
                                if let Some(s) = (*c).streams.get_mut(&id) {
                                    s.dispatched = true;

                                    // dispatch: decode the finished request and
                                    // start its RPC. Requests are parsed inline
                                    // (cheap, pure); all backend I/O is spawned
                                    // onto the runtime as Work so this thread
                                    // never blocks on it.
                                    {
                                        let ctx = &ctx;
                                    let body = std::mem::take(&mut s.req);

                                    // parse_method
                                    let method = match s.path.strip_prefix(PATH_PREFIX) {
                                        None => Method::Unknown,
                                        Some(name) => match name {
                                            b"GetLatestBlock" => Method::GetLatestBlock,
                                            b"GetBlock" => Method::GetBlock,
                                            b"GetBlockNullifiers" => Method::GetBlockNullifiers,
                                            b"GetBlockRange" => Method::GetBlockRange,
                                            b"GetBlockRangeNullifiers" => Method::GetBlockRangeNullifiers,
                                            b"GetTransaction" => Method::GetTransaction,
                                            b"GetRoster" => Method::GetRoster,
                                            b"SendTransaction" => Method::SendTransaction,
                                            // GetTaddressTxids is the deprecated alias with identical semantics
                                            b"GetTaddressTransactions" | b"GetTaddressTxids" => Method::GetTaddressTransactions,
                                            b"GetTaddressBalance" => Method::GetTaddressBalance,
                                            b"GetTaddressBalanceStream" => Method::GetTaddressBalanceStream,
                                            b"GetMempoolTx" => Method::GetMempoolTx,
                                            b"GetMempoolStream" => Method::GetMempoolStream,
                                            b"GetTreeState" => Method::GetTreeState,
                                            b"GetLatestTreeState" => Method::GetLatestTreeState,
                                            b"GetSubtreeRoots" => Method::GetSubtreeRoots,
                                            b"GetAddressUtxos" => Method::GetAddressUtxos,
                                            b"GetAddressUtxosStream" => Method::GetAddressUtxosStream,
                                            b"GetBondInfo" => Method::GetBondInfo,
                                            b"RequestFaucetDonation" => Method::RequestFaucetDonation,
                                            b"GetLightdInfo" => Method::GetLightdInfo,
                                            b"Ping" => Method::Ping,
                                            _ => Method::Unknown,
                                        },
                                    };

                                    // Sync-only arms push their response and finish; the rest
                                    // park Work (usually a spawned task) on the stream. The
                                    // immediately-called closure exists only so the arms can
                                    // use `?` — its Err lands in s.finish below.
                                    let authorized = (*c).loopback
                                        || token.as_deref().is_some_and(|t| bearer_matches(&s.authorization, t));
                                    let r: Result<(), Grr> = (|| {
                                        // Other hosts need the token, unless the server is public;
                                        // even then the faucet, which spends this node's funds,
                                        // needs it.
                                        if !authorized && !public {
                                            return Err((
                                                GRPC_UNAUTHENTICATED,
                                                "this lightwallet_server needs `authorization: Bearer <token>` from other hosts".into(),
                                            ));
                                        }
                                        if method == Method::RequestFaucetDonation && !authorized {
                                            return Err((
                                                GRPC_PERMISSION_DENIED,
                                                "RequestFaucetDonation answers only this host, or other hosts with the token".into(),
                                            ));
                                        }
                                        match method {
                                            Method::GetLatestBlock => {
                                                // get_latest_block
                                                let (height, hash) = ctx
                                                    .tip
                                                    .best_tip_height_and_hash()
                                                    .ok_or((GRPC_UNAVAILABLE, "no chain tip".to_string()))?;
                                                s.push(enc(&BlockId {
                                                    height: height.0 as u64,
                                                    hash: hash.0.to_vec(),
                                                }));
                                                s.finish(GRPC_OK, "");
                                            }

                                            Method::GetBlock | Method::GetBlockNullifiers => {
                                                // get_block
                                                let nulls = method == Method::GetBlockNullifiers;
                                                let req: BlockId = dec(one_msg(&body)?)?;
                                                let hoh = hash_or_height(&req)?;
                                                let c = ctx.clone();
                                                s.work = Work::Pending(ctx.rt.spawn(async move {
                                                    match c
                                                        .read_state
                                                        .clone()
                                                        .oneshot(ReadRequest::Block(hoh))
                                                        .await
                                                        .map_err(internal)?
                                                    {
                                                        ReadResponse::Block(Some(b)) => Ok(Work::Items(
                                                            [enc(&compact_block(&b, nulls))].into(),
                                                        )),
                                                        ReadResponse::Block(None) => Err((
                                                            GRPC_OUT_OF_RANGE,
                                                            "block not in best chain".into(),
                                                        )),
                                                        _ => Err((GRPC_INTERNAL, "unexpected state response".into())),
                                                    }
                                                }));
                                            }

                                            Method::GetTransaction => {
                                                // get_transaction
                                                let req: TxFilter = dec(one_msg(&body)?)?;
                                                if req.hash.len() != 32 {
                                                    return Err((GRPC_INVALID, "txid must be 32 bytes".into()));
                                                }
                                                let arr: [u8; 32] = req.hash[..].try_into().unwrap();
                                                let txid = transaction::Hash(arr);
                                                let c = ctx.clone();
                                                s.work = Work::Pending(ctx.rt.spawn(async move {
                                                    // mempool first, like zebra's own getrawtransaction
                                                    if let mempool::Response::Transactions(txs) = {
                                                            let mut svc = c.mempool.clone();
                                                            svc.ready().await.map_err(internal)?
                                                                .call(mempool::Request::TransactionsByMinedId(
                                                            [txid].into(),
                                                        ))
                                                                .await
                                                                .map_err(internal)?
                                                        }
                                                    {
                                                        if let Some(tx) = txs.first() {
                                                            let data = tx
                                                                .transaction
                                                                .zcash_serialize_to_vec()
                                                                .map_err(internal)?;
                                                            let height = c
                                                                .tip
                                                                .best_tip_height()
                                                                .map(|h| h.0 as u64)
                                                                .unwrap_or(0);
                                                            return Ok(Work::Items(
                                                                [enc(&RawTransaction { data, height })].into(),
                                                            ));
                                                        }
                                                    }
                                                    match c
                                                        .read_state
                                                        .clone()
                                                        .oneshot(ReadRequest::Transaction(txid))
                                                        .await
                                                        .map_err(internal)?
                                                    {
                                                        ReadResponse::Transaction(Some(mined)) => {
                                                            let data = mined
                                                                .tx
                                                                .zcash_serialize_to_vec()
                                                                .map_err(internal)?;
                                                            Ok(Work::Items(
                                                                [enc(&RawTransaction {
                                                                    data,
                                                                    height: mined.height.0 as u64,
                                                                })]
                                                                .into(),
                                                            ))
                                                        }
                                                        ReadResponse::Transaction(None) => {
                                                            Err((GRPC_NOT_FOUND, "transaction not found".into()))
                                                        }
                                                        _ => Err((GRPC_INTERNAL, "unexpected state response".into())),
                                                    }
                                                }));
                                            }

                                            Method::GetRoster => {
                                                // get_roster
                                                let c = ctx.clone();
                                                s.work = Work::Pending(ctx.rt.spawn(async move {
                                                    match c
                                                        .read_state
                                                        .clone()
                                                        .oneshot(ReadRequest::CrosslinkRosterWithAddresses)
                                                        .await
                                                        .map_err(internal)?
                                                    {
                                                        ReadResponse::CrosslinkRosterWithAddresses(roster) => {
                                                            let mut data = Vec::new();
                                                            for (member, address) in &roster {
                                                                let mut member = member.clone();
                                                                member.finalizer_address = *address;
                                                                member.write_to_vec(&mut data);
                                                            }
                                                            Ok(Work::Items([enc(&Bytes { data })].into()))
                                                        }
                                                        _ => Err((GRPC_INTERNAL, "unexpected state response".into())),
                                                    }
                                                }));
                                            }

                                            Method::SendTransaction => {
                                                // send_transaction: a transaction is up to REQ_MAX bytes,
                                                // so it is parsed on the blocking pool, not this thread,
                                                // where it would hold up every other client.
                                                let c = ctx.clone();
                                                s.work = Work::Pending(ctx.rt.spawn(async move {
                                                    let tx = tokio::task::spawn_blocking(move || {
                                                        let req: RawTransaction = dec(one_msg(&body)?)?;
                                                        Transaction::zcash_deserialize(&req.data[..])
                                                            .map_err(|e| (GRPC_INVALID, format!("bad transaction bytes: {e}")))
                                                    })
                                                    .await
                                                    .map_err(internal)??;
                                                    let txid = tx.hash();
                                                    let queued = {
                                                            let mut svc = c.mempool.clone();
                                                            svc.ready().await.map_err(internal)?
                                                                .call(mempool::Request::Queue(vec![
                                                            mempool::Gossip::Tx(tx.into()),
                                                        ]))
                                                                .await
                                                                .map_err(internal)?
                                                        };
                                                    let mempool::Response::Queued(mut results) = queued else {
                                                        return Err((GRPC_INTERNAL, "unexpected mempool response".into()));
                                                    };
                                                    let receiver = results
                                                        .pop()
                                                        .ok_or((GRPC_INTERNAL, "empty mempool queue result".to_string()))?
                                                        .map_err(|e| (GRPC_UNKNOWN, e.to_string()))?;
                                                    receiver
                                                        .await
                                                        .map_err(internal)?
                                                        .into_result()
                                                        .map_err(|e| (GRPC_UNKNOWN, e.to_string()))?;

                                                    // lightwalletd quirk: success carries the txid in error_message
                                                    Ok(Work::Items(
                                                        [enc(&SendResponse {
                                                            error_code: 0,
                                                            error_message: txid.to_string(),
                                                        })]
                                                        .into(),
                                                    ))
                                                }));
                                            }

                                            Method::GetTaddressBalance => {
                                                // get_taddress_balance / balance_of
                                                let req: AddressList = dec(one_msg(&body)?)?;
                                                too_many_addresses(req.addresses.len())?;
                                                let mut set = HashSet::new();
                                                for a in &req.addresses {
                                                    set.insert(
                                                        // parse_taddr
                                                        a.parse::<transparent::Address>().map_err(|e| {
                                                            (GRPC_INVALID, format!("bad transparent address {a:?}: {e}"))
                                                        })?,
                                                    );
                                                }
                                                if set.is_empty() {
                                                    return Err((GRPC_INVALID, "no addresses given".into()));
                                                }
                                                let c = ctx.clone();
                                                s.work = Work::Pending(ctx.rt.spawn(async move {
                                                    match c
                                                        .read_state
                                                        .clone()
                                                        .oneshot(ReadRequest::AddressBalance(set))
                                                        .await
                                                        .map_err(internal)?
                                                    {
                                                        ReadResponse::AddressBalance { balance, .. } => Ok(Work::Items(
                                                            [enc(&Balance {
                                                                value_zat: u64::from(balance) as i64,
                                                            })]
                                                            .into(),
                                                        )),
                                                        _ => Err((GRPC_INTERNAL, "unexpected state response".into())),
                                                    }
                                                }));
                                            }

                                            Method::GetTaddressBalanceStream => {
                                                // get_taddress_balance_stream / balance_of
                                                // split_msgs: the request side is N Address messages
                                                let mut set = HashSet::new();
                                                let mut p = 0usize;
                                                let mut count = 0usize;
                                                while p < body.len() {
                                                    count += 1;
                                                    too_many_addresses(count)?;
                                                    if body.len() - p < 5 {
                                                        return Err((GRPC_INVALID, "truncated grpc frame".into()));
                                                    }
                                                    if body[p] != 0 {
                                                        return Err((GRPC_UNIMPLEMENTED, "compressed messages not supported".into()));
                                                    }
                                                    let len =
                                                        u32::from_be_bytes(body[p + 1..p + 5].try_into().unwrap()) as usize;
                                                    if len > REQ_MAX || body.len() - p - 5 < len {
                                                        return Err((GRPC_RESOURCE, "message too large or truncated".into()));
                                                    }
                                                    let a: Address = dec(&body[p + 5..p + 5 + len])?;
                                                    let a = &a.address;
                                                    set.insert(
                                                        // parse_taddr
                                                        a.parse::<transparent::Address>().map_err(|e| {
                                                            (GRPC_INVALID, format!("bad transparent address {a:?}: {e}"))
                                                        })?,
                                                    );
                                                    p += 5 + len;
                                                }
                                                if set.is_empty() {
                                                    return Err((GRPC_INVALID, "no addresses given".into()));
                                                }
                                                let c = ctx.clone();
                                                s.work = Work::Pending(ctx.rt.spawn(async move {
                                                    match c
                                                        .read_state
                                                        .clone()
                                                        .oneshot(ReadRequest::AddressBalance(set))
                                                        .await
                                                        .map_err(internal)?
                                                    {
                                                        ReadResponse::AddressBalance { balance, .. } => Ok(Work::Items(
                                                            [enc(&Balance {
                                                                value_zat: u64::from(balance) as i64,
                                                            })]
                                                            .into(),
                                                        )),
                                                        _ => Err((GRPC_INTERNAL, "unexpected state response".into())),
                                                    }
                                                }));
                                            }

                                            Method::GetTreeState | Method::GetLatestTreeState => {
                                                // get_tree_state / get_latest_tree_state
                                                let hoh = if method == Method::GetTreeState {
                                                    let req: BlockId = dec(one_msg(&body)?)?;
                                                    hash_or_height(&req)?
                                                } else {
                                                    let (_, tip_hash) = ctx
                                                        .tip
                                                        .best_tip_height_and_hash()
                                                        .ok_or((GRPC_UNAVAILABLE, "no chain tip".to_string()))?;
                                                    tip_hash.into()
                                                };
                                                let c = ctx.clone();
                                                s.work = Work::Pending(ctx.rt.spawn(async move {
                                                    // tree_state_at
                                                    let (header, hash, height) = match c
                                                        .read_state
                                                        .clone()
                                                        .oneshot(ReadRequest::BlockHeader(hoh))
                                                        .await
                                                    {
                                                        Ok(ReadResponse::BlockHeader { header, hash, height, .. }) => {
                                                            (header, hash, height)
                                                        }
                                                        Ok(_) => return Err((GRPC_INTERNAL, "unexpected state response".into())),
                                                        Err(_) => return Err((GRPC_NOT_FOUND, "block not found".into())),
                                                    };
                                                    let active = |nu: NetworkUpgrade| -> bool {
                                                        nu.activation_height(&c.network).is_some_and(|a| height >= a)
                                                    };
                                                    let sapling_tree = if active(NetworkUpgrade::Sapling) {
                                                        match c
                                                            .read_state
                                                            .clone()
                                                            .oneshot(ReadRequest::SaplingTree(hash.into()))
                                                            .await
                                                            .map_err(internal)?
                                                        {
                                                            ReadResponse::SaplingTree(Some(t)) => {
                                                                hex::encode(t.to_rpc_bytes())
                                                            }
                                                            _ => String::new(),
                                                        }
                                                    } else {
                                                        String::new()
                                                    };
                                                    let orchard_tree = if active(NetworkUpgrade::Nu5) {
                                                        match c
                                                            .read_state
                                                            .clone()
                                                            .oneshot(ReadRequest::OrchardTree(hash.into()))
                                                            .await
                                                            .map_err(internal)?
                                                        {
                                                            ReadResponse::OrchardTree(Some(t)) => {
                                                                hex::encode(t.to_rpc_bytes())
                                                            }
                                                            _ => String::new(),
                                                        }
                                                    } else {
                                                        String::new()
                                                    };
                                                    Ok(Work::Items(
                                                        [enc(&TreeState {
                                                            // Crosslink does not build Ironwood
                                                            // bundles yet, so its tree is empty.
                                                            ironwood_tree: String::new(),
                                                            network: c.network.bip70_network_name(),
                                                            height: height.0 as u64,
                                                            // display-order hex, what the wallet parses
                                                            hash: hash.to_string(),
                                                            time: header.time.timestamp() as u32,
                                                            sapling_tree,
                                                            orchard_tree,
                                                        })]
                                                        .into(),
                                                    ))
                                                }));
                                            }

                                            Method::GetAddressUtxos | Method::GetAddressUtxosStream => {
                                                // get_address_utxos / begin_address_utxos_stream
                                                let unary = method == Method::GetAddressUtxos;
                                                let req: GetAddressUtxosArg = dec(one_msg(&body)?)?;
                                                too_many_addresses(req.addresses.len())?;
                                                // A caller asking for no more than the cap gets the first
                                                // `max_entries`, as lightwalletd answers. One asking for more,
                                                // or for all (0), gets an error rather than an answer the
                                                // cap cut short, which would read as a smaller balance.
                                                let asked = req.max_entries as usize;
                                                let caller_limits = asked != 0 && asked <= MAX_UTXO_ENTRIES;
                                                let limit = if caller_limits { asked } else { MAX_UTXO_ENTRIES };

                                                let mut set = HashSet::new();
                                                for a in &req.addresses {
                                                    set.insert(
                                                        // parse_taddr
                                                        a.parse::<transparent::Address>().map_err(|e| {
                                                            (GRPC_INVALID, format!("bad transparent address {a:?}: {e}"))
                                                        })?,
                                                    );
                                                }
                                                if set.is_empty() {
                                                    return Err((GRPC_INVALID, "no addresses given".into()));
                                                }
                                                let c = ctx.clone();
                                                s.work = Work::Pending(ctx.rt.spawn(async move {
                                                    let utxos = match c
                                                        .read_state
                                                        .clone()
                                                        .oneshot(ReadRequest::UtxosByAddresses(set))
                                                        .await
                                                        .map_err(internal)?
                                                    {
                                                        ReadResponse::AddressUtxos(utxos) => utxos,
                                                        _ => return Err((GRPC_INTERNAL, "unexpected state response".into())),
                                                    };
                                                    // collect_utxos
                                                    let mut replies = Vec::new();
                                                    for (address, txid, location, output) in utxos.utxos() {
                                                        let height = location.height().0 as u64;
                                                        if height < req.start_height {
                                                            continue;
                                                        }
                                                        if replies.len() == limit {
                                                            if caller_limits {
                                                                break;
                                                            }
                                                            return Err((
                                                                GRPC_RESOURCE,
                                                                format!(
                                                                    "these addresses have more than {MAX_UTXO_ENTRIES} UTXOs from \
                                                                     height {}: ask for at most {MAX_UTXO_ENTRIES} with max_entries, \
                                                                     or fewer addresses",
                                                                    req.start_height,
                                                                ),
                                                            ));
                                                        }
                                                        replies.push(GetAddressUtxosReply {
                                                            address: address.to_string(),
                                                            txid: txid.0.to_vec(),
                                                            index: location.output_index().index() as i32,
                                                            script: output.lock_script.as_raw_bytes().to_vec(),
                                                            value_zat: u64::from(output.value) as i64,
                                                            height,
                                                        });
                                                    }
                                                    if unary {
                                                        Ok(Work::Items(
                                                            [enc(&GetAddressUtxosReplyList {
                                                                address_utxos: replies,
                                                            })]
                                                            .into(),
                                                        ))
                                                    } else {
                                                        Ok(Work::Items(replies.iter().map(enc).collect()))
                                                    }
                                                }));
                                            }

                                            Method::GetBondInfo => {
                                                // get_bond_info
                                                let req: BondInfoRequest = dec(one_msg(&body)?)?;
                                                let key: [u8; 32] = req.bond_key[..]
                                                    .try_into()
                                                    .map_err(|_| (GRPC_INVALID, "bond key must be 32 bytes".to_string()))?;
                                                let c = ctx.clone();
                                                s.work = Work::Pending(ctx.rt.spawn(async move {
                                                    match c
                                                        .read_state
                                                        .clone()
                                                        .oneshot(ReadRequest::BondInfo(key))
                                                        .await
                                                        .map_err(internal)?
                                                    {
                                                        ReadResponse::BondInfo(Some(info)) => Ok(Work::Items(
                                                            [enc(&BondInfoResponse {
                                                                amount: u64::from(info.amount),
                                                                status: info.status as u32,
                                                                last_action_height: info.last_action_height.0,
                                                                version: info.version.to_vec(),
                                                            })]
                                                            .into(),
                                                        )),
                                                        ReadResponse::BondInfo(None) => {
                                                            Err((GRPC_NOT_FOUND, "Bond not found".into()))
                                                        }
                                                        _ => Err((GRPC_INTERNAL, "unexpected state response".into())),
                                                    }
                                                }));
                                            }

                                            Method::RequestFaucetDonation => {
                                                // request_faucet_donation
                                                let req: FaucetRequest = dec(one_msg(&body)?)?;
                                                let c = ctx.clone();
                                                s.work = Work::Pending(ctx.rt.spawn(async move {
                                                    match c
                                                        .tfl
                                                        .clone()
                                                        .oneshot(TFLServiceRequest::Faucet(req.address))
                                                        .await
                                                        .map_err(internal)?
                                                    {
                                                        TFLServiceResponse::Faucet(Ok(amount)) => {
                                                            Ok(Work::Items([enc(&FaucetResponse { amount })].into()))
                                                        }
                                                        TFLServiceResponse::Faucet(Err(msg)) => {
                                                            Err((GRPC_INTERNAL, msg))
                                                        }
                                                        _ => Err((GRPC_INTERNAL, "unexpected TFL response".into())),
                                                    }
                                                }));
                                            }

                                            Method::GetLightdInfo => {
                                                // get_lightd_info
                                                let tip_height = ctx.tip.best_tip_height().map(|h| h.0).unwrap_or(0);
                                                let branch_id = zebra_chain::parameters::ConsensusBranchId::current(
                                                    &ctx.network,
                                                    Height(tip_height),
                                                )
                                                .map(|id| id.to_string())
                                                .unwrap_or_default();
                                                s.push(enc(&LightdInfo {
                                                    version: env!("CARGO_PKG_VERSION").to_string(),
                                                    vendor: "Crosslink zebrad".to_string(),
                                                    taddr_support: true,
                                                    chain_name: ctx.network.bip70_network_name(),
                                                    sapling_activation_height: ctx.network.sapling_activation_height().0 as u64,
                                                    consensus_branch_id: branch_id,
                                                    block_height: tip_height as u64,
                                                    estimated_height: tip_height as u64,
                                                    ..Default::default()
                                                }));
                                                s.finish(GRPC_OK, "");
                                            }

                                            Method::Ping => {
                                                // ping
                                                let req: Duration = dec(one_msg(&body)?)?;
                                                s.push(enc(&PingResponse {
                                                    entry: req.interval_us,
                                                    exit: req.interval_us,
                                                }));
                                                s.finish(GRPC_OK, "");
                                            }

                                            Method::GetBlockRange | Method::GetBlockRangeNullifiers => {
                                                // begin_block_range
                                                let nulls = method == Method::GetBlockRangeNullifiers;
                                                let req: BlockRange = dec(one_msg(&body)?)?;
                                                let start: u32 = req
                                                    .start
                                                    .ok_or((GRPC_INVALID, "missing start".to_string()))?
                                                    .height
                                                    .try_into()
                                                    .map_err(|_| (GRPC_INVALID, "start height out of range".to_string()))?;
                                                let end: u32 = req
                                                    .end
                                                    .ok_or((GRPC_INVALID, "missing end".to_string()))?
                                                    .height
                                                    .try_into()
                                                    .map_err(|_| (GRPC_INVALID, "end height out of range".to_string()))?;
                                                let span = u64::from(start.abs_diff(end)) + 1;
                                                if span > u64::from(MAX_BLOCK_RANGE) {
                                                    return Err((
                                                        GRPC_INVALID,
                                                        format!("the range spans {span} blocks; one request may span at most {MAX_BLOCK_RANGE}"),
                                                    ));
                                                }
                                                let step = if end >= start { 1 } else { -1 };
                                                s.work = Work::Blocks { next: start, end, step, nulls, inflight: None };
                                            }

                                            Method::GetTaddressTransactions => {
                                                // begin_taddress_transactions
                                                let req: TransparentAddressBlockFilter = dec(one_msg(&body)?)?;
                                                // parse_taddr
                                                let addr = req.address.parse::<transparent::Address>().map_err(|e| {
                                                    (GRPC_INVALID, format!("bad transparent address {:?}: {e}", req.address))
                                                })?;
                                                let range = req.range.ok_or((GRPC_INVALID, "missing range".to_string()))?;
                                                let start: u32 = range
                                                    .start
                                                    .map(|b| b.height)
                                                    .unwrap_or(0)
                                                    .try_into()
                                                    .map_err(|_| (GRPC_INVALID, "start height out of range".to_string()))?;
                                                let mut end: u32 = range
                                                    .end
                                                    .map(|b| b.height)
                                                    .unwrap_or(0)
                                                    .try_into()
                                                    .map_err(|_| (GRPC_INVALID, "end height out of range".to_string()))?;

                                                let tip = ctx.tip.best_tip_height().map(|h| h.0).unwrap_or(0);
                                                if end == 0 || end > tip {
                                                    end = tip;
                                                }
                                                if start > end {
                                                    s.work = Work::Items(VecDeque::new());
                                                    return Ok(());
                                                }

                                                let c = ctx.clone();
                                                s.work = Work::Pending(ctx.rt.spawn(async move {
                                                    match c
                                                        .read_state
                                                        .clone()
                                                        .oneshot(ReadRequest::TransactionIdsByAddresses {
                                                            addresses: [addr].into(),
                                                            height_range: Height(start)..=Height(end),
                                                        })
                                                        .await
                                                        .map_err(internal)?
                                                    {
                                                        ReadResponse::AddressesTransactionIds(map) => Ok(Work::Txs {
                                                            hashes: map.into_values().collect(),
                                                            i: 0,
                                                            inflight: None,
                                                        }),
                                                        _ => Err((GRPC_INTERNAL, "unexpected state response".into())),
                                                    }
                                                }));
                                            }

                                            Method::GetMempoolTx => {
                                                // begin_mempool_tx
                                                let req: Exclude = dec(one_msg(&body)?)?;
                                                // lightwalletd semantics: exclude txids arrive in wire (internal) order
                                                // and are prefix-matched against the display-order txid
                                                let excludes: Vec<Vec<u8>> = req
                                                    .txid
                                                    .iter()
                                                    .map(|e| e.iter().rev().copied().collect())
                                                    .collect();

                                                let c = ctx.clone();
                                                s.work = Work::Pending(ctx.rt.spawn(async move {
                                                    let full = {
                                                        let mut svc = c.mempool.clone();
                                                        svc.ready().await.map_err(internal)?
                                                            .call(mempool::Request::FullTransactions)
                                                            .await
                                                            .map_err(internal)?
                                                    };
                                                    let mempool::Response::FullTransactions { transactions, .. } = full
                                                    else {
                                                        return Err((GRPC_INTERNAL, "unexpected mempool response".into()));
                                                    };

                                                    let mut items = VecDeque::new();
                                                    for vtx in &transactions {
                                                        let display =
                                                            zebra_chain::serialization::BytesInDisplayOrder::bytes_in_display_order(&vtx.transaction.id.mined_id());
                                                        if excludes
                                                            .iter()
                                                            .any(|e| !e.is_empty() && display.starts_with(e))
                                                        {
                                                            continue;
                                                        }

                                                        // compact_tx (full form, index 0 for mempool)
                                                        let tx = &vtx.transaction.transaction;
                                                        let spends: Vec<CompactSaplingSpend> = tx
                                                            .sapling_nullifiers()
                                                            .map(|nf| CompactSaplingSpend { nf: (*nf.0).to_vec() })
                                                            .collect();
                                                        let outputs: Vec<CompactSaplingOutput> = tx
                                                            .sapling_outputs()
                                                            .map(|o| CompactSaplingOutput {
                                                                cmu: o.cm_u.to_bytes().to_vec(),
                                                                ephemeral_key: <[u8; 32]>::from(o.ephemeral_key).to_vec(),
                                                                ciphertext: <[u8; 580]>::from(o.enc_ciphertext)[..52].to_vec(),
                                                            })
                                                            .collect();
                                                        let actions: Vec<CompactOrchardAction> = tx
                                                            .orchard_actions()
                                                            .map(|a| CompactOrchardAction {
                                                                nullifier: <[u8; 32]>::from(a.nullifier).to_vec(),
                                                                cmx: <[u8; 32]>::from(a.cm_x).to_vec(),
                                                                ephemeral_key: <[u8; 32]>::from(a.ephemeral_key).to_vec(),
                                                                ciphertext: <[u8; 580]>::from(a.enc_ciphertext)[..52].to_vec(),
                                                            })
                                                            .collect();
                                                        // Ironwood actions are a separate bundle with its own note commitment
                                                        // tree. From NU6.3 they carry every cross-address transfer (the Orchard
                                                        // pool is change-only), so a wallet that never sees them cannot find
                                                        // its funds at all.
                                                        let ironwood_actions: Vec<CompactOrchardAction> = tx
                                                            .ironwood_actions()
                                                            .map(|a| CompactOrchardAction {
                                                                nullifier: <[u8; 32]>::from(a.nullifier).to_vec(),
                                                                cmx: <[u8; 32]>::from(a.cm_x).to_vec(),
                                                                ephemeral_key: <[u8; 32]>::from(a.ephemeral_key).to_vec(),
                                                                ciphertext: <[u8; 580]>::from(a.enc_ciphertext)[..52].to_vec(),
                                                            })
                                                            .collect();
                                                        if spends.is_empty() && outputs.is_empty() && actions.is_empty()
                                                            && ironwood_actions.is_empty() {
                                                            continue;
                                                        }
                                                        items.push_back(enc(&CompactTx {
                                                            index: 0,
                                                            txid: tx.hash().0.to_vec(),
                                                            fee: 0,
                                                            spends,
                                                            vin: Vec::new(),
                                                            vout: Vec::new(),
                                                            ironwood_actions,
                                                            outputs,
                                                            actions,
                                                        }));
                                                    }
                                                    Ok(Work::Items(items))
                                                }));
                                            }

                                            Method::GetMempoolStream => {
                                                // begin_mempool_stream
                                                let Some((tip_height, tip0)) = ctx.tip.best_tip_height_and_hash() else {
                                                    s.work = Work::Items(VecDeque::new());
                                                    return Ok(());
                                                };
                                                let rx = ctx.mempool_events.subscribe();
                                                let c = ctx.clone();
                                                // current mempool contents first, then live additions
                                                // until the tip moves
                                                s.work = Work::Pending(ctx.rt.spawn(async move {
                                                    let mut seen = HashSet::new();
                                                    let mut backlog = VecDeque::new();
                                                    if let mempool::Response::FullTransactions { transactions, .. } = {
                                                            let mut svc = c.mempool.clone();
                                                            svc.ready().await.map_err(internal)?
                                                                .call(mempool::Request::FullTransactions)
                                                                .await
                                                                .map_err(internal)?
                                                        }
                                                    {
                                                        for vtx in &transactions {
                                                            seen.insert(vtx.transaction.id);
                                                            if let Ok(data) =
                                                                vtx.transaction.transaction.zcash_serialize_to_vec()
                                                            {
                                                                backlog.push_back(enc(&RawTransaction {
                                                                    data,
                                                                    height: tip_height.0 as u64,
                                                                }));
                                                            }
                                                        }
                                                    }
                                                    Ok(Work::Mempool { backlog, rx, seen, tip0, inflight: None })
                                                }));
                                            }

                                            Method::GetSubtreeRoots => {
                                                // begin_subtree_roots
                                                let req: GetSubtreeRootsArg = dec(one_msg(&body)?)?;
                                                let start: u16 = req
                                                    .start_index
                                                    .try_into()
                                                    .map_err(|_| (GRPC_INVALID, "start_index too large".to_string()))?;
                                                let limit = if req.max_entries > 0 {
                                                    let n: u16 = req
                                                        .max_entries
                                                        .try_into()
                                                        .map_err(|_| (GRPC_INVALID, "max_entries too large".to_string()))?;
                                                    Some(NoteCommitmentSubtreeIndex(n))
                                                } else {
                                                    None
                                                };
                                                let protocol = req.shielded_protocol;
                                                let c = ctx.clone();
                                                s.work = Work::Pending(ctx.rt.spawn(async move {
                                                    // (root hex, end height) for either pool, then
                                                    // resolve completing block hashes
                                                    let subtrees: Vec<(String, Height)> = match protocol {
                                                        0 => match c
                                                            .read_state
                                                            .clone()
                                                            .oneshot(ReadRequest::SaplingSubtrees {
                                                                start_index: NoteCommitmentSubtreeIndex(start),
                                                                limit,
                                                            })
                                                            .await
                                                            .map_err(internal)?
                                                        {
                                                            ReadResponse::SaplingSubtrees(map) => map
                                                                .values()
                                                                .map(|d| (hex::encode(d.root.to_bytes()), d.end_height))
                                                                .collect(),
                                                            _ => return Err((GRPC_INTERNAL, "unexpected state response".into())),
                                                        },
                                                        1 => match c
                                                            .read_state
                                                            .clone()
                                                            .oneshot(ReadRequest::OrchardSubtrees {
                                                                start_index: NoteCommitmentSubtreeIndex(start),
                                                                limit,
                                                            })
                                                            .await
                                                            .map_err(internal)?
                                                        {
                                                            ReadResponse::OrchardSubtrees(map) => map
                                                                .values()
                                                                .map(|d| (d.root.encode_hex::<String>(), d.end_height))
                                                                .collect(),
                                                            _ => return Err((GRPC_INTERNAL, "unexpected state response".into())),
                                                        },
                                                        _ => return Err((GRPC_INVALID, "unknown shielded protocol".into())),
                                                    };

                                                    let mut items = VecDeque::new();
                                                    for (root_hex, end_height) in subtrees {
                                                        let root_hash = hex::decode(&root_hex).map_err(internal)?;
                                                        let completing_block_hash = match c
                                                            .read_state
                                                            .clone()
                                                            .oneshot(ReadRequest::BestChainBlockHash(end_height))
                                                            .await
                                                            .map_err(internal)?
                                                        {
                                                            ReadResponse::BlockHash(Some(h)) => h.0.to_vec(),
                                                            _ => Vec::new(),
                                                        };
                                                        items.push_back(enc(&SubtreeRoot {
                                                            root_hash,
                                                            completing_block_hash,
                                                            completing_block_height: end_height.0 as u64,
                                                        }));
                                                    }
                                                    Ok(Work::Items(items))
                                                }));
                                            }

                                            Method::Unknown => {
                                                return Err((
                                                    GRPC_UNIMPLEMENTED,
                                                    format!("unknown method: {}", String::from_utf8_lossy(&s.path)),
                                                ));
                                            }
                                        }
                                        Ok(())
                                    })();

                                    if let Err((code, msg)) = r {
                                        s.finish(code, msg);
                                    }
                                    }
                                }
                            }

                            // 4. advance streaming work
                            let work_ids: Vec<i32> = (*c)
                                .streams
                                .iter()
                                .filter(|(_, s)| !matches!(s.work, Work::None))
                                .map(|(id, _)| *id)
                                .collect();
                            for id in work_ids {
                                if let Some(s) = (*c).streams.get_mut(&id) {
                                    // advance: poll/reap this stream's in-flight
                                    // runtime task and refill the out buffer under
                                    // the budget; true if it produced or finished
                                    lap |= {
                                        let ctx = &ctx;
                                    let mut progress = false;

                                    loop {
                                        if s.done || s.out.len() >= OUT_BUDGET {
                                            break;
                                        }
                                        let work = std::mem::replace(&mut s.work, Work::None);
                                        match work {
                                            Work::None => break,

                                            Work::Items(mut items) => {
                                                while s.out.len() < OUT_BUDGET {
                                                    match items.pop_front() {
                                                        Some(m) => {
                                                            s.out.extend(m);
                                                            progress = true;
                                                        }
                                                        None => break,
                                                    }
                                                }
                                                if items.is_empty() {
                                                    s.finish(GRPC_OK, "");
                                                    progress = true;
                                                } else {
                                                    s.work = Work::Items(items);
                                                }
                                                break;
                                            }

                                            Work::Pending(task) => {
                                                if !task.is_finished() {
                                                    s.work = Work::Pending(task);
                                                    break;
                                                }
                                                progress = true;
                                                match reap(&ctx.rt, task) {
                                                    // loop again to run the resolved work
                                                    Ok(next_work) => s.work = next_work,
                                                    Err((code, msg)) => s.finish(code, msg),
                                                }
                                            }

                                            Work::Blocks { next, end, step, nulls, inflight } => {
                                                if let Some((count, task)) = inflight {
                                                    if !task.is_finished() {
                                                        s.work = Work::Blocks {
                                                            next, end, step, nulls,
                                                            inflight: Some((count, task)),
                                                        };
                                                        break;
                                                    }
                                                    progress = true;
                                                    match reap(&ctx.rt, task) {
                                                        Ok((frames, hit_none)) => {
                                                            let got = frames.len() as i64;
                                                            for f in frames {
                                                                s.out.extend(f);
                                                            }
                                                            if hit_none {
                                                                let h = (next as i64 + got * step) as u32;
                                                                s.finish(
                                                                    GRPC_OUT_OF_RANGE,
                                                                    format!("height {h} is not in the best chain"),
                                                                );
                                                            } else {
                                                                let remaining = if step > 0 {
                                                                    (end - next) as i64 + 1
                                                                } else {
                                                                    (next - end) as i64 + 1
                                                                };
                                                                if count >= remaining {
                                                                    s.finish(GRPC_OK, "");
                                                                } else {
                                                                    s.work = Work::Blocks {
                                                                        next: (next as i64 + count * step) as u32,
                                                                        end, step, nulls,
                                                                        inflight: None,
                                                                    };
                                                                }
                                                            }
                                                        }
                                                        Err((code, msg)) => s.finish(code, msg),
                                                    }
                                                } else {
                                                    // spawn the next chunk of block reads
                                                    let remaining = if step > 0 {
                                                        (end - next) as i64 + 1
                                                    } else {
                                                        (next - end) as i64 + 1
                                                    };
                                                    let count = remaining.min(CHUNK);
                                                    let c = ctx.clone();
                                                    let task = ctx.rt.spawn(async move {
                                                        let mut frames = Vec::with_capacity(count as usize);
                                                        let mut h = next;
                                                        for _ in 0..count {
                                                            match c
                                                                .read_state
                                                                .clone()
                                                                .oneshot(ReadRequest::Block(Height(h).into()))
                                                                .await
                                                                .map_err(internal)?
                                                            {
                                                                ReadResponse::Block(Some(b)) => {
                                                                    frames.push(enc(&compact_block(&b, nulls)))
                                                                }
                                                                ReadResponse::Block(None) => return Ok((frames, true)),
                                                                _ => {
                                                                    return Err((
                                                                        GRPC_INTERNAL,
                                                                        "unexpected state response".into(),
                                                                    ))
                                                                }
                                                            }
                                                            h = (h as i64 + step) as u32;
                                                        }
                                                        Ok((frames, false))
                                                    });
                                                    s.work = Work::Blocks {
                                                        next, end, step, nulls,
                                                        inflight: Some((count, task)),
                                                    };
                                                    progress = true;
                                                    break;
                                                }
                                            }

                                            Work::Txs { hashes, i, inflight } => {
                                                if let Some((count, task)) = inflight {
                                                    if !task.is_finished() {
                                                        s.work = Work::Txs { hashes, i, inflight: Some((count, task)) };
                                                        break;
                                                    }
                                                    progress = true;
                                                    match reap(&ctx.rt, task) {
                                                        Ok(frames) => {
                                                            for f in frames {
                                                                s.out.extend(f);
                                                            }
                                                            let i = i + count;
                                                            if i >= hashes.len() {
                                                                s.finish(GRPC_OK, "");
                                                            } else {
                                                                s.work = Work::Txs { hashes, i, inflight: None };
                                                            }
                                                        }
                                                        Err((code, msg)) => s.finish(code, msg),
                                                    }
                                                } else if i >= hashes.len() {
                                                    s.finish(GRPC_OK, "");
                                                    progress = true;
                                                    break;
                                                } else {
                                                    // spawn the next chunk of tx reads
                                                    let count = (hashes.len() - i).min(CHUNK as usize);
                                                    let chunk: Vec<transaction::Hash> =
                                                        hashes[i..i + count].to_vec();
                                                    let c = ctx.clone();
                                                    let task = ctx.rt.spawn(async move {
                                                        let mut frames = Vec::new();
                                                        for hash in chunk {
                                                            match c
                                                                .read_state
                                                                .clone()
                                                                .oneshot(ReadRequest::Transaction(hash))
                                                                .await
                                                                .map_err(internal)?
                                                            {
                                                                ReadResponse::Transaction(Some(mined)) => {
                                                                    let data = mined
                                                                        .tx
                                                                        .zcash_serialize_to_vec()
                                                                        .map_err(internal)?;
                                                                    frames.push(enc(&RawTransaction {
                                                                        data,
                                                                        height: mined.height.0 as u64,
                                                                    }));
                                                                }
                                                                // txid was indexed but the tx vanished
                                                                // (reorg between calls): skip it
                                                                ReadResponse::Transaction(None) => {}
                                                                _ => {
                                                                    return Err((
                                                                        GRPC_INTERNAL,
                                                                        "unexpected state response".into(),
                                                                    ))
                                                                }
                                                            }
                                                        }
                                                        Ok(frames)
                                                    });
                                                    s.work = Work::Txs { hashes, i, inflight: Some((count, task)) };
                                                    progress = true;
                                                    break;
                                                }
                                            }

                                            Work::Mempool { mut backlog, mut rx, mut seen, tip0, inflight } => {
                                                while s.out.len() < OUT_BUDGET {
                                                    match backlog.pop_front() {
                                                        Some(m) => {
                                                            s.out.extend(m);
                                                            progress = true;
                                                        }
                                                        None => break,
                                                    }
                                                }
                                                if s.out.len() >= OUT_BUDGET {
                                                    s.work = Work::Mempool { backlog, rx, seen, tip0, inflight };
                                                    break;
                                                }
                                                if let Some(task) = inflight {
                                                    if !task.is_finished() {
                                                        s.work = Work::Mempool {
                                                            backlog, rx, seen, tip0,
                                                            inflight: Some(task),
                                                        };
                                                        break;
                                                    }
                                                    progress = true;
                                                    match reap(&ctx.rt, task) {
                                                        Ok(frames) => {
                                                            for f in frames {
                                                                backlog.push_back(f);
                                                            }
                                                            // drain the fresh frames next iteration
                                                            s.work = Work::Mempool {
                                                                backlog, rx, seen, tip0,
                                                                inflight: None,
                                                            };
                                                            continue;
                                                        }
                                                        Err((code, msg)) => {
                                                            s.finish(code, msg);
                                                            break;
                                                        }
                                                    }
                                                }
                                                if ctx.tip.best_tip_hash() != Some(tip0) {
                                                    s.finish(GRPC_OK, "");
                                                    progress = true;
                                                    break;
                                                }
                                                let tip_height =
                                                    ctx.tip.best_tip_height().map(|h| h.0 as u64).unwrap_or(0);
                                                let mut new_ids: HashSet<UnminedTxId> = HashSet::new();
                                                let mut closed = false;
                                                loop {
                                                    use tokio::sync::broadcast::error::TryRecvError;
                                                    match rx.try_recv() {
                                                        Ok(change) => {
                                                            if change.kind() == MempoolChangeKind::Added {
                                                                for id in change.tx_ids() {
                                                                    if !seen.contains(id) {
                                                                        seen.insert(*id);
                                                                        new_ids.insert(*id);
                                                                    }
                                                                }
                                                            }
                                                        }
                                                        Err(TryRecvError::Empty) => break,
                                                        Err(TryRecvError::Lagged(_)) => continue,
                                                        Err(TryRecvError::Closed) => {
                                                            closed = true;
                                                            break;
                                                        }
                                                    }
                                                }
                                                if !new_ids.is_empty() {
                                                    let c = ctx.clone();
                                                    let task = ctx.rt.spawn(async move {
                                                        let mut frames = Vec::new();
                                                        if let mempool::Response::Transactions(txs) = {
                                                                let mut svc = c.mempool.clone();
                                                                svc.ready().await.map_err(internal)?
                                                                    .call(mempool::Request::TransactionsById(new_ids))
                                                                    .await
                                                                    .map_err(internal)?
                                                            }
                                                        {
                                                            for tx in txs {
                                                                if let Ok(data) =
                                                                    tx.transaction.zcash_serialize_to_vec()
                                                                {
                                                                    frames.push(enc(&RawTransaction {
                                                                        data,
                                                                        height: tip_height,
                                                                    }));
                                                                }
                                                            }
                                                        }
                                                        Ok(frames)
                                                    });
                                                    s.work = Work::Mempool {
                                                        backlog, rx, seen, tip0,
                                                        inflight: Some(task),
                                                    };
                                                    progress = true;
                                                    break;
                                                }
                                                if closed {
                                                    s.finish(GRPC_OK, "");
                                                    progress = true;
                                                    break;
                                                }
                                                s.work = Work::Mempool { backlog, rx, seen, tip0, inflight: None };
                                                break;
                                            }
                                        }
                                    }
                                    progress
                                    };
                                }
                            }

                            // wake any parked data source that has bytes (or a
                            // final status) now
                            let wake: Vec<i32> = (*c)
                                .streams
                                .iter()
                                .filter(|(_, s)| s.deferred && (!s.out.is_empty() || s.done))
                                .map(|(id, _)| *id)
                                .collect();
                            for id in wake {
                                if let Some(s) = (*c).streams.get_mut(&id) {
                                    s.deferred = false;
                                }
                                ng::nghttp2_session_resume_data(session, id);
                                lap = true;
                            }

                            // 5. produce and write session output
                            if (*c).pending.is_empty() {
                                loop {
                                    let mut ptr: *const u8 = std::ptr::null();
                                    let n = ng::nghttp2_session_mem_send(session, &mut ptr);
                                    if n == 0 {
                                        break;
                                    }
                                    if n < 0 {
                                        (*c).dead = true;
                                        break 'pump true;
                                    }
                                    lap = true;
                                    let chunk = std::slice::from_raw_parts(ptr, n as usize);

                                    // write_some
                                    let mut written = 0usize;
                                    let mut broken = false;
                                    {
                                        let mut data = chunk;
                                        while !data.is_empty() {
                                            match (*c).sock.write(data) {
                                                Ok(0) => {
                                                    broken = true;
                                                    break;
                                                }
                                                Ok(n) => {
                                                    written += n;
                                                    data = &data[n..];
                                                }
                                                Err(ref e)
                                                    if e.kind()
                                                        == std::io::ErrorKind::WouldBlock =>
                                                {
                                                    break
                                                }
                                                Err(_) => {
                                                    broken = true;
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                    if broken {
                                        (*c).dead = true;
                                        break 'pump true;
                                    }
                                    if written < chunk.len() {
                                        (*c).pending = chunk[written..].to_vec();
                                        (*c).pending_off = 0;
                                        break;
                                    }
                                }
                            }

                            if ng::nghttp2_session_want_read(session) == 0
                                && ng::nghttp2_session_want_write(session) == 0
                            {
                                (*c).dead = true;
                            }
                            lap
                        }
                    };
                }
                conns.retain(|c| !c.dead);

                if !progress {
                    std::thread::sleep(std::time::Duration::from_millis(2));
                }
            }
        })
        .expect("can spawn lightwallet_server thread")
}

// -------------------------------------------------------------------------
// nghttp2 callbacks (all fire synchronously on the server thread; these stay
// as named functions because they are registered as C function pointers)

unsafe extern "C" fn cb_begin_headers(
    _session: *mut ng::nghttp2_session,
    frame: *const ng::nghttp2_frame,
    user_data: *mut std::os::raw::c_void,
) -> std::os::raw::c_int {
    let conn = &mut *(user_data as *mut Conn);
    let hd = (*frame).hd;
    if hd.type_ as u32 == ng::NGHTTP2_HEADERS && hd.stream_id > 0 {
        conn.last_used = std::time::Instant::now();
        conn.streams.entry(hd.stream_id).or_insert_with(|| {
            // Stream::new
            Stream {
                path: Vec::new(),
                authorization: Vec::new(),
                req: Vec::new(),
                req_done: false,
                dispatched: false,
                out: VecDeque::new(),
                done: false,
                status: GRPC_OK,
                status_msg: String::new(),
                deferred: false,
                work: Work::None,
            }
        });
    }
    0
}

unsafe extern "C" fn cb_header(
    _session: *mut ng::nghttp2_session,
    frame: *const ng::nghttp2_frame,
    name: *const u8,
    namelen: usize,
    value: *const u8,
    valuelen: usize,
    _flags: u8,
    user_data: *mut std::os::raw::c_void,
) -> std::os::raw::c_int {
    let conn = &mut *(user_data as *mut Conn);
    let id = (*frame).hd.stream_id;
    let name = std::slice::from_raw_parts(name, namelen);
    if name == b":path" {
        if let Some(s) = conn.streams.get_mut(&id) {
            s.path = std::slice::from_raw_parts(value, valuelen).to_vec();
        }
    }
    // nghttp2 hands header names over lowercased
    if name == b"authorization" && valuelen <= AUTHORIZATION_MAX {
        if let Some(s) = conn.streams.get_mut(&id) {
            s.authorization = std::slice::from_raw_parts(value, valuelen).to_vec();
        }
    }
    0
}

unsafe extern "C" fn cb_data_chunk(
    session: *mut ng::nghttp2_session,
    _flags: u8,
    stream_id: i32,
    data: *const u8,
    len: usize,
    user_data: *mut std::os::raw::c_void,
) -> std::os::raw::c_int {
    let conn = &mut *(user_data as *mut Conn);
    if let Some(s) = conn.streams.get_mut(&stream_id) {
        if s.req.len() + len > REQ_MAX {
            conn.streams.remove(&stream_id);
            ng::nghttp2_submit_rst_stream(
                session,
                ng::NGHTTP2_FLAG_NONE as u8,
                stream_id,
                ng::NGHTTP2_INTERNAL_ERROR,
            );
            return 0;
        }
        s.req.extend_from_slice(std::slice::from_raw_parts(data, len));
    }
    0
}

unsafe extern "C" fn cb_frame_recv(
    _session: *mut ng::nghttp2_session,
    frame: *const ng::nghttp2_frame,
    user_data: *mut std::os::raw::c_void,
) -> std::os::raw::c_int {
    let conn = &mut *(user_data as *mut Conn);
    let hd = (*frame).hd;
    if hd.type_ as u32 == ng::NGHTTP2_PING && hd.flags & (ng::NGHTTP2_FLAG_ACK as u8) != 0 {
        conn.ping_sent = None;
    }
    if hd.flags & (ng::NGHTTP2_FLAG_END_STREAM as u8) != 0 {
        if let Some(s) = conn.streams.get_mut(&hd.stream_id) {
            s.req_done = true;
        }
    }
    0
}

unsafe extern "C" fn cb_stream_close(
    _session: *mut ng::nghttp2_session,
    stream_id: i32,
    _error_code: u32,
    user_data: *mut std::os::raw::c_void,
) -> std::os::raw::c_int {
    let conn = &mut *(user_data as *mut Conn);
    conn.last_used = std::time::Instant::now();
    if let Some(s) = conn.streams.remove(&stream_id) {
        // a dead client's in-flight backend call shouldn't keep running
        match s.work {
            Work::Pending(t) => t.abort(),
            Work::Blocks { inflight: Some((_, t)), .. } => t.abort(),
            Work::Txs { inflight: Some((_, t)), .. } => t.abort(),
            Work::Mempool { inflight: Some(t), .. } => t.abort(),
            _ => {}
        }
    }
    0
}

/// Pull-based response body. Empty + not done => park (DEFERRED); the pump
/// resumes us once advance produced bytes. Empty + done => trailers.
unsafe extern "C" fn cb_data_read(
    session: *mut ng::nghttp2_session,
    stream_id: i32,
    buf: *mut u8,
    length: usize,
    data_flags: *mut u32,
    _source: *mut ng::nghttp2_data_source,
    user_data: *mut std::os::raw::c_void,
) -> isize {
    let conn = &mut *(user_data as *mut Conn);
    let Some(s) = conn.streams.get_mut(&stream_id) else {
        return ng::NGHTTP2_ERR_TEMPORAL_CALLBACK_FAILURE as isize;
    };

    if s.out.is_empty() {
        if s.done {
            *data_flags |= ng::NGHTTP2_DATA_FLAG_EOF | ng::NGHTTP2_DATA_FLAG_NO_END_STREAM;

            // submit_trailers: grpc-message must be percent-encoding-safe ASCII
            let clean: String = s
                .status_msg
                .chars()
                .map(|ch| if (' '..='~').contains(&ch) && ch != '%' { ch } else { '_' })
                .collect();
            let status_s = s.status.to_string();
            let mut nva = vec![nv(b"grpc-status", status_s.as_bytes())];
            if s.status != GRPC_OK && !clean.is_empty() {
                nva.push(nv(b"grpc-message", clean.as_bytes()));
            }
            ng::nghttp2_submit_trailer(session, stream_id, nva.as_ptr(), nva.len());

            return 0;
        }
        s.deferred = true;
        return ng::NGHTTP2_ERR_DEFERRED as isize;
    }

    let n = length.min(s.out.len());
    let dst = std::slice::from_raw_parts_mut(buf, n);
    for (i, b) in s.out.drain(..n).enumerate() {
        dst[i] = b;
    }
    if s.out.is_empty() && s.done {
        *data_flags |= ng::NGHTTP2_DATA_FLAG_EOF | ng::NGHTTP2_DATA_FLAG_NO_END_STREAM;

        // submit_trailers: grpc-message must be percent-encoding-safe ASCII
        let clean: String = s
            .status_msg
            .chars()
            .map(|ch| if (' '..='~').contains(&ch) && ch != '%' { ch } else { '_' })
            .collect();
        let status_s = s.status.to_string();
        let mut nva = vec![nv(b"grpc-status", status_s.as_bytes())];
        if s.status != GRPC_OK && !clean.is_empty() {
            nva.push(nv(b"grpc-message", clean.as_bytes()));
        }
        ng::nghttp2_submit_trailer(session, stream_id, nva.as_ptr(), nva.len());
    }
    n as isize
}

fn nv(name: &[u8], value: &[u8]) -> ng::nghttp2_nv {
    ng::nghttp2_nv {
        name: name.as_ptr() as *mut u8,
        value: value.as_ptr() as *mut u8,
        namelen: name.len(),
        valuelen: value.len(),
        flags: ng::NGHTTP2_NV_FLAG_NONE as u8,
    }
}

fn hash_or_height(b: &BlockId) -> Result<HashOrHeight, Grr> {
    if b.hash.len() == 32 {
        let arr: [u8; 32] = b.hash[..].try_into().unwrap();
        Ok(block::Hash(arr).into())
    } else if b.hash.is_empty() {
        let h: u32 = b
            .height
            .try_into()
            .map_err(|_| (GRPC_INVALID, "height out of range".to_string()))?;
        Ok(Height(h).into())
    } else {
        Err((GRPC_INVALID, "block hash must be 32 bytes".into()))
    }
}

// -------------------------------------------------------------------------
// Block -> CompactBlock

fn compact_block(b: &Block, nulls: bool) -> CompactBlock {
    let height = b.coinbase_height().map(|h| h.0 as u64).unwrap_or(0);
    let vtx = b
        .transactions
        .iter()
        .enumerate()
        .filter_map(|(i, tx)| {
            // compact_tx: strip one transaction to its compact form;
            // None if it has no shielded parts
            let spends: Vec<CompactSaplingSpend> = tx
                .sapling_nullifiers()
                .map(|nf| CompactSaplingSpend { nf: (*nf.0).to_vec() })
                .collect();

            let outputs: Vec<CompactSaplingOutput> = if nulls {
                Vec::new()
            } else {
                tx.sapling_outputs()
                    .map(|o| CompactSaplingOutput {
                        cmu: o.cm_u.to_bytes().to_vec(),
                        ephemeral_key: <[u8; 32]>::from(o.ephemeral_key).to_vec(),
                        ciphertext: <[u8; 580]>::from(o.enc_ciphertext)[..52].to_vec(),
                    })
                    .collect()
            };

            let actions: Vec<CompactOrchardAction> = tx
                .orchard_actions()
                .map(|a| {
                    if nulls {
                        CompactOrchardAction {
                            nullifier: <[u8; 32]>::from(a.nullifier).to_vec(),
                            cmx: Vec::new(),
                            ephemeral_key: Vec::new(),
                            ciphertext: Vec::new(),
                        }
                    } else {
                        CompactOrchardAction {
                            nullifier: <[u8; 32]>::from(a.nullifier).to_vec(),
                            cmx: <[u8; 32]>::from(a.cm_x).to_vec(),
                            ephemeral_key: <[u8; 32]>::from(a.ephemeral_key).to_vec(),
                            ciphertext: <[u8; 580]>::from(a.enc_ciphertext)[..52].to_vec(),
                        }
                    }
                })
                .collect();

            let sapling_output_count = if nulls {
                tx.sapling_outputs().count()
            } else {
                outputs.len()
            };
            // See the note above: Ironwood is where NU6.3 cross-address transfers live.
            let ironwood_actions: Vec<CompactOrchardAction> = tx
                .ironwood_actions()
                .map(|a| {
                    if nulls {
                        CompactOrchardAction {
                            nullifier: <[u8; 32]>::from(a.nullifier).to_vec(),
                            cmx: Vec::new(),
                            ephemeral_key: Vec::new(),
                            ciphertext: Vec::new(),
                        }
                    } else {
                        CompactOrchardAction {
                            nullifier: <[u8; 32]>::from(a.nullifier).to_vec(),
                            cmx: <[u8; 32]>::from(a.cm_x).to_vec(),
                            ephemeral_key: <[u8; 32]>::from(a.ephemeral_key).to_vec(),
                            ciphertext: <[u8; 580]>::from(a.enc_ciphertext)[..52].to_vec(),
                        }
                    }
                })
                .collect();
            if spends.is_empty() && sapling_output_count == 0 && actions.is_empty()
                && ironwood_actions.is_empty() {
                return None;
            }
            Some(CompactTx {
                index: i as u64,
                txid: tx.hash().0.to_vec(),
                fee: 0,
                spends,
                outputs,
                vin: Vec::new(),
                vout: Vec::new(),
                ironwood_actions,
                actions,
            })
        })
        .collect();
    CompactBlock {
        height,
        hash: b.hash().0.to_vec(),
        prev_hash: b.header.previous_block_hash.0.to_vec(),
        time: b.header.time.timestamp() as u32,
        header: Vec::new(),
        vtx,
        chain_metadata: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tonic::{metadata::MetadataValue, Code, Request};
    use zcash_client_backend::proto::service::{compact_tx_streamer_client::CompactTxStreamerClient, Empty};

    type Client = CompactTxStreamerClient<tonic::transport::Channel>;

    fn free_port() -> u16 {
        TcpListener::bind("127.0.0.1:0").expect("a free port").local_addr().expect("bound").port()
    }

    /// A running test server, stopped and joined when dropped, while the test's runtime its
    /// handlers use is still alive.
    struct Server {
        port: u16,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    /// A server on loopback whose faucet always pays 42, with no mempool, over an empty state.
    /// Unless `loopback_trusted`, its loopback callers are treated as other hosts.
    async fn serve(token: Option<&str>, loopback_trusted: bool) -> Server {
        serve_as(token, loopback_trusted, false).await
    }

    /// `serve`, as a public lightwalletd when `public`.
    async fn serve_as(token: Option<&str>, loopback_trusted: bool, public: bool) -> Server {
        let network = Network::new_default_testnet();
        let (_state, read_state, tip, _changes) = zebra_state::init_test_services(&network).await;
        let mempool: MempoolSvc = tower::buffer::Buffer::new(
            BoxService::new(tower::service_fn(|_: mempool::Request| async {
                Err::<mempool::Response, BoxError>("no mempool in this test".into())
            })),
            8,
        );
        let tfl: TflSvc = tower::buffer::Buffer::new(
            BoxService::new(tower::service_fn(|request: TFLServiceRequest| async move {
                match request {
                    TFLServiceRequest::Faucet(_) => Ok(TFLServiceResponse::Faucet(Ok(42))),
                    _ => Err(TFLServiceError::NotImplemented),
                }
            })),
            8,
        );
        let ctx = Ctx {
            rt: tokio::runtime::Handle::current(),
            read_state,
            mempool,
            tfl,
            tip,
            mempool_events: MempoolTxSubscriber::new(tokio::sync::broadcast::channel(8).0),
            network,
        };
        let port = free_port();
        let listen = Listen {
            grpc: SocketAddr::from((Ipv6Addr::LOCALHOST, port)),
            ready_port: free_port(),
            token: token.map(str::to_owned),
            public,
        };
        let stop = std::sync::Arc::<std::sync::atomic::AtomicBool>::default();
        let thread = spawn_server(ctx, listen, loopback_trusted, stop.clone());
        Server { port, stop, thread: Some(thread) }
    }

    async fn client(server: &Server) -> Client {
        let port = server.port;
        for _ in 0..200 {
            let endpoint = tonic::transport::Channel::from_shared(format!("http://127.0.0.1:{port}")).expect("a URL");
            if let Ok(channel) = endpoint.connect().await {
                return CompactTxStreamerClient::new(channel);
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        panic!("lightwallet_server never accepted a connection on {port}");
    }

    fn with_authorization<T>(message: T, authorization: &str) -> Request<T> {
        let mut request = Request::new(message);
        let value: MetadataValue<_> = authorization.parse().expect("ASCII header value");
        request.metadata_mut().insert("authorization", value);
        request
    }

    fn faucet() -> FaucetRequest {
        FaucetRequest { address: "any".into() }
    }

    /// The code a streaming call ends with: its own, or its first message's.
    async fn code_of_stream<T>(result: Result<tonic::Response<tonic::Streaming<T>>, tonic::Status>) -> Code {
        match result {
            Err(status) => status.code(),
            Ok(response) => match response.into_inner().message().await {
                Err(status) => status.code(),
                Ok(_) => Code::Ok,
            },
        }
    }

    #[test]
    fn grpc_binds_cover_both_loopback_addresses() {
        let listen = |grpc: &str| Listen { grpc: grpc.parse().expect("an address"), ready_port: 1, token: None, public: false };
        let addrs = |list: &[&str]| list.iter().map(|a| a.parse().expect("an address")).collect::<Vec<SocketAddr>>();
        assert_eq!(listen("[::1]:9").grpc_binds(), addrs(&["[::1]:9", "127.0.0.1:9"]));
        assert_eq!(listen("127.0.0.1:9").grpc_binds(), addrs(&["127.0.0.1:9", "[::1]:9"]));
        assert_eq!(listen("[::]:9").grpc_binds(), addrs(&["[::]:9", "0.0.0.0:9"]));
        assert_eq!(listen("0.0.0.0:9").grpc_binds(), addrs(&["0.0.0.0:9", "[::1]:9"]));
        assert_eq!(listen("192.0.2.7:9").grpc_binds(), addrs(&["192.0.2.7:9", "[::1]:9", "127.0.0.1:9"]));
    }

    /// IPv6 addresses in one /64 share a prefix and IPv4 addresses in one /16 share one; those in
    /// different blocks don't, and a v4-mapped address, once canonical, counts as IPv4.
    #[test]
    fn peer_prefix_groups_ipv6_by_64_and_ipv4_by_16() {
        let prefix = |ip: &str| peer_prefix(ip.parse::<IpAddr>().expect("an address").to_canonical());
        assert_eq!(prefix("2001:db8:1:2:aaaa::1"), prefix("2001:db8:1:2:ffff:ffff:ffff:ffff"));
        assert_eq!(prefix("2001:db8:1:2:aaaa::1").to_string(), "2001:db8:1:2::");
        assert_ne!(prefix("2001:db8:1:2::1"), prefix("2001:db8:1:3::1"));
        assert_eq!(prefix("192.0.2.7"), prefix("192.0.255.200"));
        assert_eq!(prefix("192.0.2.7").to_string(), "192.0.0.0");
        assert_ne!(prefix("192.0.2.7"), prefix("192.1.2.7"));
        assert_eq!(prefix("::ffff:192.0.2.7"), prefix("192.0.9.9"));
    }

    #[test]
    fn bearer_matches_only_the_exact_token() {
        assert!(bearer_matches(b"Bearer secret", "secret"));
        assert!(!bearer_matches(b"Bearer secreT", "secret"));
        assert!(!bearer_matches(b"Bearer secret2", "secret"));
        assert!(!bearer_matches(b"bearer secret", "secret"));
        assert!(!bearer_matches(b"secret", "secret"));
        assert!(!bearer_matches(b"", "secret"));
    }

    /// By default another host is refused every call without a token, the faucet included; this
    /// host is not.
    #[tokio::test(flavor = "multi_thread")]
    async fn another_host_needs_a_token_by_default() {
        let remote_server = serve(None, false).await;
        let remote = client(&remote_server).await;
        let refused = remote.clone().get_lightd_info(Empty {}).await.expect_err("refused");
        assert_eq!(refused.code(), Code::Unauthenticated, "{refused:?}");
        let refused = remote.clone().request_faucet_donation(faucet()).await.expect_err("refused");
        assert_eq!(refused.code(), Code::Unauthenticated, "{refused:?}");

        let local_server = serve(None, true).await;
        let local = client(&local_server).await;
        local.clone().get_lightd_info(Empty {}).await.expect("answered");
    }

    /// A public server answers another host without a token, except the faucet, which needs the
    /// token if there is one and is refused if not; this host may use the faucet either way.
    #[tokio::test(flavor = "multi_thread")]
    async fn public_server_answers_another_host_but_not_its_faucet() {
        let remote_server = serve_as(None, false, true).await;
        let remote = client(&remote_server).await;
        remote.clone().get_lightd_info(Empty {}).await.expect("answered");
        let refused = remote.clone().request_faucet_donation(faucet()).await.expect_err("refused");
        assert_eq!(refused.code(), Code::PermissionDenied, "{refused:?}");

        let tokened_server = serve_as(Some("s3cret"), false, true).await;
        let tokened = client(&tokened_server).await;
        tokened.clone().get_lightd_info(Empty {}).await.expect("answered without the token");
        let refused = tokened.clone().request_faucet_donation(faucet()).await.expect_err("refused");
        assert_eq!(refused.code(), Code::PermissionDenied, "{refused:?}");
        let paid = tokened.clone().request_faucet_donation(with_authorization(faucet(), "Bearer s3cret")).await;
        assert_eq!(paid.expect("paid").into_inner().amount, 42);

        let local_server = serve_as(None, true, true).await;
        let local = client(&local_server).await;
        assert_eq!(local.clone().request_faucet_donation(faucet()).await.expect("paid").into_inner().amount, 42);
    }

    /// With a token, another host needs it for every call, the faucet included.
    #[tokio::test(flavor = "multi_thread")]
    async fn token_gates_every_call_from_another_host() {
        let remote_server = serve(Some("s3cret"), false).await;
        let remote = client(&remote_server).await;
        for request in [Request::new(Empty {}), with_authorization(Empty {}, "Bearer wrong")] {
            let refused = remote.clone().get_lightd_info(request).await.expect_err("refused");
            assert_eq!(refused.code(), Code::Unauthenticated, "{refused:?}");
        }
        remote.clone().get_lightd_info(with_authorization(Empty {}, "Bearer s3cret")).await.expect("answered");
        let paid = remote.clone().request_faucet_donation(with_authorization(faucet(), "Bearer s3cret")).await;
        assert_eq!(paid.expect("paid").into_inner().amount, 42);

        // this host never needs it
        let local_server = serve(Some("s3cret"), true).await;
        let local = client(&local_server).await;
        local.clone().get_lightd_info(Empty {}).await.expect("answered");
    }

    /// Whether the server keeps `sock` open: it sends its SETTINGS first, so a kept connection
    /// has bytes to read, and a refused one reads as closed.
    fn kept(sock: &mut TcpStream) -> bool {
        sock.set_read_timeout(Some(std::time::Duration::from_secs(2))).expect("a timeout");
        let mut byte = [0u8; 1];
        matches!(sock.read(&mut byte), Ok(1))
    }

    /// Another host gets MAX_CONNS_PER_PREFIX connections from one IPv4 /16; this host is not
    /// counted.
    #[tokio::test(flavor = "multi_thread")]
    async fn connections_from_one_other_host_are_capped() {
        for loopback_trusted in [false, true] {
            let server = serve(None, loopback_trusted).await;
            drop(client(&server).await);
            let port = server.port;
            let mut socks = Vec::new();
            for _ in 0..MAX_CONNS_PER_PREFIX {
                let mut sock = TcpStream::connect(("127.0.0.1", port)).expect("connects");
                assert!(kept(&mut sock), "trusted: {loopback_trusted}");
                socks.push(sock);
            }
            let mut over = TcpStream::connect(("127.0.0.1", port)).expect("connects");
            assert_eq!(kept(&mut over), loopback_trusted, "trusted: {loopback_trusted}");
        }
    }

    /// A peer that never answers the keepalive PING is closed, before the idle timeout would.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_silent_peer_is_closed() {
        let server = serve(None, true).await;
        drop(client(&server).await);
        let mut sock = TcpStream::connect(("127.0.0.1", server.port)).expect("connects");
        sock.set_read_timeout(Some(KEEPALIVE_INTERVAL + KEEPALIVE_TIMEOUT + std::time::Duration::from_secs(3)))
            .expect("a timeout");
        let started = std::time::Instant::now();
        let mut buf = [0u8; 256];
        loop {
            match sock.read(&mut buf) {
                Ok(0) => break,
                Ok(_) => continue,
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => break,
                Err(e) => panic!("still open after {:?}: {e}", started.elapsed()),
            }
        }
        let elapsed = started.elapsed();
        assert!(elapsed >= KEEPALIVE_INTERVAL, "closed too soon: {elapsed:?}");
        assert!(elapsed < IDLE_TIMEOUT, "closed by the idle timeout, not the keepalive: {elapsed:?}");
    }

    /// A peer from another host that answers every PING but sends no request is kept past the
    /// keepalive, then sent GOAWAY once IDLE_TIMEOUT passes: the silent peer's control.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_idle_peer_gets_goaway() {
        const PING: u8 = 6;
        const GOAWAY: u8 = 7;
        const ACK: u8 = 1;
        let server = serve(None, false).await;
        drop(client(&server).await);
        let mut sock = TcpStream::connect(("127.0.0.1", server.port)).expect("connects");
        sock.set_read_timeout(Some(IDLE_TIMEOUT + std::time::Duration::from_secs(5))).expect("a timeout");
        // the client preface and an empty SETTINGS frame
        sock.write_all(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n\0\0\0\x04\0\0\0\0\0").expect("writes");
        let started = std::time::Instant::now();
        let mut pings = 0;
        loop {
            let mut head = [0u8; 9];
            sock.read_exact(&mut head).unwrap_or_else(|e| panic!("closed without GOAWAY after {:?}: {e}", started.elapsed()));
            let len = usize::from(head[0]) << 16 | usize::from(head[1]) << 8 | usize::from(head[2]);
            let mut payload = vec![0u8; len];
            sock.read_exact(&mut payload).expect("a whole frame");
            match head[3] {
                PING if head[4] & ACK == 0 => {
                    pings += 1;
                    let mut ack = vec![0, 0, 8, PING, ACK, 0, 0, 0, 0];
                    ack.extend_from_slice(&payload);
                    sock.write_all(&ack).expect("writes");
                }
                GOAWAY => break,
                _ => {}
            }
        }
        assert!(pings >= 1, "no keepalive PING before GOAWAY");
        assert!(started.elapsed() >= IDLE_TIMEOUT, "GOAWAY too soon: {:?}", started.elapsed());
    }

    /// One request may name at most MAX_ADDRESSES addresses; the control shows the address
    /// count is what refuses it.
    #[tokio::test(flavor = "multi_thread")]
    async fn too_many_addresses_are_refused() {
        let server = serve(None, true).await;
        let client = client(&server).await;
        for (count, needle) in [(MAX_ADDRESSES + 1, "at most"), (MAX_ADDRESSES, "bad transparent address")] {
            let addresses = vec!["not an address".to_string(); count];
            let refused = client.clone().get_taddress_balance(AddressList { addresses: addresses.clone() }).await;
            let refused = refused.expect_err("refused");
            assert_eq!(refused.code(), Code::InvalidArgument);
            assert!(refused.message().contains(needle), "{count}: {refused:?}");
            let utxos = client.clone().get_address_utxos(GetAddressUtxosArg { addresses, start_height: 0, max_entries: 0 }).await;
            assert!(utxos.expect_err("refused").message().contains(needle), "{count}");
        }
    }

    /// One block range may span at most MAX_BLOCK_RANGE blocks. The control span is allowed,
    /// and ends only because the empty state has no blocks.
    #[tokio::test(flavor = "multi_thread")]
    async fn long_block_ranges_are_refused() {
        let server = serve(None, true).await;
        let client = client(&server).await;
        let range = |end: u64| BlockRange {
            start: Some(BlockId { height: 0, hash: Vec::new() }),
            end: Some(BlockId { height: end, hash: Vec::new() }),
            pool_types: Vec::new(),
        };
        let over = u64::from(MAX_BLOCK_RANGE);
        assert_eq!(code_of_stream(client.clone().get_block_range(range(over)).await).await, Code::InvalidArgument);
        assert_eq!(code_of_stream(client.clone().get_block_range(range(over - 1)).await).await, Code::OutOfRange);
    }

    /// SendTransaction still refuses bytes that are not a transaction, now parsed off the
    /// server thread.
    #[tokio::test(flavor = "multi_thread")]
    async fn send_transaction_refuses_bad_bytes() {
        let server = serve(None, true).await;
        let client = client(&server).await;
        let refused = client.clone().send_transaction(RawTransaction { data: vec![1, 2, 3], height: 0 }).await;
        let refused = refused.expect_err("refused");
        assert_eq!(refused.code(), Code::InvalidArgument);
        assert!(refused.message().contains("bad transaction bytes"), "{refused:?}");
    }
}
