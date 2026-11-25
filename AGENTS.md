# Repository Agent Instructions

## Crosslink book subtree

`crosslink_book/` is a bidirectional, squashed Git subtree of the
`crosslink_book-upstream` remote's `code` branch. The expected remote is:

```text
crosslink_book-upstream https://github.com/ShieldedLabs/crosslink_book.git
```

Git remotes are clone-local configuration. If the expected remote is missing,
create it before using the tools:

```text
git remote add crosslink_book-upstream https://github.com/ShieldedLabs/crosslink_book.git
```

The repository-root `subtree` script is adapted from
`ShieldedLabs/zcash-dev-suite` and is the shared primitive for subtree
operations. Invoke it through Bash:

```text
bash subtree <add|ls|pull|diff|push|rm> [arguments]
```

For routine two-way synchronization, use one of the repository-root scripts:

```text
bash sync-crosslink-book.sh [commit message]
sync-crosslink-book.bat ["commit message"]
```

The scripts perform the required sequence:

1. Synchronize `dev` with `origin/dev` using merge semantics.
2. Commit uncommitted changes beneath `crosslink_book/` only.
3. Pull `crosslink_book-upstream/code` into the subtree with `--squash`.
4. Push the extracted subtree history to `crosslink_book-upstream/code`.
5. Push the resulting monolith history to `origin/dev`.

Subtree pushes intentionally do not use `--rejoin` or create rejoin checkpoint
commits. A rejoin makes the extracted book commit a parent of a monolith
commit, which makes the entire `crosslink_book` history reachable from the
monolith branch. Native subtree pushes may rescan monolith history; that cost is
intentional to keep the repositories' commit histories disconnected.

Do not replace this sequence with an ordinary topology-flattening rebase while
subtree merge commits are local-only. If the monolith remote advances after a
subtree operation, merge it with `git pull --no-rebase`.

The synchronization scripts intentionally refuse tracked or staged changes
outside `crosslink_book/`. Commit or stash those changes separately first.

For individual operations, the canonical commands are:

```text
bash subtree pull crosslink_book code
bash subtree push crosslink_book code
```

## Commit titles

A commit that fixes a GitHub issue, even partly, has `Fix #N` in its title, so
reaching `main` closes the issue: `Wallet: Fix #47 by resending transactions a
reorg drops`. Never write `for #N` or `(#N)` in its place, and never drop `Fix`
to keep an issue open; say which parts stay open in your report instead. A port
of a fix already done on another branch needs `Fix #N` too.

A semicolon in a title joins two unrelated changes only. Two parts of one fix
join with "and".

## Chain directory names

`ZebradConfig::load_with_env` in `zebra-crosslink/zebrad/src/config.rs` appends
the chain's directory name, such as `crosslink_nightly_20261102`, to the state
cache directory. To start a new chain, change only the date in that name, to
the day the chain starts, as well as either `nightly` or `featurenet`. Never
append anything else to it: no suffix naming a parameter, a feature or a reason.

Changing the name starts a new chain, and the old chain's state stays behind
under the old name. The featurenet's `crosslink_featurenet_20260929_staking3d`
predates this rule; keep it as it is, since renaming it would strand the
featurenet's state.

## Units

cTAZ means Crosslink TAZ, the featurenet's coin: 1 cTAZ is 100,000,000 zats.
The "c" is never centi-, so 10 cTAZ is 1,000,000,000 zats.

## GUI work

Any change to the GUI (`zebra-gui/`: its screens, layout, drawing, input or
speed) is built, run and checked through `zebra-gui/gui.sh`. It builds the GUI
alone in about 20 seconds, runs it on mock data with no window, and answers in
text and small images. Never build zebrad to look at a GUI change, and never
write a new script to drive or screenshot the GUI. `zebra-gui/GUI_DEV.md` is
the manual: read it before the first GUI change of a session.

```text
bash zebra-gui/gui.sh snap base          Before the first edit: record every screen as it is.
bash zebra-gui/gui.sh check              After each edit: build, run every script and random
                                         input, list what changed on screen since base.
bash zebra-gui/gui.sh run '<commands>'   Try one thing, e.g. 'click "Send"; shot send; lint send'
```

- A GUI change is done only when `check` ends in `check: ok` and you have read
  every `diff_*.png` and every `lint new:` line it printed. Say in your report
  which of them you meant to cause.
- Look at the result yourself: read the PNG that `shot`, `lint` or `diff`
  names. Do not report a visual change you have not seen.
- Check what moves (an animation, a scroll, a delay) with `settle`, `film` and
  `track`; speed with `gui.sh bench`; a real window's frame timing with
  `gui.sh pace`.
- A new screen or behaviour gets lines in a script under `zebra-gui/scripts/`,
  so `check` covers it from then on.
- A crash or a hang prints the script line it happened on. A random-input
  crash leaves `target/gui/now/monkey/monkey.gui`, which replays it.
- The mock data is in `zebra-gui/src/main.rs`. If the screen you need cannot
  be reached from it, add to the mock data; do not start a node.
- Use no worktree for GUI work: `gui.sh snap <label>` keeps a before and an
  after side by side in one checkout.
- `gui.sh` takes 5 to 30 seconds. Start it in the background when you have
  reading to do meanwhile.
- The scripted run has no real window. What only a real window shows (the
  pointer's shape on screen, the title bar, the clipboard, how it feels) is
  unchecked unless a person looked: say so.
