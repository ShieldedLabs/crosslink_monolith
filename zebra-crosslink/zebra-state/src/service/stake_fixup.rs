//! Census and repair of torn `aggregated_stakes_by_hash` rows.
//!
//! Until the stakes snapshot was folded into its block's write batch, a process
//! death between the two writes left a finalized block with no stakes row, and
//! every later BFT decision naming that block aborts the node. This module
//! rebuilds the missing rows by replaying every block's staking actions,
//! rewards, and hardfork slash burns from genesis with the same functions the
//! live commit path uses, and refuses to write unless the replay reproduces
//! every row already on disk. Run it via `zebrad --fixup-db-stake`.

use std::collections::{BTreeSet, HashMap};

use zebra_chain::{
    amount::{Amount, NonNegative, MAX_MONEY},
    block::{self, Height},
    parameters::Network,
    value_balance::ValueBalance,
};

use crate::{
    constants::{state_database_format_version_in_code, POS_BLOCK_REWARD_ZATS, STATE_DATABASE_KIND},
    service::{
        burn_delegation_bonds,
        finalized_state::{
            disk_format::{AggregatedStakes, BondKey, DelegationBond, TransactionLocation},
            slashing::{
                apply_staking_action_to_open_runs, OpenSlashRuns, SlashRunChange,
                SLASH_ANALYSIS_WINDOW,
            },
            ZebraDb, STATE_COLUMN_FAMILIES_IN_CODE,
        },
        non_finalized_state::BondStatusInChain,
        update_bonds_with_pos_issuance, update_chain_tip_with_delegation_bond,
    },
    BoxError, Config, HashOrHeight,
};

/// Checks every finalized block for its aggregated-stakes row, rebuilds any
/// missing rows by replaying the chain's staking history, and writes them back.
///
/// Interactive repair entry point for `zebrad --fixup-db-stake`: progress and
/// findings print to stdout. The state cache must not be open in a node.
///
/// The replayed rows are trustworthy because the replay uses the same staking
/// transition functions as the live commit path — including hardfork slash
/// burns at each rule's activation height — and nothing is written unless the
/// replay also reproduces, exactly, every stakes row already in the database.
/// Existing rows are never modified.
///
/// `config.hardfork_schedule` must hold the same canonical schedule the node
/// runs with, or the replay diverges at the first missing rule's activation.
///
/// `verify` runs the replay cross-check of every stored row even when no rows
/// are missing.
///
/// # Errors
///
/// - The cache is missing blocks or hashes below the finalized tip.
/// - The replay disagrees with any row already on disk.
/// - A repaired row fails to read back.
#[allow(clippy::print_stdout)]
pub fn fixup_aggregated_stakes(
    config: &Config,
    network: &Network,
    verify: bool,
) -> Result<(), BoxError> {
    println!(
        "opening state cache at {:?} (the node must be stopped)",
        config.cache_dir,
    );
    let db = ZebraDb::new(
        config,
        STATE_DATABASE_KIND,
        &state_database_format_version_in_code(),
        network,
        false,
        STATE_COLUMN_FAMILIES_IN_CODE.iter().map(ToString::to_string),
        false,
    )
    .expect("opening the finalized state database failed");

    let Some(tip_height) = db.finalized_tip_height() else {
        println!("the state cache has no finalized blocks; nothing to do");
        return Ok(());
    };

    println!(
        "census: checking aggregated-stakes rows for heights 0..={}",
        tip_height.0,
    );
    let mut missing: Vec<(Height, block::Hash)> = Vec::new();
    for h in 0..=tip_height.0 {
        let height = Height(h);
        let hash = db
            .hash(height)
            .ok_or_else(|| format!("no hash at height {h}, below the finalized tip"))?;
        if db.aggregated_stakes(&hash).is_none() {
            missing.push((height, hash));
        }
    }

    if missing.is_empty() {
        println!("census clean: every block up to the tip has its aggregated-stakes row");
        if !verify {
            return Ok(());
        }
        println!("--verify: replaying anyway to cross-check every stored row");
    }

    for (height, hash) in &missing {
        println!("missing stakes row: height {} hash {hash}", height.0);
    }
    println!("{} missing row(s)", missing.len());
    let heights: Vec<u32> = missing.iter().map(|(height, _)| height.0).collect();
    if heights.windows(2).any(|pair| pair[1] == pair[0] + 1) {
        println!(
            "note: contiguous missing heights suggest OS-level loss of the write-ahead \
             log tail rather than the known torn-write bug",
        );
    }

    // At a rule's activation height A, the live commit path burns every bond
    // that delegated to a terminated finalizer anywhere in [A - W, A), computed
    // from the slash index plus a replay of its unindexed tail
    // (`Chain::slash_window_burns`). This replay starts at genesis, so it sees
    // every delegation run directly and needs no index: closed runs are burned
    // as they close, still-open runs are swept at activation.
    let mut slash_rules: Vec<SlashRule> = config
        .hardfork_schedule
        .rules()
        .iter()
        .filter(|rule| {
            !rule.terminated_finalizers.is_empty()
                && rule.pow_activation_height <= u64::from(tip_height.0)
        })
        .map(|rule| {
            let activation =
                u32::try_from(rule.pow_activation_height).expect("at most the tip height");
            SlashRule {
                activation,
                window_start: activation.saturating_sub(SLASH_ANALYSIS_WINDOW),
                finalizers: rule.terminated_finalizers.iter().map(|f| f.0).collect(),
                open_runs: OpenSlashRuns::new(),
                burned: BTreeSet::new(),
            }
        })
        .collect();
    if !slash_rules.is_empty() {
        let activations: Vec<String> = slash_rules
            .iter()
            .map(|rule| rule.activation.to_string())
            .collect();
        println!(
            "replaying hardfork slash burns activating at height(s) {}",
            activations.join(", "),
        );
    }

    println!(
        "replaying staking history from genesis to height {}",
        tip_height.0,
    );
    let mut bonds: HashMap<BondKey, (DelegationBond, BondStatusInChain)> = HashMap::new();
    let mut fills: Vec<(Height, block::Hash, AggregatedStakes)> = Vec::new();
    let mut mismatches: u32 = 0;

    for h in 0..=tip_height.0 {
        let height = Height(h);
        let hash = db.hash(height).expect("present in census");

        // The genesis block's bond processing is also skipped by the live
        // commit path.
        if h != 0 {
            let block = db
                .block(HashOrHeight::Height(height))
                .ok_or_else(|| format!("no block at height {h}, below the finalized tip"))?;

            // Scratch pools: `update_chain_tip_with_delegation_bond` debits
            // unbonded amounts from the bonded pool, and the real pool values
            // are irrelevant here, so seed enough balance that it cannot fail.
            let mut pools: ValueBalance<NonNegative> = ValueBalance::zero();
            pools.set_staking_bonded_amount(
                Amount::try_from(MAX_MONEY).expect("constant is in range"),
            );
            let mut retargets = vec![HashMap::new()];

            for (transaction_index, transaction) in block.transactions.iter().enumerate() {
                if let Some(staking_action) = transaction.staking_action() {
                    update_chain_tip_with_delegation_bond(
                        &mut pools,
                        &mut bonds,
                        &mut retargets,
                        staking_action,
                        &transaction.hash(),
                        TransactionLocation::from_usize(height, transaction_index),
                    )?;

                    // A rule's burn set is computed from the blocks strictly
                    // below its activation, so this block feeds only the rules
                    // still ahead of it.
                    for rule in slash_rules
                        .iter_mut()
                        .filter(|rule| rule.activation > h)
                    {
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
                }
            }

            if bonds
                .values()
                .any(|(_, status)| *status == BondStatusInChain::Active)
            {
                update_bonds_with_pos_issuance(POS_BLOCK_REWARD_ZATS, &mut bonds);
            }

            // The live path burns after the activation block's own staking
            // actions and rewards (`NonFinalizedState::commit_new_chain`), so
            // the burned bonds still collect this block's reward and this
            // block's snapshot already excludes them.
            for rule in slash_rules
                .iter_mut()
                .filter(|rule| rule.activation == h)
            {
                let mut burn_set = std::mem::take(&mut rule.burned);
                for (bond, (_, start)) in rule.open_runs.iter() {
                    if start.0 < h {
                        burn_set.insert(*bond);
                    }
                }
                burn_delegation_bonds(&mut bonds, &burn_set);
                println!(
                    "applied hardfork slash burns at height {h}: {} bond(s) burned for {} \
                     terminated finalizer(s)",
                    burn_set.len(),
                    rule.finalizers.len(),
                );
            }
        }

        let mut stakes_by_finalizer: HashMap<[u8; 32], u64> = HashMap::new();
        for (bond, status) in bonds.values() {
            if *status == BondStatusInChain::Active {
                let amount: u64 = bond.amount.into();
                *stakes_by_finalizer.entry(bond.target_finalizer).or_insert(0) += amount;
            }
        }
        let mut computed: Vec<([u8; 32], u64)> = stakes_by_finalizer.into_iter().collect();
        computed.sort();

        match db.aggregated_stakes(&hash) {
            Some(mut stored) => {
                stored.sort();
                if stored != computed {
                    mismatches += 1;
                    // Once the replay diverges it usually stays diverged, so
                    // printing every row would flood the terminal for the rest
                    // of the chain.
                    if mismatches <= 5 {
                        println!(
                            "replay mismatch at height {h}: stored {} replayed {}",
                            format_stakes(&stored),
                            format_stakes(&computed),
                        );
                    } else if mismatches == 6 {
                        println!("further mismatches elided; the total is reported at the end");
                    }
                }
            }
            None => fills.push((height, hash, AggregatedStakes(computed))),
        }

        if h % 10_000 == 0 && h != 0 {
            println!("replayed to height {h}");
        }
    }

    if mismatches != 0 {
        return Err(format!(
            "the replay disagreed with {mismatches} row(s) already in the database; \
             nothing was written. This chain's rows are not reproducible from staking \
             actions and rewards alone, so rebuilt rows could not be trusted: copy the \
             missing rows from a healthy node's cache, or resync.",
        )
        .into());
    }

    let stored_row_count = u64::from(tip_height.0) + 1 - fills.len() as u64;
    if fills.is_empty() {
        println!("verified: the replay reproduced all {stored_row_count} stored row(s) exactly");
        return Ok(());
    }

    let mut batch = db.aggregated_stakes_by_hash_cf().new_batch_for_writing();
    for (height, hash, row) in &fills {
        println!(
            "writing stakes row: height {} hash {hash} stakes {}",
            height.0,
            format_stakes(&row.0),
        );
        batch = batch.zs_insert(hash, row);
    }
    batch.write_batch()?;

    for (height, hash, _row) in &fills {
        if db.aggregated_stakes(hash).is_none() {
            return Err(
                format!("the row for height {} did not read back after writing", height.0).into(),
            );
        }
    }
    println!(
        "repaired {} row(s); the replay reproduced all {stored_row_count} existing row(s) exactly",
        fills.len(),
    );
    Ok(())
}

/// One hardfork slash rule active within the replay range, with the delegation
/// runs on its terminated finalizers tracked from genesis.
struct SlashRule {
    activation: u32,
    window_start: u32,
    finalizers: BTreeSet<[u8; 32]>,
    open_runs: OpenSlashRuns,
    burned: BTreeSet<BondKey>,
}

fn format_stakes(stakes: &[([u8; 32], u64)]) -> String {
    if stakes.is_empty() {
        return "(empty)".to_string();
    }
    let entries: Vec<String> = stakes
        .iter()
        .map(|(finalizer, stake)| format!("{}:{stake}", hex::encode(finalizer)))
        .collect();
    format!("[{}]", entries.join(", "))
}
