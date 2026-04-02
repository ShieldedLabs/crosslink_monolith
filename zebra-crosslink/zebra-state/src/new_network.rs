use std::collections::{HashMap, HashSet};
use crate::{Request, Response, ReadRequest, ReadResponse};
use tower::ServiceExt;
use zebra_chain::block::{self, Block, Hash, Height};
use zebra_chain::serialization::{ZcashSerialize, ZcashDeserialize};

use tenderlink::bandwidth_test::*;
use tenderlink::native_sockets::*;
use tenderlink::parse_to_ipv6_bytes;
use tenderlink::{SliceWrite, SliceRead};

#[derive(Clone, Copy, Debug)]
pub(crate) enum BlockEvent {
    Dequeued(Hash),
    Committed(Hash),
    TradFinalized(Hash),
    CrosslinkFinalized(Hash),
}
static BLOCK_EVENT_QUEUE_SENDER: std::sync::OnceLock<tokio::sync::mpsc::Sender<BlockEvent>> = std::sync::OnceLock::new();

pub fn push_block_event(event: BlockEvent) {
    if let Some(tx) = BLOCK_EVENT_QUEUE_SENDER.get() {
        tx.blocking_send(event).unwrap();
    }
}

// ---------------------------------------------------------------------------
// NewNet packet types // @Todo: share common messages/code with Tenderlink
// ---------------------------------------------------------------------------

//const CRYPTO_MAGIC: u64 = CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2b;
const CRYPTO_MAGIC: u64 = CONNECT_MAGIC1_PLAIN_TEXT;

// @Todo: more of these, merge with Tenderlink, reintroduce peer discovery from p2p, etc.
const PACKET_TYPE_STATUS: u8 = 1;
const PACKET_TYPE_BLOCK: u8 = 2;

#[derive(Clone, Copy, Debug)]
struct PacketStatus {
    height: u32,
    hash:   Hash,
}
const PACKET_STATUS_SIZE: usize = 4 /*height*/ + 32 /*hash*/;

impl SliceWrite for PacketStatus {
    fn write_to(&self, buf: &mut [u8]) -> usize {
        let mut o = 0;
        o += self.height.write_to(&mut buf[o..]);
        o += self.hash.0.write_to(&mut buf[o..]);
        o
    }
}
impl SliceRead for PacketStatus {
    fn read_from(buf: &mut &[u8]) -> Option<Self> {
        Some(PacketStatus {
            height: u32::read_from(buf)?,
            hash:   Hash(SliceRead::read_from(buf)?)
        })
    }
}

#[derive(Clone, Copy, Debug)]
struct PacketHashTreeHdr {
    tip_height: u32,
    hashes_start_offset: u16,
    // [PacketHashBranch]
    // [Hash] @ hashes_start_offset
}
impl SliceWrite for PacketHashTreeHdr {
    fn write_to(&self, buf: &mut [u8]) -> usize {
        let mut o = 0;
        o += self.tip_height.write_to(&mut buf[o..]);
        o += self.hashes_start_offset.write_to(&mut buf[o..]);
        o
    }
}

#[derive(Clone, Copy, Debug)]
struct PacketHashBranch {
    parent_hash_idx: u16,
    // start index is implicit from sequential cursor (the end index of the previous branch)
    branch_end_idx: u16,
    // if parent_hash_idx == cursor, there's a parent_height: u32
}
impl SliceWrite for PacketHashBranch {
    fn write_to(&self, buf: &mut [u8]) -> usize {
        let mut o = 0;
        o += self.parent_hash_idx.write_to(&mut buf[o..]);
        o += self.branch_end_idx.write_to(&mut buf[o..]);
        o
    }
}

#[derive(Clone, Copy, Debug)]
struct ShadowBlock {
    this_hash:   Hash,
    parent_hash: Hash,
    this_height: u32,
    // TODO: work
}
impl ShadowBlock {
    fn work(&self) -> u32 {
        self.this_height // @Hack, not @Prod
    }
}


// currently assumes each block arrives after its parent
pub fn write_packet_hash_tree(buf: &mut [u8], tip_height: u32, blocks: &[ShadowBlock]) {
    // doing parallel chains (redundantly keeping shared prefixes)
    // ALT: index tree in single buffer

    let mut chains = Vec::<Vec<ShadowBlock>>::new();
    // (a, b), (b, c,), (c, d), (d, e), ..., (c, i), (i, j), ...

    // a b c d e f g h
    //     \ i j k l m n

    for block in blocks {
        let mut found = None;
        for (chain_idx, chain) in chains.iter().enumerate() {
            if let Some(parent_idx) = chain.iter().position(|b| b.this_hash == block.parent_hash) {
                found = Some((chain_idx, parent_idx));
                if parent_idx == chain.len()-1 {
                    break; // TODO (perf): I think we can unconditionally break on found, but I'd need to double-check invariants
                }
            }
        }

        // TODO: handling sets in theory demands prepending parents before children; TBD if
        // streaming obsoletes this

        if let Some((mut chain_idx, parent_idx)) = found {
            if parent_idx != chains[chain_idx].len()-1 {
                chains.push(chains[chain_idx][..parent_idx+1].to_vec());
                chain_idx = chains.len()-1;
            }
            chains[chain_idx].push(*block);
        } else {
            chains.push(vec![*block]);
        }
    }

    // a b c d e f g h
    // a b c i j k l m n
    chains.sort_by_key(|ch| std::cmp::Reverse(ch.len())); // proxy for work // TODO @Prod: use work

    // a b c i j k l m n
    // a b c d e f g h


    let mut runs = Vec::<(PacketHashBranch, u32)>::new();
    let mut hashes = Vec::<Hash>::new();

    //         0 1 2 3 4 5 6 7 8 9 a b c d
    // hashes: a b c i j k l m n d e f g h
    // runs:   (0,9), (2, e)

    for chain in &chains {
        let parent_idx = if let Some(parent_idx) = hashes.iter().position(|h| *h == chain[0].parent_hash) {
            parent_idx
        } else {
            // new tree
            hashes.push(chain[0].parent_hash);
            hashes.len()-1
        };

        let mut branch = PacketHashBranch {
            parent_hash_idx: parent_idx.try_into().unwrap(),
            branch_end_idx: hashes.len().try_into().unwrap(), // fixed up after loop
        };

        for block in chain {
            if hashes[parent_idx+1..].contains(&block.this_hash) {
                continue;
            }
            hashes.push(block.this_hash);
        }
        debug_assert!((branch.branch_end_idx as usize) < hashes.len(), "DEV: nothing from chain was used");

        branch.branch_end_idx = hashes.len().try_into().unwrap();
        runs.push((branch, chain[0].this_height));
    }


    // packet size = sizeof(hdr + 2 runs) + 0xa hashes [0, 1]
    //                                     0 1 2 3 4 5 6 7 8 9 a b c d
    // tip, offset_of(a), (0,9), (2, 0xa), a b c i j k l m n d - - - -
    let mut hdr = PacketHashTreeHdr {
        tip_height,
        hashes_start_offset: 0, // fixed up
    };

    // TODO (perf): merge into loop above
    let mut o      = hdr.write_to(&mut buf[..]);
    let mut hash_c = 0usize;
    for (mut branch, height) in &runs {
        let run_start_if_last_run = o + std::mem::size_of_val(&branch) + (hash_c * 32);
        if run_start_if_last_run + 32 > buf.len() {
            // wouldn't be able to fit any more hashes in, no point in starting another run
            break;
        }

        let end = run_start_if_last_run + (branch.branch_end_idx as usize * 32);
        if end > buf.len() {
            let rem_size = buf.len() - run_start_if_last_run;
            let rem_hashes = rem_size / 32;
            branch.branch_end_idx = (hash_c + rem_hashes).try_into().unwrap();
        }

        o += branch.write_to(&mut buf[o..]);
        if (<usize>::from(branch.parent_hash_idx) == hash_c) {
            o += height.write_to(&mut buf[o..]);
        }
        hash_c = branch.branch_end_idx.into();
    }

    hdr.hashes_start_offset = o.try_into().unwrap();
    _ = hdr.write_to(&mut buf[..]); // fixup initial header

    for hash in &hashes[..hash_c] {
        o += hash.0.write_to(&mut buf[o..])
    }
}


// @Todo: always only wait on real stuff, never sleeping for fixed amounts like this
const TICK_MS: u64 = 500;

type ReadState = crate::service::ReadStateService;
type State = tower::buffer::Buffer<tower::util::BoxService<Request, Response, crate::BoxError>, Request>;

pub fn get_tip(read_state: &ReadState, rt: &tokio::runtime::Handle) -> Option<(Height, Hash)> {
    let tip_maybe = rt.block_on(async {
        let res = read_state.clone().oneshot(ReadRequest::Tip).await;
        match res {
            Ok(ReadResponse::Tip(tip_maybe)) => tip_maybe,
            Err(err) => panic!("sync start err: {err:?}"),
            _ => panic!("sync err: unhandled response: {res:?}"),
        }
    });
    tip_maybe
}

pub fn sync(
    config: &crate::config::Config,
    read_state: ReadState,
    state: State,
    rt: tokio::runtime::Handle,
) {
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(500);
    BLOCK_EVENT_QUEUE_SENDER.set(event_tx).unwrap();

    let (mut tip_height, mut tip_hash) = loop {
        let Some(tip) = get_tip(&read_state, &rt)
        else {
            std::thread::yield_now();
            continue;
        };
        break tip;
    };
    println!("NewNet: Starting at height={} hash={:?}", tip_height.0, tip_hash);

    // Keypair setup
    let network_keypair;
    if let Some(string_seed) = &config.network_identity_seed_string {
        let hash = *blake3::hash(string_seed.as_bytes()).as_bytes();
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&hash);
        network_keypair = new_keypair_from_connect_magic1_with_seed(CRYPTO_MAGIC, seed).unwrap();
    }
    else {
        network_keypair = new_keypair_from_connect_magic1(CRYPTO_MAGIC).unwrap();
    }
    println!("NewNet: KEYPAIR: {}", network_keypair);

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
        if let Some(address) = STPAddress::parse(peer_str) {
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
    let mut peer_statuses = HashMap::<ConnectionKey, PacketStatus>::new();

    let mut committed_blocks = HashSet::new();

    // Main sync loop
    loop {
        if let Some(tip) = get_tip(&read_state, &rt) {
            (tip_height, tip_hash) = tip;
        }
        println!("tip height: {}", tip_height.0);

        let loop_start = std::time::Instant::now();

        // Drain block events
        loop {
            match event_rx.try_recv() {
                Ok(block_event) => {
                    match block_event {
                        BlockEvent::Committed(hash) => {
                            // @Todo: announce this block
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
                println!("NewNet: Connecting to {:?}...", address);
                let _ = connect_to(socket, &mut connections_map, &my_keypairs, address);
            }
        }

        // @Todo: Send all chains, not just best chain.
        // Currently this only lives for one tick because we currently only map height->best chain block.
        let mut serialized_blocks = HashMap::new();

        for (connection_key, peer_status) in &peer_statuses {
            let Some(connection) = get_connected(&connections_map, connection_key) else { continue; };

            for height in peer_status.height.saturating_sub(10)..peer_status.height + 2 {
                if !serialized_blocks.contains_key(&height) {
                    let res = rt.block_on(async {
                        read_state.clone().oneshot(ReadRequest::Block(Height(height).into())).await
                    });
                    match res {
                        Ok(ReadResponse::Block(Some(block))) => {
                            serialized_blocks.insert(height, (block.zcash_serialize_to_vec().unwrap(), block.hash()));
                        }
                        Ok(ReadResponse::Block(None)) => { break; }
                        Err(err) => { panic!("sync start err: {err:?}");               }
                        _        => { panic!("sync err: unhandled response: {res:?}"); }
                    }
                }
                let (serialized, hash) = &serialized_blocks[&height];

                let mut buf = Vec::new();

                if serialized.len() >= (1 << 23) - 1 {
                    eprintln!("NewNet ERROR: Block too big! Was {:?} bytes, max is {}!", serialized.len(), (1 << 23) - 1);
                    continue;
                }

                buf.push(PACKET_TYPE_BLOCK);
                buf.extend(serialized);

                packets_to_send.push((*connection_key, buf));
                // eprintln!("\x1b[93mPOWLINK2 SENDING BLOCK HASH\x1b[0m: {}", hash);
            }
        }


        let our_status = PacketStatus { height: tip_height.0, hash: tip_hash };
        for (key, connection) in &connections_map {
            if !connection.is_connected() {
                continue;
            }

            let mut buf = [0u8; 1 + PACKET_STATUS_SIZE];
            let mut o = 0;
            o += PACKET_TYPE_STATUS.write_to(&mut buf[o..]);
            o += our_status        .write_to(&mut buf[o..]);

            packets_to_send.push((*key, Vec::from(&buf[..o])));
        }

        use rand::seq::SliceRandom;
        packets_to_send.shuffle(&mut rand::thread_rng());
        // Service STP connections (send/recv).
        // @Todo: real scheduling. Right now I just want to receive everything!
        for _ in 0..256 {
            let more = service_connections(&mut connections_map,
                                           &mut packets_received,
                                           &packets_to_send,
                                           // &packets_that_failed_to_send_due_to_congestion,
                                           &mut packet_memory_encrypted,
                                           &mut packet_memory_recv,
                                           &mut packet_memory_send,
                                           socket,
                                           &my_keypairs);

            // for packet in &packets_that_failed_to_send_due_to_congestion {
            // }

            // @Todo: Just take a mut to the packets_to_send in service_connections and clear it inside?
            packets_to_send.clear();

            if !more {
                break;
            }
        }

        // // hypothetical: something "we may want" to "avoid pessimal drop behaviour"
        // packets_received.shuffle();

        // Process received packets
        while packets_received.len() > 0 {
            let (connection_key, msg) = packets_received.remove(0);

            // if time_limit_exceeded {
            //     stp_library::signal_backpressure(PacketID(&msg));
            //     break; // Congested! Drop remainder!
            // }

            if msg.is_empty() {
                // @Todo: disconnect this peer, likely denial of some kind or faulty
                continue;
            }

            let packet_type = msg[0];

            if packet_type == PACKET_TYPE_STATUS {
                let Some(status) = PacketStatus::read_from(&mut &msg[1..]) else { continue; };

                let prev = peer_statuses.get(&connection_key).copied();
                peer_statuses.insert(connection_key, status);

                // Log when a peer's height changes or is first seen
                let changed = prev.map_or(true, |p| p.height != status.height);
                if changed {
                    println!("NewNet: Peer {:?} at height={} hash={:?}", connection_key, status.height, status.hash);
                    if status.height > tip_height.0 {
                        println!("NewNet: Peer {:?} is ahead of us ({} > {})", connection_key, status.height, tip_height.0);
                    }
                }
            } else if packet_type == PACKET_TYPE_BLOCK {
                use zebra_chain::serialization::ZcashDeserializeInto;
                let Ok(block) = (&msg[1..]).zcash_deserialize_into::<Block>() else { continue; };

                let hash = block.hash();
                // eprintln!("\x1b[93mPOWLINK2 GOT BLOCK HASH\x1b[0m: {}", hash);

                // skip already committed blocks
                if committed_blocks.contains(&hash) {
                    // println!("already committed!: {}", hash);
                    continue;
                }

                println!("new block, committing!: {}", hash);

                // @Todo(Phil): Do we need to semantically verify?
                let res = rt.block_on(async {
                    state.clone().oneshot(Request::CommitSemanticallyVerifiedBlock(crate::SemanticallyVerifiedBlock::from(std::sync::Arc::new(block)))).await
                });
                match res {
                    Ok(_) => {
                        committed_blocks.insert(hash);
                        println!("committed!: {}", hash);
                    }
                    Err(error) => {
                        if let Some(commit_err) = error.downcast_ref::<crate::error::CommitSemanticallyVerifiedError>() {
                            match &commit_err.0 {
                                crate::ValidateContextError::AlreadyFinalized { .. } => {
                                    committed_blocks.insert(hash);
                                    println!("Already was committed: {}", hash);
                                }
                                other => {
                                    println!("Failed to commit {}: {:?}", hash, other);
                                }
                            }
                        } else {
                            println!("Failed to commit {}: {:?}", hash, error);
                        }
                    }
                }
            } else {
                println!("NewNet: Got unknown msg type={} len={}", packet_type, msg.len());
            }

        }

        // Remove statuses for disconnected peers
        peer_statuses.retain(|key, _| connections_map.contains_key(key));

        // Sleep remainder of tick
        let elapsed = loop_start.elapsed();
        if elapsed < tick_duration {
            std::thread::sleep(tick_duration - elapsed);
        }
    }
}

mod tests {
    use super::*;

    #[test]
    fn create_packet_from_block_hashes() {
        let blocks = [
            ShadowBlock { parent_hash: Hash([0x01;32]), this_hash: Hash([0x02;32]), this_height: 2 },
            ShadowBlock { parent_hash: Hash([0x02;32]), this_hash: Hash([0x03;32]), this_height: 3 },
            ShadowBlock { parent_hash: Hash([0x03;32]), this_hash: Hash([0x04;32]), this_height: 4 },
            ShadowBlock { parent_hash: Hash([0x04;32]), this_hash: Hash([0x05;32]), this_height: 5 },
            ShadowBlock { parent_hash: Hash([0x05;32]), this_hash: Hash([0x06;32]), this_height: 6 },
            ShadowBlock { parent_hash: Hash([0x06;32]), this_hash: Hash([0x07;32]), this_height: 7 },
            ShadowBlock { parent_hash: Hash([0x07;32]), this_hash: Hash([0x08;32]), this_height: 8 },
            ShadowBlock { parent_hash: Hash([0x08;32]), this_hash: Hash([0x09;32]), this_height: 9 },
            ShadowBlock { parent_hash: Hash([0x09;32]), this_hash: Hash([0x0a;32]), this_height:10 },

            // fork off 05
            ShadowBlock { parent_hash: Hash([0x05;32]), this_hash: Hash([0x16;32]), this_height: 6 },
            ShadowBlock { parent_hash: Hash([0x16;32]), this_hash: Hash([0x17;32]), this_height: 7 },
            ShadowBlock { parent_hash: Hash([0x17;32]), this_hash: Hash([0x18;32]), this_height: 8 },
            ShadowBlock { parent_hash: Hash([0x18;32]), this_hash: Hash([0x19;32]), this_height: 9 },
            ShadowBlock { parent_hash: Hash([0x19;32]), this_hash: Hash([0x1a;32]), this_height:10 },
            ShadowBlock { parent_hash: Hash([0x1a;32]), this_hash: Hash([0x1b;32]), this_height:11 },
        ];

        let tip_height = 11;
        let mut buf = [0u8, 1200];
        write_packet_hash_tree(&mut buf, tip_height, &blocks);

        println!("{}", hex::encode(buf));
    }
}

