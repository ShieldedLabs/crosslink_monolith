//! A prefix of the chain: the PoW blocks from genesis to a height, and the decided BFT blocks
//! that go with them, in the order a node would have met them.
//!
//! The finalized database commits blocks and cannot take them back, so the way to return a node
//! to an earlier chain state is to rebuild it from a prefix of the chain it held. This module
//! reads such a prefix out of the state and decides which BFT decisions belong to it; writing it
//! to a file and loading it back are the test format's business (`zebra-crosslink`).

use std::{collections::HashMap, fmt, str::FromStr, sync::Arc};

use zcash_primitives::bft::{BftBlock, FatPointerToBftBlock, TMSig};
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
    /// The proposer's signature on each chunk of the proposal, empty if none were stored.
    pub proposal_sigs: Vec<TMSig>,
}

impl From<StoredDecision> for BftDecision {
    fn from(stored: StoredDecision) -> Self {
        BftDecision {
            block: stored.block,
            fat_pointer: stored.fat_pointer,
            proposal_sigs: stored.proposal_sigs,
        }
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
        let tip = super::read::best_tip(&self.latest_non_finalized_state(), &self.db)
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
        let decisions = self
            .db
            .bft_chain()
            .into_iter()
            .map(BftDecision::from)
            .collect();
        ChainPrefix::new(blocks, decisions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
