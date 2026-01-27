#![allow(warnings)]

use std::{collections::HashMap, hash::Hash, sync::Mutex};

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

// @Todo: enum BlockInspection {
//     None,
//     PoW(Arc<zebra_chain::Block>),
//     PoS(Arc<zebra_chain::block::BftBlock>)
// }

pub struct ResponseFromZebra {
    pub bc_tip_height: u64,
    pub bc_finalized_tip_height: u64,
    pub bft_tip_height: u64,
    pub bc_blocks: Vec<BcBlock>,
    pub bft_blocks: Vec<BftBlock>,
    pub what_block_it_is: Hash32,
    pub json_dump_of_the_block: String, // @Todo: @Remove and replace with structured data.
    // @Todo: pub block_inspection: BlockInspection,
    pub start_bc_height: u64,

    pub orchard_pool_balance: i64,
    pub staking_bonded_pool_balance: i64,
    pub staking_unbonded_pool_balance: i64,

    pub peer_strings: Vec<String>,
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
            json_dump_of_the_block: "Data not available.".to_owned(),
            // @Todo: block_inspection: BlockInspection::None,
            start_bc_height: 0,
            orchard_pool_balance: 0,
            staking_bonded_pool_balance: 0,
            staking_unbonded_pool_balance: 0,
            peer_strings: Vec::new(),
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
}
impl Default for BftBlock {
    fn default() -> Self {
        BftBlock {
            this_hash: Hash32::from_u64(0),
            parent_hash: Hash32::from_u64(0),
            this_height: 0,
            points_at_bc_block: Hash32::from_u64(0),
            proving_blocks: Vec::with_capacity(0),
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
    pub on_screen_bcs: HashMap<Hash32, OnScreenBc>,
    pub on_screen_bfts: HashMap<Hash32, OnScreenBft>,
    pub send_to_zebra: std::sync::mpsc::SyncSender<RequestToZebra>,
    pub receive_from_zebra: std::sync::mpsc::Receiver<ResponseFromZebra>,

    pub time_since_last_animation: std::time::Instant,

    pub bc_tip_y: f32,

    pub bc_tip_height: u64,
    pub bc_finalized_tip_height: u64,
    pub bft_tip_height: u64,
    pub ui_hovered_height: Option<BlockHeight>,

    pub last_frame_hovered_hash: Hash32,

    pub inspecting_block_hash: Hash32,
    pub inspect_block_json_text: Option<String>,

    pub inspecting_block_screen_x: f32,
    pub inspecting_block_screen_y: f32,

    pub bft_ack_height: u64,
    pub bc_ack_height: u64,

    pub orchard_pool_balance: i64,
    pub staking_bonded_pool_balance: i64,
    pub staking_unbonded_pool_balance: i64,

    pub peer_strings: Vec<String>,
}

impl VizState {
    pub fn pos_at_height(&self, height: BlockHeight) -> (f32, f32, bool) {
        if height.is_in_block() {
            (0.0, -10.0 * height.0 as f32, true) // @todo: should handle sidechain x-axis, maybe this should take a hash instead
        } else {
            (0.0, 0.0, false)
        }
    }
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
        inspect_block_json_text: None,

        inspecting_block_screen_x: 0.0,
        inspecting_block_screen_y: 0.0,

        time_since_last_animation: Instant::now(),

        bft_ack_height: 0,
        bc_ack_height: 0,

        orchard_pool_balance: 0,
        staking_bonded_pool_balance: 0,
        staking_unbonded_pool_balance: 0,
        peer_strings: Vec::new(),
    };
    if fake_data {
        let block = OnScreenBc { block: BcBlock { this_hash: Hash32::from_u64(1), parent_hash: Hash32::from_u64(0), this_height: 0, txs_n: 1, is_best_chain: true, is_finalized: true, is_implicated_by_bft: false, points_at_bft_block: Hash32::from_u64(0), }, ..Default::default() };
        viz_state.on_screen_bcs.insert(block.block.this_hash, block);
        let block = OnScreenBc { block: BcBlock { this_hash: Hash32::from_u64(2), parent_hash: Hash32::from_u64(1), this_height: 1, txs_n: 6, is_best_chain: true, is_finalized: false, is_implicated_by_bft: true, points_at_bft_block: Hash32::from_u64(5), }, ..Default::default() };
        viz_state.on_screen_bcs.insert(block.block.this_hash, block);
        let block = OnScreenBc { block: BcBlock { this_hash: Hash32::from_u64(3), parent_hash: Hash32::from_u64(2), this_height: 2, txs_n: 2, is_best_chain: true, is_finalized: false, is_implicated_by_bft: true, points_at_bft_block: Hash32::from_u64(5), }, ..Default::default() };
        viz_state.on_screen_bcs.insert(block.block.this_hash, block);
        let block = OnScreenBc { block: BcBlock { this_hash: Hash32::from_u64(8), parent_hash: Hash32::from_u64(1), this_height: 1, txs_n: 4256, is_best_chain: false, is_finalized: false, is_implicated_by_bft: false, points_at_bft_block: Hash32::from_u64(5), }, ..Default::default() };
        viz_state.on_screen_bcs.insert(block.block.this_hash, block);
        let block = OnScreenBc { block: BcBlock { this_hash: Hash32::from_u64(9), parent_hash: Hash32::from_u64(2), this_height: 2, txs_n: 16, is_best_chain: false, is_finalized: false, is_implicated_by_bft: false, points_at_bft_block: Hash32::from_u64(5), }, ..Default::default() };
        viz_state.on_screen_bcs.insert(block.block.this_hash, block);
        let block = OnScreenBc { block: BcBlock { this_hash: Hash32::from_u64(10), parent_hash: Hash32::from_u64(8), this_height: 2, txs_n: 15, is_best_chain: false, is_finalized: false, is_implicated_by_bft: false, points_at_bft_block: Hash32::from_u64(5), }, ..Default::default() };
        viz_state.on_screen_bcs.insert(block.block.this_hash, block);
        let block = OnScreenBc { block: BcBlock { this_hash: Hash32::from_u64(11), parent_hash: Hash32::from_u64(10), this_height: 3, txs_n: 3, is_best_chain: false, is_finalized: false, is_implicated_by_bft: false, points_at_bft_block: Hash32::from_u64(5), }, ..Default::default() };
        viz_state.on_screen_bcs.insert(block.block.this_hash, block);

        let block = OnScreenBft { block: BftBlock { this_hash: Hash32::from_u64(5), parent_hash: Hash32::from_u64(0), this_height: 0, points_at_bc_block: Hash32::from_u64(1), ..Default::default() }, ..Default::default() };
        viz_state.on_screen_bfts.insert(block.block.this_hash, block);
        let block = OnScreenBft { block: BftBlock { this_hash: Hash32::from_u64(6), parent_hash: Hash32::from_u64(5), this_height: 1, points_at_bc_block: Hash32::from_u64(2), ..Default::default() }, ..Default::default() };
        viz_state.on_screen_bfts.insert(block.block.this_hash, block);
        let block = OnScreenBft { block: BftBlock { this_hash: Hash32::from_u64(7), parent_hash: Hash32::from_u64(6), this_height: 2, points_at_bc_block: Hash32::from_u64(3), ..Default::default() }, ..Default::default() };
        viz_state.on_screen_bfts.insert(block.block.this_hash, block);
    }

    viz_state
}
pub fn viz_gui_anything_happened_at_all(viz_state: &mut VizState) -> bool {
    let mut anything_happened = false;

    if viz_state.time_since_last_animation.elapsed().as_secs_f32() > 0.333 {
        anything_happened = true;
        viz_state.time_since_last_animation = Instant::now();
    }

    while let Ok(message) = viz_state.receive_from_zebra.try_recv() {
        anything_happened |= viz_state.bc_tip_height != message.bc_tip_height;
        viz_state.bc_tip_height = message.bc_tip_height;

        anything_happened |= viz_state.bft_tip_height != message.bft_tip_height;
        viz_state.bft_tip_height = message.bft_tip_height;

        anything_happened |= viz_state.bc_finalized_tip_height != message.bc_finalized_tip_height;
        viz_state.bc_finalized_tip_height = message.bc_finalized_tip_height;

        viz_state.peer_strings = message.peer_strings;

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
            viz_state.inspect_block_json_text = Some(message.json_dump_of_the_block);
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
                        let block = OnScreenBc { y: spawn_y, block: BcBlock { this_hash: *hash, parent_hash: parent.block.this_hash, txs_n: 1, this_height: parent.block.this_height + 1, is_best_chain: false, is_finalized: false, is_implicated_by_bft: true, points_at_bft_block: Hash32::from_u64(0), }, ..Default::default() };
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
                viz_state.on_screen_bfts.insert(bft.this_hash, OnScreenBft { block: bft, alpha: 0.0, y: spawn_y, ..Default::default() });
            }
        }
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

const ZOOM_FACTOR : f32 = 1.2;
const SCREEN_UNIT_CONST : f32 = 10.0;

pub(crate) fn viz_gui_draw_the_stuff_for_the_things(viz_state: &mut VizState, ui: &mut ui::Context, draw_ctx: &DrawCtx, dt: f32, input_ctx: &InputCtx) {
    if !ui.capture {
        let dxm = (input_ctx.mouse_pos().0.clamp(0, draw_ctx.window_width) - draw_ctx.window_width/2) as f32;
        let dym = (input_ctx.mouse_pos().1.clamp(0, draw_ctx.window_height) - draw_ctx.window_height/2) as f32;
        let old_screen_unit = SCREEN_UNIT_CONST * (ZOOM_FACTOR.powf(viz_state.zoom) * ui.scale);
        viz_state.zoom += input_ctx.zoom_delta as f32;
        viz_state.zoom = viz_state.zoom.min(26.0);
        let new_screen_unit = SCREEN_UNIT_CONST * (ZOOM_FACTOR.powf(viz_state.zoom) * ui.scale);
        viz_state.camera_x += (dxm / old_screen_unit) - (dxm / new_screen_unit);
        viz_state.camera_y += (dym / old_screen_unit) - (dym / new_screen_unit);
    }

    let zoom = (ZOOM_FACTOR.powf(viz_state.zoom) * ui.scale);
    // origin
    let screen_unit = SCREEN_UNIT_CONST * zoom;
    let very_zoom_out = screen_unit < 0.16;

    if !ui.capture && ui.mouse_pressed_id == ui::Id::default() && input_ctx.mouse_pressed(MouseButton::Left) {
        ui.mouse_pressed_id = ui::Id::VIZ_GUI;
    }
    if ui.mouse_pressed_id == ui::Id::VIZ_GUI && input_ctx.mouse_held(MouseButton::Left) {
        viz_state.camera_x -= input_ctx.mouse_delta().0 as f32 / screen_unit;
        viz_state.camera_y -= input_ctx.mouse_delta().1 as f32 / screen_unit;
    }

    viz_state.camera_x -= input_ctx.scroll_delta.0 as f32 / screen_unit;
    viz_state.camera_y -= input_ctx.scroll_delta.1 as f32 / screen_unit;

    let origin_x = (draw_ctx.window_width / 2) as f32 - viz_state.camera_x * screen_unit;
    let origin_y = (draw_ctx.window_height / 2) as f32 - viz_state.camera_y * screen_unit;

    let world_mouse_x = viz_state.camera_x + ((input_ctx.mouse_pos().0.clamp(0, draw_ctx.window_width) - draw_ctx.window_width/2) as f32) / screen_unit;
    let world_mouse_y = viz_state.camera_y + ((input_ctx.mouse_pos().1.clamp(0, draw_ctx.window_height) - draw_ctx.window_height/2) as f32) / screen_unit;

    {
        let sday = (viz_state.bc_finalized_tip_height) / UI_COPY_STAKING_DAY_PERIOD;
        let draw_staking_day_section = | day | {
            let y2 = -10.0 * ((day*UI_COPY_STAKING_DAY_PERIOD) as f32 - 0.5) * screen_unit;
            let y1 = -10.0 * ((day*UI_COPY_STAKING_DAY_PERIOD + UI_COPY_STAKING_DAY_WINDOW) as f32 + 0.5) * screen_unit;

            let text_height = 1.0 * screen_unit;
            let line_thickness = 1.0 * screen_unit;
            let line_width = 60.0;

            // window
            draw_ctx.rectangle_r(origin_x - line_width * screen_unit, origin_y + y1, origin_x + line_width * screen_unit, origin_y + y2, 1, 0x06ffffff);

            // start
            let start_color = 0xaa5fdc4c;
            draw_ctx.text_line(FontKind::Mono, (origin_x + (line_width + 1.0) * screen_unit), origin_y + y2, text_height, "Staking Day Start", start_color);
            draw_ctx.rectangle_r(origin_x - line_width * screen_unit, origin_y + y2, origin_x + line_width * screen_unit, origin_y + y2 + line_thickness, 1, start_color);

            // end
            let end_color = 0xaadc4c4f;
            draw_ctx.text_line(FontKind::Mono, (origin_x + (line_width + 1.0) * screen_unit), origin_y + y1, text_height, "Staking Day End", end_color);
            draw_ctx.rectangle_r(origin_x - line_width * screen_unit, origin_y + y1, origin_x + line_width * screen_unit, origin_y + y1 + line_thickness, 1, end_color);
        };

        draw_staking_day_section(sday);
        draw_staking_day_section(sday + 1);
    }

    let mut hovered_block = Hash32::from_u64(0);
    let mut hovered_block_screen_x = 0.0;
    let mut hovered_block_screen_y = 0.0;
    for on_screen_bc in viz_state.on_screen_bcs.values() {
        let dx = on_screen_bc.x - world_mouse_x;
        let dy = on_screen_bc.y - world_mouse_y;
        if (dx*dx + dy*dy).sqrt() < 1.0 {
            hovered_block = on_screen_bc.block.this_hash;
        }
    }
    for on_screen_bft in viz_state.on_screen_bfts.values() {
        let dx = on_screen_bft.x - world_mouse_x;
        let dy = on_screen_bft.y - world_mouse_y;
        if (dx*dx + dy*dy).sqrt() < 1.0 {
            hovered_block = on_screen_bft.block.this_hash;
        }
    }

    for on_screen_bc in magic(&mut viz_state.on_screen_bcs).values_mut() {
        if on_screen_bc.block.this_hash == hovered_block {
            on_screen_bc.t_roundness = 0.3;
            on_screen_bc.t_darkness = 0.2;
            if input_ctx.key_pressed(KeyCode::Space) {
                on_screen_bc.x = 0.0;
                on_screen_bc.y = 0.0;
                on_screen_bc.alpha = 0.0;
            }
            if input_ctx.mouse_pressed(MouseButton::Left) {
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
        if on_screen_bft.block.this_hash == hovered_block {
            on_screen_bft.t_roundness = 0.3;
            on_screen_bft.t_darkness = 0.2;
            if input_ctx.key_pressed(KeyCode::Space) {
                on_screen_bft.x = 0.0;
                on_screen_bft.y = 0.0;
                on_screen_bft.alpha = 0.0;
            }
            if input_ctx.mouse_pressed(MouseButton::Left) {
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
        on_screen_bc.x = e_lerp(on_screen_bc.x, on_screen_bc.t_x, dt);
        on_screen_bc.y = e_lerp(on_screen_bc.y, on_screen_bc.t_y, dt);
        on_screen_bc.roundness = e_lerp(on_screen_bc.roundness, on_screen_bc.t_roundness, dt);
        on_screen_bc.darkness = e_lerp(on_screen_bc.darkness, on_screen_bc.t_darkness, dt);
        on_screen_bc.alpha = e_lerp(on_screen_bc.alpha, on_screen_bc.t_alpha, dt);
        on_screen_bc.bft_arrow_alpha = e_lerp(on_screen_bc.bft_arrow_alpha, on_screen_bc.t_bft_arrow_alpha, dt);
        on_screen_bc.finalized_alpha = e_lerp(on_screen_bc.finalized_alpha, on_screen_bc.t_finalized_alpha, dt);
        on_screen_bc.implicated_by_bft_alpha = e_lerp(on_screen_bc.implicated_by_bft_alpha, on_screen_bc.t_implicated_by_bft_alpha, dt);
    }
    for on_screen_bft in viz_state.on_screen_bfts.values_mut() {
        on_screen_bft.x = e_lerp(on_screen_bft.x, on_screen_bft.t_x, dt);
        on_screen_bft.y = e_lerp(on_screen_bft.y, on_screen_bft.t_y, dt);
        on_screen_bft.roundness = e_lerp(on_screen_bft.roundness, on_screen_bft.t_roundness, dt);
        on_screen_bft.darkness = e_lerp(on_screen_bft.darkness, on_screen_bft.t_darkness, dt);
        on_screen_bft.alpha = e_lerp(on_screen_bft.alpha, on_screen_bft.t_alpha, dt);
    }

    //draw_ctx.circle(origin_x as f32, origin_y as f32, (screen_unit/2.0) as f32, 0xff_0000bb);

    let arrow_and_line_width = screen_unit / 12.0;

    for on_screen_bc in viz_state.on_screen_bcs.values() {
        let x = on_screen_bc.x;
        let y = on_screen_bc.y;
        let finalized = on_screen_bc.block.is_best_chain && on_screen_bc.block.this_height <= viz_state.bc_finalized_tip_height;
        let base_color = if on_screen_bc.block.is_best_chain {
            if viz_state.ui_hovered_height.is_some() && on_screen_bc.block.this_height == viz_state.ui_hovered_height.unwrap().0 as u64 {
                COLOR_BRIGHT
            } else {
                COLOR_BC
            }
        } else {
            COLOR_NBC
        };

        let color        = (((on_screen_bc.alpha*255.0)                                      as u32) << 24) | blend_u32(0x000000, base_color,   ((1.0 - on_screen_bc.darkness) * 255.0) as u32);
        let color_accent = (((on_screen_bc.alpha*on_screen_bc.finalized_alpha*255.0)         as u32) << 24) | blend_u32(0x000000, COLOR_ACCENT, ((1.0 - on_screen_bc.darkness) * 255.0) as u32);
        let color_bft    = (((on_screen_bc.alpha*on_screen_bc.implicated_by_bft_alpha*255.0) as u32) << 24) | blend_u32(0x000000, COLOR_BFT,    ((1.0 - on_screen_bc.darkness) * 255.0) as u32);

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
            let rad = 0.5*screen_unit * (on_screen_bc.block.txs_n as f32).log2().max(0.5);

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
        }

        if very_zoom_out == false {
            let here_text_y = (origin_y + (y - 0.5)*screen_unit);
            if here_text_y <= draw_ctx.window_height as f32 && here_text_y + screen_unit >= 0.0 {
                if on_screen_bc.block.is_best_chain {
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
                }
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
                draw_ctx.arrow(
                    origin_x + (x + dx * 2.0) * screen_unit,
                    origin_y + (y + dy * 2.0) * screen_unit,
                    origin_x + (x + dx * (l - 2.0))*screen_unit,
                    origin_y + (y + dy * (l - 2.0) )*screen_unit,
                    arrow_and_line_width, COLOR_BFT_LINK | (((on_screen_bc.alpha*on_screen_bc.bft_arrow_alpha*255.0) as u32) << 24),
                );
            }
        }
    }

    for on_screen_bft in viz_state.on_screen_bfts.values() {
        let x = on_screen_bft.x;
        let y = on_screen_bft.y;
        let color = (((on_screen_bft.alpha*255.0) as u32) << 24) | blend_u32(0x000000, COLOR_BFT, ((1.0 - on_screen_bft.darkness) * 255.0) as u32);

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

    if viz_state.last_frame_hovered_hash != hovered_block {
        viz_state.last_frame_hovered_hash = hovered_block;
        if hovered_block != Hash32::from_u64(0)
        { play_sound(SOUND_UI_HOVER, 0.5, 1.0); }
    }

    if !ui.capture && input_ctx.mouse_pressed(MouseButton::Left) {
        viz_state.inspecting_block_hash = hovered_block;
        viz_state.inspecting_block_screen_x = hovered_block_screen_x;
        viz_state.inspecting_block_screen_y = hovered_block_screen_y;
        viz_state.inspect_block_json_text = None;
    }
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
