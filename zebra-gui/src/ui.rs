#![allow(warnings)]

use std::{hash::Hash};
use winit::{event::MouseButton, keyboard::KeyCode};
use clay_layout as clay;
use clay::{fit, fixed, grow, percent, Clay, Declaration};
use clay::render_commands::RenderCommandConfig::{Rectangle, Text};
use clay::layout::{Alignment, LayoutAlignmentX, LayoutAlignmentY};

use super::*;

// make widgets nicer to construct
#[allow(unused_macros)]
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

fn dbg_ui(ui: &mut Context, _data: &mut SomeDataToKeepAround, is_rendering: bool) -> bool {
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

#[derive(Debug, Default, Copy, Clone)] enum Direction { #[default] LeftToRight, TopToBottom }
#[derive(Debug, Default, Copy, Clone)] enum AlignX    { #[default] Left, Center, Right }
#[derive(Debug, Default, Copy, Clone)] enum AlignY    { #[default] Top, Center, Bottom }
#[derive(Debug, Default, Copy, Clone)] struct Align   { x: AlignX, y: AlignY }
#[derive(Debug,          Copy, Clone)] enum Sizing    { Fit(f32, f32), Grow(f32, f32), Fixed(f32), Percent(f32) }
#[derive(Debug, Default, Copy, Clone)] struct Id      { base_id: u32, id: u32, offset: u32, chars: *const u8, len: usize }
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
#[macro_export] macro_rules! Fit {
    ($min:expr, $max:expr) => { Sizing::Fit($min, $max) };
    ($min:expr)            => { Fit!($min, f32::MAX) };
    ()                     => { Fit!(0.0) };
}
#[macro_export] macro_rules! Grow {
    ($min:expr, $max:expr) => { Sizing::Grow($min, $max) };
    ($min:expr)            => { Grow!($min, f32::MAX) };
    ()                     => { Grow!(0.0) };
}
#[macro_export] macro_rules! Fixed { ($val:expr) => { Sizing::Fixed($val) }; }
#[macro_export] macro_rules! Percent {
    ($percent:expr) => {{
        const _: () = assert!(
            $percent >= 0.0 && $percent <= 1.0,
            "Percent value must be between 0.0 and 1.0 inclusive!"
        );
        Sizing::Percent($percent)
    }};
}


#[derive(Debug, Default, Copy, Clone)]
struct Item {
    id: Id,
    direction: Direction,
    colour: (u8, u8, u8, u8),
    radius: (f32, f32, f32, f32),
    padding: (f32, f32, f32, f32),
    child_gap: f32,
    align: Align,
    width:  Sizing,
    height: Sizing,
}

impl Id {
    fn from_clay(id: clay::Clay_ElementId) -> Self {
        Self {
            base_id: id.baseId,
            id: id.id,
            offset: id.offset,
            chars: id.stringId.chars as *const u8,
            len: id.stringId.length as usize
        }
    }
}

// fn clay_string_from(s: &[u8]) -> clay::Clay_String {
//     clay::Clay_String {
//         chars: s.as_ptr() as *const i8,
//         isStaticallyAllocated: true,
//         length: s.len() as i32,
//     }
// }

impl Context {
    fn item<
        'render,
        'clay: 'render,
        ImageElementData: 'render,
        CustomElementData: 'render,
        T: FnOnce(&mut clay::ClayLayoutScope<'clay, 'render, ImageElementData, CustomElementData>)
    >(
        &self,
        c: &mut clay::ClayLayoutScope<'clay, 'render, ImageElementData, CustomElementData>,
        item: Item,
        F: T
    ) {
        fn sizing(sizing: Sizing) -> clay::layout::Sizing {
            match sizing {
                Sizing::Fit(min, max)  => { clay::layout::Sizing::Fit(min, max) }
                Sizing::Grow(min, max) => { clay::layout::Sizing::Grow(min, max) }
                Sizing::Fixed(x)       => { clay::layout::Sizing::Fixed(x) }
                Sizing::Percent(p)     => { clay::layout::Sizing::Percent(p) }
            }
        }
        c.with(
            &Declaration::new()
            .background_color(item.colour.into())
            .id(clay::id::Id {
                id: clay::Clay_ElementId {
                    baseId: item.id.base_id,
                    id: item.id.id,
                    offset: item.id.offset,
                    stringId: clay::Clay_String {
                        isStaticallyAllocated: false,
                        chars: item.id.chars as *const i8,
                        length: item.id.len as i32
                    }
                }
            })
            .layout()
                .width(sizing(item.width))
                .height(sizing(item.height))
                .padding(clay::layout::Padding {
                    left:   item.padding.0 as u16,
                    right:  item.padding.1 as u16,
                    top:    item.padding.2 as u16,
                    bottom: item.padding.3 as u16,
                })
                .child_gap(item.child_gap as u16)
                .child_alignment(Alignment {
                    x: match item.align.x {
                        AlignX::Left   => { LayoutAlignmentX::Left }
                        AlignX::Center => { LayoutAlignmentX::Center }
                        AlignX::Right  => { LayoutAlignmentX::Right }
                    },
                    y: match item.align.y {
                        AlignY::Top     => { LayoutAlignmentY::Top }
                        AlignY::Center  => { LayoutAlignmentY::Center }
                        AlignY::Bottom  => { LayoutAlignmentY::Bottom }
                    }
                })
                .direction(match item.direction {
                    Direction::TopToBottom => { clay::layout::LayoutDirection::TopToBottom }
                    Direction::LeftToRight => { clay::layout::LayoutDirection::LeftToRight }
                })
            .end()
            .corner_radius()
                .top_left(item.radius.0)
                .top_right(item.radius.1)
                .bottom_left(item.radius.2)
                .bottom_right(item.radius.3)
            .end(),
            F
        );
    }
}

trait         Dup2: Copy { fn dup2(self) -> (Self, Self); }
impl<T: Copy> Dup2 for T { fn dup2(self) -> (Self, Self) { (self, self) } }
trait         Dup3: Copy { fn dup3(self) -> (Self, Self, Self); }
impl<T: Copy> Dup3 for T { fn dup3(self) -> (Self, Self, Self) { (self, self, self) } }
trait         Dup4: Copy { fn dup4(self) -> (Self, Self, Self, Self); }
impl<T: Copy> Dup4 for T { fn dup4(self) -> (Self, Self, Self, Self) { (self, self, self, self) } }

fn run_ui(ui: &mut Context, _data: &mut SomeDataToKeepAround, is_rendering: bool) -> bool {
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

    let (window_w, window_h) = (ui.draw().window_width as f32, ui.draw().window_height as f32);
    let mouse_pos = (ui.input().mouse_pos().0 as f32, ui.input().mouse_pos().1 as f32);

    let child_gap = ui.scale16(8.0);
    let padding = &clay::layout::Padding::all(child_gap);
    const WHITE:            clay::Color = clay::Color::rgb(0xff as f32, 0xff as f32, 0xff as f32);
    const pane_col:         clay::Color = clay::Color::rgb(0x12 as f32, 0x12 as f32, 0x12 as f32);
    const inactive_tab_col: clay::Color = clay::Color::rgb(0x0f as f32, 0x0f as f32, 0x0f as f32);
    const active_tab_col:   clay::Color = pane_col;

    const button_col:       clay::Color = clay::Color::rgb(0x24 as f32, 0x24 as f32, 0x24 as f32);
    const button_hover_col: clay::Color = clay::Color::rgb(0x30 as f32, 0x30 as f32, 0x30 as f32);

    const align_center_center: Alignment = Alignment { x: LayoutAlignmentX::Center, y: LayoutAlignmentY::Center };

    let mouse_held    = ui.input().mouse_held(winit::event::MouseButton::Left);
    let mouse_clicked = ui.input().mouse_pressed(winit::event::MouseButton::Left);

    let radius = ui.scale(8.0);

    // Begin the layout
    let clay = magic(ui).clay();
    clay.set_layout_dimensions((window_w as f32, window_h as f32).into());
    clay.pointer_state(mouse_pos.into(), mouse_held);
    clay.set_measure_text_function_user_data(ui.draw(), |string, text_config, draw| {
        let h = text_config.font_size as f32;
        let w = draw.measure_text_line(h, string);
        clay::math::Dimensions::new(w, h)
    });

    let mut c = clay.begin::<(), ()>();

    c.with(&Declaration::new()
        .layout()
            .width(grow!())
            .height(grow!())
            .padding(Padding(padding))
            .child_gap(child_gap)
        .end(), |c| {
        let pane_pct = {
            let pct = 0.25;
            // clay::layout::Sizing::Percent((pct * ui.scale).min(pct))
            clay::layout::Sizing::Percent(pct * ui.scale)
        };

        // left pane
        c.with(&Declaration::new()
            .layout()
                .direction(clay::layout::LayoutDirection::TopToBottom)
                .width(pane_pct)
                .height(grow!())
            .end()
            , |c| {

            // tab bar
            c.with(&Declaration::new()
                .layout()
                    .width(percent!(1.0))
                    .height(fit!())
                    .child_gap(child_gap)
                    .child_alignment(align_center_center)
                .end(), |c| {
                let tab_text_h = ui.scale16(18.0);

                // Wallet tab
                c.with(&Declaration::new()
                    .background_color(active_tab_col)
                    .corner_radius().top_left(radius).top_right(radius).end()
                    .layout()
                        .width(grow!())
                        .height(grow!())
                        .padding(Padding(padding))
                        .child_alignment(align_center_center)
                    .end(), |c| {
                    c.text("Wallet", clay::text::TextConfig::new().font_size(tab_text_h).color(WHITE).alignment(clay::text::TextAlignment::Center).end());
                });

                // Finalizers tab
                c.with(&Declaration::new()
                    .background_color(inactive_tab_col)
                    .corner_radius().top_left(radius).top_right(radius).end()
                    .layout()
                        .width(grow!())
                        .height(grow!())
                        .padding(Padding(padding))
                        .child_alignment(align_center_center)
                    .end(), |c| {
                    c.text("Finalizers", clay::text::TextConfig::new().font_size(tab_text_h).color(WHITE).alignment(clay::text::TextAlignment::Center).end());
                });

                // History tab
                c.with(&Declaration::new()
                    .background_color(inactive_tab_col)
                    .corner_radius().top_left(radius).top_right(radius).end()
                    .layout()
                        .width(grow!())
                        .height(grow!())
                        .padding(Padding(padding))
                        .child_alignment(align_center_center)
                    .end(), |c| {
                    c.text("History", clay::text::TextConfig::new().font_size(tab_text_h).color(WHITE).alignment(clay::text::TextAlignment::Center).end());
                });
            });

            let balance_text_h = ui.scale16(48.0);

            // Main contents
            ui.item(c, Item {
                colour: (0x12, 0x12, 0x12, 0xff),
                radius: (0.0, 0.0, radius, radius),
                direction: Direction::TopToBottom,
                width: Percent!(1.0),
                height: Grow!(),
                ..Default::default()
            }, |c| {

                // spacer
                ui.item(c, Item { width: Grow!(), height: Fixed!(ui.scale(32.0)), ..Default::default() }, |c| {});

                // balance container
                c.with(&Declaration::new()
                    .layout()
                        .width(percent!(1.0))
                        .height(fit!())
                        .padding(Padding(padding))
                        .child_alignment(align_center_center)
                    .end(), |c| {
                    c.text("0.0000 cTAZ", clay::text::TextConfig::new().font_size(balance_text_h).color(WHITE).alignment(clay::text::TextAlignment::Center).end());
                });

                let child_gap = child_gap as f32;
                let padding = child_gap.dup4();

                // buttons container
                ui.item(c, Item {
                    padding, child_gap, align: Align::Center,
                    width: Percent!(1.0),
                    height: Fit!(),
                    ..Default::default()
                }, |c| {

                    let radius = ui.scale(24.0).dup4();
                    let buttons = ["Send", "Receive", "Faucet", "Stake", "Unstake"];

                    for button in buttons {
                        let id = c.id(button);
                        let hover = c.pointer_over(id);
                        let (down, click) = (hover && mouse_held, hover && mouse_clicked);

                        const button_col:       (u8, u8, u8, u8) = (0x24, 0x24, 0x24, 0xff);
                        const button_hover_col: (u8, u8, u8, u8) = (0x30, 0x30, 0x30, 0xff);

                        let colour = if hover { button_hover_col } else { button_col };

                        ui.item(c, Item {
                            id: Id::from_clay(id.id),
                            direction: Direction::TopToBottom,
                            width: Fit!(),
                            height: Fit!(),
                            child_gap,
                            align: Align::Center,
                            ..Default::default()
                        }, |c| {

                            // Button circle
                            ui.item(c, Item {
                                colour, radius, padding, child_gap, align: Align::Center,
                                width: Fixed!(radius.0 * 2.0),
                                height: Fixed!(radius.0 * 2.0),
                                ..Default::default()
                            }, |c| {
                                // let temp_letter_symbol_h = ui.scale16(32.0);
                                // c.text(&button[..1], clay::text::TextConfig::new().font_size(temp_letter_symbol_h).color(WHITE).alignment(clay::text::TextAlignment::Center).end());
                            });

                            let button_text_h = ui.scale16(16.0);
                            c.text(button, clay::text::TextConfig::new().font_size(button_text_h).color(WHITE).alignment(clay::text::TextAlignment::Center).end());
                        });
                    }

                });

            });
        });

        // central gap
        c.with(&Declaration::new()
            .corner_radius().all(radius).end()
            .layout()
                .width(grow!())
                .height(grow!())
                .padding(Padding(padding))
                .child_gap(child_gap)
            .end(), |c| {
        });

        /*
        // right pane
        c.with(&Declaration::new()
            .background_color(pane_col)
            .corner_radius().all(radius).end()
            .layout()
                .width(pane_pct)
                .height(grow!())
                .padding(Padding(padding))
                .child_gap(child_gap)
            .end(), |c| {
        });
        */
    });


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
                Text(config) => {
                    ui.draw().text_line(x1 as f32, y1 as f32, config.font_size as f32, config.text, clay_color_to_u32(config.color));
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

    result |= dbg_ui(ui, _data, is_rendering);

    result
}

// @Hack this is annoying!
pub fn Padding(padding: &clay::layout::Padding) -> clay::layout::Padding {
    clay::layout::Padding::new(padding.left, padding.right, padding.top, padding.bottom)
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
    let real_input = ui.input; let result =           run_ui(ui, data, false);
    ui.input = &dummy_input;   let result = result || run_ui(ui, data, true);
    ui.input =   real_input;
    return result;
}

#[derive(Debug, Default, Clone)]
pub struct Context {
    pub input: *const InputCtx,
    pub draw:  *const DrawCtx,
    pub clay:  *mut   Clay,

    pub debug: bool,
    pub pixel_inspector_primed: bool,

    pub delta: f64,
    pub default_style: Style,
    pub draw_commands: Vec<DrawCommand>,

    pub scale:     f32,
    pub zoom:      f32,
    pub dpi_scale: f32,
}

impl Context {
    pub fn new(_style: Style) -> Context { Context { scale: 1f32, zoom: 1f32, dpi_scale: 1f32, ..Default::default() } }
    fn draw(&self)  -> &DrawCtx  { unsafe { &*self.draw  } }
    fn input(&self) -> &InputCtx { unsafe { &*self.input } }
    fn clay(&self)  -> &mut Clay { unsafe { &mut *self.clay } }

    fn scale(&self, size: f32) -> f32 { (size * self.scale).floor() }
    fn scale32(&self, size: f32) -> u32 { self.scale(size) as u32 }
    fn scale16(&self, size: f32) -> u16 { self.scale(size) as u16 }

    fn button(&self) -> bool {
        false
    }
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

/* #[derive(Debug, Default, Clone)]
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
} */

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

/* #[derive(Debug, Clone)]
pub enum DrawCommand {
    Scissor(Rect),
    Rect(Rect, Option<isize>, Color),
    Circle(f32, f32, f32, Color),
    Box(Rect, f32, Color),
    Text(Font, isize, isize, Color, String),
} */

#[derive(Debug, Default, Copy, Clone)]
pub struct Font(u64);

//////////////////////////////////////

/* #[derive(Debug, Default, Clone, Copy)]
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
*/
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
