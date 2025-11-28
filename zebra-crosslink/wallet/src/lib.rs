//! Internal wallet
#![allow(warnings)]

use orchard::keys::SpendAuthorizingKey;
use orchard::note_encryption::{CompactAction};
use rand_chacha::rand_core::SeedableRng;
use rand_core::OsRng;
use sapling_crypto::zip32::ExtendedSpendingKey;
use tokio_rustls::rustls;
use tonic::client::GrpcService;
use tonic::transport::{Certificate, Channel, ClientTlsConfig, Endpoint};
use zcash_client_backend::fees::StandardFeeRule;
use zcash_client_backend::proto::service::RawTransaction;
use zcash_note_encryption::try_compact_note_decryption;
use zcash_primitives::transaction::components::TxOut;
use zcash_primitives::transaction::fees::zip317::{self, MINIMUM_FEE};
use zcash_primitives::transaction::sighash::{SignableInput, signature_hash};
use zcash_primitives::transaction::txid::TxIdDigester;
use zcash_primitives::transaction::{Transaction, TransactionData, TxVersion};
use zcash_primitives::transaction::builder::{BuildConfig, Builder as TxBuilder};
use zcash_proofs::prover::LocalTxProver;
use zcash_protocol::consensus::{BlockHeight, BranchId};
use zcash_protocol::value::Zatoshis;
use zcash_transparent::{
    keys::{ NonHardenedChildIndex },
    builder::{TransparentBuilder, TransparentSigningSet, Unauthorized},
    bundle::OutPoint,
};
use zebra_chain::block::{Hash as BlockHash, Height};
use zebra_chain::parameters::NetworkUpgrade;
use zebra_chain::serialization::{ZcashDeserialize, ZcashSerialize};
use zebra_chain::transaction::{LockTime};
use zebra_chain::transparent::{self, Input, MIN_TRANSPARENT_COINBASE_MATURITY, Utxo};
use std::future::Future;
use std::sync::{Arc, Mutex};

use rustls::CertificateError;
use rustls::client::danger::ServerCertVerified;
use rustls::client::danger::ServerCertVerifier;
use rustls::crypto::WebPkiSupportedAlgorithms;
use rustls::crypto::ring;
use rustls::crypto::CryptoProvider;
use rustls::crypto::verify_tls12_signature;
use rustls::crypto::verify_tls13_signature;
use tokio::runtime::Builder;
use zcash_client_backend::{
    address::{
        UnifiedAddress,
    },
    data_api::{
        self,
        chain::{
            BlockCache,
            ChainState,
        },
        Account as APIAccount,
        AccountBirthday,
        AccountPurpose,
        WalletRead,
        WalletWrite,
        Zip32Derivation,
    },
    encoding::AddressCodec,
    keys::{
        UnifiedAddressRequest,
        UnifiedFullViewingKey,
        UnifiedIncomingViewingKey,
        UnifiedSpendingKey,
    },
    proto::{
        compact_formats::CompactTx,
        service::{
            BlockId,
            BlockRange,
            ChainSpec,
            Duration,
            GetAddressUtxosArg,
            Empty,
            LightdInfo,
            TransparentAddressBlockFilter,
            compact_tx_streamer_client::CompactTxStreamerClient
        }
    },
};
use zcash_client_memory::{
    types::account::{Account as MemAccount},
    MemBlockCache,
    MemoryWalletDb,
};
use zcash_protocol::{
    consensus::{
        MAIN_NETWORK,
        TEST_NETWORK,
        NetworkType,
        Parameters,
    },
};
use zcash_transparent::{
    address::TransparentAddress,
    keys::{
        IncomingViewingKey,
        TransparentKeyScope,
    },
};

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

pub struct WalletTx {

}

pub struct WalletState {
    pub balance: i64, // in zats
    pub txs: Vec<WalletTx>,
}


impl WalletState {
    pub fn new() -> Self {
        WalletState {
            balance: 0,
            txs: Vec::new(),
        }
    }
}

pub fn wallet_main(wallet_state: Arc<Mutex<WalletState>>) {
    the_future_is_now(async {
        println!("waiting for zaino to be ready...");
        wait_for_zainod().await;
    });

    let cdb = zcash_client_sqlite::BlockDb::for_path(":memory:").unwrap();
    zcash_client_sqlite::chain::init::init_cache_database(&cdb).unwrap();

    let network = &TEST_NETWORK;

    fn wallet_from_seed_phrase<P: Parameters>(params: P, phrase: &str) -> (MemoryWalletDb<P>, MemAccount, UnifiedSpendingKey) {
        use secrecy::ExposeSecret;

        let mnemonic = bip39::Mnemonic::parse(phrase).unwrap();
        let bip39_passphrase = ""; // optional
        let seed64 = mnemonic.to_seed(bip39_passphrase);
        let seed = secrecy::SecretVec::new(seed64[..32].to_vec());
        let seed_fp = zip32::fingerprint::SeedFingerprint::from_seed(seed.expose_secret()).unwrap();
        let account_id = zip32::AccountId::try_from(0).unwrap();

        let usk = UnifiedSpendingKey::from_seed(&params, seed.expose_secret(), account_id).unwrap();
        let ufvk = usk.to_unified_full_viewing_key();
        let birthday = &AccountBirthday::from_parts(
            ChainState::empty(BlockHeight::from_u32(0), zcash_primitives::block::BlockHash([0; 32])),
            None,
        );
        let derivation = Some(Zip32Derivation::new(seed_fp, account_id));
        let purpose = AccountPurpose::Spending{ derivation };

        let mut wallet = MemoryWalletDb::new(params, 0); // TODO: max_checkpoints=?
        let account = wallet.import_account_ufvk("main_account", &ufvk, birthday, purpose, None).unwrap();
        (wallet, account, usk)
    }

    fn transparent_keys_from_usk(usk: &UnifiedSpendingKey) -> Option<(secp256k1::PublicKey, secp256k1::SecretKey)> {
        let transparent = usk.transparent();
        let account_pubkey = transparent.to_account_pubkey();
        let child_index = NonHardenedChildIndex::const_from_index(0);
        let address_pubkey = account_pubkey.derive_address_pubkey(TransparentKeyScope::EXTERNAL, child_index).ok()?;
        let address_privkey = transparent.derive_external_secret_key(child_index).ok()?;
        Some((address_pubkey, address_privkey))
    }

    fn transparent_addr_from_account(account: &MemAccount) -> Option<TransparentAddress> {
        let account_pubkey = account.ufvk()?.transparent()?;
        let child_index = NonHardenedChildIndex::const_from_index(0);
        let address_pubkey = account_pubkey.derive_address_pubkey(TransparentKeyScope::EXTERNAL, child_index).ok()?;
        Some(TransparentAddress::from_pubkey(&address_pubkey))
    }

    let (mut miner_wallet, miner_account, miner_usk) = wallet_from_seed_phrase(network,
        "abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon about"
    );
    let miner_t_addr = transparent_addr_from_account(&miner_account).unwrap();
    let miner_t_addr_str = miner_t_addr.encode(network);
    println!("Faucet miner t-address: {}", miner_t_addr_str);
    let (miner_pubkey, miner_privkey) = transparent_keys_from_usk(&miner_usk).unwrap();

    let (mut user_wallet, user_account, user_usk) = wallet_from_seed_phrase(network,
        "blur kit item praise brick misery muffin symptom cheese street tired evolve"
    );

    let user_t_addr = transparent_addr_from_account(&user_account).unwrap();
    let user_t_addr_str = user_t_addr.encode(network);

    let user_t_recs = user_wallet.get_transparent_receivers(user_account.id(), false, false).unwrap();
    let user_t_addr1 = user_t_recs.keys().collect::<Vec<_>>()[0];
    // NOTE: the default isn't the same as below, but I think this is because it forces a diversifier index
    println!("User wallet: {}/{:?}", user_t_addr_str, user_t_addr1.encode(network));


//     let lax_confirmations = ConfirmationsPolicy {
//         trusted: 1,
//         untrusted: 1,
//         allow_zero_conf_shielding: true,
    // };

    let mut block_cache = MemBlockCache::new();

    let mut txs_seen_block_height = 0;
    let mut already_sent = false;
    loop {
        the_future_is_now(async {
            let mut client = CompactTxStreamerClient::new({
                loop {
                    if let Ok(channel) = Channel::from_static("http://localhost:18233").connect().await {
                        break channel;
                    }
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                }
            });
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

            let block_range = BlockRange{
                start: Some(BlockId{ height: txs_seen_block_height, hash: Vec::new() }),
                end: Some(BlockId{ height: u32::MAX as u64, hash: Vec::new() }),
            };
            let new_blocks = match client.get_block_range(block_range.clone()).await {
                Err(err) => {
                    println!("******* GET BLOCK RANGE ERROR: {:?}", err);
                    None
                },
                Ok(res) => {
                    let mut grpc_stream = res.into_inner();
                    let mut blocks = Vec::new();
                    loop {
                         match grpc_stream.message().await {
                             Ok(Some(block)) => blocks.push(block),
                             Ok(None) => break Some(blocks),
                             Err(err) => {
                                 if err.code() == tonic::Code::OutOfRange {
                                     break Some(blocks);
                                 } else {
                                     println!("Get block range message error: {err:?}");
                                     break None;
                                 }
                             }
                        }
                    }
                }
            };

            if let Some(new_blocks) = new_blocks {
                if new_blocks.len() > 0 {
                    // get transparent transactions for same range
                    let range = BlockRange{
                        start: block_range.start,
                        end: Some(BlockId{
                            height: new_blocks.last().unwrap().height,
                            hash: Vec::new(),
                        })
                    };

                    block_cache.insert(new_blocks);

                    let t_addrs = vec![user_t_addr_str.clone()];
                    for t_addr in &t_addrs {
                        let filter = TransparentAddressBlockFilter{ address: t_addr.to_owned(), range: Some(range.clone()) };
                        let txs = match client.get_taddress_txids(filter).await {
                            Err(err) => {
                                println!("******* GET T-TRANSACTIONS ERROR: {:?}", err);
                                None
                            }
                            Ok(tx_stream) => {
                                let mut tx_stream = tx_stream.into_inner();
                                let mut txs = Vec::new();
                                loop {
                                    match tx_stream.message().await {
                                        Ok(Some(tx)) => txs.push(tx),
                                        Ok(None) => break Some(txs),
                                        Err(err) => {
                                            if err.code() == tonic::Code::OutOfRange {
                                                break Some(txs);
                                            } else {
                                                println!("Get txs message error: {err:?}");
                                                break None;
                                            }
                                        }
                                    }
                                }
                            }
                        };

                        if let Some(txs) = txs {
                            println!("txs for {t_addr}:");
                            for raw_tx in txs {
                                let zebra_tx = zebra_chain::transaction::Transaction::zcash_deserialize(raw_tx.data.as_slice());
                                let branch_id = BranchId::for_height(network, BlockHeight::from_u32(raw_tx.height.try_into().unwrap()));
                                let tx = Transaction::read(raw_tx.data.as_slice(), branch_id);
                                println!("  zebra: {zebra_tx:?}");
                                println!("  librz: {tx:?}");
                            }
                        }
                    }
                }
            }

            if !already_sent && miner_utxos.len() != 0 && miner_utxos[0].height + (MIN_TRANSPARENT_COINBASE_MATURITY as u64) < latest_block.height {
                let mut signing_set = TransparentSigningSet::new();
                signing_set.add_key(miner_privkey);

                let prover = LocalTxProver::bundled();
                let extsk: &[ExtendedSpendingKey] = &[];
                let sak: &[SpendAuthorizingKey] = &[];

                let zats = (Zatoshis::from_nonnegative_i64(miner_utxos[0].value_zat).unwrap() - MINIMUM_FEE).unwrap();
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

                already_sent = true;
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

            let zec_full = user_sum / 100_000_000;
            let zec_part = user_sum % 100_000_000;
            println!("user {} has {} UTXOs with {} zats = {}.{} cTAZ", user_t_addr_str, user_utxos.len(), user_sum, zec_full, zec_part);

            wallet_state.lock().unwrap().balance = user_sum;
            tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
        });
    }

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
