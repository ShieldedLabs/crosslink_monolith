use std::sync::{Arc, Mutex};

use wallet::{WalletTx, WalletTxKind};

fn main() {
    let wallet_state: Arc<Mutex<wallet::WalletState>> = Arc::<Mutex::<wallet::WalletState>>::new(Mutex::<wallet::WalletState>::new(wallet::WalletState::new()));
    if true {
        wallet_state.lock().unwrap().txs = vec![
            WalletTx::with_fake_data(WalletTxKind::Send, 100_000_000, 0, false, "Hello, world!"),
            WalletTx::with_fake_data(WalletTxKind::Receive, 0, 100_000_000, false, "Other things"),
            WalletTx::with_fake_data(WalletTxKind::Send, 250_000_000, 0, true, "More things"),
            WalletTx::with_fake_data(WalletTxKind::Shield, 10_000_000_000, 0, true, ""),
            // WalletTx::with_fake_data(WalletTxKind::Stake, 125_000_000, 0, false, "It's favorite, not favourite"),
            // WalletTx::with_fake_data(WalletTxKind::Unstake, 0, 125_000_000, false, "It's favorite, not favourite"),
        ];
    }

    visualizer_zcash::main_thread_run_program(wallet_state, true);
}
