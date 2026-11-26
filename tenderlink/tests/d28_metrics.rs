//! Observational metrics on the real durable runtime and localhost STP path.
use ed25519_zebra::{SigningKey, VerificationKeyBytes};
use std::{
    future::pending,
    net::{Ipv6Addr, UdpSocket},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tenderlink::{
    BlockValue, CONNECT_MAGIC1_Noise_IK_25519_ChaChaPoly_BLAKE2b, ClosureToProposeNewBlock,
    ClosureToPushDecidedBlock, ClosureToReadDecidedTip, ClosureToUpdatePeers,
    ClosureToValidateProposedBlock, HostTip, STPAddress, SortedRosterMember, TMStatus,
    TMStatusReason, ValueId,
    d28::{
        self, AtomicBlockStore,
        durable::Limits,
        runtime::{self, AccessD28, HostCallbacks, MetricsSnapshot, RuntimeConfig, RuntimeMetrics},
    },
    stp::new_keypair_from_connect_magic1,
    transport::SharedTransport,
};
use zcash_primitives::bft::{BftBlock, FatPointerToBftBlock, PubKeyID};

struct Disk {
    size: usize,
    image: Option<Vec<u8>>,
}
impl AtomicBlockStore for Disk {
    fn block_size(&self) -> usize {
        self.size
    }
    fn load(&mut self, bytes: &mut [u8]) -> d28::Result<bool> {
        if let Some(image) = &self.image {
            bytes.copy_from_slice(image);
            Ok(true)
        } else {
            Ok(false)
        }
    }
    fn replace(&mut self, bytes: &[u8]) -> d28::Result<()> {
        assert_eq!(bytes.len(), self.size);
        self.image = Some(bytes.to_vec());
        Ok(())
    }
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
fn counters(snapshot: MetricsSnapshot) -> Vec<u64> {
    let mut values = Vec::new();
    for counts in [
        snapshot.offered.hello,
        snapshot.offered.status,
        snapshot.offered.bare_votes,
        snapshot.offered.evidence,
        snapshot.received,
        snapshot.processed,
        snapshot.rejected,
        snapshot.scheduler_budget_discarded,
    ] {
        values.extend([counts.messages, counts.bytes]);
    }
    values.push(snapshot.offered.status_request_messages);
    values
}

#[test]
fn real_stp_runtime_metrics_are_shared_monotonic_and_count_encoded_app_bytes() {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let keys: Vec<_> = (1..=2).map(|i| SigningKey::from([i; 32])).collect();
            let roster: Vec<_> = keys
                .iter()
                .enumerate()
                .map(|(i, key)| SortedRosterMember {
                    pub_key: PubKeyID(VerificationKeyBytes::from(key).into()),
                    stake: 1,
                    cumulative_stake: i as u64 + 1,
                })
                .collect();
            let nodes: Vec<_> = (0..2).map(|_| node()).collect();
            let addresses: Vec<_> = nodes.iter().map(|(_, address)| address.clone()).collect();
            let metrics: Vec<_> = (0..2)
                .map(|_| Arc::new(RuntimeMetrics::default()))
                .collect();
            let decisions: Vec<_> = (0..2).map(|_| Arc::new(AtomicUsize::new(0))).collect();
            let mut tasks = Vec::new();
            for (i, (transport, _)) in nodes.into_iter().enumerate() {
                let mut config = RuntimeConfig::new(
                    1,
                    roster.clone(),
                    [11; 32],
                    2,
                    transport,
                    vec![addresses[1 - i].clone()],
                );
                assert!(config.metrics.is_none());
                config.metrics = Some(metrics[i].clone());
                config.max_body = 4096;
                config.poll_interval = Duration::from_millis(10);
                config.status_interval = Duration::from_millis(30);
                let disk = Disk {
                    size: d28::durable::image_size(Limits {
                        max_members: config.max_members,
                        max_body: config.max_body,
                    })
                    .unwrap(),
                    image: None,
                };
                let pushed = decisions[i].clone();
                let tip = HostTip {
                    height: 0,
                    block_id: ValueId::NIL,
                    next_roster: roster.clone(),
                    next_vote_namespace: [11; 32],
                    next_strict_quorum: true,
                };
                let host = HostCallbacks {
                    propose: ClosureToProposeNewBlock(Arc::new(|| {
                        Box::pin(async {
                            Some(BlockValue(
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
                            ))
                        })
                    })),
                    validate: ClosureToValidateProposedBlock(Arc::new(|_| {
                        Box::pin(async { (TMStatus::Pass, TMStatusReason::None) })
                    })),
                    push_decided: ClosureToPushDecidedBlock(Arc::new(move |_, pointer, chunks| {
                        assert_eq!(pointer.get_vote_template().height, 1);
                        assert!(chunks.is_empty());
                        pushed.fetch_add(1, Ordering::SeqCst);
                        Box::pin(pending())
                    })),
                    read_tip: ClosureToReadDecidedTip(Arc::new(move |_| Some(tip.clone()))),
                    update_peers: ClosureToUpdatePeers(Arc::new(|_| Box::pin(async {}))),
                    access: AccessD28::default(),
                };
                tasks.push(tokio::spawn(runtime::run(
                    keys[i].clone(),
                    disk,
                    config,
                    host,
                )));
            }
            let completed = tokio::time::timeout(Duration::from_secs(15), async {
                let mut previous = vec![MetricsSnapshot::default(); metrics.len()];
                loop {
                    for (i, metrics) in metrics.iter().enumerate() {
                        let now = metrics.snapshot();
                        for (old, new) in counters(previous[i]).into_iter().zip(counters(now)) {
                            assert!(new >= old, "a cumulative metric decreased");
                        }
                        previous[i] = now;
                    }
                    if decisions.iter().all(|n| n.load(Ordering::SeqCst) == 1) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await;
            for task in tasks {
                task.abort();
                let result = task.await;
                assert!(
                    result.unwrap_err().is_cancelled(),
                    "runtime stopped unexpectedly"
                );
            }
            completed.expect("real runtimes did not decide");
            let mut evidence = 0;
            let mut requests = 0;
            for metrics in metrics {
                let snapshot = metrics.snapshot();
                let offered = snapshot.offered;
                assert!(offered.hello.messages > 0);
                assert!(offered.status.messages > 0);
                assert!(offered.bare_votes.messages > 0);
                assert_eq!(offered.hello.bytes, offered.hello.messages * 98);
                assert_eq!(offered.status.bytes, offered.status.messages * 144);
                assert_eq!(offered.bare_votes.bytes, offered.bare_votes.messages * 140);
                assert!(offered.status_request_messages <= offered.status.messages);
                assert!(snapshot.received.messages > 0 && snapshot.received.bytes > 0);
                assert!(snapshot.processed.messages > 0);
                assert_eq!(snapshot.scheduler_budget_discarded.messages, 0);
                // Cancellation may leave one returned transport inbox unconsumed.
                assert!(
                    snapshot.processed.messages + snapshot.rejected.messages
                        <= snapshot.received.messages
                );
                assert!(
                    snapshot.processed.bytes + snapshot.rejected.bytes <= snapshot.received.bytes
                );
                evidence += offered.evidence.messages;
                requests += offered.status_request_messages;
                assert_eq!(
                    snapshot,
                    metrics.snapshot(),
                    "metrics changed after runtime cancellation"
                );
            }
            assert!(
                evidence > 0 && requests > 0,
                "guided evidence path was not observed"
            );
        });
}
