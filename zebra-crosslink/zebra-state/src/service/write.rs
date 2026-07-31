//! Writing blocks to the finalized and non-finalized states.

use std::sync::Arc;

use indexmap::IndexMap;
use tokio::sync::{
    mpsc::{UnboundedReceiver, UnboundedSender},
    oneshot, watch,
};

use tracing::Span;
use zebra_chain::{
    block::{self, Height},
    parallel::tree::NoteCommitmentTrees,
    transparent::EXTRA_ZEBRA_COINBASE_DATA,
};

use crate::{
    constants::MAX_BLOCK_REORG_HEIGHT,
    service::{
        check,
        finalized_state::{FinalizedState, ZebraDb},
        non_finalized_state::NonFinalizedState,
        queued_blocks::{QueuedCheckpointVerified, QueuedSemanticallyVerified},
        BoxError, ChainTipBlock, ChainTipSender,
    },
    CommitSemanticallyVerifiedError, SemanticallyVerifiedBlock,
};

// These types are used in doc links
#[allow(unused_imports)]
use crate::service::{
    chain_tip::{ChainTipChange, LatestChainTip},
    non_finalized_state::Chain,
};

/// The maximum size of the parent error map.
///
/// We allow enough space for multiple concurrent chain forks with errors.
const PARENT_ERROR_MAP_LIMIT: usize = MAX_BLOCK_REORG_HEIGHT as usize * 2;

/// Run contextual validation on the prepared block and add it to the
/// non-finalized state if it is contextually valid.
#[tracing::instrument(
    level = "debug",
    skip(finalized_state, non_finalized_state, prepared),
    fields(
        height = ?prepared.height,
        hash = %prepared.hash,
        chains = non_finalized_state.chain_count()
    )
)]
pub(crate) fn validate_and_commit_non_finalized(
    finalized_state: &ZebraDb,
    non_finalized_state: &mut NonFinalizedState,
    prepared: SemanticallyVerifiedBlock,
) -> Result<(), CommitSemanticallyVerifiedError> {
    check::initial_contextual_validity(finalized_state, non_finalized_state, &prepared)?;
    let parent_hash = prepared.block.header.previous_block_hash;

    if finalized_state.finalized_tip_hash() == parent_hash {
        non_finalized_state.commit_new_chain(prepared, finalized_state)?;
    } else {
        non_finalized_state.commit_block(prepared, finalized_state)?;
    }

    Ok(())
}

/// Update the [`LatestChainTip`], [`ChainTipChange`], and `non_finalized_state_sender`
/// channels with the latest non-finalized [`ChainTipBlock`] and
/// [`Chain`].
///
/// `last_zebra_mined_log_height` is used to rate-limit logging.
///
/// Returns the latest non-finalized chain tip height.
///
/// # Panics
///
/// If the `non_finalized_state` is empty.
#[instrument(
    level = "debug",
    skip(
        non_finalized_state,
        chain_tip_sender,
        non_finalized_state_sender,
        last_zebra_mined_log_height
    ),
    fields(chains = non_finalized_state.chain_count())
)]
fn update_latest_chain_channels(
    non_finalized_state: &NonFinalizedState,
    chain_tip_sender: &mut ChainTipSender,
    non_finalized_state_sender: &watch::Sender<NonFinalizedState>,
    last_zebra_mined_log_height: &mut Option<Height>,
) -> block::Height {
    let best_chain = non_finalized_state.best_chain().expect("unexpected empty non-finalized state: must commit at least one block before updating channels");

    let tip_block = best_chain
        .tip_block()
        .expect("unexpected empty chain: must commit at least one block before updating channels")
        .clone();
    let tip_block = ChainTipBlock::from(tip_block);

    log_if_mined_by_zebra(&tip_block, last_zebra_mined_log_height);

    let tip_block_height = tip_block.height;

    // If the final receiver was just dropped, ignore the error.
    let _ = non_finalized_state_sender.send(non_finalized_state.clone());

    chain_tip_sender.set_best_non_finalized_tip(tip_block);

    tip_block_height
}

/// A worker task that reads, validates, and writes blocks to the
/// `finalized_state` or `non_finalized_state`.
///
/// `pub(crate)` so `new_network` can drive it directly rather than through the message
/// channel. The two `handle_*` methods below are the whole of what the run loop does per
/// message, so calling them is equivalent to sending the corresponding message.
pub struct WriteBlockWorkerTask {
    pub(crate) finalized_state: FinalizedState,
    pub(crate) non_finalized_state: NonFinalizedState,
    chain_tip_sender: ChainTipSender,
    non_finalized_state_sender: watch::Sender<NonFinalizedState>,

    // Carried across messages. These were locals inside `run()`; they became fields so the
    // per-message work could be extracted into methods without changing what it does.
    last_zebra_mined_log_height: Option<Height>,
    prev_finalized_note_commitment_trees: Option<NoteCommitmentTrees>,
    /// Errors propagated down to queued child blocks: if a parent was rejected, every
    /// descendant is rejected with the same error.
    parent_error_map: IndexMap<block::Hash, CommitSemanticallyVerifiedError>,
}

impl WriteBlockWorkerTask {
    /// Build the block writer.
    ///
    /// No thread and no channels: the caller (new_network) owns this and calls the handlers
    /// directly. That makes every mutation of the chain state happen on one thread, in a known
    /// order, with the result available synchronously.
    #[instrument(
        level = "debug",
        skip_all,
        fields(
            network = %non_finalized_state.network
        )
    )]
    pub fn new(
        finalized_state: FinalizedState,
        non_finalized_state: NonFinalizedState,
        chain_tip_sender: ChainTipSender,
        non_finalized_state_sender: watch::Sender<NonFinalizedState>,
    ) -> WriteBlockWorkerTask {
        WriteBlockWorkerTask {
            finalized_state,
            non_finalized_state,
            chain_tip_sender,
            non_finalized_state_sender,
            last_zebra_mined_log_height: None,
            prev_finalized_note_commitment_trees: None,
            parent_error_map: IndexMap::new(),
        }
    }

    /// Reads blocks from the channels, writes them to the `finalized_state` or `non_finalized_state`,
    /// sends any errors on the `invalid_block_reset_sender`, then updates the `chain_tip_sender` and
    /// `non_finalized_state_sender`.
    #[instrument(
        level = "debug",
        skip(self),
        fields(
            network = %self.non_finalized_state.network
        )
    )]
    /// Commit the genesis block directly to the finalized state.
    ///
    /// Genesis is the one block with no parent and no possibility of reorg, so it goes to the
    /// finalized state rather than entering a non-finalized chain. Its hash is checked against
    /// the configured network genesis by the caller, which is the whole of what the checkpoint
    /// verifier contributed at height 0.
    /// Commit a checkpoint-verified block straight to the finalized state.
    ///
    /// Used by `zebrad copy-state`, which bulk-copies an already-validated chain between
    /// databases and so needs the write path without any verification in front of it.
    pub fn commit_checkpoint_verified(
        &mut self,
        checkpoint_verified: crate::CheckpointVerifiedBlock,
    ) -> Result<block::Hash, BoxError> {
        let (hash, trees) = self.finalized_state.commit_finalized_direct(
            checkpoint_verified.into(),
            self.prev_finalized_note_commitment_trees.take(),
            "copy-state bulk write",
        )?;
        self.prev_finalized_note_commitment_trees = Some(trees);
        Ok(hash)
    }

    pub fn commit_genesis(
        &mut self,
        genesis: std::sync::Arc<zebra_chain::block::Block>,
    ) -> Result<block::Hash, BoxError> {
        let checkpoint_verified = crate::CheckpointVerifiedBlock::from(genesis);
        let tip_block = ChainTipBlock::from(checkpoint_verified.clone());

        let (hash, trees) = self.finalized_state.commit_finalized_direct(
            checkpoint_verified.into(),
            self.prev_finalized_note_commitment_trees.take(),
            "commit genesis block",
        )?;
        self.prev_finalized_note_commitment_trees = Some(trees);

        // @Volatile: publishing the tip is not optional. Everything downstream of
        // `latest_chain_tip` -- the sync progress task, the mempool, Zaino -- reads this watch
        // channel, not the database. Committing without publishing leaves them seeing an empty
        // chain forever. The old finalized write loop did this immediately after committing.
        log_if_mined_by_zebra(&tip_block, &mut self.last_zebra_mined_log_height);
        self.chain_tip_sender.set_finalized_tip(tip_block);

        Ok(hash)
    }

    /// Crosslink-finalize `hash` and everything it implicitly finalizes.
    ///
    /// Extracted verbatim from the run loop's `CrosslinkFinalized` arm.
    pub fn handle_crosslink_finalize(
        &mut self,
        hash: block::Hash,
    ) -> Result<(block::Hash, Vec<([u8; 32], u64)>), BoxError> {
        if let Some(newly_finalized_blocks) = self.non_finalized_state.crosslink_finalize(hash) {
            update_latest_chain_channels(
                &self.non_finalized_state,
                &mut self.chain_tip_sender,
                &self.non_finalized_state_sender,
                &mut self.last_zebra_mined_log_height,
            );

            info!("finalized {}, which implicitly finalizes:", hash);
            for i in 0..newly_finalized_blocks.len() {
                let finalizable_block = self.non_finalized_state.finalize();

                match self.finalized_state.commit_finalized_direct(
                    finalizable_block,
                    None,
                    "commit Crosslink-finalized block",
                ) {
                    Ok((hash, _)) => {
                        info!("  {}: {}", i, hash);
                    }
                    Err(err) => {
                        unreachable!("unexpected finalized block commit error: {}", err)
                    }
                }
            }

            // The finalized tip changed, so this is stale and needs invalidation, or else it will write duplicate trees.
            self.prev_finalized_note_commitment_trees = None;

            let aggregated_stakes = self.finalized_state.db.aggregated_stakes(&hash)
                .unwrap_or_default();

            Ok((hash, aggregated_stakes))
        } else if self.finalized_state.db.contains_hash(hash) {
            warn!("Crosslink finalization: already de-facto finalized as below reorg height");
            let stakes = self.finalized_state.db.aggregated_stakes(&hash)
                .unwrap_or_default();
            Ok((hash, stakes))
        } else {
            Err("Couldn't find finalized block".into())
        }
    }

    /// Contextually validate and commit one semantically verified block, then finalize
    /// anything that fell past the reorg limit.
    ///
    /// Extracted verbatim from the run loop's `Commit` arm.
    pub fn handle_commit(
        &mut self,
        queued_child: SemanticallyVerifiedBlock,
    ) -> Result<(), CommitSemanticallyVerifiedError> {
        let child_hash = queued_child.hash;
        let parent_hash = queued_child.block.header.previous_block_hash;

        let queued_block_height = queued_child.block.coinbase_height().expect("committed block should have a coinbase height").0;

        // If the parent block was marked as rejected, also reject all its children.
        //
        // At this point, we know that all the block's descendants are invalid, because we
        // checked all the consensus rules before committing the failing ancestor block to
        // the non-finalized state.
        let result = if let Some(parent_error) = self.parent_error_map.get(&parent_hash) {
            Err(parent_error.clone())
        } else {
            tracing::trace!(?child_hash, "validating queued child");
            validate_and_commit_non_finalized(
                &self.finalized_state.db,
                &mut self.non_finalized_state,
                queued_child,
            )
        };

        if let Err(ref error) = result {
            // If the block is invalid, mark any descendant blocks as rejected.
            self.parent_error_map.insert(child_hash, error.clone());

            // Make sure the error map doesn't get too big.
            if self.parent_error_map.len() > PARENT_ERROR_MAP_LIMIT {
                // We only add one hash at a time, so we only need to remove one extra here.
                self.parent_error_map.shift_remove_index(0);
            }

            return result;
        }

        // Committing blocks to the finalized state keeps the same chain, so we can update the
        // chain seen by the rest of the application now.
        let tip_block_height = update_latest_chain_channels(
            &self.non_finalized_state,
            &mut self.chain_tip_sender,
            &self.non_finalized_state_sender,
            &mut self.last_zebra_mined_log_height,
        );

        while self
            .non_finalized_state
            .best_chain_len()
            .expect("just successfully inserted a non-finalized block above")
            > MAX_BLOCK_REORG_HEIGHT
        {
            tracing::trace!("finalizing block past the reorg limit");
            let contextually_verified_with_trees = self.non_finalized_state.finalize();

            self.prev_finalized_note_commitment_trees = self.finalized_state
                        .commit_finalized_direct(contextually_verified_with_trees, self.prev_finalized_note_commitment_trees.take(), "commit contextually-verified request")
                        .expect(
                            "unexpected finalized block commit error: note commitment and history trees were already checked by the non-finalized state",
                        ).1.into();
        }

        // Update the metrics if semantic and contextual validation passes
        metrics::counter!("state.full_verifier.committed.block.count").increment(1);
        metrics::counter!("zcash.chain.verified.block.total").increment(1);

        metrics::gauge!("state.full_verifier.committed.block.height")
            .set(tip_block_height.0 as f64);

        // This height gauge is updated for both fully verified and checkpoint blocks.
        metrics::gauge!("zcash.chain.verified.block.height").set(tip_block_height.0 as f64);

        tracing::trace!("finished processing queued block");

        result
    }
}


/// Log a message if this block was mined by Zebra.
///
/// Does not detect early Zebra blocks, and blocks with custom coinbase transactions.
/// Rate-limited to every 1000 blocks using `last_zebra_mined_log_height`.
fn log_if_mined_by_zebra(
    tip_block: &ChainTipBlock,
    last_zebra_mined_log_height: &mut Option<Height>,
) {
    // This logs at most every 2-3 checkpoints, which seems fine.
    const LOG_RATE_LIMIT: u32 = 1000;

    let height = tip_block.height.0;

    if let Some(last_height) = last_zebra_mined_log_height {
        if height < last_height.0 + LOG_RATE_LIMIT {
            // If we logged in the last 1000 blocks, don't log anything now.
            return;
        }
    };

    // This code is rate-limited, so we can do expensive transformations here.
    let coinbase_data = tip_block.transactions[0].inputs()[0]
        .extra_coinbase_data()
        .expect("valid blocks must start with a coinbase input")
        .clone();

    if coinbase_data
        .as_ref()
        .starts_with(EXTRA_ZEBRA_COINBASE_DATA.as_bytes())
    {
        let text = String::from_utf8_lossy(coinbase_data.as_ref());

        *last_zebra_mined_log_height = Some(Height(height));

        // No need for hex-encoded data if it's exactly what we expected.
        if coinbase_data.as_ref() == EXTRA_ZEBRA_COINBASE_DATA.as_bytes() {
            info!(
                %text,
                %height,
                hash = %tip_block.hash,
                "looks like this block was mined by Zebra!"
            );
        } else {
            // # Security
            //
            // Use the extra data as an allow-list, replacing unknown characters.
            // This makes sure control characters and harmful messages don't get logged
            // to the terminal.
            let text = text.replace(
                |c: char| {
                    !EXTRA_ZEBRA_COINBASE_DATA
                        .to_ascii_lowercase()
                        .contains(c.to_ascii_lowercase())
                },
                "?",
            );
            let data = hex::encode(coinbase_data.as_ref());

            info!(
                %text,
                %data,
                %height,
                hash = %tip_block.hash,
                "looks like this block was mined by Zebra!"
            );
        }
    }
}
