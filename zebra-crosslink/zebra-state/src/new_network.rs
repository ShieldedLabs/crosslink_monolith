use std::time::Instant;
use crate::{Request, Response, ReadRequest, ReadResponse};
use tower::ServiceExt;
use zebra_chain::serialization::ZcashSerialize;

use tenderlink::bandwidth_test::*;

#[derive(Clone, Copy, Debug)]
pub(crate) enum BlockEvent {
    Dequeued([u8; 32]),
    Committed([u8; 32]),
    TradFinalized([u8; 32]),
    CrosslinkFinalized([u8; 32]),
}
static BLOCK_EVENT_QUEUE_SENDER: std::sync::OnceLock<tokio::sync::mpsc::Sender<BlockEvent>> = std::sync::OnceLock::new();

pub async fn push_block_event(event: BlockEvent) {
    if let Some(tx) = BLOCK_EVENT_QUEUE_SENDER.get() {
        tx.send(event).await.unwrap();
    }
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

    let mut nfs_rx = rt.block_on(async {
        let res = read_state.clone().oneshot(ReadRequest::NonFinalizedBlocksListener).await;
        match res {
            Ok(ReadResponse::NonFinalizedBlocksListener(rx)) => rx.unwrap(),
            Err(err) => panic!("sync start err: {err:?}"),
            _ => panic!("sync err: unhandled response: {res:?}"),
        }
    });
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

        match nfs_rx.try_recv() {
            Ok(nfs) => {
                should_sleep = false;
                println!("nfs: {:?}", nfs.0);
            }
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {},
            Err(err) => tracing::error!("{err:?}"),
        }

        // let res = rt.block_on(read_state.clone().oneshot(ReadRequest::Block(zebra_chain::block::Height(0).into())));
        // match res {
        //     Ok(ReadResponse::Block(Some(block))) => {
        //         let mut file = std::fs::File::create("testnet-genesis.pow").unwrap();
        //         block.as_ref().zcash_serialize(&file);
        //         println!("genesis written to file");
        //         break;
        //     },
        //     Ok(ReadResponse::Block(None)) => println!("genesis not ready yet"),
        //     Err(err) => panic!("sync start err: {err:?}"),
        //     _ => panic!("sync err: unhandled response: {res:?}"),
        // }
        
        if should_sleep {
            std::thread::yield_now();
        }
    }
}
