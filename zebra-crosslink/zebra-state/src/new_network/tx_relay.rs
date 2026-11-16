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

/// The ids a whole packet asks for, if it is a well-formed `TX_REQ`.
pub fn parse_tx_req(packet: &[u8]) -> Option<Vec<UnminedTxId>> {
    let (&packet_type, body) = packet.split_first()?;
    (packet_type == PACKET_TYPE_TX_REQ).then(|| read_tx_req(body)).flatten()
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

    // ---- property tests: these parsers run at ingest, on whatever bytes a peer sends ----

    use proptest::prelude::*;
    use proptest::sample::Index;

    /// The real transactions, decoded once: arbitrary ones are too expensive to generate, so
    /// `TX` bodies are fuzzed as arbitrary bytes and as mutations of messages built from these.
    static TXS: std::sync::LazyLock<Vec<Transaction>> = std::sync::LazyLock::new(some_txs);
    /// Of [`TXS`], those with a transparent output to copy, for growing one to any size.
    static TXS_WITH_OUTPUTS: std::sync::LazyLock<Vec<&'static Transaction>> = std::sync::LazyLock::new(|| TXS.iter().filter(|tx| !tx.outputs().is_empty()).collect());

    /// The largest `TX_STATUS` and `TX_REQ`: every id witnessed (the longer kind).
    const MAX_TX_STATUS_LEN: usize = 1 + 4 + 2 + MAX_TX_IDS_PER_STATUS * (1 + 64 + 4);
    const MAX_TX_REQ_LEN: usize = 1 + 2 + MAX_TXS_PER_MESSAGE * (1 + 64);

    fn any_id() -> impl Strategy<Value = UnminedTxId> {
        prop_oneof![
            any::<[u8; 32]>().prop_map(|id| UnminedTxId::Legacy(transaction::Hash(id))),
            (any::<[u8; 32]>(), any::<[u8; 32]>()).prop_map(|(id, auth_digest)| UnminedTxId::Witnessed(WtxId { id: transaction::Hash(id), auth_digest: AuthDigest(auth_digest) })),
        ]
    }

    /// A body: `prefix_len` arbitrary bytes, a count from the edges of its range (0, the cap,
    /// one past it, u16::MAX), then anything. Uniform bytes would rarely get past the count.
    fn any_body(prefix_len: usize, cap: usize) -> impl Strategy<Value = Vec<u8>> {
        // The caps fit u16 (MAX_TX_IDS_PER_STATUS is 256), so one past them does too.
        let count = prop_oneof![Just(0u16), Just(1), Just(cap as u16), Just(cap as u16 + 1), Just(u16::MAX), any::<u16>()];
        let shaped = (proptest::collection::vec(any::<u8>(), prefix_len), count, proptest::collection::vec(any::<u8>(), 0..2048)).prop_map(|(mut body, count, rest)| {
            body.extend_from_slice(&count.to_le_bytes());
            body.extend(rest);
            body
        });
        prop_oneof![proptest::collection::vec(any::<u8>(), 0..16), proptest::collection::vec(any::<u8>(), 0..4096), shaped]
    }

    /// One change to a well-formed body. Which ones must be refused is in [`mutated`].
    #[derive(Debug, Clone)]
    enum Mutation {
        Truncate(Index),
        Append(Vec<u8>),
        FlipBit(Index, u8),
        SetCount(u16),
    }

    fn any_mutation() -> impl Strategy<Value = Mutation> {
        prop_oneof![
            any::<Index>().prop_map(Mutation::Truncate),
            proptest::collection::vec(any::<u8>(), 1..70).prop_map(Mutation::Append),
            (any::<Index>(), 0..8u8).prop_map(|(at, bit)| Mutation::FlipBit(at, bit)),
            any::<u16>().prop_map(Mutation::SetCount),
        ]
    }

    /// `body` (with its u16 count at `count_at`) mutated, and whether a reader must refuse it: a
    /// cut, an extra byte, or a count other than what follows always leaves it malformed. A
    /// flipped bit may still read, as whatever it now says.
    fn mutated(body: &[u8], count_at: usize, mutation: &Mutation) -> (Vec<u8>, bool) {
        let mut out = body.to_vec();
        match mutation {
            Mutation::Truncate(at) => {
                out.truncate(at.index(body.len()));
                (out, true)
            }
            Mutation::Append(bytes) => {
                out.extend_from_slice(bytes);
                (out, true)
            }
            Mutation::FlipBit(at, bit) => {
                out[at.index(body.len())] ^= 1 << bit;
                (out, false)
            }
            Mutation::SetCount(count) => {
                let refused = out[count_at..count_at + 2] != count.to_le_bytes();
                out[count_at..count_at + 2].copy_from_slice(&count.to_le_bytes());
                (out, refused)
            }
        }
    }

    /// What `read_txs` accepted, encoded again. Equal to the body it read only if every
    /// transaction it decoded is exactly the bytes it was sent as.
    fn txs_body(txs: &[(Arc<Transaction>, u32)]) -> Vec<u8> {
        // At most MAX_TXS_PER_MESSAGE, which read_txs checked.
        let mut body = (txs.len() as u16).to_le_bytes().to_vec();
        for (tx, len) in txs {
            body.extend_from_slice(&len.to_le_bytes());
            body.extend(tx.zcash_serialize_to_vec().unwrap());
        }
        body
    }

    /// A real transaction with `extra` copies of its first output: any size up to past
    /// [`MAX_TX_BYTES`].
    fn grown_tx(base: Index, extra: usize) -> Transaction {
        let mut tx: Transaction = (*base.get(&TXS_WITH_OUTPUTS)).clone();
        let output = tx.outputs()[0].clone();
        tx.outputs_mut().extend(std::iter::repeat_n(output, extra));
        tx
    }

    #[test]
    fn a_count_alone_never_allocates_more_than_its_cap_allows() {
        // Each reader checks its count against a cap before `Vec::with_capacity(count)`, so a
        // body declaring u16::MAX items and holding none reserves at most the cap's worth. Kept
        // small, it needs no bound by what the body could hold.
        let largest = [
            MAX_TX_IDS_PER_STATUS * std::mem::size_of::<(UnminedTxId, u32)>(),
            MAX_TXS_PER_MESSAGE * std::mem::size_of::<UnminedTxId>(),
            MAX_TXS_PER_MESSAGE * std::mem::size_of::<(Arc<Transaction>, u32)>(),
        ];
        assert!(largest.iter().all(|bytes| *bytes <= 32 << 10), "{largest:?}");
        // A declared count past its cap is refused with nothing after it.
        for count in [MAX_TXS_PER_MESSAGE as u16 + 1, u16::MAX] {
            assert_eq!(read_tx_req(&count.to_le_bytes()), None);
            assert_eq!(read_txs(&count.to_le_bytes()), None);
            let mut status = 0u32.to_le_bytes().to_vec();
            status.extend_from_slice(&count.max(MAX_TX_IDS_PER_STATUS as u16 + 1).to_le_bytes());
            assert_eq!(read_tx_status(&status), None);
        }
    }

    #[test]
    fn a_mempool_request_is_its_type_byte_alone() {
        assert_eq!(mempool_req_packet(), vec![PACKET_TYPE_MEMPOOL_REQ]);
    }

    #[test]
    fn a_tx_message_cut_at_any_offset_is_refused() {
        // Real transactions of both id kinds where the vectors have them, a few KB in all.
        let legacy = TXS.iter().filter(|tx| matches!(UnminedTxId::from(*tx), UnminedTxId::Legacy(_)));
        let witnessed = TXS.iter().filter(|tx| matches!(UnminedTxId::from(*tx), UnminedTxId::Witnessed(_)));
        let txs: Vec<&Transaction> = legacy.take(3).chain(witnessed.take(2)).collect();
        let packets = tx_packets(txs.iter().copied());
        assert_eq!(packets.len(), 1);
        let body = &packets[0][1..];
        assert_eq!(read_txs(body).map(|read| read.len()), Some(txs.len()));
        for cut in 0..body.len() {
            assert_eq!(read_txs(&body[..cut]), None, "cut at {cut} of {}", body.len());
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(1024))]

        #[test]
        fn arbitrary_bytes_never_read_as_a_tx_status_that_is_not_exactly_them(body in any_body(4, MAX_TX_IDS_PER_STATUS)) {
            if let Some((total, entries)) = read_tx_status(&body) {
                prop_assert!(entries.len() <= MAX_TX_IDS_PER_STATUS);
                prop_assert!(entries.iter().all(|(_, size)| *size as usize <= MAX_TX_BYTES));
                prop_assert_eq!(&tx_status_packet(total, entries.iter().map(|(id, size)| (id, *size)))[1..], &body[..]);
            }
        }

        #[test]
        fn arbitrary_bytes_never_read_as_a_tx_req_that_is_not_exactly_them(body in any_body(0, MAX_TXS_PER_MESSAGE)) {
            if let Some(ids) = read_tx_req(&body) {
                prop_assert!(ids.len() <= MAX_TXS_PER_MESSAGE);
                prop_assert_eq!(&tx_req_packet(&ids)[1..], &body[..]);
            }
        }

        #[test]
        fn a_tx_req_packet_parses_only_with_its_own_type_byte(packet_type in prop_oneof![Just(PACKET_TYPE_TX_REQ), any::<u8>()], body in any_body(0, MAX_TXS_PER_MESSAGE)) {
            let mut packet = vec![packet_type];
            packet.extend_from_slice(&body);
            let expected = if packet_type == PACKET_TYPE_TX_REQ { read_tx_req(&body) } else { None };
            prop_assert_eq!(parse_tx_req(&packet), expected);
            prop_assert_eq!(parse_tx_req(&[]), None);
        }

        #[test]
        fn arbitrary_bytes_never_read_as_txs_that_are_not_exactly_them(body in any_body(0, MAX_TXS_PER_MESSAGE)) {
            if let Some(txs) = read_txs(&body) {
                prop_assert!(txs.len() <= MAX_TXS_PER_MESSAGE);
                prop_assert_eq!(txs_body(&txs), body);
            }
        }

        #[test]
        fn a_tx_status_round_trips_up_to_its_cap_and_its_largest_size(total: u32, entries in proptest::collection::vec((any_id(), prop_oneof![Just(MAX_TX_BYTES as u32), 0..=MAX_TX_BYTES as u32]), 0..=MAX_TX_IDS_PER_STATUS + 64)) {
            let packet = tx_status_packet(total, entries.iter().map(|(id, size)| (id, *size)));
            prop_assert_eq!(packet[0], PACKET_TYPE_TX_STATUS);
            prop_assert!(packet.len() <= MAX_TX_STATUS_LEN);
            let listed = entries.len().min(MAX_TX_IDS_PER_STATUS);
            prop_assert_eq!(read_tx_status(&packet[1..]), Some((total, entries[..listed].to_vec())));
        }

        #[test]
        fn a_tx_req_round_trips_up_to_its_cap(ids in proptest::collection::vec(any_id(), 0..=MAX_TXS_PER_MESSAGE + 16)) {
            let packet = tx_req_packet(&ids);
            prop_assert!(packet.len() <= MAX_TX_REQ_LEN);
            let asked = ids.len().min(MAX_TXS_PER_MESSAGE);
            prop_assert_eq!(parse_tx_req(&packet), Some(ids[..asked].to_vec()));
        }

        #[test]
        fn a_mutated_tx_status_is_refused_or_reads_as_exactly_what_it_says(total: u32, entries in proptest::collection::vec((any_id(), 0..=MAX_TX_BYTES as u32), 1..=MAX_TX_IDS_PER_STATUS), mutation in any_mutation()) {
            let packet = tx_status_packet(total, entries.iter().map(|(id, size)| (id, *size)));
            let (body, refused) = mutated(&packet[1..], 4, &mutation);
            match read_tx_status(&body) {
                Some((total, entries)) => {
                    prop_assert!(!refused, "{mutation:?} read as {} entries", entries.len());
                    prop_assert_eq!(&tx_status_packet(total, entries.iter().map(|(id, size)| (id, *size)))[1..], &body[..]);
                }
                None => {}
            }
        }

        #[test]
        fn a_mutated_tx_req_is_refused_or_reads_as_exactly_what_it_says(ids in proptest::collection::vec(any_id(), 1..=MAX_TXS_PER_MESSAGE), mutation in any_mutation()) {
            let packet = tx_req_packet(&ids);
            let (body, refused) = mutated(&packet[1..], 0, &mutation);
            if let Some(ids) = read_tx_req(&body) {
                prop_assert!(!refused, "{mutation:?} read as {} ids", ids.len());
                prop_assert_eq!(&tx_req_packet(&ids)[1..], &body[..]);
            }
        }
    }

    proptest! {
        // Each case decodes up to a few real transactions.
        #![proptest_config(ProptestConfig::with_cases(512))]

        #[test]
        fn a_mutated_tx_message_is_refused_or_reads_as_exactly_what_it_says(picks in proptest::collection::vec(any::<Index>(), 1..6), mutation in any_mutation()) {
            let txs: Vec<&Transaction> = picks.iter().map(|pick| pick.get(&TXS)).collect();
            let packets = tx_packets(txs.iter().copied());
            prop_assert_eq!(packets.len(), 1);
            let (body, refused) = mutated(&packets[0][1..], 0, &mutation);
            if let Some(read) = read_txs(&body) {
                prop_assert!(!refused, "{mutation:?} read as {} txs", read.len());
                prop_assert_eq!(txs_body(&read), body);
            }
        }
    }

    proptest! {
        // Each case builds and decodes up to a few MB of transactions.
        #![proptest_config(ProptestConfig::with_cases(24))]

        #[test]
        fn tx_messages_stay_within_their_budget_and_count_and_carry_every_sendable_tx_in_order(specs in proptest::collection::vec((any::<Index>(), prop_oneof![0..64usize, 0..6000usize]), 0..40)) {
            let txs: Vec<Transaction> = specs.iter().map(|(base, extra)| grown_tx(*base, *extra)).collect();
            let sizes: Vec<usize> = txs.iter().map(|tx| tx.zcash_serialize_to_vec().unwrap().len()).collect();
            let sendable: Vec<(&Transaction, usize)> = txs.iter().zip(sizes.iter().copied()).filter(|(_, size)| *size <= MAX_TX_BYTES).collect();

            let packets = tx_packets(&txs);
            let mut read = Vec::new();
            for (i, packet) in packets.iter().enumerate() {
                prop_assert_eq!(packet[0], PACKET_TYPE_TX);
                prop_assert!(packet.len() <= TX_MESSAGE_BUDGET, "packet {} is {} bytes", i, packet.len());
                let txs = read_txs(&packet[1..]).unwrap();
                prop_assert!(!txs.is_empty() && txs.len() <= MAX_TXS_PER_MESSAGE);
                // Full before the next starts: the next one's first transaction did not fit.
                if let Some(next) = sendable.get(read.len() + txs.len()) {
                    prop_assert!(txs.len() == MAX_TXS_PER_MESSAGE || packet.len() + 4 + next.1 > TX_MESSAGE_BUDGET);
                }
                read.extend(txs);
            }
            prop_assert_eq!(read.len(), sendable.len());
            for ((got, len), (sent, size)) in read.iter().zip(&sendable) {
                prop_assert_eq!(&**got, *sent);
                prop_assert_eq!(*len as usize, *size);
            }
        }
    }
}
