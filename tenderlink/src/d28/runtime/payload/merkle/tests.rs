use super::*;
use crate::d28::runtime::PayloadMode;
use crate::d28::wire;
use crate::d28::{
    BodyTag, CertKind, Emission, Frame, Group, MemoryStore, Message, Proposal, ProposalContent,
    Status, Step, Vote, VoteContent, VoteKind, Votes,
};

struct Node {
    config: RuntimeConfig,
    key: SigningKey,
    engine: Engine<MemoryStore>,
    events: Events,
    network: Network,
}
impl Node {
    fn new(leaf_bytes: usize) -> Self {
        let (mut config, key, _, _, mut network) = crate::d28::runtime::tests::metrics_fixture();
        config.max_body = 1 << 20;
        config.payload_mode = PayloadMode::Merkle { leaf_bytes };
        let c = config
            .chain_config(&config.initial_roster, config.vote_namespace)
            .unwrap()
            .unwrap();
        let mut engine = Engine::boot(MemoryStore::default(), key.clone(), c.clone()).unwrap();
        let mut events = Events::default();
        engine.init(&c, 1, 0, &mut events).unwrap();
        engine.set_emission(Emission::Guided);
        for (i, peer) in network.peers.iter_mut().enumerate() {
            peer.member = Some(i as u32);
        }
        network.payload.sync(&engine, &network.peers, 0);
        network.payload.sync_merkle(&engine, &network.peers, 0);
        Self {
            config,
            key,
            engine,
            events,
            network,
        }
    }
    fn outline(&self, body: &[u8], round: u64) -> (Header, Vec<u8>) {
        let c = self.engine.c();
        let signer = c.proposer(1, round) as u32;
        let id = c.body_id(body);
        let header = Header::sign(
            c,
            &SigningKey::from([signer as u8 + 1; 32]),
            ProposalContent {
                height: 1,
                round,
                valid_round: None,
                id,
                signer,
            },
        );
        let votes: Vec<_> = (0..2)
            .map(|signer| {
                Vote::sign(
                    c,
                    &SigningKey::from([signer as u8 + 1; 32]),
                    VoteContent {
                        height: 1,
                        round,
                        kind: VoteKind::Precommit,
                        id: Some(id),
                        signer,
                    },
                )
            })
            .collect();
        let groups = [Group {
            kind: CertKind::Decision { round, id },
            votes: Votes::Dense(&votes),
        }];
        let bytes = wire::encode_outlined(
            Frame {
                message: Message::Dec,
                outer: None,
                proposal: Some(Proposal {
                    header: &header,
                    body,
                }),
                groups: &groups,
            },
            Some(71),
            c.hash(),
            self.config.limits(),
        )
        .unwrap();
        (header, bytes)
    }
    fn inject(&mut self, source: usize, bytes: Vec<u8>, now: u64) -> bool {
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
            .push_back((self.network.peers[source].address.connection_key(), bytes));
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
            > before
    }
    fn poll(&mut self, now: u64, ready: bool) -> (NetworkThreadPush, PayloadMetricsSnapshot) {
        self.network
            .payload
            .sync(&self.engine, &self.network.peers, now);
        let mut push = NetworkThreadPush::default();
        let mut counts = PayloadMetricsSnapshot::default();
        let PayloadMode::Merkle { leaf_bytes } = self.config.payload_mode else {
            unreachable!()
        };
        self.network
            .payload
            .offer_merkle(
                &self.engine,
                &self.network.peers,
                &self.key,
                |_| ready,
                &self.config,
                now,
                &mut push,
                &mut counts,
                leaf_bytes,
            )
            .unwrap();
        (push, counts)
    }
    fn advertise(
        &self,
        body: &[u8],
        header: Header,
        leaves: Option<&[usize]>,
    ) -> (Tree, Reference, Vec<u8>) {
        let PayloadMode::Merkle { leaf_bytes } = self.config.payload_mode else {
            unreachable!()
        };
        let tree = Tree::new(body, leaf_bytes, self.config.max_body).unwrap();
        let reference = Reference {
            config: self.engine.c().hash(),
            height: 1,
            manifest: tree.manifest(header.content.id, header.identity(self.engine.c())),
        };
        let holder = HolderAttestation::sign(
            &context(&self.engine, 1),
            &reference.manifest,
            &SigningKey::from([2; 32]),
            self.config.max_body,
        )
        .unwrap();
        let mut bitmap = Bitmap::new(&reference.manifest, self.config.max_body).unwrap();
        for index in 0..tree.leaf_count() {
            if leaves.is_none_or(|l| l.contains(&index)) {
                bitmap.set(index).unwrap();
            }
        }
        let bytes = codec::encode_have(
            Have {
                reference,
                holder,
                bitmap: bitmap.as_bytes(),
            },
            self.config.max_body,
        )
        .unwrap();
        (tree, reference, bytes)
    }
    fn wants(&self, push: &NetworkThreadPush) -> Vec<(usize, Want)> {
        push.send_unreliable
            .iter()
            .filter_map(|(address, bytes)| {
                let want = codec::decode_want(bytes, self.config.max_body).ok()?;
                let peer = self
                    .network
                    .peers
                    .iter()
                    .position(|p| p.address.connection_key() == *address)?;
                Some((peer, want))
            })
            .collect()
    }
    fn finish(&mut self) {
        let d = self.engine.view().unwrap();
        assert_ne!(d.step, Step::Decided);
        let slot = d.pending.unwrap().proposal;
        assert_eq!(d.statuses[slot.index], Status::Indeterminate);
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
fn body(headers: usize) -> Vec<u8> {
    use zcash_primitives::{
        bft::{BftBlock, FatPointerToBftBlock},
        block::{BlockHash, BlockHeaderData},
    };
    let template = BlockHeaderData {
        version: 4,
        prev_block: BlockHash([9; 32]),
        merkle_root: [8; 32],
        final_sapling_root: [0; 32],
        time: 1,
        bits: 0,
        nonce: [7; 32],
        solution: vec![],
        fat_pointer_to_bft_block: FatPointerToBftBlock::null(),
    };
    BftBlock {
        version: 2,
        height: 1,
        previous_block_fat_ptr: FatPointerToBftBlock::null(),
        headers: vec![template; headers],
        slashforks: vec![],
        do_not_include_until_bc_height: 0,
    }
    .zcash_serialize_to_vec()
    .unwrap()
}
fn packet(tree: &Tree, body: &[u8], want: Want, leaf: u32) -> Vec<u8> {
    codec::encode_leaf(
        want.ticket,
        leaf,
        &body[tree.layout().leaf_range(leaf as usize).unwrap()],
        &tree.proof(leaf as usize).unwrap(),
        body.len(),
    )
    .unwrap()
}
#[test]
fn header_first_and_roster_authenticated_relayed_holder_before_allocation() {
    let mut node = Node::new(32768);
    let body = body(400);
    let (header, outline) = node.outline(&body, 0);
    let (_, reference, have) = node.advertise(&body, header, None);
    assert!(!node.inject(0, have.clone(), 0));
    let unknown = Want {
        ticket: Ticket {
            reference,
            generation: 9,
            token: 1,
        },
        indices: [0; WINDOW],
        count: 1,
    };
    assert!(!node.inject(
        0,
        codec::encode_want(unknown, node.config.max_body).unwrap(),
        0
    ));
    assert!(node.network.payload.merkle.peers[0].requested.is_none());
    node.poll(1, true);
    assert!(node.network.payload.merkle.assemblies.is_empty());
    assert!(node.inject(0, outline, 2));
    let mut wrong_holder = have.clone();
    wrong_holder[150] ^= 1;
    assert!(!node.inject(0, wrong_holder, 3));
    // Source member 0 relays member 1's attestation: verify the roster key of
    // the original holder, NOT the authenticated STP sender's different key.
    assert!(node.inject(0, have, 4));
    let (push, counts) = node.poll(5, true);
    assert!(!node.wants(&push).is_empty());
    assert!(counts.leaf_requests > 0);
    assert_eq!(node.network.payload.merkle.assemblies.len(), 1);
    assert!(node.engine.view().unwrap().pending.is_none());
}
#[test]
fn complete_all_leaf_geometries_preserves_normal_host_validation_gate() {
    for leaf_bytes in [32768, 102400, 262144, 1048576] {
        let mut node = Node::new(leaf_bytes);
        let body = body(500);
        let (header, outline) = node.outline(&body, 0);
        let (tree, _, have) = node.advertise(&body, header, None);
        assert!(node.inject(0, outline, 0));
        assert!(node.inject(0, have, 1));
        let (push, _) = node.poll(2, true);
        for (peer, want) in node.wants(&push) {
            for &index in want.indices() {
                assert!(node.inject(peer, packet(&tree, &body, want, index), 3));
            }
        }
        assert!(node.network.payload.pending.is_empty());
        assert!(node.network.payload.merkle.assemblies.is_empty());
        let snapshot = node.config.metrics.as_ref().unwrap().snapshot().payload;
        assert_eq!(snapshot.verified_leaf_bytes, body.len() as u64);
        assert_eq!(snapshot.assembly_completions, 1);
        node.finish();
    }
}
#[test]
fn disjoint_multi_source_windows_and_partial_onward_relay() {
    let mut node = Node::new(32768);
    let body = body(3500);
    let (header, outline) = node.outline(&body, 0);
    let (tree, reference, have) = node.advertise(&body, header, None);
    assert!(node.inject(0, outline, 0));
    assert!(node.inject(0, have.clone(), 1));
    assert!(node.inject(1, have, 1));
    let (push, counts) = node.poll(2, true);
    let wants = node.wants(&push);
    assert_eq!(wants.len(), 2);
    assert_eq!(counts.leaf_requests, 16);
    assert!(
        wants[0]
            .1
            .indices()
            .iter()
            .all(|i| !wants[1].1.indices().contains(i))
    );
    for &(peer, want) in &wants {
        assert!(node.inject(peer, packet(&tree, &body, want, want.indices[0]), 3));
    }
    let assembly = &node.network.payload.merkle.assemblies[0];
    assert_eq!(assembly.tree.bitmap().count(), 2);
    assert_eq!(assembly.body.len(), body.len());
    assert!(node.engine.view().unwrap().pending.is_none());
    let leaf = wants[0].1.indices[0];
    let onward = Want {
        ticket: Ticket {
            reference,
            generation: 888,
            token: 12,
        },
        indices: [leaf, 0, 0, 0, 0, 0, 0, 0],
        count: 1,
    };
    assert!(node.inject(
        1,
        codec::encode_want(onward, node.config.max_body).unwrap(),
        4
    ));
    let (push, counts) = node.poll(102, true);
    assert_eq!(counts.leaf_responses, 1);
    assert_eq!(counts.duplicate_payload_bytes, 0);
    assert!(counts.availability_bytes > 2); // full framing/signature, not bitmap bytes
    let forwarded = codec::decode_leaf(&push.send_bulk[0].1, node.config.max_body).unwrap();
    let (proof, count) = forwarded.proof_array();
    crate::d28::merkle::verify_leaf(
        &reference.manifest,
        leaf as usize,
        forwarded.data,
        &proof[..count],
        node.config.max_body,
    )
    .unwrap();
    assert_eq!(
        node.network.payload.merkle.assemblies[0]
            .tree
            .bitmap()
            .count(),
        2
    );
    let bitmap = node.network.payload.merkle.assemblies[0]
        .tree
        .bitmap()
        .as_bytes()
        .to_vec();
    let expires = node.network.payload.merkle.assemblies[0].expires;
    let duplicate = packet(&tree, &body, wants[0].1, leaf);
    assert!(node.inject(wants[0].0, duplicate, 103));
    assert_eq!(
        node.network.payload.merkle.assemblies[0]
            .tree
            .bitmap()
            .as_bytes(),
        bitmap
    );
    assert_eq!(node.network.payload.merkle.assemblies[0].expires, expires);
    let mut corrupt = packet(&tree, &body, wants[0].1, wants[0].1.indices[1]);
    *corrupt.last_mut().unwrap() ^= 1;
    assert!(!node.inject(wants[0].0, corrupt, 104));
    assert_eq!(
        node.network.payload.merkle.assemblies[0]
            .tree
            .bitmap()
            .as_bytes(),
        bitmap
    );
}
#[test]
fn missing_only_retry_rotates_source_and_accepts_late_existing_leaf() {
    let mut node = Node::new(32768);
    node.config.max_requests = 1;
    let body = body(1800);
    let (header, outline) = node.outline(&body, 0);
    let (tree, _, have) = node.advertise(&body, header, None);
    node.inject(0, outline, 0);
    node.inject(0, have.clone(), 1);
    node.inject(1, have, 1);
    let (push, _) = node.poll(2, true);
    let (first_peer, first) = node.wants(&push)[0];
    for &index in first.indices().iter().filter(|&&i| i != first.indices[3]) {
        assert!(node.inject(first_peer, packet(&tree, &body, first, index), 3));
    }
    let (waiting, _) = node.poll(4, true);
    assert!(node.wants(&waiting).is_empty());
    let (push, counts) = node.poll(node.config.request_timeout + 3, true);
    let (second_peer, second) = node.wants(&push)[0];
    assert_ne!(first_peer, second_peer);
    assert!(second.indices().contains(&first.indices[3]));
    assert!(
        second
            .indices()
            .iter()
            .all(|i| !first.indices().contains(i) || *i == first.indices[3])
    );
    assert_eq!(counts.leaf_requests as usize, tree.leaf_count() - 7);
    assert!(node.inject(
        first_peer,
        packet(&tree, &body, first, first.indices[3]),
        node.config.request_timeout + 4
    ));
    for &index in second.indices().iter().filter(|&&i| i != first.indices[3]) {
        assert!(node.inject(
            second_peer,
            packet(&tree, &body, second, index),
            node.config.request_timeout + 5
        ));
    }
    node.finish();
}
#[test]
fn invalid_attested_root_cannot_pass_final_keyed_hash_and_releases_attempt() {
    let mut node = Node::new(32768);
    let body = body(0);
    let (header, outline) = node.outline(&body, 0);
    let mut wrong = body.clone();
    wrong[0] ^= 1;
    let (bad_tree, bad_reference, bad_have) = node.advertise(&wrong, header, None);
    let (good_tree, _, good_have) = node.advertise(&body, header, None);
    node.inject(0, outline, 0);
    node.inject(0, bad_have.clone(), 1);
    node.inject(1, good_have, 1);
    let (push, _) = node.poll(2, true);
    let (peer, want) = node.wants(&push)[0];
    assert_eq!(want.ticket.reference, bad_reference);
    assert!(!node.inject(peer, packet(&bad_tree, &wrong, want, 0), 3));
    assert!(node.network.payload.merkle.assemblies.is_empty());
    assert!(node.engine.view().unwrap().pending.is_none());
    assert_eq!(
        node.config
            .metrics
            .as_ref()
            .unwrap()
            .snapshot()
            .payload
            .assembly_completions,
        0
    );
    assert!(!node.inject(0, bad_have, 4));
    let (push, _) = node.poll(5, true);
    let (peer, want) = node.wants(&push)[0];
    assert_eq!(peer, 1);
    assert!(node.inject(peer, packet(&good_tree, &body, want, 0), 6));
    node.finish();
}
#[test]
fn canonical_height_and_encoding_gate_rejects_even_matching_keyed_hash() {
    for malformed in [false, true] {
        let mut node = Node::new(32768);
        let mut bytes = if malformed {
            body(0)
        } else {
            crate::d28::runtime::tests::body(2)
        };
        if malformed {
            bytes.push(0);
        }
        let (header, outline) = node.outline(&bytes, 0);
        let (tree, _, have) = node.advertise(&bytes, header, None);
        node.inject(0, outline, 0);
        node.inject(0, have, 1);
        let (push, _) = node.poll(2, true);
        let (peer, want) = node.wants(&push)[0];
        assert!(!node.inject(peer, packet(&tree, &bytes, want, 0), 3));
        assert!(node.engine.view().unwrap().pending.is_none());
        assert!(node.network.payload.merkle.assemblies.is_empty());
    }
}
#[test]
fn new_signed_header_never_reuses_old_validity_during_assembly() {
    let mut node = Node::new(32768);
    let body = body(400);
    let (old, outline) = node.outline(&body, 0);
    let (tree, _, have) = node.advertise(&body, old, None);
    node.inject(0, outline, 0);
    node.inject(0, have, 1);
    let (push, _) = node.poll(2, true);
    let (new, outline) = node.outline(&body, 3);
    assert_ne!(old.identity(node.engine.c()), new.identity(node.engine.c()));
    node.inject(0, outline, 3);
    for (peer, want) in node.wants(&push) {
        for &index in want.indices() {
            assert!(node.inject(peer, packet(&tree, &body, want, index), 4));
        }
    }
    let d = node.engine.view().unwrap();
    let slot = d.pending.unwrap().proposal;
    assert_eq!(node.engine.storage().header(slot.index), Some(&new));
    node.finish();
}
#[test]
fn reconnect_and_scope_retirement_fence_old_control_and_leaves() {
    let mut node = Node::new(32768);
    let body = body(400);
    let (header, outline) = node.outline(&body, 0);
    let (tree, _, have) = node.advertise(&body, header, None);
    node.inject(0, outline.clone(), 0);
    node.inject(0, have.clone(), 1);
    let (push, _) = node.poll(2, true);
    let (peer, want) = node.wants(&push)[0];
    node.network.peers[peer].generation += 1;
    assert!(!node.inject(peer, packet(&tree, &body, want, want.indices[0]), 3));
    node.network.payload.clear();
    assert!(
        !node.inject(
            peer,
            codec::encode_ack(
                Ack::single(want.ticket, want.indices[0]),
                node.config.max_body
            )
            .unwrap(),
            4
        )
    );
    assert!(!node.inject(peer, have, 4));
    assert!(!node.inject(peer, packet(&tree, &body, want, want.indices[0]), 4));
    assert!(node.network.payload.merkle.assemblies.is_empty());
    assert!(node.inject(peer, outline, 5));
    assert!(node.network.payload.merkle.assemblies.is_empty());
}
#[test]
fn outgoing_window_credit_precedes_encoding_and_survives_new_tokens() {
    let mut node = Node::new(32768);
    node.config.max_outgoing = 1;
    let body = body(500);
    let (header, outline) = node.outline(&body, 0);
    let decoded = wire::decode_outlined(&outline, node.config.limits()).unwrap();
    decoded
        .with_body(&body, |frame| {
            complete(
                &mut node.engine,
                frame,
                &mut node.events,
                &mut node.network.conflicts,
                &node.config,
            )
        })
        .unwrap();
    let (_, reference, _) = node.advertise(&body, header, None);
    node.poll(0, true); // build/cache the retained tree once
    let want = Want {
        ticket: Ticket {
            reference,
            generation: 99,
            token: 1,
        },
        indices: [0, 1, 0, 0, 0, 0, 0, 0],
        count: 2,
    };
    node.inject(
        0,
        codec::encode_want(want, node.config.max_body).unwrap(),
        1,
    );
    let (blocked, counts) = node.poll(2, false);
    assert!(blocked.send_bulk.is_empty());
    assert_eq!(counts.payload_bytes, 0);
    assert!(node.network.payload.merkle.peers[0].lease.is_none());
    let (push, counts) = node.poll(3, true);
    assert_eq!(push.send_bulk.len(), 2);
    assert_eq!(counts.duplicate_payload_bytes, 0); // distinct leaves are not duplicates
    let next = Want {
        ticket: Ticket {
            token: 2,
            ..want.ticket
        },
        ..want
    };
    node.inject(
        0,
        codec::encode_want(next, node.config.max_body).unwrap(),
        4,
    );
    node.inject(
        0,
        codec::encode_ack(Ack::single(want.ticket, 0), node.config.max_body).unwrap(),
        4,
    );
    assert!(node.poll(5, true).0.send_bulk.is_empty());
    assert!(node.inject(
        0,
        codec::encode_ack(Ack::single(want.ticket, 0), node.config.max_body).unwrap(),
        5
    ));
    assert_eq!(node.network.payload.merkle.peers[0].lease.unwrap().mask, 2);
    let mut cumulative = Ack::single(want.ticket, 1); // reordered cumulative receipt
    assert!(cumulative.add(0));
    assert!(node.inject(
        0,
        codec::encode_ack(cumulative, node.config.max_body).unwrap(),
        6,
    ));
    assert!(node.network.payload.merkle.peers[0].requested.is_some());
    assert!(node.poll(7, false).0.send_bulk.is_empty());
    let (push, counts) = node.poll(8, true);
    assert_eq!(push.send_bulk.len(), 2);
    assert_eq!(counts.duplicate_payload_bytes, counts.payload_bytes);
    assert!(!node.inject(
        0,
        codec::encode_ack(Ack::single(want.ticket, 0), node.config.max_body).unwrap(),
        9
    ));
    assert_eq!(
        node.network.payload.merkle.peers[0]
            .lease
            .unwrap()
            .want
            .ticket,
        next.ticket
    );
    assert_eq!(node.network.payload.merkle.cached.len(), 1);
}

#[test]
fn unrelated_cached_haves_cannot_overwrite_missing_body_discovery() {
    let mut node = Node::new(32768);
    let missing = body(400);
    let held = body(0);
    let (held_header, held_outline) = node.outline(&held, 0);
    wire::decode_outlined(&held_outline, node.config.limits())
        .unwrap()
        .with_body(&held, |f| {
            complete(
                &mut node.engine,
                f,
                &mut node.events,
                &mut node.network.conflicts,
                &node.config,
            )
        })
        .unwrap();
    let (header, outline) = node.outline(&missing, 3);
    let (_, reference, have_missing) = node.advertise(&missing, header, None);
    let (_, _, have_held) = node.advertise(&held, held_header, None);
    node.inject(0, outline, 0);
    // Model a single receive batch: matching HAVE then unrelated held HAVE.
    let address = node.network.peers[0].address.connection_key();
    node.network
        .pull
        .received_unreliable_messages
        .push_back((address, have_missing));
    assert!(node.inject(0, have_held, 1));
    let (push, _) = node.poll(2, true);
    assert_eq!(node.wants(&push)[0].1.ticket.reference, reference);
    assert_eq!(node.network.payload.merkle.assemblies.len(), 1);
}

#[test]
fn partial_header_rebinding_preserves_verified_leaves_and_keeps_requesting() {
    let mut node = Node::new(32768);
    node.config.max_requests = 1;
    let body = body(1800);
    let (old, outline) = node.outline(&body, 0);
    let (tree, _, have) = node.advertise(&body, old, None);
    node.inject(0, outline, 0);
    node.inject(0, have, 1);
    let (push, _) = node.poll(2, true);
    let (peer, first) = node.wants(&push)[0];
    for &index in first.indices() {
        node.inject(peer, packet(&tree, &body, first, index), 3);
    }
    assert_eq!(
        node.network.payload.merkle.assemblies[0]
            .tree
            .bitmap()
            .count(),
        8
    );
    let body_address = node.network.payload.merkle.assemblies[0].body.as_ptr();
    let (new, outline) = node.outline(&body, 3);
    let (_, reference, new_have) = node.advertise(&body, new, None);
    node.inject(peer, outline, 4);
    node.inject(peer, new_have, 5);
    assert_eq!(
        node.network.payload.merkle.assemblies[0].body.as_ptr(),
        body_address
    );
    assert_eq!(
        node.network.payload.merkle.assemblies[0]
            .tree
            .bitmap()
            .count(),
        8
    );
    let (push, _) = node.poll(6, true);
    let (peer, second) = node.wants(&push)[0];
    assert_eq!(second.ticket.reference, reference);
    assert!(
        second
            .indices()
            .iter()
            .all(|i| !first.indices().contains(i))
    );
    for &index in second.indices() {
        assert!(node.inject(peer, packet(&tree, &body, second, index), 7));
    }
    let slot = node.engine.view().unwrap().pending.unwrap().proposal;
    assert_eq!(node.engine.storage().header(slot.index), Some(&new));
    node.finish();
}

#[test]
fn orphan_wrong_root_yields_to_another_holder_before_pending_expiry() {
    for corrupt in [false, true] {
        let mut node = Node::new(32768);
        let body = body(400);
        let (header, outline) = node.outline(&body, 0);
        let mut wrong = body.clone();
        wrong[0] ^= 1;
        let (bad_tree, _, bad_have) = node.advertise(&wrong, header, None);
        let (_, good_reference, good_have) = node.advertise(&body, header, None);
        node.inject(0, outline, 0);
        node.inject(0, bad_have, 1);
        let (push, _) = node.poll(2, true);
        let (peer, want) = node.wants(&push)[0];
        let expiry = node.network.payload.pending[0].expires;
        node.inject(1, good_have, 3);
        if corrupt {
            let mut bytes = packet(&bad_tree, &wrong, want, 0);
            *bytes.last_mut().unwrap() ^= 1;
            assert!(!node.inject(peer, bytes, 4));
        }
        let at = if corrupt {
            5
        } else {
            node.config.request_timeout + 3
        };
        let (push, _) = node.poll(at, true);
        assert!(at < expiry);
        assert_eq!(node.wants(&push)[0].1.ticket.reference, good_reference);
        assert_eq!(node.network.payload.merkle.assemblies.len(), 1);
        assert_eq!(node.network.payload.pending[0].expires, expiry);
    }
}

#[test]
fn one_source_rotates_pending_interests_without_periodic_cursor_aliasing() {
    let mut node = Node::new(32768);
    node.network.peers.truncate(1);
    let a = body(400);
    let b = body(500);
    let (ah, outline_a) = node.outline(&a, 0);
    let (bh, outline_b) = node.outline(&b, 1);
    let (_, ar, have_a) = node.advertise(&a, ah, None);
    let (_, br, have_b) = node.advertise(&b, bh, None);
    node.inject(0, outline_a, 0);
    node.inject(0, outline_b, 0);
    node.inject(0, have_a.clone(), 1);
    node.inject(0, have_b.clone(), 1);
    let (push, _) = node.poll(2, true);
    assert_eq!(node.wants(&push)[0].1.ticket.reference, ar);
    // Four offer ticks before the next periodic availability batch used to
    // return a modulo-two unconditional cursor to the same starving priority.
    for time in [25, 50, 75, 100] {
        node.poll(time, true);
    }
    node.inject(0, have_a, 101);
    node.inject(0, have_b, 101);
    node.poll(102, true);
    assert_eq!(node.network.payload.merkle.assemblies.len(), 2);
    let (push, _) = node.poll(node.config.request_timeout + 3, true);
    assert_eq!(node.wants(&push)[0].1.ticket.reference, br);
    assert_eq!(
        node.network
            .payload
            .merkle
            .peers
            .iter()
            .filter(|p| p.receive.is_some())
            .count(),
        1
    );
    assert_eq!(
        node.network.payload.merkle.assemblies.len(),
        node.config.max_requests
    );
    let (_, extra) = node.outline(&body(600), 2);
    node.inject(0, extra, node.config.request_timeout + 4);
    assert_eq!(node.network.payload.pending.len(), node.config.max_requests);
}

#[test]
fn expiry_is_not_renewed_by_reordered_duplicates_or_have_refresh() {
    let mut node = Node::new(32768);
    let body = body(500);
    let (header, outline) = node.outline(&body, 0);
    let (tree, _, have) = node.advertise(&body, header, None);
    node.inject(0, outline.clone(), 0);
    node.inject(0, have.clone(), 1);
    let (push, _) = node.poll(2, true);
    let (peer, want) = node.wants(&push)[0];
    let leaf = *want.indices().last().unwrap();
    node.inject(peer, packet(&tree, &body, want, leaf), 3);
    let expiry = node.network.payload.merkle.assemblies[0].expires;
    let retry = node.network.payload.merkle.assemblies[0].retry_at;
    node.inject(peer, packet(&tree, &body, want, leaf), 4);
    node.inject(peer, have, 5);
    node.inject(peer, outline, 6);
    assert_eq!(node.network.payload.merkle.assemblies[0].expires, expiry);
    assert_eq!(node.network.payload.merkle.assemblies[0].retry_at, retry);
    node.poll(expiry, true);
    assert!(node.network.payload.pending.is_empty());
    assert!(node.network.payload.merkle.assemblies.is_empty());
    assert!(!node.inject(peer, packet(&tree, &body, want, leaf), expiry + 1));
    assert!(node.network.payload.merkle.assemblies.is_empty());
}

#[test]
fn global_send_window_limit_survives_timeout_and_independent_stp_backlog() {
    let mut node = Node::new(32768);
    node.config.max_outgoing = 1;
    let body = body(500);
    let (header, outline) = node.outline(&body, 0);
    wire::decode_outlined(&outline, node.config.limits())
        .unwrap()
        .with_body(&body, |f| {
            complete(
                &mut node.engine,
                f,
                &mut node.events,
                &mut node.network.conflicts,
                &node.config,
            )
        })
        .unwrap();
    let (_, reference, _) = node.advertise(&body, header, None);
    node.poll(0, true);
    for peer in 0..2 {
        let want = Want {
            ticket: Ticket {
                reference,
                generation: 17 + peer as u64,
                token: 1,
            },
            indices: [0, 1, 0, 0, 0, 0, 0, 0],
            count: 2,
        };
        assert!(node.inject(
            peer,
            codec::encode_want(want, node.config.max_body).unwrap(),
            1
        ));
    }
    let (push, _) = node.poll(2, true);
    assert_eq!(push.send_bulk.len(), 2);
    assert!(
        push.send_bulk
            .iter()
            .all(|(address, _)| *address == node.network.peers[0].address.connection_key())
    );
    assert_eq!(
        node.network
            .payload
            .merkle
            .peers
            .iter()
            .filter(|p| p.lease.is_some())
            .count(),
        1
    );
    assert!(node.poll(3, true).0.send_bulk.is_empty());
    let at = node.config.request_timeout + 3;
    assert!(node.poll(at, false).0.send_bulk.is_empty());
    assert!(
        node.network
            .payload
            .merkle
            .peers
            .iter()
            .all(|p| p.lease.is_none())
    );
    let (push, _) = node.poll(at + 1, true);
    assert_eq!(push.send_bulk.len(), 2);
    assert!(
        push.send_bulk
            .iter()
            .all(|(address, _)| *address == node.network.peers[1].address.connection_key())
    );
    assert_eq!(
        node.network
            .payload
            .merkle
            .peers
            .iter()
            .filter(|p| p.lease.is_some())
            .count(),
        1
    );
}

#[test]
fn receive_budget_alternates_control_and_leaf_classes_behind_bulk() {
    let mut node = Node::new(32768);
    let body = body(500);
    let (header, outline) = node.outline(&body, 0);
    let (tree, reference, have) = node.advertise(&body, header, None);
    node.inject(0, outline, 0);
    node.inject(0, have, 1);
    let (push, _) = node.poll(2, true);
    let (peer, want) = node.wants(&push)[0];
    node.config.max_receive_per_tick = 1;
    node.network.peers[peer].next_control = true;
    let connection = node.network.peers[peer].address.connection_key();
    node.network
        .pull
        .received_unreliable_messages
        .push_back((connection, packet(&tree, &body, want, 0)));
    let control = Want {
        ticket: Ticket {
            reference,
            generation: 88,
            token: 123,
        },
        indices: [0; WINDOW],
        count: 1,
    };
    assert!(node.inject(
        peer,
        codec::encode_want(control, node.config.max_body).unwrap(),
        3
    ));
    assert_eq!(node.network.payload.merkle.peers[peer].remote_token, 123);
    assert_eq!(
        node.network.payload.merkle.assemblies[0]
            .tree
            .bitmap()
            .count(),
        0
    );
    let newer = Want {
        ticket: Ticket {
            token: 124,
            ..control.ticket
        },
        ..control
    };
    node.network.pull.received_unreliable_messages.push_back((
        connection,
        codec::encode_want(newer, node.config.max_body).unwrap(),
    ));
    assert!(node.inject(peer, packet(&tree, &body, want, 0), 4));
    assert_eq!(node.network.payload.merkle.peers[peer].remote_token, 123);
    assert_eq!(
        node.network.payload.merkle.assemblies[0]
            .tree
            .bitmap()
            .count(),
        1
    );
    assert_eq!(node.network.incoming.in_use(), 0);
}

#[test]
fn dropped_window_receipt_repairs_credit_before_request_timeout() {
    let mut sender = Node::new(32768);
    let mut receiver = Node::new(32768);
    sender.config.max_outgoing = 1;
    receiver.config.max_requests = 1;
    let body = body(1800); // more than one eight-leaf window
    let (_, outline) = sender.outline(&body, 0);
    wire::decode_outlined(&outline, sender.config.limits())
        .unwrap()
        .with_body(&body, |f| {
            complete(
                &mut sender.engine,
                f,
                &mut sender.events,
                &mut sender.network.conflicts,
                &sender.config,
            )
        })
        .unwrap();
    assert!(receiver.inject(0, outline, 0));
    let (advertised, _) = sender.poll(0, true);
    let have = advertised
        .send_unreliable
        .iter()
        .find(|(_, b)| b.first() == Some(&codec::HAVE))
        .unwrap()
        .1
        .clone();
    assert!(receiver.inject(0, have, 1));
    let (request, _) = receiver.poll(2, true);
    let want = request
        .send_unreliable
        .iter()
        .find(|(_, b)| b.first() == Some(&codec::WANT))
        .unwrap()
        .1
        .clone();
    assert!(sender.inject(0, want, 3));
    let (sent, _) = sender.poll(4, true);
    assert_eq!(sent.send_bulk.len(), WINDOW);
    let old_leaf = sent.send_bulk[0].1.clone();
    for (_, leaf) in sent.send_bulk {
        assert!(receiver.inject(0, leaf, 5));
    }
    let (reply, _) = receiver.poll(6, true);
    let ack_frames: Vec<_> = reply
        .send_unreliable
        .iter()
        .filter(|(_, b)| b.first() == Some(&codec::ACK))
        .collect();
    assert_eq!(
        ack_frames.len(),
        1,
        "one cumulative frame, not eight control messages"
    );
    assert_eq!(
        codec::decode_ack(&ack_frames[0].1, receiver.config.max_body)
            .unwrap()
            .count as usize,
        WINDOW
    );
    let mut lost_receipt = false;
    for (_, bytes) in reply.send_unreliable {
        if bytes.first() == Some(&codec::ACK) && !lost_receipt {
            lost_receipt = true;
            continue; // a single ordinary best-effort control loss
        }
        if matches!(bytes.first(), Some(&codec::ACK | &codec::WANT)) {
            sender.inject(0, bytes, 7);
        }
    }
    assert!(lost_receipt);
    assert!(sender.poll(8, true).0.send_bulk.is_empty());
    // Repeated HAVE or a newer WANT is NOT an ACK. The exact old receipt must
    // repair its credit on the existing status cadence, not after a 2/10s lease.
    let (refresh, _) = receiver.poll(106, true);
    let receipts: Vec<_> = refresh
        .send_unreliable
        .into_iter()
        .filter(|(_, b)| b.first() == Some(&codec::ACK))
        .collect();
    assert!(
        !receipts.is_empty(),
        "lost transfer receipt has no bounded refresh"
    );
    for (_, ack) in receipts {
        sender.inject(0, ack, 107);
    }
    let (next, _) = sender.poll(108, true);
    assert!(
        !next.send_bulk.is_empty(),
        "old unacknowledged window still blocks the newer request"
    );
    assert!(108 < sender.config.request_timeout);
    let next_leaf = next.send_bulk[0].1.clone();
    let newer_token = codec::decode_leaf(&next_leaf, receiver.config.max_body)
        .unwrap()
        .ticket
        .token;
    assert!(receiver.inject(0, next_leaf, 109));
    let current = receiver.network.payload.merkle.peers[0].receipt.unwrap();
    assert_eq!(current.ack.ticket.token, newer_token);
    // The previous token still names useful verified data, but cannot evict the
    // newer cumulative receipt (nor renew its bounded retry lifetime).
    receiver.inject(0, old_leaf.clone(), 110);
    let after = receiver.network.payload.merkle.peers[0].receipt.unwrap();
    assert_eq!(after.ack, current.ack);
    assert_eq!(after.until, current.until);
    let (expired, _) = receiver.poll(after.until, true);
    assert!(
        expired
            .send_unreliable
            .iter()
            .all(|(_, b)| b.first() != Some(&codec::ACK))
    );
    receiver.inject(0, old_leaf, after.until + 1);
    let watermark = receiver.network.payload.merkle.peers[0].receipt.unwrap();
    assert_eq!(watermark.ack, current.ack);
    assert_eq!(watermark.until, current.until);
    assert!(
        receiver
            .poll(after.until + 2, true)
            .0
            .send_unreliable
            .iter()
            .all(|(_, b)| b.first() != Some(&codec::ACK))
    );
    receiver.network.payload.clear();
    assert!(receiver.network.payload.merkle.peers.is_empty());
    assert!(
        receiver
            .poll(after.until + 3, true)
            .0
            .send_unreliable
            .iter()
            .all(|(_, b)| b.first() != Some(&codec::ACK))
    );
}

#[test]
fn receipt_merges_reordered_leaves_without_duplicate_refresh_or_expiry_extension() {
    let mut node = Node::new(32768);
    node.config.max_requests = 1;
    let body = body(1800);
    let (header, outline) = node.outline(&body, 0);
    let (tree, _, have) = node.advertise(&body, header, None);
    node.inject(0, outline, 0);
    node.inject(0, have, 1);
    let (request, _) = node.poll(2, true);
    let (peer, want) = node.wants(&request)[0];
    assert_eq!(want.count as usize, WINDOW);
    let mut until = None;
    for (offset, index) in [7, 0, 7, 3].into_iter().enumerate() {
        let now = 3 + offset as u64 * 2;
        assert!(node.inject(peer, packet(&tree, &body, want, index), now));
        let receipt = node.network.payload.merkle.peers[peer].receipt.unwrap();
        assert_eq!(receipt.until, *until.get_or_insert(receipt.until));
        let (reply, _) = node.poll(now + 1, true);
        let acks: Vec<_> = reply
            .send_unreliable
            .iter()
            .filter(|(_, b)| b.first() == Some(&codec::ACK))
            .map(|(_, b)| codec::decode_ack(b, node.config.max_body).unwrap())
            .collect();
        if offset == 2 {
            assert!(
                acks.is_empty(),
                "duplicate leaf must not advance periodic receipt refresh"
            );
        } else {
            assert_eq!(acks.len(), 1);
            let expected: &[u32] = match offset {
                0 => &[7],
                1 => &[7, 0],
                _ => &[7, 0, 3],
            };
            assert_eq!(acks[0].indices(), expected);
        }
    }
}
