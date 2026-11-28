use ed25519_zebra::{SigningKey, VerificationKeyBytes};
use std::sync::{Arc, Mutex};
use tenderlink::d28::durable::{DurableStore, Limits, image_size};
use tenderlink::d28::*;

const LIMITS: Limits = Limits {
    max_members: 4,
    max_body: 128,
};

// This is a deterministic crash model, not a physical durability implementation.
// Cloned handles let the tests inspect disk state and choose old/new outcomes.
// Only the engine under test writes it. History is test-only crash-cut evidence.
#[derive(Clone)]
struct Disk {
    size: usize,
    state: Arc<Mutex<DiskState>>,
}
#[derive(Default)]
struct DiskState {
    block: Option<Vec<u8>>,
    replacements: usize,
    fail: Option<(usize, bool)>,
    load_error: bool,
    history: Vec<Vec<u8>>,
}
impl Disk {
    fn new(limits: Limits) -> Self {
        Self {
            size: image_size(limits).unwrap(),
            state: Arc::new(Mutex::new(DiskState::default())),
        }
    }
    fn seeded(limits: Limits, bytes: Vec<u8>) -> Self {
        let disk = Self::new(limits);
        disk.state.lock().unwrap().block = Some(bytes);
        disk
    }
    fn bytes(&self) -> Vec<u8> {
        self.state.lock().unwrap().block.clone().unwrap()
    }
    fn count(&self) -> usize {
        self.state.lock().unwrap().replacements
    }
    fn fail_after(&self, further: usize, commit_new: bool) {
        let mut state = self.state.lock().unwrap();
        state.fail = Some((state.replacements + further, commit_new));
    }
    fn clear_failure(&self) {
        self.state.lock().unwrap().fail = None;
    }
    fn loaded(&self) -> DurableStore<Self> {
        DurableStore::open(Self::seeded(LIMITS, self.bytes()), LIMITS).unwrap()
    }
}
impl AtomicBlockStore for Disk {
    fn block_size(&self) -> usize {
        self.size
    }
    fn load(&mut self, block: &mut [u8]) -> Result<bool> {
        if block.len() != self.size {
            return Err(Error::Storage);
        }
        let state = self.state.lock().unwrap();
        if state.load_error {
            return Err(Error::Storage);
        }
        let Some(saved) = &state.block else {
            return Ok(false);
        };
        if saved.len() != block.len() {
            return Err(Error::Corrupt);
        }
        block.copy_from_slice(saved);
        Ok(true)
    }
    fn replace(&mut self, next: &[u8]) -> Result<()> {
        if next.len() != self.size {
            return Err(Error::Storage);
        }
        let mut state = self.state.lock().unwrap();
        let fail = state.fail.filter(|(at, _)| *at == state.replacements);
        state.replacements += 1;
        if fail.is_none_or(|(_, commit_new)| commit_new) {
            state.block = Some(next.to_vec());
            state.history.push(next.to_vec());
        }
        // Deliberately not Error::Storage: the adapter must normalize backend
        // errors so even a surprising error variant deactivates the live engine.
        if fail.is_some() {
            Err(Error::Invalid)
        } else {
            Ok(())
        }
    }
}

fn keys() -> [SigningKey; 4] {
    std::array::from_fn(|i| SigningKey::from([u8::try_from(i + 1).unwrap(); 32]))
}
fn config(weights: [u64; 4], crosslink: bool) -> Config {
    let members = keys()
        .iter()
        .zip(weights)
        .map(|(key, weight)| Member {
            public_key: VerificationKeyBytes::from(key).into(),
            weight,
        })
        .collect();
    let t = Timeouts {
        propose: 30,
        prevote: 5,
        precommit: 10,
        inc_propose: 3,
        inc_prevote: 1,
        inc_precommit: 2,
    };
    if crosslink {
        Config::crosslink([42; 32], members, LIMITS.max_body, 10, t)
    } else {
        Config::new([42; 32], members, LIMITS.max_body, 10, t)
    }
    .unwrap()
}

#[derive(Default)]
struct Out {
    disk: Option<Disk>,
    votes: Vec<Vote>,
    proposals: Vec<Header>,
    decisions: Vec<Header>,
    calls: usize,
}
impl Sink for Out {
    fn send(&mut self, _: Target, frame: Frame<'_>) {
        self.calls += 1;
        if let Some(disk) = &self.disk {
            let store = disk.loaded();
            let d = store.descriptor().unwrap();
            if let Some(v) = frame.outer {
                let index = match v.content.kind {
                    VoteKind::Enter => 0,
                    VoteKind::Prevote => 1,
                    VoteKind::Precommit => 2,
                };
                assert_eq!(
                    d.intents[index],
                    Some(v.content),
                    "output before durable intent"
                );
                assert_eq!(
                    store.own(v.content.kind),
                    Some(v),
                    "output before durable OWN"
                );
            }
            if let Some(p) = frame.proposal {
                assert_eq!(d.proposal_intent.unwrap().content, p.header.content);
                assert!(d.proposal_published);
                assert_eq!(store.own_header(), Some(p.header));
            }
        }
        if let Some(v) = frame.outer {
            self.votes.push(*v);
        }
        if let Some(p) = frame.proposal {
            self.proposals.push(*p.header);
        }
    }
    fn decided(&mut self, _: u64, proposal: Proposal<'_>, _: Group<'_>) {
        self.calls += 1;
        if let Some(disk) = &self.disk {
            let store = disk.loaded();
            let d = store.descriptor().unwrap();
            assert_eq!(d.step, Step::Decided);
            assert_eq!(store.body(d.pending.unwrap().proposal.index), proposal.body);
        }
        self.decisions.push(*proposal.header);
    }
    fn acquire(&mut self, _: ProposalTag) {
        self.calls += 1;
    }
    fn retry(&mut self, _: BodyTag, _: Proposal<'_>) {
        self.calls += 1;
    }
}
fn boot(c: &Config) -> (Engine<DurableStore<Disk>>, Disk, Out) {
    let disk = Disk::new(LIMITS);
    let store = DurableStore::open(disk.clone(), LIMITS).unwrap();
    let mut e = Engine::boot(store, keys()[0].clone(), c.clone()).unwrap();
    let mut out = Out {
        disk: Some(disk.clone()),
        ..Out::default()
    };
    e.init(c, 4, 0, &mut out).unwrap();
    (e, disk, out)
}
fn supply(e: &mut Engine<DurableStore<Disk>>, body: &[u8], out: &mut Out) -> Result<()> {
    e.supply_proposal(e.proposal_tag().unwrap(), body, Status::Valid, out)
}
fn peer_vote(c: &Config, kind: VoteKind, id: Id) -> Vote {
    Vote::sign(
        c,
        &keys()[1],
        VoteContent {
            height: 4,
            round: 0,
            kind,
            id: Some(id),
            signer: 1,
        },
    )
}
fn receive(e: &mut Engine<DurableStore<Disk>>, v: &Vote, out: &mut Out) {
    e.receive(
        Frame {
            message: Message::Ordinary,
            outer: Some(v),
            proposal: None,
            groups: &[],
        },
        Status::Valid,
        out,
    )
    .unwrap();
}
fn recover(disk: Disk, out: &mut Out) -> Engine<DurableStore<Disk>> {
    Engine::recover_with_emission(
        DurableStore::open(disk, LIMITS).unwrap(),
        keys()[0].clone(),
        Emission::Guided,
        out,
    )
    .unwrap()
}

#[test]
fn durable_live_lock_roundtrip_and_decision_replay_in_both_signing_modes() {
    for crosslink in [false, true] {
        let c = config([4, 3, 2, 1], crosslink);
        let (mut e, disk, mut out) = boot(&c);
        supply(&mut e, b"locked body", &mut out).unwrap();
        let id = c.body_id(b"locked body");
        receive(&mut e, &peer_vote(&c, VoteKind::Prevote, id), &mut out);
        let before = e.view().unwrap().clone();
        assert_eq!(before.step, Step::Precommit);
        assert_eq!(before.locked_id, Some(id));
        assert!(before.vc.is_some());
        let own = *e.storage().own(VoteKind::Precommit).unwrap();
        let backend = e.into_storage().into_backend();
        let mut e = recover(backend, &mut out);
        assert_eq!(e.storage().config().unwrap(), &c);
        assert_eq!(e.view(), Some(&before));
        assert_eq!(e.storage().own(VoteKind::Precommit), Some(&own));
        receive(&mut e, &peer_vote(&c, VoteKind::Precommit, id), &mut out);
        assert_eq!(e.view().unwrap().step, Step::Decided);
        let decided = e.view().unwrap().clone();
        let n = out.decisions.len();
        let e = recover(e.into_storage().into_backend(), &mut out);
        assert_eq!(e.view(), Some(&decided));
        assert_eq!(out.decisions.len(), n + 1);
        assert!(
            disk.state
                .lock()
                .unwrap()
                .history
                .iter()
                .all(|bytes| bytes.len() == image_size(LIMITS).unwrap())
        );
    }
}

#[test]
fn every_acknowledged_write_can_be_reopened_even_during_staging() {
    let c = config([7, 1, 1, 1], false);
    let (mut e, disk, mut out) = boot(&c);
    supply(&mut e, b"original", &mut out).unwrap();
    let next = config([1, 7, 1, 1], true);
    e.init(&next, 5, 5, &mut out).unwrap();
    let history = disk.state.lock().unwrap().history.clone();
    assert!(history.len() > 25);
    let mut saw_old = false;
    let mut saw_new = false;
    for image in history {
        let store = DurableStore::open(Disk::seeded(LIMITS, image), LIMITS).unwrap();
        let mut replay = Out::default();
        if store.descriptor().is_none() {
            // Boot is legal before the first DESC, even with a partially staged
            // unsealed bank. It must never bypass an existing descriptor.
            let e = Engine::boot(store, keys()[0].clone(), c.clone()).unwrap();
            assert!(e.view().is_none());
            continue;
        }
        let e =
            Engine::recover_with_emission(store, keys()[0].clone(), Emission::Guided, &mut replay)
                .unwrap();
        match e.view().unwrap().height {
            4 => {
                saw_old = true;
                assert_eq!(e.storage().config(), Some(&c));
            }
            5 => {
                saw_new = true;
                assert_eq!(e.storage().config(), Some(&next));
            }
            h => panic!("mixed height authority {h}"),
        }
    }
    assert!(saw_old && saw_new);
}

#[test]
fn every_failed_replace_old_or_new_poison_and_no_double_signing_after_reload() {
    let c = config([7, 1, 1, 1], false);
    let (mut baseline, disk, mut out) = boot(&c);
    let original = disk.bytes();
    let before = disk.count();
    supply(&mut baseline, b"first", &mut out).unwrap();
    let writes = disk.count() - before;
    assert!(writes > 10);
    for commit_new in [false, true] {
        for cut in 0..writes {
            let disk = Disk::seeded(LIMITS, original.clone());
            let mut out = Out {
                disk: Some(disk.clone()),
                ..Out::default()
            };
            let mut e = recover(disk.clone(), &mut out);
            let votes_before = out.votes.len();
            let headers_before = out.proposals.len();
            disk.fail_after(cut, commit_new);
            assert_eq!(
                supply(&mut e, b"first", &mut out),
                Err(Error::Storage),
                "cut {cut}"
            );
            assert!(!e.is_active());
            assert!(e.storage().is_poisoned());
            let calls = out.calls;
            assert_eq!(e.elapse(10, &mut out), Err(Error::Inactive));
            assert_eq!(out.calls, calls, "no callback after failed replacement");
            let old_votes = out.votes[votes_before..].to_vec();
            let old_headers = out.proposals[headers_before..].to_vec();
            disk.clear_failure();
            let mut e = recover(e.into_storage().into_backend(), &mut out);
            let committed = e.view().unwrap().proposal_intent.map(|i| i.content.id);
            if e.proposal_tag().is_some() {
                supply(&mut e, b"second", &mut out).unwrap();
            }
            assert_eq!(e.view().unwrap().step, Step::Decided);
            let p = e.view().unwrap().pending.unwrap().proposal;
            let final_id = e.storage().header(p.index).unwrap().content.id;
            assert_eq!(final_id, committed.unwrap_or(c.body_id(b"second")));
            for v in old_votes {
                for after in &out.votes {
                    if after.content.height == v.content.height
                        && after.content.round == v.content.round
                        && after.content.kind == v.content.kind
                    {
                        assert_eq!(after.content, v.content, "double vote at cut {cut}");
                    }
                }
            }
            for header in old_headers {
                for after in &out.proposals {
                    if after.content.height == header.content.height
                        && after.content.round == header.content.round
                    {
                        assert_eq!(
                            after.content, header.content,
                            "double proposal at cut {cut}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn poisoned_store_cannot_acknowledge_even_an_already_sealed_config() {
    let c = config([1; 4], false);
    let (e, disk, _) = boot(&c);
    let mut store = e.into_storage();
    disk.fail_after(0, true);
    assert_eq!(store.write_body(0, b"staging"), Err(Error::Storage));
    let count = disk.count();
    assert_eq!(store.seal_config(&c), Err(Error::Storage));
    assert_eq!(store.write_body(0, b"retry"), Err(Error::Storage));
    assert_eq!(disk.count(), count);
}

// This helper deliberately knows the v2 root location: resealing a v1-style
// whole-image tail would only test checksum rejection, not v2 structure.
fn reseal(bytes: &mut [u8]) {
    // LIMITS and this offset are independent of potentially forged prefix data.
    let end = bytes.len() - 3 * LIMITS.max_body;
    let at = end - 32;
    let digest = blake3::hash(&bytes[..at]);
    bytes[at..end].copy_from_slice(digest.as_bytes());
}
fn assert_corrupt(bytes: Vec<u8>) {
    assert!(matches!(
        DurableStore::open(Disk::seeded(LIMITS, bytes), LIMITS),
        Err(Error::Corrupt)
    ));
}
#[test]
fn corrupt_truncated_oversized_and_forged_lengths_never_become_empty_storage() {
    let c = config([1; 4], false);
    let (_, disk, _) = boot(&c);
    let valid = disk.bytes();
    for at in [0, 8, 12, 20, 28, 36, 100, valid.len() - 1] {
        let mut corrupt = valid.clone();
        corrupt[at] ^= 1;
        assert_corrupt(corrupt);
    }
    for size in [0, 1, valid.len() / 2, valid.len() - 1] {
        assert_corrupt(valid[..size].to_vec());
    }
    let mut too_long = valid.clone();
    too_long.push(0);
    assert_corrupt(too_long);
    // Mutations with a recomputed checksum exercise the structural decoder,
    // not just accidental-corruption detection. CONFIG starts after its two
    // presence/sealed bytes. Its fixed metadata places N at offset 175.
    for offset in [12, 20, 28, 78, 175] {
        let mut corrupt = valid.clone();
        corrupt[offset..offset + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        reseal(&mut corrupt);
        assert_corrupt(corrupt);
    }
    for offset in [0, 8, 36, 37, 142] {
        let mut corrupt = valid.clone();
        corrupt[offset] = 0xff;
        reseal(&mut corrupt);
        assert_corrupt(corrupt);
    }
    let payload = u64::from_le_bytes(valid[28..36].try_into().unwrap()) as usize;
    let mut padding = valid.clone();
    padding[36 + payload] = 1;
    reseal(&mut padding);
    assert_corrupt(padding);
    let mut short_payload = valid.clone();
    short_payload[28..36].copy_from_slice(&1u64.to_le_bytes());
    reseal(&mut short_payload);
    assert_corrupt(short_payload);
    let failed = Disk::new(LIMITS);
    failed.state.lock().unwrap().load_error = true;
    assert!(matches!(
        DurableStore::open(failed, LIMITS),
        Err(Error::Storage)
    ));
}

#[test]
fn immutable_limits_reject_growth_without_writing_and_invalid_indices_do_not_panic() {
    let c = config([1; 4], false);
    let (e, disk, _) = boot(&c);
    let mut store = e.into_storage();
    let count = disk.count();
    let mut members = c.members().to_vec();
    members.push(Member {
        public_key: VerificationKeyBytes::from(&SigningKey::from([99; 32])).into(),
        weight: 1,
    });
    let larger = Config::new(c.domain(), members, c.max_body(), c.rho(), c.timeouts()).unwrap();
    assert_eq!(store.seal_config(&larger), Err(Error::Config));
    assert_eq!(store.write_body(0, &[0; 129]), Err(Error::Invalid));
    assert_eq!(store.write_body(3, b""), Err(Error::Corrupt));
    assert_eq!(store.copy_body(0, 0), Err(Error::Corrupt));
    assert_eq!(store.write_region_row(4, 0, None), Err(Error::Corrupt));
    assert_eq!(store.write_region_row(0, 4, None), Err(Error::Corrupt));
    assert_eq!(
        store.write_config_member(2, 0, c.members()[0].clone()),
        Err(Error::Corrupt)
    );
    assert_eq!(disk.count(), count);
    assert!(!store.is_poisoned());
    assert_eq!(
        image_size(Limits {
            max_members: 0,
            ..LIMITS
        }),
        Err(Error::Config)
    );
    assert_eq!(
        image_size(Limits {
            max_body: usize::MAX,
            ..LIMITS
        }),
        Err(Error::Config)
    );
    assert!(matches!(
        DurableStore::open(
            disk,
            Limits {
                max_members: 5,
                ..LIMITS
            }
        ),
        Err(Error::Config)
    ));
}

#[test]
fn image_size_stays_constant_through_rounds_and_roster_shrink() {
    let c = config([7, 1, 1, 1], false);
    let (mut e, disk, mut out) = boot(&c);
    e.elapse(1000, &mut out).unwrap();
    assert!(e.view().unwrap().round > 0);
    let next = Config::new(
        c.domain(),
        c.members()[..2].to_vec(),
        64,
        c.rho(),
        c.timeouts(),
    )
    .unwrap();
    e.init(&next, 5, 3, &mut out).unwrap();
    let e = recover(e.into_storage().into_backend(), &mut out);
    assert_eq!(e.storage().config(), Some(&next));
    assert_eq!(e.storage().limits(), LIMITS);
    assert!(
        disk.state
            .lock()
            .unwrap()
            .history
            .iter()
            .all(|bytes| bytes.len() == image_size(LIMITS).unwrap())
    );
}

#[test]
fn full_sized_physical_roles_fit_and_reopen_without_any_height_history() {
    let c = config([1; 4], false);
    let (e, disk, _) = boot(&c);
    let mut store = e.into_storage();
    let next = config([4, 3, 2, 1], true);
    store.seal_config(&next).unwrap();
    let body = [27; LIMITS.max_body];
    let header = Header::sign(
        &c,
        &keys()[1],
        ProposalContent {
            height: 4,
            round: 1,
            valid_round: Some(0),
            id: c.body_id(&body),
            signer: 1,
        },
    );
    // Exercise maximum physical occupancy independently of protocol reachability:
    // these are staged/unreferenced records, not a fabricated consensus state.
    for slot in 0..3 {
        store.write_body(slot, &body).unwrap();
        store.write_header(slot, header).unwrap();
    }
    for region in 0..4 {
        for signer in 0..LIMITS.max_members {
            let vote = Vote::sign(
                &c,
                &keys()[signer],
                VoteContent {
                    height: 4,
                    round: 0,
                    kind: VoteKind::Prevote,
                    id: Some(c.body_id(&body)),
                    signer: u32::try_from(signer).unwrap(),
                },
            );
            store.write_region_row(region, signer, Some(vote)).unwrap();
        }
        store
            .write_region_header(
                region,
                RegionHeader {
                    kind: CertKind::Polka {
                        round: 0,
                        id: c.body_id(&body),
                    },
                    count: LIMITS.max_members,
                    digest: [9; 32],
                },
            )
            .unwrap();
    }
    let reopened = DurableStore::open(store.into_backend(), LIMITS).unwrap();
    assert_eq!(reopened.config(), Some(&c));
    assert!((0..2).any(|bank| reopened.config_bank_hash(bank) == Some(next.hash())));
    for slot in 0..3 {
        assert_eq!(reopened.body(slot), body);
        assert_eq!(reopened.header(slot), Some(&header));
    }
    for region in 0..4 {
        assert_eq!(reopened.region_rows(region).len(), LIMITS.max_members);
        assert!(reopened.region_rows(region).iter().all(Option::is_some));
    }
    assert_eq!(disk.bytes().len(), image_size(LIMITS).unwrap());
}

#[test]
fn corrupt_body_and_region_lengths_are_rejected_before_vector_allocation() {
    let c = config([1; 4], false);
    let disk = Disk::new(LIMITS);
    let mut store = DurableStore::open(disk.clone(), LIMITS).unwrap();
    store
        .begin_config(
            0,
            ConfigMeta {
                domain: c.domain(),
                total: c.total_weight(),
                count: c.members().len(),
                max_body: c.max_body(),
                rho: c.rho(),
                timeouts: c.timeouts(),
                crosslink: false,
            },
        )
        .unwrap();
    // v2: prefix + bank flags + CONFIG scalar bytes/member rows + empty bank
    // flags + absent DESC. Each body metadata takes length + digest + absent-header.
    assert_eq!(&disk.bytes()[..8], b"D28IMG02");
    let first_body = 36 + 2 + 145 + 40 * LIMITS.max_members + 2 + 1;
    let first_region = first_body + 3 * (8 + 32 + 1);
    for offset in [first_body, first_region] {
        let mut bytes = disk.bytes();
        assert_eq!(&bytes[offset..offset + 8], &[0; 8]);
        bytes[offset..offset + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        reseal(&mut bytes);
        assert_corrupt(bytes);
    }
}

#[test]
fn failed_configuration_publication_keeps_exactly_old_or_new_signing_authority() {
    let old = config([7, 1, 1, 1], false);
    let next = config([1, 7, 1, 1], true);
    let (mut baseline, disk, mut out) = boot(&old);
    supply(&mut baseline, b"old decided body", &mut out).unwrap();
    let original = disk.bytes();
    let before = disk.count();
    baseline.init(&next, 5, 0, &mut out).unwrap();
    let writes = disk.count() - before;
    for commit_new in [false, true] {
        for cut in 0..writes {
            let disk = Disk::seeded(LIMITS, original.clone());
            let mut out = Out {
                disk: Some(disk.clone()),
                ..Out::default()
            };
            let mut e = recover(disk.clone(), &mut out);
            disk.fail_after(cut, commit_new);
            assert_eq!(e.init(&next, 5, 0, &mut out), Err(Error::Storage));
            assert!(!e.is_active());
            let calls = out.calls;
            assert_eq!(e.elapse(1, &mut out), Err(Error::Inactive));
            assert_eq!(out.calls, calls);
            disk.clear_failure();
            let mut e = recover(e.into_storage().into_backend(), &mut out);
            match e.view().unwrap().height {
                4 => {
                    assert_eq!(e.storage().config(), Some(&old));
                    assert_eq!(e.view().unwrap().step, Step::Decided);
                    e.init(&next, 5, 0, &mut out).unwrap();
                }
                5 => {
                    assert_eq!(e.storage().config(), Some(&next));
                }
                h => panic!("mixed authority at height {h}"),
            }
            let before = e.view().unwrap().clone();
            assert!(!e.init(&old, 4, 0, &mut out).unwrap());
            assert_eq!(e.view(), Some(&before));
            assert_eq!(e.storage().config(), Some(&next));
        }
    }
}

#[test]
fn v2_checks_actual_unreferenced_bodies_digests_and_all_physical_padding() {
    let c = config([1; 4], false);
    let disk = Disk::new(LIMITS);
    let mut store = DurableStore::open(disk.clone(), LIMITS).unwrap();
    store.seal_config(&c).unwrap();
    store.write_body(0, b"ABC").unwrap();
    // No DESC references any body; physical bytes must still be authenticated.
    assert!(store.descriptor().is_none());
    let valid = disk.bytes();
    assert_eq!(&valid[..8], b"D28IMG02");
    let head = valid.len() - 3 * LIMITS.max_body;
    let first_body = 36 + 2 + 145 + 40 * LIMITS.max_members + 2 + 1;
    assert_eq!(&valid[first_body..first_body + 8], &3u64.to_le_bytes());
    for offset in [
        head,
        head + 3,
        head + LIMITS.max_body,
        valid.len() - 1,
        first_body + 8,
        head - 32,
    ] {
        let mut corrupt = valid.clone();
        corrupt[offset] ^= 1;
        if offset != head - 32 {
            reseal(&mut corrupt);
        }
        assert_corrupt(corrupt);
    }
    // A forged, root-checksummed commitment does not stand in for actual bytes.
    let mut forged = valid.clone();
    forged[first_body + 8..first_body + 40].copy_from_slice(blake3::hash(b"XYZ").as_bytes());
    reseal(&mut forged);
    assert_corrupt(forged);
    // Even a correct shortened digest cannot hide nonzero trailing old bytes.
    let mut shortened = valid.clone();
    shortened[first_body..first_body + 8].copy_from_slice(&1u64.to_le_bytes());
    shortened[first_body + 8..first_body + 40].copy_from_slice(blake3::hash(b"A").as_bytes());
    reseal(&mut shortened);
    assert_corrupt(shortened);
    let mut longer = valid.clone();
    longer[first_body..first_body + 8].copy_from_slice(&4u64.to_le_bytes());
    reseal(&mut longer);
    assert_corrupt(longer);
    // Genuine byte+digest agreement is a valid different unreferenced body;
    // checksums are corruption detection, not secret-key authentication.
    let mut replaced = valid;
    replaced[head..head + 3].copy_from_slice(b"XYZ");
    replaced[first_body + 8..first_body + 40].copy_from_slice(blake3::hash(b"XYZ").as_bytes());
    reseal(&mut replaced);
    let reopened = DurableStore::open(Disk::seeded(LIMITS, replaced), LIMITS).unwrap();
    assert_eq!(reopened.body(0), b"XYZ");
}
