const DUMP_NEAR_TIP_CHAINS: bool = 1 == 0;

use std::collections::{HashMap, HashSet};
use static_assertions::const_assert;

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
const STP_HEADER_SIZE: usize = 6 + crypto_overhead_from_connect_magic1(CRYPTO_MAGIC).unwrap();
const STP_PACKLET_HDR: usize = std::mem::size_of::<PackletHeader>();
const STP_JUMBO_HDR:   usize = std::mem::size_of::<PackletOneJumboFragment>();
const PATH_MTU: usize = UDP_mMTU
                      - STP_HEADER_SIZE
                      - STP_PACKLET_HDR;
const JUMBO_FRAG_SIZE: usize = UDP_mMTU
                             - STP_HEADER_SIZE
                             - STP_PACKLET_HDR
                             - STP_JUMBO_HDR;

const CRYPTO_MAGIC: u64 = CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2s;
//const CRYPTO_MAGIC: u64 = CONNECT_MAGIC1_PLAIN_TEXT;

// @Todo: more of these, merge with Tenderlink, reintroduce peer discovery from p2p, etc.
const PACKET_TYPE_STATUS: u8 = 1;
const PACKET_TYPE_BLOCK:  u8 = 2;

const PACKET_TYPE_PEER_ADDRESS_LIST: u8 = 3;

const PACKET_TYPE_WANT_HOLE_PUNCH: u8 = 4;
const PACKET_TYPE_TRY_HOLE_PUNCH:  u8 = 5;


const PACKET_STATUS_MAX_HASHES: usize = 400; // @Lazy: gives room for 300 hashes plus room for 200 run metadatas
const PACKET_STATUS_MAX_SIZE:   usize = ((PACKET_STATUS_MAX_HASHES * 32 + JUMBO_FRAG_SIZE - 1) / JUMBO_FRAG_SIZE) * JUMBO_FRAG_SIZE; // @Cleanup @Lazy.

fn dbg_break() {
    #[cfg(target_arch = "x86_64")] unsafe { std::arch::asm!("int 3"); }
    // @Todo: AArch64 debugbreak.
}

#[cfg(debug_assertions)] #[track_caller] fn dbg_panic_internal(msg: std::fmt::Arguments<'_>) -> ! {
    dbg_break();
    std::env::set_var("RUST_BACKTRACE", "full");
    panic!("{msg}");
}
macro_rules! dbg_panic {
    ()            => { #[cfg(debug_assertions)] dbg_panic_internal(format_args!("explicit panic")); };
    ($($arg:tt)*) => { #[cfg(debug_assertions)] dbg_panic_internal(format_args!($($arg)*)); };
}

pub fn dbg_verify<T>(t: Option<T>) -> Option<T> {
    #[cfg(debug_assertions)] {
        if t.is_none() { dbg_break(); }

        #[cfg(not(target_arch = "x86_64"))]
        return Some(t.unwrap());
    }

    t
}
pub fn verify<T>(t: Option<T>) -> T {
    if t.is_none() { dbg_break(); }

    t.unwrap()
}


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

#[derive(Debug,          Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
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
        self.chains.first().and_then(|ch| ch.blocks.last()).map(|bl| bl.this_height)
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
        let res = self.write_to(buf);
        debug_assert!(res > 0, "failed to write packet");

        let branches = NearTipBranches::read_from(&mut &buf[..res]).unwrap();
        if hash_bytes_n > 0 {
            if DUMP_NEAR_TIP_CHAINS { eprintln!("\n{} branches:", branches.branches.len()); }
            branches.dump(hash_bytes_n);
        }
    }
}

impl SliceWrite for NearTipChains {
    // currently assumes each block arrives after its parent
    fn write_to(&self, buf: &mut [u8]) -> usize {
        { // Manually, at runtime, compute the min len for this function. Awful! Also, @Volatile. Fun.
            let min_len = NearTipChains::min_packet_size();
            assert!(buf.len() >= min_len, "NearTipChains::write_to() needs at least enough room for one best-chain run of {} hashes (i.e., >= {min_len} bytes).", NEAR_TIP_CHAIN_LEN + 1);
        }

        // doing parallel chains (redundantly keeping shared prefixes)
        // ALT: index tree in single buffer

        let tip_height = self.tip_height().expect("programmer error: should be non-empty");
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

            // The first branch must be the best chain
            if branches.is_empty() && end_height - 1 != hdr.tip_height {
                dbg_panic!("The first branch must be the best chain and match the header's tip height! First branch height: {}, tip height: {}", end_height - 1, hdr.tip_height);
                return None;
            }

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


const TRACE     :bool=0!=       1;


// @Todo: always only wait on real stuff, never sleeping for fixed amounts like this

// @Note: Status/block packet deliveries should NOT happen once per tick, because we don't sleep
//        when there are more packets to process in the current tick.
//        Let's disaggregate send from receive. And in fact, let's make them all happen at
//        coprime millisecond intervals, so we can make sure our code is not relying on
//        any kind of consistent or clean timing in order to function properly.

const STATUS_MS: u64 = 839;      // fastest interval at which to send status messages
const BLOCK_SEND_MS: u64 = 1013; // fastest interval at which to send blocks to peers


const MAX_PEERS_TO_CONNECT_PER_ATTEMPT: usize = 8;
const PEER_CONNECT_MS: u64 = 2000;
const PEER_GOSSIP_MS:  u64 = 3229;


const IDLE_MS: u64 = 571; // 571 is the closest prime number to (geometric_mean(839/phi, 1013/phi))

// use crate::crosslink::TFLServiceRequest;
// use crate::crosslink::TFLServiceResponse;
// use crate::crosslink::TFLServiceError;

type ReadState = crate::service::ReadStateService;
type State = tower::buffer::Buffer<tower::util::BoxService<Request, Response, crate::BoxError>, Request>;
// type TFLService = tower::buffer::Buffer<tower::util::BoxService<TFLServiceRequest, TFLServiceResponse, TFLServiceError>, TFLServiceRequest>;

// TODO: the handling for these calls is sync, so don't have the indirection through async

pub fn get_tips(read_state: &ReadState, rt: &tokio::runtime::Handle) -> (Option<(Height, Hash)>, Option<(Height, Hash)>) {
    let tip_maybe = rt.block_on(async {
        let res = read_state.clone().oneshot(ReadRequest::Tip).await;
        match res {
            Ok(ReadResponse::Tip(tip_maybe)) => tip_maybe,
            Err(err) => panic!("sync start err: {err:?}"),
            _ => panic!("sync err: unhandled response: {res:?}"),
        }
    });
    let finalized_tip_maybe = rt.block_on(async {
        let res = read_state.clone().oneshot(ReadRequest::FinalizedTip).await;
        match res {
            Ok(ReadResponse::Tip(finalized_tip_maybe)) => finalized_tip_maybe,
            Err(err) => panic!("sync start err: {err:?}"),
            _ => panic!("sync err: unhandled response: {res:?}"),
        }
    });
    (tip_maybe, finalized_tip_maybe)
}

pub fn get_tips_blocking(read_state: &ReadState, rt: &tokio::runtime::Handle) -> ((Height, Hash), (Height, Hash)) {
    loop {
        let (Some(tip), Some(finalized_tip)) = get_tips(&read_state, &rt)
        else {
            std::thread::yield_now();
            continue;
        };
        break (tip, finalized_tip)
    }
}

pub fn get_genesis_hash(read_state: &ReadState, rt: &tokio::runtime::Handle) -> Hash {
    rt.block_on(async {
        let res = read_state.clone().oneshot(ReadRequest::BestChainBlockHash(Height(0))).await;
        match res {
            Ok(ReadResponse::BlockHash(Some(hash))) => hash,
            Ok(ReadResponse::BlockHash(None)) => panic!("failed to get genesis block"),
            Err(err) => panic!("sync start err: {err:?}"),
            _ => panic!("sync err: unhandled response: {res:?}"),
        }
    })
}

pub fn get_bc_hash_at_height(read_state: &ReadState, rt: &tokio::runtime::Handle, height: Height) -> Option<Hash> {
    rt.block_on(async {
        let res = read_state.clone().oneshot(ReadRequest::BestChainBlockHash(height)).await;
        match res {
            Ok(ReadResponse::BlockHash(maybe_hash)) => maybe_hash,
            Err(err) => {
                tracing::error!("get_bc_hash_at_height({height:?}): Error: {err:?}");
                None
            },
            _ => panic!("get_bc_hash_at_height({height:?}): Unhandled response: {res:?}"),
        }
    })
}

pub fn get_hdr_at_hash(read_state: &ReadState, rt: &tokio::runtime::Handle, hash: Hash) -> Option<(std::sync::Arc<block::Header>, Height, Hash)> {
    rt.block_on(async {
        let res = read_state.clone().oneshot(ReadRequest::BlockHeader(hash.into())).await;
        match res {
            Ok(ReadResponse::BlockHeader{ header, height, hash, .. }) => Some((header, height, hash)),
            Err(err) => {
                tracing::error!("get_hdr_at_hash({hash}): Error: {err:?}");
                None
            },
            _ => panic!("get_hdr_at_hash({hash}): Unhandled response: {res:?}"),
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
                tracing::error!("get_hdrs_after_hash({pre_first_hash}): Error: {err:?}");
                None
            },
            _ => panic!("get_hdrs_after_hash({pre_first_hash}): Unhandled response: {res:?}"),
        }
    })
}

pub fn is_parent_in_chains(rt: &tokio::runtime::Handle, state: &State, near_tip_chains: &NearTipChains, parent_hash: Hash) -> bool {
    for our_chain in &near_tip_chains.chains {
        if our_chain.blocks.iter().any(|block| block.this_hash == parent_hash) {
            return true;
        }
    }

    // @Dev @Debug, but we would like to be able to quickly do this in production as well...
    let res = rt.block_on(async { state.clone().oneshot(Request::KnownBlock(parent_hash.try_into().unwrap())).await });
    match res {
        Ok(Response::KnownBlock(None)) => {
            return false;
        }
        Ok(Response::KnownBlock(Some(block))) => {
            // @Note: if we don't find the parent in near tip chains, but we ask Zebra and Zebra is aware of it, warn loudly.
            eprintln!("WARNING!! Block hash {parent_hash} was NOT in near tip chains but contained by Zebra state!!");
            //dbg_panic!();
            return true;
        }
        Err(err) => {
            println!("Error requesting {}: {:?}", parent_hash, err);
            return false;
        }
        _ => unreachable!("wrong response for KnownBlock")
    }

    return false;
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

#[derive(Debug, Default, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Peer {
    origin: PeerOrigin,
    their_tree: NearTipBranches,
}

#[derive(Debug)]
pub enum BlockCommitError {
    Duplicate,
    Other(String),
}

pub fn sync(
    commit_block: impl Fn(std::sync::Arc<Block>) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Hash, BlockCommitError>> + Send>>,
    config: &crate::config::Config,
    read_state: ReadState,
    state: State,
    // tfl_service: TFLService, // no TFLServiceHandle. Sadge!
    rt: tokio::runtime::Handle,
) {
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(500);
    BLOCK_EVENT_QUEUE_SENDER.set(event_tx).unwrap();

    let mut near_tip_chains = NearTipChains::default();
    // @Todo: @Refactor into a fn we can call to flush and reset the NearTipChains state.
    'init_near_tip_chains: {
        let ((tip_height, tip_hash), (finalized_tip_height, finalized_tip_hash)) = get_tips_blocking(&read_state, &rt);
        println!("NewNet: Starting at height={} hash={:?} finalized_height={} finalized_hash={}", tip_height.0, tip_hash, finalized_tip_height.0, finalized_tip_hash);

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
        near_tip_chains.push_blocks(&[ShadowBlock { this_hash: get_genesis_hash(&read_state, &rt), ..ShadowBlock::default() }]);

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

    let mut connections_map: HashMap<ConnectionKey, ConnectionTrackingData> = HashMap::new();

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
            println!("NewNet: Connecting to peer: {:?}", address);
            match connect_to(socket, &mut connections_map, &my_keypairs, &address) {
                Err(e) => println!("NewNet: Initial peer connect: connect_to failed: {e}"),
                Ok(()) => {},
            }
            initial_peer_addresses.push(address);
        } else {
            eprintln!("NewNet: Failed to parse peer address: {}", peer_str);
        }
    }

    println!("NewNet: STP setup complete, port={}, peers={}", config.network_local_port, initial_peer_addresses.len());

    // Sync state
    let tick_duration = std::time::Duration::from_millis(IDLE_MS);
    let status_interval = std::time::Duration::from_millis(STATUS_MS);
    let block_send_interval = std::time::Duration::from_millis(BLOCK_SEND_MS);
    let peer_gossip_interval = std::time::Duration::from_millis(PEER_GOSSIP_MS);
    let peer_connect_interval = std::time::Duration::from_millis(PEER_CONNECT_MS);

    const MAX_BLOCKS_TO_QUEUE_TO_COMMIT: usize = 12; // DO NOT MAKE LARGE, subject to N^2!!!

    let mut blocks_to_commit:  Vec<(Hash, std::sync::Arc<Block>)> = Vec::new();
    let mut blocks_to_send:    Vec<(ConnectionKey, Hash, u32)>    = Vec::new();
    let mut serialized_blocks: HashMap<Hash, Vec<u8>>             = HashMap::new(); // @Todo: cap max memory storage size for this map.

    use rand::Rng;
    let mut local_addresses_secret: u64 = rand::thread_rng().gen();

    let mut recent_peer_addresses:  HashMap<u16, HashMap<STPAddress, RecentPeerAddress>> = HashMap::new();
    let mut alleged_peer_addresses: HashMap<u16, HashMap<STPAddress, ConnectionKey>> = HashMap::new(); // keyed by sender, not address. connection key of latest sender is stored so we know who to ask to initiate UDP hole punch

    let mut peers: HashMap<ConnectionKey, Peer> = HashMap::new();
    let mut pending_selected_addresses: HashMap<ConnectionKey, std::time::Instant> = HashMap::new(); // store time for expiry

    // let mut XXX_tick_loop_counter = 0usize;

    let mut next_status       = std::time::Instant::now();
    let mut next_block_send   = std::time::Instant::now();
    let mut next_peer_gossip  = std::time::Instant::now();
    let mut next_peer_connect = std::time::Instant::now();

    let stp_address_get_short_string = |address| {
        let addr = format!("{:?}", address);
        let short = &addr[addr.len().saturating_sub(6)..]; // 6 base64 chars -> 4 bytes -> probably unique up to 65536 connections
        short.to_string()
    };

    // Main sync loop
    loop {
        let mut my_peers_to_print = Vec::new();
        for (connection_key, connection) in &connections_map {
            if connection.is_connected() {
                my_peers_to_print.push(stp_address_get_short_string(connection.address()));
            }
        }
        println!("tip height: {:?}, finalized height: {:?}, peers: {:?}", near_tip_chains.tip_height(), near_tip_chains.finalized_height, my_peers_to_print);

        let loop_start = std::time::Instant::now();

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
                            println!("NewNet: block_event: {:?}", block_event);
                        }
                    }
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                Err(err) => { tracing::error!("{err:?}"); break; },
            }
        }

        // #[cfg(debug_assertions)]
        // { // @Dev @Debug: Roundtrip test.
        //     near_tip_chains.roundtrip_to_branches(PACKET_STATUS_MAX_SIZE, if XXX_tick_loop_counter % 20 == 0 { 2 } else { 0 });
        //     XXX_tick_loop_counter += 1;
        // }

        // Try to reconnect to trusted initial seed peers
        for address in &initial_peer_addresses {
            if !connections_map.contains_key(&address.connection_key()) {
                println!("NewNet: Connecting to {:?}...", address);
                match connect_to(socket, &mut connections_map, &my_keypairs, address) {
                    Err(e) => println!("NewNet: Initial peer reconnect: connect_to failed: {e}"),
                    Ok(()) => {},
                }
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

            let mut recents: Vec<_> = recent_peer_addresses.iter().collect(); recents.shuffle(rng);
            for (_, map) in recents {
                if connection_attempts >= MAX_PEERS_TO_CONNECT_PER_ATTEMPT {
                    break;
                }
                let Some((address, _)) = map.iter().choose(rng) else {
                    continue;
                };
                if connections_map.contains_key(&address.connection_key()) {
                    continue;
                }

                match connect_to(socket, &mut connections_map, &my_keypairs, address) {
                    Err(e) => {
                        println!("NewNet: Recent-address reconnect: connect_to failed: {e}");
                        continue;
                    }
                    Ok(()) => {},
                }

                connection_attempts += 1;

                pending_selected_addresses.insert(address.connection_key(), std::time::Instant::now());

                // Hole punch
                let (mut buf, mut o) = ([0u8; 1 + STP_ADDRESS_SERIALIZED_SIZE], 0);
                o += PACKET_TYPE_WANT_HOLE_PUNCH .write_to(&mut buf[o..]);
                o += ConnectionKey::from(address).write_to(&mut buf[o..]);

                for (key, _conn) in connections_map.iter().filter(|(_, c)| c.is_connected()).choose_multiple(rng, PEERS_TO_ASK_PUNCH_FOR_RECENTS) {
                    if TRACE { println!("NewNet: Requesting hole punch to recent address {:?} via random peer: {:?}...", address, _conn.address()); }
                    packets_to_send.push((*key, Vec::from(&buf[..o])));
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
                if connections_map.contains_key(&address.connection_key()) {
                    continue;
                }

                match connect_to(socket, &mut connections_map, &my_keypairs, address) {
                    Err(e) => {
                        println!("NewNet: Alleged-address reconnect: connect_to failed: {e}");
                        continue;
                    }
                    Ok(()) => {},
                }

                connection_attempts += 1;

                // Hole punch
                let (mut buf, mut o) = ([0u8; 1 + STP_ADDRESS_SERIALIZED_SIZE], 0);
                o += PACKET_TYPE_WANT_HOLE_PUNCH .write_to(&mut buf[o..]);
                o += ConnectionKey::from(address).write_to(&mut buf[o..]);

                for (key, _conn) in connections_map.iter().filter(|(k, c)| c.is_connected() && *k != sender_connection_key).choose_multiple(rng, PEERS_TO_ASK_PUNCH_FOR_ALLEGEDS) {
                    if TRACE { println!("NewNet: Requesting hole punch to alleged address {:?} via random peer: {:?}...", address, _conn.address()); }
                    packets_to_send.push((*key, Vec::from(&buf[..o])));
                }

                if get_connected(&connections_map, &sender_connection_key).is_some() {
                    if TRACE { println!("NewNet: Requesting hole punch to alleged address {:?} via the original sender: {:?}...", address, sender_connection_key); }
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
            for (key, connection) in &connections_map {
                if !connection.is_connected() {
                    continue;
                }

                packets_to_send.push((*key, gossip_packet.clone()));
            }

            next_peer_gossip = std::time::Instant::now() + peer_gossip_interval;
        }


        blocks_to_send.sort_by_key(|(_, _, height)| *height);

        // TODO: rate limit production!
        for (connection_key, hash, height) in &blocks_to_send {
            let hash = *hash;
            let Some(connection) = get_connected(&connections_map, connection_key) else {
                if TRACE { println!("Disconnected!"); }
                continue;
            };

            if !serialized_blocks.contains_key(&hash) {
                let res = rt.block_on(async {
                    read_state.clone().oneshot(ReadRequest::Block(hash.into())).await
                });
                match res {
                    Ok(ReadResponse::Block(Some(block))) => {
                        const PACKET_BLOCK_HEADER_LEN: usize = 1 + 4 + 32;

                        let serialized = dbg_verify(block.zcash_serialize_to_vec().ok()).unwrap();
                        if serialized.len().saturating_add(PACKET_BLOCK_HEADER_LEN) >= MAX_JUMBOGRAM_LEN - 1 {
                            eprintln!("NewNet ERROR: Block too big! Was {:?} bytes, max is {}!", serialized.len(), MAX_JUMBOGRAM_LEN - 1);
                            continue;
                        }

                        let (mut tmp, mut o) = ([0u8; PACKET_BLOCK_HEADER_LEN], 0);
                        o += PACKET_TYPE_BLOCK                             .write_to(&mut tmp[o..]);
                        o += dbg_verify(block.coinbase_height()).unwrap().0.write_to(&mut tmp[o..]);
                        o += block.hash().0                                .write_to(&mut tmp[o..]);
                        assert!(o == PACKET_BLOCK_HEADER_LEN);

                        let mut buf = Vec::<u8>::with_capacity(PACKET_BLOCK_HEADER_LEN + serialized.len());

                        buf.extend(&tmp[..o]);
                        buf.extend(serialized);

                        serialized_blocks.insert(hash, buf);
                    }
                    Ok(ReadResponse::Block(None)) => {
                        // @Todo: This needs fixing!!! :SidechainSync
                        eprintln!("Couldn't get block for hash {hash}!");
                        continue;
                    }
                    Err(err) => { panic!("ReadRequest::Block({hash}): Error: {err:?}");              }
                    _        => { panic!("ReadRequest::Block({hash}): Unhandled response: {res:?}"); }
                }
            }

            if TRACE { println!("Hey. I'm sending a BLOCK. Height: {height}. Hash: {hash}."); }
            packets_to_send.push((*connection_key, serialized_blocks[&hash].clone()));
            // eprintln!("\x1b[93mPOWLINK2 SENDING BLOCK HASH\x1b[0m: {}", hash);
        }
        blocks_to_send.clear();

        // Invariant: near_tip_chains contains at least the genesis. Currently.
        assert!(near_tip_chains.tip_height().is_some());
        if std::time::Instant::now() >= next_status {
            let (mut buf, mut o) = ([0u8; PACKET_STATUS_MAX_SIZE], 0);
            o += PACKET_TYPE_STATUS.write_to(&mut buf[o..]);
            o += near_tip_chains   .write_to(&mut buf[o..]);

            for (key, connection) in &connections_map {
                if !connection.is_connected() {
                    if TRACE { println!("Disconnected!"); }
                    continue;
                }

                if TRACE { println!("Hey. I'm sending a STATUS."); }
                packets_to_send.push((*key, Vec::from(&buf[..o])));
            }

            next_status = std::time::Instant::now() + status_interval;
        }

        // Discard statuses of disconnected peers
        peers.retain(|connection_key, _| get_connected(&connections_map, connection_key).is_some());

        if std::time::Instant::now() >= next_block_send {
            'send_to_peers: for (connection_key, Peer { origin, their_tree }) in &peers {
                // @Duplicate with packet status parsing

                if *their_tree == NearTipBranches::default() {
                    continue 'send_to_peers; // No messages yet.
                }

                let mut blocks_to_this_peer = 0;
                // Skip now-disconnected peers
                let connection_address = {
                    let Some(connection) = get_connected(&connections_map, &connection_key) else {
                        eprintln!("Peer is disconnected even after we filtered out disconnected peers. Impossible!: {connection_key:?}");
                        continue 'send_to_peers;
                    };
                    connection.address()
                };

                macro_rules! warning {
                    ($($arg:tt)*) => {{
                        // let msg = format!("Peer {:?}: {}", connection_address, format!($($arg)*));
                        eprintln!("{}", format!("Peer {:?}: {}", connection_address, format!($($arg)*)).to_string());
                    }};
                }

                let Some(our_tip_height) = dbg_verify(near_tip_chains.tip_height())
                else {
                    warning!("I don't have a tip height yet");
                    continue 'send_to_peers;
                };

                // @Note: If the peer is far behind, beam them blocks from farther back in our chain to catch them up.
                // There's surely a (more expensive) Zebra lookup to check if their tip is inside our finalized chain.
                if their_tree.tip_height < near_tip_chains.finalized_height {

                    for height in their_tree.tip_height..near_tip_chains.finalized_height {
                        let res = rt.block_on(async {
                            read_state.clone().oneshot(ReadRequest::BestChainBlockHash(Height(height).into())).await
                        });
                        match res {
                            Ok(ReadResponse::BlockHash(hash)) => {
                                let Some(hash) = dbg_verify(hash) else {
                                    warning!("Couldn't get block for height {height} which should be finalized! What happened?");
                                    break;
                                };

                                if blocks_to_this_peer < MAX_BLOCKS_TO_QUEUE_TO_COMMIT {
                                    blocks_to_this_peer += 1;
                                    blocks_to_send.push((*connection_key, hash, height));
                                } else {
                                    break;
                                }
                            }
                            Err(err) => { panic!("ReadRequest::BestChainBlockHash({height}): Error: {err:?}");              }
                            _        => { panic!("ReadRequest::BestChainBlockHash({height}): Unhandled response: {res:?}"); }
                        }
                    }

                }

                // @Todo: If the peer is far ahead, request blocks from farther back in their chain to catch us up.
                if our_tip_height < their_tree.finalized_height {
                    warning!("Too far ahead of me");
                    continue 'send_to_peers;
                }

                // rule to push:
                //     per each of our chains:
                //         find the longest prefix branch they have with the chain
                //         send everything after the end of the prefix branch
                //     OR
                //     per each of our chains:
                //         if any of their branches is equal to or an extension of this chain, don't push
                //         otherwise push the blocks after the intersection
                //
                // rule to pull:
                // per each of their branches:
                //     per each of our chains:
                //         if any of our chains contain all of the branch, then break
                //         track the maximum height of the intersections
                //     request everything up from the maximum height all the way to their tip
                //

                // @Note: With this algorithm, another peer can systematically probe which
                //        blocks we have on each branch. This may have implications.
                let mut blocks_to_queue = HashSet::new();
                for our_chain in &near_tip_chains.chains {
                    assert!(our_chain.blocks.len() > 0);

                    let our_chain_height_bgn = our_chain.blocks[0].this_height;
                    let our_chain_height_end = our_chain.blocks.last().unwrap().this_height + 1;

                    let mut max_height_we_both_share = None;

                    for their_branch in &their_tree.branches {
                        let their_branch_height_bgn = their_branch[0].this_height;
                        let their_branch_height_end = their_branch.last().unwrap().this_height + 1;

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
                    }

                    let Some(max_height_we_both_share) = max_height_we_both_share else {
                        continue; // Nothing on this chain of ours that we can use to extend their branch.
                    };

                    for height in max_height_we_both_share + 1 .. our_chain_height_end {
                        let chain_i = (height - our_chain_height_bgn) as usize;

                        let block_to_queue = &our_chain.blocks[chain_i];

                        // We may submit the same block multiple times (visit >1 of our chains that share a short prefix with their branch), and that's valid
                        if blocks_to_this_peer < MAX_BLOCKS_TO_QUEUE_TO_COMMIT {
                            if blocks_to_queue.insert((block_to_queue.this_hash, height)) {
                                blocks_to_this_peer += 1;
                            }
                        } else {
                            break;
                        }
                    }
                }

                for (block, height) in blocks_to_queue {
                    blocks_to_send.push((*connection_key, block, height));
                }

                // @Todo: Pull.

            }

            next_block_send = std::time::Instant::now() + block_send_interval;
        }

        pending_selected_addresses.retain(|addr, time| std::time::Instant::now().saturating_duration_since(*time).as_secs() < 30);

        use rand::seq::SliceRandom;

        // @@@ @Todo @@@: We need speculative queueing ASAP!
        // packets_to_send.shuffle(&mut rand::thread_rng());

        let mut more_packets_likely_available = false;
        // Service STP connections (send/recv).
        // @Todo: real scheduling. Right now I just want to receive everything!
        for _ in 0..128 {
            more_packets_likely_available = service_connections(&mut connections_map,
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

            if !more_packets_likely_available || packets_received.len() > 0 {
                break;
            }
        }

        // // hypothetical: something "we may want" to "avoid pessimal drop behaviour"
        // packets_received.shuffle();

        // Process received packets
        'process_packets: while packets_received.len() > 0 {
            let (connection_key, msg) = packets_received.remove(0);
            // if TRACE { println!("got a message."); }

            // Skip processing packets from now-disconnected peers
            let connection_address = {
                let Some(connection) = get_connected(&connections_map, &connection_key) else {
                    eprintln!("Dropping message from disconnected peer: {connection_key:?}");
                    continue 'process_packets;
                };
                connection.address()
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
                    eprintln!("{}", format!("Peer {:?}: {}", connection_address, format!($($arg)*)).to_string());
                }};
            }
            macro_rules! kill {
                ($($arg:tt)*) => {{
                    // let msg = format!("Killing peer {:?}: {}", connection_address, format!($($arg)*));
                    eprintln!("{}", format!("Killing peer {:?}: {}", connection_address, format!($($arg)*)).to_string());
                    connections_map.remove(&connection_key);
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

            // if TRACE { println!("got message {packet_type}."); }

            if packet_type == PACKET_TYPE_PEER_ADDRESS_LIST {
                if TRACE { println!("Got message type: PEER_ADDRESS_LIST"); }
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

                    if TRACE { println!("Adding to alleged addresses: {address:?}"); }
                    if map.insert(address, connection_key).is_none() { // true if newly inserted
                        if map.len() >= MAX_ADDRESSES_PER_SENDER {
                            map.remove(&map.keys().choose(&mut rand::thread_rng()).cloned().unwrap());
                        }
                    }
                }

            } else if packet_type == PACKET_TYPE_WANT_HOLE_PUNCH {
                if TRACE { println!("Got message type: WANT_HOLE_PUNCH"); }
                // @Todo: rate limit consumption

                let Some(relay_to_connection_key) = some_or_kill!(ConnectionKey::read_from(&mut msg), "Connection key read failed") else {
                    continue 'process_packets;
                };

                if get_connected(&connections_map, &relay_to_connection_key).is_some() {
                    let (mut buf, mut o) = ([0u8; 1 + STP_ADDRESS_SERIALIZED_SIZE], 0);

                    o += PACKET_TYPE_TRY_HOLE_PUNCH.write_to(&mut buf[o..]);
                    o += connection_address        .write_to(&mut buf[o..]);

                    if TRACE { println!("NewNet: Relaying hole punch request from {:?} to {:?}...", connection_address, relay_to_connection_key); }
                    packets_to_send.push((relay_to_connection_key, Vec::from(&buf[..o])));
                }

            } else if packet_type == PACKET_TYPE_TRY_HOLE_PUNCH {
                if TRACE { println!("Got message type: TRY_HOLE_PUNCH"); }
                // @Todo: rate limit consumption

                let Some(address_to_punch_to) = some_or_kill!(STPAddress::read_from(&mut msg), "Address read failed") else {
                    continue 'process_packets;
                };

                if !connections_map.contains_key(&address_to_punch_to.connection_key()) {
                    if TRACE { println!("NewNet: Attempting hole punch to {:?}, requested by {:?}...", address_to_punch_to, connection_address); }
                    let _ = connect_to(socket, &mut connections_map, &my_keypairs, &address_to_punch_to);
                }

            } else if packet_type == PACKET_TYPE_STATUS {
                if TRACE { println!("Got message type: STATUS"); }
                // @Todo: rate limit consumption

                // TODO: rate limit consumption
                let Some(their_tree) = some_or_kill!(NearTipBranches::read_from(&mut msg), "NearTipBranches read failed")
                else {
                    continue 'process_packets;
                };

                peer.their_tree = their_tree;

            } else if packet_type == PACKET_TYPE_BLOCK {
                if TRACE { println!("Got message type: BLOCK"); }
                // @Todo: rate limit consumption

                let Some(our_tip_height) = dbg_verify(near_tip_chains.tip_height())
                else {
                    warning!("I don't have a tip height yet");
                    continue 'process_packets;
                };

                // @Todo: @Temporary. We will instead want to evict non-committable blocks and replace them with new blocks if they are committable.
                // if blocks_to_commit.len() >= MAX_BLOCKS_TO_QUEUE_TO_COMMIT {
                //     warning!("Block commit queue full! Dropping.");
                //     continue 'process_packets;
                // }

                // @Note: for valid blocks the height can be computed from block data, so this is an early-out optimization.
                let Some(alleged_height) = some_or_kill!(<u32>::read_from(&mut msg), "Failed to read block height") else {
                    continue 'process_packets;
                };

                // @Note: Skip blocks that are older than the base of our NearTipChain view of the best chain.
                // Depending on whether our NEAR_TIP_CHAIN_LEN is < or > Zebra's MAX_BLOCK_REORG_HEIGHT,
                // presence in the best NearTipChain *may* or *may not* logically imply that this new block
                // is "not even worth" submitting to Zebra (rejected due to being too far back).
                let min_height = near_tip_chains.chains[0].blocks[0].this_height; // @Todo: this assumes :AssumeGenesisBlockIncludedInNearTipChains

                if alleged_height < min_height {
                    warning!("Block at height {alleged_height} is below our near-tip-chain height {min_height}");
                    continue 'process_packets; // Deciding that it's "not even worth" sending to Zebra
                }

                if TRACE { println!(">= min_height."); }

                if alleged_height <= near_tip_chains.finalized_height {
                    warning!("Block at height {alleged_height} is already finalized");
                    continue 'process_packets; // Definitely already committed :)
                }

                if TRACE { println!("> finalized_height."); }

                // @Note: the hash could be computed from the block header, so this is an early-out optimization.
                let Some(alleged_hash) = some_or_kill!(<[u8; 32]>::read_from(&mut msg), "Failed to read block hash") else {
                    continue 'process_packets;
                };
                let alleged_hash = Hash(alleged_hash);

                if TRACE { println!("hash read."); }

                if blocks_to_commit.iter().any(|(queued_hash, _)| *queued_hash == alleged_hash) {
                    warning!("Block was already queued to commit!: {alleged_hash}");
                    continue 'process_packets;
                }

                if TRACE { println!("not already queued."); }

                for our_chain in &near_tip_chains.chains {
                    if our_chain.blocks.iter().any(|block| block.this_hash == alleged_hash) {
                        warning!("Block was already committed!: {alleged_hash}");
                        continue 'process_packets;
                    }
                }

                if TRACE { println!("not already in near_tip_chains."); }

                // @Volatile, depends on block header format.
                let parent_hash = {
                    let mut tmp = &msg[..];
                    let Some(version) = some_or_kill!(<u32>::read_from(&mut tmp), "Failed to read block version number") else {
                        continue 'process_packets;
                    };
                    let Some(parent_hash) = some_or_kill!(<[u8; 32]>::read_from(&mut tmp), "Failed to read parent hash") else {
                        continue 'process_packets;
                    };
                    Hash(parent_hash)
                };

                let have_parent_in_chains           = is_parent_in_chains(&rt, &state, &near_tip_chains, parent_hash);
                let have_parent_in_blocks_to_commit = blocks_to_commit.iter().any(|(queued_hash, _)| *queued_hash == parent_hash);

                if !have_parent_in_chains && !have_parent_in_blocks_to_commit {
                    warning!("Block does not link anywhere known, neither to our chains nor to our blocks-to-commit queue! Not queueing; dropping: height {alleged_height} hash {alleged_hash}");
                    continue 'process_packets;
                }

                if TRACE { println!("hash and height."); }

                use zebra_chain::serialization::ZcashDeserializeInto;
                let Some(block) = some_or_kill!(msg.zcash_deserialize_into::<Block>().ok(), "Failed to deserialize block") else {
                    continue 'process_packets;
                };

                // @Todo: - safely reorganize checks in block verification that are quite cheap to the top of the function
                //        - turn that into a function
                //        - call it at the top of the place where they are currently used
                //        - call that right here

                if TRACE { println!("deserialized."); }

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

                eprintln!("\x1b[93mPOWLINK2 GOT BLOCK HASH\x1b[0m: {}", hash);

                if TRACE { println!("hash and height."); }

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
                        && is_tail(i) && !is_parent_in_chains(&rt, &state, &near_tip_chains, blocks_to_commit[i].1.header.previous_block_hash)
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
                if TRACE { println!("Added to recent addresses: {connection_address:?}"); }
                recents_bucket.insert(connection_address.clone(), RecentPeerAddress {
                    recv_time_ns: monotonic_clock_ns(),
                    failure_count: 0u32, // @Todo: Implement this counter!
                });
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

            if !is_parent_in_chains(&rt, &state, &near_tip_chains, parent_hash) {
                return true; // keep
            }

            any_blocks_in_the_queue_can_make_progress = true;

            println!("Committing: {}", hash);
            let res = rt.block_on(commit_block(block_arc));
            match res {
                Ok(_) => {
                    println!("committed!: {}", hash);
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

        if !more_packets_likely_available {
            // Sleep remainder of tick
            let elapsed = loop_start.elapsed();
            if elapsed < tick_duration {
                std::thread::sleep(tick_duration - elapsed);
            }
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

        let mut chains = NearTipChains { chains: Vec::new() };

        // (a, b), (b, c,), (c, d), (d, e), ..., (c, i), (i, j), ...
        // a b c d e f g h
        //     \ i j k l m n
        chains.push_blocks(&blocks);

        let mut chains2 = NearTipChains { chains: Vec::new() };
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

