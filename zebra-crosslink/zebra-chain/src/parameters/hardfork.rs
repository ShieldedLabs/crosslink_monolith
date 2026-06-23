//! User-led hardfork rules.
//!
//! Crosslink supports user-led hardforks: rules, supplied in the node config,
//! that activate at a given PoW block height (and BFT certificate height) and
//! terminate a set of finalizers. Over time, user-led hardforks become assumed
//! by the software, so the config rules are merged with a set of rules shipped
//! in the executable (see [`shipped_hardforks`]).
//!
//! The canonical, merged-and-validated set is represented by [`HardForkSchedule`],
//! which is built once at config load and then shared (read-only) with everything
//! that needs it — zebra-state, zebra-consensus and zebra-crosslink.

// The per-rule type is defined in `zcash_primitives::bft` so that `BftBlock` can
// embed an entry (zebra-chain depends on zcash_primitives, not the reverse). Its
// `terminated_finalizers` are `PubKeyID` finalizer identities, which already
// round-trip in the Bitcoin-style byte order used everywhere else. Config-load
// validation (staking-aligned heights, unique and non-empty finalizers) lives on
// the type in `bft.rs` via serde `deserialize_with`. The scheduling and merge
// logic below stays here, where node config lives.
pub use zcash_primitives::bft::HardForkConfig;

/// Hardfork rules shipped in the executable.
///
/// User-led hardforks (from the config file) are merged with these, since over
/// time user-led hardforks become assumed by the software.
pub fn shipped_hardforks() -> Vec<HardForkConfig> {
    Vec::new()
}

/// The canonical, ordered set of hardfork rules in effect for a node.
///
/// Built once via [`HardForkSchedule::new`] from the user-configured rules merged
/// with [`shipped_hardforks`]. The contained rules are sorted by
/// `pow_activation_height` and each rule's `terminated_finalizers` list is sorted,
/// so the schedule is canonical and ready to be committed to by hash. Because the
/// rules are height-sorted, lookups binary-search.
///
/// Share a single `Arc<HardForkSchedule>` with everything that needs it rather
/// than copying the `Vec` around.
#[derive(Clone, Debug, Eq, PartialEq, Default)]
pub struct HardForkSchedule {
    /// Hardfork rules, sorted ascending by `pow_activation_height`.
    rules: Vec<HardForkConfig>,
}

/// A merged hardfork rule, tagged with the source list(s) it came from.
struct TaggedHardFork {
    rule: HardForkConfig,
    from_user: bool,
    from_shipped: bool,
}

/// Asserts that the rules matching `pred` occupy a contiguous run of `tagged`.
///
/// User and shipped rules may only overlap as a shared, exactly-matching block;
/// if one source's rules were interleaved with the other's, that source's rules
/// would not form a contiguous run, which we reject.
fn assert_contiguous(tagged: &[TaggedHardFork], pred: impl Fn(&TaggedHardFork) -> bool, name: &str) {
    let indices: Vec<usize> = tagged
        .iter()
        .enumerate()
        .filter(|(_, t)| pred(t))
        .map(|(i, _)| i)
        .collect();
    if let (Some(&first), Some(&last)) = (indices.first(), indices.last()) {
        assert!(
            last - first + 1 == indices.len(),
            "{name} hardfork rules interleave with the other source's rules; \
             user-led and shipped hardforks may only overlap as an exactly-matching block",
        );
    }
}

impl HardForkSchedule {
    /// Build the canonical schedule by merging `user_hardforks` (typically from
    /// the node config) with the hardfork rules shipped in the executable
    /// ([`shipped_hardforks`]), then canonicalizing ordering: each
    /// `terminated_finalizers` list is sorted, and the combined list is sorted by
    /// `pow_activation_height`.
    ///
    /// Rules that are identical in every field and appear in both sources are
    /// deduplicated. This **panics** if the two sources conflict, i.e. if they
    /// cannot be combined into a single increasing chain:
    ///
    /// - Two *different* rules at the same `pow_activation_height`, or
    /// - `bft_certificate_height` not strictly increasing in pow-sorted order
    ///   (a duplicate, or an inversion relative to `pow_activation_height`), or
    /// - The two sources interleaving by height. They may only overlap as a
    ///   shared, exactly-matching block (the shipped rules are the assumed past,
    ///   which the user rules may restate exactly and then extend).
    pub fn new(user_hardforks: Vec<HardForkConfig>) -> Self {
        Self::merge(user_hardforks, shipped_hardforks())
    }

    /// Like [`HardForkSchedule::new`], but ignores the rules shipped in the
    /// executable: the schedule is built from `user_hardforks` alone. Lets a
    /// testnet operator specify the entire schedule manually rather than
    /// inheriting the built-in assumed past (see the `disable_shipped_hardforks`
    /// config flag). The same ordering/duplicate validation still applies.
    pub fn new_without_shipped(user_hardforks: Vec<HardForkConfig>) -> Self {
        Self::merge(user_hardforks, Vec::new())
    }

    fn merge(user_hardforks: Vec<HardForkConfig>, shipped: Vec<HardForkConfig>) -> Self {
        let mut user_hardforks = user_hardforks;
        let mut shipped = shipped;

        // Canonicalize each rule's finalizer list so that equality (and thus
        // exact-duplicate detection) is independent of the order finalizers were
        // listed in.
        for hardfork in user_hardforks.iter_mut().chain(shipped.iter_mut()) {
            hardfork.terminated_finalizers.sort();
        }

        // Tag every rule with the source(s) it came from, merging exact
        // duplicates (within a list or across the two lists) as we go.
        let mut tagged: Vec<TaggedHardFork> = Vec::new();
        for (rule, from_user) in user_hardforks
            .iter()
            .map(|rule| (rule, true))
            .chain(shipped.iter().map(|rule| (rule, false)))
        {
            match tagged.iter_mut().find(|t| &t.rule == rule) {
                Some(existing) => {
                    existing.from_user |= from_user;
                    existing.from_shipped |= !from_user;
                }
                None => tagged.push(TaggedHardFork {
                    rule: rule.clone(),
                    from_user,
                    from_shipped: !from_user,
                }),
            }
        }

        // Sort by activation height. Exact duplicates are already merged above.
        tagged.sort_by_key(|t| t.rule.pow_activation_height);

        // Two *different* rules at the same pow height are a conflicting overlap.
        // bft_certificate_height carries the same constraint as pow_activation_height:
        // in this pow-sorted order it must strictly increase, so the two heights share
        // one coherent order and bft_certificate_height contains no duplicates.
        for pair in tagged.windows(2) {
            assert!(
                pair[0].rule.pow_activation_height != pair[1].rule.pow_activation_height,
                "conflicting hardfork rules at pow_activation_height {}: {:?} vs {:?}",
                pair[0].rule.pow_activation_height,
                pair[0].rule,
                pair[1].rule,
            );
            assert!(
                pair[0].rule.bft_certificate_height < pair[1].rule.bft_certificate_height,
                "incoherently ordered hardfork rules: the rule at pow_activation_height {} \
                 has bft_certificate_height {}, but the next rule (pow_activation_height {}) \
                 has bft_certificate_height {}; bft_certificate_height must strictly increase \
                 with pow_activation_height (same order, no duplicates)",
                pair[0].rule.pow_activation_height,
                pair[0].rule.bft_certificate_height,
                pair[1].rule.pow_activation_height,
                pair[1].rule.bft_certificate_height,
            );
        }

        // Neither source's rules may interleave with the other's.
        assert_contiguous(&tagged, |t| t.from_user, "user");
        assert_contiguous(&tagged, |t| t.from_shipped, "shipped");

        Self {
            rules: tagged.into_iter().map(|t| t.rule).collect(),
        }
    }

    /// The hardfork rules, sorted ascending by `pow_activation_height`.
    pub fn rules(&self) -> &[HardForkConfig] {
        &self.rules
    }

    /// The most recent hardfork rule active at `pow_height`, i.e. the rule with
    /// the greatest `pow_activation_height` that is `<= pow_height`, or `None` if
    /// no hardfork has activated by that height.
    pub fn rule_active_at(&self, pow_height: u64) -> Option<&HardForkConfig> {
        // partition_point gives the count of rules with activation <= pow_height.
        let idx = self
            .rules
            .partition_point(|rule| rule.pow_activation_height <= pow_height);
        idx.checked_sub(1).map(|i| &self.rules[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zcash_primitives::bft::PubKeyID;

    fn rule(pow: u64, bft: u64, finalizer: u8) -> HardForkConfig {
        HardForkConfig {
            pow_activation_height: pow,
            bft_certificate_height: bft,
            terminated_finalizers: vec![PubKeyID([finalizer; 32])],
        }
    }

    #[test]
    fn bft_heights_strictly_increasing_is_accepted() {
        let schedule = HardForkSchedule::new(vec![rule(300, 20, 2), rule(150, 10, 1)]);
        // Sorted by pow_activation_height; bft_certificate_height increases with it.
        let bfts: Vec<u64> = schedule.rules().iter().map(|r| r.bft_certificate_height).collect();
        assert_eq!(bfts, vec![10, 20]);
    }

    #[test]
    #[should_panic(expected = "bft_certificate_height must strictly increase")]
    fn duplicate_bft_height_is_rejected() {
        HardForkSchedule::new(vec![rule(150, 10, 1), rule(300, 10, 2)]);
    }

    #[test]
    #[should_panic(expected = "bft_certificate_height must strictly increase")]
    fn inverted_bft_height_is_rejected() {
        // pow_activation_height increases but bft_certificate_height decreases.
        HardForkSchedule::new(vec![rule(150, 20, 1), rule(300, 10, 2)]);
    }
}
