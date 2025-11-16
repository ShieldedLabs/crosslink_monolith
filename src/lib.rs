#![allow(dead_code)]
#![allow(unused_parens)]
#![allow(clippy::never_loop)]

#![allow(clippy::eq_op)]
const PRINT_PROTOCOL:       bool = 1 == 1;
const PRINT_PROTOCOL_TAG:   bool = 0 == 1;
const PRINT_ROSTER:         bool = 0 == 1;
const PRINT_ROSTER_CMD:     bool = 1 == 1;
const PRINT_NETWORK_STATS:  bool = 1 == 1;
const PRINT_PEERS:          bool = 0 == 1;
const PRINT_VALID_INCOMING: bool = 0 == 1;
const PRINT_SENDS:          bool = 0 == 1;
const PRINT_SEND_CS:        bool = 0 == 1;
const PRINT_RNGS:           bool = 0 == 1;
const PRINT_SIGN:           bool = 0 == 1;
const PRINT_BFT_PROPOSAL:   bool = 0 == 1;
const PRINT_BFT_VOTE:       bool = 1 == 1;
const PRINT_BFT_UPDATE:     bool = 1 == 1;
const PRINT_BFT_STATE:      bool = 0 == 1;
const PRINT_BFT_CONDITIONS: bool = 1 == 1;
const PRINT_BFT_TIMEOUTS:   bool = 0 == 1;


// MTU discovery is an option, but for now we're adopting a very conservative and VPN-friendly fixed-value MTU.
const ETHERNET_FRAME_SIZE:   usize = 1500;
const IPV6_HEADER_SIZE:      usize =   40;
const UDP_HEADER_SIZE:       usize =    8;
const PPPOE_HEADER_SIZE:     usize =    8;
const WIREGUARD_HEADER_SIZE: usize =   40;
const VPN_HEADER_SIZE:       usize =   64; // relatively conservative(?) OpenVPN header overhead size
const NOISE_NONCE_SIZE:      usize =    8;
const NOISE_HEADER_SIZE:     usize =   16;

const MAX_PATH_HEADERS_SIZE: usize = (IPV6_HEADER_SIZE + UDP_HEADER_SIZE + WIREGUARD_HEADER_SIZE + VPN_HEADER_SIZE + NOISE_NONCE_SIZE + NOISE_HEADER_SIZE);

const PATH_MTU: usize = ETHERNET_FRAME_SIZE - MAX_PATH_HEADERS_SIZE;

// Tweak this!
const MAX_BANDWIDTH_BYTES_PER_SECOND: usize = 1_000_000;


use static_assertions::{const_assert};
use std::{io::{Cursor, Read}, net::{Ipv6Addr, SocketAddr, SocketAddrV6}, sync::{Arc, Mutex}};
use byteorder::{LittleEndian, ReadBytesExt};
use ed25519_zebra::{Signature, SigningKey, VerificationKeyBytes, VerificationKey};
use rand::{seq::{IndexedRandom}, Rng, RngCore, SeedableRng};
use rand_chacha::ChaCha20Rng;
use rand_pcg::Lcg128CmDxsm64 as SimRng;
use snow::resolvers::CryptoResolver;
use tokio::time::Instant;

const TICK_DURATION: std::time::Duration = std::time::Duration::from_millis(300);
const TIMEOUT_DURATION: std::time::Duration = std::time::Duration::from_millis(10000);
const NONCE_FORWARD_JUMP_TOLERANCE: u64 = 512;

fn is_timeout(e: std::io::ErrorKind) -> bool{
    e == std::io::ErrorKind::WouldBlock || e == std::io::ErrorKind::TimedOut
}

#[derive(Default)]
struct NetworkStats {
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
enum TMStep {
    Propose,
    Prevote,
    // ALT: extra sign step
    Precommit,
}

#[derive(Debug)]
struct TMDecision {
    round_i: usize,
    value: BlockValue,
    //signatures: Vec<TMSig>, // ability to prove to others e.g. those catching up
}

#[derive(Debug, Copy, Clone)]
struct SentPacket {
    bytes: usize,
    time_sent: tokio::time::Instant,
}
impl Default for SentPacket {
    fn default() -> Self {
        Self {
            bytes: 0,
            time_sent: Instant::now(),
        }
    }
}

#[derive(Debug, Default, Copy, Clone)]
struct ReceivedPacket {
    bytes: usize,
}

#[derive(Debug, Default, Copy, Clone)]
struct AcknowledgedSentPacket {
    bytes: usize,
    rtt: f64,
}

#[derive(Debug, Default, Copy, Clone)]
struct Slot<T> {
    index: usize,
    value: T
}

#[derive(Debug)]
struct RingStream<T: Default, const N: usize> {
    slots: [Slot<T>; N],
}
impl<T: Default + Copy, const N: usize> Default for RingStream<T, N> {
    fn default() -> Self {
        Self {
            slots: [Slot::<T>::default(); N],
        }
    }
}
impl<T: Default, const N: usize> RingStream<T, N> {
    pub fn at<'a>(&'a mut self, index: usize) -> Option<&'a mut T> {
        let slot_index = index % N;
        if self.slots[slot_index].index == index {
            Some((&mut self.slots[slot_index].value))
        } else {
            None
        }
    }
    pub fn at_immut<'a>(&'a self, index: usize) -> Option<&'a T> {
        let slot_index = index % N;
        if self.slots[slot_index].index == index {
            Some((&self.slots[slot_index].value))
        } else {
            None
        }
    }
    pub fn set(&mut self, value: T, index: usize) {
        let slot_index = index % N;
        self.slots[slot_index].index = index;
        self.slots[slot_index].value = value;
    }
}


struct TMVote {
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
pub struct ClosureToValidateProposedBlock(pub Arc<dyn for<'a> Fn(&'a BlockValue)-> core::pin::Pin<Box<dyn Future<Output = TMStatus> + Send + 'a>> + Send + Sync + 'static>);
impl std::fmt::Debug for ClosureToValidateProposedBlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("ClosureToValidateProposedBlock(..)") }
}
#[derive(Clone)]
pub struct ClosureToPushDecidedBlock(pub Arc<dyn Fn(BlockValue, FatPointerToBftBlock3)-> core::pin::Pin<Box<dyn Future<Output = Vec<SortedRosterMember>> + Send>> + Send + Sync + 'static>);
impl std::fmt::Debug for ClosureToPushDecidedBlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("ClosureToPushDecidedBlock(..)") }
}
#[derive(Clone)]
pub struct ClosureToGetHistoricalBlock(pub Arc<dyn Fn(u64)-> core::pin::Pin<Box<dyn Future<Output = (BlockValue, FatPointerToBftBlock3)> + Send>> + Send + Sync + 'static>);
impl std::fmt::Debug for ClosureToGetHistoricalBlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("ClosureToGetHistoricalBlock(..)") }
}
#[derive(Clone)]
pub struct ClosureToUpdateRosterCmd(pub Arc<dyn Fn(Option<String>)-> core::pin::Pin<Box<dyn Future<Output = Option<String>> + Send>> + Send + Sync + 'static>);
impl std::fmt::Debug for ClosureToUpdateRosterCmd {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("ClosureToUpdateRosterCmd(..)") }
}

/*
FROM ZEBRA
DATA LAYOUT FOR VOTE
32 byte ed25519 public key of the finalizer who's vote this is
32 byte blake3 hash of value, or all zeroes to indicate Nil vote
8 byte height
4 byte round where MSB is used to indicate is_commit for the vote type. 1 bit is_commit, 31 bits round index

TOTAL: 76 B

A signed vote will be this same layout followed by the 64 byte ed25519 signature of the previous 76 bytes.
*/

/// A bundle of signed votes for a block
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)] //, serde::Serialize, serde::Deserialize)]
pub struct FatPointerToBftBlock3 {
    pub vote_for_block_without_finalizer_public_key: [u8; 76 - 32],
    pub signatures: Vec<FatPointerSignature3>,
}

impl std::fmt::Display for FatPointerToBftBlock3 {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{{hash:")?;
        for b in &self.vote_for_block_without_finalizer_public_key[0..32] {
            write!(f, "{:02x}", b)?;
        }
        write!(f, " ovd:")?;
        for b in &self.vote_for_block_without_finalizer_public_key[32..] {
            write!(f, "{:02x}", b)?;
        }
        write!(f, " signatures:[")?;
        for (i, s) in self.signatures.iter().enumerate() {
            write!(f, "{{pk:")?;
            for b in s.public_key {
                write!(f, "{:02x}", b)?;
            }
            write!(f, " sig:")?;
            for b in s.vote_signature {
                write!(f, "{:02x}", b)?;
            }
            write!(f, "}}")?;
            if i + 1 < self.signatures.len() {
                write!(f, " ")?;
            }
        }
        write!(f, "]}}")?;
        Ok(())
    }
}

fn round_data_to_fat_pointer(round_data: &RoundData, roster: &[SortedRosterMember]) -> FatPointerToBftBlock3 {
    let vote_for_block_without_finalizer_public_key: [u8; 76 - 32];
    {
        let mut sign_data = [0; 76 - 32];
        round_data.proposal_id.0.write_to(&mut sign_data[0..32]);
        round_data.height.write_to(&mut sign_data[32..]);
        (round_data.round + 0x8000_0000).write_to(&mut sign_data[40..]);
        vote_for_block_without_finalizer_public_key = sign_data;
    }

    FatPointerToBftBlock3 {
        vote_for_block_without_finalizer_public_key,
        signatures: round_data.msg_val_sigs
            .iter()
            .map(|x| &x[1])
            .enumerate()
            .filter_map(|(roster_i, (value_id, commit_signature))| {
                if *value_id == round_data.proposal_id && *commit_signature != TMSig::NIL {
                    Some(FatPointerSignature3 {
                        public_key: roster[roster_i].pub_key.0,
                        vote_signature: commit_signature.0,
                    })
                } else { None }
            })
            .collect(),
    }
}

impl FatPointerToBftBlock3 {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&self.vote_for_block_without_finalizer_public_key);
        buf.extend_from_slice(&(self.signatures.len() as u16).to_le_bytes());
        for s in &self.signatures {
            buf.extend_from_slice(&s.to_bytes());
        }
        buf
    }
    #[allow(clippy::reversed_empty_ranges)]
    pub fn try_from_bytes(bytes: &Vec<u8>) -> Option<FatPointerToBftBlock3> {
        if bytes.len() < 76 - 32 + 2 {
            return None;
        }
        let vote_for_block_without_finalizer_public_key = bytes[0..76 - 32].try_into().unwrap();
        let len = u16::from_le_bytes(bytes[76 - 32..2].try_into().unwrap()) as usize;

        if 76 - 32 + 2 + len * (32 + 64) > bytes.len() {
            return None;
        }
        let rem = &bytes[76 - 32 + 2..];
        let signatures = rem
            .chunks_exact(32 + 64)
            .map(|chunk| FatPointerSignature3::from_bytes(chunk.try_into().unwrap()))
            .collect();

        Some(Self {
            vote_for_block_without_finalizer_public_key,
            signatures,
        })
    }
}

/// A vote signature for a block
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)] //, serde::Serialize, serde::Deserialize)]
pub struct FatPointerSignature3 {
    pub public_key: [u8; 32],
    pub vote_signature: [u8; 64],
}

impl FatPointerSignature3 {
    pub fn to_bytes(&self) -> [u8; 32 + 64] {
        let mut buf = [0_u8; 32 + 64];
        buf[0..32].copy_from_slice(&self.public_key);
        buf[32..32 + 64].copy_from_slice(&self.vote_signature);
        buf
    }
    pub fn from_bytes(bytes: &[u8; 32 + 64]) -> FatPointerSignature3 {
        Self {
            public_key: bytes[0..32].try_into().unwrap(),
            vote_signature: bytes[32..32 + 64].try_into().unwrap(),
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum TMStatus {
    Indeterminate,
    Pass, // 2f+1 yes
    Fail, // f+1 no
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct ValueId([u8; 32]);
impl ValueId { const NIL: Self = Self([0; 32]); }
impl std::fmt::Display for ValueId { fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { fmt_byte_str(f, &self.0) } }
impl std::fmt::Debug   for ValueId { fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { fmt_prefixed_byte_str(f, "VId{", &self.0)?; write!(f, "}}") } }

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PubKeyID(pub [u8; 32]);
impl PubKeyID { const NIL: Self = Self([0; 32]); }
impl std::fmt::Display for PubKeyID { fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { fmt_byte_str(f, &self.0) } }
impl std::fmt::Debug   for PubKeyID { fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { fmt_prefixed_byte_str(f, "Pub{", &self.0[..2])?; write!(f, "}}") } }

#[derive(Clone, Copy, PartialEq, Eq)]
struct TMSig ([u8; 64]);
impl TMSig {
    const NIL: Self = Self([0; 64]);
    fn verify(&self, pub_key: PubKeyID, signed_data: &[u8]) -> Result<(), (ed25519_zebra::Error, &str)> {
        let signature = Signature::from_bytes(&self.0);
        let vk = match VerificationKey::try_from(pub_key.0) { Ok(v)=>v,       Err(err)=>{ return Err((err, "invalid public key")) }};
        match vk.verify(&signature, signed_data)            { Ok(())=>Ok(()), Err(err)=>{ Err((err, "invalid signature")) }}
    }
}
impl std::fmt::Debug for TMSig { fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { fmt_prefixed_byte_str(f, "Sig{", &self.0[..2])?; write!(f, "}}") } }

#[derive(Debug, Clone)]
struct RoundData {
    height: u64,
    round: u32,
    // parallel with sorted roster arrays
    // TODO: keep parallel with each other, but be sparse in members
    proposal: BlockValue,
    proposal_valid_round: i64,
    proposal_sigs: Vec<TMSig>,
    proposal_sigs_n: usize, // filling sigs with random-access
    proposal_id: ValueId,
    proposal_checked_validity: TMStatus,
    // TODO: handle early outs because of this
    proposal_is_faulty: bool,

    // TODO: we may be able to compress valueid, but we do need to track it before we have the proposal
    msg_val_sigs: Vec<[(ValueId, TMSig); 2]>, // prevote then precommit
    roster: Vec<SortedRosterMember>,

    counts: ConsensusCounts,
    // TODO: can probably do this from whether *our* node has a valid value
    // TODO: by round or for whole state?
    active_timeout: Option<Timeout>,
    timeout_triggered: [bool; 2],
}
impl RoundData {
    const EMPTY: RoundData = RoundData {
        height: 0,
        round: 0,
        proposal: BlockValue(Vec::new()), // NOTE(azmr): don't alloc until we know the size (signed by proposer)
        proposal_valid_round: -1,
        proposal_sigs: Vec::new(),
        proposal_sigs_n: 0,
        proposal_id: ValueId::NIL,
        proposal_checked_validity: TMStatus::Indeterminate,
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
    fn has_enough_info_to_determine_validity(&self) -> bool {
        self.proposal_is_faulty || self.has_full_proposal()
    }
    // auto-caching
    async fn proposal_is_valid(&mut self, validate_closure: ClosureToValidateProposedBlock) -> TMStatus {
        // TODO: may want to start doing some of these on < proposal_chunks_n, i.e. shortcut known-invalid
        if self.proposal_checked_validity == TMStatus::Indeterminate {
            if self.proposal_is_faulty {
                self.proposal_checked_validity = TMStatus::Fail;
            } else if self.has_full_proposal() {
                self.proposal_checked_validity = validate_closure.0(&self.proposal).await;
            }
        }
        self.proposal_checked_validity
    }
}

enum TMMsgData {
    Proposal(BlockValue, i64),
    Prevote(ValueId),
    Precommit(ValueId),
}
struct TMMsg {
    height: u64,
    round: u32,
    data: TMMsgData, // ALT: byteslice + step distinguisher
    sig: TMSig,
}

#[derive(Clone, Copy, PartialEq)]
struct ConsensusCounts {
    anys: u64,
    prevotes: u64,
    nil_prevotes: u64,
    yes_prevotes: u64,
    precommits: u64,
    yes_precommits: u64,
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
struct Timeout { time: Instant, height: u64, round: u32, step: TMStep }
impl Timeout {
    fn new(now: Instant, height: u64, round: u32, step: TMStep) -> Timeout {
        use std::time::Duration;
        let timeout = match step {
            // Note(Sam): These timeout should be tuned to match the maximum network load block time. An additional
            // virtue of a short block time that I had not considered is that it hides round stalls better.
            TMStep::Propose   => Duration::from_millis(2000) + round * Duration::from_millis(500),
            TMStep::Prevote   => Duration::from_millis(2000) + round * Duration::from_millis(500),
            TMStep::Precommit => Duration::from_millis(2000) + round * Duration::from_millis(500),
        };

        Timeout{ time: now + timeout, height, round, step }
    }
}


const ROSTER_MAX_N: usize = 100;
fn active_roster_len(roster: &[SortedRosterMember]) -> usize { usize::min(ROSTER_MAX_N, roster.len()) }
fn total_roster_len(roster: &[SortedRosterMember])  -> usize { roster.len() }

#[derive(PartialEq, Debug, Clone, Copy)]
pub struct HashKey(pub [u8; 32]);
impl HashKey { const NIL: Self = Self([0;32]); }
impl HashKey {
    fn hasher(&self)            -> blake3::Hasher { blake3::Hasher::new_keyed(&self.0) }
    fn hash(&self, data: &[u8]) -> [u8; 32]       { *blake3::keyed_hash(&self.0, data).as_bytes() }
}

#[derive(Debug)]
pub struct HashKeys {
    pub proposer: HashKey,
    pub value_id: HashKey,
    pub connect_contention: HashKey,
    pub proposal_sig: HashKey,
}
impl Default for HashKeys {
    fn default() -> Self {
        Self {
            proposer:           HashKey(blake3::Hasher::new_derive_key("BFT Proposer")          .finalize().into()),
            value_id:           HashKey(blake3::Hasher::new_derive_key("BFT Value ID")          .finalize().into()),
            connect_contention: HashKey(blake3::Hasher::new_derive_key("BFT Connect Contention").finalize().into()), // NOTE(azmr): skipping update
            proposal_sig:       HashKey(blake3::Hasher::new_derive_key("BFT Proposal Signature").finalize().into()),
        }
    }
}

#[derive(Debug)]
struct TMState {
    hash_keys: HashKeys,
    my_port: u16,
    my_signing_key: SigningKey,
    my_pub_key: PubKeyID,
    round: u32,
    step: TMStep,
    /// basically the chain of agreed blocks
    height: u64,
    /// most recent "possible decision value" - successful proposal + prevote
    /// when valid_value was updated
    valid_value_round: (Option<BlockValue>, i64), // TODO
    /// last value sent for precommit // TODO: non-nil only?
    /// last round on which a *non-nil* value was sent
    locked_value_round: (Option<BlockValue>, i64), // TODO

    rounds_data: Vec<RoundData>,

    recent_commit_round_cache: Vec<RoundData>, // for now will hold all completed heights

    propose_closure: ClosureToProposeNewBlock,
    validate_closure: ClosureToValidateProposedBlock,
    push_block_closure: ClosureToPushDecidedBlock,
    get_block_closure: ClosureToGetHistoricalBlock,

    roster_cmd: Option<String>,
    update_roster_cmd_closure: ClosureToUpdateRosterCmd,
}
impl TMState {
    fn init(
        my_signing_key: SigningKey, my_pub_key: PubKeyID, my_port: u16,
        propose_closure: ClosureToProposeNewBlock,
        validate_closure: ClosureToValidateProposedBlock,
        push_block_closure: ClosureToPushDecidedBlock,
        get_block_closure: ClosureToGetHistoricalBlock,
        update_roster_cmd_closure: ClosureToUpdateRosterCmd) -> Self {
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
            get_block_closure,
            roster_cmd: None,
            update_roster_cmd_closure,
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
        let Some(roster_i) = roster_i_from_pub_key(&roster[..active_roster_len(roster)], self.my_pub_key) else {
            eprintln!("{} \x1b[91mBFT ERROR\x1b[0m: failed to find my own public key in the roster", self.ctx_str(roster));
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
                    if PRINT_SIGN { println!("{}: signed proposal with {:?}", self.ctx_str(roster), sig) };

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
                if PRINT_BFT_VOTE { println!("{} {} on {}", self.ctx_str(roster), ["prevoting", "precommitting"][is_precommit as usize], value_id); }
                let packet_type = PACKET_TYPE_PREVOTE_SIGNATURES + is_precommit;
                let signed_data = make_vote_sign_datas(roster[roster_i].pub_key.0, is_precommit != 0, height, round, value_id)[1];
                let sig         = TMSig(self.my_signing_key.sign(&signed_data).to_bytes());
                if PRINT_SIGN { println!("{} signed {} with {:?}", self.ctx_str(roster), ["prevote", "precommit"][is_precommit as usize], sig) };

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
            eprintln!("\x1b[91mBFT ERROR\x1b[0m: trying to get proposer from empty roster");
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
            eprintln!("\x1b[91mBFT ERROR\x1b[0m: all roster members have no stake");
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
            let proposal = if let Some(valid_value) = self.valid_value_round.0.clone() {
                Some(valid_value)
            } else {
                self.propose_closure.0().await
            };
            if PRINT_BFT_PROPOSAL { if let Some(proposal) = &proposal { println!("{} about to propose with status '{:?}': {:?}", self.ctx_str(roster), self.validate_closure.0(&proposal).await, proposal); } }

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
        let me_str  = self.ctx_str(roster);
        let pkt_str = format!("{:20} {}.{}.{}", packet_name_from_tag(packet_type), height, round, chunk_i);

        if height != self.height {
            // eprintln!("{}: BFT: received [{}] when we're at height {}", me_str, pkt_str, self.height);
            return TMStatus::Fail;
        }

        // check if in (active) roster
        if roster_i >= active_roster_len(roster) {
            eprintln!("{} [{}]: \x1b[91mBFT FAULT\x1b[0m: {} is not in the active roster.", me_str, pkt_str, roster_i);
            return TMStatus::Fail;
        }

        let from_pub_key = roster[roster_i].pub_key;

        // pkt_str += &format!(" from {} ({})", roster_i, from_pub_key);
        let ctx_str = format!("{} [{} from {} {:?}]", me_str, pkt_str, roster_i, from_pub_key);

        // check if data was signed by pub key
        match sig.verify(from_pub_key, signed_data) { Ok(())=>{}, Err((err, str))=> {
            eprintln!("{}: \x1b[91mBFT FAULT\x1b[0m: {} [..{}]: for {} {}", ctx_str, str, signed_data.len(), value_id, err);
            return TMStatus::Fail;
        }};

        if PRINT_VALID_INCOMING { eprintln!("{}: valid signature for value id: {}", ctx_str, value_id); }

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
                let Ok(hdr) = PacketProposalChunkHeader::read_from(signed_data) else {
                    return TMStatus::Fail
                };

                // "have they previously proposed a different value?"
                if is_prev_seen_round && round_data.proposal_sigs_n > 0 {
                    if is_my_proposal &&
                      (round_data.proposal.0.len() != hdr.proposal_size as usize ||
                       round_data.proposal_id != value_id ||
                       round_data.proposal_valid_round != valid_round) { // Amnesiac Proposer's Dilemma
                        // Flush proposal id/votes. @Robustness @Duplicate: how to tersely flush the round?
                        let roster_n = active_roster_len(roster);
                        *round_data = RoundData {
                            height:               round_data.height,
                            round:                round_data.round,
                            proposal_id:          value_id,
                            proposal_valid_round: valid_round,
                            msg_val_sigs:         vec![[(ValueId::NIL, TMSig::NIL); 2]; roster_n], // TODO: just use ROSTER_MAX_N?
                            roster:               Vec::from(&roster[0..roster_n]),
                            active_timeout:       round_data.active_timeout.clone(),
                            timeout_triggered:    round_data.timeout_triggered,
                            ..RoundData::EMPTY
                        };
                        round_data.proposal.0    = vec![0;          hdr.proposal_size as usize];
                        round_data.proposal_sigs = vec![TMSig::NIL; round_data.proposal.chunks_n()];
                        eprintln!("{}: \x1b[93mAMNESIAC PROPOSER\x1b[0m at {}.{}.{}: Flushing proposal...", ctx_str, height, round, chunk_i);
                    } else {
                        if round_data.proposal.0.len() != hdr.proposal_size as usize {
                            eprintln!("{}: \x1b[91mBFT FAULT\x1b[0m at {}.{}.{}: proposer {} proposed 2 different-size values ({:?}, {:?}). Ignoring latest...",
                            ctx_str, height, round, chunk_i, roster_i, round_data.proposal.0.len(), hdr.proposal_size);
                            return TMStatus::Fail;
                        }
                        if round_data.proposal_id != value_id {
                            // TODO: immediately class both as invalid?
                            eprintln!("{}: \x1b[91mBFT FAULT\x1b[0m at {}.{}.{}: proposer {} proposed 2 different values ({:?}, {:?}). Ignoring latest...",
                            ctx_str, height, round, chunk_i, roster_i, round_data.proposal_id, value_id);
                            return TMStatus::Fail;
                        }
                        if round_data.proposal_valid_round != valid_round {
                            // TODO: immediately class both as invalid
                            eprintln!("{}: \x1b[91mBFT FAULT\x1b[0m at {}.{}.{}: proposer {} proposed 2 different valid rounds ({}, {}). Ignoring latest...",
                            ctx_str, height, round, chunk_i, roster_i, round_data.proposal_valid_round, valid_round);
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
                                    eprintln!("{}: \x1b[91mBFT FAULT\x1b[0m at {}.{}: finalizer {} {} on non-proposed value {}. Ignoring...", ctx_str, height, round, roster_i, ["prevoted","precommitted"][is_precommit], value_id);
                                    msg_val[is_precommit] = (ValueId::NIL, TMSig::NIL);
                                }
                            }
                        }

                        if prev_sig_had_fault { // recompute from scratch
                            // NOTE: this does NOT imply the current packet/proposal is faulty, so we should continue with it
                            let mut check_counts = ConsensusCounts::ZERO;
                            for (_, sig) in round_data.msg_val_sigs.iter().enumerate() {
                                check_counts = check_counts + ConsensusCounts::from(&(*sig, roster[roster_i].stake));
                            }
                            round_data.counts = check_counts;
                        }
                    }

                    if round_data.proposal_sigs_n == round_data.proposal_sigs.len() {
                        let check_value_id = round_data.proposal.id_from_value(&self.hash_keys);
                        if round_data.proposal_id != check_value_id {
                            round_data.proposal_is_faulty = true;
                            eprintln!("{}: \x1b[91mBFT FAULT\x1b[0m: proposer's value_id does not match our calculation: {} != {}",
                                ctx_str, round_data.proposal_id, check_value_id);
                            return TMStatus::Fail;
                        }
                    }

                    if PRINT_BFT_UPDATE { println!("{}: update to {}/{} proposal chunks on {}", ctx_str, round_data.proposal_sigs_n, round_data.proposal_sigs.len(), round_data.proposal_id); }

                    // TODO: include signed prevote & precommit for self?
                } else if round_data.proposal_sigs[chunk_i] != sig { // TODO: check value/sig conformance
                    if is_my_proposal { // Amnesiac Proposer's Dilemma
                        // Flush proposal id/votes. @Robustness @Duplicate: how to tersely flush the round?
                        let roster_n = active_roster_len(roster);
                        *round_data = RoundData {
                            height:               round_data.height,
                            round:                round_data.round,
                            proposal_id:          value_id,
                            proposal_valid_round: valid_round,
                            msg_val_sigs:         vec![[(ValueId::NIL, TMSig::NIL); 2]; roster_n], // TODO: just use ROSTER_MAX_N?
                            roster:               Vec::from(&roster[0..roster_n]),
                            active_timeout:       round_data.active_timeout.clone(),
                            timeout_triggered:    round_data.timeout_triggered,
                            ..RoundData::EMPTY
                        };
                        round_data.proposal.0    = vec![0;          hdr.proposal_size as usize];
                        round_data.proposal_sigs = vec![TMSig::NIL; round_data.proposal.chunks_n()];
                        eprintln!("{}: \x1b[93mAMNESIAC PROPOSER\x1b[0m at {}.{}.{}: Flushing proposal...", ctx_str, height, round, chunk_i);
                    } else {
                        // TODO: treat this as a failed is_valid & early out before awaiting full proposal
                        round_data.proposal_is_faulty = true;
                        eprintln!("{}: \x1b[91mBFT FAULT\x1b[0m: proposer signed 2 different values. Ignoring latest...", ctx_str);
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
                    eprintln!("{}: \x1b[91mBFT FAULT\x1b[0m at {}.{}: finalizer {} voted on non-proposed value {}. Ignoring...", ctx_str, height, round, roster_i, value_id);
                    return TMStatus::Fail;
                } else {
                    TMStatus::Pass
                };

                // TODO: check if specified valid_round had a different value_id

                let old_val_sig = round_data.msg_val_sigs[roster_i][is_precommit];
                let new_val_sig = (value_id, sig);
                if old_val_sig.1 != TMSig::NIL && new_val_sig != old_val_sig {
                    // TODO: do we want to allow for NIL updating to valid?
                    eprintln!("{}: \x1b[91mBFT FAULT\x1b[0m at {}.{}: finalizer {} voted on 2 different values ({:?}, {:?}). Ignoring latest...", ctx_str, height, round, roster_i, new_val_sig, old_val_sig);
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
                    println!("{}: update to {:?} (d: {:?})", ctx_str, round_data.counts, d);
                }

                if true {
                    let mut check_counts = ConsensusCounts::ZERO;
                    for (i, sig) in round_data.msg_val_sigs.iter().enumerate() {
                        check_counts = check_counts + ConsensusCounts::from(&(*sig, roster[i].stake));
                    }
                    if check_counts != round_data.counts {
                        eprintln!("{}: \x1b[91mBFT ERROR\x1b[0m: counts don't match: incremental: {:?}, absolute: {:?}", ctx_str, round_data.counts, check_counts);
                    }
                }

                status
            }


            _ => {
                eprintln!("{}: \x1b[91mBFT ERROR\x1b[0m: unexpected case: {}", ctx_str, packet_type);
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
        format!("{:05}-{:?}-{:>8}.{:3}.{:3}.{:9}", self.my_port, self.my_pub_key, format!("{:?}", roster_i_from_pub_key(roster, self.my_pub_key)), self.height, self.round, format!("{:?}", self.step))
    }
    fn name_str_other(roster: &[SortedRosterMember], peer: &Peer) -> String {
        format!("{:05}-{:?}-{:?}", peer.endpoint.unwrap_or_default().port, PubKeyID(peer.root_public_bft_key), roster_i_from_pub_key(roster, PubKeyID(peer.root_public_bft_key)))
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
            if PRINT_BFT_STATE {
                println!("{} {}={}.{}, {}/{}, {}", ctx_str,
                    ["!","="][is_current_height_and_round as usize],
                    self.rounds_data[i].height, self.rounds_data[i].round,
                    self.rounds_data[i].proposal_sigs_n, self.rounds_data[i].proposal_sigs.len(),
                    self.rounds_data[i].proposal_valid_round
                );
             }

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
                    if PRINT_BFT_CONDITIONS { println!("{}: in condition 22-0: receive first proposal this height", ctx_str); }
                    self.step = self.broadcast(roster, i, TMMsgData::Prevote(self.rounds_data[i].proposal_id));
                } else {
                    if PRINT_BFT_CONDITIONS { println!("{}: in condition 22-1: receive first proposal this height", ctx_str); }
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
                    if PRINT_BFT_CONDITIONS { println!("{}: in condition 28-0: received 2f+1 prevotes", ctx_str); }
                    self.step = self.broadcast(roster, i, TMMsgData::Prevote(self.rounds_data[i].proposal_id));
                } else {
                    if PRINT_BFT_CONDITIONS { println!("{}: in condition 28-1: received 2f+1 prevotes", ctx_str); }
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
                if PRINT_BFT_CONDITIONS { println!("{}: in condition 34: last orders on prevote period", ctx_str); }
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
                if PRINT_BFT_CONDITIONS { println!("{}: in condition 36: seen 2f+1 valid prevotes", ctx_str); }
                if self.step == TMStep::Prevote {
                    if PRINT_BFT_CONDITIONS { println!("{}: in condition 36-0: seen 2f+1 valid prevotes", ctx_str); }
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
                if PRINT_BFT_CONDITIONS { println!("{}: in condition 44: seen 2f+1 nil prevotes", ctx_str); }
                self.step = self.broadcast(roster, i, TMMsgData::Precommit(ValueId::NIL));
            }

            // line 47: last orders on precommit period
            // > upon 2f+1 <PRECOMMIT, h_p, round_p, ∗> for the first time do
            if (on_roster &&
                is_current_height_and_round &&
                big_threshold <= counts.precommits &&
                !self.rounds_data[i].timeout_triggered[1])
            {
                if PRINT_BFT_CONDITIONS { println!("{}: in condition 47: last orders on precommit period", ctx_str); }
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
                if PRINT_BFT_CONDITIONS { println!("{}: in condition 49: value decided", ctx_str); }
                let new_roster = self.push_block_closure.0(self.rounds_data[i].proposal.clone(), round_data_to_fat_pointer(&self.rounds_data[i], roster)).await;
                if PRINT_ROSTER { println!("{} new roster: {:?}", ctx_str, new_roster); }
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
                if PRINT_BFT_CONDITIONS { println!("{}: in condition 55: round catchup", ctx_str); }
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
                        if PRINT_BFT_TIMEOUTS { println!("{}: hit timeout propose", ctx_str); }
                        self.step = self.broadcast(roster, i, TMMsgData::Prevote(ValueId::NIL));
                    },
                    TMStep::Prevote => if self.step == TMStep::Prevote {
                        if PRINT_BFT_TIMEOUTS { println!("{}: hit timeout prevote", ctx_str); }
                        self.step = self.broadcast(roster, i, TMMsgData::Precommit(ValueId::NIL));
                    },
                    TMStep::Precommit => {
                        if PRINT_BFT_TIMEOUTS { println!("{}: hit timeout precommit", ctx_str); }
                        self.start_round(roster, now, self.round + 1).await
                    },
                }
            }
        }
    }
}

#[derive(Debug)]
struct PeerTransport {
    ack_latest: u64,
    ack_field: u64,
    nonce: u64,
    sent_packets:              RingStream<SentPacket, 1024>,
    received_packets:          RingStream<ReceivedPacket, 1024>,
    acknowledged_sent_packets: RingStream<AcknowledgedSentPacket, 1024>,
}
impl Default for PeerTransport {
    fn default() -> Self {
        Self {
            ack_latest: 0,
            ack_field: 0,
            nonce: 0,
            sent_packets:              RingStream::<SentPacket, 1024>::default(),
            received_packets:          RingStream::<ReceivedPacket, 1024>::default(),
            acknowledged_sent_packets: RingStream::<AcknowledgedSentPacket, 1024>::default(),
        }
    }
}


// TODO: can we megastruct these and collapse the codepaths?
#[derive(Debug)]
struct Peer {
    root_public_bft_key: [u8; 32],
    endpoint: Option<SecureUdpEndpoint>,
    outgoing_handshake_state: Option<snow::HandshakeState>,
    pending_client_ack_snow_state: Option<snow::StatelessTransportState>,
    snow_state: Option<snow::StatelessTransportState>,
    watch_dog: Instant,

    transport: PeerTransport,

    connection_is_unknown: bool,

    unacted_upon_status_height: Option<u64>,
    latest_status: Option<PacketStatus>,
}
impl Default for Peer {
    fn default() -> Self {
        Self {
            root_public_bft_key: [0_u8; 32],
            endpoint: None,
            outgoing_handshake_state: None,
            pending_client_ack_snow_state: None,
            snow_state: None,
            watch_dog: Instant::now(),
            transport: PeerTransport::default(),

            connection_is_unknown: false,
            unacted_upon_status_height: None,
            latest_status: None,
        }
    }
}

// NOTE: buf can be open-ended
trait SliceWrite         { fn write_to(&self, buf: &mut [u8]) -> usize; }
impl SliceWrite for u64  { fn write_to(&self, buf: &mut [u8]) -> usize { buf[0..8].copy_from_slice(&u64::to_le_bytes(*self)); 8 } }
impl SliceWrite for i64  { fn write_to(&self, buf: &mut [u8]) -> usize { buf[0..8].copy_from_slice(&i64::to_le_bytes(*self)); 8 } }
impl SliceWrite for u32  { fn write_to(&self, buf: &mut [u8]) -> usize { buf[0..4].copy_from_slice(&u32::to_le_bytes(*self)); 4 } }
impl SliceWrite for u16  { fn write_to(&self, buf: &mut [u8]) -> usize { buf[0..2].copy_from_slice(&u16::to_le_bytes(*self)); 2 } }
impl SliceWrite for u8   { fn write_to(&self, buf: &mut [u8]) -> usize { buf[0] = *self;                                      1 } }
impl SliceWrite for [u8] { fn write_to(&self, buf: &mut [u8]) -> usize { buf[0..self.len()].copy_from_slice(self);   self.len() } }


#[derive(Debug)]
struct UnknownPeer {
    endpoint: SecureUdpEndpoint,
    snow_state: snow::StatelessTransportState,
    pending_client_ack: bool,
    watch_dog: Instant,

    transport: PeerTransport,
    unacted_upon_status_height: Option<u64>,
}

#[derive(Clone, Copy)]
pub struct StaticDHKeyPair {
    pub private: [u8; 32],
    pub public: [u8; 32],
}

#[derive(PartialEq, Eq, Clone, Copy)]
pub struct SecureUdpEndpoint {
    pub public_key: [u8; 32],
    pub ip_address: [u8; 16],
    pub port: u16,
}
impl Default for SecureUdpEndpoint {
    fn default() -> SecureUdpEndpoint {
        SecureUdpEndpoint { public_key: [0_u8; 32], ip_address: [0_u8; 16], port: 0 }
    }
}

impl SecureUdpEndpoint {
    fn write_to(&self, buf: &mut [u8]) -> usize {
        self.public_key.write_to(&mut buf[..]);
        self.ip_address.write_to(&mut buf[32..]);
        self.port      .write_to(&mut buf[32+16..]);
        32+16+2
    }

    pub fn read_from<R: Read>(mut r: R) -> std::io::Result<Self> {
        let mut endpoint = SecureUdpEndpoint::default();
        r.read_exact(&mut endpoint.public_key)?;
        r.read_exact(&mut endpoint.ip_address)?;
        endpoint.port = r.read_u16::<LittleEndian>()?;
        Ok(endpoint)
    }
}

#[derive(Clone, Copy)]
pub struct EndpointEvidence {
    pub endpoint: SecureUdpEndpoint,
    pub root_public_bft_key: [u8; 32],
}
impl Default for EndpointEvidence {
    fn default() -> EndpointEvidence {
        EndpointEvidence { endpoint: SecureUdpEndpoint::default(), root_public_bft_key: [0_u8; 32] }
    }
}
impl EndpointEvidence {
    pub fn write_to(&self, buf: &mut [u8]) -> usize {
        let mut o = 0;
        o += self.endpoint       .write_to(&mut buf[o..]);
        o += self.root_public_bft_key.write_to(&mut buf[o..]);
        o
    }

    pub fn read_from<R: Read>(mut r: R) -> std::io::Result<Self> {
        let endpoint = SecureUdpEndpoint::read_from(&mut r)?;
        let mut key_bytes = [0_u8; 32];
        r.read_exact(&mut key_bytes)?;
        Ok(EndpointEvidence { endpoint, root_public_bft_key: key_bytes })
    }
}

fn fmt_byte_str(f: &mut std::fmt::Formatter<'_>, bytes: &[u8]) -> std::fmt::Result {
    let n = usize::min(bytes.len(), f.precision().unwrap_or(bytes.len()));
    for i in 0..n { write!(f, "{:02x}", bytes[i])?; }
    Ok(())
}

fn fmt_prefixed_byte_str(f: &mut std::fmt::Formatter<'_>, pre: &str, bytes: &[u8]) -> std::fmt::Result {
    write!(f, "{}", pre)?;
    fmt_byte_str(f, bytes)
}

impl std::fmt::Debug for StaticDHKeyPair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt_prefixed_byte_str(f, "StaticDHKeyPair { private: \"", &self.private)?;
        fmt_prefixed_byte_str(f, "\", public: \"",                &self.public)?;
        write!(f, "\" }}")
    }
}

impl std::fmt::Debug for EndpointEvidence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "EndpointEvidence {{ endpoint: ")?;
        self.endpoint.fmt(f)?;
        fmt_prefixed_byte_str(f, ", root_public_bft_key: \"", &self.root_public_bft_key)?;
        write!(f, "\" }}")
    }
}

impl std::fmt::Debug for SecureUdpEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fmt_prefixed_byte_str(f, "SecureUdpEndpoint { public_key: \"", &self.public_key)?;
        fmt_prefixed_byte_str(f, "\", ip_address: \"",                 &self.ip_address)?;
        write!(f, "\", port: {:05} }}", self.port)
    }
}

// returns true if a is initiator
fn contended_noise_is_initiator(hash_keys: &HashKeys, a: &[u8; 32], b: &[u8; 32]) -> bool {
    // TODO: do we want a fast insecure hash for this kind of thing?
    let a_to_b_hash = hash_keys.connect_contention.hasher().update(a).update(b).finalize();
    let b_to_a_hash = hash_keys.connect_contention.hasher().update(b).update(a).finalize();
    a_to_b_hash.as_bytes() < b_to_a_hash.as_bytes()
}

fn nonce_is_ok2(nonce: u64, ack_latest: u64, ack_field: u64) -> bool {(
    ack_latest <= nonce + 64 && // TODO: do we want to completely drop these or just exclude from heartbeat
    nonce != ack_latest &&
    nonce <= ack_latest + NONCE_FORWARD_JUMP_TOLERANCE &&
    (ack_latest < nonce || ack_field >> (ack_latest - nonce) & 1 == 0)
)}

fn nonce_is_ok(nonce: u64, ack_latest: u64, ack_field: u64) -> bool {
    if nonce > ack_latest && nonce > ack_latest + NONCE_FORWARD_JUMP_TOLERANCE { return false; }
    if nonce == ack_latest                                                           { return false; }
    if nonce + 64 < ack_latest                                                       { return false; }
    // NOTE: this shift can overflow if we don't return before
    if nonce < ack_latest && ack_field >> (ack_latest - nonce) & 1 != 0  { return false; }
    true
}

fn nonce_update(nonce: u64, ack_latest: &mut u64, ack_field: &mut u64) {
    // Update nonce tracking
    if nonce > *ack_latest {
        *ack_latest += 1;
        *ack_field <<= 1;
        *ack_field |= 1;
        let shift_amount = nonce - *ack_latest;
        if shift_amount >= 64 {
            *ack_field = 0;
        } else if shift_amount != 0 {
            *ack_field <<= shift_amount;
            *ack_latest = nonce;
        }
    } else {
        *ack_field |= 1_u64 << (*ack_latest - nonce);
    }
}

fn make_vote_sign_datas(pub_key: [u8; 32], is_precommit: bool, height: u64, round: u32, value_id: ValueId) -> [[u8; 76]; 2] {
    let mut sign_data_no = [0; 76];
    sign_data_no[0..32].copy_from_slice(&pub_key[..]);
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

async fn instance(my_root_private_key: SigningKey, my_static_keypair: Option<StaticDHKeyPair>, my_endpoint: Option<SecureUdpEndpoint>, roster: Vec<SortedRosterMember>, roster_endpoint_evidence: Vec<EndpointEvidence>, maybe_seed: Option<u128>) -> std::io::Result<()> {
    let block_rng = Arc::new(Mutex::new({
        let seed : u128 = maybe_seed.clone().unwrap_or_else(|| {
            let mut seed_rng = rand::rng();
            ((seed_rng.next_u64() as u128) << 64) | seed_rng.next_u64() as u128
        });
        SimRng::new(seed, 0)
    }));

    let should_propose_bad_value_sometimes = false; // my_endpoint.is_some(); // peer 0 only

    let decisions = Arc::new(Mutex::new(Vec::<(BlockValue, FatPointerToBftBlock3)>::new()));
    let decisions2 = Arc::clone(&decisions);

    let roster2 = roster.clone();
    let pub_key = PubKeyID(VerificationKeyBytes::from(&my_root_private_key.clone()).into());

    entry_point(my_root_private_key, my_static_keypair, my_endpoint, roster, roster_endpoint_evidence, maybe_seed,
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
                if block.0.len() == 0 { TMStatus::Fail }
                else if block.0[0] % 2 == 0 { TMStatus::Pass }
                //else if block.0[0] % 3 == 1 { TMStatus::Indeterminate }
                else { TMStatus::Fail }
            })
        })),
        ClosureToPushDecidedBlock(Arc::new(move |block, fat_pointer| {
            let decisions = Arc::clone(&decisions);
            let roster2 = roster2.clone();
            Box::pin(async move {
                decisions.lock().unwrap().push((block, fat_pointer));
                let mut ret = roster2.clone();
                ret.truncate(3 + decisions.lock().unwrap().len() % 2);
                ret
            })
        })),
        ClosureToGetHistoricalBlock(Arc::new(move |height| {
            let decisions = Arc::clone(&decisions2);
            Box::pin(async move {
                decisions.lock().unwrap()[height as usize].clone()
            })
        })),
        ClosureToUpdateRosterCmd(Arc::new(move |_str| { Box::pin(async move {
            Some(format!("{:?}", pub_key))
        })})),
    ).await
}

pub async fn entry_point(my_root_private_key: SigningKey, my_static_keypair: Option<StaticDHKeyPair>, my_endpoint: Option<SecureUdpEndpoint>, mut roster: Vec<SortedRosterMember>, mut roster_endpoint_evidence: Vec<EndpointEvidence>, maybe_seed: Option<u128>,
    propose_closure: ClosureToProposeNewBlock, validate_closure: ClosureToValidateProposedBlock, push_block_closure: ClosureToPushDecidedBlock, get_block_closure: ClosureToGetHistoricalBlock, roster_cmd_closure: ClosureToUpdateRosterCmd) -> std::io::Result<()> {
    hook_fail_on_panic();
    let mut base_rng = {
        let seed : u128 = maybe_seed.unwrap_or_else(|| {
            let mut seed_rng = rand::rng();
            ((seed_rng.next_u64() as u128) << 64) | seed_rng.next_u64() as u128
        });
        SimRng::new(seed, 0)
    };

    let noise_params: snow::params::NoiseParams = "Noise_IK_25519_ChaChaPoly_BLAKE2s".parse().unwrap();
    let my_root_public_bft_key = VerificationKeyBytes::from(&my_root_private_key);
    {
        let key : &[u8; 32] = &my_root_public_bft_key.into();
        print!("My root public key is \"");
        for i in 0..key.len() { print!("{:02x}", key[i]); }
        println!("\"");
    }
    let my_static_keypair = my_static_keypair.unwrap_or_else(|| {
        let kp = snow::Builder::new(noise_params.clone()).generate_keypair().unwrap();
        StaticDHKeyPair { private: kp.private.try_into().unwrap(), public: kp.public.try_into().unwrap(), }
    });

    let sock = {
        use socket2::{Domain, Protocol, Socket, Type};
        let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP)).unwrap();
        socket.set_nonblocking(true).unwrap();
        socket.set_only_v6(false).unwrap(); // Sets IPV6_V6ONLY to 0 for dual-stack (required for win32)
        socket.bind(&SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, my_endpoint.map(|e|e.port).unwrap_or(0), 0, 0).into()).unwrap();
        tokio::net::UdpSocket::from_std(socket.into()).unwrap()
    };

    let my_port = sock.local_addr().unwrap().port();

    let mut peers : Vec<Peer> = roster.iter().filter(|m| m.pub_key.0 != my_root_public_bft_key.as_ref())
        .map(|m| Peer { root_public_bft_key: m.pub_key.0, ..Peer::default() }).collect();

    for evidence in &roster_endpoint_evidence {
        if let Some(i) = peers.iter().position(|p| p.root_public_bft_key == evidence.root_public_bft_key) {
            peers[i].endpoint = Some(evidence.endpoint);
        }
    }
    if PRINT_PROTOCOL { println!("socket port={:05}, peers endpoints={:?}", my_port, peers.iter().map(|p|p.endpoint).collect::<Vec<_>>()); }

    // TODO: only convert private to public in 1 location
    let mut bft_state = TMState::init(my_root_private_key, PubKeyID(my_root_public_bft_key.into()), my_port, propose_closure, validate_closure, push_block_closure, get_block_closure, roster_cmd_closure); // TODO: double-check this is the right key
    bft_state.start_round(&roster, Instant::now(), 0).await;

    let mut my_endpoint_evidence = if let Some(i) = roster_endpoint_evidence.iter().position(|e| &e.root_public_bft_key == my_root_public_bft_key.as_ref()) {
        Some(roster_endpoint_evidence[i])
    } else {
        my_endpoint.map(|endpoint| EndpointEvidence { endpoint, root_public_bft_key: my_root_public_bft_key.into() })
    };

    let mut unknown_peers: Vec<UnknownPeer> = Vec::new();

    const ONE_SECOND: tokio::time::Duration = tokio::time::Duration::from_secs(1);
    let mut net_stats_window_start = tokio::time::Instant::now();
    let mut net_stats = NetworkStats::default();

    let mut recv_buf1 = [0; 2048];
    let mut recv_buf2 = [0; 2048];
    let mut send_buf1 = [0; 2048];
    let mut send_buf2 = [0; 2048];
    let mut next_tick_time = tokio::time::Instant::now();
    loop {
        let ctx_str = bft_state.ctx_str(&roster);

        fn read_header_and_maybe_status(msg: &[u8]) -> std::io::Result<(PacketHeader, Option<PacketStatus>, usize)> {
            let mut o   = 0;
            let mut cur = Cursor::new(&msg[o..]);
            let header  = PacketHeader::read_from(&mut cur)?;

            print_packet_tag_recv(header);

            let mut status = None;
            if header.has_status() {
                // TODO: scope down required ranges
                status = Some(PacketStatus::read_from(&mut cur)?);
            }
            o += cur.position() as usize;
            Ok((header, status, o))
        }

        fn write_header_and_maybe_status(header_: PacketHeader, include_status: bool, bft_state: &TMState, roster: &[SortedRosterMember], send_buf1: &mut [u8], peer_random: u64) -> usize {
            let mut header = header_;
            header.tag_and_ack |= if include_status { PACKET_TAG_STATUS_FLAG as u64 } else { 0 };

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
                    if PRINT_RNGS { println!("{} request proposal  chunks {:?} from {:?}", bft_state.ctx_str(roster), status.need_proposal_chunk_rngs, proposal_chunk_rngs); }

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
                        if PRINT_RNGS { println!("{} request {:9} chunks {:?} from {:?}", bft_state.ctx_str(roster), ["prevote", "precommit"][is_precommit], status.need_vote_rngs[is_precommit], vote_rngs); }
                    }

                }

                o += status.write_to(&mut send_buf1[o..]);
            }
            o
        }
        fn send_sock_msg(ctx_str: &str, transport: &mut PeerTransport, sock: &tokio::net::UdpSocket, peer_endpoint: SecureUdpEndpoint, msg: &[u8], stats: &mut NetworkStats) {
            // println!("Packet sent! Nonce: {}! {} bytes!", transport.nonce, msg.len());

            transport.sent_packets.set(SentPacket { bytes: msg.len(), time_sent: Instant::now() }, transport.nonce as usize);

            // TODO(phil) move this code
            let bytes_in_flight = {
                let mut bytes_in_flight = 0;
                for slot_i in 0..transport.sent_packets.slots.len() {
                    bytes_in_flight += transport.sent_packets.slots[slot_i].value.bytes;
                }
                bytes_in_flight
            };

            let (_, mean_rtt) = {
                let mut bytes_acknowledged = 0;
                let mut rtt_sum   = 0.0;
                let mut rtt_sum_n = 0.0;
                for slot_i in 0..transport.acknowledged_sent_packets.slots.len() {
                    let packet = transport.acknowledged_sent_packets.slots[slot_i].value;
                    bytes_acknowledged += packet.bytes;
                    if packet.rtt > 0.0 {
                        rtt_sum   += packet.rtt;
                        rtt_sum_n += 1.0;
                    }
                }
                if rtt_sum_n <= 0.0 { rtt_sum_n = 1.0; }
                (bytes_acknowledged, rtt_sum / rtt_sum_n)
            };

            // TODO(phil): Compute real bandwidth using sliding congestion windows, TCP-style
            // const BANDWIDTH_SAFETY_MARGIN: f64 =     0.8;

            let target_bytes_in_flight = ((MAX_BANDWIDTH_BYTES_PER_SECOND as f64 * mean_rtt.max(0.01)) as usize).max(PATH_MTU);

            // NOTE(phil) probabilistically lerp down towards 0 likelihood of sending a packet as we approach bandwidth limit
            // let rand_t = bytes_in_flight as f64 / target_bytes_in_flight as f64;
            // if rand::random::<f64>() < rand_t {
            //     if PRINT_PROTOCOL { println!("Dropping packet because of congestion control."); }
            //     return;
            // }

            // println!("Packet: {} bytes", msg.len());
            let addr = SocketAddr::V6(SocketAddrV6::new(Ipv6Addr::from(peer_endpoint.ip_address), peer_endpoint.port, 0, 0));
            match sock.try_send_to(msg, addr) {
                Ok(_) => {
                    stats.packets_sent += 1;
                    stats.bytes_sent += msg.len();
                    ()
                },
                Err(ref e) if e.kind() == tokio::io::ErrorKind::WouldBlock => (), // not writable, drop
                Err(error) => eprintln!("{} Socket error: {:?} sending to addr: {:?}", ctx_str, error, addr),
            }
        }
        fn send_noise_msg(ctx_str: &str, transport: &mut PeerTransport, snow_state: &mut snow::StatelessTransportState, sock: &tokio::net::UdpSocket, peer_endpoint: SecureUdpEndpoint, send_buf2: &mut [u8], msg: &[u8], stats: &mut NetworkStats) {
            let mut o = 0;
            o += transport.nonce.write_to(                           &mut send_buf2[o..]);
            o += snow_state     .write_message(transport.nonce, msg, &mut send_buf2[o..]).unwrap();
            send_sock_msg(ctx_str, transport, sock, peer_endpoint, &send_buf2[..o], stats);

            transport.nonce += 1;
        }

        fn process_acks(transport: &mut PeerTransport, header: PacketHeader) {
            let now = Instant::now();

            // println!("Packet ack! {}!", header.ack());

            let mut ack_field = header.ack_field;
            let ack = header.ack() as usize;

            // TODO(Phil): figure out the off-by-one situation here...
            if ack >= 64 {
                for i in ack-64..ack+1 {
                    let is_acknowledged = (ack_field >> 63) & 1 != 0;
                    ack_field <<= 1;
                    if !is_acknowledged { continue; }

                    if let Some(sent_packet) = transport.sent_packets.at(i) &&
                                sent_packet.bytes > 0 {
                        transport.acknowledged_sent_packets.set(AcknowledgedSentPacket { bytes: sent_packet.bytes, rtt: now.duration_since(sent_packet.time_sent).as_secs_f64() }, ack);

                        // println!("Packet acked!: {} bytes! {}", sent_packet.bytes, i + 64 - ack);

                        *sent_packet = SentPacket::default();
                    }
                }
            }

            // let rough_packet_loss = 1.0 - (header.ack_field.count_ones() as f64 / 64.0);
            // println!("Packet loss: {}%.", rough_packet_loss * 100.0);
        }

        if net_stats_window_start.elapsed() >= ONE_SECOND {
            net_stats = NetworkStats::default();
            net_stats_window_start = tokio::time::Instant::now();
        }

        let was_now = tokio::time::Instant::now();
        if was_now > next_tick_time {
            loop {
                // TICK CODE
                unknown_peers.retain(|peer| {
                    if peer.watch_dog.elapsed() > TIMEOUT_DURATION {
                        if PRINT_PROTOCOL { println!("{:05}: Disconnected from unknown peer {:?}", my_port, peer.endpoint); }
                        false
                    } else { true }
                });
                for peer in &mut peers {
                    if peer.watch_dog.elapsed() > TIMEOUT_DURATION {
                        if peer.snow_state.is_some() {
                            if PRINT_PROTOCOL { println!("{:05}: Disconnected from peer {:?}", my_port, peer.endpoint); }
                        }
                        peer.outgoing_handshake_state = None;
                        peer.pending_client_ack_snow_state = None;
                        peer.snow_state = None;
                        peer.watch_dog = Instant::now();
                        peer.unacted_upon_status_height = None;
                        peer.latest_status = None;
                    }

                    if let (Some(peer_endpoint), Some(snow_state)) = (peer.endpoint, &mut peer.snow_state) {
                        if peer.connection_is_unknown {
                            // Gossip evidence in order to trigger upgrade
                            if let Some(evidence) = my_endpoint_evidence {
                                let header = PacketHeader::new::<PACKET_TYPE_ENDPOINT_EVIDENCE>(peer.transport.ack_latest, peer.transport.ack_field); // @TodoHeaderAndStatus
                                let mut o  = 0;
                                o += header  .write_to(&mut send_buf1[o..]);
                                o += evidence.write_to(&mut send_buf1[o..]);
                                print_packet_tag_send(header);
                                send_noise_msg(&ctx_str, &mut peer.transport, snow_state, &sock, peer_endpoint, &mut send_buf2, &send_buf1[..o], &mut net_stats);
                            }
                        }
                        let header = PacketHeader::new::<PACKET_TYPE_EMPTY>(peer.transport.ack_latest, peer.transport.ack_field);
                        let mut o  = 0;
                        o += write_header_and_maybe_status(header, true, &bft_state, &roster, &mut send_buf1[..], peer.transport.nonce);
                        print_packet_tag_send(header);
                        send_noise_msg(&ctx_str, &mut peer.transport, snow_state, &sock, peer_endpoint, &mut send_buf2, &send_buf1[..o], &mut net_stats);
                    }
                }
                for peer in &mut peers {
                    if let Some(peer_endpoint) = peer.endpoint {
                        if peer.snow_state.is_none() && peer.outgoing_handshake_state.is_none() && peer.pending_client_ack_snow_state.is_none() {
                            let mut outgoing_state: snow::HandshakeState = snow::Builder::new(noise_params.clone())
                                .local_private_key(&my_static_keypair.private).unwrap()
                                .remote_public_key(&peer_endpoint.public_key).unwrap()
                                .build_initiator().unwrap();
                            peer.transport = PeerTransport::default();
                            let header = PacketHeader::new::<PACKET_TYPE_CLIENT_HELLO>(peer.transport.ack_latest, peer.transport.ack_field); // @TodoHeaderAndStatus
                            let mut o = 0;
                            o += header.write_to(&mut send_buf1[o..]);
                            let n = outgoing_state.write_message(&send_buf1[..o], &mut send_buf2).unwrap();
                            // TODO: no nonce?
                            print_packet_tag_send(header);
                            send_sock_msg(&ctx_str, &mut peer.transport, &sock, peer_endpoint, &send_buf2[..n], &mut net_stats);
                            peer.outgoing_handshake_state = Some(outgoing_state);
                        }

                        if let (Some(snow_state), Some(evidence)) =
                            (&mut peer.snow_state, roster_endpoint_evidence.choose(&mut base_rng)) {
                            let header = PacketHeader::new::<PACKET_TYPE_ENDPOINT_EVIDENCE>(peer.transport.ack_latest, peer.transport.ack_field); // @TodoHeaderAndStatus
                            let mut o = 0;
                            o += header  .write_to(&mut send_buf1[o..]);
                            o += evidence.write_to(&mut send_buf1[o..]);
                            print_packet_tag_send(header);
                            send_noise_msg(&ctx_str, &mut peer.transport, snow_state, &sock, peer_endpoint, &mut send_buf2, &send_buf1[..o], &mut net_stats);
                        }
                    }
                }

                // BFT CONSENSUS
                // account for the state updates we've accumulated
                bft_state.bft_update(&mut roster).await;

                {
                    let active_roster = &roster[..active_roster_len(&roster)];
                    // Drop those who are not in the active roster.
                    peers.retain(|p| active_roster.iter().position(|rp| rp.pub_key.0 == p.root_public_bft_key).is_some());
                    for rp in active_roster {
                        if peers.iter().position(|p| p.root_public_bft_key == rp.pub_key.0).is_none() && rp.pub_key.0 != my_root_private_key.as_ref() {
                            peers.push(Peer { root_public_bft_key: rp.pub_key.0, ..Peer::default() });
                        }
                    }
                }

                fn send_round_data_to_peer(bft_state: &TMState, should_send_prevotes: bool, round_data: &RoundData, ctx_str: &str, send_buf1: &mut [u8], send_buf2: &mut [u8], peer_transport: &mut PeerTransport, peer_endpoint: SecureUdpEndpoint, peer_snow_state: &mut snow::StatelessTransportState, peer_root_public_bft_key: [u8; 32], sock: &tokio::net::UdpSocket, stats: &mut NetworkStats) {
                    let height = round_data.height;
                    let round  = round_data.round;

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

                                let header = PacketHeader::new::<PACKET_TYPE_PROPOSAL_CHUNK>(peer_transport.ack_latest, peer_transport.ack_field); // @TodoHeaderAndStatus

                                let mut o = 0;
                                o += header   .write_to(&mut send_buf1[o..]);
                                o += chunk_hdr.write_to(&mut send_buf1[o..]);

                                let (chunk_o, chunk_size) = round_data.proposal.chunk_o_size(chunk_i);
                                o += round_data.proposal.0[chunk_o..chunk_o + chunk_size].write_to(&mut send_buf1[o..]);
                                let sig_o = o;
                                o += round_data.proposal_sigs[chunk_i].0.write_to(&mut send_buf1[o..]);

                                #[cfg(debug_assertions)] // self-check signatures as sanity check
                                match round_data.proposal_sigs[chunk_i].verify(proposer_pub_key, &send_buf1[PACKET_HEADER_SIZE..sig_o]) {
                                    Ok(_) => {}
                                    Err((err, str)) => {
                                        eprintln!("{ctx_str}: \x1b[91mBFT FAULT\x1b[0m: {str} [..{}]: for proposal from {proposer_pub_key:?} {height}.{round}.{chunk_i}: {} {err}", sig_o-1, chunk_hdr.proposal_id);
                                        continue;
                                    }
                                }

                                if PRINT_SENDS { eprintln!("{} sending proposal chunk {} to {:?}", ctx_str, chunk_i, peer_root_public_bft_key); }
                                sent_chunk_cs += 1;
                                print_packet_tag_send(header);
                                send_noise_msg(&ctx_str, peer_transport, peer_snow_state, &sock, peer_endpoint, send_buf2, &mut send_buf1[..o], stats);
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

                        let header = PacketHeader::new_(PACKET_TYPE_PREVOTE_SIGNATURES + is_precommit, peer_transport.ack_latest, peer_transport.ack_field); // @TodoHeaderAndStatus
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
                                    eprintln!("{ctx_str}: \x1b[91mBFT ERROR\x1b[0m: {} from {roster_i} - not in roster {}.{}: {}", ["prevote", "precommit"][is_precommit], packet.height, packet.round, packet.value_id);
                                    return;
                                };
                                let pub_key = member.pub_key;
                                let sign_datas = make_vote_sign_datas(pub_key.0, is_precommit != 0, packet.height, packet.round, packet.value_id);
                                let sign_data  = &sign_datas[(i >= packet.no_votes_n as usize) as usize];
                                match sig.verify(pub_key, sign_data) { Ok(_)=>{} Err((err, str)) => {
                                    eprintln!("{ctx_str}: \x1b[91mBFT FAULT\x1b[0m: {str} [..{}]: for {} from {roster_i}-{pub_key:?} {}.{}: {} {err}",
                                        sign_data.len(), ["prevote", "precommit"][is_precommit], packet.height, packet.round, packet.value_id);
                                }}
                            }
                        }

                        for roster_i in 0..round_data.msg_val_sigs.len() {
                            let (value_id, sig) = round_data.msg_val_sigs[roster_i][is_precommit as usize];
                            if sig != TMSig::NIL {
                                let pub_key_sig = PubKeySig{ roster_i: roster_i.try_into().unwrap(), sig };
                                // println!("{} {}: packing in sig from {}", ctx_str, PubKeyID(my_root_public_bft_key.into()), pub_key_sig.pub_key);

                                if packet.value_id == ValueId::NIL {
                                    // NOTE(azmr): gossip seen votes even if we haven't seen proposal, but only 1 non-nil value id per packet
                                    packet.value_id = value_id;
                                }
                                if value_id != ValueId::NIL && value_id != packet.value_id {
                                    eprintln!("{}: \x1b[91mBFT FAULT\x1b[0m: local mismatch: {:?} vs {:?}", ctx_str, packet.value_id, value_id);
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
                                    if PRINT_SENDS { println!("{}: sending full {} block: {:#?}", ctx_str, ["prevote", "precommit"][is_precommit as usize], packet); }
                                    dbg_check_votes(ctx_str, &round_data.roster, is_precommit as usize, &packet);

                                    let mut o = 0;
                                    o += header.write_to(&mut send_buf1[o..]);
                                    o += packet.write_to(&mut send_buf1[o..]);
                                    send_noise_msg(ctx_str, peer_transport, peer_snow_state, sock, peer_endpoint, send_buf2, &send_buf1[..o], stats);

                                    packet.no_votes_n  = 0;
                                    packet.yes_votes_n = 0;
                                    packet.votes       = [PubKeySig::NIL; 18];
                                }
                            }
                        }

                        // send any half-filled vote blocks
                        if (packet.no_votes_n + packet.yes_votes_n) > 0 {
                            sent_c[is_precommit as usize] += (packet.no_votes_n + packet.yes_votes_n) as usize;
                            // println!("{}: half-filled block pre-gap-close: {:#?}", ctx_str, packet);
                            // move items from end to fill gap
                            for gap_i in 0..packet.votes.len() - (packet.no_votes_n + packet.yes_votes_n) as usize {
                                packet.votes[packet.no_votes_n as usize + gap_i] = packet.votes[packet.votes.len() - 1 - gap_i];
                            }

                            if PRINT_SENDS { println!("{}: half-filled block post-gap-close: {:#?}", ctx_str, packet); }
                            dbg_check_votes(ctx_str, &round_data.roster, is_precommit as usize, &packet);

                            let mut o = 0;
                            o += header.write_to(&mut send_buf1[o..]);
                            o += packet.write_to(&mut send_buf1[o..]);
                            // TODO: maybe status
                            print_packet_tag_send(header);
                            send_noise_msg(ctx_str, peer_transport, peer_snow_state, sock, peer_endpoint, send_buf2, &send_buf1[..o], stats);
                        }
                    }

                    if PRINT_SEND_CS && sent_chunk_cs > 0 { eprintln!("{} sent {} proposal chunks", ctx_str, sent_chunk_cs); }
                    if PRINT_SEND_CS && sent_c[0]     > 0 { eprintln!("{} sent {} prevotes",        ctx_str, sent_c[0]);     }
                    if PRINT_SEND_CS && sent_c[1]     > 0 { eprintln!("{} sent {} precommits",      ctx_str, sent_c[1]);     }
                }

                if PRINT_PEERS { println!("{} {:?}", ctx_str, peers.iter().map(|p|
                    (PubKeyID(p.root_public_bft_key), p.latest_status.clone(), p.connection_is_unknown)
                ).collect::<Vec<_>>()); }

                if let Some(cmd) = &bft_state.roster_cmd && PRINT_ROSTER_CMD { eprintln!("{ctx_str} recv roster cmd: \"{}\"", cmd); }
                let roster_cmd = bft_state.update_roster_cmd_closure.0(bft_state.roster_cmd.take()).await;
                if let Some(cmd) = &roster_cmd && PRINT_ROSTER_CMD { eprintln!("{ctx_str} send roster cmd: \"{}\"", cmd); }

                for peer_i in 0..peers.len() {
                    let peer = &mut peers[peer_i];
                    if peer.endpoint.is_none() || peer.snow_state.is_none() { continue; }
                    if let Some(height) = peer.unacted_upon_status_height && height < bft_state.height {
                        peer.unacted_upon_status_height = None;
                        send_round_data_to_peer(&bft_state, false, &bft_state.recent_commit_round_cache[height as usize], &ctx_str, &mut send_buf1, &mut send_buf2, &mut peer.transport, peer.endpoint.unwrap(), peer.snow_state.as_mut().unwrap(), peer.root_public_bft_key, &sock, &mut net_stats);
                    }
                    else if let Ok(current_height_start_i) = bft_state.rounds_data.binary_search_by_key(&(bft_state.height, 0), |el| (el.height, el.round))
                    {
                        for round_i in current_height_start_i..bft_state.rounds_data.len()
                        {
                            let round_data = &bft_state.rounds_data[round_i];
                            send_round_data_to_peer(&bft_state, true, &round_data, &ctx_str, &mut send_buf1, &mut send_buf2, &mut peer.transport, peer.endpoint.unwrap(), peer.snow_state.as_mut().unwrap(), peer.root_public_bft_key, &sock, &mut net_stats);
                        }
                    } else {
                        eprintln!("{}: \x1b[91mBFT ERROR\x1b[0m: round_data array was empty", ctx_str);
                    }

                    if let Some(cmd) = &roster_cmd {
                        let header = PacketHeader::new::<PACKET_TYPE_ROSTER_CMD>(peer.transport.ack_latest, peer.transport.ack_field);
                        let mut o = write_header_and_maybe_status(header, false, &bft_state, &roster, &mut send_buf1[..], peer.transport.nonce);
                        o        += cmd.as_bytes().write_to(&mut send_buf1[o..]);
                        print_packet_tag_send(header);
                        send_noise_msg(&ctx_str, &mut peer.transport, peer.snow_state.as_mut().unwrap(), &sock, peer.endpoint.unwrap(), &mut send_buf2, &send_buf1[..o], &mut net_stats);
                    }
                }

                for peer_i in 0..unknown_peers.len() {
                    let peer = &mut unknown_peers[peer_i];
                    if let Some(height) = peer.unacted_upon_status_height && height < bft_state.height {
                        peer.unacted_upon_status_height = None;
                        send_round_data_to_peer(&bft_state, false, &bft_state.recent_commit_round_cache[height as usize], &ctx_str, &mut send_buf1, &mut send_buf2, &mut peer.transport, peer.endpoint, &mut peer.snow_state, [0; 32], &sock, &mut net_stats);
                    }

                    if let Some(cmd) = &roster_cmd {
                        let header = PacketHeader::new::<PACKET_TYPE_ROSTER_CMD>(peer.transport.ack_latest, peer.transport.ack_field);
                        let mut o = write_header_and_maybe_status(header, false, &bft_state, &roster, &mut send_buf1[..], peer.transport.nonce);
                        o        += cmd.as_bytes().write_to(&mut send_buf1[o..]);
                        print_packet_tag_send(header);
                        send_noise_msg(&ctx_str, &mut peer.transport, &mut peer.snow_state, &sock, peer.endpoint, &mut send_buf2, &send_buf1[..o], &mut net_stats);
                    }
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
                        println!("{}: \x1b[92mNET\x1b[0m: {} KB/s | {} packets/s | {} bytes/packet",
                                 ctx_str, kbps, pps, bpp);
                    }
                }

                break;
            }

            let now_now = tokio::time::Instant::now();
            if now_now - next_tick_time > TICK_DURATION {
                next_tick_time = now_now + TICK_DURATION;
            } else {
                next_tick_time += TICK_DURATION;
            }
        }

        fn timeout_drop(transport: &mut PeerTransport) {
            let mut rtt_sum   = 0.0;
            let mut rtt_sum_n = 0.0;
            for slot_i in 0..transport.acknowledged_sent_packets.slots.len() {
                let packet = transport.acknowledged_sent_packets.slots[slot_i].value;
                if packet.rtt > 0.0 {
                    rtt_sum   += packet.rtt;
                    rtt_sum_n += 1.0;
                }
            }

            let mean_rtt = rtt_sum / rtt_sum_n;

            // NOTE(phil): timeout-drop packets.
            // TODO(phil): be more intelligent
            let now = Instant::now();
            for slot_i in 0..transport.sent_packets.slots.len() {
                let packet = &mut transport.sent_packets.slots[slot_i].value;
                if packet.bytes > 0 {
                    let time_since_sent = now.duration_since(packet.time_sent).as_secs_f64();
                    if time_since_sent > mean_rtt * 1.5 { // timeout-drop
                        packet.bytes = 0;
                    }
                }
            }
        }
        for peer in &mut unknown_peers {
            timeout_drop(&mut peer.transport);
        }
        for peer in &mut peers {
            timeout_drop(&mut peer.transport);
        }

        let remaining = next_tick_time.saturating_duration_since(was_now);
        let (length, addr) = match tokio::time::timeout(remaining, sock.recv_from(&mut recv_buf1)).await {
            Err(_elapsed) => continue, // timeout
            Ok(Err(error)) => { println!("Socket error: {:?}", error); continue; },
            Ok(Ok(ret)) => ret,
        };
        if length < 8 + PACKET_HEADER_SIZE { continue; } // early out to simplify nonce code
        let raw_msg = &recv_buf1[..length];

        let from_ip = match addr {
            SocketAddr::V4(v4) => v4.ip().to_ipv6_mapped().octets(),
            SocketAddr::V6(v6) => v6.ip().octets(),
        };
        let from_port = addr.port();

        // DECRYPT
        let mut peer_index = 0;
        let mut peer_is_unknown = false;
        let mut nonce = 0;
        let mut msg: Option<&[u8]> = None;

        //  NOTE(Security): Actually we would need to loop because a peer could sign a message claiming to own an IP and PORT that it actually does not own. That also means falling back on
        //      the unknown connections array since that also shouldn't be able to be blocked.
        if let Some(i) = peers.iter().map(|p| p.endpoint.unwrap_or_default()).position(|endpoint| endpoint.ip_address == from_ip && endpoint.port == from_port) {
            let peer_endpoint = peers[i].endpoint.unwrap();
            loop {
                let peer = &mut peers[i];
                if let Some(snow_state) = &mut peer.snow_state {
                    nonce = u64::from_le_bytes(raw_msg[0..8].try_into().unwrap());
                    if let Ok(length) = snow_state.read_message(nonce, &raw_msg[8..], &mut recv_buf2) {
                        if nonce_is_ok(nonce, peer.transport.ack_latest, peer.transport.ack_field) {
                            msg        = Some(&recv_buf2[0..length]);
                            peer_index = i;
                        }
                        break;
                    }
                }

                if let Some(outgoing) = &mut peer.outgoing_handshake_state {
                    if let Ok(length) = outgoing.read_message(raw_msg, &mut recv_buf2) {
                        fn finish_outgoing_handshake(ctx_str: &str, send_buf1: &mut [u8], send_buf2: &mut [u8], sock: &tokio::net::UdpSocket, peer_endpoint: SecureUdpEndpoint, peer: &mut Peer, mut snow_state: snow::StatelessTransportState, nonce: u64, connection_is_unknown: bool, stats: &mut NetworkStats) {
                            let packet_type = if connection_is_unknown { PACKET_TYPE_CLIENT_UNKNOWN_ACK } else { PACKET_TYPE_CLIENT_ACK };

                            // TODO: we should rate-limit new connections so adversaries can't exhaust your entropy pool by rapidly asking for new nonces
                            peer.transport.nonce = rand::random::<u64>() >> (PACKET_TAG_BITS + 1);

                            let header = PacketHeader::new_(packet_type, 0, 0); // @TodoHeaderAndStatus
                            let mut o = 0;
                            o += header.write_to(&mut send_buf1[o..]);

                            print_packet_tag_send(header);
                            send_noise_msg(ctx_str, &mut peer.transport, &mut snow_state, sock, peer_endpoint, send_buf2, &send_buf1[..o], stats);

                            peer.snow_state                    = Some(snow_state);
                            peer.outgoing_handshake_state      = None;
                            peer.pending_client_ack_snow_state = None;
                            peer.transport.ack_latest          = nonce;
                            peer.transport.ack_field           = !0;
                            peer.connection_is_unknown         = connection_is_unknown;
                        }

                        if length < 8 + PACKET_HEADER_SIZE {
                            break; // presumably we don't care about standalone nonces
                        }

                        nonce = u64::from_le_bytes(recv_buf2[..8].try_into().unwrap());

                        let header_and_local_msg = &recv_buf2[8..length]; // @Duplicate
                        let Ok(header) = PacketHeader::read_from(&header_and_local_msg[..]) else { break; }; // @TodoHeaderAndStatus
                        let packet_type = header.type_();
                        let local_msg = &header_and_local_msg[PACKET_HEADER_SIZE..];

                        print_packet_tag_recv(header);

                        process_acks(&mut peer.transport, header);

                        if packet_type == PACKET_TYPE_SERVER_HELLO {
                            {
                                let them: &[u8; 32] = &peer.root_public_bft_key;
                                let me:   &[u8; 32] = &my_root_public_bft_key.into();

                                if them == me {
                                    eprintln!("\x1b[91mKEY MATCH\x1b[0m: Received a SERVER_HELLO from a peer reporting the same key as me.");
                                    break;
                                }
                            }
                            if peer.pending_client_ack_snow_state.is_none() || !contended_noise_is_initiator(&bft_state.hash_keys, &my_root_public_bft_key.into(), &peer.root_public_bft_key) {
                                if let Ok(snow_state) = peer.outgoing_handshake_state.take().unwrap().into_stateless_transport_mode() {
                                    if PRINT_PROTOCOL { println!("{:05}: Finished outgoing handshake and got nonce {} with {}", my_port, nonce, addr); }
                                    finish_outgoing_handshake(&ctx_str, &mut send_buf1, &mut send_buf2, &sock, peer_endpoint, peer, snow_state, nonce, false, &mut net_stats);
                                }
                                break;
                            }
                        } else if packet_type == PACKET_TYPE_SERVER_UNKNOWN_HELLO && local_msg.len() == 18 {
                            let my_apparent_ip       = &local_msg[  ..16];
                            let my_apparent_port     = &local_msg[16..18];
                            let my_apparent_endpoint = SecureUdpEndpoint { ip_address: my_apparent_ip.try_into().unwrap(), port: u16::from_le_bytes(my_apparent_port.try_into().unwrap()), public_key: my_static_keypair.public };
                            // TODO hash
                            if let Ok(snow_state) = peer.outgoing_handshake_state.take().unwrap().into_stateless_transport_mode() {
                                if PRINT_PROTOCOL { println!("{:05}: Finished outgoing unknown handshake and got nonce {} with {}, I am percieved as {:?}", my_port, nonce, addr, my_apparent_endpoint); }

                                if my_endpoint_evidence.is_none() {
                                    let evidence = EndpointEvidence { endpoint: my_apparent_endpoint, root_public_bft_key: my_root_public_bft_key.into() };
                                    if PRINT_PROTOCOL { println!("{:05}: I am locking in the endpoint evidence {:?}", my_port, evidence); }
                                    my_endpoint_evidence = Some(evidence);
                                }

                                finish_outgoing_handshake(&ctx_str, &mut send_buf1, &mut send_buf2, &sock, peer_endpoint, peer, snow_state, nonce, true, &mut net_stats);
                            }
                            break;
                        }
                    }
                }
                if let Some(incoming) = &mut peer.pending_client_ack_snow_state {
                    nonce = u64::from_le_bytes(raw_msg[..8].try_into().unwrap());
                    if let Ok(length) = incoming.read_message(nonce, &raw_msg[8..], &mut recv_buf2) {
                        let header_and_local_msg = &recv_buf2[..length]; // @Duplicate
                        let Ok(header) = PacketHeader::read_from(&header_and_local_msg[..]) else { break; }; // @TodoHeaderAndStatus
                        let packet_type = header.type_();

                        print_packet_tag_recv(header);

                        process_acks(&mut peer.transport, header);

                        if packet_type == PACKET_TYPE_CLIENT_ACK {
                            if PRINT_PROTOCOL { println!("{:05}: Finished incoming handshake and got nonce {} with {}", my_port, nonce, addr); }
                            peer.snow_state          = peer.pending_client_ack_snow_state.take();
                            peer.outgoing_handshake_state = None;
                            peer.transport.ack_latest         = nonce;
                            peer.transport.ack_field          = !0;
                            peer.connection_is_unknown    = false;
                            break;
                        }
                    }
                }
                let mut incoming_state: snow::HandshakeState = snow::Builder::new(noise_params.clone())
                    .local_private_key(&my_static_keypair.private).unwrap()
                    .build_responder().unwrap();
                if let Ok(length) = incoming_state.read_message(raw_msg, &mut recv_buf2) {
                    let header_and_local_msg = &recv_buf2[..length]; // @Duplicate
                    let Ok(header) = PacketHeader::read_from(&header_and_local_msg[..]) else { break; }; // @TodoHeaderAndStatus
                    let packet_type = header.type_();

                    print_packet_tag_recv(header);

                    process_acks(&mut peer.transport, header);

                    if packet_type == PACKET_TYPE_CLIENT_HELLO {
                        let client_endpoint = SecureUdpEndpoint { public_key: incoming_state.get_remote_static().unwrap().try_into().unwrap(), ip_address: from_ip, port: from_port };
                        if PRINT_PROTOCOL { println!("{:05}: Server received client hello from static key = {:?}", my_port, client_endpoint); }
                        {
                            let them: &[u8; 32] = &peer.root_public_bft_key;
                            let me:   &[u8; 32] = &my_root_public_bft_key.into();

                            if them == me {
                                eprintln!("\x1b[91mKEY MATCH\x1b[0m: Received a CLIENT_HELLO from a peer reporting the same key as me.");
                                break;
                            }
                        }
                        if peer.outgoing_handshake_state.is_none() || contended_noise_is_initiator(&bft_state.hash_keys, &my_root_public_bft_key.into(), &peer.root_public_bft_key) {

                            // TODO: we should rate-limit new connections so adversaries can't exhaust your entropy pool by rapidly asking for new nonces
                            let start_nonce = rand::random::<u64>() >> (PACKET_TAG_BITS + 1);

                            let header = PacketHeader::new::<PACKET_TYPE_SERVER_HELLO>(0, 0); // @TodoHeaderAndStatus

                            let mut o = 0;
                            o += start_nonce.write_to(&mut send_buf1[o..]);
                            o += header     .write_to(&mut send_buf1[o..]);

                            let n = incoming_state.write_message(&send_buf1[..o], &mut send_buf2).unwrap();

                            // NOTE(Phillip): Let me know if there is an important reason to send the sock message when unsuccessfully entering stateless transport mode
                            if let Ok(snow_state) = incoming_state.into_stateless_transport_mode() {
                                peer.transport.nonce = start_nonce; // @Cleanup @Lazy.
                                print_packet_tag_send(header);
                                send_sock_msg(&ctx_str, &mut peer.transport, &sock, peer_endpoint, &send_buf2[..n], &mut net_stats);
                                peer.transport.nonce += 1;
                                peer.pending_client_ack_snow_state = Some(snow_state);
                            }
                            break;
                        }
                    }
                }
                break;
            }
        } else {
            loop {
                if let Some(i) = unknown_peers.iter().position(|p| p.endpoint.ip_address == from_ip && p.endpoint.port == from_port) {
                    let peer = &mut unknown_peers[i];
                    nonce = u64::from_le_bytes(raw_msg[..8].try_into().unwrap());
                    if let Ok(length) = peer.snow_state.read_message(nonce, &raw_msg[8..], &mut recv_buf2) {
                        let header_and_local_msg = &recv_buf2[..length]; // @Duplicate
                        let Ok(header) = PacketHeader::read_from(&header_and_local_msg[..]) else { break; }; // @TodoHeaderAndStatus
                        let packet_type = header.type_();

                        print_packet_tag_recv(header);

                        process_acks(&mut peer.transport, header);

                        if peer.pending_client_ack {
                            if packet_type == PACKET_TYPE_CLIENT_UNKNOWN_ACK {
                                if PRINT_PROTOCOL { println!("{:05}: Finished incoming unknown handshake and got nonce {} with {}", my_port, nonce, addr); }
                                peer.pending_client_ack = false;
                                peer.transport.ack_latest   = nonce;
                                peer.transport.ack_field    = !0;
                                break;
                            }
                            break;
                        }

                        if nonce_is_ok(nonce, peer.transport.ack_latest, peer.transport.ack_field) {
                            msg             = Some(&header_and_local_msg);
                            peer_index      = i;
                            peer_is_unknown = true;
                        }
                        break;
                    }
                }
                let mut incoming_state: snow::HandshakeState = snow::Builder::new(noise_params.clone())
                    .local_private_key(&my_static_keypair.private).unwrap()
                    .build_responder().unwrap();
                if let Ok(length) = incoming_state.read_message(raw_msg, &mut recv_buf2) {
                    let header_and_local_msg = &recv_buf2[..length]; // @Duplicate
                    let Ok(header) = PacketHeader::read_from(&header_and_local_msg[..]) else { break; }; // @TodoHeaderAndStatus
                    let packet_type = header.type_();

                    print_packet_tag_recv(header);

                    if packet_type == PACKET_TYPE_CLIENT_HELLO {
                        let client_endpoint = SecureUdpEndpoint { public_key: incoming_state.get_remote_static().unwrap().try_into().unwrap(), ip_address: from_ip, port: from_port };
                        if PRINT_PROTOCOL { println!("{:05}: Server received client hello from unknown peer with static key = {:?}", my_port, client_endpoint); }

                        // TODO: we should rate-limit new connections so adversaries can't exhaust your entropy pool by rapidly asking for new nonces
                        let start_nonce = rand::random::<u64>() >> (PACKET_TAG_BITS + 1);

                        let header = PacketHeader::new::<PACKET_TYPE_SERVER_UNKNOWN_HELLO>(0, 0); // @TodoHeaderAndStatus

                        let mut o = 0;
                        o += start_nonce.write_to(&mut send_buf1[o..]);
                        o += header     .write_to(&mut send_buf1[o..]);
                        o += from_ip    .write_to(&mut send_buf1[o..]);
                        o += from_port  .write_to(&mut send_buf1[o..]);

                        let n = incoming_state.write_message(&send_buf1[..o], &mut send_buf2).unwrap();

                        // NOTE(Phillip): Let me know if there is an important reason to send the sock message when unsuccessfully entering stateless transport mode
                        if let Ok(stateless_snow_state) = incoming_state.into_stateless_transport_mode() {
                            let mut transport = PeerTransport::default();
                            transport.nonce = start_nonce;
                            print_packet_tag_send(header);
                            send_sock_msg(&ctx_str, &mut transport, &sock, client_endpoint, &send_buf2[..n], &mut net_stats);
                            transport.nonce += 1;
                            unknown_peers.push(UnknownPeer { endpoint: client_endpoint, snow_state: stateless_snow_state, pending_client_ack: true, watch_dog: Instant::now(), transport, unacted_upon_status_height: None });
                        }
                        break;
                    }
                }
                break;
            }
        }
        if msg.is_none() { continue; }
        let msg: &[u8] = msg.unwrap();
        if msg.len() == 0 { continue; }
        let Ok((header, status, read_o)) = read_header_and_maybe_status(&msg[..]) else {
            continue;
        };
        let packet_type = header.type_();

        if peer_is_unknown {
            let peer = &mut unknown_peers[peer_index];
            peer.watch_dog = Instant::now();
            nonce_update(nonce, &mut peer.transport.ack_latest, &mut peer.transport.ack_field);

            process_acks(&mut peer.transport, header);

            if let Some(status) = status {
                peer.unacted_upon_status_height = Some(status.height);
            }

            match packet_type {
                PACKET_TYPE_ENDPOINT_EVIDENCE => match EndpointEvidence::read_from(&msg[read_o..]) {
                    Ok(evidence) => if let Some(i) = peers.iter().position(|p| p.root_public_bft_key == evidence.root_public_bft_key) {
                        peers[i].endpoint = Some(evidence.endpoint);
                        if peer.endpoint == evidence.endpoint {
                            if PRINT_PROTOCOL { println!("{:05}: Promoting unknown peer connection {:?}", my_port, peer.endpoint); }
                            let peer = unknown_peers.remove(peer_index);
                            peers[i].outgoing_handshake_state           = None;
                            peers[i].pending_client_ack_snow_state = None;
                            peers[i].snow_state                    = Some(peer.snow_state);
                            peers[i].watch_dog                          = Instant::now();
                            peers[i].transport                          = peer.transport;
                            peers[i].connection_is_unknown              = false;
                        }
                        roster_endpoint_evidence.retain(|e| e.root_public_bft_key != evidence.root_public_bft_key);
                        roster_endpoint_evidence.push(evidence);
                    }
                    Err(err) => eprintln!("{:05}: couldn't read endpoint evidence: {}", my_port, err),
                },
                PACKET_TYPE_ROSTER_CMD => if msg[read_o..].len() > 0 {
                    if let Ok(cmd) = String::from_utf8(msg[read_o..].to_vec()) { bft_state.roster_cmd = Some(cmd); }
                },
                PACKET_TYPE_EMPTY => (),
                _ => (), //println!("{:05}:  From unknown peer!   field={:016X} packet_type=0x{:X} Got '{:?}' from {}", my_port, peer.transport.ack_field, packet_type, msg, addr),
            }
            continue;
        }

        else {
            let peer = &mut peers[peer_index];
            peer.watch_dog = Instant::now();
            nonce_update(nonce, &mut peer.transport.ack_latest, &mut peer.transport.ack_field);

            process_acks(&mut peer.transport, header);

            // TODO: other TAGs should also cause this transition
            if let Some(status) = status {
                if peer.connection_is_unknown {
                    if PRINT_PROTOCOL { println!("{:05}: Got a status, this means that the other side does not consider me unknown anymore!", my_port); }
                    peer.connection_is_unknown = false;
                }

                peer.unacted_upon_status_height = Some(status.height);
                peer.latest_status = Some(status);
            }

            const_assert!(PACKET_TYPE_PREVOTE_SIGNATURES + 1 == PACKET_TYPE_PRECOMMIT_SIGNATURES);
            match packet_type {
                PACKET_TYPE_ENDPOINT_EVIDENCE => match EndpointEvidence::read_from(&msg[read_o..]) {
                    Ok(evidence) => if let Some(i) = peers.iter().position(|p| p.root_public_bft_key == evidence.root_public_bft_key) {
                        peers[i].endpoint = Some(evidence.endpoint);
                        roster_endpoint_evidence.retain(|e| e.root_public_bft_key != evidence.root_public_bft_key);
                        roster_endpoint_evidence.push(evidence);
                    }
                    Err(err) => eprintln!("{:05}: couldn't read endpoint evidence: {}", my_port, err),
                }

                PACKET_TYPE_PROPOSAL_CHUNK => {
                    let hdr = match PacketProposalChunkHeader::read_from(&msg[read_o..]) { Ok(v)=>v, Err(err)=>{
                        eprintln!("{:05}: couldn't read proposal header: {}", my_port, err);
                        continue;
                    }};
                    let proposal_size = hdr.proposal_size as usize;
                    let chunk_i       = hdr.chunk_i       as usize;
                    let chunk_size = usize::min(PROPOSAL_CHUNK_DATA_SIZE, proposal_size - chunk_i * PROPOSAL_CHUNK_DATA_SIZE);
                    let packet_size = chunk_size + PROPOSAL_PACKET_EXTRA;

                    // NOTE: assume for the moment that this is the valid height, we'll check in the subsequent call
                    // ALT:  cache proposer for *current* round
                    if msg.len() == packet_size {
                        if let (Some(roster_i), _) = TMState::proposer_from_height_round(&bft_state.hash_keys, &roster, hdr.height, hdr.round) {
                            let sig_o = PACKET_HEADER_SIZE + PacketProposalChunkHeader::SERIALIZED_SIZE + chunk_size;
                            bft_state.check_and_incorporate_msg(hdr.height, hdr.round, hdr.chunk_i as usize, hdr.proposal_id, hdr.valid_round,
                                &roster, roster_i, packet_type, &msg[read_o..sig_o], TMSig(msg[sig_o..sig_o+64].try_into().unwrap()));
                        }
                    } else {
                        eprintln!("{:05}: couldn't read proposal chunk: incorrect size {}", my_port, msg.len());
                    }
                }

                PACKET_TYPE_PREVOTE_SIGNATURES | PACKET_TYPE_PRECOMMIT_SIGNATURES => match PacketVotes::read_from(&msg[read_o..]) {
                    Ok(packet) => {
                        let is_precommit = packet_type - PACKET_TYPE_PREVOTE_SIGNATURES;
                        let value_ids    = [ ValueId::NIL, packet.value_id ];

                        for vote_i in 0..(packet.no_votes_n + packet.yes_votes_n) as usize {
                            // Note(Sam): We can change the format of votes to be cool and branchless after the workshop.
                            if let Some(roster_member) = roster.get(packet.votes[vote_i].roster_i as usize) {
                                let sign_datas   = make_vote_sign_datas(roster_member.pub_key.0, is_precommit != 0, packet.height, packet.round, packet.value_id);
                                let no_yes_i = (vote_i >= packet.no_votes_n as usize) as usize;
                                bft_state.check_and_incorporate_msg(packet.height, packet.round, 0, value_ids[no_yes_i], -2,
                                    &roster, packet.votes[vote_i].roster_i as usize, packet_type, &sign_datas[no_yes_i], TMSig(packet.votes[vote_i].sig.0));
                            }
                        }
                    }
                    Err(err) => eprintln!("{:05}: couldn't read {}: {}", my_port, packet_name_from_tag(packet_type), err),
                }

                PACKET_TYPE_ROSTER_CMD => if msg[read_o..].len() > 0 {
                    if let Ok(cmd) = String::from_utf8(msg[read_o..].to_vec()) { bft_state.roster_cmd = Some(cmd); }
                },
                PACKET_TYPE_EMPTY => {}
                _ => {} // println!("{}:  From known peer!   field={:016X} Got '{:?}' from {}", my_port, peer.transport.ack_field, msg, addr);
            }
            continue;
        }
    }
}

// network
const PACKET_TYPE_EMPTY                : u8 =  0;
const PACKET_TYPE_CLIENT_HELLO         : u8 =  1;
const PACKET_TYPE_CLIENT_UNKNOWN_ACK   : u8 =  2;
const PACKET_TYPE_CLIENT_ACK           : u8 =  3;
const PACKET_TYPE_SERVER_UNKNOWN_HELLO : u8 =  4;
const PACKET_TYPE_SERVER_HELLO         : u8 =  5;
const PACKET_TYPE_ENDPOINT_EVIDENCE    : u8 =  6;
// consensus
const PACKET_TYPE_PROPOSAL_CHUNK       : u8 =  7;
const PACKET_TYPE_PREVOTE_SIGNATURES   : u8 =  8;
const PACKET_TYPE_PRECOMMIT_SIGNATURES : u8 =  9;
// misc
const PACKET_TYPE_ROSTER_CMD           : u8 = 10;
const PACKET_TYPE_COUNT                : u8 = 11;


const PACKET_TYPE_BITS                 : u8 =  7;
const PACKET_TYPE_MASK                 : u8 = (1 << PACKET_TYPE_BITS) - 1;

const PACKET_TAG_STATUS_SHIFT          : u8 = PACKET_TYPE_BITS;
const PACKET_TAG_STATUS_FLAG           : u8 = 1 << PACKET_TAG_STATUS_SHIFT;

const PACKET_TAG_BITS                  : u8 = 8;
const PACKET_TAG_MASK                  : u8 = ((1 << PACKET_TAG_BITS as u64) - 1) as u8;


const PACKET_TYPE_NAMES: [[&str; 2]; PACKET_TYPE_COUNT as usize] = {
    let mut names = [["<MISSING>"; 2]; PACKET_TYPE_COUNT as usize];
    names[PACKET_TYPE_EMPTY                as usize] = ["<EMPTY>",              "STATUS"];
    names[PACKET_TYPE_CLIENT_HELLO         as usize] = ["CLIENT_HELLO",         "STATUS+CLIENT_HELLO"];
    names[PACKET_TYPE_CLIENT_UNKNOWN_ACK   as usize] = ["CLIENT_UNKNOWN_ACK",   "STATUS+CLIENT_UNKNOWN_ACK"];
    names[PACKET_TYPE_CLIENT_ACK           as usize] = ["CLIENT_ACK",           "STATUS+CLIENT_ACK"];
    names[PACKET_TYPE_SERVER_UNKNOWN_HELLO as usize] = ["SERVER_UNKNOWN_HELLO", "STATUS+SERVER_UNKNOWN_HELLO"];
    names[PACKET_TYPE_SERVER_HELLO         as usize] = ["SERVER_HELLO",         "STATUS+SERVER_HELLO"];
    names[PACKET_TYPE_ENDPOINT_EVIDENCE    as usize] = ["ENDPOINT_EVIDENCE",    "STATUS+ENDPOINT_EVIDENCE"];
    names[PACKET_TYPE_PROPOSAL_CHUNK       as usize] = ["PROPOSAL_CHUNK",       "STATUS+PROPOSAL_CHUNK"];
    names[PACKET_TYPE_PREVOTE_SIGNATURES   as usize] = ["PREVOTE_SIGNATURES",   "STATUS+PREVOTE_SIGNATURES"];
    names[PACKET_TYPE_PRECOMMIT_SIGNATURES as usize] = ["PRECOMMIT_SIGNATURES", "STATUS+PRECOMMIT_SIGNATURES"];
    names[PACKET_TYPE_ROSTER_CMD           as usize] = ["PREVOTE_SIGNATURES",   "STATUS+PREVOTE_SIGNATURES"];
    const_assert!(PACKET_TYPE_COUNT == 11); // keep names array updated when adding other tags
    names
};
fn packet_name_from_tag(packet_tag: u8) -> &'static str {
    PACKET_TYPE_NAMES.get(packet_tag as usize).unwrap_or(&["<UNKNOWN>", "STATUS+<UNKNOWN>"])[(packet_tag >> PACKET_TAG_STATUS_SHIFT & 1) as usize]
}
fn print_packet_tag_send(header: PacketHeader) {
    if header.type_() >= PACKET_TYPE_PROPOSAL_CHUNK { return; } // @Debug
    if PRINT_PROTOCOL_TAG { println!("PACKET_{} ->", packet_name_from_tag(header.tag())); }
}
fn print_packet_tag_recv(header: PacketHeader) {
    if header.type_() >= PACKET_TYPE_PROPOSAL_CHUNK { return; } // @Debug
    if PRINT_PROTOCOL_TAG { println!("<- PACKET_{}", packet_name_from_tag(header.tag())); }
}

// NOTE(azmr): could add packet sizes so we can check all sizes in 1 location

// ALT: if we limit to u16 chunk indexes & have ~1KB chunk data per packet, we could have block sizes up to ~65MB
// N.B. with ranges like this, we either want to be half-exclusive & not allow type::MAX values, or use a special value for empty (e.g. hi < lo)
type ProposalRng = [u32; 2]; // [lo, hi)
type VoteRng     = [u16; 2];
const STATUS_PROPOSAL_RNGS_N: usize = 1;
const STATUS_VOTE_RNGS_N: usize = 1; // ALT: split prevote/precommit numbers
#[derive(Clone, Debug)]
struct PacketStatus {
    height: u64,
    round:  u32, // as context for following request ranges
    need_proposal_chunk_rngs: [ProposalRng; STATUS_PROPOSAL_RNGS_N],
    need_vote_rngs: [[VoteRng; STATUS_VOTE_RNGS_N]; 2], // 1 for prevote, 1 for precommit
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

    pub fn read_from<R: Read>(mut r: R) -> std::io::Result<Self> {
        let mut packet = Self {
            height: 0, round: 0,
            need_proposal_chunk_rngs: [[0;2]; STATUS_PROPOSAL_RNGS_N],
            need_vote_rngs: [[[0;2]; STATUS_VOTE_RNGS_N]; 2],
        };
        packet.height = r.read_u64::<LittleEndian>()?;
        packet.round = r.read_u32::<LittleEndian>()?;
        for chunk_rng in &mut packet.need_proposal_chunk_rngs {
            chunk_rng[0] = r.read_u32::<LittleEndian>()?;
            chunk_rng[1] = r.read_u32::<LittleEndian>()?;
        }
        for is_precommit in 0..2 {
            for vote_rng in &mut packet.need_vote_rngs[is_precommit] {
                vote_rng[0] = r.read_u16::<LittleEndian>()?;
                vote_rng[1] = r.read_u16::<LittleEndian>()?;
            }
        }
        Ok(packet)
    }
}

const PACKET_HEADER_SIZE: usize = 8 + 8; // 16
#[derive(Debug, Clone, Copy)]
struct PacketHeader {
    tag_and_ack: u64,
    ack_field: u64,
}
impl PacketHeader {
    const fn assert_valid_tag<const TAG: u8>() {
        assert!((TAG & ! PACKET_TYPE_MASK) == 0);
    }

    pub fn new<const TAG: u8>(ack_latest: u64, ack_field: u64) -> PacketHeader {
        Self::assert_valid_tag::<TAG>();
        Self::new_(TAG, ack_latest, ack_field)
    }
    pub fn new_(tag: u8, ack_latest: u64, ack_field: u64) -> PacketHeader {
        PacketHeader {
            tag_and_ack: tag as u64 | (ack_latest << PACKET_TAG_BITS),
            ack_field,
        }
    }

    pub fn has_status(&self) -> bool { self.tag_and_ack as u8  & PACKET_TAG_STATUS_FLAG != 0 }
    pub fn type_     (&self) -> u8   { self.tag_and_ack as u8  & PACKET_TYPE_MASK            }
    pub fn tag       (&self) -> u8   { self.tag_and_ack as u8  & PACKET_TAG_MASK             }
    pub fn ack       (&self) -> u64  { self.tag_and_ack       >> PACKET_TAG_BITS }

    pub fn write_to(&self, buf: &mut [u8]) -> usize {
        let mut o = 0;
        o += self.tag_and_ack.write_to(&mut buf[o..]);
        o += self.ack_field  .write_to(&mut buf[o..]);
        o
    }

    pub fn read_from<R: Read>(mut r: R) -> std::io::Result<Self> {
        let tag_and_ack = r.read_u64::<LittleEndian>()?;
        let ack_field   = r.read_u64::<LittleEndian>()?;
        Ok(Self {
            tag_and_ack,
            ack_field,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct PubKeySig { roster_i: u16, sig: TMSig, }
impl PubKeySig { const NIL: Self = Self{ roster_i: u16::MAX, sig: TMSig::NIL }; }

// ALT: common consensus packet header: { packet header, height, round, value_id }

// agnostic to prevote/precommit - communicated elsewhere
// NOTE: all votes for the same value_id (or nil)
// #[repr(C)]
#[derive(Debug)]
struct PacketVotes {
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

    pub fn read_from<R: Read>(mut r: R) -> std::io::Result<Self> {
        let mut packet = PacketVotes {
            no_votes_n: 0, yes_votes_n: 0, round: 0, height: 0,
            value_id: ValueId::NIL,
            votes: [PubKeySig::NIL; 18],
        };
        packet.no_votes_n  = r.read_u8()?;
        packet.yes_votes_n = r.read_u8()?;
        packet.round       = r.read_u32::<LittleEndian>()?;
        packet.height      = r.read_u64::<LittleEndian>()?;
        r.read_exact(&mut packet.value_id.0)?;
        for i in 0..(packet.no_votes_n + packet.yes_votes_n) as usize {
            packet.votes[i].roster_i = r.read_u16::<LittleEndian>()?;
            r.read_exact(&mut packet.votes[i].sig.0)?;
        }
        Ok(packet)
    }
}

const PROPOSAL_PACKET_EXTRA:    usize = (PACKET_HEADER_SIZE + 56 + 64);
const PROPOSAL_CHUNK_DATA_SIZE: usize = PATH_MTU - PROPOSAL_PACKET_EXTRA;

// NOTE(azmr): this is:
// - conservative in terms of max chunks, value_id, & arrival order
// - assuming a fixed total proposal size
#[derive(Debug)]
struct PacketProposalChunkHeader {
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

    pub fn read_from<R: Read>(mut r: R) -> std::io::Result<Self> {
        let mut packet = PacketProposalChunkHeader {
            chunk_i: 0, proposal_size: 0, round: 0, height: 0, valid_round: 0, proposal_id: ValueId::NIL
        };
        packet.chunk_i       = r.read_u32::<LittleEndian>()?;
        packet.proposal_size = r.read_u32::<LittleEndian>()?;
        packet.round         = r.read_u32::<LittleEndian>()?;
        let valid_round      = r.read_u32::<LittleEndian>()?;
        packet.height        = r.read_u64::<LittleEndian>()?;
        r.read_exact(&mut packet.proposal_id.0)?;

        packet.valid_round = if valid_round != u32::MAX { valid_round.into() } else { -1 };
        Ok(packet)
    }
}

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
            if i == n {
                eprintln!("...");
            }

            std::process::abort();
        }
    }))
}

#[derive(Clone)]
pub struct RustIsBadRngWrapper(pub ChaCha20Rng);
impl snow::types::Random for RustIsBadRngWrapper {
    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), snow::Error> {
        self.0.fill(dest);
        Ok(())
    }
}
pub struct SnowRngResolver {
    pub rng: RustIsBadRngWrapper,
}

impl SnowRngResolver {
    pub fn seed_from_u64(seed: u64) -> SnowRngResolver {
        SnowRngResolver { rng: RustIsBadRngWrapper(ChaCha20Rng::seed_from_u64(seed)) }
    }
}

impl CryptoResolver for SnowRngResolver {
    fn resolve_rng(&self) -> Option<Box<dyn snow::types::Random>> {
        Some(Box::new(self.rng.clone()))
    }
    fn resolve_dh(&self, choice: &snow::params::DHChoice) -> Option<Box<dyn snow::types::Dh>> {
        snow::resolvers::DefaultResolver::resolve_dh(&snow::resolvers::DefaultResolver, choice)
    }
    fn resolve_hash(&self, choice: &snow::params::HashChoice) -> Option<Box<dyn snow::types::Hash>> {
        snow::resolvers::DefaultResolver::resolve_hash(&snow::resolvers::DefaultResolver, choice)
    }
    fn resolve_cipher(&self, choice: &snow::params::CipherChoice) -> Option<Box<dyn snow::types::Cipher>> {
        snow::resolvers::DefaultResolver::resolve_cipher(&snow::resolvers::DefaultResolver, choice)
    }
}

pub fn run_instances(i: usize) {
    let rt = tokio::runtime::Runtime::new().unwrap();

    let seed: u64 = {
        const RNG_SEED_FOR_MULTIPROCESS: u64 = 0xdeadbeef12345;
        RNG_SEED_FOR_MULTIPROCESS
    };

    const N: usize = 4;

    let mut crypto_rng = ChaCha20Rng::seed_from_u64(seed);
    let static_private_keys : Vec<_> = (0..N).map(|_| {
        // NOTE: doing this manually to avoid CryptoRng incompatibilities between different rand_core versions
        let mut secret_key = [0u8; 32];
        crypto_rng.fill_bytes(&mut secret_key);
        SigningKey::from(secret_key)
    }).collect();
    let mut cumulative_stake = 0;
    let roster : Vec<SortedRosterMember> = static_private_keys.iter().enumerate().map(|(i, sk)| {
        let stake = 2000 * (N - 1 - i) as u64;
        cumulative_stake += stake;
        SortedRosterMember { pub_key: PubKeyID(sk.verification_key().into()), stake, cumulative_stake }
    }).collect();
    assert!(roster.is_sorted_by(|a,b| a.stake >= b.stake)); // descending

    if PRINT_ROSTER { println!("Roster: {:?}", roster); }

    let static_keypair_zero = {
        let kp = snow::Builder::with_resolver("Noise_IK_25519_ChaChaPoly_BLAKE2s".parse().unwrap(), Box::new(SnowRngResolver { rng: RustIsBadRngWrapper(crypto_rng.clone()) })).generate_keypair().unwrap();
        StaticDHKeyPair { private: kp.private.try_into().unwrap(), public: kp.public.try_into().unwrap(), }
    };

    let endpoint_zero : SecureUdpEndpoint = {
        let port : u16 = 3030;
        // let ip = "::1".parse::<std::net::Ipv6Addr>().unwrap();
        let ip = "127.0.0.1".parse::<std::net::Ipv4Addr>().unwrap().to_ipv6_mapped();
        SecureUdpEndpoint { ip_address: ip.octets(), port, public_key: static_keypair_zero.public }
    };

    let evidence_zero = {
        EndpointEvidence { endpoint: endpoint_zero, root_public_bft_key: static_private_keys[0].verification_key().into() }
    };

    if i == usize::MAX {
        // let _joins: [; N];
        for j in 0..N {
            if j == 0 {
                rt.spawn(instance(static_private_keys[j], Some(static_keypair_zero), Some(endpoint_zero), roster.clone(), vec![evidence_zero], None));
            } else {
                rt.spawn(instance(static_private_keys[j], None, None, roster.clone(), vec![evidence_zero], None));
            }
        }
    } else if i == 999 {
        // let _joins: [; N];
        for j in 0..N - 1 {
            if j == 0 {
                rt.spawn(instance(static_private_keys[j], Some(static_keypair_zero), Some(endpoint_zero), roster.clone(), vec![evidence_zero], None));
            } else {
                rt.spawn(instance(static_private_keys[j], None, None, roster.clone(), vec![evidence_zero], None));
            }
        }
    } else {
        if i == 0 {
            rt.spawn(instance(static_private_keys[i], Some(static_keypair_zero), Some(endpoint_zero), roster.clone(), vec![evidence_zero], None));
        }
        else if i < N {
            rt.spawn(instance(static_private_keys[i], None, None, roster.clone(), vec![evidence_zero], None));
        }
        else {
            let mut secret_key = [0u8; 32];
            crypto_rng.fill_bytes(&mut secret_key);
            rt.spawn(instance(SigningKey::from(secret_key), None, None, roster.clone(), vec![evidence_zero], None));
        }
    }
    rt.block_on(std::future::pending::<()>())
}

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
    fn check_nonce_is_ok() {
        assert!(nonce_is_ok(124, 12, !0));
        assert!(!nonce_is_ok(12, 124, !0));
        assert!(nonce_is_ok(120, 124, 0xffff_ffff_ffff_ffef));
    }

    #[test]
    fn check_gen_rngs() {
        struct Test {
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

