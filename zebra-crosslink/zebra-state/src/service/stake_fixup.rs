//! Census and repair of torn `aggregated_stakes_by_hash` rows.
//!
//! Until the stakes snapshot was folded into its block's write batch, a process
//! death between the two writes left a finalized block with no stakes row, and
//! every later BFT decision naming that block aborts the node. This module
//! rebuilds the missing rows by replaying every block's staking actions,
//! rewards, and hardfork slash burns from genesis with the same functions the
//! live commit path uses, and refuses to write unless the replay reproduces
//! every row already on disk. Run it via `zebrad --fixup-db-stake`.

use std::collections::HashMap;

use zebra_chain::{
    block::{self, Height},
    parameters::Network,
};

use crate::{
    constants::{state_database_format_version_in_code, STATE_DATABASE_KIND},
    service::{
        finalized_state::{disk_format::AggregatedStakes, ZebraDb, STATE_COLUMN_FAMILIES_IN_CODE},
        non_finalized_state::BondStatusInChain,
        staking_replay::StakingReplay,
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
    );

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

    let activations: Vec<String> = config
        .hardfork_schedule
        .rules()
        .iter()
        .filter(|rule| {
            !rule.terminated_finalizers.is_empty()
                && rule.pow_activation_height <= u64::from(tip_height.0)
        })
        .map(|rule| rule.pow_activation_height.to_string())
        .collect();
    if !activations.is_empty() {
        println!(
            "replaying hardfork slash burns activating at height(s) {}",
            activations.join(", "),
        );
    }

    println!(
        "replaying staking history from genesis to height {}",
        tip_height.0,
    );
    let mut replay = StakingReplay::new(&config.hardfork_schedule);
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

            if let Some(slash) = replay.apply_block(height, &block)? {
                println!(
                    "applied hardfork slash burns at height {h}: {} bond(s) burned for {} \
                     terminated finalizer(s)",
                    slash.burned.len(),
                    slash.finalizers.len(),
                );
            }
        }

        let mut stakes_by_finalizer: HashMap<[u8; 32], u64> = HashMap::new();
        for (bond, status) in replay.delegation_bonds.values() {
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
