//! Delegation bond queries that search both finalized and non-finalized state.

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use zcash_primitives::{bft::FinalizerAddress, transaction::StakingAction};

use crate::service::{
    finalized_state::{
        disk_format::{BondKey, BondStatus, DelegationBond},
        zebra_db::delegation::staking_action_finalizer_addresses,
        ZebraDb,
    },
    non_finalized_state::{Chain, NonFinalizedState},
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
                BondStatusInChain::Unbonding { unbonded_at } => BondStatus::Unbonding { unbonded_at: *unbonded_at },
                BondStatusInChain::Withdrawn { withdrawn_at, .. } => BondStatus::Withdrawn { withdrawn_at: *withdrawn_at },
                BondStatusInChain::Burned => BondStatus::Burned,
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
#[allow(dead_code)]
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
#[allow(dead_code)]
pub fn is_bond_unbonding(
    finalized_state: &ZebraDb,
    non_finalized_state: Option<&Chain>,
    bond_key: &BondKey,
) -> bool {
    // Check non-finalized state first
    if let Some(chain) = non_finalized_state {
        use crate::service::non_finalized_state::BondStatusInChain;

        if let Some((_, status)) = chain.delegation_bonds.get(bond_key) {
            return matches!(status, BondStatusInChain::Unbonding { .. });
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
#[allow(dead_code)]
pub fn is_bond_withdrawn(
    finalized_state: &ZebraDb,
    non_finalized_state: Option<&Chain>,
    bond_key: &BondKey,
) -> bool {
    // Check non-finalized state first
    if let Some(chain) = non_finalized_state {
        use crate::service::non_finalized_state::BondStatusInChain;

        if let Some((_, status)) = chain.delegation_bonds.get(bond_key) {
            return matches!(status, BondStatusInChain::Withdrawn { .. });
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
#[allow(dead_code)]
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

/// Resolve each finalizer public key in `keys` to a verified [`FinalizerAddress`], in order.
///
/// A raw public key can never be turned into an address (the address carries the key holder's
/// signature), so a key resolves only if some staking action on chain carried a verified
/// address for it, and resolves to `None` otherwise. Keys are looked up in the finalized
/// `finalizer_address_by_key` index first; the rest are resolved from the staking actions of the
/// retained non-finalized blocks, best chain first and in height order. Side-chain blocks are
/// safe sources: an address is self-certifying, not chain state, so a verified address for a key
/// stays valid whichever branch revealed it. Non-finalized candidates are verified here too.
///
/// Callers must take the `non_finalized_state` snapshot *before* reading `finalized_state`. The
/// write task commits a block to the database before it publishes a non-finalized state without
/// that block, so every block is then visible through at least one of the two views.
pub fn finalizer_addresses(
    non_finalized_state: &NonFinalizedState,
    finalized_state: &ZebraDb,
    keys: &[[u8; 32]],
) -> Vec<Option<FinalizerAddress>> {
    finalizer_addresses_in_chains(non_finalized_state.chain_iter(), finalized_state, keys)
}

/// [`finalizer_addresses`] over an explicit sequence of non-finalized chains, scanned in order.
fn finalizer_addresses_in_chains<'a>(
    chains: impl IntoIterator<Item = &'a Arc<Chain>>,
    finalized_state: &ZebraDb,
    keys: &[[u8; 32]],
) -> Vec<Option<FinalizerAddress>> {
    let mut addresses: Vec<Option<FinalizerAddress>> = keys
        .iter()
        .map(|key| finalized_state.finalizer_address(key))
        .collect();

    let mut missing: HashSet<[u8; 32]> = keys
        .iter()
        .zip(&addresses)
        .filter(|(_, address)| address.is_none())
        .map(|(key, _)| *key)
        .collect();
    if missing.is_empty() {
        return addresses;
    }

    let mut found = HashMap::new();
    for chain in chains {
        let actions = chain
            .blocks
            .values()
            .flat_map(|block| block.block.transactions.iter())
            .filter_map(|tx| tx.staking_action());
        found.extend(first_verified_addresses(actions, &mut missing));
        if missing.is_empty() {
            break;
        }
    }

    for (key, address) in keys.iter().zip(&mut addresses) {
        if address.is_none() {
            *address = found.get(key).copied();
        }
    }
    addresses
}

/// The first verified address `actions` carries for each key in `missing`, in action order.
/// Resolved keys are removed from `missing`; scanning stops once it is empty. Addresses that
/// fail [`FinalizerAddress::verify`] are ignored.
pub(crate) fn first_verified_addresses<'a>(
    actions: impl IntoIterator<Item = &'a StakingAction>,
    missing: &mut HashSet<[u8; 32]>,
) -> HashMap<[u8; 32], FinalizerAddress> {
    let mut found = HashMap::new();
    for address in actions
        .into_iter()
        .flat_map(staking_action_finalizer_addresses)
        .flatten()
    {
        if missing.is_empty() {
            break;
        }
        let key = address.pub_key.0;
        if missing.contains(&key) && address.verify() {
            missing.remove(&key);
            found.insert(key, address);
        }
    }
    found
}

/// The validator set at the current best PoW tip, and the finalizers who signed the fat
/// pointer that tip block carries.
///
/// This is not the BFT roster. That roster is the set at the BFT snapshot, or the last
/// non-empty roster when the snapshot's stakes are empty. Here each entry is a finalizer
/// with stake at the PoW tip itself: active bonds plus that finalizer's reward bank, the
/// same sum the per-block snapshot stores. Zero-stake keys are dropped. Members are sorted
/// by public key so a repeated read of an unchanged tip compares equal.
///
/// `non_finalized_state` must be a snapshot taken before this function reads `db`. The write
/// task commits a block to the database before it publishes a non-finalized state without
/// that block, so the tip is then visible through at least one of the two views (see
/// [`finalizer_addresses`]).
pub fn bc_tip_roster(
    non_finalized_state: &NonFinalizedState,
    db: &ZebraDb,
) -> (
    Vec<zcash_primitives::transaction::RosterMember>,
    Vec<[u8; 32]>,
) {
    // Same tip `read::find::tip` returns: the non-finalized best chain's tip when a chain
    // exists, otherwise the finalized tip. An overlap where the finalized tip is ahead is
    // acceptable either way, matching the other tip readers.
    let chain = non_finalized_state.best_chain();
    let Some((_, hash)) = super::find::tip(chain, db) else {
        return (Vec::new(), Vec::new());
    };

    let stakes = chain
        .and_then(|chain| chain.aggregated_stakes_at(hash))
        .or_else(|| db.aggregated_stakes(&hash))
        .unwrap_or_default();

    let signers = super::find::tip_block(chain, db)
        .filter(|block| block.hash() == hash)
        .map(|block| fat_pointer_signers(block.as_ref()))
        .unwrap_or_default();

    let mut stakes: Vec<_> = stakes
        .into_iter()
        .filter(|(_, power)| *power > 0)
        .collect();
    stakes.sort_by_key(|(key, _)| *key);

    let keys: Vec<[u8; 32]> = stakes.iter().map(|(key, _)| *key).collect();
    let addresses = finalizer_addresses(non_finalized_state, db, &keys);
    let roster = stakes
        .into_iter()
        .zip(addresses)
        .map(|((pub_key, voting_power), finalizer_address)| {
            zcash_primitives::transaction::RosterMember {
                pub_key,
                voting_power,
                txids: Vec::new(),
                finalizer_address,
            }
        })
        .collect();
    (roster, signers)
}

fn fat_pointer_signers(block: &zebra_chain::block::Block) -> Vec<[u8; 32]> {
    block
        .header
        .fat_pointer_to_bft_block
        .signatures
        .iter()
        .map(|sig| sig.pub_key.0)
        .collect()
}

#[cfg(test)]
mod finalizer_address_tests {
    use std::collections::HashSet;

    use zcash_primitives::{
        bft::{finalizer_key_from_seed, FinalizerAddress, TMSig},
        transaction::StakingAction,
    };
    use zebra_chain::parameters::Network;

    use crate::{
        service::{
            finalized_state::{DiskWriteBatch, FinalizedState},
            non_finalized_state::NonFinalizedState,
        },
        Config,
    };

    use std::sync::Arc;

    use zebra_chain::{
        block::{Block, Height},
        parallel::tree::NoteCommitmentTrees,
        parameters::NetworkUpgrade,
        serialization::ZcashDeserializeInto,
        transaction::{LockTime, Transaction},
        value_balance::ValueBalance,
    };

    use crate::{request::ContextuallyVerifiedBlock, service::non_finalized_state::Chain};

    use super::{finalizer_addresses, finalizer_addresses_in_chains, first_verified_addresses};

    fn address(seed: &[u8]) -> FinalizerAddress {
        let (key, _) = finalizer_key_from_seed(seed);
        FinalizerAddress::create(&key)
    }

    fn retarget(from_finalizer: FinalizerAddress, to_finalizer: FinalizerAddress) -> StakingAction {
        StakingAction::RetargetDelegationBond {
            unique_pubkey: [1; 32],
            signature: [0; 64],
            from_finalizer,
            to_finalizer,
            spends: None,
        }
    }

    #[test]
    fn first_verified_addresses_resolves_only_missing_keys_with_valid_signatures() {
        let (a, b, c) = (address(b"a"), address(b"b"), address(b"c"));
        let forged_a = FinalizerAddress { sig: TMSig([9; 64]), ..a };
        let mut missing = HashSet::from([a.pub_key.0, b.pub_key.0]);

        let found = first_verified_addresses(
            &[retarget(c, forged_a), retarget(a, b)],
            &mut missing,
        );

        assert!(missing.is_empty());
        assert_eq!(found.len(), 2);
        assert_eq!(found[&a.pub_key.0], a);
        assert_eq!(found[&b.pub_key.0], b);
        assert!(!found.contains_key(&c.pub_key.0), "c was not asked for");
    }

    #[test]
    fn finalizer_addresses_reads_the_finalized_index_in_request_order() {
        let state = FinalizedState::new(
            &Config::ephemeral(),
            &Network::Mainnet,
            #[cfg(feature = "elasticsearch")]
            false,
        )
        .expect("opening an ephemeral database should succeed");
        let (a, b) = (address(b"a"), address(b"b"));

        let mut batch = DiskWriteBatch::new();
        batch.prepare_finalizer_addresses_batch(&state.db, &[retarget(a, a)]);
        state.db.write_batch(batch).unwrap();

        let non_finalized_state = NonFinalizedState::new(&Network::Mainnet, Default::default());
        let unknown = [7; 32];
        let resolved = finalizer_addresses(
            &non_finalized_state,
            &state.db,
            &[b.pub_key.0, a.pub_key.0, unknown],
        );

        assert_eq!(resolved, vec![None, Some(a), None]);
    }

    /// A chain holding one fake block whose only extra transaction carries `action`.
    fn chain_with_staking_action(action: StakingAction) -> Arc<Chain> {
        let mut block: Block = zebra_test::vectors::BLOCK_MAINNET_419200_BYTES
            .zcash_deserialize_into()
            .unwrap();
        block.transactions.push(Arc::new(Transaction::VCrosslink {
            network_upgrade: NetworkUpgrade::Nu6,
            lock_time: LockTime::unlocked(),
            expiry_height: Height(0),
            inputs: Vec::new(),
            outputs: Vec::new(),
            sapling_shielded_data: None,
            orchard_shielded_data: None,
            ironwood_shielded_data: None,
            staking_action: Some(action),
        }));
        let block = ContextuallyVerifiedBlock::test_with_zero_spent_utxos(Arc::new(block));

        let mut chain = Chain::new(
            &Network::Mainnet,
            Height(419_199),
            NoteCommitmentTrees::default(),
            Default::default(),
            ValueBalance::zero(),
            std::iter::empty(),
            std::iter::empty(),
        );
        chain.blocks.insert(block.height, block);
        Arc::new(chain)
    }

    #[test]
    fn finalizer_addresses_fall_back_to_non_finalized_blocks() {
        let state = FinalizedState::new(
            &Config::ephemeral(),
            &Network::Mainnet,
            #[cfg(feature = "elasticsearch")]
            false,
        )
        .expect("opening an ephemeral database should succeed");
        let (a, b, c) = (address(b"a"), address(b"b"), address(b"c"));

        // `a` is finalized; `b` only appears in the best chain, `c` only in a side chain.
        let mut batch = DiskWriteBatch::new();
        batch.prepare_finalizer_addresses_batch(&state.db, &[retarget(a, a)]);
        state.db.write_batch(batch).unwrap();
        let best = chain_with_staking_action(retarget(b, b));
        let side = chain_with_staking_action(retarget(FinalizerAddress { sig: TMSig([9; 64]), ..a }, c));

        let resolved = finalizer_addresses_in_chains(
            [&best, &side],
            &state.db,
            &[a.pub_key.0, b.pub_key.0, c.pub_key.0, [7; 32]],
        );

        assert_eq!(resolved, vec![Some(a), Some(b), Some(c), None]);
    }
}
