# The other slot keeps only a finalizer's newest second value, so a finalizer that signs three prevotes in a round can displace the one line 28 needs and a node that did not hold that round's proposal stops seeing a real 2f+1 prevote certificate

**Severity**: Low
**Validation Status**: Confirmed
**Location**: `tenderlink/src/lib.rs:1119-1126` (a second value vote overwrites whatever the other slot held), `:328-335` (`stake_voting_for` reads the two slots), `:598-612` (`has_prevote_certificate`), `:1268-1273` (line 28 evaluates it at the tick)
**Found by agent:** adversarial review subagent, 2026-10-12, against the uncommitted diff to `tenderlink/src/lib.rs` on top of `dev` 6705b3ca5e234; validated the same day by running a scratch test against that diff
**In scope of audit?** Yes. Judged against the stated purpose of the uncommitted change ("a finalizer that signs two votes must not be able to hide a real 2f+1 from a node"). The slot is new in the diff. At HEAD 6705b3ca5e234 the second value vote is refused outright, so HEAD is worse; this is not a regression. No deployed branch carries the diff.

## Description

The diff gives each finalizer a second value slot per step, `msg_other_sigs`. A value vote that is not for the held proposal and differs from the one in `msg_val_sigs` goes there, "the newest replacing any earlier one" (`lib.rs:1120-1124`). `stake_voting_for` counts a finalizer toward a value if either slot names it (`lib.rs:328-335`), and `has_prevote_certificate` now uses it for line 28.

The field comment states the bound: "Newest wins, so a finalizer cannot fill memory". That bound is right. What it gives up is the third value: a finalizer that signs W, V and X leaves a node holding two of the three, and which two depends on arrival order.

For precommits this does not hide anything, because `adopt_if_certified` runs at the moment a vote lands in the slot. For line 28 the certificate is read later, at the tick (`lib.rs:1273`), from whatever the slot holds then. A node that did not hold the proposal of round vr keeps the finalizer's first value in `msg_val_sigs` and its newest other value in `msg_other_sigs`. If the newest is X, the finalizer's prevote for V is not counted.

**This is a gap for finalizers that sign three or more values in one step, at nodes that did not hold the cited round's proposal, and only when the certificate needs that finalizer's vote.** Two signed values, the case the change names, are handled. The effect is intermittent: each re-delivery of the V prevote restores it until X arrives again.

## Attack Scenario and Steps

A Byzantine finalizer whose prevote is needed for a 2f+1 in round vr.

1. Four finalizers, threshold 3. In round 0, o0 and o1 prevote V. Finalizer P signs prevotes for W, V and X. Node N never received round 0's proposal.
2. N stores P's W prevote in `msg_val_sigs` and P's V prevote in the other slot. `has_prevote_certificate(.., vr = 0, V)` is true.
3. P's X prevote arrives. It overwrites V in the other slot (`lib.rs:1124`). The certificate for V is false.
4. In round 1 the proposer re-proposes V citing vr = 0. At N's tick line 28 does not fire while the slot holds X; N stays in the propose step and prevotes NIL on timeout.
5. Honest peers that hold V for round 0 re-deliver P's V prevote; the slot flips back to V until X is delivered again.

**Attack Requirements and Assumptions:**

- A finalizer on the active roster that signs three different prevotes in one round and keeps re-sending the third to the victim.
- The certificate for V at the victim must depend on that finalizer's stake.
- The victim must not hold V as round vr's proposal. If it does, the vote for the held proposal takes `msg_val_sigs` and cannot be displaced.

## Impact on Users

The victim's operator, and the round in which the value is re-proposed.

- **The victim prevotes NIL for a re-proposal it should accept.** Inference: if the round needs the victim's prevote for 2f+1, the round fails and the next one starts.
- **Not permanent.** Any tick at which the slot holds V lets line 28 fire.

**Detectability, stated precisely.** The first conflicting vote prints "voted on 2 different values ... Keeping both"; later replacements print nothing (`lib.rs:1116-1118`). Nothing reports that a certificate was lost.

## Technical Details / Code Analysis

**The newest second value overwrites the previous one** (`tenderlink/src/lib.rs:1119-1127`):

```rust
                        if !is_for_held_proposal {
                            // Its second value goes beside the first, the newest replacing any earlier one.
                            if other.0 == value_id && other.1 != TMSig::NIL {
                                return TMStatus::Pass; // already have it
                            }
                            round_data.set_other_vote(roster_i, is_precommit, (value_id, sig));
                            if is_precommit == 1 { round_data.adopt_if_certified(value_id, roster, &ctx_str); }
                            return TMStatus::Pass;
                        }
```

**Output of the scratch test on the diff:**

```text
R2-F3 certificate for V: after W,V true; after X false; after V re-delivered true
```

## Recommendations

1. **Do not let a newer other-value prevote displace one that currently completes a 2f+1.** Before overwriting at `lib.rs:1124` for a prevote, keep the stored other vote if `stake_voting_for(0, stored value)` is at or above the threshold. The slot stays one entry, so the memory bound is unchanged.
2. **Alternatives considered.** A third slot only moves the problem to four values. Latching "certificate seen for value V in round vr" per round when it first becomes true is simple and exact, and adds one value id per round; it is a reasonable alternative to recommendation 1.
3. **Tests.** A unit test on `has_prevote_certificate` in the style of the `line28_*` tests: two prevotes for V, a third finalizer's W, V, X in that order, assert the certificate still holds. The scratch test `r2_f3_third_value_displaces_the_vote_line_28_needs` drives this and prints.
4. **Rollout.** Node-local, no consensus or wire change. Can land after the diff.

## Validation Information

**Verdict: CONFIRMED. Severity: Low.**

Built and run: a scratch crate assembled from the working-tree engine source plus `scratch_new.rs`. The effect on a round is from reading only.

| Claim | Verified at |
|-|-|
| With W then V stored, the certificate for V holds; after X it does not; after V is re-delivered it does | run, `r2_f3_third_value_displaces_the_vote_line_28_needs`, diff |
| Line 28 reads the certificate at the tick | read, `lib.rs:1268-1273` |
| A vote for the held proposal cannot be displaced | read, `lib.rs:1114`, `:1128`; run, fuzz invariant in `r2_fuzz_invariants` (no vote for the held proposal left in the other slot, 800 seeds) |
| HEAD refuses the second value vote | read, HEAD `lib.rs`, the removed lines in the diff hunk at `:1112` |
| The victim then prevotes NIL and the round can fail | inferred from the propose timeout in `bft_update`; not run |

**Severity justification.**

*Why not Medium:* intermittent, needs a finalizer signing three values and a certificate that depends on it, affects only nodes that missed round vr's proposal, costs at most rounds, and HEAD is worse.

*Why not dismissed:* the change's description says a double signer cannot hide a 2f+1; a triple signer still can, for line 28.

**Corrections made during validation.**

1. I first expected the same gap for precommits. It does not exist there: adoption is checked when the vote lands, before a later vote can displace it.

**What this issue does not claim.** It does not claim any safety effect, any effect on decisions (line 49), or that two-value equivocation is mishandled.

**Cross-references.** None required.

DO NOT DEVIATE FROM THIS TEMPLATE (`reviews/SECURITY-ISSUE-TEMPLATE.md`).
LEAVE THIS MESSAGE IN PLACE SO THAT ALL AGENTS KNOW NOT TO DEVIATE FROM THIS TEMPLATE (`reviews/SECURITY-ISSUE-TEMPLATE.md`).
