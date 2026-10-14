# A double-spend block is answered "already known" because commit errors are classified by their text, so callers and the rejection backoff never see it as refused

**Severity**: Low
**Validation Status**: Confirmed
**Location**: `zebra-crosslink/zebra-state/src/new_network.rs:3290-3296` (commit errors classified by their `Debug` text), `:3304` (a duplicate is answered `Known`), `:3319-3327` (only a `Failed` verdict records a rejection); `zebra-crosslink/zebra-state/src/error.rs:239-310` (the error variants named `Duplicate...`), `:434` (`AlreadyFinalized`); `zebra-crosslink/zebra-rpc/src/methods.rs:3870-3875` (`submitblock` answers a `Known` verdict as duplicate)
**Found by agent:** the "CROSSLINK binary test format" session on 2026-10-13, by the scenario `crosslink_mempool_verdict_goes_stale_when_a_block_spends_the_input`; validated 2026-10-14 by the "CROSSLINK spreadsheet triage" session at `dev` 890036389ac3b
**In scope of audit?** Yes. Judged against the sync and ingest path of `new_network.rs`. The classification predates the fate table of `e56e35f8`. It affects any network running this ingest path; it does not affect what is committed.

## Description

When the sync loop hands a verified block to the state, `handle_commit` returns `Result<(), ValidateContextError>`. The loop turns an error into one of two classes by formatting it with `Debug` and searching the text:

```rust
let msg = format!("{err:?}");
if msg.contains("Duplicate") || msg.contains("AlreadyFinalized") {
    BlockCommitError::Duplicate
} else {
    BlockCommitError::Other(msg)
}
```

A `Duplicate` class is then answered as `IngestOutcome::Known`, the verdict for a block the node already has.

The assumption is that an error whose text says "Duplicate" means the block is a duplicate. It does not. Of the contextual validation errors, the only ones whose names contain "Duplicate" are double spends: `DuplicateTransparentSpend`, `DuplicateSproutNullifier`, `DuplicateSaplingNullifier`, `DuplicateOrchardNullifier` and `DuplicateIronwoodNullifier`. The other string matched, `AlreadyFinalized`, is declared but never constructed anywhere in the workspace. A block that is already in the state is recognised much earlier, by `known_outcome` before verification, so the match catches no real duplicate and every double spend.

**The block is not committed; what is wrong is the verdict.** The chain is unaffected. Callers are told the wrong thing, and the bookkeeping that follows a refusal does not run.

## Attack Scenario and Steps

An ordinary event triggers it: a block that spends an output a second time, built by a miner from a stale template or served by a peer.

1. The node's chain holds a transaction that spends output `o`.
2. A block extending the tip spends `o` again (`new_network.rs:3287`, `handle_commit`).
3. Contextual validation fails with `DuplicateTransparentSpend`; the loop classifies it as `Duplicate` (`:3292`).
4. The verdict is `Known` (`:3304`). `failed` is false, so `note_rejection` is not called (`:3319-3327`). A miner calling `submitblock` is told the block is a duplicate (`methods.rs:3870-3875`) instead of rejected; a synthetic test peer is told it is known.

**Attack Requirements and Assumptions:**

- Anyone who can deliver a block: a miner, or any peer serving one.
- No stake, no signatures beyond a valid header.
- Nothing special about the network.

## Impact on Users

Miners and anything that reads ingest verdicts are misinformed; no funds or chain state are at risk.

- **A miner is told "duplicate" for a rejected block.** A miner building on a stale template has no signal that its block was invalid.
- **No rejection backoff.** The hash is never recorded as rejected, so the same body can be requested and verified again.
- **Tests read the wrong verdict.** The scenario that found this had to pin "accepted, already known" for a block that was refused.

**Detectability, stated precisely.** The node logs "Already was committed" for a block it did not commit. Nothing else shows it.

## Technical Details / Code Analysis

**The classification** (`zebra-crosslink/zebra-state/src/new_network.rs:3290-3296` at `890036389ac3b`): quoted in the description.

**The variants named Duplicate are all double spends** (`zebra-crosslink/zebra-state/src/error.rs`): `DuplicateTransparentSpend` (`:239`), `DuplicateSproutNullifier`, `DuplicateSaplingNullifier`, `DuplicateOrchardNullifier`, `DuplicateIronwoodNullifier` (`:279-310`). `AlreadyFinalized` is declared at `:434`; a search of the workspace finds no other occurrence than the string match.

**A refusal records a rejection, a known block does not** (`new_network.rs:3319-3327`):

```rust
let failed = matches!(outcome, IngestOutcome::Failed { .. });
let delivered_by = settle_fate(&mut fates, &synthetic_peers, hash, outcome);
if let Some(key) = delivered_by {
    if failed {
        note_rejection(&mut rejections, hash);
    }
```

## Recommendations

1. **Classify by asking the state, not by reading the error.** On a commit error, answer `Duplicate` only if `known_outcome(&read_state, hash)` finds the block; otherwise `Other`, which becomes `Failed` with the error as its reason. A genuine duplicate that races past the earlier check still gets `Known`; every validation failure, double spends included, is refused.
2. **Alternatives considered.** Matching the error variants instead of the text would work today but would still classify by name; the state is the authority on whether the block is there. Removing the `Duplicate` class entirely would answer a racing duplicate as refused.
3. **Tests.** `crosslink_mempool_verdict_goes_stale_when_a_block_spends_the_input` loads the double-spend block and asserts it is rejected with reason `DuplicateTransparentSpend`, and that the tip does not move. A shielded double spend (a duplicate nullifier) is not covered: the test generator cannot build shielded transactions.
4. **Rollout.** Node-local: the verdict and backoff change, the consensus rules do not. No coordinated upgrade; no backport needed for a deployed branch unless miners there rely on `submitblock` answers.

## Validation Information

**Verdict: CONFIRMED. Severity: Low.**

The wrong verdict was observed by running the scenario named above at `dev` 6c8ef573 (now 890036389ac3b), which loads a block double-spending a transparent output and saw it answered as already known while the tip did not move. The rest is from reading at 890036389ac3b.

| Claim | Verified at |
|-|-|
| Commit errors are classified by searching their `Debug` text | `zebra-state/src/new_network.rs:3290-3296` |
| The only `ValidateContextError` variants named Duplicate are double spends | `zebra-state/src/error.rs:239-310` |
| `AlreadyFinalized` is never constructed | workspace search; only `error.rs:434` and the string match |
| A double-spend block is answered as known and the tip does not move | run: `crosslink_mempool_verdict_goes_stale_when_a_block_spends_the_input` |
| A `Known` verdict skips `note_rejection` | `zebra-state/src/new_network.rs:3319-3327` |
| `submitblock` answers a `Known` verdict as duplicate | `zebra-rpc/src/methods.rs:3870-3875` |
| A peer could make the node re-verify the same refused body repeatedly | inferred from the missing backoff; not run |

**Severity justification.**

*Why not Medium:* nothing invalid is committed, the chain and balances are unaffected, and the extra work a peer can cause is bounded by the normal request path.

*Why not lower:* it is a wrong answer on the path every block takes, it misleads miners through `submitblock`, and it disables the backoff for the one class of refusal most likely to be repeated.

**Corrections made during validation.**

1. The lead said `AlreadyFinalized` was a second way a real duplicate was recognised. It is never constructed, so that half of the match is dead.

**What this issue does not claim.** It does not claim an invalid block is ever committed, or that a peer is wrongly killed: a contextual failure scores 0 either way.

**Cross-references.** [`../../crosslink-e56e35f85a98f-sync-review/low/rejected-block-backoff-is-keyed-by-hash-alone-so-one-peer-can-delay-an-honest-block-for-every-peer.md`](../../crosslink-e56e35f85a98f-sync-review/low/rejected-block-backoff-is-keyed-by-hash-alone-so-one-peer-can-delay-an-honest-block-for-every-peer.md): fixing this one makes double-spend refusals reach that backoff, so the two should be read together.

DO NOT DEVIATE FROM THIS TEMPLATE (`reviews/SECURITY-ISSUE-TEMPLATE.md`).
LEAVE THIS MESSAGE IN PLACE SO THAT ALL AGENTS KNOW NOT TO DEVIATE FROM THIS TEMPLATE (`reviews/SECURITY-ISSUE-TEMPLATE.md`).
