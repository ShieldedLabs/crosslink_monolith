# Important Disclaimer About This Repository

This repository exists to prove ideas, not to demonstrate production engineering. It is a prototype designed to let us move quickly and validate the Crosslink design.

It is **not** production-ready code, and it is **not** what would ultimately be proposed upstream. You'll find shortcuts, rough edges, and code that's optimized for rapid iteration rather than long-term maintainability.

The implementation proposed for upstream integration will be developed to the standards expected of production software, with the appropriate architecture, testing, review, and documentation. 

## Vulnerability Reporting

In the future, we would like to receive thorough reviews by bug-hunting teams, but currently we would kindly ask that you direct your vulnerability discovery efforts to: https://github.com/zcashfoundation/zebra

# crosslink_monolith

A subtree'd monorepo for all crosslink code/dependencies

## Headless staking (no GUI)

Nodes built or run without the visualizer can mine → fund → stake entirely
headlessly. The wallet's staking actions (the same ones behind the GUI's stake
buttons) can be driven by the built-in headless wallet, gated by environment
variables. **Everything is off by default**; set on the `zebrad start` process:

| Variable | Effect |
|---|---|
| `CROSSLINK_AUTO_SEND=1` | each wallet cycle, forward mined funds from the miner wallet to the user (staking) wallet |
| `CROSSLINK_AUTO_STAKE=1` | during staking windows, bond the user wallet's spendable funds to a finalizer (at most one bond per window) |
| `CROSSLINK_STAKE_TARGET=<64 hex>` | finalizer public key to bond to, in the same (display) byte order the GUI stake box accepts; **defaults to this node's own finalizer key** |

Behavior notes:
- A staking window opens every 150 PoW blocks; the bot acts only in the first
  60 blocks of a window so the transaction can confirm inside it.
- The first bond of any size implicitly registers the finalizer. After that,
  only chunks ≥ 1 cTAZ are bonded (largest power-of-10 chunk that fits the
  spendable balance, minus a small fee reserve) to avoid dust bonds.
- If the target key is unset and the node's own key isn't published yet at
  startup, the bot waits and retries — it never bonds to a zero key.
