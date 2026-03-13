use std::time::Instant;
use crate::{Request, Response, ReadRequest, ReadResponse};
use tower::ServiceExt;
use zebra_chain::serialization::ZcashSerialize;

pub fn sync(read_state: impl tower::Service<
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
        rt: tokio::runtime::Handle)
{
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
