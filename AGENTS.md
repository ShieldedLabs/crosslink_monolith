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

Whoever adds a GUI feature is its designer, even when the request says only to
implement it. Read `zebra-gui/GUI_DESIGN.md` first and decide what the person
sees first, what their hand does, and what form each control's job calls for.
The screens as they stand are not the standard to copy. A new button in a new
row is the first idea and seldom the best: ask whether something already on
screen can do the job, and what the change pushes out of place. Add a
typeface, an icon, an image, an animation or a new kind of control when the
design calls for one, and write it into that file's vocabulary.

The report of a GUI change has a part headed `Design`: for each new thing on
screen, the form and place you chose, one you turned down, and why; then the
one image a person should look at.

```text
bash zebra-gui/gui.sh run 'shot before'  Before the first edit: look at the screen as it is.
                                         With no uncommitted GUI change, any gui.sh command
                                         also records every screen, to compare with later.
bash zebra-gui/gui.sh check              After each edit: build, run every script and random
                                         input, say what changed on screen.
bash zebra-gui/gui.sh run '<commands>'   Try one thing, e.g. 'click "Send"; shot send; lint send'
```

- A GUI change is done only when `check` ends in `check: ok` and you have read
  every image it names and every line of it that says `new`. It names one
  image for each way the screens changed, not one for each screenshot. Say in
  your report which changes you meant to cause.
- Look at the result yourself: read the PNG that `shot`, `lint` or `diff`
  names. Do not report a visual change you have not seen.
- Judge a design with `squint` (what the eye lands on first), `targets` (what
  can be pressed, how big, how it answers the pointer) and `style` (the sizes
  and colours in use).
- Check what moves (an animation, a scroll, a delay) with `settle`, `film` and
  `track`; speed with `gui.sh bench`; a real window's frame timing with
  `gui.sh pace`.
- Check how a screen takes a change of size with `resize` (the window) and
  `drag` (a pane's divider). Each moves a step at a time, as a hand does, and
  says from what size text is cut off or overlaps, what jumps, and what
  follows a frame late. `check` does this for the main screen; run both on a
  screen or a pane you add or change.
- A new screen or behaviour gets lines in a script under `zebra-gui/scripts/`,
  so `check` covers it from then on.
- A crash or a hang prints the script line it happened on. A random-input
  crash leaves `target/gui/now/monkey/monkey.gui`, which replays it.
- The mock data is in `zebra-gui/src/main.rs`. If the screen you need cannot
  be reached from it, add to the mock data; do not start a node.
- Use no worktree for GUI work. `check` compares with the last commit's screens,
  which `gui.sh` records by itself under `base`: never record `base` yourself
  after editing. `gui.sh snap <label>` keeps any other state to compare with.
- `gui.sh` takes 5 to 30 seconds. Start it in the background when you have
  reading to do meanwhile.
- The scripted run has no real window. What only a real window shows (the
  pointer's shape on screen, the title bar, the clipboard, how it feels) is
  unchecked unless a person looked: say so.
