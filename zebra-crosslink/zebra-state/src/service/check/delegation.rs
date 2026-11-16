//! Delegation bond validation for contextual checks.

use std::collections::{HashMap, HashSet};

use crate::{
    service::{
        finalized_state::{disk_format::DelegationBond, ZebraDb},
        non_finalized_state::Chain,
    },
    SemanticallyVerifiedBlock, ValidateContextError,
};

/// Validate all delegation bond operations in a [`SemanticallyVerifiedBlock`].
/// If any of the operations are invalid, return an error.
///
/// This function:
/// - Validates that CreateNewDelegationBond doesn't create duplicate bonds
/// - Validates that BeginDelegationUnbonding references existing active bonds created at least
///   the network's staking action delay earlier
/// - Validates that WithdrawDelegationBond references existing unbonding bonds unbonded at least
///   the network's staking action delay earlier
/// - Validates that RetargetDelegationBond references existing active bonds
pub fn validate_delegation_bonds(
    semantically_verified: &SemanticallyVerifiedBlock,
    non_finalized_chain: &Chain,
    finalized_state: &ZebraDb,
) -> Result<(), ValidateContextError> {
    let mut in_block = InBlockBonds::default();
    for (transaction, transaction_hash) in semantically_verified.block.transactions.iter().zip(semantically_verified.transaction_hashes.iter()) {
        if let Some(staking_action) = transaction.staking_action() {
            in_block.apply(staking_action, *transaction_hash, semantically_verified.height, non_finalized_chain, finalized_state)?;
        }
    }

    Ok(())
}

/// Applies `staking_actions` in order, as the block at `height` on `non_finalized_chain` would,
/// and returns the indices of the ones that block would be rejected for. A rejected action is
/// left out, so the ones after it are checked as if it were not there.
///
/// This is how a block template keeps only actions its block will accept: the rules are the
/// ones [`validate_delegation_bonds`] applies, not a second copy that can drift from them.
pub fn invalid_staking_actions(
    staking_actions: &[(zcash_primitives::transaction::StakingAction, zebra_chain::transaction::Hash)],
    height: zebra_chain::block::Height,
    non_finalized_chain: &Chain,
    finalized_state: &ZebraDb,
) -> Vec<usize> {
    let mut in_block = InBlockBonds::default();
    let mut invalid = Vec::new();
    for (i, (staking_action, transaction_hash)) in staking_actions.iter().enumerate() {
        if in_block.apply(staking_action, *transaction_hash, height, non_finalized_chain, finalized_state).is_err() {
            invalid.push(i);
        }
    }
    invalid
}

/// What the staking actions earlier in a block have done, layered over the chain the block
/// extends. `apply` validates an action against it and then records the action, and leaves it
/// unchanged when the action is rejected.
#[derive(Default)]
struct InBlockBonds {
    new_bonds: HashMap<[u8; 32], DelegationBond>,
    // bonds that unbonded or withdrew earlier in the block; see `apply`
    unbonded_or_withdrawn: HashSet<[u8; 32]>,
    // bond_key -> target after in-block retargets, so a second retarget in the
    // same block validates its `from` against the first one's `to`
    retargets: HashMap<[u8; 32], [u8; 32]>,
    // finalizer -> bank balance after in-block conversions; the block's own
    // commission is paid after its transactions, so it never funds them
    banks: HashMap<[u8; 32], u64>,
    // bond_key -> version after in-block creates and spends, so an action on a
    // bond created or spent earlier in the block must name that transaction
    versions: HashMap<[u8; 32], [u8; 32]>,
}

/// The current version of `bond`, which `chain` holds if it is given and the finalized state
/// holds otherwise: the txid of the last action that spent it, or of the transaction that
/// created it.
pub(crate) fn bond_version(bond: &DelegationBond, chain: Option<&Chain>, finalized_state: &ZebraDb) -> [u8; 32] {
    bond.current_version(|created_at| {
        chain
            .and_then(|chain| chain.blocks.get(&created_at.height))
            .and_then(|block| block.transaction_hashes.get(created_at.index.as_usize()).copied())
            .or_else(|| finalized_state.transaction_hash(created_at))
            .expect("a bond's creating transaction is in the chain that holds the bond")
            .0
    })
}

impl InBlockBonds {
    /// The bond version an action on `bond_key` must spend, after this block's earlier actions,
    /// or `None` if there is no such bond.
    fn current_version(&self, bond_key: [u8; 32], non_finalized_chain: &Chain, finalized_state: &ZebraDb) -> Option<[u8; 32]> {
        if let Some(version) = self.versions.get(&bond_key) {
            return Some(*version);
        }
        if let Some((bond, _status)) = non_finalized_chain.delegation_bonds.get(&bond_key) {
            return Some(bond_version(bond, Some(non_finalized_chain), finalized_state));
        }
        finalized_state
            .delegation_bond(&bond_key)
            .map(|bond| bond_version(&bond, Some(non_finalized_chain), finalized_state))
    }

    fn apply(
        &mut self,
        staking_action: &zcash_primitives::transaction::StakingAction,
        transaction_hash: zebra_chain::transaction::Hash,
        height: zebra_chain::block::Height,
        non_finalized_chain: &Chain,
        finalized_state: &ZebraDb,
    ) -> Result<(), ValidateContextError> {
        use zcash_primitives::transaction::StakingActionKind;
        use zebra_chain::parameters::NetworkUpgrade;

        let bond_key = staking_action.bond_key();

        // From FeaturenetUpgrade3 a bond is a single unspent output, and an action on it must
        // spend its current version. Before, the spending form is refused, so a chain that never
        // activates the upgrade validates as it always has.
        let spends_versions = NetworkUpgrade::FeaturenetUpgrade3
            .activation_height(&non_finalized_chain.network())
            .is_some_and(|activation_height| height >= activation_height);
        if staking_action.acts_on_existing_bond() {
            match (spends_versions, staking_action.spends()) {
                (true, None) => {
                    return Err(ValidateContextError::InvalidDelegationBond(format!(
                        "from FeaturenetUpgrade3 an action on a bond must spend its version: {:?}",
                        bond_key
                    )));
                }
                (false, Some(_)) => {
                    return Err(ValidateContextError::InvalidDelegationBond(format!(
                        "an action cannot spend a bond version before FeaturenetUpgrade3: {:?}",
                        bond_key
                    )));
                }
                (true, Some(spends)) => match self.current_version(bond_key, non_finalized_chain, finalized_state) {
                    Some(current_version) if current_version == spends => {}
                    Some(current_version) => {
                        return Err(ValidateContextError::InvalidDelegationBond(format!(
                            "action spends bond version {:?}, but the bond's current version is {:?}: {:?}",
                            spends, current_version, bond_key
                        )));
                    }
                    None => {
                        return Err(ValidateContextError::InvalidDelegationBond(format!(
                            "action spends bond version {:?} of a bond that does not exist: {:?}",
                            spends, bond_key
                        )));
                    }
                },
                (false, None) => {}
            }
        }

        // Once a bond unbonds or withdraws, no later action on it in the same block can be valid:
        // retarget and unbond need an active bond, withdrawal needs STAKING_ACTION_DELAY blocks
        // after the unbond, and the key already exists for a create. The per-kind checks below
        // read the chain, which doesn't show this block's earlier actions, so this is checked once
        // for every kind. Without it, retarget after unbond was accepted (STAKING_AUDIT S3), and a
        // second withdrawal of the same bond was accepted and then panicked the commit, which
        // expects every withdrawal to find its bond unbonding.
        if self.unbonded_or_withdrawn.contains(&bond_key) {
            return Err(ValidateContextError::InvalidDelegationBond(format!(
                "delegation bond already unbonded or withdrawn earlier in this block: {:?}",
                bond_key
            )));
        }

        // The target finalizer is a capability: the address embeds the
        // finalizer key's signature over the standard message, and an action
        // whose signature doesn't verify is consensus-invalid. This is what
        // stops stake being pointed at a key nobody controls, and from
        // FeaturenetUpgrade3 at a small-order key everybody can sign for.
        // Retarget's `from_finalizer` must verify too.
        for addr in [staking_action.target_finalizer_address(), staking_action.from_finalizer_address()] {
            if let Some(addr) = addr {
                if !addr.verify_at(&non_finalized_chain.network(), height.0) {
                    return Err(ValidateContextError::InvalidDelegationBond(format!(
                        "invalid finalizer address capability: {:?}",
                        addr.pub_key
                    )));
                }
            }
        }

        // The transaction verifier checks this too, but against the height the transaction
        // entered the mempool at. A template built later, past the activation height, relies on
        // this one to leave the transaction out.
        if !staking_action.amount_is_valid_at(&non_finalized_chain.network(), height.0) {
            return Err(ValidateContextError::InvalidDelegationBond(format!(
                "bond amount {} is not a power of 10 zats: {:?}",
                staking_action.amount_zats(), bond_key
            )));
        }

        match staking_action.kind() {
            StakingActionKind::CreateNewDelegationBond => {
                // Check that bond doesn't already exist
                validate_create_new_bond(
                    bond_key,
                    &self.new_bonds,
                    non_finalized_chain,
                    finalized_state,
                )?;

                // Track this new bond for subsequent validation in this block
                let amount =
                    zebra_chain::amount::Amount::try_from(staking_action.amount_zats())
                        .map_err(|e| {
                            ValidateContextError::InvalidDelegationBond(format!(
                                "invalid bond amount: {:?}",
                                e
                            ))
                        })?;
                let target_finalizer = staking_action.target_finalizer_pk();
                let bond = DelegationBond::new(
                    amount,
                    target_finalizer,
                    crate::service::finalized_state::disk_format::TransactionLocation::from_usize(
                        height,
                        0,
                    ),
                );
                self.new_bonds.insert(bond_key, bond);
                self.versions.insert(bond_key, transaction_hash.0);
            }
            StakingActionKind::BeginDelegationUnbonding => {
                // Check that bond exists and is active
                validate_bond_for_unbonding(
                    bond_key,
                    height,
                    &self.new_bonds,
                    non_finalized_chain,
                    finalized_state,
                )?;

                self.unbonded_or_withdrawn.insert(bond_key);
            }
            StakingActionKind::WithdrawDelegationBond => {
                // Check that bond exists, is unbonding, and withdrawal amount matches bond amount
                validate_bond_for_withdrawal(
                    bond_key,
                    height,
                    staking_action.amount_zats(),
                    non_finalized_chain,
                )?;

                self.unbonded_or_withdrawn.insert(bond_key);
            }
            StakingActionKind::RetargetDelegationBond => {
                // Check that bond exists and is active
                validate_bond_for_retarget(
                    bond_key,
                    &self.new_bonds,
                    non_finalized_chain,
                    finalized_state,
                )?;

                // `from_finalizer` must name the bond's actual current
                // target (in-block retargets and creates included). This
                // keeps every bond's target derivable by replaying
                // transactions forward or backward.
                let from_pk = staking_action
                    .from_finalizer_address()
                    .expect("retarget carries from_finalizer")
                    .pub_key
                    .0;
                let current_target = self.retargets
                    .get(&bond_key)
                    .copied()
                    .or_else(|| self.new_bonds.get(&bond_key).map(|b| b.target_finalizer))
                    .or_else(|| {
                        non_finalized_chain
                            .delegation_bonds
                            .get(&bond_key)
                            .map(|(b, _status)| b.target_finalizer)
                    })
                    .or_else(|| {
                        finalized_state
                            .delegation_bond(&bond_key)
                            .map(|b| b.target_finalizer)
                    })
                    .expect("bond exists: validate_bond_for_retarget passed");
                if from_pk != current_target {
                    return Err(ValidateContextError::InvalidDelegationBond(format!(
                        "retarget from_finalizer {:?} does not match bond's current target {:?}: {:?}",
                        from_pk, current_target, bond_key
                    )));
                }
                self.retargets.insert(bond_key, staking_action.target_finalizer_pk());
            }
            StakingActionKind::ConvertFinalizerRewardToDelegationBond => {
                // The finalizer's authorization signature was checked statelessly in
                // zebra-consensus; here: the new bond key is fresh, and the bank covers it.
                validate_create_new_bond(
                    bond_key,
                    &self.new_bonds,
                    non_finalized_chain,
                    finalized_state,
                )?;

                let amount_zats = staking_action.amount_zats();
                if amount_zats == 0 {
                    return Err(ValidateContextError::InvalidDelegationBond(format!(
                        "finalizer reward conversion of zero zats: {:?}",
                        bond_key
                    )));
                }
                let finalizer = staking_action.target_finalizer_pk();
                let bank = self.banks.entry(finalizer).or_insert_with(|| {
                    // The chain's banks are seeded from the finalized state, so the
                    // chain alone is authoritative for a fork; only a chain that is
                    // being created fresh could lack the key, and then the db has it.
                    non_finalized_chain
                        .finalizer_rewards
                        .get(&finalizer)
                        .copied()
                        .unwrap_or_else(|| finalized_state.finalizer_reward(&finalizer))
                });
                if *bank < amount_zats {
                    return Err(ValidateContextError::InvalidDelegationBond(format!(
                        "finalizer {:?} reward bank {} cannot cover conversion of {}: {:?}",
                        finalizer, *bank, amount_zats, bond_key
                    )));
                }
                let amount = zebra_chain::amount::Amount::try_from(amount_zats)
                    .map_err(|e| ValidateContextError::InvalidDelegationBond(format!("invalid bond amount: {:?}", e)))?;
                *bank -= amount_zats;
                let bond = DelegationBond::new(
                    amount,
                    finalizer,
                    crate::service::finalized_state::disk_format::TransactionLocation::from_usize(
                        height,
                        0,
                    ),
                );
                self.new_bonds.insert(bond_key, bond);
                self.versions.insert(bond_key, transaction_hash.0);
            }
            StakingActionKind::Null => {}
        }

        if staking_action.spends().is_some() {
            self.versions.insert(bond_key, transaction_hash.0);
        }

        Ok(())
    }
}

/// Validates CreateNewDelegationBond: ensures the bond key doesn't already exist.
fn validate_create_new_bond(
    bond_key: [u8; 32],
    block_new_bonds: &HashMap<[u8; 32], DelegationBond>,
    non_finalized_chain: &Chain,
    finalized_state: &ZebraDb,
) -> Result<(), ValidateContextError> {
    // Check if bond was already created in this block
    if block_new_bonds.contains_key(&bond_key) {
        return Err(ValidateContextError::InvalidDelegationBond(format!(
            "duplicate delegation bond in block: {:?}",
            bond_key
        )));
    }

    // Check if bond exists in non-finalized state (in any status)
    if non_finalized_chain.delegation_bonds.contains_key(&bond_key) {
        return Err(ValidateContextError::InvalidDelegationBond(format!(
            "delegation bond already exists: {:?}",
            bond_key
        )));
    }

    // Check if bond exists in finalized state
    if finalized_state.delegation_bond(&bond_key).is_some() {
        return Err(ValidateContextError::InvalidDelegationBond(format!(
            "delegation bond already exists in finalized state: {:?}",
            bond_key
        )));
    }

    Ok(())
}

/// Rejects an action on a bond at `height` when the bond's previous action, at `last_action`, is
/// less than the network's staking action delay earlier.
fn validate_staking_action_delay(
    bond_key: [u8; 32],
    height: zebra_chain::block::Height,
    last_action: zebra_chain::block::Height,
    non_finalized_chain: &Chain,
) -> Result<(), ValidateContextError> {
    let action_delay = non_finalized_chain.network().crosslink_parameters().staking.action_delay;

    if height.0 < last_action.0.saturating_add(action_delay) {
        return Err(ValidateContextError::InvalidDelegationBond(format!(
            "staking action delay not met: last action at height {}, this one at {}, {} blocks required: {:?}",
            last_action.0, height.0, action_delay, bond_key
        )));
    }
    Ok(())
}

/// Validates BeginDelegationUnbonding: ensures the bond exists, is active, and was created at least
/// the network's staking action delay before `height`.
fn validate_bond_for_unbonding(
    bond_key: [u8; 32],
    height: zebra_chain::block::Height,
    block_new_bonds: &HashMap<[u8; 32], DelegationBond>,
    non_finalized_chain: &Chain,
    finalized_state: &ZebraDb,
) -> Result<(), ValidateContextError> {
    if block_new_bonds.contains_key(&bond_key) {
        return validate_staking_action_delay(bond_key, height, height, non_finalized_chain);
    }

    // Check if bond exists in non-finalized state
    if let Some((bond, status)) = non_finalized_chain.delegation_bonds.get(&bond_key) {
        use crate::service::non_finalized_state::BondStatusInChain;

        match status {
            BondStatusInChain::Active => {
                return validate_staking_action_delay(bond_key, height, bond.created_at.height, non_finalized_chain);
            }
            BondStatusInChain::Unbonding { .. } => {
                return Err(ValidateContextError::InvalidDelegationBond(format!(
                    "delegation bond is already unbonding: {:?}",
                    bond_key
                )));
            }
            BondStatusInChain::Withdrawn { .. } => {
                return Err(ValidateContextError::InvalidDelegationBond(format!(
                    "delegation bond is already withdrawn: {:?}",
                    bond_key
                )));
            }
            BondStatusInChain::Burned => {
                return Err(ValidateContextError::InvalidDelegationBond(format!(
                    "delegation bond is burned: {:?}",
                    bond_key
                )));
            }
        }
    }

    // Check finalized state - bond must exist and be active
    if let Some(bond) = finalized_state.delegation_bond(&bond_key) {
        if finalized_state.is_bond_active(&bond_key) {
            return validate_staking_action_delay(bond_key, height, bond.created_at.height, non_finalized_chain);
        } else {
            return Err(ValidateContextError::InvalidDelegationBond(format!(
                "delegation bond is not active: {:?}",
                bond_key
            )));
        }
    }

    // Bond not found anywhere
    Err(ValidateContextError::InvalidDelegationBond(format!(
        "delegation bond not found: {:?}",
        bond_key
    )))
}

/// Validates WithdrawDelegationBond: ensures the bond exists, is unbonding, was unbonded at least
/// the network's staking action delay before `height`, and the amount matches.
fn validate_bond_for_withdrawal(
    bond_key: [u8; 32],
    height: zebra_chain::block::Height,
    withdrawal_amount: u64,
    non_finalized_chain: &Chain,
) -> Result<(), ValidateContextError> {
    // Check if bond exists in non-finalized state
    if let Some((bond, status)) = non_finalized_chain.delegation_bonds.get(&bond_key) {
        use crate::service::non_finalized_state::BondStatusInChain;

        match status {
            BondStatusInChain::Unbonding { unbonded_at } => {
                // Validate that withdrawal amount matches bond amount
                let bond_amount: u64 = bond.amount.into();
                if withdrawal_amount != bond_amount {
                    return Err(ValidateContextError::InvalidDelegationBond(format!(
                        "withdrawal amount {} does not match bond amount {}: {:?}",
                        withdrawal_amount, bond_amount, bond_key
                    )));
                }
                return validate_staking_action_delay(bond_key, height, unbonded_at.height, non_finalized_chain);
            }
            BondStatusInChain::Active => {
                return Err(ValidateContextError::InvalidDelegationBond(format!(
                    "delegation bond must be unbonded before withdrawal: {:?}",
                    bond_key
                )));
            }
            BondStatusInChain::Withdrawn { .. } => {
                return Err(ValidateContextError::InvalidDelegationBond(format!(
                    "delegation bond is already withdrawn: {:?}",
                    bond_key
                )));
            }
            BondStatusInChain::Burned => {
                return Err(ValidateContextError::InvalidDelegationBond(format!(
                    "delegation bond is burned: {:?}",
                    bond_key
                )));
            }
        }
    }

    // Bond not found in non-finalized state
    Err(ValidateContextError::InvalidDelegationBond(format!(
        "delegation bond not found: {:?}",
        bond_key
    )))
}

/// Validates RetargetDelegationBond: ensures the bond exists and is active.
fn validate_bond_for_retarget(
    bond_key: [u8; 32],
    block_new_bonds: &HashMap<[u8; 32], DelegationBond>,
    non_finalized_chain: &Chain,
    finalized_state: &ZebraDb,
) -> Result<(), ValidateContextError> {
    // Check if bond was created in this block (allowed to retarget immediately)
    if block_new_bonds.contains_key(&bond_key) {
        return Ok(());
    }

    // Check if bond exists in non-finalized state
    if let Some((_bond, status)) = non_finalized_chain.delegation_bonds.get(&bond_key) {
        use crate::service::non_finalized_state::BondStatusInChain;

        match status {
            BondStatusInChain::Active => return Ok(()),
            BondStatusInChain::Unbonding { .. } => {
                return Err(ValidateContextError::InvalidDelegationBond(format!(
                    "cannot retarget unbonding delegation bond: {:?}",
                    bond_key
                )));
            }
            BondStatusInChain::Withdrawn { .. } => {
                return Err(ValidateContextError::InvalidDelegationBond(format!(
                    "cannot retarget withdrawn delegation bond: {:?}",
                    bond_key
                )));
            }
            BondStatusInChain::Burned => {
                return Err(ValidateContextError::InvalidDelegationBond(format!(
                    "cannot retarget burned delegation bond: {:?}",
                    bond_key
                )));
            }
        }
    }

    // Check finalized state - bond must exist and be active
    if finalized_state.delegation_bond(&bond_key).is_some() {
        if finalized_state.is_bond_active(&bond_key) {
            return Ok(());
        } else {
            return Err(ValidateContextError::InvalidDelegationBond(format!(
                "cannot retarget delegation bond that is not active: {:?}",
                bond_key
            )));
        }
    }

    // Bond not found anywhere
    Err(ValidateContextError::InvalidDelegationBond(format!(
        "delegation bond not found for retarget: {:?}",
        bond_key
    )))
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use zcash_primitives::transaction::STAKING_ACTION_DELAY;
    use zebra_chain::{
        amount::Amount, block::Height, parallel::tree::NoteCommitmentTrees, parameters::Network,
        value_balance::ValueBalance,
    };

    use super::*;
    use crate::{
        service::finalized_state::{
            disk_format::{BondStatus, TransactionLocation},
            FinalizedState,
        },
        Config,
    };

    const KEY: [u8; 32] = [1; 32];
    const CREATED: u32 = 100;

    fn loc(height: u32) -> TransactionLocation {
        TransactionLocation::from_usize(Height(height), 1)
    }

    fn bond() -> DelegationBond {
        DelegationBond::new(Amount::try_from(1000u64).unwrap(), [7; 32], loc(CREATED))
    }

    fn chain_with(status: BondStatus) -> Chain {
        Chain::new(
            &Network::Mainnet,
            Height(CREATED),
            NoteCommitmentTrees::default(),
            Default::default(),
            ValueBalance::zero(),
            [(KEY, bond(), status)],
            std::iter::empty(),
        )
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

    /// [`invalid_staking_actions`] with the `i`th action in a transaction whose hash is `i + 1`
    /// in every byte.
    fn invalid_in_order(
        staking_actions: &[zcash_primitives::transaction::StakingAction],
        height: Height,
        chain: &Chain,
        finalized_state: &ZebraDb,
    ) -> Vec<usize> {
        let staking_actions: Vec<_> = staking_actions
            .iter()
            .enumerate()
            .map(|(i, staking_action)| (*staking_action, tx_hash(i)))
            .collect();
        invalid_staking_actions(&staking_actions, height, chain, finalized_state)
    }

    fn tx_hash(i: usize) -> zebra_chain::transaction::Hash {
        zebra_chain::transaction::Hash([i as u8 + 1; 32])
    }

    /// From FeaturenetUpgrade3 a retarget must spend the bond's current version: the txid of the
    /// transaction that created the bond, then of the last action that spent it, and never the
    /// bond key. So a retarget can't be mined twice, even after the bond is moved back to where it
    /// started. Before the upgrade the spending form is refused and the old one accepted.
    #[test]
    fn bond_actions_spend_the_bond_version_from_featurenet_upgrade_3() {
        use zcash_primitives::{bft::{finalizer_key_from_seed, FinalizerAddress}, transaction::StakingAction};
        use zebra_chain::parameters::testnet::{ConfiguredActivationHeights, RegtestParameters};

        const ACTIVATION: u32 = CREATED + 50;
        const NEW_KEY: [u8; 32] = [0x20; 32];
        let network = Network::new_regtest(RegtestParameters::from(ConfiguredActivationHeights {
            featurenet_upgrade_3: Some(ACTIVATION),
            ..Default::default()
        }));
        let a = FinalizerAddress::create(&finalizer_key_from_seed(b"a").0);
        let b = FinalizerAddress::create(&finalizer_key_from_seed(b"b").0);
        // Spent once already, so its version is known without the transaction that created it,
        // which this test's chain doesn't hold.
        let spent = tx_hash(9).0;
        let bond = DelegationBond {
            version: Some(spent),
            ..DelegationBond::new(Amount::try_from(1000u64).unwrap(), a.pub_key.0, loc(CREATED))
        };
        let mut chain = Chain::new(
            &network,
            Height(CREATED),
            NoteCommitmentTrees::default(),
            Default::default(),
            ValueBalance::zero(),
            [(KEY, bond, BondStatus::Active)],
            std::iter::empty(),
        );
        let finalized_state = finalized_state();
        let db = &finalized_state.db;
        let retarget_of = |key, from: FinalizerAddress, to: FinalizerAddress, spends| StakingAction::RetargetDelegationBond {
            unique_pubkey: key, signature: [0; 64], from_finalizer: from, to_finalizer: to, spends,
        };
        let retarget = |from, to, spends| retarget_of(KEY, from, to, spends);

        let below = Height(ACTIVATION - 1);
        assert_eq!(invalid_in_order(&[retarget(a, b, None)], below, &chain, db), Vec::<usize>::new());
        assert_eq!(invalid_in_order(&[retarget(a, b, Some(spent))], below, &chain, db), vec![0]);

        let at = Height(ACTIVATION);
        assert_eq!(invalid_in_order(&[retarget(a, b, None)], at, &chain, db), vec![0]);
        assert_eq!(invalid_in_order(&[retarget(a, b, Some([9; 32]))], at, &chain, db), vec![0]);
        assert_eq!(invalid_in_order(&[retarget(a, b, Some(KEY))], at, &chain, db), vec![0]);
        assert_eq!(invalid_in_order(&[retarget(a, b, Some(spent))], at, &chain, db), Vec::<usize>::new());
        assert_eq!(invalid_in_order(&[retarget_of(NEW_KEY, a, b, Some(NEW_KEY))], at, &chain, db), vec![0]);

        // A bond created earlier in the block starts at the create's txid, not at its key.
        let create = StakingAction::CreateNewDelegationBond {
            amount_zats: 1000, unique_pubkey: NEW_KEY, bond_salt: [0; 32], target_finalizer: a, signature: [0; 64],
        };
        assert_eq!(invalid_in_order(&[create, retarget_of(NEW_KEY, a, b, Some(NEW_KEY))], at, &chain, db), vec![1]);
        assert_eq!(invalid_in_order(&[create, retarget_of(NEW_KEY, a, b, Some(tx_hash(0).0))], at, &chain, db), Vec::<usize>::new());

        // In one block: there and back, each spending the last, and then the first one again.
        let there = retarget(a, b, Some(spent));
        let back = retarget(b, a, Some(tx_hash(0).0));
        assert_eq!(invalid_in_order(&[there, back, there], at, &chain, db), vec![2]);

        // Across blocks: once mined, the bond's version is the retarget's txid, so the same
        // retarget is refused after the bond is moved back.
        let mut mine = |target: FinalizerAddress, i| {
            let (bond, _status) = chain.delegation_bonds.get_mut(&KEY).unwrap();
            bond.target_finalizer = target.pub_key.0;
            bond.version = Some(tx_hash(i).0);
        };
        mine(b, 0);
        mine(a, 1);
        assert_eq!(bond_version(&chain.delegation_bonds[&KEY].0, Some(&chain), db), tx_hash(1).0);
        assert_eq!(invalid_in_order(&[there], Height(ACTIVATION + 2), &chain, db), vec![0]);
        assert_eq!(invalid_in_order(&[retarget(a, b, Some(tx_hash(1).0))], Height(ACTIVATION + 2), &chain, db), Vec::<usize>::new());
    }

    /// A bond's version is stored after the rest of the bond only once an action has spent it,
    /// so a record written before versions existed reads back as an unspent bond, whose version
    /// is the txid of the transaction at its creation location.
    #[test]
    fn bond_version_round_trips_through_disk_format() {
        use crate::service::finalized_state::{FromDisk, IntoDisk};

        let unspent = bond();
        assert_eq!(unspent.as_bytes().len(), 45);
        assert_eq!(DelegationBond::from_bytes(unspent.as_bytes()), unspent);

        let spent = DelegationBond { version: Some([5; 32]), ..bond() };
        assert_eq!(DelegationBond::from_bytes(spent.as_bytes()), spent);
        assert_eq!(spent.current_version(|_| panic!("a spent bond's version is stored")), [5; 32]);
        assert_eq!(
            unspent.current_version(|created_at| {
                assert_eq!(created_at, loc(CREATED));
                [6; 32]
            }),
            [6; 32]
        );
    }

    #[test]
    fn unbonding_waits_for_the_delay_after_creation() {
        let finalized_state = finalized_state();
        let chain = chain_with(BondStatus::Active);
        let unbond_at = |height| {
            validate_bond_for_unbonding(KEY, Height(height), &HashMap::new(), &chain, &finalized_state.db)
        };

        assert!(unbond_at(CREATED + STAKING_ACTION_DELAY - 1).is_err());
        assert!(unbond_at(CREATED + STAKING_ACTION_DELAY).is_ok());
    }

    #[test]
    fn unbonding_a_bond_created_in_the_same_block_is_rejected() {
        let finalized_state = finalized_state();
        let chain = chain_with(BondStatus::Active);
        let block_new_bonds = HashMap::from([([2; 32], bond())]);

        assert!(validate_bond_for_unbonding([2; 32], Height(500), &block_new_bonds, &chain, &finalized_state.db).is_err());
    }

    #[test]
    fn withdrawal_waits_for_the_delay_after_unbonding() {
        let unbonded = CREATED + 400;
        let chain = chain_with(BondStatus::Unbonding { unbonded_at: loc(unbonded) });
        let withdraw_at = |height| validate_bond_for_withdrawal(KEY, Height(height), 1000, &chain);

        assert!(withdraw_at(unbonded + STAKING_ACTION_DELAY - 1).is_err());
        assert!(withdraw_at(unbonded + STAKING_ACTION_DELAY).is_ok());
    }

    /// A template keeps only the staking actions its block accepts. Actions are applied in order
    /// with the block's rules, and a rejected one is left out, so the next is checked without it.
    #[test]
    fn invalid_staking_actions_skips_each_rejected_action() {
        use zcash_primitives::transaction::StakingAction;

        let finalized_state = finalized_state();
        let chain = chain_with(BondStatus::Active);
        let unbond = |key| StakingAction::BeginDelegationUnbonding { unique_pubkey: key, signature: [0; 64], spends: None };
        let height = Height(CREATED + STAKING_ACTION_DELAY);

        // An unknown bond, then a valid unbond, then the same bond unbonding a second time.
        let actions = [unbond([9; 32]), unbond(KEY), unbond(KEY)];
        assert_eq!(invalid_in_order(&actions, height, &chain, &finalized_state.db), vec![0, 2]);

        // The staking action delay applies here too.
        let early = Height(CREATED + STAKING_ACTION_DELAY - 1);
        assert_eq!(invalid_in_order(&[unbond(KEY)], early, &chain, &finalized_state.db), vec![0]);
    }

    /// STAKING_AUDIT S3: a retarget that is valid on its own is rejected after the same bond
    /// unbonds earlier in the block, because an unbonding bond keeps its target.
    #[test]
    fn retarget_after_unbonding_in_the_same_block_is_rejected() {
        use zcash_primitives::{bft::{finalizer_key_from_seed, FinalizerAddress}, transaction::StakingAction};

        let from = FinalizerAddress::create(&finalizer_key_from_seed(b"from").0);
        let to = FinalizerAddress::create(&finalizer_key_from_seed(b"to").0);
        let bond = DelegationBond::new(Amount::try_from(1000u64).unwrap(), from.pub_key.0, loc(CREATED));
        let chain = Chain::new(
            &Network::Mainnet,
            Height(CREATED),
            NoteCommitmentTrees::default(),
            Default::default(),
            ValueBalance::zero(),
            [(KEY, bond, BondStatus::Active)],
            std::iter::empty(),
        );
        let finalized_state = finalized_state();
        let height = Height(CREATED + STAKING_ACTION_DELAY);

        let retarget = StakingAction::RetargetDelegationBond { unique_pubkey: KEY, signature: [0; 64], from_finalizer: from, to_finalizer: to, spends: None };
        let unbond = StakingAction::BeginDelegationUnbonding { unique_pubkey: KEY, signature: [0; 64], spends: None };

        assert_eq!(invalid_in_order(&[retarget], height, &chain, &finalized_state.db), Vec::<usize>::new());
        assert_eq!(invalid_in_order(&[unbond, retarget], height, &chain, &finalized_state.db), vec![1]);
    }

    /// From the rule's activation height on, a new bond must be a power of 10 zats.
    #[test]
    fn new_bond_amount_must_be_a_power_of_10_once_active() {
        use zcash_primitives::{
            bft::{finalizer_key_from_seed, FinalizerAddress},
            transaction::{is_power_of_10, StakingAction},
        };
        use zcash_protocol::consensus::{NetworkUpgrade, Parameters};

        for (n, expected) in [(0, false), (1, true), (10, true), (11, false), (20, false), (1000, true), (10u64.pow(19), true), (u64::MAX, false)] {
            assert_eq!(is_power_of_10(n), expected, "{n}");
        }

        let network = Network::new_default_testnet();
        let ACTIVATION = u32::from(network.activation_height(NetworkUpgrade::FeaturenetUpgrade1).unwrap());
        let finalized_state = finalized_state();
        let chain = Chain::new(
            &network,
            Height(CREATED),
            NoteCommitmentTrees::default(),
            Default::default(),
            ValueBalance::zero(),
            [(KEY, bond(), BondStatus::Active)],
            std::iter::empty(),
        );
        let target_finalizer = FinalizerAddress::create(&finalizer_key_from_seed(b"target").0);
        let create = |amount_zats| StakingAction::CreateNewDelegationBond {
            amount_zats, unique_pubkey: [2; 32], bond_salt: [0; 32], target_finalizer, signature: [0; 64],
        };
        let invalid_at = |height, amount_zats| {
            invalid_in_order(&[create(amount_zats)], Height(height), &chain, &finalized_state.db)
        };

        assert_eq!(invalid_at(ACTIVATION - 1, 1500), Vec::<usize>::new());
        assert_eq!(invalid_at(ACTIVATION, 1500), vec![0]);
        assert_eq!(invalid_at(ACTIVATION, 1000), Vec::<usize>::new());

        // Only a new bond is restricted: a conversion takes whatever the bank holds.
        let convert = StakingAction::ConvertFinalizerRewardToDelegationBond {
            unique_pubkey: [3; 32], signature: [0; 64], bond_salt: [0; 32], this_finalizer: [4; 32],
            amount_zats: 1500, finalizer_signature: [0; 64],
        };
        assert!(convert.amount_is_valid_at(&network, ACTIVATION));
    }

    /// Under ZIP 215 verification a small-order key accepts the all-zero signature over every
    /// message, so the identity key with that signature is a finalizer address that verifies and
    /// that everyone can sign for. From FeaturenetUpgrade3 a bond may not point at it, and its
    /// signature authorizes no reward conversion. Before the upgrade both pass, as they always did.
    #[test]
    fn a_small_order_finalizer_key_is_refused_from_featurenet_upgrade_3() {
        use zcash_primitives::{
            bft::{finalizer_key_from_seed, sign_finalizer_reward_conversion, verify_finalizer_reward_conversion_at, FinalizerAddress, PubKeyID, TMSig},
            transaction::StakingAction,
        };
        use zcash_protocol::consensus::{NetworkUpgrade, Parameters};
        use zebra_chain::parameters::testnet::{ConfiguredActivationHeights, RegtestParameters};

        let network = Network::new_regtest(RegtestParameters::from(ConfiguredActivationHeights {
            featurenet_upgrade_3: Some(CREATED + 50),
            ..Default::default()
        }));
        let activation = u32::from(network.activation_height(NetworkUpgrade::FeaturenetUpgrade3).unwrap());

        // The identity point's encoding, and the signature R = identity, s = 0.
        let mut identity = [0u8; 32];
        identity[0] = 1;
        let mut forged = [0u8; 64];
        forged[0] = 1;
        let small_order = FinalizerAddress { pub_key: PubKeyID(identity), sig: TMSig(forged) };
        assert!(small_order.verify());
        assert!(small_order.pub_key.is_small_order());
        let (finalizer_key, _) = finalizer_key_from_seed(b"target");
        let honest = FinalizerAddress::create(&finalizer_key);
        assert!(!honest.pub_key.is_small_order());

        for (height, refused) in [(activation - 1, false), (activation, true)] {
            assert_eq!(small_order.verify_at(&network, height), !refused, "height {height}");
            assert!(honest.verify_at(&network, height), "height {height}");
            assert_eq!(verify_finalizer_reward_conversion_at(&network, height, PubKeyID(identity), &[9; 32], 1, &forged), !refused, "height {height}");
            let signed = sign_finalizer_reward_conversion(&finalizer_key, &[9; 32], 1);
            assert!(verify_finalizer_reward_conversion_at(&network, height, honest.pub_key, &[9; 32], 1, &signed), "height {height}");
        }

        let finalized_state = finalized_state();
        let chain = Chain::new(
            &network,
            Height(CREATED),
            NoteCommitmentTrees::default(),
            Default::default(),
            ValueBalance::zero(),
            [(KEY, bond(), BondStatus::Active)],
            std::iter::empty(),
        );
        let create = |target_finalizer| StakingAction::CreateNewDelegationBond {
            amount_zats: 1000, unique_pubkey: [2; 32], bond_salt: [0; 32], target_finalizer, signature: [0; 64],
        };
        let invalid_at = |height, target| invalid_in_order(&[create(target)], Height(height), &chain, &finalized_state.db);
        assert_eq!(invalid_at(activation - 1, small_order), Vec::<usize>::new());
        assert_eq!(invalid_at(activation, small_order), vec![0]);
        assert_eq!(invalid_at(activation, honest), Vec::<usize>::new());
    }

    /// A second withdrawal of one bond in a block must be rejected here: the commit expects each
    /// withdrawal to find its bond unbonding, and panics on the second.
    #[test]
    fn second_withdrawal_in_the_same_block_is_rejected() {
        use zcash_primitives::transaction::StakingAction;

        let unbonded = CREATED + 400;
        let chain = chain_with(BondStatus::Unbonding { unbonded_at: loc(unbonded) });
        let finalized_state = finalized_state();
        let height = Height(unbonded + STAKING_ACTION_DELAY);

        let withdraw = StakingAction::WithdrawDelegationBond { amount_zats: 1000, unique_pubkey: KEY, signature: [0; 64], spends: None };
        assert_eq!(invalid_in_order(&[withdraw, withdraw], height, &chain, &finalized_state.db), vec![1]);
    }
}
