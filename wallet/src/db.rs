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
    // Crosslink change goes to the Ironwood pool, not Orchard. Ironwood shares Orchard's
    // receiver, keys and circuit, so this is purely a choice of *value pool* (and hence of
    // note commitment tree and nullifier set) -- the address and key machinery below stays
    // Orchard, as upstream intends (see zcash_keys::address: "Ironwood shares the Orchard
    // receiver").
    const FALLBACK_CHANGE_POOL: zcash_protocol::ShieldedPool = zcash_protocol::ShieldedPool::Ironwood;
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


    let send_zats = async |client: &mut CompactTxStreamerClient<_>, dst_ua: &UnifiedAddress, src_wallet: &mut WalletDb<_, _, _, _>, src_usk: &UnifiedSpendingKey, zats: Zatoshis, params, opts: &TxOptions| -> Option<[u8;32]> {
        let t = Timer::scope("send_zats");

        // @todo(judah): handle multiple accounts?
        let Ok(src_ids)  = src_wallet.get_account_ids() else { return None; };
        let Some(src_id) = src_ids.first() else { return None; };
        let Ok(Some(src_account)) = src_wallet.get_account(*src_id) else { return None; };

        // Crosslink change goes to the Ironwood pool. See the note in `propose_shielding` above.
        const FALLBACK_CHANGE_POOL: zcash_protocol::ShieldedPool = zcash_protocol::ShieldedPool::Ironwood;

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

    let send_zats_to_wallet = async |client: &mut CompactTxStreamerClient<_>, dst_wallet: &mut WalletDb<_, _, _, _>, src_wallet: &mut WalletDb<_, _, _, _>, src_usk: &UnifiedSpendingKey, zats: Zatoshis, params, opts: &TxOptions| -> Option<[u8;32]> {
        match addrs_from_wallet(dst_wallet) {
            Some((_, dst_ua)) => send_zats(client, &dst_ua, src_wallet, src_usk, zats, params, opts).await,
            None => None,
        }
    };

