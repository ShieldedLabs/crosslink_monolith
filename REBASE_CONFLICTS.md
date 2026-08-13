# REBASE_CONFLICTS

Branch `the_great_rebase`: one commit on top of `s1_dev` bringing in all upstream changes.
**The tree does not build yet.**

Upstream merged: zebra `05d129b88` (2026-08-07, 661 commits), librustzcash `b04030138`
(2026-08-08, 1804 commits).

    git grep -l '^<<<<<<< ' -- zebra-crosslink librustzcash

## Done

- **Crosslink transaction rebased onto mainnet v6 (Ironwood / NU6.3).** `Transaction::VCrosslink`
  was a copy of the *v5* body plus `staking_action`. It is now the *v6* body -- Orchard bundle
  widened to `orchard::ShieldedDataV6` (NU6.3 flag byte / `enableCrossAddress`) and the new
  `ironwood_shielded_data` bundle added -- with `staking_action` appended after it. Both the
  serializer and deserializer now mirror the `(6, true)` arm exactly, so a Crosslink transaction
  is a strict extension of a mainnet one. `ironwood_value_balance()` and
  `staking_action_value_balance()` are both summed into the transaction value balance.
- `zebra-chain/src/transaction.rs`, `transaction/serialize.rs`, and both root `Cargo.toml`s resolved.
- 57 files crosslink had deleted, kept deleted (53 `book/`, 2 `.github/`, `CONTRIBUTING.md`,
  `zebra-state/src/service/queued_blocks/tests/vectors.rs`).
- `librustzcash/zcash_client_memory/` **removed**, following upstream's deletion in `59c685cd3a`
  (2026-06-20). An earlier pass here kept it, on the assumption that crosslink depended on it.
  It does not: the crate was declared in `wallet/Cargo.toml` but never imported by a single `.rs`
  file anywhere in the monolith, and no other librustzcash crate depended on it. It was only being
  maintained because workspace membership forced it to keep compiling against crosslink's changed
  `WalletRead` trait -- hence the 12 files of conformance patches, and the disabled `build.rs`
  (renamed to `build_BAK.rs`). Dropping it removes 12 files / 4,070 lines and 11 conflicts.
- `zcash_extensions` accepted as deleted (upstream removed it; crosslink never modified it).

## Remaining

89 files still carry conflict markers. 5 further conflicts carry **no** markers
(modify/delete and rename/delete, left at one side's version) -- grep will not find those.

### librustzcash (19)

- `librustzcash/Cargo.lock`
- `librustzcash/components/zcash_protocol/src/consensus.rs`
- `librustzcash/components/zcash_protocol/src/constants.rs`
- `librustzcash/pczt/Cargo.toml`
- `librustzcash/rust-toolchain.toml`
- `librustzcash/zcash_client_backend/build.rs`
- `librustzcash/zcash_client_backend/lightwallet-protocol/walletrpc/service.proto`
- `librustzcash/zcash_client_backend/src/data_api/wallet.rs`
- `librustzcash/zcash_client_backend/src/proposal.rs`
- `librustzcash/zcash_client_backend/src/proto/compact_formats.rs`
- `librustzcash/zcash_client_backend/src/proto/service.rs`
- `librustzcash/zcash_client_backend/src/scan.rs`
- `librustzcash/zcash_primitives/src/block.rs`
- `librustzcash/zcash_primitives/src/transaction/builder.rs`
- `librustzcash/zcash_primitives/src/transaction/components/orchard.rs`
- `librustzcash/zcash_primitives/src/transaction/mod.rs`
- `librustzcash/zcash_primitives/src/transaction/sighash_v5.rs`
- `librustzcash/zcash_primitives/src/transaction/txid.rs`
- `librustzcash/zcash_transparent/src/builder.rs`

### zebra-crosslink (70)

- `zebra-crosslink/.github/workflows/book.yml`
- `zebra-crosslink/.github/workflows/zizmor.yml`
- `zebra-crosslink/.gitignore`
- `zebra-crosslink/Cargo.lock`
- `zebra-crosslink/README.md`
- `zebra-crosslink/book/src/README.md`
- `zebra-crosslink/book/src/SUMMARY.md`
- `zebra-crosslink/zebra-chain/Cargo.toml`
- `zebra-crosslink/zebra-chain/src/block/header.rs`
- `zebra-crosslink/zebra-chain/src/history_tree.rs`
- `zebra-crosslink/zebra-chain/src/parameters/network/testnet.rs`
- `zebra-crosslink/zebra-chain/src/parameters/network_upgrade.rs`
- `zebra-crosslink/zebra-chain/src/parameters/transaction.rs`
- `zebra-crosslink/zebra-chain/src/primitives/zcash_primitives.rs`
- `zebra-crosslink/zebra-chain/src/value_balance.rs`
- `zebra-crosslink/zebra-chain/src/value_balance/arbitrary.rs`
- `zebra-crosslink/zebra-chain/src/value_balance/tests/prop.rs`
- `zebra-crosslink/zebra-consensus/Cargo.toml`
- `zebra-crosslink/zebra-consensus/src/error.rs`
- `zebra-crosslink/zebra-consensus/src/transaction.rs`
- `zebra-crosslink/zebra-network/Cargo.toml`
- `zebra-crosslink/zebra-network/src/config.rs`
- `zebra-crosslink/zebra-network/src/config/tests/vectors.rs`
- `zebra-crosslink/zebra-rpc/Cargo.toml`
- `zebra-crosslink/zebra-rpc/src/config/mining.rs`
- `zebra-crosslink/zebra-rpc/src/methods.rs`
- `zebra-crosslink/zebra-rpc/src/methods/tests/snapshot.rs`
- `zebra-crosslink/zebra-rpc/src/methods/tests/vectors.rs`
- `zebra-crosslink/zebra-rpc/src/methods/types/get_block_template.rs`
- `zebra-crosslink/zebra-rpc/src/methods/types/get_block_template/zip317/tests.rs`
- `zebra-crosslink/zebra-rpc/src/server.rs`
- `zebra-crosslink/zebra-script/CHANGELOG.md`
- `zebra-crosslink/zebra-script/Cargo.toml`
- `zebra-crosslink/zebra-script/src/lib.rs`
- `zebra-crosslink/zebra-state/Cargo.toml`
- `zebra-crosslink/zebra-state/src/config.rs`
- `zebra-crosslink/zebra-state/src/constants.rs`
- `zebra-crosslink/zebra-state/src/error.rs`
- `zebra-crosslink/zebra-state/src/lib.rs`
- `zebra-crosslink/zebra-state/src/request.rs`
- `zebra-crosslink/zebra-state/src/response.rs`
- `zebra-crosslink/zebra-state/src/service.rs`
- `zebra-crosslink/zebra-state/src/service/arbitrary.rs`
- `zebra-crosslink/zebra-state/src/service/finalized_state.rs`
- `zebra-crosslink/zebra-state/src/service/finalized_state/disk_format/chain.rs`
- `zebra-crosslink/zebra-state/src/service/finalized_state/disk_format/upgrade.rs`
- `zebra-crosslink/zebra-state/src/service/finalized_state/zebra_db/block.rs`
- `zebra-crosslink/zebra-state/src/service/finalized_state/zebra_db/chain.rs`
- `zebra-crosslink/zebra-state/src/service/non_finalized_state/chain.rs`
- `zebra-crosslink/zebra-state/src/service/non_finalized_state/tests/prop.rs`
- `zebra-crosslink/zebra-state/src/service/non_finalized_state/tests/vectors.rs`
- `zebra-crosslink/zebra-state/src/service/queued_blocks.rs`
- `zebra-crosslink/zebra-state/src/service/read.rs`
- `zebra-crosslink/zebra-state/src/service/read/difficulty.rs`
- `zebra-crosslink/zebra-state/src/service/read/find.rs`
- `zebra-crosslink/zebra-state/src/service/tests.rs`
- `zebra-crosslink/zebra-state/src/service/write.rs`
- `zebra-crosslink/zebra-state/src/tests/setup.rs`
- `zebra-crosslink/zebra-state/tests/basic.rs`
- `zebra-crosslink/zebrad/Cargo.toml`
- `zebra-crosslink/zebrad/build.rs`
- `zebra-crosslink/zebrad/src/commands/copy_state.rs`
- `zebra-crosslink/zebrad/src/commands/start.rs`
- `zebra-crosslink/zebrad/src/components/inbound/tests/fake_peer_set.rs`
- `zebra-crosslink/zebrad/src/components/inbound/tests/real_peer_set.rs`
- `zebra-crosslink/zebrad/src/components/mempool/tests/vector.rs`
- `zebra-crosslink/zebrad/src/components/sync.rs`
- `zebra-crosslink/zebrad/src/components/sync/downloads.rs`
- `zebra-crosslink/zebrad/src/config.rs`
- `zebra-crosslink/zebrad/tests/common/cached_state.rs`

Suggested order: `zebra-chain` (10) unblocks the type change; then `zebra-state` (25),
the largest cluster, where upstream rewrote `service.rs` and deleted `queued_blocks.rs`.
