use std::sync::{Arc, Mutex};

use wallet::{WalletRosterMember, WalletTx, WalletTxKind};

fn main() {
    let wallet_state: Arc<Mutex<wallet::WalletState>> = Arc::<Mutex::<wallet::WalletState>>::new(Mutex::<wallet::WalletState>::new(wallet::WalletState::new()));
    if true {
        let txs = vec![
            WalletTx::with_fake_data(WalletTxKind::Send, 100_000_000, 0, false, "Hello, world!", 0),
            /* WalletTx::with_fake_data(WalletTxKind::Receive, 0, 100_000_000, false, "Other things", 10),
            WalletTx::with_fake_data(WalletTxKind::Send, 250_000_000, 0, true, "More things", 11),
            WalletTx::with_fake_data(WalletTxKind::Shield, 10_000_000_000, 0, true, "", 12),
            WalletTx::with_fake_data(WalletTxKind::Stake, 10_000_000_000, 0, false, "", 0),
            WalletTx::with_fake_data(WalletTxKind::Unstake, 10_000_000_000, 0, false, "", 13),
            WalletTx::with_fake_data(WalletTxKind::Send, 100_000_000, 0, false, "Hello, world!", 15),
            WalletTx::with_fake_data(WalletTxKind::Receive, 0, 100_000_000, false, "Other things", 25),
            WalletTx::with_fake_data(WalletTxKind::Send, 250_000_000, 0, true, "More things", 32),
            WalletTx::with_fake_data(WalletTxKind::Shield, 10_000_000_000, 0, true, "", 64),
            WalletTx::with_fake_data(WalletTxKind::Stake, 10_000_000_000, 0, false, "", 0),
            WalletTx::with_fake_data(WalletTxKind::Unstake, 10_000_000_000, 0, false, "", 0),
            WalletTx::with_fake_data(WalletTxKind::Send, 100_000_000, 0, false, "Hello this is a test to see if the word wrapping works properly........", 0),
            WalletTx::with_fake_data(WalletTxKind::Receive, 0, 100_000_000, false, "Other things", 0),
            WalletTx::with_fake_data(WalletTxKind::Send, 250_000_000, 0, true, "More things", 0),
            WalletTx::with_fake_data(WalletTxKind::Shield, 10_000_000_000, 0, true, "", 73),
            WalletTx::with_fake_data(WalletTxKind::Stake, 10_000_000_000, 0, false, "", 0),
            WalletTx::with_fake_data(WalletTxKind::Unstake, 10_000_000_000, 0, false, "", 74),
            WalletTx::with_fake_data(WalletTxKind::Send, 100_000_000, 0, false, "Hello, world!", 0),
            WalletTx::with_fake_data(WalletTxKind::Receive, 0, 100_000_000, false, "Other things", 0),
            WalletTx::with_fake_data(WalletTxKind::Send, 250_000_000, 0, true, "More things", 0),
            WalletTx::with_fake_data(WalletTxKind::Shield, 10_000_000_000, 0, true, "", 100),
            WalletTx::with_fake_data(WalletTxKind::Stake, 10_000_000_000, 0, false, "", 750),
            WalletTx::with_fake_data(WalletTxKind::Unstake, 10_000_000_000, 0, false, "", 800),
            WalletTx::with_fake_data(WalletTxKind::Send, 100_000_000, 0, false, "Hello, world!", 0),
            WalletTx::with_fake_data(WalletTxKind::Receive, 0, 100_000_000, false, "Other things", 0),
            WalletTx::with_fake_data(WalletTxKind::Send, 250_000_000, 0, true, "More things", 0),
            WalletTx::with_fake_data(WalletTxKind::Shield, 10_000_000_000, 0, true, "", 801),
            WalletTx::with_fake_data(WalletTxKind::Stake, 10_000_000_000, 0, false, "", 805),
            WalletTx::with_fake_data(WalletTxKind::Unstake, 10_000_000_000, 0, false, "", 9000),
            WalletTx::with_fake_data(WalletTxKind::Send, 100_000_000, 0, false, "Hello, world!", 1024),
            WalletTx::with_fake_data(WalletTxKind::Receive, 0, 100_000_000, false, "Other things", 9001),
            WalletTx::with_fake_data(WalletTxKind::Send, 250_000_000, 0, true, "More things", 0),
            WalletTx::with_fake_data(WalletTxKind::Shield, 10_000_000_000, 0, true, "", 0),
            WalletTx::with_fake_data(WalletTxKind::Stake, 10_000_000_000, 0, false, "", 0),
            WalletTx::with_fake_data(WalletTxKind::Unstake, 10_000_000_000, 0, false, "", 0), */
        ];

        let mut pending: Vec<_> = txs.iter().filter(|tx| tx.0.mined_height.is_none()).map(|tx| tx.clone()).collect();
        let mut mined:   Vec<_> = txs.iter().filter(|tx| tx.0.mined_height.is_some()).map(|tx| tx.clone()).collect();
        pending.sort_by(| a, b | a.0.txid.cmp(&b.0.txid));
        mined.sort_by(|a, b| b.0.mined_height.unwrap().cmp(&a.0.mined_height.unwrap()));
        pending.extend_from_slice(&mined);

        wallet_state.lock().unwrap().txs = pending;
        wallet_state.lock().unwrap().roster = vec![
            /* WalletRosterMember{ pub_key: [0xAAu8; 32], voting_power: 25540_000_000, txids: vec![] },
            WalletRosterMember{ pub_key: [0xBBu8; 32], voting_power: 111000100_000_000, txids: vec![] },
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
            WalletRosterMember{ pub_key: [0x99u8; 32], voting_power: 100_000_000, txids: vec![] }, */
        ];
        wallet_state.lock().unwrap().staked_roster = vec![
            /* ([0xAAu8; 32], [0x1u8; 32], 100_000_000, 250_000_000),
            ([0xBBu8; 32], [0x1u8; 32], 100_000_000, 100_000_000),
            ([0xCCu8; 32], [0x1u8; 32], 100_000_000, 250_000_000),
            ([0xDDu8; 32], [0x1u8; 32], 100_000_000, 100_000_000), */
        ];
    }

    // visualizer_zcash::main_thread_run_program(wallet_state, true);
    visualizer_zcash::main_thread_run_program(wallet_state, false);
}
