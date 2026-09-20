# Crosslink finality implementation plan

[`FINALITY.md`](./FINALITY.md) defines the behavior. This file orders the work that brings the
code to it. Each stage names the FINALITY.md sections it implements, the code it touches, and
the condition under which it is done. Where this file and FINALITY.md disagree, FINALITY.md is
right and this file is corrected.

Stages 1 to 4 are done. Stages 5 to 8 move finality state out of
`zebra-crosslink/zebra-crosslink` into `zebra-state` (FINALITY.md §7.1), each of them for a race
or a coupling that the crate boundary creates; they leave the crate holding no finality state.
Stage 9 then gives the node the second chain state that FINALITY.md §4.3 requires, so that a BFT
branch conflicting with the depth commit is recorded rather than abandoned.
Removing what is left of the crate afterwards is separate work, in
[`CRATE_REMOVAL.md`](./CRATE_REMOVAL.md); it depends on these stages, nothing depends on it, and
it changes no behavior.

Stages run one at a time, in order. Each stage ends the same way: its node tests pass, it is
committed, and the dilated two-node regtest (DILATED_REGTEST.md, `zebra-crosslink/dilated_regtest/run.sh`)
passes against a build of that commit before the next stage starts. Stages 1 and 2 edit
the same test files, and stages 3 and 4 depend on stage 1.

## Rules for every stage

- Read the FINALITY.md sections a stage cites before changing code. FINALITY.md labels each
  statement as **Book**, **Zebra Crosslink**, or **current tree**. Zebra Crosslink statements
  are requirements, Book statements are requirements wherever FINALITY.md does not record a
  Zebra Crosslink departure, and current-tree statements describe code that changes.
- No stage adds a mechanism a later stage deletes. Each stage says what it deletes, and a stage
  whose deletions all sit in a later stage is in the wrong place in the order. Moving code is
  not writing it: a stage may carry code across a crate boundary that a later stage removes.
- Nothing new is written in `zebra-crosslink/zebra-crosslink`. Work that would land there lands
  in `zebra-state` instead, even where the crate's existing structure would take it
  (FINALITY.md §7.1). That a change fits the TFL service's main loop is an argument against it.
- `σ` and the staking reward and payout code belong to other work. No stage changes
  `bc_confirmation_depth_sigma`, `POS_BLOCK_REWARD_ZATS`, `update_bonds_with_pos_issuance`,
  `fixup_aggregated_stakes`, or the wallet reward projection.
- PoS stores and databases written by an earlier derivation are deleted, not migrated. No stage
  adds code that loads them.
- A stage that changes code FINALITY.md describes as current tree updates those FINALITY.md
  statements in the same commit (FINALITY.md §§5, 6, 8).
- `VIZ_GUI_FINALITY_RULES.md` is untracked on purpose and is never committed.
- One agent at a time in one tree. Stages 6 to 9 edit the same files and the same tests, so the
  next stage starts only after the previous one is committed and its regtest has printed `PASS`.
- No stage pushes anything. An agent that believes a stage needs a push has misread it.
- Tests run through `phest.bat zebra-crosslink`, with a test-name filter as the fourth
  argument: `phest.bat zebra-crosslink Debug Win64 <filter>`. Never call `cargo` directly.
  `phargo.bat` enables `viz_gui` for `zebra-crosslink`, which puts winit on the main thread.
  The node tests in `zebrad/tests/crosslink.rs` run headless, on the crate's
  `cfg(not(feature = "viz_gui"))` path (FINALITY.md §8.1), so they run with
  `PH_NO_VIZ_GUI=1` set, which leaves the feature out of an otherwise identical build.
  Each node test boots a zebrad in the test process and ends it with `process::exit`, so a
  test run is one process per test, and the harness's capture is turned off so the
  runner's per-instruction dump survives an abort:
  `$env:PH_NO_VIZ_GUI=1; $env:RUST_TEST_THREADS=1; $env:RUST_TEST_NOCAPTURE=1; .\phest.bat zebra-crosslink Debug Win64 -p zebrad --test crosslink <test name>`.
  Both settings are environment variables because `phargo.bat` forwards `%4` through
  `%9` and splits `--test-threads=1` at the `=`, so a trailing `-- --nocapture
  --test-threads=1` never reaches the harness.
  A winit panic means the feature was left on; it is not worked around in the test code.
- After a stage is committed, `zebra-crosslink/dilated_regtest/run.sh` runs against a debug
  build of the commit and must print `PASS` (DILATED_REGTEST.md). It is the system test the
  node tests are not: wallet, staking, BFT bootstrap and finality under 90x time dilation.
- A stage's test condition is read against the node tests that fail for reasons outside the
  stages. None of them is fixed or worked around by a stage:
  - `staking_tx_create_bond` in `zebrad/tests/crosslink.rs` leaves the bond signature zero,
    and the sync path verifies staking signatures, so
    `crosslink_pow_block_with_staking_tx` and `crosslink_add_newcomer_to_roster_via_pow`
    fail at the block carrying the bond. The node does not answer the harness's submission
    of that block, so the test process stalls there and has to be killed from outside.
- A block loaded with `SHOULD_FAIL` costs the harness's full 30-second submission deadline:
  a rejected block never gets an answer from the ingest queue. The fork-rejection tests and
  diagram scene 3 therefore take minutes, not seconds.
- `REGTEST_BLOCK_BYTES` and `REGTEST_POS_BLOCK_BYTES` are written by the `#[ignore]`d
  `regen_test_data` in `zebrad/tests/crosslink.rs`; the diagram scenes by
  `crosslink_write_finality_diagram_scenes`. Both are regenerated, never hand-edited, whenever
  the block format or the header count of a BFT block changes.
- The build uses `panic = abort`: a new `assert!`, `unwrap`, or `expect` on a consensus path
  terminates the node when it fails.
- A gap between what a stage asks for and what the test format can express is reported in the
  stage's commit message and left as an `@Todo` beside the test. It never stops the stage and it
  never changes the test format, which stage 3 set.
- Encoding of new database rows, and whether a derived value is stored or recomputed, are the
  implementing stage's call. Recomputing wins where the value is a function of the chain, so
  that one derivation exists (FINALITY.md §8.1).

## Stage 1: Snapshot is `parent(headers_bc[0])`

Implements FINALITY.md §3.1 and the off-by-one item of §6.1.

`snapshot(B)` is the parent of the first carried header, which is `headers[0]`'s
previous-block hash. Every site that uses `headers[0]` itself as the finalized block changes
together (FINALITY.md §8.1, "The derivation is duplicated"):

- `handle_new_decided_bft_block` in `zebra-crosslink/zebra-crosslink/src/lib.rs`;
- `validate_bft_block`, same file;
- the PoS-store restore path in `tfl_service_main_loop`, including the replay watermark
  `prev_finalized_bc_height`;
- `BftBlock::finalization_candidate()` in `librustzcash/zcash_primitives/src/bft.rs`, replaced
  by an accessor that returns the snapshot hash;
- `test_format.rs`;
- `viz2.rs`, both the live viz response and `VizScene`; and
- the marker positions asserted by the finality-diagram tests in `zebrad/tests/crosslink.rs`
  and `viz2::scene_tests`.

In `propose_new_bft_block`, the `+ 1` in `proposed_final_height` is the old convention and
goes. A proposal improves when its snapshot, `tip − σ`, is above the parent bft-block's
snapshot.

Consequences to expect, not to work around:

- The BFT-height-1 roster moves one block below the bootstrap roster height (FINALITY.md §8.1).
- `CrosslinkFinalizeBlock` is sent the snapshot hash. The decide path still commits the decided
  block; that policy changes in stage 7.

Done when no site derives finality from `headers[0]`'s own hash, the finality-diagram tests
assert the new positions, the crosslink node tests pass, and FINALITY.md §§5.1, 6.1, and 8.1
describe the new derivation.

## Stage 2: Remove dead parameters and state

Implements FINALITY.md §1, §5.1 (`current_bc_final`), and §8.1.

- Delete the Book's `L` from `ZcashCrosslinkParameters` and `PROTOTYPE_PARAMETERS`
  in `librustzcash/zcash_primitives/src/bft.rs`, and from the parameter serialization in
  `test_format.rs`. The second parameter value stays in the instruction and is written as zero,
  so the instruction's width is unchanged and older files still load.
- Regenerate each `.zeccltf` file in `zebra-crosslink/crosslink-test-data` that a test
  generates. Keep each file that a test loads but does not generate; those carry no parameter
  instruction at all, so they load unchanged.
- Delete `TFLServiceInternal::current_bc_final` and its initialization and assignment.
- Rename the main-loop local `current_bc_tip` to `bc_best_tip`.
- Delete the `ba_mu` paragraph from `diagram_scene_1`'s doc comment in
  `zebrad/tests/crosslink.rs`.

Done when the workspace builds, no `.zeccltf` file carries the removed value, the tests that
load `.zeccltf` files pass, and FINALITY.md no longer lists these as present.

## Stage 3: Validity rules, honest proposal, honest context selection

Implements FINALITY.md §3.4 and §6.2. Depends on stage 1.

**Last Final Snapshot** (bc-block admission). `snapshot(LF(H)) ⪯ H`. It sits beside the
Extension rule in `call_from_state_to_crosslink_to_ask_about_fat_pointers`, with the same
defer/reject split: reject when `H`'s ancestry is known and does not contain the snapshot,
defer when the snapshot block is unknown.

**Linearity** (bft-block validity). `snapshot(parent(B)) ⪯ snapshot(B)`, checked in
`validate_bft_block`. A snapshot block that is not yet known returns `Indeterminate`, as a
missing header block does today.

**Tail Confirmation** (bft-block validity). `B.headers_bc` holds exactly `σ` headers, each
header's previous-block hash is the hash of the one before it, and the block at every header
is bc-valid. This is objective and does not consult the validator's best chain (FINALITY.md
§3.4). A block that is not yet known returns `Indeterminate`.

**Honest proposal.** The proposal carries the `σ`-block tail of the proposer's `bc_best`,
except where the `+40` clamp binds:

- Keep the `+40` candidate clamp (the `min(…, latest_final_block + 40)` line). Comment it as
  a Zebra Crosslink design heuristic that is not part of the Crosslink 2 specification and
  departs from honest proposal (FINALITY.md §3.4).
- Read the tip and the tail consistently: `StateRequest::Tip` followed by
  `FindBlockHeaders` can straddle a reorganization.
- Make no proposal while `bc_best` has fewer than `σ + 1` blocks.
- When the tail's snapshot is not above the parent bft-block's snapshot, or fails Linearity
  against it, keep declining to propose. Honest proposal repeats the parent's headers in that
  case; how often to do so is design question 3. Comment the decline with a reference to it.

**Honest context selection** (block templates). `FatPointerToBFTChainTip` cites the newest
decided bft-block that both satisfies the existing `do_not_include_until_bc_height` condition
and has a snapshot on the template's parent chain. The parent block's own context always
qualifies, so a template always has one.

Done when each rule has a test that feeds a violating block and sees it rejected, the existing
crosslink node tests pass, and FINALITY.md §§3.4, 6.2, and 8.1 describe these rules as
enforced. When the test format cannot express a violating block, the stage reports which rule
lacks a test instead of changing the format.

## Stage 4: Roster read separately from the commit

Implements FINALITY.md §7.3. Depends on stage 1.

After stage 1 the decide path commits `snapshot(B_{H−1})`, so the stakes that
`CrosslinkFinalized` returns are already the bonds at that block. Stage 7 stops the decide path
from committing, so the roster needs its own read:

- Add a zebra-state read request that returns the aggregated stakes at a block hash. It reads
  `aggregated_stakes_by_hash` in the finalized database.
- `handle_new_decided_bft_block` and the PoS-store restore path fill
  `finalizers_at_current_height` from that request for `snapshot(B_{H−1})`, and pass that
  block's height to `terminated_finalizers_at`.
- `CrosslinkFinalized` stops carrying stakes.

Non-finalized chains keep only their tip's bond state (`Chain::delegation_bonds`), so this read
covers only committed blocks. That holds while the decide path commits the snapshot. Stage 7,
which stops it committing, adds the per-block aggregate to `Chain` that answers the same read
above the finalized tip.

Done when the roster at every BFT height equals what the commit reply produced before, and no
code reads stakes from the commit reply.

## Stage 5: The BFT chain and its readers move into zebra-state

Implements FINALITY.md §7.1, and removes the bc-block admission and proposal items of §6.7.
Depends on stages 1, 3 and 4.

The decided bft-chain moves together with the code that reads it. A copy pushed into
`zebra-state` while its readers stay behind is two copies of consensus state, which is the
failure the PoS store file already shows (FINALITY.md §8.1).

- `new_network` holds the decided bft-chain: the blocks by BFT height, the hash-to-height index,
  the fat pointer to its tip, the proposal signatures, and the roster at the current height.
- It constructs the five closures of `tenderlink::entry_point` (FINALITY.md §7.1) and spawns the
  engine. Each closure sends one message to the block writer and awaits one reply; every chain
  read behind that reply is local.
- `propose_new_bft_block` reads the tip and the σ-block tail as one view, so the two reads can no
  longer straddle a reorganization (FINALITY.md §6.2). The `+40` candidate clamp, the
  improvement test and the decision not to propose are unchanged.
- `validate_bft_block` resolves Linearity and Tail Confirmation against the chains the writer
  holds, and keeps returning `Indeterminate` with the block it needs.
- `handle_new_decided_bft_block` stores the decision and answers the decide closure with the
  roster and vote namespace for the next height, computed from the bonds at the snapshot. That
  is stage 4's read made local. It still sends `CrosslinkFinalizeBlock`; stage 7 stops it.
- The fat-pointer check becomes a function of that store and the chains the writer already
  holds: resolve `H.context_bft` to its bft-block, take `snapshot(LF(H))`, look up its height,
  test ancestry against `H`. Extension, Last Final Snapshot and the σ-confirmation depth are the
  same rules (FINALITY.md §6.2); only their inputs are local.
- The payout verdict rides the same path with the same rule: `cert(P) != cert(parent(P))` and
  `height(P) − F ≤ σ + FINALITY_LIVENESS_ALLOWANCE`, carried on
  `SemanticallyVerifiedBlock::pos_payout` and persisted in the non-finalized state backup
  (FINALITY.md §5.4). The constants, the three copies that must agree, and the backup do not
  change.
- `FatPointerToBFTChainTip` and `Roster` become read requests. The three block-template sites in
  `zebra-rpc` and the `Roster` site in `zebrad/src/lightwalletd.rs` call them there.
- The round-state snapshot the `bft_access_closure` fills becomes a watch channel `new_network`
  publishes; `get_tfl_recency_status` reads that channel.
- The PoS store file moves unchanged, as the store's own persistence. Stage 6 replaces it.

Deletes: `ClosureToCallIntoCrosslinkFromState` and the mutex that holds it,
`call_from_state_to_crosslink_to_ask_about_fat_pointers`, the `crosslink_gate` parameter of
`new_network::sync` with the closures `zebrad/src/commands/start.rs` and `zebrad/src/fuzz.rs`
pass to it, `spawn_tenderlink`, `TFLServiceInternal::{bft_blocks, bft_block_hash_to_height,
fat_pointer_to_tip, finalizers_at_current_height, recency_status}`,
`TFLServiceRequest::{FatPointerToBFTChainTip, Roster, FinalizersRecencyStatus}`, and the
reentrancy constraint of FINALITY.md §8.1: after this stage no code in `zebra-state` calls out
of it, so there is no lock ordering to respect.

This is the largest stage, and it does not split further without leaving a second copy of the
bft-chain behind. If it has to be split, the split is by writing the target structure in
`zebra-state` first and deleting each reader from the crate as it moves, never by mirroring the
store into both.

Done when the crosslink node tests and the dilated regtest pass, `new_network::sync` takes no
closure into another crate, and the node admits and rejects the same bc-blocks as before: the
fork-rejection tests, `crosslink_test_basic_finality` and the three bft-validity tests are the
coverage for that.

## Stage 6: The BFT chain persists in the finalized database

Implements FINALITY.md §7.1 and the two-stores pitfall of §8.1. Depends on stage 5.

- Column families hold the decided bft-blocks by BFT height, their fat pointers and the proposal
  signatures, written in the batch the decision triggers.
- Startup reads them back and builds `tenderlink`'s `ingest_startup_data` from the database.
  `decided_round_data` keeps its shape.
- The roster for each restored height is recomputed from the bonds at that height's snapshot,
  never read back as stored bytes. BFT genesis is recomputed at startup from the chain at the
  bootstrap roster height rather than stored as a row, for the same reason.
- A database whose bc-tip is past the bootstrap activation height but which holds no bft rows is
  refused at startup, with a message naming the state directory to delete. It is not migrated,
  not re-bootstrapped, and not run PoW-only: no node on this branch should be carrying one.
  The cache directory namespace in `zebrad/src/config.rs` was bumped to
  `crosslink_nightly_20260921` when this plan's staging began, so a developer picking the branch
  up syncs into an empty directory and never meets the refusal.
- Adding column families is a minor `DATABASE_FORMAT_VERSION` bump.

Deletes: the PoS store file, `path_to_pos_store_file` and its configuration, the append path,
the restore path with its `KnownBlock` unwrap and its `prev_finalized_bc_height` watermark, and
the stored roster bytes that made an old store disagree with the votes that travel by roster
index (FINALITY.md §8.1).

`zebra-crosslink/dilated_regtest/run.sh` has no restart step, so this stage adds one: stop both
nodes mid-run, start them again against the same state directories, and check that BFT resumes at
the height it left. DILATED_REGTEST.md is updated with it in the same commit.

Done when a node restarted with its database intact rejoins BFT at the height it left with no
PoS store file present, and the dilated regtest passes across a restart of both nodes.

## Stage 7: `fin`

Implements FINALITY.md §3.2, §4.3 "Implementation in Zebra Crosslink", §6.1 and §6.3. Depends
on stages 5 and 6.

- `fin` is a column of the finalized database, written in or after the batch that commits the
  block it names.
- Where a commit changes the best chain, `new_network` computes `N := candidate(bc_best)`; if
  `fin ⪯ N` and `N ≠ fin` it finalizes up to `N` and writes `fin := N`. The `fin ⪯ N` test is at
  the caller. `update_latest_chain_channels` reports the best tip it sends, so the trigger is a
  change of best chain, not every commit.
- `candidate(bc_best)` resolves the best tip's `fat_pointer_to_bft_block` to its decided
  bft-block, takes that block's `snapshot_block_hash`, and clamps to `prune_σ(bc_best)` by the
  lca rule.
- A decision no longer commits. It advances `bft_final_snapshot`, which may sit on a chain that
  is not `bc_best`.
- A refused switch is reported on stdout, with an `@Todo` for the persisted hazard record.
- `Chain` carries its aggregated stakes per block, appended by `Chain::push` beside
  `bond_rewards` and `finalizer_commissions` and popped by `pop_tip`, from the same function
  `prepare_aggregated_stakes_batch` uses, so the roster read of stage 4 answers for held blocks
  above the finalized tip as well (FINALITY.md §8.1).
- The chain holding `bft_final_snapshot` is exempt from the pruning that drops the lowest-work
  chain past `MAX_NON_FINALIZED_CHAIN_FORKS`.
- Where the reorg-depth commit would write a block conflicting with `bft_final_snapshot`, the
  commit is held at the fork point while the conflict is live, up to `CONFLICT_HOLD_DEPTH` (999)
  blocks past the fork, after which it commits and reports on stdout that the node can no longer
  follow `bft_final_snapshot`. That is the interim of FINALITY.md §4.3 and carries an `@Todo`
  naming stage 9; the node never requires a resync.

Deletes: `TFLServiceInternal::latest_final_block`, `set_final_block`, `final_change_tx`,
`TFLServiceRequest::SetFinalBlockHash` with the RPC that reaches it,
`Request::CrosslinkFinalizeBlock` with `crosslink_finalize_via_new_network`,
`CROSSLINK_FINALIZE_SENDER` and the queue behind them, the unbounded retry loop on the decide
path, and with it the coupling that makes BFT progress wait on a finalized-state write
(FINALITY.md §5.2).

Two test consequences, neither a workaround:

- `crosslink_pow_switch_to_finalized_chain_fork_even_though_longer_chain_exists` asserts the
  collapse onto a decided branch. Under §4.3 the node instead keeps the branch containing `fin`,
  follows the heavier chain when `fin` is below the fork, and waits for a chain containing
  `snapshot(B)` to become best again before finality resumes (§3.4). The test is rewritten to
  that behavior: it is the only test of the fork-choice floor.
- Last Final Snapshot becomes testable. FINALITY.md §8.1 records that it cannot be tested while
  the decide path commits every snapshot, because the violating block's parent is gone before
  the block can be offered. This stage removes that, and adds the test.

Either test may want something the test format cannot express, such as waiting for a chain
containing `snapshot(B)` to become best again. Where it does, the stage reports the gap and moves
on under the rule above; the format does not change here.

Done when `fin` survives a restart, never moves backwards across a reorganization, the roster at
a snapshot held only in the non-finalized state equals the row the same block yields once
committed, and the two tests above hold.

## Stage 8: Finality readers move to the state service

Implements the FINALITY.md §7.2 consumer table. Depends on stage 7.

- `get_tfl_final_block_hash`, `get_tfl_final_block_height_and_hash`, block status and
  transaction status read `fin` and the best chain through `ReadStateService`, each in one read,
  so they cannot report two different moments.
- Finality-change notifications read a watch channel published beside the existing chain-tip
  channels, updated after `fin` is persisted.
- Block and transaction status follow the three states of the §7.2 table, including the rule
  that a block at or below `bft_final_snapshot` but above `fin` is not `Finalized`.
- An `@Todo` marks where the client exposure condition of FINALITY.md §3.5 applies.
- The absent-value and present-value tests are written at the `ReadStateService` request level
  against a populated state, not against a live JSON-RPC server. Coverage at the JSON-RPC level
  needs a harness that does not exist; an `@Todo` beside these tests records that, and building
  the harness is separate work.

Deletes: `TFLServiceRequest::{FinalBlockHeightHash, FinalBlockRx, BlockFinalityStatus,
TxFinalityStatus, IsTFLActivated}` with their handlers, `tfl_final_block_height_hash`,
`tfl_block_finality_from_height_hash` including the `BlockHeader` request it builds and never
awaits, and the two-request race behind block status (FINALITY.md §5.5).

Done when every consumer in the §7.2 table reads the state service, no finality answer reaches
the Crosslink service, and the absent-value and present-value cases of each finality RPC have a
test.

## Stage 9: A second chain state for a conflicting BFT branch

Implements FINALITY.md §4.3 "Implementation in Zebra Crosslink" (the second finalized state) and
§7.1. Depends on stages 7 and 8. It replaces the `CONFLICT_HOLD_DEPTH` interim stage 7 adds.

- The PoW state P keeps a snapshot of its finalized state at a height at or below `fin`, retaken
  as `fin` advances and left alone while finality lags. The snapshot must be openable as an
  independent, writable finalized state while P keeps writing.
- How the snapshot is taken is the storage engine's business and is chosen behind one interface,
  not spread through `zebra-state`: a hard-linked checkpoint under RocksDB, a persistent savepoint
  plus a reflink or byte clone under redb, or a logical copy into a fresh database. The last is
  the portable fallback and costs a full database of time and disk; a byte copy needs P's writer
  paused for its duration, which is one pause of the `new_network` block writer, reads
  unaffected. Zebra's move from RocksDB to redb must not change anything above this bullet.
- On a bc-block that forks below P's finalized tip but above `fin`, the node opens C from the
  snapshot, replays P's own stored blocks from the snapshot height to the fork point, and feeds C
  the conflicting chain from peers. C's fork-choice floor is `bft_final_snapshot`; C never
  depth-commits.
- `zebra-state` routes blocks and reads to both states and chooses the served best chain across
  both by the §4.3 switch rule. C is dropped when `fin` passes the fork; P's branch is recorded
  for as long as blocks arrive on it.
- The wallet's `REWIND_DISTANCE` and `CHECKPOINTS_N` cover a switch back to `fin`, which is
  deeper than `MAX_BLOCK_REORG_HEIGHT`.

Deletes: the `CONFLICT_HOLD_DEPTH` hold and its `@Todo` from stage 7.

Done when a two-node regtest in which one node is held on a PoW fork more than
`MAX_BLOCK_REORG_HEIGHT` blocks long while the other finalizes a conflicting branch rejoins
without a resync, both branches are still recorded on the held node afterwards, and bft-block
validation on the held node never stopped.

## Needs design pass

Each question below blocks a stage. A design session reads the cited FINALITY.md sections,
re-checks the code facts listed, asks the user, and records each answer in FINALITY.md as a
Zebra Crosslink requirement. It then updates the blocked stage here and deletes the question.
Question numbers are stable: an answered question is removed and its number is not reused.

### 3. Proposal cadence

FINALITY.md §3.4 says a proposal is always *possible*: when the tail does not extend the parent's
snapshot, an honest proposer repeats the parent's headers. It does not say when a proposer
*should* propose.

Code facts: Tenderlink calls the propose closure at the start of each round when it has no valid
value; the closure returns `None` unless the snapshot improves (stage 3 keeps this). The Book
ties Linearity's rationale to BFT liveness possibly requiring a minimum proposal rate
(construction.md lines 610–614).

To decide: whether a proposer repeats the parent's headers every round, only after some number
of empty rounds, or never; and whether a bft-block with an unchanged snapshot has any other
effect, such as carrying hardforks or `do_not_include_until_bc_height`.

### 4. `do_not_include_until_bc_height`

This is a Zebra-only bft-block field. A hardfork bft-block sets it to its greatest
`pow_activation_height`, later blocks carry it forward monotonically, and a bc-block may not
cite a bft-block whose value exceeds the bc-block's height.

To decide: whether it holds any consequence for Last Final Snapshot, Extension, `candidate`
monotonicity, or the roster at the previous decided bft-block's snapshot; and whether
FINALITY.md defines it as a Zebra Crosslink validity rule. Stage 3's template selection keeps it
as an extra condition, and after stage 5 the template check and the admission check are the same
code.

### 5. `MAX_BLOCK_REORG_HEIGHT` 99 → 999

FINALITY.md §4.3 records 999 as the intended value; the tree has 99 in
`zcash_protocol::consensus`.

Code facts: `ZcashCrosslinkParameters::bootstrap_is_valid` requires
`activation_height − roster_height > MAX_BLOCK_REORG_HEIGHT`, and a `const _: () = assert!` on
`PROTOTYPE_PARAMETERS` checks it while compiling; the prototype gap is 200 blocks, so 999 stops
the workspace building until the bootstrap heights move with it. The wallet's `REWIND_DISTANCE`
and `CHECKPOINTS_N` derive from the constant. zebra-chain has a separate constant of 1000, and
comments at `zebra-state/src/request.rs` and `non_finalized_state.rs` say 1000. The
non-finalized state holds up to that many blocks per chain in memory. The depth commit is the
second floor under `fin` (FINALITY.md §8.1); a larger value makes stage 7's hold and stage 9's
second state rarer, and is no longer the difference between recovering and needing a resync.

No implementation stage changes the constant. The tree keeps 99 until the bootstrap heights that
go with 999 exist, because raising it before then stops the workspace compiling.

To decide: those bootstrap heights, and which stage carries the change once they are chosen.
