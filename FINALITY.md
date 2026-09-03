# Crosslink finality semantics and Zebra policy boundaries

This document separates the four protocol quantities used by the Crosslink 2 construction
from Zebra's irreversible state-commit boundary, legacy reorg-depth fallback, and
consumer-specific meanings of "final". It records the current implementation at repository
revision `8df720061d59aeedc486eea2559e7d19721f94be` and identifies decisions that must be made
before consensus behavior changes.

A companion visual explanation is in
[`FINALITY_DIAGRAM.html`](./FINALITY_DIAGRAM.html).

## 1. Scope and source maturity

Crosslink 2 is parameterized by a best-chain protocol `Π_bc` and a BFT protocol `Π_bft`; it is
not intrinsically a PoW/PoS protocol. This tree's Zebra prototype instantiates the best-chain
side with PoW and the BFT side with a stake-based protocol. The generic model below therefore
uses `bc` and `bft`; implementation observations use PoW and PoS/BFT.

The primary design source is the original TFL Book at pinned revision
[`daira/tfl-book@fe6e1d6`](https://github.com/daira/tfl-book/tree/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2).
The adaptation in `ShieldedLabs/zebra-crosslink` is useful implementation context, but
[describes itself as an almost-direct paste that may be incomplete, confusing, or
inconsistent](https://github.com/ShieldedLabs/zebra-crosslink/blob/6d02a1b80f896d08f923e39b2505f0565efb5787/book/src/design/cl2-construction.md#L3-L7)
and records Zebra-specific omissions such as Stalled Mode.

The pinned construction currently defines
[`candidate(H)` as written below](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/construction.md#L423-L438).
Its rationale nevertheless ends with a TODO to choose between that clamp and a stronger Last
Final Snapshot rule based on proof and latency results
([lines 510–517](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/construction.md#L510-L517)).
This document treats the formula as the current construction, not as a settled protocol
decision beyond that source revision.

The prototype sets `σ = 3` and `L = 7` in
`librustzcash/zcash_primitives/src/bft.rs` (`PROTOTYPE_PARAMETERS`). The inequality `7 ≥ 2 × 3`
meets the Book's stated minimum heuristic for `L`, but does not establish that `L` is
"significantly greater" than `σ`, or that either value is secure or performant. The source
code explicitly warns that these parameters have not been verified. `L` is not enforced by
the prototype.

### Notation

`prune_k(C)` means `C` with its last `k` blocks removed, with genesis as the floor. It is the
plain-text spelling of the Book's `C ⌈bc^k`. `A ⪯ B` means that `A` is an ancestor of or equal
to `B`; `A` and `B` conflict when neither is an ancestor of the other.

The two chains have their own parent links. They also contain two cross-chain references:

- each bc-block `H` has `H.context_bft`, which commits to a bft-block; and
- each non-genesis bft-block has `headers_bc`, exactly `σ` bc-headers in deepest-first order.

## 2. Terminology and layers

The construction has one fork-choice input, one objective intermediate quantity, and two
client views:

| quantity | definition | kind |
|---|---|---|
| `bc_best` / `χ` | highest-score bc-valid chain in the node's view | raw fork-choice view |
| `candidate(H)` | `lca(snapshot(LF(H)), prune_σ(H))` | objective function of a block and its ancestry |
| `fin` | monotone local state updated from `candidate(bc_best)` | locally finalized client view |
| `ba_μ` | `prune_μ(bc_best)` when it extends `fin`, otherwise `fin` | locally bounded-available client view |

Those four quantities do not exhaust the meanings carried by "final" in this tree. Zebra also
has:

- a **canonical-finalized policy point**, proposed here as `canonical_finalized_tip`, that
  makes only chains containing that point eligible for local activation;
- a **physical database-commit boundary**, proposed as `state_commit_tip`, which advances only
  after the finalized-state write has succeeded; and
- a **legacy reorg-depth marker**, roughly `tip − MAX_BLOCK_REORG_HEIGHT`, used when no
  Crosslink marker exists.

Protocol `fin` and these Zebra quantities must not share an undocumented storage slot. In raw
CL2, `fin` can remain fixed on a branch that raw `bc_best` no longer contains. In the current
Zebra implementation, irreversible state commitment instead enforces
`canonical_finalized_tip ⪯ canonical_tip` locally. That is an additional chain-activation and
state policy.

## 3. Crosslink 2 model

### 3.1 `snapshot`, `LF`, and `candidate`

```text
snapshot(B)  := O_bc                         if B.headers_bc = ∅
             := parent(B.headers_bc[0])      otherwise
LF(H)        := bft-last-final(H.context_bft)
candidate(H) := lca(snapshot(LF(H)), prune_σ(H))
```

The walk is `bc → bft → bft → bc`, followed by the last-common-ancestor clamp. In this
prototype every entry stored in `TFLServiceInternal::bft_blocks` is already decided, so
`bft-last-final` is currently the identity for stored entries. That storage shortcut is not a
protocol identity.

The clamp puts `candidate(H)` on `H`'s own chain and no later than `prune_σ(H)`. The Book says,
“This ensures that the candidate is at least σ‑confirmed”
([lines 510–517](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/construction.md#L510-L517)).
The source immediately identifies a possible alternative rule, so this rationale is evidence
for the current formula rather than evidence that the design choice is final.

### 3.2 `fin`: node-local monotone memory

When a node's bc-best-chain view changes, it runs the
[locally-finalized-chain update](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/construction.md#L471-L488):

```text
N := candidate(bc_best)
if fin ⪯ N:
    fin := N
else:
    keep fin
    if N conflicts with fin:
        record a finalization safety hazard
```

A candidate that moves behind `fin` during a reorg leaves `fin` unchanged and is not itself a
hazard; the Book gives that
[reorg case as the reason `fin` needs local state](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/construction.md#L497-L499).
A candidate on a conflicting fork also leaves `fin` unchanged and must produce the specified
hazard record, which carries `bc_best` and the `fin` history back to the last update that was
an ancestor of `N`. `fin` is therefore a node-local time series, not a pure function of the
current tip.

Assured Finality requires honest nodes' `fin` values at arbitrary times to be
prefix-compatible. It does not require those values to be equal at the same wall-clock time.

### 3.3 `ba_μ`: bounded availability during a finalization stall

The [bounded-available chain](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/construction.md#L520-L527)
is:

```text
ba_μ := prune_μ(bc_best)  if fin ⪯ prune_μ(bc_best)
     := fin               otherwise
```

Here `0 < μ ≤ σ`, with recommended default `μ = σ`; choosing a smaller value is
[at the node's own risk](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/construction.md#L393-L399)
and affects only `ba_μ`, not `fin`. The defining invariant is `fin ⪯ ba_μ`.

For `μ = σ`, the Book says the main application choice between `fin` and `ba_μ` is
[behavior during a finalization stall, not average latency](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/construction.md#L401-L415):
use `fin` to stop immediately, or use `ba_μ` to continue within the bounded window before
Stalled Mode constrains activity.

The fallback can occur in two ways:

- `prune_μ(bc_best)` is behind `fin` on the same chain; or
- `prune_μ(bc_best)` and `fin` are on different forks.

Neither condition alone proves that the full `bc_best` conflicts with `fin`.

### 3.4 Validity rules and honest production

In addition to inherited rules, the
[bc-block validity rules](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/construction.md#L686-L693)
are:

- **Valid context:** `H.context_bft` is bft-block-valid.
- **Extension:** `LF(parent(H)) ⪯bft LF(H)`.
- **Last Final Snapshot:** `snapshot(LF(H)) ⪯bc H`.
- **Finality depth:** `height(H) − height(snapshot(LF(H))) ≤ L`, unless `H` is a valid stalled
  block.

The separately stated
[bft-proposal and bft-block validity rules](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/construction.md#L568-L574)
add:

- **Linearity:** `snapshot(parent(B)) ⪯bc snapshot(B)`.
- **Tail Confirmation:** `B.headers_bc` form the `σ`-block tail of a bc-valid chain.

The rationale for the finality-depth rule states that “The finality depth must be objectively
defined” and therefore measures `H` against `snapshot(LF(H))`, an objective function of `H`,
rather than against node-local `fin`
([lines 695–698](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/construction.md#L695-L698)).

Beyond satisfying validity and stalled-block rules, the explicit BFT-context selection
procedure chooses `H.context_bft`: among eligible bft-valid tips it chooses a longest chain,
then breaks ties by final-snapshot score and hash
([lines 705–716](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/construction.md#L705-L716)).
BFT-derived data therefore affects more than this selection procedure: it also affects block
validity, finality depth, and whether an honest producer must emit a stalled block.

### 3.5 Prefix Consistency and client exposure

Prefix Consistency at depth `σ` is a property assumed of qualifying executions of the
best-chain protocol:

```text
prune_σ(χ_i^t) ⪯ χ_j^u    for honest observations at t ≤ u
```

It is not a Crosslink checkpoint rule and is not enforced by `fin`. If a later best chain
displaces an earlier `σ`-confirmed prefix, an argument that assumes Prefix Consistency no
longer applies to that execution.

The Book [recommends baking in a BFT checkpoint and gating client
exposure](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/construction.md#L557-L562)
until the checkpoint precedes `LF(bc_best)`, its snapshot precedes `fin`, and `fin` is recent.
This is an unimplemented sync-safety recommendation, not a block-validity or consensus rule.

## 4. Raw fork choice and finalized-prefix policy

### 4.1 What raw CL2 selects

Raw CL2 leaves the underlying rule in place: choose a highest-score bc-valid chain. In the
Zebra instantiation, score is accumulated PoW. Crosslink constrains individual blocks through
the validity rules carried by their own BFT context; it does not require raw `bc_best` to
contain the observer's current `fin`.

The evidence chain is:

1. The generic best-chain model chooses a highest-score bc-valid chain
   ([construction lines 245–260](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/construction.md#L245-L260)).
2. The CL2 validity rules constrain a block relative to the BFT context that block carries
   ([lines 686–693](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/construction.md#L686-L693)).
   A chain can retain an older, still-valid BFT context.
3. Honest bc-block production says producers “must not use information from the BFT protocol”
   beyond the specified consensus rules when selecting a bc-valid chain
   ([lines 705–716](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/construction.md#L705-L716)).
4. The Questions chapter analyzes the stronger rule requiring `bc_best` to extend the latest
   final BFT snapshot in the node's view and says it breaks the current safety and liveness
   arguments
   ([lines 11–26](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/questions.md#L11-L26)).

Its exact conclusion is:

> “Probably not. I don’t know how to repair the safety and liveness arguments.”
>
> — [Questions about Crosslink, lines 44–52](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/questions.md#L44-L52)

The Questions page is partly historical: its heading predates the current Last Final Snapshot
block-validity rule. Its discussion still distinguishes that per-block rule from the stronger,
unadopted fork-choice constraint. A block can satisfy `snapshot(LF(H)) ⪯ H` using stale BFT
context; the stronger rule would constrain the selected best chain by the newest final BFT
snapshot in the observer's view.

### 4.2 The additional Zebra policy

A finalized-prefix eligibility rule would be:

```text
eligible_i(C, t) := bc_valid(C) and local_finalized_tip_i^t ⪯ C
bc_best_i^t      := highest_score({ C | eligible_i(C, t) })
```

This rule preserves the finalized prefix for each node that enforces it. It does not by itself
prove global agreement, network progress, or Prefix Consistency for unfinalized blocks. If an
enforcing node has no eligible progressing chain, local liveness must yield.

Current Zebra's `CrosslinkFinalizeBlock` behavior is stronger still: it commits database state
on the named branch and discards incompatible non-finalized branches. This makes the policy
physical. It may be a deliberate Zebra choice, but it is not mandated by the CL2 construction
and must be specified and analyzed separately.

## 5. Zebra implementation inventory

This inventory refers to symbols at analyzed revision
`8df720061d59aeedc486eea2559e7d19721f94be`; symbol names are preferred over brittle working
tree line numbers.

### 5.1 The overloaded marker and write paths

`zebra-crosslink/zebra-crosslink/src/lib.rs` defines
`TFLServiceInternal::latest_final_block: Option<(ZebBlockHeight, ZebBlockHash)>`. It is assigned
by:

- `handle_new_decided_bft_block`, after inserting the BFT block;
- `tfl_service_main_loop`, when restoring the last entry from the PoS store; and
- `tfl_set_finality_by_hash`, through the testing/service setter.

The live path computes the hash of `new_block.headers[0]`: the first header itself, rather than
`snapshot(new_block) = parent(new_block.headers[0])`.

When this marker is absent, `tfl_final_block_height_hash_pre_locked` substitutes
`tfl_reorg_final_block_height_hash`, a Zebra reorg-depth location derived from the state block
locator. The API therefore changes semantics depending on whether Crosslink has produced a
value.

`TFLServiceInternal::current_bc_final` is initialized in
`zebra-crosslink/zebra-crosslink/src/service.rs` and assigned during PoS-store startup in
`tfl_service_main_loop`. No other read or write was found. It is currently unused duplicate
state.

### 5.2 Irreversible commitment and ordering

`handle_new_decided_bft_block` assigns `latest_final_block` before it sends
`zebra_state::Request::CrosslinkFinalizeBlock`. The request is retried indefinitely after an
error. During that interval, RPC and GUI readers can observe a marker whose database state has
not been finalized.

The state behavior depends on whether the hash is known:

- `new_network` accepts a hash found in any non-finalized chain or in the finalized database.
  `NonFinalizedState::crosslink_finalize` retains the chain containing a known side-chain hash,
  so finalizing that hash can make the side chain canonical before blocks are committed by
  `WriteBlockWorkerTask::handle_crosslink_finalize`.
- an unknown hash produces an error; the Crosslink caller then keeps retrying and can remain in
  that loop indefinitely.

Consequently, the stored marker is neither a reliable `fin` implementation nor a reliable
`state_commit_tip`. A physical commit marker must advance only after the state request
succeeds, while publication of protocol `fin` must follow the CL2 update rule.

### 5.3 Consumers

The overloaded value currently reaches:

- irreversible state commitment through `CrosslinkFinalizeBlock`;
- `finalizers_at_current_height`, using the aggregate stakes returned by state finalization;
- hardfork finalizer filtering through `terminated_finalizers_at`;
- `get_tfl_final_block_*`, block-finality, and transaction-finality RPC methods;
- the GUI's finalized row, terminated-finalizer display, and visualization paging lower bound;
- BFT proposal and validation paths; and
- the main-loop finality-gap diagnostic.

`TFLServiceInternal::final_change_tx` is created and
`TFLServiceRequest::FinalBlockRx` returns subscribers. The RPC notification methods wait on
those receivers, but no `final_change_tx.send(...)` site exists in the analyzed tree. This
surface is incomplete: a waiter can remain blocked even when the marker changes.

### 5.4 Current staking rewards

At the end of `Chain::push` in
`zebra-crosslink/zebra-state/src/service/non_finalized_state/chain.rs`:

- if no bond is active, the code pushes an empty `bond_rewards` entry and mints no staking
  reward; and
- otherwise it distributes the fixed `POS_BLOCK_REWARD_ZATS` for that PoW block, increases
  `staking_bonded_amount` by the same total, and records the per-bond rewards for exact reorg
  reversal.

`update_bonds_with_pos_issuance` in `zebra-crosslink/zebra-state/src/service.rs` allocates the
total pro rata with integer division, gives the remainder to the largest active bond (then
smallest key on a tie), and adds rewards to bond principal. Rewards therefore compound.

The same per-block calculation is replayed by `fixup_aggregated_stakes` in
`zebra-crosslink/zebra-state/src/service/stake_fixup.rs` (reached through the `--fixup-db-stake`
entry point) and by the wallet projection path in `zebra-crosslink/zebra-crosslink/src/lib.rs`.
Any future consensus change must keep all three paths identical.

## 6. Divergences and hazards by category

### 6.1 Derivation and update trigger

- **Wrong trigger and input.** Protocol `fin` is updated from `candidate(bc_best)` whenever the
  best-chain view changes. Zebra updates `latest_final_block` when a BFT block is decided or
  restored, without requiring the current best chain to cite that decision.
- **Missing clamp.** Zebra does not compute
  `lca(snapshot(LF(H)), prune_σ(H))`; it takes a hash directly from the decided BFT block.
- **Off-by-one snapshot.** Honest proposal construction obtains a deepest-first `σ`-header
  tail. `headers[0]` is one block after the snapshot, but Zebra stores that header's hash rather
  than its parent. The stale `BftBlock` doc comment in
  `librustzcash/zcash_primitives/src/bft.rs` incorrectly says the in-memory order is reversed
  from the specification.
- **Missing monotonicity and hazard record.** All marker writes are unconditional. There is no
  `fin ⪯ candidate` guard and no distinction between a benign candidate regression and a
  conflicting-candidate safety incident.

### 6.2 Missing validity rules

- The Last Final Snapshot rule is not implemented for bc-block admission.
- The Finality Depth rule and Stalled Mode are not implemented. `L` is serialized only by test
  formatting; the 512-block log threshold is diagnostic, not consensus.
- BFT validation does not implement Linearity or Tail Confirmation. It checks that the first
  carried header's block is locally present, but does not establish that all `σ` headers form a
  valid chain with valid PoW.
- The Extension rule is implemented by
  `call_from_state_to_crosslink_to_ask_about_fat_pointers`, including its defer/reject
  distinction.

### 6.3 State-finalization and fork-choice policy

`CrosslinkFinalizeBlock` collapses non-finalized state onto a known named branch. This locally
enforces a finalized-prefix activation policy and prevents a higher-score conflicting chain
from becoming canonical. Raw CL2 does not impose that rule. The existing test
`crosslink_pow_switch_to_finalized_chain_fork_even_though_longer_chain_exists` documents the
prototype behavior.

The documentation and implementation must separately name:

- protocol `local_finalized_tip` (`fin`);
- the Zebra policy floor `canonical_finalized_tip`; and
- the successfully persisted `state_commit_tip`.

### 6.4 Bounded availability and Stalled Mode

`ba_μ` does not exist in the tree. The fallback branch, bounded behavior during a finalization
stall, and the consensus restrictions of Stalled Mode are therefore absent. The diagnostic
warning at a hardcoded gap does not substitute for them.

### 6.5 Client exposure and API semantics

There is no checkpoint/recency sync gate. Before the Crosslink marker exists, finality RPCs
silently expose the legacy reorg-depth fallback. Existing GUI and RPC surfaces also conflate
raw tip, confirmation, and finalization instead of defining each endpoint's contract.

### 6.6 Ordering and notification

- The visible marker advances before irreversible state commitment succeeds.
- A known side-chain hash can change the canonical branch; an unknown hash can retry forever.
- `current_bc_final` is unused duplicate state.
- `FinalBlockRx` has subscribers but no publisher send site.

## 7. Proposed names and consumer decision matrix

The protocol names should encode their definitions:

| protocol quantity | value identifier | optional newtype |
|---|---|---|
| `bc_best` | `bc_best_tip` | `BcBestTip` |
| `candidate(H)` | `finalization_candidate` | `FinalizationCandidate` |
| `fin` | `local_finalized_tip` | `LocalFinalizedTip` |
| `ba_μ` | `bounded_available_tip` | `BoundedAvailableTip` |

Policy and persistence need separate names such as `canonical_finalized_tip` and
`state_commit_tip`. The legacy reorg-depth value should keep a name that says it is a
reorg-depth marker, not Crosslink finality.

No blanket "presentation uses `ba_μ`" rule is correct. Each consumer needs a contract:

| consumer or endpoint | value | contract or unresolved work |
|---|---|---|
| raw best-tip display | `bc_best_tip` | current fork-choice result |
| confirmed/bounded-available display | `bounded_available_tip` | bounded behavior during finalization stalls |
| final display | `local_finalized_tip` | node-local monotone CL2 view |
| `get_tfl_final_block_hash` and `get_tfl_final_block_height_and_hash` | `local_finalized_tip` | return no value until actual `fin` exists and exposure gating passes |
| block/transaction status | unresolved API contract | define distinct `Confirmed` and `Finalized` states before routing either |
| finality-change notifications | `local_finalized_tip` transitions | publish only after the chosen public-finality contract is met |
| visualization paging | operational paging cursor | do not overload a finality value merely to bound a window |
| canonical state activation | `canonical_finalized_tip` policy | separate Zebra decision; not an ordinary consumer of protocol `fin` |
| physical database status | `state_commit_tip` | advance after successful state commit |
| staking rewards | objective per-block source | never use node-local `fin`; see §9.2 |

### Consensus-sensitive roster and hardfork inputs

The validator roster, voting power, and hardfork-driven membership changes are
consensus-sensitive. They must not read node-local `fin` unless there is a proof that every
validator derives the same value at the same BFT height. Prefix compatibility between honest
`fin` values is insufficient.

This remains an open design question. Candidate objective sources include `snapshot(B)` for an
agreed BFT block, `snapshot(LF(H))`, or `candidate(H)`, but each choice needs a precise rule and
proof. The design must state separately:

- which quantity selects canonical ledger state; and
- which objective quantity selects the validator set for a given BFT height.

## 8. Minimal code slice after the decisions

The first implementation patch should expose the semantic split without claiming that legacy
writers already implement CL2:

1. Rename `latest_final_block` to `local_finalized_tip` and document that it is the storage
   slot intended for protocol `fin`, currently fed by legacy logic.
2. Rename the main-loop `current_bc_tip` local to `bc_best_tip`.
3. Make the final-block accessor return only the stored Crosslink value; do not substitute the
   legacy reorg-depth marker.
4. Add a distinct successful-commit marker if callers need to report database finalization;
   update it only after `CrosslinkFinalizeBlock` succeeds.
5. Either publish `FinalBlockRx` changes at the documented transition point or remove the dead
   notification API in separate code work.

The accessor change is observable: before Crosslink produces a local finalized value, finality
queries should return `None`, not label a Zebra reorg-depth point as Crosslink finality. A
regression test should cover both the absent and explicitly present cases.

Computing `candidate`, changing the update trigger, enforcing validity rules, implementing
Stalled Mode, and changing chain eligibility or rewards are later behavior changes, not part of
a semantic rename.

## 9. Open consensus decisions

### 9.1 Canonical state and fork choice

Choose explicitly between raw CL2 fork choice and Zebra's additional finalized-prefix
chain-activation policy. Raw CL2 permits `fin` to remain off `bc_best`, while the finalized
client view stays fixed. The current physical state model requires
`canonical_finalized_tip ⪯ canonical_tip` locally. Removing or retaining that requirement has
liveness, recovery, storage, and migration consequences.

### 9.2 Objective reward trigger and reward economics

Consensus issuance cannot depend on node-local `fin`. Honest nodes can reach the same chain
through different best-chain and reorg histories, so they need not observe the same sequence
of `fin` transitions. They must nevertheless compute identical value pools for the same
chain.

An objective per-block event can instead be derived from block data, for example:

```text
payout boundary at H  iff  candidate(H) != candidate(parent(H))
```

This is not literally the event "local `fin` advanced." It is a block-local event that would
permit `fin` to advance if `H` were observed as best and its candidate were ahead of that
node's current `fin`. `snapshot(LF(H))` is another objective candidate source. The selected
function must be monotone under the enforced validity rules; today the missing Linearity and
Last Final Snapshot checks leave that premise unenforced.

Payout amount is a separate decision:

- A flat `POS_BLOCK_REWARD_ZATS` per objective advance lowers issuance during a BFT stall and
  can leave it permanently lower if advances never resume.
- A deferred amount based on elapsed PoW height can catch up only when a later payout occurs
  and only under an explicit accrual rule. Issuance is still lower at intermediate heights,
  remains lower after a permanent stall, and intervals with no active bonds need a rule: drop,
  burn, or carry their nominal reward.
- Current rewards increase bond principal every rewarded block. A lump delays that
  compounding and allocates the whole amount among bonds active at payout time. Individual
  allocations therefore change, even if aggregate eventual base issuance is preserved under
  stated assumptions.
- Non-payout blocks must still append empty `bond_rewards` entries so positional reorg reversal
  remains aligned.

The choice of objective trigger does not depend on resolving the amount formula. Conversely,
the amount decision must not obscure the already-settled requirement that a consensus trigger
be replayable from the chain alone. Detailed reward economics should live in a separate
decision document once a concrete policy is proposed.

### 9.3 Validator-set derivation

Specify the objective input for the roster, voting power, and hardfork membership at each BFT
height, and prove that validators at that height derive the same set. Also specify separately
which ledger state is read to materialize that set.

### 9.4 Remaining protocol choices

- Decide whether and how to implement the Last Final Snapshot, Finality Depth, Linearity, and
  Tail Confirmation rules. These are consensus changes in the current prototype.
- Decide how Stalled Mode behaves for Zebra transactions and block production.
- Decide whether the one-block snapshot shift requires PoS-store migration or replay rules.
- Choose `μ` and whether it is node-configurable; default `μ = σ` follows the Book's
  recommendation.
- Specify checkpoint/recency exposure gating independently of block validity.

## 10. Source appendix

- [Original TFL Book: `candidate`, `fin`, `ba_μ`, and syncing](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/construction.md#L423-L562)
- [Original TFL Book: BFT validity rules](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/construction.md#L568-L574)
- [Original TFL Book: bc validity and honest production](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/construction.md#L680-L717)
- [Original TFL Book: fork-choice question](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/questions.md#L11-L52)
- [Shielded Labs warning about the adapted construction](https://github.com/ShieldedLabs/zebra-crosslink/blob/6d02a1b80f896d08f923e39b2505f0565efb5787/book/src/design/cl2-construction.md#L1-L14).
  Protocol definitions above are cited separately from the original pinned source.

All implementation observations in §§5–6 are based on the analyzed monolith revision named at
the start of §5. Re-check the cited symbols before using this document to plan changes on a
newer revision.
