use super::*;
use crate::d28::merkle::{Bitmap, HolderContext, Tree};
use ed25519_zebra::SigningKey;

fn fixture() -> (Vec<u8>, Tree, Reference, HolderAttestation, Ticket) {
    let body = vec![43; 32768 * 8 + 17];
    let tree = Tree::new(&body, 32768, body.len()).unwrap();
    let reference = Reference {
        config: [1; 32],
        height: 7,
        manifest: tree.manifest([2; 32], [3; 32]),
    };
    let holder = HolderAttestation::sign(
        &HolderContext {
            domain: [4; 32],
            config: reference.config,
            height: reference.height,
        },
        &reference.manifest,
        &SigningKey::from([9; 32]),
        body.len(),
    )
    .unwrap();
    let ticket = Ticket {
        reference,
        generation: 11,
        token: 13,
    };
    (body, tree, reference, holder, ticket)
}
#[test]
fn have_exact_padding_bounds_and_borrowed_inventory() {
    let (body, _, reference, holder, _) = fixture();
    let mut bitmap = Bitmap::new(&reference.manifest, body.len()).unwrap();
    bitmap.set(8).unwrap();
    let bytes = encode_have(
        Have {
            reference,
            holder,
            bitmap: bitmap.as_bytes(),
        },
        body.len(),
    )
    .unwrap();
    let parsed = decode_have(&bytes, body.len()).unwrap();
    assert_eq!(parsed.reference, reference);
    assert_eq!(parsed.holder, holder);
    assert_eq!(parsed.bitmap, &[0, 1]);
    assert_eq!(parsed.bitmap.as_ptr(), bytes[HAVE_BYTES..].as_ptr());
    for end in 0..bytes.len() {
        assert!(decode_have(&bytes[..end], body.len()).is_err());
    }
    let mut extra = bytes.clone();
    extra.push(0);
    assert!(decode_have(&extra, body.len()).is_err());
    let mut padding = bytes.clone();
    *padding.last_mut().unwrap() = 2;
    assert!(decode_have(&padding, body.len()).is_err());
    let mut enormous = bytes.clone();
    enormous[106..114].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(decode_have(&enormous, body.len()).is_err());
    assert!(decode_have(&bytes, body.len() - 1).is_err());
    let mut wrong_version = bytes;
    wrong_version[1] = 2;
    assert!(decode_have(&wrong_version, body.len()).is_err());
}
#[test]
fn fixed_missing_window_rejects_zero_repeated_and_out_of_range_indices() {
    let (body, _, _, _, ticket) = fixture();
    let want = Want {
        ticket,
        indices: [0, 8, 0, 0, 0, 0, 0, 0],
        count: 2,
    };
    let bytes = encode_want(want, body.len()).unwrap();
    assert_eq!(decode_want(&bytes, body.len()).unwrap(), want);
    for end in 0..bytes.len() {
        assert!(decode_want(&bytes[..end], body.len()).is_err());
    }
    let mut bad = want;
    bad.indices[1] = 0;
    assert!(encode_want(bad, body.len()).is_err());
    bad.indices[1] = 9;
    assert!(encode_want(bad, body.len()).is_err());
    bad.count = 0;
    assert!(encode_want(bad, body.len()).is_err());
    bad.count = 9;
    assert!(encode_want(bad, body.len()).is_err());
    let mut count = bytes.clone();
    count[TICKET_BYTES] = 255;
    assert!(decode_want(&count, body.len()).is_err());
    let mut extra = bytes;
    extra.push(0);
    assert!(decode_want(&extra, body.len()).is_err());
}
#[test]
fn leaf_exact_tail_proof_depth_and_no_trailing_or_truncated_data() {
    let (body, tree, reference, _, ticket) = fixture();
    for index in [0, 8] {
        let data = &body[tree.layout().leaf_range(index).unwrap()];
        let proof = tree.proof(index).unwrap();
        let bytes = encode_leaf(ticket, index as u32, data, &proof, body.len()).unwrap();
        let leaf = decode_leaf(&bytes, body.len()).unwrap();
        assert_eq!(leaf.ticket, ticket);
        assert_eq!(leaf.index, index as u32);
        assert_eq!(leaf.data, data);
        assert_eq!(
            leaf.data.as_ptr(),
            bytes[TICKET_BYTES + 4 + proof.len() * 32..].as_ptr()
        );
        let (hashes, count) = leaf.proof_array();
        crate::d28::merkle::verify_leaf(
            &reference.manifest,
            index,
            leaf.data,
            &hashes[..count],
            body.len(),
        )
        .unwrap();
        for end in 0..bytes.len() {
            assert!(decode_leaf(&bytes[..end], body.len()).is_err());
        }
        let mut extra = bytes;
        extra.push(0);
        assert!(decode_leaf(&extra, body.len()).is_err());
        assert!(
            encode_leaf(
                ticket,
                index as u32,
                data,
                &proof[..proof.len() - 1],
                body.len()
            )
            .is_err()
        );
        assert!(
            encode_leaf(
                ticket,
                index as u32,
                &data[..data.len() - 1],
                &proof,
                body.len()
            )
            .is_err()
        );
    }
}
#[test]
fn acknowledgements_and_geometry_limits_are_exact() {
    let (body, _, _, _, ticket) = fixture();
    let mut ack = Ack::single(ticket, 8);
    assert!(ack.add(1));
    assert!(!ack.add(1)); // duplicate receipt is not duplicated on the wire
    let bytes = encode_ack(ack, body.len()).unwrap();
    assert_eq!(decode_ack(&bytes, body.len()).unwrap(), ack);
    for end in 0..bytes.len() {
        assert!(decode_ack(&bytes[..end], body.len()).is_err());
    }
    let mut repeated = bytes.clone();
    repeated[TICKET_BYTES + 5..TICKET_BYTES + 9].copy_from_slice(&8u32.to_le_bytes());
    assert!(decode_ack(&repeated, body.len()).is_err());
    let mut bad_count = bytes.clone();
    bad_count[TICKET_BYTES] = 0;
    assert!(decode_ack(&bad_count, body.len()).is_err());
    bad_count[TICKET_BYTES] = WINDOW as u8 + 1;
    assert!(decode_ack(&bad_count, body.len()).is_err());
    let mut extra = bytes;
    extra.push(0);
    assert!(decode_ack(&extra, body.len()).is_err());
    assert!(encode_ack(Ack::single(Ticket { token: 0, ..ticket }, 0), body.len()).is_err());
    for size in [32768, 102400, 262144, 1048576] {
        assert!(max_frame_bytes(10 << 20, size).is_ok());
    }
    for size in [0, 32767, 1048577, usize::MAX] {
        assert!(max_frame_bytes(10 << 20, size).is_err());
    }
    assert!(max_frame_bytes(usize::MAX, 32768).is_err());
}
