# Chain prefix files

A node's finalized database commits blocks and cannot take them back. A chain prefix file is
how a node goes back anyway, and how a chain state gets carried to other tools and nodes. It
holds the node's chain from genesis to some height, and the decided BFT blocks that go with it.
Exporting writes the file; importing loads it into a fresh state through the node's normal
admission paths (#241).

The file is ordinary `.zeccltf` test format (`zebra-crosslink/src/test_format.rs`):

- `SET_PARAMS` with the network's Crosslink parameters;
- `LOAD_POW` for every PoW block from genesis to the height, in the Zcash wire encoding;
- `LOAD_POS` for each decided BFT block: the block, the fat pointer that decided it, and the
  proposer's chunk signatures with the valid round they cover. These are what a node needs to
  re-serve the decided proposal to a lagging finalizer, and what it checks when it loads one.

Because the file is plain test format, it can be:

- read without a node: `viz_gui` can view it, and `CRATE_REMOVAL.md` describes reading it
  client-side;
- converted to another node's storage by anything that can read those three instructions;
- extended with scenario instructions. That is how a network oddity is replayed in context:
  export the chain just before the oddity, append `RECV_POW`, `BFT_RECV_*` and the other
  delivery and expectation instructions that reproduce what happened, and import or run the
  file.

## Exporting

From a stopped node, reading its state directory:

    zebrad -c node.toml export-chain prefix.zeccltf [--height H] [--bft CUTOFF] [--cache-dir DIR]

This opens the state the way `start` does, so the non-finalized backup is restored and
validated. The prefix therefore reaches the best tip, not just the finalized database, which
ends at `fin`.

From a running node, over RPC:

    {"method": "export_chain_prefix", "params": [H, "cited"]}

Both parameters are optional. The node writes the file into `chain_prefixes/` under its state
cache directory, names it for its tip and cutoff, and returns the path, tip, block counts and
size. Callers cannot choose the path.

The GUI's "Serialize chains" writes the whole best chain in the same format, with
`--bft supported`.

### Which BFT blocks

`--bft` picks the decided BFT blocks that go with PoW heights `0..=H`:

| cutoff | keeps |
| --- | --- |
| `cited` (default) | up to the newest BFT block that a PoW block in the prefix cites: the BFT chain as the PoW chain sees it |
| `supported` | every decided BFT block whose snapshot is in the prefix: all a node holding exactly these PoW blocks could validate |
| `none` | no BFT blocks |
| `N` | the first `N` BFT blocks; any whose snapshot is not in the prefix go last and are refused on import |

Each BFT block is placed as early as a live node could have decided it. That is directly after
the PoW block that is its newest carried header, or after its snapshot when those headers lie on
a branch the prefix does not hold. In every case it comes before the first PoW block that cites
it.

That rule puts BFT genesis on a chain-bootstrapped network ahead of the activation height, at
which a live node derives it. A node importing the file therefore stores the file's genesis
before it would derive the same block itself, and the two cannot race.

The database stores each decision's proposal signatures but not the valid round they were made
in, which is part of what they sign. The exporter reads it back from the signatures. It tries the
certificate's signers as the proposer, under the one or two vote namespaces the height can have,
for each round below the decided round, and keeps the valid round that verifies. At round 0
there is nothing to try. If nothing verifies, the decision is written with -1 and the export
logs a warning; a node loading it refuses it if -1 is wrong. That can only happen when the
proposer of a decision made after round 0 did not sign its certificate.

## Importing

    zebrad -c node.toml import-chain prefix.zeccltf [--cache-dir DIR] [--trusted] [--connect]

This builds a new state and runs a node on it. The configured state is never touched. By default
the new state goes beside the configured one, named for the file, as in
`<cache_dir>-import-prefix`. `--cache-dir` picks another directory. The command refuses any
directory that already holds anything. The RPC cookie is written into the new directory too, so
the configured node's RPC clients keep theirs.

The rest of the config applies as written, ports included. Stop the configured node first, or
give the import a config with other ports.

Once the node has a tip, the file is loaded through the test format's instruction reader:

- PoW blocks go through the same ingest queue `submitblock` uses. That means header and PoW,
  body, the crosslink fat-pointer gate, the proof/signature/script checks, then contextual
  validation and commit.
- BFT blocks are validated as proposals and stored as tenderlink decisions.

The state comes out as a node following that chain would have built it: staking, rosters and
`fin` advance by the live rules. The node keeps running afterwards. The log ends with
`chain import: done`, followed by either `all as the file expects` or the instructions that
were not, and the best tip. Stop the node with Ctrl-C, not a hard kill: the blocks above `fin`
reach disk on a graceful shutdown.

The node is isolated while it loads, so only the file's blocks reach it: no PoW or BFT peers,
and no internal miner. `--connect` keeps the configured peers and miner instead, for example to
let the rebuilt node rejoin a network once the file is in.

A new state directory is a new node. Its `secret.seed`, and with it the node's network identity,
wallet and default finalizer key, is generated fresh. To rebuild a node as itself, copy the old
`secret.seed` into the new directory before importing, or set `explicit_bft_key_seed`.

`--trusted` skips only the proof, signature and script checks (`verify_expensive`) for the
file's PoW blocks. Everything else above still runs. Use it for files from a node you trust,
typically your own export. The saving grows with the shielded transactions a chain holds. A
600-block regtest chain of mostly coinbase blocks loaded in 75 s fully verified and in 67 s
trusted, in a debug build, with identical results: same tip, `fin` and roster.

### Parameters

The node imports under its own consensus parameters, which are fixed per network and change
with network upgrades. The file's `SET_PARAMS` records the Crosslink parameters it was exported
under, for diagnosis. If they differ from the node's, that one instruction is reported as not
as the file expects and the import carries on. Whether the blocks are accepted depends only on
the node's own rules for their heights.

## Limits

- **Only the canonical chain.** The database holds one best chain, so side branches, arrival
  order and timing are not in the file. Append them as scenario instructions when they matter.
- **A rebuilt node does not keep valid rounds.** The file carries them and loading checks them,
  but the database has no place for them, so a restart rebuilds every decided round with -1. A
  restarted node re-serves the chunks of a re-proposed value with signatures that do not match.
  This holds for any node, not just an imported one (see `TF_PROPOSAL_SIGS.md`).
- **`supported` and `N` can include BFT blocks no PoW block in the prefix cites.** A node
  importing them holds decisions that the PoW chain has not yet acknowledged. That is the
  intent, but it is a different state from a node stopped at height `H` under `cited`.
