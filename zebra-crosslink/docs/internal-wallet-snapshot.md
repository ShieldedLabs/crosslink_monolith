# Internal Wallet Snapshot

The Crosslink internal wallet keeps an optional restart cache at:

```text
<state.cache_dir>/wallet.snapshot
```

The snapshot is a local cache for the internal wallet's derived state: manual wallet records, known transactions, the PoW hash cache, and the Orchard shard tree. Zebra state, `pos.chain`, and `secret.seed` remain the authoritative node state.

## When snapshots are used

- Persistent state runs save and load `wallet.snapshot` from `state.cache_dir`.
- Ephemeral state runs do not save or load the snapshot.
- A snapshot is used only if its stored `secret.seed` bytes and genesis hash match the current run.
- Missing, mismatched, or invalid snapshots are ignored and the internal wallet scans from genesis.

The file is written atomically via a temporary file and rename. The reader checks the snapshot magic, version, seed, genesis hash, bounded lengths, and trailing bytes before accepting it.

## Operational notes

- Treat `wallet.snapshot` as a local plaintext cache; do not share it as a recovery authority.
- After rollback or state rebuild, move `wallet.snapshot` aside and let it regenerate from the rebuilt chain state.
- The internal wallet requires zaino. If zaino is enabled but cannot start, `zebrad start` fails rather than continuing into a wallet sync loop that cannot make progress.

## Related config

```toml
[crosslink]
disable_the_headless_wallet = false
disable_zaino = false
reset_zaino_on_startup = false
```

- `disable_the_headless_wallet`: skips the internal wallet task.
- `disable_zaino`: skips zaino startup. Use this only when the internal wallet is also disabled.
- `reset_zaino_on_startup`: deletes the zaino cache under `state.cache_dir` before starting zaino.
