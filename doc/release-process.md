# Releases

Phases: planning, building, ship-or-not decision, excution

- *Planning* is up front where we've triaged issues, looked at our roadmap, etc... and decided what a future release (typically "the next one") should contain.
- *Building* is the effort that goes into preparing the upcoming release (all the software dev, testing, docs, etc...
- *Ship-or-Not* is an important pivot point where we decide "do we start the release execution process now or not?" -and if so, we decide on a set of parameters for the release (more below).
- *Execution* is the literal steps to produce a release. Ideally we just have a big green button, and all the good stuff automatically happens, but in practice there are always a sequence of steps humans do. The goal of separating out the "Ship-or-Not" step first is so that these steps can be quick / easy / require little discretion/judgement.

## Ship-or-Not

We front-load as much "discretionary decision making" in the release process or not into a "Ship-or-Not" process which answers these questions:

- Are we ready to ship a release?
- Which commit & branch are we releasing?
- Have we completed all of the tracked issues/goals intended for the release? If not, make sure we synchronize on adjusting the release goals as a team, then come back to this Ship-or-Not decision.
- Are any known gaps or flaws tracked with existing tickets?
- Select all the important parameters for a release (the official version, activation heights, any version-specific constants, hardcoded URLs/IPs/pubkeys, etc...)
  - A crucial subset of this: is this release for a Network Upgrade? If so, this should have been decided very early in the release planning phase. A Mainnet-aspiring emergency upgrade should often be based on a prior stable release with surgical fixes for the emergency, rather than "the latest dev features that haven't shipped yet plus emergency fixes."

The "-or-Not" is important: we should always consider the option of "no, it's better to postpone and sync up as a team and decide on a new plan" maybe because of test failures, external event timing, any number of reasons...

## Release Execution Checklist

Ideally, the steps in this checklist require little subjective judgement. This way, different people can do releases consistently and without unexpected delays (unless some step fails). The more we can automate here the better.

A checklist for our releases:

1. Ensure all issues in the relevant issue tracking milestone for the release are closed, except a final "do the release" issue.
2. Ensure github CI is green for the target commit.  
3. Ensure there a no changes on `main` that are not on `dev`, such that a fastforward merge of `main` to `dev` will succeed.
3. If it is the first release that codifies a Network Upgrade, ensure all versioning constants are already correctly upgraded such as `magic2`, [FIXME: list all relevant ones]
4. Announce in the SL CL team channel that we're now releasing `v<N>`
5. Tag the target commit with the new version number `v<N>`
6. Update the Shielded Labs servers deploylink on the release
7. Fast-forward `main` to the tip of `dev`; this assumes there are no unmerged changes on `main`
8. Use Github to create a new release from `main` based on the tag `v<N>`
9. Build the new release binaries
10. Mark the release as public
11. Announce the release in the Crosslink Updates Signal group
