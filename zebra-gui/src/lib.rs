#![allow(unsafe_code)]
#![allow(warnings)]

mod style;
use style::*;

mod ui;
use ui::*;

mod fontello_icons;
use fontello_icons::*;

use std::sync::{Arc, Mutex};
use wallet;

mod viz_gui;
pub use viz_gui::*;

const TURN_OFF_HASH_BASED_LAZY_RENDER: usize = 0;

#[derive(Clone, Copy, PartialOrd, PartialEq, Ord, Eq, Debug, Default)]
#[repr(u8)]
pub enum InteractiveVizOp {
    #[default]
    None,

    // on PoW
    LF,
    // fin,
    candidate,

    // on PoS
    bft_last_final,
    origbft_last_final,
    snapshot,
    tip,
}
impl InteractiveVizOp {
    pub fn valid_for_bc(&self) -> bool {
        InteractiveVizOp::LF <= *self && *self <= InteractiveVizOp::candidate
    }
    pub fn valid_for_bft(&self) -> bool {
        InteractiveVizOp::bft_last_final <= *self && *self <= InteractiveVizOp::tip
    }
    pub fn cycle_next(&self) -> Self {
        match self {
            InteractiveVizOp::None           => InteractiveVizOp::LF,
            InteractiveVizOp::LF             => InteractiveVizOp::bft_last_final,
            InteractiveVizOp::bft_last_final => InteractiveVizOp::snapshot,
            InteractiveVizOp::snapshot       => InteractiveVizOp::candidate,
            InteractiveVizOp::candidate      => InteractiveVizOp::tip,
            _ => InteractiveVizOp::None,
        }
    }
}


#[allow(unused_imports)]
use std::{
    alloc::{alloc, dealloc, Layout},
    cell::RefCell,
    hash::Hasher,
    hint::spin_loop,
    mem::{swap, transmute, MaybeUninit},
    ptr::{copy_nonoverlapping, slice_from_raw_parts},
    rc::Rc,
    sync::{
        atomic::{AtomicU32, Ordering, AtomicBool},
        Barrier,
    },
    time::{Duration, Instant},
    u32,
};
use winit::{dpi::Size, keyboard::KeyCode};

use rustybuzz::{shape, Face as RbFace, UnicodeBuffer};
#[allow(unused_imports)]
use swash::{scale::ScaleContext, text, FontRef};


pub const UI_COPY_STAKING_ACTION_DELAY_BLOCKS: u64 = 75;
pub const UI_COPY_STAKING_DAY_PERIOD: u64 = 150;
pub const UI_COPY_STAKING_DAY_WINDOW: u64 = 70;

const RENDER_TILE_SHIFT: usize = 8;
const RENDER_TILE_SIZE: usize = 1 << RENDER_TILE_SHIFT;
const RENDER_TILE_INTRA_MASK: usize = RENDER_TILE_SIZE.wrapping_sub(1);

struct ThreadContext {
    wake_up_gate: AtomicU32,
    workers_that_have_passed_the_wake_up_gate: AtomicU32,
    wake_up_barrier: Barrier,
    is_last_time: bool,
    thread_count: u32,
    begin_work_gate: AtomicU32,
    workers_at_job_site: AtomicU32,
    work_unit_count: u32,
    work_unit_take: AtomicU32,
    work_unit_complete: AtomicU32,
    work_user_pointer: usize,
    work_user_function: WideWorkFn,
}
type WideWorkFn = fn(thread_id: usize, work_id: usize, work_count: usize, user_pointer: usize);

fn worker_thread_loop(thread_id: usize, p_thread_context: usize) {
    unsafe {
        let p_thread_context = p_thread_context as *mut ThreadContext;
        // let thread_count = (*p_thread_context).thread_count;
        loop {
            (*p_thread_context).wake_up_barrier.wait();
            while (*p_thread_context).wake_up_gate.load(Ordering::Acquire) == 0 { spin_loop(); }
            (*p_thread_context).workers_that_have_passed_the_wake_up_gate.fetch_add(1, Ordering::Relaxed);

            loop {
                while (*p_thread_context).begin_work_gate.load(Ordering::Relaxed) == 0 { spin_loop(); }
                (*p_thread_context).workers_at_job_site.fetch_add(1, Ordering::Relaxed);
                if (*p_thread_context).begin_work_gate.load(Ordering::Acquire) == 0 {
                    // abort, exit job site and await orders again.
                    (*p_thread_context).workers_at_job_site.fetch_sub(1, Ordering::Release);
                    continue;
                }

                let is_last_time       = (*p_thread_context).is_last_time;
                let work_count         = (*p_thread_context).work_unit_count as usize;
                let work_user_pointer  = (*p_thread_context).work_user_pointer;
                let work_user_function = (*p_thread_context).work_user_function;
                loop {
                    let work_id = (*p_thread_context).work_unit_take.fetch_add(1, Ordering::Relaxed) as usize;
                    if work_id < work_count {
                        work_user_function(thread_id, work_id, work_count, work_user_pointer);

                        (*p_thread_context).work_unit_complete.fetch_add(1, Ordering::Release);
                    } else {
                        break;
                    }
                }
                (*p_thread_context).workers_at_job_site.fetch_sub(1, Ordering::Relaxed);
                if is_last_time { break; }
            }
        }
    }
}

// barrier()

// while (work_a_left)
// {
//     work_a()
// }

// while (work_b_left)
// {
//     work_a()
// }

// barrier()


fn dennis_parallel_for(p_thread_context: *mut ThreadContext, is_last_time: bool, work_count: usize, work_user_pointer: usize, work_user_function: WideWorkFn) {
    unsafe {
        // let thread_count = (*p_thread_context).thread_count;

        // Begin work gate should be closed.
        // We wait for the job site to be clear.
        while (*p_thread_context).workers_at_job_site.load(Ordering::Relaxed) != 0 { spin_loop(); }

        (*p_thread_context).is_last_time = is_last_time;
        (*p_thread_context).work_unit_count = work_count as u32;
        (*p_thread_context).work_unit_take.store(0, Ordering::Relaxed);
        (*p_thread_context).work_unit_complete.store(0, Ordering::Relaxed);
        (*p_thread_context).work_user_pointer = work_user_pointer;
        (*p_thread_context).work_user_function = work_user_function;

        // Open the gate and let workers descend.
        (*p_thread_context).begin_work_gate.store(1, Ordering::Release);
        // We could increment job site workers here but we are the only one who reads it so we will not.

        loop {
            let work_id = (*p_thread_context).work_unit_take.fetch_add(1, Ordering::Relaxed) as usize;
            if work_id < work_count {
                work_user_function(0, work_id, work_count, work_user_pointer);

                (*p_thread_context).work_unit_complete.fetch_add(1, Ordering::Release);
            } else {
                break;
            }
        }
        // All work is already done or in progress by someone so we can close the gate for new workers.
        // But, if it is the last time we need to leave it open in order to let workers see that and go to sleep.
        if is_last_time == false
        { (*p_thread_context).begin_work_gate.store(0, Ordering::Relaxed); }

        while (*p_thread_context).work_unit_complete.load(Ordering::Acquire) < work_count as u32 { spin_loop(); }
    }
}

use std::collections::HashMap;

// Note(Giovanni): Cache key for text measurement.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct MeasureKey {
    pub font_kind: FontKind,
    pub text_height: usize, // We use the floored usize
    pub text_content: String,
}

const MEASURE_CACHE_MAX_CAPACITY: usize = 4096;

#[derive(Clone, PartialEq, Eq, Hash)]
pub struct TextShapeKey {
    pub font_kind: FontKind,
    pub text_height: usize,
    pub text_content: String,
}

const TEXT_SHAPE_CACHE_MAX_CAPACITY: usize = 8192;

impl DrawCtx {

    pub fn _init_font_tracker(&self, ttf_file: &'static [u8], target_px_height: usize, font_kind: FontKind, fudge_to_px_height: usize) -> *mut FontTracker {
        unsafe {
            let swash_font = FontRef::from_index(ttf_file, 0).expect("font ref");

            let new_font_tracker_ptr = self.font_tracker_buffer.add(*self.font_tracker_count);
            *self.font_tracker_count += 1;

            let units_per_em: f32;
            let ppem;
            let glyph_row_shift: usize;
            let baseline_y: usize;
            let max_glyph_count;
            {
                let m = swash_font.metrics(&[]);
                units_per_em = m.units_per_em as f32;
                ppem = (if fudge_to_px_height == usize::MAX { target_px_height } else { fudge_to_px_height }) as f32 / ((m.ascent + m.descent + m.leading) / units_per_em);
                glyph_row_shift = (((m.max_width / units_per_em) * ppem).ceil() as usize).next_power_of_two().trailing_zeros() as usize;
                baseline_y = ((m.descent / units_per_em) * ppem).ceil() as usize;
                max_glyph_count = m.glyph_count;
            }
            let mut new_tracker = FontTracker {
                how_many_times_was_i_used: 0,
                font_kind,
                ttf_file,
                cached_rusty_buzz: RbFace::from_slice(ttf_file, 0).expect("bad font"),
                target_px_height,
                fudge_to_px_height,
                units_per_em,
                ppem,
                glyph_row_shift,
                baseline_y,
                row_buffers: Vec::new(),
                cached_bitmaps_counter: 0,
                cached_bitmap_widths: Vec::new(),
                glyph_to_bitmap_index: Vec::with_capacity(max_glyph_count as usize),
            };
            let actual_height = if fudge_to_px_height == usize::MAX { target_px_height } else { fudge_to_px_height };
            for _ in 0..actual_height { new_tracker.row_buffers.push(Vec::new()); }
            for _ in 0..max_glyph_count { new_tracker.glyph_to_bitmap_index.push(u16::MAX); }
            copy_nonoverlapping(&new_tracker as *const FontTracker, new_font_tracker_ptr, 1);
            std::mem::forget(new_tracker);
            new_font_tracker_ptr
        }
    }

    pub fn _find_or_create_font_tracker<'a>(&self,
                                            target_px_height: usize,
                                            font_kind: FontKind) -> (&'a mut FontTracker, usize) {
        unsafe {
            let mut found_font = std::ptr::null_mut();
            for i in 0..*self.font_tracker_count {
                let check = self.font_tracker_buffer.add(i);
                if (*check).target_px_height == target_px_height as usize && (*check).font_kind == font_kind {
                    found_font = check;
                    break;
                }
            }
            if found_font == std::ptr::null_mut() {
                match font_kind {
                FontKind::Mono => {
                    found_font = self._init_font_tracker(DEJA_VU_SANS_MONO, target_px_height, font_kind, usize::MAX);
                },
                FontKind::Normal => {
                    found_font = self._init_font_tracker(INTER, target_px_height, font_kind, usize::MAX);
                },
                FontKind::Icons => {
                    found_font = self._init_font_tracker(ICONS, target_px_height, font_kind, usize::MAX);
                },
                }
            }
            let tracker: &mut FontTracker = &mut *found_font;
            let tracker_id = (found_font as usize - self.font_tracker_buffer as usize) / size_of::<FontTracker>();
            (tracker, tracker_id)
        }
    }

    pub fn _render_glyph_if_not_cached(tracker: &mut FontTracker, glyph_id: u16) {
        unsafe {
            if tracker.glyph_to_bitmap_index[glyph_id as usize] == u16::MAX
            {
                let swash_font = FontRef::from_index(tracker.ttf_file, 0).expect("font ref");

                let bitmap_index = tracker.cached_bitmaps_counter;
                tracker.cached_bitmaps_counter += 1;
                tracker.glyph_to_bitmap_index[glyph_id as usize] = bitmap_index;

                let mut _scale = ScaleContext::new();
                let mut scale = _scale.builder(swash_font).size(tracker.ppem).hint(false).build();

                let image = swash::scale::Render::new(&[
                    swash::scale::Source::Bitmap(swash::scale::StrikeWith::BestFit),
                    swash::scale::Source::Outline,
                ])
                .format(swash::zeno::Format::Alpha)
                .render(&mut scale, glyph_id as u16).unwrap();
                assert_eq!(image.content, swash::scale::image::Content::Mask);

                tracker.cached_bitmap_widths.push((image.placement.width as isize + image.placement.left.max(0) as isize) as u16);

                let actual_height = if tracker.fudge_to_px_height == usize::MAX { tracker.target_px_height } else { tracker.fudge_to_px_height };
                for y in 0..actual_height {
                    let row_len = 1usize << tracker.glyph_row_shift;
                    let tracker_buffer_old_len = tracker.row_buffers[y].len();
                    tracker.row_buffers[y].reserve(row_len);
                    tracker.row_buffers[y].set_len(tracker_buffer_old_len + row_len);

                    let row_put: *mut u8 = tracker.row_buffers[y].as_mut_ptr().add(tracker_buffer_old_len);

                    let cy = (y as i32 - actual_height as i32 + image.placement.top + tracker.baseline_y as i32) as usize;
                    std::ptr::write_bytes(row_put, 0, row_len);
                    if (cy as u32) < image.placement.height {
                        std::ptr::copy_nonoverlapping(&image.data[image.placement.width as usize * cy], row_put.add(image.placement.left.max(0) as usize & row_len.wrapping_sub(1)), image.placement.width as usize);
                    }
                }
            }
        }
    }

    pub fn measure_text_line_is_rectangle(&self, font_kind: FontKind, text_height: f32, text_line: &str) -> bool {
        if text_height <= 0.0 || text_height.is_normal() == false { return true; }
        let text_height = text_height.min(8192.0);

        if text_height < 3.0 {
            return true;
        }
        return false;
    }
    
    pub fn measure_text_line(&mut self, font_kind: FontKind, text_height: f32, text_line: &str) -> f32 {
        if text_height <= 0.0 || !text_height.is_normal() { return 0.0; }
        let text_height_norm = text_height.min(8192.0);

        if text_height_norm < 3.0 {
            let factor = match font_kind {
                FontKind::Normal => 1.0,
                FontKind::Mono   => 4.0,
                FontKind::Icons  => 2.0,
            };
            return (factor * text_height_norm * text_line.len() as f32) / 3.0;
        }

        let text_height_px = text_height_norm.floor() as usize;

        let shape_key = TextShapeKey {
            font_kind,
            text_height: text_height_px,
            text_content: text_line.to_string(),
        };
        if let Some(glyph_run) = self.text_shape_cache.borrow().get(&shape_key) {
            return glyph_run.iter().map(|(_, adv)| *adv as usize).sum::<usize>() as f32;
        }

        // Note(Giovanni): Cache Lookup
        let cache_key = MeasureKey {
            font_kind,
            text_height: text_height_px,
            text_content: text_line.to_string(),
        };
        if let Some(&width) = self.measure_cache.get(&cache_key) {
            return width;
        }

        let (tracker, _) = self._find_or_create_font_tracker(text_height_px, font_kind);
        tracker.how_many_times_was_i_used += 1;

        let mut buf = UnicodeBuffer::new();
        buf.set_direction(rustybuzz::Direction::LeftToRight);
        buf.push_str(text_line);

        let shaped = shape(&tracker.cached_rusty_buzz, &[], buf);
        let infos = shaped.glyph_infos();
        let poss = shaped.glyph_positions();
        let mut glyph_run = Vec::<(u16, u16)>::with_capacity(infos.len());
        let mut total_width = 0usize;
        for i in 0..infos.len() {
            let g_info = &infos[i];
            let g_pos = &poss[i];
            let px_advance = ((g_pos.x_advance as f32 / tracker.units_per_em) * tracker.ppem).ceil() as usize;
            let px_advance = px_advance.min(1usize << tracker.glyph_row_shift);
            total_width += px_advance;
            glyph_run.push((g_info.glyph_id as u16, px_advance as u16));
        }
        let total_width = total_width as f32;

        {
            let mut text_shape_cache = self.text_shape_cache.borrow_mut();
            if text_shape_cache.len() >= TEXT_SHAPE_CACHE_MAX_CAPACITY {
                if let Some(key) = text_shape_cache.keys().next().cloned() {
                    text_shape_cache.remove(&key);
                }
            }
            text_shape_cache.insert(shape_key, glyph_run);
        }

        // Note(Giovanni): Memoize and Return
        if self.measure_cache.len() >= MEASURE_CACHE_MAX_CAPACITY {
            if let Some(key) = self.measure_cache.keys().next().cloned() {
                self.measure_cache.remove(&key);
            }
        }
        self.measure_cache.insert(cache_key, total_width);
        
        total_width
    }
    pub fn text_line(&self, font_kind: FontKind, text_x: f32, text_y: f32, text_height: f32, text_line: &str, color: u32) {
        if text_y + text_height <= 0.0 || text_y >= self.window_height as f32 { return; }

        unsafe {
            if text_height <= 0.0 || text_height.is_normal() == false { return; }
            let text_height = text_height.min(8192.0);

            if text_height < 3.0 {
                let factor = match font_kind {
                    FontKind::Normal => 1.0,
                    FontKind::Mono   => 4.0,
                    FontKind::Icons  => 2.0, // TODO: idk??
                };
                self.rectangle(text_x, text_y, text_x + (factor * text_height * text_line.len() as f32) / 3.0, text_y + text_height, (color&0xFFffFF) | ((color >> 26) << 24));
                return;
            }

            let text_x = text_x.floor() as isize;
            let text_y = text_y.floor() as isize;
            let text_height = text_height.floor() as usize;
            if text_y + text_height as isize <= 0 || text_y >= self.window_height { return; }

            let (tracker, tracker_id) = self._find_or_create_font_tracker(text_height, font_kind);
            tracker.how_many_times_was_i_used += 1;
            let shape_key = TextShapeKey {
                font_kind,
                text_height,
                text_content: text_line.to_string(),
            };
            {
                let mut text_shape_cache = self.text_shape_cache.borrow_mut();
                if !text_shape_cache.contains_key(&shape_key) {
                    let mut buf = UnicodeBuffer::new();
                    buf.set_direction(rustybuzz::Direction::LeftToRight);
                    buf.push_str(text_line);
                    let shaped = shape(&tracker.cached_rusty_buzz, &[], buf);
                    let infos = shaped.glyph_infos();
                    let poss = shaped.glyph_positions();
                    let mut glyph_run = Vec::<(u16, u16)>::with_capacity(infos.len());
                    for i in 0..infos.len() {
                        let g_info = &infos[i];
                        let g_pos = &poss[i];
                        let px_advance = (((g_pos.x_advance as f32 / tracker.units_per_em) * tracker.ppem).ceil() as usize)
                            .min(1usize << tracker.glyph_row_shift);
                        glyph_run.push((g_info.glyph_id as u16, px_advance as u16));
                    }
                    if text_shape_cache.len() >= TEXT_SHAPE_CACHE_MAX_CAPACITY {
                        if let Some(key) = text_shape_cache.keys().next().cloned() {
                            text_shape_cache.remove(&key);
                        }
                    }
                    text_shape_cache.insert(shape_key.clone(), glyph_run);
                }
            }
            let text_shape_cache = self.text_shape_cache.borrow();
            let glyph_run = text_shape_cache.get(&shape_key).unwrap();

            let glyph_bitmap_run_start = self.glyph_bitmap_run_allocator.add(*self.glyph_bitmap_run_allocator_position);
            let mut glyph_bitmap_run_count = 0usize;

            let mut acc_x = text_x;
            for (glyph_id, px_advance) in glyph_run.iter() {
                let px_advance = *px_advance as usize;

                // TODO: Scissor?
                if (acc_x + px_advance as isize) <= 0 { acc_x += px_advance as isize; continue; }
                if acc_x > self.window_width { break; }

                Self::_render_glyph_if_not_cached(tracker, *glyph_id);

                *glyph_bitmap_run_start.add(glyph_bitmap_run_count) = (tracker.glyph_to_bitmap_index[*glyph_id as usize], acc_x as i16);
                glyph_bitmap_run_count += 1;
                acc_x += px_advance as isize;

                if *self.glyph_bitmap_run_allocator_position + glyph_bitmap_run_count >= GLYPH_RUN_MAX { println!("WARNING, overflowing GLYPH_RUN_MAX."); return; }
            }

            *self.glyph_bitmap_run_allocator_position += glyph_bitmap_run_count;
            assert!(*self.glyph_bitmap_run_allocator_position < GLYPH_RUN_MAX);

            let actual_height = if tracker.fudge_to_px_height == usize::MAX { tracker.target_px_height } else { tracker.fudge_to_px_height };
            for y in 0..actual_height {
                let screen_y = y as isize + text_y;
                if screen_y >= 0 && screen_y < self.window_height && *self.draw_command_count + 1 <= DRAW_CALL_MAX {
                    *self.draw_command_buffer.add(*self.draw_command_count) = DrawCommand::TextRow {
                        y: screen_y as u16,
                        glyph_row_shift: tracker.glyph_row_shift as u8,
                        color,
                        font_tracker_id: tracker_id as u16,
                        font_row_index: y as u16,
                        glyph_bitmap_run: glyph_bitmap_run_start,
                        glyph_bitmap_run_len: glyph_bitmap_run_count,
                    };
                    *self.draw_command_count += 1;
                }
            }
        }
    }

    pub fn set_scissor(&self, x1: isize, y1: isize, x2: isize, y2: isize) {
        unsafe {
            if *self.draw_command_count + 1 <= DRAW_CALL_MAX {
                let put = self.draw_command_buffer.add(*self.draw_command_count);
                *self.draw_command_count += 1;
                *put = DrawCommand::Scissor {
                    x1: x1.max(0),
                    x2: x2.min(self.window_width),
                    y1: y1.max(0),
                    y2: y2.min(self.window_height),
                };
            }
        }
    }
    pub fn clear_scissor(&self) {
        self.set_scissor(0, 0, self.window_width, self.window_height);
    }

    pub fn rectangle(&self, x1: f32, y1: f32, x2: f32, y2: f32, color: u32) {
        unsafe {
            if *self.draw_command_count + 1 <= DRAW_CALL_MAX {
                let put = self.draw_command_buffer.add(*self.draw_command_count);
                *self.draw_command_count += 1;
                *put = DrawCommand::RoundedRectangle {
                    x: x1,
                    x2: x2,
                    y: y1,
                    y2: y2,
                    radius_tl: 0,
                    radius_tr: 0,
                    radius_bl: 0,
                    radius_br: 0,
                    color
                };
            }
        }
    }

    pub fn rectangle_r(&self, x1: f32, y1: f32, x2: f32, y2: f32, radius: isize, color: u32) {
        unsafe {
            if *self.draw_command_count + 1 <= DRAW_CALL_MAX {
                let put = self.draw_command_buffer.add(*self.draw_command_count);
                *self.draw_command_count += 1;
                *put = DrawCommand::RoundedRectangle {
                    x: x1,
                    x2: x2,
                    y: y1,
                    y2: y2,
                    radius_tl: radius,
                    radius_tr: radius,
                    radius_bl: radius,
                    radius_br: radius,
                    color
                };
            }
        }
    }

    // (x1, y1) and (x2, y2) can be in any order
    pub fn line(&self, mut x1: f32, mut y1: f32, mut x2: f32, mut y2: f32, thickness: f32, color: u32) {
        if (x1.max(x2) + thickness < 0.0) || (x1.min(x2) - thickness > self.window_width as f32) { return; }
        if (y1.max(y2) + thickness < 0.0) || (y1.min(y2) - thickness > self.window_height as f32) { return; }

        unsafe {
            if *self.draw_command_count + 1 <= DRAW_CALL_MAX {
                let put = self.draw_command_buffer.add(*self.draw_command_count);
                *self.draw_command_count += 1;
                let thickness = if thickness < 0.0 || thickness.is_normal() == false { 0.0 } else { thickness };

                if (x2 - x1).abs() >= (y2 - y1).abs() {
                    if x2 < x1 { swap(&mut x1, &mut x2); swap(&mut y1, &mut y2); }
                    *put = DrawCommand::PixelLineXDef { x1, x2, y1, y2, color: color, thickness: thickness };
                } else {
                    if y2 < y1 { swap(&mut y1, &mut y2); swap(&mut x1, &mut x2); }
                    *put = DrawCommand::PixelLineYDef { x1, x2, y1, y2, color: color, thickness: thickness };
                }
            }
        }
    }

    // (x1, y1) and (x2, y2) can be in any order
    pub fn arrow(&self, x1: f32, y1: f32, x2: f32, y2: f32, thickness: f32, color: u32) {
        let thickness = if thickness < 0.0 || thickness.is_normal() == false { 0.0 } else { thickness };

        self.line(x1, y1, x2, y2, thickness, color);
        let (fx, fy) = {
            let (x, y) = ((x1 - x2) as f32, (y1 - y2) as f32);
            let c = std::f32::consts::FRAC_1_SQRT_2; // 1/sqrt(2) = cos(45°) = sin(45°)

            // x' = c*(x - y)
            // y' = c*(x + y)
            let xr = c * (x - y);
            let yr = c * (x + y);

            let len = (xr * xr + yr * yr).sqrt();
            if len == 0.0 {
                (0.0, 0.0)
            } else {
                (xr / len, yr / len)
            }
        };
        self.line(x2, y2, x2 + (fx * thickness * 3.0), y2 + (fy * thickness * 3.0), thickness, color);
        self.line(x2, y2, x2 + (fy * thickness * 3.0), y2 + (-fx * thickness * 3.0), thickness, color);
    }

    pub fn rounded_rectangle(&self, x1: isize, y1: isize, x2: isize, y2: isize, radius_tl: isize, radius_tr: isize, radius_bl: isize, radius_br: isize, color: u32) {
        if x2 < 0 || x1 > self.window_width { return; }
        if y2 < 0 || y1 > self.window_height { return; }

        unsafe {
            if *self.draw_command_count + 1 <= DRAW_CALL_MAX {
                let put = self.draw_command_buffer.add(*self.draw_command_count);
                *self.draw_command_count += 1;
                *put = DrawCommand::RoundedRectangle {
                    x: x1 as f32,
                    x2: x2 as f32,
                    y: y1 as f32,
                    y2: y2 as f32,
                    radius_tl,
                    radius_tr,
                    radius_bl,
                    radius_br,
                    color
                };
            }
        }
    }

    pub fn circle(&self, x: f32, y: f32, radius: f32, color: u32) {
        if x + radius < 0.0 || x - radius > self.window_width as f32 { return; }
        if y + radius < 0.0 || y - radius > self.window_height as f32 { return; }

        unsafe {
            if *self.draw_command_count + 1 <= DRAW_CALL_MAX {
                let put = self.draw_command_buffer.add(*self.draw_command_count);
                *self.draw_command_count += 1;
                *put = DrawCommand::RoundedRectangle {
                    x: x - radius,
                    x2: x + radius,
                    y: y - radius,
                    y2: y + radius,
                    radius_tl: radius as isize,
                    radius_tr: radius as isize,
                    radius_bl: radius as isize,
                    radius_br: radius as isize,
                    color
                };
            }
        }
    }
    pub fn circle_square(&self, x: f32, y: f32, radius: f32, round_pixels: f32, color: u32) {
        if x + radius < 0.0 || x - radius > self.window_width as f32 { return; }
        if y + radius < 0.0 || y - radius > self.window_height as f32 { return; }

        unsafe {
            if *self.draw_command_count + 1 <= DRAW_CALL_MAX {
                let put = self.draw_command_buffer.add(*self.draw_command_count);
                *self.draw_command_count += 1;
                *put = DrawCommand::RoundedRectangle {
                    x: x - radius,
                    x2: x + radius,
                    y: y - radius,
                    y2: y + radius,
                    radius_tl: round_pixels as isize,
                    radius_tr: round_pixels as isize,
                    radius_bl: round_pixels as isize,
                    radius_br: round_pixels as isize,
                    color
                };
            }
        }
    }
}


#[derive(Debug, Default, Clone)]
struct InputCtx {
    mouse_moved:                 bool,
    should_process_mouse_events: bool,

    inflight_mouse_events:    Vec::<(winit::event::MouseButton, winit::event::ElementState)>,
    inflight_keyboard_events: Vec::<(winit::keyboard::KeyCode,  winit::event::ElementState)>,
    inflight_text_input:      Vec<char>,

    this_mouse_pos: (isize, isize),
    last_mouse_pos: (isize, isize),
    scroll_delta: (f64, f64),
    zoom_delta: f64,

    mouse_down:     usize,
    mouse_pressed:  usize,
    mouse_released: usize,
    keys_down1:      u128,
    keys_pressed1:   u128,
    keys_released1:  u128,
    keys_down2:      u128,
    keys_pressed2:   u128,
    keys_released2:  u128,
    text_input:     Option<Vec<char>>,
}

const MOUSE_LEFT:   usize = 1 << 0;
const MOUSE_MIDDLE: usize = 1 << 1;
const MOUSE_RIGHT:  usize = 1 << 2;

impl InputCtx {
    fn key_pressed(&self, key: winit::keyboard::KeyCode) -> bool {
        if let Some(key) = 1u128.checked_shl(key as u32) {
            return self.keys_pressed1 & key == key;
        }
        else if let Some(key) = 1u128.checked_shl(key as u32 - 128) {
            return self.keys_pressed2 & key == key;
        }
        else {
            return false;
        }
    }

    fn key_held(&self, key: winit::keyboard::KeyCode) -> bool {
        if let Some(key) = 1u128.checked_shl(key as u32) {
            return self.keys_down1 & key == key;
        }
        else if let Some(key) = 1u128.checked_shl(key as u32 - 128) {
            return self.keys_down2 & key == key;
        }
        else {
            return false;
        }
    }

    fn key_released(&self, key: winit::keyboard::KeyCode) -> bool {
        if let Some(key) = 1u128.checked_shl(key as u32) {
            return self.keys_released1 & key == key;
        }
        else if let Some(key) = 1u128.checked_shl(key as u32 - 128) {
            return self.keys_released2 & key == key;
        }
        else {
            return false;
        }
    }

    fn mouse_pos(&self) -> (isize, isize) {
        return self.this_mouse_pos;
    }

    fn mouse_delta(&self) -> (isize, isize) {
        return (self.this_mouse_pos.0 - self.last_mouse_pos.0, self.this_mouse_pos.1 - self.last_mouse_pos.1);
    }

    fn mouse_pressed(&self, button: winit::event::MouseButton) -> bool {
        let button = match button {
            winit::event::MouseButton::Left   => MOUSE_LEFT,
            winit::event::MouseButton::Middle => MOUSE_MIDDLE,
            winit::event::MouseButton::Right  => MOUSE_RIGHT,
            _ => 0,
        };

        return (self.mouse_pressed & button) != 0;
    }

    fn mouse_held(&self, button: winit::event::MouseButton) -> bool {
        let button = match button {
            winit::event::MouseButton::Left   => MOUSE_LEFT,
            winit::event::MouseButton::Middle => MOUSE_MIDDLE,
            winit::event::MouseButton::Right  => MOUSE_RIGHT,
            _ => 0,
        };

        return (self.mouse_down & button) != 0;
    }

    fn mouse_released(&self, button: winit::event::MouseButton) -> bool {
        let button = match button {
            winit::event::MouseButton::Left   => MOUSE_LEFT,
            winit::event::MouseButton::Middle => MOUSE_MIDDLE,
            winit::event::MouseButton::Right  => MOUSE_RIGHT,
            _ => 0,
        };

        return (self.mouse_released & button) != 0;
    }

    fn get_from_clipboard(&self) -> String {
        // Try xclip first (works reliably on X11), then xsel, then copypasta.
        if let Ok(output) = std::process::Command::new("xclip").args(["-selection", "clipboard", "-o"]).output() {
            if output.status.success() {
                return String::from_utf8_lossy(&output.stdout).into_owned();
            }
        }
        if let Ok(output) = std::process::Command::new("xsel").args(["--clipboard", "--output"]).output() {
            if output.status.success() {
                return String::from_utf8_lossy(&output.stdout).into_owned();
            }
        }
        use copypasta::{ClipboardContext, ClipboardProvider};
        let Ok(mut clipboard) = ClipboardContext::new() else {
            return String::new();
        };
        let Ok(text) = clipboard.get_contents() else {
            return String::new();
        };
        return text;
    }

    fn send_to_clipboard(&self, text: &str) -> bool {
        // Try xclip first (works reliably on X11), then xsel, then copypasta.
        if let Ok(mut child) = std::process::Command::new("xclip").args(["-selection", "clipboard"]).stdin(std::process::Stdio::piped()).spawn() {
            if let Some(mut stdin) = child.stdin.take() {
                use std::io::Write;
                let _ = stdin.write_all(text.as_bytes());
            }
            if let Ok(status) = child.wait() {
                if status.success() { return true; }
            }
        }
        if let Ok(mut child) = std::process::Command::new("xsel").args(["--clipboard", "--input"]).stdin(std::process::Stdio::piped()).spawn() {
            if let Some(mut stdin) = child.stdin.take() {
                use std::io::Write;
                let _ = stdin.write_all(text.as_bytes());
            }
            if let Ok(status) = child.wait() {
                if status.success() { return true; }
            }
        }
        use copypasta::{ClipboardContext, ClipboardProvider};
        let Ok(mut clipboard) = ClipboardContext::new() else {
            return false;
        };
        clipboard.set_contents(text.to_owned()).is_ok()
    }
}

#[derive(Debug, Default, Copy, Clone, PartialEq, Eq, Hash)] pub enum FontKind { #[default] Normal, Mono, Icons }
use FontKind::{Mono, Icons};

struct FontTracker {
    how_many_times_was_i_used: usize,
    font_kind: FontKind,
    target_px_height: usize,
    fudge_to_px_height: usize,
    ttf_file: &'static [u8],
    cached_rusty_buzz: RbFace<'static>,
    units_per_em: f32,
    ppem: f32,
    glyph_row_shift: usize,
    baseline_y: usize,
    row_buffers: Vec<Vec<u8>>,
    cached_bitmaps_counter: u16,
    cached_bitmap_widths: Vec<u16>,
    glyph_to_bitmap_index: Vec<u16>,
}

struct DrawCtx {
    window_width: isize,
    window_height: isize,
    draw_command_buffer: *mut DrawCommand,
    draw_command_count: *mut usize,
    glyph_bitmap_run_allocator: *mut (u16, i16),
    glyph_bitmap_run_allocator_position: *mut usize,
    font_tracker_buffer: *mut FontTracker,
    font_tracker_count: *mut usize,
    debug_pixel_inspector: *mut Option<(usize, usize)>,
    debug_pixel_inspector_last_color: *mut u32,
    measure_cache: HashMap<MeasureKey, f32>,
    text_shape_cache: RefCell<HashMap<TextShapeKey, Vec<(u16, u16)>>>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum DrawCommand {
    ClearScreenToColor {
        color: u32,
    },
    RoundedRectangle {
        x: f32,
        x2: f32,
        y: f32,
        y2: f32,
        radius_tl: isize,
        radius_tr: isize,
        radius_bl: isize,
        radius_br: isize,
        color: u32,
    },
    Scissor {
        x1: isize,
        y1: isize,
        x2: isize,
        y2: isize,
    },
    PixelLineXDef { // will draw one pixel per x
        x1: f32, // x1 must be less than x2
        y1: f32,
        x2: f32,
        y2: f32,
        color: u32,
        thickness: f32,
    },
    PixelLineYDef { // will draw one pixel per y
        x1: f32,
        y1: f32, // y1 must be less than y2
        x2: f32,
        y2: f32,
        color: u32,
        thickness: f32,
    },
    TextRow {
        y: u16,
        glyph_row_shift: u8,
        color: u32,
        font_tracker_id: u16,
        font_row_index: u16,
        // (lookup_id, start_x)
        glyph_bitmap_run: *const (u16, i16),
        glyph_bitmap_run_len: usize,
    },
}

impl Default for DrawCommand {
    fn default() -> Self {
        Self::ClearScreenToColor {
            color: 0
        }
    }
}

struct FrameStat {
    single_threaded_time_us: usize,
    work_time_us: usize,
    full_time_us: usize,
}
impl FrameStat {
    const _0: FrameStat = FrameStat {
        single_threaded_time_us: 0,
        work_time_us: 0,
        full_time_us: 0,
    };
}

#[allow(non_upper_case_globals)]
pub fn loop_curve(t: f64) -> (f64, f64) {
    // Tunables:
    const R: f64 = 5.0; // fixed circle radius
    const r: f64 = 3.0; // rolling circle radius
    const d: f64 = 4.0; // pen offset from the rolling circle center

    let k = (R - r) / r;      // frequency ratio (here = 2/3)
    let x = (R - r) * t.cos() + d * (k * t).cos();
    let y = (R - r) * t.sin() - d * (k * t).sin();
    (x, y)
}

#[allow(unused_variables)]
fn okay_but_is_it_wayland(elwt: &winit::event_loop::ActiveEventLoop) -> bool {
    #[cfg(target_os = "linux")]
    {
        use winit::platform::wayland::ActiveEventLoopExtWayland;
        elwt.is_wayland()
    }
    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

// pub static SOURCE_SERIF: &[u8] = include_bytes!("../assets/source_serif_4.ttf");
pub static INTER: &[u8] = include_bytes!("../assets/Inter-VariableFont_opsz,wght.ttf");
pub static ICONS: &[u8] = include_bytes!("../assets/fontello.ttf");
pub static DEJA_VU_SANS_MONO: &[u8] = include_bytes!("../assets/deja_vu_sans_mono.ttf");
pub static FONT_PIXEL_3X3_MONO: &[u8] = include_bytes!("../assets/3x3-Mono.ttf");
pub static FONT_PIXEL_TINY5: &[u8] = include_bytes!("../assets/Tiny5-Regular.ttf");
pub static FONT_PIXEL_GOHU_11: &[u8] = include_bytes!("../assets/gohufont-uni-11.ttf");
pub static FONT_PIXEL_GOHU_14: &[u8] = include_bytes!("../assets/gohufont-uni-14.ttf");

// These only exist for global volume. A mixer would be better but this is the minimal diff.
struct PlayingSound {
    sink: rodio::Sink,
    volume: f32, // base volume before multiplication by global volume
}

const GLOBAL_AUDIO_SCALE_FACTOR: f32 = 0.316; // 0.316 ~= perceptually "half volume"

static mut GLOBAL_OUTPUT_STREAM : *mut rodio::OutputStream = std::ptr::null_mut();
static mut playing_sounds: *mut Vec<PlayingSound> = std::ptr::null_mut(); // These only exist for global volume. A mixer would be better but this is the minimal diff.
static mut global_audio_volume: f32 = GLOBAL_AUDIO_SCALE_FACTOR;
pub fn setup_audio() {
    unsafe {
        if GLOBAL_OUTPUT_STREAM == std::ptr::null_mut() {
            if let Ok(stream) = rodio::OutputStreamBuilder::open_default_stream() {
                GLOBAL_OUTPUT_STREAM = alloc(Layout::new::<rodio::OutputStream>()) as *mut rodio::OutputStream;
                copy_nonoverlapping(&stream, GLOBAL_OUTPUT_STREAM, 1);
                std::mem::forget(stream);
            }
        }
        if playing_sounds == std::ptr::null_mut() {
            if let vec = Vec::<PlayingSound>::new() {
                playing_sounds = alloc(Layout::new::<Vec<PlayingSound>>()) as *mut Vec<PlayingSound>;
                copy_nonoverlapping(&vec, playing_sounds, 1);
                std::mem::forget(vec);
            }
        }
    }
}

pub fn play_sound(sound_file: &'static [u8], volume: f32, speed: f32) {
    unsafe {
        if GLOBAL_OUTPUT_STREAM != std::ptr::null_mut() {
            let stream = &mut *GLOBAL_OUTPUT_STREAM;
            let sink = rodio::play(stream.mixer(), std::io::Cursor::new(sound_file)).unwrap();
            sink.set_volume(volume * global_audio_volume);
            sink.set_speed(speed);

            let playing = &mut *playing_sounds;
            playing.retain(|s| !s.sink.empty()); // prune finished sounds
            playing.push(PlayingSound { sink, volume });
        }
    }
}

pub static SOUND_UI_WOOSH: &[u8] = include_bytes!("../assets/ui_woosh.ogg");
pub static SOUND_UI_HOVER: &[u8] = include_bytes!("../assets/ui_hover.ogg");
pub static SOUND_NOW_STAKING_DAY1: &[u8] = include_bytes!("../assets/now_staking_day1.ogg");
pub static SOUND_NOW_STAKING_DAY2: &[u8] = include_bytes!("../assets/now_staking_day2.ogg");
pub static SOUND_NOW_STAKING_DAY3: &[u8] = include_bytes!("../assets/now_staking_day3.ogg");
pub static SOUND_IT_IS_TIME_TO_STAKE1: &[u8] = include_bytes!("../assets/it_is_time_to_stake1.ogg");
pub static SOUND_SUBMIT_STAKING_TRANSACTIONS1: &[u8] = include_bytes!("../assets/submit_staking_transactions1.ogg");
pub static SOUND_SUBMIT_STAKING_TRANSACTIONS2: &[u8] = include_bytes!("../assets/submit_staking_transactions2.ogg");
pub static SOUND_UNLEASH_YOUR_CAPITAL1: &[u8] = include_bytes!("../assets/unleash_your_capital1.ogg");
pub static SOUND_TOKENS_AWAIT_STAKING1: &[u8] = include_bytes!("../assets/your_tokens_await_staking1.ogg");
pub static SOUND_CLAIM_YOUR_REWARDS1: &[u8] = include_bytes!("../assets/claim_your_rewards1.ogg");
pub static SOUND_LOCK_AND_EARN1: &[u8] = include_bytes!("../assets/lock_and_earn1.ogg");
pub static SOUND_STAKING_DAY_OVER1: &[u8] = include_bytes!("../assets/staking_day_over1.ogg");
pub static SOUND_STAKING_DAY_OVER2: &[u8] = include_bytes!("../assets/staking_day_over2.ogg");
pub static SOUND_STAKING_DAY_OVER3: &[u8] = include_bytes!("../assets/staking_day_over3.ogg");
pub static SOUND_STAKING_DAY_OVER4: &[u8] = include_bytes!("../assets/staking_day_over4.ogg");
pub static SOUND_STAKING_DAY_OVER5: &[u8] = include_bytes!("../assets/staking_day_over5.ogg");
pub static SOUND_STAKING_DAY_OVER6: &[u8] = include_bytes!("../assets/staking_day_over6.ogg");
pub static SOUND_DAMATIC_HORN1: &[u8] = include_bytes!("../assets/dramatic_horn1.ogg");
pub static SOUND_DAMATIC_HORN2: &[u8] = include_bytes!("../assets/dramatic_horn2.ogg");
pub static SOUND_DAMATIC_HORN3: &[u8] = include_bytes!("../assets/dramatic_horn3.ogg");
pub static SOUND_TRUMPET_FANFARE1: &[u8] = include_bytes!("../assets/trumpet_fanfare1.ogg");
pub static SOUND_GONG1: &[u8] = include_bytes!("../assets/gong1.ogg");
pub static SOUND_DRUM_FADEIN1: &[u8] = include_bytes!("../assets/drum_fadein1.ogg");
pub static SOUND_EPIC_TRANSITION1: &[u8] = include_bytes!("../assets/epic_transition1.ogg");
pub static SOUND_SOFT_CELLO1: &[u8] = include_bytes!("../assets/soft_cello1.ogg");
pub static SOUND_CELLO_DRONE1: &[u8] = include_bytes!("../assets/cello_drone1.ogg");

const DRAW_CALL_MAX: usize = 131072;
const GLYPH_RUN_MAX: usize = 16384;

// @Dev @Debug
pub const DEV_WIN32_WINDOW_ARRANGEMENT: bool = false;
pub static DEV_WIN32_WINDOW_RIGHT: AtomicBool = AtomicBool::new(false);

pub fn main_thread_run_program(wallet_state: Arc<Mutex<wallet::WalletState>>, fake_data: bool) {

    let mut viz_state = viz_gui_init(fake_data);

    // Create window + event loop.
    let event_loop = winit::event_loop::EventLoop::new().unwrap();

    let mut frame_stats = [FrameStat::_0; 512];
    let mut frame_stat_o = 0;
    let mut frame_interval_milli_hertz = 60000;
    let mut next_frame_deadline = Instant::now() + Duration::from_secs(1000) / frame_interval_milli_hertz;
    let mut prev_frame_time_single_threaded_us = 0usize;
    let mut prev_frame_time_us = 0usize;
    let mut prev_frame_time_total_us = 0usize;
    let mut prev_frame_time_total_us_max_5_seconds = 0usize;
    let mut prev_frame_time_single_thread_us_max_5_seconds = 0usize;
    let mut prev_frame_time_total_us_max_5_seconds_last_reset = Instant::now();
    let mut last_call_to_present_instant = Instant::now();
    let mut frame_is_actually_queued_by_us = false;
    let mut wayland_dropped_a_frame_on_purpose_counter = 0usize;

    let mut s_thread_context = ThreadContext {
        wake_up_gate: AtomicU32::new(0),
        workers_that_have_passed_the_wake_up_gate: AtomicU32::new(num_cpus::get_physical() as u32 - 1),
        wake_up_barrier: Barrier::new(num_cpus::get_physical()),
        is_last_time: false,
        thread_count: num_cpus::get_physical() as u32,
        begin_work_gate: AtomicU32::new(0),
        workers_at_job_site: AtomicU32::new(0),
        work_unit_count: 0,
        work_unit_take: AtomicU32::new(0),
        work_unit_complete: AtomicU32::new(0),
        work_user_pointer: 0,
        work_user_function: |_,_,_,_| {},
    };
    let p_thread_context: *mut ThreadContext = &mut s_thread_context as *mut ThreadContext;

    for thread_id in 1..unsafe { (*p_thread_context).thread_count as usize } {
        let magic_int = p_thread_context as usize;
        let _ = std::thread::spawn(move || {
            // println!("Started worker thread#{}...", thread_id);
            worker_thread_loop(thread_id, magic_int);
        });
    }

    let mut cached_square_width = 0;
    let mut render_target_0_alloc_layout = Layout::new::<u32>();
    let mut render_target_0 = std::ptr::null_mut();
    let mut saved_tile_hashes = Vec::new();
    let mut whole_screen_hash = 0u64;
    let mut did_window_resize = true;

    // staking day cosmetics
    let mut last_frame_finalized_bc_height = 0;
    let mut debug_next_animation_id = 0;
    let mut current_animation_t = None;
    let mut current_animation_id = 0;

    let mut input_ctx = InputCtx {
        mouse_moved: false,
        should_process_mouse_events: true,

        inflight_mouse_events:    Vec::new(),
        inflight_keyboard_events: Vec::new(),
        inflight_text_input:      Vec::new(),

        this_mouse_pos: (0, 0),
        last_mouse_pos: (0, 0),
        scroll_delta: (0.0, 0.0),
        zoom_delta: 0.0,

        mouse_down:     0,
        mouse_pressed:  0,
        mouse_released: 0,
        keys_down1:     0,
        keys_pressed1:  0,
        keys_released1: 0,
        keys_down2:     0,
        keys_pressed2:  0,
        keys_released2: 0,

        text_input: None,
    };

    let mut _draw_command_count = 0usize;
    let mut _glyph_bitmap_run_allocator_position = 0usize;
    let mut _font_tracker_count = 0usize;
    let mut _debug_pixel_inspector = None;
    let mut _debug_pixel_inspector_last_color = 0u32;

    let mut draw_ctx = unsafe { DrawCtx {
        window_width: 0,
        window_height: 0,
        draw_command_buffer: alloc(Layout::array::<DrawCommand>(DRAW_CALL_MAX).unwrap()) as *mut DrawCommand,
        draw_command_count: (&mut _draw_command_count) as *mut usize,
        glyph_bitmap_run_allocator: alloc(Layout::array::<(u16, i16)>(GLYPH_RUN_MAX).unwrap()) as *mut (u16, i16),
        glyph_bitmap_run_allocator_position: (&mut _glyph_bitmap_run_allocator_position) as *mut usize,
        font_tracker_buffer: alloc(Layout::array::<FontTracker>(8192).unwrap()) as *mut FontTracker,
        font_tracker_count: (&mut _font_tracker_count) as *mut usize,
        debug_pixel_inspector: (&mut _debug_pixel_inspector) as *mut Option<(usize, usize)>,
        debug_pixel_inspector_last_color: (&mut _debug_pixel_inspector_last_color) as *mut u32,
        measure_cache: HashMap::new(),
        text_shape_cache: RefCell::new(HashMap::new()),
    }};

    draw_ctx._init_font_tracker(FONT_PIXEL_3X3_MONO, 2, FontKind::Mono,  4);
    draw_ctx._init_font_tracker(FONT_PIXEL_3X3_MONO, 3, FontKind::Mono,  4);
    draw_ctx._init_font_tracker(FONT_PIXEL_3X3_MONO, 4, FontKind::Mono,  4);
    draw_ctx._init_font_tracker(FONT_PIXEL_3X3_MONO, 5, FontKind::Mono,  4);
    draw_ctx._init_font_tracker(FONT_PIXEL_TINY5,    6, FontKind::Mono,  9);
    draw_ctx._init_font_tracker(FONT_PIXEL_TINY5,    7, FontKind::Mono,  9);
    draw_ctx._init_font_tracker(FONT_PIXEL_TINY5,    8, FontKind::Mono,  9);
    draw_ctx._init_font_tracker(FONT_PIXEL_TINY5,    9, FontKind::Mono,  9);
    draw_ctx._init_font_tracker(FONT_PIXEL_GOHU_11, 10, FontKind::Mono, 13);
    draw_ctx._init_font_tracker(FONT_PIXEL_GOHU_11, 11, FontKind::Mono, 13);
    draw_ctx._init_font_tracker(FONT_PIXEL_GOHU_11, 12, FontKind::Mono, 13);
    draw_ctx._init_font_tracker(FONT_PIXEL_GOHU_11, 13, FontKind::Mono, 13);
    draw_ctx._init_font_tracker(FONT_PIXEL_GOHU_14, 14, FontKind::Mono, 15);
    draw_ctx._init_font_tracker(FONT_PIXEL_GOHU_14, 15, FontKind::Mono, 15);
    draw_ctx._init_font_tracker(FONT_PIXEL_GOHU_14, 16, FontKind::Mono, 15);

    let mut gui_clay = clay_layout::Clay::new((1280., 720.).into());

    let mut ui   = ui::Context::new();
    let mut data = ui::UiData::default();

    ui.clay = &mut gui_clay;

    // let mut t: f64 = 0.0;

    let mut window: Option<Rc<winit::window::Window>> = None;
    let mut softbuffer_context: Option<softbuffer::Context<Rc<winit::window::Window>>> = None;
    let mut softbuffer_surface: Option<softbuffer::Surface<Rc<winit::window::Window>, Rc<winit::window::Window>>> = None;
    let mut softbuffer_surface_size: (usize, usize) = (0, 0);

    let mut modifiers = winit::keyboard::ModifiersState::default();

    #[allow(deprecated)]
    event_loop.run(move |event, elwt: &winit::event_loop::ActiveEventLoop| {
        unsafe {
            global_audio_volume = ui.global_audio_volume * GLOBAL_AUDIO_SCALE_FACTOR;

            if playing_sounds != std::ptr::null_mut() {
                let playing = &mut *playing_sounds;
                playing.retain(|s| !s.sink.empty());
                for s in playing.iter() {
                    s.sink.set_volume(s.volume * global_audio_volume);
                }
            }
        }

        match event {
            winit::event::Event::Resumed => { // Runs at startup and is where we have to do init.
                let twindow = Rc::new(elwt.create_window(
                    winit::window::WindowAttributes::default()
                    .with_maximized({
                        #[cfg(target_os = "windows")]      if DEV_WIN32_WINDOW_ARRANGEMENT { false } else { true }
                        #[cfg(not(target_os = "windows"))] true
                    })
                    .with_title("Zcash Visualizer")
                    .with_inner_size(Size::Physical(winit::dpi::PhysicalSize { width: 1600, height: 900 }))
                ).unwrap());
                let context = softbuffer::Context::new(twindow.clone()).unwrap();
                let surface = softbuffer::Surface::new(&context, twindow.clone()).unwrap();

                // @Dev @Debug: Position windows for side-by-side debugging
                #[cfg(target_os = "windows")] if DEV_WIN32_WINDOW_ARRANGEMENT {
                    unsafe extern "system" {
                        fn GetConsoleWindow() -> *mut core::ffi::c_void;
                        fn SetWindowPos(hwnd: isize, insert_after: isize, x: i32, y: i32, cx: i32, cy: i32, flags: u32) -> i32;
                        fn GetSystemMetrics(index: i32) -> i32;
                    }
                    const SM_CXSCREEN: i32 = 0;
                    const SM_CYSCREEN: i32 = 1;
                    unsafe {

                        let right = DEV_WIN32_WINDOW_RIGHT.load(Ordering::Relaxed);

                        let screen_w = GetSystemMetrics(SM_CXSCREEN);
                        let screen_h = GetSystemMetrics(SM_CYSCREEN);

                        // Gui
                        use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
                        if let Ok(wh) = twindow.window_handle() {
                            if let RawWindowHandle::Win32(h) = wh.as_raw() {
                                let hwnd = h.hwnd.get() as isize;
                                let win_w = screen_w / 2;
                                let win_h = screen_h / 2;
                                let (x, y) = if right {
                                    (screen_w - win_w, screen_h - win_h) // bottom-right
                                } else {
                                    (0,                screen_h - win_h) // bottom-left
                                };
                                SetWindowPos(hwnd, 0, x, y, win_w, win_h, 0);
                            }
                        }

                        // Console
                        const SWP_NOSIZE: u32 = 0x0001;
                        let hwnd = GetConsoleWindow();
                        if !hwnd.is_null() {
                            let x = if right { screen_w / 2 } else { 0 };
                            SetWindowPos(hwnd as isize, 0, x, 0, 0, 0, SWP_NOSIZE);
                        }

                    }
                }

                window = Some(twindow);
                softbuffer_context = Some(context);
                softbuffer_surface = Some(surface);
                softbuffer_surface_size = (0, 0);

                setup_audio();
            },
            event => {
                if let Some(window) = window.as_mut() {
                    let softbuffer_context = softbuffer_context.as_mut().unwrap();
                    let softbuffer_surface = softbuffer_surface.as_mut().unwrap();
                    match event {
                        winit::event::Event::WindowEvent { window_id: _window_id, event } => {
                            // println!("Event! :) {:?}", event);
                            match event {
                                winit::event::WindowEvent::CursorEntered { device_id } |
                                winit::event::WindowEvent::CursorLeft    { device_id } => {
                                    input_ctx.should_process_mouse_events = event == winit::event::WindowEvent::CursorEntered{ device_id };
                                },
                                winit::event::WindowEvent::CursorMoved { device_id, position } => {
                                    input_ctx.this_mouse_pos = (position.x as isize, position.y as isize);
                                    input_ctx.mouse_moved    = true;
                                },
                                winit::event::WindowEvent::PinchGesture { device_id, delta, phase } => {
                                    input_ctx.zoom_delta += delta * 10.0;
                                }
                                winit::event::WindowEvent::MouseWheel { device_id, delta, phase } => {
                                    #[cfg(target_os = "macos")]
                                    {
                                        match delta {
                                            winit::event::MouseScrollDelta::LineDelta(x, y) => {
                                                input_ctx.zoom_delta -= y as f64 * 0.4;
                                            }
                                            winit::event::MouseScrollDelta::PixelDelta(pos) => {
                                                input_ctx.scroll_delta.0 += pos.x * 2.0;
                                                input_ctx.scroll_delta.1 += pos.y * 2.0;
                                            }
                                        }
                                    }
                                    #[cfg(not(target_os = "macos"))]
                                    {
                                        match delta {
                                            winit::event::MouseScrollDelta::LineDelta(x, y) => {
                                                input_ctx.zoom_delta += y as f64 * 1.0;
                                            }
                                            // GRRRRRR. NO PINCH SUPPORT ON WINDOWS OR LINUX. THANK YOU WINIT DEVS.
                                            winit::event::MouseScrollDelta::PixelDelta(pos) => {
                                                input_ctx.zoom_delta += pos.y * 0.05;
                                            }
                                        }
                                    }
                                }
                                winit::event::WindowEvent::MouseInput { device_id, state, button } => {
                                    if input_ctx.should_process_mouse_events {
                                        input_ctx.inflight_mouse_events.push((button, state));
                                    }
                                },
                                winit::event::WindowEvent::ModifiersChanged(m) => {
                                    modifiers = m.state();
                                },
                                winit::event::WindowEvent::KeyboardInput { device_id, event, is_synthetic } => {

                                    let ctrl  = modifiers.control_key() || modifiers.super_key();

                                    let is_paste = {

                                        use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
                                        let key   = event.key_without_modifiers();

                                        let shift = modifiers.shift_key();

                                        let ctrl_v       = ctrl  && matches!(key, winit::keyboard::Key::Character(ref s) if s.eq_ignore_ascii_case("v"));
                                        let shift_insert = shift && (key == winit::keyboard::Key::Named(winit::keyboard::NamedKey::Insert));

                                        ctrl_v || shift_insert
                                    };

                                    if event.state.is_pressed() && is_paste {
                                        input_ctx.inflight_text_input.extend(&input_ctx.get_from_clipboard().chars().collect::<Vec<char>>());
                                    } else {
                                        if !ctrl && let Some(text) = event.text && event.state.is_pressed() {
                                            input_ctx.inflight_text_input.extend(text.chars().filter(|c| *c >= ' ' && *c != 0x7f as char));
                                        }

                                        match event.physical_key {
                                            winit::keyboard::PhysicalKey::Code(kc) => if kc >= winit::keyboard::KeyCode::Eject && kc <= winit::keyboard::KeyCode::Undo {
                                                // println!("Skipping key: {:?}", kc);
                                            } else {
                                                input_ctx.inflight_keyboard_events.push((kc, event.state));
                                            }

                                            _ => {},
                                        }
                                    }
                                },
                                winit::event::WindowEvent::Moved(_)   |
                                winit::event::WindowEvent::Resized(_) => {
                                    did_window_resize = true;

                                    // Ensure our RedrawRequested handler will actually run on Windows
                                    frame_is_actually_queued_by_us = true;

                                    // Produce a frame for this size
                                    window.request_redraw();
                                },
                                winit::event::WindowEvent::RedrawRequested => {
                                    if frame_is_actually_queued_by_us || okay_but_is_it_wayland(elwt) {
                                        unsafe {
                                            let mut is_anything_happening_at_all_in_any_way = false;

                                            is_anything_happening_at_all_in_any_way |= did_window_resize;
                                            is_anything_happening_at_all_in_any_way |= ui.debug;
                                            is_anything_happening_at_all_in_any_way |= input_ctx.mouse_moved;
                                            is_anything_happening_at_all_in_any_way |= input_ctx.scroll_delta != (0.0, 0.0);
                                            is_anything_happening_at_all_in_any_way |= input_ctx.zoom_delta != 0.0;
                                            is_anything_happening_at_all_in_any_way |= current_animation_t.is_some();

                                            input_ctx.mouse_pressed  = 0;
                                            input_ctx.mouse_released = 0;
                                            input_ctx.keys_pressed1  = 0;
                                            input_ctx.keys_pressed2  = 0;
                                            input_ctx.keys_released1 = 0;
                                            input_ctx.keys_released2 = 0;
                                            did_window_resize = false;

                                            for (button, state) in &input_ctx.inflight_mouse_events {
                                                let button = match button {
                                                    winit::event::MouseButton::Left   => MOUSE_LEFT,
                                                    winit::event::MouseButton::Middle => MOUSE_MIDDLE,
                                                    winit::event::MouseButton::Right  => MOUSE_RIGHT,
                                                    _ => { continue; },
                                                };

                                                if state.is_pressed() {
                                                    input_ctx.mouse_down     |=  button;
                                                    input_ctx.mouse_pressed  |=  button;
                                                }
                                                else {
                                                    input_ctx.mouse_down     &= !button;
                                                    input_ctx.mouse_released |=  button;
                                                }
                                            }
                                            for (key, state) in &input_ctx.inflight_keyboard_events {
                                                if let Some(key) = 1u128.checked_shl(*key as u32) {
                                                    if state.is_pressed() {
                                                        input_ctx.keys_down1     |=  key;
                                                        input_ctx.keys_pressed1  |=  key;
                                                    }
                                                    else {
                                                        input_ctx.keys_down1     &= !key;
                                                        input_ctx.keys_released1 |=  key;
                                                    }
                                                } else if let Some(key) = 1u128.checked_shl(*key as u32 - 128) {
                                                    if state.is_pressed() {
                                                        input_ctx.keys_down2     |=  key;
                                                        input_ctx.keys_pressed2  |=  key;
                                                    }
                                                    else {
                                                        input_ctx.keys_down2     &= !key;
                                                        input_ctx.keys_released2 |=  key;
                                                    }
                                                }
                                            }
                                            input_ctx.text_input = if input_ctx.inflight_text_input.is_empty() {
                                                None
                                            } else {
                                                Some(input_ctx.inflight_text_input.clone())
                                            };

                                            is_anything_happening_at_all_in_any_way |= !input_ctx.inflight_text_input.is_empty();

                                         // is_anything_happening_at_all_in_any_way |= input_ctx.mouse_down     != 0;
                                            is_anything_happening_at_all_in_any_way |= input_ctx.mouse_pressed  != 0;
                                            is_anything_happening_at_all_in_any_way |= input_ctx.mouse_released != 0;

                                         // is_anything_happening_at_all_in_any_way |= (input_ctx.keys_down1     | input_ctx.keys_down2)     != 0;
                                            is_anything_happening_at_all_in_any_way |= (input_ctx.keys_pressed1  | input_ctx.keys_pressed2)  != 0;
                                            is_anything_happening_at_all_in_any_way |= (input_ctx.keys_released1 | input_ctx.keys_released2) != 0;

                                            input_ctx.inflight_mouse_events.clear();
                                            input_ctx.inflight_keyboard_events.clear();
                                            input_ctx.inflight_text_input.clear();

                                            // TODO: Poll Business for spontanious events. E.g. animations are still playing. Or, we recieved new blocks to display et cetera.
                                            is_anything_happening_at_all_in_any_way |= viz_gui_anything_happened_at_all(&mut viz_state);

                                            let (window_width, window_height) = {
                                                let size = window.inner_size();
                                                (size.width as usize, size.height as usize)
                                            };

                                            if is_anything_happening_at_all_in_any_way == false ||
                                               window_width <= 0 || window_height <= 0 { // Nothing to render.
                                                last_call_to_present_instant = Instant::now();
                                                frame_is_actually_queued_by_us = false;
                                                if okay_but_is_it_wayland(elwt) {
                                                    wayland_dropped_a_frame_on_purpose_counter = 2;
                                                }
                                                return;
                                            }

                                            // Note(Sam): Single threaded time is too long right now. 3 ms! So w cannot perform the wakeup here. It is too early. Optimistic wakeup should be at most 100 us early.
                                            // { // Tell workers: WAKE UP WAKE UP WAKE UP!!!
                                            //     while (*p_thread_context).workers_that_have_passed_the_wake_up_gate.load(Ordering::Relaxed) != (*p_thread_context).thread_count - 1 { spin_loop(); }
                                            //     (*p_thread_context).workers_that_have_passed_the_wake_up_gate.store(0, Ordering::Relaxed);
                                            //     (*p_thread_context).wake_up_gate.store(0, Ordering::Relaxed);
                                            //     (*p_thread_context).wake_up_barrier.wait();
                                            //     (*p_thread_context).is_last_time = false;
                                            //     (*p_thread_context).begin_work_gate.store(0, Ordering::Relaxed);
                                            //     (*p_thread_context).wake_up_gate.store(1, Ordering::Release);
                                            // }

                                            let target_frame_time_us = (1000000000.0 / (frame_interval_milli_hertz as f64)) as usize;
                                            let begin_frame_instant = Instant::now();

                                            if softbuffer_surface_size != (window_width, window_height) {
                                                softbuffer_surface.resize((window_width as u32).try_into().unwrap(), (window_height as u32).try_into().unwrap()).unwrap();
                                                softbuffer_surface_size = (window_width, window_height);
                                            }

                                            let mut buffer = softbuffer_surface.buffer_mut().unwrap();
                                            let final_output_blit_buffer = buffer.as_mut_ptr() as *mut u8;
                                            let window_square: usize = window_width.max(window_height);
                                            let tiles_wide = ((window_square + RENDER_TILE_SIZE - 1) / RENDER_TILE_SIZE).next_power_of_two();
                                            let draw_area_pixel_wide = tiles_wide * RENDER_TILE_SIZE;
                                            if cached_square_width != draw_area_pixel_wide {
                                                if render_target_0 != std::ptr::null_mut() {
                                                    dealloc(render_target_0, render_target_0_alloc_layout);
                                                }
                                                cached_square_width = draw_area_pixel_wide;
                                                render_target_0_alloc_layout = Layout::array::<u32>((draw_area_pixel_wide*draw_area_pixel_wide) as usize).unwrap().align_to(4096).unwrap();
                                                render_target_0 = alloc(render_target_0_alloc_layout);
                                                saved_tile_hashes = vec![0u64; tiles_wide*tiles_wide];
                                            }

                                            let mut dt = 1000.0 / (frame_interval_milli_hertz as f64);
                                            {
                                                let cpu_dt = prev_frame_time_total_us as f64 / 1000000.0;
                                                if cpu_dt > dt*2.0 { dt = cpu_dt; }
                                            }

                                            draw_ctx.window_width = window_width as isize;
                                            draw_ctx.window_height = window_height as isize;
                                            *draw_ctx.draw_command_count = 0;
                                            *draw_ctx.glyph_bitmap_run_allocator_position = 0;
                                            // free unused fonts, except not our special ones
                                            {
                                                let mut put = 0;
                                                for i in 0..*draw_ctx.font_tracker_count {
                                                    let take_ptr = draw_ctx.font_tracker_buffer.add(i);
                                                    if (*take_ptr).how_many_times_was_i_used == 0 && ((*take_ptr).ttf_file == DEJA_VU_SANS_MONO || (*take_ptr).ttf_file == INTER) {
                                                        std::ptr::drop_in_place(take_ptr);
                                                    } else {
                                                        let put_ptr = draw_ctx.font_tracker_buffer.add(put);
                                                        if put_ptr != take_ptr {
                                                            std::ptr::copy_nonoverlapping(take_ptr, put_ptr, 1);
                                                        }
                                                        (*put_ptr).how_many_times_was_i_used = 0;
                                                        put += 1;
                                                    }
                                                }
                                                *draw_ctx.font_tracker_count = put;
                                            }

                                            // if input_ctx.key_pressed(KeyCode::Space) {
                                            //     println!("woosh");
                                            //     play_sound(SOUND_UI_WOOSH, 0.5+0.1*rand::random::<f32>(), 1.0+0.5*rand::random::<f32>());
                                            // }

                                            // clear screen
                                            {
                                                let put = draw_ctx.draw_command_buffer.add(*draw_ctx.draw_command_count);
                                                *draw_ctx.draw_command_count += 1;
                                                assert!(*draw_ctx.draw_command_count <= DRAW_CALL_MAX);
                                                *put = DrawCommand::ClearScreenToColor { color: 0x080808 /* @todo colors */};
                                            }

                                            ui.input = &input_ctx;
                                            ui.draw  = &draw_ctx;
                                            ui.dpi_scale = window.scale_factor() as f32;
                                            ui.delta = dt as f32;
                                            
                                            // NOTE(Giovanni): Timer thingy //
                                            //{
                                            //    let timer = Timer::scope_("viz_gui_draw_the_stuff_for_the_things", true);
                                                   viz_gui_draw_the_stuff_for_the_things(&mut viz_state, &mut ui, &mut draw_ctx, dt as f32, &input_ctx);
                                            //}
                                            //////////////////////////////////
                                            {
                                                let should_quit = ui_update(&mut ui, &mut data, &mut viz_state, wallet_state.clone());
                                                if should_quit {
                                                    elwt.exit();
                                                }
                                            }

                                            {
                                                let new_height = viz_state.bc_finalized_tip_height;
                                                let new_pi= (new_height / UI_COPY_STAKING_DAY_PERIOD) * 2 + (new_height % UI_COPY_STAKING_DAY_PERIOD > UI_COPY_STAKING_DAY_WINDOW) as u64;
                                                let old_pi= (last_frame_finalized_bc_height / UI_COPY_STAKING_DAY_PERIOD) * 2 + (last_frame_finalized_bc_height % UI_COPY_STAKING_DAY_PERIOD > UI_COPY_STAKING_DAY_WINDOW) as u64;
                                                last_frame_finalized_bc_height = new_height;

                                                let debug_do_anyway = ui.debug && input_ctx.key_pressed(KeyCode::F4);
                                                if (new_pi as i64 - old_pi as i64).abs() == 1 || debug_do_anyway {
                                                    current_animation_id = new_pi;
                                                    current_animation_t = Some(0.0);
                                                    if debug_do_anyway {
                                                        current_animation_id = debug_next_animation_id;
                                                        debug_next_animation_id += 1;
                                                    }
                                                }
                                                if let Some(t) = current_animation_t.as_mut() {
                                                    let old_t = *t;
                                                    *t += dt;
                                                    let t = *t;

                                                    let music_random = (1+current_animation_id).wrapping_mul(11583847947735601999);
                                                    let voice_random = (1+current_animation_id).wrapping_mul(13723614751765347379);

                                                    let music_sound;
                                                    let music_volume;
                                                    let music_visual_delay;
                                                    let music_sound_delay;
                                                    let voice_sound;
                                                    let voice_volume;

                                                    if current_animation_id & 1 == 0 {
                                                        let music_index = music_random % 6;
                                                        let voice_index = voice_random % 10;

                                                        match music_index {
                                                            5 => {
                                                                music_sound = SOUND_EPIC_TRANSITION1;
                                                                music_volume = 1.0;
                                                                music_visual_delay = 0.5;
                                                                music_sound_delay = 1.0;
                                                            }
                                                            4 => {
                                                                music_sound = SOUND_DAMATIC_HORN3;
                                                                music_volume = 1.0;
                                                                music_visual_delay = 1.5;
                                                                music_sound_delay = 2.0;
                                                            }
                                                            3 => {
                                                                music_sound = SOUND_DAMATIC_HORN2;
                                                                music_volume = 1.0;
                                                                music_visual_delay = 1.5;
                                                                music_sound_delay = 2.0;
                                                            }
                                                            2 => {
                                                                music_sound = SOUND_DRUM_FADEIN1;
                                                                music_volume = 1.2;
                                                                music_visual_delay = 7.0;
                                                                music_sound_delay = 7.0;
                                                            }
                                                            1 => {
                                                                music_sound = SOUND_TRUMPET_FANFARE1;
                                                                music_volume = 1.0;
                                                                music_visual_delay = 12.5;
                                                                music_sound_delay = 13.5;
                                                            }
                                                            _ => {
                                                                music_sound = SOUND_DAMATIC_HORN1;
                                                                music_volume = 1.0;
                                                                music_visual_delay = 0.5;
                                                                music_sound_delay = 1.0;
                                                            }
                                                        }

                                                        match voice_index {
                                                            9 => {
                                                                voice_sound = SOUND_LOCK_AND_EARN1;
                                                                voice_volume = 1.4;
                                                            }
                                                            8 => {
                                                                voice_sound = SOUND_CLAIM_YOUR_REWARDS1;
                                                                voice_volume = 1.4;
                                                            }
                                                            7 => {
                                                                voice_sound = SOUND_SUBMIT_STAKING_TRANSACTIONS2;
                                                                voice_volume = 1.4;
                                                            }
                                                            6 => {
                                                                voice_sound = SOUND_NOW_STAKING_DAY3;
                                                                voice_volume = 1.4;
                                                            }
                                                            5 => {
                                                                voice_sound = SOUND_NOW_STAKING_DAY2;
                                                                voice_volume = 1.4;
                                                            }
                                                            4 => {
                                                                voice_sound = SOUND_TOKENS_AWAIT_STAKING1;
                                                                voice_volume = 1.8;
                                                            }
                                                            3 => {
                                                                voice_sound = SOUND_UNLEASH_YOUR_CAPITAL1;
                                                                voice_volume = 1.8;
                                                            }
                                                            2 => {
                                                                voice_sound = SOUND_SUBMIT_STAKING_TRANSACTIONS1;
                                                                voice_volume = 1.8;
                                                            }
                                                            1 => {
                                                                voice_sound = SOUND_IT_IS_TIME_TO_STAKE1;
                                                                voice_volume = 1.8;
                                                            }
                                                            _ => {
                                                                voice_sound = SOUND_NOW_STAKING_DAY1;
                                                                voice_volume = 1.8;
                                                            }
                                                        }
                                                    }
                                                    else {
                                                        let music_index = music_random % 7;
                                                        let voice_index = voice_random % 6;
                                                        match music_index {
                                                            6 => {
                                                                music_sound = SOUND_CELLO_DRONE1;
                                                                music_volume = 2.0;
                                                                music_visual_delay = 12.5;
                                                                music_sound_delay = 13.5;
                                                            }
                                                            5 => {
                                                                music_sound = SOUND_SOFT_CELLO1;
                                                                music_volume = 1.0;
                                                                music_visual_delay = 3.5;
                                                                music_sound_delay = 4.5;
                                                            }
                                                            4 => {
                                                                music_sound = SOUND_EPIC_TRANSITION1;
                                                                music_volume = 1.0;
                                                                music_visual_delay = 0.5;
                                                                music_sound_delay = 1.0;
                                                            }
                                                            3 => {
                                                                music_sound = SOUND_DAMATIC_HORN3;
                                                                music_volume = 1.0;
                                                                music_visual_delay = 1.5;
                                                                music_sound_delay = 2.0;
                                                            }
                                                            2 => {
                                                                music_sound = SOUND_DAMATIC_HORN2;
                                                                music_volume = 1.0;
                                                                music_visual_delay = 1.5;
                                                                music_sound_delay = 2.0;
                                                            }
                                                            1 => {
                                                                music_sound = SOUND_DRUM_FADEIN1;
                                                                music_volume = 1.2;
                                                                music_visual_delay = 7.0;
                                                                music_sound_delay = 7.0;
                                                            }
                                                            _ => {
                                                                music_sound = SOUND_GONG1;
                                                                music_volume = 1.5;
                                                                music_visual_delay = 0.2;
                                                                music_sound_delay = 0.5;
                                                            }
                                                        }

                                                        match voice_index {
                                                            5 => {
                                                                voice_sound = SOUND_STAKING_DAY_OVER6;
                                                                voice_volume = 1.2;
                                                            }
                                                            4 => {
                                                                voice_sound = SOUND_STAKING_DAY_OVER5;
                                                                voice_volume = 1.0;
                                                            }
                                                            3 => {
                                                                voice_sound = SOUND_STAKING_DAY_OVER4;
                                                                voice_volume = 1.2;
                                                            }
                                                            2 => {
                                                                voice_sound = SOUND_STAKING_DAY_OVER3;
                                                                voice_volume = 1.2;
                                                            }
                                                            1 => {
                                                                voice_sound = SOUND_STAKING_DAY_OVER2;
                                                                voice_volume = 1.2;
                                                            }
                                                            _ => {
                                                                voice_sound = SOUND_STAKING_DAY_OVER1;
                                                                voice_volume = 1.2;
                                                            }
                                                        }
                                                    }

                                                    let global_sound_volume = 0.7;

                                                    if old_t < 0.001 && t >= 0.001 {
                                                        play_sound(music_sound, global_sound_volume*music_volume, 1.0);
                                                    }
                                                    if old_t < music_sound_delay && t >= music_sound_delay {
                                                        play_sound(voice_sound, global_sound_volume*voice_volume, 1.0);
                                                    }

                                                    let text_h = (window_height as f32 / 3.0).min(window_width as f32 / 12.0);

                                                    let text_string = if current_animation_id & 1 != 0 { "Staking Day has Ended" } else { "Staking Day has Begun" };
                                                    let text_y = cubic_hold((t - music_visual_delay) as f32 / 5.0, 0.1, 0.7, 0.4)*1.4 * (window_height as f32 + text_h + 30.0) - (text_h + 30.0) * 1.4;

                                                    let text_w = draw_ctx.measure_text_line(FontKind::Normal, text_h, text_string);
                                                    let text_x = window_width as f32 / 2.0 - text_w / 2.0;
                                                    draw_ctx.rectangle_r(text_x - 20.0, text_y - 20.0, text_x + text_w + 20.0, text_y + text_h + 20.0, 30, 0x70_000000);
                                                    draw_ctx.text_line(FontKind::Normal, text_x, text_y, text_h, text_string, 0xffffffff);

                                                    if t > 60.0 { current_animation_t = None; }
                                                }
                                            }

                                            if (ui.prev_cursor != ui.cursor) {
                                                ui.prev_cursor  = ui.cursor.clone();
                                                window.set_cursor(ui.cursor.clone());
                                            }

                                            input_ctx.mouse_moved = false;
                                            input_ctx.last_mouse_pos = input_ctx.this_mouse_pos;
                                            input_ctx.scroll_delta = (0.0, 0.0);
                                            input_ctx.zoom_delta = 0.0;

                                            assert!(*draw_ctx.glyph_bitmap_run_allocator_position < GLYPH_RUN_MAX);
                                            assert!(*draw_ctx.draw_command_count <= DRAW_CALL_MAX);
                                            if *draw_ctx.draw_command_count == DRAW_CALL_MAX { println!("WARNING, we are at max capacity for draw commands."); }
                                            prev_frame_time_single_threaded_us = begin_frame_instant.elapsed().as_micros() as usize;

                                            // Note(Sam): We want to do this earlier but we are too slow. See comment above.
                                            { // Tell workers: WAKE UP WAKE UP WAKE UP!!!
                                                while (*p_thread_context).workers_that_have_passed_the_wake_up_gate.load(Ordering::Relaxed) != (*p_thread_context).thread_count - 1 { spin_loop(); }
                                                (*p_thread_context).workers_that_have_passed_the_wake_up_gate.store(0, Ordering::Relaxed);
                                                (*p_thread_context).wake_up_gate.store(0, Ordering::Relaxed);
                                                (*p_thread_context).wake_up_barrier.wait();
                                                (*p_thread_context).is_last_time = false;
                                                (*p_thread_context).begin_work_gate.store(0, Ordering::Relaxed);
                                                (*p_thread_context).wake_up_gate.store(1, Ordering::Release);
                                            }


                                            #[derive(Clone, Copy)]
                                            struct ExecuteCommandBufferOnTilesCtx {
                                                render_target_0: *mut u8,
                                                render_target_stride: usize,
                                                window_width: usize,
                                                window_height: usize,
                                                saved_tile_hashes: *mut u64,
                                                draw_ctx: *const DrawCtx,
                                            }
                                            let ups = ExecuteCommandBufferOnTilesCtx {
                                                render_target_0,
                                                render_target_stride: draw_area_pixel_wide,
                                                window_width,
                                                window_height,
                                                saved_tile_hashes: saved_tile_hashes.as_mut_ptr(),
                                                draw_ctx: &draw_ctx,
                                            };
                                            dennis_parallel_for(p_thread_context, false, tiles_wide*tiles_wide, &ups as *const ExecuteCommandBufferOnTilesCtx as usize,
                                                |thread_id: usize, work_id: usize, work_count: usize, user_pointer: usize| {
                                                    unsafe {
                                                        let ctx = *(user_pointer as *const ExecuteCommandBufferOnTilesCtx);
                                                        let pixel_row_shift = ctx.render_target_stride.trailing_zeros() as usize;
                                                        let intra_row_mask = ctx.render_target_stride.wrapping_sub(1);
                                                        debug_assert!(work_count.count_ones() == 1);
                                                        let tile_row_shift = work_count.trailing_zeros() / 2;
                                                        let tile_x = work_id & (1usize << tile_row_shift).wrapping_sub(1);
                                                        let tile_y = work_id >> tile_row_shift;

                                                        let tile_pixel_x = (tile_x << RENDER_TILE_SHIFT) as u32;
                                                        let tile_pixel_x2 = ((tile_x+1) << RENDER_TILE_SHIFT) as u32;
                                                        let tile_pixel_y = (tile_y << RENDER_TILE_SHIFT) as u32;
                                                        let tile_pixel_y2 = ((tile_y+1) << RENDER_TILE_SHIFT) as u32;
                                                        if tile_pixel_x as usize >= ctx.window_width { return; }
                                                        if tile_pixel_y as usize >= ctx.window_height { return; }

                                                        let debug_pixel = (*(*ctx.draw_ctx).debug_pixel_inspector).unwrap_or((usize::MAX, usize::MAX));

                                                        let mut scissor_x1: isize = 0;
                                                        let mut scissor_y1: isize = 0;
                                                        let mut scissor_x2: isize = ctx.window_width  as isize;
                                                        let mut scissor_y2: isize = ctx.window_height as isize;

                                                        let mut got_hash = 0u64;
                                                        for should_draw in 0..2 {
                                                            let should_draw = should_draw == 1;
                                                            if should_draw {
                                                                let saved = ctx.saved_tile_hashes.byte_add(8*work_id);
                                                                if got_hash == *saved && TURN_OFF_HASH_BASED_LAZY_RENDER == 0 {
                                                                    return;
                                                                } else {
                                                                    *saved = got_hash;
                                                                }
                                                            }
                                                            let mut hasher = ahash::AHasher::default();
                                                            let draw_commands = (*ctx.draw_ctx).draw_command_buffer;
                                                            let draw_command_count = *(*ctx.draw_ctx).draw_command_count;
                                                            for cmd_i in 0..draw_command_count {
                                                                match *draw_commands.byte_add(size_of::<DrawCommand>()*cmd_i) {
                                                                    DrawCommand::ClearScreenToColor { color } => {
                                                                        hasher.write_u64(0x83459345890234);
                                                                        hasher.write_u32(color);
                                                                        if should_draw == false { continue; }
                                                                        let mut row_pixels = ctx.render_target_0.byte_add(((tile_pixel_x + (tile_pixel_y << pixel_row_shift)) as usize) << 2);
                                                                        for _y in tile_pixel_y..tile_pixel_y2 {
                                                                            let mut cursor_pixels = row_pixels;
                                                                            for _x in tile_pixel_x..tile_pixel_x2 {
                                                                                *(cursor_pixels as *mut u32) = color;
                                                                                cursor_pixels = cursor_pixels.byte_add(4);
                                                                            }
                                                                            row_pixels = row_pixels.byte_add(4 << pixel_row_shift);
                                                                        }
                                                                    }
                                                                    DrawCommand::Scissor { x1, y1, x2, y2 } => {
                                                                        hasher.write_u64(0x897235923645643);
                                                                        hasher.write_isize(x1);
                                                                        hasher.write_isize(x2);
                                                                        hasher.write_isize(y1);
                                                                        hasher.write_isize(y2);
                                                                        if should_draw == false { continue; }
                                                                        scissor_x1 = x1;
                                                                        scissor_y1 = y1;
                                                                        scissor_x2 = x2;
                                                                        scissor_y2 = y2;
                                                                    }
                                                                    DrawCommand::RoundedRectangle {
                                                                        x: ofx,
                                                                        x2: ofx2,
                                                                        y: ofy,
                                                                        y2: ofy2,
                                                                        radius_tl,
                                                                        radius_tr,
                                                                        radius_bl,
                                                                        radius_br,
                                                                        color
                                                                    } => {
                                                                        let radius_tl = radius_tl as f32;
                                                                        let radius_tr = radius_tr as f32;
                                                                        let radius_bl = radius_bl as f32;
                                                                        let radius_br = radius_br as f32;

                                                                        // let radius_t = radius_tl.min(radius_tr);
                                                                        // let radius_t = radius_tl.min(radius_tr);


                                                                        let ix = (ofx.floor() as u32).max(tile_pixel_x);
                                                                        let ix2 = (ofx2.ceil() as u32).min(tile_pixel_x2);
                                                                        let iy = (ofy.floor() as u32).max(tile_pixel_y);
                                                                        let iy2 = (ofy2.ceil() as u32).min(tile_pixel_y2);
                                                                        if ix >= ix2 || iy >= iy2 { continue; }
                                                                        hasher.write_u64(0x854893982097);
                                                                        // hasher.write_u32(ofx.max(tile_pixel_x as f32 - radius).to_bits());
                                                                        // hasher.write_u32(ofx2.min(tile_pixel_x2 as f32 + radius).to_bits());
                                                                        // hasher.write_u32(ofy.max(tile_pixel_y as f32 - radius).to_bits());
                                                                        // hasher.write_u32(ofy2.min(tile_pixel_y2 as f32 + radius).to_bits());
                                                                        hasher.write_u32(ofx.to_bits());
                                                                        hasher.write_u32(ofx2.to_bits());
                                                                        hasher.write_u32(ofy.to_bits());
                                                                        hasher.write_u32(ofy2.to_bits());
                                                                        hasher.write_u32(radius_tl.to_bits());
                                                                        hasher.write_u32(radius_tr.to_bits());
                                                                        hasher.write_u32(radius_bl.to_bits());
                                                                        hasher.write_u32(radius_br.to_bits());
                                                                        hasher.write_u32(color);
                                                                        if should_draw == false { continue; }
                                                                        if scissor_x1 >= scissor_x2 { continue; }
                                                                        if scissor_y1 >= scissor_y2 { continue; }
                                                                        let mut row_pixels = ctx.render_target_0.byte_add(((ix + (iy << pixel_row_shift)) as usize) << 2);
                                                                        let color_alpha = (color >> 24) as f32 / 255.0;
                                                                        for _y in iy..iy2 {
                                                                            let _fy = _y as f32;
                                                                            let row_alpha = (1.0 - (ofy - _fy).clamp(0.0, 1.0)) * (ofy2 - _fy).clamp(0.0, 1.0);
                                                                            let mut cursor_pixels = row_pixels;
                                                                            for _x in ix..ix2 {
                                                                                let _fx = _x as f32;
                                                                                let mut cover_alpha = (1.0 - (ofx - _fx).clamp(0.0, 1.0)) * (ofx2 - _fx).clamp(0.0, 1.0) * row_alpha;
                                                                                // top left
                                                                                {
                                                                                    let lx = _fx-ofx2+radius_tl;
                                                                                    let ly = ofy-(_fy+1.0)+radius_tl;
                                                                                    let v1 = lx*lx.abs()+ly*ly.abs();
                                                                                    let new_alpha = radius_tl - v1.sqrt();
                                                                                    cover_alpha = select_float(v1 >= 0.0, cover_alpha.min(new_alpha), cover_alpha,);
                                                                                }
                                                                                // top right
                                                                                {
                                                                                    let lx = ofx-(_fx+1.0)+radius_tr;
                                                                                    let ly = ofy-(_fy+1.0)+radius_tr;
                                                                                    let v1 = lx*lx.abs()+ly*ly.abs();
                                                                                    let new_alpha = radius_tr - v1.sqrt();
                                                                                    cover_alpha = select_float(v1 >= 0.0, cover_alpha.min(new_alpha), cover_alpha,);
                                                                                }
                                                                                // bottom left
                                                                                {
                                                                                    let lx = _fx-ofx2+radius_bl;
                                                                                    let ly = _fy-ofy2+radius_bl;
                                                                                    let v1 = lx*lx.abs()+ly*ly.abs();
                                                                                    let new_alpha = radius_bl - v1.sqrt();
                                                                                    cover_alpha = select_float(v1 >= 0.0, cover_alpha.min(new_alpha), cover_alpha,);
                                                                                }
                                                                                // bottom right
                                                                                {
                                                                                    let lx = ofx-(_fx+1.0)+radius_br;
                                                                                    let ly = _fy-ofy2+radius_br;
                                                                                    let v1 = lx*lx.abs()+ly*ly.abs();
                                                                                    let new_alpha = radius_br - v1.sqrt();
                                                                                    cover_alpha = select_float(v1 >= 0.0, cover_alpha.min(new_alpha), cover_alpha,);
                                                                                }
                                                                                let pixel_alpha = color_alpha * cover_alpha;

                                                                                if (_x as isize) >= scissor_x1 && (_x as isize) < scissor_x2 &&
                                                                                   (_y as isize) >= scissor_y1 && (_y as isize) < scissor_y2 {  // @Scissor
                                                                                    *(cursor_pixels as *mut u32) = blend_u32(*(cursor_pixels as *mut u32), color,  (pixel_alpha * 255.0).round() as u32);
                                                                                }
                                                                                cursor_pixels = cursor_pixels.byte_add(4);
                                                                            }
                                                                            row_pixels = row_pixels.byte_add(4 << pixel_row_shift);
                                                                        }
                                                                    },
                                                                    DrawCommand::TextRow { y, glyph_row_shift, color, font_tracker_id, font_row_index, glyph_bitmap_run, glyph_bitmap_run_len } => {
                                                                        if (y as u32) < tile_pixel_y || (y as u32) >= tile_pixel_y2 { continue; }
                                                                        if (y as isize) < scissor_y1 || (y as isize) >= scissor_y2 { continue; } // @Scissor

                                                                        let font_tracker = &*(*ctx.draw_ctx).font_tracker_buffer.add(font_tracker_id as usize);
                                                                        let bitmap_widths = font_tracker.cached_bitmap_widths.as_ptr();
                                                                        let row_bitmaps = font_tracker.row_buffers[font_row_index as usize].as_ptr();

                                                                        for i in 0..glyph_bitmap_run_len {
                                                                            let (lookup_index, start_x) = *glyph_bitmap_run.add(i);
                                                                            let width = *bitmap_widths.add(lookup_index as usize) as usize;

                                                                            if (start_x as isize + width as isize - 1) < tile_pixel_x as isize { continue; }
                                                                            if (start_x as isize) >= tile_pixel_x2 as isize { break; }
                                                                            hasher.write_u64(0x8936730958944);
                                                                            hasher.write_u16(lookup_index);
                                                                            hasher.write_i16(start_x);
                                                                            hasher.write_u16(y);
                                                                            hasher.write_usize(row_bitmaps as usize);
                                                                            hasher.write_usize(bitmap_widths as usize);
                                                                            hasher.write_u32(color);
                                                                            // TODO rethink, font id?
                                                                            if should_draw == false { continue; }
                                                                            if scissor_x1 >= scissor_x2 { continue; }
                                                                            if scissor_y1 >= scissor_y2 { continue; }

                                                                            let mut copy_data = row_bitmaps.byte_add((lookup_index as usize) << glyph_row_shift);
                                                                            let mut put_data = ctx.render_target_0.byte_add((y as usize) << (pixel_row_shift+2)).byte_offset(start_x as isize *4);

                                                                            let mut x1 = start_x as isize;
                                                                            let x2 = (start_x as isize + width as isize).min(tile_pixel_x2 as isize);
                                                                            if x1 < tile_pixel_x as isize {
                                                                                copy_data = copy_data.byte_add((tile_pixel_x as isize - x1) as usize);
                                                                                put_data = put_data.byte_add((tile_pixel_x as isize - x1) as usize * 4);
                                                                                x1 = tile_pixel_x as isize;
                                                                            }
                                                                            let len = x2 - x1;
                                                                            if len <= 0 { continue; }
                                                                            for _x in x1..x2 {
                                                                                let blend = *copy_data as u32;
                                                                                copy_data = copy_data.byte_add(1);
                                                                                if (_x as isize) >= scissor_x1 && (_x as isize) < scissor_x2 { // @Scissor
                                                                                    *(put_data as *mut u32) = blend_u32(*(put_data as *mut u32), color, blend);
                                                                                }
                                                                                put_data = put_data.byte_add(4);
                                                                            }
                                                                        }
                                                                    },
                                                                    DrawCommand::PixelLineXDef { x1, y1, x2, y2, color, thickness, } => {
                                                                        if thickness <= 1.0 {
                                                                            let x1 = x1.round() as isize;
                                                                            let y1 = y1.round() as isize;
                                                                            let x2 = x2.round() as isize;
                                                                            let y2 = y2.round() as isize;
                                                                            let start_x = (x1 as isize).max(tile_pixel_x as isize);
                                                                            let end_x = (x2 as isize).min(tile_pixel_x2 as isize);
                                                                            if start_x >= end_x { continue; }
                                                                            let dy = (y2 as f32 - y1 as f32) / x2.wrapping_sub(x1) as f32;
                                                                            hasher.write_u64(0x75634593484);
                                                                            hasher.write_isize(x1);
                                                                            hasher.write_isize(x2);
                                                                            hasher.write_isize(y1);
                                                                            hasher.write_isize(y2);
                                                                            hasher.write_u32(color);
                                                                            hasher.write_u32(dy.to_bits());
                                                                            if should_draw == false { continue; }
                                                                            for real_x in start_x..end_x {
                                                                                let fy = y1 as f32 + dy * (real_x - x1 as isize) as f32;
                                                                                let iy1 = fy.floor() as isize;
                                                                                let iy2 = iy1+1;
                                                                                let coverage1 = (iy2 as f32 - fy).clamp(0.0, thickness);
                                                                                let coverage2 = thickness - coverage1;
                                                                                let blend1 = 255.0 * linear_to_srgb_one_channel_float(coverage1);
                                                                                let blend2 = 255.0 * linear_to_srgb_one_channel_float(coverage2);

                                                                                if iy1 >= tile_pixel_y as isize && iy1 < tile_pixel_y2 as isize {
                                                                                    let pixel = ctx.render_target_0.byte_add(((real_x + (iy1 << pixel_row_shift)) as usize) << 2);
                                                                                    let this_color = blend_u32(*(pixel as *mut u32), color, blend1 as u32);
                                                                                    *(pixel as *mut u32) = blend_u32(*(pixel as *mut u32), this_color, color >> 24);
                                                                                }
                                                                                if iy2 >= tile_pixel_y as isize && iy2 < tile_pixel_y2 as isize {
                                                                                    let pixel = ctx.render_target_0.byte_add(((real_x + (iy2 << pixel_row_shift)) as usize) << 2);
                                                                                    let this_color = blend_u32(*(pixel as *mut u32), color, blend2 as u32);
                                                                                    *(pixel as *mut u32) = blend_u32(*(pixel as *mut u32), this_color, color >> 24);
                                                                                }
                                                                            }
                                                                        } else {
                                                                            // x1 is known to be less than x2 but this does not apply to y.
                                                                            let by_min = y1.min(y2);
                                                                            let by_max = y1.max(y2);
                                                                            let ix1 = ((x1 - (thickness / 2.0)).floor() as isize - 1).max(tile_pixel_x as isize);
                                                                            let ix2 = ((x2 + (thickness / 2.0)).ceil() as isize + 1).min(tile_pixel_x2 as isize);
                                                                            let iy1 = ((by_min - (thickness / 2.0)).floor() as isize - 1).max(tile_pixel_y as isize);
                                                                            let iy2 = ((by_max + (thickness / 2.0)).ceil() as isize + 1).min(tile_pixel_y2 as isize);
                                                                            if ix1 >= ix2 || iy1 >= iy2 { continue; }

                                                                            hasher.write_u64(0x75345834958);
                                                                            hasher.write_u32(x1.to_bits());
                                                                            hasher.write_u32(x2.to_bits());
                                                                            hasher.write_u32(y1.to_bits());
                                                                            hasher.write_u32(y2.to_bits());
                                                                            hasher.write_u32(thickness.to_bits());
                                                                            hasher.write_u32(color);
                                                                            if should_draw == false { continue; }

                                                                            let dx = (x2 - x1) / (y2 - y1);
                                                                            for iy in iy1..iy2 {
                                                                                let fx = x1 as f32 + dx * (iy - y1 as isize) as f32;
                                                                                let h = (thickness / 2.0 + 1.0) / f32::sin(f32::atan(1.0/dx.abs()));
                                                                                let cull_ix1;
                                                                                let cull_ix2;
                                                                                if h.is_normal() {
                                                                                    cull_ix1 = (fx - h).floor() as isize;
                                                                                    cull_ix2 = (fx + h).ceil() as isize;
                                                                                } else {
                                                                                    cull_ix1 = 0;
                                                                                    cull_ix2 = 9999999999;
                                                                                }
                                                                                for ix in ix1.max(cull_ix1)..ix2.min(cull_ix2) {
                                                                                    let pixel = ctx.render_target_0.byte_add(((ix + (iy << pixel_row_shift)) as usize) << 2);
                                                                                    let coverage = (1.0 - sd_segment((ix as f32, iy as f32), (x1, y1), (x2, y2), thickness / 2.0)).clamp(0.0, 1.0);
                                                                                    let this_color = blend_u32(*(pixel as *mut u32), color, (coverage * 255.0) as u32);
                                                                                    *(pixel as *mut u32) = blend_u32(*(pixel as *mut u32), this_color, color >> 24);
                                                                                }
                                                                            }
                                                                        }
                                                                    },
                                                                    DrawCommand::PixelLineYDef { x1, y1, x2, y2, color, thickness, } => {
                                                                        if thickness <= 1.0 {
                                                                            let x1 = x1.round() as i16;
                                                                            let y1 = y1.round() as i16;
                                                                            let x2 = x2.round() as i16;
                                                                            let y2 = y2.round() as i16;
                                                                            let start_y = (y1 as isize).max(tile_pixel_y as isize);
                                                                            let end_y = (y2 as isize).min(tile_pixel_y2 as isize);
                                                                            if start_y >= end_y { continue; }
                                                                            let dx = (x2 as f32 - x1 as f32) / y2.wrapping_sub(y1) as f32;
                                                                            hasher.write_u64(0x83248923897);
                                                                            hasher.write_i16(x1);
                                                                            hasher.write_i16(x2);
                                                                            hasher.write_i16(y1);
                                                                            hasher.write_i16(y2);
                                                                            hasher.write_u32(color);
                                                                            hasher.write_u32(dx.to_bits());
                                                                            if should_draw == false { continue; }
                                                                            for real_y in start_y..end_y {
                                                                                let fx = x1 as f32 + dx * (real_y - y1 as isize) as f32;
                                                                                let ix1 = fx.floor() as isize;
                                                                                let ix2 = ix1+1;
                                                                                let coverage1 = (ix2 as f32 - fx).clamp(0.0, thickness);
                                                                                let coverage2 = thickness - coverage1;
                                                                                let blend1 = 255.0 * linear_to_srgb_one_channel_float(coverage1);
                                                                                let blend2 = 255.0 * linear_to_srgb_one_channel_float(coverage2);

                                                                                if ix1 >= tile_pixel_x as isize && ix1 < tile_pixel_x2 as isize {
                                                                                    let pixel = ctx.render_target_0.byte_add(((ix1 + (real_y << pixel_row_shift)) as usize) << 2);
                                                                                    let this_color = blend_u32(*(pixel as *mut u32), color, blend1 as u32);
                                                                                    *(pixel as *mut u32) = blend_u32(*(pixel as *mut u32), this_color, color >> 24);
                                                                                }
                                                                                if ix2 >= tile_pixel_x as isize && ix2 < tile_pixel_x2 as isize {
                                                                                    let pixel = ctx.render_target_0.byte_add(((ix2 + (real_y << pixel_row_shift)) as usize) << 2);
                                                                                    let this_color = blend_u32(*(pixel as *mut u32), color, blend2 as u32);
                                                                                    *(pixel as *mut u32) = blend_u32(*(pixel as *mut u32), this_color, color >> 24);
                                                                                }
                                                                            }
                                                                        } else {
                                                                            // y1 is known to be less than y2 but this does not apply to x.
                                                                            let bx_min = x1.min(x2);
                                                                            let bx_max = x1.max(x2);
                                                                            let ix1 = ((bx_min - (thickness / 2.0)).floor() as isize - 1).max(tile_pixel_x as isize);
                                                                            let ix2 = ((bx_max + (thickness / 2.0)).ceil() as isize + 1).min(tile_pixel_x2 as isize);
                                                                            let iy1 = ((y1 - (thickness / 2.0)).floor() as isize - 1).max(tile_pixel_y as isize);
                                                                            let iy2 = ((y2 + (thickness / 2.0)).ceil() as isize + 1).min(tile_pixel_y2 as isize);
                                                                            if ix1 >= ix2 || iy1 >= iy2 { continue; }

                                                                            hasher.write_u64(0x75345834958);
                                                                            hasher.write_u32(x1.to_bits());
                                                                            hasher.write_u32(x2.to_bits());
                                                                            hasher.write_u32(y1.to_bits());
                                                                            hasher.write_u32(y2.to_bits());
                                                                            hasher.write_u32(thickness.to_bits());
                                                                            hasher.write_u32(color);
                                                                            if should_draw == false { continue; }

                                                                            let dx = (x2 - x1) / (y2 - y1);
                                                                            for iy in iy1..iy2 {
                                                                                let fx = x1 as f32 + dx * (iy - y1 as isize) as f32;
                                                                                let h = (thickness / 2.0 + 1.0) / f32::sin(f32::atan(1.0/dx.abs()));
                                                                                let cull_ix1;
                                                                                let cull_ix2;
                                                                                if h.is_normal() {
                                                                                    cull_ix1 = (fx - h).floor() as isize;
                                                                                    cull_ix2 = (fx + h).ceil() as isize;
                                                                                } else {
                                                                                    cull_ix1 = 0;
                                                                                    cull_ix2 = 9999999999;
                                                                                }
                                                                                for ix in ix1.max(cull_ix1)..ix2.min(cull_ix2) {
                                                                                    let pixel = ctx.render_target_0.byte_add(((ix + (iy << pixel_row_shift)) as usize) << 2);
                                                                                    let coverage = (1.0 - sd_segment((ix as f32, iy as f32), (x1, y1), (x2, y2), thickness / 2.0)).clamp(0.0, 1.0);
                                                                                    let this_color = blend_u32(*(pixel as *mut u32), color, (coverage * 255.0) as u32);
                                                                                    *(pixel as *mut u32) = blend_u32(*(pixel as *mut u32), this_color, color >> 24);
                                                                                }
                                                                            }
                                                                        }
                                                                    },
                                                                }
                                                            }
                                                            got_hash = hasher.finish();
                                                        }
                                                    }
                                                });

                                            let need_buffer_flip;
                                            {
                                                let mut hasher = ahash::AHasher::default();
                                                hasher.write_usize(window_width);
                                                hasher.write_usize(window_height);
                                                for th in &saved_tile_hashes { hasher.write_u64(*th); }
                                                let new_hash = hasher.finish();
                                                need_buffer_flip = whole_screen_hash != new_hash;
                                                whole_screen_hash = new_hash;
                                            }

                                            prev_frame_time_us = begin_frame_instant.elapsed().as_micros() as usize;

                                            struct EndOfFrameBlitCtx {
                                                render_target_0: *mut u8,
                                                display_buffer: *mut u8,
                                                render_target_stride: usize,
                                                display_buffer_stride: usize,
                                                row_count: usize,
                                            }
                                            let mut ups = EndOfFrameBlitCtx {
                                                render_target_0,
                                                display_buffer: final_output_blit_buffer,
                                                render_target_stride: draw_area_pixel_wide,
                                                display_buffer_stride: window_width,
                                                row_count: window_height,
                                            };
                                            // Note(Sam): We need to call dennis_parallel_for with is_last_time true in order for the threads to go to sleep. Therefore if we don't want to do work we pass work_count=0.
                                            dennis_parallel_for(p_thread_context, true, (need_buffer_flip as usize)*((window_height + 32 - 1) / 32), &ups as *const EndOfFrameBlitCtx as usize,
                                            |thread_id: usize, work_id: usize, work_count: usize, user_pointer: usize| {
                                                unsafe {
                                                    let p_thread_context = user_pointer as *mut EndOfFrameBlitCtx;
                                                    let render_target_0 = (*p_thread_context).render_target_0;
                                                    let display_buffer = (*p_thread_context).display_buffer;

                                                    let row_count = (*p_thread_context).row_count;

                                                    let render_target_stride = (*p_thread_context).render_target_stride;
                                                    let display_buffer_stride = (*p_thread_context).display_buffer_stride;

                                                    let mut local_row_count = row_count / work_count;
                                                    let start_row = work_id * local_row_count;
                                                    if work_id == work_count - 1 {
                                                        local_row_count = row_count - start_row;
                                                    }

                                                    for row in start_row..start_row+local_row_count {
                                                        let pixel_src = render_target_0.byte_add(row*render_target_stride*4) as *mut u8;
                                                        let pixel_dst = display_buffer.byte_add(row*display_buffer_stride*4) as *mut u8;
                                                        copy_nonoverlapping(pixel_src, pixel_dst, display_buffer_stride*4);
                                                    }
                                                }
                                            });

                                            prev_frame_time_total_us = begin_frame_instant.elapsed().as_micros() as usize;
                                            frame_stats[frame_stat_o % frame_stats.len()].single_threaded_time_us = prev_frame_time_single_threaded_us as usize;
                                            frame_stats[frame_stat_o % frame_stats.len()].work_time_us = prev_frame_time_us as usize;
                                            frame_stats[frame_stat_o % frame_stats.len()].full_time_us = prev_frame_time_total_us as usize;
                                            frame_stat_o += 1;

                                            prev_frame_time_single_thread_us_max_5_seconds = prev_frame_time_single_thread_us_max_5_seconds.max(prev_frame_time_single_threaded_us);
                                            prev_frame_time_total_us_max_5_seconds = prev_frame_time_total_us_max_5_seconds.max(prev_frame_time_total_us);

                                            if let Some((x, y)) = *draw_ctx.debug_pixel_inspector {
                                                let pixel_src = render_target_0.byte_add((x+y*draw_area_pixel_wide)*4);
                                                *draw_ctx.debug_pixel_inspector_last_color = *(pixel_src as *mut u32);
                                            }

                                            if ui.debug {
                                                *draw_ctx.draw_command_count = 0;
                                                draw_ctx.text_line(FontKind::Mono, 8.0, 8.0, 13.0,
                                                    &format!(
                                                        "Rate: {} hz | (us) deadline: {} internal:{:>5} total:{:>5} max(5s):{:>5}",
                                                        frame_interval_milli_hertz as f32 / 1000.0,
                                                        target_frame_time_us,
                                                        prev_frame_time_us,
                                                        prev_frame_time_total_us,
                                                        prev_frame_time_total_us_max_5_seconds
                                                    ),
                                                    if prev_frame_time_total_us_max_5_seconds < target_frame_time_us { 0xff_00ff00 } else { 0xff_ff5500 },
                                                );
                                                draw_ctx.text_line(FontKind::Mono, 8.0, 20.0, 13.0,
                                                    &format!(
                                                        "SINGLE THREAD PART: (us) {:>5} max(5s):{:>5}",
                                                        prev_frame_time_single_threaded_us,
                                                        prev_frame_time_single_thread_us_max_5_seconds
                                                    ),
                                                    0xff_00ff00,
                                                );

                                                // Force a non measured render buffer reset if the real code is not active to save CPU.
                                                if need_buffer_flip == false
                                                {
                                                    for row in 0..window_height {
                                                        let pixel_src = render_target_0.byte_add(row*draw_area_pixel_wide*4) as *mut u8;
                                                        let pixel_dst = final_output_blit_buffer.byte_add(row*window_width*4) as *mut u8;
                                                        copy_nonoverlapping(pixel_src, pixel_dst, window_width*4);
                                                    }
                                                }

                                                // draw those commands *manually*
                                                for cmd_i in 0..*draw_ctx.draw_command_count {
                                                    match *draw_ctx.draw_command_buffer.add(cmd_i) {
                                                        DrawCommand::TextRow { y, glyph_row_shift, color, font_tracker_id, font_row_index, glyph_bitmap_run, glyph_bitmap_run_len } => {
                                                            let font_tracker = &*draw_ctx.font_tracker_buffer.add(font_tracker_id as usize);
                                                            let bitmap_widths = font_tracker.cached_bitmap_widths.as_ptr();
                                                            let row_bitmaps = font_tracker.row_buffers[font_row_index as usize].as_ptr();

                                                            for i in 0..glyph_bitmap_run_len {
                                                                let (lookup_index, start_x) = *glyph_bitmap_run.add(i);
                                                                let width = *bitmap_widths.add(lookup_index as usize) as usize;

                                                                let mut copy_data = row_bitmaps.byte_add((lookup_index as usize) << glyph_row_shift);
                                                                let mut put_data = final_output_blit_buffer.byte_add(y as usize *window_width*4).byte_offset(start_x as isize *4);

                                                                let mut x1 = start_x as isize;
                                                                let x2 = (start_x as isize + width as isize).min(window_width as isize);
                                                                if x1 < 0 {
                                                                    copy_data = copy_data.byte_add((0 - x1) as usize);
                                                                    put_data = put_data.byte_add((0 - x1) as usize * 4);
                                                                    x1 = 0;
                                                                }
                                                                if x1 >= x2 { continue; }
                                                                let len = x2 - x1;
                                                                debug_assert!(len != 0);
                                                                for _ in 0..len {
                                                                    let blend = *copy_data as u32;
                                                                    copy_data = copy_data.byte_add(1);
                                                                    *(put_data as *mut u32) = blend_u32(*(put_data as *mut u32), color, blend);
                                                                    put_data = put_data.byte_add(4);
                                                                }
                                                            }
                                                        },
                                                        _ => panic!("ehm what?"),
                                                    }
                                                }


                                                for (i, stat) in frame_stats.iter().enumerate() {
                                                    if !ui.frame_graph {
                                                        break;
                                                    }
                                                    let thick = 2;
                                                    let y = 40 + thick*i;
                                                    let single_w = (stat.single_threaded_time_us / 10).min(window_width);
                                                    let work_w = (stat.work_time_us / 10).min(window_width);
                                                    let full_w = (stat.full_time_us / 10).min(window_width);
                                                    let full_w = full_w.max(work_w) - work_w;
                                                    let work_w = work_w.max(single_w) - single_w;
                                                    let single_color = if stat.full_time_us < target_frame_time_us { 0xbb77ee } else { 0xffbb33 };
                                                    let work_color = if stat.full_time_us < target_frame_time_us { 0x2266ee } else { 0xee2222 };
                                                    let full_color = if stat.full_time_us < target_frame_time_us { 0x1133ff } else { 0x991111 };
                                                    for t in 0..thick {
                                                        if y+t >= window_height { continue; }
                                                        let mut put_ptr = final_output_blit_buffer.byte_add((y+t)*window_width*4) as *mut u32;
                                                        for x in 0..single_w {
                                                            *put_ptr = blend_u32(*put_ptr, single_color, 128);
                                                            put_ptr = put_ptr.byte_add(4);
                                                        }
                                                        for x in 0..work_w {
                                                            *put_ptr = blend_u32(*put_ptr, work_color, 128);
                                                            put_ptr = put_ptr.byte_add(4);
                                                        }
                                                        for x in 0..full_w {
                                                            *put_ptr = blend_u32(*put_ptr, full_color, 128);
                                                            put_ptr = put_ptr.byte_add(4);
                                                        }
                                                    }
                                                }
                                            }

                                            if prev_frame_time_total_us_max_5_seconds_last_reset.elapsed().as_secs() >= 5 {
                                                prev_frame_time_single_thread_us_max_5_seconds = 0;
                                                prev_frame_time_total_us_max_5_seconds = 0;
                                                prev_frame_time_total_us_max_5_seconds_last_reset = Instant::now();
                                            }

                                            // frame pace is not interesting for profiling
                                            let frame_pace_us = last_call_to_present_instant.elapsed().as_micros() as u64;
                                            last_call_to_present_instant = Instant::now();

                                            let need_buffer_flip = need_buffer_flip || ui.debug;
                                            if need_buffer_flip {
                                                if okay_but_is_it_wayland(elwt) {
                                                    window.pre_present_notify();
                                                }
                                                buffer.present().unwrap();
                                            }
                                            frame_is_actually_queued_by_us = false;
                                            if okay_but_is_it_wayland(elwt) {
                                                if need_buffer_flip {
                                                    window.request_redraw();
                                                } else {
                                                    wayland_dropped_a_frame_on_purpose_counter = 2;
                                                }
                                            }
                                        }
                                    }
                                },
                                winit::event::WindowEvent::CloseRequested => {
                                    elwt.exit();
                                },
                                _ => {},
                            }
                        },
                        winit::event::Event::AboutToWait => {
                            if let Some(monitor) = window.current_monitor() {
                                if let Some(refresh_rate) = monitor.refresh_rate_millihertz() {
                                    frame_interval_milli_hertz = refresh_rate;
                                }
                            }
                            if okay_but_is_it_wayland(elwt) {
                                if wayland_dropped_a_frame_on_purpose_counter == 2 {
                                    wayland_dropped_a_frame_on_purpose_counter = 1;
                                    elwt.set_control_flow(winit::event_loop::ControlFlow::WaitUntil(Instant::now() + Duration::from_secs(1000) / frame_interval_milli_hertz));
                                } else if wayland_dropped_a_frame_on_purpose_counter == 1 {
                                    wayland_dropped_a_frame_on_purpose_counter = 0;
                                    window.request_redraw();
                                    elwt.set_control_flow(winit::event_loop::ControlFlow::Wait);
                                } else {
                                    elwt.set_control_flow(winit::event_loop::ControlFlow::Wait);
                                }
                            } else {
                                let now = Instant::now();
                                if now >= next_frame_deadline {
                                    if last_call_to_present_instant > next_frame_deadline {
                                    } else {
                                        frame_is_actually_queued_by_us = true;
                                        window.request_redraw();
                                    }
                                    if now - next_frame_deadline > Duration::from_millis(250) {
                                        next_frame_deadline = Instant::now() + Duration::from_secs(1000) / frame_interval_milli_hertz;
                                    } else {
                                        while now >= next_frame_deadline {
                                            next_frame_deadline += Duration::from_secs(1000) / frame_interval_milli_hertz;
                                        }
                                    }
                                } else {
                                    std::thread::sleep(next_frame_deadline.saturating_duration_since(now));
                                }
                                elwt.set_control_flow(winit::event_loop::ControlFlow::Poll);
                            }
                        },
                        _ => (),
                    }
                }
            },
        }
    }).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_works() {
    }

    fn approx_eq(a: u8, b: u8) -> bool {
        let da = a as i16;
        let db = b as i16;
        (da - db).abs() <= 3
    }

    #[test]
    fn test_round_trip_sampled() {
        for r in (0..=255).step_by(17) {
            for g in (0..=255).step_by(17) {
                for b in (0..=255).step_by(17) {
                    let a = 255;

                    let (h, s, v, a2) = rgba_to_hsva(r, g, b, a);
                    let (rr, gg, bb, aa) = hsva_to_rgba(h, s, v, a2);

                    assert!(approx_eq(r, rr), "r={} rr={}", r, rr);
                    assert!(approx_eq(g, gg), "g={} gg={}", g, gg);
                    assert!(approx_eq(b, bb), "b={} bb={}", b, bb);
                    assert_eq!(a, aa);
                }
            }
        }
    }

    // Uncomment to brute-force all 16,777,216 colors
    /*
    #[test]
    fn test_round_trip_full() {
        for r in 0..=255 {
            for g in 0..=255 {
                for b in 0..=255 {
                    let a = 255;

                    let (h, s, v, a2) = rgba_to_hsva(r, g, b, a);
                    let (rr, gg, bb, aa) = hsva_to_rgba(h, s, v, a2);

                    assert!(approx_eq(r, rr), "r={} rr={}", r, rr);
                    assert!(approx_eq(g, gg), "g={} gg={}", g, gg);
                    assert!(approx_eq(b, bb), "b={} bb={}", b, bb);
                    assert_eq!(a, aa);
                }
            }
        }
    }
    */
}

/// Returns f1 is b is true.
#[inline(always)]
fn select_float(b: bool, f1: f32, f2: f32) -> f32 {
    f32::from_bits(select_u32(b, f1.to_bits(), f2.to_bits()))
}
/// Returns f1 is b is true.
#[inline(always)]
fn select_u32(b: bool, f1: u32, f2: u32) -> u32 {
    let mask = 0u32.wrapping_sub(b as u32);
    f2 ^ ((f1 ^ f2) & mask)
}

#[inline(always)]
fn blend_u32(color_1: u32, color_2: u32, blend: u32) -> u32 {
    let b1 = (color_1 >> 0) & 0xff;
    let g1 = (color_1 >> 8) & 0xff;
    let r1 = (color_1 >> 16) & 0xff;
    let b2 = (color_2 >> 0) & 0xff;
    let g2 = (color_2 >> 8) & 0xff;
    let r2 = (color_2 >> 16) & 0xff;
    let blend = clamp_u32(blend, 0, 255);
    let b = (b1 as u32 * (255 - blend) + b2 * blend) / 255;
    let g = (g1 as u32 * (255 - blend) + g2 * blend) / 255;
    let r = (r1 as u32 * (255 - blend) + r2 * blend) / 255;
    r << 16 | g << 8 | b
}
#[inline(always)]
fn clamp_u32(u: u32, min: u32, max: u32) -> u32 {
    let v1 = select_u32(u <= min, min, u);
    select_u32(v1 >= max, max, v1)
}
#[inline(always)]
fn linear_to_srgb_one_channel_float(linear: f32) -> f32 {
    if linear <= 0.0031308 {
        12.92 * linear
    } else {
        1.055 * fast_powf(linear, 1.0 / 2.4)  - 0.055
    }
}
#[inline(always)]
fn srgb_to_linear_one_channel_float(srgb: f32) -> f32 {
    if srgb <= 0.04045 {
        srgb / 12.92
    } else {
        fast_powf((srgb + 0.055) / 1.055, 2.4)
    }
}

#[inline(always)]
pub fn fast_log2(x: f32) -> f32 {
    // Decompose x = 2^e * m, with m in [1,2)
    let ui = x.to_bits();
    let e  = ((ui >> 23) & 0xFF) as i32 - 127;
    let m  = f32::from_bits((ui & 0x7FFFFF) | 0x3F80_0000); // 1.m

    // taylor ln(m) around 1: ln(m) ≈ t*(1 - t/2 + t^2/3)
    // then log2(m) = ln(m) * (1/ln 2)
    let t = m - 1.0;
    let ln_m = t * (1.0 - 0.5 * t + (1.0/3.0) * t * t);
    let inv_ln2 = 1.442_695_040_888_963_4_f32; // 1/ln(2)

    e as f32 + ln_m * inv_ln2
}

#[inline(always)]
pub fn fast_exp2(x: f32) -> f32 {
    // x = i + f, i = floor(x), f in [0,1)
    let i = x.floor();
    let f = x - i;

    // exp2(f) = e^(f ln2) ≈ 1 + a f + (a^2/2) f^2 + (a^3/6) f^3
    let a = 0.693_147_180_559_945_3_f32; // ln(2)
    let f2 = f * f;
    let f3 = f2 * f;
    let poly = 1.0 + a*f + 0.5*a*a*f2 + (a*a*a)*(1.0/6.0)*f3;

    // scale by 2^i via exponent bits
    let ei = (i as i32 + 127).clamp(0, 255);
    let scale = f32::from_bits((ei as u32) << 23);
    scale * poly
}

#[inline(always)]
pub fn fast_powf(x: f32, y: f32) -> f32 {
    // Domain: x > 0. For x<=0 you must special-case as you would with powf.
    fast_exp2(y * fast_log2(x))
}

#[inline(always)]
fn sd_segment(
    p: (f32, f32),
    a: (f32, f32),
    b: (f32, f32),
    r: f32,
) -> f32 {
    let ba = (b.0 - a.0, b.1 - a.1);
    let pa = (p.0 - a.0, p.1 - a.1);

    let dot_pa_ba = pa.0 * ba.0 + pa.1 * ba.1;
    let dot_ba_ba = ba.0 * ba.0 + ba.1 * ba.1;

    // Avoid divide-by-zero if segment is degenerate
    let h = if dot_ba_ba != 0.0 {
        (dot_pa_ba / dot_ba_ba).clamp(0.0, 1.0)
    } else {
        0.0
    };

    let dx = pa.0 - h * ba.0;
    let dy = pa.1 - h * ba.1;

    (dx * dx + dy * dy).sqrt() - r
}

#[inline]
fn clamp01(x: f32) -> f32 {
    if x < 0.0 { 0.0 } else if x > 1.0 { 1.0 } else { x }
}

#[inline]
fn smoothstep01(u: f32) -> f32 {
    // cubic: 3u^2 - 2u^3
    let u = clamp01(u);
    let u2 = u * u;
    (3.0 * u2) - (2.0 * u2 * u)
}

/// Cubic spline with a true "stand still" plateau.
/// Maps t in [0,1] to y in [0,1].
///
/// hold_start: start time of the standstill (0..1)
/// hold_end:   end time of the standstill   (0..1), should be > hold_start
/// hold_y:     y value during the hold (typically 0.5), should be in [0,1]
///
/// Notes:
/// - y'(0)=0, y'(hold_start)=0, y'(hold_end)=0, y'(1)=0
/// - y is constant in [hold_start, hold_end]
pub fn cubic_hold(t: f32, hold_start: f32, hold_end: f32, hold_y: f32) -> f32 {
    let t = clamp01(t);
    let hs = clamp01(hold_start);
    let he = clamp01(hold_end);
    let hy = clamp01(hold_y);

    // Avoid division by zero / nonsense ranges
    // If hold region collapses, this becomes a single smoothstep.
    let eps = 1.0e-6;

    if he <= hs + eps {
        return smoothstep01(t);
    }

    if t <= hs {
        // Ease 0 -> hy over [0, hs]
        let denom = if hs > eps { hs } else { eps };
        let u = t / denom;
        return hy * smoothstep01(u);
    }

    if t < he {
        // Flat hold
        return hy;
    }

    // Ease hy -> 1 over [he, 1]
    let denom = (1.0 - he);
    let denom = if denom > eps { denom } else { eps };
    let u = (t - he) / denom;
    hy + (1.0 - hy) * smoothstep01(u)
}
