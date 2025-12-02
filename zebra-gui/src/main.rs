use std::sync::{Arc, Mutex};

use wallet::{RosterMember, WalletTx, WalletTxKind};

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
        ];
        wallet_state.lock().unwrap().roster = vec![
            RosterMember{ pub_key: [0xAAu8; 32], stake: 250_000_000 },
            RosterMember{ pub_key: [0xBBu8; 32], stake: 100_000_000 },
            RosterMember{ pub_key: [0xCCu8; 32], stake: 300_000_000 },
            RosterMember{ pub_key: [0xDDu8; 32], stake: 500_000_000 },
            RosterMember{ pub_key: [0xEEu8; 32], stake: 500_555_000 },
            RosterMember{ pub_key: [0xFFu8; 32], stake: 000_000_000 },
            RosterMember{ pub_key: [0x11u8; 32], stake: 000_000_000 },
            RosterMember{ pub_key: [0x22u8; 32], stake: 000_000_000 },
            RosterMember{ pub_key: [0x33u8; 32], stake: 000_000_000 },
        ];
    }

    visualizer_zcash::main_thread_run_program(wallet_state, true);
}
