//! Tests for the shared verification cache key and the bounded store behind it.
//!
//! The verifiers pin their own key construction over real bundles. These cover the parts that are
//! the same for both of them: which of the key's three components separate two entries, the
//! store's capacity and eviction, and — because crosslink adds a transaction version Zakura does
//! not have — that a v7 staking action reaches the transaction identity the key is built from.

use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use tower::Service;

use zebra_chain::{
    block::Height,
    parameters::NetworkUpgrade,
    transaction::{AuthDigest, Hash, LockTime, Transaction, UnminedTxId, WtxId},
};

use super::{BoxError, CacheKey, Cached, CachedItem, ShieldedPool, VerifiedBundles};

/// Returns a transaction ID with no witness, as a v1-v4 transaction has.
fn legacy_tx_id(tag: u8) -> UnminedTxId {
    UnminedTxId::Legacy(Hash::from([tag; 32]))
}

/// Returns a witnessed transaction ID, as a v5 or v7 transaction has.
fn witnessed_tx_id(txid_tag: u8, auth_digest_tag: u8) -> UnminedTxId {
    UnminedTxId::Witnessed(WtxId {
        id: Hash::from([txid_tag; 32]),
        auth_digest: AuthDigest::from([auth_digest_tag; 32]),
    })
}

/// The pool separates the two bundles a transaction can carry under one ID and one sighash.
#[test]
fn keys_for_the_same_transaction_differ_by_pool() {
    let tx_id = witnessed_tx_id(1, 2);
    let sighash = [3; 32];

    let keys = [
        CacheKey::new(tx_id, sighash, ShieldedPool::Sapling),
        CacheKey::new(tx_id, sighash, ShieldedPool::Orchard),
    ];

    let unique: HashSet<_> = keys.iter().collect();
    assert_eq!(
        unique.len(),
        keys.len(),
        "one transaction's two shielded bundles must not share a cache key"
    );
}

/// The authorizing-data digest separates two witnessed IDs that share a txid.
///
/// Under ZIP 244 a v5 transaction's txid excludes its proofs and signatures, so a key that named
/// only the txid would answer both of these with one verification.
#[test]
fn witnessed_keys_differ_by_authorizing_data() {
    let sighash = [3; 32];

    assert_ne!(
        CacheKey::new(witnessed_tx_id(1, 2), sighash, ShieldedPool::Orchard),
        CacheKey::new(witnessed_tx_id(1, 4), sighash, ShieldedPool::Orchard),
        "the same txid with different authorizing data must not share a cache key"
    );
}

/// The sighash separates two verifications of one transaction's bundle.
///
/// The sighash is not a function of the transaction alone: the amounts and scripts of the spent
/// transparent outputs enter it, and those come from the verification context.
#[test]
fn keys_differ_by_sighash() {
    let tx_id = witnessed_tx_id(1, 2);

    assert_ne!(
        CacheKey::new(tx_id, [3; 32], ShieldedPool::Sapling),
        CacheKey::new(tx_id, [4; 32], ShieldedPool::Sapling),
        "the sighash is an input to verification, so it must be an input to the key"
    );
}

/// A legacy ID and a witnessed ID never collide, whatever they contain.
#[test]
fn legacy_and_witnessed_keys_differ() {
    let sighash = [3; 32];

    assert_ne!(
        CacheKey::new(legacy_tx_id(1), sighash, ShieldedPool::Sapling),
        CacheKey::new(witnessed_tx_id(1, 1), sighash, ShieldedPool::Sapling),
        "a v4 transaction ID must not collide with a witnessed one"
    );
}

/// Builds a minimal v7 `VCrosslink` transaction carrying `staking_action`.
fn vcrosslink_tx(
    staking_action: Option<zcash_primitives::transaction::StakingAction>,
) -> Transaction {
    Transaction::VCrosslink {
        network_upgrade: NetworkUpgrade::Nu6,
        lock_time: LockTime::unlocked(),
        expiry_height: Height(0),
        inputs: Vec::new(),
        outputs: Vec::new(),
        sapling_shielded_data: None,
        orchard_shielded_data: None,
        staking_action,
    }
}

/// Returns a `CreateNewDelegationBond` staking action bonding `amount_zats`.
///
/// `StakingAction` is one flat struct whose `kind` selects which fields
/// `StakingAction::hash_to_state` folds into the transaction digests; for this kind the amount is
/// one of them.
fn bond_action(amount_zats: u64) -> zcash_primitives::transaction::StakingAction {
    use zcash_primitives::transaction::{StakingAction, StakingActionKind};

    StakingAction {
        kind: StakingActionKind::CreateNewDelegationBond,
        amount_zats,
        arg32_0: [1; 32],
        arg32_1: [2; 32],
        arg32_2: [3; 32],
        arg32_3: [4; 32],
        arg64_0: [5; 64],
        arg64_1: [6; 64],
    }
}

/// A v7 staking action reaches the transaction identity the cache key is built from.
///
/// This is the crosslink-specific half of [`CacheKey`]'s correctness argument. `VCrosslink`
/// transactions carry shielded bundles, so they are cached like any other; the key names only the
/// transaction ID, sighash and pool, so two transactions that differ solely in their staking
/// action must not share an ID. librustzcash's crosslink digester folds the action into both the
/// txid and the authorizing-data commitment, and this pins that.
///
/// A regression here would let one verification answer for a bundle in a *different* staking
/// transaction — a consensus bug, not a performance bug.
#[test]
fn staking_actions_change_the_transaction_identity() {
    let without = vcrosslink_tx(None).unmined_id();
    let bond_one = vcrosslink_tx(Some(bond_action(1))).unmined_id();
    let bond_two = vcrosslink_tx(Some(bond_action(2))).unmined_id();

    assert_ne!(
        without, bond_one,
        "adding a staking action must change the transaction identity"
    );
    assert_ne!(
        bond_one, bond_two,
        "two staking actions differing only in amount must not share a transaction identity"
    );

    let sighash = [7; 32];
    let keys: HashSet<_> = [without, bond_one, bond_two]
        .into_iter()
        .map(|tx_id| CacheKey::new(tx_id, sighash, ShieldedPool::Orchard))
        .collect();

    assert_eq!(
        keys.len(),
        3,
        "three distinct staking transactions must have three distinct cache keys"
    );
}

/// A v7 transaction takes the witnessed form, so its ID commits to authorizing data.
///
/// [`CacheKey`]'s argument leans on this: a legacy ID would commit to the whole serialized
/// transaction, but a v5-style txid alone would not cover proofs and signatures.
#[test]
fn vcrosslink_transactions_have_witnessed_ids() {
    assert!(
        matches!(
            vcrosslink_tx(Some(bond_action(1))).unmined_id(),
            UnminedTxId::Witnessed(_)
        ),
        "a v7 transaction must have a witnessed ID, so its key covers authorizing data"
    );
}

/// The lookup set and the eviction queue always hold the same keys.
///
/// They are two representations of one fact. A path that updated one without the other would
/// either drop a key that `contains` still answers — remembering a bundle for the rest of the
/// process — or grow the queue past the capacity it was built with.
#[test]
fn the_lookup_set_and_the_eviction_queue_hold_the_same_keys() {
    let mut verified = VerifiedBundles::new(2);
    let keys = [
        CacheKey::new(legacy_tx_id(1), [0; 32], ShieldedPool::Sapling),
        CacheKey::new(legacy_tx_id(2), [0; 32], ShieldedPool::Sapling),
        CacheKey::new(legacy_tx_id(3), [0; 32], ShieldedPool::Sapling),
    ];

    for key in keys {
        verified.insert(key);
        assert_eq!(
            verified.keys.len(),
            verified.insertion_order.len(),
            "the lookup set and the eviction queue must hold the same keys"
        );
        assert!(verified.keys.len() <= 2, "the capacity bounds the cache");
    }
    assert!(
        !verified.contains(&keys[0]),
        "the oldest key must be evicted"
    );

    let repeated = verified.insert(keys[2]);
    assert!(!repeated.inserted, "a concurrent duplicate is not recorded");
    assert_eq!(repeated.evicted, 0, "a duplicate must not evict anything");
    assert_eq!(verified.keys.len(), verified.insertion_order.len());

    verified.clear();
    assert!(verified.keys.is_empty() && verified.insertion_order.is_empty());
}

/// A verifier that counts calls and returns whatever it is told to.
#[derive(Clone)]
struct CountingVerifier {
    calls: Arc<AtomicUsize>,
    result: Result<(), &'static str>,
}

impl CountingVerifier {
    fn new(result: Result<(), &'static str>) -> Self {
        Self {
            calls: Arc::new(AtomicUsize::new(0)),
            result,
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Service<TestItem> for CountingVerifier {
    type Response = ();
    type Error = BoxError;
    type Future = futures::future::BoxFuture<'static, Result<(), BoxError>>;

    fn poll_ready(&mut self, _cx: &mut std::task::Context<'_>) -> std::task::Poll<Result<(), BoxError>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, _item: TestItem) -> Self::Future {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let result = self.result.map_err(BoxError::from);
        Box::pin(async move { result })
    }
}

/// An item that reports whichever key the test gives it.
#[derive(Clone)]
struct TestItem(Option<CacheKey>);

impl CachedItem for TestItem {
    fn cache_key(&self) -> Option<CacheKey> {
        self.0
    }
}

/// A second verification of a remembered bundle never reaches the verifier.
#[tokio::test]
async fn a_hit_skips_the_inner_verifier() {
    let inner = CountingVerifier::new(Ok(()));
    let mut cached = Cached::new(inner.clone(), 16, "test");
    let item = TestItem(Some(CacheKey::new(
        legacy_tx_id(1),
        [0; 32],
        ShieldedPool::Sapling,
    )));

    cached.call(item.clone()).await.expect("first call verifies");
    assert_eq!(inner.calls(), 1, "the first verification must reach the verifier");

    cached.call(item.clone()).await.expect("second call is a hit");
    assert_eq!(
        inner.calls(),
        1,
        "a remembered bundle must not be verified again"
    );

    // A different key is a different bundle, and must still be verified.
    let other = TestItem(Some(CacheKey::new(
        legacy_tx_id(2),
        [0; 32],
        ShieldedPool::Sapling,
    )));
    cached.call(other).await.expect("a different key misses");
    assert_eq!(inner.calls(), 2, "a different key must reach the verifier");
}

/// A failed verification is never remembered.
///
/// A batch error is not per-item evidence: `Fallback` re-verifies failures singly, and a
/// shut-down batch worker reports the same way. Remembering one would reject a valid block —
/// or, worse, remember a verdict this node never reached.
#[tokio::test]
async fn a_failure_is_not_remembered() {
    let inner = CountingVerifier::new(Err("invalid proof"));
    let mut cached = Cached::new(inner.clone(), 16, "test");
    let item = TestItem(Some(CacheKey::new(
        legacy_tx_id(1),
        [0; 32],
        ShieldedPool::Orchard,
    )));

    for attempt in 1..=3 {
        cached
            .call(item.clone())
            .await
            .expect_err("the verifier rejects this item");
        assert_eq!(
            inner.calls(),
            attempt,
            "every attempt at an unverified bundle must reach the verifier"
        );
    }
}

/// An item with no key is verified every time.
#[tokio::test]
async fn an_item_without_a_key_is_always_verified() {
    let inner = CountingVerifier::new(Ok(()));
    let mut cached = Cached::new(inner.clone(), 16, "test");
    let item = TestItem(None);

    cached.call(item.clone()).await.expect("verifies");
    cached.call(item).await.expect("verifies again");

    assert_eq!(
        inner.calls(),
        2,
        "an item that offers no key must never be answered from the cache"
    );
}

/// Clearing the cache makes the next verification reach the verifier again.
#[tokio::test]
async fn clearing_forgets_every_bundle() {
    let inner = CountingVerifier::new(Ok(()));
    let mut cached = Cached::new(inner.clone(), 16, "test");
    let item = TestItem(Some(CacheKey::new(
        legacy_tx_id(1),
        [0; 32],
        ShieldedPool::Sapling,
    )));

    cached.call(item.clone()).await.expect("verifies");
    cached.call(item.clone()).await.expect("hit");
    assert_eq!(inner.calls(), 1);

    cached.clear();

    cached.call(item).await.expect("verifies again");
    assert_eq!(
        inner.calls(),
        2,
        "a cleared cache must verify the bundle again"
    );
}
