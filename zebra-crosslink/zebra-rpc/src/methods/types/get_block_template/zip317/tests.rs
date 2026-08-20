//! Tests for ZIP-317 transaction selection for block template production

use zcash_keys::address::Address;

use zcash_transparent::address::TransparentAddress;
use zebra_chain::{
    block::{FatPointerSignature, FatPointerToBftBlock, Height, PubKeyID},
    parameters::Network,
    transaction,
    transparent::OutPoint,
};
use zebra_node_services::mempool::TransactionDependencies;

use super::{non_transaction_block_bytes, select_mempool_transactions};

/// The bytes reserved for the header and transaction count must cover what
/// `Block::zcash_serialize` writes around the transactions, otherwise a full template
/// produces a block that no node can deserialize.
#[test]
fn reserves_header_and_transaction_count_bytes() {
    // 140 bytes of fixed-size header fields, a 1344-byte solution behind its 3-byte
    // length, an empty fat pointer, and the transaction count.
    let empty = non_transaction_block_bytes(&FatPointerToBftBlock::null());
    assert_eq!(empty, 140 + 3 + 1344 + 44 + 2 + 3);

    // Each signature adds a 32-byte key and a 64-byte signature.
    let signed = FatPointerToBftBlock {
        vote_for_block_without_finalizer_public_key: [0; 44],
        signatures: vec![
            FatPointerSignature {
                pub_key: PubKeyID([0; 32]),
                vote_signature: [0; 64],
            };
            12
        ],
    };
    assert_eq!(non_transaction_block_bytes(&signed), empty + 12 * 96);
}

#[test]
fn excludes_tx_with_unselected_dependencies() {
    let network = Network::Mainnet;
    let next_block_height = Height(1_000_000);
    let extra_coinbase_data = Vec::new();
    let mut mempool_tx_deps = TransactionDependencies::default();
    let miner_address = Address::from(TransparentAddress::PublicKeyHash([0x7e; 20]));

    let unmined_tx = network
        .unmined_transactions_in_blocks(..)
        .next()
        .expect("should not be empty");

    mempool_tx_deps.add(
        unmined_tx.transaction.id.mined_id(),
        vec![OutPoint::from_usize(transaction::Hash([0; 32]), 0)],
    );

    assert_eq!(
        select_mempool_transactions(
            &network,
            next_block_height,
            &miner_address,
            vec![unmined_tx],
            mempool_tx_deps,
            extra_coinbase_data,
            &FatPointerToBftBlock::null(),
        ),
        vec![],
        "should not select any transactions when dependencies are unavailable"
    );
}

#[test]
fn includes_tx_with_selected_dependencies() {
    let network = Network::Mainnet;
    let next_block_height = Height(1_000_000);
    let unmined_txs: Vec<_> = network.unmined_transactions_in_blocks(..).take(3).collect();
    let miner_address = Address::from(TransparentAddress::PublicKeyHash([0x7e; 20]));

    let dependent_tx1 = unmined_txs.first().expect("should have 3 txns");
    let dependent_tx2 = unmined_txs.get(1).expect("should have 3 txns");
    let independent_tx_id = unmined_txs
        .get(2)
        .expect("should have 3 txns")
        .transaction
        .id
        .mined_id();

    let mut mempool_tx_deps = TransactionDependencies::default();
    mempool_tx_deps.add(
        dependent_tx1.transaction.id.mined_id(),
        vec![OutPoint::from_usize(independent_tx_id, 0)],
    );
    mempool_tx_deps.add(
        dependent_tx2.transaction.id.mined_id(),
        vec![
            OutPoint::from_usize(independent_tx_id, 0),
            OutPoint::from_usize(transaction::Hash([0; 32]), 0),
        ],
    );

    let extra_coinbase_data = Vec::new();

    let selected_txs = select_mempool_transactions(
        &network,
        next_block_height,
        &miner_address,
        unmined_txs.clone(),
        mempool_tx_deps.clone(),
        extra_coinbase_data,
        &FatPointerToBftBlock::null(),
    );

    assert_eq!(
        selected_txs.len(),
        2,
        "should select the independent transaction and 1 of the dependent txs, selected: {selected_txs:?}"
    );

    let selected_tx_by_id = |id| {
        selected_txs
            .iter()
            .find(|(_, tx)| tx.transaction.id.mined_id() == id)
    };

    let (dependency_depth, _) =
        selected_tx_by_id(independent_tx_id).expect("should select the independent tx");

    assert_eq!(
        *dependency_depth, 0,
        "should return a dependency depth of 0 for the independent tx"
    );

    let (dependency_depth, _) = selected_tx_by_id(dependent_tx1.transaction.id.mined_id())
        .expect("should select dependent_tx1");

    assert_eq!(
        *dependency_depth, 1,
        "should return a dependency depth of 1 for the dependent tx"
    );
}
