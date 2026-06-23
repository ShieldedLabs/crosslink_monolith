//! Core Zcash Crosslink data structureserr//!
//! This crate deals only with in-memory state validation and excludes I/O, tokio, services, etc...
//!
//! This crate is named similarly to [zebra_chain] since it has a similar scope. In a mature crosslink-enabled Zebra these two crates may be merged.
// #![deny(unsafe_code, missing_docs)]

use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use ed25519_zebra::{Signature, SigningKey, VerificationKeyBytes, VerificationKey};
use serde::{Deserialize, Serialize};
use std::fmt::Debug;
use std::vec::Vec;
use tracing::error;
use crate::block::{ BlockHeaderData as BcBlockHeader, BlockHeader as BcBlockHeaderWrap };

// use zebra_chain::serialization::{
//     ReadZcashExt, SerializationError, ZcashDeserialize, ZcashSerialize,
// };

/// A single user-led hardfork rule.
///
/// This is the canonical per-rule type, shared by node config (in `zebra-chain`,
/// which re-exports it) and by [`BftBlock`], which embeds an optional entry from
/// version 2 onward.
///
/// When carried in a [`BftBlock`] the byte layout produced by [`zcash_serialize`]
/// is consensus-critical: it is committed to by the block hash and therefore by
/// finalizer signatures (see [`BftBlock`] docs). Do not change the layout without
/// bumping the [`BftBlock`] version.
///
/// [`zcash_serialize`]: HardForkConfig::zcash_serialize
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HardForkConfig {
    /// The PoW block height at which this hardfork activates.
    #[serde(deserialize_with = "deserialize_staking_aligned")]
    pub pow_activation_height: u64,
    /// The BFT certificate height at which this hardfork activates.
    pub bft_certificate_height: u64,
    /// Finalizers terminated by this hardfork; must list at least one. Committed
    /// to by hash later, so this list is sorted into a canonical order when the
    /// schedule is built.
    #[serde(deserialize_with = "deserialize_unique_finalizers")]
    pub terminated_finalizers: Vec<PubKeyID>,
}

fn deserialize_staking_aligned<'de, D>(d: D) -> Result<u64, D::Error> where D: serde::Deserializer<'de> {
    let n = u64::deserialize(d)?; // @Todo: should be u32
    let period = crate::transaction::STAKING_PERIOD as u64; // @Todo: u32
    if n == 0 {
        return Err(serde::de::Error::custom(format!("`pow_activation_height` must be greater than zero and a multiple of the staking period ({period})")));
    }
    if n % period != 0 {
        return Err(serde::de::Error::custom(format!("`pow_activation_height` must be a multiple of the staking period ({period}); got {n} (nearest valid: {} or {})",
                                                    n / period * period, (n / period + 1) * period)));
    }
    Ok(n)
}

fn deserialize_unique_finalizers<'de, D>(d: D) -> Result<Vec<PubKeyID>, D::Error> where D: serde::Deserializer<'de> {
    let finalizers = Vec::<PubKeyID>::deserialize(d)?;
    if finalizers.is_empty() {
        return Err(serde::de::Error::custom("`terminated_finalizers` must list at least one finalizer"));
    }
    let mut seen = std::collections::HashSet::with_capacity(finalizers.len());
    for finalizer in &finalizers {
        if !seen.insert(finalizer) {
            return Err(serde::de::Error::custom(format!("`terminated_finalizers` contains duplicate finalizer \"{finalizer}\", which was already specified")));
        }
    }
    Ok(finalizers)
}

impl HardForkConfig {
    /// Upper bound on `terminated_finalizers` accepted by [`zcash_deserialize`],
    /// to bound allocation from untrusted input.
    ///
    /// [`zcash_deserialize`]: HardForkConfig::zcash_deserialize
    const MAX_TERMINATED_FINALIZERS: u32 = 4096;

    #[allow(clippy::unwrap_in_result)]
    pub fn zcash_serialize<W: std::io::Write>(&self, mut writer: W) -> Result<(), std::io::Error> {
        writer.write_u64::<LittleEndian>(self.pow_activation_height)?;
        writer.write_u64::<LittleEndian>(self.bft_certificate_height)?;
        writer.write_u32::<LittleEndian>(self.terminated_finalizers.len().try_into().unwrap())?;
        for finalizer in &self.terminated_finalizers {
            writer.write_all(&finalizer.0)?;
        }
        Ok(())
    }

    pub fn zcash_deserialize<R: std::io::Read>(mut reader: R) -> Result<Self, std::io::Error> {
        let pow_activation_height = reader.read_u64::<LittleEndian>()?;
        let bft_certificate_height = reader.read_u64::<LittleEndian>()?;
        let count = reader.read_u32::<LittleEndian>()?;
        if count > Self::MAX_TERMINATED_FINALIZERS {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "terminated_finalizers count exceeds maximum",
            ));
        }
        let mut terminated_finalizers = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let mut bytes = [0u8; 32];
            reader.read_exact(&mut bytes)?;
            terminated_finalizers.push(PubKeyID(bytes));
        }
        Ok(HardForkConfig {
            pow_activation_height,
            bft_certificate_height,
            terminated_finalizers,
        })
    }
}

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
/// ## Versioning
///
/// The serialized form is consensus-critical (it is blake3-hashed, and that hash
/// is the value finalizers sign — see [Blake3Hash] and [Vote]). New fields are
/// therefore gated on [version]: `version >= 2` blocks serialize the [hardfork]
/// and [do_not_include_until_bc_height] tail, while earlier versions serialize
/// byte-for-byte as before, leaving their hashes and signatures unchanged.
///
/// [version]: BftBlock::version
/// [hardfork]: BftBlock::hardfork
/// [do_not_include_until_bc_height]: BftBlock::do_not_include_until_bc_height
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
#[derive(Clone, Debug, PartialEq, Eq)]//, serde::Serialize, serde::Deserialize)]
pub struct BftBlock {
    /// The Version Number
    pub version: u32,
    /// The Height of this BFT Payload
    // @Zooko: possibly not unique, may be bug-prone, maybe remove...
    pub height: u32,
    /// Hash of the previous BFT Block.
    pub previous_block_fat_ptr: FatPointerToBftBlock,
    /// The height of the PoW block that is the finalization candidate.
    pub finalization_candidate_height: u32,
    /// The PoW Headers
    // @Zooko: PoPoW?
    pub headers: Vec<BcBlockHeader>,
    /// A single user-led hardfork rule activated by this block, if any.
    ///
    /// Serialized only in `version >= 2` blocks. For earlier versions this must
    /// be `None`: it is not serialized and so not covered by the block hash or
    /// finalizer signatures.
    pub hardfork: Option<HardForkConfig>,
    /// BC height before which this BFT block must not be included.
    ///
    /// Serialized only in `version >= 2` blocks. For earlier versions this must
    /// be `0`: it is not serialized and so not covered by the block hash or
    /// finalizer signatures.
    pub do_not_include_until_bc_height: u64,
}

impl BftBlock {
// impl ZcashSerialize for BftBlock {
    #[allow(clippy::unwrap_in_result)]
    pub fn zcash_serialize<W: std::io::Write>(&self, mut writer: W) -> Result<(), std::io::Error> {
        writer.write_u32::<LittleEndian>(self.version)?;
        writer.write_u32::<LittleEndian>(self.height)?;
        self.previous_block_fat_ptr.zcash_serialize(&mut writer);
        writer.write_u32::<LittleEndian>(self.finalization_candidate_height)?;
        writer.write_u32::<LittleEndian>(self.headers.len().try_into().unwrap())?;
        for header in &self.headers {
            // header_data.zcash_serialize(&mut writer)?;
            BcBlockHeaderWrap::write_data(header, &mut writer)?;
        }
        // Version 2 tail. Earlier versions end here, so their serialization (and
        // hence hash and signatures) is unchanged.
        if self.version >= 2 {
            match &self.hardfork {
                Some(hardfork) => {
                    writer.write_u8(1)?;
                    hardfork.zcash_serialize(&mut writer)?;
                }
                None => writer.write_u8(0)?,
            }
            writer.write_u64::<LittleEndian>(self.do_not_include_until_bc_height)?;
        }
        Ok(())
    }
// }

// impl ZcashDeserialize for BftBlock {
    pub fn zcash_deserialize<R: std::io::Read>(mut reader: R) -> Result<Self, std::io::Error> { // SerializationError> {
        let version = reader.read_u32::<LittleEndian>()?;
        let height = reader.read_u32::<LittleEndian>()?;
        let previous_block_fat_ptr = FatPointerToBftBlock::zcash_deserialize(&mut reader)?;
        let finalization_candidate_height = reader.read_u32::<LittleEndian>()?;
        let header_count = reader.read_u32::<LittleEndian>()?;
        if header_count > 2048 {
            // Fail on unreasonably large number.
            // return Err(SerializationError::Parse(
            //     "header_count was greater than 2048.",
            // ));
            return Err(std::io::Error::new(std::io::ErrorKind::Other,
                "header_count was greater than 2048.",
            ));
        }
        let mut array = Vec::new();
        for i in 0..header_count {
            // array.push(zebra_chain::block::Header::zcash_deserialize(&mut reader)?);
            array.push(BcBlockHeaderWrap::read_data(&mut reader)?);
        }

        // Version 2 tail; see zcash_serialize. Earlier versions default to NIL.
        let (hardfork, do_not_include_until_bc_height) = if version >= 2 {
            let hardfork = match reader.read_u8()? {
                0 => None,
                1 => Some(HardForkConfig::zcash_deserialize(&mut reader)?),
                _ => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Other,
                        "invalid hardfork presence flag",
                    ))
                }
            };
            let do_not_include_until_bc_height = reader.read_u64::<LittleEndian>()?;
            (hardfork, do_not_include_until_bc_height)
        } else {
            (None, 0)
        };

        Ok(BftBlock {
            version,
            height,
            previous_block_fat_ptr,
            finalization_candidate_height,
            headers: array,
            hardfork,
            do_not_include_until_bc_height,
        })
    }
// }

    pub fn zcash_serialize_to_vec(&self) -> Result<Vec<u8>, std::io::Error> {
        let mut data = Vec::new();
        self.zcash_serialize(&mut data)?;
        Ok(data)
    }


    /// Refer to the [BcBlockHeader] that is the finalization candidate for this block
    pub fn finalization_candidate(&self) -> &BcBlockHeader {
        &self.headers.first().expect("Vec should never be empty")
    }

    /// Attempt to construct a [BftBlock] from headers while performing immediate validations; see [BftBlock] type docs
    pub fn try_from(
        params: &ZcashCrosslinkParameters,
        height: u32,
        previous_block_fat_ptr: FatPointerToBftBlock,
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
            hardfork: None,
            do_not_include_until_bc_height: 0,
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
        let mut hasher = HashKeys::default().value_id.hasher();
        block
            .zcash_serialize(&mut hasher)
            .expect("Sha256dWriter is infallible");
        Self(hasher.finalize().into())
    }
}

/// Validation error for [BftBlock]
#[derive(Debug)]
pub enum InvalidBftBlock {
    /// An incorrect number of headers was present
    // #[error(
    //     "invalid confirmation depth: Crosslink requires {expected} while {actual} were present"
    // )]
    IncorrectConfirmationDepth {
        /// The expected number of headers, as per [bc_confirmation_depth_sigma](ZcashCrosslinkParameters::bc_confirmation_depth_sigma)
        expected: u64,
        /// The number of headers present
        actual: u64,
    },
}
impl std::fmt::Display for InvalidBftBlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InvalidBftBlock::IncorrectConfirmationDepth{ expected, actual } => {
                write!(f, "invalid confirmation depth: Crosslink requires {expected} while {actual} were present")
            }
        }
    }
}
impl std::error::Error for InvalidBftBlock {}

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

impl Blake3Hash {
// impl ZcashSerialize for Blake3Hash {
    #[allow(clippy::unwrap_in_result)]
    pub fn zcash_serialize<W: std::io::Write>(&self, mut writer: W) -> Result<(), std::io::Error> {
        writer.write_all(&self.0);
        Ok(())
    }
// }

// impl ZcashDeserialize for Blake3Hash {
    pub fn zcash_deserialize<R: std::io::Read>(mut reader: R) -> Result<Self, std::io::Error> { // SerializationError> {
        let mut hash = Blake3Hash([0;32]);
        reader.read_exact(&mut hash.0)?;
        Ok(hash)
    }
// }
}

/// A BFT block and the fat pointer that shows it has been signed
#[derive(Clone, Debug)]
pub struct BftBlockAndFatPointerToIt {
    /// A BFT block
    pub block: BftBlock,
    /// The fat pointer to block, showing it has been signed
    pub fat_ptr: FatPointerToBftBlock,
}

impl BftBlockAndFatPointerToIt {
// impl ZcashDeserialize for BftBlockAndFatPointerToIt {
    pub fn zcash_deserialize<R: std::io::Read>(mut reader: R) -> Result<Self, std::io::Error> { // SerializationError> {
        Ok(BftBlockAndFatPointerToIt {
            block: BftBlock::zcash_deserialize(&mut reader)?,
            fat_ptr: FatPointerToBftBlock::zcash_deserialize(&mut reader)?,
        })
    }
// }

// impl ZcashSerialize for BftBlockAndFatPointerToIt {
    pub fn zcash_serialize<W: std::io::Write>(&self, mut writer: W) -> Result<(), std::io::Error> {
        self.block.zcash_serialize(&mut writer);
        self.fat_ptr.zcash_serialize(&mut writer);
        Ok(())
    }
// }


    /// Generate a bundle of the block hash and signatures affirming its validity
    pub fn from_parts(block: BftBlock, height: u64, round: u32, signatures: &[FatPointerSignature]) -> Self {
        let hash = block.blake3_hash();
        Self {
            block,
            fat_ptr: FatPointerToBftBlock::from_parts(hash, height, round, signatures),
        }
    }
}

/*
FROM ZEBRA
DATA LAYOUT FOR VOTE
32 byte ed25519 public key of the finalizer who's vote this is
32 byte blake3 hash of value, or all zeroes to indicate Nil vote
8 byte height
4 byte round where MSB is used to indicate is_commit for the vote type. 1 bit is_commit, 31 bits round index
// TODO: do we want height, round, vote type?

TOTAL: 76 B

A signed vote will be this same layout followed by the 64 byte ed25519 signature of the previous 76 bytes.
*/

pub fn fmt_byte_str(f: &mut std::fmt::Formatter<'_>, bytes: &[u8]) -> std::fmt::Result {
    let n = usize::min(bytes.len(), f.precision().unwrap_or(bytes.len()));
    for i in 0..n { write!(f, "{:02x}", bytes[i])?; }
    Ok(())
}

pub fn fmt_byte_str_rev(f: &mut std::fmt::Formatter<'_>, bytes: &[u8]) -> std::fmt::Result {
    let n = usize::min(bytes.len(), f.precision().unwrap_or(bytes.len()));
    for i in 0..n { write!(f, "{:02x}", bytes[n-(i+1)])?; }
    Ok(())
}

pub fn fmt_prefixed_byte_str(f: &mut std::fmt::Formatter<'_>, pre: &str, bytes: &[u8]) -> std::fmt::Result {
    write!(f, "{}", pre)?;
    fmt_byte_str(f, bytes)
}

pub fn fmt_prefixed_byte_str_rev(f: &mut std::fmt::Formatter<'_>, pre: &str, bytes: &[u8]) -> std::fmt::Result {
    write!(f, "{}", pre)?;
    fmt_byte_str_rev(f, bytes)
}


/// equivalent to [ed25519_zebra::VerificationKeyBytes]
#[derive(Default, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PubKeyID(pub [u8; 32]);
impl PubKeyID { pub const NIL: Self = Self([0; 32]); }
impl std::fmt::Display for PubKeyID { fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { fmt_byte_str_rev(f, &self.0) } }
impl std::fmt::Debug   for PubKeyID { fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { fmt_prefixed_byte_str_rev(f, "Pub{", &self.0[..2])?; write!(f, "}}") } }

impl serde::Serialize for PubKeyID {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where S: serde::Serializer,
    {
        let mut le_bytes = self.0;
        le_bytes.reverse();

        let mut buf = [0u8; 64];
        hex::encode_to_slice(&le_bytes, &mut buf).map_err(serde::ser::Error::custom)?;
        let le_str = std::str::from_utf8(&buf).expect("encoding should be ASCII-only");
        le_str.serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for PubKeyID {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let le_str = std::borrow::Cow::<'de, str>::deserialize(deserializer)?;
        let le_str: &str = le_str.as_ref();
        if le_str.len() != 64 {
            return Err(serde::de::Error::invalid_length(le_str.len(), &"32 bytes => 64 hex characters"));
        }

        let mut buf = [0u8; 32];
        hex::decode_to_slice(le_str, &mut buf).map_err(serde::de::Error::custom)?;
        buf.reverse();

        Ok(PubKeyID(buf))
    }
}


#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TMSig(pub [u8; 64]);
impl Default for TMSig { fn default() -> Self { Self::NIL } }
impl TMSig {
    pub const NIL: Self = Self([0; 64]);
    pub fn verify(&self, pub_key: PubKeyID, signed_data: &[u8]) -> Result<(), (ed25519_zebra::Error, &str)> {
        let signature = Signature::from_bytes(&self.0);
        let vk = match VerificationKey::try_from(pub_key.0) { Ok(v)=>v,       Err(err)=>{ return Err((err, "invalid public key")) }};
        match vk.verify(&signature, signed_data)            { Ok(())=>Ok(()), Err(err)=>{ Err((err, "invalid signature")) }}
    }
}
impl std::fmt::Debug for TMSig { fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { fmt_prefixed_byte_str(f, "Sig{", &self.0[..2])?; write!(f, "}}") } }

#[derive(PartialEq, Debug, Clone, Copy)]
pub struct HashKey(pub [u8; 32]);
impl HashKey { const NIL: Self = Self([0;32]); }
impl HashKey {
    pub fn hasher(&self)            -> blake3::Hasher { blake3::Hasher::new_keyed(&self.0) }
    pub fn hash(&self, data: &[u8]) -> [u8; 32]       { *blake3::keyed_hash(&self.0, data).as_bytes() }
}

#[derive(Debug)]
pub struct HashKeys {
    pub proposer: HashKey,
    pub value_id: HashKey,
    pub connect_contention: HashKey,
    pub proposal_sig: HashKey,
}
impl Default for HashKeys {
    fn default() -> Self {
        Self {
            proposer:           HashKey(blake3::Hasher::new_derive_key("BFT Proposer")          .finalize().into()),
            value_id:           HashKey(blake3::Hasher::new_derive_key("BFT Value ID")          .finalize().into()),
            connect_contention: HashKey(blake3::Hasher::new_derive_key("BFT Connect Contention").finalize().into()), // NOTE(azmr): skipping update
            proposal_sig:       HashKey(blake3::Hasher::new_derive_key("BFT Proposal Signature").finalize().into()),
        }
    }
}

/// A vote signature for a block
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct FatPointerSignature {
    pub pub_key: PubKeyID,
    #[serde(with = "serde_big_array::BigArray")]
    pub vote_signature: [u8; 64], // TMSig
}

impl FatPointerSignature {
    pub fn to_bytes(&self) -> [u8; 32 + 64] {
        let mut buf = [0_u8; 32 + 64];
        buf[0..32].copy_from_slice(&self.pub_key.0);
        buf[32..32 + 64].copy_from_slice(&self.vote_signature);
        buf
    }
    pub fn from_bytes(bytes: &[u8; 32 + 64]) -> FatPointerSignature {
        Self {
            pub_key: PubKeyID(bytes[0..32].try_into().unwrap()),
            vote_signature: bytes[32..32 + 64].try_into().unwrap(),
        }
    }
}

/// A bundle of signed votes for a block
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct FatPointerToBftBlock {
    #[serde(with = "serde_big_array::BigArray")]
    pub vote_for_block_without_finalizer_public_key: [u8; 76 - 32],
    pub signatures: Vec<FatPointerSignature>,
}

impl std::fmt::Display for FatPointerToBftBlock {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{{hash:")?;
        for b in &self.vote_for_block_without_finalizer_public_key[0..32] {
            write!(f, "{:02x}", b)?;
        }
        write!(f, " ovd:")?;
        for b in &self.vote_for_block_without_finalizer_public_key[32..] {
            write!(f, "{:02x}", b)?;
        }
        write!(f, " signatures:[")?;
        for (i, s) in self.signatures.iter().enumerate() {
            write!(f, "{{pk:")?;
            for b in s.pub_key.0 {
                write!(f, "{:02x}", b)?;
            }
            write!(f, " sig:")?;
            for b in s.vote_signature {
                write!(f, "{:02x}", b)?;
            }
            write!(f, "}}")?;
            if i + 1 < self.signatures.len() {
                write!(f, " ")?;
            }
        }
        write!(f, "]}}")?;
        Ok(())
    }
}

impl FatPointerToBftBlock {
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&self.vote_for_block_without_finalizer_public_key);
        buf.extend_from_slice(&(self.signatures.len() as u16).to_le_bytes());
        for s in &self.signatures {
            buf.extend_from_slice(&s.to_bytes());
        }
        buf
    }

    #[allow(clippy::reversed_empty_ranges)]
    pub fn try_from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 76 - 32 + 2 {
            return None;
        }
        let vote_for_block_without_finalizer_public_key = bytes[0..76 - 32].try_into().unwrap();
        let len = u16::from_le_bytes(bytes[76 - 32..2].try_into().unwrap()) as usize;

        if 76 - 32 + 2 + len * (32 + 64) > bytes.len() {
            return None;
        }
        let rem = &bytes[76 - 32 + 2..];
        let signatures = rem
            .chunks_exact(32 + 64)
            .map(|chunk| FatPointerSignature::from_bytes(chunk.try_into().unwrap()))
            .collect();

        Some(Self {
            vote_for_block_without_finalizer_public_key,
            signatures,
        })
    }

    pub fn from_parts(bft_block_hash: Blake3Hash, height: u64, round: u32, signatures: &[FatPointerSignature]) -> Self {
        let mut vote_for_block_without_finalizer_public_key = [0_u8; 76 - 32]; // 76-32 = 44
        vote_for_block_without_finalizer_public_key[..32].copy_from_slice(&bft_block_hash.0);
        vote_for_block_without_finalizer_public_key[32..40].copy_from_slice(&height.to_le_bytes());
        vote_for_block_without_finalizer_public_key[40..44].copy_from_slice(&(round | 0x80000000).to_le_bytes());
        Self {
            vote_for_block_without_finalizer_public_key,
            signatures: signatures.to_vec(),
        }
    }

    pub fn null() -> Self {
        Self {
            vote_for_block_without_finalizer_public_key: [0_u8; 76 - 32],
            signatures: Vec::new(),
        }
    }

    pub fn get_vote_template(&self) -> Vote {
        let mut vote_bytes = [0_u8; 76];
        vote_bytes[32..76].copy_from_slice(&self.vote_for_block_without_finalizer_public_key);
        Vote::from_bytes(&vote_bytes)
    }

    pub fn inflate(&self) -> Vec<(Vote, ed25519_zebra::ed25519::SignatureBytes)> {
        let vote_template = self.get_vote_template();
        self.signatures
            .iter()
            .map(|s| {
                let mut vote = vote_template.clone();
                vote.validator_address = s.pub_key;
                (vote, s.vote_signature)
            })
            .collect()
    }

    pub fn validate_signatures(&self) -> bool {
        let mut batch = ed25519_zebra::batch::Verifier::new();
        for (vote, signature) in self.inflate() {
            let vk_bytes = ed25519_zebra::VerificationKeyBytes::from(vote.validator_address.0);
            let sig = ed25519_zebra::Signature::from_bytes(&signature);
            let msg = vote.to_bytes();

            batch.queue((vk_bytes, sig, &msg));
        }
        batch.verify(rand::thread_rng()).is_ok()
    }
    pub fn points_at_block_hash(&self) -> Blake3Hash {
        Blake3Hash(
            self.vote_for_block_without_finalizer_public_key[0..32]
                .try_into()
                .unwrap(),
        )
    }

// impl ZcashSerialize for FatPointerToBftBlock {
    pub fn zcash_serialize<W: std::io::Write>(&self, mut writer: W) -> Result<(), std::io::Error> {
        writer.write_all(&self.vote_for_block_without_finalizer_public_key)?;
        writer.write_u16::<LittleEndian>(self.signatures.len() as u16)?;
        for signature in &self.signatures {
            writer.write_all(&signature.to_bytes())?;
        }
        Ok(())
    }
// }

// impl ZcashDeserialize for FatPointerToBftBlock {
    pub fn zcash_deserialize<R: std::io::Read>(mut reader: R) -> Result<Self, std::io::Error> { // SerializationError> {
        let mut vote_for_block_without_finalizer_public_key = [0u8; 76 - 32];
        reader.read_exact(&mut vote_for_block_without_finalizer_public_key)?;

        let len = reader.read_u16::<LittleEndian>()?;
        let mut signatures: Vec<FatPointerSignature> = Vec::with_capacity(len.into());
        for _ in 0..len {
            let mut signature_bytes = [0u8; 32 + 64];
            reader.read_exact(&mut signature_bytes)?;
            signatures.push(FatPointerSignature::from_bytes(&signature_bytes));
        }

        Ok(FatPointerToBftBlock {
            vote_for_block_without_finalizer_public_key,
            signatures,
        })
    }
// }
}

/// A vote for a value in a round
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Vote {
    pub validator_address: PubKeyID,
    pub value: Blake3Hash,
    pub height: u64,
    pub typ: bool, // true is commit
    pub round: i32,
}

/*
DATA LAYOUT FOR VOTE
32 byte ed25519 public key of the finalizer who's vote this is
32 byte blake3 hash of value, or all zeroes to indicate Nil vote
8 byte height
4 byte round where MSB is used to indicate is_commit for the vote type. 1 bit is_commit, 31 bits round index

TOTAL: 76 B

A signed vote will be this same layout followed by the 64 byte ed25519 signature of the previous 76 bytes.
*/

impl Vote {
    pub fn to_bytes(&self) -> [u8; 76] {
        let mut buf = [0_u8; 76];
        buf[0..32].copy_from_slice(self.validator_address.0.as_ref());
        buf[32..64].copy_from_slice(&self.value.0);
        buf[64..72].copy_from_slice(&self.height.to_le_bytes());

        let mut merged_round_val: u32 = (self.round & 0x7fff_ffff) as u32;
        if self.typ {
            merged_round_val |= 0x8000_0000;
        }
        buf[72..76].copy_from_slice(&merged_round_val.to_le_bytes());
        buf
    }

    pub fn from_bytes(bytes: &[u8; 76]) -> Vote {
        let value_hash_bytes = bytes[32..64].try_into().unwrap();
        let height = u64::from_le_bytes(bytes[64..72].try_into().unwrap());

        let merged_round_val = u32::from_le_bytes(bytes[72..76].try_into().unwrap());

        let typ = merged_round_val & 0x8000_0000 != 0;
        let round = (merged_round_val & 0x7fff_ffff) as i32;

        Vote {
            validator_address: PubKeyID(bytes[0..32].try_into().unwrap()),
            value: Blake3Hash(value_hash_bytes),
            height,
            typ,
            round,
        }
    }
}

#[derive(Default, Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FinalizerRecencyStatus {
    pub no_yes_votes_in_my_height: [[u64; 2]; 2], // prevote, precommit
    pub highest_round_vote: u32,
    pub last_seen_new_info_utc: i64,
    pub last_direct_connection_utc: Option<i64>,
    // TODO: locked, valid
}

#[derive(Default, Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TFLRecencyStatus {
    /// reference point for other absolute times
    pub now_utc: i64,
    pub my_height: u64,
    pub my_round: u32,
    pub my_step: u8, // propose, prevote, precommit
    pub my_locked_round: i64,
    pub my_valid_round: i64,

    // TODO: pub proposal_parts: Vec<bool>,
    pub finalizer_statuses: Vec<(PubKeyID, FinalizerRecencyStatus)>,
    // pub round_step_utc_rngs: Vec<[(u32, u32); 3]>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct ScanBond {
    pub pk: PubKeyID, // typed for serialization
    pub initial_val: u64,
    pub create_height: u32,
    pub create_txid: PubKeyID, // typed for serialization
}

#[derive(Clone, Default, Debug, serde::Serialize, serde::Deserialize)]
pub struct ScanInfo {
    pub ufvk: std::string::String,
    pub coinbases_c: usize,
    pub coinbases_value: u64,
    pub coinbase_max_height: u32,

    pub bonds: Vec<ScanBond>,
    pub bonds_value: u64,

    pub max_height_seen: u32,
    pub total_value: u64,
}


