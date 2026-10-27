//! A prefix of the chain: the PoW blocks from genesis to a height, and the decided BFT blocks
//! that go with them, in the order a node would have met them.
//!
//! The finalized database commits blocks and cannot take them back, so the way to return a node
//! to an earlier chain state is to rebuild it from a prefix of the chain it held. This module
//! reads such a prefix out of the state and decides which BFT decisions belong to it; writing it
//! to a file and loading it back are the test format's business (`zebra-crosslink`).

use std::{collections::HashMap, fmt, str::FromStr, sync::Arc};

use zcash_primitives::bft::{BftBlock, FatPointerToBftBlock, SlashForkConfig, TMSig};
use zebra_chain::block::{self, Block, Height};

use crate::service::finalized_state::zebra_db::bft::StoredDecision;

/// Which decided BFT blocks go with a PoW prefix.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BftCutoff {
    /// The BFT chain as the PoW chain sees it: up to and including the newest BFT block that a
    /// PoW block in the prefix cites. Decisions made after that, which no prefix block points at
    /// yet, are left out.
    #[default]
    Cited,
    /// Every decided BFT block whose snapshot is in the prefix: everything a node holding exactly
    /// these PoW blocks could validate. Validation needs the snapshot, not the carried headers,
    /// which may lie on a branch the prefix does not hold.
    Supported,
    /// The first `n` decided BFT blocks, whatever the prefix supports. Any whose snapshot is not
    /// in the prefix are placed after every PoW block, where loading them is refused.
    Count(u32),
    /// No BFT blocks.
    None,
}

impl FromStr for BftCutoff {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "cited" => Ok(BftCutoff::Cited),
            "supported" => Ok(BftCutoff::Supported),
            "none" => Ok(BftCutoff::None),
            _ => s.parse::<u32>().map(BftCutoff::Count).map_err(|_| {
                format!("unknown BFT cutoff {s:?}: expected cited, supported, none, or a count of BFT blocks")
            }),
        }
    }
}

impl fmt::Display for BftCutoff {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BftCutoff::Cited => write!(f, "cited"),
            BftCutoff::Supported => write!(f, "supported"),
            BftCutoff::Count(n) => write!(f, "{n}"),
            BftCutoff::None => write!(f, "none"),
        }
    }
}

/// One decided BFT height, as the database holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BftDecision {
    /// The decided block.
    pub block: BftBlock,
    /// The fat pointer to it: the precommit signatures that decided it.
    pub fat_pointer: FatPointerToBftBlock,
    /// The proposer's signature on each chunk of the proposal, empty if the round had no proposer.
    pub proposal_sigs: Vec<TMSig>,
    /// The valid round the proposal was sent in, which `proposal_sigs` cover; -1 for none.
    pub valid_round: i64,
}

impl From<StoredDecision> for BftDecision {
    /// The database does not store the valid round, so this is -1 until
    /// [`BftDecision::recover_valid_round`] finds the one the signatures were made in.
    fn from(stored: StoredDecision) -> Self {
        BftDecision {
            block: stored.block,
            fat_pointer: stored.fat_pointer,
            proposal_sigs: stored.proposal_sigs,
            valid_round: -1,
        }
    }
}

impl BftDecision {
    /// The valid round the proposal signatures were made in, read back from the signatures.
    ///
    /// The database keeps the signatures but not the valid round they cover, and a decision
    /// loaded with the wrong one is refused. The signatures settle it: they verify under only the
    /// proposer's key, the height's vote namespace and the round they were sent in. The proposer is
    /// almost always among the certificate's signers, the namespace is one of the two this height
    /// can have (with or without FeaturenetUpgrade2's strict quorum), and a valid round is below
    /// the decided round, so the search is small, and at round 0 there is nothing to search.
    ///
    /// `None` when no combination verifies, which leaves a decision whose proposer did not sign the
    /// certificate, decided after round 0, unresolved.
    pub fn recover_valid_round(&self, slashforks: &[SlashForkConfig]) -> Option<i64> {
        use crate::new_network::bft::namespace_for_bft_height;

        let round = self.fat_pointer.get_vote_template().round;
        if self.proposal_sigs.is_empty() || round <= 0 {
            return Some(-1);
        }
        let value = self.block.zcash_serialize_to_vec().ok()?;
        if self.proposal_sigs.len() != tenderlink::proposal_chunks_n(value.len()) {
            return None;
        }
        let height = u64::from(self.block.height);
        let value_id = tenderlink::ValueId(self.fat_pointer.points_at_block_hash().0);
        let namespaces =
            [false, true].map(|strict| namespace_for_bft_height(slashforks, height, strict));
        // A non-negative i32 round fits u32.
        let round_u32 = round as u32;
        (-1..i64::from(round)).find(|&valid_round| {
            namespaces.iter().any(|namespace| {
                self.fat_pointer.signatures.iter().any(|signer| {
                    tenderlink::first_bad_proposal_chunk_sig(
                        signer.pub_key,
                        &value,
                        value_id,
                        height,
                        round_u32,
                        valid_round,
                        namespace,
                        &self.proposal_sigs,
                    )
                    .is_none()
                })
            })
        })
    }
}

/// One step of a prefix, in replay order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChainPrefixItem {
    /// A PoW block, to be submitted as if mined or received.
    Pow(Arc<Block>),
    /// A decided BFT block, to be stored as if tenderlink had decided it.
    Bft(BftDecision),
}

/// The best chain from genesis to some height, and the whole decided BFT chain beside it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainPrefix {
    /// PoW blocks at heights `0..=tip`, each the child of the one before.
    pub blocks: Vec<Arc<Block>>,
    /// Decided BFT blocks at heights `0..`, unfiltered: [`ChainPrefix::items`] picks the ones
    /// that belong with `blocks`.
    pub decisions: Vec<BftDecision>,
}

impl ChainPrefix {
    /// Build a prefix, checking that `blocks` start at genesis and each is its predecessor's
    /// child.
    pub fn new(blocks: Vec<Arc<Block>>, decisions: Vec<BftDecision>) -> Result<Self, String> {
        for (i, block) in blocks.iter().enumerate() {
            // A height that does not fit u32 is not a height in the chain at all.
            let expected = Height(u32::try_from(i).map_err(|_| "too many blocks".to_string())?);
            if block.coinbase_height() != Some(expected) {
                return Err(format!(
                    "block {} has height {:?}, expected {}",
                    block.hash(),
                    block.coinbase_height(),
                    expected.0,
                ));
            }
            if i > 0 && block.header.previous_block_hash != blocks[i - 1].hash() {
                return Err(format!(
                    "block {} at height {i} is not the child of the block before it",
                    block.hash()
                ));
            }
        }
        Ok(ChainPrefix { blocks, decisions })
    }

    /// The prefix's last PoW block.
    pub fn tip(&self) -> Option<(Height, block::Hash)> {
        let tip = self.blocks.last()?;
        Some((tip.coinbase_height()?, tip.hash()))
    }

    /// How many decided BFT blocks `cutoff` keeps.
    pub fn bft_count(&self, cutoff: BftCutoff) -> usize {
        match cutoff {
            BftCutoff::None => 0,
            BftCutoff::Count(n) => self.decisions.len().min(n as usize),
            BftCutoff::Cited => {
                let index = self.decision_index();
                self.blocks
                    .iter()
                    .filter_map(|block| {
                        index.get(
                            &block
                                .header
                                .fat_pointer_to_bft_block
                                .points_at_block_hash()
                                .0,
                        )
                    })
                    .max()
                    .map_or(0, |newest| newest + 1)
            }
            BftCutoff::Supported => {
                let heights = self.pow_heights();
                self.decisions
                    .iter()
                    .take_while(|decision| snapshot_height(&heights, &decision.block).is_some())
                    .count()
            }
        }
    }

    /// The prefix in replay order: every PoW block, with each kept BFT decision placed as early
    /// as a live node could have decided it -- directly after the PoW block that is its newest
    /// carried header, or after its snapshot if those headers are on a branch the prefix does not
    /// hold -- and in any case before the first PoW block citing it. Decisions keep their order.
    pub fn items(&self, cutoff: BftCutoff) -> Vec<ChainPrefixItem> {
        let kept = self.bft_count(cutoff);
        let heights = self.pow_heights();
        let index = self.decision_index();
        let ready: Vec<Option<usize>> = self.decisions[..kept]
            .iter()
            .map(|decision| {
                top_header_height(&heights, &decision.block)
                    .or_else(|| snapshot_height(&heights, &decision.block))
            })
            .collect();

        let mut items = Vec::with_capacity(self.blocks.len() + kept);
        let mut next = 0;
        for (height, block) in self.blocks.iter().enumerate() {
            if let Some(&cited) = index.get(
                &block
                    .header
                    .fat_pointer_to_bft_block
                    .points_at_block_hash()
                    .0,
            ) {
                while next <= cited && next < kept {
                    items.push(ChainPrefixItem::Bft(self.decisions[next].clone()));
                    next += 1;
                }
            }
            items.push(ChainPrefixItem::Pow(block.clone()));
            while next < kept && ready[next].is_some_and(|ready| ready <= height) {
                items.push(ChainPrefixItem::Bft(self.decisions[next].clone()));
                next += 1;
            }
        }
        items.extend(
            self.decisions[next..kept]
                .iter()
                .cloned()
                .map(ChainPrefixItem::Bft),
        );
        items
    }

    fn pow_heights(&self) -> HashMap<[u8; 32], usize> {
        self.blocks
            .iter()
            .enumerate()
            .map(|(height, block)| (block.hash().0, height))
            .collect()
    }

    fn decision_index(&self) -> HashMap<[u8; 32], usize> {
        self.decisions
            .iter()
            .enumerate()
            .map(|(i, decision)| (decision.block.blake3_hash().0, i))
            .collect()
    }
}

/// The height in the prefix of the newest PoW header `block` carries, if it is there.
fn top_header_height(heights: &HashMap<[u8; 32], usize>, block: &BftBlock) -> Option<usize> {
    let top = block.headers.last()?;
    heights
        .get(&zcash_primitives::block::BlockHash::from_header_data(top).0)
        .copied()
}

/// The height in the prefix of the block `block` finalizes, if it is there.
fn snapshot_height(heights: &HashMap<[u8; 32], usize>, block: &BftBlock) -> Option<usize> {
    // A decided block always carries headers; `snapshot_block_hash` panics on one that does not.
    if block.headers.is_empty() {
        return None;
    }
    heights.get(&block.snapshot_block_hash().0).copied()
}

impl crate::ReadStateService {
    /// The best chain from genesis to `max_height` (the best tip if `None`), and the decided BFT
    /// chain from the database.
    ///
    /// The non-finalized state is read once, so the blocks all come from one best chain even if
    /// the node moves on while they are read.
    pub fn chain_prefix(&self, max_height: Option<Height>) -> Result<ChainPrefix, String> {
        let best_chain = self.latest_best_chain();
        let non_finalized = self.latest_non_finalized_state();
        let tip = super::read::best_tip(&non_finalized, &self.db)
            .ok_or_else(|| "the state holds no blocks".to_string())?;
        let height = max_height.unwrap_or(tip.0);
        if height > tip.0 {
            return Err(format!(
                "height {} is above the best tip at {}",
                height.0, tip.0 .0
            ));
        }

        // A u32 height fits in usize on every platform Zebra builds for.
        let mut blocks = Vec::with_capacity(height.0 as usize + 1);
        for h in 0..=height.0 {
            let block = super::read::block(best_chain.as_ref(), &self.db, Height(h).into())
                .ok_or_else(|| format!("no block at height {h}, below the best tip"))?;
            blocks.push(block);
        }
        let slashforks = non_finalized.slashfork_schedule.rules();
        let decisions = self
            .db
            .bft_chain()
            .into_iter()
            .map(|stored| {
                let mut decision = BftDecision::from(stored);
                match decision.recover_valid_round(slashforks) {
                    Some(valid_round) => decision.valid_round = valid_round,
                    None => tracing::warn!(
                        "chain prefix: BFT height {}'s proposal signatures verify under no valid round \
                         with any certificate signer as proposer; writing -1, which a node loading the \
                         file will refuse if it is wrong",
                        decision.block.height,
                    ),
                }
                decision
            })
            .collect();
        ChainPrefix::new(blocks, decisions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zcash_primitives::bft::{
        finalizer_key_from_seed, BftBlockAndFatPointerToIt, FatPointerSignature,
    };

    /// A decision at BFT height 3, decided in `round`, its certificate signed by `signer`.
    fn decision_signed_by(signer: zcash_primitives::bft::PubKeyID, round: u32) -> BftDecision {
        let block = BftBlock {
            version: 1,
            height: 3,
            previous_block_fat_ptr: FatPointerToBftBlock::null(),
            headers: Vec::new(),
            slashforks: Vec::new(),
            do_not_include_until_bc_height: 0,
        };
        let signature = FatPointerSignature {
            pub_key: signer,
            vote_signature: [0; 64],
        };
        let decided = BftBlockAndFatPointerToIt::from_parts(block, 3, round, &[signature]);
        BftDecision {
            block: decided.block,
            fat_pointer: decided.fat_ptr,
            proposal_sigs: Vec::new(),
            valid_round: -1,
        }
    }

    fn sign(
        decision: &mut BftDecision,
        key: &ed25519_zebra::SigningKey,
        round: u32,
        valid_round: i64,
        namespace: &[u8; 32],
    ) {
        let value = decision.block.zcash_serialize_to_vec().unwrap();
        let value_id = tenderlink::ValueId(decision.block.blake3_hash().0);
        decision.proposal_sigs = tenderlink::proposal_chunk_sigs(
            key,
            &value,
            value_id,
            3,
            round,
            valid_round,
            namespace,
        );
    }

    #[test]
    fn the_valid_round_is_read_back_from_the_proposal_signatures() {
        use crate::new_network::bft::namespace_for_bft_height;
        let (_, key, pub_key) = finalizer_key_from_seed(b"chain prefix proposer");
        let (_, other_key, other_pub_key) = finalizer_key_from_seed(b"chain prefix bystander");

        // Re-proposed in round 2 from a value locked in round 1, under either namespace.
        for strict in [false, true] {
            let namespace = namespace_for_bft_height(&[], 3, strict);
            let mut decision = decision_signed_by(pub_key, 2);
            sign(&mut decision, &key, 2, 1, &namespace);
            assert_eq!(
                decision.recover_valid_round(&[]),
                Some(1),
                "strict quorum {strict}"
            );

            // Proposed fresh in round 2.
            sign(&mut decision, &key, 2, -1, &namespace);
            assert_eq!(
                decision.recover_valid_round(&[]),
                Some(-1),
                "strict quorum {strict}"
            );
        }

        // A proposer who did not sign the certificate cannot be found among its signers.
        let mut decision = decision_signed_by(other_pub_key, 2);
        sign(
            &mut decision,
            &key,
            2,
            1,
            &namespace_for_bft_height(&[], 3, false),
        );
        assert_eq!(decision.recover_valid_round(&[]), None);

        // Round 0 has no earlier round to have locked a value in, and no proposer means no search.
        let mut decision = decision_signed_by(pub_key, 0);
        sign(
            &mut decision,
            &other_key,
            0,
            -1,
            &namespace_for_bft_height(&[], 3, false),
        );
        assert_eq!(decision.recover_valid_round(&[]), Some(-1));
        decision.proposal_sigs.clear();
        assert_eq!(decision.recover_valid_round(&[]), Some(-1));
    }

    #[test]
    fn cutoff_parses_what_it_prints() {
        for cutoff in [
            BftCutoff::Cited,
            BftCutoff::Supported,
            BftCutoff::None,
            BftCutoff::Count(7),
        ] {
            assert_eq!(cutoff.to_string().parse::<BftCutoff>(), Ok(cutoff));
        }
        assert!("sometimes".parse::<BftCutoff>().is_err());
    }
}
