//! Decided BFT chain database access and write methods.

use zcash_primitives::bft::{BftBlock, FatPointerToBftBlock, TMSig};

use zebra_chain::block::{self, Height};

use crate::service::finalized_state::{
    disk_db::DiskWriteBatch,
    disk_format::bft::{BftHeight, ProposalSignatures, StoredBftBlock, StoredFatPointer, TenderlinkWalRecord},
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

/// The name of this node's tenderlink WAL column family, by BFT height.
pub const TENDERLINK_WAL_BY_HEIGHT: &str = "tenderlink_wal_by_height";

/// How many BFT heights below the newest WAL record keep theirs. Not only the height being
/// decided needs its record: `restore` resumes BFT below a stored decision whose snapshot this
/// state lost, and there the record is what keeps this node to the lock it held.
pub const TENDERLINK_WAL_HEIGHTS_KEPT: u32 = 64;

/// The type for reading this node's tenderlink WAL records.
pub type TenderlinkWalByHeightCf<'cf> = TypedColumnFamily<'cf, BftHeight, TenderlinkWalRecord>;

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

    fn tenderlink_wal_by_height_cf(&self) -> TenderlinkWalByHeightCf<'_> {
        TenderlinkWalByHeightCf::new(&self.db, TENDERLINK_WAL_BY_HEIGHT)
            .expect("column family was created when database was created")
    }

    /// This node's tenderlink WAL records from BFT height `from` up, ascending.
    pub fn tenderlink_wal(&self, from: u32) -> Vec<(u32, Vec<u8>)> {
        self.tenderlink_wal_by_height_cf()
            .zs_items_in_range_ordered(BftHeight(from)..)
            .into_iter()
            .map(|(height, record)| (height.0, record.0))
            .collect()
    }

    /// Store this node's tenderlink WAL record for `height`, replacing the one stored before, and
    /// drop the records more than [`TENDERLINK_WAL_HEIGHTS_KEPT`] heights below it. Synced to
    /// disk before it returns: tenderlink sends what it signed only once it holds this.
    pub fn write_tenderlink_wal(&self, height: u32, record: Vec<u8>) -> Result<(), rocksdb::Error> {
        let keep_from = BftHeight(height.saturating_sub(TENDERLINK_WAL_HEIGHTS_KEPT));
        let cf = self.tenderlink_wal_by_height_cf();
        // Only when there is a row to drop: a range delete leaves a tombstone, and one per vote
        // would pile up faster than compaction clears them.
        let has_old_rows = cf.zs_first_key_value().is_some_and(|(first, _)| first < keep_from);
        let mut batch = DiskWriteBatch::new();
        // zs_insert and zs_delete_range write into the batch and return it; it is committed below.
        let writer = cf.with_batch_for_writing(&mut batch).zs_insert(&BftHeight(height), &TenderlinkWalRecord(record));
        if has_old_rows {
            let _ = writer.zs_delete_range(&BftHeight(0), &keep_from);
        }
        self.db.write_synced(batch)
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

    /// The decided height `height`, or `None` if its block or fat pointer is missing. Read alone,
    /// so a caller walking the chain holds one decision at a time rather than all of them (see
    /// [`Self::bft_chain`], which reads every row at once).
    pub fn bft_decision(&self, height: u32) -> Option<StoredDecision> {
        let height = BftHeight(height);
        let block = self.bft_block_by_height_cf().zs_get(&height)?;
        let fat_pointer = self.bft_fat_pointer_by_height_cf().zs_get(&height)?;
        let proposal_sigs = self.bft_proposal_sigs_by_height_cf().zs_get(&height).map(|s| s.0).unwrap_or_default();
        Some(StoredDecision { block: block.0, fat_pointer: fat_pointer.0, proposal_sigs })
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
    use std::collections::BTreeMap;

    use rand::{Rng, RngCore, SeedableRng};
    use rand_chacha::ChaCha20Rng;
    use zebra_chain::parameters::Network;

    use super::TENDERLINK_WAL_HEIGHTS_KEPT;
    use crate::{service::finalized_state::FinalizedState, Config};

    /// Tenderlink's WAL records read back as last written, from any height up, and a record is
    /// dropped only once one is written more than `TENDERLINK_WAL_HEIGHTS_KEPT` heights above it.
    /// Heights mostly climb, by one or by a jump as after catching up; some are written again,
    /// some lower, as after a restart that resumes below a stored decision, and some lie at the
    /// ends of the range.
    #[test]
    fn tenderlink_wal_rows_read_back_and_drop_only_far_below_the_newest() {
        let _init_guard = zebra_test::init();
        let seed: u64 = std::env::var("CROSSLINK_FUZZ_SEED")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| rand::thread_rng().next_u64());
        eprintln!("CROSSLINK_FUZZ_SEED={seed}");
        let cases: u64 = std::env::var("CROSSLINK_FUZZ_CASES").ok().and_then(|s| s.parse().ok()).unwrap_or(8);

        let (mut total_dropped, mut total_lowered) = (0, 0);
        for case in 0..cases {
            let mut rng = ChaCha20Rng::seed_from_u64(seed ^ case.wrapping_mul(0x9E37_79B9_7F4A_7C15));
            let state = FinalizedState::new(
                &Config::ephemeral(),
                &Network::Mainnet,
                #[cfg(feature = "elasticsearch")]
                false,
            )
            .expect("an ephemeral database opens");
            let db = &state.db;

            let mut model: BTreeMap<u32, Vec<u8>> = BTreeMap::new();
            let mut height: u32 = match rng.gen_range(0..3) {
                0 => rng.gen_range(0..TENDERLINK_WAL_HEIGHTS_KEPT * 2),
                1 => u32::MAX - rng.gen_range(0..TENDERLINK_WAL_HEIGHTS_KEPT * 4),
                _ => rng.gen(),
            };
            let (mut dropped, mut lowered) = (0, 0);
            for step in 0..rng.gen_range(50..400) {
                height = match rng.gen_range(0..10) {
                    0..=4 => height.saturating_add(1),
                    5 => height,
                    6 | 7 => height.saturating_add(rng.gen_range(2..3 * TENDERLINK_WAL_HEIGHTS_KEPT)),
                    _ => { lowered += 1; height.saturating_sub(rng.gen_range(1..TENDERLINK_WAL_HEIGHTS_KEPT)) }
                };
                let mut record = vec![0u8; rng.gen_range(0..2048)];
                rng.fill_bytes(&mut record);
                db.write_tenderlink_wal(height, record.clone()).expect("the write succeeds");

                let keep_from = height.saturating_sub(TENDERLINK_WAL_HEIGHTS_KEPT);
                let before = model.len();
                model.retain(|&h, _| h >= keep_from);
                dropped += before - model.len();
                model.insert(height, record);

                let from = match rng.gen_range(0..3) {
                    0 => 0,
                    1 => *model.keys().next().unwrap(),
                    _ => rng.gen_range(keep_from..=height),
                };
                let expected: Vec<(u32, Vec<u8>)> = model.range(from..).map(|(&h, r)| (h, r.clone())).collect();
                assert_eq!(db.tenderlink_wal(from), expected, "seed {seed} case {case} step {step}: height {height}, from {from}");
            }
            eprintln!("case {case}: {} rows kept, {dropped} dropped, {lowered} writes lower than the last", model.len());
            total_dropped += dropped;
            total_lowered += lowered;
        }
        if cases >= 4 {
            assert!(total_dropped > 0 && total_lowered > 0, "seed {seed}: {total_dropped} rows dropped, {total_lowered} writes lower than the last");
        }
    }
}
