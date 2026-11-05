//! Experimental: magic memos, public unencrypted payloads carried entirely in the
//! compact-block view of zero-value Ironwood outputs.
//!
//! An Orchard-family action's proof binds its `nf`, `rk`, `cmx` and `cv_net` (and the
//! bundle anchor and flags), so none of those can carry chosen bytes. Its `epk` is not a
//! proof input; consensus only requires it to encode a non-identity Pallas point. Its
//! `enc_ciphertext` and `out_ciphertext` are not proof inputs and consensus never
//! inspects them. All of these are bound by the transaction digest that the
//! spend-authorization and binding signatures sign, so a magic memo is written into
//! outputs that the builder has already constructed and will prove, before that digest
//! is taken (see [`crate::transaction::builder::Builder::add_ironwood_magic_memo`]).
//!
//! A compact action carries `nf`, `cmx`, `epk` and the first [`COMPACT_NOTE_SIZE`] bytes
//! of `enc_ciphertext`. A magic memo is split into chunks, one per action, each carried
//! by the compact-visible bytes that can be chosen:
//!
//! - `enc_ciphertext[..COMPACT_NOTE_SIZE]`: [`MAGIC_MEMO_PREFIX`], the chunk index, the
//!   chunk count, the chunk's payload length, then the first payload bytes;
//! - `epk[..31]`: the remaining payload bytes; `epk[31]` is the smallest value below
//!   2^6 that makes `epk` a valid non-identity point. That keeps the encoded x-coordinate
//!   below 2^254 < p, and about half of all x-coordinates are on the curve, so a value is
//!   found within a few tries.
//!
//! The rest of each action is left as the builder made it. The outputs' notes can never
//! be decrypted by anyone, including their recipient; they have zero value so that
//! nothing is lost.

use alloc::vec::Vec;

use group::{Group, GroupEncoding};
use pasta_curves::pallas;
use zcash_note_encryption::COMPACT_NOTE_SIZE;

/// The bytes that open the `enc_ciphertext` of every action carrying a magic memo chunk.
pub const MAGIC_MEMO_PREFIX: [u8; 4] = *b"MEMO";

/// The prefix, then one byte each for the chunk index, chunk count and chunk length.
const HEADER_LEN: usize = MAGIC_MEMO_PREFIX.len() + 3;

/// Payload bytes carried in the compact ciphertext after the header.
const CIPHERTEXT_PAYLOAD_LEN: usize = COMPACT_NOTE_SIZE - HEADER_LEN;

/// The length of an `epk` encoding.
const EPK_LEN: usize = 32;

/// Payload bytes carried in `epk`; its last byte selects a valid point.
const EPK_PAYLOAD_LEN: usize = EPK_LEN - 1;

/// `epk[31]` takes values below this: they leave bits 6 (keeping x below 2^254) and 7
/// (the sign of y) clear.
const EPK_TWEAK_LIMIT: u8 = 1 << 6;

/// The number of payload bytes one action carries.
pub const MAGIC_MEMO_CHUNK_LEN: usize = CIPHERTEXT_PAYLOAD_LEN + EPK_PAYLOAD_LEN;

/// The largest number of actions, and so chunks, one magic memo occupies.
pub const MAGIC_MEMO_MAX_CHUNKS: usize = u8::MAX as usize;

/// The largest payload, in bytes, that a magic memo can carry.
pub const MAGIC_MEMO_MAX_LEN: usize = MAGIC_MEMO_CHUNK_LEN * MAGIC_MEMO_MAX_CHUNKS;

/// The compact-visible bytes of one action carrying a magic memo chunk.
#[derive(Clone, PartialEq, Eq)]
pub struct MagicMemoChunk {
    epk: [u8; EPK_LEN],
    compact_ciphertext: [u8; COMPACT_NOTE_SIZE],
}

impl MagicMemoChunk {
    /// The `epk` encoding the action carries; a valid non-identity Pallas point.
    pub fn epk_bytes(&self) -> &[u8; EPK_LEN] {
        &self.epk
    }

    /// The first [`COMPACT_NOTE_SIZE`] bytes of the action's `enc_ciphertext`.
    pub fn compact_ciphertext(&self) -> &[u8; COMPACT_NOTE_SIZE] {
        &self.compact_ciphertext
    }
}

/// A magic memo, encoded as the chunks that carry it, in order.
#[derive(Clone, PartialEq, Eq)]
pub struct MagicMemo {
    chunks: Vec<MagicMemoChunk>,
}

impl core::fmt::Debug for MagicMemo {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MagicMemo")
            .field("chunks", &self.chunks.len())
            .field("payload", &self.payload())
            .finish()
    }
}

impl MagicMemo {
    /// Encodes `payload` as a magic memo of one chunk per [`MAGIC_MEMO_CHUNK_LEN`] bytes
    /// (one chunk if `payload` is empty).
    ///
    /// Returns `None` if `payload` is longer than [`MAGIC_MEMO_MAX_LEN`] bytes, or, with
    /// negligible probability, if a chunk's `epk` bytes admit no valid point.
    pub fn from_payload(payload: &[u8]) -> Option<Self> {
        if payload.len() > MAGIC_MEMO_MAX_LEN {
            return None;
        }
        let count = payload.len().div_ceil(MAGIC_MEMO_CHUNK_LEN).max(1);
        let chunks = (0..count)
            .map(|index| {
                let start = index * MAGIC_MEMO_CHUNK_LEN;
                let bytes = &payload[start..payload.len().min(start + MAGIC_MEMO_CHUNK_LEN)];
                encode_chunk(index as u8, count as u8, bytes)
            })
            .collect::<Option<Vec<_>>>()?;
        Some(MagicMemo { chunks })
    }

    /// The chunks, in order; each is carried by one action.
    pub fn chunks(&self) -> &[MagicMemoChunk] {
        &self.chunks
    }

    /// Returns the payload.
    pub fn payload(&self) -> Vec<u8> {
        reassemble(self.chunks.iter().map(|chunk| {
            decode_chunk(&chunk.compact_ciphertext, &chunk.epk).expect("encoded above")
        }))
        .expect("encoded above")
    }
}

fn encode_chunk(index: u8, count: u8, bytes: &[u8]) -> Option<MagicMemoChunk> {
    let (in_ciphertext, in_epk) = bytes.split_at(bytes.len().min(CIPHERTEXT_PAYLOAD_LEN));

    let mut compact_ciphertext = [0u8; COMPACT_NOTE_SIZE];
    compact_ciphertext[..MAGIC_MEMO_PREFIX.len()].copy_from_slice(&MAGIC_MEMO_PREFIX);
    compact_ciphertext[MAGIC_MEMO_PREFIX.len()] = index;
    compact_ciphertext[MAGIC_MEMO_PREFIX.len() + 1] = count;
    compact_ciphertext[MAGIC_MEMO_PREFIX.len() + 2] = bytes.len() as u8;
    compact_ciphertext[HEADER_LEN..][..in_ciphertext.len()].copy_from_slice(in_ciphertext);

    let mut epk = [0u8; EPK_LEN];
    epk[..in_epk.len()].copy_from_slice(in_epk);
    (0..EPK_TWEAK_LIMIT).find_map(|tweak| {
        epk[EPK_PAYLOAD_LEN] = tweak;
        is_valid_epk(&epk).then_some(MagicMemoChunk {
            epk,
            compact_ciphertext,
        })
    })
}

/// Whether `epk` encodes a non-identity Pallas point, as consensus requires.
fn is_valid_epk(epk: &[u8; EPK_LEN]) -> bool {
    Option::<pallas::Point>::from(pallas::Point::from_bytes(epk))
        .is_some_and(|point| !bool::from(point.is_identity()))
}

/// One magic memo chunk, as read back from an action.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedChunk {
    index: u8,
    count: u8,
    len: u8,
    bytes: [u8; MAGIC_MEMO_CHUNK_LEN],
}

impl DecodedChunk {
    /// The chunk's position in its memo, from 0.
    pub fn index(&self) -> u8 {
        self.index
    }

    /// The number of chunks in the memo this chunk belongs to.
    pub fn count(&self) -> u8 {
        self.count
    }

    /// The payload bytes this chunk carries.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len)]
    }
}

/// Reads the magic memo chunk an action carries, from the first [`COMPACT_NOTE_SIZE`]
/// bytes of its `enc_ciphertext` (a compact action's `ciphertext`) and its `epk`.
///
/// Returns `None` if the action carries no well-formed chunk. An ordinary note
/// ciphertext opens with [`MAGIC_MEMO_PREFIX`] with probability 2^-32, so callers that
/// act on a reassembled payload should also check that it has the shape they expect
/// (for example, UTF-8).
pub fn decode_chunk(compact_ciphertext: &[u8], epk: &[u8]) -> Option<DecodedChunk> {
    let compact_ciphertext: &[u8; COMPACT_NOTE_SIZE] = compact_ciphertext.get(..COMPACT_NOTE_SIZE)?.try_into().ok()?;
    let epk: &[u8; EPK_LEN] = epk.try_into().ok()?;
    let header = compact_ciphertext.strip_prefix(&MAGIC_MEMO_PREFIX[..])?;
    let (index, count, len) = (header[0], header[1], header[2]);
    if index >= count || usize::from(len) > MAGIC_MEMO_CHUNK_LEN {
        return None;
    }

    let mut bytes = [0u8; MAGIC_MEMO_CHUNK_LEN];
    bytes[..CIPHERTEXT_PAYLOAD_LEN].copy_from_slice(&compact_ciphertext[HEADER_LEN..]);
    bytes[CIPHERTEXT_PAYLOAD_LEN..].copy_from_slice(&epk[..EPK_PAYLOAD_LEN]);
    Some(DecodedChunk {
        index,
        count,
        len,
        bytes,
    })
}

/// Reassembles a payload from the chunks of one magic memo, in any order.
///
/// Returns `None` unless the chunks agree on their count and each index below it
/// appears exactly once.
pub fn reassemble(chunks: impl IntoIterator<Item = DecodedChunk>) -> Option<Vec<u8>> {
    let mut slots: Vec<Option<DecodedChunk>> = Vec::new();
    for chunk in chunks {
        if slots.is_empty() {
            slots.resize(usize::from(chunk.count), None);
        }
        if usize::from(chunk.count) != slots.len() {
            return None;
        }
        let slot = &mut slots[usize::from(chunk.index)];
        if slot.is_some() {
            return None;
        }
        *slot = Some(chunk);
    }
    if slots.is_empty() {
        return None;
    }

    let mut payload = Vec::new();
    for slot in slots {
        payload.extend_from_slice(slot?.bytes());
    }
    Some(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_all(memo: &MagicMemo) -> Vec<DecodedChunk> {
        memo.chunks()
            .iter()
            .map(|c| decode_chunk(c.compact_ciphertext(), c.epk_bytes()).unwrap())
            .collect()
    }

    #[test]
    fn round_trips_single_and_multi_chunk_payloads() {
        for len in [
            0,
            1,
            CIPHERTEXT_PAYLOAD_LEN,
            CIPHERTEXT_PAYLOAD_LEN + 1,
            MAGIC_MEMO_CHUNK_LEN,
            MAGIC_MEMO_CHUNK_LEN + 1,
            3 * MAGIC_MEMO_CHUNK_LEN + 5,
            MAGIC_MEMO_MAX_LEN,
        ] {
            let payload: Vec<u8> = (0..len).map(|i| (i * 7 + 3) as u8).collect();
            let memo = MagicMemo::from_payload(&payload).unwrap();
            assert_eq!(
                memo.chunks().len(),
                len.div_ceil(MAGIC_MEMO_CHUNK_LEN).max(1)
            );
            assert_eq!(memo.payload(), payload);

            for chunk in memo.chunks() {
                assert!(is_valid_epk(chunk.epk_bytes()));
            }
            let mut chunks = decode_all(&memo);
            chunks.reverse();
            assert_eq!(reassemble(chunks), Some(payload));
        }
    }

    #[test]
    fn zero_payload_still_has_a_valid_epk() {
        // The all-zero encoding is the identity, so the tweak must move off it.
        let memo = MagicMemo::from_payload(&[]).unwrap();
        assert_ne!(memo.chunks()[0].epk_bytes(), &[0u8; EPK_LEN]);
        assert!(is_valid_epk(memo.chunks()[0].epk_bytes()));
    }

    #[test]
    fn rejects_bad_inputs() {
        assert!(MagicMemo::from_payload(&[1; MAGIC_MEMO_MAX_LEN + 1]).is_none());
        assert_eq!(decode_chunk(&[0u8; COMPACT_NOTE_SIZE], &[0u8; EPK_LEN]), None);

        let memo = MagicMemo::from_payload(&[9; 2 * MAGIC_MEMO_CHUNK_LEN]).unwrap();
        let chunks = decode_all(&memo);
        assert_eq!(reassemble(chunks[..1].to_vec()), None, "missing chunk");
        assert_eq!(
            reassemble([chunks[0].clone(), chunks[0].clone()]),
            None,
            "repeated chunk"
        );
        assert_eq!(reassemble([]), None);
    }
}
