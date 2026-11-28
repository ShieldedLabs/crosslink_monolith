//! Decided BFT chain database access and write methods.

pub(crate) use super::super::disk_db::SnapshotReadError;

use zcash_primitives::bft::{BftBlock, FatPointerToBftBlock, TMSig};

use zebra_chain::block::{self, Height};

use crate::service::finalized_state::{
    disk_db::DiskWriteBatch,
    disk_format::bft::{BftConflictKey, BftHeight, ProposalSignatures, StoredBftBlock, StoredFatPointer, TenderlinkWalRecord},
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

/// The certificate of each conflicting quorum this node has seen: one for a block other than the
/// one it decided at that height. Kept as evidence of who signed both.
pub const BFT_CONFLICTING_CERTIFICATES: &str = "bft_conflicting_certificates";

/// How many BFT heights below the newest WAL record keep theirs. Not only the height being
/// decided needs its record: `restore` resumes BFT below a stored decision whose snapshot this
/// state lost, and there the record is what keeps this node to the lock it held.
pub const TENDERLINK_WAL_HEIGHTS_KEPT: u32 = 64;

/// The type for reading this node's tenderlink WAL records.
pub type TenderlinkWalByHeightCf<'cf> = TypedColumnFamily<'cf, BftHeight, TenderlinkWalRecord>;

/// Private signer metadata and three full body regions, strictly after every
/// four-byte BFT height. The root is also the old monolithic image key: replacing
/// its envelope makes old executables reject the split state, not bootstrap.
/// All legacy-height scans must remain bounded by `BftHeight(u32::MAX)`.
const D28_BLOCK_KEYS: [&[u8]; 4] = [
    &[0xff; 8],
    &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 1],
    &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 2],
    &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 3],
];

#[derive(Debug)]
struct D28BlockKey(usize);

impl crate::service::finalized_state::disk_format::IntoDisk for D28BlockKey {
    type Bytes = &'static [u8];
    fn as_bytes(&self) -> Self::Bytes { D28_BLOCK_KEYS[self.0] }
}

impl crate::service::finalized_state::disk_format::FromDisk for D28BlockKey {
    fn from_bytes(bytes: impl AsRef<[u8]>) -> Self {
        Self(D28_BLOCK_KEYS.iter().position(|key| *key == bytes.as_ref()).expect("one of four D28 keys"))
    }
}

/// Borrow changed regions until RocksDB copies them into the one write batch.
struct D28Part<'a>(std::borrow::Cow<'a, [u8]>);

impl<'a> crate::service::finalized_state::disk_format::IntoDisk for D28Part<'a> {
    type Bytes = std::borrow::Cow<'a, [u8]>;
    fn as_bytes(&self) -> Self::Bytes { self.0.clone() }
}

impl crate::service::finalized_state::disk_format::FromDisk for D28Part<'_> {
    fn from_bytes(bytes: impl AsRef<[u8]>) -> Self {
        Self(std::borrow::Cow::Owned(bytes.as_ref().to_vec()))
    }
}

pub type BftConflictingCertificatesCf<'cf> = TypedColumnFamily<'cf, BftConflictKey, StoredFatPointer>;

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

    fn bft_conflicting_certificates_cf(&self) -> BftConflictingCertificatesCf<'_> {
        BftConflictingCertificatesCf::new(&self.db, BFT_CONFLICTING_CERTIFICATES)
            .expect("column family was created when database was created")
    }

    /// Keeps the certificate of a conflicting quorum at `height` for block `other`.
    pub fn write_bft_conflicting_certificate(&self, height: u32, other: [u8; 32], fat_pointer: &FatPointerToBftBlock) -> Result<(), rocksdb::Error> {
        let mut batch = DiskWriteBatch::new();
        // zs_insert writes into the batch and returns it; the batch is committed below.
        let _ = self
            .bft_conflicting_certificates_cf()
            .with_batch_for_writing(&mut batch)
            .zs_insert(&BftConflictKey { height, other }, &StoredFatPointer(fat_pointer.clone()));
        self.db.write(batch)
    }

    /// Every conflicting certificate this node keeps, by height then block.
    pub fn bft_conflicting_certificates(&self) -> Vec<(u32, [u8; 32], FatPointerToBftBlock)> {
        self.bft_conflicting_certificates_cf()
            .zs_items_in_range_ordered(..)
            .into_iter()
            .map(|(key, fat_pointer)| (key.height, key.other, fat_pointer.0))
            .collect()
    }

    /// This node's legacy tenderlink WAL records from BFT height `from` up, ascending.
    /// The explicit upper bound excludes D28's separate local signer-image key.
    pub fn tenderlink_wal(&self, from: u32) -> Vec<(u32, Vec<u8>)> {
        let cf = self.tenderlink_wal_by_height_cf();
        // Read the last key separately: an inclusive all-FF upper bound would wrap
        // in the generic fixed-width range iterator's upper-bound conversion.
        let mut records: Vec<_> = cf
            .zs_items_in_range_ordered(BftHeight(from)..BftHeight(u32::MAX))
            .into_iter()
            .map(|(height, record)| (height.0, record.0))
            .collect();
        if let Some(record) = cf.zs_get(&BftHeight(u32::MAX)) {
            records.push((u32::MAX, record.0));
        }
        records
    }

    /// Read all four D28 keys at one RocksDB snapshot, never a mix of commits.
    /// The trusted maximum sizes bound copying; the adapter checks exact lengths,
    /// envelope and identity. DurableStore validates the reassembled logical image.
    pub(crate) fn d28_atomic_parts(&self, max_sizes: [usize; 4]) -> Result<[Option<Vec<u8>>; 4], super::super::disk_db::SnapshotReadError> {
        self.db.snapshot_get_cf(TENDERLINK_WAL_BY_HEIGHT, D28_BLOCK_KEYS, max_sizes)
    }

    /// Any D28 row is signer state, including a corrupt orphan body without a root.
    /// Presence does not copy the potentially malformed rows.
    pub(crate) fn has_d28_state(&self) -> Result<bool, rocksdb::Error> {
        self.db.snapshot_contains_any_cf(TENDERLINK_WAL_BY_HEIGHT, D28_BLOCK_KEYS)
    }

    /// All test fixtures are smaller than this fixed bound.
    #[cfg(test)]
    pub(crate) fn d28_test_parts(&self) -> [Option<Vec<u8>>; 4] {
        self.d28_atomic_parts([64 * 1024; 4]).unwrap()
    }

    /// Atomically replace the root and selected full body rows, then sync once.
    /// No row is deleted, and unlike the legacy WAL writer this never prunes heights.
    /// The single-writer adapter must supply all bodies on its first split commit.
    pub(crate) fn write_d28_atomic_parts(&self, root: &[u8], bodies: [Option<&[u8]>; 3]) -> Result<(), rocksdb::Error> {
        let mut batch = DiskWriteBatch::new();
        let cf = TypedColumnFamily::<D28BlockKey, D28Part<'_>>::new(&self.db, TENDERLINK_WAL_BY_HEIGHT)
            .expect("column family was created when database was created");
        let mut writer = cf.with_batch_for_writing(&mut batch)
            .zs_insert(&D28BlockKey(0), &D28Part(std::borrow::Cow::Borrowed(root)));
        for (slot, body) in bodies.into_iter().enumerate() {
            if let Some(body) = body {
                writer = writer.zs_insert(&D28BlockKey(slot + 1), &D28Part(std::borrow::Cow::Borrowed(body)));
            }
        }
        drop(writer);
        self.db.write_synced(batch)
    }

    /// Row-operation counter for isolated single-writer tests, not disk traffic.
    #[cfg(test)]
    pub(crate) fn d28_test_sequence_number(&self) -> u64 {
        self.db.test_sequence_number()
    }

    /// Test-only corruption and legacy fixtures; never used by the live signer.
    #[cfg(test)]
    pub(crate) fn set_d28_test_parts(&self, parts: [Option<&[u8]>; 4]) {
        let mut batch = DiskWriteBatch::new();
        let cf = TypedColumnFamily::<D28BlockKey, D28Part<'_>>::new(&self.db, TENDERLINK_WAL_BY_HEIGHT).unwrap();
        let mut writer = cf.with_batch_for_writing(&mut batch);
        for (slot, part) in parts.into_iter().enumerate() {
            writer = match part {
                Some(part) => writer.zs_insert(&D28BlockKey(slot), &D28Part(std::borrow::Cow::Borrowed(part))),
                None => writer.zs_delete(&D28BlockKey(slot)),
            };
        }
        drop(writer);
        self.db.write_synced(batch).unwrap();
    }

    /// Store this node's tenderlink WAL record for `height`, replacing the one stored before, and
    /// drop the records more than [`TENDERLINK_WAL_HEIGHTS_KEPT`] heights below it. Synced to
    /// disk before it returns: tenderlink sends what it signed only once it holds this.
    pub fn write_tenderlink_wal(&self, height: u32, record: Vec<u8>) -> Result<(), rocksdb::Error> {
        let keep_from = BftHeight(height.saturating_sub(TENDERLINK_WAL_HEIGHTS_KEPT));
        let cf = self.tenderlink_wal_by_height_cf();
        // Bound this scan too: when the D28 image is the only row, it is not a BftHeight.
        let has_old_rows = cf.zs_forward_range_iter(BftHeight(0)..keep_from).next().is_some();
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

    #[test]
    fn d28_atomic_parts_are_separate_from_all_legacy_wal_heights() {
        let _init_guard = zebra_test::init();
        let state = FinalizedState::new(
            &Config::ephemeral(),
            &Network::Mainnet,
            #[cfg(feature = "elasticsearch")]
            false,
        ).expect("an ephemeral database opens");
        let db = &state.db;
        assert_eq!(db.d28_test_parts(), [None, None, None, None]);
        assert!(!db.has_d28_state().unwrap());
        for key in super::D28_BLOCK_KEYS {
            assert!(key > u32::MAX.to_be_bytes().as_slice());
        }
        let first = vec![0xa5; 4096];
        db.write_d28_atomic_parts(&first, [Some(&first); 3]).unwrap();
        let committed = db.d28_test_parts();
        assert!(db.has_d28_state().unwrap());
        assert!(db.tenderlink_wal(0).is_empty());
        // Both the pruning scan and the max-height read must ignore all four keys.
        db.write_tenderlink_wal(0, vec![1]).unwrap();
        assert_eq!(db.tenderlink_wal(0), vec![(0, vec![1])]);
        assert_eq!(db.d28_test_parts(), committed);
        db.write_tenderlink_wal(u32::MAX - TENDERLINK_WAL_HEIGHTS_KEPT, vec![3]).unwrap();
        db.write_tenderlink_wal(u32::MAX, vec![2]).unwrap();
        assert_eq!(db.tenderlink_wal(u32::MAX), vec![(u32::MAX, vec![2])]);
        assert_eq!(db.d28_test_parts(), committed);
        let second = vec![0x5a; 4096];
        db.write_d28_atomic_parts(&second, [None, Some(&second), None]).unwrap();
        assert_eq!(db.clone().d28_test_parts(), [
            Some(second.clone()), Some(first.clone()), Some(second), Some(first),
        ]);
        assert_eq!(db.tenderlink_wal(0), vec![
            (u32::MAX - TENDERLINK_WAL_HEIGHTS_KEPT, vec![3]), (u32::MAX, vec![2]),
        ]);
    }

    #[test]
    fn d28_presence_counts_every_orphan_part_without_needing_valid_bytes() {
        let _init_guard = zebra_test::init();
        let state = FinalizedState::new(
            &Config::ephemeral(),
            &Network::Mainnet,
            #[cfg(feature = "elasticsearch")]
            false,
        ).unwrap();
        for slot in 0..4 {
            let mut parts = [None; 4];
            parts[slot] = Some(&b""[..]); // even an empty row is state, never fresh
            state.db.set_d28_test_parts(parts);
            assert!(state.db.has_d28_state().unwrap(), "part {slot}");
        }
        state.db.set_d28_test_parts([None; 4]);
        assert!(!state.db.has_d28_state().unwrap());
    }

    #[test]
    fn d28_snapshot_never_mixes_concurrent_atomic_batches() {
        let _init_guard = zebra_test::init();
        let state = FinalizedState::new(
            &Config::ephemeral(),
            &Network::Mainnet,
            #[cfg(feature = "elasticsearch")]
            false,
        ).unwrap();
        let db = &state.db;
        let first = [0; 2048];
        db.write_d28_atomic_parts(&first, [Some(&first); 3]).unwrap();
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let writer = scope.spawn(|| {
                barrier.wait();
                for generation in 1u8..=128 {
                    let next = [generation; 2048];
                    db.write_d28_atomic_parts(&next, [Some(&next); 3]).unwrap();
                    std::thread::yield_now();
                }
            });
            barrier.wait();
            for _ in 0..512 {
                let parts = db.d28_atomic_parts([2048; 4]).unwrap();
                assert!(parts.iter().all(|part| part == &parts[0]));
                std::thread::yield_now();
            }
            writer.join().unwrap();
        });
        assert_eq!(db.d28_test_parts(), std::array::from_fn(|_| Some(vec![128; 2048])));
    }

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
