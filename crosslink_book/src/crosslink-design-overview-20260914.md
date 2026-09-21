# Zcash Crosslink Design Overview

**Project:** Crosslink (Zcash)

**Status:** Living document — draft 10. Draft 7 plus material folded in from four working notes: `slashing_constraints`, `network_design`, `bootstrap_and_stake_rewards`, `storing_crosslink_in_pow`. Items marked [TBC] are unsettled or were cut off in dictation.

**Related:** *Nikete's Mechanism Design Audit of Crosslink Zebra* (recommended reading for Part IV).

## Terminology used in the working notes and adopted here

- `TFC` — the BFT certificate ("trailing finality certificate").
- `lockbox` — the finalizer bank account of section 14.
- `NSM` — Zcash's network sustainability mechanism (burn/reissue).
- `Tenderlink` — the BFT (Tendermint-derived) node component and its networking; `PoWLink` — the PoW block side-channel.

# CONTENTS

- [**Part I** — What Crosslink is and why](#part-i)
  - [1. Purpose](#section-1)
  - [2. The one-paragraph version](#section-2)
- [**Part II** — The two chains](#part-ii)
  - [3. The proof-of-work side: the fat pointer and where it lives](#section-3)
  - [4. The BFT side: certificates and `σ`](#section-4)
  - [5. Mutual reference and its consequences](#section-5)
  - [6. The BFT implementation](#section-6)
  - [7. Networking and sync](#section-7)
  - [8. Bootstrap and activation](#section-8)
- [**Part III** — What finality means here](#part-iii)
  - [9. The sticky choice rule](#section-9)
  - [10. Kinds of finality and which ledger state is current](#section-10)
- [**Part IV** — Stake](#part-iv)
  - [11. Finalizers and rosters](#section-11)
  - [12. Delegation bonds](#section-12)
  - [13. Staking days](#section-13)
  - [14. Rewards](#section-14)
  - [15. Accounting and the acceleration structure](#section-15)
- [**Part V** — Punishment](#part-v)
  - [16. Social slashing](#section-16)
- [**Part VI** — Open questions](#part-vi)

<a id="part-i"></a>

# PART I — WHAT CROSSLINK IS AND WHY

<a id="section-1"></a>

## 1. PURPOSE

Zcash is a proof-of-work chain, and proof of work gives only probabilistic finality: reversing a block grows ever more expensive, but there is never a moment at which it is definitively settled. In practice the tip of the chain can thrash — small reorganisations near the head as competing blocks race.

Crosslink adds a proof-of-stake BFT (Byzantine fault tolerant) layer whose only job is to finalize the proof-of-work chain. It does not replace proof of work and it does not carry the ledger. It periodically reaches agreement that a specific PoW block is final, and the rest of the design is arranged so that:

- this agreement has teeth (nodes will not reorg past it),
- proof of work remains the primary system, so that if the BFT layer fails the network decays gracefully back to plain PoW,
- stake is genuinely at risk, so finalizers can be held to account,
- all of this is done privately, in keeping with Zcash, and
- the whole thing is robust to malicious peers.

Most specific choices below are downstream of one of those five goals.

<a id="section-2"></a>

## 2. THE ONE-PARAGRAPH VERSION

There are two chains. The PoW chain gets a new field, the "fat pointer", that names a BFT certificate by hash and carries signatures attesting to it. The BFT chain is a sequence of certificates, each of which finalizes one PoW block and carries a few PoW headers above it as evidence of work. Each chain therefore points at the other. Nodes follow the "sticky choice" rule: most-work still picks the best chain, but once a finalized block is on your best chain you never reorg past it, so finality is a ratchet rather than an override. BFT participants are "finalizers"; anyone can delegate stake to them by creating anonymous, fixed-denomination bonds from shielded funds. Rewards are paid uniformly and only while the system is working; there is no in-protocol slashing, because the protocol cannot agree on who voted for what. Instead, slashing is a user-coordinated hard fork that burns all stake delegated to a named finalizer and jails it. All ledger state lives on the PoW chain, and every economic effect is computed from what the PoW chain can see through the fat pointer.

<a id="part-ii"></a>

# PART II — THE TWO CHAINS

<a id="section-3"></a>

## 3. THE PROOF-OF-WORK SIDE: THE FAT POINTER AND WHERE IT LIVES

### What the fat pointer is

The PoW chain carries, per block, a "fat pointer" to the BFT chain:

- a hash identifying a BFT certificate, and
- a set of Ed25519 signatures attesting to that certificate.

The certificate is referenced by hash rather than embedded by value. A certificate contains PoW headers, so embedding it in a PoW block would be recursive. Referring by hash breaks the recursion at the cost of the "not yet known" state discussed in section 5.

The fat pointer exists because a certificate cannot carry its own signatures: the signatures are over the certificate, so they cannot be inside it. Several places for the signatures were considered; alongside the hash on the PoW side is the one settled on. Whether the signature set could be made fixed-size with a different scheme is an open question (Part VI).

All ledger state — balances, bonds, rosters — lives on the PoW chain. The BFT chain finalizes and holds no state of its own. Every payout, burn or slash is computed from what the PoW chain can observe about the BFT chain, and the fat pointer is the only window.

### Where in the PoW block it lives

The current implementation modifies the block header directly and bumps the header version. (The version number is [TBC]; "version 5" was dictated but may be a confusion with transaction v5.) A ZIP call on 2025-08-05 with Jack "str4d" Grigg and Daira-Emma established that this is possible but "fractally difficult", and that the options below should be weighed first. Jack has offered to review early designs.

### Why changing the header is hard

- ASIC miners may have hardcoded header interpretation. Breaking most miners would structurally reduce PoW security.
- The version field has been used inconsistently in the wild (e.g. big-endian 4 and other small numbers). If mining follows the Stratum protocol of ZIP 301, a strict version == 4 is already required and a plain bump would work; if not, the little-endian signed value must still be positive, so the top bit is available as a flag (the approach ZIP 202 uses for "overwintered"). [TODO: survey actual version-field use on chain.]
- Exchanges and others may have custom parsers. SPV and lightwallet protocols are little used, and the mining-pool population is small enough to talk to individually, which may make this tractable.

**Storage options, in increasing order of change required**

- **a. Commit only.** Do not store Crosslink data; add it to the tree behind the existing 32-byte commitment field whose meaning is versioned. Probably insufficient, since some data must actually be stored.
- **b. Coinbase sigscript.** ~100 bytes, and the cap is far easier to raise than the header. Fits a 32-byte hash of the certificate but not signatures. Compatible with designs that ignore signatures on the PoW side, at the cost that a new PoW block's link cannot be verified as carrying the required votes without consulting an up-to-date PoS service.
- **c. Typed memo bundle on the coinbase transaction (all-zero key), committed to.** Up to 16 KB — ample for the current signature format, perhaps not for anything post-quantum.
- **d. Modify the header directly (current approach).** Daira-Emma's caution: unless notarization proofs are short and constant length they do not belong in the header, and putting them in the coinbase merges the indirection with one that is needed anyway to validate the block. A variable-length vector of 32-byte fields, length fixed by semantic version, is one shape.
- **e. Two-level header:** a small fixed-size header committing to a variable-length non-transaction "sidecar" section that holds the Crosslink data and other things. Jack and Daira-Emma were both in favour if the pain of a breaking header change is being paid anyway; there is a backlog of things they would like to fix at the same time (promoting data currently back-doored through the commitment tree, etc.). Top-level headers should fit in a network MTU; if PoW is not in the fixed part, P2P may need care.

A caveat for any option that keeps the data outside the header: PoS blocks cannot then use the PoW headers they carry to directly reach back-references to earlier PoS blocks.

**Further reading:** ZIP 200 (network upgrade mechanism); zcash issues #172, #5755, #1040.

<a id="section-4"></a>

## 4. THE BFT SIDE: TFCs AND σ

The unit of the BFT chain is a TFC. Each non-genesis TFC carries `headers_bc`: exactly `σ` PoW headers, deepest first. The block it finalizes (its "snapshot") is not among them. It is the parent of the first header, named by that header's parent hash. So the `σ` headers are the confirmations above the snapshot, and the snapshot is `σ`-confirmed by construction.

`σ` is a protocol parameter. The prototype sets `σ` = 4. This value has not been checked for security or performance.

### Two TFC validity rules concern `headers_bc`:

- **Tail Confirmation:** the headers are the `σ`-block tail of a bc-valid chain.
- **Linearity:** each snapshot is equal to or descends from the parent TFC's snapshot. Final snapshots only move forward along one PoW chain.

A validator must download and validate the PoW blocks under those headers, not just check the headers' work. Tail Confirmation requires a bc-valid chain, and a proposal whose snapshot the node cannot resolve cannot be validated yet. Headers alone do not establish validity. The trade-off is accepted: finalization waits on the validators receiving those blocks. Tail Confirmation is objective all the same: `σ` consecutive headers ending at a bc-valid block form that block's tail, whatever the validator's own best chain is.

### Inclusion Depth

A PoW block at height P may cite (via `context_bft`) a TFC whose snapshot is at height F only if P >= F + `σ` + 1: the `σ` carried headers F+1 ..= F+`σ`, then the carrier. This is a bc-block validity rule, beside Valid Context, Extension and Last Final Snapshot. Checking it takes a PoW -> PoS -> PoW lookup: resolve the pointer to its TFC, take the TFC's snapshot, look up its height. A block whose snapshot is not yet known is deferred, not rejected. Block templates apply the same test, so a miner is never handed a TFC it could not include.

### Rolling, not batch

An honest proposer carries the `σ`-block tail of its own best chain. If that tail would break Linearity (e.g. after a PoW reorg below the last final snapshot), it repeats its parent's headers instead. A decision at PoW tip T therefore finalizes T - `σ`. Each decision advances the snapshot by however many PoW blocks arrived since the last decision. It repeats the snapshot when no block arrived and jumps several blocks when decisions are slow. This rolling window is how the construction already works; it does not depend on incentives.

**Deviations from the *TFL Book*'s honest proposer:**
- Our proposer clamps the snapshot to at most 40 blocks above the previous final snapshot. When the clamp binds, the proposal is a window ending below the tip. That still satisfies Tail Confirmation. The clamp is a heuristic, not part of Crosslink 2.
- Where the honest proposer would repeat its parent's headers, ours makes no proposal. It declines when the tail would break Linearity, when the candidate would not improve on the last final snapshot, and when a PoW reorg lands between reading the tip and reading the tail. How often a proposer should repeat instead is not yet decided. Both are proposer behavior, not validity rules: a validator cannot tell whether the headers were the proposer's best-chain tail.

### Finality lag

By the Inclusion Depth rule, the first PoW block that can cite a decision at tip T (snapshot T - `σ`) is T + 1, and only if its template was built after the decision arrived. So local finality trails the best tip by at least `σ` + 1 blocks in steady state. Stale templates and slow decisions add more.

<a id="section-5"></a>

## 5. MUTUAL REFERENCE AND ITS CONSEQUENCES

Because each chain references the other, there is a serial dependency in both directions:

### PoW depends on BFT

A PoW block pointing at a certificate the node has not seen cannot be validated until the certificate arrives.

### BFT depends on PoW

A finalizer cannot vote on a proposal whose candidate it has not fully validated, which requires the candidate and its ancestry.

### The third validity state

Conventionally a proposal or block is valid or invalid. Crosslink adds "not yet determinable" on both sides: the node is waiting for data before it can take a position. That data may never arrive — a malicious peer can reference a hash for which nothing exists — so "pending" must be a state that can expire or be abandoned, never one the node blocks on indefinitely. [TBC: the exact rule.]

### Threat model

Robustness to malicious peers — dangling references, withheld data, attempts to wedge validation — is a standing constraint. Hash-only pointers, the third state, and tolerance for data that never arrives all follow from it.

<a id="section-6"></a>

## 6. THE BFT IMPLEMENTATION

The BFT layer (Tenderlink) is a reimplementation of Tendermint. Reuse was not possible because the ternary validity state must be encoded in the protocol itself.

One property shapes the whole economic design: peers reach consensus on the decision for a certificate (was it approved by two thirds of stake-weighted power?), but different peers may hold different subsets of the votes that made up that two thirds. The outcome is agreed; the exact signature set is not. The protocol therefore cannot use "who voted for what" as evidence for anything — not slashing, not per-vote payouts. See sections 11 and 16.

A second assumption of Tendermint matters for slashing: every finalizer must have an identical understanding of who is in the roster. Section 16 spells out what that forbids.

<a id="section-7"></a>

## 7. NETWORKING AND SYNC

### What went wrong with the existing stack

Zcash's sync layer assumes one logical chain in which every block has one parent, so a suffix connecting to a known ancestor is enough to validate everything in it. It bulk-syncs infrequently, and any validation failure triggers a long timeout. Crosslink has two logical chains, each needing knowledge of the other to progress, which produces round-trips like:

receive a decided BFT block → query for its PoW blocks → header missing → PoWLink downloads the chain backwards by hash until Zebra recognises a block → submit PoW blocks in order, but they contain certificate changes that query PoS state → only now can the BFT block be processed.

In workshops at an increased block rate, sync was too slow: people diverged by over a hundred blocks and could not recover without a reset, and a side channel had to be added. Mempool sync also appeared not to happen when the sender is a miner placing the transaction directly in a block.

`σ`-based security depends on sufficiently fast sync, so this is a security requirement, not just a usability one.

### Interim solution: Tenderlink networking and PoWLink

Tenderlink networking is a custom, encrypted-from-the-ground-up (NOISE/Snow) datagram protocol. Keys rather than certificates provide identity and addressing. Packets are MTU-sized with application-level fragmentation and application-controlled (naive) resend and specific-peer targeting; there is no congestion control. Proposals span many packets, votes pack many into one, status messages are one-to-one.

PoWLink is a reliable-stream side channel that downloads PoW blocks and submits them to PoW state. It exploits finality directly to linearise and find the required chain — in effect "more frequent checkpoints" — and is "always up" rather than periodic. Its gain is discovery speed more than raw download speed.

### The new networking stack

Requirements: "little and often" and "high-bandwidth serial beaming"; reliable and unreliable transport; large datagrams; multiple streams per connection; forward and backward secrecy; connection migration and rekeying; upgradeable-but-not-downgradable crypto; high bandwidth, high ping and high jitter; identical API over Nym mixnet and direct connections; and the recognition that networking is CPU work, not just I/O waiting. It is not expected to be compatible with Bitcoin-style sync. Requirements are being coordinated with Nym, ZF and Tachyon.

**Three layers:**

- **1. Transport** — use-case agnostic: congestion control (ECN, loss, bytes in flight), MTU discovery and BDP, bulk transfer with acking, minimally blocking. Data packets are all the same size for indistinguishability. Possible later: opt-in RaptorQ-style loss recovery, compression, "packlet" framing for small items.
- **2. P2P** — probably application-transparent: gossip, UDP hole punching via a third party. Hard-NAT clients (phone wallets) are expected to use a client-server model, which the protocol is designed to support well.
- **3. Application** — sensitive-metadata announcements over Nym (tx announcements; optionally BFT messages and newly mined blocks), BFT and PoW block sync, BFT votes, lightwalletd traffic, bootstrap (DNS?).

**Non-goals for now:** multiple NICs, one logical client in multiple physical locations, rapid reopen of identical connections, symmetric-NAT traversal, local-endpoint discovery, packet relay.

**Why not QUIC:** no Nym path; TLS is redundant with NOISE and brings certificate authorities and downgrade concerns; complexity and audit surface.

**Why not libp2p:** misaligned goals (NAT-poor, relay-heavy, broadcast only), too modular to use effectively, large code volume, pub-sub is the wrong model since every message is globally relevant and should always be gossiped, DHT is unneeded, and it would be a heavy non-Zcash supply-chain dependency for a key component.

### Interleaving

The two streams are currently handled separately. Interleaving them in a serializable order is planned but not done.

<a id="section-8"></a>

## 8. BOOTSTRAP AND ACTIVATION

The BFT chain has no external genesis. It is started deterministically by every node from PoW state alone; nothing about genesis is received from the network or agreed by a separate process. This section records how, since the safety argument otherwise lives only in code.

### Heights (as implemented)

`BOOTSTRAP_ROSTER_HEIGHT = STAKING_PERIOD / 2` (call it `h1`)

`BOOTSTRAP_ACTIVATION_HEIGHT = h1 + 200` (call it `h2`)

`h1`: The PoW block whose staking state supplies the roster that votes on BFT height 0.

`h2`: The PoW height at which a node walks back, finalizes `h1`, and starts BFT. Every PoW block at or below `h2` must carry a nil fat pointer; the first non-nil pointer can appear only above `h2`.

### Safety argument

There is a compile-time assertion that `h2` - `h1` > `MAX_BLOCK_REORG_HEIGHT`. By the time any node reaches `h2`, block `h1` is below the reorg limit and therefore identical on every chain a node could be following. So every node computes the same roster from `h1`, constructs the same BFT genesis, and the Tendermint requirement that all finalizers share one view of the roster (section 6) holds from the first round without any coordination.

This is the bootstrap instance of the general rule in section 16: the roster is a pure function of finalized PoW state, and the finality in question here is reorg-depth finality rather than BFT finality.

### Relation to the three-height plan in the notes

The rewards notes describe three heights: `H1` (staking transactions activate), `H2` (roster is determined), `H3` (first block that may point at a certificate), with `H2` and `H3` fixed in one governance decision and `H2` perhaps `H3` minus 100,000. The code's `h1` corresponds to the notes' `H2` and the code's `h2` to the notes' `H3`; the notes' `H1` (activation of staking actions) is not a separate constant in the code as described. [TBC: reconcile naming, and confirm whether the 200-block gap is a development value versus the ~100,000 suggested for mainnet.]

At activation every block in [0, `h1`] becomes de facto finalized. Whether BFT genesis should point at the PoW genesis or at `h1` remains open.

<a id="part-iii"></a>

# PART III — WHAT FINALITY MEANS HERE

<a id="section-9"></a>

## 9. STICKY FORK CHOICE

Every node, and in particular every miner, must choose a best chain given information from both chains. The spectrum:

### Raw work (Crosslink 2's unmodified fork choice)

Most work wins. A node can follow a heavier chain that excludes its own finalized block. The finalized point is then left on an abandoned branch.

### Follow the latest BFT snapshot (the *TFL Book*'s "Questions" rule)

The best chain must extend the snapshot of the newest final TFC in view. That point need not be on any chain the node has selected, nor `σ`-confirmed in one. The Book's author concludes "Probably not".

### Sticky fork choice — our rule

The floor is the node's own `fin` (`local_finalized_tip`), not the BFT snapshot. A node switches from its current chain to a new one iff `fin` is on the new chain and the new chain has more work (ties broken by tip hash). Equivalently: best = heaviest chain that contains `fin`.

### How `fin` moves

`fin` is never set from a BFT decision directly. On every change of best chain, the node computes

`candidate(best) = lca(snapshot(LF(best)), prune_σ(best))`

where `LF(best)` is the TFC the tip's `context_bft` points at, and `prune_σ(C)` is C with its last `σ` blocks removed. If `fin` is an ancestor of or equal to the candidate, `fin` := candidate. Otherwise `fin` stays put. So `fin` only advances along the node's own best chain, only to blocks the node has itself buried `σ` deep, and never backwards.

### The ratchet, step by step

- A decision arrives. It advances `bft_final_snapshot` (the snapshot of the newest decided TFC), which may be on a side chain. My `fin` does not move yet.
- A PoW block on my best chain cites that decision in its `context_bft`. Once that block is my best tip, `fin` ratchets up to its candidate. I will never again switch to a chain lacking it.
- A side chain carries newer decisions. I sync it anyway, store its blocks and decisions outside the finalized state, and track bonds along it (I need that for the roster, section 11). Under Linearity it contains my `fin`, so it stays eligible.
- If it ever has more work than my chain, I switch, on work alone. `fin` then ratchets to that chain's candidate.
- A heavier chain that forks below my `fin`: I refuse it, whatever its work. This refused switch is the only observable sign of a finality conflict. It is logged; a persisted hazard record is still TODO.

Caveat: Zebra also commits blocks at reorg depth (`MAX_BLOCK_REORG_HEIGHT`). If my best chain runs that far past the fork to `bft_final_snapshot`, the depth commit conflicts with it and I can never switch back. Under Linearity my branch then never finalizes again until I resync.

### Why

BFT does not choose the chain. It only ratchets a floor under the work rule, and only to points my own work-selected chain already has `σ` deep. An advance of `fin` never causes a switch. The rule departs from raw work only when a heavier chain excludes `fin`. By construction that would displace a prefix `σ`-confirmed on my own earlier best chain. While BFT is stalled or withholding, `fin` is frozen and selection above it is plain most-work, so PoW stays the Schelling point.

What this does not buy:
- No Stalled Mode, so the gap between `fin` and the tip is unbounded during a stall, and ordinary spends keep landing above `fin`.
- A miner who dominates the best chain can withhold finality progress by never updating `context_bft`; that costs nothing in validity.
- A partition in which only one side can finalize past the fork leaves that side permanently unwilling to switch to the other.
- The *TFL Book* has no safety or liveness proof for this rule. Its liveness argument relies on unmodified fork choice.

<a id="section-10"></a>

## 10. KINDS OF FINALITY AND WHICH LEDGER STATE IS CURRENT

### BFT finality (`bft_final_snapshot`)

The snapshot of the newest decided TFC. It is objective, and possibly on a chain the node does not consider best, for any length of time. "Crosslink finalized" in conversation usually means this.

### Local finality (`fin` / `local_finalized_tip`)

The node will not reorg past this block. It is node-local and monotone, advanced only from `candidate(best)` as in section 9. It always lies on the best chain and is at or below `bft_final_snapshot` (under Linearity). It is persisted in the finalized database as its own hash.

### Database finalized tip

Not finality. It is the higher of `fin` and Zebra's reorg-depth commit, so it can be above `fin`. It is a physical commit boundary, not a cache of `fin`. Never report it as Crosslink finality. Finality readers take `fin`.

No protocol quantity lies between the best tip and `fin`. A "confirmed" display is plain PoW confirmation depth, never final.

Consensus must never read `fin`. Honest nodes reach the same chain through different histories, so their `fin` values are only prefix-compatible, not equal. Anything every node must compute identically comes from objective chain data:

### Roster for BFT height H

the bonds at `snapshot(B_{H-1})`, the snapshot of the decided TFC at H - 1. BFT agreement fixes that TFC, so every validator derives the same set. It is generally above `fin` and may be off the best chain, so it is read from the synced chain leading to `bft_final_snapshot` (section 11).

### Staking rewards

an objective per-block trigger, not `fin` (section 14).

### PoW-tip state

ordinary ledger state at the best tip. Staking transactions in a block are validated against their own chain.

<a id="part-iv"></a>

# PART IV — STAKE

<a id="section-11"></a>

## 11. FINALIZERS AND ROSTERS

Participants in the BFT layer are finalizers. A finalizer's voting power is the stake delegated to it (section 12). Every "percentage of finalizers" here is stake-weighted, never a headcount.

### Active and inactive rosters

Finalizers are split into an active roster and an inactive roster. The split exists for performance and technical reasons — it bounds the number of parties per BFT round — and is not a reward or penalty mechanism. The roster for a given certificate must be a pure function of finalized PoW state plus the slashing config (section 16). The active roster is selected as the top N finalizers when sorted by voting power, where N is the active roster length (a fixed constant).

### Finalizers opt in

A finalizer publishes an address that acts as a capability carrying its own signature consenting to act as a finalizer. Delegation requires this consent.

### Who is behind a finalizer

Crosslink has no internal web of trust. The expectation is that Zcash organisations will attest outside the protocol that a finalizer is run by a particular provider or company, and that users can combine several attestations into a reasonable judgement.

A possible built-in feature: since finalizers already have keys, they could sign short, namespaced messages that nodes gossip at a rate limit of about one per active finalizer — "down for maintenance Tuesday; planned, not malice." Today such notices go out on forums or social media, unlinked to the on-chain identity. The most important use would be during a slash fork, where finalizers need to signal which side they will be on. [Idea only.]

### Uniformity, and no automatic slashing

Because peers do not agree on exact vote sets (section 6), the protocol cannot reward or punish individual votes. So there is no automatic slashing, and all active finalizers are paid uniformly at the same time. Punishment is a social process (section 16).

<a id="section-12"></a>

## 12. DELEGATION BONDS

Delegation of stake from anyone (including finalizers themselves) to a finalizer is a primitive construct, and the way essentially all stake is assigned.

### Creating a bond

A user spends shielded funds in a transaction carrying a staking action. "Create delegation bond" specifies:

- an amount, which must be a power of ten ZEC — privacy quantization, so bonds cannot be fingerprinted by size;
- a target finalizer, via its consent capability.

The result is a bond: a free-floating note controlled by an Ed25519 key. Holding the key is holding the bond, so it can be operated anonymously, decoupled from the funds that created it. The key is derived from the create action plus a salt, so repeated identical delegations yield distinct, untangled bonds.

The privacy trade-off: bonds are anonymous, but the total staked to each finalizer is public. That total is what voting weight is computed from, so it has to be.

### Bonds are atomic

Bonds cannot be split; retarget, unbond and withdraw act on the whole bond.

### Retargeting

A bond can be retargeted at any time. The transaction records both old and new target, so from the bond state at time t the retargets can be played backwards to recover the mapping at any earlier time. Slashing depends on this.

### Withdrawing: unbond, then withdraw

Two actions on two different staking days (section 13), so minimum exit is two weeks.

1. Unbond. Value depends on which block it lands in; once landed, the bond has a fixed numeric value.
2. Withdraw. The transaction must state that value explicitly and match the chain exactly, or it is invalid.

Two reasons. Accounting clarity: before funds re-enter ordinary Zcash transaction land their amount must be an explicit number, not implicit ledger state. Security: the enforced delay is what puts stake genuinely at risk — if an attack is discovered there is time to slash before the funds escape into the shielded pool.

<a id="section-13"></a>

## 13. STAKING DAYS

One day per week is a staking day. Bond creation, unbond and withdraw are quantized to staking days.

- Privacy: batching activity into one day per week removes timing information that could link actions to users.
- Security: unbond and withdraw on successive staking days gives the two-week minimum exit.

The slash window (section 16) is also measured in staking days.

<a id="section-14"></a>

## 14. REWARDS

### Design stance: smooth enforcement

The economic design leans on withheld income rather than destroyed funds. Uniform, conditional payouts make staking feel like mining: put money in, earn while the system works, and if the system is attacked or stalls you lose income, not principal. Missed income is a far smoother enforcement mechanism than burning, and it means stakers are rewarded or not for the mechanism as a whole working — never for particular finalizer or miner behaviour. Destruction of funds is reserved for the case where users identify a staker as having aided or been totally negligent in an attack, via a slash fork (Part V).

### Proposed issuance split

Of post-dev-fund issuance (ignoring the initial 20% deduction):

48%  miner subsidy 48%  staking rewards (finalizers and stakers) 4%  miner bounty for including a new finality

Within the 48% staking share, the split is 90/10:

90%  to bonds, pro rata by size against total stake 10%  to active finalizers, uniformly

The 90% includes finalizers staking to themselves. Rewards come from a fixed pool; there is no guaranteed percentage yield.

### Proposed issuance rule

IF a PoW block points at a new certificate, AND that certificate finalizes a PoW block close to the tip, THEN stakers and finalizers receive the 48% staking reward and the miner receives the 4% bounty; OTHERWISE the 52% is NSM-burned.

"Close to the tip" is measured entirely on the PoW chain: this block points to a certificate, which points to a PoW block; the delta between those two PoW heights is the distance.

This one rule serves several purposes:
- First-inclusion miner bounty: miners are paid to advance finality monotonically rather than reuse a stale pointer (audit item R2).
- Anti-jackpot: rewards cannot accumulate while finality is stalled (R4). Because the reward is single-shot per block, nothing is ever accrued or recalculated; accumulated values need not be stored anywhere. Bounty sniping is not an issue in steady state and at most a flaky-finalizer concern.
- Certificate aging by another route: instead of expiring old certificates, catch-up is incentivized by withholding rewards from finalizations far behind the tip (R6). During catch-up the current heuristic is roughly 40 blocks per certificate; finalizers are incentivized to coordinate quickly on a known shared prefix rather than skip ahead to unshared tips, because otherwise they earn nothing. This is an incentive, not a consensus constraint. (Aside: FlyClient-style proofs might help finalizers establish a shared prefix fast.)
- Slash forks suspend payouts for their duration, since the pointer stops advancing. This does not disincentivize spring-cleaning forks: their BFT activation is `u32::MAX`, so they are a single point of cremation rather than a stall.

### Bonds always earn

A bond accrues reward whether or not its finalizer is on the active roster. Backing an up-and-coming finalizer costs nothing — deliberate anti-centralization.

Open: is delegating to a non-active finalizer a "vote of no confidence" in the active roster, such that finalizer commission should shrink — i.e. should the 10% denominator be active-roster stake or total stake? ("Vote of no confidence" versus "pay for infrastructure".)

### Finalizer lockboxes (bank accounts)

The 10% goes into a per-finalizer lockbox balance on the ledger. A staking action turns lockbox balance into a bond. The lockbox itself also behaves as a virtual bond delegated to its finalizer, so it accrues from both sources: from the 90% like any bond (per total), and from the 10% (per total, or per active-roster member on each new block — open). Ordinary bonds accrue only from the 90%.

Burns are an extra complication: normal burns come from transactions, whereas these would be implicit or a new data member, and different burn designs have different complexity costs.

Because the active roster is bounded, lockboxes should not need the acceleration structure of section 15, though computing "active" as well as "total" stake is a small addition that is needed for Tenderlink anyway.

<a id="section-15"></a>

## 15. ACCOUNTING AND THE ACCELERATION STRUCTURE

### Why

Very many unmergeable bonds; finalizers, miners and wallets need fast answers (finalizer weight, what is final, what my bonds are worth). An acceleration structure over bonds is required, and it depends on payout details, so it follows them.

### Defer everything

Bond creation is cheap; ongoing work is per-finalizer and global; an individual bond's value is computed only on query or at unbond. Single-shot rewards (section 14) help here: nothing accumulates.

### Rounding

Integer zatoshis divided by an arbitrary total stake guarantee some rounding error. It is minor and should be apportioned sensibly, but the requirement is that every party, at every query resolution, agrees exactly on the value produced for each recipient. Determinism beats precision.

<a id="part-v"></a>

# PART V — PUNISHMENT

<a id="section-16"></a>

## 16. SOCIAL SLASHING

There is no automatic slashing (sections 6, 11). Punishment of stake behind misbehaving finalizers is a user-coordinated hard fork.

### Why social

The protocol cannot agree on vote sets, and "well behaved" is fuzzy: a stall can never be proven permanent. The judgement is left to users, aided by visualization tools that inform but cannot decide.

### Hard constraints on how a slash may work

1. The updated roster must be a pure function of finalized PoW state plus the slashing config. It may not depend on any PoW data not already implied by shared certificates. Tendermint assumes every finalizer has an identical view of the roster, and side chains may validly disagree about recent PoW, so:
- the new roster cannot be derived from staking actions up to the activation tip, and
- it cannot use "the highest certificate the PoW chain points at", since chains may disagree on what that is.
2. Staking actions that have landed in the PoW chain must be respected. Justice: most are unrelated to any slashed finalizer. Practicality: reclaiming them would need extra machinery. PR: in a long stall, ignoring them would look like a huge reorg.
3. PoS stores no ledger information of its own.

4. Application must be robust to users, miners and finalizers adopting the config at different times; no accidental hard forks from unspecified race conditions.

### Design virtues (soft constraints)

Users should preference-cascade to the same input values, so fewer degrees of freedom is better. Users should be able to decide at a suitable resolution. Negligent stakers should be slashed and non-negligent ones not. Miners and finalizers should not be over-weighted in the decision. Roster computation should be cheap in memory and CPU. Forks should be deferred where possible.

### Triggering a slash

Node operators modify their config to list the finalizer(s) to slash and a PoW activation height. Every slash event is a hard fork; there is no default switch, and currently nothing nudges a node onto a fork (a warning system may be added).

### Why a config, not something else

Two goals pulled in different directions: users should have a large amount of control, so the decision is not centralised; but it should also be easy for everyone to agree on the same thing and decide the same way, because divergent decisions mean multiple hard forks.

The alternative considered was having users edit the code directly. It was rejected because:
- the space of possible edits is too large to reach a Schelling point on;
- working through it exposed, as expected, many edge cases where it would be easy to produce a corrupt state, or one where an action on the PoW side rendered the PoS decisions meaningless — a tested implementation with a small set of inputs is far safer than something implemented and decided under time pressure;
- a config lets non-programmers have meaningful input.

A config with few degrees of freedom satisfies both goals: users decide, but there are few enough choices that they converge.

### Two components

#### The Incinerator (PoW chain)

Burns every bond delegated to the named finalizer(s) within a window reaching back two staking days from activation. Retarget history makes that set reconstructible. No partial slashing, no tunable amount.

#### The Sergeant-at-Arms (BFT chain)

Jails the named finalizer(s) from voting in current and subsequent rounds; their weight leaves the pool so the rest can reach two thirds.

### Bridging the fork: the "do not include by" height

PoW must not be disrupted while nodes switch. BFT has stalled; old PoW blocks keep pointing at the last old certificate; once enough finalizers adopt the fork, the reduced set produces new certificates, which must not leak into PoW before activation. So each certificate carries a "do not include by" PoW height, monotonically non-decreasing, which may only increase in the first certificate of a forked chain — the one embedding the slash config. The forked set resumes immediately on the BFT side while PoW transitions at one agreed height.

### How nodes on different configs see each other

Votes are cryptographically namespaced by config, so nodes on different configs diverge immediately, and currently the other side looks like malicious nonsense. This should be improved so a node can recognise "well behaved, different worldview". [TBC.]

### A big stick

A slash fork needs buy-in from essentially every node. Obvious cases behave like a network upgrade; splinter groups would leave no apparent canonical chain. The intended dynamic is that misbehaviour is obvious early and honest stakers retarget away in time — the threat moves stake, the burn is the fallback.

### Scenarios

#### Outright stall

BFT stops with identifiable offline participants. Available information stays consistent; probably the easy case — though one may actually be in the flaky case without knowing it.

#### Flaky stall

BFT stalls, resumes, stalls again; a gradient, not a category. Open: what happens if new certificates are produced between the creation of a slash config and its application?

#### Spring cleaning

No stall, but the stake-weighted share actually voting drifts down toward two thirds because some finalizers are permanently gone. From this perspective it may be fine for the fork to be merely a deadline by which stakers must have moved, rather than an actual punishment — i.e. analysis and application on a single block.

#### Other malicious behaviour

[TBC: not yet elaborated.]

<a id="part-vi"></a>

# PART VI — OPEN QUESTIONS

### Header and encoding

- Which storage option from section 3 to adopt; survey actual version-field use on chain; talk to miners, pools and exchanges.
- Fixed-size fat pointer via a zero-knowledge proof over the signatures: feasibility and proving cost.
- Exact encoding of the fat pointer and certificate, including the "do not include by" field.

### Protocol details

- Timeout/abandonment rule for "not yet determinable" (section 5).
- Whether BFT genesis points at PoW genesis or `h1` (section 8).
- Reconcile the notes' `H1`/`H2`/`H3` with the code's `h1`/`h2`, and the 200-block gap versus ~100,000 (section 8).
- How the active roster is selected (section 11).
- Other anti-tail-thrashing mechanisms (section 1).
- Precise definition of "close to the tip" (section 14).
- 10% denominator: active-roster stake or total (section 14).
- Lockbox accrual from the 10%: per total or per active (section 14).
- Burn representation for withheld rewards (section 14).
- Acceleration structure design once payouts are fixed (section 15).
- Non-liveness misbehaviour slashing should cover (section 16).
- Certificates produced between slash-config creation and application (section 16).
- Why the parameters are what they are: `σ` = 3, 48/48/4, 90/10, weekly cadence, two-staking-day window, power-of-ten sizes.

### Networking

- Interleaving the two sync streams in serializable order.
- Bootstrap/peer discovery (DNS?).

### Slash forks

- Recognising peers on another fork as well-behaved (section 16).
- Signed finalizer messaging channel (section 11).

### Signature cryptography

- Aggregate signatures (BLS or similar) to make the signature set fixed-size. Most such schemes assume equal-weight signers; weight by repeated voting scales badly. Weight-aware schemes may exist or be emerging. Parliament-style quantization is an alternative with its own centralization risk.
- Post-quantum story for the fat pointer, bond keys and finalizer keys — not yet considered. Possible collaboration with the post-quantum cryptography team.

### Wallet UX dependent on cryptography

- Automating the withdraw step without a live wallet.
- Scheduling a staking action from a non-staking day without relying on OS wake-up.

### Loose ends

- Andrew had one further point at the end of session 5 that slipped his mind.
- Off-topic musing from the rewards notes: pay miners by block fullness rather than tx fees?
