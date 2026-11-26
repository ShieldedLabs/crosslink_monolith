//! The running adapter, not just the typed core: bounded host work, persisted
//! authority/restart, and real localhost shared STP with receiver-selected frames.
use ed25519_zebra::{SigningKey, VerificationKeyBytes};
use std::{
    collections::VecDeque,
    future::pending,
    net::{Ipv6Addr, UdpSocket},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tenderlink::{
    BlockValue, CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2b, ClosureToProposeNewBlock,
    ClosureToPushDecidedBlock, ClosureToReadDecidedTip, ClosureToUpdatePeers,
    ClosureToValidateProposedBlock, HostTip, STPAddress, SortedRosterMember, TMStatus,
    TMStatusReason, ValueId,
    d28::{
        self, AtomicBlockStore, Config, Emission, Engine, Frame, Group, Member, Proposal, Sink,
        Step, Target, Timeouts,
        durable::{DurableStore, Limits},
        runtime::{self, AccessD28, HostCallbacks, RuntimeConfig},
    },
    stp::new_keypair_from_connect_magic1,
    transport::{Consumer, NetworkThreadPush, SharedTransport, TransportStats},
};
use zcash_primitives::bft::{BftBlock, FatPointerToBftBlock, PubKeyID};

#[derive(Clone)]
struct Disk {
    size: usize,
    image: Arc<Mutex<Option<Vec<u8>>>>,
    writes: Arc<AtomicUsize>,
    deadline: Option<Instant>,
}
impl Disk {
    fn new(config: &RuntimeConfig) -> Self {
        Self {
            size: d28::durable::image_size(Limits {
                max_members: config.max_members,
                max_body: config.max_body,
            })
            .unwrap(),
            image: Arc::new(Mutex::new(None)),
            writes: Arc::new(AtomicUsize::new(0)),
            deadline: None,
        }
    }
}
impl AtomicBlockStore for Disk {
    fn block_size(&self) -> usize {
        self.size
    }
    fn load(&mut self, bytes: &mut [u8]) -> d28::Result<bool> {
        if bytes.len() != self.size {
            return Err(d28::Error::Storage);
        }
        if let Some(image) = &*self.image.lock().unwrap() {
            bytes.copy_from_slice(image);
            Ok(true)
        } else {
            Ok(false)
        }
    }
    fn replace(&mut self, bytes: &[u8]) -> d28::Result<()> {
        // Tokio cancellation cannot interrupt a synchronous catch-up loop.
        // Only bounded liveness fixtures set this; expiry must fail the test.
        if self
            .deadline
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            return Err(d28::Error::Storage);
        }
        if bytes.len() != self.size {
            return Err(d28::Error::Storage);
        }
        *self.image.lock().unwrap() = Some(bytes.to_vec());
        self.writes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}
fn key(i: usize) -> SigningKey {
    SigningKey::from([i as u8 + 20; 32])
}
fn roster(n: usize) -> Vec<SortedRosterMember> {
    (0..n)
        .map(|i| SortedRosterMember {
            pub_key: PubKeyID(VerificationKeyBytes::from(&key(i)).into()),
            stake: 1,
            cumulative_stake: i as u64 + 1,
        })
        .collect()
}
fn node() -> (SharedTransport, STPAddress) {
    let identity =
        new_keypair_from_connect_magic1(CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2b).unwrap();
    let socket = UdpSocket::bind((Ipv6Addr::LOCALHOST, 0)).unwrap();
    let port = socket.local_addr().unwrap().port();
    drop(socket);
    let address = STPAddress::from(Ipv6Addr::LOCALHOST, port, &identity);
    (
        SharedTransport::new(vec![identity], port, None, (1_000_000, 512 * 1024, 8 << 20)),
        address,
    )
}
fn config(
    transport: SharedTransport,
    roster: Vec<SortedRosterMember>,
    seeds: Vec<STPAddress>,
) -> RuntimeConfig {
    let mut config = RuntimeConfig::new(1, roster, [11; 32], 4, transport, seeds);
    config.max_body = 4096;
    config.rho = 60;
    config.timeouts = Timeouts {
        propose: 800,
        prevote: 300,
        precommit: 500,
        inc_propose: 90,
        inc_prevote: 30,
        inc_precommit: 60,
    };
    config.poll_interval = Duration::from_millis(10);
    config.status_interval = Duration::from_millis(30);
    config.request_timeout = 300;
    config.status_ttl = 2_000;
    config
}
fn core_config(config: &RuntimeConfig) -> Config {
    Config::crosslink(
        config.vote_namespace,
        config
            .initial_roster
            .iter()
            .map(|m| Member {
                public_key: m.pub_key.0,
                weight: m.stake,
            })
            .collect(),
        config.max_body,
        config.rho,
        config.timeouts,
    )
    .unwrap()
}
fn value(height: u32) -> BlockValue {
    BlockValue(
        BftBlock {
            version: 1,
            height,
            previous_block_fat_ptr: FatPointerToBftBlock::null(),
            headers: vec![],
            slashforks: vec![],
            do_not_include_until_bc_height: 0,
        }
        .zcash_serialize_to_vec()
        .unwrap(),
    )
}
fn host(roster: Vec<SortedRosterMember>, pushed: Arc<AtomicUsize>) -> HostCallbacks {
    let tip = HostTip {
        height: 0,
        block_id: ValueId::NIL,
        next_roster: roster,
        next_vote_namespace: [11; 32],
        next_strict_quorum: true,
    };
    HostCallbacks {
        propose: ClosureToProposeNewBlock(Arc::new(|| Box::pin(async { Some(value(1)) }))),
        validate: ClosureToValidateProposedBlock(Arc::new(|_| {
            Box::pin(async { (TMStatus::Pass, TMStatusReason::None) })
        })),
        push_decided: ClosureToPushDecidedBlock(Arc::new(move |_, pointer, chunks| {
            assert_eq!(pointer.get_vote_template().height, 1);
            assert!(
                chunks.is_empty(),
                "D28 exports collected PC signatures, no legacy proposal-signing phase"
            );
            pushed.fetch_add(1, Ordering::SeqCst);
            Box::pin(pending())
        })),
        read_tip: ClosureToReadDecidedTip(Arc::new(move |_| Some(tip.clone()))),
        update_peers: ClosureToUpdatePeers(Arc::new(|_| Box::pin(async {}))),
        access: AccessD28::default(),
    }
}
fn executor() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}
async fn until(mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(12), async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("runtime made no progress");
}

#[test]
fn pending_acquisition_does_not_stop_online_timers_or_access() {
    executor().block_on(async {
        let (transport, _) = node();
        let mut config = config(transport, roster(1), vec![]);
        config.timeouts = Timeouts {
            propose: 30,
            prevote: 20,
            precommit: 30,
            inc_propose: 9,
            inc_prevote: 3,
            inc_precommit: 6,
        };
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
        task.abort();
        let _ = task.await;
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
        task.abort();
        let _ = task.await;
        let before = disk.writes.load(Ordering::SeqCst);
        let task = tokio::spawn(runtime::run(
            key(0),
            disk.clone(),
            config(transport, roster(1), vec![]),
            host(roster(1), pushes.clone()),
        ));
        until(|| pushes.load(Ordering::SeqCst) == 2).await;
        assert!(disk.writes.load(Ordering::SeqCst) >= before);
        task.abort();
        let _ = task.await;
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
        let storage = DurableStore::open(
            disk.clone(),
            Limits {
                max_members: config.max_members,
                max_body: config.max_body,
            },
        )
        .unwrap();
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
            if view.catching_up {
                assert_eq!(view.height, 5);
                observed.fetch_add(1, Ordering::SeqCst);
            }
        }));
        let height = tip_height.clone();
        callbacks.read_tip = ClosureToReadDecidedTip(Arc::new(move |_| {
            Some(HostTip {
                height: height.load(Ordering::SeqCst),
                block_id: ValueId::NIL,
                next_roster: roster(1),
                next_vote_namespace: [11; 32],
                next_strict_quorum: true,
            })
        }));
        callbacks.propose = ClosureToProposeNewBlock(Arc::new(|| Box::pin(pending())));
        let task = tokio::spawn(runtime::run(key(0), disk.clone(), config, callbacks));
        until(|| behind.load(Ordering::SeqCst) >= 3).await;
        assert_eq!(
            disk.writes.load(Ordering::SeqCst),
            before,
            "recovery must not materialize/sign while host is behind"
        );
        tip_height.store(4, Ordering::SeqCst);
        until(|| disk.writes.load(Ordering::SeqCst) > before).await;
        task.abort();
        let _ = task.await;
    });
}

#[test]
fn restored_configuration_mismatch_refuses_before_any_write() {
    executor().block_on(async {
        let (transport, _) = node();
        let mut config = config(transport, roster(1), vec![]);
        let disk = Disk::new(&config);
        let c = core_config(&config);
        let storage = DurableStore::open(
            disk.clone(),
            Limits {
                max_members: config.max_members,
                max_body: config.max_body,
            },
        )
        .unwrap();
        let mut engine = Engine::boot(storage, key(0), c.clone()).unwrap();
        engine.init(&c, 1, 0, &mut Quiet).unwrap();
        drop(engine);
        let before = disk.writes.load(Ordering::SeqCst);
        config.vote_namespace = [99; 32];
        let mut callbacks = host(roster(1), Arc::new(AtomicUsize::new(0)));
        callbacks.read_tip = ClosureToReadDecidedTip(Arc::new(|_| None));
        let error = runtime::run(key(0), disk.clone(), config, callbacks)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("restored D28 configuration"));
        assert_eq!(disk.writes.load(Ordering::SeqCst), before);
    });
}

#[test]
fn four_real_localhost_finalizers_decide_while_host_pushes_are_pending() {
    executor().block_on(async {
        let nodes: Vec<_> = (0..4).map(|_| node()).collect();
        let seeds: Vec<_> = nodes.iter().map(|(_, address)| address.clone()).collect();
        let pushes: Vec<_> = (0..4).map(|_| Arc::new(AtomicUsize::new(0))).collect();
        let mut tasks = Vec::new();
        let decided_ticks = Arc::new(AtomicUsize::new(0));
        for (i, (transport, _)) in nodes.into_iter().enumerate() {
            let config = config(
                transport,
                roster(4),
                seeds
                    .iter()
                    .enumerate()
                    .filter(|(j, _)| *j != i)
                    .map(|(_, a)| a.clone())
                    .collect(),
            );
            let disk = Disk::new(&config);
            let mut callbacks = host(roster(4), pushes[i].clone());
            // Real asynchronous validation must not stall peer/status service.
            callbacks.validate = ClosureToValidateProposedBlock(Arc::new(|_| {
                Box::pin(async {
                    tokio::time::sleep(Duration::from_millis(45)).await;
                    (TMStatus::Pass, TMStatusReason::None)
                })
            }));
            let ticks = decided_ticks.clone();
            callbacks.access = AccessD28(Arc::new(move |snapshot, _| {
                if snapshot.step == Step::Decided {
                    ticks.fetch_add(1, Ordering::SeqCst);
                }
            }));
            tasks.push(tokio::spawn(runtime::run(key(i), disk, config, callbacks)));
        }
        until(|| pushes.iter().all(|n| n.load(Ordering::SeqCst) == 1)).await;
        let before = decided_ticks.load(Ordering::SeqCst);
        until(|| decided_ticks.load(Ordering::SeqCst) >= before + 8).await;
        for task in tasks {
            task.abort();
            let _ = task.await;
        }
    });
}

#[derive(Default)]
struct QuorumProgress {
    snapshots: VecDeque<(u128, String)>,
    decision: Option<(u64, d28::Id)>,
    absent_voted: bool,
}

#[derive(Debug, Default)]
struct BaseProgress {
    services: usize,
    control_received: usize,
    bulk_received: usize,
    transport: TransportStats,
}

async fn three_of_four_localhost_case(absent: usize, host_defaults: bool, with_base: bool) {
    let started = Instant::now();
    let deadline = Duration::from_secs(90);
    let mut nodes: Vec<_> = (0..4).map(|_| Some(node())).collect();
    let seeds: Vec<_> = nodes
        .iter()
        .map(|node| node.as_ref().unwrap().1.clone())
        .collect();
    // The fourth identity remains in the stake roster and seed list, but has
    // neither a finalizer nor a listening STP endpoint for the entire case.
    drop(nodes[absent].take().unwrap());
    let pushes: Vec<_> = (0..4).map(|_| Arc::new(AtomicUsize::new(0))).collect();
    let progress: Vec<_> = (0..4)
        .map(|_| Arc::new(Mutex::new(QuorumProgress::default())))
        .collect();
    let base_progress: Vec<_> = (0..4)
        .map(|_| Arc::new(Mutex::new(BaseProgress::default())))
        .collect();
    let mut tasks = Vec::new();
    let mut base_tasks = Vec::new();
    let mut writes = Vec::new();
    for (i, node) in nodes.into_iter().enumerate() {
        let Some((transport, _)) = node else { continue };
        // No discovered address can cause a dial outside this ephemeral loopback set.
        transport.dial_only(seeds.iter().map(STPAddress::connection_key).collect());
        let peers = seeds
            .iter()
            .enumerate()
            .filter(|(j, _)| *j != i)
            .map(|(_, address)| address.clone())
            .collect();
        let config = if host_defaults {
            let config = RuntimeConfig::new(1, roster(4), [11; 32], 4, transport.clone(), peers);
            assert_eq!(config.max_body, 1 << 20);
            config
        } else {
            config(transport.clone(), roster(4), peers)
        };
        let c = core_config(&config);
        assert!(c.quorum(3) && !c.quorum(2));
        assert_eq!(
            c.proposer(1, 0),
            1,
            "identity 1 is the absent-initial-proposer case"
        );
        let mut disk = Disk::new(&config);
        disk.deadline = Some(started + deadline);
        writes.push((i, disk.writes.clone()));
        if with_base {
            let mut base = transport.attach(Consumer::Base);
            let observed = base_progress[i].clone();
            base_tasks.push((
                i,
                tokio::spawn(async move {
                    let mut push = NetworkThreadPush::default();
                    loop {
                        let pull = base.service(push).expect("Base shared transport stopped");
                        {
                            let mut observed = observed.lock().unwrap();
                            observed.services += 1;
                            for (_, message) in pull.received_unreliable_messages {
                                assert_eq!(
                                    message.len(),
                                    2,
                                    "unexpected Base test message: {message:?}"
                                );
                                assert!(usize::from(message[1]) < 4);
                                assert_ne!(usize::from(message[1]), absent);
                                assert_ne!(usize::from(message[1]), i);
                                match message[0] {
                                    1 => observed.control_received += 1,
                                    2 => observed.bulk_received += 1,
                                    _ => {
                                        panic!("non-Base test traffic routed to Base: {message:?}")
                                    }
                                }
                            }
                            observed.transport = base.stats();
                        }
                        // Preserve every offered session: omitting one means killing it
                        // for BOTH consumers. Exercise both actual Base send channels.
                        push = NetworkThreadPush {
                            send_unreliable: pull
                                .current_connections
                                .iter()
                                .map(|(address, _)| (address.connection_key(), vec![1, i as u8]))
                                .collect(),
                            send_bulk: pull
                                .current_connections
                                .iter()
                                .map(|(address, _)| (address.connection_key(), vec![2, i as u8]))
                                .collect(),
                            wanted_connections: pull.current_connections,
                            ..Default::default()
                        };
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                }),
            ));
        }
        let mut callbacks = host(roster(4), pushes[i].clone());
        callbacks.validate = ClosureToValidateProposedBlock(Arc::new(|_| {
            Box::pin(async {
                tokio::time::sleep(Duration::from_millis(45)).await;
                (TMStatus::Pass, TMStatusReason::None)
            })
        }));
        let observed = progress[i].clone();
        callbacks.access = AccessD28(Arc::new(move |view, addresses| {
            let votes = |rows: &[Option<d28::Vote>]| {
                rows.iter()
                    .flatten()
                    .map(|v| {
                        (
                            v.content.signer,
                            v.content.round,
                            v.content.id.map(|id| hex::encode(&id[..4])),
                        )
                    })
                    .collect::<Vec<_>>()
            };
            let peers: Vec<_> = view
                .roster
                .iter()
                .enumerate()
                .filter(|(_, member)| addresses.contains_key(&member.pub_key))
                .map(|(i, _)| i)
                .collect();
            let state = format!(
                "h={} r={} step={:?} lock={:?} valid={:?} catching_up={} peers={peers:?} pv={:?} pc={:?} pending={:?} conflicts={}",
                view.height,
                view.round,
                view.step,
                view.locked_round,
                view.valid_round,
                view.catching_up,
                votes(view.prevotes),
                votes(view.precommits),
                view.pending
                    .as_ref()
                    .map(|p| (p.round, hex::encode(&p.id[..4]))),
                view.conflicts.len(),
            );
            let mut observed = observed.lock().unwrap();
            observed.absent_voted |=
                view.prevotes[absent].is_some() || view.precommits[absent].is_some();
            if view.step == Step::Decided {
                if let Some(pending) = &view.pending {
                    observed.decision = Some((pending.round, pending.id));
                }
            }
            if observed
                .snapshots
                .back()
                .is_none_or(|(_, previous)| previous != &state)
            {
                if observed.snapshots.len() == 12 {
                    observed.snapshots.pop_front();
                }
                observed
                    .snapshots
                    .push_back((started.elapsed().as_millis(), state));
            }
        }));
        tasks.push((
            i,
            tokio::spawn(runtime::run(key(i), disk, config, callbacks)),
        ));
    }
    let online: Vec<_> = (0..4).filter(|&i| i != absent).collect();
    let reached = tokio::time::timeout(deadline, async {
        loop {
            if tasks.iter().any(|(_, task)| task.is_finished())
                || base_tasks.iter().any(|(_, task)| task.is_finished())
            {
                return false;
            }
            if online.iter().all(|&i| {
                pushes[i].load(Ordering::SeqCst) == 1
                    && progress[i].lock().unwrap().decision.is_some()
                    && (!with_base || {
                        let base = base_progress[i].lock().unwrap();
                        base.control_received > 0 && base.bulk_received > 0
                    })
            }) {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await;
    // Always tear down our tasks before asserting, including the timeout path.
    let mut exits = Vec::new();
    for (i, task) in tasks {
        if task.is_finished() {
            exits.push(format!("runtime {i}: {:?}", task.await));
        } else {
            task.abort();
            let _ = task.await;
        }
    }
    for (i, task) in base_tasks {
        if task.is_finished() {
            exits.push(format!("Base {i}: {:?}", task.await));
        } else {
            task.abort();
            let _ = task.await;
        }
    }
    let mut diagnostics = format!(
        "absent={absent} initial_proposer_absent={} host_defaults={host_defaults} with_base={with_base} elapsed={:?} outcome={reached:?} exits={exits:?}",
        absent == 1,
        started.elapsed(),
    );
    for (i, writes) in writes {
        diagnostics.push_str(&format!(
            "\nnode {i}: pushes={} durable_writes={} Base={:?}",
            pushes[i].load(Ordering::SeqCst),
            writes.load(Ordering::SeqCst),
            base_progress[i].lock().unwrap(),
        ));
        for (ms, snapshot) in &progress[i].lock().unwrap().snapshots {
            diagnostics.push_str(&format!("\n  +{ms}ms {snapshot}"));
        }
    }
    eprintln!("three-of-four STP: {diagnostics}");
    assert!(
        matches!(reached, Ok(true)) && exits.is_empty(),
        "{diagnostics}"
    );
    let decision = progress[online[0]].lock().unwrap().decision.unwrap().1;
    for i in online {
        let observed = progress[i].lock().unwrap();
        let (round, id) = observed.decision.unwrap();
        assert_eq!(
            id, decision,
            "online nodes decided different values: {diagnostics}"
        );
        assert!(
            !observed.absent_voted,
            "absent identity contributed a vote: {diagnostics}"
        );
        if absent == 1 {
            assert!(
                round > 0,
                "absent initial proposer must be skipped: {diagnostics}"
            );
        }
    }
}

#[test]
fn three_of_four_real_localhost_finalizers_each_absent_identity() {
    executor().block_on(async {
        for absent in [1, 0, 2, 3] {
            three_of_four_localhost_case(absent, false, false).await;
        }
    });
}

// Full-sized durable images need an optimized build to exercise the production
// timer budget, rather than the throughput of debug serialization and hashing.
#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "production-sized runtime timing: run with --release"
)]
fn three_of_four_real_localhost_finalizers_default_host_constants() {
    executor().block_on(async {
        for absent in [1, 0, 2, 3] {
            three_of_four_localhost_case(absent, true, false).await;
        }
    });
}

#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "production-sized runtime timing: run with --release"
)]
fn three_of_four_real_localhost_finalizers_default_host_constants_with_base() {
    executor().block_on(async {
        for absent in [1, 0, 2, 3] {
            three_of_four_localhost_case(absent, true, true).await;
        }
    });
}

#[derive(Debug, Default)]
struct SlowDiskTrace {
    armed: bool,
    guard_hit: bool,
    writes_since_access: usize,
    snapshots: Vec<(u128, u64, Step, usize, usize)>,
}

struct SlowDisk {
    disk: Disk,
    trace: Arc<Mutex<SlowDiskTrace>>,
}
impl SlowDisk {
    const WRITE_DELAY: Duration = Duration::from_millis(30);
}
impl AtomicBlockStore for SlowDisk {
    fn block_size(&self) -> usize {
        self.disk.block_size()
    }
    fn load(&mut self, bytes: &mut [u8]) -> d28::Result<bool> {
        self.disk.load(bytes)
    }
    fn replace(&mut self, bytes: &[u8]) -> d28::Result<()> {
        let armed = {
            let mut trace = self.trace.lock().unwrap();
            if trace.armed {
                trace.writes_since_access += 1;
                // A synchronous elapse can prevent Tokio's timeout/cancellation
                // from running. Stop runaway fixture work deterministically;
                // hitting this guard is a test FAILURE, never a passing timeout.
                if trace.writes_since_access > 64 {
                    trace.guard_hit = true;
                    return Err(d28::Error::Storage);
                }
            }
            trace.armed
        };
        if armed {
            std::thread::sleep(Self::WRITE_DELAY);
        }
        self.disk.replace(bytes)
    }
}

#[test]
#[cfg_attr(
    debug_assertions,
    ignore = "production-sized runtime timing: run with --release"
)]
fn slow_durable_writes_yield_to_runtime_and_shared_transport_between_elapsed_slices() {
    executor().block_on(async {
        let (transport, _) = node();
        // Keep production consensus deadlines and growth: only rho is shortened
        // below the injected replacement delay. Sub-second synthetic consensus
        // timers would instead measure full-image debug serialization throughput.
        let mut config = RuntimeConfig::new(1, roster(1), [11; 32], 4, transport.clone(), vec![]);
        assert_eq!(config.max_body, 1 << 20);
        config.rho = 20;
        assert!(Duration::from_millis(config.rho) < SlowDisk::WRITE_DELAY);
        let disk = Disk::new(&config);
        let writes = disk.writes.clone();
        let trace = Arc::new(Mutex::new(SlowDiskTrace::default()));
        let backend = SlowDisk { disk, trace: trace.clone() };
        let mut base = transport.attach(Consumer::Base);
        let base_services = Arc::new(AtomicUsize::new(0));
        let services = base_services.clone();
        let base_task = tokio::spawn(async move {
            let mut push = NetworkThreadPush::default();
            loop {
                let pull = base.service(push).expect("Base transport stopped during slow storage");
                assert!(pull.received_unreliable_messages.is_empty());
                push = NetworkThreadPush {
                    wanted_connections: pull.current_connections,
                    ..Default::default()
                };
                services.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        });
        let mut callbacks = host(roster(1), Arc::new(AtomicUsize::new(0)));
        callbacks.propose = ClosureToProposeNewBlock(Arc::new(|| Box::pin(pending())));
        let started = Instant::now();
        let observed = trace.clone();
        let services = base_services.clone();
        callbacks.access = AccessD28(Arc::new(move |view, _| {
            let mut trace = observed.lock().unwrap();
            let writes = trace.writes_since_access;
            trace.snapshots.push((started.elapsed().as_millis(), view.round, view.step,
                writes, services.load(Ordering::SeqCst)));
            trace.writes_since_access = 0;
            // Delay only after initialization, isolating online catch-up from boot.
            trace.armed = true;
        }));
        let task = tokio::spawn(runtime::run(key(0), backend, config, callbacks));
        let reached = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                if task.is_finished() || base_task.is_finished() {
                    return false;
                }
                if {
                    let trace = trace.lock().unwrap();
                    trace.snapshots.len() >= 40
                        && trace.snapshots.last().is_some_and(|snapshot| snapshot.1 >= 1)
                } {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await;
        let exit = if task.is_finished() {
            Some(format!("{:?}", task.await))
        } else {
            task.abort();
            let _ = task.await;
            None
        };
        let base_finished = base_task.is_finished();
        base_task.abort();
        let base_exit = base_task.await;
        let trace = trace.lock().unwrap();
        eprintln!("slow durable runtime: outcome={reached:?} exit={exit:?} writes={} Base_services={} Base_exit={base_exit:?} trace={trace:?}",
            writes.load(Ordering::SeqCst), base_services.load(Ordering::SeqCst));
        assert!(matches!(reached, Ok(true)), "slow durable storage starved runtime access: {trace:?}; {exit:?}");
        assert!(exit.is_none() && !base_finished, "runtime or Base exited: {exit:?}; {base_exit:?}");
        assert!(!trace.guard_hit, "elapsed catch-up exceeded the per-service write guard: {trace:?}");
        assert!(trace.snapshots.last().unwrap().1 >= 1, "consensus timers stopped: {trace:?}");
        assert!(base_services.load(Ordering::SeqCst) >= 10, "Base service starved: {trace:?}");
    });
}

#[test]
fn unrepresentable_chain_height_is_refused_without_signing() {
    executor().block_on(async {
        for initial in [true, false] {
            let (transport, _) = node();
            let mut config = config(transport, roster(1), vec![]);
            if initial {
                config.initial_height = u32::MAX as u64 + 1;
            }
            let disk = Disk::new(&config);
            let mut callbacks = host(roster(1), Arc::new(AtomicUsize::new(0)));
            if !initial {
                callbacks.read_tip = ClosureToReadDecidedTip(Arc::new(|_| {
                    Some(HostTip {
                        height: u32::MAX as u64,
                        block_id: ValueId::NIL,
                        next_roster: roster(1),
                        next_vote_namespace: [11; 32],
                        next_strict_quorum: true,
                    })
                }));
            }
            let error = runtime::run(key(0), disk.clone(), config, callbacks)
                .await
                .unwrap_err();
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
            Box::pin(async move {
                Some(match attempt {
                    0 => value(2),
                    1 => {
                        let mut bytes = value(1);
                        bytes.0.push(0);
                        bytes
                    }
                    2 => BlockValue(vec![0; 4]),
                    _ => value(1),
                })
            })
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
        task.abort();
        let _ = task.await;
    });
}

#[test]
fn completed_acquisition_rechecks_host_tip_before_any_old_height_proposal_or_vote() {
    executor().block_on(async {
        for advance_in_validation in [false, true] {
            let (transport, _) = node();
            let config = config(transport, roster(1), vec![]);
            let disk = Disk::new(&config);
            let host_height = Arc::new(AtomicU64::new(0));
            let seen = Arc::new(AtomicU64::new(0));
            let old_exports = Arc::new(AtomicUsize::new(0));
            let mut callbacks = host(roster(1), Arc::new(AtomicUsize::new(0)));
            let height = host_height.clone();
            callbacks.read_tip = ClosureToReadDecidedTip(Arc::new(move |_| {
                Some(HostTip {
                    height: height.load(Ordering::SeqCst),
                    block_id: ValueId::NIL,
                    next_roster: roster(1),
                    next_vote_namespace: [11; 32],
                    next_strict_quorum: true,
                })
            }));
            let height = host_height.clone();
            callbacks.propose = ClosureToProposeNewBlock(Arc::new(move || {
                let height = height.clone();
                Box::pin(async move {
                    if height.load(Ordering::SeqCst) != 0 {
                        return pending().await;
                    }
                    if !advance_in_validation {
                        height.store(1, Ordering::SeqCst);
                    }
                    Some(value(1))
                })
            }));
            let height = host_height.clone();
            callbacks.validate = ClosureToValidateProposedBlock(Arc::new(move |_| {
                let height = height.clone();
                Box::pin(async move {
                    height.store(1, Ordering::SeqCst);
                    (TMStatus::Pass, TMStatusReason::None)
                })
            }));
            let (height, exported) = (seen.clone(), old_exports.clone());
            callbacks.access = AccessD28(Arc::new(move |snapshot, _| {
                height.store(snapshot.height, Ordering::SeqCst);
                if snapshot.height == 1 {
                    assert!(snapshot.prevotes.iter().all(Option::is_none));
                    assert!(snapshot.precommits.iter().all(Option::is_none));
                    assert!(snapshot.pending.is_none());
                    exported.fetch_add(1, Ordering::SeqCst);
                }
            }));
            let task = tokio::spawn(runtime::run(key(0), disk, config, callbacks));
            until(|| seen.load(Ordering::SeqCst) == 2).await;
            assert!(
                old_exports.load(Ordering::SeqCst) > 0,
                "old snapshot is delivered before INIT"
            );
            task.abort();
            let _ = task.await;
        }
    });
}

#[test]
fn completed_decision_is_published_before_host_acknowledgment_retires_height() {
    executor().block_on(async {
        let (transport, _) = node();
        let config = config(transport, roster(1), vec![]);
        let disk = Disk::new(&config);
        let snapshots = Arc::new(Mutex::new(Vec::new()));
        let mut callbacks = host(roster(1), Arc::new(AtomicUsize::new(0)));
        callbacks.push_decided = ClosureToPushDecidedBlock(Arc::new(|_, _, _| {
            Box::pin(async {
                HostTip {
                    height: 1,
                    block_id: ValueId::NIL,
                    next_roster: roster(1),
                    next_vote_namespace: [11; 32],
                    next_strict_quorum: true,
                }
            })
        }));
        let log = snapshots.clone();
        callbacks.access = AccessD28(Arc::new(move |snapshot, _| {
            log.lock()
                .unwrap()
                .push((snapshot.height, snapshot.pending.is_some()));
        }));
        let task = tokio::spawn(runtime::run(key(0), disk, config, callbacks));
        until(|| {
            snapshots
                .lock()
                .unwrap()
                .iter()
                .any(|(height, _)| *height == 2)
        })
        .await;
        let log = snapshots.lock().unwrap();
        let new_height = log.iter().position(|(height, _)| *height == 2).unwrap();
        assert!(
            log[..new_height]
                .iter()
                .any(|&(height, pending)| height == 1 && pending)
        );
        drop(log);
        task.abort();
        let _ = task.await;
    });
}

#[test]
fn passive_fullnode_follows_host_and_resumes_after_roster_readdition() {
    executor().block_on(async {
        let (transport, _) = node();
        let passive_roster = vec![roster(2)[1].clone()];
        let config = config(transport, passive_roster.clone(), vec![]);
        let disk = Disk::new(&config);
        let host_tip = Arc::new(Mutex::new(HostTip {
            height: 0,
            block_id: ValueId::NIL,
            next_roster: passive_roster.clone(),
            next_vote_namespace: [11; 32],
            next_strict_quorum: true,
        }));
        let seen_height = Arc::new(AtomicU64::new(0));
        let proposed = Arc::new(AtomicUsize::new(0));
        let mut callbacks = host(passive_roster.clone(), Arc::new(AtomicUsize::new(0)));
        let tip = host_tip.clone();
        callbacks.read_tip =
            ClosureToReadDecidedTip(Arc::new(move |_| Some(tip.lock().unwrap().clone())));
        let seen = seen_height.clone();
        callbacks.access = AccessD28(Arc::new(move |snapshot, _| {
            seen.store(snapshot.height, Ordering::SeqCst);
        }));
        let calls = proposed.clone();
        callbacks.propose = ClosureToProposeNewBlock(Arc::new(move || {
            calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(pending())
        }));
        let task = tokio::spawn(runtime::run(key(0), disk.clone(), config, callbacks));
        until(|| seen_height.load(Ordering::SeqCst) == 1).await;
        assert_eq!(
            proposed.load(Ordering::SeqCst),
            0,
            "nonmember must not request/sign proposals"
        );
        for (height, next_roster, should_propose) in [
            (1, roster(1), true),
            (2, passive_roster, false),
            (3, roster(1), true),
        ] {
            let before = proposed.load(Ordering::SeqCst);
            {
                let mut tip = host_tip.lock().unwrap();
                tip.height = height;
                tip.next_roster = next_roster;
            }
            until(|| seen_height.load(Ordering::SeqCst) == height + 1).await;
            assert_eq!(
                proposed.load(Ordering::SeqCst),
                before + usize::from(should_propose)
            );
        }
        assert!(!task.is_finished());
        task.abort();
        let _ = task.await;
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
        task.abort();
        let _ = task.await;
    });
}
