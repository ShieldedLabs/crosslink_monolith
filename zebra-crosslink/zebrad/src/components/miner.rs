//! Internal mining in Zebra.
//!
//! # TODO
//! - pause mining if we have no peers, like `zcashd` does,
//!   and add a developer config that mines regardless of how many peers we have.
//!   <https://github.com/zcash/zcash/blob/6fdd9f1b81d3b228326c9826fa10696fc516444b/src/miner.cpp#L865-L880>
//! - move common code into zebra-chain or zebra-node-services and remove the RPC dependency.

use std::{cmp::min, sync::Arc, thread::available_parallelism, time::Duration};

use color_eyre::Report;
use futures::{stream::FuturesUnordered, StreamExt};
use thread_priority::{ThreadBuilder, ThreadPriority};
use tokio::{select, sync::watch, task::JoinHandle, time::sleep};
use tower::Service;
use tracing::{Instrument, Span};

use zebra_chain::{
    block::{self, Block},
    chain_sync_status::ChainSyncStatus,
    chain_tip::ChainTip,
    diagnostic::task::WaitForPanics,
    parameters::{Network, NetworkUpgrade},
    serialization::{AtLeastOne, ZcashSerialize},
    shutdown::is_shutting_down,
    work::equihash::{Solution, SolverCancelled},
};
use zebra_network::AddressBookPeers;
use zebra_node_services::mempool;
use zebra_rpc::{
    client::{
        BlockTemplateTimeSource,
        GetBlockTemplateCapability::{CoinbaseTxn, LongPoll},
        GetBlockTemplateParameters,
        GetBlockTemplateRequestMode::Template,
        HexData,
    },
    methods::{RpcImpl, RpcServer},
    proposal_block_from_template,
};
use zebra_state::WatchReceiver;

use zebra_rpc::config::mining::Config as MiningConfig;

// Every duration below is measured in *chain* time, not wall-clock time: they exist
// because a tip only changes so often and a template is only worth rebuilding so often.
// So they are waited on with the `zebra_debug_time` timers, which tick in chain time, and
// never with `tokio::time` directly. Waiting out `BLOCK_TEMPLATE_REFRESH_LIMIT` in real
// seconds puts a hard floor of one block every two seconds under any dilation multiplier —
// which is exactly what a fast-forwarded test network is trying to get below.

/// The amount of time we wait between block template retries.
pub const BLOCK_TEMPLATE_WAIT_TIME: Duration = Duration::from_secs(20);

/// A rate-limit for block template refreshes.
pub const BLOCK_TEMPLATE_REFRESH_LIMIT: Duration = Duration::from_secs(2);

/// How long we wait after mining a block, before expecting a new template.
///
/// This should be slightly longer than `BLOCK_TEMPLATE_REFRESH_LIMIT` to allow for template
/// generation.
pub const BLOCK_MINING_WAIT_TIME: Duration = Duration::from_secs(3);

/// Initialize the miner based on its config, and spawn a task for it.
///
/// This method is CPU and memory-intensive. It uses 144 MB of RAM and one CPU core per configured
/// mining thread.
///
/// See [`run_mining_solver()`] for more details.
pub fn spawn_init<
    Mempool,
    TFLService,
    State,
    ReadState,
    Tip,
    AddressBook,
    BlockVerifierRouter,
    SyncStatus,
>(
    network: &Network,
    mining_config: &MiningConfig,
    rpc: RpcImpl<
        Mempool,
        TFLService,
        State,
        ReadState,
        Tip,
        AddressBook,
        BlockVerifierRouter,
        SyncStatus,
    >,
) -> JoinHandle<Result<(), Report>>
// TODO: simplify or avoid repeating these generics (how?)
where
    Mempool: Service<
            mempool::Request,
            Response = mempool::Response,
            Error = zebra_node_services::BoxError,
        > + Clone
        + Send
        + Sync
        + 'static,
    Mempool::Future: Send,
    TFLService: Service<
            zebra_state::crosslink::TFLServiceRequest,
            Response = zebra_state::crosslink::TFLServiceResponse,
            Error = zebra_state::BoxError,
        > + Clone
        + Send
        + Sync
        + 'static,
    TFLService::Future: Send,
    State: Service<
            zebra_state::Request,
            Response = zebra_state::Response,
            Error = zebra_state::BoxError,
        > + Clone
        + Send
        + Sync
        + 'static,
    <State as Service<zebra_state::Request>>::Future: Send,
    ReadState: Service<
            zebra_state::ReadRequest,
            Response = zebra_state::ReadResponse,
            Error = zebra_state::BoxError,
        > + Clone
        + Send
        + Sync
        + 'static,
    <ReadState as Service<zebra_state::ReadRequest>>::Future: Send,
    Tip: ChainTip + Clone + Send + Sync + 'static,
    BlockVerifierRouter: Service<zebra_consensus::Request, Response = block::Hash, Error = zebra_consensus::BoxError>
        + Clone
        + Send
        + Sync
        + 'static,
    <BlockVerifierRouter as Service<zebra_consensus::Request>>::Future: Send,
    SyncStatus: ChainSyncStatus + Clone + Send + Sync + 'static,
    AddressBook: AddressBookPeers + Clone + Send + Sync + 'static,
{
    // TODO: spawn an entirely new executor here, so mining is isolated from higher priority tasks.
    tokio::spawn(init(network.clone(), mining_config.clone(), rpc).in_current_span())
}

/// Initialize the miner based on its config.
///
/// This method is CPU and memory-intensive. It uses 144 MB of RAM and one CPU core per configured
/// mining thread.
///
/// See [`run_mining_solver()`] for more details.
pub async fn init<
    Mempool,
    TFLService,
    State,
    ReadState,
    Tip,
    BlockVerifierRouter,
    SyncStatus,
    AddressBook,
>(
    network: Network,
    mining_config: MiningConfig,
    rpc: RpcImpl<
        Mempool,
        TFLService,
        State,
        ReadState,
        Tip,
        AddressBook,
        BlockVerifierRouter,
        SyncStatus,
    >,
) -> Result<(), Report>
where
    Mempool: Service<
            mempool::Request,
            Response = mempool::Response,
            Error = zebra_node_services::BoxError,
        > + Clone
        + Send
        + Sync
        + 'static,
    Mempool::Future: Send,
    TFLService: Service<
            zebra_state::crosslink::TFLServiceRequest,
            Response = zebra_state::crosslink::TFLServiceResponse,
            Error = zebra_state::BoxError,
        > + Clone
        + Send
        + Sync
        + 'static,
    TFLService::Future: Send,
    State: Service<
            zebra_state::Request,
            Response = zebra_state::Response,
            Error = zebra_state::BoxError,
        > + Clone
        + Send
        + Sync
        + 'static,
    <State as Service<zebra_state::Request>>::Future: Send,
    ReadState: Service<
            zebra_state::ReadRequest,
            Response = zebra_state::ReadResponse,
            Error = zebra_state::BoxError,
        > + Clone
        + Send
        + Sync
        + 'static,
    <ReadState as Service<zebra_state::ReadRequest>>::Future: Send,
    Tip: ChainTip + Clone + Send + Sync + 'static,
    BlockVerifierRouter: Service<zebra_consensus::Request, Response = block::Hash, Error = zebra_consensus::BoxError>
        + Clone
        + Send
        + Sync
        + 'static,
    <BlockVerifierRouter as Service<zebra_consensus::Request>>::Future: Send,
    SyncStatus: ChainSyncStatus + Clone + Send + Sync + 'static,
    AddressBook: AddressBookPeers + Clone + Send + Sync + 'static,
{
    // Upstream hard-codes this to 1, citing #8797: solvers must be cancelled when the best
    // tip changes, or the extra threads mine a chain that is already dead. They are —
    // `cancel_fn` below fires on every template change, and a new tip always changes the
    // template — so this is the operator's choice to make.
    let configured_threads = mining_config.internal_miner_threads.max(1);
    // If we can't detect the number of cores, use the configured number.
    let available_threads = available_parallelism()
        .map(usize::from)
        .unwrap_or(configured_threads);

    // Use the minimum of the configured and available threads.
    let mut solver_count = min(configured_threads, available_threads);
    let low_priority = mining_config.internal_miner_low_priority;

    // A network with proof-of-work disabled needs no solvers at all -- see `mine_a_block`.
    // Extra ones would only race to produce siblings of a block that costs nothing to make.
    let skip_pow = network.disable_pow();
    if skip_pow {
        solver_count = 1;
    }

    info!(
        ?solver_count,
        ?low_priority,
        ?skip_pow,
        "launching mining tasks with parallel solvers"
    );

    let (template_sender, template_receiver) = watch::channel(None);
    let template_receiver = WatchReceiver::new(template_receiver);

    // Spawn these tasks, to avoid blocked cooperative futures, and improve shutdown responsiveness.
    // This is particularly important when there are a large number of solver threads.
    let mut abort_handles = Vec::new();

    let template_generator = tokio::task::spawn(
        generate_block_templates(network, rpc.clone(), template_sender).in_current_span(),
    );
    abort_handles.push(template_generator.abort_handle());
    let template_generator = template_generator.wait_for_panics();

    let mut mining_solvers = FuturesUnordered::new();
    for solver_id in 0..solver_count {
        // Assume there are less than 256 cores. If there are more, only run 256 tasks.
        let solver_id = min(solver_id, usize::from(u8::MAX))
            .try_into()
            .expect("just limited to u8::MAX");

        let solver = tokio::task::spawn(
            run_mining_solver(
                solver_id,
                low_priority,
                skip_pow,
                template_receiver.clone(),
                rpc.clone(),
            )
            .in_current_span(),
        );
        abort_handles.push(solver.abort_handle());

        mining_solvers.push(solver.wait_for_panics());
    }

    // These tasks run forever unless there is a fatal error or shutdown.
    // When that happens, the first task to error returns, and the other JoinHandle futures are
    // cancelled.
    let first_result;
    select! {
        result = template_generator => { first_result = result; }
        result = mining_solvers.next() => {
            first_result = result
                .expect("stream never terminates because there is at least one solver task");
        }
    }

    // But the spawned async tasks keep running, so we need to abort them here.
    for abort_handle in abort_handles {
        abort_handle.abort();
    }

    // Any spawned blocking threads will keep running. When this task returns and drops the
    // `template_sender`, it cancels all the spawned miner threads. This works because we've
    // aborted the `template_generator` task, which owns the `template_sender`. (And it doesn't
    // spawn any blocking threads.)
    first_result
}

/// Generates block templates using `rpc`, and sends them to mining threads using `template_sender`.
// Note(Sam): Very very noisy.
//#[instrument(skip(rpc, template_sender))]
pub async fn generate_block_templates<
    Mempool,
    TFLService,
    State,
    ReadState,
    Tip,
    BlockVerifierRouter,
    SyncStatus,
    AddressBook,
>(
    network: Network,
    rpc: RpcImpl<
        Mempool,
        TFLService,
        State,
        ReadState,
        Tip,
        AddressBook,
        BlockVerifierRouter,
        SyncStatus,
    >,
    template_sender: watch::Sender<Option<Arc<Block>>>,
) -> Result<(), Report>
where
    Mempool: Service<
            mempool::Request,
            Response = mempool::Response,
            Error = zebra_node_services::BoxError,
        > + Clone
        + Send
        + Sync
        + 'static,
    Mempool::Future: Send,
    TFLService: Service<
            zebra_state::crosslink::TFLServiceRequest,
            Response = zebra_state::crosslink::TFLServiceResponse,
            Error = zebra_state::BoxError,
        > + Clone
        + Send
        + Sync
        + 'static,
    TFLService::Future: Send,
    State: Service<
            zebra_state::Request,
            Response = zebra_state::Response,
            Error = zebra_state::BoxError,
        > + Clone
        + Send
        + Sync
        + 'static,
    <State as Service<zebra_state::Request>>::Future: Send,
    ReadState: Service<
            zebra_state::ReadRequest,
            Response = zebra_state::ReadResponse,
            Error = zebra_state::BoxError,
        > + Clone
        + Send
        + Sync
        + 'static,
    <ReadState as Service<zebra_state::ReadRequest>>::Future: Send,
    Tip: ChainTip + Clone + Send + Sync + 'static,
    BlockVerifierRouter: Service<zebra_consensus::Request, Response = block::Hash, Error = zebra_consensus::BoxError>
        + Clone
        + Send
        + Sync
        + 'static,
    <BlockVerifierRouter as Service<zebra_consensus::Request>>::Future: Send,
    SyncStatus: ChainSyncStatus + Clone + Send + Sync + 'static,
    AddressBook: AddressBookPeers + Clone + Send + Sync + 'static,
{
    // Pass the correct arguments, even if Zebra currently ignores them.
    let mut parameters =
        GetBlockTemplateParameters::new(Template, None, vec![LongPoll, CoinbaseTxn], None, None);

    // Default to off: never mine until we've actually confirmed the toggle is on.
    let mut cached_is_mining = false;

    // Shut down the task when all the template receivers are dropped, or Zebra shuts down.
    while !template_sender.is_closed() && !is_shutting_down() {
        if let Ok(is_mining) = zebra_crosslink::wallet::GUI_ENABLE_MINE.try_lock() {
            cached_is_mining = *is_mining;
        }
        if !cached_is_mining {
            // Drop the current template so the solver threads stop mining the last one.
            if template_sender.borrow().is_some() {
                info!("mining disabled");
                let _ = template_sender.send(None);
            }
            zebra_debug_time::sleep(BLOCK_TEMPLATE_REFRESH_LIMIT).await;
            continue;
        }

        // On the first pass after re-enabling, `parameters` may still carry the long-poll id from
        // before we were disabled, so this call can block until the template changes or times out.
        // That's a one-time delay on the first block after resuming; it self-corrects below.
        let template: Result<_, _> = rpc.get_block_template(Some(parameters.clone())).await;

        // Wait for the chain to sync so we get a valid template.
        let Ok(template) = template else {
            warn!(
                ?BLOCK_TEMPLATE_WAIT_TIME,
                ?template,
                "waiting for a valid block template",
            );

            // Skip the wait if we got an error because we are shutting down.
            if !is_shutting_down() {
                zebra_debug_time::sleep(BLOCK_TEMPLATE_WAIT_TIME).await;
            }

            continue;
        };

        // Convert from RPC GetBlockTemplate to Block
        let template = template
            .try_into_template()
            .expect("invalid RPC response: proposal in response to a template request");

        info!(
            height = ?template.height(),
            transactions = ?template.transactions().len(),
            "mining with an updated block template",
        );

        // Tell the next get_block_template() call to wait until the template has changed.
        parameters = GetBlockTemplateParameters::new(
            Template,
            None,
            vec![LongPoll, CoinbaseTxn],
            Some(template.long_poll_id()),
            None,
        );

        let block = proposal_block_from_template(
            &template,
            BlockTemplateTimeSource::CurTime,
            rpc.network(),
        )?;

        // If the template has actually changed, send an updated template.
        //
        // A moved timestamp on its own is not a change worth acting on: the solver is
        // cancelled whenever a new template arrives, so republishing for the clock alone
        // throws away every hash computed since the last poll. That is a slow drip
        // normally and a flood under debug time dilation, where the template's time
        // advances by the multiplier on every poll. The block keeps the timestamp it was
        // built with, which stays valid — anything that would invalidate it (a new tip,
        // new transactions) changes another header field and does cancel the solve.
        template_sender.send_if_modified(|old_block| {
            if let Some(old_header) = old_block.as_ref().map(|b| b.header.clone()) {
                let mut new_header = zebra_chain::block::Header::clone(&block.header);
                new_header.time = old_header.time;
                if new_header == *old_header {
                    return false;
                }
            }
            *old_block = Some(Arc::new(block));
            true
        });

        // If the blockchain is changing rapidly, limit how often we'll update the template.
        // But if we're shutting down, do that immediately.
        if !template_sender.is_closed() && !is_shutting_down() {
            zebra_debug_time::sleep(BLOCK_TEMPLATE_REFRESH_LIMIT).await;
        }
    }

    Ok(())
}

/// Runs a single mining thread that gets blocks from the `template_receiver`, calculates equihash
/// solutions with nonces based on `solver_id`, and submits valid blocks to Zebra's block validator.
///
/// This method is CPU and memory-intensive. It uses 144 MB of RAM and one CPU core while running.
/// It can run for minutes or hours if the network difficulty is high. Mining uses a thread with
/// low CPU priority.
#[instrument(skip(template_receiver, rpc))]
pub async fn run_mining_solver<
    Mempool,
    TFLService,
    State,
    ReadState,
    Tip,
    BlockVerifierRouter,
    SyncStatus,
    AddressBook,
>(
    solver_id: u8,
    low_priority: bool,
    skip_pow: bool,
    mut template_receiver: WatchReceiver<Option<Arc<Block>>>,
    rpc: RpcImpl<
        Mempool,
        TFLService,
        State,
        ReadState,
        Tip,
        AddressBook,
        BlockVerifierRouter,
        SyncStatus,
    >,
) -> Result<(), Report>
where
    Mempool: Service<
            mempool::Request,
            Response = mempool::Response,
            Error = zebra_node_services::BoxError,
        > + Clone
        + Send
        + Sync
        + 'static,
    Mempool::Future: Send,
    TFLService: Service<
            zebra_state::crosslink::TFLServiceRequest,
            Response = zebra_state::crosslink::TFLServiceResponse,
            Error = zebra_state::BoxError,
        > + Clone
        + Send
        + Sync
        + 'static,
    TFLService::Future: Send,
    State: Service<
            zebra_state::Request,
            Response = zebra_state::Response,
            Error = zebra_state::BoxError,
        > + Clone
        + Send
        + Sync
        + 'static,
    <State as Service<zebra_state::Request>>::Future: Send,
    ReadState: Service<
            zebra_state::ReadRequest,
            Response = zebra_state::ReadResponse,
            Error = zebra_state::BoxError,
        > + Clone
        + Send
        + Sync
        + 'static,
    <ReadState as Service<zebra_state::ReadRequest>>::Future: Send,
    Tip: ChainTip + Clone + Send + Sync + 'static,
    BlockVerifierRouter: Service<zebra_consensus::Request, Response = block::Hash, Error = zebra_consensus::BoxError>
        + Clone
        + Send
        + Sync
        + 'static,
    <BlockVerifierRouter as Service<zebra_consensus::Request>>::Future: Send,
    SyncStatus: ChainSyncStatus + Clone + Send + Sync + 'static,
    AddressBook: AddressBookPeers + Clone + Send + Sync + 'static,
{
    // Paces block production on a network with proof-of-work disabled. See below.
    let mut block_pace: Option<tokio::time::Interval> = None;

    // Shut down the task when the template sender is dropped, or Zebra shuts down.
    while template_receiver.has_changed().is_ok() && !is_shutting_down() {
        // With proof-of-work disabled a block costs nothing to produce, so nothing paces the
        // chain: left alone the miner emits blocks as fast as the commit pipeline accepts
        // them, chain time falls behind the heights, and every timestamp ends up clamped to
        // median-time-past + 1. Pace it explicitly instead, at the one-block-per-target-
        // spacing the work used to buy -- in *chain* time, so a dilated clock scales it down.
        //
        // It ticks rather than sleeping a fixed amount after each block: a fixed sleep adds
        // the commit-and-new-template latency on top of the spacing, and the chain then runs
        // slower than asked.
        if let Some(pace) = block_pace.as_mut() {
            pace.tick().await;
        }

        // Get the latest block template, and mark the current value as seen.
        // We mark the value first to avoid missed updates.
        template_receiver.mark_as_seen();
        let template = template_receiver.cloned_watch_data();

        let Some(template) = template else {
            if solver_id == 0 {
                info!(
                    ?solver_id,
                    ?BLOCK_TEMPLATE_WAIT_TIME,
                    "solver waiting for initial block template"
                );
            } else {
                debug!(
                    ?solver_id,
                    ?BLOCK_TEMPLATE_WAIT_TIME,
                    "solver waiting for initial block template"
                );
            }

            // Skip the wait if we didn't get a template because we are shutting down.
            if !is_shutting_down() {
                zebra_debug_time::sleep(BLOCK_TEMPLATE_WAIT_TIME).await;
            }

            continue;
        };

        let height = template.coinbase_height().expect("template is valid");

        // Set up the cancellation conditions for the miner.
        let mut cancel_receiver = template_receiver.clone();
        let old_header = zebra_chain::block::Header::clone(&template.header);
        let cancel_fn = move || match cancel_receiver.has_changed() {
            // Guard against get_block_template() providing an identical header. This could happen
            // if something irrelevant to the block data changes, the time was within 1 second, or
            // there is a spurious channel change.
            Ok(has_changed) => {
                cancel_receiver.mark_as_seen();

                // We only need to check header equality, because the block data is bound to the
                // header.
                if has_changed
                    && Some(old_header.clone())
                        != cancel_receiver
                            .cloned_watch_data()
                            .map(|b| zebra_chain::block::Header::clone(&b.header))
                {
                    Err(SolverCancelled)
                } else {
                    Ok(())
                }
            }
            // If the sender was dropped, we're likely shutting down, so cancel the solver.
            Err(_sender_dropped) => Err(SolverCancelled),
        };

        // Mine at least one block using the equihash solver.
        let Ok(blocks) = mine_a_block(solver_id, low_priority, skip_pow, template, cancel_fn).await else {
            // If the solver was cancelled, we're either shutting down, or we have a new template.
            if solver_id == 0 {
                info!(
                    ?height,
                    ?solver_id,
                    new_template = ?template_receiver.has_changed(),
                    shutting_down = ?is_shutting_down(),
                    "solver cancelled: getting a new block template or shutting down"
                );
            } else {
                debug!(
                    ?height,
                    ?solver_id,
                    new_template = ?template_receiver.has_changed(),
                    shutting_down = ?is_shutting_down(),
                    "solver cancelled: getting a new block template or shutting down"
                );
            }

            // If the blockchain is changing rapidly, limit how often we'll update the template.
            // But if we're shutting down, do that immediately.
            if template_receiver.has_changed().is_ok() && !is_shutting_down() {
                zebra_debug_time::sleep(BLOCK_TEMPLATE_REFRESH_LIMIT).await;
            }

            continue;
        };

        // Submit the newly mined blocks to the verifiers.
        //
        // TODO: if there is a new template (`cancel_fn().is_err()`), and
        //       GetBlockTemplate.submit_old is false, return immediately, and skip submitting the
        //       blocks.
        let mut any_success = false;
        for block in blocks {
            let data = block
                .zcash_serialize_to_vec()
                .expect("serializing to Vec never fails");

            match rpc.submit_block(HexData(data), None).await {
                Ok(success) => {
                    info!(
                        ?height,
                        hash = ?block.hash(),
                        ?solver_id,
                        ?success,
                        "successfully mined a new block",
                    );
                    any_success = true;
                    // One solver run can yield several valid solutions, and at the difficulty
                    // floor -- where a fast-forwarded test network sits -- most of them are.
                    // They are all siblings at the same height, so at most one can ever win;
                    // submitting the rest only makes every node on the network verify and
                    // commit blocks it will immediately discard. Stop at the one that stuck.
                    break;
                }
                Err(error) => info!(
                    ?height,
                    hash = ?block.hash(),
                    ?solver_id,
                    ?error,
                    "validating a newly mined block failed, trying again",
                ),
            }
        }

        // Start re-mining quickly after a failed solution.
        // If there's a new template, we'll use it, otherwise the existing one is ok.
        if !any_success {
            // If the blockchain is changing rapidly, limit how often we'll update the template.
            // But if we're shutting down, do that immediately.
            if template_receiver.has_changed().is_ok() && !is_shutting_down() {
                zebra_debug_time::sleep(BLOCK_TEMPLATE_REFRESH_LIMIT).await;
            }
            continue;
        }

        if skip_pow && block_pace.is_none() {
            let spacing = NetworkUpgrade::target_spacing_for_height(rpc.network(), height)
                .to_std()
                .unwrap_or(Duration::from_secs(1));
            let mut pace = zebra_debug_time::interval(spacing);
            // The first tick is immediate; spend it here so the tick at the top of the next
            // iteration is the one that waits.
            pace.tick().await;
            block_pace = Some(pace);
        }

        // Wait for the new block to verify, and the RPC task to pick up a new template.
        // But don't wait too long, we could have mined on a fork.
        //
        // Templates at or below the height we just won are skipped rather than accepted:
        // while our block is still being committed the template task keeps serving the old
        // tip, and taking one of those starts a full solver run on a height we have already
        // beaten. On a fast chain that was throwing away most of the mining -- 1.6 solves
        // per height. Waiting for a strictly higher template is what this wait was always
        // for; it just used to end on the first template of any height.
        let deadline = zebra_debug_time::deadline(BLOCK_MINING_WAIT_TIME);
        loop {
            let mined_a_stale_height = tokio::select! {
                shutdown_result = template_receiver.changed() => {
                    shutdown_result?;
                    template_receiver
                        .cloned_watch_data()
                        .and_then(|block| block.coinbase_height())
                        .is_none_or(|next_height| next_height <= height)
                }
                // Don't wait past the deadline: we could have mined on a fork, in which case
                // no higher template is coming.
                _ = tokio::time::sleep_until(deadline) => false,
            };

            if !mined_a_stale_height {
                break;
            }
        }
    }

    Ok(())
}

/// Mines one or more blocks based on `template`. Calculates equihash solutions, checks difficulty,
/// and returns as soon as it has at least one block. Uses a different nonce range for each
/// `solver_id`.
///
/// If `cancel_fn()` returns an error, returns early with `Err(SolverCancelled)`.
///
/// See [`run_mining_solver()`] for more details.
pub async fn mine_a_block<F>(
    solver_id: u8,
    low_priority: bool,
    skip_pow: bool,
    template: Arc<Block>,
    cancel_fn: F,
) -> Result<AtLeastOne<Block>, SolverCancelled>
where
    F: FnMut() -> Result<(), SolverCancelled> + Send + Sync + 'static,
{
    // TODO: Replace with Arc::unwrap_or_clone() when it stabilises:
    // https://github.com/rust-lang/rust/issues/93610
    let mut header = zebra_chain::block::Header::clone(&template.header);

    // Use a different nonce for each solver thread.
    // Change both the first and last bytes, so we don't have to care if the nonces are incremented in
    // big-endian or little-endian order. And we can see the thread that mined a block from the nonce.
    *header.nonce.first_mut().unwrap() = solver_id;
    *header.nonce.last_mut().unwrap() = solver_id;

    // A network with proof-of-work disabled never checks the solution or the difficulty of
    // the blocks it accepts, so searching for a real one is pure cost -- and on this machine
    // it is the single most expensive thing the node does. Stamp the same null solution a
    // block *proposal* carries and hand the block straight back. `run_mining_solver` paces
    // the resulting free blocks to the target spacing, which is the job the work was doing.
    if skip_pow {
        header.solution = Solution::for_proposal();

        let mut block = (*template).clone();
        block.header = Arc::new(header);

        return Ok(vec![block]
            .try_into()
            .expect("a one-element vec is an AtLeastOne"));
    }

    // Mine one or more blocks using the solver, in a blocking thread. That thread runs at
    // the lowest OS priority unless the operator turned that off: on a busy node the
    // scheduler gives a nice-19 solver a small fraction of a core, which is right for a
    // real node and wrong for a test network built to run fast.
    let span = Span::current();
    let solved_headers =
        tokio::task::spawn_blocking(move || span.in_scope(move || {
            // Asking for a *higher* priority needs CAP_SYS_NICE, so the fast path just
            // leaves the thread at the runtime's default rather than requesting anything.
            let mut builder = ThreadBuilder::default().name("zebra-miner");
            if low_priority {
                builder = builder.priority(ThreadPriority::Min);
            }
            let miner_thread_handle = builder.spawn(move |priority_result| {
                if let Err(error) = priority_result {
                    info!(?error, "could not set the miner thread priority: running at default priority");
                }

                Solution::solve(header, cancel_fn)
            }).expect("unable to spawn miner thread");

            miner_thread_handle.wait_for_panics()
        }))
        .wait_for_panics()
        .await?;

    // Modify the template into solved blocks.

    // TODO: Replace with Arc::unwrap_or_clone() when it stabilises
    let block = (*template).clone();

    let solved_blocks: Vec<Block> = solved_headers
        .into_iter()
        .map(|header| {
            let mut block = block.clone();
            block.header = Arc::new(header);
            block
        })
        .collect();

    Ok(solved_blocks
        .try_into()
        .expect("a 1:1 mapping of AtLeastOne produces at least one block"))
}
