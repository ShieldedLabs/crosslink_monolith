#![allow(warnings)]

const ONE_cTAZ: u64 = 100_000_000;

use std::net::Shutdown;
use std::thread::current;
use std::{hash::Hash};
use winit::{event::MouseButton, keyboard::KeyCode};
use clay_layout as clay;
use clay::{Clay, Declaration};
use clay::render_commands::RenderCommandConfig;
use clay::render_commands::RenderCommandConfig::{Rectangle, ScissorStart, ScissorEnd};
use clay::layout::{Alignment, LayoutAlignmentX, LayoutAlignmentY};
use std::collections::HashMap;
use std::cell::RefCell;
//use clay::*; // @Temporary

use wallet::{ BlockHeight, TxParts, TxStatus, WalletState, WalletTxKind, WalletTxPart, WalletRosterMember, FinalizerFilters, FinalizerRecencyStatus, str_from_ctaz };

use super::*;

thread_local! {
    static TEXT_MEASURE_CACHE: RefCell<HashMap<(u32, u32, String), f32, ahash::RandomState>> = RefCell::new(HashMap::with_hasher(ahash::RandomState::new()));
}

pub fn magic<'a, 'b, T>(mut_ref: &'a mut T) -> &'b mut T {
    let mut_ref = mut_ref as *mut T;
    return unsafe { &mut *mut_ref };
}

#[derive(Debug, Default)]
pub struct UiData {
    pub per_frame_strs: Vec<String>,

    pub send_address:  String,
    pub send_amount_index: usize,
    pub stake_address: String,
    pub stake_amount_index: usize,
    pub recv_address:  String,

    pub textboxes: HashMap<u32, TextboxState>,
    pub scroll_containers: HashMap<u32, ScrollContainerState>,

    pub openables: HashMap<u32, OpenableState>,

    pub tooltip_text: String,
    pub jump_target_pos: bool,
}


#[macro_export]
macro_rules! frame_strf {
    ($data:expr, $($arg:tt)*) => {
        $data.frame_str(&format_args!($($arg)*).to_string())
    };
}

#[macro_export]
macro_rules! set_tooltip_text {
    ($data:expr, $($arg:tt)*) => {
        $data.tooltip_text = format_args!($($arg)*).to_string();
    };
}

impl UiData {
    pub fn frame_str(&mut self, str: &str) -> &String {
        self.per_frame_strs.push(str.to_string().clone());
        return self.per_frame_strs.last().unwrap();
    }
}

pub fn dbg_ui(ui: &mut Context, is_rendering: bool) -> bool {


    if ui.input().key_pressed(KeyCode::Backquote) {
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

    if ui.input().key_pressed(KeyCode::F8) {
        ui.viz_op = ui.viz_op.cycle_next();
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
            ui.draw().text_line(FontKind::Mono, 0.0, 0.0, 16.0, "Pixel Inspector is Primed! Click to select pixel.", 0xff_00ff00);
        }
        if let Some((x, y)) = unsafe { *ui.draw().debug_pixel_inspector } {
            let x = x as isize; let y = y as isize;
            let mut draw_x = 0;
            let mut draw_y = 0;
            if x < ui.draw().window_width/2 { draw_x = ui.draw().window_width - 256 };
            if y < ui.draw().window_height/2 { draw_y = ui.draw().window_height - 256 };
            let color = unsafe { *ui.draw().debug_pixel_inspector_last_color };
            ui.draw().rectangle(draw_x as f32, draw_y as f32, draw_x as f32 + 256.0, draw_y as f32 + 256.0, 0xff_000000 | color);
            ui.draw().text_line(FontKind::Mono, draw_x as f32, draw_y as f32, 12.0, &format!("({},{}) = {:X}", x, y, color), 0xff_000000 | (color ^ u32::MAX));
        }
    }

    return false;
}

#[derive(Debug, Copy, Clone, PartialEq, PartialOrd, Default, Hash, Ord, Eq)] pub enum Direction { #[default] LeftToRight, TopToBottom }
#[derive(Debug, Copy, Clone, PartialEq, PartialOrd, Default)]                pub enum Floating  { #[default] None, Parent, Root(f32, f32) }
#[derive(Debug, Copy, Clone, PartialEq, PartialOrd, Default)]                pub enum ClipMode  { #[default] None, ClipX, ClipY, Clip, Scroll(f32) }
#[derive(Debug, Copy, Clone, PartialEq, PartialOrd, Default, Hash, Ord, Eq)] pub struct Align   { x: AlignX, y: AlignY }
#[derive(Debug, Copy, Clone, PartialEq, PartialOrd, Default, Hash, Ord, Eq)] pub enum AlignX    { #[default] Left, Right, Center }
#[derive(Debug, Copy, Clone, PartialEq, PartialOrd, Default, Hash, Ord, Eq)] pub enum AlignY    { #[default] Top, Bottom, Center }
#[derive(Debug, Copy, Clone, PartialEq, PartialOrd)]                         pub enum Sizing    { Fit(f32, f32), Grow(f32, f32), Fixed(f32), Percent(f32) }
#[derive(Debug, Copy, Clone, PartialEq, PartialOrd, Default, Hash, Ord, Eq)] pub struct Id      { id: u32, offset: u32, base_id: u32, len: usize, chars: *const u8 }
#[derive(Debug, Copy, Clone, PartialEq, PartialOrd, Default, Hash, Ord, Eq)] pub enum Wrap      { #[default] Words, Newline, None, Chars }
impl Default for Sizing { fn default() -> Self { Self::Fit(0.0, f32::MAX) } }
impl Align {
    pub const TopLeft:     Self = Self { y: AlignY::Top,    x: AlignX::Left };
    pub const Top:         Self = Self { y: AlignY::Top,    x: AlignX::Center };
    pub const TopRight:    Self = Self { y: AlignY::Top,    x: AlignX::Right };
    pub const Left:        Self = Self { y: AlignY::Center, x: AlignX::Left };
    pub const Center:      Self = Self { y: AlignY::Center, x: AlignX::Center };
    pub const Right:       Self = Self { y: AlignY::Center, x: AlignX::Right };
    pub const BottomLeft:  Self = Self { y: AlignY::Bottom, x: AlignX::Left };
    pub const Bottom:      Self = Self { y: AlignY::Bottom, x: AlignX::Center };
    pub const BottomRight: Self = Self { y: AlignY::Bottom, x: AlignX::Right };
}
// why can we not `use` these? namaste
pub const TopLeft:     Align = Align::TopLeft;
pub const Top:         Align = Align::Top;
pub const TopRight:    Align = Align::TopRight;
pub const Left:        Align = Align::Left;
pub const Center:      Align = Align::Center;
pub const Right:       Align = Align::Right;
pub const BottomLeft:  Align = Align::BottomLeft;
pub const Bottom:      Align = Align::Bottom;
pub const BottomRight: Align = Align::BottomRight;

#[macro_export] macro_rules! fit {
    ($min:expr, $max:expr) => { Sizing::Fit($min, $max) };
    ($min:expr)            => { fit!($min, f32::MAX) };
    ()                     => { fit!(0.0) };
}
#[macro_export] macro_rules! grow {
    ($min:expr, $max:expr) => { Sizing::Grow($min, $max) };
    ($min:expr)            => { grow!($min, f32::MAX) };
    ()                     => { grow!(0.0) };
}
#[macro_export] macro_rules! fixed { ($val:expr) => { Sizing::Fixed($val) }; }
#[macro_export] macro_rules! percent {
    ($percent:expr) => {{
        const _: () = assert!(
            $percent >= 0.0 && $percent <= 1.0,
            "Percent value must be between 0.0 and 1.0 inclusive!"
        );
        Sizing::Percent($percent)
    }};
}

use ClipMode::{ClipX, ClipY, Clip, Scroll};
use Direction::*;

pub const Id: Id = Id { id: 0, offset: 0, base_id: 0, len: 0, chars: std::ptr::null() };

#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub struct Decl {
    id: Id,
    direction: Direction,
    floating: Floating,
    colour: (u8, u8, u8, u8),
    radius: (f32, f32, f32, f32),
    padding: (f32, f32, f32, f32),
    clip: ClipMode,
    child_gap: f32,
    align: Align,
    width:  Sizing,
    height: Sizing,
}
// Ease-of-use constant for the builder pattern thing, so you can write Decl{..Decl} to get a default Decl.
// I can't do #[derive_const(Default)] because that's only on Rust nightly. And it would probably be complex anyway.
pub const Decl: Decl = Decl {
    id:        Id,
    direction: Direction::LeftToRight,
    floating:  Floating::None,
    colour:    (0,   0,   0,   0),
    radius:    (0.0, 0.0, 0.0, 0.0),
    padding:   (0.0, 0.0, 0.0, 0.0),
    clip:      ClipMode::None,
    child_gap: 0.0,
    align:     TopLeft,
    width:     Sizing::Fit(0.0, f32::MAX),
    height:    Sizing::Fit(0.0, f32::MAX),
};

#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub struct TextDecl {
    font: FontKind,
    h: f32,
    colour: (u8, u8, u8, u8),
    align: AlignX,
    wrap: Wrap,
}
pub const TextDecl: TextDecl = TextDecl {
    font: FontKind::Normal,
    h: 0.0,
    colour: WHITE,
    align: AlignX::Left,
    wrap: Wrap::Words,
};



impl Id {
    pub const VIZ_GUI: Self = Self { id: 1, ..Id };
    pub const CHAIN_MINIMAP_POW: Self = Self { id: 0x6d69_6e6d, ..Id };
    pub const CHAIN_MINIMAP_POS: Self = Self { id: 0x6d70_6f73, ..Id };
    fn clay(&self) -> clay::id::Id {
        clay::id::Id {
            id: clay::Clay_ElementId {
                id: self.id,
                offset: self.offset,
                baseId: self.base_id,
                stringId: clay::Clay_String {
                    isStaticallyAllocated: false,
                    length: self.len as i32,
                    chars: self.chars as *const i8
                }
            }
        }
    }
}
pub fn id(label: &str) -> Id {
    let id = unsafe { clay::Clay__HashString(label.into(), 0, clay::Clay__GetParentElementId()) };
    Id {
        id: id.id,
        offset: id.offset,
        base_id: id.baseId,
        len: id.stringId.length as usize,
        chars: id.stringId.chars as *const u8
    }
}

pub fn id_index(label: &str, index: u32) -> Id {
    let id = unsafe { clay::Clay__HashString(label.into(), index, clay::Clay__GetParentElementId()) };
    Id {
        id: id.id,
        offset: id.offset,
        base_id: id.baseId,
        len: id.stringId.length as usize,
        chars: id.stringId.chars as *const u8
    }
}

#[derive(Default)] pub struct Element { decl: Decl }
pub fn elem() -> Element { elem_bgn(); Element::default() }
impl Drop for Element { fn drop(&mut self) { elem_end(); } }

pub fn elem_bgn() { unsafe { clay::Clay__OpenElement();  } }
pub fn elem_end() { unsafe { clay::Clay__CloseElement(); } }

pub const Clay_ElementId_ZERO: clay::Clay_ElementId = clay::Clay_ElementId { id: 0, offset: 0, baseId: 0, stringId: clay::Clay_String { isStaticallyAllocated: false, length: 0, chars: std::ptr::null() } };
pub const Clay_SizingMinMax_ZERO: clay::Clay_SizingMinMax = clay::Clay_SizingMinMax { min: 0f32, max: f32::MAX };
pub const Clay_SizingAxis_ZERO: clay::Clay_SizingAxis = clay::Clay_SizingAxis {
    size: clay::Clay_SizingAxis__bindgen_ty_1 { minMax: Clay_SizingMinMax_ZERO },
    type_: clay::Clay__SizingType_CLAY__SIZING_TYPE_FIT
};
pub const Clay_Sizing_ZERO: clay::Clay_Sizing = clay::Clay_Sizing { width: Clay_SizingAxis_ZERO, height: Clay_SizingAxis_ZERO };
pub const Clay_Padding_ZERO: clay::Clay_Padding = clay::Clay_Padding { left: 0, right: 0, top: 0, bottom: 0 };
pub const Clay_ChildAlignment_ZERO: clay::Clay_ChildAlignment = clay::Clay_ChildAlignment { x: 0 as _, y: 0 as _ };
pub const Clay_LayoutConfig_ZERO: clay::Clay_LayoutConfig = clay::Clay_LayoutConfig {
    sizing: Clay_Sizing_ZERO,
    padding: Clay_Padding_ZERO,
    childGap: 0,
    childAlignment: Clay_ChildAlignment_ZERO,
    layoutDirection: clay::Clay_LayoutDirection_CLAY_LEFT_TO_RIGHT,
};
pub const Clay_Color_ZERO: clay::Clay_Color = clay::Clay_Color { r: 0.0, g: 0.0, b: 0.0, a: 0.0 };
pub const Clay_CornerRadius_ZERO: clay::Clay_CornerRadius = clay::Clay_CornerRadius { topLeft: 0f32, topRight: 0f32, bottomLeft: 0f32, bottomRight: 0f32 };
pub const Clay_ElementDeclaration_ZERO: clay::Clay_ElementDeclaration = clay::Clay_ElementDeclaration {
    id: Clay_ElementId_ZERO,
    layout: Clay_LayoutConfig_ZERO,
    backgroundColor: Clay_Color_ZERO,
    cornerRadius: Clay_CornerRadius_ZERO,
    aspectRatio: clay::Clay_AspectRatioElementConfig { aspectRatio: 0.0 },
    image: clay::Clay_ImageElementConfig { imageData: std::ptr::null_mut() },
    floating: clay::Clay_FloatingElementConfig {
        offset: clay::Clay_Vector2 { x: 0f32, y: 0f32 },
        expand: clay::Clay_Dimensions { width: 0f32, height: 0f32 },
        parentId: 0,
        zIndex: 0,
        attachPoints: clay::Clay_FloatingAttachPoints {
            element: clay::Clay_FloatingAttachPointType_CLAY_ATTACH_POINT_LEFT_TOP,
            parent:  clay::Clay_FloatingAttachPointType_CLAY_ATTACH_POINT_LEFT_TOP,
        },
        pointerCaptureMode: clay::Clay_PointerCaptureMode_CLAY_POINTER_CAPTURE_MODE_CAPTURE,
        attachTo: clay::Clay_FloatingAttachToElement_CLAY_ATTACH_TO_NONE,
        clipTo: clay::Clay_FloatingClipToElement_CLAY_CLIP_TO_NONE
    },
    custom: clay::Clay_CustomElementConfig { customData: std::ptr::null_mut() },
    clip: clay::Clay_ClipElementConfig { horizontal: false, vertical: false, childOffset: clay::Clay_Vector2 { x: 0f32, y: 0f32 } },
    border: clay::Clay_BorderElementConfig {
        color: Clay_Color_ZERO,
        width: clay::Clay_BorderWidth { left: 0, right: 0, top: 0, bottom: 0, betweenChildren: 0 }
    },
    userData: std::ptr::null_mut(),
};

    pub fn decl(item: Decl) {
        fn sizing(sizing: Sizing) -> clay::layout::Sizing {
            match sizing {
                Sizing::Fit(min, max)  => { clay::layout::Sizing::Fit(min, max) }
                Sizing::Grow(min, max) => { clay::layout::Sizing::Grow(min, max) }
                Sizing::Fixed(x)       => { clay::layout::Sizing::Fixed(x) }
                Sizing::Percent(p)     => { clay::layout::Sizing::Percent(p) }
            }
        }

        let mut decl = Clay_ElementDeclaration_ZERO;
        decl.backgroundColor = clay::Clay_Color {
            r: item.colour.0 as f32,
            g: item.colour.1 as f32,
            b: item.colour.2 as f32,
            a: (item.colour.3 | 0x01) as f32
        };
        decl.id = item.id.clay().id;
        let (clip_x, clip_y) = match item.clip {
            ClipMode::None      => (false, false, ),
            ClipMode::ClipX     => (true,  false, ),
            ClipMode::ClipY     => (false, true,  ),
            ClipMode::Clip      => (true,  true,  ),
            ClipMode::Scroll(_) => (false, true,  ),
        };
        decl.clip = clay::Clay_ClipElementConfig {
            horizontal: clip_x,
            vertical:   clip_y,
            childOffset: match item.clip {
                ClipMode::Scroll(y) => clay::Clay_Vector2 { x: 0.0, y },
                _                   => clay::Clay_Vector2 { x: 0.0, y: 0.0 },
            }
        };
        match item.floating {
            Floating::Parent => {
                decl.floating.attachTo = clay::Clay_FloatingAttachToElement_CLAY_ATTACH_TO_PARENT;
                decl.floating.clipTo = clay::Clay_FloatingClipToElement_CLAY_CLIP_TO_ATTACHED_PARENT;
                decl.floating.attachPoints = clay::Clay_FloatingAttachPoints {
                    element: clay::Clay_FloatingAttachPointType_CLAY_ATTACH_POINT_CENTER_CENTER,
                    parent:  clay::Clay_FloatingAttachPointType_CLAY_ATTACH_POINT_CENTER_CENTER,
                };
                // TODO: decl.floating.pointerCaptureMode = clay::Clay_PointerCaptureMode_CLAY_POINTER_CAPTURE_MODE_PASSTHROUGH;
            },
            Floating::Root(x, y) => {
                decl.floating.attachTo = clay::Clay_FloatingAttachToElement_CLAY_ATTACH_TO_ROOT;
                decl.floating.offset.x = x;
                decl.floating.offset.y = y;
                decl.floating.attachPoints = clay::Clay_FloatingAttachPoints {
                    element: clay::Clay_FloatingAttachPointType_CLAY_ATTACH_POINT_LEFT_TOP,
                    parent:  clay::Clay_FloatingAttachPointType_CLAY_ATTACH_POINT_LEFT_TOP,
                };
                decl.floating.pointerCaptureMode = clay::Clay_PointerCaptureMode_CLAY_POINTER_CAPTURE_MODE_PASSTHROUGH;
            },
            _ => {},
        }
        decl.layout.sizing.width  = clay::Clay_SizingAxis::from(sizing(item.width));
        decl.layout.sizing.height = clay::Clay_SizingAxis::from(sizing(item.height));
        decl.layout.padding = clay::Clay_Padding::from(clay::Clay_Padding {
            left:   item.padding.0 as u16,
            right:  item.padding.1 as u16,
            top:    item.padding.2 as u16,
            bottom: item.padding.3 as u16,
        });
        decl.layout.childGap = item.child_gap as u16;
        decl.layout.childAlignment = clay::Clay_ChildAlignment { x: item.align.x as _, y: item.align.y as _ };
        decl.layout.layoutDirection = item.direction as _;
        decl.cornerRadius = clay::Clay_CornerRadius {
            topLeft:     item.radius.0,
            topRight:    item.radius.1,
            bottomLeft:  item.radius.2,
            bottomRight: item.radius.3
        };

        unsafe { clay::Clay__ConfigureOpenElement(decl); }
    }

impl Element {
    fn decl(&mut self, item: Decl) -> &mut Self {
        decl(item);
        self.decl = item;
        self
    }
}

pub const PANE_PERCENT_LEFT:  f32 = 0.28; // @Todo: @PreventOverflowOnZoom
pub const PANE_PERCENT_RIGHT: f32 = 0.24; // @Todo: @PreventOverflowOnZoom

pub const WHITE:            (u8, u8, u8, u8) = (0xff, 0xff, 0xff, 0xff);
// pub const PANE_COL:         (u8, u8, u8, u8) = (0x12, 0x12, 0x12, 0xff); // @FigmaScreenshot
pub const PANE_COL:         (u8, u8, u8, u8) = (0x13, 0x13, 0x13, 0xff); // @FigmaScreenshot
pub const INACTIVE_TAB_COL: (u8, u8, u8, u8) = (0x0f, 0x0f, 0x0f, 0xff);
pub const ACTIVE_TAB_COL:   (u8, u8, u8, u8) = PANE_COL;

pub const BUTTON_GREY:      (u8, u8, u8, u8) = (0x24, 0x24, 0x24, 0xff); // @FigmaScreenshot
pub const BUTTON_BLUE:      (u8, u8, u8, u8) = (0x1a, 0x36, 0x51, 0xff); // @FigmaScreenshot
pub const BUTTON_ORANGE:    (u8, u8, u8, u8) = (0x59, 0x41, 0x11, 0xff); // @FigmaScreenshot

pub const MODAL_COL: (u8, u8, u8, u8) = (0x1e, 0x1e, 0x1e, 0xff); // @FigmaScreenshot

pub const TRANSACTION_HISTORY_CONTAINER_COL: (u8, u8, u8, u8) = (0x22, 0x22, 0x24, 0xff); // @FigmaScreenshot

pub const fn clay_colour(colour: (u8, u8, u8, u8)) -> clay::Color { clay::Color::rgba(colour.0 as f32, colour.1 as f32, colour.2 as f32, colour.3 as f32) }

#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub enum Modal {
    #[default] None,
    Jump,
    Send,
    Receive,
    Stake,
    Unstake,
    Retarget,
    User, // settings
}

pub fn rgba_to_hsva(r: u8, g: u8, b: u8, a: u8) -> (u8, u8, u8, u8) {
    let rf = r as f32 / 255.0;
    let gf = g as f32 / 255.0;
    let bf = b as f32 / 255.0;

    let max = rf.max(gf).max(bf);
    let min = rf.min(gf).min(bf);
    let delta = max - min;

    // Compute Hue (0–1)
    let h = if delta == 0.0 {
        0.0
    } else if max == rf {
        ((gf - bf) / delta).rem_euclid(6.0)
    } else if max == gf {
        (bf - rf) / delta + 2.0
    } else {
        (rf - gf) / delta + 4.0
    } / 6.0;

    // Compute Saturation and Value (0–1)
    let s = if max == 0.0 { 0.0 } else { delta / max };
    let v = max;

    // Convert to u8 range
    let h8 = (h * 255.0).round().clamp(0.0, 255.0) as u8;
    let s8 = (s * 255.0).round().clamp(0.0, 255.0) as u8;
    let v8 = (v * 255.0).round().clamp(0.0, 255.0) as u8;

    (h8, s8, v8, a)
}

pub fn hsva_to_rgba(h: u8, s: u8, v: u8, a: u8) -> (u8, u8, u8, u8) {
    let hf = (h as f32 / 255.0) * 6.0;  // 0..6
    let sf = s as f32 / 255.0;          // 0..1
    let vf = v as f32 / 255.0;          // 0..1

    let c = vf * sf;
    let x = c * (1.0 - ((hf % 2.0) - 1.0).abs());
    let m = vf - c;

    let (rf, gf, bf) = match hf as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x), // covers sector 5
    };

    let r = ((rf + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    let g = ((gf + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    let b = ((bf + m) * 255.0).round().clamp(0.0, 255.0) as u8;

    (r, g, b, a)
}

pub trait HSVA_RGBA {
    #[inline(always)] fn hsva(self) -> Self;
    #[inline(always)] fn rgba(self) -> Self;
}
impl HSVA_RGBA for (u8, u8, u8, u8) {
    #[inline(always)] fn hsva(self) -> Self { rgba_to_hsva(self.0, self.1, self.2, self.3) }
    #[inline(always)] fn rgba(self) -> Self { hsva_to_rgba(self.0, self.1, self.2, self.3) }
}

impl Context {
    pub fn new() -> Context { Context { scale: 1f32, zoom: 1f32, dpi_scale: 1f32, global_audio_volume: 1.0, ..Default::default() } }
    pub fn draw(&self)  -> &DrawCtx  { unsafe { &*self.draw     } }
    pub fn draw_mut(&mut self) -> &mut DrawCtx { unsafe { &mut *(self.draw as *mut DrawCtx) } }
    pub fn input(&self) -> &InputCtx { unsafe { &*self.input    } }
    pub fn clay(&self)  -> &mut Clay { unsafe { &mut *self.clay } }

    pub fn scale(&self, size: f32) -> f32 { (size * self.scale).floor() }
    pub fn scale32(&self, size: f32) -> u32 { self.scale(size) as u32 }
    pub fn scale16(&self, size: f32) -> u16 { self.scale(size) as u16 }

    pub fn hovered_raw(&self, id: Id) -> bool { unsafe { clay::Clay_PointerOver(id.clay().id) } }
    pub fn hovered(&self, id: Id) -> bool {
        self.mouse_pressed_id == Id::default() && self.hovered_raw(id)
    }
    pub fn hovered_uniquely(&self, id: Id) -> bool {
        self.mouse_pressed_id == Id::default() && self.hovered_id == id
    }

    pub fn button_ex(&mut self, act_on_press: bool, colour: (u8, u8, u8, u8), id: Id, enabled: bool, pointer_on_hover: winit::window::CursorIcon) -> (bool, (u8, u8, u8, u8), (u8, u8, u8, u8)) {

        let mouse_hover = self.hovered_raw(id);
        let key_hover   = self.nav_enable && self.nav_id == id.id;

        let mouse_held     = mouse_hover && self.input().mouse_held(winit::event::MouseButton::Left);
        let mouse_pressed  = mouse_hover && self.input().mouse_pressed(winit::event::MouseButton::Left);
        let mouse_released = mouse_hover && self.input().mouse_released(winit::event::MouseButton::Left);

        let key_held     = key_hover && self.input().key_held(winit::keyboard::KeyCode::Enter);
        let key_pressed  = key_hover && self.input().key_pressed(winit::keyboard::KeyCode::Enter);
        let key_released = key_hover && self.input().key_released(winit::keyboard::KeyCode::Enter);

        if mouse_pressed { self.mouse_pressed_id = id; }
        if key_pressed   { self.key_pressed_id   = id; }
        let mouse_activated  = enabled && self.mouse_pressed_id == id && if act_on_press { mouse_pressed } else { mouse_released };
        let key_activated    = enabled && self.key_pressed_id   == id && if act_on_press { key_pressed   } else { key_released   };

        if mouse_hover && pointer_on_hover != winit::window::CursorIcon::Default &&
            (self.mouse_pressed_id == Id::default() || self.mouse_pressed_id == id) {
            self.cursor = winit::window::Cursor::Icon(pointer_on_hover);
        }

        let held      = mouse_held      || key_held;
        let hover     = mouse_hover     || key_hover;
        let pressed   = mouse_pressed   || key_pressed;
        let released  = mouse_released  || key_released;
        let activated = mouse_activated || key_activated;

        let mut hsva = colour.hsva();
        if !enabled {
            hsva.2 = hsva.2.mul(0.75);
            hsva.1 = hsva.1.mul(0.75);
        } else if !held && hover {
            hsva.2 = ((hsva.2 as f32) * 1.25).min(255.0) as u8;
        } else if held && (self.mouse_pressed_id == id || self.key_pressed_id == id) {
            hsva.2 = hsva.2.mul(0.85);
        }
        let colour = hsva.rgba();

        // let mut text_hsva = hsva; // WHITE.hsva();
        // // text_hsva.1 = text_hsva.1.mul(0.5);
        // text_hsva.2 = ((text_hsva.2 as f32) * 3.0).min(255.0) as u8;

        let mut text_hsva = WHITE.hsva();

        text_hsva.0 = hsva.0;
        text_hsva.1 = hsva.1.mul(0.45);
        text_hsva.2 = 0xf8;

        if !enabled {
            text_hsva.2 = text_hsva.2.mul(0.5);
        }
        let text_colour = text_hsva.rgba();

        if !self.nav_skip && !self.nav_id_to_idx.contains_key(&id.id) {
            let n = self.nav_id_to_idx.len();
            assert!(n == self.nav_idx_to_id.len());
            self.nav_id_to_idx.insert(id.id, n);
            self.nav_idx_to_id.push(id.id);
        }

        (activated, colour, text_colour)
    }

    pub fn button(&mut self, id: Id) -> (bool, (u8, u8, u8, u8), (u8, u8, u8, u8)) { return self.button_ex(true, BUTTON_GREY, id, true, winit::window::CursorIcon::Default); }

    pub fn text(&self, label: &str, decl: TextDecl) {
        let config = clay::text::TextConfig::new()
            .font_id(decl.font as u16)
            .font_size(decl.h as u16)
            .color(clay_colour(decl.colour))
            .alignment(match decl.align {
                AlignX::Left   => clay::text::TextAlignment::Left,
                AlignX::Right  => clay::text::TextAlignment::Right,
                AlignX::Center => clay::text::TextAlignment::Center,
            })
            .wrap_mode(match decl.wrap {
                Wrap::Words   => clay::text::TextElementConfigWrapMode::Words,
                Wrap::Newline => clay::text::TextElementConfigWrapMode::Newline,
                Wrap::None    => clay::text::TextElementConfigWrapMode::None,
                Wrap::Chars   => clay::text::TextElementConfigWrapMode::Chars
            })
            .end();
        unsafe { clay::Clay__OpenTextElement(label.into(), config.into()) };
    }

    pub fn tab_ex(&mut self,
              radius: (f32, f32, f32, f32),
              padding: (f32, f32, f32, f32),
              tab_id: &mut Id,
              id: Id,
              label: &str) -> Id {
        let tab_text_h = self.scale(18.0);

        let radius = (radius.0, radius.1, 0.0, 0.0);

        let (clicked, _, _) = self.button(id);
        if clicked || *tab_id == Id::default() {
            *tab_id = id;
        }

        if let _ = elem().decl(Decl {
            id,
            radius, padding,
            colour: if *tab_id == id { ACTIVE_TAB_COL } else { INACTIVE_TAB_COL },
            width: grow!(),
            height: grow!(),
            align: Center,
            ..Decl
        }) {
            let colour = WHITE.mul(if *tab_id == id { 1.0 } else { 0.35 });
            self.text(label, TextDecl { colour, h: tab_text_h, align: AlignX::Center, ..TextDecl });
        }

        id
    }

    pub fn tab(&mut self,
           radius:  (f32, f32, f32, f32),
           padding: (f32, f32, f32, f32),
           tab_id: &mut Id,
           label: &str) -> Id {
        let id = id(label);
        self.tab_ex(radius, padding, tab_id, id, label)
    }

    pub fn textbox(&mut self, data: &mut UiData, id: Id, hint: &str, text_decl: TextDecl) -> String {
        let child_gap = self.scale(12.0); // @Duplicate :TextBox
        let padding   = child_gap.dup4(); // @Duplicate :TextBox
        let radius    = child_gap.dup4(); // @Duplicate :TextBox

        let (activated, colour, text_colour) = self.button_ex(true, BUTTON_GREY, id, true, winit::window::CursorIcon::Text);

        if activated {
            self.nav_id = id.id;
            self.nav_enable = true;
        }

        let text = {
            let mut textbox_state = &mut data.textboxes.entry(id.id).or_default();

            textbox_state.h = text_decl.h;
            textbox_state.font = text_decl.font;

            if self.nav_id == id.id {
                let mut moved = false;

                let shift = (self.input().key_held(KeyCode::ShiftLeft)   || self.input().key_held(KeyCode::ShiftRight));
                let ctrl  = (self.input().key_held(KeyCode::ControlLeft) || self.input().key_held(KeyCode::ControlRight));

                if !shift && textbox_state.selection.1 != textbox_state.selection.0 {
                    let (min, max) = (textbox_state.selection.0.min(textbox_state.selection.1),
                                      textbox_state.selection.0.max(textbox_state.selection.1));
                    if self.input().key_pressed(KeyCode::ArrowRight) { moved = true; textbox_state.selection.0 = max; }
                    if self.input().key_pressed(KeyCode::ArrowLeft)  { moved = true; textbox_state.selection.0 = min; }
                } else {
                    if self.input().key_pressed(KeyCode::ArrowRight) { moved = true;                                      { textbox_state.selection.0 += 1; }   }
                    if self.input().key_pressed(KeyCode::ArrowLeft)  { moved = true; { if (textbox_state.selection.0 > 0) { textbox_state.selection.0 -= 1; } } }
                }

                if self.input().key_pressed(KeyCode::Home)       { moved = true; textbox_state.selection.0 = 0; }
                if self.input().key_pressed(KeyCode::ArrowUp)    { moved = true; textbox_state.selection.0 = 0; }

                if self.input().key_pressed(KeyCode::End)        { moved = true; textbox_state.selection.0 = textbox_state.text_buf.len(); }
                if self.input().key_pressed(KeyCode::ArrowDown)  { moved = true; textbox_state.selection.0 = textbox_state.text_buf.len(); }

                textbox_state.selection.0 = textbox_state.selection.0.min(textbox_state.text_buf.len());
                textbox_state.selection.1 = textbox_state.selection.1.min(textbox_state.text_buf.len());

                if moved && !shift {
                    textbox_state.selection.1 = textbox_state.selection.0;
                }

                let has_selection = (textbox_state.selection.1 != textbox_state.selection.0);

                let select_all = (ctrl && self.input().key_pressed(KeyCode::KeyA)) || activated;
                let copy       = (ctrl && self.input().key_pressed(KeyCode::KeyC));
                let cut        = (ctrl && self.input().key_pressed(KeyCode::KeyX)) || (shift && self.input().key_pressed(KeyCode::Delete));

                let backspace = self.input().key_pressed(KeyCode::Backspace);
                let delete    = self.input().key_pressed(KeyCode::Delete);
                let has_input = self.input().text_input.is_some();

                let should_copy = has_selection && (copy || cut);
                let should_cut  = has_selection && cut;

                if select_all {
                    textbox_state.selection.1 = 0;
                    textbox_state.selection.0 = textbox_state.text_buf.len();
                }

                // Sort selection
                let (mut min, mut max) = (textbox_state.selection.0.min(textbox_state.selection.1),
                                          textbox_state.selection.0.max(textbox_state.selection.1));

                if min == max {
                    if backspace   { min = min.saturating_sub(1); }
                    else if delete { max = max.saturating_add(1); }
                }

                min = min.min(textbox_state.text_buf.len());
                max = max.min(textbox_state.text_buf.len());

                if should_copy {
                    // Send text at selection to clipboard
                    let slice = &textbox_state.text_buf[min..max];

                    let mut text = String::with_capacity(slice.len() * 4);
                    text.extend(slice.iter().copied());
                    self.input().send_to_clipboard(&text);
                }

                if (backspace || delete || should_cut || has_input) {
                    // Replace text at selection with text input
                    let pre_slice = &textbox_state.text_buf[..min];
                    let pst_slice = &textbox_state.text_buf[max..];

                    let input_len = if let Some(input) = &self.input().text_input { input.len() } else { 0 };

                    let mut new_buf = Vec::with_capacity(pre_slice.len() + input_len + pst_slice.len());
                    new_buf.extend_from_slice(pre_slice);
                    if let Some(input) = &self.input().text_input {
                        new_buf.extend_from_slice(input);
                    }
                    new_buf.extend_from_slice(pst_slice);

                    textbox_state.text_buf = new_buf;
                    textbox_state.selection.0 = min + input_len;

                    textbox_state.selection.1 = textbox_state.selection.0;
                }
            }

            let mut text = String::with_capacity(textbox_state.text_buf.len() * 4);
            text.extend(textbox_state.text_buf.iter().copied());

            text
        };

        let str = frame_strf!(data, "{} ", text); // space character to force empty strings to have a height in clay

        if let _ = elem().decl(Decl {
            id,
            padding, child_gap, radius,
            width: grow!(),
            height: fit!(),
            clip: ClipX,
            colour,
            ..Decl
        }) {

            let use_hint  = text.len() <= 0;
            let str       = if use_hint { hint } else { str };
            let colour    = if use_hint { let mut hsva = text_colour.hsva(); hsva.2 /= 2; hsva.rgba() } else { text_colour };
            let text_decl = TextDecl { colour, ..text_decl };

            self.text(&str, text_decl);
        }

        text
    }

    pub fn scroll_container<'data>(&mut self, data: &'data mut UiData, id: Id, scroll_end_height: f32) -> (Id, ClipMode, &'data mut f32, f32, f32, f32) {
        let mut scroll_container_state = data.scroll_containers.entry(id.id).or_insert(Default::default());

        scroll_container_state.content_height = {
            let scroll_container_data: clay::Clay_ScrollContainerData = unsafe { clay::Clay_GetScrollContainerData(id.clay().id) };
            if scroll_container_data.found {
                scroll_container_data.contentDimensions.height
            } else {
                0.0
            }
        };
        let max = scroll_container_state.content_height / self.scale - scroll_end_height;

        if self.hovered(id) && !self.suppress_scroll_for_clay {
            scroll_container_state.scroll -= self.input().zoom_delta     as f32 * 32.0;
            scroll_container_state.scroll -= self.input().scroll_delta.1 as f32 * 32.0;

            if self.input().key_pressed(KeyCode::PageUp) {
                scroll_container_state.scroll -= scroll_container_state.viewport_height / self.scale;
            }
            if self.input().key_pressed(KeyCode::PageDown) {
                scroll_container_state.scroll += scroll_container_state.viewport_height / self.scale;
            }
            if self.input().key_pressed(KeyCode::Home) {
                scroll_container_state.scroll = 0.0;
            }
            if self.input().key_pressed(KeyCode::End) {
                scroll_container_state.scroll = max;
            }
        }

        if scroll_container_state.scroll > max {
            scroll_container_state.scroll = max;
        }
        if scroll_container_state.scroll < 0.0 {
            scroll_container_state.scroll = 0.0;
        }

        (id,
         Scroll(-scroll_container_state.scroll * self.scale),
         &mut scroll_container_state.scroll,
         scroll_container_state.content_height,
         scroll_container_state.viewport_height,
         max)
    }

    pub fn scroll_container_bgn(
        &mut self,
        data: &mut UiData,
        padding: (f32, f32, f32, f32), child_gap: f32,
        id: Id, clip: ClipMode, scroll: f32, content_h: f32, viewport_h: f32, max: f32
    ) {
        elem_bgn();
        decl(Decl {
            id: id_index("Outer Container", id.id),
            colour: TRANSACTION_HISTORY_CONTAINER_COL,
            radius: padding.0.dup4(),
            width:  percent!(1.0),
            height: grow!(),
            direction: LeftToRight,
            align: TopLeft,
            ..Decl
        });
        elem_bgn();
        decl(Decl {
            id,
            padding: (padding.0, 0.0, padding.2, padding.3),
            child_gap,
            width:  grow!(),
            height: percent!(1.0),
            direction: TopToBottom,
            clip,
            align: Top,
            ..Decl
        });
    }
    pub fn scroll_container_end(
        &mut self,
        data: &mut UiData,
        padding: (f32, f32, f32, f32),
        id: Id, clip: ClipMode, scroll: f32, content_h: f32, viewport_h: f32, max: f32
    ) {
        elem_end();

        let radius = self.scale(6.0);

        let scrollbar_region_h = viewport_h - padding.2 - padding.3;

        let handle_height = {
            let handle_pct = if content_h == 0.0 { 1.0 } else { viewport_h / content_h };
            let handle_height = handle_pct * scrollbar_region_h;
            handle_height.max(radius * 3.0).min(scrollbar_region_h)
        };

        let scroll_pct = (if max == 0.0 { 0.0 } else { scroll / max }).max(0.0).min(1.0);

        let handle_offset = scroll_pct * (scrollbar_region_h - handle_height);

        self.nav_skip = true; // @Hack.

        if max > 0.0 && handle_height < scrollbar_region_h && let _ = elem().decl(Decl {
            id:      id_index("Scrollbar Region", id.id as u32),
            width:   fixed!(self.scale(32.0)),
            padding: (self.scale(10.0), 0.0, padding.2, 0.0),
            direction: TopToBottom,
            align: TopLeft,
            ..Decl
        }) {
            let button_id = ui::id("Scrollbar Handle");
            let colour = (0x60, 0x60, 0x60, 0);
            let (activated, mut colour, _) = self.button_ex(true, colour, button_id, true, winit::window::CursorIcon::Default);
            colour.3 = colour.2;

            if self.mouse_pressed_id == button_id {
                let mut scroll_container_state = data.scroll_containers.entry(id.id).or_insert(Default::default());

                let delta_viewport_px  = self.input().mouse_delta().1 as f32;
                let delta_viewport_pct = delta_viewport_px / (scrollbar_region_h - handle_height);

                let content_scrollable_h = max;

                let delta_content_pct = delta_viewport_pct;
                let delta_content_px  = delta_content_pct * content_scrollable_h;

                scroll_container_state.scroll += delta_content_px;
            }

            let _ = elem().decl(Decl {                                                                            height: fixed!(handle_offset), ..Decl });
            let _ = elem().decl(Decl { id: button_id, colour, radius: radius.dup4(), width: fixed!(radius * 2.0), height: fixed!(handle_height), ..Decl });
        } else {
            let _ = elem().decl(Decl { width: fixed!(padding.1), ..Decl });
        }

        self.nav_skip = false; // @Hack.
        elem_end();
    }

    pub fn openable(&mut self, data: &mut UiData, id: Id, activated: bool, open_by_default: bool) -> bool {
        let mut openable_state = &mut data.openables.entry(id.id).or_insert(OpenableState { open: open_by_default });
        if activated {
            openable_state.open = !openable_state.open;
        }

        openable_state.open
    }
}

pub trait     Dup2: Copy { fn dup2(self) -> (Self, Self); }
impl<T: Copy> Dup2 for T { fn dup2(self) -> (Self, Self) { (self, self) } }
pub trait     Dup3: Copy { fn dup3(self) -> (Self, Self, Self); }
impl<T: Copy> Dup3 for T { fn dup3(self) -> (Self, Self, Self) { (self, self, self) } }
pub trait     Dup4: Copy { fn dup4(self) -> (Self, Self, Self, Self); }
impl<T: Copy> Dup4 for T { fn dup4(self) -> (Self, Self, Self, Self) { (self, self, self, self) } }

// Implementation of `tuple.mul(scalar)`. Helper "AsF32" trait to get it working.
// This would literally be a two-liner, if Rust generics had C++-style SFINAE.
pub trait AsF32         { #[inline(always)] fn to_f32(self) -> f32;                #[inline(always)] fn from_f32(x: f32) -> Self; }
impl      AsF32 for u8  { #[inline(always)] fn to_f32(self) -> f32 { self as f32 } #[inline(always)] fn from_f32(x: f32) -> Self { x as Self } }
impl      AsF32 for u16 { #[inline(always)] fn to_f32(self) -> f32 { self as f32 } #[inline(always)] fn from_f32(x: f32) -> Self { x as Self } }
impl      AsF32 for u32 { #[inline(always)] fn to_f32(self) -> f32 { self as f32 } #[inline(always)] fn from_f32(x: f32) -> Self { x as Self } }
impl      AsF32 for u64 { #[inline(always)] fn to_f32(self) -> f32 { self as f32 } #[inline(always)] fn from_f32(x: f32) -> Self { x as Self } }
impl      AsF32 for i8  { #[inline(always)] fn to_f32(self) -> f32 { self as f32 } #[inline(always)] fn from_f32(x: f32) -> Self { x as Self } }
impl      AsF32 for i16 { #[inline(always)] fn to_f32(self) -> f32 { self as f32 } #[inline(always)] fn from_f32(x: f32) -> Self { x as Self } }
impl      AsF32 for i32 { #[inline(always)] fn to_f32(self) -> f32 { self as f32 } #[inline(always)] fn from_f32(x: f32) -> Self { x as Self } }
impl      AsF32 for i64 { #[inline(always)] fn to_f32(self) -> f32 { self as f32 } #[inline(always)] fn from_f32(x: f32) -> Self { x as Self } }
impl      AsF32 for f32 { #[inline(always)] fn to_f32(self) -> f32 { self as f32 } #[inline(always)] fn from_f32(x: f32) -> Self { x as Self } }
impl      AsF32 for f64 { #[inline(always)] fn to_f32(self) -> f32 { self as f32 } #[inline(always)] fn from_f32(x: f32) -> Self { x as Self } }

pub trait                            Mul                  { #[inline(always)] fn mul(self, f: f32) -> Self; }
impl<T: AsF32>                       Mul for T            { #[inline(always)] fn mul(self, f: f32) -> Self { T::from_f32(self.to_f32() * f) } }
impl<A: Mul, B: Mul>                 Mul for (A, B)       { #[inline(always)] fn mul(self, f: f32) -> Self { (self.0.mul(f), self.1.mul(f)) } }
impl<A: Mul, B: Mul, C: Mul>         Mul for (A, B, C)    { #[inline(always)] fn mul(self, f: f32) -> Self { (self.0.mul(f), self.1.mul(f), self.2.mul(f)) } }
impl<A: Mul, B: Mul, C: Mul, D: Mul> Mul for (A, B, C, D) { #[inline(always)] fn mul(self, f: f32) -> Self { (self.0.mul(f), self.1.mul(f), self.2.mul(f), self.3.mul(f)) } }

#[derive(Debug, Clone, PartialEq, Default)]
pub struct TextboxState {
    pub text_buf: Vec<char>,
    pub selection: (usize, usize),
    pub bbox: (isize, isize, isize, isize),
    pub h: f32,
    pub font: FontKind,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ScrollContainerState {
    pub scroll: f32,
    pub content_height: f32,
    pub viewport_height: f32,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct OpenableState {
    pub open: bool,
}

fn display_str_with_edge_bytes(chunks: &[u64; 4], edge_bytes: usize) -> String {
    let mut bytes = {
        let mut out = [0u8; 32];
        let mut i = 0;
        for &chunk in chunks {
            let le = chunk.to_le_bytes(); // linear memory bytes for a LE u64
            out[i..i + 8].copy_from_slice(&le);
            i += 8;
        }
        out
    };

    let mut str = String::new();
    bytes.reverse();

    let edge = edge_bytes.min(bytes.len() / 2).max(1);
    for b in &bytes[0..edge] {
        str.push_str(&format!("{:02x}", b));
    }
    str.push_str("..");
    for b in &bytes[bytes.len() - edge..] {
        str.push_str(&format!("{:02x}", b));
    }

    str
}

fn display_str(chunks: &[u64; 4]) -> String {
    display_str_with_edge_bytes(chunks, 4)
}

fn format_stake_amount(stake_amount: i64) -> String {
    let full = stake_amount / 100_000_000;
    let part = stake_amount % 100_000_000;
    let part_str = format!("{part}00");
    let trim_part = part_str.trim_end_matches("0");
    format!("{}.{} cTAZ", full, &part_str[..trim_part.len().max(3)])
}

fn chunkify(bytes: &[u8; 32]) -> [u64; 4] {
    let mut chunks = [0u64; 4];
    for i in 0..4 {
        let start = i * 8;
        let end = start + 8;
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&bytes[start..end]);
        chunks[i] = u64::from_le_bytes(buf);
    }

    chunks
}

fn modal_action_button(
    ui: &mut Context,
    padding: (f32, f32, f32, f32),
    child_gap: f32,
    label: &str,
    act_on_press: bool,
    enabled: bool,
) -> bool {
    let id = id(label);
    let (clicked, colour, text_colour) = ui.button_ex(act_on_press, BUTTON_GREY, id, enabled, winit::window::CursorIcon::Default);
    if let _ = elem().decl(Decl {
        id,
        child_gap,
        align: Center,
        direction: TopToBottom,
        width: fit!(),
        height: fit!(),
        ..Decl
    }) {
        let radius = ui.scale(24.0);
        if let _ = elem().decl(Decl {
            colour,
            padding,
            child_gap,
            radius: radius.dup4(),
            align: Center,
            width: fit!(ui.scale(192.0)),
            height: fit!(radius * 2.0),
            ..Decl
        }) {
            let h = ui.scale(20.0);
            ui.text(label, TextDecl { h, colour: text_colour, align: AlignX::Center, ..TextDecl });
        }
    }
    clicked
}

fn modal_clickable_icon(
    ui: &mut Context,
    child_gap: f32,
    id: Id,
    icon: &str,
    icon_hovered: &str,
    enabled: bool,
) -> bool {
    let (clicked, colour, _) = ui.button_ex(true, (0xcc, 0xcc, 0xcc, 0xff), id, enabled, winit::window::CursorIcon::Pointer);
    let icon = if ui.hovered(id) { icon_hovered } else { icon };
    if let _ = elem().decl(Decl {
        id,
        child_gap,
        align: Center,
        direction: TopToBottom,
        width: fit!(),
        height: fit!(),
        ..Decl
    }) {
        ui.text(icon, TextDecl { font: Icons, colour, h: ui.scale(24.0), align: AlignX::Center, ..TextDecl });
    }
    clicked
}

fn scroll_container_begin(
    ui: &mut Context,
    data: &mut UiData,
    padding: (f32, f32, f32, f32),
    top_pad: f32,
    id: Id,
    clip: ClipMode,
    scroll: f32,
    content_h: f32,
    viewport_h: f32,
    max: f32,
) {
    ui.scroll_container_bgn(data, padding, top_pad, id, clip, scroll, content_h, viewport_h, max);
}

fn scroll_container_finish(
    ui: &mut Context,
    data: &mut UiData,
    padding: (f32, f32, f32, f32),
    id: Id,
    clip: ClipMode,
    scroll: f32,
    content_h: f32,
    viewport_h: f32,
    max: f32,
) {
    ui.scroll_container_end(data, padding, id, clip, scroll, content_h, viewport_h, max);
}


fn colour_from_hash(hash: &[u8; 32], is_online:bool) -> (u8, u8, u8, u8) {
    // let (mut h, mut s, mut v) = (0u8, 0u8, 0u8);
    // for i in 0..32 {
    //     h = ((h ^ hash[i]) as u32).wrapping_mul(hash[i] as u32).wrapping_mul(17*i as u32).wrapping_mul(402653189) as u8;
    //     s = ((s ^ hash[i]) as u32).wrapping_mul(hash[i] as u32).wrapping_mul(37*i as u32).wrapping_mul(805306457) as u8;
    //     v = ((v ^ hash[i]) as u32).wrapping_mul(hash[i] as u32).wrapping_mul(77*i as u32).wrapping_mul(1610612741) as u8;
    // }

    let mut hasher = std::hash::DefaultHasher::new();
    hasher.write(hash);
    let val = hasher.finish();

    let h = (val) as u8;
    let mut s = 60;
    let mut v = 80;
    if is_online {
        s = 120;
        v = 170;
    }
    // TODO(Giovanni): flashing effect

    (h, s, v, 0xff).rgba()
}

fn get_finalizer_status(bft_status: &wallet::TFLRecencyStatus, pub_key: [u8;32]) -> Option<&FinalizerRecencyStatus> {
    bft_status.finalizer_statuses.iter().find(|(pk,_)| pk.0 == pub_key).map(|st| &st.1)
}

fn finalizer_is_online_ex(f: &FinalizerRecencyStatus, bft_status: &wallet::TFLRecencyStatus, filters: &FinalizerFilters) -> bool {
    let mut ok:bool = true;
    if filters.filter_height {
        let any_vote = f.voted_in_my_height_round.0 | f.voted_in_my_height_round.1;
        ok &= any_vote;
    }

    if filters.filter_seconds_since_connected != 0{
        if bft_status.now_utc < f.last_direct_connection_utc {
            println!("Saw direct connection in future: now: {}; seen: {}", bft_status.now_utc, f.last_direct_connection_utc);
            return false;
        }

        let secs_since_direct_connection = bft_status.now_utc - f.last_direct_connection_utc;
        ok &= secs_since_direct_connection <= filters.filter_seconds_since_connected as i64;
    }
    ok
}

fn finalizer_is_online(pub_key: [u8;32], bft_status: &wallet::TFLRecencyStatus, filters: &FinalizerFilters) -> bool {
    let mut ok:bool = true;
    let Some(f) = get_finalizer_status(bft_status, pub_key) else {
        println!("Didn't find finalizer in list: {}", wallet::bft::PubKeyID(pub_key));
        return false;
    };

    finalizer_is_online_ex(&f, bft_status, filters)
}

pub fn finalizer_ratio_bar(ui: &mut Context, data: &mut UiData, bft_status: &wallet::TFLRecencyStatus, finalizers: &[WalletRosterMember], total_val: u64, height:u32, seconds_since_connected:u64, filters:&FinalizerFilters, id:ui::Id, is_real_finalizers: bool) {

    let finalizer_pill_rad = ui.scale(8.0);
    if finalizers.len() > 0 && total_val > 0 && let _ = elem().decl(Decl {
        id,
        child_gap: ui.scale(5.0),
        align: Left,
        direction: LeftToRight,
        width: percent!(1.0),
        height: fixed!(finalizer_pill_rad * 4.0),
        ..Decl
    }) {
        let mut rem = 0; // TODO: online_rem & offline_rem (or show %mixed)
        let mut done_once = false; // need for the left pill rad
        let mut seen_voting_power = 0;
        for (i, finalizer) in finalizers.iter().enumerate() {
            seen_voting_power += finalizer.voting_power;
            let pct = finalizer.voting_power as f32 / total_val as f32;

            if pct <= 0.01 {
                rem += finalizer.voting_power;
                continue;
            }
            let is_online = if is_real_finalizers {
                finalizer_is_online(finalizer.pub_key, bft_status, filters)
            } else {
                i == 0
            };


            if let el = elem().decl(Decl {
                id: id_index("finalizer bar", i as u32),
                colour: if is_real_finalizers {
                    colour_from_hash(&finalizer.pub_key, is_online)
                } else {
                    // ALT: (0xff, 0xaf, 0x0e, 0xff)
                    [(160, 64, 128, 0xff), (5, 64, 32, 0xff)][i].rgba() // [online, offline]
                },

                radius: {
                    let l = if !done_once { finalizer_pill_rad } else { 0.0 };
                    let r = if seen_voting_power == total_val { finalizer_pill_rad } else { 0.0 };
                    (r, l, r, l) // TODO: order??
                },
                align: Left,
                width:  Sizing::Percent(pct),
                height: grow!(),
                ..Decl
            }) {
                if ui.hovered(el.decl.id)  {
                    let online_string = if is_online {"ONLINE"} else {"OFFLINE"};
                    if is_real_finalizers{
                        set_tooltip_text!(data, "{}  [{online_string}]  {} cTAZ    {:.2}%", display_str(&chunkify(&finalizer.pub_key)), str_from_ctaz(finalizer.voting_power), 100.0*pct);
                    } else {
                        set_tooltip_text!(data, "[{online_string}]  {} cTAZ    {:.2}%", str_from_ctaz(finalizer.voting_power), 100.0*pct);
                    }
                }

            }

            done_once = true;
        }

        if rem != 0 {
            let pct = rem as f32 / total_val as f32;
            if let el = elem().decl(Decl {
                id: id_index("final bar", !0u32),
                colour: (0x40, 0x40, 0x40, 0xff),
                radius: {
                    let l = if rem == total_val { finalizer_pill_rad } else { 0.0 };
                    let r = finalizer_pill_rad;
                    (r, l, r, l) // TODO: order??
                },
                align: Left,
                width:  Sizing::Percent(pct),
                height: grow!(),
                ..Decl
            }) {
                if ui.hovered(el.decl.id) {
                    set_tooltip_text!(data, "(Other)    {} cTAZ    {:.2}%", str_from_ctaz(rem), 100.0*pct);
                }
            }
        }
    }
}

pub fn ui_left_pane(ui: &mut Context,
                wallet_state: Arc<Mutex<WalletState>>,
                data: &mut UiData,
                viz: &mut VizState,
                child_gap: f32,
                padding: (f32, f32, f32, f32),
                radius:  (f32, f32, f32, f32),
                tab_id: &mut Id) {
    let bft_status = &viz.bft_recency_status;

    let mut tab_id_user_wallet  = Id::default();
    let mut tab_id_miner_wallet = Id::default();

    // @Hack.
    ui.nav_skip = (ui.modal != Modal::None);
    if let _ = elem().decl(Decl {
        id: id("Tab Bar"),
        child_gap,
        width: percent!(1.0),
        height: fit!(),
        align: Center,
        ..Decl
    }) {
        tab_id_user_wallet  = ui.tab((radius.0, 0.0, radius.2, radius.3), padding, tab_id, "Your Wallet");
        tab_id_miner_wallet = ui.tab(radius, padding, tab_id, "Faucet Wallet");
    }
    ui.nav_skip = false;

    let shift_held = (ui.input().key_held(KeyCode::ShiftLeft)   || ui.input().key_held(KeyCode::ShiftRight));
    let ctrl_held  = (ui.input().key_held(KeyCode::ControlLeft) || ui.input().key_held(KeyCode::ControlRight));

    // CTRL-TAB to switch between user and miner wallet.
    if ui.input().key_pressed(KeyCode::Tab) && ctrl_held {
        if *tab_id == tab_id_miner_wallet {
            *tab_id = tab_id_user_wallet;
        } else {
            *tab_id = tab_id_miner_wallet;
        }
    }

    let (wallet_is_init, filters) = {
        let state = wallet_state.lock().unwrap();
        (state.wallet_is_init, state.filters)
    };

    // You can't operate on the miner, so you can't open any modals. // @Todo: maybe you can open Receive?
    if *tab_id == tab_id_miner_wallet {
        ui.modal = Modal::None;
    }
    const deemph_mul: f32 = 0.6;
    let grey: (u8, u8, u8, u8) = WHITE.mul(deemph_mul);

    let is_staking_day = viz.bc_tip_height % UI_COPY_STAKING_DAY_PERIOD < UI_COPY_STAKING_DAY_WINDOW;

    // USER STAKING POSITIONS
    // TODO: phillip audaciously deleted the accumulated bond amount field, bring that back
    let mut staked_roster_unbonded: Vec<([u8; 32] /* bond key */, [u8; 32] /* target finalizer */, u64 /* current estimated */)>;
    let mut staked_roster_bonded: Vec<([u8; 32] /* bond key */, [u8; 32] /* target finalizer */, u64 /* current estimated */)>;
    let mut bonded_finalizers = Vec::<WalletRosterMember>::new();
    let (mut total_bonded, mut total_unbonded) = (0, 0);
    {
        {
            let lock = wallet_state.lock().unwrap();
            staked_roster_unbonded = lock.stake_positions_unbonded.clone();
            staked_roster_bonded = lock.stake_positions_bonded.clone();
        }

        staked_roster_unbonded.sort_by_key(|x| std::cmp::Reverse((x.1, x.2)));
        staked_roster_bonded.sort_by_key(|x| std::cmp::Reverse((x.1, x.2)));

        for position in &staked_roster_unbonded {
            total_unbonded += position.2;
        }

        let mut prev_finalizer: Option<[u8; 32]> = None;
        for i in 0..staked_roster_bonded.len() {
            let &(bond_key, finalizer, position) = &staked_roster_bonded[i];

            if Some(finalizer) == prev_finalizer {
                bonded_finalizers.last_mut().unwrap().voting_power += position;
            } else {
                prev_finalizer = Some(finalizer);
                bonded_finalizers.push(WalletRosterMember {
                    pub_key: finalizer,
                    voting_power: position,
                    txids: Vec::new(),
                });
            }

            total_bonded += position;
        }
    }
    let modal_container_direction = if ui.modal == Modal::Jump { TopToBottom } else { LeftToRight };
    let modal_container_floating = if ui.modal == Modal::Jump {
        Floating::Root(0.0, 0.0)
    } else {
        Floating::Parent
    };
    let modal_container_width = if ui.modal == Modal::Jump {
        fixed!(ui.draw().window_width as f32)
    } else {
        grow!()
    };
    let modal_container_height = if ui.modal == Modal::Jump {
        fixed!(ui.draw().window_height as f32)
    } else {
        grow!()
    };
    if ui.modal != Modal::None && let _elem = elem().decl(Decl {
        child_gap,
        id: id("Modal Container"),
        padding: padding.mul(2.0),
        radius: (radius.0, 0.0, radius.2, 0.0),
        floating: modal_container_floating,
        colour: (0, 0, 0, 0xC0),
        direction: modal_container_direction,
        align: Center,
        width:  modal_container_width,
        height: modal_container_height,
        ..Decl
    }) {
        let container_id = _elem.decl.id;
        let container_hovered = ui.hovered(container_id);

        if container_hovered { ui.capture = true; }

        let mut height = grow!(ui.scale(192.0), ui.scale(384.0));
        if ui.modal == Modal::Stake || ui.modal == Modal::Send || ui.modal == Modal::Jump {
            height = fit!();
        }
        if ui.modal == Modal::Unstake {
            height = percent!(0.9);
        }

        let modal_align = if ui.modal == Modal::Jump { Center } else { Top };
        let modal_floating = if ui.modal == Modal::Jump { Floating::Parent } else { Floating::None };
        let modal_width = if ui.modal == Modal::Jump {
            fit!(ui.scale(520.0))
        } else {
            grow!(ui.scale(192.0)/* , ui.scale(384.0) */)
        };
        if let _elem = elem().decl(Decl {
            child_gap, radius,
            id: id("Modal Contents"),
            padding: padding.mul(2.0),
            colour: MODAL_COL,
            width:  modal_width,
            height,
            floating: modal_floating,
            align: modal_align,
            direction: TopToBottom,
            ..Decl
        }) {

            let balance = {
                let state = wallet_state.lock().unwrap();
                if *tab_id == tab_id_user_wallet { state.user_balance() } else { state.miner_balance() }
            };

            let contents_id = _elem.decl.id;
            let contents_hovered = ui.hovered(contents_id);

            if container_hovered { ui.capture = true; }

            let text_h = ui.scale(24.0);
            let title_bar = |ui: &mut Context, closeable, title, title_bar_id| {
                if let _ = elem().decl(Decl {
                    id: title_bar_id,
                    child_gap,
                    width:  grow!(),
                    height: fit!(),
                    align: Center,
                    direction: LeftToRight,
                    ..Decl
                }) {
                    if let _ = elem().decl(Decl { width: grow!(), align: Left, ..Decl }) {}
                    if let _ = elem().decl(Decl { width: grow!(), align: Center, ..Decl }) {
                        ui.text(title, TextDecl { h: text_h, align: AlignX::Center, ..TextDecl });
                    }
                    if let _ = elem().decl(Decl { id: id("Title Bar Right Side"), width: grow!(), align: Right, ..Decl }) && closeable {
                        let id = id("Close This Modal");

                        let (clicked, colour, _) = ui.button_ex(false, BUTTON_GREY, id, true, winit::window::CursorIcon::Default);
                        if clicked || (ui.input().key_pressed(KeyCode::Escape) && !ui.nav_enable) {
                            ui.modal = Modal::None;
                        }

                        // Click background to exit -- the code could be placed farther outside but it is here so it can be gated by `closeable`
                        if ui.hovered(container_id) && !ui.hovered(contents_id) && ui.input().mouse_pressed(winit::event::MouseButton::Left) {
                            ui.modal = Modal::None;
                            ui.mouse_pressed_id = id;
                        }

                        let radius = ui.scale(20.0);

                        // Button circle
                        if let _ = elem().decl(Decl {
                            id, colour, radius: radius.dup4(), padding, child_gap, align: Center,
                            width:  fixed!(radius * 2.0),
                            height: fixed!(radius * 2.0),
                            ..Decl
                        }) {
                            let temp_letter_symbol_h = ui.scale(24.0);
                            ui.text(ICON_CANCEL, TextDecl { font: Icons, h: temp_letter_symbol_h, align: AlignX::Center, ..TextDecl });
                        }
                    }
                }
            };

            let button_ex = |ui: &mut Context, id, label, enabled: bool| {
                let colour = {
                    let mut hsva = BUTTON_GREY.hsva();
                    hsva.2 = ((hsva.2 as f32) * 1.25).min(255.0) as u8;
                    hsva.rgba()
                };
                let (clicked, colour, text_colour) = ui.button_ex(false, colour, id, enabled, winit::window::CursorIcon::Default);
                let radius = ui.scale(24.0);
                if let _ = elem().decl(Decl {
                    id,
                    colour,
                    child_gap,
                    radius: radius.dup4(),
                    align: Align::Center,
                    direction: TopToBottom,
                    width:  fit!(ui.scale(192.0)),
                    height: fit!(radius * 2.0),
                    ..Decl
                }) {
                    let h = ui.scale(20.0);
                    ui.text(label, TextDecl { h, colour: text_colour, align: AlignX::Center, ..TextDecl });
                }

                clicked
            };
            let button = |ui: &mut Context, label, enabled: bool| {
                let id = id(label);
                button_ex(ui, id, label, enabled)
            };

            match ui.modal {
                Modal::None => {}
                Modal::Jump => {
                    title_bar(ui, true, "Jump To Height", id("Jump To Height Title Bar"));

                    if let _ = elem().decl(Decl {
                        child_gap,
                        width: grow!(),
                        height: fit!(),
                        align: Center,
                        direction: TopToBottom,
                        ..Decl
                    }) {
                        if let _ = elem().decl(Decl {
                            direction: LeftToRight,
                            child_gap: ui.scale(8.0),
                            width: fit!(),
                            height: fit!(),
                            align: Center,
                            ..Decl
                        }) {
                            let pow_id = id("Jump To Pow");
                            let (pow_clicked, pow_col, pow_txt) = ui.button_ex(true, BUTTON_GREY.mul(if !data.jump_target_pos { 1.0 } else { 0.8 }), pow_id, true, winit::window::CursorIcon::Pointer);
                            if let _ = elem().decl(Decl {
                                id: pow_id,
                                colour: pow_col,
                                radius: ui.scale(14.0).dup4(),
                                padding: (ui.scale(8.0), ui.scale(14.0), ui.scale(8.0), ui.scale(14.0)),
                                width: fit!(),
                                height: fit!(),
                                ..Decl
                            }) {
                                ui.text("PoW", TextDecl { h: ui.scale(16.0), colour: pow_txt, align: AlignX::Center, ..TextDecl });
                            }

                            let pos_id = id("Jump To Pos");
                            let (pos_clicked, pos_col, pos_txt) = ui.button_ex(true, BUTTON_GREY.mul(if data.jump_target_pos { 1.0 } else { 0.8 }), pos_id, true, winit::window::CursorIcon::Pointer);
                            if let _ = elem().decl(Decl {
                                id: pos_id,
                                colour: pos_col,
                                radius: ui.scale(14.0).dup4(),
                                padding: (ui.scale(8.0), ui.scale(14.0), ui.scale(8.0), ui.scale(14.0)),
                                width: fit!(),
                                height: fit!(),
                                ..Decl
                            }) {
                                ui.text("PoS", TextDecl { h: ui.scale(16.0), colour: pos_txt, align: AlignX::Center, ..TextDecl });
                            }

                            if pow_clicked {
                                data.jump_target_pos = false;
                            }
                            if pos_clicked {
                                data.jump_target_pos = true;
                            }
                        }

                        let jump_input_id = id("Goto Height Input");
                        let jump_text = ui.textbox(
                            data,
                            jump_input_id,
                            if data.jump_target_pos { "Enter PoS height..." } else { "Enter PoW height..." },
                            TextDecl { font: Mono, h: ui.scale(14.0), colour: WHITE, align: AlignX::Left, ..TextDecl },
                        );

                        let can_jump = jump_text.trim().len() > 0;
                        if button_ex(ui, id("Jump To Selected Height"), "Jump", can_jump) || (ui.input().key_pressed(KeyCode::Enter) && ui.nav_id == jump_input_id.id) {
                            if let Ok(h) = jump_text.trim().parse::<u64>() {
                                if data.jump_target_pos {
                                    let _ = viz.goto_pos_height(h);
                                } else {
                                    viz.goto_pow_height(h);
                                }
                                ui.modal = Modal::None;
                            }
                        }
                    }
                }
                Modal::Send => {
                    title_bar(ui, true, "Send",    id("Send Title Bar"));

                    if let _ = elem().decl(Decl {
                        child_gap, radius,
                        id: id("Send Container"),
                        colour: MODAL_COL,
                        width:  grow!(),
                        height: grow!(),
                        align: Center,
                        direction: TopToBottom,
                        ..Decl
                    }) {
                        // spacer
                        if let _ = elem().decl(Decl { width: grow!(), height: fixed!(ui.scale(4.0)), ..Default::default() }) {}

                        let send_address_id = id("Send Address Textbox");
                        if data.send_address.len() != 0 {
                            let send_address_buf: Vec<char> = data.send_address.chars().collect();
                            let textbox_state = data.textboxes.entry(send_address_id.id).or_default();
                            if textbox_state.text_buf != send_address_buf {
                                textbox_state.text_buf = send_address_buf;
                                let len = textbox_state.text_buf.len();
                                textbox_state.selection.0 = textbox_state.selection.0.min(len);
                                textbox_state.selection.1 = textbox_state.selection.1.min(len);
                            }
                        }
                        if let _ = elem().decl(Decl {
                            width: grow!(),
                            height: fit!(),
                            ..Decl
                        }) {
                            data.send_address = ui.textbox(
                                data,
                                send_address_id,
                                "Enter recipient address...",
                                TextDecl { font: Mono, h: ui.scale(14.0), colour: WHITE, align: AlignX::Left, ..TextDecl },
                            ).trim().to_string();
                        }
                        if button(ui, "Paste Address", true) {
                            data.send_address = ui.input().get_from_clipboard().trim().to_string();
                            let send_address_buf: Vec<char> = data.send_address.chars().collect();
                            let textbox_state = data.textboxes.entry(send_address_id.id).or_default();
                            textbox_state.text_buf = send_address_buf;
                            let len = textbox_state.text_buf.len();
                            textbox_state.selection.0 = len;
                            textbox_state.selection.1 = len;
                        }

                        // spacer
                        if let _ = elem().decl(Decl { width: grow!(), height: fixed!(ui.scale(16.0)), ..Default::default() }) {}

                        if (balance as u64) < ONE_cTAZ / 100 {
                            let colour = (0xff, 0xaf, 0x0e, 0xff);
                            ui.text("Insufficient funds. Try the faucet!", TextDecl { h: ui.scale(20.0), colour, align: AlignX::Center, ..TextDecl });
                        }

                        if let _ = elem().decl(Decl {
                            child_gap, radius,
                            id: id("Send Buttons"),
                            colour: MODAL_COL,
                            width:  grow!(),
                            height: grow!(),
                            align: Center,
                            direction: TopToBottom,
                            ..Decl
                        }) {
                            let (
                                balance,
                                waiting_for_send
                            ) = {
                                let wallet_state = wallet_state.lock().unwrap();
                                (
                                    wallet_state.user_balance(),
                                    wallet_state.waiting_for_send,
                                )
                            };

                            let can = !waiting_for_send && data.send_address.len() != 0;
                            let sends = [
                                ("0.1", ONE_cTAZ/10),
                                // ("0.2", 2*(ONE_cTAZ/10)),
                                ("0.6", 6*(ONE_cTAZ/10)),
                                ("1",   ONE_cTAZ),
                                // ("2",   2*ONE_cTAZ),
                                ("5",   5*ONE_cTAZ),
                                // ("10",  10*ONE_cTAZ),
                                ("31",  31*ONE_cTAZ),
                                // ("75",  75*ONE_cTAZ),
                                ("141", 141*ONE_cTAZ),
                                // ("250", 250*ONE_cTAZ),
                            ];
                            if data.send_amount_index >= sends.len() {
                                data.send_amount_index = 0;
                            }
                            if let _ = elem().decl(Decl {
                                direction: LeftToRight,
                                align: Center,
                                child_gap,
                                width: fit!(),
                                height: fit!(),
                                ..Decl
                            }) {
                                if button_ex(ui, id("Send Amount Down"), "-", data.send_amount_index > 0) {
                                    data.send_amount_index = data.send_amount_index.saturating_sub(1);
                                }
                                if let _ = elem().decl(Decl {
                                    colour: BUTTON_GREY,
                                    radius: ui.scale(18.0).dup4(),
                                    padding: (ui.scale(8.0), ui.scale(18.0), ui.scale(8.0), ui.scale(18.0)),
                                    width: fit!(),
                                    height: fit!(),
                                    align: Center,
                                    ..Decl
                                }) {
                                    ui.text(frame_strf!(data, "Amount: {}", sends[data.send_amount_index].0), TextDecl { font: Mono, h: ui.scale(20.0), colour: WHITE, align: AlignX::Center, ..TextDecl });
                                }
                                if button_ex(ui, id("Send Amount Up"), "+", data.send_amount_index + 1 < sends.len()) {
                                    data.send_amount_index += 1;
                                }
                            }
                            let selected = sends[data.send_amount_index];
                            if button(
                                ui,
                                frame_strf!(data, "Send {}", selected.0),
                                can && (balance as u64) >= selected.1,
                            ) {
                                wallet_state.lock().unwrap().send_to_address(data.send_address.clone(), selected.1);
                            }
                        }
                    }
                }

                Modal::Receive => {
                    title_bar(ui, true, "Receive", id("Receive Title Bar"));

                    if let _ = elem().decl(Decl {
                        child_gap, radius,
                        id: id("Receive Container"),
                        colour: MODAL_COL,
                        width:  grow!(),
                        height: grow!(),
                        align: Center,
                        direction: TopToBottom,
                        ..Decl
                    }) {
                        let ua = &wallet_state.lock().unwrap().user_recv_ua;
                        if ua.len() != 0 {
                            let recv_address_id = id("Receive Address Textbox");
                            let recv_address_buf: Vec<char> = ua.chars().collect();
                            {
                                let textbox_state = data.textboxes.entry(recv_address_id.id).or_default();
                                textbox_state.text_buf = recv_address_buf.clone();
                                let len = textbox_state.text_buf.len();
                                textbox_state.selection.0 = textbox_state.selection.0.min(len);
                                textbox_state.selection.1 = textbox_state.selection.1.min(len);
                            }
                            if let _ = elem().decl(Decl {
                                width: grow!(),
                                height: fit!(),
                                ..Decl
                            }) {
                                let _ = ui.textbox(
                                    data,
                                    recv_address_id,
                                    "Loading...",
                                    TextDecl { font: Mono, h: ui.scale(14.0), colour: WHITE, align: AlignX::Left, ..TextDecl },
                                );
                            }
                            {
                                let textbox_state = data.textboxes.entry(recv_address_id.id).or_default();
                                textbox_state.text_buf = recv_address_buf;
                                let len = textbox_state.text_buf.len();
                                textbox_state.selection.0 = textbox_state.selection.0.min(len);
                                textbox_state.selection.1 = textbox_state.selection.1.min(len);
                            }

                            if button(ui, "Copy Address", true) {
                                ui.input().send_to_clipboard(&ua);
                            }
                        } else {
                            ui.text("Loading...", TextDecl { font: Mono, h: ui.scale(20.0), colour: WHITE, align: AlignX::Center, ..TextDecl });
                        }
                    }
                }

                Modal::Stake => {
                    title_bar(ui, true, "Stake", id("Stake Title Bar"));

                    let stake_address_id = id("Stake Address Textbox");
                    if data.stake_address.len() != 0 {
                        let stake_address_buf: Vec<char> = data.stake_address.chars().collect();
                        let textbox_state = data.textboxes.entry(stake_address_id.id).or_default();
                        if textbox_state.text_buf != stake_address_buf {
                            textbox_state.text_buf = stake_address_buf;
                            let len = textbox_state.text_buf.len();
                            textbox_state.selection.0 = textbox_state.selection.0.min(len);
                            textbox_state.selection.1 = textbox_state.selection.1.min(len);
                        }
                    }
                    if let _ = elem().decl(Decl {
                        width: grow!(),
                        height: fit!(),
                        ..Decl
                    }) {
                        data.stake_address = ui.textbox(
                            data,
                            stake_address_id,
                            "Enter validator identity...",
                            TextDecl { font: Mono, h: ui.scale(14.0), colour: WHITE, align: AlignX::Left, ..TextDecl },
                        ).trim().to_string();
                    }
                    if button(ui, "Paste Identity", true) {
                        data.stake_address = ui.input().get_from_clipboard().trim().to_string();
                        let stake_address_buf: Vec<char> = data.stake_address.chars().collect();
                        let textbox_state = data.textboxes.entry(stake_address_id.id).or_default();
                        textbox_state.text_buf = stake_address_buf;
                        let len = textbox_state.text_buf.len();
                        textbox_state.selection.0 = len;
                        textbox_state.selection.1 = len;
                    }

                    // spacer
                    if let _ = elem().decl(Decl { width: grow!(), height: fixed!(ui.scale(16.0)), ..Default::default() }) {}

                    if (balance as u64) < ONE_cTAZ / 100 {
                        let colour = (0xff, 0xaf, 0x0e, 0xff);
                        ui.text("Insufficient funds. Try the faucet!", TextDecl { h: ui.scale(20.0), colour, align: AlignX::Center, ..TextDecl });
                    }

                    let waiting_for_stake_to_finalizer = wallet_state.lock().unwrap().waiting_for_stake_to_finalizer;

                    {
                        // if (balance as u64) < ONE_cTAZ / 100 {
                        //     let colour = (0xff, 0xaf, 0x0e, 0xff);
                        //     ui.text("Insufficient funds. Try the faucet!", TextDecl { h: ui.scale(20.0), colour, align: AlignX::Center, ..TextDecl });
                        // }

                        pub fn addr_from_str_bytes(data: &[u8]) -> Option<[u8; 32]> {
                            const VALS: [u8; 256] = {
                                let mut v = [0xff; 256];
                                v[b'0' as usize] = 0x0;
                                v[b'1' as usize] = 0x1;
                                v[b'2' as usize] = 0x2;
                                v[b'3' as usize] = 0x3;
                                v[b'4' as usize] = 0x4;
                                v[b'5' as usize] = 0x5;
                                v[b'6' as usize] = 0x6;
                                v[b'7' as usize] = 0x7;
                                v[b'8' as usize] = 0x8;
                                v[b'9' as usize] = 0x9;
                                v[b'a' as usize] = 0xa;
                                v[b'b' as usize] = 0xb;
                                v[b'c' as usize] = 0xc;
                                v[b'd' as usize] = 0xd;
                                v[b'e' as usize] = 0xe;
                                v[b'f' as usize] = 0xf;
                                v
                            };
                            let mut buf = [0u8; 32];
                            for i in 0..32 {
                                let a = data.get(2*i)?;
                                let b = data.get(2*i + 1)?;
                                let a = VALS[*a as usize];
                                if a == 0xff {
                                    return None;
                                }
                                let b = VALS[*b as usize];
                                if b == 0xff {
                                    return None;
                                }
                                buf[31-i] = (a << 4) | b
                            }
                            Some(buf)
                        }
                        let hex_dest = addr_from_str_bytes(data.stake_address.as_bytes());

                        let can = is_staking_day && !waiting_for_stake_to_finalizer && hex_dest.is_some();
                        let stakes = [
                            ("0.01", ONE_cTAZ / 100),
                            ("0.1", ONE_cTAZ / 10),
                            ("1", ONE_cTAZ),
                            ("10", ONE_cTAZ * 10),
                            ("100", ONE_cTAZ * 100),
                            ("1000", ONE_cTAZ * 1000),
                            ("10000", ONE_cTAZ * 10000),
                        ];
                        if data.stake_amount_index >= stakes.len() {
                            data.stake_amount_index = 0;
                        }

                        let mut hover = false;
                        if let _ = elem().decl(Decl {
                            direction: LeftToRight,
                            align: Center,
                            child_gap,
                            width: fit!(),
                            height: fit!(),
                            ..Decl
                        }) {
                            let down_id = id("Stake Amount Down");
                            if button_ex(ui, down_id, "-", data.stake_amount_index > 0) {
                                data.stake_amount_index = data.stake_amount_index.saturating_sub(1);
                            }
                            hover |= ui.hovered(down_id);
                            if let _ = elem().decl(Decl {
                                colour: BUTTON_GREY,
                                radius: ui.scale(18.0).dup4(),
                                padding: (ui.scale(8.0), ui.scale(18.0), ui.scale(8.0), ui.scale(18.0)),
                                width: fit!(),
                                height: fit!(),
                                align: Center,
                                ..Decl
                            }) {
                                ui.text(frame_strf!(data, "Amount: {} cTAZ", stakes[data.stake_amount_index].0), TextDecl { font: Mono, h: ui.scale(20.0), colour: WHITE, align: AlignX::Center, ..TextDecl });
                            }
                            let up_id = id("Stake Amount Up");
                            if button_ex(ui, up_id, "+", data.stake_amount_index + 1 < stakes.len()) {
                                data.stake_amount_index += 1;
                            }
                            hover |= ui.hovered(up_id);
                        }
                        let selected = stakes[data.stake_amount_index];
                        let stake_id = id("Stake Selected Amount");
                        if button_ex(
                            ui,
                            stake_id,
                            frame_strf!(data, "+{} cTAZ", selected.0),
                            can && (balance as u64) >= selected.1,
                        ) {
                            wallet_state.lock().unwrap().stake_to_finalizer(selected.1, hex_dest.unwrap());
                        }
                        hover |= ui.hovered(stake_id);

                        if !can && !is_staking_day && hover {
                            set_tooltip_text!(data, "You can only stake during Staking Day.");
                        }
                    }
                }

                Modal::Retarget => {
                    title_bar(ui, true, "Retarget Delegation Bond", id("Retarget Title Bar"));

                    let mut stake_address = "0000000000000000";
                    if data.stake_address.len() >= 16 {
                        stake_address = &data.stake_address;
                    }

                    ui.text(frame_strf!(data, "[{}..{}]", &stake_address[0..8], &stake_address[stake_address.len()-8..]), TextDecl { font: Mono, h: ui.scale(20.0), colour: WHITE, align: AlignX::Center, ..TextDecl });
                    if button(ui, "Paste Identity", true) {
                        data.stake_address = ui.input().get_from_clipboard().trim().to_string();
                    }

                    // spacer
                    if let _ = elem().decl(Decl { width: grow!(), height: fixed!(ui.scale(16.0)), ..Default::default() }) {}

                    if (balance as u64) < 10_000 {
                        let colour = (0xff, 0xaf, 0x0e, 0xff);
                        ui.text("Insufficient funds. Try the faucet!", TextDecl { h: ui.scale(20.0), colour, align: AlignX::Center, ..TextDecl });
                    }

                    pub fn addr_from_str_bytes(data: &[u8]) -> Option<[u8; 32]> {
                        const VALS: [u8; 256] = {
                            let mut v = [0xff; 256];
                            v[b'0' as usize] = 0x0;
                            v[b'1' as usize] = 0x1;
                            v[b'2' as usize] = 0x2;
                            v[b'3' as usize] = 0x3;
                            v[b'4' as usize] = 0x4;
                            v[b'5' as usize] = 0x5;
                            v[b'6' as usize] = 0x6;
                            v[b'7' as usize] = 0x7;
                            v[b'8' as usize] = 0x8;
                            v[b'9' as usize] = 0x9;
                            v[b'a' as usize] = 0xa;
                            v[b'b' as usize] = 0xb;
                            v[b'c' as usize] = 0xc;
                            v[b'd' as usize] = 0xd;
                            v[b'e' as usize] = 0xe;
                            v[b'f' as usize] = 0xf;
                            v
                        };
                        let mut buf = [0u8; 32];
                        for i in 0..32 {
                            let a = data.get(2*i)?;
                            let b = data.get(2*i + 1)?;
                            let a = VALS[*a as usize];
                            if a == 0xff {
                                return None;
                            }
                            let b = VALS[*b as usize];
                            if b == 0xff {
                                return None;
                            }
                            buf[31-i] = (a << 4) | b
                        }
                        Some(buf)
                    }
                    let hex_dest = addr_from_str_bytes(data.stake_address.as_bytes());

                    let can = hex_dest.is_some();
                    let label = "Retarget";
                    let id = id(label);
                    if button_ex(ui, id, label, can) {
                        wallet_state.lock().unwrap().retarget_bond(ui.retarget_modal_bond_key, hex_dest.unwrap());
                        ui.modal = Modal::None;
                    }
                }

                Modal::Unstake => {
                    title_bar(ui, true, "Edit Stake", id("Edit Stake Title Bar"));

                    let can = is_staking_day;

                    let (id, mut clip, mut scroll, content_h, viewport_h, max) = ui.scroll_container(data, id("Edit Stake Scroll Container"), 48.0);
                    if (staked_roster_unbonded.len() + staked_roster_bonded.len()) == 0 {
                        clip = ClipMode::None;
                        *scroll = 0.0;
                    }
                    let scroll = *scroll;
                    scroll_container_begin(ui, data, padding, 0.0, id, clip, scroll, content_h, viewport_h, max);
                    {
                        if (staked_roster_unbonded.len() + staked_roster_bonded.len()) == 0 {
                            let h = ui.scale(24.0);
                            if let _ = elem().decl(Decl {
                                direction: TopToBottom,
                                width:  percent!(1.0),
                                height: percent!(1.0),
                                child_gap,
                                align:  Center,
                                ..Decl
                            }) {
                                ui.text(ICON_EYE_OFF, TextDecl { font: Icons, colour: WHITE.mul(0.6), h: ui.scale(64.0), align: AlignX::Center, ..TextDecl });
                                ui.text("You have not staked to any finalizers yet.", TextDecl { colour: WHITE.mul(0.6), h, align: AlignX::Center, ..TextDecl });
                            }
                        } else {
                            if staked_roster_unbonded.len() > 0 {
                                let id = id_index("WithdrawableBondsContainer", 0);
                                let colour = BUTTON_GREY.mul(0.8);
                                elem_bgn();
                                decl(Decl {
                                    colour,
                                    radius,
                                    child_gap,
                                    padding,
                                    width:  grow!(),
                                    height: fit!(),
                                    align: TopLeft,
                                    direction: TopToBottom,
                                    ..Decl
                                });

                                let (activated, colour, text_colour) = ui.button_ex(true, colour, id, true, winit::window::CursorIcon::Pointer);

                                if let _ = elem().decl(Decl {
                                    id,
                                    colour,
                                    radius,
                                    padding,
                                    width:  grow!(),
                                    height: fit!(),
                                    align:  Left,
                                    ..Decl
                                })
                                {
                                    ui.text("Withdrawable Bonds", TextDecl { colour: text_colour, h: ui.scale(18.0), align: AlignX::Left, ..TextDecl });
                                    let _ = elem().decl(Decl { width: grow!(), ..Decl });
                                    let colour = (0xff, 0xaf, 0x0e, 0xff); // @todo color
                                    ui.text(frame_strf!(data, "{} cTAZ", str_from_ctaz(total_unbonded)), TextDecl { font: Mono, colour, h: ui.scale(18.0), align: AlignX::Right, ..TextDecl });
                                }

                                if ui.openable(data, id, activated, true) {
                                    for &(bond_key, finalizer, initial) in &staked_roster_unbonded {
                                        let index: u32 = {
                                            use std::hash::{Hash, Hasher};
                                            let mut h = std::collections::hash_map::DefaultHasher::new();
                                            0x890452908345u64.hash(&mut h);
                                            bond_key.hash(&mut h);
                                            h.finish() as u32
                                        };

                                        let stake_amount = initial as i64;
                                        if index > 0 { // separator
                                            let colour = {
                                                let mut col = TRANSACTION_HISTORY_CONTAINER_COL;
                                                col = col.hsva();
                                                col.2 = col.2.mul(1.5).min(255);
                                                col.rgba()
                                            };

                                            let _ = elem().decl(Decl { colour, height: fixed!(ui.scale(2.0)), width: percent!(1.0), ..Decl });
                                        }
                                        if let _ = elem().decl(Decl {
                                            id: id_index("Unstake Roster Member", index as u32),
                                            padding,
                                            child_gap,
                                            height: fixed!(ui.scale(48.0)),
                                            width: percent!(1.0),
                                            direction: LeftToRight,
                                            align: Center,
                                            ..Decl
                                        }) {
                                            // left icon
                                            if let _ = elem().decl(Decl {
                                                id: id_index("Unstake Roster Member Left Icon", index as u32),
                                                height: fit!(),
                                                width: fixed!(ui.scale(32.0)),
                                                direction: TopToBottom,
                                                align: Center,
                                                ..Decl
                                            }) {
                                                let (icon, icon_hovered) = (ICON_MONEY, ICON_WALLET);
                                                let id = id_index("Unstake Button", index as u32);
                                                if modal_clickable_icon(ui, child_gap, id, icon, icon_hovered, can) {
                                                    wallet_state.lock().unwrap().claim_bond(bond_key);
                                                }
                                                if !can && !is_staking_day && ui.hovered(id) {
                                                    set_tooltip_text!(data, "You can only withdraw stake during Staking Day.");
                                                }
                                            }

                                            // info
                                            if let _ = elem().decl(Decl {
                                                id: id_index("Unstake Roster Member Info", index as u32),
                                                height: fit!(),
                                                width: grow!(),
                                                direction: TopToBottom,
                                                align: Left,
                                                ..Decl
                                            }) {
                                                ui.text(frame_strf!(data, "{}", display_str(&chunkify(&bond_key))), TextDecl { font: Mono, h: ui.scale(14.0), align: AlignX::Left, ..TextDecl });
                                            }

                                            // right info
                                            if let _ = elem().decl(Decl {
                                                id: id_index("Unstake Roster Member Amounts", index as u32),
                                                height: fit!(),
                                                width: fit!(),
                                                direction: TopToBottom,
                                                align: Right,
                                                ..Decl
                                            }) {
                                                let id = id_index("Unstake Clickable Text", index as u32);
                                                let mut colour = (0xff, 0xaf, 0x0e, 0xff); // @todo color
                                                let mut str = frame_strf!(data, "{}", format_stake_amount(stake_amount));
                                                if ui.hovered(id) {
                                                    colour = WHITE;
                                                    str = frame_strf!(data, "{}", format_stake_amount(stake_amount));
                                                }

                                                if let _ = elem().decl(Decl {
                                                    id,
                                                    width: fit!(),
                                                    height: fit!(),
                                                    ..Decl
                                                }) {
                                                    ui.text(str, TextDecl { font: Mono, h: ui.scale(14.0), colour, align: AlignX::Right, ..TextDecl });
                                                }
                                            }
                                        }
                                    }
                                }
                                elem_end();
                            }


                            if staked_roster_bonded.len() > 0 {
                                let string = "Staked Bonds";
                                let id = id_index("StakedBondsContainer", 0);
                                let colour = BUTTON_GREY.mul(0.8);
                                elem_bgn();
                                decl(Decl {
                                    colour,
                                    radius,
                                    child_gap: child_gap * 0.5,
                                    padding,
                                    width:  grow!(),
                                    height: fit!(),
                                    align: TopLeft,
                                    direction: TopToBottom,
                                    ..Decl
                                });

                                let (activated, colour, text_colour) = ui.button_ex(true, colour, id, true, winit::window::CursorIcon::Pointer);
                                /*@TODO(Giovanni) FILL THESEEEEEE WTH ACTUAL DATA*/
                                let height = 3000;
                                let seconds_since_connected = 300;
                                /*-------------------------------------*/
                                if let _ = elem().decl(Decl {
                                    id,
                                    colour,
                                    radius,
                                    padding,
                                    width:  grow!(),
                                    height: fit!(),
                                    align:  Left,
                                    ..Decl
                                })
                                {
                                    ui.text(string, TextDecl { colour: text_colour, h: ui.scale(18.0), align: AlignX::Left, ..TextDecl });
                                    let _ = elem().decl(Decl { width: grow!(), ..Decl });
                                    let colour = (0xff, 0xaf, 0x0e, 0xff); // @todo color
                                    ui.text(frame_strf!(data, "{} cTAZ", str_from_ctaz(total_bonded)), TextDecl { font: Mono, colour, h: ui.scale(18.0), align: AlignX::Right, ..TextDecl });
                                }


                                finalizer_ratio_bar(ui, data, bft_status, &bonded_finalizers, total_bonded, height, seconds_since_connected, &filters, ui::id("Left Pane Ratio Bar"), true);


                                if ui.openable(data, id, activated, true) {
                                    let mut prev_finalizer:   Option<[u8; 32]> = None;
                                    let mut open_finalizer:   bool = false;
                                    for (bond_i, &(bond_key, finalizer, initial)) in staked_roster_bonded.iter().enumerate() {
                                        let index: u32 = {
                                            use std::hash::{Hash, Hasher};
                                            let mut h = std::collections::hash_map::DefaultHasher::new();
                                            0x3953467893478u64.hash(&mut h);
                                            bond_key.hash(&mut h);
                                            finalizer.hash(&mut h);
                                            h.finish() as u32
                                        };
                                        if prev_finalizer != Some(finalizer) {
                                            if prev_finalizer.is_some() {
                                                elem_end();
                                            }
                                            prev_finalizer = Some(finalizer);

                                            let finalizer_status = get_finalizer_status(bft_status, finalizer);
                                            let is_online = if let Some(f) = finalizer_status {
                                                finalizer_is_online_ex(&f, &bft_status, &filters)
                                            } else {
                                                false
                                            };

                                            let mut total_bonded_to_finalizer = 0;
                                            for i in bond_i..staked_roster_bonded.len() {
                                                if staked_roster_bonded[i].1 != finalizer {
                                                    break;
                                                }
                                                total_bonded_to_finalizer += staked_roster_bonded[i].2;
                                            }

                                            let label = frame_strf!(data, "{}", display_str(&chunkify(&finalizer)));

                                            let id = id_index(label, index);

                                            let colour = BUTTON_GREY.mul(0.6);

                                            let (activated, colour, text_colour) = ui.button_ex(true, colour, id, true, winit::window::CursorIcon::Pointer);

                                            elem_bgn();
                                            decl(Decl {
                                                colour,
                                                radius,
                                                child_gap,
                                                padding,
                                                width:  grow!(),
                                                height: fit!(),
                                                align: TopLeft,
                                                direction: TopToBottom,
                                                ..Decl
                                            });


                                            if let _ = elem().decl(Decl {
                                                id,
                                                colour,
                                                radius,
                                                padding,
                                                child_gap,
                                                width:  percent!(1.0),
                                                height: fit!(),
                                                align:  Left,
                                                direction: LeftToRight,
                                                ..Decl
                                            })
                                            {
                                                // ui.text("Staked to:", TextDecl { colour: text_colour, h: ui.scale(18.0), align: AlignX::Center, ..TextDecl });
                                                let h = ui.scale(18.0);

                                                let _ = elem().decl(Decl { width: fixed!(h), height: fixed!(h), radius: ui.scale(4.0).dup4(), colour: colour_from_hash(&finalizer, is_online), ..Decl });

                                                ui.text(label, TextDecl { colour: text_colour, h, align: AlignX::Left, ..TextDecl });
                                                let _ = elem().decl(Decl { width: grow!(), ..Decl });
                                                let pct = 100.0 * (total_bonded_to_finalizer as f64 / total_bonded as f64);
                                                let colour = (0xff, 0xaf, 0x0e, 0xff);
                                                ui.text(frame_strf!(data, "{} cTAZ ({:.2}%)", str_from_ctaz(total_bonded_to_finalizer), pct), TextDecl { font: Mono, colour, h: ui.scale(18.0), align: AlignX::Right, ..TextDecl });
                                            }

                                            open_finalizer = ui.openable(data, id, activated, false);
                                        }

                                        if !open_finalizer {
                                            continue;
                                        }

                                        let stake_amount = initial as i64;
                                        if index > 0 { // separator
                                            let colour = {
                                                let mut col = TRANSACTION_HISTORY_CONTAINER_COL;
                                                col = col.hsva();
                                                col.2 = col.2.mul(1.5).min(255);
                                                col.rgba()
                                            };

                                            let _ = elem().decl(Decl { colour, height: fixed!(ui.scale(2.0)), width: percent!(1.0), ..Decl });
                                        }
                                        if let _ = elem().decl(Decl {
                                            id: id_index("Unstake Roster Member", index as u32),
                                            padding,
                                            child_gap,
                                            height: fixed!(ui.scale(48.0)),
                                            width: percent!(1.0),
                                            direction: LeftToRight,
                                            align: Center,
                                            ..Decl
                                        }) {
                                            // left icon
                                            if let _ = elem().decl(Decl {
                                                id: id_index("Unstake Roster Member Left Icon", index as u32),
                                                height: fit!(),
                                                width: fixed!(ui.scale(32.0)),
                                                direction: TopToBottom,
                                                align: Center,
                                                ..Decl
                                            }) {
                                                let (icon, icon_hovered) = (ICON_LINK_1, ICON_UNLINK);
                                                let id = id_index("Unstake Button", index as u32);
                                                if modal_clickable_icon(ui, child_gap, id, icon, icon_hovered, can) {
                                                    wallet_state.lock().unwrap().unstake_from_finalizer(bond_key);
                                                }
                                                if !can && !is_staking_day && ui.hovered(id) {
                                                    set_tooltip_text!(data, "You can only unstake during Staking Day.");
                                                }
                                            }
                                            // left-right icon
                                            if let _ = elem().decl(Decl {
                                                id: id_index("Retarget Roster Member Right Icon", index as u32),
                                                height: fit!(),
                                                width: fixed!(ui.scale(32.0)),
                                                direction: TopToBottom,
                                                align: Center,
                                                ..Decl
                                            }) {
                                                let (icon, icon_hovered) = (ICON_MOVE, ICON_RESIZE_FULL_ALT);
                                                let id = id_index("Retarget Button", index as u32);
                                                if modal_clickable_icon(ui, child_gap, id, icon, icon_hovered, true) {
                                                    ui.modal = Modal::Retarget;
                                                    ui.retarget_modal_bond_key = bond_key;
                                                }
                                            }

                                            // info
                                            if let _ = elem().decl(Decl {
                                                id: id_index("Unstake Roster Member Info", index as u32),
                                                height: fit!(),
                                                width: grow!(),
                                                direction: TopToBottom,
                                                align: Left,
                                                ..Decl
                                            }) {
                                                ui.text(frame_strf!(data, "{}", display_str(&chunkify(&bond_key))), TextDecl { font: Mono, h: ui.scale(14.0), align: AlignX::Left, ..TextDecl });
                                            }

                                            // right info
                                            if let _ = elem().decl(Decl {
                                                id: id_index("Unstake Roster Member Amounts", index as u32),
                                                height: fit!(),
                                                width: fit!(),
                                                direction: TopToBottom,
                                                align: Right,
                                                ..Decl
                                            }) {
                                                let id = id_index("Unstake Clickable Text", index as u32);
                                                let mut colour = (0xff, 0xaf, 0x0e, 0xff); // @todo color
                                                let mut str = frame_strf!(data, "{}", format_stake_amount(stake_amount));
                                                if ui.hovered(id) {
                                                    colour = WHITE;
                                                    str = frame_strf!(data, "{}", format_stake_amount(stake_amount));
                                                }

                                                if let _ = elem().decl(Decl {
                                                    id,
                                                    width: fit!(),
                                                    height: fit!(),
                                                    ..Decl
                                                }) {
                                                    ui.text(str, TextDecl { font: Mono, h: ui.scale(14.0), colour, align: AlignX::Right, ..TextDecl });
                                                }
                                            }
                                        }
                                    }
                                    if prev_finalizer.is_some() {
                                        elem_end();
                                    }
                                }
                                elem_end();
                            }
                        }
                    }
                    scroll_container_finish(ui, data, padding, id, clip, scroll, content_h, viewport_h, max);
                }

                Modal::User => {
                    title_bar(ui, true, "User", id("User Title Bar"));

                    let (id, mut clip, mut scroll, content_h, viewport_h, max) = ui.scroll_container(data, id("User Scroll Container"), 48.0);
                    let scroll = *scroll;
                    scroll_container_begin(ui, data, padding, 0.0, id, clip, scroll, content_h, viewport_h, max);
                    {
                        let (seed, ufvk) = {
                            let state = wallet_state.lock().unwrap();
                            (state.user_seed_mnemonic.clone(), state.user_ufvk.clone())
                        };
                        if modal_action_button(ui, padding, child_gap, "Copy Seed", true, true) {
                            ui.input().send_to_clipboard(&seed);
                        }

                        if modal_action_button(ui, padding, child_gap, "Copy Viewing Key", true, true) {
                            ui.input().send_to_clipboard(&ufvk);
                        }

                    }
                    scroll_container_finish(ui, data, padding, id, clip, scroll, content_h, viewport_h, max);
                }
            }
        }
    }

    // @TODO: MAKE SOME OF THESE NOT USE TABS, JUST USE HEADERS


    // @Todo: How to avoid doing this? Clearing the nav array when there is a modal?
    ui.nav_skip = (ui.modal != Modal::None);

    // Main contents
    if let _ = elem().decl(Decl {
        id: id("Main Contents"),
        colour: PANE_COL,
        padding, child_gap,
        radius: (0.0, 0.0, radius.2, 0.0),
        direction: TopToBottom,
        align: Top,
        width: percent!(1.0),
        height: grow!(),
        clip: ClipX,
        ..Decl
    }) {
        let (
            balance,
            pending_balance,
            staked_balance,
            withdrawable_balance,
        ) = {
            let wallet_state = wallet_state.lock().unwrap();
            if *tab_id == tab_id_user_wallet {(
                wallet_state.user_balance(),
                wallet_state.user_pending_balance(),
                wallet_state.staked_balance,
                wallet_state.withdrawable_balance,
            )} else {(
                wallet_state.miner_balance(),
                wallet_state.miner_pending_balance(),
                0u64,
                0u64,
            )}
        };

        {
            let decl = TextDecl { h: ui.scale(48.0), align: AlignX::Center, ..TextDecl };
            ui.text(&frame_strf!(data, "{} cTAZ", str_from_ctaz(balance.try_into().unwrap())), decl);
        }
        {
            let decl = TextDecl { h: ui.scale(16.0), align: AlignX::Center, colour: (0x90, 0x90, 0x90, 0xff) /* @todo colors */, ..TextDecl };
            ui.text(&frame_strf!(data, "{} cTAZ Pending",      str_from_ctaz(pending_balance.try_into().unwrap())),      decl);
            ui.text(&frame_strf!(data, "{} cTAZ Staked",       str_from_ctaz(staked_balance.try_into().unwrap())),       decl);
            ui.text(&frame_strf!(data, "{} cTAZ Withdrawable", str_from_ctaz(withdrawable_balance.try_into().unwrap())), decl);
        }

        // spacer
        // if let _ = elem().decl(Decl { width: grow!(), height: fixed!(ui.scale(32.0)), ..Default::default() }) {}

        let child_gap = child_gap as f32;
        let padding = child_gap.dup4();

        // buttons container
        let can = (*tab_id == tab_id_user_wallet);
        if can && let _ = elem().decl(Decl {
            id: id("Buttons Container"),
            child_gap,
            align: Center,
            ..Decl
        }) {

            let button = |ui: &mut Context, enabled: bool, icon: &'static str, label: &'static str| {
                let id = id(label);
                let (clicked, colour, text_colour) = ui.button_ex(true, BUTTON_BLUE, id, enabled, winit::window::CursorIcon::Default);
                if let _ = elem().decl(Decl {
                    id, child_gap, align: Center,
                    direction: TopToBottom,
                    width: fit!(),
                    height: fit!(),
                    ..Decl
                }) {

                    let radius = ui.scale(24.0);

                    // Button circle
                    if let _ = elem().decl(Decl {
                        colour, radius: radius.dup4(), padding, child_gap, align: Center,
                        width:  fixed!(radius * 2.0),
                        height: fixed!(radius * 2.0),
                        ..Decl
                    }) {
                        let temp_letter_symbol_h = ui.scale(28.0);
                        ui.text(icon, TextDecl { colour: text_colour, font: Icons, h: temp_letter_symbol_h, align: AlignX::Center, ..TextDecl });
                    }

                    let button_text_h = ui.scale(16.0);
                    let mut hsva = WHITE.hsva();
                    if !enabled {
                        hsva.2 = hsva.2.mul(0.5);
                    }
                    let text_colour = hsva.rgba();
                    ui.text(label, TextDecl { colour: text_colour, h: button_text_h, align: AlignX::Center, ..TextDecl });
                }
                clicked
            };

            // TODO: send should use ICON_PAPER_PLANE but it would be nice to support negative glyph advance first
            if button(ui, can, ICON_UP_BIG, "Send")       { ui.modal = Modal::Send;    }
            if button(ui, can, ICON_QRCODE, "Receive")    { ui.modal = Modal::Receive; }
            if button(ui, can, ICON_LINK_1, "Stake")      { ui.modal = Modal::Stake;   }
            if button(ui, can, ICON_UNLINK, "Edit Stake") { ui.modal = Modal::Unstake; }
            if button(ui, can, ICON_COG_1,  "User")       { ui.modal = Modal::User;    } // TODO: person icon
        }

        // if let _ = elem().decl(Decl { width: grow!(), height: fixed!(ui.scale(32.0)), ..Default::default() }) {}

        let txs = {
            let (mut txs, locals, local_n) = {
                let state = wallet_state.lock().unwrap();
                if *tab_id == tab_id_user_wallet {
                    (state.user_txs.clone(), state.user_local_txs, state.user_local_txs_n)
                } else {
                    (state.miner_txs.clone(), state.miner_local_txs, state.miner_local_txs_n)
                }
            };
            for i in 0..local_n {
                txs.push(locals[i]);
            }
            txs.reverse();
            txs
        };

        let DOUBLE_ICON_OK_CIRCLED_1  = { (&*frame_strf!(magic(data), "{}{}", ICON_OK_CIRCLED_1,  ICON_OK_CIRCLED_1).as_str()) };
        let DOUBLE_ICON_OK_CIRCLED2_1 = { (&*frame_strf!(magic(data), "{}{}", ICON_OK_CIRCLED2_1, ICON_OK_CIRCLED2_1).as_str()) };

        let (id, mut clip, mut scroll, content_h, viewport_h, max) = ui.scroll_container(data, id("History Scroll Container"), 96.0);
        if txs.len() == 0 {
            clip = ClipMode::None;
            *scroll = 0.0;
        }

        let scroll = *scroll;
        ui.scroll_container_bgn(data, padding, 0.0, id, clip, scroll, content_h, viewport_h, max);
        {
            {
                if wallet_is_init == false || txs.len() == 0 {
                    let h = ui.scale(24.0);
                    if let _ = elem().decl(Decl {
                        direction: TopToBottom,
                        width:  percent!(1.0),
                        height: percent!(1.0),
                        child_gap,
                        align:  Center,
                        ..Decl
                    }) {
                        let (icon, text, font) = if ! wallet_is_init {
                            (ICON_EYE_OFF, animated_loading_icon(ui), FontKind::Icons)
                        } else {
                            (ICON_DROPBOX_1, "There are no transactions yet.", FontKind::Normal)
                        };
                        ui.text(icon, TextDecl { font: Icons, colour: WHITE.mul(0.6), h: ui.scale(64.0), align: AlignX::Center, ..TextDecl });
                        ui.text(text, TextDecl { colour: WHITE.mul(0.6), h, align: AlignX::Center, font, ..TextDecl });
                    }
                }
                else {
                    let kind_text_h        = ui.scale(18.0);
                    let transaction_text_h = ui.scale(16.0);

                    let transaction_element_height = ui.scale(100.0);
                    let separator_height           = ui.scale(8.0);
                    let transaction_inner_height   = transaction_element_height - separator_height;

                    let culled_pre_n    = ((ui.scale(scroll) / transaction_element_height).floor() as usize).saturating_sub(1);
                    let culled_in_bgn_o = culled_pre_n;
                    let culled_in_n     = 1 + (viewport_h / transaction_element_height).ceil() as usize + 1;
                    let culled_in_end_o = (culled_pre_n + culled_in_n).min(txs.len());
                    let culled_post_n   = (txs.len() - culled_in_end_o);

                    // culling preamble spacer
                    let _ = elem().decl(Decl { height: Sizing::Fixed(culled_pre_n as f32 * transaction_element_height), ..Decl });

                    let culled_txs = &txs[culled_in_bgn_o .. culled_in_end_o];
                    for (index, tx) in culled_txs.iter().enumerate() {
                        if index + culled_pre_n > 0 { // separator
                            let colour = {
                                let mut col = TRANSACTION_HISTORY_CONTAINER_COL;
                                col = col.hsva();
                                col.2 = col.2.mul(1.5).min(255);
                                col.rgba()
                            };

                            if let _ = elem().decl(Decl { height: fixed!(separator_height), width: percent!(1.0), align: Center, ..Decl }) {
                                let _ = elem().decl(Decl { colour, height: fixed!(ui.scale(2.0)), width: percent!(1.0), ..Decl });
                            }
                        }

                        let tx_h           = tx.reported_height();
                        let tx_is_in_block = tx_h.is_in_block();

                        let id = id_index("Transaction", index as u32);
                        let (clicked, hovered) = {
                            let skip_before = ui.nav_skip;
                            ui.nav_skip = true;

                            let (clicked, _, _) = ui.button_ex(true, BLACK, id, true, winit::window::CursorIcon::Pointer);
                            let hovered = ui.hovered(id);
                            ui.nav_skip = skip_before;

                            (clicked, hovered)
                        };

                        if clicked {
                            let (cx, cy, ok) = viz.pos_at_height(tx.reported_height());
                            if ok {
                                viz.camera_x = cx;
                                viz.camera_y = cy;
                                viz.zoom = 2.0;
                            }
                        }

                        if hovered {
                            viz.ui_hovered_height = Some(tx.reported_height());
                        }

                        if let _ = elem().decl(Decl {
                            id,
                            padding,
                            child_gap,
                            colour: if hovered {
                                TRANSACTION_HISTORY_CONTAINER_COL.mul(0.25)
                            } else {
                                (0, 0, 0, 0)
                            },
                            height:     fixed!(transaction_inner_height),
                            width:      percent!(1.0),
                            direction:  LeftToRight,
                            align:      Center,
                            ..Decl
                        }) {
                            let mut error_text_buf = String::new();
                            let (text_colour, mut icon_colour, status_icon, tooltip) = {
                                let CONFIRMATIONS_THRESHOLD = 3;

                                let confirmations_n = if viz.bc_tip_height >= tx_h.0 as u64 {
                                    viz.bc_tip_height - tx_h.0 as u64
                                } else {
                                    0
                                };

                                pub const RED:  (u8, u8, u8, u8) = (255, 64, 67, 0xff);      /* @todo colors */
                                pub const BLUE: (u8, u8, u8, u8) = (0x33, 0x88, 0xde, 0xff); /* @todo colors */
                                match tx.status {
                                    TxStatus::OnBc => {
                                        if tx_h.0 as u64 <= viz.bc_finalized_tip_height { // finalized
                                            (WHITE, BLUE,  DOUBLE_ICON_OK_CIRCLED_1, "This transaction is in a finalized block.")
                                        } else if tx_h.0 as u64 + CONFIRMATIONS_THRESHOLD <= viz.bc_tip_height { // confirmed
                                            (WHITE, WHITE, DOUBLE_ICON_OK_CIRCLED2_1, &format!("Confirmations: {}/3", confirmations_n).to_string() as &str)
                                        } else if tx_is_in_block {
                                            (WHITE, WHITE, ICON_OK_CIRCLED2_1, &format!("Confirmations: {}/3", confirmations_n).to_string() as &str)
                                        } else if tx_h == BlockHeight::MEMPOOL {
                                            (WHITE, WHITE, ICON_EYE_1, "This transaction is in the mempool waiting to be mined.")
                                        } else if tx_h == BlockHeight::SENT {
                                            (WHITE, WHITE, ICON_PAPER_PLANE, "This transaction is being sent to the network.")
                                        } else if tx_h == BlockHeight::BUILT {
                                            (WHITE, WHITE, ICON_WRENCH, "This transaction is being built by your wallet.")
                                        } else if tx_h == BlockHeight::PROPOSED {
                                            (grey, grey, ICON_WRENCH, "This transaction is in the queue and will be built soon.")
                                        } else { // INVALID
                                            println!("UI saw tx at {tx_h:?} ({:?}, {:?})", tx.h, tx.status);
                                            (grey, grey, ICON_CANCEL, "This transaction has encountered an error.")
                                        }
                                    }

                                    TxStatus::SoftFail(h) | TxStatus::HardFail(h, _) => {
                                        let (icon, tooltip) = if h == BlockHeight::PROPOSED {
                                            (ICON_WRENCH,      "This transaction failed to build.") // Failed to build
                                        } else if h == BlockHeight::BUILT {
                                            (ICON_PAPER_PLANE, "This transaction failed to send.") // Failed to send
                                        } else if h == BlockHeight::SENT {
                                            (ICON_EYE_OFF,     "This transaction was unable to enter the mempool.")
                                        } else if h == BlockHeight::MEMPOOL {
                                            (ICON_EYE_OFF,     "This transaction failed while in the mempool.")
                                        } else if tx_is_in_block {
                                            (ICON_FORK,        "This transaction is on a side chain.")
                                        } else {
                                            (ICON_CANCEL,      "This transaction encountered an error.")
                                        };

                                        match tx.status {
                                            TxStatus::OnBc => unreachable!("already filtered"),
                                            TxStatus::SoftFail(_) => (grey, grey, icon, tooltip),
                                            TxStatus::HardFail(_, msg) => {
                                                error_text_buf = format!("{}", msg.to_string());
                                                (RED, RED, icon, &error_text_buf as &str)
                                            }
                                        }
                                    }
                                }
                            };

                            icon_colour = icon_colour.mul(deemph_mul);

                            // left icon
                            if /*false &&*/ let _ = elem().decl(Decl {
                                id: id_index("Left Icon", index as u32),
                                height: fit!(),
                                width: fixed!(ui.scale(32.0)),
                                direction: TopToBottom,
                                align: Center,
                                ..Decl
                            }) {
                                let mut pending = tx.part_flags != TxParts::FULL_TX;
                                let icon = if pending {
                                    animated_loading_icon(ui)
                                } else {
                                    match tx.kind() {
                                        WalletTxKind::Send         => ICON_UP_SMALL,
                                        WalletTxKind::SelfSend     => ICON_DOWN_SMALL,
                                        WalletTxKind::Receive      => ICON_DOWN_SMALL,
                                        WalletTxKind::Mine         => ICON_MONEY_1, // TODO: pickaxe/tools icon
                                        WalletTxKind::Shield       => ICON_SHIELD,
                                        WalletTxKind::Stake        => ICON_LINK_1,
                                        WalletTxKind::BeginUnstake => ICON_LINK_EXT_ALT,
                                        WalletTxKind::Retarget     => ICON_MOVE,
                                        WalletTxKind::ClaimUnstake => ICON_UNLINK,
                                    }
                                };

                                // TODO: account for mempool & confirmation level
                                // let colour = if tx_is_on_best_chain || pending {
                                //     WHITE
                                // } else {
                                //     (0xc0, 0x30, 0x20, 0xff) /* @todo colors */
                                // };

                                ui.text(icon, TextDecl { font: Icons, colour: icon_colour, h: ui.scale(24.0), align: AlignX::Center, ..TextDecl });
                            }

                            // info
                            if let _elem = elem().decl(Decl {
                                id: id_index("Centre Info", index as u32),
                                height: fit!(),
                                width: grow!(),
                                direction: TopToBottom,
                                align: Left,
                                ..Decl
                            }) {
                                let tx_kind = tx.kind();
                                let label = match tx_kind {
                                    WalletTxKind::Send          => if tx_is_in_block { "Sent"      } else { "Sending..."   },
                                    WalletTxKind::Receive       => if tx_is_in_block { "Received"  } else { "Receiving..." },
                                    WalletTxKind::Mine          => if tx_is_in_block { "Mined"     } else { "Mining..."    },
                                    WalletTxKind::SelfSend      => if tx_is_in_block { "Returned"  } else { "Returning..." },
                                    WalletTxKind::Shield        => if tx_is_in_block { "Shielded"  } else { "Shielding..." },
                                    WalletTxKind::Stake         => if tx_is_in_block { "Staked"    } else { "Staking..."   },
                                    WalletTxKind::BeginUnstake  => if tx_is_in_block { "Unstaked"  } else { "Unstaking..." },
                                    WalletTxKind::Retarget      => if tx_is_in_block { "Moved Delegation"  } else { "Moving Delegation..." },
                                    WalletTxKind::ClaimUnstake  => if tx_is_in_block { "Withdrawn"  } else { "Withdrawing..." },
                                };

                                let label_str = 'get_label: {
                                    if tx_is_in_block {
                                        // NOTE: this assumes that objects maintain bond keys after retargeting
                                        if let Some(staking_action) = tx.staking_action {
                                            let sent_stake = (tx_kind == WalletTxKind::Stake || tx_kind == WalletTxKind::Retarget);

                                            if sent_stake {
                                                for bond in &staked_roster_bonded {
                                                    if bond.0 == staking_action.arg32_0 {
                                                        // TODO: split mono
                                                        break 'get_label if bond.1 != staking_action.arg32_2 {
                                                            frame_strf!(data, "{} @ {} to {} (moved to {}), now {} cTAZ", label, tx_h.0, display_str(&chunkify(&staking_action.arg32_2)), display_str(&chunkify(&bond.1)), str_from_ctaz(bond.2))
                                                        } else {
                                                            frame_strf!(data, "{} @ {} to {}, now {} cTAZ", label, tx_h.0, display_str(&chunkify(&staking_action.arg32_2)), str_from_ctaz(bond.2))
                                                        };
                                                    }
                                                }

                                                for bond in &staked_roster_unbonded {
                                                    if bond.0 == staking_action.arg32_0 {
                                                        break 'get_label frame_strf!(data, "{} @ {} to {} (unstaked), now {} cTAZ", label, tx_h.0, display_str(&chunkify(&staking_action.arg32_2)), str_from_ctaz(bond.2))
                                                    }
                                                }
                                            }

                                            // TODO: compress
                                            if tx_kind == WalletTxKind::ClaimUnstake {
                                                // NOTE: ignoring fee
                                                let b = WalletTxPart::from_staking_action(tx.staking_action);
                                                break 'get_label frame_strf!(data, "{} @ {}", label, tx_h.0);
                                            }

                                            let withdrawal = 'withdrawal: {
                                                let mut tx_i = index+1;
                                                while tx_i < txs.len() {
                                                    if txs[tx_i].is_on_bc() && txs[tx_i].h.is_in_block() {
                                                        if let (Some(staking_action), WalletTxKind::ClaimUnstake) = (txs[tx_i].staking_action, txs[tx_i].kind()) {
                                                            // NOTE: ignoring fee
                                                            let b = WalletTxPart::from_staking_action(txs[tx_i].staking_action);
                                                            break 'withdrawal Some((b, txs[tx_i].h));
                                                        }
                                                    }
                                                    tx_i += 1;
                                                }
                                                None
                                            };

                                            if let Some(withdrawal) = withdrawal {
                                                let (b, h) = withdrawal;
                                                break 'get_label if sent_stake {
                                                    frame_strf!(data, "{} @ {} to {}, claimed @ {} for {} cTAZ", label, tx_h.0, display_str(&chunkify(&staking_action.arg32_2)), h, str_from_ctaz(b.spent_zats.into_u64()))
                                                } else {
                                                    frame_strf!(data, "{} @ {}, claimed @ {} for {} cTAZ", label, tx_h.0, h, str_from_ctaz(b.spent_zats.into_u64()))
                                                };
                                            }

                                            // fallback if not found anywhere, not ideal
                                            if sent_stake {
                                                frame_strf!(data, "{} @ {} to {}", label, tx_h.0, display_str(&chunkify(&staking_action.arg32_2)))
                                            } else {
                                                frame_strf!(data, "{} @ {}", label, tx_h.0)
                                            }
                                        } else {
                                            frame_strf!(data, "{} @ {}", label, tx_h.0)
                                        }
                                    } else {
                                        frame_strf!(data, "{}", label)
                                    }
                                };
                                // let label_str = frame_strf!(data, "{}", index);

                                ui.text(label_str, TextDecl { h: kind_text_h, align: AlignX::Left, colour: text_colour, ..TextDecl });

                                let details_col = text_colour.mul(deemph_mul);
                                // spacer
                                /* // */ if let _ = elem().decl(Decl { width: grow!(), height: fixed!(ui.scale(4.0)), ..Default::default() }) {}

                                let txid = tx.txid.to_string();
                                /* // */ ui.text(frame_strf!(data, "{}..{}", &txid[0..8], &txid[txid.len() - 8..]), TextDecl { font: Mono, h: transaction_text_h, colour: details_col, align: AlignX::Left, ..TextDecl });

                                // spacer
                                /* // */ if let _ = elem().decl(Decl { width: grow!(), height: fixed!(ui.scale(4.0)), ..Default::default() }) {}

                                if let Ok(memo_str) = String::from_utf8(tx.memo.as_slice().to_vec()) {
                                    let mut memo_str = memo_str.chars().filter(|c| c.is_ascii()).collect::<String>().trim_end_matches(|c| c == '\0').to_string();
                                    if memo_str.len() != 0 {
                                        if memo_str.starts_with("@") {
                                            if let Some(end) = memo_str.find(":") {
                                                memo_str = memo_str[end + 1..].to_string();
                                            }
                                        }

                                        /* // */ ui.text(frame_strf!(data, "{}", memo_str), TextDecl { h: transaction_text_h, align: AlignX::Left, colour: details_col, ..TextDecl });
                                    }
                                }
                            }

                            // right info
                            if /*false &&*/ let _ = elem().decl(Decl {
                                id: id_index("Right Info", index as u32),
                                height: grow!(),
                                width: fit!(),
                                child_gap: ui.scale(5.0),
                                direction: TopToBottom,
                                align: Right,
                                ..Decl
                            }) {
                                let _ = elem().decl(Decl { height: grow!(), ..Decl }); // spacer

                                // @todo colors
                                let colour = match tx.kind() {
                                    WalletTxKind::Send    => (0xec, 0x27, 0x3f, 0xff),
                                    WalletTxKind::Stake   => (0xff, 0xaf, 0x0e, 0xff),
                                    WalletTxKind::Receive | WalletTxKind::ClaimUnstake | WalletTxKind::Mine => (0x5a, 0xb5, 0x52, 0xff),
                                    WalletTxKind::Shield  => (0x33, 0x88, 0xde, 0xff),
                                    WalletTxKind::BeginUnstake => WHITE,
                                    _ => WHITE,
                                };

                                let all = tx.totals(true);
                                let all_no_bond = tx.totals(false);
                                let (show_val, prefix, zats) = match tx.kind() {
                                    WalletTxKind::BeginUnstake | WalletTxKind::Retarget => (false, "", 0), // fee-only

                                    WalletTxKind::Send  => (true, "-", all.sent_zats.into_u64().saturating_sub(all.recv_zats.into_u64())),
                                    WalletTxKind::Stake => (true, "",  WalletTxPart::from_staking_action(tx.staking_action).recv_zats.into_u64()), // aka bond.recv_zats
                                    WalletTxKind::Stake => (true, "",  all.sent_zats.into_u64().saturating_sub(all_no_bond.recv_zats.into_u64())), // aka bond.recv_zats

                                    WalletTxKind::SelfSend     => (true, "",  all.recv_zats.into_u64()),
                                    WalletTxKind::Shield       => (true, "",  all.recv_zats.into_u64()),
                                    WalletTxKind::Receive      => (true, "+", all.recv_zats.into_u64()),
                                    WalletTxKind::ClaimUnstake => (true, "+", all.recv_zats.into_u64()),
                                    WalletTxKind::Mine         => (true, "+", all.recv_zats.into_u64()),
                                };

                                if show_val {
                                    ui.text(frame_strf!(data, "{}{} cTAZ", prefix, str_from_ctaz(zats)), TextDecl { h: transaction_text_h, align: AlignX::Right, colour, ..TextDecl });
                                    // let (t_z, s_z) = (tx.parts[0].recv_zats.into_u64(), tx.parts[1].recv_zats.into_u64());
                                    // ui.text(frame_strf!(data, "+{} | {} cTAZ", str_from_ctaz(t_z), str_from_ctaz(s_z)), TextDecl { h: transaction_text_h, align: AlignX::Right, colour, ..TextDecl });
                                }

                                // We care about fees if we spent any money ourselves.
                                // If we just received from somewhere else, we don't care about fees.
                                if tx.kind() != WalletTxKind::Receive &&
                                   tx.kind() != WalletTxKind::Mine // ALT: if fee != Some(0)
                                {
                                    let fee_str = if let Some(fee) = tx.fee() {
                                        str_from_ctaz(fee.into_u64())
                                    } else {
                                        format!("<unknown>")
                                    };
                                    ui.text(frame_strf!(data, "Fee: {} cTAZ", fee_str), TextDecl { h: transaction_text_h, align: AlignX::Right, colour, ..TextDecl });
                                }

                                // let _ = elem().decl(Decl { height: grow!(), ..Decl }); // spacer

                                if let _elem = elem().decl(Decl {
                                    id:     id_index("Confirmation Status", index as u32),
                                    height: grow!(),
                                    width:   fit!(),
                                    align:  BottomRight,
                                    ..Decl
                                }) {
                                    if ui.hovered(_elem.decl.id) {
                                        set_tooltip_text!(data, "{}", tooltip);
                                    }

                                    let confirmation_icons_h = if status_icon == ICON_FORK { ui.scale(20.0) } else { ui.scale(12.0) };
                                    ui.text(status_icon, TextDecl { font: Icons, colour: icon_colour, h: confirmation_icons_h, wrap: Wrap::None, align: AlignX::Right,  ..TextDecl });
                                }
                            }
                        }
                    }

                    // culling postamble spacer
                    let _ = elem().decl(Decl { height: Sizing::Fixed(culled_post_n as f32 * transaction_element_height), ..Decl });
                }
            }
        }
        ui.scroll_container_end(data, padding, id, clip, scroll, content_h, viewport_h, max);
    }

    ui.nav_skip = false;
}

pub fn ui_right_pane(ui: &mut Context,
                 wallet_state: Arc<Mutex<WalletState>>,
                 viz: &mut VizState,
                 data: &mut UiData,
                 child_gap: f32,
                 padding: (f32, f32, f32, f32),
                 radius:  (f32, f32, f32, f32),
                 dummy_1: &mut Id,
                 dummy_2: &mut Id) {

    let bft_status = &viz.bft_recency_status;
    // ui.text(frame_strf!(data, "BFT recency: {bft_recency:#?}"), TextDecl { font: Mono, h: ui.scale(20.0), colour: WHITE, align: AlignX::Center, ..TextDecl });

    // @TODO: MAKE THESE NOT USE TABS, JUST USE HEADERS
    let mut tab_id_faucet = Id::default();
    let mut tab_id_roster = Id::default();

    let (mut filters, wallet_is_init, mut roster) = {
        let state = wallet_state.lock().unwrap();

        (state.filters, state.wallet_is_init, state.roster.clone())
    };

    let mut filters_modified = true;

    roster.sort_by_key(|member| std::cmp::Reverse(member.voting_power));
    let roster = roster;

    if let _ = elem().decl(Decl {
        id: id("Finalizers Tab Bar"),
        child_gap,
        width: percent!(1.0),
        height: fit!(),
        align: Center,
        ..Decl
    }) {
        tab_id_roster = ui.tab_ex((0.0, radius.1, radius.2, radius.3), padding, dummy_1, id("Finalizers"), frame_strf!(data, "Finalizers ({})", roster.len()));
    }
    if let _ = elem().decl(Decl {
        id: id("Finalizers Contents"),
        padding, child_gap,
        radius: (0.0, 0.0, 0.0, radius.3),
        colour: PANE_COL,
        direction: TopToBottom,
        width: percent!(1.0),
        height: grow!(),
        ..Decl
    }) {
        let button_ex = |ui: &mut Context, label, act_on_press, enabled: bool| {


            let id = id(label);
            let (clicked, colour, text_colour) = ui.button_ex(act_on_press, BUTTON_GREY, id, enabled, winit::window::CursorIcon::Default);
            if let _ = elem().decl(Decl {
                id,
                child_gap,
                align: Center,
                direction: TopToBottom,
                width: fit!(),
                height: fit!(),
                ..Decl
            }) {
                let radius = ui.scale(24.0);

                // Button
                if let _ = elem().decl(Decl {
                    colour,
                    padding,
                    child_gap,
                    radius: radius.dup4(),
                    align: Center,
                    width:  fit!(ui.scale(192.0)),
                    height: fit!(radius * 2.0),
                    ..Decl
                }) {
                    let h = ui.scale(20.0);
                    ui.text(label, TextDecl { h, colour: text_colour, align: AlignX::Center, ..TextDecl });
                }
            }

            clicked
        };

        let clickable_icon = |ui: &mut Context, id, icon, enabled | {

            let (clicked, colour, _) = ui.button_ex(false, (0xcc, 0xcc, 0xcc, 0xff) /* @todo colors */, id, enabled, winit::window::CursorIcon::Pointer);
            if let _ = elem().decl(Decl {
                id,
                child_gap,
                align: Center,
                direction: TopToBottom,
                width: fit!(),
                height: fit!(),
                ..Decl
            }) {
                ui.text(icon, TextDecl { font: Icons, colour, h: ui.scale(24.0), align: AlignX::Center, ..TextDecl });
            }

            clicked
        };


        // @todo(judah): incorporate into list below? (i.e. always at the top of the list (and doesn't scroll with the rest), styled different from the others)
        if let _ = elem().decl(Decl {
            id: id("Finalizers Buttons"),
            padding, child_gap, align: Center,
            direction: TopToBottom,
            width: percent!(1.0),
            height: fit!(),
            ..Decl
        }) {


            let mut recv_address = String::new();
            for b in wallet::TENDERLINK_PUBLIC_KEY.lock().unwrap().0.iter().rev() {
                recv_address.push_str(&format!("{:02x}", b));
            }

            if let _ = elem().decl(Decl {
                id: id("Finalizers Buttons"),
                padding, child_gap, align: Center,
                direction: LeftToRight,
                width: percent!(1.0),
                height: fit!(),
                ..Decl
            }) {
                //NOTE(Giovanni): New finalizer status bar
                if let _ = elem().decl(Decl {
                    direction: TopToBottom,
                    align: Center,
                    radius: ui.scale(18.0).dup4(),
                    child_gap: ui.scale(8.0),
                    width: fit!(),
                    height: fit!(),
                    ..Decl
                }) {
                    let combo_id = id("Finalizer Online Status");
                    let base = BUTTON_GREY.mul(0.85);
                    let (activated, colour, text_colour) = ui.button_ex(true, base, combo_id, true, winit::window::CursorIcon::Pointer);
                    if let _ = elem().decl(Decl {
                        id: combo_id,
                        colour,
                        direction: LeftToRight,
                        align: Center,
                        width:  fit!(),
                        height: fit!(),
                        radius: ui.scale(4.0).dup4(),
                        ..Decl
                    }) {
                        if let _ = elem().decl(Decl {
                            colour: colour,
                            width:  fit!(),
                            height: fit!(),
                            ..Decl
                        }) {}
                        // NOTE(Giovanni): NEED THAT SPACING
                        ui.text(" ", TextDecl { h: ui.scale(24.0), colour: WHITE.mul(0.75), align: AlignX::Right, ..TextDecl });
                        ui.text(ICON_FOLDER_1, TextDecl { font: Icons, h: ui.scale(14.0), colour: WHITE.mul(0.6), align: AlignX::Left, ..TextDecl });
                        ui.text(" Finalizer Filters", TextDecl { h: ui.scale(24.0), colour: WHITE.mul(0.75), align: AlignX::Right, ..TextDecl });

                    }
                    if ui.hovered(combo_id) {
                        set_tooltip_text!(data, "Click to expand.");
                    }

                    if ui.openable(data, combo_id, activated, false) {
                        let id = id("Matches Height Button");

                        let (clicked, colour, text_colour) = ui.button_ex(true, BUTTON_GREY, id, true, winit::window::CursorIcon::Pointer);
                        let radius = ui.scale(16.0);

                        let mut state = wallet_state.lock().unwrap();

                        if let _ = elem().decl(Decl {
                            id: id,
                            colour,
                            padding,
                            child_gap,
                            radius: ui.scale(4.0).dup4(),
                            align: Center,
                            width:  fit!(ui.scale(128.0)),
                            height: fit!(radius * 2.0),
                            ..Decl
                        }) {

                            let text = if filters.filter_height {"Matches my height/round [ON]"} else {"Matches my height/round [OFF]"};
                            ui.text(text, TextDecl {font: Mono, h: ui.scale(16.0), colour: text_colour, align: AlignX::Center, ..TextDecl });
                            if clicked{ filters.filter_height = !filters.filter_height; }
                        }
                        if let _ = elem().decl(Decl {
                            id:ui::id("Finalizer Status Container"),
                            width: grow!(),
                            height: fit!(),
                            radius: ui.scale(4.0).dup4(),
                            ..Decl
                        }) {
                            let secs_string = ui.textbox(
                                data,
                                ui::id("Seconds since connected Textbox"),
                                "Seconds since connected...",
                                TextDecl { font: Mono, h: ui.scale(14.0), colour: WHITE, align: AlignX::Left, ..TextDecl },
                            );
                            let secs_str = secs_string.trim();
                            if let Ok (value) = secs_str.parse::<u32>(){
                                filters.filter_seconds_since_connected = value;
                                //println!("{:?}", state.finalizer_seconds_since_connected);
                            } else if secs_str.len() == 0 {
                                filters.filter_seconds_since_connected = 0;
                            }
                        }
                    }
                }
            }
            //@TODO(Giovanni): Maybe lock behind a key-input (hold to open the popup)...
            if(ui.hovered(id("Finalizers Buttons"))){
                #[cfg(debug_assertions)]  set_tooltip_text!(data, "last checked: {}, height: {}, round: {}", bft_status.now_utc, bft_status.my_height, bft_status.my_round);
            }
            ui.text("Your Finalizer Identity", TextDecl { h: ui.scale(20.0), colour: WHITE, align: AlignX::Center, ..TextDecl });
            ui.text(frame_strf!(data, "[{}..{}]", &recv_address[0..8], &recv_address[recv_address.len()-8..]), TextDecl { font: Mono, h: ui.scale(20.0), colour: WHITE, align: AlignX::Center, ..TextDecl });

            if button_ex(ui, "Copy Identity", true, true) {
                ui.input().send_to_clipboard(&recv_address);
            }
        }

        let mut online_stake = 0u64;

        let mut total_stake = 0u64;
        for member in &roster {
            total_stake += member.voting_power;

            if finalizer_is_online(member.pub_key, &bft_status, &filters){
                online_stake += member.voting_power;
            }
        }
        let offline_stake = total_stake - online_stake;

        /*@TODO(Giovanni) FILL THESEEEEEE WTH ACTUAL DATA*/
        let height = 3000;
        let seconds_since_connected = 300;
        /*-------------------------------------*/

        let online_offline_roster_members = [
        WalletRosterMember{
            pub_key:[0xAA;32],
            voting_power:online_stake,
            txids:Vec::new(),
        },
        WalletRosterMember{
            pub_key:[0xBB;32],
            voting_power:offline_stake,
            txids:Vec::new(),
        }];

        finalizer_ratio_bar(ui, data, bft_status, &online_offline_roster_members, total_stake, height, seconds_since_connected, &FinalizerFilters::default(), ui::id("Right Pane Ratio Bar 1"), false);

        finalizer_ratio_bar(ui, data, bft_status, &roster, total_stake, height, seconds_since_connected, &filters, ui::id("Right Pane Ratio Bar 2"), true);

        let (id, mut clip, mut scroll, content_h, viewport_h, max) = ui.scroll_container(data, id("Finalizer Scroll Container"), 48.0);
        if roster.len() == 0 {
            clip = ClipMode::None;
            *scroll = 0.0;
        }
        let scroll = *scroll;
        ui.scroll_container_bgn(data, padding, child_gap * 0.5, id, clip, scroll, content_h, viewport_h, max);
        {

            if wallet_is_init == false || roster.len() == 0 {
                let h = ui.scale(24.0);
                if let _ = elem().decl(Decl {
                    direction: TopToBottom,
                    width:  percent!(1.0),
                    height: percent!(1.0),
                    child_gap,
                    align:  Center,
                    ..Decl
                }) {
                    ui.text(ICON_EYE_OFF, TextDecl { font: Icons, colour: WHITE.mul(0.6), h: ui.scale(64.0), align: AlignX::Center, ..TextDecl });
                    let (text, font) = if ! wallet_is_init {
                        (animated_loading_icon(ui), FontKind::Icons)
                    } else {
                        ("There are no finalizers yet.", FontKind::Normal)
                    };
                    ui.text(text, TextDecl { colour: WHITE.mul(0.6), h, align: AlignX::Center, font, ..TextDecl });
                }
            }
            else {
                let kind_text_h = ui.scale(18.0);
                let transaction_text_h = ui.scale(16.0);
                let state = wallet_state.lock().unwrap();

                for (index, member) in roster.iter().enumerate() {
                    if index > 0 { // separator
                        let colour = {
                            let mut col = TRANSACTION_HISTORY_CONTAINER_COL;
                            col = col.hsva();
                            col.2 = col.2.mul(1.5).min(255);
                            col.rgba()
                        };

                        let _ = elem().decl(Decl { colour, height: fixed!(ui.scale(2.0)), width: percent!(1.0), ..Decl });
                    }

                    let finalizer_status = get_finalizer_status(bft_status, member.pub_key);
                    let roster_member_id =  id_index("Roster Member", index as u32);
                    if let _ = elem().decl(Decl {
                        id: roster_member_id,
                        padding: (padding.0/4f32, padding.1/4f32, padding.2, padding.3),
                        child_gap,
                        height: fixed!(ui.scale(48.0)),
                        width: percent!(1.0),
                        direction: LeftToRight,
                        align: Left,
                        ..Decl
                    }) {
                        let is_online = if let Some(f) = finalizer_status {
                            finalizer_is_online_ex(&f, &bft_status, &filters)
                        } else {
                            false
                        };

                        let text_colour = if is_online {
                            WHITE
                        } else {
                            WHITE.mul(0.6)
                        };

                        // left icon
                        if let _ = elem().decl(Decl {
                            id: id_index("Roster Member Left Icon", index as u32),
                            height: fit!(),
                            width: fixed!(ui.scale(32.0)),
                            direction: TopToBottom,
                            align: Center,
                            ..Decl
                        }) {
                            if clickable_icon(ui, id_index("Copy Button", index as u32), ICON_DOCS_1, true) {
                                let mut address_str = String::new();
                                for b in member.pub_key.iter().rev() {
                                    address_str.push_str(&format!("{:02x}", b));
                                }
                                ui.input().send_to_clipboard(&address_str);
                            }
                        }

                        // finalizer colour token
                        let info_h = ui.scale(18.0);


                        let _ = elem().decl(Decl { width: fixed!(info_h), height: fixed!(info_h), radius: ui.scale(4.0).dup4(), colour: colour_from_hash(&member.pub_key, is_online), ..Decl });

                        // info
                        if let _ = elem().decl(Decl {
                            id: id_index("Roster Member Info", index as u32),
                            height: fixed!(info_h),
                            width: grow!(),
                            direction: TopToBottom,
                            align: TopLeft,
                            ..Decl
                        }) {
                            ui.text(frame_strf!(data, "{}", display_str_with_edge_bytes(&chunkify(&member.pub_key), 3)), TextDecl { font: Mono, colour: text_colour, h: info_h, align: AlignX::Left, ..TextDecl });
                        }

                        // right info
                        if let _ = elem().decl(Decl {
                            id: id_index("Roster Member Amounts", index as u32),
                            height: fit!(),
                            width: grow!(),
                            direction: TopToBottom,
                            align: Right,
                            ..Decl
                        }) {
                            let colour = (0xff, 0xaf, 0x0e, 0xff);
                            ui.text(frame_strf!(data, "{} cTAZ", str_from_ctaz(member.voting_power)), TextDecl { font: Mono, h: ui.scale(14.0), colour, wrap: Wrap::None, align: AlignX::Right, ..TextDecl });

                        }
                    }

                    #[cfg(debug_assertions)]
                    if ui.hovered(roster_member_id) {
                        set_tooltip_text!(data, "{:#?}", finalizer_status);
                    }
                }
            }
        }
        ui.scroll_container_end(data, padding, id, clip, scroll, content_h, viewport_h, max);

    }
    // spacer
    if let _ = elem().decl(Decl { width: grow!(), height: fixed!(ui.scale(16.0)), ..Default::default() }) {}

    if let _ = elem().decl(Decl {
        id: id("Faucet Tab Bar"),
        child_gap,
        width: percent!(1.0),
        height: fit!(),
        align: Center,
        ..Decl
    }) {
        tab_id_faucet = ui.tab_ex((0.0, radius.1, radius.2, radius.3), padding, dummy_2, id("Faucet"), frame_strf!(data, "Faucet (Height {})", &wallet_state.lock().unwrap().miner_seen_h));
    }
    if let _ = elem().decl(Decl {
        id: id("Faucet Contents"),
        padding, child_gap,
        radius: (0.0, 0.0, 0.0, radius.3),
        colour: PANE_COL,
        direction: TopToBottom,
        align: Center,
        width: percent!(1.0),
        height: fit!(),
        ..Decl
    }) {
        // big text container
        // if let _ = elem().decl(Decl {
        //     width: percent!(1.0),
        //     height: fit!(),
        //     padding,
        //     align: Center,
        //     ..Decl
        // }) {
        //     let big_text_h = ui.scale(32.0);
        //     ui.text(&balance_str, TextDecl { h: big_text_h, align: AlignX::Center, ..TextDecl });
        // }

        let child_gap = child_gap as f32;
        let padding = child_gap.dup4();

        // info container
        // if let _ = elem().decl(Decl {
        //     padding: ui.scale(32.0).dup4(), child_gap, align: TopLeft,
        //     width: grow!(), height: fit!(),
        //     direction: TopToBottom,
        //     ..Decl
        // })
        if false {
            let title_h = ui.scale(28.0);
            let text_h = ui.scale(22.0);
            let (un, sh_p, sh_s) = {
                let w = wallet_state.lock().unwrap();
                (
                    w.miner_unshielded_funds,
                    w.miner_shielded_pending_funds,
                    w.miner_shielded_spendable_funds,
                )
            };

            let row   = Decl { width: percent!(1.0), child_gap, height: fit!(), ..Decl };

            let left  = Decl { width: grow!(), height: fit!(), align: Left,  ..Decl };
            let right = Decl { width: grow!(), height: fit!(), align: Right, ..Decl };

            let left_text  = TextDecl { h: text_h,  align: AlignX::Left,  ..TextDecl  };
            let right_text = TextDecl { font: Mono, align: AlignX::Right, ..left_text };

            if let _ = elem().decl(row) {
                if let _ = elem().decl(left)  { ui.text("Unshielded:", left_text); }
                if let _ = elem().decl(right) { ui.text(frame_strf!(data, "{} cTAZ", str_from_ctaz(un)), right_text); }
            }
            if let _ = elem().decl(row) {
                if let _ = elem().decl(left)  { ui.text("Shielded (Pending):", left_text); }
                if let _ = elem().decl(right) { ui.text(frame_strf!(data, "{} cTAZ", str_from_ctaz(sh_p)), right_text); }
            }
            if let _ = elem().decl(row) {
                if let _ = elem().decl(left)  { ui.text("Shielded (Spendable):", left_text); }
                if let _ = elem().decl(right) { ui.text(frame_strf!(data, "{} cTAZ", str_from_ctaz(sh_s)), right_text); }
            }
            // if let _ = elem().decl(Decl { width: grow!(), height: fixed!(ui.scale(32.0)), ..Default::default() }) {}
        };

        // buttons container
        // if let _ = elem().decl(Decl {
        //     id: id("Buttons Container"),
        //     padding, child_gap, align: Center,
        //     width: percent!(1.0),
        //     height: fit!(),
        //     ..Decl
        // })
        {

            let button_ex = |ui: &mut Context, id, label, act_on_press, enabled: bool| {
                let (clicked, colour, text_colour) = ui.button_ex(act_on_press, BUTTON_GREY, id, enabled, winit::window::CursorIcon::Default);
                if let _ = elem().decl(Decl {
                    id,
                    child_gap,
                    align: Center,
                    direction: TopToBottom,
                    width: fit!(),
                    height: fit!(),
                    ..Decl
                }) {
                    let radius = ui.scale(24.0);

                    // Button
                    if let _ = elem().decl(Decl {
                        colour,
                        padding,
                        child_gap,
                        radius: radius.dup4(),
                        align: Center,
                        width:  fit!(ui.scale(192.0)),
                        height: fit!(radius * 2.0),
                        ..Decl
                    }) {
                        let h = ui.scale(20.0);
                        ui.text(label, TextDecl { h, colour: text_colour, align: AlignX::Center, ..TextDecl });
                    }
                }

                clicked
            };

            let miner_shielded_spendable_funds = {
                let w = wallet_state.lock().unwrap();
                w.miner_shielded_spendable_funds
            };

            let can = miner_shielded_spendable_funds > (ONE_cTAZ * 501 / 100);

            let label = "Receive cTAZ";
            let id = id(label);
            if button_ex(ui, id, label, false, can && !wallet_state.lock().unwrap().waiting_for_faucet) {
                wallet_state.lock().unwrap().request_from_faucet();
            }
            if !can && ui.hovered(id) {
                set_tooltip_text!(data, "The faucet is mining more funds.");
            }
        }
    }
    if filters_modified{
        wallet_state.lock().unwrap().filters = filters;
    }
}

fn animated_loading_icon(ui: &Context) -> &str {
    let timer = (ui.tx_loading_animation_timer * 3.0) as usize;
    [ICON_DOT, ICON_DOT_2, ICON_DOT_3][timer % 3]
}

pub fn run_ui(ui: &mut Context, wallet_state: Arc<Mutex<WalletState>>, data: &mut UiData, viz: &mut VizState, is_rendering: bool) -> bool {


    data.per_frame_strs.clear();

    let mut result = false;

    const MIN_ZOOM: f32 = 0.5;
    const MAX_ZOOM: f32 = 1.75; // @Todo: @PreventOverflowOnZoom

    if ui.input().key_held(KeyCode::ControlLeft) || ui.input().key_held(KeyCode::ControlRight) {


        if ui.input().key_pressed(KeyCode::Equal) {
            let new_zoom = ui.zoom * (1.0f32 + 1.0f32 / 8f32);
            if new_zoom <= MAX_ZOOM {
                ui.zoom = new_zoom;
            }
        }
        if ui.input().key_pressed(KeyCode::Minus) {
            let new_zoom = ui.zoom / (1.0f32 + 1.0f32 / 8f32);
            if new_zoom >= MIN_ZOOM {
                ui.zoom = new_zoom;
            }
        }
        if ui.input().key_pressed(KeyCode::Digit0) {
            ui.zoom = 1.0f32;
        }
    }
    if ui.zoom < MIN_ZOOM {
        ui.zoom = 1.0f32; // reset instead of clamp to prevent user from shifting "off-grid" of the exponential steps
    }
    if ui.zoom > MAX_ZOOM {
        ui.zoom = 1.0f32; // reset instead of clamp to prevent user from shifting "off-grid" of the exponential steps
    }
    ui.scale = ui.zoom * ui.dpi_scale;

    ui.cursor = winit::window::Cursor::Icon(winit::window::CursorIcon::Default);

    let shift_held = (ui.input().key_held(KeyCode::ShiftLeft)   || ui.input().key_held(KeyCode::ShiftRight));
    let ctrl_held  = (ui.input().key_held(KeyCode::ControlLeft) || ui.input().key_held(KeyCode::ControlRight));
    if ctrl_held && ui.input().key_pressed(KeyCode::KeyJ) {
        ui.modal = Modal::Jump;
    }

    ui.capture = false;
    // let prev_nav_idx_to_id = ui.nav_idx_to_id.clone();
    ui.nav_id_to_idx.clear();
    ui.nav_idx_to_id.clear();

    let (window_w, window_h) = (ui.draw().window_width as f32, ui.draw().window_height as f32);
    let mouse_pos = (ui.input().mouse_pos().0 as f32, ui.input().mouse_pos().1 as f32);

    let child_gap = ui.scale(12.0);
    let padding = child_gap.dup4();

    let mouse_held    = ui.input().mouse_held(winit::event::MouseButton::Left);
    let mouse_clicked = ui.input().mouse_pressed(winit::event::MouseButton::Left);

    let radius = ui.scale(12.0).dup4();

    data.tooltip_text.clear();


    let clay = magic(ui).clay();
    clay.set_layout_dimensions((window_w as f32, window_h as f32).into());
    clay.pointer_state(mouse_pos.into(), mouse_held);

    clay.set_measure_text_function_user_data(ui.draw(), |string, text_config, draw| {

        let cache_key = (text_config.font_id as u32, text_config.font_size as u32, string.to_string());
        TEXT_MEASURE_CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            if cache.len() > 4096 {
                cache.clear();
            }
            let w = cache.entry(cache_key).or_insert_with(|| {
                let font_kind = match text_config.font_id { 0 => FontKind::Normal, 1 => FontKind::Mono, 2 => FontKind::Icons, _ => todo!() };
                let h = text_config.font_size as f32;

                if h <= 0.0 || !h.is_normal() { return 0.0; }
                let h_norm = h.min(8192.0);
                if h_norm < 3.0 {
                    let factor = match font_kind {
                        FontKind::Normal => 1.0,
                        FontKind::Mono   => 4.0,
                        FontKind::Icons  => 2.0,
                    };
                    return (factor * h_norm * string.len() as f32) / 3.0;
                }

                let text_height_px = h_norm.floor() as usize;
                if let Some(&width) = draw.measure_cache.get(&MeasureKey { font_kind, text_height: text_height_px, text_content: string.to_string() }) {
                    return width;
                }

                let (tracker, _) = draw._find_or_create_font_tracker(text_height_px, font_kind);
                let mut buf = UnicodeBuffer::new();
                buf.set_direction(rustybuzz::Direction::LeftToRight);
                buf.push_str(string);
                let shaped = shape(&tracker.cached_rusty_buzz, &[], buf);
                shaped.glyph_positions().iter().map(|g_pos| {
                    let px_advance = ((g_pos.x_advance as f32 / tracker.units_per_em) * tracker.ppem).ceil() as usize;
                    px_advance.min(1usize << tracker.glyph_row_shift)
                }).sum::<usize>() as f32
            });
            clay::math::Dimensions::new(*w, text_config.font_size as f32)
        })
    });

    let mut c = clay.begin::<(), ()>();

    unsafe { clay::Clay_SetCurrentContext(c.clay.context); }

    // c.set_debug_mode(true);

    if let _ = elem().decl(Decl {
        id: id("Main"),
        padding: (0.0, 0.0, padding.2, padding.3), child_gap,
        width: grow!(),
        height: grow!(),
        ..Decl
    }) {

        let pane_pct_left  = Sizing::Percent(PANE_PERCENT_LEFT);
        let pane_pct_right = Sizing::Percent(PANE_PERCENT_RIGHT);

        if let _elem = elem().decl(Decl {
            id: id("Left Pane"),
            direction: TopToBottom,
            width: pane_pct_left,
            height: grow!(),
            clip: ClipX,
            ..Decl
        }) {

            let id = _elem.decl.id;
            if ui.hovered(id) {
                ui.capture = true;
            }

            let mut pane_tab_l = ui.pane_tab_l;
            ui_left_pane(ui, wallet_state.clone(), data, viz, child_gap, padding, radius, &mut pane_tab_l);
            ui.pane_tab_l = pane_tab_l;
        }

        if let _elem = elem().decl(Decl {
            id: id("Central Gap"),
            radius, padding, child_gap,
            direction: TopToBottom,
            width: grow!(),
            height: grow!(),
            ..Decl
        }) {
            if let _elem = elem().decl(Decl {
                id: id("Readability Box"),
                direction: LeftToRight,
                align: Top,
                width: grow!(),
                height: fit!(),
                ..Decl
            }) {
                let _ = elem().decl(Decl { width: grow!(), ..Decl });
                if let _ = elem().decl(Decl {
                    radius,
                    padding: padding.mul(0.5),
                    // child_gap,
                    align: Top,
                    direction: TopToBottom,
                    width: fit!(),
                    colour: (0, 0, 0, 127),
                    ..Decl
                }) {
                    let decl = TextDecl { h: ui.scale(16.0), align: AlignX::Center, ..TextDecl };

                    if ui.debug {
                        ui.text("------------------", decl);
                        for peer in &viz.peer_strings {
                            ui.text(peer, decl);
                        }
                    }
                    if ui.viz_op != InteractiveVizOp::None {
                        ui.text(frame_strf!(data, "Visualizing op: {:?}", ui.viz_op), decl);
                    }
                }

                if let _ = elem().decl(Decl {
                    width: grow!(),
                    align: TopRight,
                    padding: (0.0, ui.scale(48.0), 0.0, 0.0),
                    direction: LeftToRight,
                    child_gap: child_gap * 0.5,
                    ..Decl
                }) {
                    let (wallets_sync_h, wallets_tip_h) = {
                        let state = wallet_state.lock().unwrap();
                        (state.wallets_sync_h, state.wallets_tip_h)
                    };
                    let peer_count = viz.peer_strings.len();
                    let behind = wallets_tip_h.saturating_sub(wallets_sync_h);
                    let sync_pct = if wallets_tip_h > 0 {
                        100.0 * (wallets_sync_h as f64 / wallets_tip_h as f64)
                    } else {
                        0.0
                    };
                    let (health_label, health_colour, summary) = if peer_count == 0 {
                        ("Disconnected", (0xbf, 0x36, 0x39, 0xff), "No peers connected.")
                    } else if wallets_tip_h > 0 && behind > 3 {
                        ("Syncing", (0xa8, 0x72, 0x13, 0xff), "Connected and catching up.")
                    } else {
                        ("Healthy", (0x2d, 0x8a, 0x57, 0xff), "Connected and near chain tip.")
                    };

                    if let _ = elem().decl(Decl {
                        direction: TopToBottom,
                        align: TopRight,
                        child_gap: ui.scale(8.0),
                        width: fit!(),
                        height: fit!(),
                        ..Decl
                    }) {
                        let combo_id = id("Network Health Combo");
                        let base = BUTTON_GREY.mul(0.85);
                        let (activated, colour, text_colour) = ui.button_ex(true, base, combo_id, true, winit::window::CursorIcon::Pointer);
                        if let _ = elem().decl(Decl {
                            id: combo_id,
                            colour,
                            radius: ui.scale(18.0).dup4(),
                            child_gap: ui.scale(8.0),
                            padding: (ui.scale(8.0), ui.scale(14.0), ui.scale(8.0), ui.scale(14.0)),
                            direction: LeftToRight,
                            align: Center,
                            width: fit!(),
                            height: fit!(),
                            ..Decl
                        }) {
                            if let _ = elem().decl(Decl {
                                colour: health_colour,
                                radius: ui.scale(8.0).dup4(),
                                width: fixed!(ui.scale(16.0)),
                                height: fixed!(ui.scale(16.0)),
                                ..Decl
                            }) {}
                            ui.text("Network", TextDecl { h: ui.scale(16.0), colour: WHITE.mul(0.75), align: AlignX::Left, ..TextDecl });
                            ui.text(health_label, TextDecl { h: ui.scale(16.0), colour: text_colour, align: AlignX::Left, ..TextDecl });
                            ui.text(ICON_DOWN_OPEN, TextDecl { font: Icons, h: ui.scale(14.0), colour: WHITE.mul(0.6), align: AlignX::Right, ..TextDecl });
                        }
                        if ui.hovered(combo_id) {
                            set_tooltip_text!(data, "{} Click to expand.", summary);
                        }

                        if ui.openable(data, combo_id, activated, false) {
                            if let _ = elem().decl(Decl {
                                colour: BUTTON_GREY.mul(0.90),
                                radius: ui.scale(16.0).dup4(),
                                padding: (ui.scale(12.0), ui.scale(16.0), ui.scale(12.0), ui.scale(16.0)),
                                child_gap: ui.scale(10.0),
                                direction: TopToBottom,
                                width: fit!(ui.scale(400.0)),
                                height: fit!(),
                                align: TopRight,
                                ..Decl
                            }) {
                                let row = |ui: &mut Context, data: &mut UiData, label: &str, value: &str| {
                                    if let _ = elem().decl(Decl {
                                        direction: LeftToRight,
                                        width: grow!(),
                                        height: fit!(),
                                        ..Decl
                                    }) {
                                        ui.text(label, TextDecl { h: ui.scale(15.0), colour: WHITE.mul(0.78), align: AlignX::Left, ..TextDecl });
                                        let _ = elem().decl(Decl { width: grow!(), ..Decl });
                                        ui.text(frame_strf!(data, "{}", value), TextDecl { font: Mono, h: ui.scale(15.0), align: AlignX::Right, ..TextDecl });
                                    }
                                };
                                if let _ = elem().decl(Decl {
                                    colour: BUTTON_GREY.mul(0.75),
                                    radius: ui.scale(12.0).dup4(),
                                    padding: (ui.scale(10.0), ui.scale(12.0), ui.scale(10.0), ui.scale(12.0)),
                                    child_gap: ui.scale(8.0),
                                    direction: TopToBottom,
                                    width: grow!(),
                                    height: fit!(),
                                    ..Decl
                                }) {
                                    if let _ = elem().decl(Decl {
                                        direction: LeftToRight,
                                        child_gap: ui.scale(8.0),
                                        width: grow!(),
                                        height: fit!(),
                                        ..Decl
                                    }) {
                                        if let _ = elem().decl(Decl {
                                            colour: health_colour,
                                            radius: ui.scale(6.0).dup4(),
                                            width: fixed!(ui.scale(12.0)),
                                            height: fixed!(ui.scale(12.0)),
                                            ..Decl
                                        }) {}
                                        ui.text(health_label, TextDecl { h: ui.scale(17.0), colour: WHITE, align: AlignX::Left, ..TextDecl });
                                    }
                                    ui.text(summary, TextDecl { h: ui.scale(14.0), colour: WHITE.mul(0.76), align: AlignX::Left, ..TextDecl });
                                }

                                if let _ = elem().decl(Decl {
                                    colour: BUTTON_GREY.mul(0.72),
                                    radius: ui.scale(12.0).dup4(),
                                    padding: (ui.scale(10.0), ui.scale(12.0), ui.scale(10.0), ui.scale(12.0)),
                                    child_gap: ui.scale(6.0),
                                    direction: TopToBottom,
                                    width: grow!(),
                                    height: fit!(),
                                    ..Decl
                                }) {
                                    ui.text("Live Network", TextDecl { h: ui.scale(14.0), colour: WHITE.mul(0.70), align: AlignX::Left, ..TextDecl });
                                    row(ui, data, "PoS Height", &format!("{}", viz.bft_tip_height));
                                    row(ui, data, "PoW Height", &format!("{}", viz.bc_tip_height));
                                    row(ui, data, "PoW Finalized", &format!("{}", viz.bc_finalized_tip_height));
                                    row(ui, data, "Peers", &format!("{}", peer_count));
                                    row(ui, data, "Wallet Sync", &format!("{}/{} ({:.1}%)", wallets_sync_h, wallets_tip_h, sync_pct));
                                    row(ui, data, "Blocks Behind", &format!("{}", behind));
                                }

                                if let _ = elem().decl(Decl {
                                    colour: BUTTON_GREY.mul(0.68),
                                    radius: ui.scale(12.0).dup4(),
                                    padding: (ui.scale(10.0), ui.scale(12.0), ui.scale(10.0), ui.scale(12.0)),
                                    child_gap: ui.scale(6.0),
                                    direction: TopToBottom,
                                    width: grow!(),
                                    height: fit!(),
                                    ..Decl
                                }) {
                                    ui.text("Pool Balances", TextDecl { h: ui.scale(14.0), colour: WHITE.mul(0.70), align: AlignX::Left, ..TextDecl });
                                    row(ui, data, "Orchard", &format!("{}", viz.orchard_pool_balance));
                                    row(ui, data, "Staking Bonded", &format!("{}", viz.staking_bonded_pool_balance));
                                    row(ui, data, "Staking Unbonded", &format!("{}", viz.staking_unbonded_pool_balance));
                                }
                            }
                        }

                        let mining_label = "MINING";
                        let mining_id = id(mining_label);
                        let actively_mining: bool = *wallet::GUI_ENABLE_MINE.lock().unwrap();
                        let label = ["NOT MINING", "MINING"][actively_mining as usize];
                        let (clicked, colour, text_colour) = ui.button_ex(true, BUTTON_GREY, mining_id, true, winit::window::CursorIcon::Pointer);
                        let radius = ui.scale(16.0);
                        if let _ = elem().decl(Decl {
                            id: mining_id,
                            colour,
                            padding,
                            child_gap,
                            radius: radius.dup4(),
                            align: Center,
                            width:  fit!(ui.scale(128.0)),
                            height: fit!(radius * 2.0),
                            ..Decl
                        }) {
                            ui.text(label, TextDecl { font: Mono, h: ui.scale(16.0), colour: text_colour, align: AlignX::Center, ..TextDecl });
                        }
                        if clicked {
                            *wallet::GUI_ENABLE_MINE.lock().unwrap() = !actively_mining;
                        }
                    }

                    if let _ = elem().decl(Decl {
                        direction: TopToBottom,
                        align: TopRight,
                        child_gap: ui.scale(6.0),
                        width: fit!(),
                        height: fit!(),
                        ..Decl
                    }) {
                        let mute_id = id("Mute Toggle");
                        let (clicked, colour, _) = ui.button_ex(true, BUTTON_GREY.mul(0.9), mute_id, true, winit::window::CursorIcon::Pointer);

                        let icon = if ui.global_audio_volume > 0.01 { ICON_VOLUME_HIGH } else { ICON_VOLUME_OFF_1 };
                        if let _ = elem().decl(Decl {
                            id: mute_id,
                            colour,
                            radius: ui.scale(16.0).dup4(),
                            padding: (ui.scale(8.0), ui.scale(10.0), ui.scale(8.0), ui.scale(10.0)),
                            child_gap,
                            align: Center,
                            direction: TopToBottom,
                            width: fit!(),
                            height: fit!(),
                            ..Decl
                        }) {
                            ui.text(icon, TextDecl { font: Icons, colour: WHITE.mul(0.95), h: ui.scale(32.0), align: AlignX::Center, ..TextDecl });
                        }

                        if clicked {
                            if ui.global_audio_volume > 0.01 { ui.global_audio_volume = 0.0 } else { ui.global_audio_volume = 1.0 };
                        }

                        if let _ = elem().decl(Decl {
                            colour: BUTTON_GREY.mul(0.9),
                            radius: ui.scale(12.0).dup4(),
                            padding: (ui.scale(7.0), ui.scale(8.0), ui.scale(7.0), ui.scale(8.0)),
                            child_gap: ui.scale(5.0),
                            direction: TopToBottom,
                            align: Center,
                            width: fit!(ui.scale(42.0)),
                            height: fit!(),
                            ..Decl
                        }) {
                            let volume_pct = (ui.global_audio_volume * 100.0).round() as u32;
                            ui.text(frame_strf!(data, "{}%", volume_pct), TextDecl { h: ui.scale(12.0), align: AlignX::Center, colour: WHITE.mul(0.8), ..TextDecl });
                            if let _ = elem().decl(Decl {
                                direction: TopToBottom,
                                align: Center,
                                child_gap: ui.scale(1.0),
                                width: fit!(),
                                height: fit!(),
                                ..Decl
                            }) {
                                let steps = 22usize;
                                let handle_idx = ((ui.global_audio_volume * (steps - 1) as f32).round() as usize).min(steps - 1);
                                let handle_id = id("Volume Slider Handle");
                                if ui.mouse_pressed_id == handle_id {
                                    ui.global_audio_volume = (ui.global_audio_volume - ui.input().mouse_delta().1 as f32 / ui.scale(140.0)).clamp(0.0, 1.0);
                                }
                                for i in (0..steps).rev() {
                                    let step_id = id_index("Volume Slider Step", i as u32);
                                    let step_level = i as f32 / (steps - 1) as f32;
                                    let active = step_level <= ui.global_audio_volume + 0.001;
                                    let is_handle = i == handle_idx;
                                    let col = if is_handle {
                                        WHITE.mul(0.92)
                                    } else if active {
                                        BUTTON_BLUE.mul(0.95)
                                    } else {
                                        BUTTON_GREY.mul(0.68)
                                    };
                                    let draw_id = if is_handle { handle_id } else { step_id };
                                    let (step_clicked, col, _) = ui.button_ex(true, col, draw_id, true, winit::window::CursorIcon::Pointer);
                                    if let _ = elem().decl(Decl {
                                        id: draw_id,
                                        colour: col,
                                        radius: ui.scale(3.0).dup4(),
                                        width: fixed!(if is_handle { ui.scale(14.0) } else { ui.scale(8.0) }),
                                        height: fixed!(if is_handle { ui.scale(7.0) } else { ui.scale(4.0) }),
                                        ..Decl
                                    }) {}
                                    if step_clicked {
                                        ui.global_audio_volume = step_level;
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // spacer
            if let _ = elem().decl(Decl { height: grow!(), ..Decl }) {}

            // "Reset View" button
            if let _ = elem().decl(Decl { align: Bottom, width: grow!(), ..Decl }) {
                let label = "Reset View";

                let enabled = viz.camera_x != 0.0 || viz.camera_y != viz.bc_tip_y || viz.zoom != 0.0;

                let id = id(label);
                let (clicked, colour, text_colour) = ui.button_ex(true, BUTTON_GREY, id, enabled, winit::window::CursorIcon::Default);
                let radius = ui.scale(20.0);

                if ui.hovered(id) {
                    ui.capture = true;
                }

                // Button
                if let _ = elem().decl(Decl {
                    id,
                    colour,
                    padding,
                    child_gap,
                    radius: radius.dup4(),
                    align: Center,
                    width:  fit!(ui.scale(128.0)),
                    height: fit!(radius * 2.0),
                    ..Decl
                }) {
                    let button_text_h = ui.scale(16.0);
                    ui.text(label, TextDecl { h: button_text_h, colour: text_colour, align: AlignX::Center, ..TextDecl });
                }

                if clicked {
                    viz.camera_x = 0.0;
                    viz.camera_y = viz.bc_tip_y;
                    viz.zoom = 0.0;
                }
            }
        }

        if let _elem = elem().decl(Decl {
            id: id("Right Pane"),
            direction: TopToBottom,
            width: pane_pct_right,
            height: grow!(),
            clip: ClipX,
            ..Decl
        }) {
            let id = _elem.decl.id;
            if ui.hovered(id) {
                ui.capture = true;
            }

            let mut dummy_1 = ui.dummy_1;
            let mut dummy_2 = ui.dummy_2;
            ui_right_pane(ui, wallet_state.clone(), viz, data, child_gap, padding, radius, &mut dummy_1, &mut dummy_2);
            ui.dummy_1 = dummy_1;
            ui.dummy_2 = dummy_2;
        }
    }

    if false { // @Todo: this causes a Clay crash right now, so disable it until that's fixed.
    // if viz.inspecting_block_hash != Hash32::from_u64(0) {
        let ctx_menu_pos = (viz.inspecting_block_screen_x, viz.inspecting_block_screen_y);
        let id = id("Block Inspector Contents");
        if ui.hovered(id) {
            ui.capture = true;
        }
        let border_colour = { let mut col = PANE_COL.hsva(); col.2 = 0x18; col.rgba() };
        if let _ = elem().decl(Decl {
            id,
            colour: border_colour,
            child_gap, padding, radius,
            width:  fit!(),
            height: fit!(),
            floating: Floating::Root(ctx_menu_pos.0 as f32, ctx_menu_pos.1 as f32),
            ..Decl
        }) {
            if let _ = elem().decl(Decl {
                colour: PANE_COL,
                child_gap, padding, radius,
                width:  fit!(ui.scale(192.0), ui.draw().window_width as f32 * 0.5),
                height: fit!(ui.scale(128.0)),
                direction: TopToBottom,
                ..Decl
            }) {
                let text_h = ui.scale(10.0);
                // Block Inspector Contents
                ui.text(frame_strf!(data, "Block: {}", viz.inspecting_block_hash), TextDecl { font: Mono, wrap: Wrap::Chars, h: text_h, align: AlignX::Left, ..TextDecl });

                let text = {
                    if let Some(text) = viz.inspect_block_json_text.as_ref() {
                        text.to_string()
                    } else {
                        frame_strf!(data, "Loading info for block {}...", viz.inspecting_block_hash).to_string()
                    }
                };
                ui.text(frame_strf!(data, "{}", text), TextDecl { font: Mono, wrap: Wrap::Chars, h: text_h, align: AlignX::Left, ..TextDecl });
            }
        }
    }


    let tooltip_id = id("Tooltip Floating Pane");

    if data.tooltip_text.len() > 0 {
        let mouse_pos = ui.input().mouse_pos();
        let tooltip_pos =
            ((mouse_pos.0 as f32 + 8.0 * ui.dpi_scale).min(window_w - ui.tooltip_w),
             (mouse_pos.1 as f32 + 6.0 * ui.dpi_scale).min(window_h - ui.tooltip_h));

        if let _ = elem().decl(Decl {
            id: tooltip_id,
            colour: PANE_COL,
            child_gap, padding, radius,
            width:  fit!(),
            height: fit!(),
            floating: Floating::Root(tooltip_pos.0, tooltip_pos.1),
            ..Decl
        }) {
            ui.text(&data.tooltip_text, TextDecl { h: ui.scale(16.0), align: AlignX::Left, ..TextDecl });
        }
    }

    if !ui.input().mouse_held(winit::event::MouseButton::Left) {
        ui.mouse_pressed_id = Id::default();
    }
    if ui.mouse_pressed_id == Id::default() {
        if ui.input().mouse_pressed(winit::event::MouseButton::Left) {
            // ui.nav_enable = false;
        }
    } else {
        // ui.most_recent_mouse_pressed_id = ui.mouse_pressed_id;
        if ui.nav_id != ui.mouse_pressed_id.id {
            ui.nav_enable = false;
        }
        ui.nav_id = ui.mouse_pressed_id.id;
        if ui.mouse_pressed_id != Id::VIZ_GUI
            && ui.mouse_pressed_id != Id::CHAIN_MINIMAP_POW
            && ui.mouse_pressed_id != Id::CHAIN_MINIMAP_POS
        {
            ui.capture = true;
        }
    }

    if !ui.nav_id_to_idx.contains_key(&ui.nav_id) {
        ui.nav_id = 0;
        ui.nav_enable = false;
    }
    // if ui.input().key_pressed(KeyCode::Escape) {
    //     ui.nav_enable = false;
    // }

    if ui.input().key_pressed(KeyCode::Tab) && !ctrl_held && ui.nav_idx_to_id.len() > 0 {
        let idx = if ui.nav_id != 0 {
            let old_idx = ui.nav_id_to_idx[&ui.nav_id] as isize;
            if shift_held {
                (old_idx - 1).rem_euclid(ui.nav_idx_to_id.len() as isize) as usize
            } else {
                (old_idx + 1).rem_euclid(ui.nav_idx_to_id.len() as isize) as usize
            }
        } else {
            0
        };
        ui.nav_id = ui.nav_idx_to_id[idx];
        ui.nav_enable = true;

        if let Some(textbox_state) = data.textboxes.get_mut(&ui.nav_id) {
            textbox_state.selection.1 = 0;
            textbox_state.selection.0 = textbox_state.text_buf.len();
        }
    }

    // Return the list of render commands of your layout
    let render_commands = c.end();

    {

        let mut nav_bbox: Option<(isize, isize, isize, isize)> = None;

        for mut command in render_commands {
            if let Some(scroll_container_state) = data.scroll_containers.get_mut(&command.id) {
                // @HackBecauseClayRefusesToBoundTheHeightOfTheScrollContainerAgainstOurExplicitRequestForNoKnownReasonButOnlyInProductionAndNotInTheTestingGuiBuild
                if command.bounding_box.height > window_h - command.bounding_box.y {
                    command.bounding_box.height = window_h - command.bounding_box.y;
                }

                scroll_container_state.viewport_height = command.bounding_box.height.min(window_h);
            }

            let x1 = (command.bounding_box.x)                               as isize;
            let y1 = (command.bounding_box.y)                               as isize;
            let x2 = (command.bounding_box.x + command.bounding_box.width)  as isize;
            let y2 = (command.bounding_box.y + command.bounding_box.height) as isize;

            if command.id == tooltip_id.id {
                ui.tooltip_w = command.bounding_box.width  as f32;
                ui.tooltip_h = command.bounding_box.height as f32;
            }

            if !is_rendering {
                continue;
            }

            let bbox = (x1, y1, x2, y2);

            if ui.nav_id == command.id {
                nav_bbox = Some(bbox);
            }

            if unsafe { clay::Clay_PointerOver(Id { id: command.id, ..Id }.clay().id) } {
                ui.hovered_id = Id { id: command.id, ..Id };
            }

            fn clay_color_to_u32(color: clay::Color) -> u32 {
                let r = color.r as u32;
                let g = color.g as u32;
                let b = color.b as u32;
                let a = color.a as u32;
                let color = (a << 24) | (r << 16) | (g << 8) | b;
                color
            }

            let colour = match command.config {
                Rectangle(ref config)                 => { clay_color_to_u32(config.color) }
                RenderCommandConfig::Text(ref config) => { clay_color_to_u32(config.color) }
                _ => { 0 }
            };

            let draw_debug_rect = |ui: &mut Context| {
                if ui.debug {
                    let thickness = 2.0;
                    let color = 0x80ff00ff;
                    let t  = (thickness / 2.0) as isize;

                    ui.draw().rectangle((x1-t) as f32, (y1-t) as f32, (x1+t) as f32, (y2+t) as f32, color);
                    ui.draw().rectangle((x2-t) as f32, (y1-t) as f32, (x2+t) as f32, (y2+t) as f32, color);
                    ui.draw().rectangle((x1-t) as f32, (y1-t) as f32, (x2-t) as f32, (y1+t) as f32, color);
                    ui.draw().rectangle((x1-t) as f32, (y2-t) as f32, (x2-t) as f32, (y2+t) as f32, color);
                }
            };

            // @Hack for generating nav highlight rectangle via draw commands.
            if colour == 0x01000000 {
                draw_debug_rect(ui);
                continue;
            }

            match command.config {
                Rectangle(config) => {
                    let radius_tl = config.corner_radii.top_left     as isize;
                    let radius_tr = config.corner_radii.top_right    as isize;
                    let radius_bl = config.corner_radii.bottom_left  as isize;
                    let radius_br = config.corner_radii.bottom_right as isize;
                    ui.draw().rounded_rectangle(x1, y1, x2, y2,
                                                radius_tl,
                                                radius_tr,
                                                radius_bl,
                                                radius_br,
                                                colour);
                }
                RenderCommandConfig::Text(config) => {
                    let font_kind = match config.font_id { 0 => FontKind::Normal, 1 => FontKind::Mono, 2 => FontKind::Icons, _ => todo!() };

                    // let text = frame_strf!(data, "{} ", command.id);
                    // ui.draw().text_line(font_kind, x1 as f32, y1 as f32, config.font_size as f32, text, colour);

                    ui.draw().text_line(font_kind, x1 as f32, y1 as f32, config.font_size as f32, config.text, colour);
                }
                ScissorStart() => {
                    ui.draw().set_scissor(x1, y1, x2, y2);
                }
                ScissorEnd() => {
                    ui.draw().clear_scissor();
                }
                misc => { todo!("Unsupported clay render command: {:?}", misc) }
            }

            if let Some(textbox_state) = data.textboxes.get_mut(&command.id) && ui.nav_id == command.id {
                textbox_state.bbox = bbox;

                let h = textbox_state.h;

                let padding = ui.scale(12.0); // @Duplicate :TextBox

                let head_str = {
                    let n = textbox_state.selection.0.min(textbox_state.text_buf.len());
                    let mut s = String::with_capacity(n * 4);
                    s.extend(textbox_state.text_buf[..n].iter().copied());
                    s
                };
                let tail_str = {
                    let n = textbox_state.selection.1.min(textbox_state.text_buf.len());
                    let mut s = String::with_capacity(n * 4);
                    s.extend(textbox_state.text_buf[..n].iter().copied());
                    s
                };

                let head_xoff = ui.draw_mut().measure_text_line(textbox_state.font, h, &head_str);
                let tail_xoff = ui.draw_mut().measure_text_line(textbox_state.font, h, &tail_str);
                let head_x = x1 + padding as isize + head_xoff as isize;
                let tail_x = x1 + padding as isize + tail_xoff as isize;

                let y_centre = (y1 + y2) / 2;
                let y1 = y_centre - (h * 0.5).ceil() as isize;
                let y2 = y_centre + (h * 0.5).ceil() as isize;

                if textbox_state.selection.1 == textbox_state.selection.0 {
                    let x1    = head_x;
                    let color = 0xc0ffffff;
                    let t     = (ui.scale * 1.0).ceil() as isize;

                    ui.draw().rectangle((x1-t) as f32, (y1-t) as f32, (x1+t) as f32, (y2+t) as f32, color);
                } else {
                    let (min, max) = (head_x.min(tail_x),
                                      head_x.max(tail_x));

                    let x1    = min;
                    let x2    = max;
                    let color = 0xc00000ff;

                    ui.draw().rectangle(x1 as f32, y1 as f32, x2 as f32, y2 as f32, color);
                }
            }

            draw_debug_rect(ui);
        }

        if ui.nav_enable && let Some((x1, y1, x2, y2)) = nav_bbox && !data.textboxes.contains_key(&ui.nav_id) {
            let gap = ui.scale(1.0) as isize;

            let x1 = x1 - gap;
            let y1 = y1 - gap;
            let x2 = x2 + gap;
            let y2 = y2 + gap;

            let black_t = ui.scale(2.0) as isize;
            let white_t = black_t * 2;

            {
                let color = 0xffffffff;
                let t = white_t;
                ui.draw().rectangle((x1-t) as f32, (y1-t) as f32, (x1+0) as f32, (y2+t) as f32, color);
                ui.draw().rectangle((x2-0) as f32, (y1-t) as f32, (x2+t) as f32, (y2+t) as f32, color);
                ui.draw().rectangle((x1-t) as f32, (y1-t) as f32, (x2-0) as f32, (y1+0) as f32, color);
                ui.draw().rectangle((x1-t) as f32, (y2-0) as f32, (x2-0) as f32, (y2+t) as f32, color);
            }
            {
                let color = 0xff000000;
                let t = black_t;
                ui.draw().rectangle((x1-t) as f32, (y1-t) as f32, (x1+0) as f32, (y2+t) as f32, color);
                ui.draw().rectangle((x2-0) as f32, (y1-t) as f32, (x2+t) as f32, (y2+t) as f32, color);
                ui.draw().rectangle((x1-t) as f32, (y1-t) as f32, (x2-0) as f32, (y1+0) as f32, color);
                ui.draw().rectangle((x1-t) as f32, (y2-0) as f32, (x2-0) as f32, (y2+t) as f32, color);
            }
        }
    }

    result |= dbg_ui(ui, is_rendering);

    result
}

pub fn ui_update(ui: &mut Context, data: &mut UiData, viz: &mut VizState, wallet_state: Arc<Mutex<WalletState>>) -> bool {
    viz.ui_hovered_height = None; // @todo(judah): not sure where to reset this

    ui.tx_loading_animation_timer += ui.delta;
    if ui.tx_loading_animation_timer >= 1.0 {
        ui.tx_loading_animation_timer = 0.0;
    }

    let dummy_input = InputCtx {
        this_mouse_pos: ui.input().this_mouse_pos,
        last_mouse_pos: ui.input().this_mouse_pos,

        mouse_down: ui.input().mouse_down,
        keys_down1: ui.input().keys_down1,
        keys_down2: ui.input().keys_down2,

        ..Default::default()
    };
    let real_input = ui.input; let result =           run_ui(ui, wallet_state.clone(), data, viz, false);
    ui.input = &dummy_input;   let result = result || run_ui(ui, wallet_state.clone(), data, viz, true);
    ui.input =   real_input;
    return result;
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Context {
    pub input: *const InputCtx,
    pub draw:  *const DrawCtx,
    pub clay:  *mut   Clay,

    pub global_audio_volume: f32,

    pub cursor: winit::window::Cursor,
    pub prev_cursor: winit::window::Cursor,

    pub debug: bool,
    pub pixel_inspector_primed: bool,

    pub draw_commands: Vec<DrawCommand>,

    pub scale:     f32,
    pub zoom:      f32,
    pub dpi_scale: f32,
    pub delta:     f32,

    pub capture: bool,

    pub suppress_scroll_for_clay: bool,

    pub mouse_pressed_id:             Id,
    pub key_pressed_id:               Id,
    // pub most_recent_mouse_pressed_id: Id,

    pub hovered_id: Id,

    pub tooltip_w: f32,
    pub tooltip_h: f32,

    pub nav_id_to_idx: HashMap<u32, usize>,
    pub nav_idx_to_id: Vec<u32>,
    pub nav_id: u32,
    pub nav_enable: bool,
    pub nav_skip: bool,

    pub viz_op: InteractiveVizOp,

    pub pane_tab_l: Id,
    pub dummy_1: Id,
    pub dummy_2: Id,

    pub modal: Modal,
    pub retarget_modal_bond_key: [u8; 32],

    pub tx_loading_animation_timer: f32,
}

#[derive(Debug, Default, Copy, Clone)]
pub struct Font(u64);
