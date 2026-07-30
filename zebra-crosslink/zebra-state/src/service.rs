//! [`tower::Service`]s for Zebra's cached chain state.
//!
//! Zebra provides cached state access via two main services:
//! - [`StateService`]: a read-write service that writes blocks to the state,
//!   and redirects most read requests to the [`ReadStateService`].
//! - [`ReadStateService`]: a read-only service that answers from the most
//!   recent committed block.
//!
//! Most users should prefer [`ReadStateService`], unless they need to write blocks to the state.
//!
//! Zebra also provides access to the best chain tip via:
//! - [`LatestChainTip`]: a read-only channel that contains the latest committed
//!   tip.
//! - [`ChainTipChange`]: a read-only channel that can asynchronously await
//!   chain tip changes.

use std::{
    collections::HashMap,
    convert,
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::{Duration, Instant},
};

use futures::future::FutureExt;
use tokio::sync::{oneshot, watch};
use tower::{util::BoxService, Service, ServiceExt};
use tracing::{instrument, Instrument, Span};
use derivative::Derivative;

#[cfg(any(test, feature = "proptest-impl"))]
use tower::buffer::Buffer;

use zebra_chain::{
    block::{self, CountedHeader, HeightDiff},
    diagnostic::{task::WaitForPanics, CodeTimer},
    parameters::{HardForkSchedule, Network, NetworkUpgrade},
    subtree::NoteCommitmentSubtreeIndex,
};

use zebra_chain::{block::Height, serialization::ZcashSerialize};
use zcash_primitives::bft::FatPointerToBftBlock;

use crate::{
    constants::{
        MAX_FIND_BLOCK_HASHES_RESULTS, MAX_FIND_BLOCK_HEADERS_RESULTS, MAX_LEGACY_CHAIN_BLOCKS,
    },
    response::{BondInfoResponse, KnownBlock, NonFinalizedBlocksListener},
    service::{
        block_iter::any_ancestor_blocks,
        chain_tip::{ChainTipBlock, ChainTipChange, ChainTipSender, LatestChainTip},
        finalized_state::{FinalizedState, ZebraDb},
        non_finalized_state::{Chain, NonFinalizedState},
        pending_utxos::PendingUtxos,
        watch_receiver::WatchReceiver,
    },
    BoxError, CheckpointVerifiedBlock, CommitSemanticallyVerifiedError, Config, ReadRequest,
    ReadResponse, Request, Response, SemanticallyVerifiedBlock, ValidateContextError,
};

pub mod block_iter;
pub mod chain_tip;
pub mod watch_receiver;

pub mod check;

pub(crate) mod finalized_state;
pub(crate) mod non_finalized_state;
mod pending_utxos;
mod queued_blocks;
pub(crate) mod read;
pub mod write;

#[cfg(any(test, feature = "proptest-impl"))]
pub mod arbitrary;

#[cfg(test)]
mod tests;

pub use finalized_state::{OutputLocation, TransactionIndex, TransactionLocation};
use write::NonFinalizedWriteMessage;

use self::queued_blocks::{QueuedCheckpointVerified, QueuedSemanticallyVerified};

/// A read-write service for Zebra's cached blockchain state.
///
/// This service modifies and provides access to:
/// - the non-finalized state: the ~100 most recent blocks.
///   Zebra allows chain forks in the non-finalized state,
///   stores it in memory, and re-downloads it when restarted.
/// - the finalized state: older blocks that have many confirmations.
///   Zebra stores the single best chain in the finalized state,
///   and re-loads it from disk when restarted.
///
/// Read requests to this service are buffered, then processed concurrently.
/// Block write requests are buffered, then queued, then processed in order by a separate task.
///
/// Most state users can get faster read responses using the [`ReadStateService`],
/// because its requests do not share a [`tower::buffer::Buffer`] with block write requests.
///
/// To quickly get the latest block, use [`LatestChainTip`] or [`ChainTipChange`].
/// They can read the latest block directly, without queueing any requests.
#[derive(Derivative)]
#[derivative(Debug)]
pub(crate) struct StateService {
    // Configuration
    //
    /// The configured Zcash network.
    network: Network,

    /// The height that we start storing UTXOs from finalized blocks.
    ///
    /// This height should be lower than the last few checkpoints,
    /// so the full verifier can verify UTXO spends from those blocks,
    /// even if they haven't been committed to the finalized state yet.

    // Queued Blocks
    //
    /// Queued blocks for the [`NonFinalizedState`] that arrived out of order.
    /// These blocks are awaiting their parent blocks before they can do contextual verification.

    /// Queued blocks for the [`FinalizedState`] that arrived out of order.
    /// These blocks are awaiting their parent blocks before they can do contextual verification.
    ///
    /// Indexed by their parent block hash.

    /// Channels to send blocks to the block write task.

    /// The [`block::Hash`] of the most recent block sent on
    /// `finalized_block_write_sender` or `non_finalized_block_write_sender`.
    ///
    /// On startup, this is:
    /// - the finalized tip, if there are stored blocks, or
    /// - the genesis block's parent hash, if the database is empty.
    ///
    /// If `invalid_block_write_reset_receiver` gets a reset, this is:
    /// - the hash of the last valid committed block (the parent of the invalid block).

    /// A set of block hashes that have been sent to the block write task.
    /// Hashes of blocks below the finalized tip height are periodically pruned.

    /// If an invalid block is sent on `finalized_block_write_sender`
    /// or `non_finalized_block_write_sender`,
    /// this channel gets the [`block::Hash`] of the valid tip.
    //
    // TODO: add tests for finalized and non-finalized resets (#2654)

    // Pending UTXO Request Tracking
    //
    /// The set of outpoints with pending requests for their associated transparent::Output.
    pending_utxos: PendingUtxos,

    /// Instant tracking the last time `pending_utxos` was pruned.
    last_prune: Instant,

    // Updating Concurrently Readable State
    //
    /// A cloneable [`ReadStateService`], used to answer concurrent read requests.
    ///
    /// TODO: move users of read [`Request`]s to [`ReadStateService`], and remove `read_service`.
    read_service: ReadStateService,

    // Metrics
    //
    /// A metric tracking the maximum height that's currently in `finalized_state_queued_blocks`
    ///
    /// Set to `f64::NAN` if `finalized_state_queued_blocks` is empty, because grafana shows NaNs
    /// as a break in the graph.

    #[derivative(Debug = "ignore")]
    closure_to_call_crosslink: ClosureToCallIntoCrosslinkFromState,

    /// the slash index blocks verification at hardfork activations; set at init
    hardfork_schedule: Arc<HardForkSchedule>,
}

/// Return type for the crosslink fat-pointer gate closure.
/// - `None`       — defer: re-queue the block and retry on a later flush (BFT block not yet loaded).
/// - `Some(true)` — accept: the block's fat pointer is valid.
/// - `Some(false)`— reject: the block is permanently invalid (e.g. `do_not_include_until_bc_height` violated).
pub type ClosureToCallIntoCrosslinkFromState = Arc<dyn Fn(FatPointerToBftBlock, FatPointerToBftBlock, block::Height) -> Option<bool> + Send + Sync>;

/// A read-only service for accessing Zebra's cached blockchain state.
///
/// This service provides read-only access to:
/// - the non-finalized state: the ~100 most recent blocks.
/// - the finalized state: older blocks that have many confirmations.
///
/// Requests to this service are processed in parallel,
/// ignoring any blocks queued by the read-write [`StateService`].
///
/// This quick response behavior is better for most state users.
/// It allows other async tasks to make progress while concurrently reading data from disk.
#[derive(Clone, Debug)]
pub struct ReadStateService {
    // Configuration
    //
    /// The configured Zcash network.
    network: Network,

    // Shared Concurrently Readable State
    //
    /// A watch channel with a cached copy of the [`NonFinalizedState`].
    ///
    /// This state is only updated between requests,
    /// so it might include some block data that is also on `disk`.
    non_finalized_state_receiver: WatchReceiver<NonFinalizedState>,

    /// The shared inner on-disk database for the finalized state.
    ///
    /// RocksDB allows reads and writes via a shared reference,
    /// but [`ZebraDb`] doesn't expose any write methods or types.
    ///
    /// This chain is updated concurrently with requests,
    /// so it might include some block data that is also in `best_mem`.
    db: ZebraDb,

    /// A shared handle to a task that writes blocks to the [`NonFinalizedState`] or [`FinalizedState`],
    /// once the queues have received all their parent blocks.
    ///
    /// Used to check for panics when writing blocks.
    block_write_task: Option<Arc<std::thread::JoinHandle<()>>>,
}

impl Drop for StateService {
    fn drop(&mut self) {
        // The state service owns the state, tasks, and channels,
        // so dropping it should shut down everything.

        // The block writer is owned by new_network now: there is no thread to signal and no
        // channels to close here.

        // Log database metrics before shutting down
        info!("dropping the state: logging database metrics");
        self.log_db_metrics();

        // Then drop self.read_service, which checks the block write task for panics,
        // and tries to shut down the database.
    }
}

impl Drop for ReadStateService {
    fn drop(&mut self) {
        // The read state service shares the state,
        // so dropping it should check if we can shut down.

        // TODO: move this into a try_shutdown() method
        if let Some(block_write_task) = self.block_write_task.take() {
            if let Some(block_write_task_handle) = Arc::into_inner(block_write_task) {
                // We're the last database user, so we can tell it to shut down (blocking):
                // - flushes the database to disk, and
                // - drops the database, which cleans up any database tasks correctly.
                self.db.shutdown(true);

                // We are the last state with a reference to this thread, so we can
                // wait until the block write task finishes, then check for panics (blocking).
                // (We'd also like to abort the thread, but std::thread::JoinHandle can't do that.)

                // This log is verbose during tests.
                #[cfg(not(test))]
                info!("waiting for the block write task to finish");
                #[cfg(test)]
                debug!("waiting for the block write task to finish");

                // TODO: move this into a check_for_panics() method
                if let Err(thread_panic) = block_write_task_handle.join() {
                    std::panic::resume_unwind(thread_panic);
                } else {
                    debug!("shutting down the state because the block write task has finished");
                }
            }
        } else {
            // Even if we're not the last database user, try shutting it down.
            //
            // TODO: rename this to try_shutdown()?
            self.db.shutdown(false);
        }
    }
}

impl StateService {
    const PRUNE_INTERVAL: Duration = Duration::from_secs(30);

    /// Creates a new state service for the state `config` and `network`.
    ///
    /// Uses the `max_checkpoint_height` and `checkpoint_verify_concurrency_limit`
    /// to work out when it is near the final checkpoint.
    ///
    /// Returns the read-write and read-only state services,
    /// and read-only watch channels for its best chain tip.
    pub fn new(
        config: Config,
        network: &Network,
        max_checkpoint_height: block::Height,
        checkpoint_verify_concurrency_limit: usize,
        closure_to_call_crosslink: ClosureToCallIntoCrosslinkFromState,
    ) -> (
        Self,
        ReadStateService,
        LatestChainTip,
        ChainTipChange,
        crate::service::write::WriteBlockWorkerTask,
    ) {
        let timer = CodeTimer::start();
        let finalized_state = FinalizedState::new(
            &config,
            network,
            #[cfg(feature = "elasticsearch")]
            true,
        );
        timer.finish(module_path!(), line!(), "opening finalized state database");

        let timer = CodeTimer::start();
        let initial_tip = finalized_state
            .db
            .tip_block()
            .map(CheckpointVerifiedBlock::from)
            .map(ChainTipBlock::from);

        let (chain_tip_sender, latest_chain_tip, chain_tip_change) =
            ChainTipSender::new(initial_tip, network);

        let non_finalized_state = NonFinalizedState::new(network, config.hardfork_schedule.clone());

        let (non_finalized_state_sender, non_finalized_state_receiver) =
            watch::channel(NonFinalizedState::new(&finalized_state.network(), Default::default()));

        // The writer is handed to the caller rather than spawned: new_network owns it and calls
        // it directly, so every mutation of the chain state happens on one thread in a known
        // order, with the result available synchronously.
        let block_writer = write::BlockWriteSender::new(
            finalized_state.clone(),
            non_finalized_state,
            chain_tip_sender,
            non_finalized_state_sender,
        );

        let read_service =
            ReadStateService::new(&finalized_state, None, non_finalized_state_receiver);

        let full_verifier_utxo_lookahead = max_checkpoint_height
            - HeightDiff::try_from(checkpoint_verify_concurrency_limit)
                .expect("fits in HeightDiff");
        let full_verifier_utxo_lookahead =
            full_verifier_utxo_lookahead.unwrap_or(block::Height::MIN);
        let pending_utxos = PendingUtxos::default();

        let state = Self {
            network: network.clone(),
            pending_utxos,
            last_prune: Instant::now(),
            read_service: read_service.clone(),
            closure_to_call_crosslink,
            hardfork_schedule: config.hardfork_schedule.clone(),
        };
        timer.finish(module_path!(), line!(), "initializing state service");

        tracing::info!("starting legacy chain check");
        let timer = CodeTimer::start();

        if let (Some(tip), Some(nu5_activation_height)) = (
            state.best_tip(),
            NetworkUpgrade::Nu5.activation_height(network),
        ) {
            if let Err(error) = check::legacy_chain(
                nu5_activation_height,
                any_ancestor_blocks(
                    &state.read_service.latest_non_finalized_state(),
                    &state.read_service.db,
                    tip.1,
                ),
                &state.network,
                MAX_LEGACY_CHAIN_BLOCKS,
            ) {
                let legacy_db_path = state.read_service.db.path().to_path_buf();
                panic!(
                    "Cached state contains a legacy chain.\n\
                     An outdated Zebra version did not know about a recent network upgrade,\n\
                     so it followed a legacy chain using outdated consensus branch rules.\n\
                     Hint: Delete your database, and restart Zebra to do a full sync.\n\
                     Database path: {legacy_db_path:?}\n\
                     Error: {error:?}",
                );
            }
        }

        tracing::info!("cached state consensus branch is valid: no legacy chain found");
        timer.finish(module_path!(), line!(), "legacy chain check");

        (state, read_service, latest_chain_tip, chain_tip_change, block_writer)
    }

    /// Call read only state service to log rocksdb database metrics.
    pub fn log_db_metrics(&self) {
        self.read_service.db.print_db_metrics();
    }

    /// Return the tip of the current best chain.
    pub fn best_tip(&self) -> Option<(block::Height, block::Hash)> {
        self.read_service.best_tip()
    }

    /// Assert some assumptions about the semantically verified `block` before it is queued.
    fn assert_block_can_be_validated(&self, block: &SemanticallyVerifiedBlock) {
        // required by `Request::CommitSemanticallyVerifiedBlock` call
        assert!(
            block.height > self.network.mandatory_checkpoint_height(),
            "invalid semantically verified block height: the canopy checkpoint is mandatory, pre-canopy \
            blocks, and the canopy activation block, must be committed to the state as finalized \
            blocks"
        );
    }
}

impl ReadStateService {
    /// A handle to the finalized on-disk database (shares the same RocksDB instance).
    pub fn db_clone(&self) -> ZebraDb {
        self.db.clone()
    }

    /// Creates a new read-only state service, using the provided finalized state and
    /// block write task handle.
    ///
    /// Returns the newly created service,
    /// and a watch channel for updating the shared recent non-finalized chain.
    pub(crate) fn new(
        finalized_state: &FinalizedState,
        block_write_task: Option<Arc<std::thread::JoinHandle<()>>>,
        non_finalized_state_receiver: watch::Receiver<NonFinalizedState>,
    ) -> Self {
        let read_service = Self {
            network: finalized_state.network(),
            db: finalized_state.db.clone(),
            non_finalized_state_receiver: WatchReceiver::new(non_finalized_state_receiver),
            block_write_task,
        };

        tracing::debug!("created new read-only state service");

        read_service
    }

    /// Return the tip of the current best chain.
    pub fn best_tip(&self) -> Option<(block::Height, block::Hash)> {
        read::best_tip(&self.latest_non_finalized_state(), &self.db)
    }

    /// Return the tip of the finalized chain only.
    pub fn finalized_tip(&self) -> Option<(block::Height, block::Hash)> {
        self.db.tip()
    }

    /// Return whether `hash` is known to the non-finalized or finalized state.
    pub fn known_block(&self, hash: block::Hash) -> Option<KnownBlock> {
        read::find::non_finalized_state_contains_block_hash(&self.latest_non_finalized_state(), hash)
            .or_else(|| read::find::finalized_state_contains_block_hash(&self.db, hash))
    }

    /// Return the block identified by `hash_or_height`, searching all non-finalized chains
    /// before falling back to the finalized state.
    pub fn block_from_any_chain(&self, hash_or_height: crate::HashOrHeight) -> Option<Arc<block::Block>> {
        self.non_finalized_state_receiver.with_watch_data(|non_finalized_state| {
            for chain in non_finalized_state.chain_iter() {
                if let Some(contextual) = chain.block(hash_or_height) {
                    return Some(contextual.block.clone());
                }
            }
            self.db.block(hash_or_height)
        })
    }

    /// Return the header of the block identified by `hash_or_height`, searching all
    /// non-finalized chains before falling back to the finalized state.
    ///
    /// Mirrors `block_from_any_chain`, but avoids deserializing the whole block. Needed for
    /// the crosslink fat-pointer gate, which must resolve a *parent* that may be on a side
    /// chain rather than the best chain.
    pub fn any_chain_block_header(
        &self,
        hash_or_height: crate::HashOrHeight,
    ) -> Option<Arc<block::Header>> {
        self.non_finalized_state_receiver.with_watch_data(|non_finalized_state| {
            for chain in non_finalized_state.chain_iter() {
                if let Some(contextual) = chain.block(hash_or_height) {
                    return Some(contextual.block.header.clone());
                }
            }
            self.db.block_header(hash_or_height)
        })
    }

    /// The network this state is for.
    pub fn network(&self) -> &Network {
        &self.network
    }

    /// Return the UTXO for `outpoint` if it exists in any non-finalized chain or in the
    /// finalized state.
    ///
    /// This is a *load*, not the spend check: whether the spend is legal (unspent, correctly
    /// ordered, mature coinbase) is decided later by
    /// [`check::utxo::transparent_spend()`](crate::service::check::utxo::transparent_spend)
    /// during contextual validation in the block write task.
    pub fn any_chain_utxo(&self, outpoint: &zebra_chain::transparent::OutPoint) -> Option<zebra_chain::transparent::Utxo> {
        self.non_finalized_state_receiver.with_watch_data(|non_finalized_state| {
            read::any_utxo(non_finalized_state, &self.db, *outpoint)
        })
    }

    /// Return the hash of the best chain block at `height`, if any.
    pub fn best_chain_block_hash(&self, height: block::Height) -> Option<block::Hash> {
        self.non_finalized_state_receiver.with_watch_data(|non_finalized_state| {
            read::hash_by_height(non_finalized_state.best_chain(), &self.db, height)
        })
    }

    /// Return hashes for best-chain blocks following `known_blocks`, up to `stop`.
    pub fn find_block_hashes(
        &self,
        known_blocks: Vec<block::Hash>,
        stop: Option<block::Hash>,
    ) -> Vec<block::Hash> {
        self.non_finalized_state_receiver
            .with_watch_data(|non_finalized_state| {
                read::find_chain_hashes(
                    non_finalized_state.best_chain(),
                    &self.db,
                    known_blocks,
                    stop,
                    MAX_FIND_BLOCK_HASHES_RESULTS,
                )
            })
    }

    /// Return headers for best-chain blocks following `known_blocks`, up to `stop`.
    pub fn find_block_headers(
        &self,
        known_blocks: Vec<block::Hash>,
        stop: Option<block::Hash>,
    ) -> Vec<CountedHeader> {
        self.non_finalized_state_receiver
            .with_watch_data(|non_finalized_state| {
                read::find_chain_headers(
                    non_finalized_state.best_chain(),
                    &self.db,
                    known_blocks,
                    stop,
                    MAX_FIND_BLOCK_HEADERS_RESULTS,
                )
            })
            .into_iter()
            .map(|header| CountedHeader { header })
            .collect()
    }

    /// Return the header, height, hash, and next block hash for the best-chain block identified
    /// by `hash_or_height`, if any.
    pub fn block_header(
        &self,
        hash_or_height: crate::HashOrHeight,
    ) -> Option<(Arc<block::Header>, block::Height, block::Hash, Option<block::Hash>)> {
        let best_chain = self.latest_best_chain();
        let height = hash_or_height
            .height_or_else(|hash| read::find::height_by_hash(best_chain.clone(), &self.db, hash))?;
        let hash = hash_or_height
            .hash_or_else(|height| read::find::hash_by_height(best_chain.clone(), &self.db, height))?;
        let next_block_hash = height
            .next()
            .ok()
            .and_then(|next_height| read::find::hash_by_height(best_chain.clone(), &self.db, next_height));
        let header = read::block_header(best_chain, &self.db, height.into())?;
        Some((header, height, hash, next_block_hash))
    }

    /// Run `f` against the latest non-finalized state.
    ///
    /// new_network builds its near-tip chain tree out of this instead of maintaining its own
    /// copy of it.
    pub(crate) fn with_non_finalized_state<U>(
        &self,
        f: impl FnOnce(&NonFinalizedState) -> U,
    ) -> U {
        self.non_finalized_state_receiver
            .with_watch_data(|non_finalized_state| f(&non_finalized_state))
    }

    /// Gets a clone of the latest non-finalized state from the `non_finalized_state_receiver`
    fn latest_non_finalized_state(&self) -> NonFinalizedState {
        self.non_finalized_state_receiver.cloned_watch_data()
    }

    /// Gets a clone of the latest, best non-finalized chain from the `non_finalized_state_receiver`
    #[allow(dead_code)]
    fn latest_best_chain(&self) -> Option<Arc<Chain>> {
        self.latest_non_finalized_state().best_chain().cloned()
    }

    /// Test-only access to the inner database.
    /// Can be used to modify the database without doing any consensus checks.
    #[cfg(any(test, feature = "proptest-impl"))]
    pub fn db(&self) -> &ZebraDb {
        &self.db
    }

    /// Logs rocksdb metrics using the read only state service.
    pub fn log_db_metrics(&self) {
        self.db.print_db_metrics();
    }
}

impl Service<Request> for StateService {
    type Response = Response;
    type Error = BoxError;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // Check for panics in the block write task
        let poll = self.read_service.poll_ready(cx);

        // Prune outdated UTXO requests
        let now = Instant::now();

        if self.last_prune + Self::PRUNE_INTERVAL < now {
            let tip = self.best_tip();
            let old_len = self.pending_utxos.len();

            self.pending_utxos.prune();
            self.last_prune = now;

            let new_len = self.pending_utxos.len();
            let prune_count = old_len
                .checked_sub(new_len)
                .expect("prune does not add any utxo requests");
            if prune_count > 0 {
                tracing::debug!(
                    ?old_len,
                    ?new_len,
                    ?prune_count,
                    ?tip,
                    "pruned utxo requests"
                );
            } else {
                tracing::debug!(len = ?old_len, ?tip, "no utxo requests needed pruning");
            }
        }

        poll
    }

    #[instrument(name = "state", skip(self, req))]
    fn call(&mut self, req: Request) -> Self::Future {
        req.count_metric();
        let timer = CodeTimer::start();
        let span = Span::current();

        match req {
            // Blocks are no longer committed through the state service: new_network owns the
            // writer and calls it directly. These arms exist only because the unreachable
            // consensus verifiers still name them.
            Request::CommitSemanticallyVerifiedBlock(_)
            | Request::CommitCheckpointVerifiedBlock(_) => {
                timer.finish(module_path!(), line!(), "Commit*Block (unreachable)");
                async {
                    Err(BoxError::from(
                        "blocks are committed through new_network, not the state service",
                    ))
                }
                .boxed()
            }

            // BFT finalization is routed to new_network, which owns the block writer.
            Request::CrosslinkFinalizeBlock(finalized) => {
                timer.finish(module_path!(), line!(), "CrosslinkFinalizeBlock");

                async move {
                    crate::new_network::crosslink_finalize_via_new_network(
                        finalized,
                        std::time::Duration::from_secs(30),
                    )
                    .await
                    .map(|(hash, stakes)| Response::CrosslinkFinalized(hash, stakes))
                    .map_err(BoxError::from)
                }
                .boxed()
            }

            Request::AwaitUtxo(outpoint) => {
                // Prepare the AwaitUtxo future from PendingUxtos.
                let response_fut = self.pending_utxos.queue(outpoint);
                // Only instrument `response_fut`, the ReadStateService already
                // instruments its requests with the same span.

                let response_fut = response_fut.instrument(span).boxed();

                // The two in-flight UTXO tiers are gone with the block queue. They existed for
                // Zebra's concurrent download lookahead: a block could spend an output created by
                // a block verified but not yet committed. new_network submits only blocks whose
                // parent is already committed, and commits them one at a time, so a spent output
                // is always either created within the same block or already in the state.
                // We ignore any UTXOs in FinalizedState.finalized_state_queued_blocks,
                // because it is only used during checkpoint verification.
                //
                // This creates a rare race condition, but it doesn't seem to happen much in practice.
                // See #5126 for details.

                // Manually send a request to the ReadStateService,
                // to get UTXOs from any non-finalized chain or the finalized chain.
                let read_service = self.read_service.clone();

                // Run the request in an async block, so we can await the response.
                async move {
                    let req = ReadRequest::AnyChainUtxo(outpoint);

                    let rsp = read_service.oneshot(req).await?;

                    // Optional TODO:
                    //  - make pending_utxos.respond() async using a channel,
                    //    so we can respond to all waiting requests here
                    //
                    // This change is not required for correctness, because:
                    // - any waiting requests should have returned when the block was sent to the state
                    // - otherwise, the request returns immediately if:
                    //   - the block is in the non-finalized queue, or
                    //   - the block is in any non-finalized chain or the finalized state
                    //
                    // And if the block is in the finalized queue,
                    // that's rare enough that a retry is ok.
                    if let ReadResponse::AnyChainUtxo(Some(utxo)) = rsp {
                        // We got a UTXO, so we replace the response future with the result own.
                        timer.finish(module_path!(), line!(), "AwaitUtxo/any-chain");

                        return Ok(Response::Utxo(utxo));
                    }

                    // We're finished, but the returned future is waiting on the respond() channel.
                    timer.finish(module_path!(), line!(), "AwaitUtxo/waiting");

                    response_fut.await
                }
                .boxed()
            }

            // Used by sync, inbound, and block verifier to check if a block is already in the state
            // before downloading or validating it.
            // @Todo: We'd like a new request or modification to KnownBlock to allow
            //        NewNet to send full block data from blocks on sidechains. :SidechainSync
            Request::KnownBlock(hash) => {
                let timer = CodeTimer::start();

                let read_service = self.read_service.clone();

                async move {
                    let response = read_service.known_block(hash);

                    // The work is done in the future.
                    timer.finish(module_path!(), line!(), "Request::KnownBlock");

                    Ok(Response::KnownBlock(response))
                }
                .boxed()
            }

            // Runs concurrently using the ReadStateService
            Request::Tip
            | Request::Depth(_)
            | Request::BestChainNextMedianTimePast
            | Request::BestChainBlockHash(_)
            | Request::BlockLocator
            | Request::Transaction(_)
            | Request::UnspentBestChainUtxo(_)
            | Request::Block(_)
            | Request::BlockAndSize(_)
            | Request::BlockHeader(_)
            | Request::FindBlockHashes { .. }
            | Request::FindBlockHeaders { .. }
            | Request::CheckBestChainTipNullifiersAndAnchors(_)
            | Request::BondInfo(_) => {
                // Redirect the request to the concurrent ReadStateService
                let read_service = self.read_service.clone();

                async move {
                    let req = req
                        .try_into()
                        .expect("ReadRequest conversion should not fail");

                    let rsp = read_service.oneshot(req).await?;
                    let rsp = rsp.try_into().expect("Response conversion should not fail");

                    Ok(rsp)
                }
                .boxed()
            }

            Request::CheckBlockProposalValidity(_) => {
                // Redirect the request to the concurrent ReadStateService
                let read_service = self.read_service.clone();

                async move {
                    let req = req
                        .try_into()
                        .expect("ReadRequest conversion should not fail");

                    let rsp = read_service.oneshot(req).await?;
                    let rsp = rsp.try_into().expect("Response conversion should not fail");

                    Ok(rsp)
                }
                .boxed()
            }
        }
    }
}

impl Service<ReadRequest> for ReadStateService {
    type Response = ReadResponse;
    type Error = BoxError;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        // Check for panics in the block write task
        //
        // TODO: move into a check_for_panics() method
        let block_write_task = self.block_write_task.take();

        if let Some(block_write_task) = block_write_task {
            if block_write_task.is_finished() {
                if let Some(block_write_task) = Arc::into_inner(block_write_task) {
                    // We are the last state with a reference to this task, so we can propagate any panics
                    if let Err(thread_panic) = block_write_task.join() {
                        std::panic::resume_unwind(thread_panic);
                    }
                }
            } else {
                // It hasn't finished, so we need to put it back
                self.block_write_task = Some(block_write_task);
            }
        }

        self.db.check_for_panics();

        Poll::Ready(Ok(()))
    }

    #[instrument(name = "read_state", skip(self, req))]
    fn call(&mut self, req: ReadRequest) -> Self::Future {
        req.count_metric();
        let timer = CodeTimer::start();
        let span = Span::current();

        match req {
            // Used by the `getblockchaininfo` RPC.
            ReadRequest::UsageInfo => {
                let db = self.db.clone();

                tokio::task::spawn_blocking(move || {
                    span.in_scope(move || {
                        // The work is done in the future.

                        let db_size = db.size();

                        timer.finish(module_path!(), line!(), "ReadRequest::UsageInfo");

                        Ok(ReadResponse::UsageInfo(db_size))
                    })
                })
                .wait_for_panics()
            }

            // Used by the StateService.
            ReadRequest::Tip => {
                let state = self.clone();

                tokio::task::spawn_blocking(move || {
                    span.in_scope(move || {
                        let tip = state.best_tip();

                        // The work is done in the future.
                        timer.finish(module_path!(), line!(), "ReadRequest::Tip");

                        Ok(ReadResponse::Tip(tip))
                    })
                })
                .wait_for_panics()
            }

            ReadRequest::FinalizedTip => {
                let state = self.clone();

                tokio::task::spawn_blocking(move || {
                    Ok(ReadResponse::Tip(state.finalized_tip()))
                })
                .wait_for_panics()
            }

            // Used by `getblockchaininfo` RPC method.
            ReadRequest::TipPoolValues => {
                let state = self.clone();

                tokio::task::spawn_blocking(move || {
                    span.in_scope(move || {
                        let tip_with_value_balance = state
                            .non_finalized_state_receiver
                            .with_watch_data(|non_finalized_state| {
                                read::tip_with_value_balance(
                                    non_finalized_state.best_chain(),
                                    &state.db,
                                )
                            });

                        // The work is done in the future.
                        // TODO: Do this in the Drop impl with the variant name?
                        timer.finish(module_path!(), line!(), "ReadRequest::TipPoolValues");

                        let (tip_height, tip_hash, value_balance) = tip_with_value_balance?
                            .ok_or(BoxError::from("no chain tip available yet"))?;

                        Ok(ReadResponse::TipPoolValues {
                            tip_height,
                            tip_hash,
                            value_balance,
                        })
                    })
                })
                .wait_for_panics()
            }

            // Used by getblock
            ReadRequest::BlockInfo(hash_or_height) => {
                let state = self.clone();

                tokio::task::spawn_blocking(move || {
                    span.in_scope(move || {
                        let value_balance = state.non_finalized_state_receiver.with_watch_data(
                            |non_finalized_state| {
                                read::block_info(
                                    non_finalized_state.best_chain(),
                                    &state.db,
                                    hash_or_height,
                                )
                            },
                        );

                        // The work is done in the future.
                        // TODO: Do this in the Drop impl with the variant name?
                        timer.finish(module_path!(), line!(), "ReadRequest::BlockInfo");

                        Ok(ReadResponse::BlockInfo(value_balance))
                    })
                })
                .wait_for_panics()
            }

            // Used by the StateService.
            ReadRequest::Depth(hash) => {
                let state = self.clone();

                tokio::task::spawn_blocking(move || {
                    span.in_scope(move || {
                        let depth = state.non_finalized_state_receiver.with_watch_data(
                            |non_finalized_state| {
                                read::depth(non_finalized_state.best_chain(), &state.db, hash)
                            },
                        );

                        // The work is done in the future.
                        timer.finish(module_path!(), line!(), "ReadRequest::Depth");

                        Ok(ReadResponse::Depth(depth))
                    })
                })
                .wait_for_panics()
            }

            // Used by the StateService.
            ReadRequest::BestChainNextMedianTimePast => {
                let state = self.clone();

                tokio::task::spawn_blocking(move || {
                    span.in_scope(move || {
                        let non_finalized_state = state.latest_non_finalized_state();
                        let median_time_past =
                            read::next_median_time_past(&non_finalized_state, &state.db);

                        // The work is done in the future.
                        timer.finish(
                            module_path!(),
                            line!(),
                            "ReadRequest::BestChainNextMedianTimePast",
                        );

                        Ok(ReadResponse::BestChainNextMedianTimePast(median_time_past?))
                    })
                })
                .wait_for_panics()
            }

            // Used by the get_block (raw) RPC and the StateService.
            ReadRequest::Block(hash_or_height) => {
                let state = self.clone();

                tokio::task::spawn_blocking(move || {
                    span.in_scope(move || {
                        let block = state.non_finalized_state_receiver.with_watch_data(
                            |non_finalized_state| {
                                read::block(
                                    non_finalized_state.best_chain(),
                                    &state.db,
                                    hash_or_height,
                                )
                            },
                        );

                        // The work is done in the future.
                        timer.finish(module_path!(), line!(), "ReadRequest::Block");

                        Ok(ReadResponse::Block(block))
                    })
                })
                .wait_for_panics()
            }

            // Like ReadRequest::Block, but searches all non-finalized chains.
            ReadRequest::BlockButAlsoAllChains(hash_or_height) => {
                let state = self.clone();

                tokio::task::spawn_blocking(move || {
                    span.in_scope(move || {
                        let block = state.block_from_any_chain(hash_or_height);

                        timer.finish(module_path!(), line!(), "ReadRequest::BlockButAlsoAllChains");

                        Ok(ReadResponse::BlockButAlsoAllChains(block))
                    })
                })
                .wait_for_panics()
            }

            // Used by the get_block (raw) RPC and the StateService.
            ReadRequest::BlockAndSize(hash_or_height) => {
                let state = self.clone();

                tokio::task::spawn_blocking(move || {
                    span.in_scope(move || {
                        let block_and_size = state.non_finalized_state_receiver.with_watch_data(
                            |non_finalized_state| {
                                read::block_and_size(
                                    non_finalized_state.best_chain(),
                                    &state.db,
                                    hash_or_height,
                                )
                            },
                        );

                        // The work is done in the future.
                        timer.finish(module_path!(), line!(), "ReadRequest::BlockAndSize");

                        Ok(ReadResponse::BlockAndSize(block_and_size))
                    })
                })
                .wait_for_panics()
            }

            // Used by the get_block (verbose) RPC and the StateService.
            ReadRequest::BlockHeader(hash_or_height) => {
                let state = self.clone();

                tokio::task::spawn_blocking(move || {
                    span.in_scope(move || {
                        let (header, height, hash, next_block_hash) = state
                            .block_header(hash_or_height)
                            .ok_or_else(|| BoxError::from("block hash or height not found"))?;

                        // The work is done in the future.
                        timer.finish(module_path!(), line!(), "ReadRequest::Block");

                        Ok(ReadResponse::BlockHeader {
                            header,
                            hash,
                            height,
                            next_block_hash,
                        })
                    })
                })
                .wait_for_panics()
            }

            // For the get_raw_transaction RPC and the StateService.
            ReadRequest::Transaction(hash) => {
                let state = self.clone();

                tokio::task::spawn_blocking(move || {
                    span.in_scope(move || {
                        let response =
                            read::mined_transaction(state.latest_best_chain(), &state.db, hash);

                        // The work is done in the future.
                        timer.finish(module_path!(), line!(), "ReadRequest::Transaction");

                        Ok(ReadResponse::Transaction(response))
                    })
                })
                .wait_for_panics()
            }

            // Used by the getblock (verbose) RPC.
            ReadRequest::TransactionIdsForBlock(hash_or_height) => {
                let state = self.clone();

                tokio::task::spawn_blocking(move || {
                    span.in_scope(move || {
                        let transaction_ids = state.non_finalized_state_receiver.with_watch_data(
                            |non_finalized_state| {
                                read::transaction_hashes_for_block(
                                    non_finalized_state.best_chain(),
                                    &state.db,
                                    hash_or_height,
                                )
                            },
                        );

                        // The work is done in the future.
                        timer.finish(
                            module_path!(),
                            line!(),
                            "ReadRequest::TransactionIdsForBlock",
                        );

                        Ok(ReadResponse::TransactionIdsForBlock(transaction_ids))
                    })
                })
                .wait_for_panics()
            }

            #[cfg(feature = "indexer")]
            ReadRequest::SpendingTransactionId(spend) => {
                let state = self.clone();

                tokio::task::spawn_blocking(move || {
                    span.in_scope(move || {
                        let spending_transaction_id = state
                            .non_finalized_state_receiver
                            .with_watch_data(|non_finalized_state| {
                                read::spending_transaction_hash(
                                    non_finalized_state.best_chain(),
                                    &state.db,
                                    spend,
                                )
                            });

                        // The work is done in the future.
                        timer.finish(
                            module_path!(),
                            line!(),
                            "ReadRequest::TransactionIdForSpentOutPoint",
                        );

                        Ok(ReadResponse::TransactionId(spending_transaction_id))
                    })
                })
                .wait_for_panics()
            }

            ReadRequest::UnspentBestChainUtxo(outpoint) => {
                let state = self.clone();

                tokio::task::spawn_blocking(move || {
                    span.in_scope(move || {
                        let utxo = state.non_finalized_state_receiver.with_watch_data(
                            |non_finalized_state| {
                                read::unspent_utxo(
                                    non_finalized_state.best_chain(),
                                    &state.db,
                                    outpoint,
                                )
                            },
                        );

                        // The work is done in the future.
                        timer.finish(module_path!(), line!(), "ReadRequest::UnspentBestChainUtxo");

                        Ok(ReadResponse::UnspentBestChainUtxo(utxo))
                    })
                })
                .wait_for_panics()
            }

            // Manually used by the StateService to implement part of AwaitUtxo.
            ReadRequest::AnyChainUtxo(outpoint) => {
                let state = self.clone();

                tokio::task::spawn_blocking(move || {
                    span.in_scope(move || {
                        let utxo = state.non_finalized_state_receiver.with_watch_data(
                            |non_finalized_state| {
                                read::any_utxo(non_finalized_state, &state.db, outpoint)
                            },
                        );

                        // The work is done in the future.
                        timer.finish(module_path!(), line!(), "ReadRequest::AnyChainUtxo");

                        Ok(ReadResponse::AnyChainUtxo(utxo))
                    })
                })
                .wait_for_panics()
            }

            // Used by the StateService.
            ReadRequest::BlockLocator => {
                let state = self.clone();

                tokio::task::spawn_blocking(move || {
                    span.in_scope(move || {
                        let block_locator = state.non_finalized_state_receiver.with_watch_data(
                            |non_finalized_state| {
                                read::block_locator(non_finalized_state.best_chain(), &state.db)
                            },
                        );

                        // The work is done in the future.
                        timer.finish(module_path!(), line!(), "ReadRequest::BlockLocator");

                        Ok(ReadResponse::BlockLocator(
                            block_locator.unwrap_or_default(),
                        ))
                    })
                })
                .wait_for_panics()
            }

            // Used by the StateService.
            ReadRequest::FindBlockHashes { known_blocks, stop } => {
                let state = self.clone();

                tokio::task::spawn_blocking(move || {
                    span.in_scope(move || {
                        let block_hashes = state.find_block_hashes(known_blocks, stop);

                        // The work is done in the future.
                        timer.finish(module_path!(), line!(), "ReadRequest::FindBlockHashes");

                        Ok(ReadResponse::BlockHashes(block_hashes))
                    })
                })
                .wait_for_panics()
            }

            // Used by the StateService.
            ReadRequest::FindBlockHeaders { known_blocks, stop } => {
                let state = self.clone();

                tokio::task::spawn_blocking(move || {
                    span.in_scope(move || {
                        let block_headers = state.find_block_headers(known_blocks, stop);

                        // The work is done in the future.
                        timer.finish(module_path!(), line!(), "ReadRequest::FindBlockHeaders");

                        Ok(ReadResponse::BlockHeaders(block_headers))
                    })
                })
                .wait_for_panics()
            }

            ReadRequest::SaplingTree(hash_or_height) => {
                let state = self.clone();

                tokio::task::spawn_blocking(move || {
                    span.in_scope(move || {
                        let sapling_tree = state.non_finalized_state_receiver.with_watch_data(
                            |non_finalized_state| {
                                read::sapling_tree(
                                    non_finalized_state.best_chain(),
                                    &state.db,
                                    hash_or_height,
                                )
                            },
                        );

                        // The work is done in the future.
                        timer.finish(module_path!(), line!(), "ReadRequest::SaplingTree");

                        Ok(ReadResponse::SaplingTree(sapling_tree))
                    })
                })
                .wait_for_panics()
            }

            ReadRequest::OrchardTree(hash_or_height) => {
                let state = self.clone();

                tokio::task::spawn_blocking(move || {
                    span.in_scope(move || {
                        let orchard_tree = state.non_finalized_state_receiver.with_watch_data(
                            |non_finalized_state| {
                                read::orchard_tree(
                                    non_finalized_state.best_chain(),
                                    &state.db,
                                    hash_or_height,
                                )
                            },
                        );

                        // The work is done in the future.
                        timer.finish(module_path!(), line!(), "ReadRequest::OrchardTree");

                        Ok(ReadResponse::OrchardTree(orchard_tree))
                    })
                })
                .wait_for_panics()
            }

            ReadRequest::SaplingSubtrees { start_index, limit } => {
                let state = self.clone();

                tokio::task::spawn_blocking(move || {
                    span.in_scope(move || {
                        let end_index = limit
                            .and_then(|limit| start_index.0.checked_add(limit.0))
                            .map(NoteCommitmentSubtreeIndex);

                        let sapling_subtrees = state.non_finalized_state_receiver.with_watch_data(
                            |non_finalized_state| {
                                if let Some(end_index) = end_index {
                                    read::sapling_subtrees(
                                        non_finalized_state.best_chain(),
                                        &state.db,
                                        start_index..end_index,
                                    )
                                } else {
                                    // If there is no end bound, just return all the trees.
                                    // If the end bound would overflow, just returns all the trees, because that's what
                                    // `zcashd` does. (It never calculates an end bound, so it just keeps iterating until
                                    // the trees run out.)
                                    read::sapling_subtrees(
                                        non_finalized_state.best_chain(),
                                        &state.db,
                                        start_index..,
                                    )
                                }
                            },
                        );

                        // The work is done in the future.
                        timer.finish(module_path!(), line!(), "ReadRequest::SaplingSubtrees");

                        Ok(ReadResponse::SaplingSubtrees(sapling_subtrees))
                    })
                })
                .wait_for_panics()
            }

            ReadRequest::OrchardSubtrees { start_index, limit } => {
                let state = self.clone();

                tokio::task::spawn_blocking(move || {
                    span.in_scope(move || {
                        let end_index = limit
                            .and_then(|limit| start_index.0.checked_add(limit.0))
                            .map(NoteCommitmentSubtreeIndex);

                        let orchard_subtrees = state.non_finalized_state_receiver.with_watch_data(
                            |non_finalized_state| {
                                if let Some(end_index) = end_index {
                                    read::orchard_subtrees(
                                        non_finalized_state.best_chain(),
                                        &state.db,
                                        start_index..end_index,
                                    )
                                } else {
                                    // If there is no end bound, just return all the trees.
                                    // If the end bound would overflow, just returns all the trees, because that's what
                                    // `zcashd` does. (It never calculates an end bound, so it just keeps iterating until
                                    // the trees run out.)
                                    read::orchard_subtrees(
                                        non_finalized_state.best_chain(),
                                        &state.db,
                                        start_index..,
                                    )
                                }
                            },
                        );

                        // The work is done in the future.
                        timer.finish(module_path!(), line!(), "ReadRequest::OrchardSubtrees");

                        Ok(ReadResponse::OrchardSubtrees(orchard_subtrees))
                    })
                })
                .wait_for_panics()
            }

            // For the get_address_balance RPC.
            ReadRequest::AddressBalance(addresses) => {
                let state = self.clone();

                tokio::task::spawn_blocking(move || {
                    span.in_scope(move || {
                        let (balance, received) = state
                            .non_finalized_state_receiver
                            .with_watch_data(|non_finalized_state| {
                                read::transparent_balance(
                                    non_finalized_state.best_chain().cloned(),
                                    &state.db,
                                    addresses,
                                )
                            })?;

                        // The work is done in the future.
                        timer.finish(module_path!(), line!(), "ReadRequest::AddressBalance");

                        Ok(ReadResponse::AddressBalance { balance, received })
                    })
                })
                .wait_for_panics()
            }

            // For the get_address_tx_ids RPC.
            ReadRequest::TransactionIdsByAddresses {
                addresses,
                height_range,
            } => {
                let state = self.clone();

                tokio::task::spawn_blocking(move || {
                    span.in_scope(move || {
                        let tx_ids = state.non_finalized_state_receiver.with_watch_data(
                            |non_finalized_state| {
                                read::transparent_tx_ids(
                                    non_finalized_state.best_chain(),
                                    &state.db,
                                    addresses,
                                    height_range,
                                )
                            },
                        );

                        // The work is done in the future.
                        timer.finish(
                            module_path!(),
                            line!(),
                            "ReadRequest::TransactionIdsByAddresses",
                        );

                        tx_ids.map(ReadResponse::AddressesTransactionIds)
                    })
                })
                .wait_for_panics()
            }

            // For the get_address_utxos RPC.
            ReadRequest::UtxosByAddresses(addresses) => {
                let state = self.clone();

                tokio::task::spawn_blocking(move || {
                    span.in_scope(move || {
                        let utxos = state.non_finalized_state_receiver.with_watch_data(
                            |non_finalized_state| {
                                read::address_utxos(
                                    &state.network,
                                    non_finalized_state.best_chain(),
                                    &state.db,
                                    addresses,
                                )
                            },
                        );

                        // The work is done in the future.
                        timer.finish(module_path!(), line!(), "ReadRequest::UtxosByAddresses");

                        utxos.map(ReadResponse::AddressUtxos)
                    })
                })
                .wait_for_panics()
            }

            ReadRequest::CheckBestChainTipNullifiersAndAnchors(unmined_tx) => {
                let state = self.clone();

                tokio::task::spawn_blocking(move || {
                    span.in_scope(move || {
                        let latest_non_finalized_best_chain =
                            state.latest_non_finalized_state().best_chain().cloned();

                        check::nullifier::tx_no_duplicates_in_chain(
                            &state.db,
                            latest_non_finalized_best_chain.as_ref(),
                            &unmined_tx.transaction,
                        )?;

                        check::anchors::tx_anchors_refer_to_final_treestates(
                            &state.db,
                            latest_non_finalized_best_chain.as_ref(),
                            &unmined_tx,
                        )?;

                        // The work is done in the future.
                        timer.finish(
                            module_path!(),
                            line!(),
                            "ReadRequest::CheckBestChainTipNullifiersAndAnchors",
                        );

                        Ok(ReadResponse::ValidBestChainTipNullifiersAndAnchors)
                    })
                })
                .wait_for_panics()
            }

            // Used by the get_block and get_block_hash RPCs.
            ReadRequest::BestChainBlockHash(height) => {
                let state = self.clone();

                // # Performance
                //
                // Allow other async tasks to make progress while concurrently reading blocks from disk.

                tokio::task::spawn_blocking(move || {
                    span.in_scope(move || {
                        let hash = state.best_chain_block_hash(height);

                        // The work is done in the future.
                        timer.finish(module_path!(), line!(), "ReadRequest::BestChainBlockHash");

                        Ok(ReadResponse::BlockHash(hash))
                    })
                })
                .wait_for_panics()
            }

            // Used by get_block_template and getblockchaininfo RPCs.
            ReadRequest::ChainInfo => {
                let state = self.clone();
                let latest_non_finalized_state = self.latest_non_finalized_state();

                // # Performance
                //
                // Allow other async tasks to make progress while concurrently reading blocks from disk.

                tokio::task::spawn_blocking(move || {
                    span.in_scope(move || {
                        // # Correctness
                        //
                        // It is ok to do these lookups using multiple database calls. Finalized state updates
                        // can only add overlapping blocks, and block hashes are unique across all chain forks.
                        //
                        // If there is a large overlap between the non-finalized and finalized states,
                        // where the finalized tip is above the non-finalized tip,
                        // Zebra is receiving a lot of blocks, or this request has been delayed for a long time.
                        //
                        // In that case, the `getblocktemplate` RPC will return an error because Zebra
                        // is not synced to the tip. That check happens before the RPC makes this request.
                        let get_block_template_info =
                            read::difficulty::get_block_template_chain_info(
                                &latest_non_finalized_state,
                                &state.db,
                                &state.network,
                            );

                        // The work is done in the future.
                        timer.finish(module_path!(), line!(), "ReadRequest::ChainInfo");

                        get_block_template_info.map(ReadResponse::ChainInfo)
                    })
                })
                .wait_for_panics()
            }

            // Used by getmininginfo, getnetworksolps, and getnetworkhashps RPCs.
            ReadRequest::SolutionRate { num_blocks, height } => {
                let state = self.clone();

                // # Performance
                //
                // Allow other async tasks to make progress while concurrently reading blocks from disk.

                tokio::task::spawn_blocking(move || {
                    span.in_scope(move || {
                        let latest_non_finalized_state = state.latest_non_finalized_state();
                        // # Correctness
                        //
                        // It is ok to do these lookups using multiple database calls. Finalized state updates
                        // can only add overlapping blocks, and block hashes are unique across all chain forks.
                        //
                        // The worst that can happen here is that the default `start_hash` will be below
                        // the chain tip.
                        let (tip_height, tip_hash) =
                            match read::tip(latest_non_finalized_state.best_chain(), &state.db) {
                                Some(tip_hash) => tip_hash,
                                None => return Ok(ReadResponse::SolutionRate(None)),
                            };

                        let start_hash = match height {
                            Some(height) if height < tip_height => read::hash_by_height(
                                latest_non_finalized_state.best_chain(),
                                &state.db,
                                height,
                            ),
                            // use the chain tip hash if height is above it or not provided.
                            _ => Some(tip_hash),
                        };

                        let solution_rate = start_hash.and_then(|start_hash| {
                            read::difficulty::solution_rate(
                                &latest_non_finalized_state,
                                &state.db,
                                num_blocks,
                                start_hash,
                            )
                        });

                        // The work is done in the future.
                        timer.finish(module_path!(), line!(), "ReadRequest::SolutionRate");

                        Ok(ReadResponse::SolutionRate(solution_rate))
                    })
                })
                .wait_for_panics()
            }

            ReadRequest::CheckBlockProposalValidity(semantically_verified) => {
                let state = self.clone();

                // # Performance
                //
                // Allow other async tasks to make progress while concurrently reading blocks from disk.

                tokio::task::spawn_blocking(move || {
                    span.in_scope(move || {
                        tracing::debug!("attempting to validate and commit block proposal onto a cloned non-finalized state");
                        let mut latest_non_finalized_state = state.latest_non_finalized_state();

                        // The previous block of a valid proposal must be on the best chain tip.
                        let Some((_best_tip_height, best_tip_hash)) = read::best_tip(&latest_non_finalized_state, &state.db) else {
                            return Err("state is empty: wait for Zebra to sync before submitting a proposal".into());
                        };

                        if semantically_verified.block.header.previous_block_hash != best_tip_hash {
                            return Err("proposal is not based on the current best chain tip: previous block hash must be the best chain tip".into());
                        }

                        // This clone of the non-finalized state is dropped when this closure returns.
                        // The non-finalized state that's used in the rest of the state (including finalizing
                        // blocks into the db) is not mutated here.
                        //
                        // TODO: Convert `CommitSemanticallyVerifiedError` to a new `ValidateProposalError`?
                        latest_non_finalized_state.disable_metrics();

                        write::validate_and_commit_non_finalized(
                            &state.db,
                            &mut latest_non_finalized_state,
                            semantically_verified,
                        )?;

                        // The work is done in the future.
                        timer.finish(
                            module_path!(),
                            line!(),
                            "ReadRequest::CheckBlockProposalValidity",
                        );

                        Ok(ReadResponse::ValidBlockProposal)
                    })
                })
                .wait_for_panics()
            }

            ReadRequest::TipBlockSize => {
                let state = self.clone();

                tokio::task::spawn_blocking(move || {
                    span.in_scope(move || {
                        // Get the best chain tip height.
                        let tip_height = state
                            .non_finalized_state_receiver
                            .with_watch_data(|non_finalized_state| {
                                read::tip_height(non_finalized_state.best_chain(), &state.db)
                            })
                            .unwrap_or(Height(0));

                        // Get the block at the best chain tip height.
                        let block = state.non_finalized_state_receiver.with_watch_data(
                            |non_finalized_state| {
                                read::block(
                                    non_finalized_state.best_chain(),
                                    &state.db,
                                    tip_height.into(),
                                )
                            },
                        );

                        // The work is done in the future.
                        timer.finish(module_path!(), line!(), "ReadRequest::TipBlockSize");

                        // Respond with the length of the obtained block if any.
                        match block {
                            Some(b) => Ok(ReadResponse::TipBlockSize(Some(
                                b.zcash_serialize_to_vec()?.len(),
                            ))),
                            None => Ok(ReadResponse::TipBlockSize(None)),
                        }
                    })
                })
                .wait_for_panics()
            }

            ReadRequest::NonFinalizedBlocksListener => {
                // The non-finalized blocks listener is used to notify the state service
                // about new blocks that have been added to the non-finalized state.
                let non_finalized_blocks_listener = NonFinalizedBlocksListener::spawn(
                    self.network.clone(),
                    self.non_finalized_state_receiver.clone(),
                );

                async move {
                    timer.finish(
                        module_path!(),
                        line!(),
                        "ReadRequest::NonFinalizedBlocksListener",
                    );

                    Ok(ReadResponse::NonFinalizedBlocksListener(
                        non_finalized_blocks_listener,
                    ))
                }
                .boxed()
            }

            ReadRequest::BondInfo(bond_key) => {
                let state = self.clone();

                tokio::task::spawn_blocking(move || {
                    span.in_scope(move || {
                        let bond_info = state.non_finalized_state_receiver.with_watch_data(
                            |non_finalized_state| non_finalized_state.best_chain().map(|chain| chain.delegation_bonds.get(&bond_key).cloned()),
                        ).flatten();

                        timer.finish(module_path!(), line!(), "ReadRequest::BondInfo");

                        let response = bond_info.map(|(bond, status)| {
                            use crate::service::non_finalized_state::BondStatusInChain;
                            BondInfoResponse {
                                amount: bond.amount,
                                status: match status {
                                    BondStatusInChain::Active => 0,
                                    BondStatusInChain::Unbonding => 1,
                                    BondStatusInChain::Withdrawn => 2,
                                    BondStatusInChain::Burned => 3,
                                },
                                last_action_height: bond.created_at.height.0,
                            }
                        });

                        Ok(ReadResponse::BondInfo(response))
                    })
                })
                .wait_for_panics()
            }
        }
    }
}

/// Initialize a state service from the provided [`Config`].
/// Returns a boxed state service, a read-only state service,
/// and receivers for state chain tip updates.
///
/// Each `network` has its own separate on-disk database.
///
/// The state uses the `max_checkpoint_height` and `checkpoint_verify_concurrency_limit`
/// to work out when it is near the final checkpoint.
///
/// To share access to the state, wrap the returned service in a `Buffer`,
/// or clone the returned [`ReadStateService`].
///
/// It's possible to construct multiple state services in the same application (as
/// long as they, e.g., use different storage locations), but doing so is
/// probably not what you want.
pub fn init(
    config: Config,
    network: &Network,
    max_checkpoint_height: block::Height,
    checkpoint_verify_concurrency_limit: usize,
    closure_to_call_crosslink: ClosureToCallIntoCrosslinkFromState,
) -> (
    BoxService<Request, Response, BoxError>,
    ReadStateService,
    LatestChainTip,
    ChainTipChange,
    crate::service::write::WriteBlockWorkerTask,
) {
    let (state_service, read_only_state_service, latest_chain_tip, chain_tip_change, block_writer) =
        StateService::new(
            config,
            network,
            max_checkpoint_height,
            checkpoint_verify_concurrency_limit,
            closure_to_call_crosslink,
        );

    (
        BoxService::new(state_service),
        read_only_state_service,
        latest_chain_tip,
        chain_tip_change,
        block_writer,
    )
}

/// Initialize a read state service from the provided [`Config`].
/// Returns a read-only state service,
///
/// Each `network` has its own separate on-disk database.
///
/// To share access to the state, clone the returned [`ReadStateService`].
pub fn init_read_only(
    config: Config,
    network: &Network,
) -> (
    ReadStateService,
    ZebraDb,
    tokio::sync::watch::Sender<NonFinalizedState>,
) {
    let finalized_state = FinalizedState::new_with_debug(
        &config,
        network,
        true,
        #[cfg(feature = "elasticsearch")]
        false,
        true,
    );
    let (non_finalized_state_sender, non_finalized_state_receiver) =
        tokio::sync::watch::channel(NonFinalizedState::new(network, Default::default()));

    (
        ReadStateService::new(&finalized_state, None, non_finalized_state_receiver),
        finalized_state.db.clone(),
        non_finalized_state_sender,
    )
}

/// Calls [`init_read_only`] with the provided [`Config`] and [`Network`] from a blocking task.
/// Returns a [`tokio::task::JoinHandle`] with a read state service and chain tip sender.
pub fn spawn_init_read_only(
    config: Config,
    network: &Network,
) -> tokio::task::JoinHandle<(
    ReadStateService,
    ZebraDb,
    tokio::sync::watch::Sender<NonFinalizedState>,
)> {
    let network = network.clone();
    tokio::task::spawn_blocking(move || init_read_only(config, &network))
}

/// Calls [`init`] with the provided [`Config`] and [`Network`] from a blocking task.
/// Returns a [`tokio::task::JoinHandle`] with a boxed state service,
/// a read state service, and receivers for state chain tip updates.
pub fn spawn_init(
    config: Config,
    network: &Network,
    max_checkpoint_height: block::Height,
    checkpoint_verify_concurrency_limit: usize,
    closure_to_call_crosslink: ClosureToCallIntoCrosslinkFromState,
) -> tokio::task::JoinHandle<(
    BoxService<Request, Response, BoxError>,
    ReadStateService,
    LatestChainTip,
    ChainTipChange,
    crate::service::write::WriteBlockWorkerTask,
)> {
    let network = network.clone();
    tokio::task::spawn_blocking(move || {
        init(
            config,
            &network,
            max_checkpoint_height,
            checkpoint_verify_concurrency_limit,
            closure_to_call_crosslink,
        )
    })
}

/// Returns a [`StateService`] with an ephemeral [`Config`] and a buffer with a single slot.
///
/// This can be used to create a state service for testing. See also [`init`].
#[cfg(any(test, feature = "proptest-impl"))]
pub fn init_test(network: &Network) -> Buffer<BoxService<Request, Response, BoxError>, Request> {
    // TODO: pass max_checkpoint_height and checkpoint_verify_concurrency limit
    //       if we ever need to test final checkpoint sent UTXO queries
    let (state_service, _, _, _, _block_writer) =
        StateService::new(Config::ephemeral(), network, block::Height::MAX, 0, Arc::new(|_,_,_| Some(true)));

    Buffer::new(BoxService::new(state_service), 1)
}

/// Initializes a state service with an ephemeral [`Config`] and a buffer with a single slot,
/// then returns the read-write service, read-only service, and tip watch channels.
///
/// This can be used to create a state service for testing. See also [`init`].
#[cfg(any(test, feature = "proptest-impl"))]
pub fn init_test_services(
    network: &Network,
) -> (
    Buffer<BoxService<Request, Response, BoxError>, Request>,
    ReadStateService,
    LatestChainTip,
    ChainTipChange,
) {
    // TODO: pass max_checkpoint_height and checkpoint_verify_concurrency limit
    //       if we ever need to test final checkpoint sent UTXO queries
    let (state_service, read_state_service, latest_chain_tip, chain_tip_change, _block_writer) =
        StateService::new(Config::ephemeral(), network, block::Height::MAX, 0, std::sync::Arc::new(|_,_,_| Some(true)));

    let state_service = Buffer::new(BoxService::new(state_service), 1);

    (
        state_service,
        read_state_service,
        latest_chain_tip,
        chain_tip_change,
    )
}


/// Process a delegation bond from a staking action.
///
/// Updates the chain's delegation bond state based on the staking action kind:
/// - CreateNewDelegationBond: creates new bond
/// - BeginDelegationUnbonding: looks up bond from self.delegation_bonds, updates status,
///   decreases staking_bonded pool, increases staking_unbonded pool
/// - WithdrawDelegationBond: looks up bond from self.delegation_bonds and updates status
///
/// expects a non-empty bond_retargets
pub fn update_chain_tip_with_delegation_bond(
    chain_value_pools: &mut zebra_chain::value_balance::ValueBalance<zebra_chain::amount::NonNegative>,
    delegation_bonds: &mut HashMap<finalized_state::disk_format::BondKey, (finalized_state::disk_format::DelegationBond, non_finalized_state::BondStatusInChain)>,
    bond_retargets: &mut Vec<HashMap<finalized_state::disk_format::BondKey, [u8; 32]>>,
    staking_action: &zcash_primitives::transaction::StakingAction,
    _transaction_hash: &zebra_chain::transaction::Hash,
    transaction_location: finalized_state::disk_format::TransactionLocation,
) -> Result<(), ValidateContextError> {
    use zcash_primitives::transaction::StakingActionKind;

    let bond_key = staking_action.arg32_0;

    match staking_action.kind {
        StakingActionKind::CreateNewDelegationBond => {
            // Extract bond data
            // Note: amount validation should have been done during contextual validation
            let amount = zebra_chain::amount::Amount::try_from(staking_action.amount_zats)
                .expect("bond amount should have been validated");
            let target_finalizer = staking_action.arg32_2;

            let bond = finalized_state::disk_format::DelegationBond::new(
                amount,
                target_finalizer,
                transaction_location,
            );

            // Insert as active bond
            // Note: duplicate bond check should have been done during contextual validation
            let previous = delegation_bonds.insert(bond_key, (bond, non_finalized_state::BondStatusInChain::Active));
            assert!(
                previous.is_none(),
                "duplicate delegation bond should have been rejected during validation"
            );
        }
        StakingActionKind::BeginDelegationUnbonding => {
            // Get the bond from delegation_bonds
            let (bond, _status) = delegation_bonds.get(&bond_key)
                .copied()
                .expect("bond must exist in chain (should have been validated)");

            // Update status to Unbonding and set created_at to current transaction location
            let updated_bond = finalized_state::disk_format::DelegationBond::new(
                bond.amount,
                bond.target_finalizer,
                transaction_location,
            );
            delegation_bonds.insert(bond_key, (updated_bond, non_finalized_state::BondStatusInChain::Unbonding));

            // Decrease staking_bonded pool by bond amount
            let current_bonded = chain_value_pools.staking_bonded_amount();
            let new_bonded = (current_bonded - bond.amount)
                .map_err(|e| ValidateContextError::InvalidDelegationBond(
                        format!("staking_bonded pool underflow when unbonding: {:?}", e)
                ))?;
            chain_value_pools.set_staking_bonded_amount(new_bonded);

            // Increase staking_unbonded pool by bond amount
            let current_unbonded = chain_value_pools.staking_unbonded_amount();
            let new_unbonded = (current_unbonded + bond.amount)
                .map_err(|e| ValidateContextError::InvalidDelegationBond(
                        format!("staking_unbonded pool overflow when unbonding: {:?}", e)
                ))?;
            chain_value_pools.set_staking_unbonded_amount(new_unbonded);
        }
        StakingActionKind::WithdrawDelegationBond => {
            // Get the bond from delegation_bonds
            let (bond, _status) = delegation_bonds.get(&bond_key)
                .copied()
                .expect("bond must exist in chain (should have been validated)");

            // Update status to Withdrawn and set created_at to current transaction location
            let updated_bond = finalized_state::disk_format::DelegationBond::new(
                bond.amount,
                bond.target_finalizer,
                transaction_location,
            );
            delegation_bonds.insert(bond_key, (updated_bond, non_finalized_state::BondStatusInChain::Withdrawn));
        }
        StakingActionKind::RetargetDelegationBond => {
            // Get the old target first (immutable borrow)
            let old_target = {
                let (bond, _status) = delegation_bonds.get(&bond_key)
                    .expect("bond must exist in chain (should have been validated)");
                bond.target_finalizer
            };

            // Record the pre-block target for this bond (only if not already recorded in this block)
            // This allows us to restore the original target on revert
            let retargets_this_block = bond_retargets.last_mut()
                .expect("bond_retargets should have been initialized for this block");
            retargets_this_block.entry(bond_key).or_insert(old_target);

            // Update only target_finalizer (not created_at or amount)
            let new_target = staking_action.arg32_2;
            let (bond, _status) = delegation_bonds.get_mut(&bond_key)
                .expect("bond must exist in chain");
            bond.target_finalizer = new_target;
        }
        // Other staking actions don't affect delegation bonds
        _ => {}
    }

    Ok(())
}

use finalized_state::disk_format::{BondKey, DelegationBond};
use non_finalized_state::BondStatusInChain;
use std::collections::BTreeSet;

pub fn burn_delegation_bonds(delegation_bonds: &mut HashMap<BondKey, (DelegationBond, BondStatusInChain)>, burn_set: &BTreeSet<BondKey>) -> Vec<(BondKey, BondStatusInChain)> {
    let mut reverts = Vec::new();
    for bond_key in burn_set {
        if let Some((_, status)) = delegation_bonds.get_mut(bond_key) {
            reverts.push((*bond_key, *status));
            *status = non_finalized_state::BondStatusInChain::Burned;
        }
    }
    reverts
}


/// caller must ensure delegation_bonds is non-empty
pub fn update_bonds_with_pos_issuance(
    bond_reward_total: u64,
    delegation_bonds: &mut HashMap<finalized_state::disk_format::BondKey, (finalized_state::disk_format::DelegationBond, non_finalized_state::BondStatusInChain)>
    ) -> Vec<([u8; 32], u64)>
{
    use zebra_chain::amount::Amount;

    /*
       Note(Sam): This is inspired from Andrews earlier code. We will use the fact the integer division only rounds
       down to make sure we do not accidentally mint zats. We first identify the biggest bond, this bond will recieve the remainder.
    */
    let mut total_staked_zats = 0u64;
    let max_staker = {
        let mut iter = delegation_bonds.iter().filter(|(_, (_, status))| *status == non_finalized_state::BondStatusInChain::Active);
        let first = iter.next().expect("is any checked already");
        let mut max_staker = *first.0;
        let mut biggest = first.1.0.amount.zatoshis() as u64;
        total_staked_zats += first.1.0.amount.zatoshis() as u64;
        for other in iter {
            total_staked_zats += other.1.0.amount.zatoshis() as u64;
            if other.1.0.amount.zatoshis() as u64 > biggest || (other.1.0.amount.zatoshis() as u64 == biggest && *other.0 < max_staker) {
                max_staker = *other.0;
                biggest = other.1.0.amount.zatoshis() as u64;
            }
        }
        max_staker
    };

    let mut so_far_payed_reward = 0u64;

    let mut reward_per_bond: Vec<([u8; 32], u64)> = Vec::new();
    for (bond_key, (bond, bond_status)) in delegation_bonds.iter_mut() {
        if *bond_status == non_finalized_state::BondStatusInChain::Active && *bond_key != max_staker {
            let mul: u128 = (bond.amount.zatoshis() as u128) * (bond_reward_total as u128);
            let reward = (mul / (total_staked_zats as u128)) as u64;
            so_far_payed_reward += reward;

            bond.amount = (bond.amount + Amount::new(reward as i64)).unwrap();
            reward_per_bond.push((*bond_key, reward,));
        }
    }

    // Biggest bond position gets the remainder.
    {
        let (bond, _status) = delegation_bonds.get_mut(&max_staker).expect("checked earlier");
        let reward = bond_reward_total - so_far_payed_reward;

        bond.amount = (bond.amount + Amount::new(reward as i64)).unwrap();
        reward_per_bond.push((max_staker, reward,));
    }

    reward_per_bond
}
