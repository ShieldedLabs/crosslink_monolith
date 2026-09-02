//! Delegation bond database access and write methods.

use std::collections::{BTreeMap, HashMap, HashSet};

use zebra_chain::{amount::Amount, block, block::Height};

use crate::{
    request::FinalizedBlock,
    service::finalized_state::{
        disk_db::{DiskDb, DiskWriteBatch, WriteDisk},
        disk_format::{AggregatedStakes, BondKey, BondStatus, DelegationBond, TransactionLocation},
        zebra_db::ZebraDb,
        TypedColumnFamily,
    },
    BoxError, FromDisk, IntoDisk,
};

/// The name of the delegation bond by key column family.
pub const DELEGATION_BOND_BY_KEY: &str = "delegation_bond_by_key";

/// The name of the bond status by key column family.
pub const BOND_STATUS_BY_KEY: &str = "bond_status_by_key";

/// The name of the aggregated stakes by hash column family.
pub const AGGREGATED_STAKES_BY_HASH: &str = "aggregated_stakes_by_hash";

/// The type for reading delegation bonds from the database.
pub type DelegationBondByKeyCf<'cf> = TypedColumnFamily<'cf, BondKey, DelegationBond>;

/// The type for reading bond status from the database.
pub type BondStatusByKeyCf<'cf> = TypedColumnFamily<'cf, BondKey, BondStatus>;

/// The type for reading aggregated stakes from the database.
pub type AggregatedStakesByHashCf<'cf> = TypedColumnFamily<'cf, block::Hash, AggregatedStakes>;

impl ZebraDb {
    // Column family convenience methods

    /// Returns a typed handle to the delegation bond by key column family.
    pub(crate) fn delegation_bond_by_key_cf(&self) -> DelegationBondByKeyCf<'_> {
        DelegationBondByKeyCf::new(&self.db, DELEGATION_BOND_BY_KEY)
            .expect("column family was created when database was created")
    }

    /// Returns a typed handle to the bond status by key column family.
    pub(crate) fn bond_status_by_key_cf(&self) -> BondStatusByKeyCf<'_> {
        BondStatusByKeyCf::new(&self.db, BOND_STATUS_BY_KEY)
            .expect("column family was created when database was created")
    }

    /// Returns a typed handle to the aggregated stakes by hash column family.
    pub(crate) fn aggregated_stakes_by_hash_cf(&self) -> AggregatedStakesByHashCf<'_> {
        AggregatedStakesByHashCf::new(&self.db, AGGREGATED_STAKES_BY_HASH)
            .expect("column family was created when database was created")
    }

    // Read delegation bond methods

    /// Returns the [`DelegationBond`] for a bond key, if it is in the finalized state.
    pub fn delegation_bond(&self, bond_key: &BondKey) -> Option<DelegationBond> {
        self.delegation_bond_by_key_cf().zs_get(bond_key)
    }

    /// Returns the [`BondStatus`] for a bond key, if it is in the finalized state.
    pub fn bond_status(&self, bond_key: &BondKey) -> Option<BondStatus> {
        self.bond_status_by_key_cf().zs_get(bond_key)
    }

    /// Returns the [`DelegationBond`] and [`BondStatus`] for a bond key,
    /// if it is in the finalized state.
    pub fn delegation_bond_with_status(
        &self,
        bond_key: &BondKey,
    ) -> Option<(DelegationBond, BondStatus)> {
        let bond = self.delegation_bond(bond_key)?;
        let status = self.bond_status(bond_key)?;
        Some((bond, status))
    }

    /// Returns true if a bond with the given key exists and is active.
    pub fn is_bond_active(&self, bond_key: &BondKey) -> bool {
        self.bond_status(bond_key)
            .map(|status| status.is_active())
            .unwrap_or(false)
    }

    /// Returns an iterator over all active bond keys and their data.
    ///
    /// This is used to:
    /// - Apply rewards in finalized state
    /// - Count active bonds when validating non-finalized blocks
    pub fn active_bonds(&self) -> impl Iterator<Item = (BondKey, DelegationBond)> + '_ {
        self.all_bonds()
            .filter(|(_key, _bond, status)| status.is_active())
            .map(|(key, bond, _status)| (key, bond))
    }

    /// Returns the count of active bonds in finalized state.
    pub fn active_bond_count(&self) -> usize {
        self.active_bonds().count()
    }

    /// Returns an iterator over all bond keys, their data, and their status.
    ///
    /// Used to initialize the non-finalized chain's delegation_bonds HashMap.
    pub fn all_bonds(&self) -> impl Iterator<Item = (BondKey, DelegationBond, BondStatus)> + '_ {
        let bond_status_cf = self.bond_status_by_key_cf();
        let delegation_bond_cf = self.delegation_bond_by_key_cf();

        bond_status_cf
            .zs_items_in_range_ordered(..)
            .into_iter()
            .filter_map(move |(key, status)| {
                let bond = delegation_bond_cf.zs_get(&key)?;
                Some((key, bond, status))
            })
    }

    /// Returns the aggregated stakes snapshot for a finalized block hash, if available.
    pub fn aggregated_stakes(&self, hash: &block::Hash) -> Option<Vec<([u8; 32], u64)>> {
        self.aggregated_stakes_by_hash_cf()
            .zs_get(hash)
            .map(|a| a.0)
    }

}

/// The last value a block's batch writes per bond key, per bond column family.
///
/// [`DiskWriteBatch::prepare_aggregated_stakes_batch`] lays these over the database's
/// bond set to see the post-block bond state before the batch is written. The snapshot
/// must ride in the same batch as the block itself: written separately, a process death
/// between the two writes leaves a committed block with no stakes row, permanently.
#[derive(Default)]
pub struct BondBatchOverlay {
    bonds: HashMap<BondKey, DelegationBond>,
    statuses: HashMap<BondKey, BondStatus>,
}

impl DiskWriteBatch {
    /// Prepare the delegation bond writes for `finalized.block` into this batch, and
    /// return the overlay of those writes for the stakes snapshot.
    ///
    /// # Errors
    ///
    /// - Returns an error if any delegation bond operation is invalid.
    pub fn prepare_delegation_bonds_batch(
        &mut self,
        db: &ZebraDb,
        finalized: &FinalizedBlock,
        height: &Height,
    ) -> Result<BondBatchOverlay, BoxError> {
        // Process transactions to update bond state
        use zcash_primitives::transaction::StakingActionKind;

        // Also serves the in-block role of `bonds_created_in_block`: rewards and
        // retargets must see bonds created or modified earlier in this same block,
        // which aren't in the DB yet.
        let mut overlay = BondBatchOverlay::default();

        // Iterate through all transactions in the block
        for (transaction_index, transaction) in finalized.block.transactions.iter().enumerate() {
            // Check if transaction has a staking action
            if let Some(staking_action) = transaction.staking_action() {
                let bond_key = staking_action.bond_key();
                let transaction_location =
                    TransactionLocation::from_usize(*height, transaction_index);

                match staking_action.kind() {
                    StakingActionKind::CreateNewDelegationBond => {
                        // Extract bond data
                        let amount = Amount::try_from(staking_action.amount_zats())?;
                        let target_finalizer = staking_action.target_finalizer_pk();

                        let bond =
                            DelegationBond::new(amount, target_finalizer, transaction_location);

                        // Insert new bond
                        self.prepare_new_delegation_bond(&db.db, bond_key, bond, &mut overlay);
                    }
                    StakingActionKind::BeginDelegationUnbonding => {
                        // Mark bond as unbonding
                        self.prepare_unbonding_delegation_bond(&db.db, db, bond_key, transaction_location, &mut overlay)?;
                    }
                    StakingActionKind::WithdrawDelegationBond => {
                        // Mark bond as withdrawn
                        self.prepare_withdrawn_delegation_bond(&db.db, db, bond_key, transaction_location, &mut overlay)?;
                    }
                    StakingActionKind::RetargetDelegationBond => {
                        // Update the bond's target_finalizer
                        let new_target = staking_action.target_finalizer_pk();
                        self.prepare_retarget_delegation_bond(
                            &db.db,
                            db,
                            bond_key,
                            new_target,
                            &mut overlay,
                        )?;
                    }
                    // Other staking actions don't affect delegation bonds
                    _ => {}
                }
            }
        }

        // Apply bond rewards accumulated in the non-finalized state
        let delegation_bond_by_key_cf = db.db.cf_handle(DELEGATION_BOND_BY_KEY).unwrap();
        for (bond_key, reward_amount) in &finalized.bond_rewards {
            // Get current bond from bonds modified in this block first, then fall back to DB.
            // This ensures we use the updated bond if it was retargeted/created in this block.
            let bond_opt = overlay.bonds.get(bond_key).cloned()
                .or_else(|| db.delegation_bond(bond_key));

            if let Some(mut bond) = bond_opt {
                // Add reward to bond amount
                bond.amount = (bond.amount + Amount::try_from(*reward_amount as i64)?)?;
                // Write updated bond back
                overlay.bonds.insert(*bond_key, bond.clone());
                self.zs_insert(&delegation_bond_by_key_cf, *bond_key, bond);
            }
        }

        // Persist slash burns applied to this block in the non-finalized state.
        // Deliberately after the reward write above (decided 2026-07-02): a bond
        // burned at activation height A still receives A's reward before its status
        // flips to Burned. The value is permanently locked either way; keeping
        // issuance uniform avoids a special case in the reward path. The
        // non-finalized push orders it the same way (rewards in push, burns after).
        let bond_status_by_key_cf = db.db.cf_handle(BOND_STATUS_BY_KEY).unwrap();
        for bond_key in &finalized.bond_burns {
            overlay.statuses.insert(*bond_key, BondStatus::Burned);
            self.zs_insert(&bond_status_by_key_cf, *bond_key, BondStatus::Burned);
        }

        Ok(overlay)
    }

    /// Prepare the aggregated-stakes snapshot for `hash` into this batch: the total
    /// active bond amount per finalizer as of this block, i.e. the database's bond set
    /// with this batch's own bond writes (`overlay`) applied on top.
    pub fn prepare_aggregated_stakes_batch(
        &mut self,
        db: &ZebraDb,
        hash: block::Hash,
        overlay: &BondBatchOverlay,
    ) {
        let mut stakes_by_finalizer: HashMap<[u8; 32], u64> = HashMap::new();

        let mut unseen: HashSet<BondKey> = overlay
            .bonds
            .keys()
            .chain(overlay.statuses.keys())
            .copied()
            .collect();

        for (key, bond, status) in db.all_bonds() {
            unseen.remove(&key);
            let status = overlay.statuses.get(&key).unwrap_or(&status);
            if status.is_active() {
                let bond = overlay.bonds.get(&key).unwrap_or(&bond);
                let amount: u64 = bond.amount.into();
                *stakes_by_finalizer.entry(bond.target_finalizer).or_insert(0) += amount;
            }
        }

        // Keys this batch touches that `all_bonds` didn't yield, in particular bonds
        // created in this block.
        for key in unseen {
            let Some(status) = overlay
                .statuses
                .get(&key)
                .cloned()
                .or_else(|| db.bond_status(&key))
            else {
                continue;
            };
            if !status.is_active() {
                continue;
            }
            let Some(bond) = overlay
                .bonds
                .get(&key)
                .cloned()
                .or_else(|| db.delegation_bond(&key))
            else {
                continue;
            };
            let amount: u64 = bond.amount.into();
            *stakes_by_finalizer.entry(bond.target_finalizer).or_insert(0) += amount;
        }

        let aggregated: Vec<([u8; 32], u64)> = stakes_by_finalizer.into_iter().collect();
        let cf = db.db.cf_handle(AGGREGATED_STAKES_BY_HASH).unwrap();
        self.zs_insert(&cf, hash, AggregatedStakes(aggregated));
    }

    /// Insert a new delegation bond into the database batch.
    ///
    /// Inserts into:
    /// - `delegation_bond_by_key`: stores the bond data
    /// - `bond_status_by_key`: stores Active status
    fn prepare_new_delegation_bond(
        &mut self,
        db: &DiskDb,
        bond_key: BondKey,
        bond: DelegationBond,
        overlay: &mut BondBatchOverlay,
    ) {
        let delegation_bond_by_key_cf = db.cf_handle(DELEGATION_BOND_BY_KEY).unwrap();
        let bond_status_by_key_cf = db.cf_handle(BOND_STATUS_BY_KEY).unwrap();

        // Insert bond data
        overlay.bonds.insert(bond_key, bond.clone());
        self.zs_insert(&delegation_bond_by_key_cf, bond_key, bond);

        // Insert active status
        overlay.statuses.insert(bond_key, BondStatus::Active);
        self.zs_insert(&bond_status_by_key_cf, bond_key, BondStatus::Active);
    }

    /// Mark a delegation bond as unbonding in the database batch.
    ///
    /// Updates `bond_status_by_key` to Unbonding status.
    ///
    /// # Errors
    ///
    /// Returns an error if the bond doesn't exist or is not active.
    fn prepare_unbonding_delegation_bond(
        &mut self,
        disk_db: &DiskDb,
        zebra_db: &ZebraDb,
        bond_key: BondKey,
        transaction_location: TransactionLocation,
        overlay: &mut BondBatchOverlay,
    ) -> Result<(), BoxError> {
        let bond_status_by_key_cf = disk_db.cf_handle(BOND_STATUS_BY_KEY).unwrap();

        // Verify bond exists and is active
        let current_status = zebra_db
            .bond_status(&bond_key)
            .ok_or_else(|| format!("bond {:?} not found", bond_key))?;

        if !current_status.is_active() {
            return Err(format!("bond {:?} is not active", bond_key).into());
        }

        // Update status to unbonding
        let status = BondStatus::Unbonding {
            unbonded_at: transaction_location,
        };
        overlay.statuses.insert(bond_key, status.clone());
        self.zs_insert(&bond_status_by_key_cf, bond_key, status);

        Ok(())
    }

    /// Mark a delegation bond as withdrawn in the database batch.
    ///
    /// Updates `bond_status_by_key` to Withdrawn status.
    ///
    /// # Errors
    ///
    /// Returns an error if the bond doesn't exist or is not unbonding.
    fn prepare_withdrawn_delegation_bond(
        &mut self,
        disk_db: &DiskDb,
        zebra_db: &ZebraDb,
        bond_key: BondKey,
        transaction_location: TransactionLocation,
        overlay: &mut BondBatchOverlay,
    ) -> Result<(), BoxError> {
        let bond_status_by_key_cf = disk_db.cf_handle(BOND_STATUS_BY_KEY).unwrap();

        // Verify bond exists and is unbonding
        let current_status = zebra_db
            .bond_status(&bond_key)
            .ok_or_else(|| format!("bond {:?} not found", bond_key))?;

        if !current_status.is_unbonding() {
            return Err(format!("bond {:?} is not unbonding", bond_key).into());
        }

        // Update status to withdrawn
        let status = BondStatus::Withdrawn {
            withdrawn_at: transaction_location,
        };
        overlay.statuses.insert(bond_key, status.clone());
        self.zs_insert(&bond_status_by_key_cf, bond_key, status);

        Ok(())
    }

    /// Retarget a delegation bond to a new finalizer in the database batch.
    ///
    /// Updates only the bond's `target_finalizer` field (not `created_at` or `amount`).
    ///
    /// # Errors
    ///
    /// Returns an error if the bond doesn't exist.
    fn prepare_retarget_delegation_bond(
        &mut self,
        disk_db: &DiskDb,
        zebra_db: &ZebraDb,
        bond_key: BondKey,
        new_target: [u8; 32],
        overlay: &mut BondBatchOverlay,
    ) -> Result<(), BoxError> {
        let delegation_bond_by_key_cf = disk_db.cf_handle(DELEGATION_BOND_BY_KEY).unwrap();

        // Get current bond from bonds modified in this block first, then fall back to DB.
        // This ensures we use the updated bond if it was modified earlier in this block.
        let bond_opt = overlay.bonds.get(&bond_key).cloned()
            .or_else(|| zebra_db.delegation_bond(&bond_key));

        let mut bond = bond_opt.ok_or_else(|| format!("bond {:?} not found for retarget", bond_key))?;

        // Update only target_finalizer (not created_at or amount)
        bond.target_finalizer = new_target;

        // Always update the overlay so subsequent operations in this block
        // (e.g., rewards application) see the updated bond instead of the stale DB value
        overlay.bonds.insert(bond_key, bond.clone());

        // Write updated bond
        self.zs_insert(&delegation_bond_by_key_cf, bond_key, bond);

        Ok(())
    }
}
