//! A map that also keeps its keys sorted by a value derived from each entry.
//!
//! This is the part of the `ordered-map` 0.4 crate the address book uses, copied here
//! because that crate lists `quickcheck` as a normal dependency, which brought `rand` 0.7
//! and the `syn` 1 proc-macro stack into every build.

use std::{collections::HashMap, hash::Hash};

/// A `HashMap` whose keys are also kept in descending order of `extract_comparable(&V)`.
///
/// An entry inserted with a comparable equal to existing ones goes before them.
#[derive(Clone, Debug)]
pub struct OrderedMap<K, V, C> {
    map: HashMap<K, V>,
    descending_pairs: Vec<(K, C)>,
    extract_comparable: fn(&V) -> C,
}

impl<K, V, C> OrderedMap<K, V, C>
where
    K: Eq + Hash + Copy,
    C: PartialOrd,
{
    /// Creates an empty map, ordered by `extract_comparable`.
    pub fn new(extract_comparable: fn(&V) -> C) -> Self {
        OrderedMap {
            map: HashMap::new(),
            descending_pairs: Vec::new(),
            extract_comparable,
        }
    }

    /// Returns the number of entries.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Returns the keys in descending order.
    pub fn descending_keys(&self) -> impl DoubleEndedIterator<Item = &K> {
        self.descending_pairs.iter().map(|(k, _)| k)
    }

    /// Returns the values in descending order.
    pub fn descending_values(&self) -> impl DoubleEndedIterator<Item = &V> {
        self.descending_keys().map(|k| &self.map[k])
    }

    /// Inserts `v` under `k`, returning the value it replaced.
    pub fn insert(&mut self, k: K, v: V) -> Option<V> {
        let c = (self.extract_comparable)(&v);
        let old = self.map.insert(k, v);
        if old.is_some() {
            self.remove_from_pairs(&k);
        }
        let index = self
            .descending_pairs
            .iter()
            .position(|(_, existing)| &c >= existing)
            .unwrap_or(self.descending_pairs.len());
        self.descending_pairs.insert(index, (k, c));
        old
    }

    /// Removes the entry under `k`, returning its value.
    pub fn remove(&mut self, k: &K) -> Option<V> {
        let v = self.map.remove(k)?;
        self.remove_from_pairs(k);
        Some(v)
    }

    fn remove_from_pairs(&mut self, k: &K) {
        if let Some(i) = self.descending_pairs.iter().position(|(pk, _)| pk == k) {
            self.descending_pairs.remove(i);
        }
    }
}
