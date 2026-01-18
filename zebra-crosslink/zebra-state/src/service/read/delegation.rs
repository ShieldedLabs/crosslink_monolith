//! Delegation bond queries that search both finalized and non-finalized state.

use crate::service::{
    finalized_state::{disk_format::{BondKey, BondStatus, DelegationBond}, ZebraDb},
    non_finalized_state::Chain,
};

/// Get a delegation bond from the state (finalized + non-finalized).
///
/// Searches the non-finalized state first, then falls back to finalized state.
/// Returns the bond data and its status if found.
pub fn delegation_bond(
    finalized_state: &ZebraDb,
    non_finalized_state: Option<&Chain>,
    bond_key: &BondKey,
) -> Option<(DelegationBond, BondStatus)> {
    // Check non-finalized state first
    if let Some(chain) = non_finalized_state {
        use crate::service::non_finalized_state::BondStatusInChain;

        if let Some((bond, status)) = chain.delegation_bonds.get(bond_key) {
            // Convert BondStatusInChain to BondStatus
            let finalized_status = match status {
                BondStatusInChain::Active => BondStatus::Active,
                BondStatusInChain::Unbonding => {
                    BondStatus::Unbonding {
                        unbonded_at: bond.created_at, // Using created_at as placeholder
                    }
                }
                BondStatusInChain::Withdrawn => {
                    BondStatus::Withdrawn {
                        withdrawn_at: bond.created_at, // Using created_at as placeholder
                    }
                }
            };
            return Some((*bond, finalized_status));
        }
    }

    // Fall back to finalized state
    let bond = finalized_state.delegation_bond(bond_key)?;
    let status = finalized_state.bond_status(bond_key)?;
    Some((bond, status))
}

/// Check if a bond exists and is active in the state.
///
/// Searches both non-finalized and finalized state.
pub fn is_bond_active(
    finalized_state: &ZebraDb,
    non_finalized_state: Option<&Chain>,
    bond_key: &BondKey,
) -> bool {
    // Check non-finalized state first
    if let Some(chain) = non_finalized_state {
        use crate::service::non_finalized_state::BondStatusInChain;

        if let Some((_, status)) = chain.delegation_bonds.get(bond_key) {
            return *status == BondStatusInChain::Active;
        }
    }

    // Fall back to finalized state
    finalized_state.is_bond_active(bond_key)
}

/// Check if a bond exists and is unbonding in the state.
///
/// Searches both non-finalized and finalized state.
pub fn is_bond_unbonding(
    finalized_state: &ZebraDb,
    non_finalized_state: Option<&Chain>,
    bond_key: &BondKey,
) -> bool {
    // Check non-finalized state first
    if let Some(chain) = non_finalized_state {
        use crate::service::non_finalized_state::BondStatusInChain;

        if let Some((_, status)) = chain.delegation_bonds.get(bond_key) {
            return *status == BondStatusInChain::Unbonding;
        }
    }

    // Fall back to finalized state
    finalized_state
        .bond_status(bond_key)
        .map(|status| status.is_unbonding())
        .unwrap_or(false)
}

/// Check if a bond exists and is withdrawn in the state.
///
/// Searches both non-finalized and finalized state.
pub fn is_bond_withdrawn(
    finalized_state: &ZebraDb,
    non_finalized_state: Option<&Chain>,
    bond_key: &BondKey,
) -> bool {
    // Check non-finalized state first
    if let Some(chain) = non_finalized_state {
        use crate::service::non_finalized_state::BondStatusInChain;

        if let Some((_, status)) = chain.delegation_bonds.get(bond_key) {
            return *status == BondStatusInChain::Withdrawn;
        }
    }

    // Fall back to finalized state
    finalized_state
        .bond_status(bond_key)
        .map(|status| status.is_withdrawn())
        .unwrap_or(false)
}

/// Check if a bond exists in the state (in any status).
///
/// Searches both non-finalized and finalized state.
pub fn bond_exists(
    finalized_state: &ZebraDb,
    non_finalized_state: Option<&Chain>,
    bond_key: &BondKey,
) -> bool {
    // Check non-finalized state first
    if let Some(chain) = non_finalized_state {
        if chain.delegation_bonds.contains_key(bond_key) {
            return true;
        }
    }

    // Fall back to finalized state
    finalized_state.delegation_bond(bond_key).is_some()
}
