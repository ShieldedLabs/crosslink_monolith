//! Independent transport primitives; the runtime/consensus boundary is unchanged.
use tenderlink::d28::merkle;

use ed25519_zebra::{SigningKey, VerificationKeyBytes};
use merkle::{
    Bitmap, DEFAULT_LEAF_BYTES, Error, Hash, HolderAttestation, HolderContext, MAX_LEAF_BYTES,
    MIN_LEAF_BYTES, Manifest, PartialTree, Tree, verify_leaf,
};
use rand::{Rng, RngCore, SeedableRng};
use rand_chacha::ChaCha8Rng;
use zcash_primitives::bft::{BftBlock, FatPointerToBftBlock, HashKeys};

const MAX_BODY: usize = 10 * 1024 * 1024;

fn bytes(len: usize) -> Vec<u8> {
    (0..len).map(|i| ((i / 251 + i * 17) % 256) as u8).collect()
}

fn fixture(len: usize, leaf: usize) -> (Vec<u8>, Tree, Manifest) {
    let body = bytes(len);
    let tree = Tree::new(&body, leaf, MAX_BODY).unwrap();
    let manifest = tree.manifest([1; 32], [2; 32]);
    (body, tree, manifest)
}

#[test]
fn ten_mib_default_geometry_and_all_proofs() {
    let (body, tree, manifest) = fixture(MAX_BODY, DEFAULT_LEAF_BYTES);
    let layout = manifest.validate(MAX_BODY).unwrap();
    assert_eq!(layout, tree.layout());
    assert_eq!(layout.body_len(), MAX_BODY);
    assert_eq!(layout.leaf_bytes(), 102_400);
    assert_eq!(layout.leaf_count(), 103);
    assert_eq!(tree.leaf_count(), 103);
    assert_eq!(layout.padded_leaf_count(), 128);
    assert_eq!(layout.bitmap_bytes(), 13);
    assert_eq!(layout.proof_depth(), 7);
    assert_eq!(layout.proof_bytes(), 224);
    assert_eq!(layout.tree_hash_count(), 255);
    assert_eq!(layout.tree_hash_bytes(), 8_160);
    assert_eq!(layout.known_node_bitmap_bytes(), 32);
    assert_eq!(layout.partial_metadata_bytes(), 8_205);
    assert_eq!(layout.leaf_range(102).unwrap().len(), 40_960);
    for index in 0..tree.leaf_count() {
        let proof = tree.proof(index).unwrap();
        assert_eq!(proof.len(), 7);
        verify_leaf(
            &manifest,
            index,
            &body[layout.leaf_range(index).unwrap()],
            &proof,
            MAX_BODY,
        )
        .unwrap();
    }
}

#[test]
fn four_profile_leaf_sizes_have_exact_metadata_bounds() {
    // leaf bytes, leaves, padded leaves, depth, have bytes, tree hashes,
    // known-node bitmap bytes, partial hash/bitmap payload bytes.
    for (leaf, count, padded, depth, have, hashes, known, partial) in [
        (32_768, 320, 512, 9, 40, 1023, 128, 32_904),
        (102_400, 103, 128, 7, 13, 255, 32, 8_205),
        (262_144, 40, 64, 6, 5, 127, 16, 4_085),
        (1_048_576, 10, 16, 4, 2, 31, 4, 998),
    ] {
        let manifest = Manifest {
            body_id: [1; 32],
            signed_header_identity: [2; 32],
            body_len: MAX_BODY as u64,
            leaf_bytes: leaf,
            root: [3; 32],
        };
        let layout = manifest.validate(MAX_BODY).unwrap();
        assert_eq!(layout.leaf_count(), count);
        assert_eq!(layout.padded_leaf_count(), padded);
        assert_eq!(layout.proof_depth(), depth);
        assert_eq!(layout.proof_bytes(), depth * 32);
        assert_eq!(layout.bitmap_bytes(), have);
        assert_eq!(layout.tree_hash_count(), hashes);
        assert_eq!(layout.tree_hash_bytes(), hashes * 32);
        assert_eq!(layout.known_node_bitmap_bytes(), known);
        assert_eq!(layout.partial_metadata_bytes(), partial);
    }
}

#[test]
fn exact_boundaries_and_non_power_of_two_leaf_sizes() {
    for leaf in [
        MIN_LEAF_BYTES,
        MIN_LEAF_BYTES + 1,
        DEFAULT_LEAF_BYTES,
        MAX_LEAF_BYTES,
    ] {
        for len in [1, leaf - 1, leaf, leaf + 1, 2 * leaf, 2 * leaf + 19] {
            let (body, tree, manifest) = fixture(len, leaf);
            assert_eq!(tree.leaf_count(), len.div_ceil(leaf));
            for index in 0..tree.leaf_count() {
                let range = tree.layout().leaf_range(index).unwrap();
                assert_eq!(range.start, index * leaf);
                assert_eq!(range.end, len.min((index + 1) * leaf));
                let proof = tree.proof(index).unwrap();
                verify_leaf(&manifest, index, &body[range], &proof, len).unwrap();
            }
            if len <= leaf {
                assert!(tree.proof(0).unwrap().is_empty());
                assert_eq!(tree.layout().tree_hash_count(), 1);
            }
        }
    }
}

#[test]
fn invalid_sizes_and_extreme_indexes_fail_before_allocation() {
    let (_, tree, good) = fixture(33, MIN_LEAF_BYTES);
    assert_eq!(
        Tree::new(&[], MIN_LEAF_BYTES, MAX_BODY).unwrap_err(),
        Error::BodyLength
    );
    assert_eq!(
        Tree::new(&[1], MIN_LEAF_BYTES, 0).unwrap_err(),
        Error::Bounds
    );
    assert_eq!(
        Tree::new(&[1], MIN_LEAF_BYTES, usize::MAX).unwrap_err(),
        Error::Bounds
    );
    assert_eq!(
        Tree::new(&[1, 2], MIN_LEAF_BYTES, 1).unwrap_err(),
        Error::BodyLength
    );
    for leaf in [0, MIN_LEAF_BYTES - 1, MAX_LEAF_BYTES + 1, usize::MAX] {
        assert_eq!(
            Tree::new(&[1], leaf, MAX_BODY).unwrap_err(),
            Error::LeafSize
        );
    }
    for body_len in [0, MAX_BODY as u64 + 1, u64::MAX] {
        let manifest = Manifest { body_len, ..good };
        assert_eq!(manifest.validate(MAX_BODY), Err(Error::BodyLength));
        assert_eq!(
            Bitmap::new(&manifest, MAX_BODY).unwrap_err(),
            Error::BodyLength
        );
        assert_eq!(
            PartialTree::new(&manifest, MAX_BODY).unwrap_err(),
            Error::BodyLength
        );
        assert_eq!(
            verify_leaf(&manifest, 0, &[], &[], MAX_BODY),
            Err(Error::BodyLength)
        );
    }
    for leaf_bytes in [
        0,
        (MIN_LEAF_BYTES - 1) as u32,
        (MAX_LEAF_BYTES + 1) as u32,
        u32::MAX,
    ] {
        assert_eq!(
            Manifest { leaf_bytes, ..good }.validate(MAX_BODY),
            Err(Error::LeafSize)
        );
    }
    for index in [1, usize::MAX] {
        assert_eq!(tree.proof(index), Err(Error::Index));
        assert_eq!(tree.layout().leaf_range(index), Err(Error::Index));
        assert_eq!(
            verify_leaf(&good, index, &[0; 33], &[], MAX_BODY),
            Err(Error::Index)
        );
    }
    assert_eq!(good.validate(32), Err(Error::BodyLength));
    assert_eq!(good.validate(0), Err(Error::Bounds));
}

#[test]
fn wrong_root_index_data_length_and_geometry_are_rejected() {
    let (body, tree, manifest) = fixture(3 * MIN_LEAF_BYTES + 19, MIN_LEAF_BYTES);
    let range = tree.layout().leaf_range(1).unwrap();
    let data = &body[range];
    let proof = tree.proof(1).unwrap();
    let mut wrong_root = manifest;
    wrong_root.root[0] ^= 1;
    assert_eq!(
        verify_leaf(&wrong_root, 1, data, &proof, MAX_BODY),
        Err(Error::Root)
    );
    assert_eq!(
        verify_leaf(&manifest, 0, data, &proof, MAX_BODY),
        Err(Error::Root)
    );
    let mut changed = data.to_vec();
    changed[17] ^= 1;
    assert_eq!(
        verify_leaf(&manifest, 1, &changed, &proof, MAX_BODY),
        Err(Error::Root)
    );
    assert_eq!(
        verify_leaf(&manifest, 1, &data[..data.len() - 1], &proof, MAX_BODY),
        Err(Error::LeafLength)
    );
    changed.push(0);
    assert_eq!(
        verify_leaf(&manifest, 1, &changed, &proof, MAX_BODY),
        Err(Error::LeafLength)
    );
    let mut changed_geometry = manifest;
    changed_geometry.body_len += 1;
    assert_eq!(
        verify_leaf(&changed_geometry, 1, data, &proof, MAX_BODY),
        Err(Error::Root)
    );
    changed_geometry = manifest;
    changed_geometry.leaf_bytes += 1;
    assert_eq!(
        verify_leaf(&changed_geometry, 1, data, &proof, MAX_BODY),
        Err(Error::LeafLength)
    );
    let final_proof = tree.proof(3).unwrap();
    let final_data = &body[tree.layout().leaf_range(3).unwrap()];
    verify_leaf(&manifest, 3, final_data, &final_proof, MAX_BODY).unwrap();
    let mut zero_padded = final_data.to_vec();
    zero_padded.resize(MIN_LEAF_BYTES, 0);
    assert_eq!(
        verify_leaf(&manifest, 3, &zero_padded, &final_proof, MAX_BODY),
        Err(Error::LeafLength)
    );
}

#[test]
fn short_long_reordered_duplicate_and_corrupt_siblings_are_rejected() {
    let (body, tree, manifest) = fixture(5 * MIN_LEAF_BYTES - 7, MIN_LEAF_BYTES);
    let data = &body[tree.layout().leaf_range(2).unwrap()];
    let proof = tree.proof(2).unwrap();
    assert_eq!(proof.len(), 3);
    assert_eq!(
        verify_leaf(&manifest, 2, data, &proof[..2], MAX_BODY),
        Err(Error::ProofLength)
    );
    let mut wrong = proof.clone();
    wrong.push([0; 32]);
    assert_eq!(
        verify_leaf(&manifest, 2, data, &wrong, MAX_BODY),
        Err(Error::ProofLength)
    );
    wrong = proof.clone();
    wrong.reverse();
    assert_eq!(
        verify_leaf(&manifest, 2, data, &wrong, MAX_BODY),
        Err(Error::Root)
    );
    wrong = proof.clone();
    wrong[1] = wrong[0];
    assert_eq!(
        verify_leaf(&manifest, 2, data, &wrong, MAX_BODY),
        Err(Error::Root)
    );
    for index in 0..proof.len() {
        wrong = proof.clone();
        wrong[index][0] ^= 1;
        assert_eq!(
            verify_leaf(&manifest, 2, data, &wrong, MAX_BODY),
            Err(Error::Root)
        );
    }
}

#[test]
fn bitmap_exact_length_zero_padding_and_bounded_operations() {
    let (_, _, manifest) = fixture(8 * MIN_LEAF_BYTES + 1, MIN_LEAF_BYTES);
    let mut bitmap = Bitmap::new(&manifest, MAX_BODY).unwrap();
    assert_eq!(bitmap.manifest(), &manifest);
    assert_eq!(bitmap.leaf_count(), 9);
    assert_eq!(bitmap.count(), 0);
    assert_eq!(bitmap.as_bytes(), &[0, 0]);
    assert!(!bitmap.complete());
    assert_eq!(
        bitmap.missing().collect::<Vec<_>>(),
        (0..9).collect::<Vec<_>>()
    );
    for bad in [vec![], vec![0], vec![0, 0, 0]] {
        assert_eq!(
            Bitmap::from_bytes(&manifest, &bad, MAX_BODY).unwrap_err(),
            Error::BitmapLength
        );
    }
    for bad_last in [2, 3, 0x80, 0xff] {
        assert_eq!(
            Bitmap::from_bytes(&manifest, &[0, bad_last], MAX_BODY).unwrap_err(),
            Error::BitmapPadding
        );
    }
    for index in [9, usize::MAX] {
        assert_eq!(bitmap.have(index), Err(Error::Index));
        assert_eq!(bitmap.set(index), Err(Error::Index));
    }
    for index in (0..9).rev() {
        assert_eq!(bitmap.have(index), Ok(false));
        bitmap.set(index).unwrap();
        assert_eq!(bitmap.have(index), Ok(true));
        let snapshot = bitmap.clone();
        assert_eq!(bitmap.set(index), Err(Error::Duplicate));
        assert_eq!(bitmap, snapshot);
    }
    assert_eq!(bitmap.as_bytes(), &[0xff, 1]);
    assert_eq!(bitmap.count(), 9);
    assert!(bitmap.complete());
    assert_eq!(bitmap.missing().next(), None);
    assert_eq!(
        Bitmap::from_bytes(&manifest, bitmap.as_bytes(), MAX_BODY).unwrap(),
        bitmap
    );
    // Exactly byte-aligned inventory has no padding bits to reject.
    let aligned = Manifest {
        body_len: (8 * MIN_LEAF_BYTES) as u64,
        ..manifest
    };
    assert!(
        Bitmap::from_bytes(&aligned, &[0xff], MAX_BODY)
            .unwrap()
            .complete()
    );
}

#[test]
fn verified_inventory_rejects_cross_manifest_and_failed_leaf_without_mutation() {
    let (body, tree, manifest) = fixture(2 * MIN_LEAF_BYTES + 1, MIN_LEAF_BYTES);
    let data = &body[tree.layout().leaf_range(1).unwrap()];
    let proof = tree.proof(1).unwrap();
    let mut bitmap = Bitmap::new(&manifest, MAX_BODY).unwrap();
    for field in 0..5 {
        let mut other = manifest;
        match field {
            0 => other.body_id[0] ^= 1,
            1 => other.signed_header_identity[0] ^= 1,
            2 => other.root[0] ^= 1,
            3 => other.body_len += 1,
            _ => other.leaf_bytes += 1,
        }
        assert_eq!(
            bitmap.verify_and_set(&other, 1, data, &proof),
            Err(Error::ManifestMismatch)
        );
        assert_eq!(bitmap.count(), 0);
    }
    assert_eq!(
        bitmap.verify_and_set(&manifest, 1, data, &[]),
        Err(Error::ProofLength)
    );
    assert_eq!(bitmap.count(), 0);
    bitmap.verify_and_set(&manifest, 1, data, &proof).unwrap();
    assert_eq!(bitmap.count(), 1);
    assert!(!bitmap.complete());
    assert_eq!(
        bitmap.verify_and_set(&manifest, 1, data, &proof),
        Err(Error::Duplicate)
    );
    assert_eq!(bitmap.count(), 1);
}

#[test]
fn partial_receivers_relay_before_full_body_without_per_leaf_proof_ownership() {
    let (body, tree, manifest) = fixture(MAX_BODY, DEFAULT_LEAF_BYTES);
    let mut receiver = PartialTree::new(&manifest, MAX_BODY).unwrap();
    let mut downstream = PartialTree::new(&manifest, MAX_BODY).unwrap();
    assert_eq!(receiver.manifest(), &manifest);
    assert_eq!(receiver.layout().partial_metadata_bytes(), 8_205);
    assert_eq!(receiver.known_node_count(), 1);
    assert_eq!(receiver.proof(102), Err(Error::Unavailable));
    assert_eq!(receiver.proof(103), Err(Error::Index));
    // Receive the partial last leaf first, and immediately relay its exact proof.
    let index = 102;
    let data = &body[tree.layout().leaf_range(index).unwrap()];
    receiver
        .insert_verified(index, data, &tree.proof(index).unwrap())
        .unwrap();
    assert_eq!(receiver.bitmap().count(), 1);
    assert!(!receiver.bitmap().complete());
    assert_eq!(receiver.known_node_count(), 15);
    let relayed = receiver.proof(index).unwrap();
    assert_eq!(relayed, tree.proof(index).unwrap());
    downstream.insert_verified(index, data, &relayed).unwrap();
    assert_eq!(downstream.bitmap().count(), 1);
    assert_eq!(downstream.proof(index).unwrap(), relayed);
    assert_eq!(receiver.proof(101), Err(Error::Unavailable));
    // Body bytes live outside both PartialTrees. Fill in reverse order to ensure
    // arrival order is independent of canonical leaf and sibling order.
    for index in (0..102).rev() {
        let data = &body[tree.layout().leaf_range(index).unwrap()];
        receiver
            .insert_verified(index, data, &tree.proof(index).unwrap())
            .unwrap();
        assert_eq!(receiver.proof(index).unwrap(), tree.proof(index).unwrap());
    }
    assert!(receiver.bitmap().complete());
    assert_eq!(receiver.bitmap().count(), 103);
    assert!(receiver.known_node_count() <= 255);
    for index in 0..103 {
        assert_eq!(receiver.proof(index).unwrap(), tree.proof(index).unwrap());
    }
    assert!(!downstream.bitmap().complete());
}

#[test]
fn partial_cache_failures_are_atomic_and_known_siblings_are_not_available_data() {
    let (body, tree, manifest) = fixture(5 * MIN_LEAF_BYTES - 1, MIN_LEAF_BYTES);
    let mut partial = PartialTree::new(&manifest, MAX_BODY).unwrap();
    let data = &body[tree.layout().leaf_range(0).unwrap()];
    let proof = tree.proof(0).unwrap();
    let mut bad = proof.clone();
    bad[0][0] ^= 1;
    assert_eq!(partial.insert_verified(0, data, &bad), Err(Error::Root));
    assert_eq!(partial.bitmap().count(), 0);
    assert_eq!(partial.known_node_count(), 1);
    assert_eq!(
        partial.insert_verified(0, &[], &proof),
        Err(Error::LeafLength)
    );
    assert_eq!(
        partial.insert_verified(0, data, &[]),
        Err(Error::ProofLength)
    );
    assert_eq!(
        partial.insert_verified(usize::MAX, data, &proof),
        Err(Error::Index)
    );
    assert_eq!(partial.known_node_count(), 1);
    partial.insert_verified(0, data, &proof).unwrap();
    assert_eq!(partial.known_node_count(), 7);
    assert_eq!(partial.proof(1), Err(Error::Unavailable));
    assert_eq!(partial.bitmap().have(1), Ok(false));
    assert_eq!(
        partial.insert_verified(0, data, &proof),
        Err(Error::Duplicate)
    );
    assert_eq!(partial.known_node_count(), 7);
    let sibling = &body[tree.layout().leaf_range(1).unwrap()];
    let mut bad_sibling = sibling.to_vec();
    bad_sibling[0] ^= 1;
    assert_eq!(
        partial.insert_verified(1, &bad_sibling, &tree.proof(1).unwrap()),
        Err(Error::Root)
    );
    assert_eq!(partial.known_node_count(), 7);
    assert_eq!(partial.bitmap().count(), 1);
    assert_eq!(partial.proof(0).unwrap(), proof);
}

#[test]
fn single_leaf_partial_tree_has_zero_length_proof_and_retains_no_body_copy() {
    let (body, tree, manifest) = fixture(19, DEFAULT_LEAF_BYTES);
    let mut partial = PartialTree::new(&manifest, MAX_BODY).unwrap();
    assert_eq!(partial.layout().partial_metadata_bytes(), 34);
    assert_eq!(partial.proof(0), Err(Error::Unavailable));
    assert_eq!(partial.insert_verified(0, &[0; 19], &[]), Err(Error::Root));
    partial.insert_verified(0, &body, &[]).unwrap();
    assert!(partial.bitmap().complete());
    drop(body);
    drop(tree);
    assert!(partial.proof(0).unwrap().is_empty());
    assert_eq!(partial.known_node_count(), 1);
}

#[test]
fn deterministic_randomized_proofs_and_partial_cache_consistency() {
    let mut rng = ChaCha8Rng::seed_from_u64(0xd28_0428);
    for _ in 0..96 {
        let leaf = rng.random_range(MIN_LEAF_BYTES..=MIN_LEAF_BYTES * 3);
        let len = rng.random_range(1..=leaf * 19);
        let mut body = vec![0; len];
        rng.fill_bytes(&mut body);
        let tree = Tree::new(&body, leaf, MAX_BODY).unwrap();
        let manifest = tree.manifest([7; 32], [8; 32]);
        let mut partial = PartialTree::new(&manifest, MAX_BODY).unwrap();
        let start = rng.random_range(0..tree.leaf_count());
        for offset in 0..tree.leaf_count() {
            let index = (start + offset) % tree.leaf_count();
            let data = &body[tree.layout().leaf_range(index).unwrap()];
            let proof = tree.proof(index).unwrap();
            verify_leaf(&manifest, index, data, &proof, MAX_BODY).unwrap();
            partial.insert_verified(index, data, &proof).unwrap();
            assert_eq!(partial.proof(index).unwrap(), proof);
            let mut bad = data.to_vec();
            let changed = rng.random_range(0..bad.len());
            bad[changed] ^= 1;
            assert_eq!(
                verify_leaf(&manifest, index, &bad, &proof, MAX_BODY),
                Err(Error::Root)
            );
        }
        assert!(partial.bitmap().complete());
        assert_eq!(partial.bitmap().count(), tree.leaf_count());
    }
}

fn reference_hash(
    domain: &[u8],
    len: usize,
    leaf: usize,
    position: usize,
    suffix: &[&[u8]],
) -> Hash {
    // Separate straightforward concatenation pins the protocol's byte layout.
    let mut input = domain.to_vec();
    input.extend_from_slice(&(len as u64).to_le_bytes());
    input.extend_from_slice(&(leaf as u32).to_le_bytes());
    input.extend_from_slice(&(position as u64).to_le_bytes());
    for bytes in suffix {
        input.extend_from_slice(bytes);
    }
    *blake3::hash(&input).as_bytes()
}

#[test]
fn odd_leaf_tree_uses_canonical_padding_not_duplication() {
    let leaf = MIN_LEAF_BYTES;
    let (body, tree, manifest) = fixture(2 * leaf + 17, leaf);
    let mut leaves = Vec::new();
    for (index, data) in body.chunks(leaf).enumerate() {
        leaves.push(reference_hash(
            b"tenderlink/D28/merkle/leaf/v1",
            body.len(),
            leaf,
            index,
            &[&(data.len() as u64).to_le_bytes(), data],
        ));
    }
    let pad = reference_hash(
        b"tenderlink/D28/merkle/padding/v1",
        body.len(),
        leaf,
        3,
        &[],
    );
    let left = reference_hash(
        b"tenderlink/D28/merkle/node/v1",
        body.len(),
        leaf,
        1,
        &[&leaves[0], &leaves[1]],
    );
    let right = reference_hash(
        b"tenderlink/D28/merkle/node/v1",
        body.len(),
        leaf,
        2,
        &[&leaves[2], &pad],
    );
    let root = reference_hash(
        b"tenderlink/D28/merkle/node/v1",
        body.len(),
        leaf,
        0,
        &[&left, &right],
    );
    assert_eq!(tree.root(), root);
    assert_eq!(tree.proof(2).unwrap(), vec![pad, left]);
    assert_ne!(pad, leaves[2]);
    let mut duplicated_last_leaf = tree.proof(2).unwrap();
    duplicated_last_leaf[0] = leaves[2];
    assert_eq!(
        verify_leaf(
            &manifest,
            2,
            &body[2 * leaf..],
            &duplicated_last_leaf,
            MAX_BODY
        ),
        Err(Error::Root)
    );
}

#[test]
fn holder_attestation_binds_every_manifest_and_bft_context_field() {
    let (_, _, manifest) = fixture(2 * MIN_LEAF_BYTES + 1, MIN_LEAF_BYTES);
    let context = HolderContext {
        domain: [11; 32],
        config: [12; 32],
        height: 13,
    };
    let key = SigningKey::from([14; 32]);
    let public = VerificationKeyBytes::from(&key).into();
    let attestation = HolderAttestation::sign(&context, &manifest, &key, MAX_BODY).unwrap();
    attestation
        .verify(&context, &manifest, &public, MAX_BODY)
        .unwrap();
    for field in 0..5 {
        let mut other = manifest;
        match field {
            0 => other.body_id[0] ^= 1,
            1 => other.signed_header_identity[0] ^= 1,
            2 => other.root[0] ^= 1,
            3 => other.body_len += 1,
            _ => other.leaf_bytes += 1,
        }
        assert_eq!(
            attestation.verify(&context, &other, &public, MAX_BODY),
            Err(Error::Signature)
        );
    }
    for field in 0..3 {
        let mut other = context;
        match field {
            0 => other.domain[0] ^= 1,
            1 => other.config[0] ^= 1,
            _ => other.height += 1,
        }
        assert_eq!(
            attestation.verify(&other, &manifest, &public, MAX_BODY),
            Err(Error::Signature)
        );
    }
    let other_key = SigningKey::from([15; 32]);
    let other_public = VerificationKeyBytes::from(&other_key).into();
    assert_eq!(
        attestation.verify(&context, &manifest, &other_public, MAX_BODY),
        Err(Error::Holder)
    );
    let mut wrong_holder = attestation;
    wrong_holder.holder = other_public;
    assert_eq!(
        wrong_holder.verify(&context, &manifest, &other_public, MAX_BODY),
        Err(Error::Signature)
    );
    let mut bad_signature = attestation;
    bad_signature.signature[0] ^= 1;
    assert_eq!(
        bad_signature.verify(&context, &manifest, &public, MAX_BODY),
        Err(Error::Signature)
    );
    let unbounded = Manifest {
        body_len: u64::MAX,
        ..manifest
    };
    assert_eq!(
        HolderAttestation::sign(&context, &unbounded, &key, MAX_BODY),
        Err(Error::BodyLength)
    );
    assert_eq!(
        attestation.verify(&context, &unbounded, &public, MAX_BODY),
        Err(Error::BodyLength)
    );
}

#[test]
fn valid_root_and_holder_signature_do_not_replace_existing_keyed_chain_id() {
    let block = BftBlock {
        version: 1,
        height: 9,
        previous_block_fat_ptr: FatPointerToBftBlock::null(),
        headers: vec![],
        slashforks: vec![],
        do_not_include_until_bc_height: 0,
    };
    let body = block.zcash_serialize_to_vec().unwrap();
    let original_chain_id = HashKeys::default().value_id.hash(&body);
    assert_eq!(original_chain_id, block.blake3_hash().0);
    let tree = Tree::new(&body, DEFAULT_LEAF_BYTES, MAX_BODY).unwrap();
    let manifest = tree.manifest(original_chain_id, [23; 32]);
    let proof = tree.proof(0).unwrap();
    verify_leaf(&manifest, 0, &body, &proof, MAX_BODY).unwrap();
    assert_ne!(manifest.root, original_chain_id);
    assert_eq!(manifest.body_id, original_chain_id);
    let mut wrong_id = manifest;
    wrong_id.body_id[0] ^= 1;
    // Membership correctly remains valid under identical root/geometry. A
    // holder may sign this false relation; only final chain checks reject it.
    verify_leaf(&wrong_id, 0, &body, &proof, MAX_BODY).unwrap();
    let context = HolderContext {
        domain: [24; 32],
        config: [25; 32],
        height: 9,
    };
    let key = SigningKey::from([26; 32]);
    let public = VerificationKeyBytes::from(&key).into();
    HolderAttestation::sign(&context, &wrong_id, &key, MAX_BODY)
        .unwrap()
        .verify(&context, &wrong_id, &public, MAX_BODY)
        .unwrap();
    assert_ne!(wrong_id.body_id, HashKeys::default().value_id.hash(&body));
    assert_eq!(manifest.body_id, HashKeys::default().value_id.hash(&body));
}
