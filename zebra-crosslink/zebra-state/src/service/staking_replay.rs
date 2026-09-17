//! Rebuilding staking state from blocks, with the rules of the live commit path.
//!
//! Anything that reconstructs bonds from blocks instead of reading them from a `Chain`
//! uses this: the aggregated-stakes repair (`stake_fixup`) and the wallet issuance scan in
//! zebra-crosslink. The live path applies the same three steps in the same order:
//! `Chain::update_chain_tip_with_block` applies each transaction's staking action and then
//! one block reward, and the hardfork slash burns follow at a rule's activation height.
//! A replay that pays the reward per transaction or skips the burns drifts from the stored
//! stakes from the first multi-transaction block on.
//!
//! The coarse entry point is [`StakingReplay::apply_block`]. Callers that already hold
//! transaction hashes use the per-step calls instead.

use std::collections::{BTreeSet, HashMap};

use zcash_primitives::transaction::StakingAction;
use zebra_chain::{
    amount::{Amount, NonNegative, MAX_MONEY},
    block::{Block, Height},
    parameters::HardForkSchedule,
    transaction,
    value_balance::ValueBalance,
};

use crate::{
    constants::POS_BLOCK_REWARD_ZATS,
    service::{
        burn_delegation_bonds,
        finalized_state::{
            disk_format::{BondKey, DelegationBond, TransactionLocation},
            slashing::{OpenSlashRuns, SlashRunTracker},
        },
        non_finalized_state::BondStatusInChain,
        update_bonds_with_pos_issuance, update_chain_tip_with_delegation_bond,
    },
    ValidateContextError,
};

/// Bonds advanced one block at a time from genesis, with the hardfork slash rules they
/// are subject to. Replays must start at genesis: the slash trackers follow delegation
/// runs from there, with no slash index to resume from.
#[derive(Clone, Debug, Default)]
pub struct StakingReplay {
    /// Every bond ever created, with its current amount and status.
    pub delegation_bonds: HashMap<BondKey, (DelegationBond, BondStatusInChain)>,
    /// One tracker per hardfork rule whose activation the replay has not reached yet.
    slash_trackers: Vec<SlashRunTracker>,
}

/// A hardfork slash applied by the replay.
#[derive(Clone, Debug, Default)]
pub struct SlashBurns {
    /// The finalizers the hardfork terminated.
    pub finalizers: BTreeSet<[u8; 32]>,
    /// The bonds burned for delegating to one of them inside the slash window.
    pub burned: BTreeSet<BondKey>,
}

impl StakingReplay {
    /// An empty replay subject to the slash rules of `hardfork_schedule`, which must be the
    /// node's canonical schedule.
    pub fn new(hardfork_schedule: &HardForkSchedule) -> Self {
        let slash_trackers = hardfork_schedule
            .rules()
            .iter()
            .filter(|rule| !rule.terminated_finalizers.is_empty())
            .map(|rule| {
                let activation = u32::try_from(rule.pow_activation_height).expect("activation heights fit a block height");
                let finalizers = rule.terminated_finalizers.iter().map(|finalizer| finalizer.0).collect();
                SlashRunTracker::new(finalizers, Height(activation), OpenSlashRuns::new())
            })
            .collect();
        Self { delegation_bonds: HashMap::new(), slash_trackers }
    }

    /// Applies one block: its staking actions in transaction order, then the block reward,
    /// then any hardfork slash burns activating at `height`.
    ///
    /// Genesis carries no staking state and the live path skips it, so height 0 is a no-op.
    pub fn apply_block(&mut self, height: Height, block: &Block) -> Result<Option<SlashBurns>, ValidateContextError> {
        if height.0 == 0 {
            return Ok(None);
        }

        for (transaction_index, transaction) in block.transactions.iter().enumerate() {
            if let Some(staking_action) = transaction.staking_action() {
                self.apply_staking_action(
                    staking_action,
                    &transaction.hash(),
                    TransactionLocation::from_usize(height, transaction_index),
                )?;
            }
        }

        self.apply_block_reward();
        Ok(self.apply_slash_burns(height))
    }

    /// Applies one transaction's staking action. Call for every staking transaction of a
    /// block, in block order, before [`StakingReplay::apply_block_reward`].
    pub fn apply_staking_action(
        &mut self,
        staking_action: &StakingAction,
        transaction_hash: &transaction::Hash,
        location: TransactionLocation,
    ) -> Result<(), ValidateContextError> {
        // A replay tracks no value pools. Unbonding debits the bonded pool, so seed it with
        // enough balance that the debit cannot fail and abort the replay.
        let mut pools: ValueBalance<NonNegative> = ValueBalance::zero();
        pools.set_staking_bonded_amount(Amount::try_from(MAX_MONEY).expect("constant is in range"));
        // Retargets are recorded only so a reorg can revert them, and a replay never reverts.
        let mut retargets = vec![HashMap::new()];

        update_chain_tip_with_delegation_bond(
            &mut pools,
            &mut self.delegation_bonds,
            &mut retargets,
            staking_action,
            transaction_hash,
            location,
        )?;

        for tracker in &mut self.slash_trackers {
            tracker.apply_staking_action(location.height, staking_action.kind, staking_action.arg32_0, staking_action.arg32_2);
        }
        Ok(())
    }

    /// Pays the block's staking reward, if any bond is active. Exactly once per non-genesis
    /// block, after all of its staking actions and before its slash burns, which still leaves
    /// burned bonds that block's reward.
    pub fn apply_block_reward(&mut self) {
        if self.delegation_bonds.values().any(|(_, status)| *status == BondStatusInChain::Active) {
            update_bonds_with_pos_issuance(POS_BLOCK_REWARD_ZATS, &mut self.delegation_bonds);
        }
    }

    /// Burns the bonds of a hardfork activating exactly at `height`. Call after the block's reward.
    pub fn apply_slash_burns(&mut self, height: Height) -> Option<SlashBurns> {
        let index = self.slash_trackers.iter().position(|tracker| tracker.activation() == height)?;
        let tracker = self.slash_trackers.swap_remove(index);
        let finalizers = tracker.slashed().clone();
        let burned = tracker.burn_set();
        burn_delegation_bonds(&mut self.delegation_bonds, &burned);
        Some(SlashBurns { finalizers, burned })
    }
}
