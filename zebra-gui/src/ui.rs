#![allow(warnings)]

use std::thread::current;
use std::{hash::Hash};
use winit::{event::MouseButton, keyboard::KeyCode};
use clay_layout as clay;
use clay::{fit, fixed, grow, percent, Clay, Declaration};
use clay::render_commands::RenderCommandConfig::{Rectangle, Text};
use clay::layout::{Alignment, LayoutAlignmentX, LayoutAlignmentY};

use super::*;

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
    fn id(label: &str) -> Self {
        let id = unsafe { clay::Clay__HashString(label.into(), 0, clay::Clay__GetParentElementId()) };
        Self {
            id: id.id,
            offset: id.offset,
            base_id: id.baseId,
            len: id.stringId.length as usize,
            chars: id.stringId.chars as *const u8
        }
    }
}

impl Context {
    pub fn new() -> Context { Context { scale: 1f32, zoom: 1f32, dpi_scale: 1f32, ..Default::default() } }
    fn draw(&self)  -> &DrawCtx  { unsafe { &*self.draw  } }
    fn input(&self) -> &InputCtx { unsafe { &*self.input } }
    fn clay(&self)  -> &mut Clay { unsafe { &mut *self.clay } }

    fn scale(&self, size: f32) -> f32 { (size * self.scale).floor() }
    fn scale32(&self, size: f32) -> u32 { self.scale(size) as u32 }
    fn scale16(&self, size: f32) -> u16 { self.scale(size) as u16 }

    fn item<
        'render,
        'clay: 'render,
        ImageElementData: 'render,
        CustomElementData: 'render,
        G: FnOnce(&clay::ClayLayoutScope<'clay, 'render, ImageElementData, CustomElementData>) -> Item,
        F: FnOnce(&clay::ClayLayoutScope<'clay, 'render, ImageElementData, CustomElementData>)
    >(
        &self,
        c: &clay::ClayLayoutScope<'clay, 'render, ImageElementData, CustomElementData>,
        g: G,
        f: F
    ) {
        unsafe {
            clay::Clay_SetCurrentContext(c.clay.context);
            clay::Clay__OpenElement();
        }

        let item = g(c);

        fn sizing(sizing: Sizing) -> clay::layout::Sizing {
            match sizing {
                Sizing::Fit(min, max)  => { clay::layout::Sizing::Fit(min, max) }
                Sizing::Grow(min, max) => { clay::layout::Sizing::Grow(min, max) }
                Sizing::Fixed(x)       => { clay::layout::Sizing::Fixed(x) }
                Sizing::Percent(p)     => { clay::layout::Sizing::Percent(p) }
            }
        }

        let decl = Declaration::<ImageElementData, CustomElementData>::new()
            .background_color(item.colour.into())
            .id(item.id.clay())
            // .clip(true, true, clay::math::Vector2 { x: 0.0, y: 0.0 })
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
            .end()
            .inner;

        unsafe {
            clay::Clay__ConfigureOpenElement(decl);
        }

        f(c);

        unsafe {
            clay::Clay__CloseElement();
        }
    }

    fn button<
        'render,
        'clay: 'render,
        ImageElementData: 'render,
        CustomElementData: 'render,
    >(
        &self,
        c: &clay::ClayLayoutScope<'clay, 'render, ImageElementData, CustomElementData>,
        clicked_id: &mut Id,
        id: Id
    ) -> (bool, (u8, u8, u8, u8)) {
        let mouse_held    = self.input().mouse_held(winit::event::MouseButton::Left);
        let mouse_clicked = self.input().mouse_pressed(winit::event::MouseButton::Left);

        let hover = c.pointer_over(id.clay());
        let (down, click) = (hover && mouse_held, hover && mouse_clicked);
        if click {
            *clicked_id = id;
        }

        const button_col:       (u8, u8, u8, u8) = (0x24, 0x24, 0x24, 0xff);
        const button_hover_col: (u8, u8, u8, u8) = (0x30, 0x30, 0x30, 0xff);
        const button_down_col:  (u8, u8, u8, u8) = (0x1e, 0x1e, 0x1e, 0xff);

        let colour = if down || click {
            if clicked_id.id == id.id {
                button_down_col
            } else {
                button_col
            }
        } else if hover {
            button_hover_col
        } else {
            button_col
        };

        (click, colour)
    }
}

trait         Dup2: Copy { fn dup2(self) -> (Self, Self); }
impl<T: Copy> Dup2 for T { fn dup2(self) -> (Self, Self) { (self, self) } }
trait         Dup3: Copy { fn dup3(self) -> (Self, Self, Self); }
impl<T: Copy> Dup3 for T { fn dup3(self) -> (Self, Self, Self) { (self, self, self) } }
trait         Dup4: Copy { fn dup4(self) -> (Self, Self, Self, Self); }
impl<T: Copy> Dup4 for T { fn dup4(self) -> (Self, Self, Self, Self) { (self, self, self, self) } }


fn run_ui(ui: &mut Context, wallet_state: Arc<Mutex<wallet::WalletState>>, _data: &mut SomeDataToKeepAround, is_rendering: bool) -> bool {
    let mut result = false;
    let mut balance_str = String::new();

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

    let child_gap = ui.scale(8.0);
    let padding = child_gap.dup4();

    const WHITE:            (u8, u8, u8, u8) = (0xff, 0xff, 0xff, 0xff);
    const WHITE_CLAY:       clay::Color = clay::Color::rgba(WHITE.0 as f32, WHITE.1 as f32, WHITE.2 as f32, WHITE.3 as f32);
    const pane_col:         (u8, u8, u8, u8) = (0x12, 0x12, 0x12, 0xff);
    const inactive_tab_col: (u8, u8, u8, u8) = (0x0f, 0x0f, 0x0f, 0xff);
    const active_tab_col:   (u8, u8, u8, u8) = pane_col;
    const button_col:       (u8, u8, u8, u8) = (0x24, 0x24, 0x24, 0xff);
    const button_hover_col: (u8, u8, u8, u8) = (0x30, 0x30, 0x30, 0xff);

    const align_center_center: Alignment = Alignment { x: LayoutAlignmentX::Center, y: LayoutAlignmentY::Center };

    let mouse_held    = ui.input().mouse_held(winit::event::MouseButton::Left);
    let mouse_clicked = ui.input().mouse_pressed(winit::event::MouseButton::Left);

    let radius = ui.scale(8.0).dup4();

    // Begin the layout
    let clay = magic(ui).clay();
    clay.set_layout_dimensions((window_w as f32, window_h as f32).into());
    clay.pointer_state(mouse_pos.into(), mouse_held);
    clay.set_measure_text_function_user_data(ui.draw(), |string, text_config, draw| {
        let h = text_config.font_size as f32;
        let w = draw.measure_text_line(h, string);
        clay::math::Dimensions::new(w, h)
    });

    let mut clicked_id = ui.clicked_id;
    let mut pane_tab_l = ui.pane_tab_l; // @Todo: how to not have to do this in rust?
    let mut pane_tab_r = ui.pane_tab_r; // @Todo: how to not have to do this in rust?

    let mut c = clay.begin::<(), ()>();

    ui.item(&c, |c| Item {
        id: Id::id("Main"),
        padding, child_gap,
        width: Grow!(),
        height: Grow!(),
        ..Default::default()
    }, |c| {
        let pane_pct = {
            let pct = 0.25;
            // clay::layout::Sizing::Percent((pct * ui.scale).min(pct))
            Sizing::Percent(pct * ui.scale)
        };

        ui.item(c, |c| Item {
            id: Id::id("Left Pane"),
            direction: Direction::TopToBottom,
            width: pane_pct,
            height: Grow!(),
            ..Default::default()
        }, |c| {

            let mut tab_id_wallet = Id::default();
            let mut tab_id_finalizers = Id::default();
            let mut tab_id_history = Id::default();

            ui.item(c, |c| Item {
                id: Id::id("Tab Bar"),
                child_gap,
                width: Percent!(1.0),
                height: Fit!(),
                align: Align::Center,
                ..Default::default()
            }, |c| {

                let mut tab = |tab_id: &mut Id, label| {
                    let tab_text_h = ui.scale16(18.0);

                    let radius = (radius.0, radius.1, 0.0, 0.0);

                    let id = Id::id(label);

                    let (clicked, _) = ui.button(c, &mut clicked_id, id);
                    if clicked || *tab_id == Id::default() {
                        *tab_id = id;
                    }

                    ui.item(c, |c| Item {
                        id,
                        radius, padding,
                        colour: if *tab_id == id { active_tab_col } else { inactive_tab_col },
                        width: Grow!(),
                        height: Grow!(),
                        align: Align::Center,
                        ..Default::default()
                    }, |c| {
                        c.text(label, clay::text::TextConfig::new().font_size(tab_text_h).color(WHITE_CLAY).alignment(clay::text::TextAlignment::Center).end());
                    });

                    id
                };
                tab_id_wallet     = tab(&mut pane_tab_l, "Wallet");
                tab_id_finalizers = tab(&mut pane_tab_l, "Finalizers");
                tab_id_history    = tab(&mut pane_tab_l, "History");

            });

            // Main contents
            ui.item(c, |c| Item {
                id: Id::id("Main Contents"),
                colour: (0x12, 0x12, 0x12, 0xff),
                radius: (0.0, 0.0, radius.2, radius.3),
                direction: Direction::TopToBottom,
                width: Percent!(1.0),
                height: Grow!(),
                ..Default::default()
            }, |c| {
                let balance_text_h = ui.scale16(48.0);

                // spacer
                ui.item(c, |c| Item { width: Grow!(), height: Fixed!(ui.scale(32.0)), ..Default::default() }, |c| {});

                if pane_tab_l == tab_id_wallet {

                    // balance container
                    ui.item(c, |c| Item {
                        width: Percent!(1.0),
                        height: Fit!(),
                        padding,
                        align: Align::Center,
                        ..Default::default()
                    }, |c| {
                        let balance = wallet_state.lock().unwrap().balance;
                        let zec_full = balance / 100_000_000;
                        let zec_part = balance % 100_000_000;
                        balance_str = format!("{}.{} cTAZ", zec_full, &format!("{:03}", zec_part)[..3]);
                        c.text(&balance_str, clay::text::TextConfig::new().font_size(balance_text_h).color(WHITE_CLAY).alignment(clay::text::TextAlignment::Center).end());
                    });

                    let child_gap = child_gap as f32;
                    let padding = child_gap.dup4();

                    // buttons container
                    ui.item(c, |c| Item {
                        id: Id::id("Buttons Container"),
                        padding, child_gap, align: Align::Center,
                        width: Percent!(1.0),
                        height: Fit!(),
                        ..Default::default()
                    }, |c| {

                        let mut f = |button| {
                            let id = Id::id(button);
                            let (clicked, colour) = ui.button(c, &mut clicked_id, id);
                            ui.item(c, |c| Item {
                                id, child_gap, align: Align::Center,
                                direction: Direction::TopToBottom,
                                width: Fit!(),
                                height: Fit!(),
                                ..Default::default()
                            }, |c| {

                                let radius = ui.scale(24.0);

                                // Button circle
                                ui.item(c, |c| Item {
                                    colour, radius: radius.dup4(), padding, child_gap, align: Align::Center,
                                    width:  Fixed!(radius * 2.0),
                                    height: Fixed!(radius * 2.0),
                                    ..Default::default()
                                }, |c| {
                                    let temp_letter_symbol_h = ui.scale16(32.0);
                                    c.text(&button[..1], clay::text::TextConfig::new().font_size(temp_letter_symbol_h).color(WHITE_CLAY).alignment(clay::text::TextAlignment::Center).end());
                                });

                                let button_text_h = ui.scale16(16.0);
                                c.text(button, clay::text::TextConfig::new().font_size(button_text_h).color(WHITE_CLAY).alignment(clay::text::TextAlignment::Center).end());
                            });
                            clicked
                        };

                        if f("Send")    { println!("Send!");    }
                        if f("Receive") { println!("Receive!"); }
                        if f("Faucet")  { println!("Faucet!");  }
                        if f("Stake")   { println!("Stake!");   }
                        if f("Unstake") { println!("Unstake!"); }

                    });

                } else if pane_tab_l == tab_id_finalizers {
                } else if pane_tab_l == tab_id_history {
                    ui.item(c, |c| Item {
                        width: Percent!(1.0),
                        height: Fit!(),
                        padding,
                        align: Align::Center,
                        ..Default::default()
                    }, |c| {
                        let balance = wallet_state.lock().unwrap().balance;
                        let zec_full = balance / 100_000_000;
                        let zec_part = balance % 100_000_000;
                        balance_str = format!("{}.{} cTAZ", zec_full, &format!("{:03}", zec_part)[..3]);
                        c.text(&balance_str, clay::text::TextConfig::new().font_size(balance_text_h).color(WHITE_CLAY).alignment(clay::text::TextAlignment::Center).end());
                    });
                }
            });
        });

        ui.item(c, |c| Item {
            id: Id::id("Central Gap"),
            radius, padding, child_gap,
            width: Grow!(),
            height: Grow!(),
            ..Default::default()
        }, |c| {
        });

        ui.item(c, |c| Item {
            id: Id::id("Right Pane"),
            direction: Direction::TopToBottom,
            width: pane_pct,
            height: Grow!(),
            ..Default::default()
        }, |c| {

            let mut tab_id_faucet = Id::default();
            let mut tab_id_roster = Id::default();
            let mut tab_id_settings = Id::default();

            ui.item(c, |c| Item {
                id: Id::id("Tab Bar"),
                child_gap,
                width: Percent!(1.0),
                height: Fit!(),
                align: Align::Center,
                ..Default::default()
            }, |c| {

                let mut tab = |tab_id: &mut Id, label| {
                    let tab_text_h = ui.scale16(18.0);

                    let radius = (radius.0, radius.1, 0.0, 0.0);

                    let id = Id::id(label);

                    let (clicked, _) = ui.button(c, &mut clicked_id, id);
                    if clicked || *tab_id == Id::default() {
                        *tab_id = id;
                    }

                    ui.item(c, |c| Item {
                        id,
                        radius, padding,
                        colour: if *tab_id == id { active_tab_col } else { inactive_tab_col },
                        width: Grow!(),
                        height: Grow!(),
                        align: Align::Center,
                        ..Default::default()
                    }, |c| {
                        c.text(label, clay::text::TextConfig::new().font_size(tab_text_h).color(WHITE_CLAY).alignment(clay::text::TextAlignment::Center).end());
                    });

                    id
                };

                tab_id_faucet   = tab(&mut pane_tab_r, "Faucet");
                tab_id_roster   = tab(&mut pane_tab_r, "Roster");
                tab_id_settings = tab(&mut pane_tab_r, "Settings");
            });

            // Main contents
            ui.item(c, |c| Item {
                id: Id::id("Main Contents"),
                colour: (0x12, 0x12, 0x12, 0xff),
                radius: (0.0, 0.0, radius.2, radius.3),
                direction: Direction::TopToBottom,
                width: Percent!(1.0),
                height: Grow!(),
                ..Default::default()
            }, |c| {
            });

        });
    });

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

pub fn demo_of_rendering_stuff_with_context_that_allocates_in_the_background(ui: &mut Context, data: &mut SomeDataToKeepAround, wallet_state: Arc<Mutex<wallet::WalletState>>) -> bool {
    let dummy_input = InputCtx {
        this_mouse_pos: ui.input().this_mouse_pos,
        last_mouse_pos: ui.input().last_mouse_pos,

        mouse_down: ui.input().mouse_down,
        keys_down1: ui.input().keys_down1,
        keys_down2: ui.input().keys_down2,

        ..Default::default()
    };
    let real_input = ui.input; let result =           run_ui(ui, wallet_state.clone(), data, false);
    ui.input = &dummy_input;   let result = result || run_ui(ui, wallet_state.clone(), data, true);
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

    pub draw_commands: Vec<DrawCommand>,

    pub scale:     f32,
    pub zoom:      f32,
    pub dpi_scale: f32,

    pub clicked_id: Id,

    pub pane_tab_l: Id,
    pub pane_tab_r: Id,
}

#[derive(Debug, Default, Copy, Clone)]
pub struct Font(u64);
