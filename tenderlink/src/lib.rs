#![allow(missing_docs)]
#![allow(dead_code)]
#![allow(unused_parens)]
#![allow(clippy::never_loop)]

#![allow(clippy::eq_op)]
const PRINT_PROTOCOL:       bool = 1 == 1;
const PRINT_PROTOCOL_TAG:   bool = 0 == 1;
const PRINT_ROSTER:         bool = 0 == 1;
const PRINT_NETWORK_STATS:  bool = 1 == 1;
const PRINT_PEERS:          bool = 0 == 1;
const PRINT_VALID_INCOMING: bool = 0 == 1;
const PRINT_SENDS:          bool = 0 == 1;
const PRINT_SEND_CS:        bool = 0 == 1;
const PRINT_RNGS:           bool = 0 == 1;
const PRINT_SIGN:           bool = 0 == 1;
const PRINT_BLOCK_NEEDED:   bool = 0 == 1;
const PRINT_BFT_PROPOSAL:   bool = 0 == 1;
const PRINT_BFT_VOTE:       bool = 1 == 1;
const PRINT_BFT_UPDATE:     bool = 1 == 1;
const PRINT_BFT_STATE:      bool = 0 == 1;
const PRINT_BFT_CONDITIONS: bool = 1 == 1;
const PRINT_BFT_TIMEOUTS:   bool = 0 == 1;

#[cfg(debug_assertions)] pub fn dbg_break() {
    #[cfg(target_arch = "x86_64")] #[allow(unsafe_code)] unsafe { std::arch::asm!("int 3"); }
    // @Todo: AArch64 debugbreak.
}

#[cfg(debug_assertions)] #[track_caller] pub fn dbg_panic_internal(msg: std::fmt::Arguments<'_>) -> ! {
    dbg_break();
    unsafe { std::env::set_var("RUST_BACKTRACE", "full"); }
    panic!("{msg}");
}
#[macro_export] macro_rules! dbg_panic {
    ()            => { #[cfg(debug_assertions)] tenderlink::dbg_panic_internal(format_args!("explicit panic")); };
    ($($arg:tt)*) => { #[cfg(debug_assertions)] tenderlink::dbg_panic_internal(format_args!($($arg)*)); };
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
    #[cfg(debug_assertions)] if t.is_none() { dbg_break(); }

    t.unwrap()
}


const ANSI_GRY: &'static str = "\x1b[90m";
const ANSI_RED: &'static str = "\x1b[91m";
const ANSI_GRN: &'static str = "\x1b[92m";
const ANSI_YLW: &'static str = "\x1b[93m";
const ANSI_BLU: &'static str = "\x1b[34m";
const ANSI_RST: &'static str = "\x1b[0m";

// @Todo: MTU discovery // @Duplicate with NewNet.
const UDP_mMTU:        usize = 1400; // Note(Sam): This number informs cryptography. BAD! For season one we must now not change this number. Even if it means sending jumbos to compensate. :(
const STP_HEADER_SIZE: usize = total_packet_payload_overhead_from_connect_magic1_inside_udp_payload(CRYPTO_MAGIC).unwrap();
const STP_PACKLET_HDR: usize = std::mem::size_of::<crate::bandwidth_test::PackletHeader>();
const PATH_MTU: usize = UDP_mMTU
                      - STP_HEADER_SIZE
                      - STP_PACKLET_HDR;

// Tweak this!
const MAX_BANDWIDTH_BYTES_PER_SECOND: usize = 1_000_000;


pub const CRYPTO_MAGIC: u64 = CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2s;
// pub const CRYPTO_MAGIC: u64 = CONNECT_MAGIC1_PLAIN_TEXT;


use static_assertions::const_assert;
use std::{hash::DefaultHasher, net::{Ipv6Addr, SocketAddr, SocketAddrV6}, sync::{Arc, Mutex}};
use ed25519_zebra::{SigningKey, VerificationKeyBytes, VerificationKey};
use rand::{seq::{IndexedRandom}, Rng, RngCore, SeedableRng};
use rand_chacha::ChaCha20Rng;
use rand_pcg::Lcg128CmDxsm64 as SimRng;
use snow::resolvers::CryptoResolver;
use tokio::time::Instant;
use zcash_primitives::bft::{ HashKey, HashKeys, FatPointerToBftBlock, TMSig, PubKeyID, FatPointerSignature, BftBlockAndFatPointerToIt, BftBlock };

const TICK_DURATION:         std::time::Duration = std::time::Duration::from_millis(1000);
const PEER_GOSSIP_DURATION:  std::time::Duration = std::time::Duration::from_millis(1500);
const PEER_CONNECT_DURATION: std::time::Duration = std::time::Duration::from_millis(5000);


// NOTE: Sam and Phillip discussed forward jumps; Noise trial decryption already protects connectsions against replay attacks.
const NONCE_FORWARD_JUMP_TOLERANCE: u64 = 512;

fn is_timeout(e: std::io::ErrorKind) -> bool{
    e == std::io::ErrorKind::WouldBlock || e == std::io::ErrorKind::TimedOut
}

#[derive(Default)]
pub struct NetworkStats {
    bytes_sent: usize,
    packets_sent: usize,
}

#[derive(Clone, Debug)]
pub struct SortedRosterMember {
    pub pub_key: PubKeyID,
    pub stake: u64,
    pub cumulative_stake: u64, // everyone in array prior to this point (used for determining proposer)
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub enum TMStep {
    Propose,
    Prevote,
    // ALT: extra sign step
    Precommit,
}

#[derive(Debug)]
pub struct TMDecision {
    round_i: usize,
    value: BlockValue,
    //signatures: Vec<TMSig>, // ability to prove to others e.g. those catching up
}

pub struct TMVote {
    approve: bool,
    todo_sign_bytes: [u8; 96],
}

#[derive(Clone, PartialEq, Debug)]
pub struct BlockValue(pub Vec<u8>); // NOTE (azmr): currently exactly-divided by chunk size for simplicity
impl BlockValue {
    fn id_from_value(&self, hash_keys: &HashKeys) -> ValueId { ValueId(hash_keys.value_id.hash(&self.0)) }
    fn chunks_n(&self) -> usize { self.0.len().div_ceil(PROPOSAL_CHUNK_DATA_SIZE) }
    fn chunk_o_size(&self, chunk_i: usize) -> (usize, usize) {
        let o = chunk_i * PROPOSAL_CHUNK_DATA_SIZE;
        (o, usize::min(PROPOSAL_CHUNK_DATA_SIZE, self.0.len() - o))
    }
}

#[derive(Clone)]
pub struct ClosureToProposeNewBlock(pub Arc<dyn Fn() -> core::pin::Pin<Box<dyn Future<Output = Option<BlockValue>> + Send>> + Send + Sync + 'static>);
impl std::fmt::Debug for ClosureToProposeNewBlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("ClosureToProposeNewBlock(..)") }
}
#[derive(Clone)]
pub struct ClosureToValidateProposedBlock(pub Arc<dyn for<'a> Fn(&'a BlockValue)-> core::pin::Pin<Box<dyn Future<Output = (TMStatus, TMStatusReason)> + Send + 'a>> + Send + Sync + 'static>);
impl std::fmt::Debug for ClosureToValidateProposedBlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("ClosureToValidateProposedBlock(..)") }
}
#[derive(Clone)]
pub struct ClosureToPushDecidedBlock(pub Arc<dyn Fn(BlockValue, FatPointerToBftBlock, Vec<TMSig>)-> core::pin::Pin<Box<dyn Future<Output = Vec<SortedRosterMember>> + Send>> + Send + Sync + 'static>);
impl std::fmt::Debug for ClosureToPushDecidedBlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("ClosureToPushDecidedBlock(..)") }
}
#[derive(Clone)]
pub struct ClosureToUpdatePeers(pub Arc<dyn Fn(Vec<PeerInfo>) -> core::pin::Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync + 'static>);
impl std::fmt::Debug for ClosureToUpdatePeers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("ClosureToUpdatePeers(..)") }
}
#[derive(Clone)]
pub struct ClosureToAllowBftAccess(pub Arc<dyn for<'a> Fn(&'a TMState, &'a BftAddressMap) -> core::pin::Pin<Box<dyn Future<Output = ()> + Send + 'a>> + Send + Sync + 'static>);
impl std::fmt::Debug for ClosureToAllowBftAccess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("ClosureToAllowBftAccess(..)") }
}

fn round_data_to_fat_pointer(round_data: &RoundData, roster: &[SortedRosterMember]) -> FatPointerToBftBlock {
    let vote_for_block_without_finalizer_public_key: [u8; 76 - 32];
    {
        let mut sign_data = [0; 76 - 32];
        round_data.proposal_id.0.write_to(&mut sign_data[0..32]);
        round_data.height.write_to(&mut sign_data[32..]);
        (round_data.round + 0x8000_0000).write_to(&mut sign_data[40..]);
        vote_for_block_without_finalizer_public_key = sign_data;
    }

    FatPointerToBftBlock {
        vote_for_block_without_finalizer_public_key,
        signatures: round_data.msg_val_sigs
            .iter()
            .map(|x| &x[1])
            .enumerate()
            .filter_map(|(roster_i, (value_id, commit_signature))| {
                if *value_id == round_data.proposal_id && *commit_signature != TMSig::NIL {
                    Some(FatPointerSignature {
                        pub_key: roster[roster_i].pub_key,
                        vote_signature: commit_signature.0,
                    })
                } else { None }
            })
            .collect(),
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum TMStatus {
    Indeterminate,
    Pass, // 2f+1 yes
    Fail, // f+1 no
}

#[derive(Copy, Clone, PartialEq, Eq, Debug, Default)]
pub enum TMStatusReason {
    #[default] None,
    NeedsBlock {
        hash: [u8; 32]
    },
}

#[derive(Default, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ValueId(pub [u8; 32]);
impl ValueId { pub const NIL: Self = Self([0; 32]); }
impl std::fmt::Display for ValueId { fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { fmt_byte_str(f, &self.0) } }
impl std::fmt::Debug   for ValueId { fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { fmt_prefixed_byte_str(f, "VId{", &self.0)?; write!(f, "}}") } }

#[derive(Debug, Clone)]
pub struct RoundData {
    pub height: u64,
    pub round: u32,
    // parallel with sorted roster arrays
    // TODO: keep parallel with each other, but be sparse in members
    pub proposal: BlockValue,
    pub proposal_valid_round: i64,
    pub proposal_sigs: Vec<TMSig>,
    pub proposal_sigs_n: usize, // filling sigs with random-access
    pub proposal_id: ValueId,
    pub proposal_checked_validity: (TMStatus, TMStatusReason),
    // TODO: handle early outs because of this
    pub proposal_is_faulty: bool,

    // TODO: we may be able to compress valueid, but we do need to track it before we have the proposal
    pub msg_val_sigs: Vec<[(ValueId, TMSig); 2]>, // prevote then precommit
    pub roster: Vec<SortedRosterMember>,

    pub counts: ConsensusCounts,
    // TODO: can probably do this from whether *our* node has a valid value
    // TODO: by round or for whole state?
    pub active_timeout: Option<Timeout>,
    pub timeout_triggered: [bool; 2],
}
impl RoundData {
    pub const EMPTY: RoundData = RoundData {
        height: 0,
        round: 0,
        proposal: BlockValue(Vec::new()), // NOTE(azmr): don't alloc until we know the size (signed by proposer)
        proposal_valid_round: -1,
        proposal_sigs: Vec::new(),
        proposal_sigs_n: 0,
        proposal_id: ValueId::NIL,
        proposal_checked_validity: (TMStatus::Indeterminate, TMStatusReason::None),
        proposal_is_faulty: false,
        // TODO: probably put both step messages next to each other
        msg_val_sigs: Vec::new(),
        roster: Vec::new(),
        counts: ConsensusCounts::ZERO,

        active_timeout: None,
        timeout_triggered: [false;2],
    };

    fn has_full_proposal(&self) -> bool {
        self.proposal_sigs_n > 0 && self.proposal_sigs_n == self.proposal_sigs.len()
    }

    fn flush_for_amnesiac_proposer(&mut self, value_id: ValueId, valid_round: i64, roster: &[SortedRosterMember], proposal_size: u32) {
        let roster_n = active_roster_len(roster);
        *self = RoundData {
            height:               self.height,
            round:                self.round,
            proposal_id:          value_id,
            proposal_valid_round: valid_round,
            msg_val_sigs:         vec![[(ValueId::NIL, TMSig::NIL); 2]; roster_n],
            roster:               Vec::from(&roster[0..roster_n]),
            active_timeout:       self.active_timeout.clone(),
            timeout_triggered:    self.timeout_triggered,
            ..RoundData::EMPTY
        };
        self.proposal.0    = vec![0;          proposal_size as usize];
        self.proposal_sigs = vec![TMSig::NIL; self.proposal.chunks_n()];
    }
    fn has_enough_info_to_determine_validity(&self) -> bool {
        self.proposal_is_faulty || self.has_full_proposal()
    }
    // auto-caching
    async fn proposal_is_valid(&mut self, validate_closure: ClosureToValidateProposedBlock) -> TMStatus {
        // TODO: may want to start doing some of these on < proposal_chunks_n, i.e. shortcut known-invalid
        if self.proposal_checked_validity.0 == TMStatus::Indeterminate {
            if self.proposal_is_faulty {
                self.proposal_checked_validity = (TMStatus::Fail, TMStatusReason::None);
            } else if self.has_full_proposal() {
                self.proposal_checked_validity = validate_closure.0(&self.proposal).await;
            }
        }
        self.proposal_checked_validity.0
    }
}

enum TMMsgData {
    Proposal(BlockValue, i64),
    Prevote(ValueId),
    Precommit(ValueId),
}
pub struct TMMsg {
    height: u64,
    round: u32,
    data: TMMsgData, // ALT: byteslice + step distinguisher
    sig: TMSig,
}

#[derive(Clone, Copy, PartialEq)]
pub struct ConsensusCounts {
    pub anys: u64,
    pub prevotes: u64,
    pub nil_prevotes: u64,
    pub yes_prevotes: u64,
    pub precommits: u64,
    pub yes_precommits: u64,
}
impl ConsensusCounts {
    const ZERO: Self = Self {
        anys: 0,
        prevotes: 0,
        precommits: 0,
        yes_prevotes: 0,
        yes_precommits: 0,
        nil_prevotes: 0,
    };
}
impl std::ops::Add for ConsensusCounts {
    type Output = Self;
    fn add(self, rhs: ConsensusCounts) -> ConsensusCounts {
        ConsensusCounts {
            anys:           self.anys           + rhs.anys,
            prevotes:       self.prevotes       + rhs.prevotes,
            nil_prevotes:   self.nil_prevotes   + rhs.nil_prevotes,
            yes_prevotes:   self.yes_prevotes   + rhs.yes_prevotes,
            precommits:     self.precommits     + rhs.precommits,
            yes_precommits: self.yes_precommits + rhs.yes_precommits,
        }
    }
}
impl std::ops::Sub for ConsensusCounts {
    type Output = Self;
    fn sub(self, rhs: ConsensusCounts) -> ConsensusCounts {
        ConsensusCounts {
            anys:           self.anys           - rhs.anys,
            prevotes:       self.prevotes       - rhs.prevotes,
            nil_prevotes:   self.nil_prevotes   - rhs.nil_prevotes,
            yes_prevotes:   self.yes_prevotes   - rhs.yes_prevotes,
            precommits:     self.precommits     - rhs.precommits,
            yes_precommits: self.yes_precommits - rhs.yes_precommits,
        }
    }
}
impl From<&([(ValueId, TMSig); 2], u64)> for ConsensusCounts {
    fn from(val: &([(ValueId, TMSig); 2], u64)) -> ConsensusCounts {
        let (val, stake) = val;
        let has_sigs     = [(val[0].1 != TMSig::NIL) as usize, (val[1].1 != TMSig::NIL) as usize];
        let has_any_sigs = has_sigs[0] | has_sigs[1]; // TODO: confirm prevote + precommit from the same person counts as 1

        let mut status = [[0,0], [0,0]];
        status[0][(val[0].0 != ValueId::NIL) as usize] = has_sigs[0];
        status[1][(val[1].0 != ValueId::NIL) as usize] = has_sigs[1];

        ConsensusCounts {
            anys: has_any_sigs as u64 * stake,
            prevotes: has_sigs[0] as u64 * stake,
            nil_prevotes: status[0][0] as u64 * stake,
            yes_prevotes: status[0][1] as u64 * stake,
            precommits: has_sigs[1] as u64 * stake,
            yes_precommits: status[1][1] as u64 * stake,
        }
    }
}
impl std::fmt::Debug for ConsensusCounts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Counts {{ a:{}  v:{} (nv:{} yv:{})  c:{} (yc:{}) }}",
            self.anys,
            self.prevotes,
            self.nil_prevotes,
            self.yes_prevotes,
            self.precommits,
            self.yes_precommits,
        )
    }
}


fn roster_i_from_pub_key(roster: &[SortedRosterMember], pub_key: PubKeyID) -> Option<usize> {
    roster.iter().position(|m| m.pub_key == pub_key)
}

#[derive(Debug, Clone)]
pub struct Timeout { time: Instant, height: u64, round: u32, step: TMStep }
impl Timeout {
    fn new(now: Instant, height: u64, round: u32, step: TMStep) -> Timeout {
        use std::time::Duration;
        let timeout = match step {
            // Note(Sam): These timeout should be tuned to match the maximum network load block time. An additional
            // virtue of a short block time that I had not considered is that it hides round stalls better.
            TMStep::Propose   => Duration::from_millis(5000) + round * Duration::from_millis(1000),
            TMStep::Prevote   => Duration::from_millis(5000) + round * Duration::from_millis(1000),
            TMStep::Precommit => Duration::from_millis(5000) + round * Duration::from_millis(1000),
        };

        Timeout{ time: now + timeout, height, round, step }
    }
}


const ROSTER_MAX_N: usize = 100;
fn active_roster_len(roster: &[SortedRosterMember]) -> usize { usize::min(ROSTER_MAX_N, roster.len()) }
fn total_roster_len(roster: &[SortedRosterMember])  -> usize { roster.len() }



#[derive(Debug)]
pub struct TMState {
    pub hash_keys: HashKeys,
    pub my_port: u16,
    pub my_signing_key: SigningKey,
    pub my_pub_key: PubKeyID,
    pub round: u32,
    pub step: TMStep,
    /// basically the chain of agreed blocks
    pub height: u64,
    /// most recent "possible decision value" - successful proposal + prevote
    /// when valid_value was updated
    pub valid_value_round: (Option<BlockValue>, i64), // TODO
    /// last value sent for precommit // TODO: non-nil only?
    /// last round on which a *non-nil* value was sent
    pub locked_value_round: (Option<BlockValue>, i64), // TODO

    pub rounds_data: Vec<RoundData>,

    pub recent_commit_round_cache: Vec<RoundData>, // for now will hold all completed heights

    propose_closure: ClosureToProposeNewBlock,
    validate_closure: ClosureToValidateProposedBlock,
    push_block_closure: ClosureToPushDecidedBlock,

    update_peers_cmd_closure: ClosureToUpdatePeers,
    bft_access_closure: ClosureToAllowBftAccess,
}
impl TMState {
    fn init(
        my_signing_key: SigningKey, my_pub_key: PubKeyID, my_port: u16,
        propose_closure: ClosureToProposeNewBlock,
        validate_closure: ClosureToValidateProposedBlock,
        push_block_closure: ClosureToPushDecidedBlock,
        update_peers_cmd_closure: ClosureToUpdatePeers,
        bft_access_closure: ClosureToAllowBftAccess,
    ) -> Self {
        Self {
            hash_keys: HashKeys::default(),
            my_port,
            my_signing_key,
            my_pub_key,
            round: 0,
            step: TMStep::Propose,
            height: 0,
            valid_value_round: (None, -1), // TODO: is this actually protocol-relevant or just a cache?
            locked_value_round: (None, -1),

            rounds_data: Vec::new(),
            recent_commit_round_cache: Vec::new(),

            propose_closure,
            validate_closure,
            push_block_closure,
            update_peers_cmd_closure,
            bft_access_closure,
        }
    }

    // NOTE: we just add our info to our round data & have it become equivalent to everyone else's...
    fn broadcast(&mut self, roster: &[SortedRosterMember], round_i: usize, msg: TMMsgData) -> TMStep {
        // TODO: can we get away with not signing the step or separately signing the step?
        let mut buf = [0u8; 2048];
        // TODO: send to self
        // TODO: send to (some) others
        let height   = self.rounds_data[round_i].height;
        let round    = self.rounds_data[round_i].round;
        let ctx_str = self.ctx_str(roster);
        let Some(roster_i) = roster_i_from_pub_key(&roster[..active_roster_len(roster)], self.my_pub_key) else {
            eprintln!("{ctx_str} {ANSI_RED}BFT ERROR{ANSI_RST}: failed to find my own public key in the roster");
            return self.step;
        };
        match msg {
            TMMsgData::Proposal(proposal, valid_round) => {
                let mut hdr = PacketProposalChunkHeader {
                    height, round, chunk_i: 0,
                    proposal_size: proposal.0.len().try_into().unwrap(),
                    proposal_id: /*if proposal.0[1] % 5 == 0 { ValueId([6;32]) } else*/ { proposal.id_from_value(&self.hash_keys) },
                    valid_round,
                };

                for chunk_i in 0..proposal.chunks_n() { // NOTE: excluding packet_type // TODO: check this
                    hdr.chunk_i = chunk_i as u32;
                    let mut o = 0;
                    o += hdr.write_to(&mut buf[0..]);

                    let (chunk_o, chunk_size) = proposal.chunk_o_size(chunk_i);
                    o += proposal.0[chunk_o..chunk_o + chunk_size].write_to(&mut buf[o..]);

                    // NOTE: we *DON'T* want to write it immediately to our proper store because it
                    // will confuse check_and_incorporate_msg
                    let sig = TMSig(self.my_signing_key.sign(&buf[..o]).to_bytes());
                    if PRINT_SIGN { println!("{ctx_str} {ANSI_GRY}SIGN{ANSI_RST}: signed proposal with {:?}", sig) };

                    // NOTE: we're faulty if we give our pub key for this if it's not our proposal
                    self.check_and_incorporate_msg(
                        height, round, chunk_i, hdr.proposal_id, hdr.valid_round,
                        roster, roster_i, PACKET_TYPE_PROPOSAL_CHUNK, &buf[..o], sig
                    );
                }

                TMStep::Propose
            }

            TMMsgData::Prevote(value_id) | TMMsgData::Precommit(value_id) => {
                let is_precommit: u8 = if let TMMsgData::Precommit(..) = msg { 1 } else { 0 };
                if PRINT_BFT_VOTE { println!("{ctx_str} {ANSI_GRY}BFT_VOTE{ANSI_RST}: {} on {}", ["prevoting", "precommitting"][is_precommit as usize], value_id); }
                let packet_type = PACKET_TYPE_PREVOTE_SIGNATURES + is_precommit;
                let signed_data = make_vote_sign_datas(roster[roster_i].pub_key, is_precommit != 0, height, round, value_id)[1];
                let sig         = TMSig(self.my_signing_key.sign(&signed_data).to_bytes());
                if PRINT_SIGN { println!("{ctx_str} {ANSI_GRY}SIGN{ANSI_RST}: signed {} with {:?}", ["prevote", "precommit"][is_precommit as usize], sig) };

                self.check_and_incorporate_msg(
                    height, round, 0, value_id, -2,
                    roster, roster_i, packet_type, &signed_data, sig
                );

                [TMStep::Prevote, TMStep::Precommit][is_precommit as usize]
            },
        }
    }

    /// Deterministic weighted round robin (hash & mod total zec on cumulative list)
    fn proposer_from_height_round(hash_keys: &HashKeys, roster: &[SortedRosterMember], height: u64, round: u32) -> (Option<usize>, PubKeyID) {
        if roster.len() == 0 {
            eprintln!("{ANSI_RED}BFT ERROR{ANSI_RST}: trying to get proposer from empty roster");
            return (None, PubKeyID::NIL); // TODO: is a fixed value here exploitable? Presumably nobody can sign for it?
        }

        // NOTE(azmr): this 32-byte crypto-hashing is almost certainly overkill!
        let hash = hash_keys.proposer.hasher().update(&u64::to_le_bytes(height)).update(&u32::to_le_bytes(round)).finalize();

        let mut hash_stake_bytes = [0; 8];
        hash.as_bytes()[..8].write_to(&mut hash_stake_bytes);
        let hash_stake = u64::from_le_bytes(hash_stake_bytes);

        let last_included_i = active_roster_len(roster) - 1;
        let total_included_stake = roster[last_included_i].cumulative_stake;
        if total_included_stake == 0 {
            eprintln!("{ANSI_RED}BFT ERROR{ANSI_RST}: all roster members have no stake");
            return (None, PubKeyID::NIL); // TODO: is a fixed value here exploitable? Presumably nobody can sign for it?
        }


        let proposer_stake = hash_stake % total_included_stake;

        let roster_i = roster.partition_point(|m| m.cumulative_stake <= proposer_stake);
        // println!("proposer stake hash: {} ==u64=> {:016x} ==%{}=> {} ==i=> {}", hash, hash_stake, total_included_stake, proposer_stake, roster_i);
        (Some(roster_i), roster[roster_i].pub_key)
    }

    fn insert_round(&mut self, insert_i: usize, round: u32, roster: &[SortedRosterMember]) -> usize {
        let roster_n = active_roster_len(roster);
        self.rounds_data.insert(insert_i, RoundData {
            height: self.height,
            round,
            msg_val_sigs: vec![[(ValueId::NIL, TMSig::NIL); 2]; roster_n], // TODO: just use ROSTER_MAX_N?
            roster: roster.to_vec(),
            ..RoundData::EMPTY
        });
        insert_i
    }

    async fn start_round(&mut self, roster: &[SortedRosterMember], now: Instant, round: u32) {
        self.round = round;
        // self.active_proposal_value_round = (None, -1);

        let round_i = match self.rounds_data.binary_search_by_key(&(self.height, round), |el| (el.height, el.round)) {
            Ok(round_i)  => round_i,
            Err(round_i) => self.insert_round(round_i, round, roster)
        };

        if Self::proposer_from_height_round(&self.hash_keys, roster, self.height, round).1 == self.my_pub_key {
            let ctx_str = self.ctx_str(roster);
            let proposal = if let Some(valid_value) = self.valid_value_round.0.clone() {
                Some(valid_value)
            } else {
                let ret = self.propose_closure.0().await;
                if PRINT_BFT_PROPOSAL { if ret.is_none() { println!("{ctx_str} {ANSI_GRY}BFT_PROPOSAL{ANSI_RST}: propose closure returned None."); } }
                ret
            };
            if PRINT_BFT_PROPOSAL { if let Some(proposal) = &proposal { println!("{ctx_str} {ANSI_GRY}BFT_PROPOSAL{ANSI_RST}: about to propose with status '{:?}': {:?}", self.validate_closure.0(&proposal).await, proposal); } }

            // TODO: simple approach: send proposal messages to self when broadcasting
            // self.active_proposal_value_round = (Some(proposal), self.valid_value_round.1);
            if let Some(proposal) = proposal {
                self.step = self.broadcast(roster, round_i, TMMsgData::Proposal(proposal, self.valid_value_round.1));
            } else {
                self.step = TMStep::Propose;
            }
        } else {
            self.step = TMStep::Propose;
        }
        self.rounds_data[round_i].active_timeout = Some(Timeout::new(now, self.height, self.round, TMStep::Propose));
    }

    fn f_from_n(n: u64) -> u64 {
        (n - 1) / 3
    }

    fn check_and_incorporate_msg(&mut self, height: u64, round: u32, chunk_i: usize, value_id: ValueId, valid_round: i64, roster: &[SortedRosterMember], roster_i: usize, packet_type: u8, signed_data: &[u8], sig: TMSig) -> TMStatus {
        let ctx_str  = self.ctx_str(roster);
        let pkt_str = format!("{:20} {}.{}.{}", packet_name_from_tag(packet_type), height, round, chunk_i);

        if height != self.height {
            // eprintln!("{ctx_str} {ANSI_GRY}BFT{ANSI_RST}: received [{}] when we're at height {}", pkt_str, self.height);
            return TMStatus::Fail;
        }

        // check if in (active) roster
        if roster_i >= active_roster_len(roster) {
            eprintln!("{ctx_str} ({}): {ANSI_RED}BFT FAULT{ANSI_RST}: {} is not in the active roster.", pkt_str, roster_i);
            return TMStatus::Fail;
        }

        let from_pub_key = roster[roster_i].pub_key;

        // pkt_str += &format!(" from {} ({})", roster_i, from_pub_key);
        let ctx_str = format!("{ctx_str} [{} from {} {:?}]", pkt_str, roster_i, from_pub_key);

        // check if data was signed by pub key
        match sig.verify(from_pub_key, signed_data) { Ok(())=>{}, Err((err, str))=> {
            eprintln!("{ctx_str} {ANSI_RED}BFT FAULT{ANSI_RST}: {} (..{}): for {} {}", str, signed_data.len(), value_id, err);
            #[cfg(debug_assertions)]
            {
                println!("DEBUG LOOP OVER ALL ROSTER AND TRIAL VERIFY only success should be {} but that is not so... :(", roster_i);
                for i in 0..roster.len() {
                    println!("{}: success={} pub_key: {:?} stake: {} cumulative_stake: {}", i, sig.verify(roster[i].pub_key, signed_data).is_ok(), roster[i].pub_key, roster[i].stake, roster[i].cumulative_stake);
                }
            }
            return TMStatus::Fail;
        }};

        if PRINT_VALID_INCOMING { eprintln!("{ctx_str} {ANSI_GRY}VALID_INCOMING{ANSI_RST}: valid signature for value id: {}", value_id); }

        // TODO: other checks
        // - data size check if we're doing network stuff

        let (is_prev_seen_round, round_i) = match self.rounds_data.binary_search_by_key(&(height, round), |el| (el.height, el.round)) {
            Ok(round_i)  => (true,  round_i),
            Err(round_i) => (false, self.insert_round(round_i, round, roster)),
        };
        let round_data = &mut self.rounds_data[round_i];

        // TODO: Keep a dynamic array to solve the "Amnesiac Proposer's Dilemma".
        //       If I propose my block for this height and round, but then my
        //       computer gets unplugged, I've forgotten block history and need to
        //       catch back up to the network's consensus height. However, on the
        //       way there, I'll sometimes propose new values. (This is expected,
        //       arguably, since we never truly know whether we're at the top of
        //       the consensus height.) However, in the process of catching up I
        //       may get my own *different* signed proposal that was already decided!
        //       Normally we would mark that proposer as faulty/adversarial, but we
        //       assume that we are never faulty, and must therefore be "amnesic".
        //       In Byzantine scenarios I may have been unplugged arbitrarily
        //       many times and be receiving arbitrarily many validly signed
        //       proposals of my own making, with even some signed precommits.
        //       We'll observe multiple competing proposals, all signed by us, all
        //       equally valid candidates for the decisive proposal, any of which
        //       may establish consensus. We won't know until we see 2f+1 precommits.
        //       We need to let these precommits *race*, until we observe 2f+1
        //       stake being precommitted to *any* of our proposals, at which
        //       point we accept *that* proposal as decisive. (There will never
        //       be multiple proposals with 2f+1 precommits unless the BFT
        //       network is faulty; we just need to wait and see which one
        //       is the decisive one.)  -Phil 2025-10-20
        // NOTE: @Incomplete: for now, only track the latest proposal.
        let is_my_proposal = (from_pub_key == self.my_pub_key);

        match packet_type {
            PACKET_TYPE_PROPOSAL_CHUNK => {
                let Some(hdr) = PacketProposalChunkHeader::read_from(&mut &signed_data[..])
                else { return TMStatus::Fail; };

                // "have they previously proposed a different value?"
                if is_prev_seen_round && round_data.proposal_sigs_n > 0 {
                    if is_my_proposal &&
                      (round_data.proposal.0.len() != hdr.proposal_size as usize ||
                       round_data.proposal_id != value_id ||
                       round_data.proposal_valid_round != valid_round) { // Amnesiac Proposer's Dilemma
                        round_data.flush_for_amnesiac_proposer(value_id, valid_round, roster, hdr.proposal_size);
                        eprintln!("{ctx_str} {ANSI_YLW}AMNESIAC PROPOSER{ANSI_RST} at {}.{}.{}: Flushing proposal...", height, round, chunk_i);
                    } else {
                        if round_data.proposal.0.len() != hdr.proposal_size as usize {
                            eprintln!("{ctx_str} {ANSI_RED}BFT FAULT{ANSI_RST} at {}.{}.{}: proposer {} proposed 2 different-size values ({:?}, {:?}). Ignoring latest...",
                            height, round, chunk_i, roster_i, round_data.proposal.0.len(), hdr.proposal_size);
                            return TMStatus::Fail;
                        }
                        if round_data.proposal_id != value_id {
                            // TODO: immediately class both as invalid?
                            eprintln!("{ctx_str} {ANSI_RED}BFT FAULT{ANSI_RST} at {}.{}.{}: proposer {} proposed 2 different values ({:?}, {:?}). Ignoring latest...",
                            height, round, chunk_i, roster_i, round_data.proposal_id, value_id);
                            return TMStatus::Fail;
                        }
                        if round_data.proposal_valid_round != valid_round {
                            // TODO: immediately class both as invalid
                            eprintln!("{ctx_str} {ANSI_RED}BFT FAULT{ANSI_RST} at {}.{}.{}: proposer {} proposed 2 different valid rounds ({}, {}). Ignoring latest...",
                            height, round, chunk_i, roster_i, round_data.proposal_valid_round, valid_round);
                            return TMStatus::Fail;
                        }
                    }
                } else {
                    round_data.proposal.0    = vec![0;          hdr.proposal_size as usize];
                    round_data.proposal_sigs = vec![TMSig::NIL; round_data.proposal.chunks_n()];
                }

                // Preliminary checks now finished (although not infallible from here) //////////////////////////

                // TODO: check expected proposer here if not above
                let (chunk_o, chunk_size) = round_data.proposal.chunk_o_size(chunk_i);
                let packet_chunk_o        = PacketProposalChunkHeader::SERIALIZED_SIZE;
                let chunk_data            = &signed_data[packet_chunk_o..packet_chunk_o + chunk_size];

                if round_data.proposal_sigs[chunk_i] == TMSig::NIL { // value chunk not seen before
                    chunk_data.write_to(&mut round_data.proposal.0[chunk_o..chunk_o+chunk_size]);
                    round_data.proposal_sigs[chunk_i] = sig;
                    round_data.proposal_sigs_n       += 1;
                    round_data.proposal_valid_round   = valid_round;
                    if round_data.proposal_id == ValueId::NIL { // first time we've seen any proposal chunks
                        round_data.proposal_id = value_id;

                        let mut prev_sig_had_fault = false;
                        // check whether speculative adds to round data were for the actual proposal
                        for roster_i in 0..round_data.msg_val_sigs.len() {
                            let msg_val: &mut [(ValueId, TMSig); 2] = &mut round_data.msg_val_sigs[roster_i];
                            for is_precommit in 0..2 {
                                if msg_val[is_precommit].0 != ValueId::NIL && msg_val[is_precommit].0 != value_id {
                                    prev_sig_had_fault = true;
                                    eprintln!("{ctx_str} {ANSI_RED}BFT FAULT{ANSI_RST} at {}.{}: finalizer {} {} on non-proposed value {}. Ignoring...", height, round, roster_i, ["prevoted","precommitted"][is_precommit], value_id);
                                    msg_val[is_precommit] = (ValueId::NIL, TMSig::NIL);
                                }
                            }
                        }

                        if prev_sig_had_fault { // recompute from scratch
                            // NOTE: this does NOT imply the current packet/proposal is faulty, so we should continue with it
                            let mut check_counts = ConsensusCounts::ZERO;
                            for (roster_i, sig) in round_data.msg_val_sigs.iter().enumerate() {
                                check_counts = check_counts + ConsensusCounts::from(&(*sig, roster[roster_i].stake));
                            }
                            round_data.counts = check_counts;
                        }
                    }

                    if round_data.proposal_sigs_n == round_data.proposal_sigs.len() {
                        let check_value_id = round_data.proposal.id_from_value(&self.hash_keys);
                        if round_data.proposal_id != check_value_id {
                            round_data.proposal_is_faulty = true;
                            eprintln!("{ctx_str} {ANSI_RED}BFT FAULT{ANSI_RST}: proposer's value_id does not match our calculation: {} != {}",
                                round_data.proposal_id, check_value_id);
                            return TMStatus::Fail;
                        }
                    }

                    if PRINT_BFT_UPDATE { println!("{ctx_str} {ANSI_GRY}BFT_UPDATE{ANSI_RST}: update to {}/{} proposal chunks on {}", round_data.proposal_sigs_n, round_data.proposal_sigs.len(), round_data.proposal_id); }

                    // TODO: include signed prevote & precommit for self?
                } else if round_data.proposal_sigs[chunk_i] != sig { // TODO: check value/sig conformance
                    if is_my_proposal { // Amnesiac Proposer's Dilemma
                        round_data.flush_for_amnesiac_proposer(value_id, valid_round, roster, hdr.proposal_size);
                        eprintln!("{ctx_str} {ANSI_YLW}AMNESIAC PROPOSER{ANSI_RST} at {}.{}.{}: Flushing proposal...", height, round, chunk_i);
                    } else {
                        // TODO: treat this as a failed is_valid & early out before awaiting full proposal
                        round_data.proposal_is_faulty = true;
                        eprintln!("{ctx_str} {ANSI_RED}BFT FAULT{ANSI_RST}: proposer signed 2 different values. Ignoring latest...");
                        return TMStatus::Fail;
                    }
                } else {
                    return TMStatus::Pass; // already good
                }

                TMStatus::Pass
            }


            PACKET_TYPE_PREVOTE_SIGNATURES | PACKET_TYPE_PRECOMMIT_SIGNATURES => {
                // TODO: check if this person has previously voted differently; is this covered later?
                let is_precommit = (packet_type - PACKET_TYPE_PREVOTE_SIGNATURES) as usize;

                let status = if value_id == ValueId::NIL { // always legal (except for duplicate checked later)
                    TMStatus::Pass
                } else if round_data.proposal_sigs_n == 0 {
                    // if we don't have a real proposal yet we can't check for validity
                    TMStatus::Indeterminate
                } else if round_data.proposal_id != value_id {
                    eprintln!("{ctx_str} {ANSI_RED}BFT FAULT{ANSI_RST} at {}.{}: finalizer {} voted on non-proposed value {}. Ignoring...", height, round, roster_i, value_id);
                    return TMStatus::Fail;
                } else {
                    TMStatus::Pass
                };

                // TODO: check if specified valid_round had a different value_id

                let old_val_sig = round_data.msg_val_sigs[roster_i][is_precommit];
                let new_val_sig = (value_id, sig);
                if old_val_sig.1 != TMSig::NIL && new_val_sig != old_val_sig {
                    // TODO: do we want to allow for NIL updating to valid?
                    eprintln!("{ctx_str} {ANSI_RED}BFT FAULT{ANSI_RST} at {}.{}: finalizer {} voted on 2 different values ({:?}, {:?}). Ignoring latest...", height, round, roster_i, new_val_sig, old_val_sig);
                    return TMStatus::Fail;
                }
                // Checks now finished //////////////////////////

                // Add the signature to the list & update counts
                let old_cs = ConsensusCounts::from(&(round_data.msg_val_sigs[roster_i], roster[roster_i].stake));
                round_data.msg_val_sigs[roster_i][is_precommit] = new_val_sig;
                let new_cs = ConsensusCounts::from(&(round_data.msg_val_sigs[roster_i], roster[roster_i].stake));
                let d = new_cs - old_cs; // add 1 to counts that have been updated by this message
                round_data.counts = round_data.counts + d;

                if PRINT_BFT_UPDATE && (
                    d.anys           |
                    d.prevotes       |
                    d.precommits     |
                    d.yes_prevotes   |
                    d.yes_precommits |
                    d.nil_prevotes) != 0 {
                    println!("{ctx_str} {ANSI_GRY}BFT_UPDATE{ANSI_RST}: update to {:?} (d: {:?})", round_data.counts, d);
                }

                #[cfg(debug_assertions)]
                {
                    let mut check_counts = ConsensusCounts::ZERO;
                    for (i, sig) in round_data.msg_val_sigs.iter().enumerate() {
                        check_counts = check_counts + ConsensusCounts::from(&(*sig, roster[i].stake));
                    }
                    if check_counts != round_data.counts {
                        eprintln!("{ctx_str} {ANSI_RED}BFT ERROR{ANSI_RST}: counts don't match: incremental: {:?}, absolute: {:?}", round_data.counts, check_counts);
                    }
                }

                status
            }


            _ => {
                eprintln!("{ctx_str} {ANSI_RED}BFT ERROR{ANSI_RST}: unexpected case: {}", packet_type);
                TMStatus::Fail
            }
        }
    }

    fn prune_unnecessary_data(&mut self) {
        // TODO (perf): drop 2f+1 nil-voted rounds before n-2
        todo!();
    }

    fn ctx_str(&self, roster: &[SortedRosterMember]) -> String {
        // format!("{:?} {:05}-{:?}-{:?}.{:3}.{:3}.{:9}", roster.into_iter().map(|m| m.pub_key).collect::<Vec<_>>(), self.my_port, self.my_pub_key, roster_i_from_pub_key(roster, self.my_pub_key), self.height, self.round, format!("{:?}", self.step))
        // format!("{:05}-{:?}-{:>8}.{:3}.{:3}.{:9}", self.my_port, self.my_pub_key, format!("{:?}", roster_i_from_pub_key(roster, self.my_pub_key)), self.height, self.round, format!("{:?}", self.step))
        let pk = self.my_pub_key;
        let roster_i = roster_i_from_pub_key(roster, pk);
        // let pk_str = if roster_i.is_some() { format!("{pk:?} ") } else { "          ".to_string() };
        format!("{pk:?}{:>2}:{:2}.{:2}.{:>9}",
                roster_i.map(|i| format!("{i}")).unwrap_or(Default::default()),
                self.height,
                self.round,
                format!("{:?}", self.step))
    }
    fn name_str_other(roster: &[SortedRosterMember], bft_key: PubKeyID, address: Option<&STPAddress>) -> String {
        let port = address.map_or(0, |a| a.port);
        format!("{:05}-{:?}-{:?}", port, bft_key, roster_i_from_pub_key(roster, bft_key))
    }

    async fn bft_update(&mut self, roster: &mut Vec<SortedRosterMember>) {
        let now = Instant::now();
        let mut total_active_stake = 0;
        for i in 0..active_roster_len(roster) {
            total_active_stake += roster[i].stake;
        }
        let total_active_stake = total_active_stake;
        let f = Self::f_from_n(total_active_stake);
        let big_threshold;
        let small_threshold;
        if f == 0 {
            big_threshold = total_active_stake;
            small_threshold = total_active_stake;
        } else {
            big_threshold = 2*f+1;
            small_threshold = f+1;
        }
        let ctx_str = self.ctx_str(roster);

        // NOTE: binary search to {current height, round 0} to avoid looping through data for unneeded decided heights
        let current_height_start_i = self.rounds_data.binary_search_by_key(&(self.height, 0), |el| (el.height, el.round)).unwrap_or(0);

        for i in current_height_start_i..self.rounds_data.len() {
            let on_roster = roster_i_from_pub_key(&roster[..active_roster_len(roster)], self.my_pub_key).is_some();
            let counts = self.rounds_data[i].counts.clone();
            let has_enough_info_to_determine_validity = self.rounds_data[i].has_enough_info_to_determine_validity();

            // TODO: don't spam "while" messages repeatedly
            let is_current_height_and_round = (self.height, self.round) == (self.rounds_data[i].height, self.rounds_data[i].round);
            // println!("{:#?}", self);
            if PRINT_BFT_STATE { println!("{ctx_str} {ANSI_GRY}BFT_STATE{ANSI_RST}: {}={}.{}, {}/{}, {}",
                    ["!","="][is_current_height_and_round as usize],
                    self.rounds_data[i].height, self.rounds_data[i].round,
                    self.rounds_data[i].proposal_sigs_n, self.rounds_data[i].proposal_sigs.len(),
                    self.rounds_data[i].proposal_valid_round
            ); }

            // line 11: init proposal period
            // (done elsewhere)

            // line 22: receive first proposal this height: prevote
            // > upon <PROPOSAL, h_p, round_p, v, −1> from proposer(h_p, round_p)
            // > while step_p = propose do
            // TODO: merge conditionals with below, they massively overlap
            if (on_roster &&
                is_current_height_and_round &&
                has_enough_info_to_determine_validity && // we have received the proposal value
                self.rounds_data[i].proposal_valid_round == -1 &&
                self.step == TMStep::Propose)
            {
                // TODO: do we want to prevote NIL on currently-indeterminate?
                // ALT: send NIL then later override with time-tagged message
                if self.rounds_data[i].proposal_is_valid(self.validate_closure.clone()).await == TMStatus::Pass && (
                    self.locked_value_round.1 == -1 ||
                    self.locked_value_round.0 == Some(self.rounds_data[i].proposal.clone())) // TODO(perf): use (previously-checked) ids for easier comparison?
                {
                    if PRINT_BFT_CONDITIONS { println!("{ctx_str} {ANSI_GRY}BFT_CONDITIONS{ANSI_RST}: in condition 22-0: receive first proposal this height"); }
                    self.step = self.broadcast(roster, i, TMMsgData::Prevote(self.rounds_data[i].proposal_id));
                } else {
                    if PRINT_BFT_CONDITIONS { println!("{ctx_str} {ANSI_GRY}BFT_CONDITIONS{ANSI_RST}: in condition 22-1: receive first proposal this height"); }
                    self.step = self.broadcast(roster, i, TMMsgData::Prevote(ValueId::NIL));
                }
            }

            // line 28: received 2f+1 prevotes: prevote
            // > upon <PROPOSAL, h_p, round_p, v, vr> from proposer(h_p, round_p) AND 2f+1 <PREVOTE, h_p, vr, id(v)>
            // > while step_p = propose && (0 <= vr && vr < round_p)
            if (on_roster &&
                is_current_height_and_round &&
                has_enough_info_to_determine_validity &&
                big_threshold <= counts.yes_prevotes &&
                self.step == TMStep::Propose &&
                0 <= self.rounds_data[i].proposal_valid_round && self.rounds_data[i].proposal_valid_round < self.round as i64) // we have received the proposal value
            {
                if self.rounds_data[i].proposal_is_valid(self.validate_closure.clone()).await == TMStatus::Pass && (
                    self.locked_value_round.1 <= self.rounds_data[i].proposal_valid_round ||
                    self.locked_value_round.0 == Some(self.rounds_data[i].proposal.clone()))
                {
                    if PRINT_BFT_CONDITIONS { println!("{ctx_str} {ANSI_GRY}BFT_CONDITIONS{ANSI_RST}: in condition 28-0: received 2f+1 prevotes"); }
                    self.step = self.broadcast(roster, i, TMMsgData::Prevote(self.rounds_data[i].proposal_id));
                } else {
                    if PRINT_BFT_CONDITIONS { println!("{ctx_str} {ANSI_GRY}BFT_CONDITIONS{ANSI_RST}: in condition 28-1: received 2f+1 prevotes"); }
                    self.step = self.broadcast(roster, i, TMMsgData::Prevote(ValueId::NIL));
                }
            }

            // line 34: last orders on prevote period
            // > upon 2f+1 <PREVOTE, h_p, round_p, ∗> while step_p = prevote for the first time do
            if (on_roster &&
                is_current_height_and_round &&
                // don't need the proposal itself
                big_threshold <= counts.prevotes &&
                self.step == TMStep::Prevote &&
                !self.rounds_data[i].timeout_triggered[0]) // "for the first time" // ALT: round.timeout_step != TMStep::Prevote
            {
                if PRINT_BFT_CONDITIONS { println!("{ctx_str} {ANSI_GRY}BFT_CONDITIONS{ANSI_RST}: in condition 34: last orders on prevote period"); }
                self.rounds_data[i].timeout_triggered[0] = true;
                self.rounds_data[i].active_timeout = Some(Timeout::new(now, self.height, self.round, TMStep::Prevote));
            }

            // line 36: seen 2f+1 valid prevotes: lock, valid, precommit
            // > upon <PROPOSAL, h_p, round_p, v, ∗> from proposer(h_p, round_p) AND 2f+1 <PREVOTE, h_p, round_p, id(v)>
            // > while valid(v) && step_p >= prevote for the first time do
            if (on_roster &&
                is_current_height_and_round &&
                has_enough_info_to_determine_validity &&
                big_threshold <= counts.yes_prevotes &&
                self.rounds_data[i].proposal_is_valid(self.validate_closure.clone()).await == TMStatus::Pass &&
                (self.step == TMStep::Prevote || self.step == TMStep::Precommit)) // TODO: "for the first time"
            {
                if PRINT_BFT_CONDITIONS { println!("{ctx_str} {ANSI_GRY}BFT_CONDITIONS{ANSI_RST}: in condition 36: seen 2f+1 valid prevotes"); }
                if self.step == TMStep::Prevote {
                    if PRINT_BFT_CONDITIONS { println!("{ctx_str} {ANSI_GRY}BFT_CONDITIONS{ANSI_RST}: in condition 36-0: seen 2f+1 valid prevotes"); }
                    self.locked_value_round = (Some(self.rounds_data[i].proposal.clone()), self.round as i64);
                    self.step = self.broadcast(roster, i, TMMsgData::Precommit(self.rounds_data[i].proposal_id));
                }
                self.valid_value_round = (Some(self.rounds_data[i].proposal.clone()), self.round as i64);
            }

            // line 44: seen 2f+1 nil prevotes: precommit nil
            // > upon 2f+1 <PREVOTE, h_p, round_p, nil>
            // > while step_p = prevote do
            if (on_roster &&
                is_current_height_and_round &&
                big_threshold <= counts.nil_prevotes &&
                self.step == TMStep::Prevote)
            {
                if PRINT_BFT_CONDITIONS { println!("{ctx_str} {ANSI_GRY}BFT_CONDITIONS{ANSI_RST}: in condition 44: seen 2f+1 nil prevotes"); }
                self.step = self.broadcast(roster, i, TMMsgData::Precommit(ValueId::NIL));
            }

            // line 47: last orders on precommit period
            // > upon 2f+1 <PRECOMMIT, h_p, round_p, ∗> for the first time do
            if (on_roster &&
                is_current_height_and_round &&
                big_threshold <= counts.precommits &&
                !self.rounds_data[i].timeout_triggered[1])
            {
                if PRINT_BFT_CONDITIONS { println!("{ctx_str} {ANSI_GRY}BFT_CONDITIONS{ANSI_RST}: in condition 47: last orders on precommit period"); }
                self.rounds_data[i].timeout_triggered[1] = true;
                self.rounds_data[i].active_timeout = Some(Timeout::new(now, self.height, self.round, TMStep::Precommit));
            }

            // line 49: value decided
            // > upon <PROPOSAL, h_p, r, v, ∗> from proposer(h_p, r) AND 2f+1 <PRECOMMIT, h_p, r, id(v)>
            // > while decision_p[h_p] = nil do
            // @note(judah): observers only care about this
            if (self.height == self.rounds_data[i].height && // any round
                has_enough_info_to_determine_validity &&
                big_threshold <= counts.yes_precommits &&
                self.rounds_data[i].proposal_is_valid(self.validate_closure.clone()).await == TMStatus::Pass)
            {
                if PRINT_BFT_CONDITIONS { println!("{ctx_str} {ANSI_GRY}BFT_CONDITIONS{ANSI_RST}: in condition 49: value decided"); }
                let new_roster = self.push_block_closure.0(self.rounds_data[i].proposal.clone(), round_data_to_fat_pointer(&self.rounds_data[i], roster), self.rounds_data[i].proposal_sigs.clone()).await;
                if PRINT_ROSTER { println!("{ctx_str} {ANSI_GRY}ROSTER{ANSI_RST}: new roster: {:?}", new_roster); }
                *roster = new_roster;
                self.height += 1;
                self.recent_commit_round_cache.push(self.rounds_data[i].clone());
                self.rounds_data.retain(|r| r.height < self.height);
                self.locked_value_round = (None, -1);
                self.valid_value_round = (None, -1);
                self.start_round(roster, now, 0).await;
            }

            // line 55: round catchup
            // > upon f+1 <∗, h_p, round, ∗, ∗> with round > round_p do
            if (on_roster &&
                self.height == self.rounds_data[i].height &&
                self.round    <  self.rounds_data[i].round  &&
                small_threshold <= counts.anys)
            {
                if PRINT_BFT_CONDITIONS { println!("{ctx_str} {ANSI_GRY}BFT_CONDITIONS{ANSI_RST}: in condition 55: round catchup"); }
                self.start_round(roster, now, self.rounds_data[i].round).await
            }

            // timeouts
            if let Some(timeout) = &self.rounds_data[i].active_timeout &&
                timeout.time <= now &&
                self.height  == timeout.height &&
                self.round   == timeout.round &&
                on_roster
            {
                // TODO(code): can we just use *our* step or is there a possible sequence issue? (from the presence of step checks, probably not)
                match timeout.step {
                    TMStep::Propose => if self.step == TMStep::Propose {
                        if PRINT_BFT_TIMEOUTS { println!("{ctx_str} {ANSI_GRY}BFT_TIMEOUTS{ANSI_RST}: hit timeout propose"); }
                        self.step = self.broadcast(roster, i, TMMsgData::Prevote(ValueId::NIL));
                    },
                    TMStep::Prevote => if self.step == TMStep::Prevote {
                        if PRINT_BFT_TIMEOUTS { println!("{ctx_str} {ANSI_GRY}BFT_TIMEOUTS{ANSI_RST}: hit timeout prevote"); }
                        self.step = self.broadcast(roster, i, TMMsgData::Precommit(ValueId::NIL));
                    },
                    TMStep::Precommit => {
                        if PRINT_BFT_TIMEOUTS { println!("{ctx_str} {ANSI_GRY}BFT_TIMEOUTS{ANSI_RST}: hit timeout precommit"); }
                        self.start_round(roster, now, self.round + 1).await
                    },
                }
            }


            // If this value would have been decided, but we can't validate it because we need its PoW block, let's mark this hash as required
            if (self.height == self.rounds_data[i].height &&
                self.rounds_data[i].has_full_proposal() && // has_enough_info_to_determine_validity &&
                big_threshold <= counts.yes_precommits &&
                self.rounds_data[i].proposal_checked_validity.0 == TMStatus::Indeterminate)
            {
                match self.rounds_data[i].proposal_checked_validity.1 {
                    TMStatusReason::NeedsBlock { hash } => {
                        if PRINT_BLOCK_NEEDED { println!("{ctx_str} {ANSI_YLW}BLOCK NEEDED{ANSI_RST} hash: {:?}...", hash); }
                    },
                    _ => {
                    }
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub latest_status_request_height: Option<u64>,
    pub latest_status: Option<PacketStatus>,
    pub index_counter: u64,           // for some peer randomness
    pub bft_pk: PubKeyID,             // for convenience // @Todo: @Remove! @@@!!! Don't replicate state like this; look it up from canonical unique sources.
    pub stp_address: STPAddress,      // for convenience // @Todo: @Remove! @@@!!! Don't replicate state like this; look it up from canonical unique sources.
    pub stp_handshake_hash: [u8; 64], // for convenience // @Todo: @Remove! @@@!!! Don't replicate state like this; look it up from canonical unique sources.
}
impl Default for Peer {
    fn default() -> Self { Self {
        latest_status_request_height: Default::default(),
        latest_status:                Default::default(),
        index_counter:                Default::default(),
        bft_pk:                       Default::default(),
        stp_address:                  Default::default(),
        stp_handshake_hash:           [0u8; 64],
    } }
}
impl Peer {
    fn info(&self, connected: bool, bft_key: PubKeyID) -> PeerInfo {
        return PeerInfo {
            connected,
            root_public_bft_key: Some(bft_key),
            latest_status_request_height: self.latest_status_request_height.unwrap_or_default(),
        };
    }
}

#[derive(Debug, Default, Copy, Clone)]
pub struct PeerInfo {
    pub connected: bool,
    pub root_public_bft_key: Option<PubKeyID>,
    pub latest_status_request_height: u64,
}

pub use crate::helpers::*;
pub use crate::bandwidth_test::STPAddress;
pub use crate::bandwidth_test::fmt_byte_str;
pub use crate::bandwidth_test::ConnectionKey;
pub use crate::bandwidth_test::IdentityKeyPair;
pub use crate::bandwidth_test::fmt_byte_str_rev;
pub use crate::bandwidth_test::fmt_prefixed_byte_str;
pub use crate::bandwidth_test::fmt_prefixed_byte_str_rev;
pub use crate::bandwidth_test::CONNECT_MAGIC1_PLAIN_TEXT;
pub use crate::bandwidth_test::total_packet_payload_overhead_from_connect_magic1_inside_udp_payload;
pub use crate::bandwidth_test::new_keypair_from_connect_magic1_with_seed;
pub use crate::bandwidth_test::CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2s;


pub const MAX_P2P_DISCOVERY_PUBKEY_SIZE: usize = 32;


pub const STP_ADDRESS_SERIALIZED_SIZE: usize = 16 + 8 + MAX_P2P_DISCOVERY_PUBKEY_SIZE; // @Volatile.
pub const STP_ADDRESS_MEMORY_SIZE: usize =
    16 /* ip */ +
     2 /* port */ +
     8 /* magic1 */ +
    32 /* noise curve25519 pk */; // @Volatile.


impl SliceWrite for STPAddress {
    fn write_to(&self, buf: &mut [u8]) -> usize {
        let key             = &self.key[..MAX_P2P_DISCOVERY_PUBKEY_SIZE];
        let port_and_magic1 = (self.port as u64 | self.magic1 << 16);

        let mut o = 0;
        o += self.ip.octets().write_to(&mut buf[o..]);
        o += port_and_magic1 .write_to(&mut buf[o..]);
        o += self.key        .write_to(&mut buf[o..]);
        o
    }
}
impl SliceRead for STPAddress {
    fn read_from(buf: &mut &[u8]) -> Option<Self> {
        let ip_bytes: [u8; 16]                       = SliceRead::read_from(buf)?;
        let port_and_magic1: u64                     = SliceRead::read_from(buf)?;
        let key: [u8; MAX_P2P_DISCOVERY_PUBKEY_SIZE] = SliceRead::read_from(buf)?;
        Some(Self {
            ip:     Ipv6Addr::from(ip_bytes),
            port:   port_and_magic1 as u16,
            magic1: port_and_magic1 >> 16,
            key:    key.to_vec(),
        })
    }
}

impl SliceWrite for TMSig    { fn write_to(&self, buf: &mut [u8]) -> usize { self.0.write_to(buf) } }
impl SliceWrite for PubKeyID { fn write_to(&self, buf: &mut [u8]) -> usize { self.0.write_to(buf) } }
impl SliceRead  for TMSig    { fn read_from(buf: &mut &[u8]) -> Option<Self> { Some(Self(SliceRead::read_from(buf)?)) } }
impl SliceRead  for PubKeyID { fn read_from(buf: &mut &[u8]) -> Option<Self> { Some(Self(SliceRead::read_from(buf)?)) } }

#[derive(Debug, Default, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct FinalizerPeerAddress {
    pub bft_pk: PubKeyID,
    pub address: STPAddress,
}

fn make_vote_sign_datas(pub_key: PubKeyID, is_precommit: bool, height: u64, round: u32, value_id: ValueId) -> [[u8; 76]; 2] {
    let mut sign_data_no = [0; 76];
    sign_data_no[0..32].copy_from_slice(&pub_key.0[..]);
    height.write_to(&mut sign_data_no[64..]);
    (round + 0x8000_0000 * (is_precommit as u32)).write_to(&mut sign_data_no[72..]);
    let mut sign_data_yes = sign_data_no;
    value_id.0.write_to(&mut sign_data_yes[32..64]);
    [sign_data_no, sign_data_yes]
}

pub fn gen_mostly_empty_rngs<F: Fn(usize) -> bool>(n: usize, f: F) -> Vec<[usize; 2]> {
    let mut rngs: Vec<[usize;2]> = Vec::with_capacity(n);
    let mut filled_c = 0; // consecutive fills
    let mut rng = [0, 0];
    // TODO(perf): these can be split arbitrarily & merged if we wanted to go wide
    for i in 0..n {
        if f(i) {
            rng[1] = i+1;
            filled_c = 0; // consecutive only, could also consider occupancy
        } else if rng[0] == rng[1] { // skip over leading fills
            rng[0] = i+1;
            rng[1] = i+1;
        } else {
            filled_c += 1;
            if filled_c > 1 { // 2 in a row
                rngs.push(rng);
                filled_c = 0;
                rng[0] = i+1;
                rng[1] = i+1;
            }
        }
    }
    if rng[0] != rng[1] {
        rngs.push(rng);
    }

    rngs
}

async fn instance(
    my_root_private_key: SigningKey,
    my_stp_keypair: Option<IdentityKeyPair>,
    my_endpoint: Option<STPAddress>,
    roster: Vec<SortedRosterMember>,
    finalizer_peer_addresses: Vec<FinalizerPeerAddress>,
    maybe_seed: Option<u128>,
) -> std::io::Result<()> {
    let block_rng = Arc::new(Mutex::new({
        let seed : u128 = maybe_seed.clone().unwrap_or_else(|| {
            let mut seed_rng = rand::rng();
            ((seed_rng.next_u64() as u128) << 64) | seed_rng.next_u64() as u128
        });
        SimRng::new(seed, 0)
    }));

    let should_propose_bad_value_sometimes = false; // my_endpoint.is_some(); // peer 0 only

    let decisions = Arc::new(Mutex::new(Vec::<(BlockValue, FatPointerToBftBlock)>::new()));
    let decisions2 = Arc::clone(&decisions);

    let roster2 = roster.clone();
    let pub_key = PubKeyID(VerificationKeyBytes::from(&my_root_private_key.clone()).into());

    entry_point(my_root_private_key, my_stp_keypair, my_endpoint, roster, finalizer_peer_addresses, maybe_seed,
        ClosureToProposeNewBlock(Arc::new(move || {
            let block_rng = Arc::clone(&block_rng);
            Box::pin(async move {
                let mut buf = vec![0; 6000]; // TODO: replace with real data
                block_rng.lock().unwrap().fill_bytes(&mut buf);
                if should_propose_bad_value_sometimes == false { buf[0] = 0; }
                Some(BlockValue(buf))
            })
        })),
        ClosureToValidateProposedBlock(Arc::new(move |block| {
            Box::pin(async move {
                if block.0.len() == 0 { (TMStatus::Fail, TMStatusReason::None) }
                else                  { (TMStatus::Pass, TMStatusReason::None) }

                // if block.0.len() == 0 { (TMStatus::Fail, TMStatusReason::None) }
                // else if block.0[0] % 2 == 0 { (TMStatus::Pass, TMStatusReason::None) }
                // //else if block.0[0] % 3 == 1 { (TMStatus::Indeterminate, TMStatusReason::None) }
                // else { (TMStatus::Fail, TMStatusReason::None) }
            })
        })),
        ClosureToPushDecidedBlock(Arc::new(move |block, fat_pointer, _tender_proposal_sigs| {
            let decisions = Arc::clone(&decisions);
            let roster2 = roster2.clone();
            Box::pin(async move {
                decisions.lock().unwrap().push((block, fat_pointer));
                let mut ret = roster2.clone();
                ret.truncate(3 + decisions.lock().unwrap().len() % 2);
                ret
            })
        })),
        ClosureToUpdatePeers(Arc::new(move |_all_peers| { Box::pin(async move {
        })})),

        ClosureToAllowBftAccess(Arc::new(move |_bft_state, _key_addr_map| { Box::pin(async move {
        })})),

        Vec::new(),
    ).await
}

/// Parses "IP[:port]" (IPv4 or bracketed IPv6 with port) into (16-byte IPv6, port)
pub fn parse_to_ipv6_bytes(s: &str) -> Result<(Ipv6Addr, u16), std::net::AddrParseError> {
    let sa: SocketAddr = s.parse()?;

    let (ip6, port) = match sa {
        SocketAddr::V4(v4) => {
            // Map IPv4 to IPv6-mapped ::ffff:a.b.c.d
            (v4.ip().to_ipv6_mapped(), v4.port())
        }
        SocketAddr::V6(v6) => (*v6.ip(), v6.port()),
    };

    Ok((ip6.octets().into(), port))
}

use std::hash::{Hash, Hasher};
pub fn addr_string_to_stuff(addr: &str) -> (IdentityKeyPair, STPAddress) {
    let mut hasher = DefaultHasher::new();
    hasher.write(addr.as_bytes());
    let seed = hasher.finish();

    let mut other_seed = [0; 32];
    rand_chacha::ChaCha20Rng::seed_from_u64(seed).fill(&mut other_seed);
    let static_keypair = new_keypair_from_connect_magic1_with_seed(CRYPTO_MAGIC, other_seed).unwrap();

    let (ip, port) = match parse_to_ipv6_bytes(addr) {
        Ok(v) => v,
        Err(err) => panic!("failed to parse IPV6 from {addr}"),
    };
    (
        static_keypair.clone(),
        STPAddress {
            ip,
            port,
            magic1: CRYPTO_MAGIC,
            key: static_keypair.clone().public.try_into().unwrap(),
        },
    )
}


#[derive(Debug, Default, Clone)]
pub struct BftAddressMap {
    pub by_key:  HashMap<PubKeyID, HashMap<STPAddress, Option<PeerAttestation>>>,
    pub by_addr: HashMap<STPAddress, PubKeyID>,
    pub last_packet_utcs: HashMap<PubKeyID, i64>,
}
impl BftAddressMap {
    pub fn new() -> Self { Self::default() }
    pub fn insert(&mut self, key: &PubKeyID, addr: &STPAddress, attestation: Option<PeerAttestation>) {
        self.by_key.entry(*key).or_default().insert(addr.clone(), attestation);
        self.by_addr.insert(addr.clone(), *key);
    }
    pub fn get_key(&self, addr: &STPAddress) -> Option<&PubKeyID> { self.by_addr.get(addr) }
    pub fn get_addrs(&self, key: &PubKeyID) -> impl Iterator<Item = (&STPAddress, &Option<PeerAttestation>)> { self.by_key.get(key).map(|v| v.iter()).unwrap_or_default() }
    pub fn contains_key(&self, key: &PubKeyID) -> bool { self.by_key.contains_key(key) }
    pub fn all_addrs(&self) -> impl Iterator<Item = (&STPAddress, &PubKeyID)> { self.by_addr.iter() }
}


pub async fn entry_point(my_root_private_key: SigningKey,
                         my_stp_keypair: Option<IdentityKeyPair>,
                         my_endpoint: Option<STPAddress>,
                         roster: Vec<SortedRosterMember>,
                         finalizer_peer_addresses: Vec<FinalizerPeerAddress>,
                         maybe_seed: Option<u128>,
                         propose_closure: ClosureToProposeNewBlock,
                         validate_closure: ClosureToValidateProposedBlock,
                         push_block_closure: ClosureToPushDecidedBlock,
                         peer_cmd_closure: ClosureToUpdatePeers,
                         bft_access_closure: ClosureToAllowBftAccess,
                         ingest_startup_data: Vec<RoundData>,
                        ) -> std::io::Result<()> {
    hook_fail_on_panic();

    let mut roster = roster.clone();

    let mut base_rng = {
        let seed : u128 = maybe_seed.unwrap_or_else(|| {
            let mut seed_rng = rand::rng();
            ((seed_rng.next_u64() as u128) << 64) | seed_rng.next_u64() as u128
        });
        SimRng::new(seed, 0)
    };

    let my_root_public_bft_key = VerificationKeyBytes::from(&my_root_private_key);
    {
        let key : &[u8; 32] = &my_root_public_bft_key.into();
        print!("My root public key is \"");
        for i in 0..key.len() { print!("{:02x}", key[i]); }
        println!("\"");
    }

    let my_stp_keypair = my_stp_keypair.unwrap_or(new_keypair_from_connect_magic1(CRYPTO_MAGIC).unwrap());

    use crate::bandwidth_test::*;
    use crate::native_sockets::*;

    let my_port = my_endpoint.map(|e| e.port).unwrap_or(23485); // @Dev: .unwrap_or(0); // @Todo! Get local port after sock creation! @@@
    let network_thread_handle = new_network_thread(vec![my_stp_keypair.clone()], my_port, None, (1_000_000, 256 * 1024 * 1024, 256 * 1024 * 1024));
    let mut current_connections = Vec::<(STPAddress, [u8; 64])>::new();
    let mut initiate_connections = Vec::<STPAddress>::new();
    let mut messages_to_send = Vec::new();

    let mut peers = HashMap::<ConnectionKey, Peer>::new();
    let mut bft_address_map = BftAddressMap::new();

    let mut my_address_attestations = Vec::new();

    for FinalizerPeerAddress { bft_pk, address } in &finalizer_peer_addresses {
        bft_address_map.insert(bft_pk, address, None);

        if address.magic1 != CRYPTO_MAGIC {
            // @Dev
            panic!("The magic in the config toml - {} ({}) is different from the crypto magic - {} ({})! Modify one or the other!",
                    bandwidth_test::b64(&address.magic1.to_le_bytes()[..6]),
                    bandwidth_test::crypto_string_from_connect_magic1(address.magic1).unwrap_or("<invalid>"),
                    bandwidth_test::b64(&CRYPTO_MAGIC  .to_le_bytes()[..6]),
                    bandwidth_test::crypto_string_from_connect_magic1(CRYPTO_MAGIC).unwrap(),
                    );
        }
    }

    if PRINT_PROTOCOL { println!("socket port={:05}, peers endpoints={:?}", my_port, bft_address_map.by_key); }

    // TODO: only convert private to public in 1 location
    let mut bft_state = TMState::init(
        my_root_private_key,
        PubKeyID(my_root_public_bft_key.into()),
        my_port,
        propose_closure,
        validate_closure,
        push_block_closure,
        peer_cmd_closure,
        bft_access_closure,
    ); // TODO: double-check this is the right key

    bft_state.height = ingest_startup_data.len() as u64;
    bft_state.recent_commit_round_cache = ingest_startup_data;

    bft_state.start_round(&roster, Instant::now(), 0).await;

    const ONE_SECOND: tokio::time::Duration = tokio::time::Duration::from_secs(1);
    let mut net_stats_window_start = tokio::time::Instant::now();
    let mut net_stats = NetworkStats::default();

    let mut next_peer_gossip  = std::time::Instant::now();
    let mut next_peer_connect = std::time::Instant::now();

    let mut send_buf1 = [0u8; 2048];
    let mut next_tick_time = tokio::time::Instant::now();
    loop {
        let ctx_str = bft_state.ctx_str(&roster);

        {
            let mut peers = peers.iter().map(|(ck, p)| {
                let mut bft_key = PubKeyID::NIL;
                for (c, _) in &current_connections {
                    if c.connection_key() == *ck {
                        if let Some(k) = bft_address_map.get_key(c) {
                            bft_key = *k;
                        }
                        break;
                    }
                }
                p.info(true, bft_key)
            }).collect::<Vec<PeerInfo>>();
            bft_state.update_peers_cmd_closure.0(peers).await;
        }

        fn read_header_and_maybe_status(msg: &[u8]) -> Option<(PacketHeader, Option<PacketStatus>, usize)> {
            let buf = &mut &msg[..];
            let header = PacketHeader::read_from(buf)?;

            print_packet_tag_recv(header);

            let mut status = None;
            if header.has_status() {
                // TODO: scope down required ranges
                status = Some(PacketStatus::read_from(buf)?);
            }
            let bytes_read = msg.len() - buf.len();
            Some((header, status, bytes_read))
        }

        fn write_header_and_maybe_status(header_: PacketHeader,
                                         include_status: bool,
                                         bft_state: &TMState,
                                         roster: &[SortedRosterMember],
                                         send_buf1: &mut [u8],
                                         peer_random: u64) -> usize {
            let ctx_str = bft_state.ctx_str(roster);

            let mut header = header_;
            header.tag |= if include_status { PACKET_TAG_STATUS_FLAG as u64 } else { 0 };

            let mut o = 0;
            o += header.write_to(&mut send_buf1[o..]);

            if include_status {
                let mut status = PacketStatus {
                    height: bft_state.height,
                    round: bft_state.round,
                    need_proposal_chunk_rngs: [[0, 0]],
                    need_vote_rngs: [[[0, active_roster_len(roster) as u16]]; 2],
                };


                // TODO: scope down required ranges
                // TODO: probably generate these ranges once per tick/incrementally update & pull from it
                // TODO: weight by stake? (easily determined by cumulative stake)
                if let Ok(current_round_i) = bft_state.rounds_data.binary_search_by_key(&(status.height, status.round), |el| (el.height, el.round))
                {

                    let round_data = &bft_state.rounds_data[current_round_i];

                    let proposal_chunk_rngs = gen_mostly_empty_rngs(round_data.proposal_sigs.len(), |i| round_data.proposal_sigs[i] == TMSig::NIL);
                    if proposal_chunk_rngs.len() > 0 {
                        let mut random_i = peer_random;
                        for dst_rng in &mut status.need_proposal_chunk_rngs {
                            let rng = proposal_chunk_rngs[random_i as usize % proposal_chunk_rngs.len()];
                            *dst_rng = [rng[0].try_into().unwrap(), rng[1].try_into().unwrap()];
                            random_i = random_i.wrapping_add(1610612741); // large prime
                            // TODO: "with removal"
                        }
                    }
                    if PRINT_RNGS { println!("{ctx_str} {ANSI_GRY}RNGS{ANSI_RST}: request proposal  chunks {:?} from {:?}", status.need_proposal_chunk_rngs, proposal_chunk_rngs); }

                    for is_precommit in 0..2 {
                        let vote_rngs = gen_mostly_empty_rngs(active_roster_len(roster), |i| round_data.msg_val_sigs[i][is_precommit].1 == TMSig::NIL);
                        if vote_rngs.len() > 0 {
                            let mut random_i = peer_random;
                            for dst_rng in &mut status.need_vote_rngs[is_precommit] {
                                let rng = vote_rngs[random_i as usize % vote_rngs.len()];
                                *dst_rng = [rng[0].try_into().unwrap(), rng[1].try_into().unwrap()];
                                random_i = random_i.wrapping_add(1610612741); // large prime
                                // TODO: "with removal"
                            }
                        }
                        if PRINT_RNGS { println!("{ctx_str} {ANSI_GRY}RNGS{ANSI_RST}: request {:9} chunks {:?} from {:?}", ["prevote", "precommit"][is_precommit], status.need_vote_rngs[is_precommit], vote_rngs); }
                    }

                }

                o += status.write_to(&mut send_buf1[o..]);
            }
            o
        }
        fn send_stp_msg(messages_to_send: &mut Vec<(ConnectionKey, Vec<u8>)>,
                        connection_key: &ConnectionKey,
                        msg: &[u8],
                        stats: &mut NetworkStats) {
            stats.packets_sent += 1;
            stats.bytes_sent += msg.len();
            messages_to_send.push((*connection_key, Vec::from(msg)));
        }

        if net_stats_window_start.elapsed() >= 10*ONE_SECOND {
            net_stats = NetworkStats::default();
            net_stats_window_start = tokio::time::Instant::now();
        }

        // if PRINT_PROTOCOL { println!("{ctx_str} {ANSI_GRY}PROTOCOL{ANSI_RST}: Connected to {} peers", current_connections.len()); }

        let was_now = tokio::time::Instant::now();
        if was_now > next_tick_time {
            {
                // TICK CODE
                peers.retain(|connection_key, peer| {
                    if current_connections.iter().position(|(x, _)| x.connection_key() == *connection_key).is_none() {
                        println!("{:05}: Disconnected from peer {:?}.", my_port, connection_key);
                        false
                    } else {
                        true
                    }
                });

                // Try to reconnect to "seeders"
                for FinalizerPeerAddress { address, .. } in &finalizer_peer_addresses {
                    initiate_connections.push(address.clone());
                }

                bft_state.bft_access_closure.0(&bft_state, &bft_address_map).await;

                // BFT CONSENSUS
                // account for the state updates we've accumulated
                bft_state.bft_update(&mut roster).await;

                fn send_round_data_to_peer(bft_state: &TMState,
                                           should_send_prevotes: bool,
                                           round_data: &RoundData,
                                           ctx_str: &str,
                                           roster: &[SortedRosterMember],
                                           messages_to_send: &mut Vec<(ConnectionKey, Vec<u8>)>,
                                           send_buf1: &mut [u8],
                                           peer: &mut Peer,
                                           connection_key: &ConnectionKey,
                                           peer_bft_key: PubKeyID,
                                           stats: &mut NetworkStats) {
                    let height = round_data.height;
                    let round  = round_data.round;

                    // Make sure to always sent at least one status.
                    {
                        let header = PacketHeader::new::<PACKET_TYPE_EMPTY>();
                        let mut o = 0;
                        o += write_header_and_maybe_status(header, true, bft_state, roster, &mut send_buf1[o..], peer.index_counter); peer.index_counter += 1;
                        send_stp_msg(messages_to_send, connection_key, &send_buf1[..o], stats);
                    }

                    let mut chunk_hdr = PacketProposalChunkHeader {
                        height, round, chunk_i: 0,
                        proposal_size: round_data.proposal.0.len().try_into().unwrap(),
                        proposal_id: round_data.proposal_id,
                        valid_round: round_data.proposal_valid_round,
                    };
                    let (_, proposer_pub_key) = TMState::proposer_from_height_round(&bft_state.hash_keys, &round_data.roster, height, round);

                    let mut sent_chunk_cs = 0;
                    let mut sent_c: [usize; 2] = [0; 2];

                    if round_data.proposal_sigs_n > 0 {
                        for chunk_i in 0..round_data.proposal_sigs.len() {
                            // send all of the proposal chunks we've seen
                            if round_data.proposal_sigs[chunk_i] != TMSig::NIL {
                                chunk_hdr.chunk_i = chunk_i as u32;

                                let mut o = 0;

                                let header = PacketHeader::new::<PACKET_TYPE_PROPOSAL_CHUNK>();
                                // @Todo @Speed: We should build the header once and save it for all chunks. Peers only obey the latest status anyway.
                                o += write_header_and_maybe_status(header, true, bft_state, roster, &mut send_buf1[o..], peer.index_counter); peer.index_counter += 1;
                                let signed_data_start = o; // past header+status

                                o += chunk_hdr.write_to(&mut send_buf1[o..]);

                                let (chunk_o, chunk_size) = round_data.proposal.chunk_o_size(chunk_i);
                                o += round_data.proposal.0[chunk_o..chunk_o + chunk_size].write_to(&mut send_buf1[o..]);
                                let sig_o = o;
                                o += round_data.proposal_sigs[chunk_i].0.write_to(&mut send_buf1[o..]);

                                #[cfg(debug_assertions)] // self-check signatures as sanity check
                                match round_data.proposal_sigs[chunk_i].verify(proposer_pub_key, &send_buf1[signed_data_start..sig_o]) {
                                    Ok(_) => {}
                                    Err((err, str)) => {
                                        eprintln!("{ctx_str}: {ANSI_RED}BFT FAULT{ANSI_RST}: {str} [..{}): for proposal from {proposer_pub_key:?} {height}.{round}.{chunk_i}: {} {err}", sig_o-1, chunk_hdr.proposal_id);
                                        continue;
                                    }
                                }

                                if PRINT_SENDS { println!("{ctx_str} {ANSI_GRY}SENDS{ANSI_RST}: sending proposal chunk {} to {:?}", chunk_i, peer_bft_key); }
                                sent_chunk_cs += 1;
                                print_packet_tag_send(header);
                                send_stp_msg(messages_to_send, connection_key, &send_buf1[..o], stats);
                            }
                        }
                    }

                    let vote_start: u8 = if should_send_prevotes { 0 } else { 1 };
                    for is_precommit in vote_start..2 {
                        if  (is_precommit == 0 && round_data.counts.prevotes   == 0) ||
                            (is_precommit == 1 && round_data.counts.precommits == 0)
                        {
                            continue;
                        }

                        let header = PacketHeader::new_(PACKET_TYPE_PREVOTE_SIGNATURES + is_precommit);
                        let mut packet = PacketVotes {
                            height, round,
                            value_id: round_data.proposal_id,
                            no_votes_n: 0, yes_votes_n: 0,
                            votes: [ PubKeySig::NIL; 18 ],
                        };

                        fn dbg_check_votes(ctx_str: &str, roster: &[SortedRosterMember], is_precommit: usize, packet: &PacketVotes) {
                            #[cfg(debug_assertions)] // self-check signatures as sanity check
                            for i in 0..(packet.no_votes_n + packet.yes_votes_n) as usize {
                                let (roster_i, sig) = (packet.votes[i].roster_i as usize, &packet.votes[i].sig);
                                let Some(member) = roster.get(roster_i) else {
                                    eprintln!("{ctx_str}: {ANSI_RED}BFT ERROR{ANSI_RST}: {} from {roster_i} - not in roster {}.{}: {}", ["prevote", "precommit"][is_precommit], packet.height, packet.round, packet.value_id);
                                    return;
                                };
                                let pub_key = member.pub_key;
                                let sign_datas = make_vote_sign_datas(pub_key, is_precommit != 0, packet.height, packet.round, packet.value_id);
                                let sign_data  = &sign_datas[(i >= packet.no_votes_n as usize) as usize];
                                match sig.verify(pub_key, sign_data) { Ok(_)=>{} Err((err, str)) => {
                                    eprintln!("{ctx_str}: {ANSI_RED}BFT FAULT{ANSI_RST}: {str} [..{}): for {} from {roster_i}-{pub_key:?} {}.{}: {} {err}",
                                        sign_data.len(), ["prevote", "precommit"][is_precommit], packet.height, packet.round, packet.value_id);
                                }}
                            }
                        }

                        for roster_i in 0..round_data.msg_val_sigs.len() {
                            let (value_id, sig) = round_data.msg_val_sigs[roster_i][is_precommit as usize];
                            if sig != TMSig::NIL {
                                let pub_key_sig = PubKeySig{ roster_i: roster_i.try_into().unwrap(), sig };
                                // println!("{} {}: packing in sig from {}", PubKeyID(my_root_public_bft_key.into()), pub_key_sig.pub_key);

                                if packet.value_id == ValueId::NIL {
                                    // NOTE(azmr): gossip seen votes even if we haven't seen proposal, but only 1 non-nil value id per packet
                                    packet.value_id = value_id;
                                }
                                if value_id != ValueId::NIL && value_id != packet.value_id {
                                    eprintln!("{ctx_str} {ANSI_RED}BFT FAULT{ANSI_RST}: local mismatch: {:?} vs {:?}", packet.value_id, value_id);
                                    continue;
                                }

                                // add nos and yeses from opposite ends to avoid excess moves
                                if value_id == ValueId::NIL {
                                    packet.votes[packet.no_votes_n as usize] = pub_key_sig;
                                    packet.no_votes_n += 1;
                                } else {
                                    packet.yes_votes_n += 1; // *intentionally* pre-decrement because we're indexing from end
                                    packet.votes[packet.votes.len() - packet.yes_votes_n as usize] = pub_key_sig;
                                };


                                if (packet.no_votes_n + packet.yes_votes_n) as usize == packet.votes.len() {
                                    sent_c[is_precommit as usize] += (packet.no_votes_n + packet.yes_votes_n) as usize;
                                    // full evidence block; send it
                                    if PRINT_SENDS { println!("{ctx_str} {ANSI_GRY}SENDS{ANSI_RST}: sending full {} block: {:#?}", ["prevote", "precommit"][is_precommit as usize], packet); }
                                    dbg_check_votes(ctx_str, &round_data.roster, is_precommit as usize, &packet);

                                    let mut o = 0;
                                    // @Todo @Speed: We should build the header once and save it for all votes. Peers only obey the latest status anyway.
                                    o += write_header_and_maybe_status(header, true, bft_state, roster, &mut send_buf1[o..], peer.index_counter); peer.index_counter += 1;
                                    o += packet.write_to(&mut send_buf1[o..]);
                                    send_stp_msg(messages_to_send, connection_key, &send_buf1[..o], stats);

                                    packet.no_votes_n  = 0;
                                    packet.yes_votes_n = 0;
                                    packet.votes       = [PubKeySig::NIL; 18];
                                }
                            }
                        }

                        // send any half-filled vote blocks
                        if (packet.no_votes_n + packet.yes_votes_n) > 0 {
                            sent_c[is_precommit as usize] += (packet.no_votes_n + packet.yes_votes_n) as usize;
                            // println!("{} half-filled block pre-gap-close: {:#?}", packet);
                            // move items from end to fill gap
                            for gap_i in 0..packet.votes.len() - (packet.no_votes_n + packet.yes_votes_n) as usize {
                                packet.votes[packet.no_votes_n as usize + gap_i] = packet.votes[packet.votes.len() - 1 - gap_i];
                            }

                            if PRINT_SENDS { println!("{ctx_str} {ANSI_GRY}SENDS{ANSI_RST}: half-filled block post-gap-close: {:#?}", packet); }
                            dbg_check_votes(ctx_str, &round_data.roster, is_precommit as usize, &packet);

                            let mut o = 0;
                            // @Todo @Speed: We should build the header once and save it for all votes. Peers only obey the latest status anyway.
                            o += write_header_and_maybe_status(header, true, bft_state, roster, &mut send_buf1[o..], peer.index_counter); peer.index_counter += 1;
                            o += packet.write_to(&mut send_buf1[o..]);
                            // TODO: maybe status
                            print_packet_tag_send(header);
                            send_stp_msg(messages_to_send, connection_key, &send_buf1[..o], stats);
                        }
                    }

                    if PRINT_SEND_CS && sent_chunk_cs > 0 { eprintln!("{ctx_str} {ANSI_GRY}SEND_CS{ANSI_RST}: sent {} proposal chunks", sent_chunk_cs); }
                    if PRINT_SEND_CS && sent_c[0]     > 0 { eprintln!("{ctx_str} {ANSI_GRY}SEND_CS{ANSI_RST}: sent {} prevotes",        sent_c[0]);     }
                    if PRINT_SEND_CS && sent_c[1]     > 0 { eprintln!("{ctx_str} {ANSI_GRY}SEND_CS{ANSI_RST}: sent {} precommits",      sent_c[1]);     }
                }

                if PRINT_PEERS { println!("{ctx_str} {ANSI_GRY}PEERS{ANSI_RST}: {:?}", peers.iter().map(|(ck, p)| {
                    let mut bft_key = PubKeyID::NIL;
                    for (c, _) in &current_connections {
                        if c.connection_key() == *ck {
                            if let Some(k) = bft_address_map.get_key(&c) {
                                bft_key = *k;
                            }
                            break;
                        }
                    }
                    (bft_key, p.latest_status.clone())
                }).collect::<Vec<_>>()); }

                for (connection_key, peer) in &mut peers {
                    let mut peer_bft_key = PubKeyID::NIL;
                    for (c, _) in &current_connections {
                        if c.connection_key() == *connection_key {
                            if let Some(k) = bft_address_map.get_key(&c) {
                                peer_bft_key = *k;
                            }
                            break;
                        }
                    }
                    if let Some(height) = peer.latest_status_request_height && height < bft_state.height {
                        peer.latest_status_request_height = None;
                        send_round_data_to_peer(&bft_state,
                                                false,
                                                &bft_state.recent_commit_round_cache[height as usize],
                                                &ctx_str,
                                                &roster,
                                                &mut messages_to_send,
                                                &mut send_buf1,
                                                peer,
                                                connection_key,
                                                peer_bft_key,
                                                &mut net_stats);
                    }
                    else if roster_i_from_pub_key(&roster[..active_roster_len(&roster)], peer_bft_key).is_some() {
                        if let Ok(current_height_start_i) = bft_state.rounds_data.binary_search_by_key(&(bft_state.height, 0), |el| (el.height, el.round))
                        {
                            for round_i in (current_height_start_i..bft_state.rounds_data.len()).rev()
                            {
                                let round_data = &bft_state.rounds_data[round_i];
                                send_round_data_to_peer(&bft_state,
                                                        true,
                                                        &round_data,
                                                        &ctx_str,
                                                        &roster,
                                                        &mut messages_to_send,
                                                        &mut send_buf1,
                                                        peer,
                                                        connection_key,
                                                        peer_bft_key,
                                                        &mut net_stats);
                            }
                        } else {
                            eprintln!("{ctx_str} {ANSI_RED}BFT ERROR{ANSI_RST}: round_data array was empty");
                        }
                    }
                }

                // Prune attestations that expire in <60s
                let now: u64 = chrono::Utc::now().timestamp().try_into().expect("should fit in a u64");
                for map in bft_address_map.by_key.values_mut() {
                    map.retain(|stp_address, maybe_peer_attestation| {
                        let Some(peer_attestation) = maybe_peer_attestation else {
                            return true; // keep forever if None. // @Todo: @Incomplete?
                        };

                        if peer_attestation.expiry + 120 <= now {
                            return false; // prune
                        }
                        if peer_attestation.issued >= peer_attestation.expiry {
                            return false; // prune
                        }
                        if peer_attestation.issued + 60 > peer_attestation.expiry {
                            return false; // prune
                        }

                        return true; // keep
                    });
                }
                bft_address_map.by_key.retain(|bft_pk, map| map.len() > 0);

                bft_address_map.by_addr.retain(|stp_address, bft_pk| {
                    let Some(map) = bft_address_map.by_key.get(bft_pk) else {
                        return false; // prune
                    };

                    if !map.contains_key(stp_address) {
                        return false; // prune
                    }

                    return true; // keep
                });

                if std::time::Instant::now() >= next_peer_gossip {
                    let mut peer_attestations: Vec<&PeerAttestation> = bft_address_map.by_key.values().flat_map(|a| a.values()).filter_map(|o| o.as_ref()).collect();
                    peer_attestations.shuffle(&mut rand::thread_rng());
                    peer_attestations.truncate(1191 / PEER_ATTESTATION_SERIALIZED_SIZE);
                    for (connection_key, peer) in &mut peers {
                        let mut o = 0;
                        let header = PacketHeader::new::<PACKET_TYPE_PEER_ATTESTATIONS>();
                        o += write_header_and_maybe_status(header, true, &bft_state, &roster, &mut send_buf1[o..], peer.index_counter); peer.index_counter += 1;

                        for peer_attestation in &peer_attestations {
                            let attestation_len = peer_attestation.write_to(&mut send_buf1[o..]);
                            assert!(attestation_len == PEER_ATTESTATION_SERIALIZED_SIZE);
                            o += attestation_len;
                        }

                        print_packet_tag_send(header);
                        send_stp_msg(&mut messages_to_send, &connection_key, &send_buf1[..o], &mut net_stats);
                    }

                    next_peer_gossip = std::time::Instant::now() + PEER_GOSSIP_DURATION;
                }

                use rand::seq::IteratorRandom;
                if std::time::Instant::now() >= next_peer_connect {
                    let mut connection_attempts = 0;

                    let rng = &mut rand::thread_rng();

                    const MAX_PEERS_TO_CONNECT_PER_ATTEMPT: usize = 2;
                    const PEERS_TO_ASK_PUNCH:               usize = 5;

                    let mut all_addresses: Vec<(&PubKeyID, &HashMap<STPAddress, Option<PeerAttestation>>)> = bft_address_map.by_key.iter().collect(); all_addresses.shuffle(rng);
                    for (_, map) in &all_addresses {
                        // Grab a random address associated with this BFT key. @Todo: prioritize by trustworthiness and expiry time.
                        let Some((address, _)) = map.iter().choose(rng) else {
                            continue;
                        };
                        // Don't connect to myself.
                        if address.key == my_stp_keypair.public {
                            continue;
                        }
                        // Don't connect to anyone to whom I am already connected.
                        if current_connections.iter().any(|(addr, _)| address == addr) {
                            continue;
                        }

                        // Coordinate hole punch through connected peers
                        let mut o = 0;
                        let header = PacketHeader::new::<PACKET_TYPE_WANT_HOLE_PUNCH>();
                        o += write_header_and_maybe_status(header, true, &bft_state, &roster, &mut send_buf1[o..], 0);
                        o += address.connection_key().write_to(&mut send_buf1[o..]);

                        for (conn_address, _) in current_connections.iter().choose_multiple(rng, PEERS_TO_ASK_PUNCH) {
                            if PRINT_PROTOCOL { println!("{ctx_str} {ANSI_GRY}PROTOCOL{ANSI_RST}: Requesting hole punch to address {:?} via random peer: {:?}...", address, conn_address); }

                            print_packet_tag_send(header);
                            send_stp_msg(&mut messages_to_send, &conn_address.connection_key(), &send_buf1[..o], &mut net_stats);
                        }

                        // Initiate direct connection to start hole punch
                        initiate_connections.push(address.clone());
                    }

                    next_peer_connect = std::time::Instant::now() + PEER_CONNECT_DURATION;
                }

                if PRINT_NETWORK_STATS {
                    let elapsed = net_stats_window_start.elapsed();
                    let nonzero_elapsed_sec = elapsed.as_secs_f32().max(1.0);

                    let kbps = (std::cmp::max(1, net_stats.bytes_sent)   as f32) / 1000.0 / (nonzero_elapsed_sec);
                    let  pps = (std::cmp::max(1, net_stats.packets_sent) as f32)          / (nonzero_elapsed_sec);
                    let  bpp = (std::cmp::max(1, net_stats.bytes_sent)   as f32)          / (std::cmp::max(1, net_stats.packets_sent) as f32);

                    let kbps = kbps as u32;
                    let  pps =  pps as u32;
                    let  bpp =  bpp as u32;

                    if kbps != 0 {
                        println!("{ctx_str} {ANSI_GRN}NET{ANSI_RST}: {} KB/s | {} packets/s | {} bytes/packet",
                                 kbps, pps, bpp);
                    }
                }
            }

            let now_now = tokio::time::Instant::now();
            if now_now - next_tick_time > TICK_DURATION {
                next_tick_time = now_now + TICK_DURATION;
            } else {
                next_tick_time += TICK_DURATION;
            }
        }

        use rand::seq::SliceRandom;
        messages_to_send.shuffle(&mut rand::thread_rng());
        let resp = service_connections(&network_thread_handle, NetworkThreadPush { initiate_connections, wanted_connections: current_connections.clone(), send_unreliable: messages_to_send, });
        current_connections = resp.current_connections;
        initiate_connections = Vec::new();
        let mut messages_received = resp.received_unreliable_messages;
        messages_to_send = Vec::new();

        // Ensure a Peer entry exists for every active connection
        for &(ref stp_address, handshake_hash) in &current_connections {
            let mut new = false;

            let key = stp_address.connection_key();

            let peer = &mut peers.entry(key).or_insert_with(|| { new = true; Peer::default() });

            if !new {
                continue;
            }

            peer.stp_handshake_hash = handshake_hash;
            peer.stp_address = stp_address.clone();

            let verification = { // almost @Duplicate
                let pk_bytes = my_root_public_bft_key.as_ref();
                assert!(pk_bytes.len() == 32);
                let pk = PubKeyID(pk_bytes.try_into().expect("already asserted length of 32"));
                let sig = {
                    // @Duplicate
                    let hash_key_for_stp_handshake_hash = HashKey(blake3::Hasher::new_derive_key("Tenderlink ID Hello STP Handshake Hash").finalize().into());
                    assert!(peer.stp_handshake_hash.len() == 64);
                    let keyed_hash_of_stp_handshake_hash = hash_key_for_stp_handshake_hash.hash(&peer.stp_handshake_hash[..]);

                    TMSig(my_root_private_key.sign(&keyed_hash_of_stp_handshake_hash[..]).to_bytes())
                };

                PacketIdVerification { pk, sig }
            };

            let mut o = 0;

            // send hello to start verifying identity
            let header = PacketHeader::new::<PACKET_TYPE_ID_HELLO>();
            o += write_header_and_maybe_status(header, true, &bft_state, &roster, &mut send_buf1[o..], peer.index_counter); peer.index_counter += 1;
            o += verification.write_to(&mut send_buf1[o..]);

            print_packet_tag_send(header);
            send_stp_msg(&mut messages_to_send, &key, &send_buf1[..o], &mut net_stats);
        }

        // Remove peer entries for dropped connections
        peers.retain(|key, _| current_connections.iter().position(|(x, _)| x.connection_key() == *key).is_some());

        let mut connection_keys_to_disconnect = Vec::new();

        // READ
        'process_packets: while messages_received.len() > 0 {
            let (connection_key, mut peer, msg) = {
                let (key, packet) = messages_received.remove(0);
                let Some(peer) = peers.get_mut(&key)
                else {
                    continue;
                };
                (key, peer, packet)
            };

            let msg: &[u8] = &msg[..];
            if msg.len() == 0 {
                continue;
            }
            let Some((header, status, read_o)) = read_header_and_maybe_status(&msg[..])
            else {
                continue;
            };
            let packet_type = header.type_();


            if let Some(status) = status {
                peer.latest_status_request_height = Some(status.height);
                peer.latest_status = Some(status);
            }

            const_assert!(PACKET_TYPE_PREVOTE_SIGNATURES + 1 == PACKET_TYPE_PRECOMMIT_SIGNATURES);
            if packet_type == PACKET_TYPE_PROPOSAL_CHUNK {
                let Some(hdr) = PacketProposalChunkHeader::read_from(&mut &msg[read_o..]) else {
                    eprintln!("{:05}: couldn't read proposal header", my_port);
                    continue;
                };
                let proposal_size = hdr.proposal_size as usize;
                let chunk_i       = hdr.chunk_i       as usize;
                let chunk_size = usize::min(PROPOSAL_CHUNK_DATA_SIZE, proposal_size - chunk_i * PROPOSAL_CHUNK_DATA_SIZE);
                let packet_size = chunk_size + PROPOSAL_PACKET_EXTRA;

                // NOTE: assume for the moment that this is the valid height, we'll check in the subsequent call
                // ALT:  cache proposer for *current* round
                if msg.len() == packet_size {
                    if let (Some(roster_i), _) = TMState::proposer_from_height_round(&bft_state.hash_keys, &roster, hdr.height, hdr.round) {
                        let sig_o = read_o + PacketProposalChunkHeader::SERIALIZED_SIZE + chunk_size;
                        bft_state.check_and_incorporate_msg(hdr.height, hdr.round, hdr.chunk_i as usize, hdr.proposal_id, hdr.valid_round,
                            &roster, roster_i, packet_type, &msg[read_o..sig_o], TMSig(msg[sig_o..sig_o+64].try_into().unwrap()));
                    }
                } else {
                    eprintln!("{:05}: couldn't read proposal chunk: incorrect size {}", my_port, msg.len());
                }
            }

            else if packet_type == PACKET_TYPE_PREVOTE_SIGNATURES || packet_type == PACKET_TYPE_PRECOMMIT_SIGNATURES {
                if let Some(packet) = PacketVotes::read_from(&mut &msg[read_o..]) {
                    let is_precommit = packet_type - PACKET_TYPE_PREVOTE_SIGNATURES;
                    let value_ids    = [ ValueId::NIL, packet.value_id ];

                    for vote_i in 0..(packet.no_votes_n + packet.yes_votes_n) as usize {
                        // Note(Sam): We can change the format of votes to be cool and branchless after the workshop.
                        if let Some(roster_member) = roster.get(packet.votes[vote_i].roster_i as usize) {
                            let sign_datas   = make_vote_sign_datas(roster_member.pub_key, is_precommit != 0, packet.height, packet.round, packet.value_id);
                            let no_yes_i = (vote_i >= packet.no_votes_n as usize) as usize;
                            bft_state.check_and_incorporate_msg(packet.height, packet.round, 0, value_ids[no_yes_i], -2,
                                &roster, packet.votes[vote_i].roster_i as usize, packet_type, &sign_datas[no_yes_i], TMSig(packet.votes[vote_i].sig.0));
                        }
                    }
                } else {
                    eprintln!("{:05}: couldn't read {}", my_port, packet_name_from_tag(packet_type));
                }
            }

            else if packet_type == PACKET_TYPE_PEER_ATTESTATIONS {
                let chunks = msg[read_o..].chunks(PEER_ATTESTATION_SERIALIZED_SIZE);
                for chunk in chunks {
                    let Some(peer_attestation) = PeerAttestation::read_from(&mut &chunk[..]) else {
                        if PRINT_PROTOCOL { println!("{ctx_str} {ANSI_RED}PROTOCOL{ANSI_RST}: Peer sent invalid peer attestation: Failed to read peer attestation"); }
                        connection_keys_to_disconnect.push(connection_key);
                        continue 'process_packets;
                    };

                    // @Todo: prune attestees to only BFT PKs on a current or imminently upcoming roster
                    // @Todo: prune attesters to only BFT PKs on a current or imminently upcoming roster

                    let now: u64 = chrono::Utc::now().timestamp().try_into().expect("should fit in a u64");
                    if peer_attestation.expiry + 60 <= now {
                        if PRINT_PROTOCOL { println!("{ctx_str} {ANSI_RED}PROTOCOL{ANSI_RST}: Peer sent peer attestation that will expire too soon (<60s)"); }
                        connection_keys_to_disconnect.push(connection_key);
                        continue;
                    }
                    if peer_attestation.issued >= peer_attestation.expiry {
                        if PRINT_PROTOCOL { println!("{ctx_str} {ANSI_RED}PROTOCOL{ANSI_RST}: Peer sent invalid peer attestation: Issued after expired"); }
                        connection_keys_to_disconnect.push(connection_key);
                        continue;
                    }
                    if peer_attestation.issued + 60 > peer_attestation.expiry {
                        if PRINT_PROTOCOL { println!("{ctx_str} {ANSI_RED}PROTOCOL{ANSI_RST}: Peer sent invalid peer attestation: Expires less than 60 seconds after issued"); }
                        connection_keys_to_disconnect.push(connection_key);
                        continue;
                    }

                    let keyed_hash_of_one_party_signed_attestation = {
                        // @Duplicate
                        {
                            let hash_key_for_attestation = HashKey(blake3::Hasher::new_derive_key("Tenderlink One Party Signed Peer Attestation").finalize().into());

                            let mut hasher = hash_key_for_attestation.hasher();
                            hasher.update(&peer_attestation.issued.to_le_bytes()[..]);
                            hasher.update(&peer_attestation.expiry.to_le_bytes()[..]);
                            hasher.update(&peer_attestation.stp_address.ip.octets()[..]);
                            hasher.update(&peer_attestation.stp_address.port.to_le_bytes()[..]);
                            hasher.update(&peer_attestation.stp_address.magic1.to_le_bytes()[..]);
                            hasher.update(&peer_attestation.stp_address.key[..]);
                            hasher.update(&peer_attestation.attestee_bft_pk.0[..]);
                            hasher.update(&peer_attestation.attester_bft_pk.0[..]);
                            let keyed_hash_of_one_party_signed_attestation = hasher.finalize();

                            match peer_attestation.attester_sig.verify(peer_attestation.attester_bft_pk, &keyed_hash_of_one_party_signed_attestation.as_bytes()[..]) { Ok(()) => {}, Err((err, str)) => {
                                if PRINT_PROTOCOL { println!("{ctx_str} {ANSI_RED}PROTOCOL{ANSI_RST}: Peer sent invalid peer attestation: Attester signature invalid"); }
                                connection_keys_to_disconnect.push(connection_key);
                                continue 'process_packets;
                            } };

                            keyed_hash_of_one_party_signed_attestation
                        }
                    };
                    {
                        let hash_key_for_attestation = HashKey(blake3::Hasher::new_derive_key("Tenderlink Two Party Signed Peer Attestation").finalize().into());
                        let keyed_hash_of_two_party_signed_attestation = hash_key_for_attestation.hash(&peer_attestation.attester_sig.0[..]);
                        match peer_attestation.attestee_sig.verify(peer_attestation.attestee_bft_pk, &keyed_hash_of_two_party_signed_attestation[..]) { Ok(()) => {}, Err((err, str)) => {
                            if PRINT_PROTOCOL { println!("{ctx_str} {ANSI_RED}PROTOCOL{ANSI_RST}: Peer sent invalid peer attestation: Attestee signature invalid"); }
                            connection_keys_to_disconnect.push(connection_key);
                            continue 'process_packets;
                        } };
                    }

                    bft_address_map.insert(&peer_attestation.attestee_bft_pk.clone(), &peer_attestation.stp_address.clone(), Some(peer_attestation));
                }
            }

            else if packet_type == PACKET_TYPE_ID_HELLO {

                let Some(their_verification) = PacketIdVerification::read_from(&mut &msg[read_o..]) else {
                    if PRINT_PROTOCOL { println!("{ctx_str} {ANSI_RED}PROTOCOL{ANSI_RST}: Peer failed ID verification: Failed to read ID Hello packet"); }
                    connection_keys_to_disconnect.push(connection_key);
                    continue;
                };
                {
                    // @Duplicate
                    let hash_key_for_stp_handshake_hash = HashKey(blake3::Hasher::new_derive_key("Tenderlink ID Hello STP Handshake Hash").finalize().into());
                    assert!(peer.stp_handshake_hash.len() == 64);
                    let keyed_hash_of_stp_handshake_hash = hash_key_for_stp_handshake_hash.hash(&peer.stp_handshake_hash[..]);

                    match their_verification.sig.verify(their_verification.pk, &keyed_hash_of_stp_handshake_hash[..]) { Ok(()) => {}, Err((err, str)) => {
                        if PRINT_PROTOCOL { println!("{ctx_str} {ANSI_RED}PROTOCOL{ANSI_RST}: Peer failed ID verification: Handshake hash signature invalid"); }
                        // if PRINT_PROTOCOL { println!("{ctx_str} {ANSI_RED}PROTOCOL{ANSI_RST}: Handshake hash is: {:?}", peer.stp_handshake_hash); }
                        connection_keys_to_disconnect.push(connection_key);
                        continue;
                    } };
                }

                bft_address_map.insert(&their_verification.pk, &peer.stp_address, None);
                peer.bft_pk = their_verification.pk;

                let my_verification = { // almost @Duplicate
                    let pk_bytes = my_root_public_bft_key.as_ref();
                    assert!(pk_bytes.len() == 32);
                    let pk = PubKeyID(pk_bytes.try_into().expect("already asserted length of 32"));
                    let sig = {
                        // @Duplicate
                        let hash_key_for_stp_handshake_hash = HashKey(blake3::Hasher::new_derive_key("Tenderlink ID Hello-Ack STP Handshake Hash").finalize().into());
                        assert!(peer.stp_handshake_hash.len() == 64);
                        let keyed_hash_of_stp_handshake_hash = hash_key_for_stp_handshake_hash.hash(&peer.stp_handshake_hash[..]);

                        TMSig(my_root_private_key.sign(&keyed_hash_of_stp_handshake_hash[..]).to_bytes())
                    };

                    PacketIdVerification { pk, sig }
                };

                let attestation = { // @Duplicate
                    let addr = peer.stp_address.clone();
                    let issued: u64 = chrono::Utc::now().timestamp().try_into().expect("should fit in a u64");
                    let expiry = {
                        let seconds_per_minute = 60;
                        let minutes_per_hour   = 60;
                        let hours_per_day      = 24;
                        let days_expiry        =  1;

                        issued + (seconds_per_minute * minutes_per_hour * hours_per_day * days_expiry)
                    };
                    let sig = {
                        let hash_key_for_attestation = HashKey(blake3::Hasher::new_derive_key("Tenderlink One Party Signed Peer Attestation").finalize().into());

                        let mut hasher = hash_key_for_attestation.hasher();
                        hasher.update(&issued.to_le_bytes()[..]);
                        hasher.update(&expiry.to_le_bytes()[..]);
                        hasher.update(&addr.ip.octets()[..]);
                        hasher.update(&addr.port.to_le_bytes()[..]);
                        hasher.update(&addr.magic1.to_le_bytes()[..]);
                        hasher.update(&addr.key[..]);
                        hasher.update(&peer.bft_pk.0[..]);
                        hasher.update(&my_root_public_bft_key.as_ref()[..]);
                        let keyed_hash_of_one_party_signed_attestation = hasher.finalize();

                        TMSig(my_root_private_key.sign(&keyed_hash_of_one_party_signed_attestation.as_bytes()[..]).to_bytes())
                    };

                    PacketIdAttestation { issued, expiry, addr, sig }
                };

                let mut o = 0;

                // send hello to start verifying identity
                let header = PacketHeader::new::<PACKET_TYPE_ID_HELLO_ACK>();
                o += write_header_and_maybe_status(header, true, &bft_state, &roster, &mut send_buf1[o..], peer.index_counter); peer.index_counter += 1;
                o += my_verification.write_to(&mut send_buf1[o..]);
                o += attestation    .write_to(&mut send_buf1[o..]);

                print_packet_tag_send(header);
                send_stp_msg(&mut messages_to_send, &connection_key, &send_buf1[..o], &mut net_stats);
            }
            else if packet_type == PACKET_TYPE_ID_HELLO_ACK {

                let msg = &mut &msg[read_o..];

                let Some(their_verification) = PacketIdVerification::read_from(msg) else {
                    if PRINT_PROTOCOL { println!("{ctx_str} {ANSI_RED}PROTOCOL{ANSI_RST}: Peer failed ID verification: Failed to read ID Hello Ack packet: Failed to read verification"); }
                    connection_keys_to_disconnect.push(connection_key);
                    continue;
                };
                {
                    // @Duplicate
                    let hash_key_for_stp_handshake_hash = HashKey(blake3::Hasher::new_derive_key("Tenderlink ID Hello-Ack STP Handshake Hash").finalize().into());
                    assert!(peer.stp_handshake_hash.len() == 64);
                    let keyed_hash_of_stp_handshake_hash = hash_key_for_stp_handshake_hash.hash(&peer.stp_handshake_hash[..]);

                    match their_verification.sig.verify(their_verification.pk, &keyed_hash_of_stp_handshake_hash[..]) { Ok(()) => {}, Err((err, str)) => {
                        if PRINT_PROTOCOL { println!("{ctx_str} {ANSI_RED}PROTOCOL{ANSI_RST}: Peer failed ID verification: Handshake hash signature invalid"); }
                        connection_keys_to_disconnect.push(connection_key);
                        continue;
                    } };
                }

                bft_address_map.insert(&their_verification.pk, &peer.stp_address, None);
                peer.bft_pk = their_verification.pk;

                let Some(attestation) = PacketIdAttestation::read_from(msg) else {
                    if PRINT_PROTOCOL { println!("{ctx_str} {ANSI_RED}PROTOCOL{ANSI_RST}: Peer failed ID verification: Failed to read ID Hello Ack packet: Failed to read attestation"); }
                    connection_keys_to_disconnect.push(connection_key);
                    continue;
                };

                #[cfg(debug_assertions)]
                if false
                {
                    eprintln!("attestation:            {:?}",  attestation);
                    eprintln!("attestation.addr:       {:?}",  attestation.addr);
                    eprintln!("attestation.addr.magic: {:x?}", attestation.addr.magic1);
                    eprintln!("attestation.addr.key:   {:?}",  attestation.addr.key);
                    eprintln!("my_stp_keypair:         {:?}",  my_stp_keypair);
                    eprintln!("my_stp_keypair.magic1:  {:x?}", my_stp_keypair.magic1);
                    eprintln!("my_stp_keypair.key:     {:?}",  my_stp_keypair.public);
                }
                // @Todo: list of multiple listen keypairs
                if attestation.addr.magic1 != my_stp_keypair.magic1 ||
                   attestation.addr.key    != my_stp_keypair.public {
                    if PRINT_PROTOCOL { println!("{ctx_str} {ANSI_RED}PROTOCOL{ANSI_RST}: Peer failed ID attestation: STP address is not my address"); }
                    connection_keys_to_disconnect.push(connection_key);
                    continue;
                }
                let now: u64 = chrono::Utc::now().timestamp().try_into().expect("should fit in a u64");
                if attestation.expiry <= now {
                    if PRINT_PROTOCOL { println!("{ctx_str} {ANSI_RED}PROTOCOL{ANSI_RST}: Peer failed ID attestation: Attestation has already expired"); }
                    connection_keys_to_disconnect.push(connection_key);
                    continue;
                }
                if attestation.issued >= attestation.expiry {
                    if PRINT_PROTOCOL { println!("{ctx_str} {ANSI_RED}PROTOCOL{ANSI_RST}: Peer failed ID attestation: Issued after expired"); }
                    connection_keys_to_disconnect.push(connection_key);
                    continue;
                }
                if attestation.issued + 60 > attestation.expiry {
                    if PRINT_PROTOCOL { println!("{ctx_str} {ANSI_RED}PROTOCOL{ANSI_RST}: Peer failed ID attestation: Expires less than 60 seconds after issued"); }
                    connection_keys_to_disconnect.push(connection_key);
                    continue;
                }
                let keyed_hash_of_one_party_signed_attestation = {
                    // @Duplicate
                    let addr = attestation.addr.clone();
                    let issued = attestation.issued;
                    let expiry = attestation.expiry;
                    {
                        let hash_key_for_attestation = HashKey(blake3::Hasher::new_derive_key("Tenderlink One Party Signed Peer Attestation").finalize().into());

                        let mut hasher = hash_key_for_attestation.hasher();
                        hasher.update(&issued.to_le_bytes()[..]);
                        hasher.update(&expiry.to_le_bytes()[..]);
                        hasher.update(&addr.ip.octets()[..]);
                        hasher.update(&addr.port.to_le_bytes()[..]);
                        hasher.update(&addr.magic1.to_le_bytes()[..]);
                        hasher.update(&addr.key[..]);
                        hasher.update(&my_root_public_bft_key.as_ref()[..]);
                        hasher.update(&peer.bft_pk.0[..]);
                        let keyed_hash_of_one_party_signed_attestation = hasher.finalize();

                        match attestation.sig.verify(their_verification.pk, &keyed_hash_of_one_party_signed_attestation.as_bytes()[..]) { Ok(()) => {}, Err((err, str)) => {
                            if PRINT_PROTOCOL { println!("{ctx_str} {ANSI_RED}PROTOCOL{ANSI_RST}: Peer failed ID attestation: Attestation signature invalid"); }
                            connection_keys_to_disconnect.push(connection_key);
                            continue;
                        } };

                        keyed_hash_of_one_party_signed_attestation
                    }
                };

                // Sign their signed attestation with my signature
                let sig = {
                    let hash_key_for_attestation = HashKey(blake3::Hasher::new_derive_key("Tenderlink Two Party Signed Peer Attestation").finalize().into());
                    assert!(attestation.sig.0.len() == 64);
                    let keyed_hash_of_two_party_signed_attestation = hash_key_for_attestation.hash(&attestation.sig.0[..]);
                    TMSig(my_root_private_key.sign(&keyed_hash_of_two_party_signed_attestation[..]).to_bytes())
                };

                let peer_attestation = PeerAttestation {
                    stp_address:        attestation.addr,
                    issued:             attestation.issued,
                    expiry:             attestation.expiry,
                    attester_bft_pk:    peer.bft_pk,
                    attestee_bft_pk:    PubKeyID(my_root_public_bft_key.as_ref().try_into().expect("VerificationKeyBytes should be 32 bytes")),
                    attester_sig:       attestation.sig,
                    attestee_sig:       sig,
                };
                my_address_attestations.push(peer_attestation.clone());
                bft_address_map.insert(&peer_attestation.attestee_bft_pk, &peer_attestation.stp_address, Some(peer_attestation.clone()));

                // @Todo: Decide if @Temporary?
                // if false
                {
                    let mut o = 0;
                    let header = PacketHeader::new::<PACKET_TYPE_PEER_ATTESTATIONS>();
                    o += write_header_and_maybe_status(header, true, &bft_state, &roster, &mut send_buf1[o..], peer.index_counter); peer.index_counter += 1;
                    let attestation_len = peer_attestation.write_to(&mut send_buf1[o..]);
                    assert!(attestation_len == PEER_ATTESTATION_SERIALIZED_SIZE);
                    o += attestation_len;
                    print_packet_tag_send(header);
                    send_stp_msg(&mut messages_to_send, &connection_key, &send_buf1[..o], &mut net_stats);
                }
            }

            else if packet_type == PACKET_TYPE_WANT_HOLE_PUNCH {
                // @Todo: rate limit consumption

                let msg = &mut &msg[read_o..];

                let Some(relay_to_connection_key) = ConnectionKey::read_from(msg) else {
                    if PRINT_PROTOCOL { println!("{ctx_str} {ANSI_RED}PROTOCOL{ANSI_RST}: Disconnecting from peer: Connection key read failed"); } // @Todo: better error message.
                    connection_keys_to_disconnect.push(connection_key);
                    continue 'process_packets;
                };

                if current_connections.iter().any(|(addr, _)| addr.connection_key() == relay_to_connection_key) {
                    let mut o = 0;
                    let header = PacketHeader::new::<PACKET_TYPE_TRY_HOLE_PUNCH>();
                    o += write_header_and_maybe_status(header, true, &bft_state, &roster, &mut send_buf1[o..], peer.index_counter); peer.index_counter += 1;
                    o += peer.stp_address.write_to(&mut send_buf1[o..]);

                    if PRINT_PROTOCOL { println!("{ctx_str} {ANSI_GRY}PROTOCOL{ANSI_RST}: Relaying hole punch request from {:?} to {:?}...", peer.stp_address, relay_to_connection_key); }
                    print_packet_tag_send(header);
                    send_stp_msg(&mut messages_to_send, &relay_to_connection_key, &send_buf1[..o], &mut net_stats);
                }

            } else if packet_type == PACKET_TYPE_TRY_HOLE_PUNCH {
                // @Todo: rate limit consumption

                let msg = &mut &msg[read_o..];

                let Some(address_to_punch_to) = STPAddress::read_from(msg) else {
                    if PRINT_PROTOCOL { println!("{ctx_str} {ANSI_RED}PROTOCOL{ANSI_RST}: Disconnecting from peer: Address read failed"); } // @Todo: better error message.
                    connection_keys_to_disconnect.push(connection_key);
                    continue 'process_packets;
                };

                if !current_connections.iter().any(|(addr, _)| *addr == address_to_punch_to) {
                    if PRINT_PROTOCOL { println!("{ctx_str} {ANSI_GRY}PROTOCOL{ANSI_RST}: Attempting hole punch to {:?}, requested by {:?}...", address_to_punch_to, peer.stp_address); }
                    initiate_connections.push(address_to_punch_to);
                }
            }

            else {
            }

            if let Some(pk) = bft_address_map.get_key(&peer.stp_address) {
                bft_address_map.last_packet_utcs.insert(*pk, chrono::Utc::now().timestamp());
            }
        }

        current_connections.retain(|(address, _)| !connection_keys_to_disconnect.contains(&address.connection_key()));

        // @Todo(Phillip): How long should this be?
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

// networking

// ID verification - BFT pk + BFT sig of noise handshake hash
// ID attestation  - my view of your ip:port + BFT sig of {your ip,your port,our magic1,your key,your BFT pk, my BFT pk}

const PACKET_TYPE_EMPTY:            u8 = 0;

// ID hello         - ID verification
// ID hello ack     - ID verification + ID attestation
// ID hello ack ack -                   ID attestation

const PACKET_TYPE_ID_HELLO:         u8 = 1;
const PACKET_TYPE_ID_HELLO_ACK:     u8 = 2;
const PACKET_TYPE_ID_HELLO_ACK_ACK: u8 = 3;

const PACKET_TYPE_PEER_ATTESTATIONS: u8 = 4;
const PACKET_TYPE_WANT_HOLE_PUNCH:   u8 = 5;
const PACKET_TYPE_TRY_HOLE_PUNCH:    u8 = 6;

// consensus
const PACKET_TYPE_PROPOSAL_CHUNK:       u8 =  7;
const PACKET_TYPE_PREVOTE_SIGNATURES:   u8 =  8;
const PACKET_TYPE_PRECOMMIT_SIGNATURES: u8 =  9;

// misc
const PACKET_TYPE_COUNT:                u8 = 11;


const PACKET_TYPE_BITS:                 u8 =  7;
const PACKET_TYPE_MASK:                 u8 = (1 << PACKET_TYPE_BITS) - 1;

const PACKET_TAG_STATUS_SHIFT:          u8 = PACKET_TYPE_BITS;
const PACKET_TAG_STATUS_FLAG:           u8 = 1 << PACKET_TAG_STATUS_SHIFT;

const PACKET_TAG_BITS:                  u8 = 8;
const PACKET_TAG_MASK:                  u8 = ((1 << PACKET_TAG_BITS as u64) - 1) as u8;


const PACKET_TYPE_NAMES: [[&str; 2]; PACKET_TYPE_COUNT as usize] = {
    let mut names = [["<MISSING>", "STATUS+<MISSING>"]; PACKET_TYPE_COUNT as usize];
    names[PACKET_TYPE_EMPTY                as usize] = ["EMPTY",                    "STATUS+EMPTY"];

    names[PACKET_TYPE_ID_HELLO             as usize] = ["ID_HELLO",                 "STATUS+ID_HELLO"];
    names[PACKET_TYPE_ID_HELLO_ACK         as usize] = ["ID_HELLO_ACK",             "STATUS+ID_HELLO_ACK"];
    names[PACKET_TYPE_ID_HELLO_ACK_ACK     as usize] = ["ID_HELLO_ACK_ACK",         "STATUS+ID_HELLO_ACK_ACK"];

    names[PACKET_TYPE_PEER_ATTESTATIONS    as usize] = ["PEER_ATTESTATIONS",        "STATUS+PEER_ATTESTATIONS"];
    names[PACKET_TYPE_WANT_HOLE_PUNCH      as usize] = ["WANT_HOLE_PUNCH",          "STATUS+WANT_HOLE_PUNCH"];
    names[PACKET_TYPE_TRY_HOLE_PUNCH       as usize] = ["TRY_HOLE_PUNCH",           "STATUS+TRY_HOLE_PUNCH"];

    names[PACKET_TYPE_PROPOSAL_CHUNK       as usize] = ["PROPOSAL_CHUNK",           "STATUS+PROPOSAL_CHUNK"];
    names[PACKET_TYPE_PREVOTE_SIGNATURES   as usize] = ["PREVOTE_SIGNATURES",       "STATUS+PREVOTE_SIGNATURES"];
    names[PACKET_TYPE_PRECOMMIT_SIGNATURES as usize] = ["PRECOMMIT_SIGNATURES",     "STATUS+PRECOMMIT_SIGNATURES"];
    const_assert!(PACKET_TYPE_COUNT == 11); // keep names array updated when adding other tags
    names
};
fn packet_name_from_tag(packet_tag: u8) -> &'static str {
    PACKET_TYPE_NAMES.get((packet_tag & PACKET_TYPE_MASK) as usize).unwrap_or(&["<UNKNOWN>", "STATUS+<UNKNOWN>"])[(packet_tag >> PACKET_TAG_STATUS_SHIFT & 1) as usize]
}
fn print_packet_tag_send(header: PacketHeader) {
    if PRINT_PROTOCOL_TAG { println!("PROTOCOL_TAG: PACKET_{} (0x{:X}) ->", packet_name_from_tag(header.tag()), header.tag()); }
}
fn print_packet_tag_recv(header: PacketHeader) {
    if PRINT_PROTOCOL_TAG { println!("PROTOCOL_TAG: <- PACKET_{} (0x{:X})", packet_name_from_tag(header.tag()), header.tag()); }
}

// NOTE(azmr): could add packet sizes so we can check all sizes in 1 location

// ALT: if we limit to u16 chunk indexes & have ~1KB chunk data per packet, we could have block sizes up to ~65MB
// N.B. with ranges like this, we either want to be half-exclusive & not allow type::MAX values, or use a special value for empty (e.g. hi < lo)
type ProposalRng = [u32; 2]; // [lo, hi)
type VoteRng     = [u16; 2];
#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BlockHash(pub [u8; 32]);
impl BlockHash { const NIL: Self = Self([0; 32]); }
const STATUS_PROPOSAL_RNGS_N: usize = 1;
const STATUS_VOTE_RNGS_N: usize = 1; // ALT: split prevote/precommit numbers
#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PacketStatus {
    height: u64,
    round:  u32, // as context for following request ranges
    need_proposal_chunk_rngs: [ProposalRng; STATUS_PROPOSAL_RNGS_N],
    need_vote_rngs: [[VoteRng; STATUS_VOTE_RNGS_N]; 2], // 1 for prevote, 1 for precommi
}
impl PacketStatus {
    pub fn write_to(&self, buf: &mut[u8]) -> usize {
        let mut o = 0;
        o += self.height.write_to(&mut buf[o..]);
        o += self.round .write_to(&mut buf[o..]);
        for chunk_rng in &self.need_proposal_chunk_rngs {
            o += chunk_rng[0].write_to(&mut buf[o..]);
            o += chunk_rng[1].write_to(&mut buf[o..]);
        }
        for is_precommit in 0..2 {
            for vote_rng in &self.need_vote_rngs[is_precommit] {
                o += vote_rng[0].write_to(&mut buf[o..]);
                o += vote_rng[1].write_to(&mut buf[o..]);
            }
        }
        o
    }

    pub fn read_from(buf: &mut &[u8]) -> Option<Self> {
        let mut packet = PacketStatus {
            height: u64::read_from(buf)?,
            round:  u32::read_from(buf)?,
            ..Default::default()
        };
        for chunk_rng in &mut packet.need_proposal_chunk_rngs {
            chunk_rng[0] = u32::read_from(buf)?;
            chunk_rng[1] = u32::read_from(buf)?;
        }
        for is_precommit in 0..2 {
            for vote_rng in &mut packet.need_vote_rngs[is_precommit] {
                vote_rng[0] = u16::read_from(buf)?;
                vote_rng[1] = u16::read_from(buf)?;
            }
        }
        Some(packet)
    }
}

const PACKET_HEADER_SIZE: usize = 8 + 8; // 16
const PACKET_STATUS_SIZE: usize = 8 /*height*/ + 4 /*round*/ + STATUS_PROPOSAL_RNGS_N * 8 + 2 * STATUS_VOTE_RNGS_N * 4; // 28
#[derive(Debug, Clone, Copy)]
pub struct PacketHeader {
    tag: u64,
}
impl PacketHeader {
    const fn assert_valid_tag<const TAG: u8>() {
        assert!((TAG & ! PACKET_TYPE_MASK) == 0);
    }

    pub fn new<const TAG: u8>() -> PacketHeader {
        Self::assert_valid_tag::<TAG>();
        Self::new_(TAG)
    }
    pub fn new_(tag: u8) -> PacketHeader {
        PacketHeader { tag: tag as u64 }
    }

    pub fn has_status(&self) -> bool { (self.tag as u8) & PACKET_TAG_STATUS_FLAG != 0 }
    pub fn type_     (&self) -> u8   { (self.tag as u8) & PACKET_TYPE_MASK            }
    pub fn tag       (&self) -> u8   { (self.tag as u8) & PACKET_TAG_MASK             }

    pub fn write_to(&self, buf: &mut [u8]) -> usize {
        let mut o = 0;
        o += self.tag.write_to(&mut buf[o..]);
        o += 0u64.write_to(&mut buf[o..]);
        o
    }

    pub fn read_from(buf: &mut &[u8]) -> Option<Self> {
        let tag = u64::read_from(buf)?;
        let _   = u64::read_from(buf)?;
        Some(Self { tag })
    }
}

#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PubKeySig { roster_i: u16, sig: TMSig, }
impl PubKeySig { const NIL: Self = Self{ roster_i: u16::MAX, sig: TMSig::NIL }; }

// ALT: common consensus packet header: { packet header, height, round, value_id }

// agnostic to prevote/precommit - communicated elsewhere
// NOTE: all votes for the same value_id (or nil)
// #[repr(C)]
#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PacketVotes {
    // header
    no_votes_n:  u8,
    yes_votes_n: u8,
    // pad_:     u16, // TODO: useful?
    round:      u32,
    height:     u64,
    value_id:   ValueId,
    // TODO: use u16 roster_idxs instead of pub_keys
    votes:    [PubKeySig; 18],
}
const_assert!(size_of::<PacketVotes>() == 1240); // TODO(azmr): exactly how much space is left
                                                 // after noise/nonce/ECC/...?
                                                 // TODO(phil): figure out the padding here

impl PacketVotes {
    fn write_to(&self, buf: &mut [u8]) -> usize {
        let mut o = 0;
        o += self.no_votes_n .write_to(&mut buf[o..]);
        o += self.yes_votes_n.write_to(&mut buf[o..]);
        o += self.round      .write_to(&mut buf[o..]);
        o += self.height     .write_to(&mut buf[o..]);
        o += self.value_id.0 .write_to(&mut buf[o..]);
        // NOTE(azmr): slight saving of bytes-on-wire if unused? i.e. initial few times each
        for i in 0..(self.no_votes_n + self.yes_votes_n) as usize {
            o += &self.votes[i].roster_i.write_to(&mut buf[o..]);
            o += &self.votes[i].sig   .0.write_to(&mut buf[o..]);
        }
        o
    }

    pub fn read_from(buf: &mut &[u8]) -> Option<Self> {
        let mut packet = PacketVotes {
            no_votes_n:   u8::read_from(buf)?,
            yes_votes_n:  u8::read_from(buf)?,
            round:       u32::read_from(buf)?,
            height:      u64::read_from(buf)?,
            value_id:    ValueId(SliceRead::read_from(buf)?),
            ..Default::default()
        };
        for i in 0..(packet.no_votes_n + packet.yes_votes_n) as usize {
            packet.votes[i].roster_i = u16::read_from(buf)?;
            packet.votes[i].sig.0    = SliceRead::read_from(buf)?;
        }
        Some(packet)
    }
}

#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PacketIdVerification {
    pub pk:  PubKeyID,
    pub sig: TMSig,
}
impl SliceWrite for PacketIdVerification {
    fn write_to(&self, buf: &mut [u8]) -> usize {
        let mut o = 0;
        o += self.pk .0.write_to(&mut buf[o..]);
        o += self.sig.0.write_to(&mut buf[o..]);
        o
    }
}
impl SliceRead for PacketIdVerification {
    fn read_from(buf: &mut &[u8]) -> Option<Self> {
        Some(Self {
            pk:  SliceRead::read_from(buf)?,
            sig: SliceRead::read_from(buf)?,
        })
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PacketIdAttestation {
    pub issued: u64, // Unix timestamp
    pub expiry: u64, // Unix timestamp
    pub addr:   STPAddress,
    pub sig:    TMSig,
}
impl SliceWrite for PacketIdAttestation {
    fn write_to(&self, buf: &mut [u8]) -> usize {
        let mut o = 0;
        o += self.addr  .write_to(&mut buf[o..]);
        o += self.issued.write_to(&mut buf[o..]);
        o += self.expiry.write_to(&mut buf[o..]);
        o += self.sig.0 .write_to(&mut buf[o..]);
        o
    }
}
impl SliceRead for PacketIdAttestation {
    fn read_from(buf: &mut &[u8]) -> Option<Self> {
        Some(Self {
            addr:   SliceRead::read_from(buf)?,
            issued: SliceRead::read_from(buf)?,
            expiry: SliceRead::read_from(buf)?,
            sig:    SliceRead::read_from(buf)?,
        })
    }
}

#[derive(Debug, Default,       Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PeerAttestation {
    pub stp_address:     STPAddress,
    pub issued:          u64,
    pub expiry:          u64,
    pub attester_bft_pk: PubKeyID,
    pub attestee_bft_pk: PubKeyID,
    pub attester_sig:    TMSig,
    pub attestee_sig:    TMSig,
}
impl SliceWrite for PeerAttestation {
    fn write_to(&self, buf: &mut [u8]) -> usize {
        let mut o = 0;
        o += self.stp_address      .write_to(&mut buf[o..]);
        o += self.issued           .write_to(&mut buf[o..]);
        o += self.expiry           .write_to(&mut buf[o..]);
        o += self.attester_bft_pk.0.write_to(&mut buf[o..]);
        o += self.attestee_bft_pk.0.write_to(&mut buf[o..]);
        o += self.attester_sig   .0.write_to(&mut buf[o..]);
        o += self.attestee_sig   .0.write_to(&mut buf[o..]);
        o
    }
}
impl SliceRead for PeerAttestation {
    fn read_from(buf: &mut &[u8]) -> Option<Self> {
        Some(Self {
            stp_address:     SliceRead::read_from(buf)?,
            issued:          SliceRead::read_from(buf)?,
            expiry:          SliceRead::read_from(buf)?,
            attester_bft_pk: SliceRead::read_from(buf)?,
            attestee_bft_pk: SliceRead::read_from(buf)?,
            attester_sig:    SliceRead::read_from(buf)?,
            attestee_sig:    SliceRead::read_from(buf)?,
        })
    }
}

pub const PEER_ATTESTATION_SERIALIZED_SIZE: usize = // @Volatile.
    STP_ADDRESS_SERIALIZED_SIZE /* stp_address */ +
    8 /* issued */ +
    8 /* expiry */ +
    32 /* attester_bft_pk */ +
    32 /* attestee_bft_pk */ +
    64 /* attester_sig */ +
    64 /* attestee_sig */ +
    0;


pub const PROPOSAL_PACKET_EXTRA:    usize = (PACKET_HEADER_SIZE + PACKET_STATUS_SIZE + 56 + 64);
pub const PROPOSAL_CHUNK_DATA_SIZE: usize = PATH_MTU - PROPOSAL_PACKET_EXTRA;

// NOTE(azmr): this is:
// - conservative in terms of max chunks, value_id, & arrival order
// - assuming a fixed total proposal size
#[derive(Debug)]
pub struct PacketProposalChunkHeader {
    // header
    chunk_i:       u32,
    proposal_size: u32,
    round:         u32,
    valid_round:   i64, // serialized as u32 with 0xff.ff for -1
    height:        u64,
    proposal_id:   ValueId, // for the total proposal, not just this chunk
    // data:        [u8; 1087], // 1200-113
    // proposer_signature: TMSig,
}
impl PacketProposalChunkHeader {
    const SERIALIZED_SIZE: usize = 4 * 4 + 8 + 32; // 56

    fn write_to(&self, buf: &mut [u8]) -> usize {
        let valid_round: u32 = if self.valid_round >= 0 { self.valid_round.try_into().unwrap() } else { u32::MAX };

        let mut o = 0;
        o        += self.chunk_i      .write_to(&mut buf[o..]);
        o        += self.proposal_size.write_to(&mut buf[o..]);
        o        += self.round        .write_to(&mut buf[o..]);
        o        += valid_round       .write_to(&mut buf[o..]);
        o        += self.height       .write_to(&mut buf[o..]);
        o        += self.proposal_id.0.write_to(&mut buf[o..]);
        // self.data                .write_to(&mut buf[48..]);
        // self.proposer_signature.0.write_to(&mut buf[1135..]);
        o
    }

    pub fn read_from(buf: &mut &[u8]) -> Option<Self> {
        Some(PacketProposalChunkHeader {
            chunk_i:       u32::read_from(buf)?,
            proposal_size: u32::read_from(buf)?,
            round:         u32::read_from(buf)?,
            valid_round:   if let v = u32::read_from(buf)? && v != u32::MAX { v.into() } else { -1 },
            height:        u64::read_from(buf)?,
            proposal_id:   ValueId(SliceRead::read_from(buf)?),
        })
    }
}

use std::collections::HashMap;


fn hook_fail_on_panic() {
    std::panic::set_hook(Box::new(|panic_info| {
        #[allow(clippy::print_stderr)]
        {
            use std::backtrace::*;
            let bt = Backtrace::force_capture();

            eprintln!("\n\n{panic_info}\n");

            // hacky formatting - BacktraceFmt not working for some reason...
            let str = format!("{bt}");
            let splits: Vec<_> = str.split("\n").collect();

            // skip over the internal backtrace unwind steps
            let mut start_i = 0;
            let mut i = 0;
            while i < splits.len() {
                if splits[i].ends_with("rust_begin_unwind") {
                    i += 1;
                    if i < splits.len() && splits[i].trim().starts_with("at ") {
                        i += 1;
                    }
                    start_i = i;
                }
                if splits[i].ends_with("core::panicking::panic_fmt") {
                    i += 1;
                    if i < splits.len() && splits[i].trim().starts_with("at ") {
                        i += 1;
                    }
                    start_i = i;
                    break;
                }
                i += 1;
            }

            // print backtrace
            let mut i = start_i;
            let n = 80;
            while i < n {
                let proc = if let Some(val) = splits.get(i) {
                    val.trim()
                } else {
                    break;
                };
                i += 1;

                let file_loc = if let Some(val) = splits.get(i) {
                    let val = val.trim();
                    if val.starts_with("at ") {
                        i += 1;
                        val
                    } else {
                        ""
                    }
                } else {
                    break;
                };

                eprintln!(
                    "  {}{}    {}",
                    if i < 20 { " " } else { "" },
                    proc,
                    file_loc
                );
            }

///////////////////////////////////////////////////////////
//   ____       _      _   _   ___    ____   _   _   _   //
//  |  _ \     / \    | \ | | |_ _|  / ___| | | | | | |  //
//  | |_) |   / _ \   |  \| |  | |  | |     | | | | | |  //
//  |  __/   / ___ \  | |\  |  | |  | |___  |_| |_| |_|  //
//  |_|     /_/   \_\ |_| \_| |___|  \____| (_) (_) (_)  //
//                                                       //
///////////////////////////////////////////////////////////

// The code panicked. Look down the call stack to see where!

            if i == n {
                eprintln!("...");
            }

            std::process::abort();

///////////////////////////////////////////////////////////
//   ____       _      _   _   ___    ____   _   _   _   //
//  |  _ \     / \    | \ | | |_ _|  / ___| | | | | | |  //
//  | |_) |   / _ \   |  \| |  | |  | |     | | | | | |  //
//  |  __/   / ___ \  | |\  |  | |  | |___  |_| |_| |_|  //
//  |_|     /_/   \_\ |_| \_| |___|  \____| (_) (_) (_)  //
//                                                       //
///////////////////////////////////////////////////////////
        }
    }))
}

pub fn run_instances(i: usize) {
    let rt = tokio::runtime::Runtime::new().unwrap();

    let seed: u64 = {
        const RNG_SEED_FOR_MULTIPROCESS: u64 = 0xdeadbeef12345;
        RNG_SEED_FOR_MULTIPROCESS
    };

    const N: usize = 4;

    let mut crypto_rng = ChaCha20Rng::seed_from_u64(seed);
    let bft_kps : Vec<_> = (0..N).map(|_| {
        // NOTE: doing this manually to avoid CryptoRng incompatibilities between different rand_core versions
        let mut secret_key = [0u8; 32];
        crypto_rng.fill_bytes(&mut secret_key);
        SigningKey::from(secret_key)
    }).collect();
    let bft_pks : Vec<_> = bft_kps.iter().map(|sk| PubKeyID(sk.verification_key().into())).collect();
    let mut cumulative_stake = 0;
    let roster : Vec<SortedRosterMember> = bft_pks.iter().enumerate().map(|(i, pk)| {
        let stake = 2000 * (N - 1 - i) as u64;
        cumulative_stake += stake;
        SortedRosterMember { pub_key: *pk, stake, cumulative_stake }
    }).collect();
    assert!(roster.is_sorted_by(|a,b| a.stake >= b.stake)); // descending

    if PRINT_ROSTER { println!("Roster: {:?}", roster); }

    let stp_key_zero = {
        let mut seed = [0u8; 32];
        crypto_rng.fill_bytes(&mut seed);
        new_keypair_from_connect_magic1_with_seed(CRYPTO_MAGIC, seed).unwrap()
    };

    let endpoint_zero = {
        let port : u16 = 3030;
        // let ip = "::1".parse::<std::net::Ipv6Addr>().unwrap();
        let ip = "127.0.0.1".parse::<std::net::Ipv4Addr>().unwrap().to_ipv6_mapped();
        STPAddress::from(ip, port, &stp_key_zero)
    };

    let finalizer_peer_addresses = vec![FinalizerPeerAddress { bft_pk: PubKeyID(bft_kps[0].verification_key().into()), address: endpoint_zero.clone() }];

    if i == usize::MAX {
        // let _joins: [; N];
        rt.spawn(instance(bft_kps[0], Some(stp_key_zero), Some(endpoint_zero), roster.clone(), finalizer_peer_addresses.clone(), None));
        for j in 1..N {
            rt.spawn(instance(bft_kps[j], None, None, roster.clone(), finalizer_peer_addresses.clone(), None));
        }
    } else if i == 999 {
        // let _joins: [; N];
        rt.spawn(instance(bft_kps[0], Some(stp_key_zero), Some(endpoint_zero), roster.clone(), finalizer_peer_addresses.clone(), None));
        for j in 1..N - 1 {
            rt.spawn(instance(bft_kps[j], None, None, roster.clone(), finalizer_peer_addresses.clone(), None));
        }
    } else {
        if i == 0 {
            rt.spawn(instance(bft_kps[i], Some(stp_key_zero), Some(endpoint_zero), roster.clone(), finalizer_peer_addresses.clone(), None));
        }
        else if i < N {
            rt.spawn(instance(bft_kps[i], None, None, roster.clone(), finalizer_peer_addresses.clone(), None));
        }
        else {
            let mut secret_key = [0u8; 32];
            crypto_rng.fill_bytes(&mut secret_key);
            rt.spawn(instance(SigningKey::from(secret_key), None, None, roster.clone(), finalizer_peer_addresses.clone(), None));
        }
    }
    rt.block_on(std::future::pending::<()>())
}

pub mod bandwidth_test;
pub mod p2p_test;
pub mod native_sockets;
pub mod nym_sockets;
pub mod helpers;

use helpers::*;

#[cfg(test)]
mod tests {
    use super::*;

    // #[ignore]
    // #[test]
    // fn multi_rt() {
    //     fn init_on_addr(addr_str: &'static str, peers: &'static [&'static str]) -> tokio::task::JoinHandle<()> {
    //         let rt = tokio::runtime::Runtime::new().unwrap();
    //         rt.spawn(async move { instance(addr_str, peers, None).await.expect("no errors") })
    //     }

    //     let joins = [
    //         init_on_addr("127.0.0.1:18080", &[]),
    //         init_on_addr("127.0.0.1:18081", &["127.0.0.1:18080"]),
    //         init_on_addr("127.0.0.1:18082", &["127.0.0.1:18080"]),
    //         init_on_addr("127.0.0.1:18083", &["127.0.0.1:18080"]),
    //     ];
    //     loop {
    //         std::thread::sleep(std::time::Duration::from_secs(1));
    //     }
    // }

    #[test]
    fn single_rt() {
        run_instances(usize::MAX);
    }

    #[ignore]
    #[test]
    fn check_proposer_from_height_round() {
        let roster_ = [
            SortedRosterMember{ pub_key: PubKeyID([1;32]), stake: 2000, cumulative_stake: 2000 },
            SortedRosterMember{ pub_key: PubKeyID([2;32]), stake: 1000, cumulative_stake: 3000 },
            SortedRosterMember{ pub_key: PubKeyID([2;32]), stake: 1000, cumulative_stake: 4000 },
            SortedRosterMember{ pub_key: PubKeyID([3;32]), stake: 0000, cumulative_stake: 4000 },
        ];
        let roster = [
            SortedRosterMember{ pub_key: PubKeyID([1;32]), stake: 2, cumulative_stake: 2 },
            SortedRosterMember{ pub_key: PubKeyID([2;32]), stake: 1, cumulative_stake: 3 },
            SortedRosterMember{ pub_key: PubKeyID([2;32]), stake: 1, cumulative_stake: 4 },
            SortedRosterMember{ pub_key: PubKeyID([3;32]), stake: 0, cumulative_stake: 4 },
        ];
        let roster0 = [
            SortedRosterMember{ pub_key: PubKeyID([1;32]), stake: 0, cumulative_stake: 0 },
            SortedRosterMember{ pub_key: PubKeyID([2;32]), stake: 0, cumulative_stake: 0 },
            SortedRosterMember{ pub_key: PubKeyID([3;32]), stake: 0, cumulative_stake: 0 },
        ];
        assert!((None, PubKeyID::NIL) == TMState::proposer_from_height_round(&HashKeys::default(), &[], 2, 1));
        for height in 0..8 {
            for round in 0..6 {
                let (Some(i), _) = TMState::proposer_from_height_round(&HashKeys::default(), &roster_[..], height, round) else { panic!(); };
                println!("BFT Proposer at {}.{}: {}", height, round, i);
                let (Some(i), _) = TMState::proposer_from_height_round(&HashKeys::default(), &roster[..], height, round) else { panic!(); };
                println!("BFT Proposer at {}.{}: {}", height, round, i);
                let (Some(i), _) = TMState::proposer_from_height_round(&HashKeys::default(), &roster[..1], height, round) else { panic!(); };
                println!("BFT Proposer at {}.{}: {}", height, round, i);
                // assert!(TMState::proposer_from_height_round(&roster0, 100, height, round).0.is_none());
            }
        }
        // let (Some(i), _) = TMState::proposer_from_height_round(&roster[..2], 100, heig) else { panic!(); };
        // println!("BFT Proposer at {}.{}: {}", 2, 2, i);
    }

    #[test]
    fn check_gen_rngs() {
        pub struct Test {
            arr: &'static[u8],
            rngs: &'static[[usize; 2]],
        }
        let tests = [
            Test { arr: b"00000000",  rngs: &[[0,8]] },
            Test { arr: b"00010000",  rngs: &[[0,8]] },
            Test { arr: b"10010000",  rngs: &[[1,8]] },
            Test { arr: b"10010001",  rngs: &[[1,7]] },
            Test { arr: b"10011001",  rngs: &[[1,3], [5,7]] },
            Test { arr: b"101101100", rngs: &[[1,2], [4,5], [7,9]] },
            Test { arr: b"101111100", rngs: &[[1,2],        [7,9]] },
        ];

        for (test_i, test) in tests.iter().enumerate() {
            let rngs = gen_mostly_empty_rngs(test.arr.len(), |i| test.arr[i] == b'0');
            assert_eq!(test.rngs, &rngs, "index {}", test_i);
        }
    }
}
