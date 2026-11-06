//! Defines method signatures for checking if the synchronizer is likely close to the network chain tip.

#[cfg(any(test, feature = "proptest-impl"))]
pub mod mock;

#[cfg(any(test, feature = "proptest-impl"))]
pub use mock::MockSyncStatus;

/// An interface for checking if the synchronization is likely close to the network chain tip.
pub trait ChainSyncStatus {
    /// Check if the synchronization is likely close to the network chain tip.
    fn is_close_to_tip(&self) -> bool;
}

/// Any [`ChainSyncStatus`], behind a trait object.
///
/// Code that stores a sync status can take this type instead of a type parameter, so it is
/// compiled once rather than once per sync status type in every crate that calls it.
#[derive(Clone)]
pub struct BoxSyncStatus(std::sync::Arc<dyn ChainSyncStatus + Send + Sync>);

impl BoxSyncStatus {
    /// Boxes `status`. Its clones share it, which `is_close_to_tip(&self)` cannot observe.
    pub fn new(status: impl ChainSyncStatus + Send + Sync + 'static) -> Self {
        BoxSyncStatus(std::sync::Arc::new(status))
    }
}

impl ChainSyncStatus for BoxSyncStatus {
    fn is_close_to_tip(&self) -> bool {
        self.0.is_close_to_tip()
    }
}

impl std::fmt::Debug for BoxSyncStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoxSyncStatus")
            .field("is_close_to_tip", &self.is_close_to_tip())
            .finish()
    }
}
