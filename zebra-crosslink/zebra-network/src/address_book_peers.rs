//! A AddressBookPeers trait for getting the [`MetaAddr`] of recently live peers.

use chrono::Utc;

use crate::{meta_addr::MetaAddr, PeerSocketAddr};

#[cfg(any(test, feature = "proptest-impl"))]
pub mod mock;

#[cfg(any(test, feature = "proptest-impl"))]
pub use mock::MockAddressBookPeers;

/// Method signatures for getting [`MetaAddr`]s of recently live peers.
pub trait AddressBookPeers {
    /// Return an Vec of peers we've seen recently, in reconnection attempt order.
    fn recently_live_peers(&self, now: chrono::DateTime<Utc>) -> Vec<MetaAddr>;

    /// Add a peer to the address book.
    fn add_peer(&mut self, peer: PeerSocketAddr) -> bool;
}

/// Any [`AddressBookPeers`] + `Clone`, behind a trait object.
///
/// Code that stores an address book can take this type instead of a type parameter, so it is
/// compiled once rather than once per address book type in every crate that calls it.
pub struct BoxAddressBookPeers(Box<dyn DynAddressBookPeers>);

impl BoxAddressBookPeers {
    /// Boxes `address_book`.
    pub fn new(address_book: impl AddressBookPeers + Clone + Send + Sync + 'static) -> Self {
        BoxAddressBookPeers(Box::new(address_book))
    }
}

impl Clone for BoxAddressBookPeers {
    fn clone(&self) -> Self {
        BoxAddressBookPeers(self.0.clone_box())
    }
}

impl std::fmt::Debug for BoxAddressBookPeers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoxAddressBookPeers").finish()
    }
}

impl AddressBookPeers for BoxAddressBookPeers {
    fn recently_live_peers(&self, now: chrono::DateTime<Utc>) -> Vec<MetaAddr> {
        self.0.recently_live_peers(now)
    }

    fn add_peer(&mut self, peer: PeerSocketAddr) -> bool {
        self.0.add_peer(peer)
    }
}

/// [`AddressBookPeers`] plus an object-safe `clone`.
trait DynAddressBookPeers: AddressBookPeers + Send + Sync {
    fn clone_box(&self) -> Box<dyn DynAddressBookPeers>;
}

impl<T: AddressBookPeers + Clone + Send + Sync + 'static> DynAddressBookPeers for T {
    fn clone_box(&self) -> Box<dyn DynAddressBookPeers> {
        Box::new(self.clone())
    }
}
