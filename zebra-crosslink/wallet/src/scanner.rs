use crate::*;
use crate::bft::{ ScanInfo, ScanBond, PubKeyID };
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct ScanCtx {
    pub ufvk: zcash_keys::keys::UnifiedFullViewingKey,
    pub t_addr: TransparentAddress,
    pub t_addr_p2sh: TransparentAddress,
    /// Lock scripts of the two addresses above. Outputs are matched by comparing script bytes,
    /// so the transparent pass parses nothing and derives nothing per output.
    pub p2pkh_script: Vec<u8>,
    pub p2sh_script: Vec<u8>,
    pub orchard_external_ovk: orchard::keys::OutgoingViewingKey,
    pub orchard_internal_ovk: orchard::keys::OutgoingViewingKey,
}

impl ScanCtx {
    pub fn new(
        ufvk: zcash_keys::keys::UnifiedFullViewingKey,
        t_addr: TransparentAddress,
        t_addr_p2sh: TransparentAddress,
        orchard_external_ovk: orchard::keys::OutgoingViewingKey,
        orchard_internal_ovk: orchard::keys::OutgoingViewingKey,
    ) -> Self {
        let p2pkh_script = lock_script(&t_addr);
        let p2sh_script = lock_script(&t_addr_p2sh);
        Self { ufvk, t_addr, t_addr_p2sh, p2pkh_script, p2sh_script, orchard_external_ovk, orchard_internal_ovk }
    }
}

/// The standard lock script of a transparent address, byte for byte as it appears in an output.
pub fn lock_script(addr: &TransparentAddress) -> Vec<u8> {
    match addr {
        TransparentAddress::PublicKeyHash(hash) => {
            let mut script = Vec::with_capacity(25);
            script.extend_from_slice(&[0x76, 0xa9, 0x14]); // OP_DUP OP_HASH160 push20
            script.extend_from_slice(hash);
            script.extend_from_slice(&[0x88, 0xac]); // OP_EQUALVERIFY OP_CHECKSIG
            script
        }
        TransparentAddress::ScriptHash(hash) => {
            let mut script = Vec::with_capacity(23);
            script.extend_from_slice(&[0xa9, 0x14]); // OP_HASH160 push20
            script.extend_from_slice(hash);
            script.push(0x87); // OP_EQUAL
            script
        }
    }
}

// Profiling for total_issuance_from_key: nanoseconds per phase plus the unit counts that drive
// each phase. Atomics so the scan functions keep plain signatures and the async block fetch can
// be timed inline.
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

/// librustzcash view of a tx plus its txid, for the parts of the scan that need shielded data.
pub fn parse_tx(tx_bytes: &[u8], height: u32) -> Result<(Transaction, [u8; 32]), String> {
    let block_h = LRZBlockHeight::from_u32(height);
    match Transaction::read(tx_bytes, BranchId::for_height(&TEST_NETWORK, block_h)) {
        Ok(tx) => {
            let txid = <[u8; 32]>::from(tx.txid());
            Ok((tx, txid))
        }
        Err(err) => Err(format!("{err:?}")),
    }
}

/// Transparent pass over one tx for one ufvk, on whatever the caller already has decoded:
/// `inputs` yields (prevout txid, index), `outputs` yields lock-script bytes in vout order.
/// `txid` is called only when an output is ours; the caller memoizes it across ufvks.
/// Returns (new_info, contains_my_t_spend).
pub fn scan_tx_transparent<'a>(
    info: &mut ScanInfo,
    utxos: &mut HashSet<(PubKeyID, u32)>,
    ctx: &ScanCtx,
    height: u32,
    is_coinbase: bool,
    inputs: impl Iterator<Item = ([u8; 32], u32)>,
    outputs: impl Iterator<Item = &'a [u8]>,
    txid: &mut impl FnMut() -> [u8; 32],
) -> Result<(bool, bool), String> {
    info.max_height_seen = info.max_height_seen.max(height);

    let mut contains_my_t_spend = false;
    for (prev_txid, n) in inputs {
        if utxos.contains(&(PubKeyID(prev_txid), n)) {
            contains_my_t_spend = true;
        }
    }

    // track received UTXOs so we can later determine if we spent that UTXO on a staking action
    let mut coinbase_to_us = false;
    for (out_i, script) in outputs.enumerate() {
        PROF.vouts.fetch_add(1, Relaxed);
        let p2pkh = script == ctx.p2pkh_script.as_slice();
        if p2pkh || script == ctx.p2sh_script.as_slice() {
            coinbase_to_us |= is_coinbase && p2pkh;
            let outpoint = (PubKeyID(txid()), out_i as u32);
            if ! utxos.insert(outpoint) {
                return Err(format!("multiple receipts of the same UTXO: {:?}", outpoint));
            }
        }
    }

    let mut new_info = false;
    if coinbase_to_us {
        new_info = true;
        info.coinbases_c += 1;
        info.coinbases_value += 500_000_000; // hardcoded for @testnet ClT0
        debug_assert!(info.coinbase_max_height < height, "expected linear iteration");
        info.coinbase_max_height = height;
    }

    Ok((new_info, contains_my_t_spend))
}

/// Staking pass over one parsed tx for one ufvk. `contains_my_t_spend` comes from the
/// transparent pass of the same tx. Returns whether a bond was recorded.
pub fn scan_tx_staking(info: &mut ScanInfo, tx: &Transaction, txid: [u8; 32], contains_my_t_spend: bool, height: u32, ctx: &ScanCtx) -> bool {
    let Some(staking_action) = tx.staking_action() else { return false };
    if staking_action.kind != StakingActionKind::CreateNewDelegationBond {
        return false;
    }
    let staking_action = StakingAction_CreateNewDelegationBond::try_from_union(&staking_action).unwrap();

    if contains_my_t_spend {
        println!("found staking action paid for by our transparent: {:?}", staking_action.unique_pubkey);
    }
    let mut is_my_staking_action = contains_my_t_spend;

    if ! is_my_staking_action {
        if let Some(bundle) = tx.orchard_bundle() {
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
                            println!("found staking action paid for by our orchard: {:?}", staking_action.unique_pubkey);
                            return true;
                        }
                    }
                }
                false
            });
        }
    }

    if ! is_my_staking_action {
        return false;
    }

    info.bonds.push(ScanBond {
        pk: PubKeyID(staking_action.unique_pubkey),
        initial_val: staking_action.amount_zats,
        create_txid: PubKeyID(txid),
        create_height: height,
    });
    true
}
