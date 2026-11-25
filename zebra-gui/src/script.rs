//! A window with no display behind it, driven by a script instead of a person.
//!
//! The script feeds the frame loop the same softer_gui events a real window would, so input
//! decoding, layout and rasterisation all run exactly as they do on screen. What comes out is
//! PNG screenshots, the text each frame drew, and frame timings. GUI_DEV.md is the manual.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

use softer_gui::{
    AxisDiff, Event, Framebuffer, AXIS_MOUSE_X, AXIS_MOUSE_Y, AXIS_SCROLL_H, AXIS_SCROLL_V,
    AXIS_WHEEL_H, AXIS_WHEEL_V, AXIS_ZOOM, BTN_LEFT, BTN_MIDDLE, BTN_RIGHT, EVENT_AXES,
    EVENT_BUTTONS, EVENT_CLOSE, EVENT_RENDER, EVENT_TEXT, WHEEL_CLICK,
};

use crate::CursorIcon;

/// Checks that failed (`expect`, a `click` on text that is not there, an unknown command) over
/// the life of the process. A scripted run's exit code comes from this.
pub static FAILURES: AtomicUsize = AtomicUsize::new(0);

pub struct LoggedText {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
    pub color: u32,
    /// How many draw commands the frame had issued before this text. It places the text among
    /// the rectangles and clip changes, which come from the command buffer itself.
    pub at: usize,
    pub text: String,
}

struct LoggedRect {
    /// Already cut down to the clip it was drawn under.
    b: [i32; 4],
    color: u32,
    at: usize,
    radius: u32,
}

/// Something the last frame let the pointer press: its box, and what its widget said of it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct LoggedTarget {
    pub b: [i32; 4],
    pub enabled: bool,
    /// Whether the widget asks for the hand pointer while the pointer is on it.
    pub pointer: bool,
    /// Its name, if it is dragged to resize what is beside it and not pressed.
    pub grip: String,
}

/// A text a person can see, with its whole box and the part of it inside its clip.
struct Seen<'a> { t: &'a LoggedText, full: [i32; 4], vis: [i32; 4] }

/// `targets` in progress: the pointer visits each target in turn to see how it answers.
struct Sweep {
    list: Vec<LoggedTarget>,
    i: usize,
    /// Whether the frames just drawn had the pointer on the target, or parked off every target.
    on: bool,
    rest: (i32, i32),
    home: (i32, i32),
    before: Vec<u32>,
    rows: Vec<String>,
    quiet: usize,
}

pub struct FrameReport {
    pub single_us: usize,
    pub total_us: usize,
    pub draw_commands: usize,
    pub cursor: CursorIcon,
}

struct Bench {
    left: usize,
    full: bool,
    samples: Vec<(usize, usize)>,
}

struct Monkey {
    left: usize,
    rng: u64,
}

/// A command that draws many frames and looks at each: `settle`, `film`, `track`.
struct Watch {
    left: usize,
    started: bool,
    prev: Vec<u32>,
    changed: Vec<usize>,
    still: usize,
    cells: Vec<Vec<u8>>,
    spots: Vec<Option<(i32, i32)>>,
}

/// `resize` or `drag` in progress. The window's size, or the pointer with its button held,
/// moves a step at a time, and every step is drawn twice with nothing changed in between: the
/// first frame is what a person sees mid-drag, the second is what that size comes to rest at.
struct Reflow {
    window: bool,
    from: (i32, i32),
    to: (i32, i32),
    count: usize,
    /// How much smaller than life the film shows each step.
    shrink: usize,
    /// 0 before a step's first frame is asked for, 1 once it is drawn, 2 once its second is.
    phase: u8,
    first: Vec<Shown>,
    first_us: usize,
    steps: Vec<Step>,
}

/// One step of a `resize` or `drag`, as its second frame showed it.
struct Step {
    /// The window's size, or where the pointer was.
    at: (i32, i32),
    texts: Vec<Shown>,
    shown: usize,
    faults: Vec<Fault>,
    /// The texts the first frame drew somewhere else, and the one furthest off.
    trailing: (usize, Option<(String, (i32, i32))>),
    us: usize,
    cell: (Vec<u8>, usize, usize),
}

/// A lint finding as a sweep follows it. `key` is what makes it the same fault at another step.
struct Fault { key: String, kind: &'static str, text: String, detail: String, b: [i32; 4] }

/// A fault over a sweep: the first and last steps it held at, how many it held at, and the
/// step it was at its worst.
#[derive(Debug, Clone, PartialEq)]
struct Span { key: String, first: usize, last: usize, held: usize, worst: usize }

fn rng_next(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn fresh_seed() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1) | 1
}

pub struct ScriptWindow {
    lines: Vec<String>,
    pc: usize,
    /// Commands `monkey` made up, run before the script's next line.
    injected: VecDeque<String>,
    monkey: Option<Monkey>,
    out_dir: PathBuf,
    events: VecDeque<Event>,
    closed: bool,

    pixels: Vec<u32>,
    side: usize,
    width: usize,
    height: usize,
    scale: f32,
    generation: u64,

    bits: [u64; 8],
    mouse: (i32, i32),

    /// Whether a frame has been drawn since the last input, so what `texts` and the pixels
    /// show is what the script's commands so far lead to.
    fresh: bool,
    renders_without_frame: usize,
    frame_count: usize,
    pub texts: Vec<LoggedText>,
    rects: Vec<LoggedRect>,
    /// Each clip change of the last frame: the command count it came at, and the clip.
    clips: Vec<(usize, [i32; 4])>,
    report: FrameReport,
    bench: Option<Bench>,
    watch: Option<Watch>,
    lint_lines: Vec<String>,
    targets: Vec<LoggedTarget>,
    style_lines: Vec<String>,
    content_lines: Vec<String>,
    sweep: Option<Sweep>,
    reflow: Option<Reflow>,
}

fn fail(msg: &str) {
    FAILURES.fetch_add(1, Ordering::Relaxed);
    println!("FAIL {}", msg);
}

/// Private-use characters are icon-font glyphs; they are logged as `<e809>` so a script and a
/// reader can both name them in plain text.
pub fn loggable(text: &str) -> String {
    let mut s = String::with_capacity(text.len());
    for c in text.chars() {
        if ('\u{e000}'..='\u{f8ff}').contains(&c) { s.push_str(&format!("<{:x}>", c as u32)); } else { s.push(c); }
    }
    s
}

fn key_code(name: &str) -> Option<u32> {
    const LETTERS: &str = "qwertyuiop";
    const LETTERS2: &str = "asdfghjkl";
    const LETTERS3: &str = "zxcvbnm";
    if name.len() == 1 {
        let c = name.chars().next().unwrap().to_ascii_lowercase();
        if let Some(i) = LETTERS.find(c)  { return Some(16 + i as u32); }
        if let Some(i) = LETTERS2.find(c) { return Some(30 + i as u32); }
        if let Some(i) = LETTERS3.find(c) { return Some(44 + i as u32); }
        if c == '0' { return Some(11); }
        if let Some(d) = c.to_digit(10) { return Some(1 + d); }
    }
    Some(match name.to_ascii_lowercase().as_str() {
        "esc" | "escape" => 1, "minus" => 12, "equal" => 13, "backspace" => 14, "tab" => 15,
        "enter" => 28, "ctrl" => 29, "shift" => 42, "alt" => 56, "space" => 57,
        "f1" => 59, "f2" => 60, "f3" => 61, "f4" => 62, "f5" => 63, "f6" => 64, "f7" => 65,
        "f8" => 66, "f9" => 67, "f10" => 68, "f11" => 87, "f12" => 88,
        "home" => 102, "up" => 103, "pageup" => 104, "left" => 105, "right" => 106, "end" => 107,
        "down" => 108, "pagedown" => 109, "insert" => 110, "delete" => 111,
        "mouse" | "mouseleft" => BTN_LEFT, "mouseright" => BTN_RIGHT, "mousemiddle" => BTN_MIDDLE,
        _ => return None,
    })
}

/// Splits a command's arguments on spaces, keeping a "quoted string" as one argument.
fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut any = false;
    for c in s.chars() {
        if c == '"' { quoted = !quoted; any = true; }
        else if c.is_whitespace() && !quoted { if any { out.push(std::mem::take(&mut cur)); any = false; } }
        else { cur.push(c); any = true; }
    }
    if any { out.push(cur); }
    out
}

impl ScriptWindow {
    /// `script` is a file of commands, one per line, or the commands themselves separated by `;`.
    pub fn open(script: &str, out_dir: &str) -> Option<ScriptWindow> {
        let text = match std::fs::read_to_string(script) {
            Ok(t) => t,
            Err(_) if !Path::new(script).exists() => script.replace(';', "\n"),
            Err(e) => { eprintln!("{}: {}", script, e); return None; }
        };
        let mut w = ScriptWindow {
            lines: text.lines().map(|l| l.trim().to_string()).collect(),
            pc: 0,
            injected: VecDeque::new(),
            monkey: None,
            out_dir: PathBuf::from(if out_dir.is_empty() { "." } else { out_dir }),
            events: VecDeque::new(),
            closed: false,
            pixels: Vec::new(), side: 0, width: 0, height: 0, scale: 1.0, generation: 0,
            bits: [0; 8],
            mouse: (0, 0),
            fresh: false,
            renders_without_frame: 0,
            frame_count: 0,
            texts: Vec::new(),
            rects: Vec::new(),
            clips: Vec::new(),
            report: FrameReport { single_us: 0, total_us: 0, draw_commands: 0, cursor: CursorIcon::Default },
            bench: None,
            watch: None,
            lint_lines: Vec::new(),
            targets: Vec::new(),
            style_lines: Vec::new(),
            content_lines: Vec::new(),
            sweep: None,
            reflow: None,
        };
        w.resize(1600, 900);
        report_crashes();
        Some(w)
    }

    fn resize(&mut self, width: usize, height: usize) {
        let side = softer_gui::next_pow2(width.max(height) as u32) as usize;
        if side != self.side {
            self.side = side;
            self.pixels = vec![0xff000000; side * side];
            self.generation += 1;
        }
        self.width = width;
        self.height = height;
        self.fresh = false;
    }

    pub fn get_framebuffer(&mut self) -> Framebuffer {
        Framebuffer { pixels: self.pixels.as_mut_ptr(), side: self.side, width: self.width, height: self.height, key: self.generation << 2 }
    }

    /// The frame loop calls this once per drawn frame, handing over the text that frame drew.
    pub(crate) fn frame_done(&mut self, texts: &mut Vec<LoggedText>, commands: &[crate::DrawCommand], targets: &[LoggedTarget], report: FrameReport) {
        std::mem::swap(&mut self.texts, texts);
        texts.clear();
        self.targets.clear();
        self.targets.extend_from_slice(targets);
        let window = [0, 0, self.width as i32, self.height as i32];
        let mut clip = window;
        self.rects.clear();
        self.clips.clear();
        self.clips.push((0, clip));
        for (at, c) in commands.iter().enumerate() {
            match c {
                crate::DrawCommand::Scissor { x1, y1, x2, y2 } => {
                    clip = cut([*x1 as i32, *y1 as i32, *x2 as i32, *y2 as i32], window);
                    self.clips.push((at, clip));
                }
                crate::DrawCommand::RoundedRectangle { x, x2, y, y2, color, radius_tl, radius_tr, radius_bl, radius_br } => {
                    let b = cut([*x as i32, *y as i32, *x2 as i32, *y2 as i32], clip);
                    let radius = (*radius_tl).max(*radius_tr).max(*radius_bl).max(*radius_br).max(0) as u32;
                    if area(b) > 0 { self.rects.push(LoggedRect { b, color: *color, at, radius }); }
                }
                crate::DrawCommand::ClearScreenToColor { color } => self.rects.push(LoggedRect { b: window, color: *color, at, radius: 0 }),
                _ => {}
            }
        }
        if let Some(b) = self.bench.as_mut() { b.samples.push((report.single_us, report.total_us)); }
        self.report = report;
        self.fresh = true;
        self.renders_without_frame = 0;
        self.frame_count += 1;
    }

    pub fn next_event(&mut self, ev: &mut Event) -> bool {
        HEARTBEAT.fetch_add(1, Ordering::Relaxed);
        loop {
            if let Some(e) = self.events.pop_front() { *ev = e; return true; }
            if self.closed { return false; }
            if self.pc >= self.lines.len() {
                if !self.lint_lines.is_empty() {
                    let mut text = self.lint_lines.join("\n");
                    text.push('\n');
                    self.write_file("lint.txt", text.as_bytes());
                    println!("lint: {} findings -> {}", self.lint_lines.len(), self.out_dir.join("lint.txt").display());
                }
                if !self.style_lines.is_empty() {
                    let mut text = self.style_lines.join("\n");
                    text.push('\n');
                    self.write_file("style.txt", text.as_bytes());
                }
                if !self.content_lines.is_empty() {
                    let mut text = self.content_lines.join("\n");
                    text.push('\n');
                    self.write_file("content.txt", text.as_bytes());
                }
                self.closed = true;
                self.events.push_back(Event { kind: EVENT_CLOSE, ..Event::default() });
                continue;
            }
            if let Some(line) = self.injected.front().cloned() {
                note_line(self.pc, &line);
                if self.run(&line) { self.injected.pop_front(); }
                continue;
            }
            let line = self.lines[self.pc].clone();
            note_line(self.pc, &line);
            if self.run(&line) { self.pc += 1; }
        }
    }

    fn push_render(&mut self) {
        self.events.push_back(Event {
            kind: EVENT_RENDER,
            width: self.width as u32,
            height: self.height as u32,
            scale_fx: (self.scale * 65536.0) as u32,
            cursor_x: (self.mouse.0 as i64) << 32,
            cursor_y: (self.mouse.1 as i64) << 32,
            dt_fs: 1_000_000_000_000_000 / 60,
            ..Event::default()
        });
    }

    fn push_axes(&mut self, diffs: &[AxisDiff]) {
        let mut e = Event { kind: EVENT_AXES, ..Event::default() };
        for d in diffs { e.axes[e.axis_count as usize] = *d; e.axis_count += 1; }
        self.events.push_back(e);
        self.fresh = false;
    }

    fn push_move(&mut self, x: i32, y: i32) {
        self.mouse = (x, y);
        self.push_axes(&[AxisDiff { axis: AXIS_MOUSE_X, delta: x << 8 }, AxisDiff { axis: AXIS_MOUSE_Y, delta: y << 8 }]);
    }

    fn push_button(&mut self, code: u32, down: bool) {
        let (word, bit) = ((code as usize >> 6) & 7, 1u64 << (code & 63));
        if down { self.bits[word] |= bit; } else { self.bits[word] &= !bit; }
        self.events.push_back(Event { kind: EVENT_BUTTONS, bits: self.bits, ..Event::default() });
        self.fresh = false;
    }

    /// True once a drawn frame reflects every input so far. Otherwise asks for one and returns
    /// false, and the caller's command runs again after it.
    fn need_fresh(&mut self) -> bool {
        if self.fresh { return true; }
        if self.renders_without_frame >= 3 {
            fail("the frame loop drew nothing for three frames");
            self.renders_without_frame = 0;
            return true;
        }
        self.renders_without_frame += 1;
        self.push_render();
        false
    }

    /// Text that is exactly `needle` wins over text that only contains it, so "Stake" finds
    /// the button and not "0.000 cTAZ Staked".
    fn find_text(&self, needle: &str, nth: usize) -> Option<&LoggedText> {
        if self.texts.iter().any(|t| t.text == needle) {
            return self.texts.iter().filter(|t| t.text == needle).nth(nth);
        }
        self.texts.iter().filter(|t| t.text.contains(needle)).nth(nth)
    }

    /// Runs one command. Returns false when the command must run again after the events it
    /// queued (it needed a frame drawn first, or is a multi-frame `bench`).
    fn run(&mut self, line: &str) -> bool {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') { return true; }
        let (cmd, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let rest = rest.trim();
        let args = split_args(rest);
        let num = |i: usize| -> Option<f64> { args.get(i).and_then(|a| a.parse::<f64>().ok()) };

        match cmd {
            "size" => match (num(0), num(1)) {
                (Some(w), Some(h)) if w >= 1.0 && h >= 1.0 && w <= 8192.0 && h <= 8192.0 => self.resize(w as usize, h as usize),
                _ => fail(&format!("line {}: size <width> <height>", self.pc + 1)),
            },
            "scale" => match num(0) {
                Some(s) if s > 0.0 => { self.scale = s as f32; self.fresh = false; }
                _ => fail(&format!("line {}: scale <factor>", self.pc + 1)),
            },
            "frame" => for _ in 0..num(0).unwrap_or(1.0) as usize { self.push_render(); },
            "sleep" => std::thread::sleep(std::time::Duration::from_millis(num(0).unwrap_or(0.0) as u64)),
            "log" => println!("{}", rest),

            "move" => match (num(0), num(1)) {
                (Some(x), Some(y)) => self.push_move(x as i32, y as i32),
                _ => fail(&format!("line {}: move <x> <y>", self.pc + 1)),
            },
            "down" | "up" => match key_code(args.first().map(|s| s.as_str()).unwrap_or("mouse")) {
                Some(code) => self.push_button(code, cmd == "down"),
                None => fail(&format!("line {}: unknown key or button {:?}", self.pc + 1, rest)),
            },
            "key" => match key_code(rest) {
                Some(code) => {
                    self.push_button(code, true);
                    self.push_render();
                    self.push_button(code, false);
                    self.push_render();
                    self.fresh = false;
                }
                None => fail(&format!("line {}: unknown key {:?}", self.pc + 1, rest)),
            },
            "type" => {
                let chars: Vec<char> = rest.chars().collect();
                for chunk in chars.chunks(19) {
                    let mut e = Event { kind: EVENT_TEXT, text_len: chunk.len() as u8, ..Event::default() };
                    e.text[..chunk.len()].copy_from_slice(chunk);
                    self.events.push_back(e);
                }
                self.fresh = false;
            }
            "wheel" => {
                let (dy, dx) = (num(0).unwrap_or(0.0), num(1).unwrap_or(0.0));
                self.push_axes(&[AxisDiff { axis: AXIS_WHEEL_V, delta: (-dy * WHEEL_CLICK as f64) as i32 },
                                 AxisDiff { axis: AXIS_WHEEL_H, delta: (-dx * WHEEL_CLICK as f64) as i32 }]);
            }
            "scroll" => {
                let (dx, dy) = (num(0).unwrap_or(0.0), num(1).unwrap_or(0.0));
                self.push_axes(&[AxisDiff { axis: AXIS_SCROLL_H, delta: (-dx * 256.0) as i32 },
                                 AxisDiff { axis: AXIS_SCROLL_V, delta: (-dy * 256.0) as i32 }]);
            }
            "zoom" => self.push_axes(&[AxisDiff { axis: AXIS_ZOOM, delta: (num(0).unwrap_or(0.0) * 65536.0) as i32 }]),

            "click" | "hover" => {
                let (x, y) = if let (Some(x), Some(y)) = (num(0), num(1)) { (x as i32, y as i32) } else {
                    if !self.need_fresh() { return false; }
                    let needle = args.first().cloned().unwrap_or_default();
                    let nth = num(1).unwrap_or(1.0).max(1.0) as usize - 1;
                    match self.find_text(&needle, nth) {
                        Some(t) => (t.x + t.h as i32 / 2, t.y + t.h as i32 / 2),
                        None => {
                            fail(&format!("line {}: {} found no text containing {:?} (match {})", self.pc + 1, cmd, needle, nth + 1));
                            return true;
                        }
                    }
                };
                // The hover has to be drawn before the press: a widget only takes a press
                // once layout has seen the pointer over it.
                self.push_move(x, y);
                self.push_render();
                if cmd == "click" {
                    self.push_button(BTN_LEFT, true);
                    self.push_render();
                    self.push_button(BTN_LEFT, false);
                    self.push_render();
                }
                self.fresh = false;
            }

            "expect" | "expect_not" => {
                if !self.need_fresh() { return false; }
                let needle = args.first().cloned().unwrap_or_default();
                let found = self.find_text(&needle, 0).is_some();
                if found != (cmd == "expect") {
                    fail(&format!("line {}: {} {:?}", self.pc + 1, cmd, needle));
                }
            }
            "expect_cursor" => {
                if !self.need_fresh() { return false; }
                let got = format!("{:?}", self.report.cursor);
                if !got.eq_ignore_ascii_case(rest) { fail(&format!("line {}: expect_cursor {} but the cursor is {}", self.pc + 1, rest, got)); }
            }
            "find" | "dump" => {
                if !self.need_fresh() { return false; }
                let needle = if cmd == "find" { args.first().cloned().unwrap_or_default() } else { String::new() };
                let mut rows: Vec<&LoggedText> = self.texts.iter().filter(|t| t.text.contains(&needle)).collect();
                rows.sort_by_key(|t| (t.y, t.x));
                let mut s = format!("frame {} {}x{} cursor={:?} draw_commands={} single_us={} total_us={}\n",
                    self.frame_count, self.width, self.height, self.report.cursor, self.report.draw_commands, self.report.single_us, self.report.total_us);
                for t in rows { s.push_str(&format!("{:>5} {:>5} {:>3} {:06x} {}\n", t.x, t.y, t.h, t.color & 0xffffff, t.text)); }
                match (cmd, args.first()) {
                    ("dump", Some(name)) => self.write_file(name, s.as_bytes()),
                    _ => print!("{}", s),
                }
            }
            "shot" => {
                if !self.need_fresh() { return false; }
                let name = args.first().cloned().unwrap_or_else(|| format!("shot{}", self.frame_count));
                let (mut x, mut y, mut w, mut h) = (0, 0, self.width, self.height);
                if let (Some(cx), Some(cy), Some(cw), Some(ch)) = (num(1), num(2), num(3), num(4)) {
                    x = (cx as usize).min(self.width);
                    y = (cy as usize).min(self.height);
                    w = (cw as usize).min(self.width - x);
                    h = (ch as usize).min(self.height - y);
                }
                let mut rgb = Vec::with_capacity(w * h * 3);
                for row in y..y + h {
                    for px in &self.pixels[row * self.side + x..row * self.side + x + w] {
                        rgb.extend_from_slice(&[(px >> 16) as u8, (px >> 8) as u8, *px as u8]);
                    }
                }
                self.keep_shot(&name, &rgb, w, h);
                let found = self.lint();
                self.keep_lint(&name, &found);
                let part = [x as i32, y as i32, (x + w) as i32, (y + h) as i32];
                for (kind, value, uses) in self.style(part) { self.style_lines.push(format!("{}\t{}\t{}\t{}", name, kind, value, uses)); }
                let shown = self.content(&name, part);
                self.content_lines.extend(shown);
            }
            "style" => {
                if !self.need_fresh() { return false; }
                let all = self.style([0, 0, self.width as i32, self.height as i32]);
                for kind in ["text size", "text colour", "fill", "corner radius"] {
                    let of: Vec<String> = all.iter().filter(|s| s.0 == kind).map(|s| format!("{} x{}", s.1, s.2)).collect();
                    let more = if of.len() > 16 { format!(" ... and {} more", of.len() - 16) } else { String::new() };
                    println!("style {} ({} values): {}{}", kind, of.len(), of.iter().take(16).cloned().collect::<Vec<_>>().join(", "), more);
                }
            }
            "squint" => {
                if !self.need_fresh() { return false; }
                let name = args.first().cloned().unwrap_or_else(|| format!("frame{}", self.frame_count));
                let f = self.width.div_ceil(400).max(1);
                let (rgb, w, h) = self.small([0, 0, self.width, self.height], f);
                let file = format!("squint_{}.png", name);
                self.write_file(&file, &png_encode(&rgb, w, h));
                // What pulls the eye first is big text that stands out from what is behind it.
                // Past 7:1 more contrast adds little.
                let seen = self.shown_texts();
                let mut pull: Vec<(f64, f64, &LoggedText)> = seen.iter().filter_map(|s| {
                    let ink = s.t.color & 0xffffff;
                    let ratio = contrast(ink, self.background(s.vis, ink)?);
                    Some((s.t.h as f64 * ratio.min(7.0), ratio, s.t))
                }).collect();
                pull.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
                for (i, (_, ratio, t)) in pull.iter().take(8).enumerate() {
                    println!("squint {}. {:>3} px {:>4.1}:1 at {:>5},{:<5} {:?}", i + 1, t.h, ratio, t.x, t.y, short(&t.text));
                }
                println!("squint {}: the screen at 1/{} size is {}. Listed is the text that pulls the eye first, by height and contrast: it should be what matters most here, in that order.",
                    name, f, self.out_dir.join(&file).display());
            }
            "targets" => {
                let window = [0, 0, self.width as i32, self.height as i32];
                if self.sweep.is_none() {
                    if !self.need_fresh() { return false; }
                    let mut list = self.shown_targets();
                    list.truncate(120);
                    // Somewhere to park the pointer that is on no target.
                    let mut rest = (0, 0);
                    'find: for y in (4..self.height as i32).step_by(16) {
                        for x in (4..self.width as i32).step_by(16) {
                            if !self.targets.iter().any(|t| holds(t.b, [x, y])) { rest = (x, y); break 'find; }
                        }
                    }
                    self.sweep = Some(Sweep { list, i: 0, on: false, rest, home: self.mouse, before: Vec::new(), rows: Vec::new(), quiet: 0 });
                    self.push_move(rest.0, rest.1);
                    for _ in 0..3 { self.push_render(); }
                    return false;
                }
                let mut s = self.sweep.take().unwrap();
                if s.i < s.list.len() {
                    let t = s.list[s.i].clone();
                    let b = cut(t.b, window);
                    let mut now: Vec<u32> = Vec::new();
                    for y in b[1]..b[3] {
                        let row = y as usize * self.side;
                        now.extend(self.pixels[row + b[0] as usize..row + b[2] as usize].iter().map(|p| p & 0xffffff));
                    }
                    if !s.on {
                        s.before = now;
                        self.push_move((b[0] + b[2]) / 2, (b[1] + b[3]) / 2);
                    } else {
                        let changed = now.iter().zip(&s.before).filter(|(a, b)| a != b).count();
                        if changed == 0 { s.quiet += 1; }
                        s.rows.push(format!("{:>5},{:<5} {:>4}x{:<4} cursor {:<10} hover changes {:>6} px{}  {:?}", t.b[0], t.b[1], t.b[2] - t.b[0], t.b[3] - t.b[1],
                            format!("{:?}", self.report.cursor).to_lowercase(), changed, if t.enabled { "" } else { " (disabled)" }, short(&self.label_of(t.b))));
                        self.push_move(s.rest.0, s.rest.1);
                        s.i += 1;
                    }
                    s.on = !s.on;
                    for _ in 0..3 { self.push_render(); }
                    self.renders_without_frame = 0;
                    self.sweep = Some(s);
                    return false;
                }
                for row in &s.rows { println!("target {}", row); }
                let grips: Vec<&LoggedTarget> = self.targets.iter().filter(|t| !t.grip.is_empty()).collect();
                for g in &grips { println!("grip   {:>5},{:<5} {:>4}x{:<4} {:?}", g.b[0], g.b[1], g.b[2] - g.b[0], g.b[3] - g.b[1], g.grip); }
                println!("targets: {} on screen; {} show nothing when the pointer is on them; {} grips, which `drag` takes hold of by name", s.list.len(), s.quiet, grips.len());
                self.push_move(s.home.0, s.home.1);
                self.push_render();
                self.fresh = false;
            }
            "lint" => {
                if !self.need_fresh() { return false; }
                let name = args.first().cloned().unwrap_or_else(|| format!("frame{}", self.frame_count));
                let found = self.lint();
                self.keep_lint(&name, &found);
                let mut kinds: Vec<&str> = found.iter().map(|f| f.kind).collect();
                kinds.sort();
                kinds.dedup();
                for kind in kinds {
                    let of: Vec<&Finding> = found.iter().filter(|f| f.kind == kind).collect();
                    for f in of.iter().take(8) { println!("lint {:<8} {:>5},{:<5} {:?} {}", f.kind, f.b[0], f.b[1], short(&f.text), f.detail); }
                    if of.len() > 8 { println!("lint {:<8} ... and {} more", kind, of.len() - 8); }
                }
                let mut sizes: Vec<u32> = self.texts.iter().map(|t| t.h).collect();
                sizes.sort();
                sizes.dedup();
                let mut colours: Vec<u32> = self.texts.iter().map(|t| t.color).collect();
                colours.sort();
                colours.dedup();
                let file = format!("lint_{}.png", name);
                self.write_file(&file, &self.lint_image(&found));
                println!("lint {}: {} findings, marked in {}; text sizes {:?}, {} text colours", name, found.len(), self.out_dir.join(&file).display(), sizes, colours.len());
            }
            "settle" | "film" | "track" => {
                let frames = match cmd { "settle" => num(0).unwrap_or(600.0), "film" => num(1).unwrap_or(12.0), _ => num(1).unwrap_or(30.0) }.max(1.0) as usize;
                let mut region = [0, 0, self.width, self.height];
                if let (Some(x), Some(y), Some(w), Some(h)) = (num(2), num(3), num(4), num(5)) {
                    let (x, y) = ((x as usize).min(self.width - 1), (y as usize).min(self.height - 1));
                    region = [x, y, (w as usize).clamp(1, self.width - x), (h as usize).clamp(1, self.height - y)];
                }
                let shrink = region[2].div_ceil(400).max(1);
                let needle = args.first().cloned().unwrap_or_default();
                let mut w = self.watch.take().unwrap_or(Watch { left: frames, started: false, prev: Vec::new(), changed: Vec::new(), still: 0, cells: Vec::new(), spots: Vec::new() });
                if w.started {
                    let now = self.window_pixels();
                    let changed = if now.len() != w.prev.len() { now.len() } else { now.iter().zip(&w.prev).filter(|(a, b)| (**a ^ **b) & 0xffffff != 0).count() };
                    w.still = if changed == 0 { w.still + 1 } else { 0 };
                    w.changed.push(changed);
                    w.prev = now;
                    if cmd == "film" { w.cells.push(self.small(region, shrink).0); }
                    if cmd == "track" { w.spots.push(self.find_text(&needle, 0).map(|t| (t.x, t.y))); }
                    w.left -= 1;
                } else {
                    w.prev = self.window_pixels();
                    w.started = true;
                }
                if w.left > 0 && !(cmd == "settle" && w.still >= 3) {
                    self.watch = Some(w);
                    self.renders_without_frame = 0;
                    self.push_render();
                    return false;
                }
                let per_frame = w.changed.iter().take(60).map(|c| c.to_string()).collect::<Vec<_>>().join(" ");
                match cmd {
                    "settle" => {
                        let last = w.changed.iter().rposition(|c| *c != 0).map(|i| i + 1).unwrap_or(0);
                        let first = w.changed.iter().position(|c| *c != 0).map(|i| i + 1).unwrap_or(0);
                        if w.still >= 3 {
                            println!("settle: still after {} frames ({} ms at 60 fps), first change in frame {}; changed px per frame: {}", last, last * 1000 / 60, first, per_frame);
                        } else {
                            fail(&format!("line {}: settle: still changing after {} frames; changed px per frame: {}", self.pc + 1, w.changed.len(), per_frame));
                        }
                    }
                    "film" => {
                        let name = if needle.is_empty() { "film".to_string() } else { needle };
                        let (_, cw, ch) = self.small(region, shrink);
                        let cols = w.cells.len().clamp(1, 4);
                        let rows = w.cells.len().div_ceil(cols).max(1);
                        let (sw, sh) = (cols * (cw + 4) - 4, rows * (ch + 4) - 4);
                        let mut rgb = vec![90u8; sw * sh * 3];
                        for (i, cell) in w.cells.iter().enumerate() {
                            let (ox, oy) = ((i % cols) * (cw + 4), (i / cols) * (ch + 4));
                            for y in 0..ch {
                                let d = ((oy + y) * sw + ox) * 3;
                                rgb[d..d + cw * 3].copy_from_slice(&cell[y * cw * 3..(y + 1) * cw * 3]);
                            }
                        }
                        self.keep_shot(&name, &rgb, sw, sh);
                        println!("film {}.png: {} frames, left to right then down, each 1/{} size; changed px per frame: {}", name, w.cells.len(), shrink, per_frame);
                    }
                    _ => {
                        let mut s = String::new();
                        let mut i = 0;
                        while i < w.spots.len() {
                            let mut j = i;
                            while j < w.spots.len() && w.spots[j] == w.spots[i] { j += 1; }
                            match w.spots[i] { Some((x, y)) => s.push_str(&format!(" {},{}", x, y)), None => s.push_str(" gone") }
                            if j - i > 1 { s.push_str(&format!("x{}", j - i)); }
                            i = j;
                        }
                        println!("track {:?} over {} frames:{}", needle, w.spots.len(), s);
                    }
                }
            }
            "resize" | "drag" => {
                let window = cmd == "resize";
                if self.reflow.is_none() {
                    if !self.need_fresh() { return false; }
                    let usage = if window { "resize <name> <width> <height> [steps]" } else { "drag <name> <\"grip\" or \"text\", or x y> <dx> <dy> [steps]" };
                    let (from, to, count) = if window {
                        match (num(1), num(2)) {
                            (Some(w), Some(h)) if w >= 1.0 && h >= 1.0 && w <= 8192.0 && h <= 8192.0 => ((self.width as i32, self.height as i32), (w as i32, h as i32), num(3)),
                            _ => { fail(&format!("line {}: {}", self.pc + 1, usage)); return true; }
                        }
                    } else {
                        let by_point = args.len() >= 5 && num(1).is_some() && num(2).is_some();
                        let n = if by_point { 3 } else { 2 };
                        let what = args.get(1).cloned().unwrap_or_default();
                        let from = if by_point { Some((num(1).unwrap() as i32, num(2).unwrap() as i32)) } else if what.is_empty() { None } else {
                            self.targets.iter().find(|t| !t.grip.is_empty() && t.grip.contains(&what)).map(|t| ((t.b[0] + t.b[2]) / 2, (t.b[1] + t.b[3]) / 2))
                                .or_else(|| self.find_text(&what, 0).map(|t| (t.x + t.h as i32 / 2, t.y + t.h as i32 / 2)))
                        };
                        match (from, num(n), num(n + 1)) {
                            (Some(from), Some(dx), Some(dy)) => (from, (from.0 + dx as i32, from.1 + dy as i32), num(n + 2)),
                            (None, Some(_), Some(_)) => {
                                let grips: Vec<String> = self.targets.iter().filter(|t| !t.grip.is_empty()).map(|t| format!("{:?}", t.grip)).collect();
                                fail(&format!("line {}: drag found no grip and no text named {:?}; the grips on screen: {}", self.pc + 1, what, if grips.is_empty() { "none".to_string() } else { grips.join(", ") }));
                                return true;
                            }
                            _ => { fail(&format!("line {}: {}", self.pc + 1, usage)); return true; }
                        }
                    };
                    let widest = if window { from.0.max(to.0) as usize } else { self.width };
                    self.reflow = Some(Reflow { window, from, to, count: count.unwrap_or(24.0).clamp(1.0, 120.0) as usize, shrink: widest.div_ceil(400).max(1),
                        phase: 0, first: Vec::new(), first_us: 0, steps: Vec::new() });
                    if !window {
                        // The hover has to be drawn before the press, as for `click`.
                        self.push_move(from.0, from.1);
                        self.push_render();
                        self.push_button(BTN_LEFT, true);
                        self.push_render();
                        return false;
                    }
                }
                let mut r = self.reflow.take().unwrap();
                let once = |seen: &[Seen]| shown_once(seen.iter().map(|s| Shown { x: s.t.x, y: s.t.y, w: s.t.w as i32, h: s.t.h as i32, color: String::new(), text: s.t.text.clone() }).collect());
                if r.phase == 1 {
                    r.first = once(&self.shown_texts());
                    r.first_us = self.report.total_us;
                    r.phase = 2;
                    self.reflow = Some(r);
                    self.renders_without_frame = 0;
                    self.push_render();
                    return false;
                }
                if r.phase == 2 {
                    let seen = self.shown_texts();
                    let texts = once(&seen);
                    let faults: Vec<Fault> = self.lint().into_iter().map(|f| Fault { key: format!("{}\t{}\t{}", f.kind, f.text, no_numbers(&f.detail)), kind: f.kind, text: f.text, detail: f.detail, b: f.b }).collect();
                    let cell = if film_steps(r.count).contains(&r.steps.len()) { self.small([0, 0, self.width, self.height], r.shrink) } else { (Vec::new(), 0, 0) };
                    r.steps.push(Step {
                        at: if window { (self.width as i32, self.height as i32) } else { self.mouse },
                        trailing: moved_far(&r.first, &texts, (0, 0)),
                        shown: seen.len(), texts, faults, us: r.first_us, cell,
                    });
                }
                if r.steps.len() <= r.count {
                    let (i, n) = (r.steps.len() as i32, r.count as i32);
                    let at = (r.from.0 + (r.to.0 - r.from.0) * i / n, r.from.1 + (r.to.1 - r.from.1) * i / n);
                    if window { self.resize(at.0.max(1) as usize, at.1.max(1) as usize); } else { self.push_move(at.0, at.1); }
                    r.phase = 1;
                    self.reflow = Some(r);
                    self.renders_without_frame = 0;
                    self.push_render();
                    return false;
                }
                if !window {
                    self.push_button(BTN_LEFT, false);
                    self.push_render();
                }
                self.fresh = false;
                self.reflow_report(cmd, &args[0], &r);
            }
            "bench" | "bench_idle" => {
                let b = self.bench.get_or_insert_with(|| Bench { left: num(0).unwrap_or(120.0) as usize, full: cmd == "bench", samples: Vec::new() });
                if b.left > 0 {
                    b.left -= 1;
                    // A new generation drops the loop's tile cache, so every tile is rasterised
                    // again; without it only the tiles whose commands changed are.
                    if b.full { self.generation += 1; }
                    self.renders_without_frame = 0;
                    self.push_render();
                    return false;
                }
                let b = self.bench.take().unwrap();
                let stat = |pick: fn(&(usize, usize)) -> usize| {
                    let mut v: Vec<usize> = b.samples.iter().map(pick).collect();
                    v.sort();
                    if v.is_empty() { return "none".to_string(); }
                    format!("min {} med {} p95 {} max {}", v[0], v[v.len() / 2], v[(v.len() * 95 / 100).min(v.len() - 1)], v[v.len() - 1])
                };
                println!("{} {} frames {}x{} us: layout [{}] total [{}]", cmd, b.samples.len(), self.width, self.height, stat(|s| s.0), stat(|s| s.1));
            }
            "monkey" => {
                if self.monkey.is_none() {
                    let seed = num(1).map(|s| s as u64).filter(|s| *s != 0).unwrap_or_else(fresh_seed);
                    println!("monkey seed {} (its commands are written to {})", seed, self.out_dir.join("monkey.gui").display());
                    self.write_file("monkey.gui", b"");
                    self.monkey = Some(Monkey { left: num(0).unwrap_or(500.0) as usize, rng: seed });
                }
                let (left, mut rng) = { let m = self.monkey.as_ref().unwrap(); (m.left, m.rng) };
                if left == 0 {
                    self.monkey = None;
                    println!("monkey done");
                    return true;
                }
                let lines = self.monkey_lines(&mut rng);
                self.monkey = Some(Monkey { left: left - 1, rng });
                // On disk before it runs, so a crash leaves the script that reproduces it.
                use std::io::Write;
                if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(self.out_dir.join("monkey.gui")) {
                    for l in &lines { let _ = writeln!(f, "{}", l); }
                }
                self.injected.extend(lines);
                return false;
            }
            _ => fail(&format!("line {}: unknown command {:?}", self.pc + 1, cmd)),
        }
        true
    }

    /// One random step of input, as script commands.
    fn monkey_lines(&self, rng: &mut u64) -> Vec<String> {
        const KEYS: [&str; 26] = ["esc", "enter", "tab", "backspace", "delete", "home", "end", "left", "right", "up", "down",
            "pageup", "pagedown", "space", "a", "j", "x", "c", "minus", "equal", "0", "f3", "f4", "f5", "f8", "insert"];
        const TEXT: [&str; 12] = ["1", "0.5", "999999999999", "-3", "abc", " ", "u1qqq", "é", "日本", "1e9", ".", "0x10"];
        let mut r = |n: u64| (rng_next(rng) % n.max(1)) as i64;
        let (w, h) = (self.width as u64, self.height as u64);
        // A point, usually inside the window and sometimes just outside it.
        let mut point = |r: &mut dyn FnMut(u64) -> i64| (r(w + 40) - 20, r(h + 40) - 20);
        let mut out = Vec::new();
        match r(100) {
            0..=34 if !self.texts.is_empty() => {
                let t = &self.texts[r(self.texts.len() as u64) as usize];
                out.push(format!("click {} {}", t.x + t.h as i32 / 2 + r(40) as i32, t.y + t.h as i32 / 2));
            }
            0..=49 => { let (x, y) = point(&mut r); out.push(format!("click {} {}", x, y)); }
            50..=57 => { let (x, y) = point(&mut r); out.push(format!("move {} {}", x, y)); }
            58..=67 => {
                let held = ["", "ctrl", "shift"][r(3) as usize];
                if !held.is_empty() { out.push(format!("down {}", held)); }
                out.push(format!("key {}", KEYS[r(KEYS.len() as u64) as usize]));
                if !held.is_empty() { out.push(format!("up {}", held)); }
            }
            68..=75 => out.push(format!("type {}", (0..1 + r(4)).map(|_| TEXT[r(TEXT.len() as u64) as usize]).collect::<String>())),
            76..=83 => out.push(format!("wheel {} {}", r(13) - 6, r(5) - 2)),
            84..=88 => out.push(format!("scroll {} {}", r(801) - 400, r(801) - 400)),
            89..=90 => out.push(format!("zoom {}", (r(201) - 100) as f64 / 100.0)),
            91..=95 => {
                let (x1, y1) = point(&mut r);
                let (x2, y2) = point(&mut r);
                out.extend([format!("move {} {}", x1, y1), "frame".into(), "down mouse".into(), "frame".into(),
                    format!("move {} {}", x2, y2), "frame".into(), "up mouse".into()]);
            }
            96..=98 => out.push(format!("size {} {}", 1 + r(2600), 1 + r(1500))),
            _ => out.push(format!("scale {}", [0.5, 1.0, 1.25, 1.5, 2.0, 3.0][r(6) as usize])),
        }
        out.push("frame".into());
        out
    }

    /// Says what a finished `resize` or `drag` found, and keeps its film and its findings.
    fn reflow_report(&mut self, cmd: &str, name: &str, r: &Reflow) {
        // The faults a change of size can cause. A text too faint or too small is so at any size.
        const BROUGHT: [&str; 5] = ["clipped", "overflow", "overlap", "margin", "target"];
        let n = r.steps.len();
        let place = |i: usize| { let at = r.steps[i].at; if r.window { format!("{}x{}", at.0, at.1) } else { format!("{},{}", at.0, at.1) } };
        let texts = |n: usize| if n == 1 { "1 text".to_string() } else { format!("{} texts", n) };
        let list = format!("{}.txt", name);
        let mut all: Vec<String> = Vec::new();

        // A fault the first step already shows was there before anything moved.
        let per_step: Vec<Vec<(&str, f64)>> = r.steps.iter().map(|s| s.faults.iter().filter(|f| BROUGHT.contains(&f.kind)).map(|f| (f.key.as_str(), severity(f.kind, &f.detail))).collect()).collect();
        let mut spans = fault_spans(&per_step);
        spans.retain(|s| s.first > 0);
        spans.sort_by(|a, b| (a.first, &a.key).cmp(&(b.first, &b.key)));
        let mut brought: Vec<(&Span, &Fault)> = Vec::new();
        for s in &spans {
            let Some(f) = r.steps[s.worst].faults.iter().filter(|f| f.key == s.key)
                .max_by(|a, b| severity(a.kind, &a.detail).total_cmp(&severity(b.kind, &b.detail))) else { continue };
            let words = format!("{}; at {}: {}", span_words(s, n, &place), place(s.worst), f.detail);
            // Worded alike whatever the span, so `check` lists the findings of one fault together.
            self.lint_lines.push(format!("{}\t{}\t{}\t{},{}\t{}; from {} to {}", name, f.kind, f.text.replace('\t', " "), f.b[0], f.b[1], f.detail, place(s.first), place(s.last)));
            all.push(format!("{} fault {:<8} {:?} {}", cmd, f.kind, short(&f.text), words));
            brought.push((s, f));
        }
        let held = |i: usize| spans.iter().filter(|s| per_step[i].iter().any(|f| f.0 == s.key)).count();
        println!("{} {}: {} {} -> {} in {} steps; faults the change brings, at each step: {}", cmd, name, if r.window { "window" } else { "pointer" },
            place(0), place(n - 1), n - 1, (0..n).take(61).map(|i| held(i).to_string()).collect::<Vec<_>>().join(" "));
        let mut kinds: Vec<&str> = Vec::new();
        for (_, f) in &brought { if !kinds.contains(&f.kind) { kinds.push(f.kind); } }
        for kind in &kinds {
            let of: Vec<&(&Span, &Fault)> = brought.iter().filter(|b| b.1.kind == *kind).collect();
            let (first, worst) = (of[0], of.iter().max_by(|a, b| severity(kind, &a.1.detail).total_cmp(&severity(kind, &b.1.detail))).unwrap());
            if of.len() == 1 { println!("{}", all[brought.iter().position(|b| b.1.kind == *kind).unwrap()]); continue; }
            println!("{} fault {:<8} first at {} on {:?}; on {} in all; worst on {:?} at {}: {}",
                cmd, kind, place(first.0.first), short(&first.1.text), texts(of.len()), short(&worst.1.text), place(worst.0.worst), worst.1.detail);
        }

        // Nothing has cause to move further than the side or the pointer that pushed it.
        let slack = (2.0 * self.scale.max(0.25)).round() as i32;
        let mut jumps: Vec<(usize, usize, String)> = Vec::new();
        for i in 1..n {
            let (a, b) = (r.steps[i - 1].at, r.steps[i].at);
            let (count, most) = moved_far(&r.steps[i - 1].texts, &r.steps[i].texts, ((b.0 - a.0).abs() + slack, (b.1 - a.1).abs() + slack));
            let Some((text, d)) = most else { continue };
            jumps.push((count, i, format!("{} jump  from {} to {}: {} moved further than the {} did, most of all {:?} by {},{} px",
                cmd, place(i - 1), place(i), texts(count), if r.window { "window's side" } else { "pointer" }, short(&text), d.0, d.1)));
        }
        all.extend(jumps.iter().map(|j| j.2.clone()));
        let mut biggest = jumps.clone();
        biggest.sort_by_key(|j| (std::cmp::Reverse(j.0), j.1));
        biggest.truncate(4);
        biggest.sort_by_key(|j| j.1);
        for j in &biggest { println!("{}", j.2); }
        if jumps.len() > 4 { println!("{} jump  ... and {} more, of fewer texts", cmd, jumps.len() - 4); }

        // The first step changes nothing, so its two frames differ only if the screen was moving anyway.
        let trailing: Vec<usize> = (1..n).filter(|&i| r.steps[i].trailing.0 > 0).collect();
        if r.steps[0].trailing.0 > 0 {
            println!("{} trail: the screen was still moving before the first step, so what follows the {} late cannot be told apart: `settle` first", cmd, cmd);
        } else if let Some(&i) = trailing.iter().max_by_key(|&&i| r.steps[i].trailing.0) {
            let (count, most) = &r.steps[i].trailing;
            let (text, d) = most.clone().unwrap_or_default();
            let line = format!("{} trail: at {} of {} steps part of the screen follows a frame late: the frame after, with nothing changed, moved {}, at {} most of all {:?} by {},{} px",
                cmd, trailing.len(), n - 1, texts(*count), place(i), short(&text), d.0, d.1);
            println!("{}", line);
            all.push(line);
        }
        if !all.is_empty() {
            all.push(String::new());
            self.write_file(&list, all.join("\n").as_bytes());
        }

        let mut us: Vec<usize> = r.steps.iter().map(|s| s.us).collect();
        us.sort();
        let breaks = match spans.first() {
            Some(s) => format!("no new fault until {}, then {} by {} ({})", place(s.first - 1), spans.len(), place(n - 1), self.out_dir.join(&list).display()),
            None => "no new fault".to_string(),
        };
        println!("{} {}: {}; {} jumps; {} of {} steps follow late; texts shown {} -> {}; a step's first frame took med {} max {} us",
            cmd, name, breaks, jumps.len(), trailing.len(), n - 1, r.steps[0].shown, r.steps[n - 1].shown, us[n / 2], us[n - 1]);

        let picks: Vec<usize> = film_steps(r.count).into_iter().filter(|&i| i < n && r.steps[i].cell.1 > 0).collect();
        let (cw, ch) = picks.iter().fold((1, 1), |m, &i| (m.0.max(r.steps[i].cell.1), m.1.max(r.steps[i].cell.2)));
        let cols = picks.len().clamp(1, 4);
        let (sw, sh) = (cols * (cw + 4) - 4, picks.len().div_ceil(cols).max(1) * (ch + 4) - 4);
        let mut rgb = vec![90u8; sw * sh * 3];
        for (slot, &i) in picks.iter().enumerate() {
            let step = &r.steps[i];
            let (mut cell, w, h) = step.cell.clone();
            let f = r.shrink as i32;
            for fault in &step.faults {
                if spans.iter().any(|s| s.key == fault.key) {
                    outline(&mut cell, w, h, [fault.b[0] / f - 1, fault.b[1] / f - 1, fault.b[2] / f, fault.b[3] / f], lint_colour(fault.kind));
                }
            }
            let (ox, oy) = ((slot % cols) * (cw + 4), (slot / cols) * (ch + 4));
            for y in 0..h {
                let d = ((oy + y) * sw + ox) * 3;
                rgb[d..d + w * 3].copy_from_slice(&cell[y * w * 3..(y + 1) * w * 3]);
            }
        }
        self.keep_shot(name, &rgb, sw, sh);
        println!("{} {}.png: the {} at {}, left to right then down, each 1/{} size, a box round each fault the change brings",
            cmd, name, if r.window { "window" } else { "pointer" }, picks.iter().map(|&i| place(i)).collect::<Vec<_>>().join(" "), r.shrink);
    }

    fn keep_shot(&mut self, name: &str, rgb: &[u8], w: usize, h: usize) {
        let file = format!("{}.png", name);
        self.write_file(&file, &png_encode(rgb, w, h));
        let line = format!("{} {}x{} {:016x}", file, w, h, fnv1a(rgb));
        println!("shot {}", line);
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(self.out_dir.join("shots.txt")) {
            let _ = writeln!(f, "{}", line);
        }
    }

    fn keep_lint(&mut self, name: &str, found: &[Finding]) {
        for f in found {
            self.lint_lines.push(format!("{}\t{}\t{}\t{},{}\t{}", name, f.kind, f.text.replace('\t', " "), f.b[0], f.b[1], f.detail));
        }
    }

    fn window_pixels(&self) -> Vec<u32> {
        let mut v = Vec::with_capacity(self.width * self.height);
        for row in 0..self.height { v.extend_from_slice(&self.pixels[row * self.side..row * self.side + self.width]); }
        v
    }

    /// The region `[x, y, w, h]` shrunk by a whole factor, as RGB, with its new width and height.
    fn small(&self, b: [usize; 4], f: usize) -> (Vec<u8>, usize, usize) {
        let (cw, ch) = ((b[2] / f).max(1), (b[3] / f).max(1));
        let mut rgb = Vec::with_capacity(cw * ch * 3);
        for cy in 0..ch {
            for cx in 0..cw {
                let (mut sum, mut n) = ([0u32; 3], 0u32);
                for y in 0..f {
                    for x in 0..f {
                        let (px, py) = (b[0] + cx * f + x, b[1] + cy * f + y);
                        if px < self.width && py < self.height {
                            let p = self.pixels[py * self.side + px];
                            sum[0] += (p >> 16) & 255; sum[1] += (p >> 8) & 255; sum[2] += p & 255;
                            n += 1;
                        }
                    }
                }
                let n = n.max(1);
                rgb.extend_from_slice(&[(sum[0] / n) as u8, (sum[1] / n) as u8, (sum[2] / n) as u8]);
            }
        }
        (rgb, cw, ch)
    }

    /// The commonest colour among the pixels of a box that is not near `ink`: what a text of
    /// that colour is being read against.
    fn background(&self, b: [i32; 4], ink: u32) -> Option<u32> {
        let mut px: Vec<u32> = Vec::new();
        for y in b[1]..b[3] {
            let row = y as usize * self.side;
            px.extend(self.pixels[row + b[0] as usize..row + b[2] as usize].iter().map(|p| p & 0xffffff));
        }
        px.sort_unstable();
        let near = |a: u32, b: u32| (0..3).all(|i| ((a >> (i * 8)) & 255).abs_diff((b >> (i * 8)) & 255) < 32);
        let mut best = (0usize, None);
        let mut i = 0;
        while i < px.len() {
            let mut j = i;
            while j < px.len() && px[j] == px[i] { j += 1; }
            if j - i > best.0 && !near(px[i], ink) { best = (j - i, Some(px[i])); }
            i = j;
        }
        best.1
    }

    /// The clip in force when the frame issued its draw command number `at`: a clip change at
    /// command k governs what is drawn after it.
    fn clip_at(&self, at: usize) -> [i32; 4] {
        if self.clips.is_empty() { return [0, 0, self.width as i32, self.height as i32]; }
        self.clips[self.clips.partition_point(|c| c.0 < at).max(1) - 1].1
    }

    /// The texts a person can read: on screen, inside their clip, not under something drawn later.
    fn shown_texts(&self) -> Vec<Seen<'_>> { self.texts_under(0x30) }

    /// The texts on screen that nothing drawn later covers with at least that much alpha. A
    /// dialog dims the page behind it: that page is hard to read, and still there to see.
    fn texts_under(&self, hides: u32) -> Vec<Seen<'_>> {
        let mut seen = Vec::new();
        for t in &self.texts {
            if t.w == 0 || t.text.trim().is_empty() { continue; }
            let full = [t.x, t.y, t.x + t.w as i32, t.y + t.h as i32];
            let vis = cut(full, self.clip_at(t.at));
            if area(vis) == 0 { continue; }
            if self.rects.iter().any(|r| r.at >= t.at && r.color >> 24 >= hides && area(cut(r.b, vis)) * 2 > area(vis)) { continue; }
            seen.push(Seen { t, full, vis });
        }
        seen
    }

    /// The click targets a person can reach: on screen, and not under something drawn later.
    /// A target's place in the drawing order is that of the last thing drawn inside it.
    fn shown_targets(&self) -> Vec<LoggedTarget> {
        let window = [0, 0, self.width as i32, self.height as i32];
        let mut out: Vec<LoggedTarget> = Vec::new();
        for t in &self.targets {
            let vis = cut(t.b, window);
            if area(vis) == 0 || !t.grip.is_empty() || out.iter().any(|o| o.b == t.b) { continue; }
            let inside = |b: [i32; 4]| area(b) > 0 && b[0] >= t.b[0] - 2 && b[1] >= t.b[1] - 2 && b[2] <= t.b[2] + 2 && b[3] <= t.b[3] + 2;
            let own = self.rects.iter().filter(|r| inside(r.b)).map(|r| r.at)
                .chain(self.texts.iter().filter(|x| inside(cut([x.x, x.y, x.x + x.w as i32, x.y + x.h as i32], self.clip_at(x.at)))).map(|x| x.at)).max();
            let Some(own) = own else { continue };
            if self.rects.iter().any(|r| r.at > own && r.color >> 24 >= 0x30 && !inside(r.b) && area(cut(r.b, vis)) * 2 > area(vis)) { continue; }
            out.push(t.clone());
        }
        out
    }

    /// The text that names a box in a report.
    fn label_of(&self, b: [i32; 4]) -> String { label_among(&self.shown_texts(), b) }

    /// What a part of the screen shows, a line each: its texts, then what can be pressed.
    fn content(&self, name: &str, part: [i32; 4]) -> Vec<String> {
        let mut out = Vec::new();
        for s in self.texts_under(0xf0) {
            if area(cut(s.vis, part)) == 0 { continue; }
            out.push(format!("{}\tT\t{}\t{}\t{}\t{}\t{:06x}\t{}", name, s.t.x, s.t.y, s.t.w, s.t.h, s.t.color & 0xffffff, s.t.text.replace(['\t', '\n'], " ")));
        }
        let shown = self.shown_texts();
        for t in self.shown_targets() {
            if area(cut(t.b, part)) == 0 { continue; }
            out.push(format!("{}\tP\t{}\t{}\t{}\t{}\t-\t{}", name, t.b[0], t.b[1], t.b[2] - t.b[0], t.b[3] - t.b[1], label_among(&shown, t.b).replace(['\t', '\n'], " ")));
        }
        out
    }

    /// The text sizes, colours and corner radii that part of the screen is made of, and how
    /// often each is used, commonest first within its kind.
    fn style(&self, part: [i32; 4]) -> Vec<(&'static str, String, usize)> {
        let mut uses: HashMap<(&'static str, String), usize> = HashMap::new();
        for s in self.shown_texts() {
            if area(cut(s.vis, part)) == 0 { continue; }
            *uses.entry(("text size", format!("{} px", s.t.h))).or_default() += 1;
            *uses.entry(("text colour", format!("{:06x}", s.t.color & 0xffffff))).or_default() += 1;
        }
        let window = (self.width * self.height) as i64;
        for r in &self.rects {
            if r.color >> 24 < 0x20 || area(r.b) >= window || area(cut(r.b, part)) == 0 { continue; }
            let fill = if r.color >> 24 == 0xff { format!("{:06x}", r.color & 0xffffff) } else { format!("{:06x}/{:02x}", r.color & 0xffffff, r.color >> 24) };
            *uses.entry(("fill", fill)).or_default() += 1;
            if r.radius > 0 { *uses.entry(("corner radius", format!("{} px", r.radius))).or_default() += 1; }
        }
        let mut all: Vec<(&'static str, String, usize)> = uses.into_iter().map(|((kind, value), n)| (kind, value, n)).collect();
        all.sort_by(|a, b| (a.0, std::cmp::Reverse(a.2), &a.1).cmp(&(b.0, std::cmp::Reverse(b.2), &b.1)));
        all
    }

    /// Faults a person sees at a glance and a test of behaviour does not: text cut off, text on
    /// top of text, text crowding or leaving the box behind it, text too faint or too small.
    /// Text that something drawn later hides, a dialog over the page for one, is left out.
    fn lint(&self) -> Vec<Finding> {
        let seen = self.shown_texts();

        let scale = self.scale.max(0.25) as f64;
        let margin = (4.0 * scale).round() as i32;
        let mut out = Vec::new();
        for s in &seen {
            let t = s.t;
            let mut add = |kind: &'static str, detail: String| out.push(Finding { kind, b: s.vis, text: t.text.clone(), detail });
            let (cut_l, cut_r) = (s.vis[0] - s.full[0], s.full[2] - s.vis[2]);
            if cut_l > 1 || cut_r > 1 {
                add("clipped", format!("{} px cut off on the {}", cut_l.max(cut_r), if cut_r >= cut_l { "right" } else { "left" }));
            }

            // The box behind the text: the last rectangle drawn before it that lies under its middle.
            let mid = [(s.vis[0] + s.vis[2]) / 2, (s.vis[1] + s.vis[3]) / 2];
            let behind = self.rects.iter().rev().find(|r| r.at < t.at && r.color >> 24 >= 0x20 && holds(r.b, mid)
                && r.b[3] - r.b[1] >= t.h as i32 / 2 && r.b[2] - r.b[0] >= 4);
            if let Some(r) = behind {
                let side = (s.full[0] - r.b[0]).min(r.b[2] - s.full[2]);
                if cut_l <= 1 && cut_r <= 1 {
                    if side < -1 { add("overflow", format!("{} px wider than the box behind it", -side)); }
                    else if side < margin { add("margin", format!("{} px from the side of the box behind it", side)); }
                }
                let over = (s.full[1] - r.b[1]).min(r.b[3] - s.full[3]);
                if s.vis[3] - s.vis[1] == s.full[3] - s.full[1] && over < -2 { add("overflow", format!("{} px taller than the box behind it", -over)); }
            }

            // Text is drawn at full strength whatever its colour's top byte says.
            let ink = t.color & 0xffffff;
            if let Some(bg) = self.background(s.vis, ink) {
                let ratio = contrast(ink, bg);
                if ratio < 3.0 { add("contrast", format!("{:.1}:1, {:06x} on {:06x}", ratio, ink, bg)); }
            }
            if (t.h as f64) < 10.0 * scale { add("tiny", format!("{} px tall", t.h)); }
        }

        let (wide, high) = (self.width as i32, self.height as i32);
        let (least, reach) = ((24.0 * scale).round() as i32, (12.0 * scale).round() as i32);
        for t in self.shown_targets() {
            let (w, h) = (t.b[2] - t.b[0], t.b[3] - t.b[1]);
            let (b, label) = (cut(t.b, [0, 0, wide, high]), self.label_of(t.b));
            if w.min(h) < least { out.push(Finding { kind: "target", b, text: label.clone(), detail: format!("{}x{} px to click, under {} px", w, h, least) }); }
            // The left and right sides of a maximized window are the screen's, where the pointer
            // stops: a target that reaches one cannot be overshot, and one that stops just short
            // of it can. The top and bottom are never the screen's, with a title bar above and a
            // task bar below.
            for (gap, side) in [(t.b[0], "left"), (wide - t.b[2], "right")] {
                if gap >= 1 && gap <= reach { out.push(Finding { kind: "edge", b, text: label.clone(), detail: format!("stops {} px short of the window's {} side", gap, side) }); }
            }
        }

        for (i, a) in seen.iter().enumerate() {
            for b in &seen[i + 1..] {
                let o = cut(a.vis, b.vis);
                if o[2] - o[0] <= 2 || (o[3] - o[1]) * 10 < 3 * a.t.h.min(b.t.h) as i32 { continue; }
                // The same text drawn twice in one place is a shadow or a redraw.
                if a.t.text == b.t.text && a.t.x.abs_diff(b.t.x) <= 2 && a.t.y.abs_diff(b.t.y) <= 2 { continue; }
                // A box drawn between the two hides the earlier one there.
                let (lo, hi) = (a.t.at.min(b.t.at), a.t.at.max(b.t.at));
                let mid = [(o[0] + o[2]) / 2, (o[1] + o[3]) / 2];
                if self.rects.iter().any(|r| r.at > lo && r.at < hi && r.color >> 24 >= 0x80 && holds(r.b, mid)) { continue; }
                out.push(Finding { kind: "overlap", b: o, text: a.t.text.clone(), detail: format!("on top of {:?}", short(&b.t.text)) });
            }
        }
        out
    }

    /// The window with a box drawn round each finding, one colour per kind.
    fn lint_image(&self, found: &[Finding]) -> Vec<u8> {
        let (w, h) = (self.width, self.height);
        let mut rgb = Vec::with_capacity(w * h * 3);
        for row in 0..h {
            for px in &self.pixels[row * self.side..row * self.side + w] { rgb.extend_from_slice(&[(px >> 16) as u8, (px >> 8) as u8, *px as u8]); }
        }
        for f in found { outline(&mut rgb, w, h, [f.b[0] - 2, f.b[1] - 2, f.b[2] + 1, f.b[3] + 1], lint_colour(f.kind)); }
        png_encode(&rgb, w, h)
    }

    fn write_file(&self, name: &str, bytes: &[u8]) {
        let path = self.out_dir.join(name);
        if let Some(dir) = path.parent() { let _ = std::fs::create_dir_all(dir); }
        if let Err(e) = std::fs::write(&path, bytes) { fail(&format!("{}: {}", path.display(), e)); }
    }
}

struct Finding {
    kind: &'static str,
    b: [i32; 4],
    text: String,
    detail: String,
}

fn lint_colour(kind: &str) -> [u8; 3] {
    match kind { "clipped" => [255, 40, 40], "overlap" => [255, 0, 255], "overflow" => [255, 140, 0], "margin" => [255, 230, 0], "contrast" => [0, 220, 255], "target" => [90, 255, 90], "edge" => [110, 150, 255], _ => [150, 150, 150] }
}

/// Draws the one-pixel outline of a box, its far edges included, on an RGB image.
fn outline(rgb: &mut [u8], w: usize, h: usize, b: [i32; 4], c: [u8; 3]) {
    let mut dot = |x: i32, y: i32| if x >= 0 && y >= 0 && (x as usize) < w && (y as usize) < h {
        let i = (y as usize * w + x as usize) * 3;
        rgb[i..i + 3].copy_from_slice(&c);
    };
    for x in b[0]..=b[2] { dot(x, b[1]); dot(x, b[3]); }
    for y in b[1]..=b[3] { dot(b[0], y); dot(b[2], y); }
}

/// Boxes are `[x1, y1, x2, y2]`, the far edges exclusive.
fn cut(a: [i32; 4], b: [i32; 4]) -> [i32; 4] { [a[0].max(b[0]), a[1].max(b[1]), a[2].min(b[2]), a[3].min(b[3])] }
fn area(b: [i32; 4]) -> i64 { if b[2] > b[0] && b[3] > b[1] { (b[2] - b[0]) as i64 * (b[3] - b[1]) as i64 } else { 0 } }
fn holds(b: [i32; 4], p: [i32; 2]) -> bool { b[0] <= p[0] && p[0] < b[2] && b[1] <= p[1] && p[1] < b[3] }

/// Of the texts shown inside a box, the one a person would call it by: the tallest that is not
/// an icon, the one nearest the top left among equals. Text drawn across the box by something
/// else does not lie inside it, and counts only when nothing does.
fn label_among(shown: &[Seen<'_>], b: [i32; 4]) -> String {
    let best = |of: &dyn Fn(&Seen<'_>) -> bool| shown.iter().filter(|s| of(s))
        .min_by_key(|s| (s.t.text.starts_with('<'), std::cmp::Reverse(s.t.h), s.t.y, s.t.x)).map(|s| s.t.text.clone());
    best(&|s| s.vis[0] >= b[0] - 2 && s.vis[1] >= b[1] - 2 && s.vis[2] <= b[2] + 2 && s.vis[3] <= b[3] + 2)
        .or_else(|| best(&|s| holds(b, [(s.vis[0] + s.vis[2]) / 2, (s.vis[1] + s.vis[3]) / 2])))
        .unwrap_or_else(|| "(no text)".to_string())
}

fn short(s: &str) -> String {
    if s.chars().count() > 40 { format!("{}...", s.chars().take(37).collect::<String>()) } else { s.to_string() }
}

/// The WCAG contrast ratio of two RGB colours, 1 to 21.
fn contrast(a: u32, b: u32) -> f64 {
    let luminance = |rgb: u32| {
        let ch = |v: u32| { let c = (v & 255) as f64 / 255.0; if c <= 0.03928 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) } };
        0.2126 * ch(rgb >> 16) + 0.7152 * ch(rgb >> 8) + 0.0722 * ch(rgb)
    };
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

static HEARTBEAT: AtomicUsize = AtomicUsize::new(0);
static CURRENT_LINE: Mutex<String> = Mutex::new(String::new());

fn note_line(pc: usize, line: &str) {
    if let Ok(mut cur) = CURRENT_LINE.try_lock() {
        cur.clear();
        cur.push_str(&format!("script line {}: {}", pc + 1, line));
    }
}

fn where_in_script() -> String {
    CURRENT_LINE.try_lock().map(|s| s.clone()).unwrap_or_default()
}

/// The frames of this program in the current stack, without the runtime's own around them.
fn our_frames() -> String {
    let all = std::backtrace::Backtrace::force_capture().to_string();
    let lines: Vec<&str> = all.lines().collect();
    let mut out = String::new();
    let mut kept = 0;
    let mut i = 0;
    while i < lines.len() && kept < 14 {
        let at = lines.get(i + 1).filter(|l| l.trim_start().starts_with("at ")).copied();
        let ours = |l: &str| ["zebra_gui", "zebra-gui", "clay", "wallet", "softer_gui"].iter().any(|k| l.contains(k));
        if (ours(lines[i]) || at.is_some_and(ours)) && !lines[i].contains("script::") {
            out.push_str(lines[i].trim_end());
            if let Some(at) = at { out.push_str(&format!("  {}", at.trim())); }
            out.push('\n');
            kept += 1;
        }
        i += if at.is_some() { 2 } else { 1 };
    }
    if kept == 0 { lines.iter().take(30).map(|l| format!("{}\n", l)).collect() } else { out }
}

#[cfg(windows)]
#[repr(C)]
struct ExceptionRecord {
    code: u32,
    flags: u32,
    record: *mut ExceptionRecord,
    address: *mut std::ffi::c_void,
    parameter_count: u32,
    information: [usize; 15],
}

#[cfg(windows)]
#[repr(C)]
struct ExceptionPointers {
    record: *const ExceptionRecord,
    context: *const std::ffi::c_void,
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn SetUnhandledExceptionFilter(filter: unsafe extern "system" fn(*const ExceptionPointers) -> i32) -> usize;
}

#[cfg(windows)]
unsafe extern "system" fn on_exception(info: *const ExceptionPointers) -> i32 {
    let r = &*(*info).record;
    let what = match r.code {
        0xC0000005 => format!("access violation {} address {:#x}", if r.information[0] == 1 { "writing" } else { "reading" }, r.information[1]),
        0xC00000FD => "stack overflow".to_string(),
        0xC000001D => "illegal instruction".to_string(),
        0xC0000094 => "integer divide by zero".to_string(),
        code => format!("exception {:#x}", code),
    };
    eprintln!("CRASH {} ({})", what, where_in_script());
    // There is no stack left to walk with after an overflow.
    if r.code != 0xC00000FD { eprint!("{}", our_frames()); }
    std::process::exit(139);
}

#[cfg(unix)]
unsafe extern "C" {
    fn signal(signum: i32, handler: usize) -> usize;
}

#[cfg(unix)]
extern "C" fn on_signal(sig: i32) {
    eprintln!("CRASH signal {} ({})", sig, where_in_script());
    eprint!("{}", our_frames());
    std::process::exit(128 + sig);
}

/// A crash, a panic or a hang in a scripted run says what it was and which script line it was
/// on, then ends the process. Without this a crash is a bare exit code, and a panic on a render
/// worker or a hang is a run that never returns.
fn report_crashes() {
    if cfg!(test) { return; }
    std::panic::set_hook(Box::new(|info| {
        eprintln!("PANIC {} ({})", info, where_in_script());
        eprint!("{}", our_frames());
        std::process::exit(3);
    }));
    std::thread::spawn(|| {
        let (mut last, mut stuck) = (usize::MAX, 0);
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
            let beat = HEARTBEAT.load(Ordering::Relaxed);
            if beat != last { last = beat; stuck = 0; continue; }
            stuck += 1;
            if stuck >= 30 {
                eprintln!("HANG the frame loop has not come back for 30 s ({})", where_in_script());
                std::process::exit(4);
            }
        }
    });
    #[cfg(windows)]
    unsafe { SetUnhandledExceptionFilter(on_exception); }
    #[cfg(unix)]
    unsafe { for sig in [4, 7, 8, 10, 11] { signal(sig, on_signal as usize); } }
}

static PACE_ON: AtomicBool = AtomicBool::new(false);
static PACE: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// From here on a real window shows every frame it draws and the gaps between them are kept.
pub fn pace_start() { PACE_ON.store(true, Ordering::Relaxed); }

/// The frame loop calls this each frame with the microseconds since it last showed one. True
/// while a pace run is recording, which makes the loop show this frame too.
pub fn pace_frame(us: u64) -> bool {
    if !PACE_ON.load(Ordering::Relaxed) { return false; }
    PACE.lock().unwrap().push(us.min(u32::MAX as u64) as u32);
    true
}

/// Prints what a pace run recorded: how evenly a real window's frames reached the screen.
pub fn pace_report() {
    let all = PACE.lock().unwrap();
    // The first frames are start-up: fonts, the first layout, the window appearing.
    let gaps: &[u32] = if all.len() > 20 { &all[10..] } else { &all[..] };
    if gaps.is_empty() { println!("pace: no frames were shown"); return; }
    let mut sorted = gaps.to_vec();
    sorted.sort();
    let ms = |us: u32| us as f64 / 1000.0;
    let (med, total) = (sorted[sorted.len() / 2], gaps.iter().map(|g| *g as u64).sum::<u64>());
    let late = gaps.iter().filter(|g| **g as u64 * 2 > med as u64 * 3).count();
    let (worst_at, worst) = gaps.iter().enumerate().max_by_key(|(_, g)| **g).unwrap();
    let before: u64 = gaps[..worst_at].iter().map(|g| *g as u64).sum();
    println!("pace: {} frames in {:.1} s ({:.1} fps); ms between frames: min {:.1} med {:.1} p95 {:.1} max {:.1}; {} late (over 1.5x the median); longest gap {:.1} ms at {:.1} s",
        gaps.len(), total as f64 / 1e6, gaps.len() as f64 * 1e6 / total.max(1) as f64,
        ms(sorted[0]), ms(med), ms(sorted[(sorted.len() * 95 / 100).min(sorted.len() - 1)]), ms(sorted[sorted.len() - 1]), late, ms(*worst), before as f64 / 1e6);
}

/// One repeated run: how many counted lines long, where it first appears as (file, line), and
/// each later copy with the number of lines that copy shares.
type Repeat = (usize, (usize, usize), Vec<((usize, usize), usize)>);

/// Blank lines, comments and lines of only brackets are not code worth calling repeated.
fn counts(line: &str) -> bool {
    let line = line.trim();
    !(line.is_empty() || line.starts_with("//") || line.chars().all(|c| "{}()[];,".contains(c)))
}

/// The runs of at least `min` counted lines that appear more than once in the texts, and how
/// many lines counted in all.
fn repeated_runs(texts: &[String], min: usize) -> (Vec<Repeat>, usize) {
    let min = min.max(2);
    // (file, line number, hash of the trimmed line), for the lines that count.
    let mut lines: Vec<(usize, usize, u64)> = Vec::new();
    for (file, text) in texts.iter().enumerate() {
        for (n, line) in text.lines().enumerate() {
            if counts(line) { lines.push((file, n + 1, fnv1a(line.trim().as_bytes()))); }
        }
    }
    let n = lines.len();
    let key = |i: usize| -> Option<u64> {
        if i + min > n || lines[i + min - 1].0 != lines[i].0 { return None; }
        Some(lines[i..i + min].iter().fold(0xcbf29ce484222325u64, |h, l| (h ^ l.2).wrapping_mul(0x100000001b3).rotate_left(29)))
    };
    let mut starts: HashMap<u64, Vec<usize>> = HashMap::new();
    for i in 0..n { if let Some(k) = key(i) { starts.entry(k).or_default().push(i); } }

    let mut covered = vec![false; n];
    let mut found: Vec<Repeat> = Vec::new();
    for i in 0..n {
        if covered[i] { continue; }
        let Some(group) = key(i).and_then(|k| starts.get(&k)) else { continue };
        let mut copies: Vec<(usize, usize)> = Vec::new();
        for &j in group {
            if j <= i || covered[j] { continue; }
            let mut len = 0;
            while j + len < n && i + len < j && lines[i + len].0 == lines[i].0 && lines[j + len].0 == lines[j].0 && lines[i + len].2 == lines[j + len].2 { len += 1; }
            if len >= min { copies.push((j, len)); }
        }
        if copies.is_empty() { continue; }
        let longest = copies.iter().map(|c| c.1).max().unwrap();
        for k in i..i + longest { covered[k] = true; }
        for (j, len) in &copies { for k in *j..*j + *len { covered[k] = true; } }
        found.push((longest, (lines[i].0, lines[i].1), copies.iter().map(|(j, len)| ((lines[*j].0, lines[*j].1), *len)).collect()));
    }
    (found, n)
}

/// Prints the repeated runs of code in the files, biggest first. Returns how many there are.
pub fn duplicates(paths: &[String], min: usize) -> usize {
    let texts: Vec<String> = paths.iter().map(|p| std::fs::read_to_string(p).unwrap_or_else(|_| { eprintln!("could not read {}", p); String::new() })).collect();
    let (mut found, counted) = repeated_runs(&texts, min);
    found.sort_by_key(|f| std::cmp::Reverse(f.0 * f.2.len()));
    let at = |p: (usize, usize)| format!("{}:{}", paths[p.0], p.1);
    for (longest, first, copies) in found.iter().take(25) {
        let others: Vec<String> = copies.iter().map(|(p, len)| if len == longest { at(*p) } else { format!("{} ({} lines)", at(*p), len) }).collect();
        println!("{:>4} lines x{}: {} {}", longest, copies.len() + 1, at(*first), others.join(" "));
    }
    let repeated: usize = found.iter().map(|f| f.2.iter().map(|c| c.1).sum::<usize>()).sum();
    println!("dup: {} repeated runs of {}+ lines; {} of {} counted lines are a second or later copy", found.len(), min.max(2), repeated, counted);
    found.len()
}

pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for b in bytes { h = (h ^ *b as u64).wrapping_mul(0x100000001b3); }
    h
}

// The PNG codec below exists so screenshots need no dependency. The encoder writes 8-bit RGB
// with the Up filter and one fixed-Huffman deflate block whose only matches are byte runs; the
// decoder reads back exactly that subset (plus stored blocks), not PNGs from anywhere else.

const LEN_BASE:  [u16; 29] = [3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131, 163, 195, 227, 258];
const LEN_EXTRA: [u8; 29]  = [0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0];
const DIST_BASE:  [u16; 30] = [1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537, 2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577];
const DIST_EXTRA: [u8; 30]  = [0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13, 13];

fn crc32(bytes: &[u8]) -> u32 {
    let mut c = 0xffffffffu32;
    for b in bytes {
        c ^= *b as u32;
        for _ in 0..8 { c = (c >> 1) ^ (0xedb88320 & (c & 1).wrapping_neg()); }
    }
    !c
}

fn adler32(bytes: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for chunk in bytes.chunks(5552) {
        for x in chunk { a += *x as u32; b += a; }
        a %= 65521;
        b %= 65521;
    }
    (b << 16) | a
}

struct BitWriter { out: Vec<u8>, acc: u64, n: u32 }
impl BitWriter {
    fn bits(&mut self, value: u32, count: u32) {
        self.acc |= (value as u64) << self.n;
        self.n += count;
        while self.n >= 8 { self.out.push(self.acc as u8); self.acc >>= 8; self.n -= 8; }
    }
    /// Huffman codes go in most significant bit first, unlike every other field.
    fn code(&mut self, code: u32, count: u32) { self.bits(code.reverse_bits() >> (32 - count), count); }
    fn symbol(&mut self, sym: u32) {
        match sym {
            0..=143   => self.code(0x30 + sym, 8),
            144..=255 => self.code(0x190 + sym - 144, 9),
            256..=279 => self.code(sym - 256, 7),
            _         => self.code(0xc0 + sym - 280, 8),
        }
    }
}

fn deflate_runs(data: &[u8]) -> Vec<u8> {
    let mut w = BitWriter { out: vec![0x78, 0x01], acc: 0, n: 0 };
    w.bits(1, 1);
    w.bits(1, 2);
    let mut i = 0;
    while i < data.len() {
        let b = data[i];
        w.symbol(b as u32);
        i += 1;
        let mut run = 0;
        while i + run < data.len() && data[i + run] == b { run += 1; }
        while run >= 3 {
            let len = run.min(258);
            let li = LEN_BASE.iter().rposition(|base| *base as usize <= len).unwrap();
            w.symbol(257 + li as u32);
            w.bits((len - LEN_BASE[li] as usize) as u32, LEN_EXTRA[li] as u32);
            w.code(0, 5);
            i += len;
            run -= len;
        }
    }
    w.symbol(256);
    w.bits(0, 7);
    w.out.extend_from_slice(&adler32(data).to_be_bytes());
    w.out
}

fn png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
    out.extend_from_slice(&(body.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(body);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// `rgb` is `width * height * 3` bytes, rows top to bottom.
pub fn png_encode(rgb: &[u8], width: usize, height: usize) -> Vec<u8> {
    assert!(rgb.len() == width * height * 3);
    let stride = width * 3;
    let mut raw = Vec::with_capacity((stride + 1) * height);
    for y in 0..height {
        raw.push(2);
        let row = &rgb[y * stride..(y + 1) * stride];
        if y == 0 { raw.extend_from_slice(row); } else {
            let above = &rgb[(y - 1) * stride..y * stride];
            raw.extend(row.iter().zip(above).map(|(a, b)| a.wrapping_sub(*b)));
        }
    }
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&(width as u32).to_be_bytes());
    ihdr.extend_from_slice(&(height as u32).to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
    png_chunk(&mut out, b"IHDR", &ihdr);
    png_chunk(&mut out, b"IDAT", &deflate_runs(&raw));
    png_chunk(&mut out, b"IEND", &[]);
    out
}

struct BitReader<'a> { data: &'a [u8], pos: usize, acc: u64, n: u32 }
impl BitReader<'_> {
    fn bits(&mut self, count: u32) -> Option<u32> {
        while self.n < count {
            self.acc |= (*self.data.get(self.pos)? as u64) << self.n;
            self.pos += 1;
            self.n += 8;
        }
        let v = (self.acc & ((1u64 << count) - 1)) as u32;
        self.acc >>= count;
        self.n -= count;
        Some(v)
    }
    fn code(&mut self, count: u32) -> Option<u32> { Some(self.bits(count)?.reverse_bits() >> (32 - count)) }
    fn symbol(&mut self) -> Option<u32> {
        let mut c = self.code(7)?;
        if c <= 0x17 { return Some(256 + c); }
        c = (c << 1) | self.bits(1)?;
        if (0x30..=0xbf).contains(&c) { return Some(c - 0x30); }
        if (0xc0..=0xc7).contains(&c) { return Some(280 + c - 0xc0); }
        c = (c << 1) | self.bits(1)?;
        Some(144 + c.checked_sub(0x190)?)
    }
}

fn inflate(zlib: &[u8], size_hint: usize) -> Option<Vec<u8>> {
    let mut r = BitReader { data: zlib.get(2..)?, pos: 0, acc: 0, n: 0 };
    let mut out = Vec::with_capacity(size_hint.min(1 << 26));
    loop {
        let last = r.bits(1)?;
        match r.bits(2)? {
            0 => {
                r.bits(r.n % 8)?;
                let len = r.bits(16)? as usize;
                r.bits(16)?;
                for _ in 0..len { out.push(r.bits(8)? as u8); }
            }
            1 => loop {
                let sym = r.symbol()?;
                if sym < 256 { out.push(sym as u8); continue; }
                if sym == 256 { break; }
                let li = (sym - 257) as usize;
                let len = *LEN_BASE.get(li)? as usize + r.bits(*LEN_EXTRA.get(li)? as u32)? as usize;
                let di = r.code(5)? as usize;
                let dist = *DIST_BASE.get(di)? as usize + r.bits(*DIST_EXTRA.get(di)? as u32)? as usize;
                if dist > out.len() { return None; }
                for _ in 0..len { out.push(out[out.len() - dist]); }
            },
            _ => return None,
        }
        if last == 1 { return Some(out); }
    }
}

/// Decodes a PNG this module wrote. Returns (rgb, width, height).
pub fn png_decode(png: &[u8]) -> Option<(Vec<u8>, usize, usize)> {
    if png.get(..8)? != [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a] { return None; }
    let (mut pos, mut width, mut height, mut idat) = (8, 0usize, 0usize, Vec::new());
    loop {
        let len = u32::from_be_bytes(png.get(pos..pos + 4)?.try_into().ok()?) as usize;
        let kind = png.get(pos + 4..pos + 8)?;
        let body = png.get(pos + 8..(pos + 8).checked_add(len)?)?;
        pos += 12 + len;
        match kind {
            b"IHDR" => {
                width = u32::from_be_bytes(body.get(0..4)?.try_into().ok()?) as usize;
                height = u32::from_be_bytes(body.get(4..8)?.try_into().ok()?) as usize;
                if body.get(8..13)? != [8, 2, 0, 0, 0] || width > 16384 || height > 16384 { return None; }
            }
            b"IDAT" => idat.extend_from_slice(body),
            b"IEND" => break,
            _ => {}
        }
    }
    let stride = width * 3;
    let raw = inflate(&idat, (stride + 1) * height)?;
    if raw.len() != (stride + 1) * height { return None; }
    let mut rgb = vec![0u8; stride * height];
    for y in 0..height {
        let line = &raw[y * (stride + 1)..(y + 1) * (stride + 1)];
        for x in 0..stride {
            let above = if y == 0 { 0 } else { rgb[(y - 1) * stride + x] };
            rgb[y * stride + x] = match line[0] { 0 => line[1 + x], 2 => line[1 + x].wrapping_add(above), _ => return None };
        }
    }
    Some((rgb, width, height))
}

/// A text, or a thing to press, that a screenshot showed, as `content.txt` keeps it.
#[derive(Debug, Clone, PartialEq, Default)]
struct Shown { x: i32, y: i32, w: i32, h: i32, color: String, text: String }

/// What each screenshot of a run showed: its texts, and what could be pressed.
fn read_content(dir: &Path) -> HashMap<String, (Vec<Shown>, Vec<Shown>)> {
    let mut all: HashMap<String, (Vec<Shown>, Vec<Shown>)> = HashMap::new();
    for l in std::fs::read_to_string(dir.join("content.txt")).unwrap_or_default().lines() {
        let f: Vec<&str> = l.splitn(8, '\t').collect();
        if f.len() < 8 { continue; }
        let n = |i: usize| f[i].parse::<i32>().unwrap_or(0);
        let item = Shown { x: n(2), y: n(3), w: n(4), h: n(5), color: f[6].to_string(), text: f[7].to_string() };
        let of = all.entry(f[0].to_string()).or_default();
        if f[1] == "P" { of.1.push(item) } else { of.0.push(item) }
    }
    all
}

/// Pairs what two recordings of a screen share: the same text, in the same order down the
/// screen. Returns the pairs, what only the second has, and what only the first has.
fn pair_up(a: &[Shown], b: &[Shown]) -> (Vec<(Shown, Shown)>, Vec<Shown>, Vec<Shown>) {
    let order = |v: &[Shown]| -> Vec<Shown> {
        let mut o = v.to_vec();
        o.sort_by(|p, q| (&p.text, p.y, p.x).cmp(&(&q.text, q.y, q.x)));
        o
    };
    let (oa, ob) = (order(a), order(b));
    let (mut i, mut j) = (0, 0);
    let (mut pairs, mut added, mut gone) = (Vec::new(), Vec::new(), Vec::new());
    while i < oa.len() || j < ob.len() {
        if j == ob.len() || (i < oa.len() && oa[i].text < ob[j].text) { gone.push(oa[i].clone()); i += 1; }
        else if i == oa.len() || ob[j].text < oa[i].text { added.push(ob[j].clone()); j += 1; }
        else { pairs.push((oa[i].clone(), ob[j].clone())); i += 1; j += 1; }
    }
    (pairs, added, gone)
}

fn quoted(s: &str) -> String { format!("\"{}\"", short(s)) }

/// What makes two lint findings the same fault: their kind, and their message with each
/// number in it taken out, since the number differs with the window's size and scale.
fn lint_fault(f: &[String]) -> String {
    format!("{} {}", f.get(1).map(|k| k.as_str()).unwrap_or(""), no_numbers(f.get(4).map(|m| m.as_str()).unwrap_or("")))
}

/// A message with each number in it replaced by `N`.
fn no_numbers(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_number = false;
    for c in s.chars() {
        if !c.is_ascii_digit() { out.push(c); } else if !in_number { out.push('N'); }
        in_number = c.is_ascii_digit();
    }
    out
}

/// Of the texts a frame showed, those it showed once, in the order of their text. Only those
/// can be followed from one frame to the next: of two alike, which is which is a guess.
fn shown_once(mut all: Vec<Shown>) -> Vec<Shown> {
    all.sort_by(|a, b| a.text.cmp(&b.text));
    let mut out = Vec::new();
    let mut i = 0;
    while i < all.len() {
        let mut j = i + 1;
        while j < all.len() && all[j].text == all[i].text { j += 1; }
        if j == i + 1 { out.push(all[i].clone()); }
        i = j;
    }
    out
}

/// Of the texts two frames each showed once, how many moved further than `allow` in x or in y
/// between them, and the one that went furthest past it. Both lists are as `shown_once` gives them.
fn moved_far(a: &[Shown], b: &[Shown], allow: (i32, i32)) -> (usize, Option<(String, (i32, i32))>) {
    let (mut i, mut j, mut n) = (0, 0, 0);
    let mut most: Option<(i32, String, (i32, i32))> = None;
    while i < a.len() && j < b.len() {
        match a[i].text.cmp(&b[j].text) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                let d = (b[j].x - a[i].x, b[j].y - a[i].y);
                let past = (d.0.abs() - allow.0).max(d.1.abs() - allow.1);
                if past > 0 {
                    n += 1;
                    if most.as_ref().is_none_or(|m| past > m.0) { most = Some((past, a[i].text.clone(), d)); }
                }
                i += 1;
                j += 1;
            }
        }
    }
    (n, most.map(|m| (m.1, m.2)))
}

/// How bad a finding is, to pick the step a fault was worst at: the number its message leads
/// with, counted against it where the smaller number is the worse one.
fn severity(kind: &str, detail: &str) -> f64 {
    let end = detail.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(detail.len());
    let n: f64 = detail[..end].parse().unwrap_or(0.0);
    if kind == "clipped" || kind == "overflow" { n } else { -n }
}

/// Follows each fault over the steps of a sweep. A step lists the faults it showed, each with
/// how bad it was there. The faults that held longest come first.
fn fault_spans(steps: &[Vec<(&str, f64)>]) -> Vec<Span> {
    let mut spans: Vec<(Span, f64)> = Vec::new();
    let mut index: HashMap<&str, usize> = HashMap::new();
    for (i, step) in steps.iter().enumerate() {
        for &(key, bad) in step {
            match index.get(key) {
                Some(&k) => {
                    let (s, worst) = &mut spans[k];
                    if s.last != i { s.held += 1; s.last = i; }
                    if bad > *worst { *worst = bad; s.worst = i; }
                }
                None => {
                    index.insert(key, spans.len());
                    spans.push((Span { key: key.to_string(), first: i, last: i, held: 1, worst: i }, bad));
                }
            }
        }
    }
    let mut out: Vec<Span> = spans.into_iter().map(|s| s.0).collect();
    out.sort_by(|a, b| (std::cmp::Reverse(a.held), &a.key).cmp(&(std::cmp::Reverse(b.held), &b.key)));
    out
}

/// Over which of a sweep's `n` steps a fault held, in words. `place` names a step.
fn span_words(s: &Span, n: usize, place: &dyn Fn(usize) -> String) -> String {
    if s.held == n { "at every step".to_string() }
    else if s.held < s.last - s.first + 1 { format!("at {} of the steps from {} to {}", s.held, place(s.first), place(s.last)) }
    else if s.first == s.last { format!("only at {}", place(s.first)) }
    else if s.last == n - 1 { format!("from {} on", place(s.first)) }
    else if s.first == 0 { format!("until {}", place(s.last)) }
    else { format!("from {} to {}", place(s.first), place(s.last)) }
}

/// Which steps of a sweep its film shows: all of them, or eight spread evenly from the first
/// to the last.
fn film_steps(count: usize) -> Vec<usize> {
    if count < 8 { (0..=count).collect() } else { (0..8).map(|k| k * count / 7).collect() }
}

/// Every text, and every name of a thing to press, on any screenshot of the first run and of
/// the second.
#[derive(Default)]
struct Known { texts: [std::collections::HashSet<String>; 2], press: [std::collections::HashSet<String>; 2] }

/// What changed between two recordings of one screen, in words, and a key that is the same for
/// every screen changed by the same cause. Both are empty when no text and nothing to press
/// changed. The key holds the texts no screen had before, and those no screen has any more.
/// What follows from that differs with each window size and is left out of it: the rest
/// moving over, and what that brings into view or pushes out. A text that is gone from a
/// screen where others moved may have been pushed out, so there it is left out as well.
fn describe_change(a: &(Vec<Shown>, Vec<Shown>), b: &(Vec<Shown>, Vec<Shown>), known: &Known) -> (String, String) {
    let names = |v: &[Shown]| -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for s in v { let q = quoted(&s.text); if !out.contains(&q) { out.push(q); } }
        out
    };
    let some = |v: &[String], most: usize| -> String {
        let rest = if v.len() > most { format!(" and {} more", v.len() - most) } else { String::new() };
        format!("{}{}", v.iter().take(most).cloned().collect::<Vec<_>>().join(", "), rest)
    };
    let (mut words, mut key): (Vec<String>, Vec<String>) = (Vec::new(), Vec::new());
    let (pairs, added, gone) = pair_up(&a.0, &b.0);
    let (_, press_added, press_gone) = pair_up(&a.1, &b.1);

    let fresh: Vec<Shown> = added.iter().filter(|s| !known.texts[0].contains(&s.text)).cloned().collect();
    let lost: Vec<Shown> = gone.iter().filter(|s| !known.texts[1].contains(&s.text)).cloned().collect();
    let fresh_names = names(&fresh);
    if !fresh.is_empty() { words.push(format!("new text {}", some(&fresh_names, 4))); key.push(format!("+{}", fresh_names.join(","))); }
    if !lost.is_empty() { words.push(format!("gone text {}", some(&names(&lost), 4))); }

    // A scroll bar's thumb has no text, and comes and goes with what it scrolls.
    let fresh_press: Vec<Shown> = press_added.iter().filter(|s| s.text != "(no text)" && !known.press[0].contains(&s.text)).cloned().collect();
    let lost_press: Vec<Shown> = press_gone.iter().filter(|s| s.text != "(no text)" && !known.press[1].contains(&s.text)).cloned().collect();
    if !fresh_press.is_empty() {
        let n: Vec<String> = fresh_press.iter().map(|s| format!("{} {}x{} at {},{}", quoted(&s.text), s.w, s.h, s.x, s.y)).collect();
        words.push(format!("new to press {}", some(&n, 4)));
    }
    if !lost_press.is_empty() { words.push(format!("no longer to press {}", some(&names(&lost_press), 4))); }

    // Only a text the screen shows once: of two alike, which is which is a guess.
    let once = |v: &[Shown], text: &str| v.iter().filter(|s| s.text == text).count() == 1;
    let mut restyled: Vec<String> = Vec::new();
    for (p, q) in &pairs {
        if !once(&a.0, &p.text) || !once(&b.0, &p.text) { continue; }
        if p.h != q.h { restyled.push(format!("{} {} -> {} px tall", quoted(&p.text), p.h, q.h)); }
        else if p.color != q.color { restyled.push(format!("{} {} -> {}", quoted(&p.text), p.color, q.color)); }
    }
    if !restyled.is_empty() { words.push(format!("restyled {}", some(&restyled, 3))); key.push(format!("~{}", restyled.join(","))); }

    let mut moves: Vec<((i32, i32), usize)> = Vec::new();
    for (p, q) in &pairs {
        let d = (q.x - p.x, q.y - p.y);
        if d == (0, 0) { continue; }
        match moves.iter_mut().find(|m| m.0 == d) { Some(m) => m.1 += 1, None => moves.push((d, 1)) }
    }
    moves.sort_by_key(|m| (std::cmp::Reverse(m.1), m.0));
    if let Some(&((dx, dy), n)) = moves.first() {
        let way = match (dx, dy) {
            (0, d) if d > 0 => format!("{} px down", d),
            (0, d) => format!("{} px up", -d),
            (d, 0) if d > 0 => format!("{} px right", d),
            (d, 0) => format!("{} px left", -d),
            (x, y) => format!("by {},{} px", x, y),
        };
        let others: usize = moves[1..].iter().map(|m| m.1).sum();
        let rest = if others > 0 { format!(", {} moved otherwise", others) } else { String::new() };
        words.push(format!("{} text{} moved {}{}", n, if n == 1 { "" } else { "s" }, way, rest));
    }
    if moves.is_empty() && !lost.is_empty() { key.push(format!("-{}", names(&lost).join(","))); }
    let (came, went) = (added.len() - fresh.len(), gone.len() - lost.len());
    if came > 0 { words.push(format!("{} came into view", came)); }
    if went > 0 { words.push(format!("{} went out of view", went)); }
    if key.is_empty() && !words.is_empty() { key.push("follows".to_string()); }
    (words.join("; "), key.join(" | "))
}

fn read_shots(dir: &Path) -> Vec<(String, String)> {
    // A later line for the same file wins: a rerun into the same folder appends.
    let mut out: Vec<(String, String)> = Vec::new();
    for line in std::fs::read_to_string(dir.join("shots.txt")).unwrap_or_default().lines() {
        let Some((file, rest)) = line.split_once(' ') else { continue };
        match out.iter_mut().find(|(f, _)| f == file) {
            Some(entry) => entry.1 = rest.to_string(),
            None => out.push((file.to_string(), rest.to_string())),
        }
    }
    out
}

/// What one scripted run wrote, or a folder of such runs: there, a screenshot's name is led by
/// its run's, as in `tour/main`.
#[derive(Default)]
struct Recorded {
    /// Name, size and hash, and the image file.
    shots: Vec<(String, String, PathBuf)>,
    content: HashMap<String, (Vec<Shown>, Vec<Shown>)>,
    lint: Vec<Vec<String>>,
    /// Kind and value, the first screenshot with it, and its uses.
    style: Vec<(String, String, usize)>,
    has_style: bool,
}

fn read_recorded(dir: &Path) -> Recorded {
    let mut runs: Vec<(String, PathBuf)> = Vec::new();
    if dir.join("shots.txt").exists() {
        runs.push((String::new(), dir.to_path_buf()));
    } else {
        let mut names: Vec<String> = std::fs::read_dir(dir).into_iter().flatten().flatten()
            .filter(|e| e.path().join("shots.txt").exists()).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        for n in names { runs.push((format!("{}/", n), dir.join(&n))); }
    }
    let mut r = Recorded::default();
    for (lead, run) in &runs {
        for (file, meta) in read_shots(run) { r.shots.push((format!("{}{}", lead, file.trim_end_matches(".png")), meta, run.join(&file))); }
        for (shot, shown) in read_content(run) { r.content.insert(format!("{}{}", lead, shot), shown); }
        for l in std::fs::read_to_string(run.join("lint.txt")).unwrap_or_default().lines() {
            let mut f: Vec<String> = l.split('\t').map(str::to_string).collect();
            f[0] = format!("{}{}", lead, f[0]);
            r.lint.push(f);
        }
        r.has_style |= run.join("style.txt").exists();
        for l in std::fs::read_to_string(run.join("style.txt")).unwrap_or_default().lines() {
            let f: Vec<&str> = l.split('\t').collect();
            if f.len() < 4 { continue; }
            let (key, uses) = (format!("{} {}", f[1], f[2]), f[3].parse().unwrap_or(0));
            match r.style.iter_mut().find(|s| s.0 == key) { Some(s) => s.2 += uses, None => r.style.push((key, format!("{}{}", lead, f[0]), uses)) }
        }
    }
    r
}

/// Says what differs between two scripted runs, or two folders of runs, from the files each
/// wrote. For every screenshot that differs it writes `diff_<name>.png` beside the second one:
/// the changed region before, after, and with the changed pixels marked. Screenshots changed
/// by the same cause share one line, which names one image. Returns how many things differ.
pub fn diff_dirs(dir_a: &Path, dir_b: &Path) -> usize {
    let (a, b) = (read_recorded(dir_a), read_recorded(dir_b));
    let shows = |file: &Path| file.display().to_string().replace('\\', "/");
    let mut known = Known::default();
    for (side, run) in [&a, &b].iter().enumerate() {
        for (texts, press) in run.content.values() {
            known.texts[side].extend(texts.iter().map(|s| s.text.clone()));
            known.press[side].extend(press.iter().map(|s| s.text.clone()));
        }
    }
    let (mut same, mut changed, mut differ) = (0, 0, 0);
    let mut new_shots: Vec<String> = Vec::new();
    for (shot, _, file) in b.shots.iter().filter(|s| !a.shots.iter().any(|t| t.0 == s.0)) {
        println!("new screenshot: {} -> {}", shot, shows(file));
        new_shots.push(shot.clone());
        differ += 1;
    }
    // One entry for each cause of change: its key, and the screenshots it changed, each with
    // its size in pixels, its name and its line.
    let mut ways: Vec<(String, Vec<(usize, String, String)>)> = Vec::new();
    for (shot, meta_a, file_a) in &a.shots {
        let Some((_, meta_b, file_b)) = b.shots.iter().find(|s| &s.0 == shot) else {
            println!("gone screenshot: {}", shot);
            differ += 1;
            continue;
        };
        if meta_a == meta_b { same += 1; continue; }
        changed += 1;
        let load = |file: &Path| std::fs::read(file).ok().and_then(|bytes| png_decode(&bytes));
        let (Some((pa, wa, ha)), Some((pb, wb, hb))) = (load(file_a), load(file_b)) else {
            println!("differs: {} (could not read both images)", shot);
            continue;
        };
        if (wa, ha) != (wb, hb) {
            println!("differs: {} size {}x{} -> {}x{}", shot, wa, ha, wb, hb);
            continue;
        }
        let (mut x1, mut y1, mut x2, mut y2, mut count) = (wa, ha, 0, 0, 0usize);
        for y in 0..ha {
            for x in 0..wa {
                let i = (y * wa + x) * 3;
                if pa[i..i + 3] != pb[i..i + 3] {
                    count += 1;
                    x1 = x1.min(x); y1 = y1.min(y); x2 = x2.max(x + 1); y2 = y2.max(y + 1);
                }
            }
        }
        const PAD: usize = 24;
        let (cx1, cy1, cx2, cy2) = (x1.saturating_sub(PAD), y1.saturating_sub(PAD), (x2 + PAD).min(wa), (y2 + PAD).min(ha));
        let (cw, ch) = (cx2 - cx1, cy2 - cy1);
        let gap = 4;
        let ow = cw * 3 + gap * 2;
        let mut out = vec![0x40u8; ow * ch * 3];
        for y in 0..ch {
            for x in 0..cw {
                let src = ((cy1 + y) * wa + cx1 + x) * 3;
                let differs = pa[src..src + 3] != pb[src..src + 3];
                let mark = if differs { [0xff, 0x00, 0xff] } else { [pb[src] / 3, pb[src + 1] / 3, pb[src + 2] / 3] };
                for (panel, px) in [&pa[src..src + 3], &pb[src..src + 3], &mark[..]].iter().enumerate() {
                    let dst = (y * ow + panel * (cw + gap) + x) * 3;
                    out[dst..dst + 3].copy_from_slice(px);
                }
            }
        }
        let image = file_b.with_file_name(format!("diff_{}", file_b.file_name().unwrap_or_default().to_string_lossy()));
        let _ = std::fs::write(&image, png_encode(&out, ow, ch));

        // A film, or a crop with no text in it, has no record of what it shows.
        let (mut words, mut key) = match (a.content.get(shot), b.content.get(shot)) {
            (Some(was), Some(is)) => describe_change(was, is, &known),
            _ => ("there is no record of its texts".to_string(), "unrecorded".to_string()),
        };
        if words.is_empty() {
            words = "no text and nothing to press changed".to_string();
            key = format!("pixels {} {} {} {} {}", count, x1, x2, y1, y2);
        }
        let line = format!("differs: {}: {}; {} px in x {}..{} y {}..{} -> {}", shot, words, count, x1, x2, y1, y2, shows(&image));
        match ways.iter_mut().find(|w| w.0 == key) {
            Some(w) => w.1.push((wa * ha, shot.clone(), line)),
            None => ways.push((key, vec![(wa * ha, shot.clone(), line)])),
        }
    }
    // Of the screenshots one cause changed, the smallest window is shown: there the change has
    // the least room, and the image is the cheapest to read.
    for (_, shots) in &mut ways {
        shots.sort_by_key(|s| s.0);
        println!("{}", shots[0].2);
        if shots.len() == 1 { continue; }
        let rest = if shots.len() > 9 { format!(" and {} more", shots.len() - 9) } else { String::new() };
        println!("    the same cause changed {}{}", shots[1..].iter().take(8).map(|s| s.1.clone()).collect::<Vec<_>>().join(", "), rest);
    }
    println!("{} of {} screenshots differ, in {} ways", changed, same + changed, ways.len());
    differ += changed;

    // A finding is the same one in both runs if its screenshot, kind and text match; where it
    // sits and by how much it is off can move without it being news. On a screenshot the first
    // run did not take, a finding is news only if no screenshot of the first run had its like.
    let alike = |f: &[String], g: &[String], from: usize| f.len() >= 3 && g.len() >= 3 && f[from..3] == g[from..3];
    let mut new: Vec<&Vec<String>> = Vec::new();
    for f in &b.lint {
        let from = if new_shots.contains(&f[0]) { 1 } else { 0 };
        if !a.lint.iter().any(|g| alike(f, g, from)) { new.push(f); }
    }
    let gone = a.lint.iter().filter(|g| !b.lint.iter().any(|f| alike(f, g, 0))).count();
    // One fault shows on every screenshot that has its screen, at every scale, so the findings
    // are listed by fault: those of one kind whose messages differ only in their numbers.
    let mut faults: Vec<(String, Vec<&Vec<String>>)> = Vec::new();
    for f in &new {
        let key = lint_fault(f);
        match faults.iter_mut().find(|g| g.0 == key) {
            Some(g) => g.1.push(f),
            None => faults.push((key, vec![f])),
        }
    }
    for (_, found) in faults.iter().take(30) {
        println!("lint new: {}", found[0].join("  "));
        if found.len() == 1 { continue; }
        let (mut texts, mut shots): (Vec<String>, Vec<&String>) = (Vec::new(), Vec::new());
        for f in &found[1..] {
            if f[0] != found[0][0] && !shots.contains(&&f[0]) { shots.push(&f[0]); }
            let Some(text) = f.get(2) else { continue };
            let q = quoted(text);
            if found[0].get(2) != Some(text) && !texts.contains(&q) { texts.push(q); }
        }
        let also = if texts.is_empty() { String::new() } else {
            let rest = if texts.len() > 5 { format!(" and {} more", texts.len() - 5) } else { String::new() };
            format!(", also on {}{}", texts.iter().take(5).cloned().collect::<Vec<_>>().join(", "), rest)
        };
        println!("    the same fault {} more times, on {} more screenshots{}", found.len() - 1, shots.len(), also);
    }
    if faults.len() > 30 { println!("lint new: ... and {} more faults", faults.len() - 30); }
    if !new.is_empty() || gone != 0 { println!("lint: {} new findings of {} faults, {} gone, {} in all", new.len(), faults.len(), gone, b.lint.len()); }
    for shot in &new_shots {
        let of = b.lint.iter().filter(|f| &f[0] == shot).count();
        if of > 0 { println!("lint: {} is a new screenshot with {} findings; listed are those no screenshot had before", shot, of); }
    }
    differ += new.len();

    // A size, colour or corner radius that no screen of the first run used.
    if a.has_style {
        let new: Vec<&(String, String, usize)> = b.style.iter().filter(|s| !a.style.iter().any(|t| t.0 == s.0)).collect();
        let gone = a.style.iter().filter(|t| !b.style.iter().any(|s| s.0 == t.0)).count();
        for s in new.iter().take(20) { println!("style new: {} (first in {}, used {} times)", s.0, s.1, s.2); }
        if new.len() > 20 { println!("style new: ... and {} more", new.len() - 20); }
        if !new.is_empty() || gone != 0 { println!("style: {} new values, {} gone", new.len(), gone); }
        differ += new.len();
    }
    differ
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed() -> u64 {
        std::env::var("FUZZ_SEED").ok().and_then(|s| s.parse().ok()).unwrap_or_else(fresh_seed)
    }

    #[test]
    fn fuzz_describe_change() {
        let seed = seed();
        println!("fuzz_describe_change FUZZ_SEED={}", seed);
        let mut s = seed;
        for round in 0..600 {
            // A screen of texts with unlike names, some of them shown more than once, and
            // things to press. Then the same screen with a known set of changes made to it.
            let mut pick = |n: u64| (rng_next(&mut s) % n) as i32;
            let mut before: (Vec<Shown>, Vec<Shown>) = (Vec::new(), Vec::new());
            for i in 0..3 + pick(30) {
                let copies = if pick(5) == 0 { 2 + pick(3) } else { 1 };
                for c in 0..copies {
                    before.0.push(Shown { x: pick(1500), y: i * 40 + c * 7, w: 10 + pick(200), h: 8 + pick(40), color: format!("{:06x}", pick(0xffffff)), text: format!("text {}", i) });
                }
            }
            for i in 0..pick(8) { before.1.push(Shown { x: pick(1500), y: i * 50, w: 24 + pick(200), h: 24 + pick(60), color: "-".into(), text: format!("press {}", i) }); }
            let mut after = before.clone();

            let (dx, dy) = if pick(3) == 0 { (0, 0) } else { (if pick(2) == 0 { 0 } else { pick(400) - 200 }, pick(400) - 200) };
            for t in after.0.iter_mut().chain(after.1.iter_mut()) { t.x += dx; t.y += dy; }
            let add = pick(3);
            for i in 0..add { after.0.push(Shown { x: pick(1500), y: pick(900), w: 50, h: 12, color: "ffffff".into(), text: format!("added {}", i) }); }
            // Only a text shown once is dropped: dropping one copy of several leaves the rest
            // paired with each other's places, which is a move of its own.
            let once: Vec<usize> = (0..after.0.len()).filter(|&i| after.0.iter().filter(|t| t.text == after.0[i].text).count() == 1 && after.0[i].text.starts_with("text")).collect();
            let dropped = if pick(3) == 0 && !once.is_empty() { Some(after.0.remove(once[pick(once.len() as u64) as usize]).text) } else { None };
            let button = pick(4) == 0;
            if button { after.1.push(Shown { x: 5, y: 5, w: 80, h: 30, color: "-".into(), text: "new button".into() }); }
            // Order in the file is draw order, which a change may shuffle.
            if pick(2) == 0 { after.0.reverse(); }
            let moved = (dx, dy) != (0, 0);
            let way = match (dx, dy) {
                (0, d) if d > 0 => format!("moved {} px down", d),
                (0, d) => format!("moved {} px up", -d),
                (d, 0) if d > 0 => format!("moved {} px right", d),
                (d, 0) => format!("moved {} px left", -d),
                (x, y) => format!("moved by {},{} px", x, y),
            };

            // With no other screen: what was added is new, and what was dropped is gone.
            let mut alone = Known::default();
            for (side, screen) in [&before, &after].iter().enumerate() {
                alone.texts[side].extend(screen.0.iter().map(|t| t.text.clone()));
                alone.press[side].extend(screen.1.iter().map(|t| t.text.clone()));
            }
            let (words, key) = describe_change(&before, &after, &alone);
            let ctx = format!("seed {} round {}: {:?}", seed, round, words);
            if !moved && add == 0 && dropped.is_none() && !button {
                assert!(words.is_empty() && key.is_empty(), "{}", ctx);
                continue;
            }
            assert!(!words.is_empty() && !key.is_empty(), "{}", ctx);
            for i in 0..add { assert!(words.contains(&format!("\"added {}\"", i)), "{}", ctx); }
            assert_eq!(words.contains("new text"), add > 0, "{}", ctx);
            if let Some(name) = &dropped { assert!(words.contains(&format!("gone text \"{}\"", name)), "{}", ctx); }
            else { assert!(!words.contains("gone text"), "{}", ctx); }
            assert_eq!(words.contains("new to press \"new button\" 80x30 at 5,5"), button, "{}", ctx);
            assert!(!words.contains("no longer to press") && !words.contains("restyled") && !words.contains("came into view") && !words.contains("went out"), "{}", ctx);
            assert_eq!(words.contains(&way), moved, "{} wants {:?}", ctx, way);
            // A new thing to press is in the words and not in the key; so is a text that is
            // gone where the rest moved.
            assert_eq!(key == "follows", add == 0 && (dropped.is_none() || moved), "{} key {:?}", ctx, key);

            // The key leaves the distance out, so the same change at another window size has
            // the same key.
            let mut twice = before.clone();
            for t in twice.0.iter_mut().chain(twice.1.iter_mut()) { t.x += dx * 2; t.y += dy * 2; }
            twice.0.retain(|t| Some(&t.text) != dropped.as_ref());
            twice.0.extend(after.0.iter().filter(|t| t.text.starts_with("added")).cloned());
            if button { twice.1.push(Shown { x: 9, y: 9, w: 160, h: 60, color: "-".into(), text: "new button".into() }); }
            assert_eq!(describe_change(&before, &twice, &alone).1, key, "{}", ctx);

            // With every name on some other screen of both runs, nothing is new and nothing is
            // gone: what was added came into view, what was dropped went out of it.
            let mut elsewhere = Known::default();
            for side in 0..2 {
                elsewhere.texts[side].extend(before.0.iter().chain(after.0.iter()).map(|t| t.text.clone()));
                elsewhere.press[side].extend(before.1.iter().chain(after.1.iter()).map(|t| t.text.clone()));
            }
            let (words, key) = describe_change(&before, &after, &elsewhere);
            let ctx = format!("seed {} round {} elsewhere: {:?}", seed, round, words);
            assert!(!words.contains("new text") && !words.contains("gone text") && !words.contains("to press"), "{}", ctx);
            assert_eq!(words.contains(&format!("{} came into view", add)), add > 0, "{}", ctx);
            assert_eq!(words.contains("1 went out of view"), dropped.is_some(), "{}", ctx);
            assert_eq!(words.contains(&way), moved, "{}", ctx);
            assert_eq!(key.as_str(), if words.is_empty() { "" } else { "follows" }, "{}", ctx);
        }
    }

    #[test]
    fn fuzz_repeated_runs() {
        let seed = seed();
        println!("fuzz_repeated_runs FUZZ_SEED={}", seed);
        let mut s = seed;
        const NOISE: [&str; 4] = ["", "    // A comment.", "    }", "    });"];
        let mut counter = 0u64;
        let mut fresh = |s: &mut u64| { counter += 1; format!("    let v{} = {};", counter, rng_next(s) % 7) };
        for round in 0..400 {
            let min = 2 + (rng_next(&mut s) % 9) as usize;

            // Files of lines that never repeat, with lines that do not count mixed in. A block
            // planted twice, its copies broken up differently by such lines, is the one repeat.
            let mut files: Vec<Vec<String>> = Vec::new();
            for _ in 0..1 + rng_next(&mut s) % 3 {
                let mut f = Vec::new();
                for _ in 0..rng_next(&mut s) % 60 {
                    f.push(if rng_next(&mut s) % 4 == 0 { NOISE[(rng_next(&mut s) % 4) as usize].to_string() } else { fresh(&mut s) });
                }
                files.push(f);
            }
            let plant = rng_next(&mut s) % 3 != 0;
            let block: Vec<String> = (0..min + (rng_next(&mut s) % 10) as usize).map(|_| fresh(&mut s)).collect();
            let mut spots = Vec::new();
            if plant {
                for _ in 0..2 {
                    let f = (rng_next(&mut s) % files.len() as u64) as usize;
                    files[f].push(fresh(&mut s));
                    spots.push((f, files[f].len() + 1));
                    for line in &block {
                        files[f].push(line.clone());
                        if rng_next(&mut s) % 3 == 0 { files[f].push(NOISE[(rng_next(&mut s) % 4) as usize].to_string()); }
                    }
                    files[f].push(fresh(&mut s));
                }
                spots.sort();
            }
            let texts: Vec<String> = files.iter().map(|f| f.join("\n")).collect();
            let (found, _) = repeated_runs(&texts, min);
            if plant {
                assert_eq!(found.len(), 1, "round {}", round);
                assert_eq!((found[0].0, found[0].1), (block.len(), spots[0]), "round {}", round);
                assert_eq!(found[0].2, vec![(spots[1], block.len())], "round {}", round);
            } else {
                assert!(found.is_empty(), "round {}", round);
            }

            // Lines from a tiny alphabet repeat everywhere, overlapping themselves. Every run
            // reported has to be a real one.
            let mut texts: Vec<String> = Vec::new();
            for _ in 0..2 {
                let mut f: Vec<&str> = Vec::new();
                for _ in 0..rng_next(&mut s) % 80 { f.push(["a", "b", "c", "", "}"][(rng_next(&mut s) % 5) as usize]); }
                texts.push(f.join("\n"));
            }
            let counted: Vec<Vec<(usize, &str)>> = texts.iter().map(|t| t.lines().enumerate().filter(|(_, l)| counts(l)).map(|(n, l)| (n + 1, l.trim())).collect()).collect();
            let (found, total) = repeated_runs(&texts, min);
            assert_eq!(total, counted[0].len() + counted[1].len());
            for (longest, first, copies) in &found {
                assert!(*longest >= min && !copies.is_empty(), "round {}", round);
                for (copy, len) in copies {
                    let (a, b) = (&counted[first.0], &counted[copy.0]);
                    let (ia, ib) = (a.iter().position(|l| l.0 == first.1).unwrap(), b.iter().position(|l| l.0 == copy.1).unwrap());
                    assert!(*len >= min && len <= longest, "round {}", round);
                    for k in 0..*len { assert_eq!(a[ia + k].1, b[ib + k].1, "round {}", round); }
                }
            }
        }
    }

    #[test]
    fn fuzz_lint() {
        let seed = seed();
        println!("fuzz_lint FUZZ_SEED={}", seed);
        let mut s = seed;
        let mut w = ScriptWindow::open("", "").unwrap();
        w.resize(512, 512);
        let window = [0, 0, 512, 512];
        for round in 0..600 {
            let mut r = |n: i32| (rng_next(&mut s) % n as u64) as i32;
            // One text in a box, the box's sides anywhere from inside the text to well clear of it.
            let (tx, ty, tw, th) = (200, 100 + r(50), 20 + r(130), 6 + r(24));
            let (left, right) = (r(36) - 6, r(36) - 6);
            let full = [tx, ty, tx + tw, ty + th];
            let cut_right = if r(3) == 0 { r(tw + 10) } else { 0 };
            let clip = if cut_right > 0 { [0, 0, tx + tw - cut_right, 512] } else { window };
            let covered = r(5) == 0;
            let (ink, bg) = loop {
                let (a, b) = (rng_next(&mut s) as u32 & 0xffffff, rng_next(&mut s) as u32 & 0xffffff);
                if (0..3).any(|i| ((a >> (i * 8)) & 255).abs_diff((b >> (i * 8)) & 255) >= 40) { break (a, b); }
            };

            w.pixels.fill(bg);
            let vis = cut(full, clip);
            // The first column stays background, so even a text cut to one pixel has some to read.
            for y in vis[1]..vis[3] { for x in (vis[0] + 1..vis[2]).step_by(3) { w.pixels[y as usize * w.side + x as usize] = ink; } }
            w.clips = vec![(0, window), (2, clip)];
            w.rects = vec![
                LoggedRect { b: [0, 0, 150, 512], color: 0xff000000 | !bg & 0xffffff, at: 1, radius: 0 },
                LoggedRect { b: cut([tx - left, ty - 5, tx + tw + right, ty + th + 5], clip), color: 0xff000000 | bg, at: 3, radius: 0 },
                // Too faint to hide anything.
                LoggedRect { b: window, color: 0x10ffffff, at: 20, radius: 0 },
            ];
            if covered { w.rects.push(LoggedRect { b: [tx - 50, ty - 50, tx + tw + 50, ty + th + 50], color: 0xff000000 | bg, at: 30, radius: 0 }); }
            w.texts = vec![LoggedText { x: tx, y: ty, w: tw as u32, h: th as u32, color: ink | (rng_next(&mut s) as u32) << 24, at: 5, text: "text".into() }];

            let mut pick = |n: u64| (rng_next(&mut s) % n) as i32;
            let (gap_right, gap_bottom, target_w, target_h, hidden) = (pick(20), pick(20), 10 + pick(60), 10 + pick(60), pick(4) == 0);
            let target = [512 - gap_right - target_w, 512 - gap_bottom - target_h, 512 - gap_right, 512 - gap_bottom];
            w.targets = vec![LoggedTarget { b: target, enabled: true, pointer: false, grip: String::new() }];
            w.rects.push(LoggedRect { b: target, color: 0xff000000 | bg, at: 40, radius: 0 });
            if hidden { w.rects.push(LoggedRect { b: [250, 250, 512, 512], color: 0xff000000 | bg, at: 50, radius: 0 }); }

            let mut want: Vec<&str> = Vec::new();
            if !hidden {
                if target_w.min(target_h) < 24 { want.push("target"); }
                if (1..=12).contains(&gap_right) { want.push("edge"); }
            }
            if area(vis) > 0 && !covered {
                if cut_right > 1 { want.push("clipped"); }
                else {
                    let side = left.min(if cut_right == 1 { right.min(-1) } else { right });
                    if side < -1 { want.push("overflow"); } else if side < 4 { want.push("margin"); }
                }
                if contrast(ink, bg) < 3.0 { want.push("contrast"); }
                if th < 10 { want.push("tiny"); }
            }
            let mut got: Vec<&str> = w.lint().iter().map(|f| f.kind).collect();
            got.sort();
            want.sort();
            assert_eq!(got, want, "round {}: text {:?} sides {} {} cut {} covered {} ink {:06x} bg {:06x}", round, full, left, right, cut_right, covered, ink, bg);
        }
    }

    #[test]
    fn fuzz_png_roundtrip() {
        let seed = seed();
        println!("fuzz_png_roundtrip FUZZ_SEED={}", seed);
        let mut s = seed;
        for _ in 0..400 {
            let (w, h) = (1 + (rng_next(&mut s) % 70) as usize, 1 + (rng_next(&mut s) % 70) as usize);
            // Runs of every length around the match limits, noise, and flat rows that the Up
            // filter turns into zero runs spanning rows.
            let mut rgb = vec![0u8; w * h * 3];
            let mut i = 0;
            let style = rng_next(&mut s) % 4;
            while i < rgb.len() {
                let run = match style { 0 => 1, 1 => 1 + (rng_next(&mut s) % 600) as usize, 2 => w * 3 * (1 + (rng_next(&mut s) % 3) as usize), _ => 1 + (rng_next(&mut s) % 8) as usize };
                let v = rng_next(&mut s) as u8;
                for _ in 0..run { if i < rgb.len() { rgb[i] = v; i += 1; } }
            }
            let png = png_encode(&rgb, w, h);
            let (back, bw, bh) = png_decode(&png).expect("our own PNG decodes");
            assert_eq!((bw, bh), (w, h));
            assert!(back == rgb, "roundtrip mismatch at {}x{} style {}", w, h, style);

            // A damaged file is refused or decoded, never a panic.
            let mut bad = png.clone();
            for _ in 0..3 {
                let at = (rng_next(&mut s) % bad.len() as u64) as usize;
                bad[at] ^= 1 << (rng_next(&mut s) % 8);
            }
            bad.truncate(bad.len() - (rng_next(&mut s) % 4) as usize);
            let _ = png_decode(&bad);
        }
    }

    #[test]
    fn fuzz_split_args_and_key_names() {
        let seed = seed();
        println!("fuzz_split_args_and_key_names FUZZ_SEED={}", seed);
        let mut s = seed;
        for _ in 0..2000 {
            let words: Vec<String> = (0..rng_next(&mut s) % 5).map(|_| {
                (0..1 + rng_next(&mut s) % 6).map(|_| (b'a' + (rng_next(&mut s) % 26) as u8) as char).collect::<String>()
                    + if rng_next(&mut s) % 3 == 0 { " x" } else { "" }
            }).collect();
            let line = words.iter().map(|w| if w.contains(' ') { format!("\"{}\"", w) } else { w.clone() }).collect::<Vec<_>>().join("  ");
            assert_eq!(split_args(&line), words, "line {:?}", line);
        }
        for name in ["a", "z", "0", "9", "enter", "F12", "mouse"] { assert!(key_code(name).is_some(), "{}", name); }
        assert!(key_code("nosuchkey").is_none());
    }

    #[test]
    fn fuzz_reflow() {
        let seed = seed();
        println!("fuzz_reflow FUZZ_SEED={}", seed);
        let mut s = seed;
        for round in 0..800 {
            let mut pick = |n: u64| (rng_next(&mut s) % n.max(1)) as usize;

            // A sweep of faults, each planted over steps of its own choosing: all of them, one
            // stretch, one step, or a stretch with holes in it.
            let n = 1 + pick(40);
            let mut steps: Vec<Vec<(String, f64)>> = vec![Vec::new(); n];
            let mut want: Vec<(Span, f64)> = Vec::new();
            for k in 0..pick(12) {
                let key = format!("kind\ttext {}\tN px off", k);
                let (a, b) = (pick(n as u64), pick(n as u64));
                let (first, last) = match pick(4) { 0 => (0, n - 1), 1 => (a, a), _ => (a.min(b), a.max(b)) };
                let holes = pick(3) == 0;
                let mut span = Span { key: key.clone(), first: n, last: 0, held: 0, worst: 0 };
                let mut worst = f64::MIN;
                for i in first..=last {
                    if holes && i != first && i != last && pick(2) == 0 { continue; }
                    // The same fault can show more than once at a step, on texts that read alike.
                    for _ in 0..1 + pick(3) / 2 {
                        let bad = pick(50) as f64 - 25.0;
                        steps[i].push((key.clone(), bad));
                        if bad > worst { worst = bad; span.worst = i; }
                    }
                    span.first = span.first.min(i);
                    span.last = i;
                    span.held += 1;
                }
                want.push((span, worst));
            }
            for step in &mut steps { for i in (1..step.len()).rev() { step.swap(i, pick(i as u64 + 1)); } }
            let per_step: Vec<Vec<(&str, f64)>> = steps.iter().map(|v| v.iter().map(|f| (f.0.as_str(), f.1)).collect()).collect();
            let got = fault_spans(&per_step);
            assert_eq!(got.len(), want.len(), "round {}", round);
            for pair in got.windows(2) { assert!(pair[0].held >= pair[1].held, "round {}", round); }
            for (span, worst) in &want {
                let g = got.iter().find(|g| g.key == span.key).unwrap_or_else(|| panic!("round {}: {} is missing", round, span.key));
                assert_eq!((g.first, g.last, g.held), (span.first, span.last, span.held), "round {}", round);
                assert!(steps[g.worst].iter().any(|f| f.0 == span.key && f.1 == *worst), "round {}: {} was not at its worst at step {}", round, span.key, g.worst);

                let words = span_words(g, n, &|i| format!("<{}>", i));
                let names = |i: usize| words.contains(&format!("<{}>", i));
                if g.held == n { assert_eq!(words, "at every step", "round {}", round); continue; }
                assert!(names(g.first) || g.first == 0, "round {}: {:?} for {:?}", round, words, g);
                assert!(names(g.last) || g.last == n - 1, "round {}: {:?} for {:?}", round, words, g);
                assert_eq!(words.starts_with(&format!("at {} of", g.held)), g.held < g.last - g.first + 1, "round {}: {:?} for {:?}", round, words, g);
            }

            // Two frames of texts, some shown twice, some in one frame only. A known few of
            // those shown once in both move further than allowed.
            let allow = (pick(40) as i32, pick(40) as i32);
            let (mut a, mut b): (Vec<Shown>, Vec<Shown>) = (Vec::new(), Vec::new());
            let (mut far, mut most_past) = (0, 0);
            for i in 0..pick(40) {
                let at = Shown { x: pick(2000) as i32, y: pick(1200) as i32, w: 40, h: 12, color: String::new(), text: format!("text {}", i) };
                let mut moved = at.clone();
                let jump = pick(3) == 0;
                let sign = |v: i32, k: usize| if k % 2 == 0 { v } else { -v };
                let (dx, dy) = if jump {
                    if pick(2) == 0 { (sign(allow.0 + 1 + pick(300) as i32, pick(2)), sign(pick(allow.1 as u64 + 1) as i32, pick(2))) }
                    else { (sign(pick(allow.0 as u64 + 1) as i32, pick(2)), sign(allow.1 + 1 + pick(300) as i32, pick(2))) }
                } else { (sign(pick(allow.0 as u64 + 1) as i32, pick(2)), sign(pick(allow.1 as u64 + 1) as i32, pick(2))) };
                moved.x += dx;
                moved.y += dy;
                match pick(8) {
                    0 => { a.push(at.clone()); a.push(Shown { y: at.y + 500, ..at }); b.push(moved); }
                    1 => { a.push(at); b.push(moved.clone()); b.push(Shown { x: moved.x + 900, ..moved }); }
                    2 => a.push(at),
                    3 => b.push(moved),
                    _ => {
                        a.push(at);
                        b.push(moved);
                        if jump { far += 1; most_past = most_past.max((dx.abs() - allow.0).max(dy.abs() - allow.1)); }
                    }
                }
            }
            for v in [&mut a, &mut b] { for i in (1..v.len()).rev() { v.swap(i, pick(i as u64 + 1)); } }
            let (count, most) = moved_far(&shown_once(a), &shown_once(b), allow);
            assert_eq!(count, far, "round {}", round);
            assert_eq!(most.map(|m| (m.1.0.abs() - allow.0).max(m.1.1.abs() - allow.1)).unwrap_or(0), most_past, "round {}", round);

            let count = pick(121);
            let film = film_steps(count);
            assert!(film.len() <= 8 && film[0] == 0 && *film.last().unwrap() == count, "round {}: {:?}", round, film);
            assert!(film.windows(2).all(|p| p[0] < p[1]), "round {}: {:?}", round, film);
        }
        assert_eq!(no_numbers("12 px, 3.5:1 on 00ff00"), "N px, N.N:N on NffN");
        assert!(severity("clipped", "61 px cut off") > severity("clipped", "7 px cut off"));
        assert!(severity("margin", "1 px from the side") > severity("margin", "3 px from the side"));
    }
}
