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

Subtree pushes use `git subtree push --rejoin`. The first push after enabling
this may still scan the full monolith history; the rejoin commit it creates is
the checkpoint that makes later pushes incremental. Preserve these rejoin
commits when merging monolith history.

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
