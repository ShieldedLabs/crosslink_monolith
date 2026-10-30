//! Transaction relay over the node's own STP connections: the wire format.
//!
//! Relay is pull-based. A peer lists the ids in its mempool (`TX_STATUS`), we ask for the
//! ones we lack (`TX_REQ`) and it sends the transactions (`TX`). `MEMPOOL_REQ` asks for a
//! peer's whole listing once, when relay starts. Every message here is checked at ingest:
//! counts against their caps before anything is allocated, ids against their kinds, and
//! every transaction against the largest one a block could hold.

use std::sync::Arc;

use zebra_chain::block::MAX_BLOCK_BYTES;
use zebra_chain::serialization::{ZcashDeserialize, ZcashSerialize};
use zebra_chain::transaction::{self, AuthDigest, Transaction, UnminedTxId, WtxId};

use zebra_node_services::mempool::{self, MempoolTxSubscriber};
use zebra_node_services::BoxError;

use super::bft_sync::Reader;

/// The node's mempool service, as zebrad builds it.
pub type MempoolService =
    tower::buffer::Buffer<tower::util::BoxService<mempool::Request, mempool::Response, BoxError>, mempool::Request>;

/// What `sync` needs to relay transactions: the mempool, and its stream of changes.
pub struct TxRelayLaunch {
    pub mempool: MempoolService,
    pub changes: MempoolTxSubscriber,
}
use super::{PACKET_TYPE_MEMPOOL_REQ, PACKET_TYPE_TX, PACKET_TYPE_TX_REQ, PACKET_TYPE_TX_STATUS};

/// Ids one `TX_STATUS` lists at most. A larger mempool is listed in several messages.
pub const MAX_TX_IDS_PER_STATUS: usize = 256;
/// Ids one `TX_REQ` asks for, and transactions one `TX` carries, at most.
pub const MAX_TXS_PER_MESSAGE: usize = 64;
/// A `TX` message's size cap. A transaction that alone exceeds it is never sent.
pub const TX_MESSAGE_BUDGET: usize = 1 << 20;
/// The largest transaction: no block can hold a larger one, so no peer has a reason to send it.
// MAX_BLOCK_BYTES is 100_000 in this fork, so it fits usize everywhere.
pub const MAX_TX_BYTES: usize = MAX_BLOCK_BYTES as usize;

const ID_KIND_LEGACY: u8 = 0;
const ID_KIND_WITNESSED: u8 = 1;

fn write_id(out: &mut Vec<u8>, id: &UnminedTxId) {
    match id {
        UnminedTxId::Legacy(hash) => {
            out.push(ID_KIND_LEGACY);
            out.extend_from_slice(&hash.0);
        }
        UnminedTxId::Witnessed(wtx_id) => {
            out.push(ID_KIND_WITNESSED);
            out.extend_from_slice(&wtx_id.id.0);
            out.extend_from_slice(&wtx_id.auth_digest.0);
        }
    }
}

fn read_id(r: &mut Reader) -> Option<UnminedTxId> {
    let kind = r.take(1)?[0];
    let id = transaction::Hash(r.take(32)?.try_into().ok()?);
    match kind {
        ID_KIND_LEGACY => Some(UnminedTxId::Legacy(id)),
        ID_KIND_WITNESSED => {
            let auth_digest = AuthDigest(r.take(32)?.try_into().ok()?);
            Some(UnminedTxId::Witnessed(WtxId { id, auth_digest }))
        }
        _ => None,
    }
}

/// `packet_type`, then up to `cap` ids.
fn ids_packet<'a>(packet_type: u8, ids: impl IntoIterator<Item = &'a UnminedTxId>, cap: usize) -> Vec<u8> {
    let mut body = Vec::new();
    let mut n: u16 = 0;
    for id in ids.into_iter().take(cap) {
        write_id(&mut body, id);
        n += 1;
    }
    let mut out = vec![packet_type];
    out.extend_from_slice(&n.to_le_bytes());
    out.extend(body);
    out
}

fn read_ids(r: &mut Reader, cap: usize) -> Option<Vec<UnminedTxId>> {
    let n = usize::from(r.u16()?);
    if n > cap {
        return None;
    }
    let mut ids = Vec::with_capacity(n);
    for _ in 0..n {
        ids.push(read_id(r)?);
    }
    Some(ids)
}

/// `TX_STATUS`: the mempool's size, and up to [`MAX_TX_IDS_PER_STATUS`] of its ids, each with its
/// transaction's size in bytes, so a requester can budget what it asks for by bytes.
pub fn tx_status_packet<'a>(total: u32, entries: impl IntoIterator<Item = (&'a UnminedTxId, u32)>) -> Vec<u8> {
    let mut body = Vec::new();
    let mut n: u16 = 0;
    for (id, size) in entries.into_iter().take(MAX_TX_IDS_PER_STATUS) {
        write_id(&mut body, id);
        body.extend_from_slice(&size.to_le_bytes());
        n += 1;
    }
    let mut out = vec![PACKET_TYPE_TX_STATUS];
    out.extend_from_slice(&total.to_le_bytes());
    out.extend_from_slice(&n.to_le_bytes());
    out.extend(body);
    out
}

/// `body` is the message after its type byte: `(total, [(id, size)])`. A listed size over
/// [`MAX_TX_BYTES`] is malformed: no such transaction can exist.
pub fn read_tx_status(body: &[u8]) -> Option<(u32, Vec<(UnminedTxId, u32)>)> {
    let mut r = Reader(body);
    let total = r.u32()?;
    let n = usize::from(r.u16()?);
    if n > MAX_TX_IDS_PER_STATUS {
        return None;
    }
    let mut entries = Vec::with_capacity(n);
    for _ in 0..n {
        let id = read_id(&mut r)?;
        let size = r.u32()?;
        if size as usize > MAX_TX_BYTES {
            return None;
        }
        entries.push((id, size));
    }
    r.0.is_empty().then_some((total, entries))
}

/// `TX_REQ`: up to [`MAX_TXS_PER_MESSAGE`] ids.
pub fn tx_req_packet<'a>(ids: impl IntoIterator<Item = &'a UnminedTxId>) -> Vec<u8> {
    ids_packet(PACKET_TYPE_TX_REQ, ids, MAX_TXS_PER_MESSAGE)
}

pub fn read_tx_req(body: &[u8]) -> Option<Vec<UnminedTxId>> {
    let mut r = Reader(body);
    let ids = read_ids(&mut r, MAX_TXS_PER_MESSAGE)?;
    r.0.is_empty().then_some(ids)
}

/// `MEMPOOL_REQ`: no payload.
pub fn mempool_req_packet() -> Vec<u8> {
    vec![PACKET_TYPE_MEMPOOL_REQ]
}

/// `TX` messages carrying `txs`, each under [`TX_MESSAGE_BUDGET`] and [`MAX_TXS_PER_MESSAGE`].
/// A transaction larger than [`MAX_TX_BYTES`] is left out: no peer would accept it.
pub fn tx_packets<'a>(txs: impl IntoIterator<Item = &'a Transaction>) -> Vec<Vec<u8>> {
    let mut packets = Vec::new();
    let mut body = Vec::new();
    let mut n: u16 = 0;
    let flush = |body: &mut Vec<u8>, n: &mut u16, packets: &mut Vec<Vec<u8>>| {
        if *n > 0 {
            let mut out = vec![PACKET_TYPE_TX];
            out.extend_from_slice(&n.to_le_bytes());
            out.append(body);
            packets.push(out);
            *n = 0;
        }
    };
    for tx in txs {
        let bytes = tx.zcash_serialize_to_vec().expect("a transaction in our mempool serializes");
        if bytes.len() > MAX_TX_BYTES {
            continue;
        }
        if usize::from(n) == MAX_TXS_PER_MESSAGE || 3 + body.len() + 4 + bytes.len() > TX_MESSAGE_BUDGET {
            flush(&mut body, &mut n, &mut packets);
        }
        // Bounded by MAX_TX_BYTES above.
        body.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        body.extend_from_slice(&bytes);
        n += 1;
    }
    flush(&mut body, &mut n, &mut packets);
    packets
}

/// The transactions in a `TX` body. `None` if any is malformed: over [`MAX_TX_BYTES`], not a
/// transaction, or not exactly filling its length.
pub fn read_txs(body: &[u8]) -> Option<Vec<(Arc<Transaction>, u32)>> {
    let mut r = Reader(body);
    let n = usize::from(r.u16()?);
    if n > MAX_TXS_PER_MESSAGE {
        return None;
    }
    let mut txs = Vec::with_capacity(n);
    for _ in 0..n {
        let len = r.u32()? as usize;
        // zebra's decoder also stops at MAX_BLOCK_BYTES; this refuses before decoding starts.
        if len > MAX_TX_BYTES {
            return None;
        }
        let mut bytes = r.take(len)?;
        let tx = Transaction::zcash_deserialize(&mut bytes).ok()?;
        if !bytes.is_empty() {
            return None;
        }
        // Bounded by MAX_TX_BYTES above.
        txs.push((Arc::new(tx), len as u32));
    }
    r.0.is_empty().then_some(txs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zebra_chain::serialization::ZcashDeserializeInto;

    fn some_txs() -> Vec<Transaction> {
        // Real transactions from the test vectors: v4 (legacy ids) and v5 (witnessed ids).
        zebra_test::vectors::MAINNET_BLOCKS
            .iter()
            .take(500)
            .filter_map(|(_, bytes)| bytes.zcash_deserialize_into::<zebra_chain::block::Block>().ok())
            .flat_map(|block| block.transactions.iter().map(|tx| (**tx).clone()).collect::<Vec<_>>())
            .collect()
    }

    #[test]
    fn tx_relay_messages_round_trip() {
        let txs = some_txs();
        assert!(!txs.is_empty());
        let mut ids: Vec<UnminedTxId> = txs.iter().map(UnminedTxId::from).collect();
        // Witnessed (v5) ids too, whatever the vectors hold.
        ids.insert(0, UnminedTxId::Witnessed(WtxId { id: transaction::Hash([7; 32]), auth_digest: AuthDigest([8; 32]) }));

        let entries: Vec<(UnminedTxId, u32)> = ids.iter().enumerate().map(|(i, id)| (*id, i as u32)).collect();
        let status = tx_status_packet(9_000, entries.iter().map(|(id, size)| (id, *size)));
        assert_eq!(status[0], PACKET_TYPE_TX_STATUS);
        let (total, listed) = read_tx_status(&status[1..]).unwrap();
        assert_eq!(total, 9_000);
        assert_eq!(listed, entries.iter().take(MAX_TX_IDS_PER_STATUS).copied().collect::<Vec<_>>());

        let req = tx_req_packet(&ids);
        assert_eq!(read_tx_req(&req[1..]).unwrap(), ids.iter().take(MAX_TXS_PER_MESSAGE).copied().collect::<Vec<_>>());

        let packets = tx_packets(&txs);
        let got: Vec<Transaction> = packets.iter().flat_map(|p| read_txs(&p[1..]).unwrap()).map(|(tx, _)| (*tx).clone()).collect();
        assert_eq!(got, txs);
        assert!(packets.iter().all(|p| p.len() <= TX_MESSAGE_BUDGET));
    }

    #[test]
    fn malformed_tx_relay_messages_are_refused() {
        let txs = some_txs();
        let ids: Vec<UnminedTxId> = txs.iter().map(UnminedTxId::from).collect();

        // Counts over their caps, before anything is read for them.
        let mut too_many = 0u32.to_le_bytes().to_vec();
        too_many.extend_from_slice(&(MAX_TX_IDS_PER_STATUS as u16 + 1).to_le_bytes());
        assert_eq!(read_tx_status(&too_many), None);
        assert_eq!(read_tx_req(&(MAX_TXS_PER_MESSAGE as u16 + 1).to_le_bytes()), None);
        assert_eq!(read_txs(&(MAX_TXS_PER_MESSAGE as u16 + 1).to_le_bytes()), None);

        // An unknown id kind, a truncation, a trailing byte.
        let mut bad_kind = tx_req_packet(&ids[..1]);
        bad_kind[3] = 2;
        assert_eq!(read_tx_req(&bad_kind[1..]), None);
        let status = tx_status_packet(1, ids[..3].iter().map(|id| (id, 100)));
        assert_eq!(read_tx_status(&status[1..status.len() - 1]), None);
        let mut trailing = status.clone();
        trailing.push(0);
        assert_eq!(read_tx_status(&trailing[1..]), None);
        // A listed size no transaction can have.
        let oversize = tx_status_packet(1, ids[..1].iter().map(|id| (id, MAX_TX_BYTES as u32 + 1)));
        assert_eq!(read_tx_status(&oversize[1..]), None);

        // A transaction longer than any block is refused (by our cap, and by zebra's decoder,
        // which reads at most MAX_BLOCK_BYTES)...
        let mut huge = txs.iter().rev().find(|tx| !tx.outputs().is_empty()).unwrap().clone();
        let output = huge.outputs()[0].clone();
        huge.outputs_mut().extend(std::iter::repeat_n(output, MAX_TX_BYTES / 20));
        let huge_bytes = huge.zcash_serialize_to_vec().unwrap();
        assert!(huge_bytes.len() > MAX_TX_BYTES);
        let mut huge_body = 1u16.to_le_bytes().to_vec();
        huge_body.extend_from_slice(&(huge_bytes.len() as u32).to_le_bytes());
        huge_body.extend_from_slice(&huge_bytes);
        assert_eq!(read_txs(&huge_body), None);
        // ...and it is never sent.
        assert!(tx_packets([&huge]).is_empty());

        // A transaction with a byte left over inside its length.
        let mut padded = vec![PACKET_TYPE_TX];
        padded.extend_from_slice(&1u16.to_le_bytes());
        let bytes = txs[0].zcash_serialize_to_vec().unwrap();
        padded.extend_from_slice(&(bytes.len() as u32 + 1).to_le_bytes());
        padded.extend_from_slice(&bytes);
        padded.push(0);
        assert_eq!(read_txs(&padded[1..]), None);
    }
}
