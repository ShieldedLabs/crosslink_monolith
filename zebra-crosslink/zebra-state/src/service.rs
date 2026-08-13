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
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::{Duration, Instant},
};

use futures::future::FutureExt;
use tokio::sync::oneshot;
use tower::{util::BoxService, Service, ServiceExt};
use tracing::{instrument, Instrument, Span};
use derivative::Derivative;

#[cfg(any(test, feature = "proptest-impl"))]
use tower::buffer::Buffer;

use zebra_chain::{
    block::{self, CountedHeader, HeightDiff},
    diagnostic::{task::WaitForPanics, CodeTimer},
    parameters::{HardForkSchedule, Network, NetworkUpgrade},
    serialization::ZcashSerialize,
    subtree::NoteCommitmentSubtreeIndex,
};

use zebra_chain::block::Height;
use zcash_primitives::bft::FatPointerToBftBlock;

use crate::{
    constants::{
        MAX_FIND_BLOCK_HASHES_RESULTS, MAX_FIND_BLOCK_HEADERS_RESULTS, MAX_LEGACY_CHAIN_BLOCKS,
    },
    error::{CommitBlockError, CommitCheckpointVerifiedError},
    request::TimedSpan,
    response::{BondInfoResponse, KnownBlock, NonFinalizedBlocksListener},
    service::{
        block_iter::any_ancestor_blocks,
        chain_tip::{ChainTipBlock, ChainTipChange, ChainTipSender, LatestChainTip},
        finalized_state::{FinalizedState, ZebraDb},
        non_finalized_state::{Chain, NonFinalizedState},
        pending_utxos::PendingUtxos,
        queued_blocks::QueuedBlocks,
        read::find,
        watch_receiver::WatchReceiver,
    },
    BoxError, CheckpointVerifiedBlock, CommitSemanticallyVerifiedError, Config, ReadRequest,
    ValidateContextError,
    ReadResponse, Request, Response, SemanticallyVerifiedBlock, StateInitError,
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
pub mod stake_fixup;
mod traits;
pub mod write;

#[cfg(any(test, feature = "proptest-impl"))]
pub mod arbitrary;

#[cfg(test)]
mod tests;

pub use finalized_state::{OutputLocation, TransactionIndex, TransactionLocation};

use self::queued_blocks::{QueuedCheckpointVerified, QueuedSemanticallyVerified};

pub use self::traits::{ReadState, State};

/// A read-write service for Zebra's cached blockchain state.
///
/// This service modifies and provides access to:
/// - the non-finalized state: the most recent blocks, up to
///   [`MAX_BLOCK_REORG_HEIGHT`](crate::MAX_BLOCK_REORG_HEIGHT) of them.
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

    /// Receives the hash of every non-finalized block that the write task
    /// rejected, so the corresponding entry can be removed from
    /// `non_finalized_block_write_sent_hashes`.
    ///
    /// Without this, a rejected same-hash block locks out a later honest
    /// re-delivery of a block at the same hash as a "duplicate" until restart
    /// or reorg.

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
/// - the non-finalized state: the most recent blocks, up to
///   [`MAX_BLOCK_REORG_HEIGHT`](crate::MAX_BLOCK_REORG_HEIGHT) of them.
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

        // Crosslink: the block writer is owned by new_network, so there is no write thread to
        // signal and no block-write channels to close here.

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
    pub async fn new(
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
        let (finalized_state, finalized_tip, timer) = {
            let config = config.clone();
            let network = network.clone();
            tokio::task::spawn_blocking(move || {
                let timer = CodeTimer::start();
                let finalized_state = FinalizedState::new(
                    &config,
                    &network,
                    #[cfg(feature = "elasticsearch")]
                    true,
                )
                .expect(
                    "opening the read-write finalized state database failed; check that the \
                     state cache directory is writable and not locked by another Zebra instance, \
                     and that there is free disk space",
                );
                timer.finish_desc("opening finalized state database");

                let timer = CodeTimer::start();
                let finalized_tip = finalized_state.db.tip_block();

                (finalized_state, finalized_tip, timer)
            })
            .await
            .expect("failed to join blocking task")
        };

        // # Correctness
        //
        // The state service must set the finalized block write sender to `None`
        // if there are blocks in the restored non-finalized state that are above
        // the max checkpoint height so that non-finalized blocks can be written, otherwise,
        // Zebra will be unable to commit semantically verified blocks, and its chain sync will stall.
        //
        // The state service must not set the finalized block write sender to `None` if there
        // aren't blocks in the restored non-finalized state that are above the max checkpoint height,
        // otherwise, unless checkpoint sync is disabled in the zebra-consensus configuration,
        // Zebra will be unable to commit checkpoint verified blocks, and its chain sync will stall.
        let finalized_tip_height = finalized_tip
            .as_ref()
            .map(|tip| tip.coinbase_height().expect("valid block must have height"));
        let is_finalized_tip_past_max_checkpoint =
            finalized_tip_height.is_some_and(|tip_height| tip_height >= max_checkpoint_height);
        let backup_dir_path = config.non_finalized_state_backup_dir(network);

        if backup_dir_path.is_some() && !is_finalized_tip_past_max_checkpoint {
            tracing::info!(
                ?finalized_tip_height,
                ?max_checkpoint_height,
                "not restoring the non-finalized state backup, because the finalized tip is absent \
                 or below the max checkpoint height: Zebra will re-download and re-verify the \
                 blocks above its finalized tip"
            );
        }
        let skip_backup_task = config.debug_skip_non_finalized_state_backup_task;
        let (non_finalized_state, non_finalized_state_sender, non_finalized_state_receiver) =
            NonFinalizedState::new(network, config.hardfork_schedule.clone())
                .with_backup(
                    backup_dir_path.clone(),
                    &finalized_state.db,
                    is_finalized_tip_past_max_checkpoint,
                    config.debug_skip_non_finalized_state_backup_task,
                )
                .await;

        let initial_tip = non_finalized_state
            .best_tip_block()
            .map(|cv_block| cv_block.block.clone())
            .or(finalized_tip)
            .map(CheckpointVerifiedBlock::from)
            .map(ChainTipBlock::from);

        tracing::info!(chain_tip = ?initial_tip.as_ref().map(|tip| (tip.hash, tip.height)), "loaded Zebra state cache");

        let (chain_tip_sender, latest_chain_tip, chain_tip_change) =
            ChainTipSender::new(initial_tip, network);

        // Crosslink: the writer is handed to the caller rather than spawned. new_network owns it
        // and calls it directly, so every mutation of the chain state happens on one thread in a
        // known order, with the result available synchronously.
        let block_writer = write::WriteBlockWorkerTask::new(
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
        timer.finish_desc("initializing state service");

        tracing::info!("starting legacy chain check");
        let timer = CodeTimer::start();

        if let (Some(tip), Some(nu5_activation_height)) = (
            {
                let read_state = state.read_service.clone();
                tokio::task::spawn_blocking(move || read_state.best_tip())
                    .await
                    .expect("task should not panic")
            },
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
        timer.finish_desc("legacy chain check");

        // Spawn a background task to periodically export RocksDB metrics to Prometheus
        let db_for_metrics = read_service.db.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            loop {
                interval.tick().await;
                db_for_metrics.export_metrics();
            }
        });

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
        non_finalized_state_receiver: WatchReceiver<NonFinalizedState>,
    ) -> Self {
        let read_service = Self {
            network: finalized_state.network(),
            db: finalized_state.db.clone(),
            non_finalized_state_receiver,
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
    ///
    /// Every field comes from the one source that knows the block. The chain snapshot and the
    /// live db can disagree at overlapping heights while a Crosslink finalization is landing,
    /// so per-field chain-then-db lookups could pair a hash with another block's header.
    pub fn block_header(
        &self,
        hash_or_height: crate::HashOrHeight,
    ) -> Option<(Arc<block::Header>, block::Height, block::Hash, Option<block::Hash>)> {
        let best_chain = self.latest_best_chain();
        let chain = best_chain.as_deref();

        if let Some(block) = chain.and_then(|chain| chain.block(hash_or_height)) {
            let next_block_hash = self.next_block_hash(chain, block.height, block.hash);
            return Some((block.block.header.clone(), block.height, block.hash, next_block_hash));
        }

        let height = hash_or_height.height_or_else(|hash| self.db.height(hash))?;
        let hash = hash_or_height.hash_or_else(|height| self.db.hash(height))?;
        let header = self.db.block_header(height.into())?;
        let next_block_hash = self.next_block_hash(chain, height, hash);
        Some((header, height, hash, next_block_hash))
    }

    /// Return the hash of the block above the block `hash` at `height`, from either source,
    /// but only if it actually extends `hash`: an unchecked height lookup could name a block
    /// from the other side of an in-flight Crosslink finalization.
    fn next_block_hash(
        &self,
        chain: Option<&Chain>,
        height: block::Height,
        hash: block::Hash,
    ) -> Option<block::Hash> {
        let next_height = height.next().ok()?;
        if let Some(child) = chain.and_then(|chain| chain.block(next_height.into())) {
            return (child.block.header.previous_block_hash == hash).then_some(child.hash);
        }
        let child_hash = self.db.hash(next_height)?;
        let child = self.db.block_header(next_height.into())?;
        (child.previous_block_hash == hash).then_some(child_hash)
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
    fn latest_best_chain(&self) -> Option<Arc<Chain>> {
        self.non_finalized_state_receiver
            .borrow_mapped(|non_finalized_state| non_finalized_state.best_chain().cloned())
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
        let span = Span::current();

        match req {
            // Blocks are no longer committed through the state service: new_network owns the
            // writer and calls it directly. These arms exist only because the unreachable
            // consensus verifiers still name them.
            Request::CommitSemanticallyVerifiedBlock(_)
            | Request::CommitCheckpointVerifiedBlock(_) => {
                async {
                    Err(BoxError::from(
                        "blocks are committed through new_network, not the state service",
                    ))
                }
                .boxed()
            }

            // BFT finalization is routed to new_network, which owns the block writer.
            Request::CrosslinkFinalizeBlock(finalized) => {

                // Await the channel response, flatten the result, map receive errors to
                // `CommitCheckpointVerifiedError::WriteTaskExited`.
                // Then flatten the nested Result and convert any errors to a BoxError.
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
                let timer = CodeTimer::start();
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
                        timer.finish_desc("AwaitUtxo/any-chain");

                        return Ok(Response::Utxo(utxo));
                    }

                    // We're finished, but the returned future is waiting on the respond() channel.
                    timer.finish_desc("AwaitUtxo/waiting");

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

                    timer.finish_desc("Request::KnownBlock");

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
            | Request::AnyChainTransaction(_)
            | Request::UnspentBestChainUtxo(_)
            | Request::Block(_)
            | Request::AnyChainBlock(_)
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
        let timer = CodeTimer::start_desc(req.variant_name());
        let span = Span::current();
        let timed_span = TimedSpan::new(timer, span);
        let state = self.clone();

        if let ReadRequest::NonFinalizedBlocksListener { known_chain_tips } = req {
            // The non-finalized blocks listener is used to notify the state service
            // about new blocks that have been added to the non-finalized state.
            let non_finalized_blocks_listener = NonFinalizedBlocksListener::spawn(
                self.non_finalized_state_receiver.clone(),
                known_chain_tips,
            );

            return async move {
                Ok(ReadResponse::NonFinalizedBlocksListener(
                    non_finalized_blocks_listener,
                ))
            }
            .boxed();
        };

        let request_handler = move || match req {
            // Used by the `getblockchaininfo` RPC.
            ReadRequest::UsageInfo => Ok(ReadResponse::UsageInfo(state.db.size())),

            // Used by the StateService.
            ReadRequest::Tip => Ok(ReadResponse::Tip(read::tip(
                state.latest_best_chain(),
                &state.db,
            ))),

            ReadRequest::FinalizedTip => Ok(ReadResponse::Tip(state.finalized_tip())),

            // Used by `getblockchaininfo` RPC method.
            ReadRequest::TipPoolValues => {
                let (tip_height, tip_hash, value_balance) =
                    read::tip_with_value_balance(state.latest_best_chain(), &state.db)?
                        .ok_or(BoxError::from("no chain tip available yet"))?;

                Ok(ReadResponse::TipPoolValues {
                    tip_height,
                    tip_hash,
                    value_balance,
                })
            }

            // Used by getblock
            ReadRequest::BlockInfo(hash_or_height) => Ok(ReadResponse::BlockInfo(
                read::block_info(state.latest_best_chain(), &state.db, hash_or_height),
            )),

            // Used by the StateService.
            ReadRequest::Depth(hash) => Ok(ReadResponse::Depth(read::depth(
                state.latest_best_chain(),
                &state.db,
                hash,
            ))),

            // Used by the StateService.
            ReadRequest::BestChainNextMedianTimePast => {
                Ok(ReadResponse::BestChainNextMedianTimePast(
                    read::next_median_time_past(&state.latest_non_finalized_state(), &state.db)?,
                ))
            }

            // Used by the get_block (raw) RPC and the StateService.
            ReadRequest::Block(hash_or_height) => Ok(ReadResponse::Block(read::block(
                state.latest_best_chain(),
                &state.db,
                hash_or_height,
            ))),

            ReadRequest::AnyChainBlock(hash_or_height) => Ok(ReadResponse::Block(read::any_block(
                state.latest_non_finalized_state().chain_iter(),
                &state.db,
                hash_or_height,
            ))),

            // Like ReadRequest::Block, but searches all non-finalized chains.
            ReadRequest::BlockButAlsoAllChains(hash_or_height) => {
                let block = state.block_from_any_chain(hash_or_height);


                Ok(ReadResponse::BlockButAlsoAllChains(block))
            }

            // Used by the get_block (raw) RPC and the StateService.
            ReadRequest::BlockAndSize(hash_or_height) => Ok(ReadResponse::BlockAndSize(
                read::block_and_size(state.latest_best_chain(), &state.db, hash_or_height),
            )),

            // Used by the get_block (verbose) RPC and the StateService.
            ReadRequest::BlockHeader(hash_or_height) => {
                let best_chain = state.latest_best_chain();

                let height = hash_or_height
                    .height_or_else(|hash| {
                        read::find::height_by_hash(best_chain.clone(), &state.db, hash)
                    })
                    .ok_or_else(|| BoxError::from("block hash or height not found"))?;

                let hash = hash_or_height
                    .hash_or_else(|height| {
                        read::find::hash_by_height(best_chain.clone(), &state.db, height)
                    })
                    .ok_or_else(|| BoxError::from("block hash or height not found"))?;

                let next_height = height.next()?;
                let next_block_hash =
                    read::find::hash_by_height(best_chain.clone(), &state.db, next_height);

                let header = read::block_header(best_chain, &state.db, height.into())
                    .ok_or_else(|| BoxError::from("block hash or height not found"))?;

                Ok(ReadResponse::BlockHeader {
                    header,
                    hash,
                    height,
                    next_block_hash,
                })
            }

            // For the get_raw_transaction RPC and the StateService.
            ReadRequest::Transaction(hash) => Ok(ReadResponse::Transaction(
                read::mined_transaction(state.latest_best_chain(), &state.db, hash),
            )),

            ReadRequest::AnyChainTransaction(hash) => {
                Ok(ReadResponse::AnyChainTransaction(read::any_transaction(
                    state.latest_non_finalized_state().chain_iter(),
                    &state.db,
                    hash,
                )))
            }

            // Used by the getblock (verbose) RPC.
            ReadRequest::TransactionIdsForBlock(hash_or_height) => Ok(
                ReadResponse::TransactionIdsForBlock(read::transaction_hashes_for_block(
                    state.latest_best_chain(),
                    &state.db,
                    hash_or_height,
                )),
            ),

            ReadRequest::AnyChainTransactionIdsForBlock(hash_or_height) => {
                Ok(ReadResponse::AnyChainTransactionIdsForBlock(
                    read::transaction_hashes_for_any_block(
                        state.latest_non_finalized_state().chain_iter(),
                        &state.db,
                        hash_or_height,
                    ),
                ))
            }

            #[cfg(feature = "indexer")]
            ReadRequest::SpendingTransactionId(spend) => Ok(ReadResponse::TransactionId(
                read::spending_transaction_hash(state.latest_best_chain(), &state.db, spend),
            )),

            ReadRequest::UnspentBestChainUtxo(outpoint) => Ok(ReadResponse::UnspentBestChainUtxo(
                read::unspent_utxo(state.latest_best_chain(), &state.db, outpoint),
            )),

            // Manually used by the StateService to implement part of AwaitUtxo.
            ReadRequest::AnyChainUtxo(outpoint) => Ok(ReadResponse::AnyChainUtxo(read::any_utxo(
                state.latest_non_finalized_state(),
                &state.db,
                outpoint,
            ))),

            // Used by the StateService.
            ReadRequest::BlockLocator => Ok(ReadResponse::BlockLocator(
                read::block_locator(state.latest_best_chain(), &state.db).unwrap_or_default(),
            )),

            // Used by the StateService.
            ReadRequest::FindBlockHashes { known_blocks, stop } => {
                Ok(ReadResponse::BlockHashes(read::find_chain_hashes(
                    state.latest_best_chain(),
                    &state.db,
                    known_blocks,
                    stop,
                    MAX_FIND_BLOCK_HASHES_RESULTS,
                )))
            }

            // Used by the StateService.
            ReadRequest::FindBlockHeaders { known_blocks, stop } => Ok(ReadResponse::BlockHeaders(
                read::find_chain_headers(
                    state.latest_best_chain(),
                    &state.db,
                    known_blocks,
                    stop,
                    MAX_FIND_BLOCK_HEADERS_RESULTS,
                )
                .into_iter()
                .map(|header| CountedHeader { header })
                .collect(),
            )),

            ReadRequest::FindForkPoint { known_blocks } => {
                // Reject over-long locators before doing any work, so an untrusted
                // caller can't force unbounded lookups.
                let locator_len: u64 = known_blocks
                    .len()
                    .try_into()
                    .expect("usize always fits in u64 on supported (<=64-bit) platforms");
                if locator_len > block::MAX_BLOCK_LOCATOR_LENGTH {
                    return Err(BoxError::from(format!(
                        "FindForkPoint locator length {locator_len} exceeds \
                         MAX_BLOCK_LOCATOR_LENGTH ({})",
                        block::MAX_BLOCK_LOCATOR_LENGTH,
                    )));
                }

                Ok(ReadResponse::ForkPoint(read::find_fork_point(
                    state.latest_best_chain(),
                    &state.db,
                    known_blocks,
                )))
            }

            ReadRequest::SaplingTree(hash_or_height) => Ok(ReadResponse::SaplingTree(
                read::sapling_tree(state.latest_best_chain(), &state.db, hash_or_height),
            )),

            ReadRequest::OrchardTree(hash_or_height) => Ok(ReadResponse::OrchardTree(
                read::orchard_tree(state.latest_best_chain(), &state.db, hash_or_height),
            )),

            ReadRequest::IronwoodTree(hash_or_height) => Ok(ReadResponse::IronwoodTree(
                read::ironwood_tree(state.latest_best_chain(), &state.db, hash_or_height),
            )),

            ReadRequest::SaplingSubtrees { start_index, limit } => {
                let end_index = limit
                    .and_then(|limit| start_index.0.checked_add(limit.0))
                    .map(NoteCommitmentSubtreeIndex);

                let best_chain = state.latest_best_chain();
                let sapling_subtrees = if let Some(end_index) = end_index {
                    read::sapling_subtrees(best_chain, &state.db, start_index..end_index)
                } else {
                    // If there is no end bound, just return all the trees.
                    // If the end bound would overflow, just returns all the trees, because that's what
                    // `zcashd` does. (It never calculates an end bound, so it just keeps iterating until
                    // the trees run out.)
                    read::sapling_subtrees(best_chain, &state.db, start_index..)
                };

                Ok(ReadResponse::SaplingSubtrees(sapling_subtrees))
            }

            ReadRequest::OrchardSubtrees { start_index, limit } => {
                let end_index = limit
                    .and_then(|limit| start_index.0.checked_add(limit.0))
                    .map(NoteCommitmentSubtreeIndex);

                let best_chain = state.latest_best_chain();
                let orchard_subtrees = if let Some(end_index) = end_index {
                    read::orchard_subtrees(best_chain, &state.db, start_index..end_index)
                } else {
                    // If there is no end bound, just return all the trees.
                    // If the end bound would overflow, just returns all the trees, because that's what
                    // `zcashd` does. (It never calculates an end bound, so it just keeps iterating until
                    // the trees run out.)
                    read::orchard_subtrees(best_chain, &state.db, start_index..)
                };

                Ok(ReadResponse::OrchardSubtrees(orchard_subtrees))
            }

            ReadRequest::IronwoodSubtrees { start_index, limit } => {
                let end_index = limit
                    .and_then(|limit| start_index.0.checked_add(limit.0))
                    .map(NoteCommitmentSubtreeIndex);

                let best_chain = state.latest_best_chain();
                let ironwood_subtrees = if let Some(end_index) = end_index {
                    read::ironwood_subtrees(best_chain, &state.db, start_index..end_index)
                } else {
                    // If there is no end bound, just return all the trees.
                    // If the end bound would overflow, just returns all the trees, because that's what
                    // `zcashd` does. (It never calculates an end bound, so it just keeps iterating until
                    // the trees run out.)
                    read::ironwood_subtrees(best_chain, &state.db, start_index..)
                };

                Ok(ReadResponse::IronwoodSubtrees(ironwood_subtrees))
            }

            // For the get_address_balance RPC.
            ReadRequest::AddressBalance(addresses) => {
                let (balance, received) =
                    read::transparent_balance(state.latest_best_chain(), &state.db, addresses)?;
                Ok(ReadResponse::AddressBalance { balance, received })
            }

            // For the get_address_tx_ids RPC.
            ReadRequest::TransactionIdsByAddresses {
                addresses,
                height_range,
            } => read::transparent_tx_ids(
                state.latest_best_chain(),
                &state.db,
                addresses,
                height_range,
            )
            .map(ReadResponse::AddressesTransactionIds),

            // For the get_address_utxos RPC.
            ReadRequest::UtxosByAddresses(addresses) => read::address_utxos(
                &state.network,
                state.latest_best_chain(),
                &state.db,
                addresses,
            )
            .map(ReadResponse::AddressUtxos),

            ReadRequest::CheckBestChainTipNullifiersAndAnchors(unmined_tx) => {
                let latest_non_finalized_best_chain = state.latest_best_chain();

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

                Ok(ReadResponse::ValidBestChainTipNullifiersAndAnchors)
            }

            // Used by the get_block and get_block_hash RPCs.
            ReadRequest::BestChainBlockHash(height) => Ok(ReadResponse::BlockHash(
                read::hash_by_height(state.latest_best_chain(), &state.db, height),
            )),

            // Used by get_block_template and getblockchaininfo RPCs.
            ReadRequest::ChainInfo => {
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
                read::difficulty::get_block_template_chain_info(
                    &state.latest_non_finalized_state(),
                    &state.db,
                    &state.network,
                )
                .map(ReadResponse::ChainInfo)
            }

            // Used by getmininginfo, getnetworksolps, and getnetworkhashps RPCs.
            ReadRequest::SolutionRate { num_blocks, height } => {
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

                Ok(ReadResponse::SolutionRate(solution_rate))
            }

            ReadRequest::CheckBlockProposalValidity(semantically_verified) => {
                tracing::debug!(
                    "attempting to validate and commit block proposal \
                         onto a cloned non-finalized state"
                );
                let mut latest_non_finalized_state = state.latest_non_finalized_state();

                // The previous block of a valid proposal must be on the best chain tip.
                let Some((_best_tip_height, best_tip_hash)) =
                    read::best_tip(&latest_non_finalized_state, &state.db)
                else {
                    return Err(
                        "state is empty: wait for Zebra to sync before submitting a proposal"
                            .into(),
                    );
                };

                if semantically_verified.block.header.previous_block_hash != best_tip_hash {
                    return Err("proposal is not based on the current best chain tip: \
                                    previous block hash must be the best chain tip"
                        .into());
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

                Ok(ReadResponse::ValidBlockProposal)
            }

            ReadRequest::TipBlockSize => {
                // Respond with the length of the obtained block if any.
                Ok(ReadResponse::TipBlockSize(
                    state
                        .best_tip()
                        .and_then(|(tip_height, _)| {
                            read::block_info(
                                state.latest_best_chain(),
                                &state.db,
                                tip_height.into(),
                            )
                        })
                        .map(|info| info.size().try_into().expect("u32 should fit in usize"))
                        .or_else(|| {
                            find::tip_block(state.latest_best_chain(), &state.db)
                                .map(|b| b.zcash_serialized_size())
                        }),
                ))
            }

            ReadRequest::NonFinalizedBlocksListener { .. } => {
                unreachable!("should return early");
            }

            ReadRequest::BondInfo(bond_key) => {
                let bond_info = state
                    .non_finalized_state_receiver
                    .with_watch_data(|non_finalized_state| {
                        non_finalized_state
                            .best_chain()
                            .map(|chain| chain.delegation_bonds.get(&bond_key).cloned())
                    })
                    .flatten();

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
            }

            // Used by the visualizer to render forks alongside the best chain: it follows each
            // one with a BlockSequence anchored on the tip.
            ReadRequest::SidechainForks => {
                let forks = state
                    .non_finalized_state_receiver
                    .with_watch_data(|non_finalized_state| read::sidechain_forks(&non_finalized_state));

                Ok(ReadResponse::SidechainForks(forks))
            }

            // A run of blocks read from one snapshot, so the caller cannot see half of one
            // chain and half of another. Used by the visualizer for its whole window.
            ReadRequest::BlockSequence {
                anchor,
                hi_height,
                lo_height,
                max_len,
            } => {
                let seq = state
                    .non_finalized_state_receiver
                    .with_watch_data(|non_finalized_state| {
                        read::block_sequence(
                            &non_finalized_state,
                            &state.db,
                            anchor,
                            hi_height,
                            lo_height,
                            max_len,
                        )
                    });

                Ok(ReadResponse::BlockSequence(seq))
            }

            // Used by `gettxout` RPC method.
            ReadRequest::IsTransparentOutputSpent(outpoint) => {
                let is_spent = read::unspent_utxo(state.latest_best_chain(), &state.db, outpoint);
                Ok(ReadResponse::IsTransparentOutputSpent(is_spent.is_none()))
            }
        };

        timed_span.spawn_blocking(request_handler)
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
pub async fn init(
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
        )
        .await;

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
) -> Result<
    (
        ReadStateService,
        ZebraDb,
        tokio::sync::watch::Sender<NonFinalizedState>,
    ),
    StateInitError,
> {
    let finalized_state = FinalizedState::new_with_debug(
        &config,
        network,
        true,
        #[cfg(feature = "elasticsearch")]
        false,
        true,
    )?;
    let (non_finalized_state_sender, non_finalized_state_receiver) =
        tokio::sync::watch::channel(NonFinalizedState::new(network, Default::default()));

    Ok((
        ReadStateService::new(
            &finalized_state,
            None,
            WatchReceiver::new(non_finalized_state_receiver),
        ),
        finalized_state.db.clone(),
        non_finalized_state_sender,
    ))
}

/// Calls [`init_read_only`] with the provided [`Config`] and [`Network`] from a blocking task.
///
/// Returns a [`tokio::task::JoinHandle`] whose output is a [`Result`]: awaiting it yields a
/// [`JoinError`](tokio::task::JoinError) if the blocking task panicked or was cancelled, and
/// otherwise an `Err(`[`StateInitError`]`)` if the read-only state could not be opened (for
/// example, a missing read-only database).
pub fn spawn_init_read_only(
    config: Config,
    network: &Network,
) -> tokio::task::JoinHandle<
    Result<
        (
            ReadStateService,
            ZebraDb,
            tokio::sync::watch::Sender<NonFinalizedState>,
        ),
        StateInitError,
    >,
> {
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
    tokio::task::spawn(async move {
        init(
            config,
            &network,
            max_checkpoint_height,
            checkpoint_verify_concurrency_limit,
            closure_to_call_crosslink,
        )
        .await
    })
}

/// Returns a [`StateService`] with an ephemeral [`Config`] and a buffer with a single slot.
///
/// This can be used to create a state service for testing. See also [`init`].
#[cfg(any(test, feature = "proptest-impl"))]
pub async fn init_test(
    network: &Network,
) -> Buffer<BoxService<Request, Response, BoxError>, Request> {
    // TODO: pass max_checkpoint_height and checkpoint_verify_concurrency limit
    //       if we ever need to test final checkpoint sent UTXO queries
    let (state_service, _, _, _, _block_writer) =
        StateService::new(Config::ephemeral(), network, block::Height::MAX, 0, Arc::new(|_,_,_| Some(true))).await;

    Buffer::new(BoxService::new(state_service), 1)
}

/// Initializes a state service with an ephemeral [`Config`] and a buffer with a single slot,
/// then returns the read-write service, read-only service, and tip watch channels.
///
/// This can be used to create a state service for testing. See also [`init`].
#[cfg(any(test, feature = "proptest-impl"))]
pub async fn init_test_services(
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
        StateService::new(Config::ephemeral(), network, block::Height::MAX, 0, std::sync::Arc::new(|_,_,_| Some(true))).await;

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
