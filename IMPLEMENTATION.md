# Crosslink finality implementation plan

[`FINALITY.md`](./FINALITY.md) defines the behavior. This file orders the work that brings the
code to it. Each stage names the FINALITY.md sections it implements, the code it touches, and
the condition under which it is done. Where this file and FINALITY.md disagree, FINALITY.md is
right and this file is corrected.

Stages 1–4 are ready. Stages 5 and 6 wait on the questions in
[Needs design pass](#needs-design-pass).

## Rules for every stage

- Read the FINALITY.md sections a stage cites before changing code. FINALITY.md labels each
  statement as **Book**, **Zebra Crosslink**, or **current tree**. Zebra Crosslink statements
  are requirements, Book statements are requirements wherever FINALITY.md does not record a
  Zebra Crosslink departure, and current-tree statements describe code that changes.
- `σ` and the staking reward and payout code belong to other work. No stage changes
  `bc_confirmation_depth_sigma`, `POS_BLOCK_REWARD_ZATS`, `update_bonds_with_pos_issuance`,
  `fixup_aggregated_stakes`, or the wallet reward projection.
- PoS stores and databases written by an earlier derivation are deleted, not migrated. No stage
  adds code that loads them.
- A stage that changes code FINALITY.md describes as current tree updates those FINALITY.md
  statements in the same commit (FINALITY.md §§5, 6, 8).
- `VIZ_GUI_FINALITY_RULES.md` is untracked on purpose and is never committed.
- Crosslink node tests in `zebrad/tests/crosslink.rs` panic under `viz_gui`, so they run under
  plain cargo without that feature (FINALITY.md §8.1).
- The build uses `panic = abort`: a new `assert!`, `unwrap`, or `expect` on a consensus path
  terminates the node when it fails.

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

- The BFT-height-1 roster moves one block below the activation height (FINALITY.md §8.1).
- `CrosslinkFinalizeBlock` is sent the snapshot hash. The decide path still commits the decided
  block; that policy changes in stage 5.

Done when no site derives finality from `headers[0]`'s own hash, the finality-diagram tests
assert the new positions, the crosslink node tests pass, and FINALITY.md §§5.1, 6.1, and 8.1
describe the new derivation.

## Stage 2: Remove dead parameters and state

Implements FINALITY.md §1 (`finalization_gap_bound`), §5.1 (`current_bc_final`), and §8.1.

- Delete `finalization_gap_bound` from `ZcashCrosslinkParameters` and `PROTOTYPE_PARAMETERS`
  in `librustzcash/zcash_primitives/src/bft.rs`, and from the parameter serialization in
  `test_format.rs`. The test format loses its second parameter value.
- Regenerate each `.zeccltf` file in `zebra-crosslink/crosslink-test-data` that a test
  generates. Ask the user before deleting a file that a test loads but does not generate.
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

**Honest proposal.** The proposal carries the `σ`-block tail of the proposer's `bc_best`:

- Delete the `+40` candidate clamp (the `min(…, latest_final_block + 40)` line).
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

Implements FINALITY.md §7, "Consensus-sensitive roster and hardfork inputs". Depends on
stage 1.

After stage 1 the decide path commits `snapshot(B_{H−1})`, so the stakes that
`CrosslinkFinalized` returns are already the bonds at that block. Stage 5 stops the decide path
from committing, so the roster needs its own read:

- Add a zebra-state read request that returns the aggregated stakes at a block hash. It reads
  `aggregated_stakes_by_hash` in the finalized database.
- `handle_new_decided_bft_block` and the PoS-store restore path fill
  `finalizers_at_current_height` from that request for `snapshot(B_{H−1})`, and pass that
  block's height to `terminated_finalizers_at`.
- `CrosslinkFinalized` stops carrying stakes.

Non-finalized chains keep only their tip's bond state (`Chain::delegation_bonds`), so this read
covers only committed blocks. That holds while the decide path commits the snapshot. Reading
bonds at a block that is not committed is design question 2.

Done when the roster at every BFT height equals what the commit reply produced before, and no
code reads stakes from the commit reply.

## Stage 5 (blocked): Decouple decisions from commits; persist `fin`

Implements FINALITY.md §4.3 "Implementation in Zebra Crosslink", §5.2, §7, and the related
pitfalls in §8.1. Blocked on design questions 1, 2, and 5.

- A BFT decision advances `bft_final_snapshot` and does not wait on a finalized-state write.
- On every `bc_best` change, compute `N := candidate(bc_best)`. If `fin ⪯ N` and `N ≠ fin`,
  commit `N` through `CrosslinkFinalizeBlock`, then store `N` as `fin` in the finalized
  database, in the commit's batch or after it. The `fin ⪯ N` check is at the caller.
- Report a refused switch on stdout, with an `@Todo` for a persisted hazard record.
- Replace `latest_final_block` with the persisted `fin` for every reader: RPC, GUI,
  notifications, the proposal path, and the diagnostic. Name it `local_finalized_tip`.
- Block and transaction status follow the FINALITY.md §7 table.
- Mark with an `@Todo` where the client exposure condition of FINALITY.md §3.5 applies.
- Annotate the path where the reorg-depth commit conflicts with `bft_final_snapshot`: the node
  stops following that chain, and BFT validation ends until it resyncs.

## Stage 6 (blocked): Finalized side chain and PoS in zebra-state

Implements FINALITY.md §4.3 "Implementation in Zebra Crosslink" and §7. Blocked on design
questions 1, 2, and 6.

- Sync and store the chain to `bft_final_snapshot`, its BFT decisions, and its bond state,
  whether or not it is `bc_best`, and survive a restart.
- Process BFT certificates and compute rosters in `NonFinalizedState`.
- Move the Proof-of-Stake logic into `zebra-state`, so that bc-block validity, bft-block
  validity, and roster computation run in one synchronous domain.

## Needs design pass

Each question below blocks stage 5 or 6. A design session reads the cited FINALITY.md sections,
re-checks the code facts listed, asks the user, and records each answer in FINALITY.md as a
Zebra Crosslink requirement. It then updates the blocked stage here, or deletes the question
once the stage no longer depends on it.

### 1. What moves into zebra-state

FINALITY.md §4.3 puts "the Proof-of-Stake logic" in `zebra-state` and BFT certificate
processing in `NonFinalizedState`. It does not say whether the Tenderlink engine moves: its
networking, voting rounds, and the propose, validate, and decide closures. Nor does it name the
interface that remains between Tenderlink and zebra-state.

Code facts: `tenderlink/` is a top-level crate that `zebra-crosslink/zebra-state/Cargo.toml`
already depends on. `zebra-crosslink/zebra-crosslink/src` is about 4,250 lines. Tenderlink
awaits the decide closure before `start_round` calls the propose closure. zebra-state calls back
into the Crosslink service during `CrosslinkFinalizeBlock`, which is why the service lock must be
released before state requests (FINALITY.md §8.1).

To decide: which of validation, roster computation, BFT block storage, proposal construction,
and the consensus engine live in zebra-state; the request and response types at the boundary;
and whether `zebra-crosslink/zebra-crosslink` remains a crate.

### 2. Storage for the finalized chain off `bc_best`

FINALITY.md §4.3 and §8.1 require the chain to `bft_final_snapshot`, its BFT decisions, and its
bond state to survive a restart and a best chain that has pulled ahead. They do not say how.

Code facts: per-block stakes exist only in the finalized database's `aggregated_stakes_by_hash`.
A non-finalized `Chain` keeps its tip's bond state in `delegation_bonds`, plus per-block
`bond_rewards` for reorg reversal. BFT blocks live in `TFLServiceInternal::bft_blocks` and in
the PoS store file. The finalized database discards non-finalized chains that do not contain
its tip.

To decide: new column families in the existing RocksDB database or a separate store; whether
the BFT decisions replace the PoS store file; how bond state is held for a side chain without
copying the full chain state per block; and what is pruned once the side chain becomes
`bc_best` or is committed.

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
monotonicity, or the roster at `snapshot(B_{H−1})`; and whether FINALITY.md defines it as a
Zebra Crosslink validity rule. Stage 3's template selection keeps it as an extra condition.

### 5. `MAX_BLOCK_REORG_HEIGHT` 99 → 999

FINALITY.md §4.3 records 999 as the intended value; the tree has 99 in
`zcash_protocol::consensus`.

Code facts: the wallet's `REWIND_DISTANCE` and `CHECKPOINTS_N` derive from it. zebra-chain has a
separate constant of 1000, and comments at `zebra-state/src/request.rs` and
`non_finalized_state.rs` say 1000. The non-finalized state holds up to that many blocks per
chain in memory. The depth commit is the second floor under `fin` (FINALITY.md §8.1).

To decide: whether an implementation stage changes the constant, and whether that change comes
before or after stage 5.

### 6. Ordering with the payout work

Stage 4 changes where the roster comes from, and stage 6 moves bond tracking and reward
application into a new structure. The payout work changes `Chain::push`,
`update_bonds_with_pos_issuance`, `fixup_aggregated_stakes`, and the wallet projection
(FINALITY.md §5.4, §9.1).

To decide: which lands first, and who reconciles the bond update when stage 6 moves it.
