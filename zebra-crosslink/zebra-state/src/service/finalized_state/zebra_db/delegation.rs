//! Delegation bond database access and write methods.

use std::collections::{HashMap, HashSet};

use zcash_primitives::{bft::FinalizerAddress, transaction::StakingAction};
use zebra_chain::{amount::{Amount, NonNegative}, block, block::Height};

use crate::{
    request::FinalizedBlock,
    service::finalized_state::{
        disk_db::{DiskDb, DiskWriteBatch, WriteDisk},
        disk_format::{AggregatedStakes, BondKey, BondStatus, DelegationBond, TransactionLocation},
        zebra_db::ZebraDb,
        TypedColumnFamily,
    },
    BoxError,
};

/// The name of the delegation bond by key column family.
pub const DELEGATION_BOND_BY_KEY: &str = "delegation_bond_by_key";

/// The name of the bond status by key column family.
pub const BOND_STATUS_BY_KEY: &str = "bond_status_by_key";

/// The name of the aggregated stakes by hash column family.
pub const AGGREGATED_STAKES_BY_HASH: &str = "aggregated_stakes_by_hash";

/// The name of the finalizer reward bank column family: finalizer key -> unconverted
/// commission. Only keys that have ever earned commission have a row.
pub const FINALIZER_REWARD_BY_KEY: &str = "finalizer_reward_by_key";

/// The type for reading delegation bonds from the database.
pub type DelegationBondByKeyCf<'cf> = TypedColumnFamily<'cf, BondKey, DelegationBond>;

/// The type for reading bond status from the database.
pub type BondStatusByKeyCf<'cf> = TypedColumnFamily<'cf, BondKey, BondStatus>;

/// The type for reading aggregated stakes from the database.
pub type AggregatedStakesByHashCf<'cf> = TypedColumnFamily<'cf, block::Hash, AggregatedStakes>;

/// The type for reading finalizer reward banks from the database.
pub type FinalizerRewardByKeyCf<'cf> = TypedColumnFamily<'cf, [u8; 32], Amount<NonNegative>>;

/// The name of the finalizer address column family: finalizer public key -> the first verified
/// [`FinalizerAddress`] a committed staking action carried for it.
///
/// A raw public key cannot be turned into an address: the address embeds the finalizer key's
/// signature, which only the key holder can make. So the only honest source of an address is
/// one the chain already carried, in a `CreateNewDelegationBond` target or either end of a
/// `RetargetDelegationBond`. Any verified address for a key is equally valid (the signed message
/// is fixed), so the first one seen is kept and rows are never rewritten or deleted.
pub const FINALIZER_ADDRESS_BY_KEY: &str = "finalizer_address_by_key";

/// The type for reading finalizer addresses from the database.
pub type FinalizerAddressByKeyCf<'cf> = TypedColumnFamily<'cf, [u8; 32], FinalizerAddress>;

/// The finalizer addresses a staking action carries: Create's target, and Retarget's
/// destination and source. Consensus verifies every one of them, but they are unverified data
/// here; callers verify before trusting one.
pub(crate) fn staking_action_finalizer_addresses(
    action: &StakingAction,
) -> [Option<FinalizerAddress>; 2] {
    [action.target_finalizer_address(), action.from_finalizer_address()]
}

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

    /// Returns a typed handle to the finalizer reward by key column family.
    pub(crate) fn finalizer_reward_by_key_cf(&self) -> FinalizerRewardByKeyCf<'_> {
        FinalizerRewardByKeyCf::new(&self.db, FINALIZER_REWARD_BY_KEY)
            .expect("column family was created when database was created")
    }

    /// Returns a typed handle to the finalizer address by key column family.
    pub(crate) fn finalizer_address_by_key_cf(&self) -> FinalizerAddressByKeyCf<'_> {
        FinalizerAddressByKeyCf::new(&self.db, FINALIZER_ADDRESS_BY_KEY)
            .expect("column family was created when database was created")
    }

    /// The verified [`FinalizerAddress`] for a finalizer public key, if a finalized staking
    /// action has carried one. `None` means no finalized block has revealed this key's address;
    /// it can never be derived from the key alone.
    pub fn finalizer_address(&self, finalizer: &[u8; 32]) -> Option<FinalizerAddress> {
        self.finalizer_address_by_key_cf().zs_get(finalizer)
    }

    /// Every indexed finalizer address, in key order. Used by format checks and tests.
    pub fn all_finalizer_addresses(&self) -> Vec<([u8; 32], FinalizerAddress)> {
        self.finalizer_address_by_key_cf()
            .zs_items_in_range_ordered(..)
            .into_iter()
            .collect()
    }

    /// A finalizer's unconverted commission in the finalized state; zero if it has none.
    pub fn finalizer_reward(&self, finalizer: &[u8; 32]) -> u64 {
        self.finalizer_reward_by_key_cf()
            .zs_get(finalizer)
            .map(|a| a.into())
            .unwrap_or(0)
    }

    /// Every finalizer reward bank, used to seed a non-finalized chain.
    pub fn all_finalizer_rewards(&self) -> Vec<([u8; 32], u64)> {
        self.finalizer_reward_by_key_cf()
            .zs_items_in_range_ordered(..)
            .into_iter()
            .map(|(key, amount)| (key, amount.into()))
            .collect()
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


/// Sum active bonds by target finalizer, then add each finalizer's own reward bank (a virtual
/// bond on itself).
///
/// The one definition of the validator set at a block. The finalized path builds its inputs from
/// the database plus the committing batch's own writes; `Chain` builds them from the bond state
/// it carries along its own branch. Both add up here, so a roster read answers the same on
/// either side of the finalized tip (FINALITY.md §8.1).
pub(crate) fn aggregate_stakes(
    active_bonds: impl IntoIterator<Item = ([u8; 32], u64)>,
    banks: impl IntoIterator<Item = ([u8; 32], u64)>,
) -> Vec<([u8; 32], u64)> {
    let mut stakes_by_finalizer: HashMap<[u8; 32], u64> = HashMap::new();
    for (finalizer, amount) in active_bonds {
        *stakes_by_finalizer.entry(finalizer).or_insert(0) += amount;
    }
    for (finalizer, bank) in banks {
        if bank != 0 {
            *stakes_by_finalizer.entry(finalizer).or_insert(0) += bank;
        }
    }
    stakes_by_finalizer.into_iter().collect()
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
    /// Post-block bank balance of every finalizer this block touched.
    banks: HashMap<[u8; 32], u64>,
}

impl DiskWriteBatch {
    /// Index the finalizer addresses carried by `actions` (one finalized block's staking
    /// actions, in block order) into [`FINALIZER_ADDRESS_BY_KEY`].
    ///
    /// Keeps the first verified address per key: keys already in the database, or already
    /// indexed earlier in `actions`, are skipped. Every address is verified before it is
    /// written, so checkpoint-verified blocks (which skip contextual staking checks) can never
    /// plant a forged address.
    pub fn prepare_finalizer_addresses_batch<'a>(
        &mut self,
        db: &ZebraDb,
        actions: impl IntoIterator<Item = &'a StakingAction>,
    ) {
        let finalizer_address_by_key_cf = db.db.cf_handle(FINALIZER_ADDRESS_BY_KEY).unwrap();
        let mut indexed: HashSet<[u8; 32]> = HashSet::new();

        for address in actions
            .into_iter()
            .flat_map(staking_action_finalizer_addresses)
            .flatten()
        {
            let key = address.pub_key.0;
            if indexed.contains(&key) || db.finalizer_address(&key).is_some() {
                continue;
            }
            if !address.verify() {
                continue;
            }
            indexed.insert(key);
            self.zs_insert(&finalizer_address_by_key_cf, key, address);
        }
    }

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
        // Finalizer banks this block changes (Convert debits, commission credits),
        // read once from the db and written once at the end.
        let mut banks: HashMap<[u8; 32], u64> = HashMap::new();

        self.prepare_finalizer_addresses_batch(
            db,
            finalized.block.transactions.iter().filter_map(|tx| tx.staking_action()),
        );

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
                    StakingActionKind::ConvertFinalizerRewardToDelegationBond => {
                        let finalizer = staking_action.target_finalizer_pk();
                        let amount_zats = staking_action.amount_zats();
                        let bank = banks.entry(finalizer).or_insert_with(|| db.finalizer_reward(&finalizer));
                        *bank = bank.checked_sub(amount_zats).ok_or_else(|| {
                            format!("finalizer {:?} bank cannot cover conversion of {amount_zats}", finalizer)
                        })?;

                        let bond = DelegationBond::new(Amount::try_from(amount_zats)?, finalizer, transaction_location);
                        self.prepare_new_delegation_bond(&db.db, bond_key, bond, &mut overlay);
                    }
                    StakingActionKind::Null => {}
                }

                if staking_action.spends().is_some() {
                    let mut bond = overlay.bonds.get(&bond_key).cloned()
                        .or_else(|| db.delegation_bond(&bond_key))
                        .ok_or_else(|| format!("bond {:?} not found for a spent version", bond_key))?;
                    bond.version = Some(transaction.hash().0);
                    overlay.bonds.insert(bond_key, bond);
                    let delegation_bond_by_key_cf = db.db.cf_handle(DELEGATION_BOND_BY_KEY).unwrap();
                    self.zs_insert(&delegation_bond_by_key_cf, bond_key, bond);
                }
            }
        }

        // Credit this block's commissions, then write every bank this block touched.
        for (finalizer, commission) in &finalized.finalizer_rewards {
            let bank = banks.entry(*finalizer).or_insert_with(|| db.finalizer_reward(finalizer));
            *bank += commission;
        }
        let finalizer_reward_by_key_cf = db.db.cf_handle(FINALIZER_REWARD_BY_KEY).unwrap();
        for (finalizer, bank) in &banks {
            self.zs_insert(&finalizer_reward_by_key_cf, *finalizer, Amount::<NonNegative>::try_from(*bank)?);
        }
        overlay.banks = banks;

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

        // Persist slash burns applied to this block in the non-finalized state. The
        // burns land before the block's staking actions and reward, but no action or
        // reward above touches a burned bond, so writing them last is equivalent.
        let bond_status_by_key_cf = db.db.cf_handle(BOND_STATUS_BY_KEY).unwrap();
        for bond_key in &finalized.bond_burns {
            overlay.statuses.insert(*bond_key, BondStatus::Burned);
            self.zs_insert(&bond_status_by_key_cf, *bond_key, BondStatus::Burned);
        }

        Ok(overlay)
    }

    /// Prepare the aggregated-stakes snapshot for `hash` into this batch: the total
    /// active bond amount per finalizer as of this block, plus each finalizer's own
    /// reward bank (a virtual bond on itself), i.e. the database's bond and bank sets
    /// with this batch's own writes (`overlay`) applied on top.
    pub fn prepare_aggregated_stakes_batch(
        &mut self,
        db: &ZebraDb,
        hash: block::Hash,
        overlay: &BondBatchOverlay,
    ) {
        let mut active_bonds: Vec<([u8; 32], u64)> = Vec::new();
        let mut banks: Vec<([u8; 32], u64)> = Vec::new();

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
                active_bonds.push((bond.target_finalizer, amount));
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
            active_bonds.push((bond.target_finalizer, amount));
        }

        for (finalizer, bank) in db.all_finalizer_rewards() {
            let bank = overlay.banks.get(&finalizer).copied().unwrap_or(bank);
            banks.push((finalizer, bank));
        }
        for (finalizer, bank) in &overlay.banks {
            // banks first credited in this block have no db row yet
            if !db.finalizer_reward_by_key_cf().zs_contains(finalizer) {
                banks.push((*finalizer, *bank));
            }
        }

        let aggregated = aggregate_stakes(active_bonds, banks);
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

#[cfg(test)]
mod finalizer_address_tests {
    use zcash_primitives::{
        bft::{finalizer_key_from_seed, FinalizerAddress, TMSig},
        transaction::StakingAction,
    };
    use zebra_chain::parameters::Network;

    use crate::{
        service::finalized_state::{DiskWriteBatch, FinalizedState, FromDisk, IntoDisk},
        Config,
    };

    fn address(seed: &[u8]) -> FinalizerAddress {
        let (key, _) = finalizer_key_from_seed(seed);
        FinalizerAddress::create(&key)
    }

    fn forged(address: FinalizerAddress) -> FinalizerAddress {
        FinalizerAddress { sig: TMSig([9; 64]), ..address }
    }

    fn create(target_finalizer: FinalizerAddress) -> StakingAction {
        StakingAction::CreateNewDelegationBond {
            amount_zats: 1000,
            unique_pubkey: [1; 32],
            bond_salt: [2; 32],
            target_finalizer,
            signature: [0; 64],
        }
    }

    fn retarget(from_finalizer: FinalizerAddress, to_finalizer: FinalizerAddress) -> StakingAction {
        StakingAction::RetargetDelegationBond {
            unique_pubkey: [1; 32],
            signature: [0; 64],
            from_finalizer,
            to_finalizer,
            spends: None,
        }
    }

    fn finalized_state() -> FinalizedState {
        FinalizedState::new(
            &Config::ephemeral(),
            &Network::Mainnet,
            #[cfg(feature = "elasticsearch")]
            false,
        )
        .expect("opening an ephemeral database should succeed")
    }

    #[test]
    fn finalizer_address_round_trips_through_disk_format() {
        let a = address(b"disk");
        let bytes = a.as_bytes();
        assert_eq!(&bytes[..32], &a.pub_key.0);
        assert_eq!(&bytes[32..], &a.sig.0);
        assert_eq!(FinalizerAddress::from_bytes(bytes), a);
    }

    #[test]
    fn committed_create_and_retarget_addresses_are_indexed_once_verified() {
        let state = finalized_state();
        let db = &state.db;
        let (a, b, c) = (address(b"a"), address(b"b"), address(b"c"));

        // A forged address ahead of the real one must not claim the key or block it.
        let mut batch = DiskWriteBatch::new();
        batch.prepare_finalizer_addresses_batch(
            db,
            &[create(forged(a)), create(a), retarget(b, forged(c))],
        );
        db.write_batch(batch).unwrap();

        assert_eq!(db.finalizer_address(&a.pub_key.0), Some(a));
        assert_eq!(db.finalizer_address(&b.pub_key.0), Some(b));
        assert_eq!(db.finalizer_address(&c.pub_key.0), None);

        // A later block reveals `c` as a retarget destination; the stored `a` is never replaced.
        let mut batch = DiskWriteBatch::new();
        batch.prepare_finalizer_addresses_batch(db, &[retarget(forged(a), c)]);
        db.write_batch(batch).unwrap();

        assert_eq!(db.finalizer_address(&a.pub_key.0), Some(a));
        assert_eq!(db.finalizer_address(&c.pub_key.0), Some(c));
        assert_eq!(db.all_finalizer_addresses().len(), 3);
    }

    #[test]
    fn staking_actions_without_addresses_index_nothing() {
        let state = finalized_state();
        let db = &state.db;

        let mut batch = DiskWriteBatch::new();
        batch.prepare_finalizer_addresses_batch(
            db,
            &[
                StakingAction::BeginDelegationUnbonding { unique_pubkey: [1; 32], signature: [0; 64], spends: None },
                StakingAction::ConvertFinalizerRewardToDelegationBond {
                    this_finalizer: address(b"raw").pub_key.0,
                    amount_zats: 1,
                    unique_pubkey: [3; 32],
                    bond_salt: [4; 32],
                    finalizer_signature: [0; 64],
                    signature: [0; 64],
                },
            ],
        );
        db.write_batch(batch).unwrap();

        // A raw finalizer key (Convert's `this_finalizer`) is never turned into an address.
        assert!(db.all_finalizer_addresses().is_empty());
    }
}
