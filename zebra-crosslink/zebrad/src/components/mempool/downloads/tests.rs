//! Fixed test vectors for the mempool transaction downloader.

use std::{net::SocketAddr, time::Duration};

use futures::StreamExt as _;
use tower::{service_fn, util::BoxCloneService};

use zebra_chain::parameters::Network;
use zebra_test::mock_service::{MockService, PanicAssertion};

use super::*;

/// A directly pushed full transaction must consume the sending peer's
/// per-peer admission budget, exactly like an advertised transaction ID, so
/// a single peer cannot bypass [`MAX_INBOUND_CONCURRENCY_PER_PEER`] by
/// sending `tx` messages instead of `inv` advertisements.
///
/// Regression test for `GHSA-m9xx-8rcj-vmgp`.
#[tokio::test]
async fn per_peer_cap_applies_to_pushed_transactions() {
    let _init_guard = zebra_test::init();

    let network = Network::Mainnet;
    let source = PeerSource::Legacy("192.168.0.1:8233".parse().expect("valid socket address"));

    // Distinct full transactions, as if pushed directly by a single peer.
    let pushed: Vec<Gossip> = network
        .unmined_transactions_in_blocks(1..=10)
        .take(MAX_INBOUND_CONCURRENCY_PER_PEER + 1)
        .map(|tx| Gossip::Tx(tx.transaction))
        .collect();
    assert_eq!(
        pushed.len(),
        MAX_INBOUND_CONCURRENCY_PER_PEER + 1,
        "test needs enough distinct transactions to exceed the per-peer cap",
    );

    // The cap is enforced before any download/verify work, so the services
    // are never actually driven in this test.
    let peer_set: MockService<zn::Request, zn::Response, PanicAssertion> =
        MockService::build().for_unit_tests();
    let verifier: MockService<tx::MempoolRequest, tx::MempoolResponse, PanicAssertion> =
        MockService::build().for_unit_tests();
    let state: MockService<zs::Request, zs::Response, PanicAssertion> =
        MockService::build().for_unit_tests();

    let mut downloads = Downloads::new(
        peer_set,
        verifier,
        state,
        DEFAULT_MAX_INBOUND_CONCURRENCY_PER_STP_PEER,
    );

    // The first `MAX_INBOUND_CONCURRENCY_PER_PEER` pushes from this peer are admitted.
    for gossip in pushed.iter().take(MAX_INBOUND_CONCURRENCY_PER_PEER) {
        downloads
            .download_if_needed_and_verify(gossip.clone(), Some(source), None)
            .expect("pushes below the per-peer cap are admitted");
    }

    // The next push from the same peer exceeds the per-peer cap and is
    // rejected, even though the global queue still has spare capacity.
    let result = downloads.download_if_needed_and_verify(
        pushed[MAX_INBOUND_CONCURRENCY_PER_PEER].clone(),
        Some(source),
        None,
    );
    assert!(
        matches!(result, Err(MempoolError::FullQueue)),
        "a push exceeding the per-peer cap must be rejected with `FullQueue`, got {result:?}",
    );

    // Don't leave spawned download tasks behind when the runtime shuts down.
    downloads.cancel_all();
}

/// A directly pushed transaction from a peer must keep that peer's address on
/// the `Invalid` verification error, so the mempool can score the peer's
/// misbehavior. Regression test for `GHSA-g7c4-2w6c-cr3r`.
#[tokio::test]
async fn pushed_transaction_attributes_invalid_error_to_peer() {
    use zebra_consensus::error::TransactionError;

    let _init_guard = zebra_test::init();

    let network = Network::Mainnet;
    let peer_addr: SocketAddr = "203.0.113.7:8233".parse().unwrap();
    let transaction = network
        .unmined_transactions_in_blocks(1..=1)
        .next()
        .expect("at least one test transaction")
        .transaction;
    let transaction_upgrade = transaction.transaction.network_upgrade();

    type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

    let mut downloads = Downloads::new(
        BoxCloneService::new(service_fn(|_request| async move {
            panic!("pushed transactions must not be downloaded");
        })),
        BoxCloneService::new(service_fn(|_request| async move {
            Err(Box::new(TransactionError::WrongVersion) as BoxError)
        })),
        BoxCloneService::new(service_fn(|request| async move {
            match request {
                zs::Request::Transaction(_) => Ok(zs::Response::Transaction(None)),
                zs::Request::Tip => Ok(zs::Response::Tip(None)),
                request => Err(format!("unexpected state request: {request:?}").into()),
            }
        })),
        DEFAULT_MAX_INBOUND_CONCURRENCY_PER_STP_PEER,
    );

    downloads
        .download_if_needed_and_verify(
            Gossip::Tx(transaction),
            Some(PeerSource::Legacy(peer_addr)),
            None,
        )
        .expect("download is queued");

    let result = tokio::time::timeout(Duration::from_secs(1), downloads.next())
        .await
        .expect("pushed transaction should complete")
        .expect("download stream should yield an item")
        .expect("pushed transaction should not time out");

    let error = result
        .expect_err("invalid pushed transaction should fail verification")
        .1;
    assert!(
        matches!(
            error,
            TransactionDownloadVerifyError::Invalid {
                advertiser_addr: Some(addr),
                transaction_upgrade: actual_upgrade,
                verification_height: Height(0),
                ..
            } if addr == PeerSocketAddr::from(peer_addr)
                && actual_upgrade == transaction_upgrade
        ),
        "expected the pushed transaction failure to carry its peer, branch, and height, got {error:?}"
    );
}

/// A verification timeout must release the source's per-peer slot, or a peer whose
/// transactions time out loses its whole cap until the mempool resets.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn per_peer_slot_released_after_verification_timeout() {
    let _init_guard = zebra_test::init();

    let network = Network::Mainnet;
    let source = PeerSource::Legacy("192.168.0.2:8233".parse().expect("valid socket address"));

    let pushed: Vec<Gossip> = network
        .unmined_transactions_in_blocks(1..=10)
        .take(MAX_INBOUND_CONCURRENCY_PER_PEER + 1)
        .map(|tx| Gossip::Tx(tx.transaction))
        .collect();

    // Nothing answers the state, so every task hits `RATE_LIMIT_DELAY`.
    let peer_set: MockService<zn::Request, zn::Response, PanicAssertion> =
        MockService::build().for_unit_tests();
    let verifier: MockService<tx::MempoolRequest, tx::MempoolResponse, PanicAssertion> =
        MockService::build().for_unit_tests();
    let state: MockService<zs::Request, zs::Response, PanicAssertion> =
        MockService::build().for_unit_tests();

    let mut downloads = Box::pin(Downloads::new(
        peer_set,
        verifier,
        state,
        DEFAULT_MAX_INBOUND_CONCURRENCY_PER_STP_PEER,
    ));

    for gossip in pushed.iter().take(MAX_INBOUND_CONCURRENCY_PER_PEER) {
        downloads
            .as_mut()
            .download_if_needed_and_verify(gossip.clone(), Some(source), None)
            .expect("pushes below the per-peer cap are admitted");
    }

    tokio::time::advance(RATE_LIMIT_DELAY + Duration::from_secs(5)).await;
    tokio::task::yield_now().await;

    for _ in 0..MAX_INBOUND_CONCURRENCY_PER_PEER {
        assert!(
            matches!(downloads.as_mut().next().await, Some(Err(_))),
            "every task should time out",
        );
    }

    downloads
        .as_mut()
        .download_if_needed_and_verify(
            pushed[MAX_INBOUND_CONCURRENCY_PER_PEER].clone(),
            Some(source),
            None,
        )
        .expect("timed-out tasks must release their source's slots");

    downloads.cancel_all();
}

/// The per-peer cap counts each [`PeerSource`] separately: an STP peer has the constructor's
/// cap, other STP peers and legacy peers have their own budgets, and the STP and legacy caps
/// are independent.
#[tokio::test]
async fn per_source_cap_counts_each_peer_source() {
    let _init_guard = zebra_test::init();

    const STP_CAP: usize = 3;
    let stp_a = PeerSource::Stp([0xA; 32]);
    let stp_b = PeerSource::Stp([0xB; 32]);
    let legacy = PeerSource::Legacy("192.168.0.3:8233".parse().expect("valid socket address"));

    let mut pushed = Network::Mainnet
        .unmined_transactions_in_blocks(1..=10)
        .map(|tx| Gossip::Tx(tx.transaction));
    let mut next = || pushed.next().expect("enough distinct test transactions");

    let peer_set: MockService<zn::Request, zn::Response, PanicAssertion> =
        MockService::build().for_unit_tests();
    let verifier: MockService<tx::MempoolRequest, tx::MempoolResponse, PanicAssertion> =
        MockService::build().for_unit_tests();
    let state: MockService<zs::Request, zs::Response, PanicAssertion> =
        MockService::build().for_unit_tests();
    let mut downloads = Downloads::new(peer_set, verifier, state, STP_CAP);

    for _ in 0..STP_CAP {
        downloads
            .download_if_needed_and_verify(next(), Some(stp_a), None)
            .expect("pushes below the STP cap are admitted");
    }

    let refused = next();
    assert_eq!(
        downloads.download_if_needed_and_verify(refused.clone(), Some(stp_a), None),
        Err(MempoolError::FullQueue),
        "the STP peer's cap is the constructor's, not the legacy cap",
    );

    downloads
        .download_if_needed_and_verify(refused, Some(stp_b), None)
        .expect("another STP peer has its own budget");

    downloads
        .download_if_needed_and_verify(next(), Some(legacy), None)
        .expect("a legacy peer has its own budget");

    downloads
        .download_if_needed_and_verify(next(), None, None)
        .expect("source-less candidates are not capped per peer");

    downloads.cancel_all();
}

/// An STP peer's invalid transaction carries no legacy address, so the mempool never reports
/// the STP peer to the legacy address book.
#[tokio::test]
async fn stp_pushed_transaction_has_no_advertiser_addr() {
    use zebra_consensus::error::TransactionError;

    let _init_guard = zebra_test::init();

    let transaction = Network::Mainnet
        .unmined_transactions_in_blocks(1..=1)
        .next()
        .expect("at least one test transaction")
        .transaction;

    type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

    let mut downloads = Downloads::new(
        BoxCloneService::new(service_fn(|_request| async move {
            panic!("pushed transactions must not be downloaded");
        })),
        BoxCloneService::new(service_fn(|_request| async move {
            Err(Box::new(TransactionError::WrongVersion) as BoxError)
        })),
        BoxCloneService::new(service_fn(|request| async move {
            match request {
                zs::Request::Transaction(_) => Ok(zs::Response::Transaction(None)),
                zs::Request::Tip => Ok(zs::Response::Tip(None)),
                request => Err(format!("unexpected state request: {request:?}").into()),
            }
        })),
        DEFAULT_MAX_INBOUND_CONCURRENCY_PER_STP_PEER,
    );

    downloads
        .download_if_needed_and_verify(
            Gossip::Tx(transaction),
            Some(PeerSource::Stp([7; 32])),
            None,
        )
        .expect("download is queued");

    let error = tokio::time::timeout(Duration::from_secs(1), downloads.next())
        .await
        .expect("pushed transaction should complete")
        .expect("download stream should yield an item")
        .expect("pushed transaction should not time out")
        .expect_err("invalid pushed transaction should fail verification")
        .1;
    assert!(
        matches!(
            error,
            TransactionDownloadVerifyError::Invalid {
                advertiser_addr: None,
                ..
            }
        ),
        "an STP peer must not be attributed a legacy address, got {error:?}"
    );
}
