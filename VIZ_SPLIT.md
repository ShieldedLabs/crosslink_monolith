# Splitting the visualizer: transports and capabilities

Options for running the GUI in the node's process and against a remote node, without giving a
remote client the ability to disrupt the node it is inspecting. `CRATE_REMOVAL.md` covers where
the node-side feed lives; this file covers how it is reached and what it is allowed to do.

## What the tree already provides

- `zebra-gui` builds its own binary. `zebra-gui/src/main.rs` runs `main_thread_run_program` with
  a synthetic wallet state, and `viz_gui_init(fake_data: bool)` already has a mode that draws
  without a node behind it. The separate application is not a new target; it exists and runs.
- The seam is two statics. `viz_gui.rs` puts the channel ends in `REQUESTS_TO_ZEBRA` and
  `RESPONSES_FROM_ZEBRA`, and `viz2.rs` takes them. Which transport is in use is decided entirely
  by who holds the other end, and nothing above that point knows the difference.
- `RequestToZebra` and `ResponseFromZebra` carry no node types, so they serialize as they stand.

## One protocol, several sources

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

## Transport

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

## Capability: four ways to keep a remote client from disrupting a node

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

## The four commands, individually

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

## Build and run switches

Two independent axes, which is what lets one build serve operators and developers:

- Window support is a compile-time feature. A server build does not link `winit`, `softbuffer` or
  the font stack. Developers build with it.
- Serving the feed is a *separate* compile-time feature from window support. A headless server
  build that cannot draw anything must still be able to answer a remote client, or remote
  inspection is useless exactly where it matters most.

At runtime: `--headless` on a build with window support starts the node and opens no window;
`--viz-listen=<addr>` serves the feed over STP, off by default, and answers only the client keys
in the configured whitelist.

`--remote` belongs on the `zebra-gui` binary, not on `zebrad`: that binary already exists, already
runs without a node, and a node binary that starts no node is a strange thing to ship. The GUI
takes `--connect=<addr>`, and with no argument keeps today's synthetic mode.

## Keeping the in-process path cheap

- One implementation of the answering half, reached by both transports.
- The in-process path stays the default for the development build, so it breaks loudly and early
  rather than quietly.
- A version handshake on the wire, refusing a mismatch with a message. In-process skips it. The
  failure mode this avoids — a stale client drawing a mis-decoded frame — is far more expensive to
  debug than the connection refusing.

## A cheap win alongside this

Recording `ResponseFromZebra` frames to a file and replaying them into the GUI needs no node at
all, and follows from the same abstraction as the remote source. It makes a UI bug reproducible
without reproducing the chain that caused it, and turns a visual report into an attachment.
