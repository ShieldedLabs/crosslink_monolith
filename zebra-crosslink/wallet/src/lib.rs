//! Internal wallet
#![allow(warnings)]

use rand::seq::SliceRandom;
use tokio::task::futures;
use zcash_client_backend::data_api::WalletCommitmentTrees;
use orchard::note_encryption::{ CompactAction as OrchardCompactAction, OrchardDomain };
use rand_chacha::rand_core::SeedableRng;
use rand_core::OsRng;
use rand::{Rng, thread_rng};
use secrecy::{ExposeSecret,SecretVec,Secret};
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::convert::{identity, Infallible};
use std::future::Future;
use std::{mem, slice};
use std::ptr::null;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio_rustls::rustls;
use tonic::client::GrpcService;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint};
use tonic::IntoRequest;
use zcash_client_backend::data_api::chain::{BlockCache, CommitmentTreeRoot};
use zcash_client_backend::data_api::wallet::{ConfirmationsPolicy, TargetHeight, create_proposed_transactions, propose_shielding, shield_transparent_funds};
use zcash_client_backend::proto::service::{GetSubtreeRootsArg, RawTransaction, TreeState, TxFilter};
use zcash_client_backend::wallet::WalletTransparentOutput;
use zcash_client_sqlite::error::SqliteClientError;
use zcash_client_sqlite::util::SystemClock;
use zcash_client_sqlite::{AccountUuid, WalletDb};
use zcash_note_encryption::{try_compact_note_decryption, try_note_decryption, try_output_recovery_with_ovk, ShieldedOutput};
use zcash_primitives::transaction::builder::{BuildConfig, Builder as TxBuilder};
use zcash_primitives::transaction::components::TxOut;
use zcash_primitives::transaction::fees::{
    self,
    FeeRule,
    zip317,
};
use zcash_primitives::transaction::sighash::{signature_hash, SignableInput};
use zcash_primitives::transaction::txid::TxIdDigester;
use zcash_primitives::transaction::{Authorized, StakingAction_CreateNewDelegationBond, Transaction, TransactionData, TxVersion, Unauthorized};
use zcash_primitives::transaction::{RosterMember, StakingAction, StakingActionKind, StakeTxId};
use zcash_proofs::prover::LocalTxProver;
use zcash_protocol::consensus::{BlockHeight as LRZBlockHeight, BranchId};
use zcash_protocol::memo::MemoBytes;
use zcash_protocol::value::{ZatBalance, Zatoshis};
use zcash_protocol::{PoolType, ShieldedProtocol, TxId};
use zcash_transparent::{
    address::TransparentAddress,
    builder::{TransparentBuilder, TransparentSigningSet},
    bundle::OutPoint,
    keys::{IncomingViewingKey, TransparentKeyScope, NonHardenedChildIndex},
};
use zebra_chain::block::{Hash as BlockHash, Height, ZCASH_BLOCK_VERSION};
use zebra_chain::parameters::NetworkUpgrade;
use zebra_chain::sapling;
use zebra_chain::serialization::{ZcashDeserialize, ZcashSerialize};
use zebra_chain::sprout::commitment;
use zebra_chain::transaction::LockTime;
use zebra_chain::transparent::{self, Input, Utxo, MIN_TRANSPARENT_COINBASE_MATURITY};

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
        Balance,
        wallet, Account as APIAccount, AccountBirthday, AccountPurpose, WalletRead, WalletWrite,
        Zip32Derivation,
    },
    encoding::AddressCodec,
    keys::{
        UnifiedAddressRequest, UnifiedFullViewingKey, UnifiedIncomingViewingKey, UnifiedSpendingKey,
    },
    proto::{
        compact_formats::{CompactBlock, CompactTx, CompactSaplingSpend, CompactSaplingOutput, CompactOrchardAction},
        service::{
            compact_tx_streamer_client::CompactTxStreamerClient, BlockId, BlockRange, ChainSpec,
            Empty, GetAddressUtxosArg, LightdInfo, TransparentAddressBlockFilter,
        },
    },
};

use zcash_protocol::consensus::{NetworkType, Parameters, MAIN_NETWORK, TEST_NETWORK};

// NOTE: this has slightly different semantics from the protocol version, hence the different type
// TODO: some code becomes simpler with a u64, but I'm leaving this the same as default for now
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct BlockHeight(pub u32);
impl BlockHeight {
    // NOTE: these constants corresponds to the semantic that the lower-down it is,
    // the more sure we are about its continued existence
    pub const INVALID: Self = Self(u32::MAX); // NOTE: here for headroom for +1 to fake <= using <
    // ALT: maybe better to go the other way round: use <= and saturating_sub(1)
    // TODO: creating (slow), sending, sent, mempool
    pub const INTERNAL: Self = Self(u32::MAX-1);
    pub const MEMPOOL: Self = Self(u32::MAX-2);

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

impl std::fmt::Display for BlockHeight {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            Self::INVALID => write!(f, "<invalid>"),
            Self::INTERNAL => write!(f, "<internal>"),
            Self::MEMPOOL => write!(f, "<mempool>"),
            _ => self.0.fmt(f)
        }
    }
}

/// "little endian hash"
pub struct LESlice<'a>(pub &'a [u8]);
impl std::fmt::Display for LESlice<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let n = usize::min(self.0.len(), f.precision().unwrap_or(self.0.len()));
        for i in 0..n {
            write!(f, "{:02x}", self.0[31-i])?;
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
fn unknown_tree_position() -> incrementalmerkletree::Position { incrementalmerkletree::Position::from(u64::MAX) }

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
        write!(f, "OrchardNote{{ recv:{:?}, spent:{:?}, txid:{}, nf:{:?}, value:{}, pos:{}}}",
            self.recv_h, self.spent_h, self.txid, self.nf, self.note.value().inner(),
            u64::from(self.position)
            // u64::from(self.witness.witnessed_position()), self.witness.root()
            )
    }
}

impl OrchardNote {
    fn monotonically_update(&mut self, mut new_note: OrchardNote) {
        if new_note.position < unknown_tree_position() {
            if (self.position < unknown_tree_position() &&
                self.position != new_note.position)
            {
                println!("ERROR: orchard note has 2 different valid positions: {:?} vs {:?}", self.position, new_note.position);
            }
            self.position = new_note.position;
        } else {
            new_note.position = self.position; // NOTE: just for cmp
        }

        if self != &new_note {
            println!("ERROR: orchard_note mismatch: {:?} vs {:?}", self, new_note);
        }
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

struct Timer<'a> { t_bgn: std::time::Instant, name: &'a str }
impl<'a> Timer<'a> {
    pub fn scope(name: &'a str) -> Self {
        println!("started {}", name);
        Self {
            name, t_bgn: std::time::Instant::now()
        }
    }
}
impl Drop for Timer<'_> {
    fn drop(&mut self) {
        println!("{} took {}ms", self.name, self.t_bgn.elapsed().as_millis());
    }
}


fn block_policy_10() -> ConfirmationsPolicy { ConfirmationsPolicy::new(std::num::NonZeroU32::new(5).unwrap(), std::num::NonZeroU32::new(5).unwrap(), false).unwrap() }

#[derive(Debug, Clone, PartialEq)]
enum WalletAction {
    RequestFromFaucet,
    TestStakeAction,
    StakeToFinalizer(Zatoshis, [u8; 32]),
    UnstakeFromFinalizer(TxId),
    SendToAddress(UnifiedAddress, Zatoshis),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WalletTxKind {
    Send,
    Receive,
    SelfSend,
    Shield, // a form of SelfSend
    Stake,
    Unstake,
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

    pub fn checked_add(&self, rhs: &WalletTxPart) -> Option<WalletTxPart> {
        Some(WalletTxPart {
            spent_note_count: (self.spent_note_count + rhs.spent_note_count),
            sent_note_count:  (self.sent_note_count  + rhs.sent_note_count),
            recv_note_count:  (self.recv_note_count  + rhs.recv_note_count),
            spent_zats:       (self.spent_zats       + rhs.spent_zats)?,
            sent_zats:        (self.sent_zats        + rhs.sent_zats)?,
            recv_zats:        (self.recv_zats        + rhs.recv_zats)?,
        })
    }

    pub fn unchecked_add(&self, rhs: &WalletTxPart) -> WalletTxPart {
        WalletTxPart {
            spent_note_count: (self.spent_note_count + rhs.spent_note_count),
            sent_note_count:  (self.sent_note_count  + rhs.sent_note_count),
            recv_note_count:  (self.recv_note_count  + rhs.recv_note_count),
            spent_zats:       (self.spent_zats       + rhs.spent_zats).expect("already checked"),
            sent_zats:        (self.sent_zats        + rhs.sent_zats).expect("already checked"),
            recv_zats:        (self.recv_zats        + rhs.recv_zats).expect("already checked"),
        }
    }
}

type TxPartFlags = u8;
struct TxParts(pub TxPartFlags);
impl TxParts {
    const NONE:          TxPartFlags = 0;
    const TRANSPARENT:   TxPartFlags = 1 << WalletTxPart::TRANSPARENT;
    const SHIELDED_RECV: TxPartFlags = 1 << WalletTxPart::SHIELDED;
    const SHIELDED_SENT: TxPartFlags = 1 << 2;
    const MEMO:          TxPartFlags = 1 << 3;
    const STAKING_ACTION: TxPartFlags = 1 << 4;

    const FULL_TX: TxPartFlags = (
        Self::TRANSPARENT | Self::SHIELDED_RECV | Self::SHIELDED_SENT | Self::MEMO | Self::STAKING_ACTION
    );
}

// NOTE: trying to not store data that can be computed directly
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WalletTx {
    pub account_id: usize,
    pub txid: zcash_protocol::TxId,
    pub expiry_h: Option<BlockHeight>,
    pub mined_h: BlockHeight,

    // TODO: track whether full Transaction has been read
    pub is_coinbase: bool,
    // NOTE: this is whether we have checked for parts, not whether we have any
    pub part_flags: TxPartFlags,
    pub parts: [WalletTxPart; 2], // 0=>transparent, 1=>shielded

    // TODO: keep all memos in single contiguous array as ((txid, index), memo)
    pub memo_count: usize,
    pub memo: [u8; 512],

    pub is_outside_bc: bool,

    pub staking_action: Option<StakingAction>,
}

impl WalletTx {
    pub fn with_fake_data(kind: WalletTxKind, sent: u64, recv: u64, shielding: bool, memo: &str, mined_h: u32) -> Self {
        let mut memo_as_bytes = [0u8; 512];
        &memo_as_bytes[0..memo.len()].copy_from_slice(memo.as_bytes());

        Self {
            account_id: 0,//AccountUuid::default(),
                txid: TxId::from_bytes([0; 32]),
            expiry_h: None,
            mined_h: if mined_h != 0 { (BlockHeight(mined_h)) } else { BlockHeight::MEMPOOL },
            part_flags: TxParts::FULL_TX,
            parts: [
                WalletTxPart { // Transparent
                    spent_note_count: (sent > 0 && shielding) as usize,
                    sent_note_count:  (sent > 0 && shielding) as usize,
                    recv_note_count:  0,
                    spent_zats: Zatoshis::from_u64(sent * shielding as u64).unwrap(),
                    sent_zats:  Zatoshis::from_u64(sent * shielding as u64).unwrap(),
                    recv_zats:  Zatoshis::ZERO,
                },
                WalletTxPart { // Shielded
                    spent_note_count: (sent > 0 && !shielding) as usize,
                    sent_note_count:  (sent > 0 && !shielding) as usize,
                    recv_note_count:  (recv > 0) as usize,
                    spent_zats: Zatoshis::from_u64(sent * !shielding as u64).unwrap(),
                    sent_zats:  Zatoshis::from_u64(sent * !shielding as u64).unwrap(),
                    recv_zats: Zatoshis::from_u64(if shielding { sent } else { recv }).unwrap(),
                },
            ],
            memo_count: if memo.len() != 0 { 1 } else { 0 },
                memo: memo_as_bytes,
            is_coinbase: false,
            is_outside_bc: false,
            staking_action: None,
    }
}

    pub fn totals(&self) -> WalletTxPart {
        self.parts[0].unchecked_add(&self.parts[1])
    }

    pub fn account_value_delta(&self) -> ZatBalance {
        let all = self.totals();
        // NOTE: into_i64 isn't pub...
        ZatBalance::from_i64(all.recv_zats.into_u64() as i64 - all.spent_zats.into_u64() as i64).expect("checked before")
    }

    pub fn fee(&self) -> Zatoshis {
        let all = self.totals();
        // NOTE: into_i64 isn't pub...
        let fee: u64 = (all.spent_zats.into_u64() as i64 - all.sent_zats.into_u64() as i64).try_into().expect("fee cannot be negative");
        Zatoshis::from_u64(fee).expect("fee cannot be unrepresentable")
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
            return WalletTxKind::Unstake;
        }
        let all = self.totals();
        // if *all* of the sent zats go to ourself we assume this was the purpose
        // otherwise we assume the self-sent zats are change
        // ALT: only consider it change if a single note is received (per pool?)
        let is_self_send = all.sent_zats == all.recv_zats;
        if is_self_send {
            let all_spent_is_t     = self.parts[0].spent_zats == all.spent_zats;
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
//         match self.mined_h {
//             BlockHeight::MEMPOOL  => (WalletTxLoc::Mempool,  falsself.is_outside_bc),
//             BlockHeight::INTERNAL => (WalletTxLoc::Internal, falsself.is_outside_bc),
//             _ => {
//                 if self.is_outside_bc {
//                     (WalletTxLoc::Block(0), self.is_outside_bc)
//                 } else if self.mined_h > bc_tip_h {
//                     println!("ERROR: mined h on best chain ({}) higher than tip ({})", self.mined_h, bc_tip_h);
//                     return (WalletTxLoc::Block(0), true);
//                 } else if self.mined_h <= finalized_h {
//                     (WalletTxLoc::Finalized, self.is_outside_bc)
//                 } else {
//                     (WalletTxLoc::Block(bc_tip_h - self.mined_h), self.is_outside_bc)
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

fn w_flip(use_i: &mut usize, update_i: &mut usize) {
    if *use_i == *update_i {
        *update_i = 1;
    } else {
        *use_i ^= 1;
        *update_i ^= 1;
    }
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

    pub staked_balance:  u64, // in zats
    pub show_staked_balance: bool,

    pub user_txs:      Vec<WalletTx>,
    pub miner_txs:     Vec<WalletTx>,
    pub roster:        Vec<WalletRosterMember>,
    pub staked_roster: Vec<([u8; 32] /* pub key */, [u8; 32] /* txid */, u64 /* initial */, u64 /* accumulated */)>,

    pub waiting_for_faucet: bool,
    pub waiting_for_stake_to_finalizer: bool,
    pub waiting_for_send: bool,

    pub user_recv_ua: String,

    pub actions_in_flight: VecDeque<WalletAction>,
}

impl WalletState {
    pub fn new() -> Self {
        WalletState {
            ..Default::default()
        }
    }

    pub fn user_balance(&self)          -> u64 { self.user_unshielded_funds + self.user_shielded_spendable_funds + self.user_shielded_pending_funds }
    pub fn user_pending_balance(&self)  -> u64 { self.user_shielded_pending_funds }
    pub fn miner_balance(&self)         -> u64 { self.miner_unshielded_funds + self.miner_shielded_spendable_funds + self.miner_shielded_pending_funds }
    pub fn miner_pending_balance(&self) -> u64 { self.miner_shielded_pending_funds }

    pub fn request_from_faucet(&mut self) {
        self.waiting_for_faucet = true;

        if self.actions_in_flight.iter().filter(|a| match a { WalletAction::RequestFromFaucet => true, _ => false }).count() != 0 {
            return;
        }

        self.actions_in_flight.push_back(WalletAction::RequestFromFaucet);
    }

    pub fn stake_to_finalizer(&mut self, amount: u64, target_finalizer: [u8; 32]) {
        if self.actions_in_flight.iter().filter(|a| match a { WalletAction::StakeToFinalizer(_,_) => true, _ => false }).count() != 0 {
            return;
        }

        self.waiting_for_stake_to_finalizer = true;
        self.actions_in_flight.push_back(WalletAction::StakeToFinalizer(Zatoshis::from_u64(amount).expect("Invalid amount given to stake_to_finalizer"), target_finalizer));
    }

    pub fn unstake_from_finalizer(&mut self, txid: [u8; 32]) {
        let txid = TxId::from_bytes(txid);
        if self.actions_in_flight.iter().filter(|a| match a { WalletAction::UnstakeFromFinalizer(id) if id.eq(&txid) => true, _ => false }).count() != 0 {
            return;
        }
        self.actions_in_flight.push_back(WalletAction::UnstakeFromFinalizer(txid));
    }

    pub fn send_to_address(&mut self, address: String, amount: u64) {
        let Ok(address) = UnifiedAddress::decode(&TEST_NETWORK /* @todo */, &address) else {
            println!("Invalid address for send: {}", address);
            return;
        };

        if self.actions_in_flight.iter().filter(|a| match a {
            WalletAction::SendToAddress(addr, amt) if amt.into_u64() == amount && addr.eq(&address) => true,
            _ => false
        }).count() != 0 {
            return;
        }

        self.waiting_for_send = true;
        self.actions_in_flight.push_back(WalletAction::SendToAddress(address, Zatoshis::from_u64(amount).expect("Invalid amount given to stake_to_finalizer")));
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
    Transparent {
        dst: TransparentAddress,
        zats: Zatoshis,
    },
    // TODO: sprout, sapling?
    Orchard {
        ovk: Option<orchard::keys::OutgoingViewingKey>,
        dst: orchard::Address,
        zats: u64,
        memo: MemoBytes,
    }
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

pub static wallet_main_zaino_port : Mutex<u16> = Mutex::new(0);

pub enum TxPool<'a> {
    Transparent,
    // Sprout,
    // Sapling,
    // Orchard(orchard::Anchor),
    Orchard(&'a OrchardShardTree),
}

fn transparent_keys_from_usk(usk: &UnifiedSpendingKey, index: u32) -> Option<(secp256k1::PublicKey, secp256k1::SecretKey)> {
    let transparent = usk.transparent();
    let account_pubkey = transparent.to_account_pubkey();
    let child_index = NonHardenedChildIndex::const_from_index(index);
    let address_pubkey = account_pubkey.derive_address_pubkey(TransparentKeyScope::EXTERNAL, child_index).ok()?;
    let address_privkey = transparent.derive_external_secret_key(child_index).ok()?;
    Some((address_pubkey, address_privkey))
}

fn addrs_from_account(account: &ManualAccount, index: u32) -> Option<(TransparentAddress, UnifiedAddress)> {
    // NOTE: the wallet auto-increments the child index so this isn't recognized
    let ufvk = &account.ufvk;
    let (ua, di_) = ufvk.find_address(orchard::keys::DiversifierIndex::new(), UnifiedAddressRequest::ORCHARD).ok()?;
    let account_pubkey = ufvk.transparent()?;
    let child_index = NonHardenedChildIndex::const_from_index(index);
    let address_pubkey = account_pubkey.derive_address_pubkey(TransparentKeyScope::EXTERNAL, child_index).ok()?;
    Some((TransparentAddress::from_pubkey(&address_pubkey), ua))
        // Some(account.default_address().ok()??.0)
}

fn update_insert_i(txs: &[WalletTx], insert_i: &mut usize, block_h: BlockHeight) {
    // put at the *end* of txs at the same height
    // i.e. primarily sorted by mined height, secondarily by discovered_time
    *insert_i += txs[*insert_i..].partition_point(|tx| tx.mined_h <= block_h);
}

fn update_with_tx(wallet: &mut ManualWallet, txid: TxId, mut new_tx: WalletTx, insert_i: &mut usize) {
    // find if there's an existing height/transaction for this txid
    let new_totals = new_tx.totals();
    if (new_totals.spent_note_count == 0 &&
        new_totals.recv_note_count == 0 &&
        new_totals.sent_note_count == 0)
    {
        // not our transaction; ignore
        return;
    }

    if let Some(tx_h) = wallet.tx_h_map.get_mut(&txid) {
        if let Some(tx_i) = tx_mined_h_position(&wallet.txs, *tx_h, &txid) {
            let old_tx = &wallet.txs[tx_i];
            if old_tx != &new_tx {
                if new_tx.mined_h == BlockHeight::MEMPOOL && old_tx.mined_h.is_in_block() && !old_tx.is_outside_bc {
                    // NOTE: mempool fetch is not synced to chain reading
                    println!("mempool tx already in best chain; skipping");
                    return;
                }

                println!("{} wallet updated existing transaction {txid} {:?} => {:?}", wallet.name, old_tx.mined_h, new_tx.mined_h);
                // println!("{} wallet updated existing transaction {txid} {old_tx:?} => {new_tx:?}", wallet.name);

                // leave the tx-parts from the components not provided here
                if (new_tx.part_flags & TxParts::TRANSPARENT) == 0 {
                    new_tx.parts[WalletTxPart::TRANSPARENT] = old_tx.parts[WalletTxPart::TRANSPARENT];
                }
                if (new_tx.part_flags & TxParts::SHIELDED_RECV) == 0 {
                    new_tx.parts[WalletTxPart::SHIELDED].recv_note_count = old_tx.parts[WalletTxPart::SHIELDED].recv_note_count;
                    new_tx.parts[WalletTxPart::SHIELDED].recv_zats = old_tx.parts[WalletTxPart::SHIELDED].recv_zats;
                }
                if (new_tx.part_flags & TxParts::SHIELDED_SENT) == 0 {
                    new_tx.parts[WalletTxPart::SHIELDED].spent_note_count = old_tx.parts[WalletTxPart::SHIELDED].spent_note_count;
                    new_tx.parts[WalletTxPart::SHIELDED].spent_zats       = old_tx.parts[WalletTxPart::SHIELDED].spent_zats;
                    new_tx.parts[WalletTxPart::SHIELDED].sent_note_count  = old_tx.parts[WalletTxPart::SHIELDED].sent_note_count;
                    new_tx.parts[WalletTxPart::SHIELDED].sent_zats        = old_tx.parts[WalletTxPart::SHIELDED].sent_zats;
                }
                if (new_tx.part_flags & TxParts::MEMO) == 0 {
                    new_tx.memo_count = old_tx.memo_count;
                    new_tx.memo       = old_tx.memo;
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
        *tx_h = new_tx.mined_h;
    } else {
        wallet.tx_h_map.insert(txid, new_tx.mined_h);
        println!("{} wallet inserted new transaction {txid} at {:?}", wallet.name, new_tx.mined_h);
    }
    wallet.txs.insert(*insert_i, new_tx);
    *insert_i += 1;

    // wallet.audit_txs();
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
// NOTE: WalletDb doesn't store spending key, so we'll do the same here...
#[derive(Clone, Debug)]
pub struct ManualWallet {
    pub name: &'static str,
    pub accounts: Vec<ManualAccount>,
    pub chain_tip_h: BlockHeight,
    // TODO: change type
    // TODO: to avoid nested variably-sized data, we could split these into actions that are
    // txid-linked, then reconstruct on request
    /// sorted by (mined_h, discovery_time)
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
}
// N.B. using some of the same API as WalletDb to allow smooth transition/comparison
impl ManualWallet {
    pub fn chain_height(&self)          -> BlockHeight { self.chain_tip_h }
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
    pub fn get_wallet_summary(&self, confirmations_policy: ConfirmationsPolicy) -> Result<Option<data_api::WalletSummary<usize>>, Infallible> {
        let mut account_balances = HashMap::with_capacity(self.accounts.len());
        for account_i in 0..self.accounts.len() {
            account_balances.insert(account_i, self.accounts[account_i].balance_changes.last().unwrap().1);
        }

        Ok(Some(data_api::WalletSummary::new(
            account_balances,
            LRZBlockHeight::from_u32(self.chain_tip_h.0),
            LRZBlockHeight::from_u32(self.fully_decoded_height().0), // TODO: fully_detected_height?
            // ignored:
            data_api::Progress::new(data_api::Ratio::new(0,0), None),
            0,// sapling subtree
            0,// orchard subtree
        )))
    }

    // TODO: fee API (need to account for paying for fee forcing more notes, changing the fee)
    // TODO: allow one destination to have an empty value for "send the rest here" (this could be
    // change or normal recipient)
    pub async fn send_zats<P: Parameters>(
        &mut self, network: P, client: &mut CompactTxStreamerClient<Channel>, outputs: &[TxOutput],
        src_usk: &UnifiedSpendingKey, zats: Zatoshis, fee_zats: Zatoshis, opts: &TxOptions<'_>
    ) -> Option<TxId>
    {
        let tz = Timer::scope("send_zats");
        let block_h = self.chain_tip_h.0 + 1;

        let account = &self.accounts[0];
        let keys = PreparedKeys::from_ufvk_all(&account.ufvk);
        let (t_addr, ua) = addrs_from_account(account, 0).unwrap(); // @Hack
        let orchard_addr = ua.orchard().unwrap();

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
                    orchard_anchor = match shardtree.root_at_checkpoint_id(&orchard_anchor_h).expect("Infallible MemoryShardStore") {
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
                println!("tried to send from transparent source without available transparent keys");
                return None;
            };
            signing_set.add_key(privkey);
            t_pubkey = Some(pubkey);
        }
        let mut sapling_esk: &[sapling_crypto::zip32::ExtendedSpendingKey] = &[];
        let mut orchard_sak = &[orchard::keys::SpendAuthorizingKey::from(src_usk.orchard())];


        let mut txb = TxBuilder::new(
            network,
            LRZBlockHeight::from_u32(block_h),
            BuildConfig::Standard { sapling_anchor, orchard_anchor: Some(orchard_anchor), },
        );


        //- OUTPUTS/SENDS
        let mut memo_count = 0;
        let mut memo = EMPTY_MEMO_BYTES;
        let (mut t_send_z, mut t_recv_z, mut s_send_z, mut s_recv_z) = (0, 0, 0, 0);
        let (mut t_send_c, mut t_recv_c, mut s_send_c, mut s_recv_c) = (0, 0, 0, 0);
        for output in outputs {
            match output {
                &TxOutput::Transparent{ dst, zats } => {
                    t_send_c += 1;
                    t_send_z += zats.into_u64();
                    // TODO: more comprehensive address matching
                    let is_to_me = (dst == t_addr);
                    t_recv_c += is_to_me as usize;
                    t_recv_z += is_to_me as u64 * zats.into_u64();

                    if let Err(err) = txb.add_transparent_output(&dst, zats) {
                        println!("tx build error: {err:?}");
                        return None;
                    }
                    println!("  added transparent output: {}", zats.into_u64());
                }
                TxOutput::Orchard{ ovk, dst, zats, memo: note_memo } => {
                    s_send_c += 1;
                    s_send_z += zats;
                    // TODO: more comprehensive address matching
                    let is_to_me = (dst == orchard_addr);
                    s_recv_c += is_to_me as usize;
                    s_recv_z += is_to_me as u64 * zats;

                    memo_count += !memo_is_empty(note_memo.as_array()) as usize;
                    memo = *note_memo.as_array(); // TODO: handle multiple memos

                    if let Err(err) = txb.add_orchard_output::<zip317::FeeError>(ovk.clone(), dst.clone(), *zats, note_memo.clone()) {
                        println!("tx build error: {err:?}");
                        return None;
                    }
                    println!("  added orchard output: {}", zats);
                }
            }
        }



        //- SPENDS
        let min_spend = t_send_z + s_send_z + fee_zats.into_u64();
        let (mut t_spend_z, mut s_spend_z) = (0, 0);
        let (mut t_spend_c, mut s_spend_c) = (0, 0);
        'src_pool: for pool in opts.src_pools {
            match pool {
                // TODO: account for notes that shouldn't be spent yet
                // - not enough confirmations
                // - used in another transaction that we've built

                TxPool::Transparent => {
                    let t_pubkey = t_pubkey.expect("checked above");
                    // "greedy strategy"
                    for utxo in &account.utxos {
                        if let Err(err) = txb.add_transparent_input(t_pubkey, utxo.id.clone(), utxo.txout()) {
                            println!("tx build: transparent/UTXO spend failed: {err:?}");
                            continue;
                        }
                        t_spend_z += utxo.value.into_u64();
                        t_spend_c += 1;
                        println!("  added transparent spend: {}", utxo.value.into_u64());
                        if ((t_spend_z + s_spend_z) >= min_spend) {
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

                        for note in &shuffled_notes {
                            if note.recv_h > orchard_anchor_h { continue; }

                            let witness = match tree.witness_at_checkpoint_id(note.position, &orchard_anchor_h) {
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
                            if let Err(err) = txb.add_orchard_spend::<zip317::FeeError>(fvk.clone(), note.note, merkle_path) {
                                println!("tx build: orchard note spend failed: {err:?}");
                                continue;
                            }
                            s_spend_z += note.note.value().inner();
                            s_spend_c += 1;
                            println!("  added orchard spend: {}", note.note.value().inner());
                            if ((t_spend_z + s_spend_z) >= min_spend) {
                                break 'src_pool;
                            }
                        }
                    }
                }
            }
        }

        let spend_z = (t_spend_z + s_spend_z);
        let change = match spend_z.cmp(&min_spend) {
            std::cmp::Ordering::Less => {
                println!("tx build error: can't afford {min_spend}; only {spend_z} available from given sources");
                return None
            }, // can't afford
            std::cmp::Ordering::Equal => 0,
            std::cmp::Ordering::Greater => {
                // TODO: prefer shielded output
                let change = spend_z - min_spend;
                t_send_z += change;
                t_recv_z += change;
                if let Err(err) = txb.add_transparent_output(&t_addr, Zatoshis::from_u64(change).unwrap()) {
                    println!("tx build: failed to add change: {err:?}");
                    return None;
                };
                println!("  added transparent change: {}", change);
                change
            }
        };
        // TODO: separate change outputs from intended outputs
        // TODO: account for possible fee change with additional change

        //- TOTALS GATHERED; CHECK VALUES
        let mut parts = [
            WalletTxPart { // Transparent
                spent_note_count: t_spend_c,
                sent_note_count:  t_send_c,
                recv_note_count:  t_recv_c,
                spent_zats:       to_zats_or_dump_err("tx build", t_spend_z)?,
                sent_zats:        to_zats_or_dump_err("tx build", t_send_z)?,
                recv_zats:        to_zats_or_dump_err("tx build", t_recv_z)?,
            },
            WalletTxPart { // Shielded
                spent_note_count: s_spend_c,
                sent_note_count:  s_send_c,
                recv_note_count:  s_recv_c,
                spent_zats:       to_zats_or_dump_err("tx build", s_spend_z)?,
                sent_zats:        to_zats_or_dump_err("tx build", s_send_z)?,
                recv_zats:        to_zats_or_dump_err("tx build", s_recv_z)?,
            },
        ];

        let Some(_totals) = parts[0].checked_add(&parts[1]) else {
            println!("tx build error: total values are too large to be represented by Zatoshis");
            return None;
        };


        //-- VERY EXPENSIVE TX CREATION (PARTICULARLY IF SHIELDED OUTPUT)
        use rand_chacha::ChaCha20Rng;
        let prover = LocalTxProver::bundled();
        let rng = ChaCha20Rng::from_rng(OsRng).unwrap();
        let tx_res = match txb.build(
            &signing_set,
            sapling_esk,
            orchard_sak,
            rng,
            &prover,
            &prover,
            &zip317::FeeRule::standard(),
        ) {
            Ok(tx_res) => tx_res,
            Err(err) => {
                println!("tx build error: {err:?}");
                return None;
            }
        };

        let tx = tx_res.transaction();
        let mut tx_bytes = vec![];
        tx.write(&mut tx_bytes).unwrap();

        //-- EXPENSIVE NETWORK SEND
        // TODO: don't block, maybe return a future?
        let res = client.send_transaction(RawTransaction{ data: tx_bytes, height: 0 }).await;
        println!("******* res for {:?}: {:?}", tx.txid(), res);

        //-- COMPLETION
        if res.is_ok() {
            // TODO: complete
            let new_tx = WalletTx{
                account_id: 0,
                txid: tx.txid(),
                expiry_h: None,
                mined_h: BlockHeight::INTERNAL,
                part_flags: TxParts::FULL_TX,
                parts,
                memo_count,
                memo,
                is_coinbase: false,
                is_outside_bc: false,
                staking_action: None,
            };
            let mut insert_i = 0;
            update_insert_i(&self.txs, &mut insert_i, new_tx.mined_h);
            update_with_tx(self, tx.txid(), new_tx, &mut insert_i);
            Some(tx.txid())
        } else {
            None
        }
    }


    pub async fn shield_transparent_zats<P: Parameters>(
        &mut self, network: P, client: &mut CompactTxStreamerClient<Channel>,
        src_usk: &UnifiedSpendingKey, min_zats_to_shield: u64, orchard_tree: &OrchardShardTree,
    ) -> Option<TxId>
    {
        let tz = Timer::scope("shield_transparent_zats");
        let block_h = self.chain_tip_h.0 + 1;

        let account = &self.accounts[0];
        let keys = PreparedKeys::from_ufvk_all(&account.ufvk);
        let (t_addr, ua) = addrs_from_account(account, 0).unwrap(); // @Hack
        let orchard_addr = ua.orchard().unwrap();

        let orchard_anchor_h = self.chain_tip_h.sat_sub(5);
        let orchard_anchor = match orchard_tree.root_at_checkpoint_id(&orchard_anchor_h).expect("Infallible MemoryShardStore") {
            Some(root) => orchard::Anchor::from(root),
            None => {
                println!("tx build: couldn't get anchor at {orchard_anchor_h:?}");
                return None;
            }
        };

        //- KEYS/SIGNING
        let mut signing_set = TransparentSigningSet::new();
        let mut t_pubkey = None;
        let Some((pubkey, privkey)) = transparent_keys_from_usk(&src_usk, 0) else {
            println!("tried to send from transparent source without available transparent keys");
            return None;
        };
        signing_set.add_key(privkey);
        t_pubkey = Some(pubkey);

        let mut txb = TxBuilder::new(
            network,
            LRZBlockHeight::from_u32(block_h),
            BuildConfig::Standard { sapling_anchor: None, orchard_anchor: Some(orchard_anchor), },
        );

        //- SPENDS
        let (mut t_spend_z, mut s_spend_z) = (0, 0);
        let (mut t_spend_c, mut s_spend_c) = (0, 0);
        let t_pubkey = t_pubkey.expect("checked above");
        // "greedy strategy"

        let mut shuffled_notes = account.utxos.clone();
        shuffled_notes.shuffle(&mut OsRng);
        for utxo in &shuffled_notes {
            if let Err(err) = txb.add_transparent_input(t_pubkey, utxo.id.clone(), utxo.txout()) {
                println!("tx build: transparent/UTXO spend failed: {err:?}");
                continue;
            }
            t_spend_z += utxo.value.into_u64();
            t_spend_c += 1;
            println!("  added transparent spend: {}", utxo.value.into_u64());
            if (t_spend_z + s_spend_z) >= min_zats_to_shield {
                break;
            }
        }
        if (t_spend_z + s_spend_z) < min_zats_to_shield {
            return None;
        }

        let fee = (((t_spend_c + 2) * 5000) as u64).max(10_000);

        //- OUTPUTS/SENDS
        let mut memo_count = 0;
        let mut memo = EMPTY_MEMO_BYTES;
        let (mut t_send_z, mut t_recv_z, mut s_send_z, mut s_recv_z) = (0, 0, 0, 0);
        let (mut t_send_c, mut t_recv_c, mut s_send_c, mut s_recv_c) = (0, 0, 0, 0);

        s_send_z += (t_spend_z - fee);
        s_recv_z += (t_spend_z - fee);
        s_send_c = 1;
        s_recv_c = 1;

        let memo_bytes = MemoBytes::from_bytes("shielding notes (fn sheild_transparent_zats)".as_bytes()).unwrap();
        memo_count = 1;
        memo = *memo_bytes.as_array();

        if let Err(err) = txb.add_orchard_output::<zip317::FeeError>(account.ufvk.orchard().cloned().map(|fvk| fvk.to_ovk(orchard::keys::Scope::External)), orchard_addr.clone(), s_send_z, memo_bytes) {
            println!("tx build error: {err:?}");
            return None;
        }
        println!("  added orchard output: {}", s_send_z);

        //- TOTALS GATHERED; CHECK VALUES
        let mut parts = [
            WalletTxPart { // Transparent
                spent_note_count: t_spend_c,
                sent_note_count:  t_send_c,
                recv_note_count:  t_recv_c,
                spent_zats:       to_zats_or_dump_err("tx build", t_spend_z)?,
                sent_zats:        to_zats_or_dump_err("tx build", t_send_z)?,
                recv_zats:        to_zats_or_dump_err("tx build", t_recv_z)?,
            },
            WalletTxPart { // Shielded
                spent_note_count: s_spend_c,
                sent_note_count:  s_send_c,
                recv_note_count:  s_recv_c,
                spent_zats:       to_zats_or_dump_err("tx build", s_spend_z)?,
                sent_zats:        to_zats_or_dump_err("tx build", s_send_z)?,
                recv_zats:        to_zats_or_dump_err("tx build", s_recv_z)?,
            },
        ];

        let Some(_totals) = parts[0].checked_add(&parts[1]) else {
            println!("tx build error: total values are too large to be represented by Zatoshis");
            return None;
        };

        //-- VERY EXPENSIVE TX CREATION (PARTICULARLY IF SHIELDED OUTPUT)
        use rand_chacha::ChaCha20Rng;
        let prover = LocalTxProver::bundled();
        let rng = ChaCha20Rng::from_rng(OsRng).unwrap();
        let tx_res = match txb.build(
            &signing_set,
            &[],
            &[],
            rng,
            &prover,
            &prover,
            &zip317::FeeRule::standard(),
        ) {
            Ok(tx_res) => tx_res,
            Err(err) => {
                println!("tx build error: {err:?}");
                return None;
            }
        };

        let tx = tx_res.transaction();
        let mut tx_bytes = vec![];
        tx.write(&mut tx_bytes).unwrap();

        //-- EXPENSIVE NETWORK SEND
        // TODO: don't block, maybe return a future?
        let res = client.send_transaction(RawTransaction{ data: tx_bytes, height: 0 }).await;
        println!("******* res for {:?}: {:?}", tx.txid(), res);

        //-- COMPLETION
        if res.is_ok() {
            // TODO: complete
            let new_tx = WalletTx{
                account_id: 0,
                txid: tx.txid(),
                expiry_h: None,
                mined_h: BlockHeight::INTERNAL,
                part_flags: TxParts::FULL_TX,
                parts,
                memo_count,
                memo,
                is_coinbase: false,
                is_outside_bc: false,
                staking_action: None,
            };
            let mut insert_i = 0;
            update_insert_i(&self.txs, &mut insert_i, new_tx.mined_h);
            update_with_tx(self, tx.txid(), new_tx, &mut insert_i);
            Some(tx.txid())
        } else {
            None
        }
    }


    pub async fn send_orchard_to_orchard_zats<P: Parameters>(
        &mut self, network: P, client: &mut CompactTxStreamerClient<Channel>,
        src_usk: &UnifiedSpendingKey, exact_amount_to_send: u64, orchard_tree: &OrchardShardTree, orchard_addr: &orchard::Address
    ) -> Option<TxId>
    {
        let tz = Timer::scope("send_orchard_to_orchard_zats");
        let block_h = self.chain_tip_h.0 + 1;

        let account = &self.accounts[0];
        let keys = PreparedKeys::from_ufvk_all(&account.ufvk);

        let orchard_anchor_h = self.chain_tip_h.sat_sub(5);
        let orchard_anchor = match orchard_tree.root_at_checkpoint_id(&orchard_anchor_h).expect("Infallible MemoryShardStore") {
            Some(root) => orchard::Anchor::from(root),
            None => {
                println!("tx build: couldn't get anchor at {orchard_anchor_h:?}");
                return None;
            }
        };

        //- KEYS/SIGNING
        let mut signing_set = TransparentSigningSet::new();
        let mut t_pubkey = None;
        let Some((pubkey, privkey)) = transparent_keys_from_usk(&src_usk, 0) else {
            println!("tried to send from transparent source without available transparent keys");
            return None;
        };
        signing_set.add_key(privkey);
        t_pubkey = Some(pubkey);

        let mut txb = TxBuilder::new(
            network,
            LRZBlockHeight::from_u32(block_h),
            BuildConfig::Standard { sapling_anchor: None, orchard_anchor: Some(orchard_anchor), },
        );

        //- SPENDS
        let (mut t_spend_z, mut s_spend_z) = (0, 0);
        let (mut t_spend_c, mut s_spend_c) = (0, 0usize);
        let t_pubkey = t_pubkey.expect("checked above");
        // "greedy strategy"

        let mut rolling_fee_estimate: u64 = 5000 * (s_spend_c as u64).max(2);
        rolling_fee_estimate = rolling_fee_estimate.max(10_000);

        let mut shuffled_notes = account.unspent_orchard_notes.clone();
        shuffled_notes.shuffle(&mut OsRng);
        for note in &shuffled_notes {
            if note.recv_h > orchard_anchor_h { continue; }
            let witness = match orchard_tree.witness_at_checkpoint_id(note.position, &orchard_anchor_h) {
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
            if let Err(err) = txb.add_orchard_spend::<zip317::FeeError>(keys.orchard_fvk.clone().unwrap(), note.note, merkle_path) {
                println!("tx build: orchard note spend failed: {err:?}");
                continue;
            }
            s_spend_z += note.note.value().inner();
            s_spend_c += 1;
            println!("  added orchard spend: {}", note.note.value().inner());

            rolling_fee_estimate = 5000 * (s_spend_c as u64).max(2);
            rolling_fee_estimate = rolling_fee_estimate.max(10_000);
            if s_spend_z >= exact_amount_to_send + rolling_fee_estimate {
                break;
            }
        }

        if s_spend_z < exact_amount_to_send + rolling_fee_estimate {
            println!("tx build error: not enough unspent orchard notes, got {} zats needed {}", s_spend_z, exact_amount_to_send + rolling_fee_estimate);
            return None;
        }

        //- OUTPUTS/SENDS
        let mut memo_count = 0;
        let mut memo = EMPTY_MEMO_BYTES;
        let (mut t_send_z, mut t_recv_z, mut s_send_z, mut s_recv_z) = (0, 0, 0, 0);
        let (mut t_send_c, mut t_recv_c, mut s_send_c, mut s_recv_c) = (0, 0, 0, 0);

        s_send_z = s_spend_z - rolling_fee_estimate;
        s_send_c = 2;

        let memo_bytes = MemoBytes::from_bytes("orchard send (fn send_orchard_to_orchard_zats)".as_bytes()).unwrap();
        memo_count = 1;
        memo = *memo_bytes.as_array();

        if let Err(err) = txb.add_orchard_output::<zip317::FeeError>(account.ufvk.orchard().cloned().map(|fvk| fvk.to_ovk(orchard::keys::Scope::External)), orchard_addr.clone(), exact_amount_to_send, memo_bytes) {
            println!("tx build error: {err:?}");
            return None;
        }
        println!("  added orchard output: {}", exact_amount_to_send);

        let (my_t_addr, my_ua) = addrs_from_account(&account, 0).unwrap();

        // Note(Sam): Omg wow this is actually kind of gnarly. We get two grace outputs always.
        s_recv_z = s_send_z - exact_amount_to_send;
        s_recv_c = 1;
        if let Err(err) = txb.add_orchard_output::<zip317::FeeError>(account.ufvk.orchard().cloned().map(|fvk| fvk.to_ovk(orchard::keys::Scope::External)), my_ua.orchard().unwrap().clone(), s_recv_z, MemoBytes::from_bytes(&EMPTY_MEMO_BYTES).unwrap()) {
            println!("tx build error: {err:?}");
            return None;
        }
        println!("  added orchard change: {}", s_send_z - exact_amount_to_send);

        //- TOTALS GATHERED; CHECK VALUES
        let mut parts = [
            WalletTxPart { // Transparent
                spent_note_count: t_spend_c,
                sent_note_count:  t_send_c,
                recv_note_count:  t_recv_c,
                spent_zats:       to_zats_or_dump_err("tx build", t_spend_z)?,
                sent_zats:        to_zats_or_dump_err("tx build", t_send_z)?,
                recv_zats:        to_zats_or_dump_err("tx build", t_recv_z)?,
            },
            WalletTxPart { // Shielded
                spent_note_count: s_spend_c,
                sent_note_count:  s_send_c,
                recv_note_count:  s_recv_c,
                spent_zats:       to_zats_or_dump_err("tx build", s_spend_z)?,
                sent_zats:        to_zats_or_dump_err("tx build", s_send_z)?,
                recv_zats:        to_zats_or_dump_err("tx build", s_recv_z)?,
            },
        ];

        let Some(_totals) = parts[0].checked_add(&parts[1]) else {
            println!("tx build error: total values are too large to be represented by Zatoshis");
            return None;
        };

        //-- VERY EXPENSIVE TX CREATION (PARTICULARLY IF SHIELDED OUTPUT)
        use rand_chacha::ChaCha20Rng;
        let prover = LocalTxProver::bundled();
        let rng = ChaCha20Rng::from_rng(OsRng).unwrap();
        let tx_res = match txb.build(
            &signing_set,
            &[],
            &[src_usk.orchard().into()],
            rng,
            &prover,
            &prover,
            &zip317::FeeRule::standard(),
        ) {
            Ok(tx_res) => tx_res,
            Err(err) => {
                println!("tx build error: {err:?}");
                return None;
            }
        };

        let tx = tx_res.transaction();
        let mut tx_bytes = vec![];
        tx.write(&mut tx_bytes).unwrap();

        //-- EXPENSIVE NETWORK SEND
        // TODO: don't block, maybe return a future?
        let res = client.send_transaction(RawTransaction{ data: tx_bytes, height: 0 }).await;
        println!("******* res for {:?}: {:?}", tx.txid(), res);

        //-- COMPLETION
        if res.is_ok() {
            // TODO: complete
            let new_tx = WalletTx{
                account_id: 0,
                txid: tx.txid(),
                expiry_h: None,
                mined_h: BlockHeight::INTERNAL,
                part_flags: TxParts::FULL_TX,
                parts,
                memo_count,
                memo,
                is_coinbase: false,
                is_outside_bc: false,
                staking_action: None,
            };
            let mut insert_i = 0;
            update_insert_i(&self.txs, &mut insert_i, new_tx.mined_h);
            update_with_tx(self, tx.txid(), new_tx, &mut insert_i);
            Some(tx.txid())
        } else {
            None
        }
    }


    pub async fn stake_orchard_to_finalizer<P: Parameters>(
        &mut self, network: P, client: &mut CompactTxStreamerClient<Channel>,
        src_usk: &UnifiedSpendingKey, exact_amount_to_send: u64, orchard_tree: &OrchardShardTree, target_finalizer: &[u8; 32],
    ) -> Option<TxId>
    {
        let tz = Timer::scope("stake_orchard_to_finalizer");
        let block_h = self.chain_tip_h.0 + 1;

        let account = &self.accounts[0];
        let keys = PreparedKeys::from_ufvk_all(&account.ufvk);

        let orchard_anchor_h = self.chain_tip_h.sat_sub(5);
        let orchard_anchor = match orchard_tree.root_at_checkpoint_id(&orchard_anchor_h).expect("Infallible MemoryShardStore") {
            Some(root) => orchard::Anchor::from(root),
            None => {
                println!("tx build: couldn't get anchor at {orchard_anchor_h:?}");
                return None;
            }
        };

        //- KEYS/SIGNING
        let mut signing_set = TransparentSigningSet::new();
        let mut t_pubkey = None;
        let Some((pubkey, privkey)) = transparent_keys_from_usk(&src_usk, 0) else {
            println!("tried to send from transparent source without available transparent keys");
            return None;
        };
        signing_set.add_key(privkey);
        t_pubkey = Some(pubkey);

        let mut txb = TxBuilder::new(
            network,
            LRZBlockHeight::from_u32(block_h),
            BuildConfig::Standard { sapling_anchor: None, orchard_anchor: Some(orchard_anchor), },
        );

        //- SPENDS
        let (mut t_spend_z, mut s_spend_z) = (0, 0);
        let (mut t_spend_c, mut s_spend_c) = (0, 0usize);
        let t_pubkey = t_pubkey.expect("checked above");
        // "greedy strategy"

        let mut rolling_fee_estimate: u64 = 5000 * (s_spend_c as u64).max(2);
        rolling_fee_estimate = rolling_fee_estimate.max(10_000);

        let mut shuffled_notes = account.unspent_orchard_notes.clone();
        shuffled_notes.shuffle(&mut OsRng);
        for note in &shuffled_notes {
            if note.recv_h > orchard_anchor_h { continue; }
            let witness = match orchard_tree.witness_at_checkpoint_id(note.position, &orchard_anchor_h) {
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
            if let Err(err) = txb.add_orchard_spend::<zip317::FeeError>(keys.orchard_fvk.clone().unwrap(), note.note, merkle_path) {
                println!("tx build: orchard note spend failed: {err:?}");
                continue;
            }
            s_spend_z += note.note.value().inner();
            s_spend_c += 1;
            println!("  added orchard spend: {}", note.note.value().inner());

            rolling_fee_estimate = 5000 * (s_spend_c as u64).max(2);
            rolling_fee_estimate = rolling_fee_estimate.max(10_000);
            if s_spend_z >= exact_amount_to_send + rolling_fee_estimate {
                break;
            }
        }

        if s_spend_z < exact_amount_to_send + rolling_fee_estimate {
            println!("tx build error: not enough unspent orchard notes, got {} zats needed {}", s_spend_z, exact_amount_to_send + rolling_fee_estimate);
            return None;
        }

        //- OUTPUTS/SENDS
        let mut memo_count = 0;
        let mut memo = EMPTY_MEMO_BYTES;
        let (mut t_send_z, mut t_recv_z, mut s_send_z, mut s_recv_z) = (0, 0, 0, 0);
        let (mut t_send_c, mut t_recv_c, mut s_send_c, mut s_recv_c) = (0, 0, 0, 0);

        s_send_z = s_spend_z - rolling_fee_estimate;
        s_send_c = 2;


        use rand::RngCore;
        let mut pretend_pub_key = [0u8; 32];
        rand::rngs::OsRng.fill_bytes(&mut pretend_pub_key);

        if let Err(err) = txb.put_staking_action(StakingAction_CreateNewDelegationBond { amount_zats: exact_amount_to_send, unique_pubkey: pretend_pub_key, challenge: [0u8; 32], target_finalizer: *target_finalizer, signature: [0u8; 64] }.to_union()) {
            println!("tx build error: {err:?}");
            return None;
        }
        println!("  TODO added stake output: {}", exact_amount_to_send);

        let (my_t_addr, my_ua) = addrs_from_account(&account, 0).unwrap();

        // Change is free.
        s_recv_z = s_send_z - exact_amount_to_send;
        s_recv_c = 1;
        if let Err(err) = txb.add_orchard_output::<zip317::FeeError>(account.ufvk.orchard().cloned().map(|fvk| fvk.to_ovk(orchard::keys::Scope::External)), my_ua.orchard().unwrap().clone(), s_recv_z, MemoBytes::from_bytes(&EMPTY_MEMO_BYTES).unwrap()) {
            println!("tx build error: {err:?}");
            return None;
        }
        println!("  added orchard change: {}", exact_amount_to_send);

        //- TOTALS GATHERED; CHECK VALUES
        let mut parts = [
            WalletTxPart { // Transparent
                spent_note_count: t_spend_c,
                sent_note_count:  t_send_c,
                recv_note_count:  t_recv_c,
                spent_zats:       to_zats_or_dump_err("tx build", t_spend_z)?,
                sent_zats:        to_zats_or_dump_err("tx build", t_send_z)?,
                recv_zats:        to_zats_or_dump_err("tx build", t_recv_z)?,
            },
            WalletTxPart { // Shielded
                spent_note_count: s_spend_c,
                sent_note_count:  s_send_c,
                recv_note_count:  s_recv_c,
                spent_zats:       to_zats_or_dump_err("tx build", s_spend_z)?,
                sent_zats:        to_zats_or_dump_err("tx build", s_send_z)?,
                recv_zats:        to_zats_or_dump_err("tx build", s_recv_z)?,
            },
        ];

        let Some(_totals) = parts[0].checked_add(&parts[1]) else {
            println!("tx build error: total values are too large to be represented by Zatoshis");
            return None;
        };

        //-- VERY EXPENSIVE TX CREATION (PARTICULARLY IF SHIELDED OUTPUT)
        use rand_chacha::ChaCha20Rng;
        let prover = LocalTxProver::bundled();
        let rng = ChaCha20Rng::from_rng(OsRng).unwrap();
        let tx_res = match txb.build(
            &signing_set,
            &[],
            &[src_usk.orchard().into()],
            rng,
            &prover,
            &prover,
            &zip317::FeeRule::standard(),
        ) {
            Ok(tx_res) => tx_res,
            Err(err) => {
                println!("tx build error: {err:?}");
                return None;
            }
        };

        let tx = tx_res.transaction();
        let mut tx_bytes = vec![];
        tx.write(&mut tx_bytes).unwrap();

        //-- EXPENSIVE NETWORK SEND
        // TODO: don't block, maybe return a future?
        let res = client.send_transaction(RawTransaction{ data: tx_bytes, height: 0 }).await;
        println!("******* res for {:?}: {:?}", tx.txid(), res);

        //-- COMPLETION
        if res.is_ok() {
            // TODO: complete
            let new_tx = WalletTx{
                account_id: 0,
                txid: tx.txid(),
                expiry_h: None,
                mined_h: BlockHeight::INTERNAL,
                part_flags: TxParts::FULL_TX,
                parts,
                memo_count,
                memo,
                is_coinbase: false,
                is_outside_bc: false,
                staking_action: None,
            };
            let mut insert_i = 0;
            update_insert_i(&self.txs, &mut insert_i, new_tx.mined_h);
            update_with_tx(self, tx.txid(), new_tx, &mut insert_i);
            Some(tx.txid())
        } else {
            None
        }
    }


    pub fn audit_txs(&self) {
        for tx in &self.txs {
            // if tx.parts[0].spent_note_count > 0 &&
            //     tx.parts[1].sent_note_count == 0
            // {
            //     println!("invalid shield found: {tx:?}");
            //     println!("all txs: {:?}", NL(&self.txs[..]));
            // }
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
    pub fn hash_at_h(&self, h: u64) -> Option<[u8;32]> {
        if h < self.next_tip_h {
            Some(self.hashes[h as usize])
        } else {
            None
        }
    }
    pub fn tip_hash(&self) -> [u8;32] {
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
    debug_assert!(i == 0 || notes[i-1].recv_h <= note.recv_h, "{} <= {}", notes[i-1].recv_h, note.recv_h);
    notes.insert(i, note);
}
fn orchard_spent_h_insert(notes: &mut Vec<OrchardNote>, note: OrchardNote) {
    let mut i = notes.len(); // common case append
    if let Some(last) = notes.last() {
        if last.spent_h > note.spent_h {
            i = notes.partition_point(|n| n.spent_h <= note.spent_h);
        }
    }
    debug_assert!(i == 0 || notes[i-1].spent_h <= note.spent_h, "{} <= {}", notes[i-1].spent_h, note.spent_h);
    notes.insert(i, note);
}
fn txo_recv_h_insert(notes: &mut Vec<Txo>, note: Txo) {
    let mut i = notes.len(); // common case append
    if let Some(last) = notes.last() {
        if last.recv_h > note.recv_h {
            i = notes.partition_point(|n| n.recv_h <= note.recv_h);
        }
    }
    debug_assert!(i == 0 || notes[i-1].recv_h <= note.recv_h, "{} <= {}", notes[i-1].recv_h, note.recv_h);
    notes.insert(i, note);
}
fn txo_spent_h_insert(notes: &mut Vec<Txo>, note: Txo) {
    let mut i = notes.len(); // common case append
    if let Some(last) = notes.last() {
        if last.spent_h > note.spent_h {
            i = notes.partition_point(|n| n.spent_h <= note.spent_h);
        }
    }
    debug_assert!(i == 0 || notes[i-1].spent_h <= note.spent_h, "{} <= {}", notes[i-1].spent_h, note.spent_h);
    notes.insert(i, note);
}

/// GET NOTE/TX INDEXES WITH KNOWN HEIGHTS IN SORTED SLICES
fn orchard_recv_h_position(notes: &[OrchardNote], block_h: BlockHeight, nf: &orchard::note::Nullifier) -> Option<usize> {
    let mut i = notes.partition_point(|txo| txo.recv_h < block_h);
    while i < notes.len() && notes[i].recv_h == block_h {
        if &notes[i].nf == nf {
            return Some(i);
        }
        i += 1;
    }
    None
}
fn orchard_spent_h_position(notes: &[OrchardNote], block_h: BlockHeight, nf: &orchard::note::Nullifier) -> Option<usize> {
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
fn tx_mined_h_position(txs: &[WalletTx], block_h: BlockHeight, txid: &TxId) -> Option<usize> {
    let mut i = txs.partition_point(|tx| tx.mined_h < block_h);
    while i < txs.len() && txs[i].mined_h == block_h {
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
        tx_mined_h_position(&wallet.txs, tx_h, txid)
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
        println!("transparent tx's height can't be represented in 32 bits: {}", height);
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
fn handle_orchard_action(wallet: &mut ManualWallet, account_i: usize, keys: &PreparedKeys, position: incrementalmerkletree::Position, block_h: BlockHeight, txid: &TxId, nf: &orchard::note::Nullifier, recv_note_addr: Option<(orchard::note::Note, orchard::Address)>, send_note_addr_memo: Option<(orchard::note::Note, orchard::Address, [u8; 512])>) -> Option<WalletTxPart> {
    let account = &mut wallet.accounts[account_i];

    //- HANDLE SPENT NOTES
    let (mut s_send_z, mut s_spend_z, mut s_recv_z) = (0, 0, 0);
    let (mut s_send_c, mut s_spend_c, mut s_recv_c) = (0, 0, 0);
    // TODO: map/index acceleration
    // NOTE: action.nullifier() is like prevout, it's the spent id (if a recv action)
    for (note_i, note) in account.unspent_orchard_notes.iter().enumerate() {
        if note.nf == *nf {
            // this action is a spend by us with this note/nullifier: move it to spent
            let spent_note = account.unspent_orchard_notes.remove(note_i);
            s_spend_c = 1;
            s_spend_z = spent_note.note.value().inner();
            orchard_spent_h_insert(&mut account.spent_orchard_notes, OrchardNote { spent_h: block_h, ..spent_note });
            break;
        }
    }
    if s_spend_c == 0 {
        for note in &account.spent_orchard_notes {
            if note.nf == *nf {
                s_spend_c = 1;
                s_spend_z = note.note.value().inner();
                break;
            }
        }
    }

    //- HANDLE KNOWN-SENT
    if let Some((note, _addr, _memo)) = send_note_addr_memo {
        s_send_c = 1;
        s_send_z = note.value().inner();
    }

    //- PUSH NEW RECEIVED/UNSPENT NOTES
    if let Some((note, _recipient)) = recv_note_addr {
        s_recv_c += 1;
        s_recv_z += note.value().inner();
        // if s_spend_c > 0 && s_send_c  {
        //     s_send_c += 1;
        //     s_send_z += note.value().inner();
        // }
        // NOTE: s_send_c/s_send_z equivalent handled inside update_with_tx

        let orchard_note = OrchardNote{
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
        // println!("got new note at {:?}, tree pos={:02} {:?}", block_h, u64::from(orchard_note.witness.witnessed_position()), orchard_note.witness.root());

        let txid_h = if let Some(&txid_h) = wallet.tx_h_map.get(txid) {
            txid_h
        } else {
            block_h
        };

        // TODO: can we just check if we've seen the tx && tx.is_outside_bc == false
        let have_seen = if let Some(i) = orchard_recv_h_position(&account.recv_orchard_notes, txid_h, &orchard_note.nf) {
            account.recv_orchard_notes[i].monotonically_update(orchard_note);
            true
        } else {
            orchard_recv_h_insert(&mut account.recv_orchard_notes, orchard_note.clone());
            false
        };

        if let Some(i) = orchard_recv_h_position(&account.unspent_orchard_notes, txid_h, &orchard_note.nf) {
            account.unspent_orchard_notes[i].monotonically_update(orchard_note);
        } else if !have_seen {
            orchard_recv_h_insert(&mut account.unspent_orchard_notes, orchard_note);
        }
    }

    match (Zatoshis::from_u64(s_spend_z), Zatoshis::from_u64(s_send_z), Zatoshis::from_u64(s_recv_z)) {
        (Ok(spent_zats), Ok(sent_zats), Ok(recv_zats)) => Some(WalletTxPart {
            spent_zats,
            sent_zats,
            recv_zats,
            spent_note_count: s_spend_c,
            sent_note_count: s_send_c,
            recv_note_count: s_recv_c,
        }),

        (spent, sent, recv) => {
            println!("couldn't convert all to Zats: ({spent:?}, {sent:?}, {recv:?})");
            return None;
        }
    }
}

fn read_full_tx(wallet: &mut ManualWallet, account_i: usize, keys: &PreparedKeys, block_h: BlockHeight, tx: &Transaction, insert_i: &mut usize, is_outside_bc: bool) -> Option<()> {
    // TODO: we probably want to early-out if our existing tx data is complete
    // (after checking that this doesn't get modified)
    update_insert_i(&wallet.txs, insert_i, block_h);

    let mut expiry_h = Some(BlockHeight::from(tx.expiry_height()));
    if expiry_h.unwrap().0 == 0 {
        expiry_h = None;
    }

    let txid = tx.txid();
    // println!("at h: {block_h}, transparent tx {txid} contains {} orchard actions", tx.orchard_bundle().map_or(0, |b| b.actions().len()));

    // NOTE: these are only from *our* perspective
    let mut total_received = 0;
    let mut total_spent = 0;
    let (mut t_send_z, mut t_spend_z, mut t_recv_z) = (0, 0, 0);
    let (mut t_send_c, mut t_spend_c, mut t_recv_c) = (0, 0, 0);
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
            let mut input_i = 0;
            for input in &t_bundle.vin {
                // println!("input {input_i} {input:?}");
                input_i += 1;

                if let Some(&prevout_txid_h) = wallet.tx_h_map.get(input.prevout.txid()) {
                    if let Some(utxo_i) = txo_recv_h_position(&account.utxos, prevout_txid_h, &input.prevout) {
                        let utxo = account.utxos.remove(utxo_i);
                        let stxo = Txo { spent_h: block_h, ..utxo };
                        if let Some(last_stxo) = account.stxos.last() {
                            if last_stxo.spent_h > stxo.spent_h {
                                println!("ERROR: out of sequence spent UTXO: {} > {}", last_stxo.spent_h, stxo.spent_h);
                            }
                        }
                        t_spend_c += 1;
                        t_spend_z += stxo.value.into_u64();

                        if let Some(last_stxo) = account.stxos.last() {
                            debug_assert!(last_stxo.spent_h <= stxo.spent_h, "{} <= {}", last_stxo.spent_h, stxo.spent_h);
                        }
                        account.stxos.push(stxo);
                    } else if let Some(txo_i) = txo_recv_h_position(&account.recv_txos, prevout_txid_h, &input.prevout) {
                        // NOTE: we need to use our own tracking of the TXO as otherwise we don't know the value
                        t_spend_c += 1;
                        t_spend_z += account.recv_txos[txo_i].value.into_u64();
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
            if t_spend_c > 0 {
                // we spent money in this TX, so we must be responsible for the sends as well
                t_send_c += 1;
                t_send_z += value.into_u64();
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
                    t_recv_c += 1;
                    t_recv_z += value.into_u64();

                    let txid_h = if let Some(&txid_h) = wallet.tx_h_map.get(&txid) {
                        txid_h
                    } else {
                        block_h
                    };
                    if let Some(utxo_i) = txo_recv_h_position(&account.utxos, txid_h, &utxo.id) {
                        if account.utxos[utxo_i] != utxo {
                            println!("ERROR: UTXO mismatch: {:?} vs {:?}", account.utxos[utxo_i], &utxo);
                        }
                    } else if txo_recv_h_position(&account.recv_txos, txid_h, &utxo.id).is_none() {
                        // TODO: can we just check if we've seen the tx && tx.2 == false
                        if let Some(last_txo) = account.recv_txos.last() {
                            debug_assert!(last_txo.recv_h <= utxo.recv_h, "{} <= {}", last_txo.recv_h, utxo.recv_h);
                        }
                        account.recv_txos.push(utxo.clone());

                        if let Some(last_utxo) = account.utxos.last() {
                            debug_assert!(last_utxo.recv_h <= utxo.recv_h, "{} <= {}", last_utxo.recv_h, utxo.recv_h);
                        }
                        account.utxos.push(utxo);
                    }
                }
            }
        }
    }

    let mut memo_count = 0;
    let mut memo = EMPTY_MEMO_BYTES;
    let mut shielded_part = WalletTxPart::ZERO;

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
                try_output_recovery_with_ovk(&domain, ovk, action, action.cv_net(), &action.encrypted_note().out_ciphertext)
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

            let Some(part) = handle_orchard_action(wallet, account_i, keys, unknown_tree_position(), block_h, &txid, action.nullifier(), recv_note_addr, send_res) else {
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
    }

    let parts = [
        WalletTxPart {
            spent_zats: to_zats_or_dump_err("t tx receive", t_spend_z)?,
            sent_zats: to_zats_or_dump_err("t tx receive", t_send_z)?,
            recv_zats: to_zats_or_dump_err("t tx receive", t_recv_z)?,
            spent_note_count: t_spend_c,
            sent_note_count: t_send_c,
            recv_note_count: t_recv_c,
        },
        shielded_part,
    ];

    let new_tx = WalletTx {
        account_id: 0,
        txid,
        expiry_h,
        mined_h: block_h,
        part_flags: TxParts::FULL_TX,
        parts,
        memo_count,
        memo,
        is_coinbase,
        is_outside_bc,
        staking_action: tx.staking_action(),
    };

    update_with_tx(wallet, new_tx.txid, new_tx, insert_i);
    Some(())
}

type OrchardTree = incrementalmerkletree::frontier::CommitmentTree<orchard::tree::MerkleHashOrchard, { orchard::NOTE_COMMITMENT_TREE_DEPTH as u8 }>;
type OrchardFrontier = incrementalmerkletree::frontier::Frontier<orchard::tree::MerkleHashOrchard, { orchard::NOTE_COMMITMENT_TREE_DEPTH as u8 }>;
type OrchardWitness = incrementalmerkletree::witness::IncrementalWitness<orchard::tree::MerkleHashOrchard, { orchard::NOTE_COMMITMENT_TREE_DEPTH as u8 }>;
const SHARD_HEIGHT: u8 = 16; // default => 65536 leaves per shard
type OrchardShardTree = shardtree::ShardTree::<
    shardtree::store::memory::MemoryShardStore::<
        orchard::tree::MerkleHashOrchard,
        // shardtree::Node<orchard::tree::MerkleHashOrchard, (), ()>,
        BlockHeight
    >,
    { orchard::NOTE_COMMITMENT_TREE_DEPTH as u8 },
    SHARD_HEIGHT
>;

fn shard_tree_size(tree: &OrchardShardTree) -> u64 {
    tree.max_leaf_position(None)
        .expect("Infallible Memory Store")
        .map_or(0, |pos| u64::from(pos)+1)
}

fn shard_tree_root(tree: &OrchardShardTree) -> orchard::tree::MerkleHashOrchard {
    tree.root_at_checkpoint_depth(None)
        .expect("Infallible Memory Store")
        .unwrap()
}

/// NOTE: this *must* only be called in sequential order without gaps (including after reorg/truncate)
fn read_compact_tx(wallet: &mut ManualWallet, account_i: usize, keys: &PreparedKeys, block_h: BlockHeight, tx: &CompactTx, insert_i: &mut usize, orchard_tree: &mut OrchardShardTree) -> (TxId, bool/*ours*/, bool/*ok*/) {
    let txid = TxId::from_bytes(<[u8;32]>::try_from(&tx.hash[..]).expect("successfully converted above"));

    let mut shielded_part = WalletTxPart::ZERO;

    for orchard_action in &tx.actions {
        let action = match OrchardCompactAction::try_from(orchard_action) {
            Ok(v) => v,
            Err(err) => {
                // TODO: we can't keep position updated if we fail here
                // TODO: should we fail validation for the entire block above if we can't do this?
                println!("couldn't convert CompactOrchardAction to orchard::CompactAction: {err:?}");
                continue;
            }
        };
        let domain = OrchardDomain::for_compact_action(&action);

        let note_addr: Option<(orchard::note::Note, orchard::Address)> = if let Some(ivk) = &keys.orchard_ivk {
             try_compact_note_decryption(&domain, ivk, &action)
        } else {
            None
        };
        let nf: orchard::note::Nullifier = action.nullifier();
        let cmx: orchard::note::ExtractedNoteCommitment = action.cmx();

        //- GLOBAL-VIEW UPDATES
        // NOTE: we don't care to mark our sent(-only) actions
        let retention = if note_addr.is_some() {
            incrementalmerkletree::Retention::Marked
        } else {
            incrementalmerkletree::Retention::Ephemeral
        };
        // TODO: batch_insert
        // Some kind of problem with batch insert. TODO for later / Sam
        // let position = orchard_tree.max_leaf_position(None).unwrap().unwrap_or(incrementalmerkletree::Position::from(0));
        // let res = orchard_tree.batch_insert(position, append_iter).expect("Infallible Memory Store");
        // println!("****** orchard_tree.batch_insert result {:?}", res);
        orchard_tree.append(orchard::tree::MerkleHashOrchard::from_cmx(&cmx), retention).expect("Infallible Memory Store");
        println!("orchard root at {:?} tree size={:02} {:?}", block_h, shard_tree_size(orchard_tree), shard_tree_root(orchard_tree));

        let position = orchard_tree.max_leaf_position(None).expect("Infallible Memory Store").expect("just appended");

        let Some(part) = handle_orchard_action(wallet, account_i, keys, position, block_h, &txid, &nf, note_addr, None) else {
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

    if (shielded_part.spent_note_count | shielded_part.sent_note_count | shielded_part.recv_note_count) != 0 {
        let new_tx = WalletTx {
            account_id: account_i,
            txid,
            expiry_h: None, // TODO
            mined_h: block_h,
            part_flags: TxParts::SHIELDED_RECV,
            parts: [ WalletTxPart::ZERO, shielded_part ],
            memo_count: 0,
            memo: EMPTY_MEMO_BYTES,
            is_coinbase: false,
            is_outside_bc: false,
            staking_action: None,
        };

        update_with_tx(wallet, txid, new_tx, insert_i);
        (txid, true, true)
    } else {
        (txid, false, true)
    }
}

const JUDAHS_NETWORK_TESTS: bool = true;

#[derive(Debug, Default)]
struct IndirectState {
    pub network_handle: *mut std::ffi::c_void,
}

#[allow(unsafe_code)]
impl IndirectState {
    fn connect(
        &mut self,
        address: *const u8,
        address_len: usize,
        connect_cb: unsafe extern "C" fn(*const u8, usize, *mut std::ffi::c_void) -> *mut std::ffi::c_void,
        userdata: *mut std::ffi::c_void)
    {
        unsafe { self.network_handle = connect_cb(address, address_len, userdata); }
    }

}


pub async fn wallet_main(wallet_state: Arc<Mutex<WalletState>>) {
    fn stuff_from_seed_phrase<P: Parameters + 'static>(params:P, phrase: &str) -> (
        SecretVec<u8>,
        UnifiedSpendingKey,
    ) {
        use secrecy::ExposeSecret;

        let mnemonic = bip39::Mnemonic::parse(phrase).unwrap();
        let bip39_passphrase = ""; // optional
        let seed64 = mnemonic.to_seed(bip39_passphrase);
        let seed = SecretVec::new(seed64[..32].to_vec());
        let seed_fp = zip32::fingerprint::SeedFingerprint::from_seed(seed.expose_secret()).unwrap();
        let account_id = zip32::AccountId::try_from(0).unwrap();

        let usk = UnifiedSpendingKey::from_seed(&params, seed.expose_secret(), account_id).unwrap();
        let birthday = &AccountBirthday::from_parts(
            ChainState::empty(LRZBlockHeight::from_u32(0), zcash_primitives::block::BlockHash([0; 32])),
            None,
        );

        (seed, usk)
    }

    fn wallet_from_stuff<P: Parameters + 'static>(params: P, name: &'static str, seed: SecretVec<u8>) -> (ManualWallet, ManualAccount) {
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
        };

        (wallet, account)
    }

    let addrs_from_wallet = |wallet: &ManualWallet| -> Option<(TransparentAddress, UnifiedAddress)> {
        let Some(account) = wallet.accounts.first() else { return None; };
        addrs_from_account(account, 0)
    };

    fn get_transaction_history(wallet: &ManualWallet) -> Result<Vec<WalletTx>, Infallible> {
        Ok(wallet.txs.clone())
    }

    async fn get_received_memos_and_actions<P: zcash_protocol::consensus::Parameters>(client: &mut CompactTxStreamerClient<Channel>, wallet: &ManualWallet, params: P, history: &[WalletTx])
        -> Option<(HashMap<TxId, (Option<StakingAction>, Vec<String>)>, HashMap<TxId, (Option<StakingAction>, Vec<String>)>)> {
        fn try_get_orchard_memos(tx: &TransactionData<zcash_primitives::transaction::Authorized>, ivk: &orchard::keys::PreparedIncomingViewingKey) -> Vec<String> {
            let mut memos = Vec::new();
            let Some(bundle) = tx.orchard_bundle() else { return memos; };

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
        fn try_get_orchard_sent_memos(tx: &TransactionData<zcash_primitives::transaction::Authorized>, ovk: &orchard::keys::OutgoingViewingKey) -> Vec<String> {
            // TODO: this is primarily for syncing txs that our wallet didn't observe sending; we
            // can optimize ones we sent directly
            let mut memos = Vec::new();
            let Some(bundle) = tx.orchard_bundle() else { return memos; };

            for action in bundle.actions() {
                let domain = orchard::note_encryption::OrchardDomain::for_action(action);
                if let Some((_, _, memo)) = try_output_recovery_with_ovk(&domain, ovk, action, action.cv_net(), &action.encrypted_note().out_ciphertext) {
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
        let ufvks: Vec<UnifiedFullViewingKey> = wallet.accounts
            .iter()
            .map(|acc| acc.ufvk.clone())
            .collect();

        for txid in &txids {
            let filter = TxFilter{ hash: txid.as_ref().to_vec(), ..Default::default() };
            let Ok(rawtx) = client.get_transaction(filter).await else { continue; };
            let rawtx = rawtx.into_inner();

            let block_h = LRZBlockHeight::from_u32(rawtx.height as u32);
            let Ok(tx) = Transaction::read(&*rawtx.data, BranchId::for_height(&params, block_h)) else {
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
                let possible_orchard_ivk = if let Some(orchard_ivk) = uivk.orchard() { Some(orchard_ivk.prepare()) } else { None };
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
                let Some(ufvk_orchard) = ufvk.orchard() else { continue; };
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

    let (
        miner_wallet_init,
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
        let (miner_wallet, miner_account) = wallet_from_stuff(network, "miner", Secret::new(seed.expose_secret().clone()));

        let (miner_t_addr, miner_ua) = addrs_from_account(&miner_account, 0).unwrap();
        let miner_t_addr_str = miner_t_addr.encode(network);
        let (miner_pubkey, miner_privkey) = transparent_keys_from_usk(&miner_usk, 0).unwrap();
        (miner_wallet, miner_account, seed, miner_usk, miner_pubkey, miner_privkey, miner_t_addr, miner_ua, HashMap::<TxId, (Option<StakingAction>, Vec<String>)>::new(), HashMap::<TxId, (Option<StakingAction>, Vec<String>)>::new())
    };

    let (
        user_wallet_init,
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
        let mnemonic = bip39::Mnemonic::from_entropy_in(bip39::Language::English, &global_seed).unwrap();
        let phrase = mnemonic.words().map(|s| s.to_string()).collect::<Vec<String>>().join(" ");

        let (seed, user_usk) = stuff_from_seed_phrase(network, &phrase);
        let (user_wallet, user_account) = wallet_from_stuff(network, "user", Secret::new(seed.expose_secret().clone()));
        let (user_t_addr, user_ua) = addrs_from_account(&user_account, 0).unwrap();
        let user_t_addr_str = user_t_addr.encode(network);
        let (user_pubkey, user_privkey) = transparent_keys_from_usk(&user_usk, 0).unwrap();

        // let user_t_addr1 = user_t_recs.into_iter().filter(|(addr, _)| addr == &user_t_addr).next().unwrap().0;
        // NOTE: the default isn't the same as below, but I think this is because it forces a diversifier index
        // println!("User wallet: {}/{:?}", user_t_addr_str, user_t_addr1.encode(network));

        (user_wallet, user_account, seed, user_usk, user_pubkey, user_privkey, user_t_addr, user_ua, HashMap::<TxId, (Option<StakingAction>, Vec<String>)>::new())
    };

    let user_ua_str = user_ua.encode(network);
    println!("*************************");
    println!("MINER WALLET T-ADDRESS: {}", miner_ua.encode(network));
    println!("MINER WALLET ADDRESS:   {}", miner_t_address.encode(network));
    println!("USER WALLET T-ADDRESS:  {}", user_t_address.encode(network));
    println!("USER WALLET ADDRESS:    {}", user_ua_str);
    println!("*************************");

    wallet_state.lock().unwrap().user_recv_ua = user_ua_str;


    println!("waiting for zaino to be ready...");
    wait_for_zainod().await;
    //////////////////////////////////////////////////////////////////////////////////

    // TODO: use tenderlink types & printing routines
    let mut zaino_port = 0;
    loop {
        zaino_port = *wallet_main_zaino_port.lock().unwrap();
        if zaino_port != 0 { break; }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }

    let mut indirect_state = IndirectState::default();

    async fn rust_connect(address: String) -> Option<CompactTxStreamerClient<Channel>> {
        loop {
            if let Ok(channel) = Channel::from_shared(address.clone()).unwrap().connect().await {
                return Some(CompactTxStreamerClient::new(channel));
            }

            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        }
    }

    #[allow(unsafe_code)]
    unsafe extern "C" fn c_connect(address: *const u8, address_len: usize, user_data: *mut std::ffi::c_void) -> *mut std::ffi::c_void {
        let handle = unsafe { &*(user_data as *const tokio::runtime::Handle) };
        let address = unsafe { String::from_utf8_lossy(slice::from_raw_parts(address, address_len)) }.to_string();

        let (tx, rx) = std::sync::mpsc::channel();
        handle.spawn(async move {
            tx.send(rust_connect(address).await);
        });

        tokio::task::block_in_place(move || {
            match rx.recv() {
                Ok(Some(client)) => {
                    return Box::into_raw(Box::new(client)) as *mut std::ffi::c_void;
                }

                _ => std::ptr::null_mut()
            }
        })
    }

    #[allow(unsafe_code)]
    let mut connect = async | address: String | {
        if JUDAHS_NETWORK_TESTS {
            let mut handle = tokio::runtime::Handle::current();
            indirect_state.connect(address.as_ptr(), address.len(), c_connect, &mut handle as *mut _ as *mut std::ffi::c_void);

            if indirect_state.network_handle == std::ptr::null_mut() {
                return None;
            } else {
                return Some(unsafe { Box::from_raw(indirect_state.network_handle as *mut CompactTxStreamerClient<Channel>) });
            }
        } else {
            return rust_connect(address).await.map(|client| Box::new(client));
        }
    };

    let mut client = connect(format!("http://localhost:{}", zaino_port)).await.expect("failed to connect!");

    // NOTE: current model is to reorg this many blocks back
    // ALT: have checkpoints every 16/32 blocks and always sync from the start of one of these
    const MAX_BLOCKS_TO_DOWNLOAD_AT_TIME: u64 = 64;
    let mut time_since_last_transparent_shielded = std::time::Instant::now() - std::time::Duration::from_secs(1000);

    let (mut user_use_i,  mut user_update_i)  = (0,0);
    let (mut miner_use_i, mut miner_update_i) = (0,0);
    // let mut user_wallets  = [user_wallet_init,  WalletDb::for_path(":memory:", network, SystemClock, OsRng).unwrap()];
    // let mut miner_wallets = [miner_wallet_init, WalletDb::for_path(":memory:", network, SystemClock, OsRng).unwrap()];
    let mut user_wallets  = [user_wallet_init];
    let mut miner_wallets = [miner_wallet_init];

    let mut stupid_thing_because_judah_is_tired_and_wants_this_to_work_properly = Vec::<TxId>::new();

    let genesis_hash = loop {
        match client.get_block(BlockId { height: 0, hash: Vec::new() }).await {
            Ok(block) => { break <[u8;32]>::try_from(&block.into_inner().hash[..]).unwrap() },
            Err(err) => { print!("failed to get genesis block") },
        }
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    };

    let mut roster: Vec<RosterMember> = Vec::new();
    let mut pow_cache = PoWCache::new(0, genesis_hash);
    // NOTE: checkpoints allow us to reset the tree after a reorg & also create spend anchors
    const CHECKPOINTS_N: usize = 100;
    let mut orchard_tree = OrchardShardTree::new(shardtree::store::memory::MemoryShardStore::empty(), CHECKPOINTS_N);
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
                                    println!("MEMPOOL ERROR: can't send message to channel: {err:?}");
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

    let mut faucet_shield_cooldown_instant = Instant::now() - Duration::from_secs(1000);

    let mut resync_c = 0;
    'outer_sync: loop {
        if resync_c > 0 {
            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        }
        resync_c += 1;

        // NOTE: this is desynced from local_tip because we need to speculatively request blocks
        // further back than the chain divergence on reorg to find out where it occurred
        let mut req_start_h = pow_cache.next_tip_h-1;

        // NOTE: if you're dealing with multiple wallets, you don't want to resync all blocks for each
        // of them. They can all sync from the same blocks.
        // TODO: this needs to be a bit more complicated to handle arbitrarily many transparent addresses
        let (new_blocks, miner_t_txs, mut sync_from_i, req_rng, prev_tip_chain_state):
            (Vec<CompactBlock>, Vec<(BlockHeight, Transaction)>, Option<usize>, (u64, u64), ChainState) =
             'sync_find_continuation_point: loop
        {
            // GET THE CURRENT STATE OF THE WORLD ////////////////////
            // BATCH NETWORK REQUESTS
            let (tree_state_res, lightd_res, block_range_res, t_txs_res, req_rng, t_req_rng) = {
                use std::future::Ready;
                // NOTE: clients are cheap to clone, and this is recommended in docs:
                // REF: https://docs.rs/tonic/0.14.2/tonic/client/index.html
                let (mut client0, mut client1, mut client2) = (client.clone(), client.clone(), client.clone());
                fn block_rng_from_heights(heights: (u64, u64)) -> BlockRange {
                    BlockRange {
                        start: Some(BlockId { height: heights.0, hash: Vec::new() }),
                        end:   Some(BlockId { height: heights.1, hash: Vec::new() }),
                    }
                }
                let req_rng = (req_start_h + 1, req_start_h + MAX_BLOCKS_TO_DOWNLOAD_AT_TIME);

                // ********************************************************************************
                // TODO IMPORTANT: the indexer can "succeed" without actually giving us all the txs
                // in the range we requested...
                // So we keep re-requesting the info in a trailing window...
                // LRZ/Zaino/Zebra *should* return an error when iterating through the t_txs
                // they also *shouldn't* return out-of-range responses
                // ********************************************************************************
                let t_req_rng = (req_rng.1.saturating_sub(2*MAX_BLOCKS_TO_DOWNLOAD_AT_TIME).max(1), req_rng.1);

                // println!("cache at {}, downloading blocks: {}-{}",
                //     pow_cache.next_tip_h-1, block_range.start.clone().unwrap().height, block_range.end.clone().unwrap().height);
                let (tree_state_res, lightd_res, block_range_res, t_txs_res) = tokio::join!(
                    client.get_tree_state(BlockId {height: req_start_h, hash: Vec::new()}),
                    client0.get_lightd_info(Empty {}),
                    client1.get_block_range(block_rng_from_heights(req_rng)),
                    client2.get_taddress_txids(TransparentAddressBlockFilter {
                        address: miner_t_address.encode(network),
                        range: Some(block_rng_from_heights(t_req_rng)),
                    })
                );
                (tree_state_res, lightd_res, block_range_res, t_txs_res, req_rng, t_req_rng)
            };

            //- ROSTER
            // TODO: batch
            match client.get_roster(Empty{}).await {
                Err(err) => println!("Get roster error: {err:?}"),
                Ok(res) => {
                    use std::io::{ Cursor,Read };
                    let roster_bytes = res.into_inner().data;

                    let mut ok = roster_bytes.len() > 0;
                    let mut cur = Cursor::new(&roster_bytes);

                    let mut new_roster = Vec::new();
                    let mut num_buf = [0u8; 8];
                    'read: while cur.position() < roster_bytes.len() as u64 {
                        let mut m = RosterMember{ pub_key: [0;32], voting_power:0, txids: Vec::new() };
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
                            let mut stake_txid = StakeTxId{ txid:[0;32], zats:0 };
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
                            println!("******* RECEIVED ROSTER VOTING POWER INACCURATE: {} vs {}", m.voting_power, voting_power_check);
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
                        .map(|member| WalletRosterMember{
                            pub_key: member.pub_key,
                            voting_power: member.voting_power,
                            txids: member.txids.clone()
                        })
                    .collect::<Vec<WalletRosterMember>>()
                        .clone();
                    println!("*********** WALLET ROSTER: {wallet_roster:?}");
                    wallet_state.lock().unwrap().roster = wallet_roster;
                }
            }
            println!("*********** ROSTER: {roster:?}");


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
                    for wallet in [&mut miner_wallets[miner_use_i], &mut user_wallets[user_use_i]] {
                        wallet.chain_tip_h = BlockHeight(network_tip_h);
                    }
                },
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
            for (wallet_i, wallet) in [&mut miner_wallets[miner_use_i], &mut user_wallets[user_use_i]].into_iter().enumerate() {
                for tx in &wallet.txs {
                    // TODO: trigger whenever we see a tx we want more info on
                    if in_flight_tx_requests.len() >= MAX_TXS_TO_DOWNLOAD_AT_TIME as usize {
                        break;
                    }

                    if !tx.is_outside_bc &&
                        tx.part_flags != TxParts::FULL_TX &&
                        // (tx.part_flags & TxParts::MEMO) == 0 &&
                            in_flight_tx_requests.get(&tx.txid).is_none()
                    {
                        in_flight_tx_requests.insert(tx.txid);

                        let mut client = client.clone();
                        let txid = tx.txid;
                        let filter = TxFilter{ hash: txid.as_ref().to_vec(), ..Default::default() };
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
                                    Ok(v) => Some(v.into_inner())
                                }
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
                        match block_stream.message().await { // TODO: bulk await these?
                            Ok(Some(block)) => {
                                new_blocks.push(block)
                            }
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
                    let Ok(prev_hash) = <[u8;32]>::try_from(&new_blocks[i].prev_hash[..]) else {
                        println!("invalid prev_hash for compact block at height {}: {}", new_blocks[i].height, LESlice(&new_blocks[i].prev_hash));
                        continue 'outer_sync;
                    };

                    let expected_prev_hash = pow_cache.hash_at_h(new_blocks[i].height-1);
                    if i == 0 {
                        let mut needs_resync = false;
                        if prev_hash != prev_tip_chain_state.block_hash().0 {
                            println!("non-atomic API meant block range & chain-state are torn reads: {} vs {}", LEHash(prev_hash), LEHash(prev_tip_chain_state.block_hash().0));
                            req_start_h = req_start_h.saturating_sub(MAX_BLOCKS_TO_DOWNLOAD_AT_TIME / 2);
                            needs_resync = true;
                        }
                        if Some(prev_hash) != expected_prev_hash {
                            println!("reorg occurred before height {}; hash mismatch {prev_hash:?} vs {expected_prev_hash:?}", new_blocks[0].height);
                            req_start_h = req_start_h.saturating_sub(MAX_BLOCKS_TO_DOWNLOAD_AT_TIME);
                            needs_resync = true;
                        }
                        if needs_resync {
                            println!("hit discontinuity; handling reorg!");
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
                    let hash = if let Ok(hash) = <[u8;32]>::try_from(&new_blocks[block_i].hash[..]) {
                        hash
                    } else {
                        println!("invalid hash for compact block at height {}: {}", new_blocks[block_i].height, LESlice(&new_blocks[block_i].hash));
                        data_is_invalid = true;
                        [0;32]
                    };
                    if <u32>::try_from(new_blocks[block_i].height).is_err() {
                        println!("block height cannot be stored in 32 bits: {}", new_blocks[block_i].height);
                        data_is_invalid = true;
                    }
                    for tx in &new_blocks[block_i].vtx {
                        if <[u8;32]>::try_from(&new_blocks[block_i].hash[..]).is_err() {
                            // TODO: are TxIds LE or BE?
                            println!("invalid hash for compact tx at height {}: {:?}", new_blocks[block_i].height, tx.hash);
                            data_is_invalid = true;
                            break;
                        }
                    }
                    let new_tip_h = new_blocks[block_i].height;
                    let cached_prev_hash = pow_cache.hash_at_h(new_tip_h-1);
                    let pre_new_tip_hash = <[u8;32]>::try_from(&new_blocks[block_i].prev_hash[..]).unwrap();
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
                            println!("reorg occurred in the middle of the returned blocks, caching up to the reorg, then we'll update to the other chain on the next iteration");
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
                break (Vec::new(), Vec::new(), None, req_rng, ChainState::empty(LRZBlockHeight::from_u32(0), zcash_primitives::block::BlockHash([0; 32])));
            }
            println!("downloaded compact blocks {}-{}", new_blocks.first().unwrap().height, new_blocks.last().unwrap().height);

            let compact_block_max_h = new_blocks.last().expect("non-empty vector").height;

            // TRANSPARENT TRANSACTIONS
            let mut t_failed_at_h = None;
            let mut new_raw_t_txs: Vec<RawTransaction> = Vec::new();
            let (mut min_t_h, mut max_t_h) = (u64::MAX, 0);
            match t_txs_res {
                Ok(t_txs) => {
                    let mut tx_stream = t_txs.into_inner();
                    loop {
                        match tx_stream.message().await { // TODO: bulk await these?
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
                                    t_failed_at_h = Some(new_raw_t_txs.last().map_or(0, |tx| tx.height+1));
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
                println!("downloaded transparent txs at heights {}-{}", min_t_h, max_t_h);
            }

            let mut new_t_txs = Vec::<(BlockHeight, Transaction)>::with_capacity(new_raw_t_txs.len());
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
                        println!("found sidechain transparent tx that we don't have height for, skipping...");
                        continue;
                    }
                    Some(Some(h)) => h,
                    None => break, // read error
                };

                if ! h.is_in_block() {
                    break;
                }
                if u64::from(h.0) > compact_block_max_h {
                    // @in_step_sync
                    break;
                }

                let tx = match Transaction::read(&raw_tx.data[..], BranchId::for_height(network, LRZBlockHeight::from_u32(h.0))) {
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
                println!("truncating compact blocks to match transparent at {h}");
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

            break (new_blocks, new_t_txs, sync_from_i, req_rng, prev_tip_chain_state);
        };

        let network_tip_h = user_wallets[user_use_i].chain_tip_h;

        // let mut orchard_frontier = prev_tip_chain_state.final_orchard_tree().clone();
        // let mut orchard_tree = incrementalmerkletree::frontier::CommitmentTree::from_frontier(&orchard_frontier);
        // println!("orchard root at {:?} tree: size={} {:?}", prev_tip_chain_state.block_height(), shard_tree_size(orchard_tree), shard_tree_root(orchard_tree));


        //-- REORG
        if let Some(start_block_i) = sync_from_i {
            // the regime is basically "always reorg", but that's often a no-op
            // truncate wallet for everything below height
            let sync_start_h = <u32>::try_from(new_blocks[start_block_i].height).expect("successfully converted above");
            let block_h = BlockHeight(sync_start_h);
            let last_block_h = block_h.sat_sub(1);

            orchard_tree.truncate_to_checkpoint(&last_block_h); // N.B. checkpoints are at the *end* of their block

            let (user_wallet, miner_wallet) = (&mut user_wallets[user_use_i], &mut miner_wallets[miner_use_i]);
            for (wallet_i, wallet) in [miner_wallet, user_wallet].into_iter().enumerate() {
                //-- INVALIDATE TXS >= NEW BLOCKS HEIGHT
                for account in &mut wallet.accounts {
                    account.fully_detected_h = account.fully_detected_h.min(last_block_h);
                    account.fully_decoded_h = account.fully_decoded_h.min(last_block_h);

                    // TODO: do we want to track balance changes or keep balances updated as chain changes occur?
                    let truncate_to_i = account.balance_changes.partition_point(|(b,_)| *b < block_h);
                    account.balance_changes.truncate(truncate_to_i);

                    //- UNRECEIVE NOTES
                    let utxos_at_h_start = account.utxos.partition_point(|txo| txo.recv_h < block_h);
                    account.utxos.truncate(utxos_at_h_start);
                    let recv_txos_at_h_start = account.recv_txos.partition_point(|txo| txo.recv_h < block_h);
                    account.recv_txos.truncate(recv_txos_at_h_start);

                    let unspent_orchard_notes_at_h_start = account.unspent_orchard_notes.partition_point(|txo| txo.recv_h < block_h);
                    account.unspent_orchard_notes.truncate(unspent_orchard_notes_at_h_start);
                    let recv_orchard_notes_at_h_start = account.recv_orchard_notes.partition_point(|txo| txo.recv_h < block_h);
                    account.recv_orchard_notes.truncate(recv_orchard_notes_at_h_start);

                    //- UNSPEND NOTES
                    // NOTE: spent notes are in spend_h order, NOT recv_h order
                    let stxos_at_h_start = account.stxos.partition_point(|txo| txo.spent_h < block_h);
                    for stxo in &account.stxos[stxos_at_h_start..] {
                        if stxo.recv_h < block_h {
                            txo_recv_h_insert(&mut account.utxos, Txo{ spent_h: BlockHeight(0), ..stxo.clone() });
                        }
                    }
                    account.stxos.truncate(stxos_at_h_start);

                    let spent_orchard_notes_at_h_start = account.spent_orchard_notes.partition_point(|note| note.spent_h < block_h);
                    for spent_orchard_note in &account.spent_orchard_notes[spent_orchard_notes_at_h_start..] {
                        if spent_orchard_note.recv_h < block_h {
                            orchard_recv_h_insert(&mut account.unspent_orchard_notes, OrchardNote{ spent_h: BlockHeight(0), ..spent_orchard_note.clone() });
                        }
                    }
                    account.spent_orchard_notes.truncate(spent_orchard_notes_at_h_start);
                }

                //  higher blocks & mempool
                let invalidate_from_i = wallet.txs.partition_point(|tx| tx.mined_h < block_h);
                for tx in &mut wallet.txs[invalidate_from_i..] {
                    // N.B. these may get revalidated later if the same txs are found in the new blocks
                    tx.is_outside_bc = true;
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
            let (user_wallet, miner_wallet) = (&mut user_wallets[user_use_i], &mut miner_wallets[miner_use_i]);
            let keys = PreparedKeys::from_ufvk_all(&miner_wallet.accounts[0].ufvk);
            let mut insert_i = 0;
            for t_tx_i in 0..miner_t_txs.len() {
                // kinda @in_step_sync
                let block_h = miner_t_txs[t_tx_i].0;
                let tx = &miner_t_txs[t_tx_i].1;
                read_full_tx(miner_wallet, 0, &keys, block_h, tx, &mut insert_i, false);
            }
        }

        //-- READ DOWNLOADED MEMPOOL TXS
        {
            // TODO: maybe wait until we're ~block-synced before doing this
            // NOTE: assumes we can keep up... maybe dropping with some feedback about that is better?
            let wallets = [&mut user_wallets[user_use_i], &mut miner_wallets[miner_use_i]];
            let keys = [
                PreparedKeys::from_ufvk_all(&wallets[0].accounts[0].ufvk),
                PreparedKeys::from_ufvk_all(&wallets[1].accounts[0].ufvk),
            ];
            let insert_idxs = [&mut 0, &mut 0];

            while let Ok(raw_tx) = mempool_recv.try_recv() {
                // NOTE: expected LRZ height different from abstract mempool height
                match Transaction::read(&raw_tx.data[..], BranchId::for_height(network, LRZBlockHeight::from_u32(network_tip_h.0 + 1))) {
                    Err(err) => {
                        println!("invalid mempool tx: {err:?}");
                        // NOTE: as mempool txs are not sequenced, it seems reasonable to just ignore
                        // invalid ones without skipping the rest
                    }
                    Ok(tx) => {
                        for i in 0..2 {
                            read_full_tx(wallets[i], 0, &keys[i], BlockHeight::MEMPOOL, &tx, insert_idxs[i], false);
                        }
                    }
                }
            }
        }


        //-- ADD/REVALIDATE SHIELDED TXS FROM NEW BLOCKS
        if let Some(start_block_i) = sync_from_i {
            let (user_wallet, miner_wallet) = (&mut user_wallets[user_use_i], &mut miner_wallets[miner_use_i]);
            let sync_start_h = <u32>::try_from(new_blocks[start_block_i].height).expect("successfully converted above");
            println!("cache at {}, new blocks: {}-{}; updating wallets...",
                pow_cache.next_tip_h-1, new_blocks.first().unwrap().height, new_blocks.last().unwrap().height);


            for (wallet_i, wallet) in [miner_wallet, user_wallet].into_iter().enumerate() {
                let keys = PreparedKeys::from_ufvk_ivks(&wallet.accounts[0].ufvk); // NOTE: can't use ovk for CompactTx
                let mut insert_i = 0;
                for block in &new_blocks {
                    let block_h = BlockHeight(block.height.try_into().unwrap());
                    update_insert_i(&wallet.txs, &mut insert_i, block_h);


                    //-- INCORPORATE SHIELDED TRANSACTIONS FROM COMPACT BLOCK
                    'tx_iter: for tx in &block.vtx {
                        if let (txid, true, true) = read_compact_tx(wallet, 0, &keys, block_h, tx, &mut insert_i, &mut orchard_tree) {
                            println!("found our compact tx: {txid:?}");
                        }
                    }

                    // NOTE: simple approach: checkpoint every block
                    // => allows for easy reorgs & witnesses
                    orchard_tree.checkpoint(block_h);
                    // println!("orchard root at {:?} tree: size={} {:?}", block_h, shard_tree_size(orchard_tree), shard_tree_root(orchard_tree));
                }
            }
        }


        //-- READ ANY DOWNLOADED FULL TXS
        if in_flight_tx_requests.len() > 0 {
            println!("before reading, there are {} in flight tx downloads", in_flight_tx_requests.len());
            let (user_wallet, miner_wallet) = (&mut user_wallets[user_use_i], &mut miner_wallets[miner_use_i]);
            let wallets = [miner_wallet, user_wallet];
            while let Some(tx_completion) = in_flight_tx_join_set.try_join_next() {
                let (txid, wallet_i, dl_result): (TxId, usize, Option<RawTransaction>) = match tx_completion {
                    Ok(v) => v,
                    Err(err) => {
                        println!("tx completion join error: {err:?}");
                        continue
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
                println!("download finished for {txid:?}");
                in_flight_tx_requests.remove(&txid);
                if let (Some(raw_tx), Some(existing_tx_i)) = (dl_result, tx_position(&wallets[wallet_i], &txid)) {
                    let existing_tx = &wallets[wallet_i].txs[existing_tx_i];

                    let found_h = match bc_h_from_raw_tx_h(raw_tx.height) {
                        Some(None) => existing_tx.mined_h, // on sidechain: use previously-spec'd height
                        Some(Some(h)) => {
                            if h != existing_tx.mined_h {
                                println!("requested tx {txid:?} has moved from {:?} to {h:?}", existing_tx.mined_h);
                            }
                            h
                        },
                        None => continue, // read error
                    };

                    // NOTE: there's a potential inconsistency here around branch id changes if we
                    // see it both before and after the change
                    let lrz_h = LRZBlockHeight::from_u32(if found_h.is_in_block() { found_h.0 } else { network_tip_h.0 });
                    let tx = match Transaction::read(&raw_tx.data[..], BranchId::for_height(network, lrz_h)) {
                        Ok(tx) => tx,
                        Err(err) => {
                            println!("failed to read tx at height {:?}/{found_h:?}/{lrz_h:?}", existing_tx.mined_h);
                            continue;
                        }
                    };

                    println!("reading downloaded full tx for {txid:?}");
                    let keys = PreparedKeys::from_ufvk_all(&wallets[wallet_i].accounts[0].ufvk);

                    read_full_tx(wallets[wallet_i], 0, &keys, existing_tx.mined_h, &tx, &mut 0, existing_tx.is_outside_bc);
                }
            }
            println!("after  reading, there are {} in flight tx downloads", in_flight_tx_requests.len());
        }

        //-- SEND DATA TO UI
        {
            let (user_wallet, miner_wallet) = (&mut user_wallets[user_use_i], &mut miner_wallets[miner_use_i]);
            println!("miner unspent UTXOs {:#?}", NL(&*miner_wallet.accounts[0].utxos));
            println!("miner spent   UTXOs {:#?}", NL(&*miner_wallet.accounts[0].stxos));
            println!("miner unspent notes {:#?}", NL(&*miner_wallet.accounts[0].unspent_orchard_notes));
            println!("miner spent   notes {:#?}", NL(&*miner_wallet.accounts[0].spent_orchard_notes));

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

            let mut user_txs = user_wallet.txs.clone();
            user_txs.reverse(); // TODO: just read in reverse order
            let mut miner_txs = miner_wallet.txs.clone();
            miner_txs.reverse(); // TODO: just read in reverse order

            let mut lock = wallet_state.lock().unwrap();
            lock.user_txs = user_txs;
            lock.miner_txs = miner_txs;
            lock.miner_unshielded_funds = miner_unshielded_funds;
            lock.miner_shielded_pending_funds = miner_shielded_pending_funds;
            lock.miner_shielded_spendable_funds = miner_shielded_spendable_funds;
            lock.miner_seen_h = miner_wallet.chain_tip_h.0;

            lock.user_unshielded_funds = user_unshielded_funds;
            lock.user_shielded_pending_funds = user_shielded_pending_funds;
            lock.user_shielded_spendable_funds = user_shielded_spendable_funds;
        }


        // Anchor debugging
        // {
        //     let (user_wallet, miner_wallet) = (&mut user_wallets[user_use_i], &mut miner_wallets[miner_use_i]);
        //     let orchard_anchor_h = miner_wallet.chain_tip_h.sat_sub(1);
        //     for i in 0..orchard_anchor_h.0 {
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

        if faucet_shield_cooldown_instant.elapsed().as_secs() > 5 {
            let (user_wallet, miner_wallet) = (&mut user_wallets[user_use_i], &mut miner_wallets[miner_use_i]);

            let maybe_shield_txid = miner_wallet.shield_transparent_zats(network, &mut client, &miner_usk, 1000000000, &orchard_tree).await;
            println!("Try miner shield txid: {:?}", maybe_shield_txid);
            faucet_shield_cooldown_instant = Instant::now();
        }

        // let (reorg_required) = 'process_blocks: {
        //     let mut reorg_required = false;
        //     for wallet in [&mut miner_wallet, &mut user_wallet] {
        //         // use zcash_client_backend::data_api::WalletCommitmentTrees;

        //         if let Err(err) = wallet.update_chain_tip(BlockHeight(local_tip_height as u32)) {
        //             println!("Failed to update chain tip: {:?}", err);
        //         }

        //         let mut scan_ranges = match wallet.suggest_scan_ranges() {
        //             Err(err) => {
        //                 println!("Failed to get scan ranges: {:?}", err);
        //                 continue;
        //             }
        //             Ok(scan_ranges) => scan_ranges,
        //         };

        //         while let Some(scan_range) = scan_ranges.first() {
        //             match scan_range.priority() {
        //                 ScanPriority::Verify => {
        //                     let previous_height = scan_range.block_range().start.saturating_sub(1);
        //                     let chain_state = match client.get_tree_state(BlockId { height: previous_height.into(), ..Default::default() }).await {
        //                         Ok(tree_state) => {
        //                             tree_state.into_inner().to_chain_state().unwrap()
        //                         }
        //                         Err(err) => {
        //                             println!("Failed to get tree state: {:?}", err);
        //                             continue;
        //                         }
        //                     };

        //                     match scan_cached_blocks(
        //                         &network,
        //                         &block_cache,
        //                         wallet,
        //                         scan_range.block_range().start,
        //                         &chain_state,
        //                         scan_range.len(),
        //                     ) {
        //                         Ok(_) => {
        //                             break;
        //                         }
        //                         Err(ChainError::Scan(err)) => {
        //                             let rewind_height = err.at_height().saturating_sub(1);
        //                             if let Err(err) = wallet.truncate_to_height(rewind_height) {
        //                                 assert!(false,"Failed to truncate wallet db: {:?}", err);
        //                             }

        //                             let deletion_range = ScanRange::from_parts(
        //                                 (rewind_height..BlockHeight(network_tip_height as u32)).into(),
        //                                 ScanPriority::Scanned,
        //                             );
        //                             if let Err(err) = block_cache.delete(deletion_range).await {
        //                                 assert!(false,"Failed to truncate block db: {:?}", err);
        //                             }

        //                             local_tip_height = rewind_height.into();
        //                             break 'process_blocks true;
        //                         }
        //                         Err(err) => {
        //                             assert!(false,"Failed to truncate wallet db: {:?}", err);
        //                         }
        //                     }
        //                 }

        //                 _ => {}
        //             }

        //             scan_ranges = wallet.suggest_scan_ranges().expect("failed to get new scan ranges");
        //         }
        //     }

        //     break 'process_blocks (false);
        // };

        // if reorg_required {
        //     continue;
        // }


        //     match client.get_roster(Empty{}).await {
        //         Err(err) => println!("Get roster error: {err:?}"),
        //         Ok(res) => {
        //             use std::io::{ Cursor,Read };
        //             let roster_bytes = res.into_inner().data;

        //             let mut ok = roster_bytes.len() > 0;
        //             let mut cur = Cursor::new(&roster_bytes);

        //             let mut new_roster = Vec::new();
        //             let mut num_buf = [0u8; 8];
        //             'read: while cur.position() < roster_bytes.len() as u64 {
        //                 let mut m = RosterMember{ pub_key: [0;32], voting_power:0, txids: Vec::new() };
        //                 if let Err(err) = cur.read_exact(&mut m.pub_key) {
        //                     println!("******* ROSTER DESERIALIZE ERROR: {err:?}");
        //                     ok = false;
        //                     break;
        //                 }
        //                 if let Err(err) = cur.read_exact(&mut num_buf) {
        //                     println!("******* ROSTER DESERIALIZE ERROR: {err:?}");
        //                     ok = false;
        //                     break;
        //                 }
        //                 m.voting_power = u64::from_le_bytes(num_buf);

        //                 if let Err(err) = cur.read_exact(&mut num_buf) {
        //                     println!("******* ROSTER DESERIALIZE ERROR: {err:?}");
        //                     ok = false;
        //                     break;
        //                 }

        //                 let mut voting_power_check = 0;
        //                 let txids_n = u64::from_le_bytes(num_buf);
        //                 for _ in 0..txids_n {
        //                     let mut stake_txid = StakeTxId{ txid:[0;32], zats:0 };
        //                     if let Err(err) = cur.read_exact(&mut stake_txid.txid) {
        //                         println!("******* ROSTER DESERIALIZE ERROR: {err:?}");
        //                         ok = false;
        //                         break 'read;
        //                     }
        //                     if let Err(err) = cur.read_exact(&mut num_buf) {
        //                         println!("******* ROSTER DESERIALIZE ERROR: {err:?}");
        //                         ok = false;
        //                         break 'read;
        //                     }
        //                     stake_txid.zats = u64::from_le_bytes(num_buf);
        //                     voting_power_check += stake_txid.zats;
        //                     m.txids.push(stake_txid);
        //                 }

        //                 if m.voting_power != voting_power_check {
        //                     // TODO: use manually-found one?
        //                     println!("******* RECEIVED ROSTER VOTING POWER INACCURATE: {} vs {}", m.voting_power, voting_power_check);
        //                     // ok = false;
        //                     // break;
        //                 }

        //                 new_roster.push(m);
        //             }

        //             if ok {
        //                 roster = new_roster;
        //             }

        //             let wallet_roster = roster
        //                 .iter()
        //                 .map(|member| WalletRosterMember{
        //                     pub_key: member.pub_key,
        //                     voting_power: member.voting_power,
        //                     txids: member.txids.clone()
        //                 })
        //                 .collect::<Vec<WalletRosterMember>>()
        //                 .clone();
        //             // println!("*********** WALLET ROSTER: {wallet_roster:?}");
        //             wallet_state.lock().unwrap().roster = wallet_roster;
        //         }
        //     }
        //     // println!("*********** ROSTER: {roster:?}");

        //     let Ok(info) = client.get_lightd_info(Empty {}).await else {
        //         println!("Failed to get lightd info");
        //         continue;
        //     };
        //     let network_tip_height = info.into_inner().block_height;

        //     // if let Ok(chain_height) = miner_wallets[miner_update_i].chain_height() {
        //     //     if let Some(chain_height) = chain_height {
        //     //         if network_tip_height == u64::from(chain_height) {
        //     //             w_flip(&mut miner_use_i, &mut miner_update_i);
        //     //             // println!("DOUBLE WALLET: flipping miner to {miner_use_i} at height {network_tip_height}");
        //     //             (miner_wallets[miner_update_i], miner_account) = wallet_from_stuff(network, Secret::new(miner_seed.expose_secret().clone()));
        //     //         }
        //     //     }
        //     // }

        //     // if let Ok(chain_height) = user_wallets[user_update_i].chain_height() {
        //     //     if let Some(chain_height) = chain_height {
        //     //         if network_tip_height == u64::from(chain_height) {
        //     //             w_flip(&mut user_use_i, &mut user_update_i);
        //     //             // println!("DOUBLE WALLET: flipping user to {user_use_i} at height {network_tip_height}");
        //     //             (user_wallets[user_update_i], user_account) = wallet_from_stuff(network, Secret::new(user_seed.expose_secret().clone()));
        //     //         }
        //     //     }
        //     // }

        //     // Sync wallet DBs
        //     // for (wallets, t_address, idxs) in [
        //     //     (&mut miner_wallets, miner_t_address, [miner_use_i, miner_update_i]),
        //     //     (&mut user_wallets, user_t_address, [user_use_i, user_update_i]),
        //     // ] {
        //     //     for i_i in 0..idxs.len() {
        //     //         let i = idxs[i_i];
        //     //         if i_i == 1 && idxs[0] == i {
        //     //             // don't dup work
        //     //             break;
        //     //         }
        //     //         let wallet = &mut wallets[i];

        //     //         // if 'needs_to_sync: /* what a funny language */ {
        //     //         //     if let Ok(chain_height) = wallet.chain_height() {
        //     //         //         if let Some(chain_height) = chain_height {
        //     //         //             network_tip_height != u64::from(chain_height)
        //     //         //         } else {
        //     //         //             network_tip_height > 1
        //     //         //         }
        //     //         //     } else {
        //     //         //         true
        //     //         //     }
        //     //         // }
        //     //         if network_tip_height != u64::from(wallet.chain_tip_height)
        //     //         {
        //     //             // if let Err(err) = zcash_client_backend::sync::run(&mut client, network, &mut block_cache, wallet, MAX_BLOCKS_TO_DOWNLOAD_AT_TIME).await {
        //     //             //     println!("Failed to sync wallet: {}", err);
        //     //             //     continue;
        //     //             // }
        //     //             todo!("finish sync")
        //     //         }

        //     //         let Ok(summary) = wallet.get_wallet_summary(block_policy_10()) else { continue; };
        //     //         let Some(summary) = summary else { continue; };

        //     //         let balances = summary.account_balances();
        //     //         println!("******* WALLET {:?} *******", t_address.encode(network));
        //     //         println!("BALANCES {:?}", balances);
        //     //         println!("SUMMARY  {:?}", summary);
        //     //     }
        //     // }

        //     let (user_wallet, miner_wallet) = (&mut user_wallets[user_use_i], &mut miner_wallets[miner_use_i]);
        //     // if time_since_last_transparent_shielded.elapsed().as_secs() > 15 {
        //     //     // Shield miner's transparent ZATOSHIz
        //     //     todo!("shield");
        //     // }

        //     // Update gui wallet state
        //     (async |user_wallet: &mut ManualWallet, miner_wallet: &mut ManualWallet| {
        //         let user_summary = match user_wallet.get_wallet_summary(block_policy_10()) {
        //             Ok(Some(summary)) => summary,
        //             Ok(None) => return,
        //             Err(err) => {
        //                 println!("Failed to get wallet summary: {}", err);
        //                 return;
        //             }
        //         };

        //         let balances = user_summary.account_balances();
        //         let mut spendable_balance = 0;
        //         let mut pending_balance   = 0;
        //         for (_, b) in balances {
        //             spendable_balance += b.spendable_value().into_u64();
        //             pending_balance   += b.change_pending_confirmation().into_u64() + b.value_pending_spendability().into_u64();
        //         }

        //         use core::ops::Add;
        //         let miner_vals = match miner_wallet.get_wallet_summary(block_policy_10()) {
        //             Ok(Some(summary)) => {
        //                 let bals = summary.account_balances();

        //                 let mut vals = (0,0,0);
        //                 for bal in bals {
        //                     let Ok(sh) = (*bal.1.orchard_balance() + *bal.1.sapling_balance()) else { continue; };
        //                     vals.0 += <u64>::from(bal.1.unshielded_balance().spendable_value());
        //                     vals.1 += <u64>::from(sh.change_pending_confirmation()) + <u64>::from(sh.value_pending_spendability());
        //                     vals.2 += <u64>::from(sh.spendable_value());
        //                 }
        //                 Some(vals)
        //             },
        //             _ => None
        //         };


        //         println!("WALLET HAS {} ({})) cTAZ", spendable_balance, str_from_ctaz(spendable_balance));

        //         let txs = if let Ok(mut history) = get_transaction_history(user_wallet) {
        //             if let Some((map, _sent_map)) = get_received_memos_and_actions(&mut client, user_wallet, network, &history).await {
        //                 user_txid_map = map;

        //                 let mut user_staked_txids = Vec::new();
        //                 let mut total_staked: u64 = 0;
        //                 for mem in &roster {
        //                     for mem_txid in &mem.txids {
        //                         let txid = TxId::from_bytes(mem_txid.txid);
        //                         let Some((action, memos)) = user_txid_map.get(&txid) else { continue; };
        //                         let Some(action) = action else { continue; };
        //                         match action.kind {
        //                             StakingActionKind::Add => {
        //                                 if !stupid_thing_because_judah_is_tired_and_wants_this_to_work_properly.contains(&txid) {
        //                                     total_staked += mem_txid.zats;
        //                                     user_staked_txids.push((mem.pub_key, *txid.as_ref(), action.val, mem_txid.zats))
        //                                 }
        //                             }

        //                             _ => {}
        //                         }
        //                     }
        //                 }

        //                 // println!("** USER TXID MAP: {user_txid_map:?}");
        //                 // println!("** USER TXIDS: {user_staked_txids:?}");
        //                 // println!("** USER TXIDS TOTAL: {total_staked:?}");

        //                 {
        //                     let mut wallet_lock = wallet_state.lock().unwrap();
        //                     wallet_lock.staked_roster  = user_staked_txids;
        //                     wallet_lock.staked_balance = total_staked.try_into().unwrap();
        //                 }

        //                 let mut txs: Vec<WalletTx> = history.iter().map(|tx| {
        //                     let maybe_action_memo = user_txid_map.get(&tx.txid);

        //                     let mut kind: WalletTxKind;
        //                     if tx.is_shielding {
        //                         kind = WalletTxKind::Shield;
        //                     }
        //                     else if tx.account_value_delta.is_negative() {
        //                         let is_action_stake = if let &Some((Some(action), _)) = &maybe_action_memo {
        //                             action.kind == StakingActionKind::Add
        //                         } else {
        //                             false
        //                         };
        //                         if tx.memo_count > 0 || is_action_stake {
        //                             kind = WalletTxKind::Stake;
        //                         } else {
        //                             kind = WalletTxKind::Send;
        //                         }
        //                     }
        //                     else if tx.account_value_delta.is_positive() {
        //                         kind = WalletTxKind::Receive;
        //                     }
        //                     else {
        //                         kind = WalletTxKind::Receive;
        //                     }

        //                     let mut tx = WalletTx(tx.clone(), kind);
        //                     if let Some((_, memos)) = maybe_action_memo {
        //                         if memos.len() > 0 {
        //                             if memos.len() > 1 {
        //                                 println!("received multiple memos in 1 transaction: {}", memos.len());
        //                             }
        //                             let bytes = memos[0].as_bytes();
        //                             if bytes.len() > tx.memo.len() {
        //                                 println!("memo too big ({}/{}):\"\"\"\n{}\n\"\"\"", bytes.len(), memos[0].len(), memos[0]);
        //                             }
        //                             let len = bytes.len().min(tx.memo.len());
        //                             tx.memo[..len].copy_from_slice(&bytes[..len]);
        //                         }
        //                     }

        //                     tx
        //                 })
        //                 .collect();

        //                 // @todo(judah): because of the database, we can't differentiate regular receives
        //                 // and staking receives... This is how we do that for now.
        //                 for tx in &mut txs {
        //                     if tx.memo.starts_with("@UNSTAKE_RECEIVE:".as_bytes()) {
        //                         tx.1 = WalletTxKind::Unstake;
        //                     }
        //                 }

        //                 Some(txs)
        //             } else {
        //                 None
        //             }
        //         } else {
        //             None
        //         };

        //         let tip_h: Option<u32> = Some(miner_wallet.chain_tip_height.into());
        //         // let tip_h: Option<u32> = if let Ok(Some(val)) = miner_wallet.chain_height() {
        //         //     Some(val.into())
        //         // } else {
        //         //     None
        //         // };

        //         let faucet_available = if let Some(tip_h) = tip_h {
        //             // Calculate the funds available for faucet;
        //             // This would be better done incrementally on initial scan, accounting for reorgs etc
        //             let h = tip_h.saturating_sub(MIN_TRANSPARENT_COINBASE_MATURITY + 2); // account for coinbase maturing & shielding tx

        //             if let Ok(history) = get_transaction_history(miner_wallet) {
        //                 let mut coinbase_total = 0;
        //                 let mut faucet_spent = 0;
        //                 let mut staking_spent = 0;
        //                 for tx in history {
        //                     if tx.is_shielding {
        //                         if let Some(height) = tx.mined_height {
        //                             let height: u64 = height.try_into().unwrap();
        //                             if height + (MIN_TRANSPARENT_COINBASE_MATURITY as u64 + 2 as u64) < tip_h as u64 {
        //                                 coinbase_total += tx.total_received.into_u64();
        //                             }
        //                         }
        //                     } else if tx.total_spent.into_u64() > 0 {
        //                         if tx.memo_count > 0 {
        //                             faucet_spent += tx.total_spent.into_u64();
        //                         } else {
        //                             staking_spent += tx.total_spent.into_u64();
        //                         }
        //                     }
        //                 }

        //                 println!("coinbase_total: {coinbase_total}");
        //                 println!("faucet_total: {}", coinbase_total/2);
        //                 println!("faucet_spent: {faucet_spent}");
        //                 println!("staking_spent: {staking_spent}");
        //                 Some((coinbase_total/2).saturating_sub(faucet_spent))
        //             } else {
        //                 None
        //             }
        //         } else {
        //             None
        //         };

        //         let mut automatically_send_to_the_user = false;

        //         {
        //             let mut wallet_lock = wallet_state.lock().unwrap();
        //             wallet_lock.balance         = spendable_balance as i64;
        //             wallet_lock.pending_balance = pending_balance   as i64;

        //             if let Some(txs) = txs {
        //                 wallet_lock.waiting_for_faucet = false; // TODO:???
        //                 wallet_lock.txs = txs;
        //             }
        //             if let Some(tip_h) = tip_h {
        //                 wallet_lock.miner_seen_height = tip_h;
        //             }
        //             if let Some(faucet_available) = faucet_available {
        //                 //automatically_send_to_the_user = faucet_available > 500_000_000; // @NOCHECKIN
        //                 wallet_lock.faucet_funds_available = faucet_available;
        //             }
        //             if let Some(vals) = miner_vals {
        //                 wallet_lock.miner_unshielded_funds = vals.0;
        //                 wallet_lock.miner_shielded_pending_funds = vals.1;
        //                 wallet_lock.miner_shielded_spendable_funds = vals.2;
        //             }
        //         }

        //         if automatically_send_to_the_user {
        //             let Some((_, user_ua)) = addrs_from_account(&user_account, 0) else {
        //                 println!("Failed to get transparent address from account!");
        //                 return;
        //             };

        //             let zats = (Zatoshis::from_nonnegative_i64(500_000_000).unwrap() - MINIMUM_FEE).unwrap();
        //             // NOTE: we can't send transparent->transparent through the high-level API, we
        //             // have to propose_shielding first, then send in a later block
        //             todo!("auto-send to user")
        //         }
        //     })(user_wallet, miner_wallet).await;

        //     (async |miner_wallet: &mut ManualWallet, miner_usk, network| {
        //         if let Ok(history) = get_transaction_history(miner_wallet) {
        //             if let Some((map, sent_map)) = get_received_memos_and_actions(&mut client, miner_wallet, network, &history).await {
        //                 miner_txid_map = map;
        //                 miner_sent_txid_map = sent_map;

        //                 if !CHEAT_UNSTAKING && *AM_I_THE_UNSTAKER.lock().unwrap() {
        //                     // TODO: does this have a race condition with syncing?
        //                     for (tx, (action, memos)) in &miner_txid_map {
        //                         let Some(action) = action else { continue; };
        //                         if action.kind != StakingActionKind::Sub { continue; }

        //                         println!("SAM DEBUG got here 1");

        //                         let mut handled = false;
        //                         // got a request to unstake; have we already repaid it?
        //                         for (sent_tx, (_action, sent_memos)) in &miner_sent_txid_map  {
        //                             if sent_memos.len() == 0 { continue; }
        //                             if sent_memos[0].len() < "@UNSTAKE_RECEIVE: ".len()+64 { continue; }
        //                             if Some(action.source) == StakingAction::addr_from_str_bytes(&sent_memos[0].as_bytes()["@UNSTAKE_RECEIVE: ".len().."@UNSTAKE_RECEIVE: ".len()+64]) {
        //                                 handled = true;
        //                                 break;
        //                             }
        //                         }
        //                         if !handled {
        //                             send_unstake_reward(&mut client, &roster, &miner_txid_map, &TxId::from_bytes(action.source), miner_wallet, miner_usk, network, &mut Vec::new()).await;
        //                         }
        //                     }
        //                 }
        //             }
        //         }
        //     })(miner_wallet, &miner_usk, network).await;
        //     // Process gui wallet actions

        // @todo(judah): I'm thinking the weird frame hitch we get in the UI is caused by this loop,
        // since it's probably waiting for the wallet_state mutex to unlock.
        let mut retries_this_round = 6;
        loop {
            let action: WalletAction = {
                let mut wallet_lock = wallet_state.try_lock();
                let Ok(wallet_state) = &mut wallet_lock else {
                    if retries_this_round > 0 {
                        retries_this_round -= 1;
                        println!("wallet lock retry ({retries_this_round} attempts remaining)");
                        tokio::time::sleep(tokio::time::Duration::from_millis(9)).await;
                        continue;
                    } else {
                        break;
                    }
                };

                println!("*** wallet has {:?} actions in flight", wallet_state.actions_in_flight.len());
                let Some(action) = wallet_state.actions_in_flight.front() else { break; };
                action.clone()
            };

            let ok: bool = 'process_action: {
                match &action {
                    WalletAction::RequestFromFaucet => {
                        let (user_wallet, miner_wallet) = (&mut user_wallets[user_use_i], &mut miner_wallets[miner_use_i]);
                        let maybe_send_txid = miner_wallet.send_orchard_to_orchard_zats(network, &mut client, &miner_usk, 500_000_000, &orchard_tree, user_ua.orchard().unwrap()).await;
                        println!("Try miner send txid: {:?}", maybe_send_txid);
                        match maybe_send_txid {
                            None => {
                                wallet_state.lock().unwrap().waiting_for_faucet = false;
                                true
                            }
                            Some(_) => {
                                wallet_state.lock().unwrap().waiting_for_faucet = false;
                                true
                            },
                        }
                    }

                    WalletAction::StakeToFinalizer(amount, target_finalizer) => {
                        let (user_wallet, miner_wallet) = (&mut user_wallets[user_use_i], &mut miner_wallets[miner_use_i]);
                        let maybe_send_txid = user_wallet.stake_orchard_to_finalizer(network, &mut client, &user_usk, amount.into_u64(), &orchard_tree, target_finalizer).await;
                        println!("Try miner send txid: {:?}", maybe_send_txid);
                        match maybe_send_txid {
                            None => {
                                wallet_state.lock().unwrap().waiting_for_stake_to_finalizer = false;
                                false
                            }
                            Some(_) => {
                                wallet_state.lock().unwrap().waiting_for_stake_to_finalizer = false;
                                true
                            },
                        }
                    }

                    WalletAction::SendToAddress(address, amount) => { true
                        // let Ok(Some(wallet_summary)) = user_wallet.get_wallet_summary(ConfirmationsPolicy::MIN) else {
                        //     println!("Failed to get wallet summary");
                        //     break 'process_action false;
                        // };

                        // let mut spendable = 0;
                        // let balances = wallet_summary.account_balances();
                        // for (_, b) in balances {
                        //     spendable += b.spendable_value().into_u64();
                        // }

                        // // @todo(judah): better check?
                        // let amount_with_fee = (*amount - MINIMUM_FEE).unwrap();
                        // if spendable < amount.into_u64() {
                        //     println!("Not enough spendable zats to send!");
                        //     break 'process_action false;
                        // }

                        // println!("*********** SEND ZEC {:?} ({:?}) TO {}", amount, amount_with_fee, &address.encode(network));
                        // match send_zats(&mut client, &address, user_wallet, &user_usk, amount_with_fee, network, &TxOptions::default()).await {
                        //     None => {
                        //         println!("Failed to send ZEC to {}", address.encode(network));
                        //         wallet_state.lock().unwrap().waiting_for_send = false;
                        //         false
                        //     }
                        //     Some(_) => {
                        //         wallet_state.lock().unwrap().waiting_for_send = false;
                        //         true
                        //     }
                        // }
                    }

                    WalletAction::UnstakeFromFinalizer(txid) => { true
                        // let mut ok = { // User sends unstaking action
                        //     let Some((member_pub_key, staked_txid)) = ('find_txid: {
                        //         for mem in &roster {
                        //             for mem_txid in &mem.txids {
                        //                 if TxId::from_bytes(mem_txid.txid) == *txid {
                        //                     break 'find_txid Some((mem.pub_key, mem_txid.clone()));
                        //                 }
                        //             }
                        //         }
                        //         None
                        //     }) else {
                        //         println!("*** Failed to find member with txid: {:?}", txid);
                        //         break 'process_action false;
                        //     };

                        //     let Some((action, _)) = user_txid_map.get(&txid) else {
                        //         println!("*** Failed to find user staking transaction via txid {:?}", txid);
                        //         break 'process_action false;
                        //     };

                        //     let Some(action) = action else {
                        //         println!("*** Staking action was unset in txid {:?}", txid);
                        //         break 'process_action false;
                        //     };

                        //     let opts = TxOptions {
                        //         staking_action: Some(StakingAction {
                        //             kind: StakingActionKind::Sub, // @todo: clear?
                        //             val: staked_txid.zats,
                        //             target: member_pub_key,
                        //             source: *txid.as_ref(),
                        //             insecure_target_name: "".to_owned(),
                        //             insecure_source_name: "".to_owned(),
                        //         }),
                        //         memo: None,
                        //     };

                        //     // @note(judah): the miner sends to its own address because if the user sends it,
                        //     // the tx will appear as a regular send of -0.2 cTAZ....
                        //     match send_zats(&mut client, &miner_ua, miner_wallet, &miner_usk, Zatoshis::from_u64(10_000).unwrap() /* @todo fees */, network, &opts).await {
                        //         None => {
                        //             println!("Failed to send unstaking action to miner");
                        //             false
                        //         }
                        //         Some(_) => {
                        //             println!("Successfully sent unstaking action to miner");
                        //             true
                        //         }
                        //     }
                        // };

                        // // Miner sends reward back to user
                        // if CHEAT_UNSTAKING {
                        //     ok &= send_unstake_reward(&mut client, &roster, &miner_txid_map, txid, miner_wallet, &miner_usk, network, &mut stupid_thing_because_judah_is_tired_and_wants_this_to_work_properly).await.is_some();
                        // }

                        // ok
                    }

                    _ => { true }
                }
            };

            if !ok {
                println!("** Failed to process action: {:?}", &action);
            }

            wallet_state.lock().unwrap().actions_in_flight.pop_front();
        }

        //     tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
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
