
use visualizer_zcash::{
    BftBlockInspection, BftPowHeaderInspection, BlockInspection, Hash32, PowBlockInspection,
    TxInspection,
};
use zebra_chain::value_balance::ValueBalance;
use std::cmp::{max, min};

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
        *visualizer_zcash::WINDOW_TITLE.lock().unwrap() = format!("TEST: {}", test_name);
    }

    // @Dev @Debug: detect which instance we are to position viz window
    #[cfg(target_os = "windows")] if visualizer_zcash::DEV_WIN32_WINDOW_ARRANGEMENT {
        let args: Vec<String> = std::env::args().collect();
        let args_str = args.join(" ");
        let bottom_right = args_str.contains("12302") || args_str.contains("12002") || args_str.contains("_1.local");
        visualizer_zcash::DEV_WIN32_WINDOW_RIGHT.store(bottom_right, std::sync::atomic::Ordering::Relaxed);
    }

    visualizer_zcash::main_thread_run_program(wallet_state, false);
}


/// Max best-chain blocks served per response. Bounds the startup burst (a fresh GUI
/// acks 0, which used to request the entire chain). Must comfortably exceed the
/// non-finalized window (~100) so the always-served [finalized tip..tip] span is
/// never cut. History below the window is not served for now; proper demand paging
/// is deferred (see the viz-paging branch for a prototype).
const BC_PAGE_SIZE: u64 = 1024;

/// Max BFT blocks served per response. A fresh GUI acks 0; serving only the newest
/// page keeps the first message bounded, and since the newest BFT blocks anchor to
/// on-screen best-chain blocks the ack then ratchets up and steady-state resends
/// are small. Deep BFT history is not yet demand-paged.
/// TODO: page old BFT blocks in on demand (e.g. from camera position), as bc does.
const BFT_PAGE_SIZE: usize = 2048;

/// Bridge between tokio & viz code
pub async fn service_viz_requests(
    tfl_handle: crate::TFLServiceHandle,
    params: &'static crate::ZcashCrosslinkParameters,
) {
    let call = tfl_handle.clone().call;

    let mut bc_ack_height = 0;
    let mut instr_strings: Vec<String> = Vec::new();
    // Finalization-candidate heights by candidate hash, resolved lazily from the state.
    // Lets the GUI position BFT certs at their candidate height before it has the PoW
    // block, which in turn lets camera paging fetch those blocks (a shown cert should
    // never be left without its PoW blocks). Bounded by total cert count.
    let mut bft_candidate_heights: std::collections::HashMap<Hash32, u64> = std::collections::HashMap::new();

    loop {
        let request_queue = visualizer_zcash::REQUESTS_TO_ZEBRA.lock().unwrap();
        let response_queue = visualizer_zcash::RESPONSES_FROM_ZEBRA.lock().unwrap();
        if request_queue.is_none() || response_queue.is_none() {
            continue;
        }
        let request_queue = request_queue.as_ref().unwrap();
        let response_queue = response_queue.as_ref().unwrap();

        'main_loop: loop {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            let Ok(StateReadResponse::TipPoolValues { value_balance, .. }) = (call.read_state)(StateReadRequest::TipPoolValues).await
            else {
                continue 'main_loop;
            };
            let orchard_pool_balance = value_balance.orchard_amount().zatoshis();
            let staking_bonded_pool_balance = value_balance.staking_bonded_amount().zatoshis();
            let staking_unbonded_pool_balance = value_balance.staking_unbonded_amount().zatoshis();

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
            // resent every cycle. With only [ack..tip] (ack ratchets to just behind the
            // PoW tip), older on-screen content has nothing to attach to and floats
            // detached, leaving a gap below the tip cluster.
            let finalized_height = tfl_handle.internal.lock().await.latest_final_block
                .map(|(h, _)| h.0 as i32)
                .unwrap_or(bc_ack_height);
            // Page cap: never serve more than the newest BC_PAGE_SIZE blocks; older
            // history is not served for now (history browsing deferred).
            let page_lo = (bc_tip_height + 1).saturating_sub(BC_PAGE_SIZE) as i32;
            let bc_req_h = (max(bc_ack_height.min(finalized_height), page_lo), -1);

            #[allow(clippy::never_loop)]
            let (lo_height, bc_tip, height_hashes, suspect_seq_blocks) = {
                let (lo, hi) = (bc_req_h.0, bc_req_h.1);
                assert!(
                    lo <= hi || (lo >= 0 && hi < 0),
                    "lo ({}) should be below hi ({})",
                    lo,
                    hi
                );

                let Ok(StateResponse::Tip(Some(tip_height_hash))) = (call.state)(StateRequest::Tip).await
                else {
                    continue 'main_loop;
                };

                let (h_lo, h_hi) = (
                    min(
                        tip_height_hash.0,
                        abs_block_height(lo, Some(tip_height_hash)),
                    ),
                    min(
                        tip_height_hash.0,
                        abs_block_height(hi, Some(tip_height_hash)),
                    ),
                );
                // temp
                //assert!(h_lo.0 <= h_hi.0, "lo ({}) should be below hi ({})", h_lo.0, h_hi.0);

                async fn get_height_hash(
                    call: TFLServiceCalls,
                    h: ZebBlockHeight,
                    existing_height_hash: (ZebBlockHeight, ZebBlockHash),
                ) -> Option<(ZebBlockHeight, ZebBlockHash)> {
                    if h == existing_height_hash.0 {
                        // avoid duplicating work if we've already got that value
                        // TODO: does this miss reorgs?
                        Some(existing_height_hash)
                    } else if let Ok(StateResponse::BlockHeader { hash, .. }) =
                        (call.state)(StateRequest::BlockHeader(h.into())).await
                    {
                        Some((h, hash))
                    } else {
                        error!("Failed to read block header at height {}", h.0);
                        None
                    }
                }

                let Some(hi_height_hash) = get_height_hash(call.clone(), h_hi, tip_height_hash).await
                else {
                    continue 'main_loop;
                };

                let Some(lo_height_hash) = get_height_hash(call.clone(), h_lo, hi_height_hash).await
                else {
                    continue 'main_loop;
                };

                let (height_hashes, blocks) =
                    tfl_block_sequence(&call, lo_height_hash.1, Some(hi_height_hash), true, true).await;
                (
                    lo_height_hash.0,
                    Some(tip_height_hash),
                    height_hashes,
                    blocks,
                )
            };

            // paranoid guards
            if lo_height.0 as i64 != bc_req_h.0 as i64 {
                println!("PARANOID != WRONG: lo {} vs requested {}", lo_height.0, bc_req_h.0);
                continue 'main_loop;
            }
            if suspect_seq_blocks.is_empty() {
                println!("PARANOID != WRONG: empty seq blocks");
                continue 'main_loop;
            }
            let mut seq_blocks = Vec::new();
            for (i, maybe) in suspect_seq_blocks.iter().enumerate() {
                let Some(block) = maybe
                else {
                    println!("PARANOID != WRONG: None seq block at {i}");
                    continue 'main_loop;
                };
                seq_blocks.push(block);
            }


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

            // Resolve any unresolved finalization-candidate heights for the BFT page.
            // Hashes are collected under a short internal lock; the state lookups await
            // outside it. Steady-state this is 0-1 lookups; the first cycle resolves the
            // whole page once.
            {
                let unresolved: Vec<Hash32> = {
                    let internal = tfl_handle.internal.lock().await;
                    let page_start = internal.bft_blocks.len().saturating_sub(BFT_PAGE_SIZE);
                    internal.bft_blocks[page_start..].iter()
                        .map(|b| Hash32::from_bytes(BlockHash::from_header_data(b.finalization_candidate()).0))
                        .filter(|h| !bft_candidate_heights.contains_key(h))
                        .collect()
                };
                for hash in unresolved {
                    if let Ok(StateResponse::BlockHeader { height, .. }) =
                        (call.state)(StateRequest::BlockHeader(zebra_state::HashOrHeight::Hash(ZebBlockHash(hash.as_bytes()).into()))).await
                    {
                        bft_candidate_heights.insert(hash, height.0 as u64);
                    }
                }
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

                    if !request.serialize_instrs_path.is_empty() {
                        let handle = tfl_handle.clone();
                        let ser_call = call.clone();
                        let path_string = request.serialize_instrs_path.clone();
                        tokio::task::spawn(async move {
                            let Ok(StateResponse::Tip(Some(tip))) = (ser_call.state)(StateRequest::Tip).await else { return; };
                            let Ok(StateResponse::BlockHeader { hash: lo_hash, .. }) =
                                (ser_call.state)(StateRequest::BlockHeader(ZebBlockHeight(1).into())).await else { return; };
                            let (_, blocks) = tfl_block_sequence(&ser_call, lo_hash, Some(tip), true, true).await;

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
                            for block in blocks.iter().flatten() {
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
                    // page of older best-chain blocks so coverage extends downward contiguously.
                    // Fetched before taking the internal lock since state calls await.
                    let mut backfill_hh: Vec<(ZebBlockHeight, ZebBlockHash)> = Vec::new();
                    let mut backfill_blocks: Vec<Option<Arc<Block>>> = Vec::new();
                    if request.bc_want_below > 0 && request.bc_want_below <= bc_tip_height {
                        let hi_h = ZebBlockHeight((request.bc_want_below - 1) as u32);
                        let lo_h = ZebBlockHeight(hi_h.0.saturating_sub(BC_PAGE_SIZE as u32 - 1));
                        if let (
                            Ok(StateResponse::BlockHeader { hash: lo_hash, .. }),
                            Ok(StateResponse::BlockHeader { hash: hi_hash, .. }),
                        ) = (
                            (call.state)(StateRequest::BlockHeader(lo_h.into())).await,
                            (call.state)(StateRequest::BlockHeader(hi_h.into())).await,
                        ) {
                            (backfill_hh, backfill_blocks) = tfl_block_sequence(&call, lo_hash, Some((hi_h, hi_hash)), true, true).await;
                        }
                    }

                    let mut internal = tfl_handle.internal.lock().await;
                    let mut response = visualizer_zcash::ResponseFromZebra::_0();
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
                    response.mempool_tx_strings = mempool_tx_strings.clone();
                    response.pos_tip_signers = internal.fat_pointer_to_tip.signatures.iter()
                        .map(|sig| Hash32::from_bytes(sig.pub_key.0))
                        .collect();
                    response.instr_strings = instr_strings.clone();
                    response.instr_done_n = *TEST_INSTR_C.lock().unwrap();
                    response.instr_failed = TEST_FAILED_INSTR_IDXS.lock().unwrap().clone();

                    response.orchard_pool_balance = orchard_pool_balance;
                    response.staking_bonded_pool_balance = staking_bonded_pool_balance;
                    response.staking_unbonded_pool_balance = staking_unbonded_pool_balance;

                    response.start_bc_height = lo_height.0 as u64; // actual window start (may be below ack; see bc_req_h)
                    bc_ack_height = bc_ack_height.max(request.bc_ack_height as i32);

                    let pow_inspection = |block: &Block| {
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
                    };

                    let bft_inspection = |b: &wallet::bft::BftBlock| {
                        BlockInspection::Bft(BftBlockInspection {
                            hash: Hash32::from_bytes(b.blake3_hash().0),
                            version: b.version,
                            height: b.height,
                            previous_hash: Hash32::from_bytes(b.previous_block_hash().0),
                            finalization_candidate_height: 0,
                            do_not_include_until_bc_height: b.do_not_include_until_bc_height,
                            hardforks: b.hardforks.iter().map(|hf| visualizer_zcash::HardforkInspection {
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
                    };

                    for (i, bc) in seq_blocks.iter().enumerate() {
                        let this_hash = Hash32::from_bytes(bc.header.hash().0);
                        if request.want_to_inspect_block == this_hash {
                            response.what_block_it_is = this_hash;
                            response.block_inspection = pow_inspection(bc.as_ref());
                        }
                        response.bc_blocks.push(visualizer_zcash::BcBlock {
                            this_hash,
                            parent_hash: Hash32::from_bytes(bc.header.previous_block_hash.0),
                            this_height: lo_height.0 as u64 + i as u64,
                            txs_n: bc.transactions.len(),
                            is_best_chain: true,
                            is_finalized: false,
                            knowledge: visualizer_zcash::BcKnowledge::FullBlock,
                            points_at_bft_block: Hash32::from_bytes(bc.header.fat_pointer_to_bft_block.points_at_block_hash().0),
                            // #[cfg(debug_assertions)]
                            work: bc.header.difficulty_threshold.to_work()
                                .map(|w| u64::try_from(w.as_u128()).unwrap_or(u64::MAX))
                                .unwrap_or(0xdeadbeef),
                            utc: bc.header.time.timestamp(),
                            serialized_size: bc.zcash_serialized_size(),
                            // Flag this block when it sits at a hardfork's PoW activation height.
                            // Many forks may share that height; each is flagged independently.
                            is_hardfork_activation: {
                                let h = lo_height.0 as u64 + i as u64;
                                tfl_handle.config.hardforks.iter().any(|hf| hf.pow_activation_height == h)
                            },
                        });
                    }

                    // backfill page (older best-chain blocks below the GUI's coverage)
                    for (hh, maybe_block) in backfill_hh.iter().zip(backfill_blocks.iter()) {
                        let Some(bc) = maybe_block else { continue };
                        let this_hash = Hash32::from_bytes(hh.1.0);
                        if request.want_to_inspect_block == this_hash {
                            response.what_block_it_is = this_hash;
                            response.block_inspection = pow_inspection(bc.as_ref());
                        }
                        response.bc_blocks.push(visualizer_zcash::BcBlock {
                            this_hash,
                            parent_hash: Hash32::from_bytes(bc.header.previous_block_hash.0),
                            this_height: hh.0.0 as u64,
                            txs_n: bc.transactions.len(),
                            is_best_chain: true,
                            is_finalized: false,
                            knowledge: visualizer_zcash::BcKnowledge::FullBlock,
                            points_at_bft_block: Hash32::from_bytes(bc.header.fat_pointer_to_bft_block.points_at_block_hash().0),
                            work: bc.header.difficulty_threshold.to_work()
                                .map(|w| u64::try_from(w.as_u128()).unwrap_or(u64::MAX))
                                .unwrap_or(0xdeadbeef),
                            utc: bc.header.time.timestamp(),
                            serialized_size: bc.zcash_serialized_size(),
                            is_hardfork_activation: tfl_handle.config.hardforks.iter()
                                .any(|hf| hf.pow_activation_height == hh.0.0 as u64),
                        });
                    }


                    // Fetch sidechain blocks from NonFinalizedState so the visualizer
                    // can render forks alongside the best chain.
                    if let Ok(StateReadResponse::SidechainBlocks(sidechain_blocks)) =
                        (call.read_state)(StateReadRequest::SidechainBlocks).await
                    {
                        use zebra_chain::work::difficulty::ExpandedDifficulty;

                        for (height, hash, block) in sidechain_blocks {
                            let header = &block.header;
                            let work = header.difficulty_threshold
                                .to_work()
                                .map(|w| u64::try_from(w.as_u128()).unwrap_or(u64::MAX))
                                .unwrap_or(0);

                            response.bc_blocks.push(visualizer_zcash::BcBlock {
                                this_hash: Hash32::from_bytes(hash.0),
                                parent_hash: Hash32::from_bytes(header.previous_block_hash.0),
                                this_height: height.0 as u64,
                                txs_n: block.transactions.len(),
                                is_best_chain: false,
                                is_finalized: false,
                                knowledge: visualizer_zcash::BcKnowledge::FullBlock,
                                points_at_bft_block: Hash32::from_u64(0),
                                work,
                                utc: header.time.timestamp(),
                                serialized_size: block.zcash_serialized_size(),
                                is_hardfork_activation: tfl_handle.config.hardforks.iter()
                                    .any(|hf| hf.pow_activation_height == height.0 as u64),
                            });
                        }
                    }

                    // Newest-page cap: a fresh GUI acks 0; the newest BFT blocks anchor to
                    // on-screen bc blocks, so its ack ratchets up after the first message.
                    let bft_page_start = (request.bft_ack_height as usize)
                        .max(internal.bft_blocks.len().saturating_sub(BFT_PAGE_SIZE));

                    // BFT certs for the backfill page: each PoW block's fat pointer names a
                    // cert, so the [min..max] cert-index extent linked from the page covers
                    // the certs decided over that PoW span. (Candidate-height inversion would
                    // also work as an extent source; fat pointers are the direct one.)
                    let mut bft_extent: Option<(usize, usize)> = None;
                    for maybe_block in &backfill_blocks {
                        let Some(bc) = maybe_block else { continue };
                        let ptr = bc.header.fat_pointer_to_bft_block.points_at_block_hash();
                        if let Some(&h) = internal.bft_block_hash_to_height.get(&ptr) {
                            let h = h as usize;
                            bft_extent = Some(match bft_extent {
                                None => (h, h),
                                Some((lo, hi)) => (lo.min(h), hi.max(h)),
                            });
                        }
                    }
                    // Bridge the seam to the window: a cert whose candidate is just below the
                    // window but which is only pointed at by in-window blocks belongs to
                    // neither the newest-page serving nor the page extent, leaving a hole at
                    // the load boundary. Folding the window blocks' pointers into the extent
                    // makes the served cert range contiguous across the seam.
                    if bft_extent.is_some() {
                        for bc in &seq_blocks {
                            let ptr = bc.header.fat_pointer_to_bft_block.points_at_block_hash();
                            if let Some(&h) = internal.bft_block_hash_to_height.get(&ptr) {
                                let h = h as usize;
                                bft_extent = Some(match bft_extent {
                                    None => (h, h),
                                    Some((lo, hi)) => (lo.min(h), hi.max(h)),
                                });
                            }
                        }
                    }

                    let mut bft_indices: Vec<usize> = Vec::new();
                    if let Some((lo, hi)) = bft_extent {
                        let hi = hi.min(lo + BFT_PAGE_SIZE - 1).min(internal.bft_blocks.len().saturating_sub(1));
                        bft_indices.extend((lo..=hi).filter(|i| *i < bft_page_start));
                    }
                    bft_indices.extend(bft_page_start..internal.bft_blocks.len());

                    for i in bft_indices {
                        let b = &internal.bft_blocks[i];
                        let candidate_hash = Hash32::from_bytes(BlockHash::from_header_data(b.finalization_candidate()).0);
                        let candidate_height = bft_candidate_heights.get(&candidate_hash).copied().unwrap_or(0);
                        // Newest-page certs are served only if their finalization candidate lies
                        // inside the served PoW window: a shown cert always has its PoW blocks
                        // shown. Backfill-extent certs are exempt: their PoW page ships in this
                        // very response (their candidate heights are also mostly unresolved).
                        if i >= bft_page_start && candidate_height < lo_height.0 as u64 { continue; }
                        let this_hash = Hash32::from_bytes(b.blake3_hash().0);
                        if request.want_to_inspect_block == this_hash {
                            response.what_block_it_is = this_hash;
                            response.block_inspection = bft_inspection(b);
                        }
                        response.bft_blocks.push(visualizer_zcash::BftBlock {
                            this_hash: this_hash,
                            parent_hash: Hash32::from_bytes(b.previous_block_hash().0),
                            this_height: i as u64,
                            points_at_bc_block: candidate_hash,
                            points_at_bc_height: candidate_height,
                            // full header data, so the GUI can show proven blocks it never received
                            proving_blocks: b.headers.iter().skip(1).map(|x| visualizer_zcash::ProvingHeader {
                                hash: Hash32::from_bytes(BlockHash::from_header_data(x).0),
                                parent_hash: Hash32::from_bytes(x.prev_block.0),
                                utc: x.time as i64,
                                work: zebra_chain::work::difficulty::CompactDifficulty(x.bits)
                                    .to_work()
                                    .map(|w| u64::try_from(w.as_u128()).unwrap_or(u64::MAX))
                                    .unwrap_or(0),
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

fn abs_block_height(height: i32, tip: Option<(ZebBlockHeight, ZebBlockHash)>) -> ZebBlockHeight {
    if height >= 0 {
        ZebBlockHeight(height.try_into().unwrap())
    } else if let Some(tip) = tip {
        tip.0.sat_sub(!height)
    } else {
        ZebBlockHeight(0)
    }
}

fn abs_block_heights(
    heights: (i32, i32),
    tip: Option<(ZebBlockHeight, ZebBlockHash)>,
) -> (ZebBlockHeight, ZebBlockHeight) {
    (
        abs_block_height(heights.0, tip),
        abs_block_height(heights.1, tip),
    )
}
