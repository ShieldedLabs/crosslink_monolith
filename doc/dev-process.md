# Dev Process

The [Shielded Labs](shieldedlabs.net) team which maintains this repo uses the following dev process / policies. All of this is likely to evolve.

## General Philosophy

The processes / policies are aimed at improving our overall team effectiveness, with some trade-offs. Nothing about the policies are like "iron-clad laws", but more like useful Shelling points we've setup to improve coordination.

Whenever it makes sense to deviate from this, we should, so long as we're clear / aware of the deviation. If it's a repeating pattern, maybe we should adjust this doc.

## Terminology

We use these terms, many of which replace more familiar terms in order to avoid unintended  associations, or collisions with tracking tool features, and to remain specific to this team.

**Dev Cycle:** the primary repeating rhythm of our work processes.

- *Replaces* "Sprint" or other items from particular methodologies because of unintended implications or tool collisions (e.g. "Zenhub Sprints").

### Issue Lists

- **Incoming List:** All new tickets/PRs/individual items that have not yet been *triaged*.
- **Product Backlog:** All items we decided at some point we intend to complete. This can be split up into categories, especially:
  - **Ready Queue:** Items we could begin work on as soon as we have bandwidth and have prioritized it.
  - **Pending Queues:** These are items we intend to begin work on after some future event, such as "the `vN+2` release, after we've shipped the `vN+1` release. When those future events unblock a Pending Queue, all items effectively migrate to the Ready Queue.
- **Dev Cycle Backlog:** All items we committed to completing in the current Dev Cycle.
- **Intra-Cycle States:** Various intra-cycle ticket states we might track to codify process (e.g. "implemented" vs "reviewed" vs "released" vs "deployed", etc...). We don't specify that level of detail in this doc.

### Goal Sets & Tracks

**Goal Set:** a group of goals we wish to accomplish together. Importantly these can be non-exclusive! For example, a ticket might simultaneously be in the `v9000 Release`, `Network v7 Reset`, and `2027 Q2` goal sets.
- *Replaces* "Milestone" because of multiple ambiguous meanings and tool collisions.

**Active Goal Set:** a Goal Set which we are actively focusing our effort on (in contrast to future / postponed / inactive Goal Sets).

**Goal Track:** A set of mutually exclusive Goal Sets of the same type, e.g. "Foo releases", "Network Resets", "Workshops", "Top-Level Roadmap Waypoints", etc...

A ticket may be in multiple goal sets from different tracks, but not within the same track.

For example, a ticket might be both the one "v42 Foo Release" goal set and the "Network Reset for V7" goal sets, but not _also_ in the "v41 Foo Release".

Typically only one Goal Set on a track is an Active Goal Set, for example "Implement and ship the `vN+1` release" would be active on a release track, while "Implement and ship the `vN+2` release" is inactive.

- *Replaces* multiple kinds of "Milestone"-like concepts, and also tools tend to have specialized widgets / concepts for different kinds of Goal Tracks, then also completely lack first-class support for other kinds of Goal Tracks we need.

## Dev Cycles

We use a two week dev cycle rhythm with three important team processes:

1. **Kickoff** - to begin a dev cycle, select ~1 dev-cycle-worth of prioritized product backlog goals to commit to for the cycle, for any of the currently active Goal Sets (e.g. "next release", "upcoming code audit", "next workshop", etc...).
2. **Triage** - Review the incoming fire-hose of new issues and decide how to handle them. Some useful decisions:
    a. it's urgent and cannot wait, and we should begin effort on it right away without waiting for a dev-cycle -> put it on the current sprint backlog and notify the whole team about the exception.
    b. it's irrelevant, or we do not believe it's a good use of our effort -> comment on why then close it.
    c. it may be nice-to-have, but we have no intention _currently_ to do it -> move to the "Ice Box".
    d. schedule it for a later goal set, e.g. "the `N+2` release instead of the `N+1` release we're focused on now" -> move to a Pending Queue for the appropriate Goal Set 
    e. schedule it as soon as we have bandwidth (after higher priority items) -> put it on the Ready Queue
3. **Wrap-up** - wrap up any loose ends, clean things up, have a breather before the next kickoff.
    a. Review and agree on which items are complete & can be closed / cleaned up.
    b. Decide how to handle incomplete items. Common options: continue in next cycle, postpone, split into smaller steps, replace with a different / better goal.
    c. Quick check-in: is there anything about the _process_ / _tracking_ that we could improve? Is it working well?

## Work Reviews

### Owner/Reviewer Policy

A core policy we use it owner/reviewer separation in our development process. For every ticket (issue or PR or any other unit of effort) we must have two separate people doing to different roles:

- **Owner:** Does the initial work to achieve the intended goal.
- **Reviewer:** Decides if that work achieves the intended goal well enough.

A ticket cannot be considered "complete" until the reviewer has signed off on it with some record of why they came to their decision.

For this "Core Policy", the above are the only requirements, and _how_ a reviewer decides an issue is complete can use whichever criteria and level of effort that makes sense in context.

### Other Policies

**Trunk-Dev Review Policy:** Currently we operate with a rapid-prototyping orientation and do trunk development, pushing code changes directly to `dev` branches. The "completion" criteria for reviewing code-related tickets thus relies on using the latest `dev` branch for review.

**Release Review Policy:** Each release must have an owner/reviewer, which we track with a "capstone ticket", which aims to be the last "catch-all" ticket to ensure a release succeeds. The reviewer is responsible for checking three things at minimum:

- The release is available to users and the [./release-process.md] checklist items appear complete.
- Release-scoped necessary infra updates & deployment are complete.
  - The term "release-scoped" is intended to capture a narrow scope just to ensure the release is usable, and larger scopes of infra maintenance or deployment might very well deserve a separate Goal Set outside of a package release.
- All of the tickets intended to be shipped in the release are indeed already reviewed and completed, _or else_ any incomplete items are properly tracked for future work.
