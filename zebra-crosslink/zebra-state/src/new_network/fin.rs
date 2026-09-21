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

/// Published beside the chain-tip channels, and read through the state service rather than
/// imported (FINALITY.md §7.2). A watch rather than a broadcast: `fin` is monotone, so a
/// subscriber that fell behind wants the latest value and nothing else.
static FIN_CHANGE: std::sync::LazyLock<tokio::sync::watch::Sender<Option<(Height, Hash)>>> =
    std::sync::LazyLock::new(|| tokio::sync::watch::channel(None).0);

/// The block this node has finalized, or `None` before it has finalized any.
pub fn fin() -> Option<(Height, Hash)> {
    *FIN.read().unwrap()
}

/// A subscription to every advance of `fin`.
pub fn fin_change_rx() -> tokio::sync::watch::Receiver<Option<(Height, Hash)>> {
    FIN_CHANGE.subscribe()
}

/// Load the stored `fin` into the cache at startup.
pub(crate) fn load(db: &ZebraDb) {
    if let Some((height, hash)) = db.crosslink_fin() {
        *FIN.write().unwrap() = Some((height, hash));
        FIN_CHANGE.send_replace(Some((height, hash)));
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
    FIN_CHANGE.send_replace(Some((height, hash)));
}

/// The finality status of the bc-block `hash` (FINALITY.md §7.2).
///
/// `fin` and the best chain are read together here, so the answer cannot mix two moments. The
/// three states are the table's: at or below `fin` on the best chain, on the best chain above
/// it, or not on the best chain at all -- which is also the answer for a block this node has
/// never seen.
///
/// @Todo: §7.2 carries a confirmation count with the middle state. The count is not carried
/// here because it would change `TFLBlockFinality`, and with it the test format stage 3 set.
pub fn block_finality(
    non_finalized_state: &NonFinalizedState,
    db: &ZebraDb,
    hash: Hash,
) -> crate::crosslink::TFLBlockFinality {
    use crate::crosslink::TFLBlockFinality::*;

    let height = non_finalized_state
        .best_chain()
        .and_then(|chain| chain.height_by_hash(hash))
        .or_else(|| db.height(hash));

    let Some(height) = height else {
        return CantBeFinalized;
    };

    finality_at_height(height)
}

/// The finality status of a block on the best chain at `height`.
///
/// `fin` lies on the best chain and so does the block, so the height decides ancestry.
pub fn finality_at_height(height: Height) -> crate::crosslink::TFLBlockFinality {
    use crate::crosslink::TFLBlockFinality::*;

    match fin() {
        Some((fin_height, _)) if height <= fin_height => Finalized,
        _ => NotYetFinalized,
    }
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
