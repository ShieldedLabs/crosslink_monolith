//! Bond histories

// To burn the bonds of a finalizer `T` slashed at activation height `A`, we must
// find every bond delegated to `T` at any point in the window `[A - W, A)`. This
// includes bonds still pointing at `T` (sitting ducks) and bonds that retargeted
// or unbonded away from `T` inside the analysis window (cockroaches/fleers). The
// staking action that moves a bond records only its *new* target, so we scan the
// chain and reconstruct bonds' target histories. We could persist every bond's
// history (which we eventually want, to avoid re-scanning when a new social slash
// is added) but for now we persist only bonds that delegated to slashed
// finalizers, so the index stays small.
//
// The scan is NOT one-shot. Scanning ~200k blocks can take hours, so it must run
// at startup (long before any activation height) and be pausable/resumable, then
// keep up live as finalization advances. So it is incremental: a persisted
// watermark (`SlashIndexMeta.next_height`) records the first finalized height not
// yet incorporated, and [`ZebraDb::advance_slash_index`] processes a bounded
// chunk of blocks at a time, writing the interval rows AND the new watermark in
// one atomic batch. The bulk is a chunked catch-up at startup
// ([`ZebraDb::catch_up_slash_index`]); the remainder is the same `advance` call
// driven per finalization. A full rescan (reset to genesis) happens only when the
// configured slashed set changes. Between updates it answers
// [`ZebraDb::bonds_burned_by`] for any window at or below the watermark.

use std::collections::{BTreeSet, HashMap};

use zebra_chain::block::Height;

use crate::{
    service::finalized_state::{
        disk_db::{DiskWriteBatch, WriteDisk},
        disk_format::{
            slashing::{SlashIndexMeta, SlashedBondKey},
            BondKey, MAX_ON_DISK_HEIGHT,
        },
        zebra_db::ZebraDb,
        TypedColumnFamily,
    },
    HashOrHeight,
};

pub const SLASHED_BOND_INTERVALS: &str = "slashed_bond_intervals";
pub const SLASHED_BOND_INDEX_META: &str = "slashed_bond_index_meta";

pub type SlashedBondIntervalsCf<'cf> = TypedColumnFamily<'cf, SlashedBondKey, Height>;
pub type SlashedBondIndexMetaCf<'cf> = TypedColumnFamily<'cf, (), SlashIndexMeta>;

/// in-memory currently-open runs, used while scanning.
/// `bond -> (slashed finalizer it currently points at, height the run began)`.
/// Holds only bonds currently delegated to a slashed finalizer.
/// Carried across `advance` calls and reconstructable from the index on restart
/// (see [`ZebraDb::load_open_slash_runs`]).
pub type OpenSlashRuns = HashMap<BondKey, ([u8; 32], Height)>;

impl ZebraDb {
    pub(crate) fn slashed_bond_intervals_cf(&self) -> SlashedBondIntervalsCf<'_> {
        SlashedBondIntervalsCf::new(&self.db, SLASHED_BOND_INTERVALS)
            .expect("column family was created when database was created")
    }

    pub(crate) fn slashed_bond_index_meta_cf(&self) -> SlashedBondIndexMetaCf<'_> {
        SlashedBondIndexMetaCf::new(&self.db, SLASHED_BOND_INDEX_META)
            .expect("column family was created when database was created")
    }

    /// Bonds delegated to `finalizer` anywhere in `[activation - window_len, activation)`.
    /// Only complete once `activation <= slash_index_next_height()`.
    pub fn bonds_burned_by(
        &self,
        finalizer: &[u8; 32],
        activation: Height,
        window_len: u32,
    ) -> BTreeSet<BondKey> {
        let window_start = activation.0.saturating_sub(window_len);

        // All runs for `finalizer` that began before `activation`.
        let lower = SlashedBondKey {
            finalizer: *finalizer,
            start: Height(0),
            bond: [0; 32],
        };
        let upper = SlashedBondKey {
            finalizer: *finalizer,
            start: activation,
            bond: [0; 32],
        };

        let cf = self.slashed_bond_intervals_cf();
        let mut burned = BTreeSet::new();
        for (key, end) in cf.zs_forward_range_iter(lower..upper) {
            // The scan already bounds start < activation; a run [start, end)
            // overlaps [window_start, activation) iff it also ends after the
            // window's first height. Open runs (end == MAX) always do.
            if end.0 > window_start {
                burned.insert(key.bond);
            }
        }

        burned
    }

    /// First finalized height not yet scanned into the index (0 = nothing yet).
    pub fn slash_index_next_height(&self) -> Height {
        self.slashed_bond_index_meta_cf()
            .zs_get(&())
            .map(|meta| meta.next_height)
            .unwrap_or(Height(0))
    }

    /// Rebuilds the open-run set (sentinel-ended rows) from the index, e.g. on restart.
    pub fn load_open_slash_runs(&self) -> OpenSlashRuns {
        self.slashed_bond_intervals_cf()
            .zs_items_in_range_ordered(..)
            .into_iter()
            .filter(|(_, end)| *end == MAX_ON_DISK_HEIGHT)
            .map(|(key, _)| (key.bond, (key.finalizer, key.start)))
            .collect()
    }

    /// Open-run set to resume from; resets the index to genesis if the slashed set changed.
    pub fn prepare_slash_index(&self, slashed: &BTreeSet<[u8; 32]>) -> OpenSlashRuns {
        let want: Vec<[u8; 32]> = slashed.iter().copied().collect();
        match self.slashed_bond_index_meta_cf().zs_get(&()) {
            Some(meta) if meta.slashed == want => self.load_open_slash_runs(),
            _ => {
                self.reset_slash_index(slashed);
                OpenSlashRuns::new()
            }
        }
    }

    /// Clears the index and sets the watermark back to genesis for `slashed`.
    pub fn reset_slash_index(&self, slashed: &BTreeSet<[u8; 32]>) {
        let keys: Vec<SlashedBondKey> = self
            .slashed_bond_intervals_cf()
            .zs_items_in_range_ordered(..)
            .into_keys()
            .collect();

        let intervals = self
            .db
            .cf_handle(SLASHED_BOND_INTERVALS)
            .expect("column family exists");
        let meta_cf = self
            .db
            .cf_handle(SLASHED_BOND_INDEX_META)
            .expect("column family exists");

        let mut batch = DiskWriteBatch::new();
        for key in keys {
            batch.zs_delete(&intervals, key);
        }
        batch.zs_insert(
            &meta_cf,
            (),
            SlashIndexMeta {
                next_height: Height(0),
                slashed: slashed.iter().copied().collect(),
            },
        );
        self.db
            .write(batch)
            .expect("resetting the slash index should succeed");
    }

    /// incorporate a bounded chunk of finalized blocks, starting at the watermark,
    /// updating `open` and writing the interval rows and the advanced watermark
    /// in a single atomic batch.
    ///
    /// Returns the new watermark (first not-yet-incorporated height).
    /// Idempotently resumable: on restart, reload `open` via [`ZebraDb::load_open_slash_runs`] and call again.
    ///
    /// A run is opened (sentinel-ended row) the moment a bond targets a slashed
    /// finalizer and closed (real end) when it leaves; a run that opens and closes
    /// in the same block is dropped, collapsing same-block churn to the net
    /// end-of-block target.
    pub fn advance_slash_index(
        &self,
        slashed: &BTreeSet<[u8; 32]>,
        open: &mut OpenSlashRuns,
        max_blocks: u32,
    ) -> Height {
        use zcash_primitives::transaction::StakingActionKind::*;

        let from = self.slash_index_next_height().0;
        let Some(tip) = self.finalized_tip_height() else {
            return Height(from);
        };
        if from > tip.0 {
            return Height(from);
        }
        let to = tip.0.min(from.saturating_add(max_blocks.saturating_sub(1)));

        let intervals = self
            .db
            .cf_handle(SLASHED_BOND_INTERVALS)
            .expect("column family exists");
        let meta_cf = self
            .db
            .cf_handle(SLASHED_BOND_INDEX_META)
            .expect("column family exists");
        let mut batch = DiskWriteBatch::new();

        for h in from..=to {
            let height = Height(h);
            let Some(block) = self.block(HashOrHeight::Height(height)) else {
                continue;
            };

            for tx in block.transactions.iter() {
                let Some(action) = tx.staking_action() else {
                    continue;
                };
                let bond = action.arg32_0;

                match action.kind {
                    CreateNewDelegationBond => {
                        if slashed.contains(&action.arg32_2) {
                            open.insert(bond, (action.arg32_2, height));
                            batch.zs_insert(
                                &intervals,
                                SlashedBondKey { finalizer: action.arg32_2, start: height, bond },
                                MAX_ON_DISK_HEIGHT,
                            );
                        }
                    }
                    RetargetDelegationBond => {
                        if let Some((finalizer, start)) = open.remove(&bond) {
                            let key = SlashedBondKey { finalizer, start, bond };
                            if start.0 < height.0 {
                                batch.zs_insert(&intervals, key, height);
                            } else {
                                batch.zs_delete(&intervals, key);
                            }
                        }
                        if slashed.contains(&action.arg32_2) {
                            open.insert(bond, (action.arg32_2, height));
                            batch.zs_insert(
                                &intervals,
                                SlashedBondKey { finalizer: action.arg32_2, start: height, bond },
                                MAX_ON_DISK_HEIGHT,
                            );
                        }
                    }
                    BeginDelegationUnbonding => {
                        if let Some((finalizer, start)) = open.remove(&bond) {
                            let key = SlashedBondKey { finalizer, start, bond };
                            if start.0 < height.0 {
                                batch.zs_insert(&intervals, key, height);
                            } else {
                                batch.zs_delete(&intervals, key);
                            }
                        }
                    }
                    // WithdrawDelegationBond: the run already closed at unbonding
                    // @Todo: Convert/Register/UpdateFinalizerKey
                    _ => {}
                }
            }
        }

        let next = Height(to + 1);
        batch.zs_insert(
            &meta_cf,
            (),
            SlashIndexMeta {
                next_height: next,
                slashed: slashed.iter().copied().collect(),
            },
        );
        self.db
            .write(batch)
            .expect("writing a slash index chunk should succeed");

        next
    }

    /// intended for bulk catch-up at startup
    pub fn catch_up_slash_index(&self, slashed: &BTreeSet<[u8; 32]>, max_blocks: u32) {
        let mut open = self.prepare_slash_index(slashed);
        while let Some(tip) = self.finalized_tip_height() {
            if self.slash_index_next_height().0 > tip.0 {
                break;
            }
            self.advance_slash_index(slashed, &mut open, max_blocks);
        }
    }

    /// One resumable step: prepare (reset if the slashed set changed), then advance one chunk.
    pub fn advance_slash_index_step(&self, slashed: &BTreeSet<[u8; 32]>, max_blocks: u32) -> Height {
        let mut open = self.prepare_slash_index(slashed);
        self.advance_slash_index(slashed, &mut open, max_blocks)
    }
}

/// Background driver: catch the index up to the finalized tip, then keep up as it advances.
/// The heavy scan runs in chunks on a blocking thread and resumes across restarts via the watermark.
pub async fn drive_slash_index(db: ZebraDb, slashed: BTreeSet<[u8; 32]>) {
    if slashed.is_empty() {
        return;
    }
    const CHUNK: u32 = 1000;
    loop {
        let db = db.clone();
        let slashed = slashed.clone();
        let caught_up = match tokio::task::spawn_blocking(move || {
            let next = db.advance_slash_index_step(&slashed, CHUNK);
            db.finalized_tip_height().map_or(true, |tip| next.0 > tip.0)
        })
        .await
        {
            Ok(caught_up) => caught_up,
            Err(_) => return,
        };
        if caught_up {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        constants::{state_database_format_version_in_code, STATE_DATABASE_KIND},
        service::finalized_state::STATE_COLUMN_FAMILIES_IN_CODE,
        Config,
    };
    use zebra_chain::parameters::Network::Mainnet;

    fn new_ephemeral_db() -> ZebraDb {
        ZebraDb::new(
            &Config::ephemeral(),
            STATE_DATABASE_KIND,
            &state_database_format_version_in_code(),
            &Mainnet,
            true,
            STATE_COLUMN_FAMILIES_IN_CODE.iter().map(ToString::to_string),
            false,
        )
    }

    fn put(db: &ZebraDb, finalizer: [u8; 32], start: u32, bond: [u8; 32], end: Height) {
        db.slashed_bond_intervals_cf()
            .new_batch_for_writing()
            .zs_insert(
                &SlashedBondKey {
                    finalizer,
                    start: Height(start),
                    bond,
                },
                &end,
            )
            .write_batch()
            .expect("write should succeed");
    }

    fn set(finalizers: &[[u8; 32]]) -> BTreeSet<[u8; 32]> {
        finalizers.iter().copied().collect()
    }

    /// The overlap query returns exactly the bonds delegated to the finalizer
    /// within `[A - W, A)`: sitting ducks, fleers that left inside the window, and
    /// bonds that joined inside the window — but not bonds that left at or before
    /// the window start, started at/after activation, or pointed elsewhere.
    #[test]
    fn burned_by_overlap() {
        let db = new_ephemeral_db();
        let t = [1u8; 32];
        let other = [2u8; 32];
        let b = |n: u8| [n; 32];

        put(&db, t, 500, b(1), MAX_ON_DISK_HEIGHT); // sitting duck (open across window)
        put(&db, t, 600, b(2), Height(800)); // fled inside window
        put(&db, t, 400, b(3), Height(650)); // fled before window start
        put(&db, t, 900, b(4), Height(950)); // joined inside window
        put(&db, t, 1000, b(5), MAX_ON_DISK_HEIGHT); // started at activation
        put(&db, other, 600, b(6), MAX_ON_DISK_HEIGHT); // different finalizer
        put(&db, t, 500, b(7), Height(700)); // fled exactly at window start
        put(&db, t, 700, b(8), MAX_ON_DISK_HEIGHT); // joined exactly at window start

        let burned = db.bonds_burned_by(&t, Height(1000), 300);

        let expected: BTreeSet<BondKey> = [b(1), b(2), b(4), b(8)].into_iter().collect();
        assert_eq!(burned, expected);
    }

    /// The open-run set is reconstructed from sentinel-ended rows only.
    #[test]
    fn open_runs_are_reconstructed() {
        let db = new_ephemeral_db();
        let t = [1u8; 32];
        let b = |n: u8| [n; 32];

        put(&db, t, 100, b(1), MAX_ON_DISK_HEIGHT); // open
        put(&db, t, 200, b(2), Height(250)); // closed
        put(&db, t, 300, b(3), MAX_ON_DISK_HEIGHT); // open

        let open = db.load_open_slash_runs();

        let expected: OpenSlashRuns =
            [(b(1), (t, Height(100))), (b(3), (t, Height(300)))].into_iter().collect();
        assert_eq!(open, expected);
    }

    /// `prepare` reuses progress when the slashed set is unchanged, but resets the
    /// index (clears rows, watermark back to genesis) when it changes.
    #[test]
    fn prepare_resets_only_when_slashed_set_changes() {
        let db = new_ephemeral_db();
        let t = [1u8; 32];
        let u = [9u8; 32];

        db.reset_slash_index(&set(&[t]));
        put(&db, t, 100, [1u8; 32], MAX_ON_DISK_HEIGHT);
        // Pretend the scan progressed.
        db.slashed_bond_index_meta_cf()
            .new_batch_for_writing()
            .zs_insert(&(), &SlashIndexMeta { next_height: Height(500), slashed: vec![t] })
            .write_batch()
            .expect("write should succeed");

        // Same set: progress and rows are preserved.
        let open = db.prepare_slash_index(&set(&[t]));
        assert_eq!(open.len(), 1);
        assert_eq!(db.slash_index_next_height(), Height(500));

        // Different set: full reset.
        let open = db.prepare_slash_index(&set(&[t, u]));
        assert!(open.is_empty());
        assert_eq!(db.slash_index_next_height(), Height(0));
        assert!(db.load_open_slash_runs().is_empty());
    }
}
