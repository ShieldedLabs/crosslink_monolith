# Crosslink finality semantics and Zebra policy boundaries

This document separates the three Crosslink 2 protocol quantities that Zebra's design retains
from Zebra's irreversible state-commit boundary, legacy reorg-depth fallback, and
consumer-specific meanings of "final".

It keeps three layers apart:

- **Book**: Crosslink 2 as the pinned TFL Book specifies it (§1).
- **Zebra Crosslink**: the behavior this tree implements. It is the Book's construction without
  Stalled Mode, with sticky fork choice, persisted `fin`, and every remaining CL2 validity rule.
  Statements in this layer are requirements, including where the code does not meet them yet.
- **Current tree**: the code at this revision of the repository. Statements in this layer
  describe code that the implementation changes, and are not requirements.

| section | layer |
|---|---|
| §1 Scope | Book, with Zebra Crosslink's parameter and Stalled Mode choices |
| §2 Terminology | Zebra Crosslink; the Zebra-specific quantities note their current-tree form |
| §3 Crosslink 2 model | Book, with the consequences for Zebra Crosslink |
| §4.1 Raw fork choice | Book |
| §4.2 Finalized-prefix policy | a general policy, and its current-tree form |
| §4.3 Sticky fork choice | Zebra Crosslink, ending with the current tree |
| §5 Implementation inventory | current tree |
| §6 Divergences | current tree, measured against Zebra Crosslink |
| §7 Names and consumer contracts | Zebra Crosslink |
| §8 Implementation status and pitfalls | current-tree facts that constrain the implementation |
| §9 Open decisions | outside this document's implementation work |

Where a section mixes layers, a paragraph opens with its layer in bold. The ordered
implementation work, and the questions that still need a design pass, are in
[`IMPLEMENTATION.md`](./IMPLEMENTATION.md).

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

Zebra Crosslink omits Stalled Mode. The Book builds bounded availability from three parts: the
finalization gap bound `L`, the Finality Depth rule with its stalled-block exception, and the
bounded-available client view `ba_μ` with its confirmation depth `μ`. This design has none of
them; §3.3 derives why omitting Stalled Mode removes the other two, and what the remaining
definitions guarantee.

**Current tree.** The prototype sets `σ = 4` in `librustzcash/zcash_primitives/src/bft.rs`
(`PROTOTYPE_PARAMETERS`). The source code explicitly warns that this value has not been
verified as secure or performant. The Book's `L` is not a parameter of this design: it is absent
from `ZcashCrosslinkParameters`, from the node configuration, and from the test format, whose
parameter instruction writes a zero in its place.

### Notation

`prune_k(C)` means `C` with its last `k` blocks removed, with genesis as the floor. It is the
plain-text spelling of the Book's `C ⌈bc^k`. `A ⪯ B` means that `A` is an ancestor of or equal
to `B`; `A` and `B` conflict when neither is an ancestor of the other.

The two chains have their own parent links. They also contain two cross-chain references:

- each bc-block `H` has `H.context_bft`, which commits to a bft-block; and
- each non-genesis bft-block has `headers_bc`, exactly `σ` bc-headers in deepest-first order;
  the block it finalizes is the parent of the first, named by that header's parent hash (§6.1).

## 2. Terminology and layers

The construction as adopted has one fork-choice input, one objective intermediate quantity, and
one client view:

| quantity | definition | kind |
|---|---|---|
| `bc_best` / `χ` | highest-score bc-valid chain in the node's view | raw fork-choice view |
| `candidate(H)` | `lca(snapshot(LF(H)), prune_σ(H))` | objective function of a block and its ancestry |
| `fin` | monotone local state updated from `candidate(bc_best)` | locally finalized client view |

The Book's fourth quantity, the bounded-available chain `ba_μ`, is not part of this design
(§3.3). No CL2 quantity lies between `bc_best` and `fin`.

Those three quantities do not exhaust the meanings carried by "final" in this tree. Zebra also
has:

- a **canonical-finalized policy point**, `canonical_finalized_tip`, that makes only chains
  containing that point eligible for local activation;
- a **physical database-commit boundary**, the finalized database's tip, which advances only
  after the finalized-state write has succeeded; and
- a **legacy reorg-depth marker**, roughly `tip − MAX_BLOCK_REORG_HEIGHT`. It has no consumer
  in the current tree, whose finality RPCs no longer substitute it when no Crosslink marker
  exists. It is listed here so that it is not reintroduced under a Crosslink name.

Protocol `fin` and these Zebra quantities must not share an undocumented storage slot. In raw
CL2, `fin` can remain fixed on a branch that raw `bc_best` no longer contains. A
finalized-prefix policy instead enforces `canonical_finalized_tip ⪯ canonical_tip` locally,
which is an additional chain-activation and state policy (§4.2).

**Current tree.** `CrosslinkFinalizeBlock` enforces that policy with a floor taken from each BFT
decision as it is decided (§4.2, §5.2).

**Zebra Crosslink.** Under sticky fork choice (§4.3) the policy floor is `fin` itself, so
`canonical_finalized_tip` and `fin` are one quantity. Two stored values remain: `fin`, persisted in the finalized database
as its own block hash, and the database's finalized tip, which is the higher of `fin` and the
block Zebra commits at reorg depth. The finalized tip equals `fin` while finality lags the
best tip by less than about `MAX_BLOCK_REORG_HEIGHT` blocks. Past that lag the depth commit
runs ahead of `fin`, and if it comes to lie on a branch that excludes `bft_final_snapshot`, the
node opens a second chain state of the same shape for the BFT branch (§4.3, §7.1). `fin` is one
quantity across both: every branch the node records contains it.

A fourth quantity is objective rather than node-local:

| quantity | definition | kind |
|---|---|---|
| `bft_final_snapshot` | `snapshot(B)` for the newest decided bft-block `B` in the node's view | the bc-block `Π_bft` has most recently finalized |

A BFT decision finalizes `bft_final_snapshot` in the sense of `Π_bft`, and "Crosslink finalized"
in conversation usually means this point. A node's `fin` reaches it only through the node's own
best chain, by the update rule of §3.2. Under sticky fork choice, and after each update:

```text
candidate(bc_best) ⪯ fin ⪯ bc_best
fin ⪯ bft_final_snapshot                       (under Linearity and Π_bft Final Agreement)
```

`fin` equals `candidate(bc_best)` except after a reorganization that moved the candidate back.
`bft_final_snapshot` need not be on `bc_best`, and can stay off it for any length of time: it is
on a chain the node switches to only when that chain has more work (§4.3). The first line holds
exactly under fork-choice rules that keep `fin ⪯ bc_best`; under raw work-based fork choice both
of its relations can fail.

## 3. Crosslink 2 model

### 3.1 `snapshot`, `LF`, and `candidate`

```text
snapshot(B)  := O_bc                         if B.headers_bc = ∅
             := parent(B.headers_bc[0])      otherwise
LF(H)        := bft-last-final(H.context_bft)
candidate(H) := lca(snapshot(LF(H)), prune_σ(H))
```

The walk is `bc → bft → bft → bc`, followed by the last-common-ancestor clamp.

`bft-last-final(B)` is the last final ancestor of `B`, `B` included. In Zebra, in the current
tree and in Zebra Crosslink alike, `Π_bft` decides
each bft-block individually, and a decided block is final. A bc-block's `context_bft` is a fat
pointer, and a node resolves it only against the node's own store of decided bft-blocks; a
pointer that does not resolve defers the bc-block (§6.2, Extension). Every context a node accepts
is therefore final, and `bft-last-final` is the identity on them, so `LF(H)` is the bft-block
that `H.context_bft` points at. Zebra Crosslink keeps that store in `zebra-state` beside the
chain it is resolved against (§7.1); in the current tree it is
`BftChain::blocks` (§5.5).

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

The Book's
[Local fin-depth lemma](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/construction.md#L490-L495)
bounds where that series can be:

```text
for node i honest at time t, there is a time r ≤ t with fin_i^t ⪯ prune_σ(bc_best_i^r)
```

Take `r` as the last time `fin` changed, or genesis if it never has. At `r > 0`, `fin` was set
to `candidate(bc_best^r)`, and `candidate(H) ⪯ prune_σ(H)` because an lca is an ancestor of
both of its arguments. At genesis both sides are `O_bc`, since pruning `O_bc` yields `O_bc`. The
Book combines the lemma with `Π_bc` Prefix Agreement at depth `σ` to argue Assured Finality; that
use is why `candidate` clamps to `prune_σ(H)` rather than using `snapshot(LF(H))` alone
([line 513](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/construction.md#L513)).

**When the `prune_σ` clamp binds.** When the `σ` headers of `LF(H)` are all ancestors of `H`,
the last of them is at or below `parent(H)`, so `snapshot(LF(H))` is at least `σ + 1` blocks
below `H` and the clamp does not bind. It binds when the headers lie on another chain that extends past `H`'s own chain: for
example a short side chain whose blocks sit just above `snapshot(B)` and cite a bft-block `B`
whose `σ` headers continue on a different, longer chain. Last Final Snapshot admits such a block,
since `snapshot(B)` is its ancestor. Without the clamp, a node whose best chain is that side
chain would finalize `snapshot(B)` while its own chain buried that block only one or two deep.

That matters only when `Π_bft` is subverted. A subverted `Π_bft` can decide a bft-block whose
headers come from any chain with valid PoW, including one the adversary mined privately, and so
can name as its snapshot a block that an honest node has seen only shallowly on a branch about to
be abandoned. With the clamp, a node finalizes a block only once its own best chain has buried it
`σ` deep, so under `Π_bc` Prefix Consistency every honest best chain keeps that block and honest
`fin` values stay compatible without any assumption about `Π_bft`. Without it, a subverted
`Π_bft` alone could give honest nodes conflicting `fin` values.

Assured Finality requires honest nodes' `fin` values at arbitrary times to be
prefix-compatible. It does not require those values to be equal at the same wall-clock time.

### 3.3 No bounded availability

The Book's bounded availability is one mechanism in three parts:

- the [Finality Depth rule](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/construction.md#L691-L699)
  admits a bc-block `H` with `height(H) − height(snapshot(LF(H))) > L` only if `H` is a stalled
  block;
- [Stalled Mode](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/construction.md#L401-L415)
  defines stalled blocks (proposed for Zcash as coinbase-only), so that `Π_bc` keeps producing
  blocks while no user transaction lands more than `L` blocks past the snapshot; and
- the [bounded-available chain](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/construction.md#L520-L554)
  `ba_μ := prune_μ(bc_best) if fin ⪯ prune_μ(bc_best), else fin` is the client view whose
  distance ahead of `fin` that bound limits.

Without Stalled Mode, the other two parts have no role:

1. **Finality Depth.** With no stalled blocks the rule would reduce to
   `height(H) − height(snapshot(LF(H))) ≤ L`. During a BFT stall no context can lower that
   depth, so `Π_bc` would halt `L` blocks past the snapshot. That is still
   bounded availability, in its strictest form. The Book also rejects it as a design: it calls
   stopping the chain
   [a naive approach with serious security problems under PoW](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/the-arguments-for-bounded-availability-and-finality-overrides.md#L19-L24),
   and its liveness analysis says any loss of `Π_bc` liveness
   [would be a bug because it allows tail-thrashing attacks](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/security-analysis.md#L7-L13).
   `L` and `is_stalled_block` have no other use.
2. **`ba_μ` and `μ`.** `ba_μ` differs from a plain confirmation depth only in its
   fallback to `fin`, and that fallback exists to keep `fin ⪯ ba_μ`. Without a bound there is
   nothing for that view to bound.

The same liveness analysis states that the Finality Depth rule
[is technically independent of the rest of Crosslink 2](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/security-analysis.md#L9-L11):
without it, the protocol keeps its advantages over Snap-and-Chat, but the incentive to pull the
finalization point forward is weaker. The concrete consequences for the remaining definitions
are:

- **`candidate(bc_best)` can lag without any validity cost.** The Extension rule permits
  `LF(H) = LF(parent(H))`, and the Valid Context and Last Final Snapshot rules are always
  satisfiable by reusing the parent's `context_bft`. With no depth bound, a chain that never
  updates its context stays valid at any height. Progress of `fin` while `Π_bft` is live
  therefore depends on bc-block producers following the honest context-selection procedure
  (§3.4), which is not a validity rule. A producer with enough hash rate to dominate `bc_best`
  can withhold finality progress; under bounded availability it would have been confined to
  stalled blocks after `L`.
- **The finality gap is unbounded.** During a finalization stall, `bc_best` keeps accepting
  ordinary spending transactions at any distance past `fin`. The rollback exposure of those
  transactions grows with the gap. This is the outcome the Book's bounded-availability argument
  was written to avoid; omitting Stalled Mode accepts it.
- **No client view is both available and guaranteed to extend `fin`.** An application that
  does not want to stop with finality reads `bc_best` or a confirmation prefix
  `prune_k(bc_best)`. Those are `Π_bc` views, not CL2 quantities, and their security is
  `Π_bc`'s own Prefix Consistency. The Book's "Prefix Consistency of `ba`" theorem has no
  subject. A prefix `prune_k(bc_best)` can be an ancestor of `fin` even when every assumption
  holds; the Book gives difficulty adjustment after a reorg as the reason, which is why `ba_μ`
  had its fallback. It can conflict with `fin` only if Prefix Consistency at `σ` has failed:
  `fin ⪯ prune_σ(χ^r)` for some earlier `r`, so Prefix Consistency at `σ` gives `fin ⪯ bc_best`,
  and every prefix of `bc_best` is then comparable with `fin`.
- **The application choice changes.** The Book framed it as `fin` (stop immediately) versus
  `ba_μ` (continue for at most `L` blocks). Here it is `fin` versus a `bc_best` view that never
  stops and has no bound on how much can be rolled back to `fin`.

In the current tree, the collapse onto each decided block (§4.2) locally forces
`canonical_finalized_tip ⪯ canonical_tip`; in Zebra Crosslink, sticky fork choice (§4.3) keeps
`fin ⪯ bc_best`. Either restores, as a chain-selection policy, a prefix relation that `ba_μ` provided by
definition. Neither limits the finality gap, and both cost local liveness whenever the dominant
chain excludes the floor (§4.3).

### 3.4 Validity rules and honest production

In addition to inherited rules, the
[bc-block validity rules](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/construction.md#L686-L693)
are:

- **Valid context:** `H.context_bft` is bft-block-valid.
- **Extension:** `LF(parent(H)) ⪯bft LF(H)`.
- **Last Final Snapshot:** `snapshot(LF(H)) ⪯bc H`.

The Book's fourth rule, **Finality Depth**, is omitted with Stalled Mode (§3.3).

The separately stated
[bft-proposal and bft-block validity rules](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/construction.md#L568-L574)
add:

- **Linearity:** `snapshot(parent(B)) ⪯bc snapshot(B)`.
- **Tail Confirmation:** `B.headers_bc` form the `σ`-block tail of a bc-valid chain.

**Zebra Crosslink** enforces all five rules above, and so does the **current tree** (§6.2).

Tail Confirmation is objective: `σ` consecutive headers ending at a bc-valid block are the tail
of the chain that ends at that block, whatever the validator's own best chain. The Book
separately defines what an honest proposer puts in that field.

**Honest proposal.** An
[honest proposer](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/construction.md#L625-L633)
of a bft-proposal `P`:

- sets `P.headers_bc` to the `σ`-block tail of its own `bc_best`, if that satisfies Linearity
  against `P`'s parent;
- otherwise sets `P.headers_bc` to its parent's `headers_bc`, repeating the parent's snapshot;
  and
- makes no proposals until its `bc_best` is at least `σ + 1` blocks long.

A proposal is therefore always possible once the chain is long enough. The Linearity rationale
[depends on that](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/construction.md#L610-L614):
liveness of the underlying BFT protocol can require honest proposers to propose at a minimum
rate. Honest proposal is a behavior, not a validity rule: a validator cannot tell whether the
carried tail was the proposer's best chain. An
[honest validator](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/construction.md#L639-L643)
first downloads the bc-blocks for `P.headers_bc` and checks their bc-block validity.

**Zebra Crosslink.** A proposer clamps its candidate height to at most 40 blocks above the
previous final snapshot. When the clamp binds, `P.headers_bc` is a window of `bc_best` ending
below its tip rather than its tail, which breaks honest proposal. The clamp is a design
heuristic, not part of the Crosslink 2 specification. The window still satisfies Tail
Confirmation.

**Linearity and bc reorganizations.** Let `B` be the newest final bft-block. Linearity requires
every later final snapshot to extend `snapshot(B)`. When a node's `bc_best` reorganizes onto a
branch that forks below `snapshot(B)`, the tail of that branch fails Linearity, so honest
proposers repeat `B.headers_bc`. Last Final Snapshot admits a block `H` on that branch only if
`snapshot(LF(H))` lies on the branch, so `H` cannot cite `B` or any later final bft-block, and
`candidate(H)` stays at or below the fork point. Finality for nodes on that branch resumes when
a chain containing `snapshot(B)` becomes their best chain again. Under honest proposal at every
bc-block, `snapshot(B)` sits about `σ` blocks below the proposer's tip, so a reorganization
slightly deeper than `σ` reaches this case.

**Finality lag under honest production.** This follows from the Book's honest proposal and
applies to Zebra Crosslink. A proposer at tip `T` carries headers `T − σ + 1`
through `T`, so the decided block's snapshot is `T − σ`. The first bc-block that can cite that
decision is `T + 1`, and only if its template was built after the decision arrived; then
`candidate(T + 1) = T − σ`. In steady state `fin` therefore trails the best tip by at least
`σ + 1` blocks. Every bc-block built from a template that predates the latest decision cites an
older bft-block and adds one more block of lag, and a decision that takes longer than a bc-block
interval adds more.

The Book's informal safety argument uses Linearity and Last Final Snapshot as follows:

- **Linearity with `Π_bft` Final Agreement** makes the snapshots of final bft-blocks
  bc-linear, which the Book says implies Assured Finality without any `Π_bc` safety assumption
  ([lines 587–594](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/construction.md#L587-L594)).
- **Last Final Snapshot with the `σ` carried headers** is the basis of the other half of that
  sketch: each candidate final snapshot is a `σ`-confirmed prefix of the observer's best
  chain, so `Π_bc` Prefix Agreement gives safety without any `Π_bft` assumption (same lines).
- **The two together** remove the sanitization of ledgers that Snap-and-Chat and Crosslink 1
  needed ([potential changes, lines 320–371](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/potential-changes.md#L320-L371)). That
  section's security analysis starts from the observation that neither rule affects the
  evolution of `Π_bc` unless its Prefix Consistency or Prefix Agreement would be violated, and
  breaks off mid-sentence.

The Book's safety section is
[marked as not updated for Crosslink 2](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/security-analysis.md#L52-L54), and
[a later edit](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/security-analysis.md#L281) says Linearity so far contributes to security
only heuristically. These arguments are sketches, not proofs.

Two facts used in §4.3 follow from the definitions:

- `candidate(H) ⪯ snapshot(LF(H))` and `candidate(H) ⪯ prune_σ(H)` hold for every `H`,
  because an lca is an ancestor of both of its arguments.
- With Last Final Snapshot, `snapshot(LF(H))` and `prune_σ(H)` both lie on `H`, so
  `candidate(H)` is the lower of the two. Without it, `snapshot(LF(H))` can lie on another
  branch, and `candidate(H)` is then the lower of `prune_σ(H)` and the point where that
  branch leaves `H`.

Beyond satisfying validity rules, the explicit BFT-context selection procedure chooses
`H.context_bft`: among eligible bft-valid tips it chooses a longest chain, then breaks ties by
final-snapshot score and hash
([lines 705–716](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/construction.md#L705-L716)).
BFT-derived data therefore affects block validity as well as this selection procedure. With no
Finality Depth rule, the procedure is the only thing that makes a producer advance its
context (§3.3).

### 3.5 Prefix Consistency and client exposure

Prefix Consistency at depth `σ` is a property assumed of qualifying executions of the
best-chain protocol:

```text
prune_σ(χ_i^t) ⪯ χ_j^u    for honest observations at t ≤ u
```

It is not a Crosslink checkpoint rule and is not enforced by `fin`. If a later best chain
displaces an earlier `σ`-confirmed prefix, an argument that assumes Prefix Consistency no
longer applies to that execution.

The Book [recommends baking in a BFT checkpoint and withholding `fin` from
clients](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/construction.md#L557-L562)
until the checkpoint precedes `LF(bc_best)`, its snapshot precedes `fin`, and `fin` is recent.
This is a sync-safety recommendation, not a block-validity or consensus rule. The Book applies
it to `fin` and `ba_μ`; here it covers `fin` only, and says nothing about exposing `bc_best`.
Zebra Crosslink marks with an `@Todo` where the condition applies and does not implement it;
the current tree has neither.

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

The Questions page is partly historical. It
[analyzes the Last Final Snapshot rule on its own](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/questions.md#L7-L9) and defers the
combination with Linearity to the potential-changes section, which says the Questions argument
against that rule
[was made for a protocol without Linearity](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/potential-changes.md#L346). The fork-choice
change and its "Probably not" belong to the same pre-Linearity discussion, and part of that
discussion
[relies on the Finality Depth rule and Stalled Mode](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/questions.md#L38), which this
design omits. The Book does not revisit the fork-choice change with Linearity in place. The
reason it gives for its conclusion is that an analysis that treats `Π_bft` as possibly
subverted
[can say nothing useful about `snapshot(B)`](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/questions.md#L48-L50).

The per-block rule and the fork-choice constraint remain distinct. A block can satisfy
`snapshot(LF(H)) ⪯ H` using stale BFT context; the fork-choice constraint would constrain the
selected best chain by the newest final BFT snapshot in the observer's view. §4.3 compares that
constraint with sticky fork choice.

### 4.2 The additional Zebra policy

A finalized-prefix eligibility rule would be:

```text
eligible_i(C, t) := bc_valid(C) and local_finalized_tip_i^t ⪯ C
bc_best_i^t      := highest_score({ C | eligible_i(C, t) })
```

This rule preserves the finalized prefix for each node that enforces it. It does not by itself
prove global agreement, network progress, or Prefix Consistency for unfinalized blocks. If an
enforcing node has no eligible progressing chain, local liveness must yield.

**Current tree.** `CrosslinkFinalizeBlock` is stronger still: it commits database state
on the named branch and discards incompatible non-finalized branches. This makes the policy
physical. The CL2 construction does not mandate it. In Zebra Crosslink the floor is `fin`, and
the rule becomes sticky fork choice (§4.3).

Omitting Stalled Mode changes what the policy constrains. The Book pairs raw fork choice with
Stalled Mode, which confines a dominant unfinalizable branch to stalled blocks after `L`. Without
it, raw fork choice lets that branch carry ordinary spends without limit, all of them past `fin`
and unfinalizable under Linearity. A finalized-prefix policy keeps an enforcing node off such a
branch, at the cost of that node's liveness whenever the dominant chain excludes its finalized
point.

### 4.3 Sticky fork choice

Sticky fork choice is the fork-choice rule Zebra Crosslink implements. It selects `bc_best` so
that a node's best chain never excludes its own `fin`. It is temporal: the result depends on the node's current best chain and its
current `fin`, which is node-local memory (§3.2), not only on the set of chains in view.

A node holds `current`, its best chain, and `fin`. When a bc-valid chain `new` is in view, the
node switches from `current` to `new` iff:

```text
fin ⪯ new
and ( work(new) > work(current)
      or ( work(new) = work(current) and tip_hash(new) > tip_hash(current) ) )
```

After every change of best chain, `fin` is updated from `candidate(bc_best)` by the rule in
§3.2. Equal-work chains are ordered by tip hash, which is Zebra's existing tiebreak: `Chain::cmp` in
`zebra-state/src/service/non_finalized_state/chain.rs` orders equal-work chains by tip hash
bytes, and `NonFinalizedState::best_chain` takes the greatest. That doc comment records that the
Zcash protocol specification instead prefers the block received first.

**Relation to §4.2.** `fin` only ever advances to `candidate(bc_best)`, and
`candidate(bc_best) ⪯ bc_best`, so the current chain always contains `fin`. A chain that contains
`fin` now also contained every earlier `fin`, so it was eligible when it appeared and would have
been switched to then if it were greater. The pairwise switch condition therefore selects the
greatest chain, by work and then tip hash, among the chains that contain `fin`. That is the
eligibility rule of §4.2 with `local_finalized_tip := fin`. The temporal behavior comes
entirely from `fin`: the eligible set shrinks each time `fin` advances.

#### Relation to the Book's fork-choice discussion

The nearest relative of sticky fork choice in the Book is the fork-choice change on the
Questions page (§4.1): `bc_best` must extend `snapshot(B)` for the newest final bft-block `B`
in the node's view. The two rules differ in their floor:

- The Questions rule uses `snapshot(B)`. That point need not lie on any chain the node has
  selected, nor be `σ`-confirmed in one.
- Sticky fork choice uses `fin`. It advances only to `candidate(bc_best)`, a point on the node's
  own best chain at or below `prune_σ(bc_best)`.

Under `Π_bft` Final Agreement and Linearity, `fin ⪯ snapshot(B)`: `fin` is at or below the
snapshot of some final bft-block (§3.4), and the snapshots of final bft-blocks are bc-linear.
Every chain containing `snapshot(B)` then contains `fin`, so sticky fork choice refuses a subset
of the chains the Questions rule refuses. Without Linearity the two floors can conflict, and
neither set of refused chains contains the other.

If the Last Final Snapshot rule holds as well, every chain that sticky fork choice refuses ends
in a block whose last final bft-block is strictly older than the one from which `fin` was last
advanced. Suppose `fin` was advanced from a chain with last final bft-block `F`, so
`fin ⪯ snapshot(F)`. A chain `new` with `F ⪯bft LF(new)` has `snapshot(F) ⪯ snapshot(LF(new))`
by Linearity, and `snapshot(LF(new)) ⪯ new` by Last Final Snapshot, so `new` contains `fin`. The
refused chains are therefore stale-context chains, as in the adversary strategy the Questions
page describes.

The Book makes three statements that bear on a change of this kind:

- The honest-production instruction excludes using `Π_bft` information beyond the consensus
  rules to choose among bc-valid chains (§4.1, item 3). Sticky fork choice uses `fin`.
- The liveness analysis
  [attributes its tractability to leaving `Π_bc` fork choice unmodified](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/security-analysis.md#L15-L23),
  in contrast with Casper FFG, whose fork choice follows the justified checkpoint. Under sticky
  fork choice a node never switches to a chain with less work, and an advance of `fin` never
  causes a switch, because `fin` advances only along the current chain. A node can instead
  refuse a switch. The Book has no liveness argument for that.
- The reason given for "Probably not" is that nothing useful can be said about `snapshot(B)`
  once `Π_bft` may be subverted. Under sticky fork choice a subverted `Π_bft` still cannot place
  `fin` off the node's best chain or above `prune_σ(bc_best)`. It can choose when to finalize: a
  `σ`-confirmed prefix finalized before a Prefix Consistency failure would displace it is the
  branch the node keeps. The Book has no safety argument for that either.

#### Properties

These follow from the definitions of `candidate` and `fin` (§3.1, §3.2) and the switch
condition, and hold in Zebra Crosslink. The current tree does not compute `fin` (§6.1).

- `fin ⪯ bc_best` holds on the node at all times. The raw-CL2 state in which `fin` stays fixed
  on a branch that `bc_best` no longer contains (§4.1) does not arise.
- The conflicting-candidate case of the §3.2 update cannot occur: `candidate(bc_best)` and `fin`
  both lie on `bc_best`, so they are comparable. The §3.2 hazard record is therefore never
  written. Its observable counterpart is a refused switch, meaning a chain in view with more
  work than `current` that excludes `fin`.
- The rule selects a different chain from raw work-based fork choice only when a chain with
  more work than `current` excludes `fin`. By the Local fin-depth lemma (§3.2), `fin` was part of
  `prune_σ` of this node's best chain at some earlier time. The raw choice in that situation
  would displace a prefix that was `σ`-confirmed in the node's own earlier best chain. Where no such chain is in view, the two
  rules select the same chain.
- `candidate(H) ⪯ prune_σ(H)` for every `H`, because an lca is an ancestor of both of its
  arguments. `Π_bft` can therefore move `fin` only to blocks the node had already selected by
  work and `σ`-confirmed; it cannot move the node onto a chain it did not select. While `Π_bft`
  is stalled or withholding, `fin` is frozen and selection above it is the raw work rule.
- The node never switches to a chain with less work than `current`.

#### Behavior by situation

Each case compares sticky fork choice with raw work-based fork choice. Statements under
*With Linearity* assume `Π_bft` Final Agreement and enforcement of the Linearity rule, as the
abstract outcome in FINALITY_DIAGRAM §4 does; Zebra Crosslink enforces Linearity, so they
describe it. Statements under *Without Linearity* show what the rule prevents.

- *Candidate regression with `fin` still on the heavier chain* (FINALITY_DIAGRAM §3). Both rules
  switch to the heavier chain.
- *Heavier chain forked below `fin`* (FINALITY_DIAGRAM §4). Under raw fork choice the node
  follows the heavier chain and `fin` stays behind on the other branch. Under sticky fork choice
  the node stays on the branch containing `fin`, and its tip advances only as fast as hash rate
  on that branch extends it. No amount of work on the other branch changes this; only a change
  to `fin` from outside the protocol would.
  - *With Linearity:* no final snapshot can move onto the heavier branch past the branch point.
    Under raw fork choice the node's `fin` stays frozen while that branch is its best chain.
    Under sticky fork choice `fin` can keep advancing if `Π_bft` finalizes the branch the node
    holds.
  - *Without Linearity:* `Π_bft` can finalize snapshots on the heavier branch. Under raw fork
    choice `candidate(bc_best)` then conflicts with `fin`, the node records the §3.2 hazard, and
    `fin` stays frozen. Under sticky fork choice `candidate(bc_best)` is at or below the branch
    point, so `fin` freezes without a hazard record; the event is visible only as the refused
    switch.
- *Partition while `fin` is frozen on every node.* Every chain extending the common `fin` is
  eligible, so selection is by work on both sides. After the partition heals, nodes converge on
  the heavier chain under either rule, provided no node's `fin` moved past the fork point.
- *Partition in which one side advances `fin`.* Side A holds enough stake for `Π_bft` to decide
  and advances `fin` past the fork point; side B does not. After the partition heals, A-side
  nodes never switch to B's chain, whatever its work. B-side nodes switch to A's chain once it
  has more work than theirs, since it contains their `fin`. While B's branch has more work, the
  nodes stay split along the partition. B's blocks cannot advance B-side `fin` past the fork
  point using A's decisions: with Last Final Snapshot they cannot carry that context, and
  without it their candidate is clamped to the fork point. How often this arises, and how many
  nodes land on each side, depends on how stake and hash rate are distributed across the
  partition. Under raw fork choice all nodes follow the heavier branch.
  - *With Linearity:* later final snapshots stay on A's branch, so B-side `fin` never passes the
    fork point and B-side nodes can always still switch to A. Under raw fork choice, if B is
    heavier, finality stays stalled until B's branch is abandoned. A partition lasting more than
    `MAX_BLOCK_REORG_HEIGHT` blocks puts the fork point below the B-side depth commit, which is
    the case the second finalized state of the implementation below exists to serve: the B-side
    node records both branches and switches without an operator.
  - *Without Linearity:* if `Π_bft` later finalizes a snapshot on B's branch, B-side nodes
    advance `fin` past the fork point on B. From then on neither side switches, whatever the
    work, and neither records a hazard, because each side's candidate from the other branch is
    clamped below its own `fin`. Under raw fork choice every node follows the heavier branch, and
    nodes whose `fin` lies on the other branch record the hazard once the candidate passes the
    branch point.
- *Order of observation.* Suppose a lighter branch carries final snapshots past the fork point
  and a heavier branch does not. A node that processes the lighter branch first, for example
  during sync, advances `fin` into it and then refuses the heavier branch. A node that processes
  the heavier branch first keeps `fin` at or below the fork and does not switch to the lighter
  branch until it has more work. Under raw fork choice both nodes end on the heavier branch.
  - *With Linearity:* the premise persists: no later final snapshot can move onto the heavier
    branch past the fork point.
  - *Without Linearity:* the heavier branch can later gain final snapshots past the fork point
    as well. A node that processed it first then advances `fin` into it, and the two nodes stay
    on different branches whatever the work.
- *Conflicting finality.* Each `fin` lies on `prune_σ` of its own node's earlier best chain,
  so two conflicting `fin` values require those best chains to have diverged at depth `σ`, a
  Prefix Consistency failure. Under raw fork choice both nodes follow the heavier chain, and the
  node whose `fin` it excludes records the §3.2 hazard once the candidate passes the branch
  point. Under sticky fork choice each node keeps the branch containing its own `fin`, whatever
  the work on the other, and neither records a hazard.
  - *With Linearity:* conflicting `fin` values also require a Final Agreement failure, because
    every `fin` is at or below the snapshot of a final bft-block (§3.4) and those snapshots are
    bc-linear.
  - *Without Linearity:* conflicting `fin` values need no Final Agreement failure; the
    partition case above is an example.

#### Implementation in Zebra Crosslink

These are requirements; §5 describes the current tree. The rule is implemented through the
finalized database rather than as a separate chain filter:

- On every change of `bc_best`, the node computes `N := candidate(bc_best)`. If `fin ⪯ N` and
  `N ≠ fin`, it finalizes up to `N` and stores `N` as `fin`. A candidate at or below `fin`
  changes nothing. The candidate computation and both writes belong to the code that changed the
  best chain, so they are one synchronous sequence rather than a request that can fail partway
  (§7.1).
- The commit discards every non-finalized chain that does not contain `N`, and Zebra rejects
  blocks that fork below its finalized tip. Chains that exclude `fin` therefore never enter the
  node's view, which is the switch condition above. That rejection is the refused switch; the
  node reports it on stdout, and a persisted hazard record is an `@Todo`.
- `fin` is stored in the finalized database as its own block hash, so the floor survives a
  restart. The database's finalized tip is the higher of `fin` and the reorg-depth commit (next
  bullet), so finality readers take `fin`, never the finalized tip.
- A BFT decision does not change the finalized state. It advances `bft_final_snapshot` (§2),
  which can lie on a chain that is not `bc_best`. Only a later `bc_best` change moves `fin`, and
  only by the rule above.
- The node syncs the chain leading to `bft_final_snapshot` whether or not it is `bc_best`, and
  it never needs a resync to do so. While that chain forks above the depth-committed block it is
  a chain of the non-finalized state, exempt from the pruning that drops the lowest-work chains
  past `MAX_NON_FINALIZED_CHAIN_FORKS`, and it survives a restart through the non-finalized
  backup. Every chain carries its aggregated stakes per block, beside the per-block
  `bond_rewards` and `finalizer_commissions` it already unwinds, so the validator set at any held
  block is a lookup, on either side of a finalized tip (§7.3). Under Linearity that chain contains
  `fin`, so it remains eligible, and the node switches to it once it has more work. Bc-block
  validity, bft-block validity and roster computation are therefore one synchronous domain in
  `zebra-state` (§7.1).
- Zebra also commits the root of the best chain to the finalized database once the chain is
  longer than `MAX_BLOCK_REORG_HEIGHT` (from `zcash_protocol::consensus`, applied in
  `zebra-state/src/service/write.rs`). The value in the current tree is 99. Upstream Zebra
  raised it to 999, and that change was lost when this tree was rebased onto new Zebra, so 999
  is the intended value; the depths of 99 written elsewhere in this document follow the tree.
  Chains forking below that point are no longer in view of that database.
  On a Zebra node the effective floor is the higher of `fin` and that depth-committed block.

  The depth commit is never held back indefinitely. A node whose BFT has stalled, for any length
  of time or forever, keeps committing its PoW best chain and remains a working PoW node with a
  frozen `fin`. If `bc_best` then runs more than that depth past the point where the chain to
  `bft_final_snapshot` forks from it, the depth commit writes a block that conflicts with
  `bft_final_snapshot`, and one finalized database cannot be rewound to take the other branch.
  The node must nevertheless keep syncing and recording both branches for as long as both grow,
  validating bft-blocks and computing rosters along the BFT branch, and switch its served best
  chain to that branch when the rule above says to. That is a second finalized state:

  - The PoW state **P** is today's Zebra: raw work fork choice and the depth commit. Its
    finalized state keeps, at a height at or below `fin`, a snapshot of itself from which an
    independent, writable copy can be opened while P keeps writing. The snapshot is retaken as
    `fin` advances, and during a stall it stays valid, only staler. Every chain containing `fin`
    forks at or above it.
  - When a bc-block arrives that forks below P's finalized tip but above `fin`, the node opens
    the Crosslink state **C** from that snapshot, replays P's own stored blocks from the
    snapshot height to the fork point into it, and from there feeds C the conflicting chain from
    peers. C's fork-choice floor is `bft_final_snapshot` and C never depth-commits. Both states
    keep syncing and committing; the served best chain is chosen across both by the switch rule
    above. C is dropped once `fin` passes the fork, and P's branch is recorded for as long as
    blocks arrive on it.
  - How the snapshot is taken belongs to the storage engine, and it sets the cost of a conflict
    rather than whether the node survives one: a hard-linked checkpoint or a filesystem reflink
    clone is milliseconds, a persistent savepoint plus a file clone needs P's writer paused for
    the copy, and a logical copy into a fresh database costs a full database of time and disk.
    The last is the portable floor and is acceptable, because what it pays for is a network
    partition deeper than `MAX_BLOCK_REORG_HEIGHT`.

  Until the second state exists, the node holds P's depth commit at the fork point while a
  conflict is live, up to `CONFLICT_HOLD_DEPTH` blocks past the fork, then commits and reports on
  stdout that it can no longer follow `bft_final_snapshot`; that path carries an `@Todo` naming
  the second state. The hold is an interim and never the design, because under a permanent
  conflict it is the same wall `CONFLICT_HOLD_DEPTH` blocks later. A node never requires a resync
  to resume bft-block validation.

Sticky fork choice and Linearity constrain different points. Sticky fork choice keeps `fin` on
`bc_best`; Linearity keeps each final snapshot on or after the previous one. `fin` lies at or
below the newest final snapshot, so a reorganization that forks between the two is admitted by
sticky fork choice and then leaves finality on the new branch waiting (§3.4, Linearity and bc
reorganizations).

#### Current tree

- The rule needs protocol `fin`, which the current tree does not compute (§6.1). Its collapse
  onto a BFT-decided branch (§4.2, §6.3) is a related rule with a different floor: the stored
  marker, taken directly from a decided BFT block when it is decided rather than from
  `candidate(bc_best)`. With that floor, the invariant above does not follow: the marker need not
  lie on the node's best chain when it advances, and a known side-chain hash becomes canonical
  (§5.2).
- It enforces Linearity and Last Final Snapshot (§6.2).

## 5. Current tree: implementation inventory

Everything in this section describes the current tree, not requirements. It refers to symbols
in the tree this document ships with; symbol names are preferred over brittle working tree
line numbers.

### 5.1 The overloaded marker and write paths

`zebra-crosslink/zebra-state/src/new_network/bft.rs` defines
`BftChain::latest_final_block: Option<(Height, Hash)>`. It is assigned only by
`set_final_block`, which also sends the new value on `final_change_tx`. Its callers are:

- `BftRunner::decide`, after inserting the BFT block;
- `BftRunner::restore`, when restoring the last entry from the PoS store; and
- `tfl_set_finality_by_hash`, through the testing/service setter, which reaches it as
  `set_final_block_if_activated`.

The live path takes `snapshot(new_block) = parent(new_block.headers[0])` from
`BftBlock::snapshot_block_hash`, the one accessor through which every reader derives the
finalized block (§8.1).

When this marker is absent, `tfl_final_block_height_hash` returns `None`. It previously
substituted a Zebra reorg-depth location derived from the state block locator, so that the API
changed semantics depending on whether Crosslink had produced a value; that substitution and
its helper have been removed. Removing it was safe because the substitution reached only three
readers, all RPC-facing: `tfl_block_finality_from_height_hash`, the `FinalBlockHeightHash`
service request, and the `TxFinalityStatus` service request. Every consensus-, state-, and
GUI-side reader takes `BftChain::latest_final_block` directly and never saw the substituted
value.

The Crosslink service no longer carries a second copy of the marker: `current_bc_final`, which
was written during PoS-store startup and read nowhere, has been deleted.

### 5.2 Irreversible commitment and ordering

`BftRunner::decide` assigns and publishes `latest_final_block` before it calls
`WriteBlockWorkerTask::handle_crosslink_finalize`. A decision the commit rejects is parked and
retried on later ticks. During that interval, RPC and GUI readers and notification subscribers
can observe a marker whose database state has not been finalized.

A decision and its database commit are coupled. Tenderlink awaits the decide callback before it
starts the next round, and the reply is held until the finalize succeeds, so BFT progress waits
on the finalized-state write. In Zebra Crosslink they are separate (§4.3): a decision advances
`bft_final_snapshot`, and the finalized state follows `fin`.

The state behavior depends on whether the hash is known:

- `new_network` accepts a hash found in any non-finalized chain or in the finalized database.
  `NonFinalizedState::crosslink_finalize` retains the chain containing a known side-chain hash,
  so finalizing that hash can make the side chain canonical before blocks are committed by
  `WriteBlockWorkerTask::handle_crosslink_finalize`.
- a hash the state does not know never reaches the finalize call. `BftRunner::decide`
  first asserts that `validate` passes, and validation returns `Indeterminate`
  (`NeedsBlock`) when `KnownBlock` cannot resolve the snapshot block, so the assertion panics and,
  under `panic = abort`, the node exits. The retry loop runs only for a hash known at that
  point; if the chain holding it is then dropped from the non-finalized state before the
  finalize succeeds, the decision stays parked indefinitely.
- the PoS-store restore path unwraps the same `KnownBlock` lookup for the last stored BFT block,
  so a finalized database that is behind the PoS store, for example one wiped and re-syncing,
  panics at startup. The replay-watermark loop just above it tolerates that case.

Consequently, the stored marker is neither a reliable `fin` implementation nor a reliable
record of the finalized database's tip. In Zebra Crosslink, persisted `fin` advances only after
the state request succeeds, and only by the CL2 update rule.

### 5.3 Consumers

The overloaded value currently reaches:

- irreversible state commitment through `handle_crosslink_finalize`;
- `BftChain::roster`, using the aggregate stakes the finalized database holds at the snapshot;
- hardfork finalizer filtering through `terminated_finalizers_at`;
- `get_tfl_final_block_*`, block-finality, and transaction-finality RPC methods;
- the GUI's finalized row, terminated-finalizer display, and visualization paging lower bound;
- BFT proposal and validation paths.

"Reaches" above is deliberately loose, and the distinction matters when planning a rename or a
change of derivation. The actual reads of `BftChain::latest_final_block` are only: the BFT
proposal path and three sites in `viz2.rs` (the paging lower bound, the
`terminated_finalizers_at` height input, and the GUI finalized tip). The others receive the
same value by another route rather than by reading the slot:

- `handle_crosslink_finalize` is passed the local `new_final_hash`; the field is written from that
  same local immediately before. Deleting the field would not change its behavior.
- `BftChain::roster` is a write target, populated from the aggregated stakes the finalized
  database holds for `new_final_hash`. That read is the
  node's own and is separate from the commit, so the slot carries no information the commit
  put there.
- `terminated_finalizers_at` is passed a local height at three of its four call sites; only the
  `viz2.rs` site reads the field.
- The BFT validation path's read is dead: `already_finalized_hash` is captured and then
  discarded by `let _ = already_finalized_hash;`, because the queue re-flush it once served has
  been removed.

By actual reads, the widest consumer of the slot is the visualizer, not consensus.

`TFLServiceRequest::FinalBlockRx` returns subscribers to `BftChain::final_change_tx`,
and the RPC notification methods in `zebra-crosslink/zebra-rpc/src/methods.rs` wait on them.
Every `set_final_block` call sends on that channel, so a notification carries the same
overloaded value at the same moments: on the decide path before `handle_crosslink_finalize`
succeeds, on PoS-store restore, and through the testing setter.

### 5.4 Current staking rewards

At the end of `Chain::push` in
`zebra-crosslink/zebra-state/src/service/non_finalized_state/chain.rs`:

- a block that does not pay (see the payout rule below) pushes empty `bond_rewards` and
  `finalizer_commissions` entries and mints nothing; the empty entries keep positional reorg
  reversal aligned;
- if no bond is active, the same empty entries are pushed and no staking reward is minted; and
- otherwise it distributes the fixed `POS_BLOCK_REWARD_ZATS` for that PoW block, increases
  `staking_bonded_amount` by the same total, and records the per-bond rewards for exact reorg
  reversal.

`update_bonds_with_pos_issuance` in `zebra-crosslink/zebra-state/src/service.rs` allocates the
total pro rata with integer division, gives the remainder to the largest active bond (then
smallest key on a tie), and adds rewards to bond principal. Rewards therefore compound.

#### The variable payout rule

Issuance is not paid per PoW block. A block `P` pays exactly when it *advances* finality and
does so *promptly*:

```text
payout(P)  iff  cert(P) != cert(parent(P))  and  height(P) - F <= σ + FINALITY_LIVENESS_ALLOWANCE
```

where `cert(P)` is the BFT block named by `P.context_bft` (compared by BFT block hash, not by
the whole fat pointer: two honest nodes can carry different signature sets for the same
decision) and `F` is the height of the PoW block that certificate finalizes — its snapshot.
`FINALITY_LIVENESS_ALLOWANCE = 3`, in `librustzcash/zcash_primitives/src/bft.rs`.

Both inputs are objective functions of committed chain data, so every node computes the same
answer for the same block, as §9.1 requires. The fat-pointer check (§6.2) already refuses any
`P` below `F + σ + 1`, so `height(P) − F` is at least `σ + 1`: with σ = 4 the paying gaps are 5,
6 and 7, i.e. 4, 5 or 6 blocks strictly between `F` and `P`, and a seventh earns nothing.

The decision is made in `new_network::bft::admit_fat_pointer`
(`zebra-crosslink/zebra-state/src/new_network/bft.rs`), which is the one place that can resolve both
facts, and travels with the block as `SemanticallyVerifiedBlock::pos_payout` →
`ContextuallyVerifiedBlock::pos_payout` → `Chain::push`. Paths that never run that check
(checkpoint sync, tests, blocks rebuilt from raw bytes) carry `pos_payout: false` and mint
nothing.

Because the verdict cannot be recovered from the block bytes, it is **persisted in the
non-finalized state backup** alongside the deferred pool change
(`zebra-state/src/service/non_finalized_state/backup.rs`), and restored with the block. Without
that, a node that restarts re-enters its non-finalized blocks through
`SemanticallyVerifiedBlock::from(Arc<Block>)`, which defaults to `pos_payout: false`: the
restarted node mints nothing for blocks every other node has already paid. That is not a local
accounting slip. It changes the bonded stake, the bonded stake is the voting power, and the
roster derived from it then differs between nodes — which in a two-node roster is enough to
make both nodes believe they are the proposer, prevote different values forever and stall
finality permanently. This was observed on a dilated two-node testnet: node two restarted,
restored 4 backed-up blocks, and came back exactly `4 × POS_BLOCK_REWARD_ZATS` short, after
which BFT never decided another block.

The same per-block calculation is replayed by the wallet projection path in
`zebra-crosslink/zebra-crosslink/src/lib.rs`, which recomputes the rule from committed data in
`block_pays_pos_issuance`, and by `fixup_aggregated_stakes` in
`zebra-crosslink/zebra-state/src/service/stake_fixup.rs` (reached through the `--fixup-db-stake`
entry point). Any future consensus change must keep all three paths identical.

The repair tool is the one path that cannot evaluate the rule in full: it has the PoW database
and nothing else, and `F` lives inside the BFT block. It applies the half it can see — a block
that does not advance the certificate pays nothing — and assumes an advancing block was
prompt. That is correct whenever BFT kept up. When it did not, the replay disagrees with the
rows already stored and its existing cross-check refuses to write anything, so the failure mode
is a repair that declines, never a repair that corrupts.

### 5.5 The Crosslink service crate

The decided bft-chain, the `tenderlink` engine and every reader of them live in `zebra-state`,
in `zebra-crosslink/zebra-state/src/new_network/bft.rs`. What is left of
`zebra-crosslink/zebra-crosslink` is about 1,000 lines in four files: `lib.rs`, holding the
service and the wallet, faucet and staking arms; `service.rs`, which constructs it; `viz2.rs`,
the GUI feed; and `test_format.rs`, the `.zeccltf` test driver. It exposes a tower `Service`
over `TFLServiceRequest` whose handlers take a tokio `Mutex` on `TFLServiceInternal`, which now
holds only the BFT message and error counters and the two BFT connection strings.
`tfl_service_main_loop` starts the visualizer and the test driver and then sleeps, because
`zebrad` treats the service task's exit as a node shutdown.

Nothing on a consensus path calls that service, and nothing calls into it from `zebra-state`.
Its callers outside the crate are `zebra-crosslink/zebra-rpc/src/methods.rs` — the finality
methods and the staking, wallet and faucet commands — and one site in
`zebra-crosslink/zebrad/src/lightwalletd.rs`, which asks for `Faucet`. The roster, the block
template's fat pointer and the recency status are `ReadStateService` requests
(`CrosslinkRoster`, `CrosslinkFatPointerToBftChainTip`, `CrosslinkRecencyStatus`).

`BftChain` in `new_network::bft` is the decided bft-chain: `blocks` and `hash_to_height`,
`fat_pointer_to_tip`, `roster` — the validator set read at the snapshot of the previous decided
bft-block — `latest_final_block`, which is the overloaded marker of §5.1, and `is_activated`. It
sits behind one `RwLock`; the `new_network::sync` thread is its only writer, and every reader
outside that thread takes the read lock without an asynchronous call. Recency status is a
`tokio::sync::watch` the `bft_access` closure publishes.

`BftRunner`, owned by the `sync` loop, holds the `tenderlink` side. The five closures of
`tenderlink::entry_point` each send one `BftRequest` and await one reply; the loop drains that
channel where it used to sleep, so `propose`, `validate` and `decide` run on the thread that
holds the best chain, the non-finalized state, the finalized database and the commit path. A
decision the commit rejects is parked and retried on later ticks rather than blocking the
round.

`admit_fat_pointer` answers the Extension, Last Final Snapshot and σ-confirmation verdicts and
the `pos_payout` flag (§5.4, §6.2) as a pure function of the `BftChain` and a `ReadStateService`,
called under the read lock from the retain loop that filters blocks about to be committed.

A second copy of the decided bft-chain lives in the PoS store file. The decide path appends a
record holding the `BftBlock`, its fat pointer, the roster and the proposal signatures;
`BftRunner::restore` replays the file at startup to rebuild the chain, the roster and
`tenderlink`'s `ingest_startup_data`, and recomputes the watermark `prev_finalized_bc_height`
from each record's snapshot. The restore path unwraps a state lookup for the last stored block,
so a finalized database behind the file ends the process (§5.2).

`force_feed_bft_block` injects a decided bft-block without `Π_bft`, as a message to the `sync`
thread. Its only caller is `test_format.rs`, through `TFLServiceCalls::force_feed_pos`.

`tfl_block_finality_from_height_hash` answers block status with two state requests and the final
marker between them, so its reads can straddle a reorganization. It also builds a `BlockHeader`
request that it never awaits.

## 6. Current tree: divergences from Zebra Crosslink

Each item states a current-tree fact and, where it is not evident, the Zebra Crosslink behavior
it departs from.

### 6.1 Derivation and update trigger

- **Wrong trigger and input.** Protocol `fin` is updated from `candidate(bc_best)` whenever the
  best-chain view changes. Zebra updates `latest_final_block` when a BFT block is decided or
  restored, without requiring the current best chain to cite that decision.
- **Missing clamp.** Zebra does not compute
  `lca(snapshot(LF(H)), prune_σ(H))`; it takes a hash directly from the decided BFT block.
- **Header order is enforced by validation, not by the type.** Honest proposal construction issues `FindBlockHeaders` with
  the snapshot block as the sole known hash. That request returns the headers *following* the
  intersection, ascending, so the proposal carries the `σ` blocks above the snapshot,
  deepest-first, and `parent(headers[0])` is the snapshot. `σ` headers suffice, because
  `headers[0]` carries the snapshot's hash in its parent field, and a validator must hold the
  snapshot block to validate the certificate anyway. `BftRunner::validate` enforces that order
  as part of Tail Confirmation, rejecting a block whose headers do not each name the one below
  as parent (§6.2). The type does not: `BftBlock::try_from` checks only the header count and
  logs that its documented validations are unimplemented, and the deserialization path used
  for network and PoS-store blocks does not call `try_from` at all. The snapshot is named by hash only; a consumer that needs
  its height asks the chain, and the fat-pointer check is handed a height lookup for that
  purpose (§6.2).
- **The candidate height is clamped, and the clamp is not `prune_σ`.** The proposal path
  computes `tip − σ` and then takes
  `min(tip − σ, latest_final_block + 40)`. Only when that clamp does not bind is the stored
  marker `prune_σ(tip)`, i.e. `σ` confirmations. Whenever `tip − σ > marker + 40`, which is the normal regime during
  catch-up after a restart or a BFT stall, the candidate is `marker + 40` and the block is
  finalized far deeper than `σ`. Any statement of the form "the proposal path finalizes at
  `tip − σ`" is true only in the unclamped regime.
- **The improvement test runs before the clamp.** `is_improved_final` compares the proposal's
  snapshot height, `tip − σ`, against the stored marker, so every new PoW block is proposable
  at once. The clamp can only lower the snapshot to `marker + 40`, so it never turns an
  admitted proposal into a non-improving one.
- **Missing monotonicity and hazard record.** All marker writes are unconditional. There is no
  `fin ⪯ candidate` guard and no distinction between a benign candidate regression and a
  conflicting-candidate safety incident.

### 6.2 Validity rules

- The Last Final Snapshot rule is enforced on bc-block admission, beside the Extension rule in
  `new_network::bft::admit_fat_pointer`, with the same defer/reject split: a
  snapshot the state cannot yet place on a branch defers, and one it places off the block's own
  ancestry is rejected permanently. Ancestry is read with `ReadStateService::is_ancestor_of`,
  across every chain the state holds rather than the best chain alone.
- The Finality Depth rule and Stalled Mode are omitted by design (§3.3). The 512-block log
  threshold is diagnostic, not consensus.
- BFT validation enforces Linearity and Tail Confirmation in `BftRunner::validate`. Tail
  Confirmation is checked as the three things it is: exactly `σ` headers, each naming the one
  below it, and the block at the topmost header known to this state — which, given the linkage,
  carries the bc-validity of the whole tail, since a block the state holds has been validated
  along with its ancestry. Linearity compares the parent bft-block's snapshot against this
  block's through the same ancestry read. A block either check cannot resolve yet returns
  `Indeterminate` with the hash it needs, as a missing snapshot already did.
- **The confirmation depth is enforced on inclusion.** A PoW block at height `P` may carry a
  fat pointer to a BFT block whose snapshot is at height `F` only when `P ≥ F + σ + 1`: the
  `σ` carried headers `F+1 ..= F+σ`, then the carrier. Admitting a PoW block therefore
  requires a PoW → PoS → PoW lookup: resolve the pointer to its BFT block, take that block's
  snapshot hash, and ask the state for its height.
  `new_network::bft::admit_fat_pointer` resolves the pointer in the decided chain it holds and
  asks the `ReadStateService` beside it for the height, searching every chain the state holds,
  and defers rather than rejects while the snapshot is unknown here. The block-template path
  applies the same test, so a miner is never handed a certificate that could not be committed.
  The inequality bounds depth only; whether `F` is an ancestor of `P` is the Last Final
  Snapshot rule, which the same read answers.
- The proposal path departs from honest proposal (§3.4) in two ways. When the `+40` candidate
  clamp in §6.1 binds, `headers_bc` is a window ending at `marker + 40 + σ`, not the tail of the
  proposer's `bc_best`; the window still satisfies Tail Confirmation. The clamp is a Zebra
  Crosslink design heuristic (§3.4). Where honest proposal repeats the parent's `headers_bc`,
  the path makes no proposal instead: when `is_improved_final` fails, when the candidate's
  snapshot would fail Linearity against the parent bft-block's, and when the tail the chain
  returns is short or does not link to the candidate. The two reads behind the tail run on the
  writer thread, so no commit falls between them. How often a node should repeat its
  parent's headers is an open implementation question, so declining is what it does until
  that is settled.
- Bc-block production follows the honest context-selection procedure (§3.4) as far as validity
  requires: the block template's `FatPointerToBFTChainTip` request cites the newest decided
  bft-block whose `do_not_include_until_bc_height` admits the proposed height, whose snapshot is
  deep enough for the σ-confirmation rule, and whose snapshot lies on the chain the template
  extends. It does not implement the Book's longest-chain-then-score-then-hash tie-break, which
  is a selection among bft-valid tips this tree does not hold: the decided chain is linear here.
  When no decided block qualifies, the template repeats the parent block's own `context_bft`,
  which always does; reverting to the null pointer would break the Extension rule.
- Block templates lag BFT decisions. Time-accelerated and realtime tests show miners producing
  two consecutive bc-blocks with the same fat pointer, each of which adds a block of finality
  lag (§3.4). Any rule keyed to finalization at exactly `σ + 1` below the tip misses those
  blocks.
- The Extension rule is implemented by
  `new_network::bft::admit_fat_pointer`, including its defer/reject
  distinction.

### 6.3 State-finalization and fork-choice policy

`CrosslinkFinalizeBlock` collapses non-finalized state onto a known named branch. This locally
enforces a finalized-prefix activation policy and prevents a higher-score conflicting chain
from becoming canonical. Raw CL2 does not impose that rule. The existing test
`crosslink_pow_switch_to_finalized_chain_fork_even_though_longer_chain_exists` documents that
behavior.

Zebra Crosslink names separately:

- protocol `local_finalized_tip` (`fin`), which under sticky fork choice is also the Zebra
  policy floor `canonical_finalized_tip` (§2, §4.3); and
- the finalized tip of the PoW state's database, the higher of `fin` and the reorg-depth commit;
  a second state opened for a conflicting BFT branch (§4.3) has its own.

### 6.4 Unbounded finality gap

Nothing in consensus bounds the finality gap or restricts which transactions a block far past
the snapshot may carry. This follows from omitting Stalled Mode (§3.3). It has two practical
consequences:

- The diagnostic warning at a hardcoded gap is the only signal of a long finalization stall.
  Any response to one, such as alerts, wallet warnings, or operator action, is outside
  consensus.
- A best chain that has forked below `fin` (§3.5) can carry ordinary spending transactions for
  as long as it dominates. Under raw CL2 fork choice nothing limits that activity. In Zebra
  Crosslink, sticky fork choice keeps a node off such a branch (§4.3); in the current tree, the
  collapse onto each decided block does (§4.2).

### 6.5 Client exposure and API semantics

There is no checkpoint/recency sync condition on client exposure. Before the Crosslink marker
exists, finality RPCs now report no value rather than silently exposing the legacy reorg-depth
fallback; `get_tfl_final_block_hash` and `get_tfl_final_block_height_and_hash` return `null`,
and the block- and transaction-finality methods collapse their error to `null` as well.
Existing GUI and RPC surfaces still conflate raw tip, confirmation, and finalization instead of
defining each endpoint's contract, and what these methods return once the marker *does* exist is
still the legacy-fed slot, not `fin`.

### 6.6 Ordering and notification

- The visible marker advances, and `FinalBlockRx` subscribers are notified, before irreversible
  state commitment succeeds.
- A known side-chain hash can change the canonical branch. A hash unknown to state panics the
  decide path; a hash whose chain is dropped after validation can retry forever (§5.2).

### 6.7 Placement

Zebra Crosslink keeps finality state in `zebra-state` (§7.1). The decided bft-chain, the
`tenderlink` engine and the admission check are there now (§5.5); `fin` and the persisted chain
are not, and these divergences follow from that:

- A decision and its commit are one event (§5.2). The decide path calls
  `handle_crosslink_finalize` directly and parks a decision the commit rejects for retry on a
  later tick, and `tenderlink` does not start the next round until the decide closure returns.
- The decided bft-chain is stored twice, in memory and in the PoS store file, and is restored
  from the file with its roster bytes rather than recomputed (§5.5, §8.1).
- The roster read answers from the finalized database alone, so it is correct only while the
  decide path commits the snapshot it has just decided (§8.1).

## 7. Names and consumer contracts

This section is Zebra Crosslink. The protocol names encode their definitions:

| protocol quantity | value identifier | optional newtype |
|---|---|---|
| `bc_best` | `bc_best_tip` | `BcBestTip` |
| `candidate(H)` | `finalization_candidate` | `FinalizationCandidate` |
| `fin` | `local_finalized_tip` | `LocalFinalizedTip` |
| `bft_final_snapshot` | `bft_final_snapshot` | `BftFinalSnapshot` |

The database's finalized tip has a name distinct from `fin` (§6.3). The legacy reorg-depth
value keeps a name that says it is a reorg-depth marker, not Crosslink finality.

### 7.1 Where finality state lives

Finality state is chain state. The quantities of §2 are computed from blocks the node holds —
`fin` from its own history of them (§3.2) — and every consumer of one needs the bc-chain as it
stood at the same instant. Zebra Crosslink keeps all of it in `zebra-state`, beside the finalized
database and the non-finalized state, reachable without an asynchronous call:

- `fin` is a column of the finalized database, written in or after the batch that commits the
  block it names (§8.1).
- The decided bft-chain — its blocks, their fat pointers, and the proposal signatures that
  accompany them — is stored the way bc-blocks are stored, in the same database.
- `LF(H)`, `snapshot(LF(H))`, `candidate(H)`, `bft_final_snapshot` and the σ-confirmation test
  are computed from those two by the code that also holds the chains they are compared against.
- The validator set is derived from bonds on the chain the decided bft-block names, by the same
  function that computes the finalized aggregate (§7.3).

`Π_bft` is not chain state. The `tenderlink` crate owns the vote rounds, the message transport,
and the rule that a height decides before the next one starts; none of that is a function of the
chain, and it stays where it is. It reaches its host through the arguments of
`tenderlink::entry_point`, which are the whole interface:

| argument | what the host answers with |
|---|---|
| `propose_closure` | a bft-proposal built from the host's `bc_best`, or nothing (§3.4) |
| `validate_closure` | a verdict on a proposed bft-block, or `Indeterminate` with the block it still needs |
| `push_block_closure` | acceptance of a decision, answered with the roster and vote namespace for the next height |
| `peer_cmd_closure` | the peer addresses of the current roster |
| `bft_access_closure` | a copy of round state for display; it decides nothing |

Three of the five need the bc-chain: a proposal is the σ-block tail of `bc_best`, validation is
Tail Confirmation and Linearity against the chains the node holds, and the roster is the bonds
at a bc-block. The host side therefore belongs in `zebra-state`, which already depends on
`tenderlink` and already runs its transport (`tenderlink::stp`, `tenderlink::native_sockets`)
for bc-block propagation. Nothing sits between the two: a crate that relays between `tenderlink`
and `zebra-state` can only reintroduce the boundary that makes these quantities race.

The closures stay asynchronous, because `tenderlink` awaits them. What moves is the side of the
boundary the reads happen on: a closure sends one message to the block writer and awaits one
reply, and every chain read behind that reply is local and synchronous. There is no path back
out, so there is no lock ordering to respect (§6.7). **Current tree:** this is how the five
closures are built, in `BftRunner`.

Finality state is chain state, and there can be two chain states at once. When the depth commit
and `bft_final_snapshot` come to lie on different branches, the BFT branch is a second finalized
database plus non-finalized state of the same shape as the first, opened from a snapshot of the
first at or below `fin` (§4.3). `zebra-state` routes blocks and reads between the two and chooses
the served best chain across both. How that snapshot is taken belongs to the storage engine, not
to the protocol.

**By fate.** What the Crosslink service crate held is four kinds of thing, and only the first is
irreducible. The moves marked "moved" are done (§5.5); the rest are ahead:

| current tree | fate |
|---|---|
| `tenderlink` and its `entry_point` interface | unchanged; `zebra-state` constructs the five closures |
| `bft_blocks`, `bft_block_hash_to_height`, `fat_pointer_to_tip`, `finalizers_at_current_height` | moved: `BftChain` in `new_network::bft`, still in memory rather than in the database |
| `latest_final_block` | moved to `BftChain`; becomes `fin` in `zebra-state`, with one writer and the §3.2 update rule (§6.1) |
| `propose_new_bft_block`, `BftRunner::validate`, `handle_new_decided_bft_block`, `call_from_state_to_crosslink_to_ask_about_fat_pointers` | moved: `BftRunner::{propose, validate, decide}` and `admit_fat_pointer`, each reading one consistent view on the writer thread |
| the PoS store file | dies; its records become database rows, and its roster is recomputed rather than restored |
| `TFLServiceInternal`, `tfl_service_main_loop`, `TFLServiceHandle`, the tower service over `TFLServiceRequest`, and the reentrancy constraint it imposes | the reentrancy constraint is gone; the rest die; every finality arm is answered by the state service or by a channel it publishes |
| the wallet, faucet and staking arms | survive as calls rather than as a service: they relay to the `wallet` crate and read no finality quantity |
| `viz2.rs` | survives as a view over the state service and a published diagnostic snapshot, holding no state of its own; where it lives is a packaging question |
| `test_format.rs` and `force_feed_pos` | survive as test support beside the tests they drive; injection is a message to the block writer |

Once the moves above are done, what remains holds no finality state and sits on no consensus
path, so where it lives is a packaging question and not a finality one. The GUI feed is the
largest such piece, and it is a view in the sense of the §7.2 table: it reads the state service
and the published round-state snapshot, answers the renderer's requests from them, and is free to
lag or to be absent without any consequence for consensus.

### 7.2 Consumer contracts

No protocol view lies between the best tip and the finalized tip, so no CL2 quantity is a
default for "confirmed" presentation. Each consumer needs a contract:

| consumer or endpoint | value | contract or unresolved work |
|---|---|---|
| raw best-tip display | `bc_best_tip` | current fork-choice result |
| confirmed display | `bc_best_tip` at a stated confirmation depth | `Π_bc` confirmation only; can be an ancestor of `local_finalized_tip`, or conflict with it after a Prefix Consistency failure (§3.3); never present it as final |
| final display | `local_finalized_tip` | node-local monotone CL2 view |
| `get_tfl_final_block_hash` and `get_tfl_final_block_height_and_hash` | `local_finalized_tip` | no value before the first `fin`; the exposure condition of §3.5 is an `@Todo` |
| block status | `local_finalized_tip` and `bc_best_tip` | `Finalized` if the block is an ancestor of or equal to `local_finalized_tip`; `InBestChain { confirmations }` if it is on `bc_best` above that; `NotInBestChain` otherwise, including unknown blocks |
| transaction status | status of the block containing it | the block status of its mined block under the same three states; a mempool transaction has no block status |
| finality-change notifications | `local_finalized_tip` transitions | sent after `fin` is persisted; the exposure condition of §3.5 is an `@Todo` |
| visualization paging | operational paging cursor | do not overload a finality value merely to bound a window |
| canonical state activation | `fin` | sticky fork choice floor (§4.3) |
| physical database status | database finalized tip | higher of `fin` and the reorg-depth commit, in the PoW state; a second state for a conflicting BFT branch has its own; never reported as Crosslink finality |
| staking rewards | objective per-block source | never use node-local `fin`; see §9.1 |
| validator roster and hardfork membership | bonds at `snapshot(B_{H−1})` | objective; see below |
| block-template BFT context | newest qualifying decided bft-block | the σ-confirmation and Last Final Snapshot tests are the ones bc-block admission runs, so a template never carries a certificate its own chain would refuse (§6.2) |
| BFT round diagnostics | `tenderlink` round state | display only; a published snapshot, never an input to consensus |
| visualization feed | `bc_best_tip`, `local_finalized_tip`, the decided bft-chain, round diagnostics | a view: it holds no state and decides nothing |
| wallet, faucet and staking commands | none of the above | not finality; they relay to the `wallet` crate |

Every row is served by `zebra-state`: a read request against the finalized database and the
non-finalized state, or a watch channel published beside the existing chain-tip channels. No row
is served by a component that keeps its own copy of the value (§7.1). Block and transaction
status read `fin` and the best chain together, so they cannot report two different moments.

**Current tree.** Every row keyed on `local_finalized_tip` reads `latest_final_block` instead
(§5.3), and reaches it through `TFLServiceRequest` (§5.5). The finality RPCs return no value
while that slot is empty.

### 7.3 Consensus-sensitive roster and hardfork inputs

The validator roster, voting power, and hardfork-driven membership changes are
consensus-sensitive. They must not read node-local `fin` unless there is a proof that every
validator derives the same value at the same BFT height. Prefix compatibility between honest
`fin` values is insufficient.

The two quantities are separate:

- **Canonical ledger state** is selected by sticky fork choice with floor `fin` (§4.3).
- **The validator set for BFT height `H`** (roster, voting power, and hardfork-driven
  membership) is read from the bonds at `snapshot(B_{H−1})`, where `B_{H−1}` is the decided
  bft-block at height `H − 1`. `Π_bft` agreement fixes `B_{H−1}`, its snapshot is a function of
  its `headers_bc`, and the bonds at a bc-block are a function of that block's ancestry, so every
  validator that has those blocks derives the same set. `terminated_finalizers_at` takes the
  height of the same block.

`snapshot(B_{H−1})` generally lies above `fin`, because `candidate(H) ⪯ snapshot(LF(H))` and
`fin` advances only once a bc-block citing the bft-block is best. It need not lie on `bc_best`
at all, and can stay off it for any length of time (§2, §4.3). Its bonds are therefore read from
the chain leading to `bft_final_snapshot`, which the node syncs and stores independently of its
best chain: as a non-finalized chain carrying its aggregated stakes per block, or, once it forks
below the depth commit, as the second state of §4.3. The aggregate at a block is the same function
whichever side of a finalized tip the block is on, so a block committed later yields the
identical row. An honest validator has downloaded that chain while validating `B_{H−1}` (§3.4).

Validation on that chain is interdependent but well-founded. Validating bft-block `B_H` needs
the validator set from the bonds at `snapshot(B_{H−1})` and the bc-blocks under `B_H.headers_bc`;
validating those bc-blocks needs the bft-blocks their fat pointers cite, all of which were
decided before them. Processing decisions in BFT height order, each after the bc-blocks up to its
headers, satisfies every dependency.

## 8. Implementation status and pitfalls

The ordered implementation work is in [`IMPLEMENTATION.md`](./IMPLEMENTATION.md). This section
records the current-tree facts that work starts from.

**Current tree.** The final-block accessor returns only the stored Crosslink value and never
substitutes the legacy reorg-depth marker; `tfl_reorg_final_block_height_hash` and
`tfl_final_block_height_hash_pre_locked` no longer exist. Before Crosslink produces a value,
finality queries return `None`. No regression test covers the absent and present cases, and
there is no test harness for these RPC methods; the cases are covered at the `ReadStateService`
request level instead, and a JSON-RPC harness remains separate work. `set_final_block` publishes every marker write
on `FinalBlockRx`, before state commitment.

**Current tree.** `latest_final_block` is written from the same local that the state request is
sent, so it is a *record of* what was force-finalized rather than an input to it; its actual
readers are the BFT proposal path and the visualizer. It has no
`candidate` computation, no monotonicity guard, and no `bc_best` update trigger, so naming it
`local_finalized_tip` before those exist would assert a CL2 quantity the code does not
implement. In Zebra Crosslink its readers take the persisted `fin`.

### 8.1 Implementation pitfalls

These are current-tree facts, and they hold for any change to how the marker is derived,
stored, or consumed.

- **The derivation has one accessor.** `BftBlock::snapshot_block_hash` is the only place
  `parent(headers[0])` is computed. `BftRunner::decide`, the BFT validation path,
  the PoS-store restore path and its replay watermark `prev_finalized_bc_height`,
  `test_format.rs`, and `viz2.rs` (both the live viz response and `VizScene`) read it, and the
  finality-diagram tests in `zebrad/tests/crosslink.rs` and `viz2::scene_tests` assert marker
  positions derived from it. A second derivation makes the node, the GUI, and the tests
  disagree about which block is final.
- **The roster is a function of a block, not of committing it.** `BftChain::roster`
  is filled from `FinalizedState::db::aggregated_stakes` at `snapshot(B_{H−1})`, on the decide
  path and on the PoS-store restore path alike, and `terminated_finalizers_at` takes that same
  block's height. Nothing reads stakes from the reply to `handle_crosslink_finalize`, which now
  returns the hash alone, so the roster no longer depends on a decision and a commit being one
  event. The read goes to the finalized database, so it answers for committed blocks only:
  non-finalized chains keep just their tip's bond state (`Chain::delegation_bonds`). That is
  sufficient while the decide path commits the snapshot it just decided. Once it does not
  (§4.3), the aggregate is carried per block in `Chain`, beside `bond_rewards` and
  `finalizer_commissions` and popped with them, and it is the same vector
  `prepare_aggregated_stakes_batch` writes on commit, so the roster read is a lookup on either
  side of a finalized tip. Forking the chain that holds the block and reading its unwound bond
  state is the slower equivalent and yields the same aggregate. A block on no chain the node
  holds is the case the second state of §4.3 removes.
- **The BFT genesis snapshot is below the bootstrap roster height.** Bootstrap genesis
  carries headers starting at `BOOTSTRAP_ROSTER_HEIGHT`, so its snapshot, and the roster for
  BFT height 1, is the block below that height.
- **A derivation change is a network-wide consensus change.** Nodes running two derivations
  disagree on the finalized block and on the roster. PoS stores and databases written under
  the old derivation are deleted, not migrated. A PoS-store record
  holds the `BftBlock`, the fat pointer, `BftChain::roster`, and the proposal
  signatures; restore reads the roster bytes back verbatim and recomputes the replay watermark
  `prev_finalized_bc_height` from each stored block's snapshot, so an old store loaded by new code yields
  rosters that disagree with the stored votes, which travel by roster index.
- **`fin` moves only forward.** `candidate(bc_best)` falls below `fin` after a benign reorg
  (§3.2), and `WriteBlockWorkerTask::handle_crosslink_finalize` returns success for a hash the
  database already holds, so a caller that stores whatever it committed can move `fin`
  backwards. The `fin ⪯ N` check belongs at the caller.
- **`fin` is written no earlier than its commit.** A persisted `fin` above the finalized tip can
  name a block that was only in non-finalized state, which does not survive a restart. Writing
  `fin` in the commit's batch, or after it, keeps `fin` at or below the finalized tip.
- **The database finalized tip is not `fin`.** Past `MAX_BLOCK_REORG_HEIGHT` of lag it is the
  reorg-depth commit. RPC, GUI, and notification readers take the persisted `fin`.
- **Last Final Snapshot constrains block templates.** A template must cite a bft-block whose
  snapshot lies on the template's parent chain, or the mined block is invalid (§6.2).
- **Tail Confirmation needs the whole tail.** Validation checks all `σ` carried headers and the
  bc-validity of their blocks, which a validator may first have to download (§3.4).
- **A `σ` window is a chain, and its top must be present.** Enforcing Tail Confirmation
  invalidated two fixtures in `crosslink_test_basic_finality` that predate it: a window taken as
  a slice of a block list that holds a fork carried two siblings rather than a chain, and the
  last window's topmost header named a block the test never loaded, which now defers with
  `NeedsBlock` instead of validating. Any test that builds a certificate from a slice has to
  take its blocks from one branch and load the block at the top of the window.
- **Reward logic has three copies.** `Chain::push` with `update_bonds_with_pos_issuance`,
  `fixup_aggregated_stakes` in `stake_fixup.rs`, and the wallet projection in `lib.rs` must
  change together (§5.4).
- **Header order is a property of the honest producer.** `BftBlock::try_from` checks only the
  header count, and the network and PoS-store deserialization path does not call it. Code that
  reads `headers[0]` as the deepest header relies on the producer, not on validation.
- **`NonFinalizedState` holds less than the finalized chain needs.** It lives in memory, it drops
  the lowest-work chain past `MAX_NON_FINALIZED_CHAIN_FORKS` (10), and it drops chains that do
  not contain the finalized tip, including those forking below a reorg-depth commit. The chain to
  `bft_final_snapshot` must survive all three. Its BFT decisions are database rows beside the
  bc-chain (§7.1); the chain holding `bft_final_snapshot` is exempt from the lowest-work pruning
  and is restored by the non-finalized backup; and a fork below the depth commit is the second
  state of §4.3, never a reason to stop following the chain.
- **A conflict below the depth commit is a second database, not a resync.** The Wall of Death
  (`410d99ed`) was this failure with no conflict in it: a decision's snapshot lay on the
  committed chain, deep, and the roster could not be read there, so a node that had mined past it
  could never resume BFT. Storing aggregated stakes per committed block fixed that, and the
  resyncs it had forced on feature-testnet operators are what the fix existed to end. The
  conflict case is the same failure one branch over, and the same answer applies: the node
  recovers from what it already holds, without an operator.
- **Switching to the BFT branch can be a reorganization deeper than `MAX_BLOCK_REORG_HEIGHT`.**
  The wallet's `REWIND_DISTANCE` and `CHECKPOINTS_N` derive from that constant, so a client of a
  node that can switch to a second state needs checkpoints back to `fin`.
- **A switch onto the finalized chain replaces bond state.** Bonds tracked along that chain while
  it was a side chain must agree with what the chain produces once it becomes `bc_best`; one
  implementation of the bond update serves both (§5.4).
- **The decided bft-chain is stored twice.** It is held in memory and appended to the PoS store
  file, whose records carry the roster as well as the block, the fat pointer and the proposal
  signatures. Restore reads those roster bytes back rather than recomputing them from bonds,
  which is the second derivation the first bullet of this list warns against, and it unwraps a
  state lookup that a database behind the file makes fail (§5.5). One store, with the roster
  recomputed, removes all three.
- **`MAX_BLOCK_REORG_HEIGHT` is asserted against the bootstrap gap.**
  `ZcashCrosslinkParameters::bootstrap_is_valid` requires
  `activation_height − roster_height > MAX_BLOCK_REORG_HEIGHT`, and a `const _: () = assert!` on
  `PROTOTYPE_PARAMETERS` checks it while compiling. The prototype gap is 200 blocks, so raising
  the constant past that stops the workspace building until the bootstrap heights move with it.
- **Aborts kill the node.** The build uses `panic=abort`. The decide path unwraps
  `block_height_from_hash` on the decided header, so a decided block whose header is unknown to
  state terminates the process, as does every `assert!` on that path.
- **`fin` is a time series, not a function of the tip (§3.2).** Recomputing it from
  `candidate(bc_best)` after a restart reproduces only the current candidate, which is why `fin`
  is persisted.
- **Zebra's depth commit is a second floor, per state.** Blocks deeper than
  `MAX_BLOCK_REORG_HEIGHT` on the best chain are written to the PoW state's finalized database
  regardless of `fin` (§4.3, Implementation in Zebra), and a fork-choice rule above `fin`
  operates only within that window in one database. A second state is what lets the rule reach
  past it; holding the commit back is not, because the hold has to end.
- **The `+40` candidate clamp breaks honest proposal (§6.2).** It stays, as a design heuristic
  outside the specification (§3.4). With it, one bft-block's snapshot advances by at most 40 bc-blocks; the commit, the
  roster lookup, and `terminated_finalizers_at` handle steps of any size regardless.
- **Block status never reports `bft_final_snapshot` as finalized.** A block at or below
  `bft_final_snapshot` but above `local_finalized_tip` is `InBestChain` or `NotInBestChain`,
  because `fin` is the view Assured Finality covers (§2).
- **`σ` comes from `ZcashCrosslinkParameters`.** The GUI's `apply_viz_op` hardcodes it as
  `TMP_SIGMA`, which matches only while `PROTOTYPE_PARAMETERS` is unchanged.
- **Last Final Snapshot has no node test, and cannot have one here.** A violation needs a block
  whose ancestry omits the snapshot of the bft-block it cites. The decide path finalizes every
  snapshot as it is decided (`BftRunner::decide` → `handle_crosslink_finalize`), and the
  state then collapses onto that snapshot's branch and refuses forks below it (§6.3), so by the
  time such a block could be offered its parent is gone and it is refused for the wrong reason.
  The rule is still enforced, and it is what makes the σ carried headers confirmations of the
  admitting chain; testing it needs the decoupling of decisions from commits, not a new test.
  Linearity and Tail Confirmation are tested, in
  `crosslink_reject_pos_block_that_regresses_the_snapshot`,
  `crosslink_reject_pos_block_with_lt_sigma_headers` and
  `crosslink_reject_pos_block_with_unlinked_headers`.
- **Crosslink node tests and `viz_gui`.** Tests run through `phest.bat zebra-crosslink`, and
  `phargo.bat` enables `viz_gui` for that project, which puts winit on the main thread. The
  node tests in `zebrad/tests/crosslink.rs` run headless, so they run with `PH_NO_VIZ_GUI` set,
  which leaves the feature out of an otherwise identical build.

## 9. Open decisions

Payout design belongs to separate work, recorded here for context. Implementation questions
that need a design pass are in [`IMPLEMENTATION.md`](./IMPLEMENTATION.md).

### 9.1 Objective reward trigger and reward economics

Consensus issuance cannot depend on node-local `fin`. Honest nodes can reach the same chain
through different best-chain and reorg histories, so they need not observe the same sequence
of `fin` transitions. They must nevertheless compute identical value pools for the same
chain.

An objective per-block event can instead be derived from block data, for example:

```text
payout boundary at H  iff  candidate(H) != candidate(parent(H))
```

**Implemented.** The prototype now takes this trigger, with a liveness bound added to it:
a block pays iff its certificate differs from its parent's *and* the certificate is at most
`σ + FINALITY_LIVENESS_ALLOWANCE` blocks behind it. §5.4 states the rule and where each path
evaluates it. The consequences listed below under "payout amount" are the ones this choice
accepts: a flat reward per advance, so a BFT stall lowers issuance for as long as it lasts and
never pays the missed blocks back.

This is not literally the event "local `fin` advanced." It is a block-local event that would
permit `fin` to advance if `H` were observed as best and its candidate were ahead of that
node's current `fin`. `snapshot(LF(H))` is another objective candidate source. The selected
function must be monotone along a chain under the enforced validity rules. Extension makes
`LF(H)` monotone along a chain; Linearity then makes `snapshot(LF(H))` monotone, and
`candidate(H)` follows because `prune_σ(H)` is monotone and an lca of two monotone arguments is
monotone. The current tree enforces both Extension and Linearity (§6.2). Without a Finality
Depth rule, a bc-block producer can also keep a stale `context_bft` at no validity cost (§3.3), so an
objective advance trigger lets whoever dominates `bc_best` delay payouts while `Π_bft` is live.

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

## 10. Source appendix

- [Original TFL Book: `candidate`, `fin`, and syncing](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/construction.md#L423-L562);
  the same range defines the omitted `ba_μ`
- [Original TFL Book: parameters `σ`, `L`, `μ`, and Stalled Mode](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/construction.md#L386-L415),
  of which only `σ` is used here
- [Original TFL Book: liveness argument](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/security-analysis.md#L5-L58),
  including the note that the Finality Depth rule can be omitted
- [Original TFL Book: the arguments for bounded availability](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/the-arguments-for-bounded-availability-and-finality-overrides.md),
  the case for the design this tree does not adopt
- [Original TFL Book: BFT validity rules](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/construction.md#L568-L574)
- [Original TFL Book: bc validity and honest production](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/construction.md#L680-L717)
- [Original TFL Book: fork-choice question](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/questions.md#L11-L52)
- [Original TFL Book: Linearity and Last Final Snapshot rules combined](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/potential-changes.md#L320-L371),
  including the note that the Questions argument predates Linearity
- [Original TFL Book: what the Linearity rule does](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/construction.md#L587-L604), the
  informal safety sketch for both rules
- [Original TFL Book: liveness contrast with Casper FFG](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/security-analysis.md#L15-L23) and
  [the status of the safety argument](https://github.com/daira/tfl-book/blob/fe6e1d6f403f62da46c64e8f5a7db3cb188ffae2/src/design/crosslink/security-analysis.md#L52-L54)
- [Shielded Labs warning about the adapted construction](https://github.com/ShieldedLabs/zebra-crosslink/blob/6d02a1b80f896d08f923e39b2505f0565efb5787/book/src/design/cl2-construction.md#L1-L14).
  Protocol definitions above are cited separately from the original pinned source.

Every current-tree statement describes the monolith tree this document ships with. Code can move without this file being updated, so re-check the cited symbols before
using this document to plan changes.
