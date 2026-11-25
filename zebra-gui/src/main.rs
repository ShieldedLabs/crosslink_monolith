use std::sync::{Arc, Mutex};

// Nothing calls into it: linking it is the point. cosmo-build's linker shim
// passes `--wrap` for the 123 libc entry points std uses, and cosmo-compat is
// what defines the matching `__wrap_*`. Without this the rlib is never
// referenced and the link fails on undefined `__wrap_mmap` and friends.
#[cfg(cosmo)]
extern crate cosmo_compat as _;

use wallet::{WalletRosterMember, WalletTx, WalletTxKind};

fn main() {
    // zebra-gui diff <dir> <dir> compares two scripted runs' screenshots; zebra-gui --scene empty
    // starts with no mock data. GUI_DEV.md covers both.
    let args: Vec<String> = std::env::args().collect();
    if args.len() == 4 && args[1] == "diff" {
        let differ = zebra_gui::script::diff_dirs(std::path::Path::new(&args[2]), std::path::Path::new(&args[3]));
        std::process::exit((differ != 0) as i32);
    }
    if args.len() >= 3 && args[1] == "dup" {
        let min = args[2].parse::<usize>().ok();
        zebra_gui::script::duplicates(&args[if min.is_some() { 3 } else { 2 }..], min.unwrap_or(8));
        return;
    }
    let pace = args.iter().position(|a| a == "--pace").and_then(|i| args.get(i + 1)).and_then(|s| s.parse::<f64>().ok());
    let scene = args.iter().position(|a| a == "--scene").and_then(|i| args.get(i + 1)).map(|s| s.as_str()).unwrap_or("default");
    if scene != "default" && scene != "empty" {
        eprintln!("unknown scene {:?}: expected default or empty", scene);
        std::process::exit(2);
    }

    let wallet_state: Arc<Mutex<wallet::WalletState>> = Arc::<Mutex::<wallet::WalletState>>::new(Mutex::<wallet::WalletState>::new(wallet::WalletState::new()));
    if scene == "default" {
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

        {
            let mut ws = wallet_state.lock().unwrap();
            ws.wallet_is_init = true;
            ws.user_txs = pending.iter().take(40).cloned().collect();
            ws.user_unshielded_funds = 250_000_000;
            ws.user_shielded_spendable_funds = 12_345_600_000;
            ws.user_shielded_pending_funds = 100_000_000;
            ws.staked_balance = 400_000_000;
            ws.withdrawable_balance = 50_000_000;
            ws.miner_unshielded_funds = 625_000_000;
            ws.miner_shielded_spendable_funds = 98_700_000_000;
            ws.wallets_sync_h = 9001;
            ws.wallets_tip_h = 9001;
        }
        wallet_state.lock().unwrap().miner_txs = pending;
        wallet_state.lock().unwrap().roster = vec![
            WalletRosterMember{ pub_key: [0xAAu8; 32], voting_power: 25540_000_000, txids: vec![], finalizer_address: None },
            WalletRosterMember{ pub_key: [0xBBu8; 32], voting_power: 111000_000_000, txids: vec![], finalizer_address: None },
            WalletRosterMember{ pub_key: [0xCCu8; 32], voting_power: 250_000_000, txids: vec![], finalizer_address: None },
            WalletRosterMember{ pub_key: [0xDDu8; 32], voting_power: 100_000_000, txids: vec![], finalizer_address: None },
            WalletRosterMember{ pub_key: [0xEEu8; 32], voting_power: 250_000_000, txids: vec![], finalizer_address: None },
            WalletRosterMember{ pub_key: [0xFFu8; 32], voting_power: 100_000_000, txids: vec![], finalizer_address: None },
            WalletRosterMember{ pub_key: [0x01u8; 32], voting_power: 250_000_000, txids: vec![], finalizer_address: None },
            WalletRosterMember{ pub_key: [0x11u8; 32], voting_power: 100_000_000, txids: vec![], finalizer_address: None },
            WalletRosterMember{ pub_key: [0x22u8; 32], voting_power: 250_000_000, txids: vec![], finalizer_address: None },
            WalletRosterMember{ pub_key: [0x33u8; 32], voting_power: 100_000_000, txids: vec![], finalizer_address: None },
            WalletRosterMember{ pub_key: [0x44u8; 32], voting_power: 250_000_000, txids: vec![], finalizer_address: None },
            WalletRosterMember{ pub_key: [0x55u8; 32], voting_power: 100_000_000, txids: vec![], finalizer_address: None },
            WalletRosterMember{ pub_key: [0x66u8; 32], voting_power: 250_000_000, txids: vec![], finalizer_address: None },
            WalletRosterMember{ pub_key: [0x77u8; 32], voting_power: 100_000_000, txids: vec![], finalizer_address: None },
            WalletRosterMember{ pub_key: [0x88u8; 32], voting_power: 250_000_000, txids: vec![], finalizer_address: None },
            WalletRosterMember{ pub_key: [0x99u8; 32], voting_power: 100_000_000, txids: vec![], finalizer_address: None },
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

    let Some(window) = zebra_gui::open_window() else {
        eprintln!("could not open a window");
        std::process::exit(1);
    };
    let started = std::time::Instant::now();
    if pace.is_some() { zebra_gui::script::pace_start(); }
    zebra_gui::main_thread_run_program(window, wallet_state, scene == "default", move || pace.is_some_and(|s| started.elapsed().as_secs_f64() >= s));
    if pace.is_some() { zebra_gui::script::pace_report(); }
    std::process::exit((zebra_gui::script::FAILURES.load(std::sync::atomic::Ordering::Relaxed) != 0) as i32);
}
