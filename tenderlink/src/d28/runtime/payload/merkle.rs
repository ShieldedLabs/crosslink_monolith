//! Bounded runtime leaf exchange, independent of consensus and STP packetization.
//!
//! Ownership: K contiguous assemblies plus K partial trees, <=3 retained-role
//! trees (NO extra retained bodies), K receive windows and K_tx send windows.
//! Each authenticated peer has one advertised bitmap and one sent-leaf bitmap,
//! bounded by ceil(ceil(max_body/min_leaf)/8), plus fixed eight-index windows.
//! These are component bounds, not an end-to-end memory/RSS proof: STP separately
//! owns its bounded queues, and complete-frame admission keeps its own permits.
use super::merkle_wire::{self as codec, Ack, Have, Reference, Ticket, WINDOW, Want};
use super::{
    BodyKey, Engine, Events, Network, NetworkThreadPush, PayloadMetricsSnapshot, PayloadState,
    Peer, Pending, RuntimeConfig, Storage, canonical_body, complete, core_error, retained_body,
};
use crate::d28::{
    Header,
    merkle::{Bitmap, HolderAttestation, HolderContext, PartialTree, Tree},
};
use ed25519_zebra::SigningKey;
use std::io;

fn error(error: crate::d28::merkle::Error) -> io::Error {
    io::Error::other(error)
}
fn context<S: Storage>(engine: &Engine<S>, height: u64) -> HolderContext {
    HolderContext {
        domain: engine.c().domain(),
        config: engine.c().hash(),
        height,
    }
}
fn retained_headers<S: Storage>(engine: &Engine<S>) -> Vec<(BodyKey, Header)> {
    let Some(d) = engine.view().filter(|_| engine.is_active()) else {
        return Vec::new();
    };
    let mut headers = Vec::with_capacity(3);
    for slot in [
        d.slot,
        d.vc.map(|v| v.proposal),
        d.pending.map(|p| p.proposal),
    ]
    .into_iter()
    .flatten()
    {
        if let Some(header) = engine.storage().header(slot.index) {
            let key = BodyKey {
                config: d.config,
                height: d.height,
                id: header.content.id,
            };
            if !headers.contains(&(key, *header)) {
                headers.push((key, *header));
            }
        }
    }
    headers
}
struct Available {
    reference: Reference,
    holder: HolderAttestation,
    bitmap: Bitmap,
    until: u64,
}
#[derive(Clone, Copy)]
struct Window {
    want: Want,
    until: u64,
    mask: u8,
}
struct LeafPeer {
    member: u32,
    generation: u64,
    available: Option<Available>,
    receive: Option<Window>,
    issued_through: u64,
    requested: Option<Window>,
    lease: Option<Window>,
    remote_generation: Option<u64>,
    remote_token: u64,
    acks: [Option<Ack>; WINDOW],
    // One body/geometry history per peer, not one body-sized allocation per peer.
    offered: Option<(Reference, Bitmap)>,
    failed: Option<(Reference, u64)>,
}
// Header identity is deliberately excluded only for reusing already verified
// BYTES under an authenticated replacement header. No validity tag is reused.
fn same_payload(a: Reference, b: Reference) -> bool {
    a.key() == b.key()
        && a.manifest.root == b.manifest.root
        && a.manifest.body_len == b.manifest.body_len
        && a.manifest.leaf_bytes == b.manifest.leaf_bytes
}
struct Assembly {
    reference: Reference,
    holder: HolderAttestation,
    tree: PartialTree,
    body: Vec<u8>,
    expires: u64,
    retry_at: u64,
}
struct Cached {
    key: BodyKey,
    tree: Tree,
    // At most three descriptor roles can name exact headers for the same body.
    headers: Vec<(Reference, HolderAttestation)>,
    bitmap: Vec<u8>,
}
#[derive(Default)]
pub(super) struct MerkleState {
    peers: Vec<LeafPeer>,
    assemblies: Vec<Assembly>,
    cached: Vec<Cached>,
    token: u64,
    pending_cursor: usize,
    source_cursor: usize,
    serve_cursor: usize,
    advertise_at: u64,
}
impl MerkleState {
    pub(super) fn clear(&mut self) {
        self.peers.clear();
        self.assemblies.clear();
        self.cached.clear();
        self.advertise_at = 0;
        // Never reset token high-water fencing, even across height retirement.
    }
    pub(super) fn sync<S: Storage>(
        &mut self,
        pending: &[Pending],
        engine: &Engine<S>,
        peers: &[Peer],
        now: u64,
    ) {
        self.peers.retain(|p| {
            peers
                .iter()
                .any(|q| q.member == Some(p.member) && q.generation == p.generation)
        });
        for p in peers {
            let Some(member) = p.member else { continue };
            if !self
                .peers
                .iter()
                .any(|q| q.member == member && q.generation == p.generation)
            {
                self.peers.push(LeafPeer {
                    member,
                    generation: p.generation,
                    available: None,
                    receive: None,
                    issued_through: 0,
                    requested: None,
                    lease: None,
                    remote_generation: None,
                    remote_token: 0,
                    acks: [None; WINDOW],
                    offered: None,
                    failed: None,
                });
            }
        }
        self.assemblies
            .retain(|a| now < a.expires && pending.iter().any(|p| p.key == a.reference.key()));
        self.cached
            .retain(|c| retained_body(engine, c.key).is_some());
        // A different attested root for the same interest gets a bounded turn.
        // Do not discard useful partial data merely because ONE holder failed.
        let alternatives: Vec<_> = self
            .assemblies
            .iter()
            .filter(|a| {
                now >= a.retry_at
                    && self.peers.iter().any(|p| {
                        p.available.as_ref().is_some_and(|other| {
                            other.reference.key() == a.reference.key()
                                && !same_payload(other.reference, a.reference)
                                && other.bitmap.count() != 0
                                && now < other.until
                        })
                    })
            })
            .map(|a| (a.reference, a.expires))
            .collect();
        for (reference, until) in alternatives {
            self.reject_reference(reference, until);
        }
        for peer in &mut self.peers {
            if peer.available.as_ref().is_some_and(|a| {
                now >= a.until || !pending.iter().any(|p| p.key == a.reference.key())
            }) {
                peer.available = None;
            }
            if peer.failed.is_some_and(|(_, until)| now >= until) {
                peer.failed = None;
            }
            if peer.requested.is_some_and(|w| now >= w.until) {
                peer.requested = None;
            }
            if peer.lease.is_some_and(|w| now >= w.until) {
                peer.lease = None;
            }
            if peer.receive.is_some_and(|w| {
                now >= w.until
                    || !self
                        .assemblies
                        .iter()
                        .any(|a| same_payload(a.reference, w.want.ticket.reference))
            }) {
                peer.receive = None;
            }
        }
    }
    fn refresh_cache<S: Storage>(
        &mut self,
        engine: &Engine<S>,
        config: &RuntimeConfig,
        key: &SigningKey,
        leaf_bytes: usize,
    ) -> io::Result<()> {
        let headers = retained_headers(engine);
        self.cached
            .retain(|c| headers.iter().any(|(k, _)| *k == c.key));
        for (body_key, header) in &headers {
            if !self.cached.iter().any(|c| c.key == *body_key) {
                let Some(body) = retained_body(engine, *body_key) else {
                    continue;
                };
                let tree = Tree::new(body, leaf_bytes, config.max_body).map_err(error)?;
                let mut bitmap = vec![255; tree.layout().bitmap_bytes()];
                if tree.leaf_count() % 8 != 0 {
                    *bitmap.last_mut().expect("nonempty tree") = (1 << (tree.leaf_count() % 8)) - 1;
                }
                self.cached.push(Cached {
                    key: *body_key,
                    tree,
                    headers: Vec::new(),
                    bitmap,
                });
            }
            let cached = self
                .cached
                .iter_mut()
                .find(|c| c.key == *body_key)
                .expect("inserted tree");
            cached.headers.retain(|(r, _)| {
                headers.iter().any(|(k, h)| {
                    *k == *body_key && h.identity(engine.c()) == r.manifest.signed_header_identity
                })
            });
            let reference = Reference {
                config: body_key.config,
                height: body_key.height,
                manifest: cached
                    .tree
                    .manifest(body_key.id, header.identity(engine.c())),
            };
            if !cached.headers.iter().any(|(r, _)| *r == reference) {
                let holder = HolderAttestation::sign(
                    &context(engine, body_key.height),
                    &reference.manifest,
                    key,
                    config.max_body,
                )
                .map_err(error)?;
                cached.headers.push((reference, holder));
            }
        }
        Ok(())
    }
    fn leaf<'a, S: Storage>(
        &'a self,
        engine: &'a Engine<S>,
        reference: Reference,
        index: u32,
    ) -> Option<(&'a [u8], Vec<[u8; 32]>)> {
        if let Some(c) = self
            .cached
            .iter()
            .find(|c| c.headers.iter().any(|(r, _)| *r == reference))
        {
            let body = retained_body(engine, c.key)?;
            return Some((
                &body[c.tree.layout().leaf_range(index as usize).ok()?],
                c.tree.proof(index as usize).ok()?,
            ));
        }
        let a = self.assemblies.iter().find(|a| a.reference == reference)?;
        Some((
            &a.body[a.tree.layout().leaf_range(index as usize).ok()?],
            a.tree.proof(index as usize).ok()?,
        ))
    }
    fn reject_reference(&mut self, reference: Reference, until: u64) {
        self.assemblies
            .retain(|a| !same_payload(a.reference, reference));
        for peer in &mut self.peers {
            if peer
                .available
                .as_ref()
                .is_some_and(|a| same_payload(a.reference, reference))
            {
                peer.available = None;
                peer.failed = Some((reference, until));
            }
            if peer
                .receive
                .is_some_and(|w| same_payload(w.want.ticket.reference, reference))
            {
                peer.receive = None;
            }
        }
    }
    fn start_assemblies(
        &mut self,
        pending: &[Pending],
        config: &RuntimeConfig,
        now: u64,
    ) -> io::Result<()> {
        let n = pending.len();
        let start = self.pending_cursor;
        for offset in 0..n {
            if self.assemblies.len() >= config.max_requests {
                break;
            }
            let index = (start + offset) % n;
            let p = &pending[index];
            if self.assemblies.iter().any(|a| a.reference.key() == p.key) {
                continue;
            }
            let count = self.peers.len();
            let candidate = (0..count)
                .map(|i| (self.source_cursor + i) % count)
                .find(|&i| {
                    self.peers[i].available.as_ref().is_some_and(|a| {
                        a.reference.key() == p.key && a.bitmap.count() != 0 && now < a.until
                    })
                });
            let Some(source) = candidate else { continue };
            let available = self.peers[source]
                .available
                .as_ref()
                .expect("selected holder");
            let tree =
                PartialTree::new(&available.reference.manifest, config.max_body).map_err(error)?;
            let mut body = Vec::new();
            body.try_reserve_exact(tree.layout().body_len())
                .map_err(|_| core_error(crate::d28::Error::Exhausted))?;
            body.resize(tree.layout().body_len(), 0);
            self.assemblies.push(Assembly {
                reference: available.reference,
                holder: available.holder,
                tree,
                body,
                expires: p.expires,
                retry_at: now.saturating_add(config.request_timeout),
            });
            self.source_cursor = (source + 1) % count;
            self.pending_cursor = (index + 1) % n;
        }
        Ok(())
    }
    fn offers<S: Storage>(
        &mut self,
        engine: &Engine<S>,
        pending: &[Pending],
        peers: &[Peer],
        bulk_ready: impl Fn(super::super::ConnectionKey) -> bool,
        config: &RuntimeConfig,
        now: u64,
        push: &mut NetworkThreadPush,
        counts: &mut PayloadMetricsSnapshot,
    ) -> io::Result<()> {
        // Periodic HAVE is deliberately repeated, including after an early HAVE
        // arrived before its outline. No new interest is created by HAVE alone.
        if now >= self.advertise_at {
            for peer in peers.iter().filter(|p| p.member.is_some()) {
                let address = peer.address.connection_key();
                for c in &self.cached {
                    for &(reference, holder) in &c.headers {
                        let bytes = codec::encode_have(
                            Have {
                                reference,
                                holder,
                                bitmap: &c.bitmap,
                            },
                            config.max_body,
                        )
                        .map_err(core_error)?;
                        counts.availability_bytes =
                            counts.availability_bytes.saturating_add(bytes.len() as u64);
                        counts.offered_control.record(bytes.len());
                        push.send_unreliable.push((address, bytes));
                    }
                }
                for a in &self.assemblies {
                    if a.tree.bitmap().count() == 0 {
                        continue;
                    }
                    let bytes = codec::encode_have(
                        Have {
                            reference: a.reference,
                            holder: a.holder,
                            bitmap: a.tree.bitmap().as_bytes(),
                        },
                        config.max_body,
                    )
                    .map_err(core_error)?;
                    counts.availability_bytes =
                        counts.availability_bytes.saturating_add(bytes.len() as u64);
                    counts.offered_control.record(bytes.len());
                    push.send_unreliable.push((address, bytes));
                }
            }
            self.advertise_at = now.saturating_add(super::super::millis(config.status_interval)?);
        }
        for peer in &mut self.peers {
            let Some(address) = peers
                .iter()
                .find(|p| p.member == Some(peer.member) && p.generation == peer.generation)
                .map(|p| p.address.connection_key())
            else {
                continue;
            };
            for ack in peer.acks.iter_mut().filter_map(Option::take) {
                let bytes = codec::encode_ack(ack, config.max_body).map_err(core_error)?;
                counts.offered_control.record(bytes.len());
                push.send_unreliable.push((address, bytes));
            }
        }
        self.start_assemblies(pending, config, now)?;
        let mut active = self.peers.iter().filter(|p| p.receive.is_some()).count();
        let n = self.peers.len();
        let start = self.source_cursor;
        for offset in 0..n {
            if active >= config.max_requests {
                break;
            }
            let index = (start + offset) % n;
            let peer = &self.peers[index];
            if peer.receive.is_some() {
                continue;
            }
            let Some(available) = &peer.available else {
                continue;
            };
            let Some(a) = self
                .assemblies
                .iter()
                .find(|a| same_payload(a.reference, available.reference))
            else {
                continue;
            };
            let mut indices = [0; WINDOW];
            let mut count = 0;
            for leaf in 0..a.tree.layout().leaf_count() {
                if count == WINDOW {
                    break;
                }
                if a.tree.bitmap().have(leaf).unwrap_or(false)
                    || !available.bitmap.have(leaf).unwrap_or(false)
                {
                    continue;
                }
                if self.peers.iter().filter_map(|p| p.receive).any(|w| {
                    same_payload(w.want.ticket.reference, a.reference)
                        && w.want.indices().contains(&(leaf as u32))
                }) {
                    continue;
                }
                indices[count] = leaf as u32;
                count += 1;
            }
            if count == 0 {
                continue;
            }
            let Some(address) = peers
                .iter()
                .find(|p| p.member == Some(peer.member) && p.generation == peer.generation)
                .map(|p| p.address.connection_key())
            else {
                continue;
            };
            self.token = self
                .token
                .checked_add(1)
                .ok_or_else(|| core_error(crate::d28::Error::Exhausted))?;
            let want = Want {
                ticket: Ticket {
                    reference: available.reference,
                    generation: peer.generation,
                    token: self.token,
                },
                indices,
                count: count as u8,
            };
            let bytes = codec::encode_want(want, config.max_body).map_err(core_error)?;
            // Acquire a fixed receive window before handing off a request.
            self.peers[index].receive = Some(Window {
                want,
                until: now.saturating_add(config.request_timeout),
                mask: 0,
            });
            self.peers[index].issued_through = self.token;
            counts.leaf_requests = counts.leaf_requests.saturating_add(count as u64);
            counts.offered_control.record(bytes.len());
            push.send_unreliable.push((address, bytes));
            active += 1;
            self.source_cursor = (index + 1) % n;
            // Advance on useful scheduling, not each timer tick: periodic HAVE
            // sampling must not alias a fast cursor and starve a second body.
            if let Some(position) = pending
                .iter()
                .position(|p| p.key == want.ticket.reference.key())
            {
                self.pending_cursor = (position + 1) % pending.len();
            }
        }
        let mut active = self.peers.iter().filter(|p| p.lease.is_some()).count();
        let start = self.serve_cursor;
        for offset in 0..n {
            if active >= config.max_outgoing {
                break;
            }
            let index = (start + offset) % n;
            let peer = &self.peers[index];
            if peer.lease.is_some() {
                continue;
            }
            let Some(wanted) = peer.requested else {
                continue;
            };
            let Some(address) = peers
                .iter()
                .find(|p| p.member == Some(peer.member) && p.generation == peer.generation)
                .map(|p| p.address.connection_key())
            else {
                continue;
            };
            // A timeout cannot reclaim STP's buffers. No new window is encoded
            // until BOTH application credit and the independent Bulk queue allow.
            if !bulk_ready(address) {
                continue;
            }
            self.peers[index].lease = Some(Window {
                want: wanted.want,
                until: now.saturating_add(config.request_timeout),
                mask: 0,
            });
            self.peers[index].requested = None;
            let reference = wanted.want.ticket.reference;
            if self.peers[index].offered.as_ref().is_none_or(|(r, _)| {
                r.key() != reference.key()
                    || r.manifest.root != reference.manifest.root
                    || r.manifest.leaf_bytes != reference.manifest.leaf_bytes
            }) {
                self.peers[index].offered = Some((
                    reference,
                    Bitmap::new(&reference.manifest, config.max_body).map_err(error)?,
                ));
            }
            let mut sent = 0;
            for (position, &leaf) in wanted.want.indices().iter().enumerate() {
                let Some((data, proof)) = self.leaf(engine, reference, leaf) else {
                    continue;
                };
                let len = data.len();
                let proof_len = proof.len() * 32;
                let bytes =
                    codec::encode_leaf(wanted.want.ticket, leaf, data, &proof, config.max_body)
                        .map_err(core_error)?;
                let history = &mut self.peers[index]
                    .offered
                    .as_mut()
                    .expect("bounded history")
                    .1;
                if history.have(leaf as usize).unwrap_or(false) {
                    counts.duplicate_payload_bytes =
                        counts.duplicate_payload_bytes.saturating_add(len as u64);
                } else {
                    history.set(leaf as usize).map_err(error)?;
                }
                counts.payload_bytes = counts.payload_bytes.saturating_add(len as u64);
                counts.leaf_responses = counts.leaf_responses.saturating_add(1);
                counts.leaf_proof_bytes = counts.leaf_proof_bytes.saturating_add(proof_len as u64);
                counts.offered_data.record(bytes.len());
                push.send_bulk.push((address, bytes));
                sent |= 1 << position;
            }
            if sent == 0 {
                self.peers[index].lease = None;
            } else {
                self.peers[index]
                    .lease
                    .as_mut()
                    .expect("credit acquired")
                    .mask = sent;
                active += 1;
            }
            self.serve_cursor = (index + 1) % n;
        }
        Ok(())
    }
}
impl PayloadState {
    pub(in crate::d28::runtime) fn sync_merkle<S: Storage>(
        &mut self,
        engine: &Engine<S>,
        peers: &[Peer],
        now: u64,
    ) {
        self.merkle.sync(&self.pending, engine, peers, now);
    }
    pub(in crate::d28::runtime) fn offer_merkle<S: Storage>(
        &mut self,
        engine: &Engine<S>,
        peers: &[Peer],
        key: &SigningKey,
        bulk_ready: impl Fn(super::super::ConnectionKey) -> bool,
        config: &RuntimeConfig,
        now: u64,
        push: &mut NetworkThreadPush,
        counts: &mut PayloadMetricsSnapshot,
        leaf_bytes: usize,
    ) -> io::Result<()> {
        self.sync_merkle(engine, peers, now);
        self.merkle.refresh_cache(engine, config, key, leaf_bytes)?;
        self.merkle.offers(
            engine,
            &self.pending,
            peers,
            bulk_ready,
            config,
            now,
            push,
            counts,
        )
    }
}
impl Network {
    pub(super) fn receive_merkle<S: Storage>(
        &mut self,
        engine: &mut Engine<S>,
        config: &RuntimeConfig,
        events: &mut Events,
        member: u32,
        generation: u64,
        bytes: &[u8],
        now: u64,
    ) -> io::Result<bool> {
        let super::PayloadMode::Merkle { leaf_bytes } = config.payload_mode else {
            return Ok(false);
        };
        let Some(peer_index) = self
            .payload
            .merkle
            .peers
            .iter()
            .position(|p| p.member == member && p.generation == generation)
        else {
            return Ok(false);
        };
        match bytes.first() {
            Some(&codec::HAVE) => {
                let Ok(have) = codec::decode_have(bytes, config.max_body) else {
                    return Ok(false);
                };
                let reference = have.reference;
                if !self.payload.in_scope(reference.key())
                    || reference.manifest.leaf_bytes as usize != leaf_bytes
                {
                    return Ok(false);
                }
                // An existing verified assembly also pins the originally signed
                // header when a stronger outline for the SAME body replaces it.
                // Completion still uses the exact new pending header in the core.
                let known_header = self.payload.pending.iter().any(|p| {
                    p.key == reference.key()
                        && p.evidence.header().is_some_and(|h| {
                            h.identity(engine.c()) == reference.manifest.signed_header_identity
                        })
                }) || retained_headers(engine).iter().any(|(k, h)| {
                    *k == reference.key()
                        && h.identity(engine.c()) == reference.manifest.signed_header_identity
                }) || self
                    .payload
                    .merkle
                    .assemblies
                    .iter()
                    .any(|a| a.reference == reference);
                if !known_header {
                    return Ok(false);
                }
                let Some(holder) = engine
                    .c()
                    .members()
                    .iter()
                    .find(|m| m.public_key == have.holder.holder)
                else {
                    return Ok(false);
                };
                if have
                    .holder
                    .verify(
                        &context(engine, reference.height),
                        &reference.manifest,
                        &holder.public_key,
                        config.max_body,
                    )
                    .is_err()
                {
                    return Ok(false);
                }
                let pending = &self.payload.pending;
                let state = &mut self.payload.merkle;
                // Do not let a later HAVE for an already-held unrelated body
                // erase discovery of the missing one. Multiple pending keys get
                // rotating priority, while retaining only ONE bitmap per peer.
                let Some(position) = pending.iter().position(|p| p.key == reference.key()) else {
                    return Ok(true);
                };
                let rank = |i: usize| {
                    (i + pending.len() - state.pending_cursor % pending.len()) % pending.len()
                };
                let peer = &state.peers[peer_index];
                if peer
                    .failed
                    .is_some_and(|(r, until)| same_payload(r, reference) && now < until)
                {
                    return Ok(false);
                }
                if let Some(old) = &peer.available {
                    if old.reference.key() != reference.key()
                        && pending
                            .iter()
                            .position(|p| p.key == old.reference.key())
                            .is_some_and(|i| rank(i) < rank(position))
                    {
                        return Ok(true);
                    }
                }
                // Hashes/proofs bind the payload geometry/root, not the header.
                // The replacement attestation above authenticates H2. Preserve
                // verified bytes, but always complete with the current outline.
                if pending[position].evidence.header().is_some_and(|h| {
                    h.identity(engine.c()) == reference.manifest.signed_header_identity
                }) {
                    if let Some(a) = state
                        .assemblies
                        .iter_mut()
                        .find(|a| same_payload(a.reference, reference))
                    {
                        a.reference = reference;
                        a.holder = have.holder;
                    }
                }
                let bitmap = Bitmap::from_bytes(&reference.manifest, have.bitmap, config.max_body)
                    .map_err(error)?;
                state.peers[peer_index].available = Some(Available {
                    reference,
                    holder: have.holder,
                    bitmap,
                    until: now.saturating_add(config.status_ttl),
                });
                Ok(true)
            }
            Some(&codec::WANT) => {
                let Ok(want) = codec::decode_want(bytes, config.max_body) else {
                    return Ok(false);
                };
                if !self.payload.in_scope(want.ticket.reference.key())
                    || want.ticket.reference.manifest.leaf_bytes as usize != leaf_bytes
                {
                    return Ok(false);
                }
                let state = &mut self.payload.merkle;
                let reference = want.ticket.reference;
                if !state
                    .cached
                    .iter()
                    .any(|c| c.headers.iter().any(|(r, _)| *r == reference))
                    && !state.assemblies.iter().any(|a| a.reference == reference)
                {
                    return Ok(false); // No interest/history for an unknown header or root.
                }
                let peer = &mut state.peers[peer_index];
                if peer
                    .remote_generation
                    .is_some_and(|g| g != want.ticket.generation)
                    || want.ticket.token <= peer.remote_token
                {
                    return Ok(false);
                }
                peer.remote_generation = Some(want.ticket.generation);
                peer.remote_token = want.ticket.token;
                // New scheduling metadata cannot release the older send window.
                peer.requested = Some(Window {
                    want,
                    until: now.saturating_add(config.status_ttl),
                    mask: 0,
                });
                Ok(true)
            }
            Some(&codec::ACK) => {
                let Ok(ack) = codec::decode_ack(bytes, config.max_body) else {
                    return Ok(false);
                };
                if !self.payload.in_scope(ack.ticket.reference.key()) {
                    return Ok(false);
                }
                let peer = &mut self.payload.merkle.peers[peer_index];
                let Some(lease) = &mut peer.lease else {
                    return Ok(false);
                };
                if lease.want.ticket != ack.ticket {
                    return Ok(false);
                }
                let Some(position) = lease.want.position(ack.index) else {
                    return Ok(false);
                };
                if lease.mask & (1 << position) == 0 {
                    return Ok(false);
                }
                lease.mask &= !(1 << position);
                if lease.mask == 0 {
                    peer.lease = None;
                }
                Ok(true)
            }
            Some(&codec::LEAF) => {
                let Ok(leaf) = codec::decode_leaf(bytes, config.max_body) else {
                    return Ok(false);
                };
                let reference = leaf.ticket.reference;
                if !self.payload.in_scope(reference.key()) || leaf.ticket.generation != generation {
                    return Ok(false);
                }
                let state = &mut self.payload.merkle;
                if leaf.ticket.token > state.peers[peer_index].issued_through {
                    return Ok(false);
                }
                // Old issued tokens may add useful bytes to an EXISTING pinned
                // root/geometry, including across authenticated header changes.
                // They cannot allocate an assembly or substitute header validity.
                let Some(index) = state
                    .assemblies
                    .iter()
                    .position(|a| same_payload(a.reference, reference))
                else {
                    return Ok(false);
                };
                let a = &mut state.assemblies[index];
                let duplicate = a.tree.bitmap().have(leaf.index as usize).unwrap_or(false);
                if !duplicate {
                    let (proof, count) = leaf.proof_array();
                    if a.tree
                        .insert_verified(leaf.index as usize, leaf.data, &proof[..count])
                        .is_err()
                    {
                        let peer = &mut state.peers[peer_index];
                        peer.receive = None;
                        peer.available = None;
                        peer.failed = Some((reference, now.saturating_add(config.request_timeout)));
                        if a.tree.bitmap().count() == 0 {
                            a.retry_at = now;
                        }
                        return Ok(false);
                    }
                    let range = a
                        .tree
                        .layout()
                        .leaf_range(leaf.index as usize)
                        .map_err(error)?;
                    a.body[range].copy_from_slice(leaf.data);
                    a.retry_at = now.saturating_add(config.request_timeout).min(a.expires);
                    if let Some(metrics) = &config.metrics {
                        super::saturating_add(
                            &metrics.payload.verified_leaf_bytes,
                            leaf.data.len() as u64,
                        );
                    }
                }
                // A duplicate never overwrites bytes, renews interest, or earns
                // verified-byte credit. ACK is only transfer credit, not validity.
                let peer = &mut state.peers[peer_index];
                let ack = Ack {
                    ticket: leaf.ticket,
                    index: leaf.index,
                };
                if !peer.acks.contains(&Some(ack)) {
                    if let Some(slot) = peer.acks.iter_mut().find(|a| a.is_none()) {
                        *slot = Some(ack);
                    }
                }
                if let Some(window) = &mut peer.receive {
                    if window.want.ticket == leaf.ticket {
                        if let Some(position) = window.want.position(leaf.index) {
                            window.mask |= 1 << position;
                        }
                        if window.mask.count_ones() == u32::from(window.want.count) {
                            peer.receive = None;
                        }
                    }
                }
                if !a.tree.bitmap().complete() {
                    return Ok(true);
                }
                let a = state.assemblies.remove(index);
                if !canonical_body(&a.body, reference.height, config.max_body)
                    || engine.c().body_id(&a.body) != reference.manifest.body_id
                {
                    state.reject_reference(reference, a.expires);
                    return Ok(false);
                }
                for peer in &mut state.peers {
                    if peer
                        .receive
                        .is_some_and(|w| same_payload(w.want.ticket.reference, reference))
                    {
                        peer.receive = None;
                    }
                }
                let Some(index) = self
                    .payload
                    .pending
                    .iter()
                    .position(|p| p.key == reference.key())
                else {
                    return Ok(false);
                };
                let pending = self.payload.pending.remove(index);
                if let Some(metrics) = &config.metrics {
                    super::saturating_add(&metrics.payload.assembly_completions, 1);
                }
                // Neither the root nor this structural/hash completion grants
                // Valid. Exact header/certificates and the ordinary Host gate run.
                pending.evidence.with_body(&a.body, |frame| {
                    complete(engine, frame, events, &mut self.conflicts, config)
                })
            }
            _ => Ok(false),
        }
    }
}

#[cfg(test)]
mod tests;
