//! The mempool bridge: the only place transaction relay touches the mempool's tower service.
//!
//! Tokio tasks own the mempool handle and its change stream; the sync loop talks to them over
//! channels and never awaits: requests go in with `try_send` (dropped when the bridge is busy),
//! answers come back on a channel it drains each tick. Transaction bodies are decoded here, off
//! the sync thread, so relay traffic cannot stall PoW or BFT sync.

use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tenderlink::stp::ConnectionKey;
use tokio::sync::{broadcast, mpsc, Semaphore};
use tower::{Service, ServiceExt};
use zebra_chain::transaction::{Transaction, UnminedTx, UnminedTxId};
use zebra_node_services::mempool::{self, Gossip, MempoolChangeKind, PeerSource, QueueRefusal, Verdict};

use super::tx_relay::{read_txs, MempoolService, TxRelayLaunch};
use super::tx_relay_state::Outcome;

/// Assumption: every transaction relay hands the mempool (returned by
/// `TxRelay::on_delivered`) gets exactly one `TxRelay::on_outcome`, whatever happens: a busy
/// bridge, a mempool that does not answer, a dropped verdict. Otherwise it stays marked
/// submitted and is never asked for again until given up.
pub(super) const ASSUME_ONE_OUTCOME_PER_SUBMITTED: () = ();
/// Assumption: only a verification failure that cannot depend on our state (the mempool's
/// `Verdict::Invalid { state_dependent: false }`) becomes `Outcome::Invalid`, which strikes.
pub(super) const ASSUME_INVALID_IS_STATE_INDEPENDENT: () = ();
/// Assumption: a `TX` body is only decoded if its sender has asks outstanding
/// (`TxRelay::has_requested`), and undecoded bodies queued here are bounded by
/// [`MAX_QUEUED_BODY_BYTES`], and per sender by what it was asked for plus one message
/// (the sync loop's count, decremented by the `Decoded`/`Malformed` answer); decoding is on
/// the bridge, not the sync thread.
pub(super) const ASSUME_UNASKED_TX_NOT_DECODED: () = ();
/// Assumption: every connection key relay is told about gets `TxRelay::on_peer_gone` when it
/// disconnects, and only live connections' keys are passed in, so per-peer state is bounded
/// by the connection caps.
pub(super) const ASSUME_PEER_GONE_FOR_EVERY_KEY: () = ();

/// Requests queued to the bridge at most; past it they are dropped.
const BRIDGE_QUEUE: usize = 256;
/// Bytes of undecoded transaction bodies queued to the bridge at most.
pub(super) const MAX_QUEUED_BODY_BYTES: usize = 16 << 20;
/// Requests the bridge works on at once.
const BRIDGE_CONCURRENCY: usize = 16;
/// How long a mempool request (other than awaiting a verdict) may take.
const MEMPOOL_ANSWER_WAIT: Duration = Duration::from_secs(30);
/// How long a verdict is awaited; the mempool's own verification timeout is shorter.
const VERDICT_WAIT: Duration = Duration::from_secs(600);

pub(super) enum ToBridge {
    /// Of these listed entries, those the mempool has not rejected.
    Filter { from: ConnectionKey, entries: Vec<(UnminedTxId, u32)> },
    /// A `TX` body from `from`, to decode.
    Decode { from: ConnectionKey, body: Vec<u8> },
    /// Transactions `from` delivered that we asked it for, to queue for verification.
    Submit { from: ConnectionKey, source: [u8; 32], txs: Vec<Arc<Transaction>> },
    /// Bodies to serve `to`.
    Fetch { to: ConnectionKey, ids: Vec<UnminedTxId> },
}

pub(super) enum FromBridge {
    /// Our mempool gained these, with their serialized sizes.
    Added(Vec<(UnminedTxId, u32)>),
    /// Our mempool lost these.
    Removed(Vec<UnminedTxId>),
    /// Our mempool, all of it (at start, and after the change stream lagged).
    Reset(Vec<(UnminedTxId, u32)>),
    Filtered { from: ConnectionKey, keep: Vec<(UnminedTxId, u32)> },
    /// A decoded `TX` body of `body_len` bytes: each transaction with its id and serialized length.
    Decoded { from: ConnectionKey, body_len: usize, txs: Vec<(UnminedTxId, u32, Arc<Transaction>)> },
    /// A `TX` body of `body_len` bytes that is not well formed: the sender is to be killed.
    Malformed { from: ConnectionKey, body_len: usize },
    Outcome { from: ConnectionKey, id: UnminedTxId, outcome: Outcome },
    Bodies { to: ConnectionKey, txs: Vec<UnminedTx> },
}

/// The sync loop's end of the bridge.
pub(super) struct TxBridge {
    to: mpsc::Sender<ToBridge>,
    from: std::sync::mpsc::Receiver<FromBridge>,
    queued_body_bytes: Arc<AtomicUsize>,
}

impl TxBridge {
    /// Queue a request; `false` if the bridge is busy (then it is dropped).
    pub fn send(&self, request: ToBridge) -> bool {
        if let ToBridge::Decode { body, .. } = &request {
            let len = body.len();
            if self.queued_body_bytes.load(Ordering::Relaxed) + len > MAX_QUEUED_BODY_BYTES {
                return false;
            }
            self.queued_body_bytes.fetch_add(len, Ordering::Relaxed);
            if self.to.try_send(request).is_err() {
                self.queued_body_bytes.fetch_sub(len, Ordering::Relaxed);
                return false;
            }
            return true;
        }
        self.to.try_send(request).is_ok()
    }

    /// The answers ready now.
    pub fn answers(&self) -> impl Iterator<Item = FromBridge> + '_ {
        self.from.try_iter()
    }
}

/// How the mempool's typed answers map onto what relay does with the sender. Only a failure
/// that cannot depend on our state is the peer's fault.
fn outcome_of_refusal(refusal: &QueueRefusal) -> Outcome {
    match refusal {
        QueueRefusal::AlreadyKnown => Outcome::Accepted,
        QueueRefusal::Full => Outcome::Busy,
        QueueRefusal::Rejected { .. } | QueueRefusal::SpendConflict | QueueRefusal::Disabled => Outcome::NotTaken,
    }
}

fn outcome_of_verdict(verdict: &Verdict) -> Outcome {
    let () = ASSUME_INVALID_IS_STATE_INDEPENDENT;
    match verdict {
        Verdict::Accepted | Verdict::AlreadyKnown => Outcome::Accepted,
        Verdict::Invalid { state_dependent: false, .. } => Outcome::Invalid,
        Verdict::Invalid { state_dependent: true, .. } | Verdict::Dropped { .. } | Verdict::Timeout | Verdict::Cancelled => Outcome::NotTaken,
    }
}

async fn call(mempool: &MempoolService, request: mempool::Request) -> Option<mempool::Response> {
    let mut mempool = mempool.clone();
    let answer = tokio::time::timeout(MEMPOOL_ANSWER_WAIT, async { mempool.ready().await?.call(request).await }).await;
    answer.ok()?.ok()
}

/// Ids with their serialized sizes, for those of `ids` still in the mempool.
async fn sized(mempool: &MempoolService, ids: HashSet<UnminedTxId>) -> Vec<(UnminedTxId, u32)> {
    match call(mempool, mempool::Request::TransactionsById(ids)).await {
        // A mempool transaction's size fits u32: it is at most MAX_BLOCK_BYTES.
        Some(mempool::Response::Transactions(txs)) => txs.iter().map(|tx| (tx.id, tx.size as u32)).collect(),
        _ => Vec::new(),
    }
}

async fn everything(mempool: &MempoolService) -> Vec<(UnminedTxId, u32)> {
    match call(mempool, mempool::Request::TransactionIds).await {
        Some(mempool::Response::TransactionIds(ids)) => sized(mempool, ids).await,
        _ => Vec::new(),
    }
}

/// Start the bridge on `rt`.
pub(super) fn spawn(rt: &tokio::runtime::Handle, launch: TxRelayLaunch) -> TxBridge {
    let (to_tx, mut to_rx) = mpsc::channel::<ToBridge>(BRIDGE_QUEUE);
    // Unbounded, but every answer answers a bounded request or a mempool change.
    let (from_tx, from_rx) = std::sync::mpsc::channel::<FromBridge>();
    let queued_body_bytes = Arc::new(AtomicUsize::new(0));
    let TxRelayLaunch { mempool, changes } = launch;

    // Mirror the mempool: all of it at start and after a lag, then its changes.
    {
        let mempool = mempool.clone();
        let from_tx = from_tx.clone();
        let mut changes = changes.subscribe();
        rt.spawn(async move {
            if from_tx.send(FromBridge::Reset(everything(&mempool).await)).is_err() {
                return;
            }
            loop {
                let answer = match changes.recv().await {
                    Ok(change) => match change.kind() {
                        MempoolChangeKind::Added => FromBridge::Added(sized(&mempool, change.tx_ids).await),
                        MempoolChangeKind::Invalidated | MempoolChangeKind::Mined => FromBridge::Removed(change.tx_ids.into_iter().collect()),
                    },
                    Err(broadcast::error::RecvError::Lagged(_)) => FromBridge::Reset(everything(&mempool).await),
                    Err(broadcast::error::RecvError::Closed) => return,
                };
                if from_tx.send(answer).is_err() {
                    return;
                }
            }
        });
    }

    // Requests, at most BRIDGE_CONCURRENCY at once.
    {
        let queued_body_bytes = queued_body_bytes.clone();
        let permits = Arc::new(Semaphore::new(BRIDGE_CONCURRENCY));
        rt.spawn(async move {
            while let Some(request) = to_rx.recv().await {
                let Ok(permit) = permits.clone().acquire_owned().await else { return };
                let mempool = mempool.clone();
                let from_tx = from_tx.clone();
                let queued_body_bytes = queued_body_bytes.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    serve(request, &mempool, &from_tx, &queued_body_bytes).await;
                });
            }
        });
    }

    TxBridge { to: to_tx, from: from_rx, queued_body_bytes }
}

async fn serve(request: ToBridge, mempool: &MempoolService, from_tx: &std::sync::mpsc::Sender<FromBridge>, queued_body_bytes: &AtomicUsize) {
    match request {
        ToBridge::Filter { from, entries } => {
            let ids: HashSet<UnminedTxId> = entries.iter().map(|(id, _)| *id).collect();
            // If the mempool does not answer, nothing is kept: no ask without its say.
            let keep = match call(mempool, mempool::Request::RejectedTransactionIds(ids)).await {
                Some(mempool::Response::RejectedTransactionIds(rejected)) => entries.into_iter().filter(|(id, _)| !rejected.contains(id)).collect(),
                _ => Vec::new(),
            };
            let _ = from_tx.send(FromBridge::Filtered { from, keep });
        }
        ToBridge::Decode { from, body } => {
            let body_len = body.len();
            queued_body_bytes.fetch_sub(body_len, Ordering::Relaxed);
            let answer = match read_txs(&body) {
                Some(txs) => FromBridge::Decoded { from, body_len, txs: txs.into_iter().map(|(tx, len)| (UnminedTxId::from(&*tx), len, tx)).collect() },
                None => FromBridge::Malformed { from, body_len },
            };
            let _ = from_tx.send(answer);
        }
        ToBridge::Submit { from, source, txs } => {
            let ids: Vec<UnminedTxId> = txs.iter().map(|tx| UnminedTxId::from(&**tx)).collect();
            let candidates = txs.into_iter().map(|tx| Gossip::Tx(UnminedTx::from(tx))).collect();
            let request = mempool::Request::QueueFromPeer { candidates, source: PeerSource::Stp(source) };
            let Some(mempool::Response::Queued(results)) = call(mempool, request).await else {
                for id in ids {
                    let _ = from_tx.send(FromBridge::Outcome { from, id, outcome: Outcome::NotTaken });
                }
                return;
            };
            // One answer per candidate, in order; any id the mempool did not answer for is not
            // taken.
            let () = ASSUME_ONE_OUTCOME_PER_SUBMITTED;
            let mut results = results.into_iter();
            let mut verdicts = Vec::new();
            for id in ids {
                match results.next() {
                    Some(Ok(verdict)) => verdicts.push((id, verdict)),
                    Some(Err(refusal)) => {
                        let _ = from_tx.send(FromBridge::Outcome { from, id, outcome: outcome_of_refusal(&refusal) });
                    }
                    None => {
                        let _ = from_tx.send(FromBridge::Outcome { from, id, outcome: Outcome::NotTaken });
                    }
                }
            }
            // Awaited apart from the request's permit, so slow verification does not hold up
            // filtering and serving. Bounded by the mempool's per-source cap per peer.
            let from_tx = from_tx.clone();
            tokio::spawn(async move {
                for (id, verdict) in verdicts {
                    let outcome = match tokio::time::timeout(VERDICT_WAIT, verdict).await {
                        Ok(Ok(verdict)) => outcome_of_verdict(&verdict),
                        // A dropped sender means cancelled; no answer in time is not taken.
                        _ => Outcome::NotTaken,
                    };
                    let _ = from_tx.send(FromBridge::Outcome { from, id, outcome });
                }
            });
        }
        ToBridge::Fetch { to, ids } => {
            let txs = match call(mempool, mempool::Request::TransactionsById(ids.into_iter().collect())).await {
                Some(mempool::Response::Transactions(txs)) => txs,
                _ => Vec::new(),
            };
            let _ = from_tx.send(FromBridge::Bodies { to, txs });
        }
    }
}
