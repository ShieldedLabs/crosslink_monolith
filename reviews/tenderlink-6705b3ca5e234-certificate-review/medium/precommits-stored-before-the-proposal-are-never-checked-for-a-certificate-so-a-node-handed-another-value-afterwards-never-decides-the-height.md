# `adopt_if_certified` runs only when a precommit is newly stored while a proposal is already held, so 2f+1 precommits that arrive before the first proposal chunk are never looked at again, and a node then handed the proposer's other value never decides the height

**Severity**: Medium
**Validation Status**: Confirmed
**Location**: `tenderlink/src/lib.rs:387-402` (`adopt_if_certified`, returns at once when `proposal_id` is NIL), `:1125` and `:1143` (its only two call sites, both in the vote arm), `:1037-1045` (first chunk of a round: `prefer_votes_for`, `recount`, no adoption check), `:862-864` and `:1107-1109` (a precommit already stored returns before any check), `:1005-1009` (the decided value's chunks refused as a second value)
**Found by agent:** adversarial review subagent, 2026-10-12, against the uncommitted diff to `tenderlink/src/lib.rs` on top of `dev` 6705b3ca5e234; validated the same day by running scratch tests against that diff and against HEAD
**In scope of audit?** Yes. Judged against the stated purpose of the uncommitted change ("a proposer that gives 2f+1 finalizers one value and the rest another must not be able to leave the rest unable to ever decide that height"). The stuck outcome exists at HEAD 6705b3ca5e234 for every arrival order; the diff removes it for one order and leaves it for the other. Not a regression. No deployed branch carries the diff.

## Description

The diff adds adoption on certificate. When a precommit for a value the node does not hold is stored, `adopt_if_certified` (`lib.rs:387`) checks whether that value now has 2f+1 precommit stake and, if so, replaces the held proposal with it.

The check is tied to one event: a precommit being newly stored (`lib.rs:1125`, `:1143`). It also returns immediately while no proposal is known (`lib.rs:388`, `self.proposal_id == ValueId::NIL`), because before the proposal every value vote is provisionally counted as for it.

The other event that can create the condition is the proposal becoming known. On the first chunk of a round the code calls `prefer_votes_for` and `recount` (`lib.rs:1042-1043`), which correctly stops the stored votes counting as yes votes when they name another value. It does not ask whether one of those other values already has 2f+1 precommits.

After that nothing asks again. Every later copy of the same precommits is already stored, so it returns at the repeat check (`lib.rs:862-864`) or at "already have it" (`lib.rs:1107-1109`) without reaching `adopt_if_certified`. The decided value's chunks are refused as a second value from the same proposer (`lib.rs:1005-1009`). The node holds the proposer's other value for good.

**This is the failure the change sets out to remove, left in place for one arrival order: all the precommits first, then a chunk of the other value.** It is a liveness defect for one node per attack, not a safety defect. It is not a regression: HEAD is stuck in both orders.

## Attack Scenario and Steps

A Byzantine round proposer triggers it against a node that has nothing stored for the round: one that is behind, or one that restarted (round data is in memory only).

1. Four finalizers of equal stake, threshold 3. Proposer P signs value A and value B for round 0 of height h. P, o0 and o1 hold A, precommit it and decide. Node N has not received anything for the round.
2. P, which is connected to N, sends N the three precommits for A (one vote packet) and then one chunk of B. Each precommit is stored in `msg_val_sigs`; `adopt_if_certified` returns at `lib.rs:388` because `proposal_id` is NIL.
3. The chunk of B arrives. `proposal_id` becomes B, `recount` sets `yes_precommits` to 0 (`lib.rs:1037-1045`). No adoption check runs.
4. Honest peers serve the decided round to N, repeatedly: A's chunks and A's precommits. The chunks return `Fail` at `lib.rs:1005-1009`. The precommits return `Pass` at `lib.rs:862-864` and change nothing.
5. N stays at height h. Observable as `DECIDE_WAIT` never appearing (its `yes_precommits` is 0) and the "proposed 2 different values" fault line repeating.

**Attack Requirements and Assumptions:**

- The attacker is the proposer of the round that decided (it must have signed two values for it) and can deliver packets to the victim.
- The victim must store 2f+1 precommits for A before any proposal chunk, and the chunk of B before any chunk of A. The attacker controls this order on its own link; honest peers answer a status at the next tick (250 ms, `lib.rs:112`), which is the window. Inference: not measured on a live network.
- If the two packets are reordered (B first), the diff's adoption path works and the attack fails harmlessly.
- No stake beyond the proposer's own is needed.

## Impact on Users

The node operator of the victim, and through it the network's fault margin.

- **The node cannot decide the height until it is restarted.** A restart clears the round data, and the attacker can repeat.
- **The network loses a finalizer it had.** With n = 3f+1 and the proposer faulty, one honest finalizer stuck leaves 2f live honest finalizers; the proposer then only has to stop voting. This is the consequence the diff's own test comment names.

**Detectability, stated precisely.** The operator sees the node stop advancing and the repeated "proposed 2 different values" line for the round. Nothing names the cause. Peers cannot tell.

## Technical Details / Code Analysis

**The check is skipped while the proposal is unknown** (`tenderlink/src/lib.rs:387-390`):

```rust
    fn adopt_if_certified(&mut self, value_id: ValueId, roster: &[SortedRosterMember], ctx_str: &str) {
        if self.proposal_id == ValueId::NIL || self.proposal_id == value_id || self.proposal_is_certified(roster) {
            return;
        }
```

**The proposal becoming known does not run it** (`tenderlink/src/lib.rs:1037-1045`):

```rust
                    if round_data.proposal_id == ValueId::NIL { // first time we've seen any proposal chunks
                        round_data.proposal_id = value_id;

                        // Votes that arrived before the proposal were all tallied as for it. Some may be
                        // for another value; they stay, and stop counting as yes votes.
                        round_data.prefer_votes_for(value_id);
                        round_data.recount(roster);
                        certified = round_data.proposal_is_certified(roster);
                    }
```

**A stored precommit never reaches the check again** (`tenderlink/src/lib.rs:862-864`):

```rust
                    } else if round_data.msg_val_sigs[roster_i][is_precommit] == (value_id, sig) {
                        return TMStatus::Pass;
                    }
```

**Output of the scratch test on the diff:**

```text
R2-F1 precommits_first=false: height 1 holds None counts None first A chunk statuses [Pass, Pass]
R2-F1 precommits_first=true: height 0 holds Some("B") counts Some(Counts { a:3  v:0 (nv:0 yv:0)  c:3 (yc:0) }) first A chunk statuses [Fail, Fail]
```

## Recommendations

1. **Run the adoption check when the proposal becomes known.** After `recount` at `lib.rs:1043`, if the new proposal is not certified, look for a value with 2f+1 precommit stake among the stored precommits (at most one distinct value per slot, two slots, twelve finalizers) and adopt it. The chunk that triggered it then belongs to a value that is no longer held, so return without storing it.
2. **Alternatives considered.** Running the check in `bft_update` for every round each tick: covers this and any future gap, at O(roster) per round per tick; heavier than needed but a reasonable belt. Letting the repeat check at `:862` fall through to the adoption check: adds work to the hot path of every re-delivered vote.
3. **Tests.** Extend `line49_decided_value_counts_a_finalizer_that_also_precommitted_ours` or add a sibling: three precommits for A, then B's chunks, then A's chunks, then `bft_update`; assert `height == 1`. The scratch test `r2_f1_precommits_then_other_proposal` does this and prints instead of asserting.
4. **Rollout.** Node-local, no consensus or wire change. Should land with or directly after the diff, since it completes the diff's stated purpose.

## Validation Information

**Verdict: CONFIRMED. Severity: Medium.**

**Status: fixed, and committed with the change as `5a6577e2` (re-validated 2026-10-12 against the revised uncommitted diff on `dev` 6705b3ca5e234).** The revision adds `adopt_any_certified` (`lib.rs:407-416` in the revised file) and calls it after the first chunk's `prefer_votes_for` and `recount` (`:1064-1070`); if it adopts, the triggering chunk returns `Fail`. This is recommendation 1. The revision also adds the test `line49_precommits_that_came_before_any_proposal_still_decide` (`:4057`), which is recommendation 3. Line numbers elsewhere in this report are those of the first diff.

Built and run: a scratch crate assembled from the engine source plus `scratch_common.rs` (in the review's scratch directory), once from the working tree and once from HEAD 6705b3ca5e234. Network timing claims are from reading only.

| Claim | Verified at |
|-|-|
| Precommits first, then a chunk of B: node holds B, `yes_precommits` 0, A's chunks return `Fail`, height stays 0 over 8 rounds of re-delivery and `bft_update` | run, `r2_f1_precommits_then_other_proposal`, diff |
| Chunk of B first, then the precommits: node adopts A and decides | run, same test, diff |
| HEAD is stuck in both orders | run, same test, HEAD |
| Random message sequences reach this state: 74 states in 800 seeds held an uncertified proposal while another value had 2f+1 precommits stored | run, `r2_fuzz_invariants`, diff |
| `adopt_if_certified` has two call sites, both on a newly stored precommit | read, `lib.rs:1125`, `:1143` |
| Revised diff: precommits first then a chunk of B now adopts A and decides (height 1, A's chunks `Pass`) | run, `r2_f1_precommits_then_other_proposal`, revised diff |
| Revised diff: 0 states in the same 800 seeds hold an uncertified proposal while another value has 2f+1 precommits stored (74 before); 411 adoptions, every other invariant held | run, `r2_fuzz_invariants`, revised diff |
| Revised diff: adoption in the middle of storing the chunk leaves no trace of it (`restart_proposal_assembly` replaces the buffer and zeroes `proposal_sigs_n`), cannot fire for the proposal just set or when that proposal is itself certified, and needs the same 2f+1 as before | read, revised `lib.rs:387-416`, `:1064-1070` |
| An attacker can order the two packets ahead of honest peers' chunks | inferred from `lib.rs:2151-2163` (round served on the tick after a status) and `:2361` (send order shuffled); not run |

**Severity justification.**

*Why not High:* no safety violation; one victim per attack; the victim must have nothing stored for the round; a restart clears it; the same outcome already exists at HEAD.

*Why not Low:* the result is permanent for the running process, the attacker needs only its own proposer key and two packets in order, and it is the exact outcome the change claims to close.

**Corrections made during validation.**

1. None. The lead was derived from the code and reproduced on first run.

**What this issue does not claim.** It does not claim the diff makes anything worse, that two honest nodes can decide differently, or that a node which already holds a proposal when the precommits arrive is affected.

**Cross-references.** `a-conflicting-chunk-discards-a-complete-hash-verified-certified-proposal-so-a-replayed-packet-delays-a-decision-head-would-make.md` is independent. No ordering between them.

DO NOT DEVIATE FROM THIS TEMPLATE (`reviews/SECURITY-ISSUE-TEMPLATE.md`).
LEAVE THIS MESSAGE IN PLACE SO THAT ALL AGENTS KNOW NOT TO DEVIATE FROM THIS TEMPLATE (`reviews/SECURITY-ISSUE-TEMPLATE.md`).
