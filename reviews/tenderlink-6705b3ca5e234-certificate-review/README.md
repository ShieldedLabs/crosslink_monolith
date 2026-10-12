# Tenderlink certificate review: the uncommitted diff on top of `dev` 6705b3ca5e234

**Target.** The uncommitted change to `tenderlink/src/lib.rs` on top of `dev` 6705b3ca5e234 ("Tenderlink: count votes for a value other than the held proposal"): a second value slot per finalizer per step, adoption of a value with 2f+1 precommits, restart instead of condemnation for a certified proposal, and a size and index bound on proposal chunk headers. Line numbers in the reports are those of the working tree file on 2026-10-12.

**Source.** An adversarial review subagent, 2026-10-12. Every finding here was derived from reading the diff and the surrounding engine, then confirmed by running.

**Method.** Read: the diff, `RoundData` and its methods, `ConsensusCounts`, `check_and_incorporate_msg`, `bft_update`, `has_prevote_certificate`, `round_data_to_fat_pointer`, `broadcast`, the send and receive loops, and `decided_round_data` and the finalizer status code in `zebra-crosslink/zebra-state/src/new_network/bft.rs`. Run: a scratch crate assembled from the engine source plus scratch tests, from the working tree and from HEAD; and the engine's own tests in place (25 passed, 1 failed: `line28_inactive_roster_members_ignored`, which fails in its own setup on the unchanged tree). Nothing under `zebra-crosslink` was built. No live network was run, so every statement about packet timing is an inference and is labelled so in the reports.

**Second pass, 2026-10-12.** The working tree was revised for K1 and K2. The K1, K2 and K3 scenarios and the 800-seed fuzz were re-run against the revised diff, with a scratch build in its own target directory, and a new scenario was added (K4). Results: K1 and K2 no longer reproduce; K3 reproduces unchanged and is left open on purpose; K4 reproduces on the revised diff and on HEAD. The engine's own tests in place: 27 passed, 1 failed (the same known one). Logs: `run2_new.log`, `run2_fuzz.log`, `run2_old.log`.

**Landed.** The revised diff was committed to `dev` as `5a6577e2` ("Tenderlink: take a value that has 2f+1 precommits, whatever proposal we hold"), with the K1 and K2 fixes in it. Where this folder says "the working tree", "the diff" or "HEAD", read: the change before it was committed, and `6705b3ca5e234` below it. K4 and K3 are open at `5a6577e2`; K4 was held back on purpose. The scratch material and logs named here were in a temporary directory and are not in the repository.

**Scratch material**, in the directory above this folder: `scratch_common.rs` (F1 and F2 scenarios, builds against both trees), `scratch_new.rs` (F3 scenario and a randomized invariant check, 800 seeds), `assemble2.py`, and the logs `run_new.log`, `run_fuzz.log`, `run_old.log`, `run_engine_own.log`.

## Findings

| Id | Sev | Finding | Regression vs HEAD | Confirmed by |
|-|-|-|-|-|
| K1 | medium | [Precommits stored before the proposal are never checked for a certificate](medium/precommits-stored-before-the-proposal-are-never-checked-for-a-certificate-so-a-node-handed-another-value-afterwards-never-decides-the-height.md) | No. HEAD is stuck in both arrival orders; the first diff fixed one. **Fixed in `5a6577e2`** (`adopt_any_certified`) | Running, both trees; fix re-run |
| K2 | medium | [A conflicting chunk discards a complete, hash-verified, certified proposal](medium/a-conflicting-chunk-discards-a-complete-hash-verified-certified-proposal-so-a-replayed-packet-delays-a-decision-head-would-make.md) | Was a regression in one state in the first diff. **Fixed in `5a6577e2`** (a complete, non-faulty certified proposal is kept) | Running, both trees; fix re-run |
| K4 | medium | [A faulty flag set on correct bytes before the certificate is never cleared](medium/a-faulty-flag-set-on-correct-bytes-before-the-certificate-is-never-cleared-because-the-real-chunks-return-at-the-repeat-check-so-the-node-never-decides.md) | No. HEAD has the same outcome. Open | Running, revised diff and HEAD |
| K3 | low | [A third value vote replaces the second in the other slot](low/a-third-value-vote-replaces-the-second-in-the-other-slot-so-a-triple-signer-can-hide-a-line-28-prevote-certificate.md) | No. HEAD refuses the second value outright. Open, left as is on purpose | Running, both diffs |

## Landing order

1. K1 and K2 fixes went in with the diff, as `5a6577e2`.
2. K4 recommendation 1 after the diff. It does not block it.
3. K3 any time after.

## Rows for `REVIEW_STATUS.md`

Added to the tracking table on 2026-10-12 with these ids.

| K1 | [Precommits before the proposal are never checked for a certificate](tenderlink-6705b3ca5e234-certificate-review/medium/precommits-stored-before-the-proposal-are-never-checked-for-a-certificate-so-a-node-handed-another-value-afterwards-never-decides-the-height.md) | medium | adversary | launch | FIXED `5a6577e2` | unit | tenderlink | Fixed by `adopt_any_certified` after the first chunk; scenario and 800-seed fuzz re-run clean. |
| K2 | [Conflicting chunk discards a verified certified proposal](tenderlink-6705b3ca5e234-certificate-review/medium/a-conflicting-chunk-discards-a-complete-hash-verified-certified-proposal-so-a-replayed-packet-delays-a-decision-head-would-make.md) | medium | adversary | launch | FIXED `5a6577e2` | unit | tenderlink | Fixed by keeping a complete, non-faulty certified proposal; scenario re-run clean.. Partial assemblies can still be restarted by a conflicting chunk (recommendation 2, open) |
| K4 | [Faulty flag on correct bytes before the certificate is never cleared](tenderlink-6705b3ca5e234-certificate-review/medium/a-faulty-flag-set-on-correct-bytes-before-the-certificate-is-never-cleared-because-the-real-chunks-return-at-the-repeat-check-so-the-node-never-decides.md) | medium | adversary | launch | CLEAR | unit | tenderlink | Real chunks, then a conflicting chunk, then the certificate: the node holds the right bytes and never decides. Confirmed by running on the revised diff and on HEAD; not a regression |
| K3 | [Third value vote hides a line 28 certificate](tenderlink-6705b3ca5e234-certificate-review/low/a-third-value-vote-replaces-the-second-in-the-other-slot-so-a-triple-signer-can-hide-a-line-28-prevote-certificate.md) | low | adversary | any | CLEAR | unit | tenderlink | Needs a finalizer signing three prevotes in a round; intermittent; HEAD is worse |

Recommendation rows:

| K1.1 | Run the adoption check when the first chunk of a round makes the proposal known | DONE `5a6577e2` | `adopt_any_certified`; re-run clean |
| K1.3 | Add a test: three precommits, then the other value's chunks, then the decided value's chunks; assert the height advances | DONE `5a6577e2` | `line49_precommits_that_came_before_any_proposal_still_decide` passes |
| K2.1 | Refuse further chunks once a certified proposal is complete and its hash has matched | DONE `5a6577e2` | Re-run clean |
| K2.2 | Keep a partial certified assembly from being reset by one chunk | OPEN | Optional; needs a design choice |
| K2.4 | Extend `line49_wrong_bytes_under_a_decided_id_do_not_condemn_it` with a conflicting chunk after completion | DONE `5a6577e2` | `line49_a_conflicting_chunk_does_not_cost_a_node_the_decided_value_it_has` passes |
| K4.1 | When a proposal is certified, complete and flagged faulty, recompute the hash: clear the flag if it matches, restart if not | OPEN | Not started |
| K4.3 | Add a test: real chunks, a conflicting chunk, three precommits, real chunks again; assert the height advances | OPEN | Scratch test `r2_k4_faulty_flag_set_before_the_certificate_on_correct_bytes` exists and prints |
| K3.1 | Do not let a newer other-value prevote displace one that completes a 2f+1, or latch the certificate per round | OPEN | Not started |
| K3.3 | Add a `line28_*` test with one finalizer signing three prevotes | OPEN | Scratch test `r2_f3_third_value_displaces_the_vote_line_28_needs` exists and prints |
