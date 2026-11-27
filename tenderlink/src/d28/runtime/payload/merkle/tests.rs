use super::*;
use crate::d28::runtime::PayloadMode;
use crate::d28::wire;
use crate::d28::{
    BodyTag, CertKind, Emission, Group, MemoryStore, ProposalContent, Status, Step, Vote,
    VoteContent, VoteKind, Votes,
};
use std::collections::{HashMap, HashSet};

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
        for (i, p) in network.peers.iter_mut().enumerate() {
            p.member = Some(i as u32);
        }
        network.payload.sync(&engine, &network.peers, 0);
        network.payload.sync_merkle(&engine, &network.peers, 0);
        network.payload.merkle.rng = ChaCha20Rng::from_seed([37; 32]);
        Self {
            config,
            key,
            engine,
            events,
            network,
        }
    }
    fn proposer(leaf_bytes: usize) -> Self {
        let mut n = Self::new(leaf_bytes);
        n.key = SigningKey::from([2; 32]); // height 1, round 0 proposer in this fixture
        let c = n.engine.c().clone();
        n.engine = Engine::boot(MemoryStore::default(), n.key.clone(), c.clone()).unwrap();
        n.engine.init(&c, 1, 0, &mut n.events).unwrap();
        n.engine.set_emission(Emission::Guided);
        n
    }
    fn header(&self, body: &[u8], round: u64) -> Header {
        let c = self.engine.c();
        let signer = c.proposer(1, round) as u32;
        Header::sign(
            c,
            &SigningKey::from([signer as u8 + 1; 32]),
            ProposalContent {
                height: 1,
                round,
                valid_round: None,
                id: c.body_id(body),
                signer,
            },
        )
    }
    fn outline(&self, body: &[u8], round: u64) -> (Header, Vec<u8>) {
        let h = self.header(body, round);
        let votes: Vec<_> = (0..2)
            .map(|signer| {
                Vote::sign(
                    self.engine.c(),
                    &SigningKey::from([signer as u8 + 1; 32]),
                    VoteContent {
                        height: 1,
                        round,
                        kind: VoteKind::Precommit,
                        id: Some(h.content.id),
                        signer,
                    },
                )
            })
            .collect();
        let groups = [Group {
            kind: CertKind::Decision {
                round,
                id: h.content.id,
            },
            votes: Votes::Dense(&votes),
        }];
        let bytes = wire::encode_outlined(
            Frame {
                message: Message::Dec,
                outer: None,
                proposal: Some(Proposal { header: &h, body }),
                groups: &groups,
            },
            None,
            self.engine.c().hash(),
            self.config.limits(),
        )
        .unwrap();
        (h, bytes)
    }
    fn source(
        &self,
        body: &[u8],
        header: Header,
        leaves: Option<&[usize]>,
    ) -> (Tree, Authentication, Vec<u8>) {
        let PayloadMode::Merkle { leaf_bytes } = self.config.payload_mode else {
            unreachable!()
        };
        let tree = Tree::new(body, leaf_bytes, self.config.max_body).unwrap();
        let reference = Reference {
            config: self.engine.c().hash(),
            height: 1,
            manifest: tree.manifest(header.content.id, header.identity(self.engine.c())),
        };
        let proposer = ProposerAttestation::sign(
            &context(&self.engine, 1),
            &reference.manifest,
            &SigningKey::from([header.content.signer as u8 + 1; 32]),
            self.config.max_body,
        )
        .unwrap();
        let auth = Authentication {
            header,
            reference,
            proposer,
        };
        let mut bitmap = Bitmap::new(&reference.manifest, self.config.max_body).unwrap();
        for i in 0..tree.leaf_count() {
            if leaves.is_none_or(|l| l.contains(&i)) {
                bitmap.set(i).unwrap();
            }
        }
        let have = codec::encode_have(
            Have {
                auth,
                bitmap: bitmap.as_bytes(),
            },
            self.config.max_body,
        )
        .unwrap();
        (tree, auth, have)
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
    fn capacity(&self, bytes: u64, slots: usize, backlog: u64) -> [SendCapacitySnapshot; 2] {
        std::array::from_fn(|lane| SendCapacitySnapshot {
            backlog_bytes: if lane == 1 { backlog } else { 0 },
            peers: self
                .network
                .peers
                .iter()
                .map(|p| {
                    (
                        p.address.connection_key(),
                        SendCapacity {
                            backlog_bytes: 0,
                            available_bytes: bytes,
                            outbox_slots: slots,
                        },
                    )
                })
                .collect(),
        })
    }
    fn poll(&mut self, now: u64) -> (NetworkThreadPush, PayloadMetricsSnapshot) {
        let capacity = self.capacity(u64::MAX, usize::MAX, 0);
        self.poll_with(now, &capacity, NetworkThreadPush::default())
    }
    fn poll_with(
        &mut self,
        now: u64,
        capacity: &[SendCapacitySnapshot; 2],
        mut push: NetworkThreadPush,
    ) -> (NetworkThreadPush, PayloadMetricsSnapshot) {
        self.network
            .payload
            .sync(&self.engine, &self.network.peers, now);
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
                capacity,
                &self.config,
                now,
                &mut push,
                &mut counts,
                leaf_bytes,
            )
            .unwrap();
        (push, counts)
    }
    fn wants(&self, push: &NetworkThreadPush) -> Vec<(usize, Want)> {
        push.send_unreliable
            .iter()
            .filter_map(|(address, bytes)| {
                let w = codec::decode_want(bytes, self.config.max_body).ok()?;
                let pi = self
                    .network
                    .peers
                    .iter()
                    .position(|p| p.address.connection_key() == *address)?;
                Some((pi, w))
            })
            .collect()
    }
    fn retain_proposal(&mut self, body: &[u8], header: Header) {
        self.engine
            .receive(
                Frame {
                    message: Message::Ordinary,
                    outer: None,
                    proposal: Some(Proposal {
                        header: &header,
                        body,
                    }),
                    groups: &[],
                },
                Status::Indeterminate,
                &mut self.events,
            )
            .unwrap();
        assert!(
            retained_body(
                &self.engine,
                BodyKey {
                    config: self.engine.c().hash(),
                    height: 1,
                    id: header.content.id
                }
            )
            .is_some()
        );
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
    let h = BlockHeaderData {
        version: 4,
        prev_block: BlockHash([9; 32]),
        merkle_root: [8; 32],
        final_sapling_root: [0; 32],
        time: 1,
        bits: 0,
        nonce: [7; 32],
        solution: vec![0; (headers / 1500).saturating_sub(1) * 144],
        fat_pointer_to_bft_block: FatPointerToBftBlock::null(),
    };
    BftBlock {
        version: 2,
        height: 1,
        previous_block_fat_ptr: FatPointerToBftBlock::null(),
        headers: vec![h; headers.min(1500)],
        slashforks: vec![],
        do_not_include_until_bc_height: 0,
    }
    .zcash_serialize_to_vec()
    .unwrap()
}
fn packet(tree: &Tree, body: &[u8], auth: Authentication, index: u32) -> Vec<u8> {
    codec::encode_leaf(
        auth,
        index,
        &body[tree.layout().leaf_range(index as usize).unwrap()],
        &tree.proof(index as usize).unwrap(),
        body.len(),
    )
    .unwrap()
}
fn total(push: &NetworkThreadPush) -> usize {
    [&push.send_bulk, &push.send_unreliable, &push.send_bft_live]
        .into_iter()
        .flatten()
        .map(|(_, b)| b.len())
        .sum()
}
#[test]
fn independent_shuffled_push_discovers_without_outline_and_keeps_normal_host_gate() {
    let mut n = Node::new(102400);
    let b = body(3000);
    let h = n.header(&b, 0);
    let (tree, auth, _) = n.source(&b, h, None);
    let mut order: Vec<_> = (0..tree.leaf_count() as u32).collect();
    order.shuffle(&mut ChaCha20Rng::from_seed([15; 32]));
    assert!(order.len() > 1);
    for (i, leaf) in order.iter().enumerate() {
        assert!(n.inject(0, packet(&tree, &b, auth, *leaf), i as u64));
    }
    assert!(n.network.payload.pending.is_empty());
    assert_eq!(
        n.network.payload.merkle.body(auth.reference.key()),
        Some(b.as_slice())
    );
    let d = n.engine.view().unwrap();
    let slot = d.slot.unwrap();
    assert_eq!(d.statuses[slot.index], Status::Indeterminate);
    let old_identity = slot.header;
    let (fresh, outline) = n.outline(&b, 1);
    assert!(n.inject(1, outline, 100));
    let d = n.engine.view().unwrap();
    let fresh_slot = d.pending.unwrap().proposal;
    assert_eq!(n.engine.storage().header(fresh_slot.index), Some(&fresh));
    assert_ne!(fresh_slot.header, old_identity);
    assert_eq!(d.statuses[fresh_slot.index], Status::Indeterminate);
    n.finish();
}
#[test]
fn authentication_and_index_tampering_rejected_before_ownership_allocation() {
    let mut n = Node::new(32768);
    let b = body(700);
    let (tree, auth, _) = n.source(&b, n.header(&b, 0), None);
    let mut wrong = auth;
    wrong.proposer = ProposerAttestation::sign(
        &context(&n.engine, 1),
        &auth.reference.manifest,
        &SigningKey::from([1; 32]),
        n.config.max_body,
    )
    .unwrap();
    assert!(!n.inject(0, packet(&tree, &b, wrong, 0), 0));
    let mut bad = packet(&tree, &b, auth, 0);
    bad[2..6].copy_from_slice(&1u32.to_le_bytes());
    assert!(!n.inject(0, bad, 1));
    let mut bad = packet(&tree, &b, auth, 0);
    *bad.last_mut().unwrap() ^= 1;
    assert!(!n.inject(0, bad, 2));
    let mut wrong = auth;
    wrong.reference.config[0] ^= 1;
    assert!(!n.inject(0, packet(&tree, &b, wrong, 0), 3));
    assert!(n.network.payload.merkle.owned.is_empty());
    assert!(n.inject(0, packet(&tree, &b, auth, 1), 4)); // sender need not be proposer
    assert_eq!(n.network.payload.merkle.owned[0].bitmap().count(), 1);
}
#[test]
fn every_geometry_duplicate_drop_reorder_and_no_ack_needed() {
    for size in [32768, 102400, 262144, 1048576] {
        let mut n = Node::new(size);
        let b = body(3000);
        let (h, outline) = n.outline(&b, 0);
        let (tree, auth, have) = n.source(&b, h, None);
        assert!(n.inject(0, outline, 0));
        assert!(n.inject(0, have.clone(), 1));
        assert!(n.inject(1, have, 1));
        for leaf in (1..tree.leaf_count()).rev() {
            assert!(n.inject(0, packet(&tree, &b, auth, leaf as u32), 2));
            assert!(n.inject(1, packet(&tree, &b, auth, leaf as u32), 3));
        }
        let (early, _) = n.poll(100);
        assert!(n.wants(&early).is_empty()); // initial progress grace
        let (repair, counts) = n.poll(300);
        assert_eq!(counts.leaf_requests, 1);
        let wants = n.wants(&repair);
        assert_eq!(wants.len(), 1);
        assert_eq!(wants[0].1.indices(), &[0]);
        assert!(n.inject(wants[0].0, packet(&tree, &b, auth, 0), 301));
        let (push, _) = n.poll(302);
        assert!(push.send_bulk.is_empty()); // non-proposer never broadcasts
        assert!(
            push.send_unreliable
                .iter()
                .all(|(_, b)| matches!(b[0], codec::HAVE | codec::WANT))
        );
        assert!(!n.inject(0, vec![0xBA, 2], 303));
        let m = n.config.metrics.as_ref().unwrap().snapshot().payload;
        assert_eq!(m.verified_leaf_bytes, b.len() as u64);
        assert_eq!(m.assembly_completions, 1);
        n.finish();
    }
}
#[test]
fn nonresponsive_advertiser_does_not_pin_body_and_late_shards_remain_useful() {
    let mut n = Node::new(102400);
    let b = body(3000);
    let (h, outline) = n.outline(&b, 0);
    let (tree, auth, have) = n.source(&b, h, None);
    assert!(n.inject(0, outline, 0));
    assert!(n.inject(0, have.clone(), 1));
    assert!(n.inject(1, have, 1));
    let (first, _) = n.poll(201);
    let mut missing = HashMap::new();
    for (peer, w) in n.wants(&first) {
        for &i in w.indices() {
            missing.insert(i, peer);
            if peer == 1 {
                assert!(n.inject(1, packet(&tree, &b, auth, i), 202));
            }
        }
    }
    assert!(missing.values().any(|&p| p == 0));
    let (early, _) = n.poll(220);
    assert!(n.wants(&early).is_empty());
    let (retry, _) = n.poll(702);
    for (peer, w) in n.wants(&retry) {
        for &i in w.indices() {
            assert_eq!(missing[&i], 0);
            assert_eq!(peer, 1);
            assert!(n.inject(peer, packet(&tree, &b, auth, i), 703));
        }
    }
    assert!(n.network.payload.merkle.owned[0].complete());
    // Unsolicited old replies need no request token/ACK fence and are idempotent.
    assert!(n.inject(0, packet(&tree, &b, auth, 0), 704));
    n.finish();
}
#[test]
fn unrelated_trickle_progress_cannot_postpone_an_existing_leaf_retry() {
    let mut n = Node::new(32768);
    let b = body(3000);
    let (tree, auth, have) = n.source(&b, n.header(&b, 0), None);
    assert!(n.inject(0, have.clone(), 0));
    assert!(n.inject(1, have, 0));
    let (first, _) = n.poll(200);
    let (source, w) = n.wants(&first).into_iter().next().unwrap();
    let missing = w.indices()[0];
    let mut retries = Vec::new();
    let mut others = (0..tree.leaf_count() as u32).filter(|&i| i != missing);
    for now in [300, 400, 500, 600, 700] {
        assert!(n.inject(0, packet(&tree, &b, auth, others.next().unwrap()), now));
        let (p, _) = n.poll(now);
        for (pi, w) in n.wants(&p) {
            if w.indices().contains(&missing) {
                retries.push((now, pi));
            }
        }
    }
    assert!(!retries.is_empty());
    assert!(retries[0].0 <= 700);
    assert_ne!(retries[0].1, source);
    assert!(retries.windows(2).all(|w| w[1].0 - w[0].0 >= RETRY_MIN_MS));
}
#[test]
fn verified_partial_relay_preserves_original_proposer_proof_and_never_pushes() {
    let mut n = Node::new(32768);
    let b = body(1500);
    let (tree, auth, _) = n.source(&b, n.header(&b, 0), None);
    assert!(n.inject(0, packet(&tree, &b, auth, 2), 0));
    let (p, _) = n.poll(1);
    assert!(p.send_bulk.is_empty());
    let want = Want::single(auth.reference, 2);
    assert!(n.inject(1, codec::encode_want(want, n.config.max_body).unwrap(), 2));
    let (p, m) = n.poll(3);
    assert_eq!(p.send_bulk.len(), 1);
    assert_eq!(m.leaf_responses, 1);
    let leaf = codec::decode_leaf(&p.send_bulk[0].1, n.config.max_body).unwrap();
    assert_eq!(leaf.auth, auth);
    assert_eq!(leaf.index, 2);
    let (proof, count) = leaf.proof_array();
    verify_leaf(
        &auth.reference.manifest,
        2,
        leaf.data,
        &proof[..count],
        n.config.max_body,
    )
    .unwrap();
    assert_eq!(m.payload_bytes, leaf.data.len() as u64);
    assert_eq!(m.leaf_proof_bytes, (count * 32) as u64);
    assert_eq!(m.offered_data.bytes, p.send_bulk[0].1.len() as u64);
    let (p, _) = n.poll(4);
    assert!(p.send_bulk.is_empty()); // local work released immediately, no repeat
}
#[test]
fn proposer_initial_pass_covers_all_peers_without_replacement_or_receipts() {
    let mut n = Node::proposer(32768);
    let b = body(5000);
    let h = n.header(&b, 0);
    n.retain_proposal(&b, h);
    let count = Tree::new(&b, 32768, n.config.max_body)
        .unwrap()
        .leaf_count();
    let mut seen: HashMap<_, Vec<_>> = HashMap::new();
    for tick in 0..20 {
        let (push, m) = n.poll(tick * 10);
        assert_eq!(m.leaf_responses, push.send_bulk.len() as u64);
        for (peer, bytes) in push.send_bulk {
            let leaf = codec::decode_leaf(&bytes, n.config.max_body).unwrap();
            assert!(!seen.entry(peer).or_default().contains(&leaf.index));
            seen.entry(peer).or_default().push(leaf.index);
        }
    }
    assert_eq!(seen.len(), 2);
    for order in seen.values() {
        assert_eq!(order.len(), count);
        assert_ne!(order, &(0..count as u32).collect::<Vec<_>>());
    }
    assert!(n.poll(1000).0.send_bulk.is_empty());
}
#[test]
fn repeated_old_pulls_cannot_starve_initial_coverage() {
    let mut n = Node::proposer(32768);
    let b = body(5000);
    let h = n.header(&b, 0);
    n.retain_proposal(&b, h);
    let (tree, auth, _) = n.source(&b, h, None);
    let mut seen = HashSet::new();
    for tick in 0..30 {
        if tick != 0 {
            let mut want = Want::single(auth.reference, 0);
            for i in 1..tree.leaf_count().min(MAX_WANT) {
                want.add(i as u32);
            }
            assert!(n.inject(
                0,
                codec::encode_want(want, n.config.max_body).unwrap(),
                tick * 10
            ));
        }
        let (push, _) = n.poll(tick * 10 + 1);
        for (peer, bytes) in push.send_bulk {
            if peer == n.network.peers[0].address.connection_key() {
                seen.insert(codec::decode_leaf(&bytes, n.config.max_body).unwrap().index);
            }
        }
    }
    assert_eq!(seen.len(), tree.leaf_count());
}
#[test]
fn global_encoded_budget_is_fixed_across_geometries_counts_pending_and_non_bft_backlog() {
    let mut limits = HashSet::new();
    for leaf_bytes in [32768, 102400, 262144, 1048576] {
        let mut n = Node::proposer(leaf_bytes);
        let b = body(3000);
        let h = n.header(&b, 0);
        n.retain_proposal(&b, h);
        let limit = codec::encoded_byte_limit(n.config.max_body, n.config.max_outgoing).unwrap();
        limits.insert(limit);
        let existing = 1234;
        let capacity = n.capacity(limit as u64, 2, (limit - 200_000) as u64);
        let mut staged = NetworkThreadPush::default();
        staged.send_bulk.push((
            n.network.peers[0].address.connection_key(),
            vec![0; existing],
        ));
        let (push, _) = n.poll_with(0, &capacity, staged);
        assert!(total(&push) + limit - 200_000 <= limit);
        for peer in &n.network.peers {
            assert!(
                push.send_bulk
                    .iter()
                    .filter(|(p, _)| *p == peer.address.connection_key())
                    .count()
                    <= 2
            );
        }
        let blocked = n.capacity(u64::MAX, usize::MAX, limit as u64); // includes Base-only peer work in total
        assert!(
            n.poll_with(1, &blocked, NetworkThreadPush::default())
                .0
                .send_bulk
                .is_empty()
        );
        assert!(
            n.network
                .payload
                .merkle
                .peers
                .iter()
                .any(|p| !p.bodies[0].pushed.complete())
        );
    }
    assert_eq!(limits.len(), 1);
}
#[test]
fn full_size_leaf_outgoing_one_and_nonzero_backlog_still_admits() {
    let mut n = Node::proposer(1048576);
    n.config.max_outgoing = 1;
    let initial = body(1);
    let mut block = zcash_primitives::bft::BftBlock::zcash_deserialize(initial.as_slice()).unwrap();
    // The solution's CompactSize length grows from one to five bytes.
    block.headers[0].solution = vec![19; (1 << 20) - initial.len() - 4];
    let b = block.zcash_serialize_to_vec().unwrap();
    assert_eq!(b.len(), 1 << 20);
    let (tree, auth, _) = n.source(&b, n.header(&b, 0), None);
    let ai = n
        .network
        .payload
        .merkle
        .admit(auth, &[], &n.config, 0)
        .unwrap()
        .unwrap();
    let mut bitmap = Bitmap::new(&auth.reference.manifest, n.config.max_body).unwrap();
    bitmap.set(0).unwrap();
    let a = &mut n.network.payload.merkle.owned[ai];
    a.inventory = Inventory::Full(tree, bitmap);
    a.verified = Some(VerifiedBody::new(n.engine.c(), auth.reference.key(), b, n.config.max_body).unwrap());
    a.local_proposer = true;
    let capacity = n.capacity(u64::MAX, usize::MAX, 100);
    let (push, _) = n.poll_with(0, &capacity, NetworkThreadPush::default());
    assert_eq!(push.send_bulk.len(), 1);
    assert!(total(&push) + 100 <= codec::encoded_byte_limit(n.config.max_body, 1).unwrap());
}
#[test]
fn partial_and_complete_ownership_survive_expiry_round_rebind_and_fresh_host_tags() {
    let mut n = Node::new(32768);
    let b = body(1500);
    let (h, outline) = n.outline(&b, 0);
    let (tree, auth, _) = n.source(&b, h, None);
    assert!(n.inject(0, outline, 0));
    assert!(n.inject(0, packet(&tree, &b, auth, 0), 1));
    let expires = n.network.payload.pending[0].expires;
    n.poll(expires + 1);
    assert!(n.network.payload.pending.is_empty());
    assert_eq!(n.network.payload.merkle.owned[0].bitmap().count(), 1);
    let (fresh, outline) = n.outline(&b, 1);
    let (_, new_auth, have) = n.source(&b, fresh, None);
    assert!(n.inject(1, outline, expires + 2));
    assert!(n.inject(1, have, expires + 3));
    assert_eq!(n.network.payload.merkle.owned[0].auth, new_auth);
    for i in 1..tree.leaf_count() {
        assert!(n.inject(0, packet(&tree, &b, auth, i as u32), expires + 4));
    }
    assert_eq!(n.network.payload.merkle.owned.len(), 1);
    let d = n.engine.view().unwrap();
    let slot = d.pending.unwrap().proposal;
    assert_eq!(n.engine.storage().header(slot.index), Some(&fresh));
    assert_eq!(d.statuses[slot.index], Status::Indeterminate);
    let old_tag = BodyTag {
        height: 1,
        header: slot.header,
        generation: d.body_generations[slot.index],
    };
    let (_, outline) = n.outline(&b, 2);
    assert!(n.inject(1, outline, expires + 5));
    let d = n.engine.view().unwrap();
    let slot = d.pending.unwrap().proposal;
    // Core deliberately keeps the first pinned DEC. Cache reuse must not
    // override that rule or mark its existing Host tag Valid.
    assert_eq!(n.engine.storage().header(slot.index), Some(&fresh));
    assert_eq!(slot.header, old_tag.header);
    assert_eq!(d.statuses[slot.index], Status::Indeterminate);
    assert!(
        n.config
            .metrics
            .as_ref()
            .unwrap()
            .snapshot()
            .payload
            .body_cache_hits
            >= 1
    );
    n.finish();
}
#[test]
fn rebind_clears_old_proposer_push_authority_and_cached_have_completes_fresh_header() {
    let mut n = Node::proposer(32768);
    let b = body(1500);
    let h = n.header(&b, 0);
    n.retain_proposal(&b, h);
    n.poll(0);
    assert!(n.network.payload.merkle.owned[0].local_proposer);
    let fresh = n.header(&b, 1);
    let (_, auth, have) = n.source(&b, fresh, None);
    assert!(n.inject(0, have, 1));
    assert_eq!(n.network.payload.merkle.owned[0].auth, auth);
    assert!(!n.network.payload.merkle.owned[0].local_proposer);
    n.poll(2);
    assert!(!n.network.payload.merkle.owned[0].local_proposer); // old core header cannot downgrade
    let (_, _, old_have) = n.source(&b, h, None);
    let delivered = n.network.payload.merkle.completed_deliveries;
    assert_eq!(delivered, 1);
    for now in 3..20 {
        assert!(n.inject(0, old_have.clone(), now));
    }
    assert_eq!(n.network.payload.merkle.completed_deliveries, delivered);
}
#[test]
fn retained_background_source_cannot_evict_needed_partial_with_one_slot() {
    let mut n = Node::proposer(32768);
    n.config.max_requests = 1;
    let a = body(200);
    let ha = n.header(&a, 0);
    n.retain_proposal(&a, ha);
    n.poll(0);
    let b = body(1500);
    let (hb, outline) = n.outline(&b, 0);
    let (tree, auth, _) = n.source(&b, hb, None);
    assert!(n.inject(0, outline, 1));
    for i in 0..tree.leaf_count() {
        assert!(n.inject(0, packet(&tree, &b, auth, i as u32), 2 + i as u64));
        n.poll(2 + i as u64);
        assert_eq!(n.network.payload.merkle.owned.len(), 1);
        assert_eq!(
            n.network.payload.merkle.owned[0].auth.reference.key(),
            auth.reference.key()
        );
        assert_eq!(n.network.payload.merkle.owned[0].bitmap().count(), i + 1);
    }
}
#[test]
fn matching_partial_is_upgraded_in_place_when_core_has_local_body() {
    let mut n = Node::proposer(32768);
    let b = body(1500);
    let h = n.header(&b, 0);
    let (tree, auth, _) = n.source(&b, h, None);
    assert!(n.inject(0, packet(&tree, &b, auth, 0), 0));
    let allocation = n.network.payload.merkle.owned[0].body.as_ptr();
    n.retain_proposal(&b, h);
    n.poll(1);
    assert_eq!(n.network.payload.merkle.owned.len(), 1);
    assert!(n.network.payload.merkle.owned[0].complete());
    assert_eq!(n.network.payload.merkle.owned[0].bytes().as_ptr(), allocation);
    assert_eq!(n.network.payload.merkle.owned[0].body.capacity(), 0);
}
#[test]
fn completed_cache_eviction_and_scope_retirement_are_bounded() {
    let mut n = Node::new(32768);
    n.config.max_requests = 1;
    for headers in [50, 60, 70] {
        let b = body(headers);
        let (tree, auth, _) = n.source(&b, n.header(&b, 0), None);
        assert!(n.inject(0, packet(&tree, &b, auth, 0), headers as u64));
        assert_eq!(n.network.payload.merkle.owned.len(), 1);
        assert_eq!(
            n.network.payload.merkle.body(auth.reference.key()),
            Some(b.as_slice())
        );
    }
    let b = body(70);
    let (tree, auth, _) = n.source(&b, n.header(&b, 0), None);
    let c = n.engine.c().clone();
    n.engine.init(&c, 2, 0, &mut n.events).unwrap();
    assert!(!n.inject(0, packet(&tree, &b, auth, 0), 100));
    assert!(n.network.payload.merkle.owned.is_empty());
    n.network.reset_scope();
    assert!(n.network.payload.merkle.peers.is_empty());
}
#[test]
fn retired_connection_generation_cannot_allocate_but_current_session_can_relay_old_leaf() {
    let mut n = Node::new(32768);
    let b = body(400);
    let (tree, auth, _) = n.source(&b, n.header(&b, 0), None);
    let generation = n.network.peers[0].generation;
    n.network.peers[0].generation += 1;
    n.poll(0);
    let bytes = packet(&tree, &b, auth, 0);
    assert!(
        !n.network
            .receive_merkle(
                &mut n.engine,
                &n.config,
                &mut n.events,
                0,
                generation,
                &bytes,
                1
            )
            .unwrap()
    );
    assert!(n.network.payload.merkle.owned.is_empty());
    assert!(n.inject(0, bytes, 2));
}
#[test]
fn wrong_body_identity_and_noncanonical_complete_content_never_gain_core_roles() {
    for bad_hash in [false, true] {
        let mut n = Node::new(32768);
        let b = vec![7; 50000];
        let h = if bad_hash {
            n.header(&body(400), 0)
        } else {
            n.header(&b, 0)
        };
        let (tree, auth, _) = n.source(&b, h, None);
        for i in 0..tree.leaf_count() - 1 {
            assert!(n.inject(0, packet(&tree, &b, auth, i as u32), 0));
        }
        assert!(!n.inject(0, packet(&tree, &b, auth, tree.leaf_count() as u32 - 1), 1));
        assert!(n.network.payload.merkle.owned.is_empty());
        assert!(n.engine.view().unwrap().slot.is_none());
    }
}

#[test]
fn staged_message_cap_and_preexisting_over_budget_work_refuse_new_allocations() {
    let mut n = Node::proposer(102400);
    let b = body(1500);
    let h = n.header(&b, 0);
    n.retain_proposal(&b, h);
    let cap = n.capacity(u64::MAX, usize::MAX, 0);
    let address = n.network.peers[0].address.connection_key();
    let mut staged = NetworkThreadPush::default();
    staged.send_unreliable = (0..MAX_PUSH_MESSAGES).map(|_| (address, vec![1])).collect();
    let (push, counts) = n.poll_with(0, &cap, staged);
    assert!(push.send_bulk.is_empty());
    assert_eq!(push.send_unreliable.len(), MAX_PUSH_MESSAGES);
    assert_eq!(counts, PayloadMetricsSnapshot::default());
    let limit = codec::encoded_byte_limit(n.config.max_body, n.config.max_outgoing).unwrap();
    let mut staged = NetworkThreadPush::default();
    staged.send_bulk.push((address, vec![0; limit + 1]));
    let (push, counts) = n.poll_with(1, &cap, staged);
    assert_eq!(push.send_bulk.len(), 1);
    assert!(push.send_unreliable.is_empty());
    assert_eq!(counts, PayloadMetricsSnapshot::default());
    assert!(
        n.network
            .payload
            .merkle
            .peers
            .iter()
            .all(|p| p.bodies[0].pushed.count() == 0)
    );
}

#[test]
fn trusted_config_change_retires_owners_even_at_same_synthetic_height() {
    let mut n = Node::new(32768);
    let b = body(400);
    let (tree, auth, _) = n.source(&b, n.header(&b, 0), None);
    assert!(n.inject(0, packet(&tree, &b, auth, 0), 0));
    let c = n
        .config
        .chain_config(&n.config.initial_roster, [42; 32])
        .unwrap()
        .unwrap();
    assert_ne!(c.hash(), n.engine.c().hash());
    n.engine = Engine::boot(MemoryStore::default(), n.key.clone(), c.clone()).unwrap();
    n.engine.init(&c, 1, 0, &mut n.events).unwrap();
    n.poll(1);
    assert!(n.network.payload.merkle.owned.is_empty());
    assert!(!n.inject(0, packet(&tree, &b, auth, 1), 2));
}

#[test]
fn newer_local_source_replaces_expired_partial_but_same_round_refresh_does_not_churn() {
    let mut n = Node::proposer(32768);
    for count in [400, 500] {
        let b = body(count);
        let (h, outline) = n.outline(&b, 0);
        let (tree, auth, _) = n.source(&b, h, None);
        assert!(n.inject(0, outline, 0));
        assert!(n.inject(0, packet(&tree, &b, auth, 0), 1));
    }
    assert_eq!(n.network.payload.merkle.owned.len(), n.config.max_requests);
    let expiry = n
        .network
        .payload
        .pending
        .iter()
        .map(|p| p.expires)
        .max()
        .unwrap();
    n.poll(expiry + 1);
    assert!(n.network.payload.pending.is_empty());
    let old_keys: Vec<_> = n
        .network
        .payload
        .merkle
        .owned
        .iter()
        .map(|a| a.auth.reference.key())
        .collect();
    let same = body(600);
    let same_header = n.header(&same, 0);
    n.retain_proposal(&same, same_header);
    assert!(n.poll(expiry + 2).0.send_bulk.is_empty());
    assert_eq!(
        n.network
            .payload
            .merkle
            .owned
            .iter()
            .map(|a| a.auth.reference.key())
            .collect::<Vec<_>>(),
        old_keys
    );
    let newer = body(700);
    let (header, outline) = n.outline(&newer, 2);
    let metadata = wire::decode_outlined(&outline, n.config.limits()).unwrap();
    metadata
        .with_body(&newer, |frame| {
            n.engine
                .receive(frame, Status::Indeterminate, &mut n.events)
        })
        .unwrap();
    let (first, _) = n.poll(expiry + 3);
    assert!(!first.send_bulk.is_empty());
    assert_eq!(n.network.payload.merkle.owned.len(), n.config.max_requests);
    assert!(
        n.network
            .payload
            .merkle
            .owned
            .iter()
            .any(|a| a.auth.header == header && a.complete())
    );
    let before = n
        .network
        .payload
        .merkle
        .owned
        .iter()
        .map(|a| a.auth.reference.key())
        .collect::<Vec<_>>();
    for now in expiry + 4..expiry + 9 {
        assert!(n.poll(now).0.send_bulk.is_empty());
    }
    assert_eq!(
        n.network
            .payload
            .merkle
            .owned
            .iter()
            .map(|a| a.auth.reference.key())
            .collect::<Vec<_>>(),
        before
    );
}

#[test]
fn completed_owner_moves_allocation_and_repeated_certificates_do_no_body_work() {
    use crate::d28::checked_body::hash_work;
    let mut n = Node::new(32768);
    let b = body(1500);
    let (tree, auth, _) = n.source(&b, n.header(&b, 0), None);
    let leaves: Vec<_> = (0..tree.leaf_count())
        .map(|i| packet(&tree, &b, auth, i as u32))
        .collect();
    let outlines: Vec<_> = (1..5).map(|round| n.outline(&b, round).1).collect();
    assert!(leaves.len() > 1);
    let hash_before = hash_work();
    let canonical_before = super::super::CANONICAL_BYTES.with(std::cell::Cell::get);
    assert!(n.inject(0, leaves[0].clone(), 0));
    let allocation = n.network.payload.merkle.owned[0].body.as_ptr();
    for (i, leaf) in leaves.into_iter().enumerate().skip(1) {
        assert!(n.inject(0, leaf, i as u64));
    }
    let owner = &n.network.payload.merkle.owned[0];
    assert_eq!(owner.bytes().as_ptr(), allocation, "freeze moves rather than copies assembly");
    assert_eq!(owner.body.capacity(), 0, "no second full-body allocation survives freeze");
    let hashes = hash_work();
    assert_eq!(hashes, (hash_before.0 + 1, hash_before.1 + b.len()));
    let canonical = super::super::CANONICAL_BYTES.with(std::cell::Cell::get);
    assert_eq!(canonical, canonical_before + b.len());
    let cached = n.network.payload.cached_body(&n.engine, auth.reference.key()).unwrap();
    let super::super::CachedBody::Verified(body) = cached else {
        panic!("the Merkle owner must win over the retained-core copy");
    };
    assert_eq!(body.bytes().as_ptr(), allocation);
    drop(body);
    for (i, outline) in outlines.iter().enumerate() {
        for repeat in 0..3 {
            assert!(n.inject(1, outline.clone(), 100 + (i * 3 + repeat) as u64));
        }
    }
    assert_eq!(hash_work(), hashes, "12 certificate hits must not rehash a body");
    assert_eq!(super::super::CANONICAL_BYTES.with(std::cell::Cell::get), canonical,
        "12 certificate hits must not parse or re-encode a canonical body");
    assert_eq!(n.network.payload.merkle.owned.len(), 1);
    assert_eq!(n.network.payload.merkle.owned[0].bytes().as_ptr(), allocation);
    let d = n.engine.view().unwrap();
    assert_eq!(d.statuses[d.pending.unwrap().proposal.index], Status::Indeterminate);
    assert_ne!(d.step, Step::Decided, "hash/canonical proofs never supply Host validity");
    n.finish();

    // Run the same metadata stream through the unoptimized retained-core
    // fallback. This measures the saved body passes, not network throughput.
    let mut fallback = Node::new(32768);
    let inline = wire::encode_frame(Frame {
        message: Message::Ordinary, outer: None,
        proposal: Some(Proposal { header: &auth.header, body: &b }), groups: &[],
    }, None, fallback.config.limits()).unwrap();
    assert!(fallback.inject(0, inline, 0));
    let before_hashes = hash_work();
    let before_canonical = super::super::CANONICAL_BYTES.with(std::cell::Cell::get);
    for (i, outline) in outlines.iter().enumerate() {
        for repeat in 0..3 {
            assert!(fallback.inject(1, outline.clone(), 100 + (i * 3 + repeat) as u64));
        }
    }
    let after_hashes = hash_work();
    assert_eq!(after_hashes, (before_hashes.0 + 12, before_hashes.1 + 12 * b.len()));
    assert_eq!(super::super::CANONICAL_BYTES.with(std::cell::Cell::get),
        before_canonical + 12 * b.len());
    assert!(fallback.network.payload.merkle.owned.is_empty());
    eprintln!("checked-body repeat work: body_bytes={}, repeats=12, fallback_hash_bytes={}, cached_hash_bytes=0, fallback_canonical_bytes={}, cached_canonical_bytes=0",
        b.len(), 12 * b.len(), 12 * b.len());
}

#[test]
fn deferred_completed_owner_uses_checked_delivery_and_keeps_rx_budget() {
    use crate::d28::checked_body::hash_work;
    let mut n = Node::new(32768);
    n.config.max_receive_per_tick = 1;
    let b = body(400);
    let (tree, auth, _) = n.source(&b, n.header(&b, 0), None);
    for i in 0..tree.leaf_count() {
        assert!(n.inject(0, packet(&tree, &b, auth, i as u32), i as u64));
    }
    let (_, outline) = n.outline(&b, 1);
    let evidence = wire::decode_outlined(&outline, n.config.limits()).unwrap();
    n.network.payload.defer(auth.reference.key(), evidence, 0,
        n.network.peers[0].generation, &n.config, 20);
    let hashes = hash_work();
    let canonical = super::super::CANONICAL_BYTES.with(std::cell::Cell::get);
    assert_eq!(n.network.complete_cached_payloads(&mut n.engine, &n.config, &mut n.events).unwrap(), 1);
    assert_eq!(n.network.incoming.in_use(), 0);
    assert!(n.network.payload.pending.is_empty());
    assert_eq!(hash_work(), hashes);
    assert_eq!(super::super::CANONICAL_BYTES.with(std::cell::Cell::get), canonical);
    let d = n.engine.view().unwrap();
    assert_eq!(d.statuses[d.pending.unwrap().proposal.index], Status::Indeterminate);
}

#[test]
fn checked_cache_hits_still_reject_fresh_bad_headers_certificates_and_scope() {
    let mut n = Node::new(32768);
    let b = body(400);
    let (tree, auth, _) = n.source(&b, n.header(&b, 0), None);
    for i in 0..tree.leaf_count() {
        assert!(n.inject(0, packet(&tree, &b, auth, i as u32), i as u64));
    }
    let (_, valid) = n.outline(&b, 1);
    let mut bad_header = valid.clone();
    bad_header[107] ^= 1;
    let mut foreign = valid.clone();
    foreign[2] ^= 1;
    let bad_certificate = wire::decode_outlined(&valid, n.config.limits()).unwrap()
        .with_body(&b, |frame| {
            let mut votes: Vec<_> = frame.groups[0].votes.iter().copied().collect();
            votes[0].signature[0] ^= 1;
            let groups = [Group { kind: frame.groups[0].kind, votes: Votes::Dense(&votes) }];
            wire::encode_outlined(Frame { groups: &groups, ..frame }, None,
                n.engine.c().hash(), n.config.limits()).unwrap()
        });
    let writes = n.engine.storage().writes;
    for bad in [bad_header, foreign, bad_certificate] {
        assert!(!n.inject(1, bad, 50));
        assert_eq!(n.engine.storage().writes, writes);
        assert!(n.engine.view().unwrap().pending.is_none());
    }
    assert!(n.inject(1, valid, 51));
    assert!(n.engine.view().unwrap().pending.is_some());
    assert_eq!(n.network.payload.merkle.owned.len(), 1);
}
