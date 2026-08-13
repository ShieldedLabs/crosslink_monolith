//! Structs and methods for handling Zcash transactions.
pub mod builder;
pub mod components;
pub mod fees;
pub mod sighash;
pub mod sighash_v4;
pub mod sighash_v5;
pub mod sighash_vcrosslink;
pub mod sighash_v6;

pub mod txid;

#[cfg(any(test, feature = "test-dependencies"))]
pub mod tests;

use crate::encoding::{ReadBytesExt, WriteBytesExt};
use crate::bft::PubKeyID;
use blake2b_simd::Hash as Blake2bHash;
use core::convert::TryFrom;
use core::fmt::Debug;
use core::ops::Deref;
use corez::io::{self, Read, Write};

use ::transparent::bundle::{self as transparent, OutPoint, TxIn, TxOut};
use zcash_encoding::{CompactSize, Vector};
use zcash_protocol::{
    consensus::{BlockHeight, BranchId},
    value::{BalanceError, ZatBalance, Zatoshis},
};
// `valid_in_branch` matches on every branch by bare name.
use zcash_protocol::consensus::BranchId::*;

use self::{
    components::{
        orchard as orchard_serialization, sapling as sapling_serialization,
        sprout::{self, JsDescription},
    },
    txid::{BlockTxCommitmentDigester, TxIdDigester, to_txid},
};
use ::transparent::util::sha256d::{HashReader, HashWriter};

#[cfg(feature = "circuits")]
use ::sapling::builder as sapling_builder;

use zcash_protocol::constants::{
    V3_TX_VERSION, V3_VERSION_GROUP_ID, V4_TX_VERSION, V4_VERSION_GROUP_ID, V5_TX_VERSION,
    V5_VERSION_GROUP_ID,
    VCROSSLINK_TX_VERSION,
    VCROSSLINK_VERSION_GROUP_ID,
};

use zcash_protocol::constants::{V6_TX_VERSION, V6_VERSION_GROUP_ID};

pub use zcash_protocol::TxId;
use serde::Serialize;

/// The set of defined transaction format versions.
///
/// This is serialized in the first four or eight bytes of the transaction format, and
/// represents valid combinations of the `(overwintered, version, version_group_id)`
/// transaction fields. Note that this is not dependent on epoch, only on transaction encoding.
/// For example, if a particular epoch defines a new transaction version but also allows the
/// previous version, then only the new version would be added to this enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TxVersion {
    /// Transaction versions allowed prior to Overwinter activation. The argument MUST be
    /// in the range `1..=0x7FFFFFFF`. Only versions 1 and 2 are defined; `3..=0x7FFFFFFF`
    /// was allowed by consensus but considered equivalent to 2. This is specified in
    /// [§ 7.1 Transaction Encoding and Consensus](https://zips.z.cash/protocol/protocol.pdf#txnencoding).
    Sprout(u32),
    /// Transaction version 3, which was introduced by the Overwinter network upgrade
    /// and allowed until Sapling activation. It is specified in
    /// [§ 7.1 Transaction Encoding and Consensus](https://zips.z.cash/protocol/protocol.pdf#txnencoding).
    V3,
    /// Transaction version 4, which was introduced by the Sapling network upgrade.
    /// It is specified in [§ 7.1 Transaction Encoding and Consensus](https://zips.z.cash/protocol/protocol.pdf#txnencoding).
    V4,
    /// Transaction version 5, which was introduced by the NU5 network upgrade.
    /// It is specified in [§ 7.1 Transaction Encoding and Consensus](https://zips.z.cash/protocol/protocol.pdf#txnencoding)
    /// and [ZIP 225](https://zips.z.cash/zip-0225).
    V5,
    /// Transaction version 6, specified in [ZIP 229](https://zips.z.cash/zip-0229).
    V6,
    VCrosslink,
    /// This version is used exclusively for in-development transaction
    /// serialization, and will never be active under the consensus rules.
    /// When new consensus transaction versions are added, all call sites
    /// using this constant should be inspected, and uses should be
    /// removed as appropriate in favor of the new transaction version.
    #[cfg(zcash_unstable = "zfuture")]
    ZFuture,
}

impl TxVersion {
    pub fn read<R: Read>(mut reader: R) -> io::Result<Self> {
        let header = reader.read_u32_le()?;
        let overwintered = (header >> 31) == 1;
        let version = header & 0x7FFFFFFF;

        if overwintered {
            match (version, reader.read_u32_le()?) {
                (V3_TX_VERSION, V3_VERSION_GROUP_ID) => Ok(TxVersion::V3),
                (V4_TX_VERSION, V4_VERSION_GROUP_ID) => Ok(TxVersion::V4),
                (V5_TX_VERSION, V5_VERSION_GROUP_ID) => Ok(TxVersion::V5),
                (V6_TX_VERSION, V6_VERSION_GROUP_ID) => Ok(TxVersion::V6),
                (VCROSSLINK_TX_VERSION, VCROSSLINK_VERSION_GROUP_ID) => Ok(TxVersion::VCrosslink),
                #[cfg(zcash_unstable = "zfuture")]
                (ZFUTURE_TX_VERSION, ZFUTURE_VERSION_GROUP_ID) => Ok(TxVersion::ZFuture),
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Unknown transaction format",
                )),
            }
        } else if version >= 1 {
            Ok(TxVersion::Sprout(version))
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Unknown transaction format",
            ))
        }
    }

    pub fn header(&self) -> u32 {
        // After Sprout, the overwintered bit is always set.
        let overwintered = match self {
            TxVersion::Sprout(_) => 0,
            _ => 1 << 31,
        };

        overwintered
            | match self {
                TxVersion::Sprout(v) => *v,
                TxVersion::V3 => V3_TX_VERSION,
                TxVersion::V4 => V4_TX_VERSION,
                TxVersion::V5 => V5_TX_VERSION,
                TxVersion::V6 => V6_TX_VERSION,
                TxVersion::VCrosslink => VCROSSLINK_TX_VERSION,
                #[cfg(zcash_unstable = "zfuture")]
                TxVersion::ZFuture => ZFUTURE_TX_VERSION,
            }
    }

    pub fn version_group_id(&self) -> u32 {
        match self {
            TxVersion::Sprout(_) => 0,
            TxVersion::V3 => V3_VERSION_GROUP_ID,
            TxVersion::V4 => V4_VERSION_GROUP_ID,
            TxVersion::V5 => V5_VERSION_GROUP_ID,
            TxVersion::VCrosslink => VCROSSLINK_VERSION_GROUP_ID,
            TxVersion::V6 => V6_VERSION_GROUP_ID,
        }
    }

    pub fn write<W: Write>(&self, mut writer: W) -> io::Result<()> {
        writer.write_u32_le(self.header())?;
        match self {
            TxVersion::Sprout(_) => Ok(()),
            _ => writer.write_u32_le(self.version_group_id()),
        }
    }

    /// Returns `true` if this transaction version supports the Sprout protocol.
    pub fn has_sprout(&self) -> bool {
        match self {
            TxVersion::Sprout(v) => *v >= 2u32,
            TxVersion::V3 | TxVersion::V4 => true,
            TxVersion::V5 => false,
            TxVersion::VCrosslink => false,
            TxVersion::V6 => false,
        }
    }

    pub fn has_overwinter(&self) -> bool {
        !matches!(self, TxVersion::Sprout(_))
    }

    /// Returns `true` if this transaction version supports the Sapling protocol.
    pub fn has_sapling(&self) -> bool {
        match self {
            TxVersion::Sprout(_) | TxVersion::V3 => false,
            TxVersion::V4 => true,
            TxVersion::V5 => true,
            TxVersion::VCrosslink => true,
            TxVersion::V6 => true,
        }
    }

    /// Returns `true` if this transaction version supports the Orchard protocol.
    pub fn has_orchard(&self) -> bool {
        match self {
            TxVersion::Sprout(_) | TxVersion::V3 | TxVersion::V4 => false,
            TxVersion::V5 => true,
            TxVersion::VCrosslink => true,
            TxVersion::V6 => true,
        }
    }

    /// Returns `true` if this transaction version supports the Ironwood protocol.
    pub fn has_ironwood(&self) -> bool {
        match self {
            TxVersion::Sprout(_) | TxVersion::V3 | TxVersion::V4 | TxVersion::V5 => false,
            TxVersion::V6 | TxVersion::VCrosslink => true,
        }
    }

    #[cfg(all(zcash_unstable = "nu7", feature = "zip-233"))]
    pub fn has_zip233(&self) -> bool {
        match self {
            TxVersion::Sprout(_) | TxVersion::V3 | TxVersion::V4 | TxVersion::V5 => false,
            TxVersion::V6 => true,
        }
    }

    /// Suggests the transaction version that should be used in the given Zcash epoch.
    pub fn suggested_for_branch(consensus_branch_id: BranchId) -> Self {
        match consensus_branch_id {
            BranchId::Sprout => TxVersion::Sprout(2),
            BranchId::Overwinter => TxVersion::V3,
            BranchId::Sapling | BranchId::Blossom | BranchId::Heartwood | BranchId::Canopy => {
                TxVersion::V4
            }
            BranchId::Nu5 => TxVersion::V5,
            BranchId::Nu6 => TxVersion::V5,
            BranchId::Nu6_1 => TxVersion::V5,
            BranchId::Nu6_2 => TxVersion::V5,
            BranchId::Nu6_3 => TxVersion::V6,
            #[cfg(zcash_unstable = "nu7")]
            BranchId::Nu7 => TxVersion::V6,
        }
    }

    /// Returns `true` if this transaction version is valid for us in the specified consensus
    /// branch, `false` otherwise.
    pub fn valid_in_branch(&self, consensus_branch_id: BranchId) -> bool {
        // Note: we intentionally use `match` expressions instead of the `matches!`
        // macro below because we want exhaustivity.
        match self {
            TxVersion::Sprout(_) => consensus_branch_id == Sprout,
            TxVersion::V3 => consensus_branch_id == Overwinter,
            TxVersion::VCrosslink => matches!(consensus_branch_id, Nu6_3),
            TxVersion::V4 => match consensus_branch_id {
                Sprout | Overwinter => false,
                Sapling | Blossom | Heartwood | Canopy | Nu5 | Nu6 | Nu6_1 | Nu6_2 => true,
                Nu6_3 => true,
                #[cfg(zcash_unstable = "nu7")]
                Nu7 => false, // ZIP 2003
            },
            TxVersion::V5 => match consensus_branch_id {
                Sprout | Overwinter | Sapling | Blossom | Heartwood | Canopy => false,
                Nu5 | Nu6 | Nu6_1 | Nu6_2 => true,
                Nu6_3 => true,
                #[cfg(zcash_unstable = "nu7")]
                Nu7 => true,
            },
            TxVersion::V6 => match consensus_branch_id {
                Sprout | Overwinter | Sapling | Blossom | Heartwood | Canopy | Nu5 | Nu6
                | Nu6_1 | Nu6_2 => false,
                Nu6_3 => true, // Ironwood / NU6.3
                #[cfg(zcash_unstable = "nu7")]
                Nu7 => true, // ZIP 230 or ZIP 248, whichever is chosen for activation
            },
        }
    }
}

/// Authorization state for a bundle of transaction data.
pub trait Authorization {
    type TransparentAuth: transparent::Authorization;
    type SaplingAuth: sapling::bundle::Authorization;
    type OrchardAuth: orchard::bundle::Authorization;
}

/// [`Authorization`] marker type for fully-authorized transactions.
#[derive(Clone, Debug)]
pub struct Authorized;

impl Authorization for Authorized {
    type TransparentAuth = transparent::Authorized;
    type SaplingAuth = sapling::bundle::Authorized;
    type OrchardAuth = orchard::bundle::Authorized;
}

/// [`Authorization`] marker type for non-coinbase transactions without authorization data.
///
/// Currently this includes Sapling proofs because the types in this crate support v4
/// transactions, which commit to the Sapling proofs in the transaction digest.
pub struct Unauthorized;

#[cfg(feature = "circuits")]
impl Authorization for Unauthorized {
    type TransparentAuth = ::transparent::builder::Unauthorized;
    type SaplingAuth =
        sapling_builder::InProgress<sapling_builder::Proven, sapling_builder::Unsigned>;
    type OrchardAuth =
        orchard::builder::InProgress<orchard::builder::Unproven, orchard::builder::Unauthorized>;
}

/// [`Authorization`] marker type for coinbase transactions without authorization data.
#[cfg(feature = "circuits")]
struct Coinbase;

#[cfg(feature = "circuits")]
impl Authorization for Coinbase {
    type TransparentAuth = ::transparent::builder::Coinbase;
    type SaplingAuth =
        sapling_builder::InProgress<sapling_builder::Proven, sapling_builder::Unsigned>;
    type OrchardAuth =
        orchard::builder::InProgress<orchard::builder::Unproven, orchard::builder::Unauthorized>;
}

/// A Zcash transaction.
#[derive(Debug)]
pub struct Transaction {
    txid: TxId,
    data: TransactionData<Authorized>,
}

impl Deref for Transaction {
    type Target = TransactionData<Authorized>;

    fn deref(&self) -> &TransactionData<Authorized> {
        &self.data
    }
}

impl PartialEq for Transaction {
    fn eq(&self, other: &Transaction) -> bool {
        self.txid == other.txid
    }
}

/// The information contained in a Zcash transaction.
#[derive(Debug)]
pub struct TransactionData<A: Authorization> {
    version: TxVersion,
    consensus_branch_id: BranchId,
    lock_time: u32,
    expiry_height: BlockHeight,
    #[cfg(all(zcash_unstable = "nu7", feature = "zip-233"))]
    zip233_amount: Zatoshis,
    transparent_bundle: Option<transparent::Bundle<A::TransparentAuth>>,
    sprout_bundle: Option<sprout::Bundle>,
    sapling_bundle: Option<sapling::Bundle<A::SaplingAuth, ZatBalance>>,
    orchard_bundle: Option<orchard::bundle::Bundle<A::OrchardAuth, ZatBalance>>,
    ironwood_bundle: Option<orchard::bundle::Bundle<A::OrchardAuth, ZatBalance>>,
    staking_action: Option<StakingAction>,
    #[cfg(zcash_unstable = "zfuture")]
    tze_bundle: Option<tze::Bundle<A::TzeAuth>>,
}

impl Clone for TransactionData<Authorized> {
    fn clone(&self) -> Self {
        TransactionData {
            version: self.version,
            consensus_branch_id: self.consensus_branch_id,
            lock_time: self.lock_time,
            expiry_height: self.expiry_height,
            #[cfg(all(zcash_unstable = "nu7", feature = "zip-233"))]
            zip233_amount: self.zip233_amount,
            transparent_bundle: self.transparent_bundle.clone(),
            sprout_bundle: self.sprout_bundle.clone(),
            sapling_bundle: self.sapling_bundle.clone(),
            orchard_bundle: self.orchard_bundle.clone(),
            ironwood_bundle: self.ironwood_bundle.clone(),
            staking_action: self.staking_action.clone(),
            #[cfg(zcash_unstable = "zfuture")]
            tze_bundle: self.tze_bundle.clone(),
        }
    }
}

impl Clone for Transaction {
    fn clone(&self) -> Self {
        // SAFETY: We're reconstructing the Transaction from its data.
        // The txid is deterministic from the data, so cloning data and
        // re-computing txid would be equivalent.
        Transaction {
            txid: self.txid,
            data: self.data.clone(),
        }
    }
}

impl<A: Authorization> TransactionData<A> {
    /// Constructs a `TransactionData` from its constituent parts.
    #[allow(clippy::too_many_arguments)]
    pub fn from_parts(
        version: TxVersion,
        consensus_branch_id: BranchId,
        lock_time: u32,
        expiry_height: BlockHeight,
        #[cfg(all(zcash_unstable = "nu7", feature = "zip-233"))] zip233_amount: Zatoshis,
        transparent_bundle: Option<transparent::Bundle<A::TransparentAuth>>,
        sprout_bundle: Option<sprout::Bundle>,
        sapling_bundle: Option<sapling::Bundle<A::SaplingAuth, ZatBalance>>,
        orchard_bundle: Option<orchard::Bundle<A::OrchardAuth, ZatBalance>>,
        staking_action: Option<StakingAction>,
    ) -> Self {
        TransactionData {
            version,
            consensus_branch_id,
            lock_time,
            expiry_height,
            #[cfg(all(zcash_unstable = "nu7", feature = "zip-233"))]
            zip233_amount,
            transparent_bundle,
            sprout_bundle,
            sapling_bundle,
            orchard_bundle,
            ironwood_bundle: None,
            staking_action,
            #[cfg(zcash_unstable = "zfuture")]
            tze_bundle: None,
        }
    }

    /// Constructs a V6 [`TransactionData`] from its constituent parts,
    /// including the Ironwood bundle.
    ///
    /// Both the Orchard and Ironwood bundle fields use [`orchard::Bundle`], but
    /// they are distinct V6 transaction fields with distinct bundle versions.
    /// The `orchard_bundle` argument must contain a bundle constructed for
    /// [`orchard::bundle::BundleVersion::orchard_v3`], while `ironwood_bundle`
    /// must contain a bundle constructed for
    /// [`orchard::bundle::BundleVersion::ironwood_v3`]. Supplying a bundle for
    /// the wrong field is invalid and can be rejected by later serialization or
    /// commitment construction because the bundle flags and domains are protocol
    /// specific.
    #[allow(clippy::too_many_arguments)]
    pub fn from_parts_v6(
        consensus_branch_id: BranchId,
        lock_time: u32,
        expiry_height: BlockHeight,
        #[cfg(all(zcash_unstable = "nu7", feature = "zip-233"))] zip233_amount: Zatoshis,
        transparent_bundle: Option<transparent::Bundle<A::TransparentAuth>>,
        sapling_bundle: Option<sapling::Bundle<A::SaplingAuth, ZatBalance>>,
        orchard_bundle: Option<orchard::Bundle<A::OrchardAuth, ZatBalance>>,
        ironwood_bundle: Option<orchard::Bundle<A::OrchardAuth, ZatBalance>>,
    ) -> Self {
        TransactionData {
            version: TxVersion::V6,
            consensus_branch_id,
            lock_time,
            expiry_height,
            #[cfg(all(zcash_unstable = "nu7", feature = "zip-233"))]
            zip233_amount,
            staking_action: None,
            transparent_bundle,
            sprout_bundle: None,
            sapling_bundle,
            orchard_bundle,
            ironwood_bundle,
        }
    }

    /// Returns the transaction version.
    pub fn version(&self) -> TxVersion {
        self.version
    }

    /// Returns the Zcash epoch that this transaction can be mined in.
    pub fn consensus_branch_id(&self) -> BranchId {
        self.consensus_branch_id
    }

    pub fn lock_time(&self) -> u32 {
        self.lock_time
    }

    pub fn expiry_height(&self) -> BlockHeight {
        self.expiry_height
    }

    pub fn transparent_bundle(&self) -> Option<&transparent::Bundle<A::TransparentAuth>> {
        self.transparent_bundle.as_ref()
    }

    pub fn sprout_bundle(&self) -> Option<&sprout::Bundle> {
        self.sprout_bundle.as_ref()
    }

    pub fn sapling_bundle(&self) -> Option<&sapling::Bundle<A::SaplingAuth, ZatBalance>> {
        self.sapling_bundle.as_ref()
    }

    pub fn orchard_bundle(&self) -> Option<&orchard::Bundle<A::OrchardAuth, ZatBalance>> {
        self.orchard_bundle.as_ref()
    }

    pub fn ironwood_bundle(&self) -> Option<&orchard::Bundle<A::OrchardAuth, ZatBalance>> {
        self.ironwood_bundle.as_ref()
    }

    pub fn staking_action(&self) -> Option<StakingAction> {
        self.staking_action.clone()
    }

    #[cfg(all(
        any(zcash_unstable = "nu7", zcash_unstable = "zfuture"),
        feature = "zip-233"
    ))]
    pub fn zip233_amount(&self) -> Zatoshis {
        self.zip233_amount
    }

    #[cfg(all(zcash_unstable = "nu7", feature = "zip-233"))]
    pub fn zip233_amount(&self) -> Zatoshis {
        self.zip233_amount
    }

    /// Returns the total fees paid by the transaction, given a function that can be used to
    /// retrieve the value of previous transactions' transparent outputs that are being spent in
    /// this transaction.
    pub fn fee_paid<E, F>(&self, get_prevout: F) -> Result<Option<Zatoshis>, E>
    where
        E: From<BalanceError>,
        F: FnMut(&OutPoint) -> Result<Option<Zatoshis>, E>,
    {
        let transparent_balance = self.transparent_bundle.as_ref().map_or_else(
            || Ok(Some(ZatBalance::zero())),
            |b| b.value_balance(get_prevout),
        )?;

        transparent_balance
            .map(|transparent_balance| {
                let value_balances = [
                    transparent_balance,
                    self.sprout_bundle.as_ref().map_or_else(
                        || Ok(ZatBalance::zero()),
                        |b| b.value_balance().ok_or(BalanceError::Overflow),
                    )?,
                    self.sapling_bundle
                        .as_ref()
                        .map_or_else(ZatBalance::zero, |b| *b.value_balance()),
                    self.orchard_bundle
                        .as_ref()
                        .map_or_else(ZatBalance::zero, |b| *b.value_balance()),
                    self.ironwood_bundle
                        .as_ref()
                        .map_or_else(ZatBalance::zero, |b| *b.value_balance()),
                    #[cfg(all(zcash_unstable = "nu7", feature = "zip-233"))]
                    -ZatBalance::from(self.zip233_amount),
                ];

                let overall_balance = value_balances
                    .iter()
                    .sum::<Option<_>>()
                    .ok_or(BalanceError::Overflow)?;

                Zatoshis::try_from(overall_balance).map_err(|_| BalanceError::Underflow)
            })
            .transpose()
            .map_err(E::from)
    }

    /// Computes this transaction's digest using the provided digest strategy.
    ///
    /// Version 6 transactions include the Ironwood bundle digest as a separate
    /// Orchard-shaped digest with Ironwood personalization. Earlier transaction
    /// versions do not include Ironwood in their digest.
    pub fn digest<D: TransactionDigest<A>>(&self, digester: D) -> D::Digest {
        digester.combine(
            digester.digest_header(
                self.version,
                self.consensus_branch_id,
                self.lock_time,
                self.expiry_height,
                #[cfg(all(zcash_unstable = "nu7", feature = "zip-233"))]
                &self.zip233_amount,
            ),
            digester.digest_transparent(self.transparent_bundle.as_ref()),
            digester.digest_sapling(self.version, self.sapling_bundle.as_ref()),
            digester.digest_orchard(self.version, self.orchard_bundle.as_ref()),
            digester.digest_ironwood(self.ironwood_bundle.as_ref()),
            digester.digest_crosslink(&self.staking_action),
            #[cfg(zcash_unstable = "zfuture")]
            digester.digest_tze(self.tze_bundle.as_ref()),
        )
    }

    /// Changes the consensus branch ID stored in this transaction for pre-v5 transactions.
    ///
    /// This can be used to fix an incorrect value passed to [`Transaction::read`]. Just
    /// like that method, this method does nothing for v5+ transactions.
    pub(crate) fn fix_consensus_branch_id(mut self, consensus_branch_id: BranchId) -> Self {
        match self.version() {
            TxVersion::Sprout(_) | TxVersion::V3 | TxVersion::V4 => {
                self.consensus_branch_id = consensus_branch_id;
            }
            // All later tx versions directly commit to the consensus branch ID, so what
            // we parse is what we trust.
            _ => (),
        }
        self
    }

    /// Maps the bundles from one type to another.
    ///
    /// This shouldn't be necessary for most use cases; it is provided for handling the
    /// cross-FFI builder logic in `zcashd`.
    ///
    /// `f_orchard` is also applied to the Ironwood bundle because Ironwood is
    /// represented with the Orchard bundle type.
    pub fn map_bundles<B: Authorization>(
        self,
        f_transparent: impl FnOnce(
            Option<transparent::Bundle<A::TransparentAuth>>,
        ) -> Option<transparent::Bundle<B::TransparentAuth>>,
        f_sapling: impl FnOnce(
            Option<sapling::Bundle<A::SaplingAuth, ZatBalance>>,
        ) -> Option<sapling::Bundle<B::SaplingAuth, ZatBalance>>,
        mut f_orchard: impl FnMut(
            Option<orchard::bundle::Bundle<A::OrchardAuth, ZatBalance>>,
        )
            -> Option<orchard::bundle::Bundle<B::OrchardAuth, ZatBalance>>,
    ) -> TransactionData<B> {
        TransactionData {
            version: self.version,
            consensus_branch_id: self.consensus_branch_id,
            lock_time: self.lock_time,
            expiry_height: self.expiry_height,
            #[cfg(all(zcash_unstable = "nu7", feature = "zip-233"))]
            zip233_amount: self.zip233_amount,
            transparent_bundle: f_transparent(self.transparent_bundle),
            sprout_bundle: self.sprout_bundle,
            sapling_bundle: f_sapling(self.sapling_bundle),
            orchard_bundle: f_orchard(self.orchard_bundle),
            ironwood_bundle: f_orchard(self.ironwood_bundle),
            staking_action: self.staking_action,
            #[cfg(zcash_unstable = "zfuture")]
            tze_bundle: f_tze(self.tze_bundle),
        }
    }

    /// Maps the bundles from one type to another with fallible closures.
    ///
    /// This shouldn't be necessary for most use cases; it is provided for handling the
    /// transaction extraction logic in the `pczt` crate.
    ///
    /// `f_orchard` is also applied to the Ironwood bundle because Ironwood is
    /// represented with the Orchard bundle type.
    pub fn try_map_bundles<B: Authorization, E>(
        self,
        f_transparent: impl FnOnce(
            Option<transparent::Bundle<A::TransparentAuth>>,
        )
            -> Result<Option<transparent::Bundle<B::TransparentAuth>>, E>,
        f_sapling: impl FnOnce(
            Option<sapling::Bundle<A::SaplingAuth, ZatBalance>>,
        )
            -> Result<Option<sapling::Bundle<B::SaplingAuth, ZatBalance>>, E>,
        mut f_orchard: impl FnMut(
            Option<orchard::bundle::Bundle<A::OrchardAuth, ZatBalance>>,
        ) -> Result<
            Option<orchard::bundle::Bundle<B::OrchardAuth, ZatBalance>>,
            E,
        >,
    ) -> Result<TransactionData<B>, E> {
        Ok(TransactionData {
            version: self.version,
            consensus_branch_id: self.consensus_branch_id,
            lock_time: self.lock_time,
            expiry_height: self.expiry_height,
            #[cfg(all(zcash_unstable = "nu7", feature = "zip-233"))]
            zip233_amount: self.zip233_amount,
            transparent_bundle: f_transparent(self.transparent_bundle)?,
            sprout_bundle: self.sprout_bundle,
            sapling_bundle: f_sapling(self.sapling_bundle)?,
            orchard_bundle: f_orchard(self.orchard_bundle)?,
            ironwood_bundle: f_orchard(self.ironwood_bundle)?,
            staking_action: self.staking_action,
            #[cfg(zcash_unstable = "zfuture")]
            tze_bundle: f_tze(self.tze_bundle)?,
        })
    }

    pub fn map_authorization<B: Authorization>(
        self,
        f_transparent: impl transparent::MapAuth<A::TransparentAuth, B::TransparentAuth>,
        mut f_sapling: impl sapling_serialization::MapAuth<A::SaplingAuth, B::SaplingAuth>,
        mut f_orchard: impl orchard_serialization::MapAuth<A::OrchardAuth, B::OrchardAuth>,
    ) -> TransactionData<B> {
        TransactionData {
            version: self.version,
            consensus_branch_id: self.consensus_branch_id,
            lock_time: self.lock_time,
            expiry_height: self.expiry_height,
            #[cfg(all(zcash_unstable = "nu7", feature = "zip-233"))]
            zip233_amount: self.zip233_amount,
            transparent_bundle: self
                .transparent_bundle
                .map(|b| b.map_authorization(f_transparent)),
            sprout_bundle: self.sprout_bundle,
            sapling_bundle: self.sapling_bundle.map(|b| {
                b.map_authorization(
                    &mut f_sapling,
                    |f, p| f.map_spend_proof(p),
                    |f, p| f.map_output_proof(p),
                    |f, s| f.map_auth_sig(s),
                    |f, a| f.map_authorization(a),
                )
            }),
            orchard_bundle: self.orchard_bundle.map(|b| {
                b.map_authorization(
                    &mut f_orchard,
                    |f, _, s| f.map_spend_auth(s),
                    |f, a| f.map_authorization(a),
                )
            }),
            ironwood_bundle: self.ironwood_bundle.map(|b| {
                b.map_authorization(
                    &mut f_orchard,
                    |f, _, s| f.map_spend_auth(s),
                    |f, a| f.map_authorization(a),
                )
            }),
            staking_action: self.staking_action,
            #[cfg(zcash_unstable = "zfuture")]
            tze_bundle: self.tze_bundle.map(|b| b.map_authorization(f_tze)),
        }
    }
}

impl<A: Authorization> TransactionData<A> {
    pub fn sapling_value_balance(&self) -> ZatBalance {
        self.sapling_bundle
            .as_ref()
            .map_or(ZatBalance::zero(), |b| *b.value_balance())
    }
}

impl TransactionData<Authorized> {
    pub fn freeze(self) -> io::Result<Transaction> {
        Transaction::from_data(self)
    }
}

struct V6HeaderFragment {
    consensus_branch_id: BranchId,
    lock_time: u32,
    expiry_height: BlockHeight,
    #[cfg(all(zcash_unstable = "nu7", feature = "zip-233"))]
    zip233_amount: Zatoshis,
}

impl Transaction {
    fn from_data(data: TransactionData<Authorized>) -> io::Result<Self> {
        match data.version {
            TxVersion::Sprout(_) | TxVersion::V3 | TxVersion::V4 => Self::from_data_v4(data),
            TxVersion::V5 => Ok(Self::from_data_v5(data)),
            TxVersion::VCrosslink => Ok(Self::from_data_vcrosslink(data)),
            TxVersion::V6 => Ok(Self::from_data_v6(data)),
        }
    }

    fn from_data_v4(data: TransactionData<Authorized>) -> io::Result<Self> {
        let mut tx = Transaction {
            txid: TxId::from_bytes([0; 32]),
            data,
        };
        let mut writer = HashWriter::default();
        tx.write(&mut writer)?;
        tx.txid = TxId::from_bytes(writer.into_hash().into());
        Ok(tx)
    }

    fn from_data_v5(data: TransactionData<Authorized>) -> Self {
        let txid = to_txid(
            data.version,
            data.consensus_branch_id,
            &data.digest(TxIdDigester),
        );

        Transaction { txid, data }
    }

    fn from_data_vcrosslink(data: TransactionData<Authorized>) -> Self {
        let txid = to_txid(
            data.version,
            data.consensus_branch_id,
            &data.digest(TxIdDigester),
        );

        Transaction { txid, data }
    }

    fn from_data_v6(data: TransactionData<Authorized>) -> Self {
        let txid = to_txid(
            data.version,
            data.consensus_branch_id,
            &data.digest(TxIdDigester),
        );

        Transaction { txid, data }
    }

    pub fn into_data(self) -> TransactionData<Authorized> {
        self.data
    }

    pub fn txid(&self) -> TxId {
        self.txid
    }

    pub fn read<R: Read>(reader: R, consensus_branch_id: BranchId) -> io::Result<Self> {
        let mut reader = HashReader::new(reader);

        let version = TxVersion::read(&mut reader)?;
        match version {
            TxVersion::Sprout(_) | TxVersion::V3 | TxVersion::V4 => {
                Self::read_v4(reader, version, consensus_branch_id)
            }
            TxVersion::V5 => Self::read_v5(reader.into_base_reader(), version),
            TxVersion::VCrosslink => Self::read_vcrosslink(reader.into_base_reader(), version),
            TxVersion::V6 => Self::read_v6(reader.into_base_reader(), version),
        }
    }

    #[allow(clippy::redundant_closure)]
    fn read_v4<R: Read>(
        mut reader: HashReader<R>,
        version: TxVersion,
        consensus_branch_id: BranchId,
    ) -> io::Result<Self> {
        let transparent_bundle = Self::read_transparent(&mut reader)?;

        let lock_time = reader.read_u32_le()?;
        let expiry_height: BlockHeight = if version.has_overwinter() {
            reader.read_u32_le()?.into()
        } else {
            0u32.into()
        };

        let (value_balance, shielded_spends, shielded_outputs) =
            sapling_serialization::read_v4_components(&mut reader, version.has_sapling())?;

        let sprout_bundle = if version.has_sprout() {
            let joinsplits = Vector::read(&mut reader, |r| {
                JsDescription::read(r, version.has_sapling())
            })?;

            if !joinsplits.is_empty() {
                let mut bundle = sprout::Bundle {
                    joinsplits,
                    joinsplit_pubkey: [0; 32],
                    joinsplit_sig: [0; 64],
                };
                reader.read_exact(&mut bundle.joinsplit_pubkey)?;
                reader.read_exact(&mut bundle.joinsplit_sig)?;
                Some(bundle)
            } else {
                None
            }
        } else {
            None
        };

        let binding_sig = if version.has_sapling()
            && !(shielded_spends.is_empty() && shielded_outputs.is_empty())
        {
            let mut sig = [0; 64];
            reader.read_exact(&mut sig)?;
            Some(redjubjub::Signature::from(sig))
        } else {
            None
        };

        let mut txid = [0; 32];
        let hash_bytes = reader.into_hash();
        txid.copy_from_slice(&hash_bytes);

        Ok(Transaction {
            txid: TxId::from_bytes(txid),
            data: TransactionData {
                version,
                consensus_branch_id,
                lock_time,
                expiry_height,
                #[cfg(all(zcash_unstable = "nu7", feature = "zip-233"))]
                zip233_amount: Zatoshis::ZERO,
                transparent_bundle,
                sprout_bundle,
                sapling_bundle: binding_sig.and_then(|binding_sig| {
                    sapling::Bundle::from_parts(
                        shielded_spends,
                        shielded_outputs,
                        value_balance,
                        sapling::bundle::Authorized { binding_sig },
                    )
                }),
                orchard_bundle: None,
                ironwood_bundle: None,
                staking_action: None,
                #[cfg(zcash_unstable = "zfuture")]
                tze_bundle: None,
            },
        })
    }

    fn read_transparent<R: Read>(
        mut reader: R,
    ) -> io::Result<Option<transparent::Bundle<transparent::Authorized>>> {
        let vin = Vector::read(&mut reader, TxIn::read)?;
        let vout = Vector::read(&mut reader, TxOut::read)?;
        Ok(if vin.is_empty() && vout.is_empty() {
            None
        } else {
            Some(transparent::Bundle {
                vin,
                vout,
                authorization: transparent::Authorized,
            })
        })
    }

    fn read_amount<R: Read>(mut reader: R) -> io::Result<ZatBalance> {
        let mut tmp = [0; 8];
        reader.read_exact(&mut tmp)?;
        ZatBalance::from_i64_le_bytes(tmp)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "valueBalance out of range"))
    }

    fn read_v5<R: Read>(mut reader: R, version: TxVersion) -> io::Result<Self> {
        let (consensus_branch_id, lock_time, expiry_height) =
            Self::read_header_fragment(&mut reader)?;

        #[cfg(all(zcash_unstable = "nu7", feature = "zip-233"))]
        let zip233_amount = Zatoshis::ZERO;

        let transparent_bundle = Self::read_transparent(&mut reader)?;
        let sapling_bundle = sapling_serialization::read_v5_bundle(&mut reader)?;
        let orchard_bundle =
            orchard_serialization::read_v5_bundle(&mut reader, consensus_branch_id)?;

        let data = TransactionData {
            version,
            consensus_branch_id,
            lock_time,
            expiry_height,
            #[cfg(all(zcash_unstable = "nu7", feature = "zip-233"))]
            zip233_amount,
            transparent_bundle,
            sprout_bundle: None,
            sapling_bundle,
            orchard_bundle,
            ironwood_bundle: None,
            staking_action: None,
            #[cfg(zcash_unstable = "zfuture")]
            tze_bundle: None,
        };

        Ok(Self::from_data_v5(data))
    }

    fn read_vcrosslink<R: Read>(mut reader: R, version: TxVersion) -> io::Result<Self> {
        let (consensus_branch_id, lock_time, expiry_height) =
            Self::read_header_fragment(&mut reader)?;

        #[cfg(all(
            any(zcash_unstable = "nu7", zcash_unstable = "zfuture"),
            feature = "zip-233"
        ))]
        let zip233_amount = Zatoshis::ZERO;

        let transparent_bundle = Self::read_transparent(&mut reader)?;
        let sapling_bundle = sapling_serialization::read_v5_bundle(&mut reader)?;
        let orchard_bundle = orchard_serialization::read_vcrosslink_bundle(
            &mut reader,
            consensus_branch_id,
            orchard::ValuePool::Orchard,
        )?;
        let ironwood_bundle = orchard_serialization::read_vcrosslink_bundle(
            &mut reader,
            consensus_branch_id,
            orchard::ValuePool::Ironwood,
        )?;
        let staking_action = StakingAction::read(&mut reader)?;

        let data = TransactionData {
            version,
            ironwood_bundle,
            staking_action,
            consensus_branch_id,
            lock_time,
            expiry_height,
            #[cfg(all(
                any(zcash_unstable = "nu7", zcash_unstable = "zfuture"),
                feature = "zip-233"
            ))]
            zip233_amount,
            transparent_bundle,
            sprout_bundle: None,
            sapling_bundle,
            orchard_bundle,
            #[cfg(zcash_unstable = "zfuture")]
            tze_bundle: None,
        };

        Ok(Self::from_data_vcrosslink(data))
    }

    fn read_v6<R: Read>(mut reader: R, version: TxVersion) -> io::Result<Self> {
        let header_fragment = Self::read_v6_header_fragment(&mut reader)?;

        let transparent_bundle = Self::read_transparent(&mut reader)?;
        let sapling_bundle = sapling_serialization::read_v5_bundle(&mut reader)?;
        let orchard_bundle = orchard_serialization::read_v6_bundle(
            &mut reader,
            header_fragment.consensus_branch_id,
            orchard::ValuePool::Orchard,
        )?;
        let ironwood_bundle = orchard_serialization::read_v6_bundle(
            &mut reader,
            header_fragment.consensus_branch_id,
            orchard::ValuePool::Ironwood,
        )?;

        let data = TransactionData {
            version,
            staking_action: None,
            consensus_branch_id: header_fragment.consensus_branch_id,
            lock_time: header_fragment.lock_time,
            expiry_height: header_fragment.expiry_height,
            #[cfg(all(zcash_unstable = "nu7", feature = "zip-233"))]
            zip233_amount: header_fragment.zip233_amount,
            transparent_bundle,
            sprout_bundle: None,
            sapling_bundle,
            orchard_bundle,
            ironwood_bundle,
        };

        Ok(Self::from_data_v6(data))
    }

    /// Utility function for reading header data common to v5 and v6 transactions.
    fn read_header_fragment<R: Read>(mut reader: R) -> io::Result<(BranchId, u32, BlockHeight)> {
        let consensus_branch_id = reader.read_u32_le().and_then(|value| {
            BranchId::try_from(value).map_err(|_e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    #[cfg(not(feature = "std"))]
                    "invalid consensus branch id",
                    #[cfg(feature = "std")]
                    format!(
                        "invalid consensus branch id 0x{}",
                        hex::encode(value.to_be_bytes())
                    ),
                )
            })
        })?;
        let lock_time = reader.read_u32_le()?;
        let expiry_height: BlockHeight = reader.read_u32_le()?.into();
        Ok((consensus_branch_id, lock_time, expiry_height))
    }

    fn read_v6_header_fragment<R: Read>(mut reader: R) -> io::Result<V6HeaderFragment> {
        let (consensus_branch_id, lock_time, expiry_height) =
            Self::read_header_fragment(&mut reader)?;

        Ok(V6HeaderFragment {
            consensus_branch_id,
            lock_time,
            expiry_height,
            #[cfg(all(zcash_unstable = "nu7", feature = "zip-233"))]
            zip233_amount: Self::read_zip233_amount(&mut reader)?,
        })
    }

    #[cfg(feature = "temporary-zcashd")]
    pub fn temporary_zcashd_read_v5_sapling<R: Read>(
        reader: R,
    ) -> io::Result<Option<sapling::Bundle<sapling::bundle::Authorized, ZatBalance>>> {
        sapling_serialization::read_v5_bundle(reader)
    }

    #[cfg(all(zcash_unstable = "nu7", feature = "zip-233"))]
    fn read_zip233_amount<R: Read>(mut reader: R) -> io::Result<Zatoshis> {
        Zatoshis::from_u64(reader.read_u64_le()?)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "zip233Amount out of range"))
    }

    pub fn write<W: Write>(&self, writer: W) -> io::Result<()> {
        match self.version {
            TxVersion::Sprout(_) | TxVersion::V3 | TxVersion::V4 => self.write_v4(writer),
            TxVersion::V5 => self.write_v5(writer),
            TxVersion::VCrosslink => self.write_vcrosslink(writer),
            TxVersion::V6 => self.write_v6(writer),
        }
    }

    pub fn write_v4<W: Write>(&self, mut writer: W) -> io::Result<()> {
        if self.orchard_bundle.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Orchard components cannot be present when serializing to the V4 transaction format.",
            ));
        }

        self.version.write(&mut writer)?;

        self.write_transparent(&mut writer)?;
        writer.write_u32_le(self.lock_time)?;
        if self.version.has_overwinter() {
            writer.write_u32_le(u32::from(self.expiry_height))?;
        }

        sapling_serialization::write_v4_components(
            &mut writer,
            self.sapling_bundle.as_ref(),
            self.version.has_sapling(),
        )?;

        if self.version.has_sprout() {
            if let Some(bundle) = self.sprout_bundle.as_ref() {
                Vector::write(&mut writer, &bundle.joinsplits, |w, e| e.write(w))?;
                writer.write_all(&bundle.joinsplit_pubkey)?;
                writer.write_all(&bundle.joinsplit_sig)?;
            } else {
                CompactSize::write(&mut writer, 0)?;
            }
        }

        if self.version.has_sapling()
            && let Some(bundle) = self.sapling_bundle.as_ref()
        {
            writer.write_all(&<[u8; 64]>::from(bundle.authorization().binding_sig))?;
        }

        Ok(())
    }

    pub fn write_transparent<W: Write>(&self, mut writer: W) -> io::Result<()> {
        if let Some(bundle) = &self.transparent_bundle {
            Vector::write(&mut writer, &bundle.vin, |w, e| e.write(w))?;
            Vector::write(&mut writer, &bundle.vout, |w, e| e.write(w))?;
        } else {
            CompactSize::write(&mut writer, 0)?;
            CompactSize::write(&mut writer, 0)?;
        }

        Ok(())
    }

    pub fn write_v5<W: Write>(&self, mut writer: W) -> io::Result<()> {
        if self.sprout_bundle.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Sprout components cannot be present when serializing to the V5 transaction format.",
            ));
        }
        self.write_v5_header(&mut writer)?;
        self.write_transparent(&mut writer)?;
        self.write_v5_sapling(&mut writer)?;
        orchard_serialization::write_v5_bundle(self.orchard_bundle.as_ref(), &mut writer)?;

        Ok(())
    }

    pub fn write_vcrosslink<W: Write>(&self, mut writer: W) -> io::Result<()> {
        if self.sprout_bundle.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Sprout components cannot be present when serializing to the V5 transaction format.",
            ));
        }
        self.write_vcrosslink_header(&mut writer)?;
        self.write_transparent(&mut writer)?;
        self.write_v5_sapling(&mut writer)?;
        orchard_serialization::write_vcrosslink_bundle(self.orchard_bundle.as_ref(), &mut writer)?;
        // The Ironwood bundle sits between the Orchard bundle and the Crosslink extras, exactly as
        // `read_vcrosslink` expects and as `write_v6` orders them. Omitting it here desynchronised
        // the writer from its own reader.
        orchard_serialization::write_vcrosslink_bundle(self.ironwood_bundle.as_ref(), &mut writer)?;
        StakingAction::write(&self.staking_action, &mut writer)?;

        Ok(())
    }

    pub fn write_v6<W: Write>(&self, mut writer: W) -> io::Result<()> {
        if self.sprout_bundle.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Sprout components cannot be present when serializing to the V6 transaction format.",
            ));
        }
        self.write_v6_header(&mut writer)?;

        self.write_transparent(&mut writer)?;
        self.write_v5_sapling(&mut writer)?;
        orchard_serialization::write_v6_bundle(self.orchard_bundle.as_ref(), &mut writer)?;
        orchard_serialization::write_v6_bundle(self.ironwood_bundle.as_ref(), &mut writer)?;

        Ok(())
    }

    pub fn write_v5_header<W: Write>(&self, mut writer: W) -> io::Result<()> {
        self.version.write(&mut writer)?;
        writer.write_u32_le(u32::from(self.consensus_branch_id))?;
        writer.write_u32_le(self.lock_time)?;
        writer.write_u32_le(u32::from(self.expiry_height))?;
        Ok(())
    }

    pub fn write_vcrosslink_header<W: Write>(&self, mut writer: W) -> io::Result<()> {
        self.version.write(&mut writer)?;
        writer.write_u32_le(u32::from(self.consensus_branch_id))?;
        writer.write_u32_le(self.lock_time)?;
        writer.write_u32_le(u32::from(self.expiry_height))?;
        Ok(())
    }

    pub fn write_v6_header<W: Write>(&self, mut writer: W) -> io::Result<()> {
        self.version.write(&mut writer)?;
        writer.write_u32_le(u32::from(self.consensus_branch_id))?;
        writer.write_u32_le(self.lock_time)?;
        writer.write_u32_le(u32::from(self.expiry_height))?;

        #[cfg(all(zcash_unstable = "nu7", feature = "zip-233"))]
        writer.write_u64_le(self.zip233_amount.into())?;
        Ok(())
    }

    #[cfg(feature = "temporary-zcashd")]
    pub fn temporary_zcashd_write_v5_sapling<W: Write>(
        sapling_bundle: Option<&sapling::Bundle<sapling::bundle::Authorized, ZatBalance>>,
        writer: W,
    ) -> io::Result<()> {
        sapling_serialization::write_v5_bundle(writer, sapling_bundle)
    }

    pub fn write_v5_sapling<W: Write>(&self, writer: W) -> io::Result<()> {
        sapling_serialization::write_v5_bundle(writer, self.sapling_bundle.as_ref())
    }

    // TODO: should this be moved to `from_data` and stored?
    pub fn auth_commitment(&self) -> Blake2bHash {
        self.data.digest(BlockTxCommitmentDigester)
    }
}

#[derive(Clone, Debug)]
pub struct TransparentDigests<A> {
    pub prevouts_digest: A,
    pub sequence_digest: A,
    pub outputs_digest: A,
}

#[derive(Clone, Debug)]
pub struct TxDigests<A> {
    pub header_digest: A,
    pub transparent_digests: Option<TransparentDigests<A>>,
    pub sapling_digest: Option<A>,
    pub orchard_digest: Option<A>,
    /// The digest of the Ironwood bundle used by version 6 transactions.
    ///
    /// This is `None` when the transaction has no Ironwood bundle. When a version 6 transaction
    /// ID is derived from these digests, `None` is combined as the empty Ironwood bundle digest
    /// using the Ironwood bundle personalization.
    pub ironwood_digest: Option<A>,
    pub crosslink_digest: Option<A>,
    #[cfg(zcash_unstable = "zfuture")]
    pub tze_digests: Option<TzeDigests<A>>,
}

pub trait TransactionDigest<A: Authorization> {
    type HeaderDigest;
    type TransparentDigest;
    type SaplingDigest;
    type OrchardDigest;
    /// The digest type produced for the Ironwood bundle in version 6 transactions.
    type IronwoodDigest;

    type CrosslinkDigest;

    #[cfg(zcash_unstable = "zfuture")]
    type TzeDigest;

    type Digest;

    fn digest_header(
        &self,
        version: TxVersion,
        consensus_branch_id: BranchId,
        lock_time: u32,
        expiry_height: BlockHeight,
        #[cfg(all(zcash_unstable = "nu7", feature = "zip-233"))] zip233_amount: &Zatoshis,
    ) -> Self::HeaderDigest;

    fn digest_transparent(
        &self,
        transparent_bundle: Option<&transparent::Bundle<A::TransparentAuth>>,
    ) -> Self::TransparentDigest;

    fn digest_sapling(
        &self,
        version: TxVersion,
        sapling_bundle: Option<&sapling::Bundle<A::SaplingAuth, ZatBalance>>,
    ) -> Self::SaplingDigest;

    fn digest_orchard(
        &self,
        version: TxVersion,
        orchard_bundle: Option<&orchard::Bundle<A::OrchardAuth, ZatBalance>>,
    ) -> Self::OrchardDigest;

    /// Computes the digest for the Ironwood bundle.
    ///
    /// Ironwood bundles are Orchard-shaped, but they use a distinct bundle personalization.
    /// Transaction ID digesters should return `None` when no Ironwood bundle is present;
    /// version 6 transaction ID combination substitutes the empty Ironwood bundle digest for
    /// `None`. Transaction commitment digesters may instead return an empty authorizing data
    /// digest when no Ironwood bundle is present, and may use a different anchor commitment
    /// policy than transaction ID digesters.
    fn digest_ironwood(
        &self,
        ironwood_bundle: Option<&orchard::Bundle<A::OrchardAuth, ZatBalance>>,
    ) -> Self::IronwoodDigest;

    fn digest_crosslink(
        &self,
        staking_action: &Option<StakingAction>
    ) -> Self::CrosslinkDigest;

    #[cfg(zcash_unstable = "zfuture")]
    fn digest_tze(&self, tze_bundle: Option<&tze::Bundle<A::TzeAuth>>) -> Self::TzeDigest;

    fn combine(
        &self,
        header_digest: Self::HeaderDigest,
        transparent_digest: Self::TransparentDigest,
        sapling_digest: Self::SaplingDigest,
        orchard_digest: Self::OrchardDigest,
        ironwood_digest: Self::IronwoodDigest,
        crosslink_digest: Self::CrosslinkDigest,
        #[cfg(zcash_unstable = "zfuture")] tze_digest: Self::TzeDigest,
    ) -> Self::Digest;
}

pub enum DigestError {
    NotSigned,
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]//, Serialize, Deserialize)]
/// A (temporary) small fixed-size buffer for communicating crosslink dev/test commands
pub struct CommandBuf {
    /// Data buffer to contain short command
    pub data: [u8; 128],
}
impl CommandBuf {
    /// size of the internal buffer
    pub const SIZE: usize = 128;

    /// Create an empty command buffer
    pub fn empty() -> Self {
        CommandBuf { data: [0; 128] }
    }

    pub fn is_empty(&self) -> bool {
        self.data[0] == 0
    }

    /// get a rust string from the fixed-size buffer
    pub fn from_str(str: &str) -> Self {
        let mut buf = Self::empty();
        let n = std::cmp::min(str.len(), Self::SIZE);
        buf.data[..n].copy_from_slice(&str.as_bytes()[..n]);
        buf
    }

    /// get a rust string from the fixed-size buffer
    pub fn to_str(&self) -> &str {
        let mut c = 0;
        while c < self.data.len() {
            if self.data[c] == 0 {
                break;
            }
            c += 1;
        }
        std::str::from_utf8(&self.data[..c]).expect("init with valid UTF-8")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize)]
pub enum StakingActionKind {
    Null,
    CreateNewDelegationBond,
    BeginDelegationUnbonding,
    WithdrawDelegationBond,
    RetargetDelegationBond,

    RegisterFinalizer,
    ConvertFinalizerRewardToDelegationBond,
    UpdateFinalizerKey,
}
impl From<StakingActionKind> for u8 {
    fn from(v: StakingActionKind) -> u8 {
        match v {
            StakingActionKind::Null => 0,
            StakingActionKind::CreateNewDelegationBond => 1,
            StakingActionKind::BeginDelegationUnbonding => 2,
            StakingActionKind::WithdrawDelegationBond => 3,
            StakingActionKind::RetargetDelegationBond => 4,
            StakingActionKind::RegisterFinalizer => 5,
            StakingActionKind::ConvertFinalizerRewardToDelegationBond => 6,
            StakingActionKind::UpdateFinalizerKey => 7,
        }
    }
}
impl TryFrom<u8> for StakingActionKind {
    type Error = ();
    fn try_from(v: u8) -> Result<StakingActionKind, ()> {
        match v {
            0 => Ok(StakingActionKind::Null),
            1 => Ok(StakingActionKind::CreateNewDelegationBond),
            2 => Ok(StakingActionKind::BeginDelegationUnbonding),
            3 => Ok(StakingActionKind::WithdrawDelegationBond),
            4 => Ok(StakingActionKind::RetargetDelegationBond),
            5 => Ok(StakingActionKind::RegisterFinalizer),
            6 => Ok(StakingActionKind::ConvertFinalizerRewardToDelegationBond),
            7 => Ok(StakingActionKind::UpdateFinalizerKey),
            _ => Err(()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct StakeTxId {
    #[serde(with = "hex")]
    pub txid: [u8;32],
    pub zats: u64, // accumulated, not initial
}
impl StakeTxId {
    pub fn write_to<W: Write>(&self, w: &mut W) -> io::Result<()> {
        w.write_all(&self.txid)?;
        w.write_all(&self.zats.to_le_bytes())?;
        Ok(())
    }

    pub fn read_from<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut txid = [0u8; 32];
        r.read_exact(&mut txid)?;

        let mut zats_bytes = [0u8; 8];
        r.read_exact(&mut zats_bytes)?;
        let zats = u64::from_le_bytes(zats_bytes);

        Ok(Self { txid, zats })
    }

    pub fn write_to_vec(&self, data: &mut std::vec::Vec<u8>) {
        data.write_all(&self.txid).unwrap();
        data.write_all(&self.zats.to_le_bytes()).unwrap();
    }
}

#[derive(Debug, Default, Clone, PartialEq, serde::Deserialize, serde::Serialize)]
pub struct RosterMember {
    #[serde(with = "hex")]
    pub pub_key: [u8; 32],
    pub voting_power: u64,
    pub txids: std::vec::Vec<StakeTxId>,
}
impl RosterMember {
    pub fn write_to_vec(&self, data: &mut std::vec::Vec<u8>) {
        data.write_all(&self.pub_key).unwrap();
        data.write_all(&self.voting_power.to_le_bytes()).unwrap();
        data.write_all(&(self.txids.len() as u64).to_le_bytes()).unwrap();
        for txid in &self.txids {
            txid.write_to_vec(data);
        }
    }

    pub fn read_from<R: Read>(r: &mut R) -> io::Result<Self> {
        let mut pub_key = [0u8; 32];
        r.read_exact(&mut pub_key)?;
        let voting_power = r.read_u64_le()?;
        let txids_len = r.read_u64_le()? as usize;
        let mut txids = std::vec::Vec::with_capacity(txids_len);
        for _ in 0..txids_len {
            txids.push(StakeTxId::read_from(r)?);
        }
        Ok(Self { pub_key, voting_power, txids })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct StakingAction_CreateNewDelegationBond {
    pub amount_zats: u64,
    pub unique_pubkey: [u8; 32],
    pub challenge: [u8; 32],
    pub target_finalizer: [u8; 32],
    pub signature: [u8; 64],
}

impl StakingAction_CreateNewDelegationBond {
    pub fn to_union(&self) -> StakingAction {
        StakingAction { kind: StakingActionKind::CreateNewDelegationBond, amount_zats: self.amount_zats, arg32_0: self.unique_pubkey, arg32_1: self.challenge, arg32_2: self.target_finalizer, arg64_0: self.signature, ..Default::default() }
    }
    pub fn try_from_union(union: &StakingAction) -> Option<StakingAction_CreateNewDelegationBond> {
        if union.kind == StakingActionKind::CreateNewDelegationBond {
            Some(StakingAction_CreateNewDelegationBond {
                amount_zats: union.amount_zats,
                unique_pubkey: union.arg32_0,
                challenge: union.arg32_1,
                target_finalizer: union.arg32_2,
                signature: union.arg64_0,
            })
        } else {
            None
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct StakingAction_BeginDelegationUnbonding {
    pub unique_pubkey: [u8; 32],
    pub challenge: [u8; 32],
    pub signature: [u8; 64],
}

impl StakingAction_BeginDelegationUnbonding {
    pub fn to_union(&self) -> StakingAction {
        StakingAction { kind: StakingActionKind::BeginDelegationUnbonding, arg32_0: self.unique_pubkey, arg32_1: self.challenge, arg64_0: self.signature, ..Default::default() }
    }
    pub fn try_from_union(union: &StakingAction) -> Option<StakingAction_BeginDelegationUnbonding> {
        if union.kind == StakingActionKind::BeginDelegationUnbonding {
            Some(StakingAction_BeginDelegationUnbonding {
                unique_pubkey: union.arg32_0,
                challenge: union.arg32_1,
                signature: union.arg64_0,
            })
        } else {
            None
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct StakingAction_RetargetDelegationBond {
    pub unique_pubkey: [u8; 32],
    pub challenge: [u8; 32],
    pub signature: [u8; 64],
    pub target_finalizer: [u8; 32],
}

impl StakingAction_RetargetDelegationBond {
    pub fn to_union(&self) -> StakingAction {
        StakingAction { kind: StakingActionKind::RetargetDelegationBond, arg32_0: self.unique_pubkey, arg32_1: self.challenge, arg64_0: self.signature, arg32_2: self.target_finalizer, ..Default::default() }
    }
    pub fn try_from_union(union: &StakingAction) -> Option<StakingAction_RetargetDelegationBond> {
        if union.kind == StakingActionKind::RetargetDelegationBond {
            Some(StakingAction_RetargetDelegationBond {
                unique_pubkey: union.arg32_0,
                challenge: union.arg32_1,
                signature: union.arg64_0,
                target_finalizer: union.arg32_2,
            })
        } else {
            None
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct StakingAction_WithdrawDelegationBond {
    pub amount_zats: u64,
    pub unique_pubkey: [u8; 32],
    pub challenge: [u8; 32],
    pub signature: [u8; 64],
}

impl StakingAction_WithdrawDelegationBond {
    pub fn to_union(&self) -> StakingAction {
        StakingAction { kind: StakingActionKind::WithdrawDelegationBond, amount_zats: self.amount_zats, arg32_0: self.unique_pubkey, arg32_1: self.challenge, arg64_0: self.signature, ..Default::default() }
    }
    pub fn try_from_union(union: &StakingAction) -> Option<StakingAction_WithdrawDelegationBond> {
        if union.kind == StakingActionKind::WithdrawDelegationBond {
            Some(StakingAction_WithdrawDelegationBond {
                amount_zats: union.amount_zats,
                unique_pubkey: union.arg32_0,
                challenge: union.arg32_1,
                signature: union.arg64_0,
            })
        } else {
            None
        }
    }
}

/// The number of blocks between the start of one staking day and the start of the next.
/// A new staking day starts every N blocks.
pub const STAKING_PERIOD: u32 = 150;

/// The window size within each staking day period where staking actions are allowed.
/// Staking actions are only valid when `block_height % STAKING_PERIOD < STAKING_DAY_WINDOW`.
pub const STAKING_DAY_WINDOW: u32 = 70;

// It takes 2 staking periods to withdraw funds-at-stake into shielded, so currently
// there is not much point to slashing bonds older than that; any smart attacker will
// likely have already withdrawn their stake. That's why the staking period exists: to
// give the community time to notice malicious stake and burn it. If vGloriousFuture
// prevents bond withdrawal until next finalization, a much longer window is warranted,
// but it would require chasing BFT fat pointers. For now, this is the simple answer.
pub const SLASH_ANALYSIS_WINDOW: u32 = 2 * STAKING_PERIOD;

// TODO(code org): should this be under zcash_protocol?
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize)]
pub struct StakingAction {
    pub kind: StakingActionKind,
    pub amount_zats: u64,
    pub arg32_0: [u8; 32],
    pub arg32_1: [u8; 32],
    pub arg32_2: [u8; 32],
    pub arg32_3: [u8; 32],
    #[serde(with = "serde_big_array::BigArray")]
    pub arg64_0: [u8; 64],
    #[serde(with = "serde_big_array::BigArray")]
    pub arg64_1: [u8; 64],
}
impl Default for StakingAction {
    fn default() -> Self {
        StakingAction { kind: 0u8.try_into().unwrap(), amount_zats: 0, arg32_0: [0; 32], arg32_1: [0; 32], arg32_2: [0; 32], arg32_3: [0; 32], arg64_0: [0; 64], arg64_1: [0; 64] }
    }
}

impl StakingAction {
    fn hash_to_state(&self, writer: &mut crate::encoding::StateWrite) -> Option<()> {
        if self.kind == StakingActionKind::CreateNewDelegationBond {
            writer.write_u8(u8::from(self.kind)).ok()?;
            writer.write_all(&self.arg32_0).ok()?; // unique pubkey
            writer.write_all(&self.arg32_1).ok()?; // challenge
            writer.write_all(&self.arg64_0).ok()?; // signature
            writer.write_all(&self.arg32_2).ok()?; // target finalizer
            writer.write_u64_le(self.amount_zats).ok()?;
            return Some(());
        }
        if self.kind == StakingActionKind::BeginDelegationUnbonding {
            writer.write_u8(u8::from(self.kind)).ok()?;
            writer.write_all(&self.arg32_0).ok()?; // unique pubkey
            writer.write_all(&self.arg32_1).ok()?; // challenge
            writer.write_all(&self.arg64_0).ok()?; // signature
            return Some(());
        }
        if self.kind == StakingActionKind::WithdrawDelegationBond {
            writer.write_u8(u8::from(self.kind)).ok()?;
            writer.write_all(&self.arg32_0).ok()?; // unique pubkey
            writer.write_all(&self.arg32_1).ok()?; // challenge
            writer.write_all(&self.arg64_0).ok()?; // signature
            writer.write_u64_le(self.amount_zats).ok()?;
            return Some(());
        }
        if self.kind == StakingActionKind::RetargetDelegationBond {
            writer.write_u8(u8::from(self.kind)).ok()?;
            writer.write_all(&self.arg32_0).ok()?; // unique pubkey
            writer.write_all(&self.arg32_1).ok()?; // challenge
            writer.write_all(&self.arg64_0).ok()?; // signature
            writer.write_all(&self.arg32_2).ok()?; // target finalizer
            return Some(());
        }
        if self.kind == StakingActionKind::RegisterFinalizer {
            writer.write_u8(u8::from(self.kind)).ok()?;
            writer.write_all(&self.arg32_0).ok()?; // unique pubkey
            writer.write_all(&self.arg32_1).ok()?; // challenge
            writer.write_all(&self.arg64_0).ok()?; // signature
            return Some(());
        }
        if self.kind == StakingActionKind::ConvertFinalizerRewardToDelegationBond {
            writer.write_u8(u8::from(self.kind)).ok()?;
            writer.write_all(&self.arg32_0).ok()?; // unique pubkey
            writer.write_all(&self.arg32_1).ok()?; // challenge
            writer.write_all(&self.arg64_0).ok()?; // signature
            writer.write_all(&self.arg32_2).ok()?; // this finalizer
            writer.write_u64_le(self.amount_zats).ok()?;
            writer.write_all(&self.arg32_3).ok()?; // second challenge
            writer.write_all(&self.arg64_1).ok()?; // finalizer signature
            return Some(());
        }
        if self.kind == StakingActionKind::UpdateFinalizerKey {
            writer.write_u8(u8::from(self.kind)).ok()?;
            writer.write_all(&self.arg32_0).ok()?; // unique pubkey
            writer.write_all(&self.arg32_1).ok()?; // challenge
            writer.write_all(&self.arg64_0).ok()?; // signature
            writer.write_all(&self.arg32_2).ok()?; // this finalizer
            writer.write_all(&self.arg32_3).ok()?; // second challenge
            writer.write_all(&self.arg64_1).ok()?; // finalizer signature
            return Some(());
        }
        None
    }

    // TODO: fold in existing
    pub fn str_from_addr(addr: [u8; 32]) -> std::string::String {
        let mut str = std::string::String::with_capacity(64);
        for i in 0..32 {
            str.push_str(&format!("{:02x}", addr[31-i]));
        }
        str
    }
    pub fn addr_from_str_bytes(data: &[u8]) -> Option<[u8; 32]> {
        const VALS: [u8; 256] = {
            let mut v = [0xff; 256];
            v[b'0' as usize] = 0x0;
            v[b'1' as usize] = 0x1;
            v[b'2' as usize] = 0x2;
            v[b'3' as usize] = 0x3;
            v[b'4' as usize] = 0x4;
            v[b'5' as usize] = 0x5;
            v[b'6' as usize] = 0x6;
            v[b'7' as usize] = 0x7;
            v[b'8' as usize] = 0x8;
            v[b'9' as usize] = 0x9;
            v[b'a' as usize] = 0xa;
            v[b'b' as usize] = 0xb;
            v[b'c' as usize] = 0xc;
            v[b'd' as usize] = 0xd;
            v[b'e' as usize] = 0xe;
            v[b'f' as usize] = 0xf;
            v
        };
        let mut buf = [0u8; 32];
        for i in 0..32 {
            let a = data.get(2*i)?;
            let b = data.get(2*i + 1)?;
            let a = VALS[*a as usize];
            if a == 0xff {
                return None;
            }
            let b = VALS[*b as usize];
            if b == 0xff {
                return None;
            }
            buf[31-i] = (a << 4) | b
        }
        Some(buf)
    }

    pub fn read<R: Read>(
        mut reader: R,
    ) -> io::Result<Option<StakingAction>> {
        let tag = reader.read_u8()?;
        if tag == 0 {
            return Ok(None);
        }

        let Ok(kind) = StakingActionKind::try_from(tag) else {
            return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("unexpected staking action tag: {tag}"),
            ));
        };

        if kind == StakingActionKind::CreateNewDelegationBond {
            let mut ret = StakingAction::default();
            ret.kind = kind;
            reader.read_exact(&mut ret.arg32_0)?; // unique pubkey
            reader.read_exact(&mut ret.arg32_1)?; // challenge
            reader.read_exact(&mut ret.arg64_0)?; // signature
            reader.read_exact(&mut ret.arg32_2)?; // target finalizer
            let mut buf = [0u8; 8];
            reader.read_exact(&mut buf)?;
            ret.amount_zats = u64::from_le_bytes(buf);
            return Ok(Some(ret));
        }
        if kind == StakingActionKind::BeginDelegationUnbonding {
            let mut ret = StakingAction::default();
            ret.kind = kind;
            reader.read_exact(&mut ret.arg32_0)?; // unique pubkey
            reader.read_exact(&mut ret.arg32_1)?; // challenge
            reader.read_exact(&mut ret.arg64_0)?; // signature
            return Ok(Some(ret));
        }
        if kind == StakingActionKind::WithdrawDelegationBond {
            let mut ret = StakingAction::default();
            ret.kind = kind;
            reader.read_exact(&mut ret.arg32_0)?; // unique pubkey
            reader.read_exact(&mut ret.arg32_1)?; // challenge
            reader.read_exact(&mut ret.arg64_0)?; // signature
            let mut buf = [0u8; 8];
            reader.read_exact(&mut buf)?;
            ret.amount_zats = u64::from_le_bytes(buf);
            return Ok(Some(ret));
        }
        if kind == StakingActionKind::RetargetDelegationBond {
            let mut ret = StakingAction::default();
            ret.kind = kind;
            reader.read_exact(&mut ret.arg32_0)?; // unique pubkey
            reader.read_exact(&mut ret.arg32_1)?; // challenge
            reader.read_exact(&mut ret.arg64_0)?; // signature
            reader.read_exact(&mut ret.arg32_2)?; // target finalizer
            return Ok(Some(ret));
        }
        if kind == StakingActionKind::RegisterFinalizer {
            let mut ret = StakingAction::default();
            ret.kind = kind;
            reader.read_exact(&mut ret.arg32_0)?; // unique pubkey
            reader.read_exact(&mut ret.arg32_1)?; // challenge
            reader.read_exact(&mut ret.arg64_0)?; // signature
            return Ok(Some(ret));
        }
        if kind == StakingActionKind::ConvertFinalizerRewardToDelegationBond {
            let mut ret = StakingAction::default();
            ret.kind = kind;
            reader.read_exact(&mut ret.arg32_0)?; // unique pubkey
            reader.read_exact(&mut ret.arg32_1)?; // challenge
            reader.read_exact(&mut ret.arg64_0)?; // signature
            reader.read_exact(&mut ret.arg32_2)?; // this finalizer
            let mut buf = [0u8; 8];
            reader.read_exact(&mut buf)?;
            ret.amount_zats = u64::from_le_bytes(buf);
            reader.read_exact(&mut ret.arg32_3)?; // second challenge
            reader.read_exact(&mut ret.arg64_1)?; // finalizer signature
            return Ok(Some(ret));
        }
        if kind == StakingActionKind::UpdateFinalizerKey {
            let mut ret = StakingAction::default();
            ret.kind = kind;
            reader.read_exact(&mut ret.arg32_0)?; // unique pubkey
            reader.read_exact(&mut ret.arg32_1)?; // challenge
            reader.read_exact(&mut ret.arg64_0)?; // signature
            reader.read_exact(&mut ret.arg32_2)?; // this finalizer
            reader.read_exact(&mut ret.arg32_3)?; // second challenge
            reader.read_exact(&mut ret.arg64_1)?; // finalizer signature
            return Ok(Some(ret));
        }
        return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("Kind is not implemented: {:?}", kind),
        ));
    }

    pub fn write<W: Write>(
        staking_action: &Option<StakingAction>,
        mut writer: W,
    ) -> io::Result<()> {
        if let Some(staking_action) = staking_action {
            if staking_action.kind == StakingActionKind::CreateNewDelegationBond {
                writer.write_u8(u8::from(staking_action.kind))?;
                writer.write_all(&staking_action.arg32_0)?; // unique pubkey
                writer.write_all(&staking_action.arg32_1)?; // challenge
                writer.write_all(&staking_action.arg64_0)?; // signature
                writer.write_all(&staking_action.arg32_2)?; // target finalizer
                writer.write_u64_le(staking_action.amount_zats)?;
                return Ok(());
            }
            if staking_action.kind == StakingActionKind::BeginDelegationUnbonding {
                writer.write_u8(u8::from(staking_action.kind))?;
                writer.write_all(&staking_action.arg32_0)?; // unique pubkey
                writer.write_all(&staking_action.arg32_1)?; // challenge
                writer.write_all(&staking_action.arg64_0)?; // signature
                return Ok(());
            }
            if staking_action.kind == StakingActionKind::WithdrawDelegationBond {
                writer.write_u8(u8::from(staking_action.kind))?;
                writer.write_all(&staking_action.arg32_0)?; // unique pubkey
                writer.write_all(&staking_action.arg32_1)?; // challenge
                writer.write_all(&staking_action.arg64_0)?; // signature
                writer.write_u64_le(staking_action.amount_zats)?;
                return Ok(());
            }
            if staking_action.kind == StakingActionKind::RetargetDelegationBond {
                writer.write_u8(u8::from(staking_action.kind))?;
                writer.write_all(&staking_action.arg32_0)?; // unique pubkey
                writer.write_all(&staking_action.arg32_1)?; // challenge
                writer.write_all(&staking_action.arg64_0)?; // signature
                writer.write_all(&staking_action.arg32_2)?; // target finalizer
                return Ok(());
            }
            if staking_action.kind == StakingActionKind::RegisterFinalizer {
                writer.write_u8(u8::from(staking_action.kind))?;
                writer.write_all(&staking_action.arg32_0)?; // unique pubkey
                writer.write_all(&staking_action.arg32_1)?; // challenge
                writer.write_all(&staking_action.arg64_0)?; // signature
                return Ok(());
            }
            if staking_action.kind == StakingActionKind::ConvertFinalizerRewardToDelegationBond {
                writer.write_u8(u8::from(staking_action.kind))?;
                writer.write_all(&staking_action.arg32_0)?; // unique pubkey
                writer.write_all(&staking_action.arg32_1)?; // challenge
                writer.write_all(&staking_action.arg64_0)?; // signature
                writer.write_all(&staking_action.arg32_2)?; // this finalizer
                writer.write_u64_le(staking_action.amount_zats)?;
                writer.write_all(&staking_action.arg32_3)?; // second challenge
                writer.write_all(&staking_action.arg64_1)?; // finalizer signature
                return Ok(());
            }
            if staking_action.kind == StakingActionKind::UpdateFinalizerKey {
                writer.write_u8(u8::from(staking_action.kind))?;
                writer.write_all(&staking_action.arg32_0)?; // unique pubkey
                writer.write_all(&staking_action.arg32_1)?; // challenge
                writer.write_all(&staking_action.arg64_0)?; // signature
                writer.write_all(&staking_action.arg32_2)?; // this finalizer
                writer.write_all(&staking_action.arg32_3)?; // second challenge
                writer.write_all(&staking_action.arg64_1)?; // finalizer signature
                return Ok(());
            }

            return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    format!("Kind is not implemented: {:?}", staking_action.kind),
            ));
        } else {
            writer.write_u8(0)
        }
    }
}
impl std::fmt::Display for StakingAction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let fmter = &mut f.debug_struct("StakingAction");

        let fmt_le_bytes = |fmter: &mut std::fmt::DebugStruct<'_, '_>, name, bytes: &[u8]| { // max length 64 bytes
            let le_bytes = &mut[0u8; 64][..bytes.len()];
            le_bytes.copy_from_slice(bytes);
            le_bytes.reverse();

            let buf = &mut[0u8; 128][..2*bytes.len()];
            hex::encode_to_slice(&le_bytes, buf).expect("buffer large enough to print hex");
            let le_str = std::str::from_utf8(buf).expect("encoding should be ASCII-only");

            fmter.field(name, &le_str);
        };

        fmter.field("kind", match self.kind {
            StakingActionKind::Null => &"Null",
            StakingActionKind::CreateNewDelegationBond => &"CreateNewDelegationBond",
            StakingActionKind::BeginDelegationUnbonding => &"BeginDelegationUnbonding",
            StakingActionKind::WithdrawDelegationBond => &"WithdrawDelegationBond",
            StakingActionKind::RetargetDelegationBond => &"RetargetDelegationBond",
            StakingActionKind::RegisterFinalizer => &"RegisterFinalizer",
            StakingActionKind::ConvertFinalizerRewardToDelegationBond => &"ConvertFinalizerRewardToDelegationBond",
            StakingActionKind::UpdateFinalizerKey => &"UpdateFinalizerKey",
        });

        if self.kind == StakingActionKind::CreateNewDelegationBond {
            fmt_le_bytes(fmter, "unique_public_key", &self.arg32_0);
            fmt_le_bytes(fmter, "challenge", &self.arg32_1);
            fmt_le_bytes(fmter, "signature", &self.arg64_0);
            fmt_le_bytes(fmter, "target_finalizer", &self.arg32_2);
            fmter.field("amount_zats", &self.amount_zats);
        }
        if self.kind == StakingActionKind::BeginDelegationUnbonding {
            fmt_le_bytes(fmter, "unique_public_key", &self.arg32_0);
            fmt_le_bytes(fmter, "challenge", &self.arg32_1);
            fmt_le_bytes(fmter, "signature", &self.arg64_0);
        }
        if self.kind == StakingActionKind::WithdrawDelegationBond {
            fmt_le_bytes(fmter, "unique_public_key", &self.arg32_0);
            fmt_le_bytes(fmter, "challenge", &self.arg32_1);
            fmt_le_bytes(fmter, "signature", &self.arg64_0);
            fmter.field("amount_zats", &self.amount_zats);
        }
        if self.kind == StakingActionKind::RetargetDelegationBond {
            fmt_le_bytes(fmter, "unique_public_key", &self.arg32_0);
            fmt_le_bytes(fmter, "challenge", &self.arg32_1);
            fmt_le_bytes(fmter, "signature", &self.arg64_0);
            fmt_le_bytes(fmter, "target_finalizer", &self.arg32_2);
        }
        if self.kind == StakingActionKind::RegisterFinalizer {
            fmt_le_bytes(fmter, "unique_public_key", &self.arg32_0);
            fmt_le_bytes(fmter, "challenge", &self.arg32_1);
            fmt_le_bytes(fmter, "signature", &self.arg64_0);
        }
        if self.kind == StakingActionKind::ConvertFinalizerRewardToDelegationBond {
            fmt_le_bytes(fmter, "unique_public_key", &self.arg32_0);
            fmt_le_bytes(fmter, "challenge", &self.arg32_1);
            fmt_le_bytes(fmter, "signature", &self.arg64_0);
            fmt_le_bytes(fmter, "this_finalizer", &self.arg32_2);
            fmter.field("amount_zats", &self.amount_zats);
            fmt_le_bytes(fmter, "second_challenge", &self.arg32_3);
            fmt_le_bytes(fmter, "finalizer_signature", &self.arg64_1);
        }
        if self.kind == StakingActionKind::UpdateFinalizerKey {
            fmt_le_bytes(fmter, "unique_public_key", &self.arg32_0);
            fmt_le_bytes(fmter, "challenge", &self.arg32_1);
            fmt_le_bytes(fmter, "signature", &self.arg64_0);
            fmt_le_bytes(fmter, "this_finalizer", &self.arg32_2);
            fmt_le_bytes(fmter, "second_challenge", &self.arg32_3);
            fmt_le_bytes(fmter, "finalizer_signature", &self.arg64_1);
        }
        fmter.finish()
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum StakingActionRequest {
    CreateNewDelegationBond{ amount_zats: u64, target_finalizer: PubKeyID },
    RetargetDelegationBond{ bond_key: PubKeyID, target_finalizer: PubKeyID },
    BeginDelegationUnbonding{ bond_key: PubKeyID  },
    WithdrawDelegationBond{ bond_key: PubKeyID  },
}

#[cfg(any(test, feature = "test-dependencies"))]
pub mod testing {
    use proptest::prelude::*;

    use ::transparent::bundle::testing::{self as transparent};
    use zcash_protocol::consensus::BranchId;

    use super::{
        Authorized, Transaction, TransactionData, TxId, TxVersion,
        components::{
            orchard::testing::{self as orchard},
            sapling::testing::{self as sapling},
        },
    };

    #[cfg(all(zcash_unstable = "nu7", feature = "zip-233"))]
    use zcash_protocol::value::{MAX_MONEY, Zatoshis};

    pub fn arb_txid() -> impl Strategy<Value = TxId> {
        prop::array::uniform32(any::<u8>()).prop_map(TxId::from_bytes)
    }

    pub fn arb_tx_version(branch_id: BranchId) -> impl Strategy<Value = TxVersion> {
        match branch_id {
            BranchId::Sprout => (1..=2u32).prop_map(TxVersion::Sprout).boxed(),
            BranchId::Overwinter => Just(TxVersion::V3).boxed(),
            BranchId::Sapling | BranchId::Blossom | BranchId::Heartwood | BranchId::Canopy => {
                Just(TxVersion::V4).boxed()
            }
            BranchId::Nu5 => Just(TxVersion::V5).boxed(),
            BranchId::Nu6 => Just(TxVersion::V5).boxed(),
            BranchId::Nu6_1 => Just(TxVersion::V5).boxed(),
            BranchId::Nu6_2 => Just(TxVersion::V5).boxed(),
            BranchId::Nu6_3 => Just(TxVersion::V6).boxed(),
            #[cfg(zcash_unstable = "nu7")]
            BranchId::Nu7 => Just(TxVersion::V6).boxed(),
            #[cfg(zcash_unstable = "zfuture")]
            BranchId::ZFuture => Just(TxVersion::ZFuture).boxed(),
        }
    }

    #[cfg(all(zcash_unstable = "nu7", not(feature = "zip-233")))]
    prop_compose! {
        pub fn arb_txdata(consensus_branch_id: BranchId)(
            version in arb_tx_version(consensus_branch_id)
        )(
            lock_time in any::<u32>(),
            expiry_height in any::<u32>(),
            transparent_bundle in transparent::arb_bundle(),
            sapling_bundle in sapling::arb_bundle_for_version(version),
            orchard_bundle in orchard::arb_bundle_for_version(version),
            ironwood_bundle in orchard::arb_ironwood_bundle_for_version(version),
            version in Just(version),
        ) -> TransactionData<Authorized> {
            TransactionData {
                version,
                consensus_branch_id,
                lock_time,
                expiry_height: expiry_height.into(),
                transparent_bundle,
                sprout_bundle: None,
                sapling_bundle,
                orchard_bundle,
                ironwood_bundle,
                staking_action: None,
            }
        }
    }

    #[cfg(all(zcash_unstable = "nu7", feature = "zip-233"))]
    prop_compose! {
        pub fn arb_txdata(consensus_branch_id: BranchId)(
            version in arb_tx_version(consensus_branch_id)
        )(
            lock_time in any::<u32>(),
            expiry_height in any::<u32>(),
            zip233_amount in 0..=MAX_MONEY,
            transparent_bundle in transparent::arb_bundle(),
            sapling_bundle in sapling::arb_bundle_for_version(version),
            orchard_bundle in orchard::arb_bundle_for_version(version),
            ironwood_bundle in orchard::arb_ironwood_bundle_for_version(version),
            version in Just(version),
        ) -> TransactionData<Authorized> {
            TransactionData {
                version,
                consensus_branch_id,
                lock_time,
                expiry_height: expiry_height.into(),
                zip233_amount: Zatoshis::from_u64(zip233_amount).unwrap(),
                transparent_bundle,
                sprout_bundle: None,
                sapling_bundle,
                orchard_bundle,
                ironwood_bundle,
                staking_action: None,
            }
        }
    }

    #[cfg(not(zcash_unstable = "nu7"))]
    prop_compose! {
        pub fn arb_txdata(consensus_branch_id: BranchId)(
            version in arb_tx_version(consensus_branch_id)
        )(
            lock_time in any::<u32>(),
            expiry_height in any::<u32>(),
            transparent_bundle in transparent::arb_bundle(),
            sapling_bundle in sapling::arb_bundle_for_version(version),
            orchard_bundle in orchard::arb_bundle_for_version(version),
            ironwood_bundle in orchard::arb_ironwood_bundle_for_version(version),
            version in Just(version),
        ) -> TransactionData<Authorized> {
            TransactionData {
                version,
                consensus_branch_id,
                lock_time,
                expiry_height: expiry_height.into(),
                transparent_bundle,
                sprout_bundle: None,
                sapling_bundle,
                orchard_bundle,
                ironwood_bundle,
                staking_action: None,
            }
        }
    }

    prop_compose! {
        pub fn arb_tx(branch_id: BranchId)(tx_data in arb_txdata(branch_id)) -> Transaction {
            Transaction::from_data(tx_data).unwrap()
        }
    }
}
