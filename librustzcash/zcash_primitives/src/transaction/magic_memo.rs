//! Magic memos: public, unencrypted payloads carried in the `enc_ciphertext` of a
//! zero-value Ironwood output.
//!
//! An Orchard-family action's proof binds its `nf`, `rk`, `cmx` and `cv_net` (and the
//! bundle anchor and flags), and consensus requires its `epk` to encode a non-identity
//! Pallas point, so none of those can carry arbitrary bytes. Its `enc_ciphertext` and
//! `out_ciphertext` are not proof inputs and consensus never inspects them: they are
//! bound only by the transaction digest that the spend-authorization and binding
//! signatures sign. A magic memo therefore replaces the `enc_ciphertext` of an output
//! that the builder has already constructed and will prove, before that digest is
//! taken (see [`crate::transaction::builder::Builder::add_ironwood_magic_memo`]).
//!
//! # Encoding
//!
//! `enc_ciphertext = MAGIC_MEMO_PREFIX || payload || zero padding`. Trailing zero bytes
//! of the payload are not preserved, as with ZIP 302 text memos.
//!
//! The compact block format carries only the first [`COMPACT_NOTE_SIZE`] bytes of
//! `enc_ciphertext`, so a light client sees the first [`MAGIC_MEMO_COMPACT_LEN`] bytes
//! of the payload without fetching the full transaction.
//!
//! The replaced output's note can never be decrypted by anyone, including its
//! recipient; it is created with zero value so that nothing is lost.

use zcash_note_encryption::{COMPACT_NOTE_SIZE, ENC_CIPHERTEXT_SIZE};

/// The bytes that open the `enc_ciphertext` of an action carrying a magic memo.
pub const MAGIC_MEMO_PREFIX: [u8; 4] = *b"MEMO";

/// The largest payload, in bytes, that a magic memo can carry.
pub const MAGIC_MEMO_MAX_LEN: usize = ENC_CIPHERTEXT_SIZE - MAGIC_MEMO_PREFIX.len();

/// The number of payload bytes visible in a compact action, whose `ciphertext` field
/// holds only the first [`COMPACT_NOTE_SIZE`] bytes of `enc_ciphertext`.
pub const MAGIC_MEMO_COMPACT_LEN: usize = COMPACT_NOTE_SIZE - MAGIC_MEMO_PREFIX.len();

/// A magic memo, encoded as the `enc_ciphertext` that will carry it.
#[derive(Clone, PartialEq, Eq)]
pub struct MagicMemo([u8; ENC_CIPHERTEXT_SIZE]);

impl core::fmt::Debug for MagicMemo {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("MagicMemo").field(&payload(&self.0)).finish()
    }
}

impl MagicMemo {
    /// Encodes `payload` as a magic memo.
    ///
    /// Returns `None` if `payload` is longer than [`MAGIC_MEMO_MAX_LEN`] bytes.
    pub fn from_payload(payload: &[u8]) -> Option<Self> {
        if payload.len() > MAGIC_MEMO_MAX_LEN {
            return None;
        }
        let mut enc_ciphertext = [0u8; ENC_CIPHERTEXT_SIZE];
        let (prefix, rest) = enc_ciphertext.split_at_mut(MAGIC_MEMO_PREFIX.len());
        prefix.copy_from_slice(&MAGIC_MEMO_PREFIX);
        rest[..payload.len()].copy_from_slice(payload);
        Some(MagicMemo(enc_ciphertext))
    }

    /// Returns the `enc_ciphertext` that carries this memo.
    pub fn enc_ciphertext(&self) -> &[u8; ENC_CIPHERTEXT_SIZE] {
        &self.0
    }

    /// Returns the payload, without its zero padding.
    pub fn payload(&self) -> &[u8] {
        payload(&self.0).expect("constructed with the prefix")
    }
}

/// Returns the magic memo payload carried by `ciphertext`, without its zero padding.
///
/// `ciphertext` may be a full `enc_ciphertext` or the truncated `ciphertext` of a
/// compact action; for the latter, the payload is at most [`MAGIC_MEMO_COMPACT_LEN`]
/// bytes and may be a prefix of the full payload.
///
/// Returns `None` if `ciphertext` does not open with [`MAGIC_MEMO_PREFIX`]. An ordinary
/// note ciphertext opens with it with probability 2^-32, so callers that act on the
/// payload should also check that it has the shape they expect (for example, UTF-8).
pub fn payload(ciphertext: &[u8]) -> Option<&[u8]> {
    let rest = ciphertext.strip_prefix(&MAGIC_MEMO_PREFIX[..])?;
    let len = rest.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    Some(&rest[..len])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_full_and_compact_ciphertext() {
        let memo = MagicMemo::from_payload(b"hello, chain").unwrap();
        assert_eq!(memo.payload(), b"hello, chain");
        assert_eq!(payload(memo.enc_ciphertext()), Some(&b"hello, chain"[..]));
        assert_eq!(
            payload(&memo.enc_ciphertext()[..COMPACT_NOTE_SIZE]),
            Some(&b"hello, chain"[..])
        );
    }

    #[test]
    fn compact_ciphertext_truncates_long_payloads() {
        let long = [b'x'; MAGIC_MEMO_MAX_LEN];
        let memo = MagicMemo::from_payload(&long).unwrap();
        assert_eq!(memo.payload(), &long[..]);
        assert_eq!(
            payload(&memo.enc_ciphertext()[..COMPACT_NOTE_SIZE]),
            Some(&long[..MAGIC_MEMO_COMPACT_LEN])
        );
    }

    #[test]
    fn rejects_oversized_payloads_and_unprefixed_ciphertexts() {
        assert!(MagicMemo::from_payload(&[1; MAGIC_MEMO_MAX_LEN + 1]).is_none());
        assert_eq!(payload(&[0u8; ENC_CIPHERTEXT_SIZE]), None);
    }
}
