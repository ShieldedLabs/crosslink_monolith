//! Runnable D28 adapter for the node's shared STP transport and trusted host.
//!
//! This is not another consensus implementation or decision archive. Only a
//! chain-derived host tip authorizes INIT. All host futures are owned here,
//! bounded, polled without awaiting their completion, and dropped on retirement.
//! Durability is provided by `DurableStore` before the engine emits signatures.
//!
//! Memory has separate owners and bounds: one borrowed RX frame is decoded at a
//! time; responder permits cover application-owned encoded replies until handoff;
//! STP's reassembly, inbox, outbox and send-ring copies have the independent
//! `TransportLimits`/STP budgets. A handoff is NOT a delivery acknowledgement and
//! does NOT make an STP-held copy count as a released end-to-end K_tx slot.
use super::{
    atomic::AtomicBlockStore,
    chain, durable,
    gossip::{
        receiver::{
            AdmissionBudget, Permit, Receiver, ReceiverConfig, ReplySink, RequestKey,
            RequestStatus, Responder, SenderConfig,
        },
        StatusSnapshot, StatusUpdate,
    },
    wire, BodyTag, Config, Emission, Engine, Error, Frame, Group, Id, Member, Message, Proposal,
    ProposalTag, Sink, Status, Step, Storage, Target, Timeouts, Vote, VoteKind,
};
use crate::transport::Channel;
use crate::{
    transport::{Consumer, ConsumerHandle, NetworkThreadPull, NetworkThreadPush, SharedTransport},
    BftAddressMap, BlockValue, ClosureToProposeNewBlock, ClosureToPushDecidedBlock,
    ClosureToReadDecidedTip, ClosureToUpdatePeers, ClosureToValidateProposedBlock, ConnectionKey,
    HostTip, PeerInfo, STPAddress, SortedRosterMember, TMStatus, TMStatusReason,
};
use ed25519_zebra::{Signature, SigningKey, VerificationKey, VerificationKeyBytes};
use std::{
    collections::VecDeque,
    future::{poll_fn, Future},
    io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use zcash_primitives::bft::{BftBlock, PubKeyID};

// The unchanged chain body encodes height as u32, unlike D28's wider counters.
fn chain_height(height: u64) -> io::Result<u64> {
    u32::try_from(height).map_err(|_| invalid("D28 height exceeds chain u32 domain"))?;
    Ok(height)
}
fn canonical_body(bytes: &[u8], height: u64, max_body: usize) -> bool {
    if bytes.len() > max_body || height > u32::MAX as u64 {
        return false;
    }
    let mut remaining = bytes;
    let Ok(block) = BftBlock::zcash_deserialize(&mut remaining) else {
        return false;
    };
    remaining.is_empty()
        && u64::from(block.height) == height
        && block
            .zcash_serialize_to_vec()
            .is_ok_and(|encoded| encoded == bytes)
}

type Work<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// All time values used by D28 and its request scheduler are milliseconds.
/// Consensus rho/timeouts must be identical on every node in the configuration.
pub struct RuntimeConfig {
    pub initial_height: u64,
    pub initial_roster: Vec<SortedRosterMember>,
    pub vote_namespace: Id,
    pub active_roster_max_n: usize,
    pub max_members: usize,
    pub max_body: usize,
    pub rho: u64,
    pub timeouts: Timeouts,
    pub transport: SharedTransport,
    pub seeds: Vec<STPAddress>,
    pub poll_interval: Duration,
    pub status_interval: Duration,
    pub request_timeout: u64,
    pub status_ttl: u64,
    pub max_connections: usize,
    pub max_requests: usize,
    pub max_outgoing: usize,
    /// Concurrent application decode slots (the driver currently uses one).
    /// Transport preassembly/inbox ownership has its own independent fixed caps.
    pub max_incoming: usize,
    /// Upper bound on authenticated message processing (including signature
    /// checks) per tick. Sources and control/body classes get round-robin turns.
    pub max_receive_per_tick: usize,
}
impl RuntimeConfig {
    pub fn new(
        initial_height: u64,
        initial_roster: Vec<SortedRosterMember>,
        vote_namespace: Id,
        active_roster_max_n: usize,
        transport: SharedTransport,
        seeds: Vec<STPAddress>,
    ) -> Self {
        Self {
            initial_height,
            initial_roster,
            vote_namespace,
            active_roster_max_n,
            max_members: active_roster_max_n.max(1),
            max_body: crate::PROPOSAL_MAX_SIZE,
            rho: 500,
            timeouts: Timeouts {
                propose: 2_000,
                prevote: 1_000,
                precommit: 1_500,
                inc_propose: 900,
                inc_prevote: 300,
                inc_precommit: 600,
            },
            transport,
            seeds,
            poll_interval: Duration::from_millis(25),
            status_interval: Duration::from_millis(100),
            request_timeout: 2_000,
            status_ttl: 5_000,
            max_connections: active_roster_max_n.max(8),
            max_requests: 2,
            max_outgoing: 2,
            max_incoming: 1,
            max_receive_per_tick: 32,
        }
    }
    fn limits(&self) -> wire::Limits {
        wire::Limits {
            max_members: self.max_members,
            max_body: self.max_body,
        }
    }
    fn chain_config(
        &self,
        roster: &[SortedRosterMember],
        namespace: Id,
    ) -> io::Result<Option<Config>> {
        let n = crate::active_roster_len(roster, self.active_roster_max_n);
        if n == 0 {
            return Ok(None);
        }
        if n > self.max_members {
            return Err(invalid("D28 active roster exceeds fixed store bound"));
        }
        Config::crosslink(
            namespace,
            roster[..n]
                .iter()
                .map(|m| Member {
                    public_key: m.pub_key.0,
                    weight: m.stake,
                })
                .collect(),
            self.max_body,
            self.rho,
            self.timeouts,
        )
        .map(Some)
        .map_err(core_error)
    }
}

/// Called synchronously: read/copy a bounded view, never wait on host I/O here.
#[derive(Clone)]
pub struct AccessD28(
    pub Arc<dyn for<'a> Fn(&Snapshot<'a>, &BftAddressMap) + Send + Sync + 'static>,
);
impl Default for AccessD28 {
    fn default() -> Self {
        Self(Arc::new(|_, _| {}))
    }
}
pub struct HostCallbacks {
    pub propose: ClosureToProposeNewBlock,
    pub validate: ClosureToValidateProposedBlock,
    pub push_decided: ClosureToPushDecidedBlock,
    pub read_tip: ClosureToReadDecidedTip,
    pub update_peers: ClosureToUpdatePeers,
    pub access: AccessD28,
}
#[derive(Clone, Copy, Debug)]
pub struct VoteConflict {
    pub first: Vote,
    pub second: Vote,
}
/// No historical round bank: rows and conflicts refer only to this live round.
pub struct Snapshot<'a> {
    pub height: u64,
    pub round: u64,
    pub step: Step,
    pub public_key: PubKeyID,
    pub roster: &'a [SortedRosterMember],
    pub active_roster_max_n: usize,
    pub namespace: Id,
    pub locked_round: Option<u64>,
    pub valid_round: Option<u64>,
    pub prevotes: &'a [Option<Vote>],
    pub precommits: &'a [Option<Vote>],
    pub pending: Option<PendingDecision<'a>>,
    pub conflicts: &'a [VoteConflict],
    /// Durable signing height is ahead of the reconstructed host chain.
    pub catching_up: bool,
}
pub struct PendingDecision<'a> {
    pub height: u64,
    pub round: u64,
    pub id: Id,
    pub body: &'a [u8],
    pub validity: Status,
    pub reason: TMStatusReason,
    pub certificate: Group<'a>,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}
fn core_error(error: Error) -> io::Error {
    io::Error::other(format!("D28: {error:?}"))
}
fn next(value: u64) -> io::Result<u64> {
    value
        .checked_add(1)
        .ok_or_else(|| core_error(Error::Exhausted))
}
fn millis(d: Duration) -> io::Result<u64> {
    u64::try_from(d.as_millis()).map_err(|_| core_error(Error::Exhausted))
}
fn status_of(status: TMStatus) -> Status {
    match status {
        TMStatus::Pass => Status::Valid,
        TMStatus::Fail => Status::Invalid,
        TMStatus::Indeterminate => Status::Indeterminate,
    }
}
fn poll_work<T>(work: &mut Option<Work<T>>, cx: &mut Context<'_>) -> Option<T> {
    let ready = match work.as_mut()?.as_mut().poll(cx) {
        Poll::Ready(value) => Some(value),
        Poll::Pending => None,
    };
    if ready.is_some() {
        *work = None;
    }
    ready
}

#[derive(Default)]
struct Events {
    // Guided automatic output is only three own bare votes, never bodies.
    votes: [Option<Vote>; 3],
    retries: [Option<BodyTag>; 2],
}
impl Sink for Events {
    fn send(&mut self, _: Target, frame: Frame<'_>) {
        debug_assert!(frame.proposal.is_none() && frame.groups.is_empty());
        if let Some(vote) = frame.outer {
            self.votes[vote.content.kind.index()] = Some(*vote);
        }
    }
    fn decided(&mut self, _: u64, _: Proposal<'_>, _: Group<'_>) {}
    fn retry(&mut self, tag: BodyTag, _: Proposal<'_>) {
        if self.retries.contains(&Some(tag)) {
            return;
        }
        if let Some(slot) = self.retries.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some(tag);
        }
    }
}

struct Validation {
    tag: BodyTag,
    work: Option<Work<(TMStatus, TMStatusReason)>>,
    reason: TMStatusReason,
    ready: bool,
}
struct Acquisition {
    tag: ProposalTag,
    work: Option<Work<Option<(BlockValue, TMStatus)>>>,
}
#[derive(Default)]
struct HostWork {
    acquisition: Option<Acquisition>,
    acquire_after: u64,
    validations: Vec<Validation>,
    decision: Option<Work<HostTip>>,
    decision_height: Option<u64>,
    peers: Option<Work<()>>,
}
impl HostWork {
    fn retire(&mut self) {
        // Futures are not detached tasks: dropping these also drops their body
        // ownership. The host must make its side of callbacks cancellation-safe.
        self.acquisition = None;
        self.acquire_after = 0;
        self.validations.clear();
        self.decision = None;
        self.decision_height = None;
        self.peers = None;
    }
    fn schedule<S: Storage>(
        &mut self,
        engine: &Engine<S>,
        host: &HostCallbacks,
        events: &mut Events,
        now: u64,
        max_body: usize,
    ) -> io::Result<()> {
        let tag = engine.proposal_tag();
        if self
            .acquisition
            .as_ref()
            .is_some_and(|a| Some(a.tag) != tag)
        {
            self.acquisition = None;
        }
        if self.acquisition.is_none() && now >= self.acquire_after {
            if let Some(tag) = tag {
                let propose = host.propose.clone();
                let validate = host.validate.clone();
                self.acquisition = Some(Acquisition {
                    tag,
                    work: Some(Box::pin(async move {
                        let body = (propose.0)().await?;
                        if !canonical_body(&body.0, tag.height, max_body) {
                            return None;
                        }
                        let (status, _) = (validate.0)(&body).await;
                        Some((body, status))
                    })),
                });
            }
        }
        let d = engine.view().expect("initialized");
        let refs = [d.slot, d.pending.map(|p| p.proposal)];
        let tags: Vec<_> = refs
            .into_iter()
            .flatten()
            .filter(|s| d.statuses[s.index] == Status::Indeterminate)
            .map(|s| (engine.body_tag(s), s))
            .collect();
        self.validations
            .retain(|v| tags.iter().any(|(tag, _)| *tag == v.tag));
        for (tag, slot) in tags {
            let index = if let Some(i) = self.validations.iter().position(|v| v.tag == tag) {
                i
            } else {
                self.validations.push(Validation {
                    tag,
                    work: None,
                    reason: TMStatusReason::None,
                    ready: true,
                });
                self.validations.len() - 1
            };
            let validation = &mut self.validations[index];
            if events.retries.contains(&Some(tag)) {
                validation.ready = true;
            }
            if validation.work.is_none() && validation.ready {
                validation.ready = false;
                let validate = host.validate.clone();
                let value = BlockValue(engine.storage().body(slot.index).to_vec());
                validation.work = Some(Box::pin(async move { (validate.0)(&value).await }));
            }
        }
        events.retries = [None; 2];
        if d.step == Step::Decided && self.decision_height != Some(d.height) {
            let pending = d.pending.ok_or_else(|| core_error(Error::Corrupt))?;
            let proposal = engine.proposal(pending.proposal);
            let pointer = chain::decision_fat_pointer(
                engine.c(),
                d.height,
                proposal,
                engine.group(pending.certificate),
            )
            .map_err(core_error)?;
            let value = BlockValue(proposal.body.to_vec());
            self.decision = Some((host.push_decided.0)(value, pointer, Vec::new()));
            self.decision_height = Some(d.height);
        }
        Ok(())
    }
    async fn poll<S: Storage>(
        &mut self,
        engine: &mut Engine<S>,
        events: &mut Events,
        now: u64,
        config: &RuntimeConfig,
        host: &HostCallbacks,
    ) -> io::Result<Option<HostTip>> {
        poll_fn(|cx| {
            let result = (|| {
                if let Some(acquire) = &mut self.acquisition {
                    if let Some(body) = poll_work(&mut acquire.work, cx) {
                        // A callback can complete after chain sync has already
                        // superseded this height. Fence before any new signature.
                        if let Some(tip) = completion_tip(engine, config, host)? {
                            return Ok(Some(tip));
                        }
                        if engine.proposal_tag() == Some(acquire.tag) {
                            if let Some((body, status)) = body {
                                tolerate_stale(engine.supply_proposal(
                                    acquire.tag,
                                    &body.0,
                                    status_of(status),
                                    events,
                                ))?;
                            }
                        }
                        self.acquisition = None;
                        self.acquire_after = now
                            .checked_add(config.rho)
                            .ok_or_else(|| core_error(Error::Exhausted))?;
                    }
                }
                for validation in &mut self.validations {
                    if let Some((status, reason)) = poll_work(&mut validation.work, cx) {
                        validation.reason = reason;
                        if let Some(tip) = completion_tip(engine, config, host)? {
                            return Ok(Some(tip));
                        }
                        tolerate_stale(engine.validity(validation.tag, status_of(status), events))?;
                    }
                }
                let tip = poll_work(&mut self.decision, cx);
                poll_work(&mut self.peers, cx);
                if let (Some(tip), Some(height)) = (&tip, self.decision_height) {
                    if tip.height < height {
                        return Err(invalid("host acknowledged decision below its height"));
                    }
                }
                Ok(completion_tip(engine, config, host)?.or(tip))
            })();
            Poll::Ready(result)
        })
        .await
    }
    fn reason(&self, tag: BodyTag) -> TMStatusReason {
        self.validations
            .iter()
            .find(|v| v.tag == tag)
            .map_or(TMStatusReason::None, |v| v.reason)
    }
}
fn completion_tip<S: Storage>(
    engine: &Engine<S>,
    config: &RuntimeConfig,
    host: &HostCallbacks,
) -> io::Result<Option<HostTip>> {
    let Some(tip) = (host.read_tip.0)(0) else {
        return Ok(None);
    };
    let successor = chain_height(next(tip.height)?)?;
    let height = engine.view().expect("initialized").height;
    if successor > height {
        return Ok(Some(tip));
    }
    if successor == height
        && config
            .chain_config(&tip.next_roster, tip.next_vote_namespace)?
            .as_ref()
            != Some(engine.c())
    {
        return Err(invalid("host changed D28 configuration at the same height"));
    }
    Ok(None)
}
fn tolerate_stale(result: super::Result<()>) -> io::Result<()> {
    match result {
        Ok(()) | Err(Error::Stale | Error::Invalid) => Ok(()),
        Err(error) => Err(core_error(error)),
    }
}

/// Hello signs this transport session, not an address or an unauthenticated
/// remote-supplied session id. Roster admission is a separate chain check.
pub fn hello_packet(key: &SigningKey, handshake: &[u8; 64]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(98);
    bytes.extend_from_slice(&[wire::HELLO_TAG, wire::VERSION]);
    bytes.extend_from_slice(&<[u8; 32]>::from(VerificationKeyBytes::from(key)));
    bytes.extend_from_slice(&key.sign(&hello_message(handshake)).to_bytes());
    bytes
}
fn hello_message(handshake: &[u8; 64]) -> [u8; 32] {
    blake3::derive_key("Tenderlink D28 STP hello v1", handshake)
}
fn verify_hello(bytes: &[u8], handshake: &[u8; 64], local: PubKeyID) -> Option<PubKeyID> {
    if bytes.len() != 98 || bytes[..2] != [wire::HELLO_TAG, wire::VERSION] {
        return None;
    }
    let public: [u8; 32] = bytes[2..34].try_into().ok()?;
    // STP exposes a symmetric session hash. Never admit our own BFT
    // identity as its remote endpoint, even with a valid session signature.
    if public == local.0 {
        return None;
    }
    let signature: [u8; 64] = bytes[34..].try_into().ok()?;
    VerificationKey::try_from(VerificationKeyBytes::from(public))
        .ok()?
        .verify(&Signature::from(signature), &hello_message(handshake))
        .ok()?;
    Some(PubKeyID(public))
}

struct Peer {
    address: STPAddress,
    handshake: [u8; 64],
    generation: u64,
    member: Option<u32>,
    public: Option<PubKeyID>,
    last_hello: Option<u64>,
    last_sequence: Option<u64>,
    last_height: u64,
    last_packet_utc: i64,
    next_control: bool,
}
struct Outgoing {
    key: RequestKey,
    bytes: Vec<u8>,
    permit: Permit,
}
struct Replies<'a> {
    peers: &'a [Peer],
    handle: &'a ConsumerHandle,
    limits: wire::Limits,
    outgoing: &'a mut Vec<Outgoing>,
    error: Option<Error>,
}
impl ReplySink for Replies<'_> {
    fn send(&mut self, key: RequestKey, frame: Frame<'_>, permit: Permit) -> Result<(), Permit> {
        let Some(peer) = self
            .peers
            .iter()
            .find(|p| p.member == Some(key.peer) && p.generation == key.generation)
        else {
            return Err(permit);
        };
        // A single complete frame may exceed the high-water mark. It can enter
        // an empty bounded outbox, but never build a queue behind stalled bulk.
        if self
            .handle
            .send_backlog_bytes(peer.address.connection_key(), Channel::Bulk)
            .unwrap_or(1)
            != 0
        {
            return Err(permit);
        }
        match wire::encode_frame(frame, Some(key.sequence), self.limits) {
            Ok(bytes) => {
                self.outgoing.push(Outgoing { key, bytes, permit });
                Ok(())
            }
            Err(error) => {
                self.error = Some(error);
                Err(permit)
            }
        }
    }
}
struct Network {
    handle: ConsumerHandle,
    pull: NetworkThreadPull,
    peers: Vec<Peer>,
    generation: u64,
    candidate_cursor: Option<ConnectionKey>,
    rotate_at: u64,
    rx_cursor: usize,
    receiver: Receiver,
    responder: Responder,
    incoming: AdmissionBudget,
    outgoing: Vec<Outgoing>,
    sequence: u64,
    last_status: Option<StatusSnapshot>,
    status_at: u64,
    dial_at: u64,
    addresses: BftAddressMap,
    conflicts: Vec<VoteConflict>,
    conflict_scope: Option<(u64, u64)>,
}
impl Network {
    fn new(config: &RuntimeConfig) -> io::Result<Self> {
        let handle = config.transport.attach(Consumer::Bft);
        let transport = handle.limits();
        let max_frame = config.limits().max_frame_bytes().map_err(core_error)?;
        if config.max_connections == 0
            || config.max_connections > u32::MAX as usize
            || config.max_connections < config.max_members
            || config.max_incoming == 0
            || config.max_receive_per_tick == 0
            || config.seeds.len() > config.max_connections
            || config.max_body == 0
            || config.poll_interval.is_zero()
            || config.status_interval.is_zero()
            || max_frame > transport.max_message_size
            || max_frame > transport.peer_outbox_bytes
            || transport.consumer_inbox_messages == 0
            || transport.consumer_inbox_bytes == 0
            || transport.consumer_inbox_peer_messages == 0
            || transport.consumer_inbox_peer_bytes == 0
            || transport.max_inbound_connections == 0
        {
            return Err(invalid("D28 runtime/transport limits are inconsistent"));
        }
        let receiver = Receiver::try_new(ReceiverConfig {
            max_peers: config.max_members,
            max_requests: config.max_requests,
            request_timeout: config.request_timeout,
            status_ttl: config.status_ttl,
        })
        .ok_or_else(|| invalid("invalid D28 receiver limits"))?;
        let responder = Responder::try_new(SenderConfig {
            max_peers: config.max_members,
            max_outgoing: config.max_outgoing,
            request_timeout: config.request_timeout,
            status_ttl: config.status_ttl,
        })
        .ok_or_else(|| invalid("invalid D28 responder limits"))?;
        Ok(Self {
            handle,
            pull: NetworkThreadPull::default(),
            peers: Vec::new(),
            generation: 0,
            candidate_cursor: None,
            rotate_at: 0,
            rx_cursor: 0,
            receiver,
            responder,
            incoming: AdmissionBudget::new(config.max_incoming),
            outgoing: Vec::new(),
            sequence: 0,
            last_status: None,
            status_at: 0,
            dial_at: 0,
            addresses: BftAddressMap::new(),
            conflicts: Vec::new(),
            conflict_scope: None,
        })
    }
    fn reset_scope(&mut self) {
        // Keep schedulers and their budgets; remove only retired roster metadata.
        for peer in &mut self.peers {
            if let Some(member) = peer.member.take() {
                self.receiver.remove(member);
                self.responder.remove(member);
            }
            peer.public = None;
            peer.last_sequence = None;
            peer.last_hello = None;
        }
        self.outgoing.clear();
        self.last_status = None;
        self.status_at = 0;
        self.conflicts.clear();
        self.conflict_scope = None;
    }
    fn sessions(&mut self, config: &RuntimeConfig, now: u64) -> io::Result<()> {
        // Roster identities keep their place. Unauthenticated/base-only sessions
        // get a finite hello window, then rotate without closing the shared STP
        // connection. A stable key cursor prevents the first base peers winning
        // every window when the transport has more sessions than our BFT bound.
        self.peers.retain(|peer| {
            self.pull.current_connections.iter().any(|(address, hash)| {
                address.connection_key() == peer.address.connection_key() && *hash == peer.handshake
            })
        });
        if now >= self.rotate_at {
            self.peers.retain(|peer| peer.member.is_some());
            self.rotate_at = now
                .checked_add(2_000)
                .ok_or_else(|| core_error(Error::Exhausted))?;
        }
        self.pull
            .current_connections
            .sort_unstable_by_key(|(address, _)| address.connection_key());
        let connections = &self.pull.current_connections;
        let start = self.candidate_cursor.map_or(0, |cursor| {
            connections.partition_point(|(address, _)| address.connection_key() <= cursor)
        });
        for (address, handshake) in connections[start..].iter().chain(&connections[..start]) {
            if self.peers.len() == config.max_connections {
                break;
            }
            if self
                .peers
                .iter()
                .any(|p| p.address.connection_key() == address.connection_key())
            {
                continue;
            }
            self.candidate_cursor = Some(address.connection_key());
            self.generation = next(self.generation)?;
            self.peers.push(Peer {
                address: address.clone(),
                handshake: *handshake,
                generation: self.generation,
                member: None,
                public: None,
                last_hello: None,
                last_sequence: None,
                last_height: 0,
                last_packet_utc: 0,
                next_control: true,
            });
        }
        Ok(())
    }
    fn receive<S: Storage>(
        &mut self,
        engine: &mut Engine<S>,
        config: &RuntimeConfig,
        events: &mut Events,
        local: PubKeyID,
        now: u64,
    ) -> io::Result<()> {
        let scope = engine.view().map(|d| (d.height, d.round));
        if self.conflict_scope != scope {
            self.conflicts.clear();
            self.conflict_scope = scope;
        }
        // Only move the independently byte/count-bounded pulled inbox; no body
        // is copied. Cheap classification is bounded by those transport caps.
        // Expensive parsing/signature work has an additional PER-TICK cap, fair
        // across sources and between control/live votes and complete bodies.
        let mut inbox: Vec<[VecDeque<Vec<u8>>; 2]> = (0..self.peers.len())
            .map(|_| [VecDeque::new(), VecDeque::new()])
            .collect();
        for (connection, bytes) in self.pull.received_unreliable_messages.drain(..) {
            let Some(index) = self
                .peers
                .iter()
                .position(|p| p.address.connection_key() == connection)
            else {
                continue;
            };
            let bulk = bytes.first() == Some(&wire::FRAME_TAG) && bytes.len() > 160;
            inbox[index][usize::from(bulk)].push_back(bytes);
        }
        for _ in 0..config.max_receive_per_tick {
            let n = self.peers.len();
            let Some(index) = (0..n)
                .map(|i| (self.rx_cursor + i) % n)
                .find(|&i| inbox[i].iter().any(|q| !q.is_empty()))
            else {
                break;
            };
            self.rx_cursor = (index + 1) % n;
            let preferred = usize::from(!self.peers[index].next_control);
            let class = if inbox[index][preferred].is_empty() {
                1 - preferred
            } else {
                preferred
            };
            self.peers[index].next_control = class != 0;
            let bytes = inbox[index][class].pop_front().expect("nonempty source");
            if bytes.first() == Some(&wire::HELLO_TAG) {
                if let Some(public) = verify_hello(&bytes, &self.peers[index].handshake, local) {
                    if let Some(member) = engine
                        .c()
                        .members()
                        .iter()
                        .position(|m| m.public_key == public.0)
                    {
                        // One authenticated session per roster identity. A
                        // second connection cannot multiply source priority.
                        if self
                            .peers
                            .iter()
                            .enumerate()
                            .any(|(i, p)| i != index && p.member == Some(member as u32))
                        {
                            continue;
                        }
                        let peer = &mut self.peers[index];
                        if peer.public.is_some_and(|old| old != public) {
                            continue;
                        }
                        peer.public = Some(public);
                        peer.member = Some(member as u32);
                    }
                }
                continue;
            }
            let peer = &mut self.peers[index];
            let Some(member) = peer.member else {
                continue;
            };
            peer.last_packet_utc = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs()
                .min(i64::MAX as u64) as i64;
            match bytes.first() {
                Some(tag) if *tag == wire::STATUS_TAG => {
                    let Ok(status) = wire::decode_status(&bytes) else {
                        continue;
                    };
                    if peer.last_sequence.is_some_and(|old| status.sequence <= old)
                        || status.request.is_some_and(|token| token > status.sequence)
                    {
                        continue;
                    }
                    peer.last_sequence = Some(status.sequence);
                    peer.last_height = status.snapshot.height;
                    let update = StatusUpdate {
                        peer: member,
                        generation: peer.generation,
                        sequence: status.sequence,
                        status: status.snapshot,
                    };
                    let _ = self.receiver.update(update, now);
                    let _ = self.responder.update(
                        RequestStatus {
                            update,
                            request: status.request,
                        },
                        now,
                    );
                }
                Some(tag) if *tag == wire::FRAME_TAG => {
                    let Some(_permit) = self.incoming.try_acquire() else {
                        continue;
                    };
                    let Ok(decoded) = wire::decode_frame(&bytes, config.limits()) else {
                        continue;
                    };
                    // Request tokens classify replies, never gate a late or
                    // unsolicited authenticated complete frame's validity.
                    decoded.with_frame(|frame| {
                        // The host validates chain state, but must never see a
                        // body whose canonical height disagrees with its header.
                        if frame.proposal.is_some_and(|p| {
                            !canonical_body(p.body, p.header.content.height, config.max_body)
                        }) {
                            return Ok(());
                        }
                        record_conflicts(engine, frame, &mut self.conflicts, config.max_members);
                        tolerate_stale(engine.receive(frame, Status::Indeterminate, events))
                    })?;
                }
                _ => {}
            }
        }
        // STP is best effort. Excess frames are dropped rather than building a
        // second unbounded backlog; guided requests/status and hello retry them.
        // The source/class cursor survives this drop so saturation cannot keep
        // choosing the first peer or control class forever.
        Ok(())
    }
    fn exchange<S: Storage>(
        &mut self,
        engine: Option<&Engine<S>>,
        config: &RuntimeConfig,
        key: &SigningKey,
        events: &mut Events,
        now: u64,
    ) -> io::Result<()> {
        let mut push = NetworkThreadPush {
            // Never omit a base-protocol connection merely because the bounded
            // BFT peer table did not admit it: omission is a shared-session kill.
            wanted_connections: std::mem::take(&mut self.pull.current_connections),
            ..Default::default()
        };
        if now >= self.dial_at {
            push.initiate_connections.extend(
                config
                    .seeds
                    .iter()
                    .filter(|address| {
                        !push
                            .wanted_connections
                            .iter()
                            .any(|(old, _)| old.connection_key() == address.connection_key())
                    })
                    .cloned(),
            );
            self.dial_at = now
                .checked_add(1_000)
                .ok_or_else(|| core_error(Error::Exhausted))?;
        }
        for peer in &mut self.peers {
            if peer
                .last_hello
                .is_none_or(|old| now.saturating_sub(old) >= 1_000)
            {
                push.send_unreliable.push((
                    peer.address.connection_key(),
                    hello_packet(key, &peer.handshake),
                ));
                peer.last_hello = Some(now);
            }
        }
        if let Some(engine) = engine {
            let status = engine.status();
            if status != self.last_status || now >= self.status_at {
                if let Some(snapshot) = status {
                    self.sequence = next(self.sequence)?;
                    self.receiver
                        .poll(snapshot, self.sequence, now)
                        .map_err(core_error)?;
                    for peer in &self.peers {
                        if let Some(member) = peer.member {
                            let bytes = wire::encode_status(wire::WireStatus {
                                sequence: self.sequence,
                                request: self.receiver.request_for(member),
                                snapshot,
                            })
                            .map_err(core_error)?;
                            push.send_unreliable
                                .push((peer.address.connection_key(), bytes));
                        }
                    }
                }
                self.last_status = status;
                self.status_at = now
                    .checked_add(millis(config.status_interval)?)
                    .ok_or_else(|| core_error(Error::Exhausted))?;
            }
            for vote in events.votes.iter_mut().filter_map(Option::take) {
                if engine.view().is_none_or(|d| {
                    d.height != vote.content.height || d.round != vote.content.round
                }) {
                    continue;
                }
                let bytes = wire::encode_frame(
                    Frame {
                        message: Message::Ordinary,
                        outer: Some(&vote),
                        proposal: None,
                        groups: &[],
                    },
                    None,
                    config.limits(),
                )
                .map_err(core_error)?;
                for peer in self.peers.iter().filter(|p| p.member.is_some()) {
                    push.send_unreliable
                        .push((peer.address.connection_key(), bytes.clone()));
                }
            }
            let mut sink = Replies {
                peers: &self.peers,
                handle: &self.handle,
                limits: config.limits(),
                outgoing: &mut self.outgoing,
                error: None,
            };
            self.responder
                .poll(engine, now, &mut sink)
                .map_err(core_error)?;
            if let Some(error) = sink.error {
                return Err(core_error(error));
            }
        }
        let mut handoff_permits = Vec::with_capacity(self.outgoing.len());
        for Outgoing { key, bytes, permit } in self.outgoing.drain(..) {
            if let Some(peer) = self
                .peers
                .iter()
                .find(|p| p.member == Some(key.peer) && p.generation == key.generation)
            {
                push.send_bulk.push((peer.address.connection_key(), bytes));
                handoff_permits.push(permit);
            }
        }
        self.pull = self
            .handle
            .service(push)
            .ok_or_else(|| io::Error::other("D28 shared transport stopped"))?;
        // App buffers were MOVED, not cloned, into the separately bounded
        // transport owner. Keep permits through the handoff, not a fake ACK.
        drop(handoff_permits);
        self.sessions(config, now)?;
        self.addresses = BftAddressMap::new();
        for peer in &self.peers {
            if let Some(public) = peer.public {
                self.addresses.insert(&public, &peer.address, None);
                self.addresses
                    .last_packet_utcs
                    .insert(public, peer.last_packet_utc);
            }
        }
        Ok(())
    }
    fn peer_info(&self) -> Vec<PeerInfo> {
        self.peers
            .iter()
            .filter_map(|p| {
                p.public.map(|public| PeerInfo {
                    connected: true,
                    root_public_bft_key: Some(public),
                    latest_status_request_height: p.last_height,
                })
            })
            .collect()
    }
}

fn record_conflicts<S: Storage>(
    engine: &Engine<S>,
    frame: Frame<'_>,
    conflicts: &mut Vec<VoteConflict>,
    max: usize,
) {
    let Some(d) = engine.view() else {
        return;
    };
    for vote in frame
        .outer
        .into_iter()
        .chain(frame.groups.iter().flat_map(|g| g.votes.iter()))
    {
        if vote.content.height != d.height || vote.content.round != d.round {
            continue;
        }
        let row = match vote.content.kind {
            VoteKind::Prevote => engine.prevotes(),
            VoteKind::Precommit => engine.precommits(),
            VoteKind::Enter => continue,
        };
        let Some(first) = row
            .get(vote.content.signer as usize)
            .and_then(Option::as_ref)
        else {
            continue;
        };
        if first.content.id != vote.content.id
            && vote.verify(engine.c())
            && conflicts.len() < max.saturating_mul(2)
            && !conflicts.iter().any(|c| {
                c.first.content.kind == vote.content.kind
                    && c.first.content.signer == vote.content.signer
            })
        {
            conflicts.push(VoteConflict {
                first: *first,
                second: *vote,
            });
        }
    }
}

fn snapshot<'a, S: Storage>(
    engine: &'a Engine<S>,
    work: &HostWork,
    config: &'a RuntimeConfig,
    roster: &'a [SortedRosterMember],
    public_key: PubKeyID,
    conflicts: &'a [VoteConflict],
) -> Snapshot<'a> {
    let d = engine.view().expect("initialized");
    let pending = d.pending.map(|pending| {
        let proposal = engine.proposal(pending.proposal);
        PendingDecision {
            height: d.height,
            round: proposal.header.content.round,
            id: proposal.header.content.id,
            body: proposal.body,
            validity: d.statuses[pending.proposal.index],
            reason: work.reason(engine.body_tag(pending.proposal)),
            certificate: engine.group(pending.certificate),
        }
    });
    Snapshot {
        height: d.height,
        round: d.round,
        step: d.step,
        public_key,
        roster,
        active_roster_max_n: config.active_roster_max_n,
        namespace: engine.c().domain(),
        locked_round: d.locked_round,
        valid_round: d.valid_round,
        prevotes: engine.prevotes(),
        precommits: engine.precommits(),
        pending,
        conflicts,
        catching_up: false,
    }
}

/// Run until transport/storage/finite-domain failure or cancellation. The host
/// must refuse unimportable active legacy WAL before calling this function and
/// must bind the durable backend to this signing public key.
pub async fn run<B: AtomicBlockStore + Send + 'static>(
    key: SigningKey,
    backend: B,
    config: RuntimeConfig,
    host: HostCallbacks,
) -> io::Result<()> {
    chain_height(config.initial_height)?;
    let mut store = Some(
        durable::DurableStore::open(
            backend,
            durable::Limits {
                max_members: config.max_members,
                max_body: config.max_body,
            },
        )
        .map_err(core_error)?,
    );
    let mut network = Network::new(&config)?;
    let mut engine = None;
    let mut height = config.initial_height;
    let mut roster = config.initial_roster.clone();
    let mut namespace = config.vote_namespace;
    let public_key = PubKeyID(VerificationKeyBytes::from(&key).into());
    let mut work = HostWork::default();
    let mut events = Events::default();
    let start = Instant::now();
    let mut last_tick = 0;
    let mut peers_at = 0;
    let mut acknowledged: Option<HostTip> = None;
    eprintln!("D28 bounded engine: starting at height {height}; guided receiver-selected STP");
    loop {
        let now = millis(start.elapsed())?;
        // Request zero to observe the current trusted tip even while a saved
        // signing height is ahead of the reconstructed chain. No peer supplies it.
        let read_tip = (host.read_tip.0)(0);
        let tip = match (read_tip, acknowledged.take()) {
            (Some(a), Some(b)) => Some(if a.height >= b.height { a } else { b }),
            (a, b) => a.or(b),
        };
        if let Some(tip) = tip {
            let successor = chain_height(next(tip.height)?)?;
            if successor >= height {
                if successor > height {
                    // Export pending/conflicting evidence with its OLD roster
                    // before trusted INIT retires the bounded height workspace.
                    if let Some(engine) = &engine {
                        (host.access.0)(
                            &snapshot(
                                engine,
                                &work,
                                &config,
                                &roster,
                                public_key,
                                &network.conflicts,
                            ),
                            &network.addresses,
                        );
                    }
                    height = successor;
                    roster = tip.next_roster;
                    namespace = tip.next_vote_namespace;
                } else {
                    // At the same height configuration is immutable. The host
                    // may finish reconstructing the exact configuration we need.
                    let expected =
                        config.chain_config(&tip.next_roster, tip.next_vote_namespace)?;
                    let current = config.chain_config(&roster, namespace)?;
                    if expected != current {
                        return Err(invalid("host changed D28 configuration at the same height"));
                    }
                }
            }
        }
        let agreed = config.chain_config(&roster, namespace)?;
        if engine.is_none() {
            let saved = store.as_ref().and_then(Storage::descriptor).cloned();
            let waiting = saved.as_ref().is_some_and(|d| d.height > height);
            if !waiting {
                if let Some(agreed) = &agreed {
                    if saved
                        .as_ref()
                        .is_some_and(|d| d.height == height && d.config != agreed.hash())
                    {
                        return Err(invalid(
                            "restored D28 configuration disagrees with host chain",
                        ));
                    }
                    let storage = store.take().expect("unattached store");
                    let mut attached = if storage.descriptor().is_some() {
                        Engine::recover_with_emission(
                            storage,
                            key.clone(),
                            Emission::Guided,
                            &mut events,
                        )
                        .map_err(core_error)?
                    } else {
                        let mut fresh = Engine::boot(storage, key.clone(), agreed.clone())
                            .map_err(core_error)?;
                        fresh.set_emission(Emission::Guided);
                        fresh
                    };
                    attached
                        .init(agreed, height, 0, &mut events)
                        .map_err(core_error)?;
                    engine = Some(attached);
                    last_tick = now;
                }
            }
            if engine.is_none() {
                if let Some(d) = saved {
                    (host.access.0)(
                        &Snapshot {
                            height: d.height,
                            round: d.round,
                            step: d.step,
                            public_key,
                            roster: &roster,
                            active_roster_max_n: config.active_roster_max_n,
                            namespace,
                            locked_round: d.locked_round,
                            valid_round: d.valid_round,
                            prevotes: &[],
                            precommits: &[],
                            pending: None,
                            conflicts: &[],
                            catching_up: true,
                        },
                        &network.addresses,
                    );
                }
                network.pull.received_unreliable_messages.clear();
                network.exchange::<durable::DurableStore<B>>(
                    None,
                    &config,
                    &key,
                    &mut events,
                    now,
                )?;
                last_tick = now;
                tokio::time::sleep(config.poll_interval).await;
                continue;
            }
        }
        let engine = engine.as_mut().expect("attached");
        if engine.view().is_some_and(|d| height > d.height) {
            let Some(agreed) = &agreed else {
                // A chain with no active members cannot authorize new signing.
                // Keep the old descriptor intact until an authorized roster exists.
                work.retire();
                network.pull.received_unreliable_messages.clear();
                network.exchange::<durable::DurableStore<B>>(
                    None,
                    &config,
                    &key,
                    &mut events,
                    now,
                )?;
                last_tick = now;
                tokio::time::sleep(config.poll_interval).await;
                continue;
            };
            work.retire();
            events = Events::default();
            network.reset_scope();
            engine
                .init(agreed, height, 0, &mut events)
                .map_err(core_error)?;
            last_tick = now;
        }
        network.receive(engine, &config, &mut events, public_key, now)?;
        work.schedule(engine, &host, &mut events, now, config.max_body)?;
        acknowledged = work.poll(engine, &mut events, now, &config, &host).await?;
        if acknowledged.is_some() {
            continue;
        }
        engine
            .elapse(now.saturating_sub(last_tick), &mut events)
            .map_err(core_error)?;
        last_tick = now;
        work.schedule(engine, &host, &mut events, now, config.max_body)?;
        (host.access.0)(
            &snapshot(
                engine,
                &work,
                &config,
                &roster,
                public_key,
                &network.conflicts,
            ),
            &network.addresses,
        );
        if now >= peers_at && work.peers.is_none() {
            work.peers = Some((host.update_peers.0)(network.peer_info()));
            peers_at = now
                .checked_add(1_000)
                .ok_or_else(|| core_error(Error::Exhausted))?;
        }
        network.exchange(Some(engine), &config, &key, &mut events, now)?;
        tokio::time::sleep(config.poll_interval).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::{Ipv6Addr, UdpSocket};

    fn fixture() -> (RuntimeConfig, SigningKey) {
        let identity = crate::stp::new_keypair_from_connect_magic1(
            crate::CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2b,
        )
        .unwrap();
        let socket = UdpSocket::bind((Ipv6Addr::LOCALHOST, 0)).unwrap();
        let port = socket.local_addr().unwrap().port();
        drop(socket);
        let transport =
            SharedTransport::new(vec![identity], port, None, (1_000_000, 512 * 1024, 8 << 20));
        let roster = (1..=2)
            .map(|i| SortedRosterMember {
                pub_key: PubKeyID(VerificationKeyBytes::from(&SigningKey::from([i; 32])).into()),
                stake: 1,
                cumulative_stake: u64::from(i),
            })
            .collect();
        let mut config = RuntimeConfig::new(1, roster, [11; 32], 2, transport, vec![]);
        config.max_body = 4096;
        config.max_connections = 2;
        (config, SigningKey::from([1; 32]))
    }
    fn address(port: u16) -> STPAddress {
        let identity = crate::new_keypair_from_connect_magic1_with_seed(
            crate::CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2b,
            [3; 32],
        )
        .unwrap();
        STPAddress::from(Ipv6Addr::LOCALHOST, port, &identity)
    }

    #[test]
    fn candidates_rotate_without_evicting_roster_or_dropping_shared_sessions() {
        let (config, _) = fixture();
        let mut network = Network::new(&config).unwrap();
        network.pull.current_connections = (1..=6)
            .map(|i| (address(40_000 + i), [i as u8; 64]))
            .collect();
        network.sessions(&config, 0).unwrap();
        let admitted = network.peers[0].address.connection_key();
        network.peers[0].member = Some(0);
        network.peers[0].public = Some(config.initial_roster[0].pub_key);
        let c = config
            .chain_config(&config.initial_roster, config.vote_namespace)
            .unwrap()
            .unwrap();
        let local = SigningKey::from([3; 32]); // observer can admit both remote roster members
        let mut engine = Engine::boot(
            super::super::MemoryStore::default(),
            local.clone(),
            c.clone(),
        )
        .unwrap();
        let mut events = Events::default();
        engine.init(&c, 1, 0, &mut events).unwrap();
        let mut seen = Vec::new();
        for now in [0, 2_000, 4_000, 6_000, 8_000, 10_000] {
            network.sessions(&config, now).unwrap();
            assert_eq!(network.peers.len(), 2);
            assert!(network
                .peers
                .iter()
                .any(|p| p.member == Some(0) && p.address.connection_key() == admitted));
            assert_eq!(
                network.pull.current_connections.len(),
                6,
                "base sessions remain wanted"
            );
            for peer in &network.peers {
                if !seen.contains(&peer.address.connection_key()) {
                    seen.push(peer.address.connection_key());
                }
                if peer.address.port == 40_006 {
                    network.pull.received_unreliable_messages.push_back((
                        peer.address.connection_key(),
                        hello_packet(&SigningKey::from([2; 32]), &peer.handshake),
                    ));
                }
            }
            network
                .receive(
                    &mut engine,
                    &config,
                    &mut events,
                    PubKeyID(VerificationKeyBytes::from(&local).into()),
                    now,
                )
                .unwrap();
        }
        assert!(network
            .peers
            .iter()
            .any(|p| p.member == Some(1) && p.address.port == 40_006));
        assert_eq!(
            seen.len(),
            6,
            "a finalizer behind base peers eventually gets a hello window"
        );
    }

    #[test]
    fn saturated_receive_budget_rotates_sources_and_classes_and_leaves_timers_online() {
        let (mut config, key) = fixture();
        config.max_receive_per_tick = 1;
        let c = config
            .chain_config(&config.initial_roster, config.vote_namespace)
            .unwrap()
            .unwrap();
        let mut engine =
            Engine::boot(super::super::MemoryStore::default(), key.clone(), c.clone()).unwrap();
        let mut events = Events::default();
        engine.init(&c, 1, 0, &mut events).unwrap();
        let public = PubKeyID(VerificationKeyBytes::from(&key).into());
        let mut network = Network::new(&config).unwrap();
        network.pull.current_connections = (1..=2)
            .map(|i| (address(40_000 + i), [i as u8; 64]))
            .collect();
        network.sessions(&config, 0).unwrap();
        for (i, peer) in network.peers.iter_mut().enumerate() {
            peer.member = Some(i as u32);
        }
        let initial_timer = engine.view().unwrap().timers[0].remaining;
        let mut served = Vec::new();
        for tick in 0..12 {
            let before: Vec<_> = network.peers.iter().map(|p| p.last_sequence).collect();
            for peer in &network.peers {
                for sequence in 1..=100 {
                    let bytes = wire::encode_status(wire::WireStatus {
                        sequence: tick * 100 + sequence,
                        request: None,
                        snapshot: engine.status().unwrap(),
                    })
                    .unwrap();
                    network
                        .pull
                        .received_unreliable_messages
                        .push_back((peer.address.connection_key(), bytes));
                    // Invalid bulk still consumes a bounded processing turn.
                    let mut bulk = vec![0; 200];
                    bulk[0] = wire::FRAME_TAG;
                    network
                        .pull
                        .received_unreliable_messages
                        .push_back((peer.address.connection_key(), bulk));
                }
            }
            network
                .receive(&mut engine, &config, &mut events, public, tick)
                .unwrap();
            let changed: Vec<_> = network
                .peers
                .iter()
                .enumerate()
                .filter(|(i, p)| p.last_sequence != before[*i])
                .map(|(i, _)| i)
                .collect();
            assert!(changed.len() <= 1);
            served.extend(changed);
            assert!(network.pull.received_unreliable_messages.is_empty());
            engine.elapse(1, &mut events).unwrap();
        }
        assert_eq!(
            served,
            [0, 1, 0, 1, 0, 1],
            "body turns interleave instead of starving behind control"
        );
        assert_eq!(
            engine.view().unwrap().timers[0].remaining,
            initial_timer - 12
        );
    }

    fn body(height: u32) -> Vec<u8> {
        use zcash_primitives::bft::FatPointerToBftBlock;
        BftBlock {
            version: 1,
            height,
            previous_block_fat_ptr: FatPointerToBftBlock::null(),
            headers: vec![],
            slashforks: vec![],
            do_not_include_until_bc_height: 0,
        }
        .zcash_serialize_to_vec()
        .unwrap()
    }

    fn decision_packet(c: &Config, bytes: &[u8], limits: wire::Limits) -> Vec<u8> {
        use super::super::{CertKind, Header, ProposalContent, VoteContent, Votes};
        let id = c.body_id(bytes);
        let header = Header::sign(
            c,
            &SigningKey::from([2; 32]),
            ProposalContent {
                height: 1,
                round: 0,
                valid_round: None,
                id,
                signer: 1,
            },
        );
        let votes: Vec<_> = (0..2)
            .map(|i| {
                Vote::sign(
                    c,
                    &SigningKey::from([i as u8 + 1; 32]),
                    VoteContent {
                        height: 1,
                        round: 0,
                        kind: VoteKind::Precommit,
                        id: Some(id),
                        signer: i,
                    },
                )
            })
            .collect();
        wire::encode_frame(
            Frame {
                message: Message::Dec,
                outer: None,
                proposal: Some(Proposal {
                    header: &header,
                    body: bytes,
                }),
                groups: &[Group {
                    kind: CertKind::Decision { round: 0, id },
                    votes: Votes::Dense(&votes),
                }],
            },
            None,
            limits,
        )
        .unwrap()
    }

    #[test]
    fn received_body_is_checked_before_pinning_or_validation_even_with_a_certificate() {
        let (config, key) = fixture();
        let c = config
            .chain_config(&config.initial_roster, config.vote_namespace)
            .unwrap()
            .unwrap();
        let mut engine =
            Engine::boot(super::super::MemoryStore::default(), key.clone(), c.clone()).unwrap();
        let mut events = Events::default();
        engine.init(&c, 1, 0, &mut events).unwrap();
        let mut network = Network::new(&config).unwrap();
        network
            .pull
            .current_connections
            .push((address(40_001), [1; 64]));
        network.sessions(&config, 0).unwrap();
        network.peers[0].member = Some(1);
        let mut trailing = body(1);
        trailing.push(0);
        let writes = engine.storage().writes;
        for bytes in [body(2), trailing, vec![0; 4]] {
            network.pull.received_unreliable_messages.push_back((
                address(40_001).connection_key(),
                decision_packet(&c, &bytes, config.limits()),
            ));
            network
                .receive(
                    &mut engine,
                    &config,
                    &mut events,
                    config.initial_roster[0].pub_key,
                    0,
                )
                .unwrap();
            assert!(engine.view().unwrap().pending.is_none());
            assert_eq!(engine.storage().writes, writes);
        }
        network.pull.received_unreliable_messages.push_back((
            address(40_001).connection_key(),
            decision_packet(&c, &body(1), config.limits()),
        ));
        network
            .receive(
                &mut engine,
                &config,
                &mut events,
                config.initial_roster[0].pub_key,
                0,
            )
            .unwrap();
        assert!(
            engine.view().unwrap().pending.is_some(),
            "canonical matching fixture is admitted"
        );
    }

    #[test]
    fn completed_validation_fences_new_signing_and_preserves_pending_and_conflicting_evidence() {
        use super::super::{MemoryStore, VoteContent};
        use std::sync::atomic::{AtomicU64, Ordering};
        let (config, key) = fixture();
        let c = config
            .chain_config(&config.initial_roster, config.vote_namespace)
            .unwrap()
            .unwrap();
        let mut engine = Engine::boot(MemoryStore::default(), key.clone(), c.clone()).unwrap();
        let mut events = Events::default();
        engine.init(&c, 1, 0, &mut events).unwrap();
        let mut network = Network::new(&config).unwrap();
        network
            .pull
            .current_connections
            .push((address(40_001), [1; 64]));
        network.sessions(&config, 0).unwrap();
        network.peers[0].member = Some(1);
        for id in [Some(c.body_id(&body(1))), None] {
            let vote = Vote::sign(
                &c,
                &SigningKey::from([2; 32]),
                VoteContent {
                    height: 1,
                    round: 0,
                    kind: VoteKind::Prevote,
                    id,
                    signer: 1,
                },
            );
            let bytes = wire::encode_frame(
                Frame {
                    message: Message::Ordinary,
                    outer: Some(&vote),
                    proposal: None,
                    groups: &[],
                },
                None,
                config.limits(),
            )
            .unwrap();
            network
                .pull
                .received_unreliable_messages
                .push_back((address(40_001).connection_key(), bytes));
            network
                .receive(
                    &mut engine,
                    &config,
                    &mut events,
                    config.initial_roster[0].pub_key,
                    0,
                )
                .unwrap();
        }
        network.pull.received_unreliable_messages.push_back((
            address(40_001).connection_key(),
            decision_packet(&c, &body(1), config.limits()),
        ));
        network
            .receive(
                &mut engine,
                &config,
                &mut events,
                config.initial_roster[0].pub_key,
                0,
            )
            .unwrap();
        let height = Arc::new(AtomicU64::new(0));
        let read_height = height.clone();
        let roster = config.initial_roster.clone();
        let host = HostCallbacks {
            propose: ClosureToProposeNewBlock(Arc::new(|| Box::pin(std::future::pending()))),
            validate: ClosureToValidateProposedBlock(Arc::new(move |_| {
                let height = height.clone();
                Box::pin(async move {
                    height.store(1, Ordering::SeqCst);
                    (TMStatus::Pass, TMStatusReason::None)
                })
            })),
            push_decided: ClosureToPushDecidedBlock(Arc::new(|_, _, _| {
                panic!("stale validation must not decide")
            })),
            read_tip: ClosureToReadDecidedTip(Arc::new(move |_| {
                Some(HostTip {
                    height: read_height.load(Ordering::SeqCst),
                    block_id: crate::ValueId::NIL,
                    next_roster: roster.clone(),
                    next_vote_namespace: [11; 32],
                    next_strict_quorum: true,
                })
            })),
            update_peers: ClosureToUpdatePeers(Arc::new(|_| Box::pin(async {}))),
            access: AccessD28::default(),
        };
        let mut work = HostWork::default();
        work.schedule(&engine, &host, &mut events, 0, config.max_body)
            .unwrap();
        let writes = engine.storage().writes;
        let acknowledged = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(work.poll(&mut engine, &mut events, 0, &config, &host))
            .unwrap()
            .unwrap();
        assert_eq!(acknowledged.height, 1);
        assert_eq!(
            engine.storage().writes,
            writes,
            "completion must not apply validity or sign at stale height"
        );
        let view = snapshot(
            &engine,
            &work,
            &config,
            &config.initial_roster,
            config.initial_roster[0].pub_key,
            &network.conflicts,
        );
        let pending = view.pending.unwrap();
        assert_eq!(pending.validity, Status::Indeterminate);
        assert_eq!(pending.body, body(1));
        assert_eq!(pending.certificate.votes.len(), 2);
        assert_eq!(view.conflicts.len(), 1);
        assert_ne!(
            view.conflicts[0].first.content.id,
            view.conflicts[0].second.content.id
        );
    }

    #[test]
    fn hello_admission_requires_a_distinct_remote_identity_and_current_session() {
        let local = SigningKey::from([1; 32]);
        let remote = SigningKey::from([2; 32]);
        let public = PubKeyID(VerificationKeyBytes::from(&local).into());
        let remote_public = PubKeyID(VerificationKeyBytes::from(&remote).into());
        assert_eq!(
            verify_hello(&hello_packet(&local, &[3; 64]), &[3; 64], public),
            None
        );
        assert_eq!(
            verify_hello(&hello_packet(&remote, &[3; 64]), &[3; 64], public),
            Some(remote_public)
        );
        assert_eq!(
            verify_hello(&hello_packet(&remote, &[3; 64]), &[4; 64], public),
            None
        );
    }
}
