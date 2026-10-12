# Tenderlink independent second review: `dev` 5a6577e2726e5

**Target.** The two newest commits on local `dev`, reviewed together:

- `6705b3ca` "Tenderlink: count votes for a value other than the held proposal"
- `5a6577e2` "Tenderlink: take a value that has 2f+1 precommits, whatever proposal we hold"

Base before them: `e78c6ca6`. The change is 618 lines in `tenderlink/src/lib.rs` and one line in `zebra-crosslink/zebra-state/src/new_network/bft.rs`.

**Source.** Independent second adversarial review subagent, 2026-10-12, validated at `dev` 5a6577e2726e5. The first review of the same change is in `../tenderlink-6705b3ca5e234-certificate-review/` (findings K1 to K4). This pass set out to find what the first review did not, and to settle the four specific items in the brief.

**Method.** Read: the full combined diff, `RoundData` and its new methods (`other_vote`, `set_other_vote`, `stake_voting_for`, `prefer_votes_for`, `recount`, `big_threshold`, `proposal_is_certified`, `restart_proposal_assembly`, `adopt_if_certified`, `adopt_any_certified`), `ConsensusCounts`, `check_and_incorporate_msg`, `bft_update` (lines 22/28/34/36/44/47/49/55), `has_prevote_certificate`, `round_data_to_fat_pointer`, `flush_for_amnesiac_proposer`, the send loop (`send_round_data_to_peer`, `PacketVotes`), the receive loop, `entry_point`, and in `zebra-crosslink/.../bft.rs` the `decided_round_data`, `decide`, `validate_signatures`, and roster-building code. Run: the engine's own test suite in place (`cargo test --lib -- --skip single_rt`: 27 passed, 1 known-unrelated failure, `line28_inactive_roster_members_ignored`, index out of bounds in its own setup); a standalone reproduction of `PacketVotes::read_from`; a standalone evaluation of `f_from_n`/`big_threshold`. Nothing under `zebra-crosslink` was built, and no live network was run, so every statement about packet timing and about the fat-pointer decision path is an inference from reading and is labelled so in the reports.

## Verdict on the two commits together

**NON-BLOCKING ISSUES ONLY.** No defect was found that these commits introduce: no new safety break, no new party-reachable panic (the new proposal-chunk size and index guards in fact remove a pre-existing unbounded-allocation and slice-underflow hazard), and no liveness regression relative to `e78c6ca6` (the commits strictly improve the split and other-value cases, where the base could halt but the new code decides; the residual delays require a faulty proposer and are acknowledged in the commit message). The two confirmed High findings below both predate these commits and are present on the base and the deployed engine; they are reported because the brief asks for them and because 5a6577e2's new adopt-other-value logic and its comment rest on the premise that K6 shows is false.

## Findings

| Id | Sev | Finding | Regression vs base | Confirmed by |
|-|-|-|-|-|
| K5 | high | [A crafted vote packet indexes past the 18-slot array, so any connected peer aborts a finalizer node](high/a-crafted-vote-packet-indexes-past-the-18-slot-array-so-any-connected-peer-aborts-a-finalizer-node.md) | No. Predates; `PacketVotes` and its reader are untouched by the commits | Running (standalone repro, both profiles) and reading |
| K6 | high | [The 2f+1 quorum is below two thirds when total stake is not 3f+1, so f equivocators can certify two values](high/the-2f-plus-1-threshold-is-below-two-thirds-when-total-stake-is-not-3f-plus-1-so-f-equivocators-can-certify-two-values.md) | No. Predates; the threshold formula is unchanged by the commits | Running (standalone arithmetic) and reading |

## Answers to the four specific items

1. **Issue 251 compliance.** At the counting level the committed code meets the "Expected" clauses (a), (b) and (c): a correctly signed vote for any value from an active roster member now counts toward `anys`/`prevotes`/`precommits` (`ConsensusCounts::from`, `tenderlink/src/lib.rs:512-544`), only the `yes_*` tallies depend on the held proposal, and a vote for a value we do not hold is stored rather than refused (`:1110-1158`) while two different value votes from one finalizer are still kept apart (first in `msg_val_sigs`, second in `msg_other_sigs`). This covers line 34 (the engine tests `line34_*`, `split_*` exercise it) and, by the same counts, line 47 (2f+1 precommits) and line 55 (f+1 anys from a later round): a finalizer's first vote in a step always lands in `msg_val_sigs`/`msg_nil_sigs` and is counted, and a second value vote only ever lands in `msg_other_sigs` when a first vote is already counted, so no finalizer is missed from the any-vote totals. **Where it falls short:** the network-propagation half is explicitly deferred. The send loop refuses to relay a vote whose value differs from the packet's single `value_id` (`tenderlink/src/lib.rs:2094-2098`), so a node counts another value's votes only when it receives them directly from their signer or from a peer that holds that value as its proposal. In a topology where some finalizers cannot reach each other directly, the split that issue 251 describes can still leave a node short of 2f+1 for lines 34/47/55. The counting fix is sound; the relaying is not done, as the commit message states.

2. **The two read-only leads.**
   - **Packet arithmetic panic: CONFIRMED, release node affected.** `PacketVotes::read_from` writes `packet.votes[i]` for `i` up to `no_votes_n + yes_votes_n` into an 18-element array, before any signature or roster check. A sum of 19 to 255 is an out-of-bounds write that panics in every profile (the array bounds check is unconditional), so the shipping `zebrad` release build (`panic = "abort"`) aborts; a sum above 255 and the `round + 0x8000_0000` add additionally overflow, but only under `overflow-checks`, which is the dev and test profile, not release. Reproduced by running a standalone copy of the reader. See K5.
   - **Quorum intersection: CONFIRMED, the comment's claim is false.** With `f = (n-1)/3` and quorum `2f+1`, the quorum is below `floor(2n/3)+1` for every total stake `n` not equal to `3f+1`. The smallest breaking distribution is 5 equal unit stakes (`f=1`, quorum 3); the deployed 12-seat equal-stake roster gives `f=3`, quorum 7 against a correct 9, so 3 equivocating finalizers can certify two values. The comment in `adopt_if_certified` ("Only one value can get there with at most f faulty") is wrong there. This predates the commits (the formula is unchanged) and governs the pre-existing line 49 decision and line 28 certificate as well as the new adopt path. See K6.

3. **Whole-network behaviour over rounds and heights (reasoned from code plus the engine's multi-engine tests; a multi-second real-timer, multi-height scenario was not run).** A split round that ends by timeout proceeds as ordinary Tendermint: 2f+1 prevotes start the prevote timeout (line 34), which precommits nil, 2f+1 nil precommits start the precommit timeout (line 47), which starts the next round; the commits do not touch this machinery, and each round gets a fresh `RoundData`, so the new per-round state (`msg_other_sigs`, adopt, recount) does not leak across rounds. A node that adopted a certified value while others advanced catches up: it keeps the 2f+1 precommits, drops its own bytes, fetches the decided value's chunks from peers serving the height out of `recent_commit_round_cache`, validates, and decides (this is what `line49_node_given_another_value_decides`, `line49_precommits_that_came_before_any_proposal_still_decide`, and `split_network_with_one_proposal_decides` show, all passing). No liveness regression relative to the base was found; on the contrary, the base could halt in exactly the split and other-value cases the commits fix. The one residual is a faulty proposer that interleaves conflicting chunks to keep restarting a partial assembly (acknowledged in the commit as "Not done here"); it needs a faulty proposer and is no worse than withholding, which a faulty proposer can always do.

4. **Could a test pass for the wrong reason?** `split_gossip` (`tenderlink/src/lib.rs:3834-3862`) relays every first-slot vote a node holds regardless of the node's own proposal id, which the real send loop does not (it filters to one `value_id` per packet), and it does not relay the second (`msg_other_sigs`) slot, which neither sends. I checked the two multi-engine tests whose conclusions could depend on this. `line34_two_and_two_...` does not: each node also receives the opposing side's votes directly from the opposing holders, which the real send loop does relay (those are the holders' own-proposal votes), so each node reaches 2f+1 any-votes without the helper's over-relay. `line49_node_given_another_value_decides` does not: the laggard adopts the decided value from 2f+1 precommits that live in first slots and are relayed by the real send loop too. No test's conclusion was found to rest on the helper relaying more than the real loop would.

## What was checked and found sound

- The new proposal-chunk guards (`tenderlink/src/lib.rs:984`, `:1052`, `:2480`, `PROPOSAL_MAX_SIZE`) bound allocation and prevent the receive-loop size subtraction from underflowing; they close a pre-existing hazard rather than open one.
- `stake_voting_for` reads both value slots and sums over distinct roster indices, so it neither double-counts a finalizer nor lets a 2f+1 for an unheld value be hidden; `has_prevote_certificate` now uses it (line 28).
- `adopt_if_certified` refuses to give up a value that is itself certified, sets `valid_round = -1`, and recounts; the fat pointer it later builds collects the adopted value's precommit signatures, which validate.
- The `debug_assert` that recomputes counts absolutely and compares with the incremental counts (`:1183-1192`) did not fire in any of the 27 passing tests, so no counts drift was observed in the tested paths.
- Indexing in `recount`, `adopt_any_certified`, `prefer_votes_for`, and the certified-chunk branches is in range because `msg_val_sigs`, `msg_nil_sigs`, and the roster all have the active-roster length within a height, and the certified-chunk branch conditions short-circuit before indexing `proposal_sigs[chunk_i]` on a size mismatch.

## What could not be determined

- The fat-pointer decision path in `zebra-crosslink/.../bft.rs` (`validate_signatures`, `decide`) was read but not built or run; statements about an adopted value's certificate validating are inferred from reading.
- A multi-second real-timer, multi-height run of several engines was not performed; item 3 is reasoned from the code and the passing in-process multi-engine tests.
- Whether the STP listener accepts inbound connections from arbitrary non-configured peers (as opposed to only configured peers) was not traced to a conclusion; K5 is stated for "any peer that completes the STP/Noise handshake," which certainly includes every configured peer.

## Landing order

1. K5 first, since a crashing node exhibits no consensus behaviour.
2. K6 next; it changes the consensus decision rule and wants a coordinated rollout (a hardfork-gated threshold change).
3. The issue 251 relaying gap (shortfall under item 1) and the faulty-proposer restart residual (item 3) are tracked by the first review's open items and the commit's own "Not done here" list; neither blocks these commits.

## Rows for `REVIEW_STATUS.md`

To be added to the tracking table (not edited here, per the brief).

| K5 | [Vote packet indexes past the 18-slot array](tenderlink-5a6577e2726e5-review/high/a-crafted-vote-packet-indexes-past-the-18-slot-array-so-any-connected-peer-aborts-a-finalizer-node.md) | high | adversary | any | CLEAR | unit | tenderlink | Any connected peer aborts the release node with one vote packet whose vote count exceeds 18; predates the two commits, not a regression. Confirmed by a standalone repro in both profiles |
| K6 | [2f+1 quorum below two thirds when stake is not 3f+1](tenderlink-5a6577e2726e5-review/high/the-2f-plus-1-threshold-is-below-two-thirds-when-total-stake-is-not-3f-plus-1-so-f-equivocators-can-certify-two-values.md) | high | adversary | any | DECIDE | unit | tenderlink | With a roster whose stake is not 3f+1 (e.g. 12 equal finalizers), f equivocators certify two values and fork the height; predates the commits; the new adopt path and its comment rest on the false single-value premise. Confirmed by standalone arithmetic |

Recommendation rows:

| K5.1 | Reject a vote packet whose `no_votes_n + yes_votes_n` exceeds the 18-slot array before the read loop, widening to `usize` there and in the receive loop | OPEN | Not started |
| K5.2 | Build the signing round with `round \| 0x8000_0000`, not `round + 0x8000_0000`, to remove the overflow-checks-only panic | OPEN | Not started |
| K5.4 | Add a unit test: `read_from` with 10+10 and 255+255 vote counts returns `None` rather than panicking | OPEN | Standalone repro exists under the review scratch directory |
| K6.1 | Replace `2f+1` with a strict `floor(2n/3)+1` quorum in `big_threshold`, `bft_update`, `has_prevote_certificate`, and `proposal_is_certified` | OPEN | Not started |
| K6.2 | Correct or delete the `adopt_if_certified` single-value comment | OPEN | Not started |
| K6.4 | Add tests: `big_threshold(n) > 2n/3` for a range of `n`; a 5-finalizer engine test that at most one value is certified under one equivocator | OPEN | Standalone arithmetic check exists under the review scratch directory |

## Recommendation on the two commits

Both commits are safe to keep: they fix a real halt (issue 251) and a real inability to decide an other-value quorum, and they add defensive bounds that remove a pre-existing hazard. They do not introduce a blocking defect. Before relying on BFT finality in production, schedule K6 (the quorum threshold) as a coordinated, hardfork-gated change, and land K5 (the vote-packet crash) promptly as a parsing hardening with no consensus impact.
