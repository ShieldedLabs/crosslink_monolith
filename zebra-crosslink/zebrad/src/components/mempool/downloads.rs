//! Transaction downloader and verifier.
//!
//! The main struct [`Downloads`] allows downloading and verifying transactions.
//! It is used by the mempool to get transactions into it. It is also able to
//! just verify transactions that were directly pushed.
//!
//! The verification itself is done by the [`zebra_consensus`] crate.
//!
//! Verified transactions are returned to the caller in [`Downloads::poll_next`].
//! This is in contrast to the block downloader and verifiers which don't
//! return anything and forward the verified blocks to the state themselves.
//!
//! # Correctness
//!
//! The mempool downloader doesn't send verified transactions to the [`Mempool`]
//! service. So Zebra must spawn a task that regularly polls the downloader for
//! ready transactions. (To ensure that transactions propagate across the entire
//! network in each 75s block interval, the polling interval should be around
//! 5-10 seconds.)
//!
//! Polling the downloader from [`Mempool::poll_ready`] is not sufficient.
//! [`Service::poll_ready`] is only called when there is a service request.
//! But we want to download and gossip transactions,
//! even when there are no other service requests.
//!
//! [`Mempool`]: super::Mempool
//! [`Mempool::poll_ready`]: super::Mempool::poll_ready
use std::{
    collections::{HashMap, HashSet},
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use futures::{
    future::TryFutureExt,
    ready,
    stream::{FuturesUnordered, Stream},
    FutureExt,
};
use pin_project::{pin_project, pinned_drop};
use thiserror::Error;
use tokio::{sync::oneshot, task::JoinHandle};
use tower::{Service, ServiceExt};
use tracing_futures::Instrument;

use zebra_chain::{
    block::Height,
    parameters::NetworkUpgrade,
    transaction::{self, UnminedTxId, VerifiedUnminedTx},
    transparent,
};
use zebra_consensus::transaction as tx;
use zebra_network::{self as zn, PeerSocketAddr};
use zebra_node_services::mempool::{Gossip, PeerSource, Verdict};
use zebra_state::{self as zs, CloneError};

use crate::components::{
    mempool::crawler::RATE_LIMIT_DELAY,
    sync::{BLOCK_DOWNLOAD_TIMEOUT, BLOCK_VERIFY_TIMEOUT},
};

use super::MempoolError;

type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// Controls how long we wait for a transaction download request to complete.
///
/// This is currently equal to [`BLOCK_DOWNLOAD_TIMEOUT`] for
/// consistency, even though parts of the rationale used for defining the value
/// don't apply here (e.g. we can drop transactions hashes when the queue is full).
pub(crate) const TRANSACTION_DOWNLOAD_TIMEOUT: Duration = BLOCK_DOWNLOAD_TIMEOUT;

/// Controls how long we wait for a transaction verify request to complete.
///
/// This is currently equal to [`BLOCK_VERIFY_TIMEOUT`] for
/// consistency.
///
/// This timeout may lead to denial of service, which will be handled in
/// [#2694](https://github.com/ZcashFoundation/zebra/issues/2694)
pub(crate) const TRANSACTION_VERIFY_TIMEOUT: Duration = BLOCK_VERIFY_TIMEOUT;

/// The maximum number of concurrent inbound download and verify tasks.
///
/// We expect the mempool crawler to download and verify most mempool transactions, so this bound
/// can be small. But it should be at least the default `network.peerset_initial_target_size` config,
/// to avoid disconnecting peers on startup.
///
/// ## Security
///
/// We use a small concurrency limit, to prevent memory denial-of-service
/// attacks.
///
/// The maximum transaction size is 2 million bytes. A deserialized malicious
/// transaction with ~225_000 transparent outputs can take up 9MB of RAM.
/// (See #1880 for more details.)
///
/// Malicious transactions will eventually timeout or fail validation.
/// Once validation fails, the transaction is dropped, and its memory is deallocated.
///
/// Since Zebra keeps an `inv` index, inbound downloads for malicious transactions
/// will be directed to the malicious node that originally gossiped the hash.
/// Therefore, this attack can be carried out by a single malicious node.
//
// TODO: replace with the configured value of network.peerset_initial_target_size
pub const MAX_INBOUND_CONCURRENCY: usize = 500;

/// The maximum number of concurrent inbound download tasks attributable to a
/// single advertising legacy peer ([`PeerSource::Legacy`]).
///
/// Caps how many slots of [`MAX_INBOUND_CONCURRENCY`] one peer's `Inv`
/// advertisements can occupy, so a single peer cannot saturate the global
/// queue with fake txids and deny gossip-path mempool admission for honest
/// peers. See `GHSA-4fc2-h7jh-287c`. Crawler-driven and locally-pushed
/// transactions have no source peer and are not counted against the cap.
///
/// The cap can be small because the legacy mempool crawler later fetches what a peer's
/// refused candidates would have delivered.
pub const MAX_INBOUND_CONCURRENCY_PER_PEER: usize = 5;

/// The default maximum number of concurrent inbound download and verify tasks attributable to
/// a single STP peer ([`PeerSource::Stp`]).
///
/// The STP relay has no crawler to backstop a refused candidate, so its cap is larger than the
/// legacy one: it should cover the transactions an honest peer pushes between two of our
/// verdicts. It is still far below [`MAX_INBOUND_CONCURRENCY`], so one peer cannot fill the
/// queue.
pub const DEFAULT_MAX_INBOUND_CONCURRENCY_PER_STP_PEER: usize = 32;

/// The maximum number of concurrent inbound download and verify tasks attributable to all STP
/// peers together.
///
/// Many STP peers at their per-peer cap would otherwise fill [`MAX_INBOUND_CONCURRENCY`], and
/// RPC submissions and legacy gossip would be refused as a full queue.
pub const MAX_INBOUND_CONCURRENCY_ALL_STP_PEERS: usize = 128;

/// Sends a queued candidate's [`Verdict`] exactly once.
///
/// If it is dropped unsent, because the mempool was reset or dropped a task's result, it sends
/// [`Verdict::Cancelled`], so every queued candidate's caller gets a verdict.
#[derive(Debug)]
pub struct VerdictSender(Option<oneshot::Sender<Verdict>>);

impl VerdictSender {
    /// Returns a new sender, and the receiver its verdict goes to.
    pub fn channel() -> (Self, oneshot::Receiver<Verdict>) {
        let (sender, receiver) = oneshot::channel();
        (Self(Some(sender)), receiver)
    }

    /// Sends `verdict`. A caller that dropped its receiver doesn't want it, so that is ignored.
    pub fn send(mut self, verdict: Verdict) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(verdict);
        }
    }
}

impl Drop for VerdictSender {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(Verdict::Cancelled);
        }
    }
}

/// A marker struct for the oneshot channels which cancel a pending download and verify.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
struct CancelDownloadAndVerify;

/// Errors that can occur while downloading and verifying a transaction.
#[derive(Error, Debug, Clone)]
#[allow(dead_code)]
pub enum TransactionDownloadVerifyError {
    #[error("transaction is already in state")]
    InState,

    #[error("error in state service: {0}")]
    StateError(#[source] CloneError),

    #[error("error downloading transaction: {0}")]
    DownloadFailed(#[source] CloneError),

    #[error("transaction download / verification was cancelled")]
    Cancelled,

    #[error("transaction did not pass consensus validation: {error}")]
    Invalid {
        error: zebra_consensus::error::TransactionError,
        advertiser_addr: Option<PeerSocketAddr>,
        /// The transaction's consensus branch upgrade, if its version contains one.
        transaction_upgrade: Option<NetworkUpgrade>,
        /// The candidate block height used for mempool consensus verification.
        verification_height: Height,
    },
}

impl TransactionDownloadVerifyError {
    /// The verdict a queued candidate gets for this failure.
    pub fn verdict(&self) -> Verdict {
        match self {
            TransactionDownloadVerifyError::InState => Verdict::AlreadyKnown,
            TransactionDownloadVerifyError::StateError(_)
            | TransactionDownloadVerifyError::DownloadFailed(_) => Verdict::Dropped {
                reason: self.to_string(),
            },
            TransactionDownloadVerifyError::Cancelled => Verdict::Cancelled,
            TransactionDownloadVerifyError::Invalid {
                error,
                transaction_upgrade,
                ..
            } => Verdict::Invalid {
                error: error.to_string(),
                state_dependent: error.is_state_dependent(transaction_upgrade.is_some()),
            },
        }
    }
}

/// Represents a [`Stream`] of download and verification tasks.
#[pin_project(PinnedDrop)]
#[derive(Debug)]
pub struct Downloads<ZN, ZV, ZS>
where
    ZN: Service<zn::Request, Response = zn::Response, Error = BoxError> + Send + Clone + 'static,
    ZN::Future: Send,
    ZV: Service<tx::MempoolRequest, Response = tx::MempoolResponse, Error = BoxError>
        + Send
        + Clone
        + 'static,
    ZV::Future: Send,
    ZS: Service<zs::Request, Response = zs::Response, Error = BoxError> + Send + Clone + 'static,
    ZS::Future: Send,
{
    // Services
    /// A service that forwards requests to connected peers, and returns their
    /// responses.
    network: ZN,

    /// A service that verifies downloaded transactions.
    verifier: ZV,

    /// A service that manages cached blockchain state.
    state: ZS,

    // Internal downloads state
    /// A list of pending transaction download and verify tasks.
    #[pin]
    pending: FuturesUnordered<
        JoinHandle<
            Result<
                Result<
                    (
                        VerifiedUnminedTx,
                        Vec<transparent::OutPoint>,
                        Option<Height>,
                        VerdictSender,
                    ),
                    Box<(TransactionDownloadVerifyError, UnminedTxId)>,
                >,
                (UnminedTxId, tokio::time::error::Elapsed),
            >,
        >,
    >,

    /// A list of channels that can be used to cancel pending transaction
    /// download and verify tasks. Each entry also stores the corresponding
    /// gossip request and the announcing peer (when known), so completion can
    /// release the per-peer slot by `UnminedTxId` lookup.
    cancel_handles: HashMap<
        UnminedTxId,
        (
            oneshot::Sender<CancelDownloadAndVerify>,
            Gossip,
            Option<PeerSource>,
        ),
    >,

    /// The number of currently in-flight download tasks per source peer.
    ///
    /// Invariant: a peer is present here iff some entry in [`Self::cancel_handles`]
    /// has it as the third tuple element. Enforces [`Self::max_in_flight`].
    /// See `GHSA-4fc2-h7jh-287c`.
    pending_per_peer: HashMap<PeerSource, usize>,

    /// The maximum number of in-flight tasks for each [`PeerSource::Stp`] peer.
    max_in_flight_per_stp_peer: usize,
}

impl<ZN, ZV, ZS> Stream for Downloads<ZN, ZV, ZS>
where
    ZN: Service<zn::Request, Response = zn::Response, Error = BoxError> + Send + Clone + 'static,
    ZN::Future: Send,
    ZV: Service<tx::MempoolRequest, Response = tx::MempoolResponse, Error = BoxError>
        + Send
        + Clone
        + 'static,
    ZV::Future: Send,
    ZS: Service<zs::Request, Response = zs::Response, Error = BoxError> + Send + Clone + 'static,
    ZS::Future: Send,
{
    type Item = Result<
        Result<
            (
                VerifiedUnminedTx,
                Vec<transparent::OutPoint>,
                Option<Height>,
                VerdictSender,
            ),
            Box<(UnminedTxId, TransactionDownloadVerifyError)>,
        >,
        (UnminedTxId, tokio::time::error::Elapsed),
    >;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        let this = self.project();
        // CORRECTNESS
        //
        // The current task must be scheduled for wakeup every time we return
        // `Poll::Pending`.
        //
        // If no download and verify tasks have exited since the last poll, this
        // task is scheduled for wakeup when the next task becomes ready.
        //
        // TODO: this would be cleaner with poll_map (#2693)
        let item = if let Some(join_result) = ready!(this.pending.poll_next(cx)) {
            let result = join_result.expect("transaction download and verify tasks must not panic");
            let (result, completed_txid) = match result {
                Ok(Ok((tx, spent_mempool_outpoints, tip_height, rsp_tx))) => {
                    let hash = tx.transaction.id;
                    (
                        Ok(Ok((tx, spent_mempool_outpoints, tip_height, rsp_tx))),
                        hash,
                    )
                }
                Ok(Err(boxed_err)) => {
                    let (e, hash) = *boxed_err;
                    (Ok(Err(Box::new((hash, e)))), hash)
                }
                Err((txid, elapsed)) => (Err((txid, elapsed)), txid),
            };

            // Remove the cancel handle, and release the source's slot, on every completion,
            // including a verification timeout. Otherwise a timed-out task's queued `Gossip`
            // stays resident in `cancel_handles` (a peer that gets each transaction to hit
            // `RATE_LIMIT_DELAY` could leak ~2 MB per tx until OOM), and its source loses a
            // slot of its per-peer cap until the mempool resets.
            if let Some((_, _gossip, Some(source))) = this.cancel_handles.remove(&completed_txid) {
                Self::release_peer_slot(this.pending_per_peer, source);
            }

            Some(result)
        } else {
            None
        };

        Poll::Ready(item)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.pending.size_hint()
    }
}

impl<ZN, ZV, ZS> Downloads<ZN, ZV, ZS>
where
    ZN: Service<zn::Request, Response = zn::Response, Error = BoxError> + Send + Clone + 'static,
    ZN::Future: Send,
    ZV: Service<tx::MempoolRequest, Response = tx::MempoolResponse, Error = BoxError>
        + Send
        + Clone
        + 'static,
    ZV::Future: Send,
    ZS: Service<zs::Request, Response = zs::Response, Error = BoxError> + Send + Clone + 'static,
    ZS::Future: Send,
{
    /// Initialize a new download stream with the provided services.
    ///
    /// `network` is used to download transactions.
    /// `verifier` is used to verify transactions.
    /// `state` is used to check if transactions are already in the state.
    ///
    /// `max_in_flight_per_stp_peer` caps each [`PeerSource::Stp`] peer's in-flight tasks,
    /// usually [`DEFAULT_MAX_INBOUND_CONCURRENCY_PER_STP_PEER`]. Legacy peers are capped at
    /// [`MAX_INBOUND_CONCURRENCY_PER_PEER`].
    ///
    /// The [`Downloads`] stream is agnostic to the network policy, so retry and
    /// timeout limits should be applied to the `network` service passed into
    /// this constructor.
    pub fn new(network: ZN, verifier: ZV, state: ZS, max_in_flight_per_stp_peer: usize) -> Self {
        Self {
            network,
            verifier,
            state,
            pending: FuturesUnordered::new(),
            cancel_handles: HashMap::new(),
            pending_per_peer: HashMap::new(),
            max_in_flight_per_stp_peer,
        }
    }

    /// The maximum number of in-flight download and verify tasks for `source`.
    fn max_in_flight(&self, source: PeerSource) -> usize {
        match source {
            PeerSource::Legacy(_) => MAX_INBOUND_CONCURRENCY_PER_PEER,
            PeerSource::Stp(_) => self.max_in_flight_per_stp_peer,
        }
    }

    /// Queue a transaction for download (if needed) and verification.
    ///
    /// Returns a receiver for the transaction's [`Verdict`], or why it was not queued. Failures
    /// are answered on the receiver by the download task; successes, and storage failures, by
    /// the mempool once it tries to store the verified transaction.
    ///
    /// When `source` is `Some`, its per-peer cap ([`Self::max_in_flight`]) is enforced;
    /// crawler-driven and locally-pushed transactions pass `None` and are not capped per peer.
    /// Only a [`PeerSource::Legacy`] source is attributed for misbehaviour reports.
    pub fn download_if_needed_and_verify(
        &mut self,
        gossiped_tx: Gossip,
        source: Option<PeerSource>,
    ) -> Result<oneshot::Receiver<Verdict>, MempoolError> {
        let (rsp_tx, rsp_rx) = VerdictSender::channel();
        self.queue(gossiped_tx, source, rsp_tx)
            .map_err(|(error, _rsp_tx)| error)?;
        Ok(rsp_rx)
    }

    /// Queue an already-verified transaction for verification again, because the chain grew
    /// while it was verified, sending its verdict to `rsp_tx`.
    ///
    /// The transaction is re-queued without its source, so it doesn't count against the
    /// source's cap again. If it can't be queued, its verdict is [`Verdict::Dropped`].
    pub fn requeue(&mut self, gossiped_tx: Gossip, rsp_tx: VerdictSender) {
        if let Err((error, rsp_tx)) = self.queue(gossiped_tx, None, rsp_tx) {
            rsp_tx.send(Verdict::Dropped {
                reason: error.to_string(),
            });
        }
    }

    /// Queue a transaction for download (if needed) and verification, sending its verdict to
    /// `rsp_tx`. If it can't be queued, returns why, along with `rsp_tx`.
    #[instrument(skip(self, gossiped_tx, rsp_tx), fields(txid = %gossiped_tx.id()))]
    #[allow(clippy::unwrap_in_result)]
    fn queue(
        &mut self,
        gossiped_tx: Gossip,
        source: Option<PeerSource>,
        rsp_tx: VerdictSender,
    ) -> Result<(), (MempoolError, VerdictSender)> {
        let txid = gossiped_tx.id();

        if self.cancel_handles.contains_key(&txid) {
            debug!(
                ?txid,
                queue_len = self.pending.len(),
                ?MAX_INBOUND_CONCURRENCY,
                "transaction id already queued for inbound download: ignored transaction"
            );
            metrics::gauge!("mempool.currently.queued.transactions",)
                .set(self.pending.len() as f64);

            return Err((MempoolError::AlreadyQueued, rsp_tx));
        }

        if self.pending.len() >= MAX_INBOUND_CONCURRENCY {
            debug!(
                ?txid,
                queue_len = self.pending.len(),
                ?MAX_INBOUND_CONCURRENCY,
                "too many transactions queued for inbound download: ignored transaction"
            );
            metrics::gauge!("mempool.currently.queued.transactions",)
                .set(self.pending.len() as f64);

            return Err((MempoolError::FullQueue, rsp_tx));
        }

        // Per-peer cap: a single advertising peer cannot saturate the queue
        // with attacker-supplied fake txids. See `GHSA-4fc2-h7jh-287c`.
        if let Some(source) = source {
            let count = self.pending_per_peer.get(&source).copied().unwrap_or(0);
            let max_in_flight = self.max_in_flight(source);
            if count >= max_in_flight {
                debug!(
                    ?txid,
                    ?source,
                    peer_queue_len = count,
                    max_in_flight,
                    "too many transactions queued for this peer: ignored transaction"
                );
                metrics::counter!("mempool.full_queue.per_peer.total").increment(1);
                return Err((MempoolError::FullQueue, rsp_tx));
            }
            if matches!(source, PeerSource::Stp(_)) {
                // At most MAX_INBOUND_CONCURRENCY entries, so counting is cheap.
                let all_stp: usize = self
                    .pending_per_peer
                    .iter()
                    .filter(|(source, _)| matches!(source, PeerSource::Stp(_)))
                    .map(|(_, count)| *count)
                    .sum();
                if all_stp >= MAX_INBOUND_CONCURRENCY_ALL_STP_PEERS {
                    debug!(
                        ?txid,
                        all_stp,
                        ?MAX_INBOUND_CONCURRENCY_ALL_STP_PEERS,
                        "too many transactions queued for STP peers: ignored transaction"
                    );
                    metrics::counter!("mempool.full_queue.all_stp.total").increment(1);
                    return Err((MempoolError::FullQueue, rsp_tx));
                }
            }
        }

        // This oneshot is used to signal cancellation to the download task.
        let (cancel_tx, mut cancel_rx) = oneshot::channel::<CancelDownloadAndVerify>();

        let network = self.network.clone();
        let verifier = self.verifier.clone();
        let mut state = self.state.clone();
        // Only legacy peers are reported to the address book. An STP peer's verdicts go to
        // the relay, which decides what to do about the peer.
        let pushed_advertiser_addr = match source {
            Some(PeerSource::Legacy(addr)) => Some(PeerSocketAddr::from(addr)),
            Some(PeerSource::Stp(_)) | None => None,
        };

        let gossiped_tx_req = gossiped_tx.clone();

        let fut = async move {
            // Don't download/verify if the transaction is already in the best chain.
            Self::transaction_in_best_chain(&mut state, txid).await?;

            trace!(?txid, "transaction is not in best chain");

            let (tip_height, next_height) = match state.oneshot(zs::Request::Tip).await {
                Ok(zs::Response::Tip(None)) => Ok((None, Height(0))),
                Ok(zs::Response::Tip(Some((height, _hash)))) => {
                    let next_height =
                        (height + 1).expect("valid heights are far below the maximum");
                    Ok((Some(height), next_height))
                }
                Ok(_) => unreachable!("wrong response"),
                Err(e) => Err(TransactionDownloadVerifyError::StateError(e.into())),
            }?;

            trace!(?txid, ?next_height, "got next height");

            // A `Gossip::Tx` was checked for conflicts with mempool transactions before it was
            // queued (`Mempool::queue_candidate`). A `Gossip::Id` can't be checked until it is
            // downloaded, so it is only checked when the mempool inserts it, after verification.
            // The legacy network that sends IDs caps each peer at
            // `MAX_INBOUND_CONCURRENCY_PER_PEER`, which bounds that cost.
            let (tx, advertiser_addr) = match gossiped_tx {
                Gossip::Id(txid) => {
                    let req = zn::Request::TransactionsById(std::iter::once(txid).collect());

                    let tx = match network
                        .oneshot(req)
                        .await
                        .map_err(CloneError::from)
                        .map_err(TransactionDownloadVerifyError::DownloadFailed)?
                    {
                        zn::Response::Transactions(mut txs) => txs.pop().ok_or_else(|| {
                            TransactionDownloadVerifyError::DownloadFailed(
                                BoxError::from("no transactions returned").into(),
                            )
                        })?,
                        _ => unreachable!("wrong response to transaction request"),
                    };

                    let (tx, advertiser_addr) = tx.available().expect(
                        "unexpected missing tx status: single tx failures should be errors",
                    );

                    metrics::counter!(
                        "mempool.downloaded.transactions.total",
                        "version" => format!("{}",tx.transaction.version()),
                    ).increment(1);
                    (tx, advertiser_addr)
                }
                Gossip::Tx(tx) => {
                    metrics::counter!(
                        "mempool.pushed.transactions.total",
                        "version" => format!("{}",tx.transaction.version()),
                    ).increment(1);
                    (tx, pushed_advertiser_addr)
                }
            };

            trace!(?txid, "got tx");

            let transaction_upgrade = tx.transaction.network_upgrade();
            let result = verifier
                .oneshot(tx::MempoolRequest {
                    transaction: tx.clone(),
                    height: next_height,
                })
                .map_ok(|rsp| {
                    let tx::MempoolResponse { transaction, spent_mempool_outpoints } = rsp;

                    (transaction, spent_mempool_outpoints, tip_height)
                })
                .await;

            // Hide the transaction data to avoid filling the logs
            trace!(?txid, result = ?result.as_ref().map(|_tx| ()), "verified transaction for the mempool");

            result.map_err(|error| TransactionDownloadVerifyError::Invalid {
                error: error.into(),
                advertiser_addr,
                transaction_upgrade,
                verification_height: next_height,
            })
        }
        .map_ok(|(tx, spent_mempool_outpoints, tip_height)| {
            metrics::counter!(
                "mempool.verified.transactions.total",
                "version" => format!("{}", tx.transaction.transaction.version()),
            ).increment(1);
            (tx, spent_mempool_outpoints, tip_height)
        })
        // Tack the hash onto the error so we can remove the cancel handle
        // on failure as well as on success.
        .map_err(move |e| Box::new((e, txid)))
        .inspect(move |result| {
            // Hide the transaction data to avoid filling the logs
            let result = result.as_ref().map(|_tx| txid);
            debug!("mempool transaction result: {result:?}");
        })
        .in_current_span();

        let task = tokio::spawn(async move {
            let fut = tokio::time::timeout(RATE_LIMIT_DELAY, fut);

            // Prefer the cancel handle if both are ready.
            //
            // Failures are answered here. A verified transaction carries `rsp_tx` to the
            // mempool, which answers once it tries to store it.
            tokio::select! {
                biased;
                _ = &mut cancel_rx => {
                    trace!("task cancelled prior to completion");
                    metrics::counter!("mempool.cancelled.verify.tasks.total").increment(1);
                    rsp_tx.send(Verdict::Cancelled);

                    Ok(Err(Box::new((TransactionDownloadVerifyError::Cancelled, txid))))
                }
                verification = fut => match verification {
                    Err(elapsed) => {
                        rsp_tx.send(Verdict::Timeout);
                        Err((txid, elapsed))
                    }
                    Ok(Ok((transaction, spent_mempool_outpoints, tip_height))) => {
                        Ok(Ok((transaction, spent_mempool_outpoints, tip_height, rsp_tx)))
                    }
                    Ok(Err(boxed_err)) => {
                        rsp_tx.send(boxed_err.0.verdict());
                        Ok(Err(boxed_err))
                    }
                },
            }
        });

        self.pending.push(task);
        assert!(
            self.cancel_handles
                .insert(txid, (cancel_tx, gossiped_tx_req, source))
                .is_none(),
            "transactions are only queued once"
        );
        if let Some(source) = source {
            // The per-peer cap check above ensures this can't exceed `max_in_flight(source)`.
            *self.pending_per_peer.entry(source).or_insert(0) += 1;
        }

        debug!(
            ?txid,
            queue_len = self.pending.len(),
            ?MAX_INBOUND_CONCURRENCY,
            "queued transaction hash for download"
        );
        metrics::gauge!("mempool.currently.queued.transactions",).set(self.pending.len() as f64);
        metrics::counter!("mempool.queued.transactions.total").increment(1);

        Ok(())
    }

    /// Cancel download/verification tasks of transactions with the
    /// given transaction hash (see [`UnminedTxId::mined_id`]).
    pub fn cancel(&mut self, mined_ids: &HashSet<transaction::Hash>) {
        // TODO: this can be simplified with [`HashMap::drain_filter`] which
        // is currently nightly-only experimental API.
        let removed_txids: Vec<UnminedTxId> = self
            .cancel_handles
            .keys()
            .filter(|txid| mined_ids.contains(&txid.mined_id()))
            .cloned()
            .collect();

        for txid in removed_txids {
            if let Some((cancel_tx, _gossip, source)) = self.cancel_handles.remove(&txid) {
                let _ = cancel_tx.send(CancelDownloadAndVerify);
                if let Some(source) = source {
                    Self::release_peer_slot(&mut self.pending_per_peer, source);
                }
            }
        }
    }

    /// Cancel all running tasks and reset the downloader state.
    // Note: copied from zebrad/src/components/sync/downloads.rs
    pub fn cancel_all(&mut self) {
        // Replace the pending task list with an empty one and drop it.
        let _ = std::mem::take(&mut self.pending);
        // Signal cancellation to all running tasks.
        // Since we already dropped the JoinHandles above, they should
        // fail silently.
        for (_hash, (cancel_tx, _gossip, _source)) in self.cancel_handles.drain() {
            let _ = cancel_tx.send(CancelDownloadAndVerify);
        }
        self.pending_per_peer.clear();
        assert!(self.pending.is_empty());
        assert!(self.cancel_handles.is_empty());
        metrics::gauge!("mempool.currently.queued.transactions",).set(self.pending.len() as f64);
    }

    /// Decrement the per-peer pending count for `source`, removing the entry
    /// when it reaches zero.
    fn release_peer_slot(pending_per_peer: &mut HashMap<PeerSource, usize>, source: PeerSource) {
        if let Some(count) = pending_per_peer.get_mut(&source) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                pending_per_peer.remove(&source);
            }
        }
    }

    /// Get the number of currently in-flight download tasks.
    #[allow(dead_code)]
    pub fn in_flight(&self) -> usize {
        self.pending.len()
    }

    /// Get a list of the currently pending transaction requests.
    pub fn transaction_requests(&self) -> impl Iterator<Item = &Gossip> {
        self.cancel_handles
            .iter()
            .map(|(_tx_id, (_handle, tx, _source))| tx)
    }

    /// Check if transaction is already in the best chain.
    async fn transaction_in_best_chain(
        state: &mut ZS,
        txid: UnminedTxId,
    ) -> Result<(), TransactionDownloadVerifyError> {
        match state
            .ready()
            .await
            .map_err(CloneError::from)
            .map_err(TransactionDownloadVerifyError::StateError)?
            .call(zs::Request::Transaction(txid.mined_id()))
            .await
        {
            Ok(zs::Response::Transaction(None)) => Ok(()),
            Ok(zs::Response::Transaction(Some(_))) => Err(TransactionDownloadVerifyError::InState),
            Ok(_) => unreachable!("wrong response"),
            Err(e) => Err(TransactionDownloadVerifyError::StateError(e.into())),
        }?;

        Ok(())
    }
}

#[pinned_drop]
impl<ZN, ZV, ZS> PinnedDrop for Downloads<ZN, ZV, ZS>
where
    ZN: Service<zn::Request, Response = zn::Response, Error = BoxError> + Send + Clone + 'static,
    ZN::Future: Send,
    ZV: Service<tx::MempoolRequest, Response = tx::MempoolResponse, Error = BoxError>
        + Send
        + Clone
        + 'static,
    ZV::Future: Send,
    ZS: Service<zs::Request, Response = zs::Response, Error = BoxError> + Send + Clone + 'static,
    ZS::Future: Send,
{
    fn drop(mut self: Pin<&mut Self>) {
        self.cancel_all();

        metrics::gauge!("mempool.currently.queued.transactions").set(0 as f64);
    }
}

#[cfg(test)]
mod tests;
