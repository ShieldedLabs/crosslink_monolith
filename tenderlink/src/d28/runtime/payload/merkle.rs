//! Unordered, independently authenticated shards over best-effort STP.
//!
//! K owned slots are SHARED by incomplete and completed bodies. Round/header
//! changes do not discard verified content; only height/config retirement or
//! bounded fair eviction does. Per-peer state contains bitmaps, never bodies.
//! Local byte/message admission is independent of leaf geometry and of receipts.
use super::merkle_wire::{self as codec, Authentication, Have, MAX_WANT, Reference, Want};
use super::{
    BodyKey, Engine, Events, Network, NetworkThreadPush, PayloadMetricsSnapshot, PayloadState,
    Peer, Pending, RuntimeConfig, Storage, VerifiedBody, complete_verified, core_error, retained_body,
};
use crate::d28::{
    Frame, Header, Message, Proposal,
    merkle::{Bitmap, HolderContext, PartialTree, ProposerAttestation, Tree, verify_leaf},
};
use crate::stp::ConnectionKey;
use crate::transport::{SendCapacity, SendCapacitySnapshot};
use ed25519_zebra::{SigningKey, VerificationKey};
use rand::{Rng, SeedableRng, seq::SliceRandom};
use rand_chacha::ChaCha20Rng;
use std::io;

// Fixed service bounds, not transfer windows: no remote receipt releases credit.
const MAX_PUSH_MESSAGES: usize = 256;
const DATA_PER_CALL: usize = 24;
const REPAIR_GRACE_MS: u64 = 200;
const RETRY_MIN_MS: u64 = 250;
const RETRY_JITTER_MS: u64 = 250;

/// Observed transport capacity, plus exact debits for this service call.
/// A snapshot is not a reservation against concurrent Base traffic. Native STP
/// retains its own independent limits; outbox_slots is LOCAL message headroom.
/// This ceiling governs NEW Merkle offers after charging existing queues and
/// staging. If those already exceed it, add nothing. Other runtime control
/// producers have independent bounds; this is not a whole-runtime memory cap.
struct PeerAllowance {
    connection: ConnectionKey,
    channels: [SendCapacity; 2],
    pending_bytes: [u64; 2],
    pending_messages: [usize; 2],
}
struct Admission {
    peers: Vec<PeerAllowance>,
    bytes: u64,
    messages: usize,
    limit: u64,
}
impl Admission {
    fn new(
        peers: &[Peer],
        config: &RuntimeConfig,
        push: &NetworkThreadPush,
        capacity: &[SendCapacitySnapshot; 2],
    ) -> Self {
        // ALL connected transport peers, including Base-only connections, are
        // included in snapshot totals. Count all already staged frames too.
        let mut bytes = capacity[0]
            .backlog_bytes
            .saturating_add(capacity[1].backlog_bytes);
        let mut messages = 0usize;
        for queue in [&push.send_unreliable, &push.send_bft_live, &push.send_bulk] {
            for (_, data) in queue {
                bytes = bytes.saturating_add(data.len() as u64);
                messages += 1;
            }
        }
        let peers = peers
            .iter()
            .map(|peer| {
                let connection = peer.address.connection_key();
                let channels = std::array::from_fn(|lane| {
                    capacity[lane]
                        .peers
                        .iter()
                        .find(|(address, _)| *address == connection)
                        .map(|(_, c)| *c)
                        .unwrap_or(SendCapacity {
                            backlog_bytes: 0,
                            available_bytes: 0,
                            outbox_slots: 0,
                        })
                });
                let mut pending_bytes = [0u64; 2];
                let mut pending_messages = [0usize; 2];
                for (lane, queue) in [
                    (0, &push.send_unreliable),
                    (0, &push.send_bft_live),
                    (1, &push.send_bulk),
                ] {
                    for (_, data) in queue.iter().filter(|(address, _)| *address == connection) {
                        pending_bytes[lane] = pending_bytes[lane].saturating_add(data.len() as u64);
                        pending_messages[lane] += 1;
                    }
                }
                PeerAllowance {
                    connection,
                    channels,
                    pending_bytes,
                    pending_messages,
                }
            })
            .collect();
        Self {
            peers,
            bytes,
            messages,
            limit: codec::encoded_byte_limit(config.max_body, config.max_outgoing).unwrap_or(0)
                as u64,
        }
    }
    fn reserve(&mut self, connection: ConnectionKey, bulk: bool, size: usize) -> bool {
        let lane = usize::from(bulk);
        let Some(peer) = self.peers.iter_mut().find(|p| p.connection == connection) else {
            return false;
        };
        let size = size as u64;
        if self.messages >= MAX_PUSH_MESSAGES
            || size > self.limit.saturating_sub(self.bytes)
            || size
                > peer.channels[lane]
                    .available_bytes
                    .saturating_sub(peer.pending_bytes[lane])
            || peer.pending_messages[lane] >= peer.channels[lane].outbox_slots
        {
            return false;
        }
        self.bytes += size;
        self.messages += 1;
        peer.pending_bytes[lane] += size;
        peer.pending_messages[lane] += 1;
        true
    }
}
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
// Header identity is excluded ONLY when reusing verified content, not metadata
// or Host validity. Every incoming authentication is independently checked.
fn same_payload(a: Reference, b: Reference) -> bool {
    a.key() == b.key()
        && a.manifest.root == b.manifest.root
        && a.manifest.body_len == b.manifest.body_len
        && a.manifest.leaf_bytes == b.manifest.leaf_bytes
}
fn authenticated<S: Storage>(
    engine: &Engine<S>,
    auth: Authentication,
    config: &RuntimeConfig,
) -> bool {
    let r = auth.reference;
    let h = auth.header;
    let super::PayloadMode::Merkle { leaf_bytes } = config.payload_mode else {
        return false;
    };
    if !engine.is_active()
        || engine
            .view()
            .is_none_or(|d| (d.config, d.height) != (r.config, r.height))
        || r.manifest.leaf_bytes as usize != leaf_bytes
        || h.content.height != r.height
        || h.content.id != r.manifest.body_id
        || h.identity(engine.c()) != r.manifest.signed_header_identity
        || !h.verify(engine.c())
    {
        return false;
    }
    let Some(proposer) = engine.c().members().get(h.content.signer as usize) else {
        return false;
    };
    auth.proposer
        .verify(
            &context(engine, r.height),
            &r.manifest,
            &proposer.public_key,
            config.max_body,
        )
        .is_ok()
}
struct Available {
    bitmap: Bitmap,
    until: u64,
}
struct PeerBody {
    reference: Reference,
    available: Option<Available>,
    requested: Vec<u8>,
    // The initial pass is random without replacement, and never restarted by a
    // poll or a replacement header for the same content/geometry.
    pushed: Bitmap,
    offered: Bitmap,
    have_at: u64,
}
struct LeafPeer {
    member: u32,
    generation: u64,
    bodies: Vec<PeerBody>,
    initial_turn: bool,
}
#[derive(Clone, Copy, Default)]
struct Retry {
    at: u64,
    source: Option<u32>,
}
enum Inventory {
    Empty(Bitmap),
    Partial(PartialTree),
    Full(Tree, Bitmap),
}
impl Inventory {
    fn bitmap(&self) -> &Bitmap {
        match self {
            Self::Empty(b) | Self::Full(_, b) => b,
            Self::Partial(t) => t.bitmap(),
        }
    }
    fn proof(&self, index: usize) -> Option<Vec<[u8; 32]>> {
        match self {
            Self::Empty(_) => None,
            Self::Partial(t) => t.proof(index).ok(),
            Self::Full(t, _) => t.proof(index).ok(),
        }
    }
}
struct Owned {
    auth: Authentication,
    inventory: Inventory,
    // Mutable assembly only; its allocation moves into verified on completion.
    // A complete owner has no second body buffer or persistent temporary copy.
    body: Vec<u8>,
    verified: Option<VerifiedBody>,
    local_proposer: bool,
    retries: Vec<Retry>,
    repair_after: u64,
    touched: u64,
}
impl Owned {
    fn bitmap(&self) -> &Bitmap {
        self.inventory.bitmap()
    }
    fn complete(&self) -> bool {
        self.verified.is_some()
    }
    fn bytes(&self) -> &[u8] {
        self.verified.as_ref().map_or(self.body.as_slice(), VerifiedBody::bytes)
    }
}
pub(super) struct MerkleState {
    peers: Vec<LeafPeer>,
    owned: Vec<Owned>,
    rng: ChaCha20Rng,
    serve_cursor: usize,
    interest_cursor: usize,
    #[cfg(test)]
    completed_deliveries: usize,
}
impl Default for MerkleState {
    fn default() -> Self {
        // Production randomness is OS-seeded. Deterministic replay is injected
        // only by the unit fixtures, never from a fixed production seed.
        Self {
            peers: Vec::new(),
            owned: Vec::new(),
            rng: ChaCha20Rng::from_os_rng(),
            serve_cursor: 0,
            interest_cursor: 0,
            #[cfg(test)]
            completed_deliveries: 0,
        }
    }
}
impl MerkleState {
    pub(super) fn clear(&mut self) {
        self.peers.clear();
        self.owned.clear();
    }
    pub(super) fn verified(&self, key: BodyKey) -> Option<&VerifiedBody> {
        self.owned
            .iter()
            .find(|a| a.auth.reference.key() == key && a.complete())
            .and_then(|a| a.verified.as_ref())
    }
    #[cfg(test)]
    pub(super) fn body(&self, key: BodyKey) -> Option<&[u8]> {
        self.verified(key).map(VerifiedBody::bytes)
    }
    pub(super) fn sync<S: Storage>(
        &mut self,
        _pending: &[Pending],
        _engine: &Engine<S>,
        peers: &[Peer],
        now: u64,
    ) {
        self.peers.retain(|p| {
            peers
                .iter()
                .any(|q| q.member == Some(p.member) && q.generation == p.generation)
        });
        for peer in peers {
            let Some(member) = peer.member else {
                continue;
            };
            if !self
                .peers
                .iter()
                .any(|p| p.member == member && p.generation == peer.generation)
            {
                self.peers.push(LeafPeer {
                    member,
                    generation: peer.generation,
                    bodies: Vec::new(),
                    initial_turn: true,
                });
            }
        }
        // Metadata expiry and core role changes must not retire useful bytes.
        // Body ownership is bounded separately, and clear() retires scope.
        for peer in &mut self.peers {
            peer.bodies.retain(|b| {
                self.owned
                    .iter()
                    .any(|a| same_payload(a.auth.reference, b.reference))
            });
            for body in &mut peer.bodies {
                if body.available.as_ref().is_some_and(|a| now >= a.until) {
                    body.available = None;
                }
            }
        }
    }
    fn peer_body(
        &mut self,
        peer: usize,
        reference: Reference,
        max_body: usize,
    ) -> io::Result<&mut PeerBody> {
        if let Some(index) = self.peers[peer]
            .bodies
            .iter()
            .position(|b| same_payload(b.reference, reference))
        {
            self.peers[peer].bodies[index].reference = reference;
            return Ok(&mut self.peers[peer].bodies[index]);
        }
        let bitmap = Bitmap::new(&reference.manifest, max_body).map_err(error)?;
        let len = bitmap.as_bytes().len();
        self.peers[peer].bodies.push(PeerBody {
            reference,
            available: None,
            requested: vec![0; len],
            pushed: bitmap.clone(),
            offered: bitmap,
            have_at: 0,
        });
        Ok(self.peers[peer]
            .bodies
            .last_mut()
            .expect("inserted peer bitmap"))
    }
    fn room(&mut self, pending: &[Pending], config: &RuntimeConfig) -> bool {
        if self.owned.len() < config.max_requests {
            return true;
        }
        // Prefer an idle completed/cache interest; ties use age. A completed
        // cache never permanently prevents admission of a newly needed body.
        let victim = self
            .owned
            .iter()
            .enumerate()
            .min_by_key(|(_, a)| {
                (
                    pending.iter().any(|p| p.key == a.auth.reference.key()),
                    a.touched,
                )
            })
            .map(|(i, _)| i);
        let Some(victim) = victim else {
            return false;
        };
        let reference = self.owned.remove(victim).auth.reference;
        for p in &mut self.peers {
            p.bodies.retain(|b| !same_payload(b.reference, reference));
        }
        true
    }
    fn admit(
        &mut self,
        auth: Authentication,
        pending: &[Pending],
        config: &RuntimeConfig,
        now: u64,
    ) -> io::Result<Option<usize>> {
        if let Some(index) = self
            .owned
            .iter()
            .position(|a| same_payload(a.auth.reference, auth.reference))
        {
            let a = &mut self.owned[index];
            if auth.header.content.round >= a.auth.header.content.round {
                a.local_proposer &= a.auth.proposer.proposer == auth.proposer.proposer;
                a.auth = auth;
            }
            return Ok(Some(index));
        }
        if !self.room(pending, config) {
            return Ok(None);
        }
        let bitmap = Bitmap::new(&auth.reference.manifest, config.max_body).map_err(error)?;
        let count = bitmap.leaf_count();
        self.owned.push(Owned {
            auth,
            inventory: Inventory::Empty(bitmap),
            body: Vec::new(),
            verified: None,
            local_proposer: false,
            retries: vec![Retry::default(); count],
            repair_after: now.saturating_add(REPAIR_GRACE_MS),
            touched: now,
        });
        Ok(Some(self.owned.len() - 1))
    }
    fn refresh_cache<S: Storage>(
        &mut self,
        engine: &Engine<S>,
        pending: &[Pending],
        config: &RuntimeConfig,
        key: &SigningKey,
        leaf_bytes: usize,
        now: u64,
    ) -> io::Result<()> {
        let public: [u8; 32] = VerificationKey::from(key).into();
        for (body_key, header) in retained_headers(engine) {
            // Only the original proposer can create a transport commitment.
            // Relays retain and forward the original authenticated commitment.
            if engine
                .c()
                .members()
                .get(header.content.signer as usize)
                .is_none_or(|m| m.public_key != public)
            {
                continue;
            }
            if let Some(a) = self
                .owned
                .iter_mut()
                .find(|a| a.auth.reference.key() == body_key && a.complete())
            {
                if a.auth.header == header || a.auth.header.content.round > header.content.round {
                    continue;
                }
                let mut reference = a.auth.reference;
                reference.manifest.signed_header_identity = header.identity(engine.c());
                let proposer = ProposerAttestation::sign(
                    &context(engine, body_key.height),
                    &reference.manifest,
                    key,
                    config.max_body,
                )
                .map_err(error)?;
                a.auth = Authentication {
                    header,
                    reference,
                    proposer,
                };
                // Preserve the old pass: a new signed header does not mean
                // broadcast the same body to every peer again.
                a.local_proposer = true;
                continue;
            }
            let Some(body) = retained_body(engine, body_key) else {
                continue;
            };
            // Refresh is background work, not a new receive interest. Never
            // evict a needed partial to resurrect a retained old source.
            let matching = self
                .owned
                .iter()
                .position(|a| a.auth.reference.key() == body_key);
            if matching.is_none() && self.owned.len() >= config.max_requests {
                let Some(victim) = self.owned.iter().position(|a| {
                    a.auth.header.content.round < header.content.round
                        && !pending.iter().any(|p| p.key == a.auth.reference.key())
                }) else {
                    continue;
                };
                let reference = self.owned.remove(victim).auth.reference;
                for peer in &mut self.peers {
                    peer.bodies
                        .retain(|b| !same_payload(b.reference, reference));
                }
            }
            // Detach the old partial before snapshotting a retained source: its
            // assembly allocation is the candidate's body budget, not an extra B.
            // On validation failure no emptied owner/bitmap can remain usable.
            let old = matching.map(|i| self.owned.remove(i));
            let old_reference = old.as_ref().map(|a| a.auth.reference);
            let mut bytes = old.map_or_else(Vec::new, |a| a.body);
            if bytes.capacity() < body.len() {
                bytes = Vec::new(); // release the old allocation before growing
            }
            bytes.clear();
            bytes.extend_from_slice(body);
            let Some(verified) = VerifiedBody::new(
                engine.c(), body_key, bytes, config.max_body,
            ) else {
                if let Some(old) = old_reference {
                    for peer in &mut self.peers {
                        peer.bodies.retain(|b| !same_payload(b.reference, old));
                    }
                }
                continue;
            };
            let tree = Tree::new(verified.bytes(), leaf_bytes, config.max_body).map_err(error)?;
            let reference = Reference {
                config: body_key.config,
                height: body_key.height,
                manifest: tree.manifest(body_key.id, header.identity(engine.c())),
            };
            if let Some(old) = old_reference.filter(|old| !same_payload(*old, reference)) {
                for peer in &mut self.peers {
                    peer.bodies.retain(|b| !same_payload(b.reference, old));
                }
            }
            let proposer = ProposerAttestation::sign(
                &context(engine, body_key.height),
                &reference.manifest,
                key,
                config.max_body,
            )
            .map_err(error)?;
            let mut bitmap = Bitmap::new(&reference.manifest, config.max_body).map_err(error)?;
            for i in 0..tree.leaf_count() {
                bitmap.set(i).map_err(error)?;
            }
            let retries = vec![Retry::default(); tree.leaf_count()];
            if let Some(ai) = self
                .owned
                .iter()
                .position(|a| same_payload(a.auth.reference, reference))
            {
                let a = &mut self.owned[ai];
                a.auth = Authentication {
                    header,
                    reference,
                    proposer,
                };
                a.inventory = Inventory::Full(tree, bitmap);
                a.body = Vec::new();
                a.verified = Some(verified);
                a.local_proposer = true;
                a.touched = now;
                continue;
            }
            // A same-key but different root cannot consume an extra owner.
            if let Some(ai) = self
                .owned
                .iter()
                .position(|a| a.auth.reference.key() == body_key)
            {
                let old = self.owned.remove(ai).auth.reference;
                for peer in &mut self.peers {
                    peer.bodies.retain(|b| !same_payload(b.reference, old));
                }
            }
            self.owned.push(Owned {
                auth: Authentication {
                    header,
                    reference,
                    proposer,
                },
                inventory: Inventory::Full(tree, bitmap),
                body: Vec::new(),
                verified: Some(verified),
                local_proposer: true,
                retries,
                repair_after: 0,
                touched: now,
            });
        }
        Ok(())
    }
    fn offers<S: Storage>(
        &mut self,
        _engine: &Engine<S>,
        peers: &[Peer],
        config: &RuntimeConfig,
        now: u64,
        push: &mut NetworkThreadPush,
        counts: &mut PayloadMetricsSnapshot,
        mut admission: Admission,
    ) -> io::Result<()> {
        let n = self.peers.len();
        let owned_count = self.owned.len();
        for pi in 0..n {
            for ai in 0..owned_count {
                self.peer_body(pi, self.owned[ai].auth.reference, config.max_body)?;
            }
        }
        // Compact possession advertisements repeat, never a full body broadcast.
        for offset in 0..n {
            let pi = (self.serve_cursor + offset) % n;
            let Some(address) = address(peers, &self.peers[pi]) else {
                continue;
            };
            for oi in 0..owned_count {
                let ai = (self.interest_cursor + oi) % owned_count;
                let a = &self.owned[ai];
                if a.bitmap().count() == 0 {
                    continue;
                }
                let bi = self.peers[pi]
                    .bodies
                    .iter()
                    .position(|b| same_payload(b.reference, a.auth.reference))
                    .expect("owned bitmap");
                if now < self.peers[pi].bodies[bi].have_at {
                    continue;
                }
                let size =
                    codec::have_size(a.auth.reference, config.max_body).map_err(core_error)?;
                if !admission.reserve(address, false, size) {
                    continue;
                }
                let bytes = codec::encode_have(
                    Have {
                        auth: a.auth,
                        bitmap: a.bitmap().as_bytes(),
                    },
                    config.max_body,
                )
                .map_err(core_error)?;
                counts.availability_bytes =
                    counts.availability_bytes.saturating_add(bytes.len() as u64);
                counts.offered_control.record(bytes.len());
                push.send_unreliable.push((address, bytes));
                self.peers[pi].bodies[bi].have_at =
                    now.saturating_add(super::super::millis(config.status_interval)?);
            }
        }
        // Each missing index has its own short jittered retry clock. No missing
        // receipt holds unrelated shards or the complete body behind a barrier.
        for oi in 0..owned_count {
            let ai = (self.interest_cursor + oi) % owned_count;
            if self.owned[ai].complete() {
                continue;
            }
            let reference = self.owned[ai].auth.reference;
            let mut missing: Vec<_> = self.owned[ai]
                .bitmap()
                .missing()
                .filter(|&i| {
                    now >= self.owned[ai].retries[i].at
                        && (self.owned[ai].retries[i].source.is_some()
                            || now >= self.owned[ai].repair_after)
                })
                .collect();
            missing.shuffle(&mut self.rng);
            let mut wants: Vec<Option<Want>> = vec![None; n];
            for leaf in missing {
                let previous = self.owned[ai].retries[leaf].source;
                let mut candidates: Vec<_> = (0..n)
                    .filter(|&pi| {
                        wants[pi].is_none_or(|w| usize::from(w.count) < MAX_WANT)
                            && self.peers[pi].bodies.iter().any(|b| {
                                same_payload(b.reference, reference)
                                    && b.available.as_ref().is_some_and(|a| {
                                        now < a.until && a.bitmap.have(leaf).unwrap_or(false)
                                    })
                            })
                    })
                    .collect();
                if candidates.len() > 1 {
                    candidates.retain(|&pi| Some(self.peers[pi].member) != previous);
                }
                if candidates.is_empty() {
                    continue;
                }
                let pi = candidates[self.rng.random_range(0..candidates.len())];
                match &mut wants[pi] {
                    Some(w) => {
                        w.add(leaf as u32);
                    }
                    slot => *slot = Some(Want::single(reference, leaf as u32)),
                }
            }
            for (pi, want) in wants
                .into_iter()
                .enumerate()
                .filter_map(|(i, w)| w.map(|w| (i, w)))
            {
                let Some(address) = address(peers, &self.peers[pi]) else {
                    continue;
                };
                if !admission.reserve(address, false, codec::want_size(usize::from(want.count))) {
                    continue;
                }
                let bytes = codec::encode_want(want, config.max_body).map_err(core_error)?;
                for &leaf in want.indices() {
                    self.owned[ai].retries[leaf as usize] = Retry {
                        source: Some(self.peers[pi].member),
                        at: now.saturating_add(
                            RETRY_MIN_MS + self.rng.random_range(0..=RETRY_JITTER_MS),
                        ),
                    };
                }
                counts.leaf_requests = counts.leaf_requests.saturating_add(u64::from(want.count));
                counts.offered_control.record(bytes.len());
                push.send_unreliable.push((address, bytes));
            }
        }
        // Round-robin peers, one shard per turn. Both unsolicited proposer work
        // and requested relays spend the SAME global encoded-byte allowance.
        let mut sent = 0;
        let mut idle = 0;
        while n != 0 && sent < DATA_PER_CALL && idle < n {
            let pi = self.serve_cursor % n;
            self.serve_cursor = (pi + 1) % n;
            let Some(address) = address(peers, &self.peers[pi]) else {
                idle += 1;
                continue;
            };
            let mut candidate = None;
            for oi in 0..owned_count {
                let ai = (self.interest_cursor + oi) % owned_count;
                let a = &self.owned[ai];
                let bi = self.peers[pi]
                    .bodies
                    .iter()
                    .position(|b| same_payload(b.reference, a.auth.reference))
                    .expect("owned bitmap");
                let b = &self.peers[pi].bodies[bi];
                let requested: Vec<_> = (0..a.bitmap().leaf_count())
                    .filter(|&i| bit(&b.requested, i) && a.bitmap().have(i).unwrap_or(false))
                    .collect();
                let initial = a.local_proposer && !b.pushed.complete();
                let leaf = if !requested.is_empty() && (!initial || !self.peers[pi].initial_turn) {
                    Some(requested[self.rng.random_range(0..requested.len())])
                } else if initial {
                    let rank = self
                        .rng
                        .random_range(0..b.pushed.leaf_count() - b.pushed.count());
                    b.pushed.missing().nth(rank)
                } else {
                    None
                };
                if let Some(leaf) = leaf {
                    candidate = Some((ai, bi, leaf));
                    break;
                }
            }
            let Some((ai, bi, leaf)) = candidate else {
                idle += 1;
                continue;
            };
            let a = &self.owned[ai];
            let size = codec::leaf_size(a.auth.reference, leaf as u32, config.max_body)
                .map_err(core_error)?;
            // Admission is before proof or encoded-body allocation, and includes
            // earlier allocations in this push as well as existing STP backlog.
            if !admission.reserve(address, true, size) {
                idle += 1;
                continue;
            }
            let proof = a.inventory.proof(leaf).expect("verified leaf proof");
            let layout = a
                .auth
                .reference
                .manifest
                .validate(config.max_body)
                .map_err(error)?;
            let data = &a.bytes()[layout.leaf_range(leaf).map_err(error)?];
            let bytes = codec::encode_leaf(a.auth, leaf as u32, data, &proof, config.max_body)
                .map_err(core_error)?;
            self.peers[pi].initial_turn = !self.peers[pi].initial_turn;
            let b = &mut self.peers[pi].bodies[bi];
            if b.offered.have(leaf).map_err(error)? {
                counts.duplicate_payload_bytes = counts
                    .duplicate_payload_bytes
                    .saturating_add(data.len() as u64);
            } else {
                b.offered.set(leaf).map_err(error)?;
            }
            if a.local_proposer && !b.pushed.have(leaf).map_err(error)? {
                b.pushed.set(leaf).map_err(error)?;
            }
            b.requested[leaf / 8] &= !(1 << (leaf % 8));
            counts.payload_bytes = counts.payload_bytes.saturating_add(data.len() as u64);
            counts.leaf_responses = counts.leaf_responses.saturating_add(1);
            counts.leaf_proof_bytes = counts
                .leaf_proof_bytes
                .saturating_add((proof.len() * 32) as u64);
            counts.offered_data.record(bytes.len());
            push.send_bulk.push((address, bytes));
            self.interest_cursor = (ai + 1) % owned_count;
            sent += 1;
            idle = 0;
        }
        Ok(())
    }
}
fn bit(bitmap: &[u8], index: usize) -> bool {
    bitmap[index / 8] & (1 << (index % 8)) != 0
}
fn address(peers: &[Peer], peer: &LeafPeer) -> Option<ConnectionKey> {
    peers
        .iter()
        .find(|p| p.member == Some(peer.member) && p.generation == peer.generation)
        .map(|p| p.address.connection_key())
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
        capacity: &[SendCapacitySnapshot; 2],
        config: &RuntimeConfig,
        now: u64,
        push: &mut NetworkThreadPush,
        counts: &mut PayloadMetricsSnapshot,
        leaf_bytes: usize,
    ) -> io::Result<()> {
        self.sync_merkle(engine, peers, now);
        self.merkle
            .refresh_cache(engine, &self.pending, config, key, leaf_bytes, now)?;
        let admission = Admission::new(peers, config, push, capacity);
        self.merkle
            .offers(engine, peers, config, now, push, counts, admission)
    }
}
impl Network {
    fn complete_merkle_owned<S: Storage>(
        &mut self,
        engine: &mut Engine<S>,
        config: &RuntimeConfig,
        events: &mut Events,
        ai: usize,
    ) -> io::Result<bool> {
        #[cfg(test)]
        {
            self.payload.merkle.completed_deliveries += 1;
        }
        let key = self.payload.merkle.owned[ai].auth.reference.key();
        let pending = self
            .payload
            .pending
            .iter()
            .position(|p| p.key == key)
            .map(|i| self.payload.pending.remove(i));
        let a = &self.payload.merkle.owned[ai];
        let body = a.verified.as_ref().expect("completed owner");
        if let Some(pending) = pending {
            pending.evidence.with_body(body.bytes(), |frame| {
                complete_verified(engine, frame, body, events, &mut self.conflicts, config)
            })
        } else {
            complete_verified(
                engine,
                Frame {
                    message: Message::Ordinary,
                    outer: None,
                    proposal: Some(Proposal {
                        header: &a.auth.header,
                        body: body.bytes(),
                    }),
                    groups: &[],
                },
                body,
                events,
                &mut self.conflicts,
                config,
            )
        }
    }
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
        let super::PayloadMode::Merkle { .. } = config.payload_mode else {
            return Ok(false);
        };
        let Some(pi) = self
            .payload
            .merkle
            .peers
            .iter()
            .position(|p| p.member == member && p.generation == generation)
        else {
            return Ok(false);
        };
        match bytes.first() {
            Some(&codec::WANT) => {
                let Ok(want) = codec::decode_want(bytes, config.max_body) else {
                    return Ok(false);
                };
                if !self.payload.in_scope(want.reference.key())
                    || !self
                        .payload
                        .merkle
                        .owned
                        .iter()
                        .any(|a| same_payload(a.auth.reference, want.reference))
                {
                    return Ok(false);
                }
                let body = self
                    .payload
                    .merkle
                    .peer_body(pi, want.reference, config.max_body)?;
                for &index in want.indices() {
                    body.requested[index as usize / 8] |= 1 << (index % 8);
                }
                Ok(true)
            }
            Some(&codec::HAVE) | Some(&codec::LEAF) => {
                let (auth, leaf, have) = if bytes.first() == Some(&codec::LEAF) {
                    let Ok(leaf) = codec::decode_leaf(bytes, config.max_body) else {
                        return Ok(false);
                    };
                    (leaf.auth, Some(leaf), None)
                } else {
                    let Ok(have) = codec::decode_have(bytes, config.max_body) else {
                        return Ok(false);
                    };
                    (have.auth, None, Some(have))
                };
                // Exact immutable authentication already checked in an owned slot
                // needs no second Ed25519 verification. Fresh headers/root binds
                // always pass the full configured-proposer check.
                let known_auth = self.payload.merkle.owned.iter().any(|a| a.auth == auth);
                if !self.payload.in_scope(auth.reference.key())
                    || (!known_auth && !authenticated(engine, auth, config))
                {
                    return Ok(false);
                }
                let owned = self
                    .payload
                    .merkle
                    .owned
                    .iter()
                    .any(|a| same_payload(a.auth.reference, auth.reference));
                let interested = self
                    .payload
                    .pending
                    .iter()
                    .any(|p| p.key == auth.reference.key());
                // Unsolicited discovery is only the current proposal round.
                // Older authenticated leaves can still fill an existing owner.
                if !owned
                    && !interested
                    && engine
                        .view()
                        .is_none_or(|d| d.round != auth.header.content.round)
                {
                    return Ok(false);
                }
                if let Some(leaf) = &leaf {
                    let (proof, count) = leaf.proof_array();
                    if verify_leaf(
                        &auth.reference.manifest,
                        leaf.index as usize,
                        leaf.data,
                        &proof[..count],
                        config.max_body,
                    )
                    .is_err()
                    {
                        return Ok(false);
                    }
                }
                // Header, original proposer commitment, and (for LEAF) path are
                // verified before any body/tree allocation or cache insertion.
                let previous_header = self
                    .payload
                    .merkle
                    .owned
                    .iter()
                    .find(|a| same_payload(a.auth.reference, auth.reference))
                    .map(|a| a.auth.header);
                let Some(ai) =
                    self.payload
                        .merkle
                        .admit(auth, &self.payload.pending, config, now)?
                else {
                    return Ok(false);
                };
                let rebound = previous_header != Some(self.payload.merkle.owned[ai].auth.header);
                if let Some(have) = have {
                    let bitmap =
                        Bitmap::from_bytes(&auth.reference.manifest, have.bitmap, config.max_body)
                            .map_err(error)?;
                    self.payload
                        .merkle
                        .peer_body(pi, auth.reference, config.max_body)?
                        .available = Some(Available {
                        bitmap,
                        until: now.saturating_add(config.status_ttl),
                    });
                    if (rebound || interested) && self.payload.merkle.owned[ai].complete() {
                        return self.complete_merkle_owned(engine, config, events, ai);
                    }
                    return Ok(true);
                }
                let leaf = leaf.expect("leaf branch");
                let a = &mut self.payload.merkle.owned[ai];
                if a.bitmap().have(leaf.index as usize).map_err(error)? {
                    if (rebound || interested) && a.complete() {
                        return self.complete_merkle_owned(engine, config, events, ai);
                    }
                    return Ok(true);
                }
                if matches!(a.inventory, Inventory::Empty(_)) {
                    let tree = PartialTree::new(&auth.reference.manifest, config.max_body)
                        .map_err(error)?;
                    a.body
                        .try_reserve_exact(tree.layout().body_len())
                        .map_err(|_| core_error(crate::d28::Error::Exhausted))?;
                    a.body.resize(tree.layout().body_len(), 0);
                    a.inventory = Inventory::Partial(tree);
                }
                let Inventory::Partial(tree) = &mut a.inventory else {
                    return Ok(false);
                };
                let (proof, count) = leaf.proof_array();
                tree.insert_verified(leaf.index as usize, leaf.data, &proof[..count])
                    .map_err(error)?;
                let range = tree
                    .layout()
                    .leaf_range(leaf.index as usize)
                    .map_err(error)?;
                a.body[range].copy_from_slice(leaf.data);
                // Useful progress suppresses the first repair wave; once a
                // missing index has been requested its own retry deadline rules.
                a.repair_after = now.saturating_add(REPAIR_GRACE_MS);
                a.touched = now;
                if let Some(metrics) = &config.metrics {
                    super::saturating_add(
                        &metrics.payload.verified_leaf_bytes,
                        leaf.data.len() as u64,
                    );
                }
                if !a.bitmap().complete() {
                    return Ok(true);
                }
                let Some(verified) = VerifiedBody::new(
                    engine.c(),
                    a.auth.reference.key(),
                    std::mem::take(&mut a.body),
                    config.max_body,
                ) else {
                    let reference = a.auth.reference;
                    self.payload.merkle.owned.remove(ai);
                    for peer in &mut self.payload.merkle.peers {
                        peer.bodies
                            .retain(|b| !same_payload(b.reference, reference));
                    }
                    return Ok(false);
                };
                a.verified = Some(verified);
                if let Some(metrics) = &config.metrics {
                    super::saturating_add(&metrics.payload.assembly_completions, 1);
                }
                // Keep this completed owner even if the core ignores an old
                // header or immediately changes roles. Fresh outlined metadata
                // and a fresh Host tag may reuse these bytes later.
                self.complete_merkle_owned(engine, config, events, ai)
            }
            _ => Ok(false),
        }
    }
}

#[cfg(test)]
mod tests;
