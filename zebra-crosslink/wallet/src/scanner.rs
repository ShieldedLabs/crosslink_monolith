use crate::*;

pub fn scan(tx_bytes: &[u8], height: u32, ufvk: &UnifiedFullViewingKey) -> Result<u64, String> {
    let mut total_value = 0u64;
    
    let Some((t_addr, p2sh, ua)) = addrs_from_ufvk(ufvk, 0) else{
        return Err("Could not get an address".to_owned());
    };

    let network = &TEST_NETWORK;
    let block_h = LRZBlockHeight::from_u32(height);
    let tx = match Transaction::read(tx_bytes, BranchId::for_height(network, block_h)){
        Ok(tx) => tx,
        Err(err) => return Err(format!("{err:?}")),        
    };
    if let Some(t_bundle) = tx.transparent_bundle() {
        if t_bundle.is_coinbase() {
            for output in &t_bundle.vout {
                if let Some(matched_addr) = output.recipient_address(){
                    if matched_addr == t_addr{
                        let value = output.value();
                        // println!("Found a match in a coinbase transaction at height {height}! Value: {value:?}");
                        total_value += u64::from(value);

                    }
                }
            }
        }
    }

    Ok(total_value)
}