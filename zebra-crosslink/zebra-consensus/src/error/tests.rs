//! Tests for [`TransactionError`] conversion and mempool misbehaviour scoring.

use super::*;

/// Boxed shielded proof and signature verification errors must keep their concrete type when
/// converted back from a [`BoxError`], so the mempool can assign them a misbehaviour score instead
/// of collapsing them to [`TransactionError::InternalDowncastError`] (score 0). See
/// <https://github.com/ZcashFoundation/zebra/security/advisories/GHSA-2p4c-3q4q-p463>.
#[test]
fn boxed_signature_errors_are_preserved() {
    let ed25519_error: BoxError =
        Box::new(zebra_chain::primitives::ed25519::Error::InvalidSignature);
    assert_eq!(
        TransactionError::from(ed25519_error),
        TransactionError::Ed25519(zebra_chain::primitives::ed25519::Error::InvalidSignature)
    );

    let redjubjub_error: BoxError =
        Box::new(zebra_chain::primitives::redjubjub::Error::InvalidSignature);
    assert_eq!(
        TransactionError::from(redjubjub_error),
        TransactionError::RedJubjub(zebra_chain::primitives::redjubjub::Error::InvalidSignature)
    );

    let redpallas_error: BoxError =
        Box::new(zebra_chain::primitives::reddsa::Error::InvalidSignature);
    assert_eq!(
        TransactionError::from(redpallas_error),
        TransactionError::RedPallas(zebra_chain::primitives::reddsa::Error::InvalidSignature)
    );
}

/// Every shielded proof/signature verification failure, and non-canonical Orchard/Ironwood proof
/// sizes, must earn a ban-worthy mempool misbehaviour score, so a peer forcing expensive
/// verification with invalid proofs is disconnected rather than allowed to keep sending. See
/// <https://github.com/ZcashFoundation/zebra/security/advisories/GHSA-2p4c-3q4q-p463>.
#[test]
fn verification_errors_have_high_misbehavior_score() {
    for error in [
        TransactionError::SaplingVerificationFailed,
        TransactionError::Halo2VerificationFailed,
        TransactionError::OrchardProofSize,
        TransactionError::IronwoodProofSize,
        TransactionError::Ed25519(zebra_chain::primitives::ed25519::Error::InvalidSignature),
        TransactionError::RedJubjub(zebra_chain::primitives::redjubjub::Error::InvalidSignature),
        TransactionError::RedPallas(zebra_chain::primitives::reddsa::Error::InvalidSignature),
        TransactionError::StakingActionBondKeyInvalid { bond_key: [0; 32] },
        TransactionError::StakingActionSignatureInvalid { bond_key: [0; 32] },
    ] {
        assert_eq!(error.mempool_misbehavior_score(), 100, "{error:?}");
    }
}

/// Proof, signature, script and structural failures are verdicts no honest peer can earn, so
/// they are not state-dependent for a transaction that embeds its branch ID.
#[test]
fn crypto_and_structural_failures_are_state_independent() {
    for error in [
        TransactionError::Groth16("bad proof".to_string()),
        TransactionError::MalformedGroth16("bad encoding".to_string()),
        TransactionError::Halo2VerificationFailed,
        TransactionError::SaplingVerificationFailed,
        TransactionError::SmallOrder,
        TransactionError::OrchardProofSize,
        TransactionError::IronwoodProofSize,
        TransactionError::Ed25519(zebra_chain::primitives::ed25519::Error::InvalidSignature),
        TransactionError::RedJubjub(zebra_chain::primitives::redjubjub::Error::InvalidSignature),
        TransactionError::RedPallas(zebra_chain::primitives::reddsa::Error::InvalidSignature),
        TransactionError::Script(zebra_script::Error::ScriptInvalid),
        TransactionError::StakingActionBondKeyInvalid { bond_key: [0; 32] },
        TransactionError::StakingActionSignatureInvalid { bond_key: [0; 32] },
        TransactionError::StakingActionInvalidFinalizerAddress { pub_key: [0; 32] },
        TransactionError::StakingActionFinalizerAuthorizationInvalid { finalizer: [0; 32] },
        TransactionError::BadBalance,
        TransactionError::NoInputs,
        TransactionError::NoOutputs,
        TransactionError::WrongVersion,
    ] {
        assert!(!error.is_state_dependent(true), "{error:?}");
    }
}

/// A pre-v5 transaction's sighash and Halo2 circuit come from our upgrade schedule, so its
/// signature, script and Halo2 failures are state-dependent; its proofs and structure are not.
#[test]
fn sighash_failures_depend_on_schedule_without_embedded_branch_id() {
    for error in [
        TransactionError::SaplingVerificationFailed,
        TransactionError::Halo2VerificationFailed,
        TransactionError::Ed25519(zebra_chain::primitives::ed25519::Error::InvalidSignature),
        TransactionError::RedJubjub(zebra_chain::primitives::redjubjub::Error::InvalidSignature),
        TransactionError::RedPallas(zebra_chain::primitives::reddsa::Error::InvalidSignature),
        TransactionError::Script(zebra_script::Error::ScriptInvalid),
        TransactionError::StakingActionSignatureInvalid { bond_key: [0; 32] },
    ] {
        assert!(error.is_state_dependent(false), "{error:?}");
    }

    for error in [
        TransactionError::Groth16("bad proof".to_string()),
        TransactionError::SmallOrder,
        TransactionError::BadBalance,
        TransactionError::WrongVersion,
    ] {
        assert!(!error.is_state_dependent(false), "{error:?}");
    }
}

/// Failures that depend on our tip, clock, upgrade schedule, staking schedule, mempool, or an
/// internal error are state-dependent, even where `mempool_misbehavior_score` punishes them.
#[test]
fn contextual_failures_are_state_dependent() {
    let height = block::Height(1);
    let hash = zebra_chain::transaction::Hash([0; 32]);
    let outpoint = transparent::OutPoint::from_usize(hash, 0);
    for error in [
        TransactionError::LockedUntilAfterBlockHeight(height),
        TransactionError::ExpiredTransaction {
            expiry_height: height,
            block_height: height,
            transaction_hash: hash,
        },
        TransactionError::ImmatureTransparentCoinbaseSpend {
            outpoint,
            spend_height: height,
            min_spend_height: height,
            created_height: height,
        },
        TransactionError::WrongConsensusBranchId,
        TransactionError::UnsupportedByNetworkUpgrade(
            5,
            zebra_chain::parameters::NetworkUpgrade::Nu5,
        ),
        TransactionError::StakingActionOutsideWindow {
            block_height: 1,
            period: 2,
            window: 1,
        },
        TransactionError::StakingActionBondNotFound { bond_key: [0; 32] },
        TransactionError::TransparentInputNotFound,
        TransactionError::DuplicateTransparentSpend(outpoint),
        TransactionError::NonStandardInputs,
        TransactionError::InternalDowncastError("lost".to_string()),
        TransactionError::Script(zebra_script::Error::TxIndex),
    ] {
        assert!(error.is_state_dependent(true), "{error:?}");
    }
}
