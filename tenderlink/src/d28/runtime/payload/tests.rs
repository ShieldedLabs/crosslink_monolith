use super::*;
use crate::d28::runtime::tests::{body, decision_packet, metrics_fixture};
use crate::d28::{
    BodyTag, CertKind, Config, Emission, Group, Header, MemoryStore, Message, Proposal,
    ProposalContent, Step, Vote, VoteContent, VoteKind, Votes,
};
use ed25519_zebra::SigningKey;

struct Node {
    config: RuntimeConfig,
    engine: Engine<MemoryStore>,
    events: Events,
    network: Network,
}
impl Node {
    fn new() -> Self {
        let (mut config, _, mut engine, events, mut network) = metrics_fixture();
        config.payload_mode = PayloadMode::Outlined;
        engine.set_emission(Emission::Guided);
        for (i, peer) in network.peers.iter_mut().enumerate() {
            peer.member = Some(i as u32);
        }
        network.payload.sync(&engine, &network.peers, 0);
        Self {
            config,
            engine,
            events,
            network,
        }
    }
    fn inject(&mut self, source: usize, bytes: Vec<u8>, now: u64) -> bool {
        let connection = self.network.peers[source].address.connection_key();
        let before = self
            .config
            .metrics
            .as_ref()
            .unwrap()
            .snapshot()
            .processed
            .messages;
        self.network
            .pull
            .received_unreliable_messages
            .push_back((connection, bytes));
        self.network
            .receive(
                &mut self.engine,
                &self.config,
                &mut self.events,
                self.config.initial_roster[0].pub_key,
                now,
            )
            .unwrap();
        self.config
            .metrics
            .as_ref()
            .unwrap()
            .snapshot()
            .processed
            .messages
            != before
    }
    fn poll(&mut self, now: u64, bulk_ready: bool) -> (NetworkThreadPush, PayloadMetricsSnapshot) {
        self.network
            .payload
            .sync(&self.engine, &self.network.peers, now);
        let mut push = NetworkThreadPush::default();
        let mut counts = PayloadMetricsSnapshot::default();
        self.network
            .payload
            .offer(
                &self.engine,
                &self.network.peers,
                |_| bulk_ready,
                &self.config,
                now,
                &mut push,
                &mut counts,
            )
            .unwrap();
        (push, counts)
    }
    fn inline(&self, bytes: &[u8]) -> Vec<u8> {
        decision_packet(self.engine.c(), bytes, self.config.limits())
    }
    fn outline(&self, bytes: &[u8]) -> Vec<u8> {
        outline(&self.inline(bytes), self.engine.c(), self.config.limits())
    }
    fn request(&self, source: usize) -> BodyRequest {
        self.network.payload.peers[source].sent.unwrap().request
    }
    fn finish_validation(&mut self) {
        let d = self.engine.view().unwrap();
        let slot = d.pending.unwrap().proposal;
        let tag = BodyTag {
            height: d.height,
            header: slot.header,
            generation: d.body_generations[slot.index],
        };
        self.engine
            .validity(tag, Status::Valid, &mut self.events)
            .unwrap();
        assert_eq!(self.engine.view().unwrap().step, Step::Decided);
    }
}
fn outline(inline: &[u8], c: &Config, limits: wire::Limits) -> Vec<u8> {
    wire::decode_frame(inline, limits)
        .unwrap()
        .with_frame(|frame| wire::encode_outlined(frame, Some(71), c.hash(), limits).unwrap())
}
fn at_round(
    c: &Config,
    body: &[u8],
    round: u64,
    decision: bool,
    limits: wire::Limits,
) -> (Header, Vec<u8>) {
    let signer = c.proposer(1, round) as u32;
    let header = Header::sign(
        c,
        &SigningKey::from([signer as u8 + 1; 32]),
        ProposalContent {
            height: 1,
            round,
            valid_round: None,
            id: c.body_id(body),
            signer,
        },
    );
    let votes: Vec<_> = (0..c.members().len() as u32)
        .map(|signer| {
            Vote::sign(
                c,
                &SigningKey::from([signer as u8 + 1; 32]),
                VoteContent {
                    height: 1,
                    round,
                    kind: VoteKind::Precommit,
                    id: Some(c.body_id(body)),
                    signer,
                },
            )
        })
        .collect();
    let groups = [Group {
        kind: CertKind::Decision {
            round,
            id: c.body_id(body),
        },
        votes: Votes::Dense(&votes),
    }];
    let packet = wire::encode_frame(
        Frame {
            message: if decision {
                Message::Dec
            } else {
                Message::Ordinary
            },
            outer: None,
            proposal: Some(Proposal {
                header: &header,
                body,
            }),
            groups: if decision { &groups } else { &[] },
        },
        None,
        limits,
    )
    .unwrap();
    (header, packet)
}

#[test]
fn cache_miss_fetch_materializes_complete_frame_then_normal_validation_decides() {
    let mut node = Node::new();
    let payload = body(1);
    let writes = node.engine.storage().writes;
    assert!(node.inject(1, node.outline(&payload), 0));
    assert_eq!(
        node.engine.storage().writes,
        writes,
        "references never enter the core"
    );
    assert!(node.engine.view().unwrap().pending.is_none());
    let (push, counts) = node.poll(1, false);
    assert!(push.send_bulk.is_empty());
    assert_eq!(counts.body_requests, 1, "Bulk stall cannot block a request");
    assert_eq!(counts.payload_bytes, 0);
    let request = wire::decode_body_request(&push.send_unreliable[0].1).unwrap();
    assert_eq!(request, node.request(1));
    assert!(node.inject(
        1,
        wire::encode_body_response(request, &payload, node.config.limits()).unwrap(),
        2
    ));
    assert!(node.network.payload.pending.is_empty());
    assert!(node.engine.view().unwrap().pending.is_some());
    node.finish_validation();
    let (push, counts) = node.poll(3, false);
    assert_eq!(
        wire::decode_body_ack(&push.send_unreliable[0].1).unwrap(),
        request
    );
    assert_eq!(counts.offered_control.messages, 1);
    assert_eq!(counts.body_requests, 0);
}

#[test]
fn cache_hit_uses_new_exact_header_and_certificate_without_body_response() {
    let mut node = Node::new();
    let payload = body(1);
    let (old, proposal) = at_round(node.engine.c(), &payload, 0, false, node.config.limits());
    assert!(node.inject(1, proposal, 0));
    assert!(node.engine.view().unwrap().slot.is_some());
    let (new, decision) = at_round(node.engine.c(), &payload, 1, true, node.config.limits());
    let decision = outline(&decision, node.engine.c(), node.config.limits());
    assert!(node.inject(1, decision, 1));
    let d = node.engine.view().unwrap();
    let slot = d.pending.unwrap().proposal;
    assert_eq!(node.engine.storage().header(slot.index), Some(&new));
    assert_ne!(slot.header, old.identity(node.engine.c()));
    let (_, offered) = node.poll(2, true);
    assert_eq!(offered.body_requests, 0);
    assert_eq!(offered.body_responses, 0);
    assert_eq!(
        node.config
            .metrics
            .as_ref()
            .unwrap()
            .snapshot()
            .payload
            .body_cache_hits,
        1
    );
    node.finish_validation();
}

#[test]
fn header_signature_scope_and_actual_body_are_checked_before_core_mutation() {
    let mut node = Node::new();
    let payload = body(1);
    let writes = node.engine.storage().writes;
    let mut bad_header = node.outline(&payload);
    bad_header[107] ^= 1; // signature inside exact header, no structural changes
    assert!(!node.inject(1, bad_header, 0));
    let mut foreign = node.outline(&payload);
    foreign[2] ^= 1;
    assert!(!node.inject(1, foreign, 0));
    assert!(node.network.payload.pending.is_empty());
    assert!(node.inject(1, node.outline(&payload), 0));
    node.poll(1, true);
    let request = node.request(1);
    for wrong in [body(2), vec![0; 4], {
        let mut b = payload.clone();
        b.push(0);
        b
    }] {
        let before = node.config.metrics.as_ref().unwrap().snapshot().phases.body_check.calls;
        assert!(!node.inject(
            1,
            wire::encode_body_response(request, &wrong, node.config.limits()).unwrap(),
            2
        ));
        assert_eq!(
            node.config.metrics.as_ref().unwrap().snapshot().phases.body_check.calls,
            before + 1,
            "rejected outlined bodies still contribute their validation work"
        );
    }
    for wrong in [
        BodyRequest {
            token: request.token + 1,
            ..request
        },
        BodyRequest {
            generation: request.generation + 1,
            ..request
        },
        BodyRequest {
            key: BodyKey {
                height: 2,
                ..request.key
            },
            ..request
        },
        BodyRequest {
            key: BodyKey {
                config: [0; 32],
                ..request.key
            },
            ..request
        },
        BodyRequest {
            key: BodyKey {
                id: [0; 32],
                ..request.key
            },
            ..request
        },
    ] {
        assert!(!node.inject(
            1,
            wire::encode_body_response(wrong, &payload, node.config.limits()).unwrap(),
            2
        ));
    }
    assert_eq!(node.engine.storage().writes, writes);
    assert_eq!(node.network.payload.pending.len(), 1);
    assert!(node.inject(
        1,
        wire::encode_body_response(request, &payload, node.config.limits()).unwrap(),
        3
    ));
    let retained = node.engine.view().unwrap().pending.unwrap().proposal;
    let before = node.engine.storage().body(retained.index).to_vec();
    assert!(!node.inject(
        1,
        wire::encode_body_response(request, &body(2), node.config.limits()).unwrap(),
        4
    ));
    assert_eq!(node.engine.storage().body(retained.index), before);
}

#[test]
fn wrong_canonical_height_and_noncanonical_bodies_never_gain_a_core_role() {
    for payload in [body(2), {
        let mut b = body(1);
        b.push(0);
        b
    }] {
        let mut node = Node::new();
        assert!(node.inject(1, node.outline(&payload), 0)); // valid header, wrong chain body
        node.poll(1, true);
        let request = node.request(1);
        let writes = node.engine.storage().writes;
        assert!(!node.inject(
            1,
            wire::encode_body_response(request, &payload, node.config.limits()).unwrap(),
            2
        ));
        assert_eq!(node.engine.storage().writes, writes);
        assert!(node.engine.view().unwrap().pending.is_none());
    }
}

#[test]
fn retry_rotates_sources_and_late_authenticated_body_still_completes() {
    let mut node = Node::new();
    assert!(node.inject(1, node.outline(&body(1)), 0));
    node.poll(1, true);
    let first = node.request(1);
    let deadline = 1 + node.config.request_timeout;
    let (_, counts) = node.poll(deadline, true);
    assert_eq!(counts.body_requests, 1);
    let next = node.request(0);
    assert!(next.token > first.token);
    assert_ne!(next.generation, first.generation);
    assert!(node.inject(
        1,
        wire::encode_body_response(first, &body(1), node.config.limits()).unwrap(),
        deadline + 1
    ));
    assert!(node.engine.view().unwrap().pending.is_some());
    assert!(node.network.payload.pending.is_empty());
}

#[test]
fn retired_height_reset_and_reconnect_cannot_resurrect_old_body_requests() {
    let mut node = Node::new();
    assert!(node.inject(1, node.outline(&body(1)), 0));
    node.poll(1, true);
    let first = node.request(1);
    node.network.peers[1].generation += 10;
    node.poll(2, true);
    assert!(!node.inject(
        1,
        wire::encode_body_response(first, &body(1), node.config.limits()).unwrap(),
        3
    ));
    let c = node.engine.c().clone();
    node.engine.init(&c, 2, 0, &mut node.events).unwrap();
    assert!(!node.inject(
        1,
        wire::encode_body_response(first, &body(1), node.config.limits()).unwrap(),
        4
    ));
    assert!(node.network.payload.pending.is_empty());
    assert!(node.network.payload.peers.iter().all(|p| p.sent.is_none()));
    assert!(!node.inject(
        1,
        outline(
            &decision_packet(&c, &body(1), node.config.limits()),
            &c,
            node.config.limits()
        ),
        4
    ));
    node.network.reset_scope();
    assert!(node.network.payload.peers.is_empty());
    assert!(node.network.payload.pending.is_empty());
}

#[test]
fn pending_is_global_bounded_coalesced_and_decision_does_not_lose_to_proposal() {
    let mut node = Node::new();
    node.config.max_requests = 1;
    let payload = body(1);
    assert!(node.inject(1, node.outline(&payload), 0));
    let expires = node.network.payload.pending[0].expires;
    let (_, proposal) = at_round(node.engine.c(), &payload, 2, false, node.config.limits());
    assert!(node.inject(
        0,
        outline(&proposal, node.engine.c(), node.config.limits()),
        10
    ));
    assert_eq!(node.network.payload.pending.len(), 1);
    assert!(node.network.payload.pending[0].evidence.priority().0);
    assert_eq!(node.network.payload.pending[0].expires, expires);
    assert!(node.inject(0, node.outline(&body(2)), 11));
    assert_eq!(
        node.network.payload.pending.len(),
        1,
        "cap is global, not per peer"
    );
    assert_eq!(
        node.network.payload.pending[0].key.id,
        node.engine.c().body_id(&payload)
    );
    node.poll(expires, true);
    assert!(
        node.network.payload.pending.is_empty(),
        "withholding source cannot pin metadata forever"
    );
}

#[test]
fn body_leases_survive_handoff_are_global_and_prevent_duplicate_queued_offers() {
    let mut node = Node::new();
    node.config.max_outgoing = 1;
    assert!(node.inject(1, node.inline(&body(1)), 0));
    let key = BodyKey {
        config: node.engine.c().hash(),
        height: 1,
        id: node.engine.c().body_id(&body(1)),
    };
    let first = BodyRequest {
        token: 1,
        generation: 19,
        key,
    };
    let other = BodyRequest {
        token: 1,
        generation: 27,
        key,
    };
    assert!(node.inject(0, wire::encode_body_request(first).unwrap(), 1));
    assert!(node.inject(1, wire::encode_body_request(other).unwrap(), 1));
    let (push, counts) = node.poll(2, true);
    assert_eq!(push.send_bulk.len(), 1);
    assert_eq!(counts.body_responses, 1);
    assert_eq!(counts.payload_bytes, body(1).len() as u64);
    assert_eq!(
        node.network
            .payload
            .peers
            .iter()
            .filter(|p| p.lease.is_some())
            .count(),
        1
    );
    let renewed = BodyRequest { token: 2, ..first };
    assert!(node.inject(0, wire::encode_body_request(renewed).unwrap(), 3));
    let (push, counts) = node.poll(3, true);
    assert!(push.send_bulk.is_empty());
    assert_eq!(
        counts.body_responses, 0,
        "new metadata/token cannot free body custody"
    );
    assert!(node.inject(0, wire::encode_body_ack(first).unwrap(), 4));
    assert_eq!(
        node.network.payload.peers[0].requested.unwrap().request,
        renewed,
        "ACK t1 cannot cancel newer t2"
    );
    let (push, _) = node.poll(4, true);
    assert_eq!(push.send_bulk.len(), 1);
    assert_eq!(
        wire::decode_body_response(&push.send_bulk[0].1, node.config.limits())
            .unwrap()
            .0,
        other
    );
    let renewed = BodyRequest { token: 2, ..other };
    assert!(node.inject(1, wire::encode_body_request(renewed).unwrap(), 5));
    let timeout = 4 + node.config.request_timeout;
    let (push, _) = node.poll(timeout, false);
    assert!(
        push.send_bulk.is_empty(),
        "timeout does not erase STP's outstanding Bulk bytes"
    );
    let (push, counts) = node.poll(timeout + 1, true);
    assert_eq!(push.send_bulk.len(), 1);
    assert_eq!(counts.duplicate_payload_bytes, body(1).len() as u64);
    let second_offer = wire::decode_body_response(&push.send_bulk[0].1, node.config.limits())
        .unwrap()
        .0;
    assert_eq!(second_offer, BodyRequest { token: 2, ..first });
    assert!(!node.inject(0, wire::encode_body_ack(first).unwrap(), timeout + 2));
    assert!(node.inject(0, wire::encode_body_ack(second_offer).unwrap(), timeout + 2));
    let (push, _) = node.poll(timeout + 3, true);
    assert_eq!(
        wire::decode_body_response(&push.send_bulk[0].1, node.config.limits())
            .unwrap()
            .0,
        renewed
    );
    assert!(!node.inject(1, wire::encode_body_ack(other).unwrap(), timeout + 4));
    assert!(node.inject(1, wire::encode_body_ack(renewed).unwrap(), timeout + 4));
}

#[test]
fn production_default_stays_inline_and_merkle_checks_leaf_geometry() {
    let (mut config, _) = crate::d28::runtime::tests::fixture();
    assert_eq!(config.payload_mode, PayloadMode::Inline);
    for leaf_bytes in [32768, 102400, 262144, 1048576] {
        config.payload_mode = PayloadMode::Merkle { leaf_bytes };
        assert!(Network::new(&config).is_ok());
    }
    for leaf_bytes in [0, 32767, 1048577, usize::MAX] {
        config.payload_mode = PayloadMode::Merkle { leaf_bytes };
        assert!(Network::new(&config).is_err());
    }
    config.payload_mode = PayloadMode::Outlined;
    assert!(Network::new(&config).is_ok());
}

#[test]
fn same_source_retry_accepts_late_issued_tokens_for_existing_interest() {
    let mut node = Node::new();
    node.network.peers.truncate(1);
    assert!(node.inject(0, node.outline(&body(1)), 0));
    node.poll(1, true);
    let first = node.request(0);
    node.poll(1 + node.config.request_timeout, true);
    assert!(node.request(0).token > first.token);
    node.poll(1 + 2 * node.config.request_timeout, true);
    assert!(node.request(0).token > first.token + 1);
    assert!(node.inject(
        0,
        wire::encode_body_response(first, &body(1), node.config.limits()).unwrap(),
        2 + 2 * node.config.request_timeout
    ));
    assert!(node.engine.view().unwrap().pending.is_some());
}

#[test]
fn cached_weaker_outline_cannot_discard_pending_decision() {
    let mut node = Node::new();
    let payload = body(1);
    let decision = node.outline(&payload);
    assert!(node.inject(1, decision, 0));
    let (_, proposal) = at_round(node.engine.c(), &payload, 0, false, node.config.limits());
    let weaker_outline = outline(&proposal, node.engine.c(), node.config.limits());
    assert!(node.inject(1, proposal, 1));
    assert!(node.engine.view().unwrap().pending.is_none());
    assert_eq!(node.network.payload.pending.len(), 1);
    assert!(node.inject(1, weaker_outline, 2));
    assert!(node.network.payload.pending.is_empty());
    assert!(
        node.engine.view().unwrap().pending.is_some(),
        "stronger pending certificate must reach core"
    );
    node.finish_validation();
}

#[test]
fn invalid_outline_metadata_cannot_replace_a_pending_decision() {
    for fault in 0..4 {
        let mut node = Node::new();
        let payload = body(1);
        let valid = node.inline(&payload);
        assert!(node.inject(1, node.outline(&payload), 0));
        let original = *node.network.payload.pending[0].evidence.header().unwrap();
        let bad = wire::decode_frame(&valid, node.config.limits())
            .unwrap()
            .with_frame(|frame| {
                let mut votes: Vec<_> = frame.groups[0].votes.iter().copied().collect();
                let mut outer = None;
                let mut header = *frame.proposal.unwrap().header;
                match fault {
                    0 => votes[0].signature[0] ^= 1,
                    1 => votes[1] = votes[0],
                    2 => {
                        // Both signatures and the certificate are valid, but the
                        // decision certificate does not match this newer header.
                        header =
                            at_round(node.engine.c(), &payload, 1, false, node.config.limits()).0;
                    }
                    3 => {
                        let mut vote = votes[0];
                        vote.signature[0] ^= 1;
                        outer = Some(vote);
                    }
                    _ => unreachable!(),
                }
                let groups = [Group {
                    kind: frame.groups[0].kind,
                    votes: Votes::Dense(&votes),
                }];
                wire::encode_outlined(
                    Frame {
                        message: Message::Dec,
                        outer: outer.as_ref(),
                        proposal: Some(Proposal {
                            header: &header,
                            body: &payload,
                        }),
                        groups: &groups,
                    },
                    Some(71),
                    node.engine.c().hash(),
                    node.config.limits(),
                )
                .unwrap()
            });
        let writes = node.engine.storage().writes;
        assert!(
            !node.inject(1, bad, 1),
            "fault {fault} must be rejected before replacement"
        );
        assert_eq!(node.engine.storage().writes, writes);
        assert_eq!(node.network.payload.pending.len(), 1);
        assert_eq!(
            node.network.payload.pending[0].evidence.header(),
            Some(&original)
        );
        node.poll(2, true);
        let request = node.request(1);
        assert!(node.inject(
            1,
            wire::encode_body_response(request, &payload, node.config.limits()).unwrap(),
            3,
        ));
        assert!(node.engine.view().unwrap().pending.is_some());
        node.finish_validation();
    }
}

#[test]
fn four_nodes_route_outline_requests_bodies_and_acks_with_bounded_leases() {
    use crate::{PubKeyID, SortedRosterMember};
    use ed25519_zebra::VerificationKeyBytes;
    let mut nodes: Vec<_> = (0..4)
        .map(|local| {
            let (mut config, _) = crate::d28::runtime::tests::fixture();
            config.initial_roster = (1..=4)
                .map(|i| SortedRosterMember {
                    pub_key: PubKeyID(
                        VerificationKeyBytes::from(&SigningKey::from([i; 32])).into(),
                    ),
                    stake: 1,
                    cumulative_stake: u64::from(i),
                })
                .collect();
            config.active_roster_max_n = 4;
            config.max_members = 4;
            config.max_connections = 4;
            config.payload_mode = PayloadMode::Outlined;
            let c = config
                .chain_config(&config.initial_roster, config.vote_namespace)
                .unwrap()
                .unwrap();
            let mut engine =
                Engine::boot(MemoryStore::default(), SigningKey::from([local + 1; 32]), c).unwrap();
            engine.set_emission(Emission::Guided);
            let mut events = Events::default();
            let c = engine.c().clone();
            engine.init(&c, 1, 0, &mut events).unwrap();
            let mut network = Network::new(&config).unwrap();
            network.peers = (0..4)
                .filter(|&remote| remote != local)
                .map(|remote| Peer {
                    address: crate::d28::runtime::tests::address(40_001 + remote as u16),
                    handshake: [remote; 64],
                    generation: 1 + u64::from(local * 4 + remote),
                    member: Some(u32::from(remote)),
                    public: Some(config.initial_roster[remote as usize].pub_key),
                    last_hello: None,
                    last_sequence: None,
                    last_height: 1,
                    last_packet_utc: 0,
                    next_control: true,
                })
                .collect();
            config.metrics = Some(std::sync::Arc::new(RuntimeMetrics::default()));
            Node {
                config,
                engine,
                network,
                events,
            }
        })
        .collect();
    let (_, packet) = at_round(
        nodes[0].engine.c(),
        &body(1),
        0,
        true,
        nodes[0].config.limits(),
    );
    assert!(nodes[0].inject(0, packet.clone(), 0));
    let metadata = outline(&packet, nodes[0].engine.c(), nodes[0].config.limits());
    for node in nodes.iter_mut().skip(1) {
        let source = node
            .network
            .peers
            .iter()
            .position(|p| p.member == Some(0))
            .unwrap();
        assert!(node.inject(source, metadata.clone(), 0));
    }
    let mut bodies = 0;
    for tick in 1..=8 {
        let mut packets = Vec::new();
        for (sender, node) in nodes.iter_mut().enumerate() {
            let (push, counts) = node.poll(tick, true);
            bodies += counts.body_responses;
            assert!(
                node.network
                    .payload
                    .peers
                    .iter()
                    .filter(|p| p.lease.is_some())
                    .count()
                    <= node.config.max_outgoing
            );
            for (connection, bytes) in push.send_unreliable.into_iter().chain(push.send_bulk) {
                let dest = node
                    .network
                    .peers
                    .iter()
                    .find(|p| p.address.connection_key() == connection)
                    .unwrap()
                    .member
                    .unwrap() as usize;
                packets.push((sender, dest, bytes));
            }
        }
        for (sender, dest, bytes) in packets {
            let source = nodes[dest]
                .network
                .peers
                .iter()
                .position(|p| p.member == Some(sender as u32))
                .unwrap();
            assert!(nodes[dest].inject(source, bytes, tick));
        }
    }
    assert_eq!(bodies, 3, "one whole body per initially missing receiver");
    for node in &mut nodes {
        assert!(node.network.payload.pending.is_empty());
        assert!(node.network.payload.peers.iter().all(|p| p.lease.is_none()));
        node.finish_validation();
        assert!(node.inject(0, metadata.clone(), 9));
        let (_, counts) = node.poll(10, true);
        assert_eq!(
            counts.body_requests + counts.body_responses,
            0,
            "cached certificate replay is metadata-only"
        );
    }
}

#[test]
fn two_pending_bodies_can_exchange_sources_after_attempt_deadlines() {
    let mut node = Node::new();
    let a = body(1);
    let mut b = a.clone();
    b[0] ^= 1; // distinct ID; only source scheduling is under test
    assert!(node.inject(0, node.outline(&a), 0));
    assert!(node.inject(1, node.outline(&b), 0));
    node.poll(1, true);
    let a_request = node.request(0);
    let b_request = node.request(1);
    assert_ne!(a_request.key, b_request.key);
    let (_, counts) = node.poll(1 + node.config.request_timeout, true);
    assert_eq!(counts.body_requests, 2);
    assert_eq!(
        node.request(0).key,
        b_request.key,
        "late provenance must not lock active source choice"
    );
    assert_eq!(node.request(1).key, a_request.key);
    assert!(node.inject(
        0,
        wire::encode_body_response(a_request, &a, node.config.limits()).unwrap(),
        2 + node.config.request_timeout
    ));
    assert_eq!(
        node.network.payload.pending.len(),
        1,
        "old source/key response still useful after cross-key retry"
    );
}

#[test]
fn deferred_cached_body_completes_without_new_metadata_or_unnecessary_fetch() {
    let mut node = Node::new();
    node.config.max_receive_per_tick = 1;
    let payload = body(1);
    assert!(node.inject(1, node.outline(&payload), 0));
    let (_, proposal) = at_round(node.engine.c(), &payload, 0, false, node.config.limits());
    assert!(node.inject(1, proposal, 1));
    assert!(node.engine.view().unwrap().pending.is_none());
    let (push, counts) = node.poll(2, true);
    assert!(push.send_unreliable.is_empty());
    assert_eq!(
        counts.body_requests, 0,
        "fetch only if the body is still missing"
    );
    // A cached completion consumes the single verification turn, not an
    // unbounded side loop in addition to the normal receive budget.
    let discarded = node
        .config
        .metrics
        .as_ref()
        .unwrap()
        .snapshot()
        .scheduler_budget_discarded
        .messages;
    assert!(!node.inject(0, vec![0], 3));
    assert_eq!(
        node.config
            .metrics
            .as_ref()
            .unwrap()
            .snapshot()
            .scheduler_budget_discarded
            .messages,
        discarded + 1
    );
    assert_eq!(node.network.incoming.in_use(), 0);
    assert!(node.network.payload.pending.is_empty());
    assert!(node.engine.view().unwrap().pending.is_some());
    assert_eq!(
        node.config
            .metrics
            .as_ref()
            .unwrap()
            .snapshot()
            .payload
            .body_cache_hits,
        1
    );
    node.finish_validation();
}

#[test]
fn one_source_rotates_pending_interests_and_repairs_the_body_it_retains() {
    let mut receiver = Node::new();
    receiver.network.peers.truncate(1);
    let mut sender = Node::new();
    sender.network.peers.truncate(1);
    let a = body(1);
    let mut block = super::super::BftBlock::zcash_deserialize(&mut &a[..]).unwrap();
    block.version = 2;
    block.do_not_include_until_bc_height = 1;
    let b = block.zcash_serialize_to_vec().unwrap();
    assert!(canonical_body(&a, 1, receiver.config.max_body));
    assert!(canonical_body(&b, 1, receiver.config.max_body));
    assert_ne!(a, b);
    assert!(sender.inject(0, sender.inline(&b), 0));
    assert!(receiver.inject(0, receiver.outline(&a), 0));
    assert!(receiver.inject(0, receiver.outline(&b), 0));
    let (push, counts) = receiver.poll(1, true);
    assert_eq!(counts.body_requests, 1);
    let first = wire::decode_body_request(&push.send_unreliable[0].1).unwrap();
    assert_eq!(first.key.id, receiver.engine.c().body_id(&a));
    assert!(retained_body(&sender.engine, first.key).is_none());
    assert!(sender.inject(0, push.send_unreliable[0].1.clone(), 1));
    assert!(sender.poll(1, true).0.send_bulk.is_empty());

    let retry = 1 + receiver.config.request_timeout;
    let (push, counts) = receiver.poll(retry, true);
    assert_eq!(counts.body_requests, 1);
    let second = wire::decode_body_request(&push.send_unreliable[0].1).unwrap();
    assert_eq!(
        second.key.id,
        receiver.engine.c().body_id(&b),
        "a source retry must not let the first unresolved interest monopolize the peer"
    );
    assert!(sender.inject(0, push.send_unreliable[0].1.clone(), retry));
    let (push, counts) = sender.poll(retry, true);
    assert_eq!(counts.body_responses, 1);
    assert!(receiver.inject(0, push.send_bulk[0].1.clone(), retry + 1));
    assert_eq!(receiver.network.payload.pending.len(), 1);
    assert_eq!(receiver.network.payload.pending[0].key, first.key);
    let slot = receiver.engine.view().unwrap().pending.unwrap().proposal;
    assert_eq!(receiver.engine.storage().body(slot.index), b);
    receiver.finish_validation();
}

#[test]
fn retained_core_only_cache_hits_still_copy_and_fully_validate() {
    use crate::d28::checked_body::hash_work;
    let mut node = Node::new();
    let payload = body(1);
    let inline = node.inline(&payload);
    let outline = node.outline(&payload);
    assert!(node.inject(1, inline, 0));
    let hashes = hash_work();
    let canonical = CANONICAL_BYTES.with(std::cell::Cell::get);
    assert!(node.inject(1, outline, 1));
    assert_eq!(hash_work(), (hashes.0 + 1, hashes.1 + payload.len()));
    assert_eq!(CANONICAL_BYTES.with(std::cell::Cell::get), canonical + payload.len());
    assert_eq!(node.network.payload.pending.len(), 0);
}

#[test]
fn canonical_wrapper_does_not_confuse_a_matching_hash_with_chain_encoding() {
    let node = Node::new();
    let c = node.engine.c();
    for bytes in [body(2), vec![7; 100], {
        let mut bytes = body(1);
        bytes.push(0);
        bytes
    }] {
        let key = BodyKey { config: c.hash(), height: 1, id: c.body_id(&bytes) };
        assert!(VerifiedBody::new(c, key, bytes, node.config.max_body).is_none());
    }
    let bytes = body(1);
    let key = BodyKey { config: c.hash(), height: 1, id: c.body_id(&bytes) };
    assert!(VerifiedBody::new(c, key, bytes.clone(), bytes.len() - 1).is_none());
    assert!(VerifiedBody::new(c, BodyKey { config: [0; 32], ..key }, bytes.clone(),
        node.config.max_body).is_none());
    assert!(VerifiedBody::new(c, key, bytes, node.config.max_body).is_some());
}
