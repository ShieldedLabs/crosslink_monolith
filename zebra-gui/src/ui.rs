use std::{hash::Hash};
use winit::{event::MouseButton, keyboard::KeyCode};

use super::*;

// make widgets nicer to construct
macro_rules! widget {
    // Base case: only id and flags, no fields
    ($id:expr, $flags:expr) => {{
        Widget {
            id: $id,
            flags: $flags,
            ..Default::default()
        }
    }};

    // Case with fields: id, flags, then key-value pairs
    ($id:expr, $flags:expr, $($field:ident: $value:expr),* $(,)?) => {{
        Widget {
            id: $id,
            flags: $flags,
            $($field: $value,)*
            ..Default::default()
        }
    }};
}

// poor man's bitset
macro_rules! bitset {
    // bitset with no type, defaults to u64
    ($name:ident, $($flag:ident = $value:expr),* $(,)?) => {
        bitset!($name<u64>, $($flag = $value),*);
    };

    // bitset with provided type
    ($name:ident<$type:ty>, $($flag:ident = $value:expr),* $(,)?) => {
        #[derive(Debug, Default, Copy, Clone, PartialEq, Eq)]
        pub struct $name($type);

        #[allow(dead_code)]
        impl $name {
            $(pub const $flag: Self = Self($value);)*

            pub const fn has(&self, flag: Self) -> bool {
                self.0 & flag.0 != 0
            }
        }

        impl Into<$type> for $name {
            fn into(self) -> $type {
                self.0
            }
        }

        impl From<$type> for $name {
            fn from(value: $type) -> Self {
                Self(value)
            }
        }

        impl std::ops::BitOr for $name {
            type Output = Self;
            fn bitor(self, rhs: Self) -> Self::Output {
                Self(self.0 | rhs.0)
            }
        }
        impl std::ops::BitOrAssign for $name {
            fn bitor_assign(&mut self, rhs: Self) {
                self.0 |= rhs.0;
            }
        }

        impl std::ops::BitAnd for $name {
            type Output = Self;
            fn bitand(self, rhs: Self) -> Self::Output {
                Self(self.0 & rhs.0)
            }
        }
        impl std::ops::BitAndAssign for $name {
            fn bitand_assign(&mut self, rhs: Self) {
                self.0 &= rhs.0;
            }
        }

        impl std::ops::Not for $name {
            type Output = Self;
            fn not(self) -> Self::Output {
                Self(!self.0)
            }
        }
    };
}

pub fn magic<'a, 'b, T>(mut_ref: &'a mut T) -> &'b mut T {
    let mut_ref = mut_ref as *mut T;
    return unsafe { &mut *mut_ref };
}


#[derive(Debug, Default)]
pub struct SomeDataToKeepAround {
    pub messages:          Vec<String>,
    pub can_send_messages: bool,
}

fn run_ui(ui: &mut Context, data: &mut SomeDataToKeepAround, is_rendering: bool) -> bool {
    if ui.input().key_pressed(KeyCode::Tab) {
        ui.debug = !ui.debug;
    }
    if ui.input().key_pressed(KeyCode::F5) {
        unsafe {
            if *ui.draw().debug_pixel_inspector == None {
                ui.pixel_inspector_primed = true;
            } else {
                *ui.draw().debug_pixel_inspector = None;
                ui.pixel_inspector_primed = false;
            }
        }
    }

    if ui.pixel_inspector_primed {
        if ui.input().mouse_pressed(MouseButton::Left) {
            unsafe {
                *ui.draw().debug_pixel_inspector = Some((ui.input().mouse_pos().0.clamp(0, ui.draw().window_width) as usize, ui.input().mouse_pos().1.clamp(0, ui.draw().window_height) as usize));
            }
            ui.pixel_inspector_primed = false;
        }
    }

    if is_rendering {
        if ui.pixel_inspector_primed {
            ui.draw().mono_text_line(0.0, 0.0, 16.0, "Pixel Inspector is Primed! Click to select pixel.", 0xff_00ff00);
        }
        if let Some((x, y)) = unsafe { *ui.draw().debug_pixel_inspector } {
            let x = x as isize; let y = y as isize;
            let mut draw_x = 0;
            let mut draw_y = 0;
            if x < ui.draw().window_width/2 { draw_x = ui.draw().window_width - 256 };
            if y < ui.draw().window_height/2 { draw_y = ui.draw().window_height - 256 };
            let color = unsafe { *ui.draw().debug_pixel_inspector_last_color };
            ui.draw().rectangle(draw_x as f32, draw_y as f32, draw_x as f32 + 256.0, draw_y as f32 + 256.0, 0xff_000000 | color);
            ui.draw().mono_text_line(draw_x as f32, draw_y as f32, 12.0, &format!("({},{}) = {:X}", x, y, color), 0xff_000000 | (color ^ u32::MAX));
        }
    }

    return false;
}

pub fn demo_of_rendering_stuff_with_context_that_allocates_in_the_background(ui: &mut Context, data: &mut SomeDataToKeepAround) -> bool {
    let dummy_input = InputCtx {
        this_mouse_pos: ui.input().this_mouse_pos,
        last_mouse_pos: ui.input().last_mouse_pos,

        mouse_down: ui.input().mouse_down,
        keys_down1: ui.input().keys_down1,
        keys_down2: ui.input().keys_down2,

        ..Default::default()
    };
    let real_input = ui.input;
    let result =           run_ui(ui, data, false); ui.input = &dummy_input;
    let result = result || run_ui(ui, data, true);  ui.input =   real_input;
    return result;
}

#[derive(Debug, Default, Clone)]
pub struct Context {
    pub input: *const InputCtx,
    pub draw:  *const DrawCtx,
    pub debug: bool,
    pub pixel_inspector_primed: bool,
    pub non_ui_drawable_area: Rect,

    pub delta: f64,
    pub default_style: Style,
    pub draw_commands: Vec<DrawCommand>,

    pub dpi_scale: f32,
}

impl Context {
    pub fn new(style: Style) -> Context {
        Context { ..Default::default() }
    }
    fn draw(&self)  -> &DrawCtx  { unsafe { &*self.draw  } }
    fn input(&self) -> &InputCtx { unsafe { &*self.input } }
}

bitset!(Flags<u64>,
    NONE = 0 << 0,

    HIDDEN     = 1 << 1,
    DISABLED   = 1 << 2,

    // HOVERABLE   = 1 <<  9,
    CLICKABLE   = 1 << 10,
    CHECKABLE   = 1 << 11,
    TYPEABLE    = 1 << 12,
    RESIZABLE_X = 1 << 13,
    RESIZABLE_Y = 1 << 14,

    DRAW_MONO_TEXT  = 1 << 17,
    DRAW_SERIF_TEXT = 1 << 18,
    DRAW_BACKGROUND = 1 << 19,
    DRAW_BORDER     = 1 << 20,
    DRAW_PIP        = 1 << 21,

    CLIP_CHILDREN = 1 << 25,
    KEEP_FOCUS    = 1 << 26,


    // Widget Defaults
    //////////////////

    DEFAULT_LABEL_FLAGS = Flags::DRAW_MONO_TEXT.0,

    DEFAULT_BUTTON_FLAGS = Flags::CLICKABLE.0
                         | Flags::DRAW_MONO_TEXT.0
                         | Flags::DRAW_BACKGROUND.0
                         | Flags::DRAW_BORDER.0,

    DEFAULT_CHECKBOX_FLAGS = Flags::CHECKABLE.0
                           | Flags::CLICKABLE.0
                           | Flags::DRAW_MONO_TEXT.0
                           | Flags::DRAW_PIP.0,

    DEFAULT_TEXTBOX_FLAGS = Flags::TYPEABLE.0
                          | Flags::CLICKABLE.0
                          | Flags::DRAW_MONO_TEXT.0
                          | Flags::DRAW_BACKGROUND.0
                          | Flags::DRAW_BORDER.0,

    DEFAULT_CONTAINER_FLAGS = Self::DRAW_MONO_TEXT.0
                            | Self::DRAW_SERIF_TEXT.0
                            | Self::DRAW_BACKGROUND.0
                            | Self::DRAW_BORDER.0,
);

#[derive(Debug, Default, Copy, Clone, Eq, Hash, PartialEq, PartialOrd)]
pub struct WidgetId(u64);

impl WidgetId {
    pub const INVALID: WidgetId = WidgetId(0);
}

#[derive(Debug, Default, Copy, Clone)]
enum Size {
    #[default]
    None,
    Exact(f32),
    TextContent(Font),
    PercentOfParent {
        amount:    f32,
        tolerance: f32,
    },
    SumOfChildren {
        tolerance: f32,
    },
}

#[derive(Debug, Default, Clone)]
struct Widget {
    id:    WidgetId,
    flags: Flags,

    parent:   Option<WidgetId>,
    children: Vec<WidgetId>,
    size:     (Size, Size), // semantic size (x-axis, y-axis)

    // CALCULATED PER FRAME
    abs_rect: Rect,    // absolute rect
    viz_rect: Rect,    // positioned rectangle - used to render
    interior: Rect,    // interior available content rectangle
    cursor:   (f32, f32), // layout cursor
    line_height: f32, // max height of all items on this line

    // CONTROL FIELDS
    display_text: String,

    text_idx: usize,
    text_buf: Vec<char>,

    slider_value:     f32,
    slider_value_min: f32,
    slider_value_max: f32,

    checked: bool,
}

#[derive(Debug, Default, Clone)]
pub struct WidgetEvents {
    mouse:      (isize, isize),
    drag_delta: (isize, isize),

    clicked:        bool,
    right_clicked:  bool,

    pressed:  bool,
    released: bool,
    resizing: bool,
    hovering: bool,

    text:      String,
    submitted: bool,
}

#[derive(Debug, Clone)]
pub enum DrawCommand {
    Scissor(Rect),
    Rect(Rect, Option<isize>, Color),
    Circle(f32, f32, f32, Color),
    Box(Rect, f32, Color),
    Text(Font, isize, isize, Color, String),
}

#[derive(Debug, Default, Copy, Clone)]
pub struct Font(u64);

//////////////////////////////////////

#[derive(Debug, Default, Clone, Copy)]
pub struct Rect {
    pub x1: f32,
    pub y1: f32,
    pub x2: f32,
    pub y2: f32,
}

impl Rect {
    pub const ZERO: Self = Self { x1: 0f32, y1: 0f32, x2: 0f32, y2: 0f32 };

    pub fn new(x1: f32, y1: f32, x2: f32, y2: f32) -> Self {
        Self { x1, y1, x2, y2 }
    }

    pub fn width(&self) -> f32 {
        (self.x2 - self.x1).max(0f32)
    }

    pub fn height(&self) -> f32 {
        (self.y2 - self.y1).max(0f32)
    }

    pub fn point_within(&self, px: f32, py: f32) -> bool {
        return px >= self.x1 && px <= self.x2
            && py >= self.y1 && py <= self.y2;
    }

    pub fn cut_from_left(&mut self, a: f32) -> Self {
        let minx = self.x1;
        self.x1  = self.x2.min(self.x1 + a);
        return Self::new(minx, self.y1, self.x1, self.y2);
    }

    pub fn cut_from_right(&mut self, a: f32) -> Self {
        let maxx = self.x2;
        self.x2  = self.x1.max(self.x2 - a);
        return Self::new(self.x2, self.y1, maxx, self.y2);
    }

    pub fn cut_from_top(&mut self, a: f32) -> Self {
        let miny = self.y1;
        self.y1  = self.y2.min(self.y1 + a);
        return Self::new(self.x1, miny, self.x2, self.y1);
    }

    pub fn cut_from_bottom(&mut self, a: f32) -> Self {
        let maxy = self.y2;
        self.y2  = self.y1.max(self.y2 - a);
        return Self::new(self.x1, self.y2, self.x2, maxy);
    }

    pub fn inset(&self, amount: f32) -> Self {
        Self::new(self.x1 + amount, self.y1 + amount, self.x2 - amount, self.y2 - amount)
    }

    pub fn outset(&self, amount: f32) -> Self {
        Self::new(self.x1 - amount, self.y1 - amount, self.x2 + amount, self.y2 + amount)
    }

    pub fn prepare(&mut self, from: CutFrom) -> Cut {
        return Cut{ rect: self, from }
    }

    pub fn intersect(&self, other: Self) -> Self {
        let x1 = self.x1.max(other.x1);
        let y1 = self.y1.max(other.y1);
        let x2 = self.x2.min(other.x2);
        let y2 = self.y2.min(other.y2);
        Self::new(x1, y1, x2, y2)
    }
}

#[derive(Debug)]
pub struct Cut {
    rect: *mut Rect,
    from: CutFrom,
}

#[derive(Debug, Clone)]
pub enum CutFrom {
    Top,
    Bottom,
    Left,
    Right,
}

impl Cut {
    pub fn make(&self, amount: f32) -> Rect {
        match self.from {
            CutFrom::Top    => unsafe { (*self.rect).cut_from_top(amount)    },
            CutFrom::Bottom => unsafe { (*self.rect).cut_from_bottom(amount) },
            CutFrom::Left   => unsafe { (*self.rect).cut_from_left(amount)   },
            CutFrom::Right  => unsafe { (*self.rect).cut_from_right(amount)  },
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Color {
    pub const DEBUG_RED:     Self = Self { r: 1.0, g: 0.0, b: 0.0, a: 1.0 };
    pub const DEBUG_GREEN:   Self = Self { r: 0.0, g: 1.0, b: 0.0, a: 1.0 };
    pub const DEBUG_BLUE:    Self = Self { r: 0.0, g: 0.0, b: 1.0, a: 1.0 };
    pub const DEBUG_YELLOW:  Self = Self { r: 1.0, g: 1.0, b: 0.0, a: 1.0 };
    pub const DEBUG_MAGENTA: Self = Self { r: 1.0, g: 0.0, b: 1.0, a: 1.0 };
    pub const DEBUG_CYAN:    Self = Self { r: 0.0, g: 1.0, b: 1.0, a: 1.0 };

    pub fn rgb(r: f32, g: f32, b: f32) -> Self {
        Self { r, g, b, a: 1.0 }
    }

    pub fn rgba(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }

    pub fn dim(&self, factor: f32) -> Self {
        Self {
            r: self.r * factor,
            g: self.g * factor,
            b: self.b * factor,
            a: self.a,
        }
    }

    pub fn fade(&self, factor: f32) -> Self {
        Self {
            r: self.r,
            g: self.g,
            b: self.b,
            a: self.a * factor,
        }
    }
}

impl Into<u32> for Color {
    fn into(self) -> u32 {
        ((self.a * 255.0) as u8 as u32) << 24 |
        ((self.r * 255.0) as u8 as u32) << 16 |
        ((self.g * 255.0) as u8 as u32) << 8 |
        ((self.b * 255.0) as u8 as u32)
    }
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Style {
    pub background: Color,
    pub foreground: Color,
    pub accent:     Color,
    pub border:     Color,
    pub highlight:  Color,

    pub padding: f32,
    pub spacing: f32,
    pub border_width: f32,
    pub rounded_corner_radius: f32,
}

impl Style {
    pub fn dark() -> Self {
        Self {
            background: Color::rgb(0.1, 0.1, 0.1),
            foreground: Color::rgb(0.9, 0.9, 0.9),
            accent:     Color::rgb(0.5, 0.5, 0.5),
            border:     Color::rgb(0.3, 0.3, 0.3),
            highlight:  Color::rgb(0.2, 0.2, 0.2),

            padding: 4f32,
            spacing: 8f32,
            border_width: 2f32,
            rounded_corner_radius: 8f32,
        }
    }
}

#[derive(Debug, Default, Clone)]
struct Stack<T>(std::vec::Vec<T>);

impl<T> Stack<T> {
    fn new() -> Self {
        Self(std::vec::Vec::new())
    }

    fn push(&mut self, item: T) {
        return self.0.push(item);
    }

    fn empty(&self) -> bool {
        self.0.is_empty()
    }

    #[track_caller]
    fn pop(&mut self) -> T {
        return self.0.pop().expect("stack was empty!");
    }

    #[track_caller]
    fn peek(&self) -> &T {
        return self.0.last().expect("stack was empty!");
    }

    fn peek_mut(&mut self) -> &mut T {
        return self.0.last_mut().expect("stack was empty!");
    }

    fn reset(&mut self) {
        self.0.clear();
    }
}
