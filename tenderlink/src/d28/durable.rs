//! Bounded durable D28 workspace over one [`AtomicBlockStore`] image.
//!
//! `open` preserves eager durability: every successful mutation commits the image.
//! `open_buffered` explicitly stages mutations in the same bounded working copy;
//! only `flush` / `flush_publication` commit it. The engine fences signing and
//! publication, while elapsed countdown/grid progress can wait for a checkpoint.
//! No I/O happens on drop. A mutation, encoding or replacement failure poisons
//! the adapter. Drop the engine, recover its backend with `into_backend`, reopen,
//! and use `Engine::recover_with_emission`. Never resume the newer working copy.
//!
//! The codec is independent of Rust layout. There are two configuration banks,
//! three bodies, four certificate regions, and constant-sized signing/publication
//! records. The configured bounds are immutable across roster changes. One image
//! buffer is reused; backend copies and the engine's volatile workspace are separate.
use super::metrics::AtomicWallTime;
pub use super::metrics::WallTimeSnapshot;
use super::*;
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

/// Optional persistence observations; fixed-size and cumulative. No reads of the
/// monotonic clock are added unless attached with `DurableStore::with_metrics`.
#[derive(Debug, Default)]
pub struct Metrics {
    image_encode: AtomicWallTime,
    image_checksum: AtomicWallTime,
    backend_replace: AtomicWallTime,
    body_bytes_copied: AtomicU64,
    body_bytes_hashed: AtomicU64,
    metadata_bytes_hashed: AtomicU64,
    body_bytes_zeroed: AtomicU64,
    metadata_bytes_zeroed: AtomicU64,
    body_slots_reused: AtomicU64,
}

/// Counts attempted phases, including errors. Clean/poisoned fences do no work
/// and add no samples. Encode includes checksum; checksum is a subset, NOT an
/// additional cost. Backend replacement excludes encode and includes any backend
/// blocking/delay. These are wall nanoseconds, never process/thread CPU time.
/// `image_checksum` has one sample per encoding attempt that hashes anything;
/// its duration sums only body-digest and metadata-checksum hashing, not copying.
/// Work counters count actual encoder bytes/slots, including failed attempts,
/// not staging copies, load validation, allocation initialization or backend I/O.
/// In particular they do NOT imply less physical replacement traffic.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MetricsSnapshot {
    pub image_encode: WallTimeSnapshot,
    pub image_checksum: WallTimeSnapshot,
    pub backend_replace: WallTimeSnapshot,
    pub body_bytes_copied: u64,
    pub body_bytes_hashed: u64,
    pub metadata_bytes_hashed: u64,
    pub body_bytes_zeroed: u64,
    pub metadata_bytes_zeroed: u64,
    pub body_slots_reused: u64,
}
impl Metrics {
    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            image_encode: self.image_encode.snapshot(),
            image_checksum: self.image_checksum.snapshot(),
            backend_replace: self.backend_replace.snapshot(),
            body_bytes_copied: self.body_bytes_copied.load(Ordering::Relaxed),
            body_bytes_hashed: self.body_bytes_hashed.load(Ordering::Relaxed),
            metadata_bytes_hashed: self.metadata_bytes_hashed.load(Ordering::Relaxed),
            body_bytes_zeroed: self.body_bytes_zeroed.load(Ordering::Relaxed),
            metadata_bytes_zeroed: self.metadata_bytes_zeroed.load(Ordering::Relaxed),
            body_slots_reused: self.body_slots_reused.load(Ordering::Relaxed),
        }
    }
}

const V1_MAGIC: &[u8; 8] = b"D28IMG01";
const MAGIC: &[u8; 8] = b"D28IMG02";
const VERSION: u32 = 2;
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

/// The v2 image's fixed physical regions: metadata (including its checksum),
/// followed by three full `max_body` body regions (including zero padding).
/// These ordered, adjacent regions cover exactly [`image_size`]. Bounds and
/// arithmetic are checked identically to `image_size`; no allocation is needed.
/// Version 1 uses a different layout and must be replaced in full on conversion.
pub fn image_regions(limits: Limits) -> Result<[std::ops::Range<usize>; 4]> {
    let size = image_size(limits)?;
    let metadata = size - 3 * limits.max_body;
    Ok([
        0..metadata,
        metadata..metadata + limits.max_body,
        metadata + limits.max_body..metadata + 2 * limits.max_body,
        metadata + 2 * limits.max_body..size,
    ])
}

/// Single-owner durable adapter. It is intentionally neither `Clone` nor a
/// source of a mutable backend reference: replacement errors require reopening.
pub struct DurableStore<B: AtomicBlockStore> {
    backend: B,
    limits: Limits,
    memory: MemoryStore,
    image: Vec<u8>,
    cache: ImageCache,
    buffered: bool,
    dirty: bool,
    publication_dirty: bool,
    poisoned: bool,
    metrics: Option<Arc<Metrics>>,
}

impl<B: AtomicBlockStore> DurableStore<B> {
    /// Load a complete committed image, or attach genuinely uninitialized storage.
    /// Bad size, version, padding, lengths, checksum or records fail closed. A
    /// corrupt image is never interpreted as permission to bootstrap fresh keys.
    /// Every successful mutation is durable before returning (the eager API).
    pub fn open(backend: B, limits: Limits) -> Result<Self> {
        Self::open_mode(backend, limits, false)
    }

    /// Explicit staged-write API, using the same image and bounded workspace as
    /// `open`. Successful mutations change the working copy; only barriers make
    /// them durable. Use with `Engine`, which fences signing/publication, and
    /// periodically call `Engine::checkpoint` to bound lost countdown progress.
    /// Dropping the store or taking its backend discards unflushed changes.
    pub fn open_buffered(backend: B, limits: Limits) -> Result<Self> {
        Self::open_mode(backend, limits, true)
    }

    fn open_mode(mut backend: B, limits: Limits, buffered: bool) -> Result<Self> {
        let size = image_size(limits)?;
        if backend.block_size() != size {
            return Err(Error::Config);
        }
        let mut image = capacity(size)?;
        image.resize(size, 0);
        let (memory, cache) = if backend.load(&mut image)? {
            decode_cached(&image, limits)?
        } else {
            (MemoryStore::default(), ImageCache::default())
        };
        Ok(Self {
            backend,
            limits,
            memory,
            image,
            cache,
            buffered,
            dirty: false,
            publication_dirty: false,
            poisoned: false,
            metrics: None,
        })
    }

    /// Observe future persistence attempts only. Opening/loading/recovery decoding
    /// is not measured. Sharing the handle aggregates stores without retaining
    /// any image, height or message history.
    pub fn with_metrics(mut self, metrics: Arc<Metrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    pub fn limits(&self) -> Limits {
        self.limits
    }

    pub fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// Discard every cached byte, including unflushed writes, and return the
    /// backend for a fresh load. This never performs I/O.
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

    fn mutation_result(&mut self, result: Result<()>) -> Result<()> {
        if result.is_err() {
            self.poisoned = true;
            // Even an applied-then-error replacement, or a non-Storage backend
            // error, must deactivate the core before any further sign/callback.
            Err(Error::Storage)
        } else {
            Ok(())
        }
    }

    fn update_metadata(
        &mut self,
        mutate: impl FnOnce(&mut MemoryStore) -> Result<()>,
    ) -> Result<()> {
        self.stage(mutate)?;
        self.publication_dirty = true;
        if !self.buffered {
            self.flush()?;
        }
        Ok(())
    }

    #[cfg(test)]
    fn update(&mut self, mutate: impl FnOnce(&mut MemoryStore) -> Result<()>) -> Result<()> {
        self.cache.bodies.iter_mut().for_each(|b| b.dirty = true);
        self.update_metadata(mutate)
    }

    fn stage(&mut self, mutate: impl FnOnce(&mut MemoryStore) -> Result<()>) -> Result<()> {
        self.ready()?;
        // The reference fault counter is not durable state and must not grow.
        self.memory.writes = 0;
        let lengths = self.memory.bodies.each_ref().map(Vec::len);
        let result = mutate(&mut self.memory);
        self.memory.writes = 0;
        // Descriptor publication can evict all physical bodies on a height change.
        // Track lengths across every mutation, including implicit clears. Explicit
        // body writes/copies also invalidate before staging (even at equal length).
        for (slot, before) in lengths.into_iter().enumerate() {
            if self.memory.bodies[slot].len() != before {
                self.cache.bodies[slot].dirty = true;
            }
        }
        self.mutation_result(result)?;
        self.dirty = true;
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
    fn flush(&mut self) -> Result<()> {
        self.ready()?;
        if self.dirty {
            let result = (|| {
                // Snapshot dirt before encoding updates the cache. Metadata is
                // always rewritten; first use and v1 conversion select all four
                // regions. Keep the list bounded without another allocation.
                let layout = image_regions(self.limits)?;
                let mut changed: [_; 4] = std::array::from_fn(|_| 0..0);
                changed[0] = layout[0].clone();
                let mut count = 1;
                for slot in 0..3 {
                    if self.cache.bodies[slot]
                        .needs_update(self.cache.canonical, self.memory.bodies[slot].len())
                    {
                        changed[count] = layout[slot + 1].clone();
                        count += 1;
                    }
                }
                encode_observed(
                    &self.memory,
                    self.limits,
                    &mut self.image,
                    &mut self.cache,
                    self.metrics.as_deref(),
                )?;
                let _timer = self.metrics.as_ref().map(|m| m.backend_replace.start());
                self.backend.replace_regions(&self.image, &changed[..count])
            })();
            self.mutation_result(result)?;
            self.dirty = false;
            self.publication_dirty = false;
        }
        Ok(())
    }
    fn flush_publication(&mut self) -> Result<()> {
        self.ready()?;
        if self.publication_dirty {
            self.flush()?;
        }
        Ok(())
    }
    fn write_progress_descriptor(&mut self, d: &Descriptor) -> Result<()> {
        if !self.buffered {
            return self.write_descriptor(d);
        }
        self.ready()?;
        // Normalize only explicitly permitted fields, then compare the complete
        // descriptor. New safety/authority fields are therefore denied by default.
        let valid = self.memory.descriptor().is_some_and(|old| {
            let mut normalized = d.clone();
            let countdowns = d.height_remaining <= old.height_remaining
                && d.timers.iter().zip(old.timers).all(|(new, old)| {
                    new.remaining <= old.remaining && (old.active || new.remaining == old.remaining)
                });
            normalized.height_remaining = old.height_remaining;
            for (new, old) in normalized.timers.iter_mut().zip(old.timers) {
                new.remaining = old.remaining;
            }
            normalized.grid_remaining = old.grid_remaining;
            countdowns
                && self.config().is_some_and(|c| d.grid_remaining <= c.rho)
                && normalized == *old
        });
        if !valid {
            return self.mutation_result(Err(Error::Corrupt));
        }
        // Do not clear publication_dirty: earlier staged safety changes still
        // require a publication fence even if followed by countdown progress.
        self.stage(|m| m.write_descriptor(d))
    }
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
        self.update_metadata(|m| m.begin_config(bank, meta))
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
        self.update_metadata(|m| m.write_config_member(bank, row, member))
    }
    fn seal_config_bank(&mut self, bank: usize, hash: Id) -> Result<()> {
        self.index(bank, 2)?;
        self.update_metadata(|m| m.seal_config_bank(bank, hash))
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
        // A repeated validity callback must not turn countdown-only dirt into
        // publication dirt. Preserve every earlier dirty bit on this no-op.
        if self.buffered && self.memory.descriptor() == Some(d) {
            return Ok(());
        }
        self.update_metadata(|m| m.write_descriptor(d))
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
        self.cache.bodies[slot].dirty = true;
        let result = reserve_body(&mut self.memory.bodies[slot], body.len());
        self.mutation_result(result)?;
        self.update_metadata(|m| m.write_body(slot, body))
    }
    fn copy_body(&mut self, from: usize, to: usize) -> Result<()> {
        self.index(from, 3)?;
        self.index(to, 3)?;
        if from == to {
            return Err(Error::Corrupt);
        }
        // A staged source may have a stale cached digest, including after a
        // transitive copy. Rehash the destination's current bytes at the fence.
        self.cache.bodies[to].dirty = true;
        let size = self.memory.bodies[from].len();
        let result = reserve_body(&mut self.memory.bodies[to], size);
        self.mutation_result(result)?;
        self.update_metadata(|m| m.copy_body(from, to))
    }
    fn write_header(&mut self, slot: usize, header: Header) -> Result<()> {
        self.index(slot, 3)?;
        self.update_metadata(|m| m.write_header(slot, header))
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
        self.update_metadata(|m| m.write_region_row(region, row, vote))
    }
    fn write_region_header(&mut self, region: usize, header: RegionHeader) -> Result<()> {
        self.index(region, 4)?;
        if header.count > self.limits.max_members {
            return Err(Error::Corrupt);
        }
        self.update_metadata(|m| m.write_region_header(region, header))
    }
    fn own(&self, kind: VoteKind) -> Option<&Vote> {
        self.memory.own(kind)
    }
    fn write_own(&mut self, kind: VoteKind, vote: Vote) -> Result<()> {
        self.update_metadata(|m| m.write_own(kind, vote))
    }
    fn own_header(&self) -> Option<&Header> {
        self.memory.own_header()
    }
    fn write_own_header(&mut self, header: Header) -> Result<()> {
        self.update_metadata(|m| m.write_own_header(header))
    }
}

fn reserve_body(body: &mut Vec<u8>, size: usize) -> Result<()> {
    body.try_reserve_exact(size.saturating_sub(body.len()))
        .map_err(|_| Error::Exhausted)
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

// V2 keeps the exact deployment image size. The first image_size - 3*Bmax
// bytes hold the v1 metadata grammar, except each body is length + BLAKE3 digest
// (not inline bytes). The final 32 metadata bytes checksum the entire preceding
// metadata area, including magic/version, bounds, payload length and zero tail.
// Three fixed Bmax body regions follow it. Their unused tails must be zero.
// Layout and lengths are thus bound by the metadata checksum. These checksums
// detect corruption, not adversarial rewriting of both data and commitments.
#[derive(Clone, Copy, Debug, Default)]
struct BodyCache {
    len: usize,
    digest: Id,
    dirty: bool,
}
impl BodyCache {
    fn needs_update(&self, canonical: bool, len: usize) -> bool {
        !canonical || self.dirty || self.len != len
    }
}
#[derive(Debug, Default)]
struct ImageCache {
    // True only after validation of an entire v2 image or successful encoding.
    canonical: bool,
    bodies: [BodyCache; 3],
}
fn metadata_size(limits: Limits) -> Result<usize> {
    Ok(image_size(limits)? - 3 * limits.max_body)
}
fn add_work(counter: &AtomicU64, bytes: usize) {
    let amount = u64::try_from(bytes).unwrap_or(u64::MAX);
    let _ = counter.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |old| {
        Some(old.saturating_add(amount))
    });
}
struct ChecksumWork<'a> {
    metrics: Option<&'a Metrics>,
    elapsed: Duration,
    used: bool,
}
impl ChecksumWork<'_> {
    fn hash(&mut self, bytes: &[u8], body: bool) -> Id {
        let start = self.metrics.map(|_| Instant::now());
        let digest = *blake3::hash(bytes).as_bytes();
        if let (Some(start), Some(metrics)) = (start, self.metrics) {
            self.elapsed = self.elapsed.saturating_add(start.elapsed());
            add_work(
                if body {
                    &metrics.body_bytes_hashed
                } else {
                    &metrics.metadata_bytes_hashed
                },
                bytes.len(),
            );
        }
        self.used = true;
        digest
    }
}
impl Drop for ChecksumWork<'_> {
    fn drop(&mut self) {
        if self.used {
            if let Some(metrics) = self.metrics {
                metrics.image_checksum.record(self.elapsed);
            }
        }
    }
}
#[cfg(test)]
fn encode(m: &MemoryStore, limits: Limits, image: &mut [u8]) -> Result<()> {
    encode_observed(m, limits, image, &mut ImageCache::default(), None)
}
fn encode_observed(
    m: &MemoryStore,
    limits: Limits,
    image: &mut [u8],
    cache: &mut ImageCache,
    metrics: Option<&Metrics>,
) -> Result<()> {
    let _timer = metrics.map(|m| m.image_encode.start());
    if image.len() != image_size(limits)?
        || m.bodies.iter().any(|b| b.len() > limits.max_body)
        || m.regions.iter().any(|r| r.len() > limits.max_members)
        || m.configs
            .iter()
            .flatten()
            .any(|c| c.members.len() > limits.max_members || c.max_body > limits.max_body)
    {
        return Err(Error::Corrupt);
    }
    let metadata_end = metadata_size(limits)?;
    let checksum_at = metadata_end - CHECKSUM;
    let (metadata, bodies) = image.split_at_mut(metadata_end);
    let mut checksums = ChecksumWork {
        metrics,
        elapsed: Duration::ZERO,
        used: false,
    };
    for (slot, bytes) in bodies.chunks_exact_mut(limits.max_body).enumerate() {
        let body = &m.bodies[slot];
        let cached = &mut cache.bodies[slot];
        if cached.needs_update(cache.canonical, body.len()) {
            bytes[..body.len()].copy_from_slice(body);
            // A canonical image already has zero bytes beyond the old length.
            // Migration/first use must scrub the entire unused region once.
            let zero_end = if cache.canonical {
                cached.len.max(body.len())
            } else {
                bytes.len()
            };
            bytes[body.len()..zero_end].fill(0);
            if let Some(metrics) = metrics {
                add_work(&metrics.body_bytes_copied, body.len());
                add_work(&metrics.body_bytes_zeroed, zero_end - body.len());
            }
            cached.digest = checksums.hash(body, true);
            cached.len = body.len();
            cached.dirty = false;
        } else if let Some(metrics) = metrics {
            add_work(&metrics.body_slots_reused, 1);
        }
    }
    let mut w = Writer {
        bytes: &mut metadata[..checksum_at],
        at: 0,
    };
    w.put(MAGIC)?;
    w.put(&VERSION.to_le_bytes())?;
    w.usize(limits.max_members)?;
    w.usize(limits.max_body)?;
    w.u64(0)?;
    for bank in 0..2 {
        w.flag(m.sealed[bank])?;
        w.optional(&m.configs[bank], |w, c| {
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
    for (slot, cached) in cache.bodies.iter().enumerate() {
        w.usize(cached.len)?;
        w.put(&cached.digest)?;
        w.optional(&m.headers[slot], write_header)?;
    }
    for region in 0..4 {
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
    w.bytes[w.at..].fill(0);
    if let Some(metrics) = metrics {
        add_work(&metrics.metadata_bytes_zeroed, w.bytes.len() - w.at);
    }
    let payload = u64::try_from(w.at - PREFIX).map_err(|_| Error::Corrupt)?;
    metadata[PREFIX - 8..PREFIX].copy_from_slice(&payload.to_le_bytes());
    let digest = checksums.hash(&metadata[..checksum_at], false);
    metadata[checksum_at..].copy_from_slice(&digest);
    cache.canonical = true;
    Ok(())
}

#[cfg(test)]
fn encode_v1(m: &MemoryStore, limits: Limits, image: &mut [u8]) -> Result<()> {
    if image.len() != image_size(limits)? {
        return Err(Error::Corrupt);
    }
    let checksum_at = image.len() - CHECKSUM;
    let mut w = Writer {
        bytes: &mut image[..checksum_at],
        at: 0,
    };
    w.put(V1_MAGIC)?;
    w.put(&1u32.to_le_bytes())?;
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
    // The writer overwrote the occupied prefix; clear only the unused tail.
    w.bytes[w.at..].fill(0);
    let payload = u64::try_from(w.at - PREFIX).map_err(|_| Error::Corrupt)?;
    image[PREFIX - 8..PREFIX].copy_from_slice(&payload.to_le_bytes());
    let checksum = { blake3::hash(&image[..checksum_at]) };
    image[checksum_at..].copy_from_slice(checksum.as_bytes());
    Ok(())
}

#[cfg(test)]
fn decode(image: &[u8], limits: Limits) -> Result<MemoryStore> {
    decode_cached(image, limits).map(|(memory, _)| memory)
}
fn decode_cached(image: &[u8], limits: Limits) -> Result<(MemoryStore, ImageCache)> {
    if image.len() != image_size(limits)? {
        return Err(Error::Corrupt);
    }
    let version = match (
        &image[..8],
        u32::from_le_bytes(image[8..12].try_into().map_err(|_| Error::Corrupt)?),
    ) {
        (magic, 1) if magic == V1_MAGIC => 1,
        (magic, VERSION) if magic == MAGIC => VERSION,
        _ => return Err(Error::Corrupt),
    };
    let metadata_end = if version == 1 {
        image.len()
    } else {
        metadata_size(limits)?
    };
    let checksum_at = metadata_end - CHECKSUM;
    if blake3::hash(&image[..checksum_at]).as_bytes() != &image[checksum_at..metadata_end] {
        return Err(Error::Corrupt);
    }
    let mut prefix = Reader {
        bytes: &image[..PREFIX],
        at: 0,
    };
    prefix.take(12)?;
    if prefix.usize(limits.max_members)? != limits.max_members
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
    let mut cache = ImageCache::default();
    for slot in 0..3 {
        let size = r.usize(limits.max_body)?;
        let bytes = if version == 1 {
            r.take(size)?
        } else {
            let digest: Id = r.array()?;
            let start = metadata_end + slot * limits.max_body;
            let region = &image[start..start + limits.max_body];
            let bytes = &region[..size];
            // Validate every physical body, even if the descriptor does not
            // reference it. Neither saved nor cached digests are trusted on load.
            if blake3::hash(bytes).as_bytes() != &digest || region[size..].iter().any(|b| *b != 0) {
                return Err(Error::Corrupt);
            }
            cache.bodies[slot] = BodyCache {
                len: size,
                digest,
                dirty: false,
            };
            bytes
        };
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
    cache.canonical = version == VERSION;
    Ok((m, cache))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_regions_cover_fixed_v2_layout_and_reject_invalid_bounds() {
        for limits in [
            Limits { max_members: 1, max_body: 1 },
            Limits { max_members: 19, max_body: 40 << 20 },
        ] {
            let regions = image_regions(limits).unwrap();
            assert_eq!(regions[0], 0..SCALAR_BYTES + 556 * limits.max_members);
            for slot in 1..4 {
                assert_eq!(regions[slot].len(), limits.max_body);
                assert_eq!(regions[slot].start, regions[slot - 1].end);
            }
            assert_eq!(regions[3].end, image_size(limits).unwrap());
        }
        for limits in [
            Limits { max_members: 0, max_body: 1 },
            Limits { max_members: 1, max_body: 0 },
            Limits { max_members: usize::MAX, max_body: 1 },
            Limits { max_members: 1, max_body: usize::MAX },
            Limits { max_members: 1, max_body: isize::MAX as usize / 3 },
        ] {
            assert_eq!(image_regions(limits), Err(Error::Config));
        }
    }

    // Independent cold encoder: no cache, full-image clear, then serialize all
    // physical state. Preserve the original v1 grammar/differential coverage;
    // v2 is independently written using its fixed-region layout.
    fn encode_original(m: &MemoryStore, limits: Limits, image: &mut [u8]) -> Result<()> {
        encode_reference(m, limits, image, false)
    }
    fn encode_reference(m: &MemoryStore, limits: Limits, image: &mut [u8], v2: bool) -> Result<()> {
        if image.len() != image_size(limits)? {
            return Err(Error::Corrupt);
        }
        image.fill(0);
        let checksum_at = if v2 {
            image.len() - 3 * limits.max_body - CHECKSUM
        } else {
            image.len() - CHECKSUM
        };
        let mut w = Writer {
            bytes: &mut image[..checksum_at],
            at: 0,
        };
        w.put(if v2 { MAGIC } else { V1_MAGIC })?;
        w.put(&(if v2 { VERSION } else { 1u32 }).to_le_bytes())?;
        w.usize(limits.max_members)?;
        w.usize(limits.max_body)?;
        w.u64(0)?;
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
            if v2 {
                w.put(blake3::hash(&m.bodies[slot]).as_bytes())?;
            } else {
                w.put(&m.bodies[slot])?;
            }
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
        image[checksum_at..checksum_at + CHECKSUM].copy_from_slice(checksum.as_bytes());
        if v2 {
            for slot in 0..3 {
                let start = checksum_at + CHECKSUM + slot * limits.max_body;
                image[start..start + m.bodies[slot].len()].copy_from_slice(&m.bodies[slot]);
            }
        }
        Ok(())
    }

    fn config(count: usize, max_body: usize, crosslink: bool) -> Config {
        let members = (0..count)
            .map(|i| Member {
                public_key: VerificationKeyBytes::from(&SigningKey::from([i as u8 + 1; 32])).into(),
                weight: i as u64 + 1,
            })
            .collect();
        Config::build(
            [42; 32],
            members,
            max_body,
            10,
            Timeouts {
                propose: 30,
                prevote: 5,
                precommit: 10,
                inc_propose: 3,
                inc_prevote: 1,
                inc_precommit: 2,
            },
            crosslink,
        )
        .unwrap()
    }

    // Exercise every physical role, independently of protocol reachability.
    // Signatures and reference digests are opaque bytes to this codec.
    fn populated(limits: Limits) -> MemoryStore {
        let c = config(limits.max_members, limits.max_body, false);
        let mut m = MemoryStore {
            configs: [
                Some(c.clone()),
                Some(config(limits.max_members, limits.max_body, true)),
            ],
            sealed: [true; 2],
            ..MemoryStore::default()
        };
        for slot in 0..3 {
            m.bodies[slot] = vec![slot as u8 + 1; limits.max_body];
            m.headers[slot] = Some(Header {
                content: ProposalContent {
                    height: 9,
                    round: 7,
                    valid_round: Some(6),
                    id: [slot as u8 + 11; 32],
                    signer: 0,
                },
                signature: [slot as u8 + 21; 64],
            });
        }
        m.own = [VoteKind::Enter, VoteKind::Prevote, VoteKind::Precommit].map(|kind| {
            Some(Vote {
                content: VoteContent {
                    height: 9,
                    round: 7,
                    kind,
                    id: (kind != VoteKind::Enter).then_some([11; 32]),
                    signer: 0,
                },
                signature: [kind as u8 + 31; 64],
            })
        });
        m.own_header = m.headers[0];
        let kinds = [
            CertKind::Polka {
                round: 7,
                id: [11; 32],
            },
            CertKind::Decision {
                round: 7,
                id: [12; 32],
            },
            CertKind::Timeout { round: 7 },
            CertKind::Jump { round: 8 },
        ];
        for (region, kind) in kinds.into_iter().enumerate() {
            m.regions[region] = vec![m.own[region % 3]; limits.max_members];
            m.region_headers[region] = Some(RegionHeader {
                kind,
                count: limits.max_members,
                digest: [region as u8 + 41; 32],
            });
        }
        let slot = SlotRef {
            index: 2,
            header: [51; 32],
        };
        let region = RegionRef {
            index: 3,
            digest: [52; 32],
        };
        let certified = Certified {
            proposal: slot,
            certificate: region,
        };
        m.descriptor = Some(Descriptor {
            height: 9,
            config: c.hash,
            round: 7,
            step: Step::Precommit,
            locked_round: Some(6),
            locked_id: Some([11; 32]),
            valid_round: Some(6),
            slot: Some(slot),
            justified: true,
            vc: Some(certified),
            entry: Some(region),
            pending: Some(certified),
            statuses: [Status::Indeterminate, Status::Valid, Status::Invalid],
            body_generations: [1, 2, 3],
            next_generation: 4,
            timers: std::array::from_fn(|i| Timer {
                active: i % 2 == 0,
                remaining: 100 + i as u64,
                height: 9,
                round: 7,
            }),
            height_remaining: 101,
            grid_remaining: 102,
            intents: m.own.map(|vote| vote.map(|v| v.content)),
            proposal_intent: Some(ProposalIntent {
                content: m.own_header.unwrap().content,
                slot: 0,
            }),
            proposal_published: true,
        });
        m
    }

    fn assert_original_image(m: &MemoryStore, limits: Limits, image: &mut [u8]) -> usize {
        let mut original = vec![0xa5; image_size(limits).unwrap()];
        encode_original(m, limits, &mut original).unwrap();
        encode_v1(m, limits, image).unwrap();
        assert!(
            image == original,
            "image differs from the original full-clear encoder"
        );
        let end = PREFIX
            + usize::try_from(u64::from_le_bytes(
                image[PREFIX - 8..PREFIX].try_into().unwrap(),
            ))
            .unwrap();
        let checksum_at = image.len() - CHECKSUM;
        assert!(image[end..checksum_at].iter().all(|byte| *byte == 0));
        assert_eq!(
            &image[checksum_at..],
            blake3::hash(&image[..checksum_at]).as_bytes()
        );
        // The independently decoded image must also re-encode identically.
        let reopened = decode(image, limits).unwrap();
        encode_v1(&reopened, limits, &mut original).unwrap();
        assert!(image == original, "reopened image changed");
        end
    }

    #[test]
    fn encode_matches_original_for_empty_and_three_bodies_in_dirty_images() {
        for max_body in [1, 100 * 1024, 10 * 1024 * 1024] {
            let limits = Limits {
                max_members: 4,
                max_body,
            };
            for m in [MemoryStore::default(), populated(limits)] {
                let mut image = vec![0; image_size(limits).unwrap()];
                assert_original_image(&m, limits, &mut image);
                // Every byte (including padding and checksum) starts nonzero.
                for (i, byte) in image.iter_mut().enumerate() {
                    *byte = (i % 255 + 1) as u8;
                }
                assert_original_image(&m, limits, &mut image);
            }
        }
    }

    #[test]
    fn encode_matches_original_through_shrinking_banks_bodies_and_optional_records() {
        let limits = Limits {
            max_members: 4,
            max_body: 100 * 1024,
        };
        let mut image = vec![0xff; image_size(limits).unwrap()];
        let mut m = populated(limits);
        let full_end = assert_original_image(&m, limits, &mut image);

        // Replace a full sealed bank with a smaller, unfinished staging bank.
        let small = config(1, 8, true);
        m.begin_config(
            1,
            ConfigMeta {
                domain: small.domain,
                total: small.total,
                count: small.members.len(),
                max_body: small.max_body,
                rho: small.rho,
                timeouts: small.timeouts,
                crosslink: small.crosslink,
            },
        )
        .unwrap();
        let staged_end = assert_original_image(&m, limits, &mut image);
        assert!(staged_end < full_end);
        m.write_config_member(1, 0, small.members[0].clone())
            .unwrap();
        assert_original_image(&m, limits, &mut image);
        m.seal_config_bank(1, small.hash).unwrap();
        assert_original_image(&m, limits, &mut image);

        m.configs[0] = None;
        m.sealed[0] = false;
        m.bodies[0].truncate(3);
        m.bodies[1].truncate(1);
        m.bodies[2].clear();
        m.headers[0].as_mut().unwrap().content.valid_round = None;
        m.headers[1..].fill(None);
        m.own[0] = None;
        m.own[1].as_mut().unwrap().content.id = None;
        m.own[2] = None;
        m.own_header = None;
        m.regions = [vec![m.own[1]], vec![None], Vec::new(), Vec::new()];
        m.region_headers = [
            Some(RegionHeader {
                kind: CertKind::Row {
                    round: 7,
                    kind: VoteKind::Prevote,
                },
                count: 1,
                digest: [61; 32],
            }),
            None,
            None,
            None,
        ];
        let d = m.descriptor.as_mut().unwrap();
        d.config = small.hash;
        d.locked_round = None;
        d.locked_id = None;
        d.valid_round = None;
        d.slot = None;
        d.justified = false;
        d.vc = None;
        d.entry = None;
        d.pending = None;
        d.intents = m.own.map(|vote| vote.map(|v| v.content));
        d.proposal_intent = None;
        d.proposal_published = false;
        let sparse_end = assert_original_image(&m, limits, &mut image);
        assert!(sparse_end < staged_end);
        assert!(image[sparse_end..full_end].iter().all(|byte| *byte == 0));

        // Remove the descriptor, remaining body/header/vote and both banks.
        // Then grow again without ever reinitializing the reused image buffer.
        let empty_end = assert_original_image(&MemoryStore::default(), limits, &mut image);
        assert_eq!(empty_end, PREFIX + 72);
        assert!(image[empty_end..sparse_end].iter().all(|byte| *byte == 0));
        assert_eq!(
            assert_original_image(&populated(limits), limits, &mut image),
            full_end
        );
    }

    fn assert_cached_image(store: &mut DurableStore<Block>) {
        store.flush().unwrap();
        let mut cold = vec![0x97; image_size(store.limits).unwrap()];
        encode_reference(&store.memory, store.limits, &mut cold, true).unwrap();
        assert_eq!(
            store.image, cold,
            "cached v2 differs from independent cold encoder"
        );
        assert_eq!(store.backend.image, cold);
        // Compare every private field through the independent canonical v1
        // encoding, not merely bodies or references visible to the protocol.
        let reopened = decode(&cold, store.limits).unwrap();
        let mut expected = vec![0; cold.len()];
        encode_original(&store.memory, store.limits, &mut expected).unwrap();
        encode_original(&reopened, store.limits, &mut cold).unwrap();
        assert_eq!(cold, expected, "v2 recovery changed physical state");
    }

    #[test]
    fn v2_cold_roundtrip_matches_independent_encoder_at_tiny_and_full_bounds() {
        for (max_members, max_body) in [(1, 1), (4, 128), (7, 100 * 1024), (4, 10 * 1024 * 1024)] {
            let limits = Limits {
                max_members,
                max_body,
            };
            for mut m in [MemoryStore::default(), populated(limits)] {
                let mut image = vec![0xa7; image_size(limits).unwrap()];
                let mut expected = vec![0xd3; image.len()];
                for state in 0..4 {
                    match state {
                        1 => {
                            m.bodies[0].truncate(max_body / 2);
                            m.bodies[1].clear();
                        }
                        2 => {
                            m.bodies[0] = vec![99; max_body];
                            m.bodies[2].clear();
                        }
                        3 => m = MemoryStore::default(),
                        _ => {}
                    }
                    encode(&m, limits, &mut image).unwrap();
                    encode_reference(&m, limits, &mut expected, true).unwrap();
                    assert_eq!(image, expected);
                    let decoded = decode(&image, limits).unwrap();
                    encode_original(&decoded, limits, &mut image).unwrap();
                    encode_original(&m, limits, &mut expected).unwrap();
                    assert_eq!(image, expected);
                }
            }
        }
    }

    #[test]
    fn v2_cached_matches_independent_cold_after_every_mutation_category() {
        for eager in [false, true] {
            let mut store = buffered();
            store.buffered = !eager;
            let small = config(2, 64, true);
            store
                .begin_config(
                    1,
                    ConfigMeta {
                        domain: small.domain,
                        total: small.total,
                        count: small.members.len(),
                        max_body: small.max_body,
                        rho: small.rho,
                        timeouts: small.timeouts,
                        crosslink: small.crosslink,
                    },
                )
                .unwrap();
            assert_cached_image(&mut store); // legal unsealed/unfinished bank
            for (row, member) in small.members.iter().enumerate() {
                store.write_config_member(1, row, member.clone()).unwrap();
                assert_cached_image(&mut store);
            }
            store.seal_config_bank(1, small.hash).unwrap();
            assert_cached_image(&mut store);
            store.seal_config(&small).unwrap();
            assert_cached_image(&mut store);
            let mut h = *store.header(0).unwrap();
            h.content.round += 1; // legal unreferenced/stale header
            store.write_header(0, h).unwrap();
            assert_cached_image(&mut store);
            for kind in [VoteKind::Enter, VoteKind::Prevote, VoteKind::Precommit] {
                let mut vote = *store.own(kind).unwrap();
                vote.content.round += 1;
                store.write_own(kind, vote).unwrap();
                assert_cached_image(&mut store);
                store.write_region_row(kind.index(), 1, Some(vote)).unwrap();
                assert_cached_image(&mut store);
                store.write_region_row(kind.index(), 1, None).unwrap();
                assert_cached_image(&mut store);
            }
            store.write_own_header(h).unwrap();
            assert_cached_image(&mut store);
            for region in 0..4 {
                store
                    .write_region_header(
                        region,
                        RegionHeader {
                            kind: CertKind::Row {
                                round: 8,
                                kind: VoteKind::Prevote,
                            },
                            count: 1,
                            digest: [region as u8; 32],
                        },
                    )
                    .unwrap();
                assert_cached_image(&mut store);
            }
            let mut d = store.descriptor().unwrap().clone();
            d.height_remaining -= 1;
            store.write_descriptor(&d).unwrap();
            assert_cached_image(&mut store);
            d.height_remaining -= 1;
            d.timers[0].remaining -= 1;
            d.grid_remaining = 0;
            store.write_progress_descriptor(&d).unwrap();
            assert_cached_image(&mut store);
            for body in [
                vec![81; 128],
                vec![82; 128],
                vec![83; 1],
                vec![],
                vec![84; 128],
            ] {
                store.write_body(0, &body).unwrap();
                assert_cached_image(&mut store);
                store.copy_body(0, 1).unwrap();
                assert_cached_image(&mut store);
                store.copy_body(1, 2).unwrap();
                assert_cached_image(&mut store);
            }
            // Descriptor-driven physical eviction must invalidate all body caches.
            d.height += 1;
            d.slot = None;
            d.vc = None;
            d.pending = None;
            d.entry = None;
            store.write_descriptor(&d).unwrap();
            assert_cached_image(&mut store);
            for slot in 0..3 {
                assert!(store.body(slot).is_empty());
                assert!(store.header(slot).is_none());
            }
            store.write_body(2, b"after eviction").unwrap();
            assert_cached_image(&mut store);
            store
                .update(|m| {
                    m.bodies[2][0] ^= 1;
                    Ok(())
                })
                .unwrap();
            assert_cached_image(&mut store); // arbitrary private test mutation
        }
    }

    #[test]
    fn v2_dirty_copy_chains_and_source_rewrites_use_current_bytes_not_cached_digest() {
        let mut store = buffered();
        let old_digest = store.cache.bodies[0].digest;
        store.write_body(0, &[17; 128]).unwrap();
        assert_eq!(store.cache.bodies[0].digest, old_digest);
        store.copy_body(0, 1).unwrap();
        store.copy_body(1, 2).unwrap();
        store.write_body(0, &[18; 128]).unwrap();
        store.copy_body(2, 1).unwrap();
        assert_eq!(store.backend.replacements, 0);
        assert_cached_image(&mut store);
        assert_eq!(store.body(0), &[18; 128]);
        assert_eq!(store.body(1), &[17; 128]);
        assert_eq!(store.body(2), &[17; 128]);
    }

    #[test]
    fn v2_metadata_fences_reuse_body_bytes_and_only_zero_shrunk_spans() {
        let metrics = Arc::new(Metrics::default());
        let mut store = buffered().with_metrics(metrics.clone());
        for slot in 0..3 {
            store.write_body(slot, b"installed body").unwrap();
        }
        store.flush().unwrap();
        let installed = metrics.snapshot();
        assert_eq!(installed.body_bytes_copied, 3 * 14);
        assert_eq!(installed.body_bytes_hashed, 3 * 14);
        assert_eq!(installed.body_bytes_zeroed, 3 * (128 - 14));
        let image_pointer = store.image.as_ptr();
        for _ in 0..20 {
            let mut d = store.descriptor().unwrap().clone();
            d.height_remaining -= 1;
            d.timers[0].remaining -= 1;
            store.write_progress_descriptor(&d).unwrap();
            store.flush().unwrap();
            let vote = *store.own(VoteKind::Prevote).unwrap();
            store.write_region_row(0, 0, Some(vote)).unwrap();
            store.write_own(VoteKind::Prevote, vote).unwrap();
            store.write_header(1, *store.header(1).unwrap()).unwrap();
            store.flush_publication().unwrap();
        }
        let metadata = metrics.snapshot();
        assert_eq!(metadata.body_bytes_copied, installed.body_bytes_copied);
        assert_eq!(metadata.body_bytes_hashed, installed.body_bytes_hashed);
        assert_eq!(metadata.body_bytes_zeroed, installed.body_bytes_zeroed);
        assert_eq!(
            metadata.body_slots_reused - installed.body_slots_reused,
            40 * 3
        );
        assert_eq!(
            metadata.metadata_bytes_hashed - installed.metadata_bytes_hashed,
            40 * (metadata_size(store.limits).unwrap() - CHECKSUM) as u64
        );
        assert_eq!(
            metadata.image_checksum.calls - installed.image_checksum.calls,
            40
        );
        assert!(metadata.image_encode.wall_ns >= metadata.image_checksum.wall_ns);
        assert_eq!(
            store.image.as_ptr(),
            image_pointer,
            "one image allocation is reused"
        );
        store.write_body(0, b"x").unwrap();
        store.flush().unwrap();
        let shrunk = metrics.snapshot();
        assert_eq!(shrunk.body_bytes_zeroed - metadata.body_bytes_zeroed, 13);
        store.write_body(0, b"regrown").unwrap();
        store.flush().unwrap();
        let grown = metrics.snapshot();
        assert_eq!(grown.body_bytes_zeroed, shrunk.body_bytes_zeroed);
        store.write_body(0, b"").unwrap();
        store.flush().unwrap();
        let cleared = metrics.snapshot();
        assert_eq!(cleared.body_bytes_zeroed - grown.body_bytes_zeroed, 7);
        assert_cached_image(&mut store);
    }

    #[test]
    fn v1_migrates_only_at_required_fence_and_failed_migration_recovers_old_or_new() {
        for fail in [None, Some(false), Some(true)] {
            let limits = Limits {
                max_members: 4,
                max_body: 128,
            };
            let mut memory = populated(limits);
            memory.descriptor.as_mut().unwrap().grid_remaining = 10;
            // Packed v1 bytes overlap what becomes v2 body padding.
            memory.bodies[0].truncate(5);
            let mut old = vec![0xa9; image_size(limits).unwrap()];
            encode_original(&memory, limits, &mut old).unwrap();
            let mut store = DurableStore::open_buffered(
                Block {
                    image: old.clone(),
                    replacements: 0,
                    fail,
                },
                limits,
            )
            .unwrap();
            assert!(!store.cache.canonical);
            store.flush().unwrap();
            store.flush_publication().unwrap();
            let mut d = store.descriptor().unwrap().clone();
            d.height_remaining -= 1;
            store.write_progress_descriptor(&d).unwrap();
            store.flush_publication().unwrap();
            assert_eq!(store.backend.replacements, 0);
            assert_eq!(store.backend.image, old);
            let result = store.flush();
            assert_eq!(
                result,
                if fail.is_none() {
                    Ok(())
                } else {
                    Err(Error::Storage)
                }
            );
            assert_eq!(store.is_poisoned(), fail.is_some());
            assert_eq!(store.backend.replacements, 1);
            if fail.is_some() {
                assert_eq!(store.flush(), Err(Error::Storage));
                assert_eq!(store.write_body(0, b"forbidden"), Err(Error::Storage));
            }
            let backend = store.into_backend();
            assert_eq!(backend.replacements, 1);
            let mut reopened = DurableStore::open_buffered(backend, limits).unwrap();
            assert_eq!(reopened.cache.canonical, fail != Some(false));
            assert_eq!(
                &reopened.image[..8],
                if fail == Some(false) { V1_MAGIC } else { MAGIC }
            );
            assert_eq!(reopened.body(0), memory.body(0));
            assert_eq!(
                reopened.descriptor().unwrap().height_remaining,
                if fail == Some(false) {
                    d.height_remaining + 1
                } else {
                    d.height_remaining
                }
            );
            reopened.flush().unwrap();
            assert_eq!(reopened.backend.replacements, 1);
        }
        // Eager opening is also read-only, but its next acknowledged write migrates.
        let limits = Limits {
            max_members: 1,
            max_body: 1,
        };
        let memory = populated(limits);
        let mut image = vec![0; image_size(limits).unwrap()];
        encode_original(&memory, limits, &mut image).unwrap();
        let mut store = DurableStore::open(
            Block {
                image,
                replacements: 0,
                fail: None,
            },
            limits,
        )
        .unwrap();
        assert_eq!(store.backend.replacements, 0);
        store.write_body(0, b"a").unwrap();
        assert_eq!(store.backend.replacements, 1);
        assert_cached_image(&mut store);
    }

    #[test]
    fn v1_remains_strict_with_resealed_corruption_and_unknown_versions() {
        let limits = Limits {
            max_members: 1,
            max_body: 1,
        };
        let mut image = vec![0; image_size(limits).unwrap()];
        encode_original(&MemoryStore::default(), limits, &mut image).unwrap();
        for offset in [0, 8, 12, 20, 28, 36, 37, 40, 41, 68, 111] {
            let mut corrupt = image.clone();
            corrupt[offset] = 0xff;
            let at = corrupt.len() - CHECKSUM;
            let hash = blake3::hash(&corrupt[..at]);
            corrupt[at..].copy_from_slice(hash.as_bytes());
            assert!(
                matches!(decode(&corrupt, limits), Err(Error::Corrupt)),
                "v1 offset {offset}"
            );
        }
        for size in [0, 1, image.len() - 1] {
            assert!(matches!(
                decode(&image[..size], limits),
                Err(Error::Corrupt)
            ));
        }
        image.push(0);
        assert!(matches!(decode(&image, limits), Err(Error::Corrupt)));
    }

    #[test]
    fn v2_uninitialized_dirty_load_is_canonicalized_and_optional_metrics_are_identical() {
        struct EmptyBlock(usize);
        impl AtomicBlockStore for EmptyBlock {
            fn block_size(&self) -> usize {
                self.0
            }
            fn load(&mut self, bytes: &mut [u8]) -> Result<bool> {
                bytes.fill(0xff);
                Ok(false)
            }
            fn replace(&mut self, _: &[u8]) -> Result<()> {
                Ok(())
            }
        }
        let limits = Limits {
            max_members: 1,
            max_body: 128,
        };
        let mut store =
            DurableStore::open_buffered(EmptyBlock(image_size(limits).unwrap()), limits).unwrap();
        let c = config(1, 128, false);
        store.seal_config(&c).unwrap();
        store.write_body(0, b"x").unwrap();
        store.flush().unwrap();
        assert!(store.metrics.is_none());
        let metrics = Metrics::default();
        let mut observed = vec![0x86; store.image.len()];
        encode_observed(
            &store.memory,
            limits,
            &mut observed,
            &mut ImageCache::default(),
            Some(&metrics),
        )
        .unwrap();
        assert_eq!(observed, store.image);
        assert_eq!(metrics.snapshot().body_bytes_copied, 1);
        assert_eq!(metrics.snapshot().body_bytes_hashed, 1);
        assert_eq!(metrics.snapshot().body_bytes_zeroed, 3 * 128 - 1);
        assert_eq!(
            metrics.snapshot().metadata_bytes_hashed,
            (metadata_size(limits).unwrap() - CHECKSUM) as u64
        );
        let decoded = decode(&observed, limits).unwrap();
        assert_eq!(decoded.body(0), b"x");
        // Fixed-size cache: no body/image allocation or history is stored here.
        assert!(!std::mem::needs_drop::<ImageCache>());
        assert!(std::mem::size_of::<ImageCache>() <= 3 * 64);
        let counter = AtomicU64::new(u64::MAX - 1);
        add_work(&counter, 10);
        assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
    }

    struct Block {
        image: Vec<u8>,
        replacements: usize,
        fail: Option<bool>,
    }
    impl AtomicBlockStore for Block {
        fn block_size(&self) -> usize {
            self.image.len()
        }
        fn load(&mut self, block: &mut [u8]) -> Result<bool> {
            block.copy_from_slice(&self.image);
            Ok(true)
        }
        fn replace(&mut self, next: &[u8]) -> Result<()> {
            self.replacements += 1;
            if self.fail != Some(false) {
                self.image.copy_from_slice(next);
            }
            if self.fail.is_some() {
                Err(Error::Invalid)
            } else {
                Ok(())
            }
        }
    }
    fn buffered() -> DurableStore<Block> {
        let limits = Limits {
            max_members: 4,
            max_body: 128,
        };
        let mut m = populated(limits);
        m.descriptor.as_mut().unwrap().grid_remaining = 10;
        let mut image = vec![0; image_size(limits).unwrap()];
        encode(&m, limits, &mut image).unwrap();
        DurableStore::open_buffered(
            Block {
                image,
                replacements: 0,
                fail: None,
            },
            limits,
        )
        .unwrap()
    }

    #[test]
    fn metrics_count_encode_checksum_and_replace_in_order() {
        struct ObservedBlock {
            block: Block,
            metrics: Arc<Metrics>,
        }
        impl AtomicBlockStore for ObservedBlock {
            fn block_size(&self) -> usize {
                self.block.block_size()
            }
            fn load(&mut self, bytes: &mut [u8]) -> Result<bool> {
                self.block.load(bytes)
            }
            fn replace(&mut self, bytes: &[u8]) -> Result<()> {
                let snapshot = self.metrics.snapshot();
                assert_eq!(
                    snapshot.image_encode.calls,
                    self.block.replacements as u64 + 1
                );
                assert_eq!(snapshot.image_checksum.calls, snapshot.image_encode.calls);
                assert_eq!(
                    snapshot.backend_replace.calls,
                    self.block.replacements as u64
                );
                self.block.replace(bytes)
            }
        }
        let unobserved = buffered();
        assert!(unobserved.metrics.is_none());
        let limits = unobserved.limits;
        let metrics = Arc::new(Metrics::default());
        let backend = ObservedBlock {
            block: unobserved.into_backend(),
            metrics: metrics.clone(),
        };
        let mut store = DurableStore::open_buffered(backend, limits)
            .unwrap()
            .with_metrics(metrics.clone());
        assert_eq!(metrics.snapshot(), MetricsSnapshot::default());
        store.write_body(0, b"first").unwrap();
        assert_eq!(
            metrics.snapshot(),
            MetricsSnapshot::default(),
            "staging is not encoding"
        );
        store.flush_publication().unwrap();
        let first = metrics.snapshot();
        assert_eq!(first.image_encode.calls, 1);
        assert_eq!(first.image_checksum.calls, 1);
        assert_eq!(first.backend_replace.calls, 1);
        assert!(first.image_encode.wall_ns >= first.image_checksum.wall_ns);
        store.flush().unwrap();
        store.flush_publication().unwrap();
        assert_eq!(
            metrics.snapshot(),
            first,
            "clean fences do no measured work"
        );
        store.write_body(0, b"second").unwrap();
        store.flush().unwrap();
        let second = metrics.snapshot();
        assert_eq!(second.image_encode.calls - first.image_encode.calls, 1);
        assert_eq!(second.image_checksum.calls - first.image_checksum.calls, 1);
        assert_eq!(
            second.backend_replace.calls - first.backend_replace.calls,
            1
        );
        assert!(second.image_encode.wall_ns >= first.image_encode.wall_ns);
        assert!(second.image_checksum.wall_ns >= first.image_checksum.wall_ns);
        assert!(second.backend_replace.wall_ns >= first.backend_replace.wall_ns);
        store.write_body(0, b"discard").unwrap();
        drop(store);
        assert_eq!(
            metrics.snapshot(),
            second,
            "drop does not persist or measure"
        );
    }

    #[test]
    fn metrics_failed_encode_skips_checksum_and_replace_and_failed_replace_counts() {
        let metrics = Arc::new(Metrics::default());
        let mut store = buffered().with_metrics(metrics.clone());
        store
            .update(|m| {
                m.bodies[0].resize(129, 0);
                Ok(())
            })
            .unwrap();
        assert_eq!(store.flush(), Err(Error::Storage));
        let snapshot = metrics.snapshot();
        assert_eq!(snapshot.image_encode.calls, 1);
        assert_eq!(snapshot.image_checksum.calls, 0);
        assert_eq!(snapshot.backend_replace.calls, 0);
        assert_eq!(store.flush(), Err(Error::Storage));
        assert_eq!(
            metrics.snapshot(),
            snapshot,
            "poisoned store does not retry"
        );
        for applied in [false, true] {
            let metrics = Arc::new(Metrics::default());
            let mut store = buffered().with_metrics(metrics.clone());
            store.write_body(0, b"ambiguous").unwrap();
            store.backend.fail = Some(applied);
            assert_eq!(store.flush(), Err(Error::Storage));
            let snapshot = metrics.snapshot();
            assert_eq!(snapshot.image_encode.calls, 1);
            assert_eq!(snapshot.image_checksum.calls, 1);
            assert_eq!(snapshot.backend_replace.calls, 1);
            assert!(store.is_poisoned());
            assert_eq!(store.flush(), Err(Error::Storage));
            assert_eq!(metrics.snapshot(), snapshot);
        }
    }

    #[test]
    fn buffered_batches_mutations_and_drops_without_io() {
        let mut store = buffered();
        let old = store.backend.image.clone();
        store.write_body(0, b"new body").unwrap();
        store.write_body(1, b"another body").unwrap();
        assert_eq!(store.backend.replacements, 0);
        assert_eq!(store.body(0), b"new body");
        assert_eq!(store.backend.image, old);
        store.flush_publication().unwrap();
        assert_eq!(store.backend.replacements, 1);
        assert_eq!(
            decode(&store.backend.image, store.limits).unwrap().body(0),
            b"new body"
        );
        store.flush().unwrap();
        store.flush_publication().unwrap();
        assert_eq!(store.backend.replacements, 1);
        let committed = store.backend.image.clone();
        store.write_body(0, b"discard me").unwrap();
        let block = store.into_backend();
        assert_eq!(block.replacements, 1);
        assert_eq!(block.image, committed);
    }

    #[test]
    fn buffered_progress_needs_full_checkpoint_and_never_clears_publication_dirty() {
        let mut store = buffered();
        let old = store.backend.image.clone();
        let mut d = store.descriptor().unwrap().clone();
        store.write_descriptor(&d).unwrap();
        store.flush().unwrap();
        assert_eq!(store.backend.replacements, 0);
        d.height_remaining -= 1;
        d.timers[0].remaining -= 1;
        d.grid_remaining = 0;
        store.write_progress_descriptor(&d).unwrap();
        d.grid_remaining = 10;
        store.write_progress_descriptor(&d).unwrap();
        store.write_descriptor(&d).unwrap();
        store.flush_publication().unwrap();
        assert_eq!(store.backend.image, old);
        assert_eq!(store.backend.replacements, 0);
        store.flush().unwrap();
        assert_eq!(store.backend.replacements, 1);
        assert_eq!(
            decode(&store.backend.image, store.limits)
                .unwrap()
                .descriptor(),
            Some(&d)
        );
        store.write_body(0, b"publication").unwrap();
        d.timers[0].remaining -= 1;
        store.write_progress_descriptor(&d).unwrap();
        store.write_descriptor(&d).unwrap();
        store.flush_publication().unwrap();
        assert_eq!(store.backend.replacements, 2);
        assert!(!store.dirty);
        assert!(!store.publication_dirty);
    }

    #[test]
    fn buffered_progress_rejects_authority_and_countdown_changes() {
        let changes: &[fn(&mut Descriptor)] = &[
            |d| d.config[0] ^= 1,
            |d| d.height += 1,
            |d| d.round += 1,
            |d| d.step = Step::Decided,
            |d| d.locked_id = Some([88; 32]),
            |d| d.locked_round = None,
            |d| d.valid_round = None,
            |d| d.slot = None,
            |d| d.justified = false,
            |d| d.vc = None,
            |d| d.entry = None,
            |d| d.pending = None,
            |d| d.statuses[0] = Status::Valid,
            |d| d.body_generations[0] += 1,
            |d| d.next_generation += 1,
            |d| d.timers[0].active = false,
            |d| d.timers[0].height += 1,
            |d| d.timers[0].round += 1,
            |d| d.timers[0].remaining += 1,
            |d| d.timers[1].remaining -= 1,
            |d| d.height_remaining += 1,
            |d| d.grid_remaining = 11,
            |d| d.intents[0] = None,
            |d| d.proposal_intent = None,
            |d| d.proposal_published = false,
        ];
        for change in changes {
            let mut store = buffered();
            let mut d = store.descriptor().unwrap().clone();
            change(&mut d);
            assert_eq!(store.write_progress_descriptor(&d), Err(Error::Storage));
            assert!(store.is_poisoned());
            assert_eq!(store.flush(), Err(Error::Storage));
            assert_eq!(store.flush_publication(), Err(Error::Storage));
            assert_eq!(store.backend.replacements, 0);
        }
    }

    #[test]
    fn buffered_mutation_encode_and_ambiguous_replace_errors_poison() {
        let mut store = buffered();
        assert_eq!(store.update(|_| Err(Error::Invalid)), Err(Error::Storage));
        assert!(store.is_poisoned());
        assert_eq!(store.flush_publication(), Err(Error::Storage));

        let mut store = buffered();
        store
            .update(|m| {
                m.bodies[0].resize(129, 0);
                Ok(())
            })
            .unwrap();
        assert_eq!(store.flush(), Err(Error::Storage));
        assert!(store.is_poisoned());
        assert_eq!(store.backend.replacements, 0);

        for applied in [false, true] {
            let mut store = buffered();
            let old = store.backend.image.clone();
            store.write_body(0, b"ambiguous").unwrap();
            store.backend.fail = Some(applied);
            assert_eq!(store.flush_publication(), Err(Error::Storage));
            assert!(store.is_poisoned());
            assert_eq!(store.write_body(0, b"retry"), Err(Error::Storage));
            assert_eq!(store.flush(), Err(Error::Storage));
            assert_eq!(store.backend.replacements, 1);
            assert_eq!(store.backend.image != old, applied);
            let limits = store.limits;
            let reopened = DurableStore::open_buffered(store.into_backend(), limits).unwrap();
            assert!(!reopened.is_poisoned());
            if applied {
                assert_eq!(reopened.body(0), b"ambiguous");
            } else {
                assert_eq!(reopened.image, old);
            }
        }
    }
}
