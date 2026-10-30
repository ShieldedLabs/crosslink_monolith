//! Decided BFT blocks as sync data: what a node catching up fetches from its PoW peers, so the
//! decided chain arrives as fast as the PoW blocks that cite it instead of one height per
//! tenderlink status exchange.
//!
//! Three messages, all on new_network's STP connections:
//! - `BFT_STATUS`: how many decided heights the sender holds (its BFT chain length).
//! - `BFT_REQ`: a range of heights wanted.
//! - `BFT_DECISIONS`: consecutive decided heights from the requested start, each with its
//!   certificate and the proposer's chunk signatures, as the database stores them.
//!
//! A decision fetched this way is data, not a vote: the receiver checks it exactly as a
//! force-fed decision (bft.rs `BftRequest::ForceFeed`) and additionally requires its certificate
//! to carry a quorum of the height's roster.

use zcash_primitives::bft::{BftBlock, FatPointerToBftBlock, TMSig};
use zebra_chain::serialization::{ZcashDeserialize, ZcashSerialize};

use super::{PACKET_TYPE_BFT_DECISIONS, PACKET_TYPE_BFT_REQ, PACKET_TYPE_BFT_STATUS};

/// Most decided heights one request may ask for, and one response may carry.
pub const MAX_DECISIONS_PER_MESSAGE: u16 = 64;

/// A response stops adding decisions once it reaches this many bytes. A decision is about 10 KB
/// with σ = 4 headers and at most a few hundred KB, so 64 of them fit well under the transport's
/// 4 MiB message limit (`tenderlink::transport::MAX_MESSAGE_SIZE`).
pub const DECISIONS_MESSAGE_BUDGET: usize = 2 << 20;

/// One decided height as it travels between peers.
#[derive(Clone, Debug, PartialEq)]
pub struct WireDecision {
    pub height: u64,
    pub block: BftBlock,
    pub fat_pointer: FatPointerToBftBlock,
    pub proposal_sigs: Vec<TMSig>,
}

pub fn bft_status_packet(decided_len: u64) -> Vec<u8> {
    let mut out = vec![PACKET_TYPE_BFT_STATUS];
    out.extend_from_slice(&decided_len.to_le_bytes());
    out
}

pub fn bft_req_packet(from: u64, count: u16) -> Vec<u8> {
    let mut out = vec![PACKET_TYPE_BFT_REQ];
    out.extend_from_slice(&from.to_le_bytes());
    out.extend_from_slice(&count.min(MAX_DECISIONS_PER_MESSAGE).to_le_bytes());
    out
}

/// Encode consecutive decisions, stopping before the one that would pass the byte budget (the
/// first is always included, so a large decision still travels alone).
pub fn bft_decisions_packet<'a>(decisions: impl IntoIterator<Item = (u64, &'a BftBlock, &'a FatPointerToBftBlock, &'a [TMSig])>) -> Vec<u8> {
    let mut out = vec![PACKET_TYPE_BFT_DECISIONS, 0, 0];
    let mut n: u16 = 0;
    for (height, block, fat_pointer, proposal_sigs) in decisions {
        if n == MAX_DECISIONS_PER_MESSAGE {
            break;
        }
        let mut item = Vec::new();
        item.extend_from_slice(&height.to_le_bytes());
        let block_bytes = block.zcash_serialize_to_vec().expect("a decided BFT block serializes");
        let mut fp_bytes = Vec::new();
        fat_pointer.zcash_serialize(&mut fp_bytes).expect("writing a fat pointer to a Vec cannot fail");
        for bytes in [&block_bytes, &fp_bytes] {
            // Both are bounded far below u32::MAX: a BFT block by tenderlink's 1 MiB proposal
            // limit, a fat pointer by its u16 signature count.
            item.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
            item.extend_from_slice(bytes);
        }
        // At most one signature per proposal chunk, which a 1 MiB proposal keeps far below u16::MAX.
        item.extend_from_slice(&(proposal_sigs.len() as u16).to_le_bytes());
        for sig in proposal_sigs {
            item.extend_from_slice(&sig.0);
        }
        if n > 0 && out.len() + item.len() > DECISIONS_MESSAGE_BUDGET {
            break;
        }
        out.extend_from_slice(&item);
        n += 1;
    }
    out[1..3].copy_from_slice(&n.to_le_bytes());
    out
}

/// A cursor over a received message body (after the type byte). Every read is bounds-checked.
pub(super) struct Reader<'a>(pub(super) &'a [u8]);

impl<'a> Reader<'a> {
    pub(super) fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Some(head)
    }
    pub(super) fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }
    pub(super) fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    pub(super) fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
}

/// `body` is the message after its type byte. `None` when malformed or carrying trailing bytes.
pub fn read_bft_status(body: &[u8]) -> Option<u64> {
    let mut r = Reader(body);
    let decided_len = r.u64()?;
    r.0.is_empty().then_some(decided_len)
}

/// `(from, count)`, with `count` capped at [`MAX_DECISIONS_PER_MESSAGE`].
pub fn read_bft_req(body: &[u8]) -> Option<(u64, u16)> {
    let mut r = Reader(body);
    let from = r.u64()?;
    let count = r.u16()?;
    r.0.is_empty().then_some((from, count.min(MAX_DECISIONS_PER_MESSAGE)))
}

/// Assumption, established at ingest: every BFT block a peer gives us has at least one header.
/// `read_bft_decisions` refuses one with none, and nothing else turns peer bytes into a block.
/// Code that relies on it (a block's snapshot is `headers[0]`'s parent) names this, so removing
/// the check here means revisiting every such site.
pub(crate) const ASSUME_BFT_BLOCK_HAS_HEADERS: () = ();

/// The decisions in a `BFT_DECISIONS` body, which must be consecutive heights. Blocks must be
/// canonically encoded, as tenderlink requires of proposals (`bft_block_from_canonical_bytes`).
pub fn read_bft_decisions(body: &[u8]) -> Option<Vec<WireDecision>> {
    let mut r = Reader(body);
    let n = r.u16()?;
    if n > MAX_DECISIONS_PER_MESSAGE {
        return None;
    }
    let mut decisions = Vec::with_capacity(n.into());
    for _ in 0..n {
        let height = r.u64()?;
        let block_len = r.u32()? as usize;
        // Tenderlink decides nothing larger, so a larger block is not a decision.
        if block_len > tenderlink::PROPOSAL_MAX_SIZE {
            return None;
        }
        let block = super::bft::bft_block_from_canonical_bytes(r.take(block_len)?)?;
        // ASSUME_BFT_BLOCK_HAS_HEADERS: a block's snapshot is its first header's parent, so a
        // block with none is no block.
        if block.headers.is_empty() {
            return None;
        }
        let fp_len = r.u32()? as usize;
        let mut fp_bytes = r.take(fp_len)?;
        let fat_pointer = FatPointerToBftBlock::zcash_deserialize(&mut fp_bytes).ok()?;
        if !fp_bytes.is_empty() {
            return None;
        }
        let sigs_n = r.u16()?;
        // A decision carries no proposal signatures (genesis, or ones that would not verify
        // fresh), or one per chunk of its block, as tenderlink signs proposals.
        if sigs_n != 0 && usize::from(sigs_n) != block_len.div_ceil(tenderlink::PROPOSAL_CHUNK_DATA_SIZE) {
            return None;
        }
        let mut proposal_sigs = Vec::with_capacity(usize::from(sigs_n).min(r.0.len() / 64));
        for _ in 0..sigs_n {
            proposal_sigs.push(TMSig(r.take(64)?.try_into().ok()?));
        }
        if let Some(prev) = decisions.last().map(|d: &WireDecision| d.height) {
            if height != prev + 1 {
                return None;
            }
        }
        decisions.push(WireDecision { height, block, fat_pointer, proposal_sigs });
    }
    r.0.is_empty().then_some(decisions)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zcash_primitives::block::{BlockHash, BlockHeaderData as BcBlockHeader};

    /// A decision with no proposal signatures, or (`signed`) one per chunk of its block.
    fn decision(height: u64, signed: bool) -> WireDecision {
        let header = BcBlockHeader {
            version: 4,
            prev_block: BlockHash([1; 32]),
            merkle_root: [2; 32],
            final_sapling_root: [3; 32],
            time: 4,
            bits: 5,
            nonce: [6; 32],
            solution: vec![7; 1344],
            fat_pointer_to_bft_block: FatPointerToBftBlock::null(),
        };
        let block = BftBlock {
            version: 2,
            // Heights fit u32 in these tests.
            height: height as u32,
            previous_block_fat_ptr: FatPointerToBftBlock::null(),
            headers: vec![header; 4],
            slashforks: Vec::new(),
            do_not_include_until_bc_height: 0,
        };
        let sigs = if signed { chunks_n(&block) } else { 0 };
        WireDecision {
            height,
            block,
            fat_pointer: FatPointerToBftBlock::null(),
            proposal_sigs: (0..sigs).map(|i| TMSig([i as u8; 64])).collect(),
        }
    }

    fn chunks_n(block: &BftBlock) -> usize {
        block.zcash_serialize_to_vec().unwrap().len().div_ceil(tenderlink::PROPOSAL_CHUNK_DATA_SIZE)
    }

    fn encode(decisions: &[WireDecision]) -> Vec<u8> {
        bft_decisions_packet(decisions.iter().map(|d| (d.height, &d.block, &d.fat_pointer, d.proposal_sigs.as_slice())))
    }

    #[test]
    fn bft_sync_messages_round_trip() {
        let status = bft_status_packet(12_345);
        assert_eq!(status[0], PACKET_TYPE_BFT_STATUS);
        assert_eq!(read_bft_status(&status[1..]), Some(12_345));

        let req = bft_req_packet(70, 500);
        assert_eq!(req[0], PACKET_TYPE_BFT_REQ);
        assert_eq!(read_bft_req(&req[1..]), Some((70, MAX_DECISIONS_PER_MESSAGE)));

        let decisions = vec![decision(5, true), decision(6, false), decision(7, true)];
        let packet = encode(&decisions);
        assert_eq!(packet[0], PACKET_TYPE_BFT_DECISIONS);
        assert_eq!(read_bft_decisions(&packet[1..]), Some(decisions));
    }

    #[test]
    fn malformed_bft_sync_messages_are_refused() {
        let decisions = vec![decision(5, true), decision(6, true)];
        let packet = encode(&decisions);

        // Truncated anywhere, or with a trailing byte.
        for cut in 1..packet.len() - 1 {
            assert_eq!(read_bft_decisions(&packet[1..cut]), None, "cut at {cut}");
        }
        let mut trailing = packet.clone();
        trailing.push(0);
        assert_eq!(read_bft_decisions(&trailing[1..]), None);

        // Heights that do not follow on from each other.
        let gap = encode(&[decision(5, true), decision(7, true)]);
        assert_eq!(read_bft_decisions(&gap[1..]), None);

        // A block with no headers has no snapshot (asking for it panics). It encodes canonically
        // and carries no signatures, so only the explicit check refuses it.
        let mut headerless = decision(5, false);
        headerless.block.headers.clear();
        let headerless = encode(&[headerless]);
        assert_eq!(read_bft_decisions(&headerless[1..]), None);

        // Signatures neither absent nor one per chunk of the block.
        let mut padded = decision(5, true);
        padded.proposal_sigs.push(TMSig([9; 64]));
        assert_eq!(read_bft_decisions(&encode(&[padded])[1..]), None);

        // A count beyond the cap, before anything is allocated for it.
        let mut too_many = vec![0u8; 2];
        too_many[..2].copy_from_slice(&(MAX_DECISIONS_PER_MESSAGE + 1).to_le_bytes());
        assert_eq!(read_bft_decisions(&too_many), None);

        assert_eq!(read_bft_status(&[1, 2, 3]), None);
        assert_eq!(read_bft_req(&[0; 11]), None);
    }

    #[test]
    fn a_block_larger_than_any_proposal_is_refused_before_decoding() {
        // A well-formed block, canonically encoded, that only the size cap refuses.
        let mut big = decision(5, false);
        big.block.headers = vec![big.block.headers[0].clone(); 1000];
        assert!(big.block.zcash_serialize_to_vec().unwrap().len() > tenderlink::PROPOSAL_MAX_SIZE);
        assert_eq!(read_bft_decisions(&encode(&[big])[1..]), None);
    }

    #[test]
    fn a_decisions_message_stays_within_its_budget() {
        let big: Vec<WireDecision> = (0..MAX_DECISIONS_PER_MESSAGE as u64 + 10).map(|h| decision(h, true)).collect();
        let packet = encode(&big);
        let read = read_bft_decisions(&packet[1..]).unwrap();
        assert!(read.len() <= MAX_DECISIONS_PER_MESSAGE as usize);
        assert!(packet.len() <= DECISIONS_MESSAGE_BUDGET);
        assert_eq!(read[0].height, 0);
    }
}
