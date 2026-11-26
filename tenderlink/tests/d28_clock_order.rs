//! Runtime tick ordering through real recovery, host polling and durable storage.
use ed25519_zebra::{SigningKey, VerificationKeyBytes};
use std::{
    future::{pending, poll_fn},
    net::{Ipv6Addr, UdpSocket},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::Poll,
    time::Duration,
};
use tenderlink::{
    BlockValue, CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2b, ClosureToProposeNewBlock,
    ClosureToPushDecidedBlock, ClosureToReadDecidedTip, ClosureToUpdatePeers,
    ClosureToValidateProposedBlock, HostTip, SortedRosterMember, TMStatus, TMStatusReason, ValueId,
    d28::{
        self, AtomicBlockStore, Config, Emission, Engine, Frame, Header, Member, Message, Proposal,
        ProposalContent, Sink, Status, Step, Target, Timeouts, Vote, VoteKind,
        durable::{DurableStore, Limits},
        runtime::{self, AccessD28, HostCallbacks, RuntimeConfig, Snapshot},
    },
    stp::new_keypair_from_connect_magic1,
    transport::SharedTransport,
};
use zcash_primitives::bft::{BftBlock, FatPointerToBftBlock, PubKeyID};

const PAUSE: Duration = Duration::from_millis(500);

#[derive(Clone)]
struct Disk {
    size: usize,
    image: Arc<Mutex<Option<Vec<u8>>>>,
}
impl Disk {
    fn new(config: &RuntimeConfig) -> Self {
        Self {
            size: d28::durable::image_size(limits(config)).unwrap(),
            image: Arc::new(Mutex::new(None)),
        }
    }
}
impl AtomicBlockStore for Disk {
    fn block_size(&self) -> usize {
        self.size
    }
    fn load(&mut self, bytes: &mut [u8]) -> d28::Result<bool> {
        assert_eq!(bytes.len(), self.size);
        if let Some(image) = &*self.image.lock().unwrap() {
            bytes.copy_from_slice(image);
            Ok(true)
        } else {
            Ok(false)
        }
    }
    fn replace(&mut self, bytes: &[u8]) -> d28::Result<()> {
        assert_eq!(bytes.len(), self.size);
        *self.image.lock().unwrap() = Some(bytes.to_vec());
        Ok(())
    }
}
fn limits(config: &RuntimeConfig) -> Limits {
    Limits {
        max_members: config.max_members,
        max_body: config.max_body,
    }
}
fn key(i: u8) -> SigningKey {
    SigningKey::from([i + 20; 32])
}
fn config() -> RuntimeConfig {
    let identity =
        new_keypair_from_connect_magic1(CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2b).unwrap();
    let socket = UdpSocket::bind((Ipv6Addr::LOCALHOST, 0)).unwrap();
    let port = socket.local_addr().unwrap().port();
    drop(socket);
    let transport =
        SharedTransport::new(vec![identity], port, None, (1_000_000, 512 * 1024, 8 << 20));
    // Local member 0 has quorum, but member 1 proposes at height 1 / round 0.
    let roster = [(3, 3), (1, 4)]
        .into_iter()
        .enumerate()
        .map(|(i, (stake, cumulative_stake))| SortedRosterMember {
            pub_key: PubKeyID(VerificationKeyBytes::from(&key(i as u8)).into()),
            stake,
            cumulative_stake,
        })
        .collect();
    let mut config = RuntimeConfig::new(1, roster, [11; 32], 2, transport, vec![]);
    config.max_body = 4096;
    config.rho = 10_000;
    config.timeouts = Timeouts {
        propose: 10_000,
        prevote: 200,
        precommit: 200,
        inc_propose: 90,
        inc_prevote: 30,
        inc_precommit: 60,
    };
    config.poll_interval = Duration::from_millis(10);
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
fn host(config: &RuntimeConfig) -> HostCallbacks {
    let tip = HostTip {
        height: 0,
        block_id: ValueId::NIL,
        next_roster: config.initial_roster.clone(),
        next_vote_namespace: config.vote_namespace,
        next_strict_quorum: true,
    };
    HostCallbacks {
        propose: ClosureToProposeNewBlock(Arc::new(|| Box::pin(pending()))),
        validate: ClosureToValidateProposedBlock(Arc::new(|_| Box::pin(pending()))),
        push_decided: ClosureToPushDecidedBlock(Arc::new(|_, _, _| Box::pin(pending()))),
        read_tip: ClosureToReadDecidedTip(Arc::new(move |_| Some(tip.clone()))),
        update_peers: ClosureToUpdatePeers(Arc::new(|_| Box::pin(async {}))),
        access: AccessD28::default(),
    }
}
struct Quiet;
impl Sink for Quiet {
    fn send(&mut self, _: Target, _: Frame<'_>) {}
    fn decided(&mut self, _: u64, _: Proposal<'_>, _: d28::Group<'_>) {}
}
fn seed_indeterminate_proposal(config: &RuntimeConfig, disk: Disk) {
    let c = core_config(config);
    assert_eq!(c.proposer(1, 0), 1);
    assert!(c.quorum(c.members()[0].weight));
    let storage = DurableStore::open(disk, limits(config)).unwrap();
    let mut engine = Engine::boot(storage, key(0), c.clone()).unwrap();
    engine.set_emission(Emission::Guided);
    engine.init(&c, 1, 0, &mut Quiet).unwrap();
    let body = BlockValue(
        BftBlock {
            version: 1,
            height: 1,
            previous_block_fat_ptr: FatPointerToBftBlock::null(),
            headers: vec![],
            slashforks: vec![],
            do_not_include_until_bc_height: 0,
        }
        .zcash_serialize_to_vec()
        .unwrap(),
    );
    let header = Header::sign(
        &c,
        &key(1),
        ProposalContent {
            height: 1,
            round: 0,
            valid_round: None,
            id: c.body_id(&body.0),
            signer: 1,
        },
    );
    engine
        .receive(
            Frame {
                message: Message::Ordinary,
                outer: None,
                proposal: Some(Proposal {
                    header: &header,
                    body: &body.0,
                }),
                groups: &[],
            },
            Status::Indeterminate,
            &mut Quiet,
        )
        .unwrap();
    assert_eq!(engine.view().unwrap().step, Step::Propose);
    assert!(engine.prevotes().iter().all(Option::is_none));
    assert!(engine.precommits().iter().all(Option::is_none));
}

#[derive(Debug)]
struct Observed {
    height: u64,
    round: u64,
    step: Step,
    own_precommit: Option<Vote>,
}
impl From<&Snapshot<'_>> for Observed {
    fn from(snapshot: &Snapshot<'_>) -> Self {
        Self {
            height: snapshot.height,
            round: snapshot.round,
            step: snapshot.step,
            own_precommit: snapshot.precommits[0],
        }
    }
}
fn executor() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}
async fn first_snapshot_after_pause(
    config: RuntimeConfig,
    disk: Disk,
    mut callbacks: HostCallbacks,
    gate: Arc<AtomicBool>,
) -> Observed {
    let (send, receive) = tokio::sync::oneshot::channel();
    let send = Mutex::new(Some(send));
    callbacks.access = AccessD28(Arc::new(move |snapshot, _| {
        if !gate.load(Ordering::SeqCst) {
            assert_eq!(
                (snapshot.height, snapshot.round, snapshot.step),
                (1, 0, Step::Propose)
            );
            // Deliberately stall this test driver AFTER its first tick snapshot.
            // Production AccessD28 callbacks must not block. The pause models a
            // scheduling gap; the validation below cannot finish before it ends.
            std::thread::sleep(PAUSE);
            gate.store(true, Ordering::SeqCst);
        } else if let Some(send) = send.lock().unwrap().take() {
            let _ = send.send(Observed::from(snapshot));
        }
    }));
    let task = tokio::spawn(runtime::run(key(0), disk, config, callbacks));
    let observed = tokio::time::timeout(Duration::from_secs(5), receive).await;
    // Stop and join our driver even on timeout or a failed assertion in access.
    // Only the FIRST post-pause snapshot matters; a later timeout is legitimate.
    task.abort();
    let stopped = task.await;
    assert!(
        matches!(&stopped, Err(error) if error.is_cancelled()),
        "runtime stopped unexpectedly: {stopped:?}"
    );
    observed
        .expect("runtime did not publish the post-pause snapshot")
        .expect("runtime dropped the observation channel")
}

#[test]
fn elapsed_before_validation_does_not_expire_new_precommit_timer() {
    executor().block_on(async {
        let config = config();
        let disk = Disk::new(&config);
        seed_indeterminate_proposal(&config, disk.clone());
        let gate = Arc::new(AtomicBool::new(false));
        let validated = Arc::new(AtomicBool::new(false));
        let mut callbacks = host(&config);
        let (ready, completed) = (gate.clone(), validated.clone());
        callbacks.validate = ClosureToValidateProposedBlock(Arc::new(move |_| {
            let (ready, completed) = (ready.clone(), completed.clone());
            Box::pin(poll_fn(move |_| {
                if ready.load(Ordering::SeqCst) {
                    completed.store(true, Ordering::SeqCst);
                    Poll::Ready((TMStatus::Fail, TMStatusReason::None))
                } else {
                    // HostWork polls this owned future again on each tick.
                    Poll::Pending
                }
            }))
        }));
        let observed = first_snapshot_after_pause(config, disk, callbacks, gate).await;
        assert!(validated.load(Ordering::SeqCst));
        assert_eq!(
            (observed.height, observed.round, observed.step),
            (1, 0, Step::Precommit),
            "elapsed before validation must not be charged to its newly armed timer: {observed:?}"
        );
        let vote = observed
            .own_precommit
            .expect("local nil PC after failed validation");
        assert_eq!(
            (vote.content.height, vote.content.round, vote.content.signer),
            (1, 0, 0)
        );
        assert_eq!(vote.content.kind, VoteKind::Precommit);
        assert_eq!(vote.content.id, None);
    });
}

#[test]
fn elapsed_gap_is_consumed_in_full_by_already_active_timers() {
    executor().block_on(async {
        let mut config = config();
        config.timeouts.propose = 200;
        let disk = Disk::new(&config);
        let callbacks = host(&config);
        let gate = Arc::new(AtomicBool::new(false));
        let observed = first_snapshot_after_pause(config, disk, callbacks, gate).await;
        assert_eq!(observed.height, 1);
        // The 500ms gap spans TP=200ms and then TPC=200ms. Consuming just
        // one expiry, capping at one poll interval, or dropping it cannot pass.
        assert!(
            observed.round >= 1,
            "the complete elapsed interval must drive the existing state: {observed:?}"
        );
    });
}
