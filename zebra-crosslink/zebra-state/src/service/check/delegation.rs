//! Delegation bond validation for contextual checks.

use std::collections::HashMap;

use crate::{
    service::{
        finalized_state::{disk_format::{BondKey, DelegationBond}, ZebraDb},
        non_finalized_state::Chain,
    },
    SemanticallyVerifiedBlock, ValidateContextError,
};

/// Validate all delegation bond operations in a [`SemanticallyVerifiedBlock`].
/// If any of the operations are invalid, return an error.
///
/// This function:
/// - Validates that CreateNewDelegationBond doesn't create duplicate bonds
/// - Validates that BeginDelegationUnbonding references existing active bonds
/// - Validates that WithdrawDelegationBond references existing unbonding bonds
pub fn validate_delegation_bonds(
    semantically_verified: &SemanticallyVerifiedBlock,
    non_finalized_chain: &Chain,
    finalized_state: &ZebraDb,
) -> Result<(), ValidateContextError> {
    use zcash_primitives::transaction::StakingActionKind;

    // Track bonds created and modified within this block
    let mut block_new_bonds = HashMap::new();
    let mut block_unbonding_bonds = HashMap::new();

    for transaction in &semantically_verified.block.transactions {
        if let Some(staking_action) = transaction.staking_action() {
            let bond_key = staking_action.arg32_0;

            match staking_action.kind {
                StakingActionKind::CreateNewDelegationBond => {
                    // Check that bond doesn't already exist
                    validate_create_new_bond(
                        bond_key,
                        &block_new_bonds,
                        non_finalized_chain,
                        finalized_state,
                    )?;

                    // Track this new bond for subsequent validation in this block
                    let amount =
                        zebra_chain::amount::Amount::try_from(staking_action.amount_zats)
                            .map_err(|e| {
                                ValidateContextError::InvalidDelegationBond(format!(
                                    "invalid bond amount: {:?}",
                                    e
                                ))
                            })?;
                    let target_finalizer = staking_action.arg32_2;
                    let bond = DelegationBond::new(
                        amount,
                        target_finalizer,
                        crate::service::finalized_state::disk_format::TransactionLocation::from_usize(
                            semantically_verified.height,
                            0,
                        ),
                    );
                    block_new_bonds.insert(bond_key, bond);
                }
                StakingActionKind::BeginDelegationUnbonding => {
                    // Check that bond exists and is active
                    validate_bond_for_unbonding(
                        bond_key,
                        &block_new_bonds,
                        &block_unbonding_bonds,
                        non_finalized_chain,
                        finalized_state,
                    )?;

                    // Track this unbonding for subsequent validation in this block
                    block_unbonding_bonds.insert(bond_key, ());
                }
                StakingActionKind::WithdrawDelegationBond => {
                    // Check that bond exists, is unbonding, and withdrawal amount matches bond amount
                    validate_bond_for_withdrawal(
                        bond_key,
                        staking_action.amount_zats,
                        non_finalized_chain,
                    )?;
                }
                // Other staking actions don't affect delegation bonds
                _ => {}
            }
        }
    }

    Ok(())
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

/// Validates BeginDelegationUnbonding: ensures the bond exists and is active.
fn validate_bond_for_unbonding(
    bond_key: [u8; 32],
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

    // Check if bond was created in this block (allowed)
    if block_new_bonds.contains_key(&bond_key) {
        return Ok(());
    }

    // Check if bond exists in non-finalized state
    if let Some((_bond, status)) = non_finalized_chain.delegation_bonds.get(&bond_key) {
        use crate::service::non_finalized_state::BondStatusInChain;

        match status {
            BondStatusInChain::Active => {
                return Ok(());
            }
            BondStatusInChain::Unbonding => {
                return Err(ValidateContextError::InvalidDelegationBond(format!(
                    "delegation bond is already unbonding: {:?}",
                    bond_key
                )));
            }
            BondStatusInChain::Withdrawn => {
                return Err(ValidateContextError::InvalidDelegationBond(format!(
                    "delegation bond is already withdrawn: {:?}",
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

/// Validates WithdrawDelegationBond: ensures the bond exists, is unbonding, and amount matches.
fn validate_bond_for_withdrawal(
    bond_key: [u8; 32],
    withdrawal_amount: u64,
    non_finalized_chain: &Chain,
) -> Result<(), ValidateContextError> {
    // Check if bond exists in non-finalized state
    if let Some((bond, status)) = non_finalized_chain.delegation_bonds.get(&bond_key) {
        use crate::service::non_finalized_state::BondStatusInChain;

        match status {
            BondStatusInChain::Unbonding => {
                // Validate that withdrawal amount matches bond amount
                let bond_amount: u64 = bond.amount.into();
                if withdrawal_amount != bond_amount {
                    return Err(ValidateContextError::InvalidDelegationBond(format!(
                        "withdrawal amount {} does not match bond amount {}: {:?}",
                        withdrawal_amount, bond_amount, bond_key
                    )));
                }
                return Ok(());
            }
            BondStatusInChain::Active => {
                return Err(ValidateContextError::InvalidDelegationBond(format!(
                    "delegation bond must be unbonded before withdrawal: {:?}",
                    bond_key
                )));
            }
            BondStatusInChain::Withdrawn => {
                return Err(ValidateContextError::InvalidDelegationBond(format!(
                    "delegation bond is already withdrawn: {:?}",
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
