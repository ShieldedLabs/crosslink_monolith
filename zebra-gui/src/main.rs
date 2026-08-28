use std::sync::{Arc, Mutex};

// Nothing calls into it: linking it is the point. cosmo-build's linker shim
// passes `--wrap` for the 123 libc entry points std uses, and cosmo-compat is
// what defines the matching `__wrap_*`. Without this the rlib is never
// referenced and the link fails on undefined `__wrap_mmap` and friends.
#[cfg(cosmo)]
extern crate cosmo_compat as _;

use wallet::{WalletRosterMember, WalletTx, WalletTxKind};

fn main() {
    let wallet_state: Arc<Mutex<wallet::WalletState>> = Arc::<Mutex::<wallet::WalletState>>::new(Mutex::<wallet::WalletState>::new(wallet::WalletState::new()));
    if true {
        let mut txs = vec![
            WalletTx::with_fake_data(WalletTxKind::Send, 100_000_000, 0, false, false, "Hello, world!", 0),
            WalletTx::with_fake_data(WalletTxKind::Receive, 0, 100_000_000, false, false, "Other things", 10),
            WalletTx::with_fake_data(WalletTxKind::Send, 250_000_000, 0, true, false, "More things", 11),
            WalletTx::with_fake_data(WalletTxKind::Shield, 10_000_000_000, 0, true, false, "", 12),
            WalletTx::with_fake_data(WalletTxKind::Stake, 10_000_000_000, 0, false, false, "", 0),
            WalletTx::with_fake_data(WalletTxKind::BeginUnstake, 10_000_000_000, 0, false, false, "", 13),
            WalletTx::with_fake_data(WalletTxKind::BeginUnstake, 10_000_000_000, 0, false, true, "Fork", 13),
            WalletTx::with_fake_data(WalletTxKind::Send, 100_000_000, 0, false, false, "Hello, world!", 15),
            WalletTx::with_fake_data(WalletTxKind::Receive, 0, 100_000_000, false, false, "Other things", 25),
            WalletTx::with_fake_data(WalletTxKind::Send, 250_000_000, 0, true, false, "More things", 32),
            WalletTx::with_fake_data(WalletTxKind::Shield, 10_000_000_000, 0, true, false, "", 64),
            WalletTx::with_fake_data(WalletTxKind::Stake, 10_000_000_000, 0, false, false, "", 0),
            WalletTx::with_fake_data(WalletTxKind::BeginUnstake, 10_000_000_000, 0, false, false, "", 0),
            WalletTx::with_fake_data(WalletTxKind::Send, 100_000_000, 0, false, false, "Hello this is a test to see if the word wrapping works properly........", 0),
            WalletTx::with_fake_data(WalletTxKind::Receive, 0, 100_000_000, false, false, "Other things", 0),
            WalletTx::with_fake_data(WalletTxKind::Send, 250_000_000, 0, true, false, "More things", 0),
            WalletTx::with_fake_data(WalletTxKind::Shield, 10_000_000_000, 0, true, false, "", 73),
            WalletTx::with_fake_data(WalletTxKind::Stake, 10_000_000_000, 0, false, false, "", 0),
            WalletTx::with_fake_data(WalletTxKind::BeginUnstake, 10_000_000_000, 0, false, false, "", 74),
            WalletTx::with_fake_data(WalletTxKind::Send, 100_000_000, 0, false, false, "Hello, world!", 0),
            WalletTx::with_fake_data(WalletTxKind::Receive, 0, 100_000_000, false, false, "Other things", 0),
            WalletTx::with_fake_data(WalletTxKind::Send, 250_000_000, 0, true, false, "More things", 0),
            WalletTx::with_fake_data(WalletTxKind::Shield, 10_000_000_000, 0, true, false, "", 100),
            WalletTx::with_fake_data(WalletTxKind::Stake, 10_000_000_000, 0, false, false, "", 750),
            WalletTx::with_fake_data(WalletTxKind::BeginUnstake, 10_000_000_000, 0, false, false, "", 800),
            WalletTx::with_fake_data(WalletTxKind::Send, 100_000_000, 0, false, false, "Hello, world!", 0),
            WalletTx::with_fake_data(WalletTxKind::Receive, 0, 100_000_000, false, false, "Other things", 0),
            WalletTx::with_fake_data(WalletTxKind::Send, 250_000_000, 0, true, false, "More things", 0),
            WalletTx::with_fake_data(WalletTxKind::Shield, 10_000_000_000, 0, true, false, "", 801),
            WalletTx::with_fake_data(WalletTxKind::Stake, 10_000_000_000, 0, false, false, "", 805),
            WalletTx::with_fake_data(WalletTxKind::BeginUnstake, 10_000_000_000, 0, false, false, "", 9000),
            WalletTx::with_fake_data(WalletTxKind::Send, 100_000_000, 0, false, false, "Hello, world!", 1024),
            WalletTx::with_fake_data(WalletTxKind::Receive, 0, 100_000_000, false, false, "Other things", 9001),
            WalletTx::with_fake_data(WalletTxKind::Send, 250_000_000, 0, true, false, "More things", 0),
            WalletTx::with_fake_data(WalletTxKind::Shield, 10_000_000_000, 0, true, false, "", 0),
            WalletTx::with_fake_data(WalletTxKind::Stake, 10_000_000_000, 0, false, false, "", 0),
            WalletTx::with_fake_data(WalletTxKind::BeginUnstake, 10_000_000_000, 0, false, false, "", 0),
        ];
        txs.repeat(27);

        let mut pending: Vec<_> = txs.iter().filter(|tx| !tx.h.is_in_block()).map(|tx| tx.clone()).collect();
        let mut mined:   Vec<_> = txs.iter().filter(|tx|  tx.h.is_in_block()).map(|tx| tx.clone()).collect();
        pending.sort_by(| a, b | a.txid.cmp(&b.txid));
        mined.sort_by(|a, b| b.h.cmp(&a.h));
        pending.extend_from_slice(&mined);

        wallet_state.lock().unwrap().miner_txs = pending;
        wallet_state.lock().unwrap().roster = vec![
            WalletRosterMember{ pub_key: [0xAAu8; 32], voting_power: 25540_000_000, txids: vec![] },
            WalletRosterMember{ pub_key: [0xBBu8; 32], voting_power: 111000_000_000, txids: vec![] },
            WalletRosterMember{ pub_key: [0xCCu8; 32], voting_power: 250_000_000, txids: vec![] },
            WalletRosterMember{ pub_key: [0xDDu8; 32], voting_power: 100_000_000, txids: vec![] },
            WalletRosterMember{ pub_key: [0xEEu8; 32], voting_power: 250_000_000, txids: vec![] },
            WalletRosterMember{ pub_key: [0xFFu8; 32], voting_power: 100_000_000, txids: vec![] },
            WalletRosterMember{ pub_key: [0x01u8; 32], voting_power: 250_000_000, txids: vec![] },
            WalletRosterMember{ pub_key: [0x11u8; 32], voting_power: 100_000_000, txids: vec![] },
            WalletRosterMember{ pub_key: [0x22u8; 32], voting_power: 250_000_000, txids: vec![] },
            WalletRosterMember{ pub_key: [0x33u8; 32], voting_power: 100_000_000, txids: vec![] },
            WalletRosterMember{ pub_key: [0x44u8; 32], voting_power: 250_000_000, txids: vec![] },
            WalletRosterMember{ pub_key: [0x55u8; 32], voting_power: 100_000_000, txids: vec![] },
            WalletRosterMember{ pub_key: [0x66u8; 32], voting_power: 250_000_000, txids: vec![] },
            WalletRosterMember{ pub_key: [0x77u8; 32], voting_power: 100_000_000, txids: vec![] },
            WalletRosterMember{ pub_key: [0x88u8; 32], voting_power: 250_000_000, txids: vec![] },
            WalletRosterMember{ pub_key: [0x99u8; 32], voting_power: 100_000_000, txids: vec![] },
        ];
        let dummy_bond = |pk_byte: u8, finalizer_byte: u8| (wallet::ScanBond {
            pk: wallet::PubKeyID([pk_byte; 32]),
            initial_val: 100_000_000,
            create_height: 0,
            create_txid: wallet::PubKeyID([0; 32]),
        }, [finalizer_byte; 32], 100_000_000u64);
        wallet_state.lock().unwrap().stake_positions_bonded = vec![
            dummy_bond(0x1, 0xAA),
            dummy_bond(0x2, 0xBB),
            dummy_bond(0x3, 0xCC),
            dummy_bond(0x4, 0xDD),
        ];
    }

    zebra_gui::main_thread_run_program(wallet_state, true);
    // zebra_gui::main_thread_run_program(wallet_state, false);
}
