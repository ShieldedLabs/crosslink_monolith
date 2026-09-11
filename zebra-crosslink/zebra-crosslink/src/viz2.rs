
use zebra_gui::{
    BftBlockInspection, BftPowHeaderInspection, BlockInspection, Hash32, PowBlockInspection,
    TxInspection,
};
use zebra_chain::value_balance::ValueBalance;
use std::cmp::max;

use crate::*;

pub fn viz_main(tokio_root_thread_handle: Option<std::thread::JoinHandle<()>>, wallet_state: Arc<Mutex<wallet::WalletState>>) {
    // loop {
    //     if let Some(ref thread_handle) = tokio_root_thread_handle {
    //         if thread_handle.is_finished() {
    //             return;
    //         }
    //     }
    // }

    let test_name: &'static str = *TEST_NAME.lock().unwrap();
    if test_name != "‰‰TEST_NAME_NOT_SET‰‰" {
        *zebra_gui::WINDOW_TITLE.lock().unwrap() = format!("TEST: {}", test_name);
    }

    // @Dev @Debug: detect which instance we are to position viz window
    #[cfg(target_os = "windows")] if zebra_gui::DEV_WIN32_WINDOW_ARRANGEMENT {
        let args: Vec<String> = std::env::args().collect();
        let args_str = args.join(" ");
        let bottom_right = args_str.contains("12302") || args_str.contains("12002") || args_str.contains("_1.local");
        zebra_gui::DEV_WIN32_WINDOW_RIGHT.store(bottom_right, std::sync::atomic::Ordering::Relaxed);
    }

    zebra_gui::main_thread_run_program(wallet_state, false);
}


/// Max best-chain blocks served per response. Bounds the startup burst (a fresh GUI
/// acks 0, which used to request the entire chain). Must comfortably exceed the
/// non-finalized window (~100) so the always-served [finalized tip..tip] span is
/// never cut. History below the window is served on demand as the camera reaches
/// the bottom of loaded coverage (bc_want_below).
const BC_PAGE_SIZE: u64 = 1024;

/// Max BFT blocks served per response: a safety cap on the fat-pointer extent.
/// BFT blocks are served strictly in sync with the PoW blocks served (the extent
/// over their fat pointers, plus BFT_TIP_MARGIN), so this only binds if a single
/// PoW span references an absurd number of BFT blocks.
const BFT_PAGE_SIZE: usize = 2048;

fn pow_inspection(block: &Block) -> BlockInspection {
    use zebra_chain::{transaction::Transaction, transparent};
    BlockInspection::Pow(PowBlockInspection {
        hash: Hash32::from_bytes(block.hash().0),
        height: block.coinbase_height().map(|h| h.0 as u64),
        parent_hash: Hash32::from_bytes(block.header.previous_block_hash.0),
        time: block.header.time.timestamp(),
        fat_pointer: block.header.fat_pointer_to_bft_block.to_string(),
        transactions: block.transactions.iter().map(|tx| TxInspection {
            hash: format!("{}", tx.hash()),
            is_coinbase: matches!(tx.inputs().first(), Some(transparent::Input::Coinbase { .. })),
            staking_action: match tx.as_ref() {
                Transaction::VCrosslink { staking_action: Some(sa), .. } => Some(format!("{sa}")),
                _ => None,
            },
        }).collect(),
        serialized_hex: {
            let mut bytes = Vec::new();
            let _ = block.zcash_serialize(&mut bytes);
            bytes.iter().map(|b| format!("{b:02x}")).collect()
        },
    })
}

fn bft_inspection(b: &wallet::bft::BftBlock) -> BlockInspection {
    BlockInspection::Bft(BftBlockInspection {
        hash: Hash32::from_bytes(b.blake3_hash().0),
        version: b.version,
        height: b.height,
        previous_hash: Hash32::from_bytes(b.previous_block_hash().0),
        finalization_candidate_height: 0,
        do_not_include_until_bc_height: b.do_not_include_until_bc_height,
        hardforks: b.hardforks.iter().map(|hf| zebra_gui::HardforkInspection {
            pow_activation_height: hf.pow_activation_height,
            bft_certificate_height: hf.bft_certificate_height,
            terminated_finalizers: hf.terminated_finalizers.iter().map(|id| Hash32::from_bytes(id.0)).collect(),
        }).collect(),
        pow_headers: b
            .headers
            .iter()
            .enumerate()
            .map(|(i, hdr)| BftPowHeaderInspection {
                height: i as u32,
                hash: Hash32::from_bytes(BlockHash::from_header_data(hdr).0),
            })
            .collect(),
    })
}

fn work_from_difficulty(difficulty: zebra_chain::work::difficulty::CompactDifficulty) -> u64 {
    difficulty.to_work()
        .map(|w| u64::try_from(w.as_u128()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// A chain picture read straight out of a `.zeccltf` file, with no node involved.
///
/// The visualizer's usual source is this node's own state, so it can only draw what this
/// node accepted. Several of the states worth drawing are ones it refuses: a best chain
/// that forks below the finalized block is collapsed by `CrosslinkFinalizeBlock`, and
/// `sidechain_forks` reads only non-finalized state, so nothing of that branch would
/// survive to be served even if it had been admitted. Building the same GUI records from
/// the file's own blocks makes those pictures visible without asking the node to believe
/// them.
///
/// Every LOAD_POW and LOAD_POS in the file is drawn, including ones flagged SHOULD_FAIL:
/// that flag says what the node does with a block, not whether the block can be drawn.
struct VizScene {
    bc_blocks: Vec<zebra_gui::BcBlock>,
    bft_blocks: Vec<zebra_gui::BftBlock>,
    bc_tip_height: u64,
    bc_finalized_tip_height: u64,
    bft_tip_height: u64,
    instr_strings: Vec<String>,
    pow_by_hash: std::collections::HashMap<Hash32, Arc<Block>>,
    bft_by_hash: std::collections::HashMap<Hash32, wallet::bft::BftBlock>,
}

impl VizScene {
    fn from_tf(bytes: &[u8], instrs: &[test_format::TFInstr]) -> VizScene {
        use std::collections::{HashMap, HashSet};

        let mut pow: Vec<Arc<Block>> = Vec::new();
        let mut bft: Vec<wallet::bft::BftBlock> = Vec::new();
        let mut instr_strings: Vec<String> = Vec::new();

        for instr in instrs {
            instr_strings.push(test_format::TFInstr::string_from_instr(bytes, instr));
            match test_format::tf_read_instr(bytes, instr) {
                Some(test_format::TestInstr::LoadPoW(block)) => pow.push(Arc::new(block)),
                Some(test_format::TestInstr::LoadPoS((block, _fat_ptr))) => bft.push(block),
                _ => {}
            }
        }

        // Indexes over just this file. Nothing outside it exists, so a block whose parent
        // is absent is a root and its chain simply starts there.
        let mut pow_by_hash: HashMap<Hash32, Arc<Block>> = HashMap::new();
        let mut height_of: HashMap<Hash32, u64> = HashMap::new();
        for b in &pow {
            let hash = Hash32::from_bytes(b.hash().0);
            if let Some(height) = b.coinbase_height() {
                height_of.insert(hash, height.0 as u64);
            }
            pow_by_hash.insert(hash, b.clone());
        }

        // Accumulated work along each block's own ancestry within the file, so the
        // heaviest chain can be picked the way fork choice picks it. Iterated to a fixed
        // point rather than recursed: the file's blocks come in whatever order the scene
        // was written in, and a parent may be indexed after its child.
        let mut total_work: HashMap<Hash32, u64> = HashMap::new();
        loop {
            let mut settled_one = false;
            for (hash, b) in pow_by_hash.iter() {
                if total_work.contains_key(hash) {
                    continue;
                }
                let parent = Hash32::from_bytes(b.header.previous_block_hash.0);
                let base = if pow_by_hash.contains_key(&parent) {
                    match total_work.get(&parent) {
                        Some(w) => *w,
                        None => continue, // parent not settled yet; next pass
                    }
                } else {
                    0
                };
                total_work.insert(
                    *hash,
                    base + work_from_difficulty(b.header.difficulty_threshold),
                );
                settled_one = true;
            }
            if !settled_one {
                break;
            }
        }

        // Heaviest chain, with height then hash breaking exact ties so one file always
        // draws the same way.
        let mut best_tip: Option<Hash32> = None;
        let mut best_key: (u64, u64, [u8; 32]) = (0, 0, [0; 32]);
        for (hash, work) in total_work.iter() {
            let key = (*work, height_of.get(hash).copied().unwrap_or(0), hash.as_bytes());
            if best_tip.is_none() || key > best_key {
                best_tip = Some(*hash);
                best_key = key;
            }
        }

        let ancestry = |from: Option<Hash32>| -> HashSet<Hash32> {
            let mut set = HashSet::new();
            let mut walk = from;
            while let Some(hash) = walk {
                if !set.insert(hash) {
                    break;
                }
                walk = pow_by_hash
                    .get(&hash)
                    .map(|b| Hash32::from_bytes(b.header.previous_block_hash.0));
            }
            set
        };
        let best_chain = ancestry(best_tip);

        // The finalized marker this node would publish for these blocks: the newest BFT
        // block's `headers[0]`. That is `latest_final_block`'s own derivation, off-by-one
        // and all (FINALITY.md 6.1). The point is to show what this tree does, not what
        // the construction says it should do.
        let bft_tip_block = bft.iter().max_by_key(|b| b.height);
        let finalized_hash = bft_tip_block
            .and_then(|b| b.headers.first())
            .map(|h| Hash32::from_bytes(BlockHash::from_header_data(h).0));
        let finalized_chain = ancestry(finalized_hash);
        let bc_finalized_tip_height = finalized_hash
            .and_then(|h| height_of.get(&h).copied())
            .unwrap_or(0);

        // Which BFT block names each PoW block as its finalization candidate; the newest
        // wins, matching the live path.
        let mut pointed_at_by: HashMap<Hash32, u64> = HashMap::new();
        for b in &bft {
            if let Some(hdr) = b.headers.first() {
                pointed_at_by.insert(
                    Hash32::from_bytes(BlockHash::from_header_data(hdr).0),
                    b.height as u64,
                );
            }
        }

        let hardfork_activation_heights: HashSet<u64> = bft
            .iter()
            .flat_map(|b| b.hardforks.iter().map(|hf| hf.pow_activation_height))
            .collect();

        let mut bc_blocks: Vec<zebra_gui::BcBlock> = Vec::new();
        for b in &pow {
            let this_hash = Hash32::from_bytes(b.hash().0);
            let this_height = height_of.get(&this_hash).copied().unwrap_or(0);
            bc_blocks.push(zebra_gui::BcBlock {
                this_hash,
                parent_hash: Hash32::from_bytes(b.header.previous_block_hash.0),
                this_height,
                txs_n: b.transactions.len(),
                is_best_chain: best_chain.contains(&this_hash),
                is_finalized: finalized_chain.contains(&this_hash),
                knowledge: zebra_gui::BcKnowledge::FullBlock,
                points_at_bft_block: Hash32::from_bytes(
                    b.header.fat_pointer_to_bft_block.points_at_block_hash().0,
                ),
                pointed_at_by_bft_height: pointed_at_by
                    .get(&this_hash)
                    .copied()
                    .unwrap_or(u64::MAX),
                work: work_from_difficulty(b.header.difficulty_threshold),
                utc: b.header.time.timestamp(),
                serialized_size: b.zcash_serialized_size(),
                is_hardfork_activation: hardfork_activation_heights.contains(&this_height),
            });
        }

        let hardfork_bft_heights: HashSet<u64> = bft
            .iter()
            .filter(|b| !b.hardforks.is_empty())
            .map(|b| b.height as u64)
            .collect();
        let mut bft_blocks: Vec<zebra_gui::BftBlock> = Vec::new();
        let mut bft_by_hash: HashMap<Hash32, wallet::bft::BftBlock> = HashMap::new();
        for b in &bft {
            let Some(candidate_hdr) = b.headers.first() else {
                continue;
            };
            let candidate_hash = Hash32::from_bytes(BlockHash::from_header_data(candidate_hdr).0);
            let this_hash = Hash32::from_bytes(b.blake3_hash().0);
            bft_by_hash.insert(this_hash, b.clone());
            bft_blocks.push(zebra_gui::BftBlock {
                this_hash,
                parent_hash: Hash32::from_bytes(b.previous_block_hash().0),
                this_height: b.height as u64,
                points_at_bc_block: candidate_hash,
                points_at_bc_height: height_of.get(&candidate_hash).copied().unwrap_or(0),
                proving_blocks: b
                    .headers
                    .iter()
                    .skip(1)
                    .map(|x| zebra_gui::ProvingHeader {
                        hash: Hash32::from_bytes(BlockHash::from_header_data(x).0),
                        parent_hash: Hash32::from_bytes(x.prev_block.0),
                        utc: x.time as i64,
                        work: work_from_difficulty(
                            zebra_chain::work::difficulty::CompactDifficulty(x.bits),
                        ),
                    })
                    .collect(),
                next_block_is_hardfork: hardfork_bft_heights.contains(&(b.height as u64 + 1)),
            });
        }

        VizScene {
            bc_tip_height: best_tip.and_then(|h| height_of.get(&h).copied()).unwrap_or(0),
            bc_finalized_tip_height,
            bft_tip_height: bft.iter().map(|b| b.height as u64).max().unwrap_or(0),
            bc_blocks,
            bft_blocks,
            instr_strings,
            pow_by_hash,
            bft_by_hash,
        }
    }

    fn response(
        &self,
        request: &zebra_gui::RequestToZebra,
        reset_blocks: bool,
    ) -> zebra_gui::ResponseFromZebra {
        let mut response = zebra_gui::ResponseFromZebra::_0();
        response.view_mode = true;
        response.reset_blocks = reset_blocks;
        // The whole scene every cycle: these files are tens of blocks, so none of the
        // paging the live path needs applies.
        response.bc_blocks = self.bc_blocks.clone();
        response.bft_blocks = self.bft_blocks.clone();
        response.bc_tip_height = self.bc_tip_height;
        response.bc_finalized_tip_height = self.bc_finalized_tip_height;
        response.bft_tip_height = self.bft_tip_height;
        response.start_bc_height = self
            .bc_blocks
            .iter()
            .map(|b| b.this_height)
            .min()
            .unwrap_or(0);
        response.instr_strings = self.instr_strings.clone();
        response.instr_done_n = self.instr_strings.len();

        if request.want_to_inspect_block != Hash32::from_u64(0) {
            if let Some(b) = self.pow_by_hash.get(&request.want_to_inspect_block) {
                response.what_block_it_is = request.want_to_inspect_block;
                response.block_inspection = pow_inspection(b.as_ref());
            } else if let Some(b) = self.bft_by_hash.get(&request.want_to_inspect_block) {
                response.what_block_it_is = request.want_to_inspect_block;
                response.block_inspection = bft_inspection(b);
            }
        }

        response
    }
}

/// Bridge between tokio & viz code
pub async fn service_viz_requests(
    tfl_handle: crate::TFLServiceHandle,
    params: &'static crate::ZcashCrosslinkParameters,
) {
    let call = tfl_handle.clone().call;

    let mut bc_ack_height: u64 = 0;
    let mut skipped_windows_n: u64 = 0;
    let mut instr_strings: Vec<String> = Vec::new();
    // Finalization-candidate info per BFT block, checked exactly once per block:
    // bft_candidate_hashes[i] caches the candidate hash of bft_blocks[i] for all
    // i < bft_checked_n. The fully-checked range only advances past real blocks;
    // an empty-headers placeholder block (see handle_new_decided_bft_block) stalls
    // it until filled in, which is safe because only placeholders are ever
    // overwritten. Candidate heights resolve from the state into
    // bft_candidate_heights; hashes whose lookup failed (candidate not yet synced)
    // wait in bft_unresolved_heights and retry a few per cycle.
    let mut bft_checked_n: usize = 0;
    let mut bft_candidate_hashes: Vec<Hash32> = Vec::new();
    // Inverse of bft_candidate_hashes: candidate PoW hash -> the BFT height pointing
    // at it (latest wins when several share a candidate). Served on each PoW block as
    // pointed_at_by_bft_height, so the GUI can tell that a block it already holds
    // should have a BFT block beside it, and fetch eras its page response missed.
    let mut bft_pointing_heights: std::collections::HashMap<Hash32, u64> = std::collections::HashMap::new();
    let mut bft_candidate_heights: std::collections::HashMap<Hash32, u64> = std::collections::HashMap::new();
    let mut bft_unresolved_heights: std::collections::HashSet<Hash32> = std::collections::HashSet::new();
    let mut bft_resolved_at_tip: u64 = u64::MAX; // PoW tip at the last resolution round
    // While set, the picture comes from a .zeccltf file and the node is not consulted.
    // The two reset flags make the screen start empty on each switch between the sources,
    // since they describe unrelated chains.
    let mut view_scene: Option<VizScene> = None;
    let mut view_reset = false;
    let mut live_reset = false;

    loop {
        let request_queue = zebra_gui::REQUESTS_TO_ZEBRA.lock().unwrap();
        let response_queue = zebra_gui::RESPONSES_FROM_ZEBRA.lock().unwrap();
        if request_queue.is_none() || response_queue.is_none() {
            continue;
        }
        let request_queue = request_queue.as_ref().unwrap();
        let response_queue = response_queue.as_ref().unwrap();

        'main_loop: loop {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;

            // Viewing a file answers from the file alone: the node may be on an unrelated
            // chain, or on none, and asking it anything here would mix the two pictures.
            if view_scene.is_some() {
                let mut leaving = false;
                for _ in 0..256 {
                    let Ok(request) = request_queue.try_recv() else { break };
                    crate::BFT_PAUSE.store(request.bft_pause, std::sync::atomic::Ordering::Relaxed);
                    if request.view_exit {
                        leaving = true;
                        break;
                    }
                    if !request.serialize_instrs_path.is_empty() {
                        info!("not serializing to {} while a file is being viewed: the node's chains are not what is on screen",
                              request.serialize_instrs_path);
                    }
                    if !request.load_instrs_path.is_empty() {
                        info!("not loading {} while a file is being viewed: go back to the live chain first",
                              request.load_instrs_path);
                    }
                    if !request.view_instrs_path.is_empty() {
                        match test_format::TF::read_from_file(std::path::Path::new(&request.view_instrs_path)) {
                            Ok((bytes, tf)) => {
                                view_scene = Some(VizScene::from_tf(&bytes, &tf.instrs));
                                view_reset = true;
                            }
                            Err(err) => error!("failed to view {}: {}", request.view_instrs_path, err),
                        }
                    }
                    if let Some(scene) = view_scene.as_ref() {
                        if response_queue.try_send(scene.response(&request, view_reset)).is_ok() {
                            view_reset = false;
                        }
                    }
                }
                if leaving {
                    view_scene = None;
                    live_reset = true;
                } else {
                    continue 'main_loop;
                }
            }

            let Ok(StateReadResponse::TipPoolValues { value_balance, .. }) = (call.read_state)(StateReadRequest::TipPoolValues).await
            else {
                continue 'main_loop;
            };
            let ironwood_pool_balance = value_balance.ironwood_amount().zatoshis();
            let staking_bonded_pool_balance = value_balance.staking_bonded_amount().zatoshis();
            let staking_unbonded_pool_balance = value_balance.staking_unbonded_amount().zatoshis();
            let finalizer_banks: Vec<([u8; 32], u64)> =
                if let Ok(StateReadResponse::FinalizerRewardBalances(banks)) = (call.read_state)(StateReadRequest::FinalizerRewardBalances).await {
                    banks
                } else {
                    Vec::new()
                };

            let Ok(StateResponse::Tip(Some(tip_height_hash))) = (call.state)(StateRequest::Tip).await
            else {
                continue 'main_loop;
            };
            let bc_tip_height: u64 = tip_height_hash.0.0 as u64;

            let mempool_tx_strings: Vec<String> = if let Ok(MempoolResponse::FullTransactions { transactions, .. }) =
                (call.mempool)(MempoolRequest::FullTransactions).await
            {
                transactions.iter().map(|tx| {
                    use zebra_chain::transaction::Transaction;
                    let txid = tx.transaction.transaction.hash().to_string();
                    let mut s = format!("{}..{}", &txid[..8], &txid[txid.len() - 8..]);
                    if let Transaction::VCrosslink { staking_action: Some(sa), .. } = tx.transaction.transaction.as_ref() {
                        s.push_str(&format!(" {sa}"));
                    }
                    s
                }).collect()
            } else {
                Vec::new()
            };

            // Keep the window covering the whole non-finalized span [finalized tip..tip]:
            // sidechain forks root above the finalized tip and BFT finalization candidates
            // lag the PoW tip, so anchoring both on screen needs these best-chain blocks
            // resent every cycle. Never cut that span out of the window, even when finality lags
            // the PoW tip by more than a page (that lag is exactly what this visualizer
            // should show): the page cap only bounds ack-lag. While finality is still
            // unknown (fresh restart) the ack alone bounds the window; corrects itself
            // on the first decided-block ingest.
            let page_lo = (bc_tip_height + 1).saturating_sub(BC_PAGE_SIZE);
            let finalized_lo = tfl_handle.internal.lock().await.latest_final_block
                .map(|(h, _)| h.0 as u64)
                .unwrap_or(u64::MAX);
            // lo = ack clamped to [page_lo, finalized_lo]; when finality lags below the
            // page floor, the trailing min wins and the window extends down to it,
            // bounded at a few pages: a restart mid-finality-catch-up can leave
            // latest_final_block a whole chain below the tip, and an unbounded window
            // would then serve everything down to ~0 every cycle.
            let sanity_lo = (bc_tip_height + 1).saturating_sub(4 * BC_PAGE_SIZE);
            let req_lo_height = ZebBlockHeight(bc_ack_height.max(page_lo).min(finalized_lo).max(sanity_lo).min(bc_tip_height) as u32);

            // Anchored on the same tip hash this response reports, so every block in it
            // belongs to the one chain the GUI is being told about. A tip that flipped in
            // the last few milliseconds leaves this one cycle stale, and self-corrects.
            let window_len = (bc_tip_height - req_lo_height.0 as u64 + 1) as u32;
            let seq_blocks = tfl_block_sequence(&call, tip_height_hash.1, tip_height_hash.0, req_lo_height, window_len).await;
            if seq_blocks.is_empty() {
                // The anchor left the state between the tip read and this one. Ordinary
                // during a reorganization; kept visible, and quiet, in case it is not.
                skipped_windows_n += 1;
                if skipped_windows_n == 1 || skipped_windows_n % 1000 == 0 {
                    info!("no block sequence for [{}..{}], {} skipped", req_lo_height.0, bc_tip_height, skipped_windows_n);
                }
                continue 'main_loop;
            }
            skipped_windows_n = 0;
            let lo_height = seq_blocks[0].0;


            // Blocks peers claim to have (from the new_network STATUS exchange) that we
            // don't: shown by the GUI as PeerAttested at their claimed heights. The sync
            // loop filters against our near-tip chains when publishing (see
            // PEER_ATTESTED_BLOCKS for the deep no-overlap caveat), and the GUI drops
            // claims for blocks already on screen; same data serves every response below.
            let bc_attested: Vec<(Hash32, Hash32, u64)> = zebra_state::new_network::PEER_ATTESTED_BLOCKS
                .lock()
                .unwrap()
                .iter()
                .map(|sb| (Hash32::from_bytes(sb.this_hash.0), Hash32::from_bytes(sb.parent_hash.0), sb.this_height as u64))
                .collect();

            // Advance the fully-checked range of BFT blocks: each block's finalization-
            // candidate hash is computed exactly once, then never re-checked. The
            // internal lock is held only for the scan; state lookups await outside it.
            // Steady-state this touches nothing but newly decided blocks.
            {
                {
                    let internal = tfl_handle.internal.lock().await;
                    // bounded per cycle so a bulk load (PoS store replay) doesn't hash the
                    // whole chain under one lock hold
                    let scan_end = (bft_checked_n + 4096).min(internal.bft_blocks.len());
                    while bft_checked_n < scan_end {
                        let b = &internal.bft_blocks[bft_checked_n];
                        if b.headers.is_empty() { break; } // placeholder: recheck once filled
                        let hash = Hash32::from_bytes(BlockHash::from_header_data(b.finalization_candidate()).0);
                        bft_candidate_hashes.push(hash);
                        bft_pointing_heights.insert(hash, bft_checked_n as u64);
                        if !bft_candidate_heights.contains_key(&hash) {
                            bft_unresolved_heights.insert(hash);
                        }
                        bft_checked_n += 1;
                    }
                }
                // retry a bounded batch; the set drains as candidates sync into the
                // state. Only when the chain has grown since the last round: an
                // unresolved candidate can only become resolvable when new blocks
                // arrive, so retrying against an unchanged chain is pure cost
                // (PoW catch-up used to pay hundreds of doomed lookups per cycle).
                if !bft_unresolved_heights.is_empty() && bc_tip_height != bft_resolved_at_tip {
                    bft_resolved_at_tip = bc_tip_height;
                    let pending: Vec<Hash32> = bft_unresolved_heights.iter().copied().take(256).collect();
                    for hash in pending {
                        if let Ok(StateResponse::BlockHeader { height, .. }) =
                            (call.state)(StateRequest::BlockHeader(zebra_state::HashOrHeight::Hash(ZebBlockHash(hash.as_bytes()).into()))).await
                        {
                            bft_candidate_heights.insert(hash, height.0 as u64);
                            bft_unresolved_heights.remove(&hash);
                        }
                    }
                }
            }

            // Finality-frontier page: when finality lags more than the window's sanity
            // bound below the tip (BFT catch-up), the finalization-candidate region falls
            // out of the served window and the GUI can only show it as header ghosts and
            // peer claims. Serve one page of real best-chain blocks up from the finalized
            // tip so the region the BFT chain points at stays real; it chases the frontier
            // upward as finality catches up, and disappears once the window covers it.
            // Anchored on the same tip hash as the window, like the backfill page below.
            let mut frontier_blocks: Vec<(ZebBlockHeight, ZebBlockHash, Arc<Block>)> = Vec::new();
            if finalized_lo < sanity_lo {
                let lo_h = ZebBlockHeight(finalized_lo as u32);
                let hi_h = ZebBlockHeight((finalized_lo + BC_PAGE_SIZE - 1).min(sanity_lo - 1) as u32);
                frontier_blocks = tfl_block_sequence(&call, tip_height_hash.1, hi_h, lo_h, BC_PAGE_SIZE as u32).await;
            }

            for _ in 0..256 {
                if let Ok(request) = request_queue.try_recv() {
                    crate::BFT_PAUSE.store(request.bft_pause, std::sync::atomic::Ordering::Relaxed);

                    if !request.load_instrs_path.is_empty() {
                        match test_format::TF::read_from_file(std::path::Path::new(&request.load_instrs_path)) {
                            Ok((bytes, tf)) => {
                                instr_strings = tf.instrs.iter()
                                    .map(|instr| test_format::TFInstr::string_from_instr(&bytes, instr))
                                    .collect();
                                *TEST_INSTR_C.lock().unwrap() = 0;
                                TEST_FAILED_INSTR_IDXS.lock().unwrap().clear();
                                let handle = tfl_handle.clone();
                                tokio::task::spawn(async move {
                                    test_format::read_instrs(handle, &bytes, &tf.instrs).await;
                                });
                            }
                            Err(err) => {
                                instr_strings = vec![format!("Failed to load {}: {}", request.load_instrs_path, err)];
                            }
                        }
                    }

                    if !request.view_instrs_path.is_empty() {
                        match test_format::TF::read_from_file(std::path::Path::new(&request.view_instrs_path)) {
                            Ok((bytes, tf)) => {
                                view_scene = Some(VizScene::from_tf(&bytes, &tf.instrs));
                                view_reset = true;
                                continue 'main_loop;
                            }
                            Err(err) => error!("failed to view {}: {}", request.view_instrs_path, err),
                        }
                    }

                    if !request.serialize_instrs_path.is_empty() {
                        let handle = tfl_handle.clone();
                        let ser_call = call.clone();
                        let path_string = request.serialize_instrs_path.clone();
                        tokio::task::spawn(async move {
                            let Ok(StateResponse::Tip(Some(tip))) = (ser_call.state)(StateRequest::Tip).await else { return; };

                            // The whole chain, paged so no single response has to hold it.
                            // Each page is anchored on the previous page's parent hash, so
                            // consecutive pages join into one chain by construction; a page
                            // coming back short means the chain moved and the file would be
                            // missing its base, so abandon it rather than write a gap.
                            const SER_PAGE_SIZE: u32 = 8192;
                            let mut pages: Vec<Vec<(ZebBlockHeight, ZebBlockHash, Arc<Block>)>> = Vec::new();
                            let (mut hi_hash, mut hi_height) = (tip.1, tip.0);
                            loop {
                                let page = tfl_block_sequence(&ser_call, hi_hash, hi_height, ZebBlockHeight(1), SER_PAGE_SIZE).await;
                                let Some((lo_height, _, lowest)) = page.first().cloned() else {
                                    info!("serialization abandoned: the chain moved while paging");
                                    return;
                                };
                                hi_hash = lowest.header.previous_block_hash;
                                hi_height = ZebBlockHeight(lo_height.0.saturating_sub(1));
                                pages.push(page);
                                if lo_height <= ZebBlockHeight(1) { break; }
                            }
                            pages.reverse();
                            let blocks: Vec<Arc<Block>> = pages.into_iter().flatten().map(|(_, _, block)| block).collect();

                            let (bft_blocks, fat_pointer_to_tip) = {
                                let internal = handle.internal.lock().await;
                                (internal.bft_blocks.clone(), internal.fat_pointer_to_tip.clone())
                            };
                            // The signed fat pointer to BFT block i rides in block i+1; the tip's rides alone.
                            let fat_ptr_to = |i: usize| {
                                if i + 1 < bft_blocks.len() { bft_blocks[i + 1].previous_block_fat_ptr.clone() }
                                else { fat_pointer_to_tip.clone() }
                            };
                            let bft_hashes: Vec<_> = bft_blocks.iter().map(|b| b.blake3_hash()).collect();

                            let mut tf = test_format::TF::new(params);
                            let mut next_bft = 0usize;
                            // Each BFT block goes just before the first PoW block that commits to it,
                            // preserving the chronology a replay needs.
                            for block in blocks.iter() {
                                let target = block.header.fat_pointer_to_bft_block.points_at_block_hash();
                                if let Some(j) = bft_hashes.iter().position(|h| *h == target) {
                                    while next_bft <= j {
                                        tf.push_instr_load_pos(&test_format::BftBlockAndFatPointerToItWrap(
                                            zcash_primitives::bft::BftBlockAndFatPointerToIt {
                                                block: bft_blocks[next_bft].clone(),
                                                fat_ptr: fat_ptr_to(next_bft),
                                            }), 0);
                                        next_bft += 1;
                                    }
                                }
                                tf.push_instr_load_pow(block.as_ref(), 0);
                            }
                            while next_bft < bft_blocks.len() {
                                tf.push_instr_load_pos(&test_format::BftBlockAndFatPointerToItWrap(
                                    zcash_primitives::bft::BftBlockAndFatPointerToIt {
                                        block: bft_blocks[next_bft].clone(),
                                        fat_ptr: fat_ptr_to(next_bft),
                                    }), 0);
                                next_bft += 1;
                            }

                            let ok = tf.write_to_file(std::path::Path::new(&path_string));
                            info!("serialized {} instructions to {}: ok={}", tf.instrs.len(), path_string, ok);
                        });
                    }

                    // Backfill: the GUI can see the bottom of its loaded chain; serve the next
                    // page of older blocks so coverage extends downward contiguously.
                    // Fetched before taking the internal lock since state calls await.
                    //
                    // Anchored on the same tip hash as the window above, so the two pages are
                    // from one chain and join. If that tip is gone by now this comes back empty
                    // and the GUI asks again, which beats splicing in a page from elsewhere.
                    let mut backfill_blocks: Vec<(ZebBlockHeight, ZebBlockHash, Arc<Block>)> = Vec::new();
                    if request.bc_want_below > 0 && request.bc_want_below <= bc_tip_height {
                        let hi_h = ZebBlockHeight((request.bc_want_below - 1) as u32);
                        let lo_h = ZebBlockHeight(hi_h.0.saturating_sub(BC_PAGE_SIZE as u32 - 1));
                        backfill_blocks = tfl_block_sequence(&call, tip_height_hash.1, hi_h, lo_h, BC_PAGE_SIZE as u32).await;
                    }

                    // PoS jump: the GUI wants a BFT height whose era may be nowhere near the
                    // PoW blocks otherwise served. Serve the PoW page around its finalization
                    // candidate; the jump extent below then pulls the era's BFT blocks in via
                    // this page's fat pointers. A candidate whose height hasn't resolved yet
                    // serves nothing and the GUI re-asks. Anchored on the tip hash like the
                    // pages above.
                    let mut pos_jump_blocks: Vec<(ZebBlockHeight, ZebBlockHash, Arc<Block>)> = Vec::new();
                    if request.bft_want_height != u64::MAX {
                        let candidate_height = bft_candidate_hashes.get(request.bft_want_height as usize)
                            .and_then(|hash| bft_candidate_heights.get(hash))
                            .copied();
                        if let Some(ch) = candidate_height {
                            // a little above the candidate, so the blocks whose fat pointers
                            // name the target and its successors ride along
                            let hi_h = ZebBlockHeight((ch + 64).min(bc_tip_height) as u32);
                            let lo_h = ZebBlockHeight(hi_h.0.saturating_sub(BC_PAGE_SIZE as u32 - 1));
                            pos_jump_blocks = tfl_block_sequence(&call, tip_height_hash.1, hi_h, lo_h, BC_PAGE_SIZE as u32).await;
                        }
                    }

                    // Forks alongside the best chain, each read as its own sequence anchored on
                    // its tip: a real branch with real heights whose lowest block is the child
                    // of a best-chain block in the same response. A fork rooted below the page
                    // cap (finality stalled by more than a page) is served from its tip down and
                    // renders unattached, like anything else below the window. Overlaps — forks
                    // of forks, or a reorganization between the window read and this one — send
                    // a block twice; the GUI keys blocks by hash and merges, so no dedup here.
                    let mut fork_blocks: Vec<(ZebBlockHeight, ZebBlockHash, Arc<Block>)> = Vec::new();
                    if let Ok(StateReadResponse::SidechainForks(forks)) =
                        (call.read_state)(StateReadRequest::SidechainForks).await
                    {
                        for fork in forks {
                            fork_blocks.extend(tfl_block_sequence(
                                &call, fork.tip_hash, fork.tip_height, fork.fork_height, BC_PAGE_SIZE as u32,
                            ).await);
                        }
                    }

                    let mut internal = tfl_handle.internal.lock().await;
                    let mut response = zebra_gui::ResponseFromZebra::_0();
                    response.reset_blocks = live_reset;
                    live_reset = false;
                    response.bc_attested = bc_attested.clone();
                    response.bft_recency = internal.recency_status.clone(); // TODO: do we want a better way of communicating singleton data
                    {
                        // Terminated finalizers, derived the same way tenderlink filters its roster:
                        // a pure function of the hardfork schedule at the current working height (the
                        // next block to decide) and the current finalized BC height. Identical source
                        // means the viz display and the actual consensus roster always agree.
                        let working_bft_height = internal.bft_blocks.len() as u64;
                        let finalized_bc_height = internal.latest_final_block.map(|(h, _)| h.0 as u64).unwrap_or(0);
                        response.blacklisted_finalizers = crate::terminated_finalizers_at(
                            &tfl_handle.config.hardforks, working_bft_height, finalized_bc_height,
                        )
                        .iter()
                        .map(|pk| Hash32::from_bytes(pk.0))
                        .collect();
                    }
                    response.bc_tip_height = bc_tip_height;
                    response.bc_finalized_tip_height = if let Some(latest_finalized_block) = internal.latest_final_block {
                        latest_finalized_block.0.0 as u64
                    } else {
                        0
                    };
                    response.bft_tip_height = (internal.bft_blocks.len() as u64).saturating_sub(1);
                    response.peer_strings = internal.peer_strings.clone();
                    response.pow_peer_count = zebra_state::new_network::POW_PEER_COUNT
                        .load(std::sync::atomic::Ordering::Relaxed);
                    response.mempool_tx_strings = mempool_tx_strings.clone();
                    response.pos_tip_signers = internal.fat_pointer_to_tip.signatures.iter()
                        .map(|sig| Hash32::from_bytes(sig.pub_key.0))
                        .collect();
                    response.instr_strings = instr_strings.clone();
                    response.instr_done_n = *TEST_INSTR_C.lock().unwrap();
                    response.instr_failed = TEST_FAILED_INSTR_IDXS.lock().unwrap().clone();

                    response.ironwood_pool_balance = ironwood_pool_balance;
                    response.staking_bonded_pool_balance = staking_bonded_pool_balance;
                    response.staking_unbonded_pool_balance = staking_unbonded_pool_balance;
                    response.finalizer_banks = finalizer_banks.clone();

                    response.start_bc_height = lo_height.0 as u64; // actual window start, may be below ack
                    // Clamped to the tip: the GUI derives its ack from on-screen block
                    // heights, and a bogus one above the tip would otherwise collapse the
                    // window onto the tip block itself.
                    bc_ack_height = bc_ack_height.max(request.bc_ack_height).min(bc_tip_height);

                    let push_bc_block = |response: &mut zebra_gui::ResponseFromZebra,
                                         height: &ZebBlockHeight,
                                         hash: &ZebBlockHash,
                                         bc: &Block,
                                         is_best_chain: bool| {
                        let this_hash = Hash32::from_bytes(hash.0);
                        if request.want_to_inspect_block == this_hash {
                            response.what_block_it_is = this_hash;
                            response.block_inspection = pow_inspection(bc);
                        }
                        // Whether the picture calls a block finalized is settled here rather
                        // than by the drawing code, which cannot express the case that matters:
                        // a finalized block that is not on the best chain. This node never holds
                        // that state, but a viewed file can.
                        let is_finalized =
                            is_best_chain && height.0 as u64 <= response.bc_finalized_tip_height;
                        response.bc_blocks.push(zebra_gui::BcBlock {
                            this_hash,
                            parent_hash: Hash32::from_bytes(bc.header.previous_block_hash.0),
                            this_height: height.0 as u64,
                            txs_n: bc.transactions.len(),
                            is_best_chain,
                            is_finalized,
                            knowledge: zebra_gui::BcKnowledge::FullBlock,
                            points_at_bft_block: Hash32::from_bytes(bc.header.fat_pointer_to_bft_block.points_at_block_hash().0),
                            pointed_at_by_bft_height: bft_pointing_heights.get(&this_hash).copied().unwrap_or(u64::MAX),
                            work: bc.header.difficulty_threshold.to_work()
                                .map(|w| u64::try_from(w.as_u128()).unwrap_or(u64::MAX))
                                .unwrap_or(0xdeadbeef),
                            utc: bc.header.time.timestamp(),
                            serialized_size: bc.zcash_serialized_size(),
                            // Flag this block when it sits at a hardfork's PoW activation height.
                            // Many forks may share that height; each is flagged independently.
                            is_hardfork_activation: tfl_handle.config.hardforks.iter()
                                .any(|hf| hf.pow_activation_height == height.0 as u64),
                        });
                    };

                    for (height, hash, bc) in seq_blocks.iter() {
                        push_bc_block(&mut response, height, hash, bc, true);
                    }
                    // backfill page (older best-chain blocks the GUI's camera wants),
                    // finality-frontier page (real blocks where the BFT candidates point),
                    // and PoS-jump page (blocks around a wanted BFT era's candidate)
                    for (height, hash, bc) in backfill_blocks.iter().chain(frontier_blocks.iter()).chain(pos_jump_blocks.iter()) {
                        push_bc_block(&mut response, height, hash, bc, true);
                    }
                    for (height, hash, bc) in fork_blocks.iter() {
                        push_bc_block(&mut response, height, hash, bc, false);
                    }

                    // BFT blocks strictly in sync with the PoW blocks served: each PoW block's
                    // fat pointer names a BFT block, so the [min..max] index extent over a
                    // served PoW span covers exactly the BFT blocks decided over it, plus a
                    // small margin above the newest pointer so just-decided blocks (not yet
                    // referenced by any PoW block) still appear. Two extents, not one: the
                    // near-tip extent (window + frontier pages) and the jump extent (backfill +
                    // PoS-jump pages, which may sit arbitrarily deep below the tip). A single
                    // extent over both spans would cover the whole index range in between, and
                    // its page cap — which cuts the oldest indices — would cut exactly the era
                    // that was jumped to. Nothing else is served: a BFT catch-up burst past
                    // the margin stays hidden until PoW blocks referencing it exist.
                    let bft_hash_to_height = &internal.bft_block_hash_to_height;
                    let extent_over = |lists: &[&Vec<(ZebBlockHeight, ZebBlockHash, Arc<Block>)>]| -> Option<(usize, usize)> {
                        let mut extent: Option<(usize, usize)> = None;
                        for list in lists {
                            for (_, _, bc) in list.iter() {
                                let ptr = bc.header.fat_pointer_to_bft_block.points_at_block_hash();
                                if let Some(&h) = bft_hash_to_height.get(&ptr) {
                                    let h = h as usize;
                                    extent = Some(match extent {
                                        None => (h, h),
                                        Some((lo, hi)) => (lo.min(h), hi.max(h)),
                                    });
                                }
                            }
                        }
                        extent
                    };

                    const BFT_TIP_MARGIN: usize = 16;
                    // No served PoW block names any BFT block yet (fat pointers are null
                    // until the first decided block gets referenced, e.g. a fresh chain):
                    // the just-decided tail is still news, same as the margin above the
                    // newest pointer.
                    let near_tip_extent = extent_over(&[&seq_blocks, &frontier_blocks]).or_else(|| {
                        let n = internal.bft_blocks.len();
                        (n > 0).then(|| (n.saturating_sub(BFT_TIP_MARGIN), n - 1))
                    });
                    let mut jump_extent = extent_over(&[&backfill_blocks, &pos_jump_blocks]);
                    // the asked-for BFT block itself, in case the era's fat pointers just miss it
                    if request.bft_want_height != u64::MAX && (request.bft_want_height as usize) < internal.bft_blocks.len() {
                        let want = request.bft_want_height as usize;
                        jump_extent = Some(match jump_extent {
                            None => (want, want),
                            Some((lo, hi)) => (lo.min(want), hi.max(want)),
                        });
                    }
                    let mut bft_indices: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
                    for extent in [near_tip_extent, jump_extent] {
                        if let Some((lo, hi)) = extent {
                            // each extent's page cap cuts its oldest indices, never the newest:
                            // under pressure (catch-up, deep scrolling) the tip lane must keep
                            // moving, and a jumped-to era must show the era that was asked for
                            let hi = (hi + BFT_TIP_MARGIN).min(internal.bft_blocks.len().saturating_sub(1));
                            let lo = lo.max((hi + 1).saturating_sub(BFT_PAGE_SIZE));
                            bft_indices.extend(lo..=hi);
                        }
                    }

                    for i in bft_indices {
                        let b = &internal.bft_blocks[i];
                        // Out-of-order BFT ingest pads the chain with empty-headers placeholder
                        // blocks; nothing to show until the real block arrives.
                        if b.headers.is_empty() { continue; }
                        // cached for the fully-checked range; blocks past a stalled
                        // placeholder (catch-up) compute on the fly until checked
                        let candidate_hash = bft_candidate_hashes.get(i).copied().unwrap_or_else(||
                            Hash32::from_bytes(BlockHash::from_header_data(b.finalization_candidate()).0));
                        // past a stalled placeholder the fully-checked scan hasn't seen this
                        // hash, so enqueue it here: resolution must still learn its height
                        // or the block positions at 0 forever
                        if !bft_candidate_heights.contains_key(&candidate_hash) {
                            bft_unresolved_heights.insert(candidate_hash);
                        }
                        // extent membership already proves this block's PoW span is served;
                        // the height is for GUI positioning only (0 = not yet resolved)
                        let candidate_height = bft_candidate_heights.get(&candidate_hash).copied().unwrap_or(0);
                        let this_hash = Hash32::from_bytes(b.blake3_hash().0);
                        if request.want_to_inspect_block == this_hash {
                            response.what_block_it_is = this_hash;
                            response.block_inspection = bft_inspection(b);
                        }
                        response.bft_blocks.push(zebra_gui::BftBlock {
                            this_hash: this_hash,
                            parent_hash: Hash32::from_bytes(b.previous_block_hash().0),
                            this_height: i as u64,
                            points_at_bc_block: candidate_hash,
                            points_at_bc_height: candidate_height,
                            // full header data, so the GUI can show proven blocks it never received
                            proving_blocks: b.headers.iter().skip(1).map(|x| zebra_gui::ProvingHeader {
                                hash: Hash32::from_bytes(BlockHash::from_header_data(x).0),
                                parent_hash: Hash32::from_bytes(x.prev_block.0),
                                utc: x.time as i64,
                                work: work_from_difficulty(zebra_chain::work::difficulty::CompactDifficulty(x.bits)),
                            }).collect(),
                            // Foreknowledge from the hardfork schedule (known at startup, not from
                            // the next block): flag this block when a hardfork activates at the next
                            // BFT height, so the GUI can warn before the hardfork block exists.
                            next_block_is_hardfork: tfl_handle.config.hardforks.iter()
                                .any(|hf| hf.bft_certificate_height == i as u64 + 1),
                        });

                        // TODO: compute the finalized tip height!
                    };
                    
                    // NOTE(Giovanni): fallback to find the block in the BC and BFT chains.
                    if response.what_block_it_is == Hash32::from_u64(0)
                    && request.want_to_inspect_block != Hash32::from_u64(0)
                    {
                        let want = request.want_to_inspect_block;
                        drop(internal);
                        let hash = ZebBlockHash(want.as_bytes());
                        if let Some(bc) = crate::block_from_hash(&call, hash).await {
                            response.what_block_it_is = want;
                            response.block_inspection = pow_inspection(bc.as_ref());
                        } else {
                            let internal = tfl_handle.internal.lock().await;
                            for b in internal.bft_blocks.iter() {
                                let this_hash = Hash32::from_bytes(b.blake3_hash().0);
                                if want == this_hash {
                                    response.what_block_it_is = want;
                                    response.block_inspection = bft_inspection(b);
                                    break;
                                }
                            }
                        }
                    }

                    let _ = response_queue.try_send(response);
                } else {
                    continue 'main_loop;
                }
            }
        }
    }
}

#[cfg(test)]
mod scene_tests {
    use super::*;

    /// The scenes are written by `crosslink_write_finality_diagram_scenes` in zebrad's
    /// `tests/crosslink.rs` and committed beside the other test-format data.
    fn scene(name: &str) -> VizScene {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../crosslink-test-data")
            .join(name);
        let (bytes, tf) = test_format::TF::read_from_file(&path)
            .unwrap_or_else(|err| panic!("reading {}: {}", path.display(), err));
        VizScene::from_tf(&bytes, &tf.instrs)
    }

    fn at_height(scene: &VizScene, height: u64) -> Vec<&zebra_gui::BcBlock> {
        scene.bc_blocks.iter().filter(|b| b.this_height == height).collect()
    }

    fn best_heights(scene: &VizScene) -> Vec<u64> {
        let mut heights: Vec<u64> = scene
            .bc_blocks
            .iter()
            .filter(|b| b.is_best_chain)
            .map(|b| b.this_height)
            .collect();
        heights.sort_unstable();
        heights
    }

    fn finalized_heights(scene: &VizScene) -> Vec<u64> {
        let mut heights: Vec<u64> = scene
            .bc_blocks
            .iter()
            .filter(|b| b.is_finalized)
            .map(|b| b.this_height)
            .collect();
        heights.sort_unstable();
        heights
    }

    #[test]
    fn diagram_scene_1_puts_the_marker_on_p5() {
        let scene = scene("finality_diagram_1_candidate.zeccltf");

        assert_eq!(scene.bc_blocks.len(), 10);
        assert_eq!(scene.bc_tip_height, 10);
        assert_eq!(best_heights(&scene), (1..=10).collect::<Vec<_>>());

        assert_eq!(scene.bft_blocks.len(), 3);
        assert_eq!(scene.bft_tip_height, 2);

        // The diagram's fin.
        assert_eq!(scene.bc_finalized_tip_height, 5);
        assert_eq!(finalized_heights(&scene), (1..=5).collect::<Vec<_>>());

        // Each BFT block's finalization candidate is the deepest header of its window:
        // P3, P4, P5 for bft0, bft1, bft2.
        let mut by_height = scene.bft_blocks.clone();
        by_height.sort_by_key(|b| b.this_height);
        assert_eq!(
            by_height.iter().map(|b| b.points_at_bc_height).collect::<Vec<_>>(),
            vec![3, 4, 5]
        );

        // P6 cites bft0, P7 cites bft1, P8..P10 cite bft2: a context_bft that never
        // regresses, which is the Extension rule this node enforces.
        for (height, bft_index) in [(6u64, 0usize), (7, 1), (8, 2), (9, 2), (10, 2)] {
            let block = at_height(&scene, height);
            assert_eq!(block.len(), 1, "one block at height {height}");
            assert_eq!(
                block[0].points_at_bft_block, by_height[bft_index].this_hash,
                "height {height} should cite bft{bft_index}"
            );
        }
    }

    #[test]
    fn diagram_scene_2_reorganizes_above_the_marker() {
        let scene = scene("finality_diagram_2_benign_reorg.zeccltf");

        // Ten blocks on the original branch plus four on the competing one.
        assert_eq!(scene.bc_blocks.len(), 14);
        assert_eq!(scene.bc_tip_height, 11);

        // The competing branch is longer, so it is the best chain from height 8 up, and
        // the three blocks it displaced are still drawn beside it.
        assert_eq!(best_heights(&scene), (1..=11).collect::<Vec<_>>());
        for height in 8..=10 {
            assert_eq!(at_height(&scene, height).len(), 2, "two blocks at height {height}");
        }

        // The whole new best chain still contains fin, and fin has not moved: no BFT
        // block decided, so this is the diagram's benign case rather than a conflict.
        assert_eq!(scene.bc_finalized_tip_height, 5);
        assert_eq!(finalized_heights(&scene), (1..=5).collect::<Vec<_>>());
        assert!(at_height(&scene, 5)[0].is_best_chain);
    }

    #[test]
    fn diagram_scene_3_forks_below_the_marker() {
        let scene = scene("finality_diagram_3_conflicting_fork.zeccltf");

        // Seven blocks on the branch the BFT chain finalized, five on the heavier one.
        assert_eq!(scene.bc_blocks.len(), 12);
        assert_eq!(scene.bc_tip_height, 8);
        assert_eq!(best_heights(&scene), (1..=8).collect::<Vec<_>>());

        assert_eq!(scene.bc_finalized_tip_height, 5);
        assert_eq!(finalized_heights(&scene), (1..=5).collect::<Vec<_>>());

        // The whole point of the scene: the finalized block is not on the best chain, and
        // neither is anything above the fork at height 3.
        let finalized_block = scene
            .bc_blocks
            .iter()
            .find(|b| b.this_height == 5 && b.is_finalized)
            .expect("a finalized block at height 5");
        assert!(!finalized_block.is_best_chain);
        for height in 4..=7 {
            assert!(
                at_height(&scene, height).iter().any(|b| !b.is_best_chain),
                "the abandoned branch is still drawn at height {height}"
            );
        }
        assert!(at_height(&scene, 3)[0].is_best_chain, "the branches meet at P3");
    }
}
