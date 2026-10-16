//! Decided BFT chain database access and write methods.

use zcash_primitives::bft::{BftBlock, FatPointerToBftBlock, TMSig};

use zebra_chain::block::{self, Height};

use crate::service::finalized_state::{
    disk_db::DiskWriteBatch,
    disk_format::bft::{BftHeight, ProposalSignatures, StoredBftBlock, StoredFatPointer},
    disk_format::fin::{FinKey, FinMarker},
    zebra_db::ZebraDb,
    TypedColumnFamily,
};

/// The name of the decided BFT block by BFT height column family.
pub const BFT_BLOCK_BY_HEIGHT: &str = "bft_block_by_height";

/// The name of the BFT fat pointer by BFT height column family.
pub const BFT_FAT_POINTER_BY_HEIGHT: &str = "bft_fat_pointer_by_height";

/// The name of the BFT proposal signatures by BFT height column family.
pub const BFT_PROPOSAL_SIGS_BY_HEIGHT: &str = "bft_proposal_sigs_by_height";

/// The name of the column family recording that this database dropped BFT.
pub const CROSSLINK_BFT_DROPPED: &str = "crosslink_bft_dropped";

/// The type for reading decided BFT blocks from the database.
pub type BftBlockByHeightCf<'cf> = TypedColumnFamily<'cf, BftHeight, StoredBftBlock>;

/// The type for reading BFT fat pointers from the database.
pub type BftFatPointerByHeightCf<'cf> = TypedColumnFamily<'cf, BftHeight, StoredFatPointer>;

/// The type for reading BFT proposal signatures from the database.
pub type BftProposalSigsByHeightCf<'cf> = TypedColumnFamily<'cf, BftHeight, ProposalSignatures>;

/// The type for reading the dropped-BFT record. It is one block, as for `fin`, so it reuses that
/// row's key and value types.
pub type CrosslinkBftDroppedCf<'cf> = TypedColumnFamily<'cf, FinKey, FinMarker>;

/// One decided BFT height as the database holds it.
pub struct StoredDecision {
    pub block: BftBlock,
    pub fat_pointer: FatPointerToBftBlock,
    pub proposal_sigs: Vec<TMSig>,
}

impl ZebraDb {
    fn bft_block_by_height_cf(&self) -> BftBlockByHeightCf<'_> {
        BftBlockByHeightCf::new(&self.db, BFT_BLOCK_BY_HEIGHT)
            .expect("column family was created when database was created")
    }

    fn bft_fat_pointer_by_height_cf(&self) -> BftFatPointerByHeightCf<'_> {
        BftFatPointerByHeightCf::new(&self.db, BFT_FAT_POINTER_BY_HEIGHT)
            .expect("column family was created when database was created")
    }

    fn bft_proposal_sigs_by_height_cf(&self) -> BftProposalSigsByHeightCf<'_> {
        BftProposalSigsByHeightCf::new(&self.db, BFT_PROPOSAL_SIGS_BY_HEIGHT)
            .expect("column family was created when database was created")
    }

    fn crosslink_bft_dropped_cf(&self) -> CrosslinkBftDroppedCf<'_> {
        CrosslinkBftDroppedCf::new(&self.db, CROSSLINK_BFT_DROPPED)
            .expect("column family was created when database was created")
    }

    /// The decided BFT chain, ascending from height 0. Stops at the first gap: a decision is
    /// written after its snapshot commits, so a crash in between leaves a short chain, which
    /// resumes by re-deciding that height rather than by loading past the hole.
    pub fn bft_chain(&self) -> Vec<StoredDecision> {
        let blocks = self.bft_block_by_height_cf().zs_items_in_range_ordered(..);
        let fat_pointers = self.bft_fat_pointer_by_height_cf().zs_items_in_range_ordered(..);
        let sigs = self.bft_proposal_sigs_by_height_cf().zs_items_in_range_ordered(..);

        let mut chain = Vec::with_capacity(blocks.len());
        for (i, (height, block)) in blocks.into_iter().enumerate() {
            if height.0 as usize != i {
                break;
            }
            let Some(fat_pointer) = fat_pointers.get(&height) else { break; };
            chain.push(StoredDecision {
                block: block.0,
                fat_pointer: fat_pointer.0.clone(),
                proposal_sigs: sigs.get(&height).map(|s| s.0.clone()).unwrap_or_default(),
            });
        }
        chain
    }

    /// Write one decided BFT height. The roster is not stored: it is recomputed from the bonds
    /// at the block's snapshot on every load (FINALITY.md §8.1).
    pub fn write_bft_decision(
        &self,
        height: u32,
        block: &BftBlock,
        fat_pointer: &FatPointerToBftBlock,
        proposal_sigs: &[TMSig],
    ) -> Result<(), rocksdb::Error> {
        let height = BftHeight(height);
        let mut batch = DiskWriteBatch::new();
        // zs_insert writes into the batch and returns it; the batch is committed below.
        let _ = self.bft_block_by_height_cf()
            .with_batch_for_writing(&mut batch)
            .zs_insert(&height, &StoredBftBlock(block.clone()));
        let _ = self.bft_fat_pointer_by_height_cf()
            .with_batch_for_writing(&mut batch)
            .zs_insert(&height, &StoredFatPointer(fat_pointer.clone()));
        let _ = self.bft_proposal_sigs_by_height_cf()
            .with_batch_for_writing(&mut batch)
            .zs_insert(&height, &ProposalSignatures(proposal_sigs.to_vec()));
        self.db.write(batch)
    }

    /// The decided-chain block this node could not follow when it dropped BFT, or `None` on a
    /// database that never dropped it. Once written it is never cleared: only a resync rejoins
    /// BFT (FINALITY.md §4.3).
    pub fn crosslink_bft_dropped(&self) -> Option<(Height, block::Hash)> {
        self.crosslink_bft_dropped_cf().zs_get(&FinKey).map(|m| (m.height, m.hash))
    }

    /// Record that BFT was dropped because following Π_bft needs `hash` at `height`, a block
    /// this node can no longer hold.
    pub fn write_crosslink_bft_dropped(
        &self,
        height: Height,
        hash: block::Hash,
    ) -> Result<(), rocksdb::Error> {
        let mut batch = DiskWriteBatch::new();
        // zs_insert writes into the batch and returns it; the batch is committed below.
        let _ = self.crosslink_bft_dropped_cf()
            .with_batch_for_writing(&mut batch)
            .zs_insert(&FinKey, &FinMarker { height, hash });
        self.db.write(batch)
    }
}
