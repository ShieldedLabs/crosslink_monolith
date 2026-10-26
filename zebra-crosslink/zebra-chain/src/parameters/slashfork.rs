//! User-led slashfork rules.
//!
//! Crosslink supports user-led slashforks: rules, supplied in the node config,
//! that activate at a given PoW block height (and BFT certificate height) and
//! terminate a set of finalizers. Over time, user-led slashforks become assumed
//! by the software, so the config rules are merged with a set of rules shipped
//! in the executable (see [`shipped_slashforks`]).
//!
//! The canonical, merged-and-validated set is represented by [`SlashForkSchedule`],
//! which is built once at config load and then shared (read-only) with everything
//! that needs it — zebra-state, zebra-consensus and zebra-crosslink.

// The per-rule type is defined in `zcash_primitives::bft` so that `BftBlock` can
// embed an entry (zebra-chain depends on zcash_primitives, not the reverse). Its
// `terminated_finalizers` are `PubKeyID` finalizer identities, which already
// round-trip in the Bitcoin-style byte order used everywhere else. Config-load
// validation (staking-aligned heights, unique and non-empty finalizers) lives on
// the type in `bft.rs` via serde `deserialize_with`. The scheduling and merge
// logic below stays here, where node config lives.
pub use zcash_primitives::bft::SlashForkConfig;

/// Slashfork rules shipped in the executable.
///
/// User-led slashforks (from the config file) are merged with these, since over
/// time user-led slashforks become assumed by the software.
pub fn shipped_slashforks() -> Vec<SlashForkConfig> {
    // flip hex from config toml order to PubKeyID correct order
    let finalizer_from_hex = |hex_str: &str| {
        let mut bytes = [0u8; 32];
        hex::decode_to_slice(hex_str, &mut bytes).expect("valid 64-char hex");
        bytes.reverse();
        zcash_primitives::bft::PubKeyID(bytes)
    };

    vec![SlashForkConfig {
        // Slash activations must start a staking day; this is 22 prototype staking periods.
        pow_activation_height: 228_096,
        bft_certificate_height: 2158,
        terminated_finalizers: vec![finalizer_from_hex("b8c5272fe6d34980ba6e8aa88d65a6cbf3daabaabea80bdd51a5a5d3bce30c9e")],
    }]
}

/// The canonical, ordered set of slashfork rules in effect for a node.
///
/// Built once via [`SlashForkSchedule::new`] from the user-configured rules merged
/// with [`shipped_slashforks`]. The contained rules are sorted by
/// `pow_activation_height` and each rule's `terminated_finalizers` list is sorted,
/// so the schedule is canonical and ready to be committed to by hash. Because the
/// rules are height-sorted, lookups binary-search.
///
/// Share a single `Arc<SlashForkSchedule>` with everything that needs it rather
/// than copying the `Vec` around.
#[derive(Clone, Debug, Eq, PartialEq, Default)]
pub struct SlashForkSchedule {
    /// Slashfork rules, sorted ascending by `pow_activation_height`.
    rules: Vec<SlashForkConfig>,
}

/// A merged slashfork rule, tagged with the source list(s) it came from.
struct TaggedSlashFork {
    rule: SlashForkConfig,
    from_user: bool,
    from_shipped: bool,
}

/// Asserts that the rules matching `pred` occupy a contiguous run of `tagged`.
///
/// User and shipped rules may only overlap as a shared, exactly-matching block;
/// if one source's rules were interleaved with the other's, that source's rules
/// would not form a contiguous run, which we reject.
fn assert_contiguous(tagged: &[TaggedSlashFork], pred: impl Fn(&TaggedSlashFork) -> bool, name: &str) {
    let indices: Vec<usize> = tagged
        .iter()
        .enumerate()
        .filter(|(_, t)| pred(t))
        .map(|(i, _)| i)
        .collect();
    if let (Some(&first), Some(&last)) = (indices.first(), indices.last()) {
        assert!(
            last - first + 1 == indices.len(),
            "{name} slashfork rules interleave with the other source's rules; \
             user-led and shipped slashforks may only overlap as an exactly-matching block",
        );
    }
}

impl SlashForkSchedule {
    /// Build the canonical schedule by merging `user_slashforks` (typically from
    /// the node config) with the slashfork rules shipped in the executable
    /// ([`shipped_slashforks`]), then canonicalizing ordering: each
    /// `terminated_finalizers` list is sorted, and the combined list is sorted by
    /// `pow_activation_height`.
    ///
    /// Rules that are identical in every field and appear in both sources are
    /// deduplicated. This **panics** if the two sources conflict, i.e. if they
    /// cannot be combined into a single increasing chain:
    ///
    /// - Two *different* rules at the same `pow_activation_height`, or
    /// - `bft_certificate_height` decreasing in pow-sorted order (an inversion
    ///   relative to `pow_activation_height`; duplicates are allowed — several
    ///   rules may share one BFT certificate height), or
    /// - The two sources interleaving by height. They may only overlap as a
    ///   shared, exactly-matching block (the shipped rules are the assumed past,
    ///   which the user rules may restate exactly and then extend).
    pub fn new(user_slashforks: Vec<SlashForkConfig>) -> Self {
        Self::merge(user_slashforks, shipped_slashforks())
    }

    /// Like [`SlashForkSchedule::new`], but ignores the rules shipped in the
    /// executable: the schedule is built from `user_slashforks` alone. Lets a
    /// testnet operator specify the entire schedule manually rather than
    /// inheriting the built-in assumed past (see the `disable_shipped_slashforks`
    /// config flag). The same ordering/duplicate validation still applies.
    pub fn new_without_shipped(user_slashforks: Vec<SlashForkConfig>) -> Self {
        Self::merge(user_slashforks, Vec::new())
    }

    fn merge(user_slashforks: Vec<SlashForkConfig>, shipped: Vec<SlashForkConfig>) -> Self {
        let mut user_slashforks = user_slashforks;
        let mut shipped = shipped;

        // Canonicalize each rule's finalizer list so that equality (and thus
        // exact-duplicate detection) is independent of the order finalizers were
        // listed in.
        for slashfork in user_slashforks.iter_mut().chain(shipped.iter_mut()) {
            slashfork.terminated_finalizers.sort();
        }

        // Tag every rule with the source(s) it came from, merging exact
        // duplicates (within a list or across the two lists) as we go.
        let mut tagged: Vec<TaggedSlashFork> = Vec::new();
        for (rule, from_user) in user_slashforks
            .iter()
            .map(|rule| (rule, true))
            .chain(shipped.iter().map(|rule| (rule, false)))
        {
            match tagged.iter_mut().find(|t| &t.rule == rule) {
                Some(existing) => {
                    existing.from_user |= from_user;
                    existing.from_shipped |= !from_user;
                }
                None => tagged.push(TaggedSlashFork {
                    rule: rule.clone(),
                    from_user,
                    from_shipped: !from_user,
                }),
            }
        }

        // Sort by activation height. Exact duplicates are already merged above.
        tagged.sort_by_key(|t| t.rule.pow_activation_height);

        // Two *different* rules at the same pow height are a conflicting overlap.
        // bft_certificate_height must be non-decreasing in this pow-sorted order, so
        // the two heights share one coherent order. Unlike pow_activation_height,
        // duplicates are allowed: several rules may be certified at the same BFT
        // height (the BFT block then carries them all, in this canonical order).
        for pair in tagged.windows(2) {
            assert!(
                pair[0].rule.pow_activation_height != pair[1].rule.pow_activation_height,
                "conflicting slashfork rules at pow_activation_height {}: {:?} vs {:?}",
                pair[0].rule.pow_activation_height,
                pair[0].rule,
                pair[1].rule,
            );
            assert!(
                pair[0].rule.bft_certificate_height <= pair[1].rule.bft_certificate_height,
                "incoherently ordered slashfork rules: the rule at pow_activation_height {} \
                 has bft_certificate_height {}, but the next rule (pow_activation_height {}) \
                 has bft_certificate_height {}; bft_certificate_height must be non-decreasing \
                 with pow_activation_height",
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

    /// wrap a list that is already canonical; no merging with [`shipped_slashforks`] and no validation
    pub fn from_canonical(rules: Vec<SlashForkConfig>) -> Self {
        debug_assert!(rules.windows(2).all(|pair| {
            pair[0].pow_activation_height < pair[1].pow_activation_height
                && pair[0].bft_certificate_height <= pair[1].bft_certificate_height
        }));
        Self { rules }
    }

    /// The slashfork rules, sorted ascending by `pow_activation_height`.
    pub fn rules(&self) -> &[SlashForkConfig] {
        &self.rules
    }

    /// The most recent slashfork rule active at `pow_height`, i.e. the rule with
    /// the greatest `pow_activation_height` that is `<= pow_height`, or `None` if
    /// no slashfork has activated by that height.
    pub fn rule_active_at(&self, pow_height: u64) -> Option<&SlashForkConfig> {
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

    fn rule(pow: u64, bft: u64, finalizer: u8) -> SlashForkConfig {
        SlashForkConfig {
            pow_activation_height: pow,
            bft_certificate_height: bft,
            terminated_finalizers: vec![PubKeyID([finalizer; 32])],
        }
    }

    #[test]
    fn shipped_slashforks_build_a_valid_schedule() {
        // `merge` validates ordering/duplicates, so this panics if the shipped
        // rules are ever incoherent.
        let schedule = SlashForkSchedule::new(Vec::new());
        assert_eq!(schedule.rules(), shipped_slashforks());
    }

    #[test]
    fn bft_heights_strictly_increasing_is_accepted() {
        let schedule = SlashForkSchedule::new_without_shipped(vec![rule(300, 20, 2), rule(150, 10, 1)]);
        // Sorted by pow_activation_height; bft_certificate_height increases with it.
        let bfts: Vec<u64> = schedule.rules().iter().map(|r| r.bft_certificate_height).collect();
        assert_eq!(bfts, vec![10, 20]);
    }

    #[test]
    fn duplicate_bft_height_is_accepted() {
        // Two rules certified at the same BFT height, distinct pow activations:
        // both survive, in pow order.
        let schedule = SlashForkSchedule::new_without_shipped(vec![rule(300, 10, 2), rule(150, 10, 1)]);
        let heights: Vec<(u64, u64)> = schedule
            .rules()
            .iter()
            .map(|r| (r.pow_activation_height, r.bft_certificate_height))
            .collect();
        assert_eq!(heights, vec![(150, 10), (300, 10)]);
    }

    #[test]
    #[should_panic(expected = "bft_certificate_height must be non-decreasing")]
    fn inverted_bft_height_is_rejected() {
        // pow_activation_height increases but bft_certificate_height decreases.
        SlashForkSchedule::new_without_shipped(vec![rule(150, 20, 1), rule(300, 10, 2)]);
    }
}
