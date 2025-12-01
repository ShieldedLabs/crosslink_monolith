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
        ];
        wallet_state.lock().unwrap().roster = vec![
            RosterMember{ pub_key: [1u8; 32], stake: 250_000_000 },
            RosterMember{ pub_key: [1u8; 32], stake: 100_000_000 },
        ];
    }

    visualizer_zcash::main_thread_run_program(wallet_state, true);
}
