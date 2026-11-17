//! Helpers shared by this crate's unit tests.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
};

use futures::{future::BoxFuture, FutureExt};
use tokio::sync::oneshot;
use tower::{buffer::Buffer, util::BoxService, ServiceExt};

use zebra_chain::{
    block::{self, Block, MAX_BLOCK_BYTES},
    parameters::Network,
    serialization::ZcashDeserializeInto,
};
use zebra_state as zs;

use crate::BoxError;

/// Returns the block vectors for `network` that fit in a block, with their heights.
///
/// Crosslink lowers `MAX_BLOCK_BYTES` to 100 kB until the block size returns to normal (#168),
/// so the parser refuses the ~2 MB Testnet vector 141042. Every other vector is under 100 kB.
/// [`oversized_block_vectors_are_refused`] checks that the vectors left out don't parse.
pub(crate) fn block_vectors(
    network: &Network,
) -> impl DoubleEndedIterator<Item = (&'static u32, &'static &'static [u8])> {
    network
        .block_iter()
        .filter(|(_height, block)| fits_in_a_block(block))
}

/// Returns the Mainnet and Testnet block vectors that fit in a block. See [`block_vectors`].
pub(crate) fn all_block_vectors() -> impl Iterator<Item = &'static [u8]> {
    zebra_test::vectors::BLOCKS
        .iter()
        .copied()
        .filter(|block| fits_in_a_block(block))
}

fn fits_in_a_block(block: &[u8]) -> bool {
    block.len() as u64 <= MAX_BLOCK_BYTES
}

/// The block vectors that [`block_vectors`] and [`all_block_vectors`] leave out are the ones
/// the parser refuses.
#[test]
fn oversized_block_vectors_are_refused() {
    let _init_guard = zebra_test::init();

    for network in Network::iter() {
        for (height, block) in network.block_iter() {
            if !fits_in_a_block(block) {
                assert!(
                    block.zcash_deserialize_into::<Block>().is_err(),
                    "{network} block {height} is larger than MAX_BLOCK_BYTES, so it must not parse"
                );
            }
        }
    }
}

/// A state service for checkpoint verifier tests, which commits checkpoint-verified blocks
/// through the block writer.
///
/// Crosslink's state service refuses `CommitCheckpointVerifiedBlock`: `new_network` owns the
/// block writer and commits each block by calling it directly. The checkpoint verifier still
/// sends its commits to the state service. This service answers them the way the state service
/// did before that change: it queues each block until the block below it is committed, commits
/// it through the block writer, and refuses a block below the next height. Every other request
/// goes to the state service.
pub(crate) async fn state_with_checkpoint_commits(
    network: &Network,
) -> Buffer<BoxService<zs::Request, zs::Response, BoxError>, zs::Request> {
    let (state_service, _read_state_service, _latest_chain_tip, _chain_tip_change, block_writer) =
        zs::init(zs::Config::ephemeral(), network, block::Height::MAX, 0).await;
    let state_service = Buffer::new(state_service, 1);

    let commits = Arc::new(Mutex::new(CheckpointCommits {
        block_writer,
        next_height: block::Height(0),
        queued: BTreeMap::new(),
    }));

    let service = tower::service_fn(
        move |request: zs::Request| -> BoxFuture<'static, Result<zs::Response, BoxError>> {
            match request {
                zs::Request::CommitCheckpointVerifiedBlock(block) => {
                    let committed = commits
                        .lock()
                        .expect("a commit panicked while holding the lock")
                        .queue(block);

                    async move {
                        match committed.await {
                            Ok(Ok(hash)) => Ok(zs::Response::Committed(hash)),
                            Ok(Err(error)) => Err(error),
                            Err(_) => Err(BoxError::from("queued block was dropped")),
                        }
                    }
                    .boxed()
                }
                request => state_service.clone().oneshot(request).boxed(),
            }
        },
    );

    Buffer::new(BoxService::new(service), 1)
}

/// The commit queue behind [`state_with_checkpoint_commits`].
struct CheckpointCommits {
    block_writer: zs::WriteBlockWorkerTask,
    next_height: block::Height,
    queued: BTreeMap<
        block::Height,
        (
            zs::CheckpointVerifiedBlock,
            oneshot::Sender<Result<block::Hash, BoxError>>,
        ),
    >,
}

impl CheckpointCommits {
    /// Queues `block`, then commits every queued block from the next height up, in order.
    fn queue(
        &mut self,
        block: zs::CheckpointVerifiedBlock,
    ) -> oneshot::Receiver<Result<block::Hash, BoxError>> {
        let (rsp_tx, rsp_rx) = oneshot::channel();

        if block.height < self.next_height {
            let _ = rsp_tx.send(Err(format!(
                "block {:?} is below the next height {:?}, so it is already committed",
                block.height, self.next_height
            )
            .into()));
            return rsp_rx;
        }

        if let Some((_replaced, replaced_rsp_tx)) =
            self.queued.insert(block.height, (block, rsp_tx))
        {
            let _ =
                replaced_rsp_tx.send(Err("replaced by a newer commit at the same height".into()));
        }

        while let Some((block, rsp_tx)) = self.queued.remove(&self.next_height) {
            let result = self.block_writer.commit_checkpoint_verified(block);
            let committed = result.is_ok();
            let _ = rsp_tx.send(result);

            if !committed {
                break;
            }
            self.next_height = (self.next_height + 1).expect("test chains are short");
        }

        rsp_rx
    }
}
