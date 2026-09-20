# Removing the `zebra-crosslink` crate

[`IMPLEMENTATION.md`](./IMPLEMENTATION.md) stages 5 to 8 move finality state into `zebra-state`
(FINALITY.md §7.1). They leave `zebra-crosslink/zebra-crosslink` holding no finality state and
sitting on no consensus path. This file is the work that removes what is left of it, and it
implements the closing paragraph of FINALITY.md §7.1.

It depends on stage 8 and nothing depends on it. It changes no behavior: a tree that stops after
stage 8 meets FINALITY.md while keeping a crate that holds a GUI feed, a test driver, the wallet,
faucet and staking relays, and a configuration type. Whether that crate is worth removing is a
packaging judgement, decided separately from the finality work and at any later time.

The rules and the per-stage discipline of IMPLEMENTATION.md apply here unchanged: read the
FINALITY.md sections before changing code, and the node tests and the dilated two-node regtest
(DILATED_REGTEST.md) pass against a build of the commit.

## What moves

- `viz2.rs` moves to the node side, not into `zebra-gui`. It is the answering half of a protocol
  that already exists: `zebra-gui/src/viz_gui.rs` defines `RequestToZebra` and `ResponseFromZebra`
  and carries them over two `std::sync::mpsc` statics, and `viz2.rs` fills the response from the
  state service and the published round-state snapshot. Both messages are already plain data —
  integers, strings, `Hash32`, `[u8; 32]` and `wallet` types, with no node type among them — so
  putting the GUI in its own process replaces the two channels with a transport and a
  serialization, and changes the logic on neither side.
- The message types move to a small shared crate beside `wallet`, outside the `zebra-crosslink`
  workspace, which both halves depend on. Today the node side reaches them by depending on
  `zebra-gui`, which is the wrong direction for a client that the node must not require in order
  to run.
- `zebra-gui` keeps no dependency on any node crate. That is the property that lets it become a
  separate application: a headless node serves the feed, and a GUI built from `zebra-gui`, the
  shared message types and the `wallet` crate connects to it.
- The node-side half belongs where the node already answers remote queries. The `Indexer` service
  in `zebra-rpc/proto/indexer.proto` already streams chain-tip changes, non-finalized state
  changes and mempool changes; the visualizer feed is the same kind of subscription over the same
  surface, and `zebra-rpc` already depends on the state service and the mempool that `viz2.rs`
  reads.
- Four fields of `RequestToZebra` are commands rather than reads. `bft_pause` stops this node
  proposing, and `load_instrs_path`, `serialize_instrs_path` and `view_instrs_path` name files on
  the node's machine. In one process they are a debugging convenience; across a connection they
  are privileged operations, and they are either authorized as node commands are or kept off the
  remote surface entirely.
- Splitting the GUI into its own application is later work. This stage only declines the two
  placements that would make it harder: node crates inside `zebra-gui`, and the visualizer inside
  `zebrad`.
- `test_format.rs` splits along a line it already has. The framing — `TFHdr`, `TFSlice`,
  `TFInstr`, `TF` — and the projection of a stored chain into drawable blocks go to the shared
  crate, so that a client can read a `.zeccltf` without a node. Execution against a node —
  `read_instrs`, `test_check`, the bootstrap and parameter conversions — stays node-side beside
  the tests it drives, and `force_feed_pos` becomes a message that injects a decided bft-block
  into the block writer. The split is bounded by the consensus types the format embeds
  (`BftBlockAndFatPointerToIt`, `BftBootstrap`, `ZcashCrosslinkParameters`): the shared half
  reads what is drawn, not what is validated (VIZ_SPLIT.md).
- The wallet, faucet and staking arms move to their callers in `zebra-rpc` and
  `zebrad/src/lightwalletd.rs`, which call the `wallet` crate directly. The wallet's copy of the
  payout rule, `block_pays_pos_issuance`, travels with them and reads the bft-chain through a
  read request; it stays identical to the other two copies (FINALITY.md §5.4).
- `zebra_crosslink::config::Config` moves to the node configuration. The hardfork types it
  re-exports already live in `zebra-chain`, where `zebra-state` can reach them.

Deletes: `TFLServiceInternal`, `TFLServiceHandle`, `TFLServiceCalls`, `spawn_new_tfl_service`,
`tfl_service_main_loop`, the tower service over `TFLServiceRequest` and the request and response
types themselves, the `zebra-crosslink/zebra-crosslink` crate with its workspace membership and
its optional `zebra-gui` dependency, and every `use zebra_crosslink::` in the tree. `zebrad`'s
`viz_gui` feature survives while the window still runs in the node's process, pointing at the
crate that hosts the feed rather than at the removed one.

Done when the workspace builds with no `zebra-crosslink` crate, the crosslink node tests pass
from their new home, and the dilated regtest passes.
