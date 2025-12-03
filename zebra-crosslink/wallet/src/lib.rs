//! Internal wallet
#![allow(warnings)]

use zcash_client_backend::data_api::WalletCommitmentTrees;
use orchard::keys::SpendAuthorizingKey;
use orchard::note_encryption::CompactAction;
use rand_chacha::rand_core::SeedableRng;
use rand_core::OsRng;
use sapling_crypto::zip32::ExtendedSpendingKey;
use secrecy::{ExposeSecret,SecretVec,Secret};
use std::collections::{HashMap, VecDeque};
use std::convert::{identity, Infallible};
use std::future::Future;
use std::mem;
use std::sync::{Arc, Mutex};
use tokio_rustls::rustls;
use tonic::client::GrpcService;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint};
use tonic::IntoRequest;
use zcash_client_backend::data_api::chain::{BlockCache, CommitmentTreeRoot};
use zcash_client_backend::data_api::wallet::{ConfirmationsPolicy, TargetHeight, create_proposed_transactions, propose_shielding, shield_transparent_funds};
use zcash_client_backend::fees::{self, StandardFeeRule};
use zcash_client_backend::proto::service::{GetSubtreeRootsArg, RawTransaction, TreeState, TxFilter};
use zcash_client_backend::wallet::WalletTransparentOutput;
use zcash_client_memory::MemBlockCache;
use zcash_client_sqlite::error::SqliteClientError;
use zcash_client_sqlite::util::SystemClock;
use zcash_client_sqlite::{AccountUuid, WalletDb};
use zcash_note_encryption::{try_compact_note_decryption, try_note_decryption};
use zcash_primitives::transaction::builder::{BuildConfig, Builder as TxBuilder};
use zcash_primitives::transaction::components::TxOut;
use zcash_primitives::transaction::fees::zip317::{self, MINIMUM_FEE};
use zcash_primitives::transaction::sighash::{signature_hash, SignableInput};
use zcash_primitives::transaction::txid::TxIdDigester;
use zcash_primitives::transaction::{Transaction, TransactionData, TxVersion};
use zcash_proofs::prover::LocalTxProver;
use zcash_protocol::consensus::{BlockHeight, BranchId};
use zcash_protocol::value::{ZatBalance, Zatoshis};
use zcash_protocol::TxId;
use zcash_transparent::{
    builder::{TransparentBuilder, TransparentSigningSet, Unauthorized},
    bundle::OutPoint,
    keys::NonHardenedChildIndex,
};
use zebra_chain::block::{Hash as BlockHash, Height, ZCASH_BLOCK_VERSION};
use zebra_chain::parameters::NetworkUpgrade;
use zebra_chain::sapling;
use zebra_chain::serialization::{ZcashDeserialize, ZcashSerialize};
use zebra_chain::sprout::commitment;
use zebra_chain::transaction::LockTime;
use zebra_chain::transparent::{self, Input, Utxo, MIN_TRANSPARENT_COINBASE_MATURITY};

use rustls::client::danger::ServerCertVerified;
use rustls::client::danger::ServerCertVerifier;
use rustls::crypto::ring;
use rustls::crypto::verify_tls12_signature;
use rustls::crypto::verify_tls13_signature;
use rustls::crypto::CryptoProvider;
use rustls::crypto::WebPkiSupportedAlgorithms;
use rustls::CertificateError;
use tokio::runtime::Builder;
use zcash_client_backend::{
    address::UnifiedAddress,
    data_api::{
        self,
        chain::{error::Error as ChainError, scan_cached_blocks, ChainState},
        scanning::{ScanPriority, ScanRange},
        Balance,
        wallet, Account as APIAccount, AccountBirthday, AccountPurpose, WalletRead, WalletWrite,
        Zip32Derivation,
    },
    encoding::AddressCodec,
    keys::{
        UnifiedAddressRequest, UnifiedFullViewingKey, UnifiedIncomingViewingKey, UnifiedSpendingKey,
    },
    proto::{
        compact_formats::{CompactBlock, CompactTx},
        service::{
            compact_tx_streamer_client::CompactTxStreamerClient, BlockId, BlockRange, ChainSpec,
            Duration, Empty, GetAddressUtxosArg, LightdInfo, TransparentAddressBlockFilter,
        },
    },
};
use zcash_protocol::consensus::{NetworkType, Parameters, MAIN_NETWORK, TEST_NETWORK};
use zcash_transparent::{
    address::TransparentAddress,
    keys::{IncomingViewingKey, TransparentKeyScope},
};
use zcash_primitives::transaction::{RosterMember, StakingAction, StakingActionKind, StakeTxId};

pub static GLOBAL_SEED: Mutex<Option<[u8; 32]>> = Mutex::new(None);

pub static TENDERLINK_PUBLIC_KEY: Mutex<[u8; 32]> = Mutex::new([0_u8; 32]);

async fn wait_for_zainod() {
    let mut interval = tokio::time::interval(tokio::time::Duration::from_millis(500));
    for _ in 0..10 {
        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(2))
            .timeout(std::time::Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();

        let request_body = r#"{"jsonrpc":"2.0","method":"getinfo","params":[],"id":1}"#;
        let request_builder = client
            .post("http://localhost:18232")
            .header("Content-Type", "application/json")
            .body(request_body);

        if let Ok(res) = request_builder.send().await {
            if res.status().is_success() {
                println!("ZAINO IS READY: {}", res.text().await.unwrap());
                return;
            }
        }

        interval.tick().await;
    }
}

fn block_policy_10() -> ConfirmationsPolicy { ConfirmationsPolicy::new(std::num::NonZeroU32::new(5).unwrap(), std::num::NonZeroU32::new(5).unwrap(), false).unwrap() }

#[derive(Debug, Clone, PartialEq)]
enum WalletAction {
    RequestFromFaucet,
    TestStakeAction,
    StakeToFinalizer(Zatoshis, [u8; 32]),
    UnstakeFromFinalizer(TxId),
    SendToAddress(UnifiedAddress, Zatoshis),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WalletTxKind {
    Send,
    Receive,
    Shield,
    Stake,
    Unstake,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WalletTx(pub TransactionSummary<AccountUuid>, pub WalletTxKind);

impl WalletTx {
    pub fn with_fake_data(kind: WalletTxKind, sent: u64, received: u64, shielding: bool, memo: &str, mined_height: u32) -> Self {
        let mut memo_as_bytes = [0u8; 512];
        &memo_as_bytes[0..memo.len()].copy_from_slice(memo.as_bytes());

        Self(
            TransactionSummary{
                account_id: AccountUuid::default(),
                txid: TxId::from_bytes([0; 32]),
                expiry_height: None,
                mined_height: if mined_height != 0 { Some(BlockHeight::from_u32(mined_height)) } else { None },
                account_value_delta: ZatBalance::from_i64(-(sent as i64)).unwrap(),
                total_spent: Zatoshis::from_u64(sent).unwrap(),
                total_received: Zatoshis::from_u64(received).unwrap(),
                fee_paid: None,
                spent_note_count: if kind == WalletTxKind::Send { 1 } else { 0 },
                has_change: false,
                sent_note_count: if kind == WalletTxKind::Send { 1 } else { 0 },
                received_note_count: if kind == WalletTxKind::Receive { 1 } else { 0 },
                memo_count: if memo_as_bytes.len() != 0 { 1 } else { 0 },
                expired_unmined: false,
                is_shielding: true,
                memo: memo_as_bytes,
            },
            kind,
        )
    }
}

// @note(judah): needed so the visualizer doesn't take a dependency on zcash_primitives
#[derive(Default, Debug, Clone)]
pub struct WalletRosterMember {
    pub pub_key: [u8; 32],
    pub voting_power: u64,
    pub txids: std::vec::Vec<StakeTxId>,
}

fn w_flip(use_i: &mut usize, update_i: &mut usize) {
    if *use_i == *update_i {
        *update_i = 1;
    } else {
        *use_i ^= 1;
        *update_i ^= 1;
    }
}

#[derive(Default, Debug, Clone)]
pub struct WalletState {
    pub balance:         i64, // in zats
    pub pending_balance: i64, // in zats
    pub staked_balance:  i64, // in zats
    pub show_staked_balance: bool,

    pub txs:           Vec<WalletTx>,
    pub roster:        Vec<WalletRosterMember>,
    pub staked_roster: Vec<([u8; 32] /* pub key */, [u8; 32] /* txid */, u64 /* initial */, u64 /* accumulated */)>,

    pub waiting_for_faucet: bool,
    pub waiting_for_stake_to_finalizer: bool,
    pub waiting_for_send: bool,

    pub miner_seen_height: u32,
    pub miner_unshielded_funds: u64,
    pub miner_shielded_pending_funds: u64,
    pub miner_shielded_spendable_funds: u64,
    pub faucet_funds_available: u64,

    pub user_recv_ua: String,

    pub actions_in_flight: VecDeque<WalletAction>,
}

impl WalletState {
    pub fn new() -> Self {
        WalletState {
            ..Default::default()
        }
    }

    pub fn request_from_faucet(&mut self) {
        self.waiting_for_faucet = true;

        if self.actions_in_flight.iter().filter(|a| match a { WalletAction::RequestFromFaucet => true, _ => false }).count() != 0 {
            return;
        }

        self.actions_in_flight.push_back(WalletAction::RequestFromFaucet);
    }

    pub fn stake_to_finalizer(&mut self, amount: u64, target_finalizer: [u8; 32]) {
        if self.actions_in_flight.iter().filter(|a| match a { WalletAction::StakeToFinalizer(_,_) => true, _ => false }).count() != 0 {
            return;
        }

        self.waiting_for_stake_to_finalizer = true;
        self.actions_in_flight.push_back(WalletAction::StakeToFinalizer(Zatoshis::from_u64(amount).expect("Invalid amount given to stake_to_finalizer"), target_finalizer));
    }

    pub fn unstake_from_finalizer(&mut self, txid: [u8; 32]) {
        let txid = TxId::from_bytes(txid);
        if self.actions_in_flight.iter().filter(|a| match a { WalletAction::UnstakeFromFinalizer(id) if id.eq(&txid) => true, _ => false }).count() != 0 {
            return;
        }
        self.actions_in_flight.push_back(WalletAction::UnstakeFromFinalizer(txid));
    }

    pub fn send_to_address(&mut self, address: String, amount: u64) {
        let Ok(address) = UnifiedAddress::decode(&TEST_NETWORK /* @todo */, &address) else {
            println!("Invalid address for send: {}", address);
            return;
        };

        if self.actions_in_flight.iter().filter(|a| match a {
            WalletAction::SendToAddress(addr, amt) if amt.into_u64() == amount && addr.eq(&address) => true,
            _ => false
        }).count() != 0 {
            return;
        }

        self.waiting_for_send = true;
        self.actions_in_flight.push_back(WalletAction::SendToAddress(address, Zatoshis::from_u64(amount).expect("Invalid amount given to stake_to_finalizer")));
    }
}

pub fn str_from_ctaz(val: u64) -> String {
    let full = val / 100_000_000;
    let part = val % 100_000_000;
    let part_str = format!("{part}00");
    let trim_part = part_str.trim_end_matches("0");
    format!("{full}.{}", &part_str[..trim_part.len().max(3)])
}

struct TxOptions {
    memo: Option<zcash_protocol::memo::MemoBytes>,
    staking_action: Option<StakingAction>,
}
impl Default for TxOptions {
    fn default() -> Self {
        Self {
            memo: None,
            staking_action: None,
        }
    }
}

pub async fn wallet_main(wallet_state: Arc<Mutex<WalletState>>) {
    fn stuff_from_seed_phrase<P: Parameters + 'static>(params:P, phrase: &str) -> (
        SecretVec<u8>,
        UnifiedSpendingKey,
    ) {
        use secrecy::ExposeSecret;

        let mnemonic = bip39::Mnemonic::parse(phrase).unwrap();
        let bip39_passphrase = ""; // optional
        let seed64 = mnemonic.to_seed(bip39_passphrase);
        let seed = SecretVec::new(seed64[..32].to_vec());
        let seed_fp = zip32::fingerprint::SeedFingerprint::from_seed(seed.expose_secret()).unwrap();
        let account_id = zip32::AccountId::try_from(0).unwrap();

        let usk = UnifiedSpendingKey::from_seed(&params, seed.expose_secret(), account_id).unwrap();
        let birthday = &AccountBirthday::from_parts(
            ChainState::empty(BlockHeight::from_u32(0), zcash_primitives::block::BlockHash([0; 32])),
            None,
        );

        (seed, usk)
    }

    fn wallet_from_stuff<P: Parameters + 'static>(params: P, seed: SecretVec<u8>) -> (
        WalletDb<rusqlite::Connection, P, SystemClock, OsRng>,
        zcash_client_sqlite::wallet::Account,
    ) {
        let mut wallet = zcash_client_sqlite::WalletDb::for_path(":memory:", params, SystemClock, OsRng).unwrap();
        zcash_client_sqlite::wallet::init::init_wallet_db(
            &mut wallet,
            Some(Secret::new(seed.expose_secret().clone())),
        ).unwrap();

        let birthday = &AccountBirthday::from_parts(
            ChainState::empty(BlockHeight::from_u32(0), zcash_primitives::block::BlockHash([0; 32])),
            None,
        );

        let (account_uuid, _) = wallet
            .create_account("main_account", &seed, birthday, None)
            .unwrap();
        let account = wallet.get_account(account_uuid).unwrap().unwrap();
        (wallet, account)
    }

    fn transparent_keys_from_usk(usk: &UnifiedSpendingKey, index: u32) -> Option<(secp256k1::PublicKey, secp256k1::SecretKey)> {
        let transparent = usk.transparent();
        let account_pubkey = transparent.to_account_pubkey();
        let child_index = NonHardenedChildIndex::const_from_index(index);
        let address_pubkey = account_pubkey.derive_address_pubkey(TransparentKeyScope::EXTERNAL, child_index).ok()?;
        let address_privkey = transparent.derive_external_secret_key(child_index).ok()?;
        Some((address_pubkey, address_privkey))
    }

    fn addrs_from_account(account: &zcash_client_sqlite::wallet::Account, index: u32) -> Option<(TransparentAddress, UnifiedAddress)> {
        // NOTE: the wallet auto-increments the child index so this isn't recognized
        let ufvk = account.ufvk()?;
        let (ua, di_) = ufvk.find_address(orchard::keys::DiversifierIndex::new(), UnifiedAddressRequest::ORCHARD).ok()?;
        let account_pubkey = ufvk.transparent()?;
        let child_index = NonHardenedChildIndex::const_from_index(index);
        let address_pubkey = account_pubkey.derive_address_pubkey(TransparentKeyScope::EXTERNAL, child_index).ok()?;
        Some((TransparentAddress::from_pubkey(&address_pubkey), ua))
        // Some(account.default_address().ok()??.0)
    }

    let addrs_from_wallet = |wallet: &WalletDb<_, _, _, _>| -> Option<(TransparentAddress, UnifiedAddress)> {
        let Ok(ids)  = wallet.get_account_ids() else { return None; };
        let Some(id) = ids.first() else { return None; };
        let Ok(Some(account)) = wallet.get_account(*id) else { return None; };
        addrs_from_account(&account, 0)
    };

    fn get_transaction_history<P: zcash_protocol::consensus::Parameters>(wallet: &zcash_client_sqlite::WalletDb<rusqlite::Connection, P, SystemClock, OsRng>) -> Result<Vec<TransactionSummary<AccountUuid>>, SqliteClientError> {
        let mut stmt = wallet.conn.prepare_cached(
            "SELECT accounts.uuid as account_uuid, v_transactions.*
             FROM v_transactions
             JOIN accounts ON accounts.uuid = v_transactions.account_uuid
             ORDER BY mined_height DESC, tx_index DESC",
        )?;

        let results = stmt
            .query_and_then::<_, SqliteClientError, _, _>([], |row| {
                Ok(TransactionSummary::from_parts(
                    AccountUuid::from_uuid(row.get("account_uuid")?),
                    TxId::from_bytes(row.get("txid")?),
                    row.get::<_, Option<u32>>("expiry_height")?
                        .map(BlockHeight::from),
                    row.get::<_, Option<u32>>("mined_height")?
                        .map(BlockHeight::from),
                    ZatBalance::from_i64(row.get("account_balance_delta")?)?,
                    Zatoshis::from_nonnegative_i64(row.get("total_spent")?)?,
                    Zatoshis::from_nonnegative_i64(row.get("total_received")?)?,
                    row.get::<_, Option<i64>>("fee_paid")?
                        .map(Zatoshis::from_nonnegative_i64)
                        .transpose()?,
                    row.get("spent_note_count")?,
                    row.get("has_change")?,
                    row.get("sent_note_count")?,
                    row.get("received_note_count")?,
                    row.get("memo_count")?,
                    row.get("expired_unmined")?,
                    row.get("is_shielding")?,
                    [0; 512],
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        // lol
        let mut pending: Vec<_> = results.iter().filter(|tx| tx.mined_height.is_none()).map(|tx| tx.clone()).collect();
        let mut mined:   Vec<_> = results.iter().filter(|tx| tx.mined_height.is_some()).map(|tx| tx.clone()).collect();
        pending.sort_by(| a, b | a.txid.cmp(&b.txid));
        mined.sort_by(|a, b| b.mined_height.unwrap().cmp(&a.mined_height.unwrap()));
        pending.extend_from_slice(&mined);

        return Ok(pending);
    }

    async fn get_received_memos_and_actions<P: zcash_protocol::consensus::Parameters>(client: &mut CompactTxStreamerClient<Channel>, wallet: &zcash_client_sqlite::WalletDb<rusqlite::Connection, P, SystemClock, OsRng>, params: P, history: &[TransactionSummary<AccountUuid>]) -> Option<HashMap<TxId, (Option<StakingAction>, Vec<String>)>> {
        fn try_get_orchard_memos(tx: &TransactionData<zcash_primitives::transaction::Authorized>, ivk: &orchard::keys::PreparedIncomingViewingKey) -> Vec<String> {
            let mut memos = Vec::new();
            let Some(bundle) = tx.orchard_bundle() else { return memos; };

            for action in bundle.actions() {
                let domain = orchard::note_encryption::OrchardDomain::for_action(action);
                if let Some((_, _, memo)) = try_note_decryption(&domain, ivk, action) {
                    let memo = String::from_utf8_lossy(&memo[..]);
                    if memo.len() == 0 {
                        continue;
                    }

                    memos.push(memo.to_string());
                }
            }

            return memos;
        }

        let mut txid_map = HashMap::new();
        let txids: Vec<TxId> = history.iter().map(|h| h.txid).collect();

        let Ok(ids) = wallet.get_account_ids() else { return None; };
        let accounts: Vec<zcash_client_sqlite::wallet::Account> = ids
            .into_iter()
            .map(|id| wallet.get_account(id))
            .filter_map(|acc| acc.ok())
            .filter_map(|acc| acc)
            .collect();
        let uivks: Vec<UnifiedIncomingViewingKey> = accounts
            .into_iter()
            .map(|acc| acc.uivk())
            .collect();

        for txid in &txids {
            let filter = TxFilter{ hash: txid.as_ref().to_vec(), ..Default::default() };
            let Ok(rawtx) = client.get_transaction(filter).await else { continue; };
            let rawtx = rawtx.into_inner();

            let block_height = BlockHeight::from_u32(rawtx.height as u32);
            let Ok(tx) = Transaction::read(&*rawtx.data, BranchId::for_height(&params, block_height)) else {
                continue;
            };

            let action = tx.staking_action().clone();
            let mut memos = Vec::new();
            let txdata = &tx.into_data();
            for uivk in &uivks {
                let possible_orchard_ivk = if let Some(orchard_ivk) = uivk.orchard() { Some(orchard_ivk.prepare()) } else { None };

                if let Some(orchard_ivk) = possible_orchard_ivk {
                    let m: Vec<String> = try_get_orchard_memos(txdata, &orchard_ivk)
                        .iter()
                        .map(|memo| memo.clone())
                        .collect();

                    for memo in m {
                        memos.push(memo);
                    }
                }
            }
            txid_map.insert(*txid, (action, memos));
        }

        Some(txid_map)
    }

    struct Timer { t_bgn: std::time::Instant, name: &'static str };
    impl Timer {
        pub fn scope(name: &'static str) -> Self {
            println!("started {}", name);
            Self {
                name, t_bgn: std::time::Instant::now()
            }
        }
    };
    impl Drop for Timer {
        fn drop(&mut self) {
            println!("{} took {}ms", self.name, self.t_bgn.elapsed().as_millis());
        }
    }

    let send_zats = async | client: &mut CompactTxStreamerClient<_>, dst_ua: &UnifiedAddress, src_wallet: &mut WalletDb<_, _, _, _>, src_usk: &UnifiedSpendingKey, zats: Zatoshis, params, opts: &TxOptions| -> Option<[u8;32]> {
        let t = Timer::scope("send_zats");

        // @todo(judah): handle multiple accounts?
        let Ok(src_ids)  = src_wallet.get_account_ids() else { return None; };
        let Some(src_id) = src_ids.first() else { return None; };
        let Ok(Some(src_account)) = src_wallet.get_account(*src_id) else { return None; };

        const FALLBACK_CHANGE_POOL: zcash_protocol::ShieldedProtocol = zcash_protocol::ShieldedProtocol::Orchard;

        match wallet::propose_standard_transfer_to_address::<_, _, Infallible>(
            src_wallet,
            params,
            zcash_client_backend::fees::StandardFeeRule::Zip317,
            src_account.id(),
            block_policy_10(),
            &zcash_client_backend::address::Address::Unified(dst_ua.clone()),
            zats,
            opts.memo.clone(),
            None,
            FALLBACK_CHANGE_POOL)
        {
            Err(err) => {
                println!("propose_transfer error: {err:?}");
                None
            },
            Ok(proposal) => {
                let prover = LocalTxProver::bundled();
                match wallet::create_proposed_transactions::<_, _, Infallible, _, Infallible, _>(
                    src_wallet,
                    params,
                    &prover,
                    &prover,
                    &wallet::SpendingKeys::from_unified_spending_key(src_usk.clone()),
                    zcash_client_backend::wallet::OvkPolicy::Sender,
                    &proposal,
                    opts.staking_action.clone(),
                ) {
                    Err(err) => {
                        println!("create_proposed_transactions error: {err:?}");
                        None
                    },
                    Ok(txids) => {
                        if txids.len() > 1 {
                            println!("Unexpectedly created {} transactions", txids.len());
                        }

                        let txid = txids[0];
                        let tx = match src_wallet.get_transaction(txid) {
                            Err(err) => {
                                println!("failed to get tx {txid:?} immediately after making it: {err:?}");
                                return None;
                            }
                            Ok(Some(tx)) => tx,
                            Ok(None) => {
                                println!("failed to get tx {txid:?} immediately after making it: (None)");
                                return None;
                            }
                        };

                        let mut data = Vec::new();
                        if let Err(err) = tx.write(&mut data) {
                            println!("Serialization error for tx {:?}: {:?}", txid, err);
                            return None;
                        }

                        let raw_tx = RawTransaction { data, height: 0 };
                        match client.send_transaction(raw_tx).await {
                            Ok(res)  => println!("sent transaction: {res:?}"),
                            Err(err) => {
                                return None;
                            }
                        }

                        println!("created transaction {txid:?}");
                        Some(*txid.as_ref())
                    }
                }
            }
        }
    };

    let send_zats_to_wallet = async | client: &mut CompactTxStreamerClient<_>, dst_wallet: &mut WalletDb<_, _, _, _>, src_wallet: &mut WalletDb<_, _, _, _>, src_usk: &UnifiedSpendingKey, zats: Zatoshis, params, opts: &TxOptions| -> Option<[u8;32]> {
        match addrs_from_wallet(dst_wallet) {
            Some((_, dst_ua)) => send_zats(client, &dst_ua, src_wallet, src_usk, zats, params, opts).await,
            None => None,
        }
    };

    let global_seed = loop {
        if let Some(global_seed) = *GLOBAL_SEED.lock().unwrap() {
            break global_seed;
        }
    };

    let network = &TEST_NETWORK;

    let (
        miner_wallet_init,
        mut miner_account,
        miner_seed,
        miner_usk,
        miner_pubkey,
        miner_privkey,
        miner_t_address,
        miner_ua,
        mut miner_txid_map,
    ) = {
        let (seed, miner_usk) = stuff_from_seed_phrase(network,
            "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about"
        );
        let (miner_wallet, miner_account) = wallet_from_stuff(network, Secret::new(seed.expose_secret().clone()));

        let (miner_t_addr, miner_ua) = addrs_from_account(&miner_account, 0).unwrap();
        let miner_t_addr_str = miner_t_addr.encode(network);
        let (miner_pubkey, miner_privkey) = transparent_keys_from_usk(&miner_usk, 0).unwrap();
        let miner_t_recs = miner_wallet
            .get_transparent_receivers(miner_account.id(), false, false)
            .unwrap();
        (miner_wallet, miner_account, seed, miner_usk, miner_pubkey, miner_privkey, miner_t_addr, miner_ua, HashMap::<TxId, (Option<StakingAction>, Vec<String>)>::new())
    };

    let (
        user_wallet_init,
        mut user_account,
        user_seed,
        user_usk,
        user_pubkey,
        user_privkey,
        user_t_address,
        user_ua,
        mut user_txid_map,
    ) = {
        // roundtrip seed through mnemonic phrase
        let mnemonic = bip39::Mnemonic::from_entropy_in(bip39::Language::English, &global_seed).unwrap();
        let phrase = mnemonic.words().map(|s| s.to_string()).collect::<Vec<String>>().join(" ");

        let (seed, user_usk) = stuff_from_seed_phrase(network, &phrase);
        let (user_wallet, user_account) = wallet_from_stuff(network, Secret::new(seed.expose_secret().clone()));
        let (user_t_addr, user_ua) = addrs_from_account(&user_account, 0).unwrap();
        let user_t_addr_str = user_t_addr.encode(network);
        let (user_pubkey, user_privkey) = transparent_keys_from_usk(&user_usk, 0).unwrap();
        let user_t_recs = user_wallet
            .get_transparent_receivers(user_account.id(), false, false)
            .unwrap();

        // let user_t_addr1 = user_t_recs.into_iter().filter(|(addr, _)| addr == &user_t_addr).next().unwrap().0;
        // NOTE: the default isn't the same as below, but I think this is because it forces a diversifier index
        // println!("User wallet: {}/{:?}", user_t_addr_str, user_t_addr1.encode(network));

        (user_wallet, user_account, seed, user_usk, user_pubkey, user_privkey, user_t_addr, user_ua, HashMap::new())
    };

    let user_ua_str = user_ua.encode(network);
    println!("*************************");
    println!("MINER WALLET T-ADDRESS: {}", miner_ua.encode(network));
    println!("MINER WALLET ADDRESS:   {}", miner_t_address.encode(network));
    println!("USER WALLET T-ADDRESS:  {}", user_t_address.encode(network));
    println!("USER WALLET ADDRESS:    {}", user_ua_str);
    println!("*************************");

    wallet_state.lock().unwrap().user_recv_ua = user_ua_str;


    println!("waiting for zaino to be ready...");
    wait_for_zainod().await;
    //////////////////////////////////////////////////////////////////////////////////

    // TODO: use tenderlink types & printing routines
    let mut roster: Vec<RosterMember> = Vec::new();
    let mut block_cache = MemBlockCache::new();

    // @todo(judah): investigate why requests get randomly dropped in a strange way:
    // transport error, service not ready, etc.
    let mut client = loop {
        if let Ok(channel) = Channel::from_static("http://localhost:18233").connect().await {
            break CompactTxStreamerClient::new(channel);
        }

        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    };

    let mut time_since_last_transparent_shielded = std::time::Instant::now() - std::time::Duration::from_secs(1000);

    let (mut user_use_i,  mut user_update_i)  = (0,0);
    let (mut miner_use_i, mut miner_update_i) = (0,0);
    let mut user_wallets  = [user_wallet_init,  WalletDb::for_path(":memory:", network, SystemClock, OsRng).unwrap()];
    let mut miner_wallets = [miner_wallet_init, WalletDb::for_path(":memory:", network, SystemClock, OsRng).unwrap()];

    let mut stupid_thing_because_judah_is_tired_and_wants_this_to_work_properly = Vec::<TxId>::new();

    loop {
        match client.get_roster(Empty{}).await {
            Err(err) => println!("Get roster error: {err:?}"),
            Ok(res) => {
                use std::io::{ Cursor,Read };
                let roster_bytes = res.into_inner().data;

                let mut ok = roster_bytes.len() > 0;
                let mut cur = Cursor::new(&roster_bytes);

                let mut new_roster = Vec::new();
                let mut num_buf = [0u8; 8];
                'read: while cur.position() < roster_bytes.len() as u64 {
                    let mut m = RosterMember{ pub_key: [0;32], voting_power:0, txids: Vec::new() };
                    if let Err(err) = cur.read_exact(&mut m.pub_key) {
                        println!("******* ROSTER DESERIALIZE ERROR: {err:?}");
                        ok = false;
                        break;
                    }
                    if let Err(err) = cur.read_exact(&mut num_buf) {
                        println!("******* ROSTER DESERIALIZE ERROR: {err:?}");
                        ok = false;
                        break;
                    }
                    m.voting_power = u64::from_le_bytes(num_buf);

                    if let Err(err) = cur.read_exact(&mut num_buf) {
                        println!("******* ROSTER DESERIALIZE ERROR: {err:?}");
                        ok = false;
                        break;
                    }

                    let mut voting_power_check = 0;
                    let txids_n = u64::from_le_bytes(num_buf);
                    for _ in 0..txids_n {
                        let mut stake_txid = StakeTxId{ txid:[0;32], zats:0 };
                        if let Err(err) = cur.read_exact(&mut stake_txid.txid) {
                            println!("******* ROSTER DESERIALIZE ERROR: {err:?}");
                            ok = false;
                            break 'read;
                        }
                        if let Err(err) = cur.read_exact(&mut num_buf) {
                            println!("******* ROSTER DESERIALIZE ERROR: {err:?}");
                            ok = false;
                            break 'read;
                        }
                        stake_txid.zats = u64::from_le_bytes(num_buf);
                        voting_power_check += stake_txid.zats;
                        m.txids.push(stake_txid);
                    }

                    if m.voting_power != voting_power_check {
                        // TODO: use manually-found one?
                        println!("******* RECEIVED ROSTER VOTING POWER INACCURATE: {} vs {}", m.voting_power, voting_power_check);
                        // ok = false;
                        // break;
                    }

                    new_roster.push(m);
                }

                if ok {
                    roster = new_roster;
                }

                wallet_state.lock().unwrap().roster = roster
                    .iter()
                    .map(|member| WalletRosterMember{
                        pub_key: member.pub_key,
                        voting_power: member.voting_power,
                        txids: member.txids.clone()
                    })
                    .collect::<Vec<WalletRosterMember>>()
                    .clone();
            }
        }
        println!("*********** ROSTER: {roster:?}");

        let Ok(info) = client.get_lightd_info(Empty {}).await else {
            println!("Failed to get lightd info");
            continue;
        };
        let network_tip_height = info.into_inner().block_height;

        if let Ok(chain_height) = miner_wallets[miner_update_i].chain_height() {
            if let Some(chain_height) = chain_height {
                if network_tip_height == u64::from(chain_height) {
                    println!("DOUBLE WALLET: flipping miner");
                    w_flip(&mut miner_use_i, &mut miner_update_i);
                    (miner_wallets[miner_update_i], miner_account) = wallet_from_stuff(network, Secret::new(miner_seed.expose_secret().clone()));
                }
            }
        }

        if let Ok(chain_height) = user_wallets[user_update_i].chain_height() {
            if let Some(chain_height) = chain_height {
                if network_tip_height == u64::from(chain_height) {
                    println!("DOUBLE WALLET: flipping user");
                    w_flip(&mut user_use_i, &mut user_update_i);
                    (user_wallets[user_update_i], user_account) = wallet_from_stuff(network, Secret::new(user_seed.expose_secret().clone()));
                }
            }
        }

        let (user_wallet, miner_wallet) = (&mut user_wallets[user_use_i], &mut miner_wallets[miner_use_i]);
        // Sync wallet DBs
        for (wallet, t_address) in [(miner_wallet, miner_t_address), (user_wallet, user_t_address)] {
            // TODO: outside loop?
            if 'needs_to_sync: /* what a funny language */ {
                if let Ok(chain_height) = wallet.chain_height() {
                    if let Some(chain_height) = chain_height {
                        network_tip_height != u64::from(chain_height)
                    } else {
                        network_tip_height > 1
                    }
                } else {
                    true
                }
            }
            {
                const MAX_BLOCKS_TO_DOWNLOAD_AT_TIME: u32 = 64;
                if let Err(err) = zcash_client_backend::sync::run(&mut client, network, &mut block_cache, wallet, MAX_BLOCKS_TO_DOWNLOAD_AT_TIME).await {
                    println!("Failed to sync wallet: {}", err);
                    continue;
                }
            }

            let Ok(summary) = wallet.get_wallet_summary(block_policy_10()) else { continue; };
            let Some(summary) = summary else { continue; };

            let balances = summary.account_balances();
            println!("******* WALLET {:?} *******", t_address.encode(network));
            println!("BALANCES {:?}", balances);
            println!("SUMMARY  {:?}", summary);
        }

        let (user_wallet, miner_wallet) = (&mut user_wallets[user_use_i], &mut miner_wallets[miner_use_i]);
        if time_since_last_transparent_shielded.elapsed().as_secs() > 15 {
            // Shield miner's transparent ZATOSHIz
            (async | wallet: &mut WalletDb<_, _, _, _>, account: &zcash_client_sqlite::wallet::Account, usk: &UnifiedSpendingKey | {
                let summary = match wallet.get_wallet_summary(block_policy_10()) {
                    Ok(summary) => summary,
                    Err(err) => {
                        println!("Failed to get wallet summary: {}", err);
                        return;
                    }
                };

                let Some(summary) = summary else { return; };

                if summary.chain_tip_height() != summary.fully_scanned_height() { return; }
                println!("#############*************############# === CREATING SHIELDING TRANSACTION FOR MINING OUTPUTS");

                let Some((t_addr, _ua)) = addrs_from_account(&account, 0) else {
                    println!("Failed to get transparent address from account!");
                    return;
                };

                const FEE_RULE: StandardFeeRule = StandardFeeRule::Zip317;
                const FALLBACK_CHANGE_POOL: zcash_protocol::ShieldedProtocol = zcash_protocol::ShieldedProtocol::Orchard;
                let change_strategy = fees::standard::SingleOutputChangeStrategy::new(
                    FEE_RULE,
                    None,
                    FALLBACK_CHANGE_POOL,
                    fees::DustOutputPolicy::default(),
                );
                let min_zats_for_shielding = Zatoshis::const_from_u64(10_000);
                let t_shield = Timer::scope("wallet::propose_shielding");
                match wallet::propose_shielding::<_, _, _, _, Infallible>(
                    wallet,
                    network,
                    &wallet::input_selection::GreedyInputSelector::new(),
                    &change_strategy,
                    min_zats_for_shielding,
                    &[t_addr],
                    account.id(),
                    wallet::ConfirmationsPolicy::MIN,
                ) {
                    Ok(proposal) => {
                        let prover = LocalTxProver::bundled();
                        let txids = match wallet::create_proposed_transactions::<_, _, Infallible, _, Infallible, _>(
                            wallet,
                            network,
                            &prover,
                            &prover,
                            &wallet::SpendingKeys::from_unified_spending_key(usk.clone()),
                            zcash_client_backend::wallet::OvkPolicy::Sender,
                            &proposal,
                            None,
                        ) {
                            Ok(txids) => txids,
                            Err(err) => {
                                println!("Failed to create transactions: {:?}", err);
                                return;
                            },
                        };

                        for txid in txids {
                            let tx = match wallet.get_transaction(txid) {
                                Ok(Some(tx)) => tx,
                                Ok(None) => {
                                    println!("failed to get tx {txid:?} immediately after making it: (None)");
                                    return;
                                }
                                Err(err) => {
                                    println!("failed to get tx {txid:?} immediately after making it: {err:?}");
                                    return;
                                }
                            };

                            let mut data = Vec::new();
                            if let Err(err) = tx.write(&mut data) {
                                println!("Serialization error for tx {:?}: {:?}", txid, err);
                                return;
                            }

                            let raw_tx = RawTransaction { data, height: 0 };
                            match client.send_transaction(raw_tx).await {
                                Ok(res) => println!("sent transaction: {res:?}"),
                                Err(err) => println!("failed to send transaction: {err:?}"),
                            }

                            time_since_last_transparent_shielded = std::time::Instant::now();
                            println!("created transaction {txid:?}");
                        }
                    }
                    Err(err) => {
                        println!("Failed to propose shielding: {:?}", err);
                        return;
                    }
                }
                // drop(t_shield);
            })(miner_wallet, &miner_account, &miner_usk).await;
        }

        // Update gui wallet state
        (async |user_wallet: &mut WalletDb<_, _, _, _>, miner_wallet: &mut WalletDb<_, _, _, _>| {
            let user_summary = match user_wallet.get_wallet_summary(block_policy_10()) {
                Ok(Some(summary)) => summary,
                Ok(None) => return,
                Err(err) => {
                    println!("Failed to get wallet summary: {}", err);
                    return;
                }
            };

            let balances = user_summary.account_balances();
            let mut spendable_balance = 0;
            let mut pending_balance   = 0;
            for (_, b) in balances {
                spendable_balance += b.spendable_value().into_u64();
                pending_balance   += b.value_pending_spendability().into_u64();
            }

            use core::ops::Add;
            let miner_vals = match miner_wallet.get_wallet_summary(block_policy_10()) {
                Ok(Some(summary)) => {
                    let bals = summary.account_balances();

                    let mut vals = (0,0,0);
                    for bal in bals {
                        let Ok(sh) = (*bal.1.orchard_balance() + *bal.1.sapling_balance()) else { continue; };
                        vals.0 += <u64>::from(bal.1.unshielded_balance().spendable_value());
                        vals.1 += <u64>::from(sh.change_pending_confirmation()) + <u64>::from(sh.value_pending_spendability());
                        vals.2 += <u64>::from(sh.spendable_value());
                    }
                    Some(vals)
                },
                _ => None
            };


            println!("WALLET HAS {} ({})) cTAZ", spendable_balance, str_from_ctaz(spendable_balance));

            let txs = if let Ok(mut history) = get_transaction_history(user_wallet) {
                if let Some(map) = get_received_memos_and_actions(&mut client, user_wallet, network, &history).await {
                    user_txid_map = map;

                    let mut user_staked_txids = Vec::new();
                    let mut total_staked: u64 = 0;
                    for mem in &roster {
                        for mem_txid in &mem.txids {
                            let txid = TxId::from_bytes(mem_txid.txid);
                            let Some((action, memos)) = user_txid_map.get(&txid) else { continue; };
                            let Some(action) = action else { continue; };
                            match action.kind {
                                StakingActionKind::Add => {
                                    if !stupid_thing_because_judah_is_tired_and_wants_this_to_work_properly.contains(&txid) {
                                        total_staked += mem_txid.zats;
                                        user_staked_txids.push((mem.pub_key, *txid.as_ref(), action.val, mem_txid.zats))
                                    }
                                }

                                _ => {}
                            }
                        }
                    }

                    {
                        let mut wallet_lock = wallet_state.lock().unwrap();
                        wallet_lock.staked_roster  = user_staked_txids;
                        wallet_lock.staked_balance = total_staked.try_into().unwrap();
                    }

                    let mut txs: Vec<WalletTx> = history.iter().map(|tx| {
                        let mut kind: WalletTxKind;
                        if tx.is_shielding {
                            kind = WalletTxKind::Shield;
                        }
                        else if tx.account_value_delta.is_negative() {
                            if tx.memo_count > 0 {
                                kind = WalletTxKind::Stake;
                            } else {
                                kind = WalletTxKind::Send;
                            }
                        }
                        else if tx.account_value_delta.is_positive() {
                            kind = WalletTxKind::Receive;
                        }
                        else {
                            kind = WalletTxKind::Receive;
                        }

                        let mut tx = WalletTx(tx.clone(), kind);
                        if let Some((_, memos)) = user_txid_map.get(&tx.0.txid) {
                            if memos.len() > 0 {
                                if memos.len() > 1 {
                                    println!("received multiple memos in 1 transaction: {}", memos.len());
                                }
                                let bytes = memos[0].as_bytes();
                                if bytes.len() > tx.0.memo.len() {
                                    println!("memo too big ({}/{}):\"\"\"\n{}\n\"\"\"", bytes.len(), memos[0].len(), memos[0]);
                                }
                                let len = bytes.len().min(tx.0.memo.len());
                                tx.0.memo[..len].copy_from_slice(&bytes[..len]);
                            }
                        }

                        tx
                    })
                    .collect();

                    // @todo(judah): because of the database, we can't differentiate regular receives
                    // and staking receives... This is how we do that for now.
                    for tx in &mut txs {
                        if tx.0.memo.starts_with("@UNSTAKE_RECEIVE:".as_bytes()) {
                            tx.1 = WalletTxKind::Unstake;
                        }
                    }

                    Some(txs)
                } else {
                    None
                }
            } else {
                None
            };

            let tip_h: Option<u32> = if let Ok(Some(val)) = miner_wallet.chain_height() {
                Some(val.into())
            } else {
                None
            };

            let faucet_available = if let Some(tip_h) = tip_h {
                // Calculate the funds available for faucet;
                // This would be better done incrementally on initial scan, accounting for reorgs etc
                let h = tip_h.saturating_sub(MIN_TRANSPARENT_COINBASE_MATURITY + 2); // account for coinbase maturing & shielding tx

                if let Ok(history) = get_transaction_history(miner_wallet) {
                    let mut coinbase_total = 0;
                    let mut faucet_spent = 0;
                    let mut staking_spent = 0;
                    for tx in history {
                        if tx.is_shielding {
                            if let Some(height) = tx.mined_height {
                                let height: u64 = height.try_into().unwrap();
                                if height + (MIN_TRANSPARENT_COINBASE_MATURITY as u64 + 2 as u64) < tip_h as u64 {
                                    coinbase_total += tx.total_received.into_u64();
                                }
                            }
                        } else if tx.total_spent.into_u64() > 0 {
                            if tx.memo_count > 0 {
                                faucet_spent += tx.total_spent.into_u64();
                            } else {
                                staking_spent += tx.total_spent.into_u64();
                            }
                        }
                    }

                    println!("coinbase_total: {coinbase_total}");
                    println!("faucet_total: {}", coinbase_total/2);
                    println!("faucet_spent: {faucet_spent}");
                    println!("staking_spent: {staking_spent}");
                    Some((coinbase_total/2).saturating_sub(faucet_spent))
                } else {
                    None
                }
            } else {
                None
            };

            let mut automatically_send_to_the_user = false;

            {
                let mut wallet_lock = wallet_state.lock().unwrap();
                wallet_lock.balance         = spendable_balance as i64;
                wallet_lock.pending_balance = pending_balance   as i64;

                if let Some(txs) = txs {
                    wallet_lock.waiting_for_faucet = false; // TODO:???
                    wallet_lock.txs = txs;
                }
                if let Some(tip_h) = tip_h {
                    wallet_lock.miner_seen_height = tip_h;
                }
                if let Some(faucet_available) = faucet_available {
                    automatically_send_to_the_user = faucet_available > 500_000_000; // @NOCHECKIN
                    wallet_lock.faucet_funds_available = faucet_available;
                }
                if let Some(vals) = miner_vals {
                    wallet_lock.miner_unshielded_funds = vals.0;
                    wallet_lock.miner_shielded_pending_funds = vals.1;
                    wallet_lock.miner_shielded_spendable_funds = vals.2;
                }
            }

            if automatically_send_to_the_user {
                let Some((_, user_ua)) = addrs_from_account(&user_account, 0) else {
                    println!("Failed to get transparent address from account!");
                    return;
                };

                let zats = (Zatoshis::from_nonnegative_i64(500_000_000).unwrap() - MINIMUM_FEE).unwrap();
                // NOTE: we can't send transparent->transparent through the high-level API, we
                // have to propose_shielding first, then send in a later block
                const FALLBACK_CHANGE_POOL: zcash_protocol::ShieldedProtocol = zcash_protocol::ShieldedProtocol::Orchard;
                match wallet::propose_standard_transfer_to_address::<_, _, Infallible>(
                    miner_wallet,
                    network,
                    zcash_client_backend::fees::StandardFeeRule::Zip317,
                    miner_account.id(),
                    block_policy_10(),
                    // &zcash_client_backend::address::Address::Transparent(user_t_addr),
                    &zcash_client_backend::address::Address::Unified(user_ua.clone()),
                    zats,
                    Some(zcash_protocol::memo::MemoBytes::from_bytes("Happy spending, with love from your favourite faucet".as_bytes()).unwrap()),
                    None,
                    FALLBACK_CHANGE_POOL)
                {
                    Err(err) => {
                        println!("propose_transfer error: {err:?}");
                        wallet_state.lock().unwrap().waiting_for_faucet = false;
                    },
                    Ok(proposal) => {
                        let prover = LocalTxProver::bundled();
                        match wallet::create_proposed_transactions::<_, _, Infallible, _, Infallible, _>(
                            miner_wallet,
                            network,
                            &prover,
                            &prover,
                            &wallet::SpendingKeys::from_unified_spending_key(miner_usk.clone()),
                            zcash_client_backend::wallet::OvkPolicy::Sender,
                            &proposal,
                            None,
                        ) {
                            Err(err) => println!("create_proposed_transactions error: {err:?}"),
                            Ok(txids) => for txid in txids {
                                let tx = match miner_wallet.get_transaction(txid) {
                                    Err(err) => {
                                        println!("failed to get tx {txid:?} immediately after making it: {err:?}");
                                        wallet_state.lock().unwrap().waiting_for_faucet = false;
                                        continue;
                                    }
                                    Ok(Some(tx)) => tx,
                                    Ok(None) => {
                                        println!("failed to get tx {txid:?} immediately after making it: (None)");
                                        wallet_state.lock().unwrap().waiting_for_faucet = false;
                                        continue;
                                    }
                                };

                                let mut data = Vec::new();
                                if let Err(err) = tx.write(&mut data) {
                                    println!("Serialization error for tx {:?}: {:?}", txid, err);
                                    wallet_state.lock().unwrap().waiting_for_faucet = false;
                                    continue;
                                }

                                let raw_tx = RawTransaction { data, height: 0 };

                                match client.send_transaction(raw_tx).await {
                                    Ok(res) => println!("sent transaction: {res:?}"),
                                    Err(err) => {
                                        println!("failed to send transaction: {err:?}");
                                        wallet_state.lock().unwrap().waiting_for_faucet = false;
                                    }
                                }

                                println!("created transaction {txid:?}");
                                // already_sent = true;
                            },
                        }
                    }
                }
            }
        })(user_wallet, miner_wallet).await;

        (async |miner_wallet: &mut WalletDb<_, _, _, _>| {
            if let Ok(history) = get_transaction_history(miner_wallet) {
                if let Some(map) = get_received_memos_and_actions(&mut client, miner_wallet, network, &history).await {
                    miner_txid_map = map;
                }
            }
        })(miner_wallet).await;
        // Process gui wallet actions

        // @todo(judah): I'm thinking the weird frame hitch we get in the UI is caused by this loop,
        // since it's probably waiting for the wallet_state mutex to unlock.
        let mut retries_this_round = 6;
        loop {
            let action: WalletAction = {
                let mut wallet_lock = wallet_state.try_lock();
                let Ok(wallet_state) = &mut wallet_lock else {
                    if retries_this_round > 0 {
                        retries_this_round -= 1;
                        println!("wallet lock retry ({retries_this_round} attempts remaining)");
                        tokio::time::sleep(tokio::time::Duration::from_millis(9)).await;
                        continue;
                    } else {
                        break;
                    }
                };

                println!("*** wallet has {:?} actions in flight", wallet_state.actions_in_flight.len());
                let Some(action) = wallet_state.actions_in_flight.front() else { break; };
                action.clone()
            };

            let ok: bool = 'process_action: {
                match &action {
                    WalletAction::RequestFromFaucet => {
                        // NOTE: we can't send transparent->transparent through the high-level API, we
                        // have to propose_shielding first, then send in a later block
                        let zats = (Zatoshis::from_nonnegative_i64(500_000_000).unwrap() - MINIMUM_FEE).unwrap();
                        match send_zats_to_wallet(&mut client, user_wallet, miner_wallet, &miner_usk, zats, network, &TxOptions{
                            memo: Some(zcash_protocol::memo::MemoBytes::from_bytes("Happy spending, with love from your favourite faucet".as_bytes()).unwrap()),
                            ..TxOptions::default()
                        }).await {
                            None => {
                                wallet_state.lock().unwrap().waiting_for_faucet = false;
                                true
                            }
                            Some(_) => {
                                wallet_state.lock().unwrap().waiting_for_faucet = false;
                                true
                            },
                        }
                    }

                    WalletAction::StakeToFinalizer(amount, target_finalizer) => {
                        let Ok(Some(wallet_summary)) = user_wallet.get_wallet_summary(ConfirmationsPolicy::MIN) else {
                            println!("Failed to get wallet summary");
                            break 'process_action false;
                        };

                        let mut spendable = 0;
                        let balances = wallet_summary.account_balances();
                        for (_, b) in balances {
                            spendable += b.spendable_value().into_u64();
                        }

                        // @todo(judah): better check?
                        let amount_with_fee = (*amount - MINIMUM_FEE).unwrap();
                        if spendable < amount.into_u64() {
                            println!("Not enough spendable zats to stake, will try again later...");
                            break 'process_action false;
                        }

                        println!("********** STAKING ZEC {:?} ({:?}) TO THE MINER but also to {:?}", amount, amount_with_fee, target_finalizer);
                        let opts = TxOptions {
                            staking_action: Some(StakingAction {
                                kind: StakingActionKind::Add,
                                val: amount_with_fee.into_u64(),
                                target: *target_finalizer,
                                source: [0_u8; 32],
                                insecure_target_name: "".to_owned(),
                                insecure_source_name: "".to_owned(),
                            }),
                            memo: Some(zcash_protocol::memo::MemoBytes::from_bytes(user_ua.encode(network).to_string().as_bytes()).unwrap()),
                        };

                        match send_zats_to_wallet(&mut client, miner_wallet, user_wallet, &user_usk, amount_with_fee, network, &opts).await {
                            None => {
                                println!("Failed to send ZEC to miner");
                                wallet_state.lock().unwrap().waiting_for_stake_to_finalizer = false;
                                false
                            }
                            Some(_) => {
                                wallet_state.lock().unwrap().waiting_for_stake_to_finalizer = false;
                                true
                            }
                        }
                    }

                    WalletAction::SendToAddress(address, amount) => {
                        let Ok(Some(wallet_summary)) = user_wallet.get_wallet_summary(ConfirmationsPolicy::MIN) else {
                            println!("Failed to get wallet summary");
                            break 'process_action false;
                        };

                        let mut spendable = 0;
                        let balances = wallet_summary.account_balances();
                        for (_, b) in balances {
                            spendable += b.spendable_value().into_u64();
                        }

                        // @todo(judah): better check?
                        let amount_with_fee = (*amount - MINIMUM_FEE).unwrap();
                        if spendable < amount.into_u64() {
                            println!("Not enough spendable zats to send!");
                            break 'process_action false;
                        }

                        println!("*********** SEND ZEC {:?} ({:?}) TO {}", amount, amount_with_fee, &address.encode(network));
                        match send_zats(&mut client, &address, user_wallet, &user_usk, amount_with_fee, network, &TxOptions::default()).await {
                            None => {
                                println!("Failed to send ZEC to {}", address.encode(network));
                                wallet_state.lock().unwrap().waiting_for_send = false;
                                false
                            }
                            Some(_) => {
                                wallet_state.lock().unwrap().waiting_for_send = false;
                                true
                            }
                        }
                    }

                    WalletAction::UnstakeFromFinalizer(txid) => {
                        let mut ok = { // User sends unstaking action
                            let Some((member_pub_key, staked_txid)) = ('find_txid: {
                                for mem in &roster {
                                    for mem_txid in &mem.txids {
                                        if TxId::from_bytes(mem_txid.txid) == *txid {
                                            break 'find_txid Some((mem.pub_key, mem_txid.clone()));
                                        }
                                    }
                                }
                                None
                            }) else {
                                println!("*** Failed to find member with txid: {:?}", txid);
                                break 'process_action false;
                            };

                            let Some((action, _)) = user_txid_map.get(&txid) else {
                                println!("*** Failed to find user staking transaction via txid {:?}", txid);
                                break 'process_action false;
                            };

                            let Some(action) = action else {
                                println!("*** Staking action was unset in txid {:?}", txid);
                                break 'process_action false;
                            };

                            let opts = TxOptions {
                                staking_action: Some(StakingAction {
                                    kind: StakingActionKind::Sub, // @todo: clear?
                                    val: staked_txid.zats,
                                    target: member_pub_key,
                                    source: *txid.as_ref(),
                                    insecure_target_name: "".to_owned(),
                                    insecure_source_name: "".to_owned(),
                                }),
                                memo: None,
                            };

                            // @note(judah): the miner sends to its own address because if the user sends it,
                            // the tx will appear as a regular send of -0.2 cTAZ....
                            match send_zats(&mut client, &miner_ua, miner_wallet, &miner_usk, Zatoshis::from_u64(10_000).unwrap() /* @todo fees */, network, &opts).await {
                                None => {
                                    println!("Failed to send unstaking action to miner");
                                    false
                                }
                                Some(_) => {
                                    println!("Successfully sent unstaking action to miner");
                                    true
                                }
                            }
                        };

                        ok &= { // Miner sends reward back to user
                            let Some(staked_txid) = ('find_txid: {
                                for mem in &roster {
                                    for mem_txid in &mem.txids {
                                        if TxId::from_bytes(mem_txid.txid) == *txid {
                                            break 'find_txid Some(mem_txid.clone());
                                        }
                                    }
                                }
                                None
                            }) else {
                                println!("*** Failed to find member with txid: {:?}", txid);
                                break 'process_action false;
                            };

                            let Some((action, memos)) = miner_txid_map.get(&txid) else {
                                println!("*** Failed to find miner staking transaction via txid {:?}", txid);
                                break 'process_action false;
                            };

                            let Some(destination_address) = memos.iter().find(|memo| memo.starts_with("utest")) else {
                                println!("*** Failed to find destination address memo in txid {:?}", txid);
                                break 'process_action false;
                            };

                            let destination_address = destination_address.trim_end_matches(|c| c == '\0');
                            let Ok(destination_ua) = UnifiedAddress::decode(network, destination_address) else {
                                println!("*** Failed to decode destination address {:?}", destination_address);
                                break 'process_action false;
                            };

                            let options = TxOptions{
                                memo: Some(zcash_protocol::memo::MemoBytes::from_bytes("@UNSTAKE_RECEIVE:Thanks for staking!".as_bytes()).unwrap()),
                                ..Default::default()
                            };

                            match send_zats(&mut client, &destination_ua, miner_wallet, &miner_usk, Zatoshis::from_u64(staked_txid.zats).unwrap(), network, &options).await {
                                None => {
                                    println!("Failed to send reward to user");
                                    false
                                }
                                Some(_) => {
                                    println!("Successfully sent reward to user");
                                    stupid_thing_because_judah_is_tired_and_wants_this_to_work_properly.push(*txid);
                                    true
                                }
                            }
                        };

                        ok
                    }

                    _ => { true }
                }
            };

            if !ok {
                println!("** Failed to process action: {:?}", &action);
            }

            wallet_state.lock().unwrap().actions_in_flight.pop_front();
        }

        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    }
}

/*
#[derive(Debug)]
struct DerVerifier {
    certificate: &'static [u8],
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for DerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &tonic::transport::CertificateDer<'_>,
        _intermediates: &[tonic::transport::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        if end_entity.as_ref() == self.certificate {
            Ok(ServerCertVerified::assertion())
        }
        else {
            Err(rustls::Error::InvalidCertificate(CertificateError::UnknownIssuer))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &tonic::transport::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &tonic::transport::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}
*/

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TransactionSummary<AccountId> {
    pub account_id: AccountId,
    pub txid: zcash_protocol::TxId,
    pub expiry_height: Option<BlockHeight>,
    pub mined_height: Option<BlockHeight>,
    pub account_value_delta: ZatBalance,
    pub total_spent: Zatoshis,
    pub total_received: Zatoshis,
    pub fee_paid: Option<Zatoshis>,
    pub spent_note_count: usize,
    pub has_change: bool,
    pub sent_note_count: usize,
    pub received_note_count: usize,
    pub memo_count: usize,
    pub expired_unmined: bool,
    pub is_shielding: bool,
    pub memo: [u8; 512],
}

impl<AccountId> TransactionSummary<AccountId> {
    /// Constructs a `TransactionSummary` from its parts.
    ///
    /// See the documentation for each getter method below to determine how each method
    /// argument should be prepared.
    #[allow(clippy::too_many_arguments)]
    pub fn from_parts(
        account_id: AccountId,
        txid: zcash_protocol::TxId,
        expiry_height: Option<BlockHeight>,
        mined_height: Option<BlockHeight>,
        account_value_delta: ZatBalance,
        total_spent: Zatoshis,
        total_received: Zatoshis,
        fee_paid: Option<Zatoshis>,
        spent_note_count: usize,
        has_change: bool,
        sent_note_count: usize,
        received_note_count: usize,
        memo_count: usize,
        expired_unmined: bool,
        is_shielding: bool,
        memo: [u8; 512],
    ) -> Self {
        Self {
            account_id,
            txid,
            expiry_height,
            mined_height,
            account_value_delta,
            total_spent,
            total_received,
            fee_paid,
            spent_note_count,
            has_change,
            sent_note_count,
            received_note_count,
            memo_count,
            expired_unmined,
            is_shielding,
            memo,
        }
    }
}
