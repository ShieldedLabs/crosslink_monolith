# Crosslink finality semantics and Zebra policy boundaries

This document separates the three Crosslink 2 protocol quantities that Zebra's design retains
from Zebra's irreversible state-commit boundary, legacy reorg-depth fallback, and
consumer-specific meanings of "final". It records the implementation as of this revision of
the repository, describes the behavior the implementation is being changed to (sticky fork
choice, persisted `fin`, and every CL2 validity rule), and identifies the decisions that remain
open.

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

The prototype sets `σ = 3` in `librustzcash/zcash_primitives/src/bft.rs`
(`PROTOTYPE_PARAMETERS`). The source code explicitly warns that this value has not been
verified as secure or performant. The same struct also carries `finalization_gap_bound: 7`, the
Book's `L`. It has no protocol meaning in this design: only test formatting reads it, and its
doc comment still describes Stalled Mode activation.

### Notation

`prune_k(C)` means `C` with its last `k` blocks removed, with genesis as the floor. It is the
plain-text spelling of the Book's `C ⌈bc^k`. `A ⪯ B` means that `A` is an ancestor of or equal
to `B`; `A` and `B` conflict when neither is an ancestor of the other.

The two chains have their own parent links. They also contain two cross-chain references:

- each bc-block `H` has `H.context_bft`, which commits to a bft-block; and
- each non-genesis bft-block has `headers_bc`, exactly `σ` bc-headers in deepest-first order.

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

- a **canonical-finalized policy point**, proposed here as `canonical_finalized_tip`, that
  makes only chains containing that point eligible for local activation;
- a **physical database-commit boundary**, the finalized database's tip, which advances only
  after the finalized-state write has succeeded; and
- a **legacy reorg-depth marker**, roughly `tip − MAX_BLOCK_REORG_HEIGHT`, formerly substituted
  by the finality RPCs when no Crosslink marker existed. That substitution has been removed and
  the marker now has no consumer, but the quantity remains distinct from the three above and is
  listed here so that it is not reintroduced under a Crosslink name.

Protocol `fin` and these Zebra quantities must not share an undocumented storage slot. In raw
CL2, `fin` can remain fixed on a branch that raw `bc_best` no longer contains. In the current
Zebra implementation, irreversible state commitment instead enforces
`canonical_finalized_tip ⪯ canonical_tip` locally. That is an additional chain-activation and
state policy.

Under sticky fork choice (§4.3) the policy floor is `fin` itself, so `canonical_finalized_tip`
and `fin` are one quantity. Two stored values remain: `fin`, persisted in the finalized database
as its own block hash, and the database's finalized tip, which is the higher of `fin` and the
block Zebra commits at reorg depth. The finalized tip equals `fin` while finality lags the
best tip by less than about `MAX_BLOCK_REORG_HEIGHT` blocks.

## 3. Crosslink 2 model

### 3.1 `snapshot`, `LF`, and `candidate`

```text
snapshot(B)  := O_bc                         if B.headers_bc = ∅
             := parent(B.headers_bc[0])      otherwise
LF(H)        := bft-last-final(H.context_bft)
candidate(H) := lca(snapshot(LF(H)), prune_σ(H))
```

The walk is `bc → bft → bft → bc`, followed by the last-common-ancestor clamp.

`bft-last-final(B)` is the last final ancestor of `B`, `B` included. In Zebra, `Π_bft` decides
each bft-block individually, and a decided block is final. A bc-block's `context_bft` is a fat
pointer, and a node resolves it only against `TFLServiceInternal::bft_blocks`, whose entries are
all decided; a pointer that does not resolve defers the bc-block (§6.2, Extension). Every
context a node accepts is therefore final, and `bft-last-final` is the identity on them, so
`LF(H)` is the bft-block that `H.context_bft` points at.

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

In the current Zebra prototype, the finalized-prefix policy of §4.2 locally forces
`canonical_finalized_tip ⪯ canonical_tip`, and sticky fork choice (§4.3) keeps `fin ⪯ bc_best`.
Either restores, as a chain-selection policy, a prefix relation that `ba_μ` provided by
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

Zebra Crosslink implements all five rules above (§6.2 lists where the current tree does not
yet).

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

**Linearity and bc reorganizations.** Let `B` be the newest final bft-block. Linearity requires
every later final snapshot to extend `snapshot(B)`. When a node's `bc_best` reorganizes onto a
branch that forks below `snapshot(B)`, the tail of that branch fails Linearity, so honest
proposers repeat `B.headers_bc`. Last Final Snapshot admits a block `H` on that branch only if
`snapshot(LF(H))` lies on the branch, so `H` cannot cite `B` or any later final bft-block, and
`candidate(H)` stays at or below the fork point. Finality for nodes on that branch resumes when
a chain containing `snapshot(B)` becomes their best chain again. Under honest proposal at every
bc-block, `snapshot(B)` sits about `σ` blocks below the proposer's tip, so a reorganization
slightly deeper than `σ` reaches this case.

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
This is an unimplemented sync-safety recommendation, not a block-validity or consensus rule. The
Book applies it to `fin` and `ba_μ`; here it covers `fin` only, and says nothing about exposing
`bc_best`.

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

Current Zebra's `CrosslinkFinalizeBlock` behavior is stronger still: it commits database state
on the named branch and discards incompatible non-finalized branches. This makes the policy
physical. The CL2 construction does not mandate it. §4.3 specifies the rule it becomes when the
floor is `fin`.

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
condition, and hold wherever protocol `fin` is computed, which this tree does not do yet
(§6.1).

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
abstract outcome in FINALITY_DIAGRAM §4 does. Statements under *Without Linearity* apply to the
current prototype, which enforces neither Linearity nor Last Final Snapshot (§6.2).

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
    heavier, finality stays stalled until B's branch is abandoned.
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

#### Implementation in Zebra

The rule is implemented through the finalized database rather than as a separate chain filter:

- On every change of `bc_best`, the node computes `N := candidate(bc_best)`. If `fin ⪯ N` and
  `N ≠ fin`, it commits `N` through `CrosslinkFinalizeBlock` and then stores `N` as `fin`. A
  candidate at or below `fin` changes nothing.
- The commit discards every non-finalized chain that does not contain `N`, and Zebra rejects
  blocks that fork below its finalized tip. Chains that exclude `fin` therefore never enter the
  node's view, which is the switch condition above. That rejection is the refused switch; the
  node reports it on stdout, and a persisted hazard record is future work.
- `fin` is stored in the finalized database as its own block hash, so the floor survives a
  restart. The database's finalized tip is the higher of `fin` and the reorg-depth commit (next
  bullet), so finality readers take `fin`, never the finalized tip.
- Zebra also commits the root of the best chain to the finalized database once the chain is
  longer than `MAX_BLOCK_REORG_HEIGHT` (99, from `zcash_protocol::consensus`, applied in
  `zebra-state/src/service/write.rs`). Chains forking below that point are no longer in view.
  On a Zebra node the effective floor is the higher of `fin` and that depth-committed block.

Sticky fork choice and Linearity constrain different points. Sticky fork choice keeps `fin` on
`bc_best`; Linearity keeps each final snapshot on or after the previous one. `fin` lies at or
below the newest final snapshot, so a reorganization that forks between the two is admitted by
sticky fork choice and then leaves finality on the new branch waiting (§3.4, Linearity and bc
reorganizations).

Current tree:

- The rule needs protocol `fin`, which this tree does not compute (§6.1). The current collapse
  onto a BFT-decided branch (§4.2, §6.3) is a related rule with a different floor: the stored
  marker, taken directly from a decided BFT block when it is decided rather than from
  `candidate(bc_best)`. With that floor, the invariant above does not follow: the marker need not
  lie on the node's best chain when it advances, and a known side-chain hash becomes canonical
  (§5.2).
- The prototype enforces neither Linearity nor Last Final Snapshot (§6.2), so the
  *Without Linearity* outcomes above are the ones that apply to it.

## 5. Zebra implementation inventory

This inventory refers to symbols in the tree this document ships with; symbol names are
preferred over brittle working tree line numbers.

### 5.1 The overloaded marker and write paths

`zebra-crosslink/zebra-crosslink/src/lib.rs` defines
`TFLServiceInternal::latest_final_block: Option<(ZebBlockHeight, ZebBlockHash)>`. It is assigned
only by `set_final_block`, which also sends the new value on `final_change_tx`. Its callers are:

- `handle_new_decided_bft_block`, after inserting the BFT block;
- `tfl_service_main_loop`, when restoring the last entry from the PoS store; and
- `tfl_set_finality_by_hash`, through the testing/service setter.

The live path computes the hash of `new_block.headers[0]`: the first header itself, rather than
`snapshot(new_block) = parent(new_block.headers[0])`.

When this marker is absent, `tfl_final_block_height_hash` returns `None`. It previously
substituted a Zebra reorg-depth location derived from the state block locator, so that the API
changed semantics depending on whether Crosslink had produced a value; that substitution and
its helper have been removed. Removing it was safe because the substitution reached only three
readers, all RPC-facing: `tfl_block_finality_from_height_hash`, the `FinalBlockHeightHash`
service request, and the `TxFinalityStatus` service request. Every consensus-, state-, and
GUI-side reader takes `internal.latest_final_block` directly and never saw the substituted
value.

`TFLServiceInternal::current_bc_final` is initialized in
`zebra-crosslink/zebra-crosslink/src/service.rs` and assigned during PoS-store startup in
`tfl_service_main_loop`. No other read or write was found. It is currently unused duplicate
state.

### 5.2 Irreversible commitment and ordering

`handle_new_decided_bft_block` assigns and publishes `latest_final_block` before it sends
`zebra_state::Request::CrosslinkFinalizeBlock`. The request is retried indefinitely after an
error. During that interval, RPC and GUI readers and notification subscribers can observe a
marker whose database state has not been finalized.

The state behavior depends on whether the hash is known:

- `new_network` accepts a hash found in any non-finalized chain or in the finalized database.
  `NonFinalizedState::crosslink_finalize` retains the chain containing a known side-chain hash,
  so finalizing that hash can make the side chain canonical before blocks are committed by
  `WriteBlockWorkerTask::handle_crosslink_finalize`.
- a hash the state does not know never reaches the request. `handle_new_decided_bft_block`
  first asserts that `validate_bft_block` passes, and validation returns `Indeterminate`
  (`NeedsBlock`) when `KnownBlock` cannot resolve `headers[0]`, so the assertion panics and,
  under `panic = abort`, the node exits. The retry loop runs only for a hash known at that
  point; if the chain holding it is then dropped from the non-finalized state before the request
  succeeds, the loop can retry indefinitely.
- the PoS-store restore path unwraps the same `KnownBlock` lookup for the last stored BFT block,
  so a finalized database that is behind the PoS store, for example one wiped and re-syncing,
  panics at startup. The replay-watermark loop just above it tolerates that case.

Consequently, the stored marker is neither a reliable `fin` implementation nor a reliable
record of the finalized database's tip. Persisted `fin` advances only after the state request
succeeds, and only by the CL2 update rule.

### 5.3 Consumers

The overloaded value currently reaches:

- irreversible state commitment through `CrosslinkFinalizeBlock`;
- `finalizers_at_current_height`, using the aggregate stakes returned by state finalization;
- hardfork finalizer filtering through `terminated_finalizers_at`;
- `get_tfl_final_block_*`, block-finality, and transaction-finality RPC methods;
- the GUI's finalized row, terminated-finalizer display, and visualization paging lower bound;
- BFT proposal and validation paths; and
- the main-loop finality-gap diagnostic.

"Reaches" above is deliberately loose, and the distinction matters when planning a rename or a
change of derivation. The actual reads of `internal.latest_final_block` are only: the BFT
proposal path, the main-loop diagnostic, and three sites in `viz2.rs` (the paging lower bound,
the `terminated_finalizers_at` height input, and the GUI finalized tip). The others receive the
same value by another route rather than by reading the slot:

- `CrosslinkFinalizeBlock` is sent the local `new_final_hash`; the field is written from that
  same local immediately before. Deleting the field would not change its behavior.
- `finalizers_at_current_height` is a write target, populated from the aggregated stakes that
  the `CrosslinkFinalizeBlock` call returns.
- `terminated_finalizers_at` is passed a local height at three of its four call sites; only the
  `viz2.rs` site reads the field.
- The BFT validation path's read is dead: `already_finalized_hash` is captured and then
  discarded by `let _ = already_finalized_hash;`, because the queue re-flush it once served has
  been removed.

By actual reads, the widest consumer of the slot is the visualizer, not consensus.

`TFLServiceRequest::FinalBlockRx` returns subscribers to `TFLServiceInternal::final_change_tx`,
and the RPC notification methods in `zebra-crosslink/zebra-rpc/src/methods.rs` wait on them.
Every `set_final_block` call sends on that channel, so a notification carries the same
overloaded value at the same moments: on the decide path before `CrosslinkFinalizeBlock`
succeeds, on PoS-store restore, and through the testing setter.

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
  than its parent. The proposal path picks a candidate height, then issues `FindBlockHeaders`
  with that block as the sole known hash. That request is specified to return the headers
  *following* the intersection, ascending, and the implementation iterates an ascending range
  from `intersection + 1`, so `headers[0]` is the block one above the candidate height and
  `parent(headers[0])` is the candidate height itself. Storing `hash(headers[0])` therefore
  finalizes one block shallower than intended.
  The in-memory header order is consequently deepest-first, matching the specification, so the
  `BftBlock` doc comment in `librustzcash/zcash_primitives/src/bft.rs` claiming the order is
  reversed from the specification was not merely stale but inverted. Nothing enforces that
  order: `BftBlock::try_from` checks only the header count and logs that its documented
  validations are unimplemented, and the deserialization path used for network and PoS-store
  blocks does not call `try_from` at all. Deepest-first is a property of the honest producer,
  not of the type.
- **The candidate height is clamped, and the clamp is not `prune_σ`.** The proposal path
  computes `tip − σ` and then takes
  `min(tip − σ, latest_final_block + 40)`. Only when that clamp does not bind is the stored
  marker `prune_σ(tip) + 1`, i.e. `σ − 1` confirmations — two rather than three under
  `PROTOTYPE_PARAMETERS`. Whenever `tip − σ > marker + 40`, which is the normal regime during
  catch-up after a restart or a BFT stall, the candidate is `marker + 40` and the block is
  finalized far deeper than `σ`. Any statement of the form "the proposal path finalizes at
  `tip − σ`" is true only in the unclamped regime.
- **Four sites derive the marker from `headers.first()`**, not three: the decide path, the BFT
  validation path, the PoS-store restore path, and — separately — the historical replay
  watermark `prev_finalized_bc_height` computed during restore. `BftBlock::finalization_candidate()`
  is a fifth accessor with the same convention, and `viz2.rs` repeats the derivation for the GUI
  and for `VizScene`. The replay watermark is the one that makes a change of derivation costly;
  see §9.2.
- **The improvement test encodes the same convention.** `is_improved_final` compares
  `proposed_final_height`, the anchor plus one, against the stored marker, so every new PoW
  block is proposable at once. That `+ 1` is the `headers[0]` convention and changes with it.
  The test runs before the `+40` clamp. The clamp can only lower the anchor to `marker + 40`,
  so it never turns an admitted proposal into a non-improving one.
- **Missing monotonicity and hazard record.** All marker writes are unconditional. There is no
  `fin ⪯ candidate` guard and no distinction between a benign candidate regression and a
  conflicting-candidate safety incident.

### 6.2 Missing validity rules

- The Last Final Snapshot rule is not implemented for bc-block admission.
- The Finality Depth rule and Stalled Mode are omitted by design (§3.3).
  `finalization_gap_bound` is read only by test formatting, and the 512-block log threshold is
  diagnostic, not consensus.
- BFT validation does not implement Linearity or Tail Confirmation. It checks that the first
  carried header's block is locally present, but does not establish that all `σ` headers form a
  valid chain with valid PoW.
- The proposal path departs from honest proposal (§3.4) in two ways. When the `+40` candidate
  clamp in §6.1 binds, `headers_bc` is a window ending at `marker + 40 + σ`, not the tail of the
  proposer's `bc_best`; the window still satisfies Tail Confirmation. When `is_improved_final`
  fails, the path makes no proposal, where honest proposal repeats the parent's `headers_bc`.
- Bc-block production does not follow the honest context-selection procedure (§3.4): the block
  template's `FatPointerToBFTChainTip` request cites the newest decided bft-block whose
  `do_not_include_until_bc_height` admits the proposed height, whatever that block's snapshot. Under Last Final Snapshot a template whose parent chain does not contain that
  snapshot produces an invalid block.
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

- protocol `local_finalized_tip` (`fin`), which under sticky fork choice is also the Zebra
  policy floor `canonical_finalized_tip` (§2, §4.3); and
- the database's finalized tip, the higher of `fin` and the reorg-depth commit.

### 6.4 Unbounded finality gap

Nothing in consensus bounds the finality gap or restricts which transactions a block far past
the snapshot may carry. This follows from omitting Stalled Mode (§3.3). It has two practical
consequences:

- The diagnostic warning at a hardcoded gap is the only signal of a long finalization stall.
  Any response to one, such as alerts, wallet warnings, or operator action, is outside
  consensus.
- A best chain that has forked below `fin` (§3.5) can carry ordinary spending transactions for
  as long as it dominates. Under raw CL2 fork choice nothing limits that activity. In the
  current prototype, only the finalized-prefix policy of §4.2 keeps an enforcing node from
  activating such a branch.

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
- `current_bc_final` is unused duplicate state.

## 7. Proposed names and consumer decision matrix

The protocol names should encode their definitions:

| protocol quantity | value identifier | optional newtype |
|---|---|---|
| `bc_best` | `bc_best_tip` | `BcBestTip` |
| `candidate(H)` | `finalization_candidate` | `FinalizationCandidate` |
| `fin` | `local_finalized_tip` | `LocalFinalizedTip` |

The database's finalized tip needs a name distinct from `fin` (§6.3). The legacy reorg-depth value should keep a name that says it is a
reorg-depth marker, not Crosslink finality.

No protocol view lies between the best tip and the finalized tip, so no CL2 quantity is a
default for "confirmed" presentation. Each consumer needs a contract:

| consumer or endpoint | value | contract or unresolved work |
|---|---|---|
| raw best-tip display | `bc_best_tip` | current fork-choice result |
| confirmed display | `bc_best_tip` at a stated confirmation depth | `Π_bc` confirmation only; can be an ancestor of `local_finalized_tip`, or conflict with it after a Prefix Consistency failure (§3.3); never present it as final |
| final display | `local_finalized_tip` | node-local monotone CL2 view |
| `get_tfl_final_block_hash` and `get_tfl_final_block_height_and_hash` | `local_finalized_tip` | partly implemented: they now return no value when the marker is absent, but when present it is still the legacy-fed slot, and the checkpoint/recency exposure condition of §6.5 does not exist |
| block/transaction status | unresolved API contract | define distinct `Confirmed` and `Finalized` states before routing either |
| finality-change notifications | `local_finalized_tip` transitions | publish only after the chosen public-finality contract is met |
| visualization paging | operational paging cursor | do not overload a finality value merely to bound a window |
| canonical state activation | `fin` | sticky fork choice floor (§4.3) |
| physical database status | database finalized tip | higher of `fin` and the reorg-depth commit; never reported as Crosslink finality |
| staking rewards | objective per-block source | never use node-local `fin`; see §9.1 |
| validator roster and hardfork membership | bonds at `snapshot(B_{H−1})` | objective; see below |

### Consensus-sensitive roster and hardfork inputs

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
`fin` advances only once a bc-block citing the bft-block is best. Its bonds are therefore
usually read from a non-finalized chain. An honest validator has downloaded that chain while
validating `B_{H−1}` (§3.4).

## 8. Minimal code slice after the decisions

The first implementation patch should expose the semantic split without claiming that legacy
writers already implement CL2:

1. **Done.** Make the final-block accessor return only the stored Crosslink value; do not
   substitute the legacy reorg-depth marker. `tfl_reorg_final_block_height_hash` and
   `tfl_final_block_height_hash_pre_locked` then have no callers and were deleted with it.
2. Store `fin` in the finalized database as its own block hash, updated only after
   `CrosslinkFinalizeBlock` for that hash succeeds (§4.3).
3. **Done in part.** `set_final_block` publishes every marker write on `FinalBlockRx`. The send
   sits at the marker write, before state commitment and without the public-finality contract
   of §7; it moves to the documented transition point once that point is chosen.
4. Rename the main-loop `current_bc_tip` local to `bc_best_tip`.
5. Rename `latest_final_block` and document what feeds it.

The accessor change is observable: before Crosslink produces a local finalized value, finality
queries return `None` rather than labelling a Zebra reorg-depth point as Crosslink finality. The
regression test covering the absent and explicitly present cases has **not** been written; there
is no test harness for these RPC methods.

Step 5 depends on step 2 and on the update trigger of §4.3. The slot is written from the same
local that the state request is sent, so it is a *record of* what was force-finalized rather
than an input to it; its actual readers are the BFT proposal path, the main-loop diagnostic, and
the visualizer. It has no `candidate` computation, no monotonicity guard, and no `bc_best`
update trigger, so naming it `local_finalized_tip` before those exist would assert a CL2
quantity the code does not implement. Once they exist, its readers take the persisted `fin`.

The behavior changes that follow the mechanical steps are:

- compute `candidate(bc_best)` and advance `fin` on every `bc_best` change, which implements
  sticky fork choice (§4.3);
- derive `snapshot(B)` as `parent(B.headers_bc[0])` at every site listed in §8.1;
- enforce Last Final Snapshot, Linearity, and Tail Confirmation, and follow honest proposal and
  honest context selection (§3.4, §6.2);
- read the validator set for BFT height `H` from the bonds at `snapshot(B_{H−1})` (§7); and
- report a refused switch on stdout (§4.3).

### 8.1 Implementation pitfalls

These hold for any change to how the marker is derived, stored, or consumed.

- **The derivation is duplicated.** `hash(headers[0])` is computed independently in
  `handle_new_decided_bft_block`, the BFT validation path, the PoS-store restore path, the
  restore replay watermark `prev_finalized_bc_height`, `BftBlock::finalization_candidate()`,
  `test_format.rs`, and `viz2.rs` (both the live viz response and `VizScene`). The
  finality-diagram tests in `zebrad/tests/crosslink.rs` and `viz2::scene_tests` assert
  marker positions derived the same way. A change to one site without the others makes the
  node, the GUI, and the tests disagree about which block is final.
- **The roster is consensus data reached through the marker.** `finalizers_at_current_height`
  is the aggregated stake set that `CrosslinkFinalizeBlock` returns for the marker hash, and
  `terminated_finalizers_at` takes the marker height. That is objective today only because
  `Π_bft` agreement fixes the hash. Once the commit target is `candidate(bc_best)`, the stakes
  that call returns are node-local, so the validator set reads the bonds at
  `snapshot(B_{H−1})` through its own lookup, which also covers non-finalized chains (§7).
- **The BFT genesis snapshot moves.** Bootstrap genesis carries headers starting at the
  activation height, so under `parent(headers_bc[0])` its snapshot, and the roster for BFT
  height 1, is one block below that height.
- **A derivation change is a network-wide consensus change.** Nodes running two derivations
  disagree on the finalized block and on the roster. Existing PoS-store records carry roster
  bytes computed under the old derivation and are read back verbatim (§9.2).
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
- **Reward logic has three copies.** `Chain::push` with `update_bonds_with_pos_issuance`,
  `fixup_aggregated_stakes` in `stake_fixup.rs`, and the wallet projection in `lib.rs` must
  change together (§5.4).
- **Header order is a property of the honest producer.** `BftBlock::try_from` checks only the
  header count, and the network and PoS-store deserialization path does not call it. Code that
  reads `headers[0]` as the deepest header relies on the producer, not on validation.
- **The BFT service lock must be released before any state request.** zebra-state can call back
  into the Crosslink service during `CrosslinkFinalizeBlock`. An update trigger on `bc_best`
  changes adds a state-to-Crosslink call path with the same reentrancy constraint.
- **Aborts kill the node.** The build uses `panic=abort`. The decide path unwraps
  `block_height_from_hash` on the decided header, so a decided block whose header is unknown to
  state terminates the process, as does every `assert!` on that path.
- **`fin` is a time series, not a function of the tip (§3.2).** Recomputing it from
  `candidate(bc_best)` after a restart reproduces only the current candidate, which is why `fin`
  is persisted.
- **Zebra's depth commit is a second floor.** Blocks deeper than `MAX_BLOCK_REORG_HEIGHT` on the
  best chain are written to the finalized database regardless of `fin` (§4.3, Implementation in
  Zebra). Any fork-choice rule above `fin` operates only within that window.
- **The `+40` candidate clamp breaks honest proposal (§6.2).** Without it, one bft-block's
  snapshot can advance by any number of bc-blocks, so the commit, the roster lookup, and
  `terminated_finalizers_at` each handle steps of any size.
- **`σ` comes from `ZcashCrosslinkParameters`.** The GUI's `apply_viz_op` hardcodes it as
  `TMP_SIGMA`, which matches only while `PROTOTYPE_PARAMETERS` is unchanged.
- **Removing `finalization_gap_bound` changes the test format.** `test_format.rs` serializes it
  as the second parameter value, so existing `.zeccltf` files in `crosslink-test-data` need
  regenerating or a compatible reader.
- **Crosslink node tests run without `viz_gui`.** Every node test in `zebrad/tests/crosslink.rs`
  panics in winit when that feature is enabled, and `phargo.bat` enables it, so those tests run
  under plain cargo without the feature.

## 9. Open decisions

### 9.1 Objective reward trigger and reward economics

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
function must be monotone along a chain under the enforced validity rules. Extension, which is
implemented, makes `LF(H)` monotone along a chain; Linearity then makes `snapshot(LF(H))`
monotone, and `candidate(H)` follows because `prune_σ(H)` is monotone and an lca of two
monotone arguments is monotone. The missing Linearity check leaves the premise unenforced
today. Without a Finality Depth rule, a
bc-block producer can also keep a stale `context_bft` at no validity cost (§3.3), so an
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

### 9.2 Remaining protocol choices

- Remove `finalization_gap_bound` from `ZcashCrosslinkParameters` and the test format, or
  re-document it as unused; its doc comment still describes Stalled Mode.
- Choose between PoS-store migration and replay rules for the one-block snapshot shift; replay
  rules are the minimum. The PoS store record is not a serialized `BftBlock`
  alone: each record appends the block, the fat pointer, `finalizers_at_current_height`, and the
  proposal signatures. The roster is marker-derived — it is the aggregated stakes that
  `CrosslinkFinalizeBlock(hash(headers[0]))` returned — and restore reads it back verbatim
  rather than recomputing it, so existing files carry old-derivation roster bytes that corrected
  code will not correct. Meanwhile the replay watermark `prev_finalized_bc_height` *is*
  recomputed from `headers.first()` during restore and feeds `terminated_finalizers_at`, whose
  third argument is the marker height at every call site. Votes travel by roster index, so a
  divergent roster re-indexes stored votes through seats that never voted; the code comment at
  that site records this having already jailed and unjailed finalizers one certificate early at
  a hardfork activation boundary. Nodes running the two derivations would also disagree about
  which bc-block is finalized. The RocksDB side is unaffected: aggregated stakes are keyed by
  block hash and written in the block's own batch, so both derivations' rows already exist.
- Specify the checkpoint and recency condition for exposing `fin` to clients (§3.5),
  independently of block validity.
- Define the block and transaction status contract, with distinct confirmed and finalized
  states (§6.5, §7).

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

All implementation observations in §§5–6 describe the monolith tree this document ships
with. Code can move without this file being updated, so re-check the cited symbols before
using this document to plan changes.
