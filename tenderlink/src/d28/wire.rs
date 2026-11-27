//! Versioned BFT-only wire records with bounded, allocation-free admission.
//!
//! These records are not legacy packets or ledger encodings. All integers are
//! little-endian; tags are explicit, optional scalar payloads occupy fixed space
//! and must be zero when absent. No trailing bytes or unknown tags are accepted.
//! A frame has a request sequence, message, optional outer vote, optional exact
//! header and length-prefixed body, then a count of certificate groups. Each
//! group has a fixed certificate descriptor and a count of dense vote records.
//! Sparse local rows are flattened, never reconstructed as N-sized remote rows.
//!
//! Decoding checks the complete record before allocating O(N) scalar/vote
//! metadata. The proposal body remains borrowed. These are structural checks,
//! not signature, quorum, duplicate-signer, round-domain or body-id validation:
//! the core must still receive and validate the complete frame. In particular,
//! `request` is only an untrusted requester sequence, not a local grant, peer
//! generation or authorization. Status configuration hashes are copied exactly.

use super::gossip::StatusSnapshot;
use super::{
    CertKind, Error, Frame, Header, Id, Message, Proposal, ProposalContent, Result, Step, Vote,
    VoteContent, VoteKind, Votes,
};

/// All records here belong to the BFT family, never the base packet family.
pub const BFT_FAMILY: u8 = 0x80;
/// Reserved for the runtime's separately authenticated hello, not parsed here.
pub const HELLO_TAG: u8 = 0xB0;
pub const STATUS_TAG: u8 = 0xB1;
pub const FRAME_TAG: u8 = 0xB2;
pub const OUTLINED_TAG: u8 = 0xB3;
pub const BODY_REQUEST_TAG: u8 = 0xB4;
pub const BODY_RESPONSE_TAG: u8 = 0xB5;
pub const BODY_ACK_TAG: u8 = 0xB6;
pub const VERSION: u8 = 1;
pub const BODY_CONTROL_BYTES: usize = 90;
pub const BODY_RESPONSE_OVERHEAD: usize = BODY_CONTROL_BYTES + 8;
/// Status has a fixed size, including zero-filled absent optional payloads.
pub const STATUS_BYTES: usize = 144;

const VOTE_BYTES: usize = 118;
const HEADER_BYTES: usize = 125;
const GROUP_BYTES: usize = 42 + 8; // certificate descriptor + vote count
const FRAME_BYTES: usize = 22; // tags, request, message, presence flags, group count
const PROPOSAL_BYTES: usize = HEADER_BYTES + 8; // header + body length

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub max_members: usize,
    pub max_body: usize,
}
impl Limits {
    /// Maximum encoded frame length, including N+1 groups and N+1 occupied
    /// vote positions (the outer vote counts as a position). Checked before
    /// inspecting a frame or reserving memory. Unrepresentable limits fail.
    pub fn max_frame_bytes(self) -> Result<usize> {
        let positions = self.max_positions()?;
        let metadata = positions
            .checked_mul(GROUP_BYTES + VOTE_BYTES)
            .ok_or(Error::Exhausted)?;
        let bytes = FRAME_BYTES
            .checked_add(PROPOSAL_BYTES)
            .and_then(|n| n.checked_add(metadata))
            .and_then(|n| n.checked_add(self.max_body))
            .ok_or(Error::Exhausted)?;
        if bytes > isize::MAX as usize {
            return Err(Error::Exhausted);
        }
        Ok(bytes)
    }

    pub fn max_outlined_bytes(self) -> Result<usize> {
        self.max_frame_bytes()?
            .checked_sub(self.max_body)
            .and_then(|n| n.checked_add(32))
            .ok_or(Error::Exhausted)
    }

    fn max_positions(self) -> Result<usize> {
        self.max_members.checked_add(1).ok_or(Error::Exhausted)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WireStatus {
    pub sequence: u64,
    pub request: Option<u64>,
    pub snapshot: StatusSnapshot,
}

pub fn encode_status(status: WireStatus) -> Result<Vec<u8>> {
    let mut out = reserved(STATUS_BYTES)?;
    out.extend_from_slice(&[STATUS_TAG, VERSION]);
    put_u64(&mut out, status.sequence);
    put_option_u64(&mut out, status.request);
    let s = status.snapshot;
    out.extend_from_slice(&s.config);
    put_u64(&mut out, s.height);
    put_u64(&mut out, s.round);
    out.push(match s.step {
        Step::PreRound => 0,
        Step::Propose => 1,
        Step::Prevote => 2,
        Step::Precommit => 3,
        Step::Decided => 4,
    });
    put_option_id(&mut out, s.proposal);
    out.push(u8::from(s.polka.is_some()));
    let (round, id) = s.polka.unwrap_or((0, [0; 32]));
    put_u64(&mut out, round);
    out.extend_from_slice(&id);
    out.extend_from_slice(&[u8::from(s.prevote_quorum), u8::from(s.justified)]);
    debug_assert_eq!(out.len(), STATUS_BYTES);
    Ok(out)
}

pub fn decode_status(bytes: &[u8]) -> Result<WireStatus> {
    if bytes.len() != STATUS_BYTES {
        return Err(Error::Invalid);
    }
    let mut r = Reader::new(bytes);
    r.prefix(STATUS_TAG)?;
    let sequence = r.u64()?;
    let request = r.option_u64()?;
    let config = r.array()?;
    let height = r.u64()?;
    let round = r.u64()?;
    let step = match r.byte()? {
        0 => Step::PreRound,
        1 => Step::Propose,
        2 => Step::Prevote,
        3 => Step::Precommit,
        4 => Step::Decided,
        _ => return Err(Error::Invalid),
    };
    let proposal = r.option_id()?;
    let has_polka = r.boolean()?;
    let polka_round = r.u64()?;
    let polka_id = r.array()?;
    let polka = if has_polka {
        Some((polka_round, polka_id))
    } else {
        if polka_round != 0 || polka_id != [0; 32] {
            return Err(Error::Invalid);
        }
        None
    };
    let snapshot = StatusSnapshot {
        config,
        height,
        round,
        step,
        proposal,
        polka,
        prevote_quorum: r.boolean()?,
        justified: r.boolean()?,
    };
    r.finish()?;
    Ok(WireStatus {
        sequence,
        request,
        snapshot,
    })
}

struct DecodedGroup {
    kind: CertKind,
    start: usize,
    end: usize,
}

/// Bounded dense metadata with a proposal body borrowed from the input record.
/// Use `with_frame` to present the complete package to the consensus core.
pub struct DecodedFrame<'a> {
    pub request: Option<u64>,
    message: Message,
    outer: Option<Vote>,
    proposal: Option<(Header, &'a [u8])>,
    groups: Vec<DecodedGroup>,
    votes: Vec<Vote>,
}
/// Owned O(N) evidence only. A complete, independently validated body must be
/// supplied before this can be materialized for the core.
pub struct DecodedOutlined {
    pub config: Id,
    pub body_len: Option<usize>,
    metadata: DecodedFrame<'static>,
}
impl DecodedOutlined {
    pub fn header(&self) -> Option<&Header> {
        self.metadata.proposal.as_ref().map(|(h, _)| h)
    }
    pub(crate) fn priority(&self) -> (bool, u64, usize) {
        (
            self.metadata.message == Message::Dec,
            self.header().map_or(0, |h| h.content.round),
            self.metadata.votes.len(),
        )
    }
    pub fn with_body<R>(&self, body: &[u8], f: impl FnOnce(Frame<'_>) -> R) -> R {
        self.metadata.with_frame(|frame| {
            f(Frame {
                proposal: frame.proposal.map(|p| Proposal {
                    header: p.header,
                    body,
                }),
                ..frame
            })
        })
    }
}

pub fn decode_outlined(bytes: &[u8], limits: Limits) -> Result<DecodedOutlined> {
    let parsed = preflight(bytes, limits, true)?;
    let config = parsed.config.ok_or(Error::Invalid)?;
    let body_len = parsed.body_len;
    let decoded = decode_parsed(parsed, limits)?;
    Ok(DecodedOutlined {
        config,
        body_len,
        metadata: DecodedFrame {
            request: decoded.request,
            message: decoded.message,
            outer: decoded.outer,
            proposal: decoded.proposal.map(|(header, _)| (header, &[][..])),
            groups: decoded.groups,
            votes: decoded.votes,
        },
    })
}

/// Complete transfer fences. Generation is chosen by the requester for this
/// authenticated session and echoed verbatim; it is not the sender's counter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BodyKey {
    pub config: Id,
    pub height: u64,
    pub id: Id,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BodyRequest {
    pub token: u64,
    pub generation: u64,
    pub key: BodyKey,
}
fn put_body_control(out: &mut Vec<u8>, tag: u8, request: BodyRequest) -> Result<()> {
    if request.token == 0 || request.generation == 0 {
        return Err(Error::Invalid);
    }
    out.extend_from_slice(&[tag, VERSION]);
    put_u64(out, request.token);
    put_u64(out, request.generation);
    out.extend_from_slice(&request.key.config);
    put_u64(out, request.key.height);
    out.extend_from_slice(&request.key.id);
    Ok(())
}
fn read_body_control(r: &mut Reader<'_>, tag: u8) -> Result<BodyRequest> {
    r.prefix(tag)?;
    let request = BodyRequest {
        token: r.u64()?,
        generation: r.u64()?,
        key: BodyKey {
            config: r.array()?,
            height: r.u64()?,
            id: r.array()?,
        },
    };
    if request.token == 0 || request.generation == 0 {
        return Err(Error::Invalid);
    }
    Ok(request)
}
pub fn encode_body_request(request: BodyRequest) -> Result<Vec<u8>> {
    let mut out = reserved(BODY_CONTROL_BYTES)?;
    put_body_control(&mut out, BODY_REQUEST_TAG, request)?;
    Ok(out)
}
pub fn encode_body_ack(request: BodyRequest) -> Result<Vec<u8>> {
    let mut out = reserved(BODY_CONTROL_BYTES)?;
    put_body_control(&mut out, BODY_ACK_TAG, request)?;
    Ok(out)
}
fn decode_body_control(bytes: &[u8], tag: u8) -> Result<BodyRequest> {
    if bytes.len() != BODY_CONTROL_BYTES {
        return Err(Error::Invalid);
    }
    let mut r = Reader::new(bytes);
    let request = read_body_control(&mut r, tag)?;
    r.finish()?;
    Ok(request)
}
pub fn decode_body_request(bytes: &[u8]) -> Result<BodyRequest> {
    decode_body_control(bytes, BODY_REQUEST_TAG)
}
pub fn decode_body_ack(bytes: &[u8]) -> Result<BodyRequest> {
    decode_body_control(bytes, BODY_ACK_TAG)
}
pub fn encode_body_response(request: BodyRequest, body: &[u8], limits: Limits) -> Result<Vec<u8>> {
    if body.len() > limits.max_body {
        return Err(Error::Invalid);
    }
    let size = BODY_RESPONSE_OVERHEAD
        .checked_add(body.len())
        .ok_or(Error::Exhausted)?;
    check_capacity::<u8>(size)?;
    let mut out = reserved(size)?;
    put_body_control(&mut out, BODY_RESPONSE_TAG, request)?;
    put_u64(&mut out, body.len() as u64);
    out.extend_from_slice(body);
    Ok(out)
}
/// No allocation: validate all length arithmetic and reject trailing data first.
pub fn decode_body_response(bytes: &[u8], limits: Limits) -> Result<(BodyRequest, &[u8])> {
    let max = BODY_RESPONSE_OVERHEAD
        .checked_add(limits.max_body)
        .ok_or(Error::Exhausted)?;
    if bytes.len() > max {
        return Err(Error::Invalid);
    }
    let mut r = Reader::new(bytes);
    let request = read_body_control(&mut r, BODY_RESPONSE_TAG)?;
    let len = r.count(limits.max_body)?;
    let body = r.take(len)?;
    r.finish()?;
    Ok((request, body))
}

impl DecodedFrame<'_> {
    /// Creates O(N) temporary group references, not another copy of the votes
    /// or body. Neither group references nor a `Frame` can escape the closure.
    pub fn with_frame<R>(&self, f: impl FnOnce(Frame<'_>) -> R) -> R {
        let groups: Vec<_> = self
            .groups
            .iter()
            .map(|g| super::Group {
                kind: g.kind,
                votes: Votes::Dense(&self.votes[g.start..g.end]),
            })
            .collect();
        f(Frame {
            message: self.message,
            outer: self.outer.as_ref(),
            proposal: self
                .proposal
                .as_ref()
                .map(|(header, body)| Proposal { header, body }),
            groups: &groups,
        })
    }
}

/// Checks all bounds before the output Vec is allocated. Sparse local rows are
/// admitted only up to N storage slots, then encoded as their occupied votes.
pub fn encode_frame(frame: Frame<'_>, request: Option<u64>, limits: Limits) -> Result<Vec<u8>> {
    encode_evidence(frame, request, limits, None)
}

/// Same exact signed header and evidence, with only the body bytes omitted.
/// The declared length is an untrusted bounded hint, never a signed commitment.
pub fn encode_outlined(
    frame: Frame<'_>,
    request: Option<u64>,
    config: Id,
    limits: Limits,
) -> Result<Vec<u8>> {
    encode_evidence(frame, request, limits, Some(config))
}

fn encode_evidence(
    frame: Frame<'_>,
    request: Option<u64>,
    limits: Limits,
    config: Option<Id>,
) -> Result<Vec<u8>> {
    let mut size = encoded_size(frame, limits)?;
    if config.is_some() {
        size -= frame.proposal.map_or(0, |p| p.body.len());
        add_size(&mut size, 32)?;
    }
    let mut out = reserved(size)?;
    out.extend_from_slice(&[
        if config.is_some() {
            OUTLINED_TAG
        } else {
            FRAME_TAG
        },
        VERSION,
    ]);
    if let Some(config) = config {
        out.extend_from_slice(&config);
    }
    put_option_u64(&mut out, request);
    out.push(match frame.message {
        Message::Ordinary => 0,
        Message::Dec => 1,
    });
    out.push(u8::from(frame.outer.is_some()));
    if let Some(vote) = frame.outer {
        put_vote(&mut out, vote);
    }
    out.push(u8::from(frame.proposal.is_some()));
    if let Some(p) = frame.proposal {
        put_header(&mut out, p.header);
        put_u64(&mut out, p.body.len() as u64);
        if config.is_none() {
            out.extend_from_slice(p.body);
        }
    }
    put_u64(&mut out, frame.groups.len() as u64);
    for group in frame.groups {
        put_cert(&mut out, group.kind);
        put_u64(&mut out, group.votes.len() as u64);
        for vote in group.votes.iter() {
            put_vote(&mut out, vote);
        }
    }
    debug_assert_eq!(out.len(), size);
    Ok(out)
}

/// The first pass validates the entire byte record without allocating; only
/// then does the second pass reserve bounded dense group/vote metadata.
pub fn decode_frame(bytes: &[u8], limits: Limits) -> Result<DecodedFrame<'_>> {
    decode_parsed(preflight(bytes, limits, false)?, limits)
}

fn decode_parsed<'a>(parsed: Parsed<'a>, limits: Limits) -> Result<DecodedFrame<'a>> {
    // Check every backing allocation's byte arithmetic before making any one
    // allocation; native metadata sizes need not equal the encoded sizes.
    check_capacity::<DecodedGroup>(parsed.group_count)?;
    check_capacity::<Vote>(parsed.vote_count)?;
    check_capacity::<super::Group<'_>>(parsed.group_count)?;
    let mut groups = reserved(parsed.group_count)?;
    let mut votes = reserved(parsed.vote_count)?;
    let mut r = Reader::new(parsed.groups);
    for _ in 0..parsed.group_count {
        let (kind, count, encoded_votes) = r.group(limits.max_members, parsed.vote_count)?;
        let start = votes.len();
        let end = start.checked_add(count).ok_or(Error::Exhausted)?;
        let mut vr = Reader::new(encoded_votes);
        for _ in 0..count {
            votes.push(vr.vote()?);
        }
        groups.push(DecodedGroup { kind, start, end });
    }
    Ok(DecodedFrame {
        request: parsed.request,
        message: parsed.message,
        outer: parsed.outer,
        proposal: parsed.proposal,
        groups,
        votes,
    })
}

fn encoded_size(frame: Frame<'_>, limits: Limits) -> Result<usize> {
    let max_bytes = limits.max_frame_bytes()?;
    let max_positions = limits.max_positions()?;
    if frame.groups.len() > max_positions {
        return Err(Error::Invalid);
    }
    let mut positions = usize::from(frame.outer.is_some());
    let mut size = FRAME_BYTES;
    if frame.outer.is_some() {
        add_size(&mut size, VOTE_BYTES)?;
    }
    if let Some(p) = frame.proposal {
        if p.body.len() > limits.max_body {
            return Err(Error::Invalid);
        }
        add_size(&mut size, PROPOSAL_BYTES)?;
        add_size(&mut size, p.body.len())?;
    }
    for g in frame.groups {
        if g.votes.storage_len() > limits.max_members {
            return Err(Error::Invalid);
        }
        let count = g.votes.len();
        positions = positions.checked_add(count).ok_or(Error::Exhausted)?;
        if positions > max_positions {
            return Err(Error::Invalid);
        }
        add_size(&mut size, GROUP_BYTES)?;
        add_size(
            &mut size,
            count.checked_mul(VOTE_BYTES).ok_or(Error::Exhausted)?,
        )?;
    }
    if size > max_bytes {
        return Err(Error::Invalid);
    }
    Ok(size)
}

struct Parsed<'a> {
    config: Option<Id>,
    body_len: Option<usize>,
    request: Option<u64>,
    message: Message,
    outer: Option<Vote>,
    proposal: Option<(Header, &'a [u8])>,
    groups: &'a [u8],
    group_count: usize,
    vote_count: usize,
}

fn preflight(bytes: &[u8], limits: Limits, outlined: bool) -> Result<Parsed<'_>> {
    let max = if outlined {
        limits.max_outlined_bytes()?
    } else {
        limits.max_frame_bytes()?
    };
    if bytes.len() > max {
        return Err(Error::Invalid);
    }
    let max_positions = limits.max_positions()?;
    let mut r = Reader::new(bytes);
    r.prefix(if outlined { OUTLINED_TAG } else { FRAME_TAG })?;
    let config = if outlined { Some(r.array()?) } else { None };
    let mut body_len = None;
    let request = r.option_u64()?;
    let message = match r.byte()? {
        0 => Message::Ordinary,
        1 => Message::Dec,
        _ => return Err(Error::Invalid),
    };
    let outer = if r.boolean()? { Some(r.vote()?) } else { None };
    let proposal = if r.boolean()? {
        let header = r.header()?;
        let length = r.count(limits.max_body)?;
        body_len = Some(length);
        Some((header, if outlined { &[] } else { r.take(length)? }))
    } else {
        None
    };
    let group_count = r.count(max_positions)?;
    let groups = r.bytes;
    let mut remaining = max_positions - usize::from(outer.is_some());
    let mut vote_count = 0usize;
    for _ in 0..group_count {
        let (_, count, encoded_votes) = r.group(limits.max_members, remaining)?;
        remaining = remaining.checked_sub(count).ok_or(Error::Invalid)?;
        vote_count = vote_count.checked_add(count).ok_or(Error::Exhausted)?;
        let mut vr = Reader::new(encoded_votes);
        for _ in 0..count {
            vr.vote()?;
        }
    }
    r.finish()?;
    Ok(Parsed {
        config,
        body_len,
        request,
        message,
        outer,
        proposal,
        groups,
        group_count,
        vote_count,
    })
}

fn add_size(total: &mut usize, bytes: usize) -> Result<()> {
    *total = total.checked_add(bytes).ok_or(Error::Exhausted)?;
    Ok(())
}

fn check_capacity<T>(count: usize) -> Result<()> {
    let bytes = count
        .checked_mul(std::mem::size_of::<T>())
        .ok_or(Error::Exhausted)?;
    if bytes > isize::MAX as usize {
        return Err(Error::Exhausted);
    }
    Ok(())
}

fn reserved<T>(count: usize) -> Result<Vec<T>> {
    let mut out = Vec::new();
    out.try_reserve_exact(count).map_err(|_| Error::Exhausted)?;
    Ok(out)
}

fn put_u64(out: &mut Vec<u8>, n: u64) {
    out.extend_from_slice(&n.to_le_bytes());
}
fn put_option_u64(out: &mut Vec<u8>, value: Option<u64>) {
    out.push(u8::from(value.is_some()));
    put_u64(out, value.unwrap_or(0));
}
fn put_option_id(out: &mut Vec<u8>, value: Option<Id>) {
    out.push(u8::from(value.is_some()));
    out.extend_from_slice(&value.unwrap_or([0; 32]));
}
fn vote_kind(kind: VoteKind) -> u8 {
    match kind {
        VoteKind::Enter => 0,
        VoteKind::Prevote => 1,
        VoteKind::Precommit => 2,
    }
}
fn put_vote(out: &mut Vec<u8>, vote: &Vote) {
    let v = vote.content;
    put_u64(out, v.height);
    put_u64(out, v.round);
    out.push(vote_kind(v.kind));
    put_option_id(out, v.id);
    out.extend_from_slice(&v.signer.to_le_bytes());
    out.extend_from_slice(&vote.signature);
}
fn put_header(out: &mut Vec<u8>, header: &Header) {
    let p = header.content;
    put_u64(out, p.height);
    put_u64(out, p.round);
    put_option_u64(out, p.valid_round);
    out.extend_from_slice(&p.id);
    out.extend_from_slice(&p.signer.to_le_bytes());
    out.extend_from_slice(&header.signature);
}
fn put_cert(out: &mut Vec<u8>, kind: CertKind) {
    let (tag, round, id, vote) = match kind {
        CertKind::Polka { round, id } => (0, round, id, 0),
        CertKind::Decision { round, id } => (1, round, id, 0),
        CertKind::Timeout { round } => (2, round, [0; 32], 0),
        CertKind::Jump { round } => (3, round, [0; 32], 0),
        CertKind::Row { round, kind } => (4, round, [0; 32], vote_kind(kind)),
    };
    out.push(tag);
    put_u64(out, round);
    out.extend_from_slice(&id);
    out.push(vote);
}

struct Reader<'a> {
    bytes: &'a [u8],
}
impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }
    fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        if count > self.bytes.len() {
            return Err(Error::Invalid);
        }
        let (taken, rest) = self.bytes.split_at(count);
        self.bytes = rest;
        Ok(taken)
    }
    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        self.take(N)?.try_into().map_err(|_| Error::Invalid)
    }
    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.array()?))
    }
    fn boolean(&mut self) -> Result<bool> {
        match self.byte()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(Error::Invalid),
        }
    }
    fn prefix(&mut self, tag: u8) -> Result<()> {
        if self.byte()? != tag || self.byte()? != VERSION {
            return Err(Error::Invalid);
        }
        Ok(())
    }
    fn finish(&self) -> Result<()> {
        if self.bytes.is_empty() {
            Ok(())
        } else {
            Err(Error::Invalid)
        }
    }
    fn count(&mut self, max: usize) -> Result<usize> {
        let count = usize::try_from(self.u64()?).map_err(|_| Error::Invalid)?;
        if count > max {
            return Err(Error::Invalid);
        }
        Ok(count)
    }
    fn option_u64(&mut self) -> Result<Option<u64>> {
        let present = self.boolean()?;
        let value = self.u64()?;
        if !present && value != 0 {
            return Err(Error::Invalid);
        }
        Ok(present.then_some(value))
    }
    fn option_id(&mut self) -> Result<Option<Id>> {
        let present = self.boolean()?;
        let value = self.array()?;
        if !present && value != [0; 32] {
            return Err(Error::Invalid);
        }
        Ok(present.then_some(value))
    }
    fn vote_kind(&mut self) -> Result<VoteKind> {
        match self.byte()? {
            0 => Ok(VoteKind::Enter),
            1 => Ok(VoteKind::Prevote),
            2 => Ok(VoteKind::Precommit),
            _ => Err(Error::Invalid),
        }
    }
    fn vote(&mut self) -> Result<Vote> {
        Ok(Vote {
            content: VoteContent {
                height: self.u64()?,
                round: self.u64()?,
                kind: self.vote_kind()?,
                id: self.option_id()?,
                signer: u32::from_le_bytes(self.array()?),
            },
            signature: self.array()?,
        })
    }
    fn header(&mut self) -> Result<Header> {
        Ok(Header {
            content: ProposalContent {
                height: self.u64()?,
                round: self.u64()?,
                valid_round: self.option_u64()?,
                id: self.array()?,
                signer: u32::from_le_bytes(self.array()?),
            },
            signature: self.array()?,
        })
    }
    fn cert(&mut self) -> Result<CertKind> {
        let tag = self.byte()?;
        let round = self.u64()?;
        let id = self.array()?;
        let kind = self.vote_kind()?;
        match tag {
            0 if kind == VoteKind::Enter => Ok(CertKind::Polka { round, id }),
            1 if kind == VoteKind::Enter => Ok(CertKind::Decision { round, id }),
            2 if kind == VoteKind::Enter && id == [0; 32] => Ok(CertKind::Timeout { round }),
            3 if kind == VoteKind::Enter && id == [0; 32] => Ok(CertKind::Jump { round }),
            4 if id == [0; 32] => Ok(CertKind::Row { round, kind }),
            _ => Err(Error::Invalid),
        }
    }
    fn group(
        &mut self,
        max_members: usize,
        remaining: usize,
    ) -> Result<(CertKind, usize, &'a [u8])> {
        let kind = self.cert()?;
        let count = self.count(max_members.min(remaining))?;
        let size = count.checked_mul(VOTE_BYTES).ok_or(Error::Invalid)?;
        Ok((kind, count, self.take(size)?))
    }
}

#[cfg(test)]
mod payload_tests {
    use super::*;
    const LIMITS: Limits = Limits {
        max_members: 2,
        max_body: 4096,
    };
    fn request() -> BodyRequest {
        BodyRequest {
            token: 7,
            generation: 9,
            key: BodyKey {
                config: [1; 32],
                height: 3,
                id: [2; 32],
            },
        }
    }
    #[test]
    fn outlined_preserves_exact_header_and_metadata_without_body() {
        let body = vec![77; 4096];
        let header = Header {
            content: ProposalContent {
                height: 3,
                round: 5,
                valid_round: Some(2),
                id: [3; 32],
                signer: 1,
            },
            signature: [4; 64],
        };
        let frame = Frame {
            message: Message::Dec,
            outer: None,
            proposal: Some(Proposal {
                header: &header,
                body: &body,
            }),
            groups: &[],
        };
        let inline = encode_frame(frame, Some(4), LIMITS).unwrap();
        let bytes = encode_outlined(frame, Some(4), [1; 32], LIMITS).unwrap();
        assert_eq!(bytes.len() + body.len(), inline.len() + 32);
        let decoded = decode_outlined(&bytes, LIMITS).unwrap();
        assert_eq!(decoded.header(), Some(&header));
        assert_eq!(decoded.body_len, Some(body.len()));
        decoded.with_body(&body, |f| {
            assert_eq!(encode_frame(f, Some(4), LIMITS).unwrap(), inline)
        });
        for end in 0..bytes.len() {
            assert!(decode_outlined(&bytes[..end], LIMITS).is_err());
        }
        let mut extra = bytes.clone();
        extra.push(0);
        assert!(decode_outlined(&extra, LIMITS).is_err());
        let mut oversized = bytes;
        // No outer vote: header starts at 46, then its body-length hint.
        oversized[171..179].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(decode_outlined(&oversized, LIMITS).is_err());
    }
    #[test]
    fn body_records_are_borrowed_exact_and_bounded() {
        let req = request();
        let bytes = encode_body_request(req).unwrap();
        assert_eq!(bytes.len(), BODY_CONTROL_BYTES);
        assert_eq!(decode_body_request(&bytes).unwrap(), req);
        assert_eq!(
            decode_body_ack(&encode_body_ack(req).unwrap()).unwrap(),
            req
        );
        let body = vec![8; LIMITS.max_body];
        let bytes = encode_body_response(req, &body, LIMITS).unwrap();
        let (parsed, borrowed) = decode_body_response(&bytes, LIMITS).unwrap();
        assert_eq!(parsed, req);
        assert_eq!(borrowed, body);
        assert_eq!(borrowed.as_ptr(), bytes[BODY_RESPONSE_OVERHEAD..].as_ptr());
        for end in 0..bytes.len() {
            assert!(decode_body_response(&bytes[..end], LIMITS).is_err());
        }
        let mut extra = bytes;
        extra.push(0);
        assert!(decode_body_response(&extra, LIMITS).is_err());
        assert!(encode_body_response(req, &vec![8; LIMITS.max_body + 1], LIMITS).is_err());
        assert!(encode_body_request(BodyRequest { token: 0, ..req }).is_err());
    }
}
