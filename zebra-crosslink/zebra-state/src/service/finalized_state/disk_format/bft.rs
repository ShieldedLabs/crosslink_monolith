//! Disk format for the decided BFT chain.
//!
//! One row per BFT height in each of three column families: the block, the fat pointer that
//! names it, and the proposal signatures that came with it. The roster is deliberately not
//! stored: it is recomputed from the bonds at each block's snapshot, so a node can never load a
//! validator set that disagrees with the votes, which travel by roster index (FINALITY.md §8.1).

use zcash_primitives::bft::{BftBlock, FatPointerToBftBlock, TMSig};
use zebra_chain::serialization::{ZcashDeserialize, ZcashSerialize};

use crate::service::finalized_state::disk_format::{FromDisk, IntoDisk};

/// A BFT height, used as the key of every BFT column family. Big-endian so a range scan walks
/// the chain in order.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct BftHeight(pub u32);

impl IntoDisk for BftHeight {
    type Bytes = [u8; 4];

    fn as_bytes(&self) -> Self::Bytes {
        self.0.to_be_bytes()
    }
}

impl FromDisk for BftHeight {
    fn from_bytes(bytes: impl AsRef<[u8]>) -> Self {
        BftHeight(u32::from_be_bytes(bytes.as_ref().try_into().expect("BFT height is 4 bytes")))
    }
}

/// A decided BFT block, in its network serialization.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredBftBlock(pub BftBlock);

impl IntoDisk for StoredBftBlock {
    type Bytes = Vec<u8>;

    fn as_bytes(&self) -> Self::Bytes {
        self.0.zcash_serialize_to_vec().expect("a decided BFT block serializes")
    }
}

impl FromDisk for StoredBftBlock {
    fn from_bytes(bytes: impl AsRef<[u8]>) -> Self {
        StoredBftBlock(
            BftBlock::zcash_deserialize(bytes.as_ref())
                .expect("deserialization format should match the serialization format used by IntoDisk"),
        )
    }
}

/// The fat pointer naming a decided BFT block, in its network serialization.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredFatPointer(pub FatPointerToBftBlock);

impl IntoDisk for StoredFatPointer {
    type Bytes = Vec<u8>;

    fn as_bytes(&self) -> Self::Bytes {
        let mut bytes = Vec::new();
        self.0.zcash_serialize(&mut bytes).expect("a fat pointer serializes");
        bytes
    }
}

impl FromDisk for StoredFatPointer {
    fn from_bytes(bytes: impl AsRef<[u8]>) -> Self {
        StoredFatPointer(
            FatPointerToBftBlock::zcash_deserialize(bytes.as_ref())
                .expect("deserialization format should match the serialization format used by IntoDisk"),
        )
    }
}

/// The proposal signatures carried with a decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProposalSignatures(pub Vec<TMSig>);

impl IntoDisk for ProposalSignatures {
    type Bytes = Vec<u8>;

    fn as_bytes(&self) -> Self::Bytes {
        let mut bytes = Vec::with_capacity(4 + self.0.len() * 64);
        bytes.extend_from_slice(&(self.0.len() as u32).to_be_bytes());
        for sig in &self.0 {
            bytes.extend_from_slice(&sig.0);
        }
        bytes
    }
}

impl FromDisk for ProposalSignatures {
    fn from_bytes(bytes: impl AsRef<[u8]>) -> Self {
        let bytes = bytes.as_ref();
        assert!(bytes.len() >= 4, "ProposalSignatures needs at least 4 bytes for count");
        let count = u32::from_be_bytes(bytes[0..4].try_into().unwrap()) as usize;
        assert_eq!(bytes.len(), 4 + count * 64, "ProposalSignatures byte length mismatch");
        let mut sigs = Vec::with_capacity(count);
        for i in 0..count {
            let offset = 4 + i * 64;
            sigs.push(TMSig(bytes[offset..offset + 64].try_into().unwrap()));
        }
        ProposalSignatures(sigs)
    }
}
