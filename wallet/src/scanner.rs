use crate::*;
use crate::bft::{ ScanInfo, ScanBond, PubKeyID };
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct ScanCtx {
    pub ufvk: zcash_keys::keys::UnifiedFullViewingKey,
    pub t_addr: TransparentAddress,
    pub t_addr_p2sh: TransparentAddress,
    pub orchard_external_ovk: orchard::keys::OutgoingViewingKey,
    pub orchard_internal_ovk: orchard::keys::OutgoingViewingKey,
}

// Profiling for total_issuance_from_key: nanoseconds per phase plus the unit counts that drive
// each phase. Atomics so scan_tx keeps its signature and the async block fetch can be timed inline.
pub struct ScanProf {
    pub fetch_ns: AtomicU64,
    pub serialize_ns: AtomicU64,
    pub txid_ns: AtomicU64,
    pub replay_ns: AtomicU64,
    pub parse_ns: AtomicU64,
    pub transparent_ns: AtomicU64,
    pub orchard_ns: AtomicU64,

    pub blocks: AtomicU64,
    pub txs: AtomicU64,
    pub staking: AtomicU64,
    pub vouts: AtomicU64,
    pub trial_decrypts: AtomicU64,
}

pub static PROF: ScanProf = ScanProf {
    fetch_ns: AtomicU64::new(0),
    serialize_ns: AtomicU64::new(0),
    txid_ns: AtomicU64::new(0),
    replay_ns: AtomicU64::new(0),
    parse_ns: AtomicU64::new(0),
    transparent_ns: AtomicU64::new(0),
    orchard_ns: AtomicU64::new(0),

    blocks: AtomicU64::new(0),
    txs: AtomicU64::new(0),
    staking: AtomicU64::new(0),
    vouts: AtomicU64::new(0),
    trial_decrypts: AtomicU64::new(0),
};

#[inline]
pub fn timed<T>(slot: &AtomicU64, f: impl FnOnce() -> T) -> T {
    let t = Instant::now();
    let r = f();
    slot.fetch_add(t.elapsed().as_nanos() as u64, Relaxed);
    r
}

impl ScanProf {
    pub fn reset(&self) {
        for a in [&self.fetch_ns, &self.serialize_ns, &self.txid_ns, &self.replay_ns, &self.parse_ns,
                  &self.transparent_ns, &self.orchard_ns, &self.blocks, &self.txs,
                  &self.staking, &self.vouts, &self.trial_decrypts] {
            a.store(0, Relaxed);
        }
    }

    pub fn report(&self, wall: Duration) {
        let g = |a: &AtomicU64| a.load(Relaxed);
        let txs = g(&self.txs).max(1);
        let wall_ns = wall.as_nanos() as u64;
        let mut sum = 0u64;
        for (name, slot) in [("fetch", &self.fetch_ns), ("serialize", &self.serialize_ns),
                             ("zebra_txid", &self.txid_ns),
                             ("replay", &self.replay_ns), ("parse", &self.parse_ns),
                             ("transparent", &self.transparent_ns), ("orchard", &self.orchard_ns)] {
            let ns = g(slot);
            sum += ns;
            println!("scanprof {name:>12}: {:10.1} ms  {:5.1}%  {:9.2} us/tx",
                     ns as f64 / 1e6, 100.0 * ns as f64 / wall_ns.max(1) as f64, ns as f64 / 1e3 / txs as f64);
        }
        let rest = wall_ns.saturating_sub(sum);
        println!("scanprof  unaccounted: {:10.1} ms  {:5.1}%  {:9.2} us/tx",
                 rest as f64 / 1e6, 100.0 * rest as f64 / wall_ns.max(1) as f64, rest as f64 / 1e3 / txs as f64);
        println!("scanprof counts: blocks {} txs {} staking {} vouts {} trial_decrypts {} wall {:.1} ms",
                 g(&self.blocks), g(&self.txs), g(&self.staking), g(&self.vouts), g(&self.trial_decrypts),
                 wall_ns as f64 / 1e6);
    }
}


pub fn scan_tx(info: &mut ScanInfo, utxos: &mut HashSet<(PubKeyID, u32)>, tx_bytes: &[u8], tx_i: usize, height: u32, ctx: &ScanCtx, txid_zeb: [u8; 32]) -> Result<bool, String> {
    let tz = Timer::scope("scan_tx");
    let mut new_info = false;
    info.max_height_seen = info.max_height_seen.max(height);

    let network = &TEST_NETWORK;
    let block_h = LRZBlockHeight::from_u32(height);
    let (tx, txid_lrz) = match timed(&PROF.parse_ns, || {
        Transaction::read(tx_bytes, BranchId::for_height(network, block_h)).map(|tx| { let id = tx.txid(); (tx, id) })
    }) {
        Ok(v) => v,
        Err(err) => return Err(format!("{err:?}")),
    };

    assert!(txid_zeb == <[u8;32]>::from(txid_lrz), "txids from zebra/librustzcash disagree: {} vs {}", txid_lrz, TxId::from_bytes(txid_zeb));

    // println!("scanning {txid_lrz} at height {height}");

    let mut contains_my_t_spend = false;
    let mut coinbase_ok = false;
    if let Some(t_bundle) = tx.transparent_bundle() {
        let tz = Timer::scope("scan_tx > t_bundle");
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
                info.coinbases_value += 500_000_000; // hardcoded for @Crosslink @Testnet
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
        let dup = timed(&PROF.transparent_ns, || {
            let mut dup = None;
            for (out_i, txout) in t_bundle.vout.iter().enumerate() {
                PROF.vouts.fetch_add(1, Relaxed);
                if let Some(t_addr) = txout.recipient_address() {
                    // Both addresses are derived once in ScanCtx; deriving them per output
                    // (t_addr_belongs_to_ufvk_index) was three quarters of the scan.
                    if t_addr == ctx.t_addr || t_addr == ctx.t_addr_p2sh {
                        let outpoint = (PubKeyID(*txid_lrz.as_ref()), out_i.try_into().unwrap());
                        if ! utxos.insert(outpoint) {
                            dup = Some(outpoint);
                            break;
                        }
                    }
                }
            }
            dup
        });
        if let Some(outpoint) = dup {
            return Err(format!("multiple receipts of the same UTXO: {:?}", outpoint));
        }
    }

    if tx_i == 0 && !coinbase_ok {
        return Err("no coinbase found".to_owned());
    }


    if let Some(staking_action) = tx.staking_action() {
        // Create and Convert both mint a bond (Convert is funded by the finalizer bank,
        // but the fee-paying notes are still ours)
        if let Some((_target, amount_zats, _salt)) = staking_action.bond_terms() {
            let unique_pubkey = staking_action.unique_pubkey();
            if contains_my_t_spend {
                println!("found staking action paid for by our transparent: {:?}", unique_pubkey);
            }
            let mut is_my_staking_action = contains_my_t_spend;

            if is_my_staking_action {
            } else if let Some(bundle) = tx.ironwood_bundle() {
                is_my_staking_action = timed(&PROF.orchard_ns, || {
                    for action in bundle.actions() {
                        let action: &orchard::Action<_> = action; // type-check
                        let domain = orchard::note_encryption::OrchardDomain::for_action(action);

                        for ovk in [&ctx.orchard_external_ovk, &ctx.orchard_internal_ovk] {
                            PROF.trial_decrypts.fetch_add(1, Relaxed);
                            if let Some((_note, _addr, _send_memo)) = try_output_recovery_with_ovk(
                                &domain,
                                ovk,
                                action,
                                action.cv_net(),
                                &action.encrypted_note().out_ciphertext
                            ) {
                                println!("found staking action paid for by our orchard: {:?}", unique_pubkey);
                                return true;
                            }
                        }
                    }
                    false
                });
            }


            if is_my_staking_action {
                new_info = true;
                info.bonds.push(ScanBond {
                    pk: PubKeyID(unique_pubkey),
                    initial_val: amount_zats,
                    create_txid: PubKeyID(<[u8;32]>::from(txid_lrz)),
                    create_height: height,
                });
            }
        }
    }

    Ok(new_info)
}
