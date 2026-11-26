//! Bounded durable D28 workspace over one [`AtomicBlockStore`] image.
//!
//! Each successful `Storage` mutation commits the entire image before returning.
//! This deliberately favors the existing publication contract over write efficiency:
//! even an unreferenced certificate row is durable, not an acknowledged RAM write.
//! An unsuccessful replacement poisons this adapter. Drop the engine, recover its
//! backend with `into_backend`, reopen, and use `Engine::recover_with_emission`.
//! Never continue from the possibly newer working copy after a replacement error.
//!
//! The codec is independent of Rust layout. There are two configuration banks,
//! three bodies, four certificate regions, and constant-sized signing/publication
//! records. The configured bounds are immutable across roster changes. One image
//! buffer is reused; backend copies and the engine's volatile workspace are separate.
use super::*;

const MAGIC: &[u8; 8] = b"D28IMG01";
const VERSION: u32 = 1;
const PREFIX: usize = 36; // magic, version, Nmax, Bmax, payload length
const CHECKSUM: usize = 32;
// All scalar records, option tags, length words, and headers fit below this
// allowance (including both CONFIGs and the complete DESC). Unused bytes are zero.
const SCALAR_BYTES: usize = 4096;
const MEMBER_BYTES: usize = 40;
const OPTIONAL_VOTE_BYTES: usize = 119;

/// Fixed deployment bounds, not the current height's roster/body lengths.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub max_members: usize,
    pub max_body: usize,
}

/// The exact physical block size for these bounds. All arithmetic is checked.
pub fn image_size(limits: Limits) -> Result<usize> {
    if limits.max_members == 0
        || limits.max_members > u32::MAX as usize
        || limits.max_body == 0
        || u64::try_from(limits.max_body).is_err()
    {
        return Err(Error::Config);
    }
    let members = limits
        .max_members
        .checked_mul(2 * MEMBER_BYTES + 4 * OPTIONAL_VOTE_BYTES)
        .ok_or(Error::Config)?;
    let bodies = limits.max_body.checked_mul(3).ok_or(Error::Config)?;
    let size = SCALAR_BYTES
        .checked_add(members)
        .and_then(|n| n.checked_add(bodies))
        .ok_or(Error::Config)?;
    if size > isize::MAX as usize {
        return Err(Error::Config);
    }
    Ok(size)
}

/// Single-owner durable adapter. It is intentionally neither `Clone` nor a
/// source of a mutable backend reference: replacement errors require reopening.
pub struct DurableStore<B: AtomicBlockStore> {
    backend: B,
    limits: Limits,
    memory: MemoryStore,
    image: Vec<u8>,
    poisoned: bool,
}

impl<B: AtomicBlockStore> DurableStore<B> {
    /// Load a complete committed image, or attach genuinely uninitialized storage.
    /// Bad size, version, padding, lengths, checksum or records fail closed. A
    /// corrupt image is never interpreted as permission to bootstrap fresh keys.
    pub fn open(mut backend: B, limits: Limits) -> Result<Self> {
        let size = image_size(limits)?;
        if backend.block_size() != size {
            return Err(Error::Config);
        }
        let mut image = capacity(size)?;
        image.resize(size, 0);
        let memory = if backend.load(&mut image)? {
            decode(&image, limits)?
        } else {
            MemoryStore::default()
        };
        Ok(Self {
            backend,
            limits,
            memory,
            image,
            poisoned: false,
        })
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// Discard every cached byte and return the backend for a fresh load.
    pub fn into_backend(self) -> B {
        self.backend
    }

    fn ready(&self) -> Result<()> {
        if self.poisoned {
            Err(Error::Storage)
        } else {
            Ok(())
        }
    }

    fn update(&mut self, mutate: impl FnOnce(&mut MemoryStore) -> Result<()>) -> Result<()> {
        self.ready()?;
        // MemoryStore's counter/fault injector belongs to the reference tests,
        // not to durable state, and must not grow over this store's lifetime.
        self.memory.writes = 0;
        let result = mutate(&mut self.memory)
            .and_then(|()| encode(&self.memory, self.limits, &mut self.image))
            .and_then(|()| self.backend.replace(&self.image));
        self.memory.writes = 0;
        if result.is_err() {
            self.poisoned = true;
            // A backend may report any Error variant. Normalize it so the core
            // always deactivates before another callback or signing operation.
            return Err(Error::Storage);
        }
        Ok(())
    }

    fn index(&self, index: usize, bound: usize) -> Result<()> {
        self.ready()?;
        if index < bound {
            Ok(())
        } else {
            Err(Error::Corrupt)
        }
    }
}

impl<B: AtomicBlockStore> Storage for DurableStore<B> {
    fn config(&self) -> Option<&Config> {
        self.memory.config()
    }
    fn config_bank_hash(&self, bank: usize) -> Option<Id> {
        (bank < 2)
            .then(|| self.memory.config_bank_hash(bank))
            .flatten()
    }
    fn begin_config(&mut self, bank: usize, meta: ConfigMeta) -> Result<()> {
        self.index(bank, 2)?;
        if meta.count == 0
            || meta.count > self.limits.max_members
            || meta.max_body == 0
            || meta.max_body > self.limits.max_body
        {
            return Err(Error::Config);
        }
        if self
            .memory
            .descriptor()
            .is_some_and(|d| self.memory.config_bank_hash(bank) == Some(d.config))
        {
            return Err(Error::Corrupt);
        }
        self.update(|m| m.begin_config(bank, meta))
    }
    fn write_config_member(&mut self, bank: usize, row: usize, member: Member) -> Result<()> {
        self.index(bank, 2)?;
        let count = self.memory.configs[bank]
            .as_ref()
            .ok_or(Error::Corrupt)?
            .members
            .len();
        self.index(row, count)?;
        if self.memory.sealed[bank] {
            return Err(Error::Corrupt);
        }
        self.update(|m| m.write_config_member(bank, row, member))
    }
    fn seal_config_bank(&mut self, bank: usize, hash: Id) -> Result<()> {
        self.index(bank, 2)?;
        self.update(|m| m.seal_config_bank(bank, hash))
    }
    fn seal_config(&mut self, c: &Config) -> Result<()> {
        self.ready()?;
        if c.members.len() > self.limits.max_members || c.max_body > self.limits.max_body {
            return Err(Error::Config);
        }
        if (0..2).any(|bank| self.config_bank_hash(bank) == Some(c.hash)) {
            return Ok(());
        }
        let current = self.descriptor().map(|d| d.config);
        let bank = (0..2)
            .find(|bank| self.config_bank_hash(*bank) != current || current.is_none())
            .ok_or(Error::Exhausted)?;
        self.begin_config(
            bank,
            ConfigMeta {
                domain: c.domain,
                total: c.total,
                count: c.members.len(),
                max_body: c.max_body,
                rho: c.rho,
                timeouts: c.timeouts,
                crosslink: c.crosslink,
            },
        )?;
        for (row, member) in c.members.iter().enumerate() {
            self.write_config_member(bank, row, member.clone())?;
        }
        self.seal_config_bank(bank, c.hash)
    }
    fn descriptor(&self) -> Option<&Descriptor> {
        self.memory.descriptor()
    }
    fn write_descriptor(&mut self, d: &Descriptor) -> Result<()> {
        self.ready()?;
        if !(0..2).any(|bank| self.config_bank_hash(bank) == Some(d.config)) {
            return Err(Error::Corrupt);
        }
        self.update(|m| m.write_descriptor(d))
    }
    fn body(&self, slot: usize) -> &[u8] {
        self.memory.bodies.get(slot).map_or(&[], Vec::as_slice)
    }
    fn header(&self, slot: usize) -> Option<&Header> {
        self.memory.headers.get(slot).and_then(Option::as_ref)
    }
    fn write_body(&mut self, slot: usize, body: &[u8]) -> Result<()> {
        self.index(slot, 3)?;
        let max = self.config().ok_or(Error::Config)?.max_body;
        if body.len() > max || body.len() > self.limits.max_body {
            return Err(Error::Invalid);
        }
        self.update(|m| m.write_body(slot, body))
    }
    fn copy_body(&mut self, from: usize, to: usize) -> Result<()> {
        self.index(from, 3)?;
        self.index(to, 3)?;
        if from == to {
            return Err(Error::Corrupt);
        }
        self.update(|m| m.copy_body(from, to))
    }
    fn write_header(&mut self, slot: usize, header: Header) -> Result<()> {
        self.index(slot, 3)?;
        self.update(|m| m.write_header(slot, header))
    }
    fn region_header(&self, region: usize) -> Option<RegionHeader> {
        self.memory.region_headers.get(region).copied().flatten()
    }
    fn region_rows(&self, region: usize) -> &[Option<Vote>] {
        if region >= 4 {
            return &[];
        }
        self.memory.region_rows(region)
    }
    fn write_region_row(&mut self, region: usize, row: usize, vote: Option<Vote>) -> Result<()> {
        self.index(region, 4)?;
        self.index(row, self.config().ok_or(Error::Config)?.members.len())?;
        self.update(|m| m.write_region_row(region, row, vote))
    }
    fn write_region_header(&mut self, region: usize, header: RegionHeader) -> Result<()> {
        self.index(region, 4)?;
        if header.count > self.limits.max_members {
            return Err(Error::Corrupt);
        }
        self.update(|m| m.write_region_header(region, header))
    }
    fn own(&self, kind: VoteKind) -> Option<&Vote> {
        self.memory.own(kind)
    }
    fn write_own(&mut self, kind: VoteKind, vote: Vote) -> Result<()> {
        self.update(|m| m.write_own(kind, vote))
    }
    fn own_header(&self) -> Option<&Header> {
        self.memory.own_header()
    }
    fn write_own_header(&mut self, header: Header) -> Result<()> {
        self.update(|m| m.write_own_header(header))
    }
}

fn capacity<T>(size: usize) -> Result<Vec<T>> {
    let mut out = Vec::new();
    out.try_reserve_exact(size).map_err(|_| Error::Exhausted)?;
    Ok(out)
}

struct Writer<'a> {
    bytes: &'a mut [u8],
    at: usize,
}
impl Writer<'_> {
    fn put(&mut self, bytes: &[u8]) -> Result<()> {
        let end = self.at.checked_add(bytes.len()).ok_or(Error::Corrupt)?;
        self.bytes
            .get_mut(self.at..end)
            .ok_or(Error::Corrupt)?
            .copy_from_slice(bytes);
        self.at = end;
        Ok(())
    }
    fn byte(&mut self, v: u8) -> Result<()> {
        self.put(&[v])
    }
    fn flag(&mut self, v: bool) -> Result<()> {
        self.byte(u8::from(v))
    }
    fn u64(&mut self, v: u64) -> Result<()> {
        self.put(&v.to_le_bytes())
    }
    fn usize(&mut self, v: usize) -> Result<()> {
        self.u64(u64::try_from(v).map_err(|_| Error::Corrupt)?)
    }
    fn optional<T>(
        &mut self,
        v: &Option<T>,
        write: impl FnOnce(&mut Self, &T) -> Result<()>,
    ) -> Result<()> {
        self.flag(v.is_some())?;
        if let Some(v) = v {
            write(self, v)?;
        }
        Ok(())
    }
}
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}
impl<'a> Reader<'a> {
    fn take(&mut self, size: usize) -> Result<&'a [u8]> {
        let end = self.at.checked_add(size).ok_or(Error::Corrupt)?;
        let out = self.bytes.get(self.at..end).ok_or(Error::Corrupt)?;
        self.at = end;
        Ok(out)
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        self.take(N)?.try_into().map_err(|_| Error::Corrupt)
    }
    fn byte(&mut self) -> Result<u8> {
        Ok(self.array::<1>()?[0])
    }
    fn flag(&mut self) -> Result<bool> {
        match self.byte()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(Error::Corrupt),
        }
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.array()?))
    }
    fn usize(&mut self, max: usize) -> Result<usize> {
        let n = usize::try_from(self.u64()?).map_err(|_| Error::Corrupt)?;
        if n > max {
            return Err(Error::Corrupt);
        }
        Ok(n)
    }
    fn optional<T>(&mut self, read: impl FnOnce(&mut Self) -> Result<T>) -> Result<Option<T>> {
        if self.flag()? {
            Ok(Some(read(self)?))
        } else {
            Ok(None)
        }
    }
    fn remaining(&self) -> usize {
        self.bytes.len() - self.at
    }
}

fn write_timeouts(w: &mut Writer<'_>, t: Timeouts) -> Result<()> {
    for x in [
        t.propose,
        t.prevote,
        t.precommit,
        t.inc_propose,
        t.inc_prevote,
        t.inc_precommit,
    ] {
        w.u64(x)?;
    }
    Ok(())
}
fn read_timeouts(r: &mut Reader<'_>) -> Result<Timeouts> {
    Ok(Timeouts {
        propose: r.u64()?,
        prevote: r.u64()?,
        precommit: r.u64()?,
        inc_propose: r.u64()?,
        inc_prevote: r.u64()?,
        inc_precommit: r.u64()?,
    })
}
fn write_vote_content(w: &mut Writer<'_>, v: &VoteContent) -> Result<()> {
    w.u64(v.height)?;
    w.u64(v.round)?;
    w.byte(v.kind as u8)?;
    w.optional(&v.id, |w, id| w.put(id))?;
    w.put(&v.signer.to_le_bytes())
}
fn read_vote_kind(r: &mut Reader<'_>) -> Result<VoteKind> {
    match r.byte()? {
        0 => Ok(VoteKind::Enter),
        1 => Ok(VoteKind::Prevote),
        2 => Ok(VoteKind::Precommit),
        _ => Err(Error::Corrupt),
    }
}
fn read_vote_content(r: &mut Reader<'_>) -> Result<VoteContent> {
    Ok(VoteContent {
        height: r.u64()?,
        round: r.u64()?,
        kind: read_vote_kind(r)?,
        id: r.optional(Reader::array)?,
        signer: u32::from_le_bytes(r.array()?),
    })
}
fn write_vote(w: &mut Writer<'_>, v: &Vote) -> Result<()> {
    write_vote_content(w, &v.content)?;
    w.put(&v.signature)
}
fn read_vote(r: &mut Reader<'_>) -> Result<Vote> {
    Ok(Vote {
        content: read_vote_content(r)?,
        signature: r.array()?,
    })
}
fn write_content(w: &mut Writer<'_>, p: &ProposalContent) -> Result<()> {
    w.u64(p.height)?;
    w.u64(p.round)?;
    w.optional(&p.valid_round, |w, v| w.u64(*v))?;
    w.put(&p.id)?;
    w.put(&p.signer.to_le_bytes())
}
fn read_content(r: &mut Reader<'_>) -> Result<ProposalContent> {
    Ok(ProposalContent {
        height: r.u64()?,
        round: r.u64()?,
        valid_round: r.optional(Reader::u64)?,
        id: r.array()?,
        signer: u32::from_le_bytes(r.array()?),
    })
}
fn write_header(w: &mut Writer<'_>, h: &Header) -> Result<()> {
    write_content(w, &h.content)?;
    w.put(&h.signature)
}
fn read_header(r: &mut Reader<'_>) -> Result<Header> {
    Ok(Header {
        content: read_content(r)?,
        signature: r.array()?,
    })
}
fn write_slot(w: &mut Writer<'_>, s: &SlotRef) -> Result<()> {
    w.usize(s.index)?;
    w.put(&s.header)
}
fn read_slot(r: &mut Reader<'_>) -> Result<SlotRef> {
    Ok(SlotRef {
        index: r.usize(2)?,
        header: r.array()?,
    })
}
fn write_region(w: &mut Writer<'_>, s: &RegionRef) -> Result<()> {
    w.usize(s.index)?;
    w.put(&s.digest)
}
fn read_region(r: &mut Reader<'_>) -> Result<RegionRef> {
    Ok(RegionRef {
        index: r.usize(3)?,
        digest: r.array()?,
    })
}
fn write_certified(w: &mut Writer<'_>, c: &Certified) -> Result<()> {
    write_slot(w, &c.proposal)?;
    write_region(w, &c.certificate)
}
fn read_certified(r: &mut Reader<'_>) -> Result<Certified> {
    Ok(Certified {
        proposal: read_slot(r)?,
        certificate: read_region(r)?,
    })
}
fn write_descriptor(w: &mut Writer<'_>, d: &Descriptor) -> Result<()> {
    w.u64(d.height)?;
    w.put(&d.config)?;
    w.u64(d.round)?;
    w.byte(d.step as u8)?;
    w.optional(&d.locked_round, |w, v| w.u64(*v))?;
    w.optional(&d.locked_id, |w, v| w.put(v))?;
    w.optional(&d.valid_round, |w, v| w.u64(*v))?;
    w.optional(&d.slot, write_slot)?;
    w.flag(d.justified)?;
    w.optional(&d.vc, write_certified)?;
    w.optional(&d.entry, write_region)?;
    w.optional(&d.pending, write_certified)?;
    for s in d.statuses {
        w.byte(s as u8)?;
    }
    for g in d.body_generations {
        w.u64(g)?;
    }
    w.u64(d.next_generation)?;
    for t in d.timers {
        w.flag(t.active)?;
        w.u64(t.remaining)?;
        w.u64(t.height)?;
        w.u64(t.round)?;
    }
    w.u64(d.height_remaining)?;
    w.u64(d.grid_remaining)?;
    for intent in &d.intents {
        w.optional(intent, write_vote_content)?;
    }
    w.optional(&d.proposal_intent, |w, p| {
        write_content(w, &p.content)?;
        w.usize(p.slot)
    })?;
    w.flag(d.proposal_published)
}
fn read_descriptor(r: &mut Reader<'_>) -> Result<Descriptor> {
    let height = r.u64()?;
    let config = r.array()?;
    let round = r.u64()?;
    let step = match r.byte()? {
        0 => Step::PreRound,
        1 => Step::Propose,
        2 => Step::Prevote,
        3 => Step::Precommit,
        4 => Step::Decided,
        _ => return Err(Error::Corrupt),
    };
    let locked_round = r.optional(Reader::u64)?;
    let locked_id = r.optional(Reader::array)?;
    let valid_round = r.optional(Reader::u64)?;
    let slot = r.optional(read_slot)?;
    let justified = r.flag()?;
    let vc = r.optional(read_certified)?;
    let entry = r.optional(read_region)?;
    let pending = r.optional(read_certified)?;
    let mut statuses = [Status::Indeterminate; 3];
    for status in &mut statuses {
        *status = match r.byte()? {
            0 => Status::Indeterminate,
            1 => Status::Valid,
            2 => Status::Invalid,
            _ => return Err(Error::Corrupt),
        };
    }
    let mut body_generations = [0; 3];
    for g in &mut body_generations {
        *g = r.u64()?;
    }
    let next_generation = r.u64()?;
    let mut timers = [Timer::off(0, 0); 4];
    for t in &mut timers {
        *t = Timer {
            active: r.flag()?,
            remaining: r.u64()?,
            height: r.u64()?,
            round: r.u64()?,
        };
    }
    let height_remaining = r.u64()?;
    let grid_remaining = r.u64()?;
    let mut intents = [None; 3];
    for intent in &mut intents {
        *intent = r.optional(read_vote_content)?;
    }
    let proposal_intent = r.optional(|r| {
        Ok(ProposalIntent {
            content: read_content(r)?,
            slot: r.usize(2)?,
        })
    })?;
    let proposal_published = r.flag()?;
    Ok(Descriptor {
        height,
        config,
        round,
        step,
        locked_round,
        locked_id,
        valid_round,
        slot,
        justified,
        vc,
        entry,
        pending,
        statuses,
        body_generations,
        next_generation,
        timers,
        height_remaining,
        grid_remaining,
        intents,
        proposal_intent,
        proposal_published,
    })
}
fn write_region_header(w: &mut Writer<'_>, h: &RegionHeader) -> Result<()> {
    match h.kind {
        CertKind::Polka { round, id } => {
            w.byte(0)?;
            w.u64(round)?;
            w.put(&id)?;
        }
        CertKind::Decision { round, id } => {
            w.byte(1)?;
            w.u64(round)?;
            w.put(&id)?;
        }
        CertKind::Timeout { round } => {
            w.byte(2)?;
            w.u64(round)?;
        }
        CertKind::Jump { round } => {
            w.byte(3)?;
            w.u64(round)?;
        }
        CertKind::Row { round, kind } => {
            w.byte(4)?;
            w.u64(round)?;
            w.byte(kind as u8)?;
        }
    }
    w.usize(h.count)?;
    w.put(&h.digest)
}
fn read_region_header(r: &mut Reader<'_>, limits: Limits) -> Result<RegionHeader> {
    let tag = r.byte()?;
    let round = r.u64()?;
    let kind = match tag {
        0 => CertKind::Polka {
            round,
            id: r.array()?,
        },
        1 => CertKind::Decision {
            round,
            id: r.array()?,
        },
        2 => CertKind::Timeout { round },
        3 => CertKind::Jump { round },
        4 => CertKind::Row {
            round,
            kind: read_vote_kind(r)?,
        },
        _ => return Err(Error::Corrupt),
    };
    Ok(RegionHeader {
        kind,
        count: r.usize(limits.max_members)?,
        digest: r.array()?,
    })
}

fn encode(m: &MemoryStore, limits: Limits, image: &mut [u8]) -> Result<()> {
    if image.len() != image_size(limits)? {
        return Err(Error::Corrupt);
    }
    image.fill(0);
    let checksum_at = image.len() - CHECKSUM;
    let mut w = Writer {
        bytes: &mut image[..checksum_at],
        at: 0,
    };
    w.put(MAGIC)?;
    w.put(&VERSION.to_le_bytes())?;
    w.usize(limits.max_members)?;
    w.usize(limits.max_body)?;
    w.u64(0)?; // Filled with the actual bounded payload length below.
    for bank in 0..2 {
        w.flag(m.sealed[bank])?;
        w.optional(&m.configs[bank], |w, c| {
            if c.members.len() > limits.max_members || c.max_body > limits.max_body {
                return Err(Error::Corrupt);
            }
            w.put(&c.domain)?;
            w.u64(c.total)?;
            w.usize(c.max_body)?;
            w.u64(c.rho)?;
            write_timeouts(w, c.timeouts)?;
            w.flag(c.crosslink)?;
            w.put(&c.hash)?;
            w.usize(c.members.len())?;
            for member in &c.members {
                w.put(&member.public_key)?;
                w.u64(member.weight)?;
            }
            Ok(())
        })?;
    }
    w.optional(&m.descriptor, write_descriptor)?;
    for slot in 0..3 {
        if m.bodies[slot].len() > limits.max_body {
            return Err(Error::Corrupt);
        }
        w.usize(m.bodies[slot].len())?;
        w.put(&m.bodies[slot])?;
        w.optional(&m.headers[slot], write_header)?;
    }
    for region in 0..4 {
        if m.regions[region].len() > limits.max_members {
            return Err(Error::Corrupt);
        }
        w.usize(m.regions[region].len())?;
        for vote in &m.regions[region] {
            w.optional(vote, write_vote)?;
        }
        w.optional(&m.region_headers[region], write_region_header)?;
    }
    for own in &m.own {
        w.optional(own, write_vote)?;
    }
    w.optional(&m.own_header, write_header)?;
    let payload = u64::try_from(w.at - PREFIX).map_err(|_| Error::Corrupt)?;
    image[PREFIX - 8..PREFIX].copy_from_slice(&payload.to_le_bytes());
    let checksum = blake3::hash(&image[..checksum_at]);
    image[checksum_at..].copy_from_slice(checksum.as_bytes());
    Ok(())
}

fn decode(image: &[u8], limits: Limits) -> Result<MemoryStore> {
    if image.len() != image_size(limits)? {
        return Err(Error::Corrupt);
    }
    let checksum_at = image.len() - CHECKSUM;
    if blake3::hash(&image[..checksum_at]).as_bytes() != &image[checksum_at..] {
        return Err(Error::Corrupt);
    }
    let mut prefix = Reader {
        bytes: &image[..PREFIX],
        at: 0,
    };
    if prefix.take(8)? != MAGIC
        || u32::from_le_bytes(prefix.array()?) != VERSION
        || prefix.usize(limits.max_members)? != limits.max_members
        || prefix.usize(limits.max_body)? != limits.max_body
    {
        return Err(Error::Corrupt);
    }
    let payload = prefix.usize(checksum_at - PREFIX)?;
    let end = PREFIX + payload;
    if image[end..checksum_at].iter().any(|b| *b != 0) {
        return Err(Error::Corrupt);
    }
    let mut r = Reader {
        bytes: &image[PREFIX..end],
        at: 0,
    };
    let mut m = MemoryStore::default();
    for bank in 0..2 {
        m.sealed[bank] = r.flag()?;
        m.configs[bank] = r.optional(|r| {
            let domain = r.array()?;
            let total = r.u64()?;
            let max_body = r.usize(limits.max_body)?;
            let rho = r.u64()?;
            let timeouts = read_timeouts(r)?;
            let crosslink = r.flag()?;
            let hash = r.array()?;
            let count = r.usize(limits.max_members)?;
            if count == 0 || max_body == 0 || count > r.remaining() / MEMBER_BYTES {
                return Err(Error::Corrupt);
            }
            let mut members = capacity(count)?;
            for _ in 0..count {
                members.push(Member {
                    public_key: r.array()?,
                    weight: r.u64()?,
                });
            }
            // Unsealed banks may legitimately contain zero/unfinished roster rows
            // after any acknowledged staging write. Do not treat them as authority.
            Ok(Config {
                domain,
                members,
                total,
                max_body,
                rho,
                timeouts,
                hash,
                crosslink,
            })
        })?;
        if m.sealed[bank] {
            let c = m.configs[bank].as_ref().ok_or(Error::Corrupt)?;
            let rebuilt = if c.crosslink {
                Config::crosslink(c.domain, c.members.clone(), c.max_body, c.rho, c.timeouts)
            } else {
                Config::new(c.domain, c.members.clone(), c.max_body, c.rho, c.timeouts)
            }
            .map_err(|_| Error::Corrupt)?;
            if rebuilt != *c {
                return Err(Error::Corrupt);
            }
        }
    }
    m.descriptor = r.optional(read_descriptor)?;
    if m.descriptor.is_some() && m.config().is_none() {
        return Err(Error::Corrupt);
    }
    for slot in 0..3 {
        let size = r.usize(limits.max_body)?;
        let bytes = r.take(size)?;
        let mut body = capacity(size)?;
        body.extend_from_slice(bytes);
        m.bodies[slot] = body;
        m.headers[slot] = r.optional(read_header)?;
    }
    for region in 0..4 {
        let count = r.usize(limits.max_members)?;
        if count > r.remaining() {
            return Err(Error::Corrupt);
        }
        let mut votes = capacity(count)?;
        for _ in 0..count {
            votes.push(r.optional(read_vote)?);
        }
        m.regions[region] = votes;
        m.region_headers[region] = r.optional(|r| read_region_header(r, limits))?;
    }
    for own in &mut m.own {
        *own = r.optional(read_vote)?;
    }
    m.own_header = r.optional(read_header)?;
    if r.remaining() != 0 {
        return Err(Error::Corrupt);
    }
    Ok(m)
}
