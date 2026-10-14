# Crosslink ingest finding at dev 890036389ac3b

One report, in the template at [`../SECURITY-ISSUE-TEMPLATE.md`](../SECURITY-ISSUE-TEMPLATE.md). Its status is tracked as row S2 in [`../REVIEW_STATUS.md`](../REVIEW_STATUS.md).

- **Target:** the commit-error classification in `zebra-crosslink/zebra-state/src/new_network.rs` on `dev` at `890036389ac3b`.
- **Source:** the "CROSSLINK binary test format" session, 2026-10-13, from a scenario whose double-spend block was answered as already known.
- **Method:** the wrong verdict was observed by running that scenario; the cause was traced by reading.

| # | Severity | Verdict | Fires | File |
|-|-|-|-|-|
| S2 | Low | Confirmed | any block that double-spends | [a double-spend block is answered already known](low/a-double-spend-block-is-answered-already-known-because-commit-errors-are-classified-by-their-text.md) |

**Status:** fixed in `1be28cdd` ("NewNet: answer a double-spend block as refused, not already known"), which also changed the scenario to expect the refusal. A shielded double spend is not covered.
