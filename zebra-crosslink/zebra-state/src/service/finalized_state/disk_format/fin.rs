//! Disk format for `fin`, the node-local finalized marker.
//!
//! One row, holding the height and hash of the block `fin` names. `fin` is node-local memory
//! rather than a function of the chain (FINALITY.md §3.2), so it has to survive a restart: a
//! node that forgot it could switch to a chain excluding a block it had already called final.

use zebra_chain::block::{self, Height};

use crate::service::finalized_state::disk_format::{FromDisk, IntoDisk};

/// The key of the single `fin` row.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct FinKey;

impl IntoDisk for FinKey {
    type Bytes = [u8; 1];

    fn as_bytes(&self) -> Self::Bytes {
        [0]
    }
}

impl FromDisk for FinKey {
    fn from_bytes(_bytes: impl AsRef<[u8]>) -> Self {
        FinKey
    }
}

/// The block `fin` names.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct FinMarker {
    pub height: Height,
    pub hash: block::Hash,
}

impl IntoDisk for FinMarker {
    type Bytes = [u8; 36];

    fn as_bytes(&self) -> Self::Bytes {
        let mut bytes = [0u8; 36];
        bytes[..4].copy_from_slice(&self.height.0.to_be_bytes());
        bytes[4..].copy_from_slice(&self.hash.0);
        bytes
    }
}

impl FromDisk for FinMarker {
    fn from_bytes(bytes: impl AsRef<[u8]>) -> Self {
        let bytes = bytes.as_ref();
        assert_eq!(bytes.len(), 36, "fin marker is a height and a hash");
        FinMarker {
            height: Height(u32::from_be_bytes(bytes[..4].try_into().unwrap())),
            hash: block::Hash(bytes[4..].try_into().unwrap()),
        }
    }
}
