# Chain prefix files

A node's finalized database commits blocks and cannot take them back. A chain prefix file is
how a node goes back anyway, and how a chain state gets carried to other tools and nodes. It
holds the node's chain from genesis to some height, and the decided BFT blocks that go with it.
Exporting writes the file; importing loads it into a fresh state through the node's normal
admission paths (#241).

The file is ordinary `.zeccltf` test format (`zebra-crosslink/src/test_format.rs`):

- `SET_PARAMS` with the network's Crosslink parameters;
- `LOAD_POW` for every PoW block from genesis to the height, in the Zcash wire encoding;
- `LOAD_POS` for each decided BFT block: the block and the fat pointer that decided it.

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

## Importing

    zebrad -c node.toml import-chain prefix.zeccltf [--trusted]

This starts the node as `start` would and, once it has a tip, loads the file through the test
format's instruction reader:

- PoW blocks go through the same ingest queue `submitblock` uses. That means header and PoW,
  body, the crosslink fat-pointer gate, the proof/signature/script checks, then contextual
  validation and commit.
- BFT blocks are validated as proposals and stored as tenderlink decisions.

The state comes out as a node following that chain would have built it: staking, rosters and
`fin` advance by the live rules. The node keeps running afterwards. The log ends with
`chain import: done`, followed by either `all as the file expects` or the instructions that
were not, and the best tip.

`--trusted` skips only the proof, signature and script checks (`verify_expensive`) for the
file's PoW blocks. Everything else above still runs. Use it for files from a node you trust,
typically your own export. The saving grows with the shielded transactions a chain holds. A
600-block regtest chain of mostly coinbase blocks loaded in 75 s fully verified and in 67 s
trusted, in a debug build, with identical results: same tip, `fin` and roster.

Import into an empty state directory. Blocks the state already holds load as known. Blocks on
another chain are treated as any competing branch would be: the node's fork choice and `fin`
decide what becomes of them.

To replay without outside influence, start the importing node isolated:

- `[state] network_initial_peers = []`
- `[crosslink] bft_peers = []`
- `[mining] internal_miner = false`

Otherwise peers and the node's own miner add blocks while the file is loading.

## Limits

- **Only the canonical chain.** The database holds one best chain, so side branches, arrival
  order and timing are not in the file. Append them as scenario instructions when they matter.
- **Proposal signatures are dropped.** `LOAD_POS` does not carry the proposer's per-chunk
  proposal signatures that the database stores. A node rebuilt from a file cannot re-serve
  those decided proposals to lagging finalizers. Everything else replays identically.
  `TF_PROPOSAL_SIGS.md` proposes the instruction that keeps them, and notes a related gap in the
  database itself: the signed `valid_round` is not stored either.
- **`supported` and `N` can include BFT blocks no PoW block in the prefix cites.** A node
  importing them holds decisions that the PoW chain has not yet acknowledged. That is the
  intent, but it is a different state from a node stopped at height `H` under `cited`.
