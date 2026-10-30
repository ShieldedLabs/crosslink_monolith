//! The Zebra mempool.
//!
//! A service that manages known unmined Zcash transactions.

use std::{collections::HashSet, fmt, net::SocketAddr};

use tokio::sync::oneshot;
use zebra_chain::{
    block,
    transaction::{self, UnminedTx, UnminedTxId, VerifiedUnminedTx},
    transparent,
};

use crate::BoxError;

mod gossip;
mod mempool_change;
mod service_trait;
mod transaction_dependencies;

pub use self::{
    gossip::Gossip,
    mempool_change::{MempoolChange, MempoolChangeKind, MempoolTxSubscriber},
    service_trait::MempoolService,
    transaction_dependencies::TransactionDependencies,
};

/// The peer a queued candidate transaction came from.
///
/// The mempool caps how many of each source's candidates it downloads and verifies at once, so
/// one peer cannot crowd out the others.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum PeerSource {
    /// A legacy `zebra-network` peer, by address.
    ///
    /// Capped at `MAX_INBOUND_CONCURRENCY_PER_PEER` candidates in flight. Transactions it
    /// sent that fail verification are reported to the address book as misbehaviour.
    Legacy(SocketAddr),

    /// A peer on our own STP network, by its 32-byte static public key.
    ///
    /// Capped at the mempool's per-STP-peer limit. Never reported to the legacy address book:
    /// the relay decides what to do about the peer from each candidate's verdict.
    Stp([u8; 32]),
}

/// Why the mempool refused to queue a candidate transaction, before downloading or verifying
/// it.
///
/// # Security
///
/// Every refusal depends on our local state: the mempool's contents, its queue, its cached
/// rejections, and whether it is active. Callers must never penalise a peer for a refusal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum QueueRefusal {
    /// The transaction is already in the mempool, or already queued for verification.
    AlreadyKnown,

    /// The mempool has a cached rejection of this transaction, or of one with the same
    /// effects, at the current chain tip or chain. `reason` is for logs only: the cache is
    /// local state, so it is not a verdict on the peer.
    Rejected {
        /// Why the transaction was rejected, for logs.
        reason: String,
    },

    /// The download and verify queue, or this source's share of it, is full.
    Full,

    /// The transaction conflicts with one already in the mempool: it spends the same
    /// transparent outpoint, reveals the same Sprout, Sapling, Orchard or Ironwood nullifier,
    /// acts on the same bond, or converts the same finalizer's reward bank.
    ///
    /// Checked only for full transactions ([`Gossip::Tx`]), before any proof is verified, so a
    /// spender of one real note can't make us verify many conflicting transactions. Whether
    /// two transactions conflict depends on which one we saw first, so this is never grounds
    /// to penalise a peer.
    SpendConflict,

    /// The mempool is not active yet, because the node is still syncing.
    Disabled,
}

impl fmt::Display for QueueRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            QueueRefusal::AlreadyKnown => {
                f.write_str("transaction is already in the mempool or queued for verification")
            }
            QueueRefusal::Rejected { reason } => {
                write!(f, "transaction was already rejected: {reason}")
            }
            QueueRefusal::Full => f.write_str("transaction dropped because the queue is full"),
            QueueRefusal::SpendConflict => f.write_str(
                "transaction conflicts with a mempool transaction: it spends the same inputs, \
                 reveals the same nullifiers, or acts on the same bond or reward bank",
            ),
            QueueRefusal::Disabled => {
                f.write_str("mempool is disabled since synchronization is behind the chain tip")
            }
        }
    }
}

impl std::error::Error for QueueRefusal {}

/// The outcome of downloading, verifying and storing a queued candidate transaction.
///
/// Every queued candidate gets exactly one verdict. A closed channel, which the mempool avoids,
/// means the same as [`Verdict::Cancelled`].
///
/// # Security
///
/// Only [`Verdict::Invalid`] with `state_dependent: false` says anything about the peer that
/// sent the transaction. Every other verdict can happen to an honest peer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Verdict {
    /// The transaction was verified and added to the mempool.
    Accepted,

    /// The transaction is already in the mempool, or already in the best chain.
    AlreadyKnown,

    /// The transaction was not added, for a reason that says nothing about its validity: it
    /// conflicted with a transaction that reached the mempool first, it was evicted, it has a
    /// cached rejection, it is non-standard, or we couldn't download it or read our state.
    Dropped {
        /// Why the transaction was dropped, for logs.
        reason: String,
    },

    /// Downloading or verifying the transaction took too long, which can depend on our load.
    Timeout,

    /// Verification was cancelled, because the transaction was mined or the mempool was reset
    /// (by a chain reorganisation or network upgrade). The mempool may retry the transaction
    /// itself, without sending another verdict.
    Cancelled,

    /// The transaction failed consensus or policy verification.
    Invalid {
        /// The verification error, for logs.
        error: String,

        /// Whether the failure could depend on our local state: our chain tip, clock, mempool
        /// contents, network upgrade schedule, or an internal failure. `false` only for
        /// failures no honest peer can trigger whatever our state is, such as an invalid proof
        /// or signature.
        state_dependent: bool,
    },
}

impl Verdict {
    /// Converts this verdict into the success or error result callers that predate
    /// [`Verdict`] expect: `Ok(())` only for [`Verdict::Accepted`].
    pub fn into_result(self) -> Result<(), BoxError> {
        match self {
            Verdict::Accepted => Ok(()),
            Verdict::AlreadyKnown => {
                Err("transaction already exists in mempool or best chain".into())
            }
            Verdict::Dropped { reason } => Err(format!("transaction dropped: {reason}").into()),
            Verdict::Timeout => Err("timeout waiting for verification result".into()),
            Verdict::Cancelled => Err("verification cancelled".into()),
            Verdict::Invalid { error, .. } => Err(format!("failed to validate tx: {error}").into()),
        }
    }
}

/// A mempool service request.
///
/// Requests can query the current set of mempool transactions,
/// queue transactions to be downloaded and verified, or
/// run the mempool to check for newly verified transactions.
///
/// Requests can't modify the mempool directly,
/// because all mempool transactions must be verified.
#[derive(Debug, Eq, PartialEq)]
pub enum Request {
    /// Query all [`UnminedTxId`]s in the mempool.
    TransactionIds,

    /// Query matching [`UnminedTx`] in the mempool,
    /// using a unique set of [`UnminedTxId`]s.
    TransactionsById(HashSet<UnminedTxId>),

    /// Query matching [`UnminedTx`] in the mempool,
    /// using a unique set of [`transaction::Hash`]es. Pre-V5 transactions are matched
    /// directly; V5 transaction are matched just by the Hash, disregarding
    /// the [`AuthDigest`](zebra_chain::transaction::AuthDigest).
    TransactionsByMinedId(HashSet<transaction::Hash>),

    /// Request a [`transparent::Output`] identified by the given [`OutPoint`](transparent::OutPoint),
    /// waiting until it becomes available if it is unknown.
    ///
    /// This request is purely informational, and there are no guarantees about
    /// whether the UTXO remains unspent or is on the best chain, or any chain.
    /// Its purpose is to allow orphaned mempool transaction verification.
    ///
    /// # Correctness
    ///
    /// Output requests should be wrapped in a timeout, so that
    /// out-of-order and invalid requests do not hang indefinitely.
    ///
    /// Outdated requests are pruned on a regular basis.
    AwaitOutput(transparent::OutPoint),

    /// Request a [`VerifiedUnminedTx`] and its dependencies by its mined id.
    TransactionWithDepsByMinedId(transaction::Hash),

    /// Get all the [`VerifiedUnminedTx`] in the mempool.
    ///
    /// Equivalent to `TransactionsById(TransactionIds)`,
    /// but each transaction also includes the `miner_fee` and `legacy_sigop_count` fields.
    //
    // TODO: make the Transactions response return VerifiedUnminedTx,
    //       and remove the FullTransactions variant
    FullTransactions,

    /// Query matching cached rejected transaction IDs in the mempool,
    /// using a unique set of [`UnminedTxId`]s.
    RejectedTransactionIds(HashSet<UnminedTxId>),

    /// Queue a list of gossiped transactions or transaction IDs, or
    /// crawled transaction IDs.
    ///
    /// The transaction downloader checks for duplicates across IDs and transactions.
    ///
    /// Answered with [`Response::Queued`]. Candidates have no source peer, so they are not
    /// capped per peer and never reported as misbehaviour.
    Queue(Vec<Gossip>),

    /// Queue candidate transactions received from a specific peer — either
    /// transaction IDs advertised via an `Inv` message, or a full transaction
    /// pushed via a `Tx` message — tagging each one with the sending peer so the
    /// downloader can enforce a per-peer queue cap. This routes every
    /// peer-originated candidate through the same per-peer admission accounting,
    /// so a single peer cannot crowd out honest peers' transaction relay.
    /// See `GHSA-4fc2-h7jh-287c` and `GHSA-m9xx-8rcj-vmgp`.
    ///
    /// Answered with [`Response::Queued`], one result per candidate, in order.
    QueueFromPeer {
        /// The candidate transactions received from the peer.
        candidates: Vec<Gossip>,
        /// The peer that sent them.
        source: PeerSource,
    },

    /// Check for newly verified transactions.
    ///
    /// The transaction downloader does not push transactions into the mempool.
    /// So a task should send this request regularly (every 5-10 seconds).
    ///
    /// These checks also happen for other request variants,
    /// but we can't rely on peers to send queries regularly,
    /// and crawler queue requests depend on peer responses.
    /// Also, crawler requests aren't frequent enough for transaction propagation.
    ///
    /// # Correctness
    ///
    /// This request is required to avoid hangs in the mempool.
    ///
    /// The queue checker task can't call `poll_ready` directly on the mempool
    /// service, because the service is wrapped in a `Buffer`. Calling
    /// `Buffer::poll_ready` reserves a buffer slot, which can cause hangs
    /// when too many slots are reserved but unused:
    /// <https://docs.rs/tower/0.4.10/tower/buffer/struct.Buffer.html#a-note-on-choosing-a-bound>
    CheckForVerifiedTransactions,

    /// Request summary statistics from the mempool for `getmempoolinfo`.
    QueueStats,

    /// Check whether a transparent output is spent in the mempool.
    UnspentOutput(transparent::OutPoint),
}

/// A response to a mempool service request.
///
/// Responses can read the current set of mempool transactions,
/// check the queued status of transactions to be downloaded and verified, or
/// confirm that the mempool has been checked for newly verified transactions.
#[derive(Debug)]
pub enum Response {
    /// Returns all [`UnminedTxId`]s from the mempool.
    TransactionIds(HashSet<UnminedTxId>),

    /// Returns matching [`UnminedTx`] from the mempool.
    ///
    /// Since the [`Request::TransactionsById`] request is unique,
    /// the response transactions are also unique. The same applies to
    /// [`Request::TransactionsByMinedId`] requests, since the mempool does not allow
    /// different transactions with different mined IDs.
    Transactions(Vec<UnminedTx>),

    /// Response to [`Request::AwaitOutput`] with the transparent output
    UnspentOutput(transparent::Output),

    /// Response to [`Request::TransactionWithDepsByMinedId`].
    TransactionWithDeps {
        /// The queried transaction
        transaction: VerifiedUnminedTx,
        /// A list of dependencies of the queried transaction.
        dependencies: HashSet<transaction::Hash>,
    },

    /// Returns all [`VerifiedUnminedTx`] in the mempool.
    //
    // TODO: make the Transactions response return VerifiedUnminedTx,
    //       and remove the FullTransactions variant
    FullTransactions {
        /// All [`VerifiedUnminedTx`]s in the mempool
        transactions: Vec<VerifiedUnminedTx>,

        /// All transaction dependencies in the mempool
        transaction_dependencies: TransactionDependencies,

        /// Last seen chain tip hash by mempool service
        last_seen_tip_hash: zebra_chain::block::Hash,
    },

    /// Returns matching cached rejected [`UnminedTxId`]s from the mempool,
    RejectedTransactionIds(HashSet<UnminedTxId>),

    /// The response to [`Request::Queue`] and [`Request::QueueFromPeer`]: for each candidate,
    /// either why it was refused at queue time, or a receiver for its [`Verdict`].
    ///
    /// Each result matches the request at the corresponding vector index.
    Queued(Vec<Result<oneshot::Receiver<Verdict>, QueueRefusal>>),

    /// Confirms that the mempool has checked for recently verified transactions.
    CheckedForVerifiedTransactions,

    /// Summary statistics for the mempool: count, total size, memory usage, and regtest info.
    QueueStats {
        /// Number of transactions currently in the mempool
        size: usize,
        /// Total size in bytes of all transactions
        bytes: usize,
        /// Estimated memory usage in bytes
        usage: usize,
        /// Whether all transactions have been fully notified (regtest only)
        fully_notified: Option<bool>,
    },

    /// Returns whether a transparent output is created or spent in the mempool, if present.
    TransparentOutput(Option<CreatedOrSpent>),
}

/// Indicates whether an output was created or spent by a mempool transaction.
#[derive(Debug)]
pub enum CreatedOrSpent {
    /// An unspent output that was created by a transaction in the mempool and not spent by any other mempool tx.
    Created {
        /// The output
        output: transparent::Output,
        /// The version
        tx_version: u32,
        /// The last seen hash
        last_seen_hash: block::Hash,
    },
    /// Indicates that an output was spent by a mempool transaction.
    Spent,
}
