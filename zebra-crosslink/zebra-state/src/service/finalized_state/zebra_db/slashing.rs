//! Bond histories

// To burn the bonds of a finalizer `T` slashed at activation height `A`, we must
// find every bond delegated to `T` at any point in the window `[A - W, A)`. This
// includes bonds still pointing at `T` (sitting ducks) and bonds that retargeted
// or unbonded away from `T` inside the analysis window (cockroaches/fleers). The
// staking action that moves a bond records only its *new* target, so we scan the
// chain and reconstruct bonds' target histories. We could actually persist every
// bonds' history (which we actually do want, in order to avoid re-scanning chain
// history every time a new social slashing is added) but for now we just persist
// bonds that delegated to slashed finalizers, so the index can stay small. It is
// just rebuilt by another full chain scan whenever the set of slashed finalizers
// changes. Between rebuilds, it answers [`ZebraDb::bonds_burned_by`], no rescan.

use std::collections::{BTreeSet, HashMap};

use zebra_chain::block::Height;

use crate::{
    service::finalized_state::{
        disk_format::{slashing::SlashedBondKey, BondKey, MAX_ON_DISK_HEIGHT},
        zebra_db::ZebraDb,
        TypedColumnFamily,
    },
    HashOrHeight,
};

pub const SLASHED_BOND_INTERVALS: &str = "slashed_bond_intervals";

pub type SlashedBondIntervalsCf<'cf> = TypedColumnFamily<'cf, SlashedBondKey, Height>;

impl ZebraDb {
    pub(crate) fn slashed_bond_intervals_cf(&self) -> SlashedBondIntervalsCf<'_> {
        SlashedBondIntervalsCf::new(&self.db, SLASHED_BOND_INTERVALS)
            .expect("column family was created when database was created")
    }

    /// Returns the bonds delegated to `finalizer` at any height in the analysis
    /// window: `[activation - window_len, activation)`.
    /// Answered entirely from the index; the caller must have built it for a
    /// slashed-finalizer set containing `finalizer` (see [`ZebraDb::rebuild_slash_index`]).
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

    /// Rebuilds the index from the full finalized chain for the given `slashed`
    /// finalizers, replacing any previous contents.
    ///
    /// One pass over every finalized block, tracking only bonds currently
    /// delegated to a slashed finalizer, so both the work kept in memory and the
    /// stored output are proportional to slashed-finalizer activity rather than to
    /// the whole chain. A run is written only when non-empty, which collapses
    /// same-block retarget churn to the net end-of-block target.
    pub fn rebuild_slash_index(&self, slashed: &BTreeSet<[u8; 32]>) {
        use zcash_primitives::transaction::StakingActionKind::*;

        let cf = self.slashed_bond_intervals_cf();

        // Replace prior contents (the slashed set may have changed).
        let existing: Vec<SlashedBondKey> =
            cf.zs_items_in_range_ordered(..).into_keys().collect();
        let mut batch = cf.new_batch_for_writing();
        for key in existing {
            batch = batch.zs_delete(&key);
        }

        // bond -> (slashed finalizer it currently points at, height the run began).
        // Only holds bonds currently delegated to a slashed finalizer.
        let mut open: HashMap<BondKey, ([u8; 32], Height)> = HashMap::new();

        let tip = self.finalized_tip_height().unwrap_or(Height(0));
        for h in 0..=tip.0 {
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
                        }
                    }
                    RetargetDelegationBond => {
                        if let Some((finalizer, start)) = open.remove(&bond) {
                            if start.0 < height.0 {
                                let key = SlashedBondKey { finalizer, start, bond };
                                batch = batch.zs_insert(&key, &height);
                            }
                        }
                        if slashed.contains(&action.arg32_2) {
                            open.insert(bond, (action.arg32_2, height));
                        }
                    }
                    BeginDelegationUnbonding => {
                        if let Some((finalizer, start)) = open.remove(&bond) {
                            if start.0 < height.0 {
                                let key = SlashedBondKey { finalizer, start, bond };
                                batch = batch.zs_insert(&key, &height);
                            }
                        }
                    }
                    // WithdrawDelegationBond: the run already closed at unbonding
                    // @Todo: Convert/Register/UpdateFinalizerKey
                    _ => {}
                }
            }
        }

        // Flush runs still open at the tip with the open-run sentinel end.
        for (bond, (finalizer, start)) in open {
            let key = SlashedBondKey { finalizer, start, bond };
            batch = batch.zs_insert(&key, &MAX_ON_DISK_HEIGHT);
        }

        batch
            .write_batch()
            .expect("writing the slash index batch should succeed");
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
}
