//! Experimental: magic memos, payloads carried entirely in the compact-block view of
//! zero-value Ironwood outputs, either public (plaintext) or encrypted to a recipient.
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
//! # Chunks
//!
//! A compact action carries `nf`, `cmx`, `epk` and the first [`COMPACT_NOTE_SIZE`] bytes
//! of `enc_ciphertext`. A memo is split into chunks, one per action. A chunk's data is
//! those ciphertext bytes followed by `epk[..31]`: [`MAGIC_MEMO_PREFIX`], the chunk
//! index, the chunk count, the chunk's payload length, then up to
//! [`MAGIC_MEMO_CHUNK_LEN`] payload bytes. `epk[31]` is the smallest value below 2^6 that
//! makes `epk` a valid non-identity point; that keeps the encoded x-coordinate below
//! 2^254 < p, and about half of all x-coordinates are on the curve.
//!
//! # Public and encrypted memos
//!
//! A public memo ([`MagicMemo::from_payload`]) stores chunk data as is, so anyone can
//! read it, which is useful for debugging and exploration.
//!
//! An encrypted memo ([`MagicMemo::encrypted`]) uses Ironwood's note encryption key
//! agreement. Chunk 0's `epk` is a genuine ephemeral key `[esk] g_d` for the recipient's
//! address, so it carries no payload; the recipient derives the shared key from it with
//! their incoming viewing key, exactly as for a note ([`decrypt_with_ivk`]). Every
//! chunk's data is XORed with a keystream under that key and the chunk index, so the
//! chunks are indistinguishable from other actions to anyone else. Optionally, chunk 0's
//! `out_ciphertext` is a standard outgoing ciphertext under the sender's outgoing viewing
//! key, from which the sender can recover the key later ([`recover_with_ovk`]); unlike
//! everything else, that needs the full transaction. There is no authentication tag
//! beyond the prefix check: the transaction's signatures already prevent changes after
//! signing, and as with note memos the recipient learns nothing about who sent it.
//!
//! The rest of each action is left as the builder made it. The outputs' notes can never
//! be decrypted by anyone, including their recipient; they have zero value so that
//! nothing is lost.

use alloc::vec::Vec;

use blake2b_simd::Params;
use chacha20poly1305::{ChaCha20Poly1305, KeyInit, aead::AeadInPlace};
use group::{Group, GroupEncoding};
use orchard::{
    keys::{OutgoingViewingKey, PreparedIncomingViewingKey},
    note::{ExtractedNoteCommitment, NoteVersion, RandomSeed, Rho},
    note_encryption::IronwoodDomain,
    value::{NoteValue, ValueCommitment},
};
use pasta_curves::pallas;
use rand_core::{CryptoRng, RngCore};
use zcash_note_encryption::{
    COMPACT_NOTE_SIZE, Domain, EphemeralKeyBytes, NoteEncryption, OUT_CIPHERTEXT_SIZE,
    OUT_PLAINTEXT_SIZE, OutPlaintextBytes,
};

/// The bytes that open every chunk's data.
pub const MAGIC_MEMO_PREFIX: [u8; 4] = *b"MEMO";

/// The prefix, then one byte each for the chunk index, chunk count and chunk length.
const HEADER_LEN: usize = MAGIC_MEMO_PREFIX.len() + 3;

/// Payload bytes carried in the compact ciphertext after the header.
const CIPHERTEXT_PAYLOAD_LEN: usize = COMPACT_NOTE_SIZE - HEADER_LEN;

/// The length of an `epk` encoding.
const EPK_LEN: usize = 32;

/// Payload bytes carried in `epk`; its last byte selects a valid point.
const EPK_PAYLOAD_LEN: usize = EPK_LEN - 1;

/// A chunk's data: the compact ciphertext, then `epk[..EPK_PAYLOAD_LEN]`.
const DATA_LEN: usize = COMPACT_NOTE_SIZE + EPK_PAYLOAD_LEN;

/// `epk[31]` takes values below this: they leave bits 6 (keeping x below 2^254) and 7
/// (the sign of y) clear.
const EPK_TWEAK_LIMIT: u8 = 1 << 6;

/// The number of payload bytes one action carries, except chunk 0 of an encrypted memo,
/// which carries [`MAGIC_MEMO_ENCRYPTED_FIRST_CHUNK_LEN`].
pub const MAGIC_MEMO_CHUNK_LEN: usize = DATA_LEN - HEADER_LEN;

/// The number of payload bytes chunk 0 of an encrypted memo carries; its `epk` is the
/// key agreement's ephemeral key.
pub const MAGIC_MEMO_ENCRYPTED_FIRST_CHUNK_LEN: usize = CIPHERTEXT_PAYLOAD_LEN;

/// The largest number of actions, and so chunks, one magic memo occupies.
pub const MAGIC_MEMO_MAX_CHUNKS: usize = u8::MAX as usize;

/// The largest payload, in bytes, that a public magic memo can carry.
pub const MAGIC_MEMO_MAX_LEN: usize = MAGIC_MEMO_CHUNK_LEN * MAGIC_MEMO_MAX_CHUNKS;

/// The largest payload, in bytes, that an encrypted magic memo can carry.
pub const MAGIC_MEMO_ENCRYPTED_MAX_LEN: usize =
    MAGIC_MEMO_ENCRYPTED_FIRST_CHUNK_LEN + MAGIC_MEMO_CHUNK_LEN * (MAGIC_MEMO_MAX_CHUNKS - 1);

/// BLAKE2b personalization deriving the memo key from the note encryption key.
const KEY_PERSONALIZATION: &[u8; 16] = b"Zcash_MagicMemoK";

/// BLAKE2b personalization of the keystream blocks.
const KEYSTREAM_PERSONALIZATION: &[u8; 16] = b"Zcash_MagicMemoS";

/// The length of a BLAKE2b keystream block.
const KEYSTREAM_BLOCK_LEN: usize = 64;

/// What the builder needs to write chunk 0's `out_ciphertext` once it knows the action's
/// `cv_net` and `cmx`.
#[derive(Clone, Debug)]
struct OutRecovery {
    ovk: OutgoingViewingKey,
    note: orchard::Note,
}

/// The bytes one action carrying a magic memo chunk is given.
#[derive(Clone, Debug)]
pub struct MagicMemoChunk {
    epk: [u8; EPK_LEN],
    compact_ciphertext: [u8; COMPACT_NOTE_SIZE],
    out: Option<OutRecovery>,
}

impl PartialEq for MagicMemoChunk {
    fn eq(&self, other: &Self) -> bool {
        self.epk == other.epk && self.compact_ciphertext == other.compact_ciphertext
    }
}
impl Eq for MagicMemoChunk {}

impl MagicMemoChunk {
    /// The `epk` encoding the action carries; a valid non-identity Pallas point.
    pub fn epk_bytes(&self) -> &[u8; EPK_LEN] {
        &self.epk
    }

    /// The first [`COMPACT_NOTE_SIZE`] bytes of the action's `enc_ciphertext`.
    pub fn compact_ciphertext(&self) -> &[u8; COMPACT_NOTE_SIZE] {
        &self.compact_ciphertext
    }

    /// The `out_ciphertext` the action must carry for the sender to recover an encrypted
    /// memo with their outgoing viewing key, given the action's `cv_net` and `cmx`, or
    /// `None` if this chunk has none to carry.
    pub fn out_ciphertext<R: RngCore>(
        &self,
        cv_net: &ValueCommitment,
        cmx: &ExtractedNoteCommitment,
        rng: &mut R,
    ) -> Option<[u8; OUT_CIPHERTEXT_SIZE]> {
        let out = self.out.as_ref()?;
        let esk = IronwoodDomain::derive_esk(&out.note).expect("Ironwood notes derive esk");
        let encryption = NoteEncryption::<IronwoodDomain>::new_with_esk(
            esk,
            Some(out.ovk.clone()),
            out.note,
            [0; 512],
        );
        Some(encryption.encrypt_outgoing_plaintext(cv_net, cmx, rng))
    }
}

/// A magic memo, encoded as the chunks that carry it, in order.
#[derive(Clone, PartialEq, Eq)]
pub struct MagicMemo {
    chunks: Vec<MagicMemoChunk>,
    encrypted: bool,
}

impl core::fmt::Debug for MagicMemo {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mut s = f.debug_struct("MagicMemo");
        s.field("chunks", &self.chunks.len())
            .field("encrypted", &self.encrypted);
        if !self.encrypted {
            s.field("payload", &self.payload());
        }
        s.finish()
    }
}

impl MagicMemo {
    /// Encodes `payload` as a public magic memo of one chunk per [`MAGIC_MEMO_CHUNK_LEN`]
    /// bytes (one chunk if `payload` is empty).
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
                seal_data(chunk_data(index as u8, count as u8, bytes), None)
            })
            .collect::<Option<Vec<_>>>()?;
        Some(MagicMemo {
            chunks,
            encrypted: false,
        })
    }

    /// Encodes `payload` as a magic memo that only the holder of `recipient`'s incoming
    /// viewing key can read ([`decrypt_with_ivk`]) and, if `ovk` is given, the holder of
    /// `ovk` too, from the full transaction ([`recover_with_ovk`]).
    ///
    /// Chunk 0 carries [`MAGIC_MEMO_ENCRYPTED_FIRST_CHUNK_LEN`] payload bytes and each
    /// further chunk [`MAGIC_MEMO_CHUNK_LEN`].
    ///
    /// Returns `None` if `payload` is longer than [`MAGIC_MEMO_ENCRYPTED_MAX_LEN`] bytes,
    /// or, with negligible probability, if a chunk's `epk` bytes admit no valid point.
    pub fn encrypted<R: RngCore + CryptoRng>(
        recipient: orchard::Address,
        ovk: Option<OutgoingViewingKey>,
        payload: &[u8],
        rng: &mut R,
    ) -> Option<Self> {
        if payload.len() > MAGIC_MEMO_ENCRYPTED_MAX_LEN {
            return None;
        }

        // The key agreement goes through a note to `recipient` only because that is how
        // the note encryption API derives `esk` and `[esk] g_d`; the note goes nowhere.
        let note = random_note(recipient, rng);
        let esk = IronwoodDomain::derive_esk(&note).expect("Ironwood notes derive esk");
        let epk = IronwoodDomain::epk_bytes(&IronwoodDomain::ka_derive_public(&note, &esk));
        let shared = IronwoodDomain::ka_agree_enc(&esk, &IronwoodDomain::get_pk_d(&note));
        let key = memo_key(IronwoodDomain::kdf(shared, &epk).as_ref());

        let first_len = payload.len().min(MAGIC_MEMO_ENCRYPTED_FIRST_CHUNK_LEN);
        let (first, rest) = payload.split_at(first_len);
        let count = 1 + rest.len().div_ceil(MAGIC_MEMO_CHUNK_LEN);

        let mut ciphertext = [0u8; COMPACT_NOTE_SIZE];
        let mut data = chunk_data(0, count as u8, first);
        apply_keystream(&key, 0, &mut data[..COMPACT_NOTE_SIZE]);
        ciphertext.copy_from_slice(&data[..COMPACT_NOTE_SIZE]);
        let mut chunks = alloc::vec![MagicMemoChunk {
            epk: epk.0,
            compact_ciphertext: ciphertext,
            out: ovk.map(|ovk| OutRecovery { ovk, note }),
        }];

        for index in 1..count {
            let start = (index - 1) * MAGIC_MEMO_CHUNK_LEN;
            let bytes = &rest[start..rest.len().min(start + MAGIC_MEMO_CHUNK_LEN)];
            let mut data = chunk_data(index as u8, count as u8, bytes);
            apply_keystream(&key, index as u8, &mut data);
            chunks.push(seal_data(data, None)?);
        }
        Some(MagicMemo {
            chunks,
            encrypted: true,
        })
    }

    /// The chunks, in order; each is carried by one action.
    pub fn chunks(&self) -> &[MagicMemoChunk] {
        &self.chunks
    }

    /// Whether this memo is encrypted.
    pub fn is_encrypted(&self) -> bool {
        self.encrypted
    }

    /// Returns the payload of a public memo, or `None` for an encrypted one.
    pub fn payload(&self) -> Option<Vec<u8>> {
        if self.encrypted {
            return None;
        }
        reassemble(self.chunks.iter().map(|chunk| {
            decode_chunk(&chunk.compact_ciphertext, &chunk.epk).expect("encoded above")
        }))
    }
}

/// A note to `recipient` with random `rho` and `rseed`, retried until both are valid.
fn random_note<R: RngCore + CryptoRng>(recipient: orchard::Address, rng: &mut R) -> orchard::Note {
    loop {
        let mut bytes = [0u8; 32];
        rng.fill_bytes(&mut bytes);
        let Some(rho) = Rho::from_bytes(&bytes).into_option() else {
            continue;
        };
        rng.fill_bytes(&mut bytes);
        let Some(rseed) = RandomSeed::from_bytes(bytes, &rho).into_option() else {
            continue;
        };
        if let Some(note) = orchard::Note::from_parts(
            recipient,
            NoteValue::from_raw(0),
            rho,
            rseed,
            NoteVersion::V3,
        )
        .into_option()
        {
            return note;
        }
    }
}

fn chunk_data(index: u8, count: u8, bytes: &[u8]) -> [u8; DATA_LEN] {
    let mut data = [0u8; DATA_LEN];
    data[..MAGIC_MEMO_PREFIX.len()].copy_from_slice(&MAGIC_MEMO_PREFIX);
    data[MAGIC_MEMO_PREFIX.len()] = index;
    data[MAGIC_MEMO_PREFIX.len() + 1] = count;
    data[MAGIC_MEMO_PREFIX.len() + 2] = bytes.len() as u8;
    data[HEADER_LEN..][..bytes.len()].copy_from_slice(bytes);
    data
}

/// Splits chunk data into its compact ciphertext and `epk`, choosing `epk[31]` so that
/// `epk` is a valid point.
fn seal_data(data: [u8; DATA_LEN], out: Option<OutRecovery>) -> Option<MagicMemoChunk> {
    let mut compact_ciphertext = [0u8; COMPACT_NOTE_SIZE];
    compact_ciphertext.copy_from_slice(&data[..COMPACT_NOTE_SIZE]);
    let mut epk = [0u8; EPK_LEN];
    epk[..EPK_PAYLOAD_LEN].copy_from_slice(&data[COMPACT_NOTE_SIZE..]);
    (0..EPK_TWEAK_LIMIT).find_map(|tweak| {
        epk[EPK_PAYLOAD_LEN] = tweak;
        is_valid_epk(&epk).then(|| MagicMemoChunk {
            epk,
            compact_ciphertext,
            out: out.clone(),
        })
    })
}

/// Whether `epk` encodes a non-identity Pallas point, as consensus requires.
fn is_valid_epk(epk: &[u8; EPK_LEN]) -> bool {
    Option::<pallas::Point>::from(pallas::Point::from_bytes(epk))
        .is_some_and(|point| !bool::from(point.is_identity()))
}

/// Derives the memo key from a note encryption symmetric key.
fn memo_key(symmetric_key: &[u8]) -> [u8; 32] {
    let hash = Params::new()
        .hash_length(32)
        .personal(KEY_PERSONALIZATION)
        .hash(symmetric_key);
    let mut key = [0u8; 32];
    key.copy_from_slice(hash.as_bytes());
    key
}

/// XORs `data` with the keystream for chunk `index` under `key`: keyed BLAKE2b over
/// `(index, block number)`.
fn apply_keystream(key: &[u8; 32], index: u8, data: &mut [u8]) {
    for (block_i, block) in data.chunks_mut(KEYSTREAM_BLOCK_LEN).enumerate() {
        let stream = Params::new()
            .hash_length(KEYSTREAM_BLOCK_LEN)
            .key(key)
            .personal(KEYSTREAM_PERSONALIZATION)
            .hash(&[index, block_i as u8]);
        for (byte, k) in block.iter_mut().zip(stream.as_bytes()) {
            *byte ^= k;
        }
    }
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

fn decode_data(data: &[u8; DATA_LEN], max_len: usize) -> Option<DecodedChunk> {
    let header = data.strip_prefix(&MAGIC_MEMO_PREFIX[..])?;
    let (index, count, len) = (header[0], header[1], header[2]);
    if index >= count || usize::from(len) > max_len {
        return None;
    }
    let mut bytes = [0u8; MAGIC_MEMO_CHUNK_LEN];
    bytes.copy_from_slice(&data[HEADER_LEN..]);
    Some(DecodedChunk {
        index,
        count,
        len,
        bytes,
    })
}

/// Gathers an action's chunk data from its compact ciphertext and `epk`.
fn action_data(compact_ciphertext: &[u8], epk: &[u8]) -> Option<[u8; DATA_LEN]> {
    let compact_ciphertext = compact_ciphertext.get(..COMPACT_NOTE_SIZE)?;
    let epk = epk.get(..EPK_PAYLOAD_LEN)?;
    let mut data = [0u8; DATA_LEN];
    data[..COMPACT_NOTE_SIZE].copy_from_slice(compact_ciphertext);
    data[COMPACT_NOTE_SIZE..].copy_from_slice(epk);
    Some(data)
}

/// Reads the public magic memo chunk an action carries, from the first
/// [`COMPACT_NOTE_SIZE`] bytes of its `enc_ciphertext` (a compact action's `ciphertext`)
/// and its `epk`.
///
/// Returns `None` if the action carries no well-formed public chunk. An ordinary note
/// ciphertext opens with [`MAGIC_MEMO_PREFIX`] with probability 2^-32, so callers that
/// act on a reassembled payload should also check that it has the shape they expect
/// (for example, UTF-8).
pub fn decode_chunk(compact_ciphertext: &[u8], epk: &[u8]) -> Option<DecodedChunk> {
    decode_data(&action_data(compact_ciphertext, epk)?, MAGIC_MEMO_CHUNK_LEN)
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

/// One action, as seen in a compact block: its compact `ciphertext` and its `epk`.
pub type CompactView<'a> = (&'a [u8], &'a [u8]);

/// Decrypts the encrypted magic memo whose chunk 0 is `actions[first_i]`, under `key`.
fn decrypt_with_key(key: &[u8; 32], first_i: usize, actions: &[CompactView<'_>]) -> Option<Vec<u8>> {
    let (ciphertext, _) = actions[first_i];
    let mut data = [0u8; DATA_LEN];
    data[..COMPACT_NOTE_SIZE].copy_from_slice(ciphertext.get(..COMPACT_NOTE_SIZE)?);
    apply_keystream(key, 0, &mut data[..COMPACT_NOTE_SIZE]);
    let first = decode_data(&data, MAGIC_MEMO_ENCRYPTED_FIRST_CHUNK_LEN)?;
    if first.index != 0 {
        return None;
    }

    // The builder shuffles actions, so each further chunk is looked for among them all.
    let mut chunks = alloc::vec![first.clone()];
    for index in 1..first.count {
        let chunk = actions.iter().enumerate().find_map(|(i, (ciphertext, epk))| {
            if i == first_i {
                return None;
            }
            let mut data = action_data(ciphertext, epk)?;
            apply_keystream(key, index, &mut data);
            decode_data(&data, MAGIC_MEMO_CHUNK_LEN).filter(|chunk| chunk.index == index && chunk.count == first.count)
        })?;
        chunks.push(chunk);
    }
    reassemble(chunks)
}

/// Decrypts the encrypted magic memo carried by a transaction's Ironwood actions that is
/// addressed to `ivk`, if there is one, using only what compact actions show.
///
/// This costs one key agreement per action, as trial decryption of the actions' notes
/// does.
pub fn decrypt_with_ivk(ivk: &PreparedIncomingViewingKey, actions: &[CompactView<'_>]) -> Option<Vec<u8>> {
    actions.iter().enumerate().find_map(|(i, (_, epk))| {
        let epk = EphemeralKeyBytes(<[u8; EPK_LEN]>::try_from(*epk).ok()?);
        let point = IronwoodDomain::epk(&epk)?;
        let shared = IronwoodDomain::ka_agree_dec(ivk, &IronwoodDomain::prepare_epk(point));
        let key = memo_key(IronwoodDomain::kdf(shared, &epk).as_ref());
        decrypt_with_key(&key, i, actions)
    })
}

/// Recovers the encrypted magic memo carried by `bundle` that was sent with `ovk`, if
/// there is one. Unlike [`decrypt_with_ivk`], this needs the full transaction, for
/// chunk 0's `out_ciphertext`.
pub fn recover_with_ovk<A: orchard::bundle::Authorization, V>(
    ovk: &OutgoingViewingKey,
    bundle: &orchard::Bundle<A, V>,
) -> Option<Vec<u8>> {
    let views: Vec<([u8; COMPACT_NOTE_SIZE], [u8; EPK_LEN])> = bundle
        .actions()
        .iter()
        .map(|action| {
            let note = action.encrypted_note();
            let mut ciphertext = [0u8; COMPACT_NOTE_SIZE];
            ciphertext.copy_from_slice(&note.enc_ciphertext[..COMPACT_NOTE_SIZE]);
            (ciphertext, note.epk_bytes)
        })
        .collect();
    let views: Vec<CompactView<'_>> = views.iter().map(|(c, e)| (&c[..], &e[..])).collect();

    bundle.actions().iter().enumerate().find_map(|(i, action)| {
        let epk = EphemeralKeyBytes(action.encrypted_note().epk_bytes);
        let ock = IronwoodDomain::derive_ock(ovk, action.cv_net(), &action.cmx().to_bytes(), &epk);
        let out_ciphertext = &action.encrypted_note().out_ciphertext;
        let mut plaintext = OutPlaintextBytes([0; OUT_PLAINTEXT_SIZE]);
        plaintext.0.copy_from_slice(&out_ciphertext[..OUT_PLAINTEXT_SIZE]);
        ChaCha20Poly1305::new(ock.as_ref().into())
            .decrypt_in_place_detached(
                [0u8; 12][..].into(),
                &[],
                &mut plaintext.0,
                out_ciphertext[OUT_PLAINTEXT_SIZE..].into(),
            )
            .ok()?;
        let pk_d = IronwoodDomain::extract_pk_d(&plaintext)?;
        let esk = IronwoodDomain::extract_esk(&plaintext)?;
        let key = memo_key(IronwoodDomain::kdf(IronwoodDomain::ka_agree_enc(&esk, &pk_d), &epk).as_ref());
        decrypt_with_key(&key, i, &views)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand_core::OsRng;

    fn decode_all(memo: &MagicMemo) -> Vec<DecodedChunk> {
        memo.chunks()
            .iter()
            .map(|c| decode_chunk(c.compact_ciphertext(), c.epk_bytes()).unwrap())
            .collect()
    }

    fn views(memo: &MagicMemo) -> Vec<CompactView<'_>> {
        memo.chunks()
            .iter()
            .map(|c| (&c.compact_ciphertext()[..], &c.epk_bytes()[..]))
            .collect()
    }

    fn keys(seed: u8) -> (orchard::Address, PreparedIncomingViewingKey) {
        let sk = orchard::keys::SpendingKey::from_bytes([seed; 32]).unwrap();
        let fvk = orchard::keys::FullViewingKey::from(&sk);
        let ivk = fvk.to_ivk(orchard::keys::Scope::External);
        (ivk.address_at(0u32), PreparedIncomingViewingKey::new(&ivk))
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
            assert_eq!(memo.payload(), Some(payload.clone()));

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

    #[test]
    fn encrypted_memo_opens_only_for_its_recipient() {
        let (recipient, ivk) = keys(1);
        let (_, other_ivk) = keys(2);
        for len in [
            0,
            MAGIC_MEMO_ENCRYPTED_FIRST_CHUNK_LEN,
            MAGIC_MEMO_ENCRYPTED_FIRST_CHUNK_LEN + 1,
            MAGIC_MEMO_ENCRYPTED_FIRST_CHUNK_LEN + 2 * MAGIC_MEMO_CHUNK_LEN + 3,
        ] {
            let payload: Vec<u8> = (0..len).map(|i| (i * 5 + 1) as u8).collect();
            let memo = MagicMemo::encrypted(recipient, None, &payload, &mut OsRng).unwrap();
            assert!(memo.is_encrypted());
            assert_eq!(memo.payload(), None);
            for chunk in memo.chunks() {
                assert!(is_valid_epk(chunk.epk_bytes()));
                assert_eq!(decode_chunk(chunk.compact_ciphertext(), chunk.epk_bytes()), None, "not public");
            }

            // Shuffled among unrelated actions, as the builder would.
            let decoy = MagicMemo::from_payload(b"decoy").unwrap();
            let mut actions = views(&memo);
            actions.reverse();
            actions.insert(1, views(&decoy)[0]);
            assert_eq!(decrypt_with_ivk(&ivk, &actions), Some(payload));
            assert_eq!(decrypt_with_ivk(&other_ivk, &actions), None);
        }
        assert!(MagicMemo::encrypted(recipient, None, &[0; MAGIC_MEMO_ENCRYPTED_MAX_LEN + 1], &mut OsRng).is_none());
    }
}
