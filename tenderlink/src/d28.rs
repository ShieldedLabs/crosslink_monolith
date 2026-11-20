//! Bounded, one-height D28 WeightedPacketJump baseline.
//!
//! This module is deliberately independent of STP and of the legacy engine. All
//! time is **online** time. The caller supplies complete borrowed frames and
//! trimodal validity results; networking, body acquisition, INIT agreement and
//! the decided archive are outside this core. A storage error poisons the live
//! engine: recover it before doing anything else. `Storage` describes logical
//! publication records for the executable reference. The application
//! persistence contract is `AtomicBlockStore`; its adapter/backend are deferred.
//! Two bounded CONFIG banks permit trusted roster replacement at higher INIT.
//! This is a deliberate extension of static-roster D28, not a claim that its
//! unchanged static-CONFIG proof or exact memory count covers configuration changes.
//!
//! Workspace has three proposal slots, four certificate regions, three current
//! OWN rows, one own header, two current raw rows and O(N) reusable scalar
//! scratch. There is no round journal, future-vote bank or output queue. Use
//! configured/high-water Nmax and Bmax across accepted roster changes when
//! budgeting memory: the two CONFIG banks add O(Nmax) key/weight words, and Vec
//! capacities/allocator overhead mean these are logical role bounds, **not** an
//! exact byte/RSS bound or a fixed-size codec. Reject untrusted wire lengths
//! before allocating a `Frame`; this API only admits borrowed typed frames.
//! Receive indexing adds at most N+1 pairs of usize physical coordinates and
//! N+1 u64 cached weights (24*(N+1) payload bytes on 64-bit targets, plus two
//! Vec headers/capacity/allocator overhead). They contain no votes or bodies,
//! are reused across frames, and are cleared at RX release, INIT and recovery.
//! Accepted position access is O(1); conflicting-row weight preparation and
//! max-cut jump selection are O(N^2) worst case, not a claim of linear whole
//! receive work. Group duplicate checking and sparse-row traversal also remain
//! quadratic in the worst case. Signature verification stays <=N+2 per frame.
//!
//! Validity and acquisition are trusted host events tagged to retained copies
//! and current proposal opportunities. A retained determinate validity latch
//! cannot change via `validity`; a newly evaluated same-id network copy may
//! resolve a retained pending record to Valid without replacing its exact header.
//! `Sink::acquire` does not await: the host supplies locally ready candidates
//! before advancing online time if it relies on D28's zero-time getValue premise.
//! Timer arithmetic uses checked u64 durations and fails closed at exhaustion;
//! the mathematical design's unbounded-round liveness is not claimed past that
//! finite integer domain. Transport admission/delivery and host INIT agreement
//! remain external assumptions.
use ed25519_zebra::{Signature, SigningKey, VerificationKey, VerificationKeyBytes};

pub type Id = [u8; 32];
pub type Round = u64;
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Member {
    pub public_key: [u8; 32],
    pub weight: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timeouts {
    pub propose: u64,
    pub prevote: u64,
    pub precommit: u64,
    pub inc_propose: u64,
    pub inc_prevote: u64,
    pub inc_precommit: u64,
}
impl Timeouts {
    fn at(self, kind: usize, r: Round) -> Result<u64> {
        let (base, inc) = match kind {
            0 => (self.propose, self.inc_propose),
            1 | 2 => (self.prevote, self.inc_prevote),
            _ => (self.precommit, self.inc_precommit),
        };
        base.checked_add(inc.checked_mul(r).ok_or(Error::Exhausted)?)
            .ok_or(Error::Exhausted)
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    domain: Id,
    members: Vec<Member>,
    total: u64,
    max_body: usize,
    rho: u64,
    timeouts: Timeouts,
    hash: Id,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Config,
    Inactive,
    Storage,
    Corrupt,
    Invalid,
    Stale,
    Exhausted,
}
pub type Result<T> = std::result::Result<T, Error>;
impl Config {
    pub fn new(
        domain: Id,
        members: Vec<Member>,
        max_body: usize,
        rho: u64,
        timeouts: Timeouts,
    ) -> Result<Self> {
        if members.is_empty()
            || members.len() > u32::MAX as usize
            || rho == 0
            || max_body == 0
            || !(timeouts.inc_propose > timeouts.inc_precommit
                && timeouts.inc_precommit > timeouts.inc_prevote
                && timeouts.inc_prevote > 0)
        {
            return Err(Error::Config);
        }
        let mut total = 0u64;
        for (i, m) in members.iter().enumerate() {
            if m.weight == 0
                || members[..i].iter().any(|x| x.public_key == m.public_key)
                || VerificationKey::try_from(VerificationKeyBytes::from(m.public_key)).is_err()
            {
                return Err(Error::Config);
            }
            total = total.checked_add(m.weight).ok_or(Error::Config)?;
        }
        total.checked_mul(3).ok_or(Error::Config)?;
        let mut h = blake3::Hasher::new();
        h.update(b"tenderlink/D28/config/v1");
        h.update(&domain);
        for m in &members {
            h.update(&m.public_key);
            h.update(&m.weight.to_le_bytes());
        }
        h.update(&(max_body as u64).to_le_bytes());
        h.update(&rho.to_le_bytes());
        for t in [
            timeouts.propose,
            timeouts.prevote,
            timeouts.precommit,
            timeouts.inc_propose,
            timeouts.inc_prevote,
            timeouts.inc_precommit,
        ] {
            h.update(&t.to_le_bytes());
        }
        Ok(Self {
            domain,
            members,
            total,
            max_body,
            rho,
            timeouts,
            hash: *h.finalize().as_bytes(),
        })
    }
    pub fn domain(&self) -> Id {
        self.domain
    }
    pub fn max_body(&self) -> usize {
        self.max_body
    }
    pub fn rho(&self) -> u64 {
        self.rho
    }
    pub fn timeouts(&self) -> Timeouts {
        self.timeouts
    }
    pub fn members(&self) -> &[Member] {
        &self.members
    }
    pub fn total_weight(&self) -> u64 {
        self.total
    }
    pub fn hash(&self) -> Id {
        self.hash
    }
    pub fn quorum(&self, w: u64) -> bool {
        w <= self.total && 3 * w > 2 * self.total
    }
    pub fn witness(&self, w: u64) -> bool {
        w <= self.total && 3 * w > self.total
    }
    pub fn proposer(&self, h: u64, r: u64) -> usize {
        ((h % self.members.len() as u64 + r % self.members.len() as u64)
            % self.members.len() as u64) as usize
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum VoteKind {
    Enter,
    Prevote,
    Precommit,
}
impl VoteKind {
    fn index(self) -> usize {
        self as usize
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VoteContent {
    pub height: u64,
    pub round: Round,
    pub kind: VoteKind,
    pub id: Option<Id>,
    pub signer: u32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Vote {
    pub content: VoteContent,
    pub signature: [u8; 64],
}
fn vote_bytes(c: &Config, v: VoteContent) -> [u8; 133] {
    let mut b = [0; 133];
    b[..16].copy_from_slice(b"D28/vote/v1\0\0\0\0\0");
    b[16..48].copy_from_slice(&c.hash);
    b[48..80].copy_from_slice(&c.domain);
    b[80..88].copy_from_slice(&v.height.to_le_bytes());
    b[88..96].copy_from_slice(&v.round.to_le_bytes());
    b[96] = v.kind as u8;
    b[97..101].copy_from_slice(&v.signer.to_le_bytes());
    // Hash the tagged id into a fixed tail; nil cannot alias an all-zero id.
    let mut h = blake3::Hasher::new();
    h.update(&[v.id.is_some() as u8]);
    if let Some(id) = v.id {
        h.update(&id);
    }
    b[101..].copy_from_slice(h.finalize().as_bytes());
    b
}
impl Vote {
    pub fn sign(config: &Config, key: &SigningKey, content: VoteContent) -> Self {
        Self {
            content,
            signature: key.sign(&vote_bytes(config, content)).into(),
        }
    }
    pub fn verify(&self, c: &Config) -> bool {
        let v = self.content;
        if v.kind == VoteKind::Enter && v.id.is_some() {
            return false;
        }
        c.members
            .get(v.signer as usize)
            .and_then(|m| VerificationKey::try_from(VerificationKeyBytes::from(m.public_key)).ok())
            .is_some_and(|k| {
                k.verify(&Signature::from(self.signature), &vote_bytes(c, v))
                    .is_ok()
            })
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProposalContent {
    pub height: u64,
    pub round: Round,
    pub valid_round: Option<Round>,
    pub id: Id,
    pub signer: u32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub content: ProposalContent,
    pub signature: [u8; 64],
}
fn header_bytes(c: &Config, p: ProposalContent) -> [u8; 133] {
    let mut b = [0; 133];
    b[..16].copy_from_slice(b"D28/prop/v1\0\0\0\0\0");
    b[16..48].copy_from_slice(&c.hash);
    b[48..80].copy_from_slice(&c.domain);
    b[80..88].copy_from_slice(&p.height.to_le_bytes());
    b[88..96].copy_from_slice(&p.round.to_le_bytes());
    b[96] = p.valid_round.is_some() as u8;
    b[97..105].copy_from_slice(&p.valid_round.unwrap_or(0).to_le_bytes());
    b[105..].copy_from_slice(&p.id[..28]);
    // The full id is bound by an additional digest in Header::message.
    b
}
impl Header {
    fn message(c: &Config, p: ProposalContent) -> Id {
        let mut h = blake3::Hasher::new();
        h.update(&header_bytes(c, p));
        h.update(&p.id);
        h.update(&p.signer.to_le_bytes());
        *h.finalize().as_bytes()
    }
    pub fn sign(c: &Config, k: &SigningKey, content: ProposalContent) -> Self {
        Self {
            content,
            signature: k.sign(&Self::message(c, content)).into(),
        }
    }
    pub fn verify(&self, c: &Config) -> bool {
        let p = self.content;
        (p.valid_round.is_none_or(|r| r < p.round))
            && p.signer as usize == c.proposer(p.height, p.round)
            && c.members
                .get(p.signer as usize)
                .and_then(|m| {
                    VerificationKey::try_from(VerificationKeyBytes::from(m.public_key)).ok()
                })
                .is_some_and(|k| {
                    k.verify(&Signature::from(self.signature), &Self::message(c, p))
                        .is_ok()
                })
    }
    pub fn identity(&self, c: &Config) -> Id {
        let mut h = blake3::Hasher::new();
        h.update(&Self::message(c, self.content));
        h.update(&self.signature);
        *h.finalize().as_bytes()
    }
}
pub fn body_id(body: &[u8]) -> Id {
    *blake3::hash(body).as_bytes()
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Indeterminate,
    Valid,
    Invalid,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CertKind {
    Polka { round: Round, id: Id },
    Decision { round: Round, id: Id },
    Timeout { round: Round },
    Jump { round: Round },
    Row { round: Round, kind: VoteKind },
}
#[derive(Clone, Copy)]
pub enum Votes<'a> {
    Dense(&'a [Vote]),
    Rows(&'a [Option<Vote>]),
}
impl<'a> Votes<'a> {
    pub fn storage_len(self) -> usize {
        match self {
            Self::Dense(v) => v.len(),
            Self::Rows(v) => v.len(),
        }
    }
    pub fn len(self) -> usize {
        match self {
            Self::Dense(v) => v.len(),
            Self::Rows(v) => v.iter().filter(|x| x.is_some()).count(),
        }
    }
    pub fn is_empty(self) -> bool {
        self.len() == 0
    }
    pub fn iter(self) -> impl Iterator<Item = &'a Vote> {
        let (dense, rows) = match self {
            Self::Dense(v) => (v, &[][..]),
            Self::Rows(v) => (&[][..], v),
        };
        dense.iter().chain(rows.iter().filter_map(Option::as_ref))
    }
}
#[derive(Clone, Copy)]
pub struct Group<'a> {
    pub kind: CertKind,
    pub votes: Votes<'a>,
}
#[derive(Clone, Copy)]
pub struct Proposal<'a> {
    pub header: &'a Header,
    pub body: &'a [u8],
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Message {
    Ordinary,
    Dec,
}
#[derive(Clone, Copy)]
pub struct Frame<'a> {
    pub message: Message,
    pub outer: Option<&'a Vote>,
    pub proposal: Option<Proposal<'a>>,
    pub groups: &'a [Group<'a>],
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    All,
    Peer(u32),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BodyTag {
    pub height: u64,
    pub header: Id,
    pub generation: u64,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProposalTag {
    pub height: u64,
    pub round: Round,
}
/// Calls are synchronous and nonblocking. Borrowed data must not be retained.
/// `acquire` requests a fresh local candidate; `retry` retries retained validity
/// dependencies only on the persistent rho grid. Neither is a core await.
pub trait Sink {
    fn send(&mut self, target: Target, frame: Frame<'_>);
    fn decided(&mut self, height: u64, proposal: Proposal<'_>, certificate: Group<'_>);
    fn acquire(&mut self, _tag: ProposalTag) {}
    fn retry(&mut self, _tag: BodyTag, _proposal: Proposal<'_>) {}
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    PreRound,
    Propose,
    Prevote,
    Precommit,
    Decided,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Timer {
    pub active: bool,
    pub remaining: u64,
    pub height: u64,
    pub round: Round,
}
impl Timer {
    fn off(h: u64, r: Round) -> Self {
        Self {
            active: false,
            remaining: 0,
            height: h,
            round: r,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlotRef {
    pub index: usize,
    pub header: Id,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegionRef {
    pub index: usize,
    pub digest: Id,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Certified {
    pub proposal: SlotRef,
    pub certificate: RegionRef,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Descriptor {
    pub height: u64,
    pub config: Id,
    pub round: Round,
    pub step: Step,
    pub locked_round: Option<Round>,
    pub locked_id: Option<Id>,
    pub valid_round: Option<Round>,
    pub slot: Option<SlotRef>,
    pub justified: bool,
    pub vc: Option<Certified>,
    pub entry: Option<RegionRef>,
    pub pending: Option<Certified>,
    pub statuses: [Status; 3],
    pub body_generations: [u64; 3],
    pub next_generation: u64,
    pub timers: [Timer; 4],
    pub height_remaining: u64,
    pub grid_remaining: u64,
    pub intents: [Option<VoteContent>; 3],
    pub proposal_intent: Option<ProposalIntent>,
    pub proposal_published: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProposalIntent {
    pub content: ProposalContent,
    pub slot: usize,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RegionHeader {
    pub kind: CertKind,
    pub count: usize,
    pub digest: Id,
}
/// Scalar header for a staged per-height configuration bank.
#[derive(Clone, Copy, Debug)]
pub struct ConfigMeta {
    pub domain: Id,
    pub total: u64,
    pub count: usize,
    pub max_body: usize,
    pub rho: u64,
    pub timeouts: Timeouts,
}
/// Logical bounded-workspace/publication interface used by the reference core.
/// Applications are not required to implement physical row writes or a WAL;
/// see `AtomicBlockStore` for the agreed physical API (adapter deferred).
/// For publication/crash tests each `write_*` except `write_body` is one
/// atomic, durable small-record replacement. A successful call must survive a
/// crash. `write_body` may tear on failure, but is only used on unreferenced
/// slots; bodies precede header/intent/descriptor publication. `copy_body`
/// copies a still-retained valid body to an unreferenced slot without a core
/// scratch body. Reads expose only bounded physical records. No implementation
/// may silently acknowledge an undurable write. On errors no more output occurs.
pub trait Storage {
    fn config(&self) -> Option<&Config>;
    fn config_bank_hash(&self, bank: usize) -> Option<Id>;
    /// Clear only the unreferenced configuration bank and allocate its bounded
    /// roster rows. Metadata is a small atomic record; member rows are atomic.
    fn begin_config(&mut self, bank: usize, meta: ConfigMeta) -> Result<()>;
    fn write_config_member(&mut self, bank: usize, row: usize, member: Member) -> Result<()>;
    fn seal_config_bank(&mut self, bank: usize, hash: Id) -> Result<()>;
    fn seal_config(&mut self, c: &Config) -> Result<()> {
        if (0..2).any(|i| self.config_bank_hash(i) == Some(c.hash)) {
            return Ok(());
        }
        let current = self.descriptor().map(|d| d.config);
        let bank = (0..2)
            .find(|i| self.config_bank_hash(*i) != current || current.is_none())
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
            },
        )?;
        for (row, m) in c.members.iter().enumerate() {
            self.write_config_member(bank, row, m.clone())?;
        }
        self.seal_config_bank(bank, c.hash)
    }
    fn descriptor(&self) -> Option<&Descriptor>;
    fn write_descriptor(&mut self, d: &Descriptor) -> Result<()>;
    fn body(&self, slot: usize) -> &[u8];
    fn header(&self, slot: usize) -> Option<&Header>;
    fn write_body(&mut self, slot: usize, body: &[u8]) -> Result<()>;
    fn copy_body(&mut self, from: usize, to: usize) -> Result<()>;
    fn write_header(&mut self, slot: usize, header: Header) -> Result<()>;
    fn region_header(&self, region: usize) -> Option<RegionHeader>;
    fn region_rows(&self, region: usize) -> &[Option<Vote>];
    fn write_region_row(&mut self, region: usize, row: usize, vote: Option<Vote>) -> Result<()>;
    fn write_region_header(&mut self, region: usize, header: RegionHeader) -> Result<()>;
    fn own(&self, kind: VoteKind) -> Option<&Vote>;
    fn write_own(&mut self, kind: VoteKind, vote: Vote) -> Result<()>;
    fn own_header(&self) -> Option<&Header>;
    fn write_own_header(&mut self, header: Header) -> Result<()>;
}
/// Bounded reference storage and deterministic fail-before-write crash harness.
/// Applications must replace this with durable storage before using real keys.
#[derive(Clone)]
pub struct MemoryStore {
    configs: [Option<Config>; 2],
    sealed: [bool; 2],
    descriptor: Option<Descriptor>,
    bodies: [Vec<u8>; 3],
    headers: [Option<Header>; 3],
    regions: [Vec<Option<Vote>>; 4],
    region_headers: [Option<RegionHeader>; 4],
    own: [Option<Vote>; 3],
    own_header: Option<Header>,
    pub writes: usize,
    pub fail_at: Option<usize>,
}
impl Default for MemoryStore {
    fn default() -> Self {
        Self {
            configs: [None, None],
            sealed: [false; 2],
            descriptor: None,
            bodies: std::array::from_fn(|_| Vec::new()),
            headers: [None; 3],
            regions: std::array::from_fn(|_| Vec::new()),
            region_headers: [None; 4],
            own: [None; 3],
            own_header: None,
            writes: 0,
            fail_at: None,
        }
    }
}
impl MemoryStore {
    fn write(&mut self) -> Result<()> {
        if self.fail_at == Some(self.writes) {
            return Err(Error::Storage);
        }
        self.writes += 1;
        Ok(())
    }
    pub fn clear_fault(&mut self) {
        self.fail_at = None;
    }
}
impl Storage for MemoryStore {
    fn config(&self) -> Option<&Config> {
        let wanted = self.descriptor.as_ref().map(|d| d.config);
        (0..2)
            .filter(|i| self.sealed[*i])
            .filter_map(|i| self.configs[i].as_ref())
            .find(|c| wanted.is_none_or(|h| c.hash == h))
    }
    fn config_bank_hash(&self, i: usize) -> Option<Id> {
        self.sealed[i].then(|| self.configs[i].as_ref().unwrap().hash)
    }
    fn begin_config(&mut self, i: usize, m: ConfigMeta) -> Result<()> {
        self.write()?;
        self.sealed[i] = false;
        self.configs[i] = Some(Config {
            domain: m.domain,
            members: vec![
                Member {
                    public_key: [0; 32],
                    weight: 0
                };
                m.count
            ],
            total: m.total,
            max_body: m.max_body,
            rho: m.rho,
            timeouts: m.timeouts,
            hash: [0; 32],
        });
        Ok(())
    }
    fn write_config_member(&mut self, i: usize, row: usize, m: Member) -> Result<()> {
        self.write()?;
        self.configs[i].as_mut().ok_or(Error::Corrupt)?.members[row] = m;
        Ok(())
    }
    fn seal_config_bank(&mut self, i: usize, h: Id) -> Result<()> {
        self.write()?;
        let c = self.configs[i].as_mut().ok_or(Error::Corrupt)?;
        let rebuilt = Config::new(c.domain, c.members.clone(), c.max_body, c.rho, c.timeouts)?;
        if rebuilt.hash != h || rebuilt.total != c.total {
            return Err(Error::Corrupt);
        }
        c.hash = h;
        self.sealed[i] = true;
        Ok(())
    }
    fn descriptor(&self) -> Option<&Descriptor> {
        self.descriptor.as_ref()
    }
    fn write_descriptor(&mut self, d: &Descriptor) -> Result<()> {
        self.write()?;
        let changed = self
            .descriptor
            .as_ref()
            .is_some_and(|old| old.height != d.height);
        self.descriptor = Some(d.clone());
        if changed && d.slot.is_none() && d.vc.is_none() && d.pending.is_none() && d.entry.is_none()
        {
            for body in &mut self.bodies {
                body.clear();
                body.shrink_to_fit();
            }
            self.headers = [None; 3];
            self.region_headers = [None; 4];
            let n = self.config().ok_or(Error::Corrupt)?.members.len();
            for region in &mut self.regions {
                region.clear();
                region.resize(n, None);
                region.shrink_to_fit();
            }
        }
        Ok(())
    }
    fn body(&self, s: usize) -> &[u8] {
        &self.bodies[s]
    }
    fn header(&self, s: usize) -> Option<&Header> {
        self.headers[s].as_ref()
    }
    fn write_body(&mut self, s: usize, b: &[u8]) -> Result<()> {
        if b.len() > self.config().ok_or(Error::Config)?.max_body {
            return Err(Error::Invalid);
        }
        self.write()?;
        self.bodies[s].clear();
        self.bodies[s].extend_from_slice(b);
        Ok(())
    }
    fn copy_body(&mut self, from: usize, to: usize) -> Result<()> {
        self.write()?;
        if from == to {
            return Err(Error::Corrupt);
        }
        let [a, b, c] = &mut self.bodies;
        match (from, to) {
            (0, 1) => b.clone_from(a),
            (0, 2) => c.clone_from(a),
            (1, 0) => a.clone_from(b),
            (1, 2) => c.clone_from(b),
            (2, 0) => a.clone_from(c),
            (2, 1) => b.clone_from(c),
            _ => return Err(Error::Corrupt),
        }
        Ok(())
    }
    fn write_header(&mut self, s: usize, h: Header) -> Result<()> {
        self.write()?;
        self.headers[s] = Some(h);
        Ok(())
    }
    fn region_header(&self, r: usize) -> Option<RegionHeader> {
        self.region_headers[r]
    }
    fn region_rows(&self, r: usize) -> &[Option<Vote>] {
        let n = self.config().map_or(0, |c| c.members.len());
        &self.regions[r][..n.min(self.regions[r].len())]
    }
    fn write_region_row(&mut self, r: usize, i: usize, v: Option<Vote>) -> Result<()> {
        self.write()?;
        let n = self.config().ok_or(Error::Config)?.members.len();
        self.regions[r].resize(n, None);
        self.regions[r][i] = v;
        Ok(())
    }
    fn write_region_header(&mut self, r: usize, h: RegionHeader) -> Result<()> {
        self.write()?;
        self.region_headers[r] = Some(h);
        Ok(())
    }
    fn own(&self, k: VoteKind) -> Option<&Vote> {
        self.own[k.index()].as_ref()
    }
    fn write_own(&mut self, k: VoteKind, v: Vote) -> Result<()> {
        self.write()?;
        self.own[k.index()] = Some(v);
        Ok(())
    }
    fn own_header(&self) -> Option<&Header> {
        self.own_header.as_ref()
    }
    fn write_own_header(&mut self, h: Header) -> Result<()> {
        self.write()?;
        self.own_header = Some(h);
        Ok(())
    }
}

/// Volatile public emission policy (not persisted, not consensus). `Reference`
/// is the default and the policy after `recover`: every vote carries its
/// evidence. `CompactLive` makes only live state-transition votes (initial and
/// transition ENTER/PREVOTE/PRECOMMIT) bare signed votes; proposer proposals,
/// reactive replies and every periodic rho replay (and recovery replay) remain
/// the complete reference frames, so loss is repaired by the existing grid.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Emission {
    #[default]
    Reference,
    CompactLive,
    /// Guided-only: every automatic (unsolicited) network emission is a bare
    /// signed ENTER/PREVOTE/PRECOMMIT to everyone (live, periodic rho replay
    /// and recovery replay). Proposals, groups and DEC are never sent
    /// automatically; complete frames leave only through `Engine::serve`.
    Guided,
}
pub struct Engine<S: Storage> {
    emission: Emission,
    store: S,
    key: SigningKey,
    me: usize,
    active: bool,
    pv: Vec<Option<Vote>>,
    pc: Vec<Option<Vote>>,
    replied: Vec<bool>,
    selected: Vec<Option<usize>>,
    group_ok: Vec<bool>,
    outer_ok: bool,
    // Physical (group, row) coordinates, not vote copies. Sparse Rows keep
    // their physical row index; outer position zero uses a dummy coordinate.
    rx_positions: Vec<(usize, usize)>,
    rx_weights: Vec<u64>,
    acquiring: bool,
}
impl<S: Storage> Engine<S> {
    /// Attach a sealed or empty store. Use `recover` for an existing DESC;
    /// `boot` rejects one so a restart cannot accidentally act as fresh INIT.
    pub fn boot(mut store: S, key: SigningKey, config: Config) -> Result<Self> {
        if store.descriptor().is_some() {
            return Err(Error::Stale);
        }
        store.seal_config(&config)?;
        Self::attach(store, key)
    }
    fn attach(store: S, key: SigningKey) -> Result<Self> {
        let c = store.config().ok_or(Error::Config)?;
        let public: [u8; 32] = VerificationKeyBytes::from(&key).into();
        let me = c
            .members
            .iter()
            .position(|m| m.public_key == public)
            .unwrap_or(usize::MAX);
        let n = c.members.len();
        Ok(Self {
            emission: Emission::Reference,
            store,
            key,
            me,
            active: true,
            pv: vec![None; n],
            pc: vec![None; n],
            replied: vec![false; n],
            selected: vec![None; n],
            group_ok: vec![false; n + 1],
            outer_ok: false,
            rx_positions: Vec::with_capacity(n + 1),
            rx_weights: Vec::with_capacity(n + 1),
            acquiring: false,
        })
    }
    pub fn emission(&self) -> Emission {
        self.emission
    }
    pub fn set_emission(&mut self, emission: Emission) {
        self.emission = emission;
    }
    pub fn storage(&self) -> &S {
        &self.store
    }
    pub fn into_storage(self) -> S {
        self.store
    }
    pub fn view(&self) -> Option<&Descriptor> {
        self.store.descriptor()
    }
    pub fn prevotes(&self) -> &[Option<Vote>] {
        &self.pv
    }
    pub fn precommits(&self) -> &[Option<Vote>] {
        &self.pc
    }
    pub fn is_active(&self) -> bool {
        self.active
    }
    pub fn local_index(&self) -> Option<usize> {
        (self.me != usize::MAX).then_some(self.me)
    }
    fn c(&self) -> &Config {
        self.store.config().expect("sealed CONFIG")
    }
    fn d(&self) -> &Descriptor {
        self.store.descriptor().expect("initialized DESC")
    }
    fn check(&self) -> Result<()> {
        if !self.active {
            Err(Error::Inactive)
        } else if self.store.descriptor().is_none() {
            Err(Error::Stale)
        } else {
            Ok(())
        }
    }
    fn commit(&mut self, d: Descriptor) -> Result<()> {
        self.store.write_descriptor(&d)
    }
    fn finish<T>(&mut self, r: Result<T>) -> Result<T> {
        if matches!(r, Err(Error::Storage | Error::Corrupt | Error::Exhausted)) {
            self.active = false;
        }
        r
    }
    pub fn init(
        &mut self,
        config: &Config,
        height: u64,
        tau: u64,
        sink: &mut impl Sink,
    ) -> Result<bool> {
        let r = self.init_inner(config, height, tau, sink);
        self.finish(r)
    }
    fn init_inner(
        &mut self,
        config: &Config,
        height: u64,
        tau: u64,
        sink: &mut impl Sink,
    ) -> Result<bool> {
        if !self.active {
            return Err(Error::Inactive);
        }
        if self.store.descriptor().is_some_and(|d| height <= d.height) {
            return Ok(false);
        }
        self.store.seal_config(config)?;
        // Preserve the existing grid without postponing service past the new
        // height's period when trusted INIT changes retransmission parameters.
        let grid = self
            .store
            .descriptor()
            .map_or(config.rho, |d| d.grid_remaining.min(config.rho));
        let d = Descriptor {
            height,
            config: config.hash,
            round: 0,
            step: Step::PreRound,
            locked_round: None,
            locked_id: None,
            valid_round: None,
            slot: None,
            justified: false,
            vc: None,
            entry: None,
            pending: None,
            statuses: [Status::Indeterminate; 3],
            body_generations: [0; 3],
            next_generation: 1,
            timers: [Timer::off(height, 0); 4],
            height_remaining: tau,
            grid_remaining: grid,
            intents: [None; 3],
            proposal_intent: None,
            proposal_published: false,
        };
        self.commit(d)?;
        let public: [u8; 32] = VerificationKeyBytes::from(&self.key).into();
        self.me = self
            .c()
            .members
            .iter()
            .position(|m| m.public_key == public)
            .unwrap_or(usize::MAX);
        let n = self.c().members.len();
        self.pv.resize(n, None);
        self.pc.resize(n, None);
        self.replied.resize(n, false);
        self.selected.resize(n, None);
        self.group_ok.resize(n + 1, false);
        self.pv.shrink_to_fit();
        self.pc.shrink_to_fit();
        self.replied.shrink_to_fit();
        self.selected.shrink_to_fit();
        self.group_ok.shrink_to_fit();
        self.clear_volatile();
        self.clear_rx();
        // Keep scalar scratch bounded by the new roster, including on shrink.
        self.rx_positions.shrink_to(n + 1);
        self.rx_weights.shrink_to(n + 1);
        self.rx_positions.reserve(n + 1);
        self.rx_weights.reserve(n + 1);
        if tau == 0 {
            self.start(0, None, true, sink)?;
        }
        self.drive(sink)?;
        Ok(true)
    }
    fn clear_volatile(&mut self) {
        self.pv.fill(None);
        self.pc.fill(None);
        self.replied.fill(false);
        self.selected.fill(None);
        self.acquiring = false;
    }
    fn proposal(&self, s: SlotRef) -> Proposal<'_> {
        Proposal {
            header: self.store.header(s.index).expect("published header"),
            body: self.store.body(s.index),
        }
    }
    fn group(&self, r: RegionRef) -> Group<'_> {
        Group {
            kind: self
                .store
                .region_header(r.index)
                .expect("published region")
                .kind,
            votes: Votes::Rows(self.store.region_rows(r.index)),
        }
    }
    fn slot_free(&self) -> Result<usize> {
        (0..3).find(|i| !self.slot_used(*i)).ok_or(Error::Exhausted)
    }
    fn slot_used(&self, i: usize) -> bool {
        let d = self.d();
        d.slot.is_some_and(|s| s.index == i)
            || d.vc.is_some_and(|s| s.proposal.index == i)
            || d.pending.is_some_and(|s| s.proposal.index == i)
            || (!d.proposal_published && d.proposal_intent.is_some_and(|s| s.slot == i))
    }
    fn region_free(&self) -> Result<usize> {
        let d = self.d();
        (0..4)
            .find(|i| {
                !d.entry.is_some_and(|r| r.index == *i)
                    && !d.vc.is_some_and(|s| s.certificate.index == *i)
                    && !d.pending.is_some_and(|s| s.certificate.index == *i)
            })
            .ok_or(Error::Exhausted)
    }
    fn current(&self, v: &Vote) -> bool {
        let d = self.d();
        v.content.height == d.height && v.content.round == d.round
    }
    fn own_current(&self, k: VoteKind) -> Option<&Vote> {
        self.store.own(k).filter(|v| self.current(v))
    }
    fn row_weight(&self, k: VoteKind, id: Option<Option<Id>>) -> u64 {
        let row = if k == VoteKind::Prevote {
            &self.pv
        } else {
            &self.pc
        };
        row.iter()
            .flatten()
            .filter(|v| id.is_none_or(|id| v.content.id == id))
            .map(|v| self.c().members[v.content.signer as usize].weight)
            .sum()
    }
    fn insert_own(&mut self, v: Vote) {
        if self.me == usize::MAX {
            return;
        }
        match v.content.kind {
            VoteKind::Prevote => self.pv[self.me] = Some(v),
            VoteKind::Precommit => self.pc[self.me] = Some(v),
            _ => {}
        }
    }
    fn materialize(&mut self, k: VoteKind) -> Result<()> {
        if self.me == usize::MAX {
            return Ok(());
        }
        let Some(intent) = self.d().intents[k.index()] else {
            return Ok(());
        };
        if intent.height != self.d().height
            || intent.round != self.d().round
            || intent.kind != k
            || intent.signer as usize != self.me
            || (k == VoteKind::Enter && intent.id.is_some())
        {
            return Err(Error::Corrupt);
        }
        let v = match self.own_current(k) {
            Some(v) if v.content == intent => *v,
            Some(_) => return Err(Error::Corrupt),
            None => {
                let v = Vote::sign(self.c(), &self.key, intent);
                self.store.write_own(k, v)?;
                v
            }
        };
        self.insert_own(v);
        Ok(())
    }
    fn vote(&mut self, k: VoteKind, id: Option<Id>, sink: &mut impl Sink) -> Result<()> {
        if self.d().step == Step::PreRound || self.d().step == Step::Decided {
            return Err(Error::Corrupt);
        }
        if self.me == usize::MAX {
            let mut d = self.d().clone();
            match k {
                VoteKind::Prevote => d.step = Step::Prevote,
                VoteKind::Precommit => d.step = Step::Precommit,
                _ => {}
            }
            self.commit(d)?;
            return Ok(());
        }
        if let Some(old) = self.d().intents[k.index()] {
            if old.id != id {
                return Err(Error::Corrupt);
            }
            return Ok(());
        }
        let mut d = self.d().clone();
        d.intents[k.index()] = Some(VoteContent {
            height: d.height,
            round: d.round,
            kind: k,
            id,
            signer: self.me as u32,
        });
        self.commit(d)?;
        self.materialize(k)?;
        let mut d = self.d().clone();
        match k {
            VoteKind::Prevote => d.step = Step::Prevote,
            VoteKind::Precommit => d.step = Step::Precommit,
            _ => {}
        }
        self.commit(d)?;
        self.emit_vote(k, Target::All, sink);
        Ok(())
    }
    /// Live state-transition emission, subject to the volatile policy.
    fn emit_vote(&self, k: VoteKind, target: Target, sink: &mut impl Sink) {
        if self.emission != Emission::Reference {
            let Some(v) = self.own_current(k) else { return };
            sink.send(
                target,
                Frame {
                    message: Message::Ordinary,
                    outer: Some(v),
                    proposal: None,
                    groups: &[],
                },
            );
            return;
        }
        self.emit_vote_complete(k, target, sink);
    }
    /// Periodic rho and recovery replay: the complete reference frame, except
    /// that Guided replays the bare current own vote.
    fn emit_replay(&self, k: VoteKind, sink: &mut impl Sink) {
        if self.emission == Emission::Guided {
            self.emit_vote(k, Target::All, sink);
        } else {
            self.emit_vote_complete(k, Target::All, sink);
        }
    }
    /// Complete evidence frame (reference replay and targeted service); not
    /// affected by the CompactLive policy.
    fn emit_vote_complete(&self, k: VoteKind, target: Target, sink: &mut impl Sink) {
        let Some(v) = self.own_current(k) else { return };
        let mut proposal = None;
        let group = match k {
            VoteKind::Enter => self.d().entry.map(|r| self.group(r)),
            VoteKind::Prevote => {
                if let Some(s) = self.d().slot {
                    if v.content.id == Some(self.proposal(s).header.content.id) {
                        proposal = Some(self.proposal(s));
                    }
                }
                Some(Group {
                    kind: CertKind::Row {
                        round: self.d().round,
                        kind: VoteKind::Prevote,
                    },
                    votes: Votes::Rows(&self.pv),
                })
            }
            VoteKind::Precommit => {
                if v.content.id.is_some() {
                    if let (Some(s), Some(vc)) = (self.d().slot, self.d().vc) {
                        if v.content.id == Some(self.proposal(s).header.content.id)
                            && self.d().valid_round == Some(self.d().round)
                        {
                            proposal = Some(self.proposal(s));
                            Some(self.group(vc.certificate))
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    Some(Group {
                        kind: CertKind::Row {
                            round: self.d().round,
                            kind: VoteKind::Prevote,
                        },
                        votes: Votes::Rows(&self.pv),
                    })
                }
            }
        };
        let groups = group;
        sink.send(
            target,
            Frame {
                message: Message::Ordinary,
                outer: Some(v),
                proposal,
                groups: groups.as_slice(),
            },
        );
    }
    fn emit_proposal(&self, target: Target, sink: &mut impl Sink) {
        if self.emission == Emission::Guided {
            return;
        }
        if self.c().proposer(self.d().height, self.d().round) != self.me {
            return;
        }
        let Some(s) = self.d().slot else { return };
        let p = self.proposal(s);
        if p.header.content.signer as usize != self.me {
            return;
        }
        let group = self
            .d()
            .vc
            .filter(|vc| self.proposal(vc.proposal).header.content.id == p.header.content.id)
            .map(|vc| self.group(vc.certificate));
        let groups = group;
        sink.send(
            target,
            Frame {
                message: Message::Ordinary,
                outer: None,
                proposal: Some(p),
                groups: groups.as_slice(),
            },
        );
    }
    fn stage_selected(&mut self, kind: CertKind, source: Selection<'_>) -> Result<RegionRef> {
        let index = self.region_free()?;
        let n = self.c().members.len();
        let mut count = 0;
        let mut hash = region_hasher(kind);
        for signer in 0..n {
            let vote = match source {
                Selection::Pv(id) => self.pv[signer].filter(|v| v.content.id == id),
                Selection::Pc(id) => {
                    self.pc[signer].filter(|v| id.is_none_or(|id| v.content.id == id))
                }
                Selection::Frame(frame) => {
                    self.selected[signer].and_then(|i| self.accepted_vote(frame, i).copied())
                }
                Selection::Group(g) => g
                    .votes
                    .iter()
                    .find(|v| v.content.signer as usize == signer)
                    .copied(),
                Selection::Timeout(frame) => self.timeout_vote(frame, signer),
            };
            self.store.write_region_row(index, signer, vote)?;
            if let Some(v) = vote {
                count += 1;
                hash_vote(&mut hash, &v);
            }
        }
        let digest = *hash.finalize().as_bytes();
        self.store.write_region_header(
            index,
            RegionHeader {
                kind,
                count,
                digest,
            },
        )?;
        Ok(RegionRef { index, digest })
    }
    fn start(
        &mut self,
        round: Round,
        entry: Option<RegionRef>,
        height_start: bool,
        sink: &mut impl Sink,
    ) -> Result<()> {
        let mut d = self.d().clone();
        d.round = round;
        d.step = Step::Propose;
        d.entry = entry;
        d.intents = [None; 3];
        if self.me != usize::MAX {
            d.intents[0] = Some(VoteContent {
                height: d.height,
                round,
                kind: VoteKind::Enter,
                id: None,
                signer: self.me as u32,
            });
        }
        d.proposal_intent = None;
        d.proposal_published = false;
        d.height_remaining = 0;
        d.timers = [Timer::off(d.height, round); 4];
        d.timers[0] = Timer {
            active: true,
            remaining: self.c().timeouts.at(0, round)?,
            height: d.height,
            round,
        };
        if !height_start {
            d.slot = None;
            d.justified = false;
        }
        self.commit(d)?;
        if !height_start {
            self.clear_volatile();
        } else {
            self.replied.fill(false);
            self.acquiring = false;
        }
        self.materialize(VoteKind::Enter)?;
        self.emit_vote(VoteKind::Enter, Target::All, sink);
        self.request_proposal(sink);
        Ok(())
    }
    fn request_proposal(&mut self, sink: &mut impl Sink) {
        if self.d().step == Step::Propose
            && self.c().proposer(self.d().height, self.d().round) == self.me
            && self.d().proposal_intent.is_none()
            && self.d().vc.is_none()
            && self.own_current(VoteKind::Prevote).is_none()
            && !self.acquiring
        {
            self.acquiring = true;
            sink.acquire(ProposalTag {
                height: self.d().height,
                round: self.d().round,
            });
        }
    }
    pub fn proposal_tag(&self) -> Option<ProposalTag> {
        self.store
            .descriptor()
            .filter(|d| {
                self.active
                    && d.step == Step::Propose
                    && self.c().proposer(d.height, d.round) == self.me
                    && d.proposal_intent.is_none()
            })
            .map(|d| ProposalTag {
                height: d.height,
                round: d.round,
            })
    }
    pub fn supply_proposal(
        &mut self,
        tag: ProposalTag,
        body: &[u8],
        status: Status,
        sink: &mut impl Sink,
    ) -> Result<()> {
        let r = self.supply_inner(tag, body, status, sink);
        self.finish(r)
    }
    fn supply_inner(
        &mut self,
        tag: ProposalTag,
        body: &[u8],
        status: Status,
        sink: &mut impl Sink,
    ) -> Result<()> {
        self.check()?;
        if self.proposal_tag() != Some(tag) || self.own_current(VoteKind::Prevote).is_some() {
            return Err(Error::Stale);
        }
        if status != Status::Valid {
            return Ok(());
        }
        if body.len() > self.c().max_body {
            return Err(Error::Invalid);
        }
        let slot = self.slot_free()?;
        let (id, vr) = if let Some(vc) = self.d().vc {
            self.store.copy_body(vc.proposal.index, slot)?;
            (
                self.proposal(vc.proposal).header.content.id,
                self.d().valid_round,
            )
        } else {
            self.store.write_body(slot, body)?;
            (body_id(body), None)
        };
        let content = ProposalContent {
            height: tag.height,
            round: tag.round,
            valid_round: vr,
            id,
            signer: self.me as u32,
        };
        let mut d = self.d().clone();
        d.proposal_intent = Some(ProposalIntent { content, slot });
        self.commit(d)?;
        self.complete_proposal()?;
        self.emit_proposal(Target::All, sink);
        self.drive(sink)
    }
    fn complete_proposal(&mut self) -> Result<()> {
        let Some(intent) = self.d().proposal_intent else {
            return Ok(());
        };
        if self.d().proposal_published {
            return Ok(());
        }
        let content = intent.content;
        if content.height != self.d().height || content.round != self.d().round {
            return Err(Error::Corrupt);
        }
        if self.d().slot.is_some_and(|s| {
            self.store
                .header(s.index)
                .is_some_and(|h| h.content == content)
        }) {
            return Ok(());
        }
        if body_id(self.store.body(intent.slot)) != content.id {
            return Err(Error::Corrupt);
        }
        let header = match self.store.own_header() {
            Some(h) if h.content == content => *h,
            Some(h) if h.content.height == content.height && h.content.round == content.round => {
                return Err(Error::Corrupt);
            }
            _ => {
                let h = Header::sign(self.c(), &self.key, content);
                self.store.write_own_header(h)?;
                h
            }
        };
        self.store.write_header(intent.slot, header)?;
        if let Some(vr) = content.valid_round {
            let Some(vc) = self.d().vc else {
                return Err(Error::Corrupt);
            };
            if self.group(vc.certificate).kind
                != (CertKind::Polka {
                    round: vr,
                    id: content.id,
                })
            {
                return Err(Error::Corrupt);
            }
        }
        let mut d = self.d().clone();
        d.slot = Some(SlotRef {
            index: intent.slot,
            header: header.identity(self.c()),
        });
        d.statuses[intent.slot] = Status::Valid;
        d.body_generations[intent.slot] = d.next_generation;
        d.next_generation = d.next_generation.checked_add(1).ok_or(Error::Exhausted)?;
        d.justified = content.valid_round.is_some();
        d.proposal_published = true;
        self.commit(d)?;
        self.acquiring = false;
        Ok(())
    }
}
#[derive(Clone, Copy)]
enum Selection<'a> {
    Pv(Option<Id>),
    Pc(Option<Option<Id>>),
    Frame(Frame<'a>),
    Group(Group<'a>),
    Timeout(Frame<'a>),
}
fn region_hasher(kind: CertKind) -> blake3::Hasher {
    let mut h = blake3::Hasher::new();
    h.update(b"D28/region/v1");
    match kind {
        CertKind::Polka { round, id } => {
            h.update(&[0]);
            h.update(&round.to_le_bytes());
            h.update(&id);
        }
        CertKind::Decision { round, id } => {
            h.update(&[1]);
            h.update(&round.to_le_bytes());
            h.update(&id);
        }
        CertKind::Timeout { round } => {
            h.update(&[2]);
            h.update(&round.to_le_bytes());
        }
        CertKind::Jump { round } => {
            h.update(&[3]);
            h.update(&round.to_le_bytes());
        }
        CertKind::Row { round, kind } => {
            h.update(&[4, kind as u8]);
            h.update(&round.to_le_bytes());
        }
    }
    h
}
fn hash_vote(h: &mut blake3::Hasher, v: &Vote) {
    h.update(&v.content.height.to_le_bytes());
    h.update(&v.content.round.to_le_bytes());
    h.update(&[v.content.kind as u8, v.content.id.is_some() as u8]);
    h.update(&v.content.signer.to_le_bytes());
    if let Some(id) = v.content.id {
        h.update(&id);
    }
    h.update(&v.signature);
}

pub mod atomic;
pub mod gossip;
pub use atomic::AtomicBlockStore;
mod core;
mod recovery;
#[cfg(test)]
mod tests;
