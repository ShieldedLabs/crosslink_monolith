//! BFT-only unordered shard transport, version 2. No application ACK or window.
//! Decode borrows data/proofs/bitmaps and checks all lengths before allocation.
use crate::d28::{
    Error, Header, Id, ProposalContent, Result,
    merkle::{Layout, MIN_LEAF_BYTES, Manifest, ProposerAttestation},
};

pub(in crate::d28::runtime) const HAVE: u8 = 0xB7;
pub(in crate::d28::runtime) const WANT: u8 = 0xB8;
pub(in crate::d28::runtime) const LEAF: u8 = 0xB9;
// An index-list encoding bound, NOT a receive/send window or completion gate.
pub(super) const MAX_WANT: usize = 32;
const VERSION: u8 = 2;
const REFERENCE_BYTES: usize = 148;
const HEADER_BYTES: usize = 125;
const AUTH_BYTES: usize = HEADER_BYTES + REFERENCE_BYTES + 96;
const HAVE_BYTES: usize = 2 + AUTH_BYTES;
const LEAF_BYTES: usize = 2 + 4 + AUTH_BYTES;
pub(super) const MAX_DEPTH: usize = usize::BITS as usize;

/// Fixed encoded-byte allowance across leaf-size experiments: K_tx complete
/// bodies plus their WORST geometry envelope at MIN_LEAF_BYTES. Includes one
/// HAVE and one maximum WANT per body. This is observed queued/staged work,
/// not a bound on all native STP/FEC allocations or concurrent Base producers.
/// Older runtime control producers keep independent bounds; if their staging
/// already exceeds this ceiling, the Merkle scheduler adds no further work.
pub(in crate::d28::runtime) fn encoded_byte_limit(
    max_body: usize,
    outgoing: usize,
) -> Result<usize> {
    let geometry = Manifest {
        body_id: [0; 32],
        signed_header_identity: [0; 32],
        root: [0; 32],
        body_len: u64::try_from(max_body).map_err(|_| Error::Invalid)?,
        leaf_bytes: MIN_LEAF_BYTES as u32,
    }
    .validate(max_body)
    .map_err(|_| Error::Invalid)?;
    let envelope = LEAF_BYTES
        .checked_add(geometry.proof_bytes())
        .ok_or(Error::Exhausted)?;
    let body = max_body
        .checked_add(
            geometry
                .leaf_count()
                .checked_mul(envelope)
                .ok_or(Error::Exhausted)?,
        )
        .and_then(|n| n.checked_add(HAVE_BYTES + geometry.bitmap_bytes() + want_size(MAX_WANT)))
        .ok_or(Error::Exhausted)?;
    body.checked_mul(outgoing).ok_or(Error::Exhausted)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Reference {
    pub config: Id,
    pub height: u64,
    pub manifest: Manifest,
}
impl Reference {
    pub fn key(self) -> super::BodyKey {
        super::BodyKey {
            config: self.config,
            height: self.height,
            id: self.manifest.body_id,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Authentication {
    pub header: Header,
    pub reference: Reference,
    pub proposer: ProposerAttestation,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Want {
    pub reference: Reference,
    pub indices: [u32; MAX_WANT],
    pub count: u8,
}
impl Want {
    pub fn indices(&self) -> &[u32] {
        &self.indices[..usize::from(self.count)]
    }
    pub fn single(reference: Reference, index: u32) -> Self {
        let mut indices = [0; MAX_WANT];
        indices[0] = index;
        Self {
            reference,
            indices,
            count: 1,
        }
    }
    pub fn add(&mut self, index: u32) -> bool {
        if usize::from(self.count) == MAX_WANT || self.indices().contains(&index) {
            return false;
        }
        self.indices[usize::from(self.count)] = index;
        self.count += 1;
        true
    }
}
pub(super) struct Have<'a> {
    pub auth: Authentication,
    pub bitmap: &'a [u8],
}
pub(super) struct Leaf<'a> {
    pub index: u32,
    pub auth: Authentication,
    pub data: &'a [u8],
    pub proof: &'a [u8],
}
impl Leaf<'_> {
    pub fn proof_array(&self) -> ([[u8; 32]; MAX_DEPTH], usize) {
        let mut hashes = [[0; 32]; MAX_DEPTH];
        let count = self.proof.len() / 32;
        for (hash, bytes) in hashes.iter_mut().zip(self.proof.chunks_exact(32)) {
            hash.copy_from_slice(bytes);
        }
        (hashes, count)
    }
}
fn layout(reference: Reference, max_body: usize) -> Result<Layout> {
    reference
        .manifest
        .validate(max_body)
        .map_err(|_| Error::Invalid)
}
fn valid_bitmap(bitmap: &[u8], geometry: Layout) -> Result<()> {
    if bitmap.len() != geometry.bitmap_bytes()
        || (geometry.leaf_count() % 8 != 0
            && bitmap[bitmap.len() - 1] >> (geometry.leaf_count() % 8) != 0)
    {
        return Err(Error::Invalid);
    }
    Ok(())
}
fn reserved(size: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(size)
        .map_err(|_| Error::Exhausted)?;
    Ok(bytes)
}
fn put_reference(out: &mut Vec<u8>, reference: Reference) {
    out.extend_from_slice(&reference.config);
    out.extend_from_slice(&reference.height.to_le_bytes());
    let m = reference.manifest;
    out.extend_from_slice(&m.body_id);
    out.extend_from_slice(&m.signed_header_identity);
    out.extend_from_slice(&m.body_len.to_le_bytes());
    out.extend_from_slice(&m.leaf_bytes.to_le_bytes());
    out.extend_from_slice(&m.root);
}
fn put_auth(out: &mut Vec<u8>, auth: Authentication) {
    let p = auth.header.content;
    out.extend_from_slice(&p.height.to_le_bytes());
    out.extend_from_slice(&p.round.to_le_bytes());
    out.push(u8::from(p.valid_round.is_some()));
    out.extend_from_slice(&p.valid_round.unwrap_or(0).to_le_bytes());
    out.extend_from_slice(&p.id);
    out.extend_from_slice(&p.signer.to_le_bytes());
    out.extend_from_slice(&auth.header.signature);
    put_reference(out, auth.reference);
    out.extend_from_slice(&auth.proposer.proposer);
    out.extend_from_slice(&auth.proposer.signature);
}
pub(super) fn have_size(reference: Reference, max_body: usize) -> Result<usize> {
    HAVE_BYTES
        .checked_add(layout(reference, max_body)?.bitmap_bytes())
        .ok_or(Error::Exhausted)
}
pub(super) fn encode_have(have: Have<'_>, max_body: usize) -> Result<Vec<u8>> {
    valid_bitmap(have.bitmap, layout(have.auth.reference, max_body)?)?;
    let mut out = reserved(have_size(have.auth.reference, max_body)?)?;
    out.extend_from_slice(&[HAVE, VERSION]);
    put_auth(&mut out, have.auth);
    out.extend_from_slice(have.bitmap);
    Ok(out)
}
pub(super) fn decode_have(bytes: &[u8], max_body: usize) -> Result<Have<'_>> {
    let mut r = Reader::new(bytes, HAVE)?;
    let auth = r.auth()?;
    let geometry = layout(auth.reference, max_body)?;
    let bitmap = r.take(geometry.bitmap_bytes())?;
    valid_bitmap(bitmap, geometry)?;
    r.end()?;
    Ok(Have { auth, bitmap })
}
fn valid_want(want: &Want, max_body: usize) -> Result<()> {
    let geometry = layout(want.reference, max_body)?;
    let count = usize::from(want.count);
    if count == 0 || count > MAX_WANT {
        return Err(Error::Invalid);
    }
    for (pos, &index) in want.indices().iter().enumerate() {
        geometry
            .leaf_range(index as usize)
            .map_err(|_| Error::Invalid)?;
        if want.indices()[..pos].contains(&index) {
            return Err(Error::Invalid);
        }
    }
    Ok(())
}
pub(super) fn want_size(count: usize) -> usize {
    2 + REFERENCE_BYTES + 1 + count * 4
}
pub(super) fn encode_want(want: Want, max_body: usize) -> Result<Vec<u8>> {
    valid_want(&want, max_body)?;
    let mut out = reserved(want_size(usize::from(want.count)))?;
    out.extend_from_slice(&[WANT, VERSION]);
    put_reference(&mut out, want.reference);
    out.push(want.count);
    for index in want.indices() {
        out.extend_from_slice(&index.to_le_bytes());
    }
    Ok(out)
}
pub(super) fn decode_want(bytes: &[u8], max_body: usize) -> Result<Want> {
    let mut r = Reader::new(bytes, WANT)?;
    let reference = r.reference()?;
    let count = r.array::<1>()?[0];
    if count == 0 || usize::from(count) > MAX_WANT {
        return Err(Error::Invalid);
    }
    let mut indices = [0; MAX_WANT];
    for index in &mut indices[..usize::from(count)] {
        *index = u32::from_le_bytes(r.array()?);
    }
    r.end()?;
    let want = Want {
        reference,
        indices,
        count,
    };
    valid_want(&want, max_body)?;
    Ok(want)
}
pub(super) fn leaf_size(reference: Reference, index: u32, max_body: usize) -> Result<usize> {
    let geometry = layout(reference, max_body)?;
    LEAF_BYTES
        .checked_add(geometry.proof_bytes())
        .and_then(|n| n.checked_add(geometry.leaf_range(index as usize).ok()?.len()))
        .ok_or(Error::Invalid)
}
pub(super) fn encode_leaf(
    auth: Authentication,
    index: u32,
    data: &[u8],
    proof: &[[u8; 32]],
    max_body: usize,
) -> Result<Vec<u8>> {
    let geometry = layout(auth.reference, max_body)?;
    if data.len()
        != geometry
            .leaf_range(index as usize)
            .map_err(|_| Error::Invalid)?
            .len()
        || proof.len() != geometry.proof_depth()
    {
        return Err(Error::Invalid);
    }
    let mut out = reserved(leaf_size(auth.reference, index, max_body)?)?;
    out.extend_from_slice(&[LEAF, VERSION]);
    // Position, not an arrival sequence: authenticated by the indexed leaf hash.
    out.extend_from_slice(&index.to_le_bytes());
    put_auth(&mut out, auth);
    for sibling in proof {
        out.extend_from_slice(sibling);
    }
    out.extend_from_slice(data);
    Ok(out)
}
pub(super) fn decode_leaf(bytes: &[u8], max_body: usize) -> Result<Leaf<'_>> {
    let mut r = Reader::new(bytes, LEAF)?;
    let index = u32::from_le_bytes(r.array()?);
    let auth = r.auth()?;
    let geometry = layout(auth.reference, max_body)?;
    let len = geometry
        .leaf_range(index as usize)
        .map_err(|_| Error::Invalid)?
        .len();
    let proof = r.take(geometry.proof_bytes())?;
    let data = r.take(len)?;
    r.end()?;
    Ok(Leaf {
        index,
        auth,
        data,
        proof,
    })
}
pub(in crate::d28::runtime) fn max_frame_bytes(
    max_body: usize,
    leaf_bytes: usize,
) -> Result<usize> {
    let reference = Reference {
        config: [0; 32],
        height: 0,
        manifest: Manifest {
            body_id: [0; 32],
            signed_header_identity: [0; 32],
            body_len: u64::try_from(max_body).map_err(|_| Error::Invalid)?,
            leaf_bytes: u32::try_from(leaf_bytes).map_err(|_| Error::Invalid)?,
            root: [0; 32],
        },
    };
    let geometry = layout(reference, max_body)?;
    u32::try_from(geometry.leaf_count()).map_err(|_| Error::Invalid)?;
    Ok(leaf_size(reference, 0, max_body)?
        .max(have_size(reference, max_body)?)
        .max(want_size(MAX_WANT)))
}
struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}
impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8], tag: u8) -> Result<Self> {
        if bytes.get(..2) != Some(&[tag, VERSION]) {
            return Err(Error::Invalid);
        }
        Ok(Self { bytes, offset: 2 })
    }
    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self.offset.checked_add(len).ok_or(Error::Invalid)?;
        let bytes = self.bytes.get(self.offset..end).ok_or(Error::Invalid)?;
        self.offset = end;
        Ok(bytes)
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        self.take(N)?.try_into().map_err(|_| Error::Invalid)
    }
    fn reference(&mut self) -> Result<Reference> {
        Ok(Reference {
            config: self.array()?,
            height: u64::from_le_bytes(self.array()?),
            manifest: Manifest {
                body_id: self.array()?,
                signed_header_identity: self.array()?,
                body_len: u64::from_le_bytes(self.array()?),
                leaf_bytes: u32::from_le_bytes(self.array()?),
                root: self.array()?,
            },
        })
    }
    fn auth(&mut self) -> Result<Authentication> {
        let height = u64::from_le_bytes(self.array()?);
        let round = u64::from_le_bytes(self.array()?);
        let tag = self.array::<1>()?[0];
        let valid = u64::from_le_bytes(self.array()?);
        let valid_round = match (tag, valid) {
            (0, 0) => None,
            (1, n) => Some(n),
            _ => return Err(Error::Invalid),
        };
        let header = Header {
            content: ProposalContent {
                height,
                round,
                valid_round,
                id: self.array()?,
                signer: u32::from_le_bytes(self.array()?),
            },
            signature: self.array()?,
        };
        Ok(Authentication {
            header,
            reference: self.reference()?,
            proposer: ProposerAttestation {
                proposer: self.array()?,
                signature: self.array()?,
            },
        })
    }
    fn end(self) -> Result<()> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(Error::Invalid)
        }
    }
}

#[cfg(test)]
mod tests;
