use std::collections::HashMap;
use crate::{Request, Response, ReadRequest, ReadResponse};
use tower::ServiceExt;
use zebra_chain::block::{self, Block, Hash, Height};
use zebra_chain::serialization::{ZcashSerialize, ZcashDeserialize};

use tenderlink::bandwidth_test::*;
use tenderlink::native_sockets::*;
use tenderlink::parse_to_ipv6_bytes;

#[derive(Clone, Copy, Debug)]
pub(crate) enum BlockEvent {
    Dequeued(Hash),
    Committed(Hash),
    TradFinalized(Hash),
    CrosslinkFinalized(Hash),
}
static BLOCK_EVENT_QUEUE_SENDER: std::sync::OnceLock<tokio::sync::mpsc::Sender<BlockEvent>> = std::sync::OnceLock::new();

pub async fn push_block_event(event: BlockEvent) {
    if let Some(tx) = BLOCK_EVENT_QUEUE_SENDER.get() {
        tx.send(event).await.unwrap();
    }
}

// ---------------------------------------------------------------------------
// NewNet packet types // @Todo: share common messages/code with Tenderlink
// ---------------------------------------------------------------------------

// @Todo: more of these, merge with Tenderlink, reintroduce peer discovery from p2p, etc.
const PACKET_TYPE_STATUS: u8 = 1;

// @Todo: Use encryption; put real STP addresses in the
// network_initial_peers instead of computing deterministic secrets from the adddress
fn peer_string_to_stp_address(addr: &str) -> Option<STPAddress> {
    let (ip, port) = parse_to_ipv6_bytes(addr).ok()?;
    use std::hash::{Hash as StdHash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    addr.hash(&mut hasher);
    let seed = hasher.finish();
    use rand::{Rng, SeedableRng};
    let mut other_seed = [0u8; 32];
    rand_chacha::ChaCha20Rng::seed_from_u64(seed).fill(&mut other_seed);
    let static_keypair = new_keypair_from_connect_magic1_with_seed(CONNECT_MAGIC1_PLAIN_TEXT, other_seed)?;
    Some(STPAddress {
        ip,
        port,
        magic1: CONNECT_MAGIC1_PLAIN_TEXT,
        key: static_keypair.public,
    })
}

#[derive(Clone, Copy, Debug)]
struct ShadowBlock {
    this_hash: Hash,
    parent_hash: Hash,
}

// @Todo: always only wait on real stuff, never sleeping for fixed amts like this
const TICK_MS: u64 = 300;

pub fn sync(
        config: &crate::config::Config,
        read_state: impl tower::Service<
            ReadRequest,
            Response = ReadResponse,
            Error = crate::BoxError,
        > + Clone
        + Send
        + Sync
        + 'static,
        state: impl tower::Service<
            Request,
            Response = Response,
            Error = crate::BoxError,
        > + Clone
        + Send
        + Sync
        + 'static,
        rt: tokio::runtime::Handle,
) {
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(500);
    BLOCK_EVENT_QUEUE_SENDER.set(event_tx).unwrap();

    let mut finalized_tip_maybe = None;
    loop {
        finalized_tip_maybe = rt.block_on(async {
            let res = read_state.clone().oneshot(ReadRequest::FinalizedTip).await;
            match res {
                Ok(ReadResponse::Tip(finalized_tip_maybe)) => finalized_tip_maybe,
                Err(err) => panic!("sync start err: {err:?}"),
                _ => panic!("sync err: unhandled response: {res:?}"),
            }
        });
        if finalized_tip_maybe.is_none() {
            std::thread::yield_now();
            continue;
        }
        break;
    }
    let (mut finalized_height, mut finalized_hash) = finalized_tip_maybe.unwrap();
    println!("NewNet: Starting at height={} hash={:?}", finalized_height.0, finalized_hash);

    // Keypair setup
    let network_keypair;
    if let Some(string_seed) = &config.network_identity_seed_string {
        let hash = *blake3::hash(string_seed.as_bytes()).as_bytes();
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&hash);
        network_keypair = new_keypair_from_connect_magic1_with_seed(CONNECT_MAGIC1_PLAIN_TEXT, seed).unwrap();
    }
    else {
        network_keypair = new_keypair_from_connect_magic1(CONNECT_MAGIC1_PLAIN_TEXT).unwrap();
    }
    println!("NETWORK KEYPAIR: {}", network_keypair);

    // STP networking setup
    socket_setup();
    monotonic_clock_setup();

    let socket = match setup_and_bind_udp_socket(config.network_local_port) {
        Some(s) => {
            println!("NewNet: Bound to port {}", config.network_local_port);
            s
        }
        None => {
            eprintln!("NewNet: Port {} unavailable, binding to ephemeral port", config.network_local_port);
            setup_and_bind_udp_socket(0).expect("Failed to bind UDP socket on any port")
        }
    };
    let my_keypairs = vec![&network_keypair];

    let mut packet_memory_encrypted = new_packet_memory();
    let mut packet_memory_recv      = new_packet_memory();
    let mut packet_memory_send      = new_packet_memory();

    let mut packets_to_send:  Vec<(ConnectionKey, Vec<u8>)> = Vec::new();
    let mut packets_received: Vec<(ConnectionKey, Vec<u8>)> = Vec::new();

    let mut connections_map = HashMap::<ConnectionKey, ConnectionTrackingData>::new();

    // Parse and connect to initial peers
    let mut peer_addresses: Vec<STPAddress> = Vec::new();
    for peer_str in &config.network_initial_peers {
        if let Some(address) = peer_string_to_stp_address(peer_str) {
            println!("NewNet: Connecting to peer: {:?}", address);
            let _ = connect_to(socket, &mut connections_map, &my_keypairs, &address);
            peer_addresses.push(address);
        } else {
            eprintln!("NewNet: Failed to parse peer address: {}", peer_str);
        }
    }

    println!("NewNet: STP setup complete, port={}, peers={}", config.network_local_port, peer_addresses.len());

    // Sync state
    let mut awaiting_blocks_from: Option<ConnectionKey> = None;
    let mut last_request_time = std::time::Instant::now();
    let tick_duration = std::time::Duration::from_millis(TICK_MS);

    // Main sync loop
    loop {
        let loop_start = std::time::Instant::now();

        // Drain block events
        loop {
            match event_rx.try_recv() {
                Ok(block_event) => {
                    match block_event {
                        BlockEvent::Committed(hash) => {
                            // @Todo: real block stuff, put it in a data structure, repeatedly announce blocks, etc.
                            let mut msg = [0u8; 11];
                            msg[0] = hash.0[0] | 1;
                            msg[1..11].copy_from_slice("New block!".as_bytes()); // @Debug

                            for (key, connection) in &connections_map {
                                if !connection.is_connected() {
                                    continue;
                                }
                                packets_to_send.push((*key, Vec::from(msg)));
                            }
                        }
                        _ => {
                            println!("NewNet: block_event: {:?}", block_event);
                        }
                    }
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                Err(err) => { tracing::error!("{err:?}"); break; },
            }
        }

        // Try to reconnect to known but disconnected peers
        // @Todo: peer discovery
        for address in &peer_addresses {
            if !connections_map.contains_key(&address.connection_key()) {
                let _ = connect_to(socket, &mut connections_map, &my_keypairs, address);
            }
        }


        for (key, connection) in &connections_map {
            if !connection.is_connected() {
                continue;
            }

            let mut msg = [0u8; 8];
            msg[0] = 0;
            msg[1..8].copy_from_slice("Status!".as_bytes()); // @Debug

            packets_to_send.push((*key, Vec::from(msg)));
        }

        // Service STP connections (send/recv)
        service_connections(&mut connections_map,
                            &mut packets_received,
                            &packets_to_send,
                            &mut packet_memory_encrypted,
                            &mut packet_memory_recv,
                            &mut packet_memory_send,
                            socket,
                            &my_keypairs);
        packets_to_send.clear();

        // Process received packets
        while let Some((connection_key, msg)) = packets_received.pop() {
            if msg.is_empty() {
                // @Todo: disconnect this peer, likely denial of some kind or faulty
                continue;
            }

            println!("NewNet: Got new msg: {:?}", msg[0]);
            if msg.len() > 1 {
                let msg = &msg[1..];
                println!("NewNet: Msg had contents: {:?}", std::str::from_utf8(msg).unwrap_or("?").trim_end_matches('\0'));
            }
        }

        // Sleep remainder of tick
        let elapsed = loop_start.elapsed();
        if elapsed < tick_duration {
            std::thread::sleep(tick_duration - elapsed);
        }
    }
}
