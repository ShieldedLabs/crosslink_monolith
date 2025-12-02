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

pub trait VizColor {
    fn as_viz_color(self) -> u32;
}

impl VizColor for (u8, u8, u8, u8) {
    fn as_viz_color(self) -> u32 {
        let (r, g, b, a) = self;
        ((a as u32) << 24) | ((b as u32) << 16) | ((g as u32) << 8) | (r as u32)
    }
}
