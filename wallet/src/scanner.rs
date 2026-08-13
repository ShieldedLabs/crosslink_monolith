use crate::*;
use crate::bft::{ ScanInfo, ScanBond, PubKeyID };

#[derive(Clone, Debug)]
pub struct ScanCtx {
    pub ufvk: zcash_keys::keys::UnifiedFullViewingKey,
    pub t_addr: TransparentAddress,
    pub orchard_external_ovk: orchard::keys::OutgoingViewingKey,
    pub orchard_internal_ovk: orchard::keys::OutgoingViewingKey,
}


pub fn scan_tx(info: &mut ScanInfo, utxos: &mut HashSet<(PubKeyID, u32)>, tx_bytes: &[u8], tx_i: usize, height: u32, ctx: &ScanCtx, txid_zeb: [u8; 32]) -> Result<bool, String> {
    let tz = Timer::scope_("scan_tx", true);
    let mut new_info = false;
    info.max_height_seen = info.max_height_seen.max(height);

    let network = &TEST_NETWORK;
    let block_h = LRZBlockHeight::from_u32(height);
    let tx = match Transaction::read(tx_bytes, BranchId::for_height(network, block_h)) {
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
            let mut is_to_ufvk = false;
            for output in &t_bundle.vout {
                coinbase_ok = true;

                if let Some(matched_addr) = output.recipient_address() {
                    // is_to_ufvk |= t_addr_belongs_to_ufvk_index(&ctx.ufvk, 0, matched_addr);
                    is_to_ufvk |= matched_addr == ctx.t_addr;
                }
            }

            if is_to_ufvk {
                // println!("Found a match in a coinbase transaction at height {height}! Value: {value:?}");
                new_info = true;
                info.coinbases_c += 1;
                info.coinbases_value += 500_000_000; // hardcoded for @testnet ClT0
                debug_assert!(info.coinbase_max_height < height, "expected linear iteration");
                info.coinbase_max_height = height;
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
                if t_addr_belongs_to_ufvk_index(&ctx.ufvk, 0, t_addr) {
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

                    for ovk in [&ctx.orchard_external_ovk, &ctx.orchard_internal_ovk] {
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
                    create_txid: PubKeyID(<[u8;32]>::from(txid_lrz)),
                    create_height: height,
                });
            }
        }
    }

    Ok(new_info)
}
