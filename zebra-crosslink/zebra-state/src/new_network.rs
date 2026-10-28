//! Crosslink peer sync.
//!
//! The types and the tick loop here are documented by the comments next to them. The
//! crate-level `missing_docs` lint is silenced for this module rather than repeated on
//! every field of the packet and peer structs.
//!
//! `dbg_panic!` and `kill!` abort only in debug builds. The statement after them is the
//! release path, so it is not dead code.
#![allow(missing_docs, unreachable_code)]

const DUMP_NEAR_TIP_CHAINS: bool = 1 == 0;

use std::collections::{HashMap, HashSet};
use static_assertions::const_assert;

use zebra_chain::block::{self, Block, Hash, Height};
use zebra_chain::serialization::ZcashSerialize;

use tenderlink::stp::*;
use tenderlink::native_sockets::*;
use tenderlink::transport::{new_network_thread, service_connections, Channel, NetworkThreadPush};
use tenderlink::{SliceWrite, SliceRead};
use tenderlink::{dbg_panic, dbg_verify};

mod checkpoint;
pub mod bft;
pub mod fin;
use checkpoint::Checkpoint;

// ---------------------------------------------------------------------------
// NewNet packet types // @Todo: share common messages/code with Tenderlink
// ---------------------------------------------------------------------------

const JUMBO_FRAG_SIZE: usize = max_single_datagram_message_size(CRYPTO_MAGIC).unwrap();

// STP 2.0 replaced the magic1 selectors, so 1.x (11Q8bpGo) and 0.x (1fgEw5Nx) peer addresses
// are refused at config parse below and by the handshake; this one prints as -fCmrBqp.
const CRYPTO_MAGIC: u64 = CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2b;
//const CRYPTO_MAGIC: u64 = CONNECT_MAGIC1_PLAIN_TEXT;

// @Todo: more of these, merge with Tenderlink, etc.
const PACKET_TYPE_STATUS:            u8 = 1;
const PACKET_TYPE_BLOCK_CHUNK:       u8 = 2;
const PACKET_TYPE_BLOCK_REQ:         u8 = 3;
const PACKET_TYPE_PEER_ADDRESS_LIST: u8 = 4;
const PACKET_TYPE_WANT_HOLE_PUNCH:   u8 = 5;
const PACKET_TYPE_TRY_HOLE_PUNCH:    u8 = 6;
const PACKET_TYPE_COUNT:             u8 = 7;

#[allow(dead_code)] // looked up by the commented packet-type log in the recv loop
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
#[allow(dead_code)] // looked up by the commented packet-type log in the recv loop
fn packet_name_from_type(packet_type: u8) -> &'static str {
    let string_maybe = PACKET_TYPE_NAMES.get(packet_type as usize);
    let string       = string_maybe.unwrap_or(&"<UNKNOWN>");
    string
}


const PACKET_STATUS_MAX_HASHES: usize = 400; // @Lazy: gives room for 300 hashes plus room for 200 run metadatas
const PACKET_STATUS_MAX_SIZE:   usize = ((PACKET_STATUS_MAX_HASHES * 32 + JUMBO_FRAG_SIZE - 1) / JUMBO_FRAG_SIZE) * JUMBO_FRAG_SIZE; // @Cleanup @Lazy.

const DOWNLOAD_UNMODIFIED_TIMEOUT_DUR: std::time::Duration = std::time::Duration::from_secs(8);

// How long a block a peer served us that failed verification goes unrequested. The wait doubles
// with each repeat failure of the same hash, up to the max.
const REJECTED_BLOCK_BACKOFF_MIN: std::time::Duration = std::time::Duration::from_secs(1);
const REJECTED_BLOCK_BACKOFF_MAX: std::time::Duration = std::time::Duration::from_secs(64);

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

/// Parallel chain branches, as they go on the wire in a STATUS packet.
///
/// Transient: built on demand by `near_tip_chains_from_state()` out of the NonFinalizedState
/// plus a tail of the finalized best chain, and dropped again the same tick. Nothing maintains
/// it incrementally, so it cannot drift from what the node actually has.
///
/// Not the same shape as NonFinalizedState: that one is rooted at the finalized tip, and this
/// deliberately reaches past it so peers that are behind us have something to attach to.
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
        let hdr_len = PacketHashTreeHdr::default()            .write_to(&mut buf[..]);
        let run_len = PacketHashBranch ::default()            .write_to(&mut buf[..]);
        let hgt_len = ShadowBlock      ::default().this_height.write_to(&mut buf[..]);
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

            let chain_idx = if let Some((chain_idx, parent_idx)) = found {
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
            let disconnected = usize::from(branch.parent_hash_idx) == hash_c;

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
        let hashes_start = hdr.hashes_start_offset as usize;
        if hashes_start < hdr_len || hashes_start > full_buf.len() {
            println!("received invalid Hash Tree packet (hashes start offset)");
            dbg_panic!("received invalid Hash Tree packet (hashes start offset)");
            return None;
        }
        *buf = &buf[..hashes_start - hdr_len];
        let buf_hashes: &mut &[u8] = &mut &full_buf[hashes_start..];
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

            let _end_height = dbg_verify(bgn_height.checked_add((end_i - hash_c) as u32))?;

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
        let _their_branch_height_bgn = their_branch[0].this_height;
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

const STATUS_MS: u64 = 4000;  // fastest interval at which to send status messages
const BLOCK_SEND_MS: u64 = 500; // fastest interval at which to send blocks to peers
const ATTESTED_PUBLISH_MS: u64 = 2000; // interval at which to publish PEER_ATTESTED_BLOCKS (display-facing; prime so it stays off the other timers)


const MAX_PEERS_TO_CONNECT_PER_ATTEMPT: usize = 2;
const PEER_CONNECT_MS: u64 = 6000;
const PEER_GOSSIP_MS:  u64 = 2500;
const PEER_REQUEST_MS: u64 = 300;
const PEER_RESPOND_MS: u64 = 300;


const IDLE_MS: u64 = 100;

const SYNC_PROFILE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);

/// What the lowest queued block did in one pass of the commit loop: the block every later one
/// waits behind, so the reason it did not commit is the reason the sync did not advance.
#[derive(Clone, Copy)]
enum CommitHead {
    Committed,
    ParentMissing,
    /// `admit_fat_pointer` could not resolve the BFT block the PoW block cites.
    BftGate,
    DeferredOther,
    Failed,
}

/// Where the sync's time goes, counted per tick and logged every `SYNC_PROFILE_INTERVAL` as one
/// `key=value` line, so a run's log can be read back into a table.
struct SyncProfile {
    since: std::time::Instant,
    ticks: u64,
    committed: u64,
    ticks_committing: u64,
    ticks_queue_empty: u64,
    ticks_parent_missing: u64,
    ticks_bft_gate: u64,
    ticks_deferred_other: u64,
    ticks_failed: u64,
    ticks_bft_parked: u64,
    commit_time: std::time::Duration,
}

impl SyncProfile {
    fn new() -> Self {
        Self {
            since: std::time::Instant::now(),
            ticks: 0,
            committed: 0,
            ticks_committing: 0,
            ticks_queue_empty: 0,
            ticks_parent_missing: 0,
            ticks_bft_gate: 0,
            ticks_deferred_other: 0,
            ticks_failed: 0,
            ticks_bft_parked: 0,
            commit_time: std::time::Duration::ZERO,
        }
    }

    fn record_tick(&mut self, head: Option<CommitHead>, committed: u64, commit_time: std::time::Duration, bft_parked: bool) {
        self.ticks += 1;
        self.committed += committed;
        self.commit_time += commit_time;
        self.ticks_bft_parked += u64::from(bft_parked);
        if committed > 0 {
            self.ticks_committing += 1;
            return;
        }
        match head {
            None => self.ticks_queue_empty += 1,
            Some(CommitHead::Committed) => self.ticks_committing += 1,
            Some(CommitHead::ParentMissing) => self.ticks_parent_missing += 1,
            Some(CommitHead::BftGate) => self.ticks_bft_gate += 1,
            Some(CommitHead::DeferredOther) => self.ticks_deferred_other += 1,
            Some(CommitHead::Failed) => self.ticks_failed += 1,
        }
    }

    fn report_if_due(&mut self, tip_height: u32, bft_height: usize, queued: usize, downloads: u32, peers: usize) {
        let elapsed = self.since.elapsed();
        if elapsed < SYNC_PROFILE_INTERVAL {
            return;
        }
        tracing::info!(
            "NewNet: sync profile: seconds={:.1} tip={tip_height} bft_height={bft_height} committed={} blocks_per_s={:.2} \
             ticks={} committing={} queue_empty={} parent_missing={} bft_gate={} deferred_other={} failed={} \
             bft_parked={} commit_ms={} queued={queued} downloads={downloads} peers={peers}",
            elapsed.as_secs_f64(),
            self.committed,
            self.committed as f64 / elapsed.as_secs_f64(),
            self.ticks,
            self.ticks_committing,
            self.ticks_queue_empty,
            self.ticks_parent_missing,
            self.ticks_bft_gate,
            self.ticks_deferred_other,
            self.ticks_failed,
            self.ticks_bft_parked,
            self.commit_time.as_millis(),
        );
        *self = Self::new();
    }
}

const MAX_BANDWIDTH_BYTES_PER_MS: usize = 7_000; // 7 MB/s
const MAX_BANDWIDTH_BYTES_PER_RES: usize = MAX_BANDWIDTH_BYTES_PER_MS * PEER_RESPOND_MS as usize;
// x2: the budget sizes every in-flight block at MAX_BLOCK_BYTES, but real blocks are a small
// fraction of that, so the unscaled count leaves most of the bandwidth budget unused.
const MAX_BANDWIDTH_BLOCKS_PER_RES: usize = 2 * (MAX_BANDWIDTH_BYTES_PER_RES / zebra_chain::block::MAX_BLOCK_BYTES as usize);
/// The share of that budget by-hash requests for BFT may hold. Honest validation needs a handful
/// of hashes, so this binds only when proposals name blocks that do not exist.
const MAX_BFT_BLOCKS_PER_RES: usize = MAX_BANDWIDTH_BLOCKS_PER_RES / 4;


// use crate::crosslink::TFLServiceRequest;
// use crate::crosslink::TFLServiceResponse;
// use crate::crosslink::TFLServiceError;

type ReadState = crate::service::ReadStateService;
// type TFLService = tower::buffer::Buffer<tower::util::BoxService<TFLServiceRequest, TFLServiceResponse, TFLServiceError>, TFLServiceRequest>;

pub fn get_tips_blocking(read_state: &ReadState) -> ((Height, Hash), (Height, Hash)) {
    loop {
        let (Some(tip), Some(finalized_tip)) = (read_state.best_tip(), read_state.finalized_tip())
        else {
            std::thread::sleep(std::time::Duration::from_millis(1));
            continue;
        };
        break (tip, finalized_tip)
    }
}

/// The heights we advertise: the last [`NEAR_TIP_CHAIN_LEN`] ending at `tip_h`.
fn near_tip_start_height(tip_h: u32) -> u32 {
    tip_h.saturating_sub(NEAR_TIP_CHAIN_LEN - 1)
}

/// Materialize one run of blocks over `[bgn_h ..= tip_h]`.
///
/// `in_chain` answers for the non-finalized part. Anything it declines comes from the finalized
/// state, which is the shared ancestry of every non-finalized chain.
fn shadow_run(
    db: &crate::service::finalized_state::ZebraDb,
    bgn_h: u32,
    tip_h: u32,
    in_chain: impl Fn(u32) -> Option<Hash>,
) -> Option<Vec<ShadowBlock>> {
    let hash_at = |h: u32| in_chain(h).or_else(|| db.hash(Height(h)));

    let mut parent_hash = if bgn_h == 0 { Hash([0; 32]) } else { hash_at(bgn_h - 1)? };

    let mut blocks = Vec::with_capacity((tip_h + 1 - bgn_h) as usize);
    for this_height in bgn_h..=tip_h {
        let this_hash = hash_at(this_height)?;
        blocks.push(ShadowBlock { parent_hash, this_hash, this_height });
        parent_hash = this_hash;
    }
    Some(blocks)
}

/// Build the near-tip chain tree from the state, rather than mirroring it.
///
/// The non-finalized state alone isn't enough. It is rooted at the finalized tip and is empty
/// on a fresh node, but a peer that is behind us needs a finalized tail to find an attach
/// point in. So every chain is extended backwards along the finalized best chain until the run
/// starts at the same height for all of them.
fn near_tip_chains_from_state(read_state: &ReadState) -> Option<NearTipChains> {
    let db = read_state.db_clone();
    let (finalized_height, _) = db.tip()?;

    read_state.with_non_finalized_state(|nfs| {
        let best_tip_h = match nfs.best_chain() {
            Some(chain) => chain.non_finalized_tip_height().0,
            None => finalized_height.0,
        };
        let bgn_h = near_tip_start_height(best_tip_h);

        let mut out = NearTipChains { finalized_height: finalized_height.0, chains: Vec::new() };

        // chain_iter() is best-first by real cumulative work, and we don't re-sort, so branch 0
        // of the status packet is the chain we actually consider best.
        for chain in nfs.chain_iter() {
            let tip_h  = chain.non_finalized_tip_height().0;
            let root_h = chain.non_finalized_root_height().0;
            if tip_h < bgn_h {
                continue; // aged out from under the window
            }
            let in_chain = |h| if h >= root_h { chain.hash_by_height(Height(h)) } else { None };
            if let Some(blocks) = shadow_run(&db, bgn_h, tip_h, in_chain) {
                out.push_chain_unchecked(blocks);
            }
        }

        if out.chains.is_empty() {
            // Nothing non-finalized: the finalized best chain is the only chain there is.
            out.push_chain_unchecked(shadow_run(&db, bgn_h, best_tip_h, |_| None)?);
        }

        debug_assert!(out.finalized_height <= out.tip_height().unwrap_or(0));
        Some(out)
    })
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

/// How many queued block hashes we advertise in a STATUS packet.
///
/// @Volatile: this is WIRE FORMAT. It sizes the status buffer and peers reject a status that
/// claims more than this. Changing it is a protocol change, not a tuning knob -- which is why
/// the local cache below is a separate limit.
const MAX_BLOCKS_TO_QUEUE_TO_COMMIT: usize = 10;

/// How many blocks to hold locally while they wait for something.
///
/// Two things make a block wait, and neither means it is invalid:
///   - its parent has not been committed yet (arrived out of order), or
///   - the crosslink gate cannot resolve its BFT pointer yet.
///
/// Both resolve on their own as the chain advances. Without this cache we drop the block and
/// re-download it, which on a fresh sync cost ~2 redundant downloads per commit. Holding it
/// costs memory and nothing else.
///
/// Every block still gets a commit attempt in the tick it arrives -- the cache is only where
/// it lands after that attempt defers.
const MAX_DEFERRED_BLOCKS: usize = 256;

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
    reassembly: BlockReassembly, // accumulates block fragments across (re)requests
    last_modified: std::time::Instant,
    timeout_strikes: u8, // quiet periods survived; fragments reset it (see dl-init timeout)
    last_requested: Option<std::time::Instant>, // when the last BLOCK_REQ for this slot went out
    request_attempts: u8, // BLOCK_REQs sent for this slot; sets the retry backoff
    // When the slot's FIRST BLOCK_REQ went out. Anchors the response deadline (see
    // `block_download_expired`); set once per slot generation and never moved by a retry, a
    // fragment or any other traffic from the peer.
    first_requested: Option<std::time::Instant>,
}
impl Default for PeerPowBlockDownload {
    fn default() -> Self {
        Self {
            height_hash: HeightAndHashOr0::default(),
            reassembly: BlockReassembly::default(),
            last_modified: std::time::Instant::now(),
            timeout_strikes: 0,
            last_requested: None,
            request_attempts: 0,
            first_requested: None,
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

const BLOCK_DOWNLOADS_N: usize = 32;

/// Largest block payload one BLOCK_CHUNK message carries. It is a whole consensus-max block,
/// so a block answers a request in one STP message; the offset/size header and reassembly
/// stay so a request can still resume from a prefix.
const BLOCK_RESPONSE_MAX_PAYLOAD: usize = zebra_chain::block::MAX_BLOCK_BYTES as usize;
/// Largest BLOCK_CHUNK message: payload plus type byte and chunk header.
const BLOCK_RESPONSE_MAX_MESSAGE: usize = BLOCK_RESPONSE_MAX_PAYLOAD + 1 + 4 + 4 + 4 + 32;
const_assert!(BLOCK_RESPONSE_MAX_MESSAGE <= tenderlink::transport::MAX_MESSAGE_SIZE);

/// First BLOCK_REQ retry for a slot that has seen no data, doubling per attempt up to the cap.
/// STP is best-effort: a whole-block message can be lost, and only a retry recovers it. The
/// backoff keeps a slow link from being sent the same block again every request tick.
const BLOCK_REQUEST_RETRY_BASE: std::time::Duration = std::time::Duration::from_millis(1000);
const BLOCK_REQUEST_RETRY_MAX:  std::time::Duration = std::time::Duration::from_millis(4000);

fn block_request_retry_delay(attempts: u8) -> std::time::Duration {
    BLOCK_REQUEST_RETRY_BASE
        .saturating_mul(1u32 << attempts.saturating_sub(1).min(8))
        .min(BLOCK_REQUEST_RETRY_MAX)
}

/// ASSUMED floor on the useful payload rate of one peer's Bulk flow to us. It is a design
/// assumption that sizes the response deadline, NOT a network guarantee: a link slower than
/// this can still lose a slot that was making progress.
const BLOCK_MIN_GOODPUT_BYTES_PER_SEC: u64 = 64 * 1024;
/// Added to the drain time for round trips, the request/respond/dl-init tick cadences and
/// decoding on both ends.
const BLOCK_RESPONSE_DEADLINE_ALLOWANCE: std::time::Duration = std::time::Duration::from_secs(8);

/// How long a requested download slot waits for its whole block, counted from the slot's
/// first BLOCK_REQ.
///
/// STP delivers only complete messages and reports no per-message receive progress, so a
/// block that is still arriving looks exactly like one that is lost. The responder admits a
/// response only while its Bulk backlog to us (outbox, send ring and STP queue: everything
/// not yet transmitted, including checkpoint-probe and stale responses queued earlier) plus
/// the response fits `peer_bulk_cap_bytes` (see `send_budget` and `admit_block_response`), so
/// at most that many bytes are ahead of and including our block. At the assumed floor rate
/// they drain in `cap / BLOCK_MIN_GOODPUT_BYTES_PER_SEC`. With the default 8 MiB cap this is
/// 128s + 8s = 136s.
///
/// Limit: the bound assumes the response is admitted at our first request. A response the
/// responder deferred (backlog full) and admitted on a later retry starts its drain later.
fn block_response_deadline(peer_bulk_cap_bytes: u64) -> std::time::Duration {
    let drain_ms = peer_bulk_cap_bytes.saturating_mul(1000) / BLOCK_MIN_GOODPUT_BYTES_PER_SEC;
    std::time::Duration::from_millis(drain_ms) + BLOCK_RESPONSE_DEADLINE_ALLOWANCE
}

/// Whether the download in `dl` should be cancelled at `now`.
///
/// - A slot whose BLOCK_REQ never went out keeps the old rule: `DOWNLOAD_UNMODIFIED_TIMEOUT_DUR`
///   without change, one strike, then the same again (16s).
/// - A requested slot lives until `first_requested + deadline` (inclusive) and is cancelled
///   after it. Nothing renews it: retries, STATUS or other packets from the peer, and partial
///   fragments all leave `first_requested` alone, so a resumed download cannot extend forever.
///   A slot is re-anchored only when it is refilled (`BlockDownloads::fill_slot`).
fn block_download_expired(dl: &mut PeerPowBlockDownload, now: std::time::Instant, deadline: std::time::Duration) -> bool {
    if let Some(first_requested) = dl.first_requested {
        return now.saturating_duration_since(first_requested) > deadline;
    }
    if now.saturating_duration_since(dl.last_modified) > DOWNLOAD_UNMODIFIED_TIMEOUT_DUR {
        if dl.timeout_strikes == 0 {
            // one grace period before cancelling: cancelling throws away the partial
            // reassembly, and a slow tick can strand already-arrived fragments in the network
            // thread until *after* this pass runs. any fragment resets the strike (see
            // BLOCK_CHUNK), so only a genuinely dead download gets cancelled.
            dl.timeout_strikes = 1;
            dl.last_modified = now;
        } else {
            return true;
        }
    }
    false
}

/// Bookkeeping for a fragment that landed in `dl`. It resets only the never-requested rule's
/// quiet timer and strike. It moves neither the deadline anchor (`first_requested`) nor the
/// retry schedule: a suffix still missing is asked for at the next scheduled retry, so a peer
/// feeding partial fragments can neither extend the slot nor make us re-ask faster than the
/// 1/2/4/4s backoff.
fn note_block_fragment(dl: &mut PeerPowBlockDownload, now: std::time::Instant) {
    dl.last_modified = now;
    dl.timeout_strikes = 0;
}

/// Whether a BLOCK_REQ for `dl` goes out at `now`; if so, records it. Retries back off
/// 1/2/4/4...s (`block_request_retry_delay`) and stop once the slot is past its deadline,
/// so one slot sends at most about deadline/4s + 4 requests (36 for the default 136s).
fn block_request_due(dl: &mut PeerPowBlockDownload, now: std::time::Instant, deadline: std::time::Duration) -> bool {
    if dl.first_requested.is_some_and(|first| now.saturating_duration_since(first) > deadline) {
        return false;
    }
    // Retry with backoff: a block answers in one message, so re-asking every request tick
    // would only resend it while it is still in flight.
    if dl.last_requested.is_some_and(|at| now.saturating_duration_since(at) < block_request_retry_delay(dl.request_attempts)) {
        return false;
    }
    dl.first_requested.get_or_insert(now);
    dl.last_requested = Some(now);
    dl.request_attempts = dl.request_attempts.saturating_add(1);
    true
}

/// Most distinct (hash, offset) responses remembered per peer while its Bulk backlog is
/// undrained. Past it, new responses to that peer wait (the requester retries).
/// A requester may have up to `MAX_BANDWIDTH_BLOCKS_PER_RES` downloads active, so a burst
/// beyond the cap waits a drain: bounded batching, traded for a fixed-size record.
const MAX_QUEUED_BLOCK_RESPONSES_PER_PEER: usize = 32;

/// Responder side: block responses queued to one peer on the Bulk channel since its Bulk
/// backlog was last seen empty. A request for one of them is a retry of a response still in
/// flight; answering it again would queue a second copy (up to `BLOCK_RESPONSE_MAX_MESSAGE`)
/// behind the first. The record is forgotten once the backlog drains, so a response that was
/// lost on the wire can be sent again on the next retry. Lives in `Peer`, so it is freed with the peer.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct QueuedBlockResponses {
    queued: Vec<(Hash, u32)>, // at most MAX_QUEUED_BLOCK_RESPONSES_PER_PEER
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResponseAdmission {
    Admit,
    /// already queued and not yet known transmitted
    Duplicate,
    /// the per-peer record is full until the backlog drains
    RecordFull,
    /// the peer's Bulk budget cannot take `reserved + needed` more bytes
    NoBudget,
}

impl QueuedBlockResponses {
    /// Call once per tick, before any response is reserved this tick and after the previous
    /// tick's responses reached the transport (`service`), with that peer's Bulk backlog.
    /// `None` (a synthetic peer, whose packets are handed over at once) also counts as drained.
    fn forget_if_drained(&mut self, bulk_backlog_bytes: Option<u64>) {
        if bulk_backlog_bytes.is_none_or(|bytes| bytes == 0) {
            self.queued.clear();
        }
    }

    fn record(&mut self, hash: Hash, offset: u32) {
        debug_assert!(self.queued.len() < MAX_QUEUED_BLOCK_RESPONSES_PER_PEER);
        self.queued.push((hash, offset));
    }
}

/// Whether to answer a request for `(hash, offset)` with `needed` bytes, given what this tick
/// already reserved for the same peer. `budget` is the peer's `send_budget(Bulk)`, or `None`
/// for a synthetic peer, which is served around STP and so has no budget.
fn admit_block_response(queued: &QueuedBlockResponses, hash: Hash, offset: u32, budget: Option<u64>, reserved: u64, needed: u64) -> ResponseAdmission {
    if queued.queued.contains(&(hash, offset)) {
        return ResponseAdmission::Duplicate;
    }
    if queued.queued.len() >= MAX_QUEUED_BLOCK_RESPONSES_PER_PEER {
        return ResponseAdmission::RecordFull;
    }
    if budget.is_some_and(|budget| budget < reserved.saturating_add(needed)) {
        return ResponseAdmission::NoBudget;
    }
    ResponseAdmission::Admit
}

/// Reassembles one block from BLOCK_CHUNK fragments. The buffer never grows past the
/// fragment ends the caller admits, and the caller bounds those by the declared size, which
/// is itself bounded by `MAX_BLOCK_BYTES`.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BlockReassembly {
    pub buf: Vec<u8>,
    pub total_len: Option<u32>,
    pub received: Vec<(u32, u32)>, // sorted non-overlapping non-adjacent ranges: [start, end)
}

impl BlockReassembly {
    /// returns (success, is_complete)
    pub fn insert(&mut self, offset: usize, data: &[u8], is_fin: bool) -> (bool, bool) {
        let start = offset as u32;
        let end = (offset + data.len()) as u32;

        // If this is the final fragment, derive total_len from offset + data.len()
        if is_fin {
            if let Some(existing) = self.total_len {
                if existing != end { return (false, false); }
            } else {
                if let Some(&(_, last_end)) = self.received.last() {
                    if last_end > end { return (false, false); }
                }
                self.total_len = Some(end);
                self.buf.resize(end as usize, 0);
            }
        }

        // Empty fragment: no data to insert, bounds check offset then check completion
        if data.is_empty() {
            if let Some(tl) = self.total_len {
                if start > tl { return (false, false); }
            }
            return (true, self.is_complete());
        }

        if let Some(tl) = self.total_len {
            if start >= tl || end > tl { return (false, false); }
        }

        if end as usize > self.buf.len() {
            self.buf.resize(end as usize, 0);
        }

        // Find all ranges that overlap or are adjacent to [start, end)
        let first = self.received.partition_point(|&(_, e)| e < start);
        let last = self.received.partition_point(|&(s, _)| s <= end);

        // Verify overlapping bytes match
        for &(rs, re) in &self.received[first..last] {
            let ov_start = start.max(rs) as usize;
            let ov_end = end.min(re) as usize;
            if ov_start < ov_end {
                let data_off = ov_start - offset;
                if self.buf[ov_start..ov_end] != data[data_off..data_off + (ov_end - ov_start)] {
                    return (false, false);
                }
            }
        }

        self.buf[offset..offset + data.len()].copy_from_slice(data);

        let merged_start = if first < last { self.received[first].0.min(start) } else { start };
        let merged_end = if first < last { self.received[last - 1].1.max(end) } else { end };
        self.received.splice(first..last, std::iter::once((merged_start, merged_end)));

        (true, self.is_complete())
    }

    fn is_complete(&self) -> bool {
        self.total_len.is_some_and(|tl| self.received.len() == 1 && self.received[0] == (0, tl))
    }
}

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
                reassembly: BlockReassembly::default(),
                last_modified: std::time::Instant::now(),
                timeout_strikes: 0,
                last_requested: None,
                request_attempts: 0,
                first_requested: None,
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

    #[allow(dead_code)]
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
    #[allow(dead_code)]
    fn prospective_position_or_end(&self, height_hash: HeightAndHashOr0) -> usize {
        self.position(height_hash).unwrap_or_else(|| {
            self.used_flags.trailing_ones() as usize
        })
    }

    fn slot_is_used(&self, dl_i: usize) -> bool {
        dl_i < self.slots.len() && ((self.used_flags >> dl_i) & 1) != 0
    }

    #[allow(dead_code)]
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

    // has served us the checkpoint block (see the checkpoint gate in dl-init)
    has_checkpoint_block: bool,

    // block responses queued to this peer and not yet known transmitted (duplicate guard)
    queued_block_responses: QueuedBlockResponses,

    // their last STATUS showed them wholly below our near-tip window: send them the next
    // historical window on the next tick rather than at the heartbeat
    history_wanted: bool,
}

#[derive(Debug)]
pub enum BlockCommitError {
    Duplicate,
    Other(String),
}

// ---------------------------------------------------------------------------
// Synchronous verification: types shared with zebra-consensus.
//
// These live here, not in zebra-consensus, because of the crate dependency direction:
// zebra-consensus depends on zebra-state, so it can name these, but zebra-state can NOT
// name anything in zebra-consensus. Defining them here is what lets `sync()` hold plain
// `fn` pointers to the verification functions instead of boxed closures.
// ---------------------------------------------------------------------------

/// Values derived while running the cheap block checks.
///
/// Returned rather than recomputed: `transaction_hashes` costs a hash of every transaction
/// in the block, and the expensive pass needs it again for sighashing.
#[derive(Clone, Debug)]
pub struct CheapBlockChecks {
    pub hash: Hash,
    pub height: Height,
    pub transaction_hashes: std::sync::Arc<[zebra_chain::transaction::Hash]>,
    pub coinbase_tx: std::sync::Arc<zebra_chain::transaction::Transaction>,
    pub expected_block_subsidy: zebra_chain::amount::Amount<zebra_chain::amount::NonNegative>,
    pub deferred_pool_balance_change: zebra_chain::amount::DeferredPoolBalanceChange,
}

/// A block verification failure.
///
/// `misbehavior_score` mirrors `zebra_consensus::RouterError::misbehavior_score()`, so the
/// caller can weight or drop a peer without needing to name the consensus error types.
#[derive(Clone, Debug)]
pub struct BlockVerifyError {
    pub msg: String,
    pub misbehavior_score: u32,
}

/// The synchronous verification entry points, injected from zebrad.
///
/// Plain `fn` pointers, not closures: these functions capture nothing, so there is no
/// allocation and no dynamic dispatch.
#[derive(Clone, Copy)]
pub struct VerifyFns {
    /// Header-only: PoW, difficulty, header time. Runs before the body is trusted.
    pub check_header: fn(
        &zebra_chain::block::Header,
        &zebra_chain::parameters::Network,
        Height,
        chrono::DateTime<chrono::Utc>,
        bool,
    ) -> Result<(), BlockVerifyError>,

    /// Body: binds the transactions to the header (merkle), then the subsidy rules.
    /// Must run before anything trusts the height, including the crosslink gate.
    /// Takes the height `check_header` ran at, and rejects a block whose coinbase disagrees.
    pub check_body: fn(
        &Block,
        &zebra_chain::parameters::Network,
        Height,
    ) -> Result<CheapBlockChecks, BlockVerifyError>,

    pub check_cheap: fn(
        &Block,
        &zebra_chain::parameters::Network,
        chrono::DateTime<chrono::Utc>,
        bool,
    ) -> Result<CheapBlockChecks, BlockVerifyError>,

    pub verify_expensive: fn(
        &Block,
        &zebra_chain::parameters::Network,
        &CheapBlockChecks,
        &dyn Fn(&zebra_chain::transparent::OutPoint) -> Option<zebra_chain::transparent::Utxo>,
    ) -> Result<
        std::collections::HashMap<
            zebra_chain::transparent::OutPoint,
            zebra_chain::transparent::OrderedUtxo,
        >,
        BlockVerifyError,
    >,
}

/// Wait for CPU-only work, joining it unless shutdown discards the result.
///
/// The worker must own only immutable verification inputs: no database handles,
/// writer, chain/BFT locks, or runtime references. Dropping its handle on shutdown
/// leaves at most one read-only proof/key calculation until process exit, just as
/// the process-lifetime Rayon workers do. Database writes are never run here.
fn await_cpu_verification<T>(
    receiver: std::sync::mpsc::Receiver<T>,
    worker: std::thread::JoinHandle<()>,
    should_stop: impl Fn() -> bool,
) -> Option<T> {
    loop {
        if should_stop() { return None; }
        match receiver.recv_timeout(std::time::Duration::from_millis(5)) {
            Ok(result) => {
                if let Err(panic) = worker.join() { std::panic::resume_unwind(panic); }
                return Some(result);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {},
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                if let Err(panic) = worker.join() { std::panic::resume_unwind(panic); }
                panic!("verification worker exited without sending a result");
            }
        }
    }
}

fn verify_expensive_cancellable(
    verify_fns: VerifyFns,
    block: std::sync::Arc<Block>,
    network: zebra_chain::parameters::Network,
    cheap: CheapBlockChecks,
    spent_utxos: std::collections::HashMap<zebra_chain::transparent::OutPoint, zebra_chain::transparent::Utxo>,
) -> Option<Result<
    std::collections::HashMap<zebra_chain::transparent::OutPoint, zebra_chain::transparent::OrderedUtxo>,
    BlockVerifyError,
>> {
    let (sender, receiver) = std::sync::mpsc::channel();
    let worker = std::thread::Builder::new().name("block-verifier".to_string()).spawn(move || {
        let lookup = |outpoint: &zebra_chain::transparent::OutPoint| spent_utxos.get(outpoint).cloned();
        let result = (verify_fns.verify_expensive)(&block, &network, &cheap, &lookup);
        let _ = sender.send(result);
    }).expect("unable to start CPU-only verification worker");
    await_cpu_verification(receiver, worker, zebra_chain::shutdown::is_shutting_down)
}

// ---------------------------------------------------------------------------
// Block ingest: the doorway for everything that is NOT new_network's own downloads.
//
// Producers are submit_block (the RPC, and therefore the internal miner) and the crosslink
// test harness. Network-sourced blocks do NOT come through here -- they go straight into
// blocks_to_commit -- so every entry on this queue is a local submission with no retry path
// behind it. That is why the send side applies backpressure instead of dropping.
// ---------------------------------------------------------------------------

/// What happened to a submitted block.
#[derive(Clone, Debug)]
pub enum IngestOutcome {
    /// Verified and committed.
    Committed(Hash),
    /// The state already knows this hash.
    ///
    /// `location` distinguishes best chain from a side chain or the commit queue -- a
    /// distinction `is_duplicate_request()` throws away, and one miners care about.
    Known {
        location: crate::response::KnownBlockLocation,
        height: Height,
    },
    /// Rejected. `misbehavior_score` mirrors the consensus error's own score.
    Failed {
        reason: String,
        misbehavior_score: u32,
    },
    /// Held in the commit queue, waiting on its parent or a BFT decision. It may still commit or
    /// fail later; a submitter hears nothing more.
    Pending {
        reason: String,
    },
}

/// A block submitted from outside new_network, with a channel for the verdict.
pub struct BlockSubmission {
    pub block: std::sync::Arc<Block>,
    pub reply: tokio::sync::oneshot::Sender<IngestOutcome>,
    /// Skip `verify_expensive` (proofs, signatures, scripts) for this block: see
    /// [`submit_trusted_block_to_new_network`].
    pub trusted: bool,
}

/// Bounded, but sized for bulk: the legacy syncer submits here too, and it runs with a
/// download concurrency of 50 and a lookahead. Backpressure still applies -- submitters wait
/// for a slot rather than being dropped -- this just stops the common case blocking.
const BLOCK_SUBMISSION_QUEUE_LEN: usize = 256;

static BLOCK_SUBMISSION_SENDER: std::sync::OnceLock<tokio::sync::mpsc::Sender<BlockSubmission>> =
    std::sync::OnceLock::new();

/// A stand-in for one STP peer, for tests. Packets sent on `inbound` reach the sync loop exactly
/// as if the network thread had just decrypted and reassembled them from this peer, and every
/// packet the node addresses to the peer comes out of `outbound` instead of a socket. Only the
/// transport is simulated: packet parsing, kill rules, download slots and the commit pipeline are
/// the real ones.
pub struct SyntheticPeer {
    pub address: STPAddress,
    pub inbound: std::sync::mpsc::Sender<Vec<u8>>,
    pub outbound: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    pub events: tokio::sync::mpsc::UnboundedReceiver<SyntheticPeerEvent>,
}

#[derive(Clone, Debug)]
pub enum SyntheticPeerEvent {
    /// The node disconnected the peer for misbehaving. The peer stays disconnected.
    Killed(String),
    /// What became of a block the peer delivered. A block held in the commit queue reports
    /// `Pending` once, then its final verdict when it leaves.
    Outcome { hash: Hash, outcome: IngestOutcome },
}

/// The sync loop's end of a `SyntheticPeer`.
struct SyntheticPeerLink {
    address: STPAddress,
    inbound: std::sync::mpsc::Receiver<Vec<u8>>,
    outbound: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    events: tokio::sync::mpsc::UnboundedSender<SyntheticPeerEvent>,
    alive: bool,
}

/// Picked up by the sync loop at the top of its next tick.
static SYNTHETIC_PEERS_TO_ATTACH: std::sync::Mutex<Vec<SyntheticPeerLink>> = std::sync::Mutex::new(Vec::new());

/// Connects synthetic peer `index` to the running node. Distinct indices are distinct peers.
///
/// Turns the debug-build traps off for the process: a node killing a peer that sent it bad data
/// is behaviour tests assert on, and in debug builds the kill path would otherwise stop the
/// process.
pub fn attach_synthetic_peer(index: u16) -> SyntheticPeer {
    tenderlink::DBG_TRAPS.store(false, std::sync::atomic::Ordering::Relaxed);

    // 127.2.x.y is loopback nobody listens on, so if the address is ever gossiped or dialled,
    // nothing answers. The key's first two bytes make the ConnectionKey unique per index.
    let [hi, lo] = index.to_be_bytes();
    let mut key = vec![0x5e; 32];
    key[..2].copy_from_slice(&index.to_le_bytes());
    let address = STPAddress {
        ip: std::net::Ipv4Addr::new(127, 2, hi, lo).to_ipv6_mapped(),
        port: 8233,
        magic1: CRYPTO_MAGIC,
        key,
    };

    let (inbound_tx, inbound_rx) = std::sync::mpsc::channel();
    let (outbound_tx, outbound_rx) = tokio::sync::mpsc::unbounded_channel();
    let (events_tx, events_rx) = tokio::sync::mpsc::unbounded_channel();
    SYNTHETIC_PEERS_TO_ATTACH.lock().unwrap().push(SyntheticPeerLink {
        address: address.clone(),
        inbound: inbound_rx,
        outbound: outbound_tx,
        events: events_tx,
        alive: true,
    });
    SyntheticPeer { address, inbound: inbound_tx, outbound: outbound_rx, events: events_rx }
}

/// A STATUS packet: the queued hashes, then `chains` as a hash tree. The sync loop sends its own
/// status with this, so a synthetic peer's status is encoded identically.
pub fn status_packet(queued: &[Hash], chains: &NearTipChains, tip_height_override: Option<u32>) -> Vec<u8> {
    let queue_len = queued.len().min(MAX_BLOCKS_TO_QUEUE_TO_COMMIT);
    let (mut buf, mut o) = ([0u8; 1 + 1 + MAX_BLOCKS_TO_QUEUE_TO_COMMIT * 32 + PACKET_STATUS_MAX_SIZE], 0);
    o += PACKET_TYPE_STATUS.write_to(&mut buf[o..]);
    o += (queue_len as u8)  .write_to(&mut buf[o..]);
    for queued_hash in &queued[..queue_len] {
        o += queued_hash.0.write_to(&mut buf[o..]);
    }
    o += chains.write_to(&mut buf[o..], tip_height_override);
    Vec::from(&buf[..o])
}

/// The BLOCK_CHUNK packets that answer a request for `serialized` from `offset` on.
///
/// With STP 1.0 carrying messages up to [`tenderlink::transport::MAX_MESSAGE_SIZE`], every
/// block fits in one packet: the remainder from `offset` goes out whole. STP delivery is still
/// best-effort, so the requester keeps the slot, its retry backoff and timeout, and resumes
/// from its highest contiguous prefix if a (legacy, partial) fragment ever arrives.
pub fn block_chunk_packets(serialized: &[u8], height: u32, hash: Hash, offset: usize) -> Vec<Vec<u8>> {
    let total_size: u32 = serialized.len().try_into().unwrap();
    let height_hash = HeightAndHashOr0 { height: block::Height(height), hash_or_0: hash };

    let mut packets = Vec::new();
    for (chunk_i, chunk) in serialized[offset..].chunks(BLOCK_RESPONSE_MAX_PAYLOAD).enumerate() {
        let hdr = PeerPowBlockResponseChunkHdr {
            offset: (offset + chunk_i * BLOCK_RESPONSE_MAX_PAYLOAD) as u32,
            size: total_size,
            height_hash,
        };
        let mut hdr_buf = [0u8; BLOCK_RESPONSE_MAX_MESSAGE - BLOCK_RESPONSE_MAX_PAYLOAD];
        let mut o = 0;
        o += PACKET_TYPE_BLOCK_CHUNK.write_to(&mut hdr_buf[o..]);
        o += hdr.write_to(&mut hdr_buf[o..]);
        let mut packet = Vec::with_capacity(o + chunk.len());
        packet.extend_from_slice(&hdr_buf[..o]);
        packet.extend_from_slice(chunk);
        packets.push(packet);
    }
    packets
}

/// `(height, hash, offset)` if `packet` is a BLOCK_REQ. A zero hash asks by height alone.
fn block_request_packet(height_hash: HeightAndHashOr0, offset: u32) -> Vec<u8> {
    let mut buf = [0u8; JUMBO_FRAG_SIZE];
    let mut o = 0;
    o += PACKET_TYPE_BLOCK_REQ.write_to(&mut buf[o..]);
    o += PeerPowBlockRequest { height_hash, offset }.write_to(&mut buf[o..]);
    Vec::from(&buf[..o])
}

pub fn parse_block_request(packet: &[u8]) -> Option<(u32, Hash, u32)> {
    let mut msg = packet;
    if u8::read_from(&mut msg)? != PACKET_TYPE_BLOCK_REQ {
        return None;
    }
    let request = PeerPowBlockRequest::read_from(&mut msg)?;
    Some((request.height_hash.height.0, request.height_hash.hash_or_0, request.offset))
}

/// Blocks that peers claim to have (from the near-tip trees they send in STATUS
/// packets) but that are absent from our own near-tip chains. Deduplicated across
/// peers. Refreshed by the sync loop on the status cadence; readers (e.g. the
/// visualizer) lock and clone at any time.
///
/// NOTE: a branch sharing no run with our near-tip chains is attested whole, so it
/// may include old blocks we do have (below our comparison window); filter against
/// the state if that matters.
pub static PEER_ATTESTED_BLOCKS: std::sync::Mutex<Vec<ShadowBlock>> = std::sync::Mutex::new(Vec::new());

/// How many STP peers this node is currently connected to. Written by the sync loop
/// whenever the connection set is refreshed; readers (e.g. the visualizer) load it at
/// any time. This is the PoW peer count: on the crosslink networks the legacy
/// zebra-network syncer is not spawned, so these are the only block-carrying peers.
pub static POW_PEER_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Submit a block to new_network and wait for its verdict.
///
/// Applies backpressure rather than dropping: nothing re-submits a miner's solved block, so a
/// silent drop loses it. Never blocks the calling task -- `reserve()` yields instead.
pub async fn submit_block_to_new_network(
    block: std::sync::Arc<Block>,
    timeout: std::time::Duration,
) -> Result<IngestOutcome, String> {
    submit_to_new_network(block, false, timeout).await
}

/// Submit a block the caller vouches for, such as one from a chain prefix this node exported
/// earlier, and wait for its verdict.
///
/// It takes the same path as [`submit_block_to_new_network`] -- header and PoW, body, the
/// crosslink fat-pointer gate, and contextual validation with its staking and finality effects --
/// except that `verify_expensive` is skipped: no proofs, signatures or scripts are checked. The
/// transparent outputs that check would have returned are computed directly instead. Only for
/// bytes whose origin the caller trusts; blocks from the network are never trusted.
pub async fn submit_trusted_block_to_new_network(
    block: std::sync::Arc<Block>,
    timeout: std::time::Duration,
) -> Result<IngestOutcome, String> {
    submit_to_new_network(block, true, timeout).await
}

async fn submit_to_new_network(
    block: std::sync::Arc<Block>,
    trusted: bool,
    timeout: std::time::Duration,
) -> Result<IngestOutcome, String> {
    let Some(tx) = BLOCK_SUBMISSION_SENDER.get() else {
        return Err("new_network is not running".to_string());
    };

    let permit = match tokio::time::timeout(timeout, tx.reserve()).await {
        Ok(Ok(permit)) => permit,
        Ok(Err(_)) => return Err("new_network stopped accepting blocks".to_string()),
        Err(_) => return Err("timed out waiting for the block ingest queue".to_string()),
    };

    let (reply, rx) = tokio::sync::oneshot::channel();
    permit.send(BlockSubmission { block, reply, trusted });

    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(outcome)) => Ok(outcome),
        Ok(Err(_)) => Err("new_network dropped the reply channel".to_string()),
        Err(_) => Err("timed out waiting for the block verdict".to_string()),
    }
}

// The fate table: one entry per block in blocks_to_commit, holding everyone owed word of what
// becomes of it. Blocks leave the queue only through settle_fate(), so nobody is left waiting.
#[derive(Default)]
struct BlockFate {
    reply: Option<tokio::sync::oneshot::Sender<IngestOutcome>>,
    delivered_by: Option<ConnectionKey>,
    cheap_checks: Option<CheapBlockChecks>,
    pending_told: bool,
    /// Submitted with [`submit_trusted_block_to_new_network`]: `verify_expensive` is skipped.
    trusted: bool,
}

impl BlockFate {
    fn tell(&mut self, synthetic_peers: &[SyntheticPeerLink], hash: Hash, outcome: IngestOutcome) {
        if let Some(reply) = self.reply.take() {
            let _ = reply.send(outcome.clone());
        }
        if let Some(key) = self.delivered_by {
            tell_synthetic_peer(synthetic_peers, key, hash, outcome);
        }
    }

    fn tell_pending(&mut self, synthetic_peers: &[SyntheticPeerLink], hash: Hash, reason: std::fmt::Arguments) {
        if !self.pending_told {
            self.pending_told = true;
            self.tell(synthetic_peers, hash, IngestOutcome::Pending { reason: reason.to_string() });
        }
    }
}

/// Takes the block's entry out of the fate table and answers everyone waiting on it. Returns the
/// peer that delivered it.
fn settle_fate(fates: &mut HashMap<Hash, BlockFate>, synthetic_peers: &[SyntheticPeerLink], hash: Hash, outcome: IngestOutcome) -> Option<ConnectionKey> {
    let mut fate = fates.remove(&hash).expect("every queued block has a fate");
    fate.tell(synthetic_peers, hash, outcome);
    fate.delivered_by
}

fn tell_synthetic_peer(synthetic_peers: &[SyntheticPeerLink], key: ConnectionKey, hash: Hash, outcome: IngestOutcome) {
    if let Some(synthetic) = synthetic_peers.iter().find(|p| p.address.connection_key() == key) {
        let _ = synthetic.events.send(SyntheticPeerEvent::Outcome { hash, outcome });
    }
}

fn known_outcome(read_state: &ReadState, hash: Hash) -> Option<IngestOutcome> {
    read_state.known_block(hash).map(|known| IngestOutcome::Known { location: known.location, height: known.height })
}

// A peer-served block that failed verification. Its hash goes unrequested until `retry_at` rather
// than being banned: the hash covers only the header, and a forged body or forged signatures can
// sit under an honest block's hash, so a ban would let any peer keep us off the honest block.
struct Rejection {
    strikes: u32,
    retry_at: std::time::Instant,
}

fn note_rejection(rejections: &mut HashMap<Hash, Rejection>, hash: Hash) {
    let now = std::time::Instant::now();
    let rejection = rejections.entry(hash).or_insert(Rejection { strikes: 0, retry_at: now });
    rejection.strikes += 1;
    let backoff = REJECTED_BLOCK_BACKOFF_MIN.saturating_mul(1 << (rejection.strikes - 1).min(16));
    rejection.retry_at = now + backoff.min(REJECTED_BLOCK_BACKOFF_MAX);
}

fn is_backing_off(rejections: &HashMap<Hash, Rejection>, hash: Hash) -> bool {
    rejections.get(&hash).is_some_and(|rejection| std::time::Instant::now() < rejection.retry_at)
}

fn kill_peer(
    current_connections: &mut Vec<(STPAddress, [u8; 64])>,
    recent_peer_addresses: &mut HashMap<u16, HashMap<STPAddress, RecentPeerAddress>>,
    synthetic_peers: &mut [SyntheticPeerLink],
    local_addresses_secret: u64,
    address: &STPAddress,
    reason: String,
) {
    tracing::error!("NewNet: Killing peer {:?}: {}", address, reason);
    let key = address.connection_key();
    current_connections.retain(|(addr, _)| addr.connection_key() != key);
    let kill_bucket = address_bucket(local_addresses_secret, address) & (MAX_RECENT_BUCKETS - 1);
    if let Some(recents) = recent_peer_addresses.get_mut(&(kill_bucket as u16)) {
        recents.remove(address);
    }
    if let Some(synthetic) = synthetic_peers.iter_mut().find(|p| p.address.connection_key() == key) {
        synthetic.alive = false;
        let _ = synthetic.events.send(SyntheticPeerEvent::Killed(reason));
    }
}

// Which cached block to evict when the cache is full. The queue is kept sorted by height,
// and the low blocks are the next in line to commit, so evict from the top: prefer the
// highest block whose parent is neither committed nor queued (a tail that can't commit any
// time soon); if everything connects, take the highest. The lowest block is never picked --
// even "dangling" it's the closest to committable (its parent is the most imminent arrival).
fn eviction_index(blocks_to_commit: &[(Hash, std::sync::Arc<Block>)], read_state: &ReadState) -> Option<usize> {
    if blocks_to_commit.is_empty() {
        return None;
    }
    let queued: HashSet<Hash> = blocks_to_commit.iter().map(|(hash, _)| *hash).collect();
    for (i, (_, block)) in blocks_to_commit.iter().enumerate().rev() {
        if i == 0 {
            break;
        }
        let parent = block.header.previous_block_hash;
        if !queued.contains(&parent) && read_state.known_block(parent).is_none() {
            return Some(i);
        }
    }
    Some(blocks_to_commit.len() - 1)
}

pub fn sync(
    config: &crate::config::Config,
    read_state: ReadState,
    // tfl_service: TFLService, // no TFLServiceHandle. Sadge!
    rt: tokio::runtime::Handle,
    verify_fns: VerifyFns,
    // The finalizer identity and BFT peers; `None` runs no BFT at all (every non-null fat
    // pointer then stays unresolvable, so only pre-activation chains sync).
    bft: Option<bft::BftLaunch>,
    // The block writer. This loop owns it, so every mutation of the chain state happens on
    // this thread, in a known order, with the result available synchronously.
    mut block_writer: crate::service::write::WriteBlockWorkerTask,
    genesis: std::sync::Arc<Block>,
) {
    let (submit_tx, mut submit_rx) = tokio::sync::mpsc::channel(BLOCK_SUBMISSION_QUEUE_LEN);
    let _ = BLOCK_SUBMISSION_SENDER.set(submit_tx);

    // Commit genesis before anything waits on a tip. This loop owns the writer, so nothing else
    // can do it -- and `get_tips_blocking` below spins until a finalized tip exists, which would
    // never happen.
    if read_state.finalized_tip().is_none() {
        match block_writer.commit_genesis(genesis) {
            Ok(hash) => tracing::info!("NewNet: committed genesis {hash}"),
            Err(err) => panic!("could not commit genesis block: {err}"),
        }
    }
    let mut fates: HashMap<Hash, BlockFate> = HashMap::new();
    let mut rejections: HashMap<Hash, Rejection> = HashMap::new();
    let mut peers_to_kill: Vec<(ConnectionKey, String)> = Vec::new();

    {
        let ((tip_height, tip_hash), (finalized_tip_height, finalized_tip_hash)) = get_tips_blocking(&read_state);
        tracing::info!("NewNet: Starting at height={} hash={:?} finalized_height={} finalized_hash={}", tip_height.0, tip_hash, finalized_tip_height.0, finalized_tip_hash);
        assert!(finalized_tip_height <= tip_height);
    }

    // BFT restores its persisted chain against the bc-chain, so this runs after genesis is in.
    let mut bft_runner = bft::BftRunner::new(bft, config, &read_state, &mut block_writer, rt.clone());
    let crosslink_params = read_state.network().crosslink_parameters();

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
    // small min keeps the send buffer rate-adaptive (clamp(1s * rate, 512KiB, 256MiB)) instead of a flat 256MiB/conn
    let mut network_thread_handle = new_network_thread(my_keypairs.clone(), actual_network_local_port, None, (1_000_000, 512 * 1024, 256 * 1024 * 1024));
    // Sized from the per-(peer, channel) byte cap the transport actually enforces, which is
    // what bounds the responder's Bulk backlog ahead of our block (see `block_response_deadline`).
    let block_response_deadline = block_response_deadline(network_thread_handle.limits().peer_outbox_bytes as u64);
    tracing::info!("NewNet: Bound to port {}", actual_network_local_port);
    // A fresh peer string for other nodes' network_initial_peers (replace the IP with this
    // node's reachable one). STP 2.0 addresses cannot be reused from 1.x or 0.x configs.
    tracing::info!("NewNet: PEER ADDRESS: {:?}", STPAddress::from(std::net::Ipv6Addr::LOCALHOST, actual_network_local_port, &network_keypair));

    let mut current_connections = Vec::<(STPAddress, [u8; 64])>::new();
    let mut initiate_connections = Vec::<STPAddress>::new();
    let mut packets_to_send: Vec<(ConnectionKey, Vec<u8>)> = Vec::new();
    // Whole-block responses travel on the transport's bulk channel, so this node's status and
    // requests to the same peer are not queued behind them.
    let mut bulk_packets_to_send: Vec<(ConnectionKey, Vec<u8>)> = Vec::new();
    let mut synthetic_peers: Vec<SyntheticPeerLink> = Vec::new();

    // Parse and connect to initial peers
    let mut initial_peer_addresses: Vec<STPAddress> = Vec::new();
    for peer_str in &config.network_initial_peers {
        if let Some(address) = STPAddress::parse(peer_str) {
            if address.magic1 != CRYPTO_MAGIC {
                // @Dev
                panic!("The magic in the config toml - {} ({}) is different from the crypto magic - {} ({})! Modify one or the other!",
                        tenderlink::stp::b64(&address.magic1.to_le_bytes()[..6]),
                        tenderlink::stp::crypto_string_from_connect_magic1(address.magic1).unwrap_or("<invalid>"),
                        tenderlink::stp::b64(&CRYPTO_MAGIC  .to_le_bytes()[..6]),
                        tenderlink::stp::crypto_string_from_connect_magic1(CRYPTO_MAGIC).unwrap(),
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
    let local_addresses_secret: u64 = rand::thread_rng().gen();

    let mut recent_peer_addresses:  HashMap<u16, HashMap<STPAddress, RecentPeerAddress>> = HashMap::new();
    let mut alleged_peer_addresses: HashMap<u16, HashMap<STPAddress, ConnectionKey>> = HashMap::new(); // keyed by sender, not address. connection key of latest sender is stored so we know who to ask to initiate UDP hole punch

    let mut peers: HashMap<ConnectionKey, Peer> = HashMap::new();
    let mut pending_selected_addresses: HashMap<ConnectionKey, std::time::Instant> = HashMap::new(); // store time for expiry

    #[allow(non_snake_case)] // temporary counter, named so a later grep finds it
    let mut XXX_tick_loop_counter = 0usize;

    let mut next_status       = std::time::Instant::now();
    // The tip hash in the last STATUS we sent, so a new one can be advertised immediately
    // instead of waiting out the heartbeat.
    let mut last_status_tip: Option<Hash> = None;
    let mut next_dl_init      = std::time::Instant::now();
    let mut next_peer_gossip  = std::time::Instant::now();
    let mut next_peer_connect = std::time::Instant::now();
    let mut next_peer_request = std::time::Instant::now();
    let mut next_console_status_print = std::time::Instant::now();
    let mut next_attested_publish = std::time::Instant::now();


    // Lite checkpoint: enforced at the commit-queue push, and stays armed forever.
    let checkpoint = Checkpoint::from_config(&config.network_checkpoint);
    // while false we need to filter out peers who do not have the checkpoint
    let mut checkpoint_committed_locally = match &checkpoint {
        None => true,
        Some(cp) => read_state.best_chain_block_hash(Height(cp.height)) == Some(cp.hash),
    };
    if let Some(cp) = &checkpoint {
        tracing::info!("NewNet: Checkpoint {} @ height {} active.", cp.hash, cp.height);
        if let Some(local) = read_state.best_chain_block_hash(Height(cp.height)) {
            if local != cp.hash {
                // We synced past the checkpoint height on the wrong chain before this
                // checkpoint was configured; the commit rule can't undo that.
                tracing::error!("NewNet: local best chain has {local} at checkpoint height {}, expected {}! On the wrong chain; a resync is needed.", cp.height, cp.hash);
            }
        }
    }

    let stp_address_get_short_string = |address| {
        let addr = format!("{:?}", address);
        let short = &addr[addr.len().saturating_sub(6)..]; // 6 base64 chars -> 4 bytes -> probably unique up to 65536 connections
        short.to_string()
    };

    let mut sync_profile = SyncProfile::new();

    // Stop only between writes. A commit and its chain/BFT bookkeeping finish
    // together on this thread; dropping it mid-commit is never a shutdown path.
    while !zebra_chain::shutdown::is_shutting_down() {
        let loop_start = std::time::Instant::now();

        bft_runner.tick(&read_state, &mut block_writer);

        if loop_start > next_console_status_print {
            let mut my_peers_to_print = Vec::new();
            for (address, _) in &current_connections {
                let key = address.connection_key();
                if let Some(peer) = peers.get(&key) {
                    my_peers_to_print.push(format!("{}@{}/{}", stp_address_get_short_string(address.clone()), peer.their_tree.finalized_height, peer.their_tree.tip_height));
                }
            }

            tracing::info!("tip height: {:?}, finalized height: {:?}, peers: {:?}",
                           read_state.best_tip().map(|(h, _)| h.0),
                           read_state.finalized_tip().map(|(h, _)| h.0),
                           my_peers_to_print);
            next_console_status_print = loop_start + std::time::Duration::from_secs(2);
        }

        // Publish the blocks peers claim that we don't have ourselves (see
        // PEER_ATTESTED_BLOCKS). Display-facing housekeeping on its own timer, kept
        // out of the packet send/receive paths so they never touch the lock.
        if loop_start >= next_attested_publish {
            next_attested_publish = loop_start + std::time::Duration::from_millis(ATTESTED_PUBLISH_MS);

            if let Some(near_tip_chains) = near_tip_chains_from_state(&read_state) {
                let mut seen: HashSet<Hash> = HashSet::new(); // cross-peer dedup
                let mut attested: Vec<ShadowBlock> = Vec::new();
                for peer in peers.values() {
                    for branch in &peer.their_tree.branches {
                        // Everything above the longest run shared with any of our chains
                        // is the peer's claim beyond our knowledge. A shared run also
                        // vouches for the branch below it (parent-linked), so no run
                        // means the whole branch is unshared. One block per height, so
                        // the unshared part is just the suffix past the shared run.
                        let mut shared_n = 0;
                        for our_chain in &near_tip_chains.chains {
                            if let Some(last) = chain_intersect_prefix(branch, &our_chain.blocks).last() {
                                shared_n = shared_n.max((last.this_height + 1 - branch[0].this_height) as usize);
                            }
                        }
                        // never attest blocks we hold ourselves (a no-overlap branch from a
                        // far-behind peer is otherwise published whole, and the GUI would
                        // show real best-chain blocks as mere claims)
                        attested.extend(branch[shared_n..].iter().copied().filter(|b|
                            seen.insert(b.this_hash) && read_state.known_block(b.this_hash).is_none()));
                    }
                }
                *PEER_ATTESTED_BLOCKS.lock().unwrap() = attested;
            }
        }


        // Drain externally submitted blocks (submit_block RPC, miner, test harness) into the
        // same commit queue the network path uses, so they go through identical verification.
        loop {
            match submit_rx.try_recv() {
                Ok(BlockSubmission { block, reply, trusted }) => {
                    let hash = block.hash();

                    if let Some(known) = read_state.known_block(hash) {
                        let _ = reply.send(IngestOutcome::Known {
                            location: known.location,
                            height: known.height,
                        });
                        continue;
                    }
                    if blocks_to_commit.iter().any(|(queued, _)| *queued == hash) {
                        let _ = reply.send(IngestOutcome::Known {
                            location: crate::response::KnownBlockLocation::Queue,
                            height: block.coinbase_height().unwrap_or(Height(0)),
                        });
                        continue;
                    }
                    // The commit loop expects every queued block to have a coinbase height, and a
                    // block that parses with no transactions has none. The packet path refuses
                    // those before queueing; this is the same refusal for submissions. Mirrors
                    // zebra_consensus::BlockError::MissingHeight and its misbehavior score.
                    if block.coinbase_height().is_none() {
                        let _ = reply.send(IngestOutcome::Failed {
                            reason: format!("invalid block {hash:?}: missing block height"),
                            misbehavior_score: 100,
                        });
                        continue;
                    }

                    blocks_to_commit.push((hash, block));
                    fates.insert(hash, BlockFate { reply: Some(reply), trusted, ..Default::default() });
                }
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                Err(err) => { tracing::error!("block submission queue: {err:?}"); break; }
            }
        }

        #[cfg(debug_assertions)]
        if false { // @Dev @Debug: Roundtrip test.
            if let Some(chains) = near_tip_chains_from_state(&read_state) {
                chains.roundtrip_to_branches(PACKET_STATUS_MAX_SIZE, if XXX_tick_loop_counter % 20 == 0 { 2 } else { 0 });
            }
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
        #[allow(dead_code)]
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

        // Backpressure before work: a response is built only when the peer's untransmitted
        // backlog (our outbox plus STP's queue, see `send_budget`) leaves room for it, counting
        // what this tick already queued. A skipped request is not lost: the requester's slot
        // retries with backoff until its response deadline (`block_response_deadline`), counted
        // from its first request. A request for a response still queued to that peer is
        // skipped as a duplicate (`QueuedBlockResponses`).
        let mut reserved_send_bytes: HashMap<ConnectionKey, u64> = HashMap::new();
        let mut deferred_block_responses = 0usize;
        let mut duplicate_block_responses = 0usize;
        // The one point per tick where the duplicate guard may forget: last tick's responses
        // have been handed to the transport (and so counted in its backlog), and nothing has
        // been reserved yet this tick. Clearing anywhere after a reservation below and before
        // `service` would forget a response that is queued but not yet submitted.
        for (connection_key, peer) in peers.iter_mut() {
            let is_synthetic = synthetic_peers.iter().any(|p| p.address.connection_key() == *connection_key);
            let backlog = if is_synthetic { None } else { Some(network_thread_handle.send_backlog_bytes(*connection_key, Channel::Bulk).unwrap_or(0)) };
            peer.queued_block_responses.forget_if_drained(backlog);
        }
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
            let Ok(offset_u32) = u32::try_from(*offset) else { continue };

            let reserved = reserved_send_bytes.get(connection_key).copied().unwrap_or(0);
            let needed = match serialized_blocks.get(&hash) {
                Some(serialized) => (serialized.len().saturating_sub(*offset) + BLOCK_RESPONSE_MAX_MESSAGE - BLOCK_RESPONSE_MAX_PAYLOAD) as u64,
                None => BLOCK_RESPONSE_MAX_MESSAGE as u64, // unknown until serialized: assume the max
            };
            // Synthetic peers are served around the network thread (see the outbound
            // interception before service_connections), so STP's backlog does not apply.
            let is_synthetic = synthetic_peers.iter().any(|p| p.address.connection_key() == *connection_key);
            let budget = if is_synthetic { None } else { Some(network_thread_handle.send_budget(*connection_key, Channel::Bulk)) };
            match admit_block_response(&peer.queued_block_responses, hash, offset_u32, budget, reserved, needed) {
                ResponseAdmission::Admit => {}
                ResponseAdmission::Duplicate => { duplicate_block_responses += 1; continue; }
                ResponseAdmission::RecordFull | ResponseAdmission::NoBudget => { deferred_block_responses += 1; continue; }
            }

            if !serialized_blocks.contains_key(&hash) {
                match read_state.block_from_any_chain(hash.into()) {
                    Some(block) => {
                        let serialized = dbg_verify(block.zcash_serialize_to_vec().ok()).unwrap();
                        assert!(serialized.len() as u64 <= zebra_chain::block::MAX_BLOCK_BYTES, "Internal block is too big! {} B", serialized.len());
                        serialized_blocks.insert(hash, serialized);
                    }
                    None => {
                        // tracing::warn!("NewNet: Couldn't get block for hash {hash}!");
                        continue;
                    }
                }
            }

            let serialized_block = &serialized_blocks[&hash];

            if *offset >= serialized_block.len() {
                if TRACE { tracing::info!("Ignoring out-of-range block offset request: {offset} >= {}", serialized_block.len()); }
                continue;
            }

            if TRACE { tracing::info!("Sending BLOCK (Height: {height}, Hash: {hash}, {offset}-{})", serialized_block.len()); }

            // Reserve the exact bytes before building, so a later request this tick for the
            // same peer is checked against them.
            let exact = (serialized_block.len() - *offset + BLOCK_RESPONSE_MAX_MESSAGE - BLOCK_RESPONSE_MAX_PAYLOAD) as u64;
            if budget.is_some_and(|budget| budget < reserved.saturating_add(exact)) {
                deferred_block_responses += 1;
                continue;
            }
            *reserved_send_bytes.entry(*connection_key).or_insert(0) += exact;
            peer.queued_block_responses.record(hash, offset_u32);
            let packets = block_chunk_packets(serialized_block, *height, hash, *offset);
            debug_assert_eq!(packets.iter().map(|p| p.len() as u64).sum::<u64>(), exact);
            for packet in packets {
                bulk_packets_to_send.push((*connection_key, packet));
            }
        }
        if deferred_block_responses > 0 {
            tracing::debug!("NewNet: deferred {deferred_block_responses} block responses: peer send backlog full");
        }
        if duplicate_block_responses > 0 {
            tracing::debug!("NewNet: skipped {duplicate_block_responses} block requests already answered and still queued");
        }
        blocks_to_send.clear();

        // A new tip is news: tell peers now rather than at the next heartbeat. STATUS is the
        // only thing that advertises our tip, so on a fast chain a peer spent up to
        // `status_interval` mining a fork it could not know was already lost -- we measured
        // three wasted blocks per height on a sub-second chain. The heartbeat stays as the
        // floor for *re-*announcing an unchanged tip, and the tick rate bounds how often this
        // can fire, so it cannot become a flood.
        let tip_for_status = read_state.best_tip().map(|(_, hash)| hash);
        let tip_is_news = tip_for_status.is_some() && tip_for_status != last_status_tip;

        // Peers catching up from far behind get their next historical window as soon as they
        // report progress; everyone else waits for news or the heartbeat.
        let heartbeat_due = tip_is_news || std::time::Instant::now() >= next_status;
        let history_due = peers.values().any(|peer| peer.history_wanted);

        if heartbeat_due || history_due { 'send_status: {
            if heartbeat_due {
                next_status = std::time::Instant::now() + status_interval;
                last_status_tip = tip_for_status;
            }

            // Our chain tree, as of right now. Built here and dropped at the end of the block, so
            // it cannot go stale and nothing has to keep it up to date.
            let Some(near_tip_chains) = near_tip_chains_from_state(&read_state) else {
                tracing::warn!("NewNet: no tip yet; skipping STATUS");
                break 'send_status;
            };

            let queued: Vec<Hash> = blocks_to_commit.iter().take(MAX_BLOCKS_TO_QUEUE_TO_COMMIT).map(|(hash, _)| *hash).collect();
            let status = status_packet(&queued, &near_tip_chains, None);

            for (address, _) in &current_connections {
                let key = address.connection_key();

                let Some(peer) = peers.get_mut(&key) else {
                    if TRACE { tracing::info!("Trying to send a STATUS to {:?} but they haven't finished being created", address); }
                    continue;
                };
                if !heartbeat_due && !peer.history_wanted {
                    continue;
                }
                peer.history_wanted = false;
                let peer = &*peer;

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

                        // Root the window at the highest block of theirs that's on our best chain
                        // (their pre-fork blocks are ours too), so it lands right where they can
                        // attach, regardless of how far their finalized height lags their tip.
                        let mut shared_h = None;
                        for branch in &peer.their_tree.branches {
                            for b in branch.iter().rev() {
                                if shared_h.is_some_and(|sh| b.this_height <= sh) {
                                    break;
                                }
                                if read_state.best_chain_block_hash(block::Height(b.this_height)) == Some(b.this_hash) {
                                    shared_h = Some(b.this_height);
                                    break;
                                }
                            }
                        }

                        // subtract once so the first block of the window is one they provably have, confirming the attach point (because our chain_intersect_prefix() does not match parents),
                        // (in the fallback, subtract again because in order to send them that block we need to get that block's parent.)
                        let root_height = match shared_h {
                            Some(h) => h.saturating_sub(1),
                            None    => peer.their_tree.finalized_height.saturating_sub(2), // nothing of theirs is on our chain; old finalized-based rooting
                        };
                        let Some(root_hash) = read_state.best_chain_block_hash(block::Height(root_height)) else {
                            if TRACE { tracing::info!("Can't send a historical STATUS to {:?} as we don't have blocks @ {}", address, root_height); }
                            break 'try_send_historical true; // fallback to tip chains if we don't successfully send historical here
                        };

                        let hashes_from_root = read_state.find_block_hashes(vec![root_hash], None);
                        if hashes_from_root.len() == 0 {
                            if TRACE { tracing::info!("Can't send a historical STATUS to {:?} as we found 0 hashes after block @ {}, {}", address, root_height, root_hash); }
                            break 'try_send_historical true;
                        }

                        let mut parent_hash = root_hash;
                        let mut this_height = root_height + 1;
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

                        for &this_hash in &hashes_from_root {
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

                        let historical_status = status_packet(&queued, &historical_chains, Some(near_tip_chains.tip_height().expect("near_tip_chains_from_state() always produces at least one chain")));

                        // if TRACE { tracing::info!("Send a historical STATUS to {:?} @ {}", address, their_final_parent_parent_height + 1); }
                        packets_to_send.push((key, historical_status));
                        false
                    };
                }

                if send_tip_chains {
                    packets_to_send.push((key, status.clone()));
                }
            }
        }}

        // Discard statuses of disconnected peers
        peers.retain(|connection_key, _| current_connections.iter().any(|(addr, _)| addr.connection_key() == *connection_key));

        if std::time::Instant::now() >= next_dl_init { 'init_dls: {
            next_dl_init = std::time::Instant::now() + dl_init_interval;

            // An entry outlives its backoff by the max, so a hash failing again soon after keeps
            // its strike count and waits longer.
            rejections.retain(|_, rejection| std::time::Instant::now() < rejection.retry_at + REJECTED_BLOCK_BACKOFF_MAX);

            // Same deal as the STATUS block: built here, used here, dropped here.
            let Some(near_tip_chains) = near_tip_chains_from_state(&read_state) else {
                tracing::warn!("NewNet: no tip yet; skipping download init");
                break 'init_dls;
            };

            if !checkpoint_committed_locally {
                if let Some(cp) = &checkpoint {
                    checkpoint_committed_locally = read_state.best_chain_block_hash(Height(cp.height)) == Some(cp.hash);
                }
            }

            let mut active_block_dls = 0;
            // By-hash requests for BFT, counted apart: they are capped at a share of the budget,
            // so hashes named in proposals can never crowd out near-tip sync.
            let mut bft_block_dls = 0;
            let mut requests_by_hash: HashMap<Hash, usize> = HashMap::new();
            let mut requests_by_height: HashMap<u32, usize> = HashMap::new();
            const MAX_REQUEST_DUPLICATES_N: usize = 2; // TODO: reconsider @Prod
            for (_, peer) in &mut peers {
                for dl_i in 0..peer.block_downloads.slots.len() {
                    if peer.block_downloads.slot_is_used(dl_i) {
                        let HeightAndHashOr0 { height: block::Height(height), hash_or_0 } = peer.block_downloads.slots[dl_i].height_hash;

                        if height <= near_tip_chains.finalized_height {
                            if TRACE { tracing::info!("Cancelling download request for block @ {}, {} - <= finalized @ {}", height, hash_or_0, near_tip_chains.finalized_height); }
                            peer.block_downloads.remove(dl_i);

                        } else if block_download_expired(&mut peer.block_downloads.slots[dl_i], std::time::Instant::now(), block_response_deadline) {
                            if TRACE { tracing::info!("Cancelling download request for block @ {}, {} - timed out", height, hash_or_0); }
                            peer.block_downloads.remove(dl_i);
                            // TODO: lower weighting on this peer?

                        } else if hash_or_0 != block::Hash([0; 32]) {
                            requests_by_hash.entry(hash_or_0).and_modify(|c| *c += 1).or_insert(1);
                            if height == u32::MAX {
                                bft_block_dls += 1;
                            }
                            let is_probe = !checkpoint_committed_locally && checkpoint.is_some_and(|cp| cp.height == height && cp.hash == hash_or_0);
                            if !is_probe { active_block_dls += 1; }

                        } else {
                            requests_by_height.entry(height).and_modify(|c| *c += 1).or_insert(1);
                            active_block_dls += 1;
                        }
                    }
                }
            }
            let prev_active_block_dls = active_block_dls;
            let missing_bft_pow_blocks = bft::missing_pow_blocks(&read_state);

            // TODO: weight peers by observed quality
            const MAX_PEERS_TO_INIT_DLS_FROM: usize = 16;

            /*  Note(Sam): CRITICAL BUG THAT I AM ADDRESSING NOW. We cannot randomly select a fixed number of
                peers. We must first filter the peers by who has something we can download. Otherwise we will
                fail in the case where most of the peers are behind us.
            */

            let mut peer_random_keys: Vec<ConnectionKey> = peers.keys().cloned().collect();
            // shuffle in-place
            peer_random_keys.shuffle(&mut rand::thread_rng());

            let mut count_of_peers_we_started_download_from = 0;
            'send_to_peers: for connection_key in peer_random_keys {
                let Peer { origin: _, their_tree, their_queue, ref mut block_downloads, has_checkpoint_block, .. } = peers.get_mut(&connection_key).unwrap();
                if count_of_peers_we_started_download_from >= MAX_PEERS_TO_INIT_DLS_FROM { break 'send_to_peers; }
                let active_block_dls_before_this_peer = active_block_dls; // to detect if we start any dl for this peer


                if *their_tree == NearTipBranches::default() {
                    continue 'send_to_peers; // No messages yet.
                }

                let _blocks_to_this_peer = their_queue.len();
                // Skip now-disconnected peers
                let connection_address = {
                    let Some((addr, _)) = current_connections.iter().find(|(addr, _)| addr.connection_key() == connection_key) else {
                        tracing::error!("NewNet: Peer is disconnected even after we filtered out disconnected peers. Impossible!: {connection_key:?}");
                        continue 'send_to_peers;
                    };
                    addr.clone()
                };

                // Checkpoint gate: until our own best chain contains the checkpoint block, only
                // sync from peers that have proven they hold it (served it to a by-hash request,
                // see BLOCK_CHUNK). Otherwise an eclipsing wrong-chain peer could feed us its
                // whole chain and we'd only find out at the checkpoint height. The probe slot
                // times out and gets reinserted, so it doubles as the retry pacing.
                if let Some(cp) = &checkpoint {
                    if !checkpoint_committed_locally && !*has_checkpoint_block {
                        let probe = HeightAndHashOr0 {
                            height: block::Height(cp.height),
                            hash_or_0: cp.hash,
                        };
                        if block_downloads.position(probe).is_none() {
                            block_downloads.insert(probe);
                        }
                        continue 'send_to_peers;
                    }
                }

                // A BFT proposal can arrive before its PoW tail. Request those exact hashes from
                // peers directly; waiting for a hash to reappear in a moving STATUS window can
                // deadlock BFT validation and every PoW block whose fat pointer names it.
                for &hash in &missing_bft_pow_blocks {
                    if active_block_dls >= MAX_BANDWIDTH_BLOCKS_PER_RES || bft_block_dls >= MAX_BFT_BLOCKS_PER_RES {
                        break;
                    }
                    let dups = requests_by_hash.entry(hash).or_insert(0);
                    if *dups >= MAX_REQUEST_DUPLICATES_N || is_backing_off(&rejections, hash) {
                        continue;
                    }
                    let request = HeightAndHashOr0 {
                        // The hash is authoritative. The serving peer replaces this sentinel with
                        // the block's coinbase height in its response.
                        height: block::Height(u32::MAX),
                        hash_or_0: hash,
                    };
                    if block_downloads.position(request).is_none()
                        && block_downloads.insert(request).is_some()
                    {
                        tracing::debug!("Requesting PoW block {hash} needed by BFT validation from peer {connection_key:?}");
                        active_block_dls += 1;
                        bft_block_dls += 1;
                        *dups += 1;
                    }
                }

                #[allow(unused_macros)]
                macro_rules! warning {
                    ($($arg:tt)*) => {{
                        // let msg = format!("NewNet: Peer {:?}: {}", connection_address, format!($($arg)*));
                        tracing::warn!("{}", format!("NewNet: Peer {:?}: {}", connection_address, format!($($arg)*)).to_string());
                    }};
                }

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

                            let _our_chain_height_bgn = our_chain.blocks[0].this_height;
                            let our_chain_height_end = our_chain.blocks.last().unwrap().this_height + 1;


                            let prefix = chain_intersect_prefix(&their_branch, &our_chain.blocks);
                            // print_shadow_block_intersection(&their_branch, &our_chain.blocks, 1);


                            let height_of_match = if !prefix.is_empty() {
                                prefix.last().unwrap().this_height
                            } else if their_branch_height_bgn > 0
                                   && our_chain.blocks.iter().any(|b| b.this_hash == their_branch[0].parent_hash
                                                                   && b.this_height + 1 == their_branch_height_bgn) {
                                // no common height, but their branch extends our chain by parent link
                                // (e.g. a fresh block mined right above our tip)
                                their_branch_height_bgn - 1
                            } else {
                                continue; // no overlap
                            };

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
                                if is_backing_off(&rejections, hash) {
                                    continue;
                                }

                                // We may submit the same block multiple times (visit >1 of our chains that share a short prefix with their branch), and that's valid
                                let height_hash = HeightAndHashOr0 {
                                    height: block::Height(height),
                                    hash_or_0: hash,
                                };

                                if let Some(_dl_i) = block_downloads.position(height_hash) {
                                    // already included
                                } else {
                                    let dups = requests_by_hash.entry(hash).or_insert(0);
                                    if *dups < MAX_REQUEST_DUPLICATES_N {
                                        if let Some(_dl_i) = block_downloads.insert(height_hash) {
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
        }}

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
                        if hash_or_0 != block::Hash([0;32]) && read_state.known_block(hash_or_0).is_some() {
                            tracing::warn!("Don't need to re-request: block @ {height}, {hash_or_0} was already committed!");
                            peer.block_downloads.remove(dl_i);
                            continue 'downloads;
                        }

                        if !block_request_due(&mut peer.block_downloads.slots[dl_i], std::time::Instant::now(), block_response_deadline) {
                            continue 'downloads;
                        }
                        let dl = &peer.block_downloads.slots[dl_i];

                        // resume from the highest contiguous prefix we've already reassembled
                        let offset = match dl.reassembly.received.first() {
                            Some(&(0, end)) => end,
                            _ => 0,
                        };

                        tracing::debug!("Requesting height {} hash {} offset {offset} from peer {connection_key:?}", dl.height_hash.height.0, dl.height_hash.hash_or_0);

                        packets_to_send.push((*connection_key, block_request_packet(dl.height_hash, offset)));
                    }
                }
            }

            next_peer_request = std::time::Instant::now() + peer_request_interval;
        }

        pending_selected_addresses.retain(|_addr, time| std::time::Instant::now().saturating_duration_since(*time).as_secs() < 30);

        use rand::seq::SliceRandom;

        // @@@ @Todo @@@: We need speculative queueing ASAP!
        // packets_to_send.shuffle(&mut rand::thread_rng());

        // Synthetic peers are served here instead of by the network thread, which never hears of
        // them: their packets leave and arrive around it, and they are re-added to the connection
        // list it hands back each tick until the node kills them.
        synthetic_peers.extend(SYNTHETIC_PEERS_TO_ATTACH.lock().unwrap().drain(..));
        for queue in [&mut packets_to_send, &mut bulk_packets_to_send] {
            queue.retain(|(key, packet)| {
                let Some(synthetic) = synthetic_peers.iter().find(|p| p.address.connection_key() == *key) else {
                    return true;
                };
                let _ = synthetic.outbound.send(packet.clone());
                false
            });
        }

        // Service STP connections (send/recv). `None` means the network thread
        // did not answer in time because it died or wedged: keep our connection
        // list as it was instead of replacing it with an empty answer, so this
        // tick loop -- and with it Zebra's shutdown -- can continue.
        let mut packets_received = match service_connections(&mut network_thread_handle, NetworkThreadPush {
            initiate_connections,
            wanted_connections: current_connections.iter().filter(|(addr, _)| !synthetic_peers.iter().any(|p| p.address == *addr)).cloned().collect(),
            send_unreliable: packets_to_send,
            send_bulk: std::mem::take(&mut bulk_packets_to_send),
        }) {
            Some(resp) => {
                current_connections = resp.current_connections;
                resp.received_unreliable_messages
            }
            None => Default::default(),
        };
        for synthetic in synthetic_peers.iter().filter(|p| p.alive) {
            let key = synthetic.address.connection_key();
            current_connections.push((synthetic.address.clone(), [0; 64]));
            packets_received.extend(synthetic.inbound.try_iter().map(|packet| (key, packet)));
        }
        POW_PEER_COUNT.store(current_connections.len(), std::sync::atomic::Ordering::Relaxed);
        initiate_connections = Vec::new();
        packets_to_send = Vec::new();

        // Process received packets
        'process_packets: while let Some((connection_key, msg)) = packets_received.pop_front() {

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
                    kill_peer(&mut current_connections, &mut recent_peer_addresses, &mut synthetic_peers, local_addresses_secret, &connection_address, format!($($arg)*));
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

                    let _address_clone_for_printing = address.clone();
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

                let Some(mut their_tree) = some_or_kill!(NearTipBranches::read_from(&mut msg), "NearTipBranches read failed") else {
                    continue 'process_packets;
                };

                // Lite checkpoint: don't track gossiped branches past a conflicting block at the
                // checkpoint height — we'd reject them at commit anyway, so don't request them.
                if let Some(cp) = &checkpoint {
                    for branch in &mut their_tree.branches {
                        if let Some(i) = branch.iter().position(|b| cp.rejects(b.this_height, b.this_hash)) {
                            branch.truncate(i);
                        }
                    }
                    their_tree.branches.retain(|b| !b.is_empty());
                }

                tracing::debug!(
                    "STATUS from {connection_address:?}: finalized {}, branches {:?}",
                    their_tree.finalized_height,
                    their_tree.branches.iter().map(|b| {
                        let first = &b[0];
                        let last = b.last().unwrap();
                        format!("{}..{} ({}..{})", first.this_height, last.this_height, first.this_hash, last.this_hash)
                    }).collect::<Vec<_>>(),
                );
                peer.their_tree = their_tree;
                peer.their_queue = their_queue;

                // A far-behind peer learns what to fetch only from historical windows, 100 hashes
                // at a time. It sends a STATUS whenever its tip moves, so answering each one with
                // the next window moves it on once per round trip instead of once per heartbeat.
                let their_height = peer.their_tree.branches.iter().filter_map(|b| b.last()).map(|b| b.this_height).max().unwrap_or(peer.their_tree.finalized_height);
                let our_height = read_state.best_tip().map_or(0, |(h, _)| h.0);
                peer.history_wanted = their_height < near_tip_start_height(our_height);

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
                } else if height_hash.height.0 == u32::MAX {
                    // A by-hash request from BFT validation does not know the PoW height. Return
                    // the real height so the receiver can apply its normal finalized/window checks.
                    height_hash.height = read_state
                        .block_from_any_chain(height_hash.hash_or_0.into())
                        .and_then(|block| block.coinbase_height())
                        .unwrap_or(block::Height(u32::MAX));
                }

                if height_hash.hash_or_0 != block::Hash([0;32])
                    && height_hash.height.0 != u32::MAX
                {
                    blocks_to_send.push((connection_key, height_hash.hash_or_0, height_hash.height.0, request.offset as usize));
                } else {
                    // by-height request for a block we don't have on the best chain; nothing to send
                    // println!("Height requested that doesn't exist on BC: {:?}", height_hash.height);
                }
            } else if packet_type == PACKET_TYPE_BLOCK_CHUNK {
                let Some(hdr) = some_or_kill!(PeerPowBlockResponseChunkHdr::read_from(&mut msg), "failed to read PoW chunk header") else {
                    continue 'process_packets;
                };
                tracing::debug!("BLOCK chunk @ {} {} offset {} from peer {connection_key:?}", hdr.height_hash.height.0, hdr.height_hash.hash_or_0, hdr.offset);

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
                // A by-hash request made without knowing the height (BFT validation) uses
                // `u32::MAX`. A peer may echo it back unchanged, so the height is only known once
                // the block is deserialized; the height checks below run then instead.
                let height_is_alleged = alleged_height != u32::MAX;

                // Real peers get no reply when we drop a block they served, but a synthetic peer
                // is told what became of it.
                macro_rules! drop_block {
                    ($hash:expr, $($arg:tt)*) => {{
                        let hash = $hash;
                        warning!($($arg)*);
                        let outcome = if let Some(known) = known_outcome(&read_state, hash) {
                            known
                        } else if blocks_to_commit.iter().any(|(queued, _)| *queued == hash) {
                            IngestOutcome::Known { location: crate::response::KnownBlockLocation::Queue, height: block::Height(alleged_height) }
                        } else {
                            IngestOutcome::Failed { reason: format!($($arg)*), misbehavior_score: 0 }
                        };
                        tell_synthetic_peer(&synthetic_peers, connection_key, hash, outcome);
                    }};
                }

                // @Note: Skip blocks older than the base of the window we advertise. Depending on
                // whether NEAR_TIP_CHAIN_LEN is < or > Zebra's MAX_BLOCK_REORG_HEIGHT, being below
                // it *may* or *may not* imply the block is "not even worth" submitting to Zebra
                // (rejected due to being too far back).
                let (best_tip_height, _) = read_state.best_tip().expect("genesis is committed before the sync loop starts");
                let min_height = near_tip_start_height(best_tip_height.0);

                // if TRACE { tracing::info!("Block @ {alleged_height}, offset {}...", hdr.offset); }

                // A block on the decided chain is downloaded whatever height the peer alleges:
                // where it sits against the committed tip is what decides whether BFT is
                // dropped, and that is read from the block itself below.
                let on_decided_chain = bft::is_decided_missing_pow_block(hdr.height_hash.hash_or_0);
                let height_is_alleged = height_is_alleged && !on_decided_chain;

                if height_is_alleged && alleged_height < min_height {
                    drop_block!(hdr.height_hash.hash_or_0, "Block at height {alleged_height} is below our near-tip-chain height {min_height}");
                    peer.block_downloads.remove(dl_i);
                    continue 'process_packets; // Deciding that it's "not even worth" sending to Zebra
                }

                let finalized_height = read_state.finalized_tip().map_or(0, |(h, _)| h.0);
                if height_is_alleged && alleged_height <= finalized_height {
                    drop_block!(hdr.height_hash.hash_or_0, "Block at height {alleged_height} is already finalized");
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
                    if dl.height_hash.height.0 == u32::MAX {
                        // by-hash request; adopt whatever height the peer reports, verified below
                        dl.height_hash.height = hdr.height_hash.height;
                    } else if dl.height_hash.height != hdr.height_hash.height {
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


                // an unproven peer's checkpoint probe still needs to reassemble even if we
                // already have the block, so the possession proof lands; dropped post-proof below
                let is_unproven_checkpoint_probe = !checkpoint_committed_locally
                    && !peer.has_checkpoint_block
                    && checkpoint.is_some_and(|cp| cp.hash == alleged_hash);

                if !is_unproven_checkpoint_probe {
                    if blocks_to_commit.iter().any(|(queued_hash, _)| *queued_hash == alleged_hash) {
                        drop_block!(alleged_hash, "Block was already queued to commit!: {alleged_hash}");
                        peer.block_downloads.remove(dl_i);
                        continue 'process_packets;
                    }

                    if read_state.known_block(alleged_hash).is_some() {
                        drop_block!(alleged_hash, "Block was already committed!: {alleged_hash}");
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
                note_block_fragment(&mut peer.block_downloads.slots[dl_i], std::time::Instant::now());

                if !complete {
                    if TRACE { tracing::info!("Block @ {alleged_height} hash {alleged_hash}: fragment at offset {} ({} bytes); awaiting more", hdr.offset, msg.len()); }
                    continue 'process_packets;
                }

                if TRACE { tracing::info!("Block @ {alleged_height} hash {alleged_hash}: All fragments received ({} bytes)", hdr.size); }

                let block_data_vec = peer.block_downloads.remove(dl_i).reassembly.buf;
                let block_data = &block_data_vec[..];

                // TODO: we should be able to early out once we have chunk 0 or a hdr-contained parent hash
                // @Volatile, depends on block header format.
                let parent_hash = {
                    let mut tmp = &block_data[..];
                    let Some(_version) = some_or_kill!(<u32>::read_from(&mut tmp), "Failed to read block version number") else {
                        continue 'process_packets;
                    };
                    let Some(parent_hash) = some_or_kill!(<[u8; 32]>::read_from(&mut tmp), "Failed to read parent hash") else {
                        continue 'process_packets;
                    };
                    Hash(parent_hash)
                };

                let _have_parent_in_chains           = read_state.known_block(parent_hash).is_some();
                let _have_parent_in_blocks_to_commit = blocks_to_commit.iter().any(|(queued_hash, _)| *queued_hash == parent_hash);

                // @Experimental: Accept non-committable tails by commenting out the skip. This should be vetted for DoS - could an adversary queue nonsense blocks?
                // if !have_parent_in_chains && !have_parent_in_blocks_to_commit {
                //     // TODO: we should keep these, but have them easily evictable, for OoO block downloads
                //     warning!("Block does not link anywhere known, neither to our chains nor to our blocks-to-commit queue! Not queueing; dropping: height {alleged_height} hash {alleged_hash}, parent {parent_hash}");
                //     continue 'process_packets;
                // }
                if TRACE { tracing::info!("Block @ {alleged_height} hash {alleged_hash}, valid hash and height"); } // and links somewhere known..."); }

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

                if height_is_alleged && height != Height(alleged_height) {
                    kill!("Computed block height did not match advertised hash");
                    continue 'process_packets;
                }

                if on_decided_chain {
                    if height.0 <= finalized_height {
                        // The hash commits to the header alone, so the height is only this
                        // block's once the body is checked against the header's merkle root.
                        if let Err(err) = (verify_fns.check_body)(&block, read_state.network(), height) {
                            drop_block!(hash, "Block {hash} on the decided chain has an invalid body: {}", err.msg);
                            continue 'process_packets;
                        }
                        if read_state.known_block(hash).is_none() {
                            bft::drop_bft(
                                format!(
                                    "Finalizers holding at least two thirds of the stake decided a chain containing block {hash} at height {}, which conflicts with the block this node committed at that height.",
                                    height.0,
                                ),
                                (height, hash),
                                &block_writer.finalized_state.db,
                            );
                        }
                        drop_block!(hash, "Block at height {} is already finalized", height.0);
                        continue 'process_packets;
                    } else if read_state.known_block(parent_hash).is_none() {
                        bft::decided_missing_pow_block_needs_parent(hash, parent_hash);
                    }
                } else if !height_is_alleged {
                    // Deferred height checks for a by-hash request (see `height_is_alleged`).
                    if height.0 < min_height {
                        drop_block!(hash, "Block at height {} is below our near-tip-chain height {min_height}", height.0);
                        continue 'process_packets;
                    }
                    if height.0 <= finalized_height {
                        drop_block!(hash, "Block at height {} is already finalized", height.0);
                        continue 'process_packets;
                    }
                }

                // Lite checkpoint: never commit a conflicting block at the pinned height. Just
                // drop the block, not the peer — they served what we asked for, and peers on the
                // wrong fork still share all their pre-fork blocks with us.
                if let Some(cp) = &checkpoint {
                    if cp.rejects(height.0, hash) {
                        drop_block!(hash, "block {hash} @ {} conflicts with checkpoint {} @ {}; dropping block", height.0, cp.hash, cp.height);
                        continue 'process_packets;
                    }
                    if hash == cp.hash {
                        // peer proved it holds the checkpoint block; passes the dl-init gate now
                        peer.has_checkpoint_block = true;
                        // re-run the dup checks skipped for probes; don't queue the block twice
                        if blocks_to_commit.iter().any(|(queued_hash, _)| *queued_hash == hash)
                            || read_state.known_block(hash).is_some() {
                            drop_block!(hash, "Checkpoint block {hash} is already queued or committed");
                            continue 'process_packets;
                        }
                    }
                }

                if TRACE { tracing::info!("NewNet: \x1b[93mGOT BLOCK HASH\x1b[0m: {}", hash); }

                // @Todo(Phil): Semantic verification.

                // @Note: Make room if the cache is full. Anything evicted gets re-downloaded if
                // a peer still advertises it, so this is a cost, not a correctness issue.
                if blocks_to_commit.len() >= MAX_DEFERRED_BLOCKS {
                    if let Some(evict_i) = eviction_index(&blocks_to_commit, &read_state) {
                        let (evicted, _) = blocks_to_commit.remove(evict_i);
                        warning!("Block cache full ({MAX_DEFERRED_BLOCKS}); evicting: {}", evicted);
                        settle_fate(&mut fates, &synthetic_peers, evicted, IngestOutcome::Failed {
                            reason: "evicted from the full commit queue".to_string(),
                            misbehavior_score: 0,
                        });
                    }
                }

                println!("Queueing for commit: {}", hash);
                blocks_to_commit.push((hash, std::sync::Arc::new(block)));
                fates.insert(hash, BlockFate { delivered_by: Some(connection_key), ..Default::default() });
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
        let commit_start = std::time::Instant::now();
        let mut commit_head: Option<CommitHead> = None;
        let mut committed_this_tick = 0u64;
        blocks_to_commit.retain(|(hash, block_arc)| {
            if zebra_chain::shutdown::is_shutting_down() { return true; }
            // A pass can verify many blocks, and tenderlink waits on this thread for every
            // Validate and Decided answer. Serve it between blocks, not only between ticks; a
            // decision stored here can also let the next block's fat pointer resolve.
            bft_runner.drain(&read_state, &mut block_writer);
            let hash = *hash;
            let block_arc = block_arc.clone();

            let parent_hash = block_arc.header.previous_block_hash;
            let fate = fates.get_mut(&hash).expect("every queued block has a fate");

            if read_state.known_block(parent_hash).is_none() {
                commit_head.get_or_insert(CommitHead::ParentMissing);
                fate.tell_pending(&synthetic_peers, hash, format_args!("parent {parent_hash} is not committed yet"));
                return true; // keep
            }

            any_blocks_in_the_queue_can_make_progress = true;

            let height = block_arc.coinbase_height().expect("all blocks in the commit queue should already have been confirmed to have a height").0;
            // a deferred block gets retried every tick; only narrate the first attempt
            let first_attempt = fate.cheap_checks.is_none();
            if first_attempt { println!("Committing: @ {height}, {hash}"); }
            // Verify synchronously, immediately before committing. This must NOT run at
            // packet-receipt time: the block that created the outputs this one spends may still
            // be sitting in this queue rather than committed, and the UTXO load would fail for a
            // perfectly valid block.
            let verdict = {
                let network = read_state.network().clone();
                // Matches SemanticBlockVerifier: PoW is skipped on networks that disable it.
                let check_pow = !network.disable_pow();

                // The agreed order: header (PoW gate) -> body (merkle binds the height) ->
                // crosslink gate -> expensive. The crosslink gate MUST come after the body:
                // it makes permanent, height-keyed decisions, and until the merkle root is
                // checked the height is not bound to the PoW'd header.
                let cheap_result = if let Some(cheap) = &fate.cheap_checks {
                    Ok(cheap.clone())
                } else {
                    let res = (verify_fns.check_header)(&block_arc.header, &network, block::Height(height), zebra_debug_time::now(), check_pow)
                        .map_err(|err| ("header", err))
                        .and_then(|()| (verify_fns.check_body)(&block_arc, &network, block::Height(height)).map_err(|err| ("body", err)));
                    if let Ok(cheap) = &res {
                        fate.cheap_checks = Some(cheap.clone());
                    }
                    res
                };
                match cheap_result {
                    Err((phase, err)) => Err((phase, err, false)),
                    Ok(cheap) => {
                        // Crosslink fat-pointer gate, mirroring the state service's version
                        // (:CrosslinkGate in service.rs). Three-way:
                        //   None        -> the parent's pointer or the referenced BFT block is
                        //                  not resolvable yet. Reversible, but per the agreed
                        //                  policy we fail rather than defer: an internal retry
                        //                  queue is unbounded in time, and the network layer
                        //                  re-offers the block anyway.
                        //   Some(false) -> permanently invalid.
                        //   Some(true)  -> proceed.
                        let child_fat_pointer = block_arc.header.fat_pointer_to_bft_block.clone();
                        let parent_fat_pointer = read_state
                            .any_chain_block_header(parent_hash.into())
                            .map(|hdr| hdr.fat_pointer_to_bft_block.clone());

                        // Signatures dominate the Display form and never matter for "why is this
                        // stuck" -- hash + ovd + count is enough to identify the pointer.
                        let fp_brief = |fp: &zcash_primitives::bft::FatPointerToBftBlock| {
                            let v = &fp.vote_for_block_without_finalizer_public_key;
                            format!("{{hash:{} ovd:{} sigs:{}}}", hex::encode(&v[0..32]), hex::encode(&v[32..]), fp.signatures.len())
                        };

                        let (gate, defer_msg) = if let Some(parent_fp) = parent_fat_pointer {
                            let msg = format!("child fp {} / parent fp {} not resolvable yet", fp_brief(&child_fat_pointer), fp_brief(&parent_fp));
                            (bft::admit_fat_pointer(&bft::bft_chain().read().unwrap(), &crosslink_params, &read_state, parent_hash, &parent_fp, &child_fat_pointer, block::Height(height)), msg)
                        } else {
                            // known_block() saw the parent but any_chain_block_header() did not;
                            // the two views disagreeing is itself worth seeing in the log.
                            (None, format!("parent header {parent_hash} not found (child fp {})", fp_brief(&child_fat_pointer)))
                        };

                        // @Todo: this rejects on the REVERSIBLE answer, which costs re-downloads.
                        //
                        // `None` means "not resolvable yet" and is explicitly documented as
                        // reversible; `Some(false)` is permanent. We currently fail on both.
                        // Measured on a fresh sync to height 62: 108 rejections across 20
                        // distinct blocks -- ~5.4 network re-downloads per affected block --
                        // while every other verification phase rejected nothing at all.
                        //
                        // The real fix is not to re-check the gate later, but for new_network to
                        // sync the BFT chain itself. Then it knows when each fat pointer becomes
                        // resolvable and can submit blocks in dependency order, so the gate never
                        // sees an unresolvable pointer and this whole class of retry disappears
                        // by construction. Accepting the redundant traffic until then: it costs
                        // apparent sync speed but keeps this path simple.
                        match gate {
                            // Reversible: the BFT block has not arrived. Hold the block and
                            // re-check next tick rather than dropping and re-downloading it.
                            None => Err(("crosslink", BlockVerifyError { msg: defer_msg, misbehavior_score: 0 }, true)),
                            // Permanent, per the gate's own documentation. Drop it.
                            Some(crate::CrosslinkVerdict::Reject) => Err(("crosslink", BlockVerifyError { msg: "fat pointer regressed, is too early, or does not carry a valid certificate".to_string(), misbehavior_score: 0 }, false)),
                            // `pos_payout` travels with the block to `Chain::push`: the gate is the
                            // only place that resolves the certificate and what it finalizes.
                            Some(crate::CrosslinkVerdict::Accept { pos_payout }) => {
                                // Snapshot on the writer thread before giving pure crypto work
                                // to the worker. Same-block outputs are still resolved by the
                                // verifier itself; it never receives a ReadState or DB handle.
                                let mut spent_utxos = std::collections::HashMap::new();
                                for tx in &block_arc.transactions {
                                    if zebra_chain::shutdown::is_shutting_down() { return true; }
                                    for input in tx.inputs() {
                                        if let zebra_chain::transparent::Input::PrevOut { outpoint, .. } = input {
                                            if !spent_utxos.contains_key(outpoint) {
                                                if let Some(utxo) = read_state.any_chain_utxo(outpoint) {
                                                    spent_utxos.insert(*outpoint, utxo);
                                                }
                                            }
                                        }
                                    }
                                }
                                let result = if fate.trusted {
                                    Ok(zebra_chain::transparent::new_ordered_outputs(&block_arc, &cheap.transaction_hashes))
                                } else {
                                    let Some(result) = verify_expensive_cancellable(
                                        verify_fns, block_arc.clone(), network, cheap.clone(), spent_utxos,
                                    ) else { return true; };
                                    result
                                };
                                match result {
                                    Err(err) => Err(("expensive", err, false)),
                                    Ok(new_outputs) => Ok((cheap, new_outputs, pos_payout)),
                                }
                            }
                        }
                    }
                }
            };

            // Shutdown is not a verification failure: keep the block and its pending
            // reply for teardown rather than reporting a bad peer or starting a write.
            // Once handle_commit starts, it and all bookkeeping below must finish.
            if zebra_chain::shutdown::is_shutting_down() { return true; }

            // A deferrable verdict means "not yet", not "no": keep the block so the next tick
            // can retry it, instead of dropping it and paying for another download.
            if let Err((phase, err, true)) = &verdict {
                commit_head.get_or_insert(if *phase == "crosslink" { CommitHead::BftGate } else { CommitHead::DeferredOther });
                if first_attempt { println!("Deferring: @ {height}, {hash}: {phase}: {}", err.msg); }
                fate.tell_pending(&synthetic_peers, hash, format_args!("{phase}: {}", err.msg));
                return true; // keep
            }

            // The sync path now GATES the commit: a rejection here means the block is never
            // submitted. The old route (verifier router -> StateService -> queue_and_commit ->
            // orphan queue -> sent-hash bookkeeping) is bypassed entirely; the write task is
            // handed a block we already verified, leaving only contextual validation.
            let res = match &verdict {
                Ok((cheap, new_outputs, pos_payout)) => {
                    let semantically_verified = crate::SemanticallyVerifiedBlock {
                        block: block_arc.clone(),
                        hash,
                        height: cheap.height,
                        new_outputs: new_outputs.clone(),
                        transaction_hashes: cheap.transaction_hashes.clone(),
                        pos_payout: *pos_payout,
                    };
                    block_writer
                        .handle_commit(semantically_verified)
                        .map(|()| hash)
                        .map_err(|err| {
                            // Ask the state whether the block is there, not the error's text: the
                            // only commit errors named Duplicate are double spends
                            // (DuplicateTransparentSpend, Duplicate*Nullifier), which are rejections.
                            if known_outcome(&read_state, hash).is_some() {
                                BlockCommitError::Duplicate
                            } else {
                                BlockCommitError::Other(format!("{err:?}"))
                            }
                        })
                }
                Err((phase, err, _)) => Err(BlockCommitError::Other(format!("{phase}: {}", err.msg))),
            };

            let outcome = match (&verdict, &res) {
                (_, Ok(committed)) => IngestOutcome::Committed(*committed),
                (_, Err(BlockCommitError::Duplicate)) => known_outcome(&read_state, hash).unwrap_or(IngestOutcome::Known {
                    location: crate::response::KnownBlockLocation::BestChain,
                    height: block::Height(height),
                }),
                (Err((phase, err, _)), _) => IngestOutcome::Failed {
                    reason: format!("{phase}: {}", err.msg),
                    misbehavior_score: err.misbehavior_score,
                },
                (_, Err(BlockCommitError::Other(why))) => IngestOutcome::Failed {
                    reason: why.clone(),
                    misbehavior_score: 0,
                },
            };
            let failed = matches!(outcome, IngestOutcome::Failed { .. });
            if res.is_ok() {
                committed_this_tick += 1;
            }
            commit_head.get_or_insert(if failed { CommitHead::Failed } else { CommitHead::Committed });
            let delivered_by = settle_fate(&mut fates, &synthetic_peers, hash, outcome);

            // Only a bad header gets the peer killed: the hash commits to the header and nothing
            // else, so the peer advertised a block that cannot exist. A bad body may be forged
            // under an honest block's hash, and the same peer may still serve the honest one.
            if let Some(key) = delivered_by {
                if failed {
                    note_rejection(&mut rejections, hash);
                }
                if let Err(("header", err, _)) = &verdict {
                    if err.misbehavior_score > 0 {
                        peers_to_kill.push((key, format!("served block {hash} with an invalid header: {}", err.msg)));
                    }
                }
            }

            match res {
                Ok(_) => {
                    // handle_commit() already published the new chain state through the tip and
                    // non-finalized-state channels, on this thread, before returning. Anything
                    // that wants the chain view reads it back out of the state.
                    println!("committed!: @ {height}, {hash}");
                    false // remove
                }
                Err(BlockCommitError::Duplicate) => {
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

        sync_profile.record_tick(commit_head, committed_this_tick, commit_start.elapsed(), bft_runner.has_parked_decision());
        sync_profile.report_if_due(
            read_state.best_tip().map_or(0, |(h, _)| h.0),
            bft::bft_chain().read().unwrap().blocks.len(),
            blocks_to_commit.len(),
            peers.iter().map(|(_, peer)| peer.block_downloads.used_flags.count_ones()).sum(),
            current_connections.len(),
        );

        // Blocks at or below the finalized height can never commit, so holding them is pure
        // waste. This replaces a blanket clear() that used to throw away the whole cache --
        // including blocks that were merely waiting for their parent -- whenever no block
        // happened to make progress in a tick.
        let finalized_height = read_state.finalized_tip().map_or(0, |(h, _)| h.0);
        blocks_to_commit.retain(|(hash, block)| {
            let height = block.coinbase_height().map_or(0, |h| h.0);
            if height <= finalized_height {
                if TRACE { tracing::info!("Dropping cached block @ {height} {hash}: at or below finalized {finalized_height}"); }
                let outcome = known_outcome(&read_state, *hash).unwrap_or_else(|| IngestOutcome::Failed {
                    reason: format!("height {height} is at or below the finalized height {finalized_height}"),
                    misbehavior_score: 0,
                });
                settle_fate(&mut fates, &synthetic_peers, *hash, outcome);
                return false;
            }
            true
        });

        // Bound the cache. See MAX_DEFERRED_BLOCKS and eviction_index().
        while blocks_to_commit.len() > MAX_DEFERRED_BLOCKS {
            let Some(evict_i) = eviction_index(&blocks_to_commit, &read_state) else { break; };
            let (evicted, _) = blocks_to_commit.remove(evict_i);
            if TRACE { tracing::info!("Block cache full; evicting: {}", evicted); }
            settle_fate(&mut fates, &synthetic_peers, evicted, IngestOutcome::Failed {
                reason: "evicted from the full commit queue".to_string(),
                misbehavior_score: 0,
            });
        }
        debug_assert_eq!(fates.len(), blocks_to_commit.len());

        for (key, reason) in peers_to_kill.drain(..) {
            if let Some((address, _)) = current_connections.iter().find(|(addr, _)| addr.connection_key() == key).cloned() {
                kill_peer(&mut current_connections, &mut recent_peer_addresses, &mut synthetic_peers, local_addresses_secret, &address, reason);
                dbg_panic!();
            }
        }

        let _ = any_blocks_in_the_queue_can_make_progress;

        // The remainder of the tick goes to BFT requests, so a round does not wait out the tick.
        if !zebra_chain::shutdown::is_shutting_down() {
            bft_runner.wait(loop_start + tick_duration, &read_state, &mut block_writer);
        }
    }
    rt.block_on(block_writer.finish_backup());
    tracing::info!("NewNet: writer stopped after completing in-flight commits");
}

mod tests {
    #[allow(unused_imports)]
    use super::*;

    #[cfg(test)]
    fn parse_chunk(packet: &[u8]) -> (PeerPowBlockResponseChunkHdr, &[u8]) {
        assert_eq!(packet[0], PACKET_TYPE_BLOCK_CHUNK);
        let mut msg = &packet[1..];
        let hdr = PeerPowBlockResponseChunkHdr::read_from(&mut msg).unwrap();
        (hdr, msg)
    }

    #[test]
    fn max_size_block_is_answered_in_one_bounded_message() {
        let block: Vec<u8> = (0..BLOCK_RESPONSE_MAX_PAYLOAD).map(|i| (i * 7) as u8).collect();
        let hash = Hash([9; 32]);
        let packets = block_chunk_packets(&block, 5, hash, 0);
        assert_eq!(packets.len(), 1);
        assert!(packets[0].len() <= BLOCK_RESPONSE_MAX_MESSAGE);
        let (hdr, payload) = parse_chunk(&packets[0]);
        assert_eq!((hdr.offset, hdr.size, hdr.height_hash.height.0), (0, block.len() as u32, 5));
        let mut r = BlockReassembly::default();
        assert_eq!(r.insert(hdr.offset as usize, payload, true), (true, true));
        assert_eq!(r.buf, block);

        // A request that resumes from a prefix gets exactly the remainder, again in one message.
        let packets = block_chunk_packets(&block, 5, hash, 1000);
        assert_eq!(packets.len(), 1);
        let (hdr, payload) = parse_chunk(&packets[0]);
        assert_eq!(hdr.offset, 1000);
        assert_eq!(payload, &block[1000..]);
    }

    #[test]
    fn block_reassembly_resumes_and_rejects_inconsistent_fragments() {
        let block: Vec<u8> = (0..10_000u32).map(|i| i as u8).collect();
        let mut r = BlockReassembly::default();
        assert_eq!(r.insert(0, &block[..3000], false), (true, false));
        // a lost middle fragment leaves a gap; the prefix is what a retry resumes from
        assert_eq!(r.insert(6000, &block[6000..], true), (true, false));
        assert_eq!(r.received.first().map(|r| r.1), Some(3000));
        // overlapping retransmission with the same bytes is accepted and completes it
        assert_eq!(r.insert(2000, &block[2000..7000], false), (true, true));
        assert_eq!(r.buf, block);

        let mut r = BlockReassembly::default();
        assert_eq!(r.insert(0, &block[..100], false), (true, false));
        let mut forged = block[50..150].to_vec();
        forged[0] ^= 1;
        assert_eq!(r.insert(50, &forged, false).0, false, "conflicting overlap");
        assert_eq!(r.insert(100, &block[100..200], true), (true, true));
        assert_eq!(r.insert(150, &block[150..300], false).0, false, "past the declared end");
        assert_eq!(r.buf.len(), 200, "buffer never grows past the declared end");
    }

    #[test]
    fn block_request_retry_backs_off_to_a_cap() {
        let secs: Vec<u64> = (1..=6).map(|a| block_request_retry_delay(a).as_millis() as u64).collect();
        assert_eq!(secs, vec![1000, 2000, 4000, 4000, 4000, 4000]);
        assert_eq!(block_request_retry_delay(u8::MAX), BLOCK_REQUEST_RETRY_MAX);
        // the capped backoff leaves a requested slot many retries before its response deadline
        assert!(BLOCK_REQUEST_RETRY_MAX * 30 < default_block_response_deadline());
    }

    #[cfg(test)]
    fn default_block_response_deadline() -> std::time::Duration {
        block_response_deadline(tenderlink::transport::TransportLimits::default().peer_outbox_bytes as u64)
    }

    #[test]
    fn block_response_deadline_drains_the_whole_default_bulk_cap_at_the_floor_rate() {
        // 8 MiB / 64 KiB/s = 128s, + 8s allowance
        assert_eq!(default_block_response_deadline(), std::time::Duration::from_secs(136));
        assert_eq!(block_response_deadline(1 << 20), std::time::Duration::from_secs(16 + 8));
    }

    #[test]
    fn never_requested_slot_still_expires_after_16s() {
        let deadline = default_block_response_deadline();
        let t0 = std::time::Instant::now();
        let ms = std::time::Duration::from_millis;
        let mut dl = PeerPowBlockDownload { last_modified: t0, ..Default::default() };
        assert!(!block_download_expired(&mut dl, t0 + ms(8000), deadline));
        assert!(!block_download_expired(&mut dl, t0 + ms(8001), deadline), "first quiet period is a strike");
        assert_eq!(dl.timeout_strikes, 1);
        assert!(!block_download_expired(&mut dl, t0 + ms(16001), deadline));
        assert!(block_download_expired(&mut dl, t0 + ms(16002), deadline));
        assert!(dl.first_requested.is_none());
    }

    #[test]
    fn requested_slot_lives_until_its_original_deadline_and_nothing_renews_it() {
        let deadline = default_block_response_deadline();
        let t0 = std::time::Instant::now();
        let s = std::time::Duration::from_secs;
        let mut dl = PeerPowBlockDownload { last_modified: t0, ..Default::default() };
        assert!(block_request_due(&mut dl, t0, deadline));
        assert_eq!(dl.first_requested, Some(t0));
        // well past the old 8s + strike, with no fragment at all
        for at in [s(9), s(17), s(40), s(100)] {
            assert!(!block_download_expired(&mut dl, t0 + at, deadline), "{at:?}");
        }
        // retries, a partial fragment and a refreshed quiet timer leave the anchor alone
        assert!(block_request_due(&mut dl, t0 + s(101), deadline));
        note_block_fragment(&mut dl, t0 + s(120));
        dl.reassembly.insert(0, &[1, 2, 3], false);
        assert!(block_request_due(&mut dl, t0 + s(125), deadline));
        assert_eq!(dl.first_requested, Some(t0));
        // exact boundary: alive at the deadline, gone just after it
        assert!(!block_download_expired(&mut dl, t0 + deadline, deadline));
        assert!(block_download_expired(&mut dl, t0 + deadline + std::time::Duration::from_nanos(1), deadline));
        // and no request goes out past it
        assert!(!block_request_due(&mut dl, t0 + deadline + s(5), deadline));
        // a refilled slot (new generation) starts unanchored
        let mut downloads = BlockDownloads::default();
        let hh = HeightAndHashOr0 { height: Height(1), hash_or_0: Hash([1; 32]) };
        let i = downloads.insert(hh).unwrap();
        downloads.slots[i] = dl;
        downloads.remove(i);
        assert_eq!(downloads.insert(hh), Some(i));
        assert_eq!(downloads.slots[i].first_requested, None);
    }

    #[test]
    fn missing_peer_slot_expires_on_time_after_a_bounded_number_of_requests() {
        // Production cadence on a virtual clock: dl-init (expiry) every 500ms, requests every
        // 300ms; simulated on a 100ms grid. The peer never answers anything.
        let deadline = default_block_response_deadline();
        let t0 = std::time::Instant::now();
        let mut dl = PeerPowBlockDownload { last_modified: t0, ..Default::default() };
        let (mut requests, mut request_times, mut expired_at) = (0, Vec::new(), None);
        for tick in 0..2000u64 {
            let ms = tick * 100;
            let now = t0 + std::time::Duration::from_millis(ms);
            if ms % 500 == 0 && block_download_expired(&mut dl, now, deadline) {
                expired_at = Some(ms);
                break;
            }
            if ms % 300 == 0 && block_request_due(&mut dl, now, deadline) {
                requests += 1;
                request_times.push(ms);
            }
        }
        assert_eq!(expired_at, Some(136_500), "first dl-init pass after the deadline");
        assert!(requests <= 36, "{requests} requests: {request_times:?}");
        assert!(requests >= 30, "{requests} requests: {request_times:?}");
        assert!(request_times.windows(2).all(|w| w[1] - w[0] >= 1000));
        assert!(*request_times.last().unwrap() <= 136_000);
    }

    #[test]
    fn queued_block_responses_skip_duplicates_until_the_bulk_backlog_drains() {
        let mut q = QueuedBlockResponses::default();
        let (a, b) = (Hash([1; 32]), Hash([2; 32]));
        assert_eq!(admit_block_response(&q, a, 0, Some(10_000), 0, 2_000), ResponseAdmission::Admit);
        q.record(a, 0);
        // retries of the queued response, any budget: skipped
        assert_eq!(admit_block_response(&q, a, 0, Some(u64::MAX), 0, 1), ResponseAdmission::Duplicate);
        assert_eq!(admit_block_response(&q, a, 0, None, 0, 1), ResponseAdmission::Duplicate);
        // a second, different block or suffix is not a duplicate
        assert_eq!(admit_block_response(&q, b, 0, Some(10_000), 2_000, 2_000), ResponseAdmission::Admit);
        assert_eq!(admit_block_response(&q, a, 100, Some(10_000), 2_000, 2_000), ResponseAdmission::Admit);
        // same-tick reservation counts against the budget
        assert_eq!(admit_block_response(&q, b, 0, Some(10_000), 8_001, 2_000), ResponseAdmission::NoBudget);
        assert_eq!(admit_block_response(&q, b, 0, Some(10_000), 8_000, 2_000), ResponseAdmission::Admit);
        // synthetic peers have no STP budget
        assert_eq!(admit_block_response(&q, b, 0, None, u64::MAX, u64::MAX), ResponseAdmission::Admit);
        // not drained: kept; drained: forgotten, so a lost response can be retried
        q.forget_if_drained(Some(1));
        assert_eq!(admit_block_response(&q, a, 0, Some(10_000), 0, 2_000), ResponseAdmission::Duplicate);
        q.forget_if_drained(Some(0));
        assert_eq!(admit_block_response(&q, a, 0, Some(10_000), 0, 2_000), ResponseAdmission::Admit);
        // the record is capped; full means wait for the drain, never evict
        for i in 0..MAX_QUEUED_BLOCK_RESPONSES_PER_PEER as u32 {
            q.record(a, i);
        }
        assert_eq!(admit_block_response(&q, b, 0, None, 0, 1), ResponseAdmission::RecordFull);
        assert_eq!(admit_block_response(&q, a, 3, None, 0, 1), ResponseAdmission::Duplicate);
        // a synthetic peer (no backlog) is drained every tick, never stuck
        q.forget_if_drained(None);
        assert_eq!(admit_block_response(&q, b, 0, None, 0, 1), ResponseAdmission::Admit);
        assert!(q.queued.is_empty());
    }

    /// Two real STP pumps on loopback. The responder's Bulk flow is rate-capped and already
    /// holds 24 other block responses for us (stale or probe-like: we have no slot for them),
    /// so our max-size block, one whole STP message, lands only after the old 8s + strike
    /// timeout (16s) with no fragment before it. The requester runs the production slot,
    /// expiry, request and fragment helpers; the responder the production duplicate guard and
    /// Bulk budget. The block must complete into the SAME slot with its original anchor, with
    /// no second copy queued while the first is untransmitted, while Control pings stay fast.
    #[test]
    fn slow_whole_block_completes_in_its_original_slot() {
        use tenderlink::transport::{NetworkThreadHandle, TransportLimits};
        use std::time::{Duration, Instant};
        fn free_port() -> u16 {
            std::net::UdpSocket::bind("[::1]:0").unwrap().local_addr().unwrap().port()
        }
        fn node(max_pps: Option<u64>) -> (NetworkThreadHandle, STPAddress) {
            let key = new_keypair_from_connect_magic1(CRYPTO_MAGIC).unwrap();
            let port = free_port();
            let address = STPAddress::from(std::net::Ipv6Addr::LOCALHOST, port, &key);
            let h = NetworkThreadHandle::new(vec![key], port, max_pps, (1_000_000, 512 * 1024, 32 << 20), TransportLimits::default());
            (h, address)
        }
        fn pseudo_random(len: usize, mut x: u64) -> Vec<u8> {
            (0..len).map(|_| { x ^= x << 13; x ^= x >> 7; x ^= x << 17; x as u8 }).collect()
        }
        const PING: u8 = 0xF0;
        const PONG: u8 = 0xF1;
        const BLOCK_LEN: usize = BLOCK_RESPONSE_MAX_PAYLOAD;
        const AHEAD: usize = 24;
        // Measured on loopback: STP moves ~5 KB of payload per unit of `max_pps`, so 20 gives
        // ~100 KB/s and the 25 max-size responses take ~20-30s.
        let pps = 20;

        let (mut req, req_addr) = node(None);
        let (mut resp, resp_addr) = node(Some(pps));
        let deadline = block_response_deadline(resp.limits().peer_outbox_bytes as u64);

        let mut push = NetworkThreadPush { initiate_connections: vec![resp_addr.clone()], ..Default::default() };
        let connect_by = Instant::now() + Duration::from_secs(10);
        let (to_resp, to_req) = loop {
            let pr = req.service(std::mem::take(&mut push));
            let ps = resp.service(NetworkThreadPush::default());
            if let (Some(a), Some(b)) = (pr.current_connections.first(), ps.current_connections.first()) {
                break (a.0.connection_key(), b.0.connection_key());
            }
            assert!(Instant::now() < connect_by, "connect timed out");
            std::thread::sleep(Duration::from_millis(5));
        };

        let block = pseudo_random(BLOCK_LEN, 0x9E37_79B9_7F4A_7C15);
        let hash = Hash([0x5A; 32]);
        let hh = HeightAndHashOr0 { height: Height(1234), hash_or_0: hash };
        let one_copy = (BLOCK_LEN + BLOCK_RESPONSE_MAX_MESSAGE - BLOCK_RESPONSE_MAX_PAYLOAD) as u64;

        // the responder's Bulk backlog to us, queued through the same guard and budget
        let mut queued = QueuedBlockResponses::default();
        let mut push_resp = NetworkThreadPush::default();
        let mut reserved = 0u64;
        for i in 0..AHEAD {
            let other = Hash([i as u8; 32]);
            assert_eq!(admit_block_response(&queued, other, 0, Some(resp.send_budget(to_req, Channel::Bulk)), reserved, one_copy), ResponseAdmission::Admit);
            reserved += one_copy;
            queued.record(other, 0);
            push_resp.send_bulk.extend(block_chunk_packets(&pseudo_random(BLOCK_LEN, i as u64 + 1), 1000 + i as u32, other, 0).into_iter().map(|p| (to_req, p)));
        }
        resp.service(push_resp);
        assert_eq!(resp.send_backlog_bytes(to_req, Channel::Bulk), Some(AHEAD as u64 * one_copy));

        let mut downloads = BlockDownloads::default();
        let slot = downloads.insert(hh).unwrap();
        // the old rule, for the counterexample: same slot, never anchored
        let mut old_rule = PeerPowBlockDownload { last_modified: Instant::now(), ..Default::default() };
        let mut old_rule_cancelled_at = None;

        let (mut admitted, mut duplicates, mut requests, mut stale_chunks) = (0u32, 0u32, 0u32, 0u32);
        let mut pings: HashMap<u32, Instant> = HashMap::new();
        let (mut ping_seq, mut max_rtt, mut pongs) = (0u32, Duration::ZERO, 0u32);
        let (mut next_request, mut next_init, mut next_ping) = (Instant::now(), Instant::now(), Instant::now());
        let mut req_inbox: Vec<Vec<u8>> = Vec::new();
        let mut first_requested = None;
        let mut ours_queued_since: Option<Instant> = None;
        let mut completed = None;
        let started = Instant::now();

        while completed.is_none() {
            let now = Instant::now();
            assert!(now < started + Duration::from_secs(90), "transfer did not complete");
            let mut push_req = NetworkThreadPush::default();
            if now >= next_init {
                next_init = now + Duration::from_millis(500);
                assert!(downloads.slot_is_used(slot));
                assert!(!block_download_expired(&mut downloads.slots[slot], now, deadline), "slot expired after {:?}", now - started);
                if old_rule_cancelled_at.is_none() && block_download_expired(&mut old_rule, now, deadline) {
                    old_rule_cancelled_at = Some(now - started);
                }
            }
            if now >= next_request {
                next_request = now + Duration::from_millis(300);
                if block_request_due(&mut downloads.slots[slot], now, deadline) {
                    requests += 1;
                    push_req.send_unreliable.push((to_resp, block_request_packet(downloads.slots[slot].height_hash, 0)));
                }
                first_requested = first_requested.or(downloads.slots[slot].first_requested);
                assert_eq!(downloads.slots[slot].first_requested, first_requested, "anchor moved");
            }
            if now >= next_ping {
                next_ping = now + Duration::from_millis(500);
                pings.insert(ping_seq, now);
                let mut p = vec![PING];
                p.extend_from_slice(&ping_seq.to_le_bytes());
                push_req.send_unreliable.push((to_resp, p));
                ping_seq += 1;
            }
            push_req.wanted_connections = vec![(resp_addr.clone(), [0; 64])];
            let pr = req.service(push_req);
            assert!(req.is_connected(to_resp));
            for (_, msg) in pr.received_unreliable_messages {
                match msg[0] {
                    PONG => {
                        let seq = u32::from_le_bytes(msg[1..5].try_into().unwrap());
                        max_rtt = max_rtt.max(pings.remove(&seq).unwrap().elapsed());
                        pongs += 1;
                    }
                    PACKET_TYPE_BLOCK_CHUNK => {
                        let (hdr, payload) = parse_chunk(&msg);
                        // like production: a response with no slot is discarded
                        let Some(dl_i) = downloads.position(hdr.height_hash) else {
                            assert_ne!(hdr.height_hash, hh);
                            stale_chunks += 1;
                            continue;
                        };
                        assert_eq!(dl_i, slot, "the response must find its original slot");
                        let is_fin = hdr.offset as usize + payload.len() == hdr.size as usize;
                        let dl = &mut downloads.slots[slot];
                        let (ok, complete) = dl.reassembly.insert(hdr.offset as usize, payload, is_fin);
                        assert!(ok && complete, "a block is one whole message");
                        note_block_fragment(dl, Instant::now());
                        let first = dl.first_requested.unwrap();
                        completed = Some((first.elapsed(), downloads.remove(slot).reassembly.buf));
                    }
                    other => panic!("unexpected packet {other}"),
                }
            }

            // responder tick: the forget point first, then answer what arrived last tick
            let backlog = resp.send_backlog_bytes(to_req, Channel::Bulk).unwrap_or(0);
            queued.forget_if_drained(Some(backlog));
            let mut push_resp = NetworkThreadPush::default();
            let mut reserved = 0u64;
            for msg in req_inbox.drain(..) {
                if msg[0] == PING {
                    push_resp.send_unreliable.push((to_req, [&[PONG][..], &msg[1..]].concat()));
                    continue;
                }
                let (height, hash, offset) = parse_block_request(&msg).unwrap();
                let needed = (BLOCK_LEN - offset as usize + BLOCK_RESPONSE_MAX_MESSAGE - BLOCK_RESPONSE_MAX_PAYLOAD) as u64;
                match admit_block_response(&queued, hash, offset, Some(resp.send_budget(to_req, Channel::Bulk)), reserved, needed) {
                    ResponseAdmission::Admit => {
                        // a second copy only once the first has left the responder entirely
                        assert!(ours_queued_since.is_none() || backlog == 0, "second copy queued behind an untransmitted first");
                        ours_queued_since = Some(Instant::now());
                        admitted += 1;
                        reserved += needed;
                        queued.record(hash, offset);
                        for packet in block_chunk_packets(&block, height, hash, offset as usize) {
                            push_resp.send_bulk.push((to_req, packet));
                        }
                    }
                    ResponseAdmission::Duplicate => duplicates += 1,
                    other => panic!("unexpected admission {other:?}"),
                }
            }
            push_resp.wanted_connections = vec![(req_addr.clone(), [0; 64])];
            let ps = resp.service(push_resp);
            req_inbox.extend(ps.received_unreliable_messages.into_iter().map(|(_, m)| m));
            let backlog = resp.send_backlog_bytes(to_req, Channel::Bulk).unwrap_or(0);
            assert!(backlog <= (AHEAD as u64 + 1) * one_copy, "Bulk backlog {backlog}: more than one copy of ours");
            std::thread::sleep(Duration::from_millis(10));
        }

        let (since_first_request, buf) = completed.unwrap();
        assert!(buf == block, "payload corrupted");
        assert!(since_first_request > 2 * DOWNLOAD_UNMODIFIED_TIMEOUT_DUR, "transfer too fast to test anything: {since_first_request:?}");
        assert!(since_first_request < deadline);
        assert!(old_rule_cancelled_at.is_some(), "the old 8s + strike rule would have cancelled this slot");
        assert_eq!(stale_chunks, AHEAD as u32, "every stale response arrived first and was discarded");
        // A retry landing after our copy left the responder but before the requester decoded
        // it may legitimately be answered again; the assertion above rules out a copy queued
        // behind an untransmitted one.
        assert!((1..=2).contains(&admitted), "{admitted} copies admitted");
        assert!(duplicates >= 3 && requests >= 4, "retries happened and were deduplicated: {requests} requests, {duplicates} duplicates");
        assert!(pongs >= 20, "{pongs} pongs");
        assert!(max_rtt < Duration::from_secs(1), "Control ping took {max_rtt:?} behind the Bulk backlog");
        eprintln!("slow block: {since_first_request:?}, {admitted} copies, {requests} requests, {duplicates} duplicates, max ping rtt {max_rtt:?}, pps {pps}, old rule cancelled at {old_rule_cancelled_at:?}");
    }

    #[test]
    fn cpu_verification_shutdown_does_not_wait_for_worker() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let (ready_sender, ready_receiver) = std::sync::mpsc::channel();
        let (release_sender, release_receiver) = std::sync::mpsc::channel();
        let (finished_sender, finished_receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            ready_sender.send(()).unwrap();
            release_receiver.recv().unwrap();
            let _ = sender.send(42);
            finished_sender.send(()).unwrap();
        });
        ready_receiver.recv_timeout(std::time::Duration::from_secs(1)).unwrap();
        let checks = std::cell::Cell::new(0);
        let start = std::time::Instant::now();
        let result = await_cpu_verification(receiver, worker, || {
            checks.set(checks.get() + 1);
            checks.get() >= 2
        });
        assert_eq!(result, None, "cancelled result must not reach the commit path");
        assert!(start.elapsed() < std::time::Duration::from_millis(250));
        assert!(matches!(finished_receiver.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty)));
        // Release and observe completion so this test leaves no background worker.
        release_sender.send(()).unwrap();
        finished_receiver.recv_timeout(std::time::Duration::from_secs(1)).unwrap();
    }

    #[test]
    fn cpu_verification_success_joins_worker() {
        let (sender, receiver) = std::sync::mpsc::channel();
        let finished = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_finished = finished.clone();
        let worker = std::thread::spawn(move || {
            sender.send(42).unwrap();
            worker_finished.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        assert_eq!(await_cpu_verification(receiver, worker, || false), Some(42));
        assert!(finished.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[test]
    #[should_panic(expected = "synthetic verification panic")]
    fn cpu_verification_panic_is_propagated() {
        let (sender, receiver) = std::sync::mpsc::channel::<()>();
        let worker = std::thread::spawn(move || {
            let _sender = sender;
            panic!("synthetic verification panic");
        });
        let _ = await_cpu_verification(receiver, worker, || false);
    }

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

        let _buf = [0u8; PACKET_STATUS_MAX_SIZE];

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
        let _tip_height = 11;

        debug_assert_eq!(chains, chains2, "building incrementally should be functionally equivalent to batch-built");
        debug_assert_eq!(chains.tip_height(), Some(11));
        print_shadow_block_slice(blocks[0].this_height, &blocks, 1);
        eprintln!("");

        chains.roundtrip_to_branches(PACKET_STATUS_MAX_SIZE, 1);

//         println!("{}", hex::encode(buf));
    }
}

