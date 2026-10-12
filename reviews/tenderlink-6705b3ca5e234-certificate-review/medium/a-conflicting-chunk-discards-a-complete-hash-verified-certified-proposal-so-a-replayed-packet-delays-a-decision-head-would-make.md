# For a certified proposal any chunk that does not fit restarts the assembly, even when the stored bytes are complete and already hash to the id, so one replayed proposer-signed packet throws away a verified value and delays a decision that HEAD would make at the next tick

**Severity**: Medium
**Validation Status**: Confirmed
**Location**: `tenderlink/src/lib.rs:986-992` (certified branch: restart on a size, valid round or chunk signature mismatch, with no test for a complete proposal), `:375-381` (`restart_proposal_assembly` drops the bytes and the cached validity), `:1348-1351` (line 49 needs the complete proposal and `Pass`), `:1979-1982` (the send loop relays every chunk a node holds)
**Found by agent:** adversarial review subagent, 2026-10-12, against the uncommitted diff to `tenderlink/src/lib.rs` on top of `dev` 6705b3ca5e234; validated the same day by running scratch tests against that diff and against HEAD
**In scope of audit?** Yes. Judged against the stated purpose of the uncommitted change ("a proposer must not be able to condemn a decided value by signing wrong bytes under its id"). The code is new in the diff. In one state it is a regression relative to HEAD 6705b3ca5e234; in the other states it is an improvement on HEAD that stops short. No deployed branch carries the diff.

## Description

For a proposal with 2f+1 precommits the diff replaces "mark faulty" with "restart the assembly". In the certified branch (`lib.rs:987-992`) a chunk sent under the certified id restarts the assembly when the faulty flag is set, when its size or valid round differs from what is stored, or when the chunk index is already filled with a different signature. `restart_proposal_assembly` (`lib.rs:375-381`) empties the buffer and resets `proposal_checked_validity`.

The stated reasoning, from the comment at `lib.rs:976-978`: "its bytes are whatever hashes to its id. So nothing sent under its id can condemn it; a chunk that does not fit only restarts the assembly."

The branch does not distinguish a partial assembly from a complete one whose hash has already been checked against the id (`lib.rs:1047-1049`). Once the stored bytes hash to the certified id they are the value; no later chunk can be more right. The code discards them anyway, and line 49 (`lib.rs:1348-1351`) then cannot fire until the value is reassembled with no conflicting chunk arriving before the tick.

**This is a delay that lasts as long as the attacker keeps one packet arriving after the real chunks, not a permanent condemnation and not a safety defect.** The packet must carry the round proposer's signature over a conflicting chunk, so a Byzantine proposer has to have produced it once; after that any peer can replay it.

## Attack Scenario and Steps

A Byzantine round proposer creates the packet; it or any peer that has seen the packet replays it.

1. Four finalizers, threshold 3. Proposer P proposes value A (two chunks) in round 0. Node N assembles A, the hash matches, N runs line 22 (validity cached `Pass`) and prevotes.
2. Three precommits for A reach N. The proposal is certified. N would decide at its next tick.
3. Before the tick, N receives chunk 1 of other bytes that P signed under A's id. At `lib.rs:990` the stored signature differs, so the assembly restarts: buffer emptied, validity back to `Indeterminate`, then the conflicting chunk is stored as chunk 1.
4. At the tick, `has_enough_info_to_determine_validity` is false. N does not decide. At HEAD the same packet sets `proposal_is_faulty`, the cached `Pass` stands, and N decides.
5. Each tick honest peers resend A's chunks (the first real chunk 1 restarts again, then the value completes) and the attacker resends the conflicting chunk. Whenever the conflicting chunk is processed after the last real chunk and before the tick, N loses the value again.

**Attack Requirements and Assumptions:**

- One conflicting chunk signed by the round's proposer under the certified id. Only the proposer can create it.
- A peer connection to the victim and enough packets per tick to land after the honest chunks. Message order is shuffled at the sender (`lib.rs:2361`), so with N conflicting and M real chunk packets per tick a one-chunk value survives a tick with probability about M/(N+M). Inference, not measured.
- Inference from `lib.rs:1979-1982`: an honest node that has stored the conflicting chunk after a restart relays it with the proposer's valid signature, so honest nodes can pass it on for a tick without the attacker.

## Impact on Users

Node operators whose node is certified on a value and has not yet decided: live nodes in the window between the last precommit and the tick, and nodes catching up, which stay in that state longer while validity is `Indeterminate`.

- **A live node misses a decision it would have made at HEAD.** It decides when it next holds the complete value at a tick, or in a later round. Inference: honest nodes are locked on the value, so a later round with an honest proposer decides it; the cost is rounds, which a proposer can also waste by proposing nothing.
- **A node catching up can be held at a height for as long as the replay continues.** At HEAD one such packet before validation holds it there permanently, so the diff is still an improvement for this case.
- **Validation is repeated.** Each reassembly clears the cached validity, so the validate closure runs again per reassembly.

**Detectability, stated precisely.** The victim logs nothing on the restart path at `lib.rs:991`. `DECIDE_WAIT` shows `yc` at the threshold with the proposal count dropping back, which an operator would have to notice.

## Technical Details / Code Analysis

**The certified branch restarts without asking whether the stored bytes are already verified** (`tenderlink/src/lib.rs:986-992`):

```rust
                if is_prev_seen_round && round_data.proposal_sigs_n > 0 {
                    if certified {
                        if round_data.proposal_is_faulty ||
                           round_data.proposal.0.len() != proposal_size || round_data.proposal_valid_round != valid_round ||
                           (round_data.proposal_sigs[chunk_i] != TMSig::NIL && round_data.proposal_sigs[chunk_i] != sig) {
                            round_data.restart_proposal_assembly();
                        }
```

**The restart drops the bytes and the cached validity** (`tenderlink/src/lib.rs:375-381`):

```rust
    fn restart_proposal_assembly(&mut self) {
        self.proposal                  = BlockValue(Vec::new());
        self.proposal_sigs             = Vec::new();
        self.proposal_sigs_n           = 0;
        self.proposal_checked_validity = (TMStatus::Indeterminate, TMStatusReason::None);
        self.proposal_is_faulty        = false;
    }
```

**Output of the scratch test on the diff, then on HEAD:**

```text
R2-F2 validated_first=true: full before true; conflicting chunk status Pass; after (full, sigs_n, faulty, validity) (false, 1, false, Indeterminate); height after that tick 0; after real chunks resent 1; 50 ticks of [real x2, conflicting x1] height Some(0)
R2-F2 validated_first=false: full before true; conflicting chunk status Pass; after (full, sigs_n, faulty, validity) (false, 1, false, Indeterminate); height after that tick 0; after real chunks resent 1; 50 ticks of [real x2, conflicting x1] height Some(0)
R2-F2 validated_first=true: full before true; conflicting chunk status Fail; after (full, sigs_n, faulty, validity) (true, 2, true, Pass); height after that tick 1; after real chunks resent 1; 50 ticks of [real x2, conflicting x1] height Some(0)
R2-F2 validated_first=false: full before true; conflicting chunk status Fail; after (full, sigs_n, faulty, validity) (true, 2, true, Indeterminate); height after that tick 0; after real chunks resent 0; 50 ticks of [real x2, conflicting x1] height Some(0)
```

The first two lines are the diff, the last two are HEAD.

## Recommendations

1. **Refuse every further chunk once a certified proposal is complete and its hash has matched.** In the certified branch, before the restart test: if `has_full_proposal()` and the faulty flag is clear, return without touching the round. The hash was checked at completion (`lib.rs:1047-1049`), so a complete, non-faulty, certified buffer is the value. This removes the regression and makes a verified value immune to replay.
2. **Keep a partial assembly from being reset by one chunk.** For the incomplete case, an option is to keep the stored chunk and hold the newcomer aside for the same index (two candidates per index), checking the hash over the combinations at completion. This costs memory and code; see alternatives.
3. **Alternatives considered.** Dropping the peer that sent the conflicting chunk: the peer may be honest and relaying what it holds. Rate-limiting restarts per round: bounds the work but not the delay. Recommendation 2 is optional; recommendation 1 alone closes the confirmed regression and the catch-up case once a node completes the value once.
4. **Tests.** Extend `line49_wrong_bytes_under_a_decided_id_do_not_condemn_it`: after the real value is complete and three precommits are stored, deliver one conflicting chunk, run `bft_update`, assert `height == 1`. The scratch test `r2_f2_conflicting_chunk_after_complete_certified_proposal` drives this and prints.
5. **Rollout.** Node-local, no consensus or wire change. Land with the diff: without recommendation 1 the diff changes a decision HEAD makes into a missed one.

## Validation Information

**Verdict: CONFIRMED. Severity: Medium.**

**Status: fixed, and committed with the change as `5a6577e2` (re-validated 2026-10-12 against the revised uncommitted diff on `dev` 6705b3ca5e234).** The revision keeps a certified proposal that is complete and not faulty: a chunk for it returns `Pass` if size and chunk signature match what is stored and `Fail` otherwise, before the restart test (`lib.rs:1002-1007` in the revised file). This is recommendation 1. The restart path now logs a line (`:1011-1012`). The revision adds the test `line49_a_conflicting_chunk_does_not_cost_a_node_the_decided_value_it_has` (`:4077`), which is recommendation 4. Recommendation 2 (partial assemblies) is not done. Line numbers elsewhere in this report are those of the first diff.

Built and run: a scratch crate assembled from the engine source plus `scratch_common.rs`, once from the working tree and once from HEAD 6705b3ca5e234. Probability and relay claims are from reading only.

| Claim | Verified at |
|-|-|
| Complete, hash-verified, validated, certified proposal plus one conflicting chunk: diff does not decide at that tick | run, `r2_f2_conflicting_chunk_after_complete_certified_proposal`, diff |
| Same sequence at HEAD decides at that tick | run, same test, HEAD |
| Without prior validation HEAD never decides and the diff decides once the real chunks are resent | run, same test, both trees |
| Real chunks then the conflicting chunk each tick: no decision in 50 ticks | run, same test, diff |
| Revised diff: the conflicting chunk returns `Fail`, the proposal stays complete, the node decides at that tick in both states, and decides under 50 ticks of real chunks then the conflicting chunk | run, `r2_f2_conflicting_chunk_after_complete_certified_proposal`, revised diff |
| Revised diff: no path found to a complete, non-faulty proposal whose bytes do not hash to its id. The hash is checked whenever the last chunk is stored; adoption and both amnesiac flushes change the id only together with emptying the assembly; rounds built by zebra-state enter only `recent_commit_round_cache` | read, revised `lib.rs:1074-1086`, `:397-399`, `:418-436`, `:1812`; run, fuzz invariant "complete and not faulty implies hash equals id", 800 seeds |
| Per-tick survival probability about M/(N+M) | inferred from `lib.rs:2361`; not run |
| Honest nodes relay a stored conflicting chunk | inferred from `lib.rs:1979-1982`; not run |
| A later round decides the value when the attacker is not its proposer | inferred from lines 36 and 49 in `bft_update`; not run |

**Severity justification.**

*Why not High:* no safety violation; the delay ends when the replay stops or a later round decides; only the round's proposer can create the packet; for nodes that had not validated, the diff is strictly better than HEAD.

*Why not Low:* in the validated state it is a liveness regression relative to HEAD confirmed by running both trees; the cost to the attacker is one stored packet replayed per tick; it leaves the diff's stated purpose only partly met.

**Corrections made during validation.**

1. My first reading treated this as the diff being worse than HEAD in general. Running both trees showed HEAD is worse when the node has not cached validity (it never decides), so the regression is confined to the validated state.

**What this issue does not claim.** It does not claim a permanent halt, that the attacker gains more at network level than a proposer that proposes nothing, or that the 50-tick result holds for other packet orders: in that test the conflicting chunk is always the last packet before the tick.

**Cross-references.** `a-faulty-flag-set-on-correct-bytes-before-the-certificate-is-never-cleared-because-the-real-chunks-return-at-the-repeat-check-so-the-node-never-decides.md` is the neighbouring order (conflicting chunk before the certificate) and is open. `precommits-stored-before-the-proposal-are-never-checked-for-a-certificate-so-a-node-handed-another-value-afterwards-never-decides-the-height.md` is independent. No ordering between them.

DO NOT DEVIATE FROM THIS TEMPLATE (`reviews/SECURITY-ISSUE-TEMPLATE.md`).
LEAVE THIS MESSAGE IN PLACE SO THAT ALL AGENTS KNOW NOT TO DEVIATE FROM THIS TEMPLATE (`reviews/SECURITY-ISSUE-TEMPLATE.md`).
