//! Delegation bond serialization formats for finalized data.
//!
//! # Correctness
//!
//! [`crate::constants::state_database_format_version_in_code()`] must be incremented
//! each time the database format (column, serialization, etc) changes.

use zebra_chain::{
    amount::{Amount, NonNegative},
    serialization::{ZcashDeserializeInto, ZcashSerialize},
};

use crate::service::finalized_state::disk_format::{
    block::TransactionLocation, FromDisk, IntoDisk,
};

#[cfg(any(test, feature = "proptest-impl"))]
use proptest_derive::Arbitrary;

// Delegation Bond types

/// A delegation bond's unique identifier (the unique pubkey from arg32_0)
pub type BondKey = [u8; 32];

/// A delegation bond stored in the state
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[cfg_attr(
    any(test, feature = "proptest-impl"),
    derive(Arbitrary, serde::Serialize, serde::Deserialize)
)]
pub struct DelegationBond {
    /// The amount of the bond in zatoshis
    pub amount: Amount<NonNegative>,

    /// The target finalizer (from arg32_2)
    pub target_finalizer: [u8; 32],

    /// The location where this bond was created
    pub created_at: TransactionLocation,
}

impl DelegationBond {
    /// Creates a new delegation bond
    pub fn new(
        amount: Amount<NonNegative>,
        target_finalizer: [u8; 32],
        created_at: TransactionLocation,
    ) -> Self {
        Self {
            amount,
            target_finalizer,
            created_at,
        }
    }

    /// Returns the amount of this bond
    pub fn amount(&self) -> Amount<NonNegative> {
        self.amount
    }

    /// Returns the target finalizer for this bond
    pub fn target_finalizer(&self) -> &[u8; 32] {
        &self.target_finalizer
    }

    /// Returns the location where this bond was created
    pub fn created_at(&self) -> TransactionLocation {
        self.created_at
    }
}

/// Status of a delegation bond
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[cfg_attr(
    any(test, feature = "proptest-impl"),
    derive(Arbitrary, serde::Serialize, serde::Deserialize)
)]
pub enum BondStatus {
    /// Bond is active and can be unbonded
    Active,
    /// Bond is unbonding (BeginDelegationUnbonding called)
    Unbonding {
        /// The transaction location where unbonding began
        unbonded_at: TransactionLocation,
    },
    /// Bond has been withdrawn (WithdrawDelegationBond called)
    Withdrawn {
        /// The transaction location where withdrawal occurred
        withdrawn_at: TransactionLocation,
    },
}

impl BondStatus {
    /// Returns true if this bond is active
    pub fn is_active(&self) -> bool {
        matches!(self, BondStatus::Active)
    }

    /// Returns true if this bond is unbonding
    pub fn is_unbonding(&self) -> bool {
        matches!(self, BondStatus::Unbonding { .. })
    }

    /// Returns true if this bond is withdrawn
    pub fn is_withdrawn(&self) -> bool {
        matches!(self, BondStatus::Withdrawn { .. })
    }
}

// Disk serialization

/// BondKey is stored as 32 bytes on disk
const BOND_KEY_DISK_BYTES: usize = 32;

/// Target finalizer is stored as 32 bytes on disk
const TARGET_FINALIZER_DISK_BYTES: usize = 32;

/// Amount is stored as 8 bytes on disk
const AMOUNT_DISK_BYTES: usize = 8;

/// TransactionLocation is stored as 5 bytes on disk (from block.rs)
const TRANSACTION_LOCATION_DISK_BYTES: usize = 5;

/// DelegationBond disk size: 8 (amount) + 32 (target_finalizer) + 5 (created_at)
const DELEGATION_BOND_DISK_BYTES: usize =
    AMOUNT_DISK_BYTES + TARGET_FINALIZER_DISK_BYTES + TRANSACTION_LOCATION_DISK_BYTES;

impl IntoDisk for BondKey {
    type Bytes = [u8; BOND_KEY_DISK_BYTES];

    fn as_bytes(&self) -> Self::Bytes {
        *self
    }
}

impl FromDisk for BondKey {
    fn from_bytes(bytes: impl AsRef<[u8]>) -> Self {
        bytes
            .as_ref()
            .try_into()
            .expect("BondKey should be exactly 32 bytes")
    }
}

impl IntoDisk for DelegationBond {
    type Bytes = Vec<u8>;

    fn as_bytes(&self) -> Self::Bytes {
        let mut bytes = Vec::with_capacity(DELEGATION_BOND_DISK_BYTES);

        // Serialize amount (8 bytes, u64 big-endian)
        bytes.extend_from_slice(&self.amount.zatoshis().to_be_bytes());

        // Serialize target_finalizer (32 bytes)
        bytes.extend_from_slice(&self.target_finalizer);

        // Serialize created_at location (5 bytes via IntoDisk)
        bytes.extend_from_slice(self.created_at.as_bytes().as_ref());

        bytes
    }
}

impl FromDisk for DelegationBond {
    fn from_bytes(bytes: impl AsRef<[u8]>) -> Self {
        let bytes = bytes.as_ref();
        assert!(
            bytes.len() >= DELEGATION_BOND_DISK_BYTES,
            "DelegationBond requires at least {} bytes, got {}",
            DELEGATION_BOND_DISK_BYTES,
            bytes.len()
        );

        let mut offset = 0;

        // Deserialize amount (8 bytes)
        let amount_bytes: [u8; 8] = bytes[offset..offset + AMOUNT_DISK_BYTES]
            .try_into()
            .expect("amount should be 8 bytes");
        let amount_zatoshis = u64::from_be_bytes(amount_bytes);
        let amount = Amount::try_from(amount_zatoshis)
            .expect("amount should be valid");
        offset += AMOUNT_DISK_BYTES;

        // Deserialize target_finalizer (32 bytes)
        let target_finalizer: [u8; 32] = bytes[offset..offset + TARGET_FINALIZER_DISK_BYTES]
            .try_into()
            .expect("target_finalizer should be 32 bytes");
        offset += TARGET_FINALIZER_DISK_BYTES;

        // Deserialize created_at location (5 bytes)
        let created_at = TransactionLocation::from_bytes(&bytes[offset..offset + TRANSACTION_LOCATION_DISK_BYTES]);

        Self {
            amount,
            target_finalizer,
            created_at,
        }
    }
}

impl IntoDisk for BondStatus {
    type Bytes = Vec<u8>;

    fn as_bytes(&self) -> Self::Bytes {
        let mut bytes = Vec::new();

        match self {
            BondStatus::Active => {
                // Tag: 0
                bytes.push(0);
            }
            BondStatus::Unbonding { unbonded_at } => {
                // Tag: 1
                bytes.push(1);
                // Serialize transaction location (5 bytes)
                bytes.extend_from_slice(unbonded_at.as_bytes().as_ref());
            }
            BondStatus::Withdrawn { withdrawn_at } => {
                // Tag: 2
                bytes.push(2);
                // Serialize transaction location (5 bytes)
                bytes.extend_from_slice(withdrawn_at.as_bytes().as_ref());
            }
        }

        bytes
    }
}

impl FromDisk for BondStatus {
    fn from_bytes(bytes: impl AsRef<[u8]>) -> Self {
        let bytes = bytes.as_ref();
        assert!(
            !bytes.is_empty(),
            "BondStatus requires at least 1 byte for the tag"
        );

        let tag = bytes[0];
        match tag {
            0 => BondStatus::Active,
            1 => {
                assert!(
                    bytes.len() >= 1 + TRANSACTION_LOCATION_DISK_BYTES,
                    "BondStatus::Unbonding requires {} bytes, got {}",
                    1 + TRANSACTION_LOCATION_DISK_BYTES,
                    bytes.len()
                );
                let unbonded_at = TransactionLocation::from_bytes(&bytes[1..1 + TRANSACTION_LOCATION_DISK_BYTES]);
                BondStatus::Unbonding { unbonded_at }
            }
            2 => {
                assert!(
                    bytes.len() >= 1 + TRANSACTION_LOCATION_DISK_BYTES,
                    "BondStatus::Withdrawn requires {} bytes, got {}",
                    1 + TRANSACTION_LOCATION_DISK_BYTES,
                    bytes.len()
                );
                let withdrawn_at = TransactionLocation::from_bytes(&bytes[1..1 + TRANSACTION_LOCATION_DISK_BYTES]);
                BondStatus::Withdrawn { withdrawn_at }
            }
            _ => panic!("Invalid BondStatus tag: {}", tag),
        }
    }
}
