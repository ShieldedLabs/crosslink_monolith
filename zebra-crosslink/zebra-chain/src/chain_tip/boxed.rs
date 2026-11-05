//! A type-erased [`ChainTip`].

use std::{fmt, future::Future, pin::Pin, sync::Arc};

use chrono::{DateTime, Utc};

use crate::{block, parameters::Network, transaction, BoxError};

use super::ChainTip;

/// Any [`ChainTip`], behind a trait object.
///
/// Code that stores or passes on a chain tip can take this type instead of a type parameter,
/// so it is compiled once rather than once per chain tip type in every crate that calls it.
/// Every method, including the provided ones a mock may override, forwards to the inner tip.
pub struct BoxChainTip(Box<dyn DynChainTip>);

impl BoxChainTip {
    /// Boxes `tip`.
    pub fn new(tip: impl ChainTip + Clone + Send + Sync + 'static) -> Self {
        BoxChainTip(Box::new(tip))
    }
}

impl Clone for BoxChainTip {
    fn clone(&self) -> Self {
        BoxChainTip(self.0.clone_box())
    }
}

impl fmt::Debug for BoxChainTip {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BoxChainTip")
            .field("best_tip_height_and_hash", &self.0.best_tip_height_and_hash())
            .finish()
    }
}

/// The object-safe form of [`ChainTip`]: `best_tip_changed` returns a boxed future.
trait DynChainTip: Send + Sync {
    fn best_tip_height(&self) -> Option<block::Height>;
    fn best_tip_hash(&self) -> Option<block::Hash>;
    fn best_tip_height_and_hash(&self) -> Option<(block::Height, block::Hash)>;
    fn best_tip_block_time(&self) -> Option<DateTime<Utc>>;
    fn best_tip_height_and_block_time(&self) -> Option<(block::Height, DateTime<Utc>)>;
    fn best_tip_mined_transaction_ids(&self) -> Arc<[transaction::Hash]>;
    fn best_tip_changed(&mut self) -> Pin<Box<dyn Future<Output = Result<(), BoxError>> + Send + '_>>;
    fn mark_best_tip_seen(&mut self);
    fn estimate_network_chain_tip_height(
        &self,
        network: &Network,
        now: DateTime<Utc>,
    ) -> Option<block::Height>;
    fn estimate_distance_to_network_chain_tip(
        &self,
        network: &Network,
    ) -> Option<(block::HeightDiff, block::Height)>;
    fn is_at_or_near_network_tip(&self, network: &Network) -> bool;
    fn clone_box(&self) -> Box<dyn DynChainTip>;
}

impl<T: ChainTip + Clone + Send + Sync + 'static> DynChainTip for T {
    fn best_tip_height(&self) -> Option<block::Height> {
        ChainTip::best_tip_height(self)
    }
    fn best_tip_hash(&self) -> Option<block::Hash> {
        ChainTip::best_tip_hash(self)
    }
    fn best_tip_height_and_hash(&self) -> Option<(block::Height, block::Hash)> {
        ChainTip::best_tip_height_and_hash(self)
    }
    fn best_tip_block_time(&self) -> Option<DateTime<Utc>> {
        ChainTip::best_tip_block_time(self)
    }
    fn best_tip_height_and_block_time(&self) -> Option<(block::Height, DateTime<Utc>)> {
        ChainTip::best_tip_height_and_block_time(self)
    }
    fn best_tip_mined_transaction_ids(&self) -> Arc<[transaction::Hash]> {
        ChainTip::best_tip_mined_transaction_ids(self)
    }
    fn best_tip_changed(&mut self) -> Pin<Box<dyn Future<Output = Result<(), BoxError>> + Send + '_>> {
        Box::pin(ChainTip::best_tip_changed(self))
    }
    fn mark_best_tip_seen(&mut self) {
        ChainTip::mark_best_tip_seen(self)
    }
    fn estimate_network_chain_tip_height(
        &self,
        network: &Network,
        now: DateTime<Utc>,
    ) -> Option<block::Height> {
        ChainTip::estimate_network_chain_tip_height(self, network, now)
    }
    fn estimate_distance_to_network_chain_tip(
        &self,
        network: &Network,
    ) -> Option<(block::HeightDiff, block::Height)> {
        ChainTip::estimate_distance_to_network_chain_tip(self, network)
    }
    fn is_at_or_near_network_tip(&self, network: &Network) -> bool {
        ChainTip::is_at_or_near_network_tip(self, network)
    }
    fn clone_box(&self) -> Box<dyn DynChainTip> {
        Box::new(self.clone())
    }
}

impl ChainTip for BoxChainTip {
    fn best_tip_height(&self) -> Option<block::Height> {
        self.0.best_tip_height()
    }
    fn best_tip_hash(&self) -> Option<block::Hash> {
        self.0.best_tip_hash()
    }
    fn best_tip_height_and_hash(&self) -> Option<(block::Height, block::Hash)> {
        self.0.best_tip_height_and_hash()
    }
    fn best_tip_block_time(&self) -> Option<DateTime<Utc>> {
        self.0.best_tip_block_time()
    }
    fn best_tip_height_and_block_time(&self) -> Option<(block::Height, DateTime<Utc>)> {
        self.0.best_tip_height_and_block_time()
    }
    fn best_tip_mined_transaction_ids(&self) -> Arc<[transaction::Hash]> {
        self.0.best_tip_mined_transaction_ids()
    }
    fn best_tip_changed(&mut self) -> impl Future<Output = Result<(), BoxError>> + Send {
        self.0.best_tip_changed()
    }
    fn mark_best_tip_seen(&mut self) {
        self.0.mark_best_tip_seen()
    }
    fn estimate_network_chain_tip_height(
        &self,
        network: &Network,
        now: DateTime<Utc>,
    ) -> Option<block::Height> {
        self.0.estimate_network_chain_tip_height(network, now)
    }
    fn estimate_distance_to_network_chain_tip(
        &self,
        network: &Network,
    ) -> Option<(block::HeightDiff, block::Height)> {
        self.0.estimate_distance_to_network_chain_tip(network)
    }
    fn is_at_or_near_network_tip(&self, network: &Network) -> bool {
        self.0.is_at_or_near_network_tip(network)
    }
}
