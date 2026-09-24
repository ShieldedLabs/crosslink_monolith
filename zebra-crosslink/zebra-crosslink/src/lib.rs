//! Internal Zebra service for managing the Crosslink consensus protocol

#![allow(clippy::print_stdout)]
#![allow(unexpected_cfgs, unused, missing_docs)]

use async_trait::async_trait;

use zebra_chain::serialization::ZcashSerialize;
use zcash_primitives::block::BlockHash;
use zcash_primitives::transaction::RosterMember;
use zebra_state::crosslink::*;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::Instant;
use tracing::{error, info, warn};

use zcash_primitives::bft::*;
use zcash_protocol::consensus::{BlockHeight, TEST_NETWORK};

pub use wallet;

use std::sync::Mutex;
use tokio::sync::Mutex as TokioMutex;

pub static TEST_INSTR_C: Mutex<usize> = Mutex::new(0);
pub static TEST_MODE: Mutex<bool> = Mutex::new(false);
pub static TEST_FAILED: Mutex<i32> = Mutex::new(0);
pub static TEST_FAILED_INSTR_IDXS: Mutex<Vec<(usize, String)>> = Mutex::new(Vec::new());
pub static TEST_CHECK_ASSERT: Mutex<u8> = Mutex::new(1);
pub static TEST_INSTR_PATH: Mutex<Option<std::path::PathBuf>> = Mutex::new(None);
pub static TEST_INSTR_BYTES: Mutex<Vec<u8>> = Mutex::new(Vec::new());
pub static TEST_INSTRS: Mutex<Vec<test_format::TFInstr>> = Mutex::new(Vec::new());
pub static TEST_SHUTDOWN_FN: Mutex<fn()> = Mutex::new(|| ());
pub static TEST_NAME: Mutex<&'static str> = Mutex::new("‰‰TEST_NAME_NOT_SET‰‰");

/// Runtime-configurable failure handling, ported from reece_smith_merchant. A wrapped
/// `Result`/`Option` panics only when `on_fail` carries `PANIC`, otherwise it is logged
/// (`LOG`) or returned untouched. This lets one code path assert-fail for a human running a
/// test yet hand the error back to the fuzzer grinding through malformed inputs, without
/// duplicating the path.
#[allow(dead_code)]
pub mod uhh {
    pub const LOG: u32 = 1 << 0;
    pub const CALLSTACK: u32 = 1 << 1;
    pub const PANIC: u32 = 1 << 2;
}

pub fn uhh<T, E: std::fmt::Debug>(result: Result<T, E>, on_fail: u32) -> Result<T, E> {
    if let Err(e) = &result {
        if on_fail & (uhh::LOG | uhh::PANIC) != 0 {
            eprintln!("{:?}", e);
        }
        // CALLSTACK backtrace is not implemented (matches the source); never set it here.
        if on_fail & uhh::PANIC != 0 {
            panic!("error marked as unrecoverable");
        }
    }
    result
}

pub fn uhh_option<T>(option: Option<T>, on_fail: u32) -> Option<T> {
    if option.is_none() {
        if on_fail & (uhh::LOG | uhh::PANIC) != 0 {
            eprintln!("Option of '{}' was None.", std::any::type_name::<T>());
        }
        if on_fail & uhh::PANIC != 0 {
            panic!("error marked as unrecoverable");
        }
    }
    option
}

/// Failure mode for the test-format load path (see [`uhh`]). Normal tests keep `PANIC`, so
/// malformed data aborts; the fuzzer clears `PANIC` so the same path recovers and keeps
/// grinding.
pub static TEST_ON_FAIL: Mutex<u32> = Mutex::new(uhh::PANIC);

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

    // The canonical hardfork types live in `zebra-chain` so that zebra-state and
    // zebra-consensus — which cannot depend on zebra-crosslink — can share them.
    // Re-exported here for ergonomic access via `zebra_crosslink::config::*`.
    pub use zebra_chain::parameters::hardfork::{
        shipped_hardforks, HardForkConfig, HardForkSchedule,
    };

    /// Configuration for the state service.
    #[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
    #[serde(deny_unknown_fields, default)]
    pub struct Config {
        /// Public address for this node, e.g. "/ip4/127.0.0.1/udp/24834/quic-v1" if testing
        /// internally, or the public IP address if using externally.
        pub public_address: Option<String>,
        /// Use the public IP instead of the generated seed
        pub explicit_bft_key_seed: Option<String>,
        /// List of public IP addresses for peers, in the same format as `public_address`.
        pub bft_peers: Vec<String>,
        /// Disable the headless wallet.
        pub disable_the_headless_wallet: bool,
        /// Disable lightwallet_server.
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
    }
    impl Default for Config {
        fn default() -> Self {
            Self {
                public_address: None,
                bft_peers: Vec::new(),
                explicit_bft_key_seed: None,
                disable_the_headless_wallet: false,
                disable_zaino: false,
                hardforks: Vec::new(),
                disable_shipped_hardforks: false,
            }
        }
    }
}

pub mod test_format;

#[cfg(feature = "viz_gui")]
pub mod viz2;

use crate::service::{TFLServiceCalls, TFLServiceHandle};

// TODO: do we want to start differentiating BCHeight/PoWHeight, MalHeight/PoSHeigh etc?
use zebra_chain::block::{
    Block, CountedHeader, Hash as ZebBlockHash, Header as ZebBlockHeader, Height as ZebBlockHeight,
};
use zebra_node_services::mempool::{Request as MempoolRequest, Response as MempoolResponse};
use zebra_state::{crosslink::*, Request as StateRequest, Response as StateResponse, ReadRequest as StateReadRequest, ReadResponse as StateReadResponse};

pub use zebra_state::new_network::bft::bc_hdr_to_lrz;

#[allow(dead_code)]
#[derive(Debug)]
pub(crate) struct TFLServiceInternal {
    bft_msg_flags: u64, // ALT: Vec of messages/combine flags
    bft_err_flags: u64,
    our_set_bft_string: Option<String>,
    active_bft_string: Option<String>,
}

/// Recomputes, for a block already on the chain, the PoS-issuance decision that bc-block
/// admission made when that block was committed.
///
/// The live rule lives in `zebra_state::new_network::bft::admit_fat_pointer`, which is where
/// both facts are known; replay paths (here, the wallet issuance projection) have to
/// reconstruct it from committed data. Both halves are objective: the
/// certificate identity comes from the two block headers, and what that certificate finalizes
/// comes from the decided BFT block, which is immutable once decided.
///
/// Returns an error rather than `false` when the certificate cannot be resolved: silently
/// skipping a payout would understate issuance without saying so.
async fn block_pays_pos_issuance(
    internal_handle: &TFLServiceHandle,
    block_height: ZebBlockHeight,
    fat_pointer: &FatPointerToBftBlock,
    parent_fat_pointer: &FatPointerToBftBlock,
) -> Result<bool, String> {
    // No advance, no payout. Also covers every pre-activation block, where both pointers are null.
    if fat_pointer.points_at_block_hash() == parent_fat_pointer.points_at_block_hash() {
        return Ok(false);
    }

    let snapshot_hash = {
        let chain = zebra_state::new_network::bft::bft_chain().read().unwrap();
        match chain.hash_to_height.get(&fat_pointer.points_at_block_hash()) {
            Some(&h) if !chain.blocks[h as usize].headers.is_empty() => {
                ZebBlockHash(chain.blocks[h as usize].snapshot_block_hash().0)
            }
            _ => {
                return Err(format!(
                    "block at height {} carries a certificate this node cannot resolve ({:?});                      its issuance cannot be replayed",
                    block_height.0,
                    fat_pointer.points_at_block_hash(),
                ))
            }
        }
    };

    let Some(snapshot_height) = block_height_from_hash(&internal_handle.call, snapshot_hash).await
    else {
        return Err(format!(
            "the block finalized by the certificate in block {} is not in this database",
            block_height.0,
        ));
    };

    let sigma = internal_handle.params.bc_confirmation_depth_sigma;
    let gap = (block_height.0 as u64).saturating_sub(snapshot_height.0 as u64);
    Ok(gap <= sigma + zcash_primitives::bft::FINALITY_LIVENESS_ALLOWANCE)
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

async fn _block_prev_hash_from_hash(call: &TFLServiceCalls, hash: ZebBlockHash) -> Option<ZebBlockHash> {
    if let Ok(StateResponse::BlockHeader { header, .. }) =
        (call.state)(StateRequest::BlockHeader(hash.into())).await
    {
        Some(header.previous_block_hash)
    } else {
        None
    }
}

// NAME: rng_sk_pk_from_addr
// The derivation itself is shared with the wallet (see `bft::finalizer_key_from_seed`),
// which needs the same key to authorize finalizer reward conversions.
pub fn rng_private_public_key_from_address(
    addr: &[u8],
) -> (rand::rngs::StdRng, ed25519_zebra::SigningKey, PubKeyID) {
    zcash_primitives::bft::finalizer_key_from_seed(addr)
}

const MAIN_LOOP_SLEEP_INTERVAL: Duration = Duration::from_millis(125);
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

async fn tfl_service_main_loop(internal_handle: TFLServiceHandle) -> Result<(), String> {
    let call = internal_handle.call.clone();
    let config = internal_handle.config.clone();
    let params = internal_handle.params;

    #[cfg(feature = "viz_gui")]
    {
        let rt = tokio::runtime::Handle::current();
        let viz_tfl_handle = internal_handle.clone();
        tokio::task::spawn_blocking(move || {
            rt.block_on(viz2::service_viz_requests(viz_tfl_handle, params))
        });

        *wallet::RECENCY_REQUEST.lock().unwrap() = Some(wallet::RecencyRequestClosure(Arc::new(move || {
            serde_json::to_string_pretty(&zebra_state::new_network::bft::bft_recency_status()).ok()
        })));
    }

    if *TEST_MODE.lock().unwrap() {
        run_tfl_test(internal_handle.clone());
    }

    // The BFT chain, the engine and every reader of them live in `zebra_state::new_network::bft`.
    // What is left here are the request handlers, which run on their own tasks; this loop only
    // keeps the service task alive, since zebrad treats its exit as a shutdown.
    loop {
        tokio::time::sleep(MAIN_LOOP_SLEEP_INTERVAL).await;
    }
}

async fn total_issuance_from_key(
    internal_handle: TFLServiceHandle,
    ufvks: Vec<zcash_keys::keys::UnifiedFullViewingKey>,
    first_height: ZebBlockHeight,
    last_height: ZebBlockHeight,
) -> Result<Vec<ScanInfo>, String> {
    use std::sync::atomic::Ordering::Relaxed;
    use futures::StreamExt;
    use wallet::scanner::{PROF, timed};

    let call = internal_handle.call.clone();
    let t_wall = std::time::Instant::now();
    PROF.reset();

    let hardfork_schedule = zebra_chain::parameters::HardForkSchedule::from_canonical(internal_handle.config.hardforks.clone());
    let mut staking = zebra_state::StakingReplay::new(&hardfork_schedule);
    // The certificate carried by the previously scanned block, to tell whether the next one
    // advances it. `None` until the first block of the range, whose parent is outside it.
    let mut prev_fat_pointer: Option<FatPointerToBftBlock> = None;
    let mut utxos_per_ufvk = vec![HashSet::<(PubKeyID, u32)>::new(); ufvks.len()]; // NOTE: hashsets here are grow-only
    let mut t_spend_per_ufvk = vec![false; ufvks.len()];

    let mut scan_infos = Vec::<ScanInfo>::with_capacity(ufvks.len());
    let mut scan_ctxs = Vec::<wallet::scanner::ScanCtx>::with_capacity(ufvks.len());
    for ufvk in &ufvks {
        scan_infos.push(ScanInfo { ufvk: ufvk.encode(&TEST_NETWORK), ..ScanInfo::default() });

        let external_keys = wallet::PreparedKeys::from_ufvk_all(&ufvk);
        let internal_keys = wallet::PreparedKeys::from_ufvk_all_internal(&ufvk);
        let (Some(orchard_external_ovk), Some(orchard_internal_ovk)) = (external_keys.orchard_ovk, internal_keys.orchard_ovk) else {
            return Err("could not create orchard ovks".to_owned());
        };

        let Some((t_addr, t_addr_p2sh, _ua)) = wallet::addrs_from_ufvk(ufvk, 0) else{
            return Err("Could not get an address".to_owned());
        };

        scan_ctxs.push(wallet::scanner::ScanCtx::new(ufvk.clone(), t_addr, t_addr_p2sh, orchard_external_ovk, orchard_internal_ovk));
    }

    // Blocks are requested PREFETCH ahead through the concurrent ReadStateService, so the rocksdb
    // reads and zebra deserialization of the next blocks overlap with scanning this one. The
    // fetch bucket then measures the stall waiting for a block, not the read itself.
    const PREFETCH: usize = 16;
    let read_state = call.read_state.clone();
    let mut blocks = std::pin::pin!(futures::stream::iter(first_height.0..=last_height.0)
        .map(move |height| {
            let read_state = read_state.clone();
            async move { (height, (read_state)(StateReadRequest::Block(ZebBlockHeight(height).into())).await) }
        })
        .buffered(PREFETCH));

    loop {
        let t_fetch = std::time::Instant::now();
        let Some((height, res)) = blocks.next().await else { break };
        PROF.fetch_ns.fetch_add(t_fetch.elapsed().as_nanos() as u64, Relaxed);
        PROF.blocks.fetch_add(1, Relaxed);
        if height % 1000 == 0 {
            println!("scanning height {height}");
        }
        let block = match res {
            Ok(StateReadResponse::Block(Some(block))) => block,
            Ok(StateReadResponse::Block(None)) => return Err(format!("failed to get block at height {height}")),
            _ => return Err(format!("unexpectedly failed to get block at height {height}: {res:?}")),
        };

        if block.transactions.len() == 0 {
            return Err(format!("block at height {height} had 0 transactions"));
        }

        // The live path burns before the activation block's staking actions.
        if height != 0 && staking.slash_activates_at(ZebBlockHeight(height)) {
            let mut window_blocks = Vec::new();
            for window_height in zebra_state::slash_window(ZebBlockHeight(height)) {
                match (call.read_state)(StateReadRequest::Block(window_height.into())).await {
                    Ok(StateReadResponse::Block(Some(window_block))) => window_blocks.push(window_block),
                    _ => return Err(format!("failed to get block at height {} in the slash window", window_height.0)),
                }
            }
            if let Some(slash) = timed(&PROF.replay_ns, || staking.apply_slash_burns(ZebBlockHeight(height), window_blocks)) {
                println!("applied hardfork slash burns at height {height}: {} bond(s) burned for {} terminated finalizer(s)", slash.burned.len(), slash.finalizers.len());
            }
        }

        for (tx_i, tx) in block.transactions.iter().enumerate() {
            PROF.txs.fetch_add(1, Relaxed);
            let is_coinbase = tx.is_coinbase();
            if tx_i == 0 && ! is_coinbase {
                return Err(format!("no coinbase found at height {height}"));
            }

            // The transparent pass reads zebra's already-decoded inputs and outputs. The txid is
            // hashed only when an output is ours, at most once per tx across the ufvks.
            let mut txid_memo: Option<[u8; 32]> = None;
            let mut txid = || *txid_memo.get_or_insert_with(|| timed(&PROF.txid_ns, || tx.hash().0));
            for (ufvk_i, scan_ctx) in scan_ctxs.iter().enumerate() {
                let inputs = tx.inputs().iter().filter_map(|input| match input {
                    zebra_chain::transparent::Input::PrevOut { outpoint, .. } => Some((outpoint.hash.0, outpoint.index)),
                    _ => None,
                });
                let outputs = tx.outputs().iter().map(|output| output.lock_script.as_raw_bytes());
                let scan_info = &mut scan_infos[ufvk_i];
                let utxos = &mut utxos_per_ufvk[ufvk_i];
                match timed(&PROF.transparent_ns, || wallet::scanner::scan_tx_transparent(scan_info, utxos, scan_ctx, height, is_coinbase, inputs, outputs, &mut txid)) {
                    Ok((new_info, contains_my_t_spend)) => {
                        t_spend_per_ufvk[ufvk_i] = contains_my_t_spend;
                        if new_info {
                            println!("scan info at {height}: {scan_info:?}");
                        }
                    }
                    Err(err) => return Err(format!("failed to scan tx {tx_i} at height {height}: {err}")),
                }
            }

            // Only staking txs need the librustzcash view (bond terms, Orchard trial decryption).
            let staking_action = tx.staking_action();
            let parsed = if staking_action.is_some() {
                PROF.staking.fetch_add(1, Relaxed);
                let tx_bytes = match timed(&PROF.serialize_ns, || tx.zcash_serialize_to_vec()) {
                    Ok(bytes) => bytes,
                    Err(err) => return Err(format!("failed to serialize tx {tx_i} at height {height}: {err:?}")),
                };
                match timed(&PROF.parse_ns, || wallet::scanner::parse_tx(&tx_bytes, height)) {
                    Ok(parsed) => Some(parsed),
                    Err(err) => return Err(format!("failed to parse tx {tx_i} at height {height}: {err}")),
                }
            } else {
                None
            };

            if let (Some(staking_action), Some((_, txid_lrz))) = (staking_action, &parsed) {
                debug_assert_eq!(*txid_lrz, tx.hash().0, "txids from zebra/librustzcash disagree");
                // Note(Sam): It seems weird that the bonds never get deleted. I don't know what I was
                // thinking when I did that. But it makes this code easy.
                let location = zebra_state::TransactionLocation {
                    height: ZebBlockHeight(height),
                    index: zebra_state::TransactionIndex::from_index(tx_i.try_into().unwrap()),
                };
                let replayed = timed(&PROF.replay_ns, || staking.apply_staking_action(staking_action, &zebra_chain::transaction::Hash(*txid_lrz), location));
                if let Err(err) = replayed {
                    return Err(format!("failed to replay the staking action of tx {tx_i} at height {height}: {err}"));
                }
            }

            if let Some((tx_lrz, txid_lrz)) = &parsed {
                for (ufvk_i, scan_ctx) in scan_ctxs.iter().enumerate() {
                    let scan_info = &mut scan_infos[ufvk_i];
                    if wallet::scanner::scan_tx_staking(scan_info, tx_lrz, *txid_lrz, t_spend_per_ufvk[ufvk_i], height, scan_ctx) {
                        println!("scan info at {height}: {scan_info:?}");
                    }
                }
            }
        }

        // PoS issuance is applied once per block, after that block's staking actions, exactly as
        // the live commit path does -- and only for blocks that pay under the variable payout
        // rule. `block_pays_pos_issuance` replays that decision.
        let fat_pointer = block.header.fat_pointer_to_bft_block.clone();
        let parent_fat_pointer = match &prev_fat_pointer {
            Some(fat_pointer) => fat_pointer.clone(),
            None if height == 0 => FatPointerToBftBlock::null(),
            None => {
                // First block of the range: its parent was not scanned, so read its header.
                match (call.read_state)(StateReadRequest::Block(ZebBlockHeight(height - 1).into())).await {
                    Ok(StateReadResponse::Block(Some(parent))) => parent.header.fat_pointer_to_bft_block.clone(),
                    _ => return Err(format!("failed to get block at height {} to read its certificate", height - 1)),
                }
            }
        };
        if height != 0
            && block_pays_pos_issuance(&internal_handle, ZebBlockHeight(height), &fat_pointer, &parent_fat_pointer).await?
        {
            timed(&PROF.replay_ns, || staking.apply_block_reward());
        }

        prev_fat_pointer = Some(fat_pointer);
    }

    for scan_info in &mut scan_infos {
        let mut bonds_value = 0;
        let mut kept_bonds = Vec::with_capacity(scan_info.bonds.len());
        for bond in std::mem::take(&mut scan_info.bonds) {
            let Some((bond_state, status)) = staking.delegation_bonds.get(&bond.pk.0) else {
                return Err(format!("couldn't find bond {:?}", bond));
            };
            let initial_val: u64 = bond.initial_val;
            let final_val = u64::from(bond_state.amount);
            let issuance_gained = final_val - initial_val;
            let burned = *status == zebra_state::BondStatusInChain::Burned;
            println!("bond {:?}: initial value = {}; final value = {}; gained {}; burned {}", bond, initial_val, final_val, issuance_gained, burned);
            if burned {
                scan_info.burned_bonds_initial_value += initial_val;
                scan_info.burned_bonds_value += issuance_gained;
                scan_info.burned_bonds.push(bond);
            } else {
                bonds_value += issuance_gained;
                kept_bonds.push(bond);
            }
        }

        scan_info.bonds = kept_bonds;
        scan_info.bonds_value = bonds_value;
        scan_info.total_value = scan_info.coinbases_value + scan_info.bonds_value;
        println!("final scan info: {scan_info:?}");
    }

    PROF.report(t_wall.elapsed());
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

        // As with staking, only the wallet can fund and build the transaction, so the request is
        // staged for the wallet loop and we wait on the channel it answers with.
        TFLServiceRequest::WalletBasicSend(value_zats, address) => Ok(TFLServiceResponse::WalletBasicSend({
            let rx = {
                let mut lock = wallet::BASIC_SEND_STAGE.lock().unwrap();
                match *lock {
                    None => {
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        *lock = Some((value_zats, address, tx));
                        rx
                    }
                    Some(_) => {
                        return Err(TFLServiceError::Misc(
                            "Another send in progress, please try again soon".to_string(),
                        ))
                    }
                }
            };

            match rx.await {
                Ok(result) => result,
                Err(err) => return Err(TFLServiceError::Misc(format!("{err}"))),
            }
        })),

        TFLServiceRequest::WalletStakingPositions => Ok(TFLServiceResponse::WalletStakingPositions(wallet::STAKING_POSITIONS.lock().unwrap().clone())),

        TFLServiceRequest::WalletSpendableFunds => Ok(TFLServiceResponse::WalletSpendableFunds(wallet::SPENDABLE_FUNDS.lock().unwrap().clone())),

        // workshop - mining & staking via PoW
        TFLServiceRequest::TotalIssuanceFromKey(ufvk_str, first_height, last_height) => {
            Ok(TFLServiceResponse::TotalIssuanceFromKey({
                total_issuance_from_key(internal_handle.clone(), ufvk_str, first_height, last_height).await
            }))
        }

        // `staking_command` takes the request as a JSON string so the RPC surface stays a single
        // stringly-typed method, and dispatches it down the same staging path as
        // `WalletStakingAction`. The wallet is the only thing that can build and fund a staking
        // transaction, so both entry points must funnel into `wallet::STAKING_STAGE`.
        TFLServiceRequest::StakingCmd(cmd) => {
            let request: zcash_primitives::transaction::StakingActionRequest = serde_json::from_str(&cmd).map_err(|err| {
                TFLServiceError::Misc(format!(
                    "staking command must be a JSON StakingActionRequest, e.g. \
                     {{\"CreateNewDelegationBond\":{{\"amount_zats\":100000,\"target_finalizer\":\"<zfin address>\"}}}}: {err}"
                ))
            })?;

            let rx = {
                let mut lock = wallet::STAKING_STAGE.lock().unwrap();
                match *lock {
                    None => {
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        *lock = Some((request, tx));
                        rx
                    }
                    Some(_) => {
                        return Err(TFLServiceError::Misc(
                            "Another stake in progress, please try again soon".to_string(),
                        ))
                    }
                }
            };

            match rx.await {
                Ok(Ok(_)) => Ok(TFLServiceResponse::StakingCmd),
                Ok(Err(err)) => Err(TFLServiceError::Misc(err)),
                Err(err) => Err(TFLServiceError::Misc(format!("{err}"))),
            }
        }

        TFLServiceRequest::WalletUfvk => Ok(TFLServiceResponse::WalletUfvk(wallet::USER_UFVK_STRING.lock().unwrap().clone())),
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

/// Blocks in `[lo_height ..= hi_height]` on `anchor`'s chain as one chain, ascending by
/// height, at most `max_len` of them counting down. `hi_height` is clamped to `anchor`'s own
/// height. Empty if the state holds no such run right now.
///
/// The state resolves the heights within `anchor`'s chain and walks parent links inside one
/// snapshot, so the result is a single chain even when `anchor` is not on the best chain, and
/// a reorganization can only shorten it. Callers that pass a tip hash they read earlier get a
/// run from that tip's chain or nothing, never a mixture; they redraw from whatever comes back
/// and retry, and there is nothing here to assert.
async fn tfl_block_sequence(
    call: &TFLServiceCalls,
    anchor: ZebBlockHash,
    hi_height: ZebBlockHeight,
    lo_height: ZebBlockHeight,
    max_len: u32,
) -> Vec<(ZebBlockHeight, ZebBlockHash, Arc<Block>)> {
    if let Ok(StateReadResponse::BlockSequence(seq)) = (call.read_state)(
        StateReadRequest::BlockSequence { anchor, hi_height, lo_height, max_len },
    )
    .await
    {
        seq
    } else {
        Vec::new()
    }
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
impl HasBlockHash for (ZebBlockHeight, ZebBlockHash, Arc<Block>) {
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

fn tfl_dump_blocks(blocks: &[(ZebBlockHeight, ZebBlockHash, Arc<Block>)]) {
    let highlight_chars_n = block_hash_unique_chars_n(blocks);

    let print_color = true;

    for (_, hash, block) in blocks.iter() {
        print!("  ");
        if print_color {
            dump_hash_highlight_lo(hash, highlight_chars_n);
        } else {
            print!("{}", hash);
        }

        {
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
    anchor: ZebBlockHash,
    hi_height: ZebBlockHeight,
    lo_height: ZebBlockHeight,
    max_len: u32,
) {
    let blocks = tfl_block_sequence(call, anchor, hi_height, lo_height, max_len).await;
    tfl_dump_blocks(&blocks[..]);
}
