//! Chain prefix files: a node's chain from genesis to some height, written as a `.zeccltf` test
//! file and loaded back into a fresh node (#241).
//!
//! The finalized database commits blocks and cannot take them back. A prefix file is the way
//! back: export the chain up to the height to return to, start a node on an empty state, and
//! import the file. The file is ordinary test format -- `SET_PARAMS`, then `LOAD_POW` for every
//! PoW block from genesis and `LOAD_POS` for each decided BFT block, with its proposal signatures,
//! placed directly after the PoW block it was decided on -- so it is readable without a node, can
//! be converted to another
//! node's storage, and can have scenario instructions appended to replay something that
//! happened on the network from the state the network was in.
//!
//! Importing replays the file through the production admission paths: PoW blocks through the
//! same ingest queue `submitblock` uses, BFT blocks as tenderlink decisions. The resulting state
//! is built the way a node following that chain would have built it, `fin` included. `--trusted`
//! keeps those paths but skips the proof, signature and script checks, for files this node
//! exported itself.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Mutex;

use tracing::{error, info, warn};
use zcash_primitives::bft::{BftBlockAndFatPointerToIt, ZcashCrosslinkParameters};
use zebra_state::crosslink::ChainPrefixExport;
use zebra_state::{BftCutoff, BftDecision, ChainPrefix, ChainPrefixItem};

use crate::service::TFLServiceHandle;
use crate::test_format::{self, BftBlockAndFatPointerToItWrap, TF};

/// The test file for `prefix`, with the BFT blocks `cutoff` selects.
pub fn prefix_tf(params: &ZcashCrosslinkParameters, prefix: &ChainPrefix, cutoff: BftCutoff) -> TF {
    let mut tf = TF::new(params);
    for item in prefix.items(cutoff) {
        match item {
            ChainPrefixItem::Pow(block) => tf.push_instr_load_pow(&block, 0),
            ChainPrefixItem::Bft(decision) => push_decision(&mut tf, decision),
        }
    }
    tf
}

/// The one place a decided BFT block enters a prefix file: with the proposer's chunk signatures
/// and the valid round they cover, which a node loading it checks against the round's proposer,
/// as it would a tenderlink decision.
fn push_decision(tf: &mut TF, decision: BftDecision) {
    tf.push_instr_load_pos_signed(
        &BftBlockAndFatPointerToItWrap(BftBlockAndFatPointerToIt {
            block: decision.block,
            fat_ptr: decision.fat_pointer,
        }),
        &decision.proposal_sigs,
        decision.valid_round,
        0,
    );
}

/// Write `prefix` to `path`, with the BFT blocks `cutoff` selects.
pub fn write_prefix_file(
    params: &ZcashCrosslinkParameters,
    prefix: &ChainPrefix,
    cutoff: BftCutoff,
    path: &Path,
) -> Result<ChainPrefixExport, String> {
    let (tip_height, tip_hash) = prefix
        .tip()
        .ok_or_else(|| "the prefix holds no blocks".to_string())?;
    let tf = prefix_tf(params, prefix, cutoff);
    let bytes = tf.write_to_bytes();
    std::fs::write(path, &bytes)
        .map_err(|err| format!("could not write {}: {err}", path.display()))?;
    Ok(ChainPrefixExport {
        path: path.display().to_string(),
        tip_height: tip_height.0,
        tip_hash: tip_hash.to_string(),
        pow_blocks: prefix.blocks.len(),
        bft_blocks: prefix.bft_count(cutoff),
        bft_cutoff: cutoff.to_string(),
        bytes: bytes.len(),
    })
}

/// Where a running node writes the prefix files the `export_chain_prefix` RPC asks for. Set by
/// zebrad at startup; the RPC refuses while it is unset. Callers name a height and a cutoff,
/// never a path, so the RPC cannot be used to write anywhere else.
pub static CHAIN_PREFIX_DIR: Mutex<Option<PathBuf>> = Mutex::new(None);

/// The file name an export of `cutoff` up to `tip` gets in [`CHAIN_PREFIX_DIR`].
pub fn prefix_file_name(
    tip_height: u32,
    tip_hash: &zebra_chain::block::Hash,
    cutoff: BftCutoff,
) -> String {
    format!(
        "chain-prefix-{tip_height}-{}-bft-{cutoff}.zeccltf",
        &tip_hash.to_string()[..16]
    )
}

/// Export the running node's best chain up to `max_height` into [`CHAIN_PREFIX_DIR`].
pub(crate) async fn export_from_node(
    handle: &TFLServiceHandle,
    max_height: Option<zebra_chain::block::Height>,
    cutoff: BftCutoff,
) -> Result<ChainPrefixExport, String> {
    use zebra_state::{ReadRequest, ReadResponse};

    let dir = CHAIN_PREFIX_DIR
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| "this node has no chain prefix directory".to_string())?;
    let prefix = match (handle.call.read_state)(ReadRequest::CrosslinkChainPrefix(max_height)).await
    {
        Ok(ReadResponse::CrosslinkChainPrefix(prefix)) => prefix?,
        Ok(other) => return Err(format!("unexpected state response {other:?}")),
        Err(err) => return Err(format!("state request failed: {err}")),
    };
    let params = handle.params;
    // Serializing every block in the chain is CPU and disk work, kept off the async workers.
    tokio::task::spawn_blocking(move || {
        let (tip_height, tip_hash) = prefix
            .tip()
            .ok_or_else(|| "the prefix holds no blocks".to_string())?;
        std::fs::create_dir_all(&dir)
            .map_err(|err| format!("could not create {}: {err}", dir.display()))?;
        let path = dir.join(prefix_file_name(tip_height.0, &tip_hash, cutoff));
        write_prefix_file(&params, &prefix, cutoff, &path)
    })
    .await
    .map_err(|err| format!("export task failed: {err}"))?
}

/// A prefix file to load into this node once it is running.
#[derive(Clone, Debug)]
pub struct ChainImport {
    pub path: PathBuf,
    /// Skip proof, signature and script verification of the file's PoW blocks.
    pub trusted: bool,
}

/// Set by zebrad before the node starts; taken by the TFL service once it is running.
pub static CHAIN_IMPORT: Mutex<Option<ChainImport>> = Mutex::new(None);

/// Load `import` into the running node, logging what did not go as the file expects.
///
/// Runs the file through the test format's instruction reader, so any instruction a scenario
/// appended after the prefix runs too. Nothing here panics or exits: a failed instruction is
/// logged, and the node keeps running on whatever the file built.
pub(crate) async fn run_import(handle: TFLServiceHandle, import: ChainImport) {
    use zebra_state::{Request, Response};

    let started = std::time::Instant::now();
    loop {
        if let Ok(Response::Tip(Some((height, _)))) = (handle.call.state)(Request::Tip).await {
            if height.0 > 0 {
                warn!(
                    "chain import: the state already holds blocks up to height {}. The file's blocks \
                     on this chain load as already known; the node's fork choice decides any on another \
                     chain. Import into an empty state to rebuild exactly the file's chain.",
                    height.0,
                );
            }
            break;
        }
        if started.elapsed() > std::time::Duration::from_secs(60) {
            error!(
                "chain import: the node has no tip after 60s; not importing {}",
                import.path.display()
            );
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }

    let bytes = match std::fs::read(&import.path) {
        Ok(bytes) => bytes,
        Err(err) => {
            error!(
                "chain import: could not read {}: {err}",
                import.path.display()
            );
            return;
        }
    };
    let tf = match TF::read_from_bytes(&bytes) {
        Ok(tf) => tf,
        Err(err) => {
            error!(
                "chain import: {} is not a test format file: {err}",
                import.path.display()
            );
            return;
        }
    };

    // A failed check is logged rather than fatal, an instruction that does not parse is
    // skipped, and no timing rows are written: this is a node, not a test run.
    *crate::TEST_CHECK_ASSERT.lock().unwrap() = 1;
    *crate::TEST_ON_FAIL.lock().unwrap() = crate::uhh::LOG;
    *crate::TEST_INSTR_C.lock().unwrap() = 0;
    crate::TEST_FAILED_INSTR_IDXS.lock().unwrap().clear();
    crate::test_timing::RECORD.store(false, Relaxed);
    test_format::TRUSTED_LOAD_POW.store(import.trusted, Relaxed);

    info!(
        "chain import: loading {} instructions from {}{}",
        tf.instrs.len(),
        import.path.display(),
        if import.trusted {
            ", trusting its blocks' proofs and signatures"
        } else {
            ""
        },
    );
    let start = std::time::Instant::now();
    test_format::read_instrs(handle.clone(), &bytes, &tf.instrs).await;
    test_format::TRUSTED_LOAD_POW.store(false, Relaxed);

    let tip = match (handle.call.state)(Request::Tip).await {
        Ok(Response::Tip(Some((height, hash)))) => format!("{} ({hash})", height.0),
        _ => "unknown".to_string(),
    };
    let failed = crate::TEST_FAILED_INSTR_IDXS.lock().unwrap().clone();
    if failed.is_empty() {
        info!(
            "chain import: done in {:.1?}; {} instructions, all as the file expects; best tip {tip}",
            start.elapsed(),
            tf.instrs.len(),
        );
    } else {
        warn!(
            "chain import: done in {:.1?}; {} of {} instructions did not do what the file expects; best tip {tip}",
            start.elapsed(),
            failed.len(),
            tf.instrs.len(),
        );
        for (i, message) in failed.iter().take(20) {
            warn!("chain import: instruction {i}: {message}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;
    use test_format::{read_load_pos, tf_read_instr, TestInstr, HARNESS_PARAMETERS};
    use zebra_chain::block::Block;

    /// The PoW blocks and decided BFT blocks in a stored scene, in file order.
    fn scene(name: &str) -> (Vec<Arc<Block>>, Vec<BftDecision>) {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../crosslink-test-data")
            .join(name);
        let (bytes, tf) = TF::read_from_file(&path).expect("the stored scene reads");
        let (mut blocks, mut decisions) = (Vec::new(), Vec::new());
        for instr in &tf.instrs {
            if let Some((block, fat_pointer, proposal_sigs, valid_round)) =
                read_load_pos(&bytes, instr)
            {
                decisions.push(BftDecision {
                    block,
                    fat_pointer,
                    proposal_sigs,
                    valid_round,
                });
            } else if let Some(TestInstr::LoadPoW(block)) = tf_read_instr(&bytes, instr) {
                blocks.push(Arc::new(block));
            }
        }
        (blocks, decisions)
    }

    /// The scene's longest chain, from the regtest genesis it builds on, as a chain prefix. Scenes
    /// hold forks, which a prefix of one node's best chain never does.
    fn scene_prefix(name: &str) -> ChainPrefix {
        let (blocks, decisions) = scene(name);
        let genesis = zebra_chain::block::genesis::regtest_genesis_block();
        let by_hash: HashMap<_, _> = blocks
            .iter()
            .map(|block| (block.hash(), block.clone()))
            .collect();
        let tip = blocks
            .iter()
            .max_by_key(|block| block.coinbase_height())
            .expect("the scene has PoW blocks");
        let mut chain = vec![tip.clone()];
        while let Some(parent) = by_hash.get(&chain.last().unwrap().header.previous_block_hash) {
            chain.push(parent.clone());
        }
        assert_eq!(
            chain.last().unwrap().header.previous_block_hash,
            genesis.hash(),
            "the scene starts on regtest genesis"
        );
        chain.push(genesis);
        chain.reverse();
        ChainPrefix::new(chain, decisions).expect("parent links make one chain")
    }

    /// Stored scenes whose longest chain cites decided BFT blocks.
    const BFT_SCENES: [&str; 3] = [
        "finality_diagram_1_candidate.zeccltf",
        "finality_diagram_2_benign_reorg.zeccltf",
        "finality_diagram_3_conflicting_fork.zeccltf",
    ];

    #[test]
    fn every_decision_follows_its_snapshot_and_precedes_its_citations() {
        for scene_name in BFT_SCENES {
            let prefix = scene_prefix(scene_name);
            assert!(
                prefix.bft_count(BftCutoff::Cited) > 0,
                "{scene_name} cites decided BFT blocks"
            );

            for cutoff in [BftCutoff::Cited, BftCutoff::Supported] {
                let items = prefix.items(cutoff);
                let mut seen_pow: HashMap<[u8; 32], usize> = HashMap::new();
                let mut seen_bft: HashMap<[u8; 32], usize> = HashMap::new();
                for (i, item) in items.iter().enumerate() {
                    match item {
                        ChainPrefixItem::Pow(block) => {
                            let cited = block
                                .header
                                .fat_pointer_to_bft_block
                                .points_at_block_hash()
                                .0;
                            if prefix
                                .decisions
                                .iter()
                                .any(|d| d.block.blake3_hash().0 == cited)
                            {
                                assert!(seen_bft.contains_key(&cited), "{scene_name} {cutoff}: block {} cites a BFT block not yet loaded", block.hash());
                            }
                            seen_pow.insert(block.hash().0, i);
                        }
                        ChainPrefixItem::Bft(decision) => {
                            // What validating it needs: the block it finalizes.
                            let snapshot = decision.block.snapshot_block_hash().0;
                            assert!(
                                seen_pow.contains_key(&snapshot),
                                "{scene_name} {cutoff}: BFT block {} comes before its snapshot",
                                decision.block.height
                            );
                            // And as early as a live node could have decided it: right after its
                            // newest carried header, when that is in the chain.
                            let top = zcash_primitives::block::BlockHash::from_header_data(
                                decision.block.headers.last().unwrap(),
                            )
                            .0;
                            if let Some(&top_i) = seen_pow.get(&top) {
                                assert!(items[top_i + 1..i].iter().all(|item| matches!(item, ChainPrefixItem::Bft(_))),
                                "{scene_name} {cutoff}: BFT block {} waits past PoW blocks after its headers", decision.block.height);
                            }
                            seen_bft.insert(decision.block.blake3_hash().0, i);
                        }
                    }
                }
                assert_eq!(
                    seen_pow.len(),
                    prefix.blocks.len(),
                    "{cutoff}: every PoW block is in the file"
                );
                assert_eq!(
                    seen_bft.len(),
                    prefix.bft_count(cutoff),
                    "{cutoff}: the cutoff's BFT blocks are in the file"
                );
            }
        }
    }

    #[test]
    fn cutoffs_nest() {
        let prefix = scene_prefix(BFT_SCENES[0]);
        let cited = prefix.bft_count(BftCutoff::Cited);
        let supported = prefix.bft_count(BftCutoff::Supported);
        assert!(
            cited <= supported,
            "a cited BFT block has its headers in the prefix"
        );
        assert!(supported <= prefix.decisions.len());
        assert_eq!(prefix.bft_count(BftCutoff::None), 0);
        assert_eq!(
            prefix.bft_count(BftCutoff::Count(2)),
            2.min(prefix.decisions.len())
        );

        // Cutting the PoW chain lower keeps fewer BFT blocks, never more.
        let half = ChainPrefix::new(
            prefix.blocks[..prefix.blocks.len() / 2].to_vec(),
            prefix.decisions.clone(),
        )
        .unwrap();
        assert!(half.bft_count(BftCutoff::Cited) <= cited);
        assert!(half.bft_count(BftCutoff::Supported) <= supported);
    }

    #[test]
    fn a_prefix_file_reads_back_as_the_same_chain() {
        let mut prefix = scene_prefix(BFT_SCENES[0]);
        // The scenes' rosters have no proposer, so they carry no proposal signatures; give one
        // decision a full set, re-proposed in round 2, so the round trip covers them.
        let decision = &mut prefix.decisions[1];
        let chunks_n =
            tenderlink::proposal_chunks_n(decision.block.zcash_serialize_to_vec().unwrap().len());
        decision.proposal_sigs = vec![zcash_primitives::bft::TMSig([7; 64]); chunks_n];
        decision.valid_round = 2;

        let bytes = prefix_tf(&HARNESS_PARAMETERS, &prefix, BftCutoff::Cited).write_to_bytes();
        let tf = TF::read_from_bytes(&bytes).expect("a written prefix reads");

        let mut read = Vec::new();
        for instr in &tf.instrs[1..] {
            if let Some((block, fat_pointer, proposal_sigs, valid_round)) =
                read_load_pos(&bytes, instr)
            {
                read.push(ChainPrefixItem::Bft(BftDecision {
                    block,
                    fat_pointer,
                    proposal_sigs,
                    valid_round,
                }));
                continue;
            }
            match tf_read_instr(&bytes, instr) {
                Some(TestInstr::LoadPoW(block)) => read.push(ChainPrefixItem::Pow(Arc::new(block))),
                other => panic!(
                    "a prefix file holds only loads after SET_PARAMS, not {}",
                    other.is_some()
                ),
            }
        }
        assert_eq!(read, prefix.items(BftCutoff::Cited));
        assert_eq!(
            test_format::crosslink_parameters_for_test(&bytes),
            HARNESS_PARAMETERS
        );
    }

    #[test]
    fn a_prefix_rejects_blocks_that_are_not_one_chain() {
        let prefix = scene_prefix(BFT_SCENES[0]);
        let mut skipped = prefix.blocks.clone();
        skipped.remove(1);
        assert!(ChainPrefix::new(skipped, Vec::new()).is_err());
        assert!(
            ChainPrefix::new(prefix.blocks[1..].to_vec(), Vec::new()).is_err(),
            "a prefix starts at genesis"
        );
    }
}
