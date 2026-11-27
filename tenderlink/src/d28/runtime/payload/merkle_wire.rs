//! BFT-only leaf transport. Decode validates the entire record before allocation.
//! Bitmaps/data/proofs remain borrowed; no peer-selected count controls a Vec.
use crate::d28::{
    Error, Id, Result,
    merkle::{HolderAttestation, Layout, Manifest},
};

pub(in crate::d28::runtime) const HAVE: u8 = 0xB7;
pub(in crate::d28::runtime) const WANT: u8 = 0xB8;
pub(in crate::d28::runtime) const LEAF: u8 = 0xB9;
pub(in crate::d28::runtime) const ACK: u8 = 0xBA;
pub(super) const WINDOW: usize = 8;
const VERSION: u8 = 1;
const REFERENCE_BYTES: usize = 148; // config, height, exact manifest
const TICKET_BYTES: usize = 2 + REFERENCE_BYTES + 16;
const HAVE_BYTES: usize = 2 + REFERENCE_BYTES + 96;
pub(super) const MAX_DEPTH: usize = usize::BITS as usize;

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
pub(super) struct Ticket {
    pub reference: Reference,
    pub generation: u64,
    pub token: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Want {
    pub ticket: Ticket,
    pub indices: [u32; WINDOW],
    pub count: u8,
}
impl Want {
    pub fn indices(&self) -> &[u32] {
        &self.indices[..usize::from(self.count)]
    }
    pub fn position(&self, index: u32) -> Option<usize> {
        self.indices().iter().position(|&i| i == index)
    }
}
pub(super) struct Have<'a> {
    pub reference: Reference,
    pub holder: HolderAttestation,
    pub bitmap: &'a [u8],
}
pub(super) struct Leaf<'a> {
    pub ticket: Ticket,
    pub index: u32,
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Ack {
    pub ticket: Ticket,
    pub index: u32,
}

fn layout(reference: Reference, max_body: usize) -> Result<Layout> {
    reference
        .manifest
        .validate(max_body)
        .map_err(|_| Error::Invalid)
}
fn valid_bitmap(bitmap: &[u8], layout: Layout) -> Result<()> {
    if bitmap.len() != layout.bitmap_bytes()
        || (layout.leaf_count() % 8 != 0
            && bitmap[bitmap.len() - 1] >> (layout.leaf_count() % 8) != 0)
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
fn put_ticket(out: &mut Vec<u8>, tag: u8, ticket: Ticket) -> Result<()> {
    if ticket.token == 0 {
        return Err(Error::Invalid);
    }
    out.extend_from_slice(&[tag, VERSION]);
    put_reference(out, ticket.reference);
    out.extend_from_slice(&ticket.generation.to_le_bytes());
    out.extend_from_slice(&ticket.token.to_le_bytes());
    Ok(())
}
pub(super) fn encode_have(have: Have<'_>, max_body: usize) -> Result<Vec<u8>> {
    valid_bitmap(have.bitmap, layout(have.reference, max_body)?)?;
    let mut out = reserved(
        HAVE_BYTES
            .checked_add(have.bitmap.len())
            .ok_or(Error::Exhausted)?,
    )?;
    out.extend_from_slice(&[HAVE, VERSION]);
    put_reference(&mut out, have.reference);
    out.extend_from_slice(&have.holder.holder);
    out.extend_from_slice(&have.holder.signature);
    out.extend_from_slice(have.bitmap);
    Ok(out)
}
pub(super) fn decode_have(bytes: &[u8], max_body: usize) -> Result<Have<'_>> {
    let mut r = Reader::new(bytes, HAVE)?;
    let reference = r.reference()?;
    let geometry = layout(reference, max_body)?;
    let holder = HolderAttestation {
        holder: r.array()?,
        signature: r.array()?,
    };
    let bitmap = r.take(geometry.bitmap_bytes())?;
    valid_bitmap(bitmap, geometry)?;
    r.end()?;
    Ok(Have {
        reference,
        holder,
        bitmap,
    })
}
fn valid_want(want: &Want, max_body: usize) -> Result<()> {
    let geometry = layout(want.ticket.reference, max_body)?;
    let count = usize::from(want.count);
    if count == 0 || count > WINDOW || want.ticket.token == 0 {
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
pub(super) fn encode_want(want: Want, max_body: usize) -> Result<Vec<u8>> {
    valid_want(&want, max_body)?;
    let mut out = reserved(TICKET_BYTES + 1 + usize::from(want.count) * 4)?;
    put_ticket(&mut out, WANT, want.ticket)?;
    out.push(want.count);
    for index in want.indices() {
        out.extend_from_slice(&index.to_le_bytes());
    }
    Ok(out)
}
pub(super) fn decode_want(bytes: &[u8], max_body: usize) -> Result<Want> {
    let mut r = Reader::new(bytes, WANT)?;
    let ticket = r.ticket()?;
    let count = r.array::<1>()?[0];
    if count == 0 || usize::from(count) > WINDOW {
        return Err(Error::Invalid);
    }
    let mut indices = [0; WINDOW];
    for index in &mut indices[..usize::from(count)] {
        *index = u32::from_le_bytes(r.array()?);
    }
    r.end()?;
    let want = Want {
        ticket,
        indices,
        count,
    };
    valid_want(&want, max_body)?;
    Ok(want)
}
pub(super) fn encode_leaf(
    ticket: Ticket,
    index: u32,
    data: &[u8],
    proof: &[[u8; 32]],
    max_body: usize,
) -> Result<Vec<u8>> {
    let geometry = layout(ticket.reference, max_body)?;
    if data.len()
        != geometry
            .leaf_range(index as usize)
            .map_err(|_| Error::Invalid)?
            .len()
        || proof.len() != geometry.proof_depth()
    {
        return Err(Error::Invalid);
    }
    let size = (TICKET_BYTES + 4)
        .checked_add(geometry.proof_bytes())
        .and_then(|n| n.checked_add(data.len()))
        .ok_or(Error::Exhausted)?;
    let mut out = reserved(size)?;
    put_ticket(&mut out, LEAF, ticket)?;
    out.extend_from_slice(&index.to_le_bytes());
    for sibling in proof {
        out.extend_from_slice(sibling);
    }
    out.extend_from_slice(data);
    Ok(out)
}
pub(super) fn decode_leaf(bytes: &[u8], max_body: usize) -> Result<Leaf<'_>> {
    let mut r = Reader::new(bytes, LEAF)?;
    let ticket = r.ticket()?;
    let geometry = layout(ticket.reference, max_body)?;
    let index = u32::from_le_bytes(r.array()?);
    let len = geometry
        .leaf_range(index as usize)
        .map_err(|_| Error::Invalid)?
        .len();
    let proof = r.take(geometry.proof_bytes())?;
    let data = r.take(len)?;
    r.end()?;
    Ok(Leaf {
        ticket,
        index,
        data,
        proof,
    })
}
pub(super) fn encode_ack(ack: Ack, max_body: usize) -> Result<Vec<u8>> {
    layout(ack.ticket.reference, max_body)?
        .leaf_range(ack.index as usize)
        .map_err(|_| Error::Invalid)?;
    let mut out = reserved(TICKET_BYTES + 4)?;
    put_ticket(&mut out, ACK, ack.ticket)?;
    out.extend_from_slice(&ack.index.to_le_bytes());
    Ok(out)
}
pub(super) fn decode_ack(bytes: &[u8], max_body: usize) -> Result<Ack> {
    let mut r = Reader::new(bytes, ACK)?;
    let ticket = r.ticket()?;
    let index = u32::from_le_bytes(r.array()?);
    r.end()?;
    layout(ticket.reference, max_body)?
        .leaf_range(index as usize)
        .map_err(|_| Error::Invalid)?;
    Ok(Ack { ticket, index })
}
pub(in crate::d28::runtime) fn max_frame_bytes(
    max_body: usize,
    leaf_bytes: usize,
) -> Result<usize> {
    let manifest = Manifest {
        body_id: [0; 32],
        signed_header_identity: [0; 32],
        body_len: u64::try_from(max_body).map_err(|_| Error::Invalid)?,
        leaf_bytes: u32::try_from(leaf_bytes).map_err(|_| Error::Invalid)?,
        root: [0; 32],
    };
    let geometry = manifest.validate(max_body).map_err(|_| Error::Invalid)?;
    // u32 leaf indices are a checked runtime limit, never a truncating conversion.
    u32::try_from(geometry.leaf_count()).map_err(|_| Error::Invalid)?;
    let data = (TICKET_BYTES + 4)
        .checked_add(geometry.proof_bytes())
        .and_then(|n| n.checked_add(max_body.min(leaf_bytes)))
        .ok_or(Error::Exhausted)?;
    let have = HAVE_BYTES
        .checked_add(geometry.bitmap_bytes())
        .ok_or(Error::Exhausted)?;
    Ok(data.max(have).max(TICKET_BYTES + 1 + WINDOW * 4))
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
    fn ticket(&mut self) -> Result<Ticket> {
        let reference = self.reference()?;
        let generation = u64::from_le_bytes(self.array()?);
        let token = u64::from_le_bytes(self.array()?);
        if token == 0 {
            return Err(Error::Invalid);
        }
        Ok(Ticket {
            reference,
            generation,
            token,
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
