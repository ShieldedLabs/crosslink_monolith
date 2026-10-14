//! Tower Service implementation for TFLService.
//!
//! This module integrates `TFLServiceHandle` with the `tower::Service` trait,
//! allowing it to handle asynchronous service requests.

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::sync::{broadcast, Mutex};
use tokio::task::JoinHandle;

use tracing::{error, info, warn};

use zebra_chain::block::{Hash as BlockHash, Height as BlockHeight};
use zebra_chain::transaction::Hash as TxHash;
use zebra_node_services::mempool::{Request as MempoolRequest, Response as MempoolResponse};
use zebra_state::{crosslink::*, Request as StateRequest, Response as StateResponse, ReadRequest as StateReadRequest, ReadResponse as StateReadResponse};

use zcash_primitives::transaction::RosterMember;
use zcash_primitives::bft::*;
use crate::{tfl_service_incoming_request, TFLBlockFinality, TFLServiceInternal};

use tower::Service;
impl Service<TFLServiceRequest> for TFLServiceHandle {
    type Response = TFLServiceResponse;
    type Error = TFLServiceError;
    type Future = Pin<Box<dyn Future<Output = Result<TFLServiceResponse, TFLServiceError>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
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

/// Delivers raw wire bytes to the node's inbound peer-request service as if the peer at the
/// address had just sent them: they are decoded with the peer codec and routed as a connection
/// routes an unsolicited message.
pub(crate) type InboundWireProcedure = Arc<
    dyn Fn(
            std::net::SocketAddr,
            Vec<u8>,
        ) -> Pin<Box<dyn Future<Output = Result<(), Box<dyn std::error::Error + Send + Sync>>> + Send>>
        + Send
        + Sync,
>;

/// Builds a block from the node's getblocktemplate response, as the node's own miner does,
/// without submitting it.
pub(crate) type BlockFromTemplateProcedure = Arc<
    dyn Fn() -> Pin<Box<dyn Future<Output = Result<zebra_chain::block::Block, String>> + Send>>
        + Send
        + Sync,
>;

/// Calls the RPC method named by the first argument with the JSON params in the second, through
/// the node's own JSON-RPC method table, and answers its JSON result, or its JSON error.
pub(crate) type RpcCallProcedure = Arc<
    dyn Fn(String, String) -> Pin<Box<dyn Future<Output = Result<String, String>> + Send>> + Send + Sync,
>;

/// `TFLServiceCalls` encapsulates the service calls that this service needs to make to other services.
/// Simply put, it is a function pointer bundle for all outgoing calls to the rest of Zebra.
#[derive(Clone)]
pub struct TFLServiceCalls {
    pub(crate) state: StateServiceProcedure,
    pub(crate) read_state: ReadStateServiceProcedure,
    pub(crate) mempool: MempoolServiceProcedure,
    pub(crate) inbound_wire: InboundWireProcedure,
    /// Set once the RPC implementation exists: it is built after this service, and takes this
    /// service as an input, so it cannot be passed in at spawn time.
    pub(crate) block_from_template: Arc<std::sync::OnceLock<BlockFromTemplateProcedure>>,
    /// Set at the same point as `block_from_template`, for the same reason.
    pub(crate) rpc_call: Arc<std::sync::OnceLock<RpcCallProcedure>>,
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
    state_service_call: StateServiceProcedure,
    read_state_service_call: ReadStateServiceProcedure,
    mempool_service_call: MempoolServiceProcedure,
    inbound_wire_call: InboundWireProcedure,
    block_from_template: Arc<std::sync::OnceLock<BlockFromTemplateProcedure>>,
    rpc_call: Arc<std::sync::OnceLock<RpcCallProcedure>>,
    config: crate::config::Config,
    params: ZcashCrosslinkParameters,
    network: zebra_chain::parameters::Network,
) -> (TFLServiceHandle, JoinHandle<Result<(), String>>) {
    let internal = Arc::new(Mutex::new(TFLServiceInternal {
        bft_msg_flags: 0,
        bft_err_flags: 0,
        our_set_bft_string: None,
        active_bft_string: None,
    }));

    let handle1 = TFLServiceHandle {
        internal,
        call: TFLServiceCalls {
            state: state_service_call,
            read_state: read_state_service_call,
            mempool: mempool_service_call,
            inbound_wire: inbound_wire_call,
            block_from_template,
            rpc_call,
        },
        config,
        params,
        network,
    };

    let handle2 = handle1.clone();
    (
        handle1,
        tokio::spawn(async move { crate::tfl_service_main_loop(handle2).await }),
    )
}

/// A wrapper around the `TFLServiceInternal` and `TFLServiceCalls` types, used to manage
/// the internal state of the TFLService and the service calls that can be made to it.
#[derive(Clone, Debug)]
pub struct TFLServiceHandle {
    /// A threadsafe wrapper around the stored internal data
    pub(crate) internal: Arc<Mutex<TFLServiceInternal>>,
    /// The collection of service calls available
    pub(crate) call: TFLServiceCalls,
    /// The file-generated config data
    pub config: crate::config::Config,
    /// The network's Crosslink consensus parameters. Immutable, and kept outside `internal` so the
    /// fat-pointer gate can decide on them before taking that lock.
    pub(crate) params: ZcashCrosslinkParameters,
    /// The network, for the staking share of the block subsidy (`pos_subsidy`) in replays.
    pub(crate) network: zebra_chain::parameters::Network,
}
