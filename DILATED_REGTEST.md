# Dilated regtest

A two-node Crosslink regtest run under 90x time dilation, as a system test of a build: it
funds a wallet, bonds stake to both finalizers, bootstraps BFT from the chain and checks that
BFT keeps deciding while PoW runs. It exercises the node, the in-node wallet, the light-wallet
server, tenderlink and the finality rules together, which no unit test does.

    zebra-crosslink/dilated_regtest/run.sh [TARGET]

Runs from Git Bash on Windows or a Linux shell; needs `curl`, `jq` and a debug `zebrad` built
with `phuild.bat zebra-crosslink Debug Win64 -p zebrad`. Everything it writes goes to
`zebra-crosslink/dilated_regtest/out/` (ignored by git): the rendered node configs, both
state directories and both nodes' logs, with the pre-restart logs kept beside them as
`node<n>.log.1`. Exit status 0 is a pass.

## What it does

1. Renders `node0.toml.in` and `node1.toml.in` with the repository path and the launch time
   as `start_unix_time`, so apparent time starts at the real launch and runs 90x faster. Both
   nodes start with the internal miner off.
2. Waits for node 0's wallet to finish its first sync pass, mines four blocks with the regtest
   `generate` RPC (coinbase to the wallet's miner account; coinbase maturity is two blocks on
   this tree) and moves 0.5 ZEC to the user account through `requestfaucetdonation`.
3. Bonds 0.2 ZEC to each node's finalizer address through `staking_command`, taking the
   addresses from each node's own `finalizer address:` startup line, and mines each
   transaction in. Both bonds must be in the chain before the bootstrap roster height (75),
   because BFT height 1's roster is the set of stakes at that height and an empty roster means
   BFT never starts.
4. Mines through `generate` without pausing until node 0's tip reaches the restart height,
   halfway between the activation height and `TARGET`, printing a sample of both nodes every
   50 blocks.
5. Kills both nodes, moves their logs aside and starts them again against the same state
   directories, then mines on to `TARGET` (default 450). The kill is hard, so each node comes
   back at its committed height rather than its old tip and re-mines the difference.
6. Checks both nodes and prints `PASS` or every failed check.

## What it checks

- Both nodes reach `TARGET` and agree on the tip within two blocks.
- Node 0 logged the bootstrap at the activation height (275) and neither node reported an
  empty roster.
- No panic, and no `ERROR` line other than the known placeholder
  `not yet implemented: all the documented validations`, which `BftBlock::try_from` logs for
  every BFT block it builds.
- At least one BFT decision per four PoW blocks after activation, on both nodes.
- The final height is within 40 blocks of the tip on both nodes.
- Both nodes resumed BFT from their database after the restart, at no lower a height than they
  had decided before it, and neither re-ran the bootstrap. The resumed height may be one short
  of the decisions counted in the pre-restart log: a decision advances `bft_final_snapshot`
  before its row is written, so a kill in between loses that row and the height is decided
  again.
- Both nodes decided further BFT blocks after the restart.
- No `pos.chain` file exists anywhere under the output directory.

## Timing

The dilation applies to the consensus clock and the miner's pacing, and within tenderlink to
the send tick and to the Prevote and Precommit timeouts, all through
`zebra_debug_time::real_duration`. A step's timeout dilates exactly when the thing it waits for
dilates: Prevote and Precommit wait on vote gossip, which leaves on the dilated tick, while
Propose waits on the proposer reading the chain and building a block, which is real work on a
real state service and so keeps its undilated budget. Dilating a step past what it waits on
makes every message late by construction, which is what a run measures: with the tick left real
against dilated timeouts the two nodes decided 20 blocks to height 450 and finality trailed the
tip by 124 blocks mid-run; with both corrected they decide about 80 and finality trails by
single digits. Transport and the wallet's proving time are real. BFT then keeps pace with PoW:
one BFT block per PoW block, with the fat pointer advancing every one to three blocks. In a
debug build `generate` takes about three seconds per block, so a run to 450 takes some twenty
minutes; the internal miner mines about four times faster but cannot be switched on from
outside the GUI, which is what keeps the test on `generate`.

Mining must not pause once stake is placed. While the tip sits within σ of the last final
height every proposal is empty, both nodes prevote nil and each round's timeouts grow with the
round number, so a pause of three minutes has cost seven minutes of BFT recovery.

## Known noise

- `****** WALLET TREE ROOT MISMATCH at H` from node 0's wallet: the wallet's orchard tree
  disagrees with the tree state the light-wallet server reports, while the node itself accepts
  every transaction the wallet builds from that tree. The reported tree state is what is
  stale.
- `chain updates have stalled` from the sync progress task while the wallet is being funded.

## Towards a post-commit system test

- A mining toggle RPC (setting `wallet::GUI_ENABLE_MINE`) lets the run use the internal miner
  once stake is placed, cutting a run to 450 to a few minutes; a release build cuts the
  wallet's proving time further. The target is a full run in about two minutes, stopping
  between heights 300 and 600.
- A `generate` that returns as soon as the block is submitted, rather than after the template
  round trip, would give the same speedup without a new RPC.
- The run is a step of every stage in `IMPLEMENTATION.md`, after the stage's node tests pass
  and before its commit is pushed. Wired to a post-commit hook or a self-hosted runner it
  becomes the system test for every commit on `dev`.
