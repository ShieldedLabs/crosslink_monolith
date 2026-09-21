//! `fin`, the node-local finalized marker, and the rule that moves it.
//!
//! `fin` is not a function of the chain: it is memory of what this node has already called
//! final, and the fork-choice floor that memory imposes (FINALITY.md §3.2, §4.3). It advances
//! only where the best chain changes, only to `candidate(bc_best)`, and never backwards.
//!
//! The in-memory copy here is a cache of the database row, so readers do not touch rocksdb; the
//! row is what survives a restart. Both are written by [`advance`], which runs on the sync
//! thread that owns the block writer, so there is exactly one writer.

use std::sync::RwLock;

use zcash_primitives::bft::FatPointerToBftBlock;
use zebra_chain::block::{Hash, Height};

use crate::service::finalized_state::ZebraDb;
use crate::service::non_finalized_state::NonFinalizedState;

static FIN: RwLock<Option<(Height, Hash)>> = RwLock::new(None);

static FIN_CHANGE: std::sync::LazyLock<tokio::sync::broadcast::Sender<(Height, Hash)>> =
    std::sync::LazyLock::new(|| tokio::sync::broadcast::channel(16).0);

/// The block this node has finalized, or `None` before it has finalized any.
pub fn fin() -> Option<(Height, Hash)> {
    *FIN.read().unwrap()
}

/// A subscription to every advance of `fin`.
pub fn fin_change_rx() -> tokio::sync::broadcast::Receiver<(Height, Hash)> {
    FIN_CHANGE.subscribe()
}

/// Load the stored `fin` into the cache at startup.
pub(crate) fn load(db: &ZebraDb) {
    if let Some((height, hash)) = db.crosslink_fin() {
        *FIN.write().unwrap() = Some((height, hash));
        tracing::info!("crosslink fin: restored at height {} ({})", height.0, hash);
    }
}

/// Record that `fin` has moved to `hash` at `height`.
///
/// The caller owns the §3.2 test: it has already established that the new marker is a descendant
/// of the old one and that the block is committed. A regression is a programmer error rather
/// than a recoverable condition, so it aborts instead of being silently ignored -- a `fin` that
/// moved back would let the node abandon a block it had already called final.
pub(crate) fn advance(db: &ZebraDb, height: Height, hash: Hash) {
    let previous = *FIN.read().unwrap();
    if let Some((previous_height, _)) = previous {
        assert!(
            height >= previous_height,
            "fin regressed from {} to {}",
            previous_height.0,
            height.0,
        );
    }

    if let Err(err) = db.write_crosslink_fin(height, hash) {
        tracing::error!("could not store fin at height {}: {err}", height.0);
        return;
    }

    *FIN.write().unwrap() = Some((height, hash));
    let _ = FIN_CHANGE.send((height, hash));
}

/// `candidate(H) = lca(snapshot(LF(H)), prune_σ(H))` for the best chain `H` (FINALITY.md §3.1).
///
/// Both arguments are ancestors of `H` -- Last Final Snapshot puts the snapshot on `H`'s own
/// chain, and `prune_σ(H)` is `H` walked back -- so their lca is whichever of the two is lower,
/// and no ancestry walk is needed. The clamp is what keeps a subverted `Π_bft` from finalizing a
/// block this node has buried only a block or two deep (FINALITY.md §3.2).
///
/// `None` while the tip carries no bft pointer, while the bft-block it names has not reached
/// this node, or while the snapshot it finalizes is not a block this node holds.
pub fn candidate(
    chain: &crate::new_network::bft::BftChain,
    non_finalized_state: &NonFinalizedState,
    db: &ZebraDb,
    sigma: u64,
) -> Option<(Height, Hash)> {
    let best_chain = non_finalized_state.best_chain()?;
    let tip = best_chain.tip_block()?;
    let tip_height = tip.height;

    let fat_pointer = &tip.block.header.fat_pointer_to_bft_block;
    if *fat_pointer == FatPointerToBftBlock::null() {
        return None;
    }
    let bft_height = *chain.hash_to_height.get(&fat_pointer.points_at_block_hash())?;
    let bft_block = chain.blocks.get(bft_height as usize)?;
    if bft_block.headers.is_empty() {
        return None;
    }
    let snapshot_hash = Hash(bft_block.snapshot_block_hash().0);

    let snapshot_height = best_chain
        .height_by_hash(snapshot_hash)
        .or_else(|| db.height(snapshot_hash))?;
    let sigma_height = Height(tip_height.0.saturating_sub(sigma as u32));

    if snapshot_height <= sigma_height {
        Some((snapshot_height, snapshot_hash))
    } else {
        let hash = best_chain.hash_by_height(sigma_height).or_else(|| db.hash(sigma_height))?;
        Some((sigma_height, hash))
    }
}
