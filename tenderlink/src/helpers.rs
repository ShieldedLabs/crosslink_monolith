
// NOTE: buf can be open-ended
pub trait SliceWrite     { fn write_to(&self, buf: &mut [u8]) -> usize; }
impl SliceWrite for u128 { fn write_to(&self, buf: &mut [u8]) -> usize { buf[0..16].copy_from_slice(&self.to_le_bytes()); 16 } }
impl SliceWrite for i128 { fn write_to(&self, buf: &mut [u8]) -> usize { buf[0..16].copy_from_slice(&self.to_le_bytes()); 16 } }
impl SliceWrite for u64  { fn write_to(&self, buf: &mut [u8]) -> usize { buf[0.. 8].copy_from_slice(&self.to_le_bytes());  8 } }
impl SliceWrite for i64  { fn write_to(&self, buf: &mut [u8]) -> usize { buf[0.. 8].copy_from_slice(&self.to_le_bytes());  8 } }
impl SliceWrite for u32  { fn write_to(&self, buf: &mut [u8]) -> usize { buf[0.. 4].copy_from_slice(&self.to_le_bytes());  4 } }
impl SliceWrite for i32  { fn write_to(&self, buf: &mut [u8]) -> usize { buf[0.. 4].copy_from_slice(&self.to_le_bytes());  4 } }
impl SliceWrite for u16  { fn write_to(&self, buf: &mut [u8]) -> usize { buf[0.. 2].copy_from_slice(&self.to_le_bytes());  2 } }
impl SliceWrite for i16  { fn write_to(&self, buf: &mut [u8]) -> usize { buf[0.. 2].copy_from_slice(&self.to_le_bytes());  2 } }
impl SliceWrite for u8   { fn write_to(&self, buf: &mut [u8]) -> usize { buf[0] = *self as u8;                                   1 } }
impl SliceWrite for i8   { fn write_to(&self, buf: &mut [u8]) -> usize { buf[0] = *self as u8;                                   1 } }
impl SliceWrite for [u8] { fn write_to(&self, buf: &mut [u8]) -> usize { buf[0..self.len()].copy_from_slice(self);      self.len() } }

pub trait SliceRead: Sized { fn read_from(buf: &mut &[u8]) -> Option<Self>; }
impl SliceRead for u128    { fn read_from(buf: &mut &[u8]) -> Option<Self> { if buf.len() < 16 { return None; } let (b, rest) = buf.split_at(16); *buf = rest; Some(u128::from_le_bytes(b.try_into().unwrap())) } }
impl SliceRead for i128    { fn read_from(buf: &mut &[u8]) -> Option<Self> { if buf.len() < 16 { return None; } let (b, rest) = buf.split_at(16); *buf = rest; Some(i128::from_le_bytes(b.try_into().unwrap())) } }
impl SliceRead for u64     { fn read_from(buf: &mut &[u8]) -> Option<Self> { if buf.len() <  8 { return None; } let (b, rest) = buf.split_at( 8); *buf = rest; Some( u64::from_le_bytes(b.try_into().unwrap())) } }
impl SliceRead for i64     { fn read_from(buf: &mut &[u8]) -> Option<Self> { if buf.len() <  8 { return None; } let (b, rest) = buf.split_at( 8); *buf = rest; Some( i64::from_le_bytes(b.try_into().unwrap())) } }
impl SliceRead for u32     { fn read_from(buf: &mut &[u8]) -> Option<Self> { if buf.len() <  4 { return None; } let (b, rest) = buf.split_at( 4); *buf = rest; Some( u32::from_le_bytes(b.try_into().unwrap())) } }
impl SliceRead for i32     { fn read_from(buf: &mut &[u8]) -> Option<Self> { if buf.len() <  4 { return None; } let (b, rest) = buf.split_at( 4); *buf = rest; Some( i32::from_le_bytes(b.try_into().unwrap())) } }
impl SliceRead for u16     { fn read_from(buf: &mut &[u8]) -> Option<Self> { if buf.len() <  2 { return None; } let (b, rest) = buf.split_at( 2); *buf = rest; Some( u16::from_le_bytes(b.try_into().unwrap())) } }
impl SliceRead for i16     { fn read_from(buf: &mut &[u8]) -> Option<Self> { if buf.len() <  2 { return None; } let (b, rest) = buf.split_at( 2); *buf = rest; Some( i16::from_le_bytes(b.try_into().unwrap())) } }
impl SliceRead for u8      { fn read_from(buf: &mut &[u8]) -> Option<Self> { if buf.len() <  1 { return None; } let v = buf[0];        *buf = &buf[1..]; Some(v)       } }
impl SliceRead for i8      { fn read_from(buf: &mut &[u8]) -> Option<Self> { if buf.len() <  1 { return None; } let v = buf[0] as i8;  *buf = &buf[1..]; Some(v)       } }
impl<const N: usize> SliceRead for [u8; N] {
    fn read_from(buf: &mut &[u8]) -> Option<Self> { if buf.len() < N { return None; } let (b, rest) = buf.split_at(N); *buf = rest; Some(b.try_into().unwrap()) }
}

// Byte helpers and the rate type come from the transport crate.
pub use shielded_transport_pigeon::helpers::{
    store_u64, load_u64, store_u48, load_u48, store_u32, load_u32,
    store_u24, load_u24, store_u16, load_u16,
    BytesPerSecond,
};

use shielded_transport_pigeon::stp::ConnectionKey;
use std::net::Ipv6Addr;

impl SliceWrite for ConnectionKey {
    fn write_to(&self, buf: &mut [u8]) -> usize {
        let mut o = 0;
        o += self.ip.octets().write_to(&mut buf[o..]);
        o += self.port       .write_to(&mut buf[o..]);
        o += self.key_15_bits.write_to(&mut buf[o..]);
        o
    }
}
impl SliceRead for ConnectionKey {
    fn read_from(buf: &mut &[u8]) -> Option<Self> {
        Some(Self {
            ip:          Ipv6Addr::from(<[u8; 16]>::read_from(buf)?),
            port:        SliceRead::read_from(buf)?,
            key_15_bits: SliceRead::read_from(buf)?,
        })
    }
}
