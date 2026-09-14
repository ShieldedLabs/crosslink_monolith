//! Async Sapling batch verifier service

use core::fmt;
use std::{
    future::Future,
    mem,
    pin::Pin,
    task::{Context, Poll},
};

use futures::{future::BoxFuture, FutureExt, TryFutureExt};
use once_cell::sync::Lazy;
use rand::thread_rng;
use tokio::sync::watch;
use tower::{util::ServiceFn, Service};
use tower_batch_control::{Batch, BatchControl, RequestWeight};
use tower_fallback::Fallback;

use sapling_crypto::{bundle::Authorized, BatchValidator, Bundle};
use zcash_protocol::value::ZatBalance;
use zebra_chain::transaction::{SigHash, UnminedTxId};

use crate::groth16::SAPLING;
use crate::primitives::cache::{CacheKey, Cached, CachedItem, ShieldedPool, CACHE_CAPACITY};

#[derive(Clone)]
pub struct Item {
    /// The bundle containing the Sapling shielded data to verify.
    bundle: Bundle<Authorized, ZatBalance>,
    /// The sighash of the transaction that contains the Sapling shielded data.
    sighash: SigHash,
    /// The key this item's successful verification is remembered under, if the caller
    /// supplied the transaction's identity. `None` items are always verified.
    cache_key: Option<CacheKey>,
}

impl Item {
    /// Creates a new [`Item`] from a Sapling bundle and sighash.
    ///
    /// Items made this way are verified normally but are not cached, because they carry no
    /// transaction identity. Use [`Item::new_cacheable`] where one is available.
    pub fn new(bundle: Bundle<Authorized, ZatBalance>, sighash: SigHash) -> Self {
        Self {
            bundle,
            sighash,
            cache_key: None,
        }
    }

    /// Creates an [`Item`] whose successful verification can be remembered.
    ///
    /// `tx_id` must identify the transaction `bundle` was parsed from; see [`CacheKey`] for why
    /// that, the sighash and the pool determine the verification.
    pub fn new_cacheable(
        bundle: Bundle<Authorized, ZatBalance>,
        sighash: SigHash,
        tx_id: UnminedTxId,
    ) -> Self {
        Self {
            bundle,
            sighash,
            cache_key: Some(CacheKey::new(tx_id, sighash.0, ShieldedPool::Sapling)),
        }
    }
}

impl RequestWeight for Item {}

impl CachedItem for Item {
    fn cache_key(&self) -> Option<CacheKey> {
        self.cache_key
    }
}

/// A service that verifies Sapling shielded data in batches.
///
/// Handles batching incoming requests, driving batches to completion, and reporting results.
#[derive(Default)]
pub struct Verifier {
    /// A batch verifier for Sapling shielded data.
    batch: BatchValidator,

    /// A channel for broadcasting the verification result of the batch.
    ///
    /// Each batch gets a newly created channel, so there is only ever one result sent per channel.
    /// Tokio doesn't have a oneshot multi-consumer channel, so we use a watch channel.
    tx: watch::Sender<Option<bool>>,
}

impl fmt::Debug for Verifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Verifier")
            .field("batch", &"..")
            .field("tx", &self.tx)
            .finish()
    }
}

impl Drop for Verifier {
    // Flush the current batch in case there are still any pending futures.
    //
    // Flushing the batch means we need to validate it. This function fires off the validation and
    // returns immediately, usually before the validation finishes.
    fn drop(&mut self) {
        let batch = mem::take(&mut self.batch);
        let tx = mem::take(&mut self.tx);

        // The validation is CPU-intensive; do it on a dedicated thread so it does not block.
        rayon::spawn_fifo(move || {
            let (spend_vk, output_vk) = SAPLING.verifying_keys();

            // Validate the batch and send the result through the channel.
            let res = batch.validate(&spend_vk, &output_vk, thread_rng());
            let _ = tx.send(Some(res));
        });
    }
}

impl Service<BatchControl<Item>> for Verifier {
    type Response = ();
    type Error = Box<dyn std::error::Error + Send + Sync>;
    type Future = Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: BatchControl<Item>) -> Self::Future {
        match req {
            BatchControl::Item(item) => {
                let mut rx = self.tx.subscribe();

                let bundle_check = self
                    .batch
                    .check_bundle(item.bundle, item.sighash.into())
                    .then_some(())
                    .ok_or("invalid Sapling bundle");

                async move {
                    bundle_check?;

                    rx.changed()
                        .await
                        .map_err(|_| "verifier was dropped without flushing")
                        .and_then(|_| {
                            // We use a new channel for each batch, so we always get the correct
                            // batch result here.
                            rx.borrow()
                                .ok_or("threadpool unexpectedly dropped channel sender")?
                                .then(|| {
                                    metrics::counter!("proofs.groth16.verified").increment(1);
                                })
                                .ok_or_else(|| {
                                    metrics::counter!("proofs.groth16.invalid").increment(1);
                                    "batch verification of Sapling shielded data failed"
                                })
                        })
                        .map_err(Self::Error::from)
                }
                .boxed()
            }

            BatchControl::Flush => {
                let batch = mem::take(&mut self.batch);
                let tx = mem::take(&mut self.tx);

                async move {
                    tokio::task::spawn_blocking(move || {
                        let (spend_vk, output_vk) = SAPLING.verifying_keys();

                        let res = batch.validate(&spend_vk, &output_vk, thread_rng());
                        tx.send(Some(res))
                    })
                    .await?
                    .map_err(Self::Error::from)
                }
                .boxed()
            }
        }
    }
}

/// Verifies a single [`Item`].
pub fn verify_single(
    item: Item,
) -> Pin<Box<dyn Future<Output = Result<(), Box<dyn std::error::Error + Send + Sync>>> + Send>> {
    async move {
        let mut verifier = Verifier::default();

        let check = verifier
            .batch
            .check_bundle(item.bundle, item.sighash.into())
            .then_some(())
            .ok_or("invalid Sapling bundle");
        check?;

        tokio::task::spawn_blocking(move || {
            let (spend_vk, output_vk) = SAPLING.verifying_keys();

            mem::take(&mut verifier.batch).validate(&spend_vk, &output_vk, thread_rng())
        })
        .await
        .map_err(|_| "Sapling bundle validation thread panicked")?
        .then_some(())
        .ok_or("invalid proof or sig in Sapling bundle")
    }
    .map_err(Box::from)
    .boxed()
}

/// Global batch verification context for Sapling shielded data.
///
/// The stack is wrapped in a [`Cached`] so that proofs and signatures gossiped into the mempool
/// are not verified a second time when the block that mines them arrives.
pub static VERIFIER: Lazy<
    Cached<
        Fallback<
            Batch<Verifier, Item>,
            ServiceFn<
                fn(
                    Item,
                )
                    -> BoxFuture<'static, Result<(), Box<dyn std::error::Error + Send + Sync>>>,
            >,
        >,
    >,
> = Lazy::new(|| Cached::new(batch_fallback_verifier(), CACHE_CAPACITY, "groth16_sapling"));

/// Builds the uncached batching-and-fallback stack.
fn batch_fallback_verifier() -> Fallback<
    Batch<Verifier, Item>,
    ServiceFn<fn(Item) -> BoxFuture<'static, Result<(), Box<dyn std::error::Error + Send + Sync>>>>,
> {
    Fallback::new(
        Batch::new(
            Verifier::default(),
            super::MAX_BATCH_SIZE,
            None,
            super::MAX_BATCH_LATENCY,
        ),
        tower::service_fn(verify_single),
    )
}
