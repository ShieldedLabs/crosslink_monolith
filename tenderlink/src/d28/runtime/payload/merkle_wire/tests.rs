use super::*;
use crate::d28::merkle::{Bitmap, HolderContext, Tree};
use ed25519_zebra::SigningKey;

fn fixture() -> (Vec<u8>, Tree, Authentication) {
    let body = vec![43; 32768 * 8 + 17];
    let tree = Tree::new(&body, 32768, body.len()).unwrap();
    let reference = Reference {
        config: [1; 32],
        height: 7,
        manifest: tree.manifest([2; 32], [3; 32]),
    };
    let proposer = ProposerAttestation::sign(
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
    // Codec fixtures are synthetic, not runtime authentication fixtures.
    let header = Header {
        content: ProposalContent {
            height: 7,
            round: 2,
            valid_round: Some(1),
            id: [2; 32],
            signer: 0,
        },
        signature: [8; 64],
    };
    (
        body,
        tree,
        Authentication {
            reference,
            header,
            proposer,
        },
    )
}
#[test]
fn have_exact_padding_bounds_and_borrowed_inventory() {
    let (body, _, auth) = fixture();
    let mut bitmap = Bitmap::new(&auth.reference.manifest, body.len()).unwrap();
    bitmap.set(8).unwrap();
    let bytes = encode_have(
        Have {
            auth,
            bitmap: bitmap.as_bytes(),
        },
        body.len(),
    )
    .unwrap();
    assert_eq!(bytes.len(), have_size(auth.reference, body.len()).unwrap());
    let parsed = decode_have(&bytes, body.len()).unwrap();
    assert_eq!(parsed.auth, auth);
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
    enormous[HEADER_BYTES + 106..HEADER_BYTES + 114].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(decode_have(&enormous, body.len()).is_err());
    assert!(decode_have(&bytes, body.len() - 1).is_err());
    let mut old = bytes;
    old[1] = 1;
    assert!(decode_have(&old, body.len()).is_err());
}
#[test]
fn missing_index_lists_are_bounded_unique_and_not_ack_records() {
    let (body, _, auth) = fixture();
    let mut want = Want::single(auth.reference, 0);
    assert!(want.add(8));
    assert!(!want.add(8));
    let bytes = encode_want(want, body.len()).unwrap();
    assert_eq!(decode_want(&bytes, body.len()).unwrap(), want);
    assert_eq!(bytes.len(), want_size(2));
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
    bad.count = MAX_WANT as u8 + 1;
    assert!(encode_want(bad, body.len()).is_err());
    let mut count = bytes.clone();
    count[2 + REFERENCE_BYTES] = 255;
    assert!(decode_want(&count, body.len()).is_err());
    let mut obsolete_ack = bytes.clone();
    obsolete_ack[0] = 0xBA;
    assert!(decode_want(&obsolete_ack, body.len()).is_err());
    let mut extra = bytes;
    extra.push(0);
    assert!(decode_want(&extra, body.len()).is_err());
}
#[test]
fn index_is_first_field_and_every_independent_leaf_has_exact_auth_and_path() {
    let (body, tree, auth) = fixture();
    for index in [8, 2, 0, 7] {
        let data = &body[tree.layout().leaf_range(index).unwrap()];
        let proof = tree.proof(index).unwrap();
        let bytes = encode_leaf(auth, index as u32, data, &proof, body.len()).unwrap();
        assert_eq!(&bytes[2..6], &(index as u32).to_le_bytes());
        assert_eq!(
            bytes.len(),
            leaf_size(auth.reference, index as u32, body.len()).unwrap()
        );
        let leaf = decode_leaf(&bytes, body.len()).unwrap();
        assert_eq!(leaf.auth, auth);
        assert_eq!(leaf.index, index as u32);
        assert_eq!(leaf.data, data);
        assert_eq!(
            leaf.data.as_ptr(),
            bytes[LEAF_BYTES + proof.len() * 32..].as_ptr()
        );
        let (hashes, count) = leaf.proof_array();
        crate::d28::merkle::verify_leaf(
            &auth.reference.manifest,
            index,
            leaf.data,
            &hashes[..count],
            body.len(),
        )
        .unwrap();
        for end in 0..bytes.len() {
            assert!(decode_leaf(&bytes[..end], body.len()).is_err());
        }
        if index != 8 {
            let mut tamper = bytes.clone();
            tamper[2..6].copy_from_slice(&1u32.to_le_bytes());
            let leaf = decode_leaf(&tamper, body.len()).unwrap();
            assert!(
                crate::d28::merkle::verify_leaf(
                    &auth.reference.manifest,
                    1,
                    leaf.data,
                    &hashes[..count],
                    body.len()
                )
                .is_err()
            );
        }
        let mut extra = bytes;
        extra.push(0);
        assert!(decode_leaf(&extra, body.len()).is_err());
        assert!(
            encode_leaf(
                auth,
                index as u32,
                data,
                &proof[..proof.len() - 1],
                body.len()
            )
            .is_err()
        );
        assert!(
            encode_leaf(
                auth,
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
fn geometry_and_optional_header_fields_are_canonical() {
    for size in [32768, 102400, 262144, 1048576] {
        assert!(max_frame_bytes(10 << 20, size).is_ok());
    }
    for size in [0, 32767, 1048577, usize::MAX] {
        assert!(max_frame_bytes(10 << 20, size).is_err());
    }
    assert!(max_frame_bytes(usize::MAX, 32768).is_err());
    let (body, tree, mut auth) = fixture();
    auth.header.content.valid_round = None;
    let mut bytes = encode_leaf(
        auth,
        0,
        &body[tree.layout().leaf_range(0).unwrap()],
        &tree.proof(0).unwrap(),
        body.len(),
    )
    .unwrap();
    bytes[6 + 16 + 1] = 1; // absent optional round must encode zero
    assert!(decode_leaf(&bytes, body.len()).is_err());
}
