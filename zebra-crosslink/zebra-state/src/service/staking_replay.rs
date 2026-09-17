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
            slashing::{apply_staking_action_to_open_runs, OpenSlashRuns, SlashRunChange, SLASH_ANALYSIS_WINDOW},
        },
        non_finalized_state::BondStatusInChain,
        update_bonds_with_pos_issuance, update_chain_tip_with_delegation_bond,
    },
    ValidateContextError,
};

/// One hardfork slash rule, with the delegation runs onto its terminated finalizers
/// tracked from genesis.
#[derive(Clone, Debug)]
struct SlashRule {
    activation: u32,
    window_start: u32,
    finalizers: BTreeSet<[u8; 32]>,
    open_runs: OpenSlashRuns,
    burned: BTreeSet<BondKey>,
}

/// Bonds advanced one block at a time from genesis, with the hardfork slash rules they
/// are subject to. Replays must start at genesis: the slash rules track delegation runs
/// from there.
#[derive(Clone, Debug, Default)]
pub struct StakingReplay {
    /// Every bond ever created, with its current amount and status.
    pub delegation_bonds: HashMap<BondKey, (DelegationBond, BondStatusInChain)>,
    slash_rules: Vec<SlashRule>,
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
                SlashRule {
                    activation,
                    window_start: activation.saturating_sub(SLASH_ANALYSIS_WINDOW),
                    finalizers: rule.terminated_finalizers.iter().map(|finalizer| finalizer.0).collect(),
                    open_runs: OpenSlashRuns::new(),
                    burned: BTreeSet::new(),
                }
            })
            .collect();
        Self { delegation_bonds: HashMap::new(), slash_rules }
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

        // A rule's burn set is computed from the blocks strictly below its activation, so
        // this action feeds only the rules still ahead of it.
        let height = location.height;
        for rule in self.slash_rules.iter_mut().filter(|rule| rule.activation > height.0) {
            for change in apply_staking_action_to_open_runs(
                &mut rule.open_runs,
                &rule.finalizers,
                height,
                staking_action.kind,
                staking_action.arg32_0,
                staking_action.arg32_2,
            ) {
                if let SlashRunChange::Close(key, end) = change {
                    if end.0 > rule.window_start {
                        rule.burned.insert(key.bond);
                    }
                }
            }
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

    /// Burns the bonds of a hardfork activating exactly at `height`: runs closed inside its
    /// window and runs still open at activation. Call after the block's reward.
    pub fn apply_slash_burns(&mut self, height: Height) -> Option<SlashBurns> {
        let rule = self.slash_rules.iter_mut().find(|rule| rule.activation == height.0)?;
        let mut burned = std::mem::take(&mut rule.burned);
        for (bond, (_, start)) in rule.open_runs.iter() {
            if start.0 < height.0 {
                burned.insert(*bond);
            }
        }
        let finalizers = rule.finalizers.clone();
        burn_delegation_bonds(&mut self.delegation_bonds, &burned);
        Some(SlashBurns { finalizers, burned })
    }
}
