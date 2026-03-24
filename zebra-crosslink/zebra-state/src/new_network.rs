use std::time::Instant;
use crate::{Request, Response, ReadRequest, ReadResponse};
use tower::ServiceExt;
use zebra_chain::serialization::ZcashSerialize;
use zebra_chain::block::Hash;

use tenderlink::bandwidth_test::*;

#[derive(Clone, Copy, Debug)]
pub(crate) enum BlockEvent {
    Dequeued(Hash),
    Committed(Hash),
    TradFinalized(Hash),
    CrosslinkFinalized(Hash),
}
static BLOCK_EVENT_QUEUE_SENDER: std::sync::OnceLock<tokio::sync::mpsc::Sender<BlockEvent>> = std::sync::OnceLock::new();

pub async fn push_block_event(event: BlockEvent) {
    if let Some(tx) = BLOCK_EVENT_QUEUE_SENDER.get() {
        tx.send(event).await.unwrap();
    }
}

#[derive(Clone, Copy, Debug)]
struct ShadowBlock {
    this_hash: Hash,
    parent_hash: Hash,
}

pub fn sync(
        config: &crate::config::Config,
        read_state: impl tower::Service<
            ReadRequest,
            Response = ReadResponse,
            Error = crate::BoxError,
        > + Clone
        + Send
        + Sync
        + 'static,
        state: impl tower::Service<
            Request,
            Response = Response,
            Error = crate::BoxError,
        > + Clone
        + Send
        + Sync
        + 'static,
        rt: tokio::runtime::Handle,
) {
    let (event_tx, mut event_rx) = tokio::sync::mpsc::channel(500);
    BLOCK_EVENT_QUEUE_SENDER.set(event_tx).unwrap();

    let mut finalized_tip_maybe = None;
    loop {
        finalized_tip_maybe = rt.block_on(async {
            let res = read_state.clone().oneshot(ReadRequest::FinalizedTip).await;
            match res {
                Ok(ReadResponse::Tip(finalized_tip_maybe)) => finalized_tip_maybe,
                Err(err) => panic!("sync start err: {err:?}"),
                _ => panic!("sync err: unhandled response: {res:?}"),
            }
        });
        if finalized_tip_maybe.is_none() {
            std::thread::yield_now();
            continue;
        }
        break;
    }
    let mut finalized_tip = finalized_tip_maybe.unwrap().1;
    println!("NEW NETWORK: Starting with hash: {:?}", finalized_tip);
    
    let network_keypair;
    if let Some(string_seed) = &config.network_identity_seed_string {
        let hash = *blake3::hash(string_seed.as_bytes()).as_bytes();
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&hash);
        network_keypair = new_keypair_from_connect_magic1_with_seed(CONNECT_MAGIC1_PLAIN_TEXT, seed).unwrap();
    }
    else {
        network_keypair = new_keypair_from_connect_magic1(CONNECT_MAGIC1_PLAIN_TEXT).unwrap();
    }
    println!("NETWORK KEYPAIR: {}", network_keypair);
    let my_local_stp_address = STPAddress {
        ip: "::1".parse().unwrap(),
        port: config.network_local_port,
        magic1: CONNECT_MAGIC1_PLAIN_TEXT,
        key: network_keypair.public,
    };
    println!("NETWORK LOCALHOST STP ADDRESS: {:?}", my_local_stp_address);
    println!("I need to connect to: {:?}", config.network_initial_peers);

    loop {
        let mut should_sleep = true;

        match event_rx.try_recv() {
            Ok(block_event) => {
                should_sleep = false;
                println!("block_event: {:?}", block_event);
            }
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {},
            Err(err) => tracing::error!("{err:?}"),
        }
        
        if should_sleep {
            std::thread::yield_now();
        }
    }
}
