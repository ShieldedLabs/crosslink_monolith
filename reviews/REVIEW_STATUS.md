# Code review status: 29 reported findings, 15 test-format leads and 4 campaign leads

Tracking table. One row per finding. Six sources feed it:

- **Consensus review** (`C` rows): 11 reports in `crosslink-e99404e3de7cc-review/`, validated at `dev` @ `e99404e3de7cc`.
- **GUI review** (`G` rows): 10 reports in `crosslink-d00b6a44ef594-gui-review/`, validated at `dev` @ `d00b6a44ef594`.
- **Sync follow-up** (`S` rows): 1 report in `crosslink-e56e35f85a98f-sync-review/`, a lead raised while re-checking commit `e56e35f8` and validated at `dev` @ `e56e35f85a98f`; and 1 in `crosslink-890036389ac3b-ingest-review/`, found by a scenario and validated at `dev` @ `890036389ac3b`.
- **Test-format review** (`F` and `T` rows): 15 leads on `test_format.rs` and `zebrad/tests/crosslink.rs`, raised at `dev` @ `8ea4db1f`. They have no report files and no severity. The two sessions that own that code checked them and disputed none; see the section below.
- **Tenderlink certificate review** (`K` rows): 4 reports in `tenderlink-6705b3ca5e234-certificate-review/`, from an adversarial review of a change to `tenderlink/src/lib.rs` before it was committed, on top of `dev` @ `6705b3ca5e234`. The change landed as `5a6577e2`.
- **Tenderlink second review** (`K5`, `K6`): 2 reports in `tenderlink-5a6577e2726e5-review/`, from a second, read-only review of the same two commits, validated at `dev` @ `5a6577e2726e5`.
- **Fix campaign leads** (`N` rows): 4 leads with no report files, raised by the October 2026 fix campaign while it worked; see the section below.

Status is assessed against `dev` @ `e56e35f85` on 2026-10-01, except the `K` rows, which are assessed against `5a6577e2` on 2026-10-12. Every assessment outside the `K` rows is from reading code, and nothing was built or run for those reviews or for this table. The `K` findings were each confirmed by running a scenario against the engine. Rows the fix campaign closed were updated on 2026-10-26 on the `integrate` branch, which merges the campaign's seven branches onto `dev` @ `578092d0`; the integration report has the suites run there. Narrative, quotes and fix plans are in the report files. The structure every report follows, and the row formats used below, are in [`SECURITY-ISSUE-TEMPLATE.md`](SECURITY-ISSUE-TEMPLATE.md).

`C` numbers are the ones the consensus reports use to cross-reference each other, so they are kept as they are and the rows are sorted by severity.

### Column meanings

**Sev**: the containing folder: `high`, `medium` or `low`.

**Fires**: what it takes to trigger.

- Consensus rows: `honest` = an ordinary event such as a crash, a restart, a network split or a tip change · `adversary` = needs a dishonest proposer or miner · `config` = not reachable with the shipped parameters.
- GUI rows: every one fires in ordinary use, so the column gives the platforms: `all`, or the ones named.

**Gate**: when it has to land. `behind FeaturenetUpgrade2, unscheduled` = the fix is in, switched on by `NetworkUpgrade::FeaturenetUpgrade2`, which has no activation height on any network yet.

- `fork` = the fix changes consensus rules. It is free to land until BFT activation, which is height 36,288 on the shipped parameters. After that it needs a coordinated upgrade. This is a fact about the fix.
- `launch` = node-local or GUI-local, and it should be in before the public launch. This is a recommendation.
- `any` = no deadline.

**Status**: `FIXED` (with commit) · `PARTIAL` (bounded, mechanism survives) · `CLEAR` (obvious fix, not yet done) · `DECIDE` (needs a person) · `MEASURE` (needs a number first) · `EXTERNAL` (cannot be fixed in this repo).

**Test**: which harness exercises it. Three exist:

- `test-format`: the scenario tests in `zebra-crosslink/zebrad/tests/crosslink.rs`, about 45 of them. One in-process node, regtest only, proof of work disabled, ephemeral state, no restart.
- `dilated`: `zebra-crosslink/dilated_regtest/run.sh`. Two child-process nodes with real signatures, real templates and a restart past activation. It asserts liveness only.
- `cargo`: ordinary unit tests. `zebra-gui` has two small test modules and nothing that drives `run_ui` or the chain view.

A row a fix covered names the harness that covers it (`test-format`, or `cargo` for ordinary unit tests). Every other row shows a route not yet taken: `unit` = easy pure-logic test · `intg` = easy with a harness that already exists · `hard` = needs new infrastructure · `none` = manual only.

**Owner**: the area that can close it. `node` = `zebra-state` and its callers · `spec+node` = a consensus rule has to be decided first, then coded · `gui` · `gui+wallet` · `gui+softer_gui` = needs a change in the vendored windowing patch.

---

## Consensus review (11)

| # | Finding | Sev | Fires | Gate | Status | Test | Owner | Notes |
|-|-|-|-|-|-|-|-|-|
| C2 | [Tail Confirmation trusts each header's own `nBits`](crosslink-e99404e3de7cc-review/high/tail-confirmation-checks-carried-headers-against-their-own-nbits-so-a-proposer-can-fake-confirmations.md) | high | adversary | behind FeaturenetUpgrade2, unscheduled | FIXED `767abedd`, `d6d33d3a` | cargo | spec+node | From FeaturenetUpgrade2 at the snapshot's height, each carried header's `nBits` must equal `ThresholdBits` at its position, with its time inside the median-time-past bounds; `propose` declines a tail it would refuse. The minimum-difficulty start height is now a `testnet::Parameters` field, never on the featurenet, and the exemption ends wherever FeaturenetUpgrade2 is active. Covered by unit tests; the end-to-end forged-tail scenario needs a configured-testnet harness (lead N2) |
| C11 | [Fat pointer sits outside the Equihash input](crosslink-e99404e3de7cc-review/high/fat-pointer-sits-outside-the-equihash-input-so-miners-can-grind-the-block-hash-with-sha256d.md) | high | adversary | behind FeaturenetUpgrade2, unscheduled | FIXED `0bc75e55` | test-format | spec+node | From FeaturenetUpgrade2, `hashBlockCommitments` binds a BLAKE2b-256 digest of the fat pointer where ZIP 244 has its zero terminator; the template gains `fatpointerdigest`. The header layout and every pre-upgrade hash are unchanged. **Accepted residual:** headers carried inside BFT proposals cannot prove their binding from the header alone; that grind is bounded by C2's difficulty check and the sigma-headers cost. The grind test itself still needs PoW enabled in the harness. **On `integrate` two of its scenarios fail**, `crosslink_fat_pointer_changed_after_the_commitment_is_refused_from_featurenet_upgrade_2` and `crosslink_template_commits_to_its_fat_pointer_from_featurenet_upgrade_2`: their scene's pointers are unsigned and at round 1, which C7's check refuses first once the upgrade is active. A nil-roster scene at round 0 fixes the template test but not the refusal test, whose height and signature tampering C7 now refuses before the commitment is checked |
| C6 | [Dropped decided snapshot halts BFT and can abort](crosslink-e99404e3de7cc-review/high/dropped-decided-snapshot-leaves-validate-indeterminate-forever-and-decide-can-abort.md) | high | honest | launch | FIXED `2edd11b1` | test-format | node | `41f3517d` replaced the conflict hold with dropping BFT before the campaign. `2edd11b1` records the drop in the database (`crosslink_bft_dropped`, format 31.4.0) before the depth commit, so restore starts dropped only when the row exists; `decide` parks an indeterminate re-validation instead of asserting, and the push closure waits instead of panicking. Covered by a drop scenario and the decide race through the node's own engine. The dilated restart after a drop is not done |
| C7 | [Fat pointer certificate accepted by hash only](crosslink-e99404e3de7cc-review/medium/admit-fat-pointer-accepts-a-certificate-by-hash-without-checking-signatures.md) | medium | adversary | behind FeaturenetUpgrade2, unscheduled | FIXED `d66bad21` | test-format | spec+node | From FeaturenetUpgrade2, a PoW block's fat pointer, a proposal's previous-block pointer and a force-fed decision must carry a valid certificate for the decided height's voting roster, namespace and quorum. Before the upgrade every path stays hash-only. Covered by unit tests of each defect and a twelve-seat scenario on both sides of the upgrade (`eea988f6`) |
| C3 | [`MISSING_POW_BLOCKS` never expires](crosslink-e99404e3de7cc-review/medium/missing-pow-blocks-grows-without-bound-from-unverified-proposal-snapshots.md) | medium | adversary | launch | FIXED `89d1d757` | test-format | node | Entries expire unless a validation asks again within 30 s, the set holds at most 16, and BFT by-hash requests may hold a quarter of the download budget. The carried headers' PoW is checked before the snapshot lookup. Snapshots of the decided chain stay in their own set, so restore's refetch (C1) still works |
| C1 | [Restart exits when the decided snapshot was only in memory](crosslink-e99404e3de7cc-review/medium/restart-exits-when-the-decided-snapshot-is-still-only-in-the-non-finalized-state.md) | medium | honest | launch | FIXED `ca09bbdb` | intg | node | Restore replays the stored chain up to the first decision whose snapshot this state does not hold, starts BFT there and fetches that snapshot by hash so the height is decided again; the exit is gone. Only a recorded drop (C6) starts the node with BFT stopped. Flushing the backup at decision time (C1.3) was not done. No restart test has passed: the dilated run's new `LOSE_BACKUP` option timed out before its restart on the branch |
| C4 | [Restart exits between activation and genesis](crosslink-e99404e3de7cc-review/low/restart-exits-when-killed-between-activation-and-the-bft-genesis-decision.md) | low | honest | launch | FIXED `d458bf22` | hard | node | The exit is gone: an empty store above activation is logged and the first tick derives genesis again, which is a pure function of the headers above the roster height. No test: it needs the row-deletion tool C4.T and C10.4 share |
| C5 | [Bootstrap check ignores sigma](crosslink-e99404e3de7cc-review/low/bootstrap-genesis-headers-can-still-be-reorged-so-late-joiners-build-a-different-genesis.md) | low | config | any | CLEAR | unit | node | Not exposed: the margin is 1,724 blocks against a need of 99. The heights were re-chosen in `ca131a7d` and `00565513` without the fix |
| C8 | [Template pointer chosen against the live tip](crosslink-e99404e3de7cc-review/low/template-fat-pointer-is-chosen-against-the-live-tip-not-the-template-parent.md) | low | honest | any | CLEAR | intg | node | Costs one mined block, and only after a reorg deeper than sigma across a decision. Needs one new test-format instruction that names the template parent |
| C9 | [Template pointer walk cost under the read lock](crosslink-e99404e3de7cc-review/low/template-fat-pointer-walk-scans-the-whole-bft-chain-under-the-read-lock.md) | low | honest | any | MEASURE | intg | node | No timing has been taken. The loop bound is CLEAR and rides on C8. The clone-per-iteration cost is the part to measure |
| C10 | [`AGENTS.md` cast and misleading-default rules](crosslink-e99404e3de7cc-review/low/new-bft-code-breaks-agents-md-cast-comment-and-misleading-default-rules.md) | low | honest | launch | CLEAR | unit | node | 39 uncommented casts, all safe today. The part that matters: a missing stake record is treated as zero stake at two sites. Sigma of 0 is still accepted by config validation |

## GUI review (10)

| # | Finding | Sev | Fires | Gate | Status | Test | Owner | Notes |
|-|-|-|-|-|-|-|-|-|
| G1 | [Edit Stake amounts shown wrong](crosslink-d00b6a44ef594-gui-review/high/edit-stake-modal-shows-bond-amounts-wrong-because-the-fraction-is-not-zero-padded.md) | high | all | launch | FIXED `9f7dddcc` | cargo | gui | Upstream's fix: `format_stake_amount` is now a wrapper over `str_from_ctaz`, pinned by `edit_stake_rows_match_the_padded_total`. The campaign's duplicate fix was resolved in upstream's favour; its `str_from_ctaz` test (`63b20389`) is kept |
| G2 | [No clipboard backend on Windows or Wayland](crosslink-d00b6a44ef594-gui-review/high/clipboard-has-no-windows-or-wayland-backend-so-paste-only-flows-cannot-complete.md) | high | Windows; Linux without `xclip` or `xsel` | launch | FIXED `2d587956` | hard | gui+softer_gui | `2d587956` and `8999aaf1` moved the Windows and macOS clipboard into `softer_gui`. G3 landed after it, in `16103262`. The Wayland backend and the visible failure notice (G2.2, G2.3) were not checked |
| G3 | [Non-ASCII paste aborts the node](crosslink-d00b6a44ef594-gui-review/medium/pasted-non-ascii-text-panics-the-gui-on-a-byte-index-slice.md) | medium | macOS, Linux | launch | FIXED `16103262` | cargo | gui | `abbreviate_ends` cuts on character boundaries in all three modals, and `pasted_address` trims, drops control characters and caps at 512 characters. Unit tested; no manual CJK paste was done |
| G6 | [Send never validates the address](crosslink-d00b6a44ef594-gui-review/medium/send-modal-never-validates-the-address-and-a-failed-send-is-silent.md) | medium | all | any | FIXED `9cd05f56` | cargo | gui+wallet | `wallet::decode_send_address` is the one rule, shared with the RPC send, with a reason for each wrong kind. Send enables its amount buttons only on a decoded address, shows the reason, closes once the send is queued and reports a failed build on the wallet tab. The RPC scenario (G6.6) is not done |
| G7 | [Finalizer Filters zeroes the seconds filter](crosslink-d00b6a44ef594-gui-review/medium/opening-finalizer-filters-zeroes-the-seconds-filter-and-marks-everyone-online.md) | medium | all | any | FIXED `98571241` | cargo | gui | The seconds textbox is seeded from the stored filter before `ui.textbox` runs, and the dead seed is gone. G7.3 is still the owner's call |
| G8 | ["claimed @" label uses the wrong bond](crosslink-d00b6a44ef594-gui-review/medium/claimed-at-label-in-transaction-history-uses-the-wrong-bonds-claim.md) | medium | all | any | FIXED `8fae6ead` | cargo | gui | `claim_for_bond` matches on `bond_key` over the whole list; Convert rows take the roster lookup, and an Unstake row whose bond is still withdrawable shows its current value |
| G4 | [Modal backdrop passes clicks through](crosslink-d00b6a44ef594-gui-review/low/modal-backdrop-passes-clicks-through-to-the-controls-underneath.md) | low | all | any | CLEAR | unit | gui | Worst single outcome: an unrequested MINING toggle. Only the Jump modal and the block inspector are affected; the other seven modals capture |
| G5 | [Faucet tab resets every modal](crosslink-d00b6a44ef594-gui-review/low/faucet-wallet-tab-resets-every-modal-so-jump-and-convert-rewards-never-open.md) | low | all | any | CLEAR | unit | gui | The reset is deliberate for wallet modals. Jump and Convert Rewards were added later and were caught by it |
| G9 | [Chain view reacts under panes and modals](crosslink-d00b6a44ef594-gui-review/low/chain-view-pans-and-recenters-while-the-pointer-is-over-a-pane-or-modal.md) | low | all | any | CLEAR | none | gui | Fires most often of the three input findings: every touchpad scroll over a pane. Three one-line gates |
| G10 | [Esc dead after textbox focus; Jump gives no feedback](crosslink-d00b6a44ef594-gui-review/low/esc-cannot-close-a-modal-after-textbox-focus-and-jump-gives-no-feedback.md) | low | all | any | CLEAR | unit | gui | After one Tab, Esc is dead in all eight modals. Half of a two-stage Esc: the clearing handler is commented out |

## Sync follow-up (2)

Raised by the "crosslink spreadsheet triage" session while reading `e56e35f8`, then validated by tracing the code. The Fires column uses the consensus values.

| # | Finding | Sev | Fires | Gate | Status | Test | Owner | Notes |
|-|-|-|-|-|-|-|-|-|
| S1 | [Rejected-block backoff is keyed by hash alone](crosslink-e56e35f85a98f-sync-review/low/rejected-block-backoff-is-keyed-by-hash-alone-so-one-peer-can-delay-an-honest-block-for-every-peer.md) | low | adversary | any | CLEAR | intg | node | Partially confirmed. Each forged copy a peer gets queued under an honest hash stops new requests for that hash to every peer for 1 s doubling to 64 s. One dishonest connection among honest peers normally loses to the honest copy; sustained delay needs most of the node's connections. A regression against `e56e35f8^`, where a forgery cost about one 500 ms pass. The trade-off was weighed in the commit's comments; the existing forged-body test covers one strike only |
| S2 | [Double-spend block answered already known](crosslink-890036389ac3b-ingest-review/low/a-double-spend-block-is-answered-already-known-because-commit-errors-are-classified-by-their-text.md) | low | adversary | launch | FIXED `1be28cdd` | test-format | node | Commit errors were classified by their text, and the only errors named Duplicate are double spends. Fixed by asking the state whether it has the block. Covered by `crosslink_mempool_verdict_goes_stale_when_a_block_spends_the_input`; a shielded double spend is not covered, the generator cannot build shielded transactions |

## Tenderlink certificate review (4)

Raised by an adversarial review subagent on 2026-10-12, against the change that became `5a6577e2` ("Tenderlink: take a value that has 2f+1 precommits, whatever proposal we hold"), in two passes. Each was confirmed by running a scenario against the engine built from the working tree and from the commit below it; nothing was run on a network, so statements about packet timing in the reports are inferences. K1 and K2 were fixed before the commit. The Fires column uses the consensus values. The Test column: `cargo` = covered by a test in `tenderlink/src/lib.rs`.

| # | Finding | Sev | Fires | Gate | Status | Test | Owner | Notes |
|-|-|-|-|-|-|-|-|-|
| K1 | [Precommits before the proposal are never checked for a certificate](tenderlink-6705b3ca5e234-certificate-review/medium/precommits-stored-before-the-proposal-are-never-checked-for-a-certificate-so-a-node-handed-another-value-afterwards-never-decides-the-height.md) | medium | adversary | launch | FIXED `5a6577e2` | cargo | tenderlink | Fixed by `adopt_any_certified` when the first chunk makes the proposal known. The reviewer's scenario and 800-seed randomized check ran clean on the fix. Covered by `line49_precommits_that_came_before_any_proposal_still_decide` |
| K2 | [Conflicting chunk discards a verified certified proposal](tenderlink-6705b3ca5e234-certificate-review/medium/a-conflicting-chunk-discards-a-complete-hash-verified-certified-proposal-so-a-replayed-packet-delays-a-decision-head-would-make.md) | medium | adversary | launch | FIXED `5a6577e2` | cargo | tenderlink | Existed only in the first version of the change, never in a commit. A complete, hash-checked certified proposal is now kept. Covered by `line49_a_conflicting_chunk_does_not_cost_a_node_the_decided_value_it_has`. A partial assembly can still be restarted by a conflicting chunk (K2.2, open) |
| K4 | [Faulty flag on correct bytes before the certificate is never cleared](tenderlink-6705b3ca5e234-certificate-review/medium/a-faulty-flag-set-on-correct-bytes-before-the-certificate-is-never-cleared-because-the-real-chunks-return-at-the-repeat-check-so-the-node-never-decides.md) | medium | adversary | launch | FIXED `2792e9c2` | cargo | tenderlink | Each tick, a whole, certified proposal flagged faulty is settled: kept if its bytes hash to the id, fetched again if not. Covered by `line49_wrong_bytes_before_the_certificate_do_not_condemn_correct_bytes` |
| K3 | [Third value vote hides a line 28 certificate](tenderlink-6705b3ca5e234-certificate-review/low/a-third-value-vote-replaces-the-second-in-the-other-slot-so-a-triple-signer-can-hide-a-line-28-prevote-certificate.md) | low | adversary | any | CLEAR | unit | tenderlink | Needs a finalizer signing three prevotes in a round; intermittent. Before `6705b3ca` the second value was refused outright, which was worse |

## Tenderlink second review (2)

Raised by a second, independent review subagent on 2026-10-12, validated at `dev` @ `5a6577e2726e5`. Both findings are in code that predates the two tenderlink commits. This review was read only: nothing was built or run for it, and it was cut short, so its README says which of its questions went unanswered. A later run of a four-engine network with real timers, not written up as a report, found no finding of its own to add here. The Fires column uses the consensus values.

| # | Finding | Sev | Fires | Gate | Status | Test | Owner | Notes |
|-|-|-|-|-|-|-|-|-|
| K5 | [Vote packet indexes past the 18-slot array](tenderlink-5a6577e2726e5-review/high/a-crafted-vote-packet-indexes-past-the-18-slot-array-so-any-connected-peer-aborts-a-finalizer-node.md) | high | adversary | any | FIXED `3813c932`, `fdd8ce68` | unit | tenderlink | Any connected peer aborts the release node with one vote packet whose vote count exceeds 18; predates the two commits, not a regression. Confirmed by a standalone repro in both profiles. Closing it found a second attack in the same code, fixed in `fdd8ce68`: see K5.2 |
| K6 | [2f+1 quorum below two thirds when stake is not 3f+1](tenderlink-5a6577e2726e5-review/high/the-2f-plus-1-threshold-is-below-two-thirds-when-total-stake-is-not-3f-plus-1-so-f-equivocators-can-certify-two-values.md) | high | adversary | behind FeaturenetUpgrade2, unscheduled | FIXED `07bb64b5` | test-format | tenderlink | From FeaturenetUpgrade2, chosen per BFT height by the roster snapshot's height, the quorum is `floor(2n/3)+1` everywhere the engine counted `2f+1`, and the FeaturenetUpgrade2 branch ID is hashed into the vote namespace so old and new finalizers stop counting each other. Covered by tenderlink unit and engine tests and twelve-seat scenarios on both sides of the upgrade (`eea988f6`) |

## Fix campaign leads (4)

Raised by the agents of the October 2026 fix campaign while they worked, and recorded at integration. None has a report file or was validated by a separate review.

| # | Lead | Area | Status | Notes |
|-|-|-|-|-|
| N1 | A truncated replay can re-decide a height whose old certificate exists elsewhere | node | OPEN | Since C1's fix, restore stops below the first decision whose snapshot it lacks and decides that height again. If no peer serves the snapshot, the height could be decided again with a different value while the old certificate exists elsewhere. Not validated |
| N2 | The forged-tail scenario needs a configured-testnet harness | tests | OPEN | C2's end-to-end case needs a new genesis, difficulty-aware block generation and the regtest special cases removed; regtest pins every block at PoWLimit. Blocks C2.4 |
| N3 | The null-pointer commitment rule is ungated | node | OPEN | `0bc75e55` requires the null fat pointer on blocks whose commitment cannot bind one (genesis, the Heartwood activation block, pre-NU5 heights) at every height, not only from FeaturenetUpgrade2 |
| N4 | The template fallback can still copy a pre-activation parent's garbage pointer | node | OPEN | `getblocktemplate`'s fallback copies the parent's pointer; a parent below FeaturenetUpgrade2 may carry one whose certificate was never checked, which a post-activation block would then cite. Not validated; near the fallbacks C8.2 replaces |

## Test-format review (15 leads)

Raised by `/code-review xhigh` on 2026-10-01 against the binary test format and its scenario tests. These rows have no report file, so the Notes column carries the whole lead. `F` rows are about the format and harness in `zebra-crosslink/zebra-crosslink/src/test_format.rs`; `T` rows are about the scenarios in `zebra-crosslink/zebrad/tests/crosslink.rs`.

**Checked by**: `yes` = the session that owns the code confirmed it against `dev` @ `8ea4db1f` · `part` = confirmed with a part it did not verify, named in Notes.

**Owner**: `format` = the "Crosslink binary test format" session · `tests` = the "crosslink spreadsheet triage" session · `node` = the fix is in `zebra-state` · `review` = the "crosslink code review" session, which maintains this file.

| # | Lead | Where | Checked by | Status | Owner | Notes |
|-|-|-|-|-|-|-|
| F1 | `SHOULD_FAIL` passes when the harness gets no answer | `test_format.rs:1263` | yes | **FIXED** `5caf2d14` | format | `e56e35f8` made an unknown-parent block answer `Pending` at once. `5caf2d14` makes `LOAD_POW`, `MINE_FROM_TEMPLATE` and `RECV_POW` check the `IngestOutcome` itself: `SHOULD_FAIL` passes only on `Failed`, the new `SHOULD_DEFER` only on `Pending`, no flag only on `Committed`/`Known`, and no verdict (timeout, dead queue, unparseable `RECV_POW`) never passes. `LOAD_POS` gets a timeout and the same split. The 14 fork-refusal descendant loads (T1) now say `SHOULD_DEFER`; their reasons are still to add. Suite 37/37 |
| T1 | 14 fork-refusal assertions pass by timeout, not by a refusal | `crosslink.rs:957`, `:1069`, `:2386` | yes | **PARTIAL** `e56e35f8` | tests | Read, not re-run: the 13 orphan blocks and the closing one now get an immediate `Pending`, so about 420 s of waiting goes away. The assertions are unchanged. They still pass because the parent is unknown, and say nothing about the fork-choice floor |
| T2 | First generated PoS block is always invalid; two tests are vacuous | `crosslink.rs:260`, `:361`, `:481` | yes | CLEAR | tests | `regen_test_data` builds it over the two sibling blocks at height 3. Six other PoS files are unused and unloadable |
| F2 | `EXPECT_REJECTION_REASON` cannot say which rule rejected | `test_format.rs:938` | yes | CLEAR | review | Assigned to the review session on 2026-10-01. **On hold: the user's instruction is not to touch real consensus code at the moment**, and the fix is in `validate` and `admit_fat_pointer`. Not started. Every failing exit of `validate` returns `(Fail, None)`. Five PoS tests record identical text. Five `admit_fat_pointer` rules still share one message. `e56e35f8` splits the PoW reason into `header` and `body`, which does not reach these |
| F9 | `LOAD_POS` cannot express a certificate test | `test_format.rs:936` | part | DECIDE | node | A bad signature panics in `decide()` at `bft.rs:1079` instead of returning a rejection; zero signatures are accepted. The format owner did not verify the roster and quorum half, and reads the panic as a node robustness issue. Rides on the C7 decision and on C6.3 |
| F3 | `EXPECT_MEMPOOL_REJECTED` is true for a mined transaction | `test_format.rs:1470` | yes | **FIXED** `41e925b2` | format | The mempool checks ask the state whether the transaction is on the best chain: mined is not rejected, and each check reports resident, rejected and mined. All five uncalled builders now have a caller in `crosslink_mempool_and_staking_state_instructions`, including a mined spend asserted not rejected |
| F4 | `EXPECT_POW_BLOCK_FINALITY` has no timeout, and `None` is ambiguous | `test_format.rs:1143` | part | **FIXED** `41e925b2` | format | Both reads time out, and a failed or missing answer is no verdict, which never passes; the state always answers with a finality, so an expected `None` never matches (no scenario expects `None`). Still not verified: that an unknown block answers `CantBeFinalized` |
| F5 | The instruction dump panics on odd blocks and can deadlock | `test_format.rs:185` | yes | **FIXED** `5e48837e` | format | `ab5dc8b4` fixed the hang: the dump and the panic hook try each test lock for 500 ms and report a held one instead of blocking on it. `5e48837e` fixed the panics: a PoW block with no coinbase height and a PoS block with no headers are described, not unwrapped. Covered by `test_format_survives_malformed_files_and_odd_blocks` |
| F6 | `read_from_bytes` panics on malformed files and checks no magic | `test_format.rs:568` | yes | **FIXED** `5e48837e` | format | `read_from_bytes` checks the magic, the instruction size, the instruction offset and every instruction's data range; the 32-byte fields, an unknown finality code, an unknown kind and an oversized peer index no longer panic. Seven malformed-file cases are tested. The magic is not yet versioned (F7) |
| F7 | Files are not self-describing: calendar omitted, payload unversioned | `test_format.rs:643` | yes | **FIXED** `c1a0dce8` | format | Files are written as `ZECCLTF1`, whose `SET_PARAMS` always carries the calendar; `ZECCLTF0` files are still read under the old rule (calendar absent = the current prototype). The three diagram scenes are regenerated as version 1. The other three stored `.zeccltf` files stay version 0 |
| F8 | Conformance mode can no longer finish | `test_format.rs:666` | part | CLEAR | format + tests | Same lead as T4. About 20,740 blocks at about 92 ms each against a 360 s limit. The block arithmetic was not re-derived by the owner. Needs a batched block load |
| F10 | Missing instructions | `test_format.rs:77` | yes | **PARTIAL** `f0d42b34` | format | `c1a0dce8`: `SET_PARAMS` carries a zebrad.toml fragment, so a file can set the `[crosslink]` hardfork schedule, regtest activation heights and the other `[network]` parameters, and `[state] network_checkpoint`; no scenario uses it yet, and BlockGen still assumes NU6.3 from height 1. `f0d42b34`: `EXPECT_POW_TIP` and `EXPECT_FINALIZED_TIP` by hash; `MINE_FROM_TEMPLATE` remembers its block in a slot those two accept; also `CHECK_POW`, `CHECK_POS` and `SET_TIME` (immediate verdicts and a test-supplied clock). Still open: restart; an eventual expectation; slots for the other hash-taking instructions; one peer identity across `RECV_TX` and STP |
| T3 | The scene test rewrites three tracked files on every run | `crosslink.rs:2423` | yes | CLEAR | tests | Another test binary reads them at `viz2.rs:1072`, `:1108`, `:1130`. Compare against the checked-in bytes instead |
| T5 | The bond lifecycle test never asserts bond state | `crosslink.rs:2572` | yes | CLEAR | tests | Only block acceptance and chain length. The one `EXPECT_BOND` ignores the amount |
| T6 | Enforced consensus rules with no scenario | `crosslink.rs:2460` | yes | CLEAR | tests | Twelve named cases, from a null pointer after a non-null parent to the node's own template moving `fin`. Several wait on F9 and F10. The owner will take the ones today's format can express, plus C10's ghost roster |

Status of these 15: **6 FIXED**, **2 PARTIAL**, **1 DECIDE**, **6 CLEAR**. Line numbers are from the review at `8ea4db1f`; `e56e35f8` added 38 lines to `crosslink.rs` at `:1653`, so every later citation there moves down by that much.

---

## Roll-up

The tables in this section count the 29 reported findings. The test-format leads are counted above.

| Status | High | Medium | Low | Total |
|-|-|-|-|-|
| FIXED | 7 | 10 | 2 | **19** |
| PARTIAL | 0 | 0 | 0 | 0 |
| CLEAR (obvious fix, pending) | 0 | 0 | 9 | **9** |
| DECIDE | 0 | 0 | 0 | 0 |
| MEASURE | 0 | 0 | 1 | 1 |
| total | 7 | 10 | 12 | **29** |

By trigger, consensus rows: **6 honest**, **4 adversary**, **1 config**. All 10 GUI rows fire in ordinary use. The sync rows need a dishonest peer (S1) or any block that double-spends (S2). The 6 tenderlink rows need a dishonest proposer, finalizer or peer.

By gate: **4 behind FeaturenetUpgrade2, unscheduled** (C2, C7, C11, K6), **12 launch** (C1, C3, C4, C6, C10, G1, G2, G3, K1, K2, K4, S2), **13 any**.

By test route, what CI would actually catch on a bad push:

| Route | Findings | Runs in CI? |
|-|-|-|
| `test-format` (C3, C6, C7, C11, K6, S2) | 6 | not confirmed: the suite is not known to run in CI |
| `dilated` | 0 | n/a |
| `cargo` (C2, G1, G3, G6, G7, G8, K1, K2, K4, K5) | 10 | not confirmed: `cargo test` in `tenderlink/`, `wallet/` and `zebra-gui/` is not known to run in CI. K5's row says `unit` as upstream wrote it; its tests exist |
| **covered subtotal** | **16** | |
| `unit` (easy, unwritten) | 6 | no |
| `intg` (easy, unwritten) | 4 | no |
| `hard` | 2 | no |
| `none` | 1 | never |

**16 of 29 are covered by a test, and none by something confirmed to run automatically.** Three fixed rows have no test: C1 (`intg`), C4 and G2 (`hard`). 10 of the uncovered rows have a cheap route.

## What the table makes obvious

- **Every High and Medium finding is fixed.** What remains is nine Low rows and C9's measurement.
- **Four fixes change consensus rules and are switched off until FeaturenetUpgrade2 is scheduled** (C2, C7, C11, K6). Until a release sets its height, the network runs the old rules, and the fixes are tested only in regtest on both sides of a test-chosen activation height.
- **No decision is open.** The fix plan took the C11 and K6 choices (digest in `hashBlockCommitments` only; the strict quorum with the namespace flip), and `41f3517d` settled C6 by removing the hold. The owner has not confirmed the first two.
- **Three fixed rows have no test that has passed**: C1 and C4 need a restart with deleted state, and G2 a manual clipboard check.
- **The campaign found four new leads** (N1 to N4), two of them in its own fixes' edges.
- **Nothing is confirmed to run automatically.** Every test above runs only when someone runs it.

---

# Recommendation-level evaluation

The table above tracks the 29 **findings**. This one tracks the 98 individual **recommendations** inside them. A finding moves to PARTIAL when some of these land and others do not, and the ones that do not are where the remaining work is.

Numbering follows each report's own Recommendations section, so `C6.3` is item 3 in the C6 report. Items that only list rejected alternatives or rollout timing are left out; rollout is the Gate column above. `T` marks a report's unnumbered test list.

**Verdicts.** `DONE` (implemented, commit named) · `PARTIAL` · `OPEN` (not done, still applies) · `SUPERSEDED` (the facts changed under it) · `DECLINED` (considered and not doing, reason given).

**A deliberate bias:** anything not verified against the tree is marked `OPEN`, not assumed done.

| Verdict | Count |
|-|-|
| DONE | 45 |
| PARTIAL | 9 |
| SUPERSEDED / DECLINED | 2 |
| **OPEN** | **42** |
| total | **98** |

## Consensus findings

| # | Recommendation | Verdict | Note |
|-|-|-|-|
| C1.1 | Truncate the replay at the first stored decision whose snapshot is not held; delete the exit | DONE `ca09bbdb` | The exit is deleted |
| C1.2 | Correct the five stale comments about write ordering | DONE `ca09bbdb` | |
| C1.3 | Optional: flush the snapshot to the backup at decision time | OPEN | `8cc3f5b7` gave the writer a `backup_task` handle, which removes the report's stated obstacle. It is used only at shutdown. Not taken by the campaign |
| C1.T | Tests: snapshot lost with the backup; backup disabled; two-node catch-up after truncation | OPEN | `ca09bbdb` adds `LOSE_BACKUP=<n>` to the dilated run. Its one run on the branch timed out at the wallet stage, before the restart |
| C2.1 | Contextual difficulty check on every carried header in `validate`; `propose` uses the same helper | DONE `767abedd` | From FeaturenetUpgrade2 at the snapshot's height; `propose` uses the same check |
| C2.2 | Make the minimum-difficulty start height a `testnet::Parameters` field and turn it off for the new testnet | DONE `d6d33d3a` | `minimum_difficulty_start_height`, never on the featurenet parameters; the exemption also ends wherever FeaturenetUpgrade2 is active, so the two switch together |
| C2.4 | Tests: forged tail rejected, side-chain variant, honest tail passes, minimum-difficulty case, helper unit test | PARTIAL `767abedd`, `d6d33d3a` | Unit tests of the context walk across a side chain, the pass and fail decision, and a minimum-difficulty carried header. The forged-tail and side-chain scenarios need a configured-testnet harness (lead N2) |
| C3.1 | Entries expire unless a live proposal refreshes them, plus a hard cap | DONE `89d1d757` | 30 s expiry unless asked again, cap 16 evicting the least recently asked; decided-chain blocks in their own set |
| C3.2 | Run the header PoW check before the snapshot lookup in `validate` | DONE `89d1d757` | |
| C3.3 | Cap by-hash requests at a fixed share of the download budget | DONE `89d1d757` | A quarter of the download budget |
| C3.5 | Tests: bogus snapshots do not stall sync; a real missing snapshot is still fetched; expiry and cap unit test | DONE `89d1d757` | `missing_pow_blocks_expire_unless_asked_for_again_and_stay_under_the_cap`, `crosslink_bogus_bft_snapshots_do_not_stall_pow_sync`, `crosslink_missing_bft_snapshot_is_fetched_by_hash` |
| C4.1 | Remove the empty-store exit in `restore`; log and let the first tick re-derive genesis | DONE `d458bf22` | |
| C4.2 | Rewrite the guard comment and `restore`'s doc comment | DONE `d458bf22` | |
| C4.3 | Keep the failed genesis write at `error`, and say the height is re-decided on next start | DONE `d458bf22` | |
| C4.T | Tests: empty store above activation; row 0 missing; one tick crosses activation | OPEN | Needs a row-deletion tool. Same tool as C10.4 |
| C5.1 | `bootstrap_is_valid` bounds `h1 + sigma` and rejects sigma of 0 | OPEN | The function was edited in `ca131a7d` for `h0` and still ignores sigma |
| C5.2 | `build_bootstrap_genesis` reads headers from the finalized database and logs a miss | OPEN | Also removes two of C10.3's casts |
| C5.4 | Tests: config-load rejection, boundary scenario, unit boundary cases | OPEN | The report's example heights now also need a valid `bootstrap_staking_height` |
| C6.1 | Persist the abandonment as a hazard row before the first commit past the fork | DONE `2edd11b1` | The row records a drop (the hold no longer exists, `41f3517d`), written before the depth commit; database format 31.4.0 |
| C6.2 | Add a detached state to `BftChain`: `validate` returns `Indeterminate`, `propose` declines | SUPERSEDED | `41f3517d`'s drop is this state: once BFT is dropped `validate` answers `Indeterminate` and `propose` declines |
| C6.3 | Replace `decide`'s `assert_eq!` with a match that parks an `Indeterminate` decision | DONE `2edd11b1` | An indeterminate answer parks the decision, retried every tick; only a `Fail` panics |
| C6.4 | Keep the tenderlink handle and abort it on detach; the push closure parks, never panics | DONE `2edd11b1` | The push closure waits instead of panicking. The tenderlink task is aborted on a drop since `41f3517d` |
| C6.5 | `restore` starts detached when the hazard row explains the missing snapshot | DONE `2edd11b1` | Restore starts dropped whenever the row exists |
| C6.6 | Log the hold at `warn!`, repeat an `error!` while detached, add two gauges | SUPERSEDED | The hold is gone (`41f3517d`), and its drop logs at `error!` and repeats. `2edd11b1` adds one gauge, `state.crosslink.bft_dropped` |
| C6.T | Tests: test-format give-up scenario; decide-race integration test; dilated restart after give-up | PARTIAL `2edd11b1` | `crosslink_dropping_bft_at_the_reorg_depth_records_it_and_keeps_pow_running`, and `crosslink_bft_decision_whose_snapshot_was_dropped_is_parked_not_aborted`, the decide race through the node's own engine. The dilated restart after a drop is not done |
| C7.1 | Record the voting roster per decided height; `certificate_is_valid` in `admit_fat_pointer` and on `previous_block_fat_ptr` | DONE `d66bad21` | From FeaturenetUpgrade2. Also checks a force-fed decision; the template skips a non-tip pointer that would fail |
| C7.3 | Tests: malformed-certificate matrix; migrate the scenes that pass `sigs = &[]` | PARTIAL `d66bad21`, `eea988f6` | The matrix as unit tests and as a twelve-seat scenario on both sides of the upgrade. The scenes that pass `sigs = &[]` were not migrated; they run with FeaturenetUpgrade2 unscheduled |
| C7.4 | File the Equihash grinding issue separately | **DONE** | It is report C11 |
| C8.1 | The request carries the template parent's hash; the height is derived from it | OPEN | |
| C8.2 | Both `getblocktemplate` sites send `chain_info.tip_hash`; return an RPC error instead of a null pointer | OPEN | Same null fallback as C9.1 and C10.3 |
| C8.4 | Tests: a template-on-named-parent instruction and a deep-reorg scenario | OPEN | |
| C9.1 | Bound the loop at the parent pointer's index; errors, not null defaults | OPEN | A correctness guard, not a speedup. Lands with C8 |
| C9.2 | One `NonFinalizedState` snapshot per selection | OPEN | |
| C9.3 | Add a tracing span with the iteration count; shorten the lock hold only if it matters | OPEN | This is the measurement the MEASURE status waits on |
| C9.5 | Tests: 64 extra decisions then three templates; null-parent case | OPEN | |
| C10.1 | `snapshot_stakes` helper that panics on a missing record for a held block; reword the warning | OPEN | The `restore` half lands with C1.1 and C6.5 |
| C10.2 | Publish the non-finalized state after the finalize loop, in both commit paths | OPEN | |
| C10.3 | Casts: `u64::from`, `usize` in `hash_to_height`, a sigma bound, `try_from`, clippy cast lints | OPEN | 39 casts still uncommented |
| C10.4 | Tests: zero-stake scenario; restart with a deleted stakes row; cast leaf unit test | OPEN | |
| C11.1 | Bind a pointer digest into the ZIP 244 terminator of `hashBlockCommitments`; add `fatpointerdigest` to the template | DONE `0bc75e55` | From FeaturenetUpgrade2 |
| C11.2 | Decide the carried-header path with C2 before implementing C11.1 | DONE `0bc75e55` | Taken as the fix plan proposed: the digest in `hashBlockCommitments` only, no header change; the carried-header grind is an accepted residual, bounded by C2 |
| C11.4 | Tests: tampered pointer, pre-activation hash stability, template round trip, digest unit test, regenerate vectors | PARTIAL `0bc75e55` | Tampered pointer on both sides of the upgrade, pre-upgrade hashes pinned, template round trip and digest unit tests done; six `get_block_template` snapshots and a vector were edited by hand. The grind test is still `hard`: the harness disables PoW. Two of these scenarios fail on `integrate` against C7's check (C11 row) |

## GUI findings

| # | Recommendation | Verdict | Note |
|-|-|-|-|
| G1.1 | Delete `format_stake_amount`; both rows call `str_from_ctaz` | DONE `9f7dddcc` | Upstream kept `format_stake_amount` as a wrapper over `str_from_ctaz`, so both rows go through the padded formatter |
| G1.2 | Unit test `str_from_ctaz` with a table of zero-padded cases | DONE `63b20389` | `str_from_ctaz_pads_the_fraction`; upstream's `edit_stake_rows_match_the_padded_total` pins the rows |
| G2.1 | In-process Win32 clipboard in the vendored `softer_gui` | DONE `2d587956` | Landed before G3.1, against the ordering rule below |
| G2.2 | Add `wl-paste` and `wl-copy` when `WAYLAND_DISPLAY` is set | OPEN | |
| G2.3 | Make failure visible: "Clipboard unavailable", "Copied", "Copy failed" | OPEN | Shares the notice line with G6.3 |
| G2.4 | Enable the address textboxes in Send and Stake; add one to Retarget | OPEN | First ask why `c9231740` reverted them |
| G2.6 | Tests: Windows round trip in `softer_gui`; program-table unit test; manual matrix | OPEN | |
| G3.1 | One char-boundary-safe `abbreviate_ends` helper for all three modals | DONE `16103262` | |
| G3.2 | One `pasted_address` function: trim, drop control characters, cap the length | DONE `16103262` | Capped at 512 characters |
| G3.4 | Unit test for `abbreviate_ends`; manual CJK paste check | PARTIAL `16103262` | `abbreviate_ends_counts_characters_not_bytes`, `pasted_address_is_trimmed_printable_and_capped`. No manual CJK paste |
| G4.1 | `Floating::Root` captures; a new `RootPassthrough` variant for the tooltip only | OPEN | Same code as G9.3 |
| G4.2 | A `z` field on `Decl` so the Jump modal sits above the block inspector | OPEN | The stacking claim is inferred from Clay's root sort |
| G4.4 | Clay-only unit test through the real `decl()`; six manual steps | OPEN | |
| G5.1 | `Modal::acts_on_user_wallet()`; reset only when it is true | OPEN | |
| G5.3 | Unit test pinning the classification; manual check of three entry points | OPEN | |
| G6.1 | `wallet::decode_send_address` as the single rule, shared with the RPC path | DONE `9cd05f56` | The RPC send uses it |
| G6.2 | Cache the decode as Stake does and gate the amount buttons on it | DONE `9cd05f56` | |
| G6.3 | Show the reason under the address in all three modals | PARTIAL `9cd05f56` | Shown in the Send modal; Stake and Retarget were not changed |
| G6.4 | Report the outcome of a click: close on accept, show a send error, return the real result | DONE `9cd05f56` | |
| G6.6 | Tests: decode across address kinds; RPC scenario for a Sapling-only address | PARTIAL `9cd05f56` | `decode_send_address_accepts_only_testnet_unified_addresses_with_orchard`. The RPC scenario is not done |
| G7.1 | Seed the textbox from the stored filter before calling `ui.textbox`; delete the dead seed | DONE `98571241` | |
| G7.2 | Optional: show the active filter on the popup button | OPEN | |
| G7.3 | Owner's call: may a never-connected finalizer be ONLINE when both filters are off | OPEN | A product decision, not a bug fix |
| G7.5 | Unit tests: the seed survives; `finalizer_is_online_ex` for a never-connected status | PARTIAL `98571241` | `seconds_textbox_seed_survives_the_textbox`. The `finalizer_is_online_ex` test waits on G7.3 |
| G8.1 | Replace the positional search with `claim_for_bond`, matching on `bond_key()` | DONE `8fae6ead` | |
| G8.2 | Include `ConvertReward` in `sent_stake` | DONE `8fae6ead` | |
| G8.3 | Label unclaimed `BeginUnstake` rows from the unbonded roster | DONE `8fae6ead` | |
| G8.4 | Unit test `claim_for_bond` with a two-bond list | DONE `8fae6ead` | `claim_for_bond_matches_the_bond_key` |
| G9.1 | Gate the touchpad pan on `!ui.capture` | OPEN | |
| G9.2 | Clear `hovered_block` when `ui.capture` is true | OPEN | Removes the recenter, the highlight and the hover sound together |
| G9.3 | Fix `ui.rs:1744` to test `contents_hovered` | OPEN | Found during validation. Same code as G4.1 |
| G9.5 | Six manual steps | OPEN | No mechanical route |
| G10.1 | Esc always closes the modal; a live handler clears focus | OPEN | **Before G10.2** |
| G10.2 | Focus the Jump textbox on open | OPEN | |
| G10.3 | `parse_jump_height(text, tip)`; disable Jump and show a warning for bad or out-of-range input | OPEN | |
| G10.4 | Unit test for `parse_jump_height`; seven manual steps | OPEN | |

## Sync finding

| # | Recommendation | Verdict | Note |
|-|-|-|-|
| S1.1 | Key the backoff by hash and connection: pass the serving peer when recording a strike and the candidate peer when checking | OPEN | The honest peers stay eligible while the peer that served the failed copy waits |
| S1.2 | Run the merkle check at receipt, before the block takes the queue slot for its hash | OPEN | Stops a merkle-mismatched body from shadowing the honest copy. The code already has an `@Todo` for it. The shadowing predates `e56e35f8` |
| S1.4 | Tests: six forged `RECV_POW` from one peer, then the honest block from another; a parent-missing variant; a unit test of the two pure functions | OPEN | At `e56e35f85` the sixth strike sets a 32 s wait, longer than the harness's 30 s answer wait. Inferred, not run |
| S2.1 | Classify a commit error as a duplicate only when the state has the block | DONE `1be28cdd` | `known_outcome` instead of the error's text |
| S2.3 | Tests: a transparent double spend is refused with its reason; a shielded double spend | PARTIAL | Transparent covered in `1be28cdd`; shielded needs the generator to build shielded transactions |

## Tenderlink certificate findings

Recommendation numbers are the ones each report uses. Alternatives and rollout items that need no action are not listed.

| # | Recommendation | Verdict | Note |
|-|-|-|-|
| K1.1 | Run the adoption check when the first chunk of a round makes the proposal known | DONE `5a6577e2` | `adopt_any_certified`; the reviewer's scenario and randomized check ran clean |
| K1.3 | Add a test: three precommits, then the other value's chunks, then the decided value's chunks; assert the height advances | DONE `5a6577e2` | `line49_precommits_that_came_before_any_proposal_still_decide` |
| K2.1 | Refuse further chunks once a certified proposal is complete and its hash has matched | DONE `5a6577e2` | Scenario re-run clean |
| K2.2 | Keep a partial certified assembly from being reset by one chunk | OPEN | Needs a design choice; taking the value from one peer at a time would bound it |
| K2.4 | Extend the wrong-bytes test with a conflicting chunk after completion | DONE `5a6577e2` | `line49_a_conflicting_chunk_does_not_cost_a_node_the_decided_value_it_has` |
| K4.1 | When a proposal is certified, complete and flagged faulty, recompute the hash: clear the flag if it matches, restart if not | DONE `2792e9c2` | `settle_faulty_if_certified`, each tick before line 49 |
| K4.3 | Add a test: real chunks, a conflicting chunk, three precommits, real chunks again; assert the height advances | DONE `2792e9c2` | `line49_wrong_bytes_before_the_certificate_do_not_condemn_correct_bytes` |
| K3.1 | Do not let a newer other-value prevote displace one that completes a 2f+1, or latch the certificate per round | OPEN | Not started |
| K3.3 | Add a `line28_*` test with one finalizer signing three prevotes | OPEN | The reviewer's scratch scenario exists outside the repo and only prints |
| K5.1 | Reject a vote packet whose `no_votes_n + yes_votes_n` exceeds the 18-slot array before the read loop, widening to `usize` there and in the receive loop | DONE `3813c932` | Widened in `read_from` only. The receive loop's `u8` sum is safe because `read_from` now caps it at 18 |
| K5.2 | Build the signing round by or-ing in the top bit (`PRECOMMIT_ROUND_BIT`), not by adding `0x8000_0000`, to remove the overflow-checks-only panic | DONE `fdd8ce68` | Worse than a debug panic. Nothing bounded a received round, and the release build wraps, so a prevote in round r signed the same bytes as a precommit in round 0x8000_0000 + r. Any peer could replay honest votes into such rounds, and the round-catchup rule would move every finalizer to a round whose timeouts last decades. Now rounds with that bit are refused on receipt, and a fat pointer must carry the bit to validate |
| K5.4 | Add a unit test: `read_from` with 10+10 and 255+255 vote counts returns `None` rather than panicking | DONE `fdd8ce68` | `vote_packet_with_more_votes_than_slots_is_refused`; the replay has `votes_in_a_round_with_the_precommit_bit_are_refused` |
| K6.1 | Replace `2f+1` with a strict `floor(2n/3)+1` quorum in `big_threshold`, `bft_update`, `has_prevote_certificate`, and `proposal_is_certified` | DONE `07bb64b5` | From FeaturenetUpgrade2, with the vote namespace flip |
| K6.2 | Correct or delete the `adopt_if_certified` single-value comment | DONE `07bb64b5` | |
| K6.4 | Add tests: `big_threshold(n) > 2n/3` for a range of `n`; a 5-finalizer engine test that at most one value is certified under one equivocator | DONE `07bb64b5` | `strict_quorum_is_the_least_stake_above_two_thirds_and_2f_plus_1_is_not`, `one_equivocator_of_five_cannot_certify_two_values_under_the_strict_quorum`, `engines_either_side_of_the_namespace_flip_do_not_count_each_others_votes`; scenarios in `eea988f6` |

---

# How the recommendations interact

The recommendations are not independent. Treating the 42 open ones as a flat backlog loses the pairs that must land together and the orderings that prevent a regression.

## Must land together (splitting them causes harm)

| Pair | Why | State |
|-|-|-|
| **C2.1 + C2.2** | The contextual check alone is bypassable. From height 299,188 the minimum-difficulty rule lets a proposer earn PoWLimit legitimately by spacing fake timestamps 150 s apart | Both DONE (`767abedd`, `d6d33d3a`); they switch together at FeaturenetUpgrade2 |
| **C1.1 + C4.1 + C6.5 + C10.1** | All four change what `restore()` does when state is missing. Done separately, each fix rewrites the branch the previous one added | C1.1, C4.1 and C6.5 DONE in the one restore branch; C10.1 OPEN |
| **C8.1 + C8.2 + C9.1** | The loop bound is only correct once the height and the parent agree, which C8 provides | All OPEN |
| **G2.3 + G6.2 + G6.3** | They share the notice line under the paste button. Clipboard failure takes priority over a validation error | G6.2 DONE, G6.3 PARTIAL (Send only), G2.3 OPEN |
| **G4.1 + G9.3** | Both edit the modal hover code in `ui_left_pane` | Both OPEN |

## Ordering rules

| Rule | Source | State |
|-|-|-|
| **G3.1 before G2.1** | A working Windows clipboard makes the G3 node abort reachable on Windows | Broken in order, now harmless: G2.1 landed in `2d587956` before G3.1 in `16103262`; both are in |
| **G10.1 before G10.2** | With focus on open and no Esc fix, Esc is dead from the first frame | Not yet at risk |
| **C11.2 before C11.1** | The digest definition depends on how carried headers prove their binding | Taken together in `0bc75e55` |
| **C11.1 and C7.1 before BFT activation** | Both change block validity after the first non-null pointer | Superseded: both are behind FeaturenetUpgrade2 instead |
| **C5.2 before or with C4.1** | Re-deriving genesis is only safe if genesis is a pure function of finalized data | **C4.1 DONE (`d458bf22`) without C5.2, which is OPEN.** The commit argues genesis is already a pure function of the headers above the roster height; not re-checked here |
| **C3.1 must keep a node's own stored decisions fetchable** | C1.1 relies on re-fetching the missing snapshot through this set | Met: decided-chain blocks have their own set that never expires (`89d1d757`) |
| **Choose the template pointer before computing default roots** | After C11.1 the template's commitments depend on the pointer, and C8 edits the same `getblocktemplate` path | Coordinate C8 with C11 |
| **G3 before G6.2 and G6.3** | Same lines; the validation sits on top of the safe abbreviation | Met: `16103262`, then `9cd05f56` |

## Anti-correlated / mutually exclusive

| Tension | Resolution |
|-|-|
| **C6: re-anchor BFT on the node's own branch** vs **halt BFT on this node** | **Re-anchoring DECLINED in the report.** It allows two conflicting final snapshots. Halting the whole process is also declined: one cheap reorg would then shut every node down |
| **C6: interim detached state** vs **stage 9's second chain state** | Settled by `41f3517d`, which removed stage 9 and the hold; the drop is the detached state, and `2edd11b1` records it |
| **C7: checked certificates** vs **hash-only pointer with empty signatures** | Checked certificates, from FeaturenetUpgrade2 (`d66bad21`) |
| **C11: exclude the pointer from the difficulty hash** | **DECLINED in the report.** It unbinds the pointer, so a relay could swap it for free |
| **C11: make the pointer canonical** | **DECLINED in the report.** Honest nodes hold different signature sets, and a roster key holder can re-sign |
| **C9: the original review's early stop as a speedup** | **SUPERSEDED.** The loop already stops there. The bound stays, as a correctness guard under C9.1 |
| **C3.2 alone** | Moving the PoW check earlier does not bound the set. It needs C3.1 |

## The same work, counted twice or more

| Work | Appears as |
|-|-|
| What `restore()` does when a snapshot or a row is missing | C1.1, C4.1, C6.5, C10.1 |
| Replace the null fat pointer fallbacks in `fat_pointer_for_template` with errors | C8.2, C9.1, C10.3 |
| Bound sigma in parameter validation | C5.1, C10.3 |
| Casts in `build_bootstrap_genesis` and `admit_fat_pointer` | C5.2, C7.1, C10.3 |
| `decide` must never abort the process on a lifecycle race | C6.3, C6.4, and the shutdown path under "Facts that moved" |
| A test tool that deletes database rows between stop and start | C4.T, C10.4 |
| A restart scenario in the dilated run | C1.T, C6.T |
| The notice line under the paste button | G2.3, G6.3 |
| Sanitising and abbreviating a pasted address | G3.1, G3.2, G6.2 |
| The Jump modal's hover and capture code | G4.1, G9.3 |

## Clusters: one decision or design unlocks many

| Cluster | Members | Blocked on |
|-|-|-|
| **Startup recovery** | C1.1 to C1.3, C4.1 to C4.3, C6.1, C6.2, C6.5, C10.1 | Done except C1.3 and C10.1 |
| **Carried-header binding** | C2.1, C2.2, C11.1, C11.2 | Done behind FeaturenetUpgrade2; the carried-header residual is accepted (C11 row) |
| **Header certificate** | C7.1, C7.3 | Done behind FeaturenetUpgrade2 |
| **Template pointer** | C8.1, C8.2, C9.1, C9.2 | Nothing. Coordinate with C11.1 on the template roots |
| **Paste-an-address flows** | G2.1 to G2.4, G3.1, G3.2, G6.1 to G6.4 | G2.1, G3.1, G3.2, G6.1, G6.2, G6.4 done; G2.2 to G2.4 and G6.3 left |
| **Pointer and keyboard routing** | G4.1, G4.2, G9.1 to G9.3, G10.1, G10.2 | Nothing |
| **Wrong figures** | G1.1, G7.1, G8.1, G8.2 | Done: `9f7dddcc`, `98571241`, `8fae6ead` |

## Facts that moved under the reports

The consensus reports were written at `e99404e3de7cc`. Eight commits later, at `d00b6a44ef594`, these statements in them are out of date. The reports themselves have not been edited.

| Report | What it says | What is true now |
|-|-|-|
| C1 | A graceful stop is covered by the scenario, because nothing flushes the backup at shutdown | `8cc3f5b7` flushes the backup on a clean stop. Crash, hard kill and backup-off are unchanged |
| C1, C4, C6 | Restart tests need a new child-process harness | The dilated run already restarts both nodes past activation |
| C4, C5 | Roster height 75, activation height 275, gap 200 | Roster height 34,560, activation 36,288, gap 1,728, since `ca131a7d` and `00565513` |
| C5 | Margin of 196 blocks | Margin of 1,724 blocks. Still unexposed, and still unexposed if the reorg limit goes to 999 |
| C7, C11 | Up to 100 signatures in a pointer | Up to 12, since `3351fc4c` |
| C11 | After C7's fix, quorum-subset freedom reaches about 88.8 bits at 100 members | About 10.6 bits at 12 members. A roster key holder can still re-sign without limit, so the conclusion stands |
| all `write.rs` and `new_network.rs` citations | Line numbers | `write.rs` moved down about 10 lines. `new_network.rs` moved about 46 lines at `d00b6a44`, and again at `e56e35f8`, which adds about 100 lines above the commit loop. `bft.rs` line numbers are still exact |

### At integration, 2026-10-26

| Fact | Effect |
|-|-|
| `41f3517d` removed the conflict hold and stage 9, before the campaign: a decision that needs a reorg past the reorg limit now drops BFT | C6's decision is gone; C6.2 and C6.6 are SUPERSEDED, and the campaign recorded the drop in the database instead of building a detached state |
| `148dd940` ends support on the featurenet two weeks after release height 93,545 | Unchanged by the campaign. FeaturenetUpgrade2 has no height and must be scheduled in a release that ships before that end |
| Upstream's STP 2.0 migration (2026-10-22) rewrote tenderlink's transport and `new_network.rs`'s network thread | The campaign branches were cut before it and merged onto it with no compile adaptation; the integration report has the suite results on both sides |
| Upstream fixed K5 (`3813c932`, `fdd8ce68`) and G1 (`9f7dddcc`) while the campaign fixed them too | Upstream's forms were kept; the campaign's duplicate hunks were dropped at the merge |

### Re-check at `e56e35f8`

One commit landed after the first assessment: `e56e35f8`, "Answer every queued block through a fate table; back off re-fetching rejected blocks". It touches `new_network.rs`, the submit path in `methods.rs`, the two download components, six lines of `test_format.rs`, and adds one scenario test.

| Row | Effect |
|-|-|
| C1, C4, C6, C10 | None. `bft.rs` and `write.rs` are untouched |
| C3 | None. The new backoff applies to a block a peer served that failed verification. C3's bogus hashes are never served, so they never enter the backoff table, and `MISSING_POW_BLOCKS` is unchanged |
| C6 | None on the re-download loop. A by-hash block dropped as "already finalized" goes through the new `drop_block!` path, which reports to a test peer and does not start a backoff |
| C8 | None. The `methods.rs` change is in `submitblock`, where a held block now answers `Inconclusive`. There is still no tip check |
| C2, C5, C7, C9, C11, all `G` rows | None. Their files are untouched |
| F1, T1 | PARTIAL, as described in the test-format section |

Two things in the commit are new behaviour and belong to no row. When a served block fails verification, the peer is now disconnected only if the header is invalid, not for a bad body; the hash is retried after a backoff of 1 to 64 seconds. And `settle_fate` aborts the node if a queued block has no fate entry; both paths that queue a block insert one, checked by reading.

One new finding came out of this commit: the backoff is keyed by hash alone. It was raised by the "crosslink spreadsheet triage" session, validated by tracing the code, and is tracked as row S1 above. The lead as first stated overstated the reach, and the report records the corrections: the backoff gates only new requests, the node asks two peers per hash, and an honest download already in flight is not cancelled. It has not been raised with the commit's author.

One item is new and belongs to no report yet. `sync()` can now return at shutdown, which drops the BFT request receiver. A decision arriving at that moment may reach the `panic!` at `bft.rs:1517` or the `expect` at `:1520`, and the build aborts on panic. **This is inferred from reading and was not traced end to end.** C6.4 would cover it.

## Not tracked here

Both reviews cut lower-priority candidates to stay within their limit of ten. None was validated, so none has a row.

- **Consensus:** tenderlink's replies are answered only between ticks on the sync thread, so a burst of blocks may cause round timeouts; `validate()` compares hardforks byte by byte although the type derives `Eq`; `bft.rs:338-339` fills `ConsensusCounts` with signature counts where tenderlink uses stake-weighted sums.
- **GUI:** the nine items listed at the end of `crosslink-d00b6a44ef594-gui-review/README.md`.
- **Unconfirmed deployment facts:** the PoWLimit the new testnet will actually deploy with, and whether the `staking3d` network has already started. C2 and C11 are priced against the in-tree default of `2^251 - 1`.
