# A conflicting chunk that arrives before the certificate sets `proposal_is_faulty` on a proposal whose stored bytes are correct, and the restart that would clear the flag is never reached because every real chunk returns at the repeat check, so the node never decides the height

**Severity**: Medium
**Validation Status**: Confirmed
**Location**: `tenderlink/src/lib.rs:1091-1100` (uncertified: a chunk with another signature for a filled index sets `proposal_is_faulty`), `:879-892` (repeat check: a chunk identical to the stored one returns `Pass` before anything else), `:1001-1014` (certified branch: the faulty flag leads to a restart, but only for a chunk that gets this far), `:441-451` (`proposal_is_valid` caches `Fail` when the flag is set), `:1375-1378` (line 49 needs `Pass`)
**Found by agent:** adversarial review subagent, 2026-10-12, second pass, against the revised uncommitted diff to `tenderlink/src/lib.rs` on top of `dev` 6705b3ca5e234; validated the same day by running a scratch test against that revised diff and against HEAD
**In scope of audit?** Yes. Judged against the stated purpose of the uncommitted change ("a proposer must not be able to condemn a decided value by signing wrong bytes under its id") and its claim that for a certified proposal "an existing faulty flag" restarts the assembly. HEAD 6705b3ca5e234 has the same outcome, so this is not a regression. No deployed branch carries the diff.

## Description

While a proposal is not certified, a chunk for an index that is already filled with a different signature sets `proposal_is_faulty` and is refused (`lib.rs:1091-1100`). The stored bytes are not touched. If the stored bytes are the real value, the node now holds a complete proposal that hashes to its id and is flagged faulty.

When the precommits then arrive the proposal is certified. The diff's certified branch does treat the flag as a reason to restart (`lib.rs:1008-1013`), which would clear it. But that branch is reached only by a chunk that passes the repeat check at `lib.rs:879-892`, and the repeat check returns `Pass` for any chunk whose signature, id, valid round, header and bytes equal what is stored. The node's stored chunks are the real ones, so every chunk an honest peer sends is a repeat. The restart never runs.

At the tick, `proposal_is_valid` sees the flag and caches `Fail` (`lib.rs:444-445`). Line 49 needs `Pass`. The node holds the right bytes and the certificate and does not decide.

The diff's test `line49_wrong_bytes_under_a_decided_id_do_not_condemn_it` covers the other order, where the wrong bytes are what is stored and the real chunk is the one that differs; there the real chunk reaches the certified branch and the restart heals it.

**This is the HEAD behaviour surviving in one order: real bytes stored first, conflicting chunk second, certificate third.** It is a liveness defect for one node, not a safety defect and not a regression. A node that validated the proposal before the conflicting chunk (a live participant after its first tick) is not affected, because its cached `Pass` stands.

## Attack Scenario and Steps

A Byzantine round proposer, against a node that is catching up on the round or has not yet run a tick since completing the proposal.

1. Four finalizers, threshold 3. Proposer P proposed A (two chunks) in round 0 and also signed chunk 1 of other bytes under A's id. The other three decided A.
2. Node N receives A's two real chunks. The hash matches. N has not validated the proposal: for a round that is not its current one, validation happens only at line 49.
3. N receives the conflicting chunk 1. Index 1 is filled with another signature and the proposal is not certified, so `lib.rs:1097` sets `proposal_is_faulty`.
4. N receives the three precommits for A. The proposal is certified.
5. Honest peers resend A's chunks and precommits every tick. Each chunk returns `Pass` at `lib.rs:879-892`. At the tick `proposal_is_valid` caches `Fail`.
6. N stays at the height. A further conflicting chunk would now restart the assembly and heal it; honest traffic never does.

**Attack Requirements and Assumptions:**

- One conflicting chunk signed by the round's proposer under the decided id; only the proposer can create it, any peer can replay it.
- It must reach the victim after the real chunk for the same index and before the certificate, and before the victim caches validity. The attacker can send the real chunks and then the conflicting one itself, ahead of honest peers. Inference: not measured on a live network.
- If the conflicting chunk is stored first, the diff's restart path heals the node.

## Impact on Users

The victim's operator, and through it the network's fault margin, as for a node that cannot decide a height.

- **The node cannot decide the height while it runs.** A restart clears the round data; the attacker can repeat.
- **It holds everything needed to decide.** Correct bytes, matching hash, 2f+1 precommits.

**Detectability, stated precisely.** `DECIDE_WAIT` prints each tick with `yc` at the threshold, a full proposal and `validity:(Fail, None)`. The one "proposer signed 2 different values" line was printed once, earlier.

## Technical Details / Code Analysis

**The flag is set without touching correct bytes** (`tenderlink/src/lib.rs:1095-1100`):

```rust
                    } else {
                        // TODO: treat this as a failed is_valid & early out before awaiting full proposal
                        round_data.proposal_is_faulty = true;
                        eprintln!("{ctx_str} {ANSI_RED}BFT FAULT{ANSI_RST}: proposer signed 2 different values. Ignoring latest...");
                        return TMStatus::Fail;
                    }
```

**The flag decides validity** (`tenderlink/src/lib.rs:443-446`):

```rust
        if self.proposal_checked_validity.0 == TMStatus::Indeterminate {
            if self.proposal_is_faulty {
                self.proposal_checked_validity = (TMStatus::Fail, TMStatusReason::None);
            } else if self.has_full_proposal() {
```

**Output of the scratch test, identical on the revised diff and on HEAD:**

```text
R2-K4 conflicting chunk status Fail; height 0; (full, faulty, validity, yes_precommits, hash matches id) Some((true, true, Fail, 3, true)); first resend statuses [Pass, Pass]
```

## Recommendations

1. **Settle the flag when the proposal is certified, without waiting for a chunk.** Where a proposal is found certified, complete and flagged faulty, recompute the hash: if it matches the id, clear the flag and the cached validity; if not, restart the assembly. The natural places are `proposal_is_valid` (it is what turns the flag into `Fail`) or line 49's guard in `bft_update`, since both run each tick.
2. **Alternatives considered.** Clearing the flag in the repeat check at `:879`: puts a hash over the whole value on the hot path of every re-delivered chunk. Not setting the flag at `:1097` when the stored proposal is complete and hashed correctly: also correct, and it removes the cause for the uncertified case too, but it changes what an uncertified node prevotes after seeing proposer equivocation, which is a wider decision.
3. **Tests.** In the style of `line49_wrong_bytes_under_a_decided_id_do_not_condemn_it`: real chunks, one conflicting chunk, three precommits, real chunks again, `bft_update`; assert `height == 1`. The scratch test `r2_k4_faulty_flag_set_before_the_certificate_on_correct_bytes` drives this and prints.
4. **Rollout.** Node-local, no consensus or wire change. Can land after the diff; it does not block it.

## Validation Information

**Verdict: CONFIRMED. Severity: Medium.**

Built and run: a scratch crate assembled from the engine source plus `scratch_common.rs`, once from the revised working tree and once from HEAD 6705b3ca5e234. The network ordering claim is from reading only.

| Claim | Verified at |
|-|-|
| Real chunks, conflicting chunk, precommits, then 8 rounds of real chunks, precommits and `bft_update`: height stays 0; proposal complete, hash matches, flag set, validity `Fail`, `yes_precommits` 3 | run, `r2_k4_faulty_flag_set_before_the_certificate_on_correct_bytes`, revised diff |
| Resent real chunks return `Pass` | run, same test (first resend statuses) |
| HEAD gives the same result | run, same test, HEAD |
| The repeat check returns before the certified branch | read, `lib.rs:879-892`, `:1001` |
| The attacker can deliver real chunks then the conflicting one ahead of honest peers | inferred from `lib.rs:2178-2190` (round served on the tick after a status); not run |

**Severity justification.**

*Why not High:* no safety violation; one victim per attack; only nodes that have not cached validity; a restart clears it; HEAD has the same outcome.

*Why not Low:* permanent for the running process; the attacker needs one signed chunk and control of order on its own link; the node is left unable to decide a value it fully holds, which is the outcome the change's fourth claim rules out.

**Corrections made during validation.**

1. None. The lead came from asking whether "complete and not faulty is kept" could leave a complete and faulty proposal with correct bytes unhealed, and reproduced on first run.

**What this issue does not claim.** It does not claim the revised diff makes anything worse, that wrong bytes can be kept as a complete non-faulty value (no such path was found), or that a node which already validated the proposal is affected.

**Cross-references.** `a-conflicting-chunk-discards-a-complete-hash-verified-certified-proposal-so-a-replayed-packet-delays-a-decision-head-would-make.md` is the neighbouring order (certificate first, conflicting chunk second) and is fixed in the working tree. No ordering between them.

DO NOT DEVIATE FROM THIS TEMPLATE (`reviews/SECURITY-ISSUE-TEMPLATE.md`).
LEAVE THIS MESSAGE IN PLACE SO THAT ALL AGENTS KNOW NOT TO DEVIATE FROM THIS TEMPLATE (`reviews/SECURITY-ISSUE-TEMPLATE.md`).
