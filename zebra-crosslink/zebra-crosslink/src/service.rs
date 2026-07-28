//! Tower Service implementation for TFLService.
//!
//! This module integrates `TFLServiceHandle` with the `tower::Service` trait,
//! allowing it to handle asynchronous service requests.

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::task::{Context, Poll};

use futures::task::AtomicWaker;
use tokio::sync::{broadcast, Mutex};
use tokio::task::JoinHandle;

use tracing::{error, info, warn};

use zebra_chain::block::{Hash as BlockHash, Height as BlockHeight};
use zebra_chain::transaction::Hash as TxHash;
use zebra_node_services::mempool::{Request as MempoolRequest, Response as MempoolResponse};
use zebra_state::{crosslink::*, Request as StateRequest, Response as StateResponse, ReadRequest as StateReadRequest, ReadResponse as StateReadResponse};

use zcash_primitives::transaction::RosterMember;
use zcash_primitives::bft::*;
use crate::{
    bootstrap_roster_from_config, decode_consensus_public_key_hex,
    tfl_service_incoming_request, TFLBlockFinality, TFLServiceInternal,
    SERVICE_HEALTH_FAILED, SERVICE_HEALTH_READY,
    SERVICE_HEALTH_STARTING,
};

use tower::Service;
impl Service<TFLServiceRequest> for TFLServiceHandle {
    type Response = TFLServiceResponse;
    type Error = TFLServiceError;
    type Future = Pin<Box<dyn Future<Output = Result<TFLServiceResponse, TFLServiceError>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        loop {
            match self.service_health.load(Ordering::Acquire) {
                SERVICE_HEALTH_READY => return Poll::Ready(Ok(())),
                SERVICE_HEALTH_STARTING => {
                    self.service_health_waker.register(_cx.waker());
                    if self.service_health.load(Ordering::Acquire) == SERVICE_HEALTH_STARTING {
                        return Poll::Pending;
                    }
                }
                SERVICE_HEALTH_FAILED => {
                    return Poll::Ready(Err(TFLServiceError::Misc(
                        "crosslink consensus service has terminated".to_owned(),
                    )))
                }
                _ => {
                    return Poll::Ready(Err(TFLServiceError::Misc(
                        "crosslink is observer-only and is not validator-ready".to_owned(),
                    )))
                }
            }
        }
    }

    fn call(&mut self, request: TFLServiceRequest) -> Self::Future {
        let duplicate_handle = self.clone();
        Box::pin(async move { tfl_service_incoming_request(duplicate_handle, request).await })
    }
}

/// A pinned-in-memory, heap-allocated, reference-counted, thread-safe, asynchronous function
/// pointer that takes a `StateRequest` as input and returns a `StateResponse` as output.
///
/// The error is boxed to allow for dynamic error types.
pub(crate) type StateServiceProcedure = Arc<
    dyn Fn(
            StateRequest,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<StateResponse, Box<dyn std::error::Error + Send + Sync>>>
                    + Send,
            >,
        > + Send
        + Sync,
>;

pub(crate) type ReadStateServiceProcedure = Arc<
    dyn Fn(
            StateReadRequest,
        ) -> Pin<
            Box<
                dyn Future<Output = Result<StateReadResponse, Box<dyn std::error::Error + Send + Sync>>>
                    + Send,
            >,
        > + Send
        + Sync,
>;

pub(crate) type MempoolServiceProcedure = Arc<
    dyn Fn(
            MempoolRequest,
        ) -> Pin<
            Box<
                dyn Future<
                        Output = Result<MempoolResponse, Box<dyn std::error::Error + Send + Sync>>,
                    > + Send,
            >,
        > + Send
        + Sync,
>;

/// A pinned-in-memory, heap-allocated, reference-counted, thread-safe, asynchronous function
/// pointer that takes an `Arc<Block>` as input and returns `()` as its output.

/// A pinned-in-memory, heap-allocated, reference-counted, thread-safe, asynchronous function
/// pointer that takes an `Arc<Block>` as input and returns `()` as its output.
pub(crate) type ForceFeedPoSBlockProcedure = Arc<
    dyn Fn(Arc<BftBlock>, FatPointerToBftBlock) -> Pin<Box<dyn Future<Output = Result<(),String>> + Send>>
        + Send
        + Sync,
>;

/// `TFLServiceCalls` encapsulates the service calls that this service needs to make to other services.
/// Simply put, it is a function pointer bundle for all outgoing calls to the rest of Zebra.
#[derive(Clone)]
pub struct TFLServiceCalls {
    pub(crate) state: StateServiceProcedure,
    pub(crate) read_state: ReadStateServiceProcedure,
    pub(crate) mempool: MempoolServiceProcedure,
    pub(crate) force_feed_pos: ForceFeedPoSBlockProcedure,
}
impl fmt::Debug for TFLServiceCalls {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "TFLServiceCalls")
    }
}

/// Spawn a Trailing Finality Service that uses the provided
/// closures to call out to other services.
///
/// - `state_service_call` takes a [`StateRequest`] as input and returns a [`StateResponse`] as output.
///
/// [`TFLServiceHandle`] is a shallow handle that can be cloned and passed between threads.
pub fn spawn_new_tfl_service(
    is_regtest: bool,
    global_seed: [u8; 32],
    path_to_pos_store_file: PathBuf,
    state_service_call: StateServiceProcedure,
    read_state_service_call: ReadStateServiceProcedure,
    mempool_service_call: MempoolServiceProcedure,
    config: crate::config::Config,
    closure_from_state_to_here_mutex: Arc<std::sync::Mutex<Option<zebra_state::ClosureToCallIntoCrosslinkFromState>>>,
) -> (TFLServiceHandle, JoinHandle<Result<(), String>>) {
    let (finalizers_at_current_height, finalizers_keys_to_names) = {
        let array = bootstrap_roster_from_config(&config).unwrap_or_default();
        let mut map =
            std::collections::HashMap::with_capacity(config.bft_peer_identities.len());

        for peer in &config.bft_peer_identities {
            if let Ok(public_key) = decode_consensus_public_key_hex(&peer.consensus_public_key) {
                // Transport configuration never grants roster membership.
                map.insert(public_key, peer.address.clone());
            }
        }

        (array, map)
    };

    let internal = Arc::new(Mutex::new(TFLServiceInternal {
        my_public_key: PubKeyID::NIL,
        latest_final_block: None,
        tfl_is_activated: if is_regtest { true } else { false },
        final_change_tx: broadcast::channel(16).0,
        bft_msg_flags: 0,
        bft_err_flags: 0,
        bft_blocks: Vec::new(),
        bft_height_by_hash: std::collections::HashMap::new(),
        fat_pointer_to_tip: FatPointerToBftBlock::null(),
        peer_strings: Vec::new(),
        our_set_bft_string: None,
        active_bft_string: None,
        finalizers_at_current_height,
        finalizers_keys_to_names,
        current_bc_final: None,
        path_to_pos_store_file: path_to_pos_store_file.clone(),
        pos_store_file: None,
        pos_store_read_file: None,
        pos_store_records: Vec::new(),
        pending_reflush: None,
        pos_store_unverified_tail: None,
        recency_status: TFLRecencyStatus::default(),
    }));

    let service_health = Arc::new(AtomicU8::new(SERVICE_HEALTH_STARTING));
    let service_health_waker = Arc::new(AtomicWaker::new());

    let handle_mtx = Arc::new(std::sync::Mutex::new(None));

    let handle_mtx2 = handle_mtx.clone();
    let force_feed_pos: ForceFeedPoSBlockProcedure = Arc::new(move |block: Arc<BftBlock>, fat_pointer: FatPointerToBftBlock| {
        let handle = handle_mtx2.lock().unwrap().clone().unwrap();
        Box::pin(async move {
            let fat_pointer_hash = fat_pointer.points_at_block_hash();
            let block_hash       = block.blake3_hash();
            if fat_pointer_hash != block_hash {
                return Err(format!("block ({block_hash}) is not the one signed for ({fat_pointer_hash})"));
            };

            let (status, reason) = crate::validate_bft_block(&handle, block.as_ref()).await;
            match status {
                tenderlink::TMStatus::Pass => {
                    crate::apply_verified_decided_bft_block(
                        &handle,
                        block.as_ref(),
                        &fat_pointer,
                        -1,
                        Vec::new(),
                    )
                    .await?;
                    info!("Successfully force-fed and durably applied certified BFT block");
                    Ok(())
                },

                tenderlink::TMStatus::Indeterminate |
                tenderlink::TMStatus::Fail => {
                    error!("Failed to force-feed BFT block");
                    Err(format!("PoS validation = {status:?}: {reason:?}"))
                },
            }
        })
    });

    let handle1 = TFLServiceHandle {
        internal,
        decision_apply_gate: Arc::new(Mutex::new(())),
        call: TFLServiceCalls {
            state: state_service_call,
            read_state: read_state_service_call,
            mempool: mempool_service_call,
            force_feed_pos,
        },
        config,
        service_health: service_health.clone(),
        service_health_waker: service_health_waker.clone(),
    };

    *handle_mtx.lock().unwrap() = Some(handle1.clone());

    let handle3 = handle1.clone();
    *closure_from_state_to_here_mutex.lock().unwrap() = Some(Arc::new(move |fpa, fpb, height| crate::call_from_state_to_crosslink_to_ask_about_fat_pointers(&handle3, fpa, fpb, height)));

    let handle2 = handle1.clone();
    let service_health_for_task = service_health;
    let service_health_waker_for_task = service_health_waker;
    let task = tokio::spawn(async move {
        let result = crate::tfl_service_main_loop(
            handle2,
            global_seed,
            path_to_pos_store_file,
            is_regtest,
        )
        .await;
        if result.is_err() {
            service_health_for_task.store(SERVICE_HEALTH_FAILED, Ordering::Release);
            service_health_waker_for_task.wake();
        }
        result
    });
    (handle1, task)
}

/// A wrapper around the `TFLServiceInternal` and `TFLServiceCalls` types, used to manage
/// the internal state of the TFLService and the service calls that can be made to it.
#[derive(Clone, Debug)]
pub struct TFLServiceHandle {
    /// A threadsafe wrapper around the stored internal data
    pub(crate) internal: Arc<Mutex<TFLServiceInternal>>,
    /// Serializes decided-value application without holding `internal` across
    /// the state callback, which must re-enter the crosslink service.
    pub(crate) decision_apply_gate: Arc<Mutex<()>>,
    /// The collection of service calls available
    pub(crate) call: TFLServiceCalls,
    /// The file-generated config data
    pub config: crate::config::Config,
    /// Readiness is separate from task existence: legacy/observer configurations remain unready.
    pub(crate) service_health: Arc<AtomicU8>,
    pub(crate) service_health_waker: Arc<AtomicWaker>,
}

impl TFLServiceHandle {
    pub(crate) fn set_service_health(&self, status: u8) {
        if self.service_health.swap(status, Ordering::AcqRel) != status {
            self.service_health_waker.wake();
        }
    }
}
