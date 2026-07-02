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
        queued_blocks::QueuedBlocks,
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
mod write;

#[cfg(any(test, feature = "proptest-impl"))]
pub mod arbitrary;

#[cfg(test)]
mod tests;

pub use finalized_state::{OutputLocation, TransactionIndex, TransactionLocation};
use write::NonFinalizedWriteMessage;

use self::queued_blocks::{QueuedCheckpointVerified, QueuedSemanticallyVerified, SentHashes};

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
    full_verifier_utxo_lookahead: block::Height,

    // Queued Blocks
    //
    /// Queued blocks for the [`NonFinalizedState`] that arrived out of order.
    /// These blocks are awaiting their parent blocks before they can do contextual verification.
    non_finalized_state_queued_blocks: QueuedBlocks,

    /// Queued blocks for the [`FinalizedState`] that arrived out of order.
    /// These blocks are awaiting their parent blocks before they can do contextual verification.
    ///
    /// Indexed by their parent block hash.
    finalized_state_queued_blocks: HashMap<block::Hash, QueuedCheckpointVerified>,

    /// Channels to send blocks to the block write task.
    block_write_sender: write::BlockWriteSender,

    /// The [`block::Hash`] of the most recent block sent on
    /// `finalized_block_write_sender` or `non_finalized_block_write_sender`.
    ///
    /// On startup, this is:
    /// - the finalized tip, if there are stored blocks, or
    /// - the genesis block's parent hash, if the database is empty.
    ///
    /// If `invalid_block_write_reset_receiver` gets a reset, this is:
    /// - the hash of the last valid committed block (the parent of the invalid block).
    finalized_block_write_last_sent_hash: block::Hash,

    /// A set of block hashes that have been sent to the block write task.
    /// Hashes of blocks below the finalized tip height are periodically pruned.
    non_finalized_block_write_sent_hashes: SentHashes,

    /// If an invalid block is sent on `finalized_block_write_sender`
    /// or `non_finalized_block_write_sender`,
    /// this channel gets the [`block::Hash`] of the valid tip.
    //
    // TODO: add tests for finalized and non-finalized resets (#2654)
    invalid_block_write_reset_receiver: tokio::sync::mpsc::UnboundedReceiver<block::Hash>,

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
    max_finalized_queue_height: f64,

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

        // Close the channels (non-blocking)
        // This makes the block write thread exit the next time it checks the channels.
        // We want to do this here so we get any errors or panics from the block write task before it shuts down.
        self.invalid_block_write_reset_receiver.close();

        std::mem::drop(self.block_write_sender.finalized.take());
        std::mem::drop(self.block_write_sender.non_finalized.take());

        self.clear_finalized_block_queue(
            "dropping the state: dropped unused finalized state queue block",
        );
        self.clear_non_finalized_block_queue(CommitSemanticallyVerifiedError::from(
            ValidateContextError::DroppedUnusedBlock,
        ));

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
    ) -> (Self, ReadStateService, LatestChainTip, ChainTipChange) {
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

        let finalized_state_for_writing = finalized_state.clone();
        let (block_write_sender, invalid_block_write_reset_receiver, block_write_task) =
            write::BlockWriteSender::spawn(
                finalized_state_for_writing,
                non_finalized_state,
                chain_tip_sender,
                non_finalized_state_sender,
            );

        let read_service = ReadStateService::new(
            &finalized_state,
            block_write_task,
            non_finalized_state_receiver,
        );

        let full_verifier_utxo_lookahead = max_checkpoint_height
            - HeightDiff::try_from(checkpoint_verify_concurrency_limit)
                .expect("fits in HeightDiff");
        let full_verifier_utxo_lookahead =
            full_verifier_utxo_lookahead.unwrap_or(block::Height::MIN);
        let non_finalized_state_queued_blocks = QueuedBlocks::default();
        let pending_utxos = PendingUtxos::default();

        let finalized_block_write_last_sent_hash = finalized_state.db.finalized_tip_hash();

        let state = Self {
            network: network.clone(),
            full_verifier_utxo_lookahead,
            non_finalized_state_queued_blocks,
            finalized_state_queued_blocks: HashMap::new(),
            block_write_sender,
            finalized_block_write_last_sent_hash,
            non_finalized_block_write_sent_hashes: SentHashes::default(),
            invalid_block_write_reset_receiver,
            pending_utxos,
            last_prune: Instant::now(),
            read_service: read_service.clone(),
            max_finalized_queue_height: f64::NAN,
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

        (state, read_service, latest_chain_tip, chain_tip_change)
    }

    /// Call read only state service to log rocksdb database metrics.
    pub fn log_db_metrics(&self) {
        self.read_service.db.print_db_metrics();
    }

    /// Queue a checkpoint verified block for verification and storage in the finalized state.
    ///
    /// Returns a channel receiver that provides the result of the block commit.
    fn queue_and_commit_to_finalized_state(
        &mut self,
        checkpoint_verified: CheckpointVerifiedBlock,
    ) -> oneshot::Receiver<Result<block::Hash, BoxError>> {
        // # Correctness & Performance
        //
        // This method must not block, access the database, or perform CPU-intensive tasks,
        // because it is called directly from the tokio executor's Future threads.

        let queued_prev_hash = checkpoint_verified.block.header.previous_block_hash;
        let queued_height = checkpoint_verified.height;

        // If we're close to the final checkpoint, make the block's UTXOs available for
        // semantic block verification, even when it is in the channel.
        if self.is_close_to_final_checkpoint(queued_height) {
            self.non_finalized_block_write_sent_hashes
                .add_finalized(&checkpoint_verified)
        }

        let (rsp_tx, rsp_rx) = oneshot::channel();
        let queued = (checkpoint_verified, rsp_tx);

        if self.block_write_sender.finalized.is_some() {
            // We're still committing checkpoint verified blocks
            if let Some(duplicate_queued) = self
                .finalized_state_queued_blocks
                .insert(queued_prev_hash, queued)
            {
                Self::send_checkpoint_verified_block_error(
                    duplicate_queued,
                    "dropping older checkpoint verified block: got newer duplicate block",
                );
            }

            self.drain_finalized_queue_and_commit();
        } else {
            // We've finished committing checkpoint verified blocks to the finalized state,
            // so drop any repeated queued blocks, and return an error.
            //
            // TODO: track the latest sent height, and drop any blocks under that height
            //       every time we send some blocks (like QueuedSemanticallyVerifiedBlocks)
            Self::send_checkpoint_verified_block_error(
                queued,
                "already finished committing checkpoint verified blocks: dropped duplicate block, \
                 block is already committed to the state",
            );

            self.clear_finalized_block_queue(
                "already finished committing checkpoint verified blocks: dropped duplicate block, \
                 block is already committed to the state",
            );
        }

        if self.finalized_state_queued_blocks.is_empty() {
            self.max_finalized_queue_height = f64::NAN;
        } else if self.max_finalized_queue_height.is_nan()
            || self.max_finalized_queue_height < queued_height.0 as f64
        {
            // if there are still blocks in the queue, then either:
            //   - the new block was lower than the old maximum, and there was a gap before it,
            //     so the maximum is still the same (and we skip this code), or
            //   - the new block is higher than the old maximum, and there is at least one gap
            //     between the finalized tip and the new maximum
            self.max_finalized_queue_height = queued_height.0 as f64;
        }

        metrics::gauge!("state.checkpoint.queued.max.height").set(self.max_finalized_queue_height);
        metrics::gauge!("state.checkpoint.queued.block.count")
            .set(self.finalized_state_queued_blocks.len() as f64);

        rsp_rx
    }

    /// Finds finalized state queue blocks to be committed to the state in order,
    /// removes them from the queue, and sends them to the block commit task.
    ///
    /// After queueing a finalized block, this method checks whether the newly
    /// queued block (and any of its descendants) can be committed to the state.
    ///
    /// Returns an error if the block commit channel has been closed.
    pub fn drain_finalized_queue_and_commit(&mut self) {
        use tokio::sync::mpsc::error::{SendError, TryRecvError};

        // # Correctness & Performance
        //
        // This method must not block, access the database, or perform CPU-intensive tasks,
        // because it is called directly from the tokio executor's Future threads.

        // If a block failed, we need to start again from a valid tip.
        match self.invalid_block_write_reset_receiver.try_recv() {
            Ok(reset_tip_hash) => self.finalized_block_write_last_sent_hash = reset_tip_hash,
            Err(TryRecvError::Disconnected) => {
                info!("Block commit task closed the block reset channel. Is Zebra shutting down?");
                return;
            }
            // There are no errors, so we can just use the last block hash we sent
            Err(TryRecvError::Empty) => {}
        }

        while let Some(queued_block) = self
            .finalized_state_queued_blocks
            .remove(&self.finalized_block_write_last_sent_hash)
        {
            let last_sent_finalized_block_height = queued_block.0.height;

            self.finalized_block_write_last_sent_hash = queued_block.0.hash;

            // If we've finished sending finalized blocks, ignore any repeated blocks.
            // (Blocks can be repeated after a syncer reset.)
            if let Some(finalized_block_write_sender) = &self.block_write_sender.finalized {
                let send_result = finalized_block_write_sender.send(queued_block);

                // If the receiver is closed, we can't send any more blocks.
                if let Err(SendError(queued)) = send_result {
                    // If Zebra is shutting down, drop blocks and return an error.
                    Self::send_checkpoint_verified_block_error(
                        queued,
                        "block commit task exited. Is Zebra shutting down?",
                    );

                    self.clear_finalized_block_queue(
                        "block commit task exited. Is Zebra shutting down?",
                    );
                } else {
                    metrics::gauge!("state.checkpoint.sent.block.height")
                        .set(last_sent_finalized_block_height.0 as f64);
                };
            }
        }
    }

    /// Drops all finalized state queue blocks, and sends an error on their result channels.
    fn clear_finalized_block_queue(&mut self, error: impl Into<BoxError> + Clone) {
        for (_hash, queued) in self.finalized_state_queued_blocks.drain() {
            Self::send_checkpoint_verified_block_error(queued, error.clone());
        }
    }

    /// Send an error on a `QueuedCheckpointVerified` block's result channel, and drop the block
    fn send_checkpoint_verified_block_error(
        queued: QueuedCheckpointVerified,
        error: impl Into<BoxError>,
    ) {
        let (finalized, rsp_tx) = queued;

        // The block sender might have already given up on this block,
        // so ignore any channel send errors.
        let _ = rsp_tx.send(Err(error.into()));
        std::mem::drop(finalized);
    }

    /// Drops all non-finalized state queue blocks, and sends an error on their result channels.
    fn clear_non_finalized_block_queue(&mut self, error: CommitSemanticallyVerifiedError) {
        for (_hash, queued) in self.non_finalized_state_queued_blocks.drain() {
            Self::send_semantically_verified_block_error(queued, error.clone());
        }
    }

    /// Send an error on a `QueuedSemanticallyVerified` block's result channel, and drop the block
    fn send_semantically_verified_block_error(
        queued: QueuedSemanticallyVerified,
        error: CommitSemanticallyVerifiedError,
    ) {
        let (finalized, rsp_tx) = queued;

        // The block sender might have already given up on this block,
        // so ignore any channel send errors.
        let _ = rsp_tx.send(Err(error));
        std::mem::drop(finalized);
    }

    /// Queue a semantically verified block for contextual verification and check if any queued
    /// blocks are ready to be verified and committed to the state.
    ///
    /// This function encodes the logic for [committing non-finalized blocks][1]
    /// in RFC0005.
    ///
    /// [1]: https://zebra.zfnd.org/dev/rfcs/0005-state-updates.html#committing-non-finalized-blocks
    #[instrument(level = "debug", skip(self, semantically_verified))]
    fn queue_and_commit_to_non_finalized_state(
        &mut self,
        semantically_verified: SemanticallyVerifiedBlock,
    ) -> oneshot::Receiver<Result<block::Hash, CommitSemanticallyVerifiedError>> {
        tracing::debug!(block = %semantically_verified.block, "queueing block for contextual verification");
        let block_height = semantically_verified.block.coinbase_height().unwrap_or(zebra_chain::block::Height(0));
        let parent_hash = semantically_verified.block.header.previous_block_hash;
        let parent_block_header = self.read_service.non_finalized_state_receiver.with_watch_data(
            |non_finalized_state| {
                let mut ret = None;
                for chain in non_finalized_state.chain_iter() {
                    if ret.is_none() {
                        ret = chain.block(crate::HashOrHeight::Hash(parent_hash)).map(|b| b.block.header.clone());
                    }
                }
                ret
            },
        );
        let parent_block_header = if parent_block_header.is_some() { parent_block_header } else { self.read_service.db.block_header(crate::HashOrHeight::Hash(parent_hash)) };
        let parent_block_fat_pointer = parent_block_header.map(|h| h.fat_pointer_to_bft_block.clone());

        let this_header_fat_pointer = semantically_verified.block.header.fat_pointer_to_bft_block.clone();
        let semantically_verified_height = semantically_verified.height;

        // BAD? Bug? sent_hashes is never cleaned up on failure, so this can
        // permanently block re-commits. But removing it causes duplicate chains
        // in non_finalized_state which crashes in chain.rs Ord impl.
        if self
            .non_finalized_block_write_sent_hashes
            .contains(&semantically_verified.hash)
        {
            let (rsp_tx, rsp_rx) = oneshot::channel();
            let _ = rsp_tx.send(Err(CommitSemanticallyVerifiedError::from(
                ValidateContextError::DuplicateCommitRequest {
                    block_hash: semantically_verified.hash,
                },
            )));
            return rsp_rx;
        }

        if self
            .read_service
            .db
            .contains_height(semantically_verified.height)
        {
            let (rsp_tx, rsp_rx) = oneshot::channel();
            let _ = rsp_tx.send(Err(CommitSemanticallyVerifiedError::from(
                ValidateContextError::AlreadyFinalized {
                    block_height: semantically_verified.height,
                },
            )));
            return rsp_rx;
        }

        // [`Request::CommitSemanticallyVerifiedBlock`] contract: a request to commit a block which
        // has been queued but not yet committed to the state fails the older request and replaces
        // it with the newer request.
        let rsp_rx = if let Some((_, old_rsp_tx)) = self
            .non_finalized_state_queued_blocks
            .get_mut(&semantically_verified.hash)
        {
            tracing::debug!("replacing older queued request with new request");
            let (mut rsp_tx, rsp_rx) = oneshot::channel();
            std::mem::swap(old_rsp_tx, &mut rsp_tx);
            let _ = rsp_tx.send(Err(CommitSemanticallyVerifiedError::from(
                ValidateContextError::ReplacedByNewerRequest {
                    block_hash: semantically_verified.hash,
                },
            )));
            rsp_rx
        } else {
            let (rsp_tx, rsp_rx) = oneshot::channel();
            self.non_finalized_state_queued_blocks
                .queue((semantically_verified, rsp_tx));
            rsp_rx
        };

        // We've finished sending checkpoint verified blocks when:
        // - we've sent the verified block for the last checkpoint, and
        // - it has been successfully written to disk.
        //
        // We detect the last checkpoint by looking for non-finalized blocks
        // that are a child of the last block we sent.
        //
        // TODO: configure the state with the last checkpoint hash instead?
        if self.block_write_sender.finalized.is_some()
            && self
                .non_finalized_state_queued_blocks
                .has_queued_children(self.finalized_block_write_last_sent_hash)
            && self.read_service.db.finalized_tip_hash()
                == self.finalized_block_write_last_sent_hash
        {
            // CROSSLINK
            if block_height != zebra_chain::block::Height(32265) && block_height != zebra_chain::block::Height(32266) && (parent_block_fat_pointer.is_none() || Some(true) != (self.closure_to_call_crosslink)(parent_block_fat_pointer.unwrap(), this_header_fat_pointer, block_height)) {
                let (rsp_tx, rsp_rx) = oneshot::channel();
                let _ = rsp_tx.send(Err(CommitSemanticallyVerifiedError::from(
                    ValidateContextError::CrosslinkNotReady {
                        block_height: semantically_verified_height,
                    },
                )));
                return rsp_rx;
            }

            // Tell the block write task to stop committing checkpoint verified blocks to the finalized state,
            // and move on to committing semantically verified blocks to the non-finalized state.
            std::mem::drop(self.block_write_sender.finalized.take());
            // Remove any checkpoint-verified block hashes from `non_finalized_block_write_sent_hashes`.
            self.non_finalized_block_write_sent_hashes = SentHashes::default();
            // Mark `SentHashes` as usable by the `can_fork_chain_at()` method.
            self.non_finalized_block_write_sent_hashes
                .can_fork_chain_at_hashes = true;
            // Send blocks from non-finalized queue
            self.send_ready_non_finalized_queued(self.finalized_block_write_last_sent_hash);
            // We've finished committing checkpoint verified blocks to finalized state, so drop any repeated queued blocks.
            self.clear_finalized_block_queue(
                "already finished committing checkpoint verified blocks: dropped duplicate block, \
                 block is already committed to the state",
            );
        } else if !self.can_fork_chain_at(&parent_hash) {
            tracing::trace!("unready to verify, returning early");
        } else if self.block_write_sender.finalized.is_none() {
            // CROSSLINK
            if block_height != zebra_chain::block::Height(32265) && block_height != zebra_chain::block::Height(32266) && (parent_block_fat_pointer.is_none() || Some(true) != (self.closure_to_call_crosslink)(parent_block_fat_pointer.unwrap(), this_header_fat_pointer, block_height)) {
                let (rsp_tx, rsp_rx) = oneshot::channel();
                let _ = rsp_tx.send(Err(CommitSemanticallyVerifiedError::from(
                    ValidateContextError::CrosslinkNotReady {
                        block_height: semantically_verified_height,
                    },
                )));
                return rsp_rx;
            }

            // Wait until block commit task is ready to write non-finalized blocks before dequeuing them
            self.send_ready_non_finalized_queued(parent_hash);

            let finalized_tip_height = self.read_service.db.finalized_tip_height().expect(
                "Finalized state must have at least one block before committing non-finalized state",
            );

            self.non_finalized_state_queued_blocks
                .prune_by_height(finalized_tip_height);

            self.non_finalized_block_write_sent_hashes
                .prune_by_height(finalized_tip_height);
        }

        rsp_rx
    }

    fn send_crosslink_finalized_to_non_finalized_state(
        &mut self,
        hash: block::Hash,
    ) -> oneshot::Receiver<Result<(block::Hash, Vec<([u8; 32], u64)>), BoxError>> {
        let (rsp_tx, rsp_rx) = oneshot::channel();

        if self.block_write_sender.finalized.is_none() {
            if let Some(tx) = &self.block_write_sender.non_finalized {
                if let Err(err) =
                    tx.send(NonFinalizedWriteMessage::CrosslinkFinalized(hash, rsp_tx))
                {
                    tracing::warn!(
                        ?err,
                        "failed to send Crosslink-finalized hash to NonFinalizedState"
                    );
                };
            } else {
                let _ = rsp_tx.send(Err("not ready to crosslink-finalize blocks".into()));
            }
        } else {
            let _ = rsp_tx.send(Err("not ready to crosslink-finalize blocks".into()));
        }

        rsp_rx
    }

    /// Returns `true` if `hash` is a valid previous block hash for new non-finalized blocks.
    fn can_fork_chain_at(&self, hash: &block::Hash) -> bool {
        self.non_finalized_block_write_sent_hashes
            .can_fork_chain_at(hash)
            || &self.read_service.db.finalized_tip_hash() == hash
    }

    /// Returns `true` if `queued_height` is near the final checkpoint.
    ///
    /// The semantic block verifier needs access to UTXOs from checkpoint verified blocks
    /// near the final checkpoint, so that it can verify blocks that spend those UTXOs.
    ///
    /// If it doesn't have the required UTXOs, some blocks will time out,
    /// but succeed after a syncer restart.
    fn is_close_to_final_checkpoint(&self, queued_height: block::Height) -> bool {
        queued_height >= self.full_verifier_utxo_lookahead
    }

    /// Sends all queued blocks whose parents have recently arrived starting from `new_parent`
    /// in breadth-first ordering to the block write task which will attempt to validate and commit them
    #[tracing::instrument(level = "debug", skip(self, new_parent))]
    fn send_ready_non_finalized_queued(&mut self, new_parent: block::Hash) {
        use tokio::sync::mpsc::error::SendError;
        if let Some(non_finalized_block_write_sender) = &self.block_write_sender.non_finalized {
            let mut new_parents: Vec<block::Hash> = vec![new_parent];

            while let Some(parent_hash) = new_parents.pop() {
                let queued_children = self
                    .non_finalized_state_queued_blocks
                    .dequeue_children(parent_hash);

                for queued_child in queued_children {
                    let (SemanticallyVerifiedBlock { hash, .. }, _) = queued_child;

                    // CROSSLINK: commit-time fat-pointer gate (mirrors the intake check, incl. the
                    // 32265 bypass). A block is committed only once its fat pointer is resolvable
                    // and does not regress relative to its parent's (see
                    // `call_from_state_to_crosslink_to_ask_about_fat_pointers`). If the parent's
                    // pointer cannot be resolved yet — the parent is not committed to state (e.g. a
                    // child flushed earlier this pass), or the referenced BFT block has not entered
                    // this node — the block is *deferred*, not rejected: it is re-queued and
                    // re-evaluated on a later flush, since it may become valid once those load.
                    let child_fat_pointer = queued_child.0.block.header.fat_pointer_to_bft_block.clone();
                    let child_parent_hash = queued_child.0.block.header.previous_block_hash;
                    let parent_header = self.read_service.non_finalized_state_receiver.with_watch_data(
                        |non_finalized_state| {
                            let mut ret = None;
                            for chain in non_finalized_state.chain_iter() {
                                if ret.is_none() {
                                    ret = chain.block(crate::HashOrHeight::Hash(child_parent_hash)).map(|b| b.block.header.clone());
                                }
                            }
                            ret
                        },
                    );
                    let parent_header = if parent_header.is_some() { parent_header } else { self.read_service.db.block_header(crate::HashOrHeight::Hash(child_parent_hash)) };
                    let parent_fat_pointer = parent_header.map(|h| h.fat_pointer_to_bft_block.clone());

                    let child_block_height = queued_child.0.height;
                    let crosslink_result = if child_block_height == zebra_chain::block::Height(32265) || child_block_height == zebra_chain::block::Height(32266) {
                        Some(true)
                    } else if parent_fat_pointer.is_none() {
                        None
                    } else {
                        (self.closure_to_call_crosslink)(parent_fat_pointer.unwrap(), child_fat_pointer, child_block_height)
                    };

                    match crosslink_result {
                        None => {
                            // Defer: re-queue without flushing or recursing; re-evaluated on a later flush.
                            self.non_finalized_state_queued_blocks.queue(queued_child);
                            continue;
                        }
                        Some(false) => {
                            // Hard reject: the block is permanently invalid (do_not_include_until_bc_height violated).
                            Self::send_semantically_verified_block_error(
                                queued_child,
                                CommitSemanticallyVerifiedError::from(
                                    ValidateContextError::CrosslinkFatPointerTooEarly {
                                        block_height: child_block_height,
                                        do_not_include_until: 0,
                                    },
                                ),
                            );
                            continue;
                        }
                        Some(true) => {}
                    }

                    // at a hardfork's PoW activation height, block any verification until the slash index reaches that height
                    if let Some(rule) = self.hardfork_schedule.rule_active_at(child_block_height.0 as u64) {
                        if rule.pow_activation_height == child_block_height.0 as u64 && self.read_service.db.slash_index_next_height().0 < child_block_height.0 {
                            self.non_finalized_state_queued_blocks.queue(queued_child);
                            continue;
                        }
                    }

                    self.non_finalized_block_write_sent_hashes
                        .add(&queued_child.0);
                    let send_result = non_finalized_block_write_sender.send(queued_child.into());

                    if let Err(SendError(NonFinalizedWriteMessage::Commit(queued))) = send_result {
                        // If Zebra is shutting down, drop blocks and return an error.
                        Self::send_semantically_verified_block_error(
                            queued,
                            CommitSemanticallyVerifiedError::from(
                                ValidateContextError::CommitTaskExited,
                            ),
                        );

                        self.clear_non_finalized_block_queue(
                            CommitSemanticallyVerifiedError::from(
                                ValidateContextError::CommitTaskExited,
                            ),
                        );

                        return;
                    };

                    new_parents.push(hash);
                }
            }

            self.non_finalized_block_write_sent_hashes.finish_batch();
        };
    }

    /// Return the tip of the current best chain.
    pub fn best_tip(&self) -> Option<(block::Height, block::Hash)> {
        self.read_service.best_tip()
    }

    fn send_invalidate_block(
        &self,
        hash: block::Hash,
    ) -> oneshot::Receiver<Result<block::Hash, BoxError>> {
        let (rsp_tx, rsp_rx) = oneshot::channel();

        let Some(sender) = &self.block_write_sender.non_finalized else {
            let _ = rsp_tx.send(Err(
                "cannot invalidate blocks while still committing checkpointed blocks".into(),
            ));
            return rsp_rx;
        };

        if let Err(tokio::sync::mpsc::error::SendError(error)) =
            sender.send(NonFinalizedWriteMessage::Invalidate { hash, rsp_tx })
        {
            let NonFinalizedWriteMessage::Invalidate { rsp_tx, .. } = error else {
                unreachable!("should return the same Invalidate message could not be sent");
            };

            let _ = rsp_tx.send(Err(
                "failed to send invalidate block request to block write task".into(),
            ));
        }

        rsp_rx
    }

    fn send_reconsider_block(
        &self,
        hash: block::Hash,
    ) -> oneshot::Receiver<Result<Vec<block::Hash>, BoxError>> {
        let (rsp_tx, rsp_rx) = oneshot::channel();

        let Some(sender) = &self.block_write_sender.non_finalized else {
            let _ = rsp_tx.send(Err(
                "cannot reconsider blocks while still committing checkpointed blocks".into(),
            ));
            return rsp_rx;
        };

        if let Err(tokio::sync::mpsc::error::SendError(error)) =
            sender.send(NonFinalizedWriteMessage::Reconsider { hash, rsp_tx })
        {
            let NonFinalizedWriteMessage::Reconsider { rsp_tx, .. } = error else {
                unreachable!("should return the same Reconsider message could not be sent");
            };

            let _ = rsp_tx.send(Err(
                "failed to send reconsider block request to block write task".into(),
            ));
        }

        rsp_rx
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
    pub fn db(&self) -> ZebraDb {
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
            // Uses non_finalized_state_queued_blocks and pending_utxos in the StateService
            // Accesses shared writeable state in the StateService, NonFinalizedState, and ZebraDb.
            Request::CommitSemanticallyVerifiedBlock(semantically_verified) => {
                self.assert_block_can_be_validated(&semantically_verified);

                self.pending_utxos
                    .check_against_ordered(&semantically_verified.new_outputs);

                // # Performance
                //
                // Allow other async tasks to make progress while blocks are being verified
                // and written to disk. But wait for the blocks to finish committing,
                // so that `StateService` multi-block queries always observe a consistent state.
                //
                // Since each block is spawned into its own task,
                // there shouldn't be any other code running in the same task,
                // so we don't need to worry about blocking it:
                // https://docs.rs/tokio/latest/tokio/task/fn.block_in_place.html

                let rsp_rx = tokio::task::block_in_place(move || {
                    span.in_scope(|| {
                        self.queue_and_commit_to_non_finalized_state(semantically_verified)
                    })
                });

                // TODO:
                //   - check for panics in the block write task here,
                //     as well as in poll_ready()

                // The work is all done, the future just waits on a channel for the result
                timer.finish(module_path!(), line!(), "CommitSemanticallyVerifiedBlock");

                // Await the channel response, mapping any receive error into a BoxError.
                // Then flatten the nested Result by converting the inner CommitSemanticallyVerifiedError into a BoxError.
                let span = Span::current();
                async move {
                    rsp_rx
                        .await
                        .map_err(|_recv_error| {
                            BoxError::from(CommitSemanticallyVerifiedError::from(
                                ValidateContextError::NotReadyToBeCommitted,
                            ))
                        })
                        // TODO: replace with Result::flatten once it stabilises
                        // https://github.com/rust-lang/rust/issues/70142
                        .and_then(|res| res.map_err(BoxError::from))
                        .map(Response::Committed)
                }
                .instrument(span)
                .boxed()
            }

            // Uses finalized_state_queued_blocks and pending_utxos in the StateService.
            // Accesses shared writeable state in the StateService.
            Request::CommitCheckpointVerifiedBlock(finalized) => {
                // # Consensus
                //
                // A semantic block verification could have called AwaitUtxo
                // before this checkpoint verified block arrived in the state.
                // So we need to check for pending UTXO requests sent by running
                // semantic block verifications.
                //
                // This check is redundant for most checkpoint verified blocks,
                // because semantic verification can only succeed near the final
                // checkpoint, when all the UTXOs are available for the verifying block.
                //
                // (Checkpoint block UTXOs are verified using block hash checkpoints
                // and transaction merkle tree block header commitments.)
                self.pending_utxos
                    .check_against_ordered(&finalized.new_outputs);

                // # Performance
                //
                // This method doesn't block, access the database, or perform CPU-intensive tasks,
                // so we can run it directly in the tokio executor's Future threads.
                let rsp_rx = self.queue_and_commit_to_finalized_state(finalized);

                // TODO:
                //   - check for panics in the block write task here,
                //     as well as in poll_ready()

                // The work is all done, the future just waits on a channel for the result
                timer.finish(module_path!(), line!(), "CommitCheckpointVerifiedBlock");

                async move {
                    rsp_rx
                        .await
                        .map_err(|_recv_error| {
                            BoxError::from("block was dropped from the queue of finalized blocks")
                        })
                        // TODO: replace with Result::flatten once it stabilises
                        // https://github.com/rust-lang/rust/issues/70142
                        .and_then(convert::identity)
                        .map(Response::Committed)
                }
                .instrument(span)
                .boxed()
            }

            Request::CrosslinkFinalizeBlock(finalized) => {
                info!("Trying to Crosslink-finalize {}", finalized);
                // # Performance
                //
                // This method doesn't block, access the database, or perform CPU-intensive tasks,
                // so we can run it directly in the tokio executor's Future threads.
                let rsp_rx = self.send_crosslink_finalized_to_non_finalized_state(finalized);

                // CROSSLINK: a BFT decision just landed (the new block is already in
                // `bft_blocks` by the time this request arrives), which may make the fat
                // pointers of previously-deferred non-finalized blocks resolvable. Re-flush
                // the queue so those are re-evaluated now rather than waiting for the next
                // incoming block — which may never come on an idle chain.
                //
                // `block_in_place` is REQUIRED: this arm runs synchronously on a tokio runtime
                // thread, and the fat-pointer gate reached from `send_ready_non_finalized_queued`
                // acquires the crosslink lock with `blocking_lock()`, which panics on a runtime
                // thread unless we are inside `block_in_place`. (The intake path at
                // `CommitSemanticallyVerifiedBlock` is wrapped for the same reason.)
                tokio::task::block_in_place(|| {
                    if let Some((_, tip_hash)) = self.best_tip() {
                        self.send_ready_non_finalized_queued(tip_hash);
                    }
                });

                // TODO:
                //   - check for panics in the block write task here,
                //     as well as in poll_ready()

                // The work is all done, the future just waits on a channel for the result
                timer.finish(module_path!(), line!(), "CrosslinkFinalizeBlock");

                async move {
                    rsp_rx
                        .await
                        .map_err(|_recv_error| {
                            BoxError::from("block was dropped from the queue of finalized blocks")
                        })
                        // TODO: replace with Result::flatten once it stabilises
                        // https://github.com/rust-lang/rust/issues/70142
                        .and_then(convert::identity)
                        .map(|(hash, aggregated_stakes)| Response::CrosslinkFinalized(hash, aggregated_stakes))
                }
                .instrument(span)
                .boxed()
            }

            // Uses pending_utxos and non_finalized_state_queued_blocks in the StateService.
            // If the UTXO isn't in the queued blocks, runs concurrently using the ReadStateService.
            Request::AwaitUtxo(outpoint) => {
                // Prepare the AwaitUtxo future from PendingUxtos.
                let response_fut = self.pending_utxos.queue(outpoint);
                // Only instrument `response_fut`, the ReadStateService already
                // instruments its requests with the same span.

                let response_fut = response_fut.instrument(span).boxed();

                // Check the non-finalized block queue outside the returned future,
                // so we can access mutable state fields.
                if let Some(utxo) = self.non_finalized_state_queued_blocks.utxo(&outpoint) {
                    self.pending_utxos.respond(&outpoint, utxo);

                    // We're finished, the returned future gets the UTXO from the respond() channel.
                    timer.finish(module_path!(), line!(), "AwaitUtxo/queued-non-finalized");

                    return response_fut;
                }

                // Check the sent non-finalized blocks
                if let Some(utxo) = self.non_finalized_block_write_sent_hashes.utxo(&outpoint) {
                    self.pending_utxos.respond(&outpoint, utxo);

                    // We're finished, the returned future gets the UTXO from the respond() channel.
                    timer.finish(module_path!(), line!(), "AwaitUtxo/sent-non-finalized");

                    return response_fut;
                }

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

            Request::InvalidateBlock(block_hash) => {
                let rsp_rx = tokio::task::block_in_place(move || {
                    span.in_scope(|| self.send_invalidate_block(block_hash))
                });

                let span = Span::current();
                async move {
                    rsp_rx
                        .await
                        .map_err(|_recv_error| {
                            BoxError::from("invalidate block request was unexpectedly dropped")
                        })
                        // TODO: replace with Result::flatten once it stabilises
                        // https://github.com/rust-lang/rust/issues/70142
                        .and_then(convert::identity)
                        .map(Response::Invalidated)
                }
                .instrument(span)
                .boxed()
            }

            Request::ReconsiderBlock(block_hash) => {
                let rsp_rx = tokio::task::block_in_place(move || {
                    span.in_scope(|| self.send_reconsider_block(block_hash))
                });

                let span = Span::current();
                async move {
                    rsp_rx
                        .await
                        .map_err(|_recv_error| {
                            BoxError::from("reconsider block request was unexpectedly dropped")
                        })
                        // TODO: replace with Result::flatten once it stabilises
                        // https://github.com/rust-lang/rust/issues/70142
                        .and_then(convert::identity)
                        .map(Response::Reconsidered)
                }
                .instrument(span)
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
) {
    let (state_service, read_only_state_service, latest_chain_tip, chain_tip_change) =
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
    let (state_service, _, _, _) =
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
    let (state_service, read_state_service, latest_chain_tip, chain_tip_change) =
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
