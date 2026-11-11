//! Decided BFT chain database access and write methods.

use tenderlink::TMLock;
use zcash_primitives::bft::{BftBlock, FatPointerToBftBlock, TMSig};

use zebra_chain::block::{self, Height};

use crate::service::finalized_state::{
    disk_db::DiskWriteBatch,
    disk_format::bft::{BftHeight, BftLockKey, ProposalSignatures, StoredBftBlock, StoredBftLock, StoredFatPointer},
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

/// The name of the finalizer's lock column family (one row).
pub const BFT_LOCK: &str = "bft_lock";

/// The type for reading the finalizer's lock from the database.
pub type BftLockCf<'cf> = TypedColumnFamily<'cf, BftLockKey, StoredBftLock>;

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

    fn bft_lock_cf(&self) -> BftLockCf<'_> {
        BftLockCf::new(&self.db, BFT_LOCK)
            .expect("column family was created when database was created")
    }

    /// The lock this finalizer last stored, at whatever height it was taken.
    pub fn bft_lock(&self) -> Option<TMLock> {
        self.bft_lock_cf().zs_get(&BftLockKey).map(|stored| stored.0)
    }

    /// Replace the stored lock. Tenderlink asks for this before each value precommit and does
    /// not precommit until it succeeds.
    pub fn write_bft_lock(&self, lock: &TMLock) -> Result<(), rocksdb::Error> {
        let mut batch = DiskWriteBatch::new();
        // zs_insert writes into the batch and returns it; the batch is committed below.
        let _ = self.bft_lock_cf()
            .with_batch_for_writing(&mut batch)
            .zs_insert(&BftLockKey, &StoredBftLock(lock.clone()));
        self.db.write(batch)
    }

    /// The decided BFT chain, ascending from height 0. Stops at the first gap: a crash before
    /// a row lands leaves a short chain, which resumes by re-deciding that height rather than by
    /// loading past the hole. A row is written before its snapshot is durable (the snapshot can
    /// still be only in the non-finalized state), so a loaded row may name a snapshot this state
    /// does not hold; `restore` resumes BFT below the first such row.
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

    /// Up to `count` decided heights from `from` on, ascending, stopping at the first gap like
    /// [`Self::bft_chain`]: what a peer catching up asks for. Read one height at a time, and no
    /// further once the decisions read reach `byte_budget` (the last one may cross it), so a
    /// request costs what its answer can carry, not `count` reads.
    pub fn bft_decisions(&self, from: u32, count: u32, byte_budget: usize) -> Vec<StoredDecision> {
        /// Counts the bytes a serializer writes, without keeping them.
        struct ByteCount(usize);
        impl std::io::Write for ByteCount {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0 += buf.len();
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let (blocks, fat_pointers, sigs) =
            (self.bft_block_by_height_cf(), self.bft_fat_pointer_by_height_cf(), self.bft_proposal_sigs_by_height_cf());
        let mut decisions = Vec::new();
        let mut bytes = 0usize;
        for height in (from..from.saturating_add(count)).map(BftHeight) {
            if bytes >= byte_budget {
                break;
            }
            let (Some(block), Some(fat_pointer)) = (blocks.zs_get(&height), fat_pointers.zs_get(&height)) else { break };
            let proposal_sigs = sigs.zs_get(&height).map(|s| s.0).unwrap_or_default();
            let mut size = ByteCount(proposal_sigs.len() * std::mem::size_of::<TMSig>());
            block.0.zcash_serialize(&mut size).expect("counting bytes cannot fail");
            fat_pointer.0.zcash_serialize(&mut size).expect("counting bytes cannot fail");
            bytes += size.0;
            decisions.push(StoredDecision { block: block.0, fat_pointer: fat_pointer.0, proposal_sigs });
        }
        decisions
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

#[cfg(test)]
mod tests {
    use tenderlink::{BlockValue, TMLock};
    use zebra_chain::parameters::Network;

    use crate::{
        constants::{state_database_format_version_in_code, STATE_DATABASE_KIND},
        service::finalized_state::{ZebraDb, STATE_COLUMN_FAMILIES_IN_CODE},
        Config,
    };

    fn open(cache_dir: &std::path::Path) -> ZebraDb {
        let config = Config { cache_dir: cache_dir.to_path_buf(), ephemeral: false, ..Config::default() };
        ZebraDb::new(
            &config,
            STATE_DATABASE_KIND,
            &state_database_format_version_in_code(),
            &Network::Mainnet,
            true,
            STATE_COLUMN_FAMILIES_IN_CODE.iter().map(ToString::to_string),
            false,
        )
        .expect("opening the database should succeed")
    }

    /// The lock is what a finalizer must not forget across a restart, so it has to come back from
    /// a database that was closed and opened again, and the newest one wins.
    #[test]
    fn the_bft_lock_survives_reopening_the_database() {
        let _init_guard = zebra_test::init();
        let cache_dir = tempfile::tempdir().expect("creating a temporary directory should succeed");
        let first = TMLock { height: 7, round: 3, value: BlockValue((0..2000u32).map(|i| i as u8).collect()) };
        let second = TMLock { height: 8, round: 0, value: BlockValue(vec![9; 17]) };

        let db = open(cache_dir.path());
        assert_eq!(db.bft_lock(), None);
        db.write_bft_lock(&first).expect("writing the lock should succeed");
        drop(db);

        let db = open(cache_dir.path());
        assert_eq!(db.bft_lock(), Some(first));
        db.write_bft_lock(&second).expect("writing the lock should succeed");
        drop(db);

        let db = open(cache_dir.path());
        assert_eq!(db.bft_lock(), Some(second));
    }
}
