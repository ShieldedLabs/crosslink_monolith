#![allow(unsafe_code)]

mod ui;
use ui::*;

mod viz_gui;
pub use viz_gui::*;

const TURN_OFF_HASH_BASED_LAZY_RENDER: usize = 0;

use std::{alloc::{alloc, dealloc, Layout}, hash::Hasher, hint::spin_loop, mem::{swap, transmute, MaybeUninit}, ptr::{copy_nonoverlapping, slice_from_raw_parts}, rc::Rc, sync::{atomic::{AtomicU32, Ordering}, Barrier}, time::{Duration, Instant}, u32};
use twox_hash::xxhash3_64;
use winit::{dpi::Size, keyboard::KeyCode};

use rustybuzz::{shape, Face as RbFace, UnicodeBuffer};
use swash::{scale::ScaleContext, text, FontRef};

const RENDER_TILE_SHIFT: usize = 7;
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
        let thread_count = (*p_thread_context).thread_count;
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
        let thread_count = (*p_thread_context).thread_count;

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

impl DrawCtx {

    pub fn _init_font_tracker(&self, ttf_file: &'static [u8], target_px_height: usize, is_mono: bool, fudge_to_px_height: usize) -> *mut FontTracker {
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
                is_mono: is_mono,
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

    pub fn _find_or_create_font_tracker<'a>(&self, target_px_height: usize, is_mono: bool) -> (&'a mut FontTracker, usize) {
        unsafe {
            let mut found_font = std::ptr::null_mut();
            for i in 0..*self.font_tracker_count {
                let check = self.font_tracker_buffer.add(i);
                if (*check).target_px_height == target_px_height as usize && (*check).is_mono == is_mono {
                    found_font = check;
                    break;
                }
            }
            if found_font == std::ptr::null_mut() {
                if is_mono {
                    found_font = self._init_font_tracker(DEJA_VU_SANS_MONO, target_px_height, true, usize::MAX);
                } else {
                    found_font = self._init_font_tracker(SOURCE_SERIF, target_px_height, false, usize::MAX);
                }
            }
            let tracker: &mut FontTracker = &mut *found_font;
            let tracker_id = (found_font as usize - self.font_tracker_buffer as usize) / size_of::<FontTracker>();
            (tracker, tracker_id)
        }
    }

    pub fn _render_glyph_if_not_cached(tracker: &mut FontTracker, glyph_id: u16, px_advance: u16) {
        unsafe {
            if tracker.glyph_to_bitmap_index[glyph_id as usize] == u16::MAX
            {
                let swash_font = FontRef::from_index(tracker.ttf_file, 0).expect("font ref");

                let bitmap_index = tracker.cached_bitmaps_counter;
                tracker.cached_bitmaps_counter += 1;
                tracker.glyph_to_bitmap_index[glyph_id as usize] = bitmap_index;

                tracker.cached_bitmap_widths.push(px_advance);

                let mut _scale = ScaleContext::new();
                let mut scale = _scale.builder(swash_font).size(tracker.ppem).hint(false).build();

                let image = swash::scale::Render::new(&[
                    swash::scale::Source::Bitmap(swash::scale::StrikeWith::BestFit),
                    swash::scale::Source::Outline,
                ])
                .format(swash::zeno::Format::Alpha)
                .render(&mut scale, glyph_id as u16).unwrap();
                assert_eq!(image.content, swash::scale::image::Content::Mask);

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
                        std::ptr::copy_nonoverlapping(&image.data[image.placement.width as usize * cy], row_put.add(image.placement.left as usize & row_len.wrapping_sub(1)), image.placement.width as usize);
                    }
                }
            }
        }
    }

    pub fn measure_text_line(&self, text_height: f32, text_line: &str) -> f32 {
        if text_height <= 0.0 || text_height.is_normal() == false { return 0.0; }
        let text_height = text_height.min(8192.0);

        if text_height < 3.0 {
            return (1.0 * text_height * text_line.len() as f32) / 3.0;
        }
        let text_height = text_height.floor() as usize;

        let (tracker, _) = self._find_or_create_font_tracker(text_height, false);
        tracker.how_many_times_was_i_used += 1;

        let mut buf = UnicodeBuffer::new();
        buf.set_direction(rustybuzz::Direction::LeftToRight);
        buf.push_str(text_line);
        buf.set_direction(rustybuzz::Direction::LeftToRight);

        let shaped = shape(&tracker.cached_rusty_buzz, &[], buf);
        let infos = shaped.glyph_infos();
        let poss = shaped.glyph_positions();
        assert_eq!(infos.len(), poss.len());

        if poss.len() > 0 {
            poss.iter().map(|g_pos| {
                let px_advance = (((g_pos.x_advance as f32 / tracker.units_per_em) * tracker.ppem).ceil() as usize).min(1usize << tracker.glyph_row_shift);
                px_advance
            }).reduce(|acc, a| acc + a).unwrap() as f32
        } else {
            0.0
        }
    }

    pub fn text_line(&self, text_x: f32, text_y: f32, text_height: f32, text_line: &str, color: u32) {
        unsafe {
            if text_height <= 0.0 || text_height.is_normal() == false { return; }
            let text_height = text_height.min(8192.0);

            if text_height < 3.0 {
                self.rectangle(text_x, text_y, text_x + (1.0 * text_height * text_line.len() as f32) / 3.0, text_y + text_height, (color&0xFFffFF) | ((color >> 26) << 24));
                return;
            }

            let text_x = text_x.floor() as isize;
            let text_y = text_y.floor() as isize;
            let text_height = text_height.floor() as usize;

            let (tracker, tracker_id) = self._find_or_create_font_tracker(text_height, false);
            tracker.how_many_times_was_i_used += 1;

            let mut buf = UnicodeBuffer::new();
            buf.set_direction(rustybuzz::Direction::LeftToRight);
            buf.push_str(text_line);
            buf.set_direction(rustybuzz::Direction::LeftToRight);

            let shaped = shape(&tracker.cached_rusty_buzz, &[], buf);
            let infos = shaped.glyph_infos();
            let poss = shaped.glyph_positions();
            assert_eq!(infos.len(), poss.len());

            let glyph_bitmap_run_start = self.glyph_bitmap_run_allocator.add(*self.glyph_bitmap_run_allocator_position);
            let mut glyph_bitmap_run_count = 0usize;

            let mut acc_x = text_x;
            for text_index in 0..infos.len() {
                let g_info = &infos[text_index];
                let g_pos = &poss[text_index];

                let px_advance = (((g_pos.x_advance as f32 / tracker.units_per_em) * tracker.ppem).ceil() as usize).min(1usize << tracker.glyph_row_shift);

                // TODO: Scissor
                if (acc_x + px_advance as isize) <= 0 { acc_x += px_advance as isize; continue; }
                if acc_x > self.window_width { break; }

                Self::_render_glyph_if_not_cached(tracker, g_info.glyph_id as u16, px_advance as u16);

                *glyph_bitmap_run_start.add(glyph_bitmap_run_count) = (tracker.glyph_to_bitmap_index[g_info.glyph_id as usize], acc_x as i16);
                glyph_bitmap_run_count += 1;
                acc_x += px_advance as isize;
            }

            *self.glyph_bitmap_run_allocator_position += glyph_bitmap_run_count;

            let actual_height = if tracker.fudge_to_px_height == usize::MAX { tracker.target_px_height } else { tracker.fudge_to_px_height };
            for y in 0..actual_height {
                let screen_y = y as isize + text_y;
                if screen_y >= 0 && screen_y < self.window_height {
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

    // TODO: Make float?
    pub fn measure_mono_text_line(&self, text_height: f32, text_line: &str) -> f32 {
        if text_height <= 0.0 || text_height.is_normal() == false { return 0.0; }
        let text_height = text_height.min(8192.0);

        if text_height < 3.0 {
            return (4.0 * text_height * text_line.len() as f32) / 3.0;
        }

        let text_height = text_height.floor() as usize;

        let (tracker, _) = self._find_or_create_font_tracker(text_height, true);
        tracker.how_many_times_was_i_used += 1;

        let mut buf = UnicodeBuffer::new();
        buf.set_direction(rustybuzz::Direction::LeftToRight);
        buf.push_str(text_line);
        buf.set_direction(rustybuzz::Direction::LeftToRight);

        let shaped = shape(&tracker.cached_rusty_buzz, &[], buf);
        let infos = shaped.glyph_infos();
        let poss = shaped.glyph_positions();
        assert_eq!(infos.len(), poss.len());

        poss.iter().map(|g_pos| {
            let px_advance = (((g_pos.x_advance as f32 / tracker.units_per_em) * tracker.ppem).ceil() as usize).min(1usize << tracker.glyph_row_shift);
            px_advance
        }).reduce(|acc, a| acc + a).unwrap() as f32
    }

    pub fn mono_text_line(&self, text_x: f32, text_y: f32, text_height: f32, text_line: &str, color: u32) {
        unsafe {
            if text_height <= 0.0 || text_height.is_normal() == false { return; }
            let text_height = text_height.min(8192.0);

            if text_height < 3.0 {
                self.rectangle(text_x, text_y, text_x + (4.0 * text_height * text_line.len() as f32) / 3.0, text_y + text_height, (color&0xFFffFF) | ((color >> 26) << 24));
                return;
            }

            let text_x = text_x.floor() as isize;
            let text_y = text_y.floor() as isize;
            let text_height = text_height.floor() as usize;

            let (tracker, tracker_id) = self._find_or_create_font_tracker(text_height, true);
            tracker.how_many_times_was_i_used += 1;

            let mut buf = UnicodeBuffer::new();
            buf.set_direction(rustybuzz::Direction::LeftToRight);
            buf.push_str(text_line);
            buf.set_direction(rustybuzz::Direction::LeftToRight);

            let shaped = shape(&tracker.cached_rusty_buzz, &[], buf);
            let infos = shaped.glyph_infos();
            let poss = shaped.glyph_positions();
            assert_eq!(infos.len(), poss.len());

            let glyph_bitmap_run_start = self.glyph_bitmap_run_allocator.add(*self.glyph_bitmap_run_allocator_position);
            let mut glyph_bitmap_run_count = 0usize;

            let mut acc_x = text_x;
            for text_index in 0..infos.len() {
                let g_info = &infos[text_index];
                let g_pos = &poss[text_index];

                let px_advance = (((g_pos.x_advance as f32 / tracker.units_per_em) * tracker.ppem).ceil() as usize).min(1usize << tracker.glyph_row_shift);

                // TODO: Scissor
                if (acc_x + px_advance as isize) <= 0 { acc_x += px_advance as isize; continue; }
                if acc_x > self.window_width { break; }

                Self::_render_glyph_if_not_cached(tracker, g_info.glyph_id as u16, px_advance as u16);

                *glyph_bitmap_run_start.add(glyph_bitmap_run_count) = (tracker.glyph_to_bitmap_index[g_info.glyph_id as usize], acc_x as i16);
                glyph_bitmap_run_count += 1;
                acc_x += px_advance as isize;
            }

            *self.glyph_bitmap_run_allocator_position += glyph_bitmap_run_count;

            let actual_height = if tracker.fudge_to_px_height == usize::MAX { tracker.target_px_height } else { tracker.fudge_to_px_height };
            for y in 0..actual_height {
                let screen_y = y as isize + text_y;
                if screen_y >= 0 && screen_y < self.window_height {
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
    }

    pub fn rectangle(&self, x1: f32, y1: f32, x2: f32, y2: f32, color: u32) {
        unsafe {
            let put = self.draw_command_buffer.add(*self.draw_command_count);
            *self.draw_command_count += 1;
            *put = DrawCommand::ColoredRectangle { x: x1.max(0.0), x2: x2.min(self.window_width as f32), y: y1.max(0.0) as f32, y2: y2.min(self.window_height as f32), round_pixels: 0.0, color: color };
        }
    }

    // (x1, y1) and (x2, y2) can be in any order
    pub fn line(&self, mut x1: f32, mut y1: f32, mut x2: f32, mut y2: f32, thickness: f32, color: u32) {
        unsafe {
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
        self.line(x2, y2, x2 + (fx * thickness * 4.0), y2 + (fy * thickness * 4.0), thickness, color);
        self.line(x2, y2, x2 + (fy * thickness * 4.0), y2 + (-fx * thickness * 4.0), thickness, color);
    }

    pub fn rounded_rectangle(&self, x1: isize, y1: isize, x2: isize, y2: isize, radius: isize, color: u32) {
        unsafe {
            let put = self.draw_command_buffer.add(*self.draw_command_count);
            *self.draw_command_count += 1;
            *put = DrawCommand::ColoredRectangle { x: x1.max(0) as f32, x2: x2.min(self.window_width) as f32, y: y1.max(0) as f32, y2: y2.min(self.window_height) as f32, round_pixels: radius as f32, color, };
        }
    }

    pub fn circle(&self, x: f32, y: f32, radius: f32, color: u32) {
        unsafe {
            let put = self.draw_command_buffer.add(*self.draw_command_count);
            *self.draw_command_count += 1;
            *put = DrawCommand::ColoredRectangle { x: x - radius, x2: x + radius, y: y - radius, y2: y + radius, round_pixels: radius, color, };
        }
    }
    pub fn circle_square(&self, x: f32, y: f32, radius: f32, round_pixels: f32, color: u32) {
        unsafe {
            let put = self.draw_command_buffer.add(*self.draw_command_count);
            *self.draw_command_count += 1;
            *put = DrawCommand::ColoredRectangle { x: x - radius, x2: x + radius, y: y - radius, y2: y + radius, round_pixels: round_pixels, color, };
        }
    }
}


#[derive(Debug)]
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
}

struct FontTracker {
    how_many_times_was_i_used: usize,
    is_mono: bool,
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
}

#[derive(Clone, Copy, Debug)]
enum DrawCommand {
    ClearScreenToColor {
        color: u32,
    },
    ColoredRectangle {
        x: f32,
        x2: f32,
        y: f32,
        y2: f32,
        round_pixels: f32,
        color: u32,
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

pub static SOURCE_SERIF: &[u8] = include_bytes!("../assets/source_serif_4.ttf");
pub static DEJA_VU_SANS_MONO: &[u8] = include_bytes!("../assets/deja_vu_sans_mono.ttf");
pub static FONT_PIXEL_3X3_MONO: &[u8] = include_bytes!("../assets/3x3-Mono.ttf");
pub static FONT_PIXEL_TINY5: &[u8] = include_bytes!("../assets/Tiny5-Regular.ttf");
pub static FONT_PIXEL_GOHU_11: &[u8] = include_bytes!("../assets/gohufont-uni-11.ttf");
pub static FONT_PIXEL_GOHU_14: &[u8] = include_bytes!("../assets/gohufont-uni-14.ttf");

static mut GLOBAL_OUTPUT_STREAM : *mut rodio::OutputStream = std::ptr::null_mut();
pub fn setup_audio() {
    unsafe {
        if GLOBAL_OUTPUT_STREAM == std::ptr::null_mut() {
            if let Ok(stream) = rodio::OutputStreamBuilder::open_default_stream() {
                GLOBAL_OUTPUT_STREAM = alloc(Layout::new::<rodio::OutputStream>()) as *mut rodio::OutputStream;
                copy_nonoverlapping(&stream, GLOBAL_OUTPUT_STREAM, 1);
                std::mem::forget(stream);
            }
        }
    }
}

pub fn play_sound(sound_file: &'static [u8], volume: f32, speed: f32) {
    unsafe {
        if GLOBAL_OUTPUT_STREAM != std::ptr::null_mut() {
            let stream = &mut *GLOBAL_OUTPUT_STREAM;
            let sink = rodio::play(stream.mixer(), std::io::Cursor::new(sound_file)).unwrap();
            sink.set_volume(volume);
            sink.set_speed(speed);
            sink.detach();
        }
    }
}

pub static SOUND_UI_WOOSH: &[u8] = include_bytes!("../assets/ui_woosh.ogg");
pub static SOUND_UI_HOVER: &[u8] = include_bytes!("../assets/ui_hover.ogg");

pub fn main_thread_run_program() {

    let mut viz_state = viz_gui_init();

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
            println!("Started worker thread#{}...", thread_id);
            worker_thread_loop(thread_id, magic_int);
        });
    }

    let mut cached_square_width = 0;
    let mut render_target_0_alloc_layout = Layout::new::<u32>();
    let mut render_target_0 = std::ptr::null_mut();
    let mut saved_tile_hashes = Vec::new();
    let mut whole_screen_hash = 0u64;
    let mut did_window_resize = true;

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
        draw_command_buffer: alloc(Layout::array::<DrawCommand>(8192).unwrap()) as *mut DrawCommand,
        draw_command_count: (&mut _draw_command_count) as *mut usize,
        glyph_bitmap_run_allocator: alloc(Layout::array::<(u16, i16)>(8192).unwrap()) as *mut (u16, i16),
        glyph_bitmap_run_allocator_position: (&mut _glyph_bitmap_run_allocator_position) as *mut usize,
        font_tracker_buffer: alloc(Layout::array::<FontTracker>(8192).unwrap()) as *mut FontTracker,
        font_tracker_count: (&mut _font_tracker_count) as *mut usize,
        debug_pixel_inspector: (&mut _debug_pixel_inspector) as *mut Option<(usize, usize)>,
        debug_pixel_inspector_last_color: (&mut _debug_pixel_inspector_last_color) as *mut u32,
    }};

    draw_ctx._init_font_tracker(FONT_PIXEL_3X3_MONO, 2, true, 4);
    draw_ctx._init_font_tracker(FONT_PIXEL_3X3_MONO, 3, true, 4);
    draw_ctx._init_font_tracker(FONT_PIXEL_3X3_MONO, 4, true, 4);
    draw_ctx._init_font_tracker(FONT_PIXEL_3X3_MONO, 5, true, 4);
    draw_ctx._init_font_tracker(FONT_PIXEL_TINY5, 6, true,9);
    draw_ctx._init_font_tracker(FONT_PIXEL_TINY5, 7, true,9);
    draw_ctx._init_font_tracker(FONT_PIXEL_TINY5, 8, true,9);
    draw_ctx._init_font_tracker(FONT_PIXEL_TINY5, 9, true,9);
    draw_ctx._init_font_tracker(FONT_PIXEL_GOHU_11, 10, true, 13);
    draw_ctx._init_font_tracker(FONT_PIXEL_GOHU_11, 11, true, 13);
    draw_ctx._init_font_tracker(FONT_PIXEL_GOHU_11, 12, true, 13);
    draw_ctx._init_font_tracker(FONT_PIXEL_GOHU_11, 13, true, 13);
    draw_ctx._init_font_tracker(FONT_PIXEL_GOHU_14, 14, true, 15);
    draw_ctx._init_font_tracker(FONT_PIXEL_GOHU_14, 15, true, 15);
    draw_ctx._init_font_tracker(FONT_PIXEL_GOHU_14, 16, true, 15);

    let mut gui_ctx = ui::Context::new(ui::Style::dark());
    let mut some_data_to_keep_around = ui::SomeDataToKeepAround{ ..Default::default() };

    let mut t: f64 = 0.0;

    let mut window: Option<Rc<winit::window::Window>> = None;
    let mut softbuffer_context: Option<softbuffer::Context<Rc<winit::window::Window>>> = None;
    let mut softbuffer_surface: Option<softbuffer::Surface<Rc<winit::window::Window>, Rc<winit::window::Window>>> = None;
    event_loop.run(move |event, elwt: &winit::event_loop::ActiveEventLoop| {
        match event {
            winit::event::Event::Resumed => { // Runs at startup and is where we have to do init.
                let twindow = Rc::new(elwt.create_window(
                    winit::window::WindowAttributes::default()
                    .with_maximized(true)
                    .with_title("ZCash Visualizer")
                    .with_inner_size(Size::Physical(winit::dpi::PhysicalSize { width: 1600, height: 900 }))
                ).unwrap());
                let context = softbuffer::Context::new(twindow.clone()).unwrap();
                let surface = softbuffer::Surface::new(&context, twindow.clone()).unwrap();
                window = Some(twindow);
                softbuffer_context = Some(context);
                softbuffer_surface = Some(surface);

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
                                winit::event::WindowEvent::KeyboardInput { device_id, event, is_synthetic } => {
                                    if event.state.is_pressed() && event.repeat == false && event.physical_key == winit::keyboard::PhysicalKey::Code(KeyCode::KeyV) && (input_ctx.key_held(KeyCode::ControlLeft) || input_ctx.key_held(KeyCode::ControlRight))  {
                                        input_ctx.inflight_text_input.extend(&arboard::Clipboard::new().ok().map(|mut c| c.get_text().ok()).flatten().map(|s| s.chars().collect::<Vec<char>>()).unwrap_or_default());
                                    } else {
                                        if let Some(text) = event.text && event.state.is_pressed() {
                                            input_ctx.inflight_text_input.extend(text.chars().filter(|c| *c >= ' ' && *c != 0x7f as char));
                                        }

                                        match event.physical_key {
                                            winit::keyboard::PhysicalKey::Code(kc) => if kc >= winit::keyboard::KeyCode::Eject && kc <= winit::keyboard::KeyCode::Undo {
                                                println!("Skipping key: {:?}", kc);
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
                                            is_anything_happening_at_all_in_any_way |= gui_ctx.debug;
                                            is_anything_happening_at_all_in_any_way |= input_ctx.mouse_moved;
                                            is_anything_happening_at_all_in_any_way |= input_ctx.scroll_delta != (0.0, 0.0);
                                            is_anything_happening_at_all_in_any_way |= input_ctx.zoom_delta != 0.0;

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

                                            // Tell workers: WAKE UP WAKE UP WAKE UP!!!
                                            while (*p_thread_context).workers_that_have_passed_the_wake_up_gate.load(Ordering::Relaxed) != (*p_thread_context).thread_count - 1 { spin_loop(); }
                                            (*p_thread_context).workers_that_have_passed_the_wake_up_gate.store(0, Ordering::Relaxed);
                                            (*p_thread_context).wake_up_gate.store(0, Ordering::Relaxed);
                                            (*p_thread_context).wake_up_barrier.wait();
                                            (*p_thread_context).is_last_time = false;
                                            (*p_thread_context).begin_work_gate.store(0, Ordering::Relaxed);
                                            (*p_thread_context).wake_up_gate.store(1, Ordering::Release);

                                            let target_frame_time_us = (1000000000.0 / (frame_interval_milli_hertz as f64)) as usize;
                                            let begin_frame_instant = Instant::now();

                                            softbuffer_surface.resize((window_width as u32).try_into().unwrap(), (window_height as u32).try_into().unwrap()).unwrap();

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
                                                    if (*take_ptr).how_many_times_was_i_used == 0 && ((*take_ptr).ttf_file == DEJA_VU_SANS_MONO || (*take_ptr).ttf_file == SOURCE_SERIF) {
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

                                            if input_ctx.key_pressed(KeyCode::Space) {
                                                println!("woosh");

                                                play_sound(SOUND_UI_WOOSH, 0.5+0.1*rand::random::<f32>(), 1.0+0.5*rand::random::<f32>());
                                            }

                                            // clear screen
                                            {
                                                let put = draw_ctx.draw_command_buffer.add(*draw_ctx.draw_command_count);
                                                *draw_ctx.draw_command_count += 1;
                                                *put = DrawCommand::ClearScreenToColor { color: 0x111111 };
                                            }

                                            gui_ctx.delta = dt;
                                            gui_ctx.input = &input_ctx;
                                            gui_ctx.draw  = &draw_ctx;

                                            viz_gui_draw_the_stuff_for_the_things(&mut viz_state, &draw_ctx, dt as f32, &input_ctx);

                                            {
                                                let should_quit = demo_of_rendering_stuff_with_context_that_allocates_in_the_background(&mut gui_ctx, &mut some_data_to_keep_around);
                                                if should_quit {
                                                    elwt.exit();
                                                }
                                            }

                                            input_ctx.mouse_moved = false;
                                            input_ctx.last_mouse_pos = input_ctx.this_mouse_pos;
                                            input_ctx.scroll_delta = (0.0, 0.0);
                                            input_ctx.zoom_delta = 0.0;

                                            prev_frame_time_single_threaded_us = begin_frame_instant.elapsed().as_micros() as usize;


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
                                                            let mut hasher = xxhash3_64::Hasher::new();
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
                                                                    DrawCommand::ColoredRectangle { x: ofx, x2: ofx2, y: ofy, y2: ofy2, round_pixels, color } => {
                                                                        let ix = (ofx.floor() as u32).max(tile_pixel_x);
                                                                        let ix2 = (ofx2.ceil() as u32).min(tile_pixel_x2);
                                                                        let iy = (ofy.floor() as u32).max(tile_pixel_y);
                                                                        let iy2 = (ofy2.ceil() as u32).min(tile_pixel_y2);
                                                                        if ix >= ix2 || iy >= iy2 { continue; }
                                                                        hasher.write_u64(0x854893982097);
                                                                        hasher.write_u32(ofx.max(tile_pixel_x as f32 - round_pixels).to_bits());
                                                                        hasher.write_u32(ofx2.min(tile_pixel_x2 as f32 + round_pixels).to_bits());
                                                                        hasher.write_u32(ofy.max(tile_pixel_y as f32 - round_pixels).to_bits());
                                                                        hasher.write_u32(ofy2.min(tile_pixel_y2 as f32 + round_pixels).to_bits());
                                                                        hasher.write_u32(round_pixels.to_bits());
                                                                        hasher.write_u32(color);
                                                                        if should_draw == false { continue; }
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
                                                                                    let lx = _fx-ofx2+round_pixels;
                                                                                    let ly = ofy-(_fy+1.0)+round_pixels;
                                                                                    let v1 = lx*lx.abs()+ly*ly.abs();
                                                                                    let new_alpha = round_pixels - v1.sqrt();
                                                                                    cover_alpha = select_float(v1 >= 0.0, cover_alpha.min(new_alpha), cover_alpha,);
                                                                                }
                                                                                // top right
                                                                                {
                                                                                    let lx = ofx-(_fx+1.0)+round_pixels;
                                                                                    let ly = ofy-(_fy+1.0)+round_pixels;
                                                                                    let v1 = lx*lx.abs()+ly*ly.abs();
                                                                                    let new_alpha = round_pixels - v1.sqrt();
                                                                                    cover_alpha = select_float(v1 >= 0.0, cover_alpha.min(new_alpha), cover_alpha,);
                                                                                }
                                                                                // bottom left
                                                                                {
                                                                                    let lx = _fx-ofx2+round_pixels;
                                                                                    let ly = _fy-ofy2+round_pixels;
                                                                                    let v1 = lx*lx.abs()+ly*ly.abs();
                                                                                    let new_alpha = round_pixels - v1.sqrt();
                                                                                    cover_alpha = select_float(v1 >= 0.0, cover_alpha.min(new_alpha), cover_alpha,);
                                                                                }
                                                                                // bottom right
                                                                                {
                                                                                    let lx = ofx-(_fx+1.0)+round_pixels;
                                                                                    let ly = _fy-ofy2+round_pixels;
                                                                                    let v1 = lx*lx.abs()+ly*ly.abs();
                                                                                    let new_alpha = round_pixels - v1.sqrt();
                                                                                    cover_alpha = select_float(v1 >= 0.0, cover_alpha.min(new_alpha), cover_alpha,);
                                                                                }
                                                                                let pixel_alpha = color_alpha * cover_alpha;

                                                                                *(cursor_pixels as *mut u32) = blend_u32(*(cursor_pixels as *mut u32), color,  (pixel_alpha * 255.0).round() as u32);
                                                                                cursor_pixels = cursor_pixels.byte_add(4);
                                                                            }
                                                                            row_pixels = row_pixels.byte_add(4 << pixel_row_shift);
                                                                        }
                                                                    },
                                                                    DrawCommand::TextRow { y, glyph_row_shift, color, font_tracker_id, font_row_index, glyph_bitmap_run, glyph_bitmap_run_len } => {
                                                                        if (y as u32) < tile_pixel_y || (y as u32) >= tile_pixel_y2 { continue; }

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
                                                                            for _ in 0..len {
                                                                                let blend = *copy_data as u32;
                                                                                copy_data = copy_data.byte_add(1);
                                                                                *(put_data as *mut u32) = blend_u32(*(put_data as *mut u32), color, blend);
                                                                                put_data = put_data.byte_add(4);
                                                                            }
                                                                        }
                                                                    },
                                                                    DrawCommand::PixelLineXDef { x1, y1, x2, y2, color, thickness, } => {
                                                                        let x1 = x1 as i16;
                                                                        let y1 = y1 as i16;
                                                                        let x2 = x2 as i16;
                                                                        let y2 = y2 as i16;
                                                                        let start_x = (x1 as isize).max(tile_pixel_x as isize);
                                                                        let end_x = (x2 as isize).min(tile_pixel_x2 as isize);
                                                                        if start_x >= end_x { continue; }
                                                                        let dy = (y2 as f32 - y1 as f32) / x2.wrapping_sub(x1) as f32;
                                                                        hasher.write_u64(0x75634593484);
                                                                        hasher.write_i16(x1);
                                                                        hasher.write_i16(x2);
                                                                        hasher.write_i16(y1);
                                                                        hasher.write_i16(y2);
                                                                        hasher.write_u32(color);
                                                                        hasher.write_u32(dy.to_bits());
                                                                        if should_draw == false { continue; }
                                                                        if thickness <= 1.0 {
                                                                            for real_x in start_x..end_x {
                                                                                let fy = y1 as f32 + dy * (real_x - x1 as isize) as f32;
                                                                                let iy1 = fy.floor() as isize;
                                                                                let iy2 = iy1+1;
                                                                                let coverage1 = (iy2 as f32 - fy).clamp(0.0, thickness);
                                                                                let coverage2 = thickness - coverage1;
                                                                                let blend1 = 255.0 * coverage1;
                                                                                let blend2 = 255.0 * coverage2;

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
                                                                            let thickness = thickness - 0.0001 * (thickness.ceil() == thickness.round()) as u32 as f32;
                                                                            let whole_pixels = thickness.floor();
                                                                            let whole_pixels_i = whole_pixels as isize;
                                                                            for real_x in start_x..end_x {
                                                                                let fy = y1 as f32 + dy * (real_x - x1 as isize) as f32 - thickness/2.0;
                                                                                let iy1 = fy.floor() as isize;
                                                                                let iy2 = iy1+1;
                                                                                let coverage1 = (iy2 as f32 - fy).clamp(0.0, thickness - thickness.floor());
                                                                                let coverage2 = (thickness - thickness.floor()) - coverage1;
                                                                                let blend1 = 255.0 * coverage1;
                                                                                let blend2 = 255.0 * coverage2;

                                                                                let iy2 = iy2 + whole_pixels_i;

                                                                                if iy1 >= tile_pixel_y as isize && iy1 < tile_pixel_y2 as isize {
                                                                                    let pixel = ctx.render_target_0.byte_add(((real_x + (iy1 << pixel_row_shift)) as usize) << 2);
                                                                                    let this_color = blend_u32(*(pixel as *mut u32), color, blend1 as u32);
                                                                                    *(pixel as *mut u32) = blend_u32(*(pixel as *mut u32), this_color, color >> 24);
                                                                                }
                                                                                for y_mid in iy1.max(tile_pixel_y as isize - 1)+1..iy2.min(tile_pixel_y2 as isize + 1) {
                                                                                    if y_mid >= tile_pixel_y as isize && y_mid < tile_pixel_y2 as isize {
                                                                                        let pixel = ctx.render_target_0.byte_add(((real_x + (y_mid << pixel_row_shift)) as usize) << 2);
                                                                                        let this_color = color;
                                                                                        *(pixel as *mut u32) = blend_u32(*(pixel as *mut u32), this_color, color >> 24);
                                                                                    }
                                                                                }
                                                                                if iy2 >= tile_pixel_y as isize && iy2 < tile_pixel_y2 as isize {
                                                                                    let pixel = ctx.render_target_0.byte_add(((real_x + (iy2 << pixel_row_shift)) as usize) << 2);
                                                                                    let this_color = blend_u32(*(pixel as *mut u32), color, blend2 as u32);
                                                                                    *(pixel as *mut u32) = blend_u32(*(pixel as *mut u32), this_color, color >> 24);
                                                                                }
                                                                            }
                                                                        }
                                                                    },
                                                                    DrawCommand::PixelLineYDef { x1, y1, x2, y2, color, thickness, } => {
                                                                        let x1 = x1 as i16;
                                                                        let y1 = y1 as i16;
                                                                        let x2 = x2 as i16;
                                                                        let y2 = y2 as i16;
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
                                                                        if thickness <= 1.0 {
                                                                            for real_y in start_y..end_y {
                                                                                let fx = x1 as f32 + dx * (real_y - y1 as isize) as f32;
                                                                                let ix1 = fx.floor() as isize;
                                                                                let ix2 = ix1+1;
                                                                                let coverage1 = (ix2 as f32 - fx).clamp(0.0, thickness);
                                                                                let coverage2 = thickness - coverage1;
                                                                                let blend1 = 255.0 * coverage1;
                                                                                let blend2 = 255.0 * coverage2;

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
                                                                            let thickness = thickness - 0.0001 * (thickness.ceil() == thickness.round()) as u32 as f32;
                                                                            let whole_pixels = thickness.floor();
                                                                            let whole_pixels_i = whole_pixels as isize;
                                                                            for real_y in start_y..end_y {
                                                                                let fx = x1 as f32 + dx * (real_y - y1 as isize) as f32 - thickness/2.0;
                                                                                let ix1 = fx.floor() as isize;
                                                                                let ix2 = ix1+1;
                                                                                let coverage1 = (ix2 as f32 - fx).clamp(0.0, thickness - thickness.floor());
                                                                                let coverage2 = (thickness - thickness.floor()) - coverage1;
                                                                                let blend1 = 255.0 * coverage1;
                                                                                let blend2 = 255.0 * coverage2;

                                                                                let ix2 = ix2 + whole_pixels_i;

                                                                                if ix1 >= tile_pixel_x as isize && ix1 < tile_pixel_x2 as isize {
                                                                                    let pixel = ctx.render_target_0.byte_add(((ix1 + (real_y << pixel_row_shift)) as usize) << 2);
                                                                                    let this_color = blend_u32(*(pixel as *mut u32), color, blend1 as u32);
                                                                                    *(pixel as *mut u32) = blend_u32(*(pixel as *mut u32), this_color, color >> 24);
                                                                                }
                                                                                for x_mid in ix1.max(tile_pixel_x as isize - 1)+1..ix2.min(tile_pixel_x2 as isize + 1) {
                                                                                    if x_mid >= tile_pixel_x as isize && x_mid < tile_pixel_x2 as isize {
                                                                                        let pixel = ctx.render_target_0.byte_add(((x_mid + (real_y << pixel_row_shift)) as usize) << 2);
                                                                                        let this_color = color;
                                                                                        *(pixel as *mut u32) = blend_u32(*(pixel as *mut u32), this_color, color >> 24);
                                                                                    }
                                                                                }
                                                                                if ix2 >= tile_pixel_x as isize && ix2 < tile_pixel_x2 as isize {
                                                                                    let pixel = ctx.render_target_0.byte_add(((ix2 + (real_y << pixel_row_shift)) as usize) << 2);
                                                                                    let this_color = blend_u32(*(pixel as *mut u32), color, blend2 as u32);
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
                                                let mut hasher = xxhash3_64::Hasher::new();
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

                                            prev_frame_time_total_us_max_5_seconds = prev_frame_time_total_us_max_5_seconds.max(prev_frame_time_total_us);

                                            if let Some((x, y)) = *draw_ctx.debug_pixel_inspector {
                                                let pixel_src = render_target_0.byte_add((x+y*draw_area_pixel_wide)*4);
                                                *draw_ctx.debug_pixel_inspector_last_color = *(pixel_src as *mut u32);
                                            }

                                            if gui_ctx.debug {
                                                let begin_draw_commands = *draw_ctx.draw_command_count;
                                                draw_ctx.mono_text_line(8.0, 8.0, 13.0,
                                                    &format!(
                                                        "Rate: {} hz | (us) deadline: {} internal:{:>5} total:{:>5} max(5s):{:>5}",
                                                        frame_interval_milli_hertz as f32 / 1000.0,
                                                        target_frame_time_us,
                                                        prev_frame_time_us,
                                                        prev_frame_time_total_us,
                                                        prev_frame_time_total_us_max_5_seconds
                                                    ),
                                                    if prev_frame_time_total_us < target_frame_time_us { 0xff_00ff00 } else { 0xff_ff5500 },
                                                );
                                                let end_draw_commands = *draw_ctx.draw_command_count;
                                                *draw_ctx.draw_command_count = begin_draw_commands;

                                                // Force a non measured render buffer reset if the real code is not active to save CPU.
                                                if need_buffer_flip == false
                                                {
                                                    for row in 0..window_height {
                                                        let pixel_src = render_target_0.byte_add(row*draw_area_pixel_wide*4) as *mut u8;
                                                        let pixel_dst = final_output_blit_buffer.byte_add(row*window_width*4) as *mut u8;
                                                        copy_nonoverlapping(pixel_src, pixel_dst, window_width*4);
                                                    }
                                                }

                                                // draw those commands *manually* that are in the unallocated space of the buffer
                                                for cmd_i in begin_draw_commands..end_draw_commands {
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
                                                        for _ in 0..single_w {
                                                            *put_ptr = single_color;
                                                            put_ptr = put_ptr.byte_add(4);
                                                        }
                                                        for _ in 0..work_w {
                                                            *put_ptr = work_color;
                                                            put_ptr = put_ptr.byte_add(4);
                                                        }
                                                        for _ in 0..full_w {
                                                            *put_ptr = full_color;
                                                            put_ptr = put_ptr.byte_add(4);
                                                        }
                                                    }
                                                }
                                            }

                                            if prev_frame_time_total_us_max_5_seconds_last_reset.elapsed().as_secs() >= 5 {
                                                prev_frame_time_total_us_max_5_seconds = 0;
                                                prev_frame_time_total_us_max_5_seconds_last_reset = Instant::now();
                                            }

                                            // frame pace is not interesting for profiling
                                            let frame_pace_us = last_call_to_present_instant.elapsed().as_micros() as u64;
                                            last_call_to_present_instant = Instant::now();

                                            let need_buffer_flip = need_buffer_flip || gui_ctx.debug;
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
