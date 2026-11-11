//! A counted set: the part of the `mset` 0.1 crate the non-finalized state uses.

use std::{
    borrow::Borrow,
    collections::{hash_map, HashMap},
    fmt,
    hash::Hash,
};

/// A set that counts how many times each element was inserted.
#[derive(Clone)]
pub(crate) struct MultiSet<T> {
    // Every stored count is at least one.
    counts: HashMap<T, usize>,
}

impl<T> MultiSet<T> {
    /// Creates an empty multiset.
    pub(crate) fn new() -> Self {
        MultiSet {
            counts: HashMap::new(),
        }
    }

    /// Returns the total number of elements, counting repeats.
    pub(crate) fn len(&self) -> usize {
        self.counts.values().sum()
    }

    /// Returns `true` if the multiset has no elements.
    pub(crate) fn is_empty(&self) -> bool {
        self.counts.is_empty()
    }

    /// Iterates over each distinct element once, in arbitrary order.
    pub(crate) fn distinct_elements(&self) -> hash_map::Keys<'_, T, usize> {
        self.counts.keys()
    }
}

impl<T: Hash + Eq> MultiSet<T> {
    /// Adds one `value`, returning `true` if it was not already present.
    pub(crate) fn insert(&mut self, value: T) -> bool {
        let count = self.counts.entry(value).or_insert(0);
        *count += 1;
        *count == 1
    }

    /// Removes one `value`, returning `true` if it was present.
    pub(crate) fn remove<Q: ?Sized + Hash + Eq>(&mut self, value: &Q) -> bool
    where
        T: Borrow<Q>,
    {
        let Some(count) = self.counts.get_mut(value) else {
            return false;
        };
        *count -= 1;
        if *count == 0 {
            self.counts.remove(value);
        }
        true
    }

    /// Returns `true` if `value` is present at least once.
    pub(crate) fn contains<Q: ?Sized + Hash + Eq>(&self, value: &Q) -> bool
    where
        T: Borrow<Q>,
    {
        self.counts.contains_key(value)
    }
}

impl<T> Default for MultiSet<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// Like `mset`'s, equality compares only the total number of elements, not which elements
/// they are. `TransparentTransfers` derives its equality through it, and so `Chain`'s
/// `eq_internal_state` does too.
impl<T> PartialEq for MultiSet<T> {
    fn eq(&self, other: &Self) -> bool {
        self.len() == other.len()
    }
}

impl<T> Eq for MultiSet<T> {}

impl<T: fmt::Debug> fmt::Debug for MultiSet<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set().entries(self.counts.iter()).finish()
    }
}

impl<T: Hash + Eq> FromIterator<T> for MultiSet<T> {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        let mut set = MultiSet::new();
        for value in iter {
            set.insert(value);
        }
        set
    }
}
