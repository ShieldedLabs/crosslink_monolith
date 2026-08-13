//! Writing blocks to the finalized and non-finalized states.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use indexmap::IndexMap;
use tokio::sync::{
    mpsc::{UnboundedReceiver, UnboundedSender},
    oneshot, watch,
};

use tracing::Span;
use zebra_chain::{
    block::{self, Height},
    parameters::Network,
};

use crate::{
    constants::MAX_BLOCK_REORG_HEIGHT,
    service::{
        check,
        finalized_state::{FinalizedState, ZebraDb},
        non_finalized_state::NonFinalizedState,
        queued_blocks::{QueuedCheckpointVerified, QueuedSemanticallyVerified},
        ChainTipBlock, ChainTipSender,
    },
    BoxError, CommitSemanticallyVerifiedError, SemanticallyVerifiedBlock, ValidateContextError,
};
use zebra_chain::parallel::tree::NoteCommitmentTrees;

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
) -> Result<(), ValidateContextError> {
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
///
/// If `backup_dir_path` is `Some`, the non-finalized state is written to the backup
/// directory before updating the channels.
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
        backup_dir_path,
    ),
    fields(chains = non_finalized_state.chain_count())
)]
fn update_latest_chain_channels(
    non_finalized_state: &NonFinalizedState,
    chain_tip_sender: &mut ChainTipSender,
    non_finalized_state_sender: &watch::Sender<NonFinalizedState>,
    backup_dir_path: Option<&Path>,
) -> block::Height {
    let best_chain = non_finalized_state.best_chain().expect("unexpected empty non-finalized state: must commit at least one block before updating channels");

    let tip_block = best_chain
        .tip_block()
        .expect("unexpected empty chain: must commit at least one block before updating channels")
        .clone();
    let tip_block = ChainTipBlock::from(tip_block);

    let tip_block_height = tip_block.height;

    if let Some(backup_dir_path) = backup_dir_path {
        non_finalized_state.write_to_backup(backup_dir_path);
    }

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
    prev_finalized_note_commitment_trees: Option<NoteCommitmentTrees>,
    /// Errors propagated down to queued child blocks: if a parent was rejected, every
    /// descendant is rejected with the same error.
    parent_error_map: IndexMap<block::Hash, ValidateContextError>,
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
            prev_finalized_note_commitment_trees: None,
            parent_error_map: IndexMap::new(),
        }
    }

    /// Commit a checkpoint-verified block straight to the finalized state.
    ///
    /// Used by `zebrad copy-state` and tests, which have an already-validated chain and so
    /// need the write path without any verification in front of it.
    pub fn commit_checkpoint_verified(
        &mut self,
        checkpoint_verified: crate::CheckpointVerifiedBlock,
    ) -> Result<block::Hash, BoxError> {
        let tip_block = ChainTipBlock::from(checkpoint_verified.clone());

        let (hash, trees) = self.finalized_state.commit_finalized_direct(
            checkpoint_verified.into(),
            self.prev_finalized_note_commitment_trees.take(),
            "commit checkpoint-verified block",
        )?;
        self.prev_finalized_note_commitment_trees = Some(trees);

        // @Volatile: publishing the tip is not optional. Everything downstream of
        // `latest_chain_tip` -- the sync progress task, the mempool, lightwallet_server -- reads this watch
        // channel, not the database. Committing without publishing leaves them seeing an empty
        // chain forever. The old finalized write loop did this immediately after committing.
        self.chain_tip_sender.set_finalized_tip(tip_block);

        Ok(hash)
    }

    /// Commit the genesis block directly to the finalized state.
    ///
    /// Genesis is the one block with no parent and no possibility of reorg, so it goes to the
    /// finalized state rather than entering a non-finalized chain. Its hash is checked against
    /// the configured network genesis by the caller, which is the whole of what the checkpoint
    /// verifier contributed at height 0.
    pub fn commit_genesis(
        &mut self,
        genesis: std::sync::Arc<zebra_chain::block::Block>,
    ) -> Result<block::Hash, BoxError> {
        self.commit_checkpoint_verified(crate::CheckpointVerifiedBlock::from(genesis))
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
                None,
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
    ) -> Result<(), ValidateContextError> {
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
            None,
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


