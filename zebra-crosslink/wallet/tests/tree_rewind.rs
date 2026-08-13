// Reproduces the wallet sync loop's note-commitment-tree call sequence around a reorg,
// using the same shardtree/orchard types as wallet/src/lib.rs, to test the
// Finding 1 hypothesis: a rewind distance (MAX_BLOCKS_TO_DOWNLOAD_AT_TIME,
// 64 -> 1024 in 0bb34d93) that exceeds checkpoint retention (CHECKPOINTS_N, then 100)
// makes `truncate_to_checkpoint` a silent no-op, after which the rescan double-appends and
// every anchor/witness the wallet produces is wrong. The v11-distance test runs the
// identical sequence and recovers, as does the shipped rewind distance under the
// retention derived from it.

use incrementalmerkletree::{Position, Retention};
use orchard::tree::MerkleHashOrchard;
use zcash_protocol::consensus::MAX_BLOCK_REORG_HEIGHT;

const DEPTH: u8 = orchard::NOTE_COMMITMENT_TREE_DEPTH as u8;
const SHARD_HEIGHT: u8 = 16;
const V12_REWIND: u32 = 1024; // MAX_BLOCKS_TO_DOWNLOAD_AT_TIME, the rewind distance v12 shipped
const V11_REWIND: u32 = 64;
const BUGGY_CHECKPOINTS_N: usize = 100;
// Mirrors wallet_main's REWIND_DISTANCE and CHECKPOINTS_N, which are function-local. Rooted
// in the same MAX_BLOCK_REORG_HEIGHT the wallet derives from, so the two cannot drift apart.
const REWIND_DISTANCE: u32 = MAX_BLOCK_REORG_HEIGHT + 1;
const FIXED_CHECKPOINTS_N: usize = REWIND_DISTANCE as usize + 1;
const LEAVES_PER_BLOCK: u64 = 2;

type Tree = shardtree::ShardTree<
    shardtree::store::memory::MemoryShardStore<MerkleHashOrchard, u32>,
    DEPTH,
    SHARD_HEIGHT,
>;

fn leaf(chain_tag: u64, h: u32, i: u64) -> MerkleHashOrchard {
    // Low 128 bits are always a valid Pallas base element.
    let mut b = [0u8; 32];
    b[..8].copy_from_slice(&((h as u64) * LEAVES_PER_BLOCK + i + 1).to_le_bytes());
    b[8..16].copy_from_slice(&chain_tag.to_le_bytes());
    Option::from(MerkleHashOrchard::from_bytes(&b)).expect("valid field element")
}

fn block_leaves(h: u32, fork_h: u32, new_chain: bool) -> Vec<MerkleHashOrchard> {
    let tag = if new_chain && h >= fork_h { 1 } else { 0 };
    (0..LEAVES_PER_BLOCK).map(|i| leaf(tag, h, i)).collect()
}

// Mirrors shard_tree_size (lib.rs:2704).
fn size(t: &Tree) -> u64 {
    t.max_leaf_position(None)
        .expect("Infallible Memory Store")
        .map_or(0, |p| u64::from(p) + 1)
}

// Mirrors read_compact_tx's append predicate (lib.rs:2830-2838) and the per-block
// checkpoint (lib.rs:4147-4152). Returns whether the checkpoint was actually added.
fn scan_block(t: &mut Tree, h: u32, fork_h: u32, new_chain: bool, next_pos: &mut u64) -> bool {
    for lf in block_leaves(h, fork_h, new_chain) {
        if *next_pos >= size(t) {
            assert_eq!(*next_pos, size(t), "should be appending sequentially");
            t.append(lf, Retention::Marked).expect("Infallible Memory Store");
        }
        *next_pos += 1;
    }
    assert_eq!(*next_pos, size(t));
    t.checkpoint(h).expect("Infallible Memory Store")
}

// Mirrors wallet startup (lib.rs:3360-3361) plus a scan to `tip`.
fn build_chain(tip: u32, fork_h: u32, new_chain: bool, checkpoints_n: usize) -> Tree {
    let mut t = Tree::new(
        shardtree::store::memory::MemoryShardStore::empty(),
        checkpoints_n,
    );
    t.checkpoint(0).unwrap();
    let mut next_pos = 0u64;
    for h in 1..=tip {
        assert!(scan_block(&mut t, h, fork_h, new_chain, &mut next_pos));
    }
    t
}

fn root_at(t: &Tree, h: u32) -> [u8; 32] {
    t.root_at_checkpoint_id(&h)
        .expect("Infallible Memory Store")
        .expect("checkpoint exists")
        .to_bytes()
}

#[test]
fn v12_1024_rewind_silently_corrupts_the_tree() {
    const TIP: u32 = 1200;
    let mut t = build_chain(TIP, TIP, false, BUGGY_CHECKPOINTS_N);
    let size_at_tip = size(&t);
    assert_eq!(size_at_tip, (TIP as u64) * LEAVES_PER_BLOCK);

    // Retention boundary: only the newest BUGGY_CHECKPOINTS_N ids survive pruning.
    assert!(matches!(t.root_at_checkpoint_id(&(TIP - 99)), Ok(Some(_))));
    assert!(matches!(t.root_at_checkpoint_id(&(TIP - 100)), Ok(None)));

    // Reorg at the tip: the wallet re-requests from tip+1-1024 (lib.rs:3699) and
    // truncates to the block before the range (lib.rs:3929-3933), ignoring the result.
    let sync_start_h = TIP + 1 - V12_REWIND;
    let last_block_h = sync_start_h - 1;
    assert!(matches!(t.truncate_to_checkpoint(&last_block_h), Ok(false)));
    assert_eq!(size(&t), size_at_tip, "the truncation was a silent no-op");

    // The rescan starts from the un-truncated size (lib.rs:4129-4132) and re-appends
    // the whole range; per-block checkpoints are refused up to the old tip.
    let mut next_pos = size(&t);
    let mut checkpoints_refused = 0u32;
    for h in sync_start_h..=TIP + 1 {
        if !scan_block(&mut t, h, TIP, true, &mut next_pos) {
            checkpoints_refused += 1;
        }
    }
    assert_eq!(checkpoints_refused, TIP - sync_start_h + 1);
    assert_eq!(
        size(&t),
        size_at_tip + ((TIP + 2 - sync_start_h) as u64) * LEAVES_PER_BLOCK,
        "every rescanned leaf was appended a second time"
    );

    // The fresh checkpoint past the old tip roots the corrupted tree, not the chain.
    let ctrl_new = build_chain(TIP + 1, TIP, true, BUGGY_CHECKPOINTS_N);
    assert_ne!(root_at(&t, TIP + 1), root_at(&ctrl_new, TIP + 1));

    // An old note (position 100, received in block 51) still witnesses at the stale tip
    // checkpoint, but the proven root is the abandoned chain's, which the node rejects:
    // the tx builds, then "failed to send".
    let ctrl_old = build_chain(TIP, TIP, false, BUGGY_CHECKPOINTS_N);
    let w = t
        .witness_at_checkpoint_id(Position::from(100u64), &TIP)
        .unwrap()
        .unwrap();
    let proven = w.root(leaf(0, 51, 0)).to_bytes();
    assert_eq!(proven, root_at(&ctrl_old, TIP));
    assert_ne!(proven, root_at(&ctrl_new, TIP));

    // A note re-discovered during the rescan (block 300) gets a position shifted past
    // the stale checkpoint's position: witness errors (NotContained), "failed to build".
    let shifted = size_at_tip + ((300 - sync_start_h) as u64) * LEAVES_PER_BLOCK;
    assert!(t
        .witness_at_checkpoint_id(Position::from(shifted), &TIP)
        .is_err());

    // A later reorg rewinds by 1024 again and also no-ops: no self-heal until restart.
    assert!(matches!(
        t.truncate_to_checkpoint(&(TIP + 1 - V12_REWIND)),
        Ok(false)
    ));
}

#[test]
fn v11_64_rewind_recovers_the_tree() {
    const TIP: u32 = 1200;
    let mut t = build_chain(TIP, TIP, false, BUGGY_CHECKPOINTS_N);

    let sync_start_h = TIP + 1 - V11_REWIND;
    let last_block_h = sync_start_h - 1;
    assert!(matches!(t.truncate_to_checkpoint(&last_block_h), Ok(true)));
    assert_eq!(size(&t), (last_block_h as u64) * LEAVES_PER_BLOCK);

    let mut next_pos = size(&t);
    for h in sync_start_h..=TIP + 1 {
        assert!(scan_block(&mut t, h, TIP, true, &mut next_pos));
    }
    assert_eq!(size(&t), ((TIP + 1) as u64) * LEAVES_PER_BLOCK);

    let ctrl_new = build_chain(TIP + 1, TIP, true, BUGGY_CHECKPOINTS_N);
    assert_eq!(root_at(&t, TIP + 1), root_at(&ctrl_new, TIP + 1));

    let w = t
        .witness_at_checkpoint_id(Position::from(100u64), &(TIP + 1))
        .unwrap()
        .unwrap();
    assert_eq!(
        w.root(leaf(0, 51, 0)).to_bytes(),
        root_at(&ctrl_new, TIP + 1)
    );
}

// Retention keeps the newest N checkpoint ids, spanning N-1 back from the scan front,
// while a rewind of R truncates to the checkpoint R back: N must be at least R + 1.
#[test]
fn retention_must_exceed_rewind_by_one() {
    const TIP: u32 = 300;
    const R: u32 = REWIND_DISTANCE;

    let mut t = build_chain(TIP, TIP, false, R as usize);
    assert!(matches!(t.truncate_to_checkpoint(&(TIP - R)), Ok(false)));

    let mut t = build_chain(TIP, TIP, false, R as usize + 1);
    assert!(matches!(t.truncate_to_checkpoint(&(TIP - R)), Ok(true)));
}

// The shipped shape end to end: a rewind of the deepest reorg the node will accept, under
// retention derived from that same bound, recovers the tree exactly. The truncate lands on
// the oldest surviving checkpoint, so this also pins that the derivation has no slack left.
#[test]
fn fixed_retention_survives_the_deepest_reorg_rewind() {
    // Long enough that pruning has actually begun before the reorg.
    const TIP: u32 = 4200;
    let mut t = build_chain(TIP, TIP, false, FIXED_CHECKPOINTS_N);

    // Pruning boundary under the fixed retention.
    assert!(matches!(
        t.root_at_checkpoint_id(&(TIP - FIXED_CHECKPOINTS_N as u32 + 1)),
        Ok(Some(_))
    ));
    assert!(matches!(
        t.root_at_checkpoint_id(&(TIP - FIXED_CHECKPOINTS_N as u32)),
        Ok(None)
    ));

    let sync_start_h = TIP + 1 - REWIND_DISTANCE;
    let last_block_h = sync_start_h - 1;
    assert!(matches!(t.truncate_to_checkpoint(&last_block_h), Ok(true)));
    assert_eq!(size(&t), (last_block_h as u64) * LEAVES_PER_BLOCK);

    let mut next_pos = size(&t);
    for h in sync_start_h..=TIP + 1 {
        assert!(scan_block(&mut t, h, TIP, true, &mut next_pos));
    }
    assert_eq!(size(&t), ((TIP + 1) as u64) * LEAVES_PER_BLOCK);

    let ctrl_new = build_chain(TIP + 1, TIP, true, FIXED_CHECKPOINTS_N);
    assert_eq!(root_at(&t, TIP + 1), root_at(&ctrl_new, TIP + 1));

    let w = t
        .witness_at_checkpoint_id(Position::from(100u64), &(TIP + 1))
        .unwrap()
        .unwrap();
    assert_eq!(
        w.root(leaf(0, 51, 0)).to_bytes(),
        root_at(&ctrl_new, TIP + 1)
    );
}
