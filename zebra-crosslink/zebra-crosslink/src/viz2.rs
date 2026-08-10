
use visualizer_zcash::{
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

    let mut bc_ack_height: u64 = 0;
    let mut skipped_windows_n: u64 = 0;
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
            // Never cut [finalized tip..tip] out of the window, even when finality lags
            // the PoW tip by more than a page (that lag is exactly what this visualizer
            // should show): the page cap only bounds ack-lag. While finality is still
            // unknown (fresh restart) the ack alone bounds the window; corrects itself
            // on the first BFT block ingest.
            let page_lo = (bc_tip_height + 1).saturating_sub(BC_PAGE_SIZE);
            let finalized_lo = tfl_handle.internal.lock().await.latest_final_block
                .map(|(h, _)| h.0 as u64)
                .unwrap_or(u64::MAX);
            // lo = ack clamped to [page_lo, finalized_lo]; when finality lags below the
            // page floor, the trailing min wins and the window extends down to it.
            let req_lo_height = ZebBlockHeight(bc_ack_height.max(page_lo).min(finalized_lo).min(bc_tip_height) as u32);

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

            // Resolve any unresolved finalization-candidate heights for the BFT page.
            // Hashes are collected under a short internal lock; the state lookups await
            // outside it. Steady-state this is 0-1 lookups; the first cycle resolves the
            // whole page once.
            {
                let unresolved: Vec<Hash32> = {
                    let internal = tfl_handle.internal.lock().await;
                    let page_start = internal.bft_blocks.len().saturating_sub(BFT_PAGE_SIZE);
                    internal.bft_blocks[page_start..].iter()
                        // out-of-order BFT ingest pads the chain with empty-headers placeholder
                        // certs (see handle_new_decided_bft_block); they have no candidate yet
                        .filter(|b| !b.headers.is_empty())
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

                    response.start_bc_height = lo_height.0 as u64; // actual window start, may be below ack
                    // Clamped to the tip: the GUI derives its ack from on-screen block
                    // heights, and a bogus one above the tip would otherwise collapse the
                    // window onto the tip block itself.
                    bc_ack_height = bc_ack_height.max(request.bc_ack_height).min(bc_tip_height);

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

                    let push_bc_block = |response: &mut visualizer_zcash::ResponseFromZebra,
                                         height: &ZebBlockHeight,
                                         hash: &ZebBlockHash,
                                         bc: &Block,
                                         is_best_chain: bool| {
                        let this_hash = Hash32::from_bytes(hash.0);
                        if request.want_to_inspect_block == this_hash {
                            response.what_block_it_is = this_hash;
                            response.block_inspection = pow_inspection(bc);
                        }
                        response.bc_blocks.push(visualizer_zcash::BcBlock {
                            this_hash,
                            parent_hash: Hash32::from_bytes(bc.header.previous_block_hash.0),
                            this_height: height.0 as u64,
                            txs_n: bc.transactions.len(),
                            is_best_chain,
                            is_finalized: false,
                            knowledge: visualizer_zcash::BcKnowledge::FullBlock,
                            points_at_bft_block: Hash32::from_bytes(bc.header.fat_pointer_to_bft_block.points_at_block_hash().0),
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
                    // backfill page (older best-chain blocks below the GUI's coverage)
                    for (height, hash, bc) in backfill_blocks.iter() {
                        push_bc_block(&mut response, height, hash, bc, true);
                    }
                    for (height, hash, bc) in fork_blocks.iter() {
                        push_bc_block(&mut response, height, hash, bc, false);
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
                    for (_, _, bc) in &backfill_blocks {
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
                        for (_, _, bc) in &seq_blocks {
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
                        // Out-of-order BFT ingest pads the chain with empty-headers placeholder
                        // certs; nothing to show until the real block arrives.
                        if b.headers.is_empty() { continue; }
                        let candidate_hash = Hash32::from_bytes(BlockHash::from_header_data(b.finalization_candidate()).0);
                        let candidate_height = bft_candidate_heights.get(&candidate_hash).copied();
                        // Newest-page certs are skipped only when their finalization candidate is
                        // KNOWN to lie below the served PoW window: unresolved heights must pass,
                        // else one failed lookup permanently drops the cert once the GUI's ack
                        // ratchets past it. Backfill-extent certs are exempt: their PoW page
                        // ships in this very response.
                        if i >= bft_page_start && candidate_height.is_some_and(|h| h < lo_height.0 as u64) { continue; }
                        let candidate_height = candidate_height.unwrap_or(0);
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

