//! Hardfork slash burns.
//!
//! To burn the bonds of a finalizer `T` slashed at activation height `A`, we must
//! find every bond delegated to `T` at the end of any block in the window `[A - W, A)`. This
//! includes bonds still pointing at `T` (sitting ducks) and bonds that retargeted
//! or unbonded away from `T` inside the window (cockroaches/fleers).
//!
//! Because a Retarget action names both its `from` and `to` finalizers, the whole
//! computation is lazy and local: just before the activation block's staking
//! actions, combine the current bond state (which names every bond still parked on,
//! or unbonding from, a terminated finalizer) with a read of the blocks after `A - W`
//! and below `A` (whose Retarget `from`s name every bond that left one inside the window).
//! No genesis scan, no persistent index, no background catch-up.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use zebra_chain::block::{Block, Height};

pub use zcash_primitives::transaction::SLASH_ANALYSIS_WINDOW;

use crate::service::{
    finalized_state::disk_format::{BondKey, DelegationBond},
    non_finalized_state::BondStatusInChain,
};

/// The burn set for a hardfork activating at `activation`: every bond delegated
/// to a finalizer in `slashed` at the end of any block in `[activation - W, activation)`.
///
/// `bonds` is the bond state at the end of block `activation - 1`, before any of the
/// activation block's staking actions, and `window_blocks` yields the blocks at
/// heights `(activation - W, activation)`, in any order — no state is threaded
/// between them. A bond that leaves `T` in block `activation - W` itself is spared:
/// it is no longer on `T` at the end of that block.
///
/// Every delegation stretch onto a slashed finalizer is caught by exactly one of
/// two checks:
/// - the stretch reaches the present: the bond still targets `T` in `bonds`,
///   either Active or Unbonding (unbonding keeps the target, and `unbonded_at`
///   dates the stretch's end, so a bond that unbonded at or before the window start
///   is spared, however long before the window it was created);
/// - the stretch ended with an in-window Retarget: that action's `from` is `T`.
/// A stretch that *began* in the window needs no check of its own — it either
/// still stands (first case) or ended by retarget (second) or by unbonding
/// (first, via the kept target).
///
/// Withdrawn bonds are skipped, and none of them escapes: `SLASH_ANALYSIS_WINDOW` is
/// sized so that a bond still delegated at the end of the window's first block can't
/// withdraw before the activation block, and the activation block's actions see it
/// already burned.
pub fn slash_burn_set(
    bonds: &HashMap<BondKey, (DelegationBond, BondStatusInChain)>,
    window_blocks: impl IntoIterator<Item = Arc<Block>>,
    slashed: &BTreeSet<[u8; 32]>,
    activation: Height,
) -> BTreeSet<BondKey> {
    use zcash_primitives::transaction::StakingAction;

    let window_start = activation.0.saturating_sub(SLASH_ANALYSIS_WINDOW);
    let mut burned = BTreeSet::new();

    for (bond_key, (bond, status)) in bonds {
        if !slashed.contains(&bond.target_finalizer) {
            continue;
        }
        let in_window = match status {
            BondStatusInChain::Active => true,
            BondStatusInChain::Unbonding { unbonded_at } => unbonded_at.height.0 > window_start,
            BondStatusInChain::Withdrawn { .. } | BondStatusInChain::Burned => false,
        };
        if in_window {
            burned.insert(*bond_key);
        }
    }

    for block in window_blocks {
        for tx in block.transactions.iter() {
            if let Some(StakingAction::RetargetDelegationBond { unique_pubkey, from_finalizer, .. }) =
                tx.staking_action()
            {
                if slashed.contains(&from_finalizer.pub_key.0) {
                    burned.insert(*unique_pubkey);
                }
            }
        }
    }

    burned
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashMap};

    use zcash_primitives::transaction::StakingAction;
    use zebra_chain::{
        amount::{Amount, NonNegative},
        block::Height,
        parameters::Network,
        parallel::tree::NoteCommitmentTrees,
        transaction,
        value_balance::ValueBalance,
    };

    use super::slash_burn_set;
    use crate::service::{
        burn_delegation_bonds,
        finalized_state::disk_format::{BondKey, BondStatus, DelegationBond, TransactionLocation},
        non_finalized_state::{BondStatusInChain, Chain},
        update_chain_tip_with_delegation_bond,
    };

    // The window is [820, 1050). Activation heights are multiples of the staking period.
    const ACTIVATION: u32 = 1050;
    const SLASHED: [u8; 32] = [7; 32];
    const OTHER: [u8; 32] = [8; 32];
    const BOND_ZATS: u64 = 1000;

    fn loc(height: u32) -> TransactionLocation {
        TransactionLocation::from_usize(Height(height), 1)
    }

    fn bond(target: [u8; 32], created: u32) -> DelegationBond {
        DelegationBond::new(Amount::try_from(BOND_ZATS).unwrap(), target, loc(created))
    }

    fn burn_set(bonds: &HashMap<BondKey, (DelegationBond, BondStatusInChain)>) -> BTreeSet<BondKey> {
        slash_burn_set(bonds, std::iter::empty(), &BTreeSet::from([SLASHED]), Height(ACTIVATION))
    }

    #[test]
    fn bond_created_before_window_and_unbonded_inside_it_is_burned() {
        let key = [1; 32];
        let mut bonds = HashMap::from([(key, (bond(SLASHED, 100), BondStatusInChain::Active))]);
        let mut pools = ValueBalance::<NonNegative>::zero();
        pools.set_staking_bonded_amount(Amount::try_from(BOND_ZATS).unwrap());

        update_chain_tip_with_delegation_bond(
            &mut pools,
            &mut bonds,
            &mut vec![HashMap::new()],
            &mut HashMap::new(),
            &StakingAction::BeginDelegationUnbonding { unique_pubkey: key, signature: [0; 64] },
            &transaction::Hash([0; 32]),
            loc(900),
        )
        .unwrap();

        let unbonding = BondStatusInChain::Unbonding { unbonded_at: loc(900) };
        assert_eq!(bonds[&key], (bond(SLASHED, 100), unbonding));

        let burned = burn_set(&bonds);
        assert_eq!(burned, BTreeSet::from([key]));

        let reverts = burn_delegation_bonds(&mut bonds, &burned);
        assert_eq!(bonds[&key].1, BondStatusInChain::Burned);
        assert_eq!(reverts, vec![(key, unbonding)]);
    }

    #[test]
    fn bond_loaded_from_finalized_state_unbonded_inside_window_is_burned() {
        let key = [1; 32];
        let chain = Chain::new(
            &Network::Mainnet,
            Height(950),
            NoteCommitmentTrees::default(),
            Default::default(),
            ValueBalance::zero(),
            [(key, bond(SLASHED, 100), BondStatus::Unbonding { unbonded_at: loc(900) })],
            std::iter::empty(),
        );

        assert_eq!(burn_set(&chain.delegation_bonds), BTreeSet::from([key]));
    }

    #[test]
    fn burn_set_covers_exactly_the_bonds_on_the_slashed_finalizer_in_window() {
        let active = [1; 32];
        let unbonded_just_inside = [2; 32];
        let unbonded_at_window_start = [3; 32];
        let unbonded_before_window = [4; 32];
        let active_elsewhere = [5; 32];
        let unbonding_elsewhere = [6; 32];
        let withdrawn_before_window = [10; 32];
        let burned_already = [9; 32];

        let bonds = HashMap::from([
            (active, (bond(SLASHED, 100), BondStatusInChain::Active)),
            (unbonded_just_inside, (bond(SLASHED, 100), BondStatusInChain::Unbonding { unbonded_at: loc(821) })),
            (unbonded_at_window_start, (bond(SLASHED, 100), BondStatusInChain::Unbonding { unbonded_at: loc(820) })),
            (unbonded_before_window, (bond(SLASHED, 100), BondStatusInChain::Unbonding { unbonded_at: loc(750) })),
            (active_elsewhere, (bond(OTHER, 100), BondStatusInChain::Active)),
            (unbonding_elsewhere, (bond(OTHER, 100), BondStatusInChain::Unbonding { unbonded_at: loc(900) })),
            (
                withdrawn_before_window,
                (bond(SLASHED, 100), BondStatusInChain::Withdrawn { withdrawn_at: loc(750), unbonded_at: Some(loc(610)) }),
            ),
            (burned_already, (bond(SLASHED, 100), BondStatusInChain::Burned)),
        ]);

        assert_eq!(burn_set(&bonds), BTreeSet::from([active, unbonded_just_inside]));
    }

    #[test]
    fn no_bond_delegated_inside_window_can_withdraw_before_activation() {
        use zcash_primitives::transaction::{STAKING_ACTION_DELAY, STAKING_DAY_WINDOW, STAKING_PERIOD};
        use super::SLASH_ANALYSIS_WINDOW;

        let is_staking_day = |h: u32| h % STAKING_PERIOD < STAKING_DAY_WINDOW;
        let earliest_withdrawal = |unbonded: u32| (unbonded + STAKING_ACTION_DELAY..).find(|&h| is_staking_day(h)).unwrap();
        for activation in (2..10).map(|k| k * STAKING_PERIOD) {
            let window_start = activation - SLASH_ANALYSIS_WINDOW;
            for unbonded in (window_start + 1..activation).filter(|&h| is_staking_day(h)) {
                let withdrawn = earliest_withdrawal(unbonded);
                assert!(withdrawn >= activation, "unbonded at {unbonded}, withdrawn at {withdrawn}, activation {activation}");
            }
            // The earlier staking day ends just below the window, and it had to stay out.
            assert!(!is_staking_day(window_start) && is_staking_day(window_start - 1));
            assert!(earliest_withdrawal(window_start - 1) < activation);
        }
    }
}
