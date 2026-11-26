pub const WHITE: (u8, u8, u8, u8) = (0xff, 0xff, 0xff, 0xff);
pub const BLACK: (u8, u8, u8, u8) = (0x00, 0x00, 0x00, 0xff);

pub const RED:      (u8, u8, u8, u8) = (0xdc, 0x4c, 0x4f, 0xff);
pub const RED_DARK: (u8, u8, u8, u8) = (0x9a, 0x2d, 0x37, 0xff);

pub const GREEN:      (u8, u8, u8, u8) = (0x4c, 0xdc, 0x72, 0xff);
pub const GREEN_DARK: (u8, u8, u8, u8) = (0x2d, 0x9a, 0x5e, 0xff);

pub const BLUE:      (u8, u8, u8, u8) = (0x4c, 0x69, 0xdc, 0xff);
pub const BLUE_DARK: (u8, u8, u8, u8) = (0x2d, 0x4e, 0x9a, 0xff);

pub const GOLD:      (u8, u8, u8, u8) = (0xff, 0xaf, 0x0e, 0xff);
pub const GOLD_DARK: (u8, u8, u8, u8) = (0x8a, 0x62, 0x12, 0xff);

// Text sizes, in unscaled pixels, chosen at scale 1 on a 1080p display. Five, each a clear
// step from the next; a text that matters less is smaller or dimmer, never both a pixel off.
/// Captions, units, hashes, fees, memos, axis labels: what is read second.
pub const TEXT_SMALL: f32 = 12.0;
/// Body text, buttons, tabs, list titles, amounts in lists.
pub const TEXT_BODY: f32 = 14.0;
/// A dialog's title, a pane's heading, the amount a dialog is about.
pub const TEXT_HEADING: f32 = 18.0;
/// The wallet's balance, the one first thing on the main screen.
pub const TEXT_HERO: f32 = 36.0;

// Icon sizes: an icon beside text matches the text's size; these are the icons that stand alone.
/// An icon on a round action button.
pub const ICON_ACTION: f32 = 20.0;
/// An icon that fills an empty list's space.
pub const ICON_EMPTY: f32 = 40.0;

// Control sizes, in unscaled pixels.
/// The height of a button with a text label.
pub const BUTTON_H: f32 = 32.0;
/// The diameter of a round action button under the balance.
pub const ACTION_D: f32 = 40.0;
/// The height of a tab in a pane's tab bar.
pub const TAB_H: f32 = 36.0;
/// One transaction in the history, separator included.
pub const HISTORY_ROW_H: f32 = 64.0;
/// A list's scroll bar, its margins included.
pub const SCROLLBAR_W: f32 = 20.0;
/// One finalizer in the roster.
pub const ROSTER_ROW_H: f32 = 36.0;

// Pane widths, in unscaled pixels. Each side pane has the width its content needs, not a
// share of the window; the block map between them takes what is left, and gives up room
// first. Below that the panes shrink toward their least widths, together.
/// The wallet pane, divider included. Holds a 36 px balance and five action buttons.
pub const LEFT_PANE_W: f32 = 400.0;
pub const LEFT_PANE_MIN: f32 = 340.0;
/// The finalizer pane, divider included.
pub const RIGHT_PANE_W: f32 = 380.0;
pub const RIGHT_PANE_MIN: f32 = 320.0;
/// The widest a divider can make a side pane.
pub const PANE_MAX: f32 = 720.0;
/// The least the block map keeps between the panes: its two height strips and the network box.
pub const CENTRE_MIN_W: f32 = 340.0;

pub trait VizColor {
    fn as_viz_color(self) -> u32;
}

impl VizColor for (u8, u8, u8, u8) {
    fn as_viz_color(self) -> u32 {
        let (r, g, b, a) = self;
        ((a as u32) << 24) | ((b as u32) << 16) | ((g as u32) << 8) | (r as u32)
    }
}
