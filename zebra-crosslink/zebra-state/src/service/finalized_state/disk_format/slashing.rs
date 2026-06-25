//! Disk format for the localized delegation-interval index used by finalizer slashing.

use zebra_chain::block::Height;

use crate::service::finalized_state::disk_format::{
    block::HEIGHT_DISK_BYTES, BondKey, FromDisk, IntoDisk,
};

#[cfg(any(test, feature = "proptest-impl"))]
use proptest_derive::Arbitrary;

/// Key: `finalizer || start || bond`
/// Value: end height (exclusive), or MAX_ON_DISK_HEIGHT if still open as of the indexing watermark
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
#[cfg_attr(
    any(test, feature = "proptest-impl"),
    derive(Arbitrary, serde::Serialize, serde::Deserialize)
)]
pub struct SlashedBondKey {
    pub finalizer: [u8; 32], // PubKeyID
    pub start: Height,
    pub bond: BondKey,
}

const SLASHED_BOND_KEY_BYTES: usize = 32 + HEIGHT_DISK_BYTES + 32;

impl IntoDisk for SlashedBondKey {
    type Bytes = [u8; SLASHED_BOND_KEY_BYTES];

    fn as_bytes(&self) -> Self::Bytes {
        let mut bytes = [0u8; SLASHED_BOND_KEY_BYTES];
        bytes[0..32].copy_from_slice(&self.finalizer);
        bytes[32..32 + HEIGHT_DISK_BYTES].copy_from_slice(self.start.as_bytes().as_ref());
        bytes[32 + HEIGHT_DISK_BYTES..].copy_from_slice(&self.bond);
        bytes
    }
}

impl FromDisk for SlashedBondKey {
    fn from_bytes(bytes: impl AsRef<[u8]>) -> Self {
        let bytes = bytes.as_ref();
        let finalizer: [u8; 32] = bytes[0..32].try_into().expect("finalizer is 32 bytes");
        let start = Height::from_bytes(&bytes[32..32 + HEIGHT_DISK_BYTES]);
        let bond: BondKey = bytes[32 + HEIGHT_DISK_BYTES..SLASHED_BOND_KEY_BYTES]
            .try_into()
            .expect("bond key is 32 bytes");
        Self {
            finalizer,
            start,
            bond,
        }
    }
}

/// Single-row metadata for the slashed-bond index: how far the scan has gotten,
/// and which slashed set it was built for. `next_height` is the first finalized
/// height not yet incorporated (0 = nothing scanned). If `slashed` no longer
/// matches the configured set, the index is reset and rescanned from genesis.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SlashIndexMeta {
    pub next_height: Height,
    pub slashed: Vec<[u8; 32]>,
}

impl IntoDisk for SlashIndexMeta {
    type Bytes = Vec<u8>;

    fn as_bytes(&self) -> Self::Bytes {
        let mut bytes = Vec::with_capacity(HEIGHT_DISK_BYTES + 4 + self.slashed.len() * 32);
        bytes.extend_from_slice(self.next_height.as_bytes().as_ref());
        bytes.extend_from_slice(&(self.slashed.len() as u32).to_be_bytes());
        for finalizer in &self.slashed {
            bytes.extend_from_slice(finalizer);
        }
        bytes
    }
}

impl FromDisk for SlashIndexMeta {
    fn from_bytes(bytes: impl AsRef<[u8]>) -> Self {
        let bytes = bytes.as_ref();
        let next_height = Height::from_bytes(&bytes[0..HEIGHT_DISK_BYTES]);
        let count = u32::from_be_bytes(
            bytes[HEIGHT_DISK_BYTES..HEIGHT_DISK_BYTES + 4]
                .try_into()
                .expect("count is 4 bytes"),
        ) as usize;
        let base = HEIGHT_DISK_BYTES + 4;
        let mut slashed = Vec::with_capacity(count);
        for i in 0..count {
            let off = base + i * 32;
            slashed.push(bytes[off..off + 32].try_into().expect("finalizer is 32 bytes"));
        }
        Self {
            next_height,
            slashed,
        }
    }
}
