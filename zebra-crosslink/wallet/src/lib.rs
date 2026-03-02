//! Internal wallet
#![allow(warnings)]

const AUTO_SPEND: bool = false; // automatically make spends without requiring GUI interaction
const DUMP_ACTIONS: bool = false;
const DUMP_FAUCET: bool = false;
const DUMP_NOTES: bool = false;
const DUMP_ROSTER: bool = false;
const DUMP_SYNC: bool = true;
const DUMP_TREES: bool = false;
const DUMP_TX_BUILD: bool = false;
const DUMP_TX_RECV: bool = false;
const DUMP_TX_SEND: bool = false;
const AUDIT_TXS: bool = true;

use orchard::note_encryption::{CompactAction as OrchardCompactAction, OrchardDomain};
use rand::seq::SliceRandom;
use rand::{thread_rng, Rng};
use rand_chacha::rand_core::SeedableRng;
use rand_core::OsRng;
use secrecy::{ExposeSecret, Secret, SecretVec};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::convert::{identity, Infallible};
use std::future::Future;
use std::mem;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_rustls::rustls;
use tonic::client::GrpcService;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint};
use tonic::IntoRequest;
use zcash_client_backend::data_api::chain::{BlockCache, CommitmentTreeRoot};
use zcash_client_backend::data_api::wallet::{
    create_proposed_transactions, propose_shielding, shield_transparent_funds, ConfirmationsPolicy,
    TargetHeight,
};
use zcash_client_backend::data_api::WalletCommitmentTrees;
use zcash_client_backend::proto::service::{
    FaucetRequest, GetSubtreeRootsArg, RawTransaction, TreeState, TxFilter,
};
use zcash_client_backend::wallet::WalletTransparentOutput;
use zcash_client_sqlite::error::SqliteClientError;
use zcash_client_sqlite::util::SystemClock;
use zcash_client_sqlite::{AccountUuid, WalletDb};
use zcash_note_encryption::{
    try_compact_note_decryption, try_note_decryption, try_output_recovery_with_ovk, ShieldedOutput,
};
use zcash_primitives::transaction::builder::{
    self, BuildConfig, BuildResult as TxBuildResult, Builder as TxBuilder,
};
use zcash_primitives::transaction::components::TxOut;
use zcash_primitives::transaction::fees::{self, zip317, FeeRule};
use zcash_primitives::transaction::sighash::{signature_hash, SignableInput};
use zcash_primitives::transaction::txid::TxIdDigester;
use zcash_primitives::transaction::{
    Authorized, StakingAction_BeginDelegationUnbonding, StakingAction_CreateNewDelegationBond,
    StakingAction_RetargetDelegationBond, StakingAction_WithdrawDelegationBond, Transaction,
    TransactionData, TxVersion, Unauthorized,
};
use zcash_primitives::transaction::{RosterMember, StakeTxId, StakingAction, StakingActionKind};
use zcash_proofs::prover::LocalTxProver;
use zcash_protocol::consensus::{BlockHeight as LRZBlockHeight, BranchId};
use zcash_protocol::memo::MemoBytes;
use zcash_protocol::value::{ZatBalance, Zatoshis};
use zcash_protocol::{PoolType, ShieldedProtocol, TxId};
use zcash_transparent::{
    address::TransparentAddress,
    builder::{InputKind as TransparentInputKind, TransparentInputInfo, TransparentSigningSet},
    bundle::OutPoint,
    keys::{IncomingViewingKey, NonHardenedChildIndex, TransparentKeyScope},
};

use rustls::client::danger::ServerCertVerified;
use rustls::client::danger::ServerCertVerifier;
use rustls::crypto::ring;
use rustls::crypto::verify_tls12_signature;
use rustls::crypto::verify_tls13_signature;
use rustls::crypto::CryptoProvider;
use rustls::crypto::WebPkiSupportedAlgorithms;
use rustls::CertificateError;
use tokio::runtime::Builder;
use zcash_client_backend::{
    address::UnifiedAddress,
    data_api::{
        self,
        chain::{error::Error as ChainError, scan_cached_blocks, ChainState},
        scanning::{ScanPriority, ScanRange},
        wallet, Account as APIAccount, AccountBirthday, AccountPurpose, Balance, WalletRead,
        WalletWrite, Zip32Derivation,
    },
    encoding::AddressCodec,
    keys::{
        UnifiedAddressRequest, UnifiedFullViewingKey, UnifiedIncomingViewingKey, UnifiedSpendingKey,
    },
    proto::{
        compact_formats::{
            CompactBlock, CompactOrchardAction, CompactSaplingOutput, CompactSaplingSpend,
            CompactTx,
        },
        service::{
            compact_tx_streamer_client::CompactTxStreamerClient, BlockId, BlockRange, ChainSpec,
            Empty, GetAddressUtxosArg, LightdInfo, TransparentAddressBlockFilter,
        },
    },
};

use zcash_protocol::consensus::{NetworkType, Parameters, MAIN_NETWORK, TEST_NETWORK};

#[derive(Clone)]
pub struct FaucetRequestClosure(
    pub Arc<dyn Fn(String) -> Result<u64, String> + Sync + Send + 'static>,
);
pub static FAUCET_REQUEST: Mutex<Option<FaucetRequestClosure>> = Mutex::new(None);

// NOTE: this has slightly different semantics from the protocol version, hence the different type
// TODO: some code becomes simpler with a u64, but I'm leaving this the same as default for now
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockHeight(pub u32);
impl BlockHeight {
    // NOTE: these constants corresponds to the semantic that the lower-down it is,
    // the more sure we are about its continued existence
    pub const INVALID: Self = Self(u32::MAX); // NOTE: here for headroom for +1 to fake <= using <
                                              // ALT: maybe better to go the other way round: use <= and saturating_sub(1)
    pub const PROPOSED: Self = Self(u32::MAX - 1); // NOTE: no valid txid here
    pub const BUILT: Self = Self(u32::MAX - 2);
    pub const SENT: Self = Self(u32::MAX - 3);
    pub const MEMPOOL: Self = Self(u32::MAX - 4);

    pub fn is_in_block(&self) -> bool {
        self.0 < Self::MEMPOOL.0
    }
    /// assumes non-insane `b`
    pub fn sat_add(&self, b: u32) -> Self {
        if self.is_in_block() {
            Self(self.0 + b)
        } else {
            *self
        }
    }
    pub fn sat_sub(&self, b: u32) -> Self {
        if self.is_in_block() {
            Self(self.0.saturating_sub(b))
        } else {
            *self
        }
    }
}
impl From<LRZBlockHeight> for BlockHeight {
    fn from(h: LRZBlockHeight) -> BlockHeight {
        BlockHeight(<u32>::from(h))
    }
}

impl std::fmt::Debug for BlockHeight {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BlockHeight(")?;
        match *self {
            Self::INVALID => write!(f, "<invalid>"),
            Self::PROPOSED => write!(f, "<proposed>"),
            Self::BUILT => write!(f, "<built>"),
            Self::SENT => write!(f, "<sent>"),
            Self::MEMPOOL => write!(f, "<mempool>"),
            _ => self.0.fmt(f),
        }?;
        write!(f, ")")
    }
}
impl std::fmt::Display for BlockHeight {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            Self::INVALID => write!(f, "<invalid>"),
            Self::PROPOSED => write!(f, "<proposed>"),
            Self::BUILT => write!(f, "<built>"),
            Self::SENT => write!(f, "<sent>"),
            Self::MEMPOOL => write!(f, "<mempool>"),
            _ => self.0.fmt(f),
        }
    }
}

/// "little endian hash"
pub struct LESlice<'a>(pub &'a [u8]);
impl std::fmt::Display for LESlice<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let n = usize::min(self.0.len(), f.precision().unwrap_or(self.0.len()));
        for i in 0..n {
            write!(f, "{:02x}", self.0[31 - i])?;
        }
        Ok(())
    }
}
pub struct LEHash(pub [u8; 32]);
impl std::fmt::Display for LEHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        LESlice(&self.0).fmt(f)
    }
}

// if unspent, this is a UTXO; the notion of a "spent unspent transaction output" is slightly silly
#[derive(Clone, Debug, PartialEq)]
struct Txo {
    pub recv_h: BlockHeight,
    pub spent_h: BlockHeight,
    pub id: OutPoint, // txid + index in tx // (kind of nullifier-like)
    // both from TxOut
    pub value: Zatoshis,
    pub t_addr: TransparentAddress, // convertible to/from pubkey_script
}
impl Txo {
    pub fn txout(&self) -> TxOut {
        TxOut::new(self.value, self.t_addr.script().into())
    }
    pub fn txid(&self) -> TxId {
        *self.id.txid()
    }
}

pub struct NL<'a, T>(pub &'a [T]);
impl<T: std::fmt::Debug> std::fmt::Debug for NL<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut i = 0;
        write!(f, "[")?;
        for it in self.0 {
            write!(f, "\n  {:?}", self.0[i])?;
            i += 1;
        }
        if i > 0 {
            write!(f, "\n]")?;
        } else {
            write!(f, "]")?;
        }
        Ok(())
    }
}

// NOTE: only a function because from() isn't const
// This is u64::MAX in large part to operate correctly on anchor comparison
// ALT: Option
fn unknown_tree_position() -> incrementalmerkletree::Position {
    incrementalmerkletree::Position::from(u64::MAX)
}

// ALT: collapse into Txo with internal enum(s)
#[derive(Clone, Copy, PartialEq)]
struct OrchardNote {
    // NOTE: the reason we want to keep witnesses up-to-date is to increase the time-domain anonymity
    pub recv_h: BlockHeight,
    pub spent_h: BlockHeight,
    pub nf: orchard::note::Nullifier,
    pub txid: TxId,
    // TODO: could be compressed by decomposition
    pub note: orchard::note::Note,
    // TODO: commitment? ephemeralkey?
    pub position: incrementalmerkletree::Position,
    // pub witness: OrchardWitness, // includes position
}
impl std::fmt::Debug for OrchardNote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "OrchardNote{{ recv:{:?}, spent:{:?}, txid:{}, nf:{:?}, value:{}, pos:{}}}",
            self.recv_h,
            self.spent_h,
            self.txid,
            self.nf,
            self.note.value().inner(),
            u64::from(self.position) // u64::from(self.witness.witnessed_position()), self.witness.root()
        )
    }
}

impl OrchardNote {
    fn monotonically_update(&mut self, mut new_note: OrchardNote) {
        if new_note.position < unknown_tree_position() {
            if (self.position < unknown_tree_position() && self.position != new_note.position) {
                println!(
                    "ERROR: orchard note has 2 different valid positions: {:?} vs {:?}",
                    self.position, new_note.position
                );
            }
            // println!("updating note position at {:?} {:?} => {:?}", self.recv_h, self.position, new_note.position);
            self.position = new_note.position;
        } else {
            new_note.position = self.position; // NOTE: just for cmp
        }

        if self != &new_note {
            println!(
                "ERROR: orchard_note mismatch:\n  {:?} vs\n  {:?}",
                self, new_note
            );
        }
    }

    fn value(&self) -> Zatoshis {
        Zatoshis::from_u64(self.note.value().inner()).expect("already validated")
    }
}

struct ProposedTransparentOutput {
    pub dst: TransparentAddress,
    pub zats: Zatoshis,
}

struct ProposedOrchardSpend {
    pub fvk: orchard::keys::FullViewingKey,
    pub note: OrchardNote,
    pub witness_merkle_path: orchard::tree::MerklePath,
}

struct ProposedOrchardOutput {
    pub ovk: Option<orchard::keys::OutgoingViewingKey>,
    pub dst: orchard::Address,
    pub zats: Zatoshis,
    pub memo: MemoBytes,
}

struct BuildPrep {
    block_h: u32,
    build_config: BuildConfig,

    t_keys: TransparentSigningSet,
    s_keys: Vec<sapling_crypto::zip32::ExtendedSpendingKey>,
    o_keys: Vec<orchard::keys::SpendAuthorizingKey>,

    t_inputs: Vec<TransparentInputInfo>,
    t_outputs: Vec<ProposedTransparentOutput>,
    o_inputs: Vec<ProposedOrchardSpend>,
    o_outputs: Vec<ProposedOrchardOutput>,

    staking_action: Option<StakingAction>,
}
impl BuildPrep {
    fn fee_required(&self) -> Result<Zatoshis, builder::FeeError<zip317::FeeError>> {
        // NOTE: we can impl these ourselves
        use zcash_primitives::transaction::fees::transparent::{InputView, OutputView};
        let orchard_actions = orchard::builder::BundleType::DEFAULT
            .num_actions(self.o_inputs.len(), self.o_outputs.len())
            .map_err(|e| builder::FeeError::Bundle(e))?;

        zip317::FeeRule::standard()
            .fee_required(
                &TEST_NETWORK,
                LRZBlockHeight::from_u32(self.block_h),
                self.t_inputs.iter().map(|input| input.serialized_size()),
                self.t_outputs
                    .iter()
                    .map(|output| 8 + output.dst.script().0.len()), // DUP from zcash_primitives/src/transaction/fees/transparent.rs
                0,
                0, // sapling
                orchard_actions,
            )
            .map_err(|e| builder::FeeError::FeeRule(e))
    }
}

struct ProposedTx {
    pub tx: WalletTx,
    pub prep: Option<BuildPrep>,
    pub tx_res: Option<TxBuildResult>,
    pub is_user_faucet: bool, // and NOT an RPC faucet
                              // pub orchard_anchor_h: u32,
                              // pub orchard_fvk: Option<orchard::keys::FullViewingKey>,
                              // pub src_usk: UnifiedSpendingKey,
                              // pub outputs: Vec<TxOutput>,
}
impl ProposedTx {
    const EMPTY: Self = Self {
        tx: WalletTx::EMPTY,
        prep: None,
        tx_res: None,
        is_user_faucet: false,
    };

    fn is_in_progress(&self) -> bool {
        let result = BlockHeight::SENT <= self.tx.h
            && self.tx.h <= BlockHeight::PROPOSED
            && self.tx.is_on_bc();
        result
    }
}

const CHEAT_UNSTAKING: bool = false;

pub static AM_I_THE_UNSTAKER: Mutex<bool> = Mutex::new(false);

pub static GLOBAL_SEED: Mutex<Option<[u8; 32]>> = Mutex::new(None);

pub static TENDERLINK_PUBLIC_KEY: Mutex<[u8; 32]> = Mutex::new([0_u8; 32]);

async fn wait_for_zainod() {
    let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(500));
    for _ in 0..10 {
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(2))
            .timeout(std::time::Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();

        let request_body = r#"{"jsonrpc":"2.0","method":"getinfo","params":[],"id":1}"#;
        let request_builder = client
            .post("http://localhost:18232")
            .header("Content-Type", "application/json")
            .body(request_body);

        if let Ok(res) = request_builder.send().await {
            if res.status().is_success() {
                println!("ZAINO IS READY: {}", res.text().await.unwrap());
                return;
            }
        }

        interval.tick().await;
    }
}

struct Timer<'a> {
    t_bgn: std::time::Instant,
    name: &'a str,
    loud: bool,
}
impl<'a> Timer<'a> {
    pub fn scope_(name: &'a str, loud: bool) -> Self {
        if loud {
            println!("started {}", name);
        }
        Self {
            name,
            t_bgn: std::time::Instant::now(),
            loud,
        }
    }

    pub fn scope(name: &'a str) -> Self {
        Timer::scope_(name, true)
    }
}
impl Drop for Timer<'_> {
    fn drop(&mut self) {
        if self.loud {
            println!("{} took {}ms", self.name, self.t_bgn.elapsed().as_millis());
        }
    }
}

// fn block_policy_10() -> ConfirmationsPolicy { ConfirmationsPolicy::new(std::num::NonZeroU32::new(5).unwrap(), std::num::NonZeroU32::new(5).unwrap(), false).unwrap() }

#[derive(Debug, Clone, PartialEq)]
enum WalletAction {
    RequestFromFaucet,
    TestStakeAction,
    StakeToFinalizer(Zatoshis, [u8; 32]),
    UnstakeFromFinalizer(TxId),
    RetargetBond(TxId, [u8; 32]),
    ClaimBond(TxId),
    SendToAddress(UnifiedAddress, Zatoshis),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WalletTxKind {
    Send,
    Receive,
    Mine,
    SelfSend,
    Shield, // a form of SelfSend
    Stake,
    BeginUnstake,
    Retarget,
    ClaimUnstake,
}

// #[derive(Debug, Clone, Copy, PartialEq)]
// pub enum WalletTxLoc {
//     Internal
//     Mempool,
//     Block(u32), // confirmations or 0 if sidechain
//     Finalized, // by crosslink
// }

// NOTE: needed because we get shielded & transparent transactions at different times
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WalletTxPart {
    // NOTE: spend is our UTXOs that we consume, sent is created from those & transferred
    pub spent_note_count: usize,
    pub spent_zats: Zatoshis,
    // TODO: calculate rather than store
    pub sent_note_count: usize,
    pub sent_zats: Zatoshis,
    pub recv_note_count: usize,
    pub recv_zats: Zatoshis,
    // TODO: differentiate change?
}
impl WalletTxPart {
    pub const TRANSPARENT: usize = 0;
    pub const SHIELDED: usize = 1;
    pub const ZERO: Self = Self {
        spent_note_count: 0,
        spent_zats: Zatoshis::ZERO,
        sent_note_count: 0,
        sent_zats: Zatoshis::ZERO,
        recv_note_count: 0,
        recv_zats: Zatoshis::ZERO,
    };

    pub fn from_staking_action(staking_action: Option<StakingAction>) -> Self {
        let mut b = WalletTxPart::ZERO;

        if let Some(staking_action) = staking_action {
            match staking_action.kind {
                StakingActionKind::Null => (),

                StakingActionKind::CreateNewDelegationBond => {
                    b.sent(
                        Zatoshis::from_u64(staking_action.amount_zats).expect("already converted"),
                        true,
                    )
                    .unwrap();
                    b.recv(
                        Zatoshis::from_u64(staking_action.amount_zats).expect("already converted"),
                        true,
                    )
                    .unwrap();
                }

                StakingActionKind::BeginDelegationUnbonding
                | StakingActionKind::RetargetDelegationBond => {}

                StakingActionKind::WithdrawDelegationBond => {
                    b.spent(
                        Zatoshis::from_u64(staking_action.amount_zats).expect("already converted"),
                        true,
                    )
                    .unwrap();
                }

                StakingActionKind::RegisterFinalizer
                | StakingActionKind::ConvertFinalizerRewardToDelegationBond
                | StakingActionKind::UpdateFinalizerKey => todo!("remaining staking actions"),
            }
        }

        b
    }

    pub fn checked_add(&self, rhs: &WalletTxPart) -> Option<WalletTxPart> {
        Some(WalletTxPart {
            spent_note_count: (self.spent_note_count + rhs.spent_note_count),
            sent_note_count: (self.sent_note_count + rhs.sent_note_count),
            recv_note_count: (self.recv_note_count + rhs.recv_note_count),
            spent_zats: (self.spent_zats + rhs.spent_zats)?,
            sent_zats: (self.sent_zats + rhs.sent_zats)?,
            recv_zats: (self.recv_zats + rhs.recv_zats)?,
        })
    }

    pub fn unchecked_add(&self, rhs: &WalletTxPart) -> WalletTxPart {
        WalletTxPart {
            spent_note_count: (self.spent_note_count + rhs.spent_note_count),
            sent_note_count: (self.sent_note_count + rhs.sent_note_count),
            recv_note_count: (self.recv_note_count + rhs.recv_note_count),
            spent_zats: (self.spent_zats + rhs.spent_zats).expect("already checked"),
            sent_zats: (self.sent_zats + rhs.sent_zats).expect("already checked"),
            recv_zats: (self.recv_zats + rhs.recv_zats).expect("already checked"),
        }
    }

    pub fn checked_sum(parts: &[WalletTxPart]) -> Option<WalletTxPart> {
        let mut v = WalletTxPart::ZERO;
        for part in parts {
            v = v.checked_add(part)?;
        }
        Some(v)
    }

    pub fn unchecked_sum(parts: &[WalletTxPart]) -> WalletTxPart {
        let mut v = WalletTxPart::ZERO;
        for part in parts {
            v = v.unchecked_add(part);
        }
        v
    }

    pub fn spent(&mut self, zats: Zatoshis, loud: bool) -> Option<()> {
        self.spent_note_count += 1;
        match self.spent_zats + zats {
            Some(v) => {
                self.spent_zats = v;
                Some(())
            }
            None => {
                if loud {
                    println!(
                        "note spend error: couldn't add {:?} to {:?}",
                        self.spent_zats, zats
                    );
                }
                None
            }
        }
    }

    pub fn sent(&mut self, zats: Zatoshis, loud: bool) -> Option<()> {
        self.sent_note_count += 1;
        match self.sent_zats + zats {
            Some(v) => {
                self.sent_zats = v;
                Some(())
            }
            None => {
                if loud {
                    println!(
                        "note send error: couldn't add {:?} to {:?}",
                        self.sent_zats, zats
                    );
                }
                None
            }
        }
    }

    pub fn recv(&mut self, zats: Zatoshis, loud: bool) -> Option<()> {
        self.recv_note_count += 1;
        match self.recv_zats + zats {
            Some(v) => {
                self.recv_zats = v;
                Some(())
            }
            None => {
                if loud {
                    println!(
                        "note receive error: couldn't add {:?} to {:?}",
                        self.recv_zats, zats
                    );
                }
                None
            }
        }
    }
    pub fn maybe_recv(&mut self, is_me: bool, zats: Zatoshis, loud: bool) -> Option<()> {
        self.recv_note_count += is_me as usize; // allow branchless
        match self.recv_zats
            + Zatoshis::from_u64(zats.into_u64() * is_me as u64).expect("prev val or 0")
        {
            Some(v) => {
                self.recv_zats = v;
                Some(())
            }
            None => {
                if loud {
                    println!(
                        "note receive error: couldn't add {:?} to {:?}",
                        self.recv_zats, zats
                    );
                }
                None
            }
        }
    }
}

type TxPartFlags = u8;
pub struct TxParts(pub TxPartFlags);
impl TxParts {
    pub const NONE: TxPartFlags = 0;
    pub const TRANSPARENT: TxPartFlags = 1 << WalletTxPart::TRANSPARENT;
    pub const SHIELDED_RECV: TxPartFlags = 1 << WalletTxPart::SHIELDED;
    pub const SHIELDED_SENT: TxPartFlags = 1 << 2;
    pub const MEMO: TxPartFlags = 1 << 3;
    pub const STAKING_ACTION: TxPartFlags = 1 << 4;

    pub const FULL_TX: TxPartFlags = (Self::TRANSPARENT
        | Self::SHIELDED_RECV
        | Self::SHIELDED_SENT
        | Self::MEMO
        | Self::STAKING_ACTION);
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ErrBuf(pub [u8; 128]);
impl ErrBuf {
    pub fn from_str(err_str: &str) -> ErrBuf {
        let mut buf = ErrBuf([0; 128]);
        let err_bytes = err_str.as_bytes();
        let len = err_bytes.len().min(buf.0.len());
        buf.0.copy_from_slice(&err_bytes[..len]);
        buf
    }
    pub fn to_string(&self) -> String {
        String::from_utf8_lossy(&self.0).to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TxStatus {
    OnBc,
    SoftFail(BlockHeight),
    HardFail(BlockHeight, ErrBuf),
}
impl TxStatus {
    pub fn to_any_fail(&self) -> Option<BlockHeight> {
        match self {
            TxStatus::OnBc => None,
            TxStatus::SoftFail(h) => Some(*h),
            TxStatus::HardFail(h, _) => Some(*h),
        }
    }

    pub fn is_on_bc(&self) -> bool {
        match self {
            TxStatus::OnBc => true,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum DevNoteActionKind {
    Recv,
    Unrecv,
    Spend,
    Unspend,
    // OntoBc,
    // OffBc,
    // ChangeHeight,
}
#[derive(Debug, Clone, PartialEq)]
pub enum DevNote {
    Txo(Txo),
    OrchardNote(OrchardNote),
}

#[derive(Debug, Clone, PartialEq)]
pub struct DevNoteAction {
    pub seq: u64,
    pub action_h: BlockHeight,
    pub kind: DevNoteActionKind,
    pub note: DevNote,
    pub tip_h: BlockHeight,
}

// NOTE: trying to not store data that can be computed directly
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WalletTx {
    pub account_id: usize,
    pub txid: zcash_protocol::TxId,
    pub expiry_h: Option<BlockHeight>,
    pub h: BlockHeight, // this is a logical height that is focused on ordering

    // TODO: track whether full Transaction has been read
    pub is_coinbase: bool,
    // NOTE: this is whether we have checked for parts, not whether we have any
    pub part_flags: TxPartFlags,
    pub parts: [WalletTxPart; 2], // 0=>transparent, 1=>shielded(, 2=>bonded?)

    // TODO: keep all memos in single contiguous array as ((txid, index), memo)
    pub memo_count: usize,
    pub memo: [u8; 512],

    pub status: TxStatus,
    pub staking_action: Option<StakingAction>,
}
impl Default for WalletTx {
    fn default() -> Self {
        WalletTx::EMPTY
    }
}

impl WalletTx {
    const EMPTY: WalletTx = WalletTx {
        account_id: 0,
        txid: TxId::from_bytes([0; 32]),
        expiry_h: None,
        h: BlockHeight::INVALID,
        is_coinbase: false,
        status: TxStatus::SoftFail(BlockHeight::INVALID),
        part_flags: 0,
        parts: [WalletTxPart::ZERO; 2],
        memo_count: 0,
        memo: EMPTY_MEMO_BYTES,
        staking_action: None,
    };

    pub fn reported_height(&self) -> BlockHeight {
        self.status.to_any_fail().unwrap_or(self.h)
    }

    pub fn is_on_bc(&self) -> bool {
        self.status.is_on_bc()
    }

    pub fn with_fake_data(
        kind: WalletTxKind,
        sent: u64,
        recv: u64,
        shielding: bool,
        is_outside_bc: bool,
        memo: &str,
        mined_h: u32,
    ) -> Self {
        let mut memo_as_bytes = EMPTY_MEMO_BYTES;
        &memo_as_bytes[0..memo.len()].copy_from_slice(memo.as_bytes());

        Self {
            account_id: 0, //AccountUuid::default(),
            txid: TxId::from_bytes([0; 32]),
            expiry_h: None,
            h: if mined_h != 0 {
                (BlockHeight(mined_h))
            } else {
                BlockHeight::MEMPOOL
            },
            part_flags: TxParts::FULL_TX,
            parts: [
                WalletTxPart {
                    // Transparent
                    spent_note_count: (sent > 0 && shielding) as usize,
                    sent_note_count: (sent > 0 && shielding) as usize,
                    recv_note_count: 0,
                    spent_zats: Zatoshis::from_u64(sent * shielding as u64).unwrap(),
                    sent_zats: Zatoshis::from_u64(sent * shielding as u64).unwrap(),
                    recv_zats: Zatoshis::ZERO,
                },
                WalletTxPart {
                    // Shielded
                    spent_note_count: (sent > 0 && !shielding) as usize,
                    sent_note_count: (sent > 0 && !shielding) as usize,
                    recv_note_count: (recv > 0) as usize,
                    spent_zats: Zatoshis::from_u64(sent * !shielding as u64).unwrap(),
                    sent_zats: Zatoshis::from_u64(sent * !shielding as u64).unwrap(),
                    recv_zats: Zatoshis::from_u64(if shielding { sent } else { recv }).unwrap(),
                },
            ],
            memo_count: if memo.len() != 0 { 1 } else { 0 },
            memo: memo_as_bytes,
            is_coinbase: false,
            status: if is_outside_bc {
                TxStatus::SoftFail(BlockHeight(mined_h))
            } else {
                TxStatus::OnBc
            }, // may need to change...
            staking_action: None,
        }
    }

    pub fn totals(&self, include_staking: bool) -> WalletTxPart {
        let res = self.parts[0].unchecked_add(&self.parts[1]);
        if include_staking {
            let b = WalletTxPart::from_staking_action(self.staking_action);
            res.unchecked_add(&b)
        } else {
            res
        }
    }

    pub fn account_value_delta(&self, include_staking: bool) -> ZatBalance {
        let all = self.totals(include_staking);
        // NOTE: into_i64 isn't pub...
        ZatBalance::from_i64(all.recv_zats.into_u64() as i64 - all.spent_zats.into_u64() as i64)
            .expect("checked before")
    }

    pub fn fee(&self) -> Option<Zatoshis> {
        let all = self.totals(true);
        // NOTE: into_i64 isn't pub...
        let spent = all.spent_zats.into_u64();
        let sent = all.sent_zats.into_u64();
        if spent >= sent {
            Some(Zatoshis::from_u64(spent - sent).ok()?)
        } else {
            None
        }
    }

    // TODO:
    // pub fn expired_unmined() -> bool {}

    // TODO: split shielding/unshielding/shielded/transparent/mixed from
    // send/recv/self-send/coinbase
    // TODO: staking
    pub fn kind(&self) -> WalletTxKind {
        if let Some(staking_action) = &self.staking_action {
            if staking_action.kind == StakingActionKind::CreateNewDelegationBond {
                return WalletTxKind::Stake;
            }
            if staking_action.kind == StakingActionKind::BeginDelegationUnbonding {
                return WalletTxKind::BeginUnstake;
            }
            if staking_action.kind == StakingActionKind::RetargetDelegationBond {
                return WalletTxKind::Retarget;
            }
            if staking_action.kind == StakingActionKind::WithdrawDelegationBond {
                return WalletTxKind::ClaimUnstake;
            }
            return WalletTxKind::SelfSend;
        }
        let all = self.totals(true);

        if self.is_coinbase && all.spent_zats == Zatoshis::ZERO && all.recv_zats > Zatoshis::ZERO {
            return WalletTxKind::Mine;
        }

        // if *all* of the sent zats go to ourself we assume this was the purpose
        // otherwise we assume the self-sent zats are change
        // ALT: only consider it change if a single note is received (per pool?)
        let is_self_send = all.sent_zats == all.recv_zats;
        if is_self_send {
            let all_spent_is_t = self.parts[0].spent_zats == all.spent_zats;
            let all_recv_is_shield = self.parts[1].recv_zats == all.recv_zats;
            if all_spent_is_t && all_recv_is_shield && all.recv_zats > Zatoshis::ZERO {
                WalletTxKind::Shield
            } else {
                WalletTxKind::SelfSend
            }
        } else if all.spent_zats > Zatoshis::ZERO {
            WalletTxKind::Send
        } else {
            WalletTxKind::Receive
        }
    }

    //     pub fn loc(&self, finalized_h: BlockHeight, bc_tip_h: BlockHeight) -> (WalletTxLoc, u32, bool/*finalized*/, bool/*outside_bc*/) {
    //         match self.h {
    //             BlockHeight::MEMPOOL  => (WalletTxLoc::Mempool,  falsself.is_outside_bc),
    //             BlockHeight::INTERNAL => (WalletTxLoc::Internal, falsself.is_outside_bc),
    //             _ => {
    //                 if self.is_outside_bc {
    //                     (WalletTxLoc::Block(0), self.is_outside_bc)
    //                 } else if self.h > bc_tip_h {
    //                     println!("ERROR: mined h on best chain ({}) higher than tip ({})", self.h, bc_tip_h);
    //                     return (WalletTxLoc::Block(0), true);
    //                 } else if self.h <= finalized_h {
    //                     (WalletTxLoc::Finalized, self.is_outside_bc)
    //                 } else {
    //                     (WalletTxLoc::Block(bc_tip_h - self.h), self.is_outside_bc)
    //                 }
    //             }
    //         }
    //     }
}

// @note(judah): needed so the visualizer doesn't take a dependency on zcash_primitives
#[derive(Default, Debug, Clone)]
pub struct WalletRosterMember {
    pub pub_key: [u8; 32],
    pub voting_power: u64,
    pub txids: std::vec::Vec<StakeTxId>,
}

#[derive(Default, Debug, Clone)]
pub struct WalletState {
    pub miner_seen_h: u32,
    pub miner_unshielded_funds: u64,
    pub miner_shielded_pending_funds: u64,
    pub miner_shielded_spendable_funds: u64,
    // pub faucet_funds_available: u64,
    pub user_unshielded_funds: u64,
    pub user_shielded_pending_funds: u64,
    pub user_shielded_spendable_funds: u64,

    pub staked_balance: u64,       // in zats
    pub withdrawable_balance: u64, // in zats

    pub user_local_txs_n: usize,
    pub user_local_txs: [WalletTx; 3],
    pub user_txs: Vec<WalletTx>,
    pub miner_local_txs_n: usize,
    pub miner_local_txs: [WalletTx; 3],
    pub miner_txs: Vec<WalletTx>,
    pub roster: Vec<WalletRosterMember>,

    pub waiting_for_faucet: bool,
    pub waiting_for_stake_to_finalizer: bool,
    pub waiting_for_send: bool,

    pub wallets_sync_h: u64,
    pub wallets_tip_h: u64,

    pub user_recv_ua: String,

    pub actions_in_flight: VecDeque<WalletAction>,

    pub stake_positions_bonded: Vec<(
        [u8; 32], /* bond key */
        [u8; 32], /* target finalizer */
        u64,      /* initial */
    )>,
    pub stake_positions_unbonded: Vec<(
        [u8; 32], /* bond key */
        [u8; 32], /* target finalizer */
        u64,      /* initial */
    )>,
}

impl WalletState {
    pub fn new() -> Self {
        WalletState {
            ..Default::default()
        }
    }

    pub fn user_balance(&self) -> u64 {
        self.user_unshielded_funds
            + self.user_shielded_spendable_funds
            + self.user_shielded_pending_funds
    }
    pub fn user_pending_balance(&self) -> u64 {
        self.user_shielded_pending_funds
    }
    pub fn miner_balance(&self) -> u64 {
        self.miner_unshielded_funds
            + self.miner_shielded_spendable_funds
            + self.miner_shielded_pending_funds
    }
    pub fn miner_pending_balance(&self) -> u64 {
        self.miner_shielded_pending_funds
    }

    pub fn request_from_faucet(&mut self) {
        self.waiting_for_faucet = true;

        if self
            .actions_in_flight
            .iter()
            .filter(|a| match a {
                WalletAction::RequestFromFaucet => true,
                _ => false,
            })
            .count()
            != 0
        {
            return;
        }

        self.actions_in_flight
            .push_back(WalletAction::RequestFromFaucet);
    }

    pub fn stake_to_finalizer(&mut self, amount: u64, target_finalizer: [u8; 32]) {
        if self
            .actions_in_flight
            .iter()
            .filter(|a| match a {
                WalletAction::StakeToFinalizer(_, _) => true,
                _ => false,
            })
            .count()
            != 0
        {
            return;
        }

        self.waiting_for_stake_to_finalizer = true;
        self.actions_in_flight
            .push_back(WalletAction::StakeToFinalizer(
                Zatoshis::from_u64(amount).expect("Invalid amount given to stake_to_finalizer"),
                target_finalizer,
            ));
    }

    pub fn unstake_from_finalizer(&mut self, txid: [u8; 32]) {
        let txid = TxId::from_bytes(txid);
        if self
            .actions_in_flight
            .iter()
            .filter(|a| match a {
                WalletAction::UnstakeFromFinalizer(id) if id.eq(&txid) => true,
                _ => false,
            })
            .count()
            != 0
        {
            return;
        }
        self.actions_in_flight
            .push_back(WalletAction::UnstakeFromFinalizer(txid));
    }

    pub fn retarget_bond(&mut self, txid: [u8; 32], new_target: [u8; 32]) {
        let txid = TxId::from_bytes(txid);
        if self
            .actions_in_flight
            .iter()
            .filter(|a| match a {
                WalletAction::RetargetBond(id, _to) if id.eq(&txid) => true,
                _ => false,
            })
            .count()
            != 0
        {
            return;
        }
        self.actions_in_flight
            .push_back(WalletAction::RetargetBond(txid, new_target));
    }

    pub fn claim_bond(&mut self, txid: [u8; 32]) {
        let txid = TxId::from_bytes(txid);
        if self
            .actions_in_flight
            .iter()
            .filter(|a| match a {
                WalletAction::ClaimBond(id) if id.eq(&txid) => true,
                _ => false,
            })
            .count()
            != 0
        {
            return;
        }
        self.actions_in_flight
            .push_back(WalletAction::ClaimBond(txid));
    }

    pub fn send_to_address(&mut self, address: String, amount: u64) {
        let Ok(address) = UnifiedAddress::decode(&TEST_NETWORK /* @todo */, &address) else {
            println!("Invalid address for send: {}", address);
            return;
        };

        if self
            .actions_in_flight
            .iter()
            .filter(|a| match a {
                WalletAction::SendToAddress(addr, amt)
                    if amt.into_u64() == amount && addr.eq(&address) =>
                {
                    true
                }
                _ => false,
            })
            .count()
            != 0
        {
            return;
        }

        self.waiting_for_send = true;
        self.actions_in_flight
            .push_back(WalletAction::SendToAddress(
                address,
                Zatoshis::from_u64(amount).expect("Invalid amount given to stake_to_finalizer"),
            ));
    }
}

pub fn str_from_ctaz(val: u64) -> String {
    let full = val / 100_000_000;
    let part = val % 100_000_000;
    let part_str = format!("{:08}", part);
    let trim_part = part_str.trim_end_matches("0");
    format!("{full}.{}", &part_str[..trim_part.len().max(3)])
}

enum TxOutput {
    // TODO: bool "allow fee deduction from here"
    Transparent {
        dst: TransparentAddress,
        zats: Zatoshis,
    },
    // TODO: sprout, sapling?
    Orchard {
        ovk: Option<orchard::keys::OutgoingViewingKey>,
        dst: orchard::Address,
        zats: Zatoshis,
        memo: MemoBytes,
    },
}

struct TxOptions<'a> {
    src_pools: &'a [TxPool<'a>], // in descending preference order
    staking_action: Option<StakingAction>,
}
impl<'a> TxOptions<'a> {
    // TODO: reconsider
    // pub const DEFAULT_SRC_POOLS: [TxPool; 2] = [TxPool::Sapling, TxPool::Orchard];
    // pub const DEFAULT_SRC_POOLS: [TxPool; 1] = [TxPool::Orchard];
}
impl<'a> Default for TxOptions<'a> {
    fn default() -> Self {
        Self {
            // TODO: does a default anchor make sense for shielded pools?
            src_pools: &[], //TxOptions::DEFAULT_SRC_POOLS,
            staking_action: None,
        }
    }
}

pub static wallet_main_zaino_port: Mutex<u16> = Mutex::new(0);

pub enum TxPool<'a> {
    Transparent,
    // Sprout,
    // Sapling,
    // Orchard(orchard::Anchor),
    Orchard(&'a OrchardShardTree),
}

fn transparent_keys_from_usk(
    usk: &UnifiedSpendingKey,
    index: u32,
) -> Option<(secp256k1::PublicKey, secp256k1::SecretKey)> {
    let transparent = usk.transparent();
    let account_pubkey = transparent.to_account_pubkey();
    let child_index = NonHardenedChildIndex::const_from_index(index);
    let address_pubkey = account_pubkey
        .derive_address_pubkey(TransparentKeyScope::EXTERNAL, child_index)
        .ok()?;
    let address_privkey = transparent.derive_external_secret_key(child_index).ok()?;
    Some((address_pubkey, address_privkey))
}

fn addrs_from_account(
    account: &ManualAccount,
    index: u32,
) -> Option<(TransparentAddress, UnifiedAddress)> {
    // NOTE: the wallet auto-increments the child index so this isn't recognized
    let ufvk = &account.ufvk;
    let (ua, di_) = ufvk
        .find_address(
            orchard::keys::DiversifierIndex::new(),
            UnifiedAddressRequest::ORCHARD,
        )
        .ok()?;
    let account_pubkey = ufvk.transparent()?;
    let child_index = NonHardenedChildIndex::const_from_index(index);
    let address_pubkey = account_pubkey
        .derive_address_pubkey(TransparentKeyScope::EXTERNAL, child_index)
        .ok()?;
    Some((TransparentAddress::from_pubkey(&address_pubkey), ua))
    // Some(account.default_address().ok()??.0)
}

fn update_insert_i(txs: &[WalletTx], insert_i: &mut usize, block_h: BlockHeight) {
    // put at the *end* of txs at the same height
    // i.e. primarily sorted by mined height, secondarily by discovered_time
    *insert_i += txs[*insert_i..].partition_point(|tx| tx.h <= block_h);
}

fn update_with_tx(wallet: &mut ManualWallet, mut new_tx: WalletTx, insert_i: &mut usize) {
    if AUDIT_TXS {
        wallet.audit_tx(&new_tx);
    } // pre-edit

    let txid = new_tx.txid;
    // find if there's an existing height/transaction for this txid
    // NOTE: we ignore staking here because we don't track if they're ours properly without
    // signatures, and we always spend for fee or receive in orchard with them
    let new_totals = new_tx.totals(false);
    if (new_totals.spent_note_count == 0
        && new_totals.recv_note_count == 0
        && new_totals.sent_note_count == 0)
    {
        // not our transaction; ignore
        return;
    }
    // TODO: more specifics on the staking action check
    // debug_assert!(new_totals.sent_zats == Zatoshis::const_from_u64(0) || new_totals.sent_zats < new_totals.spent_zats, "must spend for send");
    if !(new_totals.sent_zats == Zatoshis::const_from_u64(0)
        || new_totals.sent_zats < new_totals.spent_zats)
    {
        println!("TX ERROR: no spend seen for send {new_totals:?}")
    }

    if let Some(tx_h) = wallet.tx_h_map.get_mut(&txid) {
        if let Some(tx_i) = tx_h_position(&wallet.txs, *tx_h, &txid) {
            let old_tx = &wallet.txs[tx_i];
            if old_tx != &new_tx {
                if new_tx.h >= BlockHeight::MEMPOOL && old_tx.h.is_in_block() && old_tx.is_on_bc() {
                    // NOTE: mempool fetch is not synced to chain reading
                    println!("transient tx already in best chain; skipping");
                    return;
                }

                if DUMP_TX_RECV {
                    println!(
                        "{} wallet updated existing transaction {txid} {:?} => {:?}",
                        wallet.name, old_tx.h, new_tx.h
                    );
                }
                // println!("{} wallet updated existing transaction {txid} {old_tx:?} => {new_tx:?}", wallet.name);

                // leave the tx-parts from the components not provided here
                if (new_tx.part_flags & TxParts::TRANSPARENT) == 0 {
                    new_tx.parts[WalletTxPart::TRANSPARENT] =
                        old_tx.parts[WalletTxPart::TRANSPARENT];
                }
                if (new_tx.part_flags & TxParts::SHIELDED_RECV) == 0 {
                    new_tx.parts[WalletTxPart::SHIELDED].recv_note_count =
                        old_tx.parts[WalletTxPart::SHIELDED].recv_note_count;
                    new_tx.parts[WalletTxPart::SHIELDED].recv_zats =
                        old_tx.parts[WalletTxPart::SHIELDED].recv_zats;
                }
                if (new_tx.part_flags & TxParts::SHIELDED_SENT) == 0 {
                    new_tx.parts[WalletTxPart::SHIELDED].spent_note_count =
                        old_tx.parts[WalletTxPart::SHIELDED].spent_note_count;
                    new_tx.parts[WalletTxPart::SHIELDED].spent_zats =
                        old_tx.parts[WalletTxPart::SHIELDED].spent_zats;
                    new_tx.parts[WalletTxPart::SHIELDED].sent_note_count =
                        old_tx.parts[WalletTxPart::SHIELDED].sent_note_count;
                    new_tx.parts[WalletTxPart::SHIELDED].sent_zats =
                        old_tx.parts[WalletTxPart::SHIELDED].sent_zats;
                }
                if (new_tx.part_flags & TxParts::MEMO) == 0 {
                    new_tx.memo_count = old_tx.memo_count;
                    new_tx.memo = old_tx.memo;
                }
                if (new_tx.part_flags & TxParts::STAKING_ACTION) == 0 {
                    new_tx.staking_action = old_tx.staking_action;
                }

                // // if "shielding", we only see the incoming part in the compact blocks
                // if (components == (1 << WalletTxPart::SHIELDED) &&
                //     new_tx.parts[WalletTxPart::TRANSPARENT].spent_note_count > 0 &&
                //     new_tx.parts[WalletTxPart::SHIELDED].sent_note_count == 0)
                // {
                //     new_tx.parts[WalletTxPart::SHIELDED].sent_note_count = new_tx.parts[WalletTxPart::SHIELDED].recv_note_count;
                //     new_tx.parts[WalletTxPart::SHIELDED].sent_zats = new_tx.parts[WalletTxPart::SHIELDED].recv_zats;
                // }

                new_tx.part_flags |= old_tx.part_flags;
            }

            if tx_i < *insert_i {
                *insert_i -= 1;
            }
            wallet.txs.remove(tx_i);
        } else {
            println!("ERROR: {txid:?} not found at associated height {tx_h:?}");
        }
        *tx_h = new_tx.h;
    } else {
        wallet.tx_h_map.insert(txid, new_tx.h);
        if DUMP_TX_RECV {
            println!(
                "{} wallet inserted new transaction {txid} at {:?}",
                wallet.name, new_tx.h
            );
        }
    }

    if AUDIT_TXS {
        wallet.audit_tx(&new_tx);
    } // post-edit
    wallet.txs.insert(*insert_i, new_tx);
    *insert_i += 1;

    if AUDIT_TXS {
        wallet.audit_txs();
    }
}

fn to_zats_or_dump_err(src: &str, z: u64) -> Option<Zatoshis> {
    match Zatoshis::from_u64(z) {
        Ok(zats) => Some(zats),
        Err(err) => {
            println!("{src} error: couldn't convert {z} to Zatoshis: {err:?}");
            None
        }
    }
}

const EMPTY_MEMO_BYTES: [u8; 512] = {
    let mut bytes = [0; 512];
    bytes[0] = 0xf6;
    bytes
};
fn memo_is_empty(memo_bytes: &[u8; 512]) -> bool {
    memo_bytes[0] == 0xf6
}

#[derive(Clone, Debug)]
pub struct ManualAccount {
    // NOTE: this is per account so that you can scan historically for e.g. a new transparent address
    // without losing all the rest of your info
    // (if you add a new account with an earlier birthday, everything from then forward has to be rescanned)
    pub fully_detected_h: BlockHeight,
    pub fully_decoded_h: BlockHeight,
    pub ufvk: UnifiedFullViewingKey,
    pub birthday: BlockHeight,
    pub balance_changes: Vec<(BlockHeight, data_api::AccountBalance)>, // TODO: account for mempool

    // unspent: sorted by recv height
    // ALT: store stxos by both recv & spend height
    // ALT: hashmap txo to both heights
    // ALT: utxos & stxos just store (height, index into recv_txos) (careful with stability)
    pub recv_txos: Vec<Txo>,
    pub utxos: Vec<Txo>,
    // TODO: handle partial spends i.e. spend created locally but not seen in block
    // spent: sorted by spend height
    pub stxos: Vec<Txo>,

    pub recv_orchard_notes: Vec<OrchardNote>,
    pub unspent_orchard_notes: Vec<OrchardNote>,
    pub spent_orchard_notes: Vec<OrchardNote>,
}

#[cfg(debug_assertions)]
pub struct NoteLog(
    Option<HashMap<(TxId, &'static str), Vec<DevNoteAction>>>,
    u64,
);

#[cfg(debug_assertions)]
impl NoteLog {
    pub fn get_expected<'a>(
        &'a mut self,
        wallet_name: &'static str,
        txid: &TxId,
        action: &str,
    ) -> Option<(&'a mut Vec<DevNoteAction>, u64)> {
        if self.0.is_none() {
            self.0 = Some(HashMap::new());
        }

        let seq = self.1;
        self.1 += 1;
        if let Some(log) = self.0.as_mut().unwrap().get_mut(&(*txid, wallet_name)) {
            Some((log, seq))
        } else {
            println!("TX LOG ERROR: trying to {action} notes with no matching tx: {txid:?}");
            None
        }
    }

    pub fn get_or_new<'a>(
        &'a mut self,
        wallet_name: &'static str,
        txid: &TxId,
        action: &str,
    ) -> (&'a mut Vec<DevNoteAction>, u64) {
        if self.0.is_none() {
            self.0 = Some(HashMap::new());
        }

        let seq = self.1;
        self.1 += 1;
        let log = self
            .0
            .as_mut()
            .unwrap()
            .entry((*txid, wallet_name))
            .or_insert(Vec::new());
        (log, seq)
    }
}

#[cfg(debug_assertions)]
static NOTE_LOG: Mutex<NoteLog> = Mutex::new(NoteLog(None, 0));

// NOTE: WalletDb doesn't store spending key, so we'll do the same here...
#[derive(Clone, Debug)]
pub struct ManualWallet {
    pub name: &'static str,
    pub accounts: Vec<ManualAccount>,
    pub chain_tip_h: BlockHeight,
    // TODO: change type
    // TODO: to avoid nested variably-sized data, we could split these into actions that are
    // txid-linked, then reconstruct on request
    /// sorted by (h, discovery_time)
    pub txs: Vec<WalletTx>,
    pub tx_h_map: HashMap<TxId, BlockHeight>, // NOTE: not a direct index because txs get inserted
    // data_api has max_scanned in case they're scanned out of order
    // pub next_sapling_subtree_index: u64,
    // pub next_orchard_subtree_index: u64,

    // TODO: have a finalized balance etc and everything above that be a single volatile system

    // TODO: tiered tx definitiveness:
    // - local-only
    // - sent to lightwalletd
    // - seen in mempool
    // - any best-chain block
    // - best-chain block confirmed by N
    // - finalized block
    pub seen_bond_values: HashMap<[u8; 32], u64>,
    pub care_about_bonds: Vec<[u8; 32]>,
}
// N.B. using some of the same API as WalletDb to allow smooth transition/comparison
impl ManualWallet {
    pub fn chain_height(&self) -> BlockHeight {
        self.chain_tip_h
    }
    pub fn fully_detected_height(&self) -> BlockHeight {
        let mut h = BlockHeight(0);
        for account in &self.accounts {
            h = h.min(account.fully_detected_h);
        }
        h
    }

    pub fn fully_decoded_height(&self) -> BlockHeight {
        let mut h = BlockHeight(0);
        for account in &self.accounts {
            h = h.min(account.fully_decoded_h);
        }
        h
    }

    // TODO: confirmations_policy should be overridden by finalized height
    pub fn get_wallet_summary(
        &self,
        confirmations_policy: ConfirmationsPolicy,
    ) -> Result<Option<data_api::WalletSummary<usize>>, Infallible> {
        let mut account_balances = HashMap::with_capacity(self.accounts.len());
        for account_i in 0..self.accounts.len() {
            account_balances.insert(
                account_i,
                self.accounts[account_i].balance_changes.last().unwrap().1,
            );
        }

        Ok(Some(data_api::WalletSummary::new(
            account_balances,
            LRZBlockHeight::from_u32(self.chain_tip_h.0),
            LRZBlockHeight::from_u32(self.fully_decoded_height().0), // TODO: fully_detected_height?
            // ignored:
            data_api::Progress::new(data_api::Ratio::new(0, 0), None),
            0, // sapling subtree
            0, // orchard subtree
        )))
    }

    // TODO: always return full tx if created
    async fn send_built_tx<P: Parameters>(
        &mut self,
        network: P,
        client: &mut CompactTxStreamerClient<Channel>,
        wallet_tx: &mut WalletTx,
        tx: &Transaction,
    ) -> bool {
        let tz = Timer::scope_("send_built_tx", DUMP_TX_SEND);

        //-- EXPENSIVE NETWORK SEND
        // TODO: don't block, maybe return a future?
        let mut raw_tx = RawTransaction {
            data: Vec::new(),
            height: 0,
        };
        if let Err(err) = tx.write(&mut raw_tx.data) {
            println!("couldn't serialize transaction for network send: {err:?}");
            let err_buf = ErrBuf::from_str(&format!("couldn't serialize: {err:?}"));
            wallet_tx.status = TxStatus::HardFail(wallet_tx.h, err_buf); // i.e. built but not sent
            wallet_tx.h = self.chain_tip_h; // TODO: this should maybe be "sync'd height"
        } else {
            let res = client.send_transaction(raw_tx).await;
            if DUMP_TX_SEND {
                println!("******* res for {:?}: {:?}", tx.txid(), res);
            }
            // TODO: distinguish sends that weren't network issues
            if res.is_ok() {
                wallet_tx.h = BlockHeight::SENT;
            } else {
                wallet_tx.status = TxStatus::SoftFail(wallet_tx.h); // i.e. built but not sent
                wallet_tx.h = self.chain_tip_h; // TODO: this should maybe be "sync'd height"
            };
        }

        wallet_tx.is_on_bc()
    }

    fn build_tx_from_prep<P: Parameters>(
        &mut self,
        network: P,
        tx: &mut ProposedTx,
        prep: BuildPrep,
    ) -> bool {
        let tz = Timer::scope_("build_tx_from_prep", DUMP_TX_SEND | DUMP_TX_BUILD);
        let prep_fee = prep.fee_required();

        let BuildPrep {
            block_h,
            build_config,
            t_keys,
            s_keys,
            o_keys,
            t_inputs,
            t_outputs,
            o_inputs,
            o_outputs,
            staking_action,
        } = prep;

        let mut txb = TxBuilder::new(network, LRZBlockHeight::from_u32(block_h), build_config);

        for t_input in t_inputs {
            let (outpoint, coin) = (t_input.outpoint().clone(), t_input.coin().clone());
            if let Err(err) = match t_input.kind() {
                &TransparentInputKind::P2pkh { pubkey } => {
                    txb.add_transparent_input(pubkey, outpoint, coin)
                }
                TransparentInputKind::P2sh { redeem_script } => {
                    txb.add_transparent_p2sh_input(redeem_script.clone(), outpoint, coin)
                }
            } {
                if DUMP_TX_BUILD {
                    println!("constructing transparent input: {err:?}");
                }
                return false;
            }
        }

        for ProposedTransparentOutput { dst, zats } in t_outputs {
            if let Err(err) = txb.add_transparent_output(&dst, zats) {
                if DUMP_TX_BUILD {
                    println!("constructing transparent output: {err:?}");
                }
                return false;
            }
        }

        for ProposedOrchardSpend {
            fvk,
            note,
            witness_merkle_path,
        } in o_inputs
        {
            if let Err(err) =
                txb.add_orchard_spend::<zip317::FeeError>(fvk, note.note, witness_merkle_path)
            {
                if DUMP_TX_BUILD {
                    println!("constructing orchard spend: {err:?}");
                }
                return false;
            }
        }

        for ProposedOrchardOutput {
            ovk,
            dst,
            zats,
            memo: spend_memo,
        } in o_outputs
        {
            if let Err(err) =
                txb.add_orchard_output::<zip317::FeeError>(ovk, dst, zats.into_u64(), spend_memo)
            {
                if DUMP_TX_BUILD {
                    println!("constructing orchard output: {err:?}");
                }
                return false;
            }
        }

        if let Some(staking_action) = staking_action {
            if let Err(err) = txb.put_staking_action(staking_action) {
                if DUMP_TX_BUILD {
                    println!("constructing staking action: {err:?}");
                }
                return false;
            }
        }

        if let Ok(txb_fee) = txb.get_fee(&zip317::FeeRule::standard()) {
            if prep_fee.is_err() || &txb_fee != prep_fee.as_ref().unwrap() {
                println!(
                    "WARNING: fees calculated in 2 ways don't match: {txb_fee:?} vs {prep_fee:?}"
                );
            }
        }

        //-- VERY EXPENSIVE TX CREATION (PARTICULARLY IF SHIELDED OUTPUT)
        use rand_chacha::ChaCha20Rng;
        let prover = LocalTxProver::bundled();
        let rng = ChaCha20Rng::from_rng(OsRng).unwrap();

        match txb.build(
            &t_keys,
            &s_keys,
            &o_keys,
            rng,
            &prover,
            &prover,
            &zip317::FeeRule::standard(),
        ) {
            Ok(tx_res) => {
                tx.tx = WalletTx {
                    txid: tx_res.transaction().txid(),
                    h: BlockHeight::BUILT,
                    status: TxStatus::OnBc,
                    ..tx.tx
                };
                tx.tx_res = Some(tx_res);
                true
            }

            Err(err) => {
                println!("tx build error: {err:?}");
                false
            }
        }
    }

    // TODO: fee API (need to account for paying for fee forcing more notes, changing the fee)
    // TODO: allow one destination to have an empty value for "send the rest here" (this could be
    // change or normal recipient)
    pub fn send_zats_no_insert<P: Parameters>(
        &mut self,
        network: P,
        tx: &mut ProposedTx,
        client: &mut CompactTxStreamerClient<Channel>,
        outputs: &[TxOutput],
        src_usk: &UnifiedSpendingKey,
        opts: &TxOptions<'_>,
    ) -> Option<()> {
        let block_h = self.chain_tip_h.0 + 1;

        let account_id = 0;
        let account = &self.accounts[account_id];
        let keys = PreparedKeys::from_ufvk_all(&account.ufvk);
        let (t_addr, ua) = addrs_from_account(account, account_id.try_into().unwrap()).unwrap(); // @Hack
        let orchard_addr = ua.orchard().unwrap();

        tx.tx.account_id = account_id;

        //- CHECK SOURCES, FIND ANCHORS
        let (mut sapling_anchor, mut orchard_anchor) = (None, orchard::Anchor::empty_tree());
        let (mut has_transparent_src, mut has_orchard_src) = (false, false);
        let mut orchard_anchor_h = BlockHeight(0);

        for pool in opts.src_pools {
            match pool {
                TxPool::Transparent => {
                    if has_transparent_src {
                        println!("build error: repeated transparent source pool (can only have 1)");
                        // ALT: ignore the latter ones?
                        return None;
                    }
                    has_transparent_src = true;
                }

                TxPool::Orchard(shardtree) => {
                    if has_orchard_src {
                        println!("build error: repeated orchard source pool (can only have 1)");
                        // ALT: ignore the latter ones (maybe iff they have the same anchor)?
                        return None;
                    }
                    has_orchard_src = true;
                    // TODO: balance:
                    // - up-to-date anchor (privacy)
                    // - not too close to tip (reduced probability of loss through reorg)
                    // - spendable note heights (must be higher than all needed)
                    //   - more notes needed if fees change -> change in anchor
                    orchard_anchor_h = self.chain_tip_h.sat_sub(1);
                    orchard_anchor = match shardtree
                        .root_at_checkpoint_id(&orchard_anchor_h)
                        .expect("Infallible MemoryShardStore")
                    {
                        Some(root) => orchard::Anchor::from(root),
                        None => {
                            println!("tx build: couldn't get anchor at {orchard_anchor_h:?}");
                            return None;
                        }
                    }
                }
            }
        }

        //- KEYS/SIGNING
        let mut signing_set = TransparentSigningSet::new();
        let mut t_pubkey = None;
        if has_transparent_src {
            let Some((pubkey, privkey)) = transparent_keys_from_usk(&src_usk, 0) else {
                println!(
                    "tried to send from transparent source without available transparent keys"
                );
                return None;
            };
            signing_set.add_key(privkey);
            t_pubkey = Some(pubkey);
        }

        let mut prep = BuildPrep {
            build_config: BuildConfig::Standard {
                sapling_anchor,
                orchard_anchor: Some(orchard_anchor),
            },
            block_h,

            t_keys: signing_set,
            s_keys: vec![],
            o_keys: vec![orchard::keys::SpendAuthorizingKey::from(src_usk.orchard())],

            t_inputs: Vec::new(),
            t_outputs: Vec::new(),
            o_inputs: Vec::new(),
            o_outputs: Vec::new(),
            staking_action: None,
        };

        let mut txb = TxBuilder::new(
            network,
            LRZBlockHeight::from_u32(block_h),
            prep.build_config,
        );

        let (mut t, mut s, mut b) = (WalletTxPart::ZERO, WalletTxPart::ZERO, WalletTxPart::ZERO);

        //- OUTPUTS/SENDS
        let staking_action = opts.staking_action.unwrap_or_default();

        // NOTE: staking actions are currently in-themselves free (piggy back on orchard)
        match staking_action.kind {
            StakingActionKind::Null => (),

            StakingActionKind::CreateNewDelegationBond => {
                b.sent(
                    to_zats_or_dump_err("tx build: new bond", staking_action.amount_zats)?,
                    true,
                )?;
                b.recv(
                    to_zats_or_dump_err("tx build: new bond", staking_action.amount_zats)?,
                    true,
                )?;
                if DUMP_TX_BUILD {
                    println!(
                        "  added new staking position: {}",
                        staking_action.amount_zats
                    );
                }
            }

            StakingActionKind::BeginDelegationUnbonding => {
                // let txid = TxId::from_bytes(staking_action.arg32_0);
                // TODO: check it hasn't already been unstaked
                // let Some(bond_tx) = self.txs.iter().find(|t| t.txid == txid) else {
                //     println!("Could not find bond txid {:?} that was ready to unstake.", txid);
                //     return None;
                // };
                // no direct send - fee for transitioning between 2 pools we don't touch directly
                // TODO: (fallback to) pay for fee from bond?
                if DUMP_TX_BUILD {
                    println!("  unstaking bond: {}", staking_action.amount_zats);
                }
            }

            StakingActionKind::RetargetDelegationBond => {
                // let txid = TxId::from_bytes(staking_action.arg32_0);
                // TODO: check it hasn't already been unstaked
                // let Some(bond_tx) = self.txs.iter().find(|t| t.txid == txid) else {
                //     println!("Could not find bond txid {:?} that was ready to unstake.", txid);
                //     return None;
                // };
                // no direct send - fee for transitioning between 2 pools we don't touch directly
                // TODO: (fallback to) pay for fee from bond?
                if DUMP_TX_BUILD {
                    println!("  unstaking bond: {}", staking_action.amount_zats);
                }
            }

            StakingActionKind::WithdrawDelegationBond => {
                let bond_key = &staking_action.arg32_0; // TODO is this always true?
                                                        // TODO: check it hasn't already been unbonded
                let Some(bond_tx) = self.txs.iter().find(|t| {
                    t.staking_action
                        .filter(|s| s.kind == StakingActionKind::BeginDelegationUnbonding)
                        .map(|s| s.arg32_0)
                        .unwrap_or_default()
                        == *bond_key
                }) else {
                    println!(
                        "Could not find bond {:?} that was ready to claim.",
                        bond_key
                    );
                    return None;
                };

                // fee comes from bond itself
                // TODO: does this now have the correct amount?
                b.spent(
                    to_zats_or_dump_err("tx build: new bond", staking_action.amount_zats)?,
                    true,
                )?;
                if DUMP_TX_BUILD {
                    println!("  withdrawing bond: {}", staking_action.amount_zats);
                }
            }

            StakingActionKind::RegisterFinalizer
            | StakingActionKind::ConvertFinalizerRewardToDelegationBond
            | StakingActionKind::UpdateFinalizerKey => todo!("remaining staking actions"),
        }

        let staking_action = if staking_action.kind != StakingActionKind::Null {
            if let Err(err) = txb.put_staking_action(staking_action) {
                println!("tx build staking action error: {err:?}");
                return None;
            }
            prep.staking_action = Some(staking_action);
            tx.tx.staking_action = Some(staking_action);
            Some(staking_action)
        } else {
            None
        };

        for output in outputs {
            match output {
                &TxOutput::Transparent { dst, zats } => {
                    t.sent(zats, true)?;
                    let is_to_me = (dst == t_addr); // TODO: more comprehensive address matching
                    t.maybe_recv(is_to_me, zats, true)?;

                    if let Err(err) = txb.add_transparent_output(&dst, zats) {
                        println!("tx build error: {err:?}");
                        return None;
                    }
                    prep.t_outputs.push(ProposedTransparentOutput { dst, zats });
                    if DUMP_TX_BUILD {
                        println!("  added transparent output: {}", zats.into_u64());
                    }
                }
                &TxOutput::Orchard {
                    ref ovk,
                    dst,
                    zats,
                    memo: ref note_memo,
                } => {
                    s.sent(zats, true)?;
                    let is_to_me = (dst == *orchard_addr); // TODO: more comprehensive address matching
                    s.maybe_recv(is_to_me, zats, true)?;

                    tx.tx.memo_count += !memo_is_empty(note_memo.as_array()) as usize;
                    tx.tx.memo = *note_memo.as_array(); // TODO: handle multiple memos

                    if let Err(err) = txb.add_orchard_output::<zip317::FeeError>(
                        ovk.clone(),
                        dst.clone(),
                        zats.into_u64(),
                        note_memo.clone(),
                    ) {
                        println!("tx build error: {err:?}");
                        return None;
                    }
                    prep.o_outputs.push(ProposedOrchardOutput {
                        ovk: ovk.clone(),
                        dst: dst.clone(),
                        zats,
                        memo: note_memo.clone(),
                    });
                    if DUMP_TX_BUILD {
                        println!("  added orchard output: {}", zats.into_u64());
                    }
                }
            }
        }

        //- SPENDS
        // TODO: use fee_required with our own data directly
        fn calc_fee<P: Parameters>(txb: &TxBuilder<'_, P, ()>, prep: &BuildPrep) -> Option<u64> {
            let txb_fee = txb.get_fee(&zip317::FeeRule::standard());
            let dbg_txb_fee = txb_fee.as_ref().map(|z| z.into_u64()).unwrap_or(0);
            let prep_fee = prep.fee_required();
            let dbg_prep_fee = prep_fee.as_ref().map(|z| z.into_u64()).unwrap_or(0);
            debug_assert!(
                (prep_fee.is_err() && txb_fee.is_err())
                    || (prep_fee.as_ref().unwrap() == txb_fee.as_ref().unwrap()),
                "prep_fee {prep_fee:?}, txb_fee {txb_fee:?}"
            );

            match prep_fee {
                Ok(zats) => Some(zats.into_u64()),
                Err(err) => {
                    println!("tx build fee calc error: {err:?}");
                    None
                }
            }
        }

        let target_send = t.sent_zats.into_u64() + s.sent_zats.into_u64() + b.sent_zats.into_u64();
        let mut total_spend = b.spent_zats.into_u64(); // TODO: maybe treat as WalletTxPart (but t actions are slightly distinct from spends...)
        'src_pool: for pool in opts.src_pools {
            match pool {
                // TODO: account for notes that shouldn't be spent yet
                // - not enough confirmations
                // - used in another transaction that we've built
                TxPool::Transparent => {
                    let t_pubkey = t_pubkey.expect("checked above");
                    // "greedy strategy"
                    for utxo in &account.utxos {
                        if let Err(err) =
                            txb.add_transparent_input(t_pubkey, utxo.id.clone(), utxo.txout())
                        {
                            println!("tx build: transparent/UTXO spend failed: {err:?}");
                            continue;
                        }
                        t.spent(utxo.value, true)?;
                        if DUMP_TX_BUILD {
                            println!("  added transparent spend: {}", utxo.value.into_u64());
                        }

                        prep.t_inputs = txb.transparent_inputs().to_vec(); // ALT: do something *not* bad

                        total_spend += utxo.value.into_u64();
                        if (total_spend >= target_send + calc_fee::<P>(&txb, &prep)?) {
                            break 'src_pool;
                        }
                    }
                }

                TxPool::Orchard(tree) => {
                    if let Some(fvk) = &keys.orchard_fvk {
                        // determine the anchor height
                        // TODO: max this with tip_h - 10 or so
                        // let mut max_note_h = BlockHeight(0);
                        // let s_spend_z_check = 0;
                        // for note in &account.unspent_orchard_notes {
                        //     max_note_h = max_note_h.max(note.recv_h);
                        //     s_spend_z_check += note.note.value().inner();
                        //     if ((t_spend_z + s_spend_z_check) >= min_spend) {
                        //         break;
                        //     }
                        // }

                        let mut shuffled_notes = account.unspent_orchard_notes.clone();
                        shuffled_notes.shuffle(&mut OsRng);

                        for &note in &shuffled_notes {
                            if note.recv_h > orchard_anchor_h {
                                continue;
                            }

                            let witness = match tree
                                .witness_at_checkpoint_id(note.position, &orchard_anchor_h)
                            {
                                // NOTE: presumably can fail from too-recent note
                                Ok(Some(witness)) => witness,
                                Ok(None) => {
                                    println!("tx build: no orchard checkpoint exists at {orchard_anchor_h:?}");
                                    return None;
                                }
                                Err(err) => {
                                    println!("tx build: orchard witness error: {err:?}");
                                    return None;
                                }
                            };
                            let merkle_path = orchard::tree::MerklePath::from(witness);
                            if let Err(err) = txb.add_orchard_spend::<zip317::FeeError>(
                                fvk.clone(),
                                note.note,
                                merkle_path.clone(),
                            ) {
                                println!("tx build: orchard note spend failed: {err:?} ({note:?})");
                                continue;
                            }
                            let note_val = note.note.value().inner();

                            s.spent(
                                to_zats_or_dump_err("tx build orchard note", note_val)?,
                                true,
                            )?;
                            if DUMP_TX_BUILD {
                                println!("  added orchard spend: {}", note_val);
                            }

                            prep.o_inputs.push(ProposedOrchardSpend {
                                fvk: fvk.clone(),
                                note,
                                witness_merkle_path: merkle_path,
                            });

                            total_spend += note_val;
                            if (total_spend >= target_send + calc_fee::<P>(&txb, &prep)?) {
                                break 'src_pool;
                            }
                        }
                    }
                }
            }
        }

        let min_spend = target_send + calc_fee::<P>(&txb, &prep)?;
        let change = match total_spend.cmp(&min_spend) {
            std::cmp::Ordering::Less => {
                println!("tx build error: can't afford {min_spend}; only {total_spend} available from given sources");
                return None;
            } // can't afford
            std::cmp::Ordering::Equal => Zatoshis::const_from_u64(0),
            std::cmp::Ordering::Greater => {
                // TODO: prefer shielded output
                let change = to_zats_or_dump_err("tx build change", total_spend - min_spend)?;
                if let Some(ovk) = keys.orchard_ovk {
                    s.sent(change, true)?;
                    s.recv(change, true)?;
                    if let Err(err) = txb.add_orchard_output::<zip317::FeeError>(
                        Some(ovk.clone()),
                        orchard_addr.clone(),
                        change.into_u64(),
                        MemoBytes::empty(),
                    ) {
                        println!("tx build: failed to add change: {err:?}");
                        return None;
                    };
                    prep.o_outputs.push(ProposedOrchardOutput {
                        ovk: Some(ovk),
                        dst: orchard_addr.clone(),
                        zats: change,
                        memo: MemoBytes::empty(),
                    });
                    if DUMP_TX_BUILD {
                        println!("  added orchard change: {}", change.into_u64());
                    }
                } else {
                    t.sent(change, true)?;
                    t.recv(change, true)?;
                    if let Err(err) = txb.add_transparent_output(&t_addr, change) {
                        println!("tx build: failed to add change: {err:?}");
                        return None;
                    };
                    prep.t_outputs.push(ProposedTransparentOutput {
                        dst: t_addr,
                        zats: change,
                    });
                    if DUMP_TX_BUILD {
                        println!("  added transparent change: {}", change.into_u64());
                    }
                }
                change
            }
        };

        tx.tx = WalletTx {
            part_flags: TxParts::FULL_TX,
            parts: [t, s],
            h: BlockHeight::PROPOSED,
            status: TxStatus::OnBc,
            ..tx.tx
        };
        tx.prep = Some(prep);
        Some(())
    }

    pub fn send_zats<P: Parameters>(
        &mut self,
        network: P,
        tx: &mut ProposedTx,
        client: &mut CompactTxStreamerClient<Channel>,
        outputs: &[TxOutput],
        src_usk: &UnifiedSpendingKey,
        opts: &TxOptions<'_>,
    ) -> Option<()> {
        let res = self.send_zats_no_insert(network, tx, client, outputs, src_usk, opts);
        // let mut insert_i = 0;
        // update_insert_i(&self.txs, &mut insert_i, tx.tx.h);
        // update_with_tx(self, tx.tx, &mut insert_i);
        res
    }

    // NOTE: because we get 2 grace actions, we don't need to try and special-case getting all
    // change outputs within a single output, although that might be better for later note use...
    pub fn shield_transparent_zats<P: Parameters>(
        &mut self,
        network: P,
        tx: &mut ProposedTx,
        client: &mut CompactTxStreamerClient<Channel>,
        src_usk: &UnifiedSpendingKey,
        min_zats_to_shield: u64,
        orchard_tree: &OrchardShardTree,
        memo: MemoBytes,
    ) -> Option<()> {
        let tz = Timer::scope("shield_transparent_zats");
        let account = &self.accounts[0];
        let keys = PreparedKeys::from_ufvk_all(&account.ufvk);
        let (t_addr, ua) = addrs_from_account(account, 0).unwrap(); // @Hack
        if let (Some(ovk), Some(&dst)) = (keys.orchard_ovk, ua.orchard()) {
            let zats = to_zats_or_dump_err("shielding zats", min_zats_to_shield)?;
            let out = &[TxOutput::Orchard {
                ovk: Some(ovk),
                dst,
                zats,
                memo,
            }];
            let opts = &TxOptions {
                src_pools: &[TxPool::Transparent],
                ..TxOptions::default()
            };
            self.send_zats(network, tx, client, out, src_usk, opts)
        } else {
            None
        }
    }

    pub fn send_orchard_to_orchard_zats<P: Parameters>(
        &mut self,
        network: P,
        tx: &mut ProposedTx,
        client: &mut CompactTxStreamerClient<Channel>,
        src_usk: &UnifiedSpendingKey,
        exact_amount_to_send: u64,
        orchard_tree: &OrchardShardTree,
        dst: orchard::Address,
        memo: MemoBytes,
    ) -> Option<()> {
        let tz = Timer::scope("send_orchard_to_orchard_zats");
        let account = &self.accounts[0];
        let keys = PreparedKeys::from_ufvk_all(&account.ufvk);
        let (t_addr, ua) = addrs_from_account(account, 0).unwrap(); // @Hack
        if let Some(ovk) = keys.orchard_ovk {
            let zats = to_zats_or_dump_err("shielding zats", exact_amount_to_send)?;
            let out = &[TxOutput::Orchard {
                ovk: Some(ovk),
                dst,
                zats,
                memo,
            }];
            let opts = &TxOptions {
                src_pools: &[TxPool::Orchard(orchard_tree)],
                ..TxOptions::default()
            };
            self.send_zats(network, tx, client, out, src_usk, opts)
        } else {
            None
        }
    }

    pub fn stake_orchard_to_finalizer<P: Parameters>(
        &mut self,
        network: P,
        tx: &mut ProposedTx,
        client: &mut CompactTxStreamerClient<Channel>,
        src_usk: &UnifiedSpendingKey,
        exact_amount_to_send: u64,
        orchard_tree: &OrchardShardTree,
        target_finalizer: [u8; 32],
    ) -> Option<()> {
        let tz = Timer::scope("stake_orchard_to_finalizer");
        let account = &self.accounts[0];
        let keys = PreparedKeys::from_ufvk_all(&account.ufvk);
        if let Some(ovk) = keys.orchard_ovk {
            use rand::RngCore;
            let mut pretend_pub_key = [0u8; 32];
            rand::rngs::OsRng.fill_bytes(&mut pretend_pub_key);

            let opts = &TxOptions {
                src_pools: &[TxPool::Orchard(orchard_tree)],
                staking_action: Some(
                    StakingAction_CreateNewDelegationBond {
                        amount_zats: exact_amount_to_send,
                        unique_pubkey: pretend_pub_key,
                        challenge: [0u8; 32],
                        target_finalizer,
                        signature: [0u8; 64],
                    }
                    .to_union(),
                ),
            };
            self.send_zats(network, tx, client, &[], src_usk, opts)
        } else {
            None
        }
    }

    pub fn begin_unbonding_using_orchard<P: Parameters>(
        &mut self,
        network: P,
        tx: &mut ProposedTx,
        client: &mut CompactTxStreamerClient<Channel>,
        src_usk: &UnifiedSpendingKey,
        orchard_tree: &OrchardShardTree,
        bond_key: [u8; 32],
    ) -> Option<()> {
        let tz = Timer::scope("begin_unbonding_using_orchard");
        let account = &self.accounts[0];
        let opts = &TxOptions {
            src_pools: &[TxPool::Orchard(orchard_tree)],
            staking_action: Some(
                StakingAction_BeginDelegationUnbonding {
                    unique_pubkey: bond_key,
                    challenge: [0u8; 32],
                    signature: [0u8; 64],
                }
                .to_union(),
            ),
        };
        self.send_zats(network, tx, client, &[], src_usk, opts)
    }

    pub fn retarget_bond_using_orchard<P: Parameters>(
        &mut self,
        network: P,
        tx: &mut ProposedTx,
        client: &mut CompactTxStreamerClient<Channel>,
        src_usk: &UnifiedSpendingKey,
        orchard_tree: &OrchardShardTree,
        bond_key: [u8; 32],
        new_target: [u8; 32],
    ) -> Option<()> {
        let tz = Timer::scope("begin_unbonding_using_orchard");
        let account = &self.accounts[0];
        let opts = &TxOptions {
            src_pools: &[TxPool::Orchard(orchard_tree)],
            staking_action: Some(
                StakingAction_RetargetDelegationBond {
                    unique_pubkey: bond_key,
                    challenge: [0u8; 32],
                    signature: [0u8; 64],
                    target_finalizer: new_target,
                }
                .to_union(),
            ),
        };
        self.send_zats(network, tx, client, &[], src_usk, opts)
    }

    pub async fn claim_bond_using_orchard<P: Parameters>(
        &mut self,
        network: P,
        tx: &mut ProposedTx,
        client: &mut CompactTxStreamerClient<Channel>,
        src_usk: &UnifiedSpendingKey,
        orchard_tree: &OrchardShardTree,
        bond_key: [u8; 32],
    ) -> Option<()> {
        let tz = Timer::scope("claim_bond_using_orchard");
        let account = &self.accounts[0];
        let keys = PreparedKeys::from_ufvk_all(&account.ufvk);
        let (_t_addr, ua) = addrs_from_account(account, 0).unwrap(); // @Hack
        if let (Some(ovk), Some(&dst)) = (keys.orchard_ovk, ua.orchard()) {
            let amount_zats = match client
                .get_bond_info(zcash_client_backend::proto::service::BondInfoRequest {
                    bond_key: bond_key.to_vec(),
                })
                .await
            {
                Ok(response) => {
                    let info = response.into_inner();
                    info.amount
                }
                Err(e) => {
                    println!("Failed to get bond info: {:?}", e);
                    return None;
                }
            };
            self.seen_bond_values.insert(bond_key, amount_zats);

            let memo = MemoBytes::from_bytes("Claimed bond".as_bytes()).unwrap();
            let zats = to_zats_or_dump_err("claim bond", amount_zats)?;
            let out = &[TxOutput::Orchard {
                ovk: Some(ovk),
                dst,
                zats,
                memo,
            }];
            let opts = &TxOptions {
                src_pools: &[],
                staking_action: Some(
                    StakingAction_WithdrawDelegationBond {
                        amount_zats,
                        unique_pubkey: bond_key,
                        challenge: [0u8; 32],
                        signature: [0u8; 64],
                    }
                    .to_union(),
                ),
            };
            self.send_zats(network, tx, client, &[], src_usk, opts)
        } else {
            None
        }
    }

    pub fn audit_tx(&self, tx: &WalletTx) {
        if tx.is_coinbase || !(tx.reported_height() < BlockHeight::MEMPOOL && tx.is_on_bc()) {
            return;
        }
        // println!("auditing {}", tx.txid);

        let (mut t, mut s, mut b) = (WalletTxPart::ZERO, WalletTxPart::ZERO, WalletTxPart::ZERO);
        for account in &self.accounts {
            // for note in &account.recv_txos {
            //     if note.txid() == tx.txid {
            //         let Some(_) = t.recv(note.value, true) else {
            //             continue;
            //         };
            //     }
            // }
            // for note in &account.stxos {
            //     if note.txid() == tx.txid {
            //         let Some(_) = t.spent(note.value, true) else {
            //             continue;
            //         };
            //     }
            // }

            for note in &account.recv_orchard_notes {
                if note.txid == tx.txid {
                    let Some(_) = s.recv(note.value(), true) else {
                        continue;
                    };
                }
            }
            // for note in &account.spent_orchard_notes {
            //     if note.txid == tx.txid {
            //         let Some(_) = s.spent(note.value(), true) else {
            //             continue;
            //         };
            //     }
            // }
        }

        let checks = [t, s, b];
        if let Some(check_all) = WalletTxPart::checked_sum(&checks) {
            let part_strs = ["transparent", "shielded", "bonded"];
            let tx_parts = [
                tx.parts[0],
                tx.parts[1],
                WalletTxPart::from_staking_action(tx.staking_action),
            ];
            for i in 1..2 {
                // ignore bonded for now
                let mut ok = true;
                ok &= tx_parts[i].recv_zats == checks[i].recv_zats;
                ok &= tx_parts[i].recv_note_count == checks[i].recv_note_count;
                // ok &= tx_parts[i].spent_zats == checks[i].spent_zats;
                // ok &= tx_parts[i].spent_note_count == checks[i].spent_note_count;
                // ok &= tx_parts[i].sent_zats == checks[i].sent_zats;
                // ok &= tx_parts[i].sent_note_count == checks[i].sent_note_count;
                if !ok {
                    println!(
                        "TX SYNC ERROR in {}/{} {} part mismatches check:\n  {:?} vs\n  {:?}",
                        self.name, tx.txid, part_strs[i], tx_parts[i], checks[i]
                    );
                    #[cfg(debug_assertions)]
                    {
                        let mut g_log = NOTE_LOG.lock().unwrap();
                        if let Some((tx_log, seq)) = g_log.get_expected(self.name, &tx.txid, "read")
                        {
                            println!("Note log: {:?}", NL(&tx_log));
                        }
                    }
                }
                ok = true; // breakpoint
            }
        } else {
            println!(
                "TX SYNC ERROR in {}/{}: invalid sum for [{t:?}, {s:?}, {b:?}]",
                self.name, tx.txid
            );
        }
    }

    pub fn audit_txs(&self) {
        // let tz = Timer::scope("audit_txs");
        // NOTE: this could be significantly optimized if we want it running a lot
        for tx in &self.txs {
            self.audit_tx(tx);
        }
    }
}

struct PoWCache {
    pub hashes: Vec<[u8; 32]>,
    // pub hashes: [[u8;32]; 512],
    // pub h_o: usize, // trails tip
    /// ideally hashes.len() ahead of height_o, but not when initially syncing or after reorgs
    pub next_tip_h: u64,
}
impl PoWCache {
    pub fn new(init_h: u64, init_hash: [u8; 32]) -> Self {
        Self {
            hashes: vec![init_hash],
            // hashes: [[0;32]; 512],
            // h_o: 0,
            next_tip_h: init_h + 1,
        }
    }
    pub fn push_new_tip(&mut self, h: u64, hash: [u8; 32]) {
        println!("pushed tip at {h}: {}", LEHash(hash));
        assert!(h <= self.next_tip_h as u64);
        if h < self.next_tip_h as u64 {
            self.hashes.truncate(h as usize + 1);
            self.hashes[h as usize] = hash;
        } else {
            self.hashes.push(hash);
        }
        self.next_tip_h = h + 1;
    }
    pub fn hash_at_h(&self, h: u64) -> Option<[u8; 32]> {
        if h < self.next_tip_h {
            Some(self.hashes[h as usize])
        } else {
            None
        }
    }
    pub fn tip_hash(&self) -> [u8; 32] {
        self.hashes[self.next_tip_h as usize - 1]
    }
}
//     2                  1
// #############-------------------
//              ###################

/// PUSH NOTE/TXS IN HEIGHT ORDER
// ALT: dump non mempool
// ALT: linear search backwards
// TODO: this makes notes within a height slightly unstable within a height, could sub-sort by nf,
// then we can binary search exactly
fn orchard_recv_h_insert(notes: &mut Vec<OrchardNote>, note: OrchardNote) {
    let mut i = notes.len(); // common case append
    if let Some(last) = notes.last() {
        if last.recv_h > note.recv_h {
            i = notes.partition_point(|n| n.recv_h <= note.recv_h);
        }
    }
    debug_assert!(
        i == 0 || notes[i - 1].recv_h <= note.recv_h,
        "{} <= {}",
        notes[i - 1].recv_h,
        note.recv_h
    );
    notes.insert(i, note);
}
fn orchard_spent_h_insert(notes: &mut Vec<OrchardNote>, note: OrchardNote) {
    let mut i = notes.len(); // common case append
    if let Some(last) = notes.last() {
        if last.spent_h > note.spent_h {
            i = notes.partition_point(|n| n.spent_h <= note.spent_h);
        }
    }
    debug_assert!(
        i == 0 || notes[i - 1].spent_h <= note.spent_h,
        "{} <= {}",
        notes[i - 1].spent_h,
        note.spent_h
    );
    notes.insert(i, note);
}
fn txo_recv_h_insert(notes: &mut Vec<Txo>, note: Txo) {
    let mut i = notes.len(); // common case append
    if let Some(last) = notes.last() {
        if last.recv_h > note.recv_h {
            i = notes.partition_point(|n| n.recv_h <= note.recv_h);
        }
    }
    debug_assert!(
        i == 0 || notes[i - 1].recv_h <= note.recv_h,
        "{} <= {}",
        notes[i - 1].recv_h,
        note.recv_h
    );
    notes.insert(i, note);
}
fn txo_spent_h_insert(notes: &mut Vec<Txo>, note: Txo) {
    let mut i = notes.len(); // common case append
    if let Some(last) = notes.last() {
        if last.spent_h > note.spent_h {
            i = notes.partition_point(|n| n.spent_h <= note.spent_h);
        }
    }
    debug_assert!(
        i == 0 || notes[i - 1].spent_h <= note.spent_h,
        "{} <= {}",
        notes[i - 1].spent_h,
        note.spent_h
    );
    notes.insert(i, note);
}

/// GET NOTE/TX INDEXES WITH KNOWN HEIGHTS IN SORTED SLICES
fn orchard_recv_h_position(
    notes: &[OrchardNote],
    block_h: BlockHeight,
    nf: &orchard::note::Nullifier,
) -> Option<usize> {
    let mut i = notes.partition_point(|txo| txo.recv_h < block_h);
    while i < notes.len() && notes[i].recv_h == block_h {
        if &notes[i].nf == nf {
            return Some(i);
        }
        i += 1;
    }
    None
}
fn orchard_spent_h_position(
    notes: &[OrchardNote],
    block_h: BlockHeight,
    nf: &orchard::note::Nullifier,
) -> Option<usize> {
    let mut i = notes.partition_point(|txo| txo.spent_h < block_h);
    while i < notes.len() && notes[i].spent_h == block_h {
        if &notes[i].nf == nf {
            return Some(i);
        }
        i += 1;
    }
    None
}
fn txo_recv_h_position(notes: &[Txo], block_h: BlockHeight, utxo_id: &OutPoint) -> Option<usize> {
    let mut i = notes.partition_point(|txo| txo.recv_h < block_h);
    while i < notes.len() && notes[i].recv_h == block_h {
        if &notes[i].id == utxo_id {
            return Some(i);
        }
        i += 1;
    }
    None
}
fn txo_spent_h_position(notes: &[Txo], block_h: BlockHeight, utxo_id: &OutPoint) -> Option<usize> {
    let mut i = notes.partition_point(|txo| txo.spent_h < block_h);
    while i < notes.len() && notes[i].spent_h == block_h {
        if &notes[i].id == utxo_id {
            return Some(i);
        }
        i += 1;
    }
    None
}
fn tx_h_position(txs: &[WalletTx], block_h: BlockHeight, txid: &TxId) -> Option<usize> {
    let mut i = txs.partition_point(|tx| tx.h < block_h);
    while i < txs.len() && txs[i].h == block_h {
        if &txs[i].txid == txid {
            return Some(i);
        }
        i += 1;
    }
    None
}

// GET NOTE/TX INDEXES WITH UNKNOWN HEIGHTS IN SORTED SLICES
fn tx_position(wallet: &ManualWallet, txid: &TxId) -> Option<usize> {
    if let Some(&tx_h) = wallet.tx_h_map.get(txid) {
        tx_h_position(&wallet.txs, tx_h, txid)
    } else {
        None
    }
}

struct PreparedKeys {
    pub orchard_fvk: Option<orchard::keys::FullViewingKey>,
    pub orchard_ivk: Option<orchard::keys::PreparedIncomingViewingKey>,
    pub orchard_ovk: Option<orchard::keys::OutgoingViewingKey>,
    // TODO: transparent, sapling
}
impl PreparedKeys {
    pub fn from_ufvk_all(ufvk: &UnifiedFullViewingKey) -> Self {
        let mut keys = PreparedKeys {
            orchard_fvk: None,
            orchard_ivk: None,
            orchard_ovk: None,
        };

        if let Some(fvk) = ufvk.orchard() {
            // TODO: other scopes?
            keys.orchard_fvk = Some(fvk.clone());
            keys.orchard_ivk = Some(fvk.to_ivk(orchard::keys::Scope::External).prepare());
            keys.orchard_ovk = Some(fvk.to_ovk(orchard::keys::Scope::External));
        };

        keys
    }

    pub fn from_ufvk_ivks(ufvk: &UnifiedFullViewingKey) -> Self {
        let mut keys = PreparedKeys {
            orchard_fvk: None,
            orchard_ivk: None,
            orchard_ovk: None,
        };

        if let Some(fvk) = ufvk.orchard() {
            // TODO: other scopes?
            keys.orchard_fvk = Some(fvk.clone());
            keys.orchard_ivk = Some(fvk.to_ivk(orchard::keys::Scope::External).prepare());
        };

        keys
    }
}

// Some(None) => sidechain
// None => read error
// may return BlockHeight::MEMPOOL
fn bc_h_from_raw_tx_h(height: u64) -> Option<Option<BlockHeight>> {
    // height can be 0 for mempool, 0xff..ff for sidechain
    if height == u64::MAX {
        return Some(None);
    }
    let Ok(height) = <u32>::try_from(height) else {
        println!(
            "transparent tx's height can't be represented in 32 bits: {}",
            height
        );
        return None;
    };

    Some(Some(if height == 0 {
        BlockHeight::MEMPOOL
    } else {
        BlockHeight(height)
    }))
}

// TODO: handle memo here instead of caller?
// TODO: replace orchard_action_tree_position_by_cmx with position
fn handle_orchard_action(
    wallet: &mut ManualWallet,
    account_i: usize,
    keys: &PreparedKeys,
    position: incrementalmerkletree::Position,
    block_h: BlockHeight,
    txid: &TxId,
    spent_nf: &orchard::note::Nullifier,
    recv_note_addr: Option<(orchard::note::Note, orchard::Address)>,
    send_note_addr_memo: Option<(orchard::note::Note, orchard::Address, [u8; 512])>,
) -> Option<WalletTxPart> {
    let account = &mut wallet.accounts[account_i];

    //- HANDLE SPENT NOTES
    let mut s = WalletTxPart::ZERO;
    // TODO: map/index acceleration
    // NOTE: action.nullifier() is like prevout, it's the spent id (if a recv action)
    for (note_i, note) in account.unspent_orchard_notes.iter().enumerate() {
        if note.nf == *spent_nf {
            // this action is a spend by us with this note/nullifier: move it to spent
            let unspent_note = account.unspent_orchard_notes.remove(note_i);

            s.spent(
                to_zats_or_dump_err(
                    "read orchard action spend",
                    unspent_note.note.value().inner(),
                )?,
                true,
            )?;
            let spent_note = OrchardNote {
                spent_h: block_h,
                ..unspent_note
            };
            orchard_spent_h_insert(&mut account.spent_orchard_notes, spent_note.clone());
            // println!("{} found new spent note at {block_h:?}, tree pos={:02}: spent_nf:{:?}", wallet.name, u64::from(position), *spent_nf);

            #[cfg(debug_assertions)]
            {
                let mut g_log = NOTE_LOG.lock().unwrap();
                let (tx_log, seq) = g_log.get_or_new(wallet.name, txid, "spend");
                tx_log.push(DevNoteAction {
                    seq,
                    kind: DevNoteActionKind::Spend,
                    note: DevNote::OrchardNote(spent_note),
                    action_h: block_h,
                    tip_h: wallet.chain_tip_h,
                });
            }

            break;
        }
    }
    if s.spent_note_count == 0 {
        for note in &account.spent_orchard_notes {
            if note.nf == *spent_nf {
                s.spent(
                    to_zats_or_dump_err("read orchard action spend", note.note.value().inner())?,
                    true,
                )?;
                // println!("{} found old spent note at {block_h:?}, tree pos={:02}", wallet.name, u64::from(position));
                break;
            }
        }
    }

    //- HANDLE KNOWN-SENT
    if let Some((note, _addr, _memo)) = send_note_addr_memo {
        s.sent(
            to_zats_or_dump_err("read orchard action send", note.value().inner())?,
            true,
        )?;
    }

    //- PUSH NEW RECEIVED/UNSPENT NOTES
    if let Some((note, _recipient)) = recv_note_addr {
        s.recv(
            to_zats_or_dump_err("read orchard action receive", note.value().inner())?,
            true,
        )?;
        // if s_spend_c > 0 && s_send_c  {
        //     s_send_c += 1;
        //     s_send_z += note.value().inner();
        // }
        // NOTE: s_send_c/s_send_z equivalent handled inside update_with_tx

        let orchard_note = OrchardNote {
            recv_h: block_h,
            spent_h: BlockHeight(0),
            txid: *txid,
            note,
            position,
            // witness: OrchardWitness::from_tree(orchard_tree.clone()).expect("just appended"),
            // in note:
            // value: match Zatoshis::from_u64(note.value().inner()) {
            //     Ok(v) => v,
            //     Err(err) => {
            //         println!("couldn't convert {:?} to Zatoshis: {err:?}", note.value());
            //         continue 'tx_iter;
            //     }
            // },
            nf: note.nullifier(keys.orchard_fvk.as_ref().expect("implied by ivk presence")), // TODO: cache or recompute?
        };
        // println!("{} got new note at {block_h:?}, tree pos={:02}: {orchard_note:?}", wallet.name, u64::from(position));

        let txid_h = if let Some(&txid_h) = wallet.tx_h_map.get(txid) {
            txid_h
        } else {
            block_h
        };

        // TODO: can we just check if we've seen the tx && tx.is_on_bc()
        let have_seen = if let Some(i) =
            orchard_recv_h_position(&account.recv_orchard_notes, txid_h, &orchard_note.nf)
        {
            account.recv_orchard_notes[i].monotonically_update(orchard_note);
            true
        } else {
            #[cfg(debug_assertions)]
            {
                let mut g_log = NOTE_LOG.lock().unwrap();
                let (tx_log, seq) = g_log.get_or_new(wallet.name, txid, "receive");
                tx_log.push(DevNoteAction {
                    seq,
                    kind: DevNoteActionKind::Recv,
                    note: DevNote::OrchardNote(orchard_note.clone()),
                    action_h: block_h,
                    tip_h: wallet.chain_tip_h,
                });
            }

            orchard_recv_h_insert(&mut account.recv_orchard_notes, orchard_note.clone());
            false
        };

        if let Some(i) =
            orchard_recv_h_position(&account.unspent_orchard_notes, txid_h, &orchard_note.nf)
        {
            account.unspent_orchard_notes[i].monotonically_update(orchard_note);
        } else if !have_seen {
            orchard_recv_h_insert(&mut account.unspent_orchard_notes, orchard_note);
        }
    }

    Some(s)
}

fn read_full_tx(
    wallet: &mut ManualWallet,
    account_i: usize,
    keys: &PreparedKeys,
    block_h: BlockHeight,
    tx: &Transaction,
    insert_i: &mut usize,
    status: TxStatus,
) -> Option<()> {
    // TODO: we probably want to early-out if our existing tx data is complete
    // (after checking that this doesn't get modified)

    let mut expiry_h = Some(BlockHeight::from(tx.expiry_height()));
    if expiry_h.unwrap().0 == 0 {
        expiry_h = None;
    }

    let txid = tx.txid();
    // println!("at h: {block_h}, transparent tx {txid} contains {} orchard actions", tx.orchard_bundle().map_or(0, |b| b.actions().len()));

    // NOTE: these are only from *our* perspective
    let mut t = WalletTxPart::ZERO;
    let mut is_coinbase = false;
    let account = &mut wallet.accounts[account_i];

    // TODO: handle multiple addresses per account
    let (account_t_addr, account_ua) = addrs_from_account(account, 0)?;

    if let Some(t_bundle) = tx.transparent_bundle() {
        // TODO: t_bundle.authorization

        // println!("t_bundle: {t_bundle:?}");
        is_coinbase = t_bundle.is_coinbase();
        // HANDLE SPENT TXIDS
        if !is_coinbase {
            for input in &t_bundle.vin {
                if let Some(&prevout_txid_h) = wallet.tx_h_map.get(input.prevout.txid()) {
                    if let Some(utxo_i) =
                        txo_recv_h_position(&account.utxos, prevout_txid_h, &input.prevout)
                    {
                        let utxo = account.utxos.remove(utxo_i);
                        let stxo = Txo {
                            spent_h: block_h,
                            ..utxo
                        };
                        t.spent(stxo.value, true)?;

                        #[cfg(debug_assertions)]
                        {
                            let mut g_log = NOTE_LOG.lock().unwrap();
                            let (tx_log, seq) = g_log.get_or_new(wallet.name, &txid, "spend");
                            tx_log.push(DevNoteAction {
                                seq,
                                kind: DevNoteActionKind::Spend,
                                note: DevNote::Txo(stxo.clone()),
                                action_h: block_h,
                                tip_h: wallet.chain_tip_h,
                            });
                        }

                        // if let Some(last_stxo) = account.stxos.last() {
                        //     if last_stxo.spent_h > stxo.spent_h {
                        //         println!("ERROR: out of sequence spent UTXO: {} > {}", last_stxo.spent_h, stxo.spent_h);
                        //     }
                        //     debug_assert!(last_stxo.spent_h <= stxo.spent_h, "{} <= {}", last_stxo.spent_h, stxo.spent_h);
                        // }
                        txo_spent_h_insert(&mut account.stxos, stxo);
                    } else if let Some(txo_i) =
                        txo_recv_h_position(&account.recv_txos, prevout_txid_h, &input.prevout)
                    {
                        // NOTE: we need to use our own tracking of the TXO as otherwise we don't know the value
                        t.spent(account.recv_txos[txo_i].value, true)?;
                    } else {
                        // accounted for by moving it into stxos(?)
                    }
                } else {
                    // not spent by us in a block(?)
                }
            }
        }

        // PUSH NEW UNSPENT UTXOS
        for (out_i, txout) in t_bundle.vout.iter().enumerate() {
            let value = txout.value();
            if t.spent_note_count > 0 {
                // we spent money in this TX, so we must be responsible for the sends as well
                t.sent(value, true)?;
            }

            if let Some(t_addr) = txout.recipient_address() {
                if t_addr == account_t_addr {
                    let utxo = Txo {
                        recv_h: block_h,
                        spent_h: BlockHeight(0),
                        id: OutPoint::new(txid.into(), out_i.try_into().unwrap()),
                        value,
                        t_addr,
                    };
                    t.recv(value, true)?;

                    let txid_h = if let Some(&txid_h) = wallet.tx_h_map.get(&txid) {
                        txid_h
                    } else {
                        block_h
                    };
                    if let Some(utxo_i) = txo_recv_h_position(&account.utxos, txid_h, &utxo.id) {
                        if account.utxos[utxo_i] != utxo {
                            println!(
                                "ERROR: UTXO mismatch: {:?} vs {:?}",
                                account.utxos[utxo_i], &utxo
                            );
                        }
                    } else if txo_recv_h_position(&account.recv_txos, txid_h, &utxo.id).is_none() {
                        #[cfg(debug_assertions)]
                        {
                            let mut g_log = NOTE_LOG.lock().unwrap();
                            let (tx_log, seq) = g_log.get_or_new(wallet.name, &txid, "receive");
                            tx_log.push(DevNoteAction {
                                seq,
                                kind: DevNoteActionKind::Recv,
                                note: DevNote::Txo(utxo.clone()),
                                action_h: block_h,
                                tip_h: wallet.chain_tip_h,
                            });
                        }

                        // TODO: can we just check if we've seen the tx && tx.2 == false
                        if let Some(last_txo) = account.recv_txos.last() {
                            debug_assert!(
                                last_txo.recv_h <= utxo.recv_h,
                                "{} <= {}",
                                last_txo.recv_h,
                                utxo.recv_h
                            );
                        }
                        account.recv_txos.push(utxo.clone());

                        if let Some(last_utxo) = account.utxos.last() {
                            debug_assert!(
                                last_utxo.recv_h <= utxo.recv_h,
                                "{} <= {}",
                                last_utxo.recv_h,
                                utxo.recv_h
                            );
                        }
                        account.utxos.push(utxo);
                    }
                }
            }
        }
    }

    let mut memo_count = 0;
    let mut memo = EMPTY_MEMO_BYTES;
    let mut s = WalletTxPart::ZERO;

    if let Some(bundle) = tx.orchard_bundle() {
        for action in bundle.actions() {
            let action: &orchard::Action<_> = action; // type-check
            let domain = orchard::note_encryption::OrchardDomain::for_action(action);

            let (mut recv_note_addr, mut recv_memo) = (None, None);
            if let Some(ivk) = &keys.orchard_ivk {
                if let Some((note, addr, note_memo)) = try_note_decryption(&domain, ivk, action) {
                    (recv_note_addr, recv_memo) = (Some((note, addr)), Some(note_memo));
                }
            }

            let send_res = if let Some(ovk) = &keys.orchard_ovk {
                // TODO: do we get more useful info if we do both?
                // we use the nullifier to detect spends anyway,
                // and we've already got any memos from the above
                try_output_recovery_with_ovk(
                    &domain,
                    ovk,
                    action,
                    action.cv_net(),
                    &action.encrypted_note().out_ciphertext,
                )
            } else {
                None
            };

            let note_memo = if recv_memo.is_some() {
                recv_memo
            } else if let Some((_note, _addr, send_memo)) = send_res {
                Some(send_memo)
            } else {
                None
            };
            if let Some(note_memo) = note_memo {
                if !memo_is_empty(&note_memo) {
                    memo_count += 1;
                    memo = note_memo; // TODO: handle multiple memos
                }
            }

            let Some(part) = handle_orchard_action(
                wallet,
                account_i,
                keys,
                unknown_tree_position(),
                block_h,
                &txid,
                action.nullifier(),
                recv_note_addr,
                send_res,
            ) else {
                // error creating zats (already printed)
                // TODO: can we validly continue at all?
                continue;
            };

            s = if let Some(s) = s.checked_add(&part) {
                s
            } else {
                println!("invalid addition in orchard action");
                // TODO: can we validly continue at all?
                continue;
            };
        }
    }

    let new_tx = WalletTx {
        account_id: 0,
        txid,
        expiry_h,
        h: block_h,
        part_flags: TxParts::FULL_TX,
        parts: [t, s],
        memo_count,
        memo,
        is_coinbase,
        status,
        staking_action: tx.staking_action(),
    };

    update_insert_i(&wallet.txs, insert_i, block_h);
    update_with_tx(wallet, new_tx, insert_i);
    Some(())
}

type OrchardTree = incrementalmerkletree::frontier::CommitmentTree<
    orchard::tree::MerkleHashOrchard,
    { orchard::NOTE_COMMITMENT_TREE_DEPTH as u8 },
>;
type OrchardFrontier = incrementalmerkletree::frontier::Frontier<
    orchard::tree::MerkleHashOrchard,
    { orchard::NOTE_COMMITMENT_TREE_DEPTH as u8 },
>;
type OrchardWitness = incrementalmerkletree::witness::IncrementalWitness<
    orchard::tree::MerkleHashOrchard,
    { orchard::NOTE_COMMITMENT_TREE_DEPTH as u8 },
>;
const SHARD_HEIGHT: u8 = 16; // default => 65536 leaves per shard
type OrchardShardTree = shardtree::ShardTree<
    shardtree::store::memory::MemoryShardStore<
        orchard::tree::MerkleHashOrchard,
        // shardtree::Node<orchard::tree::MerkleHashOrchard, (), ()>,
        BlockHeight,
    >,
    { orchard::NOTE_COMMITMENT_TREE_DEPTH as u8 },
    SHARD_HEIGHT,
>;

fn shard_tree_size(tree: &OrchardShardTree) -> u64 {
    tree.max_leaf_position(None)
        .expect("Infallible Memory Store")
        .map_or(0, |pos| u64::from(pos) + 1)
}

fn shard_tree_root(tree: &OrchardShardTree) -> orchard::tree::MerkleHashOrchard {
    tree.root_at_checkpoint_depth(None)
        .expect("Infallible Memory Store")
        .unwrap()
}

const FAUCET_Q_LEN: usize = 16;
struct FaucetQ {
    pub read_o: u8,
    pub write_o: u8,
    pub data: [Option<orchard::Address>; FAUCET_Q_LEN],
}
impl FaucetQ {
    fn len(&self) -> usize {
        self.write_o.wrapping_sub(self.read_o).into()
    }
}
// TODO: atomic
static FAUCET_Q: Mutex<FaucetQ> = Mutex::new(FaucetQ {
    read_o: 0,
    write_o: 0,
    data: [None; FAUCET_Q_LEN],
});
const TEST_FAUCET: bool = false;
const FAUCET_VALUE: u64 = 500_000_000;

// CHEAT
fn user_view_of_faucet_tx(tx: &WalletTx) -> WalletTx {
    WalletTx {
        parts: [
            WalletTxPart::ZERO,
            WalletTxPart {
                recv_note_count: 1,
                recv_zats: Zatoshis::from_u64(
                    tx.parts[1]
                        .sent_zats
                        .into_u64()
                        .saturating_sub(tx.parts[1].recv_zats.into_u64()),
                )
                .unwrap_or(Zatoshis::const_from_u64(0)),
                ..WalletTxPart::ZERO
            },
        ],
        ..*tx
    }
}

/// NOTE: this *must* only be called in sequential order without gaps (including after reorg/truncate)
fn read_compact_tx(
    wallet: &mut ManualWallet,
    account_i: usize,
    keys: &PreparedKeys,
    block_h: BlockHeight,
    tx: &CompactTx,
    next_orchard_pos: &mut u64,
    insert_i: &mut usize,
    orchard_tree: &mut OrchardShardTree,
) -> (TxId, bool /*ours*/, bool /*ok*/) {
    let txid =
        TxId::from_bytes(<[u8; 32]>::try_from(&tx.hash[..]).expect("successfully converted above"));

    let mut shielded_part = WalletTxPart::ZERO;

    for orchard_action in &tx.actions {
        let action = match OrchardCompactAction::try_from(orchard_action) {
            Ok(v) => v,
            Err(err) => {
                // TODO: we can't keep position updated if we fail here
                // TODO: should we fail validation for the entire block above if we can't do this?
                println!(
                    "couldn't convert CompactOrchardAction to orchard::CompactAction: {err:?}"
                );
                continue;
            }
        };
        let domain = OrchardDomain::for_compact_action(&action);

        let note_addr: Option<(orchard::note::Note, orchard::Address)> =
            if let Some(ivk) = &keys.orchard_ivk {
                try_compact_note_decryption(&domain, ivk, &action)
            } else {
                None
            };

        let orchard_pos = incrementalmerkletree::Position::from(*next_orchard_pos);
        //- GLOBAL-VIEW UPDATES
        // TODO: we want to mark if *any* of the wallets care about this.
        let retention = incrementalmerkletree::Retention::Marked;
        // NOTE: we don't care to mark our sent(-only) actions
        // Track a frontier & insert_frontier[_nodes] if its ours & behind
        // let retention = if note_addr.is_some() {
        //     incrementalmerkletree::Retention::Marked
        // let retention = if note_addr.is_some() {
        //     incrementalmerkletree::Retention::Marked
        // } else {
        //     incrementalmerkletree::Retention::Ephemeral
        // };
        //
        // TODO: batch_insert
        // Some kind of problem with batch insert. TODO for later / Sam
        // let position = orchard_tree.max_leaf_position(None).unwrap().unwrap_or(incrementalmerkletree::Position::from(0));
        // let res = orchard_tree.batch_insert(position, append_iter).expect("Infallible Memory Store");
        // println!("****** orchard_tree.batch_insert result {:?}", res);
        if *next_orchard_pos >= shard_tree_size(&orchard_tree) {
            assert_eq!(
                *next_orchard_pos,
                shard_tree_size(&orchard_tree),
                "should be appending sequentially"
            );
            orchard_tree
                .append(
                    orchard::tree::MerkleHashOrchard::from_cmx(&action.cmx()),
                    retention,
                )
                .expect("Infallible Memory Store");
            if DUMP_TREES {
                println!(
                    "new orchard root at {:?} tree size={:02} {:?}",
                    block_h,
                    shard_tree_size(orchard_tree),
                    shard_tree_root(orchard_tree)
                );
            }
            // let position = orchard_tree.max_leaf_position(None).expect("Infallible Memory Store").expect("just appended");
        }
        *next_orchard_pos += 1;

        let Some(part) = handle_orchard_action(
            wallet,
            account_i,
            keys,
            orchard_pos,
            block_h,
            &txid,
            &action.nullifier(),
            note_addr,
            None,
        ) else {
            // error creating zats (already printed)
            // TODO: can we validly continue at all?
            continue;
        };
        shielded_part = if let Some(shielded_part) = shielded_part.checked_add(&part) {
            shielded_part
        } else {
            println!("invalid addition in orchard action");
            // TODO: can we validly continue at all?
            continue;
        };
    }

    // TODO: sapling

    // TODO: do we want to always recompute or can we assume data is constant
    // if txid is the same? (N.B. the txid is a hash of some transaction data
    // but not all)
    // Conservative approach: always recompute
    // TODO: decrypt our transactions & fill in actual data here
    // TODO: get full info with memos

    if (shielded_part.spent_note_count
        | shielded_part.sent_note_count
        | shielded_part.recv_note_count)
        != 0
    {
        let new_tx = WalletTx {
            account_id: account_i,
            txid,
            expiry_h: None, // TODO
            h: block_h,
            part_flags: TxParts::SHIELDED_RECV,
            parts: [WalletTxPart::ZERO, shielded_part],
            memo_count: 0,
            memo: EMPTY_MEMO_BYTES,
            is_coinbase: false,
            status: TxStatus::OnBc,
            staking_action: None,
        };

        update_with_tx(wallet, new_tx, insert_i);
        (txid, true, true)
    } else {
        (txid, false, true)
    }
}

pub async fn wallet_main(wallet_state: Arc<Mutex<WalletState>>) {
    fn stuff_from_seed_phrase<P: Parameters + 'static>(
        params: P,
        phrase: &str,
    ) -> (SecretVec<u8>, UnifiedSpendingKey) {
        use secrecy::ExposeSecret;

        let mnemonic = bip39::Mnemonic::parse(phrase).unwrap();
        let bip39_passphrase = ""; // optional
        let seed64 = mnemonic.to_seed(bip39_passphrase);
        let seed = SecretVec::new(seed64[..32].to_vec());
        let seed_fp = zip32::fingerprint::SeedFingerprint::from_seed(seed.expose_secret()).unwrap();
        let account_id = zip32::AccountId::try_from(0).unwrap();

        let usk = UnifiedSpendingKey::from_seed(&params, seed.expose_secret(), account_id).unwrap();
        let birthday = &AccountBirthday::from_parts(
            ChainState::empty(
                LRZBlockHeight::from_u32(0),
                zcash_primitives::block::BlockHash([0; 32]),
            ),
            None,
        );

        (seed, usk)
    }

    fn wallet_from_stuff<P: Parameters + 'static>(
        params: P,
        name: &'static str,
        seed: SecretVec<u8>,
    ) -> (ManualWallet, ManualAccount) {
        // TODO: skip this by changing API slightly
        let account_id = zip32::AccountId::try_from(0).unwrap();
        let usk = UnifiedSpendingKey::from_seed(&params, seed.expose_secret(), account_id).unwrap();

        let account = ManualAccount {
            ufvk: usk.to_unified_full_viewing_key(),
            birthday: BlockHeight(0),
            balance_changes: vec![(BlockHeight(0), data_api::AccountBalance::ZERO)],
            fully_decoded_h: BlockHeight(0),
            fully_detected_h: BlockHeight(0),
            recv_txos: Vec::new(),
            utxos: Vec::new(),
            stxos: Vec::new(),
            recv_orchard_notes: Vec::new(),
            unspent_orchard_notes: Vec::new(),
            spent_orchard_notes: Vec::new(),
        };

        let wallet = ManualWallet {
            name,
            accounts: vec![account.clone()],
            chain_tip_h: BlockHeight(0),
            txs: Vec::new(),
            tx_h_map: HashMap::new(),
            seen_bond_values: HashMap::new(),
            care_about_bonds: Vec::new(),
        };

        (wallet, account)
    }

    let addrs_from_wallet =
        |wallet: &ManualWallet| -> Option<(TransparentAddress, UnifiedAddress)> {
            let Some(account) = wallet.accounts.first() else {
                return None;
            };
            addrs_from_account(account, 0)
        };

    fn get_transaction_history(wallet: &ManualWallet) -> Result<Vec<WalletTx>, Infallible> {
        Ok(wallet.txs.clone())
    }

    async fn get_received_memos_and_actions<P: zcash_protocol::consensus::Parameters>(
        client: &mut CompactTxStreamerClient<Channel>,
        wallet: &ManualWallet,
        params: P,
        history: &[WalletTx],
    ) -> Option<(
        HashMap<TxId, (Option<StakingAction>, Vec<String>)>,
        HashMap<TxId, (Option<StakingAction>, Vec<String>)>,
    )> {
        fn try_get_orchard_memos(
            tx: &TransactionData<zcash_primitives::transaction::Authorized>,
            ivk: &orchard::keys::PreparedIncomingViewingKey,
        ) -> Vec<String> {
            let mut memos = Vec::new();
            let Some(bundle) = tx.orchard_bundle() else {
                return memos;
            };

            for action in bundle.actions() {
                let domain = orchard::note_encryption::OrchardDomain::for_action(action);
                if let Some((_, _, memo)) = try_note_decryption(&domain, ivk, action) {
                    let memo = String::from_utf8_lossy(&memo[..]);
                    if memo.len() == 0 {
                        continue;
                    }

                    memos.push(memo.to_string());
                }
            }

            return memos;
        }
        fn try_get_orchard_sent_memos(
            tx: &TransactionData<zcash_primitives::transaction::Authorized>,
            ovk: &orchard::keys::OutgoingViewingKey,
        ) -> Vec<String> {
            // TODO: this is primarily for syncing txs that our wallet didn't observe sending; we
            // can optimize ones we sent directly
            let mut memos = Vec::new();
            let Some(bundle) = tx.orchard_bundle() else {
                return memos;
            };

            for action in bundle.actions() {
                let domain = orchard::note_encryption::OrchardDomain::for_action(action);
                if let Some((_, _, memo)) = try_output_recovery_with_ovk(
                    &domain,
                    ovk,
                    action,
                    action.cv_net(),
                    &action.encrypted_note().out_ciphertext,
                ) {
                    let memo = String::from_utf8_lossy(&memo[..]);
                    if memo.len() == 0 {
                        continue;
                    }

                    memos.push(memo.to_string());
                }
            }

            return memos;
        }

        let mut txid_map = HashMap::new();
        let mut out_txid_map = HashMap::new();
        let txids: Vec<TxId> = history.iter().map(|h| h.txid).collect();

        // let Ok(ids) = wallet.get_account_ids() else { return None; };
        // let accounts: Vec<zcash_client_sqlite::wallet::Account> = ids
        //     .into_iter()
        //     .map(|id| wallet.get_account(id))
        //     .filter_map(|acc| acc.ok())
        //     .filter_map(|acc| acc)
        //     .collect();
        let ufvks: Vec<UnifiedFullViewingKey> =
            wallet.accounts.iter().map(|acc| acc.ufvk.clone()).collect();

        for txid in &txids {
            let filter = TxFilter {
                hash: txid.as_ref().to_vec(),
                ..Default::default()
            };
            let Ok(rawtx) = client.get_transaction(filter).await else {
                continue;
            };
            let rawtx = rawtx.into_inner();

            let block_h = LRZBlockHeight::from_u32(rawtx.height as u32);
            let Ok(tx) = Transaction::read(&*rawtx.data, BranchId::for_height(&params, block_h))
            else {
                continue;
            };

            // TODO: compress
            // TODO: sapling
            // TODO: sprout?
            let action = tx.staking_action().clone();
            let mut memos = Vec::new();
            let txdata = &tx.into_data();
            for ufvk in &ufvks {
                let uivk = ufvk.to_unified_incoming_viewing_key();
                let possible_orchard_ivk = if let Some(orchard_ivk) = uivk.orchard() {
                    Some(orchard_ivk.prepare())
                } else {
                    None
                };
                if let Some(orchard_ivk) = possible_orchard_ivk {
                    let m: Vec<String> = try_get_orchard_memos(txdata, &orchard_ivk)
                        .iter()
                        .map(|memo| memo.clone())
                        .collect();

                    for memo in m {
                        memos.push(memo);
                    }
                }
            }
            txid_map.insert(*txid, (action.clone(), memos));

            let mut memos = Vec::new();
            for ufvk in &ufvks {
                let Some(ufvk_orchard) = ufvk.orchard() else {
                    continue;
                };
                let ovk = ufvk_orchard.to_ovk(orchard::keys::Scope::External);
                let m: Vec<String> = try_get_orchard_sent_memos(txdata, &ovk)
                    .iter()
                    .map(|memo| memo.clone())
                    .collect();

                for memo in m {
                    memos.push(memo.clone());
                }
            }
            if memos.len() > 0 {
                out_txid_map.insert(*txid, (action, memos));
            }
        }

        Some((txid_map, out_txid_map))
    }

    // let send_zats = async |client: &mut CompactTxStreamerClient<_>, dst_ua: &UnifiedAddress, src_wallet: &mut WalletDb<_, _, _, _>, src_usk: &UnifiedSpendingKey, zats: Zatoshis, params, opts: &TxOptions| -> Option<[u8;32]> {
    //     let t = Timer::scope("send_zats");

    //     // @todo(judah): handle multiple accounts?
    //     let Ok(src_ids)  = src_wallet.get_account_ids() else { return None; };
    //     let Some(src_id) = src_ids.first() else { return None; };
    //     let Ok(Some(src_account)) = src_wallet.get_account(*src_id) else { return None; };

    //     const FALLBACK_CHANGE_POOL: zcash_protocol::ShieldedProtocol = zcash_protocol::ShieldedProtocol::Orchard;

    //     match wallet::propose_standard_transfer_to_address::<_, _, Infallible>(
    //         src_wallet,
    //         params,
    //         zcash_client_backend::fees::StandardFeeRule::Zip317,
    //         src_account.id(),
    //         block_policy_10(),
    //         &zcash_client_backend::address::Address::Unified(dst_ua.clone()),
    //         zats,
    //         opts.memo.clone(),
    //         None,
    //         FALLBACK_CHANGE_POOL)
    //     {
    //         Err(err) => {
    //             println!("propose_transfer error: {err:?}");
    //             None
    //         },
    //         Ok(mut proposal) => {
    //             let mut different: Vec<_> = proposal.steps.clone().into_iter().map(|mut x| { if opts.staking_action.is_some() { x.payment_pools = BTreeMap::new(); } x }).collect();
    //             proposal.steps.head = different.remove(0);
    //             proposal.steps.tail = different;
    //             let prover = LocalTxProver::bundled();
    //             match wallet::create_proposed_transactions::<_, _, Infallible, _, Infallible, _>(
    //                 src_wallet,
    //                 params,
    //                 &prover,
    //                 &prover,
    //                 &wallet::SpendingKeys::from_unified_spending_key(src_usk.clone()),
    //                 zcash_client_backend::wallet::OvkPolicy::Sender,
    //                 &proposal,
    //                 opts.staking_action.clone(),
    //             ) {
    //                 Err(err) => {
    //                     println!("create_proposed_transactions error: {err:?}");
    //                     None
    //                 },
    //                 Ok(txids) => {
    //                     if txids.len() > 1 {
    //                         println!("Unexpectedly created {} transactions", txids.len());
    //                     }

    //                     let txid = txids[0];
    //                     let tx = match src_wallet.get_transaction(txid) {
    //                         Err(err) => {
    //                             println!("failed to get tx {txid:?} immediately after making it: {err:?}");
    //                             return None;
    //                         }
    //                         Ok(Some(tx)) => tx,
    //                         Ok(None) => {
    //                             println!("failed to get tx {txid:?} immediately after making it: (None)");
    //                             return None;
    //                         }
    //                     };

    //                     let mut data = Vec::new();
    //                     if let Err(err) = tx.write(&mut data) {
    //                         println!("Serialization error for tx {:?}: {:?}", txid, err);
    //                         return None;
    //                     }

    //                     let raw_tx = RawTransaction { data, height: 0 };
    //                     match client.send_transaction(raw_tx).await {
    //                         Ok(res)  => println!("sent transaction: {res:?}"),
    //                         Err(err) => {
    //                             return None;
    //                         }
    //                     }

    //                     println!("created transaction {txid:?}");
    //                     Some(*txid.as_ref())
    //                 }
    //             }
    //         }
    //     }
    // };

    // let send_zats_to_wallet = async |client: &mut CompactTxStreamerClient<_>, dst_wallet: &mut WalletDb<_, _, _, _>, src_wallet: &mut WalletDb<_, _, _, _>, src_usk: &UnifiedSpendingKey, zats: Zatoshis, params, opts: &TxOptions| -> Option<[u8;32]> {
    //     match addrs_from_wallet(dst_wallet) {
    //         Some((_, dst_ua)) => send_zats(client, &dst_ua, src_wallet, src_usk, zats, params, opts).await,
    //         None => None,
    //     }
    // };

    // let send_unstake_reward = async |client: &mut CompactTxStreamerClient<_>, roster: &[RosterMember], txid_map: &HashMap<TxId, (Option<StakingAction>, Vec<String>)>, txid: &TxId, src_wallet: &mut WalletDb<_, _, _, _>, src_usk: &UnifiedSpendingKey, params, thing: &mut Vec::<TxId>| -> Option<[u8;32]> {
    //     println!("SENDING UNSTAKE REWARD");

    //     let Some(staked_txid) = ('find_txid: {
    //         for mem in roster {
    //             for mem_txid in &mem.txids {
    //                 if TxId::from_bytes(mem_txid.txid) == *txid {
    //                     break 'find_txid Some(mem_txid.clone());
    //                 }
    //             }
    //         }
    //         None
    //     }) else {
    //         println!("*** Failed to find member with txid: {:?}", txid);
    //         return None;
    //     };

    //     let Some((action, memos)) = txid_map.get(&txid) else {
    //         println!("*** Failed to find miner staking transaction via txid {:?}", txid);
    //         return None;
    //     };

    //     let Some(destination_address) = memos.iter().find(|memo| memo.starts_with("utest")) else {
    //         println!("*** Failed to find destination address memo in txid {:?}", txid);
    //         return None;
    //     };

    //     let destination_address = destination_address.trim_end_matches(|c| c == '\0');
    //     let Ok(destination_ua) = UnifiedAddress::decode(params, destination_address) else {
    //         println!("*** Failed to decode destination address {:?}", destination_address);
    //         return None;
    //     };

    //     let memo_str = &format!("@UNSTAKE_RECEIVE: {}\nThanks for staking!", StakingAction::str_from_addr(staked_txid.txid));
    //     let options = TxOptions{
    //         memo: Some(zcash_protocol::memo::MemoBytes::from_bytes(memo_str.as_bytes()).unwrap()),
    //         ..Default::default()
    //     };

    //     match send_zats(client, &destination_ua, src_wallet, &src_usk, Zatoshis::from_u64(staked_txid.zats).unwrap(), params, &options).await {
    //         None => {
    //             println!("Failed to send reward to user");
    //             None
    //         }
    //         Some(_) => {
    //             println!("Successfully sent reward to user");
    //             if CHEAT_UNSTAKING {
    //                 thing.push(*txid);
    //             }
    //             Some(*txid.as_ref())
    //         }
    //     }
    // };

    let global_seed = loop {
        if let Some(global_seed) = *GLOBAL_SEED.lock().unwrap() {
            break global_seed;
        }
    };

    let network = &TEST_NETWORK;

    *FAUCET_REQUEST.lock().unwrap() = Some(FaucetRequestClosure(Arc::new(
        |ua_str: String| -> Result<u64, String> {
            // let ua = zcash_address::unified::Address::decode(&ua_str).map_err(|err|
            //     Err(format!("invalid address: \"{ua_str}\" failed: {err}"))
            // )?.1;
            let ua = match zcash_keys::address::Address::decode(network, &ua_str) {
                Some(zcash_keys::address::Address::Unified(ua)) => ua,
                Some(_) => return Err(format!("must be an orchard-containing UA")),
                None => return Err(format!("couldn't decode address")),
            };
            let Some(orchard_addr) = ua.orchard() else {
                return Err(format!("must contain an orchard receiver"));
            };

            let mut q = FAUCET_Q.lock().unwrap();
            if q.len() == q.data.len() {
                return Err(format!("faucet too busy, come back later"));
            }

            for idx in 0..q.len() {
                let i = (q.read_o as usize + idx) % q.data.len();
                if let Some(existing_addr) = &q.data[i] {
                    if orchard_addr == existing_addr {
                        return Err(format!(
                            "the last request for this address is still pending, come back later"
                        ));
                    }
                } else {
                    println!("Faucet Q error: got None result where there should be valid data");
                }
            }

            let i = q.write_o as usize % q.data.len();
            q.write_o += 1;
            q.data[i] = Some(orchard_addr.clone());

            Ok(FAUCET_VALUE)
        },
    )));

    let (
        mut miner_wallet,
        mut miner_account,
        miner_seed,
        miner_usk,
        miner_pubkey,
        miner_privkey,
        miner_t_address,
        miner_ua,
        mut miner_txid_map,
        mut miner_sent_txid_map,
    ) = {
        let (seed, miner_usk) = stuff_from_seed_phrase(network,
            "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about"
        );
        let (miner_wallet, miner_account) =
            wallet_from_stuff(network, "miner", Secret::new(seed.expose_secret().clone()));

        let (miner_t_addr, miner_ua) = addrs_from_account(&miner_account, 0).unwrap();
        let miner_t_addr_str = miner_t_addr.encode(network);
        let (miner_pubkey, miner_privkey) = transparent_keys_from_usk(&miner_usk, 0).unwrap();
        (
            miner_wallet,
            miner_account,
            seed,
            miner_usk,
            miner_pubkey,
            miner_privkey,
            miner_t_addr,
            miner_ua,
            HashMap::<TxId, (Option<StakingAction>, Vec<String>)>::new(),
            HashMap::<TxId, (Option<StakingAction>, Vec<String>)>::new(),
        )
    };

    let (
        mut user_wallet,
        mut user_account,
        user_seed,
        user_usk,
        user_pubkey,
        user_privkey,
        user_t_address,
        user_ua,
        mut user_txid_map,
    ) = {
        // roundtrip seed through mnemonic phrase
        let mnemonic =
            bip39::Mnemonic::from_entropy_in(bip39::Language::English, &global_seed).unwrap();
        let phrase = mnemonic
            .words()
            .map(|s| s.to_string())
            .collect::<Vec<String>>()
            .join(" ");

        let (seed, user_usk) = stuff_from_seed_phrase(network, &phrase);
        let (user_wallet, user_account) =
            wallet_from_stuff(network, "user", Secret::new(seed.expose_secret().clone()));
        let (user_t_addr, user_ua) = addrs_from_account(&user_account, 0).unwrap();
        let user_t_addr_str = user_t_addr.encode(network);
        let (user_pubkey, user_privkey) = transparent_keys_from_usk(&user_usk, 0).unwrap();

        // let user_t_addr1 = user_t_recs.into_iter().filter(|(addr, _)| addr == &user_t_addr).next().unwrap().0;
        // NOTE: the default isn't the same as below, but I think this is because it forces a diversifier index
        // println!("User wallet: {}/{:?}", user_t_addr_str, user_t_addr1.encode(network));

        (
            user_wallet,
            user_account,
            seed,
            user_usk,
            user_pubkey,
            user_privkey,
            user_t_addr,
            user_ua,
            HashMap::<TxId, (Option<StakingAction>, Vec<String>)>::new(),
        )
    };

    let miner_ua_str = miner_ua.encode(network);
    let user_ua_str = user_ua.encode(network);
    println!("*************************");
    println!(
        "MINER WALLET T-ADDRESS: {}",
        miner_t_address.encode(network)
    );
    println!("MINER WALLET ADDRESS:   {}", miner_ua_str);
    println!("USER WALLET T-ADDRESS:  {}", user_t_address.encode(network));
    println!("USER WALLET ADDRESS:    {}", user_ua_str);
    println!("*************************");

    wallet_state.lock().unwrap().user_recv_ua = user_ua_str.clone();

    println!("waiting for zaino to be ready...");
    wait_for_zainod().await;
    //////////////////////////////////////////////////////////////////////////////////

    // TODO: use tenderlink types & printing routines
    let mut zaino_port = 0;
    loop {
        zaino_port = *wallet_main_zaino_port.lock().unwrap();
        if zaino_port != 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }

    // @todo(judah): investigate why requests get randomly dropped in a strange way:
    // transport error, service not ready, etc.
    let mut client = loop {
        if let Ok(channel) = Channel::from_shared(format!("http://localhost:{}", zaino_port))
            .unwrap()
            .connect()
            .await
        {
            break CompactTxStreamerClient::new(channel);
        }

        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    };

    if TEST_FAUCET {
        println!(
            "faucet: {:?}",
            client
                .request_faucet_donation(FaucetRequest {
                    address: user_ua_str.clone()
                })
                .await
        );
        println!(
            "faucet: {:?}",
            client
                .request_faucet_donation(FaucetRequest {
                    address: "arosienarsoienaroisetn".to_owned()
                })
                .await
        );
        println!(
            "faucet: {:?}",
            client
                .request_faucet_donation(FaucetRequest {
                    address: user_ua_str.clone()
                })
                .await
        );
        FAUCET_Q.lock().unwrap().read_o += 1; // fake read
        println!(
            "faucet: {:?}",
            client
                .request_faucet_donation(FaucetRequest {
                    address: user_ua_str.clone()
                })
                .await
        );
    }

    // NOTE: current model is to reorg this many blocks back
    // ALT: have checkpoints every 16/32 blocks and always sync from the start of one of these
    const MAX_BLOCKS_TO_DOWNLOAD_AT_TIME: u64 = 64;
    let mut time_since_last_transparent_shielded =
        std::time::Instant::now() - std::time::Duration::from_secs(1000);

    let mut stupid_thing_because_judah_is_tired_and_wants_this_to_work_properly =
        Vec::<TxId>::new();

    let genesis_hash = loop {
        match client
            .get_block(BlockId {
                height: 0,
                hash: Vec::new(),
            })
            .await
        {
            Ok(block) => break <[u8; 32]>::try_from(&block.into_inner().hash[..]).unwrap(),
            Err(err) => {
                print!("failed to get genesis block")
            }
        }
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    };

    let mut roster: Vec<RosterMember> = Vec::new();
    let mut pow_cache = PoWCache::new(0, genesis_hash);
    // NOTE: checkpoints allow us to reset the tree after a reorg & also create spend anchors
    const CHECKPOINTS_N: usize = 100;
    let mut orchard_tree = OrchardShardTree::new(
        shardtree::store::memory::MemoryShardStore::empty(),
        CHECKPOINTS_N,
    );
    orchard_tree.checkpoint(BlockHeight(0)).unwrap();

    const MAX_TXS_TO_DOWNLOAD_AT_TIME: u64 = 64;
    // TODO: this is bad and should be replaced
    let mut in_flight_tx_requests = HashSet::<TxId>::new();
    let mut in_flight_tx_join_set = tokio::task::JoinSet::new();

    // TODO: full sync should have continously-full circular-buffer pipeline of requests for chunks
    // of increasing height that gets cleared when a reorg is found or we know we're at the tip.
    // TODO: randomly sync from a list of multiple lightwalletd servers to reduce info leakage/blind trust.
    //       difficulty: handling discrepancies between them

    let mut mempool_client = client.clone();
    // NOTE: having a channel/queue that we push into async lets us do reasonable sync event reading
    let (mempool_send, mut mempool_recv) = tokio::sync::mpsc::channel::<RawTransaction>(512);
    tokio::spawn(async move {
        'mempool_reconnect: loop {
            match mempool_client.get_mempool_stream(Empty {}).await {
                Ok(s) => {
                    let mut strm = s.into_inner();
                    loop {
                        match strm.message().await {
                            Ok(Some(tx)) => {
                                // println!("MEMPOOL: got new message");
                                if let Err(err) = mempool_send.send(tx).await {
                                    println!(
                                        "MEMPOOL ERROR: can't send message to channel: {err:?}"
                                    );
                                    break;
                                }
                            }
                            Ok(None) => {
                                // println!("MEMPOOL: no more messages (will reconnect shortly)");
                                break;
                            }
                            Err(err) => {
                                println!("MEMPOOL: failed to get message tx: {err:?} (will reconnect shortly)");
                                break; // we can still update with the blocks we got, we don't need to fully reset
                            }
                        }
                    }
                }

                Err(err) => {
                    println!("MEMPOOL stream connection error: {err:?}");
                }
            }
        }

        tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;
    });

    let mut auto_spend = (false,);

    let mut faucet_shield_cooldown_instant = Instant::now() - Duration::from_secs(1000);

    let mut proposed_faucet = ProposedTx::EMPTY;
    let mut proposed_miner_shield = ProposedTx::EMPTY;
    let mut proposed_stake = ProposedTx::EMPTY;
    let mut proposed_send = ProposedTx::EMPTY;

    let mut wallet_state_push_time = Instant::now();

    let mut just_init_new_tx = false;
    let mut resync_c = 0;
    'outer_sync: loop {
        if TEST_FAUCET {
            println!(
                "faucet: {:?}",
                client
                    .request_faucet_donation(FaucetRequest {
                        address: user_ua_str.clone()
                    })
                    .await
            );
            println!(
                "faucet: {:?}",
                client
                    .request_faucet_donation(FaucetRequest {
                        address: miner_ua_str.clone()
                    })
                    .await
            );
        }

        if !just_init_new_tx && resync_c > 0 {
            tokio::time::sleep(tokio::time::Duration::from_millis(250)).await;
        }
        resync_c += 1;
        just_init_new_tx = false;

        // NOTE: this is desynced from local_tip because we need to speculatively request blocks
        // further back than the chain divergence on reorg to find out where it occurred
        let mut req_start_h = pow_cache.next_tip_h - 1;

        // NOTE: if you're dealing with multiple wallets, you don't want to resync all blocks for each
        // of them. They can all sync from the same blocks.
        // TODO: this needs to be a bit more complicated to handle arbitrarily many transparent addresses
        let (new_blocks, miner_t_txs, mut sync_from_i, req_rng, prev_tip_chain_state): (
            Vec<CompactBlock>,
            Vec<(BlockHeight, Transaction)>,
            Option<usize>,
            (u64, u64),
            ChainState,
        ) = 'sync_find_continuation_point: loop {
            // GET THE CURRENT STATE OF THE WORLD ////////////////////
            // BATCH NETWORK REQUESTS
            let (tree_state_res, lightd_res, block_range_res, t_txs_res, req_rng, t_req_rng) = {
                use std::future::Ready;
                // NOTE: clients are cheap to clone, and this is recommended in docs:
                // REF: https://docs.rs/tonic/0.14.2/tonic/client/index.html
                let (mut client0, mut client1, mut client2) =
                    (client.clone(), client.clone(), client.clone());
                fn block_rng_from_heights(heights: (u64, u64)) -> BlockRange {
                    BlockRange {
                        start: Some(BlockId {
                            height: heights.0,
                            hash: Vec::new(),
                        }),
                        end: Some(BlockId {
                            height: heights.1,
                            hash: Vec::new(),
                        }),
                    }
                }
                let req_rng = (
                    req_start_h + 1,
                    req_start_h + MAX_BLOCKS_TO_DOWNLOAD_AT_TIME,
                );

                // ********************************************************************************
                // TODO IMPORTANT: the indexer can "succeed" without actually giving us all the txs
                // in the range we requested...
                // So we keep re-requesting the info in a trailing window...
                // LRZ/Zaino/Zebra *should* return an error when iterating through the t_txs
                // they also *shouldn't* return out-of-range responses
                // ********************************************************************************
                let t_req_rng = (
                    req_rng
                        .1
                        .saturating_sub(2 * MAX_BLOCKS_TO_DOWNLOAD_AT_TIME)
                        .max(1),
                    req_rng.1,
                );

                // println!("cache at {}, downloading blocks: {}-{}",
                //     pow_cache.next_tip_h-1, block_range.start.clone().unwrap().height, block_range.end.clone().unwrap().height);
                let (tree_state_res, lightd_res, block_range_res, t_txs_res) = tokio::join!(
                    client.get_tree_state(BlockId {
                        height: req_start_h,
                        hash: Vec::new()
                    }),
                    client0.get_lightd_info(Empty {}),
                    client1.get_block_range(block_rng_from_heights(req_rng)),
                    client2.get_taddress_txids(TransparentAddressBlockFilter {
                        address: miner_t_address.encode(network),
                        range: Some(block_rng_from_heights(t_req_rng)),
                    })
                );
                (
                    tree_state_res,
                    lightd_res,
                    block_range_res,
                    t_txs_res,
                    req_rng,
                    t_req_rng,
                )
            };

            //- ROSTER
            // TODO: batch
            match client.get_roster(Empty {}).await {
                Err(err) => println!("Get roster error: {err:?}"),
                Ok(res) => {
                    use std::io::{Cursor, Read};
                    let roster_bytes = res.into_inner().data;

                    let mut ok = roster_bytes.len() > 0;
                    let mut cur = Cursor::new(&roster_bytes);

                    let mut new_roster = Vec::new();
                    let mut num_buf = [0u8; 8];
                    'read: while cur.position() < roster_bytes.len() as u64 {
                        let mut m = RosterMember {
                            pub_key: [0; 32],
                            voting_power: 0,
                            txids: Vec::new(),
                        };
                        if let Err(err) = cur.read_exact(&mut m.pub_key) {
                            println!("******* ROSTER DESERIALIZE ERROR: {err:?}");
                            ok = false;
                            break;
                        }
                        if let Err(err) = cur.read_exact(&mut num_buf) {
                            println!("******* ROSTER DESERIALIZE ERROR: {err:?}");
                            ok = false;
                            break;
                        }
                        m.voting_power = u64::from_le_bytes(num_buf);

                        if let Err(err) = cur.read_exact(&mut num_buf) {
                            println!("******* ROSTER DESERIALIZE ERROR: {err:?}");
                            ok = false;
                            break;
                        }

                        let mut voting_power_check = 0;
                        let txids_n = u64::from_le_bytes(num_buf);
                        for _ in 0..txids_n {
                            let mut stake_txid = StakeTxId {
                                txid: [0; 32],
                                zats: 0,
                            };
                            if let Err(err) = cur.read_exact(&mut stake_txid.txid) {
                                println!("******* ROSTER DESERIALIZE ERROR: {err:?}");
                                ok = false;
                                break 'read;
                            }
                            if let Err(err) = cur.read_exact(&mut num_buf) {
                                println!("******* ROSTER DESERIALIZE ERROR: {err:?}");
                                ok = false;
                                break 'read;
                            }
                            stake_txid.zats = u64::from_le_bytes(num_buf);
                            voting_power_check += stake_txid.zats;
                            m.txids.push(stake_txid);
                        }

                        if m.voting_power != voting_power_check {
                            // TODO: use manually-found one?
                            if DUMP_ROSTER {
                                println!(
                                    "******* RECEIVED ROSTER VOTING POWER INACCURATE: {} vs {}",
                                    m.voting_power, voting_power_check
                                );
                            }
                            // ok = false;
                            // break;
                        }

                        new_roster.push(m);
                    }

                    if ok {
                        roster = new_roster;
                    }

                    let wallet_roster = roster
                        .iter()
                        .map(|member| WalletRosterMember {
                            pub_key: member.pub_key,
                            voting_power: member.voting_power,
                            txids: member.txids.clone(),
                        })
                        .collect::<Vec<WalletRosterMember>>()
                        .clone();
                    if DUMP_ROSTER {
                        println!("*********** WALLET ROSTER: {wallet_roster:?}");
                    }
                    wallet_state.lock().unwrap().roster = wallet_roster;
                }
            }
            if DUMP_ROSTER {
                println!("*********** ROSTER: {roster:?}");
            }

            // NETWORK TIP HEIGHT
            // NOTE: I think this is only needed for telling the user how sync'd we are?
            match lightd_res {
                Ok(info) => {
                    let h = info.into_inner().block_height;
                    let Ok(network_tip_h) = <u32>::try_from(h) else {
                        println!("lightd network tip height not representable in 32 bits: {h}");
                        continue 'outer_sync; // TODO: don't continue if it's not actually critical
                    };

                    // AFAICT there's no downside to updating these as frequently as possible, even if the
                    // rest of sync is lagging behind
                    for wallet in [&mut miner_wallet, &mut user_wallet] {
                        wallet.chain_tip_h = BlockHeight(network_tip_h);
                    }
                }
                Err(err) => {
                    println!("Failed to get lightd info: {err:?}");
                    continue 'outer_sync; // TODO: don't continue if it's not actually critical
                }
            }

            // PREV CHAIN STATE
            // TODO: do we need to redownload this? surely we have it locally (and catch reorgs without it)
            //       maybe we don't get enough info to compute it in CompactBlocks...
            let prev_tip_chain_state: ChainState = {
                let tree_state = match tree_state_res {
                    Ok(tree_state) => tree_state.into_inner(),
                    Err(err) => {
                        println!("Failed to get tree state: {err:?}");
                        continue 'outer_sync;
                    }
                };

                match tree_state.to_chain_state() {
                    Ok(chain_state) => chain_state,
                    Err(err) => {
                        println!("Failed to convert tree state to chain state at {req_start_h:?}: {err:?}");
                        continue 'outer_sync;
                    }
                }
            };

            // START DOWNLOADS FOR FULL TRANSACTIONS
            // TODO: fairer requests
            for (wallet_i, wallet) in [&mut miner_wallet, &mut user_wallet]
                .into_iter()
                .enumerate()
            {
                for tx in &wallet.txs {
                    // TODO: trigger whenever we see a tx we want more info on
                    if in_flight_tx_requests.len() >= MAX_TXS_TO_DOWNLOAD_AT_TIME as usize {
                        break;
                    }

                    if tx.is_on_bc() &&
                        tx.part_flags != TxParts::FULL_TX &&
                        // (tx.part_flags & TxParts::MEMO) == 0 &&
                            in_flight_tx_requests.get(&tx.txid).is_none()
                    {
                        in_flight_tx_requests.insert(tx.txid);

                        let mut client = client.clone();
                        let txid = tx.txid;
                        let filter = TxFilter {
                            hash: txid.as_ref().to_vec(),
                            ..Default::default()
                        };
                        let _abort_handle = in_flight_tx_join_set.spawn(async move {
                            let str = format!("download {txid:?}");
                            let tz = Timer::scope(&str);
                            (
                                txid,
                                wallet_i,
                                match client.get_transaction(filter).await {
                                    Err(err) => {
                                        println!("get transaction: {err:?}");
                                        None
                                    }
                                    Ok(v) => Some(v.into_inner()),
                                },
                            )
                        });
                    }
                }
            }

            // COMPACT BLOCKS - DOWNLOAD
            let mut new_blocks: Vec<CompactBlock> = Vec::new();
            match block_range_res {
                Ok(blocks) => {
                    let mut block_stream = blocks.into_inner();
                    loop {
                        match block_stream.message().await {
                            // TODO: bulk await these?
                            Ok(Some(block)) => new_blocks.push(block),
                            Ok(None) => break,
                            Err(err) => {
                                if err.code() != tonic::Code::OutOfRange {
                                    println!("Failed to get block: {err:?}");
                                }
                                break; // we can still update with the blocks we got, we don't need to fully reset
                            }
                        }
                    }
                }
                Err(err) => {
                    println!("Failed to get block range: {err:?}");
                    continue 'outer_sync;
                }
            };

            // COMPACT BLOCKS - VALIDATE & CACHE
            let mut sync_from_i = None;
            if new_blocks.len() > 0 {
                // TODO: max of this & finalised height
                let mut first_new_block_i = 0;
                {
                    let i = 0;
                    // NOTE: here we always truncate back to the first block in a range that overlaps ours
                    // ALT: loop over the range to find the discontiguity. The downside of this is that
                    // we'd need to be serially dependent on requesting the tree state at that point,
                    // and that also provides another race condition for out-of-sync data.
                    // TODO: determine whether we actually need tree/chain state (commitment trees etc) for syncing
                    // for i in 0..new_blocks.len()
                    let Ok(prev_hash) = <[u8; 32]>::try_from(&new_blocks[i].prev_hash[..]) else {
                        println!(
                            "invalid prev_hash for compact block at height {}: {}",
                            new_blocks[i].height,
                            LESlice(&new_blocks[i].prev_hash)
                        );
                        continue 'outer_sync;
                    };

                    let expected_prev_hash = pow_cache.hash_at_h(new_blocks[i].height - 1);
                    if i == 0 {
                        let mut needs_resync = false;
                        if prev_hash != prev_tip_chain_state.block_hash().0 {
                            println!("non-atomic API meant block range & chain-state are torn reads: {} vs {}", LEHash(prev_hash), LEHash(prev_tip_chain_state.block_hash().0));
                            req_start_h =
                                req_start_h.saturating_sub(MAX_BLOCKS_TO_DOWNLOAD_AT_TIME / 2);
                            needs_resync = true;
                        }
                        if Some(prev_hash) != expected_prev_hash {
                            if DUMP_SYNC {
                                println!("reorg occurred before height {}; hash mismatch {prev_hash:?} vs {expected_prev_hash:?}", new_blocks[0].height);
                            }
                            req_start_h =
                                req_start_h.saturating_sub(MAX_BLOCKS_TO_DOWNLOAD_AT_TIME);
                            needs_resync = true;
                        }
                        if needs_resync {
                            if DUMP_SYNC {
                                println!("hit discontinuity; handling reorg!");
                            }
                            continue 'sync_find_continuation_point;
                        }
                    }
                    // else {
                    //     if Some(prev_hash) != expected_prev_hash {
                    //         // desync occurred within the existing range
                    //         first_new_block_i = i-1;
                    //         break;
                    //     }
                    // }
                }

                for block_i in first_new_block_i..new_blocks.len() {
                    // TODO: more data validation?
                    let mut data_is_invalid = false;
                    let hash = if let Ok(hash) = <[u8; 32]>::try_from(&new_blocks[block_i].hash[..])
                    {
                        hash
                    } else {
                        println!(
                            "invalid hash for compact block at height {}: {}",
                            new_blocks[block_i].height,
                            LESlice(&new_blocks[block_i].hash)
                        );
                        data_is_invalid = true;
                        [0; 32]
                    };
                    if <u32>::try_from(new_blocks[block_i].height).is_err() {
                        println!(
                            "block height cannot be stored in 32 bits: {}",
                            new_blocks[block_i].height
                        );
                        data_is_invalid = true;
                    }
                    for tx in &new_blocks[block_i].vtx {
                        if <[u8; 32]>::try_from(&new_blocks[block_i].hash[..]).is_err() {
                            // TODO: are TxIds LE or BE?
                            println!(
                                "invalid hash for compact tx at height {}: {:?}",
                                new_blocks[block_i].height, tx.hash
                            );
                            data_is_invalid = true;
                            break;
                        }
                    }
                    let new_tip_h = new_blocks[block_i].height;
                    let cached_prev_hash = pow_cache.hash_at_h(new_tip_h - 1);
                    let pre_new_tip_hash =
                        <[u8; 32]>::try_from(&new_blocks[block_i].prev_hash[..]).unwrap();
                    // println!("pushing {} at {new_tip_h}, prev hash {pre_new_tip_hash:?} vs cached prev {cached_prev_hash:?}", LEHash(hash));
                    if let Some(cached_prev_hash) = cached_prev_hash {
                        if (cached_prev_hash != pre_new_tip_hash) {
                            // NOTE: there appears to be no guarantee of atomicity within range, e.g.:
                            // request       v--------v
                            // original #######################
                            // reorg    ###--------------------------
                            // switch at       |
                            // invalid res   ##--------
                            // valid 1       ##########
                            // valid 2       ----------
                            if DUMP_SYNC {
                                println!("reorg occurred in the middle of the returned blocks, caching up to the reorg, then we'll update to the other chain on the next iteration");
                            }
                            data_is_invalid = true;
                        }
                    }

                    if data_is_invalid {
                        new_blocks.truncate(block_i);
                        break;
                    }

                    pow_cache.push_new_tip(new_tip_h, hash);
                    // TODO: if this changes we need to skip to the equivalent on the transparent txs
                    sync_from_i = Some(first_new_block_i);
                }
            }

            if sync_from_i.is_none() {
                // println!("nothing to sync");
                break (
                    Vec::new(),
                    Vec::new(),
                    None,
                    req_rng,
                    ChainState::empty(
                        LRZBlockHeight::from_u32(0),
                        zcash_primitives::block::BlockHash([0; 32]),
                    ),
                );
            }
            if DUMP_SYNC {
                println!(
                    "downloaded compact blocks {}-{}",
                    new_blocks.first().unwrap().height,
                    new_blocks.last().unwrap().height
                );
            }

            let compact_block_max_h = new_blocks.last().expect("non-empty vector").height;

            // TRANSPARENT TRANSACTIONS
            let mut t_failed_at_h = None;
            let mut new_raw_t_txs: Vec<RawTransaction> = Vec::new();
            let (mut min_t_h, mut max_t_h) = (u64::MAX, 0);
            match t_txs_res {
                Ok(t_txs) => {
                    let mut tx_stream = t_txs.into_inner();
                    loop {
                        match tx_stream.message().await {
                            // TODO: bulk await these?
                            Ok(Some(tx)) => {
                                min_t_h = min_t_h.min(tx.height);
                                max_t_h = max_t_h.max(tx.height);
                                new_raw_t_txs.push(tx)
                            }
                            Ok(None) => break,
                            Err(err) => {
                                if err.code() != tonic::Code::OutOfRange {
                                    println!("failed to get transparent tx: {err:?}");
                                    // NOTE: this is overly conservative
                                    t_failed_at_h =
                                        Some(new_raw_t_txs.last().map_or(0, |tx| tx.height + 1));
                                }
                                break; // we can still update with the txs we got, we don't need to fully reset
                            }
                        }
                    }
                }
                Err(err) => {
                    println!("Failed to get block range: {err:?}");
                    continue 'outer_sync;
                }
            };
            if new_raw_t_txs.len() > 0 {
                if DUMP_SYNC {
                    println!(
                        "downloaded transparent txs at heights {}-{}",
                        min_t_h, max_t_h
                    );
                }
            }

            let mut new_t_txs =
                Vec::<(BlockHeight, Transaction)>::with_capacity(new_raw_t_txs.len());
            for tx_i in 0..new_raw_t_txs.len() {
                // ********************************************************************************
                // TODO IMPORTANT: we can get the mined block hash for txs *individually* if we
                // go through the JSON-RPC version of getrawtransaction (but none of the stream
                // wrappers for it). Otherwise we're blindly assuming these match up with the
                // previous CompactBlocks by height... This can be bad at least between syncs.
                // I'm not currently clear if this causes persistent issues, as we *should* be
                // dropping these if they're above a reorg next time we detect it via CompactBlock.
                // ********************************************************************************

                let raw_tx = &new_raw_t_txs[tx_i];

                let h = match bc_h_from_raw_tx_h(raw_tx.height) {
                    Some(None) => {
                        if DUMP_SYNC {
                            println!("found sidechain transparent tx that we don't have height for, skipping...");
                        }
                        continue;
                    }
                    Some(Some(h)) => h,
                    None => break, // read error
                };

                if !h.is_in_block() {
                    break;
                }
                if u64::from(h.0) > compact_block_max_h {
                    // @in_step_sync
                    break;
                }

                let tx = match Transaction::read(
                    &raw_tx.data[..],
                    BranchId::for_height(network, LRZBlockHeight::from_u32(h.0)),
                ) {
                    Ok(tx) => tx,
                    Err(err) => {
                        println!("failed to read transparent tx at height {h}");
                        t_failed_at_h = Some(raw_tx.height); // this will be < the previous val
                                                             // @in_step_sync
                                                             // remove everything at the failed height
                        while new_t_txs.len() > 0 {
                            if new_t_txs.last().unwrap().0 != h {
                                break;
                            }
                            new_t_txs.pop();
                        }
                        break;
                    }
                };
                new_t_txs.push((h, tx));
            }

            // truncate compact blocks to match transparent // @in_step_sync
            if let Some(h) = t_failed_at_h {
                if DUMP_SYNC {
                    println!("truncating compact blocks to match transparent at {h}");
                }
                // ALT: partition_point then truncate
                while new_blocks.len() > 0 {
                    if new_blocks.last().unwrap().height < h {
                        break;
                    }
                    new_blocks.pop();
                }

                if new_blocks.len() == 0 {
                    sync_from_i = None;
                }

                if let Some(hash) = pow_cache.hash_at_h(h) {
                    pow_cache.push_new_tip(h, hash);
                }
            }

            break (
                new_blocks,
                new_t_txs,
                sync_from_i,
                req_rng,
                prev_tip_chain_state,
            );
        };

        let wallets_sync_h = BlockHeight((pow_cache.next_tip_h - 1).try_into().unwrap());
        let network_tip_h = user_wallet.chain_tip_h;

        // let mut orchard_frontier = prev_tip_chain_state.final_orchard_tree().clone();
        // let mut orchard_tree = incrementalmerkletree::frontier::CommitmentTree::from_frontier(&orchard_frontier);
        // println!("orchard root at {:?} tree: size={} {:?}", prev_tip_chain_state.block_height(), shard_tree_size(orchard_tree), shard_tree_root(orchard_tree));

        //-- REORG
        // TODO: double check mempool invalidation sequences correctly with async read (account for tip height on downloaded tx)
        if let Some(start_block_i) = sync_from_i {
            // the regime is basically "always reorg", but that's often a no-op
            // truncate wallet for everything below height
            let sync_start_h = <u32>::try_from(new_blocks[start_block_i].height)
                .expect("successfully converted above");
            let block_h = BlockHeight(sync_start_h);
            let last_block_h = block_h.sat_sub(1);

            orchard_tree.truncate_to_checkpoint(&last_block_h); // N.B. checkpoints are at the *end* of their block

            for (wallet_i, wallet) in [&mut miner_wallet, &mut user_wallet]
                .into_iter()
                .enumerate()
            {
                //-- INVALIDATE TXS >= NEW BLOCKS HEIGHT
                for account in &mut wallet.accounts {
                    account.fully_detected_h = account.fully_detected_h.min(last_block_h);
                    account.fully_decoded_h = account.fully_decoded_h.min(last_block_h);

                    // TODO: do we want to track balance changes or keep balances updated as chain changes occur?
                    let truncate_to_i = account
                        .balance_changes
                        .partition_point(|(b, _)| *b < block_h);
                    account.balance_changes.truncate(truncate_to_i);

                    //- UNRECEIVE NOTES
                    {
                        let utxos_at_h_start =
                            account.utxos.partition_point(|txo| txo.recv_h < block_h);
                        account.utxos.truncate(utxos_at_h_start);
                        let recv_txos_at_h_start = account
                            .recv_txos
                            .partition_point(|txo| txo.recv_h < block_h);

                        #[cfg(debug_assertions)]
                        for txo in &account.recv_txos[recv_txos_at_h_start..] {
                            let mut g_log = NOTE_LOG.lock().unwrap();
                            if let Some((tx_log, seq)) =
                                g_log.get_expected(wallet.name, &txo.txid(), "unreceive")
                            {
                                tx_log.push(DevNoteAction {
                                    seq,
                                    kind: DevNoteActionKind::Unrecv,
                                    note: DevNote::Txo(txo.clone()),
                                    action_h: block_h,
                                    tip_h: network_tip_h,
                                });
                            }
                        }

                        account.recv_txos.truncate(recv_txos_at_h_start);
                    }

                    {
                        let unspent_orchard_notes_at_h_start = account
                            .unspent_orchard_notes
                            .partition_point(|txo| txo.recv_h < block_h);
                        account
                            .unspent_orchard_notes
                            .truncate(unspent_orchard_notes_at_h_start);
                        let recv_orchard_notes_at_h_start = account
                            .recv_orchard_notes
                            .partition_point(|txo| txo.recv_h < block_h);
                        #[cfg(debug_assertions)]
                        for note in &account.recv_orchard_notes[recv_orchard_notes_at_h_start..] {
                            let mut g_log = NOTE_LOG.lock().unwrap();
                            if let Some((tx_log, seq)) =
                                g_log.get_expected(wallet.name, &note.txid, "unreceive")
                            {
                                tx_log.push(DevNoteAction {
                                    seq,
                                    kind: DevNoteActionKind::Unrecv,
                                    note: DevNote::OrchardNote(note.clone()),
                                    action_h: block_h,
                                    tip_h: network_tip_h,
                                });
                            }
                        }
                        account
                            .recv_orchard_notes
                            .truncate(recv_orchard_notes_at_h_start);
                    }

                    //- UNSPEND NOTES
                    // NOTE: spent notes are in spend_h order, NOT recv_h order
                    {
                        let stxos_at_h_start =
                            account.stxos.partition_point(|txo| txo.spent_h < block_h);
                        for stxo in &account.stxos[stxos_at_h_start..] {
                            #[cfg(debug_assertions)]
                            {
                                let mut g_log = NOTE_LOG.lock().unwrap();
                                if let Some((tx_log, seq)) =
                                    g_log.get_expected(wallet.name, &stxo.txid(), "unspend")
                                {
                                    tx_log.push(DevNoteAction {
                                        seq,
                                        kind: DevNoteActionKind::Unspend,
                                        note: DevNote::Txo(stxo.clone()),
                                        action_h: block_h,
                                        tip_h: network_tip_h,
                                    });
                                }
                            }

                            if stxo.recv_h < block_h {
                                txo_recv_h_insert(
                                    &mut account.utxos,
                                    Txo {
                                        spent_h: BlockHeight(0),
                                        ..stxo.clone()
                                    },
                                );
                            }
                        }
                        account.stxos.truncate(stxos_at_h_start);
                    }

                    {
                        let spent_orchard_notes_at_h_start = account
                            .spent_orchard_notes
                            .partition_point(|note| note.spent_h < block_h);
                        for note in &account.spent_orchard_notes[spent_orchard_notes_at_h_start..] {
                            #[cfg(debug_assertions)]
                            {
                                let mut g_log = NOTE_LOG.lock().unwrap();
                                if let Some((tx_log, seq)) =
                                    g_log.get_expected(wallet.name, &note.txid, "unspend")
                                {
                                    tx_log.push(DevNoteAction {
                                        seq,
                                        kind: DevNoteActionKind::Unspend,
                                        note: DevNote::OrchardNote(note.clone()),
                                        action_h: block_h,
                                        tip_h: network_tip_h,
                                    });
                                }
                            }

                            if note.recv_h < block_h {
                                orchard_recv_h_insert(
                                    &mut account.unspent_orchard_notes,
                                    OrchardNote {
                                        spent_h: BlockHeight(0),
                                        ..note.clone()
                                    },
                                );
                            }
                        }
                        account
                            .spent_orchard_notes
                            .truncate(spent_orchard_notes_at_h_start);
                    }
                }

                //  higher blocks & mempool
                let invalidate_from_i = wallet.txs.partition_point(|tx| tx.h < block_h);
                for tx in &mut wallet.txs[invalidate_from_i..] {
                    if tx.h > BlockHeight::MEMPOOL {
                        // mid-construction items aren't auto-invalidated
                        // maybe sent should be?
                        break;
                    }
                    // N.B. these may get revalidated later if the same txs are found in the new blocks
                    tx.status = TxStatus::SoftFail(tx.h);
                    tx.h = wallet.chain_tip_h;
                    wallet.tx_h_map.remove(&tx.txid);
                    wallet.tx_h_map.insert(tx.txid, tx.h);
                }
            }
        }

        //-- ADD/REVALIDATE TRANSPARENT TXS (and attached shielded data)
        {
            // see note above on transparent syncing
            // let sync_start_h = if let Some(start_block_i) = sync_from_i {
            //     <u32>::try_from(new_blocks[start_block_i].height).expect("successfully converted above")
            // } else {
            //     req_rng.0.try_into.expect("fits in u32")
            // };
            let keys = PreparedKeys::from_ufvk_all(&miner_wallet.accounts[0].ufvk);
            let mut insert_i = 0;
            for t_tx_i in 0..miner_t_txs.len() {
                // kinda @in_step_sync
                let block_h = miner_t_txs[t_tx_i].0;
                let tx = &miner_t_txs[t_tx_i].1;
                read_full_tx(
                    &mut miner_wallet,
                    0,
                    &keys,
                    block_h,
                    tx,
                    &mut insert_i,
                    TxStatus::OnBc,
                );
            }
        }

        //-- READ DOWNLOADED MEMPOOL TXS (if we're close to sync'd)
        // if network_tip_h.0 <= wallets_sync_h.0 + 10 // TODO: we want this on the download start
        {
            // TODO: maybe wait until we're ~block-synced before doing this
            // NOTE: assumes we can keep up... maybe dropping with some feedback about that is better?
            let wallets = [&mut user_wallet, &mut miner_wallet];
            let keys = [
                PreparedKeys::from_ufvk_all(&wallets[0].accounts[0].ufvk),
                PreparedKeys::from_ufvk_all(&wallets[1].accounts[0].ufvk),
            ];
            let insert_idxs = [&mut 0, &mut 0];

            while let Ok(raw_tx) = mempool_recv.try_recv() {
                if DUMP_SYNC {
                    println!(
                        "got mempool tx with tip height: {} vs chain tip {}",
                        raw_tx.height, wallets[0].chain_tip_h.0
                    );
                }
                // NOTE: expected LRZ height different from abstract mempool height
                match Transaction::read(
                    &raw_tx.data[..],
                    BranchId::for_height(network, LRZBlockHeight::from_u32(network_tip_h.0 + 1)),
                ) {
                    Err(err) => {
                        println!("invalid mempool tx: {err:?}");
                        // NOTE: as mempool txs are not sequenced, it seems reasonable to just ignore
                        // invalid ones without skipping the rest
                    }
                    Ok(tx) => {
                        for i in 0..2 {
                            read_full_tx(
                                wallets[i],
                                0,
                                &keys[i],
                                BlockHeight::MEMPOOL,
                                &tx,
                                insert_idxs[i],
                                TxStatus::OnBc,
                            );
                        }
                    }
                }
            }
        }

        //-- ADD/REVALIDATE SHIELDED TXS FROM NEW BLOCKS -- CANONICAL "SPINE"
        if let Some(start_block_i) = sync_from_i {
            let sync_start_h = <u32>::try_from(new_blocks[start_block_i].height)
                .expect("successfully converted above");
            if DUMP_SYNC {
                println!(
                    "cache at {}, new blocks: {}-{}; updating wallets...",
                    pow_cache.next_tip_h - 1,
                    new_blocks.first().unwrap().height,
                    new_blocks.last().unwrap().height
                );
            }

            let rng_start_orchard_tree_size = shard_tree_size(&orchard_tree);

            for (wallet_i, wallet) in [&mut miner_wallet, &mut user_wallet]
                .into_iter()
                .enumerate()
            {
                let mut next_orchard_pos = rng_start_orchard_tree_size;
                let keys = PreparedKeys::from_ufvk_ivks(&wallet.accounts[0].ufvk); // NOTE: can't use ovk for CompactTx
                let mut insert_i = 0;
                for block in &new_blocks {
                    let block_h = BlockHeight(block.height.try_into().unwrap());
                    update_insert_i(&wallet.txs, &mut insert_i, block_h);

                    //-- INCORPORATE SHIELDED TRANSACTIONS FROM COMPACT BLOCK
                    'tx_iter: for tx in &block.vtx {
                        if let (txid, true, true) = read_compact_tx(
                            wallet,
                            0,
                            &keys,
                            block_h,
                            tx,
                            &mut next_orchard_pos,
                            &mut insert_i,
                            &mut orchard_tree,
                        ) {
                            // println!("found our compact tx: {txid:?}");
                        }
                    }

                    // NOTE: simple approach: checkpoint every block
                    // => allows for easy reorgs & witnesses
                    if next_orchard_pos == shard_tree_size(&orchard_tree) {
                        orchard_tree.checkpoint(block_h);
                        // println!("checkpoint: orchard root at {:?} tree: size={} {:?}", block_h, shard_tree_size(&orchard_tree), shard_tree_root(&orchard_tree));
                    }
                }
            }
        }

        //-- READ ANY DOWNLOADED FULL TXS
        if in_flight_tx_requests.len() > 0 {
            if DUMP_SYNC {
                println!(
                    "before reading, there are {} in flight tx downloads",
                    in_flight_tx_requests.len()
                );
            }
            let wallets = [&mut miner_wallet, &mut user_wallet];
            while let Some(tx_completion) = in_flight_tx_join_set.try_join_next() {
                let (txid, wallet_i, dl_result): (TxId, usize, Option<RawTransaction>) =
                    match tx_completion {
                        Ok(v) => v,
                        Err(err) => {
                            println!("tx completion join error: {err:?}");
                            continue;
                        }
                    };

                // ALT: do (most of) this work in the task
                // ALT: we have the option for best-chain/height info of:
                // - update individual transactions ASAP (-> use returned height & is_outside_bc)
                // - try to keep all blocks self-consistent (-> use *current* height/is_outside_bc)
                //   (this may be different from the requested height)
                //   * Choosing this because currently block-reading is the only way we detect
                //     inconsistency around reorg; we don't want to add stale data here that never
                //     gets fixed
                if DUMP_SYNC {
                    println!("download finished for {txid:?}");
                }
                in_flight_tx_requests.remove(&txid);
                if let (Some(raw_tx), Some(existing_tx_i)) =
                    (dl_result, tx_position(&wallets[wallet_i], &txid))
                {
                    let existing_tx = &wallets[wallet_i].txs[existing_tx_i];

                    let found_h = match bc_h_from_raw_tx_h(raw_tx.height) {
                        Some(None) => existing_tx.h, // on sidechain: use previously-spec'd height
                        Some(Some(h)) => {
                            if h != existing_tx.h {
                                println!(
                                    "requested tx {txid:?} has moved from {:?} to {h:?}",
                                    existing_tx.h
                                );
                            }
                            h
                        }
                        None => continue, // read error
                    };

                    // NOTE: there's a potential inconsistency here around branch id changes if we
                    // see it both before and after the change
                    let lrz_h = LRZBlockHeight::from_u32(if found_h.is_in_block() {
                        found_h.0
                    } else {
                        network_tip_h.0
                    });
                    let tx = match Transaction::read(
                        &raw_tx.data[..],
                        BranchId::for_height(network, lrz_h),
                    ) {
                        Ok(tx) => tx,
                        Err(err) => {
                            println!(
                                "failed to read tx at height {:?}/{found_h:?}/{lrz_h:?}",
                                existing_tx.h
                            );
                            continue;
                        }
                    };

                    if DUMP_SYNC {
                        println!("reading downloaded full tx for {txid:?}");
                    }
                    let keys = PreparedKeys::from_ufvk_all(&wallets[wallet_i].accounts[0].ufvk);

                    read_full_tx(
                        wallets[wallet_i],
                        0,
                        &keys,
                        existing_tx.h,
                        &tx,
                        &mut 0,
                        existing_tx.status,
                    );
                }
            }
            if DUMP_SYNC {
                println!(
                    "after  reading, there are {} in flight tx downloads",
                    in_flight_tx_requests.len()
                );
            }
        }

        //-- SEND DATA TO UI
        {
            if DUMP_NOTES {
                // println!("miner unspent UTXOs {:#?}", NL(&*miner_wallet.accounts[0].utxos));
                // println!("miner spent   UTXOs {:#?}", NL(&*miner_wallet.accounts[0].stxos));
                println!(
                    "miner unspent notes {:#?}",
                    NL(&*miner_wallet.accounts[0].unspent_orchard_notes)
                );
                println!(
                    "miner spent   notes {:#?}",
                    NL(&*miner_wallet.accounts[0].spent_orchard_notes)
                );
                println!(
                    "user  unspent notes {:#?}",
                    NL(&*user_wallet.accounts[0].unspent_orchard_notes)
                );
                println!(
                    "user  spent   notes {:#?}",
                    NL(&*user_wallet.accounts[0].spent_orchard_notes)
                );
            }

            let mut miner_unshielded_funds = 0;
            let mut miner_shielded_pending_funds = 0;
            let mut miner_shielded_spendable_funds = 0;
            for txo in &miner_wallet.accounts[0].utxos {
                miner_unshielded_funds += txo.value.into_u64();
            }
            for note in &miner_wallet.accounts[0].unspent_orchard_notes {
                let val = note.note.value().inner();
                if note.recv_h < miner_wallet.chain_tip_h.sat_sub(5) {
                    miner_shielded_spendable_funds += val;
                } else {
                    miner_shielded_pending_funds += val;
                }
            }

            let mut user_unshielded_funds = 0;
            let mut user_shielded_pending_funds = 0;
            let mut user_shielded_spendable_funds = 0;
            for txo in &user_wallet.accounts[0].utxos {
                user_unshielded_funds += txo.value.into_u64();
            }
            for note in &user_wallet.accounts[0].unspent_orchard_notes {
                let val = note.note.value().inner();
                if note.recv_h < user_wallet.chain_tip_h.sat_sub(5) {
                    user_shielded_spendable_funds += val;
                } else {
                    user_shielded_pending_funds += val;
                }
            }

            let user_txs = user_wallet.txs.clone();
            let miner_txs = miner_wallet.txs.clone();

            let mut stake_positions_bonded = Vec::new();
            let mut stake_positions_unbonded = Vec::new();
            for tx in &user_wallet.txs {
                if !(tx.is_on_bc() && tx.h.is_in_block()) {
                    continue;
                }
                if let Some(staking_action) = (&tx.staking_action) {
                    if let Some(create_bond) =
                        StakingAction_CreateNewDelegationBond::try_from_union(staking_action)
                    {
                        stake_positions_bonded.push((
                            create_bond.unique_pubkey,
                            create_bond.target_finalizer,
                            create_bond.amount_zats,
                        ));
                    }
                    if let Some(retarget) =
                        StakingAction_RetargetDelegationBond::try_from_union(staking_action)
                    {
                        if let Some(existing_i) = stake_positions_bonded
                            .iter()
                            .position(|p| p.0 == retarget.unique_pubkey)
                        {
                            stake_positions_bonded[existing_i].1 = retarget.target_finalizer;
                        }
                    }
                    if let Some(unbond) =
                        StakingAction_BeginDelegationUnbonding::try_from_union(staking_action)
                    {
                        if let Some(existing_i) = stake_positions_bonded
                            .iter()
                            .position(|p| p.0 == unbond.unique_pubkey)
                        {
                            stake_positions_unbonded.push((
                                unbond.unique_pubkey,
                                stake_positions_bonded[existing_i].1,
                                stake_positions_bonded[existing_i].2,
                            ));
                            stake_positions_bonded.remove(existing_i);
                        } else {
                            stake_positions_unbonded.push((
                                unbond.unique_pubkey,
                                [0; 32],
                                u64::MAX,
                            ));
                        }
                    }
                    if let Some(unbond) =
                        StakingAction_WithdrawDelegationBond::try_from_union(staking_action)
                    {
                        if let Some(existing_i) = stake_positions_unbonded
                            .iter()
                            .position(|p| p.0 == unbond.unique_pubkey)
                        {
                            stake_positions_unbonded.remove(existing_i);
                        }
                    }
                }
            }
            let mut user_staked_funds = 0;
            let mut user_withdrawable_funds = 0;
            for p in &mut stake_positions_bonded {
                if let Some(zats) = user_wallet.seen_bond_values.get(&p.0) {
                    p.2 = *zats;
                }
                user_staked_funds += p.2;
            }
            for p in &mut stake_positions_unbonded {
                if let Some(zats) = user_wallet.seen_bond_values.get(&p.0) {
                    p.2 = *zats;
                }
                user_withdrawable_funds += p.2;
            }
            user_wallet.care_about_bonds = stake_positions_bonded
                .iter()
                .map(|p| p.0)
                .chain(stake_positions_unbonded.iter().map(|p| p.0))
                .collect();

            fn push_if_proposed_tx(
                arr: &mut [WalletTx],
                n: &mut usize,
                proposed: &ProposedTx,
                min_stage: BlockHeight,
            ) -> bool {
                if proposed.is_in_progress()
                    && proposed.tx.h >= min_stage
                    && proposed.tx.h != BlockHeight::INVALID
                {
                    // TODO: ordering by sequence number?
                    arr[*n] = proposed.tx;
                    *n += 1;
                    true
                } else {
                    false
                }
            }
            let mut user_local_txs = [WalletTx::EMPTY; 3];
            let mut user_local_txs_n = 0;
            let mut miner_local_txs = [WalletTx::EMPTY; 3];
            let mut miner_local_txs_n = 0;
            let waiting_for_send = push_if_proposed_tx(
                &mut user_local_txs,
                &mut user_local_txs_n,
                &proposed_send,
                BlockHeight::PROPOSED,
            );
            let waiting_for_stake_to_finalizer = push_if_proposed_tx(
                &mut user_local_txs,
                &mut user_local_txs_n,
                &proposed_stake,
                BlockHeight::PROPOSED,
            );
            let waiting_for_faucet = push_if_proposed_tx(
                &mut miner_local_txs,
                &mut miner_local_txs_n,
                &proposed_faucet,
                BlockHeight::PROPOSED,
            );
            let waiting_for_shield = push_if_proposed_tx(
                &mut miner_local_txs,
                &mut miner_local_txs_n,
                &proposed_miner_shield,
                BlockHeight::PROPOSED,
            );

            // CHEATING USER-VIEW OF FAUCET BUILD
            if proposed_faucet.is_user_faucet {
                let user_view_of_faucet_tx = ProposedTx {
                    tx: user_view_of_faucet_tx(&proposed_faucet.tx),
                    prep: None,
                    tx_res: None,
                    is_user_faucet: false,
                };
                push_if_proposed_tx(
                    &mut user_local_txs,
                    &mut user_local_txs_n,
                    &user_view_of_faucet_tx,
                    BlockHeight::PROPOSED,
                );
            }

            let new_wallet_state_push_time = Instant::now();
            // println!("\n################ Wallet state period: {:#?}\n", new_wallet_state_push_time.duration_since(wallet_state_push_time));
            wallet_state_push_time = new_wallet_state_push_time;
            // DO NOT DO ANY WORK AFTER THIS LOCK IS TAKEN
            let mut lock = wallet_state.lock().unwrap();
            lock.waiting_for_send = waiting_for_send;
            lock.waiting_for_faucet = waiting_for_faucet;
            lock.waiting_for_stake_to_finalizer = waiting_for_stake_to_finalizer;

            lock.user_local_txs = user_local_txs;
            lock.user_local_txs_n = user_local_txs_n;
            lock.miner_local_txs = miner_local_txs;
            lock.miner_local_txs_n = miner_local_txs_n;

            lock.user_txs = user_txs;
            lock.miner_txs = miner_txs;
            lock.miner_unshielded_funds = miner_unshielded_funds;
            lock.miner_shielded_pending_funds = miner_shielded_pending_funds;
            lock.miner_shielded_spendable_funds = miner_shielded_spendable_funds;
            lock.miner_seen_h = miner_wallet.chain_tip_h.0;

            lock.user_unshielded_funds = user_unshielded_funds;
            lock.user_shielded_pending_funds = user_shielded_pending_funds;
            lock.user_shielded_spendable_funds = user_shielded_spendable_funds;

            lock.stake_positions_bonded = stake_positions_bonded;
            lock.stake_positions_unbonded = stake_positions_unbonded;

            lock.wallets_sync_h = wallets_sync_h.0.into();
            lock.wallets_tip_h = network_tip_h.0.into();

            lock.staked_balance = user_staked_funds;
            lock.withdrawable_balance = user_withdrawable_funds;
        }

        // Anchor debugging
        // {
        //     for i in 0..miner_wallet.chain_tip_h.0 {
        //         let orchard_anchor_h = BlockHeight(i);
        //         let try_anchor = match orchard_tree.root_at_checkpoint_id(&orchard_anchor_h).expect("Infallible MemoryShardStore") {
        //             Some(root) => Some(orchard::Anchor::from(root)),
        //             None => {
        //                 println!("tx build: couldn't get anchor at {orchard_anchor_h:?}");
        //                 None
        //             }
        //         };
        //         println!("wallet compute anchor {} => {:?}", i, try_anchor);
        //     }
        // }

        if faucet_shield_cooldown_instant.elapsed().as_secs() > 15
            && !proposed_miner_shield.is_in_progress()
        {
            let memo = MemoBytes::from_bytes("shielding notes".as_bytes()).unwrap();
            let ok = miner_wallet
                .shield_transparent_zats(
                    network,
                    &mut proposed_miner_shield,
                    &mut client,
                    &miner_usk,
                    1000000000,
                    &orchard_tree,
                    memo,
                )
                .is_some();
            if DUMP_TX_BUILD {
                println!("Try miner shield {ok:?}");
            }
            faucet_shield_cooldown_instant = Instant::now();

            // also inspect bonds
            for bond_key in &user_wallet.care_about_bonds {
                match client
                    .get_bond_info(zcash_client_backend::proto::service::BondInfoRequest {
                        bond_key: bond_key.to_vec(),
                    })
                    .await
                {
                    Ok(response) => {
                        let info = response.into_inner();
                        user_wallet.seen_bond_values.insert(*bond_key, info.amount);
                    }
                    Err(e) => {
                        println!("Failed to get bond info: {:?}", e);
                    }
                };
            }
        }

        if AUTO_SPEND {
            if
            /*user_wallet.accounts[0].unspent_orchard_notes.len() == 0 &&*/
            !proposed_faucet.is_in_progress() {
                // the user needs money, try to send some (doesn't matter if we fail until we've mined some)
                let ok = miner_wallet
                    .send_orchard_to_orchard_zats(
                        network,
                        &mut proposed_faucet,
                        &mut client,
                        &miner_usk,
                        FAUCET_VALUE,
                        &orchard_tree,
                        *user_ua.orchard().unwrap(),
                        MemoBytes::from_bytes("auto-sent from miner".as_bytes()).unwrap(),
                    )
                    .is_some();
            } else if !auto_spend.0 {
                // auto_spend.0 = true;
                // proposed_stake = user_wallet.stake_orchard_to_finalizer(network, &mut client, &user_usk, 100_000_000, &orchard_tree, [0xcd;32]);
            }
        }

        // @todo(judah): I'm thinking the weird frame hitch we get in the UI is caused by this loop,
        // since it's probably waiting for the wallet_state mutex to unlock.
        let mut retries_this_round = 6;
        loop {
            // Check lock availability without storing the MutexGuard type
            if wallet_state.try_lock().is_err() {
                if retries_this_round > 0 {
                    retries_this_round -= 1;
                    println!("wallet lock retry ({retries_this_round} attempts remaining)");
                    tokio::time::sleep(tokio::time::Duration::from_millis(9)).await;
                    continue;
                } else {
                    break;
                }
            }

            let action: WalletAction = {
                let mut wallet_state = wallet_state.lock().unwrap();

                if DUMP_ACTIONS {
                    println!(
                        "*** wallet has {:?} actions in flight",
                        wallet_state.actions_in_flight.len()
                    );
                }
                let Some(action) = wallet_state.actions_in_flight.front() else {
                    // if we're not doing anything else, process a faucet RPC request
                    if !proposed_faucet.is_in_progress() {
                        let mut q = FAUCET_Q.lock().unwrap();
                        if DUMP_FAUCET {
                            println!("faucet Q read_o: {}, write_o: {}", q.read_o, q.write_o);
                        }
                        if q.len() > 0 {
                            let i = q.read_o as usize % q.data.len();
                            if DUMP_FAUCET {
                                println!("faucet Q new element at {i}: {:?}", q.data[i]);
                            }
                            if let Some(orchard_addr) = q.data[i] {
                                let memo = MemoBytes::from_bytes("With love from your favourite faucet... Don't spend it all at once!".as_bytes()).unwrap();
                                let ok = miner_wallet
                                    .send_orchard_to_orchard_zats(
                                        network,
                                        &mut proposed_faucet,
                                        &mut client,
                                        &miner_usk,
                                        FAUCET_VALUE,
                                        &orchard_tree,
                                        orchard_addr,
                                        memo,
                                    )
                                    .is_some();
                                proposed_faucet.is_user_faucet = false;
                                if DUMP_ACTIONS {
                                    println!("Try RPC faucet send: {ok:?}");
                                }
                                just_init_new_tx |= ok;
                                q.read_o += 1;
                            } else {
                                println!("Faucet Q error: got None result where there should be valid data");
                            }
                        }
                    }

                    break;
                };
                action.clone()
            };

            let ok: bool = match &action {
                &WalletAction::RequestFromFaucet => {
                    let memo = MemoBytes::from_bytes(
                        "With love from your favourite faucet... Don't spend it all at once!"
                            .as_bytes(),
                    )
                    .unwrap();
                    let ok = miner_wallet
                        .send_orchard_to_orchard_zats(
                            network,
                            &mut proposed_faucet,
                            &mut client,
                            &miner_usk,
                            FAUCET_VALUE,
                            &orchard_tree,
                            *user_ua.orchard().unwrap(),
                            memo,
                        )
                        .is_some();
                    proposed_faucet.is_user_faucet = true;
                    just_init_new_tx |= ok;
                    if DUMP_ACTIONS {
                        println!("Try miner send: {ok:?}");
                    }
                    true // ALT ok
                }

                &WalletAction::StakeToFinalizer(amount, target_finalizer) => {
                    let ok = user_wallet
                        .stake_orchard_to_finalizer(
                            network,
                            &mut proposed_stake,
                            &mut client,
                            &user_usk,
                            amount.into_u64(),
                            &orchard_tree,
                            target_finalizer,
                        )
                        .is_some();
                    println!("Try stake: {ok:?}");
                    just_init_new_tx |= ok;
                    ok
                }

                WalletAction::SendToAddress(address, amount) => {
                    if let Some(orchard_address) = address.orchard() {
                        let memo =
                            MemoBytes::from_bytes("send from user wallet".as_bytes()).unwrap();
                        let ok = user_wallet
                            .send_orchard_to_orchard_zats(
                                network,
                                &mut proposed_send,
                                &mut client,
                                &user_usk,
                                amount.into_u64(),
                                &orchard_tree,
                                *orchard_address,
                                memo,
                            )
                            .is_some();
                        just_init_new_tx |= ok;
                        if DUMP_ACTIONS {
                            println!("Try user send: {ok:?}");
                        }
                        true // ALT ok
                    } else {
                        false
                    }
                }

                &WalletAction::UnstakeFromFinalizer(txid) => {
                    let ok = user_wallet
                        .begin_unbonding_using_orchard(
                            network,
                            &mut proposed_stake,
                            &mut client,
                            &user_usk,
                            &orchard_tree,
                            *txid.as_ref(),
                        )
                        .is_some();
                    just_init_new_tx |= ok;
                    if DUMP_ACTIONS {
                        println!("Try unstake: {ok:?}");
                    }
                    ok
                }

                &WalletAction::RetargetBond(txid, new_target) => {
                    let ok = user_wallet
                        .retarget_bond_using_orchard(
                            network,
                            &mut proposed_stake,
                            &mut client,
                            &user_usk,
                            &orchard_tree,
                            *txid.as_ref(),
                            new_target,
                        )
                        .is_some();
                    just_init_new_tx |= ok;
                    if DUMP_ACTIONS {
                        println!("Try retarget: {ok:?}");
                    }
                    ok
                }

                &WalletAction::ClaimBond(txid) => {
                    let ok = user_wallet
                        .claim_bond_using_orchard(
                            network,
                            &mut proposed_stake,
                            &mut client,
                            &user_usk,
                            &orchard_tree,
                            *txid.as_ref(),
                        )
                        .await
                        .is_some();
                    just_init_new_tx |= ok;
                    if DUMP_ACTIONS {
                        println!("Try withdraw stake: {ok:?}");
                    }
                    ok
                }

                &WalletAction::TestStakeAction => true,
            };

            if !ok {
                println!("** Failed to process action: {:?}", &action);
            }

            wallet_state.lock().unwrap().actions_in_flight.pop_front();
        }

        //-- INCREMENTALLY SEND TXS
        // (we want to skip this slow work to send tx data to UI while this is still single-threaded to show the started tx immediately)
        if !just_init_new_tx {
            async fn continue_proposed_tx<P: Parameters>(
                wallet: &mut ManualWallet,
                network: P,
                tx: &mut ProposedTx,
                client: &mut CompactTxStreamerClient<Channel>,
                desc: &str,
                loud: bool,
            ) -> WalletTx {
                let pre_mined_h = tx.tx.h;
                match tx.tx.h {
                    BlockHeight::PROPOSED => {
                        let mut ok = false;
                        if let None = tx.tx.parts[0].checked_add(&tx.tx.parts[1]).and_then(|a| {
                            a.checked_add(&WalletTxPart::from_staking_action(tx.tx.staking_action))
                        }) {
                            println!("tx build error: total values are too large to be represented by Zatoshis");
                        } else if let Some(prep) = tx.prep.take() {
                            ok = wallet.build_tx_from_prep(network, tx, prep);
                        } else {
                            println!("unexpectedly no prep ready for build");
                        };

                        if !ok {
                            // give it a final failed height
                            tx.tx.status =
                                TxStatus::HardFail(tx.tx.h, ErrBuf::from_str("failed to build"));
                            tx.tx.h = wallet.chain_tip_h;
                        }
                        let mut insert_i = 0;
                        update_insert_i(&wallet.txs, &mut insert_i, tx.tx.h);
                        update_with_tx(wallet, tx.tx, &mut insert_i);
                        let result = tx.tx;

                        if loud {
                            println!(
                                "tried to build {desc}: ({ok}) was {pre_mined_h}, now {} ({:?})",
                                tx.tx.h, tx.tx.status
                            );
                        }
                        if !ok {
                            *tx = ProposedTx::EMPTY; // Meaningless to retry
                        }
                        result
                    }

                    // NOTE: do a full loop before returning here
                    BlockHeight::BUILT => {
                        let mut ok = false;
                        if let Some(tx_res) = &tx.tx_res {
                            ok = wallet
                                .send_built_tx(network, client, &mut tx.tx, tx_res.transaction())
                                .await;
                            if loud {
                                println!(
                                    "tried to send {desc}: ({ok}) was {pre_mined_h}, now {} ({:?})",
                                    tx.tx.h, tx.tx.status
                                );
                            }
                        } else {
                            *tx = ProposedTx::EMPTY; // not enough info to retry; bail
                            println!("unexpectedly no tx_res ready for send");
                        }

                        let mut insert_i = 0;
                        update_insert_i(&wallet.txs, &mut insert_i, tx.tx.h);
                        update_with_tx(wallet, tx.tx, &mut insert_i);
                        let result = tx.tx;

                        if ok {
                            *tx = ProposedTx::EMPTY; // Done. Don't retry
                        } else {
                            *tx = ProposedTx::EMPTY; // TODO: fixed number of retries
                        }

                        result
                    }

                    _ => WalletTx::EMPTY,
                }
            }

            let faucet_tx = continue_proposed_tx(
                &mut miner_wallet,
                network,
                &mut proposed_faucet,
                &mut client,
                "faucet send",
                DUMP_TX_SEND,
            )
            .await;
            if proposed_faucet.is_user_faucet && faucet_tx.txid != TxId::from_bytes([0; 32]) {
                let user_faucet_tx = user_view_of_faucet_tx(&faucet_tx);
                let mut insert_i = 0;
                update_insert_i(&user_wallet.txs, &mut insert_i, user_faucet_tx.h);
                update_with_tx(&mut user_wallet, user_faucet_tx, &mut insert_i);
            }

            continue_proposed_tx(
                &mut miner_wallet,
                network,
                &mut proposed_miner_shield,
                &mut client,
                "miner shield",
                DUMP_TX_SEND && false,
            )
            .await;
            continue_proposed_tx(
                &mut user_wallet,
                network,
                &mut proposed_stake,
                &mut client,
                "stake",
                DUMP_TX_SEND,
            )
            .await;
            continue_proposed_tx(
                &mut user_wallet,
                network,
                &mut proposed_send,
                &mut client,
                "send",
                DUMP_TX_SEND,
            )
            .await;
        }
    }
}

/*
#[derive(Debug)]
struct DerVerifier {
    certificate: &'static [u8],
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for DerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &tonic::transport::CertificateDer<'_>,
        _intermediates: &[tonic::transport::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if end_entity.as_ref() == self.certificate {
            Ok(ServerCertVerified::assertion())
        }
        else {
            Err(rustls::Error::InvalidCertificate(CertificateError::UnknownIssuer))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &tonic::transport::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &tonic::transport::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}
*/
