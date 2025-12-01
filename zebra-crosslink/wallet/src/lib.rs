//! Internal wallet
#![allow(warnings)]

use zcash_client_backend::data_api::WalletCommitmentTrees;
use orchard::keys::SpendAuthorizingKey;
use orchard::note_encryption::CompactAction;
use rand_chacha::rand_core::SeedableRng;
use rand_core::OsRng;
use sapling_crypto::zip32::ExtendedSpendingKey;
use std::collections::VecDeque;
use std::convert::{identity, Infallible};
use std::future::Future;
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
use zcash_primitives::transaction::StakingAction;
use zcash_primitives::transaction::StakingActionKind;

pub static GLOBAL_SEED: Mutex<Option<[u8; 32]>> = Mutex::new(None);

fn the_future_is_now<F: Future>(future: F) -> F::Output {
    Builder::new_current_thread()
        .enable_time()
        .enable_io()
        .build()
        .unwrap()
        .block_on(future)
}

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

#[derive(Debug, Copy, Clone, PartialEq)]
enum WalletAction {
    RequestFromFaucet,
    TestStakeAction,
    StakeToMiner(Zatoshis, [u8; 32]),
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
    pub fn with_fake_data(kind: WalletTxKind, sent: u64, received: u64, shielding: bool, memo: &str) -> Self {
        let mut memo_as_bytes = [0u8; 512];
        &memo_as_bytes[0..memo.len()].copy_from_slice(memo.as_bytes());

        Self(
            TransactionSummary{
                account_id: AccountUuid::default(),
                txid: TxId::from_bytes([0; 32]),
                expiry_height: None,
                mined_height: Some(BlockHeight::from_u32(10)),
                account_value_delta: ZatBalance::from_i64((sent.saturating_sub(received)) as i64).unwrap(),
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

#[derive(Default, Debug, Clone)]
pub struct WalletState {
    pub balance: i64, // in zats
    pub pending_balance: i64, // in zats
    pub txs: Vec<WalletTx>,
    pub roster: Vec<RosterMember>,

    pub waiting_for_faucet: bool,
    pub waiting_for_stake_to_miner: bool,

    pub miner_seen_height: u32,
    pub miner_unshielded_funds: u64,
    pub miner_shielded_pending_funds: u64,
    pub miner_shielded_spendable_funds: u64,
    pub faucet_funds_available: u64,

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

    pub fn stake_to_miner(&mut self, amount: u64, target_finalizer: [u8; 32]) {
        self.waiting_for_stake_to_miner = true;

        if self.actions_in_flight.iter().filter(|a| match a { WalletAction::StakeToMiner(_,_) => true, _ => false }).count() != 0 {
            return;
        }

        self.actions_in_flight.push_back(WalletAction::StakeToMiner(Zatoshis::from_u64(amount).expect("Invalid amount given to stake_to_miner"), target_finalizer));
    }
}

pub fn str_from_ctaz(val: u64) -> String {
    let full = val / 100_000_000;
    let part = val % 100_000_000;
    let part_str = format!("{part}00");
    let trim_part = part_str.trim_end_matches("0");
    format!("{full}.{}", &part_str[..trim_part.len().max(3)])
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct RosterMember {
    pub pub_key: [u8; 32],
    pub stake: u64,
}

pub fn wallet_main(wallet_state: Arc<Mutex<WalletState>>) {
    fn wallet_from_seed_phrase<P: Parameters + 'static>(params: P, phrase: &str) -> (
        zcash_client_sqlite::WalletDb<rusqlite::Connection, P, SystemClock, OsRng>,
        zcash_client_sqlite::wallet::Account,
        UnifiedSpendingKey,
    ) {
        use secrecy::ExposeSecret;

        let mnemonic = bip39::Mnemonic::parse(phrase).unwrap();
        let bip39_passphrase = ""; // optional
        let seed64 = mnemonic.to_seed(bip39_passphrase);
        let seed = secrecy::SecretVec::new(seed64[..32].to_vec());
        let seed_fp = zip32::fingerprint::SeedFingerprint::from_seed(seed.expose_secret()).unwrap();
        let account_id = zip32::AccountId::try_from(0).unwrap();

        let usk = UnifiedSpendingKey::from_seed(&params, seed.expose_secret(), account_id).unwrap();
        let birthday = &AccountBirthday::from_parts(
            ChainState::empty(BlockHeight::from_u32(0), zcash_primitives::block::BlockHash([0; 32])),
            None,
        );

        let mut wallet = zcash_client_sqlite::WalletDb::for_path(":memory:", params, SystemClock, OsRng).unwrap();
        zcash_client_sqlite::wallet::init::init_wallet_db(
            &mut wallet,
            Some(secrecy::Secret::new(seed.expose_secret().clone())),
        ).unwrap();

        let (account_uuid, _) = wallet
            .create_account("main_account", &seed, birthday, None)
            .unwrap();
        let account = wallet.get_account(account_uuid).unwrap().unwrap();

        (wallet, account, usk)
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

        return Ok(results);
    }

    async fn get_received_memos<P: zcash_protocol::consensus::Parameters>(client: &mut CompactTxStreamerClient<Channel>, wallet: &zcash_client_sqlite::WalletDb<rusqlite::Connection, P, SystemClock, OsRng>, params: P) -> Vec<(TxId, String)> {

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

        let mut memos = Vec::new();
        let Ok(history) = get_transaction_history(&wallet) else { return memos; };
        let txids: Vec<TxId> = history.iter().map(|h| h.txid).collect();

        let Ok(ids) = wallet.get_account_ids() else { return memos; };
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

            let txdata = &tx.into_data();
            for uivk in &uivks {
                let possible_orchard_ivk = if let Some(orchard_ivk) = uivk.orchard() { Some(orchard_ivk.prepare()) } else { None };

                if let Some(orchard_ivk) = possible_orchard_ivk {
                    let m: Vec<(TxId, String)> = try_get_orchard_memos(txdata, &orchard_ivk)
                        .iter()
                        .map(|memo| (*txid, memo.clone()))
                        .collect();

                    memos.extend_from_slice(&m[..]);
                }
            }
        }

        memos
    }

    let send_zats = async | client: &mut CompactTxStreamerClient<_>, dst_wallet: &mut WalletDb<_, _, _, _>, src_wallet: &mut WalletDb<_, _, _, _>, src_usk: &UnifiedSpendingKey, zats: Zatoshis, params, staking_action: Option<StakingAction>| -> bool {
        // @todo(judah): handle multiple accounts?
        let Ok(src_ids)  = src_wallet.get_account_ids() else { return false; };
        let Some(src_id) = src_ids.first() else { return false; };
        let Ok(Some(src_account)) = src_wallet.get_account(*src_id) else { return false; };

        let Ok(dst_ids)  = dst_wallet.get_account_ids() else { return false; };
        let Some(dst_id) = dst_ids.first() else { return false; };
        let Ok(Some(dst_account)) = dst_wallet.get_account(*dst_id) else { return false; };
        let Some((_, dst_ua)) = addrs_from_account(&dst_account, 0) else { return false; };

        const FALLBACK_CHANGE_POOL: zcash_protocol::ShieldedProtocol = zcash_protocol::ShieldedProtocol::Orchard;

        match wallet::propose_standard_transfer_to_address::<_, _, Infallible>(
            src_wallet,
            params,
            zcash_client_backend::fees::StandardFeeRule::Zip317,
            src_account.id(),
            wallet::ConfirmationsPolicy::MIN,
            &zcash_client_backend::address::Address::Unified(dst_ua.clone()),
            zats,
            Some(zcash_protocol::memo::MemoBytes::from_bytes(dst_ua.encode(params).to_string().as_bytes()).unwrap()),
            None,
            FALLBACK_CHANGE_POOL)
        {
            Err(err) => {
                println!("propose_transfer error: {err:?}");
                return false;
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
                    staking_action,
                ) {
                    Err(err) => println!("create_proposed_transactions error: {err:?}"),
                    Ok(txids) => for txid in txids {
                        let tx = match src_wallet.get_transaction(txid) {
                            Err(err) => {
                                println!("failed to get tx {txid:?} immediately after making it: {err:?}");
                                continue;
                            }
                            Ok(Some(tx)) => tx,
                            Ok(None) => {
                                println!("failed to get tx {txid:?} immediately after making it: (None)");
                                continue;
                            }
                        };

                        let mut data = Vec::new();
                        if let Err(err) = tx.write(&mut data) {
                            println!("Serialization error for tx {:?}: {:?}", txid, err);
                            continue;
                        }

                        let raw_tx = RawTransaction { data, height: 0 };
                        match client.send_transaction(raw_tx).await {
                            Ok(res)  => println!("sent transaction: {res:?}"),
                            Err(err) => {
                                continue;
                            }
                        }

                        println!("created transaction {txid:?}");
                    }
                }
            }
        }

        return true;
    };

    the_future_is_now(async {
        println!("waiting for zaino to be ready...");
        wait_for_zainod().await;
    });

    let global_seed = loop {
        if let Some(global_seed) = *GLOBAL_SEED.lock().unwrap() {
            break global_seed;
        }
    };

    let network = &TEST_NETWORK;

    let (
        mut miner_wallet,
        miner_account,
        miner_usk,
        miner_pubkey,
        miner_privkey,
        miner_t_address,
    ) = {
        let (miner_wallet, miner_account, miner_usk) = wallet_from_seed_phrase(network,
            "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about"
        );
        let (miner_t_addr, miner_ua) = addrs_from_account(&miner_account, 0).unwrap();
        let miner_t_addr_str = miner_t_addr.encode(network);
        let (miner_pubkey, miner_privkey) = transparent_keys_from_usk(&miner_usk, 0).unwrap();
        let miner_t_recs = miner_wallet
            .get_transparent_receivers(miner_account.id(), false, false)
            .unwrap();
        (miner_wallet, miner_account, miner_usk, miner_pubkey, miner_privkey, miner_t_addr)
    };

    let (
        mut user_wallet,
        user_account,
        user_usk,
        user_pubkey,
        user_privkey,
        user_t_address,
        user_ua,
    ) = {
        // roundtrip seed through mnemonic phrase
        let mnemonic = bip39::Mnemonic::from_entropy_in(bip39::Language::English, &global_seed).unwrap();
        let phrase = mnemonic.words().map(|s| s.to_string()).collect::<Vec<String>>().join(" ");

        let (user_wallet, user_account, user_usk) = wallet_from_seed_phrase(network, &phrase);
        let (user_t_addr, user_ua) = addrs_from_account(&user_account, 0).unwrap();
        let user_t_addr_str = user_t_addr.encode(network);
        let (user_pubkey, user_privkey) = transparent_keys_from_usk(&user_usk, 0).unwrap();
        let user_t_recs = user_wallet
            .get_transparent_receivers(user_account.id(), false, false)
            .unwrap();

        // let user_t_addr1 = user_t_recs.into_iter().filter(|(addr, _)| addr == &user_t_addr).next().unwrap().0;
        // NOTE: the default isn't the same as below, but I think this is because it forces a diversifier index
        // println!("User wallet: {}/{:?}", user_t_addr_str, user_t_addr1.encode(network));

        (user_wallet,  user_account, user_usk, user_pubkey, user_privkey, user_t_addr, user_ua)
    };

    println!("*************************");
    println!("MINER WALLET ADDRESS: {}", miner_t_address.encode(network));
    println!("USER WALLET ADDRESS:  {}", user_t_address.encode(network));
    println!("*************************");

    // TODO: use tenderlink types & printing routines
    let mut roster: Vec<RosterMember> = Vec::new();
    let mut block_cache = MemBlockCache::new();
    the_future_is_now(async {
        // @todo(judah): investigate why requests get randomly dropped in a strange way:
        // transport error, service not ready, etc.
        let mut client = loop {
            if let Ok(channel) = Channel::from_static("http://localhost:18233").connect().await {
                break CompactTxStreamerClient::new(channel);
            }

            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        };

        loop {
            if let Ok(res) = client.get_roster(Empty{}).await {
                use std::io::{ Cursor,Read };
                let roster_bytes = res.into_inner().data;

                let mut ok = roster_bytes.len() > 0;
                let mut cur = Cursor::new(&roster_bytes);

                let mut new_roster = Vec::new();
                let mut num_buf = [0u8; 8];
                while cur.position() < roster_bytes.len() as u64 {
                    let mut m = RosterMember{ pub_key: [0;32], stake:0 };
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
                    m.stake = u64::from_le_bytes(num_buf);
                    new_roster.push(m);
                }

                if ok {
                    roster = new_roster;
                }
                wallet_state.lock().unwrap().roster = roster.clone();
            }
            println!("*********** ROSTER: {roster:?}");

            // Sync wallet DBs
            for (wallet, t_address) in [(&mut miner_wallet, miner_t_address), (&mut user_wallet, user_t_address)] {
                // TODO: outside loop?
                let Ok(info) = client.get_lightd_info(Empty {}).await else {
                    println!("Failed to get lightd info");
                    continue;
                };

                let network_tip_height = info.into_inner().block_height;

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

                let Ok(summary) = wallet.get_wallet_summary(ConfirmationsPolicy::MIN) else { continue; };
                let Some(summary) = summary else { continue; };

                let balances = summary.account_balances();
                println!("******* WALLET {:?} *******", t_address.encode(network));
                println!("BALANCES {:?}", balances);
                println!("SUMMARY  {:?}", summary);

                println!("MEMOS");
                let memos = get_received_memos(&mut client, wallet, network).await;
                for memo in memos {
                    println!("\tTx {:?}: {}", memo.0, memo.1);
                }
            }

            // Shield miner's transparent ZATOSHIz
            (async | wallet: &mut WalletDb<_, _, _, _>, account: &zcash_client_sqlite::wallet::Account, usk: &UnifiedSpendingKey | {
                let summary = match wallet.get_wallet_summary(ConfirmationsPolicy::MIN) {
                    Ok(summary) => summary,
                    Err(err) => {
                        println!("Failed to get wallet summary: {}", err);
                        return;
                    }
                };

                let Some(summary) = summary else { return; };

                // let Ok(chain_height) = wallet.chain_height() else {
                //     println!("Failed to get chain height");
                //     return;
                // };
                // let target_height: TargetHeight = match chain_height {
                //     Some(height) => (height + 1).into(),
                //     None => {
                //         return; // nothing to do yet
                //     }
                // };
                // let Ok(tbaances) = wallet.get_transparent_balances(miner_account.id(), target_height, ConfirmationsPolicy::MIN) else { return; };
                // let from_addrs = tbalances
                //     .into_keys();

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

                            println!("created transaction {txid:?}");
                        }
                    }
                    Err(err) => {
                        println!("Failed to propose shielding: {:?}", err);
                        return;
                    }
                }
            })(&mut miner_wallet, &miner_account, &miner_usk).await;

            // Update gui wallet state
            (async |user_wallet: &mut WalletDb<_, _, _, _>, miner_wallet: &mut WalletDb<_, _, _, _>| {
                let user_summary = match user_wallet.get_wallet_summary(ConfirmationsPolicy::MIN) {
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
                let miner_vals = match miner_wallet.get_wallet_summary(ConfirmationsPolicy::MIN) {
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
                    let memos = get_received_memos(&mut client, user_wallet, network).await;
                    let txs: Vec<WalletTx> = history.iter().map(|tx| {
                        let mut kind: WalletTxKind;
                        if tx.is_shielding {
                            kind = WalletTxKind::Shield;
                        } else if tx.received_note_count > 0 {
                            kind = WalletTxKind::Receive;
                        } else {
                            kind = WalletTxKind::Receive;
                        }

                        let mut tx = WalletTx(tx.clone(), kind);
                        if let Some(memo) = memos.iter().find(|m| m.0 == tx.0.txid) {
                            let bytes = memo.1.as_bytes();
                            tx.0.memo[0..bytes.len()].copy_from_slice(bytes);
                        }

                        tx
                    }).collect();

                    Some(txs)
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
                            println!("{tx:?}");
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
                        wallet_lock.txs = txs; // @temp: doesn't need to be its own type
                    }
                    if let Some(tip_h) = tip_h {
                        wallet_lock.miner_seen_height = tip_h;
                    }
                    if let Some(faucet_available) = faucet_available {
                        automatically_send_to_the_user = faucet_available > 500_000_000;
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
                        wallet::ConfirmationsPolicy::MIN,
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
            })(&mut user_wallet, &mut miner_wallet).await;

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
                    *action
                };
                let ok: bool = match action {
                    WalletAction::RequestFromFaucet => if false {
                        let Ok(miner_utxos) = client.get_address_utxos(GetAddressUtxosArg{
                            addresses: [miner_t_address.encode(network).to_string()].to_vec(),
                            start_height: 0,
                            max_entries: 0,
                        }).await else {
                            println!("Miner had no UTXOs to use");
                            break;
                        };

                        let miner_utxos = miner_utxos.into_inner().address_utxos;
                        if miner_utxos.is_empty() {
                            println!("Miner had no UTXOs to use");
                            break;
                        }

                        let zats = (Zatoshis::from_nonnegative_i64(miner_utxos[0].value_zat).unwrap() - MINIMUM_FEE).unwrap();

                        let mut signing_set = TransparentSigningSet::new();
                        signing_set.add_key(miner_privkey);

                        let prover = LocalTxProver::bundled();
                        let extsk: &[ExtendedSpendingKey] = &[];
                        let sak: &[SpendAuthorizingKey] = &[];

                        let script = zcash_transparent::address::Script(zcash_script::script::Code(miner_utxos[0].script.clone()));

                        let outpoint = OutPoint::new(miner_utxos[0].txid[..32].try_into().unwrap(), miner_utxos[0].index as u32);

                        let Some(target_height) = miner_wallet.chain_height().expect("Failed to get chain height") else {
                            println!("Failed to get miner's chain height");
                            break;
                        };

                        let mut txb = TxBuilder::new(
                            network,
                            target_height + 1,
                            BuildConfig::Standard {
                                sapling_anchor: None,
                                orchard_anchor: None,
                            },
                        );

                        txb.add_transparent_input(miner_pubkey, outpoint, TxOut::new((zats + MINIMUM_FEE).unwrap(), script)).unwrap();
                        txb.add_transparent_output(&user_t_address, zats).unwrap();

                        use rand_chacha::ChaCha20Rng;
                        let rng = ChaCha20Rng::from_rng(OsRng).unwrap();
                        let tx_res = txb.build(
                            &signing_set,
                            extsk,
                            sak,
                            rng,
                            &prover,
                            &prover,
                            &zip317::FeeRule::standard(),
                        ).unwrap();

                        let tx = tx_res.transaction();
                        let mut tx_bytes = vec![];
                        tx.write(&mut tx_bytes).unwrap();

                        match client.send_transaction(RawTransaction{ data: tx_bytes, height: 0 }).await {
                            Ok(_) => {
                                println!("Faucet transaction sent successfully");
                            }
                            Err(err) => {
                                println!("Failed to send faucet transaction: {}", err);
                                wallet_state.lock().unwrap().waiting_for_faucet = false;
                            },
                        }
                        true
                    } else {
                        let zats = (Zatoshis::from_nonnegative_i64(500_000_000).unwrap() - MINIMUM_FEE).unwrap();
                        // NOTE: we can't send transparent->transparent through the high-level API, we
                        // have to propose_shielding first, then send in a later block
                        const FALLBACK_CHANGE_POOL: zcash_protocol::ShieldedProtocol = zcash_protocol::ShieldedProtocol::Orchard;
                        match wallet::propose_standard_transfer_to_address::<_, _, Infallible>(
                            &mut miner_wallet,
                            network,
                            zcash_client_backend::fees::StandardFeeRule::Zip317,
                            miner_account.id(),
                            wallet::ConfirmationsPolicy::MIN,
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
                                    &mut miner_wallet, network,
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
                        true
                    }

                    WalletAction::StakeToMiner(amount, target_finalizer) => {
                        let Ok(Some(wallet_summary)) = user_wallet.get_wallet_summary(ConfirmationsPolicy::MIN) else {
                            println!("Failed to get wallet summary");
                            break;
                        };

                        let mut spendable = 0;
                        let balances = wallet_summary.account_balances();
                        for (_, b) in balances {
                            spendable += b.spendable_value().into_u64();
                        }

                        // @todo(judah): better check?
                        let amount_with_fee = (amount - MINIMUM_FEE).unwrap();
                        if spendable < amount.into_u64() {
                            println!("Not enough spendable zats to stake, will try again later...");
                            break;
                        }

                        println!("********** STAKING ZEC {:?} ({:?}) TO THE MINER but also to {:?}", amount, amount_with_fee, target_finalizer);
                        let ok = send_zats(&mut client, &mut miner_wallet, &mut user_wallet, &user_usk, amount_with_fee, network,
                            Some(StakingAction {
                                kind: StakingActionKind::Add,
                                val: amount_with_fee.into_u64(),
                                target: target_finalizer,
                                source: [0_u8; 32],
                                insecure_target_name: "".to_owned(),
                                insecure_source_name: "".to_owned(),
                            })
                        ).await;
                        if !ok {
                            println!("Failed to send ZEC to miner");
                            break;
                        }

                        if ok {
                            wallet_state.lock().unwrap().waiting_for_stake_to_miner = false;
                        }

                        ok
                    }

                    _ => { true }
                };
                if ok {
                    wallet_state.lock().unwrap().actions_in_flight.pop_front();
                }
            }

            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        };
    });

    // SOMEWHAT BROKEN MANUAL SYNC CODE
    /*
    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;

        // GET THE CURRENT STATE OF THE WORLD
        let (sync_failed, network_tip_height, tree_state) = 'sync_state: {
            let tree_state = if local_tip_height == 0 {
                TreeState::default()
            } else {
                match client.get_tree_state(BlockId {height: local_tip_height, ..Default::default()}).await {
                    Ok(info) => info.into_inner(),
                    Err(err) => {
                        println!("Failed to get tree state: {:?}", err);
                        break 'sync_state (true, 0, TreeState::default());
                    }
                }
            };

            let Ok(info) = client.get_lightd_info(Empty {}).await else {
                println!("Failed to get lightd info");
                break 'sync_state (true, 0, TreeState::default());
            };

            let network_tip_height = info.into_inner().block_height;

            match client.get_block_range(BlockRange {
                start: Some(BlockId { height: local_tip_height,   ..Default::default() }),
                end:   Some(BlockId { height: network_tip_height, ..Default::default() }),
            }).await {
                Ok(blocks) => {
                    let mut new_blocks: Vec<CompactBlock> = Vec::new();
                    let mut block_stream = blocks.into_inner();
                    loop {
                        match block_stream.message().await {
                            Ok(Some(block)) => {
                                local_tip_height = block.height;
                                new_blocks.push(block)
                            }
                            Ok(None) => break,
                            Err(err) => {
                                if err.code() == tonic::Code::OutOfRange {
                                    break;
                                } else {
                                    println!("Failed to get block: {:?}", err);
                                    break;
                                }
                            }
                        }
                    }

                    if let Err(err) = block_cache.insert(new_blocks).await {
                        println!("Failed to update block cache: {:?}", err);
                        break 'sync_state (true, 0, TreeState::default());
                    }
                }
                Err(err) => {
                    println!("Failed to get block range: {:?}", err);
                    break 'sync_state (true, 0, TreeState::default());
                }
            }

            (false, network_tip_height, tree_state)
        };

        if sync_failed {
            continue;
        }

        let (reorg_required) = 'process_blocks: {
            let mut reorg_required = false;
            for wallet in [&mut miner_wallet, &mut user_wallet] {
                // use zcash_client_backend::data_api::WalletCommitmentTrees;

                if let Err(err) = wallet.update_chain_tip(BlockHeight::from_u32(local_tip_height as u32)) {
                    println!("Failed to update chain tip: {:?}", err);
                }

                let mut scan_ranges = match wallet.suggest_scan_ranges() {
                    Err(err) => {
                        println!("Failed to get scan ranges: {:?}", err);
                        continue;
                    }
                    Ok(scan_ranges) => scan_ranges,
                };

                while let Some(scan_range) = scan_ranges.first() {
                    match scan_range.priority() {
                        ScanPriority::Verify => {
                            let previous_height = scan_range.block_range().start.saturating_sub(1);
                            let chain_state = match client.get_tree_state(BlockId { height: previous_height.into(), ..Default::default() }).await {
                                Ok(tree_state) => {
                                    tree_state.into_inner().to_chain_state().unwrap()
                                }
                                Err(err) => {
                                    println!("Failed to get tree state: {:?}", err);
                                    continue;
                                }
                            };

                            match scan_cached_blocks(
                                &network,
                                &block_cache,
                                wallet,
                                scan_range.block_range().start,
                                &chain_state,
                                scan_range.len(),
                            ) {
                                Ok(_) => {
                                    break;
                                }
                                Err(ChainError::Scan(err)) => {
                                    let rewind_height = err.at_height().saturating_sub(1);
                                    if let Err(err) = wallet.truncate_to_height(rewind_height) {
                                        assert!(false,"Failed to truncate wallet db: {:?}", err);
                                    }

                                    let deletion_range = ScanRange::from_parts(
                                        (rewind_height..BlockHeight::from_u32(network_tip_height as u32)).into(),
                                        ScanPriority::Scanned,
                                    );
                                    if let Err(err) = block_cache.delete(deletion_range).await {
                                        assert!(false,"Failed to truncate block db: {:?}", err);
                                    }

                                    local_tip_height = rewind_height.into();
                                    break 'process_blocks true;
                                }
                                Err(err) => {
                                assert!(false,"Failed to truncate wallet db: {:?}", err);
                                }
                            }
                        }

                        _ => {}
                    }

                    scan_ranges = wallet.suggest_scan_ranges().expect("failed to get new scan ranges");
                }
            }

            break 'process_blocks (false);
        };

        if reorg_required {
            continue;
        }

        'process_wallet_txs: {
            let Ok(chain_state) = tree_state.to_chain_state() else {
                break 'process_wallet_txs;
            };

            for (wallet, t_address) in [
                (&mut miner_wallet, miner_t_address),
                (&mut user_wallet, user_t_address),
            ] {
                let scan_ranges = match wallet.suggest_scan_ranges() {
                    Err(err) => {
                        assert!(false,"Failed to get scan ranges: {:?}", err);
                        break 'process_wallet_txs;
                    }
                    Ok(scan_ranges) => scan_ranges,
                };

                for range in &scan_ranges {
                    let scan_result = scan_cached_blocks(
                        &network,
                        &block_cache,
                        wallet,
                        range.block_range().start,
                        &chain_state,
                        range.len(),
                    );
                }

                let lowest_start = &scan_ranges
                    .iter()
                    .min_by(|a, b| a.block_range().start.cmp(&b.block_range().start));
                let highest_end = &scan_ranges
                    .iter()
                    .max_by(|a, b| a.block_range().end.cmp(&b.block_range().end));
                match (lowest_start, highest_end) {
                    (Some(lo), Some(hi)) => {
                        let range = BlockRange {
                            start: Some(BlockId {
                                height: lo.block_range().start.into(),
                                ..Default::default()
                            }),
                            end: Some(BlockId {
                                height: hi.block_range().end.into(),
                                ..Default::default()
                            }),
                        };

                        println!("TADDRESS SEARCH RANGE: {:?}", range);

                        match client
                            .get_taddress_transactions(TransparentAddressBlockFilter {
                                address: t_address.encode(network),
                                range: Some(range.clone()),
                            })
                            .await
                        {
                            Ok(tx_stream) => {
                                let mut tx_stream = tx_stream.into_inner();
                                loop {
                                    match tx_stream.message().await {
                                        Ok(Some(raw_tx)) => {
                                            let tx_height = BlockHeight::from_u32(
                                                raw_tx.height.try_into().unwrap(),
                                            );
                                            let branch_id =
                                                BranchId::for_height(network, tx_height);
                                            let tx = Transaction::read(
                                                raw_tx.data.as_slice(),
                                                branch_id,
                                            )
                                            .expect("failed to read transaction");

                                            if let Err(err) =
                                                wallet::decrypt_and_store_transaction(
                                                    network, wallet, &tx, None,
                                                )
                                            {
                                                println!("Error decrypting and storing transaction: {}", err);
                                            }

                                            // let Some(bundle) = tx.transparent_bundle() else {
                                            //     continue;
                                            // };

                                            // if let Err(err) = wallet::decrypt_and_store_transaction(network, wallet, &tx, Some(tx_height)) {
                                            //     println!("transparent tx decrypt error: {err}");
                                            // }

                                            // Process outputs for received UTXOs
                                            // for (index, txout) in bundle.vout.iter().enumerate() {
                                            //     let Some(recipient) = txout.recipient_address() else {
                                            //         println!("Couldn't get transparent address from pubkey {:?}", txout.script_pubkey);
                                            //         continue;
                                            //     };
                                            //     if recipient.encode(network) == t_addr_str {
                                            //         let outpoint = OutPoint::new(tx.txid().into(), index as u32);
                                            //         let height = BlockHeight::from_u32(raw_tx.height as u32);
                                            //         let Some(wto) = zcash_client_backend::wallet::WalletTransparentOutput::from_parts(
                                            //             outpoint,
                                            //             txout.clone(),
                                            //             Some(height),
                                            //         ) else { continue };
                                            //         if let Err(err) = wallet.put_received_transparent_utxo(&wto) {
                                            //             println!("put_received_transparent_utxo error: {err}");
                                            //             continue;
                                            //         };
                                            //         // println!("Received transparent UTXO at height {}", height);
                                            //     }
                                            // }
                                        }
                                        Ok(None) => {
                                            break;
                                        }
                                        Err(err) => {
                                            assert!(false,"Failed to fetch transactions for address {} in range {:?}: {}", t_address.encode(network), &range, err);
                                            break;
                                        }
                                    }
                                }
                            }
                            Err(err) => {
                                assert!(false,"Failed to fetch transactions for address {} in range {:?}: {}", t_address.encode(network), &range, err);
                            }
                        }
                    }

                    _ => {}
                }
            }

            let mut wallet_balance = 0;

            let target_height =
                wallet::TargetHeight::from(BlockHeight::from_u32(local_tip_height as u32));
            if let Ok(ids) = miner_wallet.get_account_ids() {
                for id in ids {
                    let Ok(balances) = miner_wallet.get_transparent_balances(
                        id.into(),
                        target_height,
                        ConfirmationsPolicy::MIN,
                    ) else {
                        continue;
                    };
                    for b in balances {
                        wallet_balance += b.1 .1.total().into_u64();
                    }
                }
            }

            let zec_full = wallet_balance / 100_000_000;
            let zec_part = wallet_balance % 100_000_000;
            println!(
                "MINER HAS {} ({}.{})) cTAZ",
                wallet_balance, zec_full, zec_part
            );

            wallet_state.lock().unwrap().balance = wallet_balance as i64;
        };

        'process_wallet_actions: {
            /*
            if let Ok(wallet_state) = &mut wallet_state.try_lock() {
                while let Some(action) = wallet_state.actions_in_flight.front() {
                    match action {
                        WalletAction::RequestFromFaucet => {
                            /*
                            println!("***** WALLET GUI REQUESTED FUNDS FROM THE FAUCET (txs_seen_block_height: {})", txs_seen_block_height);
                            let action = wallet_state.actions_in_flight.pop_front().unwrap();
                            wallet_state.waiting_for_faucet = true;

                            let zats = (Zatoshis::from_nonnegative_i64(miner_utxos[0].value_zat).unwrap() - MINIMUM_FEE).unwrap();

                            let mut signing_set = TransparentSigningSet::new();
                            signing_set.add_key(miner_privkey);

                            let prover = LocalTxProver::bundled();
                            let extsk: &[ExtendedSpendingKey] = &[];
                            let sak: &[SpendAuthorizingKey] = &[];

                            let script = zcash_transparent::address::Script(zcash_script::script::Code(miner_utxos[0].script.clone()));

                            let outpoint = OutPoint::new(miner_utxos[0].txid[..32].try_into().unwrap(), miner_utxos[0].index as u32);

                            let mut txb = TxBuilder::new(
                                network,
                                BlockHeight::from_u32(latest_block.height as u32),
                                BuildConfig::Standard {
                                    sapling_anchor: None,
                                    orchard_anchor: None,
                                },
                            );

                            txb.add_transparent_input(miner_pubkey, outpoint, TxOut::new((zats + MINIMUM_FEE).unwrap(), script)).unwrap();
                            txb.add_transparent_output(&user_t_addr, zats).unwrap();

                            use rand_chacha::ChaCha20Rng;
                            let rng = ChaCha20Rng::from_rng(OsRng).unwrap();
                            let tx_res = txb.build(
                                &signing_set,
                                extsk,
                                sak,
                                rng,
                                &prover,
                                &prover,
                                &zip317::FeeRule::standard(),
                            ).unwrap();

                            let tx = tx_res.transaction();
                            let mut tx_bytes = vec![];
                            tx.write(&mut tx_bytes).unwrap();

                            let res = client.send_transaction(RawTransaction{ data: tx_bytes, height: 0 }).await;
                            println!("******* res: {:?}", res);
                            */
                        },

                        _ => {
                            wallet_state.actions_in_flight.pop_front();
                        }
                    }
                }
            }
            */
        };
    }
    */

    // ORIGINAL CODE FOR ANDREW'S COPYING/PASTING PLEASURE
    /*
    the_future_is_now(async {
        let latest_block = client.get_latest_block(ChainSpec{}).await.unwrap().into_inner();
        let miner_utxos = match client.get_address_utxos(GetAddressUtxosArg {
            addresses: vec![miner_t_addr_str.to_owned()],
            start_height: 0,
            max_entries: 0
        }).await {
            Err(err) => {
                println!("******* GET UTXOS ERROR: {:?}", err);
                vec![]
            },
            Ok(res) => res.into_inner().address_utxos,
        };

        let user_utxos = match client.get_address_utxos(GetAddressUtxosArg {
            addresses: vec![user_t_addr_str.to_owned()],
            start_height: 0,
            max_entries: 0
        }).await {
            Err(err) => {
                println!("******* GET UTXOS ERROR: {:?}", err);
                vec![]
            },
            Ok(res) => res.into_inner().address_utxos,
        };

        // match client.get_lightd_info(Empty{}).await {
        //     Err(err) => {
        //         println!("******* GET UTXOS ERROR: {:?}", err);
        //     }
        //     Ok(info) => {
        //         tip_h = info.into_inner().block_height.try_into().unwrap();
        //     }
        // }
        // let tip_h = match block_cache.get_tip_height(None) {
        //     Ok(Some(tip_h)) => tip_h,
        //     Ok(None) => 0.into(),
        //     Err(err) => {
        //         println!("******* CACHE TIP ERROR: {:?}", err);
        //         return;
        //     },
        // };

        fn block_range_from_scan_range(scan_range: &ScanRange) -> BlockRange {
            let r = scan_range.block_range();
            BlockRange{
                start: Some(BlockId{ height: <u64>::from(r.start), hash: Vec::new() }),
                end: Some(BlockId{ height: <u64>::from(r.end /*-1*/) as u64, hash: Vec::new() }),
            }
        }

        let t_addr_wallets = vec![
            (user_t_addr.clone(), &mut user_wallet, user_usk.clone(), user_account.id()),
            (miner_t_addr.clone(), &mut miner_wallet, miner_usk.clone(), user_account.id()),
        ];
        for (t_addr, wallet, usk, account_id) in t_addr_wallets {
            let t_addr_str = t_addr.encode(network);
            let tip_height = BlockHeight::from_u32(tip_h.try_into().unwrap());
            if let Err(err) = wallet.update_chain_tip(tip_height) {
                println!("update chain tip error: {err}");
            }

            let mut got_transparent = false;
            loop {
                // NOTE: may have changed between loops
                let scan_ranges = match wallet.suggest_scan_ranges() {
                    Ok(ranges) => ranges,
                    Err(err) => {
                        println!("******* SCAN RANGE ERROR: {:?}", err);
                        break;
                    }
                };
                let Some(scan_range) = scan_ranges.first() else {
                    break;
                };
                // if scan_range.priority() != ScanPriority::Verify {
                //     break;
                // }

                let prev_state_h = scan_range.block_range().start.saturating_sub(1).into();
                let chain_state = if prev_state_h == 0 {
                    ChainState::empty(BlockHeight::from_u32(0), zcash_primitives::block::BlockHash([0; 32]))
                    // ChainState::empty(
                    //     BlockHeight::from_u32(0),
                    //     genesys_block_hash.expect("we should've set the genesis block hash before this"),
                    // )
                } else {
                    // TODO: it feels like we should be able to compute this locally
                    match client.get_tree_state(BlockId{height:prev_state_h, hash:Vec::new()}).await {
                        Err(err) => {
                            println!("******* GET TREE STATE ERROR: {:?}", err);
                            continue;
                        },
                        Ok(result) => {
                            let tree_state = result.into_inner();
                            match tree_state.to_chain_state() {
                                Err(err) => {
                                    println!("******* TREE STATE TO CHAIN STATE ERROR: {:?}", err);
                                    continue;
                                }
                                Ok(chain_state) => chain_state
                            }
                        }
                    }
                };

                let range = block_range_from_scan_range(&scan_range);
                let mut new_blocks: Vec<CompactBlock> = Vec::new();
                match client.get_block_range(range.clone()).await {
                    Err(err) => println!("******* GET BLOCK RANGE ERROR: {:?}", err),
                    Ok(res) => {
                    }
                };

                if let Err(err) = block_cache.insert(new_blocks).await {
                    println!("block cache insert error: {err}");
                };

                let block_range = scan_range.block_range();
                let scan_res = scan_cached_blocks(network, &block_cache, wallet, block_range.start, &chain_state, scan_range.len());
                println!("scan: {scan_res:?}");
                match scan_res {
                    Ok(_) => {
                        // At this point, the cache and scanned data are locally consistent (though
                        // not necessarily consistent with the latest chain tip - this would be
                        // discovered the next time this codepath is executed after new blocks are
                        // received) so we can break out of the loop.
                    }

                    Err(ChainError::Scan(err)) if err.is_continuity_error() => {
                        // must be at least one block before the height at which the error occurred
                        let rewind_height = err.at_height().saturating_sub(10);
                        if let Err(err) = wallet.truncate_to_height(rewind_height) {
                            println!("******* TRUNCATE ERROR: {:?}", err);
                            break;
                        }
                        block_cache.delete(ScanRange::from_parts(
                                rewind_height .. (tip_h+1).into(),
                                ScanPriority::Scanned,
                        ));
                        continue;
                    }

                    Err(err) => {
                        println!("******* SCAN ERROR: {:?}", err);
                        break;
                    }
                };


                // Update wallet with transparent transactions for same range
                // TODO: off-by-1?
                let range = BlockRange{
                    start: Some(BlockId{ height: block_range.start.into(), hash: Vec::new() }),
                    end: Some(BlockId{ height: block_range.end.into(), hash: Vec::new() }),
                };

                let filter = TransparentAddressBlockFilter{ address: t_addr_str.to_owned(), range: Some(range.clone()) };
                let mut txs = Vec::new();
                match client.get_taddress_txids(filter).await {
                    Err(err) => println!("******* GET T-TRANSACTIONS ERROR: {:?}", err),
                    Ok(tx_stream) => {
                        let mut tx_stream = tx_stream.into_inner();
                        loop {
                            match tx_stream.message().await {
                                Ok(Some(tx)) => txs.push(tx),
                                Ok(None) => break,
                                Err(err) => {
                                    if err.code() == tonic::Code::OutOfRange {
                                        break;
                                    } else {
                                        println!("Get txs message error: {err:?}");
                                        // txs.truncate(0);
                                        break;
                                    }
                                }
                            }
                        }
                    }
                };

                if txs.len() > 0 {
                    // println!("txs for {t_addr}:");
                    got_transparent = true;
                }
                for raw_tx in &txs {
                    let tx_height = BlockHeight::from_u32(raw_tx.height.try_into().unwrap());
                    let branch_id = BranchId::for_height(network, tx_height);
                    let tx = Transaction::read(raw_tx.data.as_slice(), branch_id);
                    // println!("  {tx:?}");
                    let tx = match tx {
                        Ok(tx) => tx,
                        Err(err) => {
                            println!("transaction read error: {err}");
                            continue;
                        }
                    };

                    let Some(bundle) = tx.transparent_bundle() else {
                        continue;
                    };

                    // if let Err(err) = wallet::decrypt_and_store_transaction(network, wallet, &tx, Some(tx_height)) {
                    //     println!("transparent tx decrypt error: {err}");
                    // }

                    // Process outputs for received UTXOs
                    for (index, txout) in bundle.vout.iter().enumerate() {
                        let Some(recipient) = txout.recipient_address() else {
                            println!("Couldn't get transparent address from pubkey {:?}", txout.script_pubkey);
                            continue;
                        };
                        if recipient.encode(network) == t_addr_str {
                            let outpoint = OutPoint::new(tx.txid().into(), index as u32);
                            let height = BlockHeight::from_u32(raw_tx.height as u32);
                            let Some(wto) = zcash_client_backend::wallet::WalletTransparentOutput::from_parts(
                                outpoint,
                                txout.clone(),
                                Some(height),
                            ) else { continue };
                            if let Err(err) = wallet.put_received_transparent_utxo(&wto) {
                                println!("put_received_transparent_utxo error: {err}");
                                continue;
                            };
                            // println!("Received transparent UTXO at height {}", height);
                        }
                    }

                    // TODO: do we need to explicitly handle vin here or does that get covered
                    // by sending?
                }
            }

            println!("{} {:#?}", t_addr_str, wallet.get_wallet_summary(wallet::ConfirmationsPolicy::MIN));

            // immediately shield newly-received transparent transactions
            if got_transparent {
                let min_zats_for_shielding = Zatoshis::const_from_u64(10_000);
                match wallet::propose_shielding::<_, _, _, _, Infallible>(
                    wallet,
                    network,
                    &wallet::input_selection::GreedyInputSelector::new(),
                    &change_strategy,
                    min_zats_for_shielding,
                    &[t_addr],
                    account_id,
                    wallet::ConfirmationsPolicy::MIN,
                ) {
                    Err(err) => println!("propose_shielding error: {err:?}"),
                    Ok(proposal) => {
                        let prover = LocalTxProver::bundled();
                        match wallet::create_proposed_transactions::<_, _, Infallible, _, Infallible, _>(
                            wallet, network,
                            &prover,
                            &prover,
                            &wallet::SpendingKeys::from_unified_spending_key(usk.clone()),
                            zcash_client_backend::wallet::OvkPolicy::Sender,
                            &proposal)
                        {
                            Err(err) => println!("shielding create_proposed_transactions error: {err:?}"),
                            Ok(txids) => for txid in txids {
                                println!("created shielding transaction {txid:?}");

                                let tx = match wallet.get_transaction(txid) {
                                    Err(err) => {
                                        println!("failed to get tx {txid:?} immediately after making it: {err:?}");
                                        continue;
                                    }
                                    Ok(Some(tx)) => tx,
                                    Ok(None) => {
                                        println!("failed to get tx {txid:?} immediately after making it: (None)");
                                        continue;
                                    }
                                };

                                let mut data = Vec::new();
                                if let Err(err) = tx.write(&mut data) {
                                    println!("Serialization error for tx {:?}: {:?}", txid, err);
                                    continue;
                                }

                                let raw_tx = RawTransaction { data, height: 0 };

                                match client.send_transaction(raw_tx).await {
                                    Ok(res) => println!("sent transaction: {res:?}"),
                                    Err(err) => println!("failed to send transaction: {err:?}"),
                                }
                            }
                        }
                    }
                }
            }
        }

        txs_seen_block_height = tip_h as i32;

        {
            let history = get_transaction_history(&miner_wallet).unwrap();
            println!("********************* miner history: {:?}", history);
        }

        let history = get_transaction_history(&user_wallet).unwrap();
        wallet_state.lock().unwrap().txs = history.iter().map(|h| WalletTx(h.clone())).collect(); // @temp: shouldn't be a separate type

        if history.len() != total_user_txs {
            total_user_txs = history.len();
            wallet_state.lock().unwrap().waiting_for_faucet = false;
        }

        if !already_sent && txs_seen_block_height >= 5 {
        // if !already_sent && miner_utxos.len() != 0 && miner_utxos[0].height + (MIN_TRANSPARENT_COINBASE_MATURITY as u64) < latest_block.height {
            let zats = (Zatoshis::from_nonnegative_i64(miner_utxos[0].value_zat).unwrap() - MINIMUM_FEE).unwrap();
            // NOTE: we can't send transparent->transparent through the high-level API, we
            // have to propose_shielding first, then send in a later block
            match wallet::propose_standard_transfer_to_address::<_, _, Infallible>(
                &mut miner_wallet,
                network,
                zcash_client_backend::fees::StandardFeeRule::Zip317,
                miner_account.id(),
                wallet::ConfirmationsPolicy::MIN,
                &zcash_client_backend::address::Address::Transparent(user_t_addr),
                zats,
                None,
                None,
                FALLBACK_CHANGE_POOL)
            {
                Err(err) => println!("propose_transfer error: {err:?}"),
                Ok(proposal) => {
                    let prover = LocalTxProver::bundled();
                    match wallet::create_proposed_transactions::<_, _, Infallible, _, Infallible, _>(
                        &mut miner_wallet, network,
                        &prover,
                        &prover,
                        &wallet::SpendingKeys::from_unified_spending_key(miner_usk.clone()),
                        zcash_client_backend::wallet::OvkPolicy::Sender,
                        &proposal)
                    {
                        Err(err) => println!("create_proposed_transactions error: {err:?}"),
                        Ok(txids) => for txid in txids {
                            let tx = match miner_wallet.get_transaction(txid) {
                                Err(err) => {
                                    println!("failed to get tx {txid:?} immediately after making it: {err:?}");
                                    return;
                                }
                                Ok(Some(tx)) => tx,
                                Ok(None) => {
                                    println!("failed to get tx {txid:?} immediately after making it: (None)");
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

                            println!("created transaction {txid:?}");
                            already_sent = true;
                        },
                    }
                }
            }
        }

        // let latest = client.get_latest_block(ChainSpec{}).await.unwrap().into_inner();
        // let consensus_branch_id = BranchId::for_height(network, BlockHeight::from_u32(latest.height as u32));

        // let mut tbundle = TransparentBuilder::empty().build();
        // // tbundle.add_output(&user_t_addr, Zatoshis::const_from_u64(500)).unwrap();
        // // let tbundle = tbundle.build().unwrap();

        // let unauthed_tx: TransactionData::<zcash_protocol::transaction::Unauthorized> = TransactionData::from_parts(
        //     TxVersion::VCrosslink,
        //     consensus_branch_id,
        //     0,
        //     BlockHeight::from_u32(0),
        //     tbundle,
        //     None, None, None, None);

        // let txid_parts = unauthed_tx.digest(TxIdDigester);

        // let transparent_bundle = unauthed_tx
        //     .transparent_bundle()
        //     .map(|tb| tb.clone().apply_signatures(|thing| {
        //         let sig_hash = signature_hash(&unauthed_tx, &SignableInput::Shielded, &txid_parts);
        //         let sig_hash: [u8; 32] = sig_hash.as_ref().clone();
        //         sig_hash
        //     }, &TransparentSigningSet::default()).unwrap());

        // let mut tx_bytes = vec![];
        // tx_bytes.write(&mut tx_bytes).unwrap();

        let mut user_sum = 0;
        for utxo in &user_utxos {
            user_sum += utxo.value_zat;
        }

        wallet_state.lock().unwrap().balance = user_sum;

        let zec_full = user_sum / 100_000_000;
        let zec_part = user_sum % 100_000_000;
        println!("user {} has {} UTXOs with {} zats = {}.{} cTAZ", user_t_addr_str, user_utxos.len(), user_sum, zec_full, zec_part);

        }

        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    });
    */

    /*
    let uivk = UnifiedIncomingViewingKey::decode(&MAIN_NETWORK, "uivk1u7ty6ntudngulxlxedkad44w7g6nydknyrdsaw0jkacy0z8k8qk37t4v39jpz2qe3y98q4vs0s05f4u2vfj5e9t6tk9w5r0a3p4smfendjhhm5au324yvd84vsqe664snjfzv9st8z4s8faza5ytzvte5s9zruwy8vf0ze0mhq7ldfl2js8u58k5l9rjlz89w987a9akhgvug3zaz55d5h0d6ndyt4udl2ncwnm30pl456frnkj").unwrap();

    let ua = uivk.default_address(UnifiedAddressRequest::SHIELDED).unwrap().0;
    println!("UA: {}", ua.encode(&MAIN_NETWORK));

    let https_uri = "https://na.zec.rocks:443";
    let cert = include_bytes!("../na.zec.rocks-leaf.der");

    let transactions = the_future_is_now(async move {
        CryptoProvider::install_default(ring::default_provider()).unwrap();

        let mut cfg = rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(DerVerifier{
                certificate: cert,
                algorithms: CryptoProvider::get_default()
                    .map(|provider| provider.signature_verification_algorithms)
                    .unwrap(),
            }))
            .with_no_client_auth();

        cfg.alpn_protocols.push(b"h2".to_vec());

        let mut client = CompactTxStreamerClient::new({
            let v = Endpoint::from_shared(https_uri).unwrap();
            let v = v.tls_config(cfg).unwrap();
            v.connect().await.unwrap()
        });
    */

    /*
    let transactions = the_future_is_now(async move {
        let mut client = CompactTxStreamerClient::new({
            let c = Channel::from_shared("127.0.0.1:8080").unwrap();
            c.connect().await.unwrap()
        });

        let block_stream = client.get_block_range(BlockRange{
            start: Some(BlockId{height: 3051998, hash: Vec::new()}),
            end:   Some(BlockId{height: 3052065, hash: Vec::new()}),
        }).await.unwrap();
        let mut block_grpc = block_stream.into_inner();

        let mut blocks = Vec::new();
        loop {
            if let Ok(msg) = block_grpc.message().await {
                if let Some(block) = msg {
                    blocks.push(block);
                    continue;
                }
            }

            break;
        }

        let sapling_ivk = if let Some(ivk) = uivk.sapling() { Some(ivk.prepare()) } else { None };
        let orchard_ivk = if let Some(ivk) = uivk.orchard() { Some(ivk.prepare()) } else { None };

        let mut txs = Vec::new();
        for b in &blocks {
            for tx in &b.vtx {
                let mut transaction_is_ours = false;

                if let Some(ivk) = &sapling_ivk {
                    for sapling_output in &tx.outputs {
                        let Ok(compact_output) = CompactOutputDescription::try_from(sapling_output) else { continue };
                        if let Some((note, _)) = try_sapling_compact_note_decryption(ivk, &compact_output, Zip212Enforcement::On) {
                            println!("Sapling Note: {:#?}", note);
                            transaction_is_ours = true;
                            break;
                        }
                    }
                }

                if let Some(ivk) = &orchard_ivk {
                    for action in &tx.actions {
                        let Ok(compact_action) = CompactAction::try_from(action) else { continue };
                        let domain = OrchardDomain::for_compact_action(&compact_action);
                        if let Some((note, _recipient)) = try_compact_note_decryption(&domain, ivk, &compact_action) {
                            println!("Orchard Note: {:#?}", note);
                            transaction_is_ours = true;
                            break;
                        }
                    }
                }

                if transaction_is_ours {
                    txs.push(tx.clone());
                }
            }
        }

        txs
    });
    */
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
