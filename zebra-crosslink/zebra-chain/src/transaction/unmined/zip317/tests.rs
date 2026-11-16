//! ZIP-317 tests.

use super::{conventional_actions, conventional_fee, mempool_checks, Amount, Error};
use crate::{serialization::ZcashDeserializeInto, transaction::Transaction};

/// A `CreateNewDelegationBond` transaction as the node wallet builds it (two Ironwood actions:
/// the fee input and its change), mined on a local regtest. The wallet paid 15,000 zats:
/// `5,000 * (max(2, 2) + 1)`.
fn regtest_bond_tx() -> Transaction {
    let bytes = hex::decode(include_str!("testdata/regtest_bond_tx.hex").trim()).expect("valid hex");
    bytes.zcash_deserialize_into().expect("a valid regtest transaction")
}

#[test]
fn a_staking_transaction_pays_for_its_ironwood_actions_and_its_staking_action() {
    let tx = regtest_bond_tx();
    assert!(matches!(tx, Transaction::VCrosslink { staking_action: Some(_), .. }));
    let ironwood_actions = tx.ironwood_actions().count() as u32;
    assert_eq!(ironwood_actions, 2);

    assert_eq!(conventional_actions(&tx), 2 + 1);
    assert_eq!(i64::from(conventional_fee(&tx)), 15_000);
}

#[test]
fn a_crosslink_transaction_without_a_staking_action_counts_like_a_v6_one() {
    let mut tx = regtest_bond_tx();
    if let Transaction::VCrosslink { staking_action, .. } = &mut tx {
        *staking_action = None;
    }

    // Never below the grace actions: a zero would make every such transaction "fully paid"
    // at any fee and give it the highest block-production weight.
    assert_eq!(conventional_actions(&tx), 2);
}
#[test]
fn zip317_unpaid_actions_err() {
    let check = mempool_checks(1, Amount::try_from(1).unwrap(), 1);

    assert!(check.is_err());
    assert_eq!(check.err(), Some(Error::UnpaidActions));
}

#[test]
fn zip317_minimum_rate_fee_err() {
    let check = mempool_checks(0, Amount::try_from(1).unwrap(), 1000);

    assert!(check.is_err());
    assert_eq!(check.err(), Some(Error::FeeBelowMinimumRate));
}

#[test]
fn zip317_mempool_checks_ok() {
    assert!(mempool_checks(0, Amount::try_from(100).unwrap(), 1000).is_ok())
}
