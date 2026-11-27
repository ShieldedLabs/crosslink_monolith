//! Buffered persistence regressions. The disk is a deterministic atomic old/new
//! crash model, not evidence about a filesystem's physical power-loss behavior.
use ed25519_zebra::{SigningKey, VerificationKeyBytes};
use std::sync::{Arc, Mutex};
use tenderlink::d28::durable::{DurableStore, Limits, image_size};
use tenderlink::d28::*;

const LIMITS: Limits = Limits {
    max_members: 4,
    max_body: 128,
};
type Node = Engine<DurableStore<Disk>>;

#[derive(Clone)]
struct Disk(Arc<Mutex<DiskState>>);
#[derive(Default)]
struct DiskState {
    bytes: Option<Vec<u8>>,
    replacements: usize,
    fail: Option<(usize, bool)>,
    failed: bool,
    history: Vec<Vec<u8>>,
}
impl Disk {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(DiskState::default())))
    }
    fn seeded(bytes: Vec<u8>) -> Self {
        Self(Arc::new(Mutex::new(DiskState {
            bytes: Some(bytes.clone()),
            history: vec![bytes],
            ..DiskState::default()
        })))
    }
    fn bytes(&self) -> Vec<u8> {
        self.0.lock().unwrap().bytes.clone().unwrap()
    }
    fn count(&self) -> usize {
        self.0.lock().unwrap().replacements
    }
    fn fail_after(&self, further: usize, commit_new: bool) {
        let mut state = self.0.lock().unwrap();
        state.fail = Some((state.replacements + further, commit_new));
    }
    fn clear_failure(&self) {
        let mut state = self.0.lock().unwrap();
        state.fail = None;
        state.failed = false;
    }
    fn loaded(&self) -> DurableStore<Self> {
        // An independent read-only observer never shares the live working image.
        DurableStore::open(Self::seeded(self.bytes()), LIMITS).unwrap()
    }
    fn history(&self) -> Vec<Vec<u8>> {
        self.0.lock().unwrap().history.clone()
    }
}
impl AtomicBlockStore for Disk {
    fn block_size(&self) -> usize {
        image_size(LIMITS).unwrap()
    }
    fn load(&mut self, block: &mut [u8]) -> Result<bool> {
        assert_eq!(block.len(), self.block_size());
        let state = self.0.lock().unwrap();
        let Some(saved) = &state.bytes else {
            return Ok(false);
        };
        assert_eq!(saved.len(), block.len());
        block.copy_from_slice(saved);
        Ok(true)
    }
    fn replace(&mut self, next: &[u8]) -> Result<()> {
        assert_eq!(next.len(), self.block_size());
        let mut state = self.0.lock().unwrap();
        assert!(
            !state.failed,
            "backend called again after ambiguous failure"
        );
        let fail = state.fail.filter(|(at, _)| *at == state.replacements);
        state.replacements += 1;
        if fail.is_none_or(|(_, commit_new)| commit_new) {
            state.bytes = Some(next.to_vec());
            state.history.push(next.to_vec());
        }
        if fail.is_some() {
            state.failed = true;
            // Unexpected backend variants must also normalize to Storage.
            Err(Error::Invalid)
        } else {
            Ok(())
        }
    }
}

fn key(signer: usize) -> SigningKey {
    SigningKey::from([u8::try_from(signer + 1).unwrap(); 32])
}
fn config(weights: [u64; 4], crosslink: bool) -> Config {
    let members = weights
        .into_iter()
        .enumerate()
        .map(|(i, weight)| Member {
            public_key: VerificationKeyBytes::from(&key(i)).into(),
            weight,
        })
        .collect();
    let timeouts = Timeouts {
        propose: 30,
        prevote: 5,
        precommit: 10,
        inc_propose: 3,
        inc_prevote: 1,
        inc_precommit: 2,
    };
    if crosslink {
        Config::crosslink([42; 32], members, LIMITS.max_body, 10, timeouts)
    } else {
        Config::new([42; 32], members, LIMITS.max_body, 10, timeouts)
    }
    .unwrap()
}
fn vote(c: &Config, signer: usize, round: u64, kind: VoteKind, id: Option<Id>) -> Vote {
    Vote::sign(
        c,
        &key(signer),
        VoteContent {
            height: 4,
            round,
            kind,
            id,
            signer: u32::try_from(signer).unwrap(),
        },
    )
}
fn header(c: &Config, round: u64, body: &[u8]) -> Header {
    let signer = c.proposer(4, round);
    Header::sign(
        c,
        &key(signer),
        ProposalContent {
            height: 4,
            round,
            valid_round: None,
            id: c.body_id(body),
            signer: u32::try_from(signer).unwrap(),
        },
    )
}
fn ordinary<'a>(
    outer: Option<&'a Vote>,
    proposal: Option<Proposal<'a>>,
    groups: &'a [Group<'a>],
) -> Frame<'a> {
    Frame {
        message: Message::Ordinary,
        outer,
        proposal,
        groups,
    }
}
fn vote_index(kind: VoteKind) -> usize {
    match kind {
        VoteKind::Enter => 0,
        VoteKind::Prevote => 1,
        VoteKind::Precommit => 2,
    }
}
fn assert_complete(store: &DurableStore<Disk>) {
    let Some(d) = store.descriptor() else {
        return;
    };
    let c = store.config().unwrap();
    assert_eq!(d.config, c.hash());
    for slot in [
        d.slot,
        d.vc.map(|p| p.proposal),
        d.pending.map(|p| p.proposal),
    ]
    .into_iter()
    .flatten()
    {
        let h = store.header(slot.index).expect("published header missing");
        assert!(h.verify(c));
        assert_eq!(h.identity(c), slot.header);
        assert_eq!(h.content.height, d.height);
        assert_eq!(c.body_id(store.body(slot.index)), h.content.id);
        assert!(store.body(slot.index).len() <= c.max_body());
    }
    if let Some(intent) = d.proposal_intent {
        if !d.proposal_published {
            assert_eq!(c.body_id(store.body(intent.slot)), intent.content.id);
        } else {
            assert_eq!(store.own_header().unwrap().content, intent.content);
        }
    }
    for region in [
        d.entry,
        d.vc.map(|p| p.certificate),
        d.pending.map(|p| p.certificate),
    ]
    .into_iter()
    .flatten()
    {
        let h = store.region_header(region.index).unwrap();
        assert_eq!(h.digest, region.digest);
        let rows = store.region_rows(region.index);
        assert_eq!(rows.iter().flatten().count(), h.count);
        assert!(rows.iter().flatten().all(|v| v.verify(c)));
    }
}
fn assert_proposal(store: &DurableStore<Disk>, p: Proposal<'_>) {
    let d = store.descriptor().unwrap();
    assert!(
        [
            d.slot,
            d.vc.map(|p| p.proposal),
            d.pending.map(|p| p.proposal)
        ]
        .into_iter()
        .flatten()
        .any(|s| store.header(s.index) == Some(p.header) && store.body(s.index) == p.body),
        "output proposal is not completely durable"
    );
}

struct Out {
    disk: Disk,
    me: usize,
    votes: Vec<Vote>,
    proposals: Vec<Header>,
    decisions: Vec<Header>,
    calls: usize,
    retries: usize,
}
impl Out {
    fn new(disk: Disk, me: usize) -> Self {
        Self {
            disk,
            me,
            votes: Vec::new(),
            proposals: Vec::new(),
            decisions: Vec::new(),
            calls: 0,
            retries: 0,
        }
    }
    fn observe(&mut self) -> DurableStore<Disk> {
        assert!(
            !self.disk.0.lock().unwrap().failed,
            "callback after failed replace"
        );
        self.calls += 1;
        let store = self.disk.loaded();
        assert_complete(&store);
        store
    }
    fn assert_non_equivocation(&self) {
        for (i, before) in self.votes.iter().enumerate() {
            for after in &self.votes[i + 1..] {
                if (
                    before.content.height,
                    before.content.round,
                    before.content.kind,
                ) == (
                    after.content.height,
                    after.content.round,
                    after.content.kind,
                ) {
                    assert_eq!(before.content, after.content, "conflicting own votes");
                }
            }
        }
        for (i, before) in self.proposals.iter().enumerate() {
            for after in &self.proposals[i + 1..] {
                if (before.content.height, before.content.round)
                    == (after.content.height, after.content.round)
                {
                    assert_eq!(before.content, after.content, "conflicting own proposals");
                }
            }
        }
    }
}
impl Sink for Out {
    fn send(&mut self, _: Target, frame: Frame<'_>) {
        let store = self.observe();
        let d = store.descriptor().unwrap();
        if let Some(v) = frame.outer {
            assert_eq!(d.intents[vote_index(v.content.kind)], Some(v.content));
            assert_eq!(store.own(v.content.kind), Some(v));
            assert!(v.verify(store.config().unwrap()));
            self.votes.push(*v);
        }
        if let Some(p) = frame.proposal {
            assert_proposal(&store, p);
            if p.header.content.signer as usize == self.me {
                assert_eq!(d.proposal_intent.unwrap().content, p.header.content);
                assert!(d.proposal_published);
                assert_eq!(store.own_header(), Some(p.header));
                self.proposals.push(*p.header);
            }
        }
        if frame.message == Message::Dec {
            assert_eq!(d.step, Step::Decided);
        }
    }
    fn decided(&mut self, height: u64, proposal: Proposal<'_>, certificate: Group<'_>) {
        let store = self.observe();
        let d = store.descriptor().unwrap();
        assert_eq!(d.step, Step::Decided);
        assert_eq!(d.height, height);
        assert_proposal(&store, proposal);
        let region = d.pending.unwrap().certificate.index;
        assert_eq!(store.region_header(region).unwrap().kind, certificate.kind);
        assert_eq!(
            store
                .region_rows(region)
                .iter()
                .flatten()
                .copied()
                .collect::<Vec<_>>(),
            certificate.votes.iter().copied().collect::<Vec<_>>()
        );
        self.decisions.push(*proposal.header);
    }
    fn acquire(&mut self, tag: ProposalTag) {
        let store = self.observe();
        let d = store.descriptor().unwrap();
        assert_eq!((d.height, d.round), (tag.height, tag.round));
    }
    fn retry(&mut self, tag: BodyTag, proposal: Proposal<'_>) {
        let store = self.observe();
        let d = store.descriptor().unwrap();
        assert_proposal(&store, proposal);
        assert_eq!(d.height, tag.height);
        assert!(
            [d.slot, d.pending.map(|p| p.proposal)]
                .into_iter()
                .flatten()
                .any(|s| s.header == tag.header && d.body_generations[s.index] == tag.generation)
        );
        self.retries += 1;
    }
}

fn boot(c: &Config, me: usize, emission: Emission, buffered: bool) -> (Node, Disk, Out) {
    let disk = Disk::new();
    let store = if buffered {
        DurableStore::open_buffered(disk.clone(), LIMITS)
    } else {
        DurableStore::open(disk.clone(), LIMITS)
    }
    .unwrap();
    let mut e = Engine::boot(store, key(me), c.clone()).unwrap();
    e.set_emission(emission);
    let mut out = Out::new(disk.clone(), me);
    e.init(c, 4, 0, &mut out).unwrap();
    (e, disk, out)
}
fn recover(disk: Disk, me: usize, emission: Emission, out: &mut Out) -> Node {
    Engine::recover_with_emission(
        DurableStore::open_buffered(disk, LIMITS).unwrap(),
        key(me),
        emission,
        out,
    )
    .unwrap()
}
fn supply(e: &mut Node, body: &[u8], out: &mut Out) -> Result<()> {
    e.supply_proposal(e.proposal_tag().unwrap(), body, Status::Valid, out)
}
fn receive_vote(e: &mut Node, vote: &Vote, out: &mut Out) {
    e.receive(ordinary(Some(vote), None, &[]), Status::Valid, out)
        .unwrap();
}
fn assert_history(disk: &Disk, me: usize, emission: Emission) {
    let history = disk.history();
    let mut previous: Option<DurableStore<Disk>> = None;
    for image in history {
        assert_eq!(image.len(), image_size(LIMITS).unwrap());
        let copy = Disk::seeded(image);
        let store = copy.loaded();
        assert_complete(&store);
        // A first committed signed OWN record must have an earlier committed
        // intent. This distinguishes a pre-sign fence from an output-only flush.
        for kind in [VoteKind::Enter, VoteKind::Prevote, VoteKind::Precommit] {
            if let Some(v) = store.own(kind) {
                if previous.as_ref().and_then(|p| p.own(kind)) != Some(v) {
                    let before = previous.as_ref().expect("OWN before intent image");
                    assert_eq!(
                        before.descriptor().unwrap().intents[vote_index(kind)],
                        Some(v.content)
                    );
                }
            }
        }
        if let Some(h) = store.own_header() {
            if previous.as_ref().and_then(|p| p.own_header()) != Some(h) {
                let before = previous.as_ref().expect("header before intent image");
                let intent = before.descriptor().unwrap().proposal_intent.unwrap();
                assert_eq!(intent.content, h.content);
                assert_eq!(
                    before.config().unwrap().body_id(before.body(intent.slot)),
                    h.content.id
                );
            }
        }
        if store.descriptor().is_some() {
            let mut replay = Out::new(copy.clone(), me);
            // Recovery revalidates hashes, certificate weights and publication
            // references, not just the local structural assertions above.
            let _ = recover(copy, me, emission, &mut replay);
        }
        previous = Some(store);
    }
}

#[test]
fn buffered_barriers_preserve_complete_signing_lock_and_decision_state() {
    for crosslink in [false, true] {
        for emission in [Emission::Reference, Emission::Guided] {
            let c = config([4, 3, 2, 1], crosslink);
            let (mut e, disk, mut out) = boot(&c, 0, emission, true);
            let body = [27; LIMITS.max_body];
            supply(&mut e, &body, &mut out).unwrap();
            let id = c.body_id(&body);
            receive_vote(
                &mut e,
                &vote(&c, 1, 0, VoteKind::Prevote, Some(id)),
                &mut out,
            );
            assert_eq!(e.view().unwrap().step, Step::Precommit);
            assert_eq!(e.view().unwrap().locked_id, Some(id));
            let saved = disk.loaded().descriptor().unwrap().clone();
            let own = *e.storage().own(VoteKind::Precommit).unwrap();
            let mut e = recover(e.into_storage().into_backend(), 0, emission, &mut out);
            assert_eq!(e.view(), Some(&saved));
            assert_eq!(e.storage().own(VoteKind::Precommit), Some(&own));
            receive_vote(
                &mut e,
                &vote(&c, 1, 0, VoteKind::Precommit, Some(id)),
                &mut out,
            );
            assert_eq!(e.view().unwrap().step, Step::Decided);
            let decisions = out.decisions.len();
            let e = recover(e.into_storage().into_backend(), 0, emission, &mut out);
            assert_eq!(out.decisions.len(), decisions + 1);
            assert_eq!(
                e.storage()
                    .body(e.view().unwrap().pending.unwrap().proposal.index),
                body
            );
            out.assert_non_equivocation();
            assert_history(&disk, 0, emission);
        }
    }
}

#[test]
fn buffered_countdowns_and_old_vote_replays_do_not_rewrite_until_checkpoint() {
    let c = config([1; 4], false);
    let (mut e, disk, mut out) = boot(&c, 0, Emission::Guided, true);
    let initial = disk.bytes();
    let count = disk.count();
    let calls = out.calls;
    for _ in 0..23 {
        e.elapse(1, &mut out).unwrap();
    }
    assert!(out.calls > calls, "rho replay must actually run");
    assert_eq!(disk.count(), count);
    assert_eq!(disk.bytes(), initial);
    assert_eq!(e.view().unwrap().timers[0].remaining, 7);
    assert_eq!(e.view().unwrap().grid_remaining, 7);
    let crash = Disk::seeded(disk.bytes());
    let mut replay = Out::new(crash.clone(), 0);
    let restored = recover(crash, 0, Emission::Guided, &mut replay);
    assert_eq!(restored.view().unwrap().timers[0].remaining, 30);
    assert_eq!(restored.view().unwrap().grid_remaining, 10);
    let live = e.view().unwrap().clone();
    e.checkpoint().unwrap();
    assert_eq!(disk.count(), count + 1);
    assert_eq!(disk.loaded().descriptor(), Some(&live));
    e.checkpoint().unwrap();
    assert_eq!(disk.count(), count + 1, "clean checkpoint must not replace");
    let e = recover(
        e.into_storage().into_backend(),
        0,
        Emission::Guided,
        &mut out,
    );
    assert_eq!(e.view(), Some(&live));
}

#[test]
fn received_complete_body_is_durable_but_partial_peer_rows_are_not() {
    for emission in [Emission::Reference, Emission::Guided] {
        let c = config([1; 4], false);
        let (mut e, disk, mut out) = boot(&c, 3, emission, true);
        let body = [54; LIMITS.max_body];
        let h = header(&c, 0, &body);
        let calls = out.calls;
        e.receive(
            ordinary(
                None,
                Some(Proposal {
                    header: &h,
                    body: &body,
                }),
                &[],
            ),
            Status::Indeterminate,
            &mut out,
        )
        .unwrap();
        assert_eq!(
            out.calls, calls,
            "this path must not rely on an output barrier"
        );
        let slot = e.view().unwrap().slot.unwrap();
        assert_eq!(disk.loaded().body(slot.index), body);
        assert_eq!(disk.loaded().header(slot.index), Some(&h));
        let count = disk.count();
        receive_vote(
            &mut e,
            &vote(&c, 1, 0, VoteKind::Prevote, Some(h.content.id)),
            &mut out,
        );
        assert!(e.prevotes()[1].is_some());
        assert_eq!(disk.count(), count);
        let mut e = recover(e.into_storage().into_backend(), 3, emission, &mut out);
        assert!(e.prevotes().iter().all(Option::is_none));
        assert_eq!(e.storage().body(slot.index), body);
        e.elapse(c.rho(), &mut out).unwrap();
        assert!(out.retries > 0);
        e.validity(e.body_tag(slot), Status::Valid, &mut out)
            .unwrap();
        assert_eq!(
            e.storage().own(VoteKind::Prevote).unwrap().content.id,
            Some(h.content.id)
        );
        assert_history(&disk, 3, emission);
    }
}

#[test]
fn staged_body_and_certificate_rows_are_discarded_without_a_barrier() {
    let c = config([1; 4], false);
    let (e, disk, _) = boot(&c, 0, Emission::Guided, true);
    let original = disk.bytes();
    let count = disk.count();
    let mut store = e.into_storage();
    let body = [99; LIMITS.max_body];
    let h = header(&c, 0, &body);
    let row = vote(&c, 1, 0, VoteKind::Prevote, Some(h.content.id));
    store.write_body(2, &body).unwrap();
    store.write_header(2, h).unwrap();
    store.write_region_row(3, 1, Some(row)).unwrap();
    assert_eq!(store.body(2), body);
    assert_eq!(store.region_rows(3)[1], Some(row));
    assert_eq!(disk.count(), count);
    assert_eq!(disk.bytes(), original);
    let store = DurableStore::open_buffered(store.into_backend(), LIMITS).unwrap();
    assert!(store.body(2).is_empty());
    assert!(store.header(2).is_none());
    assert!(store.region_rows(3).iter().all(Option::is_none));
    assert_eq!(disk.bytes(), original);
}

#[test]
fn recovered_lock_rejects_unproved_conflicting_proposal_in_later_round() {
    for crosslink in [false, true] {
        let c = config([4, 3, 2, 1], crosslink);
        let (mut e, disk, mut out) = boot(&c, 0, Emission::Reference, true);
        supply(&mut e, b"locked", &mut out).unwrap();
        let id = c.body_id(b"locked");
        receive_vote(
            &mut e,
            &vote(&c, 1, 0, VoteKind::Prevote, Some(id)),
            &mut out,
        );
        let mut e = recover(
            e.into_storage().into_backend(),
            0,
            Emission::Reference,
            &mut out,
        );
        let future = [
            vote(&c, 1, 1, VoteKind::Prevote, None),
            vote(&c, 2, 1, VoteKind::Prevote, None),
        ];
        let groups = [Group {
            kind: CertKind::Row {
                round: 1,
                kind: VoteKind::Prevote,
            },
            votes: Votes::Dense(&future),
        }];
        e.receive(ordinary(None, None, &groups), Status::Valid, &mut out)
            .unwrap();
        assert_eq!(e.view().unwrap().round, 1);
        let h = header(&c, 1, b"conflicting");
        e.receive(
            ordinary(
                None,
                Some(Proposal {
                    header: &h,
                    body: b"conflicting",
                }),
                &[],
            ),
            Status::Valid,
            &mut out,
        )
        .unwrap();
        assert_eq!(e.storage().own(VoteKind::Prevote).unwrap().content.id, None);
        assert_eq!(e.view().unwrap().locked_round, Some(0));
        assert_eq!(e.view().unwrap().locked_id, Some(id));
        let e = recover(
            e.into_storage().into_backend(),
            0,
            Emission::Reference,
            &mut out,
        );
        assert_eq!(e.view().unwrap().locked_id, Some(id));
        assert_eq!(e.storage().own(VoteKind::Prevote).unwrap().content.id, None);
        out.assert_non_equivocation();
        assert_history(&disk, 0, Emission::Reference);
    }
}

#[test]
fn reproposal_copies_complete_retained_body_before_new_header_intent() {
    for crosslink in [false, true] {
        let c = config([4, 3, 2, 1], crosslink);
        let (mut e, disk, mut out) = boot(&c, 1, Emission::Guided, true);
        let body = [71; LIMITS.max_body];
        let h = header(&c, 0, &body);
        e.receive(
            ordinary(
                None,
                Some(Proposal {
                    header: &h,
                    body: &body,
                }),
                &[],
            ),
            Status::Valid,
            &mut out,
        )
        .unwrap();
        receive_vote(
            &mut e,
            &vote(&c, 0, 0, VoteKind::Prevote, Some(h.content.id)),
            &mut out,
        );
        assert_eq!(e.view().unwrap().locked_id, Some(h.content.id));
        let mut e = recover(
            e.into_storage().into_backend(),
            1,
            Emission::Guided,
            &mut out,
        );
        receive_vote(&mut e, &vote(&c, 0, 1, VoteKind::Enter, None), &mut out);
        let d = e.view().unwrap();
        assert_eq!(d.round, 1);
        assert!(d.proposal_published);
        let slot = d.slot.unwrap();
        let retained = d.vc.unwrap().proposal;
        assert_ne!(slot.index, retained.index);
        assert_ne!(slot.header, retained.header);
        assert_eq!(
            e.storage().header(slot.index).unwrap().content.valid_round,
            Some(0)
        );
        assert_eq!(disk.loaded().body(slot.index), body);
        assert_eq!(disk.loaded().body(retained.index), body);
        let mut behind = e.status().unwrap();
        behind.proposal = None;
        behind.polka = None;
        behind.justified = false;
        assert!(matches!(
            e.serve(2, &behind, &mut out),
            Ok(gossip::Served::Sent(_))
        ));
        let e = recover(
            e.into_storage().into_backend(),
            1,
            Emission::Guided,
            &mut out,
        );
        assert_eq!(
            e.storage().body(e.view().unwrap().slot.unwrap().index),
            body
        );
        assert_history(&disk, 1, Emission::Guided);
        out.assert_non_equivocation();
    }
}

#[test]
fn serve_flushes_staged_publication_and_fails_closed_before_callback() {
    let c = config([1; 4], false);
    for commit_new in [false, true] {
        let (mut e, disk, mut out) = boot(&c, 3, Emission::Guided, true);
        let body = b"retained but not yet validated";
        let h = header(&c, 0, body);
        e.receive(
            ordinary(None, Some(Proposal { header: &h, body }), &[]),
            Status::Indeterminate,
            &mut out,
        )
        .unwrap();
        let count = disk.count();
        let calls = out.calls;
        let votes: Vec<_> = (0..3)
            .map(|signer| vote(&c, signer, 0, VoteKind::Prevote, Some(h.content.id)))
            .collect();
        let groups = [Group {
            kind: CertKind::Row {
                round: 0,
                kind: VoteKind::Prevote,
            },
            votes: Votes::Dense(&votes),
        }];
        e.receive(ordinary(None, None, &groups), Status::Valid, &mut out)
            .unwrap();
        assert_eq!(out.calls, calls);
        assert!(e.view().unwrap().timers[1].active);
        assert_eq!(disk.count(), count, "arming alone is staged");
        assert!(!disk.loaded().descriptor().unwrap().timers[1].active);
        let mut peer = e.status().unwrap();
        peer.proposal = None;
        peer.justified = false;
        assert_eq!(e.gap_for(&peer), Some(gossip::Gap::Proposal));
        disk.fail_after(0, commit_new);
        assert_eq!(e.serve(1, &peer, &mut out), Err(Error::Storage));
        assert_eq!(
            out.calls, calls,
            "serve emitted before its barrier succeeded"
        );
        assert_inactive(&mut e, &c, &mut out);
        disk.clear_failure();
        let e = recover(
            e.into_storage().into_backend(),
            3,
            Emission::Guided,
            &mut out,
        );
        assert_eq!(e.view().unwrap().timers[1].active, commit_new);
        assert_eq!(
            e.storage().body(e.view().unwrap().slot.unwrap().index),
            body
        );
        assert!(e.prevotes().iter().all(Option::is_none));
    }
}

fn assert_inactive(e: &mut Node, c: &Config, out: &mut Out) {
    let calls = out.calls;
    let count = out.disk.count();
    assert!(!e.is_active());
    assert!(e.storage().is_poisoned());
    assert_eq!(e.elapse(100, out), Err(Error::Inactive));
    assert_eq!(
        e.receive(ordinary(None, None, &[]), Status::Valid, out),
        Err(Error::Inactive)
    );
    assert_eq!(
        e.supply_proposal(
            ProposalTag {
                height: 4,
                round: 0
            },
            b"later",
            Status::Valid,
            out
        ),
        Err(Error::Inactive)
    );
    assert_eq!(
        e.validity(
            BodyTag {
                height: 4,
                header: [0; 32],
                generation: 0
            },
            Status::Valid,
            out
        ),
        Err(Error::Inactive)
    );
    assert_eq!(e.init(c, 9, 0, out), Err(Error::Inactive));
    assert_eq!(e.checkpoint(), Err(Error::Inactive));
    let status = gossip::StatusSnapshot {
        config: c.hash(),
        height: 4,
        round: 0,
        step: Step::Propose,
        proposal: None,
        polka: None,
        prevote_quorum: false,
        justified: false,
    };
    assert_eq!(e.serve(1, &status, out), Err(Error::Inactive));
    assert_eq!(out.calls, calls, "inactive entry point emitted output");
    assert_eq!(
        out.disk.count(),
        count,
        "inactive entry point attempted I/O"
    );
}

#[test]
fn every_buffered_replace_failure_poison_and_recovery_never_equivocate() {
    for crosslink in [false, true] {
        for emission in [Emission::Reference, Emission::Guided] {
            let c = config([7, 1, 1, 1], crosslink);
            let (e, disk, _) = boot(&c, 0, emission, true);
            let original = disk.bytes();
            drop(e);
            let mut out = Out::new(disk.clone(), 0);
            let mut baseline = recover(disk.clone(), 0, emission, &mut out);
            let before = disk.count();
            supply(&mut baseline, b"first", &mut out).unwrap();
            let writes = disk.count() - before;
            assert!(writes > 0);
            assert_eq!(baseline.view().unwrap().step, Step::Decided);
            for commit_new in [false, true] {
                for cut in 0..writes {
                    let disk = Disk::seeded(original.clone());
                    let mut out = Out::new(disk.clone(), 0);
                    let mut e = recover(disk.clone(), 0, emission, &mut out);
                    disk.fail_after(cut, commit_new);
                    assert_eq!(
                        supply(&mut e, b"first", &mut out),
                        Err(Error::Storage),
                        "cut={cut}, new={commit_new}"
                    );
                    assert_inactive(&mut e, &c, &mut out);
                    let mut store = e.into_storage();
                    let count = disk.count();
                    assert_eq!(store.flush(), Err(Error::Storage));
                    assert_eq!(store.flush_publication(), Err(Error::Storage));
                    assert_eq!(store.seal_config(&c), Err(Error::Storage));
                    assert_eq!(store.write_body(0, b"later"), Err(Error::Storage));
                    assert_eq!(disk.count(), count);
                    let committed = disk
                        .loaded()
                        .descriptor()
                        .unwrap()
                        .proposal_intent
                        .map(|i| i.content.id);
                    disk.clear_failure();
                    let mut e = recover(store.into_backend(), 0, emission, &mut out);
                    if e.proposal_tag().is_some() {
                        supply(&mut e, b"second", &mut out).unwrap();
                    }
                    assert_eq!(e.view().unwrap().step, Step::Decided);
                    let slot = e.view().unwrap().pending.unwrap().proposal.index;
                    assert_eq!(
                        e.storage().header(slot).unwrap().content.id,
                        committed.unwrap_or(c.body_id(b"second"))
                    );
                    out.assert_non_equivocation();
                }
            }
        }
    }
}

#[test]
fn checkpoint_failure_with_only_progress_poison_before_any_later_output() {
    let c = config([1; 4], false);
    for commit_new in [false, true] {
        let (mut e, disk, mut out) = boot(&c, 0, Emission::Guided, true);
        let original = disk.loaded().descriptor().unwrap().clone();
        e.elapse(7, &mut out).unwrap();
        let live = e.view().unwrap().clone();
        disk.fail_after(0, commit_new);
        assert_eq!(e.checkpoint(), Err(Error::Storage));
        assert_inactive(&mut e, &c, &mut out);
        disk.clear_failure();
        let e = recover(
            e.into_storage().into_backend(),
            0,
            Emission::Guided,
            &mut out,
        );
        assert_eq!(e.view(), Some(if commit_new { &live } else { &original }));
    }
}

#[test]
fn height_configuration_and_roster_shrink_are_atomic_even_without_output() {
    let old = config([7, 1, 1, 1], false);
    let next =
        Config::crosslink([81; 32], old.members()[..2].to_vec(), 64, 7, old.timeouts()).unwrap();
    for tau in [0, 5] {
        let (mut e, disk, mut out) = boot(&old, 0, Emission::Guided, true);
        supply(&mut e, b"old body", &mut out).unwrap();
        let original = disk.bytes();
        let count = disk.count();
        let calls = out.calls;
        e.init(&next, 5, tau, &mut out).unwrap();
        if tau != 0 {
            assert_eq!(out.calls, calls);
        }
        let writes = disk.count() - count;
        assert!(writes > 0);
        assert_eq!(disk.loaded().descriptor().unwrap().height, 5);
        assert_eq!(disk.loaded().config(), Some(&next));
        assert_eq!(e.storage().limits(), LIMITS);
        assert_history(&disk, 0, Emission::Guided);
        for commit_new in [false, true] {
            for cut in 0..writes {
                let disk = Disk::seeded(original.clone());
                let mut out = Out::new(disk.clone(), 0);
                let mut e = recover(disk.clone(), 0, Emission::Guided, &mut out);
                disk.fail_after(cut, commit_new);
                assert_eq!(e.init(&next, 5, tau, &mut out), Err(Error::Storage));
                assert_inactive(&mut e, &old, &mut out);
                disk.clear_failure();
                let mut e = recover(
                    e.into_storage().into_backend(),
                    0,
                    Emission::Guided,
                    &mut out,
                );
                match e.view().unwrap().height {
                    4 => {
                        assert_eq!(e.storage().config(), Some(&old));
                        assert_eq!(e.view().unwrap().step, Step::Decided);
                        e.init(&next, 5, tau, &mut out).unwrap();
                    }
                    5 => assert_eq!(e.storage().config(), Some(&next)),
                    height => panic!("mixed authority at height {height}"),
                }
                let saved = e.view().unwrap().clone();
                assert!(!e.init(&old, 4, 0, &mut out).unwrap());
                assert_eq!(e.view(), Some(&saved));
                assert_eq!(e.storage().config(), Some(&next));
                assert_eq!(disk.bytes().len(), image_size(LIMITS).unwrap());
                out.assert_non_equivocation();
            }
        }
    }
}

#[test]
fn progress_descriptor_cannot_smuggle_authority_or_publication_changes() {
    let c = config([1; 4], false);
    let (_, disk, _) = boot(&c, 0, Emission::Guided, true);
    let original = disk.bytes();
    let changes: &[fn(&mut Descriptor)] = &[
        |d| d.height += 1,
        |d| d.config = [3; 32],
        |d| d.round += 1,
        |d| d.step = Step::Prevote,
        |d| d.locked_round = Some(0),
        |d| d.locked_id = Some([3; 32]),
        |d| d.valid_round = Some(0),
        |d| {
            d.slot = Some(SlotRef {
                index: 0,
                header: [3; 32],
            })
        },
        |d| d.justified = true,
        |d| {
            d.entry = Some(RegionRef {
                index: 0,
                digest: [3; 32],
            })
        },
        |d| {
            d.vc = Some(Certified {
                proposal: SlotRef {
                    index: 0,
                    header: [3; 32],
                },
                certificate: RegionRef {
                    index: 0,
                    digest: [3; 32],
                },
            })
        },
        |d| {
            d.pending = Some(Certified {
                proposal: SlotRef {
                    index: 0,
                    header: [3; 32],
                },
                certificate: RegionRef {
                    index: 0,
                    digest: [3; 32],
                },
            })
        },
        |d| d.statuses[0] = Status::Valid,
        |d| d.body_generations[0] += 1,
        |d| d.next_generation += 1,
        |d| d.intents[0] = None,
        |d| {
            d.proposal_intent = Some(ProposalIntent {
                content: ProposalContent {
                    height: d.height,
                    round: d.round,
                    valid_round: None,
                    id: [3; 32],
                    signer: 0,
                },
                slot: 0,
            })
        },
        |d| d.proposal_published = true,
        |d| d.timers[0].active = false,
        |d| d.timers[0].height += 1,
        |d| d.timers[0].round += 1,
    ];
    for (case, change) in changes.iter().enumerate() {
        let disk = Disk::seeded(original.clone());
        let mut store = DurableStore::open_buffered(disk.clone(), LIMITS).unwrap();
        let mut d = store.descriptor().unwrap().clone();
        change(&mut d);
        assert_eq!(
            store.write_progress_descriptor(&d),
            Err(Error::Storage),
            "case={case}"
        );
        assert!(store.is_poisoned());
        assert_eq!(store.flush(), Err(Error::Storage));
        assert_eq!(store.flush_publication(), Err(Error::Storage));
        assert_eq!(disk.count(), 0);
        assert_eq!(disk.bytes(), original);
    }
}

#[test]
fn buffered_mode_batches_physical_replacements_without_changing_fixed_format() {
    let c = config([7, 1, 1, 1], false);
    let mut replacements = Vec::new();
    for buffered in [false, true] {
        let (mut e, disk, mut out) = boot(&c, 0, Emission::Reference, buffered);
        supply(&mut e, b"same decision", &mut out).unwrap();
        assert_eq!(e.view().unwrap().step, Step::Decided);
        replacements.push(disk.count());
        let eager = DurableStore::open(e.into_storage().into_backend(), LIMITS).unwrap();
        assert_eq!(eager.descriptor().unwrap().step, Step::Decided);
        assert_eq!(eager.config(), Some(&c));
        assert!(
            disk.history()
                .iter()
                .all(|b| b.len() == image_size(LIMITS).unwrap())
        );
    }
    assert!(
        replacements[1] < replacements[0],
        "buffered={:?}, eager={:?}",
        replacements[1],
        replacements[0]
    );
}
