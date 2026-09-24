//! Delegation bond validation for contextual checks.

use std::collections::HashMap;

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
///   `STAKING_ACTION_DELAY` blocks earlier
/// - Validates that WithdrawDelegationBond references existing unbonding bonds unbonded at least
///   `STAKING_ACTION_DELAY` blocks earlier
/// - Validates that RetargetDelegationBond references existing active bonds
pub fn validate_delegation_bonds(
    semantically_verified: &SemanticallyVerifiedBlock,
    non_finalized_chain: &Chain,
    finalized_state: &ZebraDb,
) -> Result<(), ValidateContextError> {
    let mut in_block = InBlockBonds::default();
    for transaction in &semantically_verified.block.transactions {
        if let Some(staking_action) = transaction.staking_action() {
            in_block.apply(staking_action, semantically_verified.height, non_finalized_chain, finalized_state)?;
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
    staking_actions: &[zcash_primitives::transaction::StakingAction],
    height: zebra_chain::block::Height,
    non_finalized_chain: &Chain,
    finalized_state: &ZebraDb,
) -> Vec<usize> {
    let mut in_block = InBlockBonds::default();
    let mut invalid = Vec::new();
    for (i, staking_action) in staking_actions.iter().enumerate() {
        if in_block.apply(staking_action, height, non_finalized_chain, finalized_state).is_err() {
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
    unbonding_bonds: HashMap<[u8; 32], ()>,
    // bond_key -> target after in-block retargets, so a second retarget in the
    // same block validates its `from` against the first one's `to`
    retargets: HashMap<[u8; 32], [u8; 32]>,
    // finalizer -> bank balance after in-block conversions; the block's own
    // commission is paid after its transactions, so it never funds them
    banks: HashMap<[u8; 32], u64>,
}

impl InBlockBonds {
    fn apply(
        &mut self,
        staking_action: &zcash_primitives::transaction::StakingAction,
        height: zebra_chain::block::Height,
        non_finalized_chain: &Chain,
        finalized_state: &ZebraDb,
    ) -> Result<(), ValidateContextError> {
        use zcash_primitives::transaction::StakingActionKind;

        let bond_key = staking_action.bond_key();

        // The target finalizer is a capability: the address embeds the
        // finalizer key's signature over the standard message, and an action
        // whose signature doesn't verify is consensus-invalid. This is what
        // stops stake being pointed at a key nobody controls. Retarget's
        // `from_finalizer` must verify too.
        for addr in [staking_action.target_finalizer_address(), staking_action.from_finalizer_address()] {
            if let Some(addr) = addr {
                if !addr.verify() {
                    return Err(ValidateContextError::InvalidDelegationBond(format!(
                        "invalid finalizer address capability: {:?}",
                        addr.pub_key
                    )));
                }
            }
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
            }
            StakingActionKind::BeginDelegationUnbonding => {
                // Check that bond exists and is active
                validate_bond_for_unbonding(
                    bond_key,
                    height,
                    &self.new_bonds,
                    &self.unbonding_bonds,
                    non_finalized_chain,
                    finalized_state,
                )?;

                // Track this unbonding for subsequent validation in this block
                self.unbonding_bonds.insert(bond_key, ());
            }
            StakingActionKind::WithdrawDelegationBond => {
                // Check that bond exists, is unbonding, and withdrawal amount matches bond amount
                validate_bond_for_withdrawal(
                    bond_key,
                    height,
                    staking_action.amount_zats(),
                    non_finalized_chain,
                )?;
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
            }
            StakingActionKind::Null => {}
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
/// less than `STAKING_ACTION_DELAY` blocks earlier.
fn validate_staking_action_delay(
    bond_key: [u8; 32],
    height: zebra_chain::block::Height,
    last_action: zebra_chain::block::Height,
) -> Result<(), ValidateContextError> {
    use zcash_primitives::transaction::STAKING_ACTION_DELAY;

    if height.0 < last_action.0.saturating_add(STAKING_ACTION_DELAY) {
        return Err(ValidateContextError::InvalidDelegationBond(format!(
            "staking action delay not met: last action at height {}, this one at {}, {} blocks required: {:?}",
            last_action.0, height.0, STAKING_ACTION_DELAY, bond_key
        )));
    }
    Ok(())
}

/// Validates BeginDelegationUnbonding: ensures the bond exists, is active, and was created at least
/// `STAKING_ACTION_DELAY` blocks before `height`.
fn validate_bond_for_unbonding(
    bond_key: [u8; 32],
    height: zebra_chain::block::Height,
    block_new_bonds: &HashMap<[u8; 32], DelegationBond>,
    block_unbonding_bonds: &HashMap<[u8; 32], ()>,
    non_finalized_chain: &Chain,
    finalized_state: &ZebraDb,
) -> Result<(), ValidateContextError> {
    // Check if already unbonding in this block
    if block_unbonding_bonds.contains_key(&bond_key) {
        return Err(ValidateContextError::InvalidDelegationBond(format!(
            "delegation bond already unbonding in block: {:?}",
            bond_key
        )));
    }

    if block_new_bonds.contains_key(&bond_key) {
        return validate_staking_action_delay(bond_key, height, height);
    }

    // Check if bond exists in non-finalized state
    if let Some((bond, status)) = non_finalized_chain.delegation_bonds.get(&bond_key) {
        use crate::service::non_finalized_state::BondStatusInChain;

        match status {
            BondStatusInChain::Active => {
                return validate_staking_action_delay(bond_key, height, bond.created_at.height);
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
            return validate_staking_action_delay(bond_key, height, bond.created_at.height);
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
/// `STAKING_ACTION_DELAY` blocks before `height`, and the amount matches.
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
                return validate_staking_action_delay(bond_key, height, unbonded_at.height);
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

    #[test]
    fn unbonding_waits_for_the_delay_after_creation() {
        let finalized_state = finalized_state();
        let chain = chain_with(BondStatus::Active);
        let unbond_at = |height| {
            validate_bond_for_unbonding(KEY, Height(height), &HashMap::new(), &HashMap::new(), &chain, &finalized_state.db)
        };

        assert!(unbond_at(CREATED + STAKING_ACTION_DELAY - 1).is_err());
        assert!(unbond_at(CREATED + STAKING_ACTION_DELAY).is_ok());
    }

    #[test]
    fn unbonding_a_bond_created_in_the_same_block_is_rejected() {
        let finalized_state = finalized_state();
        let chain = chain_with(BondStatus::Active);
        let block_new_bonds = HashMap::from([([2; 32], bond())]);

        assert!(validate_bond_for_unbonding([2; 32], Height(500), &block_new_bonds, &HashMap::new(), &chain, &finalized_state.db).is_err());
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
        let unbond = |key| StakingAction::BeginDelegationUnbonding { unique_pubkey: key, signature: [0; 64] };
        let height = Height(CREATED + STAKING_ACTION_DELAY);

        // An unknown bond, then a valid unbond, then the same bond unbonding a second time.
        let actions = [unbond([9; 32]), unbond(KEY), unbond(KEY)];
        assert_eq!(invalid_staking_actions(&actions, height, &chain, &finalized_state.db), vec![0, 2]);

        // The staking action delay applies here too.
        let early = Height(CREATED + STAKING_ACTION_DELAY - 1);
        assert_eq!(invalid_staking_actions(&[unbond(KEY)], early, &chain, &finalized_state.db), vec![0]);
    }
}
