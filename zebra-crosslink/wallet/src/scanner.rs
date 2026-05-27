use crate::*;
use crate::bft::PubKeyID;

#[derive(Clone, Debug)]
pub struct ScanBond {
    pub pk: PubKeyID,
    pub initial_val: u64,
    pub create_height: u32,
    pub create_txid: TxId
}

#[derive(Clone, Default, Debug)]
pub struct ScanInfo {
    pub coinbases_c: usize,
    pub coinbases_value: u64,
    pub coinbase_max_height: u32,

    pub bonds: Vec<ScanBond>,
    pub bonds_value: u64,

    pub max_height_seen: u32,
}
impl ScanInfo {
    pub fn total_value(&self) -> u64 {
        self.coinbases_value + self.bonds_value
    }
}


pub fn scan_tx(info: &mut ScanInfo, utxos: &mut HashSet<(PubKeyID, u32)>, tx_bytes: &[u8], tx_i: usize, height: u32, ufvk: &UnifiedFullViewingKey, txid_zeb: [u8; 32]) -> Result<bool, String> {
    let tz = Timer::scope_("scan_tx", true);
    let mut new_info = false;
    info.max_height_seen = info.max_height_seen.max(height);

    let Some((t_addr, p2sh, ua)) = addrs_from_ufvk(ufvk, 0) else{
        return Err("Could not get an address".to_owned());
    };

    let network = &TEST_NETWORK;
    let block_h = LRZBlockHeight::from_u32(height);
    let tx = match Transaction::read(tx_bytes, BranchId::for_height(network, block_h)){
        Ok(tx) => tx,
        Err(err) => return Err(format!("{err:?}")),
    };

    let txid_lrz = tx.txid();
    assert!(txid_zeb == <[u8;32]>::from(txid_lrz), "txids from zebra/librustzcash disagree: {} vs {}", txid_lrz, TxId::from_bytes(txid_zeb));

    // println!("scanning {txid_lrz} at height {height}");

    let mut contains_my_t_spend = false;
    let mut coinbase_ok = false;
    if let Some(t_bundle) = tx.transparent_bundle() {
        let tz = Timer::scope_("scan_tx > t_bundle", true);
        if t_bundle.is_coinbase() {
            for output in &t_bundle.vout {
                coinbase_ok = true;

                if let Some(matched_addr) = output.recipient_address(){
                    if matched_addr == t_addr {
                        // println!("Found a match in a coinbase transaction at height {height}! Value: {value:?}");

                        new_info = true;
                        info.coinbases_c += 1;
                        info.coinbases_value += 500_000_000; // hardcoded for @testnet ClT0
                        debug_assert!(info.coinbase_max_height < height, "expected linear iteration");
                        info.coinbase_max_height = height;
                    }
                }
            }
        }

        for input in &t_bundle.vin {
            if utxos.contains(&(PubKeyID(*input.prevout.txid().as_ref()), input.prevout.n())) {
                contains_my_t_spend = true;
            }
        }

        // track received UTXOs so we can later determine if we spent that UTXO on a staking action
        for (out_i, txout) in t_bundle.vout.iter().enumerate() {
            if let Some(t_addr) = txout.recipient_address() {
                if t_addr_belongs_to_ufvk_index(ufvk, 0, t_addr) {
                    let outpoint = (PubKeyID(*txid_lrz.as_ref()), out_i.try_into().unwrap());
                    if ! utxos.insert(outpoint) {
                        return Err(format!("multiple receipts of the same UTXO: {:?}", outpoint));
                    }
                }
            }
        }
    }

    if tx_i == 0 && !coinbase_ok {
        return Err("no coinbase found".to_owned());
    }


    let keys = PreparedKeys::from_ufvk_all(&ufvk);
    let internal_keys = PreparedKeys::from_ufvk_all_internal(&ufvk);
    let (Some(orchard_ovk), Some(orchard_internal_ovk)) = (keys.orchard_ovk, internal_keys.orchard_ovk) else {
        return Err("could not create orchard ovks".to_owned());
    };

    if let Some(staking_action) = tx.staking_action() {
        if staking_action.kind == StakingActionKind::CreateNewDelegationBond {
            let staking_action = StakingAction_CreateNewDelegationBond::try_from_union(&staking_action).unwrap();

            if contains_my_t_spend {
                println!("found staking action paid for by our transparent: {:?}", staking_action.unique_pubkey);
            }
            let mut is_my_staking_action = contains_my_t_spend;

            if is_my_staking_action {
            } else if let Some(bundle) = tx.orchard_bundle() {
                'actions: for action in bundle.actions() {
                    let action: &orchard::Action<_> = action; // type-check
                    let domain = orchard::note_encryption::OrchardDomain::for_action(action);

                    for ovk in [&orchard_ovk, &orchard_internal_ovk] {
                        if let Some((_note, _addr, _send_memo)) = try_output_recovery_with_ovk(
                            &domain,
                            ovk,
                            action,
                            action.cv_net(),
                            &action.encrypted_note().out_ciphertext
                        ) {
                            println!("found staking action paid for by our orchard: {:?}", staking_action.unique_pubkey);
                            is_my_staking_action = true;
                            break 'actions;
                        }
                    }
                }
            }


            if is_my_staking_action {
                new_info = true;
                info.bonds.push(ScanBond {
                    pk: PubKeyID(staking_action.unique_pubkey),
                    initial_val: staking_action.amount_zats,
                    create_txid: txid_lrz,
                    create_height: height,
                });
            }
        }
    }

    Ok(new_info)
}
