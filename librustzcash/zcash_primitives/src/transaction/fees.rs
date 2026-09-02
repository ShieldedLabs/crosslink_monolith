//! Abstractions and types related to fee calculations.

use crate::transaction::fees::transparent::InputSize;
use zcash_protocol::{
    consensus::{self, BlockHeight},
    value::Zatoshis,
};

#[cfg(feature = "non-standard-fees")]
pub mod fixed;
pub mod transparent;
pub mod zip317;

/// A trait that represents the ability to compute the fees that must be paid
/// by a transaction having a specified set of inputs and outputs.
pub trait FeeRule {
    type Error;

    /// Computes the total fee required for a transaction given the provided inputs and outputs.
    ///
    /// Implementations of this method should compute the fee amount given exactly the inputs and
    /// outputs specified, and should NOT compute speculative fees given any additional change
    /// outputs that may need to be created in order for inputs and outputs to balance.
    #[allow(clippy::too_many_arguments)]
    fn fee_required<P: consensus::Parameters>(
        &self,
        params: &P,
        target_height: BlockHeight,
        transparent_input_sizes: impl IntoIterator<Item = InputSize>,
        transparent_output_sizes: impl IntoIterator<Item = usize>,
        sapling_input_count: usize,
        sapling_output_count: usize,
        orchard_action_count: usize,
        ironwood_action_count: usize,
        // Crosslink: a staking action counts as one logical action, but is never
        // covered by the grace allowance — it always adds a full marginal fee on
        // top of the standard fee (so 15000 for a base tx today, and it scales
        // automatically if a dynamic-fee update changes the marginal fee).
        staking_action_count: usize,
    ) -> Result<Zatoshis, Self::Error>;
}
