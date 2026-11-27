//! Process-local proof of a configured body hash over immutable owned bytes.
//!
//! This is neither a proposal/header certificate nor application validity. It
//! is never serialized: recovery checks the stored bytes through the ordinary
//! core path. Only bounded runtime owners may retain clones of this handle.
use super::{Config, Error, Id, Result};
use std::sync::Arc;

#[cfg(test)]
std::thread_local! {
    static HASH_WORK: std::cell::Cell<(usize, usize)> = const { std::cell::Cell::new((0, 0)) };
}
#[cfg(test)]
pub(super) fn record_hash(bytes: usize) {
    HASH_WORK.with(|count| {
        let (calls, total) = count.get();
        count.set((calls + 1, total + bytes));
    });
}
#[cfg(test)]
pub(super) fn hash_work() -> (usize, usize) {
    HASH_WORK.with(std::cell::Cell::get)
}

#[derive(Clone)]
pub(super) struct CheckedBody {
    config: Id,
    height: u64,
    id: Id,
    // Arc<Vec> transfers an assembly allocation without copying it. No mutable
    // access or Arc is exposed, so a checked allocation can never be changed.
    bytes: Arc<Vec<u8>>,
}
impl CheckedBody {
    pub(super) fn new(config: &Config, height: u64, id: Id, bytes: Vec<u8>) -> Result<Self> {
        if bytes.len() > config.max_body() {
            return Err(Error::Invalid);
        }
        #[cfg(test)]
        record_hash(bytes.len());
        if config.body_id(&bytes) != id {
            return Err(Error::Invalid);
        }
        Ok(Self {
            config: config.hash(),
            height,
            id,
            bytes: Arc::new(bytes),
        })
    }
    pub(super) fn bytes(&self) -> &[u8] {
        &self.bytes
    }
    pub(super) fn matches(&self, config: &Config, height: u64, id: Id) -> bool {
        self.config == config.hash()
            && self.height == height
            && self.id == id
            && self.bytes.len() <= config.max_body()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::d28::{Member, Timeouts};
    use ed25519_zebra::{SigningKey, VerificationKeyBytes};

    fn config(domain: u8, crosslink: bool) -> Config {
        let members = vec![Member {
            public_key: VerificationKeyBytes::from(&SigningKey::from([1; 32])).into(),
            weight: 1,
        }];
        let timeouts = Timeouts {
            propose: 30,
            prevote: 5,
            precommit: 10,
            inc_propose: 3,
            inc_prevote: 1,
            inc_precommit: 2,
        };
        if crosslink {
            Config::crosslink([domain; 32], members, 1024, 10, timeouts).unwrap()
        } else {
            Config::new([domain; 32], members, 1024, 10, timeouts).unwrap()
        }
    }

    #[test]
    fn checked_body_moves_and_shares_only_the_verified_allocation() {
        let c = config(1, false);
        let bytes = vec![7; 512];
        let allocation = bytes.as_ptr();
        let id = c.body_id(&bytes);
        let body = CheckedBody::new(&c, 4, id, bytes).unwrap();
        let copy = body.clone();
        assert_eq!(body.bytes().as_ptr(), allocation);
        assert_eq!(copy.bytes().as_ptr(), allocation);
        assert!(body.matches(&c, 4, id));
        assert!(!body.matches(&c, 5, id));
        assert!(!body.matches(&c, 4, [0; 32]));
        assert!(!body.matches(&config(2, false), 4, id));
        assert!(!body.matches(&config(1, true), 4, id));
    }

    #[test]
    fn checked_body_requires_actual_bytes_hash_and_configured_bound() {
        for crosslink in [false, true] {
            let c = config(1, crosslink);
            let bytes = vec![9; 512];
            let id = c.body_id(&bytes);
            assert!(CheckedBody::new(&c, 4, id, bytes).is_ok());
            assert!(CheckedBody::new(&c, 4, id, vec![8; 512]).is_err());
            let oversized = vec![9; 1025];
            let id = c.body_id(&oversized);
            assert!(CheckedBody::new(&c, 4, id, oversized).is_err());
        }
    }
}
