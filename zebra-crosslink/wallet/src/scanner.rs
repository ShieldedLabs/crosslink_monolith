use crate::*;

#[derive(Clone, Copy, Default, Debug)]
pub struct ScanInfo {
    coinbases_c: usize,
    coinbases_value: u64,
    coinbase_max_height: u32,

    bonds_c: usize,
    bonds_value: u64,
    bond_max_height: u32,

    max_height_seen: u32,
}
impl ScanInfo {
    pub fn total_value(&self) -> u64 {
        self.coinbases_value + self.bonds_value
    }
}


pub fn scan_tx(info: &mut ScanInfo, tx_bytes: &[u8], height: u32, ufvk: &UnifiedFullViewingKey, txid_zeb: [u8; 32]) -> Result<bool, String> {
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
    assert_eq!(txid_zeb, <[u8;32]>::from(txid_lrz), "txids from zebra/librustzcash disagree");

    if let Some(t_bundle) = tx.transparent_bundle() {
        if t_bundle.is_coinbase() {
            for output in &t_bundle.vout {
                if let Some(matched_addr) = output.recipient_address(){
                    if matched_addr == t_addr {
                        let value = output.value();
                        // println!("Found a match in a coinbase transaction at height {height}! Value: {value:?}");

                        new_info = true;
                        info.coinbases_c += 1;
                        info.coinbases_value += u64::from(value);
                        // info.total_value += u64::from(value);
                        debug_assert!(info.coinbase_max_height < height, "expected linear iteration");
                        info.coinbase_max_height = height;
                    }
                }
            }
        }
    }

    Ok(new_info)
}
