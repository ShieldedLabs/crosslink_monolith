//! Bounded, transport-only Merkle payloads, independent of STP's packet MTU.
//!
//! A root commits to a nonempty byte string and its leaf geometry, not to the
//! chain's existing keyed body ID or to a signed header. Authenticate a manifest
//! in its BFT context before using it, retain that exact manifest during assembly,
//! and check the reconstructed body's existing keyed ID and canonical/host
//! validity before admitting it to consensus. Even a valid holder attestation
//! does not prove that its root and claimed chain body ID describe the same body.
//!
//! Leaves are ordered by byte offset. The final leaf has its exact remaining
//! length; it is never zero-filled. The tree pads to the next power of two with
//! domain-separated padding leaves, then hashes ordered pairs. Proofs contain
//! exactly ceil(log2(real leaves)) sibling hashes, bottom-up. All hashes bind the
//! body length, leaf size and tree position. Only the tree stores O(leaves)
//! hashes; it owns no body bytes. Verification allocates nothing.

use ed25519_zebra::{Signature, SigningKey, VerificationKey, VerificationKeyBytes};
use std::{fmt, ops::Range};

pub type Hash = [u8; 32];
pub type Result<T> = std::result::Result<T, Error>;

pub const MIN_LEAF_BYTES: usize = 32_768;
pub const MAX_LEAF_BYTES: usize = 1_048_576;
pub const DEFAULT_LEAF_BYTES: usize = 102_400;

const LEAF_DOMAIN: &[u8] = b"tenderlink/D28/merkle/leaf/v1";
const NODE_DOMAIN: &[u8] = b"tenderlink/D28/merkle/node/v1";
const PADDING_DOMAIN: &[u8] = b"tenderlink/D28/merkle/padding/v1";
const HOLDER_DOMAIN: &[u8] = b"tenderlink/D28/BFT/merkle/holder/v1";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    BodyLength,
    LeafSize,
    Bounds,
    Allocation,
    Index,
    LeafLength,
    ProofLength,
    Root,
    BitmapLength,
    BitmapPadding,
    ManifestMismatch,
    Duplicate,
    Unavailable,
    Holder,
    Signature,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid Merkle payload: {self:?}")
    }
}
impl std::error::Error for Error {}

/// Untrusted fields until checked against local bounds and an authenticated
/// signed header/holder. Neither `validate` nor leaf membership establishes the
/// claimed relationship between `body_id` and `root`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Manifest {
    pub body_id: Hash,
    /// Identity of the exact signed header, not merely its body ID.
    pub signed_header_identity: Hash,
    pub body_len: u64,
    pub leaf_bytes: u32,
    pub root: Hash,
}

impl Manifest {
    /// Checks all sizes before any tree, bitmap or body allocation. `max_body`
    /// must be a trusted local bound, never a value taken from this manifest.
    pub fn validate(&self, max_body: usize) -> Result<Layout> {
        Layout::new(self.body_len, self.leaf_bytes as usize, max_body)
    }
}

/// Checked geometry. Private fields prevent bypassing the allocation bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Layout {
    body_len: usize,
    leaf_bytes: usize,
    leaf_count: usize,
    padded_leaf_count: usize,
    tree_hash_count: usize,
    proof_depth: usize,
    bitmap_bytes: usize,
}

impl Layout {
    fn new(body_len: u64, leaf_bytes: usize, max_body: usize) -> Result<Self> {
        if !(MIN_LEAF_BYTES..=MAX_LEAF_BYTES).contains(&leaf_bytes) {
            return Err(Error::LeafSize);
        }
        if max_body == 0 || max_body > isize::MAX as usize {
            return Err(Error::Bounds);
        }
        let body_len = usize::try_from(body_len).map_err(|_| Error::BodyLength)?;
        if body_len == 0 || body_len > max_body {
            return Err(Error::BodyLength);
        }
        // Division first avoids an overflow from body_len + leaf_bytes - 1.
        let leaf_count = 1 + (body_len - 1) / leaf_bytes;
        let padded_leaf_count = leaf_count
            .checked_next_power_of_two()
            .ok_or(Error::Bounds)?;
        let tree_hash_count = padded_leaf_count
            .checked_mul(2)
            .and_then(|n| n.checked_sub(1))
            .ok_or(Error::Bounds)?;
        let tree_bytes = tree_hash_count.checked_mul(32).ok_or(Error::Bounds)?;
        if tree_bytes > isize::MAX as usize {
            return Err(Error::Bounds);
        }
        Ok(Self {
            body_len,
            leaf_bytes,
            leaf_count,
            padded_leaf_count,
            tree_hash_count,
            proof_depth: padded_leaf_count.trailing_zeros() as usize,
            bitmap_bytes: 1 + (leaf_count - 1) / 8,
        })
    }

    pub fn body_len(self) -> usize {
        self.body_len
    }
    pub fn leaf_bytes(self) -> usize {
        self.leaf_bytes
    }
    pub fn leaf_count(self) -> usize {
        self.leaf_count
    }
    pub fn padded_leaf_count(self) -> usize {
        self.padded_leaf_count
    }
    pub fn tree_hash_count(self) -> usize {
        self.tree_hash_count
    }
    /// Hash payload bytes only, excluding Vec/struct and allocator overhead.
    pub fn tree_hash_bytes(self) -> usize {
        self.tree_hash_count * 32
    }
    pub fn proof_depth(self) -> usize {
        self.proof_depth
    }
    pub fn proof_bytes(self) -> usize {
        self.proof_depth * 32
    }
    pub fn bitmap_bytes(self) -> usize {
        self.bitmap_bytes
    }
    pub fn known_node_bitmap_bytes(self) -> usize {
        1 + (self.tree_hash_count - 1) / 8
    }
    /// Hash and bitmap payloads for a partial receiver; no body bytes, Vec/struct
    /// headers, transient proof vector or allocator overhead are included.
    pub fn partial_metadata_bytes(self) -> usize {
        self.tree_hash_bytes() + self.known_node_bitmap_bytes() + self.bitmap_bytes
    }
    /// The exact body slice to send for a real leaf, including a partial last one.
    pub fn leaf_range(self, index: usize) -> Result<Range<usize>> {
        if index >= self.leaf_count {
            return Err(Error::Index);
        }
        let start = index * self.leaf_bytes;
        let len = self.leaf_bytes.min(self.body_len - start);
        Ok(start..start + len)
    }
}

fn hasher(domain: &[u8], layout: Layout, position: usize) -> blake3::Hasher {
    let mut h = blake3::Hasher::new();
    h.update(domain);
    h.update(&(layout.body_len as u64).to_le_bytes());
    h.update(&(layout.leaf_bytes as u32).to_le_bytes());
    h.update(&(position as u64).to_le_bytes());
    h
}

fn leaf_hash(layout: Layout, index: usize, data: &[u8]) -> Hash {
    let mut h = hasher(LEAF_DOMAIN, layout, index);
    h.update(&(data.len() as u64).to_le_bytes());
    h.update(data);
    *h.finalize().as_bytes()
}

fn padding_hash(layout: Layout, index: usize) -> Hash {
    *hasher(PADDING_DOMAIN, layout, index).finalize().as_bytes()
}

fn node_hash(layout: Layout, position: usize, left: &Hash, right: &Hash) -> Hash {
    let mut h = hasher(NODE_DOMAIN, layout, position);
    h.update(left);
    h.update(right);
    *h.finalize().as_bytes()
}

fn reserved<T>(count: usize) -> Result<Vec<T>> {
    let mut out = Vec::new();
    out.try_reserve_exact(count)
        .map_err(|_| Error::Allocation)?;
    Ok(out)
}

/// A flat, complete binary tree in heap order: root at 0, then children at
/// 2*i+1 and 2*i+2. There is no unused sentinel and no copy of the body.
#[derive(Debug)]
pub struct Tree {
    layout: Layout,
    hashes: Vec<Hash>,
}

impl Tree {
    pub fn new(body: &[u8], leaf_bytes: usize, max_body: usize) -> Result<Self> {
        let body_len = u64::try_from(body.len()).map_err(|_| Error::Bounds)?;
        let layout = Layout::new(body_len, leaf_bytes, max_body)?;
        let mut hashes = reserved(layout.tree_hash_count)?;
        hashes.resize(layout.tree_hash_count, [0; 32]);
        let start = layout.padded_leaf_count - 1;
        for index in 0..layout.padded_leaf_count {
            hashes[start + index] = if index < layout.leaf_count {
                leaf_hash(layout, index, &body[layout.leaf_range(index)?])
            } else {
                padding_hash(layout, index)
            };
        }
        for position in (0..start).rev() {
            hashes[position] = node_hash(
                layout,
                position,
                &hashes[2 * position + 1],
                &hashes[2 * position + 2],
            );
        }
        Ok(Self { layout, hashes })
    }

    pub fn root(&self) -> Hash {
        self.hashes[0]
    }
    pub fn layout(&self) -> Layout {
        self.layout
    }
    pub fn leaf_count(&self) -> usize {
        self.layout.leaf_count
    }
    /// Conveniently attach the caller's existing IDs, without deriving or
    /// validating them. They do not participate in this tree's root hash.
    pub fn manifest(&self, body_id: Hash, signed_header_identity: Hash) -> Manifest {
        Manifest {
            body_id,
            signed_header_identity,
            body_len: self.layout.body_len as u64,
            leaf_bytes: self.layout.leaf_bytes as u32,
            root: self.root(),
        }
    }

    pub fn proof(&self, index: usize) -> Result<Vec<Hash>> {
        self.layout.leaf_range(index)?;
        let mut proof = reserved(self.layout.proof_depth)?;
        let mut position = self.layout.padded_leaf_count - 1 + index;
        while position != 0 {
            let sibling = if position % 2 == 1 {
                position + 1
            } else {
                position - 1
            };
            proof.push(self.hashes[sibling]);
            position = (position - 1) / 2;
        }
        Ok(proof)
    }
}

/// Verify one independently relayable leaf using O(proof depth) time and O(1)
/// space. The proof/data remain borrowed and may be forwarded immediately; no
/// other leaf or full reconstructed body is required. This proves membership
/// under this root/geometry only, not the manifest's IDs or chain validity.
/// Authenticate those fields separately and retain the exact manifest.
pub fn verify_leaf(
    manifest: &Manifest,
    index: usize,
    data: &[u8],
    proof: &[Hash],
    max_body: usize,
) -> Result<()> {
    let layout = manifest.validate(max_body)?;
    if data.len() != layout.leaf_range(index)?.len() {
        return Err(Error::LeafLength);
    }
    if proof.len() != layout.proof_depth {
        return Err(Error::ProofLength);
    }
    let mut hash = leaf_hash(layout, index, data);
    let mut position = layout.padded_leaf_count - 1 + index;
    for sibling in proof {
        let parent = (position - 1) / 2;
        hash = if position % 2 == 1 {
            node_hash(layout, parent, &hash, sibling)
        } else {
            node_hash(layout, parent, sibling, &hash)
        };
        position = parent;
    }
    if hash == manifest.root {
        Ok(())
    } else {
        Err(Error::Root)
    }
}

/// A bounded, exact-manifest inventory. Bits are little-endian within each byte
/// (leaf i is bit i%8 of byte i/8); unused high bits must be zero.
///
/// For local verified inventory, start empty and use `verify_and_set` after
/// authenticating the manifest. `set` is for leaves already verified by the
/// caller. `from_bytes` is structural validation of advertised inventory, not
/// proof that a peer holds/verified those leaves. Never use remote bits as local
/// verification evidence. `complete` means all bits, NOT a valid chain body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bitmap {
    manifest: Manifest,
    layout: Layout,
    bytes: Vec<u8>,
    present: usize,
}

impl Bitmap {
    pub fn new(manifest: &Manifest, max_body: usize) -> Result<Self> {
        let layout = manifest.validate(max_body)?;
        let mut bytes = reserved(layout.bitmap_bytes)?;
        bytes.resize(layout.bitmap_bytes, 0);
        Ok(Self {
            manifest: *manifest,
            layout,
            bytes,
            present: 0,
        })
    }

    pub fn from_bytes(manifest: &Manifest, bytes: &[u8], max_body: usize) -> Result<Self> {
        let layout = manifest.validate(max_body)?;
        if bytes.len() != layout.bitmap_bytes {
            return Err(Error::BitmapLength);
        }
        let used = layout.leaf_count % 8;
        if used != 0 && bytes[bytes.len() - 1] >> used != 0 {
            return Err(Error::BitmapPadding);
        }
        let present = bytes.iter().map(|byte| byte.count_ones() as usize).sum();
        let mut owned = reserved(bytes.len())?;
        owned.extend_from_slice(bytes);
        Ok(Self {
            manifest: *manifest,
            layout,
            bytes: owned,
            present,
        })
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub fn leaf_count(&self) -> usize {
        self.layout.leaf_count
    }
    pub fn count(&self) -> usize {
        self.present
    }
    pub fn complete(&self) -> bool {
        self.present == self.layout.leaf_count
    }
    pub fn have(&self, index: usize) -> Result<bool> {
        self.layout.leaf_range(index)?;
        Ok(self.bytes[index / 8] & (1 << (index % 8)) != 0)
    }
    pub fn missing(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.layout.leaf_count).filter(|&index| self.bytes[index / 8] & (1 << (index % 8)) == 0)
    }
    /// Mark a leaf already verified by the caller. A duplicate is an error and
    /// leaves the bitmap unchanged, as does every other failed operation.
    pub fn set(&mut self, index: usize) -> Result<()> {
        if self.have(index)? {
            return Err(Error::Duplicate);
        }
        self.bytes[index / 8] |= 1 << (index % 8);
        self.present += 1;
        Ok(())
    }
    /// Reject context substitution and duplicates before marking any local bit.
    /// Leaves may arrive in any order; proof siblings may not be reordered.
    pub fn verify_and_set(
        &mut self,
        manifest: &Manifest,
        index: usize,
        data: &[u8],
        proof: &[Hash],
    ) -> Result<()> {
        if manifest != &self.manifest {
            return Err(Error::ManifestMismatch);
        }
        if self.have(index)? {
            return Err(Error::Duplicate);
        }
        verify_leaf(manifest, index, data, proof, self.layout.body_len)?;
        self.set(index)
    }
}

/// A receiver's bounded proof-node cache. It retains no body bytes and no proof
/// vectors per leaf: verified paths share the fixed heap and known-node bitmap.
/// After a successful insertion, `proof(index)` is available immediately, so a
/// receiver can relay that leaf before obtaining the rest of the body. The
/// caller retains the leaf bytes and still performs final chain validation.
#[derive(Debug)]
pub struct PartialTree {
    verified: Bitmap,
    hashes: Vec<Hash>,
    known: Vec<u8>,
}

impl PartialTree {
    pub fn new(manifest: &Manifest, max_body: usize) -> Result<Self> {
        let verified = Bitmap::new(manifest, max_body)?;
        let layout = verified.layout;
        let mut hashes = reserved(layout.tree_hash_count)?;
        hashes.resize(layout.tree_hash_count, [0; 32]);
        hashes[0] = manifest.root;
        let mut known = reserved(layout.known_node_bitmap_bytes())?;
        known.resize(layout.known_node_bitmap_bytes(), 0);
        known[0] = 1;
        Ok(Self {
            verified,
            hashes,
            known,
        })
    }

    pub fn manifest(&self) -> &Manifest {
        self.verified.manifest()
    }
    pub fn bitmap(&self) -> &Bitmap {
        &self.verified
    }
    pub fn layout(&self) -> Layout {
        self.verified.layout
    }
    pub fn known_node_count(&self) -> usize {
        self.known
            .iter()
            .map(|byte| byte.count_ones() as usize)
            .sum()
    }

    fn check_node(&self, position: usize, hash: &Hash) -> Result<()> {
        if self.known[position / 8] & (1 << (position % 8)) != 0 && &self.hashes[position] != hash {
            return Err(Error::Root);
        }
        Ok(())
    }
    fn put_node(&mut self, position: usize, hash: Hash) {
        self.hashes[position] = hash;
        self.known[position / 8] |= 1 << (position % 8);
    }

    /// The manifest is fixed at construction, so a packet must first be routed
    /// to this exact manifest by the runtime. Bad proofs, inconsistent paths and
    /// duplicates leave both the verified bitmap and cached nodes unchanged.
    pub fn insert_verified(&mut self, index: usize, data: &[u8], proof: &[Hash]) -> Result<()> {
        if self.verified.have(index)? {
            return Err(Error::Duplicate);
        }
        let layout = self.layout();
        if data.len() != layout.leaf_range(index)?.len() {
            return Err(Error::LeafLength);
        }
        if proof.len() != layout.proof_depth {
            return Err(Error::ProofLength);
        }
        let leaf = leaf_hash(layout, index, data);
        let mut hash = leaf;
        let mut position = layout.padded_leaf_count - 1 + index;
        self.check_node(position, &hash)?;
        for sibling in proof {
            let sibling_position = if position % 2 == 1 {
                position + 1
            } else {
                position - 1
            };
            self.check_node(sibling_position, sibling)?;
            let parent = (position - 1) / 2;
            hash = if position % 2 == 1 {
                node_hash(layout, parent, &hash, sibling)
            } else {
                node_hash(layout, parent, sibling, &hash)
            };
            position = parent;
            self.check_node(position, &hash)?;
        }
        // Also covers a single leaf, whose path has no siblings.
        if hash != self.manifest().root {
            return Err(Error::Root);
        }
        // All checks precede mutation. Recompute only the small parent path;
        // leaf data is hashed once and insertions allocate no scratch vectors.
        hash = leaf;
        position = layout.padded_leaf_count - 1 + index;
        self.put_node(position, hash);
        for sibling in proof {
            let sibling_position = if position % 2 == 1 {
                position + 1
            } else {
                position - 1
            };
            self.put_node(sibling_position, *sibling);
            let parent = (position - 1) / 2;
            hash = if position % 2 == 1 {
                node_hash(layout, parent, &hash, sibling)
            } else {
                node_hash(layout, parent, sibling, &hash)
            };
            position = parent;
            self.put_node(position, hash);
        }
        self.verified.set(index)
    }

    /// Only locally verified leaves can be served, even if a neighbor's leaf
    /// hash happens to be known as a sibling. The result is a transient vector.
    pub fn proof(&self, index: usize) -> Result<Vec<Hash>> {
        if !self.verified.have(index)? {
            return Err(Error::Unavailable);
        }
        let layout = self.layout();
        let mut proof = reserved(layout.proof_depth)?;
        let mut position = layout.padded_leaf_count - 1 + index;
        while position != 0 {
            let sibling = if position % 2 == 1 {
                position + 1
            } else {
                position - 1
            };
            debug_assert_ne!(self.known[sibling / 8] & (1 << (sibling % 8)), 0);
            proof.push(self.hashes[sibling]);
            position = (position - 1) / 2;
        }
        Ok(proof)
    }
}

/// The externally established BFT context, not a peer-selected claim to accept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HolderContext {
    pub domain: Hash,
    pub config: Hash,
    pub height: u64,
}

/// Optional BFT-only holder claim. Its signature binds the entire manifest and
/// context, but cannot establish that its root corresponds to its chain body ID.
/// Roster membership, holder authorization and exact-header authentication are
/// the runtime's responsibility; this helper changes no chain signing format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HolderAttestation {
    pub holder: [u8; 32],
    pub signature: [u8; 64],
}

fn holder_message(context: &HolderContext, manifest: &Manifest, holder: &[u8; 32]) -> Hash {
    let mut h = blake3::Hasher::new();
    h.update(HOLDER_DOMAIN);
    h.update(&context.domain);
    h.update(&context.config);
    h.update(&context.height.to_le_bytes());
    h.update(holder);
    h.update(&manifest.signed_header_identity);
    h.update(&manifest.body_id);
    h.update(&manifest.body_len.to_le_bytes());
    h.update(&manifest.leaf_bytes.to_le_bytes());
    h.update(&manifest.root);
    *h.finalize().as_bytes()
}

impl HolderAttestation {
    pub fn sign(
        context: &HolderContext,
        manifest: &Manifest,
        key: &SigningKey,
        max_body: usize,
    ) -> Result<Self> {
        manifest.validate(max_body)?;
        let holder = VerificationKeyBytes::from(key).into();
        let signature = key.sign(&holder_message(context, manifest, &holder)).into();
        Ok(Self { holder, signature })
    }

    /// `authenticated_holder` must be a key the runtime has authenticated and
    /// admitted in the active roster, not a copy of this untrusted claim's key.
    pub fn verify(
        &self,
        context: &HolderContext,
        manifest: &Manifest,
        authenticated_holder: &[u8; 32],
        max_body: usize,
    ) -> Result<()> {
        manifest.validate(max_body)?;
        if &self.holder != authenticated_holder {
            return Err(Error::Holder);
        }
        let key = VerificationKey::try_from(VerificationKeyBytes::from(self.holder))
            .map_err(|_| Error::Holder)?;
        key.verify(
            &Signature::from(self.signature),
            &holder_message(context, manifest, &self.holder),
        )
        .map_err(|_| Error::Signature)
    }
}
