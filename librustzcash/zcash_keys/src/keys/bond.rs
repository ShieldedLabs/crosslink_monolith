/// An account's bond authority: the secret every one of its delegation bond keys descends
/// from.
///
/// Derived from the seed and account index rather than from a `UnifiedSpendingKey`, whose
/// every component is pool-specific and so would be the wrong root once a new pool can fund a
/// bond.
///
/// `Debug` is redacted because staking actions are logged with `{:?}` nearby. Otherwise plain
/// bytes with no wipe on drop, like `orchard::keys::SpendingKey`: in this stack the layer that
/// holds a secret owns destroying it, which is why seeds reach `zcash_keys` and
/// `zcash_client_backend` as `SecretVec` while the derivation functions just borrow `&[u8]`.
#[derive(Clone)]
#[repr(transparent)]
pub struct BondSpendingKey([u8; 32]);

impl core::fmt::Debug for BondSpendingKey {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("BondSpendingKey(<redacted>)")
    }
}



impl BondSpendingKey {
    /// Which derivation turns a seed into a [`BondSpendingKey`].
    ///
    /// Both are domain-separated KDFs over the same seed and neither is stronger here; the choice
    /// is one of ecosystem convention. They produce different keys, so this has to be settled
    /// before a network carries real bonds: changing it afterwards is fund loss, not a version
    /// bump.
    ///
    /// A constant rather than a Cargo feature deliberately. Features unify additively across the
    /// whole graph, so `--all-features`, or the docs.rs build which sets `all-features = true`,
    /// would silently flip the scheme. A constant also keeps both arms compiled, so the unselected
    /// one cannot rot.
    pub const DEV_KEY_IS_BLAKE3_INSTEAD_OF_ZIP32: bool = true; // TODO(@prod): decide
    pub const BOND_ROOT_BLAKE3_CONTEXT: &str = "Crosslink delegation bond root v1";
    pub const BOND_KEY_DERIVATION_CONTEXT: &str = "Crosslink delegation bond key v1";
    /// Applied by both schemes, so an input valid under one is valid under the other.
    /// `ChildIndex::hardened` panics above the account bound.
    const BOND_ROOT_SEED_MIN: usize = 32;
    const BOND_ROOT_SEED_MAX: usize = 252;
    const BOND_ROOT_ACCOUNT_MAX: u32 = 1 << 31;


    /// TODO(@Prod): placeholder, not a real allocation. Must be set before any network carries
    /// real bonds; changing it moves every bond key.
    pub const CROSSLINK_ZIP_NUMBER: u16 = 0;

    const ZIP32_CONTEXT: &'static [u8] = Self::BOND_ROOT_BLAKE3_CONTEXT.as_bytes();

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    fn inputs_ok(seed: &[u8], account_id: u32) -> bool {
        seed.len() >= Self::BOND_ROOT_SEED_MIN
            && seed.len() <= Self::BOND_ROOT_SEED_MAX
            && account_id < Self::BOND_ROOT_ACCOUNT_MAX
    }

    // Material layout for `derive_signing_key`, as cumulative offsets so the slices and the
    // total come from one chain and cannot drift apart.
    const ROOT_O: usize = 0;
    const TARGET_O: usize = Self::ROOT_O + size_of::<Self>();
    const AMOUNT_O: usize = Self::TARGET_O + size_of::<[u8; 32]>();
    const SALT_O: usize = Self::AMOUNT_O + size_of::<u64>();
    const BOND_KEY_MATERIAL_LEN: usize = Self::SALT_O + size_of::<[u8; 32]>();

    /// A bond's permanent identity, derived once at creation. `target_finalizer` is therefore
    /// the value in the original create action, not wherever a later retarget moved the bond.
    ///
    /// `bond_salt` separates two bonds identical in every other input, which would otherwise
    /// collide on one key and be rejected as a duplicate. Carrying it on the wire is what lets
    /// recovery need nothing wallet-local: read the salt, target and amount off the create
    /// action and derive.
    pub fn derive_signing_key(
        &self,
        target_finalizer: &[u8; 32],
        amount_zats: u64,
        bond_salt: &[u8; 32],
    ) -> ed25519_zebra::SigningKey {
        let mut material = [0u8; Self::BOND_KEY_MATERIAL_LEN];
        material[Self::ROOT_O..Self::TARGET_O].copy_from_slice(&self.0);
        material[Self::TARGET_O..Self::AMOUNT_O].copy_from_slice(target_finalizer);
        material[Self::AMOUNT_O..Self::SALT_O].copy_from_slice(&amount_zats.to_le_bytes());
        material[Self::SALT_O..].copy_from_slice(bond_salt);

        let seed = blake3::derive_key(Self::BOND_KEY_DERIVATION_CONTEXT, &material);
        ed25519_zebra::SigningKey::from(seed)
    }

    /// The signing key for a bond this account created, from the create action's terms and
    /// the `unique_pubkey` it published; `None` if that key is not ours. This is the recovery
    /// test, and it needs nothing wallet-local beyond the account key.
    pub fn recover_signing_key(
        &self,
        target_finalizer: &[u8; 32],
        amount_zats: u64,
        bond_salt: &[u8; 32],
        unique_pubkey: [u8; 32],
    ) -> Option<ed25519_zebra::SigningKey> {
        let signing_key = self.derive_signing_key(target_finalizer, amount_zats, bond_salt);
        let pub_key = ed25519_zebra::VerificationKeyBytes::from(&signing_key);

        if <[u8; 32]>::from(pub_key) != unique_pubkey {
            return None;
        }

        Some(signing_key)
    }

    /// The bond's `unique_pubkey`.
    pub fn derive_pub_key(
        &self,
        target_finalizer: &[u8; 32],
        amount_zats: u64,
        bond_salt: &[u8; 32],
    ) -> ed25519_zebra::VerificationKeyBytes {
        let signing_key = self.derive_signing_key(target_finalizer, amount_zats, bond_salt);
        ed25519_zebra::VerificationKeyBytes::from(&signing_key)
    }

    pub fn from_seed(seed: &[u8], account_id: u32) -> Option<Self> {
        match Self::DEV_KEY_IS_BLAKE3_INSTEAD_OF_ZIP32 {
            true => Self::from_seed_blake3(seed, account_id),
            false => Self::from_seed_zip32(seed, account_id),
        }
    }

    pub fn from_seed_blake3(seed: &[u8], account_id: u32) -> Option<Self> {
        if !Self::inputs_ok(seed, account_id) {
            return None;
        }

        let mut hasher = blake3::Hasher::new_derive_key(Self::BOND_ROOT_BLAKE3_CONTEXT);
        hasher.update(seed);
        hasher.update(&account_id.to_le_bytes());
        Some(Self(*hasher.finalize().as_bytes()))
    }

    pub fn from_seed_zip32(seed: &[u8], account_id: u32) -> Option<Self> {
        if !Self::inputs_ok(seed, account_id) {
            return None;
        }

        let subpath = [zip32::registered::PathElement::new(
            zip32::ChildIndex::hardened(account_id),
            b"",
        )];
        let key = zip32::registered::SecretKey::from_subpath(
            Self::ZIP32_CONTEXT,
            seed,
            Self::CROSSLINK_ZIP_NUMBER,
            &subpath,
        );

        match key {
            Ok(key) => Some(Self(*key.data())),
            Err(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::BondSpendingKey;
    use ed25519_zebra::{VerificationKey, VerificationKeyBytes};

    const ROOT_A: [u8; 32] = [0xa1; 32];
    const ROOT_B: [u8; 32] = [0xb2; 32];
    const FINALIZER: [u8; 32] = [0xf0; 32];
    const SALT: [u8; 32] = [0x5a; 32];
    const SEED: [u8; 32] = [0x7c; 32];

    fn sk(bytes: [u8; 32]) -> BondSpendingKey {
        BondSpendingKey::from_bytes(bytes)
    }

    fn pub_key(root: &BondSpendingKey, salt: &[u8; 32]) -> [u8; 32] {
        <[u8; 32]>::from(root.derive_pub_key(&FINALIZER, 100_000, salt))
    }

    /// The published bond key is a real verification key over a usable secret.
    #[test]
    fn pub_key_matches_its_signing_key() {
        let root = sk(ROOT_A);
        let signing_key = root.derive_signing_key(&FINALIZER, 100_000, &SALT);
        let pk = root.derive_pub_key(&FINALIZER, 100_000, &SALT);

        assert_eq!(VerificationKeyBytes::from(&signing_key), pk);

        let sig = signing_key.sign(b"bond key liveness");
        let vk = VerificationKey::try_from(pk).expect("derived key is valid");
        assert!(vk.verify(&sig, b"bond key liveness").is_ok());
    }

    /// Varying the salt alone is what lets one account bond the same amount to the same
    /// finalizer twice without colliding on a key consensus rejects as a duplicate.
    #[test]
    fn every_input_separates_keys() {
        let base = pub_key(&sk(ROOT_A), &SALT);

        assert_ne!(base, pub_key(&sk(ROOT_B), &SALT));
        assert_ne!(base, <[u8; 32]>::from(sk(ROOT_A).derive_pub_key(&[0xf1; 32], 100_000, &SALT)));
        assert_ne!(base, <[u8; 32]>::from(sk(ROOT_A).derive_pub_key(&FINALIZER, 100_001, &SALT)));
        assert_ne!(base, pub_key(&sk(ROOT_A), &[0x5b; 32]));
    }

    /// Recovery needs nothing but the create terms as they appear on chain.
    #[test]
    fn recovers_own_bond_from_its_terms_alone() {
        let published = pub_key(&sk(ROOT_A), &SALT);

        let recovered = sk(ROOT_A)
            .recover_signing_key(&FINALIZER, 100_000, &SALT, published)
            .expect("our own bond");
        assert_eq!(<[u8; 32]>::from(VerificationKeyBytes::from(&recovered)), published);

        assert!(sk(ROOT_B).recover_signing_key(&FINALIZER, 100_000, &SALT, published).is_none());
        // a scrambled salt no longer derives to the published key, so it must stay in the txid
        assert!(sk(ROOT_A).recover_signing_key(&FINALIZER, 100_000, &[0x5b; 32], published).is_none());
    }

    /// Known answers. Every other test here is relational, so transposing the material
    /// slices, or switching the account index to big-endian, would leave them all green while
    /// silently moving every bond key that has ever been issued.
    #[test]
    fn derivation_matches_known_answers() {
        let root = BondSpendingKey::from_seed_blake3(&SEED, 0).expect("in range");
        assert_eq!(*root.as_bytes(), [
            0xd4, 0x3c, 0x33, 0x38, 0xe7, 0x77, 0xe6, 0x48,
            0x6c, 0x72, 0x24, 0xb3, 0x57, 0xb7, 0xcb, 0xe2,
            0x6a, 0xc4, 0x4e, 0x4d, 0x7b, 0x61, 0x7d, 0x63,
            0xa5, 0x29, 0x19, 0x91, 0x18, 0x4e, 0x85, 0xa7,
        ]);

        assert_eq!(pub_key(&sk(ROOT_A), &SALT), [
            0xb7, 0x6a, 0x07, 0x37, 0x71, 0xda, 0x48, 0x3f,
            0x97, 0x33, 0x84, 0xd6, 0xb9, 0x21, 0x37, 0x76,
            0x48, 0xa0, 0x3f, 0x0b, 0x94, 0xae, 0xb9, 0x29,
            0x0b, 0xa0, 0x3f, 0xea, 0xbf, 0x47, 0xda, 0x8b,
        ]);
    }

    /// A bond made under one account must not be recoverable under another.
    #[test]
    fn roots_are_account_scoped() {
        assert_ne!(
            *BondSpendingKey::from_seed(&SEED, 0).expect("in range").as_bytes(),
            *BondSpendingKey::from_seed(&SEED, 1).expect("in range").as_bytes(),
        );
    }

    /// Out-of-range inputs return `None` rather than panicking in `ChildIndex::hardened`.
    /// The upper seed bound exists only so both schemes accept the same inputs.
    #[test]
    fn out_of_range_inputs_are_rejected() {
        assert!(BondSpendingKey::from_seed(&[0x7c; 31], 0).is_none());
        assert!(BondSpendingKey::from_seed(&[0x7c; 253], 0).is_none());
        assert!(BondSpendingKey::from_seed(&SEED, 1 << 31).is_none());
        assert!(BondSpendingKey::from_seed(&SEED, (1 << 31) - 1).is_some());
    }

    /// The schemes disagree, which is why the choice has to be settled before launch:
    /// flipping it afterwards orphans every existing bond key.
    #[test]
    fn root_schemes_disagree() {
        assert_ne!(
            *BondSpendingKey::from_seed_blake3(&SEED, 0).expect("in range").as_bytes(),
            *BondSpendingKey::from_seed_zip32(&SEED, 0).expect("in range").as_bytes(),
        );
    }
}
