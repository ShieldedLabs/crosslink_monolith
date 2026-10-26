# Removing the `zebra-crosslink` crate and splitting off the GUI

Finality state lives in `zebra-state` (FINALITY.md §7.1), so `zebra-crosslink/zebra-crosslink`
holds no finality state and sits on no consensus path. This file is the work that removes what
is left of it, and it implements the closing paragraph of FINALITY.md §7.1.

Nothing depends on it. It changes no behavior: the tree as it stands meets FINALITY.md while
keeping a crate that holds a GUI feed, a test driver, the wallet, faucet and staking relays, and
a configuration type. Whether that crate is worth removing is a packaging judgement, decided
separately from the finality work and at any later time.

Part 1 is where the crate's contents go. Part 2 is how the visualizer feed is reached once it is
served by the node, and what a client of it may ask for.

## Rules for every change

- Read the FINALITY.md sections a change touches before changing code. FINALITY.md labels each
  statement as **Book**, **Zebra Crosslink**, or **current tree**. Zebra Crosslink statements
  are requirements, Book statements are requirements wherever FINALITY.md does not record a
  Zebra Crosslink departure, and current-tree statements describe code that changes.
- Nothing new is written in `zebra-crosslink/zebra-crosslink`. Work that would land there lands
  in `zebra-state` instead, even where the crate's existing structure would take it
  (FINALITY.md §7.1). That a change fits the TFL service's main loop is an argument against it.
- `σ` and the staking reward and payout code belong to other work. Nothing here changes
  `bc_confirmation_depth_sigma`, `pos_subsidy`, `update_bonds_with_pos_issuance`,
  `fixup_aggregated_stakes`, or the wallet reward projection.
- Databases written by an earlier derivation are deleted, not migrated. No code is added that
  loads them.
- A change to code FINALITY.md describes as current tree updates those FINALITY.md statements
  in the same commit (FINALITY.md §§5, 6, 8).
- `VIZ_GUI_FINALITY_RULES.md` is untracked on purpose and is never committed.
- One agent at a time in one tree, and nothing is pushed.
- Tests run through `phest.bat zebra-crosslink`, with a test-name filter as the fourth
  argument: `phest.bat zebra-crosslink Debug Win64 <filter>`. Never call `cargo` directly.
  The node tests in `zebrad/tests/crosslink.rs` run headless (FINALITY.md §8.1); a non-empty
  `ZEBRA_TEST_GUI` opens the visualizer window for them instead.
  Each node test boots a zebrad in the test process and ends it with `process::exit`, so a
  test run is one process per test, and the harness's capture is turned off so the
  runner's per-instruction dump survives an abort:
  `$env:RUST_TEST_THREADS=1; $env:RUST_TEST_NOCAPTURE=1; .\phest.bat zebra-crosslink Debug Win64 -p zebrad --test crosslink <test name>`.
  Both settings are environment variables because `phargo.bat` forwards `%4` through
  `%9` and splits `--test-threads=1` at the `=`, so a trailing `-- --nocapture
  --test-threads=1` never reaches the harness.
- After a change is committed, `zebra-crosslink/dilated_regtest/run.sh` runs against a debug
  build of the commit and must print `PASS` (DILATED_REGTEST.md). It is the system test the
  node tests are not: wallet, staking, BFT bootstrap and finality under 90x time dilation.
  No `PASS` is recorded against the tree since finality state moved into `zebra-state`.
- A block loaded with `SHOULD_FAIL` costs the harness's full 30-second submission deadline:
  a rejected block never gets an answer from the ingest queue. The fork-rejection tests and
  diagram scene 3 therefore take minutes, not seconds.
- `REGTEST_BLOCK_BYTES` and `REGTEST_POS_BLOCK_BYTES` are written by the `#[ignore]`d
  `regen_test_data` in `zebrad/tests/crosslink.rs`; the diagram scenes by
  `crosslink_write_finality_diagram_scenes`. Both are regenerated, never hand-edited, whenever
  the block format or the header count of a BFT block changes.
- The build uses `panic = abort`: a new `assert!`, `unwrap`, or `expect` on a consensus path
  terminates the node when it fails.
- A gap between what a change asks for and what the test format can express is reported in the
  commit message and left as an `@Todo` beside the test. It never changes the test format.

## Open: one writer thread, or one logical authority

FINALITY.md §7.1 requires one authoritative state, coherent chain views, and serialized
consensus mutations. The tree meets that by running proposal, validation, decisions, bootstrap,
persistence and `fin` on the one `new_network::sync` thread. That couples BFT responsiveness and
liveness to PoW synchronization, and gives one loop several unrelated jobs.

The requirement is narrower than the implementation: related reads use one versioned snapshot,
and mutations that cross domains pass through one serialized authority. A dedicated BFT actor,
or atomic state-service operations, meets it without the shared thread.

To decide: whether FINALITY.md §7.1 states the requirement in that form. If it does, the moves
in Part 1 are written against it rather than against the sync loop.

## Part 1: What moves

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
- Splitting the GUI into its own application is later work. This work only declines the two
  placements that would make it harder: node crates inside `zebra-gui`, and the visualizer inside
  `zebrad`.
- `test_format.rs` splits along a line it already has. The framing — `TFHdr`, `TFSlice`,
  `TFInstr`, `TF` — and the projection of a stored chain into drawable blocks go to the shared
  crate, so that a client can read a `.zeccltf` without a node. Execution against a node —
  `read_instrs`, `test_check`, the bootstrap and parameter conversions — stays node-side beside
  the tests it drives, and `force_feed_pos` becomes a message that injects a decided bft-block
  into the block writer. The split is bounded by the consensus types the format embeds
  (`BftBlockAndFatPointerToIt`, `BftBootstrap`, `ZcashCrosslinkParameters`): the shared half
  reads what is drawn, not what is validated (part 2, `view_instrs_path`).
- The wallet, faucet and staking arms move to their callers in `zebra-rpc` and
  `zebrad/src/lightwalletd.rs`, which call the `wallet` crate directly. The wallet's copy of the
  payout rule, `block_pays_pos_issuance`, travels with them and reads the bft-chain through a
  read request; it stays identical to the other two copies (FINALITY.md §5.4).
- `zebra_crosslink::config::Config` moves to the node configuration. The slashfork types it
  re-exports already live in `zebra-chain`, where `zebra-state` can reach them.

Deletes: `TFLServiceInternal`, `TFLServiceHandle`, `TFLServiceCalls`, `spawn_new_tfl_service`,
`tfl_service_main_loop`, the tower service over `TFLServiceRequest` and the request and response
types themselves, the `zebra-crosslink/zebra-crosslink` crate with its workspace membership and
its `zebra-gui` dependency, and every `use zebra_crosslink::` in the tree. `viz2::run_node`
(open the window if there is a display, else run headless) moves with the feed, to the crate
that hosts it rather than the removed one.

Done when the workspace builds with no `zebra-crosslink` crate, the crosslink node tests pass
from their new home, and the dilated regtest passes.

## Part 2: Reaching the feed: transports and capabilities

Options for running the GUI in the node's process and against a remote node, without giving a
remote client the ability to disrupt the node it is inspecting.

### What the tree already provides

- `zebra-gui` builds its own binary. `zebra-gui/src/main.rs` runs `main_thread_run_program` with
  a synthetic wallet state, and `viz_gui_init(fake_data: bool)` already has a mode that draws
  without a node behind it. The separate application is not a new target; it exists and runs.
- The seam is two statics. `viz_gui.rs` puts the channel ends in `REQUESTS_TO_ZEBRA` and
  `RESPONSES_FROM_ZEBRA`, and `viz2.rs` takes them. Which transport is in use is decided entirely
  by who holds the other end, and nothing above that point knows the difference.
- `RequestToZebra` and `ResponseFromZebra` carry no node types, so they serialize as they stand.

### One protocol, several sources

The GUI consumes `ResponseFromZebra` and emits `RequestToZebra`. Four producers fit behind that
one protocol without the drawing code distinguishing them:

| source | what holds the far end |
|---|---|
| in-process | `viz2.rs` in the node's own process, over the existing channels |
| remote | a client connection to a node serving the feed, over STP |
| synthetic | the fake-data path that already exists |
| replay | a recorded stream of `ResponseFromZebra` frames, played back |

Keeping all of them behind the same message type is what keeps the in-process path from rotting:
the remote server cannot drift from it without breaking it, because it is the same code answering
the same request.

### Transport

Remote messages go over STP (`tenderlink/src/stp.rs`), the transport the node already runs for
bc-block propagation. The connection is encrypted, and each end has an identity keypair, so a
node knows which key it is talking to rather than which address. A node serves the feed to the
client keys in a whitelist it is configured with, and to no others; an empty whitelist serves
no one. STP supplies the authenticated identity and the node's configuration decides which
identities it answers, which is the whole of the authentication — there is no password, cookie
or token scheme of the node's own to write.

Authentication says who is connected, not what they may ask for, so it replaces nothing in the
section below. A whitelisted developer key is still not a reason for a node to read a path off
its own disk on request, and whitelist entries are long-lived while a debugging session is not.
The two questions are independent, and the command surface stays small whether or not the caller
is known.

The wallet runs on the client side. The node serves the feed and does not run a wallet on a
client's behalf, so the GUI keeps `wallet` as a dependency of its own, as `zebra-gui` has today.

### Capability: four ways to keep a remote client from disrupting a node

`RequestToZebra` mixes reads with commands. `bft_pause` stops this node proposing;
`load_instrs_path`, `serialize_instrs_path` and `view_instrs_path` name files on the node's
machine. The question is how the read surface becomes remote without the commands following it.

1. **Check each field at the server.** A remote request arrives, the server refuses the fields it
   should not honour. Cheap to write, and the check is one forgotten `if` away from being absent.
   Every new field is a new chance to forget.
2. **Split the type.** `RequestToZebra` becomes a query part and a command part. The remote
   server's request type contains only the query part, so there is no code path to forget: a
   command cannot be expressed on the wire. Adding a command to the query type is a visible,
   deliberate edit.
3. **Two surfaces.** The read feed and the command surface are separate endpoints, the second one
   off unless the node is started with it on. Same structural property as 2, plus the ability to
   run the read feed publicly while the command surface stays on loopback.
4. **Commands never leave the process.** The wire protocol has only reads. Every command stays a
   thing the in-process GUI can do, because a developer debugging a node is running beside it.

4 costs nothing and loses nothing for a developer with a window open on their own node, so it is
the right starting posture. It fails only for the case of pausing or feeding a node on another
machine, which is a real dev-loop need on a multi-node testnet. 2 is the extension when that
need arrives: the command part exists on the wire, and the node only listens for it when started
with an explicit flag.

None of the four asks for a permission system of the node's own. The whitelist above says which
clients are answered; these decide what any answered client can ask for, and the cheapest version
of that is a surface with nothing dangerous on it.

### The four commands, individually

- `view_instrs_path` need not reach the node at all. It draws a `.zeccltf` file *instead of* the
  node's chains, so it is a client-side concern: with the format's framing in a crate the GUI can
  depend on, the client reads the file itself and the node never learns the path exists, and one
  command disappears rather than being secured. What bounds this is that `test_format.rs` embeds
  consensus types — `BftBlockAndFatPointerToIt`, `BftBootstrap`, `ZcashCrosslinkParameters` — and
  a client that depends on those is not a client any more. It holds only if the subset needed to
  draw a chain (hashes, heights, parents, the fields the inspection structs show) can be read
  without them. If it cannot, view mode stays node-side and is governed like the other commands.
- `load_instrs_path` and `serialize_instrs_path` become content rather than paths. Loading sends
  the file's bytes; serializing streams bytes back and the client writes them where it likes.
  Path traversal stops being possible because no path crosses the connection, and the remote case
  gets better behaviour than the local one: the file is on the machine the developer is sitting
  at. Loading still feeds blocks to a node, so it stays a command under 2 or 4.
- `bft_pause` is the one genuine node-mutating control. It is reversible and scoped to one node,
  but on a node with a finalizer key it is a liveness problem, so it is in-process only until
  someone turns the command surface on deliberately.

### Build and run switches

`zebrad` is one binary: it opens the window when there is a display and runs headless when there
is not, and `--headless` skips the attempt (`viz2::run_node`). Serving the feed must not depend
on being able to draw, or remote inspection is useless exactly where it matters most.

`--viz-listen=<addr>` serves the feed over STP, off by default, and answers only the client keys
in the configured whitelist.

`--remote` belongs on the `zebra-gui` binary, not on `zebrad`: that binary already exists, already
runs without a node, and a node binary that starts no node is a strange thing to ship. The GUI
takes `--connect=<addr>`, and with no argument keeps today's synthetic mode.

### Keeping the in-process path cheap

- One implementation of the answering half, reached by both transports.
- The in-process path stays the default for the development build, so it breaks loudly and early
  rather than quietly.
- A version handshake on the wire, refusing a mismatch with a message. In-process skips it. The
  failure mode this avoids — a stale client drawing a mis-decoded frame — is far more expensive to
  debug than the connection refusing.

### A cheap win alongside this

Recording `ResponseFromZebra` frames to a file and replaying them into the GUI needs no node at
all, and follows from the same abstraction as the remote source. It makes a UI bug reproducible
without reproducing the chain that caused it, and turns a visual report into an attachment.
