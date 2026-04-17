# staking-cli

`staking-cli` is a thin command-line wrapper around `stake_orchard_to_finalizer_batch` for sequential multi-bond staking.

It reads a seed file, syncs a single account from `lightwalletd` or `zainod`, then builds and optionally broadcasts one orchard stake transaction per requested amount.

The seed file must contain either:

- a plain-text bip39 mnemonic
- a 32-byte hex seed for testing only

Example:

```bash
cargo run --manifest-path zebra-crosslink/staking-cli/Cargo.toml -- \
  --target 00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff \
  --amounts 100000,200000,300000 \
  --seed /path/to/seed.txt \
  --network regtest \
  --lightwalletd-url http://127.0.0.1:9067 \
  --dry-run
```

The endpoint at `--lightwalletd-url` must already be synced enough for the wallet scan to build the orchard tree state needed for staking.
