use std::sync::{Arc, Mutex};

use wallet::{WalletRosterMember, WalletTx, WalletTxKind};

fn main() {
    let wallet_state: Arc<Mutex<wallet::WalletState>> = Arc::<Mutex::<wallet::WalletState>>::new(Mutex::<wallet::WalletState>::new(wallet::WalletState::new()));
    if true {
        wallet_state.lock().unwrap().txs = vec![
            WalletTx::with_fake_data(WalletTxKind::Send, 100_000_000, 0, false, "Hello, world!"),
            WalletTx::with_fake_data(WalletTxKind::Receive, 0, 100_000_000, false, "Other things"),
            WalletTx::with_fake_data(WalletTxKind::Send, 250_000_000, 0, true, "More things"),
            WalletTx::with_fake_data(WalletTxKind::Shield, 10_000_000_000, 0, true, ""),
            WalletTx::with_fake_data(WalletTxKind::Stake, 10_000_000_000, 0, false, ""),
            WalletTx::with_fake_data(WalletTxKind::Unstake, 10_000_000_000, 0, false, ""),
            WalletTx::with_fake_data(WalletTxKind::Send, 100_000_000, 0, false, "Hello, world!"),
            WalletTx::with_fake_data(WalletTxKind::Receive, 0, 100_000_000, false, "Other things"),
            WalletTx::with_fake_data(WalletTxKind::Send, 250_000_000, 0, true, "More things"),
            WalletTx::with_fake_data(WalletTxKind::Shield, 10_000_000_000, 0, true, ""),
            WalletTx::with_fake_data(WalletTxKind::Stake, 10_000_000_000, 0, false, ""),
            WalletTx::with_fake_data(WalletTxKind::Unstake, 10_000_000_000, 0, false, ""),
            WalletTx::with_fake_data(WalletTxKind::Send, 100_000_000, 0, false, "Hello, world!"),
            WalletTx::with_fake_data(WalletTxKind::Receive, 0, 100_000_000, false, "Other things"),
            WalletTx::with_fake_data(WalletTxKind::Send, 250_000_000, 0, true, "More things"),
            WalletTx::with_fake_data(WalletTxKind::Shield, 10_000_000_000, 0, true, ""),
            WalletTx::with_fake_data(WalletTxKind::Stake, 10_000_000_000, 0, false, ""),
            WalletTx::with_fake_data(WalletTxKind::Unstake, 10_000_000_000, 0, false, ""),
            WalletTx::with_fake_data(WalletTxKind::Send, 100_000_000, 0, false, "Hello, world!"),
            WalletTx::with_fake_data(WalletTxKind::Receive, 0, 100_000_000, false, "Other things"),
            WalletTx::with_fake_data(WalletTxKind::Send, 250_000_000, 0, true, "More things"),
            WalletTx::with_fake_data(WalletTxKind::Shield, 10_000_000_000, 0, true, ""),
            WalletTx::with_fake_data(WalletTxKind::Stake, 10_000_000_000, 0, false, ""),
            WalletTx::with_fake_data(WalletTxKind::Unstake, 10_000_000_000, 0, false, ""),
            WalletTx::with_fake_data(WalletTxKind::Send, 100_000_000, 0, false, "Hello, world!"),
            WalletTx::with_fake_data(WalletTxKind::Receive, 0, 100_000_000, false, "Other things"),
            WalletTx::with_fake_data(WalletTxKind::Send, 250_000_000, 0, true, "More things"),
            WalletTx::with_fake_data(WalletTxKind::Shield, 10_000_000_000, 0, true, ""),
            WalletTx::with_fake_data(WalletTxKind::Stake, 10_000_000_000, 0, false, ""),
            WalletTx::with_fake_data(WalletTxKind::Unstake, 10_000_000_000, 0, false, ""),
            WalletTx::with_fake_data(WalletTxKind::Send, 100_000_000, 0, false, "Hello, world!"),
            WalletTx::with_fake_data(WalletTxKind::Receive, 0, 100_000_000, false, "Other things"),
            WalletTx::with_fake_data(WalletTxKind::Send, 250_000_000, 0, true, "More things"),
            WalletTx::with_fake_data(WalletTxKind::Shield, 10_000_000_000, 0, true, ""),
            WalletTx::with_fake_data(WalletTxKind::Stake, 10_000_000_000, 0, false, ""),
            WalletTx::with_fake_data(WalletTxKind::Unstake, 10_000_000_000, 0, false, ""),
        ];
        wallet_state.lock().unwrap().roster = vec![
            WalletRosterMember{ pub_key: [0xAAu8; 32], voting_power: 250_000_000, txids: vec![] },
            WalletRosterMember{ pub_key: [0xBBu8; 32], voting_power: 100_000_000, txids: vec![] },
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
    }

    visualizer_zcash::main_thread_run_program(wallet_state, true);
}
