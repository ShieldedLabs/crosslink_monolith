use static_assertions::*;
use std::{io::Write, mem::align_of, mem::size_of};
use zebra_chain::serialization::{ZcashDeserialize, ZcashSerialize, SerializationError};
use zebra_chain::transaction::{Transaction, UnminedTxId};
use zerocopy::*;
use zerocopy_derive::*;

use zcash_primitives::bft::*;
pub use tenderlink::TMStep;

pub struct BftBlockAndFatPointerToItWrap(pub BftBlockAndFatPointerToIt);
impl ZcashDeserialize for BftBlockAndFatPointerToItWrap {
    fn zcash_deserialize<R: std::io::Read>(mut reader: R) -> Result<Self, SerializationError> { // SerializationError> {
        Ok(Self(BftBlockAndFatPointerToIt::zcash_deserialize(&mut reader)?))
    }
}
impl ZcashSerialize for BftBlockAndFatPointerToItWrap {
    fn zcash_serialize<W: std::io::Write>(&self, mut writer: W) -> Result<(), std::io::Error> {
        self.0.zcash_serialize(&mut writer)?;
        Ok(())
    }
}


#[repr(C)]
#[derive(Immutable, KnownLayout, IntoBytes, FromBytes)]
pub struct TFHdr {
    pub magic: [u8; 8],
    pub instrs_o: u64,
    pub instrs_n: u32,
    pub instr_size: u32, // used as stride
}

#[repr(C)]
#[derive(Clone, Copy, Immutable, IntoBytes, FromBytes)]
pub struct TFSlice {
    pub o: u64,
    pub size: u64,
}

impl TFSlice {
    pub fn as_val(self) -> [u64; 2] {
        [self.o, self.size]
    }

    pub fn as_byte_slice_in(self, bytes: &[u8]) -> &[u8] {
        &bytes[self.o as usize..(self.o + self.size) as usize]
    }
}

impl From<&[u64; 2]> for TFSlice {
    fn from(val: &[u64; 2]) -> TFSlice {
        TFSlice {
            o: val[0],
            size: val[1],
        }
    }
}

type TFInstrKind = u32;

#[repr(C)]
#[derive(Clone, Copy, Immutable, IntoBytes, FromBytes)]
pub struct TFInstr {
    pub kind: TFInstrKind,
    pub flags: u32,
    pub data: TFSlice,
    pub val: [u64; 2],
}

/// A value an EXPECT_* instruction should not check.
pub const TEST_STAKE_IGNORED: u64 = u64::MAX;

/// Bond status codes for EXPECT_BOND, as the state reports them, plus one for "no such bond".
pub const TF_BOND_ACTIVE: u64 = 0;
pub const TF_BOND_UNBONDING: u64 = 1;
pub const TF_BOND_WITHDRAWN: u64 = 2;
pub const TF_BOND_BURNED: u64 = 3;
pub const TF_BOND_ABSENT: u64 = 255;

/// BFT_RECV_VOTE bits, above the finalizer index in `val[0]`.
pub const TF_BFT_PRECOMMIT: u64 = 1 << 32;
pub const TF_BFT_NIL: u64 = 1 << 33;
pub const TF_BFT_FOR_NODE_PROPOSAL: u64 = 1 << 34;
pub const TF_BFT_WRONG_NAMESPACE: u64 = 1 << 35;
/// EXPECT_BFT_PROPOSER's name for the node itself.
pub const TF_BFT_THE_NODE: u64 = u64::MAX;
/// EXPECT_BFT_STEP's step for any step.
pub const TF_BFT_ANY_STEP: u64 = 0xff;
/// The node's own finalizer identity seed, unless a test's node config names one.
pub const TEST_NODE_FINALIZER_SEED: &str = "crosslink test node finalizer";

/// The seed synthetic finalizer `k` derives its key from, with `finalizer_key_from_seed`.
pub fn synthetic_finalizer_seed(k: u64) -> String {
    format!("crosslink test finalizer {k}")
}

/// The id tenderlink gives a proposal of `value`.
pub fn bft_value_id(value: &[u8]) -> [u8; 32] {
    tenderlink::value_id_of(&HashKeys::default(), value).0
}

/// The finalizer that proposes at `height` and `round` when the roster is `members` (finalizer
/// key, stake), by the node's own roster ordering and proposer choice. For writing a scenario
/// that has to know who proposes before the node runs.
pub fn bft_proposer(members: &[([u8; 32], u64)], height: u64, round: u32) -> Option<[u8; 32]> {
    let members: Vec<zcash_primitives::transaction::RosterMember> = members
        .iter()
        .map(|&(pub_key, voting_power)| zcash_primitives::transaction::RosterMember { pub_key, voting_power, ..Default::default() })
        .collect();
    let roster = zebra_state::new_network::bft::tenderlink_roster_from_internal(&members, &Default::default());
    let proposer_i = tenderlink::proposer_index(&HashKeys::default(), &roster, height, round)?;
    Some(roster[proposer_i].pub_key.0)
}

/// The public key of synthetic finalizer `k`.
pub fn synthetic_finalizer_pub_key(k: u64) -> [u8; 32] {
    finalizer_key_from_seed(synthetic_finalizer_seed(k).as_bytes()).2 .0
}

/// `height << 32 | round`, as the BFT instructions take them.
pub const fn bft_height_round(height: u64, round: u32) -> u64 {
    height << 32 | round as u64
}

static TF_INSTR_KIND_STRS: [&str; TFInstr::COUNT as usize] = {
    let mut strs = [""; TFInstr::COUNT as usize];
    strs[TFInstr::LOAD_POW as usize] = "LOAD_POW";
    strs[TFInstr::LOAD_POS as usize] = "LOAD_POS";
    strs[TFInstr::SET_PARAMS as usize] = "SET_PARAMS";
    strs[TFInstr::EXPECT_POW_CHAIN_LENGTH as usize] = "EXPECT_POW_CHAIN_LENGTH";
    strs[TFInstr::EXPECT_POS_CHAIN_LENGTH as usize] = "EXPECT_POS_CHAIN_LENGTH";
    strs[TFInstr::EXPECT_POW_BLOCK_FINALITY as usize] = "EXPECT_POW_BLOCK_FINALITY";
    strs[TFInstr::ROSTER_FORCE_INCLUDE as usize] = "ROSTER_FORCE_INCLUDE";
    strs[TFInstr::EXPECT_ROSTER_INCLUDES as usize] = "EXPECT_ROSTER_INCLUDES";
    strs[TFInstr::EXPECT_REJECTION_REASON as usize] = "EXPECT_REJECTION_REASON";
    strs[TFInstr::EXPECT_NODE_ALIVE as usize] = "EXPECT_NODE_ALIVE";
    strs[TFInstr::RECV_TX as usize] = "RECV_TX";
    strs[TFInstr::SUBMIT_TX as usize] = "SUBMIT_TX";
    strs[TFInstr::EXPECT_MEMPOOL_CONTAINS as usize] = "EXPECT_MEMPOOL_CONTAINS";
    strs[TFInstr::EXPECT_MEMPOOL_ABSENT as usize] = "EXPECT_MEMPOOL_ABSENT";
    strs[TFInstr::EXPECT_MEMPOOL_REJECTED as usize] = "EXPECT_MEMPOOL_REJECTED";
    strs[TFInstr::EXPECT_BOND as usize] = "EXPECT_BOND";
    strs[TFInstr::EXPECT_FINALIZER_BANK as usize] = "EXPECT_FINALIZER_BANK";
    strs[TFInstr::EXPECT_POOL_TOTALS as usize] = "EXPECT_POOL_TOTALS";
    strs[TFInstr::RECV_POW as usize] = "RECV_POW";
    strs[TFInstr::RECV_STP_PACKET as usize] = "RECV_STP_PACKET";
    strs[TFInstr::MINE_FROM_TEMPLATE as usize] = "MINE_FROM_TEMPLATE";
    strs[TFInstr::EXPECT_POW_TIP as usize] = "EXPECT_POW_TIP";
    strs[TFInstr::EXPECT_FINALIZED_TIP as usize] = "EXPECT_FINALIZED_TIP";
    strs[TFInstr::CHECK_POW as usize] = "CHECK_POW";
    strs[TFInstr::CHECK_POS as usize] = "CHECK_POS";
    strs[TFInstr::SET_TIME as usize] = "SET_TIME";
    strs[TFInstr::CALL_RPC as usize] = "CALL_RPC";
    strs[TFInstr::EXPECT_STAKING_REPLAY_MATCHES as usize] = "EXPECT_STAKING_REPLAY_MATCHES";
    strs[TFInstr::BFT_RECV_PACKET as usize] = "BFT_RECV_PACKET";
    strs[TFInstr::BFT_RECV_VOTE as usize] = "BFT_RECV_VOTE";
    strs[TFInstr::BFT_RECV_PROPOSAL as usize] = "BFT_RECV_PROPOSAL";
    strs[TFInstr::ADVANCE_TIME as usize] = "ADVANCE_TIME";
    strs[TFInstr::EXPECT_BFT_STEP as usize] = "EXPECT_BFT_STEP";
    strs[TFInstr::EXPECT_BFT_SENT as usize] = "EXPECT_BFT_SENT";
    strs[TFInstr::EXPECT_BFT_PROPOSER as usize] = "EXPECT_BFT_PROPOSER";

    const_assert!(TFInstr::COUNT == 35);
    strs
};

impl TFInstr {
    // NOTE: we want to deal with unknown values at the *application* layer, not the
    // (de)serialization layer.
    // TODO: there may be a crate that makes an enum feasible here
    pub const LOAD_POW: TFInstrKind = 0;
    pub const LOAD_POS: TFInstrKind = 1;
    pub const SET_PARAMS: TFInstrKind = 2;
    pub const EXPECT_POW_CHAIN_LENGTH: TFInstrKind = 3;
    pub const EXPECT_POS_CHAIN_LENGTH: TFInstrKind = 4;
    pub const EXPECT_POW_BLOCK_FINALITY: TFInstrKind = 5;
    pub const ROSTER_FORCE_INCLUDE: TFInstrKind = 6;
    pub const EXPECT_ROSTER_INCLUDES: TFInstrKind = 7;
    /// The previous instruction was rejected, and its reason contains this instruction's data
    /// (UTF-8). `SHOULD_FAIL` alone passes on any rejection, which is how a test passes for the
    /// wrong reason; this pins which rejection it was.
    pub const EXPECT_REJECTION_REASON: TFInstrKind = 8;
    /// The node still answers: the state service returns its tip within a bound, and
    /// re-submitting the tip block through the ingest path answers `Known`.
    pub const EXPECT_NODE_ALIVE: TFInstrKind = 9;
    /// A wire message (the data, header included) arriving from synthetic peer `val[0]`: decoded
    /// with the peer codec and routed through the inbound service exactly as a connection routes
    /// an unsolicited message. A `tx` message reaches the mempool as that peer's transaction.
    /// Checks only that it was delivered; verification is asynchronous, so follow it with an
    /// EXPECT_MEMPOOL_* instruction.
    pub const RECV_TX: TFInstrKind = 10;
    /// A transaction (the data) submitted locally, the doorway the wallet and RPC use. Checks the
    /// mempool's verdict, and its message is the rejection reason when rejected.
    pub const SUBMIT_TX: TFInstrKind = 11;
    /// The transaction (the data) becomes resident in the mempool within a bound. Fails early if
    /// it is rejected instead.
    pub const EXPECT_MEMPOOL_CONTAINS: TFInstrKind = 12;
    /// The transaction (the data) is not resident in the mempool, or stops being so within a
    /// bound: after it is mined, or evicted as a conflict. It holds at once for a transaction the
    /// mempool never had, so follow an EXPECT_MEMPOOL_CONTAINS with it to check an eviction.
    pub const EXPECT_MEMPOOL_ABSENT: TFInstrKind = 13;
    /// The transaction (the data) is in the mempool's rejected set within a bound.
    pub const EXPECT_MEMPOOL_REJECTED: TFInstrKind = 14;
    /// The bond whose key is the data has status `val[1]` (a `TF_BOND_*` code, `TF_BOND_ABSENT`
    /// for none) and amount `val[0]` in zatoshis, at the best chain tip. `TEST_STAKE_IGNORED`
    /// skips either.
    pub const EXPECT_BOND: TFInstrKind = 15;
    /// The reward bank of the finalizer whose public key is the data holds `val[0]` zatoshis.
    pub const EXPECT_FINALIZER_BANK: TFInstrKind = 16;
    /// The best chain tip's staking pools: the data is bonded, unbonded and finalizer rewards,
    /// three little-endian u64 zatoshi amounts, each skipped when `TEST_STAKE_IGNORED`.
    pub const EXPECT_POOL_TOTALS: TFInstrKind = 17;
    /// A PoW block (the data, serialized) served by synthetic STP peer `val[0]` the way a real
    /// peer serves one: the peer advertises it in a STATUS, waits for the node's BLOCK_REQ, and
    /// answers with BLOCK_CHUNK packets from the requested offset. Accepted and rejected as
    /// LOAD_POW is, and rejected too when the node kills the peer or never asks for the block.
    pub const RECV_POW: TFInstrKind = 18;
    /// An STP application packet (the data, type byte first) arriving from synthetic peer
    /// `val[0]`. Fails when the node kills the peer for it; any other handling passes.
    pub const RECV_STP_PACKET: TFInstrKind = 19;
    /// The node builds a block from its own getblocktemplate response, as its miner does, and
    /// the block is submitted through the LOAD_POW doorway. Accepted and rejected as LOAD_POW is;
    /// also rejected when no template or block could be made. A non-zero `val[0]` names a slot
    /// that remembers the block's hash, for instructions that take a slot in place of a hash:
    /// the hash of a block the node builds is not known when the file is written.
    pub const MINE_FROM_TEMPLATE: TFInstrKind = 20;
    /// The best chain's tip is the block whose hash is the data, or, with no data, the block
    /// remembered in slot `val[0]`. Says which of two equally long branches the node is on.
    pub const EXPECT_POW_TIP: TFInstrKind = 21;
    /// `fin`, the finalized tip, is the block named as for EXPECT_POW_TIP. With no data and
    /// slot 0, nothing is finalized yet.
    pub const EXPECT_FINALIZED_TIP: TFInstrKind = 22;
    /// The node's header and body checks, run on the PoW block in the data at once, with the
    /// current time taken to be `val[0]` (UNIX seconds). Nothing is queued or stored, so the
    /// answer needs no wait and no chain context: it covers the rules a block can break by
    /// itself (its timestamp against the clock, its merkle root, its coinbase), not the ones
    /// that need its ancestors. The message says which stage refused.
    pub const CHECK_POW: TFInstrKind = 23;
    /// Whether the PoS block in the data (a LOAD_POS payload) is valid on the node's current BFT
    /// chain: the check a proposal gets, answered at once, with nothing stored or decided. An
    /// indeterminate answer (a PoW block it needs is missing) is pending, as for SHOULD_DEFER.
    pub const CHECK_POS: TFInstrKind = 24;
    /// The node's clock reads `val[0]` (UNIX seconds) from now on, and does not advance; 0
    /// returns it to the real clock. Covers the clock reads that go through zebra_debug_time
    /// (header timestamp rules, template times), not the node's real-time timers.
    pub const SET_TIME: TFInstrKind = 25;
    /// Calls the node's RPC method named in the data, with the JSON params after it (the data
    /// is `method`, a newline, then the params array or object), through the same method table
    /// the RPC server serves. Accepted when the call returns a result, rejected when it returns
    /// an error; the message carries the JSON either way, so EXPECT_REJECTION_REASON can pin an
    /// error, such as a method that no longer exists.
    pub const CALL_RPC: TFInstrKind = 26;
    /// Every bond and finalizer bank the node holds at its best tip is what replaying the best
    /// chain's staking actions and rewards from genesis gives, with the node's own slashfork
    /// schedule and slash window. Catches state the node accumulated that the chain does not
    /// imply, such as a reward paid twice.
    pub const EXPECT_STAKING_REPLAY_MATCHES: TFInstrKind = 27;

    // BFT rounds. The node's own BFT engine runs once BFT starts (a FromChain bootstrap in
    // SET_PARAMS, with the node's finalizer bonded). Synthetic finalizer `k` holds the key from
    // `synthetic_finalizer_seed(k)` and is connected to the engine on first use, after the
    // transport. Heights and rounds are packed into one value as `height << 32 | round`.
    // Signatures use the vote namespace of the engine's height at its last tick.

    /// A tenderlink packet (the data, header first) arriving from synthetic finalizer `val[0]`.
    /// Fails when the node disconnects the peer for it; any other handling passes.
    pub const BFT_RECV_PACKET: TFInstrKind = 28;
    /// A prevote or precommit by synthetic finalizer `val[0] & 0xffff` at `val[1]`, for the
    /// value id in the data. The `TF_BFT_*` bits of `val[0]` make it a precommit, a nil vote, a
    /// vote for the proposal the node sent at that height and round, or one signed under the
    /// wrong namespace. Fails when the finalizer is not on the engine's roster or the node
    /// disconnects it.
    pub const BFT_RECV_VOTE: TFInstrKind = 29;
    /// The proposal in the data, chunked and signed by synthetic finalizer `val[0] & 0xffff` at
    /// `val[1]`, with valid round `((val[0] >> 16) & 0xffff) - 1`. A non-zero `val[0] >> 32` is a
    /// mask of the chunks to send; zero sends all. Whether it counts is the node's call: it only
    /// takes a proposal from that round's proposer.
    pub const BFT_RECV_PROPOSAL: TFInstrKind = 30;
    /// Moves the clock BFT steps and ticks run on forward by `val[0]` milliseconds. In a test
    /// that clock never follows real time, so nothing else moves it. Every synthetic finalizer
    /// first sends the node a status that needs everything at the engine's height and round, and
    /// the instruction returns once the engine has acted on the new time and everything it sent
    /// is recorded.
    pub const ADVANCE_TIME: TFInstrKind = 31;
    /// The engine is at height `val[0]`, round `val[1] >> 8`, step `val[1] & 0xff` (0 propose,
    /// 1 prevote, 2 precommit, 0xff any).
    pub const EXPECT_BFT_STEP: TFInstrKind = 32;
    /// The node has sent, since the test began, its own vote or proposal of kind `val[0]` (0
    /// prevote, 1 precommit, 2 proposal) at `val[1]`, for the value id in the data; with no data,
    /// a nil vote, or a proposal of any value. With SHOULD_FAIL, that it has sent no such thing.
    pub const EXPECT_BFT_SENT: TFInstrKind = 33;
    /// The proposer at `val[1]` is synthetic finalizer `val[0]`, or the node when `val[0]` is
    /// `TF_BFT_THE_NODE`.
    pub const EXPECT_BFT_PROPOSER: TFInstrKind = 34;
    pub const COUNT: TFInstrKind = 35;

    pub fn str_from_kind(kind: TFInstrKind) -> &'static str {
        let kind = kind as usize;
        if kind < TF_INSTR_KIND_STRS.len() {
            TF_INSTR_KIND_STRS[kind]
        } else {
            "<unknown>"
        }
    }

    pub fn string_from_instr(bytes: &[u8], instr: &TFInstr) -> String {
        let mut str = Self::str_from_kind(instr.kind).to_string();
        str += " (";

        match tf_read_instr(&bytes, instr) {
            // Malformed blocks are what rejection tests load, so describing one must not panic:
            // this runs in the panic hook and in the GUI's file viewer.
            Some(TestInstr::LoadPoW(block)) => {
                let height = match block.coinbase_height() {
                    Some(height) => height.0.to_string(),
                    None => "no height".to_string(),
                };
                str += &format!("{} - {}, parent: {}", height, block.hash(), block.header.previous_block_hash)
            }
            Some(TestInstr::LoadPoS((block, fat_ptr))) => match (block.headers.first(), block.headers.last()) {
                (Some(first), Some(last)) => {
                    str += &format!(
                        "{}, snapshot: {}, hdrs: [{} .. {}]",
                        block.blake3_hash(),
                        block.snapshot_block_hash(),
                        BlockHash::from_header_data(first),
                        BlockHash::from_header_data(last)
                    )
                }
                _ => str += &format!("{}, no headers", block.blake3_hash()),
            },
            Some(TestInstr::SetParams(params)) => {
                str += &format!(
                    "{} {:?}",
                    params.bc_confirmation_depth_sigma, params.bootstrap
                )
            }
            Some(TestInstr::ExpectPoWChainLength(h)) => str += &h.to_string(),
            Some(TestInstr::ExpectPoSChainLength(h)) => str += &h.to_string(),
            Some(TestInstr::ExpectPoWBlockFinality(hash, f)) => {
                str += &format!("{} => {:?}", hash, f)
            }
            Some(TestInstr::ExpectRosterIncludes(pub_key, stake)) => {
                str += &format!("{} => {}", PubKeyID(pub_key), stake)
            }
            Some(TestInstr::RosterForceInclude(pub_key, stake)) => {
                str += &format!("{} => {}", PubKeyID(pub_key), stake)
            }
            Some(TestInstr::ExpectRejectionReason(reason)) => str += &format!("{reason:?}"),
            Some(TestInstr::ExpectNodeAlive) => {}
            Some(TestInstr::RecvTx { wire, peer }) => {
                str += &format!("{} bytes from peer {peer}", wire.len())
            }
            Some(TestInstr::SubmitTx(tx))
            | Some(TestInstr::ExpectMempoolContains(tx))
            | Some(TestInstr::ExpectMempoolAbsent(tx))
            | Some(TestInstr::ExpectMempoolRejected(tx)) => str += &tx.hash().to_string(),
            Some(TestInstr::ExpectBond(key, status, amount)) => {
                str += &format!("{} status {status} amount {amount}", hex32(&key))
            }
            Some(TestInstr::ExpectFinalizerBank(pub_key, amount)) => {
                str += &format!("{} => {amount}", PubKeyID(pub_key))
            }
            Some(TestInstr::ExpectPoolTotals(pools)) => str += &format!("{pools:?}"),
            Some(TestInstr::RecvPoW { block, peer }) => {
                str += &match Block::zcash_deserialize(&block[..]) {
                    Ok(parsed) => format!("{} from peer {peer}", parsed.hash()),
                    Err(_) => format!("{} unparseable bytes from peer {peer}", block.len()),
                }
            }
            Some(TestInstr::MineFromTemplate { slot }) => {
                if slot != 0 {
                    str += &format!("into slot {slot}")
                }
            }
            Some(TestInstr::CheckPoW { block, now }) => str += &format!("{} at time {now}", block.hash()),
            Some(TestInstr::CheckPoS(block)) => str += &block.blake3_hash().to_string(),
            Some(TestInstr::SetTime(unix_time)) => str += &unix_time.to_string(),
            Some(TestInstr::CallRpc { method, params }) => str += &format!("{method} {params}"),
            Some(TestInstr::ExpectStakingReplayMatches) => {}
            Some(TestInstr::BftRecvPacket { packet, finalizer }) => {
                str += &format!("{} bytes from finalizer {finalizer}", packet.len())
            }
            Some(TestInstr::BftRecvVote { finalizer, bits, height, round, value_id }) => {
                str += &format!(
                    "{} by finalizer {finalizer} at {height}.{round} for {}{}",
                    if bits & TF_BFT_PRECOMMIT != 0 { "precommit" } else { "prevote" },
                    if bits & TF_BFT_NIL != 0 {
                        "nil".to_string()
                    } else if bits & TF_BFT_FOR_NODE_PROPOSAL != 0 {
                        "the node's proposal".to_string()
                    } else {
                        value_id.map_or("nil".to_string(), |v| hex32(&v))
                    },
                    if bits & TF_BFT_WRONG_NAMESPACE != 0 { ", wrong namespace" } else { "" },
                )
            }
            Some(TestInstr::BftRecvProposal { finalizer, height, round, valid_round, chunk_mask, value }) => {
                str += &format!(
                    "{} by finalizer {finalizer} at {height}.{round}, valid round {valid_round}, chunks {}",
                    hex32(&bft_value_id(&value)),
                    if chunk_mask == 0 { "all".to_string() } else { format!("{chunk_mask:#x}") },
                )
            }
            Some(TestInstr::AdvanceTime { millis }) => str += &format!("{millis} ms"),
            Some(TestInstr::ExpectBftStep { height, round, step }) => str += &format!("{height}.{round} step {step}"),
            Some(TestInstr::ExpectBftSent { kind, height, round, value_id }) => {
                str += &format!("kind {kind} at {height}.{round} for {}", value_id.map_or("none".to_string(), |v| hex32(&v)))
            }
            Some(TestInstr::ExpectBftProposer { finalizer, height, round }) => {
                str += &format!("finalizer {finalizer} at {height}.{round}")
            }
            Some(TestInstr::ExpectPoWTip { hash, slot }) | Some(TestInstr::ExpectFinalizedTip { hash, slot }) => {
                str += &match hash {
                    Some(hash) => hash.to_string(),
                    None if slot != 0 => format!("slot {slot}"),
                    None => "none".to_string(),
                }
            }
            Some(TestInstr::RecvStpPacket { packet, peer }) => {
                str += &format!("type {:?}, {} bytes from peer {peer}", packet.first(), packet.len())
            }
            None => {}
        }

        str += ")";

        if instr.flags != 0 {
            str += " [";
            if (instr.flags & SHOULD_FAIL) != 0 {
                str += " SHOULD_FAIL";
            }
            if (instr.flags & SHOULD_DEFER) != 0 {
                str += " SHOULD_DEFER";
            }
            if (instr.flags & ANY_OUTCOME) != 0 {
                str += " ANY_OUTCOME";
            }
            str += " ]";
        }

        str
    }

    pub fn data_slice<'a>(&self, bytes: &'a [u8]) -> &'a [u8] {
        self.data.as_byte_slice_in(bytes)
    }
}

// Flags
pub const SHOULD_FAIL: u32 = 1 << 0;
/// The block is held pending (an unknown parent, or a wait on BFT) rather than accepted or
/// rejected. Overrides SHOULD_FAIL.
pub const SHOULD_DEFER: u32 = 1 << 1;
/// Any verdict passes: for an action whose outcome the writer cannot know, such as a fuzzed
/// block. What it does not cover is the node giving no verdict at all, which still fails, as do
/// the expectations that follow. Overrides SHOULD_FAIL and SHOULD_DEFER.
pub const ANY_OUTCOME: u32 = 1 << 2;

pub struct TF {
    pub instrs: Vec<TFInstr>,
    pub data: Vec<u8>,
}

pub const TF_NOT_YET_FINALIZED: u64 = 0;
pub const TF_FINALIZED: u64 = 1;
pub const TF_CANT_BE_FINALIZED: u64 = 2;

pub fn finality_from_val(val: &[u64; 2]) -> Option<TFLBlockFinality> {
    if (val[0] == 0) {
        None
    } else {
        match (val[1]) {
            test_format::TF_NOT_YET_FINALIZED => Some(TFLBlockFinality::NotYetFinalized),
            test_format::TF_FINALIZED => Some(TFLBlockFinality::Finalized),
            test_format::TF_CANT_BE_FINALIZED => Some(TFLBlockFinality::CantBeFinalized),
            _ => panic!("unexpected finality value"),
        }
    }
}

pub fn val_from_finality(val: Option<TFLBlockFinality>) -> [u64; 2] {
    match (val) {
        Some(TFLBlockFinality::NotYetFinalized) => [1u64, test_format::TF_NOT_YET_FINALIZED],
        Some(TFLBlockFinality::Finalized) => [1u64, test_format::TF_FINALIZED],
        Some(TFLBlockFinality::CantBeFinalized) => [1u64, test_format::TF_CANT_BE_FINALIZED],
        None => [0u64; 2],
    }
}

/// The file magic this build writes. Version 1 always writes the staking calendar in SET_PARAMS
/// and may follow it with a node config fragment; version 0 files are still read.
pub const TF_MAGIC: &[u8; 8] = b"ZECCLTF1";
const TF_MAGIC_V0: &[u8; 8] = b"ZECCLTF0";

/// 1 for a current file, 0 for one from before the format was versioned.
fn file_version(bytes: &[u8]) -> u8 {
    if bytes.starts_with(TF_MAGIC_V0) {
        0
    } else {
        1
    }
}

impl TF {
    pub fn new(params: &ZcashCrosslinkParameters) -> TF {
        Self::new_with_node_config(params, "")
    }

    /// Also sets parts of the node's config: `node_config` is a zebrad.toml fragment, of which
    /// the harness takes the `[network]` parameters (on regtest, under the Crosslink parameters
    /// given here), the `[crosslink]` slashfork schedule, and `[state] network_checkpoint`.
    pub fn new_with_node_config(params: &ZcashCrosslinkParameters, node_config: &str) -> TF {
        let mut tf = TF {
            instrs: Vec::new(),
            data: Vec::new(),
        };

        // Enforce that every parameter is written: adding a member fails to compile here.
        let ZcashCrosslinkParameters {
            bc_confirmation_depth_sigma,
            bootstrap,
            staking,
        } = *params;
        tf.push_instr_ex(
            TFInstr::SET_PARAMS,
            0,
            &params_to_bytes(bootstrap, staking, node_config),
            [bc_confirmation_depth_sigma, 0],
        );

        tf
    }

    pub fn push_serialize<Z: ZcashSerialize>(&mut self, z: &Z) -> TFSlice {
        let bgn = (size_of::<TFHdr>() + self.data.len()) as u64;
        z.zcash_serialize(&mut self.data);
        let end = (size_of::<TFHdr>() + self.data.len()) as u64;

        TFSlice {
            o: bgn,
            size: end - bgn,
        }
    }

    pub fn push_data(&mut self, bytes: &[u8]) -> TFSlice {
        let result = TFSlice {
            o: (size_of::<TFHdr>() + self.data.len()) as u64,
            size: bytes.len() as u64,
        };
        self.data.write_all(bytes);
        result
    }

    pub fn push_instr_ex(&mut self, kind: TFInstrKind, flags: u32, data: &[u8], val: [u64; 2]) {
        let data = self.push_data(data);
        self.instrs.push(TFInstr {
            kind,
            flags,
            data,
            val,
        });
    }

    pub fn push_instr(&mut self, kind: TFInstrKind, data: &[u8]) {
        self.push_instr_ex(kind, 0, data, [0; 2])
    }

    pub fn push_instr_val(&mut self, kind: TFInstrKind, val: [u64; 2]) {
        self.push_instr_ex(kind, 0, &[0; 0], val)
    }

    pub fn push_instr_serialize_ex<Z: ZcashSerialize>(
        &mut self,
        kind: TFInstrKind,
        flags: u32,
        data: &Z,
        val: [u64; 2],
    ) {
        let data = self.push_serialize(data);
        self.instrs.push(TFInstr {
            kind,
            flags,
            data,
            val,
        });
    }

    pub fn push_instr_serialize<Z: ZcashSerialize>(&mut self, kind: TFInstrKind, data: &Z) {
        self.push_instr_serialize_ex(kind, 0, data, [0; 2])
    }

    pub fn push_instr_load_pow(&mut self, data: &Block, flags: u32) {
        self.push_instr_serialize_ex(TFInstr::LOAD_POW, flags, data, [0; 2])
    }
    pub fn push_instr_load_pow_bytes(&mut self, data: &[u8], flags: u32) {
        self.push_instr_ex(TFInstr::LOAD_POW, flags, data, [0; 2])
    }

    pub fn push_instr_load_pos(&mut self, data: &BftBlockAndFatPointerToItWrap, flags: u32) {
        self.push_instr_serialize_ex(TFInstr::LOAD_POS, flags, data, [0; 2])
    }
    pub fn push_instr_load_pos_bytes(&mut self, data: &[u8], flags: u32) {
        self.push_instr_ex(TFInstr::LOAD_POS, flags, data, [0; 2])
    }

    pub fn push_instr_expect_pow_chain_length(&mut self, length: usize, flags: u32) {
        self.push_instr_ex(
            TFInstr::EXPECT_POW_CHAIN_LENGTH,
            flags,
            &[0; 0],
            [length as u64, 0],
        )
    }

    pub fn push_instr_expect_pos_chain_length(&mut self, length: usize, flags: u32) {
        self.push_instr_ex(
            TFInstr::EXPECT_POS_CHAIN_LENGTH,
            flags,
            &[0; 0],
            [length as u64, 0],
        )
    }

    pub fn push_instr_expect_pow_block_finality(
        &mut self,
        pow_hash: &ZebBlockHash,
        finality: Option<TFLBlockFinality>,
        flags: u32,
    ) {
        self.push_instr_ex(
            TFInstr::EXPECT_POW_BLOCK_FINALITY,
            flags,
            &pow_hash.0,
            test_format::val_from_finality(finality),
        )
    }

    pub fn push_instr_roster_force_include(&mut self, pub_key: [u8; 32], stake: u64, flags: u32) {
        self.push_instr_ex(TFInstr::ROSTER_FORCE_INCLUDE, flags, &pub_key, [stake, 0])
    }

    pub fn push_instr_expect_roster_includes(&mut self, pub_key: [u8; 32], stake: u64, flags: u32) {
        self.push_instr_ex(TFInstr::EXPECT_ROSTER_INCLUDES, flags, &pub_key, [stake, 0])
    }

    pub fn push_instr_expect_rejection_reason(&mut self, reason: &str, flags: u32) {
        self.push_instr_ex(TFInstr::EXPECT_REJECTION_REASON, flags, reason.as_bytes(), [0; 2])
    }

    pub fn push_instr_expect_node_alive(&mut self, flags: u32) {
        self.push_instr_ex(TFInstr::EXPECT_NODE_ALIVE, flags, &[0; 0], [0; 2])
    }

    /// A wire message delivered as if synthetic peer `peer` sent it. For a transaction, frame it
    /// with `zebra_network::wire::tx_message_bytes`.
    pub fn push_instr_recv_tx_wire(&mut self, wire: &[u8], peer: u64, flags: u32) {
        self.push_instr_ex(TFInstr::RECV_TX, flags, wire, [peer, 0])
    }

    pub fn push_instr_submit_tx(&mut self, tx: &Transaction, flags: u32) {
        self.push_instr_serialize_ex(TFInstr::SUBMIT_TX, flags, tx, [0; 2])
    }

    pub fn push_instr_expect_mempool_contains(&mut self, tx: &Transaction, flags: u32) {
        self.push_instr_serialize_ex(TFInstr::EXPECT_MEMPOOL_CONTAINS, flags, tx, [0; 2])
    }

    pub fn push_instr_expect_mempool_absent(&mut self, tx: &Transaction, flags: u32) {
        self.push_instr_serialize_ex(TFInstr::EXPECT_MEMPOOL_ABSENT, flags, tx, [0; 2])
    }

    pub fn push_instr_expect_mempool_rejected(&mut self, tx: &Transaction, flags: u32) {
        self.push_instr_serialize_ex(TFInstr::EXPECT_MEMPOOL_REJECTED, flags, tx, [0; 2])
    }

    /// `status` is a `TF_BOND_*` code; either argument may be `TEST_STAKE_IGNORED`.
    pub fn push_instr_expect_bond(&mut self, bond_key: [u8; 32], status: u64, amount: u64, flags: u32) {
        self.push_instr_ex(TFInstr::EXPECT_BOND, flags, &bond_key, [amount, status])
    }

    pub fn push_instr_expect_finalizer_bank(&mut self, pub_key: [u8; 32], amount: u64, flags: u32) {
        self.push_instr_ex(TFInstr::EXPECT_FINALIZER_BANK, flags, &pub_key, [amount, 0])
    }

    /// Bonded, unbonded and finalizer-reward pool totals; any may be `TEST_STAKE_IGNORED`.
    pub fn push_instr_expect_pool_totals(&mut self, bonded: u64, unbonded: u64, finalizer_rewards: u64, flags: u32) {
        let mut data = Vec::with_capacity(24);
        for amount in [bonded, unbonded, finalizer_rewards] {
            data.extend_from_slice(&amount.to_le_bytes());
        }
        self.push_instr_ex(TFInstr::EXPECT_POOL_TOTALS, flags, &data, [0; 2])
    }

    /// A block served over the STP block-sync exchange by synthetic peer `peer`.
    pub fn push_instr_recv_pow(&mut self, block: &Block, peer: u64, flags: u32) {
        self.push_instr_serialize_ex(TFInstr::RECV_POW, flags, block, [peer, 0])
    }

    /// Block bytes served as-is, for encodings `Block` would not produce.
    pub fn push_instr_recv_pow_bytes(&mut self, block: &[u8], peer: u64, flags: u32) {
        self.push_instr_ex(TFInstr::RECV_POW, flags, block, [peer, 0])
    }

    pub fn push_instr_recv_stp_packet(&mut self, packet: &[u8], peer: u64, flags: u32) {
        self.push_instr_ex(TFInstr::RECV_STP_PACKET, flags, packet, [peer, 0])
    }

    pub fn push_instr_mine_from_template(&mut self, flags: u32) {
        self.push_instr_ex(TFInstr::MINE_FROM_TEMPLATE, flags, &[0; 0], [0; 2])
    }

    /// As push_instr_mine_from_template, remembering the mined block's hash in `slot` (non-zero).
    pub fn push_instr_mine_from_template_into(&mut self, slot: u64, flags: u32) {
        self.push_instr_ex(TFInstr::MINE_FROM_TEMPLATE, flags, &[0; 0], [slot, 0])
    }

    pub fn push_instr_expect_pow_tip(&mut self, hash: &ZebBlockHash, flags: u32) {
        self.push_instr_ex(TFInstr::EXPECT_POW_TIP, flags, &hash.0, [0; 2])
    }

    pub fn push_instr_expect_pow_tip_slot(&mut self, slot: u64, flags: u32) {
        self.push_instr_ex(TFInstr::EXPECT_POW_TIP, flags, &[0; 0], [slot, 0])
    }

    /// `None` expects that nothing is finalized yet.
    pub fn push_instr_expect_finalized_tip(&mut self, hash: Option<&ZebBlockHash>, flags: u32) {
        match hash {
            Some(hash) => self.push_instr_ex(TFInstr::EXPECT_FINALIZED_TIP, flags, &hash.0, [0; 2]),
            None => self.push_instr_ex(TFInstr::EXPECT_FINALIZED_TIP, flags, &[0; 0], [0; 2]),
        }
    }

    pub fn push_instr_expect_finalized_tip_slot(&mut self, slot: u64, flags: u32) {
        self.push_instr_ex(TFInstr::EXPECT_FINALIZED_TIP, flags, &[0; 0], [slot, 0])
    }

    /// The header and body checks on `block`, taking the current time to be `now_unix_time`.
    pub fn push_instr_check_pow(&mut self, block: &Block, now_unix_time: u64, flags: u32) {
        self.push_instr_serialize_ex(TFInstr::CHECK_POW, flags, block, [now_unix_time, 0])
    }

    /// Whether the node would accept this PoS block as a proposal now, without storing it.
    pub fn push_instr_check_pos(&mut self, data: &BftBlockAndFatPointerToItWrap, flags: u32) {
        self.push_instr_serialize_ex(TFInstr::CHECK_POS, flags, data, [0; 2])
    }

    /// The node's clock reads `unix_time` until set again; 0 returns it to the real clock.
    /// `params` is the JSON params array or object, e.g. `[]` or `["abc", 1]`.
    pub fn push_instr_call_rpc(&mut self, method: &str, params: &str, flags: u32) {
        let data = format!("{method}\n{params}");
        self.push_instr_ex(TFInstr::CALL_RPC, flags, data.as_bytes(), [0; 2])
    }

    pub fn push_instr_expect_staking_replay_matches(&mut self, flags: u32) {
        self.push_instr_ex(TFInstr::EXPECT_STAKING_REPLAY_MATCHES, flags, &[0; 0], [0; 2])
    }

    pub fn push_instr_set_time(&mut self, unix_time: u64, flags: u32) {
        self.push_instr_ex(TFInstr::SET_TIME, flags, &[0; 0], [unix_time, 0])
    }

    pub fn push_instr_bft_recv_packet(&mut self, finalizer: u64, packet: &[u8], flags: u32) {
        self.push_instr_ex(TFInstr::BFT_RECV_PACKET, flags, packet, [finalizer, 0])
    }

    /// `value_id` is ignored with TF_BFT_NIL or TF_BFT_FOR_NODE_PROPOSAL in `bits`.
    pub fn push_instr_bft_recv_vote(&mut self, finalizer: u16, bits: u64, height: u64, round: u32, value_id: &[u8; 32], flags: u32) {
        let data: &[u8] = if bits & (TF_BFT_NIL | TF_BFT_FOR_NODE_PROPOSAL) != 0 { &[] } else { value_id };
        self.push_instr_ex(TFInstr::BFT_RECV_VOTE, flags, data, [u64::from(finalizer) | bits, bft_height_round(height, round)])
    }

    /// `chunk_mask` 0 sends every chunk; `valid_round` is -1 for none.
    pub fn push_instr_bft_recv_proposal(&mut self, finalizer: u16, height: u64, round: u32, valid_round: i64, chunk_mask: u32, value: &[u8], flags: u32) {
        let valid_round = u64::try_from(valid_round + 1).unwrap_or(0) & 0xffff;
        let val0 = u64::from(finalizer) | valid_round << 16 | u64::from(chunk_mask) << 32;
        self.push_instr_ex(TFInstr::BFT_RECV_PROPOSAL, flags, value, [val0, bft_height_round(height, round)])
    }

    pub fn push_instr_advance_time(&mut self, millis: u64, flags: u32) {
        self.push_instr_ex(TFInstr::ADVANCE_TIME, flags, &[0; 0], [millis, 0])
    }

    /// `step` is `None` for any step.
    pub fn push_instr_expect_bft_step(&mut self, height: u64, round: u32, step: Option<tenderlink::TMStep>, flags: u32) {
        let step = match step {
            Some(tenderlink::TMStep::Propose) => 0,
            Some(tenderlink::TMStep::Prevote) => 1,
            Some(tenderlink::TMStep::Precommit) => 2,
            None => TF_BFT_ANY_STEP,
        };
        self.push_instr_ex(TFInstr::EXPECT_BFT_STEP, flags, &[0; 0], [height, u64::from(round) << 8 | step])
    }

    /// `kind` 0 prevote, 1 precommit, 2 proposal. `None` is a nil vote, or a proposal of any value.
    pub fn push_instr_expect_bft_sent(&mut self, kind: u64, height: u64, round: u32, value_id: Option<&[u8; 32]>, flags: u32) {
        let data: &[u8] = match value_id { Some(value_id) => value_id, None => &[] };
        self.push_instr_ex(TFInstr::EXPECT_BFT_SENT, flags, data, [kind, bft_height_round(height, round)])
    }

    /// `finalizer` is a synthetic finalizer's index, or TF_BFT_THE_NODE.
    pub fn push_instr_expect_bft_proposer(&mut self, finalizer: u64, height: u64, round: u32, flags: u32) {
        self.push_instr_ex(TFInstr::EXPECT_BFT_PROPOSER, flags, &[0; 0], [finalizer, bft_height_round(height, round)])
    }

    /// A block the node must reject for `reason`, and must survive rejecting: the load, the
    /// reason check, and the liveness check, as three instructions.
    pub fn push_instr_load_pow_rejected(&mut self, block: &Block, reason: &str) {
        self.push_instr_load_pow(block, SHOULD_FAIL);
        self.push_instr_expect_rejection_reason(reason, 0);
        self.push_instr_expect_node_alive(0);
    }

    fn is_a_power_of_2(v: usize) -> bool {
        v != 0 && ((v & (v - 1)) == 0)
    }

    fn align_up(v: usize, mut align: usize) -> usize {
        assert!(Self::is_a_power_of_2(align));
        align -= 1;
        (v + align) & !align
    }

    pub fn write<W: std::io::Write>(&self, writer: &mut W) -> bool {
        let instrs_o_unaligned = size_of::<TFHdr>() + self.data.len();
        let instrs_o = Self::align_up(instrs_o_unaligned, align_of::<TFInstr>());
        let hdr = TFHdr {
            magic: *TF_MAGIC,
            instrs_o: instrs_o as u64,
            instrs_n: self.instrs.len() as u32,
            instr_size: size_of::<TFInstr>() as u32,
        };
        writer
            .write_all(hdr.as_bytes())
            .expect("writing shouldn't fail");
        writer
            .write_all(&self.data)
            .expect("writing shouldn't fail");

        if instrs_o > instrs_o_unaligned {
            const ALIGN_0S: [u8; align_of::<TFInstr>()] = [0u8; align_of::<TFInstr>()];
            let align_size = instrs_o - instrs_o_unaligned;
            let align_bytes = &ALIGN_0S[..align_size];
            writer.write_all(align_bytes);
        }
        writer
            .write_all(self.instrs.as_bytes())
            .expect("writing shouldn't fail");

        true
    }

    pub fn write_to_file(&self, path: &std::path::Path) -> bool {
        if let Ok(mut file) = std::fs::File::create(path) {
            self.write(&mut file)
        } else {
            false
        }
    }

    pub fn write_to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        self.write(&mut bytes);
        bytes
    }

    // Simple version, all in one go... for large files we'll want to break this up; get hdr &
    // get/stream instrs, then read data as needed
    /// Every offset and length in the file is checked here, so nothing that later slices the
    /// file can index out of bounds: the GUI opens arbitrary files, and a panic there aborts the
    /// running node.
    pub fn read_from_bytes(bytes: &[u8]) -> Result<Self, String> {
        let tf_hdr = match TFHdr::ref_from_prefix(bytes) {
            Ok((hdr, _)) => hdr,
            Err(err) => return Err(err.to_string()),
        };
        if &tf_hdr.magic != TF_MAGIC && &tf_hdr.magic != TF_MAGIC_V0 {
            return Err(format!("not a crosslink test file: magic {:?}", tf_hdr.magic));
        }
        if tf_hdr.instr_size as usize != size_of::<TFInstr>() {
            return Err(format!(
                "instruction size is {} bytes, this build reads {}",
                tf_hdr.instr_size,
                size_of::<TFInstr>()
            ));
        }
        let instrs_o = match usize::try_from(tf_hdr.instrs_o) {
            Ok(o) if o >= size_of::<TFHdr>() && o <= bytes.len() => o,
            _ => return Err(format!("instructions start at {}, outside the file", tf_hdr.instrs_o)),
        };

        let read_instrs = <[TFInstr]>::ref_from_prefix_with_elems(&bytes[instrs_o..], tf_hdr.instrs_n as usize);

        let instrs = match read_instrs {
            Ok((instrs, _)) => instrs,
            Err(err) => return Err(err.to_string()),
        };
        for (instr_i, instr) in instrs.iter().enumerate() {
            let in_file = instr.data.o.checked_add(instr.data.size).is_some_and(|end| end <= bytes.len() as u64);
            if !in_file {
                return Err(format!("instruction {instr_i}'s data runs past the end of the file"));
            }
        }

        let data = &bytes[size_of::<TFHdr>()..instrs_o];

        // TODO: just use slices, don't copy to vectors
        let tf = TF {
            instrs: instrs.to_vec(),
            data: data.to_vec(),
        };

        Ok(tf)
    }

    pub fn read_from_file(path: &std::path::Path) -> Result<(Vec<u8>, Self), String> {
        let bytes = match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(err) => return Err(err.to_string()),
        };

        Self::read_from_bytes(&bytes).map(|tf| (bytes, tf))
    }
}

// TODO: macro for a stringified condition
fn test_check(flags: u32, condition: bool, message: &str) {
    let holds = flags & ANY_OUTCOME != 0 || condition == ((flags & SHOULD_FAIL) == 0);
    test_check_outcome(condition, holds, message);
}

/// Records an instruction's outcome. `accepted` is what EXPECT_REJECTION_REASON reads back;
/// `holds` is whether that outcome is the one the instruction's flags expected.
fn test_check_outcome(accepted: bool, holds: bool, message: &str) {
    *TEST_LAST_CHECK.lock().unwrap() = Some((accepted, message.to_string()));

    if !holds {
        let test_instr_i = *TEST_INSTR_C.lock().unwrap();
        TEST_FAILED_INSTR_IDXS.lock().unwrap().push((test_instr_i, message.to_string()));

        match *TEST_CHECK_ASSERT.lock().unwrap() {
            0 => {},
            1 => error!("test check did not hold, message:\n{}", message),
            _ => panic!("test check did not hold (and TEST_CHECK_ASSERT enabled), message:\n{}", message),
        }
    }
}

/// A delivered block meets its instruction's expectation: accepted by default, rejected under
/// SHOULD_FAIL, held pending under SHOULD_DEFER. An `Err` (no verdict came back) meets none of
/// them, so a node that stopped answering cannot pass for one that rejected.
fn check_ingest(flags: u32, label: &str, outcome: Result<zebra_state::new_network::IngestOutcome, String>) {
    use zebra_state::new_network::IngestOutcome;
    let any = flags & ANY_OUTCOME != 0;
    let expect_pending = any || flags & SHOULD_DEFER != 0;
    let expect_rejected = any || (!expect_pending && flags & SHOULD_FAIL != 0);
    let expect_accepted = any || (!expect_pending && !expect_rejected);
    let (accepted, holds, message) = match outcome {
        Ok(IngestOutcome::Committed(_)) => (true, expect_accepted, format!("{label}accepted: PoW ingest ok")),
        Ok(IngestOutcome::Known { .. }) => (true, expect_accepted, format!("{label}accepted: PoW already known")),
        Ok(IngestOutcome::Failed { reason, .. }) => (false, expect_rejected, format!("{label}rejected: {reason}")),
        Ok(IngestOutcome::Pending { reason }) => (false, expect_pending, format!("{label}pending: {reason}")),
        Err(reason) => (false, false, format!("{label}no verdict: {reason}")),
    };
    test_check_outcome(accepted, holds, &message);
}

use crate::*;

/// Parameters for scenarios that feed BFT blocks in directly, which is every scenario that is not
/// testing the bootstrap itself. A file with no `SET_PARAMS` is read as these: every scenario was
/// written that way before the bootstrap became a parameter.
pub const HARNESS_PARAMETERS: ZcashCrosslinkParameters = ZcashCrosslinkParameters {
    bootstrap: BftBootstrap::Supplied,
    // Sigma is pinned here rather than inherited from `PROTOTYPE_PARAMETERS`. The
    // scenes in the test suite are hand-built at specific heights: a BFT block carries exactly
    // sigma headers, and the PoW block that cites it has to sit at least sigma + 1 above the
    // block that certificate finalizes. Inheriting sigma would silently invalidate every one of
    // those scenes the moment the network parameter moved, which is not what changing a network
    // parameter should mean. The rules under test do not depend on sigma's value; the live
    // network's value is exercised on a testnet, not here.
    bc_confirmation_depth_sigma: 3,
    // Pinned for the same reason as sigma: the staking scenes are built at heights chosen for
    // this calendar. A scenario that wants a short one uses `short_staking`.
    staking: PROTOTYPE_STAKING,
};

/// The shortest staking calendar a scenario on `params` can use and still mean what it would on
/// a real network, for
/// `ZcashCrosslinkParameters { staking: short_staking(&params), ..params }`.
///
/// `StakingParameters::is_valid` is only what consensus needs to run. This adds what a scenario
/// needs to stay realistic:
/// - a day window of 3, so a test can act first inside, last inside and first outside it;
/// - an action delay one past the window, with room left in the period, so a withdrawal can
///   land early or late in a later window;
/// - a period at least `2 * (sigma + FINALITY_LIVENESS_ALLOWANCE + 1)`, so roster and stake
///   changes turn over slower than finality can reflect them, as they do in the prototype calendar.
///
/// A scenario that needs more, such as an edge further into the window, sets its own calendar.
///
/// With `CROSSLINK_TEST_MODE=conformance` this returns the prototype calendar instead, so every
/// scenario written against it also runs at real-network values. That run is the check on
/// anything the short calendar's margins miss, which is why scenarios derive their heights from
/// the calendar rather than hard-coding them.
pub fn short_staking(params: &ZcashCrosslinkParameters) -> StakingParameters {
    if conformance_mode() {
        return PROTOTYPE_STAKING;
    }
    short_calendar(params)
}

/// Whether this run is a conformance run (`CROSSLINK_TEST_MODE=conformance`): real-network
/// calendar values, generated fresh.
pub fn conformance_mode() -> bool {
    std::env::var("CROSSLINK_TEST_MODE").is_ok_and(|mode| mode == "conformance")
}

fn short_calendar(params: &ZcashCrosslinkParameters) -> StakingParameters {
    let day_window = 3;
    let action_delay = day_window + 1;
    let finality_gap = params.bc_confirmation_depth_sigma + FINALITY_LIVENESS_ALLOWANCE;
    let finality_floor = u32::try_from(2 * (finality_gap + 1)).expect("sigma is a small test value");
    let staking = StakingParameters {
        period: finality_floor.max(day_window + action_delay),
        day_window,
        action_delay,
    };
    assert!(staking.is_valid(), "short_staking built an invalid calendar: {staking:?}");
    staking
}

// `SET_PARAMS` carries sigma in `val[0]`, and in its data the bootstrap followed by the staking
// calendar (period, day window, action delay, each a little-endian u32). The calendar is written
// only when it isn't the prototype's, and data that ends after the bootstrap is read as the
// prototype calendar, so files from before the calendar was a parameter, and every file that
// keeps the prototype calendar, stay byte-identical.
// `val[1]` is written as zero and never read: it used to carry the Book's `L`, which Zebra
// Crosslink does not have.
const TF_BOOTSTRAP_SUPPLIED: u8 = 0;
const TF_BOOTSTRAP_FROM_CHAIN: u8 = 1;

/// The calendar is always written: a file that leaves it out means whatever the prototype
/// calendar is when it is read, and that changed under every stored file once (00565513).
fn params_to_bytes(bootstrap: BftBootstrap, staking: StakingParameters, node_config: &str) -> Vec<u8> {
    let mut bytes = match bootstrap {
        BftBootstrap::Supplied => vec![TF_BOOTSTRAP_SUPPLIED],
        BftBootstrap::FromChain { staking_height, roster_height, activation_height } => {
            let mut bytes = vec![TF_BOOTSTRAP_FROM_CHAIN];
            bytes.extend_from_slice(&staking_height.to_le_bytes());
            bytes.extend_from_slice(&roster_height.to_le_bytes());
            bytes.extend_from_slice(&activation_height.to_le_bytes());
            bytes
        }
    };
    for value in [staking.period, staking.day_window, staking.action_delay] {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes.extend_from_slice(node_config.as_bytes());
    bytes
}

/// The SET_PARAMS payload of a file of `version`: bootstrap, calendar, node config fragment.
fn params_from_bytes(bytes: &[u8], version: u8) -> Option<(BftBootstrap, StakingParameters, String)> {
    if version == 0 {
        let (bootstrap, staking) = params_from_bytes_v0(bytes)?;
        return Some((bootstrap, staking, String::new()));
    }
    let (bootstrap, rest) = bootstrap_from_bytes(bytes)?;
    if rest.len() < 12 {
        return None;
    }
    let word = |i: usize| u32::from_le_bytes(rest[4 * i..4 * i + 4].try_into().expect("four bytes"));
    let staking = StakingParameters { period: word(0), day_window: word(1), action_delay: word(2) };
    let node_config = String::from_utf8(rest[12..].to_vec()).ok()?;
    Some((bootstrap, staking, node_config))
}

fn bootstrap_from_bytes(bytes: &[u8]) -> Option<(BftBootstrap, &[u8])> {
    match bytes {
        [TF_BOOTSTRAP_SUPPLIED, rest @ ..] => Some((BftBootstrap::Supplied, rest)),
        [TF_BOOTSTRAP_FROM_CHAIN, s0, s1, s2, s3, r0, r1, r2, r3, a0, a1, a2, a3, rest @ ..] => Some((
            BftBootstrap::FromChain {
                staking_height: u32::from_le_bytes([*s0, *s1, *s2, *s3]),
                roster_height: u32::from_le_bytes([*r0, *r1, *r2, *r3]),
                activation_height: u32::from_le_bytes([*a0, *a1, *a2, *a3]),
            },
            rest,
        )),
        _ => None,
    }
}

/// Version 0 left the calendar out when it equalled the prototype's, so such a file is read
/// under whatever the prototype calendar is now.
fn params_from_bytes_v0(bytes: &[u8]) -> Option<(BftBootstrap, StakingParameters)> {
    let (bootstrap, rest) = bootstrap_from_bytes(bytes)?;
    let word = |i: usize| u32::from_le_bytes(rest[4 * i..4 * i + 4].try_into().expect("four bytes"));
    let staking = match rest.len() {
        0 => PROTOTYPE_STAKING,
        12 => StakingParameters { period: word(0), day_window: word(1), action_delay: word(2) },
        _ => return None,
    };
    Some((bootstrap, staking))
}

/// The zebrad.toml fragment a test file's leading `SET_PARAMS` carries, empty if none. See
/// [`TF::new_with_node_config`] for the parts the harness applies.
pub fn node_config_for_test(bytes: &[u8]) -> String {
    let Ok(tf) = TF::read_from_bytes(bytes) else {
        return String::new();
    };
    match tf.instrs.first() {
        Some(first) if first.kind == TFInstr::SET_PARAMS => {
            match params_from_bytes(first.data_slice(bytes), file_version(bytes)) {
                Some((_, _, node_config)) => node_config,
                None => String::new(),
            }
        }
        _ => String::new(),
    }
}

/// The Crosslink parameters a test file's node must run with: its leading `SET_PARAMS`, or
/// [`HARNESS_PARAMETERS`] if it has none. They are consensus parameters, so the harness builds the
/// network with them before the node boots instead of applying them when the instruction runs.
pub fn crosslink_parameters_for_test(bytes: &[u8]) -> ZcashCrosslinkParameters {
    let Ok(tf) = TF::read_from_bytes(bytes) else {
        return HARNESS_PARAMETERS;
    };
    let Some(first) = tf.instrs.first() else {
        return HARNESS_PARAMETERS;
    };
    if first.kind != TFInstr::SET_PARAMS {
        return HARNESS_PARAMETERS;
    }
    // A malformed SET_PARAMS falls back here, then fails loudly when the instruction is read.
    if let Some(TestInstr::SetParams(params)) = tf_read_instr(bytes, first) {
        params
    } else {
        HARNESS_PARAMETERS
    }
}

pub(crate) fn tf_read_instr(bytes: &[u8], instr: &TFInstr) -> Option<TestInstr> {
    const_assert!(TFInstr::COUNT == 35);
    match instr.kind {
        TFInstr::LOAD_POW => {
            let block = Block::zcash_deserialize(instr.data_slice(bytes)).ok()?;
            Some(TestInstr::LoadPoW(block))
        }

        TFInstr::LOAD_POS => {
            let block_and_fat_ptr =
                BftBlockAndFatPointerToItWrap::zcash_deserialize(instr.data_slice(bytes)).ok()?;
            Some(TestInstr::LoadPoS((
                block_and_fat_ptr.0.block,
                block_and_fat_ptr.0.fat_ptr,
            )))
        }

        TFInstr::SET_PARAMS => {
            let (bootstrap, staking, _node_config) = params_from_bytes(instr.data_slice(bytes), file_version(bytes))?;
            Some(TestInstr::SetParams(ZcashCrosslinkParameters {
                bc_confirmation_depth_sigma: instr.val[0],
                bootstrap,
                staking,
            }))
        }

        TFInstr::EXPECT_POW_CHAIN_LENGTH => {
            Some(TestInstr::ExpectPoWChainLength(instr.val[0] as u32))
        }
        TFInstr::EXPECT_POS_CHAIN_LENGTH => Some(TestInstr::ExpectPoSChainLength(instr.val[0])),

        TFInstr::EXPECT_POW_BLOCK_FINALITY => {
            // finality_from_val panics on an unknown code, and the code comes from the file.
            if instr.val[0] != 0 && instr.val[1] > TF_CANT_BE_FINALIZED {
                return None;
            }
            Some(TestInstr::ExpectPoWBlockFinality(
                ZebBlockHash(instr.data_slice(bytes).try_into().ok()?),
                finality_from_val(&instr.val),
            ))
        }

        TFInstr::ROSTER_FORCE_INCLUDE => Some(TestInstr::RosterForceInclude(
            instr.data_slice(bytes).try_into().ok()?,
            instr.val[0],
        )),
        TFInstr::EXPECT_ROSTER_INCLUDES => Some(TestInstr::ExpectRosterIncludes(
            instr.data_slice(bytes).try_into().ok()?,
            instr.val[0],
        )),

        TFInstr::EXPECT_REJECTION_REASON => Some(TestInstr::ExpectRejectionReason(
            String::from_utf8(instr.data_slice(bytes).to_vec()).ok()?,
        )),
        TFInstr::EXPECT_NODE_ALIVE => Some(TestInstr::ExpectNodeAlive),
        TFInstr::RECV_TX => Some(TestInstr::RecvTx {
            wire: instr.data_slice(bytes).to_vec(),
            peer: instr.val[0],
        }),
        TFInstr::SUBMIT_TX => Some(TestInstr::SubmitTx(
            Transaction::zcash_deserialize(instr.data_slice(bytes)).ok()?,
        )),
        TFInstr::EXPECT_MEMPOOL_CONTAINS => Some(TestInstr::ExpectMempoolContains(
            Transaction::zcash_deserialize(instr.data_slice(bytes)).ok()?,
        )),
        TFInstr::EXPECT_MEMPOOL_ABSENT => Some(TestInstr::ExpectMempoolAbsent(
            Transaction::zcash_deserialize(instr.data_slice(bytes)).ok()?,
        )),
        TFInstr::EXPECT_MEMPOOL_REJECTED => Some(TestInstr::ExpectMempoolRejected(
            Transaction::zcash_deserialize(instr.data_slice(bytes)).ok()?,
        )),
        TFInstr::EXPECT_BOND => Some(TestInstr::ExpectBond(
            instr.data_slice(bytes).try_into().ok()?,
            instr.val[1],
            instr.val[0],
        )),
        TFInstr::EXPECT_FINALIZER_BANK => Some(TestInstr::ExpectFinalizerBank(
            instr.data_slice(bytes).try_into().ok()?,
            instr.val[0],
        )),
        TFInstr::EXPECT_POOL_TOTALS => {
            let data: [u8; 24] = instr.data_slice(bytes).try_into().ok()?;
            let amount = |i: usize| u64::from_le_bytes(data[8 * i..8 * i + 8].try_into().expect("eight bytes"));
            Some(TestInstr::ExpectPoolTotals([amount(0), amount(1), amount(2)]))
        }
        TFInstr::RECV_POW => Some(TestInstr::RecvPoW {
            block: instr.data_slice(bytes).to_vec(),
            peer: instr.val[0],
        }),
        TFInstr::MINE_FROM_TEMPLATE => Some(TestInstr::MineFromTemplate { slot: instr.val[0] }),
        TFInstr::CHECK_POW => Some(TestInstr::CheckPoW {
            block: Block::zcash_deserialize(instr.data_slice(bytes)).ok()?,
            now: instr.val[0],
        }),
        TFInstr::CHECK_POS => {
            let block_and_fat_ptr = BftBlockAndFatPointerToItWrap::zcash_deserialize(instr.data_slice(bytes)).ok()?;
            Some(TestInstr::CheckPoS(block_and_fat_ptr.0.block))
        }
        TFInstr::SET_TIME => Some(TestInstr::SetTime(instr.val[0])),
        TFInstr::CALL_RPC => {
            let text = std::str::from_utf8(instr.data_slice(bytes)).ok()?;
            let (method, params) = text.split_once('\n')?;
            Some(TestInstr::CallRpc { method: method.to_string(), params: params.to_string() })
        }
        TFInstr::EXPECT_STAKING_REPLAY_MATCHES => Some(TestInstr::ExpectStakingReplayMatches),
        TFInstr::BFT_RECV_PACKET => Some(TestInstr::BftRecvPacket { packet: instr.data_slice(bytes).to_vec(), finalizer: instr.val[0] }),
        TFInstr::BFT_RECV_VOTE => {
            let data = instr.data_slice(bytes);
            Some(TestInstr::BftRecvVote {
                finalizer: instr.val[0] & 0xffff,
                bits: instr.val[0] & !0xffff,
                height: instr.val[1] >> 32,
                round: instr.val[1] as u32,
                value_id: if data.is_empty() { None } else { Some(data.try_into().ok()?) },
            })
        }
        TFInstr::BFT_RECV_PROPOSAL => Some(TestInstr::BftRecvProposal {
            finalizer: instr.val[0] & 0xffff,
            height: instr.val[1] >> 32,
            round: instr.val[1] as u32,
            valid_round: ((instr.val[0] >> 16) & 0xffff) as i64 - 1,
            chunk_mask: (instr.val[0] >> 32) as u32,
            value: instr.data_slice(bytes).to_vec(),
        }),
        TFInstr::ADVANCE_TIME => Some(TestInstr::AdvanceTime { millis: instr.val[0] }),
        TFInstr::EXPECT_BFT_STEP => Some(TestInstr::ExpectBftStep {
            height: instr.val[0],
            round: u32::try_from(instr.val[1] >> 8).ok()?,
            step: instr.val[1] & 0xff,
        }),
        TFInstr::EXPECT_BFT_SENT => {
            let data = instr.data_slice(bytes);
            Some(TestInstr::ExpectBftSent {
                kind: instr.val[0],
                height: instr.val[1] >> 32,
                round: instr.val[1] as u32,
                value_id: if data.is_empty() { None } else { Some(data.try_into().ok()?) },
            })
        }
        TFInstr::EXPECT_BFT_PROPOSER => Some(TestInstr::ExpectBftProposer {
            finalizer: instr.val[0],
            height: instr.val[1] >> 32,
            round: instr.val[1] as u32,
        }),
        TFInstr::EXPECT_POW_TIP | TFInstr::EXPECT_FINALIZED_TIP => {
            let data = instr.data_slice(bytes);
            let hash = if data.is_empty() { None } else { Some(ZebBlockHash(data.try_into().ok()?)) };
            let slot = instr.val[0];
            if instr.kind == TFInstr::EXPECT_POW_TIP {
                Some(TestInstr::ExpectPoWTip { hash, slot })
            } else {
                Some(TestInstr::ExpectFinalizedTip { hash, slot })
            }
        }
        TFInstr::RECV_STP_PACKET => Some(TestInstr::RecvStpPacket {
            packet: instr.data_slice(bytes).to_vec(),
            peer: instr.val[0],
        }),

        // An instruction this build doesn't know. The caller decides whether that is fatal
        // (a test) or skippable (the fuzzer, the GUI viewer).
        _ => None,
    }
}

#[derive(Clone)]
pub(crate) enum TestInstr {
    LoadPoW(Block),
    LoadPoS((BftBlock, FatPointerToBftBlock)),
    SetParams(ZcashCrosslinkParameters),
    ExpectPoWChainLength(u32),
    ExpectPoSChainLength(u64),
    ExpectPoWBlockFinality(ZebBlockHash, Option<TFLBlockFinality>),
    ExpectRejectionReason(String),
    ExpectNodeAlive,
    RecvTx { wire: Vec<u8>, peer: u64 },
    SubmitTx(Transaction),
    ExpectMempoolContains(Transaction),
    ExpectMempoolAbsent(Transaction),
    ExpectMempoolRejected(Transaction),
    /// (bond key, expected status, expected amount)
    ExpectBond([u8; 32], u64, u64),
    ExpectFinalizerBank([u8; 32], u64),
    /// [bonded, unbonded, finalizer rewards]
    ExpectPoolTotals([u64; 3]),
    /// Raw bytes: the peer serves exactly what the file holds, parseable or not.
    RecvPoW { block: Vec<u8>, peer: u64 },
    RecvStpPacket { packet: Vec<u8>, peer: u64 },
    MineFromTemplate { slot: u64 },
    /// The block named by `hash`, or by `slot` when there is no hash.
    ExpectPoWTip { hash: Option<ZebBlockHash>, slot: u64 },
    ExpectFinalizedTip { hash: Option<ZebBlockHash>, slot: u64 },
    /// `now` is the time the checks take as the present, in UNIX seconds.
    CheckPoW { block: Block, now: u64 },
    CheckPoS(BftBlock),
    SetTime(u64),
    CallRpc { method: String, params: String },
    ExpectStakingReplayMatches,
    BftRecvPacket { packet: Vec<u8>, finalizer: u64 },
    /// `bits` are the TF_BFT_* bits; `value_id` is `None` when the data was empty.
    BftRecvVote { finalizer: u64, bits: u64, height: u64, round: u32, value_id: Option<[u8; 32]> },
    BftRecvProposal { finalizer: u64, height: u64, round: u32, valid_round: i64, chunk_mask: u32, value: Vec<u8> },
    AdvanceTime { millis: u64 },
    ExpectBftStep { height: u64, round: u32, step: u64 },
    ExpectBftSent { kind: u64, height: u64, round: u32, value_id: Option<[u8; 32]> },
    ExpectBftProposer { finalizer: u64, height: u64, round: u32 },
    RosterForceInclude([u8; 32], u64),   // public address
    ExpectRosterIncludes([u8; 32], u64), // public address
}

pub(crate) async fn handle_instr(
    internal_handle: &TFLServiceHandle,
    bytes: &[u8],
    instr: TestInstr,
    flags: u32,
    instr_i: usize,
) {
    let is_bft = matches!(
        instr,
        TestInstr::BftRecvPacket { .. }
            | TestInstr::BftRecvVote { .. }
            | TestInstr::BftRecvProposal { .. }
            | TestInstr::AdvanceTime { .. }
            | TestInstr::ExpectBftStep { .. }
            | TestInstr::ExpectBftSent { .. }
            | TestInstr::ExpectBftProposer { .. }
    );
    if is_bft {
        bft_await_start(internal_handle).await;
    }
    match instr {
        TestInstr::LoadPoW(block) => {
            // let path = format!("../crosslink-test-data/test_pow_block_{}.bin", instr_i);
            // info!("writing binary at {}", path);
            // let mut file = std::fs::File::create(&path).expect("valid file");
            // file.write_all(instr.data_slice(bytes));

            // Through new_network's ingest queue, the doorway submitblock uses, so blocks take
            // the production admission path rather than a parallel one.
            let outcome = zebra_state::new_network::submit_block_to_new_network(Arc::new(block), NODE_ANSWER_WAIT).await;
            check_ingest(flags, "", outcome);
        }

        TestInstr::MineFromTemplate { slot } => match internal_handle.call.block_from_template.get() {
            None => check_ingest(
                flags,
                "MINE_FROM_TEMPLATE: ",
                Err("the node has no RPC implementation to take a template from".to_string()),
            ),
            Some(block_from_template) => match block_from_template().await {
                Err(err) => check_ingest(flags, "MINE_FROM_TEMPLATE: ", Err(err)),
                Ok(block) => {
                    let label = format!(
                        "MINE_FROM_TEMPLATE {} @ {:?}, {} transaction(s): ",
                        block.hash(),
                        block.coinbase_height().map(|height| height.0),
                        block.transactions.len()
                    );
                    if slot != 0 {
                        let mut slots = TEST_BLOCK_SLOTS.lock().unwrap();
                        slots.retain(|(taken, _)| *taken != slot);
                        slots.push((slot, block.hash()));
                    }
                    let outcome = zebra_state::new_network::submit_block_to_new_network(Arc::new(block), NODE_ANSWER_WAIT).await;
                    check_ingest(flags, &label, outcome);
                }
            },
        },

        TestInstr::CheckPoW { block, now } => {
            use zebra_consensus::sync_verify::{block_check_body, block_check_header};
            let network = &internal_handle.network;
            let label = format!("CHECK_POW {} at time {now}", block.hash());
            let (Some(height), Some(time)) = (block.coinbase_height(), chrono::DateTime::from_timestamp(now as i64, 0)) else {
                test_check(flags, false, &format!("{label}: rejected: no coinbase height, or a time out of range"));
                return;
            };
            // The order the commit loop runs them in: the header, then the body.
            let verdict = match block_check_header(&block.header, network, height, time, !network.disable_pow()) {
                Err(err) => Err(format!("header: {}", err.msg)),
                Ok(()) => match block_check_body(&block, network, height) {
                    Err(err) => Err(format!("body: {}", err.msg)),
                    Ok(_) => Ok(()),
                },
            };
            match verdict {
                Ok(()) => test_check(flags, true, &format!("{label}: accepted: the header and body checks pass")),
                Err(reason) => test_check(flags, false, &format!("{label}: rejected: {reason}")),
            }
        }

        TestInstr::CheckPoS(block) => {
            use tenderlink::TMStatus;
            let expect_pending = flags & SHOULD_DEFER != 0;
            let expect_rejected = !expect_pending && flags & SHOULD_FAIL != 0;
            let expect_accepted = !expect_pending && !expect_rejected;
            let answer = tokio::time::timeout(NODE_ANSWER_WAIT, zebra_state::new_network::bft::validate_bft_block(block)).await;
            match answer {
                Ok(Ok((TMStatus::Pass, _))) => test_check_outcome(true, expect_accepted, "CHECK_POS: accepted"),
                Ok(Ok((TMStatus::Fail, reason))) => {
                    test_check_outcome(false, expect_rejected, &format!("CHECK_POS: rejected: {reason:?}"))
                }
                Ok(Ok((TMStatus::Indeterminate, reason))) => {
                    test_check_outcome(false, expect_pending, &format!("CHECK_POS: pending: {reason:?}"))
                }
                Ok(Err(msg)) => test_check_outcome(false, false, &format!("CHECK_POS: no verdict: {msg}")),
                Err(_) => test_check_outcome(false, false, &format!("CHECK_POS: no verdict within {NODE_ANSWER_WAIT:?}")),
            }
        }

        TestInstr::CallRpc { method, params } => match internal_handle.call.rpc_call.get() {
            None => test_check_outcome(false, false, "CALL_RPC: the node has no RPC implementation"),
            Some(rpc_call) => match tokio::time::timeout(NODE_ANSWER_WAIT, rpc_call(method.clone(), params)).await {
                Ok(Ok(result)) => test_check(flags, true, &format!("CALL_RPC {method}: result {result}")),
                Ok(Err(error)) => test_check(flags, false, &format!("CALL_RPC {method}: error {error}")),
                Err(_) => test_check_outcome(false, false, &format!("CALL_RPC {method}: no verdict within {NODE_ANSWER_WAIT:?}")),
            },
        },

        TestInstr::ExpectStakingReplayMatches => {
            let (holds, message) = staking_replay_matches(internal_handle).await;
            test_check(flags, holds, &message);
        }

        TestInstr::SetTime(unix_time) => {
            zebra_debug_time::supply_time(unix_time as i64);
            let message = if unix_time == 0 {
                "SET_TIME: the node reads the real clock".to_string()
            } else {
                format!("SET_TIME: the node's clock reads {unix_time}")
            };
            test_check(flags, true, &message);
        }

        TestInstr::BftRecvPacket { packet, finalizer } => {
            let (survived, message) = bft_deliver(finalizer, vec![packet]).await;
            test_check(flags, survived, &format!("BFT_RECV_PACKET: {message}"));
        }

        TestInstr::BftRecvVote { finalizer, bits, height, round, value_id } => {
            let is_precommit = bits & TF_BFT_PRECOMMIT != 0;
            let packet = bft_engine_view().and_then(|view| {
                let key = synthetic_finalizer_key(finalizer);
                let roster_i = roster_index(&view.roster, &key)
                    .ok_or_else(|| format!("synthetic finalizer {finalizer} is not on the engine's roster"))?;
                let value_id = if bits & TF_BFT_NIL != 0 {
                    tenderlink::ValueId::NIL
                } else if bits & TF_BFT_FOR_NODE_PROPOSAL != 0 {
                    node_proposal_id(internal_handle, &view, height, round)
                        .ok_or_else(|| format!("the node has sent no proposal at {height}.{round}"))?
                } else {
                    value_id.map_or(tenderlink::ValueId::NIL, tenderlink::ValueId)
                };
                let mut namespace = view.vote_namespace;
                if bits & TF_BFT_WRONG_NAMESPACE != 0 {
                    namespace[0] ^= 1;
                }
                Ok(tenderlink::vote_packet(&key, roster_i as u16, is_precommit, height, round, value_id, &namespace))
            });
            match packet {
                Err(err) => test_check_outcome(false, false, &format!("BFT_RECV_VOTE: {err}")),
                Ok(packet) => {
                    let (survived, message) = bft_deliver(finalizer, vec![packet]).await;
                    test_check(flags, survived, &format!("BFT_RECV_VOTE: {message}"));
                }
            }
        }

        TestInstr::BftRecvProposal { finalizer, height, round, valid_round, chunk_mask, value } => {
            match bft_engine_view() {
                Err(err) => test_check_outcome(false, false, &format!("BFT_RECV_PROPOSAL: {err}")),
                Ok(view) => {
                    let key = synthetic_finalizer_key(finalizer);
                    let value_id = tenderlink::value_id_of(&view.hash_keys, &value);
                    let packets: Vec<Vec<u8>> = tenderlink::proposal_chunk_packets(
                        &key, &value, value_id, height, round, valid_round, &view.vote_namespace, view.roster.len(),
                    )
                    .into_iter()
                    .enumerate()
                    .filter(|(chunk_i, _)| chunk_mask == 0 || (*chunk_i < 32 && chunk_mask & (1 << chunk_i) != 0))
                    .map(|(_, packet)| packet)
                    .collect();
                    let (survived, message) = bft_deliver(finalizer, packets).await;
                    test_check(flags, survived, &format!("BFT_RECV_PROPOSAL: {message}"));
                }
            }
        }

        TestInstr::AdvanceTime { millis } => {
            // Every synthetic finalizer on the roster is connected, so whatever the node sends a
            // finalizer reaches the record whether or not the test has used that one yet. The
            // engine takes the connections and statuses in before the clock moves: a tick that
            // ran first would answer peers it does not have yet.
            let mut settled = Ok(());
            if let Ok(view) = bft_engine_view() {
                let status = tenderlink::status_packet_needing_everything(view.height, view.round, view.roster.len());
                {
                    let mut peers = TEST_BFT_PEERS.lock().unwrap();
                    for k in 0..SYNTHETIC_FINALIZERS_MAX_N {
                        if roster_index(&view.roster, &synthetic_finalizer_key(k.into())).is_some() {
                            bft_peer_index(&mut peers, k);
                        }
                    }
                    for peer in peers.iter() {
                        let _ = peer.link.inbound.send(status.clone());
                    }
                }
                settled = bft_settle().await;
            }
            zebra_debug_time::advance_instant(Duration::from_millis(millis));
            if !bft_engine_running() {
                test_check(flags, true, &format!("ADVANCE_TIME: the clock moved {millis} ms; BFT is not running"));
            } else {
                match settled.and(bft_settle().await) {
                    Ok(()) => test_check(flags, true, &format!("ADVANCE_TIME: the clock moved {millis} ms and the engine caught up")),
                    Err(err) => test_check_outcome(false, false, &format!("ADVANCE_TIME: {err}")),
                }
            }
        }

        TestInstr::ExpectBftStep { height, round, step } => match bft_engine_view() {
            Err(err) => test_check_outcome(false, false, &format!("BFT step: {err}")),
            Ok(view) => {
                let actual_step = match view.step {
                    tenderlink::TMStep::Propose => 0,
                    tenderlink::TMStep::Prevote => 1,
                    tenderlink::TMStep::Precommit => 2,
                };
                let holds = view.height == height && view.round == round && (step == TF_BFT_ANY_STEP || step == actual_step);
                test_check(flags, holds, &format!(
                    "BFT step: expected {height}.{round} step {step}, the engine is at {}.{} {:?}",
                    view.height, view.round, view.step,
                ));
            }
        },

        TestInstr::ExpectBftSent { kind, height, round, value_id } => {
            let settled = if bft_engine_running() { bft_settle().await } else { Ok(()) };
            match settled.and_then(|()| bft_engine_view()) {
                Err(err) => test_check_outcome(false, false, &format!("BFT sent: {err}")),
                Ok(view) => {
                    let node_i = node_roster_index(internal_handle, &view);
                    let proposer = view_proposer(&view, height, round);
                    let wanted = value_id.map_or(tenderlink::ValueId::NIL, tenderlink::ValueId);
                    let sent = TEST_BFT_SENT.lock().unwrap().iter().any(|sent| match *sent {
                        tenderlink::SentConsensus::Vote { is_precommit, height: h, round: r, value_id, roster_i } => {
                            kind == u64::from(is_precommit)
                                && (h, r) == (height, round)
                                && value_id == wanted
                                && node_i == Some(usize::from(roster_i))
                        }
                        tenderlink::SentConsensus::ProposalChunk { height: h, round: r, proposal_id, .. } => {
                            kind == 2
                                && (h, r) == (height, round)
                                && (value_id.is_none() || proposal_id == wanted)
                                && node_i.is_some()
                                && proposer == node_i
                        }
                    });
                    test_check(flags, sent, &format!(
                        "BFT sent: the node {} kind {kind} at {height}.{round} for {}",
                        if sent { "sent" } else { "has not sent" },
                        value_id.map_or("none".to_string(), |v| hex32(&v)),
                    ));
                }
            }
        }

        TestInstr::ExpectBftProposer { finalizer, height, round } => match bft_engine_view() {
            Err(err) => test_check_outcome(false, false, &format!("BFT proposer: {err}")),
            Ok(view) => {
                let expected = if finalizer == TF_BFT_THE_NODE {
                    node_roster_index(internal_handle, &view)
                } else {
                    roster_index(&view.roster, &synthetic_finalizer_key(finalizer))
                };
                let actual = view_proposer(&view, height, round);
                test_check(flags, expected.is_some() && expected == actual, &format!(
                    "BFT proposer at {height}.{round}: roster index {actual:?}, finalizer {finalizer} is at {expected:?}",
                ));
            }
        },

        TestInstr::ExpectPoWTip { hash, slot } => match named_block(hash, slot) {
            Err(err) => test_check_outcome(false, false, &format!("PoW tip: {err}")),
            Ok(expect) => {
                let answer = tokio::time::timeout(NODE_ANSWER_WAIT, (internal_handle.call.state)(StateRequest::Tip)).await;
                match answer {
                    Ok(Ok(StateResponse::Tip(Some((height, actual))))) => test_check(
                        flags,
                        Some(actual) == expect,
                        &format!("PoW tip: expected {expect:?}, actually {actual} @ {}", height.0),
                    ),
                    Ok(Ok(StateResponse::Tip(None))) => {
                        test_check(flags, expect.is_none(), &format!("PoW tip: expected {expect:?}, but the node has no tip"))
                    }
                    Ok(other) => test_check_outcome(false, false, &format!("PoW tip: no verdict, the tip request answered {other:?}")),
                    Err(_) => test_check_outcome(false, false, &format!("PoW tip: no verdict within {NODE_ANSWER_WAIT:?}")),
                }
            }
        },

        TestInstr::ExpectFinalizedTip { hash, slot } => match named_block(hash, slot) {
            Err(err) => test_check_outcome(false, false, &format!("finalized tip: {err}")),
            Ok(expect) => {
                let fin = zebra_state::new_network::fin::fin();
                let actual = fin.map(|(_, hash)| hash);
                test_check(
                    flags,
                    actual == expect,
                    &format!("finalized tip: expected {expect:?}, actually {:?}", fin.map(|(height, hash)| (height.0, hash))),
                );
            }
        },

        TestInstr::LoadPoS((block, fat_ptr)) => {
            // let path = format!("../crosslink-test-data/test_pos_block_{}.bin", instr_i);
            // info!("writing binary at {}", path);
            // let mut file = std::fs::File::create(&path).expect("valid file");
            // file.write_all(instr.data_slice(bytes)).expect("write success");

            // A force-feed is decided or refused at once, so it is never pending: SHOULD_DEFER
            // cannot hold. Not reaching the BFT loop at all is no verdict, as for LOAD_POW.
            let expect_rejected = flags & SHOULD_DEFER == 0 && flags & SHOULD_FAIL != 0;
            let expect_accepted = flags & SHOULD_DEFER == 0 && !expect_rejected;
            let answer = tokio::time::timeout(
                NODE_ANSWER_WAIT,
                zebra_state::new_network::bft::force_feed_bft_block(Arc::new(block), fat_ptr),
            )
            .await;
            match answer {
                Ok(Ok(())) => test_check_outcome(true, expect_accepted, "accepted: PoS force feed ok"),
                Ok(Err(msg)) if msg.starts_with("new_network ") => {
                    test_check_outcome(false, false, &format!("no verdict: {msg}"))
                }
                Ok(Err(msg)) => test_check_outcome(false, expect_rejected, &format!("rejected: {msg}")),
                Err(_) => test_check_outcome(false, false, &format!("no verdict: no answer within {NODE_ANSWER_WAIT:?}")),
            }
        }

        TestInstr::SetParams(params) => {
            debug_assert!(instr_i == 0, "should only be set at the beginning");
            // Consensus parameters are fixed when the network is built, before the node boots (see
            // `crosslink_parameters_for_test`), so all that remains is confirming the node agrees.
            test_check(
                flags,
                params == internal_handle.params,
                &format!("SET_PARAMS: file declares {:?}, node runs {:?}", params, internal_handle.params),
            );
        }

        TestInstr::ExpectPoWChainLength(h) => {
            // Bounded, and every answer is recorded: an unbounded wait hangs the test if the state
            // service stops answering, and skipping the check on a missing tip passed silently.
            let (holds, message) = match tokio::time::timeout(
                NODE_ANSWER_WAIT,
                (internal_handle.call.state)(StateRequest::Tip),
            )
            .await
            {
                Ok(Ok(StateResponse::Tip(Some((height, _))))) => {
                    let actual = height.0 + 1;
                    (h == actual, format!("PoW chain length: expected {h}, actually {actual}"))
                }
                Ok(Ok(StateResponse::Tip(None))) => {
                    (false, format!("PoW chain length: expected {h}, but the node has no tip"))
                }
                Ok(Ok(other)) => (false, format!("PoW chain length: the tip request answered {other:?}")),
                Ok(Err(err)) => (false, format!("PoW chain length: the tip request failed: {err}")),
                Err(_) => (false, format!("PoW chain length: no answer to the tip request within {NODE_ANSWER_WAIT:?}")),
            };
            test_check(flags, holds, &message);
        }

        TestInstr::ExpectRejectionReason(reason) => {
            let previous = TEST_PREV_OUTCOME.lock().unwrap().clone();
            let (holds, message) = match previous {
                Some((false, rejection)) => (
                    rejection.contains(&reason),
                    format!("rejection reason: expected it to contain {reason:?}, actually {rejection:?}"),
                ),
                Some((true, accepted)) => (
                    false,
                    format!("rejection reason: expected a rejection containing {reason:?}, but the previous instruction was accepted: {accepted:?}"),
                ),
                None => (
                    false,
                    format!("rejection reason: expected a rejection containing {reason:?}, but the previous instruction made no check"),
                ),
            };
            test_check(flags, holds, &message);
        }

        TestInstr::ExpectNodeAlive => {
            let (alive, message) = node_alive(internal_handle).await;
            test_check(flags, alive, &message);
        }

        TestInstr::RecvTx { wire, peer } => {
            let sender = synthetic_peer(peer);
            let delivery = tokio::time::timeout(NODE_ANSWER_WAIT, (internal_handle.call.inbound_wire)(sender, wire)).await;
            let (delivered, message) = match delivery {
                Ok(Ok(())) => (true, format!("RECV_TX: delivered from peer {sender}")),
                Ok(Err(err)) => (false, format!("RECV_TX: the message from peer {sender} was refused: {err}")),
                Err(_) => (false, format!("RECV_TX: no answer delivering from peer {sender} within {NODE_ANSWER_WAIT:?}")),
            };
            test_check(flags, delivered, &message);
        }

        TestInstr::SubmitTx(transaction) => {
            let (accepted, message) = submit_tx(internal_handle, transaction).await;
            test_check(flags, accepted, &message);
        }

        TestInstr::RecvPoW { block, peer } => match take_stp_peer(peer) {
            Err(err) => check_ingest(flags, "RECV_POW: ", Err(err)),
            Ok(mut stp) => {
                let outcome = serve_block(internal_handle, &mut stp, &block).await;
                put_stp_peer(stp);
                check_ingest(flags, "RECV_POW: ", outcome);
            }
        },

        TestInstr::RecvStpPacket { packet, peer } => match take_stp_peer(peer) {
            Err(err) => test_check_outcome(false, false, &format!("RECV_STP_PACKET: {err}")),
            Ok(mut stp) => {
                let (survived, message) = deliver_stp_packet(&mut stp, packet).await;
                put_stp_peer(stp);
                test_check(flags, survived, &message);
            }
        },

        TestInstr::ExpectMempoolContains(transaction) => {
            let (holds, message) = await_mempool(internal_handle, &transaction, MempoolExpect::Resident).await;
            test_check(flags, holds, &message);
        }

        TestInstr::ExpectMempoolAbsent(transaction) => {
            let (holds, message) = await_mempool(internal_handle, &transaction, MempoolExpect::Absent).await;
            test_check(flags, holds, &message);
        }

        TestInstr::ExpectMempoolRejected(transaction) => {
            let (holds, message) = await_mempool(internal_handle, &transaction, MempoolExpect::Rejected).await;
            test_check(flags, holds, &message);
        }

        TestInstr::ExpectBond(key, status, amount) => {
            let answer = tokio::time::timeout(
                NODE_ANSWER_WAIT,
                (internal_handle.call.state)(StateRequest::BondInfo(key)),
            )
            .await;
            let (holds, message) = match answer {
                Ok(Ok(StateResponse::BondInfo(info))) => {
                    let actual_status = info.as_ref().map_or(TF_BOND_ABSENT, |info| u64::from(info.status));
                    let actual_amount = info.as_ref().map(|info| u64::from(info.amount));
                    let status_holds = status == TEST_STAKE_IGNORED || status == actual_status;
                    let amount_holds = amount == TEST_STAKE_IGNORED || actual_amount == Some(amount);
                    let actual = match &info {
                        Some(info) => format!(
                            "status {} amount {} (last action at {}, target {})",
                            info.status,
                            u64::from(info.amount),
                            info.last_action_height,
                            PubKeyID(info.target_finalizer),
                        ),
                        None => "no such bond".to_string(),
                    };
                    (
                        status_holds && amount_holds,
                        format!("bond {}: expected status {} amount {}, actually {actual}", hex32(&key), stake_expectation(status), stake_expectation(amount)),
                    )
                }
                Ok(Ok(other)) => (false, format!("bond {}: the bond request answered {other:?}", hex32(&key))),
                Ok(Err(err)) => (false, format!("bond {}: the bond request failed: {err}", hex32(&key))),
                Err(_) => (false, format!("bond {}: no answer within {NODE_ANSWER_WAIT:?}", hex32(&key))),
            };
            test_check(flags, holds, &message);
        }

        TestInstr::ExpectFinalizerBank(pub_key, amount) => {
            let answer = tokio::time::timeout(
                NODE_ANSWER_WAIT,
                (internal_handle.call.state)(StateRequest::FinalizerRewardBalance(pub_key)),
            )
            .await;
            let finalizer = PubKeyID(pub_key);
            let (holds, message) = match answer {
                Ok(Ok(StateResponse::FinalizerRewardBalance(bank))) => {
                    (bank == amount, format!("finalizer {finalizer} bank: expected {amount}, actually {bank}"))
                }
                Ok(Ok(other)) => (false, format!("finalizer {finalizer} bank: the request answered {other:?}")),
                Ok(Err(err)) => (false, format!("finalizer {finalizer} bank: the request failed: {err}")),
                Err(_) => (false, format!("finalizer {finalizer} bank: no answer within {NODE_ANSWER_WAIT:?}")),
            };
            test_check(flags, holds, &message);
        }

        TestInstr::ExpectPoolTotals(expected) => {
            let answer = tokio::time::timeout(
                NODE_ANSWER_WAIT,
                (internal_handle.call.read_state)(zebra_state::ReadRequest::TipPoolValues),
            )
            .await;
            let (holds, message) = match answer {
                Ok(Ok(zebra_state::ReadResponse::TipPoolValues { tip_height, value_balance, .. })) => {
                    let actual = [
                        u64::from(value_balance.staking_bonded_amount()),
                        u64::from(value_balance.staking_unbonded_amount()),
                        u64::from(value_balance.finalizer_rewards_amount()),
                    ];
                    let holds = expected.iter().zip(actual).all(|(&e, a)| e == TEST_STAKE_IGNORED || e == a);
                    (
                        holds,
                        format!(
                            "pool totals at height {} [bonded, unbonded, finalizer rewards]: expected [{}], actually {actual:?}",
                            tip_height.0,
                            expected.map(stake_expectation).join(", ")
                        ),
                    )
                }
                Ok(Ok(other)) => (false, format!("pool totals: the request answered {other:?}")),
                Ok(Err(err)) => (false, format!("pool totals: the request failed: {err}")),
                Err(_) => (false, format!("pool totals: no answer within {NODE_ANSWER_WAIT:?}")),
            };
            test_check(flags, holds, &message);
        }

        TestInstr::ExpectPoSChainLength(h) => {
            let expect = h as usize;
            let actual = zebra_state::new_network::bft::bft_chain().read().unwrap().blocks.len();
            test_check(
                flags,
                expect == actual,
                &format!("PoS chain length: expected {}, actually {}", expect, actual),
            ); // TODO: maybe assert in test but recoverable error in-GUI
        }

        TestInstr::ExpectPoWBlockFinality(hash, f) => {
            let expect = f;
            // Only for the message, so a slow or failed lookup just leaves it out.
            let height = tokio::time::timeout(NODE_ANSWER_WAIT, block_height_from_hash(&internal_handle.call.clone(), hash))
                .await
                .ok()
                .flatten();
            // The state service answers `fin` and the best chain in one read (FINALITY.md
            // §7.2), so the harness asks it exactly as the RPC does. It always answers with a
            // finality, so an expected `None` never matches; a failed or missing answer is no
            // verdict, which never passes.
            let answer = tokio::time::timeout(
                NODE_ANSWER_WAIT,
                (internal_handle.call.read_state)(zebra_state::ReadRequest::CrosslinkBlockFinality(hash)),
            )
            .await;
            match answer {
                Ok(Ok(zebra_state::ReadResponse::CrosslinkBlockFinality(finality))) => {
                    let actual = Some(finality);
                    test_check(
                        flags,
                        expect == actual,
                        &format!(
                            "PoW block finality at hash={}, height={:?}: expected {:?}, actually {:?}",
                            hash, height, expect, actual
                        ),
                    );
                }
                Ok(other) => test_check_outcome(
                    false,
                    false,
                    &format!("PoW block finality at hash={hash}: no verdict, the read answered {other:?}"),
                ),
                Err(_) => test_check_outcome(
                    false,
                    false,
                    &format!("PoW block finality at hash={hash}: no verdict within {NODE_ANSWER_WAIT:?}"),
                ),
            }
        }

        TestInstr::ExpectRosterIncludes(pub_key, stake) => {
            let key = PubKeyID(pub_key);
            let finalizer = zebra_state::new_network::bft::bft_chain()
                .read()
                .unwrap()
                .roster
                .iter()
                .find(|x| PubKeyID(x.pub_key) == key)
                .cloned();

            if let Some(finalizer) = finalizer {
                test_check(
                    flags,
                    stake == TEST_STAKE_IGNORED || stake == finalizer.voting_power,
                    &format!(
                        "Finalizer stake: expected {}, actually {}",
                        stake, finalizer.voting_power
                    ),
                );
            } else {
                test_check(
                    flags,
                    false,
                    &format!("Finalizer found: {:?}", key),
                );
            }
        }

        TestInstr::RosterForceInclude(pub_key, stake) => {
            zebra_state::new_network::bft::bft_chain()
                .write()
                .unwrap()
                .roster
                .push(RosterMember { pub_key, voting_power: stake, txids: Vec::new(), finalizer_address: None });
        }
    }
}

/// How long an instruction waits for the node to answer before recording that it didn't.
const NODE_ANSWER_WAIT: Duration = Duration::from_secs(30);

/// Whether the node still answers: its state service returns the tip within a bound, and
/// re-submitting the tip block through the ingest path -- the doorway every LOAD_POW uses --
/// answers `Known`, which only a live sync loop can do.
async fn node_alive(internal_handle: &TFLServiceHandle) -> (bool, String) {
    let tip = tokio::time::timeout(NODE_ANSWER_WAIT, (internal_handle.call.state)(StateRequest::Tip)).await;
    let (height, hash) = match tip {
        Ok(Ok(StateResponse::Tip(Some(tip)))) => tip,
        Ok(Ok(other)) => return (false, format!("node alive: the tip request answered {other:?}")),
        Ok(Err(err)) => return (false, format!("node alive: the tip request failed: {err}")),
        Err(_) => return (false, format!("node alive: no answer to the tip request within {NODE_ANSWER_WAIT:?}")),
    };
    let block = tokio::time::timeout(
        NODE_ANSWER_WAIT,
        (internal_handle.call.state)(StateRequest::Block(hash.into())),
    )
    .await;
    let block = match block {
        Ok(Ok(StateResponse::Block(Some(block)))) => block,
        other => return (false, format!("node alive: could not read the tip block {hash} at height {}: {other:?}", height.0)),
    };
    match zebra_state::new_network::submit_block_to_new_network(block, NODE_ANSWER_WAIT).await {
        Ok(zebra_state::new_network::IngestOutcome::Known { .. }) => {
            (true, format!("node alive: re-submitting tip {hash} at height {} answered Known", height.0))
        }
        Ok(other) => (false, format!("node alive: re-submitting the tip answered {other:?}, not Known")),
        Err(err) => (false, format!("node alive: the ingest loop did not answer re-submitting the tip: {err}")),
    }
}

fn hex32(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A synthetic STP peer and what the harness knows about it. A killed peer stays killed for the
/// rest of the test, as a real one would.
struct HarnessStpPeer {
    index: u64,
    link: zebra_state::new_network::SyntheticPeer,
    killed: Option<String>,
}

/// Peers wait here between instructions; an instruction takes its peer out, so no lock is held
/// across an await.
static TEST_STP_PEERS: Mutex<Vec<HarnessStpPeer>> = Mutex::new(Vec::new());

/// How long a packet gets to provoke a kill. The sync loop ticks every 100 ms, and nothing
/// acknowledges a packet, so this is a bound rather than a handshake.
const STP_PACKET_SETTLE: Duration = Duration::from_secs(1);

/// How long to watch a peer advertise a block the node already has. The node never requests it,
/// so there is no verdict to wait for, only a kill to rule out.
const KNOWN_BLOCK_SETTLE: Duration = Duration::from_secs(2);

/// Replays the best chain's staking from genesis and compares the result with the node's bonds
/// and finalizer banks. `holds` is false on any difference; a read the node did not answer is a
/// failure too, since the comparison could not be made.
async fn staking_replay_matches(internal_handle: &TFLServiceHandle) -> (bool, String) {
    use zebra_state::{BondStatusInChain, StakingReplay};

    let tip = match tokio::time::timeout(NODE_ANSWER_WAIT, (internal_handle.call.state)(StateRequest::Tip)).await {
        Ok(Ok(StateResponse::Tip(Some((height, _))))) => height.0,
        other => return (false, format!("staking replay: no verdict, the tip request answered {other:?}")),
    };
    let mut blocks: Vec<Arc<Block>> = Vec::with_capacity(tip as usize + 1);
    for height in 0..=tip {
        let request = zebra_state::ReadRequest::Block(zebra_chain::block::Height(height).into());
        match tokio::time::timeout(NODE_ANSWER_WAIT, (internal_handle.call.read_state)(request)).await {
            Ok(Ok(zebra_state::ReadResponse::Block(Some(block)))) => blocks.push(block),
            other => return (false, format!("staking replay: no verdict, block {height} answered {other:?}")),
        }
    }

    let slashfork_schedule = zebra_chain::parameters::SlashForkSchedule::from_canonical(internal_handle.config.slashforks.clone());
    let mut replay = StakingReplay::new(&slashfork_schedule, internal_handle.params.staking.slash_analysis_window());
    for (height, block) in blocks.iter().enumerate() {
        // Every block pays issuance: variable reward is disabled in Chain::push, and the stake
        // repair tool (stake_fixup.rs) replays with the same rule. If that changes, so must this.
        let pays_reward = true;
        let block_at = |at: zebra_chain::block::Height| blocks[at.0 as usize].clone();
        let height = zebra_chain::block::Height(height as u32);
        if let Err(err) = replay.apply_block(&internal_handle.network, height, block, pays_reward, block_at) {
            return (false, format!("staking replay: the chain does not replay at height {}: {err:?}", height.0));
        }
    }

    let mut differences = Vec::new();
    for (key, (bond, status)) in &replay.delegation_bonds {
        let replayed_status: u8 = match status {
            BondStatusInChain::Active => 0,
            BondStatusInChain::Unbonding { .. } => 1,
            BondStatusInChain::Withdrawn { .. } => 2,
            BondStatusInChain::Burned => 3,
        };
        let answer = tokio::time::timeout(NODE_ANSWER_WAIT, (internal_handle.call.state)(StateRequest::BondInfo(*key))).await;
        match answer {
            Ok(Ok(StateResponse::BondInfo(Some(info)))) => {
                if info.amount != bond.amount || info.status != replayed_status || info.target_finalizer != bond.target_finalizer {
                    differences.push(format!(
                        "bond {}: replay has amount {} status {replayed_status}, node has amount {} status {}",
                        hex32(key),
                        u64::from(bond.amount),
                        u64::from(info.amount),
                        info.status
                    ));
                }
            }
            Ok(Ok(StateResponse::BondInfo(None))) => differences.push(format!("bond {}: in the replay, not in the node", hex32(key))),
            other => return (false, format!("staking replay: no verdict, bond {} answered {other:?}", hex32(key))),
        }
    }

    let node_banks = match tokio::time::timeout(NODE_ANSWER_WAIT, (internal_handle.call.state)(StateRequest::FinalizerRewardBalances)).await {
        Ok(Ok(StateResponse::FinalizerRewardBalances(banks))) => banks,
        other => return (false, format!("staking replay: no verdict, the banks request answered {other:?}")),
    };
    let replayed_banks: std::collections::BTreeMap<[u8; 32], u64> =
        replay.finalizer_rewards.iter().filter(|(_, bank)| **bank != 0).map(|(finalizer, bank)| (*finalizer, *bank)).collect();
    let node_banks: std::collections::BTreeMap<[u8; 32], u64> = node_banks.into_iter().filter(|(_, bank)| *bank != 0).collect();
    for finalizer in replayed_banks.keys().chain(node_banks.keys()).collect::<std::collections::BTreeSet<_>>() {
        let (replayed, node) = (replayed_banks.get(finalizer), node_banks.get(finalizer));
        if replayed != node {
            differences.push(format!("bank of {}: replay has {replayed:?}, node has {node:?}", PubKeyID(*finalizer)));
        }
    }

    let checked = format!("{} bonds and {} banks over {} blocks", replay.delegation_bonds.len(), replayed_banks.len(), blocks.len());
    if differences.is_empty() {
        (true, format!("staking replay matches the node: {checked}"))
    } else {
        (false, format!("staking replay differs from the node ({checked}): {}", differences.join("; ")))
    }
}

/// Hashes of blocks the node built during the test, by the slot MINE_FROM_TEMPLATE put them in.
static TEST_BLOCK_SLOTS: Mutex<Vec<(u64, ZebBlockHash)>> = Mutex::new(Vec::new());

/// The block an instruction names: its hash, else the one remembered in its slot, else none.
fn named_block(hash: Option<ZebBlockHash>, slot: u64) -> Result<Option<ZebBlockHash>, String> {
    if hash.is_some() || slot == 0 {
        return Ok(hash);
    }
    match TEST_BLOCK_SLOTS.lock().unwrap().iter().find(|(taken, _)| *taken == slot) {
        Some((_, hash)) => Ok(Some(*hash)),
        None => Err(format!("slot {slot} holds no block; MINE_FROM_TEMPLATE fills slots")),
    }
}

struct HarnessBftPeer {
    index: u64,
    link: tenderlink::SyntheticBftPeer,
}

static TEST_BFT_PEERS: Mutex<Vec<HarnessBftPeer>> = Mutex::new(Vec::new());

/// The votes and proposal chunks the node has sent any synthetic finalizer during the test, each
/// once. The node resends what a peer's status asks for on every tick, so most arrive many times.
static TEST_BFT_SENT: Mutex<Vec<tenderlink::SentConsensus>> = Mutex::new(Vec::new());

fn synthetic_finalizer_key(k: u64) -> ed25519_zebra::SigningKey {
    finalizer_key_from_seed(synthetic_finalizer_seed(k).as_bytes()).1
}

fn roster_index(roster: &[tenderlink::SortedRosterMember], key: &ed25519_zebra::SigningKey) -> Option<usize> {
    let pub_key = PubKeyID(ed25519_zebra::VerificationKeyBytes::from(key).into());
    roster.iter().position(|member| member.pub_key == pub_key)
}

fn node_roster_index(internal_handle: &TFLServiceHandle, view: &tenderlink::EngineView) -> Option<usize> {
    let seed = internal_handle.config.explicit_bft_key_seed.as_deref().unwrap_or(TEST_NODE_FINALIZER_SEED);
    roster_index(&view.roster, &finalizer_key_from_seed(seed.as_bytes()).1)
}

fn view_proposer(view: &tenderlink::EngineView, height: u64, round: u32) -> Option<usize> {
    tenderlink::proposer_index(&view.hash_keys, &view.roster, height, round)
}

/// The id of the proposal the node sent at `height` and `round`, when it was that round's proposer.
fn node_proposal_id(internal_handle: &TFLServiceHandle, view: &tenderlink::EngineView, height: u64, round: u32) -> Option<tenderlink::ValueId> {
    let node_i = node_roster_index(internal_handle, view)?;
    if view_proposer(view, height, round) != Some(node_i) {
        return None;
    }
    TEST_BFT_SENT.lock().unwrap().iter().find_map(|sent| match *sent {
        tenderlink::SentConsensus::ProposalChunk { height: h, round: r, proposal_id, .. } if (h, r) == (height, round) => Some(proposal_id),
        _ => None,
    })
}

fn bft_engine_running() -> bool {
    zebra_state::new_network::bft::bft_chain().read().unwrap().is_activated
}

/// BFT starts on the sync loop's tick after the activation block commits, so it can trail the
/// instruction that loaded that block. With a bootstrap configured, waits for it to start; the
/// bound is for a node that never starts it, which the instruction then reports.
async fn bft_await_start(internal_handle: &TFLServiceHandle) {
    if bft_engine_running() || internal_handle.params.bootstrap.activation_height().is_none() {
        return;
    }
    let _ = tokio::time::timeout(NODE_ANSWER_WAIT, async {
        while !bft_engine_running() {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await;
}

fn bft_engine_view() -> Result<tenderlink::EngineView, String> {
    if !bft_engine_running() {
        return Err("BFT is not running (it starts at a FromChain bootstrap's activation height)".to_string());
    }
    tenderlink::engine_view().ok_or_else(|| "the BFT engine has not ticked yet; ADVANCE_TIME makes it".to_string())
}

/// Where synthetic finalizer `k` is in `peers`, connecting it to the engine, with its identity
/// hello, if it is not there yet.
fn bft_peer_index(peers: &mut Vec<HarnessBftPeer>, k: u16) -> usize {
    if let Some(peer_i) = peers.iter().position(|p| p.index == u64::from(k)) {
        return peer_i;
    }
    let link = tenderlink::attach_synthetic_bft_peer(k);
    let _ = link.inbound.send(tenderlink::id_hello_packet(&synthetic_finalizer_key(k.into()), &link.handshake_hash));
    peers.push(HarnessBftPeer { index: k.into(), link });
    peers.len() - 1
}

/// Synthetic finalizers are looked for on the roster up to this index.
pub const SYNTHETIC_FINALIZERS_MAX_N: u16 = 64;

/// Hands `packets` to the engine as synthetic finalizer `k` sent them, connecting it first, then
/// waits for the engine to take them in. Answers whether the peer is still connected, and why not.
async fn bft_deliver(k: u64, packets: Vec<Vec<u8>>) -> (bool, String) {
    if !bft_engine_running() {
        return (false, "BFT is not running (it starts at a FromChain bootstrap's activation height)".to_string());
    }
    let Ok(link_index) = u16::try_from(k) else {
        return (false, format!("synthetic finalizer index {k} does not fit in 16 bits"));
    };
    let packets_n = packets.len();
    {
        let mut peers = TEST_BFT_PEERS.lock().unwrap();
        let peer_i = bft_peer_index(&mut peers, link_index);
        let peer = &peers[peer_i];
        if !peer.link.alive.load(std::sync::atomic::Ordering::Relaxed) {
            return (false, format!("the node had already disconnected synthetic finalizer {k}"));
        }
        for packet in packets {
            let _ = peer.link.inbound.send(packet);
        }
    }
    if let Err(err) = bft_settle().await {
        return (false, err);
    }
    let alive = TEST_BFT_PEERS.lock().unwrap().iter().find(|p| p.index == k).is_some_and(|p| p.link.alive.load(std::sync::atomic::Ordering::Relaxed));
    if alive {
        (true, format!("{packets_n} packet(s) from synthetic finalizer {k} taken in"))
    } else {
        (false, format!("the node disconnected synthetic finalizer {k}"))
    }
}

/// Returns once the engine has run the loop passes that take in, read and act on whatever reached
/// it before the call, then records what the node sent. The bound is for an engine that has
/// stopped; a running one passes in milliseconds.
async fn bft_settle() -> Result<(), String> {
    use std::sync::atomic::Ordering;
    let start = tenderlink::LOOP_PASSES.load(Ordering::Acquire);
    let waited = tokio::time::timeout(NODE_ANSWER_WAIT, async {
        while tenderlink::LOOP_PASSES.load(Ordering::Acquire) < start + 3 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await;
    if waited.is_err() {
        return Err(format!("the BFT engine ran no loop passes within {NODE_ANSWER_WAIT:?}"));
    }
    let mut sent = TEST_BFT_SENT.lock().unwrap();
    for peer in TEST_BFT_PEERS.lock().unwrap().iter() {
        while let Ok(packet) = peer.link.outbound.try_recv() {
            for item in tenderlink::sent_consensus(&packet) {
                if !sent.contains(&item) {
                    sent.push(item);
                }
            }
        }
    }
    Ok(())
}

fn take_stp_peer(index: u64) -> Result<HarnessStpPeer, String> {
    let mut peers = TEST_STP_PEERS.lock().unwrap();
    if let Some(i) = peers.iter().position(|p| p.index == index) {
        return Ok(peers.swap_remove(i));
    }
    let Ok(link_index) = u16::try_from(index) else {
        return Err(format!("synthetic peer index {index} does not fit in 16 bits"));
    };
    Ok(HarnessStpPeer { index, link: zebra_state::new_network::attach_synthetic_peer(link_index), killed: None })
}

fn put_stp_peer(peer: HarnessStpPeer) {
    TEST_STP_PEERS.lock().unwrap().push(peer);
}

/// Forgets whatever the peer received since its last instruction, keeping only a kill.
fn drain_stp_peer(stp: &mut HarnessStpPeer) {
    while stp.link.outbound.try_recv().is_ok() {}
    while let Ok(event) = stp.link.events.try_recv() {
        if let zebra_state::new_network::SyntheticPeerEvent::Killed(reason) = event {
            stp.killed = Some(reason);
        }
    }
}

async fn deliver_stp_packet(stp: &mut HarnessStpPeer, packet: Vec<u8>) -> (bool, String) {
    use zebra_state::new_network::SyntheticPeerEvent;
    let index = stp.index;
    drain_stp_peer(stp);
    if let Some(reason) = &stp.killed {
        return (false, format!("RECV_STP_PACKET: peer {index} was already killed: {reason}"));
    }
    let _ = stp.link.inbound.send(packet);

    let deadline = tokio::time::Instant::now() + STP_PACKET_SETTLE;
    loop {
        tokio::select! {
            event = stp.link.events.recv() => match event {
                Some(SyntheticPeerEvent::Killed(reason)) => {
                    stp.killed = Some(reason.clone());
                    return (false, format!("RECV_STP_PACKET: the node killed peer {index}: {reason}"));
                }
                Some(_) => {}
                None => return (false, "RECV_STP_PACKET: the sync loop is gone".to_string()),
            },
            _ = tokio::time::sleep_until(deadline) => {
                return (true, format!("RECV_STP_PACKET: peer {index} still connected after {STP_PACKET_SETTLE:?}"));
            }
        }
    }
}

/// Plays a peer that has `wire_block`: advertises it, then answers the node's requests for it.
/// A kill is a rejection: the node refused the peer, and the block with it. `Err` is a delivery
/// that never reached a verdict.
async fn serve_block(
    internal_handle: &TFLServiceHandle,
    stp: &mut HarnessStpPeer,
    wire_block: &[u8],
) -> Result<zebra_state::new_network::IngestOutcome, String> {
    use zebra_state::new_network::{
        block_chunk_packets, parse_block_request, status_packet, IngestOutcome, NearTipChains, ShadowBlock,
        SyntheticPeerEvent,
    };
    let index = stp.index;
    let killed = |reason: String| Ok(IngestOutcome::Failed { reason, misbehavior_score: 0 });
    let Ok(block) = Block::zcash_deserialize(wire_block) else {
        return Err("the block does not parse, so no STATUS can advertise it; send its packets with RECV_STP_PACKET".to_string());
    };
    let hash = block.hash();
    let Some(height) = block.coinbase_height().map(|height| height.0) else {
        return Err(format!("block {hash} has no coinbase height, so no STATUS can advertise it"));
    };

    drain_stp_peer(stp);
    if let Some(reason) = &stp.killed {
        return killed(format!("peer {index} was already killed: {reason}"));
    }

    let already_known = match tokio::time::timeout(
        NODE_ANSWER_WAIT,
        (internal_handle.call.state)(StateRequest::KnownBlock(hash)),
    )
    .await
    {
        Ok(Ok(StateResponse::KnownBlock(Some(known)))) => Some(known),
        _ => None,
    };

    let mut chains = NearTipChains::default();
    chains.push_chain_unchecked(vec![ShadowBlock { this_hash: hash, parent_hash: block.header.previous_block_hash, this_height: height }]);
    let _ = stp.link.inbound.send(status_packet(&[], &chains, None));

    let deadline = tokio::time::Instant::now() + if already_known.is_some() { KNOWN_BLOCK_SETTLE } else { NODE_ANSWER_WAIT };
    let (mut requests, mut chunks) = (0usize, 0usize);
    loop {
        tokio::select! {
            packet = stp.link.outbound.recv() => {
                let Some(packet) = packet else {
                    return Err("the sync loop is gone".to_string());
                };
                let Some((req_height, req_hash, offset)) = parse_block_request(&packet) else {
                    continue; // STATUS, address gossip, hole punching: a real peer would handle these, but they don't bear on this block
                };
                let asks_for_this = req_hash == hash || (req_hash == ZebBlockHash([0; 32]) && req_height == height);
                if asks_for_this && (offset as usize) < wire_block.len() {
                    requests += 1;
                    for chunk in block_chunk_packets(wire_block, height, hash, offset as usize) {
                        chunks += 1;
                        let _ = stp.link.inbound.send(chunk);
                    }
                }
            }
            event = stp.link.events.recv() => match event {
                Some(SyntheticPeerEvent::Killed(reason)) => {
                    stp.killed = Some(reason.clone());
                    return killed(format!("the node killed peer {index}: {reason}"));
                }
                Some(SyntheticPeerEvent::Outcome { hash: decided, outcome }) if decided == hash => {
                    return Ok(outcome);
                }
                Some(_) => {}
                None => return Err("the sync loop is gone".to_string()),
            },
            _ = tokio::time::sleep_until(deadline) => {
                if let (Some(known), 0) = (&already_known, requests) {
                    return Ok(IngestOutcome::Known { location: known.location.clone(), height: known.height });
                }
                return Err(format!(
                    "{hash} @ {height} after {NODE_ANSWER_WAIT:?}; the node sent {requests} request(s) for it, answered with {chunks} chunk(s)"
                ));
            }
        }
    }
}

fn stake_expectation(value: u64) -> String {
    if value == TEST_STAKE_IGNORED { "*".to_string() } else { value.to_string() }
}

/// The address synthetic peer `index` sends from. Distinct peers get distinct addresses, so each
/// meets the mempool's per-peer limits separately.
fn synthetic_peer(index: u64) -> std::net::SocketAddr {
    std::net::SocketAddr::from(([127, 1, (index >> 8) as u8, index as u8], 18233))
}

/// Submits `transaction` locally, as the wallet and RPC do, and returns the mempool's verdict:
/// accepted, or rejected with its reason.
async fn submit_tx(internal_handle: &TFLServiceHandle, transaction: Transaction) -> (bool, String) {
    use zebra_node_services::mempool::{Gossip, Request, Response};

    let txid = transaction.hash();
    let queued = tokio::time::timeout(
        NODE_ANSWER_WAIT,
        (internal_handle.call.mempool)(Request::Queue(vec![Gossip::Tx(transaction.into())])),
    )
    .await;
    let verdict = match queued {
        Ok(Ok(Response::Queued(mut results))) if results.len() == 1 => match results.remove(0) {
            Ok(verdict) => verdict,
            Err(err) => return (false, format!("SUBMIT_TX {txid}: the mempool refused to queue it: {err}")),
        },
        Ok(Ok(other)) => return (false, format!("SUBMIT_TX {txid}: the mempool answered {other:?}")),
        Ok(Err(err)) => return (false, format!("SUBMIT_TX {txid}: the queue request failed: {err}")),
        Err(_) => return (false, format!("SUBMIT_TX {txid}: no answer from the mempool within {NODE_ANSWER_WAIT:?}")),
    };
    match tokio::time::timeout(NODE_ANSWER_WAIT, verdict).await {
        Ok(Ok(Ok(()))) => (true, format!("SUBMIT_TX {txid}: accepted into the mempool")),
        Ok(Ok(Err(err))) => (false, format!("SUBMIT_TX {txid}: rejected: {err}")),
        Ok(Err(_)) => (false, format!("SUBMIT_TX {txid}: the mempool dropped the verdict")),
        Err(_) => (false, format!("SUBMIT_TX {txid}: no verdict within {NODE_ANSWER_WAIT:?}")),
    }
}

#[derive(Clone, Copy, Debug)]
enum MempoolExpect {
    Resident,
    Absent,
    Rejected,
}

/// Waits, within a bound, for `transaction` to reach the state `expect` names in the mempool.
/// Peer-sent transactions are downloaded and verified asynchronously, so a single read would race
/// the verification.
async fn await_mempool(
    internal_handle: &TFLServiceHandle,
    transaction: &Transaction,
    expect: MempoolExpect,
) -> (bool, String) {
    use zebra_node_services::mempool::{Request, Response};

    let id = UnminedTxId::from(transaction);
    let txid = transaction.hash();
    let ask = |request: Request| {
        tokio::time::timeout(NODE_ANSWER_WAIT, (internal_handle.call.mempool)(request))
    };
    let deadline = tokio::time::Instant::now() + NODE_ANSWER_WAIT;
    loop {
        let resident = match ask(Request::TransactionIds).await {
            Ok(Ok(Response::TransactionIds(ids))) => ids.contains(&id),
            other => return (false, format!("mempool {txid}: the transaction ids request answered {other:?}")),
        };
        let in_rejected_set = match ask(Request::RejectedTransactionIds([id].into_iter().collect())).await {
            Ok(Ok(Response::RejectedTransactionIds(ids))) => !ids.is_empty(),
            other => return (false, format!("mempool {txid}: the rejected ids request answered {other:?}")),
        };
        // The mempool files a mined transaction under its rejected ids too (as Mined), and does
        // not say which reason applies, so the best chain decides: mined is not rejected.
        let mined = match tokio::time::timeout(NODE_ANSWER_WAIT, (internal_handle.call.state)(StateRequest::Transaction(txid))).await {
            Ok(Ok(StateResponse::Transaction(found))) => found.is_some(),
            other => return (false, format!("mempool {txid}: the mined transaction request answered {other:?}")),
        };
        let rejected = in_rejected_set && !mined;
        let state = format!("resident {resident}, rejected {rejected}, mined {mined}");
        let reached = match expect {
            MempoolExpect::Resident => resident,
            MempoolExpect::Absent => !resident,
            MempoolExpect::Rejected => rejected,
        };
        if reached {
            return (true, format!("mempool {txid}: {expect:?} ({state})"));
        }
        // A rejected or mined transaction won't become resident, and a mined one won't become
        // rejected; say so now rather than at the deadline.
        let settled_otherwise = match expect {
            MempoolExpect::Resident => rejected || mined,
            MempoolExpect::Rejected => mined,
            MempoolExpect::Absent => false,
        };
        if settled_otherwise {
            return (false, format!("mempool {txid}: expected {expect:?}, but it is {state}"));
        }
        if tokio::time::Instant::now() >= deadline {
            return (
                false,
                format!(
                    "mempool {txid}: expected {expect:?}, still {state} after {NODE_ANSWER_WAIT:?} \
                     (a disabled mempool drops peer transactions without a word)"
                ),
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

pub async fn read_instrs(internal_handle: TFLServiceHandle, bytes: &[u8], instrs: &[TFInstr]) {
    // A failed deserialize is a hard error for a normal test but an expected input rejection
    // for the fuzzer; `uhh_option` decides which via TEST_ON_FAIL (PANIC vs recover).
    let on_fail = *TEST_ON_FAIL.lock().unwrap();
    for instr_i in 0..instrs.len() {
        // info!(
        //     "Loading instruction {}: {} ({})",
        //     instr_i,
        //     TFInstr::string_from_instr(bytes, &instrs[instr_i]),
        //     instrs[instr_i].kind
        // );

        let parsed = tf_read_instr(bytes, &instrs[instr_i]);
        if parsed.is_none() {
            #[allow(clippy::print_stderr)]
            {
                eprintln!(
                    "test format: instruction {instr_i} ({}, kind {}) does not parse",
                    TFInstr::str_from_kind(instrs[instr_i].kind),
                    instrs[instr_i].kind
                );
            }
        }
        if let Some(instr) = uhh_option(parsed, on_fail) {
            let height = match &instr {
                TestInstr::LoadPoW(block) => block.coinbase_height().map(|height| height.0),
                TestInstr::RecvPoW { block, .. } => Block::zcash_deserialize(&block[..])
                    .ok()
                    .and_then(|block| block.coinbase_height())
                    .map(|height| height.0),
                _ => None,
            };
            let failed_before = TEST_FAILED_INSTR_IDXS.lock().unwrap().len();
            *TEST_LAST_CHECK.lock().unwrap() = None;
            let start = std::time::Instant::now();

            handle_instr(
                &internal_handle,
                bytes,
                instr,
                instrs[instr_i].flags,
                instr_i,
            )
            .await;

            let outcome = TEST_LAST_CHECK.lock().unwrap().take();
            *TEST_PREV_OUTCOME.lock().unwrap() = outcome.clone();
            let failed = TEST_FAILED_INSTR_IDXS.lock().unwrap().len() > failed_before;
            let test = *TEST_NAME.lock().unwrap();
            crate::test_timing::record_instr(
                test,
                instr_i,
                TFInstr::str_from_kind(instrs[instr_i].kind),
                instrs[instr_i].flags & SHOULD_FAIL != 0,
                start,
                outcome,
                failed,
                height,
                instrs[instr_i].data_slice(bytes).len(),
            );
        } else {
            // An instruction that didn't parse (only reachable when the fuzzer recovers) made
            // no check, so an instruction about it must not see the one before it.
            *TEST_PREV_OUTCOME.lock().unwrap() = None;
        }

        *TEST_INSTR_C.lock().unwrap() = instr_i + 1; // accounts for end
    }
}

pub(crate) async fn instr_reader(internal_handle: TFLServiceHandle) {
    use zebra_chain::serialization::{ZcashDeserialize, ZcashSerialize};
    let call = internal_handle.call.clone();
    println!("waiting for tip before starting the test...");
    let before_time = Instant::now();
    loop {
        if let Ok(StateResponse::Tip(Some(_))) = (call.state)(StateRequest::Tip).await {
            break;
        } else {
            // warn!("Failed to read tip");
            if before_time.elapsed().as_secs() > 30 {
                panic!("Timeout waiting for test to start.");
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
    println!("Starting test!");

    if let Some(path) = TEST_INSTR_PATH.lock().unwrap().clone() {
        *TEST_INSTR_BYTES.lock().unwrap() = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) => panic!("Invalid test file: {:?}: {}", path, err), // TODO: specifics
        };
    }

    let bytes = TEST_INSTR_BYTES.lock().unwrap().clone();

    // Normal tests PANIC on an unparseable envelope; the fuzzer recovers (TEST_ON_FAIL).
    let on_fail = *TEST_ON_FAIL.lock().unwrap();
    let tf = match uhh(TF::read_from_bytes(&bytes), on_fail) {
        Ok(tf) => tf,
        Err(_) => return, // uhh already panicked (PANIC) or logged (fuzzer)
    };

    *TEST_INSTRS.lock().unwrap() = tf.instrs.clone();

    let params = internal_handle.params;
    read_instrs(internal_handle, &bytes, &tf.instrs).await;

    // Copy everything out and release every lock BEFORE asserting. The panic hook runs before
    // unwinding, while anything the failing statement holds is still held, and it calls
    // dump_test_instrs, which locks TEST_FAILED_INSTR_IDXS and TEST_INSTR_C; the shutdown path
    // below does the same. std Mutex is not reentrant, so a guard alive at either point deadlocks
    // this thread: a failing test hangs in the hook instead of aborting, a passing one at exit.
    let failed_instrs = TEST_FAILED_INSTR_IDXS.lock().unwrap().clone();
    let completed_instrs = *TEST_INSTR_C.lock().unwrap();
    let test = *TEST_NAME.lock().unwrap();

    // Before the asserts, so a failing test still gets its timing row.
    let passed = failed_instrs.is_empty() && completed_instrs == tf.instrs.len();
    crate::test_timing::record_test_end(test, &params, passed);

    assert_eq!(completed_instrs, tf.instrs.len(), "didn't complete test {test}");
    // The (instruction index, message) pairs make a red test self-describing.
    assert!(failed_instrs.is_empty(), "failed test {test}: {failed_instrs:?}");
    println!("Test done, shutting down");
    // #[cfg(feature = "viz_gui")]
    // tokio::time::sleep(Duration::from_secs(120)).await;

    TEST_SHUTDOWN_FN.lock().unwrap()();
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHORT: StakingParameters = StakingParameters { period: 10, day_window: 5, action_delay: 6 };

    #[test]
    fn set_params_round_trips_the_staking_calendar_and_node_config() {
        for bootstrap in [BftBootstrap::Supplied, BftBootstrap::FromChain { staking_height: 0, roster_height: 5, activation_height: 300 }] {
            for staking in [PROTOTYPE_STAKING, SHORT] {
                for node_config in ["", "[state]\nnetwork_checkpoint = \"10:00\"\n"] {
                    assert_eq!(
                        params_from_bytes(&params_to_bytes(bootstrap, staking, node_config), 1),
                        Some((bootstrap, staking, node_config.to_string()))
                    );
                }
            }
        }
    }

    /// A version 0 file ends after the bootstrap when its calendar was the prototype's, and is
    /// read under the prototype calendar of the build reading it. Version 1 always writes it.
    #[test]
    fn version_0_set_params_without_a_calendar_is_the_current_prototype_calendar() {
        assert_eq!(params_from_bytes(&[TF_BOOTSTRAP_SUPPLIED], 0), Some((BftBootstrap::Supplied, PROTOTYPE_STAKING, String::new())));
        assert_eq!(params_from_bytes(&[TF_BOOTSTRAP_SUPPLIED], 1), None);
        assert_eq!(params_to_bytes(BftBootstrap::Supplied, PROTOTYPE_STAKING, "").len(), 1 + 12);
    }

    #[test]
    fn short_staking_follows_sigma_and_keeps_its_margins() {
        // Harness sigma 3 and liveness allowance 3 give a period of 2 * (3 + 3 + 1) = 14.
        assert_eq!(short_calendar(&HARNESS_PARAMETERS), StakingParameters { period: 14, day_window: 3, action_delay: 4 });
        // The prototype's sigma of 4 gives 16.
        assert_eq!(short_calendar(&PROTOTYPE_PARAMETERS).period, 16);

        let staking = short_calendar(&HARNESS_PARAMETERS);
        // Room for a withdrawal late in a later window: the delay leaves the rest of the period.
        assert!(staking.action_delay <= staking.period - staking.day_window);
    }

    #[test]
    fn set_params_with_a_partial_calendar_is_rejected() {
        let mut bytes = params_to_bytes(BftBootstrap::Supplied, SHORT, "");
        bytes.pop();
        assert_eq!(params_from_bytes(&bytes, 1), None);
        assert_eq!(params_from_bytes(&bytes, 0), None);
    }
}
