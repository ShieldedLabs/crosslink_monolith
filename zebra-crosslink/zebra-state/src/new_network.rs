const DUMP_NEAR_TIP_CHAINS: bool = 1 == 0;

use std::collections::{HashMap, HashSet};
use static_assertions::const_assert;

use zebra_chain::block::{self, Block, Hash, Height};
use zebra_chain::serialization::{ZcashSerialize, ZcashDeserialize};

use tenderlink::bandwidth_test::*;
use tenderlink::native_sockets::*;
use tenderlink::parse_to_ipv6_bytes;
use tenderlink::{SliceWrite, SliceRead};
use tenderlink::{dbg_panic, dbg_verify};

#[derive(Clone, Copy, Debug)]
pub(crate) enum BlockEvent {
    Dequeued(ShadowBlock),
    Committed(ShadowBlock),
    TradFinalized(ShadowBlock),
    CrosslinkFinalized(ShadowBlock),
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

// @Todo: MTU discovery // @Duplicate with Tenderlink.
const UDP_mMTU:        usize = ASSUMED_SMALLEST_POSSIBLE_UDP_FRAME_WITH_GUARANTEED_DELIVERY;
const STP_HEADER_SIZE: usize = total_packet_payload_overhead_from_connect_magic1_inside_udp_payload(CRYPTO_MAGIC).unwrap();
const STP_PACKLET_HDR: usize = 2;
const STP_JUMBO_HDR:   usize = 8;
const PATH_MTU: usize = UDP_mMTU
                      - STP_HEADER_SIZE
                      - STP_PACKLET_HDR;
const JUMBO_FRAG_SIZE: usize = UDP_mMTU
                             - STP_HEADER_SIZE
                             - STP_PACKLET_HDR
                             - STP_JUMBO_HDR;

const CRYPTO_MAGIC: u64 = CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2s;
//const CRYPTO_MAGIC: u64 = CONNECT_MAGIC1_PLAIN_TEXT;

// @Todo: more of these, merge with Tenderlink, etc.
const PACKET_TYPE_STATUS:            u8 = 1;
const PACKET_TYPE_BLOCK_CHUNK:       u8 = 2;
const PACKET_TYPE_BLOCK_REQ:         u8 = 3;
const PACKET_TYPE_PEER_ADDRESS_LIST: u8 = 4;
const PACKET_TYPE_WANT_HOLE_PUNCH:   u8 = 5;
const PACKET_TYPE_TRY_HOLE_PUNCH:    u8 = 6;
const PACKET_TYPE_COUNT:             u8 = 7;

const PACKET_TYPE_NAMES: [&str; PACKET_TYPE_COUNT as usize] = {
    let mut names = ["<UNKNOWN>"; PACKET_TYPE_COUNT as usize];
    names[PACKET_TYPE_STATUS            as usize] = "STATUS";
    names[PACKET_TYPE_BLOCK_CHUNK       as usize] = "BLOCK_CHUNK";
    names[PACKET_TYPE_BLOCK_REQ         as usize] = "BLOCK_REQ";
    names[PACKET_TYPE_PEER_ADDRESS_LIST as usize] = "PEER_ADDRESS_LIST";
    names[PACKET_TYPE_WANT_HOLE_PUNCH   as usize] = "WANT_HOLE_PUNCH";
    names[PACKET_TYPE_TRY_HOLE_PUNCH    as usize] = "TRY_HOLE_PUNCH";
    const_assert!(PACKET_TYPE_COUNT == 7); // keep names array updated when adding other types
    names
};
fn packet_name_from_type(packet_type: u8) -> &'static str {
    let string_maybe = PACKET_TYPE_NAMES.get(packet_type as usize);
    let string       = string_maybe.unwrap_or(&"<UNKNOWN>");
    string
}


const PACKET_STATUS_MAX_HASHES: usize = 400; // @Lazy: gives room for 300 hashes plus room for 200 run metadatas
const PACKET_STATUS_MAX_SIZE:   usize = ((PACKET_STATUS_MAX_HASHES * 32 + JUMBO_FRAG_SIZE - 1) / JUMBO_FRAG_SIZE) * JUMBO_FRAG_SIZE; // @Cleanup @Lazy.

const DOWNLOAD_UNMODIFIED_TIMEOUT_DUR: std::time::Duration = std::time::Duration::from_secs(8);

#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PacketHashTreeHdr {
    pub tip_height: u32,
    pub finalized_height: u32,
    pub hashes_start_offset: u16, // we will never want >16384 branches in one status... that's a bushy tip
    // [PacketHashBranch]
    // [Hash] @ hashes_start_offset
}
impl SliceWrite for PacketHashTreeHdr {
    fn write_to(&self, buf: &mut [u8]) -> usize {
        let mut o = 0;
        o += self.tip_height.write_to(&mut buf[o..]);
        o += self.finalized_height.write_to(&mut buf[o..]);
        o += self.hashes_start_offset.write_to(&mut buf[o..]);
        o
    }
}
impl SliceRead for PacketHashTreeHdr {
    fn read_from(buf: &mut &[u8]) -> Option<Self> {
        Some(Self {
            tip_height:          dbg_verify(u32::read_from(buf))?,
            finalized_height:    dbg_verify(u32::read_from(buf))?,
            hashes_start_offset: dbg_verify(u16::read_from(buf))?,
        })
    }
}

#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PacketHashBranch {
    pub parent_hash_idx: u16,
    // start index is implicit from sequential cursor (the end index of the previous branch)
    pub branch_end_idx: u16,
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
            parent_hash_idx: dbg_verify(u16::read_from(buf))?,
            branch_end_idx:  dbg_verify(u16::read_from(buf))?,
        })
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ShadowBlock {
    pub this_hash:   Hash,
    pub parent_hash: Hash,
    pub this_height: u32,
    // TODO: work
}
impl ShadowBlock {
    fn work(&self) -> u128 {
        1 // @Hack, not @Prod
    }
}

impl Default for ShadowBlock {
    fn default() -> Self { Self { this_hash: Hash([0u8; 32]), parent_hash: Hash([0u8; 32]), this_height: 0 } }
}

const PRINT_H_1: usize = 1;
pub fn print_shadow_block_slice(indent_start_h: u32, blocks: &[ShadowBlock], bytes_n: usize) {
    const INCLUDE_PARENT: usize = 0;
    const INCLUDE_PARENT_1: usize = 1 & (1-INCLUDE_PARENT);
    const PRINT_H: usize = 0;
    const PARENS: usize = INCLUDE_PARENT | PRINT_H;

    if blocks.len() == 0 {
        return;
    }

    if PRINT_H_1 == 1 {
        eprint!("{:3}: ", blocks[0].this_height); // NOTE: NOT the parent height, even when that's prepended
    }

    {
        // indent for alignment
        let w_base   = bytes_n * 2 + ", ".len();
        let w_parent = INCLUDE_PARENT * (" ".len() + bytes_n * 2);
        let w_parens = PARENS * 2;
        let w_h      = PRINT_H * 4;
        let w_per    = w_base + w_parent + w_parens + w_h;
        let w = (blocks[0].this_height - indent_start_h) as usize * w_per;
        eprint!("{:w$}", "");
    }

    fn print_block(block: &ShadowBlock, bytes_n: usize) {
        if PARENS == 1 { eprint!("("); }

        if INCLUDE_PARENT == 1 {
            for byte in &block.parent_hash.0[..bytes_n] {
                eprint!("{:02x}", byte);
            }
            eprint!(" ");
        }

        for byte in &block.this_hash.0[..bytes_n] {
            eprint!("{:02x}", byte);
        }

        if PRINT_H == 1 {
            eprint!(" {:3}", block.this_height);
        }
        if PARENS == 1 { eprint!(")"); }
    }

    if INCLUDE_PARENT_1 == 1 {
        print_block(&ShadowBlock{ parent_hash: Hash([0;32]), this_hash: blocks[0].parent_hash, this_height: blocks[0].this_height.wrapping_sub(1)}, bytes_n);
        eprint!("| ");
    }

    for block in blocks {
        print_block(block, bytes_n);
        eprint!(", ");
    }
    eprintln!("");
}

pub fn print_hash_slice(indent_start_h: u32, h: u32, parent: Option<Hash>, hashes: &[Hash], bytes_n: usize) {
    if hashes.len() == 0 {
        return;
    }

    if PRINT_H_1 == 1 {
        eprint!("{:3}: ", h); // NOTE: NOT the parent height, even when that's prepended
    }

    {
        // indent for alignment
        let w_base   = bytes_n * 2 + ", ".len();
        let w_per    = w_base;
        let w = (h - indent_start_h) as usize * w_per;
        eprint!("{:w$}", "");
    }

    fn print_hash(hash: &Hash, bytes_n: usize) {
        for byte in &hash.0[..bytes_n] {
            eprint!("{:02x}", byte);
        }
    }

    if let Some(parent) = parent {
        print_hash(&parent, bytes_n);
        eprint!("| ");
    }

    for hash in hashes {
        print_hash(hash, bytes_n);
        eprint!(", ");
    }
    eprintln!("");
}

pub fn print_shadow_block_intersection(a: &[ShadowBlock], b: &[ShadowBlock], bytes_n: usize) {
    if a.len() == 0 || b.len() == 0 {
        eprintln!("empty slice: no intersection");
        return;
    }

    eprintln!("Intersection:");
    let min = a[0].this_height.min(b[0].this_height);
    let prefix = chain_intersect_prefix(a, b);
    print_shadow_block_slice(min, a, bytes_n);
    print_shadow_block_slice(min, prefix, bytes_n);
    print_shadow_block_slice(min, b, bytes_n);
}


const NEAR_TIP_CHAIN_LEN: u32 = 100;

#[derive(Debug, Default, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NearTipChain {
    pub work: u128,
    pub blocks: Vec<ShadowBlock>, // TODO: circular buffer tracking behind tip (N.B. tip not necessarily being longest means that buffers must have independent start points)
}
impl NearTipChain {
    pub fn push_block(&mut self, block: ShadowBlock) -> usize {
        while self.blocks.len() >= NEAR_TIP_CHAIN_LEN as usize {
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
/// (either on startup or after crosslink finalization).
///
/// :ReplicatingZebraState
/// There is a big @Todo here which is: don't replicate Zebra NonFinalizedState at all!
/// However, by design, our replica currently does not exactly overlap NonFinalizedState.
/// We ignore whether blocks are finalized or not when storing in the NearTipChains.
///
#[derive(Debug, Default, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NearTipChains {
    pub finalized_height: u32,
    pub chains: Vec<NearTipChain>, // @Todo: should we cap max chains tracked? (we could make everything fixed size!!)
}
impl NearTipChains {
    /// Height of *best* chain, which is probably, but not necessarily, the longest chain
    pub fn tip_height(&self) -> Option<u32> {
        let chain = self.chains.get(0)?;
        let block = chain.blocks.get(chain.blocks.len() - 1)?;
        Some(block.this_height)
    }

    pub fn min_packet_size() -> usize {
        let mut buf     = [0u8; 128];
        let mut hdr_len = PacketHashTreeHdr::default()            .write_to(&mut buf[..]);
        let mut run_len = PacketHashBranch ::default()            .write_to(&mut buf[..]);
        let mut hgt_len = ShadowBlock      ::default().this_height.write_to(&mut buf[..]);
        let     hsh_len = 32;
        let     min_len = hdr_len
                        + (run_len + hgt_len)
                        + (NEAR_TIP_CHAIN_LEN as usize + 1) * hsh_len;
        min_len
    }

    pub fn push_chain_unchecked(&mut self, blocks: Vec<ShadowBlock>) -> usize {
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
                    self.push_chain_unchecked(blocks[..parent_idx+1].to_vec())
                } else {
                    chain_idx
                }
            } else {
                self.push_chain_unchecked(Vec::new())
            };

            self.chains[chain_idx].push_block(*block);
        }

        self.chains.sort_by_key(|ch| std::cmp::Reverse(ch.work)); // ensure the new chain is BC if it should be

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

    fn remove_chains_invalidated_by_finalized(&mut self, final_block: &ShadowBlock) {
        self.finalized_height = self.finalized_height.max(final_block.this_height);
        self.chains.retain_mut(|chain| {
            debug_assert!(chain.blocks.len() > 0, "should have been removed if empty");
            let final_block_slice = [*final_block];
            let prefix = chain_intersect_prefix(&final_block_slice, &chain.blocks);

            if prefix.len() > 0 && prefix[0].this_hash == final_block.this_hash {
                // contains finalized block; all good
                return true;
            }

            if chain.blocks[0].this_height > final_block.this_height {
                // possibly long branch that clipped after finalized; too soon to tell
                // TODO: work out if finalized is ancestor; it will eventually get phased out anyway
                return true;
            }

            // finalization invalidates chain, remove it
            false
        });
    }


    /// 0 print_bytes => no print, otherwise it's the number of bytes to print from the hashes
    fn roundtrip_to_branches(&self, max_size: usize, hash_bytes_n: usize) {
        if hash_bytes_n > 0 {
            let mut min_h = u32::MAX;
            for chain in &self.chains {
                min_h = min_h.min(chain.blocks[0].this_height);
            }
            if DUMP_NEAR_TIP_CHAINS { eprintln!("{} chains:", self.chains.len()) };
            for chain in &self.chains {
                print_shadow_block_slice(min_h, &chain.blocks, hash_bytes_n);
            }
        }

        let buf = &mut [0u8; PACKET_STATUS_MAX_SIZE][..max_size];
        let res = self.write_to(buf, None);
        debug_assert!(res > 0, "failed to write packet");

        let branches = NearTipBranches::read_from(&mut &buf[..res]).unwrap();
        if hash_bytes_n > 0 {
            if DUMP_NEAR_TIP_CHAINS { eprintln!("\n{} branches:", branches.branches.len()); }
            branches.dump(hash_bytes_n);
        }
    }
}

impl NearTipChains {
    // currently assumes each block arrives after its parent
    fn write_to(&self, buf: &mut [u8], tip_height_override: Option<u32>) -> usize {
        { // Manually, at runtime, compute the min len for this function. Awful! Also, @Volatile. Fun.
            let min_len = NearTipChains::min_packet_size();
            assert!(buf.len() >= min_len, "NearTipChains::write_to() needs at least enough room for one best-chain run of {} hashes (i.e., >= {min_len} bytes).", NEAR_TIP_CHAIN_LEN + 1);
        }

        // doing parallel chains (redundantly keeping shared prefixes)
        // ALT: index tree in single buffer

        let tip_height = tip_height_override.unwrap_or(self.tip_height().expect("programmer error: should be non-empty"));
        let finalized_height = self.finalized_height;
        assert!(finalized_height <= tip_height);

        let mut runs = Vec::<(PacketHashBranch, u32, usize)>::new();
        let mut hashes = Vec::<Hash>::new();

        //         0 1 2 3 4 5 6 7 8 9 a b c d
        // hashes: a b c i j k l m n d e f g h
        // runs:   (0,9), (2, e)

        for chain in &self.chains {
            assert!(chain.blocks.len() > 0);

            let (parent_idx_into_hashes, base_idx_into_chain) = 'found: {
                // find the newest block with a parent that was already serialized
                for (chain_idx, block) in chain.blocks.iter().enumerate().rev() {
                    if let Some(i) = hashes.iter().rposition(|hash| *hash == block.parent_hash) {
                        break 'found (i, chain_idx);
                    }
                }

                // no blocks found with a parent contained in the serialized hashes; start a new tree
                hashes.push(chain.blocks[0].parent_hash);
                (hashes.len()-1, 0)
            };
            let start_idx = hashes.len();
            let base_height = chain.blocks[base_idx_into_chain].this_height;

            for chain_idx in base_idx_into_chain..chain.blocks.len() {
                hashes.push(chain.blocks[chain_idx].this_hash);
            }

            let branch = PacketHashBranch {
                parent_hash_idx: parent_idx_into_hashes.try_into().unwrap(),
                branch_end_idx: hashes.len().try_into().unwrap(),
            };
            runs.push((branch, base_height, start_idx));
        }

        if false { // @Dev @Debug
            let mut min_h = u32::MAX;
            for run in &runs {
                min_h = min_h.min(run.1);
            }
            for (i, run) in runs.iter().enumerate() {
                eprintln!("run {i}");
                print_hash_slice(min_h, run.1, Some(hashes[run.0.parent_hash_idx as usize]), &hashes[run.2..run.0.branch_end_idx as usize], 1);
            }
        }


        // packet size = sizeof(hdr + 2 runs) + 0xa hashes [0, 1]
        //                                     0 1 2 3 4 5 6 7 8 9 a b c d
        // tip, offset_of(a), (0,9), (2, 0xa), a b c i j k l m n d - - - -
        let mut hdr = PacketHashTreeHdr {
            tip_height,
            finalized_height,
            hashes_start_offset: 0, // fixed up
        };

        // TODO (perf): merge into loop above
        let mut o      = hdr.write_to(&mut buf[..]);
        let mut hash_c = 0usize;
        for (mut branch, height, _fork_idx) in &runs {
            let disconnected = (<usize>::from(branch.parent_hash_idx) == hash_c);

            let hashes_start_if_last = o + std::mem::size_of_val(&branch) + disconnected as usize * std::mem::size_of_val(&height);

            let run_bgn_if_last = hashes_start_if_last + (hash_c                         * 32);
            let run_end_if_last = hashes_start_if_last + (branch.branch_end_idx as usize * 32);

            if run_bgn_if_last + ((disconnected as usize + 1) * 32) > buf.len() {
                // wouldn't be able to fit even one more this_hash in, no point in starting another run
                break;
            }

            if run_end_if_last > buf.len() {
                let rem_size = buf.len() - run_bgn_if_last;
                let rem_hashes = rem_size / 32;
                branch.branch_end_idx = (hash_c + rem_hashes).try_into().unwrap();
                assert!(branch.branch_end_idx as usize <= hashes.len());
            }

            // write (parent, end)
            o += branch.write_to(&mut buf[o..]);
            if disconnected {
                // write height, so we will have written (parent, end, height)
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

#[derive(Debug, Default, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct NearTipBranches {
    tip_height: u32,
    finalized_height: u32,
    branches: Vec<Vec<ShadowBlock>>,
}
impl NearTipBranches {
    pub fn dump(&self, hash_bytes_n: usize) {
        if hash_bytes_n > 0 {
            let mut min_h = u32::MAX;
            for blocks in &self.branches {
                min_h = min_h.min(blocks[0].this_height);
            }
            for blocks in &self.branches {
                print_shadow_block_slice(min_h, blocks, hash_bytes_n);
            }
        }
    }
}
impl SliceRead for NearTipBranches {
    fn read_from(buf: &mut &[u8]) -> Option<Self> {
        let full_buf = *buf;
        let hdr = dbg_verify(PacketHashTreeHdr::read_from(buf))?;
        let hdr_len = full_buf.len() - buf.len(); // @Todo: better way to do this.
        *buf = &buf[..(hdr.hashes_start_offset as usize).saturating_sub(hdr_len)];
        let mut buf_hashes: &mut &[u8] = &mut &full_buf[hdr.hashes_start_offset as usize..];
        let mut branches: Vec<Vec<ShadowBlock>> = Vec::new();

        let mut hash_c = 0usize;
        while buf.len() > 0 {
            let (parent_i, end_i) = {
                let branch = dbg_verify(PacketHashBranch::read_from(buf))?;
                (branch.parent_hash_idx as usize, branch.branch_end_idx as usize)
            };
            if end_i.saturating_mul(32) > buf_hashes.len() {
                println!("received invalid Hash Tree packet (branch end)");
                dbg_panic!("received invalid Hash Tree packet (branch end)");
                return None;
            }
            if parent_i.saturating_add(1).saturating_mul(32) > buf_hashes.len() {
                println!("received invalid Hash Tree packet (parent hash)");
                dbg_panic!("received invalid Hash Tree packet (parent hash)");
                return None;
            }

            let mut parent_hash = Hash([0u8; 32]);
            parent_hash.0.copy_from_slice(&buf_hashes[parent_i * 32..(parent_i+1) * 32]);

            let bgn_height = if parent_i > hash_c {
                dbg_panic!("parent_i > hash_c, this is malformed! Parent_i: {parent_i}, hash_c: {hash_c}");
                return None; // deciding this is malformed for now...
            } else if parent_i == hash_c {
                hash_c += 1;
                dbg_verify(u32::read_from(buf))?
            } else {
                // Get branch height from previous hash point (height from beginning of run + offset).
                // @Todo(Perf) @Speed: store a Vec of [bgn_i, end_i) so we can binary search to find which branch parent_i is in.
                'find_branch: {
                    for branch in &branches {
                        for block in branch {
                            if block.parent_hash == parent_hash {
                                break 'find_branch block.this_height;
                            } else if block.this_hash == parent_hash {
                                break 'find_branch dbg_verify(block.this_height.checked_add(1))?;
                            }
                        }
                    }
                    dbg_panic!("Could not locate the parent in the branches list! Parent hash: {parent_hash}, hash_c: {hash_c}");
                    return None;
                }
            };

            if end_i <= hash_c { // zero-length branch or otherwise somehow precedes the current cursor
                dbg_panic!("Zero-length branch/somehow precedes current cursor! Hash_c: {hash_c}, end: {end_i}");
                return None;
            }

            let branch_hashes_n = end_i - hash_c;
            debug_assert!(branch_hashes_n < u32::MAX as usize, "more blocks in a branch than could be in a blockchain with 32-bit height values");

            let end_height = dbg_verify(bgn_height.checked_add((end_i - hash_c) as u32))?;

            let mut branch_blocks = Vec::with_capacity(branch_hashes_n);

            // We NEED to bounds check all the heights in this chain branch.
            // This means we can later increment heights by 1 without checking.
            let _ = dbg_verify(bgn_height.checked_add(dbg_verify(branch_hashes_n.try_into().ok())?))?;

            for i in hash_c..end_i {
                let mut this_hash = Hash([0u8; 32]);
                this_hash.0.copy_from_slice(&buf_hashes[i*32 .. (i+1)*32]);

                let this_height = bgn_height + (i - hash_c) as u32;
                branch_blocks.push(ShadowBlock { parent_hash, this_hash, this_height });
                parent_hash = this_hash;
            }

            hash_c = end_i;

            debug_assert!(branch_blocks.len() > 0, "should not have been able to be empty");
            branches.push(branch_blocks);
        }

        Some(Self { tip_height: hdr.tip_height, finalized_height: hdr.finalized_height, branches })
    }
}


fn max_shared_height_with_tree(their_tree: &NearTipBranches, blocks: &[ShadowBlock]) -> Option<u32> {
    if blocks.len() == 0 {
        dbg_panic!("0-length chain where there shouldn't be");
        return None;
    }

    let our_chain_height_end = blocks.last().unwrap().this_height + 1;

    let mut max_height_we_both_share = None;

    for their_branch in &their_tree.branches {
        let their_branch_height_bgn = their_branch[0].this_height;
        let their_branch_height_end = their_branch.last().unwrap().this_height + 1;

        let prefix = chain_intersect_prefix(&their_branch, blocks);
        // print_shadow_block_intersection(&their_branch, &our_chain.blocks, 1);


        // If the prefix was empty, there was no overlap.
        if prefix.is_empty() {
            continue;
        }

        let height_of_match = prefix.last().unwrap().this_height;

        assert!(height_of_match < our_chain_height_end);
        assert!(height_of_match < their_branch_height_end);

        max_height_we_both_share = max_height_we_both_share.max(Some(height_of_match));
    }

    max_height_we_both_share
}


// @Todo: @Test.
pub fn height_intersect(haystack: &[ShadowBlock], height_bgn: u32, height_end: u32) -> &[ShadowBlock] {
    if haystack.is_empty() {
        return &haystack[0..0];
    }

    let haystack_height_bgn = haystack[             0].this_height;
    let haystack_height_end = haystack.last().unwrap().this_height + 1;

    if (haystack_height_end - haystack_height_bgn) as usize != haystack.len() {
        #[cfg(debug_assertions)] panic!("Invariant violated: ShadowBlock chains are supposed to have one block per height");
        return &haystack[0..0];
    }

    let ol_bgn = haystack_height_bgn.max(height_bgn);
    let ol_end = haystack_height_end.min(height_end);
    if ol_bgn < ol_end {
        let i_bgn = (ol_bgn - haystack_height_bgn) as usize;
        let i_end = (ol_end - haystack_height_bgn) as usize;
        &haystack[i_bgn..i_end]
    } else {
        &haystack[0..0]
    }
}

// @Todo: we can extract a shared parent without having any intersection prefix!
pub fn chain_intersect_prefix<'l>(a: &'l [ShadowBlock], b: &'l [ShadowBlock]) -> &'l [ShadowBlock] {
    if a.is_empty() {
        return &a[0..0];
    }
    if b.is_empty() {
        return &a[0..0];
    }

    let a_height_bgn = a[             0].this_height;
    let a_height_end = a.last().unwrap().this_height + 1;
    let b_height_bgn = b[             0].this_height;
    let b_height_end = b.last().unwrap().this_height + 1;
    let a_ol = height_intersect(a, b_height_bgn, b_height_end);
    let b_ol = height_intersect(b, a_height_bgn, a_height_end);

    if a_ol.is_empty() {
        return &a[0..0];
    }
    if b_ol.is_empty() {
        return &a[0..0];
    }

    debug_assert!(a_ol.len() == b_ol.len(), "height_intersect on two chains should have the same length");

    let mut n = a_ol.len();
    for i in 0..n {
        if a_ol[i] != b_ol[i] { // NOTE: equality check includes parent hash
            n = i;
            break;
        }
    }

    &a_ol[..n]
}


const TRACE     :bool=0!=       0;


// @Todo: always only wait on real stuff, never sleeping for fixed amounts like this

// @Note: Status/block packet deliveries should NOT happen once per tick, because we don't sleep
//        when there are more packets to process in the current tick.
//        Let's disaggregate send from receive. And in fact, let's make them all happen at
//        coprime millisecond intervals, so we can make sure our code is not relying on
//        any kind of consistent or clean timing in order to function properly.

const STATUS_MS: u64 = 8000;  // fastest interval at which to send status messages
const BLOCK_SEND_MS: u64 = 1000; // fastest interval at which to send blocks to peers


const MAX_PEERS_TO_CONNECT_PER_ATTEMPT: usize = 2;
const PEER_CONNECT_MS: u64 = 6000;
const PEER_GOSSIP_MS:  u64 = 2500;
const PEER_REQUEST_MS: u64 = 400;
const PEER_RESPOND_MS: u64 = 400;


const IDLE_MS: u64 = 400;

const MAX_BANDWIDTH_BYTES_PER_MS: usize = 5_000; // 5 MB/s
const MAX_BANDWIDTH_BYTES_PER_RES: usize = MAX_BANDWIDTH_BYTES_PER_MS * PEER_RESPOND_MS as usize;
const MAX_BANDWIDTH_BLOCKS_PER_RES: usize = MAX_BANDWIDTH_BYTES_PER_RES / zebra_chain::block::MAX_BLOCK_BYTES as usize;


// use crate::crosslink::TFLServiceRequest;
// use crate::crosslink::TFLServiceResponse;
// use crate::crosslink::TFLServiceError;

type ReadState = crate::service::ReadStateService;
// type TFLService = tower::buffer::Buffer<tower::util::BoxService<TFLServiceRequest, TFLServiceResponse, TFLServiceError>, TFLServiceRequest>;

pub fn get_tips_blocking(read_state: &ReadState) -> ((Height, Hash), (Height, Hash)) {
    loop {
        let (Some(tip), Some(finalized_tip)) = (read_state.best_tip(), read_state.finalized_tip())
        else {
            std::thread::yield_now();
            continue;
        };
        break (tip, finalized_tip)
    }
}

pub fn is_parent_in_chains(read_state: &ReadState, near_tip_chains: &NearTipChains, parent_hash: Hash) -> bool {
    for our_chain in &near_tip_chains.chains {
        if our_chain.blocks.iter().any(|block| block.this_hash == parent_hash) {
            return true;
        }
    }

    // @Dev @Debug, but we would like to be able to quickly do this in production as well...
    match read_state.known_block(parent_hash) {
        None => false,
        Some(_block) => {
            // @Note: if we don't find the parent in near tip chains, but we ask Zebra and Zebra is aware of it, warn loudly.
            tracing::warn!("NewNet: Block hash {parent_hash} was NOT in near tip chains but contained by Zebra state!!");
            //dbg_panic!();
            true
        }
    }
}

use tenderlink::STP_ADDRESS_MEMORY_SIZE;
use tenderlink::STP_ADDRESS_SERIALIZED_SIZE;

pub const MAX_RECENT_BUCKETS:          usize = 2048;
pub const MAX_PEERS_PER_BUCKET: usize = 128;

pub const MAX_SENDER_BUCKETS:              usize = 1024;
pub const MAX_ADDRESSES_PER_SENDER: usize = 64;

pub const MAX_RECENT_ADDRESS_MAP_STORAGE_SIZE:  usize = MAX_RECENT_BUCKETS * MAX_PEERS_PER_BUCKET * (STP_ADDRESS_MEMORY_SIZE + RECENT_ADDRESS_SIZE);
pub const MAX_ALLEGED_ADDRESS_MAP_STORAGE_SIZE: usize = MAX_SENDER_BUCKETS * MAX_ADDRESSES_PER_SENDER * STP_ADDRESS_MEMORY_SIZE;

const ONE_MEGABYTE: usize = 1024 * 1024;

const_assert!(64 * ONE_MEGABYTE > MAX_RECENT_ADDRESS_MAP_STORAGE_SIZE + MAX_ALLEGED_ADDRESS_MAP_STORAGE_SIZE);

#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RecentPeerAddress {
    recv_time_ns: u64,
    failure_count: u32, // @Todo: Implement this counter!
}
const RECENT_ADDRESS_SIZE: usize =
    8 /* recv_time_ns */ +
    4 /* failure_count */;

// Bucket addresses by prefix as a crude proxy for ASN allocation, which is itself
// a proxy for a distinct operator, to increase the cost of an eclipse attack.
// @Todo: IP->ASN map support.
pub fn address_bucket(local_secret: u64, address: &STPAddress) -> usize {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&local_secret.to_le_bytes());

    let ip = u128::from_be_bytes(address.ip.octets());
    let prefix: u128 = if address.is_ipv4() { ip >>  16 }  // prefix range /16, bits ffffxxxx
                                       else { ip >> 104 }; // prefix range /24, bits 00xxxxxx
    hasher.update(&prefix.to_le_bytes());
    hasher.update(&address.magic1.to_le_bytes());
    let hash = hasher.finalize();

    usize::from_le_bytes(hash.as_bytes()[..8].try_into().unwrap())
}

#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PeerOrigin {
    #[default] Unsolicited, // unprompted or gossiped/holepunched;    more likely to be Sybil. AKA:  inbound connection.
    Selected,               // chosen from our address map diversely; less likely to be Sybil. AKA: outbound connection.
}

const MAX_BLOCKS_TO_QUEUE_TO_COMMIT: usize = 10; // DO NOT MAKE LARGE, subject to N^2!!!

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeightAndHashOr0 {
    height:    block::Height,
    hash_or_0: block::Hash,
}
impl Default for HeightAndHashOr0 {
    fn default() -> Self {
        Self {
            height: block::Height(0), // NOTE: nobody needs to request the genesis
            hash_or_0: block::Hash([0u8; 32]),
        }
    }
}
impl SliceRead for HeightAndHashOr0 {
    fn read_from(buf: &mut &[u8]) -> Option<Self> {
        Some(Self{
            height: block::Height(SliceRead::read_from(buf)?),
            hash_or_0: block::Hash(SliceRead::read_from(buf)?),
        })
    }
}
impl SliceWrite for HeightAndHashOr0 {
    fn write_to(&self, buf: &mut [u8]) -> usize {
        let mut o = 0;
        o += self.height.0.write_to(&mut buf[o..]);
        o += self.hash_or_0.0.write_to(&mut buf[o..]);
        o
    }
}


#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct PeerPowBlockRequest {
    height_hash: HeightAndHashOr0,
    offset: u32,
}
impl SliceRead for PeerPowBlockRequest {
    fn read_from(buf: &mut &[u8]) -> Option<Self> {
        Some(Self{
            height_hash: SliceRead::read_from(buf)?,
            offset: SliceRead::read_from(buf)?,
        })
    }
}
impl SliceWrite for PeerPowBlockRequest {
    fn write_to(&self, buf: &mut [u8]) -> usize {
        let mut o = 0;
        o += self.height_hash.write_to(&mut buf[o..]);
        o += self.offset.write_to(&mut buf[o..]);
        o
    }
}


#[derive(Debug, Clone, PartialEq, Eq)]
struct PeerPowBlockDownload {
    height_hash: HeightAndHashOr0,
    reassembly: ReassemblySlot, // accumulates block fragments across (re)requests
    last_modified: std::time::Instant,
}
impl Default for PeerPowBlockDownload {
    fn default() -> Self {
        Self {
            height_hash: HeightAndHashOr0::default(),
            reassembly: ReassemblySlot::new(),
            last_modified: std::time::Instant::now(),
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct PeerPowBlockResponseChunkHdr {
    // block_request_i: u8, // generated by requester
    offset: u32,
    // (simpler: always send this; hyper-fitted: only send on first)
    size: u32,
    height_hash: HeightAndHashOr0, // expects non-0 hash
} // followed by hdr? and then actual block data

impl SliceRead for PeerPowBlockResponseChunkHdr {
    fn read_from(buf: &mut &[u8]) -> Option<Self> {
        Some(Self {
            offset: SliceRead::read_from(buf)?,
            size: SliceRead::read_from(buf)?,
            height_hash: SliceRead::read_from(buf)?,
        })
    }
}
impl SliceWrite for PeerPowBlockResponseChunkHdr {
    fn write_to(&self, buf: &mut [u8]) -> usize {
        let mut o = 0;
        o += self.offset.write_to(&mut buf[o..]);
        o += self.size.write_to(&mut buf[o..]);
        o += self.height_hash.write_to(&mut buf[o..]);
        o
    }
}

const BLOCK_DOWNLOADS_N: usize = 8;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BlockDownloads {
    slots: [PeerPowBlockDownload; BLOCK_DOWNLOADS_N],
    used_flags: u64,
}
impl BlockDownloads {
    fn fill_slot(&mut self, dl_i: usize, height_hash: HeightAndHashOr0) -> Option<usize> {
        if dl_i < self.slots.len() {
            self.slots[dl_i] = PeerPowBlockDownload {
                height_hash,
                reassembly: ReassemblySlot::new(),
                last_modified: std::time::Instant::now(),
            };
            self.used_flags |= 1 << dl_i;
            Some(dl_i)
        } else {
            None
        }
    }


    fn insert(&mut self, height_hash: HeightAndHashOr0) -> Option<usize> {
        // if TRACE { tracing::info!("Inserting new download for height {} hash {}", height_hash.height.0, height_hash.hash_or_0); }
        self.fill_slot(self.used_flags.trailing_ones() as usize, height_hash)
    }

    /// NOTE: doesn't enforce height match if going by hash
    fn position(&self, height_hash: HeightAndHashOr0) -> Option<usize> {
        self.slots.iter().position(|dl| if dl.height_hash.hash_or_0 == block::Hash([0;32]) || height_hash.hash_or_0 == block::Hash([0;32]) {
            dl.height_hash.height == height_hash.height
        } else {
            dl.height_hash.hash_or_0 == height_hash.hash_or_0
        })
    }

    fn insert_or_position(&mut self, height_hash: HeightAndHashOr0) -> Option<usize> {
        self.position(height_hash).or_else(|| {
            self.insert(height_hash)
        })
    }

    fn remove(&mut self, dl_i: usize) -> PeerPowBlockDownload {
        let dl =  &mut self.slots[dl_i];
        let mut dl2 = PeerPowBlockDownload::default();

        std::mem::swap(&mut dl2, dl);

        self.used_flags &= !(1 << dl_i);
        self.slots[dl_i] = PeerPowBlockDownload::default();

        dl2
    }

    /// actual position if it's currently there, or where it would be inserted if it fits
    fn prospective_position_or_end(&self, height_hash: HeightAndHashOr0) -> usize {
        self.position(height_hash).unwrap_or_else(|| {
            self.used_flags.trailing_ones() as usize
        })
    }

    fn slot_is_used(&self, dl_i: usize) -> bool {
        dl_i < self.slots.len() && ((self.used_flags >> dl_i) & 1) != 0
    }

    fn used_count(&self) -> usize {
        self.used_flags.count_ones() as usize
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Peer {
    origin: PeerOrigin,
    their_tree: NearTipBranches,
    their_queue: HashSet<Hash>,

    // sending to peer
    // TODO: handle requests arriving out of order, minimising oversending
    block_requests: BlockDownloads,

    // receiving from peer
    // TODO: this is a poor substitute for proper reliable streams
    // TODO: also handle PoS
    // TODO: cross-peer tracking
    block_downloads: BlockDownloads,

    // Lite checkpointing (only meaningful while CheckpointState::Locked).
    // Whether this peer's main chain has been confirmed to contain the configured
    // checkpoint block: we requested the block at the checkpoint height and they
    // returned the expected hash.
    checkpoint_passed: bool,
    // When we last sent (or evaluated) a checkpoint test for this peer. Used to
    // re-test non-passing peers periodically in case their chain reorgs onto the
    // checkpoint.
    checkpoint_last_tested: Option<std::time::Instant>,
}

#[derive(Debug)]
pub enum BlockCommitError {
    Duplicate,
    Other(String),
}

/// Lite-checkpointing state machine, engaged when the operator configures
/// `config.network_checkpoint_block_hash`. When no checkpoint is configured we
/// stay in `Normal` and behave exactly as before.
///
/// ```text
///   SEARCH ──(obtained block from a peer)──► LOCKED ──(committed on best chain)──► NORMAL
///
///   Startup entry point:
///     - no checkpoint configured ............................ NORMAL
///     - configured, already committed locally .............. NORMAL
///     - configured, not yet committed locally .............. SEARCH
/// ```
///
/// - `Search`: we have the target hash but not the block. Freeze normal sync (do
///   not accept/commit any block), actively probe peers for the block *by hash*,
///   and watch for it locally. As soon as we obtain it (and thus learn its
///   height) → `Locked`. LOCKED is only ever reached from here.
/// - `Locked`: only request blocks from peers that pass the height test (asked
///   for the block at the checkpoint height, returned the expected hash). Non-passing
///   peers are skipped and periodically re-tested.
/// - `Normal`: no gating; behave as if no checkpoint was configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CheckpointState {
    Normal,
    Search { target: Hash },
    Locked { target: Hash, height: u32 },
}

/// Sentinel "height unknown" value used for SEARCH-mode by-hash probe downloads.
/// Peers serve blocks by hash regardless of the requested height, and no real
/// block download ever uses this height, so it doubles as a tag we can clean up.
const CHECKPOINT_SEARCH_HEIGHT: u32 = u32::MAX;
/// How often to re-test a peer that has not (yet) passed the checkpoint test.
const CHECKPOINT_RETEST_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

pub fn sync(
    commit_block: impl Fn(std::sync::Arc<Block>) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Hash, BlockCommitError>> + Send>>,
    config: &crate::config::Config,
    read_state: ReadState,
    // tfl_service: TFLService, // no TFLServiceHandle. Sadge!
    rt: tokio::runtime::Handle,
) {
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(500);
    BLOCK_EVENT_QUEUE_SENDER.set(event_tx).unwrap();

    let mut near_tip_chains = NearTipChains::default();
    // @Todo: @Refactor into a fn we can call to flush and reset the NearTipChains state.
    'init_near_tip_chains: {
        let ((tip_height, tip_hash), (finalized_tip_height, finalized_tip_hash)) = get_tips_blocking(&read_state);
        tracing::info!("NewNet: Starting at height={} hash={:?} finalized_height={} finalized_hash={}", tip_height.0, tip_hash, finalized_tip_height.0, finalized_tip_hash);

        near_tip_chains.finalized_height = finalized_tip_height.0;

        assert!(near_tip_chains.finalized_height <= tip_height.0);

        // let res = rt.block_on(async { tfl_service.clone().oneshot(Request::Get).await });
        // if let Some((height, hash)) = match res {
        //     Ok(TFLServiceResponse::FinalBlockHeightHash(height_and_hash)) => height_and_hash,
        //     Err(err) => {
        //         tracing::error!("FinalBlockHeightHash(): Error: {err:?}");
        //         None
        //     }
        //     _ => panic!("FinalBlockHeightHash(): Unhandled response: {res:?}"),
        // } {
        //     near_tip_chains.finalized_height_crosslink = height.0;
        // }

        // Push genesis into near_tip_chains for two reasons:
        // - Prevents NearTipChains serialization asserts on new nodes
        // - Allows early nodes to overlap with and push to new nodes
        // This can be undone once fartipchain sync is ready.
        near_tip_chains.push_blocks(&[ShadowBlock { this_hash: read_state.best_chain_block_hash(Height(0)).expect("failed to get genesis block"), ..ShadowBlock::default() }]);

        let near_tip_start_height = Height(tip_height.0.saturating_sub(NEAR_TIP_CHAIN_LEN+1));
        let Some(near_tip_start_hash) = read_state.best_chain_block_hash(near_tip_start_height) else {
            break 'init_near_tip_chains;
        };
        let near_tip_hdrs = read_state.find_block_headers(vec![near_tip_start_hash], None);

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

        assert!(near_tip_chains.finalized_height <= near_tip_chains.tip_height().unwrap()); // :AssumeGenesisBlockIncludedInNearTipChains
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
    tracing::info!("NewNet: KEYPAIR: {}", network_keypair);

    // STP networking setup
    socket_setup();
    monotonic_clock_setup();

    let actual_network_local_port : u16 = {
        if config.network_local_port == 0 {
            use rand::RngCore;
            rand::thread_rng().next_u32() as u16 % 45869 + 2000
        } else {
            config.network_local_port
        }
    };

    let my_keypairs = vec![network_keypair.clone()];
    let network_thread_handle = new_network_thread(my_keypairs.clone(), actual_network_local_port, None, (1_000_000, 256 * 1024 * 1024, 256 * 1024 * 1024));
    tracing::info!("NewNet: Bound to port {}", actual_network_local_port);

    let mut current_connections = Vec::<(STPAddress, [u8; 64])>::new();
    let mut initiate_connections = Vec::<STPAddress>::new();
    let mut packets_to_send: Vec<(ConnectionKey, Vec<u8>)> = Vec::new();

    // Parse and connect to initial peers
    let mut initial_peer_addresses: Vec<STPAddress> = Vec::new();
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
            tracing::info!("NewNet: Connecting to peer: {:?}", address);
            initiate_connections.push(address.clone());
            initial_peer_addresses.push(address);
        } else {
            tracing::warn!("NewNet: Failed to parse peer address: {}", peer_str);
        }
    }

    tracing::info!("NewNet: STP setup complete, port={}, peers={}", actual_network_local_port, initial_peer_addresses.len());

    // Sync state
    let tick_duration = std::time::Duration::from_millis(IDLE_MS);
    let status_interval = std::time::Duration::from_millis(STATUS_MS);
    let dl_init_interval = std::time::Duration::from_millis(BLOCK_SEND_MS);
    let peer_gossip_interval = std::time::Duration::from_millis(PEER_GOSSIP_MS);
    let peer_connect_interval = std::time::Duration::from_millis(PEER_CONNECT_MS);
    let peer_request_interval = std::time::Duration::from_millis(PEER_REQUEST_MS);

    let mut blocks_to_commit:  Vec<(Hash, std::sync::Arc<Block>)>     = Vec::new();
    let mut blocks_to_send:    Vec<(ConnectionKey, Hash, u32, usize)> = Vec::new();
    let mut serialized_blocks: HashMap<Hash, Vec<u8>>             = HashMap::new(); // @Todo: cap max memory storage size for this map.

    use rand::Rng;
    let mut local_addresses_secret: u64 = rand::thread_rng().gen();

    let mut recent_peer_addresses:  HashMap<u16, HashMap<STPAddress, RecentPeerAddress>> = HashMap::new();
    let mut alleged_peer_addresses: HashMap<u16, HashMap<STPAddress, ConnectionKey>> = HashMap::new(); // keyed by sender, not address. connection key of latest sender is stored so we know who to ask to initiate UDP hole punch

    let mut peers: HashMap<ConnectionKey, Peer> = HashMap::new();
    let mut pending_selected_addresses: HashMap<ConnectionKey, std::time::Instant> = HashMap::new(); // store time for expiry

    let mut XXX_tick_loop_counter = 0usize;

    let mut next_status       = std::time::Instant::now();
    let mut next_dl_init      = std::time::Instant::now();
    let mut next_peer_gossip  = std::time::Instant::now();
    let mut next_peer_connect = std::time::Instant::now();
    let mut next_peer_request = std::time::Instant::now();
    let mut next_console_status_print = std::time::Instant::now();

    // Lite checkpointing. See CheckpointState.
    let checkpoint_target: Option<Hash> = match &config.network_checkpoint_block_hash {
        None => None,
        // "" is the explicit opt-out: with a default checkpoint baked into the
        // config, omitting the field no longer means "no checkpoint".
        Some(s) if s.is_empty() => None,
        Some(s) => match s.parse::<Hash>() {
            Ok(hash) => Some(hash),
            Err(err) => {
                tracing::error!("NewNet: Failed to parse network_checkpoint_block_hash {s:?}: {err:?}. Ignoring checkpoint.");
                None
            }
        },
    };
    let mut checkpoint_state = match checkpoint_target {
        None => CheckpointState::Normal,
        Some(target) => {
            // If the checkpoint is already committed on our best chain, there's nothing to do.
            let already_committed_locally = read_state.block_header(target.into())
                .map(|(_, height, _, _)| read_state.best_chain_block_hash(height) == Some(target))
                .unwrap_or(false);
            if already_committed_locally {
                tracing::info!("NewNet: Checkpoint {target} already committed locally; checkpointing satisfied (NORMAL).");
                CheckpointState::Normal
            } else {
                tracing::info!("NewNet: Checkpoint {target} configured but not committed locally; entering SEARCH.");
                CheckpointState::Search { target }
            }
        }
    };
    let checkpoint_check_interval = std::time::Duration::from_millis(1000);
    let mut next_checkpoint_check = std::time::Instant::now() + checkpoint_check_interval;

    let stp_address_get_short_string = |address| {
        let addr = format!("{:?}", address);
        let short = &addr[addr.len().saturating_sub(6)..]; // 6 base64 chars -> 4 bytes -> probably unique up to 65536 connections
        short.to_string()
    };

    // Main sync loop
    loop {
        let loop_start = std::time::Instant::now();

        if loop_start > next_console_status_print {
            let mut my_peers_to_print = Vec::new();
            for (address, _) in &current_connections {
                let key = address.connection_key();
                if let Some(peer) = peers.get(&key) {
                    my_peers_to_print.push(format!("{}@{}/{}", stp_address_get_short_string(address.clone()), peer.their_tree.finalized_height, peer.their_tree.tip_height));
                }
            }

            tracing::info!("tip height: {:?}, finalized height: {:?}, peers: {:?}", near_tip_chains.tip_height(), near_tip_chains.finalized_height, my_peers_to_print);
            next_console_status_print = loop_start + std::time::Duration::from_secs(2);
        }

        // Drain block events
        loop {
            match event_rx.try_recv() {
                Ok(block_event) => {
                    match block_event {
                        BlockEvent::Committed(block) => {
                            near_tip_chains.push_blocks(&[block]);
                        }
                        BlockEvent::TradFinalized(block) | BlockEvent::CrosslinkFinalized(block) => {
                            near_tip_chains.remove_chains_invalidated_by_finalized(&block);
                        }

                        _ => {
                            tracing::info!("NewNet: block_event: {:?}", block_event);
                        }
                    }
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                Err(err) => { tracing::error!("{err:?}"); break; },
            }
        }

        #[cfg(debug_assertions)]
        if false { // @Dev @Debug: Roundtrip test.
            near_tip_chains.roundtrip_to_branches(PACKET_STATUS_MAX_SIZE, if XXX_tick_loop_counter % 20 == 0 { 2 } else { 0 });
            XXX_tick_loop_counter += 1;
        }

        // Try to reconnect to trusted initial seed peers
        for address in &initial_peer_addresses {
            if !current_connections.iter().any(|(addr, _)| addr.connection_key() == address.connection_key()) {
                initiate_connections.push(address.clone());
            }
        }

        let now = monotonic_clock_ns();
        const ONE_SECOND: u64 = 1_000_000_000;
        const ONE_MINUTE: u64 = ONE_SECOND * 60;
        const ONE_HOUR:   u64 = ONE_MINUTE * 60;

        // evict old recent addresses
        for (_, map) in &mut recent_peer_addresses {
            map.retain(|addr, recent_peer_address| {
                if initial_peer_addresses.contains(addr) {
                    return false;
                }

                recent_peer_address.recv_time_ns + 10 * ONE_MINUTE >= now
            });
        }
        recent_peer_addresses .retain(|_, map| map.len() > 0);

        for (_, map) in alleged_peer_addresses.iter_mut() {
            map.retain(|addr, _| {
                if initial_peer_addresses.contains(addr) {
                    return false;
                }

                let addr_bucket = address_bucket(local_addresses_secret, addr) & (MAX_RECENT_BUCKETS - 1);
                if let Some(recents) = recent_peer_addresses.get(&(addr_bucket as u16)) {
                    !recents.contains_key(addr)
                } else {
                    true
                }
            });
        }
        alleged_peer_addresses.retain(|_, map| map.len() > 0);

        use rand::seq::IteratorRandom;

        if std::time::Instant::now() >= next_peer_connect {
            let mut connection_attempts = 0;

            let rng = &mut rand::thread_rng();

            const PEERS_TO_ASK_PUNCH_FOR_RECENTS:  usize = 5;
            const PEERS_TO_ASK_PUNCH_FOR_ALLEGEDS: usize = 2;

            let mut recents: Vec<(&u16, &HashMap<STPAddress, RecentPeerAddress>)> = recent_peer_addresses.iter().collect(); recents.shuffle(rng);
            for (_, map) in &recents {
                if connection_attempts >= MAX_PEERS_TO_CONNECT_PER_ATTEMPT {
                    break;
                }
                let Some((address, _)) = map.iter().choose(rng) else {
                    continue;
                };
                if current_connections.iter().any(|(addr, _)| addr.connection_key() == address.connection_key()) {
                    continue;
                }

                initiate_connections.push(address.clone());

                connection_attempts += 1;

                pending_selected_addresses.insert(address.connection_key(), std::time::Instant::now());

                // Hole punch
                let (mut buf, mut o) = ([0u8; 1 + STP_ADDRESS_SERIALIZED_SIZE], 0);
                o += PACKET_TYPE_WANT_HOLE_PUNCH.write_to(&mut buf[o..]);
                o += address.connection_key()   .write_to(&mut buf[o..]);

                for (addr, _) in current_connections.iter().choose_multiple(rng, PEERS_TO_ASK_PUNCH_FOR_RECENTS) {
                    if TRACE { tracing::info!("Requesting hole punch to recent address {:?} via random peer: {:?}...", address, addr); }
                    packets_to_send.push((addr.connection_key(), Vec::from(&buf[..o])));
                }
            }

            let mut allegeds: Vec<_> = alleged_peer_addresses.iter().collect(); allegeds.shuffle(rng);
            for (_, map) in allegeds {
                if connection_attempts >= MAX_PEERS_TO_CONNECT_PER_ATTEMPT {
                    break;
                }
                let Some((address, sender_connection_key)) = map.iter().choose(rng) else {
                    continue;
                };
                if current_connections.iter().any(|(addr, _)| addr.connection_key() == address.connection_key()) {
                    continue;
                }

                initiate_connections.push(address.clone());

                connection_attempts += 1;

                // Hole punch
                let (mut buf, mut o) = ([0u8; 1 + STP_ADDRESS_SERIALIZED_SIZE], 0);
                o += PACKET_TYPE_WANT_HOLE_PUNCH.write_to(&mut buf[o..]);
                o += address.connection_key()   .write_to(&mut buf[o..]);

                for (addr, _) in current_connections.iter().filter(|(addr, _)| addr.connection_key() != *sender_connection_key).choose_multiple(rng, PEERS_TO_ASK_PUNCH_FOR_ALLEGEDS) {
                    if TRACE { tracing::info!("Requesting hole punch to alleged address {:?} via random peer: {:?}...", address, addr); }
                    packets_to_send.push((addr.connection_key(), Vec::from(&buf[..o])));
                }

                if current_connections.iter().any(|(addr, _)| addr.connection_key() == *sender_connection_key) {
                    if TRACE { tracing::info!("Requesting hole punch to alleged address {:?} via the original sender: {:?}...", address, sender_connection_key); }
                    packets_to_send.push((*sender_connection_key, Vec::from(&buf[..o])));
                }
            }

            next_peer_connect = std::time::Instant::now() + peer_connect_interval;
        }

        if std::time::Instant::now() >= next_peer_gossip {

            let rng = &mut rand::thread_rng();

            let recent_addrs_n: usize = recent_peer_addresses.values().map(|m| m.len()).sum();
            let pckt_addrs_n = ((JUMBO_FRAG_SIZE - 1) / STP_ADDRESS_SERIALIZED_SIZE).min(recent_addrs_n);

            use rand::seq::IteratorRandom;

            let mut bucket_keys: Vec<_> = recent_peer_addresses.keys().collect(); bucket_keys.shuffle(rng);

            let mut pckt_addrs = HashSet::new();

            loop {
                let prev = pckt_addrs.len();
                for key in &bucket_keys {
                    if pckt_addrs.len() >= pckt_addrs_n {
                        break;
                    }

                    let bucket = &recent_peer_addresses[*key];
                    if let Some(addr) = bucket.keys().filter(|a| !pckt_addrs.contains(*a)).choose(rng) {
                        pckt_addrs.insert(addr.clone());
                    }
                }
                if pckt_addrs.len() >= pckt_addrs_n || pckt_addrs.len() <= prev {
                    break;
                }
            }

            let (mut buf, mut o) = ([0u8; JUMBO_FRAG_SIZE], 0); // ensure no fragmentation
            o += PACKET_TYPE_PEER_ADDRESS_LIST.write_to(&mut buf[o..]);
            for address in pckt_addrs {
                o += address.write_to(&mut buf[o..]);
            }

            let gossip_packet = Vec::from(&buf[..o]);
            for (addr, _) in &current_connections {
                packets_to_send.push((addr.connection_key(), gossip_packet.clone()));
            }

            next_peer_gossip = std::time::Instant::now() + peer_gossip_interval;
        }


        // TODO: replace with a later block_packets_to_send, which should be sorted by (in-block offset, height)
        blocks_to_send.sort_by_key(|(_, _, height, _)| *height);

        // TODO: rate limit production!
        for (connection_key, hash, height, offset) in &blocks_to_send {
            let hash = *hash;
            if !current_connections.iter().any(|(addr, _)| addr.connection_key() == *connection_key) {
                if TRACE { tracing::info!("Can't send block that was queued for sending: Peer was disconnected: {connection_key:?}"); }
                continue;
            }
            let Some(peer) = peers.get_mut(connection_key) else {
                if TRACE { tracing::info!("Can't send block that was queued for sending: Peer does not exist for connection: {connection_key:?}"); }
                continue;
            };

            if !serialized_blocks.contains_key(&hash) {
                match read_state.block_from_any_chain(hash.into()) {
                    Some(block) => {
                        let serialized = dbg_verify(block.zcash_serialize_to_vec().ok()).unwrap();
                        assert!(serialized.len() as u64 <= zebra_chain::block::MAX_BLOCK_BYTES, "Internal block is too big! {} B", serialized.len());
                        serialized_blocks.insert(hash, serialized);
                    }
                    None => {
                        tracing::warn!("NewNet: Couldn't get block for hash {hash}!");
                        continue;
                    }
                }
            }

            let serialized_block = &serialized_blocks[&hash];

            if *offset >= serialized_block.len() {
                if TRACE { tracing::info!("Ignoring out-of-range block offset request: {offset} >= {}", serialized_block.len()); }
                continue;
            }

            let total_size: u32 = serialized_block.len().try_into().unwrap();

            if TRACE { tracing::info!("Sending BLOCK (Height: {height}, Hash: {hash}, {offset}-{})", serialized_block.len()); }

            // Poor-man's fragmentation: split the block into single-packet chunks, each an
            // independently deliverable & re-requestable message. STP delivery is unreliable,
            // so the receiver reassembles and re-requests from its highest contiguous prefix.
            let mut pkt_buf = [0u8; JUMBO_FRAG_SIZE];

            // payload bytes available per packet after the chunk header
            let chunk_size = JUMBO_FRAG_SIZE - {
                let hdr = PeerPowBlockResponseChunkHdr {
                    offset: 0,
                    size: total_size,
                    height_hash: HeightAndHashOr0 { height: block::Height(*height), hash_or_0: hash },
                };
                let mut o = 0;
                o += PACKET_TYPE_BLOCK_CHUNK.write_to(&mut pkt_buf[o..]);
                o += hdr.write_to(&mut pkt_buf[o..]);
                o
            };

            for (chunk_i, chunk) in serialized_block[*offset..].chunks(chunk_size).enumerate() {
                let hdr = PeerPowBlockResponseChunkHdr {
                    offset: *offset as u32 + (chunk_i * chunk_size) as u32,
                    size: total_size,
                    height_hash: HeightAndHashOr0 {
                        height: block::Height(*height),
                        hash_or_0: hash,
                    }
                };

                let mut o = 0;
                o += PACKET_TYPE_BLOCK_CHUNK.write_to(&mut pkt_buf[o..]);
                o += hdr.write_to(&mut pkt_buf[o..]);
                o += chunk.write_to(&mut pkt_buf[o..]);

                packets_to_send.push((*connection_key, Vec::from(&pkt_buf[..o])));
            }
        }
        blocks_to_send.clear();

        // Invariant: near_tip_chains contains at least the genesis. Currently.
        assert!(near_tip_chains.tip_height().is_some());
        if std::time::Instant::now() >= next_status {

            assert!(blocks_to_commit.len() < MAX_BLOCKS_TO_QUEUE_TO_COMMIT);
            let queue_len = blocks_to_commit.len().min(MAX_BLOCKS_TO_QUEUE_TO_COMMIT) as u8;

            let (mut buf, mut o) = ([0u8; 1 + 1 + MAX_BLOCKS_TO_QUEUE_TO_COMMIT * 32 + PACKET_STATUS_MAX_SIZE], 0);
            o += PACKET_TYPE_STATUS.write_to(&mut buf[o..]);
            o += queue_len         .write_to(&mut buf[o..]);
            for (queued_hash, _) in &blocks_to_commit {
                o += queued_hash.0.write_to(&mut buf[o..]);
            }
            o += near_tip_chains   .write_to(&mut buf[o..], None);

            for (address, _) in &current_connections {
                let key = address.connection_key();

                let Some(peer) = peers.get(&key) else {
                    if TRACE { tracing::info!("Trying to send a STATUS to {:?} but they haven't finished being created", address); }
                    continue;
                };

                let mut send_tip_chains = near_tip_chains.finalized_height < peer.their_tree.finalized_height;
                if ! send_tip_chains {
                    for our_chain in &near_tip_chains.chains {
                        if max_shared_height_with_tree(&peer.their_tree, &our_chain.blocks).is_some() {
                            send_tip_chains = true;
                            break;
                        }
                    }
                }

                if ! send_tip_chains {
                    send_tip_chains = 'try_send_historical: {
                        // they are very far behind us: construct & send them a historical status

                        // subtract once to get the finalized block at their height to confirm the attach point (because our chain_intersect_prefix() does not match parents),
                        // subtract again because in order to send them that block we need to get that block's parent.
                        let their_final_parent_parent_height = peer.their_tree.finalized_height.saturating_sub(2);
                        let Some(their_final_parent_parent_hash) = read_state.best_chain_block_hash(block::Height(their_final_parent_parent_height)) else {
                            if TRACE { tracing::info!("Can't send a historical STATUS to {:?} as we don't have blocks @ {}", address, their_final_parent_parent_height); }
                            break 'try_send_historical true; // fallback to tip chains if we don't successfully send historical here
                        };

                        let mut hashes_from_their_final_parent = read_state.find_block_hashes(vec![their_final_parent_parent_hash], None);
                        if hashes_from_their_final_parent.len() == 0 {
                            if TRACE { tracing::info!("Can't send a historical STATUS to {:?} as we found 0 hashes after block @ {}, {}", address, their_final_parent_parent_height, their_final_parent_parent_hash); }
                            break 'try_send_historical true;
                        }

                        let mut parent_hash = their_final_parent_parent_hash;
                        let mut this_height = their_final_parent_parent_height + 1;
                        let mut historical_chain: Vec<ShadowBlock> = Vec::new();

                        // if the first block we send is the block-after-genesis,
                        // we need to manually include genesis.
                        if this_height <= 1 {
                            historical_chain.push(ShadowBlock {
                                parent_hash: block::Hash([0;32]),
                                this_height: 0,
                                this_hash:   parent_hash,
                            });
                        }

                        for &this_hash in &hashes_from_their_final_parent {
                            historical_chain.push(ShadowBlock {
                                parent_hash,
                                this_height,
                                this_hash,
                            });
                            parent_hash = this_hash;
                            this_height += 1;
                        }

                        if let Some(h) = max_shared_height_with_tree(&peer.their_tree, &historical_chain) {
                            let h_d = h.saturating_sub(historical_chain[0].this_height).saturating_sub(2);
                            historical_chain.drain(..h_d as usize);
                        }

                        historical_chain.truncate(NEAR_TIP_CHAIN_LEN as usize);
                        let mut historical_chains = NearTipChains::default();
                        historical_chains.finalized_height = near_tip_chains.finalized_height;
                        historical_chains.push_chain_unchecked(historical_chain);

                        let (mut buf, mut o) = ([0u8; 1 + 1 + MAX_BLOCKS_TO_QUEUE_TO_COMMIT * 32 + PACKET_STATUS_MAX_SIZE], 0);
                        o += PACKET_TYPE_STATUS.write_to(&mut buf[o..]);
                        o += queue_len         .write_to(&mut buf[o..]);
                        for (queued_hash, _) in &blocks_to_commit {
                            o += queued_hash.0 .write_to(&mut buf[o..]);
                        }
                        o += historical_chains .write_to(&mut buf[o..], Some(near_tip_chains.tip_height().expect("at least genesis block assumed included in near tip chains"))); // @TODO: this will break if we remove genesis...


                        // if TRACE { tracing::info!("Send a historical STATUS to {:?} @ {}", address, their_final_parent_parent_height + 1); }
                        packets_to_send.push((key, Vec::from(&buf[..o])));
                        false
                    };
                }

                if send_tip_chains {
                    packets_to_send.push((key, Vec::from(&buf[..o])));
                }
            }

            next_status = std::time::Instant::now() + status_interval;
        }

        // Discard statuses of disconnected peers
        peers.retain(|connection_key, _| current_connections.iter().any(|(addr, _)| addr.connection_key() == *connection_key));

        if std::time::Instant::now() >= next_dl_init {
            let mut active_block_dls = 0;
            let mut requests_by_hash: HashMap<Hash, usize> = HashMap::new();
            let mut requests_by_height: HashMap<u32, usize> = HashMap::new();
            const MAX_REQUEST_DUPLICATES_N: usize = 2; // TODO: reconsider @Prod
            for (_, peer) in &mut peers {
                for dl_i in 0..peer.block_downloads.slots.len() {
                    if peer.block_downloads.slot_is_used(dl_i) {
                        let HeightAndHashOr0 { height: block::Height(height), hash_or_0 } = peer.block_downloads.slots[dl_i].height_hash;

                        // Drop leftover SEARCH-mode by-hash probes once we've left SEARCH.
                        if height == CHECKPOINT_SEARCH_HEIGHT
                            && !matches!(checkpoint_state, CheckpointState::Search { .. }) {
                            peer.block_downloads.remove(dl_i);

                        } else if height <= near_tip_chains.finalized_height {
                            if TRACE { tracing::info!("Cancelling download request for block @ {}, {} - <= finalized @ {}", height, hash_or_0, near_tip_chains.finalized_height); }
                            peer.block_downloads.remove(dl_i);

                        } else if peer.block_downloads.slots[dl_i].last_modified.elapsed() > DOWNLOAD_UNMODIFIED_TIMEOUT_DUR {
                            if TRACE { tracing::info!("Cancelling download request for block @ {}, {} - timed out", height, hash_or_0); }
                            peer.block_downloads.remove(dl_i);
                            // TODO: lower weighting on this peer?

                        } else if hash_or_0 != block::Hash([0; 32]) {
                            requests_by_hash.entry(hash_or_0).and_modify(|c| *c += 1).or_insert(1);
                            // A SEARCH by-hash probe (sentinel height) is bookkeeping, not real
                            // block sync, so it must not consume the download budget.
                            if height != CHECKPOINT_SEARCH_HEIGHT { active_block_dls += 1; }

                        } else {
                            requests_by_height.entry(height).and_modify(|c| *c += 1).or_insert(1);
                            // A pending LOCKED checkpoint test is a by-height (hash-0) probe. Peers
                            // below the checkpoint height never answer it, so it churns until timeout;
                            // counting it would let such probes exhaust MAX_BANDWIDTH_BLOCKS_PER_RES
                            // and starve real downloads from peers that DID pass. Don't count it.
                            // (Once a peer passes, the slot's hash is filled in and it lands in the
                            // branch above as a real download.) @Volatile: assumes real requests are
                            // always by-hash; the historical by-height path is currently disabled.
                            let is_locked_probe = matches!(checkpoint_state,
                                CheckpointState::Locked { height: cp, .. } if cp == height);
                            if !is_locked_probe { active_block_dls += 1; }
                        }
                    }
                }
            }
            let prev_active_block_dls = active_block_dls;

            // TODO: weight peers by observed quality
            const MAX_PEERS_TO_INIT_DLS_FROM: usize = 8;

            /*  Note(Sam): CRITICAL BUG THAT I AM ADDRESSING NOW. We cannot randomly select a fixed number of
                peers. We must first filter the peers by who has something we can download. Otherwise we will
                fail in the case where most of the peers are behind us.
            */

            let mut peer_random_keys: Vec<ConnectionKey> = peers.keys().cloned().collect();
            // shuffle in-place
            peer_random_keys.shuffle(&mut rand::thread_rng());

            let mut count_of_peers_we_started_download_from = 0;
            'send_to_peers: for connection_key in peer_random_keys {
                let Peer { origin, their_tree, their_queue, ref mut block_downloads, checkpoint_passed, checkpoint_last_tested, .. } = peers.get_mut(&connection_key).unwrap();
                if count_of_peers_we_started_download_from >= MAX_PEERS_TO_INIT_DLS_FROM { break 'send_to_peers; }
                let active_block_dls_before_this_peer = active_block_dls; // to detect if we start any dl for this peer


                if *their_tree == NearTipBranches::default() {
                    continue 'send_to_peers; // No messages yet.
                }

                let mut blocks_to_this_peer = their_queue.len();
                // Skip now-disconnected peers
                let connection_address = {
                    let Some((addr, _)) = current_connections.iter().find(|(addr, _)| addr.connection_key() == connection_key) else {
                        tracing::error!("NewNet: Peer is disconnected even after we filtered out disconnected peers. Impossible!: {connection_key:?}");
                        continue 'send_to_peers;
                    };
                    addr.clone()
                };

                // Lite checkpointing: gate / redirect what we request from this peer.
                match checkpoint_state {
                    CheckpointState::Search { target } => {
                        // Freeze normal sync; only probe this peer for the checkpoint block by hash
                        // (height unknown until we obtain it, hence the sentinel height).
                        let probe = HeightAndHashOr0 {
                            height: block::Height(CHECKPOINT_SEARCH_HEIGHT),
                            hash_or_0: target,
                        };
                        if block_downloads.position(probe).is_none() {
                            block_downloads.insert(probe);
                        }
                        continue 'send_to_peers;
                    }
                    CheckpointState::Locked { height: cp_height, .. } => {
                        if !*checkpoint_passed {
                            // Only sync from peers that pass the test; (re)issue the height test otherwise.
                            let due = checkpoint_last_tested.as_ref().map_or(true, |t| t.elapsed() >= CHECKPOINT_RETEST_INTERVAL);
                            if due {
                                let probe = HeightAndHashOr0 {
                                    height: block::Height(cp_height),
                                    hash_or_0: block::Hash([0; 32]),
                                };
                                if block_downloads.position(probe).is_none() {
                                    block_downloads.insert(probe);
                                }
                                *checkpoint_last_tested = Some(std::time::Instant::now());
                            }
                            continue 'send_to_peers;
                        }
                        // Passed peer: fall through to normal sync.
                    }
                    CheckpointState::Normal => {}
                }

                macro_rules! warning {
                    ($($arg:tt)*) => {{
                        // let msg = format!("NewNet: Peer {:?}: {}", connection_address, format!($($arg)*));
                        tracing::warn!("{}", format!("NewNet: Peer {:?}: {}", connection_address, format!($($arg)*)).to_string());
                    }};
                }

                let Some(our_tip_height) = dbg_verify(near_tip_chains.tip_height())
                else {
                    warning!("I don't have a tip height yet");
                    continue 'send_to_peers;
                };



                // // rule to push:
                // //     per each of our chains:
                // //         find the longest prefix branch they have with the chain
                // //         send everything after the end of the prefix branch
                // //     OR
                // //     per each of our chains:
                // //         if any of their branches is equal to or an extension of this chain, don't push
                // //         otherwise push the blocks after the intersection

                // // @Note: With this algorithm, another peer can systematically probe which
                // //        blocks we have on each branch. This may have implications.
                // let mut queue_blocks_to_send_unsolicited = || {
                //     let mut blocks_to_queue = HashSet::new();

                //     // @Note: If the peer is far behind, beam them blocks from farther back in our chain to catch them up.
                //     // There's surely a (more expensive) Zebra lookup to check if their tip is inside our finalized chain.
                //     if their_tree.tip_height < near_tip_chains.finalized_height {

                //         for height in their_tree.tip_height..near_tip_chains.finalized_height {
                //             // TODO: merge in get_bc_hash_at_height
                //             let res = rt.block_on(async {
                //                 read_state.clone().oneshot(ReadRequest::BestChainBlockHash(Height(height).into())).await
                //             });
                //             match res {
                //                 Ok(ReadResponse::BlockHash(hash)) => {
                //                     let Some(hash) = dbg_verify(hash) else {
                //                         warning!("Couldn't get block for height {height} which should be finalized! What happened?");
                //                         break;
                //                     };

                //                     if their_queue.contains(&hash) {
                //                         if TRACE { tracing::info!("Skipped sending block already in peer's queue. Peer {connection_address:?}. Hash: {hash}"); }
                //                         continue;
                //                     }

                //                     if blocks_to_this_peer < MAX_BLOCKS_TO_QUEUE_TO_COMMIT {
                //                         if blocks_to_queue.insert((hash, height)) {
                //                             blocks_to_this_peer += 1;
                //                         }
                //                     } else {
                //                         break;
                //                     }
                //                 }
                //                 Err(err) => { panic!("ReadRequest::BestChainBlockHash({height}): Error: {err:?}");              }
                //                 _        => { panic!("ReadRequest::BestChainBlockHash({height}): Unhandled response: {res:?}"); }
                //             }
                //         }

                //     }

                //     for our_chain in &near_tip_chains.chains {
                //         assert!(our_chain.blocks.len() > 0);
                //         let our_chain_height_bgn = our_chain.blocks[0].this_height;
                //         let our_chain_height_end = our_chain.blocks.last().unwrap().this_height + 1;
                //         let max_height_we_both_share = max_shared_height_with_tree(their_tree, &our_chain.blocks);

                //         let Some(max_height_we_both_share) = max_height_we_both_share else {
                //             continue; // Nothing on this chain of ours that we can use to extend their branch.
                //         };

                //         for height in max_height_we_both_share + 1 .. our_chain_height_end {
                //             let chain_i = (height - our_chain_height_bgn) as usize;

                //             let block_to_queue = &our_chain.blocks[chain_i];

                //             let hash = block_to_queue.this_hash;

                //             if their_queue.contains(&hash) {
                //                 if TRACE { tracing::info!("Skipped sending block already in peer's queue. Peer {connection_address:?}. Hash: {hash}"); }
                //                 continue;
                //             }

                //             // We may submit the same block multiple times (visit >1 of our chains that share a short prefix with their branch), and that's valid
                //             if blocks_to_this_peer < MAX_BLOCKS_TO_QUEUE_TO_COMMIT {
                //                 if blocks_to_queue.insert((hash, height)) {
                //                     blocks_to_this_peer += 1;
                //                 }
                //             } else {
                //                 break;
                //             }
                //         }
                //     }

                //     for (block, height) in blocks_to_queue {
                //         blocks_to_send.push((*connection_key, block, height, 0));
                //     }
                // };
                // if false {
                //     queue_blocks_to_send_unsolicited();
                // }


                // rule to pull:
                // per each of their branches:
                //     per each of our chains:
                //         if any of our chains contain all of the branch, then break
                //         track the maximum height of the intersections
                //     request everything up from the maximum height all the way to their tip
                //
                let mut queue_blocks_to_request = || {
                    // If the peer is far ahead, request blocks from farther back in their chain to catch us up.

                    for their_branch in &their_tree.branches {
                        let their_branch_height_bgn = their_branch[0].this_height;
                        let their_branch_height_end = their_branch.last().unwrap().this_height + 1;

                        let mut max_height_we_both_share = None;

                        for our_chain in &near_tip_chains.chains {
                            assert!(our_chain.blocks.len() > 0);

                            let our_chain_height_bgn = our_chain.blocks[0].this_height;
                            let our_chain_height_end = our_chain.blocks.last().unwrap().this_height + 1;


                            let prefix = chain_intersect_prefix(&their_branch, &our_chain.blocks);
                            // print_shadow_block_intersection(&their_branch, &our_chain.blocks, 1);


                            // If the prefix was empty, there was no overlap.
                            if prefix.is_empty() {
                                continue;
                            }

                            let height_of_match = prefix.last().unwrap().this_height;

                            assert!(height_of_match < our_chain_height_end);
                            assert!(height_of_match < their_branch_height_end);

                            max_height_we_both_share = max_height_we_both_share.max(Some(height_of_match));
                            // TODO: early out if fully contained
                        }

                        let Some(max_height_we_both_share) = max_height_we_both_share else {
                            continue; // Nothing on this chain of ours that we can use to extend their branch.
                        };

                        let bgn_h = near_tip_chains.finalized_height.max(max_height_we_both_share) + 1; // have to do extra check because inverted ranges panic
                        if bgn_h < their_branch_height_end {
                            for height in bgn_h .. their_branch_height_end {
                                if active_block_dls >= MAX_BANDWIDTH_BLOCKS_PER_RES {
                                    break;
                                }

                                let chain_i = (height - their_branch_height_bgn) as usize;

                                let block_to_queue = &their_branch[chain_i];

                                let hash = block_to_queue.this_hash;

                                if blocks_to_commit.iter().any(|(block_hash, _)| *block_hash == hash) {
                                    if TRACE { tracing::info!("Skipped requesting block already in our queue. Peer {connection_address:?}. Hash: {hash}"); }
                                    continue;
                                }

                                // We may submit the same block multiple times (visit >1 of our chains that share a short prefix with their branch), and that's valid
                                let height_hash = HeightAndHashOr0 {
                                    height: block::Height(height),
                                    hash_or_0: hash,
                                };

                                if let Some(dl_i) = block_downloads.position(height_hash) {
                                    // already included
                                } else {
                                    let dups = requests_by_hash.entry(hash).or_insert(0);
                                    if *dups < MAX_REQUEST_DUPLICATES_N {
                                        if let Some(dl_i) = block_downloads.insert(height_hash) {
                                            active_block_dls += 1; // total in flight
                                            *dups += 1; // duplicates of this block
                                            if TRACE { tracing::info!("Include request for near-tip   block @ {height}, {hash}, x{}! New DL count for peer: {}", *dups, block_downloads.used_flags.count_ones()); }
                                        } else {
                                            break; // not enough space for more downloads
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // let mut req_h = our_tip_height + 1;
                    // while req_h < their_tree.finalized_height {
                    //     if active_block_dls >= MAX_BANDWIDTH_BLOCKS_PER_RES {
                    //         break;
                    //     }

                    //     let height_hash = HeightAndHashOr0 {
                    //         height: block::Height(req_h),
                    //         hash_or_0: block::Hash([0;32]),
                    //     };
                    //     if let Some(dl_i) = block_downloads.position(height_hash) {
                    //         // already included
                    //     } else {
                    //         let dups = requests_by_height.entry(req_h).or_insert(0);
                    //         if *dups < MAX_REQUEST_DUPLICATES_N {
                    //             if let Some(dl_i) = block_downloads.insert(height_hash) {
                    //                 active_block_dls += 1; // total in-flight
                    //                 *dups += 1; // duplicates of "this block"
                    //                 if TRACE { tracing::info!("Include request for historical block @ {req_h}, x{}, offset {}! New DL count for peer: {}", *dups, block_downloads.slots[dl_i].offset, block_downloads.used_flags.count_ones()); }
                    //             } else {
                    //                 break;
                    //             }
                    //         }
                    //     }
                    //     req_h += 1;
                    // }
                };

                queue_blocks_to_request();

                // active_block_dls only advances inside queue_blocks_to_request, so a bump
                // means this peer started at least one download.
                if active_block_dls > active_block_dls_before_this_peer {
                    count_of_peers_we_started_download_from += 1;
                }
            }

            if TRACE { tracing::info!("Started {} new block dls (currently {active_block_dls}/{MAX_BANDWIDTH_BLOCKS_PER_RES})", active_block_dls - prev_active_block_dls); }
            next_dl_init = std::time::Instant::now() + dl_init_interval;
        }

        if std::time::Instant::now() >= next_peer_request {
            for (connection_key, peer) in peers.iter_mut() {
                // TODO: can we pack these into packlets now?
                let dls_n = peer.block_downloads.used_flags.count_ones();
                if dls_n > 0 {
                    if TRACE { tracing::info!("Sending {} DL requests for peer", dls_n); }
                }
                'downloads: for dl_i in 0..peer.block_downloads.slots.len() { // ALT: iterate LSB on flags directly
                    if peer.block_downloads.slot_is_used(dl_i) {
                        // Prevent cycles of receiving a sub-chunk after completing, then re-requesting a block we now have
                        // TODO: there may be a neater way to achieve the same thing
                        let HeightAndHashOr0 { height: block::Height(height), hash_or_0 } = peer.block_downloads.slots[dl_i].height_hash;
                        if hash_or_0 != block::Hash([0;32]) {
                            for our_chain in &near_tip_chains.chains {
                                if our_chain.blocks.iter().any(|block| block.this_hash == hash_or_0) {
                                    tracing::warn!("Don't need to re-request: block @ {height}, {hash_or_0} was already committed!");
                                    peer.block_downloads.remove(dl_i);
                                    continue 'downloads;
                                }
                            }
                        }

                        let dl = &peer.block_downloads.slots[dl_i];

                        // resume from the highest contiguous prefix we've already reassembled
                        let offset = match dl.reassembly.received.first() {
                            Some(&(0, end)) => end,
                            _ => 0,
                        };

                        let mut buf = [0u8; JUMBO_FRAG_SIZE];
                        let mut o = 0;
                        o += PACKET_TYPE_BLOCK_REQ.write_to(&mut buf[o..]);
                        o += PeerPowBlockRequest {
                            height_hash: dl.height_hash,
                            offset,
                        }.write_to(&mut buf[o..]);

                        if TRACE { tracing::info!("Requesting height {} hash {} offset {offset} from peer {connection_key:?}", dl.height_hash.height.0, dl.height_hash.hash_or_0); }

                        packets_to_send.push((*connection_key, Vec::from(&buf[..o])));
                    }
                }
            }

            next_peer_request = std::time::Instant::now() + peer_request_interval;
        }

        pending_selected_addresses.retain(|addr, time| std::time::Instant::now().saturating_duration_since(*time).as_secs() < 30);

        use rand::seq::SliceRandom;

        // @@@ @Todo @@@: We need speculative queueing ASAP!
        // packets_to_send.shuffle(&mut rand::thread_rng());

        // Service STP connections (send/recv).
        let resp = service_connections(&network_thread_handle, NetworkThreadPush {
            initiate_connections,
            wanted_connections: current_connections.clone(),
            send_unreliable: packets_to_send,
        });
        current_connections = resp.current_connections;
        let mut packets_received = resp.received_unreliable_messages;
        initiate_connections = Vec::new();
        packets_to_send = Vec::new();

        // Process received packets
        'process_packets: while packets_received.len() > 0 {
            let (connection_key, msg) = packets_received.remove(0);

            // Skip processing packets from now-disconnected peers
            let connection_address = {
                let Some((addr, _)) = current_connections.iter().find(|(addr, _)| addr.connection_key() == connection_key) else {
                    tracing::info!("NewNet: Dropping message from disconnected peer: {connection_key:?}");
                    continue 'process_packets;
                };
                addr.clone()
            };

            let peer = peers.entry(connection_key).or_insert(Peer::default());
            if pending_selected_addresses.contains_key(&connection_key) {
                peer.origin = PeerOrigin::Selected;
                pending_selected_addresses.remove(&connection_key);
            }

            let mut msg = &msg[..];

            macro_rules! warning {
                ($($arg:tt)*) => {{
                    // let msg = format!("Peer {:?}: {}", connection_address, format!($($arg)*));
                    tracing::warn!("{}", format!("NewNet: Peer {:?}: {}", connection_address, format!($($arg)*)).to_string());
                }};
            }
            macro_rules! kill {
                ($($arg:tt)*) => {{
                    tracing::error!("{}", format!("NewNet: Killing peer {:?}: {}", connection_address, format!($($arg)*)).to_string());
                    current_connections.retain(|(addr, _)| addr.connection_key() != connection_key);
                    let kill_bucket = address_bucket(local_addresses_secret, &connection_address) & (MAX_RECENT_BUCKETS - 1);
                    if let Some(recents) = recent_peer_addresses.get_mut(&(kill_bucket as u16)) {
                        recents.remove(&connection_address);
                    }
                    dbg_panic!();
                }};
            }
            macro_rules! some_or_kill {
                ($maybe_val:expr, $($arg:tt)*) => {{
                    let val = dbg_verify($maybe_val);
                    if val.is_none() {
                        kill!($($arg)*);
                    }
                    val
                }};
            }

            // if time_limit_exceeded {
            //     stp_library::signal_backpressure(PacketID(&msg));
            //     break; // Congested! Drop remainder!
            // }

            let Some(packet_type) = some_or_kill!(<u8>::read_from(&mut msg), "Packet type read failed") else {
                continue 'process_packets;
            };

            // if TRACE { tracing::info!("Got message type: {} ({})", packet_name_from_type(packet_type), packet_type); }

            if packet_type == PACKET_TYPE_PEER_ADDRESS_LIST {
                // @Todo: rate limit consumption

                let mut new_alleged_addresses = HashSet::new();

                let chunks = msg.chunks_exact(STP_ADDRESS_SERIALIZED_SIZE);
                for chunk in chunks {
                    let Some(address) = some_or_kill!(STPAddress::read_from(&mut &chunk[..]), "Address read failed") else {
                        continue 'process_packets;
                    };
                    new_alleged_addresses.insert(address);
                }

                // Prune my own addresses. // @Todo: Lazy-prune elsewhere, don't waste precious packet time eagerly-pruning.
                new_alleged_addresses.retain(|a| !my_keypairs.iter().any(|kp| a.key == kp.public && a.magic1 == kp.magic1));

                let sender_bucket = address_bucket(local_addresses_secret, &connection_address) & (MAX_SENDER_BUCKETS - 1);
                const_assert!(MAX_SENDER_BUCKETS.is_power_of_two());

                for address in new_alleged_addresses {
                    let map = alleged_peer_addresses.entry(sender_bucket as u16).or_default();

                    let address_clone_for_printing = address.clone();
                    if map.insert(address, connection_key).is_none() { // true if newly inserted
                        if map.len() >= MAX_ADDRESSES_PER_SENDER {
                            map.remove(&map.keys().choose(&mut rand::thread_rng()).cloned().unwrap());
                        }
                    }
                    // Note(Sam): Too noisy.
                    //if TRACE { tracing::info!("Adding new address to alleged addresses: {address_clone_for_printing:?}"); }
                }

            } else if packet_type == PACKET_TYPE_WANT_HOLE_PUNCH {
                // @Todo: rate limit consumption

                let Some(relay_to_connection_key) = some_or_kill!(ConnectionKey::read_from(&mut msg), "Connection key read failed") else {
                    continue 'process_packets;
                };

                if current_connections.iter().any(|(addr, _)| addr.connection_key() == relay_to_connection_key) {
                    let (mut buf, mut o) = ([0u8; 1 + STP_ADDRESS_SERIALIZED_SIZE], 0);

                    o += PACKET_TYPE_TRY_HOLE_PUNCH.write_to(&mut buf[o..]);
                    o += connection_address        .write_to(&mut buf[o..]);

                    if TRACE { tracing::info!("Relaying hole punch request from {:?} to {:?}...", connection_address, relay_to_connection_key); }
                    packets_to_send.push((relay_to_connection_key, Vec::from(&buf[..o])));
                }

            } else if packet_type == PACKET_TYPE_TRY_HOLE_PUNCH {
                // @Todo: rate limit consumption

                let Some(address_to_punch_to) = some_or_kill!(STPAddress::read_from(&mut msg), "Address read failed") else {
                    continue 'process_packets;
                };

                if !current_connections.iter().any(|(addr, _)| *addr == address_to_punch_to) {
                    if TRACE { tracing::info!("Attempting hole punch to {:?}, requested by {:?}...", address_to_punch_to, connection_address); }
                    initiate_connections.push(address_to_punch_to);
                }

            } else if packet_type == PACKET_TYPE_STATUS {
                // @Todo: rate limit consumption

                let Some(queue_len) = some_or_kill!(u8::read_from(&mut msg), "Failed to read block queue length") else {
                    continue 'process_packets;
                };
                if queue_len as usize > MAX_BLOCKS_TO_QUEUE_TO_COMMIT {
                    kill!("Too many queued block hashes sent in a peer status! Was {queue_len}, max is {MAX_BLOCKS_TO_QUEUE_TO_COMMIT}.");
                    continue 'process_packets;
                }

                let mut their_queue = HashSet::new();
                for _ in 0..queue_len {
                    let Some(hash) = some_or_kill!(<[u8; 32]>::read_from(&mut msg), "Failed to read block hash") else {
                        continue 'process_packets;
                    };
                    their_queue.insert(Hash(hash));
                };

                let Some(their_tree) = some_or_kill!(NearTipBranches::read_from(&mut msg), "NearTipBranches read failed") else {
                    continue 'process_packets;
                };

                peer.their_tree = their_tree;
                peer.their_queue = their_queue;

            } else if packet_type == PACKET_TYPE_BLOCK_REQ {

                let Some(request) = some_or_kill!(PeerPowBlockRequest::read_from(&mut msg), "PeerPowBlockRequest read failed") else {
                    continue 'process_packets;
                };

                if TRACE { tracing::info!("Received request for height {} hash {} offset {} from peer {connection_key:?}", request.height_hash.height.0, request.height_hash.hash_or_0, request.offset); }

                let mut height_hash = request.height_hash;
                if height_hash.hash_or_0 == block::Hash([0;32]) {
                    if let Some(hash) = read_state.best_chain_block_hash(height_hash.height) {
                        height_hash.hash_or_0 = hash;
                    }
                }

                if height_hash.hash_or_0 != block::Hash([0;32]) {
                    blocks_to_send.push((connection_key, height_hash.hash_or_0, height_hash.height.0, request.offset as usize));
                } else {
                    println!("Height requested that doesn't exist on BC: {:?}", height_hash.height);
                }
            } else if packet_type == PACKET_TYPE_BLOCK_CHUNK {
                let Some(our_tip_height) = dbg_verify(near_tip_chains.tip_height()) else {
                    warning!("I don't have a tip height yet");
                    continue 'process_packets;
                };

                let Some(hdr) = some_or_kill!(PeerPowBlockResponseChunkHdr::read_from(&mut msg), "failed to read PoW chunk header") else {
                    continue 'process_packets;
                };

                // @Todo: rate limit consumption

                // NOTE: this has to change if we want to support both push & pull options
                let Some(dl_i) = peer.block_downloads.position(hdr.height_hash) else {
                    if TRACE { tracing::info!("block @ {}, {} not requested (any longer), skip it", hdr.height_hash.height.0, hdr.height_hash.hash_or_0); }
                    continue 'process_packets;
                };

                // @Todo: @Temporary. We will instead want to evict non-committable blocks and replace them with new blocks if they are committable.
                // if blocks_to_commit.len() >= MAX_BLOCKS_TO_QUEUE_TO_COMMIT {
                //     warning!("Block commit queue full! Dropping.");
                //     continue 'process_packets;
                // }

                // @Note: for valid blocks the height can be computed from block data, so this is an early-out optimization.
                let alleged_height = hdr.height_hash.height.0;

                // Lite checkpointing: a block arriving at the checkpoint height reveals whether
                // this peer's main chain matches the checkpoint. (Evaluated from the chunk header,
                // so a wrong-chain peer is rejected after a single chunk rather than a full block.)
                if let CheckpointState::Locked { target, height: cp_height } = checkpoint_state {
                    if alleged_height == cp_height {
                        let their_hash = hdr.height_hash.hash_or_0;
                        let passed = their_hash == target;
                        peer.checkpoint_passed = passed;
                        peer.checkpoint_last_tested = Some(std::time::Instant::now());
                        if !passed {
                            if TRACE { tracing::info!("NewNet: Peer failed checkpoint test: {their_hash} != target {target} @ height {cp_height}"); }
                            peer.block_downloads.remove(dl_i);
                            continue 'process_packets;
                        }
                        // Passed: this is the checkpoint block itself; let it commit normally.
                    }
                }

                // @Note: Skip blocks that are older than the base of our NearTipChain view of the best chain.
                // Depending on whether our NEAR_TIP_CHAIN_LEN is < or > Zebra's MAX_BLOCK_REORG_HEIGHT,
                // presence in the best NearTipChain *may* or *may not* logically imply that this new block
                // is "not even worth" submitting to Zebra (rejected due to being too far back).
                let min_height = near_tip_chains.chains[0].blocks[0].this_height; // @Todo: this assumes :AssumeGenesisBlockIncludedInNearTipChains

                // if TRACE { tracing::info!("Block @ {alleged_height}, offset {}...", hdr.offset); }

                if alleged_height < min_height {
                    warning!("Block at height {alleged_height} is below our near-tip-chain height {min_height}");
                    peer.block_downloads.remove(dl_i);
                    continue 'process_packets; // Deciding that it's "not even worth" sending to Zebra
                }

                if alleged_height <= near_tip_chains.finalized_height {
                    warning!("Block at height {alleged_height} is already finalized");
                    peer.block_downloads.remove(dl_i);
                    continue 'process_packets; // Definitely already committed :)
                }


                // @Note: the hash could be computed from the block header, so this is an early-out optimization.
                let alleged_hash = hdr.height_hash.hash_or_0;

                // if TRACE { tracing::info!("Block @ {alleged_height} hash {alleged_hash}, offset {}...", hdr.offset); }

                if alleged_hash == block::Hash([0;32]) {
                    kill!("the provided hash should not be 0");
                    continue 'process_packets;
                }


                {
                    if hdr.size as u64 > zebra_chain::block::MAX_BLOCK_BYTES {
                        kill!("peer tried to send a block larger than consensus allows: {}", hdr.size);
                        continue 'process_packets;
                    }
                    let dl = &mut peer.block_downloads.slots[dl_i];
                    if dl.height_hash.height != hdr.height_hash.height {
                        kill!("the hash matches but the height doesn't");
                        continue 'process_packets;
                    }
                    if let Some(existing) = dl.reassembly.total_len {
                        if existing != hdr.size {
                            kill!("peer tried to change size of block mid-send (from {} to {})", existing, hdr.size);
                            continue 'process_packets;
                        }
                    }
                    dl.height_hash.hash_or_0 = hdr.height_hash.hash_or_0;
                }


                if blocks_to_commit.iter().any(|(queued_hash, _)| *queued_hash == alleged_hash) {
                    warning!("Block was already queued to commit!: {alleged_hash}");
                    peer.block_downloads.remove(dl_i);
                    continue 'process_packets;
                }

                for our_chain in &near_tip_chains.chains {
                    if our_chain.blocks.iter().any(|block| block.this_hash == alleged_hash) {
                        warning!("Block was already committed!: {alleged_hash}");
                        peer.block_downloads.remove(dl_i);
                        continue 'process_packets;
                    }
                }

                // reject fragments that extend past the declared size (bounds the reassembly buffer)
                if hdr.offset as u64 + msg.len() as u64 > hdr.size as u64 {
                    kill!("block fragment extends beyond declared size ({} + {} > {})", hdr.offset, msg.len(), hdr.size);
                    continue 'process_packets;
                }

                // Insert this fragment; the final fragment (reaching `size`) carries the fin marker.
                let is_fin = hdr.offset as u64 + msg.len() as u64 == hdr.size as u64;
                let (success, complete) = {
                    let dl = &mut peer.block_downloads.slots[dl_i];
                    dl.reassembly.insert(hdr.offset as usize, msg, is_fin)
                };
                if !success {
                    kill!("overlapping/out-of-bounds block fragment");
                    continue 'process_packets;
                }
                peer.block_downloads.slots[dl_i].last_modified = std::time::Instant::now();

                if !complete {
                    if TRACE { tracing::info!("Block @ {alleged_height} hash {alleged_hash}: fragment at offset {} ({} bytes); awaiting more", hdr.offset, msg.len()); }
                    continue 'process_packets;
                }

                if TRACE { tracing::info!("Block @ {alleged_height} hash {alleged_hash}: All fragments received ({} bytes)", hdr.size); }

                let block_data_vec = peer.block_downloads.remove(dl_i).reassembly.buf;
                let block_data = &block_data_vec[..];

                // Lite checkpointing: handle SEARCH-mode by-hash probe responses (sentinel height).
                // If it's the target we acquire it (learning its height) and lock on; otherwise it's
                // a leftover probe and we simply drop it. The download slot was already removed above.
                if alleged_height == CHECKPOINT_SEARCH_HEIGHT {
                    if let CheckpointState::Search { target } = checkpoint_state {
                        if alleged_hash == target {
                            if let Some(b) = block_data.zcash_deserialize_into::<Block>().ok() {
                                if b.hash() == target {
                                    if let Some(h) = b.coinbase_height() {
                                        tracing::info!("NewNet: Acquired checkpoint block {target} @ height {}; SEARCH -> LOCKED.", h.0);
                                        checkpoint_state = CheckpointState::Locked { target, height: h.0 };
                                    }
                                }
                            }
                        }
                    }
                    continue 'process_packets;
                }

                // Lite checkpointing: while SEARCHing we do not accept/commit any block from any peer.
                if matches!(checkpoint_state, CheckpointState::Search { .. }) {
                    continue 'process_packets;
                }


                // TODO: we should be able to early out once we have chunk 0 or a hdr-contained parent hash
                // @Volatile, depends on block header format.
                let parent_hash = {
                    let mut tmp = &block_data[..];
                    let Some(version) = some_or_kill!(<u32>::read_from(&mut tmp), "Failed to read block version number") else {
                        continue 'process_packets;
                    };
                    let Some(parent_hash) = some_or_kill!(<[u8; 32]>::read_from(&mut tmp), "Failed to read parent hash") else {
                        continue 'process_packets;
                    };
                    Hash(parent_hash)
                };

                let have_parent_in_chains           = is_parent_in_chains(&read_state, &near_tip_chains, parent_hash);
                let have_parent_in_blocks_to_commit = blocks_to_commit.iter().any(|(queued_hash, _)| *queued_hash == parent_hash);

                if !have_parent_in_chains && !have_parent_in_blocks_to_commit {
                    // TODO: we should keep these, but have them easily evictable, for OoO block downloads
                    warning!("Block does not link anywhere known, neither to our chains nor to our blocks-to-commit queue! Not queueing; dropping: height {alleged_height} hash {alleged_hash}, parent {parent_hash}");
                    continue 'process_packets;
                }

                if TRACE { tracing::info!("Block @ {alleged_height} hash {alleged_hash}, valid hash and height and links somewhere known..."); }

                use zebra_chain::serialization::ZcashDeserializeInto;
                let Some(block) = some_or_kill!(block_data.zcash_deserialize_into::<Block>().ok(), "Failed to deserialize block") else {
                    continue 'process_packets;
                };

                // @Todo: - safely reorganize checks in block verification that are quite cheap to the top of the function
                //        - turn that into a function
                //        - call it at the top of the place where they are currently used
                //        - call that right here

                if TRACE { tracing::info!("Block @ {alleged_height} hash {alleged_hash}, deserialized..."); }

                let hash = block.hash();
                if hash != alleged_hash {
                    kill!("Deserialized block hash did not match advertised hash");
                    continue 'process_packets;
                }

                if block.header.previous_block_hash != parent_hash {
                    kill!("Deserialized block parent hash did not match advertised parent hash");
                    continue 'process_packets;
                }

                let Some(height) = some_or_kill!(block.coinbase_height(), "Failed to read coinbase height") else {
                    continue 'process_packets;
                };

                if height != Height(alleged_height) {
                    kill!("Computed block height did not match advertised hash");
                    continue 'process_packets;
                }

                if TRACE { tracing::info!("NewNet: \x1b[93mGOT BLOCK HASH\x1b[0m: {}", hash); }

                // @Todo(Phil): Semantic verification.

                // @Note: Evict a sidechain tail to make room. Prefer non-committable tails.
                if blocks_to_commit.len() >= MAX_BLOCKS_TO_QUEUE_TO_COMMIT {

                    // @Lazy. N squared. Not cool.
                    let is_tail = |i: usize| {
                        let (h, _) = &blocks_to_commit[i];
                        !blocks_to_commit.iter().any(|(_, b)| b.header.previous_block_hash == *h)
                    };

                    // Prefer a non-committable tail (parent not in chains).
                    // Never evict the incoming block's parent — that would orphan the block we're about to push.
                    let evict_idx = (0..blocks_to_commit.len()).find(|&i| {
                        blocks_to_commit[i].0 != parent_hash
                        && is_tail(i) && !is_parent_in_chains(&read_state, &near_tip_chains, blocks_to_commit[i].1.header.previous_block_hash)
                    }).or_else(|| if have_parent_in_chains {
                        // All tails are committable; only evict if new block is also committable.
                        (0..blocks_to_commit.len()).filter(|&i| blocks_to_commit[i].0 != parent_hash && is_tail(i)).last()
                    } else {
                        None
                    });

                    if let Some(idx) = evict_idx {
                        warning!("Evicting block from commit queue to make room");
                        blocks_to_commit.swap_remove(idx);
                    } else {
                        warning!("Commit queue full of committable blocks; dropping non-committable block");
                        continue 'process_packets;
                    }
                }

                println!("Queueing for commit: {}", hash);
                blocks_to_commit.push((hash, std::sync::Arc::new(block)));
            } else {
                warning!("NewNet: Got unknown msg type={} len={}, ignoring.", packet_type, msg.len());
                continue 'process_packets;
            }

            // Valid packet arrived and was processed successfully. Mark this peer as a recent address.

            // Once we have received a message from this connection, they have proven their path.
            // Update our address maps.
            let bucket = address_bucket(local_addresses_secret, &connection_address) & (MAX_RECENT_BUCKETS - 1);
            const_assert!(MAX_RECENT_BUCKETS.is_power_of_two());

            let recents_bucket = recent_peer_addresses.entry(bucket as u16).or_default();

            // Update existing address, or evict oldest + insert new.
            if let Some(existing) = recents_bucket.get_mut(&connection_address) {
                existing.recv_time_ns = monotonic_clock_ns();
                existing.failure_count = 0;
            } else {
                if recents_bucket.len() >= MAX_PEERS_PER_BUCKET {
                    let oldest = recents_bucket.iter().min_by_key(|(_, v)| v.recv_time_ns).map(|(k, _)| k.clone());
                    if let Some(k) = oldest { recents_bucket.remove(&k); }
                }
                let prev = recents_bucket.insert(connection_address.clone(), RecentPeerAddress {
                    recv_time_ns: monotonic_clock_ns(),
                    failure_count: 0u32, // @Todo: Implement this counter!
                });
                if prev.is_none() {
                    // if TRACE { tracing::info!("Added address to recent addresses: {connection_address:?}"); }
                }
            }

            // Alleged addresses that are now in recent_peer_addresses are skipped lazily
            // at connection time, and pruned from the alleged map each loop iteration.

        }

        blocks_to_commit.sort_by_key(|(_, block)| block.coinbase_height().expect("all blocks in the commit queue should already have been confirmed to have a height"));

        // @Temporary: just pull one and wait to commit it.
        let mut any_blocks_in_the_queue_can_make_progress = false;
        blocks_to_commit.retain(|(hash, block_arc)| {
            let hash = *hash;
            let block_arc = block_arc.clone();

            let parent_hash = block_arc.header.previous_block_hash;

            if !is_parent_in_chains(&read_state, &near_tip_chains, parent_hash) {
                return true; // keep
            }

            any_blocks_in_the_queue_can_make_progress = true;

            let height = block_arc.coinbase_height().expect("all blocks in the commit queue should already have been confirmed to have a height").0;
            println!("Committing: @ {height}, {hash}");
            let res = rt.block_on(commit_block(block_arc));
            match res {
                Ok(_) => {
                    println!("committed!: @ {height}, {hash}");
                    false // remove
                }
                Err(BlockCommitError::Duplicate) => {
                    // @Todo: We would like to update the finalized height here, but
                    //        we would also have to call near_tip_chains.remove_chains_invalidated_by_finalized() with that height.
                    println!("Already was committed: {}", hash);
                    false // remove
                }
                Err(BlockCommitError::Other(description)) => {
                    // @Todo: detect request timeout and keep the block if it merely timed out.
                    println!("Failed to commit {}: {}", hash, description);
                    false // remove
                }
            }
        });

        if blocks_to_commit.len() > 0 && !any_blocks_in_the_queue_can_make_progress {
            // dbg_panic!("No blocks made progress in the queue this tick!? This should never hit! Currently we are only queueing blocks that can make progress!"); // @Temporary.
            blocks_to_commit.clear();
        }

        // Lite checkpointing: exit LOCKED once the checkpoint block is committed on our best
        // chain locally (not merely sitting in the commit queue). SEARCH -> LOCKED happens
        // inline when we acquire the block (see the BLOCK_CHUNK handler).
        if std::time::Instant::now() >= next_checkpoint_check {
            if let CheckpointState::Locked { target, height } = checkpoint_state {
                if read_state.best_chain_block_hash(Height(height)) == Some(target) {
                    tracing::info!("NewNet: Checkpoint {target} committed locally @ height {height}; LOCKED -> NORMAL.");
                    checkpoint_state = CheckpointState::Normal;
                }
            }
            next_checkpoint_check = std::time::Instant::now() + checkpoint_check_interval;
        }

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

        // {
        //     let blocks2 = [
        //         ShadowBlock { parent_hash: Hash([0x01;32]), this_hash: Hash([0x02;32]), this_height: 2 },
        //         ShadowBlock { parent_hash: Hash([0x02;32]), this_hash: Hash([0x03;32]), this_height: 3 },
        //         ShadowBlock { parent_hash: Hash([0x03;32]), this_hash: Hash([0x04;32]), this_height: 4 },
        //         ShadowBlock { parent_hash: Hash([0x04;32]), this_hash: Hash([0x05;32]), this_height: 5 },
        //         ShadowBlock { parent_hash: Hash([0x05;32]), this_hash: Hash([0x16;32]), this_height: 6 },
        //         ShadowBlock { parent_hash: Hash([0x16;32]), this_hash: Hash([0x17;32]), this_height: 7 },
        //         ShadowBlock { parent_hash: Hash([0x17;32]), this_hash: Hash([0x18;32]), this_height: 8 },
        //         ShadowBlock { parent_hash: Hash([0x18;32]), this_hash: Hash([0x19;32]), this_height: 9 },
        //         ShadowBlock { parent_hash: Hash([0x19;32]), this_hash: Hash([0x1a;32]), this_height:10 },
        //         ShadowBlock { parent_hash: Hash([0x1a;32]), this_hash: Hash([0x1b;32]), this_height:11 },
        //     ];

        //     print_shadow_block_intersection(&blocks[0..8], &blocks2[2..9], 1);
        //     eprintln!("");
        // }

        let mut buf = [0u8; PACKET_STATUS_MAX_SIZE];

        let mut chains = NearTipChains { finalized_height: 0, chains: Vec::new() };

        // (a, b), (b, c,), (c, d), (d, e), ..., (c, i), (i, j), ...
        // a b c d e f g h
        //     \ i j k l m n
        chains.push_blocks(&blocks);

        let mut chains2 = NearTipChains { finalized_height: 0, chains: Vec::new() };
        // (a, b), (b, c,), (c, d), (d, e), ..., (c, i), (i, j), ...
        // a b c d e f g h
        //     \ i j k l m n
        for block in &blocks {
            chains2.push_blocks(&[*block]);
        }
        let tip_height = 11;

        debug_assert_eq!(chains, chains2, "building incrementally should be functionally equivalent to batch-built");
        debug_assert_eq!(chains.tip_height(), Some(11));
        print_shadow_block_slice(blocks[0].this_height, &blocks, 1);
        eprintln!("");

        chains.roundtrip_to_branches(PACKET_STATUS_MAX_SIZE, 1);

//         println!("{}", hex::encode(buf));
    }
}

