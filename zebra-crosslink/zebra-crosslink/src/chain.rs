//! Core Zcash Crosslink data structures
//!
//! This crate deals only with in-memory state validation and excludes I/O, tokio, services, etc...
//!
//! This crate is named similarly to [zebra_chain] since it has a similar scope. In a mature crosslink-enabled Zebra these two crates may be merged.
#![deny(unsafe_code, missing_docs)]

use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use serde::{Deserialize, Serialize};
use std::fmt::Debug;
use thiserror::Error;
use tracing::error;
use zebra_chain::block::Header as BcBlockHeader;

use zebra_chain::serialization::{
    ReadZcashExt, SerializationError, ZcashDeserialize, ZcashSerialize,
};

use crate::FatPointerToBftBlock2;

/// The BFT block content for Crosslink
///
/// # Constructing [BftBlock]s
///
/// A [BftBlock] may be constructed from a node's local view in order to create a new BFT proposal, or they may be constructed from unknown sources across a network protocol.
///
/// To construct a [BftBlock] for a new BFT proposal, build a [Vec] of [BcBlockHeader] values, starting from the latest known PoW tip and traversing back in time (following [previous_block_hash](BcBlockHeader::previous_block_hash)) until exactly [bc_confirmation_depth_sigma](ZcashCrosslinkParameters::bc_confirmation_depth_sigma) headers are collected, then pass this to [BftBlock::try_from].
///
/// To construct from an untrusted source, call the same [BftBlock::try_from].
///
/// ## Validation and Limitations
///
/// The [BftBlock::try_from] method is the only way to construct [BftBlock] values and performs the following validation internally:
///
/// 1. The number of headers matches the expected protocol confirmation depth, [bc_confirmation_depth_sigma](ZcashCrosslinkParameters::bc_confirmation_depth_sigma).
/// 2. The [version](BcBlockHeader::version) field is a known expected value.
/// 3. The headers are in the correct order given the [previous_block_hash](BcBlockHeader::previous_block_hash) fields.
/// 4. The PoW solutions validate.
///
/// These validations use *immediate data* and are *stateless*, and in particular the following stateful validations are **NOT** performed:
///
/// 1. The [difficulty_threshold](BcBlockHeader::difficulty_threshold) is within correct bounds for the Difficulty Adjustment Algorithm.
/// 2. The [time](BcBlockHeader::time) field is within correct bounds.
/// 3. The [merkle_root](BcBlockHeader::merkle_root) field is sensible.
///
/// No other validations are performed.
///
/// **TODO:** Ensure deserialization delegates to [BftBlock::try_from].
///
/// ## Design Notes
///
/// This *assumes* is is more natural to fetch the latest BC tip in Zebra, then to iterate to parent blocks, appending each to the [Vec]. This means the in-memory header order is *reversed from the specification* [^1]:
///
/// > Each bft‑proposal has, in addition to origbft‑proposal fields, a headers_bc field containing a sequence of exactly σ bc‑headers (zero‑indexed, deepest first).
///
/// The [TryFrom] impl performs internal validations and is the only way to construct a [BftBlock], whether locally generated or from an unknown source. This is the safest design, though potentially less efficient.
///
/// # References
///
/// [^1]: [Zcash Trailing Finality Layer §3.3.3 Structural Additions](https://electric-coin-company.github.io/tfl-book/design/crosslink/construction.html#structural-additions)
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)] //, Serialize, Deserialize)]
pub struct BftBlock {
    /// The Version Number
    pub version: u32,
    /// The Height of this BFT Payload
    // @Zooko: possibly not unique, may be bug-prone, maybe remove...
    pub height: u32,
    /// Hash of the previous BFT Block.
    pub previous_block_fat_ptr: FatPointerToBftBlock2,
    /// The height of the PoW block that is the finalization candidate.
    pub finalization_candidate_height: u32,
    /// The PoW Headers
    // @Zooko: PoPoW?
    pub headers: Vec<BcBlockHeader>,
}

impl ZcashSerialize for BftBlock {
    #[allow(clippy::unwrap_in_result)]
    fn zcash_serialize<W: std::io::Write>(&self, mut writer: W) -> Result<(), std::io::Error> {
        writer.write_u32::<LittleEndian>(self.version)?;
        writer.write_u32::<LittleEndian>(self.height)?;
        self.previous_block_fat_ptr.zcash_serialize(&mut writer);
        writer.write_u32::<LittleEndian>(self.finalization_candidate_height)?;
        writer.write_u32::<LittleEndian>(self.headers.len().try_into().unwrap())?;
        for header in &self.headers {
            header.zcash_serialize(&mut writer)?;
        }
        Ok(())
    }
}

impl ZcashDeserialize for BftBlock {
    fn zcash_deserialize<R: std::io::Read>(mut reader: R) -> Result<Self, SerializationError> {
        let version = reader.read_u32::<LittleEndian>()?;
        let height = reader.read_u32::<LittleEndian>()?;
        let previous_block_fat_ptr = FatPointerToBftBlock2::zcash_deserialize(&mut reader)?;
        let finalization_candidate_height = reader.read_u32::<LittleEndian>()?;
        let header_count = reader.read_u32::<LittleEndian>()?;
        if header_count > 2048 {
            // Fail on unreasonably large number.
            return Err(SerializationError::Parse(
                "header_count was greater than 2048.",
            ));
        }
        let mut array = Vec::new();
        for i in 0..header_count {
            array.push(zebra_chain::block::Header::zcash_deserialize(&mut reader)?);
        }

        Ok(BftBlock {
            version,
            height,
            previous_block_fat_ptr,
            finalization_candidate_height,
            headers: array,
        })
    }
}

impl BftBlock {
    /// Refer to the [BcBlockHeader] that is the finalization candidate for this block
    pub fn finalization_candidate(&self) -> &BcBlockHeader {
        &self.headers.first().expect("Vec should never be empty")
    }

    /// Attempt to construct a [BftBlock] from headers while performing immediate validations; see [BftBlock] type docs
    pub fn try_from(
        params: &ZcashCrosslinkParameters,
        height: u32,
        previous_block_fat_ptr: FatPointerToBftBlock2,
        finalization_candidate_height: u32,
        headers: Vec<BcBlockHeader>,
    ) -> Result<Self, InvalidBftBlock> {
        let expected = params.bc_confirmation_depth_sigma;
        let actual = headers.len() as u64;
        if actual != expected {
            return Err(InvalidBftBlock::IncorrectConfirmationDepth { expected, actual });
        }

        error!("not yet implemented: all the documented validations");

        Ok(BftBlock {
            version: 1,
            height,
            previous_block_fat_ptr,
            finalization_candidate_height,
            headers,
        })
    }

    /// Hash for the block
    /// ([BftBlock::hash]).
    pub fn blake3_hash(&self) -> Blake3Hash {
        self.into()
    }

    /// Just the hash of the previous block, which identifies it but does not provide any
    /// guarantees. Consider using the [`previous_block_fat_ptr`] instead
    pub fn previous_block_hash(&self) -> Blake3Hash {
        self.previous_block_fat_ptr.points_at_block_hash()
    }
}

impl<'a> From<&'a BftBlock> for Blake3Hash {
    fn from(block: &'a BftBlock) -> Self {
        let mut hash_writer = if *crate::TEST_MODE.lock().unwrap() {
            // Note(Sam): Only until we regenerate the test data.
            blake3::Hasher::new()
        } else {
            blake3::Hasher::new_keyed(&tenderlink::HashKeys::default().value_id.0)
        };
        block
            .zcash_serialize(&mut hash_writer)
            .expect("Sha256dWriter is infallible");
        Self(hash_writer.finalize().into())
    }
}

/// Validation error for [BftBlock]
#[derive(Debug, Error)]
pub enum InvalidBftBlock {
    /// An incorrect number of headers was present
    #[error(
        "invalid confirmation depth: Crosslink requires {expected} while {actual} were present"
    )]
    IncorrectConfirmationDepth {
        /// The expected number of headers, as per [bc_confirmation_depth_sigma](ZcashCrosslinkParameters::bc_confirmation_depth_sigma)
        expected: u64,
        /// The number of headers present
        actual: u64,
    },
}

/// Zcash Crosslink protocol parameters
///
/// This is provided as a trait so that downstream users can define or plug in their own alternative parameters.
///
/// Ref: [Zcash Trailing Finality Layer §3.3.3 Parameters](https://electric-coin-company.github.io/tfl-book/design/crosslink/construction.html#parameters)
#[derive(Clone, Debug)]
pub struct ZcashCrosslinkParameters {
    /// The best-chain confirmation depth, `σ`
    ///
    /// At least this many PoW blocks must be atop the PoW block used to obtain a finalized view.
    pub bc_confirmation_depth_sigma: u64,

    /// The depth of unfinalized PoW blocks past which "Stalled Mode" activates, `L`
    ///
    /// Quoting from [Zcash Trailing Finality Layer §3.3.3 Stalled Mode](https://electric-coin-company.github.io/tfl-book/design/crosslink/construction.html#stalled-mode):
    ///
    /// > In practice, L should be at least 2σ.
    pub finalization_gap_bound: u64,
}

/// Crosslink parameters chosed for prototyping / testing
///
/// <div class="warning">No verification has been done on the security or performance of these parameters.</div>
pub const PROTOTYPE_PARAMETERS: ZcashCrosslinkParameters = ZcashCrosslinkParameters {
    bc_confirmation_depth_sigma: 3,
    finalization_gap_bound: 7,
};

/// A BLAKE3 hash.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Copy, Serialize, Deserialize)]
pub struct Blake3Hash(pub [u8; 32]);

impl std::fmt::Display for Blake3Hash {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        for &b in self.0.iter() {
            write!(f, "{:02x}", b)?;
        }
        Ok(())
    }
}

impl std::fmt::Debug for Blake3Hash {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        for &b in self.0.iter() {
            write!(f, "{:02x}", b)?;
        }
        Ok(())
    }
}

impl ZcashSerialize for Blake3Hash {
    #[allow(clippy::unwrap_in_result)]
    fn zcash_serialize<W: std::io::Write>(&self, mut writer: W) -> Result<(), std::io::Error> {
        writer.write_all(&self.0);
        Ok(())
    }
}

impl ZcashDeserialize for Blake3Hash {
    fn zcash_deserialize<R: std::io::Read>(mut reader: R) -> Result<Self, SerializationError> {
        Ok(Blake3Hash(reader.read_32_bytes()?))
    }
}

/// A BFT block and the fat pointer that shows it has been signed
#[derive(Clone, Debug)]
pub struct BftBlockAndFatPointerToIt {
    /// A BFT block
    pub block: BftBlock,
    /// The fat pointer to block, showing it has been signed
    pub fat_ptr: FatPointerToBftBlock2,
}

impl ZcashDeserialize for BftBlockAndFatPointerToIt {
    fn zcash_deserialize<R: std::io::Read>(mut reader: R) -> Result<Self, SerializationError> {
        Ok(BftBlockAndFatPointerToIt {
            block: BftBlock::zcash_deserialize(&mut reader)?,
            fat_ptr: FatPointerToBftBlock2::zcash_deserialize(&mut reader)?,
        })
    }
}

impl ZcashSerialize for BftBlockAndFatPointerToIt {
    fn zcash_serialize<W: std::io::Write>(&self, mut writer: W) -> Result<(), std::io::Error> {
        self.block.zcash_serialize(&mut writer);
        self.fat_ptr.zcash_serialize(&mut writer);
        Ok(())
    }
}
