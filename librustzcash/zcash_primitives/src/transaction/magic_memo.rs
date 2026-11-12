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
//! # Content
//!
//! A memo carries a [`MagicMemoContent`], typed as [ZIP 302] types memos but without
//! padding. Its bytes are stored Brotli-compressed when that takes fewer actions, as the
//! header records.
//!
//! # Layout
//!
//! A compact action carries `nf`, `cmx`, `epk` and the first [`COMPACT_NOTE_SIZE`] bytes
//! of `enc_ciphertext`. A memo occupies a run of consecutive actions, in action order;
//! the builder retries its shuffle until the memo's outputs land consecutively. An
//! action's data is its compact ciphertext followed by `epk[..31]`. `epk[31]` keeps bit 6
//! clear (so the encoded x-coordinate is below 2^254 < p); its sign bit and its low six
//! bits come from a seed, and the low bits then advance until `epk` is a valid point
//! (about half of all x-coordinates are on the curve). In an encrypted memo the seed is
//! keystream, so `epk[31]` is distributed as in any other `epk`.
//!
//! Only the first action has a header: a little-endian `u16` whose top bit marks a
//! compressed payload and whose other bits give the stored payload's length, and so the
//! number of actions. Every later action's data is all payload.
//!
//! - Public: the first action's data is the header, [`MAGIC_MEMO_PLAINTEXT_PREFIX`] (the
//!   start of its content, and what identifies it), then payload.
//! - Encrypted: the first action's compact ciphertext is the header, a 16-byte random
//!   salt, then payload. Its note is rebuilt with `rho` = the action's `nf` and `rseed` =
//!   BLAKE2b(salt, `nf`, `cmx`, header, stored payload), and the action's `epk` is that
//!   note's, `[esk]·g_d` for the recipient's diversified base `g_d`; its `out_ciphertext`
//!   is rewritten to match. The recipient derives the note encryption key from `epk`
//!   with their incoming viewing key as usual, and from it the memo key, under which
//!   every action's data is XORed with a keystream per position. After decrypting, they
//!   recompute `esk` from the salt and payload and accept only if `[esk⁻¹]·epk` is the
//!   `g_d` of one of their addresses: a 255-bit check covering the whole memo, with no
//!   magic bytes. Without the keys, or an exact guess of the payload and the 2^128
//!   salts, nothing distinguishes the actions from others. Wallets that do not know magic
//!   memos see nothing. The sender recovers the memo with their outgoing viewing key
//!   from the first action's `out_ciphertext` (full transaction only), accepting only if
//!   the decrypted salt and payload derive the `esk` it holds.
//!
//! The rest of each action is left as the builder made it. The outputs' notes cannot be
//! decrypted as notes by anyone; they have zero value so that nothing is lost.
//!
//! [ZIP 302]: https://zips.z.cash/zip-0302

use alloc::{collections::BTreeMap, string::String, vec::Vec};
use std::io::{Read, Write};

use blake2b_simd::Params;
use chacha20poly1305::{ChaCha20Poly1305, KeyInit, aead::AeadInPlace};
use ff::{Field, FromUniformBytes};
use group::{Group, GroupEncoding};
use orchard::{
    keys::{OutgoingViewingKey, PreparedIncomingViewingKey},
    note::{NoteVersion, RandomSeed, Rho},
    note_encryption::{IronwoodDomain, IronwoodNoteEncryption},
    value::NoteValue,
};
use pasta_curves::pallas;
use rand_core::RngCore;
use subtle::ConstantTimeEq;
use zcash_note_encryption::{COMPACT_NOTE_SIZE, Domain, EphemeralKeyBytes, OUT_CIPHERTEXT_SIZE, OUT_PLAINTEXT_SIZE, OutPlaintextBytes};
use zcash_spec::PrfExpand;

/// The bytes that open a public memo's content in its first action, after the header.
pub const MAGIC_MEMO_PLAINTEXT_PREFIX: [u8; 4] = *b"MEMO";

/// The first action's header: a flag and the stored payload's length.
const HEADER_LEN: usize = 2;

/// The header bit marking a Brotli-compressed payload; the other bits are its length.
const COMPRESSED_FLAG: u16 = 1 << 15;

/// The random salt in an encrypted memo's first action, which keeps an exact guess of
/// the payload from being checkable against `epk` (128 bits, the curve's security level).
const SALT_LEN: usize = 16;

/// The length of an `epk` encoding.
const EPK_LEN: usize = 32;

/// Payload bytes carried in `epk`; its last byte selects a valid point.
const EPK_PAYLOAD_LEN: usize = EPK_LEN - 1;

/// An action's data: the compact ciphertext, then `epk[..EPK_PAYLOAD_LEN]`.
const DATA_LEN: usize = COMPACT_NOTE_SIZE + EPK_PAYLOAD_LEN;

/// The low bits of `epk[31]` that are searched for a valid point.
const EPK_TWEAK_MASK: u8 = (1 << 6) - 1;

/// The bit of `epk[31]` giving the sign of y; either is valid for a valid x-coordinate.
const EPK_SIGN_BIT: u8 = 1 << 7;

/// Payload bytes in a public memo's first action.
pub const MAGIC_MEMO_PUBLIC_FIRST_LEN: usize = DATA_LEN - HEADER_LEN - MAGIC_MEMO_PLAINTEXT_PREFIX.len();

/// Payload bytes in an encrypted memo's first action, whose `epk` is the key agreement's.
pub const MAGIC_MEMO_ENCRYPTED_FIRST_LEN: usize = COMPACT_NOTE_SIZE - HEADER_LEN - SALT_LEN;

/// Payload bytes in each action after the first.
pub const MAGIC_MEMO_NEXT_LEN: usize = DATA_LEN;

/// The largest number of actions one magic memo occupies.
pub const MAGIC_MEMO_MAX_ACTIONS: usize = u8::MAX as usize;

/// The largest stored payload, in bytes, that a public magic memo can carry.
pub const MAGIC_MEMO_PUBLIC_MAX_LEN: usize = MAGIC_MEMO_PUBLIC_FIRST_LEN + MAGIC_MEMO_NEXT_LEN * (MAGIC_MEMO_MAX_ACTIONS - 1);

/// The largest stored payload, in bytes, that an encrypted magic memo can carry.
pub const MAGIC_MEMO_ENCRYPTED_MAX_LEN: usize = MAGIC_MEMO_ENCRYPTED_FIRST_LEN + MAGIC_MEMO_NEXT_LEN * (MAGIC_MEMO_MAX_ACTIONS - 1);

/// The largest content, in ZIP 302 bytes, that a magic memo can carry once decompressed.
pub const MAGIC_MEMO_MAX_CONTENT_LEN: usize = 1 << 16;

const _: () = assert!(MAGIC_MEMO_PUBLIC_MAX_LEN < COMPRESSED_FLAG as usize);
const _: () = assert!(MAGIC_MEMO_ENCRYPTED_MAX_LEN < COMPRESSED_FLAG as usize);

/// Brotli's densest setting; memos are small, so its cost does not matter.
const BROTLI_QUALITY: u32 = 11;

/// Brotli's window, in bits: enough to cover the largest content.
const BROTLI_WINDOW_BITS: u32 = 16;

/// The buffer size Brotli's reader and writer work through.
const BROTLI_BUFFER_LEN: usize = 4096;

/// The largest ZIP 302 lead byte of text, which is part of the text.
const ZIP302_TEXT_MAX_LEAD: u8 = 0xF4;

/// The ZIP 302 lead byte of arbitrary data, which is not part of the data.
const ZIP302_ARBITRARY_LEAD: u8 = 0xFF;

/// BLAKE2b personalization deriving the memo key from the note encryption key.
const KEY_PERSONALIZATION: &[u8; 16] = b"Zcash_MagicMemoK";

/// BLAKE2b personalization of the keystream blocks.
const KEYSTREAM_PERSONALIZATION: &[u8; 16] = b"Zcash_MagicMemoS";

/// BLAKE2b personalization deriving an encrypted memo's `rseed`.
const SEED_PERSONALIZATION: &[u8; 16] = b"Zcash_MagicMemoR";

/// The length of a BLAKE2b keystream block.
const KEYSTREAM_BLOCK_LEN: usize = 64;

/// Any valid `rho` serves to derive an address's `g_d`; this one is zero.
const PROBE_RHO: [u8; 32] = [0; 32];

/// Whether a magic memo is public or encrypted to a recipient.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MagicMemoKind {
    /// Readable by anyone.
    Public,
    /// Readable only by the recipient's incoming viewing key and the sender's outgoing
    /// viewing key.
    Encrypted,
}

impl MagicMemoKind {
    fn first_len(self) -> usize {
        match self {
            MagicMemoKind::Public => MAGIC_MEMO_PUBLIC_FIRST_LEN,
            MagicMemoKind::Encrypted => MAGIC_MEMO_ENCRYPTED_FIRST_LEN,
        }
    }

    /// The largest stored payload, in bytes, that a memo of this kind can carry.
    pub fn max_stored_len(self) -> usize {
        match self {
            MagicMemoKind::Public => MAGIC_MEMO_PUBLIC_MAX_LEN,
            MagicMemoKind::Encrypted => MAGIC_MEMO_ENCRYPTED_MAX_LEN,
        }
    }
}

/// What a magic memo says, typed by its first byte as [ZIP 302] types memos.
///
/// [ZIP 302]: https://zips.z.cash/zip-0302
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MagicMemoContent {
    /// Nothing: no bytes. Empty text encodes the same way and reads back as this.
    Empty,
    /// UTF-8 text, whose first byte is at most `0xF4`.
    Text(String),
    /// Arbitrary data, after a `0xFF` lead byte.
    Arbitrary(Vec<u8>),
    /// Bytes whose lead byte ZIP 302 reserves (`0xF5` to `0xFE`), lead byte included.
    Reserved(Vec<u8>),
}

impl MagicMemoContent {
    /// The content's ZIP 302 bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        match self {
            MagicMemoContent::Empty => Vec::new(),
            MagicMemoContent::Text(text) => text.as_bytes().to_vec(),
            MagicMemoContent::Arbitrary(data) => {
                let mut bytes = Vec::with_capacity(1 + data.len());
                bytes.push(ZIP302_ARBITRARY_LEAD);
                bytes.extend_from_slice(data);
                bytes
            },
            MagicMemoContent::Reserved(bytes) => bytes.clone(),
        }
    }

    /// Reads ZIP 302 bytes. Returns `None` if they are typed as text but are not UTF-8.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        match bytes.first() {
            None => Some(MagicMemoContent::Empty),
            Some(&lead) if lead <= ZIP302_TEXT_MAX_LEAD => String::from_utf8(bytes.to_vec()).ok().map(MagicMemoContent::Text),
            Some(&ZIP302_ARBITRARY_LEAD) => Some(MagicMemoContent::Arbitrary(bytes[1..].to_vec())),
            Some(_) => Some(MagicMemoContent::Reserved(bytes.to_vec())),
        }
    }
}

/// The compact-visible bytes one action of a magic memo is given, and for an encrypted
/// memo's first action its `out_ciphertext`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MagicMemoChunk {
    epk:                [u8; EPK_LEN],
    compact_ciphertext: [u8; COMPACT_NOTE_SIZE],
    out_ciphertext:     Option<[u8; OUT_CIPHERTEXT_SIZE]>,
}

impl MagicMemoChunk {
    /// The action's `epk`; a valid non-identity Pallas point.
    pub fn epk_bytes(&self) -> &[u8; EPK_LEN] { &self.epk }

    /// The first [`COMPACT_NOTE_SIZE`] bytes of the action's `enc_ciphertext`.
    pub fn compact_ciphertext(&self) -> &[u8; COMPACT_NOTE_SIZE] { &self.compact_ciphertext }

    /// The action's new `out_ciphertext`, if it is to be replaced.
    pub fn out_ciphertext(&self) -> Option<&[u8; OUT_CIPHERTEXT_SIZE]> { self.out_ciphertext.as_ref() }
}

/// A magic memo to publish: its stored payload and where its zero-value outputs go.
#[derive(Clone, Debug)]
pub struct MagicMemo {
    stored:     Vec<u8>,
    compressed: bool,
    recipient:  orchard::Address,
    /// The sender's outgoing viewing key, present exactly when the memo is encrypted.
    ovk:        Option<OutgoingViewingKey>,
}

impl MagicMemo {
    /// A public magic memo carrying `content`, whose zero-value outputs go to `outputs_to`
    /// (which gains nothing from them).
    ///
    /// Returns `None` if `content` does not fit ([`fits`]).
    pub fn public(content: &MagicMemoContent, outputs_to: orchard::Address) -> Option<Self> {
        let (stored, compressed) = store(content, MagicMemoKind::Public)?;
        Some(MagicMemo { stored, compressed, recipient: outputs_to, ovk: None })
    }

    /// A magic memo carrying `content` that only `recipient`'s incoming viewing key can
    /// read ([`decrypt_with_ivk`]), and `ovk` from the full transaction
    /// ([`recover_with_ovk`]).
    ///
    /// Returns `None` if `content` does not fit ([`fits`]).
    pub fn encrypted(content: &MagicMemoContent, recipient: orchard::Address, ovk: OutgoingViewingKey) -> Option<Self> {
        let (stored, compressed) = store(content, MagicMemoKind::Encrypted)?;
        Some(MagicMemo { stored, compressed, recipient, ovk: Some(ovk) })
    }

    /// Whether this memo is public or encrypted.
    pub fn kind(&self) -> MagicMemoKind { if self.ovk.is_some() { MagicMemoKind::Encrypted } else { MagicMemoKind::Public } }

    /// Whether the payload is stored compressed.
    pub fn is_compressed(&self) -> bool { self.compressed }

    /// The address the memo's zero-value outputs go to: its recipient, if encrypted.
    pub fn recipient(&self) -> orchard::Address { self.recipient }

    /// The outgoing viewing key the memo's outputs must be built with, if encrypted.
    pub fn ovk(&self) -> Option<&OutgoingViewingKey> { self.ovk.as_ref() }

    /// The number of consecutive actions, and so zero-value outputs, the memo occupies.
    pub fn actions_n(&self) -> usize { actions_for(self.stored.len(), self.kind().first_len()) }

    fn header(&self) -> [u8; HEADER_LEN] {
        let flag = if self.compressed { COMPRESSED_FLAG } else { 0 };
        // Every stored payload is shorter than the flag bit, as asserted above.
        (self.stored.len() as u16 | flag).to_le_bytes()
    }

    /// The bytes for the memo's [`Self::actions_n`] consecutive actions, given the first
    /// of them as the builder made it.
    ///
    /// Returns `None`, with negligible probability, if an action's `epk` bytes admit no
    /// valid point or the first action's `nf` is not a valid `rho`.
    pub fn chunks_for<A, R: RngCore>(&self, first: &orchard::Action<A>, rng: &mut R) -> Option<Vec<MagicMemoChunk>> {
        let Some(ovk) = &self.ovk else {
            return self.public_chunks();
        };
        let header = self.header();
        let nf = first.nullifier().to_bytes();
        let cmx = first.cmx().to_bytes();
        let rho = Rho::from_bytes(&nf).into_option()?;

        // A salt whose seed is a valid rseed; all but a negligible fraction are.
        let (salt, note) = loop {
            let mut salt = [0u8; SALT_LEN];
            rng.fill_bytes(&mut salt);
            let seed = memo_seed(&salt, &nf, &cmx, &header, &self.stored);
            if let Some(note) = memo_note(self.recipient, &rho, &seed) {
                break (salt, note);
            }
        };
        let esk = IronwoodDomain::derive_esk(&note)?;
        let epk = IronwoodDomain::epk_bytes(&IronwoodDomain::ka_derive_public(&note, &esk));
        let shared = IronwoodDomain::ka_agree_enc(&esk, &IronwoodDomain::get_pk_d(&note));
        let key = memo_key(IronwoodDomain::kdf(shared, &epk).as_ref());
        // Holds `esk` and the recipient's `pk_d` under the sender's ovk, as for any output.
        let out_ciphertext = IronwoodNoteEncryption::new(Some(ovk.clone()), note, [0u8; 512]).encrypt_outgoing_plaintext(first.cv_net(), first.cmx(), rng);

        let mut ciphertext = [0u8; COMPACT_NOTE_SIZE];
        ciphertext[..HEADER_LEN].copy_from_slice(&header);
        ciphertext[HEADER_LEN..][..SALT_LEN].copy_from_slice(&salt);
        let (first_bytes, rest) = split_first(&self.stored, MAGIC_MEMO_ENCRYPTED_FIRST_LEN);
        ciphertext[HEADER_LEN + SALT_LEN..][..first_bytes.len()].copy_from_slice(first_bytes);
        apply_keystream(&key, 0, &mut ciphertext);
        let mut chunks = alloc::vec![MagicMemoChunk { epk: epk.0, compact_ciphertext: ciphertext, out_ciphertext: Some(out_ciphertext) }];
        for (i, bytes) in rest.chunks(MAGIC_MEMO_NEXT_LEN).enumerate() {
            // One byte past the data takes the keystream as is, seeding `epk[31]`.
            let mut buf = [0u8; DATA_LEN + 1];
            buf[..bytes.len()].copy_from_slice(bytes);
            apply_keystream(&key, (i + 1) as u8, &mut buf);
            let (data, tweak_seed) = buf.split_at(DATA_LEN);
            chunks.push(seal(data.try_into().ok()?, tweak_seed[0])?);
        }
        Some(chunks)
    }

    fn public_chunks(&self) -> Option<Vec<MagicMemoChunk>> {
        let mut data = [0u8; DATA_LEN];
        data[..HEADER_LEN].copy_from_slice(&self.header());
        data[HEADER_LEN..][..MAGIC_MEMO_PLAINTEXT_PREFIX.len()].copy_from_slice(&MAGIC_MEMO_PLAINTEXT_PREFIX);
        let (first_bytes, rest) = split_first(&self.stored, MAGIC_MEMO_PUBLIC_FIRST_LEN);
        data[HEADER_LEN + MAGIC_MEMO_PLAINTEXT_PREFIX.len()..][..first_bytes.len()].copy_from_slice(first_bytes);
        // A public memo announces itself, so its `epk[31]` need not look random.
        let mut chunks = alloc::vec![seal(&data, 0)?];
        for bytes in rest.chunks(MAGIC_MEMO_NEXT_LEN) {
            let mut data = [0u8; DATA_LEN];
            data[..bytes.len()].copy_from_slice(bytes);
            chunks.push(seal(&data, 0)?);
        }
        Some(chunks)
    }
}

/// Whether `content` fits in a magic memo of `kind`, compressed if need be.
pub fn fits(content: &MagicMemoContent, kind: MagicMemoKind) -> bool { store(content, kind).is_some() }

/// The stored payload for `content` and whether it is compressed: compressed exactly
/// when that takes fewer actions.
fn store(content: &MagicMemoContent, kind: MagicMemoKind) -> Option<(Vec<u8>, bool)> {
    let raw = content.to_bytes();
    if raw.len() > MAGIC_MEMO_MAX_CONTENT_LEN {
        return None;
    }
    let compressed = compress(&raw);
    let (stored, is_compressed) = if actions_for(compressed.len(), kind.first_len()) < actions_for(raw.len(), kind.first_len()) { (compressed, true) } else { (raw, false) };
    (stored.len() <= kind.max_stored_len()).then_some((stored, is_compressed))
}

fn compress(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut writer = brotli::CompressorWriter::new(&mut out, BROTLI_BUFFER_LEN, BROTLI_QUALITY, BROTLI_WINDOW_BITS);
        writer.write_all(raw).expect("writing to a Vec does not fail");
    }
    out
}

/// Decompresses at most [`MAGIC_MEMO_MAX_CONTENT_LEN`] bytes, refusing more.
fn decompress(stored: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    brotli::Decompressor::new(stored, BROTLI_BUFFER_LEN).take(MAGIC_MEMO_MAX_CONTENT_LEN as u64 + 1).read_to_end(&mut out).ok()?;
    (out.len() <= MAGIC_MEMO_MAX_CONTENT_LEN).then_some(out)
}

/// The content of a stored payload.
fn unstore(stored: &[u8], compressed: bool) -> Option<MagicMemoContent> { if compressed { MagicMemoContent::from_bytes(&decompress(stored)?) } else { MagicMemoContent::from_bytes(stored) } }

/// The stored length and compression flag in `header`, if the length is possible.
fn parse_header(header: [u8; HEADER_LEN], kind: MagicMemoKind) -> Option<(usize, bool)> {
    let value = u16::from_le_bytes(header);
    let len = usize::from(value & !COMPRESSED_FLAG);
    (len <= kind.max_stored_len()).then_some((len, value & COMPRESSED_FLAG != 0))
}

/// The number of actions a stored payload of `len` bytes occupies when its first action
/// holds `first_len`.
fn actions_for(len: usize, first_len: usize) -> usize { 1 + len.saturating_sub(first_len).div_ceil(MAGIC_MEMO_NEXT_LEN) }

fn split_first(payload: &[u8], first_len: usize) -> (&[u8], &[u8]) { payload.split_at(payload.len().min(first_len)) }

/// Splits action data into its compact ciphertext and `epk`, choosing `epk[31]` from
/// `tweak_seed` so that `epk` is a valid point.
fn seal(data: &[u8; DATA_LEN], tweak_seed: u8) -> Option<MagicMemoChunk> {
    let mut compact_ciphertext = [0u8; COMPACT_NOTE_SIZE];
    compact_ciphertext.copy_from_slice(&data[..COMPACT_NOTE_SIZE]);
    let mut epk = [0u8; EPK_LEN];
    epk[..EPK_PAYLOAD_LEN].copy_from_slice(&data[COMPACT_NOTE_SIZE..]);
    let sign = tweak_seed & EPK_SIGN_BIT;
    (0..=EPK_TWEAK_MASK).find_map(|step| {
        epk[EPK_PAYLOAD_LEN] = (tweak_seed.wrapping_add(step) & EPK_TWEAK_MASK) | sign;
        is_valid_epk(&epk).then_some(MagicMemoChunk { epk, compact_ciphertext, out_ciphertext: None })
    })
}

/// Whether `epk` encodes a non-identity Pallas point, as consensus requires.
fn is_valid_epk(epk: &[u8; EPK_LEN]) -> bool { Option::<pallas::Point>::from(pallas::Point::from_bytes(epk)).is_some_and(|point| !bool::from(point.is_identity())) }

/// Derives the memo key from a note encryption symmetric key.
fn memo_key(symmetric_key: &[u8]) -> [u8; 32] {
    let hash = Params::new().hash_length(32).personal(KEY_PERSONALIZATION).hash(symmetric_key);
    let mut key = [0u8; 32];
    key.copy_from_slice(hash.as_bytes());
    key
}

/// XORs `data` with the keystream for the memo's action `position` under `key`: keyed
/// BLAKE2b over `(position, block number)`.
fn apply_keystream(key: &[u8; 32], position: u8, data: &mut [u8]) {
    for (block_i, block) in data.chunks_mut(KEYSTREAM_BLOCK_LEN).enumerate() {
        let stream = Params::new().hash_length(KEYSTREAM_BLOCK_LEN).key(key).personal(KEYSTREAM_PERSONALIZATION).hash(&[position, block_i as u8]);
        for (byte, k) in block.iter_mut().zip(stream.as_bytes()) {
            *byte ^= k;
        }
    }
}

/// The `rseed` of an encrypted memo's first note, binding its salt, the action's `nf`
/// and `cmx`, and the whole stored payload.
fn memo_seed(salt: &[u8], nf: &[u8], cmx: &[u8], header: &[u8], stored: &[u8]) -> [u8; 32] {
    let mut state = Params::new().hash_length(32).personal(SEED_PERSONALIZATION).to_state();
    for part in [salt, nf, cmx, header, stored] {
        state.update(part);
    }
    let mut seed = [0u8; 32];
    seed.copy_from_slice(state.finalize().as_bytes());
    seed
}

/// The zero-value Ironwood note to `recipient` with this `rho` and `rseed`, if `rseed` is
/// valid for `rho`.
fn memo_note(recipient: orchard::Address, rho: &Rho, rseed: &[u8; 32]) -> Option<orchard::Note> {
    let rseed = RandomSeed::from_bytes(*rseed, rho).into_option()?;
    orchard::Note::from_parts(recipient, NoteValue::from_raw(0), *rho, rseed, NoteVersion::V3).into_option()
}

/// The scalar of the `esk` that `rseed` derives for `rho`, as ZIP 212 derives it, if it
/// is nonzero.
fn esk_scalar(rseed: &[u8; 32], rho: &[u8; 32]) -> Option<pallas::Scalar> {
    let esk = pallas::Scalar::from_uniform_bytes(&PrfExpand::ORCHARD_ESK.with(rseed, rho));
    (!bool::from(esk.is_zero())).then_some(esk)
}

/// The encoding of `address`'s diversified base `g_d`, as `[esk⁻¹]·epk` for a probe note
/// to it; orchard does not expose `g_d` itself.
fn g_d_bytes(address: orchard::Address) -> Option<[u8; 32]> {
    let rho = Rho::from_bytes(&PROBE_RHO).into_option()?;
    let (rseed, note) = (0..=u8::MAX).find_map(|b| memo_note(address, &rho, &[b; 32]).map(|note| ([b; 32], note)))?;
    let esk = IronwoodDomain::derive_esk(&note)?;
    let epk = IronwoodDomain::epk_bytes(&IronwoodDomain::ka_derive_public(&note, &esk));
    let point = pallas::Point::from_bytes(&epk.0).into_option()?;
    let inverse = esk_scalar(&rseed, &PROBE_RHO)?.invert().into_option()?;
    Some((point * inverse).to_bytes())
}

/// The addresses whose encrypted magic memos a wallet reads, looked up by `g_d`.
#[derive(Clone, Debug)]
pub struct MagicMemoRecipients {
    by_g_d: BTreeMap<[u8; 32], orchard::Address>,
}

impl MagicMemoRecipients {
    /// The given addresses, which should all belong to the incoming viewing key passed
    /// to [`decrypt_with_ivk`] with them.
    pub fn new(addresses: impl IntoIterator<Item = orchard::Address>) -> Self {
        let by_g_d = addresses.into_iter().filter_map(|address| Some((g_d_bytes(address)?, address))).collect();
        MagicMemoRecipients { by_g_d }
    }

    /// The number of distinct addresses.
    pub fn len(&self) -> usize { self.by_g_d.len() }

    /// Whether there are no addresses.
    pub fn is_empty(&self) -> bool { self.by_g_d.is_empty() }
}

/// One action, as a compact block shows it.
#[derive(Clone, Copy, Debug)]
pub struct ActionView<'a> {
    nf:         &'a [u8],
    cmx:        &'a [u8],
    epk:        &'a [u8],
    ciphertext: &'a [u8],
}

impl<'a> ActionView<'a> {
    /// A compact action's nullifier, `cmx`, `epk` and (at least [`COMPACT_NOTE_SIZE`]
    /// bytes of) ciphertext.
    pub fn new(nf: &'a [u8], cmx: &'a [u8], epk: &'a [u8], ciphertext: &'a [u8]) -> Self { ActionView { nf, cmx, epk, ciphertext } }

    /// The view of one of a bundle's actions.
    pub fn of<A>(action: &'a orchard::Action<A>, nf: &'a [u8; 32], cmx: &'a [u8; 32]) -> Self {
        let note = action.encrypted_note();
        ActionView::new(nf, cmx, &note.epk_bytes, &note.enc_ciphertext[..COMPACT_NOTE_SIZE])
    }

    fn epk(&self) -> Option<EphemeralKeyBytes> { Some(EphemeralKeyBytes(self.epk.try_into().ok()?)) }

    fn nf(&self) -> Option<&'a [u8; 32]> { self.nf.try_into().ok() }

    fn compact_ciphertext(&self) -> Option<[u8; COMPACT_NOTE_SIZE]> { self.ciphertext.get(..COMPACT_NOTE_SIZE)?.try_into().ok() }

    fn data(&self) -> Option<[u8; DATA_LEN]> {
        let mut data = [0u8; DATA_LEN];
        data[..COMPACT_NOTE_SIZE].copy_from_slice(&self.compact_ciphertext()?);
        data[COMPACT_NOTE_SIZE..].copy_from_slice(self.epk.get(..EPK_PAYLOAD_LEN)?);
        Some(data)
    }
}

/// Reads the stored payload of `len` bytes whose first `first.len()` bytes are `first`,
/// continuing in the actions after it, each XORed with `keystream(position)` if given.
fn read_rest(first: &[u8], len: usize, rest: &[ActionView<'_>], keystream: Option<&[u8; 32]>) -> Option<Vec<u8>> {
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
/// bytes with probability 2^-32, so callers that act on a memo should also check that it
/// has the shape they expect (for example, text).
pub fn decode_public(actions: &[ActionView<'_>]) -> Option<MagicMemoContent> {
    actions.iter().enumerate().find_map(|(j, action)| {
        let data = action.data()?;
        let (header, content) = data.split_at(HEADER_LEN);
        let first = content.strip_prefix(&MAGIC_MEMO_PLAINTEXT_PREFIX[..])?;
        let (len, compressed) = parse_header(header.try_into().ok()?, MagicMemoKind::Public)?;
        let actions_n = actions_for(len, MAGIC_MEMO_PUBLIC_FIRST_LEN);
        if j + actions_n > actions.len() {
            return None;
        }
        let stored = read_rest(first, len, &actions[j + 1..j + actions_n], None)?;
        unstore(&stored, compressed)
    })
}

/// An encrypted memo opened under some key, before the check that it is genuine.
struct Opened {
    /// The `rseed` its salt and payload derive.
    seed:       [u8; 32],
    stored:     Vec<u8>,
    compressed: bool,
}

/// Opens the encrypted memo whose first action would be `actions[j]` under `key`, if
/// its decrypted length fits the actions there are; this rejects almost every other
/// action before any curve arithmetic.
fn open_encrypted(key: &[u8; 32], j: usize, actions: &[ActionView<'_>]) -> Option<Opened> {
    let mut first = actions[j].compact_ciphertext()?;
    apply_keystream(key, 0, &mut first);
    let header: [u8; HEADER_LEN] = first[..HEADER_LEN].try_into().ok()?;
    let (len, compressed) = parse_header(header, MagicMemoKind::Encrypted)?;
    let actions_n = actions_for(len, MAGIC_MEMO_ENCRYPTED_FIRST_LEN);
    if j + actions_n > actions.len() {
        return None;
    }
    let salt = &first[HEADER_LEN..][..SALT_LEN];
    let stored = read_rest(&first[HEADER_LEN + SALT_LEN..], len, &actions[j + 1..j + actions_n], Some(key))?;
    let seed = memo_seed(salt, actions[j].nf, actions[j].cmx, &header, &stored);
    Some(Opened { seed, stored, compressed })
}

/// Decrypts the encrypted magic memo carried by a transaction's Ironwood actions that is
/// addressed to one of `recipients`, whose incoming viewing key is `ivk`, if there is
/// one, using only what compact actions show. Returns its content and the address it was
/// sent to.
///
/// An action opens only if the `esk` its decrypted salt and payload derive maps its
/// `epk` to the `g_d` of one of `recipients`. This costs one key agreement per action,
/// as trial decryption of the actions' notes does, and a scalar multiplication more when
/// the decrypted length is plausible.
pub fn decrypt_with_ivk(ivk: &PreparedIncomingViewingKey, recipients: &MagicMemoRecipients, actions: &[ActionView<'_>]) -> Option<(MagicMemoContent, orchard::Address)> {
    actions.iter().enumerate().find_map(|(j, action)| {
        let epk = action.epk()?;
        let point = IronwoodDomain::epk(&epk)?;
        let shared = IronwoodDomain::ka_agree_dec(ivk, &IronwoodDomain::prepare_epk(point));
        let key = memo_key(IronwoodDomain::kdf(shared, &epk).as_ref());
        let opened = open_encrypted(&key, j, actions)?;
        let inverse = esk_scalar(&opened.seed, action.nf()?)?.invert().into_option()?;
        let epk_point = pallas::Point::from_bytes(&epk.0).into_option()?;
        let address = recipients.by_g_d.get(&(epk_point * inverse).to_bytes())?;
        Some((unstore(&opened.stored, opened.compressed)?, *address))
    })
}

/// Recovers the encrypted magic memo carried by `bundle` that was sent with `ovk`, if
/// there is one. Unlike [`decrypt_with_ivk`], this needs the full transaction, for the
/// first action's `out_ciphertext`.
///
/// An action opens only if its `out_ciphertext` decrypts under `ovk` and the decrypted
/// salt and payload derive the `esk` that ciphertext holds.
pub fn recover_with_ovk<A: orchard::bundle::Authorization, V>(ovk: &OutgoingViewingKey, bundle: &orchard::Bundle<A, V>) -> Option<MagicMemoContent> {
    let ids: Vec<([u8; 32], [u8; 32])> = bundle.actions().iter().map(|action| (action.nullifier().to_bytes(), action.cmx().to_bytes())).collect();
    let views: Vec<ActionView<'_>> = bundle.actions().iter().zip(&ids).map(|(action, (nf, cmx))| ActionView::of(action, nf, cmx)).collect();
    // Any valid address will do: the check below compares esk, which depends only on
    // the note's rseed and rho.
    let any_address = orchard::keys::FullViewingKey::from(&orchard::keys::SpendingKey::from_bytes([0; 32]).expect("a valid spending key")).address_at(0u32, orchard::keys::Scope::External);

    bundle.actions().iter().enumerate().find_map(|(j, action)| {
        let epk = EphemeralKeyBytes(action.encrypted_note().epk_bytes);
        let ock = IronwoodDomain::derive_ock(ovk, action.cv_net(), &action.cmx().to_bytes(), &epk);
        let out_ciphertext = &action.encrypted_note().out_ciphertext;
        let mut plaintext = OutPlaintextBytes([0; OUT_PLAINTEXT_SIZE]);
        plaintext.0.copy_from_slice(&out_ciphertext[..OUT_PLAINTEXT_SIZE]);
        ChaCha20Poly1305::new(ock.as_ref().into()).decrypt_in_place_detached([0u8; 12][..].into(), &[], &mut plaintext.0, out_ciphertext[OUT_PLAINTEXT_SIZE..].into()).ok()?;
        let pk_d = IronwoodDomain::extract_pk_d(&plaintext)?;
        let esk = IronwoodDomain::extract_esk(&plaintext)?;
        let key = memo_key(IronwoodDomain::kdf(IronwoodDomain::ka_agree_enc(&esk, &pk_d), &epk).as_ref());
        let opened = open_encrypted(&key, j, &views)?;
        let rho = Rho::from_bytes(&ids[j].0).into_option()?;
        let derived = memo_note(any_address, &rho, &opened.seed).and_then(|note| IronwoodDomain::derive_esk(&note))?;
        if !bool::from(derived.ct_eq(&esk)) {
            return None;
        }
        unstore(&opened.stored, opened.compressed)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address() -> orchard::Address { orchard::keys::FullViewingKey::from(&orchard::keys::SpendingKey::from_bytes([3; 32]).unwrap()).address_at(0u32, orchard::keys::Scope::External) }

    /// Views of public chunks, which need no action to build; `nf` and `cmx` are unused.
    fn views(chunks: &[MagicMemoChunk]) -> Vec<ActionView<'_>> { chunks.iter().map(|c| ActionView::new(&[0; 32], &[0; 32], &c.epk[..], &c.compact_ciphertext[..])).collect() }

    fn public_chunks(content: &MagicMemoContent) -> Vec<MagicMemoChunk> {
        let memo = MagicMemo::public(content, address()).unwrap();
        let chunks = memo.public_chunks().unwrap();
        assert_eq!(chunks.len(), memo.actions_n());
        chunks
    }

    /// An action that is not part of any memo.
    fn junk(valid_epk: &MagicMemoChunk) -> MagicMemoChunk { MagicMemoChunk { epk: valid_epk.epk, compact_ciphertext: [0x55; COMPACT_NOTE_SIZE], out_ciphertext: None } }

    /// Arbitrary data of `len` bytes that does not compress.
    fn incompressible(len: usize) -> MagicMemoContent {
        let mut state = 0x9E37_79B9_7F4A_7C15u64;
        MagicMemoContent::Arbitrary(
            (0..len)
                .map(|_| {
                    // xorshift64: deterministic and incompressible enough for Brotli.
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    state as u8
                })
                .collect(),
        )
    }

    #[test]
    fn public_round_trips_and_needs_consecutive_actions() {
        // Arbitrary data adds a lead byte, so these stored lengths are `len + 1`.
        for len in [0, MAGIC_MEMO_PUBLIC_FIRST_LEN - 1, MAGIC_MEMO_PUBLIC_FIRST_LEN, MAGIC_MEMO_PUBLIC_FIRST_LEN + 3 * MAGIC_MEMO_NEXT_LEN + 5, MAGIC_MEMO_PUBLIC_MAX_LEN - 1] {
            let content = incompressible(len);
            let chunks = public_chunks(&content);
            assert_eq!(chunks.len(), actions_for(len + 1, MAGIC_MEMO_PUBLIC_FIRST_LEN));
            for chunk in &chunks {
                assert!(is_valid_epk(chunk.epk_bytes()));
            }
            assert_eq!(decode_public(&views(&chunks)), Some(content.clone()));

            // Unrelated actions before and after are skipped.
            let mut wrapped = chunks.clone();
            wrapped.insert(0, junk(&chunks[0]));
            wrapped.push(junk(&chunks[0]));
            assert_eq!(decode_public(&views(&wrapped)), Some(content.clone()));
        }
        assert!(!fits(&incompressible(MAGIC_MEMO_PUBLIC_MAX_LEN), MagicMemoKind::Public));
    }

    #[test]
    fn public_memo_cut_short_is_not_read() {
        let chunks = public_chunks(&incompressible(MAGIC_MEMO_PUBLIC_FIRST_LEN + MAGIC_MEMO_NEXT_LEN));
        assert_eq!(decode_public(&views(&chunks[..1])), None);
    }

    #[test]
    fn compresses_only_when_it_saves_an_action() {
        let short = MagicMemoContent::Text("Thanks for dinner!".into());
        assert!(!MagicMemo::public(&short, address()).unwrap().is_compressed());

        let long = MagicMemoContent::Text("the quick brown fox jumps over the lazy dog; ".repeat(40));
        let raw_len = long.to_bytes().len();
        assert!(raw_len > MAGIC_MEMO_PUBLIC_MAX_LEN / 20);
        let memo = MagicMemo::public(&long, address()).unwrap();
        assert!(memo.is_compressed());
        assert!(memo.actions_n() < actions_for(raw_len, MAGIC_MEMO_PUBLIC_FIRST_LEN));
        assert_eq!(decode_public(&views(&memo.public_chunks().unwrap())), Some(long));

        // Content too long to store raw still fits when it compresses well enough.
        let huge = MagicMemoContent::Text("a".repeat(MAGIC_MEMO_MAX_CONTENT_LEN));
        assert!(fits(&huge, MagicMemoKind::Encrypted));
        let too_huge = MagicMemoContent::Text("a".repeat(MAGIC_MEMO_MAX_CONTENT_LEN + 1));
        assert!(!fits(&too_huge, MagicMemoKind::Encrypted));
    }

    #[test]
    fn decompression_is_bounded() {
        let bomb = compress(&[0u8; 2 * MAGIC_MEMO_MAX_CONTENT_LEN]);
        assert_eq!(decompress(&bomb), None);
        assert_eq!(decompress(b"not brotli at all"), None);
    }

    #[test]
    fn zip_302_typing_round_trips() {
        for content in [MagicMemoContent::Empty, MagicMemoContent::Text("héllo, 世界".into()), MagicMemoContent::Arbitrary(alloc::vec![0, 1, 0xFF]), MagicMemoContent::Arbitrary(Vec::new()), MagicMemoContent::Reserved(alloc::vec![0xF6, 0, 0])] {
            assert_eq!(MagicMemoContent::from_bytes(&content.to_bytes()), Some(content));
        }
        assert_eq!(MagicMemoContent::from_bytes(&MagicMemoContent::Text(String::new()).to_bytes()), Some(MagicMemoContent::Empty));
        // Typed as text, but not UTF-8.
        assert_eq!(MagicMemoContent::from_bytes(&[b'a', 0xC0]), None);
    }

    #[test]
    fn sealed_epk_takes_its_sign_and_tweak_from_the_seed() {
        let data = [0x3Cu8; DATA_LEN];
        let mut signs = [false; 2];
        let mut tweaks = BTreeMap::new();
        for seed in 0..=u8::MAX {
            let chunk = seal(&data, seed).unwrap();
            let last = chunk.epk[EPK_PAYLOAD_LEN];
            assert!(is_valid_epk(&chunk.epk));
            assert_eq!(last & EPK_SIGN_BIT, seed & EPK_SIGN_BIT);
            assert_eq!(last & !(EPK_SIGN_BIT | EPK_TWEAK_MASK), 0, "x stays below 2^254");
            signs[usize::from(last & EPK_SIGN_BIT != 0)] = true;
            *tweaks.entry(last & EPK_TWEAK_MASK).or_insert(0) += 1;
        }
        assert_eq!(signs, [true, true]);
        // Every valid tweak for this data is reachable, not just the first one.
        assert!(tweaks.len() > 1);
    }

    #[test]
    fn esk_scalar_matches_orchard() {
        let rho = Rho::from_bytes(&PROBE_RHO).unwrap();
        let address = address();
        let (rseed, note) = (0..=u8::MAX).find_map(|b| memo_note(address, &rho, &[b; 32]).map(|note| ([b; 32], note))).unwrap();
        let esk = IronwoodDomain::derive_esk(&note).unwrap();
        let epk = IronwoodDomain::epk_bytes(&IronwoodDomain::ka_derive_public(&note, &esk));
        let g_d = pallas::Point::from_bytes(&g_d_bytes(address).unwrap()).unwrap();
        assert_eq!((g_d * esk_scalar(&rseed, &PROBE_RHO).unwrap()).to_bytes(), epk.0);

        // Distinct addresses of one key have distinct bases.
        let other = orchard::keys::FullViewingKey::from(&orchard::keys::SpendingKey::from_bytes([3; 32]).unwrap()).address_at(1u32, orchard::keys::Scope::External);
        assert_ne!(g_d_bytes(address), g_d_bytes(other));
        assert_eq!(MagicMemoRecipients::new([address, other, address]).len(), 2);
    }
}
