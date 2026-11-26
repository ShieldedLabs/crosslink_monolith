//! The running adapter, not just the typed core: bounded host work, persisted
//! authority/restart, and real localhost shared STP with receiver-selected frames.
use ed25519_zebra::{SigningKey, VerificationKeyBytes};
use std::{future::pending, net::{Ipv6Addr, UdpSocket}, sync::{Arc, Mutex, atomic::{AtomicU64, AtomicUsize, Ordering}}, time::Duration};
use tenderlink::{BlockValue, ClosureToProposeNewBlock, ClosureToPushDecidedBlock,
    ClosureToReadDecidedTip, ClosureToUpdatePeers, ClosureToValidateProposedBlock,
    HostTip, SortedRosterMember, STPAddress, TMStatus, TMStatusReason, ValueId,
    CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2b, stp::new_keypair_from_connect_magic1,
    transport::SharedTransport,
    d28::{self, AtomicBlockStore, Config, Emission, Engine, Frame, Group, Member,
        Proposal, Sink, Step, Target, Timeouts, durable::{DurableStore, Limits},
        runtime::{self, AccessD28, HostCallbacks, RuntimeConfig}},
};
use zcash_primitives::bft::{BftBlock, FatPointerToBftBlock, PubKeyID};

#[derive(Clone)]
struct Disk { size: usize, image: Arc<Mutex<Option<Vec<u8>>>>, writes: Arc<AtomicUsize> }
impl Disk {
    fn new(config: &RuntimeConfig) -> Self {
        Self { size: d28::durable::image_size(Limits { max_members: config.max_members, max_body: config.max_body }).unwrap(),
            image: Arc::new(Mutex::new(None)), writes: Arc::new(AtomicUsize::new(0)) }
    }
}
impl AtomicBlockStore for Disk {
    fn block_size(&self) -> usize { self.size }
    fn load(&mut self, bytes: &mut [u8]) -> d28::Result<bool> {
        if bytes.len() != self.size { return Err(d28::Error::Storage); }
        if let Some(image) = &*self.image.lock().unwrap() { bytes.copy_from_slice(image); Ok(true) } else { Ok(false) }
    }
    fn replace(&mut self, bytes: &[u8]) -> d28::Result<()> {
        if bytes.len() != self.size { return Err(d28::Error::Storage); }
        *self.image.lock().unwrap() = Some(bytes.to_vec());
        self.writes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}
fn key(i: usize) -> SigningKey { SigningKey::from([i as u8 + 20; 32]) }
fn roster(n: usize) -> Vec<SortedRosterMember> {
    (0..n).map(|i| SortedRosterMember { pub_key: PubKeyID(VerificationKeyBytes::from(&key(i)).into()),
        stake: 1, cumulative_stake: i as u64 + 1 }).collect()
}
fn node() -> (SharedTransport, STPAddress) {
    let identity = new_keypair_from_connect_magic1(CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2b).unwrap();
    let socket = UdpSocket::bind((Ipv6Addr::LOCALHOST, 0)).unwrap();
    let port = socket.local_addr().unwrap().port();
    drop(socket);
    let address = STPAddress::from(Ipv6Addr::LOCALHOST, port, &identity);
    (SharedTransport::new(vec![identity], port, None, (1_000_000, 512 * 1024, 8 << 20)), address)
}
fn config(transport: SharedTransport, roster: Vec<SortedRosterMember>, seeds: Vec<STPAddress>) -> RuntimeConfig {
    let mut config = RuntimeConfig::new(1, roster, [11; 32], 4, transport, seeds);
    config.max_body = 4096;
    config.rho = 60;
    config.timeouts = Timeouts { propose: 800, prevote: 300, precommit: 500,
        inc_propose: 90, inc_prevote: 30, inc_precommit: 60 };
    config.poll_interval = Duration::from_millis(10);
    config.status_interval = Duration::from_millis(30);
    config.request_timeout = 300;
    config.status_ttl = 2_000;
    config
}
fn core_config(config: &RuntimeConfig) -> Config {
    Config::crosslink(config.vote_namespace, config.initial_roster.iter().map(|m| Member {
        public_key: m.pub_key.0, weight: m.stake,
    }).collect(), config.max_body, config.rho, config.timeouts).unwrap()
}
fn value(height: u32) -> BlockValue {
    BlockValue(BftBlock { version: 1, height, previous_block_fat_ptr: FatPointerToBftBlock::null(),
        headers: vec![], slashforks: vec![], do_not_include_until_bc_height: 0 }.zcash_serialize_to_vec().unwrap())
}
fn host(roster: Vec<SortedRosterMember>, pushed: Arc<AtomicUsize>) -> HostCallbacks {
    let tip = HostTip { height: 0, block_id: ValueId::NIL, next_roster: roster,
        next_vote_namespace: [11; 32], next_strict_quorum: true };
    HostCallbacks {
        propose: ClosureToProposeNewBlock(Arc::new(|| Box::pin(async { Some(value(1)) }))),
        validate: ClosureToValidateProposedBlock(Arc::new(|_| Box::pin(async { (TMStatus::Pass, TMStatusReason::None) }))),
        push_decided: ClosureToPushDecidedBlock(Arc::new(move |_, pointer, chunks| {
            assert_eq!(pointer.get_vote_template().height, 1);
            assert!(chunks.is_empty(), "D28 exports collected PC signatures, no legacy proposal-signing phase");
            pushed.fetch_add(1, Ordering::SeqCst);
            Box::pin(pending())
        })),
        read_tip: ClosureToReadDecidedTip(Arc::new(move |_| Some(tip.clone()))),
        update_peers: ClosureToUpdatePeers(Arc::new(|_| Box::pin(async {}))),
        access: AccessD28::default(),
    }
}
fn executor() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap()
}
async fn until(mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(12), async {
        while !ready() { tokio::time::sleep(Duration::from_millis(10)).await; }
    }).await.expect("runtime made no progress");
}

#[test]
fn pending_acquisition_does_not_stop_online_timers_or_access() {
    executor().block_on(async {
        let (transport, _) = node();
        let mut config = config(transport, roster(1), vec![]);
        config.timeouts = Timeouts { propose: 30, prevote: 20, precommit: 30,
            inc_propose: 9, inc_prevote: 3, inc_precommit: 6 };
        let disk = Disk::new(&config);
        let seen_round = Arc::new(AtomicU64::new(0));
        let accesses = Arc::new(AtomicUsize::new(0));
        let mut callbacks = host(roster(1), Arc::new(AtomicUsize::new(0)));
        callbacks.propose = ClosureToProposeNewBlock(Arc::new(|| Box::pin(pending())));
        let (round, calls) = (seen_round.clone(), accesses.clone());
        callbacks.access = AccessD28(Arc::new(move |snapshot, _| {
            round.fetch_max(snapshot.round, Ordering::SeqCst);
            calls.fetch_add(1, Ordering::SeqCst);
        }));
        let task = tokio::spawn(runtime::run(key(0), disk, config, callbacks));
        until(|| seen_round.load(Ordering::SeqCst) >= 1).await;
        assert!(accesses.load(Ordering::SeqCst) >= 3);
        task.abort(); let _ = task.await;
    });
}

#[test]
fn unavailable_proposal_retries_and_decision_is_redelivered_after_restart() {
    executor().block_on(async {
        let (transport, _) = node();
        let config1 = config(transport.clone(), roster(1), vec![]);
        let disk = Disk::new(&config1);
        let pushes = Arc::new(AtomicUsize::new(0));
        let tries = Arc::new(AtomicUsize::new(0));
        let mut callbacks = host(roster(1), pushes.clone());
        let attempts = tries.clone();
        callbacks.propose = ClosureToProposeNewBlock(Arc::new(move || {
            let attempt = attempts.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { (attempt > 0).then(|| value(1)) })
        }));
        let task = tokio::spawn(runtime::run(key(0), disk.clone(), config1, callbacks));
        until(|| pushes.load(Ordering::SeqCst) == 1).await;
        assert!(tries.load(Ordering::SeqCst) >= 2);
        task.abort(); let _ = task.await;
        let before = disk.writes.load(Ordering::SeqCst);
        let task = tokio::spawn(runtime::run(key(0), disk.clone(), config(transport, roster(1), vec![]), host(roster(1), pushes.clone())));
        until(|| pushes.load(Ordering::SeqCst) == 2).await;
        assert!(disk.writes.load(Ordering::SeqCst) >= before);
        task.abort(); let _ = task.await;
    });
}

struct Quiet;
impl Sink for Quiet {
    fn send(&mut self, _: Target, _: Frame<'_>) {}
    fn decided(&mut self, _: u64, _: Proposal<'_>, _: Group<'_>) {}
}
#[test]
fn durable_height_ahead_of_host_does_not_recover_or_write_until_caught_up() {
    executor().block_on(async {
        let (transport, _) = node();
        let config = config(transport, roster(1), vec![]);
        let disk = Disk::new(&config);
        let c = core_config(&config);
        let storage = DurableStore::open(disk.clone(), Limits { max_members: config.max_members, max_body: config.max_body }).unwrap();
        let mut engine = Engine::boot(storage, key(0), c.clone()).unwrap();
        engine.set_emission(Emission::Guided);
        engine.init(&c, 5, 0, &mut Quiet).unwrap();
        engine.elapse(1, &mut Quiet).unwrap();
        drop(engine);
        let before = disk.writes.load(Ordering::SeqCst);
        let behind = Arc::new(AtomicUsize::new(0));
        let tip_height = Arc::new(AtomicU64::new(0));
        let mut callbacks = host(roster(1), Arc::new(AtomicUsize::new(0)));
        let observed = behind.clone();
        callbacks.access = AccessD28(Arc::new(move |view, _| {
            if view.catching_up { assert_eq!(view.height, 5); observed.fetch_add(1, Ordering::SeqCst); }
        }));
        let height = tip_height.clone();
        callbacks.read_tip = ClosureToReadDecidedTip(Arc::new(move |_| Some(HostTip {
            height: height.load(Ordering::SeqCst), block_id: ValueId::NIL,
            next_roster: roster(1), next_vote_namespace: [11;32], next_strict_quorum: true,
        })));
        callbacks.propose = ClosureToProposeNewBlock(Arc::new(|| Box::pin(pending())));
        let task = tokio::spawn(runtime::run(key(0), disk.clone(), config, callbacks));
        until(|| behind.load(Ordering::SeqCst) >= 3).await;
        assert_eq!(disk.writes.load(Ordering::SeqCst), before, "recovery must not materialize/sign while host is behind");
        tip_height.store(4, Ordering::SeqCst);
        until(|| disk.writes.load(Ordering::SeqCst) > before).await;
        task.abort(); let _ = task.await;
    });
}

#[test]
fn restored_configuration_mismatch_refuses_before_any_write() {
    executor().block_on(async {
        let (transport, _) = node();
        let mut config = config(transport, roster(1), vec![]);
        let disk = Disk::new(&config);
        let c = core_config(&config);
        let storage = DurableStore::open(disk.clone(), Limits { max_members: config.max_members, max_body: config.max_body }).unwrap();
        let mut engine = Engine::boot(storage, key(0), c.clone()).unwrap();
        engine.init(&c, 1, 0, &mut Quiet).unwrap();
        drop(engine);
        let before = disk.writes.load(Ordering::SeqCst);
        config.vote_namespace = [99; 32];
        let mut callbacks = host(roster(1), Arc::new(AtomicUsize::new(0)));
        callbacks.read_tip = ClosureToReadDecidedTip(Arc::new(|_| None));
        let error = runtime::run(key(0), disk.clone(), config, callbacks).await.unwrap_err();
        assert!(error.to_string().contains("restored D28 configuration"));
        assert_eq!(disk.writes.load(Ordering::SeqCst), before);
    });
}

#[test]
fn four_real_localhost_finalizers_decide_while_host_pushes_are_pending() {
    executor().block_on(async {
        let nodes: Vec<_> = (0..4).map(|_| node()).collect();
        let seeds: Vec<_> = nodes.iter().map(|(_,address)| address.clone()).collect();
        let pushes: Vec<_> = (0..4).map(|_| Arc::new(AtomicUsize::new(0))).collect();
        let mut tasks = Vec::new();
        let decided_ticks = Arc::new(AtomicUsize::new(0));
        for (i, (transport, _)) in nodes.into_iter().enumerate() {
            let config = config(transport, roster(4), seeds.iter().enumerate().filter(|(j,_)| *j != i).map(|(_,a)| a.clone()).collect());
            let disk = Disk::new(&config);
            let mut callbacks = host(roster(4), pushes[i].clone());
            // Real asynchronous validation must not stall peer/status service.
            callbacks.validate = ClosureToValidateProposedBlock(Arc::new(|_| Box::pin(async {
                tokio::time::sleep(Duration::from_millis(45)).await;
                (TMStatus::Pass, TMStatusReason::None)
            })));
            let ticks = decided_ticks.clone();
            callbacks.access = AccessD28(Arc::new(move |snapshot, _| {
                if snapshot.step == Step::Decided { ticks.fetch_add(1, Ordering::SeqCst); }
            }));
            tasks.push(tokio::spawn(runtime::run(key(i), disk, config, callbacks)));
        }
        until(|| pushes.iter().all(|n| n.load(Ordering::SeqCst) == 1)).await;
        let before = decided_ticks.load(Ordering::SeqCst);
        until(|| decided_ticks.load(Ordering::SeqCst) >= before + 8).await;
        for task in tasks { task.abort(); let _ = task.await; }
    });
}

#[test]
fn unrepresentable_chain_height_is_refused_without_signing() {
    executor().block_on(async {
        for initial in [true, false] {
            let (transport, _) = node();
            let mut config = config(transport, roster(1), vec![]);
            if initial { config.initial_height = u32::MAX as u64 + 1; }
            let disk = Disk::new(&config);
            let mut callbacks = host(roster(1), Arc::new(AtomicUsize::new(0)));
            if !initial {
                callbacks.read_tip = ClosureToReadDecidedTip(Arc::new(|_| Some(HostTip {
                    height: u32::MAX as u64, block_id: ValueId::NIL,
                    next_roster: roster(1), next_vote_namespace: [11; 32], next_strict_quorum: true,
                })));
            }
            let error = runtime::run(key(0), disk.clone(), config, callbacks).await.unwrap_err();
            assert!(error.to_string().contains("chain u32 domain"));
            assert_eq!(disk.writes.load(Ordering::SeqCst), 0);
        }
    });
}

#[test]
fn acquired_body_must_be_canonical_and_match_requested_height_before_validation() {
    executor().block_on(async {
        let (transport, _) = node();
        let config = config(transport, roster(1), vec![]);
        let disk = Disk::new(&config);
        let attempts = Arc::new(AtomicUsize::new(0));
        let validated = Arc::new(AtomicUsize::new(0));
        let pushed = Arc::new(AtomicUsize::new(0));
        let mut callbacks = host(roster(1), pushed.clone());
        let calls = attempts.clone();
        callbacks.propose = ClosureToProposeNewBlock(Arc::new(move || {
            let attempt = calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move { Some(match attempt {
                0 => value(2),
                1 => { let mut bytes = value(1); bytes.0.push(0); bytes },
                2 => BlockValue(vec![0; 4]),
                _ => value(1),
            }) })
        }));
        let calls = validated.clone();
        callbacks.validate = ClosureToValidateProposedBlock(Arc::new(move |body| {
            assert_eq!(body.0, value(1).0);
            calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { (TMStatus::Pass, TMStatusReason::None) })
        }));
        let task = tokio::spawn(runtime::run(key(0), disk, config, callbacks));
        until(|| pushed.load(Ordering::SeqCst) == 1).await;
        assert_eq!(validated.load(Ordering::SeqCst), 1);
        assert!(attempts.load(Ordering::SeqCst) >= 4);
        task.abort(); let _ = task.await;
    });
}

#[test]
fn empty_roster_observer_does_not_initialize_signing_state() {
    executor().block_on(async {
        let (transport, _) = node();
        let config = config(transport, vec![], vec![]);
        let disk = Disk::new(&config);
        let callbacks = host(vec![], Arc::new(AtomicUsize::new(0)));
        let task = tokio::spawn(runtime::run(key(0), disk.clone(), config, callbacks));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!task.is_finished());
        assert_eq!(disk.writes.load(Ordering::SeqCst), 0);
        task.abort(); let _ = task.await;
    });
}
