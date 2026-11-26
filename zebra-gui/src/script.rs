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
#[derive(Clone)]
struct Fault { key: String, kind: &'static str, text: String, detail: String, b: [i32; 4] }

/// A fault over a sweep: the first and last steps it held at, how many it held at, and the
/// step it was at its worst.
#[derive(Debug, Clone, PartialEq)]
struct Span { key: String, first: usize, last: usize, held: usize, worst: usize }

/// Something `explore` presses: its box, and the text that names it.
#[derive(Debug, Clone, PartialEq, Default)]
struct Aim { b: [i32; 4], label: String }

impl Aim {
    /// How a report names it: by its text, or by its place if it has no text of its own.
    fn say(&self) -> String {
        if self.label.starts_with('(') || self.label.starts_with('<') { format!("{} at {},{}", self.label, self.b[0], self.b[1]) } else { quoted(&self.label) }
    }
}

/// A screen at rest, as `explore` tells one from another: its texts and what can be pressed,
/// its lint findings, and its pixels.
struct Look { shown: (Vec<Shown>, Vec<Shown>), faults: Vec<Fault>, pixels: Vec<u32> }

/// What a press changed on a screen, by name: the texts and the things to press that it
/// brought and that it took away, each with how many the screen had before, and whether it
/// is something to press. A later screen has the press undone when none of what it brought
/// is left and all it took away is back, whatever else has changed since: a press that
/// nothing undoes, such as one on a slider, leaves its mark on every screen after it.
#[derive(Debug, Clone, PartialEq, Default)]
struct Change { brought: Vec<(bool, String, usize)>, took: Vec<(bool, String, usize)>, was: (Vec<Shown>, Vec<Shown>) }

impl Change {
    fn between(was: &(Vec<Shown>, Vec<Shown>), now: &(Vec<Shown>, Vec<Shown>)) -> Change {
        let count = |v: &[Shown], name: &str| v.iter().filter(|s| s.text == name).count();
        let mut change = Change { was: was.clone(), ..Change::default() };
        for (press, a, b) in [(false, &was.0, &now.0), (true, &was.1, &now.1)] {
            let mut names: Vec<&str> = a.iter().chain(b.iter()).map(|s| s.text.as_str()).collect();
            names.sort();
            names.dedup();
            for name in names {
                // A scroll bar's thumb has no text, and comes and goes with what it scrolls.
                if press && name == "(no text)" { continue; }
                let (n, m) = (count(a, name), count(b, name));
                if m > n { change.brought.push((press, name.to_string(), n)); }
                if m < n { change.took.push((press, name.to_string(), n)); }
            }
        }
        change
    }

    fn undone(&self, now: &(Vec<Shown>, Vec<Shown>)) -> bool {
        // A press that only moved or recoloured things is undone when they are all back.
        if self.brought.is_empty() && self.took.is_empty() { return *now == self.was; }
        let count = |press: bool, name: &str| (if press { &now.1 } else { &now.0 }).iter().filter(|s| s.text == name).count();
        self.brought.iter().all(|(press, name, n)| count(*press, name) <= *n) && self.took.iter().all(|(press, name, n)| count(*press, name) >= *n)
    }

    /// Whether something the press brought is on a later screen. What it took away says
    /// less: a press that opens one menu may be what shuts another.
    fn shows(&self, now: &(Vec<Shown>, Vec<Shown>)) -> bool {
        let count = |press: bool, name: &str| (if press { &now.1 } else { &now.0 }).iter().filter(|s| s.text == name).count();
        self.brought.iter().any(|(press, name, n)| count(*press, name) > *n)
    }
}

/// The level a screen is that of, if a press has been undone on it: the level before the
/// outermost one whose opening is undone, or else the deepest if `own` is undone.
fn level_of(levels: &[Level], own: Option<&Change>, now: &(Vec<Shown>, Vec<Shown>)) -> Option<usize> {
    if let Some(j) = (1..levels.len()).find(|j| levels[*j].change.undone(now)) { return Some(j - 1); }
    own.filter(|c| c.undone(now)).map(|_| levels.len() - 1)
}

/// How many pixels differ between two screens of one size: outside a box, and inside it.
fn differing(a: &[u32], b: &[u32], width: usize, apart: Option<[i32; 4]>) -> (usize, usize) {
    if a.len() != b.len() { return (a.len().max(b.len()), 0); }
    let (mut out, mut inside) = (0, 0);
    for (i, (p, q)) in a.iter().zip(b).enumerate() {
        if (p ^ q) & 0xffffff == 0 { continue; }
        let at = [(i % width.max(1)) as i32, (i / width.max(1)) as i32];
        if apart.is_some_and(|r| holds(r, at)) { inside += 1; } else { out += 1; }
    }
    (out, inside)
}

/// Takes a level up again on the screen as it now is. What is left of its list from `from`
/// on is found where it now lies, by its box or else by its name and size, since a press
/// that nothing undid may have moved it. Gives what could not be found.
fn refit(level: &mut Level, from: usize, look: Look) -> Vec<String> {
    let boxed = |s: &Shown| [s.x, s.y, s.x + s.w, s.y + s.h];
    let mut rest: Vec<(Aim, bool)> = level.list.split_off(from.min(level.list.len())).into_iter().map(|a| (a, false)).collect();
    let mut free: Vec<&Shown> = look.shown.1.iter().collect();
    for (a, found) in &mut rest {
        if let Some(p) = free.iter().position(|s| boxed(s) == a.b) { free.swap_remove(p); *found = true; }
    }
    for (a, found) in &mut rest {
        if *found { continue; }
        let size = (a.b[2] - a.b[0], a.b[3] - a.b[1]);
        let near = free.iter().enumerate().filter(|(_, s)| s.text == a.label && (s.w, s.h) == size).min_by_key(|(_, s)| (s.x - a.b[0]).abs() + (s.y - a.b[1]).abs()).map(|(p, _)| p);
        if let Some(p) = near { a.b = boxed(free[p]); free.swap_remove(p); *found = true; }
    }
    let lost = rest.iter().filter(|r| !r.1).map(|r| format!("{}{}", level.path, r.0.say())).collect();
    level.list.extend(rest.into_iter().filter(|r| r.1).map(|r| r.0));
    level.look = look;
    lost
}

/// A screen `explore` has reached, and how far it is through what can be pressed there.
struct Level {
    look: Look,
    /// The press that led here from the level above, that press's row in the report, and
    /// what it changed.
    via: Option<(Aim, usize)>,
    change: Change,
    /// What, pressed here, led back to the level above, as a close button does.
    closers: Vec<Aim>,
    list: Vec<Aim>,
    i: usize,
    /// What the names of the screenshots taken from here start with.
    lead: String,
    /// The presses that led here, in words, for the next press to follow.
    path: String,
}

/// A press that led to another screen: what changed, how the press is undone, the faults the
/// new screen has that the one before it did not, and its screenshot.
struct Row { path: String, words: String, back: String, brought: Vec<String>, file: String }

/// A way a press may be undone.
#[derive(Debug, Clone, PartialEq)]
enum Try { Esc, Again, Beside(Aim), Closer(Aim), Wait }

#[derive(Default)]
enum Stage {
    #[default]
    Start,
    /// Press the next thing of the deepest level, or leave the level when nothing is left.
    Press,
    /// The press has come to rest.
    Seen,
    /// A second press, after a first that only redrew, has come to rest.
    Redone,
    /// So many of `tries` have been made, and the last has come to rest.
    Back(usize),
    /// Time is passing to see whether the press undoes itself: the try to check after it,
    /// and how many frames are left, each a sixth of a second after the last.
    Wait(usize, usize),
    /// The screen is that of the level before this one: press the way into this one again.
    Enter(usize),
    Entered(usize),
    /// Press again, one after another, each thing a second press closed.
    Open(usize),
    /// The one just pressed open has come to rest.
    Opened(usize),
    OpenSeen,
    /// Close them, last first: how many are left.
    Close(usize),
    Done,
}

/// Something pressed open among the others that a second press closes: where it was pressed,
/// the screen before that and after it, what the press changed, and the name the thing has
/// since.
struct Opened { aim: Aim, was: (Vec<Shown>, Vec<Shown>), after: (Vec<Shown>, Vec<Shown>), change: Change, open_as: String }

/// `explore` in progress, and what it has found so far. It presses each thing on the screen
/// in turn, looks at the screen that leaves, presses what is new there, and undoes each
/// press before the next, so every press starts from a screen it knows.
#[derive(Default)]
struct Explore {
    name: String,
    depth: usize,
    /// The most presses it makes.
    most: usize,
    /// Where the pointer waits between presses, on nothing, and where it was at the start.
    rest: (i32, i32),
    mouse: (i32, i32),
    stage: Stage,
    levels: Vec<Level>,
    /// The press being undone with its row and what it changed, and the ways to try in order.
    undo: (Aim, usize, Change),
    tries: Vec<Try>,

    /// Whether frames are being drawn until `need` in a row change nothing.
    waiting: bool,
    need: usize,
    prev: Vec<u32>,
    still: usize,
    frames: usize,
    /// How often the screen was still changing 90 frames after a press.
    restless: usize,

    rows: Vec<Row>,
    /// Presses that led back to a screen passed on the way.
    closes: Vec<String>,
    /// Presses that changed pixels and no text: how many pixels, and whether a second press
    /// put them back.
    drawn: Vec<(String, usize, bool)>,
    /// Presses that changed nothing.
    quiet: Vec<String>,
    /// What a press left drawn that a second press did not take away, as a text field its
    /// caret. It goes when something else is pressed, and that is not the other press's doing.
    caret: Option<[i32; 4]>,
    /// What on the first screen a second press closed, and each of them as it was pressed
    /// to have them all open at once.
    toggles: Vec<(Aim, Change)>,
    opened: Vec<Opened>,
    /// How the screen differs from what it was once all that was open at once is closed.
    not_closed: String,
    /// Every press and every try made, in order, each as the script command that makes it.
    trail: Vec<String>,
    pressed: usize,
    on_screen: usize,
    disabled: usize,
    alike: usize,
    /// What was to be pressed and was not: the screen it was on could not be reached again,
    /// or `most` ran out.
    unpressed: Vec<String>,
    used: Vec<String>,
}

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
    explore: Option<Explore>,
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

/// The window the GUI is designed for, and the smallest it has to stay whole in: the room a
/// maximized window has on a 1920x1080 and on a 1280x720 display at scale 1. A title bar and
/// a task bar take about 80 px of the display's height.
const MAINLINE: (usize, usize) = (1920, 1000);
const MINSPEC: (usize, usize) = (1280, 640);

/// A window size among a command's arguments, from argument `at`: `mainline`, `minspec`, or a
/// width and a height. Also gives where the arguments after it start.
fn size_arg(args: &[String], at: usize) -> Option<(usize, usize, usize)> {
    match args.get(at).map(|a| a.as_str()) {
        Some("mainline") => Some((MAINLINE.0, MAINLINE.1, at + 1)),
        Some("minspec") => Some((MINSPEC.0, MINSPEC.1, at + 1)),
        _ => {
            let w = args.get(at)?.parse::<f64>().ok()?;
            let h = args.get(at + 1)?.parse::<f64>().ok()?;
            if w >= 1.0 && h >= 1.0 && w <= 8192.0 && h <= 8192.0 { Some((w as usize, h as usize, at + 2)) } else { None }
        }
    }
}

impl ScriptWindow {
    /// `script` is a file of commands, one per line, or the commands themselves separated by `;`.
    pub fn open(script: &str, out_dir: &str) -> Option<ScriptWindow> {
        let text = match std::fs::read_to_string(script) {
            Ok(t) => t,
            Err(_) if !Path::new(script).exists() => script.replace(';', "\n"),
            Err(e) => { eprintln!("{}: {}", script, e); return None; }
        };
        // `include <file>` stands for the lines of another script beside this one.
        let mut lines = Vec::new();
        for l in text.lines().map(str::trim) {
            match l.strip_prefix("include ") {
                Some(name) => {
                    let file = Path::new(script).parent().unwrap_or(Path::new("")).join(name.trim());
                    match std::fs::read_to_string(&file) {
                        Ok(t) => lines.extend(t.lines().map(|l| l.trim().to_string())),
                        Err(e) => { eprintln!("include {}: {}", file.display(), e); return None; }
                    }
                }
                None => lines.push(l.to_string()),
            }
        }
        let mut w = ScriptWindow {
            lines,
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
            explore: None,
        };
        w.resize(MAINLINE.0, MAINLINE.1);
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

    fn push_render(&mut self) { self.push_render_of(1); }

    /// A frame that many sixtieths of a second after the last, as a slow machine draws one.
    fn push_render_of(&mut self, sixtieths: u64) {
        self.events.push_back(Event {
            kind: EVENT_RENDER,
            width: self.width as u32,
            height: self.height as u32,
            scale_fx: (self.scale * 65536.0) as u32,
            cursor_x: (self.mouse.0 as i64) << 32,
            cursor_y: (self.mouse.1 as i64) << 32,
            dt_fs: 1_000_000_000_000_000 * sixtieths / 60,
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
            "size" => match size_arg(&args, 0) {
                Some((w, h, _)) => self.resize(w, h),
                None => fail(&format!("line {}: size <width> <height>, or size mainline, or size minspec", self.pc + 1)),
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
                self.keep_screen(&name, [x, y, w, h], [x as i32, y as i32, (x + w) as i32, (y + h) as i32]);
                let found = self.lint();
                self.keep_lint(&name, &found);
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
                    let rest = self.rest_spot();
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
                    let usage = if window { "resize <name> <width> <height> [steps], or resize <name> minspec [steps]" } else { "drag <name> <\"grip\" or \"text\", or x y> <dx> <dy> [steps]" };
                    let (from, to, count) = if window {
                        match size_arg(&args, 1) {
                            Some((w, h, next)) => ((self.width as i32, self.height as i32), (w as i32, h as i32), num(next)),
                            None => { fail(&format!("line {}: {}", self.pc + 1, usage)); return true; }
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
                    let faults = self.faults();
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
            "explore" => if self.explore_step(&args).is_none() { return false; },
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
                let mut totals: Vec<usize> = b.samples.iter().map(|s| s.1).collect();
                totals.sort();
                let p95 = totals.get((totals.len() * 95 / 100).min(totals.len().saturating_sub(1))).copied().unwrap_or(0);
                println!("{} {} frames {}x{} us: layout [{}] total [{}]; p95 is {}% of a 60 Hz frame", cmd, b.samples.len(), self.width, self.height, stat(|s| s.0), stat(|s| s.1), p95 * 100 * 60 / 1_000_000);
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
        const BROUGHT: [&str; 6] = ["clipped", "outside", "overflow", "overlap", "margin", "target"];
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

    /// The screen as `explore` tells one from another. Lint is the dear part of it, so a
    /// screen that shows what one of `levels` shows is given that level's findings.
    fn look(&self, levels: &[Level]) -> Look {
        let shown = self.showing([0, 0, self.width as i32, self.height as i32]);
        let faults = match levels.iter().find(|l| l.look.shown == shown) { Some(l) => l.look.faults.clone(), None => self.faults() };
        Look { shown, faults, pixels: self.window_pixels() }
    }

    /// What `explore` presses on the screen as it is: each thing a person can reach, but what
    /// `known`, the things to press on the screen before, already had under the same name at
    /// the same size, wherever it lies now. Of four or more of one size in a column it takes
    /// the first and the last. Also gives how many it left out as disabled, and as alike.
    fn aims(&self, known: &[Shown]) -> (Vec<Aim>, usize, usize) {
        let window = [0, 0, self.width as i32, self.height as i32];
        let shown = self.shown_texts();
        let mut known: Vec<(&str, i32, i32)> = known.iter().map(|k| (k.text.as_str(), k.w, k.h)).collect();
        let (mut list, mut disabled) = (Vec::new(), 0);
        for t in self.shown_targets() {
            let label = label_among(&shown, t.b).replace(['\t', '\n'], " ");
            if let Some(i) = known.iter().position(|k| *k == (label.as_str(), t.b[2] - t.b[0], t.b[3] - t.b[1])) {
                known.swap_remove(i);
                continue;
            }
            // What its list has scrolled out of sight cannot be pressed.
            if area(cut(cut(t.b, window), self.clip_at(self.drawn_at(t.b).unwrap_or(0)))) == 0 { continue; }
            if !t.enabled { disabled += 1; continue; }
            list.push(Aim { b: t.b, label });
        }
        let column = |a: &Aim| (a.b[0], a.b[2], a.b[3] - a.b[1]);
        let all = list.clone();
        list.retain(|a| {
            let mut ys: Vec<i32> = all.iter().filter(|o| column(o) == column(a)).map(|o| o.b[1]).collect();
            ys.sort();
            ys.len() < 4 || a.b[1] == ys[0] || a.b[1] == ys[ys.len() - 1]
        });
        let alike = all.len() - list.len();
        (list, disabled, alike)
    }

    /// Where a box is pressed: on a part of it that can be seen and that nothing smaller to
    /// press lies on.
    fn press_spot(&self, b: [i32; 4]) -> [i32; 2] {
        let window = [0, 0, self.width as i32, self.height as i32];
        let vis = cut(cut(b, window), self.clip_at(self.drawn_at(b).unwrap_or(0)));
        let vis = if area(vis) > 0 { vis } else { cut(b, window) };
        let (w, h) = (vis[2] - vis[0], vis[3] - vis[1]);
        let spots = [(2, 2), (1, 2), (3, 2), (2, 1), (2, 3)].map(|(i, j)| [vis[0] + w * i / 4, vis[1] + h * j / 4]);
        let free = |p: &&[i32; 2]| !self.targets.iter().any(|t| t.b != b && area(t.b) < area(b) && holds(t.b, **p));
        *spots.iter().find(free).unwrap_or(&spots[0])
    }

    /// A line of the record of presses: the script command that presses a box, and why.
    fn trail_press(&self, b: [i32; 4], why: &str) -> String {
        let at = self.press_spot(b);
        format!("{:<16}{}", format!("click {} {}", at[0], at[1]), why)
    }

    /// Presses the mouse on a box, then parks the pointer.
    fn press_at(&mut self, b: [i32; 4], rest: (i32, i32)) {
        let at = self.press_spot(b);
        // The hover has to be drawn before the press, as for `click`.
        self.push_move(at[0], at[1]);
        self.push_render();
        self.push_button(BTN_LEFT, true);
        self.push_render();
        self.push_button(BTN_LEFT, false);
        self.push_render();
        self.push_move(rest.0, rest.1);
        self.push_render();
    }

    /// The ways a press may be undone, in the order to try them, given the pixels before the
    /// press and after it. What is still there to press is pressed again first. Then come
    /// its neighbours in a row that the press redrew, as one tab dims when another is chosen:
    /// pressing any other neighbour would only add that one's own doing. Esc comes after
    /// them, since it may close more than the press opened, and first for what the press
    /// covered. Waiting is last.
    fn undo_tries(&self, aim: &Aim, was: &[u32], after: &[u32]) -> Vec<Try> {
        let now = self.shown_targets();
        let there = self.still_there(aim);
        let (w, h) = (self.width as i32, self.height as i32);
        let redrawn = |b: [i32; 4]| {
            let v = cut(b, [0, 0, w, h]);
            was.len() != after.len() || was.len() != (w * h) as usize || (v[1]..v[3]).any(|y| (v[0]..v[2]).any(|x| (was[(y * w + x) as usize] ^ after[(y * w + x) as usize]) & 0xffffff != 0))
        };
        let mut tries = if there { vec![Try::Again] } else { vec![Try::Esc, Try::Again] };
        let shown = self.shown_texts();
        let mut beside: Vec<&LoggedTarget> = now.iter().filter(|t| there && t.enabled && t.b != aim.b && (t.b[1] - aim.b[1]).abs() <= 2 && (t.b[3] - aim.b[3]).abs() <= 2 && redrawn(t.b)).collect();
        beside.sort_by_key(|t| (t.b[0] + t.b[2] - aim.b[0] - aim.b[2]).abs());
        for t in beside.into_iter().take(3) { tries.push(Try::Beside(Aim { b: t.b, label: label_among(&shown, t.b) })); }
        if there { tries.push(Try::Esc); }
        tries.push(Try::Wait);
        tries
    }

    /// Whether what was pressed can be pressed there again: its box is still one to press, or
    /// one of its name lies where it was pressed and has about its corner, as a row does that
    /// opens in place. The page may have grown a scroll bar as it opened, and moved it a little.
    fn still_there(&self, aim: &Aim) -> bool {
        let (b, at) = (aim.b, self.press_spot(aim.b));
        let shown = self.shown_texts();
        self.shown_targets().iter().any(|t| t.b == b
            || ((t.b[0] - b[0]).abs() <= 16 && (t.b[1] - b[1]).abs() <= 16 && holds(t.b, at) && label_among(&shown, t.b).replace(['\t', '\n'], " ") == aim.label))
    }

    /// Where something pressed before is now: the nearest thing to press that has its name.
    fn find_aim(&self, aim: &Aim) -> Option<[i32; 4]> {
        let shown = self.shown_texts();
        self.shown_targets().iter().filter(|t| t.enabled && label_among(&shown, t.b).replace(['\t', '\n'], " ") == aim.label)
            .min_by_key(|t| (t.b[0] - aim.b[0]).abs() + (t.b[1] - aim.b[1]).abs()).map(|t| t.b)
    }

    /// Keeps a screen a press led to: its screenshot, what it shows, and the findings and the
    /// style that the screen before it did not have. The rest is on record where that screen
    /// was shot, so a fault of the page is not counted again for each thing that opens on it.
    fn keep_look(&mut self, name: &str, path: String, was: &Look, now: &Look) -> Row {
        const WORST_FIRST: [&str; 9] = ["outside", "clipped", "overlap", "overflow", "target", "margin", "edge", "contrast", "tiny"];
        let (mut words, _) = describe_change(&was.shown, &now.shown, &Known::default());
        if words.is_empty() { words = "the same texts, drawn otherwise".to_string(); }
        let mut brought: Vec<&Fault> = now.faults.iter().filter(|f| !was.faults.iter().any(|g| g.key == f.key)).collect();
        brought.sort_by_key(|f| WORST_FIRST.iter().position(|k| *k == f.kind));
        for f in &brought { self.lint_lines.push(lint_line(name, f.kind, &f.text, f.b, &f.detail)); }
        let (_, texts, _) = pair_up(&was.shown.0, &now.shown.0);
        let (_, press, _) = pair_up(&was.shown.1, &now.shown.1);
        let new = texts.iter().chain(&press).fold([i32::MAX, i32::MAX, i32::MIN, i32::MIN], |b, s| [b[0].min(s.x), b[1].min(s.y), b[2].max(s.x + s.w), b[3].max(s.y + s.h)]);
        self.keep_screen(name, [0, 0, self.width, self.height], new);
        Row { path, words, back: String::new(), brought: brought.iter().map(|f| format!("{} {} {}", f.kind, quoted(&f.text), f.detail)).collect(), file: format!("{}.png", name) }
    }

    /// One turn of `explore`: reads the frames the last turn asked for, then presses the next
    /// thing or tries the next way back. Gives what it found once everything is pressed, and
    /// nothing until then, having queued the input and the frames its next turn reads.
    fn explore_step(&mut self, args: &[String]) -> Option<Explore> {
        let window = [0, 0, self.width as i32, self.height as i32];
        if self.explore.is_none() {
            if !self.need_fresh() { return None; }
            let num = |i: usize, or: usize| args.get(i).and_then(|a| a.parse::<usize>().ok()).unwrap_or(or);
            let rest = self.rest_spot();
            self.explore = Some(Explore { name: args.first().cloned().unwrap_or_else(|| "explore".to_string()), depth: num(1, 2).clamp(1, 4), most: num(2, 200),
                rest, mouse: self.mouse, waiting: true, need: 10, ..Explore::default() });
            self.push_move(rest.0, rest.1);
            self.push_render();
            return None;
        }
        let mut ex = self.explore.take().unwrap();
        if ex.waiting {
            ex.still = if self.same_pixels(&mut ex.prev) { ex.still + 1 } else { 0 };
            ex.frames += 1;
            // The first wait is a long one: what a change of the window's size set moving may
            // only move once the pointer does, and comes to rest slowly.
            if ex.still < ex.need && ex.frames < 30 * ex.need {
                self.renders_without_frame = 0;
                self.push_render();
                self.explore = Some(ex);
                return None;
            }
            if ex.still < ex.need { ex.restless += 1; }
            ex.waiting = false;
            ex.need = 3;
            ex.prev = Vec::new();
        }
        loop {
            let depth = ex.levels.len();
            // Whether the stage queued input, to be read once the screen has come to rest.
            let queued = match ex.stage {
                Stage::Start => {
                    let look = self.look(&[]);
                    let (list, disabled, alike) = self.aims(&[]);
                    (ex.on_screen, ex.disabled, ex.alike) = (look.shown.1.len(), disabled, alike);
                    ex.levels.push(Level { look, via: None, change: Change::default(), closers: Vec::new(), list, i: 0, lead: ex.name.clone(), path: String::new() });
                    ex.stage = Stage::Press;
                    false
                }
                Stage::Press => {
                    let top = &ex.levels[depth - 1];
                    if top.i < top.list.len() && ex.pressed < ex.most {
                        let aim = top.list[top.i].clone();
                        note_line(self.pc, &format!("explore {}: pressing {}{}", ex.name, top.path, aim.say()));
                        ex.trail.push(self.trail_press(aim.b, &format!("{}{}", top.path, aim.say())));
                        ex.pressed += 1;
                        self.press_at(aim.b, ex.rest);
                        ex.stage = Stage::Seen;
                        true
                    } else {
                        let left: Vec<String> = top.list.iter().skip(top.i).map(|a| format!("{}{}", top.path, a.say())).collect();
                        ex.unpressed.extend(left);
                        if depth == 1 { ex.stage = Stage::Open(0); } else {
                            // Everything the press brought up has been pressed: undo the press.
                            let level = ex.levels.pop().unwrap();
                            let (aim, row) = level.via.unwrap();
                            let aim = Aim { b: self.find_aim(&aim).unwrap_or(aim.b), ..aim };
                            ex.tries = self.undo_tries(&aim, &ex.levels[depth - 2].look.pixels, &level.look.pixels);
                            // What closed it before is the way out that esc may not be.
                            let wait = ex.tries.len() - 1;
                            for (n, closer) in level.closers.into_iter().enumerate() { ex.tries.insert(wait + n, Try::Closer(closer)); }
                            ex.undo = (aim, row, level.change);
                            ex.stage = Stage::Back(0);
                        }
                        false
                    }
                }
                Stage::Seen => {
                    let now = self.look(&ex.levels);
                    let top = &ex.levels[depth - 1];
                    let aim = top.list[top.i].clone();
                    let path = format!("{}{}", top.path, aim.say());
                    let mut second = false;
                    if now.shown == top.look.shown {
                        let (px, lost) = differing(&now.pixels, &top.look.pixels, self.width, ex.caret);
                        if px == 0 {
                            ex.quiet.push(path);
                            // The caret it took away is gone for the presses after it too.
                            if lost > 0 { ex.caret = None; ex.levels[depth - 1].look = now; }
                            ex.levels[depth - 1].i += 1;
                            ex.stage = Stage::Press;
                        } else {
                            // A switch that shows its state by colour alone: a second press
                            // should put it back, and the presses after it need it back.
                            ex.drawn.push((path, px, false));
                            ex.trail.push(self.trail_press(aim.b, "  again, since it only redrew"));
                            self.press_at(aim.b, ex.rest);
                            ex.stage = Stage::Redone;
                            second = true;
                        }
                    } else if let Some(j) = level_of(&ex.levels, None, &now.shown) {
                        ex.caret = None;
                        ex.closes.push(path);
                        if j + 2 == depth { ex.levels[depth - 1].closers.push(aim); }
                        ex.levels[depth - 1].i += 1;
                        let from = ex.levels[j].i + 1;
                        ex.unpressed.extend(refit(&mut ex.levels[j], from, now));
                        ex.stage = Stage::Enter(j + 1);
                    } else {
                        ex.caret = None;
                        let lead = fresh_name(&mut ex.used, format!("{}_{}", top.lead, slug(&aim.label)));
                        ex.rows.push(self.keep_look(&lead, path.clone(), &top.look, &now));
                        let change = Change::between(&top.look.shown, &now.shown);
                        let (list, disabled, alike) = if depth < ex.depth { self.aims(&top.look.shown.1) } else { (Vec::new(), 0, 0) };
                        ex.disabled += disabled;
                        ex.alike += alike;
                        let row = ex.rows.len() - 1;
                        if list.is_empty() {
                            ex.tries = self.undo_tries(&aim, &top.look.pixels, &now.pixels);
                            ex.undo = (aim, row, change);
                            ex.stage = Stage::Back(0);
                        } else {
                            ex.levels.push(Level { look: now, via: Some((aim, row)), change, closers: Vec::new(), list, i: 0, lead, path: format!("{} > ", path) });
                            ex.stage = Stage::Press;
                        }
                    }
                    second
                }
                Stage::Redone => {
                    let now = self.look(&ex.levels);
                    let top = &mut ex.levels[depth - 1];
                    let same = differing(&now.pixels, &top.look.pixels, self.width, ex.caret).0 == 0 && now.shown == top.look.shown;
                    ex.drawn.last_mut().unwrap().2 = same;
                    let b = top.list[top.i].b;
                    ex.caret = (!same).then_some([b[0] - 4, b[1] - 4, b[2] + 4, b[3] + 4]);
                    // Whatever it left is the screen the next press starts from.
                    top.i += 1;
                    let from = top.i;
                    ex.unpressed.extend(refit(top, from, now));
                    ex.stage = Stage::Press;
                    false
                }
                Stage::Back(k) => {
                    // Which level's screen this is, if the last try has been made.
                    // An outer level's screen means the try closed what the press was in. That
                    // may take the press's change with it, or leave it standing on the page.
                    let at = if k == 0 { None } else {
                        let shown = self.showing(window);
                        level_of(&ex.levels, Some(&ex.undo.2), &shown).map(|j| (j, j + 1 == depth || shown == ex.levels[j].look.shown))
                    };
                    if let Some((j, gone)) = at {
                        let by = match &ex.tries[k - 1] {
                            Try::Esc => "esc".to_string(),
                            Try::Again => "pressing it again".to_string(),
                            Try::Beside(a) => format!("pressing {} beside it", a.say()),
                            Try::Closer(a) => format!("esc does not undo it once things in it were pressed; pressing {} in it", a.say()),
                            Try::Wait => String::new(),
                        };
                        let row = ex.undo.1;
                        ex.rows[row].back = match (j + 1 == depth, gone, by.is_empty()) {
                            (true, _, false) => format!("{} undoes it", by),
                            (true, _, true) => "it goes away by itself".to_string(),
                            (false, true, false) => format!("{} undoes it, and what it was in with it", by),
                            (false, true, true) => "it goes away by itself, and what it was in with it".to_string(),
                            (false, false, false) => format!("nothing undoes it; {} closes what it was in, and the change stays", by),
                            (false, false, true) => "nothing undoes it; what it was in goes away by itself, and the change stays".to_string(),
                        };
                        if depth == 1 && ex.tries[k - 1] == Try::Again { ex.toggles.push((ex.undo.0.clone(), ex.undo.2.clone())); }
                        ex.levels[depth - 1].i += 1;
                        // An outer level is still at the press that led further in.
                        let from = ex.levels[j].i + (j + 1 < depth) as usize;
                        let look = self.look(&ex.levels);
                        ex.unpressed.extend(refit(&mut ex.levels[j], from, look));
                        ex.stage = Stage::Enter(j + 1);
                        false
                    } else {
                        // The next way that the screen as it is lets be tried.
                        let aim = ex.undo.0.clone();
                        let now = self.shown_targets();
                        let there = |b: [i32; 4]| now.iter().any(|t| t.b == b);
                        let mut k = k;
                        while match ex.tries.get(k) { Some(Try::Again) => !self.still_there(&aim), Some(Try::Beside(a)) => !there(a.b), Some(Try::Closer(a)) => self.find_aim(a).is_none(), _ => false } { k += 1; }
                        ex.stage = Stage::Back(k + 1);
                        ex.trail.push(match ex.tries.get(k) {
                            Some(Try::Esc) => format!("{:<16}  to undo it", "key esc"),
                            Some(Try::Again) => self.trail_press(aim.b, "  to undo it: again"),
                            Some(Try::Beside(a)) => self.trail_press(a.b, &format!("  to undo it: {} beside it", a.say())),
                            Some(Try::Closer(a)) => self.trail_press(self.find_aim(a).unwrap_or(a.b), &format!("  to undo it: {}, which closed it before", a.say())),
                            Some(Try::Wait) => format!("{:<16}  to undo it: wait 3 s", "frame 200"),
                            None => format!("{:<16}  nothing undid it", ""),
                        });
                        match ex.tries.get(k).cloned() {
                            Some(Try::Closer(a)) => { let b = self.find_aim(&a).unwrap_or(a.b); self.press_at(b, ex.rest); true }
                            Some(Try::Esc) => {
                                self.push_button(1, true);
                                self.push_render();
                                self.push_button(1, false);
                                self.push_render();
                                true
                            }
                            Some(Try::Again) => { self.press_at(aim.b, ex.rest); true }
                            Some(Try::Beside(a)) => { self.press_at(a.b, ex.rest); true }
                            Some(Try::Wait) => { ex.stage = Stage::Wait(k + 1, 20); false }
                            None => {
                                // Nothing undid it. What is left to press on this level is
                                // pressed from the screen as it now is.
                                ex.rows[ex.undo.1].back = "nothing undoes it: not esc, not pressing it again, not what is beside it or closed it before, not waiting".to_string();
                                ex.levels[depth - 1].i += 1;
                                let from = ex.levels[depth - 1].i;
                                let look = self.look(&ex.levels);
                                ex.unpressed.extend(refit(&mut ex.levels[depth - 1], from, look));
                                ex.stage = Stage::Press;
                                false
                            }
                        }
                    }
                }
                Stage::Wait(next, left) => {
                    let back = level_of(&ex.levels, Some(&ex.undo.2), &self.showing(window)).is_some();
                    if left > 0 && !back {
                        self.renders_without_frame = 0;
                        self.push_render_of(10);
                        ex.stage = Stage::Wait(next, left - 1);
                        self.explore = Some(ex);
                        return None;
                    }
                    ex.stage = Stage::Back(next);
                    false
                }
                Stage::Enter(k) if k >= depth => { ex.stage = Stage::Press; false }
                Stage::Enter(k) => {
                    let via = &ex.levels[k].via.as_ref().unwrap().0;
                    let b = self.find_aim(via).unwrap_or(via.b);
                    ex.trail.push(self.trail_press(b, &format!("  back into {}", via.say())));
                    self.press_at(b, ex.rest);
                    ex.stage = Stage::Entered(k);
                    true
                }
                Stage::Entered(k) => {
                    let look = self.look(&ex.levels);
                    if !ex.levels[k].change.undone(&look.shown) {
                        // It may have kept something of what was pressed in it. The rest is
                        // pressed from the screen as it now is.
                        let from = ex.levels[k].i + (k + 1 < depth) as usize;
                        ex.unpressed.extend(refit(&mut ex.levels[k], from, look));
                        ex.stage = Stage::Enter(k + 1);
                    } else {
                        // An outer level is still at the press that led further in.
                        let left: Vec<String> = ex.levels.iter().enumerate().skip(k)
                            .flat_map(|(n, l)| l.list.iter().skip(l.i + (n + 1 < depth) as usize).map(|a| format!("{}{}", l.path, a.say()))).collect();
                        ex.unpressed.extend(left);
                        let row = ex.levels[k].via.as_ref().unwrap().1;
                        if ex.rows[row].back.is_empty() { ex.rows[row].back = "it did not open a second time".to_string(); }
                        ex.levels.truncate(k);
                        ex.levels[k - 1].i += 1;
                        let from = ex.levels[k - 1].i;
                        ex.unpressed.extend(refit(&mut ex.levels[k - 1], from, look));
                        ex.stage = Stage::Press;
                    }
                    false
                }
                Stage::Open(_) if ex.toggles.len() < 2 => { ex.stage = Stage::Done; false }
                Stage::Open(n) if n == ex.toggles.len() => { ex.stage = Stage::OpenSeen; false }
                Stage::Open(n) => {
                    ex.stage = Stage::Open(n + 1);
                    match self.find_aim(&ex.toggles[n].0) {
                        Some(b) => {
                            let aim = Aim { b, label: ex.toggles[n].0.label.clone() };
                            ex.trail.push(self.trail_press(b, &format!("to have all open: {}", aim.say())));
                            ex.opened.push(Opened { aim, was: self.showing(window), after: Default::default(), change: Change::default(), open_as: String::new() });
                            self.press_at(b, ex.rest);
                            ex.stage = Stage::Opened(n + 1);
                            true
                        }
                        None => false,
                    }
                }
                Stage::Opened(next) => {
                    ex.stage = Stage::Open(next);
                    let now = self.showing(window);
                    let last = ex.opened.last_mut().unwrap();
                    // What it changes on a screen with the others open tells whether it is
                    // among them: what it changed alone may be long out of date.
                    last.change = Change::between(&last.was, &now);
                    last.after = now;
                    // Its own press may have moved it, as when it shuts a menu above it.
                    let b = self.find_aim(&last.aim).unwrap_or(last.aim.b);
                    last.open_as = label_among(&self.shown_texts(), b).replace(['\t', '\n'], " ");
                    false
                }
                Stage::OpenSeen => {
                    let now = self.look(&ex.levels);
                    // One may have closed as another opened, as a menu does.
                    let open = ex.opened.iter().filter(|o| o.change.shows(&now.shown)).count();
                    if open >= 2 {
                        let name = fresh_name(&mut ex.used, format!("{}_all_open", ex.name));
                        let path = if open == ex.toggles.len() { format!("all {} that a second press closes, open at once", open) }
                            else { format!("the {} of {} that a second press closes and that stay open together", open, ex.toggles.len()) };
                        ex.rows.push(self.keep_look(&name, path, &ex.levels[0].look, &now));
                    }
                    ex.stage = Stage::Close(ex.opened.len());
                    false
                }
                Stage::Close(0) => {
                    if let Some(first) = ex.opened.first() {
                        let now = self.showing(window);
                        if now != first.was { ex.not_closed = describe_change(&first.was, &now, &Known::default()).0; }
                    }
                    ex.stage = Stage::Done;
                    false
                }
                Stage::Close(n) => {
                    ex.stage = Stage::Close(n - 1);
                    let o = &ex.opened[n - 1];
                    let now = self.showing(window);
                    // It may have another name while it is open, as a lock has another icon.
                    let b = self.find_aim(&o.aim).or_else(|| self.find_aim(&Aim { b: o.aim.b, label: o.open_as.clone() })).or_else(|| self.still_there(&o.aim).then_some(o.aim.b));
                    // One that is shut already, as a menu is by the next thing pressed, would
                    // only be pressed open again. The screen says which it is: with all
                    // that was pressed after this one closed, it is as this one's press left
                    // it. A screen that time changes too is never quite that, and then what
                    // the press brought says it.
                    let open = now == o.after || o.change.shows(&now);
                    match b.filter(|_| open) {
                        Some(b) => {
                            ex.trail.push(self.trail_press(b, &format!("to close them: {}", o.aim.say())));
                            self.press_at(b, ex.rest);
                            true
                        }
                        None => {
                            ex.trail.push(format!("{:<16}to close them: {} is left alone, being shut already or not to be found", "", o.aim.say()));
                            false
                        }
                    }
                }
                Stage::Done => {
                    self.push_move(ex.mouse.0, ex.mouse.1);
                    self.push_render();
                    self.fresh = false;
                    self.explore_report(&ex);
                    return Some(ex);
                }
            };
            if queued {
                (ex.waiting, ex.still, ex.frames) = (true, 0, 0);
                self.renders_without_frame = 0;
                self.explore = Some(ex);
                return None;
            }
        }
    }

    /// Says what a finished `explore` found, and keeps the whole of it as `<name>.txt`.
    fn explore_report(&mut self, ex: &Explore) {
        let count = |n: usize, one: &str, many: &str| format!("{} {}", n, if n == 1 { one } else { many });
        let some = |v: &[String], most: usize| format!("{}{}", v.iter().take(most).cloned().collect::<Vec<_>>().join(", "), if v.len() > most { format!(" and {} more", v.len() - most) } else { String::new() });
        let (mut all, mut faults): (Vec<String>, usize) = (Vec::new(), 0);
        for r in &ex.rows {
            let back = if r.back.is_empty() { String::new() } else { format!("; {}", r.back) };
            let line = format!("explore {}: {}{} -> {}", r.path, r.words, back, self.out_dir.join(&r.file).display());
            println!("{}", line);
            all.push(line);
            if r.brought.is_empty() { continue; }
            faults += r.brought.len();
            println!("explore {}: brings {}: {}", r.path, count(r.brought.len(), "fault", "faults"), some(&r.brought, 3));
            all.extend(r.brought.iter().map(|f| format!("    brings {}", f)));
        }
        let drawn: Vec<String> = ex.drawn.iter().map(|d| format!("{} ({} px{})", d.0, d.1, if d.2 { "" } else { ", and a second press did not put it back" })).collect();
        let lists = [
            (&ex.closes, "led back to a screen passed on the way, as a close button does"),
            (&drawn, "changed how something is drawn and moved no text"),
            (&ex.quiet, "changed nothing on screen"),
        ];
        for (list, what) in lists {
            if list.is_empty() { continue; }
            println!("explore {}: {} {}: {}", ex.name, count(list.len(), "press", "presses"), what, some(list, 6));
            all.push(format!("explore {}: {} {}: {}", ex.name, count(list.len(), "press", "presses"), what, list.join(", ")));
        }
        let mut notes: Vec<String> = Vec::new();
        if ex.restless > 0 { notes.push(format!("{} the screen was still changing 90 frames after a press: an animation that does not stop, or a caret", count(ex.restless, "time", "times"))); }
        if !ex.not_closed.is_empty() { notes.push(format!("closing all that was open at once, last first, did not bring the screen back: {}", ex.not_closed)); }
        if !ex.unpressed.is_empty() {
            notes.push(format!("{} not pressed, since the screen each was on could not be reached again or the {} presses ran out: {}", count(ex.unpressed.len(), "thing was", "things were"), ex.most, ex.unpressed.join(", ")));
        }
        for note in &notes { println!("explore {}: {}", ex.name, note); }
        all.extend(notes.iter().map(|note| format!("explore {}: {}", ex.name, note)));

        let file = format!("{}.txt", ex.name);
        let screens = ex.rows.iter().filter(|r| !r.brought.is_empty()).count();
        let line = format!("explore {}: pressed {} at {}x{}, {} deep, of {} on screen: {} opened another screen, {} led back, {} only redrew, {} changed nothing; left out {} alike in a list and {} disabled; what opened brought {} on {} -> {}",
            ex.name, count(ex.pressed, "thing", "things"), self.width, self.height, ex.depth, ex.on_screen, ex.rows.len(), ex.closes.len(), ex.drawn.len(), ex.quiet.len(),
            ex.alike, ex.disabled, count(faults, "fault", "faults"), count(screens, "screen", "screens"), self.out_dir.join(&file).display());
        println!("{}", line);
        all.push(line);
        all.push(String::new());
        all.push(format!("Every press, in order, with the script command that makes it. After each, the pointer was parked (`move {} {}`) and the screen left to come to rest (`settle`).", ex.rest.0, ex.rest.1));
        all.extend(ex.trail.iter().cloned());
        all.push(String::new());
        self.write_file(&file, all.join("\n").as_bytes());
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
        for f in found { self.lint_lines.push(lint_line(name, f.kind, &f.text, f.b, &f.detail)); }
    }

    /// Saves part of the window, `[x, y, width, height]`, as `<name>.png`, and records under
    /// that name what it shows and the style of what lies in `styled`.
    fn keep_screen(&mut self, name: &str, part: [usize; 4], styled: [i32; 4]) {
        let [x, y, w, h] = part;
        let mut rgb = Vec::with_capacity(w * h * 3);
        for row in y..y + h {
            for px in &self.pixels[row * self.side + x..row * self.side + x + w] {
                rgb.extend_from_slice(&[(px >> 16) as u8, (px >> 8) as u8, *px as u8]);
            }
        }
        self.keep_shot(name, &rgb, w, h);
        for (kind, value, uses) in self.style(styled) { self.style_lines.push(format!("{}\t{}\t{}\t{}", name, kind, value, uses)); }
        let shown = self.content(name, [x as i32, y as i32, (x + w) as i32, (y + h) as i32]);
        self.content_lines.extend(shown);
    }

    /// Somewhere to park the pointer that is on nothing a person can press.
    fn rest_spot(&self) -> (i32, i32) {
        for y in (4..self.height as i32).step_by(16) {
            for x in (4..self.width as i32).step_by(16) {
                if !self.targets.iter().any(|t| holds(t.b, [x, y])) { return (x, y); }
            }
        }
        (0, 0)
    }

    /// Whether the window shows what `prev` holds. If not, `prev` is made to hold it.
    fn same_pixels(&self, prev: &mut Vec<u32>) -> bool {
        let rows = |row: usize| (&self.pixels[row * self.side..row * self.side + self.width], row * self.width);
        let same = prev.len() == self.width * self.height
            && (0..self.height).all(|row| { let (a, at) = rows(row); a.iter().zip(&prev[at..at + self.width]).fold(0, |d, (p, q)| d | (p ^ q)) & 0xffffff == 0 });
        if !same { *prev = self.window_pixels(); }
        same
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

    /// A target's place in the drawing order: that of the last thing drawn inside it.
    fn drawn_at(&self, b: [i32; 4]) -> Option<usize> {
        self.rects.iter().filter(|r| inside(r.b, b)).map(|r| r.at)
            .chain(self.texts.iter().filter(|x| inside(cut([x.x, x.y, x.x + x.w as i32, x.y + x.h as i32], self.clip_at(x.at)), b)).map(|x| x.at)).max()
    }

    /// The click targets a person can reach: on screen, and not under something drawn later.
    fn shown_targets(&self) -> Vec<LoggedTarget> {
        let window = [0, 0, self.width as i32, self.height as i32];
        let mut out: Vec<LoggedTarget> = Vec::new();
        for t in &self.targets {
            let vis = cut(t.b, window);
            if area(vis) == 0 || !t.grip.is_empty() || out.iter().any(|o| o.b == t.b) { continue; }
            let Some(own) = self.drawn_at(t.b) else { continue };
            let over = |r: &LoggedRect, from: usize| r.at > from && r.color >> 24 >= 0x30 && !inside(r.b, t.b) && area(cut(r.b, vis)) * 2 > area(vis);
            if self.rects.iter().any(|r| over(r, own)) { continue; }
            // A dialog's text may lie inside the box of a button behind the dialog, and is not
            // the button's own. A target with a rectangle of its own size is under whatever
            // much larger thing is drawn after that rectangle and covers it whole.
            let face = self.rects.iter().filter(|r| inside(r.b, t.b) && inside(t.b, r.b)).map(|r| r.at).min();
            if face.is_some_and(|at| self.rects.iter().any(|r| over(r, at) && inside(vis, r.b) && area(r.b) >= 4 * area(t.b))) { continue; }
            out.push(t.clone());
        }
        out
    }

    /// The text that names a box in a report.
    fn label_of(&self, b: [i32; 4]) -> String { label_among(&self.shown_texts(), b) }

    /// What a part of the screen shows: its texts, then what can be pressed, each by its name.
    fn showing(&self, part: [i32; 4]) -> (Vec<Shown>, Vec<Shown>) {
        let flat = |s: &str| s.replace(['\t', '\n'], " ");
        let texts = self.texts_under(0xf0).iter().filter(|s| area(cut(s.vis, part)) > 0)
            .map(|s| Shown { x: s.t.x, y: s.t.y, w: s.t.w as i32, h: s.t.h as i32, color: format!("{:06x}", s.t.color & 0xffffff), text: flat(&s.t.text) }).collect();
        let shown = self.shown_texts();
        let press = self.shown_targets().iter().filter(|t| area(cut(t.b, part)) > 0)
            .map(|t| Shown { x: t.b[0], y: t.b[1], w: t.b[2] - t.b[0], h: t.b[3] - t.b[1], color: "-".to_string(), text: flat(&label_among(&shown, t.b)) }).collect();
        (texts, press)
    }

    /// What a part of the screen shows, a line each, as `content.txt` keeps it.
    fn content(&self, name: &str, part: [i32; 4]) -> Vec<String> {
        let (texts, press) = self.showing(part);
        texts.iter().map(|s| ("T", s)).chain(press.iter().map(|s| ("P", s)))
            .map(|(kind, s)| format!("{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}", name, kind, s.x, s.y, s.w, s.h, s.color, s.text)).collect()
    }

    /// The screen's lint findings, each with the key that makes it the same fault elsewhere.
    fn faults(&self) -> Vec<Fault> {
        self.lint().into_iter().map(|f| Fault { key: format!("{}\t{}\t{}", f.kind, f.text, no_numbers(&f.detail)), kind: f.kind, text: f.text, detail: f.detail, b: f.b }).collect()
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
            // What a scrolling list cuts off can be scrolled back. What the window itself cuts
            // off, as of a dialog taller than the window, cannot be reached at all.
            let clip = self.clip_at(self.drawn_at(t.b).unwrap_or(0));
            let past = [(t.b[3] - high, clip[3] >= high, "bottom"), (t.b[2] - wide, clip[2] >= wide, "right"), (-t.b[1], clip[1] <= 0, "top"), (-t.b[0], clip[0] <= 0, "left")];
            if let Some((by, _, side)) = past.into_iter().filter(|p| p.0 > 1 && p.1).max() {
                out.push(Finding { kind: "outside", b, text: label.clone(), detail: format!("{} px past the window's {}, where nothing scrolls it back", by, side) });
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

/// A finding as `lint.txt` keeps it.
fn lint_line(name: &str, kind: &str, text: &str, b: [i32; 4], detail: &str) -> String {
    format!("{}\t{}\t{}\t{},{}\t{}", name, kind, text.replace('\t', " "), b[0], b[1], detail)
}

fn lint_colour(kind: &str) -> [u8; 3] {
    match kind { "clipped" => [255, 40, 40], "outside" => [255, 255, 255], "overlap" => [255, 0, 255], "overflow" => [255, 140, 0], "margin" => [255, 230, 0], "contrast" => [0, 220, 255], "target" => [90, 255, 90], "edge" => [110, 150, 255], _ => [150, 150, 150] }
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
/// Whether box `b` lies inside box `t`, give or take two pixels.
fn inside(b: [i32; 4], t: [i32; 4]) -> bool { area(b) > 0 && b[0] >= t[0] - 2 && b[1] >= t[1] - 2 && b[2] <= t[2] + 2 && b[3] <= t[3] + 2 }

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

/// A text as part of a file's name: its letters and digits in lower case, joined by `_`.
fn slug(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if out.len() >= 24 { break; }
        if c.is_ascii_alphanumeric() { out.push(c.to_ascii_lowercase()); } else if !out.is_empty() && !out.ends_with('_') { out.push('_'); }
    }
    let out = out.trim_end_matches('_');
    if out.is_empty() { "unnamed".to_string() } else { out.to_string() }
}

/// `want`, or `want` with the first number after it that makes a name not used yet.
fn fresh_name(used: &mut Vec<String>, want: String) -> String {
    let name = (1..).map(|n| if n == 1 { want.clone() } else { format!("{}_{}", want, n) }).find(|n| !used.contains(n)).unwrap();
    used.push(name.clone());
    name
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
    if kind == "clipped" || kind == "overflow" || kind == "outside" { n } else { -n }
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
            // Something to press near the window's bottom right corner, anywhere from clear of
            // both sides to wholly past them. What it is drawn under is the window, or a list
            // that stops short of the window's bottom, of its right side, or of both.
            let (gap_right, gap_bottom, target_w, target_h, hidden) = (pick(50) - 30, pick(50) - 30, 10 + pick(60), 10 + pick(60), pick(4) == 0);
            let target = [512 - gap_right - target_w, 512 - gap_bottom - target_h, 512 - gap_right, 512 - gap_bottom];
            let (list_right, list_bottom) = (pick(3) == 0, pick(3) == 0);
            let under = [0, 0, if list_right { 508 } else { 512 }, if list_bottom { 508 } else { 512 }];
            w.clips.push((35, under));
            w.targets = vec![LoggedTarget { b: target, enabled: true, pointer: false, grip: String::new() }];
            w.rects.push(LoggedRect { b: cut(target, under), color: 0xff000000 | bg, at: 40, radius: 0 });
            if hidden { w.rects.push(LoggedRect { b: [250, 250, 512, 512], color: 0xff000000 | bg, at: 50, radius: 0 }); }

            let mut want: Vec<&str> = Vec::new();
            // One with nothing of it drawn has no place in the drawing order, and is left out.
            if !hidden && area(cut(target, under)) > 0 {
                if target_w.min(target_h) < 24 { want.push("target"); }
                if (1..=12).contains(&gap_right) { want.push("edge"); }
                if (gap_bottom < -1 && !list_bottom) || (gap_right < -1 && !list_right) { want.push("outside"); }
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

    #[derive(Clone, Copy, PartialEq, Debug)]
    enum Kind { Nothing, Redraw, Toggle, Menu, Grow, Field, Modal, Toast, Sticky, Count, Off, Close, Keep }

    #[derive(Clone, Debug)]
    struct Thing { kind: Kind, label: String, w: i32, on: bool, left: usize, lasts: usize, kids: Vec<Thing> }

    /// A made-up screen for `fuzz_explore`: a column of things to press, each of one kind, a
    /// row of tabs among them, the dialog one of them may have open, and at the top of the
    /// page a count of presses that nothing takes back, wherever the press was made. A `deaf`
    /// one does not close its dialog on esc. A menu closes when anything else is pressed, and
    /// a field keeps the caret until then. What grows opens within its own box. A switch may sit beside one of the things, in its
    /// row. `best` is the model as it was when the most was last open at once.
    #[derive(Clone)]
    struct Model {
        things: Vec<Thing>, tabs: usize, tab_row: usize, tab: usize, modal: Option<usize>, keeps: bool, deaf: bool, count: usize,
        focus: Option<Vec<usize>>, lock_row: Option<usize>, lock_on: bool, mouse: (i32, i32), bits: [u64; 8], most_open: usize, best: Vec<Model>,
        /// With `slow` set, "busy" shows once two things have been open at once for that many
        /// sixtieths of a second, as a page does that moves on by itself in some state.
        slow: usize, together: usize,
    }

    /// What leads the place of a tab, and what is the place of the switch, among what a frame
    /// of the model lets be pressed.
    const TABS: usize = usize::MAX;
    const LOCK: usize = usize::MAX - 1;
    const MODEL: [i32; 4] = [0, 0, 400, 300];

    /// One thing a frame of the model draws: a text, or a rectangle.
    struct Piece { text: Option<String>, b: [i32; 4], color: u32 }

    impl Model {
        /// A frame: its rectangles and texts in the order drawn, and what can be pressed, each
        /// with its place in the model, its box, and whether the pointer can reach it.
        fn frame(&self) -> (Vec<Piece>, Vec<(Vec<usize>, [i32; 4], bool)>) {
            let (mut pieces, mut press) = (Vec::new(), Vec::new());
            let text = |s: String, x: i32, y: i32, w: i32| Piece { text: Some(s), b: [x, y, x + w, y + 14], color: 0xffe0e0e0 };
            // A name is at the top of its box, so that a box the window cuts off still shows it.
            let face = |t: &Thing, b: [i32; 4], place: &[usize]| [
                Piece { text: None, b, color: if self.focus.as_deref() == Some(place) { 0xff203080 } else if t.kind == Kind::Redraw && t.on { 0xff805020 } else { 0xff404040 } },
                text(t.label.clone(), b[0] + 6, b[1], 40),
            ];
            let free = self.modal.is_none();
            pieces.push(text(format!("count {}", self.count), 150, 4, 100));
            if self.slow > 0 && self.together >= self.slow { pieces.push(text("busy".to_string(), 255, 4, 40)); }
            let mut y = 20;
            for (i, t) in self.things.iter().enumerate() {
                if i == self.tab_row && self.tabs > 0 {
                    for k in 0..self.tabs {
                        let b = [20 + 70 * k as i32, y, 80 + 70 * k as i32, y + 24];
                        pieces.push(Piece { text: None, b, color: if k == self.tab { 0xff606060 } else { 0xff404040 } });
                        pieces.push(text(format!("tab{}", k), b[0] + 6, y, 40));
                        press.push((vec![TABS, k], b, free));
                    }
                    // What a tab shows is beside the tabs, so it is in the window whenever they are.
                    pieces.push(text(format!("tab {} body", self.tab), 30 + 70 * self.tabs as i32, y, 100));
                    y += 30;
                }
                let tall = t.kind == Kind::Grow && t.on;
                let b = [20, y, 20 + t.w, y + if tall { 52 } else { 24 }];
                pieces.extend(face(t, b, &[i]));
                if tall { pieces.push(text(format!("body of {}", t.label), 40, y + 30, 100)); }
                if self.lock_row == Some(i) {
                    let lock = [300, y, 360, y + 24];
                    pieces.push(Piece { text: None, b: lock, color: if self.lock_on { 0xff805020 } else { 0xff404040 } });
                    pieces.push(text("lock".to_string(), lock[0] + 6, y, 40));
                    press.push((vec![LOCK], lock, free));
                }
                if t.on && t.kind == Kind::Sticky { pieces.push(text(format!("kept {}", t.label), 30 + t.w, y, 60)); }
                if t.left > 0 { pieces.push(text(format!("toast {}", t.label), 300, 4, 80)); }
                press.push((vec![i], b, free));
                y += if tall { 58 } else { 30 };
                if matches!(t.kind, Kind::Toggle | Kind::Menu) && t.on {
                    pieces.push(text(format!("body of {}", t.label), 40, y, 100));
                    y += 22;
                    for (j, c) in t.kids.iter().enumerate() {
                        pieces.extend(face(c, [40, y, 40 + c.w, y + 24], &[i, j]));
                        press.push((vec![i, j], [40, y, 40 + c.w, y + 24], free));
                        y += 30;
                        if c.kind == Kind::Toggle && c.on {
                            pieces.push(text(format!("body of {}", c.label), 60, y, 100));
                            y += 22;
                        }
                    }
                }
            }
            if let Some(m) = self.modal {
                // The page behind a dialog is dimmed, and still logs what it would let be pressed.
                pieces.push(Piece { text: None, b: MODEL, color: 0x80000000 });
                pieces.push(Piece { text: None, b: [100, 40, 390, 260], color: 0xff303030 });
                pieces.push(text(format!("dialog {}", self.things[m].label), 110, 50, 100));
                for (j, c) in self.things[m].kids.iter().enumerate() {
                    let b = [110, 80 + 34 * j as i32, 110 + c.w, 104 + 34 * j as i32];
                    pieces.extend(face(c, b, &[m, j]));
                    if c.on && c.kind == Kind::Keep { pieces.push(text(format!("kept {}", c.label), 120 + c.w, b[1] + 5, 60)); }
                    press.push((vec![m, j], b, true));
                }
            }
            (pieces, press)
        }

        fn at(&self, place: &[usize]) -> Option<&Thing> {
            if place[0] == TABS || place[0] == LOCK { return None; }
            Some(if place.len() == 1 { &self.things[place[0]] } else { &self.things[place[0]].kids[place[1]] })
        }

        /// Draws a frame into the window and hands it over as the frame loop does.
        fn draw(&self, w: &mut ScriptWindow) {
            let (pieces, press) = self.frame();
            let mut commands = vec![crate::DrawCommand::ClearScreenToColor { color: 0xff101010 }];
            let mut texts = Vec::new();
            for y in 0..w.height { w.pixels[y * w.side..y * w.side + w.width].fill(0xff101010); }
            for p in &pieces {
                let v = cut(p.b, MODEL);
                let ink = match &p.text { Some(s) => 0xff808080 | fnv1a(s.as_bytes()) as u32, None => p.color };
                for y in v[1]..v[3] {
                    for px in &mut w.pixels[y as usize * w.side + v[0] as usize..y as usize * w.side + v[2] as usize] {
                        *px = if ink >> 24 == 0xff { ink } else { 0xff000000 | (*px >> 1) & 0x7f7f7f };
                    }
                }
                // A text is a command of its own in the frame loop, so each has its own place in
                // the drawing order here too: a rectangle of no size stands in for it.
                let b = if p.text.is_some() { [0; 4] } else { p.b };
                if let Some(s) = &p.text {
                    texts.push(LoggedText { x: p.b[0], y: p.b[1], w: (p.b[2] - p.b[0]) as u32, h: (p.b[3] - p.b[1]) as u32, color: p.color, at: commands.len(), text: s.clone() });
                }
                commands.push(crate::DrawCommand::RoundedRectangle { x: b[0] as f32, x2: b[2] as f32, y: b[1] as f32, y2: b[3] as f32,
                    radius_tl: 0, radius_tr: 0, radius_bl: 0, radius_br: 0, color: p.color });
            }
            let targets: Vec<LoggedTarget> = press.iter().map(|(place, b, _)| LoggedTarget { b: *b, enabled: self.at(place).is_none_or(|t| t.kind != Kind::Off), pointer: false, grip: String::new() }).collect();
            w.frame_done(&mut texts, &commands, &targets, FrameReport { single_us: 0, total_us: 0, draw_commands: commands.len(), cursor: CursorIcon::Default });
        }

        fn close(&mut self) {
            let Some(m) = self.modal.take() else { return };
            self.focus = None;
            if !self.keeps { for c in &mut self.things[m].kids { c.on = false; } }
        }

        /// Takes one event of the window's, as the frame loop does.
        fn feed(&mut self, w: &mut ScriptWindow, e: Event) {
            let down = |bits: &[u64; 8], code: u32| bits[(code as usize >> 6) & 7] >> (code & 63) & 1 == 1;
            match e.kind {
                EVENT_AXES => for d in &e.axes[..e.axis_count as usize] {
                    if d.axis == AXIS_MOUSE_X { self.mouse.0 = d.delta >> 8; }
                    if d.axis == AXIS_MOUSE_Y { self.mouse.1 = d.delta >> 8; }
                },
                EVENT_BUTTONS => {
                    let (press, esc) = (down(&e.bits, BTN_LEFT) && !down(&self.bits, BTN_LEFT), down(&e.bits, 1) && !down(&self.bits, 1));
                    self.bits = e.bits;
                    if esc && !self.deaf { self.close(); }
                    let (_, on_screen) = self.frame();
                    let Some((place, ..)) = on_screen.iter().rev().find(|(_, b, reach)| press && *reach && holds(*b, [self.mouse.0, self.mouse.1])) else { return };
                    self.focus = None;
                    for (i, t) in self.things.iter_mut().enumerate() { if t.kind == Kind::Menu && place[..] != [i] { t.on = false; } }
                    if place[0] == LOCK { self.lock_on = !self.lock_on; return; }
                    if place[0] == TABS { self.tab = place[1]; return; }
                    let t = if place.len() == 1 { &mut self.things[place[0]] } else { &mut self.things[place[0]].kids[place[1]] };
                    match t.kind {
                        Kind::Redraw | Kind::Toggle | Kind::Menu | Kind::Grow => t.on = !t.on,
                        Kind::Field => self.focus = Some(place.clone()),
                        Kind::Sticky | Kind::Keep => t.on = true,
                        Kind::Toast => t.left = t.lasts,
                        Kind::Modal => self.modal = Some(place[0]),
                        Kind::Close => self.close(),
                        Kind::Count => self.count += 1,
                        Kind::Nothing | Kind::Off => {}
                    }
                    let open = self.things.iter().filter(|t| matches!(t.kind, Kind::Toggle | Kind::Menu | Kind::Grow) && t.on).count();
                    if open >= self.most_open {
                        self.most_open = open;
                        let mut best = self.clone();
                        best.best.clear();
                        self.best = vec![best];
                    }
                }
                EVENT_RENDER => {
                    let sixtieths = ((e.dt_fs * 60 + 500_000_000_000_000) / 1_000_000_000_000_000).max(1) as usize;
                    for t in &mut self.things { t.left = t.left.saturating_sub(sixtieths); }
                    let open = self.things.iter().filter(|t| matches!(t.kind, Kind::Toggle | Kind::Menu | Kind::Grow) && t.on).count();
                    self.together = if open >= 2 { self.together + sixtieths } else { 0 };
                    self.draw(w);
                }
                _ => {}
            }
        }
    }

    #[test]
    fn fuzz_explore() {
        let seed = seed();
        println!("fuzz_explore FUZZ_SEED={}", seed);
        let mut s = seed;
        let out = std::env::temp_dir().join(format!("zebra-gui-fuzz-explore-{}", seed));
        for round in 0..120 {
            let mut pick = |n: u64| (rng_next(&mut s) % n.max(1)) as usize;
            let thing = |kind: Kind, label: String, w: i32, lasts: usize| Thing { kind, label, w, on: false, left: 0, lasts, kids: Vec::new() };
            // A column of things of every kind, each of its own width so none is like another.
            const TOP: [Kind; 14] = [Kind::Nothing, Kind::Redraw, Kind::Toggle, Kind::Toggle, Kind::Menu, Kind::Field, Kind::Modal, Kind::Modal, Kind::Toast, Kind::Sticky, Kind::Count, Kind::Off, Kind::Menu, Kind::Grow];
            let mut things: Vec<Thing> = Vec::new();
            for i in 0..2 + pick(6) {
                let mut t = thing(TOP[pick(TOP.len() as u64)], format!("thing{}", i), 100 + 7 * i as i32, 30 + pick(120));
                let inside: &[Kind] = match t.kind { Kind::Toggle => &[Kind::Nothing, Kind::Redraw, Kind::Toggle, Kind::Count, Kind::Field], Kind::Modal => &[Kind::Nothing, Kind::Redraw, Kind::Keep, Kind::Close, Kind::Count, Kind::Field], _ => &[] };
                if !inside.is_empty() { for j in 0..pick(4) { t.kids.push(thing(inside[pick(inside.len() as u64)], format!("thing{}kid{}", i, j), 80 + 5 * j as i32, 0)); } }
                things.push(t);
            }
            // A dialog that esc does not close needs its own way out.
            let deaf = pick(4) == 0;
            for t in things.iter_mut().filter(|t| deaf && t.kind == Kind::Modal && t.kids.iter().all(|c| c.kind != Kind::Close)) {
                let kid = thing(Kind::Close, format!("{}kid{}", t.label, t.kids.len()), 80 + 5 * t.kids.len() as i32, 0);
                t.kids.push(kid);
            }
            // Sometimes a list of rows of one width, of which only the first and last get pressed.
            if pick(3) == 0 { for i in 0..4 + pick(3) { things.push(thing([Kind::Nothing, Kind::Redraw][pick(2)], format!("row{}", i), 90, 0)); } }
            let tabs = [0, 2, 3][pick(3)];
            let lock_row = if pick(2) == 0 { Some(pick(things.len() as u64)) } else { None };
            let slow = if pick(3) == 0 { 2 + pick(40) } else { 0 };
            let mut model = Model { tab_row: pick(things.len() as u64), things, tabs, tab: pick(tabs as u64), modal: None, keeps: pick(2) == 0, deaf, count: 0,
                focus: None, lock_row, lock_on: false, mouse: (0, 0), bits: [0; 8], most_open: 0, best: Vec::new(), slow, together: 0 };
            let home = model.clone();
            // The way out of such a dialog is found by pressing what is in it.
            let depth = 1 + (deaf || pick(4) > 0) as usize;

            let mut w = ScriptWindow::open("", &out.to_string_lossy()).unwrap();
            w.resize(MODEL[2] as usize, MODEL[3] as usize);
            let args = vec!["made_up".to_string(), depth.to_string()];
            let mut turns = 0;
            let ex = loop {
                turns += 1;
                assert!(turns < 100_000, "round {}: explore does not end", round);
                if let Some(ex) = w.explore_step(&args) { break ex; }
                while let Some(e) = w.events.pop_front() { model.feed(&mut w, e); }
            };

            // What a state of the model lets be pressed: what is in reach, and in the window at
            // least in part.
            let named = |m: &Model| -> Vec<(Vec<usize>, [i32; 4])> { m.frame().1.into_iter().filter(|(_, b, reach)| *reach && b[1] < MODEL[3]).map(|p| (p.0, p.1)).collect() };
            let quoted_name = |m: &Model, place: &[usize]| match m.at(place) {
                Some(t) => format!("{:?}", t.label),
                None if place[0] == LOCK => "\"lock\"".to_string(),
                None => format!("{:?}", format!("tab{}", place[1])),
            };
            // The names of what a state leaves past the window's bottom, as lint names them.
            let outside = |m: &Model| -> Vec<String> {
                let mut v: Vec<String> = named(m).iter().filter(|(_, b)| b[3] - MODEL[3] > 1).map(|(place, _)| quoted_name(m, place)).collect();
                v.sort();
                v
            };
            // Whether what a thing at that place opens under itself is in the window.
            let seen_open = |b: [i32; 4]| b[1] + 30 < MODEL[3];
            let row = |path: &str| ex.rows.iter().find(|r| r.path == path);
            let untouched = |path: &str| row(path).is_none() && !ex.quiet.iter().any(|q| q == path) && !ex.drawn.iter().any(|d| d.0 == path) && !ex.closes.iter().any(|c| c == path);
            let told = |path: &str, kind: Kind, back: &str| match kind {
                Kind::Nothing => assert!(ex.quiet.iter().any(|q| q == path), "round {}: {} should change nothing", round, path),
                Kind::Redraw => assert!(ex.drawn.iter().any(|d| d.0 == path && d.2), "round {}: {} should only redraw, and a second press put it back", round, path),
                Kind::Field => assert!(ex.drawn.iter().any(|d| d.0 == path && !d.2), "round {}: {} should only redraw, and a second press not put it back", round, path),
                Kind::Close => assert!(ex.closes.iter().any(|c| c == path), "round {}: {} should lead back", round, path),
                Kind::Off => assert!(untouched(path), "round {}: {} is disabled", round, path),
                _ => {
                    let r = row(path).unwrap_or_else(|| panic!("round {}: {} should lead to another screen", round, path));
                    assert!(r.back.starts_with(back), "round {}: {} is undone otherwise than by {:?}: {:?}", round, path, back, r.back);
                }
            };

            // Of the rows of one width, the first and the last on screen stand for the rest.
            let first = named(&home);
            let alike: Vec<&(Vec<usize>, [i32; 4])> = first.iter().filter(|(place, b)| place[0] != TABS && b[2] - b[0] == 90).collect();
            let mut toggles = 0;
            for (place, b) in &first {
                let path = quoted_name(&home, place);
                // Nothing is undone by pressing the switch beside it: that would leave it on.
                if place[0] == LOCK { told(&path, Kind::Redraw, ""); continue; }
                if place[0] == TABS {
                    if place[1] == home.tab { told(&path, Kind::Nothing, ""); } else {
                        let r = row(&path).unwrap_or_else(|| panic!("round {}: {} should lead to another screen", round, path));
                        assert!(r.back.starts_with("pressing \"tab") && r.back.ends_with("beside it undoes it"), "round {}: {} {:?}", round, path, r.back);
                    }
                    continue;
                }
                if alike.len() >= 4 && b[2] - b[0] == 90 && *b != alike[0].1 && *b != alike[alike.len() - 1].1 {
                    assert!(untouched(&path), "round {}: {} is one of a list and not its first or last", round, path);
                    continue;
                }
                let t = &home.things[place[0]];
                // What opens under the last thing in the window opens out of sight.
                let opens = matches!(t.kind, Kind::Toggle | Kind::Menu | Kind::Grow);
                // What grows is seen to, by its box, even when what it shows is out of sight.
                if opens && t.kind != Kind::Grow && !seen_open(*b) { told(&path, Kind::Nothing, ""); continue; }
                let back = match t.kind {
                    Kind::Toggle | Kind::Menu | Kind::Grow => "pressing it again undoes it",
                    Kind::Modal if deaf => "esc does not undo it once things in it were pressed; pressing \"thing",
                    Kind::Modal => "esc undoes it",
                    Kind::Toast => "it goes away by itself",
                    _ => "nothing undoes it",
                };
                told(&path, t.kind, back);
                if !opens && t.kind != Kind::Modal { continue; }

                let mut opened = home.clone();
                opened.things[place[0]].on = opens;
                opened.modal = (t.kind == Kind::Modal).then_some(place[0]);
                if opens {
                    toggles += 1;
                    assert!(ex.toggles.iter().any(|a| format!("{:?}", a.0.label) == path), "round {}: {} opens in place", round, path);
                    // What opening it pushes past the window's bottom is a fault it brings.
                    let was = outside(&home);
                    let want: Vec<String> = outside(&opened).into_iter().filter(|o| !was.contains(o)).collect();
                    let mut got: Vec<String> = row(&path).unwrap().brought.iter().filter_map(|f| f.strip_prefix("outside ")).map(|f| f[..f[1..].find('"').unwrap() + 2].to_string()).collect();
                    got.sort();
                    assert_eq!(got, want, "round {}: what opening {} pushes past the window's bottom", round, path);
                }
                for (inner, at) in named(&opened).iter().filter(|(p, _)| p.len() == 2 && p[0] == place[0]) {
                    let kid = &t.kids[inner[1]];
                    let inner_path = format!("{} > {:?}", path, kid.label);
                    if depth == 1 { assert!(untouched(&inner_path), "round {}: {} is a level too deep", round, inner_path); continue; }
                    let kind = if kid.kind == Kind::Toggle && !seen_open(*at) { Kind::Nothing } else { kid.kind };
                    // A dialog takes what was kept in it out of sight when esc closes it. The
                    // count is on the page, where nothing takes it back.
                    told(&inner_path, kind, match (kind, t.kind) {
                        (Kind::Keep | Kind::Count, Kind::Modal) if deaf => "nothing undoes it: ",
                        (Kind::Keep, _) => "esc undoes it, and what it was in with it",
                        (Kind::Count, Kind::Modal) => "nothing undoes it; esc closes what it was in, and the change stays",
                        (Kind::Count, _) => "nothing undoes it: ",
                        _ => "pressing it again undoes it",
                    });
                }
            }
            assert_eq!(ex.toggles.len(), toggles, "round {}", round);
            // A press that nothing undoes loses none of the presses after it.
            assert!(ex.unpressed.is_empty(), "round {}: {:?}", round, ex.unpressed);
            assert_eq!(ex.disabled, first.iter().filter(|(place, _)| home.at(place).is_some_and(|t| t.kind == Kind::Off)).count(), "round {}", round);
            assert_eq!(ex.alike, if alike.len() >= 4 { alike.len() - 2 } else { 0 }, "round {}", round);
            // With all of them pressed open, a menu is shut by the next one pressed, and what
            // opens past the window's bottom is not seen to be open.
            let together = ex.rows.iter().map(|r| r.path.as_str()).find(|p| p.starts_with("all ") || p.starts_with("the "));
            if home.slow > 0 {
                // What "busy" does to the count of what is seen open together is not worked out.
            } else if toggles >= 2 {
                let open = model.best[0].frame().0.iter().filter(|p| p.b[1] < MODEL[3] && p.text.as_ref().is_some_and(|t| t.starts_with("body of thing") && !t.contains("kid"))).count();
                let want = if open < 2 { None } else if open == toggles { Some(format!("all {} that ", open)) } else { Some(format!("the {} of {} that ", open, toggles)) };
                assert_eq!(together.map(|p| p[..want.as_ref().map_or(0, |w| w.len()).min(p.len())].to_string()), want, "round {}: {:?}", round, together);
            } else {
                assert!(together.is_none(), "round {}: {:?}", round, together);
            }
            assert!(ex.not_closed.is_empty(), "round {}: {:?}", round, ex.not_closed);

            // Every press is undone but one that nothing undoes, so the window ends as it began.
            while let Some(e) = w.events.pop_front() { model.feed(&mut w, e); }
            // The caret stays in the last field pressed, as it does for a hand.
            model.focus = None;
            let in_window = |m: &Model| -> Vec<(Option<String>, [i32; 4], u32)> {
                m.frame().0.into_iter().filter(|p| p.b[1] < MODEL[3] && !p.text.as_ref().is_some_and(|t| t.starts_with("kept ") || t.starts_with("count "))).map(|p| (p.text, p.b, p.color)).collect()
            };
            assert!(in_window(&model) == in_window(&home), "round {}: the window is not left as it was found", round);
        }
        let _ = std::fs::remove_dir_all(&out);
    }
}
