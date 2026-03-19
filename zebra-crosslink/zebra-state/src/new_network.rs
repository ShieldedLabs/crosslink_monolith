use std::time::Instant;
use crate::{Request, Response, ReadRequest, ReadResponse};
use tower::ServiceExt;
use zebra_chain::serialization::ZcashSerialize;

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
    // if let Some() = config.network_identity_seed_string This is broken, phillip fix.
/*
    The Plan.
1. Call new_keypair_from_connect_magic1_with_seed to generate a key or new_keypair_from_connect_magic1 if none.
2. Run the chat program ish from here. With peer discovery.
3. Every tick, iterate all chains, gather all [this_hash, parent_hash].
4. Gossip/Share status to others.
5. Implement reliable streams and stream blocks. One is unprompted the other is a request to be streamed to.
*/

    let mut nfs_rx = rt.block_on(async {
        let res = read_state.clone().oneshot(ReadRequest::NonFinalizedBlocksListener).await;
        match res {
            Ok(ReadResponse::NonFinalizedBlocksListener(rx)) => rx.unwrap(),
            Err(err) => panic!("sync start err: {err:?}"),
            _ => panic!("sync err: unhandled response: {res:?}"),
        }
    });
    loop {
        std::thread::sleep(std::time::Duration::from_millis(500));

        match nfs_rx.try_recv() {
            Ok(nfs) => {
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

        // println!("syncing! tip: {tip:?}");
        println!("syncing! tip");
    }
}
