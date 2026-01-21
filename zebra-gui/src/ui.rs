#![allow(warnings)]

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
//use clay::*; // @Temporary

use wallet::{ BlockHeight, WalletState, WalletTxKind, str_from_ctaz };

use super::*;

pub fn magic<'a, 'b, T>(mut_ref: &'a mut T) -> &'b mut T {
    let mut_ref = mut_ref as *mut T;
    return unsafe { &mut *mut_ref };
}

#[derive(Debug, Default)]
pub struct UiData {
    pub per_frame_strs: Vec<String>,

    pub send_address:  String,
    pub stake_address: String,
    pub recv_address:  String,

    pub textboxes: HashMap<u32, TextboxState>,
    pub scroll_containers: HashMap<u32, ScrollContainerState>,

    pub openables: HashMap<u32, OpenableState>,
}


#[macro_export]
macro_rules! frame_strf {
    ($data:expr, $($arg:tt)*) => {
        $data.frame_str(&format_args!($($arg)*).to_string())
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
#[derive(Debug, Copy, Clone, PartialEq, PartialOrd, Default)]                pub enum ClipMode  { #[default] None, Clip, Scroll(f32, f32) }
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

use ClipMode::{Clip, Scroll};
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
pub fn elem() -> Element { unsafe { clay::Clay__OpenElement(); } Element::default() }
impl Drop for Element { fn drop(&mut self) { unsafe { clay::Clay__CloseElement(); } } }

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

impl Element {
    fn decl(&mut self, item: Decl) -> &mut Self {
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
        let clipping = match item.clip { ClipMode::None => false, _ => true };
        decl.clip = clay::Clay_ClipElementConfig {
            horizontal: clipping,
            vertical: clipping,
            childOffset: match item.clip {
                ClipMode::None => clay::Clay_Vector2 { x: 0.0, y: 0.0 },
                ClipMode::Clip => clay::Clay_Vector2 { x: 0.0, y: 0.0 },
                ClipMode::Scroll(x, y) => clay::Clay_Vector2 { x, y },
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
            },
            Floating::Root(x, y) => {
                decl.floating.attachTo = clay::Clay_FloatingAttachToElement_CLAY_ATTACH_TO_ROOT;
                decl.floating.offset.x = x;
                decl.floating.offset.y = y;
                decl.floating.attachPoints = clay::Clay_FloatingAttachPoints {
                    element: clay::Clay_FloatingAttachPointType_CLAY_ATTACH_POINT_LEFT_TOP,
                    parent:  clay::Clay_FloatingAttachPointType_CLAY_ATTACH_POINT_LEFT_TOP,
                };
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

        self.decl = item;
        self
    }
}

pub const PANE_PERCENT: f32 = 0.27; // @PreventPanesColliding

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
    Send,
    Receive,
    Stake,
    Unstake,
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
    pub fn new() -> Context { Context { scale: 1f32, zoom: 1f32, dpi_scale: 1f32, ..Default::default() } }
    pub fn draw(&self)  -> &DrawCtx  { unsafe { &*self.draw     } }
    pub fn input(&self) -> &InputCtx { unsafe { &*self.input    } }
    pub fn clay(&self)  -> &mut Clay { unsafe { &mut *self.clay } }

    pub fn scale(&self, size: f32) -> f32 { (size * self.scale).floor() }
    pub fn scale32(&self, size: f32) -> u32 { self.scale(size) as u32 }
    pub fn scale16(&self, size: f32) -> u16 { self.scale(size) as u16 }

    pub fn hovered(&self, id: Id) -> bool { unsafe { clay::Clay_PointerOver(id.clay().id) } }

    pub fn button_ex(&mut self, act_on_press: bool, colour: (u8, u8, u8, u8), id: Id, enabled: bool, pointer_on_hover: winit::window::CursorIcon) -> (bool, (u8, u8, u8, u8), (u8, u8, u8, u8)) {

        let mouse_hover = self.hovered(id);
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

        if mouse_hover && pointer_on_hover != winit::window::CursorIcon::Default {
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
            self.text(label, TextDecl { h: tab_text_h, align: AlignX::Center, ..TextDecl });
        }

        id
    }

    pub fn tab(&mut self,
           radius: (f32, f32, f32, f32),
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

    pub fn scroll_container<'data>(&mut self, data: &'data mut UiData, id: Id, scroll_end_height: f32) -> (Id, ClipMode, &'data mut f32) {
        let mut scroll_container_state = data.scroll_containers.entry(id.id).or_default();

        if self.hovered(id) {
            scroll_container_state.scroll -= self.input().zoom_delta     as f32 * 32.0;
            scroll_container_state.scroll -= self.input().scroll_delta.1 as f32 * 32.0;

            if self.input().mouse_held(MouseButton::Left) {
                scroll_container_state.scroll -= self.input().mouse_delta().1 as f32 / self.scale;
            }
        }

        let scroll_container_data: clay::Clay_ScrollContainerData = unsafe { clay::Clay_GetScrollContainerData(id.clay().id) };
        if scroll_container_data.found {
            let max = scroll_container_data.contentDimensions.height / self.scale - scroll_end_height;
            if scroll_container_state.scroll > max {
                scroll_container_state.scroll = max;
            }
        };
        if scroll_container_state.scroll < 0.0 {
            scroll_container_state.scroll = 0.0;
        }

        (id, Scroll(0.0, -scroll_container_state.scroll * self.scale), &mut scroll_container_state.scroll)
    }

    pub fn openable(&mut self, data: &mut UiData, id: Id, activated: bool) -> bool {
        let mut openable_state = &mut data.openables.entry(id.id).or_default();
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
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ScrollContainerState {
    pub scroll: f32,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct OpenableState {
    pub open: bool,
}

pub fn ui_left_pane(ui: &mut Context,
                wallet_state: Arc<Mutex<WalletState>>,
                data: &mut UiData,
                viz: &mut VizState,
                child_gap: f32,
                padding: (f32, f32, f32, f32),
                radius:  (f32, f32, f32, f32),
                tab_id: &mut Id) {

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
        tab_id_miner_wallet = ui.tab(radius, padding, tab_id, "Miner Wallet");
    }
    ui.nav_skip = false;

    // You can't operate on the miner, so you can't open any modals. // @Todo: maybe you can open Receive?
    if *tab_id == tab_id_miner_wallet {
        ui.modal = Modal::None;
    }

    if ui.modal != Modal::None && let _elem = elem().decl(Decl {
        child_gap,
        id: id("Modal Container"),
        padding: padding.mul(2.0),
        radius: (radius.0, 0.0, radius.2, 0.0),
        floating: Floating::Parent,
        colour: (0, 0, 0, 0xC0),
        align: Center,
        width:  grow!(),
        height: grow!(),
        ..Decl
    }) {

        let container_id = _elem.decl.id;
        let container_hovered = ui.hovered(container_id);

        if container_hovered { ui.capture = true; }

        let mut height = grow!(ui.scale(192.0), ui.scale(384.0));
        if ui.modal == Modal::Stake {
            height = fit!();
        }

        if let _elem = elem().decl(Decl {
            child_gap, radius,
            id: id("Modal Contents"),
            padding: padding.mul(2.0),
            colour: MODAL_COL,
            width:  grow!(ui.scale(192.0), ui.scale(384.0)),
            height: height,
            align: Top,
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

            let mut button_ex = |ui: &mut Context, label, enabled: bool| {
                let id = id(label);
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

            match ui.modal {
                Modal::None => {}
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

                        data.send_address = ui.textbox(data, id("Send Address Textbox"), "Destination address...", TextDecl {
                            h: ui.scale(16.0),
                            ..TextDecl
                        });

                        // spacer
                        if let _ = elem().decl(Decl { width: grow!(), height: fixed!(ui.scale(16.0)), ..Default::default() }) {}

                        if (balance as u64) < ONE_cTAZ / 100 {
                            let colour = (0xff, 0xaf, 0x0e, 0xff);
                            ui.text("Insufficient funds. Try the faucet!", TextDecl { h: ui.scale(20.0), colour, align: AlignX::Center, ..TextDecl });
                        }

                        const ONE_cTAZ: u64 = 100_000_000;

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
                            if button_ex(ui,    "1 cTAZ", can && (balance as u64) >= ONE_cTAZ)       { wallet_state.lock().unwrap().send_to_address(data.send_address.clone(), ONE_cTAZ);       }
                            if button_ex(ui,   "10 cTAZ", can && (balance as u64) >= ONE_cTAZ * 10)  { wallet_state.lock().unwrap().send_to_address(data.send_address.clone(), ONE_cTAZ * 10);  }
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
                            ui.text(frame_strf!(data, "[{}..{}]", &ua[..8], &ua[ua.len() - 8..]), TextDecl { font: Mono, h: ui.scale(20.0), colour: WHITE, align: AlignX::Center, ..TextDecl });

                            if button_ex(ui, "Copy Address", true) {
                                ui.input().send_to_clipboard(&ua);
                            }
                        } else {
                            ui.text("Loading...", TextDecl { font: Mono, h: ui.scale(20.0), colour: WHITE, align: AlignX::Center, ..TextDecl });
                        }
                    }
                }
                Modal::Stake => {
                    title_bar(ui, true, "Stake", id("Stake Title Bar"));

                    let mut stake_address = "0000000000000000";
                    if data.stake_address.len() >= 16 {
                        stake_address = &data.stake_address;
                    }

                    ui.text(frame_strf!(data, "[{}..{}]", &stake_address[0..8], &stake_address[stake_address.len()-8..]), TextDecl { font: Mono, h: ui.scale(20.0), colour: WHITE, align: AlignX::Center, ..TextDecl });
                    if button_ex(ui, "Paste Address", true) {
                        data.stake_address = ui.input().get_from_clipboard().trim().to_string();
                    }

                    // spacer
                    if let _ = elem().decl(Decl { width: grow!(), height: fixed!(ui.scale(16.0)), ..Default::default() }) {}

                    if (balance as u64) < ONE_cTAZ / 100 {
                        let colour = (0xff, 0xaf, 0x0e, 0xff);
                        ui.text("Insufficient funds. Try the faucet!", TextDecl { h: ui.scale(20.0), colour, align: AlignX::Center, ..TextDecl });
                    }

                    const ONE_cTAZ: u64 = 100_000_000;
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

                        let can = !waiting_for_stake_to_finalizer && hex_dest.is_some();
                        if button_ex(ui, "+0.01 cTAZ", can && (balance as u64) >= ONE_cTAZ / 100) { wallet_state.lock().unwrap().stake_to_finalizer(ONE_cTAZ / 100, hex_dest.unwrap()); }
                        if button_ex(ui,  "+0.1 cTAZ", can && (balance as u64) >= ONE_cTAZ / 10)  { wallet_state.lock().unwrap().stake_to_finalizer(ONE_cTAZ / 10, hex_dest.unwrap());  }
                        if button_ex(ui,    "+1 cTAZ", can && (balance as u64) >= ONE_cTAZ)       { wallet_state.lock().unwrap().stake_to_finalizer(ONE_cTAZ, hex_dest.unwrap());       }
                        if button_ex(ui,   "+10 cTAZ", can && (balance as u64) >= ONE_cTAZ * 10)  { wallet_state.lock().unwrap().stake_to_finalizer(ONE_cTAZ * 10, hex_dest.unwrap());  }
                    }
                }
                Modal::Unstake => {
                    title_bar(ui, true, "Unstake", id("Unstake Title Bar"));

                    fn display_str(chunks: &[u64; 4]) -> String {
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

                        for b in &bytes[0..4] {
                            str.push_str(&format!("{:02x}", b));
                        }
                        str.push_str("..");
                        for b in &bytes[bytes.len() - 4..] {
                            str.push_str(&format!("{:02x}", b));
                        }

                        str
                    }

                    let mut button_ex = |ui: &mut Context, label, act_on_press, enabled: bool| {
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

                    let mut clickable_icon = |ui: &mut Context, id, icon, icon_hovered, enabled | {
                        let (clicked, colour, _) = ui.button_ex(true, (0xcc, 0xcc, 0xcc, 0xff) /* @todo colors */, id, enabled, winit::window::CursorIcon::Pointer);

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
                    };

                    // TODO: phillip audaciously deleted the accumulated bond amount field, bring that back
                    let mut staked_roster = Vec::<(bool /* is_unbonded */, [u8; 32] /* bond key */, [u8; 32] /* target finalizer */, u64 /* initial */)>::new();
                    {
                        let lock = wallet_state.lock().unwrap();
                        for p in &lock.stake_positions_unbonded {
                            staked_roster.push((true, p.0, p.1, p.2));
                        }
                        for p in &lock.stake_positions_bonded {
                            staked_roster.push((false, p.0, p.1, p.2));
                        }
                    }

                    staked_roster.sort_by_key(|x| std::cmp::Reverse((x.0, x.2, x.3)));

                    let (id, mut clip, mut scroll) = ui.scroll_container(data, id("Unstake Scroll Container"), 48.0);
                    if staked_roster.len() == 0 {
                        clip = Scroll(0.0, 0.0);
                        *scroll = 0.0;
                    }
                    if let _ = elem().decl(Decl {
                        id,
                        colour: TRANSACTION_HISTORY_CONTAINER_COL,
                        child_gap: child_gap * 0.5, padding,
                        radius: padding.0.dup4(),
                        width:  percent!(1.0),
                        height: grow!(),
                        // height: percent!(1.0),
                        direction: TopToBottom,
                        clip,
                        align: Top,
                        ..Decl
                    }) {
                        let chunkify = |bytes: &[u8; 32]| {
                            let mut chunks = [0u64; 4];
                            for i in 0..4 {
                                let start = i * 8;
                                let end = start + 8;
                                let mut buf = [0u8; 8];
                                buf.copy_from_slice(&bytes[start..end]);
                                chunks[i] = u64::from_le_bytes(buf);
                            }

                            chunks
                        };

                        if staked_roster.len() == 0 {
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
                        }
                        else {
                            let mut prev_is_unbonded: bool = false;
                            let mut open_is_unbonded: bool = false;
                            let mut prev_finalizer:   [u8; 32] = [0; 32];
                            let mut open_finalizer:   bool = false;

                            for (index, &(is_unbonded, bond_key, finalizer, initial)) in staked_roster.iter().enumerate() {
                                let index: u32 = {
                                    use std::hash::{Hash, Hasher};
                                    let mut h = std::collections::hash_map::DefaultHasher::new();
                                    bond_key.hash(&mut h);
                                    index.hash(&mut h);
                                    h.finish() as u32
                                };

                                if (prev_is_unbonded != is_unbonded) {
                                    prev_is_unbonded  = is_unbonded;
                                    let string = if is_unbonded { "Claimable Bonds" } else { "Active Positions" };

                                    let id = id_index(string, index);

                                    let (activated, colour, text_colour) = ui.button_ex(true, BUTTON_GREY.mul(0.8), id, true, winit::window::CursorIcon::Pointer);

                                    if let _ = elem().decl(Decl {
                                        id,
                                        colour,
                                        padding,
                                        width:  percent!(1.0),
                                        height: fit!(),
                                        align:  Left,
                                        ..Decl
                                    }) {
                                        ui.text(string, TextDecl { colour: text_colour, h: ui.scale(18.0), align: AlignX::Left, ..TextDecl });
                                    }

                                    open_is_unbonded = ui.openable(data, id, activated);
                                }

                                if !open_is_unbonded {
                                    continue;
                                }

                                if (prev_finalizer != finalizer) {
                                    prev_finalizer  = finalizer;

                                    let chunks = chunkify(&finalizer);
                                    let label = frame_strf!(data, "{}", display_str(&chunks));

                                    let id = id_index(label, index);

                                    let (activated, colour, text_colour) = ui.button_ex(true, BUTTON_GREY.mul(0.8), id, true, winit::window::CursorIcon::Pointer);

                                    if let _ = elem().decl(Decl {
                                        id,
                                        colour,
                                        padding,
                                        width:  percent!(0.8),
                                        height: fit!(),
                                        align:  Left,
                                        ..Decl
                                    }) {
                                        ui.text(label, TextDecl { colour: text_colour, h: ui.scale(18.0), align: AlignX::Center, ..TextDecl });
                                    }

                                    open_finalizer = ui.openable(data, id, activated);
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
                                        let (icon, icon_hovered) = if is_unbonded {
                                            (ICON_MONEY, ICON_WALLET)
                                        } else {
                                            (ICON_LINK_1, ICON_UNLINK)
                                        };
                                        if clickable_icon(ui, id_index("Unstake Button", index as u32), icon, icon_hovered, true) {
                                            if is_unbonded {
                                                wallet_state.lock().unwrap().claim_bond(bond_key);
                                            } else {
                                                wallet_state.lock().unwrap().unstake_from_finalizer(bond_key);
                                            }
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
                                        let chunks = chunkify(&finalizer);

                                        ui.text(frame_strf!(data, "{}", display_str(&chunks)), TextDecl { font: Mono, h: ui.scale(14.0), align: AlignX::Left, ..TextDecl });
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
                                        // TODO: let accumulated_stake_amount;
                                        let full = stake_amount / 100_000_000;
                                        let part = stake_amount % 100_000_000;
                                        let part_str = format!("{part}00");
                                        let trim_part = part_str.trim_end_matches("0");

                                        let id = id_index("Unstake Clickable Text", index as u32);
                                        let mut colour = (0xff, 0xaf, 0x0e, 0xff); // @todo color
                                        let mut str = frame_strf!(data, "{}.{} cTAZ", full, &part_str[..trim_part.len().max(3)]);
                                        if ui.hovered(id) {
                                            // TODO: let initial_stake_amount;
                                            let full = stake_amount / 100_000_000;
                                            let part = stake_amount % 100_000_000;
                                            let part_str = format!("{part}00");
                                            let trim_part = part_str.trim_end_matches("0");

                                            colour = WHITE;
                                            str = frame_strf!(data, "{}.{} cTAZ", full, &part_str[..trim_part.len().max(3)]);
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
                    }
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
        clip: Clip,
        ..Decl
    }) {
        let balance_text_h = ui.scale(48.0);
        let accent_text_h  = ui.scale(16.0);

        // spacer
        // if let _ = elem().decl(Decl { width: grow!(), height: fixed!(ui.scale(16.0)), ..Default::default() }) {}

        let (
            balance,
            pending_balance,
            staked_balance,
            show_staked_balance,
        ) = {
            let wallet_state = wallet_state.lock().unwrap();
            if *tab_id == tab_id_user_wallet {(
                wallet_state.user_balance(),
                wallet_state.user_pending_balance(),
                wallet_state.staked_balance,
                wallet_state.show_staked_balance,
            )} else {(
                wallet_state.miner_balance(),
                wallet_state.miner_pending_balance(),
                0u64,
                wallet_state.show_staked_balance,
            )}
        };

        // balance container
        let mut balance = balance;
        let mut colour  = WHITE;
        if show_staked_balance {
            balance = staked_balance;
            colour  = (0xff, 0xaf, 0x0e, 0xff);
        }

        let balance_id = id("Balance Text");
        let (clicked, colour, _) = ui.button_ex(true, colour, balance_id, true, winit::window::CursorIcon::Pointer);
        if clicked {
            wallet_state.lock().unwrap().show_staked_balance = !show_staked_balance;
        }

        if let _ = elem().decl(Decl {
            id: balance_id,
            width: grow!(),
            height: fit!(),
            direction: LeftToRight,
            align: Center,
            ..Decl
        }) {
            let suffix = if !show_staked_balance && staked_balance > 0 { "*" } else { "" };
            let balance_str = frame_strf!(data, "{} cTAZ{}", str_from_ctaz(balance.try_into().unwrap()), suffix);
            ui.text(&balance_str, TextDecl { colour, h: balance_text_h, align: AlignX::Center, ..TextDecl });
        }

        // pending container
        if let _ = elem().decl(Decl {
            width: grow!(),
            height: fit!(),
            align: Center,
            ..Decl
        }) {
            // let staked_str = frame_strf!(data, "{} cTAZ Staked", str_from_ctaz(staked_balance.try_into().unwrap()));
            // ui.text(&staked_str, TextDecl { h: accent_text_h, align: AlignX::Center, colour: (0x90, 0x90, 0x90, 0xff) /* @todo colors */, ..TextDecl });

            let balance_str = frame_strf!(data, "{} cTAZ Pending", str_from_ctaz(pending_balance.try_into().unwrap()));
            ui.text(&balance_str, TextDecl { h: accent_text_h, align: AlignX::Center, colour: (0x90, 0x90, 0x90, 0xff) /* @todo colors */, ..TextDecl });
        }

        // spacer
        // if let _ = elem().decl(Decl { width: grow!(), height: fixed!(ui.scale(32.0)), ..Default::default() }) {}

        let child_gap = child_gap as f32;
        let padding = child_gap.dup4();

        // buttons container
        if let _ = elem().decl(Decl {
            id: id("Buttons Container"),
            padding, child_gap, align: Center,
            width: grow!(),
            height: fit!(),
            ..Decl
        }) {

            let mut button = |ui: &mut Context, enabled: bool, icon: &'static str, label: &'static str| {
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

            let can = (*tab_id == tab_id_user_wallet);
            if button(ui, can, ICON_UP_BIG, "Send")    { ui.modal = Modal::Send;    }
            if button(ui, can, ICON_QRCODE, "Receive") { ui.modal = Modal::Receive; }
            if button(ui, can, ICON_LINK_1, "Stake")   { ui.modal = Modal::Stake;   }
            if button(ui, can, ICON_UNLINK, "Unstake") { ui.modal = Modal::Unstake; }
        }

        // if let _ = elem().decl(Decl { width: grow!(), height: fixed!(ui.scale(32.0)), ..Default::default() }) {}

        {
            let txs = {
                let state = wallet_state.lock().unwrap();
                if *tab_id == tab_id_user_wallet {
                    state.user_txs.clone()
                } else {
                    state.miner_txs.clone()
                }
            };

            let DOUBLE_ICON_OK_CIRCLED_1  = { (&*frame_strf!(magic(data), "{}{}", ICON_OK_CIRCLED_1,  ICON_OK_CIRCLED_1).as_str()) };
            let DOUBLE_ICON_OK_CIRCLED2_1 = { (&*frame_strf!(magic(data), "{}{}", ICON_OK_CIRCLED2_1, ICON_OK_CIRCLED2_1).as_str()) };

            let (id, mut clip, mut scroll) = ui.scroll_container(data, id("History Scroll Container"), 96.0);
            if txs.len() == 0 {
                clip = Scroll(0.0, 0.0);
                *scroll = 0.0;
            }
            if let _ = elem().decl(Decl {
                id,
                colour: TRANSACTION_HISTORY_CONTAINER_COL,
                child_gap: child_gap * 0.5, padding,
                radius: padding.0.dup4(),
                width:  percent!(1.0),
                height: grow!(),
                // height: percent!(1.0),
                direction: TopToBottom,
                clip,
                align: Top,
                ..Decl
            }) {
                if txs.len() == 0 {
                    let h = ui.scale(24.0);
                    if let _ = elem().decl(Decl {
                        direction: TopToBottom,
                        width:  percent!(1.0),
                        height: percent!(1.0),
                        child_gap,
                        align:  Center,
                        ..Decl
                    }) {
                        ui.text(ICON_DROPBOX_1, TextDecl { font: Icons, colour: WHITE.mul(0.6), h: ui.scale(64.0), align: AlignX::Center, ..TextDecl });
                        ui.text("There are no transactions yet.", TextDecl { colour: WHITE.mul(0.6), h, align: AlignX::Center, ..TextDecl });
                    }
                }
                else {
                    let kind_text_h = ui.scale(18.0);
                    let transaction_text_h = ui.scale(16.0);

                    for (index, tx) in txs.iter().rev().enumerate() {
                        if index > 0 { // separator
                            let colour = {
                                let mut col = TRANSACTION_HISTORY_CONTAINER_COL;
                                col = col.hsva();
                                col.2 = col.2.mul(1.5).min(255);
                                col.rgba()
                            };

                            let _ = elem().decl(Decl { colour, height: fixed!(ui.scale(2.0)), width: percent!(1.0), ..Decl });
                        }

                        let tx_is_in_block      = tx.mined_h.is_in_block();
                        let tx_is_on_best_chain = tx.mined_h.is_in_block() && !tx.is_outside_bc;

                        if let _ = elem().decl(Decl {
                            id: id_index("Transaction", index as u32),
                            padding: padding.mul(0.5),
                            child_gap,
                            height: fit!(),
                            width: percent!(1.0),
                            direction: LeftToRight,
                            align: Center,
                            ..Decl
                        }) {
                            // left icon
                            if let _ = elem().decl(Decl {
                                id: id_index("Left Icon", index as u32),
                                height: fit!(),
                                width: fixed!(ui.scale(32.0)),
                                direction: TopToBottom,
                                align: Center,
                                ..Decl
                            }) {
                                // TODO: account for mempool
                                let icon = match (tx.kind(), tx_is_in_block) {
                                    (WalletTxKind::Send,     true) => ICON_UP_SMALL,
                                    (WalletTxKind::SelfSend, true) => ICON_DOWN_SMALL,
                                    (WalletTxKind::Receive,  true) => ICON_DOWN_SMALL,
                                    (WalletTxKind::Mine,     true) => ICON_MONEY_1, // TODO: pickaxe/tools icon
                                    (WalletTxKind::Shield,   true) => ICON_SHIELD,
                                    (WalletTxKind::Stake,    true) => ICON_LINK_1,
                                    (WalletTxKind::BeginUnstake,  true) => ICON_LINK_EXT_ALT,
                                    (WalletTxKind::ClaimUnstake,  true) => ICON_UNLINK,
                                    _ => {
                                        let timer = (ui.tx_loading_animation_timer * 3.0) as u64;
                                        if timer % 3 == 0 {
                                            ICON_DOT
                                        } else if timer % 3 == 1 {
                                            ICON_DOT_2
                                        } else {
                                            ICON_DOT_3
                                        }
                                    }
                                };

                                // TODO: account for mempool & confirmation level
                                let colour = if tx_is_on_best_chain {
                                    WHITE
                                } else {
                                    (0x60, 0x60, 0x60, 0xff) /* @todo colors */
                                };

                                ui.text(icon, TextDecl { font: Icons, colour, h: ui.scale(24.0), align: AlignX::Center, ..TextDecl });
                            }

                            // info
                            if let _ = elem().decl(Decl {
                                id: id_index("Centre Info", index as u32),
                                height: fit!(),
                                width: grow!(),
                                direction: TopToBottom,
                                align: Left,
                                ..Decl
                            }) {
                                let label = match tx.kind() {
                                    WalletTxKind::Send          => if tx_is_in_block { "Sent"      } else { "Sending"   },
                                    WalletTxKind::Receive       => if tx_is_in_block { "Received"  } else { "Receiving" },
                                    WalletTxKind::Mine          => if tx_is_in_block { "Mined"     } else { "Mining"    },
                                    WalletTxKind::SelfSend      => if tx_is_in_block { "Returned"  } else { "Returning" },
                                    WalletTxKind::Shield        => if tx_is_in_block { "Shielded"  } else { "Shielding" },
                                    WalletTxKind::Stake         => if tx_is_in_block { "Staked"    } else { "Staking"   },
                                    WalletTxKind::BeginUnstake  => if tx_is_in_block { "Unstaked"  } else { "Unstaking" },
                                    WalletTxKind::ClaimUnstake  => if tx_is_in_block { "Unbonded"  } else { "Unbonding" },
                                };

                                let label_str = if tx_is_in_block {
                                    frame_strf!(data, "{} @ {}", label, tx.mined_h.0)
                                } else {
                                    frame_strf!(data, "{}", label)
                                };

                                ui.text(label_str, TextDecl { h: kind_text_h, align: AlignX::Left, ..TextDecl });

                                // spacer
                                if let _ = elem().decl(Decl { width: grow!(), height: fixed!(ui.scale(4.0)), ..Default::default() }) {}

                                let txid = tx.txid.to_string();
                                ui.text(frame_strf!(data, "{}..{}", &txid[0..8], &txid[txid.len() - 8..]), TextDecl { font: Mono, h: transaction_text_h, colour: (0x90, 0x90, 0x90, 0xff) /* @todo colors */, align: AlignX::Left, ..TextDecl });

                                // spacer
                                if let _ = elem().decl(Decl { width: grow!(), height: fixed!(ui.scale(4.0)), ..Default::default() }) {}

                                if let Ok(memo_str) = String::from_utf8(tx.memo.as_slice().to_vec()) {
                                    let mut memo_str = memo_str.chars().filter(|c| c.is_ascii()).collect::<String>().trim_end_matches(|c| c == '\0').to_string();
                                    if memo_str.len() != 0 {
                                        if memo_str.starts_with("@") {
                                            if let Some(end) = memo_str.find(":") {
                                                memo_str = memo_str[end + 1..].to_string();
                                            }
                                        }

                                        ui.text(frame_strf!(data, "{}", memo_str), TextDecl { h: transaction_text_h, align: AlignX::Left, colour: (0x90, 0x90, 0x90, 0xff) /* @todo colors */, ..TextDecl });
                                    }
                                }
                            }

                            // right info
                            if let _ = elem().decl(Decl {
                                id: id_index("Right Info", index as u32),
                                height: grow!(),
                                width: fit!(),
                                child_gap: ui.scale(5.0),
                                direction: TopToBottom,
                                align: Right,
                                ..Decl
                            }) {
                                let confirmation_icons_h = ui.scale(10.0);

                                ui.text(" ", TextDecl { font: Icons, colour, h: confirmation_icons_h, align: AlignX::Center, ..TextDecl });

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

                                let totals = tx.totals();
                                match tx.kind() {
                                    // TODO: don't use account_value_delta, use sent_zats.
                                    // We care about fees if we spent any money ourselves.
                                    // If we just received from someone else, we don't care about fees.
                                    // TODO: BeginUnstake sends just a fee with no receive, ClaimUnstake receives with no send.
                                    WalletTxKind::Send | WalletTxKind::SelfSend | WalletTxKind::Stake | WalletTxKind::BeginUnstake => {
                                        let send_amount: i64 = tx.account_value_delta().into(); // TODO: don't use account_value_delta, use sent_zats.
                                        let send_amount: u64 = send_amount.abs() as u64;

                                        let prefix = if tx.kind() == WalletTxKind::Send { "-" } else { "" };
                                        ui.text(frame_strf!(data, "{}{} cTAZ", prefix, str_from_ctaz(send_amount)), TextDecl { h: transaction_text_h, align: AlignX::Right, colour, ..TextDecl });
                                    },
                                    WalletTxKind::Receive | WalletTxKind::ClaimUnstake | WalletTxKind::Mine => {
                                        if true { // total
                                            ui.text(frame_strf!(data, "+{} cTAZ", str_from_ctaz(totals.recv_zats.into_u64())), TextDecl { h: transaction_text_h, align: AlignX::Right, colour, ..TextDecl });
                                        } else { // transparent, shielded
                                            let (t_z, s_z) = (tx.parts[0].recv_zats.into_u64(), tx.parts[1].recv_zats.into_u64());
                                            ui.text(frame_strf!(data, "+{} | {} cTAZ", str_from_ctaz(t_z), str_from_ctaz(s_z)), TextDecl { h: transaction_text_h, align: AlignX::Right, colour, ..TextDecl });
                                        }
                                    },
                                    WalletTxKind::Shield => {
                                        // TODO: (how) do we want to show the fee?
                                        let shield_amount = totals.recv_zats.into_u64();

                                        ui.text(frame_strf!(data, "{} cTAZ", str_from_ctaz(shield_amount)), TextDecl { h: transaction_text_h, align: AlignX::Right, colour, ..TextDecl });
                                    },
                                    _ => {
                                        ui.text("TODO cTAZ", TextDecl { h: transaction_text_h, align: AlignX::Right, colour, ..TextDecl });
                                    },
                                }

                                if tx.kind() != WalletTxKind::Receive &&
                                   tx.kind() != WalletTxKind::BeginUnstake
                                {
                                    let fee_str = if let Some(fee) = tx.fee() {
                                        str_from_ctaz(fee.into_u64())
                                    } else {
                                        format!("<unknown>")
                                    };
                                    ui.text(frame_strf!(data, "Fee: {} cTAZ", fee_str), TextDecl { h: transaction_text_h, align: AlignX::Right, colour, ..TextDecl });
                                }

                                let _ = elem().decl(Decl { height: grow!(), ..Decl }); // spacer

                                if let _ = elem().decl(Decl {
                                    id: id_index("Confirmation Status", index as u32),
                                    height: fit!(),
                                    width: grow!(),
                                    align: Right,
                                    ..Decl
                                }) {
                                    let CONFIRMATIONS_THRESHOLD = 3;

                                    let finalized = tx.mined_h.0 as u64                           <= 16;                // @Todo: Use wallet state instead of viz state for this
                                    let confirmed = tx.mined_h.0 as u64 + CONFIRMATIONS_THRESHOLD <= viz.bc_tip_height; // @Todo: Use wallet state instead of viz state for this

                                    pub const RED:  (u8, u8, u8, u8) = (255, 64, 67, 0xff);      /* @todo colors */
                                    pub const BLUE: (u8, u8, u8, u8) = (0x33, 0x88, 0xde, 0xff); /* @todo colors */
                                    let (colour, text) = if finalized           { (BLUE,  DOUBLE_ICON_OK_CIRCLED_1) }
                                                    else if confirmed           { (WHITE, DOUBLE_ICON_OK_CIRCLED2_1) }
                                                    else if tx_is_on_best_chain { (WHITE, ICON_OK_CIRCLED2_1) }
                                                    else if tx_is_in_block      { (RED,   ICON_FORK) }
                                                    // TODO: proposing case
                                                    // TODO: building case
                                                    else                        { (WHITE, " ") };
                                    let colour = colour.mul(0.75);

                                    ui.text(text, TextDecl { font: Icons, colour, h: confirmation_icons_h, wrap: Wrap::None, align: AlignX::Right,  ..TextDecl });
                                }
                            }
                        }
                    }
                }
            }
        }
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
                 tab_id: &mut Id) {
    // @TODO: MAKE THESE NOT USE TABS, JUST USE HEADERS
    let mut tab_id_faucet = Id::default();
    let mut tab_id_roster = Id::default();

    let roster = &wallet_state.lock().unwrap().roster.clone();

    if let _ = elem().decl(Decl {
        id: id("Finalizers Tab Bar"),
        child_gap,
        width: percent!(1.0),
        height: fit!(),
        align: Center,
        ..Decl
    }) {
        tab_id_roster = ui.tab_ex((0.0, radius.1, radius.2, radius.3), padding, tab_id, id("Finalizers"), frame_strf!(data, "Finalizers ({})", roster.len()));
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
        fn display_str(chunks: &[u64; 4]) -> String {
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

            for b in &bytes[0..4] {
                str.push_str(&format!("{:02x}", b));
            }
            str.push_str("..");
            for b in &bytes[bytes.len() - 4..] {
                str.push_str(&format!("{:02x}", b));
            }

            str
        }

        let mut button_ex = |ui: &mut Context, label, act_on_press, enabled: bool| {
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

        let mut clickable_icon = |ui: &mut Context, id, icon, enabled | {
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
            for b in wallet::TENDERLINK_PUBLIC_KEY.lock().unwrap().iter().rev() {
                recv_address.push_str(&format!("{:02x}", b));
            }

            ui.text("Your Finalizer Address", TextDecl { h: ui.scale(20.0), colour: WHITE, align: AlignX::Center, ..TextDecl });
            ui.text(frame_strf!(data, "[{}..{}]", &recv_address[0..8], &recv_address[recv_address.len()-8..]), TextDecl { font: Mono, h: ui.scale(20.0), colour: WHITE, align: AlignX::Center, ..TextDecl });

            if button_ex(ui, "Copy Address", true, true) {
                ui.input().send_to_clipboard(&recv_address);
            }
        }

        let (id, mut clip, mut scroll) = ui.scroll_container(data, id("Finalizer Scroll Container"), 48.0);
        if roster.len() == 0 {
            clip = Scroll(0.0, 0.0);
            *scroll = 0.0;
        }
        if let _ = elem().decl(Decl {
            id,
            colour: TRANSACTION_HISTORY_CONTAINER_COL,
            child_gap: child_gap * 0.5, padding,
            radius: padding.0.dup4(),
            width:  percent!(1.0),
            height: grow!(),
            // height: percent!(1.0),
            direction: TopToBottom,
            clip,
            align: Top,
            ..Decl
        }) {
            if roster.len() == 0 {
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
                    ui.text("There are no finalizers yet.", TextDecl { colour: WHITE.mul(0.6), h, align: AlignX::Center, ..TextDecl });
                }
            }
            else {
                let kind_text_h = ui.scale(18.0);
                let transaction_text_h = ui.scale(16.0);

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
                    if let _ = elem().decl(Decl {
                        id: id_index("Roster Member", index as u32),
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

                        // info
                        if let _ = elem().decl(Decl {
                            id: id_index("Roster Member Info", index as u32),
                            height: fit!(),
                            width: grow!(),
                            direction: TopToBottom,
                            align: Left,
                            ..Decl
                        }) {
                            let bytes = member.pub_key;
                            let chunks = {
                                let mut chunks = [0u64; 4];
                                for i in 0..4 {
                                    let start = i * 8;
                                    let end = start + 8;
                                    let mut buf = [0u8; 8];
                                    buf.copy_from_slice(&bytes[start..end]);
                                    chunks[i] = u64::from_le_bytes(buf);
                                }

                                chunks
                            };

                            ui.text(frame_strf!(data, "{}", display_str(&chunks)), TextDecl { font: Mono, h: ui.scale(18.0), align: AlignX::Left, ..TextDecl });
                        }

                        // right info
                        if let _ = elem().decl(Decl {
                            id: id_index("Roster Member Amounts", index as u32),
                            height: fit!(),
                            width: fit!(),
                            direction: TopToBottom,
                            align: Right,
                            ..Decl
                        }) {
                            let colour = (0xff, 0xaf, 0x0e, 0xff);
                            ui.text(frame_strf!(data, "{} cTAZ", str_from_ctaz(member.voting_power)), TextDecl { font: Mono, h: ui.scale(16.0), colour, align: AlignX::Right, ..TextDecl });
                        }
                    }
                }
            }
        }

        // spacer
        // if let _ = elem().decl(Decl { width: grow!(), height: fixed!(ui.scale(32.0)), ..Default::default() }) {}
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
        tab_id_faucet = ui.tab_ex(radius, padding, tab_id, id("Faucet"), frame_strf!(data, "Faucet (Height {})", &wallet_state.lock().unwrap().miner_seen_h));
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
        // spacer
        if let _ = elem().decl(Decl { width: grow!(), height: fixed!(ui.scale(16.0)), ..Default::default() }) {}

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
        if let _ = elem().decl(Decl {
            padding: ui.scale(32.0).dup4(), child_gap, align: TopLeft,
            width: grow!(), height: fit!(),
            direction: TopToBottom,
            ..Decl
        }) {
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
        if let _ = elem().decl(Decl {
            id: id("Buttons Container"),
            padding, child_gap, align: Center,
            width: percent!(1.0),
            height: fit!(),
            ..Decl
        }) {

            let mut button_ex = |label, act_on_press, enabled: bool| {
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

            if button_ex("Receive cTAZ", false, !wallet_state.lock().unwrap().waiting_for_faucet) {
                wallet_state.lock().unwrap().request_from_faucet();
            }
        }
    }
}


pub fn run_ui(ui: &mut Context, wallet_state: Arc<Mutex<WalletState>>, data: &mut UiData, viz: &mut VizState, is_rendering: bool) -> bool {
    data.per_frame_strs.clear();

    let mut result = false;

    const MIN_ZOOM: f32 = 0.5;
    const MAX_ZOOM: f32 = 1.65; // @PreventPanesColliding

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

    // Begin the layout
    let clay = magic(ui).clay();
    clay.set_layout_dimensions((window_w as f32, window_h as f32).into());
    clay.pointer_state(mouse_pos.into(), mouse_held);
    clay.set_measure_text_function_user_data(ui.draw(), |string, text_config, draw| {
        let font_kind = match text_config.font_id { 0 => FontKind::Normal, 1 => FontKind::Mono, 2 => FontKind::Icons, _ => todo!() };
        let h = text_config.font_size as f32;
        let w = draw.measure_text_line(font_kind, h, string);
        clay::math::Dimensions::new(w, h)
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

        let pane_pct = Sizing::Percent(ui.zoom * PANE_PERCENT);

        if let _elem = elem().decl(Decl {
            id: id("Left Pane"),
            direction: TopToBottom,
            width: pane_pct,
            height: grow!(),
            clip: Clip,
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
                direction: TopToBottom,
                align: Top,
                width: grow!(),
                height: fit!(),
                ..Decl
            }) {
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
                    // @todo: two vertical panes with right aligned label and left aligned height
                    let decl = TextDecl { h: ui.scale(16.0), align: AlignX::Center, ..TextDecl };
                    ui.text(frame_strf!(data, "PoS Height: {}", viz.bft_tip_height), decl);
                    ui.text(frame_strf!(data, "PoW Height: {}", viz.bc_tip_height), decl);
                    ui.text(frame_strf!(data, "PoW Finalized Height: {}", viz.bc_finalized_tip_height), decl);
                    ui.text(frame_strf!(data, "Orchard Pool Zatoshis: {}", viz.orchard_pool_balance), decl);
                    ui.text(frame_strf!(data, "Staking (Bonded) Pool Zatoshis: {}", viz.staking_bonded_pool_balance), decl);
                    ui.text(frame_strf!(data, "Staking (Unbonded) Pool Zatoshis: {}", viz.staking_unbonded_pool_balance), decl);
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
            width: pane_pct,
            height: grow!(),
            clip: Clip,
            ..Decl
        }) {
            let id = _elem.decl.id;
            if ui.hovered(id) {
                ui.capture = true;
            }

            let mut pane_tab_r = ui.pane_tab_r;
            ui_right_pane(ui, wallet_state.clone(), viz, data, child_gap, padding, radius, &mut pane_tab_r);
            ui.pane_tab_r = pane_tab_r;
        }
    }

    if viz.inspecting_block_hash != Hash32::from_u64(0) {
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
        ui.capture = true;
    }

    if !ui.nav_id_to_idx.contains_key(&ui.nav_id) {
        ui.nav_id = 0;
        ui.nav_enable = false;
    }
    // if ui.input().key_pressed(KeyCode::Escape) {
    //     ui.nav_enable = false;
    // }

    if ui.input().key_pressed(KeyCode::Tab) && ui.nav_idx_to_id.len() > 0 {
        let idx = if ui.nav_id != 0 {
            let old_idx = ui.nav_id_to_idx[&ui.nav_id] as isize;
            if (ui.input().key_held(KeyCode::ShiftLeft) || ui.input().key_held(KeyCode::ShiftRight)) {
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

    if is_rendering {
        let mut nav_bbox: Option<(isize, isize, isize, isize)> = None;

        for command in render_commands {
            fn clay_color_to_u32(color: clay::Color) -> u32 {
                let r = color.r as u32;
                let g = color.g as u32;
                let b = color.b as u32;
                let a = color.a as u32;
                let color = (a << 24) | (r << 16) | (g << 8) | b;
                color
            }

            let x1 = (command.bounding_box.x)                               as isize;
            let y1 = (command.bounding_box.y)                               as isize;
            let x2 = (command.bounding_box.x + command.bounding_box.width)  as isize;
            let y2 = (command.bounding_box.y + command.bounding_box.height) as isize;

            let bbox = (x1, y1, x2, y2);

            if ui.nav_id == command.id {
                nav_bbox = Some(bbox);
            }

            let colour = match command.config {
                Rectangle(ref config)                 => { clay_color_to_u32(config.color) }
                RenderCommandConfig::Text(ref config) => { clay_color_to_u32(config.color) }
                _ => { 0 }
            };

            let draw_debug_rect = || {
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
                draw_debug_rect();
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

                let compute_x = |idx: usize| {
                    let n = idx.min(textbox_state.text_buf.len());
                    let mut str = String::with_capacity(n * 4);
                    str.extend(textbox_state.text_buf[..n].iter().copied());
                    let xoff = ui.draw().measure_text_line(FontKind::Normal, textbox_state.h, &str);
                    x1 + padding as isize + xoff as isize
                };

                let head_x = compute_x(textbox_state.selection.0);
                let tail_x = compute_x(textbox_state.selection.1);

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

            draw_debug_rect();
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

    pub mouse_pressed_id:             Id,
    pub key_pressed_id:               Id,
    // pub most_recent_mouse_pressed_id: Id,

    pub nav_id_to_idx: HashMap<u32, usize>,
    pub nav_idx_to_id: Vec<u32>,
    pub nav_id: u32,
    pub nav_enable: bool,
    pub nav_skip: bool,

    pub pane_tab_l: Id,
    pub pane_tab_r: Id,

    pub modal: Modal,

    pub tx_loading_animation_timer: f32,
}

#[derive(Debug, Default, Copy, Clone)]
pub struct Font(u64);
