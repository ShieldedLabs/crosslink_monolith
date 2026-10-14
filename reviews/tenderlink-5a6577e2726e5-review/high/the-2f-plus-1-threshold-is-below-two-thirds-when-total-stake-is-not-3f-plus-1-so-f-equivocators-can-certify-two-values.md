# The `2f+1` quorum is below two thirds of the stake whenever total stake is not `3f+1`, so `f` equivocating finalizers can certify two different values and fork the height

**Severity**: High
**Validation Status**: Confirmed
**Location**: `tenderlink/src/lib.rs:843-845` (`f_from_n`, `f = (n-1)/3`), `tenderlink/src/lib.rs:361-365` (`RoundData::big_threshold`, `2*f+1`), `tenderlink/src/lib.rs:1236-1245` (`bft_update` computes the same `big_threshold`), `tenderlink/src/lib.rs:383-403` (`adopt_if_certified`, whose comment asserts "Only one value can get there with at most f faulty"), `tenderlink/src/lib.rs:367-372` (`proposal_is_certified`), `tenderlink/src/lib.rs:612-626` (`has_prevote_certificate`, same `2*f+1`)
**Found by agent:** raised as an unconfirmed read-only lead in the first review (item 2b of the brief); validated by the second adversarial review subagent, 2026-10-12, at `dev` 5a6577e2726e5
**In scope of audit?** Yes, with a boundary. The threshold formula predates both commits under review and is unchanged by them. What is new in 5a6577e2 is `adopt_if_certified`, which makes a node abandon the value it holds and decide another value on the strength of `2f+1` precommits for it, and whose comment states the safety premise ("Only one value can get there with at most f faulty") that this finding shows is false. The deployed engine uses this threshold, and the active roster is capped at 12 (`librustzcash/zcash_primitives/src/bft.rs:884`, `ACTIVE_ROSTER_MAX_N = 12`).

## Description

Tenderlink derives its fault bound as `f = (n - 1) / 3` over total active stake `n` (integer division, `tenderlink/src/lib.rs:844`) and its decision quorum as `2*f + 1` (`tenderlink/src/lib.rs:364`, `:1243`). A BFT quorum must be strictly larger than two thirds of the stake so that any two quorums share at least one honest member; with at most `f` faulty members, that shared honest member cannot vote for two values, so only one value reaches a quorum.

That holds only when `n = 3f + 1` exactly. For a given `f`, integer division lets `n` be `3f+1`, `3f+2`, or `3f+3`. When `n` is `3f+2` or `3f+3`, the quorum `2f+1` is strictly less than `floor(2n/3)+1`, the smallest correct quorum. Two quorums of `2f+1` each can then overlap only in the `f` faulty members: every honest member of the first quorum is distinct from every honest member of the second, and the `f` faulty members sit in both and equivocate. Two different values each reach `2f+1`.

The condition for two such disjoint-honest quorums to exist is `2((2f+1) - f) <= n - f`, that is `n >= 3f + 2`. The arithmetic: a quorum has at least `(2f+1) - f = f+1` honest members; two quorums with no common honest member need `2(f+1)` distinct honest members; there are `n - f` honest members in all.

**This is a consensus-safety weakness under the nominal fault budget `f`, not merely a liveness or counting issue: with `n` not equal to `3f+1`, `f` equivocating finalizers can get two values decided, and two honest nodes then finalize different blocks at one height.** What bounds it is that the adversary must actually control `f` finalizer keys (a real breach of the fault budget's membership, not of its count); what does not bound it is anything unusual about the stake distribution beyond `n` not being `3f+1`, which the natural case of equal-stake finalizers routinely is.

## Attack Scenario and Steps

The adversary controls `f` finalizer keys (the maximum the budget nominally tolerates) and is the round proposer, or colludes with it.

1. The roster has 12 active finalizers of equal stake 1, so `n = 12`, `f = (12-1)/3 = 3`, and the quorum is `2f+1 = 7`. A correct quorum would be `floor(24/3)+1 = 9`. This is the deployed cap's natural shape.
2. The proposer signs two values, A and B, for the round. It sends A to one group of honest finalizers and B to another.
3. Four honest finalizers prevote and precommit A; four other honest finalizers prevote and precommit B; the 3 adversarial finalizers precommit both A and B.
4. A has 4 honest + 3 adversarial = 7 = `2f+1` precommits. B likewise has 4 + 3 = 7. Both clear the quorum.
5. A node holding A reaches `proposal_is_certified` for A and decides A (`bft_update` line 49, `tenderlink/src/lib.rs:1376-1395`). A node holding B decides B. A node that held B but hears A's 7 precommits runs `adopt_if_certified(A)` (`tenderlink/src/lib.rs:1171`), abandons B, and decides A, and symmetrically for the other side. The height is finalized to two different blocks across honest nodes: a fork.

**Attack Requirements and Assumptions:**

- An adversary controlling `f` finalizer keys, with the round's proposer either adversarial or induced to double-sign (double-signing a proposal is already modelled as a proposer fault elsewhere in the engine).
- Total active stake not equal to `3f+1`. Equal-stake rosters of 5, 6, 8, 9, 11, 12, 14, 15, ... all qualify; the deployed 12-seat cap at equal stake is `3f+3`.
- It does not require any stake imbalance, any signature forgery, or more than the nominal `f` faults.

## Impact on Users

Everyone relying on BFT finality at the affected height bears the loss.

- **Two honest nodes finalize different blocks at one height.** The fat pointer each builds carries `2f+1` genuine precommit signatures for its value (`round_data_to_fat_pointer`, `tenderlink/src/lib.rs:197-223`), so each node's `decide` accepts it (`validate_signatures` passes, `zebra-crosslink/.../bft.rs:1205`): both forks look fully certified.
- **The new adopt-other-value path acts on the false premise.** `adopt_if_certified` switches a node from the value it holds to a value with `2f+1` precommits, trusting that only one value can be there. When two are, the switch helps an attacker drive honest nodes onto whichever fork reaches them first.

**Detectability, stated precisely.** A node that sees both certificates could in principle notice two values each at `2f+1` and flag equivocation, but the engine does not: each node acts on the certificate it assembles and never cross-checks the other value's precommit stake against its own. Operators would detect the fork only after the fact, as two finalized blocks at one height reported by different nodes. Within a single node there is no signal.

## Technical Details / Code Analysis

**The fault bound and quorum** (`tenderlink/src/lib.rs:843-845`, `:361-365`):

```rust
fn f_from_n(n: u64) -> u64 {
    (n - 1) / 3
}
```

```rust
fn big_threshold(roster: &[SortedRosterMember]) -> u64 {
    let total: u64 = roster[..active_roster_len(roster)].iter().map(|m| m.stake).sum();
    let f = TMState::f_from_n(total.max(1));
    if f == 0 { total } else { 2*f + 1 }
}
```

For `n = 12`, `f = 3`, `2f+1 = 7`, while a correct `>2/3` quorum is `floor(2*12/3)+1 = 9`. The quorum is below two thirds.

**The premise the new code states** (`tenderlink/src/lib.rs:383-387`):

```rust
/// Called when a precommit for `value_id` has just been stored. If `value_id` is not the proposal
/// we hold and now has 2f+1 precommits, it is decided, so it replaces ours: the votes stay, the
/// bytes of ours go, and its bytes are taken as they arrive. Only one value can get there with
/// at most f faulty, and a value that has is never given up for another.
```

"Only one value can get there with at most f faulty" is the exact statement contradicted above for `n != 3f+1`.

**The same `2f+1` governs the line 49 decision and line 28 certificate** (`tenderlink/src/lib.rs:1243`, `:624`), so the weakness is not confined to the new adopt path; it is the engine's decision quorum throughout.

## Recommendations

1. **Compute the quorum as a strict two-thirds-of-stake threshold, not `2f+1`.** Replace `big_threshold` and the inline `2*f+1` sites with `total / 3 * 2 + 1` adjusted to `floor(2*total/3) + 1` (equivalently the smallest integer strictly greater than `2*total/3`). This makes any two quorums share honest stake for every `n`, restoring the single-value invariant. Apply it in `RoundData::big_threshold`, `bft_update` (`:1243`), `has_prevote_certificate` (`:624`), and `proposal_is_certified`. The small-roster `f == 0` special case (require all stake) stays.
2. **Correct or delete the `adopt_if_certified` comment** so no future reader relies on "only one value can get there with at most f faulty"; once recommendation 1 lands, the restated invariant is "only one value can reach a strict two-thirds quorum with less than one third faulty stake."
3. **Alternatives considered.** Requiring the roster to always have `n = 3f+1` was rejected: stake is `voting_power` from bonds plus bank (`zebra-crosslink/.../bft.rs:383`) and the active set is a top-`12` cut, so `n` is whatever the stake distribution yields and is frequently not `3f+1`. Keeping `2f+1` but adding a cross-value equivocation detector was rejected as a patch over the wrong threshold: it would catch the fork after both certificates exist rather than prevent either.
4. **Tests.** Add a unit test over `f_from_n`/`big_threshold` asserting `big_threshold(n) > 2*n/3` for all `n` in a range (it currently fails at `n = 5, 6, 8, 9, 11, 12, ...`). Add an engine-level test with 5 equal-stake finalizers where one equivocating precommitter lets two values each reach the quorum, asserting that at most one value is ever certified. During validation, a standalone program computed `f_from_n` and `big_threshold` for `n = 1..=15` and confirmed `2f+1 < floor(2n/3)+1` for every `n` not of the form `3f+1`.
5. **Rollout.** This changes the consensus decision rule: a node computing the corrected quorum will refuse to decide on a bare `2f+1` that the old rule accepted, so all finalizers must adopt it together, ideally gated behind a hardfork certificate height using the existing vote-namespacing hardfork machinery. A deployed network running the old threshold is exposed until then and should be treated as such.

## Validation Information

**Verdict: CONFIRMED. Severity: High.**

Confirmed by reading the engine at `dev` 5a6577e2726e5 and by building and running a standalone program that evaluates the engine's own `f_from_n` and `big_threshold` arithmetic; no live network was run.

| Claim | Verified at |
|-|-|
| `f = (n-1)/3` | `tenderlink/src/lib.rs:844` |
| quorum is `2f+1` for `f>0` | `tenderlink/src/lib.rs:364`, `:1243`, `:624` |
| `2f+1 < floor(2n/3)+1` for `n = 5,6,8,9,11,12,14,15` | standalone program, run; printed table |
| two disjoint-honest quorums exist iff `n >= 3f+2` | standalone program, run; derived `2((2f+1)-f) <= n-f` |
| `n=12` equal stake gives `f=3`, quorum 7, correct quorum 9 | standalone program, run |
| the new adopt path decides another value on `2f+1` precommits | `tenderlink/src/lib.rs:387-403`, `:1171`, `:1069` |
| its comment asserts the single-value premise | `tenderlink/src/lib.rs:386` |
| each fork's fat pointer carries genuine `2f+1` signatures and passes validation | `round_data_to_fat_pointer` `tenderlink/src/lib.rs:197-223`; `validate_signatures` `librustzcash/zcash_primitives/src/bft.rs:1101-1121`; `zebra-crosslink/.../bft.rs:1205` (inferred from reading, not run) |
| threshold predates the two commits | `git diff e78c6ca6 5a6577e2` leaves `f_from_n` and the `2*f+1` sites unchanged |
| active roster cap is 12 | `librustzcash/zcash_primitives/src/bft.rs:884` |

**Severity justification.**

*Why not Critical:* it requires the adversary to actually control `f` finalizer keys, which is the edge of the fault budget rather than a single-peer action, and it needs a double-signing proposer; it does not let a lone outsider fork the chain.

*Why not Medium:* it is a genuine consensus-safety (double-finalization) break within the nominal fault budget `f`, for the ordinary case of a roster whose stake is not `3f+1`, which the deployed 12-seat equal-stake shape is; the resulting two certificates both verify, so the fork is not self-correcting.

**Corrections made during validation.**

1. The lead asked for "the smallest stake distribution where adoption or line 49 could go wrong." The smallest is `n = 5` equal unit stakes (`f = 1`, quorum 3); the deployed-relevant case is `n = 12` equal stakes (`f = 3`, quorum 7 against a correct 9).
2. The lead framed the question around the new `adopt_if_certified` path. The weakness is broader: the same low quorum governs the pre-existing line 49 decision and line 28 certificate, so the fork does not depend on the new adopt code; the new code only adds a second way for a node to act on a false certificate.

**What this issue does not claim.** It does not claim a lone peer or a sub-`f` adversary can fork the chain, nor that `n = 3f+1` rosters are affected (they are safe), nor that any signature is forged: the precommits on both forks are real. It does not claim the two commits introduced the threshold; they did not.

**Cross-references.** Independent of K5 (`a-crafted-vote-packet-indexes-past-the-18-slot-array-...`). K5 should be fixed first only because a crashing node shows no consensus behaviour; the two fixes are otherwise unordered.

DO NOT DEVIATE FROM THIS TEMPLATE (`reviews/SECURITY-ISSUE-TEMPLATE.md`).
LEAVE THIS MESSAGE IN PLACE SO THAT ALL AGENTS KNOW NOT TO DEVIATE FROM THIS TEMPLATE (`reviews/SECURITY-ISSUE-TEMPLATE.md`).
