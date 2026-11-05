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
//! # Layout
//!
//! A compact action carries `nf`, `cmx`, `epk` and the first [`COMPACT_NOTE_SIZE`] bytes
//! of `enc_ciphertext`. A memo occupies a run of consecutive actions, in action order;
//! the builder retries its shuffle until the memo's outputs land consecutively. An
//! action's data is its compact ciphertext followed by `epk[..31]`; `epk[31]` is then the
//! smallest value below 2^6 that makes `epk` a valid non-identity point (keeping the
//! encoded x-coordinate below 2^254 < p; about half of all x-coordinates are on the
//! curve).
//!
//! Only the first action has a header, the payload's length as a little-endian `u16`,
//! which also gives the number of actions. Every later action's data is all payload.
//!
//! - Public: the first action's data is the length, [`MAGIC_MEMO_PLAINTEXT_PREFIX`] (the
//!   start of its content, and what identifies it), then payload.
//! - Encrypted: the first action keeps the `epk` the builder made for its zero-value note
//!   to the recipient, so the recipient derives the note encryption key `K` from it with
//!   their incoming viewing key as usual, and from that the memo key. Its compact
//!   ciphertext is the length, that note's `rseed`, then payload, and every action's data
//!   is XORed with a keystream under the memo key and the action's position in the memo.
//!   The recipient rebuilds the note from `rseed`, their own address and the action's
//!   `nf` (the note's `rho`), and accepts only if it reproduces the action's `epk` and
//!   `cmx`: that holds only for a memo to them, so no magic bytes are needed. Ordinary
//!   decryption with `K` yields garbage, so wallets that do not know magic memos ignore
//!   it. The sender recovers it with their outgoing viewing key from the first action's
//!   `out_ciphertext`, which the builder wrote and which is left as is (full transaction
//!   only).
//!
//! The rest of each action is left as the builder made it. The outputs' notes cannot be
//! decrypted as notes by anyone; they have zero value so that nothing is lost.

use alloc::vec::Vec;

use blake2b_simd::Params;
use chacha20poly1305::{ChaCha20Poly1305, KeyInit, aead::AeadInPlace};
use group::{Group, GroupEncoding};
use orchard::{
    keys::{OutgoingViewingKey, PreparedIncomingViewingKey},
    note::{ExtractedNoteCommitment, NoteVersion, RandomSeed, Rho},
    note_encryption::IronwoodDomain,
    value::NoteValue,
};
use pasta_curves::pallas;
use subtle::ConstantTimeEq;
use zcash_note_encryption::{
    COMPACT_NOTE_SIZE, Domain, EphemeralKeyBytes, OUT_PLAINTEXT_SIZE, OutPlaintextBytes,
    try_output_recovery_with_ovk,
};

/// The bytes that open a public memo's content in its first action, after the length.
pub const MAGIC_MEMO_PLAINTEXT_PREFIX: [u8; 4] = *b"MEMO";

/// The first action's header: the payload length.
const LEN_LEN: usize = 2;

/// The length of a note's `rseed`.
const RSEED_LEN: usize = 32;

/// The length of an `epk` encoding.
const EPK_LEN: usize = 32;

/// Payload bytes carried in `epk`; its last byte selects a valid point.
const EPK_PAYLOAD_LEN: usize = EPK_LEN - 1;

/// An action's data: the compact ciphertext, then `epk[..EPK_PAYLOAD_LEN]`.
const DATA_LEN: usize = COMPACT_NOTE_SIZE + EPK_PAYLOAD_LEN;

/// `epk[31]` takes values below this: they leave bits 6 (keeping x below 2^254) and 7
/// (the sign of y) clear.
const EPK_TWEAK_LIMIT: u8 = 1 << 6;

/// Payload bytes in a public memo's first action.
pub const MAGIC_MEMO_PUBLIC_FIRST_LEN: usize = DATA_LEN - LEN_LEN - MAGIC_MEMO_PLAINTEXT_PREFIX.len();

/// Payload bytes in an encrypted memo's first action, whose `epk` is the key agreement's.
pub const MAGIC_MEMO_ENCRYPTED_FIRST_LEN: usize = COMPACT_NOTE_SIZE - LEN_LEN - RSEED_LEN;

/// Payload bytes in each action after the first.
pub const MAGIC_MEMO_NEXT_LEN: usize = DATA_LEN;

/// The largest number of actions one magic memo occupies.
pub const MAGIC_MEMO_MAX_ACTIONS: usize = u8::MAX as usize;

/// The largest payload, in bytes, that a public magic memo can carry.
pub const MAGIC_MEMO_PUBLIC_MAX_LEN: usize =
    MAGIC_MEMO_PUBLIC_FIRST_LEN + MAGIC_MEMO_NEXT_LEN * (MAGIC_MEMO_MAX_ACTIONS - 1);

/// The largest payload, in bytes, that an encrypted magic memo can carry.
pub const MAGIC_MEMO_ENCRYPTED_MAX_LEN: usize =
    MAGIC_MEMO_ENCRYPTED_FIRST_LEN + MAGIC_MEMO_NEXT_LEN * (MAGIC_MEMO_MAX_ACTIONS - 1);

/// BLAKE2b personalization deriving the memo key from the note encryption key.
const KEY_PERSONALIZATION: &[u8; 16] = b"Zcash_MagicMemoK";

/// BLAKE2b personalization of the keystream blocks.
const KEYSTREAM_PERSONALIZATION: &[u8; 16] = b"Zcash_MagicMemoS";

/// The length of a BLAKE2b keystream block.
const KEYSTREAM_BLOCK_LEN: usize = 64;

/// The compact-visible bytes one action of a magic memo is given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MagicMemoChunk {
    epk: [u8; EPK_LEN],
    compact_ciphertext: [u8; COMPACT_NOTE_SIZE],
}

impl MagicMemoChunk {
    /// The action's `epk`; a valid non-identity Pallas point.
    pub fn epk_bytes(&self) -> &[u8; EPK_LEN] {
        &self.epk
    }

    /// The first [`COMPACT_NOTE_SIZE`] bytes of the action's `enc_ciphertext`.
    pub fn compact_ciphertext(&self) -> &[u8; COMPACT_NOTE_SIZE] {
        &self.compact_ciphertext
    }
}

/// A magic memo to publish: its payload and where its zero-value outputs go.
#[derive(Clone, Debug)]
pub struct MagicMemo {
    payload: Vec<u8>,
    recipient: orchard::Address,
    /// The sender's outgoing viewing key, present exactly when the memo is encrypted.
    ovk: Option<OutgoingViewingKey>,
}

impl MagicMemo {
    /// A public magic memo carrying `payload`, whose zero-value outputs go to `outputs_to`
    /// (which gains nothing from them).
    ///
    /// Returns `None` if `payload` is longer than [`MAGIC_MEMO_PUBLIC_MAX_LEN`] bytes.
    pub fn public(payload: &[u8], outputs_to: orchard::Address) -> Option<Self> {
        (payload.len() <= MAGIC_MEMO_PUBLIC_MAX_LEN).then(|| MagicMemo {
            payload: payload.to_vec(),
            recipient: outputs_to,
            ovk: None,
        })
    }

    /// A magic memo carrying `payload` that only `recipient`'s incoming viewing key can
    /// read ([`decrypt_with_ivk`]), and `ovk` from the full transaction
    /// ([`recover_with_ovk`]).
    ///
    /// Returns `None` if `payload` is longer than [`MAGIC_MEMO_ENCRYPTED_MAX_LEN`] bytes.
    pub fn encrypted(payload: &[u8], recipient: orchard::Address, ovk: OutgoingViewingKey) -> Option<Self> {
        (payload.len() <= MAGIC_MEMO_ENCRYPTED_MAX_LEN).then(|| MagicMemo {
            payload: payload.to_vec(),
            recipient,
            ovk: Some(ovk),
        })
    }

    /// The payload.
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// Whether this memo is encrypted.
    pub fn is_encrypted(&self) -> bool {
        self.ovk.is_some()
    }

    /// The address the memo's zero-value outputs go to: its recipient, if encrypted.
    pub fn recipient(&self) -> orchard::Address {
        self.recipient
    }

    /// The outgoing viewing key the memo's outputs must be built with, if encrypted.
    pub fn ovk(&self) -> Option<&OutgoingViewingKey> {
        self.ovk.as_ref()
    }

    /// The number of consecutive actions, and so zero-value outputs, the memo occupies.
    pub fn actions_n(&self) -> usize {
        let first_len = if self.is_encrypted() {
            MAGIC_MEMO_ENCRYPTED_FIRST_LEN
        } else {
            MAGIC_MEMO_PUBLIC_FIRST_LEN
        };
        actions_for(self.payload.len(), first_len)
    }

    /// The bytes for the memo's [`Self::actions_n`] consecutive actions, given the first
    /// of them as the builder made it (an output to [`Self::recipient`] built with
    /// [`Self::ovk`]).
    ///
    /// Returns `None` if the first action is not such an output, or, with negligible
    /// probability, if an action's `epk` bytes admit no valid point.
    pub fn chunks_for<A>(&self, first: &orchard::Action<A>) -> Option<Vec<MagicMemoChunk>> {
        let Some(ovk) = &self.ovk else {
            return self.public_chunks();
        };
        let len = (self.payload.len() as u16).to_le_bytes();

        // The note the builder made for the first output, recovered as its sender would.
        let encrypted_note = first.encrypted_note();
        let (note, to, _) = try_output_recovery_with_ovk(
            &IronwoodDomain::for_action(first),
            ovk,
            first,
            first.cv_net(),
            &encrypted_note.out_ciphertext,
        )?;
        if to != self.recipient || note.value() != NoteValue::from_raw(0) {
            return None;
        }
        let epk = EphemeralKeyBytes(encrypted_note.epk_bytes);
        let esk = IronwoodDomain::derive_esk(&note)?;
        let shared = IronwoodDomain::ka_agree_enc(&esk, &IronwoodDomain::get_pk_d(&note));
        let key = memo_key(IronwoodDomain::kdf(shared, &epk).as_ref());

        let mut ciphertext = [0u8; COMPACT_NOTE_SIZE];
        ciphertext[..LEN_LEN].copy_from_slice(&len);
        ciphertext[LEN_LEN..][..RSEED_LEN].copy_from_slice(note.rseed().as_bytes());
        let (first_bytes, rest) = split_first(&self.payload, MAGIC_MEMO_ENCRYPTED_FIRST_LEN);
        ciphertext[LEN_LEN + RSEED_LEN..][..first_bytes.len()].copy_from_slice(first_bytes);
        apply_keystream(&key, 0, &mut ciphertext);
        let mut chunks = alloc::vec![MagicMemoChunk {
            epk: epk.0,
            compact_ciphertext: ciphertext,
        }];
        for (i, bytes) in rest.chunks(MAGIC_MEMO_NEXT_LEN).enumerate() {
            let mut data = [0u8; DATA_LEN];
            data[..bytes.len()].copy_from_slice(bytes);
            apply_keystream(&key, (i + 1) as u8, &mut data);
            chunks.push(seal(data)?);
        }
        Some(chunks)
    }

    fn public_chunks(&self) -> Option<Vec<MagicMemoChunk>> {
        let mut data = [0u8; DATA_LEN];
        data[..LEN_LEN].copy_from_slice(&(self.payload.len() as u16).to_le_bytes());
        data[LEN_LEN..][..MAGIC_MEMO_PLAINTEXT_PREFIX.len()].copy_from_slice(&MAGIC_MEMO_PLAINTEXT_PREFIX);
        let (first_bytes, rest) = split_first(&self.payload, MAGIC_MEMO_PUBLIC_FIRST_LEN);
        data[LEN_LEN + MAGIC_MEMO_PLAINTEXT_PREFIX.len()..][..first_bytes.len()].copy_from_slice(first_bytes);
        let mut chunks = alloc::vec![seal(data)?];
        for bytes in rest.chunks(MAGIC_MEMO_NEXT_LEN) {
            let mut data = [0u8; DATA_LEN];
            data[..bytes.len()].copy_from_slice(bytes);
            chunks.push(seal(data)?);
        }
        Some(chunks)
    }
}

/// The number of actions a payload of `len` bytes occupies when its first action holds
/// `first_len`.
fn actions_for(len: usize, first_len: usize) -> usize {
    1 + len.saturating_sub(first_len).div_ceil(MAGIC_MEMO_NEXT_LEN)
}

fn split_first(payload: &[u8], first_len: usize) -> (&[u8], &[u8]) {
    payload.split_at(payload.len().min(first_len))
}

/// Splits action data into its compact ciphertext and `epk`, choosing `epk[31]` so that
/// `epk` is a valid point.
fn seal(data: [u8; DATA_LEN]) -> Option<MagicMemoChunk> {
    let mut compact_ciphertext = [0u8; COMPACT_NOTE_SIZE];
    compact_ciphertext.copy_from_slice(&data[..COMPACT_NOTE_SIZE]);
    let mut epk = [0u8; EPK_LEN];
    epk[..EPK_PAYLOAD_LEN].copy_from_slice(&data[COMPACT_NOTE_SIZE..]);
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

/// XORs `data` with the keystream for the memo's action `position` under `key`: keyed
/// BLAKE2b over `(position, block number)`.
fn apply_keystream(key: &[u8; 32], position: u8, data: &mut [u8]) {
    for (block_i, block) in data.chunks_mut(KEYSTREAM_BLOCK_LEN).enumerate() {
        let stream = Params::new()
            .hash_length(KEYSTREAM_BLOCK_LEN)
            .key(key)
            .personal(KEYSTREAM_PERSONALIZATION)
            .hash(&[position, block_i as u8]);
        for (byte, k) in block.iter_mut().zip(stream.as_bytes()) {
            *byte ^= k;
        }
    }
}

/// One action, as a compact block shows it.
#[derive(Clone, Copy, Debug)]
pub struct ActionView<'a> {
    nf: &'a [u8],
    cmx: &'a [u8],
    epk: &'a [u8],
    ciphertext: &'a [u8],
}

impl<'a> ActionView<'a> {
    /// A compact action's nullifier, `cmx`, `epk` and (at least [`COMPACT_NOTE_SIZE`]
    /// bytes of) ciphertext.
    pub fn new(nf: &'a [u8], cmx: &'a [u8], epk: &'a [u8], ciphertext: &'a [u8]) -> Self {
        ActionView {
            nf,
            cmx,
            epk,
            ciphertext,
        }
    }

    /// The view of one of a bundle's actions.
    pub fn of<A>(action: &'a orchard::Action<A>, nf: &'a [u8; 32], cmx: &'a [u8; 32]) -> Self {
        let note = action.encrypted_note();
        ActionView::new(nf, cmx, &note.epk_bytes, &note.enc_ciphertext[..COMPACT_NOTE_SIZE])
    }

    fn epk(&self) -> Option<EphemeralKeyBytes> {
        Some(EphemeralKeyBytes(self.epk.try_into().ok()?))
    }

    fn compact_ciphertext(&self) -> Option<[u8; COMPACT_NOTE_SIZE]> {
        self.ciphertext.get(..COMPACT_NOTE_SIZE)?.try_into().ok()
    }

    fn data(&self) -> Option<[u8; DATA_LEN]> {
        let mut data = [0u8; DATA_LEN];
        data[..COMPACT_NOTE_SIZE].copy_from_slice(&self.compact_ciphertext()?);
        data[COMPACT_NOTE_SIZE..].copy_from_slice(self.epk.get(..EPK_PAYLOAD_LEN)?);
        Some(data)
    }
}

/// Reads the payload of `len` bytes whose first `first.len()` bytes are `first`, continuing
/// in the actions after it, each XORed with `keystream(position)` if given.
fn read_rest(
    first: &[u8],
    len: usize,
    rest: &[ActionView<'_>],
    keystream: Option<&[u8; 32]>,
) -> Option<Vec<u8>> {
    let mut payload = Vec::with_capacity(len);
    payload.extend_from_slice(&first[..first.len().min(len)]);
    for (i, action) in rest.iter().enumerate() {
        if payload.len() >= len {
            break;
        }
        let mut data = action.data()?;
        if let Some(key) = keystream {
            apply_keystream(key, (i + 1) as u8, &mut data);
        }
        let take = (len - payload.len()).min(DATA_LEN);
        payload.extend_from_slice(&data[..take]);
    }
    (payload.len() == len).then_some(payload)
}

/// Reads the public magic memo carried by a transaction's Ironwood actions, in action
/// order, if there is one.
///
/// [`MAGIC_MEMO_PLAINTEXT_PREFIX`] opens an ordinary note ciphertext's third to sixth
/// bytes with probability 2^-32, so callers that act on a payload should also check that
/// it has the shape they expect (for example, UTF-8).
pub fn decode_public(actions: &[ActionView<'_>]) -> Option<Vec<u8>> {
    actions.iter().enumerate().find_map(|(j, action)| {
        let data = action.data()?;
        let (len, content) = data.split_at(LEN_LEN);
        let first = content.strip_prefix(&MAGIC_MEMO_PLAINTEXT_PREFIX[..])?;
        let len = usize::from(u16::from_le_bytes(<[u8; LEN_LEN]>::try_from(len).ok()?));
        let actions_n = actions_for(len, MAGIC_MEMO_PUBLIC_FIRST_LEN);
        if len > MAGIC_MEMO_PUBLIC_MAX_LEN || j + actions_n > actions.len() {
            return None;
        }
        read_rest(first, len, &actions[j + 1..j + actions_n], None)
    })
}

/// Opens the encrypted memo whose first action is `actions[j]`, under `key` derived from
/// that action, given a check that its note is genuine.
fn open_encrypted(
    key: &[u8; 32],
    j: usize,
    actions: &[ActionView<'_>],
    is_note: impl FnOnce(&[u8; RSEED_LEN]) -> bool,
) -> Option<Vec<u8>> {
    let mut first = actions[j].compact_ciphertext()?;
    apply_keystream(key, 0, &mut first);
    let len = usize::from(u16::from_le_bytes(<[u8; LEN_LEN]>::try_from(&first[..LEN_LEN]).ok()?));
    let actions_n = actions_for(len, MAGIC_MEMO_ENCRYPTED_FIRST_LEN);
    if len > MAGIC_MEMO_ENCRYPTED_MAX_LEN || j + actions_n > actions.len() {
        return None;
    }
    let rseed: &[u8; RSEED_LEN] = first[LEN_LEN..][..RSEED_LEN].try_into().ok()?;
    if !is_note(rseed) {
        return None;
    }
    read_rest(&first[LEN_LEN + RSEED_LEN..], len, &actions[j + 1..j + actions_n], Some(key))
}

/// The zero-value Ironwood note to `recipient` that a memo action with nullifier `nf`
/// and seed `rseed` carries.
fn memo_note(recipient: orchard::Address, nf: &[u8], rseed: &[u8; RSEED_LEN]) -> Option<orchard::Note> {
    // An action's output note takes the action's nullifier as its rho.
    let rho = Rho::from_bytes(<&[u8; 32]>::try_from(nf).ok()?).into_option()?;
    let rseed = RandomSeed::from_bytes(*rseed, &rho).into_option()?;
    orchard::Note::from_parts(recipient, NoteValue::from_raw(0), rho, rseed, NoteVersion::V3).into_option()
}

/// Decrypts the encrypted magic memo carried by a transaction's Ironwood actions that is
/// addressed to `recipient`, whose incoming viewing key is `ivk`, if there is one, using
/// only what compact actions show.
///
/// An action opens only if the note it carries, rebuilt from the decrypted `rseed`,
/// reproduces the action's `epk` and `cmx`. This costs one key agreement per action, as
/// trial decryption of the actions' notes does, and a scalar multiplication more when
/// the decrypted length is plausible.
pub fn decrypt_with_ivk(
    ivk: &PreparedIncomingViewingKey,
    recipient: orchard::Address,
    actions: &[ActionView<'_>],
) -> Option<Vec<u8>> {
    actions.iter().enumerate().find_map(|(j, action)| {
        let epk = action.epk()?;
        let point = IronwoodDomain::epk(&epk)?;
        let shared = IronwoodDomain::ka_agree_dec(ivk, &IronwoodDomain::prepare_epk(point));
        let key = memo_key(IronwoodDomain::kdf(shared, &epk).as_ref());
        open_encrypted(&key, j, actions, |rseed| {
            let Some(note) = memo_note(recipient, action.nf, rseed) else {
                return false;
            };
            let Some(esk) = IronwoodDomain::derive_esk(&note) else {
                return false;
            };
            // The cheaper check first: almost every candidate fails it.
            IronwoodDomain::epk_bytes(&IronwoodDomain::ka_derive_public(&note, &esk)).0 == epk.0
                && ExtractedNoteCommitment::from(note.commitment()).to_bytes()[..] == *action.cmx
        })
    })
}

/// Recovers the encrypted magic memo carried by `bundle` that was sent with `ovk`, if
/// there is one. Unlike [`decrypt_with_ivk`], this needs the full transaction, for the
/// first action's `out_ciphertext`.
///
/// An action opens only if its `out_ciphertext` decrypts under `ovk` and the decrypted
/// `rseed` derives the `esk` that ciphertext holds.
pub fn recover_with_ovk<A: orchard::bundle::Authorization, V>(
    ovk: &OutgoingViewingKey,
    bundle: &orchard::Bundle<A, V>,
) -> Option<Vec<u8>> {
    let ids: Vec<([u8; 32], [u8; 32])> = bundle
        .actions()
        .iter()
        .map(|action| (action.nullifier().to_bytes(), action.cmx().to_bytes()))
        .collect();
    let views: Vec<ActionView<'_>> = bundle
        .actions()
        .iter()
        .zip(&ids)
        .map(|(action, (nf, cmx))| ActionView::of(action, nf, cmx))
        .collect();
    // Any valid address will do: the check below compares esk, which depends only on
    // the note's rseed and rho.
    let any_address = orchard::keys::FullViewingKey::from(
        &orchard::keys::SpendingKey::from_bytes([0; 32]).expect("a valid spending key"),
    )
    .address_at(0u32, orchard::keys::Scope::External);

    bundle.actions().iter().enumerate().find_map(|(j, action)| {
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
        open_encrypted(&key, j, &views, |rseed| {
            memo_note(any_address, &ids[j].0, rseed)
                .and_then(|note| IronwoodDomain::derive_esk(&note))
                .is_some_and(|derived| bool::from(derived.ct_eq(&esk)))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address() -> orchard::Address {
        orchard::keys::FullViewingKey::from(&orchard::keys::SpendingKey::from_bytes([3; 32]).unwrap())
            .address_at(0u32, orchard::keys::Scope::External)
    }

    /// Views of public chunks, which need no action to build; `nf` and `cmx` are unused.
    fn views(chunks: &[MagicMemoChunk]) -> Vec<ActionView<'_>> {
        chunks
            .iter()
            .map(|c| ActionView::new(&[0; 32], &[0; 32], &c.epk[..], &c.compact_ciphertext[..]))
            .collect()
    }

    fn public_chunks(payload: &[u8]) -> Vec<MagicMemoChunk> {
        let memo = MagicMemo::public(payload, address()).unwrap();
        let chunks = memo.public_chunks().unwrap();
        assert_eq!(chunks.len(), memo.actions_n());
        chunks
    }

    /// An action that is not part of any memo.
    fn junk(valid_epk: &MagicMemoChunk) -> MagicMemoChunk {
        MagicMemoChunk {
            epk: valid_epk.epk,
            compact_ciphertext: [0x55; COMPACT_NOTE_SIZE],
        }
    }

    #[test]
    fn public_round_trips_and_needs_consecutive_actions() {
        for len in [
            0,
            1,
            MAGIC_MEMO_PUBLIC_FIRST_LEN,
            MAGIC_MEMO_PUBLIC_FIRST_LEN + 1,
            MAGIC_MEMO_PUBLIC_FIRST_LEN + 3 * MAGIC_MEMO_NEXT_LEN + 5,
            MAGIC_MEMO_PUBLIC_MAX_LEN,
        ] {
            let payload: Vec<u8> = (0..len).map(|i| (i * 7 + 3) as u8).collect();
            let chunks = public_chunks(&payload);
            assert_eq!(chunks.len(), actions_for(len, MAGIC_MEMO_PUBLIC_FIRST_LEN));
            for chunk in &chunks {
                assert!(is_valid_epk(chunk.epk_bytes()));
            }
            assert_eq!(decode_public(&views(&chunks)), Some(payload.clone()));

            // Unrelated actions before and after are skipped.
            let mut wrapped = chunks.clone();
            wrapped.insert(0, junk(&chunks[0]));
            wrapped.push(junk(&chunks[0]));
            assert_eq!(decode_public(&views(&wrapped)), Some(payload.clone()));
        }
        assert!(MagicMemo::public(&[0; MAGIC_MEMO_PUBLIC_MAX_LEN + 1], address()).is_none());
    }

    #[test]
    fn public_memo_cut_short_is_not_read() {
        let payload = [7u8; MAGIC_MEMO_PUBLIC_FIRST_LEN + MAGIC_MEMO_NEXT_LEN];
        let chunks = public_chunks(&payload);
        assert_eq!(decode_public(&views(&chunks[..1])), None);
    }
}
