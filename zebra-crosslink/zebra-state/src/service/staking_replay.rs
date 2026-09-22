//! Rebuilding staking state from blocks, with the rules of the live commit path.
//!
//! Anything that reconstructs bonds from blocks instead of reading them from a `Chain`
//! uses this: the aggregated-stakes repair (`stake_fixup`) and the wallet issuance scan in
//! zebra-crosslink. The live path applies the same three steps in the same order:
//! `Chain::update_chain_tip_with_block_except_trees` applies each transaction's staking
//! action and then the block reward if the block pays one, and the hardfork slash burns
//! follow at a rule's activation height. A replay that pays the reward per transaction or
//! skips the burns drifts from the stored stakes from the first multi-transaction block on.
//!
//! Whether a block pays the reward depends on the BFT certificate it carries, which the
//! callers see differently, so they decide it and pass it in.
//!
//! The coarse entry point is [`StakingReplay::apply_block`]. Callers that already hold
//! transaction hashes, or that cannot read the slash window synchronously, use the
//! per-step calls instead.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

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
            slashing::{slash_burn_set, SLASH_ANALYSIS_WINDOW},
        },
        non_finalized_state::BondStatusInChain,
        update_bonds_with_pos_issuance, update_chain_tip_with_delegation_bond,
    },
    ValidateContextError,
};

/// Bonds and finalizer reward banks advanced one block at a time from genesis, with the
/// hardfork slash rules they are subject to.
#[derive(Clone, Debug, Default)]
pub struct StakingReplay {
    /// Every bond ever created, with its current amount and status.
    pub delegation_bonds: HashMap<BondKey, (DelegationBond, BondStatusInChain)>,
    /// Each finalizer's commission bank, a virtual bond on that finalizer.
    pub finalizer_rewards: HashMap<[u8; 32], u64>,
    /// `(activation, terminated finalizers)` for every hardfork rule that slashes.
    slash_rules: Vec<(Height, BTreeSet<[u8; 32]>)>,
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
        let slash_rules = hardfork_schedule
            .rules()
            .iter()
            .filter(|rule| !rule.terminated_finalizers.is_empty())
            .map(|rule| {
                let activation = u32::try_from(rule.pow_activation_height).expect("activation heights fit a block height");
                (Height(activation), rule.terminated_finalizers.iter().map(|finalizer| finalizer.0).collect())
            })
            .collect();
        Self { delegation_bonds: HashMap::new(), finalizer_rewards: HashMap::new(), slash_rules }
    }

    /// Applies one block: its staking actions in transaction order, then the block reward if
    /// `pays_reward`, then any hardfork slash burns activating at `height`. `block_at` must
    /// return the block at any height in the slash window below and including `height`.
    ///
    /// Genesis carries no staking state and the live path skips it, so height 0 is a no-op.
    pub fn apply_block(
        &mut self,
        height: Height,
        block: &Block,
        pays_reward: bool,
        block_at: impl FnMut(Height) -> Arc<Block>,
    ) -> Result<Option<SlashBurns>, ValidateContextError> {
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

        if pays_reward {
            self.apply_block_reward();
        }
        if !self.slash_activates_at(height) {
            return Ok(None);
        }
        Ok(self.apply_slash_burns(height, slash_window(height).map(block_at)))
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
            &mut self.finalizer_rewards,
            staking_action,
            transaction_hash,
            location,
        )
    }

    /// Pays the block's staking reward. Call once per paying non-genesis block, after all of
    /// its staking actions and before its slash burns, which still leaves burned bonds that
    /// block's reward.
    pub fn apply_block_reward(&mut self) {
        update_bonds_with_pos_issuance(POS_BLOCK_REWARD_ZATS, &mut self.delegation_bonds, &mut self.finalizer_rewards);
    }

    /// Whether a hardfork slash activates at `height`, so the caller must read its window.
    pub fn slash_activates_at(&self, height: Height) -> bool {
        self.slash_rules.iter().any(|(activation, _)| *activation == height)
    }

    /// Burns the bonds of every hardfork activating exactly at `height`. `window_blocks`
    /// must yield the blocks at [`slash_window`]`(height)`, in any order. Call after the
    /// block's reward.
    pub fn apply_slash_burns(
        &mut self,
        height: Height,
        window_blocks: impl IntoIterator<Item = Arc<Block>>,
    ) -> Option<SlashBurns> {
        let mut finalizers = BTreeSet::new();
        for (activation, rule_finalizers) in &self.slash_rules {
            if *activation == height {
                finalizers.extend(rule_finalizers.iter().copied());
            }
        }
        if finalizers.is_empty() {
            return None;
        }
        let burned = slash_burn_set(&self.delegation_bonds, window_blocks, &finalizers, height);
        burn_delegation_bonds(&mut self.delegation_bonds, &burned);
        Some(SlashBurns { finalizers, burned })
    }
}

/// The heights whose blocks decide the burns of a slash activating at `activation`.
pub fn slash_window(activation: Height) -> impl Iterator<Item = Height> {
    (activation.0.saturating_sub(SLASH_ANALYSIS_WINDOW) + 1..=activation.0).map(Height)
}
