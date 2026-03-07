
#[inline]
pub fn store_u64(buf: &mut [u8], value: u64) {
    debug_assert!(buf.len() == 8);
    buf[..8].copy_from_slice(&value.to_le_bytes());
}
#[inline]
pub fn load_u64(buf: &[u8]) -> u64 {
    debug_assert!(buf.len() == 8);
    u64::from_le_bytes(buf[..8].try_into().unwrap())
}
#[inline]
pub fn store_u48(buf: &mut [u8], value: u64) {
    debug_assert!(buf.len() == 6);
    buf.copy_from_slice(&value.to_le_bytes()[..6]);
}
#[inline]
pub fn load_u48(buf: &[u8]) -> u64 {
    debug_assert!(buf.len() == 6);

    let mut tmp = [0u8; 8];
    tmp[..6].copy_from_slice(buf);
    u64::from_le_bytes(tmp)
}
#[inline]
pub fn store_u24(buf: &mut [u8], value: u32) {
    debug_assert!(buf.len() == 3);
    buf.copy_from_slice(&value.to_le_bytes()[..3]);
}
#[inline]
pub fn load_u24(buf: &[u8]) -> u32 {
    debug_assert!(buf.len() == 3);

    let mut tmp = [0u8; 4];
    tmp[..3].copy_from_slice(buf);
    u32::from_le_bytes(tmp)
}
#[inline]
pub fn store_u16(buf: &mut [u8], value: u16) {
    debug_assert!(buf.len() == 2);
    buf.copy_from_slice(&value.to_le_bytes()[..2]);
}
#[inline]
pub fn load_u16(buf: &[u8]) -> u16 {
    debug_assert!(buf.len() == 2);

    let mut tmp = [0u8; 2];
    tmp[..2].copy_from_slice(buf);
    u16::from_le_bytes(tmp)
}

/// Bytes per second, formatted in binary units (KiB/MiB/GiB/TiB).
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct BytesPerSecond(pub u64);

impl BytesPerSecond {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * 1024 * 1024;
    const TIB: u64 = 1024 * 1024 * 1024 * 1024;

    pub fn best_unit(bps: u64) -> (u64, &'static str) {
        if bps >= Self::TIB {
            (Self::TIB, "TiB/s")
        } else if bps >= Self::GIB {
            (Self::GIB, "GiB/s")
        } else if bps >= Self::MIB {
            (Self::MIB, "MiB/s")
        } else if bps >= Self::KIB {
            (Self::KIB, "KiB/s")
        } else {
            (1, "B/s")
        }
    }

    pub fn format_value(value: u64, unit: u64) -> (u64, u64) {
        // integer + 2-decimal fixed point, rounded half-up:
        // scaled = round(value * 100 / unit)
        let scaled = (value.saturating_mul(100) + unit / 2) / unit;
        (scaled / 100, scaled % 100)
    }
}

impl std::fmt::Display for BytesPerSecond {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let bps = self.0;
        let (unit, suffix) = Self::best_unit(bps);

        if unit == 1 {
            return write!(f, "{} {}", bps, suffix);
        }

        let (whole, frac) = Self::format_value(bps, unit);

        // If it's an exact integer in that unit, print without decimals.
        if frac == 0 {
            write!(f, "{} {}", whole, suffix)
        } else if whole >= 10 {
            // For >= 10, 1 decimal place is usually plenty.
            let one_decimal = (bps.saturating_mul(10) + unit / 2) / unit; // rounded
            write!(f, "{}.{:01} {}", one_decimal / 10, one_decimal % 10, suffix)
        } else {
            // For < 10, print 2 decimals for a bit more resolution.
            write!(f, "{}.{:02} {}", whole, frac, suffix)
        }
    }
}

impl std::fmt::Debug for BytesPerSecond {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Debug prints both the pretty display and the raw value.
        f.debug_tuple("BytesPerSecond")
            .field(&format_args!("{}", self))
            .field(&self.0)
            .finish()
    }
}
