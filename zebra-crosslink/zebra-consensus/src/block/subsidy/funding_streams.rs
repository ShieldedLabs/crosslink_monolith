//! Funding Streams calculations. - [§7.8][7.8]
//!
//! [7.8]: https://zips.z.cash/protocol/protocol.pdf#subsidies

use zebra_chain::{
    block::Height,
    parameters::{subsidy::*, Network},
    transparent::{self},
};

#[cfg(test)]
mod tests;

/// Returns the position in the address slice for each funding stream
/// as described in [protocol specification §7.10][7.10]
///
/// [7.10]: https://zips.z.cash/protocol/protocol.pdf#fundingstreams
fn funding_stream_address_index(
    height: Height,
    network: &Network,
    receiver: FundingStreamReceiver,
) -> Option<usize> {
    if receiver == FundingStreamReceiver::Deferred {
        return None;
    }

    if network.magic().0 == [b'C',b'l',b'T',b'0'] {
        return Some(0);
    }

    let funding_streams = network.funding_streams(height)?;
    let num_addresses = funding_streams.recipient(receiver)?.addresses().len();

    let index = 1u32
        .checked_add(funding_stream_address_period(height, network))
        .expect("no overflow should happen in this sum")
        .checked_sub(funding_stream_address_period(
            funding_streams.height_range().start,
            network,
        ))
        .expect("no overflow should happen in this sub") as usize;

    assert!(index > 0);
    // Wrap the index to prevent out-of-bounds panic when the address
    // period spans more intervals than there are addresses (Crosslink fix).
    Some((index - 1) % num_addresses)
}

/// Return the address corresponding to given height, network and funding stream receiver.
///
/// This function only returns transparent addresses, because the current Zcash funding streams
/// only use transparent addresses,
pub fn funding_stream_address(
    height: Height,
    network: &Network,
    receiver: FundingStreamReceiver,
) -> Option<&transparent::Address> {
    let index = funding_stream_address_index(height, network, receiver)?;
    let funding_streams = network.funding_streams(height)?;
    funding_streams.recipient(receiver)?.addresses().get(index)
}
