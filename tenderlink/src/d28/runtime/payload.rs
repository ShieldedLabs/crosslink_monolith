//! Runtime-only payload transport. Consensus still receives complete frames.
use super::{
    AtomicMessageCounts, Engine, Error, Events, Frame, MessageCounts, Network, NetworkThreadPush,
    Peer, RuntimeConfig, RuntimeMetrics, Status, Storage, canonical_body, core_error,
    record_conflicts, saturating_add, tolerate_stale, wire,
};
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use wire::{BodyKey, BodyRequest, DecodedOutlined};

/// Payload transport is opt-in; ledger bytes, body IDs and signatures do not change.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PayloadMode {
    #[default]
    Inline,
    Outlined,
    /// Reserved for the separately validated chunked transport implementation.
    Merkle {
        leaf_bytes: usize,
    },
}

/// Cumulative application offers, not transport delivery or peer custody.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PayloadMetricsSnapshot {
    /// Successful retained-body lookups for outlined evidence, including repeated
    /// outlines and deferred evidence completed after another local path filled it.
    pub body_cache_hits: u64,
    /// Failed lookups per authenticated outline, not distinct/admitted bodies.
    pub body_cache_misses: u64,
    pub body_requests: u64,
    pub body_responses: u64,
    /// Raw body bytes offered per recipient, including bodies in inline frames.
    /// A subset of encoded evidence/data bytes; never add it to total wire bytes.
    pub payload_bytes: u64,
    /// Locally known repeat raw-byte offers of the most recently offered body to
    /// that peer in the current height/session. Bounded one-ID history per peer;
    /// not a count of global duplicates, received duplicates, or delivery.
    pub duplicate_payload_bytes: u64,
    /// Subset of `offered.evidence.bytes`, never additional wire bytes.
    pub outlined_evidence_bytes: u64,
    /// Body request/ACK application records, separate from evidence and status.
    pub offered_control: MessageCounts,
    /// Encoded body responses, separate from evidence and status.
    pub offered_data: MessageCounts,
}

#[derive(Debug, Default)]
pub(super) struct AtomicPayloadMetrics {
    body_cache_hits: AtomicU64,
    body_cache_misses: AtomicU64,
    body_requests: AtomicU64,
    body_responses: AtomicU64,
    payload_bytes: AtomicU64,
    duplicate_payload_bytes: AtomicU64,
    outlined_evidence_bytes: AtomicU64,
    offered_control: AtomicMessageCounts,
    offered_data: AtomicMessageCounts,
}
impl AtomicPayloadMetrics {
    pub(super) fn cache(&self, hit: bool) {
        saturating_add(
            if hit {
                &self.body_cache_hits
            } else {
                &self.body_cache_misses
            },
            1,
        );
    }
    pub(super) fn offer(&self, counts: PayloadMetricsSnapshot) {
        saturating_add(&self.body_requests, counts.body_requests);
        saturating_add(&self.body_responses, counts.body_responses);
        saturating_add(&self.payload_bytes, counts.payload_bytes);
        saturating_add(
            &self.duplicate_payload_bytes,
            counts.duplicate_payload_bytes,
        );
        saturating_add(
            &self.outlined_evidence_bytes,
            counts.outlined_evidence_bytes,
        );
        self.offered_control.add(counts.offered_control);
        self.offered_data.add(counts.offered_data);
    }
    pub(super) fn snapshot(&self) -> PayloadMetricsSnapshot {
        PayloadMetricsSnapshot {
            body_cache_hits: self.body_cache_hits.load(Ordering::Relaxed),
            body_cache_misses: self.body_cache_misses.load(Ordering::Relaxed),
            body_requests: self.body_requests.load(Ordering::Relaxed),
            body_responses: self.body_responses.load(Ordering::Relaxed),
            payload_bytes: self.payload_bytes.load(Ordering::Relaxed),
            duplicate_payload_bytes: self.duplicate_payload_bytes.load(Ordering::Relaxed),
            outlined_evidence_bytes: self.outlined_evidence_bytes.load(Ordering::Relaxed),
            offered_control: self.offered_control.snapshot(),
            offered_data: self.offered_data.snapshot(),
        }
    }
}

#[derive(Clone, Copy)]
struct TimedRequest {
    request: BodyRequest,
    until: u64,
}
struct BodyPeer {
    member: u32,
    generation: u64,
    // Scalar hints only, never evidence or proof of body custody.
    hint: Option<BodyKey>,
    sent: Option<TimedRequest>,
    issued_through: u64, // token high-water fence; not a promise of body custody
    requested: Option<TimedRequest>,
    remote_generation: Option<u64>,
    remote_token: u64,
    lease: Option<TimedRequest>,
    ack: Option<BodyRequest>,
    last_offer: Option<BodyKey>,
}
struct Pending {
    key: BodyKey,
    evidence: DecodedOutlined,
    expires: u64,
    attempt: Option<(u32, u64, u64)>, // source/session/retry deadline, not metadata token
    cursor: u32,
}
/// Separate ownership from Receiver/Responder permits: global K pending
/// evidence records (O(K*N)), O(N) peer scalars, and at most K_tx body leases.
/// No second long-lived body cache: complete response bytes are borrowed and
/// retained engine bodies are consulted before making one temporary RX copy.
#[derive(Default)]
pub(super) struct PayloadState {
    scope: Option<(super::Id, u64)>,
    peers: Vec<BodyPeer>,
    pending: Vec<Pending>,
    token: u64,
    serve_cursor: usize,
}
impl PayloadState {
    pub(super) fn clear(&mut self) {
        self.scope = None;
        self.peers.clear();
        self.pending.clear();
        // Tokens never reset within this runtime, even across height retirement.
    }
    pub(super) fn sync<S: Storage>(&mut self, engine: &Engine<S>, peers: &[Peer], now: u64) {
        let scope = engine
            .view()
            .filter(|_| engine.is_active())
            .map(|d| (d.config, d.height));
        if scope != self.scope {
            self.clear();
            self.scope = scope;
        }
        self.peers.retain(|p| {
            peers
                .iter()
                .any(|q| q.member == Some(p.member) && q.generation == p.generation)
        });
        for peer in peers {
            let Some(member) = peer.member else { continue };
            if !self
                .peers
                .iter()
                .any(|p| p.member == member && p.generation == peer.generation)
            {
                self.peers.push(BodyPeer {
                    member,
                    generation: peer.generation,
                    hint: None,
                    sent: None,
                    issued_through: 0,
                    requested: None,
                    remote_generation: None,
                    remote_token: 0,
                    lease: None,
                    ack: None,
                    last_offer: None,
                });
            }
        }
        for peer in &mut self.peers {
            if peer.lease.is_some_and(|r| now >= r.until) {
                peer.lease = None;
            }
            if peer.requested.is_some_and(|r| now >= r.until) {
                peer.requested = None;
            }
            if peer.sent.is_some_and(|r| now >= r.until) {
                peer.sent = None;
            }
        }
        self.pending.retain(|p| now < p.expires);
        for p in &mut self.pending {
            if p.attempt.is_some_and(|(member, generation, until)| {
                now >= until
                    || !self
                        .peers
                        .iter()
                        .any(|q| q.member == member && q.generation == generation)
            }) {
                p.attempt = None;
            }
        }
    }
    fn in_scope(&self, key: BodyKey) -> bool {
        self.scope == Some((key.config, key.height))
    }
    fn peer_mut(&mut self, member: u32, generation: u64) -> Option<&mut BodyPeer> {
        self.peers
            .iter_mut()
            .find(|p| p.member == member && p.generation == generation)
    }
    pub(super) fn status_hint(
        &mut self,
        member: u32,
        generation: u64,
        status: super::StatusSnapshot,
    ) {
        if self.scope != Some((status.config, status.height)) {
            return;
        }
        if let Some(peer) = self.peer_mut(member, generation) {
            peer.hint = status
                .proposal
                .or(status.polka.map(|(_, id)| id))
                .map(|id| BodyKey {
                    config: status.config,
                    height: status.height,
                    id,
                });
        }
    }
    fn defer(
        &mut self,
        key: BodyKey,
        evidence: DecodedOutlined,
        member: u32,
        generation: u64,
        config: &RuntimeConfig,
        now: u64,
    ) {
        if let Some(peer) = self.peer_mut(member, generation) {
            peer.hint = Some(key);
        }
        if let Some(pending) = self.pending.iter_mut().find(|p| p.key == key) {
            // One record per body, not one per peer/certificate. Keep the stronger
            // decision/round package; no copies of votes are merged or fabricated.
            if evidence.priority() >= pending.evidence.priority() {
                pending.evidence = evidence;
            }
            return;
        }
        if self.pending.len() == config.max_requests {
            return;
        }
        self.pending.push(Pending {
            key,
            evidence,
            expires: now.saturating_add(
                config.status_ttl.max(
                    config
                        .request_timeout
                        .saturating_mul(config.max_members as u64 + 1),
                ),
            ),
            attempt: None,
            cursor: member,
        });
    }
    pub(super) fn body_offer(
        &mut self,
        member: u32,
        generation: u64,
        key: BodyKey,
        len: usize,
        counts: &mut PayloadMetricsSnapshot,
    ) {
        counts.payload_bytes = counts.payload_bytes.saturating_add(len as u64);
        if let Some(peer) = self.peer_mut(member, generation) {
            if peer.last_offer == Some(key) {
                counts.duplicate_payload_bytes =
                    counts.duplicate_payload_bytes.saturating_add(len as u64);
            }
            peer.last_offer = Some(key);
        }
    }
    pub(super) fn offer<S: Storage>(
        &mut self,
        engine: &Engine<S>,
        peers: &[Peer],
        bulk_ready: impl Fn(super::ConnectionKey) -> bool,
        config: &RuntimeConfig,
        now: u64,
        push: &mut NetworkThreadPush,
        counts: &mut PayloadMetricsSnapshot,
    ) -> io::Result<()> {
        // ACKs and missing-body requests always use BftLive, independent of Bulk.
        for peer in &mut self.peers {
            let Some(address) = peers
                .iter()
                .find(|p| p.member == Some(peer.member) && p.generation == peer.generation)
                .map(|p| p.address.connection_key())
            else {
                continue;
            };
            if let Some(ack) = peer.ack.take() {
                let bytes = wire::encode_body_ack(ack).map_err(core_error)?;
                counts.offered_control.record(bytes.len());
                push.send_unreliable.push((address, bytes));
            }
        }
        let mut busy: Vec<_> = self
            .pending
            .iter()
            .filter_map(|p| {
                p.attempt
                    .map(|(member, generation, _)| (member, generation))
            })
            .collect();
        for pending in &mut self.pending {
            if pending.attempt.is_some() || retained_body(engine, pending.key).is_some() {
                // Acquisition/another receive path may have filled this since
                // deferral. The next receive turn materializes it under RX budget.
                continue;
            }
            // Prefer the advertising source on the first attempt. Subsequent
            // attempts rotate through every authenticated source, including peers
            // whose status has not advertised this body. Hints are not proof.
            let candidate = self
                .peers
                .iter_mut()
                .filter(|p| !busy.contains(&(p.member, p.generation)))
                .min_by_key(|p| (p.member < pending.cursor, p.member));
            let Some(peer) = candidate else { continue };
            self.token = self
                .token
                .checked_add(1)
                .ok_or_else(|| core_error(Error::Exhausted))?;
            let request = BodyRequest {
                token: self.token,
                generation: peer.generation,
                key: pending.key,
            };
            let Some(address) = peers
                .iter()
                .find(|p| p.member == Some(peer.member) && p.generation == peer.generation)
                .map(|p| p.address.connection_key())
            else {
                continue;
            };
            let bytes = wire::encode_body_request(request).map_err(core_error)?;
            counts.body_requests = counts.body_requests.saturating_add(1);
            counts.offered_control.record(bytes.len());
            push.send_unreliable.push((address, bytes));
            // Keep bounded provenance, separate from active source occupancy.
            // Older issued tokens can satisfy an existing scoped interest; they
            // cannot create a new interest or authorize a future-height body.
            peer.issued_through = request.token;
            peer.sent = Some(TimedRequest {
                request,
                until: pending.expires,
            });
            busy.push((peer.member, peer.generation));
            pending.attempt = Some((
                peer.member,
                peer.generation,
                now.saturating_add(config.request_timeout),
            ));
            pending.cursor = peer.member.saturating_add(1);
        }
        let n = self.peers.len();
        let mut active = self.peers.iter().filter(|p| p.lease.is_some()).count();
        let start = self.serve_cursor;
        for offset in 0..n {
            if active >= config.max_outgoing {
                break;
            }
            let index = (start + offset) % n;
            let peer = &mut self.peers[index];
            if peer.lease.is_some() {
                continue;
            }
            let Some(wanted) = peer.requested else {
                continue;
            };
            let Some(connection) = peers
                .iter()
                .find(|p| p.member == Some(peer.member) && p.generation == peer.generation)
                .map(|p| p.address.connection_key())
            else {
                continue;
            };
            // An expired lease is not proof STP released its bytes. Do not queue
            // another full body while any previous Bulk copy is still in flight.
            if !bulk_ready(connection) {
                continue;
            }
            let Some(body) = retained_body(engine, wanted.request.key) else {
                continue;
            };
            let bytes = wire::encode_body_response(wanted.request, body, config.limits())
                .map_err(core_error)?;
            counts.body_responses = counts.body_responses.saturating_add(1);
            counts.offered_data.record(bytes.len());
            let (member, generation) = (peer.member, peer.generation);
            peer.requested = None;
            peer.lease = Some(TimedRequest {
                request: wanted.request,
                until: now.saturating_add(config.request_timeout),
            });
            self.body_offer(member, generation, wanted.request.key, body.len(), counts);
            push.send_bulk.push((connection, bytes));
            active += 1;
            self.serve_cursor = (index + 1) % n;
        }
        Ok(())
    }
}

/// Look up ONLY descriptor-retained roles, not stale bytes left in a store slot.
/// Body equality never substitutes an older header or external validity tag.
fn retained_body<S: Storage>(engine: &Engine<S>, key: BodyKey) -> Option<&[u8]> {
    if !engine.is_active() {
        return None;
    }
    let d = engine.view()?;
    if d.config != key.config || d.height != key.height {
        return None;
    }
    for slot in [
        d.slot,
        d.vc.map(|v| v.proposal),
        d.pending.map(|p| p.proposal),
    ]
    .into_iter()
    .flatten()
    {
        let header = engine.storage().header(slot.index)?;
        if header.content.height == key.height && header.content.id == key.id {
            return Some(engine.storage().body(slot.index));
        }
    }
    None
}

pub(super) fn complete<S: Storage>(
    engine: &mut Engine<S>,
    frame: Frame<'_>,
    events: &mut Events,
    conflicts: &mut Vec<super::VoteConflict>,
    config: &RuntimeConfig,
) -> io::Result<bool> {
    if frame
        .proposal
        .is_some_and(|p| !canonical_body(p.body, p.header.content.height, config.max_body))
    {
        return Ok(false);
    }
    record_conflicts(engine, frame, conflicts, config.max_members);
    let result = engine.receive(frame, Status::Indeterminate, events);
    let processed = result.is_ok();
    tolerate_stale(result)?;
    Ok(processed)
}

impl Network {
    /// Deferred evidence is bounded by K, but completing it still spends the
    /// same per-tick verification budget and one RX body permit as a new frame.
    pub(super) fn complete_cached_payloads<S: Storage>(
        &mut self,
        engine: &mut Engine<S>,
        config: &RuntimeConfig,
        events: &mut Events,
    ) -> io::Result<usize> {
        let mut completed = 0;
        while completed < config.max_receive_per_tick {
            let Some((index, body)) = self
                .payload
                .pending
                .iter()
                .enumerate()
                .find_map(|(i, pending)| retained_body(engine, pending.key).map(|body| (i, body)))
            else {
                break;
            };
            let Some(_permit) = self.incoming.try_acquire() else {
                break;
            };
            let body = body.to_vec();
            let pending = self.payload.pending.remove(index);
            if let Some(metrics) = &config.metrics {
                metrics.payload.cache(true);
            }
            pending.evidence.with_body(&body, |frame| {
                complete(engine, frame, events, &mut self.conflicts, config)
            })?;
            completed += 1;
        }
        Ok(completed)
    }

    pub(super) fn receive_payload<S: Storage>(
        &mut self,
        engine: &mut Engine<S>,
        config: &RuntimeConfig,
        events: &mut Events,
        member: u32,
        generation: u64,
        bytes: &[u8],
        now: u64,
    ) -> io::Result<bool> {
        if config.payload_mode == PayloadMode::Inline {
            return Ok(false);
        }
        match bytes.first() {
            Some(&wire::OUTLINED_TAG) => {
                let Ok(evidence) = wire::decode_outlined(bytes, config.limits()) else {
                    return Ok(false);
                };
                let Some(d) = engine.view() else {
                    return Ok(false);
                };
                if evidence.config != d.config {
                    return Ok(false);
                }
                let Some(header) = evidence.header() else {
                    return evidence.with_body(&[], |f| {
                        complete(engine, f, events, &mut self.conflicts, config)
                    });
                };
                if header.content.height != d.height || !header.verify(engine.c()) {
                    return Ok(false);
                }
                let key = BodyKey {
                    config: d.config,
                    height: d.height,
                    id: header.content.id,
                };
                if let Some(body) = retained_body(engine, key) {
                    if let Some(metrics) = &config.metrics {
                        metrics.payload.cache(true);
                    }
                    // One temporary RX body copy, under the existing incoming
                    // permit; the core still verifies the hash/header/certificates.
                    let body = body.to_vec();
                    let evidence = if let Some(index) =
                        self.payload.pending.iter().position(|p| p.key == key)
                    {
                        let pending = self.payload.pending.remove(index);
                        if pending.evidence.priority() > evidence.priority() {
                            pending.evidence
                        } else {
                            evidence
                        }
                    } else {
                        evidence
                    };
                    return evidence.with_body(&body, |f| {
                        complete(engine, f, events, &mut self.conflicts, config)
                    });
                }
                if let Some(metrics) = &config.metrics {
                    metrics.payload.cache(false);
                }
                self.payload
                    .defer(key, evidence, member, generation, config, now);
                Ok(true)
            }
            Some(&wire::BODY_REQUEST_TAG) => {
                let Ok(request) = wire::decode_body_request(bytes) else {
                    return Ok(false);
                };
                if !self.payload.in_scope(request.key) {
                    return Ok(false);
                }
                let Some(peer) = self.payload.peer_mut(member, generation) else {
                    return Ok(false);
                };
                if peer
                    .remote_generation
                    .is_some_and(|g| g != request.generation)
                    || request.token <= peer.remote_token
                {
                    return Ok(false);
                }
                peer.remote_generation = Some(request.generation);
                peer.remote_token = request.token;
                // A new metadata/request token never frees an existing body lease.
                peer.requested = Some(TimedRequest {
                    request,
                    until: now.saturating_add(config.status_ttl),
                });
                Ok(true)
            }
            Some(&wire::BODY_ACK_TAG) => {
                let Ok(request) = wire::decode_body_ack(bytes) else {
                    return Ok(false);
                };
                if !self.payload.in_scope(request.key) {
                    return Ok(false);
                }
                let Some(peer) = self.payload.peer_mut(member, generation) else {
                    return Ok(false);
                };
                if !peer.lease.is_some_and(|l| l.request == request) {
                    return Ok(false);
                }
                peer.lease = None;
                // A receipt for token t1 says nothing about a newer t2 request.
                if peer.requested.is_some_and(|r| r.request == request) {
                    peer.requested = None;
                }
                Ok(true)
            }
            Some(&wire::BODY_RESPONSE_TAG) => {
                let Ok((request, body)) = wire::decode_body_response(bytes, config.limits()) else {
                    return Ok(false);
                };
                if !self.payload.in_scope(request.key) || request.generation != generation {
                    return Ok(false);
                }
                let Some(peer) = self.payload.peer_mut(member, generation) else {
                    return Ok(false);
                };
                if request.token > peer.issued_through {
                    return Ok(false);
                }
                let recent = peer
                    .sent
                    .is_some_and(|s| s.request.key == request.key && now < s.until);
                let pending_index = self
                    .payload
                    .pending
                    .iter()
                    .position(|p| p.key == request.key);
                if pending_index.is_none() && !recent {
                    return Ok(false);
                }
                if !canonical_body(body, request.key.height, config.max_body)
                    || engine.c().body_id(body) != request.key.id
                {
                    if let Some(pending) = self
                        .payload
                        .pending
                        .iter_mut()
                        .find(|p| p.key == request.key)
                    {
                        if pending.attempt.is_some_and(|(source, session, _)| {
                            source == member && session == generation
                        }) {
                            pending.attempt = None;
                        }
                    }
                    return Ok(false);
                }
                let Some(index) = pending_index else {
                    // Duplicate valid response: useful for releasing a sender
                    // lease only if the body really is still retained locally.
                    if retained_body(engine, request.key).is_some() {
                        self.payload
                            .peer_mut(member, generation)
                            .expect("current source")
                            .ack = Some(request);
                        return Ok(true);
                    }
                    return Ok(false);
                };
                let pending = self.payload.pending.remove(index);
                self.payload
                    .peer_mut(member, generation)
                    .expect("current source")
                    .ack = Some(request);
                pending.evidence.with_body(body, |f| {
                    complete(engine, f, events, &mut self.conflicts, config)
                })
            }
            _ => Ok(false),
        }
    }
}

#[cfg(test)]
mod tests;
