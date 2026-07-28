//! Queued blocks that are awaiting their parent block for verification.

use std::{
    collections::{hash_map::Drain, BTreeMap, HashMap, HashSet, VecDeque},
    iter, mem,
};

use tokio::sync::oneshot;
use tracing::instrument;

use zebra_chain::{block, transparent};

use crate::{
    BoxError, CheckpointVerifiedBlock, CommitSemanticallyVerifiedError, SemanticallyVerifiedBlock,
    ValidateContextError,
};


/// A queued checkpoint verified block, and its corresponding [`Result`] channel.
pub type QueuedCheckpointVerified = (
    CheckpointVerifiedBlock,
    oneshot::Sender<Result<block::Hash, BoxError>>,
);

/// A queued semantically verified block, and its corresponding [`Result`] channel.
pub type QueuedSemanticallyVerified = (
    SemanticallyVerifiedBlock,
    oneshot::Sender<Result<block::Hash, CommitSemanticallyVerifiedError>>,
);
