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

// const CRYPTO_MAGIC: u64 = CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2s;
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
impl SliceRead for PacketHashTreeHdr {
    fn read_from(buf: &mut &[u8]) -> Option<Self> {
        Some(Self {
            tip_height: u32::read_from(buf)?,
            hashes_start_offset: u16::read_from(buf)?,
        })
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
impl SliceRead for PacketHashBranch {
    fn read_from(buf: &mut &[u8]) -> Option<Self> {
        Some(PacketHashBranch {
            parent_hash_idx: u16::read_from(buf)?,
            branch_end_idx: u16::read_from(buf)?,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ShadowBlock {
    this_hash:   Hash,
    parent_hash: Hash,
    this_height: u32,
    // TODO: work
}
impl ShadowBlock {
    fn work(&self) -> u128 {
        1 // @Hack, not @Prod
    }
}


const NEAR_TIP_CHAIN_LEN: u32 = 100;

#[derive(Clone, Debug, PartialEq, Eq)]
struct NearTipChain {
    work: u128,
    blocks: Vec<ShadowBlock>, // TODO: circular buffer tracking behind tip (N.B. tip not necessarily being longest means that buffers must have independent start points)
}
impl NearTipChain {
    pub fn push_block(&mut self, block: ShadowBlock) -> usize {
        if (self.blocks.len() == NEAR_TIP_CHAIN_LEN as usize) {
            self.blocks.remove(0);
        }
        self.work += block.work();
        self.blocks.push(block);
        self.blocks.len()-1
    }
}

/// Similar parallel chain model to Zebra's NonFinalizedState.
///
/// Not exactly "non-finalized", as it may include finalized blocks
/// (either on startup or after crosslink finalization)
#[derive(Clone, Debug, PartialEq, Eq)]
struct NearTipChains {
    chains: Vec<NearTipChain>,
}
impl NearTipChains {
    /// Height of *best* chain, which is probably, but not necessarily, the longest chain
    pub fn tip_height(&self) -> Option<u32> {
        self.chains.first().and_then(|ch| ch.blocks.last()).map(|bl| bl.this_height)
    }

    pub fn push_chain(&mut self, blocks: Vec<ShadowBlock>) -> usize {
        let mut work = 0;
        for block in &blocks {
            work += block.work();
        }
        self.chains.push(NearTipChain { work, blocks });
        self.chains.len()-1
    }


    pub fn push_blocks(&mut self, blocks: &[ShadowBlock]) {
        if blocks.len() == 0 {
            return;
        }

        for block in blocks {
            let mut found = None;
            // find chain on which the parent lives (ideally as the last item)
            for (chain_idx, chain) in self.chains.iter().enumerate() {
                if let Some(parent_idx) = chain.blocks.iter().position(|b| b.this_hash == block.parent_hash) {
                    found = Some((chain_idx, parent_idx));
                    if parent_idx == chain.blocks.len()-1 {
                        break; // TODO (perf): I think we can unconditionally break on found, but I'd need to double-check invariants
                    }
                }
            }

            let chain_idx = if let Some((mut chain_idx, parent_idx)) = found {
                let blocks = &self.chains[chain_idx].blocks;
                if parent_idx != blocks.len()-1 {
                    self.push_chain(blocks[..parent_idx+1].to_vec())
                } else {
                    chain_idx
                }
            } else {
                self.push_chain(Vec::new())
            };

            self.chains[chain_idx].push_block(*block);
        }

        // TODO: drop the bad chains if they're
        // updated to the max(chain.tip_height, bc_tip_height).saturating_sub(NEAR_TIP_CHAIN_LEN)
        let bc_tip_height = self.tip_height().expect("early-outed if there were no blocks");
        self.chains.retain_mut(|chain| {
            debug_assert!(chain.blocks.len() > 0, "should have been removed if empty");
            while chain.blocks[0].this_height < bc_tip_height.saturating_sub(NEAR_TIP_CHAIN_LEN) {
                chain.blocks.remove(0);
                if chain.blocks.len() == 0 {
                    return false;
                }
            }
            true
        });

        self.chains.sort_by_key(|ch| std::cmp::Reverse(ch.work));
    }
}

impl SliceWrite for NearTipChains {
    // currently assumes each block arrives after its parent
    fn write_to(&self, buf: &mut [u8]) -> usize {
        // doing parallel chains (redundantly keeping shared prefixes)
        // ALT: index tree in single buffer

        let tip_height = self.tip_height().expect("programmer error: should be non-empty");

        let mut runs = Vec::<(PacketHashBranch, u32)>::new();
        let mut hashes = Vec::<Hash>::new();

        //         0 1 2 3 4 5 6 7 8 9 a b c d
        // hashes: a b c i j k l m n d e f g h
        // runs:   (0,9), (2, e)

        for chain in &self.chains {
            let parent_idx = if let Some(parent_idx) = hashes.iter().position(|h| *h == chain.blocks[0].parent_hash) {
                parent_idx
            } else {
                // new tree
                hashes.push(chain.blocks[0].parent_hash);
                hashes.len()-1
            };

            let mut branch = PacketHashBranch {
                parent_hash_idx: parent_idx.try_into().unwrap(),
                branch_end_idx: hashes.len().try_into().unwrap(), // fixed up after loop
            };

            for block in &chain.blocks {
                if hashes[parent_idx+1..].contains(&block.this_hash) {
                    continue;
                }
                hashes.push(block.this_hash);
            }
            debug_assert!((branch.branch_end_idx as usize) < hashes.len(), "DEV: nothing from chain was used");

            branch.branch_end_idx = hashes.len().try_into().unwrap();
            runs.push((branch, chain.blocks[0].this_height));
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

        o
    }
}

struct NearTipBranches {
    tip_height: u32,
    branches: Vec<Vec<ShadowBlock>>,
}
impl SliceRead for NearTipBranches {
    fn read_from(buf: &mut &[u8]) -> Option<Self> {
        let full_buf = *buf;
        let hdr = PacketHashTreeHdr::read_from(buf)?;
        *buf = &buf[..(hdr.hashes_start_offset as usize).saturating_sub(std::mem::size_of::<PacketHashTreeHdr>())];
        let mut buf_hashes: &mut &[u8] = &mut &full_buf[hdr.hashes_start_offset as usize..];
        let mut branches = Vec::new();

        let mut hash_c = 0;
        let mut branch_height = 0;
        while buf.len() > 0 {
            let branch = PacketHashBranch::read_from(buf)?;
            if branch.parent_hash_idx >= branch.branch_end_idx {
                branch_height = u32::read_from(buf)?;
                hash_c += (branch.parent_hash_idx == branch.branch_end_idx) as u16;
            }
            else {
                // TODO: get branch height from previous hash point (height from beginning of run +
                // offset)
                // branches.find...
            }
            if branch.branch_end_idx as usize * 32 > buf_hashes.len() {
                println!("received invalid Hash Tree packet (branch end)");
                return None;
            }
            let parent_i = branch.parent_hash_idx as usize;
            if (parent_i.saturating_add(1)) * 32 > buf_hashes.len() {
                println!("received invalid Hash Tree packet (parent hash)");
                return None;
            }


            let mut parent_hash = [0u8;32];
            parent_hash.copy_from_slice(&buf_hashes[parent_i * 32..(parent_i+1) * 32]);

            let branch_hashes_size = (branch.branch_end_idx - hash_c) as usize;
            let mut branch_blocks = Vec::with_capacity(branch_hashes_size / 32);
            for i in (hash_c as usize) .. (branch.branch_end_idx as usize) {
                let mut hash = [0u8;32];
                hash.copy_from_slice(&buf_hashes[i*32 .. (i+1)*32]);

                branch_blocks.push(ShadowBlock{
                    parent_hash: Hash(parent_hash),
                    this_hash: Hash(hash),
                    this_height: branch_height,
                });
                parent_hash = hash;
                branch_height += 1;
            }

            hash_c = branch.branch_end_idx;

            branches.push(branch_blocks);
        }

        Some(Self { tip_height: hdr.tip_height, branches })
    }
}


// @Todo: always only wait on real stuff, never sleeping for fixed amounts like this
const TICK_MS: u64 = 500;

type ReadState = crate::service::ReadStateService;
type State = tower::buffer::Buffer<tower::util::BoxService<Request, Response, crate::BoxError>, Request>;

// TODO: the handling for these calls is sync, so don't have the indirection through async

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

pub fn get_bc_hash_at_height(read_state: &ReadState, rt: &tokio::runtime::Handle, height: Height) -> Option<Hash> {
    rt.block_on(async {
        let res = read_state.clone().oneshot(ReadRequest::BestChainBlockHash(height)).await;
        match res {
            Ok(ReadResponse::BlockHash(maybe_hash)) => maybe_hash,
            Err(err) => {
                tracing::error!("header fetch: {err:?}");
                None
            },
            _ => panic!("sync err: unhandled response: {res:?}"),
        }
    })
}

pub fn get_hdr_at_hash(read_state: &ReadState, rt: &tokio::runtime::Handle, hash: Hash) -> Option<(std::sync::Arc<block::Header>, Height, Hash)> {
    rt.block_on(async {
        let res = read_state.clone().oneshot(ReadRequest::BlockHeader(hash.into())).await;
        match res {
            Ok(ReadResponse::BlockHeader{ header, height, hash, .. }) => Some((header, height, hash)),
            Err(err) => {
                tracing::error!("header fetch: {err:?}");
                None
            },
            _ => panic!("header fetch: unhandled response: {res:?}"),
        }
    })
}

pub fn get_hdrs_after_hash(read_state: &ReadState, rt: &tokio::runtime::Handle, pre_first_hash: Hash, last_hash: Option<Hash>) -> Option<Vec<block::CountedHeader>> {
    rt.block_on(async {
        let res = read_state.clone().oneshot(ReadRequest::FindBlockHeaders{
            known_blocks: vec![pre_first_hash],
            stop: last_hash,
        }).await;
        match res {
            Ok(ReadResponse::BlockHeaders(hdrs)) => Some(hdrs),
            Err(err) => {
                tracing::error!("header fetch: {err:?}");
                None
            },
            _ => panic!("header fetch: unhandled response: {res:?}"),
        }
    })
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

    let mut near_tip_chains = NearTipChains { chains: Vec::new() };
    'init_near_tip_chains: {
        let near_tip_start_height = Height(tip_height.0.saturating_sub(NEAR_TIP_CHAIN_LEN+1));
        let Some(near_tip_start_hash) = get_bc_hash_at_height(&read_state, &rt, near_tip_start_height) else {
            break 'init_near_tip_chains;
        };
        let Some(near_tip_hdrs) = get_hdrs_after_hash(&read_state, &rt, near_tip_start_hash, None) else {
            break 'init_near_tip_chains;
        };

        let mut parent_hash = near_tip_start_hash;
        let mut shadow_blocks = Vec::with_capacity(near_tip_hdrs.len());
        for (i, hdr) in near_tip_hdrs.iter().enumerate() {
            let this_height = near_tip_start_height.0 + 1 + i as u32;
            // TODO: double-check height
            let block = ShadowBlock {
                parent_hash,
                this_hash: hdr.header.hash(),
                this_height,
            };
            parent_hash = block.this_hash;
            shadow_blocks.push(block);
        };

        near_tip_chains.push_blocks(&shadow_blocks);
    }

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
            if address.magic1 != CRYPTO_MAGIC {
                // @Dev
                panic!("The magic in the config toml - {} ({}) is different from the crypto magic - {} ({})! Modify one or the other!",
                        tenderlink::bandwidth_test::b64(&address.magic1.to_le_bytes()[..6]),
                        tenderlink::bandwidth_test::crypto_string_from_connect_magic1(address.magic1).unwrap_or("<invalid>"),
                        tenderlink::bandwidth_test::b64(&CRYPTO_MAGIC  .to_le_bytes()[..6]),
                        tenderlink::bandwidth_test::crypto_string_from_connect_magic1(CRYPTO_MAGIC).unwrap(),
                        );
            }
            println!("NewNet: Connecting to peer: {:?}", address);
            let _ = connect_to(socket, &mut connections_map, &my_keypairs, &address);
            peer_addresses.push(address);
        } else {
            eprintln!("NewNet: Failed to parse peer address: {}", peer_str);
        }
    }

    println!("NewNet: STP setup complete, port={}, peers={}", config.network_local_port, peer_addresses.len());

    // Sync state
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
                            // TODO: BlockEvents should contain enough info to insert a shadow block
                            // @Todo: announce this block
                            if let Some((hdr, height, hash)) = get_hdr_at_hash(&read_state, &rt, hash){
                                near_tip_chains.push_blocks(&[ShadowBlock {
                                    parent_hash: hdr.previous_block_hash,
                                    this_hash: hash,
                                    this_height: height.0,
                                }]);
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
        let mut buf = [0u8; 1 + 1024];
        let mut o = 0;
        o += PACKET_TYPE_STATUS.write_to(&mut buf[o..]);
        o += our_status        .write_to(&mut buf[o..]);
        // o += near_tip_chains.write_to(&mut buf[o..]); // @Phillip <<<<<<<<<<<<<<

        for (key, connection) in &connections_map {
            if !connection.is_connected() {
                continue;
            }

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
                let Some(status) = PacketStatus::read_from(&mut &msg[1..]) else { continue; }; // @Phillip <<<<<<<<<<<<<<

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

                let (hash, height) = (block.hash(), block.coinbase_height());
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
        let mut buf = [0u8, 1200];

        let mut chains = NearTipChains { chains: Vec::new() };
        let res = chains.write_packet_hash_tree(&mut buf);
        debug_assert_eq!(res, None);

        // (a, b), (b, c,), (c, d), (d, e), ..., (c, i), (i, j), ...
        // a b c d e f g h
        //     \ i j k l m n
        chains.push_blocks(blocks);

        let mut chains2 = NearTipChains { chains: Vec::new() };
        // (a, b), (b, c,), (c, d), (d, e), ..., (c, i), (i, j), ...
        // a b c d e f g h
        //     \ i j k l m n
        for block in &blocks {
            chains2.push_blocks(&[block]);
        }
        let tip_height = 11;

        debug_assert_eq!(chains, chains2, "building incrementally should be functionally equivalent to batch-built");
        debug_assert_eq!(chains.tip_height(), Some(11));


        let res = write_packet_hash_tree(&mut buf, tip_height, &blocks);
        debug_assert!(res.is_some(), "failed to write packet");

        println!("{}", hex::encode(buf));
    }
}

