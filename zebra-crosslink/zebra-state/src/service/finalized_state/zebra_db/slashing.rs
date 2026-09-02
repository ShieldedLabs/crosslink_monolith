//! Hardfork slash burns.
//!
//! To burn the bonds of a finalizer `T` slashed at activation height `A`, we must
//! find every bond delegated to `T` at any point in the window `(A - W, A]`. This
//! includes bonds still pointing at `T` (sitting ducks) and bonds that retargeted
//! or unbonded away from `T` inside the window (cockroaches/fleers).
//!
//! Because a Retarget action names both its `from` and `to` finalizers, the whole
//! computation is lazy and local: wait until the activation block, then combine
//! the current bond state (which names every bond still parked on, or unbonding
//! from, a terminated finalizer) with a read of the W blocks below activation
//! (whose Retarget `from`s name every bond that left one inside the window).
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
/// to a finalizer in `slashed` at any point in `(activation - W, activation]`.
///
/// `bonds` is the bond state *after* the activation block's staking actions (the
/// live commit path burns after applying the block), and `window_blocks` yields
/// the blocks at heights `(activation - W, activation]`, in any order — no state
/// is threaded between them.
///
/// Every delegation stretch onto a slashed finalizer is caught by exactly one of
/// two checks:
/// - the stretch reaches the present: the bond still targets `T` in `bonds`,
///   either Active or Unbonding (unbonding keeps the target; its `created_at`
///   was rewritten to the unbonding location, which dates the stretch's end, so
///   a bond that unbonded at or before the window start is spared);
/// - the stretch ended with an in-window Retarget: that action's `from` is `T`.
/// A stretch that *began* in the window needs no check of its own — it either
/// still stands (first case) or ended by retarget (second) or by unbonding
/// (first, via the kept target). Withdrawal can't end an in-window stretch:
/// withdrawing takes longer than W after unbonding.
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
            BondStatusInChain::Unbonding => bond.created_at.height.0 > window_start,
            BondStatusInChain::Withdrawn | BondStatusInChain::Burned => false,
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
