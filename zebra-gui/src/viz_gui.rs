#![allow(warnings)]

use std::{collections::HashMap, hash::Hash, sync::Mutex};

use chrono::{DateTime, Utc};
use wallet::BlockHeight;
// use twox_hash::XxHash3_64;
use winit::event::MouseButton;

use super::*;

pub static REQUESTS_TO_ZEBRA: Mutex<Option<std::sync::mpsc::Receiver<RequestToZebra>>> = Mutex::new(None);
pub static RESPONSES_FROM_ZEBRA: Mutex<Option<std::sync::mpsc::SyncSender<ResponseFromZebra>>> = Mutex::new(None);

pub struct RequestToZebra {
    pub want_to_inspect_block: Hash32,
    pub bft_ack_height: u64,
    pub bc_ack_height: u64,
}
impl RequestToZebra {
    pub fn _0() -> Self {
        RequestToZebra {
            want_to_inspect_block: Hash32::from_u64(0),
            bft_ack_height: 0,
            bc_ack_height: 0,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub enum BlockInspection {
    #[default]
    None,
    Pow(PowBlockInspection),
    Bft(BftBlockInspection),
}

#[derive(Clone, Debug)]
pub struct PowBlockInspection {
    pub hash: Hash32,
    pub height: Option<u64>,
    pub parent_hash: Hash32,
    pub time: i64,
    pub fat_pointer: String,
    pub transactions: Vec<TxInspection>,
}

#[derive(Clone, Debug)]
pub struct BftBlockInspection {
    pub hash: Hash32,
    pub version: u32,
    pub height: u32,
    pub previous_hash: Hash32,
    pub finalization_candidate_height: u32,
    pub do_not_include_until_bc_height: u64,
    pub hardforks: Vec<HardforkInspection>,
    pub pow_headers: Vec<BftPowHeaderInspection>,
}

#[derive(Clone, Debug)]
pub struct HardforkInspection {
    pub pow_activation_height: u64,
    pub bft_certificate_height: u64,
    pub terminated_finalizers: Vec<Hash32>,
}

#[derive(Clone, Debug)]
pub struct BftPowHeaderInspection {
    pub height: u32,
    pub hash: Hash32,
}

#[derive(Clone, Debug)]
pub struct TxInspection {
    pub hash: String,
    pub is_coinbase: bool,
    pub staking_action: Option<String>,
}

impl BlockInspection {
    pub fn to_display_string(&self) -> String {
        match self {
            BlockInspection::None => String::new(),
            BlockInspection::Pow(pow) => {
                let mut s = String::from("PoW Block\n");
                s.push_str(&format!("  Hash: {}\n", pow.hash));
                if let Some(h) = pow.height {
                    s.push_str(&format!("  Height: {h}\n"));
                }
                s.push_str(&format!("  Parent: {}\n", pow.parent_hash));
                s.push_str(&format!("  Time: {}\n", DateTime::<Utc>::from_timestamp_secs(pow.time).unwrap_or(DateTime::<Utc>::MAX_UTC).to_string()));
                s.push_str(&format!("  BFT pointer: {}\n", pow.fat_pointer));
                s.push_str(&format!("  Transactions: {}\n", pow.transactions.len()));
                for (i, tx) in pow.transactions.iter().enumerate() {
                    s.push_str(&format!("  [{i}] {}", tx.hash));
                    if tx.is_coinbase {
                        s.push_str(" (coinbase)");
                    }
                    if let Some(sa) = &tx.staking_action {
                        s.push_str(&format!(" {sa}"));
                    }
                    s.push('\n');
                }
                s
            }
            BlockInspection::Bft(bft) => {
                let mut s = String::from("BFT Block\n");
                s.push_str(&format!("  Hash: {}\n", bft.hash));
                s.push_str(&format!("  Version: {}\n", bft.version));
                s.push_str(&format!("  Height: {}\n", bft.height));
                s.push_str(&format!("  Previous: {}\n", bft.previous_hash));
                if bft.do_not_include_until_bc_height > 0 {
                    s.push_str(&format!("  Do not include until BC height: {}\n", bft.do_not_include_until_bc_height));
                }
                for hf in &bft.hardforks {
                    s.push_str("  Hardfork:\n");
                    s.push_str(&format!("    PoW activation height: {}\n", hf.pow_activation_height));
                    s.push_str(&format!("    BFT certificate height: {}\n", hf.bft_certificate_height));
                    s.push_str(&format!("    Terminated finalizers ({}):\n", hf.terminated_finalizers.len()));
                    for id in &hf.terminated_finalizers {
                        s.push_str(&format!("      {id}\n"));
                    }
                }
                s.push_str("  PoW headers:\n");
                for hdr in &bft.pow_headers {
                    s.push_str(&format!("    [{}] - {}\n", hdr.height, hdr.hash));
                }
                s
            }
        }
    }
}

pub struct ResponseFromZebra {
    pub bc_tip_height: u64,
    pub bc_finalized_tip_height: u64,
    pub bft_tip_height: u64,
    pub bc_blocks: Vec<BcBlock>,
    pub bft_blocks: Vec<BftBlock>,
    pub what_block_it_is: Hash32,
    pub block_inspection: BlockInspection,
    pub start_bc_height: u64,

    pub orchard_pool_balance: i64,
    pub staking_bonded_pool_balance: i64,
    pub staking_unbonded_pool_balance: i64,

    pub peer_strings: Vec<String>,

    /// One display line per mempool transaction: truncated txid plus any staking action.
    pub mempool_tx_strings: Vec<String>,
    /// Public keys of the finalizers whose signatures are on the fat pointer to the BFT tip.
    pub pos_tip_signers: Vec<Hash32>,

    pub bft_recency: wallet::TFLRecencyStatus,

    /// 32-byte ids of finalizers terminated by a user-led hardfork (the rolling
    /// blacklist). The right-hand pane excludes these from the normal list and
    /// shows them at the bottom with a cancel (X) icon.
    pub blacklisted_finalizers: Vec<Hash32>,
}
impl ResponseFromZebra {
    pub fn _0() -> Self {
        ResponseFromZebra {
            bc_tip_height: 0,
            bc_finalized_tip_height: 0,
            bft_tip_height: 0,
            bc_blocks: Vec::new(),
            bft_blocks: Vec::new(),
            what_block_it_is: Hash32::from_u64(0),
            block_inspection: BlockInspection::None,
            start_bc_height: 0,
            orchard_pool_balance: 0,
            staking_bonded_pool_balance: 0,
            staking_unbonded_pool_balance: 0,
            peer_strings: Vec::new(),
            mempool_tx_strings: Vec::new(),
            pos_tip_signers: Vec::new(),
            bft_recency: wallet::TFLRecencyStatus::default(),
            blacklisted_finalizers: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)] // , Serialize, Deserialize)]
pub struct BcBlock {
    pub this_hash: Hash32,
    pub parent_hash: Hash32,
    pub this_height: u64,
    pub txs_n: usize,
    pub is_best_chain: bool,
    pub is_finalized: bool,
    pub is_implicated_by_bft: bool,
    pub points_at_bft_block: Hash32,
    // #[cfg(debug_assertions)]
    pub work: u64,
    pub utc: i64,
    pub serialized_size: usize,
    /// This block is at a user-led hardfork's PoW activation height. Many forked
    /// blocks may share that height, so each one carries the flag and gets the
    /// hardfork-activation marker drawn below it.
    pub is_hardfork_activation: bool,
}
impl Default for BcBlock {
    fn default() -> Self {
        BcBlock {
            this_hash: Hash32::from_u64(0),
            parent_hash: Hash32::from_u64(0),
            this_height: 0,
            txs_n: 0,
            is_best_chain: false,
            is_finalized: false,
            is_implicated_by_bft: false,
            points_at_bft_block: Hash32::from_u64(0),
            // #[cfg(debug_assertions)]
            work: 0,
            utc: 0,
            serialized_size: 0,
            is_hardfork_activation: false,
        }
    }
}
struct OnScreenBc {
    x: f32,
    y: f32,
    roundness: f32,
    darkness: f32,
    alpha: f32,
    bft_arrow_alpha: f32,
    finalized_alpha: f32,
    implicated_by_bft_alpha: f32,

    t_x: f32,
    t_y: f32,
    t_roundness: f32,
    t_darkness: f32,
    t_alpha: f32,
    t_bft_arrow_alpha: f32,
    t_finalized_alpha: f32,
    t_implicated_by_bft_alpha: f32,
    block: BcBlock,
}
impl Default for OnScreenBc {
    fn default() -> Self {
        OnScreenBc {
            x: 0.0,
            y: 0.0,
            roundness: 1.0,
            darkness: 0.0,
            alpha: 1.0,
            bft_arrow_alpha: 1.0,
            finalized_alpha: 0.0,
            implicated_by_bft_alpha: 0.0,
            t_x: 0.0,
            t_y: 0.0,
            t_roundness: 1.0,
            t_darkness: 0.0,
            t_alpha: 1.0,
            t_bft_arrow_alpha: 1.0,
            t_finalized_alpha: 0.0,
            t_implicated_by_bft_alpha: 0.0,
            block: BcBlock::default(),
        }
    }
}

// use serde::{Serialize, Deserialize};
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)] // , Serialize, Deserialize)]
pub struct BftBlock {
    pub this_hash: Hash32,
    pub parent_hash: Hash32,
    pub this_height: u64,
    pub points_at_bc_block: Hash32,
    pub proving_blocks: Vec<Hash32>,
    /// The *next* BFT block (this block's successor) activates a user-led
    /// hardfork. We mark this block so the GUI can signal that a hardfork is
    /// imminent — i.e. that the block about to be built is special.
    pub next_block_is_hardfork: bool,
}
impl Default for BftBlock {
    fn default() -> Self {
        BftBlock {
            this_hash: Hash32::from_u64(0),
            parent_hash: Hash32::from_u64(0),
            this_height: 0,
            points_at_bc_block: Hash32::from_u64(0),
            proving_blocks: Vec::with_capacity(0),
            next_block_is_hardfork: false,
        }
    }
}
struct OnScreenBft {
    x: f32,
    y: f32,
    roundness: f32,
    darkness: f32,
    alpha: f32,

    t_x: f32,
    t_y: f32,
    t_roundness: f32,
    t_darkness: f32,
    t_alpha: f32,
    block: BftBlock,
}
impl Default for OnScreenBft {
    fn default() -> Self {
        OnScreenBft {
            x: 0.0,
            y: 0.0,
            roundness: 1.0,
            darkness: 0.0,
            alpha: 1.0,
            t_x: 0.0,
            t_y: 0.0,
            t_roundness: 1.0,
            t_darkness: 0.0,
            t_alpha: 1.0,
            block: BftBlock::default(),
        }
    }
}

const COLOR_BC:     u32 = 0x82ccc0;
const COLOR_NBC:    u32 = 0x808080;
const COLOR_BFT:    u32 = 0xdc4c4f;
const COLOR_ACCENT: u32 = 0x121212;
const COLOR_BRIGHT: u32 = 0xffffff;

const COLOR_BC_LINK:    u32 = 0x4e7b73;
const COLOR_BFT_LINK:   u32 = 0x9a2d37;
const COLOR_CROSS_LINK: u32 = 0x4e7b73;
const COLOR_NBC_LINK:   u32 = 0x4f4f4f;

pub struct VizState {
    pub camera_x: f32,
    pub camera_y: f32,
    pub zoom: f32,
    /// Camera eases toward the PoW tip every frame. Any manual camera movement
    /// (pan, world scroll, minimap scrub, jump, click-to-recenter) switches it off.
    pub follow_tip: bool,
    /// Freeze ingestion from zebra so a moving chain can be inspected. The sync
    /// channel back-pressures harmlessly while set.
    pub pause_incoming: bool,
    pub on_screen_bcs: HashMap<Hash32, OnScreenBc>,
    pub on_screen_bfts: HashMap<Hash32, OnScreenBft>,
    pub send_to_zebra: std::sync::mpsc::SyncSender<RequestToZebra>,
    pub receive_from_zebra: std::sync::mpsc::Receiver<ResponseFromZebra>,

    // TODO: move to zaino
    pub bft_recency_status: wallet::TFLRecencyStatus,
    /// 32-byte ids of terminated finalizers (the rolling blacklist), used by the
    /// right-hand pane to move them to the bottom with a cancel (X) icon.
    pub terminated_finalizer_ids: std::collections::HashSet<[u8; 32]>,

    pub time_since_last_animation: std::time::Instant,

    pub bc_tip_y: f32,

    pub bc_tip_height: u64,
    pub bc_finalized_tip_height: u64,
    pub bft_tip_height: u64,
    pub ui_hovered_height: Option<BlockHeight>,

    pub last_frame_hovered_hash: Hash32,

    pub inspecting_block_hash: Hash32,
    pub block_inspection: Option<BlockInspection>,

    pub should_reset_clay_text_cache: bool,

    pub inspecting_block_screen_x: f32,
    pub inspecting_block_screen_y: f32,

    pub bft_ack_height: u64,
    pub bc_ack_height: u64,

    pub orchard_pool_balance: i64,
    pub staking_bonded_pool_balance: i64,
    pub staking_unbonded_pool_balance: i64,

    pub peer_strings: Vec<String>,
    pub mempool_tx_strings: Vec<String>,
    pub pos_tip_signers: Vec<Hash32>,
}

impl VizState {
    pub fn pos_at_height(&self, height: BlockHeight) -> (f32, f32, bool) {
        if height.is_in_block() {
            (0.0, -10.0 * height.0 as f32, true) // @todo: should handle sidechain x-axis, maybe this should take a hash instead
        } else {
            (0.0, 0.0, false)
        }
    }

    fn camera_y_for_bft_hash(&self, mut bft_hash: Hash32) -> Option<f32> {
        for _ in 0usize..65536 {
            let bft = self.on_screen_bfts.get(&bft_hash)?;
            if let Some(bc) = self.on_screen_bcs.get(&bft.block.points_at_bc_block) {
                return Some(bc.t_y - 10.0 / 2.0);
            }
            if bft.block.parent_hash == Hash32::from_u64(0) {
                return None;
            }
            bft_hash = bft.block.parent_hash;
        }
        None
    }

    pub fn goto_pow_height(&mut self, h: u64) {
        self.follow_tip = false;
        self.camera_y = -10.0 * h as f32;
        self.zoom = 2.0;
    }

    pub fn goto_pos_height(&mut self, h: u64) -> bool {
        let Some(bft) = self
            .on_screen_bfts
            .values()
            .find(|b| b.block.this_height == h)
        else {
            return false;
        };
        let Some(cy) = self.camera_y_for_bft_hash(bft.block.this_hash) else {
            return false;
        };
        // self.camera_x = 5.0;
        self.follow_tip = false;
        self.camera_y = cy;
        true
    }

    pub fn inspecting_block_is_bft(&self) -> bool {
        self.on_screen_bfts.contains_key(&self.inspecting_block_hash)
    }

    fn nearest_bft_height_to_camera_y(&self, camera_y: f32) -> u64 {
        let (fallback, _) = bft_minimap_height_span(self);
        let mut best: Option<(f32, u64)> = None;
        for bft in self.on_screen_bfts.values() {
            let y_ref = self
                .camera_y_for_bft_hash(bft.block.this_hash)
                .unwrap_or(bft.t_y);
            let d = (y_ref - camera_y).abs();
            best = Some(match best {
                None => (d, bft.block.this_height),
                Some((bd, bh)) if d < bd => (d, bft.block.this_height),
                Some(prev) => prev,
            });
        }
        best.map(|(_, h)| h).unwrap_or(fallback)
    }
}

pub fn bc_at_h(state: &VizState, height: u64) -> Hash32 {
    for pow in &state.on_screen_bcs {
        if pow.1.block.is_best_chain && pow.1.block.this_height == height {
            return pow.1.block.this_hash;
        }
    }
    Hash32::from_u64(0)
}

// TODO(perf): can have a few ancestor backlinks that double in distance for O(log(n))
pub fn viz_block_lca(state: &VizState, bc_hash_0: Hash32, bc_hash_1: Hash32) -> Vec<Hash32> {
    let Some(mut bc0) = state.on_screen_bcs.get(&bc_hash_0) else {
        return Vec::new()
    };
    let Some(mut bc1) = state.on_screen_bcs.get(&bc_hash_1) else {
        return Vec::new()
    };

    let mut res = Vec::new();
    loop {
        match bc0.block.this_height.cmp(&bc1.block.this_height) {
            std::cmp::Ordering::Less => {
                bc1 = match state.on_screen_bcs.get(&bc1.block.parent_hash) {
                    Some(v) => v,
                    None => return Vec::new(),
                };
                if res.len() == 0 || res.last().unwrap() != &bc1.block.this_hash {
                    res.push(bc1.block.this_hash);
                }
            },
            std::cmp::Ordering::Greater => {
                bc0 = match state.on_screen_bcs.get(&bc0.block.parent_hash) {
                    Some(v) => v,
                    None => return Vec::new(),
                };
                if res.len() == 0 || res.last().unwrap() != &bc0.block.this_hash {
                    res.push(bc0.block.this_hash);
                }
            },
            std::cmp::Ordering::Equal => {
                if bc0.block.this_hash == bc1.block.this_hash {
                    return res;
                }

                bc0 = match state.on_screen_bcs.get(&bc0.block.parent_hash) {
                    Some(v) => v,
                    None => return Vec::new(),
                };
                bc1 = match state.on_screen_bcs.get(&bc1.block.parent_hash) {
                    Some(v) => v,
                    None => return Vec::new(),
                };
                if res.len() == 0 || res.last().unwrap() != &bc0.block.this_hash {
                    res.push(bc0.block.this_hash);
                }
                if res.len() == 0 || res.last().unwrap() != &bc1.block.this_hash {
                    res.push(bc1.block.this_hash);
                }
            },
        }
    }
}

pub fn apply_viz_op(state: &VizState, block: Hash32, op: InteractiveVizOp) -> Vec<Hash32> {
    let bc = state.on_screen_bcs.get(&block);
    let bft = state.on_screen_bfts.get(&block);

    let mut res = Vec::new();

    if op.valid_for_bc() && bc.is_none() {
        // println!("no on-screen PoW block found for hash {block}");
        return res;
    }

    if op.valid_for_bft() && bft.is_none() {
        // println!("no on-screen BFT block found for hash {block}");
        return Vec::new();
    }

    const TMP_SIGMA: u64 = 3;
    match op {
        InteractiveVizOp::None => {},

        InteractiveVizOp::LF => {
            res.push(bc.unwrap().block.points_at_bft_block);
            res.extend(apply_viz_op(state, res[0], InteractiveVizOp::bft_last_final));
        }
        InteractiveVizOp::candidate => {
            let lf = apply_viz_op(state, block, InteractiveVizOp::LF);
            if lf.len() == 0 {
                return Vec::new();
            }

            let snap = apply_viz_op(state, *lf.last().unwrap(), InteractiveVizOp::snapshot);
            if snap.len() == 0 {
                return Vec::new();
            }

            let sigma_block = bc_at_h(state, state.bc_tip_height.saturating_sub(TMP_SIGMA));

            let lca = viz_block_lca(state, snap[0], sigma_block);
            if lca.len() == 0 {
                return Vec::new();
            }

            res.extend(lf);
            res.extend(snap);
            res.extend(lca);
        },

        InteractiveVizOp::tip => {
            res.push(*bft.unwrap().block.proving_blocks.last().unwrap_or(&Hash32::from_u64(0)));
        }

        InteractiveVizOp::bft_last_final => res.push(bft.unwrap().block.parent_hash),
        InteractiveVizOp::origbft_last_final => todo!(),
        InteractiveVizOp::snapshot => {
            // TODO: what is the `ceil(1, bc')` suffix?
            res.push(*bft.unwrap().block.proving_blocks.first().unwrap_or(&Hash32::from_u64(0))); // TODO: fall back to genesis hash instead of 0
        },
    }

    res
}


pub fn viz_gui_init(fake_data: bool) -> VizState {
    let (me_send, zebra_receive) = std::sync::mpsc::sync_channel(128);
    let (zebra_send, me_receive) = std::sync::mpsc::sync_channel(128);

    *REQUESTS_TO_ZEBRA.lock().unwrap() = Some(zebra_receive);
    *RESPONSES_FROM_ZEBRA.lock().unwrap() = Some(zebra_send);

    let mut viz_state = VizState {
        camera_x: 0.0,
        camera_y: 0.0,
        zoom: 0.0,
        follow_tip: false,
        pause_incoming: false,
        on_screen_bcs: HashMap::new(),
        on_screen_bfts: HashMap::new(),
        send_to_zebra: me_send,
        receive_from_zebra: me_receive,
        bc_tip_height: 0,
        bc_finalized_tip_height: 0,
        bft_tip_height: 0,
        ui_hovered_height: None,

        bc_tip_y: 0.0,

        last_frame_hovered_hash: Hash32::from_u64(0),

        inspecting_block_hash: Hash32::from_u64(0),
        block_inspection: None,
        should_reset_clay_text_cache: false,

        inspecting_block_screen_x: 0.0,
        inspecting_block_screen_y: 0.0,

        time_since_last_animation: Instant::now(),

        bft_recency_status: wallet::TFLRecencyStatus::default(),
        terminated_finalizer_ids: std::collections::HashSet::new(),

        bft_ack_height: 0,
        bc_ack_height: 0,

        orchard_pool_balance: 0,
        staking_bonded_pool_balance: 0,
        staking_unbonded_pool_balance: 0,
        peer_strings: Vec::new(),
        mempool_tx_strings: Vec::new(),
        pos_tip_signers: Vec::new(),
    };

    if fake_data {
        // TODO: pull from binary test data
        let mut make_bc = |seq: &mut u64, parent_hash: Hash32, points_at_bft_block: Hash32, txs_n: usize, is_best_chain: bool, is_finalized: bool, is_implicated_by_bft: bool| -> Hash32 {
            let this_height = if let Some(parent) = viz_state.on_screen_bcs.get(&parent_hash) {
                parent.block.this_height+1
            } else {
                 0
            };

            if is_best_chain {
                viz_state.bc_tip_height = viz_state.bc_tip_height.max(this_height);
                if is_finalized {
                    viz_state.bc_finalized_tip_height = viz_state.bc_finalized_tip_height.max(this_height);
                }
            }

            *seq += 1;
            let this_hash = Hash32::from_u64(*seq);
            let block = OnScreenBc { block: BcBlock { this_hash, parent_hash, this_height, txs_n, is_best_chain, is_finalized, is_implicated_by_bft, points_at_bft_block, work:1234, utc:0, serialized_size: 0, is_hardfork_activation: false }, ..Default::default() };
            viz_state.on_screen_bcs.insert(block.block.this_hash, block);
            this_hash
        };

        let mut bft_parent_hash = Hash32::from_u64(0);
        let mut make_bft = |seq: &mut u64, points_at_bc_block: Hash32| -> Hash32 {
            *seq += 1;
            let this_height = viz_state.on_screen_bfts.len() as u64;
            viz_state.bft_tip_height = this_height;

            let this_hash = Hash32::from_u64(*seq);
            let block = OnScreenBft { block: BftBlock { this_hash, parent_hash: bft_parent_hash, this_height, points_at_bc_block, proving_blocks: vec![points_at_bc_block], next_block_is_hardfork: false }, ..Default::default() };
            viz_state.on_screen_bfts.insert(block.block.this_hash, block);

            bft_parent_hash = this_hash;
            this_hash
        };

        let seq = &mut 0u64;


        let bc_0  = make_bc(seq, Hash32::from_u64(0), Hash32::from_u64(0), 1, /*bc*/true, /*final*/true, /*bft-implicated*/false);
        let bft0  = make_bft(seq, bc_0);
        let bc_1  = make_bc(seq, bc_0,  bft0,    6, /*bc*/true,  /*final*/true,  /*bft-implicated*/true);
        let bc_2  = make_bc(seq, bc_1,  bft0,    2, /*bc*/true,  /*final*/true,  /*bft-implicated*/true);
        let bc_1a = make_bc(seq, bc_0,  bft0, 4256, /*bc*/false, /*final*/false, /*bft-implicated*/false);
        let bc_2a = make_bc(seq, bc_1,  bft0,   16, /*bc*/false, /*final*/false, /*bft-implicated*/false);
        let bc_2b = make_bc(seq, bc_1a, bft0,   15, /*bc*/false, /*final*/false, /*bft-implicated*/false);
        let bc_3b = make_bc(seq, bc_2b, bft0,    3, /*bc*/false, /*final*/false, /*bft-implicated*/false);

        let bft1  = make_bft(seq, bc_1);
        let bft2  = make_bft(seq, bc_2);

        let bc_3  = make_bc(seq, bc_2,  bft2,    3, /*bc*/true, /*final*/true,  /*bft-implicated*/false);
        let bc_4  = make_bc(seq, bc_3,  bft2,    3, /*bc*/true, /*final*/true,  /*bft-implicated*/false);
        let bc_5  = make_bc(seq, bc_4,  bft2,   18, /*bc*/true, /*final*/false, /*bft-implicated*/false);
        let bft3  = make_bft(seq, bc_4);

        let bc_6  = make_bc(seq, bc_5,  bft3,    5, /*bc*/true, /*final*/false, /*bft-implicated*/false);
        let bc_7  = make_bc(seq, bc_6,  bft3,    5, /*bc*/true, /*final*/false, /*bft-implicated*/false);
        let bc_8  = make_bc(seq, bc_7,  bft3,    5, /*bc*/true, /*final*/false, /*bft-implicated*/false);
        let bc_8  = make_bc(seq, bc_7,  bft3,    5, /*bc*/true, /*final*/false, /*bft-implicated*/false);
    }

    viz_state
}

pub fn viz_gui_anything_happened_at_all(viz_state: &mut VizState) -> bool {
    let mut anything_happened = false;

    if viz_state.time_since_last_animation.elapsed().as_secs_f32() > 0.333 {
        anything_happened = true;
        viz_state.time_since_last_animation = Instant::now();
    }

    let mut new_blocks_n = 0usize;
    while let Ok(message) = viz_state.receive_from_zebra.try_recv() {
        if viz_state.pause_incoming {
            // Block ingestion frozen; only serve inspection so clicked blocks still resolve.
            if message.what_block_it_is == viz_state.inspecting_block_hash
                && message.what_block_it_is != Hash32::from_u64(0)
            {
                anything_happened = true;
                viz_state.should_reset_clay_text_cache = true;
                viz_state.block_inspection = Some(message.block_inspection);
            }
            continue;
        }

        anything_happened |= viz_state.bc_tip_height != message.bc_tip_height;
        viz_state.bc_tip_height = message.bc_tip_height;

        anything_happened |= viz_state.bft_recency_status != message.bft_recency;
        viz_state.bft_recency_status = message.bft_recency;

        let new_terminated: std::collections::HashSet<[u8; 32]> =
            message.blacklisted_finalizers.iter().map(|h| h.as_bytes()).collect();
        anything_happened |= viz_state.terminated_finalizer_ids != new_terminated;
        viz_state.terminated_finalizer_ids = new_terminated;

        anything_happened |= viz_state.bft_tip_height != message.bft_tip_height;
        viz_state.bft_tip_height = message.bft_tip_height;

        anything_happened |= viz_state.bc_finalized_tip_height != message.bc_finalized_tip_height;
        viz_state.bc_finalized_tip_height = message.bc_finalized_tip_height;

        viz_state.peer_strings = message.peer_strings;

        anything_happened |= viz_state.mempool_tx_strings != message.mempool_tx_strings;
        viz_state.mempool_tx_strings = message.mempool_tx_strings;

        anything_happened |= viz_state.pos_tip_signers != message.pos_tip_signers;
        viz_state.pos_tip_signers = message.pos_tip_signers;

        viz_state.orchard_pool_balance = message.orchard_pool_balance;
        viz_state.staking_bonded_pool_balance = message.staking_bonded_pool_balance;
        viz_state.staking_unbonded_pool_balance = message.staking_unbonded_pool_balance;

        // @hack
        for bc in viz_state.on_screen_bcs.values_mut() {
            if bc.block.this_height >= message.start_bc_height {
                bc.block.is_best_chain = false;
            }
        }

        if message.what_block_it_is == viz_state.inspecting_block_hash {
            viz_state.should_reset_clay_text_cache = true;
            viz_state.block_inspection = Some(message.block_inspection);
        }

        let zoom = ZOOM_FACTOR.powf(viz_state.zoom);
        // origin
        let screen_unit = SCREEN_UNIT_CONST * zoom;
        let spawn_y = viz_state.camera_y - 7000.0 / screen_unit;

        for bc in &message.bc_blocks {
            if let Some(r) = viz_state.on_screen_bcs.get_mut(&bc.this_hash) {
                anything_happened |= r.block != *bc;
                let was_implicated = r.block.is_implicated_by_bft;
                r.block = *bc;
                r.block.is_implicated_by_bft = was_implicated;
            } else {
                anything_happened |= true;
                new_blocks_n += 1;
                viz_state.on_screen_bcs.insert(bc.this_hash, OnScreenBc { y: spawn_y, block: *bc, alpha: 0.0, ..Default::default() });
            }
        }
        for bft in message.bft_blocks {
            let mut missing = true;
            if let Some(bc) = viz_state.on_screen_bcs.get_mut(&bft.points_at_bc_block) {
                missing = false;
                viz_state.bc_ack_height = viz_state.bc_ack_height.max(bc.block.this_height);
                bc.block.is_implicated_by_bft = true;

                let mut prev = bft.points_at_bc_block;
                for hash in &bft.proving_blocks {
                    if let Some(bc) = viz_state.on_screen_bcs.get_mut(hash) { bc.block.is_implicated_by_bft = true; }
                    else {
                        let parent = viz_state.on_screen_bcs.get(&prev).unwrap();
                        let block = OnScreenBc { y: spawn_y, block: BcBlock {
                            this_hash: *hash,
                            parent_hash: parent.block.this_hash,
                            txs_n: 1,
                            this_height: parent.block.this_height + 1,
                            is_best_chain: false,
                            is_finalized: false,
                            is_implicated_by_bft: true,
                            points_at_bft_block: Hash32::from_u64(0),
                            // #[cfg(debug_assertions)]
                            work: 0,
                            utc: 0,
                            serialized_size: 0,
                            is_hardfork_activation: false,
                        }, ..Default::default() };
                        viz_state.on_screen_bcs.insert(block.block.this_hash, block);
                    }
                    prev = *hash;
                }
            }
            if let Some(r) = viz_state.on_screen_bfts.get_mut(&bft.this_hash) {
                anything_happened |= r.block != bft;
                r.block = bft;
            } else {
                if missing == false { viz_state.bft_ack_height = viz_state.bft_ack_height.max(bft.this_height); }
                anything_happened |= true;
                new_blocks_n += 1;
                viz_state.on_screen_bfts.insert(bft.this_hash, OnScreenBft { block: bft, alpha: 0.0, y: spawn_y, ..Default::default() });
            }
        }
    }

    // Once per drain, pitched-up hover blip. Large batches are sync catch-up, not news.
    if new_blocks_n >= 1 && new_blocks_n < 8 {
        play_sound(SOUND_UI_HOVER, 0.4, 1.6);
    }

    if anything_happened == false {
        let _ = viz_state.send_to_zebra.try_send(RequestToZebra {
            want_to_inspect_block: viz_state.inspecting_block_hash,
            bft_ack_height: viz_state.bft_ack_height,
            bc_ack_height: viz_state.bc_ack_height,
        });
    }

    // animations
    const MARGIN : f32 = 0.001;
    anything_happened |= viz_state.follow_tip && (viz_state.camera_y - viz_state.bc_tip_y).abs() > MARGIN;
    for on_screen_bc in viz_state.on_screen_bcs.values() {
        anything_happened |= (on_screen_bc.t_x - on_screen_bc.x).abs() > MARGIN;
        anything_happened |= (on_screen_bc.t_y - on_screen_bc.y).abs() > MARGIN;
        anything_happened |= (on_screen_bc.t_roundness - on_screen_bc.roundness).abs() > MARGIN;
        anything_happened |= (on_screen_bc.t_darkness - on_screen_bc.darkness).abs() > MARGIN;
        anything_happened |= (on_screen_bc.t_alpha - on_screen_bc.alpha).abs() > MARGIN;
    }
    for on_screen_bft in viz_state.on_screen_bfts.values() {
        anything_happened |= (on_screen_bft.t_x - on_screen_bft.x).abs() > MARGIN;
        anything_happened |= (on_screen_bft.t_y - on_screen_bft.y).abs() > MARGIN;
        anything_happened |= (on_screen_bft.t_roundness - on_screen_bft.roundness).abs() > MARGIN;
        anything_happened |= (on_screen_bft.t_darkness - on_screen_bft.darkness).abs() > MARGIN;
        anything_happened |= (on_screen_bft.t_alpha - on_screen_bft.alpha).abs() > MARGIN;
    }

    anything_happened
}

fn e_lerp(from: f32, to: f32, dt: f32) -> f32 {
    let k = 10.0; // damping/smoothing rate; higher = faster response
    let delta = (to - from) * (1.0 - (-k * dt).exp());
    let next = from + delta;
    // Snap if we’re about to overshoot
    if (to - from).signum() != (to - next).signum() {
        to
    } else {
        next
    }
}

fn hovered_hash_in_world<T>(
    items: impl Iterator<Item = T>,
    world_mouse_x: f32,
    world_mouse_y: f32,
    sample: impl Fn(T) -> (f32, f32, Hash32),
) -> Option<Hash32> {
    let mut hovered = None;
    for item in items {
        let (x, y, hash) = sample(item);
        let dx = x - world_mouse_x;
        let dy = y - world_mouse_y;
        if (dx * dx + dy * dy).sqrt() < 1.0 {
            hovered = Some(hash);
        }
    }
    hovered
}

fn animate_bc_towards_target(on_screen_bc: &mut OnScreenBc, dt: f32) {
    on_screen_bc.x = e_lerp(on_screen_bc.x, on_screen_bc.t_x, dt);
    on_screen_bc.y = e_lerp(on_screen_bc.y, on_screen_bc.t_y, dt);
    on_screen_bc.roundness = e_lerp(on_screen_bc.roundness, on_screen_bc.t_roundness, dt);
    on_screen_bc.darkness = e_lerp(on_screen_bc.darkness, on_screen_bc.t_darkness, dt);
    on_screen_bc.alpha = e_lerp(on_screen_bc.alpha, on_screen_bc.t_alpha, dt);
    on_screen_bc.bft_arrow_alpha = e_lerp(on_screen_bc.bft_arrow_alpha, on_screen_bc.t_bft_arrow_alpha, dt);
    on_screen_bc.finalized_alpha = e_lerp(on_screen_bc.finalized_alpha, on_screen_bc.t_finalized_alpha, dt);
    on_screen_bc.implicated_by_bft_alpha = e_lerp(on_screen_bc.implicated_by_bft_alpha, on_screen_bc.t_implicated_by_bft_alpha, dt);
}

fn animate_bft_towards_target(on_screen_bft: &mut OnScreenBft, dt: f32) {
    on_screen_bft.x = e_lerp(on_screen_bft.x, on_screen_bft.t_x, dt);
    on_screen_bft.y = e_lerp(on_screen_bft.y, on_screen_bft.t_y, dt);
    on_screen_bft.roundness = e_lerp(on_screen_bft.roundness, on_screen_bft.t_roundness, dt);
    on_screen_bft.darkness = e_lerp(on_screen_bft.darkness, on_screen_bft.t_darkness, dt);
    on_screen_bft.alpha = e_lerp(on_screen_bft.alpha, on_screen_bft.t_alpha, dt);
}

const ZOOM_FACTOR : f32 = 1.2;
const SCREEN_UNIT_CONST : f32 = 10.0;

fn effective_vertical_wheel_for_scrub(input_ctx: &InputCtx) -> f32 {
    if input_ctx.scroll_delta.1.abs() > 1e-6 || input_ctx.scroll_delta.0.abs() > 1e-6 {
        return input_ctx.scroll_delta.1 as f32;
    }
    -(input_ctx.zoom_delta as f32) * 28.0
}

#[derive(Clone, Copy)]
enum MinimapLane {
    Pow,
    Pos,
}

fn minimap_screen_rect(draw_ctx: &DrawCtx, ui: &ui::Context, lane: MinimapLane) -> (f32, f32, f32, f32) {
    let w = draw_ctx.window_width as f32;
    let h = draw_ctx.window_height as f32;
    let ds = ui.dpi_scale;
    let gx = 10.0 * ds;
    let strip = 46.0 * ds;
    let (x0, x1) = match lane {
        MinimapLane::Pow => {
            let x0 = w * ui.left_pane_width + gx;
            (x0, x0 + strip)
        }
        MinimapLane::Pos => {
            let x1 = w * (1.0 - ui.right_pane_width) - gx;
            (x1 - strip, x1)
        }
    };
    let y0 = 50.0 * ds;
    let y1 = h - 86.0 * ds;
    (x0, y0, x1, y1)
}

fn minimap_interaction_rect(mm_rect: (f32, f32, f32, f32), ui: &ui::Context) -> (f32, f32, f32, f32) {
    let (x0, y0, x1, y1) = mm_rect;
    let ds = ui.dpi_scale;
    let px = 8.0 * ds;
    let py_top = 46.0 * ds;
    let py_bot = 26.0 * ds;
    (x0 - px, y0 - py_top, x1 + px, y1 + py_bot)
}

fn chain_minimap_height_span(viz_state: &VizState) -> (u64, u64) {
    let mut h_min = u64::MAX;
    let mut h_max = 0u64;
    for bc in viz_state.on_screen_bcs.values() {
        if bc.block.is_best_chain {
            h_min = h_min.min(bc.block.this_height);
            h_max = h_max.max(bc.block.this_height);
        }
    }
    if h_min == u64::MAX {
        h_min = 0;
    }
    h_max = h_max.max(viz_state.bc_tip_height);
    if h_max < h_min {
        h_max = h_min;
    }
    (h_min, h_max)
}

fn bft_minimap_height_span(viz_state: &VizState) -> (u64, u64) {
    let mut h_min = u64::MAX;
    let mut h_max = 0u64;
    for bft in viz_state.on_screen_bfts.values() {
        h_min = h_min.min(bft.block.this_height);
        h_max = h_max.max(bft.block.this_height);
    }
    if h_min == u64::MAX {
        h_min = 0;
    }
    h_max = h_max.max(viz_state.bft_tip_height);
    if h_max < h_min {
        h_max = h_min;
    }
    (h_min, h_max)
}

fn apply_scrub_bft_height(viz: &mut VizState, mut h: u64) {
    let (lo, hi) = bft_minimap_height_span(viz);
    h = h.clamp(lo, hi);
    if viz.goto_pos_height(h) {
        return;
    }
    if let Some((_, bh)) = viz
        .on_screen_bfts
        .values()
        .map(|b| {
            let d = b.block.this_height.abs_diff(h);
            (d, b.block.this_height)
        })
        .min_by_key(|x| x.0)
    {
        let _ = viz.goto_pos_height(bh);
    }
}

fn apply_scrub_pow_height(viz: &mut VizState, h: f32, h_min: u64, h_max: u64) {
    let h_next = h.clamp(h_min as f32, h_max as f32);
    viz.follow_tip = false;
    viz.camera_y = -10.0 * h_next;
    // viz.camera_x = 0.0;
}

fn minimap_wheel_delta_height(dy: f32, strip_h: f32, span_h: f32) -> f32 {
    -(dy / strip_h.max(1.0)) * span_h * 0.45
}

fn chain_minimap_h_to_y(h: u64, h_min: u64, h_max: u64, y_top: f32, y_bot: f32) -> f32 {
    let span = h_max.saturating_sub(h_min).max(1) as f32;
    let t = (h.saturating_sub(h_min)) as f32 / span;
    y_top + (1.0 - t) * (y_bot - y_top)
}

fn chain_minimap_y_to_h(my: f32, h_min: u64, h_max: u64, y_top: f32, y_bot: f32) -> u64 {
    let span = h_max.saturating_sub(h_min).max(1) as f32;
    let t = 1.0 - ((my - y_top) / (y_bot - y_top).max(1.0)).clamp(0.0, 1.0);
    let hf = h_min as f32 + t * span;
    (hf.round() as u64).clamp(h_min, h_max)
}

fn chain_minimap_hf_to_y(hf: f32, h_min: u64, h_max: u64, y_top: f32, y_bot: f32) -> f32 {
    let span = h_max.saturating_sub(h_min).max(1) as f32;
    let t = ((hf - h_min as f32) / span).clamp(0.0, 1.0);
    y_top + (1.0 - t) * (y_bot - y_top)
}

fn draw_chain_minimap_overlay(
    viz_state: &VizState,
    ui: &ui::Context,
    draw_ctx: &mut DrawCtx,
    origin_y: f32,
    screen_unit: f32,
    mm_rect: (f32, f32, f32, f32),
) {
    let (mx0, my0, mx1, my1) = mm_rect;
    if my1 <= my0 + 8.0 || mx1 <= mx0 + 4.0 {
        return;
    }
    let (h_min, h_max) = chain_minimap_height_span(viz_state);
    if viz_state.on_screen_bcs.is_empty() && viz_state.bc_tip_height == 0 {
        return;
    }
    let ds = ui.dpi_scale;
    let track_l = mx0 + 3.5 * ds;
    let track_r = mx1 - 2.0 * ds;
    let xc = (track_l + track_r) * 0.5;
    let title_h = (11.0 * ds).max(9.0);
    let tick_h = (9.0 * ds).max(7.0);
    let outer_pad = 4.0 * ds;
    let title_band = title_h + 6.0 * ds;
    let legend_h = tick_h + 4.0 * ds;
    let ox0 = mx0 - outer_pad;
    let oy0 = my0 - outer_pad - title_band;
    let ox1 = mx1 + outer_pad;
    let oy1 = my1 + outer_pad + legend_h;

    let frame_outer = 0xe01f2528_u32;
    let frame_mid = blend_u32(0xff131618, 0xff4e7b73, 48);
    let track_deep = blend_u32(0xff0c0e10, 0xff82ccc0, 22);
    let grid_col = 0x2882ccc0_u32;
    let viewport_fill = 0x3282ccc0_u32;
    let tick_final = blend_u32(0xff0e1214, 0xff5fdc8a, 210);
    let tick_open = blend_u32(0xff0e1214, 0xffdcb85f, 195);
    let side_mute = blend_u32(0xff101214, 0xff808080, 130);
    let title_col = 0xffdceeea_u32;
    let subtitle_col = blend_u32(0xffc8ddd9, 0xff4e7b73, 100);
    let label_mute = blend_u32(0xffb8ccc8, 0xff4e7b73, 115);

    draw_ctx.rectangle_r(ox0, oy0, ox1, oy1, 5, frame_outer);
    draw_ctx.rectangle_r(ox0 + 1.5, oy0 + 1.5, ox1 - 1.5, oy1 - 1.5, 4, frame_mid);
    draw_ctx.rectangle(mx0 - 0.5, my0 - 0.5, mx1 + 0.5, my1 + 0.5, track_deep);
    draw_ctx.rectangle(mx0, my0, mx0 + 2.5 * ds, my1, 0xff4e7b73);

    for q in 1..4 {
        let t = q as f32 / 4.0;
        let yy = my0 + (my1 - my0) * t;
        draw_ctx.rectangle(track_l, yy, track_r, yy + 1.0, grid_col);
    }

    let wy_top = (0.0 - origin_y) / screen_unit;
    let wy_bot = ((draw_ctx.window_height as f32) - origin_y) / screen_unit;
    let h_top = -wy_top / 10.0;
    let h_bot = -wy_bot / 10.0;
    let (hv_lo, hv_hi) = if h_top <= h_bot { (h_top, h_bot) } else { (h_bot, h_top) };
    let hv_lo = hv_lo.max(h_min as f32);
    let hv_hi = hv_hi.min(h_max as f32).max(hv_lo);
    let vy_a = chain_minimap_hf_to_y(hv_lo, h_min, h_max, my0, my1);
    let vy_b = chain_minimap_hf_to_y(hv_hi, h_min, h_max, my0, my1);
    let (vy0, vy1) = if vy_a <= vy_b { (vy_a, vy_b) } else { (vy_b, vy_a) };
    draw_ctx.rectangle(track_l, vy0, track_r, vy1, viewport_fill);
    draw_ctx.rectangle_r(track_l, vy0, track_r, vy1, 1, 0x5582ccc0_u32);

    let mut best_by_h: HashMap<u64, bool> = HashMap::new();
    for bc in viz_state.on_screen_bcs.values() {
        if !bc.block.is_best_chain {
            continue;
        }
        let fin = bc.block.this_height <= viz_state.bc_finalized_tip_height;
        best_by_h
            .entry(bc.block.this_height)
            .and_modify(|f| *f |= fin)
            .or_insert(fin);
    }
    let mut heights: Vec<u64> = best_by_h.keys().copied().collect();
    heights.sort_unstable();
    let step = (heights.len() / 900 + 1).max(1);
    for (i, &ht) in heights.iter().enumerate() {
        if i % step != 0 {
            continue;
        }
        let fin = *best_by_h.get(&ht).unwrap_or(&false);
        let yy = chain_minimap_h_to_y(ht, h_min, h_max, my0, my1);
        let col = if fin { tick_final } else { tick_open };
        draw_ctx.rectangle(xc - 2.5 * ds, yy - 0.8, xc + 2.5 * ds, yy + 0.8, col);
    }

    let mut n_side = 0usize;
    for bc in viz_state.on_screen_bcs.values() {
        if bc.block.is_best_chain {
            continue;
        }
        n_side += 1;
        if n_side > 400 {
            break;
        }
        let ht = bc.block.this_height.clamp(h_min, h_max);
        let yy = chain_minimap_h_to_y(ht, h_min, h_max, my0, my1);
        draw_ctx.rectangle(mx0 + 0.5 * ds, yy, mx0 + 3.0 * ds, yy + 1.2 * ds, side_mute);
    }

    let h_mid = h_min.saturating_add(h_max.saturating_sub(h_min) / 2);
    let s_top = format!("{}", h_max);
    let s_mid = format!("{}", h_mid);
    let s_bot = format!("{}", h_min);
    let max_lab_w = draw_ctx
        .measure_text_line(FontKind::Mono, tick_h, &s_top)
        .max(draw_ctx.measure_text_line(FontKind::Mono, tick_h, &s_mid))
        .max(draw_ctx.measure_text_line(FontKind::Mono, tick_h, &s_bot));
    let label_x = (track_r - 3.0 * ds - max_lab_w).max(track_l + 6.0 * ds);
    let y_top_lab = chain_minimap_h_to_y(h_max, h_min, h_max, my0, my1) - tick_h * 0.35;
    let y_mid_lab = chain_minimap_h_to_y(h_mid, h_min, h_max, my0, my1) - tick_h * 0.35;
    let y_bot_lab = chain_minimap_h_to_y(h_min, h_min, h_max, my0, my1) - tick_h * 0.35;
    draw_ctx.text_line(FontKind::Mono, label_x, y_top_lab, tick_h, &s_top, label_mute);
    draw_ctx.text_line(FontKind::Mono, label_x, y_mid_lab, tick_h, &s_mid, label_mute);
    draw_ctx.text_line(FontKind::Mono, label_x, y_bot_lab, tick_h, &s_bot, label_mute);

    draw_ctx.text_line(FontKind::Mono, mx0, oy0 + 3.0 * ds, title_h, "PoW", title_col);

    let dot = 4.0 * ds;
    let leg_y = my1 + 5.0 * ds;
    draw_ctx.rectangle(mx0, leg_y, mx0 + dot, leg_y + dot, tick_final);
    draw_ctx.text_line(FontKind::Mono, mx0 + dot + 5.0 * ds, leg_y - 1.0 * ds, tick_h * 0.88, "finalized", label_mute);
    let lx2 = mx0 + 56.0 * ds;
    draw_ctx.rectangle(lx2, leg_y, lx2 + dot, leg_y + dot, tick_open);
    draw_ctx.text_line(FontKind::Mono, lx2 + dot + 5.0 * ds, leg_y - 1.0 * ds, tick_h * 0.88, "open", label_mute);
}

fn draw_pos_minimap_overlay(
    viz_state: &VizState,
    ui: &ui::Context,
    draw_ctx: &mut DrawCtx,
    origin_y: f32,
    screen_unit: f32,
    mm_rect: (f32, f32, f32, f32),
) {
    let (mx0, my0, mx1, my1) = mm_rect;
    if my1 <= my0 + 8.0 || mx1 <= mx0 + 4.0 {
        return;
    }
    let (h_min, h_max) = bft_minimap_height_span(viz_state);
    if viz_state.on_screen_bfts.is_empty() && viz_state.bft_tip_height == 0 {
        return;
    }
    let ds = ui.dpi_scale;
    let track_l = mx0 + 3.5 * ds;
    let track_r = mx1 - 2.0 * ds;
    let xc = (track_l + track_r) * 0.5;
    let title_h = (11.0 * ds).max(9.0);
    let tick_h = (9.0 * ds).max(7.0);
    let outer_pad = 4.0 * ds;
    let title_band = title_h + 6.0 * ds;
    let legend_h = tick_h + 4.0 * ds;
    let ox0 = mx0 - outer_pad;
    let oy0 = my0 - outer_pad - title_band;
    let ox1 = mx1 + outer_pad;
    let oy1 = my1 + outer_pad + legend_h;

    let frame_outer = 0xe01f2528_u32;
    let frame_mid = blend_u32(0xff131618, 0xffa24545, 56);
    let track_deep = blend_u32(0xff0c0e10, 0xffff8a8a, 24);
    let grid_col = 0x28ff8a8a_u32;
    let viewport_fill = 0x34ff8a8a_u32;
    let tick_col = blend_u32(0xff1a0f0f, 0xffff8a8a, 210);
    let title_col = 0xffffecec_u32;
    let subtitle_col = blend_u32(0xfff0d0d0, 0xffa24545, 96);
    let label_mute = blend_u32(0xffe8caca, 0xffa24545, 110);

    draw_ctx.rectangle_r(ox0, oy0, ox1, oy1, 5, frame_outer);
    draw_ctx.rectangle_r(ox0 + 1.5, oy0 + 1.5, ox1 - 1.5, oy1 - 1.5, 4, frame_mid);
    draw_ctx.rectangle(mx0 - 0.5, my0 - 0.5, mx1 + 0.5, my1 + 0.5, track_deep);
    draw_ctx.rectangle(mx1 - 2.5 * ds, my0, mx1, my1, 0xffa24545);

    for q in 1..4 {
        let t = q as f32 / 4.0;
        let yy = my0 + (my1 - my0) * t;
        draw_ctx.rectangle(track_l, yy, track_r, yy + 1.0, grid_col);
    }

    let wy_top = (0.0 - origin_y) / screen_unit;
    let wy_bot = ((draw_ctx.window_height as f32) - origin_y) / screen_unit;
    let y_lo = wy_top.min(wy_bot);
    let y_hi = wy_top.max(wy_bot);
    let mut in_view: Vec<u64> = viz_state
        .on_screen_bfts
        .values()
        .filter(|b| b.y >= y_lo && b.y <= y_hi)
        .map(|b| b.block.this_height)
        .collect();
    let (vy0, vy1) = if !in_view.is_empty() {
        let b_lo = *in_view.iter().min().unwrap();
        let b_hi = *in_view.iter().max().unwrap();
        let vy_a = chain_minimap_h_to_y(b_lo, h_min, h_max, my0, my1);
        let vy_b = chain_minimap_h_to_y(b_hi, h_min, h_max, my0, my1);
        if vy_a <= vy_b { (vy_a, vy_b) } else { (vy_b, vy_a) }
    } else {
        let hc = viz_state.nearest_bft_height_to_camera_y(viz_state.camera_y) as f32;
        let vy_a = chain_minimap_hf_to_y(hc - 0.5, h_min, h_max, my0, my1);
        let vy_b = chain_minimap_hf_to_y(hc + 0.5, h_min, h_max, my0, my1);
        if vy_a <= vy_b { (vy_a, vy_b) } else { (vy_b, vy_a) }
    };
    draw_ctx.rectangle(track_l, vy0, track_r, vy1, viewport_fill);
    draw_ctx.rectangle_r(track_l, vy0, track_r, vy1, 1, 0x55ff8a8a_u32);

    let mut heights: Vec<u64> = viz_state
        .on_screen_bfts
        .values()
        .map(|bft| bft.block.this_height)
        .collect();
    heights.sort_unstable();
    heights.dedup();
    let step = (heights.len() / 900 + 1).max(1);
    for (i, &ht) in heights.iter().enumerate() {
        if i % step != 0 {
            continue;
        }
        let yy = chain_minimap_h_to_y(ht, h_min, h_max, my0, my1);
        draw_ctx.rectangle(xc - 2.5 * ds, yy - 0.8, xc + 2.5 * ds, yy + 0.8, tick_col);
    }

    let h_mid = h_min.saturating_add(h_max.saturating_sub(h_min) / 2);
    let s_top = format!("{}", h_max);
    let s_mid = format!("{}", h_mid);
    let s_bot = format!("{}", h_min);
    let max_lab_w = draw_ctx
        .measure_text_line(FontKind::Mono, tick_h, &s_top)
        .max(draw_ctx.measure_text_line(FontKind::Mono, tick_h, &s_mid))
        .max(draw_ctx.measure_text_line(FontKind::Mono, tick_h, &s_bot));
    let label_x = (track_r - 3.0 * ds - max_lab_w).max(track_l + 6.0 * ds);
    let y_top_lab = chain_minimap_h_to_y(h_max, h_min, h_max, my0, my1) - tick_h * 0.35;
    let y_mid_lab = chain_minimap_h_to_y(h_mid, h_min, h_max, my0, my1) - tick_h * 0.35;
    let y_bot_lab = chain_minimap_h_to_y(h_min, h_min, h_max, my0, my1) - tick_h * 0.35;
    draw_ctx.text_line(FontKind::Mono, label_x, y_top_lab, tick_h, &s_top, label_mute);
    draw_ctx.text_line(FontKind::Mono, label_x, y_mid_lab, tick_h, &s_mid, label_mute);
    draw_ctx.text_line(FontKind::Mono, label_x, y_bot_lab, tick_h, &s_bot, label_mute);

    draw_ctx.text_line(FontKind::Mono, mx0, oy0 + 3.0 * ds, title_h, "PoS", title_col);

    let dot = 4.0 * ds;
    let leg_y = my1 + 5.0 * ds;
    draw_ctx.rectangle(mx0, leg_y, mx0 + dot, leg_y + dot, tick_col);
    draw_ctx.text_line(FontKind::Mono, mx0 + dot + 5.0 * ds, leg_y - 1.0 * ds, tick_h * 0.88, "bft blocks", label_mute);
}

pub(crate) fn viz_gui_draw_the_stuff_for_the_things(viz_state: &mut VizState, ui: &mut ui::Context, draw_ctx: &mut DrawCtx, dt: f32, input_ctx: &InputCtx) {
    ui.suppress_scroll_for_clay = false;
    let mm_rect = minimap_screen_rect(draw_ctx, ui, MinimapLane::Pow);
    let (_, mm_y0, _, mm_y1) = mm_rect;
    let hit_rect = minimap_interaction_rect(mm_rect, ui);
    let (hit_x0, hit_y0, hit_x1, hit_y1) = hit_rect;
    let pos_mm_rect = minimap_screen_rect(draw_ctx, ui, MinimapLane::Pos);
    let (_, pos_mm_y0, _, pos_mm_y1) = pos_mm_rect;
    let pos_hit_rect = minimap_interaction_rect(pos_mm_rect, ui);
    let (pos_hit_x0, pos_hit_y0, pos_hit_x1, pos_hit_y1) = pos_hit_rect;
    let mx_scr = input_ctx.mouse_pos().0.clamp(0, draw_ctx.window_width) as f32;
    let my_scr = input_ctx.mouse_pos().1.clamp(0, draw_ctx.window_height) as f32;
    let inside_pow_minimap =
        mx_scr >= hit_x0 && mx_scr <= hit_x1 && my_scr >= hit_y0 && my_scr <= hit_y1;
    let inside_pos_minimap =
        mx_scr >= pos_hit_x0 && mx_scr <= pos_hit_x1 && my_scr >= pos_hit_y0 && my_scr <= pos_hit_y1;
    let inside_any_minimap = inside_pow_minimap || inside_pos_minimap;

    if !ui.capture {
        let dxm = (input_ctx.mouse_pos().0.clamp(0, draw_ctx.window_width) - draw_ctx.window_width/2) as f32;
        let dym = (input_ctx.mouse_pos().1.clamp(0, draw_ctx.window_height) - draw_ctx.window_height/2) as f32;
        let old_screen_unit = SCREEN_UNIT_CONST * (ZOOM_FACTOR.powf(viz_state.zoom) * ui.dpi_scale);
        viz_state.zoom += input_ctx.zoom_delta as f32;
        viz_state.zoom = viz_state.zoom.min(26.0);
        let new_screen_unit = SCREEN_UNIT_CONST * (ZOOM_FACTOR.powf(viz_state.zoom) * ui.dpi_scale);
        viz_state.camera_x += (dxm / old_screen_unit) - (dxm / new_screen_unit);
        viz_state.camera_y += (dym / old_screen_unit) - (dym / new_screen_unit);
    }

    let zoom = (ZOOM_FACTOR.powf(viz_state.zoom) * ui.dpi_scale);
    // origin
    let screen_unit = SCREEN_UNIT_CONST * zoom;
    let very_zoom_out = screen_unit < 0.16;

    if viz_state.follow_tip {
        viz_state.camera_y = e_lerp(viz_state.camera_y, viz_state.bc_tip_y, dt);
    }

    if ui.mouse_pressed_id == ui::Id::VIZ_GUI && input_ctx.mouse_held(MouseButton::Left) {
        if input_ctx.mouse_delta() != (0, 0) {
            viz_state.follow_tip = false;
        }
        viz_state.camera_x -= input_ctx.mouse_delta().0 as f32 / screen_unit;
        viz_state.camera_y -= input_ctx.mouse_delta().1 as f32 / screen_unit;
    }

    let mut minimap_wheel_scrubbed = false;
    if inside_any_minimap {
        let dy = effective_vertical_wheel_for_scrub(input_ctx);
        if dy.abs() > 1e-6
            || input_ctx.scroll_delta.0.abs() > 1e-6
            || input_ctx.scroll_delta.1.abs() > 1e-6
            || input_ctx.zoom_delta.abs() > 1e-12
        {
            if inside_pos_minimap {
                let (bft_min, bft_max) = bft_minimap_height_span(viz_state);
                let strip_h = (pos_mm_y1 - pos_mm_y0).max(1.0);
                let span_b = bft_max.saturating_sub(bft_min).max(1) as f32;
                let dh_b = minimap_wheel_delta_height(dy, strip_h, span_b);
                let h_cur = viz_state.nearest_bft_height_to_camera_y(viz_state.camera_y) as f32;
                let h_next = (h_cur + dh_b).round() as u64;
                apply_scrub_bft_height(viz_state, h_next);
                minimap_wheel_scrubbed = true;
            } else {
                let (h_min, h_max) = chain_minimap_height_span(viz_state);
                let strip_h = (mm_y1 - mm_y0).max(1.0);
                let span_h = h_max.saturating_sub(h_min).max(1) as f32;
                let dh = minimap_wheel_delta_height(dy, strip_h, span_h);
                let h_cur = ((-viz_state.camera_y) / 10.0).clamp(h_min as f32, h_max as f32);
                apply_scrub_pow_height(viz_state, h_cur + dh, h_min, h_max);
                minimap_wheel_scrubbed = true;
            }
        }
    }

    viz_state.camera_x -= input_ctx.scroll_delta.0 as f32 / screen_unit;
    if !inside_any_minimap || !minimap_wheel_scrubbed {
        if input_ctx.scroll_delta.1 != 0.0 {
            viz_state.follow_tip = false;
        }
        viz_state.camera_y -= input_ctx.scroll_delta.1 as f32 / screen_unit;
    }

    let mut origin_x = (draw_ctx.window_width / 2) as f32 - viz_state.camera_x * screen_unit;
    let mut origin_y = (draw_ctx.window_height / 2) as f32 - viz_state.camera_y * screen_unit;

    let mut world_mouse_x = viz_state.camera_x + ((input_ctx.mouse_pos().0.clamp(0, draw_ctx.window_width) - draw_ctx.window_width/2) as f32) / screen_unit;
    let mut world_mouse_y = viz_state.camera_y + ((input_ctx.mouse_pos().1.clamp(0, draw_ctx.window_height) - draw_ctx.window_height/2) as f32) / screen_unit;

    let mut minimap_scrub_this_frame = false;
    if !ui.capture || inside_any_minimap {
        if (ui.mouse_pressed_id == ui::Id::CHAIN_MINIMAP_POW || ui.mouse_pressed_id == ui::Id::CHAIN_MINIMAP_POS)
            && input_ctx.mouse_held(MouseButton::Left)
        {
            if ui.mouse_pressed_id == ui::Id::CHAIN_MINIMAP_POS {
                let (bft_min, bft_max) = bft_minimap_height_span(viz_state);
                let ht = chain_minimap_y_to_h(
                    my_scr.clamp(pos_mm_y0, pos_mm_y1),
                    bft_min,
                    bft_max,
                    pos_mm_y0,
                    pos_mm_y1,
                );
                apply_scrub_bft_height(viz_state, ht);
                minimap_scrub_this_frame = true;
            } else {
                let (h_min, h_max) = chain_minimap_height_span(viz_state);
                let ht = chain_minimap_y_to_h(my_scr.clamp(mm_y0, mm_y1), h_min, h_max, mm_y0, mm_y1);
                apply_scrub_pow_height(viz_state, ht as f32, h_min, h_max);
                minimap_scrub_this_frame = true;
            }
        }
        if ui.mouse_pressed_id == ui::Id::default()
            && input_ctx.mouse_pressed(MouseButton::Left)
            && inside_pow_minimap
        {
            let (h_min, h_max) = chain_minimap_height_span(viz_state);
            let ht = chain_minimap_y_to_h(my_scr, h_min, h_max, mm_y0, mm_y1);
            apply_scrub_pow_height(viz_state, ht as f32, h_min, h_max);
            //viz_state.zoom = 2.0;
            ui.mouse_pressed_id = ui::Id::CHAIN_MINIMAP_POW;
            minimap_scrub_this_frame = true;
        }
        if ui.mouse_pressed_id == ui::Id::default()
            && input_ctx.mouse_pressed(MouseButton::Left)
            && inside_pos_minimap
        {
            let (bft_min, bft_max) = bft_minimap_height_span(viz_state);
            let ht = chain_minimap_y_to_h(my_scr, bft_min, bft_max, pos_mm_y0, pos_mm_y1);
            apply_scrub_bft_height(viz_state, ht);
            //viz_state.zoom = 2.0;
            ui.mouse_pressed_id = ui::Id::CHAIN_MINIMAP_POS;
            minimap_scrub_this_frame = true;
        }
    }
    if !ui.capture {
        if ui.mouse_pressed_id == ui::Id::default() && input_ctx.mouse_pressed(MouseButton::Left) && !inside_any_minimap {
            ui.mouse_pressed_id = ui::Id::VIZ_GUI;
        }
    }

    origin_x = (draw_ctx.window_width / 2) as f32 - viz_state.camera_x * screen_unit;
    origin_y = (draw_ctx.window_height / 2) as f32 - viz_state.camera_y * screen_unit;
    world_mouse_x = viz_state.camera_x + ((input_ctx.mouse_pos().0.clamp(0, draw_ctx.window_width) - draw_ctx.window_width/2) as f32) / screen_unit;
    world_mouse_y = viz_state.camera_y + ((input_ctx.mouse_pos().1.clamp(0, draw_ctx.window_height) - draw_ctx.window_height/2) as f32) / screen_unit;

    {
        let sday = (viz_state.bc_finalized_tip_height) / UI_COPY_STAKING_PERIOD;
        let mut draw_staking_day_section = | day | {
            let y2 = -10.0 * ((day*UI_COPY_STAKING_PERIOD) as f32 - 0.25) * screen_unit;
            let y1 = -10.0 * ((day*UI_COPY_STAKING_PERIOD + UI_COPY_STAKING_DAY_WINDOW) as f32 - 0.75) * screen_unit;

            let text_height = 2.0 * screen_unit;
            let line_thickness = 0.5 * screen_unit;
            let line_width = 60.0;

            let mut bft_keys = viz_state.on_screen_bfts.keys();
            let bft_x = viz_state.on_screen_bfts.get(bft_keys.nth(0).unwrap_or(&Hash32::from_u64(0))).unwrap_or(&OnScreenBft::default()).x;

            let right_margin = draw_ctx.window_width as f32 - (draw_ctx.window_width as f32 * PANE_PERCENT_RIGHT);

            // window
            draw_ctx.rectangle_r(0.0, origin_y + y1, draw_ctx.window_width as f32, origin_y + y2, 1, 0x06ffffff);

            // start
            let start_color = 0xaa5fdc4c;
            let str = "* STAKING DAY START *";
            let start_w = draw_ctx.measure_text_line(FontKind::Mono, text_height, str);
            draw_ctx.text_line(FontKind::Mono, (right_margin - start_w).max(origin_x + bft_x + 10.0 * screen_unit), origin_y + y2 + (text_height - 4.0 /* @note(judah): do not ask about the number 4 */), text_height, str, start_color);
            draw_ctx.rectangle_r(0.0, origin_y + y2, draw_ctx.window_width as f32, origin_y + y2 + line_thickness, 1, start_color);

            // end
            let end_color = 0xaadc4c4f;
            let str = "* STAKING DAY END *";
            let end_w = draw_ctx.measure_text_line(FontKind::Mono, text_height, str);
            draw_ctx.text_line(FontKind::Mono, (right_margin - end_w).max(origin_x + bft_x + 10.0 * screen_unit), (origin_y + y1) - (text_height + 1.0), text_height, str, end_color);
            draw_ctx.rectangle_r(0.0, origin_y + y1, draw_ctx.window_width as f32, origin_y + y1 + line_thickness, 1, end_color);
        };

        draw_staking_day_section(sday);
        draw_staking_day_section(sday + 1);
    }

    let mut hovered_block = Hash32::from_u64(0);
    let mut hovered_block_screen_x = 0.0;
    let mut hovered_block_screen_y = 0.0;
    if let Some(hash) = hovered_hash_in_world(
        viz_state.on_screen_bcs.values(),
        world_mouse_x,
        world_mouse_y,
        |on_screen_bc| (on_screen_bc.x, on_screen_bc.y, on_screen_bc.block.this_hash),
    ) {
        hovered_block = hash;
    }
    if let Some(hash) = hovered_hash_in_world(
        viz_state.on_screen_bfts.values(),
        world_mouse_x,
        world_mouse_y,
        |on_screen_bft| (on_screen_bft.x, on_screen_bft.y, on_screen_bft.block.this_hash),
    ) {
        hovered_block = hash;
    }

    if inside_any_minimap {
        hovered_block = Hash32::from_u64(0);
    }

    let viz_blocks = apply_viz_op(viz_state, hovered_block, ui.viz_op);

    for on_screen_bc in magic(&mut viz_state.on_screen_bcs).values_mut() {
        if on_screen_bc.block.this_hash == hovered_block || viz_blocks.contains(&on_screen_bc.block.this_hash) {
            on_screen_bc.t_roundness = 0.3;
            on_screen_bc.t_darkness = 0.2;
            if input_ctx.key_pressed(KeyCode::Space) {
                on_screen_bc.x = 0.0;
                on_screen_bc.y = 0.0;
                on_screen_bc.alpha = 0.0;
            }
            if input_ctx.mouse_pressed(MouseButton::Left) && !minimap_scrub_this_frame {
                viz_state.follow_tip = false;
                viz_state.camera_x = on_screen_bc.t_x;
                viz_state.camera_y = on_screen_bc.t_y;
                viz_state.zoom = 2.0;
            }

            on_screen_bc.t_bft_arrow_alpha = 1.0;
        } else {
            on_screen_bc.t_roundness = 1.0;
            on_screen_bc.t_darkness = 0.0;

            on_screen_bc.t_bft_arrow_alpha = if on_screen_bc.block.is_best_chain { 1.0 } else { 0.1 };
        }
        if on_screen_bc.block.this_hash == viz_state.inspecting_block_hash { on_screen_bc.t_darkness += 0.2; }
        on_screen_bc.t_finalized_alpha = if on_screen_bc.block.is_finalized { 1.0 } else { 0.0 };
        on_screen_bc.t_implicated_by_bft_alpha = if on_screen_bc.block.is_implicated_by_bft { 1.0 } else { 0.0 };
        on_screen_bc.t_x = -5.0;
        on_screen_bc.t_y = -10.0 * on_screen_bc.block.this_height as f32;

        if on_screen_bc.block.this_height == viz_state.bc_tip_height {
            viz_state.bc_tip_y = on_screen_bc.t_y;
        }

        if !on_screen_bc.block.is_best_chain {
            on_screen_bc.alpha = 0.25;
        }
    }
    for on_screen_bft in magic(&mut viz_state.on_screen_bfts).values_mut() {
        if on_screen_bft.block.this_hash == hovered_block || viz_blocks.contains(&on_screen_bft.block.this_hash) {
            on_screen_bft.t_roundness = 0.3;
            on_screen_bft.t_darkness = 0.2;
            if input_ctx.key_pressed(KeyCode::Space) {
                on_screen_bft.x = 0.0;
                on_screen_bft.y = 0.0;
                on_screen_bft.alpha = 0.0;
            }
            // same deal as PoW branch above, bft click to recenter should not piggyback on minimap press
            if input_ctx.mouse_pressed(MouseButton::Left) && !minimap_scrub_this_frame {
                viz_state.follow_tip = false;
                viz_state.camera_x = on_screen_bft.t_x;
                viz_state.camera_y = on_screen_bft.t_y;
                viz_state.zoom = 2.0;
            }
        } else {
            on_screen_bft.t_roundness = 1.0;
            on_screen_bft.t_darkness = 0.0;
        }
        if on_screen_bft.block.this_hash == viz_state.inspecting_block_hash { on_screen_bft.t_darkness += 0.2; }
        on_screen_bft.t_x = 5.0;
        on_screen_bft.t_y = if let Some(on_screen_bc) = viz_state.on_screen_bcs.get(&on_screen_bft.block.points_at_bc_block) {
            on_screen_bc.y - 10.0 / 2.0
        } else {
            if let Some(parent_bft) = viz_state.on_screen_bfts.get(&on_screen_bft.block.parent_hash) {
                parent_bft.y - 10.0
            } else {
                on_screen_bft.t_y
            }
        }
    }

    let null_hash_display_string = Hash32::from_u64(0).display_str();
    let null_hash_display_string_star = format!("{}*", &null_hash_display_string);
    let null_hash_is_rectangle = draw_ctx.measure_text_line_is_rectangle(FontKind::Mono, screen_unit, &null_hash_display_string_star);
    {
        let mut working_map = HashMap::<Hash32, u16>::new();
        let mut width_map = HashMap::<u64, u16>::new();
        let mut off_chain: Vec<(u64, Hash32)> = Vec::new();
        for bc in viz_state.on_screen_bcs.values() {
            if bc.block.is_best_chain == false {
                off_chain.push((bc.block.this_height, bc.block.this_hash));
            }
        }
        off_chain.sort_by_key(|x| u64::MAX - x.0);

        fn recurse_layout_children_then_self(this_index: usize, off_chain: &Vec<(u64, Hash32)>, working_map: &mut HashMap<Hash32, u16>, width_map: &mut HashMap<u64, u16>, on_screen_bcs: &HashMap<Hash32, OnScreenBc>, min_width: u16) {
            let (this_height, this_hash) = off_chain[this_index];
            let here_width = *width_map.get(&this_height).unwrap_or(&0).max(&min_width);

            let mut child_scanner = this_index;
            while child_scanner > 0 {
                child_scanner -= 1;
                let (other_height, other_hash) = off_chain[child_scanner];
                if other_height > this_height + 1 { break; }
                let block = on_screen_bcs.get(&other_hash).unwrap().block;

                if block.parent_hash == this_hash {
                    recurse_layout_children_then_self(child_scanner, off_chain, working_map, width_map, on_screen_bcs, here_width);
                }
            }

            working_map.insert(this_hash, here_width);
            width_map.insert(this_height, here_width + 1);
        }

        for (index, (height, hash)) in off_chain.iter().enumerate() {
            let parent_hash = viz_state.on_screen_bcs.get(&hash).unwrap().block.parent_hash;
            if let Some(parent) = viz_state.on_screen_bcs.get_mut(&parent_hash) {
                if parent.block.is_best_chain {
                    recurse_layout_children_then_self(index, &off_chain, &mut working_map, &mut width_map, &viz_state.on_screen_bcs, 1);
                    let (this_height, this_hash) = off_chain[index];
                    *working_map.get_mut(&this_hash).unwrap() -= 1;
                }
            }
        }

        let hash_text_line_w = draw_ctx.measure_text_line(FontKind::Mono, screen_unit, &null_hash_display_string_star) / screen_unit;
        for (hash, x_pos) in &working_map {
            viz_state.on_screen_bcs.get_mut(hash).unwrap().t_x = -5.0 - (hash_text_line_w+3.0) * (1.0 + *x_pos as f32);
        }
    }

    // animate to targets
    for on_screen_bc in viz_state.on_screen_bcs.values_mut() {
        animate_bc_towards_target(on_screen_bc, dt);
    }
    for on_screen_bft in viz_state.on_screen_bfts.values_mut() {
        animate_bft_towards_target(on_screen_bft, dt);
    }

    //draw_ctx.circle(origin_x as f32, origin_y as f32, (screen_unit/2.0) as f32, 0xff_0000bb);

    let arrow_and_line_width = screen_unit / 12.0;

    for on_screen_bc in viz_state.on_screen_bcs.values() {
        let x = on_screen_bc.x;
        let y = on_screen_bc.y;
        let finalized = on_screen_bc.block.is_best_chain && on_screen_bc.block.this_height <= viz_state.bc_finalized_tip_height;
        let base_color = if viz_blocks.last() == Some(&on_screen_bc.block.this_hash) {
            COLOR_BRIGHT
        } else if on_screen_bc.block.is_best_chain {
            // if viz_state.ui_hovered_height.is_some() && on_screen_bc.block.this_height == viz_state.ui_hovered_height.unwrap().0 as u64 {
            COLOR_BC
        } else {
            COLOR_NBC
        };

        let color        = (((on_screen_bc.alpha*255.0)                              as u32) << 24) | blend_u32(0x000000, base_color,   ((1.0 - on_screen_bc.darkness) * 255.0) as u32);
        let color_accent = (((on_screen_bc.alpha*on_screen_bc.finalized_alpha*255.0) as u32) << 24) | blend_u32(0x000000, COLOR_ACCENT, ((1.0 - on_screen_bc.darkness) * 255.0) as u32);

        if hovered_block == on_screen_bc.block.this_hash {
            hovered_block_screen_x = origin_x + (x*screen_unit);
            hovered_block_screen_y = origin_y + (y*screen_unit);
        }
        if viz_state.inspecting_block_hash == on_screen_bc.block.this_hash {
            viz_state.inspecting_block_screen_x = origin_x + (x*screen_unit);
            viz_state.inspecting_block_screen_y = origin_y + (y*screen_unit);
        }

        // draw PoW node
        {
            let rad = 0.5*screen_unit * ((2.0 * on_screen_bc.block.txs_n as f32).log2() * 0.25).max(1.0);

            let (pt_x, pt_y) = (origin_x + (x*screen_unit), origin_y + (y*screen_unit));
            if very_zoom_out {
                draw_ctx.rectangle(pt_x - screen_unit*30.0, pt_y, pt_x + screen_unit*5.0, pt_y + screen_unit*2.0, (color&0xFFffFF) | ((color >> 26) << 24));
            } else {
                draw_ctx.circle_square(pt_x, pt_y, rad,  rad  * on_screen_bc.roundness, color);
                let rad2 = 0.5 * rad;
                draw_ctx.circle_square(pt_x, pt_y, rad2, rad2 * on_screen_bc.roundness, color_accent);
            }

            if !finalized {
                draw_ctx.circle_square(pt_x, pt_y, rad * 0.65, rad * on_screen_bc.roundness * 0.65, 0xFF080808);
            }

            // Hardfork activation marker: a small orange badge just below this block,
            // which sits at a user-led hardfork's PoW activation height. Several forked
            // blocks may share that height; each one is marked independently.
            if on_screen_bc.block.is_hardfork_activation {
                let orange = (((on_screen_bc.alpha*255.0) as u32) << 24) | 0xFF8C00;
                let marker_rad = 0.6 * screen_unit;
                draw_ctx.circle_square(pt_x, pt_y + 1.2*screen_unit, marker_rad, marker_rad * on_screen_bc.roundness, orange);
            }
        }

        if very_zoom_out == false {
            let here_text_y = (origin_y + (y - 0.5)*screen_unit);
            if here_text_y <= draw_ctx.window_height as f32 && here_text_y + screen_unit >= 0.0 {
                let w = if on_screen_bc.block.is_best_chain {
                    // hash
                    let text_line_buf;
                    let text_line = if null_hash_is_rectangle {
                        &null_hash_display_string
                    } else {
                        text_line_buf = on_screen_bc.block.this_hash.display_str();
                        &text_line_buf
                    };
                    let w = draw_ctx.measure_text_line(FontKind::Mono, screen_unit, &text_line) / screen_unit;
                    draw_ctx.text_line(FontKind::Mono, origin_x + (x - 1.5 - w)*screen_unit, here_text_y as f32, screen_unit, &on_screen_bc.block.this_hash.display_str(), color);

                    let height_text_buf;
                    let height_text = if null_hash_is_rectangle {
                        "12345"
                    } else {
                        height_text_buf = format!("{}", on_screen_bc.block.this_height);
                        &height_text_buf
                    };
                    // height
                    draw_ctx.text_line(FontKind::Mono, origin_x + (x + 1.5)*screen_unit, here_text_y as f32, screen_unit, height_text, color);
                    w
                } else {
                    // hash
                    let text_line_buf;
                    let text_line = if null_hash_is_rectangle {
                        &null_hash_display_string_star
                    } else {
                        text_line_buf = format!("{}*", &on_screen_bc.block.this_hash.display_str());
                        &text_line_buf
                    };
                    let w = draw_ctx.measure_text_line(FontKind::Mono, screen_unit, &text_line) / screen_unit;
                    draw_ctx.text_line(FontKind::Mono, origin_x + (x - 1.5 - w)*screen_unit, here_text_y as f32, screen_unit, &text_line, color);
                    w
                };

                let extra_info = DateTime::<Utc>::from_timestamp_secs(on_screen_bc.block.utc).unwrap_or(DateTime::<Utc>::MAX_UTC).to_string();
                let extra_info = &extra_info[..extra_info.len()-4]; // remove " UTC"
                let extra_info2 = format!("work: 0x{:x}", on_screen_bc.block.work);
                let extra_info3 = format!("size: {} ({})", on_screen_bc.block.serialized_size, (on_screen_bc.block.serialized_size + 37 + 1399) / 1400);
                // #[cfg(debug_assertions)]
                let s = screen_unit as f32 * 1.2; // .2 to include padding
                draw_ctx.text_line(FontKind::Mono, origin_x + (x - 1.5 - w)*screen_unit, here_text_y + 1.0*s, screen_unit, extra_info, color);
                draw_ctx.text_line(FontKind::Mono, origin_x + (x - 1.5 - w)*screen_unit, here_text_y + 2.0*s, screen_unit, &extra_info2, color);
                draw_ctx.text_line(FontKind::Mono, origin_x + (x - 1.5 - w)*screen_unit, here_text_y + 3.0*s, screen_unit, &extra_info3, color);
            }

            if let Some(parent) = viz_state.on_screen_bcs.get(&on_screen_bc.block.parent_hash) {
                let px = parent.x;
                let py = parent.y;
                let dx = px-x;
                let dy = py-y;
                let (dx, dy, l) = split_vector(dx, dy);

                draw_ctx.line(
                    origin_x + (x + dx * 2.0) * screen_unit,
                    origin_y + (y + dy * 2.0) * screen_unit,
                    origin_x + (x + dx * (l - 2.0))*screen_unit,
                    origin_y + (y + dy * (l - 2.0) )*screen_unit,
                    arrow_and_line_width,
                    base_color | (((on_screen_bc.alpha*255.0) as u32) << 24),
                );
            }
            if let Some(pointing_at_bft) = viz_state.on_screen_bfts.get(&on_screen_bc.block.points_at_bft_block) {
                let px = pointing_at_bft.x;
                let py = pointing_at_bft.y;
                let dx = px-x;
                let dy = py-y;
                let (dx, dy, l) = split_vector(dx, dy);
                let x1 = origin_x + (x + dx * 2.0) * screen_unit;
                let y1 = origin_y + (y + dy * 2.0) * screen_unit;
                let x2 = origin_x + (x + dx * (l - 2.0))*screen_unit;
                let y2 = origin_y + (y + dy * (l - 2.0) )*screen_unit;
                // Don't draw if neither end point is anywhere near on screen.
                if  y1.min(y2) > -2.0 * draw_ctx.window_height as f32 ||
                    y1.max(y2) < 2.0 * draw_ctx.window_height as f32
                {
                    draw_ctx.arrow(
                        x1, y1, x2, y2,
                        arrow_and_line_width, COLOR_BFT_LINK | (((on_screen_bc.alpha*on_screen_bc.bft_arrow_alpha*255.0) as u32) << 24),
                    );
                }
            }
        }
    }

    for on_screen_bft in viz_state.on_screen_bfts.values() {
        let x = on_screen_bft.x;
        let y = on_screen_bft.y;
        let base_color = if viz_blocks.last() == Some(&on_screen_bft.block.this_hash) {
            COLOR_BRIGHT
        } else {
            COLOR_BFT
        };
        let color = (((on_screen_bft.alpha*255.0) as u32) << 24) | blend_u32(0x000000, base_color, ((1.0 - on_screen_bft.darkness) * 255.0) as u32);

        if hovered_block == on_screen_bft.block.this_hash {
            hovered_block_screen_x = origin_x + (x*screen_unit);
            hovered_block_screen_y = origin_y + (y*screen_unit);
        }
        if viz_state.inspecting_block_hash == on_screen_bft.block.this_hash {
            viz_state.inspecting_block_screen_x = origin_x + (x*screen_unit);
            viz_state.inspecting_block_screen_y = origin_y + (y*screen_unit);
        }
        if very_zoom_out {
            draw_ctx.rectangle(origin_x + (x*screen_unit) - screen_unit*5.0, origin_y + (y*screen_unit), origin_x + (x*screen_unit) + screen_unit*30.0, origin_y + (y*screen_unit) + screen_unit*4.0, (color&0xFFffFF) | ((color >> 26) << 24));
        }
        else {
            draw_ctx.circle_square(origin_x + (x*screen_unit), origin_y + (y*screen_unit), screen_unit, screen_unit*on_screen_bft.roundness, color);
        }

        // Hardfork warning marker: a small orange badge a little above and to the
        // right of this block, signalling that the *next* BFT block will activate
        // a user-led hardfork.
        if on_screen_bft.block.next_block_is_hardfork {
            let orange = (((on_screen_bft.alpha*255.0) as u32) << 24) | 0xFF8C00;
            let rad = 0.6 * screen_unit;
            draw_ctx.circle_square(origin_x + (x + 1.2)*screen_unit, origin_y + (y - 1.2)*screen_unit, rad, rad*on_screen_bft.roundness, orange);
        }

        if very_zoom_out == false {
            let here_text_y = (origin_y + (y - 0.5)*screen_unit);
            if here_text_y <= draw_ctx.window_height as f32 && here_text_y + screen_unit >= 0.0 {
                // hash
                draw_ctx.text_line(FontKind::Mono, (origin_x + (x + 1.5)*screen_unit) as f32, (origin_y + (y - 0.5)*screen_unit) as f32, screen_unit as f32, &on_screen_bft.block.this_hash.display_str(), color);

                // height
                let text_line = format!("{}", on_screen_bft.block.this_height);
                let text_line_w = draw_ctx.measure_text_line(FontKind::Mono, screen_unit, &text_line);
                draw_ctx.text_line(FontKind::Mono, origin_x + (x - 1.5)*screen_unit - text_line_w, (origin_y + (y - 0.5)*screen_unit) as f32, screen_unit, &text_line, color);
            }

            if let Some(parent) = viz_state.on_screen_bfts.get(&on_screen_bft.block.parent_hash) {
                let px = parent.x;
                let py = parent.y;
                let dx = px-x;
                let dy = py-y;
                let (dx, dy, l) = split_vector(dx, dy);
                draw_ctx.line(
                    origin_x + (x + dx * 2.0) * screen_unit,
                    origin_y + (y + dy * 2.0) * screen_unit,
                    origin_x + (x + dx * (l - 2.0))*screen_unit,
                    origin_y + (y + dy * (l - 2.0) )*screen_unit,
                    arrow_and_line_width, COLOR_BFT_LINK | (((on_screen_bft.alpha*255.0) as u32) << 24),
                );
            }
            if let Some(pointing_at_bc) = viz_state.on_screen_bcs.get(&on_screen_bft.block.points_at_bc_block) {
                let px = pointing_at_bc.x;
                let py = pointing_at_bc.y;
                let dx = px-x;
                let dy = py-y;
                let (dx, dy, l) = split_vector(dx, dy);
                draw_ctx.arrow(
                    origin_x + (x + dx * 2.0) * screen_unit,
                    origin_y + (y + dy * 2.0) * screen_unit,
                    origin_x + (x + dx * (l - 2.0))*screen_unit,
                    origin_y + (y + dy * (l - 2.0) )*screen_unit,
                    arrow_and_line_width, COLOR_CROSS_LINK | (((on_screen_bft.alpha*255.0) as u32) << 24),
                );
            }
        }
    }

    draw_chain_minimap_overlay(viz_state, ui, draw_ctx, origin_y, screen_unit, mm_rect);
    draw_pos_minimap_overlay(viz_state, ui, draw_ctx, origin_y, screen_unit, pos_mm_rect);

    if viz_state.last_frame_hovered_hash != hovered_block {
        viz_state.last_frame_hovered_hash = hovered_block;
        if hovered_block != Hash32::from_u64(0)
        { play_sound(SOUND_UI_HOVER, 0.5, 1.0); }
    }

    if !ui.capture && input_ctx.mouse_pressed(MouseButton::Left) && !minimap_scrub_this_frame {
        viz_state.inspecting_block_hash = hovered_block;
        viz_state.inspecting_block_screen_x = hovered_block_screen_x;
        viz_state.inspecting_block_screen_y = hovered_block_screen_y;
        viz_state.block_inspection = None;
    }

    let ww = draw_ctx.window_width as f32;
    let in_center_column =
        mx_scr >= ww * PANE_PERCENT_LEFT && mx_scr <= ww * (1.0 - PANE_PERCENT_RIGHT);
    let wheelish =
        input_ctx.scroll_delta != (0.0, 0.0) || input_ctx.zoom_delta != 0.0;
    ui.suppress_scroll_for_clay = ((inside_any_minimap && wheelish) || minimap_wheel_scrubbed)
        || ((ui.mouse_pressed_id == ui::Id::CHAIN_MINIMAP_POW || ui.mouse_pressed_id == ui::Id::CHAIN_MINIMAP_POS)
            && input_ctx.mouse_held(MouseButton::Left))
        || (in_center_column && wheelish && !ui.capture);
}

fn split_vector(x: f32, y: f32) -> (f32, f32, f32) {
    let len = f32::sqrt(x*x + y*y);
    if len < 0.0000001 { return (0.0, 0.0, 0.0); }
    (x/len, y/len, len)
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)] // , Serialize, Deserialize)]
pub struct Hash32 {
    le_chunks: [u64; 4],
}
impl Hash32 {
    #[inline]
    pub fn as_bytes(&self) -> [u8; 32] {
        let mut out = [0u8; 32];
        let mut i = 0;
        for &chunk in &self.le_chunks {
            let le = chunk.to_le_bytes(); // linear memory bytes for a LE u64
            out[i..i + 8].copy_from_slice(&le);
            i += 8;
        }
        out
    }
    #[inline]
    pub fn from_bytes(bytes: [u8; 32]) -> Hash32 {
        let mut chunks = [0u64; 4];

        for i in 0..4 {
            let start = i * 8;
            let end = start + 8;
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&bytes[start..end]);
            chunks[i] = u64::from_le_bytes(buf);
        }

        Hash32 { le_chunks: chunks }
    }
    pub fn from_u64(u: u64) -> Hash32 {
        Hash32 { le_chunks: [u,0u64,0u64,0u64], }
    }

    pub fn display_str(&self) -> String {
        let mut str = String::new();
        let mut bytes = self.as_bytes();
        bytes.reverse();

        for b in &bytes[0..4] {
            str.push_str(&format!("{:02x}", b));
        }
        str.push_str("..");
        for b in &bytes[bytes.len() - 4..] {
            str.push_str(&format!("{:02x}", b));
        }

        str
    }
}

impl std::fmt::Display for Hash32 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for b in self.as_bytes().iter().rev() {
            write!(f, "{:02x}", b)?;
        }
        Ok(())
    }
}

impl std::fmt::Debug for Hash32 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(self, f)
    }
}
