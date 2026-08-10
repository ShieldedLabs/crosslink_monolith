//! Synchronous, executor-free block verification.
//!
//! Zebra's normal path ([`crate::block::SemanticBlockVerifier`]) is a tower `Service` that
//! returns a boxed future, and its crypto goes through `tower_batch_control::Batch`
//! services which accumulate items across *independent callers* under a 64-item / 100ms
//! policy (see `MAX_BATCH_SIZE` / `MAX_BATCH_LATENCY` in `primitives.rs`). That policy is
//! correct for Zebra, which verifies many blocks concurrently behind a download lookahead.
//!
//! It is the worst case for a caller that commits one block at a time down a serially
//! dependent chain: a single block rarely contributes 64 items, so every block waits out
//! the full 100ms latency timer and then verifies a batch of two or three — paying all of
//! the latency for almost none of the batching.
//!
//! These functions do the same checks, in the same order, on the same data, with the batch
//! boundary set to *one block* instead of "whatever arrived in 100ms". Nothing here is a
//! new consensus rule:
//!
//! - [`block_check_cheap`] calls straight into [`crate::block::check`], in the same order
//!   as `SemanticBlockVerifier::call`.
//! - [`block_verify_shielded_batched`] feeds the same `sapling_crypto` / `orchard` batch
//!   validators that the `Batch` services feed, just queued directly and flushed once,
//!   with no timer, no channel, and no executor.
//!
//! Within-block batching keeps nearly all of the algorithmic win (a batch verification is
//! a single multi-scalar multiplication rather than N independent ones, so per-item cost
//! falls as the batch grows). What is given up is cross-block amortization, which is worth
//! close to nothing when blocks are verified one at a time anyway.
//!
//! @Note: batch verification is all-or-nothing — a failed batch does not say which item
//!        failed. Zebra wraps its batch services in `tower_fallback::Fallback` to re-verify
//!        individually and attribute the failure. Block verification does not need that:
//!        any invalid signature or proof invalidates the whole block. Attribution only
//!        matters for the mempool, so there is no fallback path here.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use rand::thread_rng;

use zebra_state::new_network::{BlockVerifyError, CheapBlockChecks};

use zebra_chain::{
    amount::{Amount, NonNegative},
    block::{self, Block, Height},
    parameters::{Network, NetworkUpgrade},
    transaction::{self, HashType, Transaction},
    transparent,
    work::equihash,
};
use zebra_script::{CachedFfiTransaction, Sigops};

use crate::{
    block::{check, VerifyBlockError, MAX_BLOCK_SIGOPS},
    error::BlockError,
    groth16::SAPLING,
    primitives::halo2::VERIFYING_KEY,
    transaction as tx,
};


/// Bridge from the consensus error types to the shared, crate-agnostic one.
///
/// `misbehavior_score` is carried across so the caller can weight or drop a peer without
/// having to name `VerifyBlockError` (which lives in a crate zebra-state cannot depend on).
impl From<VerifyBlockError> for BlockVerifyError {
    fn from(err: VerifyBlockError) -> Self {
        BlockVerifyError {
            misbehavior_score: err.misbehavior_score(),
            msg: err.to_string(),
        }
    }
}

/// The header-only checks: proof of work, difficulty, and header time.
///
/// Split out from the body checks so a caller reassembling a block from the network can run
/// these on the first fragment — the header is 1487 bytes — and drop a bad-PoW block without
/// downloading the rest. Everything after this point is protected by proof-of-work cost.
///
/// `alleged_height` is the height claimed by whoever supplied the block. It is NOT trusted
/// here: difficulty depends on height, so one is needed, but the value is only bound to the
/// header once [`block_check_body`] validates the merkle root and re-derives the height from
/// the coinbase transaction. A caller that runs this early MUST also run `block_check_body`
/// before acting on anything height-dependent.
///
/// `check_pow` is `false` for block proposals and for networks with PoW disabled — matching
/// the `request.is_proposal() || network.disable_pow()` branch in `SemanticBlockVerifier`.
///
/// `now` is passed in rather than read from the clock inside, so the caller controls it.
pub fn block_check_header(
    header: &block::Header,
    network: &Network,
    alleged_height: Height,
    now: DateTime<Utc>,
    check_pow: bool,
) -> Result<(), BlockVerifyError> {
    let hash = header.hash();

    if alleged_height > Height::MAX {
        Err(VerifyBlockError::from(BlockError::MaxHeight(
            alleged_height,
            hash,
            Height::MAX,
        )))?;
    }

    // @Volatile: the order below is load-bearing and must match `SemanticBlockVerifier`.
    // Difficulty goes first deliberately — it raises the cost of attacks that manipulate any
    // of the other fields, so it must not be reordered for tidiness.
    if check_pow {
        check::difficulty_is_valid(header, network, &alleged_height, &hash)
            .map_err(VerifyBlockError::from)?;
        check::equihash_solution_is_valid(header).map_err(VerifyBlockError::from)?;
    } else {
        check::difficulty_threshold_is_valid(header, network, &alleged_height, &hash)
            .map_err(VerifyBlockError::from)?;
    }

    check::time_is_valid_at(header, now, &alleged_height, &hash).map_err(VerifyBlockError::Time)?;

    Ok(())
}

/// The body checks: bind the transactions to the header, then the block-level subsidy rules.
///
/// The merkle check here is what makes the rest of the block trustworthy — until it passes,
/// the transactions (and therefore `coinbase_height()`) are not committed to by the PoW'd
/// header and an attacker can vary them freely. Anything that consumes the height, including
/// the crosslink fat-pointer gate, must run after this.
pub fn block_check_body(
    block: &Block,
    network: &Network,
) -> Result<CheapBlockChecks, BlockVerifyError> {
    let hash = block.hash();

    let height = block
        .coinbase_height()
        .ok_or(BlockError::MissingHeight(hash))
        .map_err(VerifyBlockError::from)?;

    // Computed once here and handed back: the merkle check needs them now, sighashing needs
    // them later, and re-deriving them means re-hashing every transaction in the block.
    let transaction_hashes: Arc<[_]> = block.transactions.iter().map(|t| t.hash()).collect();

    check::merkle_root_validity(network, block, &transaction_hashes)
        .map_err(VerifyBlockError::from)?;

    let coinbase_tx = check::coinbase_is_first(block).map_err(VerifyBlockError::from)?;

    let expected_block_subsidy = zebra_chain::parameters::subsidy::block_subsidy(height, network)
        .map_err(|e| VerifyBlockError::from(BlockError::from(e)))?;

    let deferred_pool_balance_change =
        check::subsidy_is_valid(block, network, expected_block_subsidy)
            .map_err(VerifyBlockError::from)?;

    tx::check::coinbase_outputs_are_decryptable(&coinbase_tx, network, height)
        .map_err(VerifyBlockError::Transaction)?;

    Ok(CheapBlockChecks {
        hash,
        height,
        transaction_hashes,
        coinbase_tx,
        expected_block_subsidy,
        deferred_pool_balance_change,
    })
}

/// Header and body checks together, in `SemanticBlockVerifier` order.
///
/// Convenience for callers that already hold the whole block and have no reason to split.
pub fn block_check_cheap(
    block: &Block,
    network: &Network,
    now: DateTime<Utc>,
    check_pow: bool,
) -> Result<CheapBlockChecks, BlockVerifyError> {
    // The height is re-derived and bound to the header inside `block_check_body`; passing it
    // here only satisfies the difficulty check's need for a height.
    let alleged_height = block
        .coinbase_height()
        .ok_or(BlockError::MissingHeight(block.hash()))
        .map_err(VerifyBlockError::from)?;

    block_check_header(&block.header, network, alleged_height, now, check_pow)?;
    block_check_body(block, network)
}

/// The expensive per-block verification: transparent scripts, sigops, fees, and the shielded
/// proofs and signatures batched once per proof system.
///
/// Walks the block a single time. Per transaction it runs the synchronous work directly —
/// script verification, sigop counting, fee accounting — and *collects* the shielded bundles
/// without verifying them. Only after the walk are the Sapling and Orchard batches flushed,
/// once each, for the whole block.
///
/// That is the same work Zebra's `Batch` services do, with the accumulation boundary moved
/// from "64 items or 100ms, across all concurrent callers" to "this block". A caller
/// committing one block at a time down a serially dependent chain never fills a 64-item batch,
/// so under the original policy it waits out the full latency timer and then verifies a batch
/// of two or three. Here there is no timer at all, and within-block batching still collapses
/// N verifications into one multi-scalar multiplication.
///
/// `lookup_utxo` resolves transparent outputs spent by this block that were created by
/// *earlier* blocks. Outputs created within this block are resolved internally and never reach
/// the closure. The caller owning the lookup is what lets this function be synchronous and
/// hold no state handle.
///
/// @Note: this is a *load*, not the spend check. Whether a spend is legal — the output exists,
///        is unspent, is correctly ordered within the chain, and is a mature coinbase — is
///        contextual, and is decided by `check::utxo::transparent_spend` in the state's write
///        task. That check is already synchronous; nothing here duplicates or replaces it.
///
/// @Note: batch verification is all-or-nothing — a failed batch does not say which item failed.
///        Zebra wraps its batch services in `tower_fallback::Fallback` to re-verify individually
///        and attribute the failure. Block verification does not need that: any invalid
///        signature or proof invalidates the whole block. Attribution only matters for the
///        mempool, so there is no fallback path here.
pub fn block_verify_expensive(
    block: &Block,
    network: &Network,
    cheap: &CheapBlockChecks,
    lookup_utxo: &dyn Fn(&transparent::OutPoint) -> Option<transparent::Utxo>,
) -> Result<HashMap<transparent::OutPoint, transparent::OrderedUtxo>, BlockVerifyError> {
    let nu = NetworkUpgrade::current(network, cheap.height);

    // Outputs created by earlier transactions in this same block. Spends of these can never be
    // resolved by the caller — they are not in any committed chain yet.
    let known_utxos = transparent::new_ordered_outputs(block, &cheap.transaction_hashes);

    let mut sapling_bundles = Vec::new();
    let mut orchard_bundles = Vec::new();

    let mut block_sigops: u32 = 0;
    let mut block_miner_fees = Amount::<NonNegative>::zero();

    for tx in block.transactions.iter() {
        // Resolve the outputs this transaction spends. Coinbase inputs have a null prevout and
        // spend nothing, so they contribute no entries.
        let mut spent_utxos = HashMap::new();
        let mut spent_outputs = Vec::with_capacity(tx.inputs().len());
        for input in tx.inputs() {
            let outpoint = match input {
                transparent::Input::PrevOut { outpoint, .. } => outpoint,
                transparent::Input::Coinbase { .. } => continue,
            };

            let utxo = known_utxos
                .get(outpoint)
                .map(|ordered| ordered.utxo.clone())
                .or_else(|| lookup_utxo(outpoint))
                .ok_or_else(|| BlockVerifyError {
                    msg: format!("could not load the output spent by transparent input {outpoint:?}"),
                    misbehavior_score: 0,
                })?;

            spent_outputs.push(utxo.output.clone());
            spent_utxos.insert(*outpoint, utxo);
        }

        // The sighash for v5+ binds the spent outputs (ZIP-244), so this has to be built even
        // for a shielded-only transaction.
        let cached = CachedFfiTransaction::new(tx.clone(), Arc::new(spent_outputs), nu)
            .map_err(|_| BlockVerifyError {
                msg: format!("transaction is not supported by network upgrade {nu:?}"),
                misbehavior_score: 100,
            })?;

        // Transparent scripts. `script::Verifier`'s tower wrapper does nothing but call this;
        // the work was always synchronous.
        if !tx.is_coinbase() {
            for input_index in 0..tx.inputs().len() {
                cached.is_valid(input_index).map_err(|err| BlockVerifyError {
                    msg: format!("script verification failed for input {input_index}: {err}"),
                    misbehavior_score: 100,
                })?;
            }
        }

        block_sigops = block_sigops
            .checked_add(tx.sigops().map_err(|err| BlockVerifyError {
                msg: format!("could not count sigops: {err}"),
                misbehavior_score: 100,
            })?)
            .ok_or_else(|| BlockVerifyError {
                msg: "sigop count overflowed".to_string(),
                misbehavior_score: 100,
            })?;

        // Coinbase transactions consume the miner fee, so they add nothing to the block total.
        if !tx.is_coinbase() {
            let value_balance = tx.value_balance(&spent_utxos).map_err(|_| BlockVerifyError {
                msg: "incorrect fee: could not compute the transaction value balance".to_string(),
                misbehavior_score: 100,
            })?;
            let miner_fee = value_balance.remaining_transaction_value().map_err(|_| BlockVerifyError {
                msg: "incorrect fee: negative remaining transaction value".to_string(),
                misbehavior_score: 100,
            })?;
            block_miner_fees = (block_miner_fees + miner_fee).map_err(|err| BlockVerifyError {
                msg: format!("summing miner fees overflowed: {err}"),
                misbehavior_score: 100,
            })?;
        }

        let sighasher = cached.sighasher();
        let sighash = sighasher.sighash(HashType::ALL, None);

        if let Some(bundle) = sighasher.sapling_bundle() {
            sapling_bundles.push((bundle, sighash));
        }
        if let Some(bundle) = sighasher.orchard_bundle() {
            orchard_bundles.push((bundle, sighash));
        }
    }

    // Block-level totals.
    if block_sigops > MAX_BLOCK_SIGOPS {
        return Err(VerifyBlockError::from(BlockError::TooManyTransparentSignatureOperations {
            height: cheap.height,
            hash: cheap.hash,
            sigops: block_sigops,
        })
        .into());
    }

    check::miner_fees_are_valid(
        &cheap.coinbase_tx,
        cheap.height,
        block_miner_fees,
        cheap.expected_block_subsidy,
        cheap.deferred_pool_balance_change,
        network,
    )
    .map_err(VerifyBlockError::from)?;

    // One flush per proof system for the whole block. No timer, no channel, no executor.
    if !sapling_bundles.is_empty() {
        let mut validator = sapling_crypto::BatchValidator::new();

        for (bundle, sighash) in sapling_bundles {
            // check_bundle does the structural/queueing half and can reject immediately.
            if !validator.check_bundle(bundle, sighash.into()) {
                return Err(BlockVerifyError { msg: "invalid Sapling bundle in block".to_string(), misbehavior_score: 100 });
            }
        }

        let (spend_vk, output_vk) = SAPLING.verifying_keys();
        if !validator.validate(&spend_vk, &output_vk, thread_rng()) {
            return Err(BlockVerifyError { msg: "invalid Sapling bundle in block".to_string(), misbehavior_score: 100 });
        }
    }

    if !orchard_bundles.is_empty() {
        let mut validator = orchard::bundle::BatchValidator::new();

        for (bundle, sighash) in orchard_bundles {
            validator.add_bundle(&bundle, sighash.0);
        }

        if !validator.validate(&VERIFYING_KEY, thread_rng()) {
            return Err(BlockVerifyError { msg: "invalid Orchard bundle in block".to_string(), misbehavior_score: 100 });
        }
    }

    Ok(known_utxos)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zebra_chain::serialization::ZcashDeserialize;

    /// The cheap checks must accept every known-good mainnet block vector the semantic path
    /// can verify. Heights below Canopy activation are excluded: `subsidy_is_valid` has an
    /// explicit `unreachable!` for them (pre-Canopy funding rules were never implemented,
    /// those heights are checkpoint-only territory), so `SemanticBlockVerifier` cannot
    /// verify them either and acceptance would be unfaithful.
    ///
    /// This is the faithfulness test for the phase-2 hoist: these blocks are all real and all
    /// valid, so any rejection here means a check was reordered, dropped, or given the wrong
    /// inputs relative to `SemanticBlockVerifier`.
    #[test]
    fn cheap_checks_accept_all_mainnet_vectors() {
        let network = Network::Mainnet;
        let canopy_height = NetworkUpgrade::Canopy
            .activation_height(&network)
            .expect("Canopy activation height is known on mainnet");
        let mut checked = 0;

        for (height, bytes) in zebra_test::vectors::MAINNET_BLOCKS.iter() {
            if Height(*height) < canopy_height {
                continue;
            }

            let block = Block::zcash_deserialize(&bytes[..]).expect("test vector is a valid block");

            // `time_is_valid_at` rejects blocks more than 2h ahead of `now`, and these vectors
            // are historical, so anchor `now` to each block's own header time.
            let now = block.header.time;

            let cheap = block_check_cheap(&block, &network, now, true)
                .unwrap_or_else(|e| panic!("mainnet block {height} was rejected: {}", e.msg));

            assert_eq!(cheap.height.0, *height, "height mismatch for block {height}");
            assert_eq!(cheap.hash, block.hash(), "hash mismatch for block {height}");
            assert_eq!(
                cheap.transaction_hashes.len(),
                block.transactions.len(),
                "transaction hash count mismatch for block {height}"
            );
            checked += 1;
        }

        // 9 post-Canopy vectors exist today; the floor guards against a filter change
        // silently emptying the loop, not against the vector set shrinking.
        assert!(checked >= 5, "expected several post-Canopy vectors, got {checked}");
    }

    /// A block whose PoW does not meet the difficulty threshold must be rejected, and the
    /// rejection must carry a misbehaviour score (these are peer-attributable failures).
    #[test]
    fn cheap_checks_reject_tampered_pow() {
        let network = Network::Mainnet;
        let bytes = zebra_test::vectors::MAINNET_BLOCKS
            .get(&1)
            .expect("block 1 vector exists");
        let mut block = Block::zcash_deserialize(&bytes[..]).expect("valid block");

        // Bump the nonce: the header no longer hashes below its own difficulty threshold.
        block.header = Arc::new({
            let mut header = block::Header::clone(&block.header);
            header.nonce = [0xff; 32].into();
            header
        });

        let err = block_check_cheap(&block, &network, block.header.time, true)
            .expect_err("tampered proof of work must be rejected");
        assert!(err.misbehavior_score > 0, "PoW failure should be peer-attributable: {err:?}");
    }
}
