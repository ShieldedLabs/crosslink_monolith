#![allow(warnings)]

use std::thread::current;
use std::{hash::Hash};
use winit::{event::MouseButton, keyboard::KeyCode};
use clay_layout as clay;
use clay::{Clay, Declaration};
use clay::render_commands::RenderCommandConfig;
use clay::render_commands::RenderCommandConfig::{Rectangle, ScissorStart, ScissorEnd};
use clay::layout::{Alignment, LayoutAlignmentX, LayoutAlignmentY};
//use clay::*; // @Temporary

use wallet::str_from_ctaz;

use super::*;

pub fn magic<'a, 'b, T>(mut_ref: &'a mut T) -> &'b mut T {
    let mut_ref = mut_ref as *mut T;
    return unsafe { &mut *mut_ref };
}

#[derive(Debug, Default)]
pub struct UiData {
    pub per_frame_strs:    Vec<String>,
}


#[macro_export]
macro_rules! frame_strf {
    ($data:expr, $($arg:tt)*) => {
        $data.frame_str(&format_args!($($arg)*).to_string())
    };
}

impl UiData {
    fn frame_str(&mut self, str: &str) -> &String {
        self.per_frame_strs.push(str.to_string().clone());
        return self.per_frame_strs.last().unwrap();
    }
}

fn dbg_ui(ui: &mut Context, is_rendering: bool) -> bool {
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

#[derive(Debug, Default, Copy, Clone, PartialEq)] enum Direction { #[default] LeftToRight, TopToBottom }
#[derive(Debug, Default, Copy, Clone, PartialEq)] enum AlignX    { #[default] Left, Right, Center }
#[derive(Debug, Default, Copy, Clone, PartialEq)] enum AlignY    { #[default] Top, Bottom, Center }
#[derive(Debug, Default, Copy, Clone, PartialEq)] struct Align   { x: AlignX, y: AlignY }
#[derive(Debug,          Copy, Clone, PartialEq)] enum Sizing    { Fit(f32, f32), Grow(f32, f32), Fixed(f32), Percent(f32) }
#[derive(Debug, Default, Copy, Clone, PartialEq)] struct Id { id: u32, offset: u32, base_id: u32, len: usize, chars: *const u8 }
impl Default for Sizing { fn default() -> Self { Self::Fit(0.0, f32::MAX) } }
impl Align {
    const TopLeft:     Self = Self { y: AlignY::Top,    x: AlignX::Left };
    const Top:         Self = Self { y: AlignY::Top,    x: AlignX::Center };
    const TopRight:    Self = Self { y: AlignY::Top,    x: AlignX::Right };
    const Left:        Self = Self { y: AlignY::Center, x: AlignX::Left };
    const Center:      Self = Self { y: AlignY::Center, x: AlignX::Center };
    const Right:       Self = Self { y: AlignY::Center, x: AlignX::Right };
    const BottomLeft:  Self = Self { y: AlignY::Bottom, x: AlignX::Left };
    const Bottom:      Self = Self { y: AlignY::Bottom, x: AlignX::Center };
    const BottomRight: Self = Self { y: AlignY::Bottom, x: AlignX::Right };
}
// why can we not `use` these? namaste
const TopLeft:     Align = Align::TopLeft;
const Top:         Align = Align::Top;
const TopRight:    Align = Align::TopRight;
const Left:        Align = Align::Left;
const Center:      Align = Align::Center;
const Right:       Align = Align::Right;
const BottomLeft:  Align = Align::BottomLeft;
const Bottom:      Align = Align::Bottom;
const BottomRight: Align = Align::BottomRight;

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

use Direction::*;


#[derive(Debug, Default, Copy, Clone, PartialEq)]
struct Decl {
    id: Id,
    direction: Direction,
    floating: bool,
    colour: (u8, u8, u8, u8),
    radius: (f32, f32, f32, f32),
    padding: (f32, f32, f32, f32),
    clip: bool,
    child_gap: f32,
    align: Align,
    width:  Sizing,
    height: Sizing,
}
// Ease-of-use constant for the builder pattern thing, so you can write Decl{..Decl} to get a default Decl.
// I can't do #[derive_const(Default)] because that's only on Rust nightly. And it would probably be complex anyway.
const Decl: Decl = Decl {
    id:        Id { id: 0, offset: 0, base_id: 0, len: 0, chars: std::ptr::null() },
    direction: Direction::LeftToRight,
    floating:  false,
    colour:    (0,   0,   0,   0),
    radius:    (0.0, 0.0, 0.0, 0.0),
    padding:   (0.0, 0.0, 0.0, 0.0),
    clip:      false,
    child_gap: 0.0,
    align:     TopLeft,
    width:     Sizing::Fit(0.0, f32::MAX),
    height:    Sizing::Fit(0.0, f32::MAX),
};

#[derive(Debug, Default, Copy, Clone, PartialEq)]
struct TextDecl {
    font: FontKind,
    h: f32,
    colour: (u8, u8, u8, u8),
    align: AlignX,
}
const TextDecl: TextDecl = TextDecl {
    font: FontKind::Normal,
    h: 0.0,
    colour: WHITE,
    align: AlignX::Left,
};



impl Id {
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
fn id(label: &str) -> Id {
    let id = unsafe { clay::Clay__HashString(label.into(), 0, clay::Clay__GetParentElementId()) };
    Id {
        id: id.id,
        offset: id.offset,
        base_id: id.baseId,
        len: id.stringId.length as usize,
        chars: id.stringId.chars as *const u8
    }
}

#[derive(Default)] struct Element { decl: Decl }
fn elem() -> Element { unsafe { clay::Clay__OpenElement(); } Element::default() }
impl Drop for Element { fn drop(&mut self) { unsafe { clay::Clay__CloseElement(); } } }

const Clay_ElementId_ZERO: clay::Clay_ElementId = clay::Clay_ElementId { id: 0, offset: 0, baseId: 0, stringId: clay::Clay_String { isStaticallyAllocated: false, length: 0, chars: std::ptr::null() } };
const Clay_SizingMinMax_ZERO: clay::Clay_SizingMinMax = clay::Clay_SizingMinMax { min: 0f32, max: f32::MAX };
const Clay_SizingAxis_ZERO: clay::Clay_SizingAxis = clay::Clay_SizingAxis {
    size: clay::Clay_SizingAxis__bindgen_ty_1 { minMax: Clay_SizingMinMax_ZERO },
    type_: clay::Clay__SizingType_CLAY__SIZING_TYPE_FIT
};
const Clay_Sizing_ZERO: clay::Clay_Sizing = clay::Clay_Sizing { width: Clay_SizingAxis_ZERO, height: Clay_SizingAxis_ZERO };
const Clay_Padding_ZERO: clay::Clay_Padding = clay::Clay_Padding { left: 0, right: 0, top: 0, bottom: 0 };
const Clay_ChildAlignment_ZERO: clay::Clay_ChildAlignment = clay::Clay_ChildAlignment { x: 0 as _, y: 0 as _ };
const Clay_LayoutConfig_ZERO: clay::Clay_LayoutConfig = clay::Clay_LayoutConfig {
    sizing: Clay_Sizing_ZERO,
    padding: Clay_Padding_ZERO,
    childGap: 0,
    childAlignment: Clay_ChildAlignment_ZERO,
    layoutDirection: clay::Clay_LayoutDirection_CLAY_LEFT_TO_RIGHT,
};
const Clay_Color_ZERO: clay::Clay_Color = clay::Clay_Color { r: 0.0, g: 0.0, b: 0.0, a: 0.0 };
const Clay_CornerRadius_ZERO: clay::Clay_CornerRadius = clay::Clay_CornerRadius { topLeft: 0f32, topRight: 0f32, bottomLeft: 0f32, bottomRight: 0f32 };
const Clay_ElementDeclaration_ZERO: clay::Clay_ElementDeclaration = clay::Clay_ElementDeclaration {
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
            a: item.colour.3 as f32
        };
        decl.id = item.id.clay().id;
        decl.clip = clay::Clay_ClipElementConfig { horizontal: item.clip, vertical: item.clip, childOffset: clay::Clay_Vector2 { x: 0.0, y: 0.0 } };
        if item.floating {
            decl.floating.attachTo = clay::Clay_FloatingAttachToElement_CLAY_ATTACH_TO_PARENT;
            // decl.floating.clipTo = clay::Clay_FloatingClipToElement_CLAY_CLIP_TO_NONE;
            decl.floating.clipTo = clay::Clay_FloatingClipToElement_CLAY_CLIP_TO_ATTACHED_PARENT;
            decl.floating.attachPoints = clay::Clay_FloatingAttachPoints {
                element: clay::Clay_FloatingAttachPointType_CLAY_ATTACH_POINT_CENTER_CENTER,
                parent:  clay::Clay_FloatingAttachPointType_CLAY_ATTACH_POINT_CENTER_CENTER,
            };
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

const PANE_PERCENT: f32 = (0.25 + 0.333) / 2.0;

const WHITE:            (u8, u8, u8, u8) = (0xff, 0xff, 0xff, 0xff);
const PANE_COL:         (u8, u8, u8, u8) = (0x12, 0x12, 0x12, 0xff); // @FigmaScreenshot
const INACTIVE_TAB_COL: (u8, u8, u8, u8) = (0x0f, 0x0f, 0x0f, 0xff);
const ACTIVE_TAB_COL:   (u8, u8, u8, u8) = PANE_COL;
const BUTTON_COL:       (u8, u8, u8, u8) = (0x24, 0x24, 0x24, 0xff); // @FigmaScreenshot
const BUTTON_HOVER_COL: (u8, u8, u8, u8) = (0x30, 0x30, 0x30, 0xff);
const BUTTON_DOWN_COL:  (u8, u8, u8, u8) = (0x1c, 0x1c, 0x1c, 0xff);

const MODAL_COL: (u8, u8, u8, u8) = (0x1e, 0x1e, 0x1e, 0xff); // @FigmaScreenshot

const TRANSACTION_HISTORY_CONTAINER_COL: (u8, u8, u8, u8) = (0x22, 0x22, 0x24, 0xff); // @FigmaScreenshot

const fn clay_colour(colour: (u8, u8, u8, u8)) -> clay::Color { clay::Color::rgba(colour.0 as f32, colour.1 as f32, colour.2 as f32, colour.3 as f32) }

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

trait HSVA_RGBA {
    #[inline(always)] fn hsva(self) -> Self;
    #[inline(always)] fn rgba(self) -> Self;
}
impl HSVA_RGBA for (u8, u8, u8, u8) {
    #[inline(always)] fn hsva(self) -> Self { rgba_to_hsva(self.0, self.1, self.2, self.3) }
    #[inline(always)] fn rgba(self) -> Self { hsva_to_rgba(self.0, self.1, self.2, self.3) }
}

impl Context {
    pub fn new() -> Context { Context { scale: 1f32, zoom: 1f32, dpi_scale: 1f32, ..Default::default() } }
    fn draw(&self)  -> &DrawCtx  { unsafe { &*self.draw     } }
    fn input(&self) -> &InputCtx { unsafe { &*self.input    } }
    fn clay(&self)  -> &mut Clay { unsafe { &mut *self.clay } }

    fn scale(&self, size: f32) -> f32 { (size * self.scale).floor() }
    fn scale32(&self, size: f32) -> u32 { self.scale(size) as u32 }
    fn scale16(&self, size: f32) -> u16 { self.scale(size) as u16 }

    fn hovered(&self, id: Id) -> bool { unsafe { clay::Clay_PointerOver(id.clay().id) } }

    fn button_ex(&mut self, clicked_id: &mut Id, id: Id, act_on_press: bool) -> (bool, (u8, u8, u8, u8)) {
        let mouse_held     = self.input().mouse_held(winit::event::MouseButton::Left);
        let mouse_pressed  = self.input().mouse_pressed(winit::event::MouseButton::Left);
        let mouse_released = self.input().mouse_released(winit::event::MouseButton::Left);

        let hover    = self.hovered(id);
        let down     = hover && mouse_held;
        let pressed  = hover && mouse_pressed;
        let released = hover && mouse_released;
        if pressed {
            *clicked_id = id;
        }

        if hover {
            self.cursor = winit::window::Cursor::Icon(winit::window::CursorIcon::Pointer);
        }

        let activated = (*clicked_id == id) && if act_on_press {
            pressed
        } else {
            released
        };

        let colour = if down || pressed {
            if clicked_id.id == id.id {
                BUTTON_DOWN_COL
            } else {
                BUTTON_COL
            }
        } else if hover {
            BUTTON_HOVER_COL
        } else {
            BUTTON_COL
        };

        (activated, colour)
    }

    fn button               (&mut self, clicked_id: &mut Id, id: Id) -> (bool, (u8, u8, u8, u8)) { return self.button_ex(clicked_id, id, true); }
    fn button_act_on_release(&mut self, clicked_id: &mut Id, id: Id) -> (bool, (u8, u8, u8, u8)) { return self.button_ex(clicked_id, id, false); }

    fn text(&self, label: &str, decl: TextDecl) {
        let config = clay::text::TextConfig::new()
            .font_id(decl.font as u16)
            .font_size(decl.h as u16)
            .color(clay_colour(decl.colour))
            .alignment(match decl.align {
                AlignX::Left   => clay::text::TextAlignment::Left,
                AlignX::Right  => clay::text::TextAlignment::Right,
                AlignX::Center => clay::text::TextAlignment::Center,
            })
            .end();
        unsafe { clay::Clay__OpenTextElement(label.into(), config.into()) };
    }
    fn text_no_wrap(&self, label: &str, decl: TextDecl) {
        let config = clay::text::TextConfig::new()
            .font_id(decl.font as u16)
            .font_size(decl.h as u16)
            .color(clay_colour(decl.colour))
            .alignment(match decl.align {
                AlignX::Left   => clay::text::TextAlignment::Left,
                AlignX::Right  => clay::text::TextAlignment::Right,
                AlignX::Center => clay::text::TextAlignment::Center,
            })
            .wrap_mode(clay::text::TextElementConfigWrapMode::All)
            .end();
        unsafe { clay::Clay__OpenTextElement(label.into(), config.into()) };
    }

    fn tab_ex(&mut self,
              radius: (f32, f32, f32, f32),
              padding: (f32, f32, f32, f32),
              tab_id: &mut Id,
              clicked_id: &mut Id,
              id: Id,
              label: &str) -> Id {
        let tab_text_h = self.scale(18.0);

        let radius = (radius.0, radius.1, 0.0, 0.0);

        let (clicked, _) = self.button(clicked_id, id);
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

    fn tab(&mut self,
           radius: (f32, f32, f32, f32),
           padding: (f32, f32, f32, f32),
           tab_id: &mut Id,
           clicked_id: &mut Id,
           label: &str) -> Id {
        let id = id(label);
        self.tab_ex(radius, padding, tab_id, clicked_id, id, label)
    }
}

trait         Dup2: Copy { fn dup2(self) -> (Self, Self); }
impl<T: Copy> Dup2 for T { fn dup2(self) -> (Self, Self) { (self, self) } }
trait         Dup3: Copy { fn dup3(self) -> (Self, Self, Self); }
impl<T: Copy> Dup3 for T { fn dup3(self) -> (Self, Self, Self) { (self, self, self) } }
trait         Dup4: Copy { fn dup4(self) -> (Self, Self, Self, Self); }
impl<T: Copy> Dup4 for T { fn dup4(self) -> (Self, Self, Self, Self) { (self, self, self, self) } }

// Implementation of `tuple.mul(scalar)`. Helper "AsF32" trait to get it working.
// This would literally be a two-liner, if Rust generics had C++-style SFINAE.
trait AsF32         { #[inline(always)] fn to_f32(self) -> f32;                #[inline(always)] fn from_f32(x: f32) -> Self; }
impl  AsF32 for u8  { #[inline(always)] fn to_f32(self) -> f32 { self as f32 } #[inline(always)] fn from_f32(x: f32) -> Self { x as Self } }
impl  AsF32 for u16 { #[inline(always)] fn to_f32(self) -> f32 { self as f32 } #[inline(always)] fn from_f32(x: f32) -> Self { x as Self } }
impl  AsF32 for u32 { #[inline(always)] fn to_f32(self) -> f32 { self as f32 } #[inline(always)] fn from_f32(x: f32) -> Self { x as Self } }
impl  AsF32 for u64 { #[inline(always)] fn to_f32(self) -> f32 { self as f32 } #[inline(always)] fn from_f32(x: f32) -> Self { x as Self } }
impl  AsF32 for i8  { #[inline(always)] fn to_f32(self) -> f32 { self as f32 } #[inline(always)] fn from_f32(x: f32) -> Self { x as Self } }
impl  AsF32 for i16 { #[inline(always)] fn to_f32(self) -> f32 { self as f32 } #[inline(always)] fn from_f32(x: f32) -> Self { x as Self } }
impl  AsF32 for i32 { #[inline(always)] fn to_f32(self) -> f32 { self as f32 } #[inline(always)] fn from_f32(x: f32) -> Self { x as Self } }
impl  AsF32 for i64 { #[inline(always)] fn to_f32(self) -> f32 { self as f32 } #[inline(always)] fn from_f32(x: f32) -> Self { x as Self } }
impl  AsF32 for f32 { #[inline(always)] fn to_f32(self) -> f32 { self as f32 } #[inline(always)] fn from_f32(x: f32) -> Self { x as Self } }
impl  AsF32 for f64 { #[inline(always)] fn to_f32(self) -> f32 { self as f32 } #[inline(always)] fn from_f32(x: f32) -> Self { x as Self } }

trait                                Mul                  { #[inline(always)] fn mul(self, f: f32) -> Self; }
impl<T: AsF32>                       Mul for T            { #[inline(always)] fn mul(self, f: f32) -> Self { T::from_f32(self.to_f32() * f) } }
impl<A: Mul, B: Mul>                 Mul for (A, B)       { #[inline(always)] fn mul(self, f: f32) -> Self { (self.0.mul(f), self.1.mul(f)) } }
impl<A: Mul, B: Mul, C: Mul>         Mul for (A, B, C)    { #[inline(always)] fn mul(self, f: f32) -> Self { (self.0.mul(f), self.1.mul(f), self.2.mul(f)) } }
impl<A: Mul, B: Mul, C: Mul, D: Mul> Mul for (A, B, C, D) { #[inline(always)] fn mul(self, f: f32) -> Self { (self.0.mul(f), self.1.mul(f), self.2.mul(f), self.3.mul(f)) } }

fn ui_left_pane(ui: &mut Context,
                wallet_state: Arc<Mutex<wallet::WalletState>>,
                data: &mut UiData,
                viz: &mut VizState,
                child_gap: f32,
                padding: (f32, f32, f32, f32),
                radius:  (f32, f32, f32, f32),
                clicked_id: &mut Id,
                tab_id: &mut Id) {

    if ui.modal != Modal::None && let _elem = elem().decl(Decl {
        child_gap,
        id: id("Modal Container"),
        padding: padding.mul(2.0),
        radius: (radius.0, 0.0, radius.2, 0.0),
        floating: true,
        colour: (0, 0, 0, 0xC0),
        align: Center,
        width:  grow!(),
        height: grow!(),
        ..Decl
    }) {

        let container_id = _elem.decl.id;
        let container_hovered = ui.hovered(container_id);

        if let _elem = elem().decl(Decl {
            child_gap, radius,
            id: id("Modal Contents"),
            padding: padding.mul(2.0),
            colour: MODAL_COL,
            width:  grow!(ui.scale(192.0), ui.scale(384.0)),
            height: grow!(ui.scale(192.0), ui.scale(384.0)),
            align: Top,
            direction: TopToBottom,
            ..Decl
        }) {

            let contents_id = _elem.decl.id;
            let contents_hovered = ui.hovered(contents_id);

            let text_h = ui.scale(24.0);
            let title_bar = |ui: &mut Context, clicked_id: &mut Id, closeable, title, title_bar_id| {
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

                        let (clicked, colour) = ui.button_act_on_release(clicked_id, id);
                        if clicked || ui.input().key_pressed(KeyCode::Escape) {
                            ui.modal = Modal::None;
                        }

                        // Click background to exit -- the code could be placed farther outside but it is here so it can be gated by `closeable`
                        if ui.hovered(container_id) && !ui.hovered(contents_id) && ui.input().mouse_pressed(winit::event::MouseButton::Left) {
                            ui.modal = Modal::None;
                            *clicked_id = id;
                        }

                        let radius = ui.scale(20.0);

                        // Button circle
                        if let _ = elem().decl(Decl {
                            id, colour, radius: radius.dup4(), padding, child_gap, align: Center,
                            width:  fixed!(radius * 2.0),
                            height: fixed!(radius * 2.0),
                            ..Decl
                        }) {
                            let temp_letter_symbol_h = ui.scale(32.0);
                            ui.text("x", TextDecl { h: temp_letter_symbol_h, align: AlignX::Center, ..TextDecl });
                        }
                    }
                }
            };
            match ui.modal {
                Modal::None => {}
                Modal::Send => {
                    title_bar(ui, clicked_id, true, "Send",    id("Send Title Bar"));
                }
                Modal::Receive => {
                    title_bar(ui, clicked_id, true, "Receive", id("Receive Title Bar"));
                }
                Modal::Stake => {
                    title_bar(ui, clicked_id, true, "Stake",   id("Stake Title Bar"));

                    let mut button_ex = |label, act_on_press, disabled: bool| {
                        let id = id(label);
                        let (clicked, mut colour) = ui.button_ex(clicked_id, id, act_on_press);
                        if let _ = elem().decl(Decl {
                            id,
                            child_gap,
                            align: Align::Center,
                            direction: TopToBottom,
                            width: fit!(),
                            height: fit!(),
                            ..Decl
                        }) {
                            let radius = ui.scale(24.0);

                            // @TEMP: real disabling
                            if disabled {
                                colour.3 = 100;
                            }

                            // Button
                            if let _ = elem().decl(Decl {
                                colour,
                                padding,
                                child_gap,
                                radius: radius.dup4(),
                                align: Align::Center,
                                width:  fit!(ui.scale(192.0)),
                                height: fit!(radius * 2.0),
                                ..Decl
                            }) {
                                let h = ui.scale(20.0);
                                let colour = if disabled { INACTIVE_TAB_COL } else { WHITE };
                                ui.text(label, TextDecl { h, colour, align: AlignX::Center, ..TextDecl });
                            }
                        }

                        clicked && !disabled // @TEMP: real disabling
                    };

                    if button_ex("Stake 1 ZEC", false, wallet_state.lock().unwrap().waiting_for_stake_to_miner) {
                        wallet_state.lock().unwrap().stake_to_miner();
                    }
                }
                Modal::Unstake => {
                    title_bar(ui, clicked_id, true, "Unstake", id("Unstake Title Bar"));
                }
            }
        }
    }


    let mut tab_id_wallet = Id::default();
    let mut tab_id_finalizers = Id::default();
    let mut tab_id_history = Id::default();

    if let _ = elem().decl(Decl {
        id: id("Tab Bar"),
        child_gap,
        width: percent!(1.0),
        height: fit!(),
        align: Center,
        ..Decl
    }) {
        tab_id_wallet     = ui.tab((radius.0, 0.0, radius.2, radius.3), padding, tab_id, clicked_id, "Wallet");
        // tab_id_finalizers = ui.tab(radius, padding, tab_id, clicked_id, "Finalizers");
        tab_id_history    = ui.tab_ex(radius, padding, tab_id, clicked_id, id("History"), frame_strf!(data, "History ({})", &wallet_state.lock().unwrap().txs.len()));
    }

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
        ..Decl
    }) {
        let balance_text_h = ui.scale(48.0);

        // spacer
        if let _ = elem().decl(Decl { width: grow!(), height: fixed!(ui.scale(32.0)), ..Default::default() }) {}

        if *tab_id == tab_id_wallet {

            // balance container
            if let _ = elem().decl(Decl {
                width: grow!(),
                height: fit!(),
                padding,
                align: Center,
                ..Decl
            }) {
                let balance_str = frame_strf!(data, "{} cTAZ", str_from_ctaz(wallet_state.lock().unwrap().balance.try_into().unwrap()));
                ui.text(&balance_str, TextDecl { h: balance_text_h, align: AlignX::Center, ..TextDecl });
            }

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

                let mut button = |ui: &mut Context, icon: &'static str, label: &'static str| {
                    let id = id(label);
                    let (clicked, colour) = ui.button(clicked_id, id);
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
                            let temp_letter_symbol_h = ui.scale(32.0);
                            ui.text(ICON_CANCEL, TextDecl { font: Icons, h: temp_letter_symbol_h, align: AlignX::Center, ..TextDecl });
                        }

                        let button_text_h = ui.scale(16.0);
                        ui.text(label, TextDecl { h: button_text_h, align: AlignX::Center, ..TextDecl });
                    }
                    clicked
                };

                if button(ui, ICON_PAPER_PLANE, "Send")    { ui.modal = Modal::Send;    }
                if button(ui, ICON_QRCODE,      "Receive") { ui.modal = Modal::Receive; }
                if button(ui, ICON_PLUS,        "Stake")   { ui.modal = Modal::Stake;   }
                if button(ui, ICON_MINUS_1,     "Unstake") { ui.modal = Modal::Unstake; }

            }

            if let _ = elem().decl(Decl {
                colour: TRANSACTION_HISTORY_CONTAINER_COL,
                child_gap, padding,
                radius: radius.mul(2.0),
                width:  grow!(radius.0 * 2.0),
                height: grow!(radius.0 * 2.0),
                align: Center,
                ..Decl
            }) {
                let h = ui.scale(24.0);
                ui.text("There are no transactions yet.", TextDecl { h: h, align: AlignX::Center, ..TextDecl });
            }

        } else if *tab_id == tab_id_finalizers {
        } else if *tab_id == tab_id_history {
            if let _ = elem().decl(Decl {
                id: id("Balance"),
                padding,
                child_gap,
                width: percent!(1.0),
                height: fit!(),
                direction: TopToBottom,
                align: Center,
                ..Decl
            }) {
                let txs = &wallet_state.lock().unwrap().txs;

                let tx_count_text_h = ui.scale(24.0);
                ui.text(frame_strf!(data, "Transactions ({})", txs.len()), TextDecl { h: tx_count_text_h, align: AlignX::Center, ..TextDecl });

                let transaction_text_h = ui.scale(12.0);

                for tx in txs {
                    if let _ = elem().decl(Decl{
                        padding,
                        child_gap,
                        height: grow!(),
                        width: fit!(),
                        direction: TopToBottom,
                        align: Top,
                        ..Decl
                    }) {
                        // manually split id text
                        let string = format!("{:?}", tx.0.txid);
                        ui.text(frame_strf!(data, "{} {}", &string[..string.len()/2], &string[string.len()/2..]), TextDecl { h: transaction_text_h, align: AlignX::Left, ..TextDecl });
                        ui.text(frame_strf!(data, "{} cTAZ", str_from_ctaz(tx.0.total_received.into())), TextDecl { h: transaction_text_h, align: AlignX::Left, ..TextDecl });
                    }
                }
            }
        }
    }
}

fn ui_right_pane(ui: &mut Context,
                 wallet_state: Arc<Mutex<wallet::WalletState>>,
                 viz: &mut VizState,
                 data: &mut UiData,
                 child_gap: f32,
                 padding: (f32, f32, f32, f32),
                 radius:  (f32, f32, f32, f32),
                 clicked_id: &mut Id,
                 tab_id: &mut Id) {
    let mut tab_id_faucet = Id::default();
    let mut tab_id_roster = Id::default();
    let mut tab_id_settings = Id::default();

    if let _ = elem().decl(Decl {
        id: id("Tab Bar"),
        child_gap,
        width: percent!(1.0),
        height: fit!(),
        align: Center,
        ..Decl
    }) {
        tab_id_faucet   = ui.tab(radius, padding, tab_id, clicked_id, "Faucet");
        // tab_id_roster   = ui.tab(radius, padding, tab_id, clicked_id, "Roster");
        tab_id_settings = ui.tab((0.0, radius.1, radius.2, radius.3), padding, tab_id, clicked_id, "Settings");
    }

    // Main contents
    if let _ = elem().decl(Decl {
        id: id("Main Contents"),
        colour: PANE_COL,
        radius: (0.0, 0.0, 0.0, radius.3),
        direction: TopToBottom,
        width: percent!(1.0),
        height: grow!(),
        ..Decl
    }) {

        // spacer
        if let _ = elem().decl(Decl { width: grow!(), height: fixed!(ui.scale(32.0)), ..Default::default() }) {}

        if *tab_id == tab_id_faucet {

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

            // buttons container
            if let _ = elem().decl(Decl {
                id: id("Buttons Container"),
                padding, child_gap, align: Center,
                width: percent!(1.0),
                height: fit!(),
                ..Decl
            }) {

                let mut button_ex = |label, act_on_press, disabled: bool| {
                    let id = id(label);
                    let (clicked, mut colour) = ui.button_ex(clicked_id, id, act_on_press);
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

                        // @TEMP: real disabling
                        if disabled {
                            colour.3 = 100;
                        }

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
                            let colour = if disabled { INACTIVE_TAB_COL } else { WHITE };
                            ui.text(label, TextDecl { h, colour, align: AlignX::Center, ..TextDecl });
                        }
                    }

                    clicked && !disabled // @TEMP: real disabling
                };

                // @TEMP: real disabling
                if button_ex("Receive cTAZ", false, wallet_state.lock().unwrap().waiting_for_faucet) {
                    wallet_state.lock().unwrap().request_from_faucet();
                }
                if button_ex("Test Stake Action cTAZ", false, wallet_state.lock().unwrap().waiting_for_test_stake_action) {
                    wallet_state.lock().unwrap().perform_test_stake_action();
                }
            }

            if let _ = elem().decl(Decl {
                padding: ui.scale(16.0).dup4(), child_gap, align: TopLeft,
                width: grow!(), height: fit!(),
                direction: TopToBottom,
                ..Decl
            }) {
                let title_h = ui.scale(28.0);
                let text_h = ui.scale(22.0);
                let (h, un, sh_p, sh_s, fc) = {
                    let w = wallet_state.lock().unwrap();
                    (
                        w.miner_seen_height,
                        w.miner_unshielded_funds,
                        w.miner_shielded_pending_funds,
                        w.miner_shielded_spendable_funds,
                        w.faucet_funds_available,
                    )
                };

                let row   = Decl { width: percent!(1.0), child_gap, height: fit!(), ..Decl };

                let left  = Decl { width: grow!(), height: fit!(), align: Left,  ..Decl };
                let right = Decl { width: grow!(), height: fit!(), align: Right, ..Decl };

                let left_text  = TextDecl { h: text_h,  align: AlignX::Left,  ..TextDecl  };
                let right_text = TextDecl { font: Mono, align: AlignX::Right, ..left_text };

                ui.text(frame_strf!(data, "Miner funds at height {}:", h), TextDecl { h: title_h, ..left_text });
                if let _ = elem().decl(row) {
                    if let _ = elem().decl(left)  { ui.text("Unshielded:", left_text); }
                    if let _ = elem().decl(right) { ui.text(frame_strf!(data, "{} cTAZ", str_from_ctaz(un)), right_text); }
                }
                if let _ = elem().decl(row) {
                    if let _ = elem().decl(left)  { ui.text("Shielded (pending):", left_text); }
                    if let _ = elem().decl(right) { ui.text(frame_strf!(data, "{} cTAZ", str_from_ctaz(sh_p)), right_text); }
                }
                if let _ = elem().decl(row) {
                    if let _ = elem().decl(left)  { ui.text("Shielded (spendable):", left_text); }
                    if let _ = elem().decl(right) { ui.text(frame_strf!(data, "{} cTAZ", str_from_ctaz(sh_s)), right_text); }
                }
                if let _ = elem().decl(row) {
                    if let _ = elem().decl(left)  { ui.text("Faucet available:", left_text); }
                    if let _ = elem().decl(right) { ui.text(frame_strf!(data, "{} cTAZ", str_from_ctaz(fc)), right_text); }
                }
                if let _ = elem().decl(Decl { width: grow!(), height: fixed!(ui.scale(32.0)), ..Default::default() }) {}

            };
        } else if *tab_id == tab_id_roster {
        } else if *tab_id == tab_id_settings {
        }
    }

    // spacer
    if let _ = elem().decl(Decl { width: grow!(), height: fixed!(ui.scale(32.0)), ..Default::default() }) {}

    // Block Inspector Contents
    if let _ = elem().decl(Decl {
        id: id("Block Inspector Contents"),
        colour: PANE_COL,
        radius: (0.0, radius.1, 0.0, radius.3),
        direction: TopToBottom,
        width: percent!(1.0),
        height: grow!(),
        ..Decl
    }) {
        let text_h = ui.scale(22.0);
        if viz.inspecting_block_hash == Hash32::from_u64(0) {
            ui.text(frame_strf!(data, "Click on a Block to Inspect its JSON!"), TextDecl { h: text_h, align: AlignX::Left, ..TextDecl });
        } else {
            ui.text_no_wrap(frame_strf!(data, "Block: {}", viz.inspecting_block_hash), TextDecl { h: text_h, align: AlignX::Left, ..TextDecl });
        }
    }
}


fn run_ui(ui: &mut Context, wallet_state: Arc<Mutex<wallet::WalletState>>, data: &mut UiData, viz: &mut VizState, is_rendering: bool) -> bool {
    data.per_frame_strs.clear();

    let mut result = false;

    const MIN_ZOOM: f32 = 0.5;
    const MAX_ZOOM: f32 = 2.0;

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

    let mut clicked_id = ui.clicked_id;
    let mut focused_id = ui.focused_id;
    let mut pane_tab_l = ui.pane_tab_l; // @Todo: how to not have to do this in rust?
    let mut pane_tab_r = ui.pane_tab_r; // @Todo: how to not have to do this in rust?

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

        let clicked_id = &mut clicked_id;
        let focused_id = &mut focused_id;
        let pane_tab_l = &mut pane_tab_l;
        let pane_tab_r = &mut pane_tab_r;

        let pane_pct = Sizing::Percent(ui.zoom * PANE_PERCENT);

        if let _ = elem().decl(Decl {
            id: id("Left Pane"),
            direction: TopToBottom,
            width: pane_pct,
            height: grow!(),
            clip: true,
            ..Decl
        }) {
            ui_left_pane(ui, wallet_state.clone(), data, viz, child_gap, padding, radius, clicked_id, pane_tab_l);
        }

        if let _ = elem().decl(Decl {
            id: id("Central Gap"),
            radius, padding, child_gap,
            direction: TopToBottom,
            width: grow!(),
            height: grow!(),
            ..Decl
        }) {

            if let _ = elem().decl(Decl { align: Top, width: grow!(), ..Decl }) {
                ui.text(frame_strf!(data, "TODO: Put BFT height here!"), TextDecl { h: ui.scale(24.0), align: AlignX::Center, ..TextDecl });
            }

            if let _ = elem().decl(Decl { height: grow!(), ..Decl }) {}

            // "Reset View" button
            if let _ = elem().decl(Decl { align: Bottom, width: grow!(), ..Decl }) {
                let label = "Reset View";
                let id = id(label);
                let (clicked, colour) = ui.button_ex(clicked_id, id, true);
                let radius = ui.scale(20.0);

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
                    let colour = WHITE;
                    ui.text(label, TextDecl { h: button_text_h, colour, align: AlignX::Center, ..TextDecl });
                }

                if clicked {
                    viz.camera_x = 0.0;
                    viz.camera_y = 0.0;
                    viz.zoom = 0.0;
                }
            }
        }

        if let _ = elem().decl(Decl {
            id: id("Right Pane"),
            direction: TopToBottom,
            width: pane_pct,
            height: grow!(),
            clip: true,
            ..Decl
        }) {
            ui_right_pane(ui, wallet_state.clone(), viz, data, child_gap, padding, radius, clicked_id, pane_tab_r);
        }
    }

    if !ui.input().mouse_held(winit::event::MouseButton::Left) {
        clicked_id = Id::default();
    }
    ui.clicked_id = clicked_id;
    ui.pane_tab_l = pane_tab_l;
    ui.pane_tab_r = pane_tab_r;

    // Return the list of render commands of your layout
    let render_commands = c.end();

    if is_rendering {
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
                                                clay_color_to_u32(config.color));
                }
                RenderCommandConfig::Text(config) => {
                    let font_kind = match config.font_id { 0 => FontKind::Normal, 1 => FontKind::Mono, 2 => FontKind::Icons, _ => todo!() };
                    ui.draw().text_line(font_kind, x1 as f32, y1 as f32, config.font_size as f32, config.text, clay_color_to_u32(config.color));
                }
                ScissorStart() => {
                    ui.draw().set_scissor(x1, y1, x2, y2);
                }
                ScissorEnd() => {
                    ui.draw().clear_scissor();
                }
                misc => { todo!("Unsupported clay render command: {:?}", misc) }
            }

            if ui.debug {
                let thickness = 2.0;
                let color = 0x80ff00ff;
                let t  = (thickness / 2.0) as isize;

                ui.draw().rectangle((x1-t) as f32, (y1-t) as f32, (x1+t) as f32, (y2+t) as f32, color);
                ui.draw().rectangle((x2-t) as f32, (y1-t) as f32, (x2+t) as f32, (y2+t) as f32, color);
                ui.draw().rectangle((x1-t) as f32, (y1-t) as f32, (x2-t) as f32, (y1+t) as f32, color);
                ui.draw().rectangle((x1-t) as f32, (y2-t) as f32, (x2-t) as f32, (y2+t) as f32, color);
            }
        }
    }

    result |= dbg_ui(ui, is_rendering);

    result
}

pub fn ui_update(ui: &mut Context, data: &mut UiData, viz: &mut VizState, wallet_state: Arc<Mutex<wallet::WalletState>>) -> bool {
    let dummy_input = InputCtx {
        this_mouse_pos: ui.input().this_mouse_pos,
        last_mouse_pos: ui.input().last_mouse_pos,

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

#[derive(Debug, Default, Clone)]
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

    pub clicked_id: Id,
    pub focused_id: Id,

    pub pane_tab_l: Id,
    pub pane_tab_r: Id,

    pub modal: Modal,
}

#[derive(Debug, Default, Copy, Clone)]
pub struct Font(u64);
