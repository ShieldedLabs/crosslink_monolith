//! `start` subcommand - entry point for starting a zebra node
//!
//! ## Application Structure
//!
//! A zebra node consists of the following major services and tasks:
//!
//! Peers:
//!  * Peer Connection Pool Service
//!    * primary external interface for outbound requests from this node to remote peers
//!    * accepts requests from services and tasks in this node, and sends them to remote peers
//!  * Peer Discovery Service
//!    * maintains a list of peer addresses, and connection priority metadata
//!    * discovers new peer addresses from existing peer connections
//!    * initiates new outbound peer connections in response to demand from tasks within this node
//!  * Peer Cache Service
//!    * Reads previous peer cache on startup, and adds it to the configured DNS seed peers
//!    * Periodically updates the peer cache on disk from the latest address book state
//!
//! Blocks & Mempool Transactions:
//!  * Consensus Service
//!    * handles all validation logic for the node
//!    * verifies blocks using zebra-chain, then stores verified blocks in zebra-state
//!    * verifies mempool and block transactions using zebra-chain and zebra-script,
//!      and returns verified mempool transactions for mempool storage
//!  * Inbound Service
//!    * primary external interface for inbound peer requests to this node
//!    * handles requests from peers for network data, chain data, and mempool transactions
//!    * spawns download and verify tasks for each gossiped block
//!    * sends gossiped transactions to the mempool service
//!
//! Blocks:
//!  * Sync Task
//!    * runs in the background and continuously queries the network for
//!      new blocks to be verified and added to the local state
//!    * spawns download and verify tasks for each crawled block
//!  * State Service
//!    * contextually verifies blocks
//!    * handles in-memory storage of multiple non-finalized chains
//!    * handles permanent storage of the best finalized chain
//!  * Old State Version Cleanup Task
//!    * deletes outdated state versions
//!  * Block Gossip Task
//!    * runs in the background and continuously queries the state for
//!      newly committed blocks to be gossiped to peers
//!  * Block Notify Task
//!    * if the user has configured a `notify.block_notify_command`, runs that command
//!      whenever the best chain tip changes (Zebra's equivalent of zcashd's `-blocknotify`)
//!  * Progress Task
//!    * logs progress towards the chain tip
//!
//! Block Mining:
//!  * Internal Miner Task
//!    * if the user has configured Zebra to mine blocks, spawns tasks to generate new blocks,
//!      and submits them for verification. This automatically shares these new blocks with peers.
//!
//! Mempool Transactions:
//!  * Mempool Service
//!    * activates when the syncer is near the chain tip
//!    * spawns download and verify tasks for each crawled or gossiped transaction
//!    * handles in-memory storage of unmined transactions
//!  * Queue Checker Task
//!    * runs in the background, polling the mempool to store newly verified transactions
//!  * Transaction Gossip Task
//!    * runs in the background and gossips newly added mempool transactions
//!      to peers
//!
//! Remote Procedure Calls:
//!  * JSON-RPC Service
//!    * answers RPC client requests using the State Service and Mempool Service
//!    * submits client transactions to the node's mempool
//!
//! Zebra also has diagnostic support:
//! * [metrics](https://github.com/ZcashFoundation/zebra/blob/main/book/src/user/metrics.md)
//! * [tracing](https://github.com/ZcashFoundation/zebra/blob/main/book/src/user/tracing.md)
//! * [progress-bar](https://docs.rs/howudoin/0.1.1/howudoin)
//!
//! Some of the diagnostic features are optional, and need to be enabled at compile-time.

use std::{sync::Arc, time::Duration};

use abscissa_core::{config, Command, FrameworkError};
use color_eyre::eyre::{eyre, Report};
use futures::FutureExt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::Path;
use tokio::{pin, select, sync::{oneshot, watch}, time::timeout};
use tower::{builder::ServiceBuilder, util::BoxService, ServiceExt};
use tracing_futures::Instrument;

use zebra_chain::block::genesis::regtest_genesis_block;
use zebra_chain::parameters::{Network, HardForkSchedule};
use zebra_consensus::router::BackgroundTaskHandles;
use zebra_rpc::{methods::RpcImpl, server::RpcServer, SubmitBlockChannel};

use crate::{
    application::{build_version, user_agent, LAST_WARN_ERROR_LOG_SENDER},
    components::{
        health,
        inbound::{self, InboundSetupData, MAX_INBOUND_RESPONSE_TIME},
        mempool::{self, Mempool},
        notify::{self, BlockNotifyError},
        sync::{self, show_block_chain_progress, VERIFICATION_PIPELINE_SCALING_MULTIPLIER},
        tokio::{RuntimeRun, TokioComponent},
        zcashd_compat, ChainSync, Inbound,
    },
    config::ZebradConfig,
    prelude::*,
};

#[cfg(feature = "internal-miner")]
use crate::components;

use tower::Service;

/// Start the application (default command)
#[derive(Command, Debug, Default, clap::Parser)]
pub struct StartCmd {
    /// Filter strings which override the config file and defaults
    #[clap(help = "tracing filters which override the zebrad.toml config")]
    filters: Vec<String>,

    /// Enable zcashd-compat mode.
    #[clap(long)]
    zcashd_compat: bool,

    /// Continue startup even when zcashd-compat preflight detects minimum hardware shortfalls.
    #[clap(long = "unsafe-low-specs")]
    unsafe_low_specs: bool,
}

/// Warns if Linux TCP slow-start-after-idle is enabled, which significantly
/// reduces single-peer throughput for block propagation.
///
/// See `book/src/user/troubleshooting.md`.
#[cfg(target_os = "linux")]
fn check_tcp_slow_start_after_idle() {
    const PATH: &str = "/proc/sys/net/ipv4/tcp_slow_start_after_idle";

    let raw = match std::fs::read_to_string(PATH) {
        Ok(raw) => raw,
        Err(error) => {
            debug!(
                ?error,
                path = PATH,
                "could not read TCP sysctl, skipping check"
            );
            return;
        }
    };

    if raw.trim() == "0" {
        return;
    }

    warn!(
        setting = "net.ipv4.tcp_slow_start_after_idle",
        "TCP slow-start-after-idle is enabled, which resets TCP's congestion window \
         between block requests and significantly reduces single-peer throughput for \
         block propagation. \
         Hint: set `net.ipv4.tcp_slow_start_after_idle=0` via sysctl. \
         See https://zebra.zfnd.org/user/troubleshooting.html#linux-tcp-tuning-for-block-propagation"
    );
}

#[cfg(not(target_os = "linux"))]
fn check_tcp_slow_start_after_idle() {}

impl StartCmd {
    /// Extra time Zebra waits for the zcashd-compat supervisor task beyond the
    /// child's `shutdown_grace_period`. The supervisor's `terminate_child` waits
    /// the full grace period before its SIGKILL last resort, so the outer wait
    /// must be strictly longer or aborting the task races the graceful path.
    const ZCASHD_COMPAT_SHUTDOWN_TIMEOUT_MARGIN: std::time::Duration =
        std::time::Duration::from_secs(30);

    /// Returns the Zebra P2P address supervised zcashd should `-connect` to.
    ///
    /// Uses `zcashd_compat.p2p_connect_addr` when set, otherwise Zebra's bound
    /// P2P listener, substituting loopback for unspecified addresses so
    /// zcashd gets a dialable target on the same host.
    fn zcashd_compat_p2p_connect_addr(
        config: &ZebradConfig,
        local_listener: SocketAddr,
    ) -> SocketAddr {
        if let Some(addr) = config.zcashd_compat.p2p_connect_addr {
            return addr;
        }

        if local_listener.ip().is_unspecified() {
            // Substitute the loopback address of the same IP family: an
            // IPv6-only listener is not reachable via 127.0.0.1.
            match local_listener.ip() {
                IpAddr::V4(_) => SocketAddr::from(([127, 0, 0, 1], local_listener.port())),
                IpAddr::V6(_) => {
                    SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), local_listener.port())
                }
            }
        } else {
            local_listener
        }
    }

    /// Returns the default inbound peer IPs that always receive block gossip in
    /// zcashd-compat mode.
    fn zcashd_compat_default_block_gossip_peer_ips() -> Vec<IpAddr> {
        vec![
            IpAddr::V4(Ipv4Addr::LOCALHOST),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
        ]
    }

    /// Returns the supervisor shutdown timeout when zcashd-compat `zcashd` supervision is active.
    ///
    /// This is the configured `shutdown_grace_period` plus a fixed margin, so the
    /// supervisor task always gets to finish its own SIGTERM → grace → SIGKILL
    /// sequence before Zebra gives up on the task.
    fn zcashd_compat_supervisor_shutdown_timeout(
        config: &ZebradConfig,
    ) -> Option<std::time::Duration> {
        (config.zcashd_compat.enabled && config.zcashd_compat.manage_zcashd).then_some(
            config
                .zcashd_compat
                .shutdown_grace_period
                .saturating_add(Self::ZCASHD_COMPAT_SHUTDOWN_TIMEOUT_MARGIN),
        )
    }

    /// Returns `false` so Zebra keeps running if zcashd-compat supervision exits unexpectedly.
    fn zcashd_compat_supervisor_should_exit(
        zcashd_compat_result: Result<Result<(), Report>, tokio::task::JoinError>,
    ) -> bool {
        zcashd_compat::set_supervision_unexpectedly_disabled_metrics();

        match zcashd_compat_result {
            Ok(Ok(())) => {
                warn!(
                    "zcashd-compat supervisor task exited unexpectedly in supervision mode; \
                     continuing without zcashd supervision"
                );
            }
            Ok(Err(err)) => {
                warn!(
                    ?err,
                    "zcashd-compat supervisor task failed in supervision mode; \
                     continuing without zcashd supervision"
                );
            }
            Err(join_err) => {
                warn!(
                    ?join_err,
                    "zcashd-compat supervisor task panicked in supervision mode; \
                     continuing without zcashd supervision"
                );
            }
        }

        false
    }

    async fn start(&self) -> Result<(), Report> {

        let config = APPLICATION.config();

        #[cfg(not(feature = "viz_gui"))]
        {
            if config.crosslink.disable_the_headless_wallet == false {
                let wallet_state = Arc::new(std::sync::Mutex::new(wallet::WalletState::new()));
                tokio::spawn(zebra_crosslink::wallet::wallet_main(wallet_state));
            }
        }
        *zebra_crosslink::wallet::GUI_ENABLE_MINE.lock().unwrap() = config.mining.internal_miner;

        let is_regtest = config.network.network.is_regtest();

        let is_clt0 = 'is_clt0: { // Crosslink_Testnet_0
            if let Network::Testnet(params) = &config.network.network {
                if params.network_magic().0 == [b'C',b'l',b'T',b'0'] {
                    break 'is_clt0 true;
                }
            }
            false
        };


        // workshop-specific key seed
        let global_seed = loop {
            use std::{fs::File, io::Read, io::Write};
            use rand::Rng;

            let mut key_path = config.state.cache_dir.clone();
            let _ = std::fs::create_dir_all(key_path.clone());

            key_path.push("secret.seed");
            let mut seed = [0u8; 32];
            println!("getting key seed from {:?}", key_path);
            if let Ok(mut f) = File::open(key_path.clone()) {
                match f.read_exact(&mut seed) {
                    Ok(()) => break seed,
                    Err(err) => warn!("couldn't read seed at {key_path:?} ({err:?}); creating a new one"),
                }
            }

            // all else failed, create/replace file from scratch
            seed = rand::thread_rng().gen();
            let mut f = File::create(key_path).expect("couldn't create seed file; add one manually");
            f.write(&seed).expect("couldn't write to seed file; add one manually");

            break seed;
        };
        *wallet::GLOBAL_SEED.lock().unwrap() = Some(global_seed);

        let path_to_pos_store_file = if config.state.ephemeral { std::path::PathBuf::new() } else {
            let mut key_path = config.state.cache_dir.clone();
            let _ = std::fs::create_dir_all(key_path.clone());

            key_path.push("pos.chain");
            key_path
        };


        let config = if is_regtest {
            fn add_to_port(mut addr: std::net::SocketAddr, addend: u16) -> std::net::SocketAddr {
                addr.set_port(addr.port() + addend);
                addr
            }
            let nextest_slot: u16 = if let Ok(str) = std::env::var("NEXTEST_TEST_GLOBAL_SLOT") {
                if let Ok(slot) = str.parse::<u16>() {
                    slot
                } else {
                    0
                }
            } else {
                0
            };

            Arc::new(ZebradConfig {
                mempool: mempool::Config {
                    debug_enable_at_height: Some(0),
                    ..config.mempool
                },
                network: zebra_network::config::Config {
                    listen_addr: add_to_port(config.network.listen_addr.clone(), nextest_slot * 7),
                    ..config.network.clone()
                },
                rpc: zebra_rpc::config::rpc::Config {
                    listen_addr: config
                        .rpc
                        .listen_addr
                        .clone()
                        .map(|addr| add_to_port(addr, nextest_slot * 7)),
                    ..config.rpc.clone()
                },
                ..Arc::unwrap_or_clone(config)
            })
        } else if is_clt0 {
            // debug_enable_at_height: Some(0)
            Arc::new(ZebradConfig {
                // mempool: mempool::Config {
                //     debug_enable_at_height: Some(0),
                //     ..config.mempool
                // },
                mining: zebra_rpc::config::mining::Config {
                    miner_address: Some(config.mining.miner_address.clone().unwrap_or_else(||{
                        use zcash_address::ToAddress;

                        let t_addr = wallet::default_p2pkh_from_entropy(&config.network.network, &global_seed).expect("unable to initialize miner");
                        info!("Miner address unspecified. Mining to {}", wallet::string_from_t_addr(&config.network.network, t_addr));
                        t_addr.to_zcash_address(config.network.network.kind().into())
                    })),
                    // TODO: extra_coinbase_data
                    ..config.mining.clone()
                },
                ..Arc::unwrap_or_clone(config)
            })
        } else {
            config
        };


        info!("initializing node state");
        let (_, max_checkpoint_height) = zebra_consensus::router::init_checkpoint_list(
            config.consensus.clone(),
            &config.network.network,
        );

        let zcashd_compat_block_gossip_peer_ips = if config.zcashd_compat.enabled {
            if config.zcashd_compat.block_gossip_peer_ips.is_empty() {
                // The sidecar privileges (pinned gossip, reserved slot, stall
                // exemption) match on the sidecar's *source* IP. In
                // cross-container/cross-host topologies that source is not
                // loopback, so the default list would silently strip the
                // sidecar of everything this mode provides.
                if config
                    .zcashd_compat
                    .p2p_connect_addr
                    .is_some_and(|addr| !addr.ip().is_loopback())
                {
                    warn!(
                        p2p_connect_addr = ?config.zcashd_compat.p2p_connect_addr,
                        "zcashd_compat.p2p_connect_addr is not loopback, but \
                         zcashd_compat.block_gossip_peer_ips defaults to loopback only; \
                         if the sidecar connects from a non-loopback IP, set \
                         block_gossip_peer_ips to that IP or it will not receive \
                         pinned block gossip"
                    );
                }

                Self::zcashd_compat_default_block_gossip_peer_ips()
            } else {
                config.zcashd_compat.block_gossip_peer_ips.clone()
            }
        } else {
            Vec::new()
        };

        if config.zcashd_compat.enabled {
            // Preflight does blocking filesystem and /proc reads, and can hash
            // the cached zcashd binary, so keep it off the async runtime.
            let preflight_config = config.clone();
            let unsafe_low_specs = self.unsafe_low_specs;
            tokio::task::spawn_blocking(move || {
                zcashd_compat::run_preflight(&preflight_config, unsafe_low_specs)
            })
            .await
            .map_err(|err| eyre!("failed to join zcashd-compat preflight task: {err}"))??;
        }

        let resolved_zcashd_path = if config.zcashd_compat.enabled
            && config.zcashd_compat.manage_zcashd
        {
            let zcashd_compat_config = config.zcashd_compat.clone();
            let state_cache_dir = config.state.cache_dir.clone();
            Some(
                tokio::task::spawn_blocking(move || {
                    zcashd_compat::resolve_zcashd_binary_path(
                        &zcashd_compat_config,
                        &state_cache_dir,
                    )
                })
                .await
                .map_err(|err| eyre!("failed to join managed zcashd binary resolver: {err}"))??,
            )
        } else {
            None
        };

        info!("opening database, this may take a few minutes");

        let actual_closure: Arc<std::sync::Mutex<Option<zebra_state::ClosureToCallIntoCrosslinkFromState>>> = Arc::new(std::sync::Mutex::new(None));
        let actual_closure2 = Arc::clone(&actual_closure);
        let actual_closure3 = Arc::clone(&actual_closure);

        let mut state_config = config.state.clone();
        // config.crosslink.hardforks is already canonical and merged (see ZebradConfig::load)
        state_config.hardfork_schedule = Arc::new(HardForkSchedule::from_canonical(config.crosslink.hardforks.clone()));

        let (state_service, read_only_state_service, latest_chain_tip, chain_tip_change, block_writer) =
            zebra_state::spawn_init(
                state_config,
                &config.network.network,
                max_checkpoint_height,
                config.sync.checkpoint_verify_concurrency_limit
                    * (VERIFICATION_PIPELINE_SCALING_MULTIPLIER + 1),
                Arc::new(move |fat_pointer_a, fat_pointer_b, height| {
                    if let Some(closure) = actual_closure.lock().unwrap().as_mut() {
                        (closure)(fat_pointer_a, fat_pointer_b, height)
                    } else {
                        tracing::error!("State -> Crosslink closure not yet initialized.");
                        None
                    }
                }),
            )
            .await
            .expect("failed to join the state initialisation task");

        info!("logging database metrics on startup");
        read_only_state_service.log_db_metrics();

        // Drive the finalizer-slashing bond index: catch up the backlog at startup, then keep up live.
        let state = ServiceBuilder::new()
            .buffer(Self::state_buffer_bound())
            .service(state_service);

        info!("initializing network");
        // The service that our node uses to respond to requests by peers. The
        // load_shed middleware ensures that we reduce the size of the peer set
        // in response to excess load.
        //
        // # Security
        //
        // This layer stack is security-sensitive, modifying it can cause hangs,
        // or enable denial of service attacks.
        //
        // See `zebra_network::Connection::drive_peer_request()` for details.
        let (setup_tx, setup_rx) = oneshot::channel();
        let inbound = ServiceBuilder::new()
            .load_shed()
            .buffer(inbound::downloads::MAX_INBOUND_CONCURRENCY)
            .timeout(MAX_INBOUND_RESPONSE_TIME)
            .service(Inbound::new(
                config.sync.full_verify_concurrency_limit,
                setup_rx,
            ));

        let (peer_set, address_book, misbehavior_sender) =
            zebra_network::init_with_block_gossip_peer_ips(
                config.network.clone(),
                inbound,
                latest_chain_tip.clone(),
                user_agent(),
                zcashd_compat_block_gossip_peer_ips,
            )
            .await;

        // Start health server if configured (after sync_status is available)

        info!("initializing verifiers");
        let (tx_verifier_setup_tx, tx_verifier_setup_rx) = oneshot::channel();
        let (block_verifier_router, tx_verifier, consensus_task_handles, max_checkpoint_height) =
            zebra_consensus::router::init(
                config.consensus.clone(),
                &config.network.network,
                state.clone(),
                tx_verifier_setup_rx,
            )
            .await;

        info!("initializing syncer");
        let (mut syncer, sync_status) = ChainSync::new(
            &config,
            max_checkpoint_height,
            peer_set.clone(),
            block_verifier_router.clone(),
            state.clone(),
            latest_chain_tip.clone(),
            misbehavior_sender.clone(),
        );

        info!("initializing mempool");
        let (mempool, mempool_transaction_subscriber) = Mempool::new(
            &config.network.network,
            &config.mempool,
            peer_set.clone(),
            state.clone(),
            tx_verifier,
            sync_status.clone(),
            latest_chain_tip.clone(),
            chain_tip_change.clone(),
            misbehavior_sender.clone(),
        );
        let mempool = BoxService::new(mempool);
        let mempool = ServiceBuilder::new()
            .buffer(mempool::downloads::MAX_INBOUND_CONCURRENCY)
            .service(mempool);

        if tx_verifier_setup_tx.send(mempool.clone()).is_err() {
            warn!("error setting up the transaction verifier with a handle to the mempool service");
        };

        info!("fully initializing inbound peer request handler");
        // Fully start the inbound service as soon as possible
        let setup_data = InboundSetupData {
            address_book: address_book.clone(),
            block_download_peer_set: peer_set.clone(),
            block_verifier: block_verifier_router.clone(),
            mempool: mempool.clone(),
            state: state.clone(),
            latest_chain_tip: latest_chain_tip.clone(),
            misbehavior_sender,
        };
        setup_tx
            .send(setup_data)
            .map_err(|_| eyre!("could not send setup data to inbound service"))?;
        // And give it time to clear its queue
        tokio::task::yield_now().await;

        // Create a channel to send mined blocks to the gossip task
        let submit_block_channel = SubmitBlockChannel::new();

        let mempool2 = mempool.clone();
        info!("spawning tfl service task");
        let (tfl_handle, tfl_service_task_handle) = {
            let state = state.clone();
            let read_only_state_service = read_only_state_service.clone();
            zebra_crosslink::service::spawn_new_tfl_service(
                is_regtest,
                global_seed,
                path_to_pos_store_file,
                Arc::new(move |req| {
                    let state = state.clone();
                    Box::pin(async move { state.clone().ready().await?.call(req).await })
                }),
                Arc::new(move |req| {
                    let read_only_state_service = read_only_state_service.clone();
                    Box::pin(async move { read_only_state_service.clone().ready().await?.call(req).await })
                }),
                Arc::new(move |req| {
                    let mempool = mempool2.clone();
                    Box::pin(async move { mempool.clone().ready().await?.call(req).await })
                }),
                config.crosslink.clone(),
                actual_closure2,
            )
        };
        let tfl_service = BoxService::new(tfl_handle);
        let tfl_service = ServiceBuilder::new().buffer(1).service(tfl_service);

        // new_network is the block pipeline, not an option: every block entering this node
        // goes through it, so there is nothing to gate.
        {

            let config = Arc::clone(&config);
            let sync_read_state = read_only_state_service.clone();

            // new_network owns the block writer, so it is the only thing that can commit
            // genesis. It does so before waiting for a tip -- otherwise it would wait forever
            // for a block only it can write.
            let genesis_block_for_new_network: Arc<zebra_chain::block::Block> = if is_regtest {
                regtest_genesis_block()
            } else if is_clt0 {
                use zebra_chain::serialization::ZcashDeserialize;
                let genesis_bytes = include_bytes!("../../../ClT0-genesis.pow");
                Arc::new(zebra_chain::block::Block::zcash_deserialize(&genesis_bytes[..]).expect("hardcoded genesis must be valid"))
            } else {
                panic!("unhandled special-case genesis");
            };
            assert_eq!(genesis_block_for_new_network.hash(), config.network.network.genesis_hash(),
                "genesis hash does not match the configured network genesis; consider editing your config");
            let sync_block_verifier = block_verifier_router.clone();
            tokio::task::spawn_blocking(move || {
                use zebra_state::new_network::BlockCommitError;

                // Synchronous verification entry points. Passed as plain fn pointers because
                // zebra-state cannot depend on zebra-consensus (the dependency runs the other
                // way), so new_network cannot call these directly.
                let verify_fns = zebra_state::new_network::VerifyFns {
                    check_header: zebra_consensus::sync_verify::block_check_header,
                    check_body: zebra_consensus::sync_verify::block_check_body,
                    check_cheap: zebra_consensus::sync_verify::block_check_cheap,
                    verify_expensive: zebra_consensus::sync_verify::block_verify_expensive,
                };
                // The same state -> crosslink closure the state service holds, so new_network
                // can run the fat-pointer gate itself rather than discovering it at commit time.
                let crosslink_gate: zebra_state::ClosureToCallIntoCrosslinkFromState =
                    Arc::new(move |fat_pointer_a, fat_pointer_b, height| {
                        if let Some(closure) = actual_closure3.lock().unwrap().as_mut() {
                            (closure)(fat_pointer_a, fat_pointer_b, height)
                        } else {
                            tracing::error!("NewNet -> Crosslink closure not yet initialized.");
                            None
                        }
                    });
                zebra_state::new_network::sync(&config.state, sync_read_state, /* tfl_service2, */ tokio::runtime::Handle::current(), verify_fns, crosslink_gate, block_writer, genesis_block_for_new_network)
            });
        }

        // Launch RPC server
        let (rpc_impl, mut rpc_tx_queue_handle) = RpcImpl::new(
            config.network.network.clone(),
            config.mining.clone(),
            config.rpc.debug_force_finished_sync,
            build_version(),
            user_agent(),
            mempool.clone(),
            tfl_service.clone(),
            state.clone(),
            read_only_state_service.clone(),
            block_verifier_router.clone(),
            sync_status.clone(),
            latest_chain_tip.clone(),
            address_book.clone(),
            LAST_WARN_ERROR_LOG_SENDER.subscribe(),
            Some(submit_block_channel.sender()),
        );
        let rpc_impl = rpc_impl.with_end_of_support_height(
            sync::end_of_support::end_of_support_height(&config.network.network),
        );

        let rpc_task_handle = if config.rpc.listen_addr.is_some() {
            RpcServer::start(rpc_impl.clone(), config.rpc.clone())
                .await
                .expect("server should start")
        } else {
            tokio::spawn(std::future::pending().in_current_span())
        };

        let zcashd_compat_shutdown_timeout =
            Self::zcashd_compat_supervisor_shutdown_timeout(&config);
        let (zcashd_compat_shutdown_tx, zcashd_compat_shutdown_rx) = watch::channel(false);
        let mut zcashd_compat_task_handle = if let Some(resolved_zcashd_path) = resolved_zcashd_path
        {
            let local_listener = address_book
                .lock()
                .expect("unexpected panic in address book mutex guard")
                .local_listener_socket_addr();
            let supervisor_config = zcashd_compat::SupervisorConfig::new(
                &config.zcashd_compat,
                resolved_zcashd_path,
                &config.state.cache_dir,
                config.network.network.kind(),
                Self::zcashd_compat_p2p_connect_addr(&config, local_listener),
            );

            info!(
                connect = %supervisor_config.zebra_p2p_addr,
                "zcashd-compat mode enabled"
            );

            tokio::spawn(
                zcashd_compat::run_supervisor(supervisor_config, zcashd_compat_shutdown_rx)
                    .in_current_span(),
            )
        } else {
            if config.zcashd_compat.enabled {
                zcashd_compat::set_supervision_config_disabled_metrics();
                info!("zcashd-compat mode enabled: zcashd supervision disabled");
            }

            tokio::spawn(std::future::pending().in_current_span())
        };

        // TODO: Add a shutdown signal and start the server with `serve_with_incoming_shutdown()` if
        //       any related unit tests sometimes crash with memory errors
        let indexer_rpc_task_handle = {
            if let Some(indexer_listen_addr) = config.rpc.indexer_listen_addr {
                info!("spawning indexer RPC server");
                let (indexer_rpc_task_handle, _listen_addr) = zebra_rpc::indexer::server::init(
                    indexer_listen_addr,
                    read_only_state_service.clone(),
                    latest_chain_tip.clone(),
                    mempool_transaction_subscriber.clone(),
                )
                .await
                .map_err(|err| eyre!(err))?;

                indexer_rpc_task_handle
            } else {
                warn!("configure an indexer_listen_addr to start the indexer RPC server");
                tokio::spawn(std::future::pending().in_current_span())
            }
        };

        // Start concurrent tasks which don't add load to other tasks
        info!("spawning block gossip task");
        let block_gossip_task_handle = tokio::spawn(
            sync::gossip_best_tip_block_hashes(
                sync_status.clone(),
                chain_tip_change.clone(),
                peer_set.clone(),
                Some(submit_block_channel.receiver()),
            )
            .in_current_span(),
        );

        info!("spawning block notify task");
        let block_notify_task_handle: tokio::task::JoinHandle<Result<(), BlockNotifyError>> =
            if let Some(command) = config.notify.block_notify_command.clone() {
                tokio::spawn(
                    notify::run_block_notify(
                        command,
                        sync_status.clone(),
                        chain_tip_change.clone(),
                    )
                    .in_current_span(),
                )
            } else {
                tokio::spawn(std::future::pending().in_current_span())
            };

        info!("spawning mempool queue checker task");
        let mempool_queue_checker_task_handle = mempool::QueueChecker::spawn(mempool.clone());

        info!("spawning mempool transaction gossip task");
        let tx_gossip_task_handle = tokio::spawn(
            mempool::gossip_mempool_transaction_id(
                mempool_transaction_subscriber.subscribe(),
                peer_set.clone(),
            )
            .in_current_span(),
        );

        info!("spawning delete old databases task");
        let mut old_databases_task_handle = zebra_state::check_and_delete_old_state_databases(
            &config.state,
            &config.network.network,
        );

        info!("spawning progress logging task");
        let (chain_tip_metrics_sender, chain_tip_metrics_receiver) =
            health::ChainTipMetrics::channel();
        let progress_task_handle = tokio::spawn(
            show_block_chain_progress(
                config.network.network.clone(),
                latest_chain_tip.clone(),
                sync_status.clone(),
                chain_tip_metrics_sender,
            )
            .in_current_span(),
        );

        // Start health server if configured
        info!("initializing health endpoints");
        let (health_task_handle, _) = health::init(
            config.health.clone(),
            config.network.network.clone(),
            chain_tip_metrics_receiver,
            sync_status.clone(),
            address_book.clone(),
        )
        .await;

        // Spawn never ending end of support task.
        info!("spawning end of support checking task");
        let end_of_support_task_handle = tokio::spawn(
            sync::end_of_support::start(config.network.network.clone(), latest_chain_tip.clone())
                .in_current_span(),
        );

        // Give the inbound service more time to clear its queue,
        // then start concurrent tasks that can add load to the inbound service
        // (by opening more peer connections, so those peers send us requests)
        tokio::task::yield_now().await;

        // The crawler only activates immediately in tests that use mempool debug mode
        info!("spawning mempool crawler task");
        let mempool_crawler_task_handle = mempool::Crawler::spawn(
            &config.mempool,
            peer_set,
            mempool.clone(),
            sync_status.clone(),
            chain_tip_change.clone(),
        );

        info!("spawning syncer task");
        let syncer_task_handle = if is_regtest || is_clt0 {
            // Genesis is committed by new_network at startup: it owns the block writer, so no
            // one else can. See the genesis handling in `new_network::sync`.
            tokio::spawn(std::future::pending().in_current_span())
        } else {
            tokio::spawn(syncer.sync().in_current_span())
        };

        // And finally, spawn the internal Zcash miner, if it is enabled.
        //
        // TODO: add a config to enable the miner rather than a feature.

        // @Note: We have a GUI toggle that disables mining *inside* the miner task.
        //        This GUI toggle is initialized to the config `internal_miner` bool,
        //        which will ensure mining is off even when the task is alive and well.
        //        We want to allow the GUI to *re-enable* mining if the config had
        //        initialized with mining off, so we *don't* gate this task behind config.

        /* The logic here is hairy when expressed as Rust; the more readable JAI equivalent is this:

            #if Features.Internal_Miner {
                #if Features.Viz_Gui {
                    spawn_miner(); // unconditionally
                } else {
                    if config.internal_miner {
                        spawn_miner();
                    } else {
                        dummy();
                    }
                }
            } else {
                dummy();
            }

        */

        #[cfg(all(feature = "internal-miner", feature = "viz_gui"))]
        let miner_task_handle = {
            info!("spawning Zcash miner");
            components::miner::spawn_init(&config.network.network, &config.metrics, rpc_impl)
        };

        #[cfg(all(feature = "internal-miner", not(feature = "viz_gui")))]
        let miner_task_handle = if config.mining.is_internal_miner_enabled() {
            info!("spawning Zcash miner");
            components::miner::spawn_init(&config.network.network, &config.metrics, rpc_impl)
        } else {
            tokio::spawn(std::future::pending().in_current_span())
        };

        #[cfg(not(feature = "internal-miner"))]
        // Spawn a dummy miner task which doesn't do anything and never finishes.
        let miner_task_handle: tokio::task::JoinHandle<Result<(), Report>> =
            tokio::spawn(std::future::pending().in_current_span());

        info!("spawned initial Zebra tasks");

        // TODO: put tasks into an ongoing FuturesUnordered and a startup FuturesUnordered?

        // ongoing tasks
        // The managed zcashd sidecar can exit on its own (it is optional and may fail) while
        // Zebra keeps running, so its handle must be fused.
        let mut zcashd_compat_task_finished = false;
        let zcashd_compat_task_handle_fused = (&mut zcashd_compat_task_handle).fuse();
        pin!(zcashd_compat_task_handle_fused);

        pin!(rpc_task_handle);
        pin!(indexer_rpc_task_handle);
        pin!(syncer_task_handle);
        pin!(block_gossip_task_handle);
        pin!(block_notify_task_handle);
        pin!(mempool_crawler_task_handle);
        pin!(mempool_queue_checker_task_handle);
        pin!(tx_gossip_task_handle);
        pin!(tfl_service_task_handle);
        pin!(progress_task_handle);
        pin!(end_of_support_task_handle);
        pin!(miner_task_handle);

        // startup tasks
        let BackgroundTaskHandles {
            mut state_checkpoint_verify_handle,
        } = consensus_task_handles;

        let state_checkpoint_verify_handle_fused = (&mut state_checkpoint_verify_handle).fuse();
        pin!(state_checkpoint_verify_handle_fused);

        let old_databases_task_handle_fused = (&mut old_databases_task_handle).fuse();
        pin!(old_databases_task_handle_fused);

        // Lightwalletd gRPC server, served straight from the read state
        // service and mempool. This is lightwallet_server; it takes over the
        // legacy port, and the wallet connects to it unchanged.
        {
            let zebra_port_base = config.network.listen_addr.port();
            let lwd_port = zebra_port_base + 10001;
            let lwd_ctx = crate::lightwalletd::Ctx {
                rt: tokio::runtime::Handle::current(),
                read_state: read_only_state_service.clone(),
                mempool: mempool.clone(),
                tfl: tfl_service.clone(),
                tip: latest_chain_tip.clone(),
                mempool_events: mempool_transaction_subscriber.clone(),
                network: config.network.network.clone(),
            };
            crate::lightwalletd::lightwalletd_spawn(lwd_ctx, lwd_port, zebra_port_base + 10000);
            *zebra_crosslink::wallet::wallet_main_lightwalletd_port.lock().unwrap() = lwd_port;
        }

        // Wait for tasks to finish
        let exit_status = loop {
            let mut exit_when_task_finishes = true;

            let result = select! {
                rpc_join_result = &mut rpc_task_handle => {
                    let rpc_server_result = rpc_join_result
                        .expect("unexpected panic in the rpc task");
                    info!(?rpc_server_result, "rpc task exited");
                    Ok(())
                }

                rpc_tx_queue_result = &mut rpc_tx_queue_handle => {
                    rpc_tx_queue_result
                        .expect("unexpected panic in the rpc transaction queue task");
                    info!("rpc transaction queue task exited");
                    Ok(())
                }

                indexer_rpc_join_result = &mut indexer_rpc_task_handle => {
                    let indexer_rpc_server_result = indexer_rpc_join_result
                        .expect("unexpected panic in the indexer task");
                    info!(?indexer_rpc_server_result, "indexer rpc task exited");
                    Ok(())
                }

                sync_result = &mut syncer_task_handle => sync_result
                    .expect("unexpected panic in the syncer task")
                    .map(|_| info!("syncer task exited")),

                block_gossip_result = &mut block_gossip_task_handle => block_gossip_result
                    .expect("unexpected panic in the chain tip block gossip task")
                    .map(|_| info!("chain tip block gossip task exited"))
                    .map_err(|e| eyre!(e)),

                block_notify_result = &mut block_notify_task_handle => block_notify_result
                    .expect("unexpected panic in the block notify task")
                    .map(|_| info!("block notify task exited"))
                    .map_err(|e| eyre!(e)),

                mempool_crawl_result = &mut mempool_crawler_task_handle => mempool_crawl_result
                    .expect("unexpected panic in the mempool crawler")
                    .map(|_| info!("mempool crawler task exited"))
                    .map_err(|e| eyre!(e)),

                mempool_queue_result = &mut mempool_queue_checker_task_handle => mempool_queue_result
                    .expect("unexpected panic in the mempool queue checker")
                    .map(|_| info!("mempool queue checker task exited"))
                    .map_err(|e| eyre!(e)),

                tx_gossip_result = &mut tx_gossip_task_handle => tx_gossip_result
                    .expect("unexpected panic in the transaction gossip task")
                    .map(|_| info!("transaction gossip task exited"))
                    .map_err(|e| eyre!(e)),

                tfl_service_result = &mut tfl_service_task_handle => tfl_service_result
                    .expect("unexpected panic in the tfl service task")
                    .map(|_| info!("tfl service task exited"))
                    .map_err(|e| eyre!(e)),

                // The progress task runs forever, unless it panics.
                // So we don't need to provide an exit status for it.
                progress_result = &mut progress_task_handle => {
                    info!("chain progress task exited");
                    progress_result
                        .expect("unexpected panic in the chain progress task");
                }

                end_of_support_result = &mut end_of_support_task_handle => end_of_support_result
                    .expect("unexpected panic in the end of support task")
                    .map(|_| info!("end of support task exited")),

                // We also expect the state checkpoint verify task to finish.
                state_checkpoint_verify_result = &mut state_checkpoint_verify_handle_fused => {
                    state_checkpoint_verify_result
                        .unwrap_or_else(|_| panic!(
                            "unexpected panic checking previous state followed the best chain"));

                    exit_when_task_finishes = false;
                    Ok(())
                }

                // And the old databases task should finish while Zebra is running.
                old_databases_result = &mut old_databases_task_handle_fused => {
                    old_databases_result
                        .unwrap_or_else(|_| panic!(
                            "unexpected panic deleting old database directories"));

                    exit_when_task_finishes = false;
                    Ok(())
                }

                miner_result = &mut miner_task_handle => miner_result
                    .expect("unexpected panic in the miner task")
                    .map(|_| info!("miner task exited")),

                zcashd_compat_result = &mut zcashd_compat_task_handle_fused => {
                    zcashd_compat_task_finished = true;
                    exit_when_task_finishes =
                        Self::zcashd_compat_supervisor_should_exit(zcashd_compat_result);
                    Ok(())
                },
            };

            // Stop Zebra if a task finished and returned an error,
            // or if an ongoing task exited.
            if let Err(err) = result {
                break Err(err);
            }

            if exit_when_task_finishes {
                break Ok(());
            }
        };

        info!("exiting Zebra because an ongoing task exited: asking other tasks to stop");

        // ongoing tasks
        rpc_task_handle.abort();
        rpc_tx_queue_handle.abort();
        health_task_handle.abort();
        syncer_task_handle.abort();
        block_gossip_task_handle.abort();
        block_notify_task_handle.abort();
        mempool_crawler_task_handle.abort();
        mempool_queue_checker_task_handle.abort();
        tx_gossip_task_handle.abort();
        tfl_service_task_handle.abort();
        progress_task_handle.abort();
        end_of_support_task_handle.abort();
        miner_task_handle.abort();
        if zcashd_compat_task_finished {
            debug!("zcashd-compat supervisor task already exited before shutdown");
        } else if let Some(zcashd_compat_shutdown_timeout) = zcashd_compat_shutdown_timeout {
            info!(
                ?zcashd_compat_shutdown_timeout,
                "requesting zcashd-compat supervisor shutdown"
            );
            if zcashd_compat_shutdown_tx.send(true).is_err() {
                warn!("zcashd-compat supervisor shutdown request was not delivered");
            }
            if tokio::time::timeout(
                zcashd_compat_shutdown_timeout,
                &mut zcashd_compat_task_handle,
            )
            .await
            .is_err()
            {
                warn!(
                    ?zcashd_compat_shutdown_timeout,
                    "zcashd-compat supervisor did not finish before shutdown timeout; \
                     abandoning child process handle"
                );
                // The supervisor spawns zcashd without kill_on_drop, so this
                // abort abandons an already-signalled child rather than
                // SIGKILLing it mid-flush.
                zcashd_compat_task_handle.abort();
            }
        } else {
            debug!("aborting zcashd-compat supervisor task without managed child shutdown");
            zcashd_compat_task_handle.abort();
        }

        // startup tasks
        state_checkpoint_verify_handle.abort();
        old_databases_task_handle.abort();

        info!(
            "exiting Zebra: all tasks have been asked to stop, waiting for remaining tasks to finish"
        );

        exit_status
    }

    /// Returns the bound for the state service buffer,
    /// based on the configurations of the services that use the state concurrently.
    fn state_buffer_bound() -> usize {
        let config = APPLICATION.config();

        // Ignore the checkpoint verify limit, because it is very large.
        //
        // TODO: do we also need to account for concurrent use across services?
        //       we could multiply the maximum by 3/2, or add a fixed constant
        [
            config.sync.download_concurrency_limit,
            config.sync.full_verify_concurrency_limit,
            inbound::downloads::MAX_INBOUND_CONCURRENCY,
            mempool::downloads::MAX_INBOUND_CONCURRENCY,
        ]
        .into_iter()
        .max()
        .unwrap()
    }
}

impl Runnable for StartCmd {
    /// Start the application.
    fn run(&self) {
        info!("Starting zebrad");
        let rt = APPLICATION
            .state()
            .components_mut()
            .get_downcast_mut::<TokioComponent>()
            .expect("TokioComponent should be available")
            .rt
            .take();

        rt.expect("runtime should not already be taken")
            .run(self.start());

        info!("stopping zebrad");
    }
}

impl config::Override<ZebradConfig> for StartCmd {
    // Process the given command line options, overriding settings from
    // a configuration file using explicit flags taken from command-line
    // arguments.
    fn override_config(&self, mut config: ZebradConfig) -> Result<ZebradConfig, FrameworkError> {
        if !self.filters.is_empty() {
            config.tracing.filter = Some(self.filters.join(","));
        }

        // `--zcashd-compat` is a one-way override that enables zcashd-compat mode.
        // The actual zcashd-compat guardrails are applied below using
        // `config.zcashd_compat.enabled` so CLI and config-file activation share one path.
        if self.zcashd_compat {
            config.zcashd_compat.enabled = true;
        }

        if !config.zcashd_compat.enabled && !config.zcashd_compat.block_gossip_peer_ips.is_empty() {
            return Err(std::io::Error::other(
                "zcashd_compat.block_gossip_peer_ips requires zcashd_compat.enabled = true",
            )
            .into());
        }

        if config.zcashd_compat.enabled && config.zcashd_compat.manage_zcashd {
            zcashd_compat::reject_peer_selection_extra_args(
                &config.zcashd_compat.zcashd_extra_args,
            )
            .map_err(|err| std::io::Error::other(err.to_string()))?;

            match zcashd_compat::effective_zcashd_source(&config.zcashd_compat) {
                Ok(zcashd_compat::ZcashdBinarySource::Path(path))
                    if !zcashd_compat::is_command_resolvable(Path::new(&path)) =>
                {
                    return Err(std::io::Error::other(format!(
                        "zcashd-compat mode could not resolve zcashd_path={}",
                        path.display()
                    ))
                    .into());
                }
                Ok(_) => {}
                Err(err) => return Err(std::io::Error::other(err.to_string()).into()),
            }
        }

        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use abscissa_core::config::Override;
    use color_eyre::eyre::eyre;

    use super::StartCmd;
    use crate::components::zcashd_compat;
    use crate::config::ZebradConfig;

    #[test]
    fn zcashd_compat_flag_enables_mode() {
        let cmd = StartCmd {
            filters: Vec::new(),
            zcashd_compat: true,
            unsafe_low_specs: false,
        };
        let mut config = ZebradConfig::default();
        config.zcashd_compat.manage_zcashd = false;

        let config = cmd
            .override_config(config)
            .expect("zcashd-compat override config should succeed");

        assert!(config.zcashd_compat.enabled);
    }

    #[test]
    fn zcashd_compat_config_enables_mode() {
        let cmd = StartCmd {
            filters: Vec::new(),
            zcashd_compat: false,
            unsafe_low_specs: false,
        };
        let mut config = ZebradConfig::default();
        config.zcashd_compat.enabled = true;
        config.zcashd_compat.manage_zcashd = false;

        let config = cmd
            .override_config(config)
            .expect("zcashd-compat override config should succeed");

        assert!(config.zcashd_compat.enabled);
    }

    #[test]
    fn block_gossip_peer_ips_require_zcashd_compat() {
        let cmd = StartCmd {
            filters: Vec::new(),
            zcashd_compat: false,
            unsafe_low_specs: false,
        };
        let mut config = ZebradConfig::default();
        config.zcashd_compat.block_gossip_peer_ips =
            vec![std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)];

        let error = cmd
            .override_config(config)
            .expect_err("block gossip peers should require zcashd-compat");

        assert!(
            error
                .to_string()
                .contains("zcashd_compat.block_gossip_peer_ips requires"),
            "error should explain the zcashd-compat requirement: {error}"
        );
    }

    #[test]
    fn zcashd_compat_config_rejects_peer_selection_extra_args() {
        let cmd = StartCmd {
            filters: Vec::new(),
            zcashd_compat: false,
            unsafe_low_specs: false,
        };
        let mut config = ZebradConfig::default();
        config.zcashd_compat.enabled = true;
        config.zcashd_compat.manage_zcashd = true;
        config.zcashd_compat.zcashd_source = zcashd_compat::ConfigZcashdBinarySource::Embedded;
        config.zcashd_compat.zcashd_extra_args = vec!["-addnode=1.2.3.4".to_string()];

        let error = cmd
            .override_config(config)
            .expect_err("peer-selection extra args should be rejected");
        assert!(
            error.to_string().contains("peer-selection"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn zcashd_compat_manage_zcashd_requires_resolvable_path() {
        let cmd = StartCmd {
            filters: Vec::new(),
            zcashd_compat: true,
            unsafe_low_specs: false,
        };
        let mut config = ZebradConfig::default();
        config.zcashd_compat.manage_zcashd = true;
        config.zcashd_compat.zcashd_path = Some("/definitely/missing/zcashd-compat".into());

        let error = cmd
            .override_config(config)
            .expect_err("zcashd-compat override should fail for an unresolvable zcashd path");

        assert!(
            error
                .to_string()
                .contains("zcashd-compat mode could not resolve zcashd_path"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn zcashd_compat_path_source_requires_explicit_path() {
        let cmd = StartCmd {
            filters: Vec::new(),
            zcashd_compat: true,
            unsafe_low_specs: false,
        };
        let mut config = ZebradConfig::default();
        config.zcashd_compat.manage_zcashd = true;
        config.zcashd_compat.zcashd_source = zcashd_compat::ConfigZcashdBinarySource::Path;
        config.zcashd_compat.zcashd_path = None;

        let error = cmd
            .override_config(config)
            .expect_err("path source should require explicit zcashd_path");
        assert!(
            error.to_string().contains("zcashd_source=path"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn zcashd_compat_embedded_source_allows_missing_local_path() {
        let cmd = StartCmd {
            filters: Vec::new(),
            zcashd_compat: true,
            unsafe_low_specs: false,
        };
        let mut config = ZebradConfig::default();
        config.zcashd_compat.manage_zcashd = true;
        config.zcashd_compat.zcashd_source = zcashd_compat::ConfigZcashdBinarySource::Embedded;
        config.zcashd_compat.zcashd_path = None;

        cmd.override_config(config)
            .expect("embedded source should be validated at runtime, not override-time");
    }

    #[test]
    fn zcashd_compat_config_manage_zcashd_requires_resolvable_path() {
        let cmd = StartCmd {
            filters: Vec::new(),
            zcashd_compat: false,
            unsafe_low_specs: false,
        };
        let mut config = ZebradConfig::default();
        config.zcashd_compat.enabled = true;
        config.zcashd_compat.manage_zcashd = true;
        config.zcashd_compat.zcashd_path = Some("/definitely/missing/zcashd-compat".into());

        let error = cmd
            .override_config(config)
            .expect_err("zcashd-compat config should fail for an unresolvable zcashd path");

        assert!(
            error
                .to_string()
                .contains("zcashd-compat mode could not resolve zcashd_path"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn zcashd_compat_supervisor_shutdown_timeout_matches_config() {
        let mut config = ZebradConfig::default();

        config.zcashd_compat.enabled = true;
        config.zcashd_compat.manage_zcashd = true;
        config.zcashd_compat.shutdown_grace_period = std::time::Duration::from_secs(42);
        assert_eq!(
            StartCmd::zcashd_compat_supervisor_shutdown_timeout(&config),
            Some(
                std::time::Duration::from_secs(42)
                    + StartCmd::ZCASHD_COMPAT_SHUTDOWN_TIMEOUT_MARGIN
            ),
            "outer supervisor wait must exceed the child grace period so task \
             abort cannot preempt graceful termination",
        );

        config.zcashd_compat.manage_zcashd = false;
        assert_eq!(
            StartCmd::zcashd_compat_supervisor_shutdown_timeout(&config),
            None
        );

        config.zcashd_compat.enabled = false;
        config.zcashd_compat.manage_zcashd = true;
        assert_eq!(
            StartCmd::zcashd_compat_supervisor_shutdown_timeout(&config),
            None
        );
    }

    #[test]
    fn zcashd_compat_supervisor_ok_exit_does_not_exit_zebra() {
        assert!(!StartCmd::zcashd_compat_supervisor_should_exit(Ok(Ok(()))));
    }

    #[test]
    fn zcashd_compat_supervisor_error_does_not_exit_zebra() {
        assert!(!StartCmd::zcashd_compat_supervisor_should_exit(Ok(Err(
            eyre!("simulated zcashd supervisor runtime failure"),
        ))));
    }

    #[tokio::test]
    async fn zcashd_compat_supervisor_panic_does_not_exit_zebra() {
        let join_err = tokio::spawn(async {
            panic!("simulated zcashd supervisor panic");
        })
        .await
        .expect_err("task should panic");

        assert!(!StartCmd::zcashd_compat_supervisor_should_exit(Err(
            join_err
        )));
    }
}
