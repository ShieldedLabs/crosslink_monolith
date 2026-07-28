//! Internal Zebra service for managing the Crosslink consensus protocol

#![allow(clippy::print_stdout)]
#![allow(unexpected_cfgs, unused, missing_docs)]

#[macro_use]
extern crate lazy_static;

use color_eyre::install;

use async_trait::async_trait;
use strum::{EnumCount, IntoEnumIterator};
use strum_macros::{EnumCount, EnumIter};

use tenderlink::SortedRosterMember;
use tracing_futures::WithSubscriber;
use zcash_primitives::transaction::{RosterMember, StakingAction, StakingActionKind};
use ed25519_zebra::VerificationKeyBytes;
use zebra_chain::serialization::{
    SerializationError, ZcashDeserialize, ZcashDeserializeInto, ZcashSerialize,
};
use zebra_state::crosslink::*;

use multiaddr::Multiaddr;
use rand::{CryptoRng, RngCore};
use rand::Rng;
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{File, OpenOptions};
use std::io::Cursor;
use std::io::{Read, Seek, SeekFrom};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::broadcast;
use tokio::time::Instant;
use tracing::{error, info, warn};

use bytes::{Bytes, BytesMut};

use zcash_primitives::bft::*;
use zcash_primitives::block::{
    BlockHash,
    BlockHeaderData as BcBlockHeader,
    BlockHeader as BcBlockHeaderWrap,
};
use zcash_protocol::consensus::{BlockHeight, TEST_NETWORK};

use chrono::DateTime;

pub use wallet;

use std::sync::Mutex;
use tokio::sync::Mutex as TokioMutex;

pub static TEST_INSTR_C: Mutex<usize> = Mutex::new(0);
pub static TEST_MODE: Mutex<bool> = Mutex::new(false);
const MAX_POS_STORE_ROSTER_MEMBERS: u64 = 100_000;
const MAX_POS_STORE_PROPOSAL_SIGNATURES: u64 = 100_000;
const POS_STORE_V2_MAGIC: [u8; 8] = *b"CTAZPSV2";
const POS_STORE_V2_HASH_DOMAIN: &[u8] = b"ctaz-pos-store-v2-frame";
const POS_STORE_V2_HEADER_LEN: u64 = 8 + 8 + 32;
const MAX_POS_STORE_V2_PAYLOAD_BYTES: u64 = 64 * 1024 * 1024;
const MAX_CANONICAL_CONSENSUS_ROUND: i64 = 0x7fff_ffff;
const SIGNER_MIGRATION_RECEIPT_SCHEMA: &str = "ctaz.signer-migration-receipt.v1";
const SIGNER_MIGRATION_RECEIPT_ACTION: &str = "authorize_non_genesis_signer_bootstrap";
const MAX_SIGNER_MIGRATION_RECEIPT_BYTES: u64 = 64 * 1024;
pub(crate) const SERVICE_HEALTH_STARTING: u8 = 0;
pub(crate) const SERVICE_HEALTH_READY: u8 = 1;
pub(crate) const SERVICE_HEALTH_OBSERVER_ONLY: u8 = 2;
pub(crate) const SERVICE_HEALTH_FAILED: u8 = 3;

fn open_exclusive_pos_store(path: &Path) -> Result<(File, bool), String> {
    let existed = match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
                return Err(format!("PoS store must be a regular non-symlink file: {}", path.display()));
            }
            true
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
        Err(error) => return Err(format!("failed to inspect PoS store {}: {error}", path.display())),
    };

    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use nix::fcntl::OFlag;
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags((OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC).bits());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(0);
    }
    let file = options.open(path)
        .map_err(|error| format!("failed to open PoS store {}: {error}", path.display()))?;
    let metadata = file.metadata()
        .map_err(|error| format!("failed to stat opened PoS store {}: {error}", path.display()))?;
    if !metadata.file_type().is_file() {
        return Err(format!("opened PoS store is not a regular file: {}", path.display()));
    }
    #[cfg(unix)]
    {
        use nix::unistd::geteuid;
        use std::os::unix::fs::MetadataExt;
        if metadata.uid() != geteuid().as_raw() {
            return Err(format!("PoS store is not owned by the service user: {}", path.display()));
        }
        if metadata.mode() & 0o077 != 0 {
            return Err(format!("PoS store permissions are broader than 0600: {}", path.display()));
        }
        if metadata.nlink() != 1 {
            return Err(format!("PoS store has unexpected hard links: {}", path.display()));
        }
    }
    file.try_lock()
        .map_err(|error| format!("failed to acquire exclusive PoS-store ownership {}: {error}", path.display()))?;

    if !existed {
        file.sync_all()
            .map_err(|error| format!("failed to sync new PoS store {}: {error}", path.display()))?;
        #[cfg(unix)]
        {
            let parent = path.parent().unwrap_or_else(|| Path::new("."));
            File::open(parent)
                .and_then(|directory| directory.sync_all())
                .map_err(|error| format!("failed to sync PoS-store parent {}: {error}", parent.display()))?;
        }
    }
    Ok((file, !existed))
}
pub static TEST_FAILED: Mutex<i32> = Mutex::new(0);
pub static TEST_FAILED_INSTR_IDXS: Mutex<Vec<(usize, String)>> = Mutex::new(Vec::new());
pub static TEST_CHECK_ASSERT: Mutex<u8> = Mutex::new(1);
pub static TEST_INSTR_PATH: Mutex<Option<std::path::PathBuf>> = Mutex::new(None);
pub static TEST_INSTR_BYTES: Mutex<Vec<u8>> = Mutex::new(Vec::new());
pub static TEST_INSTRS: Mutex<Vec<test_format::TFInstr>> = Mutex::new(Vec::new());
pub static TEST_SHUTDOWN_FN: Mutex<fn()> = Mutex::new(|| ());
pub static TEST_PARAMS: Mutex<Option<ZcashCrosslinkParameters>> = Mutex::new(None);
pub static TEST_NAME: Mutex<&'static str> = Mutex::new("‰‰TEST_NAME_NOT_SET‰‰");

pub fn dump_test_instrs() {
    #![allow(clippy::print_stderr)]

    let failed_instr_idxs_lock = TEST_FAILED_INSTR_IDXS.lock();
    let failed_instr_idxs = failed_instr_idxs_lock.as_ref().unwrap();
    if failed_instr_idxs.is_empty() {
        eprintln!(
            "no failed instructions recorded. We should have at least 1 failed instruction here"
        );
    }

    let done_instr_c = *TEST_INSTR_C.lock().unwrap();

    let mut failed_instr_idx_i = 0;
    let instrs_lock = TEST_INSTRS.lock().unwrap();
    let instrs: &Vec<test_format::TFInstr> = instrs_lock.as_ref();
    let bytes_lock = TEST_INSTR_BYTES.lock().unwrap();
    let bytes = bytes_lock.as_ref();
    for instr_i in 0..instrs.len() {
        let (col, msg) = if failed_instr_idx_i < failed_instr_idxs.len()
            && instr_i == failed_instr_idxs[failed_instr_idx_i].0
        {
            let msg = Some(failed_instr_idxs[failed_instr_idx_i].1.clone());
            failed_instr_idx_i += 1;
            ("\x1b[91m F  ", msg) // red
        } else if instr_i < done_instr_c {
            ("\x1b[92m P  ", None) // green
        } else {
            ("\x1b[37m    ", None) // grey
        };
        eprintln!(
            "  {}{}\x1b[0;0m",
            col,
            &test_format::TFInstr::string_from_instr(bytes, &instrs[instr_i])
        );
        if let Some(msg) = msg {
            eprintln!("      {}", msg);
        }
    }
}

pub mod service;
/// Configuration for the state service.
pub mod config {
    use serde::{Deserialize, Serialize};
    use std::fmt;

    // The canonical hardfork types live in `zebra-chain` so that zebra-state and
    // zebra-consensus — which cannot depend on zebra-crosslink — can share them.
    // Re-exported here for ergonomic access via `zebra_crosslink::config::*`.
    pub use zebra_chain::parameters::hardfork::{
        shipped_hardforks, HardForkConfig, HardForkSchedule,
    };

    /// An exact 32-byte lowercase-hex secret. Its debug representation is always redacted.
    #[derive(Clone, Eq, PartialEq, Deserialize, Serialize)]
    #[serde(transparent)]
    pub struct SecretHex32(String);

    impl SecretHex32 {
        pub(crate) fn expose_secret(&self) -> &str {
            &self.0
        }
    }

    impl fmt::Debug for SecretHex32 {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("SecretHex32(REDACTED)")
        }
    }

    /// A legacy opaque secret whose serialized form remains compatible but whose debug output
    /// is always redacted. It is never admitted as validator identity.
    #[derive(Clone, Eq, PartialEq, Deserialize, Serialize)]
    #[serde(transparent)]
    pub struct RedactedLegacySecret(String);

    impl fmt::Debug for RedactedLegacySecret {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("RedactedLegacySecret(REDACTED)")
        }
    }

    /// Canonical binding between a consensus identity and one explicit Noise endpoint.
    #[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    pub struct BftPeerIdentity {
        /// Raw roster key, exactly 32 bytes of lowercase hex.
        pub consensus_public_key: String,
        /// Network endpoint in `IP:port` or `[IPv6]:port` form.
        pub address: String,
        /// Noise static public key, exactly 32 bytes of lowercase hex.
        pub noise_public_key: String,
    }

    /// Canonical bootstrap voting identity. Transport routes are deliberately separate.
    #[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    pub struct BftBootstrapRosterMember {
        /// Raw roster key, exactly 32 bytes of lowercase hex.
        pub consensus_public_key: String,
        /// Explicit nonzero genesis/bootstrap voting power.
        pub voting_power: u64,
    }

    /// Configuration for the state service.
    #[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
    #[serde(deny_unknown_fields, default)]
    pub struct Config {
        /// Public address for this node, e.g. "/ip4/127.0.0.1/udp/24834/quic-v1" if testing
        /// internally, or the public IP address if using externally.
        pub public_address: Option<String>,
        /// Use the public IP instead of the generated seed
        ///
        /// Legacy only. It is never used for validator identity; a configuration that
        /// supplies only this field starts unready and observer-only.
        pub explicit_bft_key_seed: Option<RedactedLegacySecret>,
        /// List of public IP addresses for peers, in the same format as `public_address`.
        ///
        /// Legacy only. Endpoints are never converted into consensus or Noise keys.
        pub bft_peers: Vec<String>,
        /// Explicit validator Ed25519 signing seed, exactly 32 bytes of lowercase hex.
        pub validator_signing_key_seed: Option<SecretHex32>,
        /// Exact raw consensus public key expected from `validator_signing_key_seed`.
        pub validator_consensus_public_key: Option<String>,
        /// Explicit local Noise static-key seed, exactly 32 bytes of lowercase hex.
        pub validator_noise_static_key_seed: Option<SecretHex32>,
        /// Explicit key-bound peer endpoints. These are transport routes, never roster grants.
        pub bft_peer_identities: Vec<BftPeerIdentity>,
        /// Explicit initial roster used only before a durable PoS history supplies its next roster.
        pub bootstrap_bft_roster: Vec<BftBootstrapRosterMember>,
        /// Disable the headless wallet.
        pub disable_the_headless_wallet: bool,
        /// Disable zaino.
        pub disable_zaino: bool,
        /// User-led hardfork rules, as supplied in the config file. These are
        /// merged with [`shipped_hardforks`] and validated by building a
        /// [`HardForkSchedule`]; after loading, this holds the canonical, merged
        /// list (see `ZebradConfig::load`).
        pub hardforks: Vec<HardForkConfig>,
        /// Ignore the hardfork rules shipped in the executable
        /// ([`shipped_hardforks`]) and use only `hardforks`. Lets a testnet
        /// operator specify the entire hardfork schedule manually instead of
        /// inheriting the built-in (mainnet) assumed past. Defaults to `false`.
        pub disable_shipped_hardforks: bool,
        /// Key-scoped append-only Tenderlink signing WAL. Both this and an independent anchor
        /// are required before the process may sign; absence is observer-only.
        pub signer_wal_path: Option<std::path::PathBuf>,
        /// Monotonic anchor outside the WAL/state rollback domain and shared by every holder of
        /// this consensus key. Merely placing a second file on the same disk is not sufficient.
        pub signer_anchor_path: Option<std::path::PathBuf>,
        /// Explicit action gate proving the configured anchor also globally fences this key.
        /// False is always observer-only.
        pub signer_independent_anchor_authorized: bool,
        /// BLAKE3 hash (64 lowercase hex characters) of the operator-sealed one-time
        /// non-genesis bootstrap receipt. The exact receipt file and this hash are both
        /// required at a non-genesis startup; neither value self-authorizes a key.
        pub signer_non_genesis_bootstrap_receipt_blake3: Option<String>,
        /// Local structured receipt whose exact bytes are pinned by
        /// `signer_non_genesis_bootstrap_receipt_blake3`. It contains public bindings only.
        pub signer_non_genesis_bootstrap_receipt_path: Option<std::path::PathBuf>,
    }
    impl Default for Config {
        fn default() -> Self {
            Self {
                public_address: None,
                bft_peers: Vec::new(),
                explicit_bft_key_seed: None,
                validator_signing_key_seed: None,
                validator_consensus_public_key: None,
                validator_noise_static_key_seed: None,
                bft_peer_identities: Vec::new(),
                bootstrap_bft_roster: Vec::new(),
                disable_the_headless_wallet: false,
                disable_zaino: false,
                hardforks: Vec::new(),
                disable_shipped_hardforks: false,
                signer_wal_path: None,
                signer_anchor_path: None,
                signer_independent_anchor_authorized: false,
                signer_non_genesis_bootstrap_receipt_blake3: None,
                signer_non_genesis_bootstrap_receipt_path: None,
            }
        }
    }
}

pub mod test_format;
#[cfg(feature = "viz_gui")]
pub mod viz;

#[cfg(feature = "viz_gui")]
pub mod viz2;

use crate::service::{TFLServiceCalls, TFLServiceHandle};

// TODO: do we want to start differentiating BCHeight/PoWHeight, MalHeight/PoSHeigh etc?
use zebra_chain::block::{
    Block, CountedHeader, Hash as ZebBlockHash, Header as ZebBlockHeader, Height as ZebBlockHeight,
};
use zebra_node_services::mempool::{Request as MempoolRequest, Response as MempoolResponse};
use zebra_state::{crosslink::*, Request as StateRequest, Response as StateResponse, ReadRequest as StateReadRequest, ReadResponse as StateReadResponse};

/// Placeholder activation height for Crosslink functionality
pub const TFL_ACTIVATION_HEIGHT: ZebBlockHeight = ZebBlockHeight(0);

#[derive(Debug, Copy, Clone, EnumCount, EnumIter)]
enum BFTMsgFlag {
    ConsensusReady,
    StartedRound,
    GetValue,
    ProcessSyncedValue,
    GetValidatorSet,
    Decided,
    GetHistoryMinHeight,
    GetDecidedValue,
    ExtendVote,
    VerifyVoteExtension,
    ReceivedProposalPart,
}

pub fn bc_hdr_to_lrz(header: &ZebBlockHeader) -> BcBlockHeader {
    // @Hack
    let mut bytes = Vec::new();
    header.zcash_serialize(&mut bytes);
    BcBlockHeaderWrap::read_data(&*bytes).unwrap()


    // let time = header.time.signed_duration_since(chrono::DateTime::UNIX_EPOCH).num_seconds();
    // debug_assert_eq!(header.time, chrono::DateTime::<chrono::Utc>::from_timestamp_secs(time).unwrap());

    // BcBlockHeader {
    //     version: header.version.try_into().expect("non-negative version number"),
    //     prev_block: BlockHash(header.previous_block_hash.0),
    //     merkle_root: header.merkle_root.0,
    //     final_sapling_root: *header.commitment_bytes,
    //     time: time.try_into().unwrap(),
    //     bits: header.difficulty_threshold.into(),
    //     nonce: *header.nonce,
    //     solution: header.solution.value().to_vec(),
    //     fat_pointer_to_bft_block: header.fat_pointer_to_bft_block,
    // }
}

/// The set of finalizers terminated (blacklisted) by user-led hardforks at a given point,
/// derived purely from the hardfork schedule — no stored state. Like `namespace_for_bft_height`
/// and the viz, this is a pure function of `(schedule, bft_height, finalized_bc_height)`.
///
/// A finalizer is terminated at BFT height `bft_height` when some scheduled hardfork:
/// - has `bft_certificate_height <= bft_height` — in effect at this height, **inclusive**, so the
///   finalizers a hardfork terminates are already excluded from the roster that votes on the very
///   block carrying that hardfork (this matches the vote-namespacing inclusiveness); and
/// - has `pow_activation_height > finalized_bc_height` — the activation has not yet been finalized.
///   Once it is, the bonds are terminated at the source (so the finalizer is gone from the roster
///   anyway) and the key is free to be restaked to, so it must no longer be suppressed here.
pub(crate) fn terminated_finalizers_at(
    hardforks: &[crate::config::HardForkConfig],
    bft_height: u64,
    finalized_bc_height: u64,
) -> HashSet<PubKeyID> {
    let mut set = HashSet::new();
    for hf in hardforks.iter().filter(|hf| {
        hf.bft_certificate_height <= bft_height && hf.pow_activation_height > finalized_bc_height
    }) {
        for &finalizer in &hf.terminated_finalizers {
            set.insert(finalizer);
        }
    }
    set
}

#[allow(dead_code)]
#[derive(Debug)]
pub(crate) struct TFLServiceInternal {
    my_public_key: PubKeyID,
    latest_final_block: Option<(ZebBlockHeight, ZebBlockHash)>,
    tfl_is_activated: bool,

    // channels
    final_change_tx: broadcast::Sender<(ZebBlockHeight, ZebBlockHash)>,

    bft_msg_flags: u64, // ALT: Vec of messages/combine flags
    bft_err_flags: u64,
    bft_blocks: Vec<BftBlock>,
    bft_height_by_hash: HashMap<[u8; 32], usize>,
    fat_pointer_to_tip: FatPointerToBftBlock,
    our_set_bft_string: Option<String>,
    active_bft_string: Option<String>,

    peer_strings: Vec<String>,

    // TODO: 2 versions of this: ever-added (in sequence) & currently non-0
    finalizers_keys_to_names: HashMap<PubKeyID, String>,
    finalizers_at_current_height: Vec<RosterMember>,

    recency_status: TFLRecencyStatus,

    current_bc_final: Option<(ZebBlockHeight, ZebBlockHash)>,
    path_to_pos_store_file: PathBuf,
    // Held for the lifetime of the service. This removes pathname re-open races and
    // keeps exclusive ownership of the committed PoS history while signing is possible.
    pos_store_file: Option<File>,
    // A duplicated handle to the same exclusively held file. Historical relays use
    // positional reads, so they never move the append cursor.
    pos_store_read_file: Option<Arc<File>>,
    // One fixed-size authenticated replay receipt per committed height. The block
    // and proposal bytes remain on disk rather than growing another in-memory chain.
    pos_store_records: Vec<PosStoreRecordIndex>,
    // A post-install CrosslinkFinalizeBlock reflush remains pending until state confirms it.
    // This is deliberately in memory: replay reconstructs it from the installed durable tip.
    pending_reflush: Option<ZebBlockHash>,
    // A final v2 frame that ended at EOF before completion. It is never discarded on replay.
    // Only an exact certified decision whose complete frame has these bytes as a strict prefix
    // may replace it.
    pos_store_unverified_tail: Option<PosStoreTornTail>,
}

#[derive(Debug, Clone)]
struct PosStoreTornTail {
    offset: u64,
    bytes: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PosStoreRecordIndex {
    offset: u64,
    len: u64,
    finalized_bc_height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PosStoreAppendReceipt {
    durable_parent_commit: [u8; 32],
    offset: u64,
    len: u64,
}

fn call_from_state_to_crosslink_to_ask_about_fat_pointers(internal_handle: &TFLServiceHandle, parent_fat_pointer: FatPointerToBftBlock, child_fat_pointer: FatPointerToBftBlock, pow_block_height: ZebBlockHeight) -> Option<bool> {
    // Return value:
    //   None        => DEFER  — re-queue and re-evaluate on a later flush. REVERSIBLE. This is
    //                           the answer whenever we lack the information to be *certain* a
    //                           block is invalid — i.e. an unresolved BFT pointer (its block has
    //                           not entered this node yet) that may resolve later.
    //   Some(false) => REJECT — PERMANENT and IRREVERSIBLE: the block is dropped and every
    //                           descendant queued behind it is orphaned. We may ONLY return this
    //                           on facts that are immutable and view-independent, so that the
    //                           decision can never turn out to have been a transient mistake.
    //   Some(true)  => ACCEPT.
    //
    // This runs synchronously inside the state service. `blocking_lock` panics if called on a
    // tokio runtime thread *unless* it is inside `tokio::task::block_in_place`, so EVERY caller
    // of this function (via the state→crosslink closure) MUST be wrapped in `block_in_place`.
    // See `queue_and_commit_to_non_finalized_state` (already wrapped) and the
    // `CrosslinkFinalizeBlock` arm in zebra-state's `service.rs`. Acquiring the lock by blocking
    // (rather than `try_lock`) means there is no spurious "lock busy" defer: the answer is always
    // the real decision, never a transient miss.
    let internal = internal_handle.internal.blocking_lock();

    let parent_is_null = parent_fat_pointer == FatPointerToBftBlock::null();
    let child_is_null = child_fat_pointer == FatPointerToBftBlock::null();

    // PERMANENT, decided purely from the (immutable) pointer values, without resolving either
    // block: the child reverts to no BFT pointer while its parent had one. A null pointer can
    // never be "as new or newer" than a real one, so this is a certain regression.
    if child_is_null && !parent_is_null {
        return Some(false);
    }

    // Resolve the child pointer against the in-memory BFT chain. A non-null pointer we cannot
    // resolve yet (its BFT block has not entered this node) is NOT a failure — defer.
    let child_index = if child_is_null {
        None
    } else {
        match internal
            .bft_height_by_hash
            .get(&child_fat_pointer.points_at_block_hash().0)
            .copied()
        {
            Some(h) => Some(h),
            None => return None, // unresolved child -> defer (reversible)
        }
    };

    // PERMANENT, from immutable data: a resolved BFT block carries its own
    // `do_not_include_until_bc_height`, and the PoW block carries its own height. Both are fixed,
    // so a PoW block referencing a BFT block before that BFT block is allowed to be included can
    // never become valid later -> safe to reject permanently.
    if let Some(h) = child_index {
        let do_not_include = internal.bft_blocks[h].do_not_include_until_bc_height;
        if (pow_block_height.0 as u64) < do_not_include {
            return Some(false);
        }
    }

    // Resolve the parent pointer. Unresolved non-null parent -> defer.
    let parent_index = if parent_is_null {
        None
    } else {
        match internal
            .bft_height_by_hash
            .get(&parent_fat_pointer.points_at_block_hash().0)
            .copied()
        {
            Some(h) => Some(h),
            None => return None, // unresolved parent -> defer (reversible)
        }
    };

    // Ordering: the child must reference a BFT block at least as new as its parent's. A real
    // pointer ranks as `index + 1`; null ranks as 0 (kept strictly below any real pointer).
    //
    // Once BOTH pointers resolve, this is a PERMANENT fact: the BFT chain is append-only
    // (`handle_new_decided_bft_block` writes each block at the index equal to its own height,
    // inserts strictly in order — asserted against the predecessor — and never overwrites a real
    // block; finalized BFT blocks never revert). So a resolved pointer's index is fixed forever,
    // and a genuine ordering violation can never become valid later -> safe to reject permanently.
    // (Unresolved pointers already returned None above, so reaching here means both are known.)
    let child_rank = child_index.map(|h| h + 1).unwrap_or(0);
    let parent_rank = parent_index.map(|h| h + 1).unwrap_or(0);
    Some(child_rank >= parent_rank)
}

// TODO: Result?
async fn block_height_from_hash(call: &TFLServiceCalls, hash: ZebBlockHash) -> Option<ZebBlockHeight> {
    if let Ok(StateResponse::KnownBlock(Some(known_block))) =
        (call.state)(StateRequest::KnownBlock(hash.into())).await
    {
        Some(known_block.height)
    } else {
        None
    }
}

async fn block_height_hash_from_hash(
    call: &TFLServiceCalls,
    hash: ZebBlockHash,
) -> Option<(ZebBlockHeight, ZebBlockHash)> {
    if let Ok(StateResponse::BlockHeader {
        height,
        hash: check_hash,
        ..
    }) = (call.state)(StateRequest::BlockHeader(hash.into())).await
    {
        assert_eq!(hash, check_hash);
        Some((height, hash))
    } else {
        None
    }
}

async fn block_from_hash(
    call: &TFLServiceCalls,
    hash: ZebBlockHash,
) -> Option<Arc<Block>> {
    if let Ok(StateResponse::Block(Some(block))) = (call.state)(StateRequest::Block(zebra_state::HashOrHeight::Hash(hash.into()))).await {
        let check_hash = block.as_ref().hash();
        assert_eq!(hash, check_hash);
        Some(block)
    } else {
        None
    }
}

async fn is_block_known(
    call: &TFLServiceCalls,
    hash: ZebBlockHash,
) -> bool {
    if let Ok(StateResponse::KnownBlock(Some(known_block))) = (call.state)(StateRequest::KnownBlock(hash.into())).await {
        known_block.location == zebra_state::KnownBlockLocation::BestChain || known_block.location == zebra_state::KnownBlockLocation::SideChain
    } else {
        false
    }
}

async fn _block_header_from_hash(
    call: &TFLServiceCalls,
    hash: ZebBlockHash,
) -> Option<Arc<ZebBlockHeader>> {
    if let Ok(StateResponse::BlockHeader { header, .. }) =
        (call.state)(StateRequest::BlockHeader(hash.into())).await
    {
        Some(header)
    } else {
        None
    }
}

async fn _block_prev_hash_from_hash(call: &TFLServiceCalls, hash: ZebBlockHash) -> Option<ZebBlockHash> {
    if let Ok(StateResponse::BlockHeader { header, .. }) =
        (call.state)(StateRequest::BlockHeader(hash.into())).await
    {
        Some(header.previous_block_hash)
    } else {
        None
    }
}

async fn tfl_reorg_final_block_height_hash(
    call: &TFLServiceCalls,
) -> Option<(ZebBlockHeight, ZebBlockHash)> {
    let locator = (call.state)(StateRequest::BlockLocator).await;

    // NOTE: although this is a vector, the docs say it may skip some blocks
    // so we can't just `.get(MAX_BLOCK_REORG_HEIGHT)`
    if let Ok(StateResponse::BlockLocator(hashes)) = locator {
        let result_1 = match hashes.last() {
            Some(hash) => block_height_from_hash(call, *hash)
                .await
                .map(|height| (height, *hash)),
            None => None,
        };

        /* Alternative implementations:
        use std::ops::Sub;
        use zebra_chain::block::HeightDiff as BlockHeightDiff;

        let result_2 = if hashes.len() == 0 {
            None
        } else {
            let tip_block_height = block_height_from_hash(call, *hashes.first().unwrap()).await;

            if let Some(height) = tip_block_height {
                if height < ZebBlockHeight(zebra_state::MAX_BLOCK_REORG_HEIGHT) {
                    // not enough blocks for any to be finalized
                    None // may be different from `locator.last()` in this case
                } else {
                    let pre_reorg_height = height
                        .sub(BlockHeightDiff::from(zebra_state::MAX_BLOCK_REORG_HEIGHT))
                        .unwrap();
                    let final_block_req = StateRequest::BlockHeader(pre_reorg_height.into());
                    let final_block_hdr = (call.state)(final_block_req).await;

                    if let Ok(StateResponse::BlockHeader { height, hash, .. }) = final_block_hdr
                    {
                        Some((height, hash))
                    } else {
                        None
                    }
                }
            } else {
                None
            }
        };

        let mut result_3 = None;
        if hashes.len() > 0 {
            let tip_block_hdr = block_height_from_hash(call, *hashes.first().unwrap()).await;

            if let Some(height) = tip_block_hdr {
                if height >= ZebBlockHeight(zebra_state::MAX_BLOCK_REORG_HEIGHT) {
                    // not enough blocks for any to be finalized
                    let pre_reorg_height = height
                        .sub(BlockHeightDiff::from(zebra_state::MAX_BLOCK_REORG_HEIGHT))
                        .unwrap();
                    let final_block_req = StateRequest::BlockHeader(pre_reorg_height.into());
                    let final_block_hdr = (call.state)(final_block_req).await;

                    if let Ok(StateResponse::BlockHeader { height, hash, .. }) = final_block_hdr
                    {
                        result_3 = Some((height, hash))
                    }
                }
            }
        };
        let result_3 = result_3;

        //assert_eq!(result_1, result_2); // NOTE: possible race condition: only for testing
        //assert_eq!(result_1, result_3); // NOTE: possible race condition: only for testing
        // Sam: YES! Indeed there were race conditions.
        */

        result_1
    } else {
        None
    }
}

async fn tfl_final_block_height_hash(
    internal_handle: &TFLServiceHandle,
) -> Option<(ZebBlockHeight, ZebBlockHash)> {
    let mut internal = internal_handle.internal.lock().await;
    tfl_final_block_height_hash_pre_locked(internal_handle, &mut internal).await
}

async fn tfl_final_block_height_hash_pre_locked(
    internal_handle: &TFLServiceHandle,
    internal: &mut TFLServiceInternal,
) -> Option<(ZebBlockHeight, ZebBlockHash)> {
    #[allow(unused_mut)]
    if internal.latest_final_block.is_some() {
        internal.latest_final_block
    } else {
        tfl_reorg_final_block_height_hash(&internal_handle.call).await
    }
}

async fn push_new_bft_msg_flags(
    tfl_handle: &TFLServiceHandle,
    bft_msg_flags: u64,
    bft_err_flags: u64,
) {
    let mut internal = tfl_handle.internal.lock().await;
    internal.bft_msg_flags |= bft_msg_flags;
    internal.bft_err_flags |= bft_err_flags;
}

async fn propose_new_bft_block(tfl_handle: &TFLServiceHandle) -> Option<BftBlock> {
    #[cfg(feature = "viz_gui")]
    if let Some(state) = viz::VIZ_G.lock().unwrap().as_ref() {
        if state.bft_pause_button {
            return None;
        }
    }

    let call = tfl_handle.call.clone();
    let params = &PROTOTYPE_PARAMETERS;
    let (tip_height, _tip_hash) = match bounded_proposer_state_call(
        &call,
        StateRequest::Tip,
        "BFT proposer PoW-tip lookup",
    )
    .await
    {
        Ok(StateResponse::Tip(Some(value))) => value,
        Ok(StateResponse::Tip(None)) => return None,
        Ok(_) => {
            warn!("BFT proposer PoW-tip lookup returned the wrong response type");
            return None;
        }
        Err(error) => {
            warn!(%error, "BFT proposer could not read the PoW tip");
            return None;
        }
    };

    use std::ops::Sub;
    use zebra_chain::block::HeightDiff as BlockHeightDiff;

    let finality_candidate_height = tip_height.sub(BlockHeightDiff::from(
        params.bc_confirmation_depth_sigma as i64,
    ));

    let finality_candidate_height = if let Some(h) = finality_candidate_height {
        h
    } else {
        info!(
            "not enough blocks to enforce finality; tip height: {}",
            tip_height.0
        );
        return None;
    };

    let (latest_final_block, latest_bft_block_hash) = {
        let internal = tfl_handle.internal.lock().await;
        (
            internal.latest_final_block,
            internal
                .bft_blocks
                .last()
                .map_or(Blake3Hash([0u8; 32]), |b| b.blake3_hash()),
        )
    };
    let is_improved_final =
        latest_final_block.is_none() || finality_candidate_height > latest_final_block.unwrap().0;

    if !is_improved_final {
        info!(
            "candidate block can't be final: height {}, final height: {:?}",
            finality_candidate_height.0, latest_final_block
        );
        return None;
    }

    let finality_candidate_height = ZebBlockHeight(finality_candidate_height.0.min(if let Some(v) = latest_final_block { v.0.0+40 } else { u32::MAX }));

    let candidate_hash = match bounded_proposer_state_call(
        &call,
        StateRequest::BlockHeader(finality_candidate_height.into()),
        "BFT proposer finality-candidate lookup",
    )
    .await
    {
        Ok(StateResponse::BlockHeader { hash, .. }) => hash,
        Ok(_) => {
            warn!("BFT proposer finality-candidate lookup returned the wrong response type");
            return None;
        }
        Err(error) => {
            warn!(%error, "BFT proposer could not read the finality candidate");
            return None;
        }
    };

    // NOTE: probably faster to request 2x as many blocks as we need rather than have another async call
    let mut headers: Vec<BcBlockHeader> = match bounded_proposer_state_call(
        &call,
        StateRequest::FindBlockHeaders {
            known_blocks: vec![candidate_hash],
            stop: None,
        },
        "BFT proposer confirmation-header lookup",
    )
    .await
    {
        Ok(StateResponse::BlockHeaders(headers)) => headers
            .into_iter()
            .map(|counted| bc_hdr_to_lrz(&counted.header))
            .collect(),
        Ok(_) => {
            warn!("BFT proposer confirmation-header lookup returned the wrong response type");
            return None;
        }
        Err(error) => {
            warn!(%error, "BFT proposer could not read confirmation headers");
            return None;
        }
    };
    headers.truncate(params.bc_confirmation_depth_sigma as usize);

    let internal = tfl_handle.internal.lock().await;

    // 0-based canonical height = the chain index this block will occupy.
    // Serialization re-adds the legacy +1 for v1 blocks (see BftBlock::zcash_serialize).
    let bft_height = internal.bft_blocks.len() as u64;
    let fat_ptr = internal.fat_pointer_to_tip.clone();
    // Parent (current tip) do_not_include, carried forward so the value never regresses.
    let parent_do_not_include = internal.bft_blocks.last().map_or(0, |p| p.do_not_include_until_bc_height);
    // The user-led hardforks scheduled at this BFT height (several rules may share
    // one certificate height). The config list is the canonical schedule, so this
    // filter preserves canonical (ascending pow_activation_height) order — the same
    // order validate_bft_block requires byte-for-byte.
    let scheduled_hardforks: Vec<crate::config::HardForkConfig> = tfl_handle
        .config
        .hardforks
        .iter()
        .filter(|hf| hf.bft_certificate_height == bft_height)
        .cloned()
        .collect();
    drop(internal);

    match BftBlock::try_from(params, bft_height as u32, fat_ptr, headers) {
        Ok(mut block) => {
            // Always propose v2 blocks: existing v1 blocks remain v1 (so their hashes/signatures
            // are untouched), and v2 >= any parent's version satisfies the monotonic-version check.
            block.version = 2;
            // Emit the scheduled hardforks (if any) and propagate do_not_include monotonically:
            // a hardfork block sets do_not_include = the greatest activation height it carries
            // (pointing at this block commits to every certificate in it, so the strictest one
            // governs); otherwise carry the parent's forward so it never regresses (both as
            // validate_bft_block requires).
            if let Some(last) = scheduled_hardforks.last() {
                block.do_not_include_until_bc_height = last.pow_activation_height;
                block.hardforks = scheduled_hardforks;
            } else {
                block.do_not_include_until_bc_height = parent_do_not_include;
            }
            Some(block)
        }
        Err(e) => {
            warn!("Unable to create BftBlock to propose, Error={:?}", e,);
            None
        }
    }
}

const CONSENSUS_STATE_CALL_TIMEOUT: Duration = Duration::from_secs(8);
const PROPOSER_STATE_CALL_TIMEOUT: Duration = Duration::from_secs(2);

async fn bounded_proposer_state_call(
    call: &TFLServiceCalls,
    request: StateRequest,
    operation: &'static str,
) -> Result<StateResponse, String> {
    match tokio::time::timeout(PROPOSER_STATE_CALL_TIMEOUT, (call.state)(request)).await {
        Err(_) => Err(format!("{operation} timed out after 2 seconds")),
        Ok(Err(error)) => Err(format!("{operation} failed: {error}")),
        Ok(Ok(response)) => Ok(response),
    }
}

async fn bounded_state_call(
    call: &TFLServiceCalls,
    request: StateRequest,
    operation: &'static str,
) -> Result<StateResponse, String> {
    match tokio::time::timeout(CONSENSUS_STATE_CALL_TIMEOUT, (call.state)(request)).await {
        Err(_) => Err(format!("{operation} timed out after 8 seconds")),
        Ok(Err(error)) => Err(format!("{operation} failed: {error}")),
        Ok(Ok(response)) => Ok(response),
    }
}

async fn bounded_crosslink_reflush(
    call: &TFLServiceCalls,
    final_hash: ZebBlockHash,
    operation: &'static str,
) -> Result<(), String> {
    let response = bounded_state_call(
        call,
        StateRequest::CrosslinkFinalizeBlock(final_hash),
        operation,
    )
    .await?;
    let StateResponse::CrosslinkFinalized(reflushed_hash, _) = response else {
        return Err(format!("{operation} returned the wrong response type"));
    };
    if reflushed_hash != final_hash {
        return Err(format!("{operation} finalized a different PoW hash"));
    }
    Ok(())
}

fn read_stored_roster_member<R: Read>(reader: &mut R) -> Result<RosterMember, String> {
    let mut pub_key = [0u8; 32];
    reader
        .read_exact(&mut pub_key)
        .map_err(|error| format!("stored roster key is truncated: {error}"))?;
    let mut u64_bytes = [0u8; 8];
    reader
        .read_exact(&mut u64_bytes)
        .map_err(|error| format!("stored roster stake is truncated: {error}"))?;
    let voting_power = u64::from_le_bytes(u64_bytes);
    reader
        .read_exact(&mut u64_bytes)
        .map_err(|error| format!("stored roster txid count is truncated: {error}"))?;
    let txids_len = u64::from_le_bytes(u64_bytes);
    if txids_len != 0 {
        return Err("PoS-store rosters must not contain transaction-detail vectors".into());
    }
    Ok(RosterMember {
        pub_key,
        voting_power,
        txids: Vec::new(),
    })
}

fn reconstructed_decided_round(
    block: &BftBlock,
    fat_pointer: &FatPointerToBftBlock,
    roster: &[SortedRosterMember],
    vote_namespace: [u8; 32],
    proposal_sigs: Vec<TMSig>,
) -> Result<tenderlink::RoundData, String> {
    tenderlink::validate_consensus_roster(roster)?;
    let active_len = usize::min(100, roster.len());
    let block_hash = block.blake3_hash();
    if fat_pointer.points_at_block_hash() != block_hash {
        return Err("fat pointer does not identify the decided BFT block".into());
    }
    let vote = fat_pointer.get_vote_template();
    if !vote.typ || vote.height != block.height as u64 || vote.value != block_hash {
        return Err("fat-pointer vote template has the wrong step, height, or value".into());
    }
    let round = u32::try_from(vote.round)
        .map_err(|_| "fat-pointer vote round is outside the canonical domain")?;
    if round > 0x7fff_ffff {
        return Err("fat-pointer vote round exceeds the canonical 31-bit domain".into());
    }

    let mut signatures = HashMap::with_capacity(fat_pointer.signatures.len());
    for signature in &fat_pointer.signatures {
        if !roster[..active_len]
            .iter()
            .any(|member| member.pub_key == signature.pub_key)
        {
            return Err("fat pointer contains a signer outside the active roster".into());
        }
        if signatures
            .insert(signature.pub_key, signature.vote_signature)
            .is_some()
        {
            return Err("fat pointer contains a duplicate signer key".into());
        }
    }

    let proposal = tenderlink::BlockValue(
        block
            .zcash_serialize_to_vec()
            .map_err(|error| format!("failed to serialize decided BFT block: {error}"))?,
    );
    let proposal_id = tenderlink::ValueId(block_hash.0);
    let msg_val_sigs = roster
        .iter()
        .map(|member| {
            let signature = signatures
                .get(&member.pub_key)
                .copied()
                .map(TMSig)
                .unwrap_or(TMSig::NIL);
            [
                (tenderlink::ValueId::NIL, TMSig::NIL),
                (if signature == TMSig::NIL {
                    tenderlink::ValueId::NIL
                } else {
                    proposal_id
                }, signature),
            ]
        })
        .collect();
    let proposal_sigs_n = proposal_sigs.len();
    Ok(tenderlink::RoundData {
        height: block.height as u64,
        round,
        proposal,
        proposal_id,
        proposal_sigs,
        proposal_sigs_n,
        msg_val_sigs,
        roster: roster.to_vec(),
        vote_namespace,
        ..tenderlink::RoundData::EMPTY
    })
}

fn verify_decided_fat_pointer_quorum(
    block: &BftBlock,
    fat_pointer: &FatPointerToBftBlock,
    roster: &[SortedRosterMember],
    vote_namespace: [u8; 32],
    proposal_sigs: Vec<TMSig>,
) -> Result<tenderlink::RoundData, String> {
    let round_data = reconstructed_decided_round(
        block,
        fat_pointer,
        roster,
        vote_namespace,
        proposal_sigs,
    )?;
    tenderlink::verify_reconstructed_precommit_quorum(&round_data, roster)?;
    Ok(round_data)
}

async fn validated_pow_header_chain(
    call: &TFLServiceCalls,
    block: &BftBlock,
    previous_final_height: Option<ZebBlockHeight>,
) -> Result<(ZebBlockHeight, ZebBlockHash), String> {
    let expected = PROTOTYPE_PARAMETERS.bc_confirmation_depth_sigma as usize;
    if block.headers.len() != expected {
        return Err(format!(
            "BFT block carries {} PoW headers, expected {expected}",
            block.headers.len()
        ));
    }

    let first_header = block
        .headers
        .first()
        .ok_or("BFT block has no finalization candidate")?;
    let first_hash = ZebBlockHash(BlockHash::from_header_data(first_header).0);
    let mut previous_hash = BlockHash::from_header_data(first_header);
    for carried in block.headers.iter().skip(1) {
        if carried.prev_block != previous_hash {
            return Err("carried PoW headers are not a contiguous hash-linked chain".into());
        }
        previous_hash = BlockHash::from_header_data(carried);
    }

    // One exact best-chain lookup of the last carried header proves the complete
    // locally hash-linked prefix is its canonical ancestry. Avoid one state/RPC
    // round trip per confirmation header during live validation and replay.
    let last_header = block.headers.last().expect("non-empty checked above");
    let last_hash = ZebBlockHash(BlockHash::from_header_data(last_header).0);
    let response = bounded_state_call(
        call,
        StateRequest::BlockHeader(last_hash.into()),
        "canonical PoW-header lookup",
    )
    .await?;
    let StateResponse::BlockHeader {
        header,
        height: last_height,
        hash,
        ..
    } = response
    else {
        return Err("canonical PoW-header lookup returned the wrong response type".into());
    };
    if hash != last_hash || header.hash() != hash || bc_hdr_to_lrz(&header) != *last_header {
        return Err("last carried PoW header is not byte-identical to the canonical best chain".into());
    }
    let preceding = u32::try_from(expected - 1)
        .map_err(|_| "PoW confirmation depth does not fit u32")?;
    let first_height = ZebBlockHeight(
        last_height
            .0
            .checked_sub(preceding)
            .ok_or("canonical PoW-header height underflows the carried chain")?,
    );
    if previous_final_height.is_some_and(|height| first_height <= height) {
        return Err("BFT decision does not strictly advance the PoW finality target".into());
    }
    let response = bounded_state_call(call, StateRequest::Tip, "PoW tip lookup").await?;
    let StateResponse::Tip(Some((tip_height, _))) = response else {
        return Err("PoW tip is unavailable while validating BFT depth".into());
    };
    let confirmed_span = tip_height
        .0
        .checked_sub(first_height.0)
        .and_then(|depth| depth.checked_add(1))
        .ok_or("BFT finalization candidate is above the PoW tip")?;
    if confirmed_span < expected as u32 {
        return Err("BFT finalization candidate lacks the required canonical depth".into());
    }
    Ok((first_height, first_hash))
}

fn validate_proposal_valid_round(valid_round: i64, decision_round: i32) -> Result<(), String> {
    if !(0..=MAX_CANONICAL_CONSENSUS_ROUND).contains(&i64::from(decision_round)) {
        return Err("decision round is outside the canonical domain".into());
    }
    if valid_round == -1 {
        return Ok(());
    }
    if !(0..=MAX_CANONICAL_CONSENSUS_ROUND).contains(&valid_round) {
        return Err("proposal valid_round is outside the canonical domain".into());
    }
    if valid_round >= i64::from(decision_round) {
        return Err("proposal valid_round must precede the decision round".into());
    }
    Ok(())
}

fn pos_store_v2_payload_hash(payload: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(POS_STORE_V2_HASH_DOMAIN);
    hasher.update(&(payload.len() as u64).to_le_bytes());
    hasher.update(payload);
    hasher.finalize().into()
}

fn encode_pos_store_v2_frame(
    new_block: &BftBlock,
    fat_pointer: &FatPointerToBftBlock,
    next_finalizers: &[RosterMember],
    proposal_valid_round: i64,
    tender_proposal_sigs: &[TMSig],
) -> Result<Vec<u8>, String> {
    if next_finalizers.len() as u64 > MAX_POS_STORE_ROSTER_MEMBERS {
        return Err("next PoS roster exceeds the durable-store bound".into());
    }
    if tender_proposal_sigs.len() as u64 > MAX_POS_STORE_PROPOSAL_SIGNATURES {
        return Err("proposal-signature set exceeds the durable-store bound".into());
    }
    validate_proposal_valid_round(
        proposal_valid_round,
        fat_pointer.get_vote_template().round,
    )?;
    if tender_proposal_sigs.is_empty() && proposal_valid_round != -1 {
        return Err("proposal valid_round requires a non-empty proposal manifest".into());
    }

    let mut payload = Vec::new();
    new_block
        .zcash_serialize(&mut payload)
        .map_err(|error| format!("failed to encode BFT decision: {error}"))?;
    fat_pointer
        .zcash_serialize(&mut payload)
        .map_err(|error| format!("failed to encode BFT certificate: {error}"))?;
    payload.extend_from_slice(&(next_finalizers.len() as u64).to_le_bytes());
    for member in next_finalizers {
        member.write_to_vec(&mut payload);
    }
    payload.extend_from_slice(&proposal_valid_round.to_le_bytes());
    payload.extend_from_slice(&(tender_proposal_sigs.len() as u64).to_le_bytes());
    for signature in tender_proposal_sigs {
        payload.extend_from_slice(&signature.0);
    }
    if payload.len() as u64 > MAX_POS_STORE_V2_PAYLOAD_BYTES {
        return Err("PoS v2 decision payload exceeds the durable-store bound".into());
    }

    let mut frame = Vec::with_capacity(POS_STORE_V2_HEADER_LEN as usize + payload.len());
    frame.extend_from_slice(&POS_STORE_V2_MAGIC);
    frame.extend_from_slice(&(payload.len() as u64).to_le_bytes());
    frame.extend_from_slice(&pos_store_v2_payload_hash(&payload));
    frame.extend_from_slice(&payload);
    Ok(frame)
}

fn decode_complete_pos_store_v2_frame(bytes: &[u8]) -> Result<StoredPosDecision, String> {
    if bytes.len() < POS_STORE_V2_HEADER_LEN as usize || bytes[..8] != POS_STORE_V2_MAGIC {
        return Err("PoS v2 frame header is missing".into());
    }
    let payload_len = u64::from_le_bytes(bytes[8..16].try_into().unwrap());
    if payload_len > MAX_POS_STORE_V2_PAYLOAD_BYTES {
        return Err("PoS v2 payload length exceeds the durable-store bound".into());
    }
    let expected_len = POS_STORE_V2_HEADER_LEN
        .checked_add(payload_len)
        .ok_or("PoS v2 frame length overflows")?;
    if bytes.len() as u64 != expected_len {
        return Err("PoS v2 frame length does not match its header".into());
    }
    let payload = &bytes[POS_STORE_V2_HEADER_LEN as usize..];
    let expected_hash: [u8; 32] = bytes[16..48].try_into().unwrap();
    if pos_store_v2_payload_hash(payload) != expected_hash {
        return Err("PoS v2 frame payload hash mismatch".into());
    }
    let mut cursor = Cursor::new(payload);
    let record = read_stored_pos_decision_payload(&mut cursor, true)?;
    if cursor.position() != payload.len() as u64 {
        return Err("PoS v2 payload contains trailing bytes".into());
    }
    Ok(record)
}

fn decode_complete_pos_store_record(bytes: &[u8]) -> Result<StoredPosDecision, String> {
    if bytes.starts_with(&POS_STORE_V2_MAGIC) {
        return decode_complete_pos_store_v2_frame(bytes);
    }
    let mut cursor = Cursor::new(bytes);
    let record = read_stored_pos_decision_payload(&mut cursor, false)?;
    if cursor.position() != bytes.len() as u64 {
        return Err("legacy PoS record contains trailing bytes".into());
    }
    Ok(record)
}

fn read_exact_pos_store_at(file: &File, offset: u64, bytes: &mut [u8]) -> Result<(), String> {
    let mut filled = 0usize;
    while filled < bytes.len() {
        let read_offset = offset
            .checked_add(
                u64::try_from(filled)
                    .map_err(|_| "PoS positional-read offset does not fit u64")?,
            )
            .ok_or("PoS positional-read offset overflows u64")?;
        #[cfg(unix)]
        let read = {
            use std::os::unix::fs::FileExt;
            file.read_at(&mut bytes[filled..], read_offset)
        };
        #[cfg(windows)]
        let read = {
            use std::os::windows::fs::FileExt;
            file.seek_read(&mut bytes[filled..], read_offset)
        };
        #[cfg(not(any(unix, windows)))]
        let read: std::io::Result<usize> = Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "positional PoS-store reads are unsupported on this platform",
        ));
        let read = read.map_err(|error| {
            format!("failed positional PoS-store read at byte {read_offset}: {error}")
        })?;
        if read == 0 {
            return Err(format!(
                "PoS-store record is truncated at byte {read_offset}"
            ));
        }
        filled = filled
            .checked_add(read)
            .ok_or("PoS positional-read length overflows usize")?;
    }
    Ok(())
}

fn read_indexed_pos_store_record(
    file: &File,
    index: PosStoreRecordIndex,
) -> Result<StoredPosDecision, String> {
    let maximum_len = POS_STORE_V2_HEADER_LEN
        .checked_add(MAX_POS_STORE_V2_PAYLOAD_BYTES)
        .ok_or("maximum PoS record length overflows u64")?;
    if index.len == 0 || index.len > maximum_len {
        return Err(format!(
            "indexed PoS record length {} is outside the bounded loader domain",
            index.len,
        ));
    }
    let len = usize::try_from(index.len)
        .map_err(|_| "indexed PoS record length does not fit usize")?;
    let mut bytes = vec![0u8; len];
    read_exact_pos_store_at(file, index.offset, &mut bytes)?;
    decode_complete_pos_store_record(&bytes)
}

fn populate_reconstructed_round_counts(
    round: &mut tenderlink::RoundData,
    roster: &[SortedRosterMember],
    fat_pointer: &FatPointerToBftBlock,
) -> Result<(), String> {
    let active_len = usize::min(100, roster.len());
    let signed_stake = roster[..active_len]
        .iter()
        .filter(|member| {
            fat_pointer
                .signatures
                .iter()
                .any(|signature| signature.pub_key == member.pub_key)
        })
        .try_fold(0u64, |total, member| total.checked_add(member.stake))
        .ok_or("reconstructed signing stake overflows u64")?;
    round.counts = tenderlink::ConsensusCounts {
        anys: signed_stake,
        prevotes: 0,
        nil_prevotes: 0,
        yes_prevotes: 0,
        precommits: signed_stake,
        yes_precommits: signed_stake,
    };
    Ok(())
}

async fn load_historical_committed_round(
    tfl_handle: &TFLServiceHandle,
    bootstrap_roster: Arc<Vec<RosterMember>>,
    height: u64,
) -> Result<Option<tenderlink::RoundData>, String> {
    let requested_index = usize::try_from(height)
        .map_err(|_| "requested historical BFT height does not fit usize")?;
    let (file, target_index, previous_index) = {
        let internal = tfl_handle.internal.lock().await;
        if internal.path_to_pos_store_file.as_os_str().is_empty() {
            return Ok(None);
        }
        if internal.pos_store_records.len() != internal.bft_blocks.len() {
            return Err("PoS record index is not aligned with the committed BFT chain".into());
        }
        if requested_index >= internal.bft_blocks.len() {
            return Ok(None);
        }
        let target_index = *internal
            .pos_store_records
            .get(requested_index)
            .ok_or("authenticated PoS record index is missing the requested height")?;
        let previous_index = requested_index
            .checked_sub(1)
            .map(|index| internal.pos_store_records[index]);
        let file = internal
            .pos_store_read_file
            .as_ref()
            .cloned()
            .ok_or("configured PoS store has no held positional-read handle")?;
        (file, target_index, previous_index)
    };
    let config = tfl_handle.config.clone();

    tokio::task::spawn_blocking(move || {
        let target = read_indexed_pos_store_record(&file, target_index)?;
        if !target.is_v2 || target.proposal_sigs.is_empty() {
            // Legacy records and force-fed records do not carry an authenticated
            // proposal manifest, so proposal chunks must never be fabricated.
            return Ok(None);
        }
        let previous = previous_index
            .map(|index| read_indexed_pos_store_record(&file, index))
            .transpose()?;
        let null_parent = FatPointerToBftBlock::null();
        let expected_parent = previous
            .as_ref()
            .map(|record| &record.fat_pointer)
            .unwrap_or(&null_parent);
        validate_stored_bft_semantics(
            &config,
            &target.block,
            previous.as_ref().map(|record| &record.block),
            height,
            expected_parent,
        )?;

        let previous_final_height = previous_index
            .map(|index| u64::from(index.finalized_bc_height))
            .unwrap_or(0);
        let current_finalizers = previous
            .as_ref()
            .map(|record| record.next_roster.clone())
            .unwrap_or_else(|| bootstrap_roster.as_ref().clone());
        let terminated = terminated_finalizers_at(
            &config.hardforks,
            height,
            previous_final_height,
        );
        let current_roster = tenderlink_roster_from_internal(
            &current_finalizers,
            &terminated,
        );
        let vote_namespace = namespace_for_bft_height(&config.hardforks, height);
        let mut round = verify_decided_fat_pointer_quorum(
            &target.block,
            &target.fat_pointer,
            &current_roster,
            vote_namespace,
            target.proposal_sigs.clone(),
        )?;
        round.proposal_valid_round = target.proposal_valid_round;
        tenderlink::verify_reconstructed_proposal_manifest(
            &HashKeys::default(),
            &round,
        )?;
        populate_reconstructed_round_counts(
            &mut round,
            &current_roster,
            &target.fat_pointer,
        )?;
        if round.height != height {
            return Err("authenticated historical round has the wrong height".into());
        }
        Ok(Some(round))
    })
    .await
    .map_err(|error| format!("historical PoS loader task failed: {error}"))?
}

fn is_exact_strict_frame_prefix(tail: &[u8], complete_frame: &[u8]) -> bool {
    tail.len() < complete_frame.len() && complete_frame.starts_with(tail)
}

fn append_pos_store_decision(
    internal: &mut TFLServiceInternal,
    new_block: &BftBlock,
    fat_pointer: &FatPointerToBftBlock,
    next_finalizers: &[RosterMember],
    proposal_valid_round: i64,
    tender_proposal_sigs: &[TMSig],
) -> Result<Option<PosStoreAppendReceipt>, String> {
    if internal.path_to_pos_store_file.as_os_str().is_empty() {
        return Ok(None);
    }
    let append_bytes = encode_pos_store_v2_frame(
        new_block,
        fat_pointer,
        next_finalizers,
        proposal_valid_round,
        tender_proposal_sigs,
    )?;

    let torn_tail = internal.pos_store_unverified_tail.clone();

    let file = internal
        .pos_store_file
        .as_mut()
        .ok_or("configured PoS store is not held exclusively")?;
    let durable_end = file
        .seek(SeekFrom::End(0))
        .map_err(|error| format!("failed to seek PoS store: {error}"))?;
    let (start_offset, original_tail) = if let Some(torn) = &torn_tail {
        if torn.offset.checked_add(torn.bytes.len() as u64) != Some(durable_end) {
            return Err("quarantined PoS tail does not end at the durable EOF".into());
        }
        if !is_exact_strict_frame_prefix(&torn.bytes, &append_bytes) {
            return Err(
                "quarantined PoS tail is not an exact strict prefix of this certified decision"
                    .into(),
            );
        }
        file.seek(SeekFrom::Start(torn.offset))
            .map_err(|error| format!("failed to seek exact PoS-tail repair: {error}"))?;
        (torn.offset, Some(torn.bytes.clone()))
    } else {
        (durable_end, None)
    };
    let write_result = (|| -> Result<[u8; 32], String> {
        file.write_all(&append_bytes)
            .map_err(|error| format!("failed to append PoS decision: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("failed to sync PoS decision: {error}"))?;
        file.seek(SeekFrom::Start(start_offset))
            .map_err(|error| format!("failed to seek PoS decision readback: {error}"))?;
        let mut readback = vec![0u8; append_bytes.len()];
        file.read_exact(&mut readback)
            .map_err(|error| format!("failed to reread PoS decision: {error}"))?;
        if readback != append_bytes {
            return Err("durable PoS decision differs from its append bytes".into());
        }
        let reread = decode_complete_pos_store_v2_frame(&readback)?;
        if reread.block != *new_block || reread.fat_pointer != *fat_pointer {
            return Err("durably reread BFT decision differs from the certified value".into());
        }
        if reread.next_roster != next_finalizers {
            return Err("durably reread next roster differs from finalized state".into());
        }
        if reread.proposal_valid_round != proposal_valid_round
            || reread.proposal_sigs != tender_proposal_sigs
        {
            return Err("durable proposal context changed during readback".into());
        }
        Ok(reread.block.blake3_hash().0)
    })();

    match write_result {
        Ok(hash) => {
            internal.pos_store_unverified_tail = None;
            file.seek(SeekFrom::End(0))
                .map_err(|error| format!("failed to restore PoS append position: {error}"))?;
            Ok(Some(PosStoreAppendReceipt {
                durable_parent_commit: hash,
                offset: start_offset,
                len: u64::try_from(append_bytes.len())
                    .map_err(|_| "PoS decision frame length does not fit u64")?,
            }))
        }
        Err(error) => {
            let rollback = (|| -> std::io::Result<()> {
                file.set_len(start_offset)?;
                file.seek(SeekFrom::Start(start_offset))?;
                if let Some(bytes) = &original_tail {
                    file.write_all(bytes)?;
                }
                file.sync_all()?;
                file.seek(SeekFrom::End(0))?;
                Ok(())
            })();
            match rollback {
                Ok(()) => Err(error),
                Err(rollback_error) => Err(format!(
                    "{error}; failed to roll back the torn PoS-store append: {rollback_error}"
                )),
            }
        }
    }
}

async fn apply_verified_decided_bft_block(
    tfl_handle: &TFLServiceHandle,
    new_block: &BftBlock,
    fat_pointer: &FatPointerToBftBlock,
    proposal_valid_round: i64,
    tender_proposal_sigs: Vec<TMSig>,
) -> Result<(Vec<tenderlink::SortedRosterMember>, [u8; 32], Option<[u8; 32]>), String> {
    let _decision_guard = tfl_handle.decision_apply_gate.lock().await;
    let call = tfl_handle.call.clone();
    let (current_finalizers, previous_final, expected_tip, expected_height) = {
        let internal = tfl_handle.internal.lock().await;
        (
            internal.finalizers_at_current_height.clone(),
            internal.latest_final_block,
            internal.fat_pointer_to_tip.clone(),
            internal.bft_blocks.len(),
        )
    };
    if new_block.height as usize != expected_height {
        return Err("decided BFT block height is not the next chain index".into());
    }
    if new_block.previous_block_fat_ptr.points_at_block_hash()
        != expected_tip.points_at_block_hash()
    {
        return Err("decided BFT block does not extend the current certified tip".into());
    }
    let previous_final_height = previous_final.map(|(height, _)| height);
    let terminated = terminated_finalizers_at(
        &tfl_handle.config.hardforks,
        new_block.height as u64,
        previous_final_height.map_or(0, |height| height.0 as u64),
    );
    let current_roster = tenderlink_roster_from_internal(&current_finalizers, &terminated);
    let vote_namespace = namespace_for_bft_height(
        &tfl_handle.config.hardforks,
        new_block.height as u64,
    );
    let mut decided_round = verify_decided_fat_pointer_quorum(
        new_block,
        fat_pointer,
        &current_roster,
        vote_namespace,
        tender_proposal_sigs.clone(),
    )?;
    decided_round.proposal_valid_round = proposal_valid_round;
    if tender_proposal_sigs.is_empty() {
        if proposal_valid_round != -1 {
            return Err("proposal valid_round requires a complete proposal manifest".into());
        }
    } else {
        tenderlink::verify_reconstructed_proposal_manifest(
            &HashKeys::default(),
            &decided_round,
        )?;
    }

    if validate_bft_block(tfl_handle, new_block).await
        != (tenderlink::TMStatus::Pass, tenderlink::TMStatusReason::None)
    {
        return Err("decided BFT block failed commit-time semantic validation".into());
    }
    let (new_final_height, new_final_hash) =
        validated_pow_header_chain(&call, new_block, previous_final_height).await?;

    let response = bounded_state_call(
        &call,
        StateRequest::CrosslinkFinalizeBlock(new_final_hash),
        "crosslink finalization",
    )
    .await?;
    let StateResponse::CrosslinkFinalized(finalized_hash, aggregated_stakes) = response else {
        return Err("crosslink finalization returned the wrong response type".into());
    };
    if finalized_hash != new_final_hash {
        return Err("state finalized a different PoW hash".into());
    }
    let next_finalizers = if aggregated_stakes.is_empty() {
        if expected_height == 0 {
            current_finalizers
        } else {
            return Err("state returned an empty bonded roster after non-genesis finalization".into());
        }
    } else {
        aggregated_stakes
            .into_iter()
            .map(|(pub_key, voting_power)| RosterMember {
                pub_key,
                voting_power,
                txids: Vec::new(),
            })
            .collect()
    };
    let next_bft_height = (new_block.height as u64)
        .checked_add(1)
        .ok_or("BFT height overflow")?;
    let next_terminated = terminated_finalizers_at(
        &tfl_handle.config.hardforks,
        next_bft_height,
        new_final_height.0 as u64,
    );
    let next_roster = tenderlink_roster_from_internal(&next_finalizers, &next_terminated);
    tenderlink::validate_consensus_roster(&next_roster)?;
    let next_vote_namespace =
        namespace_for_bft_height(&tfl_handle.config.hardforks, next_bft_height);

    let mut internal = tfl_handle.internal.lock().await;
    if internal.bft_blocks.len() != expected_height
        || internal.fat_pointer_to_tip != expected_tip
        || internal.latest_final_block != previous_final
        || internal.bft_height_by_hash.len() != expected_height
    {
        return Err("BFT tip changed while the decided block was being applied".into());
    }
    let new_block_hash = new_block.blake3_hash().0;
    if internal.bft_height_by_hash.contains_key(&new_block_hash) {
        return Err("decided BFT block hash already exists at another height".into());
    }
    if !internal.path_to_pos_store_file.as_os_str().is_empty()
        && internal.pos_store_records.len() != expected_height
    {
        return Err("PoS record index is not aligned with the decided BFT height".into());
    }
    if !internal.path_to_pos_store_file.as_os_str().is_empty()
        && internal.pos_store_read_file.is_none()
    {
        return Err("configured PoS store has no held positional-read handle".into());
    }
    let append_receipt = append_pos_store_decision(
        &mut internal,
        new_block,
        fat_pointer,
        &next_finalizers,
        proposal_valid_round,
        &tender_proposal_sigs,
    )?;
    let durable_parent_commit = append_receipt.map(|receipt| receipt.durable_parent_commit);
    if let Some(receipt) = append_receipt {
        internal.pos_store_records.push(PosStoreRecordIndex {
            offset: receipt.offset,
            len: receipt.len,
            finalized_bc_height: new_final_height.0,
        });
    }
    internal.bft_blocks.push(new_block.clone());
    let replaced = internal
        .bft_height_by_hash
        .insert(new_block_hash, expected_height);
    debug_assert!(replaced.is_none());
    internal.fat_pointer_to_tip = fat_pointer.clone();
    internal.latest_final_block = Some((new_final_height, new_final_hash));
    internal.current_bc_final = Some((new_final_height, new_final_hash));
    internal.finalizers_at_current_height = next_finalizers;
    internal.pending_reflush = Some(new_final_hash);
    drop(internal);

    match bounded_crosslink_reflush(
        &call,
        new_final_hash,
        "post-install crosslink reflush",
    )
    .await
    {
        Ok(()) => {
            let mut internal = tfl_handle.internal.lock().await;
            if internal.pending_reflush == Some(new_final_hash) {
                internal.pending_reflush = None;
            }
        }
        Err(error) => {
            warn!(%error, "post-install crosslink reflush remains pending for bounded retry");
        }
    }
    info!(
        "Applied certified BFT block {} and crosslink-finalized {}",
        new_block.height, new_final_hash
    );
    Ok((next_roster, next_vote_namespace, durable_parent_commit))
}

#[cfg(any())]
async fn handle_new_decided_bft_block(
    tfl_handle: &TFLServiceHandle,
    new_block: &BftBlock,
    fat_pointer: &FatPointerToBftBlock,
    tender_proposal_sigs: Vec<TMSig>,
) -> (Vec<tenderlink::SortedRosterMember>, [u8; 32], Option<[u8; 32]>) {
    // CHECK PRECONDITIONS
    {
        if fat_pointer.points_at_block_hash() != new_block.blake3_hash() {
            error!(
                "Fat Pointer hash does not match block hash. fp: {} block: {}",
                fat_pointer.points_at_block_hash(),
                new_block.blake3_hash()
            );
            panic!();
        }
        // TODO: check public keys on the fat pointer against the roster
        // Vote namespacing: the precommit signatures were made at this block's height with that
        // height's namespace folded in, so verify with the same namespace.
        let vote_namespace = namespace_for_bft_height(&tfl_handle.config.hardforks, new_block.height as u64);
        if fat_pointer.validate_signatures(&vote_namespace) == false {
            error!("Signatures are not valid. Rejecting block.");
            panic!();
        }

        assert_eq!(validate_bft_block(&tfl_handle, new_block).await,
            (tenderlink::TMStatus::Pass, tenderlink::TMStatusReason::None)
        );
    }

    let call = tfl_handle.call.clone();
    #[cfg(any())]
    {
    let new_final_hash = ZebBlockHash(BlockHash::from_header_data(new_block.headers.first().expect("at least 1 header")).0);
    let new_final_height = block_height_from_hash(&call, new_final_hash).await.unwrap();
    // `height` is now the 0-based canonical height, i.e. the chain index directly.
    let insert_i = new_block.height as usize;

    let mut internal = tfl_handle.internal.lock().await;

    // HACK: ensure there are enough blocks to overwrite this at the correct index
    for i in internal.bft_blocks.len()..=insert_i {
        let parent_i = i.saturating_sub(1); // just a simple chain
        internal.bft_blocks.push(BftBlock {
            version: 0,
            height: i as u32,
            previous_block_fat_ptr: FatPointerToBftBlock {
                vote_for_block_without_finalizer_public_key: [0u8; 76 - 32],
                signatures: Vec::new(),
            },
            headers: Vec::new(),
            hardforks: Vec::new(),
            do_not_include_until_bc_height: 0,
        });
    }

    if insert_i > 0 {
        assert_eq!(
            internal.bft_blocks[insert_i - 1].blake3_hash(),
            new_block.previous_block_fat_ptr.points_at_block_hash()
        );
    }
    assert!(insert_i == 0 || new_block.previous_block_hash() != Blake3Hash([0u8; 32]));
    assert!(
        internal.bft_blocks[insert_i].headers.is_empty(),
        "{:?}",
        internal.bft_blocks[insert_i]
    );
    assert!(!new_block.headers.is_empty());
    // info!("Inserting bft block at {} with hash {}", insert_i, new_block.blake3_hash());
    internal.bft_blocks[insert_i] = new_block.clone();
    internal.fat_pointer_to_tip = fat_pointer.clone();
    internal.latest_final_block = Some((new_final_height, new_final_hash));

    drop(internal); // Note(Sam): IT IS VERY IMPORTANT THAT WE DROP THE LOCK BECAUSE ZEBRA_STATE MAY CALL US BACK
    let got_stakes = loop {
        match (call.state)(zebra_state::Request::CrosslinkFinalizeBlock(new_final_hash)).await {
            Ok(zebra_state::Response::CrosslinkFinalized(hash, aggregated_stakes)) => {
                info!("Successfully crosslink-finalized {}, active stakes: {:?}", hash, aggregated_stakes);
                assert_eq!(
                    hash, new_final_hash,
                    "PoW finalized hash should now match ours"
                );
                break aggregated_stakes;
            }
            Ok(_) => unreachable!("wrong response type"),
            Err(err) => {
                error!(?err);
                warn!("I'm just going to sleep for one second and try the race condition again.");
                tokio::time::sleep(std::time::Duration::from_secs(1)).await;
            }
        }
    };

    internal = tfl_handle.internal.lock().await;

    if got_stakes.len() > 0 {
        internal.finalizers_at_current_height = got_stakes.into_iter().map(|s| RosterMember { pub_key: s.0, voting_power: s.1, txids: Vec::new() }).collect();
    } else {
        let mut any_non_zero = false;
        for val in &internal.finalizers_at_current_height {
            if val.voting_power > 1 {
                any_non_zero = true;
            }
        }
        if any_non_zero {
            panic!("We must never get zero stakes except at init!");
        }
    }

//println!("Storing pow ({:?}, {:?}) with roster: {:?}", new_final_height, new_final_hash, internal.finalizers_at_current_height);
    let (durable_parent_commit, durable_next_finalizers) = if internal.path_to_pos_store_file.to_str() != Some("") {
        assert!((internal.finalizers_at_current_height.len() as u64) <= MAX_POS_STORE_ROSTER_MEMBERS,
            "next PoS roster exceeds the durable store bound");
        assert!((tender_proposal_sigs.len() as u64) <= MAX_POS_STORE_PROPOSAL_SIGNATURES,
            "proposal-signature set exceeds the durable store bound");
        let mut append_bytes: Vec<u8> = Vec::new();
        new_block.zcash_serialize(&mut append_bytes).unwrap();
        fat_pointer.zcash_serialize(&mut append_bytes).unwrap();
        append_bytes.extend_from_slice(&(internal.finalizers_at_current_height.len() as u64).to_le_bytes());
        for v in &internal.finalizers_at_current_height {
            v.write_to_vec(&mut append_bytes);
        }
        append_bytes.extend_from_slice(&(tender_proposal_sigs.len() as u64).to_le_bytes());
        for sig in &tender_proposal_sigs {
            append_bytes.extend_from_slice(&sig.0);
        }
        let (reread_hash, reread_roster) = {
            let file = internal.pos_store_file.as_mut()
                .expect("configured PoS store must remain exclusively open for the service lifetime");
            let start_offset = file.seek(SeekFrom::End(0)).unwrap();
            file.write_all(&append_bytes).unwrap();
            file.sync_all().unwrap();
            file.seek(SeekFrom::Start(start_offset)).unwrap();
            let mut readback = vec![0u8; append_bytes.len()];
            file.read_exact(&mut readback).unwrap();
            assert_eq!(readback, append_bytes, "durable PoS-store readback differs from appended decision");
            let mut cursor = Cursor::new(&readback);
            let reread_block = BftBlock::zcash_deserialize(&mut cursor).unwrap();
            assert_eq!(reread_block, *new_block, "durably reread BFT block differs from decided block");
            let reread_fat_pointer = FatPointerToBftBlock::zcash_deserialize(&mut cursor).unwrap();
            assert_eq!(reread_fat_pointer, *fat_pointer, "durably reread fat pointer differs from decided certificate");

            let mut count_bytes = [0u8; 8];
            cursor.read_exact(&mut count_bytes).unwrap();
            let reread_roster_count = u64::from_le_bytes(count_bytes);
            assert!(reread_roster_count <= MAX_POS_STORE_ROSTER_MEMBERS,
                "durably reread roster exceeds the PoS-store bound");
            let mut reread_roster = Vec::with_capacity(reread_roster_count as usize);
            for _ in 0..reread_roster_count {
                reread_roster.push(RosterMember::read_from(&mut cursor).unwrap());
            }
            assert_eq!(reread_roster, internal.finalizers_at_current_height,
                "durably reread next roster differs from the finalized state roster");

            cursor.read_exact(&mut count_bytes).unwrap();
            let reread_signature_count = u64::from_le_bytes(count_bytes);
            assert!(reread_signature_count <= MAX_POS_STORE_PROPOSAL_SIGNATURES,
                "durably reread proposal-signature set exceeds the PoS-store bound");
            let mut reread_signatures = Vec::with_capacity(reread_signature_count as usize);
            for _ in 0..reread_signature_count {
                let mut signature = TMSig::NIL;
                cursor.read_exact(&mut signature.0).unwrap();
                reread_signatures.push(signature);
            }
            assert_eq!(reread_signatures, tender_proposal_sigs,
                "durably reread proposal signatures differ from the decided record");
            assert_eq!(cursor.position(), readback.len() as u64,
                "durable PoS-store decision record has trailing or unparsed bytes");
            (reread_block.blake3_hash().0, reread_roster)
        };
        (Some(reread_hash), reread_roster)
    } else {
        (None, internal.finalizers_at_current_height.clone())
    };

    // The returned roster is for the NEXT height (tenderlink advances to it after this decision):
    // its index is the new chain length. Exclude finalizers terminated at that height, inclusive,
    // so they are already out of the roster that will vote on a hardfork block scheduled there.
    let next_bft_height = internal.bft_blocks.len() as u64;
    let terminated = terminated_finalizers_at(&tfl_handle.config.hardforks, next_bft_height, new_final_height.0 as u64);
    let next_vote_namespace = namespace_for_bft_height(&tfl_handle.config.hardforks, next_bft_height);
    (
        tenderlink_roster_from_internal(
            &durable_next_finalizers,
            &terminated,
        ),
        next_vote_namespace,
        durable_parent_commit,
    )
}
}

/// Build the tenderlink consensus roster from the internal roster, excluding any finalizer in
/// `terminated` (terminated by a user-led hardfork; see [`terminated_finalizers_at`]). The
/// filtering is a pure membership test, mirroring how the viz excludes terminated finalizers.
/// Pass an empty set to build the roster unfiltered.
fn tenderlink_roster_from_internal(
    vals: &[RosterMember],
    terminated: &HashSet<PubKeyID>,
) -> Vec<SortedRosterMember> {
    let mut ret: Vec<SortedRosterMember> = vals
        .iter()
        .map(|v| SortedRosterMember {
            // Consensus keys are raw bond identities. Byte-reversed twins are distinct
            // identities and must never be normalized relative to this node.
            pub_key: PubKeyID(v.pub_key.into()),
            stake: v.voting_power,
            cumulative_stake: 0,
        })
        .filter(|m| !terminated.contains(&m.pub_key))
        .collect();

    // Roster needs to be sorted for various reasons, including determining who is under max, and
    // giving a consistent index that can be used to represent a roster member.
    // Needs to uniquely & stably tie-break members with the same stake, so that everyone has
    // exactly the same view regardless of whether they found out about members in different
    // orders... so we use pub_key.
    //
    // We separately keep track of cumulative stake to make weighted round-robin easy.
    // fn prepare_roster(roster: &mut [SortedRosterMember])
    {
        ret.sort_by_key(|m: &SortedRosterMember| std::cmp::Reverse((m.stake, m.pub_key)));
        debug_assert!(ret.is_sorted_by(|a, b| a.stake >= b.stake)); // descending

        let mut cumulative_stake = 0;
        for m in &mut ret {
            cumulative_stake += m.stake;
            m.cumulative_stake = cumulative_stake;
        }
    }

    ret
}

pub(crate) fn bootstrap_roster_from_config(
    config: &crate::config::Config,
) -> Result<Vec<RosterMember>, String> {
    let mut seen = HashSet::new();
    let mut roster = Vec::with_capacity(config.bootstrap_bft_roster.len());
    for member in &config.bootstrap_bft_roster {
        let public_key = decode_consensus_public_key_hex(&member.consensus_public_key)?;
        if member.voting_power == 0 {
            return Err(format!("bootstrap roster member {public_key} has zero voting power"));
        }
        if !seen.insert(public_key) {
            return Err(format!("bootstrap roster contains duplicate key {public_key}"));
        }
        roster.push(RosterMember {
            pub_key: public_key.0,
            voting_power: member.voting_power,
            txids: Vec::new(),
        });
    }
    Ok(roster)
}

fn finalizer_peer_addresses_from_explicit_config(
    configured_peers: &[crate::config::BftPeerIdentity],
    public_address: &str,
    my_public_key: PubKeyID,
    my_noise_keypair: &tenderlink::bandwidth_test::IdentityKeyPair,
) -> Result<Vec<tenderlink::FinalizerPeerAddress>, String> {
    use tenderlink::bandwidth_test::STPAddress;

    let mut configured_by_key = std::collections::BTreeMap::new();
    for peer in configured_peers {
        let public_key = decode_consensus_public_key_hex(&peer.consensus_public_key)?;
        let noise_public_key = decode_exact_lower_hex_32_named(
            &peer.noise_public_key,
            "peer Noise public key",
            true,
        )?;
        let (ip, port) = tenderlink::parse_to_ipv6_bytes(&peer.address)
            .map_err(|error| format!("invalid peer endpoint {}: {error}", peer.address))?;
        let address = STPAddress {
            ip,
            port,
            magic1: tenderlink::CRYPTO_MAGIC,
            key: noise_public_key.to_vec(),
        };
        if configured_by_key.insert(public_key, address).is_some() {
            return Err(format!("duplicate peer consensus key {public_key}"));
        }
    }
    let (local_ip, local_port) = tenderlink::parse_to_ipv6_bytes(public_address)
        .map_err(|error| format!("invalid local validator endpoint {public_address}: {error}"))?;
    let local_address = STPAddress::from(local_ip, local_port, my_noise_keypair);
    if let Some(configured_local) = configured_by_key.insert(my_public_key, local_address.clone()) {
        if configured_local != local_address {
            return Err(
                "local validator peer binding conflicts with its asserted endpoint or Noise key"
                    .into(),
            );
        }
    }
    Ok(configured_by_key
        .into_iter()
        .map(|(bft_pk, address)| {
            tenderlink::FinalizerPeerAddress { bft_pk, address }
        })
        .collect())
}

async fn validate_bft_block(
    tfl_handle: &TFLServiceHandle,
    new_block: &BftBlock,
) -> (tenderlink::TMStatus, tenderlink::TMStatusReason) {
    let mut internal = tfl_handle.internal.lock().await;
    let call = tfl_handle.call.clone();

    if new_block.headers.is_empty() {
        warn!("BFT block has no PoW finalization-candidate header");
        return (tenderlink::TMStatus::Fail, tenderlink::TMStatusReason::None);
    }

    if new_block.previous_block_fat_ptr.points_at_block_hash()
        != internal.fat_pointer_to_tip.points_at_block_hash()
    {
        warn!(
            "Block has invalid previous block fat pointer hash: was {} but should be {}",
            new_block.previous_block_fat_ptr.points_at_block_hash(),
            internal.fat_pointer_to_tip.points_at_block_hash(),
        );
        return (tenderlink::TMStatus::Fail, tenderlink::TMStatusReason::None);
    }

    // The linkage check above guarantees the parent is the current tip, so this block
    // will occupy the next index — which is its canonical 0-based height.
    let bft_height = internal.bft_blocks.len() as u64;

    // The self-reported height must match the position the block will occupy.
    if new_block.height as u64 != bft_height {
        warn!(
            "BFT block height {} does not match its chain position {}",
            new_block.height, bft_height,
        );
        return (tenderlink::TMStatus::Fail, tenderlink::TMStatusReason::None);
    }

    let parent = internal.bft_blocks.last();
    let parent_version = parent.map_or(0, |p| p.version);
    let parent_do_not_include = parent.map_or(0, |p| p.do_not_include_until_bc_height);

    // Version must be monotonic non-decreasing along the chain. (All v1 blocks share
    // version 1, so this holds trivially for existing history.)
    if new_block.version < parent_version {
        warn!(
            "BFT block version {} is below its parent's version {}",
            new_block.version, parent_version,
        );
        return (tenderlink::TMStatus::Fail, tenderlink::TMStatusReason::None);
    }

    // The proposal must carry exactly the hardforks this node has scheduled at this BFT
    // height — byte-for-byte, in canonical (ascending pow_activation_height) order, and
    // none at all when none are scheduled — and set its do_not_include_until_bc_height
    // to the greatest activation height among them (pointing at this block commits to
    // every certificate in it, so the strictest one governs).
    //
    // This runs before the generic do_not_include_until_bc_height monotonicity check
    // below so that a regression *caused by the hardfork activation* is reported as its
    // own distinct error rather than the generic one.
    let scheduled_hardforks: Vec<&crate::config::HardForkConfig> = tfl_handle
        .config
        .hardforks
        .iter()
        .filter(|hf| hf.bft_certificate_height == bft_height)
        .collect();
    {
        let serialize = |hf: &crate::config::HardForkConfig| {
            let mut bytes = Vec::new();
            hf.zcash_serialize(&mut bytes).expect("serializing to a Vec is infallible");
            bytes
        };
        let proposal_matches = new_block.hardforks.len() == scheduled_hardforks.len()
            && new_block
                .hardforks
                .iter()
                .zip(scheduled_hardforks.iter())
                .all(|(carried, scheduled)| serialize(carried) == serialize(scheduled));
        if !proposal_matches {
            warn!(
                "BFT block at height {} must carry exactly the {} scheduled hardfork(s) byte-for-byte in schedule order, but does not",
                bft_height, scheduled_hardforks.len(),
            );
            return (tenderlink::TMStatus::Fail, tenderlink::TMStatusReason::None);
        }
    }

    // Rules are pow-sorted, so the last scheduled rule has the greatest activation.
    if let Some(last_scheduled) = scheduled_hardforks.last() {
        if new_block.do_not_include_until_bc_height != last_scheduled.pow_activation_height {
            warn!(
                "BFT hardfork block at height {} must set do_not_include_until_bc_height to the greatest carried pow_activation_height {}, but it is {}",
                bft_height, last_scheduled.pow_activation_height, new_block.do_not_include_until_bc_height,
            );
            return (tenderlink::TMStatus::Fail, tenderlink::TMStatusReason::None);
        }

        // Separate error: the hardfork's PoW activation height must not regress the
        // monotonic do_not_include_until_bc_height relative to the parent.
        if last_scheduled.pow_activation_height < parent_do_not_include {
            warn!(
                "Hardfork pow_activation_height {} at BFT height {} is below the parent's do_not_include_until_bc_height {}; the hardfork activation regresses do_not_include_until_bc_height",
                last_scheduled.pow_activation_height, bft_height, parent_do_not_include,
            );
            return (tenderlink::TMStatus::Fail, tenderlink::TMStatusReason::None);
        }
    }

    // do_not_include_until_bc_height must be monotonic non-decreasing. (v1 blocks use
    // the implicit value 0, so this holds for existing history.) In the hardfork case
    // the checks above already guarantee this passes.
    if new_block.do_not_include_until_bc_height < parent_do_not_include {
        warn!(
            "BFT block do_not_include_until_bc_height {} is below its parent's {}",
            new_block.do_not_include_until_bc_height, parent_do_not_include,
        );
        return (tenderlink::TMStatus::Fail, tenderlink::TMStatusReason::None);
    }

    // Captured before dropping the lock: an already-finalized hash we can safely use to kick
    // the state's non-finalized queue below without risking a premature finalization.
    let previous_final_height = internal.latest_final_block.map(|(height, _)| height);
    drop(internal);

    match validated_pow_header_chain(&call, new_block, previous_final_height).await {
        Ok(_) => (tenderlink::TMStatus::Pass, tenderlink::TMStatusReason::None),
        Err(error) => {
            warn!(%error, "BFT proposal failed canonical PoW ancestry validation");
            let is_transient = error.contains("timed out")
                || error.contains(" lookup failed:")
                || error.contains("is unavailable");
            if is_transient {
                let needed_hash = BlockHash::from_header_data(
                    new_block.headers.first().expect("non-empty checked above"),
                );
                (
                    tenderlink::TMStatus::Indeterminate,
                    tenderlink::TMStatusReason::NeedsBlock { hash: needed_hash.0 },
                )
            } else {
                (tenderlink::TMStatus::Fail, tenderlink::TMStatusReason::None)
            }
        }
    }
}

fn fat_pointer_to_block_at_height(
    bft_blocks: &[BftBlock],
    fat_pointer_to_tip: &FatPointerToBftBlock,
    at_height: u64,
) -> Option<FatPointerToBftBlock> {
    if at_height == 0 || at_height as usize - 1 >= bft_blocks.len() {
        return None;
    }

    if at_height as usize == bft_blocks.len() {
        Some(fat_pointer_to_tip.clone())
    } else {
        Some(
            bft_blocks[at_height as usize]
                .previous_block_fat_ptr
                .clone(),
        )
    }
}

async fn get_historical_bft_block_at_height(
    tfl_handle: &TFLServiceHandle,
    at_height: u64,
) -> Option<(BftBlock, FatPointerToBftBlock)> {
    let mut internal = tfl_handle.internal.lock().await;
    if at_height == 0 || at_height as usize - 1 >= internal.bft_blocks.len() {
        return None;
    }
    let block = internal.bft_blocks[at_height as usize - 1].clone();
    Some((
        block,
        fat_pointer_to_block_at_height(
            &internal.bft_blocks,
            &internal.fat_pointer_to_tip,
            at_height,
        )
        .unwrap(),
    ))
}

const MAIN_LOOP_SLEEP_INTERVAL: Duration = Duration::from_millis(125);
const MAIN_LOOP_INFO_DUMP_INTERVAL: Duration = Duration::from_millis(8000);
pub fn run_tfl_test(internal_handle: TFLServiceHandle) {
    // ensure that tests fail on panic/assert(false); otherwise tokio swallows them
    std::panic::set_hook(Box::new(|panic_info| {
        #[allow(clippy::print_stderr)]
        {
            *TEST_FAILED.lock().unwrap() = -1;

            use std::backtrace::{self, *};
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

            eprintln!("\n\nInstruction sequence:");
            dump_test_instrs();

            #[cfg(not(feature = "viz_gui"))]
            std::process::abort();
        }
    }));

    tokio::task::spawn(test_format::instr_reader(internal_handle));
}

/// Vote-namespacing domain separator for a BFT height: a flat blake3 hash of the prefix of
/// scheduled hardforks whose `bft_certificate_height <= bft_height` (inclusive of a hardfork at
/// `bft_height` itself), concatenated in canonical schedule order. An empty prefix yields
/// `[0; 32]` (nil), so the no-hardfork case is a backwards-compatible no-op in tenderlink's
/// signing. The schedule is sorted with non-decreasing `bft_certificate_height` (several rules
/// may share one certificate height), so the filtered set is exactly the prefix.
fn namespace_for_bft_height(hardforks: &[crate::config::HardForkConfig], bft_height: u64) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    let mut any = false;
    for hf in hardforks.iter().filter(|hf| hf.bft_certificate_height <= bft_height) {
        let mut bytes = Vec::new();
        hf.zcash_serialize(&mut bytes).expect("serializing to a Vec is infallible");
        hasher.update(&bytes);
        any = true;
    }
    if any { hasher.finalize().into() } else { [0u8; 32] }
}

fn deserialize_bft_block_exact(bytes: &[u8]) -> Result<BftBlock, String> {
    let mut cursor = Cursor::new(bytes);
    let block = BftBlock::zcash_deserialize(&mut cursor)
        .map_err(|error| format!("failed to deserialize BFT block: {error}"))?;
    if cursor.position() != bytes.len() as u64 {
        return Err("BFT block payload contains trailing bytes".into());
    }
    Ok(block)
}

fn decode_exact_lower_hex_32_named(
    value: &str,
    field: &'static str,
    reject_zero: bool,
) -> Result<[u8; 32], String> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)) {
        return Err(format!(
            "{field} must be exactly 64 lowercase hex characters"
        ));
    }
    let nibble = |byte: u8| -> u8 {
        match byte {
            b'0'..=b'9' => byte - b'0',
            b'a'..=b'f' => byte - b'a' + 10,
            _ => unreachable!("hex alphabet checked above"),
        }
    };
    let bytes = value.as_bytes();
    let mut decoded = [0u8; 32];
    for (index, output) in decoded.iter_mut().enumerate() {
        *output = (nibble(bytes[index * 2]) << 4) | nibble(bytes[index * 2 + 1]);
    }
    if reject_zero && decoded == [0u8; 32] {
        return Err(format!("{field} must not be zero"));
    }
    Ok(decoded)
}

fn decode_exact_lower_hex_32(value: &str) -> Result<[u8; 32], String> {
    decode_exact_lower_hex_32_named(value, "bootstrap receipt BLAKE3", true)
}

pub(crate) fn decode_consensus_public_key_hex(value: &str) -> Result<PubKeyID, String> {
    decode_exact_lower_hex_32_named(value, "consensus public key", true).map(PubKeyID)
}

#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct SignerMigrationReceiptV1 {
    schema: String,
    action: String,
    operator_authorized: bool,
    independent_anchor_authorized: bool,
    global_single_signer_fence_confirmed: bool,
    frozen_legacy_binary_sha256: String,
    frozen_legacy_config_sha256: String,
    composite_checkpoint_manifest_sha256: String,
    pos_store_sha256: String,
    pos_store_size_bytes: u64,
    pos_store_complete_eof: bool,
    pos_store_record_count: u64,
    pos_store_first_bft_height: u64,
    validator_consensus_public_key: String,
    chain_id: String,
    replayed_next_bft_height: u64,
    bootstrap_parent_commit: String,
    bootstrap_vote_namespace: String,
    bootstrap_consensus_config_hash: String,
    authenticated_bootstrap_roster_hash: String,
    active_roster_hash: String,
    active_roster_index: u32,
    active_roster_len: u32,
    finalized_pow_height: u32,
    finalized_pow_hash: String,
    peer_route_map_blake3: String,
    peer_route_voting_power: u64,
    required_route_voting_power: u64,
    legacy_signer_fence_receipt_sha256: String,
    wal_path: PathBuf,
    anchor_path: PathBuf,
    pos_store_path: PathBuf,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SignerJournalState {
    Uninitialized,
    Existing,
}

#[derive(Clone, Copy)]
struct SignerMigrationContext<'a> {
    validator_consensus_public_key: PubKeyID,
    chain_id: [u8; 32],
    startup_bft_height: u64,
    parent_commit: [u8; 32],
    vote_namespace: [u8; 32],
    consensus_config_hash: [u8; 32],
    authenticated_bootstrap_roster_hash: [u8; 32],
    active_roster_hash: [u8; 32],
    active_roster_index: u32,
    active_roster_len: u32,
    finalized_pow_height: u32,
    finalized_pow_hash: [u8; 32],
    pos_store_size_bytes: u64,
    pos_store_record_count: u64,
    pos_store_complete_eof: bool,
    wal_path: &'a Path,
    anchor_path: &'a Path,
    pos_store_path: &'a Path,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SignerStartupAuthority {
    non_genesis_receipt_hash: Option<[u8; 32]>,
}

fn signer_journal_file_len(path: &Path) -> Result<u64, String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
                return Err("signer journal path must be a regular non-symlink file".into());
            }
            Ok(metadata.len())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(format!("failed to inspect signer journal path: {error}")),
    }
}

fn signer_journal_state(wal_path: &Path, anchor_path: &Path) -> Result<SignerJournalState, String> {
    let wal_len = signer_journal_file_len(wal_path)?;
    let anchor_len = signer_journal_file_len(anchor_path)?;
    match (wal_len, anchor_len) {
        (0, 0) => Ok(SignerJournalState::Uninitialized),
        (wal, anchor) if wal > 0 && anchor > 0 => Ok(SignerJournalState::Existing),
        _ => Err("signer WAL and anchor initialization states differ".into()),
    }
}

fn read_sealed_signer_migration_receipt(path: &Path) -> Result<Vec<u8>, String> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| format!("failed to inspect signer migration receipt: {error}"))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err("signer migration receipt must be a regular non-symlink file".into());
    }
    if metadata.len() == 0 || metadata.len() > MAX_SIGNER_MIGRATION_RECEIPT_BYTES {
        return Err("signer migration receipt size is outside the accepted bound".into());
    }

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use nix::fcntl::OFlag;
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags((OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC).bits());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.share_mode(0);
    }
    let mut file = options
        .open(path)
        .map_err(|error| format!("failed to open signer migration receipt: {error}"))?;
    let opened_metadata = file
        .metadata()
        .map_err(|error| format!("failed to stat signer migration receipt: {error}"))?;
    if !opened_metadata.file_type().is_file() || opened_metadata.len() != metadata.len() {
        return Err("signer migration receipt changed while it was opened".into());
    }
    #[cfg(unix)]
    {
        use nix::unistd::geteuid;
        use std::os::unix::fs::MetadataExt;
        if opened_metadata.uid() != geteuid().as_raw() {
            return Err("signer migration receipt owner mismatch".into());
        }
        if opened_metadata.mode() & 0o077 != 0 {
            return Err("signer migration receipt permissions are broader than 0600".into());
        }
        if opened_metadata.nlink() != 1 {
            return Err("signer migration receipt has unexpected hard links".into());
        }
    }

    let mut bytes = Vec::with_capacity(opened_metadata.len() as usize);
    std::io::Read::by_ref(&mut file)
        .take(MAX_SIGNER_MIGRATION_RECEIPT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("failed to read signer migration receipt: {error}"))?;
    if bytes.len() as u64 != opened_metadata.len()
        || bytes.len() as u64 > MAX_SIGNER_MIGRATION_RECEIPT_BYTES
    {
        return Err("signer migration receipt changed or exceeded its bound while read".into());
    }
    Ok(bytes)
}

fn receipt_hash_field(value: &str, field: &'static str) -> Result<[u8; 32], String> {
    decode_exact_lower_hex_32_named(value, field, true)
}

fn require_receipt_hash(
    value: &str,
    field: &'static str,
    expected: [u8; 32],
) -> Result<(), String> {
    if receipt_hash_field(value, field)? != expected {
        return Err(format!("signer migration receipt {field} mismatch"));
    }
    Ok(())
}

fn verify_signer_migration_receipt(
    bytes: &[u8],
    pinned_hash: [u8; 32],
    journal_state: SignerJournalState,
    context: &SignerMigrationContext<'_>,
) -> Result<SignerStartupAuthority, String> {
    let actual_hash: [u8; 32] = blake3::hash(bytes).into();
    if actual_hash != pinned_hash {
        return Err("signer migration receipt bytes do not match the configured BLAKE3".into());
    }
    let receipt: SignerMigrationReceiptV1 = serde_json::from_slice(bytes)
        .map_err(|error| format!("signer migration receipt JSON is invalid: {error}"))?;
    if receipt.schema != SIGNER_MIGRATION_RECEIPT_SCHEMA
        || receipt.action != SIGNER_MIGRATION_RECEIPT_ACTION
    {
        return Err("signer migration receipt schema or action mismatch".into());
    }
    if !receipt.operator_authorized
        || !receipt.independent_anchor_authorized
        || !receipt.global_single_signer_fence_confirmed
    {
        return Err("signer migration receipt lacks explicit operator/fence authority".into());
    }
    for (value, field) in [
        (receipt.frozen_legacy_binary_sha256.as_str(), "frozen legacy binary SHA256"),
        (receipt.frozen_legacy_config_sha256.as_str(), "frozen legacy config SHA256"),
        (
            receipt.composite_checkpoint_manifest_sha256.as_str(),
            "composite checkpoint manifest SHA256",
        ),
        (receipt.pos_store_sha256.as_str(), "PoS store SHA256"),
        (
            receipt.legacy_signer_fence_receipt_sha256.as_str(),
            "legacy signer fence receipt SHA256",
        ),
        (receipt.peer_route_map_blake3.as_str(), "peer route map BLAKE3"),
    ] {
        receipt_hash_field(value, field)?;
    }
    if receipt.pos_store_size_bytes == 0
        || !receipt.pos_store_complete_eof
        || receipt.pos_store_record_count == 0
        || receipt.pos_store_first_bft_height != 0
        || receipt.replayed_next_bft_height != receipt.pos_store_record_count
    {
        return Err("signer migration receipt does not bind a complete height-zero PoS history".into());
    }
    if !context.pos_store_complete_eof
        || receipt.pos_store_size_bytes > context.pos_store_size_bytes
        || receipt.pos_store_record_count > context.pos_store_record_count
        || receipt.replayed_next_bft_height > context.startup_bft_height
    {
        return Err("signer migration receipt PoS checkpoint is not an ancestor of loaded history".into());
    }
    if receipt.required_route_voting_power == 0
        || receipt.peer_route_voting_power < receipt.required_route_voting_power
    {
        return Err("signer migration receipt lacks required authenticated peer-route stake".into());
    }
    if decode_consensus_public_key_hex(&receipt.validator_consensus_public_key)?
        != context.validator_consensus_public_key
    {
        return Err("signer migration receipt validator consensus key mismatch".into());
    }
    require_receipt_hash(&receipt.chain_id, "chain ID", context.chain_id)?;
    require_receipt_hash(
        &receipt.bootstrap_consensus_config_hash,
        "consensus config hash",
        context.consensus_config_hash,
    )?;
    require_receipt_hash(
        &receipt.authenticated_bootstrap_roster_hash,
        "authenticated bootstrap roster hash",
        context.authenticated_bootstrap_roster_hash,
    )?;
    if receipt.wal_path != context.wal_path
        || receipt.anchor_path != context.anchor_path
        || receipt.pos_store_path != context.pos_store_path
    {
        return Err("signer migration receipt journal or PoS-store path mismatch".into());
    }

    match journal_state {
        SignerJournalState::Uninitialized => {
            if receipt.replayed_next_bft_height != context.startup_bft_height
                || receipt.pos_store_size_bytes != context.pos_store_size_bytes
                || receipt.pos_store_record_count != context.pos_store_record_count
                || receipt.active_roster_index != context.active_roster_index
                || receipt.active_roster_len != context.active_roster_len
                || receipt.finalized_pow_height != context.finalized_pow_height
            {
                return Err("signer migration receipt bootstrap counters mismatch".into());
            }
            require_receipt_hash(
                &receipt.bootstrap_parent_commit,
                "bootstrap parent commit",
                context.parent_commit,
            )?;
            require_receipt_hash(
                &receipt.bootstrap_vote_namespace,
                "bootstrap vote namespace",
                context.vote_namespace,
            )?;
            require_receipt_hash(
                &receipt.active_roster_hash,
                "active roster hash",
                context.active_roster_hash,
            )?;
            require_receipt_hash(
                &receipt.finalized_pow_hash,
                "finalized PoW hash",
                context.finalized_pow_hash,
            )?;
        }
        SignerJournalState::Existing => {
            if receipt.active_roster_len == 0
                || receipt.active_roster_index >= receipt.active_roster_len
                || receipt.finalized_pow_height > context.finalized_pow_height
            {
                return Err("signer migration receipt origin counters are invalid".into());
            }
            receipt_hash_field(&receipt.bootstrap_parent_commit, "bootstrap parent commit")?;
            receipt_hash_field(&receipt.bootstrap_vote_namespace, "bootstrap vote namespace")?;
            receipt_hash_field(&receipt.active_roster_hash, "active roster hash")?;
            receipt_hash_field(&receipt.finalized_pow_hash, "finalized PoW hash")?;
        }
    }

    Ok(SignerStartupAuthority {
        non_genesis_receipt_hash: Some(pinned_hash),
    })
}

fn signer_startup_authority(
    config: &crate::config::Config,
    context: &SignerMigrationContext<'_>,
) -> Result<SignerStartupAuthority, String> {
    if context.wal_path == context.anchor_path {
        return Err("signer WAL and independent anchor paths must differ".into());
    }
    if !config.signer_independent_anchor_authorized {
        return Err("independent anti-rollback/key-fencing authority is absent".into());
    }
    if !context.pos_store_complete_eof {
        return Err("PoS history has an unverified torn tail".into());
    }
    let journal_state = signer_journal_state(context.wal_path, context.anchor_path)?;
    if context.startup_bft_height == 0 {
        if config.signer_non_genesis_bootstrap_receipt_blake3.is_some()
            || config.signer_non_genesis_bootstrap_receipt_path.is_some()
        {
            return Err("genesis signer startup must not supply a non-genesis receipt".into());
        }
        return Ok(SignerStartupAuthority {
            non_genesis_receipt_hash: None,
        });
    }

    let pinned_hash_text = config
        .signer_non_genesis_bootstrap_receipt_blake3
        .as_deref()
        .ok_or_else(|| "non-genesis signer startup receipt hash is absent".to_owned())?;
    let pinned_hash = decode_exact_lower_hex_32(pinned_hash_text)?;
    let receipt_path = config
        .signer_non_genesis_bootstrap_receipt_path
        .as_deref()
        .ok_or("non-genesis signer startup receipt path is absent")?;
    if receipt_path == context.wal_path
        || receipt_path == context.anchor_path
        || receipt_path == context.pos_store_path
    {
        return Err("signer migration receipt must be outside the WAL, anchor, and PoS store".into());
    }
    let bytes = read_sealed_signer_migration_receipt(receipt_path)?;
    verify_signer_migration_receipt(&bytes, pinned_hash, journal_state, context)
}

fn canonical_validator_identity_configured(
    config: &crate::config::Config,
) -> Result<bool, String> {
    let fields = [
        config.validator_signing_key_seed.is_some(),
        config.validator_consensus_public_key.is_some(),
        config.validator_noise_static_key_seed.is_some(),
    ];
    let configured = fields.iter().filter(|value| **value).count();
    if configured != 0 && configured != fields.len() {
        return Err("canonical validator identity is partially configured".into());
    }
    let complete = configured == fields.len();
    if complete && (config.explicit_bft_key_seed.is_some() || !config.bft_peers.is_empty()) {
        return Err(
            "legacy endpoint-derived identity fields cannot be mixed with canonical validator identity"
                .into(),
        );
    }
    Ok(complete)
}

struct StoredPosDecision {
    block: BftBlock,
    fat_pointer: FatPointerToBftBlock,
    next_roster: Vec<RosterMember>,
    proposal_valid_round: i64,
    proposal_sigs: Vec<TMSig>,
    is_v2: bool,
}

fn read_stored_pos_decision_payload<R: Read>(
    reader: &mut R,
    is_v2: bool,
) -> Result<StoredPosDecision, String> {
    let block = BftBlock::zcash_deserialize(&mut *reader)
        .map_err(|error| format!("stored BFT block is invalid or truncated: {error}"))?;
    let fat_pointer = FatPointerToBftBlock::zcash_deserialize(&mut *reader)
        .map_err(|error| format!("stored BFT certificate is invalid or truncated: {error}"))?;

    let mut count_bytes = [0u8; 8];
    reader
        .read_exact(&mut count_bytes)
        .map_err(|error| format!("stored next-roster count is truncated: {error}"))?;
    let roster_count = u64::from_le_bytes(count_bytes);
    if roster_count > MAX_POS_STORE_ROSTER_MEMBERS {
        return Err(format!(
            "stored next-roster count {roster_count} exceeds {MAX_POS_STORE_ROSTER_MEMBERS}"
        ));
    }
    let mut next_roster = Vec::with_capacity(roster_count as usize);
    for _ in 0..roster_count {
        next_roster.push(read_stored_roster_member(reader)?);
    }

    let proposal_valid_round = if is_v2 {
        reader
            .read_exact(&mut count_bytes)
            .map_err(|error| format!("stored proposal valid_round is truncated: {error}"))?;
        i64::from_le_bytes(count_bytes)
    } else {
        -1
    };
    validate_proposal_valid_round(
        proposal_valid_round,
        fat_pointer.get_vote_template().round,
    )?;

    reader
        .read_exact(&mut count_bytes)
        .map_err(|error| format!("stored proposal-signature count is truncated: {error}"))?;
    let proposal_sig_count = u64::from_le_bytes(count_bytes);
    if proposal_sig_count > MAX_POS_STORE_PROPOSAL_SIGNATURES {
        return Err(format!(
            "stored proposal-signature count {proposal_sig_count} exceeds {MAX_POS_STORE_PROPOSAL_SIGNATURES}"
        ));
    }
    let mut proposal_sigs = Vec::with_capacity(proposal_sig_count as usize);
    for _ in 0..proposal_sig_count {
        let mut signature = TMSig::NIL;
        reader
            .read_exact(&mut signature.0)
            .map_err(|error| format!("stored proposal signature is truncated: {error}"))?;
        proposal_sigs.push(signature);
    }

    Ok(StoredPosDecision {
        block,
        fat_pointer,
        next_roster,
        proposal_valid_round,
        proposal_sigs,
        is_v2,
    })
}

fn validate_stored_bft_semantics(
    config: &crate::config::Config,
    block: &BftBlock,
    parent: Option<&BftBlock>,
    expected_height: u64,
    expected_parent: &FatPointerToBftBlock,
) -> Result<(), String> {
    if block.headers.is_empty() {
        return Err("stored BFT block has no PoW finalization-candidate header".into());
    }
    if block.height as u64 != expected_height {
        return Err(format!(
            "stored BFT block height {} does not match index {expected_height}",
            block.height
        ));
    }
    if block.previous_block_fat_ptr.points_at_block_hash()
        != expected_parent.points_at_block_hash()
    {
        return Err("stored BFT block does not extend the preceding certified tip".into());
    }

    let parent_version = parent.map_or(0, |value| value.version);
    let parent_minimum = parent.map_or(0, |value| value.do_not_include_until_bc_height);
    if block.version < parent_version {
        return Err("stored BFT block version regresses its parent".into());
    }
    if block.do_not_include_until_bc_height < parent_minimum {
        return Err("stored BFT inclusion floor regresses its parent".into());
    }

    let scheduled: Vec<&crate::config::HardForkConfig> = config
        .hardforks
        .iter()
        .filter(|rule| rule.bft_certificate_height == expected_height)
        .collect();
    let serialize = |rule: &crate::config::HardForkConfig| -> Result<Vec<u8>, String> {
        let mut bytes = Vec::new();
        rule.zcash_serialize(&mut bytes)
            .map_err(|error| format!("failed to encode configured hardfork rule: {error}"))?;
        Ok(bytes)
    };
    if block.hardforks.len() != scheduled.len() {
        return Err("stored BFT block carries the wrong scheduled-hardfork count".into());
    }
    for (carried, expected) in block.hardforks.iter().zip(scheduled.iter()) {
        if serialize(carried)? != serialize(expected)? {
            return Err("stored BFT block carries a non-canonical hardfork rule".into());
        }
    }
    if let Some(last) = scheduled.last() {
        if block.do_not_include_until_bc_height != last.pow_activation_height {
            return Err("stored hardfork block carries the wrong inclusion floor".into());
        }
        if last.pow_activation_height < parent_minimum {
            return Err("stored hardfork activation regresses its parent inclusion floor".into());
        }
    }
    Ok(())
}

struct VerifiedPosReplay {
    file: File,
    rounds: Vec<tenderlink::RoundData>,
    records: Vec<PosStoreRecordIndex>,
    blocks: Vec<BftBlock>,
    tip: FatPointerToBftBlock,
    next_roster: Vec<RosterMember>,
    final_block: Option<(ZebBlockHeight, ZebBlockHash)>,
    torn_tail: Option<PosStoreTornTail>,
}

async fn replay_verified_pos_store(
    path: &Path,
    call: &TFLServiceCalls,
    config: &crate::config::Config,
    initial_roster: Vec<RosterMember>,
) -> Result<VerifiedPosReplay, String> {
    let (mut file, _) = open_exclusive_pos_store(path)?;
    let file_len = file
        .metadata()
        .map_err(|error| format!("failed to stat PoS store before replay: {error}"))?
        .len();
    let mut rounds = VecDeque::with_capacity(
        tenderlink::MAX_RECENT_COMMIT_ROUNDS_IN_MEMORY,
    );
    let mut records = Vec::new();
    let mut blocks: Vec<BftBlock> = Vec::new();
    let mut tip = FatPointerToBftBlock::null();
    let mut next_roster = initial_roster;
    let mut final_block: Option<(ZebBlockHeight, ZebBlockHash)> = None;
    let mut torn_tail = None;

    while file
        .stream_position()
        .map_err(|error| format!("failed reading PoS-store position: {error}"))?
        < file_len
    {
        let record_offset = file
            .stream_position()
            .map_err(|error| format!("failed reading PoS-store record position: {error}"))?;
        let remaining = file_len - record_offset;
        let prefix_len = usize::try_from(remaining.min(POS_STORE_V2_MAGIC.len() as u64))
            .map_err(|_| "PoS-store prefix length does not fit usize")?;
        let mut prefix = vec![0u8; prefix_len];
        file.read_exact(&mut prefix)
            .map_err(|error| format!("failed to inspect PoS-store record at byte {record_offset}: {error}"))?;
        file.seek(SeekFrom::Start(record_offset))
            .map_err(|error| format!("failed to rewind PoS-store record at byte {record_offset}: {error}"))?;

        let is_v2_prefix = POS_STORE_V2_MAGIC[..prefix_len] == prefix;
        let record = if remaining < POS_STORE_V2_MAGIC.len() as u64 && is_v2_prefix {
            let mut bytes = vec![0u8; remaining as usize];
            file.read_exact(&mut bytes)
                .map_err(|error| format!("failed to quarantine short PoS v2 tail: {error}"))?;
            torn_tail = Some(PosStoreTornTail { offset: record_offset, bytes });
            break;
        } else if prefix.as_slice() == POS_STORE_V2_MAGIC {
            if remaining < POS_STORE_V2_HEADER_LEN {
                let mut bytes = vec![0u8; remaining as usize];
                file.read_exact(&mut bytes)
                    .map_err(|error| format!("failed to quarantine PoS v2 header tail: {error}"))?;
                torn_tail = Some(PosStoreTornTail { offset: record_offset, bytes });
                break;
            }
            let mut header = [0u8; POS_STORE_V2_HEADER_LEN as usize];
            file.read_exact(&mut header)
                .map_err(|error| format!("failed to read PoS v2 header at byte {record_offset}: {error}"))?;
            let payload_len = u64::from_le_bytes(header[8..16].try_into().unwrap());
            if payload_len > MAX_POS_STORE_V2_PAYLOAD_BYTES {
                return Err(format!(
                    "PoS v2 record at byte {record_offset} declares an oversized payload"
                ));
            }
            let frame_len = POS_STORE_V2_HEADER_LEN
                .checked_add(payload_len)
                .ok_or("PoS v2 frame length overflows")?;
            file.seek(SeekFrom::Start(record_offset))
                .map_err(|error| format!("failed to rewind PoS v2 record: {error}"))?;
            if remaining < frame_len {
                let mut bytes = vec![0u8; remaining as usize];
                file.read_exact(&mut bytes)
                    .map_err(|error| format!("failed to quarantine PoS v2 payload tail: {error}"))?;
                torn_tail = Some(PosStoreTornTail { offset: record_offset, bytes });
                break;
            }
            let mut frame = vec![0u8; frame_len as usize];
            file.read_exact(&mut frame)
                .map_err(|error| format!("failed to read complete PoS v2 frame: {error}"))?;
            decode_complete_pos_store_v2_frame(&frame)
                .map_err(|error| format!("PoS v2 record at byte {record_offset} is rejected: {error}"))?
        } else {
            read_stored_pos_decision_payload(&mut file, false)
                .map_err(|error| format!("legacy PoS-store record at byte {record_offset} is rejected: {error}"))?
        };
        let record_end = file
            .stream_position()
            .map_err(|error| format!("failed reading PoS-store record end: {error}"))?;
        if record_end > file_len {
            return Err(format!(
                "PoS-store record at byte {record_offset} extends beyond the durable file"
            ));
        }

        let expected_height = blocks.len() as u64;
        validate_stored_bft_semantics(
            config,
            &record.block,
            blocks.last(),
            expected_height,
            &tip,
        )?;
        let previous_final_height = final_block.map(|(height, _)| height);
        let terminated = terminated_finalizers_at(
            &config.hardforks,
            expected_height,
            previous_final_height.map_or(0, |height| height.0 as u64),
        );
        let current_roster = tenderlink_roster_from_internal(&next_roster, &terminated);
        let vote_namespace = namespace_for_bft_height(&config.hardforks, expected_height);
        let mut round = verify_decided_fat_pointer_quorum(
            &record.block,
            &record.fat_pointer,
            &current_roster,
            vote_namespace,
            record.proposal_sigs.clone(),
        )
        .map_err(|error| format!("PoS-store certificate at byte {record_offset} is rejected: {error}"))?;
        round.proposal_valid_round = record.proposal_valid_round;
        let has_authenticated_proposal_context = record.is_v2 && !record.proposal_sigs.is_empty();
        if record.is_v2 && record.proposal_sigs.is_empty() && record.proposal_valid_round != -1 {
            return Err(format!(
                "PoS v2 record at byte {record_offset} has valid_round without a proposal manifest"
            ));
        }
        if has_authenticated_proposal_context {
            tenderlink::verify_reconstructed_proposal_manifest(
                &HashKeys::default(),
                &round,
            )
            .map_err(|error| {
                format!("PoS v2 proposal manifest at byte {record_offset} is rejected: {error}")
            })?;
        }
        let (new_final_height, new_final_hash) = validated_pow_header_chain(
            call,
            &record.block,
            previous_final_height,
        )
        .await
        .map_err(|error| format!("PoS-store PoW ancestry at byte {record_offset} is rejected: {error}"))?;

        let response = bounded_state_call(
            call,
            StateRequest::CrosslinkFinalizeBlock(new_final_hash),
            "PoS-store replay finalization",
        )
        .await?;
        let StateResponse::CrosslinkFinalized(finalized_hash, aggregated_stakes) = response else {
            return Err("PoS-store replay finalization returned the wrong response type".into());
        };
        if finalized_hash != new_final_hash {
            return Err("PoS-store replay finalized a different PoW hash".into());
        }
        let expected_next_roster = if aggregated_stakes.is_empty() {
            if expected_height == 0 {
                next_roster.clone()
            } else {
                return Err("PoS-store replay found an empty non-genesis bonded roster".into());
            }
        } else {
            aggregated_stakes
                .into_iter()
                .map(|(pub_key, voting_power)| RosterMember {
                    pub_key,
                    voting_power,
                    txids: Vec::new(),
                })
                .collect()
        };
        if record.next_roster != expected_next_roster {
            return Err(format!(
                "PoS-store next roster at byte {record_offset} differs from finalized state"
            ));
        }
        let next_height = expected_height
            .checked_add(1)
            .ok_or("BFT replay height overflow")?;
        let next_terminated = terminated_finalizers_at(
            &config.hardforks,
            next_height,
            new_final_height.0 as u64,
        );
        tenderlink::validate_consensus_roster(&tenderlink_roster_from_internal(
            &expected_next_roster,
            &next_terminated,
        ))?;

        let active_len = usize::min(100, current_roster.len());
        let signed_stake = current_roster[..active_len]
            .iter()
            .filter(|member| {
                record
                    .fat_pointer
                    .signatures
                    .iter()
                    .any(|signature| signature.pub_key == member.pub_key)
            })
            .try_fold(0u64, |total, member| total.checked_add(member.stake))
            .ok_or("replayed signing stake overflows u64")?;
        round.counts = tenderlink::ConsensusCounts {
            anys: signed_stake,
            prevotes: 0,
            nil_prevotes: 0,
            yes_prevotes: 0,
            precommits: signed_stake,
            yes_precommits: signed_stake,
        };
        if !has_authenticated_proposal_context {
            // Legacy records do not carry valid_round, and explicit force-fed v2 decisions can
            // lack a proposal manifest. Retain the precommit QC but never advertise proposal
            // chunks without complete verified signing context.
            round.proposal_valid_round = -1;
            round.proposal_sigs.clear();
            round.proposal_sigs_n = 0;
        }

        records.push(PosStoreRecordIndex {
            offset: record_offset,
            len: record_end
                .checked_sub(record_offset)
                .ok_or("PoS-store record end precedes its start")?,
            finalized_bc_height: new_final_height.0,
        });
        if rounds.len() == tenderlink::MAX_RECENT_COMMIT_ROUNDS_IN_MEMORY {
            rounds.pop_front();
        }
        rounds.push_back(round);
        blocks.push(record.block);
        tip = record.fat_pointer;
        next_roster = expected_next_roster;
        final_block = Some((new_final_height, new_final_hash));
    }

    if file
        .stream_position()
        .map_err(|error| format!("failed reading final PoS-store position: {error}"))?
        != file_len
    {
        return Err("PoS-store replay did not end exactly at the durable EOF".into());
    }
    file.seek(SeekFrom::End(0))
        .map_err(|error| format!("failed to position verified PoS store for append: {error}"))?;
    Ok(VerifiedPosReplay {
        file,
        rounds: rounds.into_iter().collect(),
        records,
        blocks,
        tip,
        next_roster,
        final_block,
        torn_tail,
    })
}

async fn tfl_service_main_loop(
    internal_handle: TFLServiceHandle,
    global_seed: [u8; 32],
    path_to_pos_store_file: PathBuf,
    is_regtest: bool,
) -> Result<(), String> {
    let call = internal_handle.call.clone();
    let config = internal_handle.config.clone();
    let params = &PROTOTYPE_PARAMETERS;

    #[cfg(feature = "viz_gui")]
    {
        let rt = tokio::runtime::Handle::current();
        let viz_tfl_handle = internal_handle.clone();
        tokio::task::spawn_blocking(move || {
            rt.block_on(viz2::service_viz_requests(viz_tfl_handle, params))
        });

        let rt = tokio::runtime::Handle::current();
        let tfl_handle2 = internal_handle.clone();
        *wallet::RECENCY_REQUEST.lock().unwrap() = Some(wallet::RecencyRequestClosure(Arc::new(move || {
            rt.block_on(
                async {
                    let lock = tfl_handle2.internal.lock().await;
                    let recency_status = &lock.recency_status;
                    serde_json::to_string_pretty(recency_status).ok()
                }
            )
        })));
    }

    if *TEST_MODE.lock().unwrap() {
        run_tfl_test(internal_handle.clone());
    }

    let public_ip_string = config
        .public_address
        .clone()
        .unwrap_or_else(|| "127.0.0.1:23485".to_owned());
    info!(endpoint = %public_ip_string, "configured BFT endpoint");

    let canonical_validator_identity = canonical_validator_identity_configured(&config)?;

    let (my_private_key, my_public_key, static_keypair, local_endpoint) =
        if canonical_validator_identity {
            let signing_seed = decode_exact_lower_hex_32_named(
                config
                    .validator_signing_key_seed
                    .as_ref()
                    .expect("complete identity checked above")
                    .expose_secret(),
                "validator signing-key seed",
                true,
            )?;
            let private_key = ed25519_zebra::SigningKey::from(signing_seed);
            let derived_public_key = PubKeyID(
                <[u8; 32]>::from(ed25519_zebra::VerificationKeyBytes::from(&private_key)),
            );
            let asserted_public_key = decode_consensus_public_key_hex(
                config
                    .validator_consensus_public_key
                    .as_ref()
                    .expect("complete identity checked above"),
            )?;
            if derived_public_key != asserted_public_key {
                return Err(
                    "validator signing seed does not match validator_consensus_public_key".into(),
                );
            }
            let noise_seed = decode_exact_lower_hex_32_named(
                config
                    .validator_noise_static_key_seed
                    .as_ref()
                    .expect("complete identity checked above")
                    .expose_secret(),
                "validator Noise static-key seed",
                true,
            )?;
            let noise_keypair = tenderlink::bandwidth_test::new_keypair_from_connect_magic1_with_seed(
                tenderlink::CRYPTO_MAGIC,
                noise_seed,
            )
            .ok_or("failed to derive the configured Noise static key")?;
            let (ip, port) = tenderlink::parse_to_ipv6_bytes(&public_ip_string)
                .map_err(|error| format!("invalid local validator endpoint: {error}"))?;
            let endpoint = tenderlink::bandwidth_test::STPAddress::from(ip, port, &noise_keypair);
            (private_key, derived_public_key, noise_keypair, endpoint)
        } else {
            // Legacy configuration never enters validator mode. Use separate domain-derived,
            // process-scoped observer identities; neither secret comes from an endpoint and the
            // durable signer is forced observer-only below.
            let observer_secret = |domain: &[u8]| -> [u8; 32] {
                let mut hasher = blake3::Hasher::new();
                hasher.update(domain);
                hasher.update(&global_seed);
                hasher.finalize().into()
            };
            let private_key = ed25519_zebra::SigningKey::from(observer_secret(
                b"ctaz-observer-consensus-key-v1",
            ));
            let public_key = PubKeyID(
                <[u8; 32]>::from(ed25519_zebra::VerificationKeyBytes::from(&private_key)),
            );
            let noise_keypair = tenderlink::bandwidth_test::new_keypair_from_connect_magic1_with_seed(
                tenderlink::CRYPTO_MAGIC,
                observer_secret(b"ctaz-observer-noise-key-v1"),
            )
            .ok_or("failed to derive observer Noise static key")?;
            let (ip, port) = tenderlink::parse_to_ipv6_bytes(&public_ip_string)
                .map_err(|error| format!("invalid observer endpoint: {error}"))?;
            let endpoint = tenderlink::bandwidth_test::STPAddress::from(ip, port, &noise_keypair);
            warn!("legacy BFT identity configuration is observer-only and fails readiness");
            (private_key, public_key, noise_keypair, endpoint)
        };
    internal_handle.internal.lock().await.my_public_key = my_public_key;

    let mut tenderlink_task: tokio::task::JoinHandle<std::io::Result<()>>;
    let validator_readiness_configured: bool;
    {
        let static_keypair_maybe = Some(static_keypair.clone());
        let endpoint_maybe = Some(local_endpoint.clone());

        let tfl_handle1 = internal_handle.clone();
        let tfl_handle2 = internal_handle.clone();
        let tfl_handle3 = internal_handle.clone();
        let tfl_handle4 = internal_handle.clone();
        let tfl_handle5 = internal_handle.clone();
        let tfl_handle6 = internal_handle.clone();
        let tfl_handle7 = internal_handle.clone();
        let tfl_handle8 = internal_handle.clone();
        let tfl_handle9 = internal_handle.clone();
        let tfl_handle10 = internal_handle.clone();

        *wallet::TENDERLINK_PUBLIC_KEY.lock().unwrap() = my_public_key;

        // TODO(Sam): Fill this out.
        let mut ingest_data_for_tenderlink: Vec<tenderlink::RoundData> = Vec::new();

        let mut i_bft_blocks: Vec<BftBlock> = Vec::new();
        let mut fat_pointer_to_tip: FatPointerToBftBlock = FatPointerToBftBlock::null();
        let mut unsorted_roster = bootstrap_roster_from_config(&config)?;
        let bootstrap_roster_for_history = Arc::new(unsorted_roster.clone());

        let mut held_pos_store_file = None;
        let mut pos_store_read_file = None;
        let mut pos_store_records = Vec::new();
        let mut replay_final_block = None;
        let mut replay_torn_tail = None;
        if path_to_pos_store_file.to_str() != Some("") {
            let replay = replay_verified_pos_store(
                &path_to_pos_store_file,
                &call,
                &config,
                unsorted_roster.clone(),
            )
            .await?;
            let replay_read_file = Arc::new(replay.file.try_clone().map_err(|error| {
                format!("failed to duplicate exclusive PoS-store handle: {error}")
            })?);
            ingest_data_for_tenderlink = replay.rounds;
            pos_store_records = replay.records;
            i_bft_blocks = replay.blocks;
            fat_pointer_to_tip = replay.tip;
            unsorted_roster = replay.next_roster;
            replay_final_block = replay.final_block;
            replay_torn_tail = replay.torn_tail;
            held_pos_store_file = Some(replay.file);
            pos_store_read_file = Some(replay_read_file);
        }
        let pos_store_size_bytes = held_pos_store_file
            .as_ref()
            .map(|file| {
                file.metadata()
                    .map(|metadata| metadata.len())
                    .map_err(|error| format!("failed to stat replayed PoS store: {error}"))
            })
            .transpose()?
            .unwrap_or(0);
        let pos_store_record_count = u64::try_from(pos_store_records.len())
            .map_err(|_| "PoS record count does not fit u64")?;
        let pos_store_complete_eof = replay_torn_tail.is_none();
        #[cfg(any())]
        if path_to_pos_store_file.to_str() != Some("") {
            let (mut pos_file, _) = open_exclusive_pos_store(&path_to_pos_store_file)?;
            let mut valid_byte_count = 0;
            'big_loop: loop {
                valid_byte_count = pos_file.stream_position()
                    .map_err(|error| format!("failed reading PoS-store position: {error}"))?;
                let block = if let Ok(block) = BftBlock::zcash_deserialize(&mut pos_file) { block } else { break; };
                let fat_pointer = if let Ok(fat_pointer) = FatPointerToBftBlock::zcash_deserialize(&mut pos_file) { fat_pointer } else { break; };

                let mut buf = [0u8; 8];
                if pos_file.read_exact(&mut buf).is_err() { break; }
                let new_roster_count = u64::from_le_bytes(buf);
                if new_roster_count > MAX_POS_STORE_ROSTER_MEMBERS { break; }
                let mut new_roster = Vec::new();
                for _ in 0..new_roster_count {
                    if let Ok(v) = RosterMember::read_from(&mut pos_file) {
                        new_roster.push(v);
                    } else { break 'big_loop; }
                }

                let mut buf = [0u8; 8];
                if pos_file.read_exact(&mut buf).is_err() { break; }
                let proposal_sigs_n = u64::from_le_bytes(buf);
                if proposal_sigs_n > MAX_POS_STORE_PROPOSAL_SIGNATURES { break; }
                let mut proposal_sigs = Vec::new();
                for _ in 0..proposal_sigs_n {
                    let mut sig = TMSig::NIL;
                    if pos_file.read_exact(&mut sig.0).is_err() { break 'big_loop; }
                    proposal_sigs.push(sig);
                }

                if block.previous_block_fat_ptr.points_at_block_hash() != fat_pointer_to_tip.points_at_block_hash() { break; }

                let mut round_data = tenderlink::RoundData::EMPTY;
                // Historical round replay: filter the roster exactly as the live path did at this
                // height, so it matches the roster that actually voted on this decided block (the
                // sigs/counts below are derived from it). Uses this block's own height and the BC
                // height it finalized (derived from its finalization-candidate header). For pre-
                // hardfork heights this is a no-op, so existing chains are unaffected.
                let this_bft_height = ingest_data_for_tenderlink.len() as u64;
                let this_finalized_bc_height: u64 = if let Some(candidate) = block.headers.first() {
                    let candidate_hash = ZebBlockHash(BlockHash::from_header_data(candidate).0);
                    block_height_from_hash(&call, candidate_hash).await.map(|h| h.0 as u64).unwrap_or(0)
                } else { 0 };
                let this_terminated = terminated_finalizers_at(&config.hardforks, this_bft_height, this_finalized_bc_height);
                round_data.roster = tenderlink_roster_from_internal(
                    &unsorted_roster,
                    &this_terminated,
                );
                round_data.msg_val_sigs = round_data.roster.iter().map(|v| fat_pointer.signatures.iter().find(|s| s.pub_key == v.pub_key).map(|s| s.vote_signature).unwrap_or([0u8; 64])).map(|s| [(tenderlink::ValueId::NIL, TMSig::NIL), (tenderlink::ValueId(fat_pointer.points_at_block_hash().0), TMSig(s))]).collect();
                round_data.counts.precommits = fat_pointer.signatures.len() as u64;
                round_data.counts.yes_precommits = fat_pointer.signatures.len() as u64;
                round_data.proposal_sigs_n = proposal_sigs_n as usize;
                round_data.proposal_sigs = proposal_sigs;
                round_data.proposal = tenderlink::BlockValue(block.zcash_serialize_to_vec().unwrap());
                round_data.proposal_id = tenderlink::ValueId(fat_pointer.points_at_block_hash().0);
                round_data.height = ingest_data_for_tenderlink.len() as u64;
                round_data.round = fat_pointer.get_vote_template().round as u32;
                // Vote namespacing: this loaded height's domain separator (inclusive of any
                // hardfork scheduled at it). Nil when no hardforks apply -> backwards compatible.
                round_data.vote_namespace = namespace_for_bft_height(&config.hardforks, round_data.height);

                ingest_data_for_tenderlink.push(round_data);
                i_bft_blocks.push(block);
                fat_pointer_to_tip = fat_pointer;
                unsorted_roster = new_roster;
            }
            if pos_file.metadata()
                .map_err(|error| format!("failed to stat PoS store after replay: {error}"))?
                .len() != valid_byte_count
            {
                pos_file.set_len(valid_byte_count)
                    .map_err(|error| format!("failed to truncate torn PoS-store tail: {error}"))?;
                pos_file.sync_all()
                    .map_err(|error| format!("failed to sync PoS-store truncation: {error}"))?;
            }
            pos_file.seek(SeekFrom::End(0))
                .map_err(|error| format!("failed to position PoS store for append: {error}"))?;
            held_pos_store_file = Some(pos_file);
        }

        // Peer routes are explicit key bindings. Neither consensus nor Noise identity is ever
        // derived from the endpoint string.
        let finalizer_peer_addresses = finalizer_peer_addresses_from_explicit_config(
            &config.bft_peer_identities,
            &public_ip_string,
            my_public_key,
            &static_keypair,
        )?;

        let (new_final_height, new_final_hash) = replay_final_block
            .unwrap_or((ZebBlockHeight(0), ZebBlockHash([0; 32])));

        let signer_parent_commit = fat_pointer_to_tip.points_at_block_hash().0;
        let startup_bft_height = u64::try_from(i_bft_blocks.len())
            .map_err(|_| "BFT history length does not fit u64")?;
        let bft_height_by_hash: HashMap<[u8; 32], usize> = i_bft_blocks
            .iter()
            .enumerate()
            .map(|(height, block)| (block.blake3_hash().0, height))
            .collect();
        if bft_height_by_hash.len() != i_bft_blocks.len() {
            return Err("PoS store replays duplicate BFT block hashes".into());
        }
        let roster = {
            let mut internal = internal_handle.internal.lock().await;

            // Startup roster is for the next height to decide (the loaded chain length), with the
            // terminated finalizers excluded inclusively at that height — derived purely from the
            // schedule (no stored blacklist; see `terminated_finalizers_at`).
            let terminated = terminated_finalizers_at(&config.hardforks, startup_bft_height, new_final_height.0 as u64);
            let roster = tenderlink_roster_from_internal(
                &unsorted_roster,
                &terminated,
            );
            if canonical_validator_identity
                && !roster[..usize::min(100, roster.len())]
                    .iter()
                    .any(|member| member.pub_key == my_public_key)
            {
                return Err(
                    "asserted validator consensus key is absent from the active startup roster"
                        .into(),
                );
            }
            internal.finalizers_at_current_height = unsorted_roster;
            internal.bft_blocks = i_bft_blocks;
            internal.bft_height_by_hash = bft_height_by_hash;
            internal.fat_pointer_to_tip = fat_pointer_to_tip;
            internal.pos_store_file = held_pos_store_file;
            internal.pos_store_read_file = pos_store_read_file;
            internal.pos_store_records = pos_store_records;
            internal.pos_store_unverified_tail = replay_torn_tail;
            if new_final_hash != ZebBlockHash([0; 32]) {
                internal.current_bc_final = Some((new_final_height, new_final_hash));
                internal.latest_final_block = Some((new_final_height, new_final_hash));
            }
            roster
        };

        // new_network owns the deferred PoW queue and re-evaluates it every tick.
        // Replaying the durable BFT tip must not synthesize a finalize request.

        // Vote namespacing uses the durable chain height, not the length of the
        // bounded in-memory recent-round window.
        let initial_vote_namespace =
            namespace_for_bft_height(&config.hardforks, startup_bft_height);

        let signer_chain_id: [u8; 32] = {
            let mut hasher = blake3::Hasher::new();
            hasher.update(b"ctaz-tenderlink-canonical-network-v2");
            hasher.update(if is_regtest {
                b"ctaz-regtest".as_slice()
            } else {
                b"ctaz-public-network".as_slice()
            });
            hasher.finalize().into()
        };
        let signer_consensus_config_hash: [u8; 32] = {
            let mut bytes = vec![config.disable_shipped_hardforks as u8];
            bytes.extend_from_slice(&(config.hardforks.len() as u64).to_le_bytes());
            for hardfork in &config.hardforks {
                hardfork.zcash_serialize(&mut bytes).expect("serializing hardfork config to Vec is infallible");
            }
            blake3::hash(&bytes).into()
        };
        let observer_only = |reason: String| tenderlink::SignerStartup::ObserverOnly {
            reason,
            chain_id: signer_chain_id,
            parent_commit: signer_parent_commit,
            consensus_config_hash: signer_consensus_config_hash,
        };
        let (readiness_configured, signer_startup) = if !canonical_validator_identity {
            (
                false,
                observer_only("canonical validator identity is absent".into()),
            )
        } else if path_to_pos_store_file.as_os_str().is_empty() {
            (
                false,
                observer_only("durable PoS history path is absent".into()),
            )
        } else {
            match (
                config.signer_wal_path.clone(),
                config.signer_anchor_path.clone(),
            ) {
                (Some(wal_path), Some(anchor_path)) => {
                    let bootstrap_tenderlink_roster = tenderlink_roster_from_internal(
                        bootstrap_roster_for_history.as_ref().as_slice(),
                        &HashSet::new(),
                    );
                    let authenticated_bootstrap_roster_hash =
                        tenderlink::consensus_roster_hash(&bootstrap_tenderlink_roster)?;
                    let (active_roster_hash, active_roster_index, active_roster_len) =
                        tenderlink::signer_epoch_roster_binding(&roster, my_public_key)?;
                    let context = SignerMigrationContext {
                        validator_consensus_public_key: my_public_key,
                        chain_id: signer_chain_id,
                        startup_bft_height,
                        parent_commit: signer_parent_commit,
                        vote_namespace: initial_vote_namespace,
                        consensus_config_hash: tenderlink::signer_consensus_config_binding(
                            signer_consensus_config_hash,
                        ),
                        authenticated_bootstrap_roster_hash,
                        active_roster_hash,
                        active_roster_index,
                        active_roster_len,
                        finalized_pow_height: new_final_height.0,
                        finalized_pow_hash: new_final_hash.0,
                        pos_store_size_bytes,
                        pos_store_record_count,
                        pos_store_complete_eof,
                        wal_path: &wal_path,
                        anchor_path: &anchor_path,
                        pos_store_path: &path_to_pos_store_file,
                    };
                    match signer_startup_authority(&config, &context) {
                        Ok(authority) => (
                            true,
                            tenderlink::SignerStartup::Durable {
                                wal_path,
                                anchor_path,
                                independent_anchor_authorized: true,
                                non_genesis_bootstrap_receipt_hash: authority
                                    .non_genesis_receipt_hash,
                                chain_id: signer_chain_id,
                                parent_commit: signer_parent_commit,
                                consensus_config_hash: signer_consensus_config_hash,
                            },
                        ),
                        Err(reason) => {
                            warn!(%reason, "validator signer remains observer-only");
                            (false, observer_only(reason))
                        }
                    }
                }
                _ => (
                    false,
                    observer_only("signer WAL or independent anchor path is absent".into()),
                ),
            }
        };
        validator_readiness_configured = readiness_configured;

        tenderlink_task = tokio::spawn(tenderlink::entry_point(
            my_private_key,
            static_keypair_maybe,
            endpoint_maybe,
            roster,
            finalizer_peer_addresses,
            None,
            signer_startup,
            tenderlink::ClosureToProposeNewBlock(Arc::new(move || {
                let tfl_handle1 = tfl_handle1.clone();
                Box::pin(async move {
                    propose_new_bft_block(&tfl_handle1).await.map(|block| {
                        tenderlink::BlockValue(block.zcash_serialize_to_vec().unwrap())
                    })
                })
            })),
            tenderlink::ClosureToValidateProposedBlock(Arc::new(move |block| {
                let tfl_handle2 = tfl_handle2.clone();
                Box::pin(async move {
                    match deserialize_bft_block_exact(&block.0) {
                        Ok(bft_block) => validate_bft_block(&tfl_handle2, &bft_block).await,
                        Err(error) => {
                            error!(%error, "Failed to deserialize exact Tenderlink payload");
                            (tenderlink::TMStatus::Fail, tenderlink::TMStatusReason::None)
                        }
                    }
                })
            })),
            tenderlink::ClosureToPushDecidedBlock(Arc::new(move |block, fat_pointer, proposal_valid_round, tender_proposal_sigs| {
                let tfl_handle3 = tfl_handle3.clone();
                Box::pin(async move {
                    let decided_block = deserialize_bft_block_exact(&block.0)?;
                    let (roster, namespace, durable_parent_commit) = apply_verified_decided_bft_block(
                        &tfl_handle3,
                        &decided_block,
                        &fat_pointer.into(),
                        proposal_valid_round,
                        tender_proposal_sigs,
                    )
                    .await?;
                    Ok(tenderlink::DurableDecisionOutcome {
                        next_roster: roster,
                        next_vote_namespace: namespace,
                        durable_parent_commit,
                    })
                })
            })),
            tenderlink::ClosureToLoadCommittedRound(Arc::new(move |height| {
                let tfl_handle = tfl_handle10.clone();
                let bootstrap_roster = Arc::clone(&bootstrap_roster_for_history);
                Box::pin(async move {
                    load_historical_committed_round(
                        &tfl_handle,
                        bootstrap_roster,
                        height,
                    )
                    .await
                })
            })),
            tenderlink::ClosureToUpdatePeers(Arc::new(move |all_peers| {
                let tfl_handle = tfl_handle8.clone();
                Box::pin(async move {
                    let mut internal = tfl_handle.internal.lock().await;
                    internal.peer_strings.truncate(0);

                    for peer in &all_peers {
                        internal.peer_strings.push(format!("{} {} ({})",
                            if let Some(pubkey) = peer.root_public_bft_key { pubkey.to_string() } else { "unknown peer".to_string() },
                            if peer.connected { "connected" } else { "disconnected" },
                            peer.latest_status_request_height,
                        ));
                    }
                })
            })),
            tenderlink::ClosureToAllowBftAccess(Arc::new(move |bft_state: &tenderlink::TMState, bft_key_address_map: &tenderlink::BftAddressMap| {
                let tfl_handle = tfl_handle9.clone();
                Box::pin(async move {
                    let now_utc = chrono::Utc::now().timestamp();
                    let signer_is_active = bft_state.durable_signer.is_active();
                    let mut finalizer_statuses = Vec::<(PubKeyID, FinalizerRecencyStatus)>::new();

                    // ~current height
                    for round in &bft_state.rounds_data {
                        let is_my_height = round.height == bft_state.height;// && round.round == bft_state.round;

                        for (roster_i, member) in round.roster.iter().enumerate() {
                            use zcash_primitives::bft::TMSig;
                            use tenderlink::ConsensusCounts;

                            let st = if let Some(v) = finalizer_statuses.iter_mut().find(|(key, _st)| *key == member.pub_key) {
                                v
                            } else {
                                let last_i = finalizer_statuses.len();
                                finalizer_statuses.push((member.pub_key, FinalizerRecencyStatus::default()));
                                &mut finalizer_statuses[last_i]
                            };

                            let cs = ConsensusCounts::from(&(round.msg_val_sigs[roster_i], 1)); // simple (not weighted) counts
                            if cs.anys > 0 {
                                if is_my_height {
                                    st.1.no_yes_votes_in_my_height[0][0] += cs.nil_prevotes;
                                    st.1.no_yes_votes_in_my_height[0][1] += cs.yes_prevotes;
                                    st.1.no_yes_votes_in_my_height[1][0] += cs.precommits.saturating_sub(cs.yes_precommits); // no explicit nil_precommits
                                    st.1.no_yes_votes_in_my_height[1][1] += cs.yes_precommits;

                                    st.1.highest_round_vote = st.1.highest_round_vote.max(round.round);
                                }
                            }

                            if member.pub_key == bft_state.my_pub_key {
                                st.1.last_direct_connection_utc = Some(now_utc);
                            } else {
                                let utc = bft_key_address_map.last_packet_utcs.get(&member.pub_key);
                                st.1.last_direct_connection_utc = st.1.last_direct_connection_utc.max(utc.copied());
                            }
                        }
                    }

                    // for data in &bft_state.recent_commit_round_cache {
                    //     println!("LIVENESS recent_commit_round_cache: height: {}, round: {}", data.height, data.round);
                    // }

                    let mut internal = tfl_handle.internal.lock().await;
                    internal.recency_status = TFLRecencyStatus {
                        now_utc,
                        my_height: bft_state.height,
                        my_round:  bft_state.round,
                        my_step: match bft_state.step {
                            tenderlink::TMStep::Propose => 0,
                            tenderlink::TMStep::Prevote => 1,
                            tenderlink::TMStep::Precommit => 2,
                        },
                        my_locked_round: bft_state.locked_value_round.1,
                        my_valid_round:  bft_state.valid_value_round.1,
                        finalizer_statuses,
                    };
                    let has_torn_tail = internal.pos_store_unverified_tail.is_some();
                    drop(internal);
                    tfl_handle.set_service_health(if validator_readiness_configured {
                        if signer_is_active && !has_torn_tail {
                            SERVICE_HEALTH_READY
                        } else {
                            SERVICE_HEALTH_STARTING
                        }
                    } else {
                        SERVICE_HEALTH_OBSERVER_ONLY
                    });
                })
            })),
            ingest_data_for_tenderlink,
            initial_vote_namespace,
        ));
    }

    tokio::task::yield_now().await;
    if tenderlink_task.is_finished() {
        internal_handle.set_service_health(SERVICE_HEALTH_FAILED);
        return match tenderlink_task.await {
            Ok(Ok(())) => Err("Tenderlink terminated unexpectedly".into()),
            Ok(Err(error)) => Err(format!("Tenderlink terminated with an I/O error: {error}")),
            Err(error) => Err(format!("Tenderlink task panicked or was cancelled: {error}")),
        };
    }
    internal_handle.set_service_health(
        if validator_readiness_configured {
            SERVICE_HEALTH_STARTING
        } else {
            SERVICE_HEALTH_OBSERVER_ONLY
        },
    );

    let mut run_instant = Instant::now();
    let mut last_diagnostic_print = Instant::now();
    let mut current_bc_tip: Option<(ZebBlockHeight, ZebBlockHash)> = None;

    loop {
        // Calculate this prior to message handling so that handlers can use it. The state
        // service cannot freeze consensus indefinitely.
        let new_bc_tip = match bounded_state_call(
            &call,
            StateRequest::Tip,
            "crosslink main-loop PoW-tip lookup",
        )
        .await
        {
            Ok(StateResponse::Tip(value)) => value,
            Ok(_) => {
                warn!("crosslink main-loop PoW-tip lookup returned the wrong response type");
                None
            }
            Err(error) => {
                warn!(%error, "crosslink main-loop PoW-tip lookup failed");
                None
            }
        };

        tokio::select! {
            tenderlink_result = &mut tenderlink_task => {
                internal_handle.set_service_health(SERVICE_HEALTH_FAILED);
                return match tenderlink_result {
                    Ok(Ok(())) => Err("Tenderlink terminated unexpectedly".into()),
                    Ok(Err(error)) => Err(format!("Tenderlink terminated with an I/O error: {error}")),
                    Err(error) => Err(format!("Tenderlink task panicked or was cancelled: {error}")),
                };
            }
            _ = tokio::time::sleep_until(run_instant) => {}
        }
        run_instant += MAIN_LOOP_SLEEP_INTERVAL;

        let pending_reflush = internal_handle.internal.lock().await.pending_reflush;
        if let Some(final_hash) = pending_reflush {
            match bounded_crosslink_reflush(
                &call,
                final_hash,
                "pending crosslink reflush retry",
            )
            .await
            {
                Ok(()) => {
                    let mut internal = internal_handle.internal.lock().await;
                    if internal.pending_reflush == Some(final_hash) {
                        internal.pending_reflush = None;
                    }
                }
                Err(error) => warn!(%error, "bounded crosslink reflush retry remains pending"),
            }
        }

        // from this point onwards we must race to completion in order to avoid stalling incoming requests
        // NOTE: split to avoid deadlock from non-recursive mutex - can we reasonably change type?
        #[allow(unused_mut)]
        let mut internal = internal_handle.internal.lock().await;

        // Check TFL is activated before we do anything that assumes it
        if !internal.tfl_is_activated {
            if let Some((height, _hash)) = new_bc_tip {
                if height < TFL_ACTIVATION_HEIGHT {
                    continue;
                } else {
                    internal.tfl_is_activated = true;
                    info!("activating TFL!");
                }
            }
        }

        if last_diagnostic_print.elapsed() >= MAIN_LOOP_INFO_DUMP_INTERVAL {
            last_diagnostic_print = Instant::now();
            if let (Some((tip_height, _tip_hash)), Some((final_height, _final_hash))) =
                (current_bc_tip, internal.latest_final_block)
            {
                if tip_height < final_height {
                    info!(
                        "Our PoW tip is {} blocks away from the latest final block.",
                        final_height - tip_height
                    );
                } else {
                    let behind = tip_height - final_height;
                    if behind > 512 {
                        warn!("WARNING! BFT-Finality is falling behind the PoW chain. Current gap to tip is {:?} blocks.", behind);
                    }
                }
            }
        }

        current_bc_tip = new_bc_tip;
    }
}

async fn tfl_block_finality_from_height_hash(
    internal_handle: TFLServiceHandle,
    height: ZebBlockHeight,
    hash: ZebBlockHash,
) -> Result<Option<TFLBlockFinality>, TFLServiceError> {
    // TODO: None is no longer ever returned
    let call = internal_handle.call.clone();
    let block_hdr = (call.state)(StateRequest::BlockHeader(hash.into()));
    let (final_height, final_hash) = match tfl_final_block_height_hash(&internal_handle).await {
        Some(v) => v,
        None => {
            return Err(TFLServiceError::Misc(
                "There is no final block.".to_string(),
            ));
        }
    };

    if height > final_height {
        // N.B. this may be invalidated by the time it is received
        Ok(Some(TFLBlockFinality::NotYetFinalized))
    } else {
        let cmp_hash = if height == final_height {
            final_hash // we already have the hash at the final height, no point in re-getting it
        } else {
            match (call.state)(StateRequest::BlockHeader(height.into())).await {
                Ok(StateResponse::BlockHeader { hash, .. }) => hash,

                Err(err) => return Err(TFLServiceError::Misc(err.to_string())),

                _ => {
                    return Err(TFLServiceError::Misc(
                        "Invalid BlockHeader response type".to_string(),
                    ))
                }
            }
        };

        // We have the hash of the block at the given height from the best chain.
        // If it matches the queried hash then our block is on the best chain under the finalization
        // height & is thus finalized.
        // Otherwise it can't be finalized.
        Ok(Some(if hash == cmp_hash {
            TFLBlockFinality::Finalized
        } else {
            TFLBlockFinality::CantBeFinalized
        }))
    }
}

async fn total_issuance_from_key(
    internal_handle: TFLServiceHandle,
    ufvks: Vec<zcash_keys::keys::UnifiedFullViewingKey>,
    first_height: ZebBlockHeight,
    last_height: ZebBlockHeight,
) -> Result<Vec<ScanInfo>, String> {
    let call = internal_handle.call.clone();

    let mut delegation_bonds = HashMap::new();
    let mut utxos_per_ufvk = vec![HashSet::<(PubKeyID, u32)>::new(); ufvks.len()]; // NOTE: hashsets here are grow-only

    let mut scan_infos = Vec::<ScanInfo>::with_capacity(ufvks.len());
    let mut scan_ctxs = Vec::<wallet::scanner::ScanCtx>::with_capacity(ufvks.len());
    for ufvk in &ufvks {
        scan_infos.push(ScanInfo { ufvk: ufvk.encode(&TEST_NETWORK), ..ScanInfo::default() });

        let external_keys = wallet::PreparedKeys::from_ufvk_all(&ufvk);
        let internal_keys = wallet::PreparedKeys::from_ufvk_all_internal(&ufvk);
        let (Some(orchard_external_ovk), Some(orchard_internal_ovk)) = (external_keys.orchard_ovk, internal_keys.orchard_ovk) else {
            return Err("could not create orchard ovks".to_owned());
        };

        let Some((t_addr, _p2sh, _ua)) = wallet::addrs_from_ufvk(ufvk, 0) else{
            return Err("Could not get an address".to_owned());
        };

        scan_ctxs.push(wallet::scanner::ScanCtx { ufvk: ufvk.clone(), t_addr, orchard_external_ovk, orchard_internal_ovk });
    }

    for height in first_height.0..=last_height.0 {
        // let tz = wallet::Timer::scope_("scan height", true);
        println!("scanning height {height}");
        let res = (call.state)(StateRequest::Block(ZebBlockHeight(height).into())).await;
        let block = match res {
            Ok(StateResponse::Block(Some(block))) => block,
            Ok(StateResponse::Block(None)) => return Err(format!("failed to get block at height {height}")),
            _ => return Err(format!("unexpectedly failed to get block at height {height}: {res:?}")),
        };

        if block.transactions.len() == 0 {
            return Err(format!("block at height {height} had 0 transactions"));
        }



        for (tx_i, tx) in block.transactions.iter().enumerate() {
            let coinbase_tx_bytes = match tx.zcash_serialize_to_vec() {
                Ok(tx) => tx,
                Err(err) => return Err(format!("failed to serialize coinbase tx at height {height}: {err:?}")),
            };


            let txid = tx.unmined_id().mined_id();

            if let Some(staking_action) = tx.staking_action() {
                let mut bond_retargets = vec![HashMap::new()];
                // Note(Sam): It seems weird that the bonds never get deleted. I don't know what I was
                // thinking when I did that. But it makes this code easy.
                zebra_state::update_chain_tip_with_delegation_bond(
                    &mut zebra_chain::value_balance::ValueBalance::zero(),
                    &mut delegation_bonds,
                    &mut bond_retargets,
                    &staking_action,
                    &txid.0.into(),
                    zebra_state::TransactionLocation {
                        height: ZebBlockHeight(height),
                        index: zebra_state::TransactionIndex::from_index(tx_i.try_into().unwrap()),
                    }
                );
            }

            if delegation_bonds.len() > 0 {
                let reward_store_for_revert = zebra_state::update_bonds_with_pos_issuance(zebra_state::constants::POS_BLOCK_REWARD_ZATS, &mut delegation_bonds);
            }

            for (ufvk_i, scan_ctx) in scan_ctxs.iter().enumerate() {
                let utxos = &mut utxos_per_ufvk[ufvk_i];
                let scan_info = &mut scan_infos[ufvk_i];
                match wallet::scanner::scan_tx(scan_info, utxos, &coinbase_tx_bytes, tx_i, height, scan_ctx, txid.0) {
                    Ok(false) => {},
                    Ok(true) => println!("scan info at {height}: {scan_info:?}"),
                    Err(err) => return Err(format!("failed to scan {txid:?} at height {height}: {err}")),
                }
            }
        }
    }

    for scan_info in &mut scan_infos {
        let mut bonds_value = 0;
        for bond in &scan_info.bonds {
            match delegation_bonds.get(&bond.pk.0) {
                Some(bond_info) => {
                    let initial_val: u64 = bond.initial_val;
                    let final_val = u64::from(bond_info.0.amount);
                    let issuance_gained = final_val - initial_val;
                    println!("bond {:?}: initial value = {}; final value = {}; gained {}", bond, initial_val, final_val, issuance_gained);
                    bonds_value += issuance_gained;
                },
                None => return Err(format!("couldn't find bond {:?}", bond)),
            }
        }

        scan_info.bonds_value = bonds_value;
        scan_info.total_value = scan_info.coinbases_value + scan_info.bonds_value;
        println!("final scan info: {scan_info:?}");
    }

    Ok(scan_infos)
}

async fn tfl_service_incoming_request(
    internal_handle: TFLServiceHandle,
    request: TFLServiceRequest,
) -> Result<TFLServiceResponse, TFLServiceError> {
    let call = internal_handle.call.clone();

    // from this point onwards we must race to completion in order to avoid stalling the main thread

    #[allow(unreachable_patterns)]
    match request {
        TFLServiceRequest::IsTFLActivated => Ok(TFLServiceResponse::IsTFLActivated(
            internal_handle.internal.lock().await.tfl_is_activated,
        )),

        TFLServiceRequest::FinalBlockHeightHash => Ok(TFLServiceResponse::FinalBlockHeightHash(
            tfl_final_block_height_hash(&internal_handle).await,
        )),

        TFLServiceRequest::FinalBlockRx => {
            let internal = internal_handle.internal.lock().await;
            Ok(TFLServiceResponse::FinalBlockRx(
                internal.final_change_tx.subscribe(),
            ))
        }

        TFLServiceRequest::SetFinalBlockHash(hash) => Ok(TFLServiceResponse::SetFinalBlockHash(
            tfl_set_finality_by_hash(internal_handle.clone(), hash).await,
        )),

        TFLServiceRequest::BlockFinalityStatus(height, hash) => {
            match tfl_block_finality_from_height_hash(internal_handle.clone(), height, hash).await {
                Ok(val) => Ok(TFLServiceResponse::BlockFinalityStatus({ val })), // N.B. may still be None
                Err(err) => Err(err),
            }
        }

        TFLServiceRequest::TxFinalityStatus(hash) => Ok(TFLServiceResponse::TxFinalityStatus({
            if let Ok(StateResponse::Transaction(Some(tx))) =
                (call.state)(StateRequest::Transaction(hash)).await
            {
                let (final_height, _final_hash) =
                    match tfl_final_block_height_hash(&internal_handle).await {
                        Some(v) => v,
                        None => {
                            return Err(TFLServiceError::Misc(
                                "There is no final block.".to_string(),
                            ));
                        }
                    };

                if tx.height <= final_height {
                    // TODO: CantBeFinalized
                    Some(TFLBlockFinality::Finalized)
                } else {
                    Some(TFLBlockFinality::NotYetFinalized)
                }
            } else {
                None
            }
        })),

        TFLServiceRequest::Roster => Ok(TFLServiceResponse::Roster({
            let internal = internal_handle.internal.lock().await;
            internal
                .finalizers_at_current_height
                .iter()
                .map(|v| RosterMember{ pub_key:<[u8; 32]>::from(v.pub_key), voting_power: v.voting_power, txids: Vec::new() })
                .collect()
        })),

        TFLServiceRequest::FatPointerToBFTChainTip(proposed_pow_height) => {
            let internal = internal_handle.internal.lock().await;
            // Walk back from the tip to find the highest BFT block whose
            // do_not_include_until_bc_height <= proposed_pow_height.
            let n = internal.bft_blocks.len();
            let suitable_height = (0..n).rev()
                .find(|&i| internal.bft_blocks[i].do_not_include_until_bc_height <= proposed_pow_height)
                .map(|i| i + 1); // 1-based (see fat_pointer_to_block_at_height)
            let fat_ptr = if let Some(h) = suitable_height {
                fat_pointer_to_block_at_height(&internal.bft_blocks, &internal.fat_pointer_to_tip, h as u64)
                    .unwrap_or_else(|| FatPointerToBftBlock::null())
            } else {
                FatPointerToBftBlock::null()
            };
            Ok(TFLServiceResponse::FatPointerToBFTChainTip(fat_ptr))
        }

        // wallet
        TFLServiceRequest::Faucet(request) => {
            Ok(TFLServiceResponse::Faucet({
                let closure = wallet::FAUCET_REQUEST.lock().unwrap();
                if let Some(closure) = closure.as_ref() {
                    (closure.0)(request)
                } else {
                    Err("No faucet available".to_owned())
                }
            }))
        }

        TFLServiceRequest::WalletStakingAction(request) => Ok(TFLServiceResponse::WalletStakingAction({
            let rx = {
                let mut lock = wallet::STAKING_STAGE.lock().unwrap();
                match *lock {
                    None => {
                        let (tx, mut rx) = tokio::sync::oneshot::channel();
                        *lock = Some((request, tx));
                        rx
                    }

                    Some(_) => return Err(zebra_state::crosslink::TFLServiceError::Misc("Another stake in progress, please try again soon".to_string())),
                }
            };

            match rx.await {
                Ok(result) => result,
                Err(err) => return Err(zebra_state::crosslink::TFLServiceError::Misc(format!("{err}"))),
            }
        })),

        // workshop - mining & staking via PoW
        TFLServiceRequest::TotalIssuanceFromKey(ufvk_str, first_height, last_height) => {
            Ok(TFLServiceResponse::TotalIssuanceFromKey({
                total_issuance_from_key(internal_handle.clone(), ufvk_str, first_height, last_height).await
            }))
        }

        // crosslink direct
        TFLServiceRequest::FinalizersRecencyStatus => {
            let internal = internal_handle.internal.lock().await;
            println!("FinalizersRecencyStatus: {:?}", internal.recency_status);
            Ok(TFLServiceResponse::FinalizersRecencyStatus(internal.recency_status.clone()))
        }

        TFLServiceRequest::StakingCmd(String) => Err(TFLServiceError::NotImplemented),

        TFLServiceRequest::WalletUfvk => Ok(TFLServiceResponse::WalletUfvk(wallet::USER_UFVK_STRING.lock().unwrap().clone())),
    }
}

async fn tfl_set_finality_by_hash(
    internal_handle: TFLServiceHandle,
    hash: ZebBlockHash,
) -> Option<ZebBlockHeight> {
    // ALT: Result with no success val?
    let mut internal = internal_handle.internal.lock().await;

    if internal.tfl_is_activated {
        // TODO: sanity checks
        let new_height = block_height_from_hash(&internal_handle.call, hash).await;

        if let Some(height) = new_height {
            internal.latest_final_block = Some((height, hash));
        }

        new_height
    } else {
        None
    }
}

trait SatSubAffine<D> {
    fn sat_sub(&self, d: D) -> Self;
}

/// Saturating subtract: goes to 0 if self < d
impl SatSubAffine<i32> for ZebBlockHeight {
    fn sat_sub(&self, d: i32) -> ZebBlockHeight {
        use std::ops::Sub;
        use zebra_chain::block::HeightDiff as BlockHeightDiff;
        self.sub(BlockHeightDiff::from(d)).unwrap_or(ZebBlockHeight(0))
    }
}

// TODO: can we change the signature to unwrap the block options? The blocks must exist if the
// hashes do
// NOTE: this is currently best-chain-only due to request/response limitations
// TODO: add more request/response pairs directly in zebra-state's StateService
/// always returns block hashes. If read_extra_info is set, also returns Blocks, otherwise returns an empty vector.
async fn tfl_block_sequence(
    call: &TFLServiceCalls,
    start_hash: ZebBlockHash,
    final_height_hash: Option<(ZebBlockHeight, ZebBlockHash)>,
    include_start_hash: bool,
    read_extra_info: bool, // NOTE: done here rather than on print to isolate async from sync code
) -> (Vec<(ZebBlockHeight, ZebBlockHash)>, Vec<Option<Arc<Block>>>) {
    // get "real" initial values //////////////////////////////
    let (start_height, init_hash) = {
        if let Ok(StateResponse::BlockHeader { height, header, .. }) =
            (call.state)(StateRequest::BlockHeader(start_hash.into())).await
        {
            if include_start_hash {
                // NOTE: BlockHashes does not return the first hash provided, so we move back 1.
                //       We would probably also be fine to just push it directly.
                (Some(height), Some(header.previous_block_hash))
            } else {
                (Some(ZebBlockHeight(height.0 + 1)), Some(start_hash))
            }
        } else {
            (None, None)
        }
    };
    let (final_height, final_hash) = if let Some((height, hash)) = final_height_hash {
        (Some(height), Some(hash))
    } else if let Ok(StateResponse::Tip(val)) = (call.state)(StateRequest::Tip).await {
        val.unzip()
    } else {
        (None, None)
    };

    // check validity //////////////////////////////
    if start_height.is_none() {
        error!(?start_hash, "start_hash has invalid height");
        return (Vec::new(), Vec::new());
    }
    let start_height = start_height.unwrap();
    let init_hash = init_hash.unwrap();

    if final_height.is_none() {
        error!(?final_height, "final_hash has invalid height");
        return (Vec::new(), Vec::new());
    }
    let final_height = final_height.unwrap();

    if final_height < start_height {
        error!(?final_height, ?start_height, "final_height < start_height");
        return (Vec::new(), Vec::new());
    }

    // build vector //////////////////////////////
    let mut hashes = Vec::with_capacity((final_height - start_height + 1) as usize);
    let mut chunk_i = 0;
    let mut chunk =
        Vec::with_capacity(zebra_state::constants::MAX_FIND_BLOCK_HASHES_RESULTS as usize);
    // NOTE: written as if for iterator
    let mut c = 0;
    loop {
        if chunk_i >= chunk.len() {
            let chunk_start_hash = if chunk.is_empty() {
                &init_hash
            } else {
                // NOTE: as the new first element, this won't be repeated
                chunk.last().expect("should have chunk elements by now")
            };

            let res = (call.state)(StateRequest::FindBlockHashes {
                known_blocks: vec![*chunk_start_hash],
                stop: final_hash,
            })
            .await;

            if let Ok(StateResponse::BlockHashes(chunk_hashes)) = res {
                if c == 0 && include_start_hash && !chunk_hashes.is_empty() {
                    assert_eq!(
                        chunk_hashes[0], start_hash,
                        "first hash is not the one requested"
                    );
                }

                chunk = chunk_hashes;
            } else {
                break; // unexpected
            }

            chunk_i = 0;
        }

        if let Some(val) = chunk.get(chunk_i) {
            let height = ZebBlockHeight(
                start_height.0 + <u32>::try_from(hashes.len()).expect("should fit in u32"),
            );
            // debug_assert!(if let Some(h) = block_height_from_hash(call, *val).await {
            //     if h != height {
            //         error!("expected: {:?}, actual: {:?}", height, h);
            //     }
            //     h == height
            // } else {
            //     true
            // });
            hashes.push((height, *val));
        } else {
            break; // expected
        };
        chunk_i += 1;
        c += 1;
    }

    let mut infos = Vec::with_capacity(if read_extra_info { hashes.len() } else { 0 });
    if read_extra_info {
        for hash in &hashes {
            infos.push(
                if let Ok(StateResponse::Block(block)) =
                    (call.state)(StateRequest::Block((hash.1).into())).await
                {
                    block
                } else {
                    None
                },
            )
        }
    }

    (hashes, infos)
}

fn dump_hash_highlight_lo(hash: &ZebBlockHash, highlight_chars_n: usize) {
    let hash_string = hash.to_string();
    let hash_str = hash_string.as_bytes();
    let bgn_col_str = "\x1b[90m".as_bytes(); // "bright black" == grey
    let end_col_str = "\x1b[0m".as_bytes(); // "reset"
    let grey_len = hash_str.len() - highlight_chars_n;

    let mut buf: [u8; 64 + 9] = [0; 73];
    let mut at = 0;
    buf[at..at + bgn_col_str.len()].copy_from_slice(bgn_col_str);
    at += bgn_col_str.len();

    buf[at..at + grey_len].copy_from_slice(&hash_str[..grey_len]);
    at += grey_len;

    buf[at..at + end_col_str.len()].copy_from_slice(end_col_str);
    at += end_col_str.len();

    buf[at..at + highlight_chars_n].copy_from_slice(&hash_str[grey_len..]);
    at += highlight_chars_n;

    let s = std::str::from_utf8(&buf[..at]).expect("invalid utf-8 sequence");
    print!("{}", s);
}

trait HasBlockHash {
    fn get_hash(&self) -> Option<ZebBlockHash>;
}
impl HasBlockHash for ZebBlockHash {
    fn get_hash(&self) -> Option<ZebBlockHash> {
        Some(*self)
    }
}
impl HasBlockHash for (ZebBlockHeight, ZebBlockHash) {
    fn get_hash(&self) -> Option<ZebBlockHash> {
        Some(self.1)
    }
}

/// "How many little-endian chars are needed to uniquely identify any of the blocks in the given
/// slice"
fn block_hash_unique_chars_n<T>(hashes: &[T]) -> usize
where
    T: HasBlockHash,
{
    let is_unique = |prefix_len: usize, hashes: &[T]| -> bool {
        let mut prefixes = HashSet::<ZebBlockHash>::with_capacity(hashes.len());

        // NOTE: characters correspond to nibbles
        let bytes_n = prefix_len / 2;
        let is_nib = (prefix_len % 2) != 0;

        for hash in hashes {
            if let Some(hash) = hash.get_hash() {
                let mut subhash = ZebBlockHash([0; 32]);
                subhash.0[..bytes_n].clone_from_slice(&hash.0[..bytes_n]);

                if is_nib {
                    subhash.0[bytes_n] = hash.0[bytes_n] & 0xf;
                }

                if !prefixes.insert(subhash) {
                    return false;
                }
            }
        }

        true
    };

    let mut unique_chars_n: usize = 1;
    while !is_unique(unique_chars_n, hashes) {
        unique_chars_n += 1;
        assert!(unique_chars_n <= 64);
    }

    unique_chars_n
}

fn tfl_dump_blocks(blocks: &[(ZebBlockHeight, ZebBlockHash)], infos: &[Option<Arc<Block>>]) {
    let highlight_chars_n = block_hash_unique_chars_n(blocks);

    let print_color = true;

    for (block_i, (_, hash)) in blocks.iter().enumerate() {
        print!("  ");
        if print_color {
            dump_hash_highlight_lo(hash, highlight_chars_n);
        } else {
            print!("{}", hash);
        }

        if let Some(Some(block)) = infos.get(block_i) {
            let shielded_c = block
                .transactions
                .iter()
                .filter(|tx| tx.has_shielded_data())
                .count();
            print!(
                " - {}, height: {}, work: {:?}, {:3} transactions ({} shielded)",
                block.header.time,
                block.coinbase_height().unwrap_or(ZebBlockHeight(0)).0,
                block.header.difficulty_threshold.to_work().unwrap(),
                block.transactions.len(),
                shielded_c
            );
        }

        println!();
    }
}

async fn _tfl_dump_block_sequence(
    call: &TFLServiceCalls,
    start_hash: ZebBlockHash,
    final_height_hash: Option<(ZebBlockHeight, ZebBlockHash)>,
    include_start_hash: bool,
) {
    let (blocks, infos) = tfl_block_sequence(
        call,
        start_hash,
        final_height_hash,
        include_start_hash,
        true,
    )
    .await;
    tfl_dump_blocks(&blocks[..], &infos[..]);
}

#[cfg(test)]
mod liveness_regression_tests {
    use super::*;

    fn roster_member(key: PubKeyID, voting_power: u64) -> RosterMember {
        RosterMember {
            pub_key: key.0,
            voting_power,
            txids: Vec::new(),
        }
    }

    #[test]
    fn raw_roster_identity_preserves_reversed_twins() {
        let mut bytes = [0u8; 32];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = index as u8;
        }
        let raw = PubKeyID(bytes);
        bytes.reverse();
        let reversed_twin = PubKeyID(bytes);
        let roster = tenderlink_roster_from_internal(
            &[roster_member(raw, 2), roster_member(reversed_twin, 1)],
            &HashSet::new(),
        );
        assert_eq!(roster.len(), 2);
        assert!(roster.iter().any(|member| member.pub_key == raw));
        assert!(roster.iter().any(|member| member.pub_key == reversed_twin));
    }

    #[test]
    fn peer_addresses_are_key_bound_and_survive_roster_changes() {
        let peer_a = "127.0.0.1:30111".to_owned();
        let peer_b = "127.0.0.1:30112".to_owned();
        let public_address = "127.0.0.1:30113";
        let key_a = PubKeyID([0x11; 32]);
        let key_b = PubKeyID([0x22; 32]);
        let noise_a = [0x31; 32];
        let noise_b = [0x32; 32];
        let mut local_bytes = [0u8; 32];
        for (index, byte) in local_bytes.iter_mut().enumerate() {
            *byte = (index as u8).wrapping_add(1);
        }
        let local = PubKeyID(local_bytes);
        let local_noise = tenderlink::bandwidth_test::new_keypair_from_connect_magic1_with_seed(
            tenderlink::CRYPTO_MAGIC,
            [0x33; 32],
        )
        .unwrap();
        let configured = vec![
            crate::config::BftPeerIdentity {
                consensus_public_key: "11".repeat(32),
                address: peer_a,
                noise_public_key: "31".repeat(32),
            },
            crate::config::BftPeerIdentity {
                consensus_public_key: "22".repeat(32),
                address: peer_b,
                noise_public_key: "32".repeat(32),
            },
        ];

        let as_map = || {
            finalizer_peer_addresses_from_explicit_config(
                &configured,
                public_address,
                local,
                &local_noise,
            )
            .unwrap()
            .into_iter()
            .map(|entry| (entry.bft_pk, entry.address))
            .collect::<HashMap<_, _>>()
        };
        let first = as_map();
        assert_eq!(first.len(), 3);
        assert_eq!(first.get(&key_a).unwrap().key, noise_a);
        assert_eq!(first.get(&key_b).unwrap().key, noise_b);
        assert_eq!(first.get(&local).unwrap().key, local_noise.public);
        // Configured endpoints are transport seeds even before their key enters a
        // post-recovery roster. Consensus messages remain authorized by the live
        // roster, not by presence in this address map.
        assert!(first.contains_key(&key_a));
    }

    #[test]
    fn transport_routes_never_grant_bootstrap_voting_power() {
        let mut config = crate::config::Config::default();
        config.bft_peer_identities.push(crate::config::BftPeerIdentity {
            consensus_public_key: "11".repeat(32),
            address: "127.0.0.1:30111".to_owned(),
            noise_public_key: "31".repeat(32),
        });
        assert!(bootstrap_roster_from_config(&config).unwrap().is_empty());

        config.bootstrap_bft_roster.push(crate::config::BftBootstrapRosterMember {
            consensus_public_key: "11".repeat(32),
            voting_power: 7,
        });
        let roster = bootstrap_roster_from_config(&config).unwrap();
        assert_eq!(roster, vec![roster_member(PubKeyID([0x11; 32]), 7)]);

        config.bootstrap_bft_roster.push(crate::config::BftBootstrapRosterMember {
            consensus_public_key: "11".repeat(32),
            voting_power: 8,
        });
        assert!(bootstrap_roster_from_config(&config).is_err());
    }

    #[test]
    fn pos_v2_frame_is_exact_hashed_and_preserves_context_marker() {
        let block = BftBlock {
            version: 2,
            height: 0,
            previous_block_fat_ptr: FatPointerToBftBlock::null(),
            headers: Vec::new(),
            hardforks: Vec::new(),
            do_not_include_until_bc_height: 0,
        };
        let pointer = FatPointerToBftBlock::from_parts(block.blake3_hash(), 0, 1, &[]);
        let next_roster = vec![roster_member(PubKeyID([7u8; 32]), 42)];
        let proposal_sigs = vec![TMSig([9u8; 64])];
        let frame = encode_pos_store_v2_frame(
            &block,
            &pointer,
            &next_roster,
            0,
            &proposal_sigs,
        )
        .unwrap();
        let decoded = decode_complete_pos_store_v2_frame(&frame).unwrap();
        assert!(decoded.is_v2);
        assert_eq!(decoded.block, block);
        assert_eq!(decoded.fat_pointer, pointer);
        assert_eq!(decoded.next_roster, next_roster);
        assert_eq!(decoded.proposal_valid_round, 0);
        assert_eq!(decoded.proposal_sigs, proposal_sigs);

        let mut corrupt = frame.clone();
        *corrupt.last_mut().unwrap() ^= 1;
        assert!(decode_complete_pos_store_v2_frame(&corrupt).is_err());
        assert!(decode_complete_pos_store_v2_frame(&frame[..frame.len() - 1]).is_err());
        for prefix_len in 1..frame.len() {
            assert!(is_exact_strict_frame_prefix(&frame[..prefix_len], &frame));
        }
        assert!(!is_exact_strict_frame_prefix(&frame, &frame));
        let mut mismatched_prefix = frame[..frame.len() - 1].to_vec();
        *mismatched_prefix.last_mut().unwrap() ^= 1;
        assert!(!is_exact_strict_frame_prefix(&mismatched_prefix, &frame));
    }

    #[test]
    fn indexed_pos_store_read_reauthenticates_without_moving_append_cursor() {
        let block = BftBlock {
            version: 1,
            height: 0,
            previous_block_fat_ptr: FatPointerToBftBlock::null(),
            headers: Vec::new(),
            hardforks: Vec::new(),
            do_not_include_until_bc_height: 0,
        };
        let pointer = FatPointerToBftBlock::from_parts(block.blake3_hash(), 0, 1, &[]);
        let next_roster = vec![roster_member(PubKeyID([7u8; 32]), 42)];
        let proposal_sigs = vec![TMSig([9u8; 64])];
        let frame = encode_pos_store_v2_frame(
            &block,
            &pointer,
            &next_roster,
            0,
            &proposal_sigs,
        )
        .unwrap();

        let mut append_file = tempfile::tempfile().unwrap();
        let prefix = b"held-prefix";
        append_file.write_all(prefix).unwrap();
        append_file.write_all(&frame).unwrap();
        let append_cursor = append_file.stream_position().unwrap();
        let read_file = append_file.try_clone().unwrap();
        let decoded = read_indexed_pos_store_record(
            &read_file,
            PosStoreRecordIndex {
                offset: prefix.len() as u64,
                len: frame.len() as u64,
                finalized_bc_height: 123,
            },
        )
        .unwrap();

        assert!(decoded.is_v2);
        assert_eq!(decoded.block, block);
        assert_eq!(decoded.fat_pointer, pointer);
        assert_eq!(decoded.next_roster, next_roster);
        assert_eq!(append_file.stream_position().unwrap(), append_cursor);
    }

    #[test]
    fn legacy_pos_payload_is_explicitly_contextless() {
        let block = BftBlock {
            version: 1,
            height: 0,
            previous_block_fat_ptr: FatPointerToBftBlock::null(),
            headers: Vec::new(),
            hardforks: Vec::new(),
            do_not_include_until_bc_height: 0,
        };
        let pointer = FatPointerToBftBlock::from_parts(block.blake3_hash(), 0, 1, &[]);
        let mut legacy = block.zcash_serialize_to_vec().unwrap();
        pointer.zcash_serialize(&mut legacy).unwrap();
        legacy.extend_from_slice(&1u64.to_le_bytes());
        roster_member(PubKeyID([7u8; 32]), 42).write_to_vec(&mut legacy);
        legacy.extend_from_slice(&1u64.to_le_bytes());
        legacy.extend_from_slice(&[9u8; 64]);

        let mut cursor = Cursor::new(&legacy);
        let decoded = read_stored_pos_decision_payload(&mut cursor, false).unwrap();
        assert!(!decoded.is_v2);
        assert_eq!(decoded.proposal_valid_round, -1);
        assert_eq!(decoded.proposal_sigs, vec![TMSig([9u8; 64])]);
        assert_eq!(cursor.position(), legacy.len() as u64);
    }

    #[test]
    fn torn_v2_tail_is_repaired_only_by_its_exact_certified_frame() {
        fn test_block() -> BftBlock {
            BftBlock {
                version: 2,
                height: 0,
                previous_block_fat_ptr: FatPointerToBftBlock::null(),
                headers: Vec::new(),
                hardforks: Vec::new(),
                do_not_include_until_bc_height: 0,
            }
        }
        fn internal_with_tail(
            path: PathBuf,
            file: File,
            prefix: Vec<u8>,
        ) -> TFLServiceInternal {
            TFLServiceInternal {
                my_public_key: PubKeyID::NIL,
                latest_final_block: None,
                tfl_is_activated: false,
                final_change_tx: broadcast::channel(1).0,
                bft_msg_flags: 0,
                bft_err_flags: 0,
                bft_blocks: Vec::new(),
                bft_height_by_hash: HashMap::new(),
                fat_pointer_to_tip: FatPointerToBftBlock::null(),
                our_set_bft_string: None,
                active_bft_string: None,
                peer_strings: Vec::new(),
                finalizers_keys_to_names: HashMap::new(),
                finalizers_at_current_height: Vec::new(),
                recency_status: TFLRecencyStatus::default(),
                current_bc_final: None,
                path_to_pos_store_file: path,
                pos_store_file: Some(file),
                pos_store_read_file: None,
                pos_store_records: Vec::new(),
                pending_reflush: None,
                pos_store_unverified_tail: Some(PosStoreTornTail {
                    offset: 0,
                    bytes: prefix,
                }),
            }
        }
        fn held_bytes(internal: &mut TFLServiceInternal) -> Vec<u8> {
            let file = internal.pos_store_file.as_mut().unwrap();
            file.seek(SeekFrom::Start(0)).unwrap();
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes).unwrap();
            bytes
        }

        let block = test_block();
        let pointer = FatPointerToBftBlock::from_parts(block.blake3_hash(), 0, 1, &[]);
        let roster = vec![roster_member(PubKeyID([7u8; 32]), 42)];
        let sigs = vec![TMSig([9u8; 64])];
        let expected = encode_pos_store_v2_frame(&block, &pointer, &roster, 0, &sigs).unwrap();
        let prefix = expected[..POS_STORE_V2_HEADER_LEN as usize].to_vec();

        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("repair.pos");
        let (mut file, _) = open_exclusive_pos_store(&path).unwrap();
        file.write_all(&prefix).unwrap();
        file.sync_all().unwrap();
        let mut internal = internal_with_tail(path, file, prefix.clone());
        append_pos_store_decision(&mut internal, &block, &pointer, &roster, 0, &sigs)
            .unwrap();
        assert!(internal.pos_store_unverified_tail.is_none());
        assert_eq!(held_bytes(&mut internal), expected);

        let path = temp.path().join("mismatch.pos");
        let (mut file, _) = open_exclusive_pos_store(&path).unwrap();
        file.write_all(&prefix).unwrap();
        file.sync_all().unwrap();
        let mut internal = internal_with_tail(path, file, prefix.clone());
        let mismatched_sigs = vec![TMSig([8u8; 64])];
        assert!(append_pos_store_decision(
            &mut internal,
            &block,
            &pointer,
            &roster,
            0,
            &mismatched_sigs,
        )
        .is_err());
        assert!(internal.pos_store_unverified_tail.is_some());
        assert_eq!(held_bytes(&mut internal), prefix);
    }

    #[test]
    fn secret_config_debug_is_redacted() {
        /*
        let secret: crate::config::SecretHex32 =
            serde_json::from_str(&format!("\{}\", "ab".repeat(32))).unwrap();
        */
        let secret: crate::config::SecretHex32 =
            serde_json::from_value(serde_json::Value::String("ab".repeat(32))).unwrap();
        let rendered = format!("{secret:?}");
        assert!(rendered.contains("REDACTED"));
        assert!(!rendered.contains(&"ab".repeat(32)));

        let legacy: crate::config::RedactedLegacySecret = serde_json::from_value(
            serde_json::Value::String("legacy-secret-material".to_owned()),
        )
        .unwrap();
        let mut config = crate::config::Config::default();
        config.explicit_bft_key_seed = Some(legacy);
        let rendered_config = format!("{config:?}");
        assert!(rendered_config.contains("REDACTED"));
        assert!(!rendered_config.contains("legacy-secret-material"));
    }

    #[test]
    fn legacy_or_partial_identity_never_enters_validator_mode() {
        let secret = || -> crate::config::SecretHex32 {
            serde_json::from_value(serde_json::Value::String("ab".repeat(32))).unwrap()
        };
        let mut config = crate::config::Config::default();
        assert!(!canonical_validator_identity_configured(&config).unwrap());

        config.validator_signing_key_seed = Some(secret());
        assert!(canonical_validator_identity_configured(&config).is_err());
        config.validator_consensus_public_key = Some("11".repeat(32));
        config.validator_noise_static_key_seed = Some(secret());
        assert!(canonical_validator_identity_configured(&config).unwrap());

        config.bft_peers.push("127.0.0.1:30111".to_owned());
        assert!(canonical_validator_identity_configured(&config).is_err());
    }

    #[test]
    fn exact_bft_decoder_rejects_a_valid_prefix_with_trailing_bytes() {
        let block = BftBlock {
            version: 0,
            height: 0,
            previous_block_fat_ptr: FatPointerToBftBlock::null(),
            headers: Vec::new(),
            hardforks: Vec::new(),
            do_not_include_until_bc_height: 0,
        };
        let mut bytes = block.zcash_serialize_to_vec().unwrap();
        assert_eq!(deserialize_bft_block_exact(&bytes).unwrap(), block);
        bytes.push(0);
        assert!(deserialize_bft_block_exact(&bytes).is_err());
    }

    #[test]
    fn bootstrap_receipt_hash_is_exact_lowercase_nonzero_hex() {
        assert_eq!(
            decode_exact_lower_hex_32(&"01".repeat(32)).unwrap(),
            [1u8; 32]
        );
        assert!(decode_exact_lower_hex_32(&"00".repeat(32)).is_err());
        assert!(decode_exact_lower_hex_32(&"AA".repeat(32)).is_err());
        assert!(decode_exact_lower_hex_32(&"01".repeat(31)).is_err());
        assert!(decode_exact_lower_hex_32(&format!("{}g1", "01".repeat(31))).is_err());
    }

    fn migration_context<'a>(
        wal_path: &'a Path,
        anchor_path: &'a Path,
        pos_store_path: &'a Path,
    ) -> SignerMigrationContext<'a> {
        SignerMigrationContext {
            validator_consensus_public_key: PubKeyID([0x11; 32]),
            chain_id: [0x12; 32],
            startup_bft_height: 7,
            parent_commit: [0x13; 32],
            vote_namespace: [0x14; 32],
            consensus_config_hash: [0x15; 32],
            authenticated_bootstrap_roster_hash: [0x16; 32],
            active_roster_hash: [0x17; 32],
            active_roster_index: 1,
            active_roster_len: 3,
            finalized_pow_height: 42,
            finalized_pow_hash: [0x18; 32],
            pos_store_size_bytes: 12_345,
            pos_store_record_count: 7,
            pos_store_complete_eof: true,
            wal_path,
            anchor_path,
            pos_store_path,
        }
    }

    fn valid_migration_receipt(context: &SignerMigrationContext<'_>) -> serde_json::Value {
        serde_json::json!({
            "schema": SIGNER_MIGRATION_RECEIPT_SCHEMA,
            "action": SIGNER_MIGRATION_RECEIPT_ACTION,
            "operator_authorized": true,
            "independent_anchor_authorized": true,
            "global_single_signer_fence_confirmed": true,
            "frozen_legacy_binary_sha256": "21".repeat(32),
            "frozen_legacy_config_sha256": "22".repeat(32),
            "composite_checkpoint_manifest_sha256": "23".repeat(32),
            "pos_store_sha256": "24".repeat(32),
            "pos_store_size_bytes": context.pos_store_size_bytes,
            "pos_store_complete_eof": true,
            "pos_store_record_count": context.pos_store_record_count,
            "pos_store_first_bft_height": 0,
            "validator_consensus_public_key": "11".repeat(32),
            "chain_id": "12".repeat(32),
            "replayed_next_bft_height": context.startup_bft_height,
            "bootstrap_parent_commit": "13".repeat(32),
            "bootstrap_vote_namespace": "14".repeat(32),
            "bootstrap_consensus_config_hash": "15".repeat(32),
            "authenticated_bootstrap_roster_hash": "16".repeat(32),
            "active_roster_hash": "17".repeat(32),
            "active_roster_index": context.active_roster_index,
            "active_roster_len": context.active_roster_len,
            "finalized_pow_height": context.finalized_pow_height,
            "finalized_pow_hash": "18".repeat(32),
            "peer_route_map_blake3": "25".repeat(32),
            "peer_route_voting_power": 5,
            "required_route_voting_power": 5,
            "legacy_signer_fence_receipt_sha256": "26".repeat(32),
            "wal_path": context.wal_path,
            "anchor_path": context.anchor_path,
            "pos_store_path": context.pos_store_path,
        })
    }

    #[test]
    fn structured_migration_receipt_is_exact_and_context_bound() {
        let temp = tempfile::tempdir().unwrap();
        let wal = temp.path().join("signer.wal");
        let anchor = temp.path().join("signer.anchor");
        let pos = temp.path().join("pos.chain");
        let context = migration_context(&wal, &anchor, &pos);
        let value = valid_migration_receipt(&context);
        let bytes = serde_json::to_vec(&value).unwrap();
        let pinned: [u8; 32] = blake3::hash(&bytes).into();
        assert_eq!(
            verify_signer_migration_receipt(
                &bytes,
                pinned,
                SignerJournalState::Uninitialized,
                &context,
            )
            .unwrap()
            .non_genesis_receipt_hash,
            Some(pinned),
        );

        let mut wrong_context = value.clone();
        wrong_context["active_roster_hash"] = serde_json::Value::String("31".repeat(32));
        let wrong_bytes = serde_json::to_vec(&wrong_context).unwrap();
        let wrong_pin: [u8; 32] = blake3::hash(&wrong_bytes).into();
        assert!(verify_signer_migration_receipt(
            &wrong_bytes,
            wrong_pin,
            SignerJournalState::Uninitialized,
            &context,
        )
        .is_err());

        let mut unknown = value;
        unknown["unsealed_extra_authority"] = serde_json::Value::Bool(true);
        let unknown_bytes = serde_json::to_vec(&unknown).unwrap();
        let unknown_pin: [u8; 32] = blake3::hash(&unknown_bytes).into();
        assert!(verify_signer_migration_receipt(
            &unknown_bytes,
            unknown_pin,
            SignerJournalState::Uninitialized,
            &context,
        )
        .is_err());
    }

    #[test]
    fn complete_authority_gate_is_read_only_until_every_binding_passes() {
        let temp = tempfile::tempdir().unwrap();
        let wal = temp.path().join("signer.wal");
        let anchor = temp.path().join("signer.anchor");
        let pos = temp.path().join("pos.chain");
        let receipt_path = temp.path().join("migration-receipt.json");
        let context = migration_context(&wal, &anchor, &pos);
        let bytes = serde_json::to_vec(&valid_migration_receipt(&context)).unwrap();
        let pinned: [u8; 32] = blake3::hash(&bytes).into();

        let mut config = crate::config::Config::default();
        config.signer_independent_anchor_authorized = true;
        assert!(signer_startup_authority(&config, &context).is_err());
        assert!(!wal.exists());
        assert!(!anchor.exists());

        std::fs::write(&receipt_path, &bytes).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&receipt_path, std::fs::Permissions::from_mode(0o600))
                .unwrap();
        }
        config.signer_non_genesis_bootstrap_receipt_blake3 = Some(hex::encode(pinned));
        config.signer_non_genesis_bootstrap_receipt_path = Some(receipt_path);
        assert_eq!(
            signer_startup_authority(&config, &context)
                .unwrap()
                .non_genesis_receipt_hash,
            Some(pinned),
        );
        assert!(!wal.exists(), "authority check created a WAL");
        assert!(!anchor.exists(), "authority check created an anchor");
    }

    #[test]
    fn stored_roster_rejects_transaction_detail_vectors() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&[7u8; 32]);
        bytes.extend_from_slice(&42u64.to_le_bytes());
        bytes.extend_from_slice(&1u64.to_le_bytes());
        assert!(read_stored_roster_member(&mut Cursor::new(bytes)).is_err());

        let mut canonical = Vec::new();
        canonical.extend_from_slice(&[7u8; 32]);
        canonical.extend_from_slice(&42u64.to_le_bytes());
        canonical.extend_from_slice(&0u64.to_le_bytes());
        assert_eq!(
            read_stored_roster_member(&mut Cursor::new(canonical)).unwrap(),
            roster_member(PubKeyID([7u8; 32]), 42)
        );
    }

    #[test]
    fn strict_store_semantics_rejects_headerless_history() {
        let block = BftBlock {
            version: 0,
            height: 0,
            previous_block_fat_ptr: FatPointerToBftBlock::null(),
            headers: Vec::new(),
            hardforks: Vec::new(),
            do_not_include_until_bc_height: 0,
        };
        assert!(validate_stored_bft_semantics(
            &crate::config::Config::default(),
            &block,
            None,
            0,
            &FatPointerToBftBlock::null(),
        )
        .is_err());
    }
}
