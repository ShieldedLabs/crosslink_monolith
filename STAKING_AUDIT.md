# Staking Audit

2026-09-17

Scope: how staking actions (bond creation, retarget, unbond, withdraw, finalizer reward conversion) are admitted to the mempool, carried into block templates, validated in blocks, and slashed.

## Summary

| ID | Severity | Finding |
|----|----------|---------|
| S1 | High | The mempool admits staking actions that block validation rejects, so one bond holder can make every miner's blocks invalid |
| S2 | High | Unbonding and withdrawing inside the slash window escapes the slash |
| S3 | Medium | Consensus accepts retargeting a bond that unbonded earlier in the same block |
| S4 | Medium | Staking actions that spend no inputs can be replayed |
| S5 | Low | The mempool is first-seen per bond: no replacement and no sequencing |
| S6 | Low | Every pending staking transaction is fully re-verified on every new block |
| S7 | Low | The staking day window has hardcoded exception heights |
| S8 | Medium | ZIP-317 fees for VCrosslink transactions ignore everything but the staking action |

The authorization model is sound: the bond key is an ed25519 key, and every staking action must carry that key's signature over the transaction's sighash (`staking_action_signature`). No one without the private key can author, alter or replace an action on someone else's bond. The signature is committed to by the transaction's auth digest, so it can't be swapped in a relayed block either. Every finding below is about what the *holder* of a bond key, or anyone holding a copy of a transaction that key already signed, can do.

---

## S1. The mempool admits staking actions that block validation rejects

**Severity: High**

Block validation (`validate_delegation_bonds`) checks, among other things:

- A retarget's `from_finalizer` must equal the bond's current target.
- A finalizer reward conversion must be covered by that finalizer's reward bank.

The mempool check (`check_staking_action_bond_state`) enforces neither. Bond info queries don't return the bond's target or the bank balance, so the mempool can't check them.

**Impact.** A retarget with a wrong `from`, or a conversion larger than the bank, passes mempool verification and is placed in every block template. The mined block is then rejected by the state, so the miner loses the block. The transaction passes re-verification on every new tip too, so it stays in the mempool and keeps spoiling templates until it expires. A staking action with expiry height 0 never expires. The cost to the attacker is one bond (for the retarget) or one finalizer key (for the conversion). The attacker never pays the transaction's fee, because the transaction is never mined. This is the same class of failure as a template carrying an action on an already-unbonding bond.

**Fix.**
1. Return the bond's target and the finalizer's bank balance from bond info queries. Check `from_finalizer` and the bank in `check_staking_action_bond_state`.
2. As defence in depth, have the block template builder validate its candidate block against the state and evict any transaction that fails. The mempool check and the block check are separate code paths, so they will drift apart again.

## S2. Unbonding and withdrawing inside the slash window escapes the slash

**Severity: High**

Slashing burns bonds that pointed at the slashed finalizer at any point in the window before activation: bonds still active on it, bonds unbonding from it, and bonds that retargeted away from it. Withdrawn bonds are skipped. The only wait between unbond and withdraw is `STAKING_ACTION_DELAY`, which is much shorter than the slash window. So a delegator who sees misbehaviour coming can unbond and withdraw before activation and keep the full amount. The slashing module already notes this.

**Fix.** Either burn (or claw back) bonds withdrawn inside the window, or require the unbonding period before withdrawal to be at least the slash window.

## S3. Consensus accepts retargeting a bond that unbonded earlier in the same block

**Severity: Medium**

`validate_delegation_bonds` tracks what earlier transactions in the block did using separate per-kind maps: new bonds, unbonding bonds and retargets. `validate_bond_for_retarget` consults the new-bonds map and the chain, but not the unbonding map. So a block with *unbond, then retarget* on the same bond is accepted, even though retargeting an unbonding bond is otherwise invalid. The mempool rejects the same pair across two blocks, so this behaviour appears only in blocks a miner builds deliberately.

**Impact.** It breaks the rule that unbonding bonds keep their target. Slashing still sees the retarget's `from`, so this is not known to escape a slash. But any other logic that assumes an unbonding bond's target is fixed can now be wrong.

**Fix.** Replace the per-kind maps with one in-block overlay per bond: status, target, amount, last action height (and sequence number, see below). Validate every action against that one record and update it after each action. This removes the whole class of "one map was forgotten" bugs, not just this instance.

## S4. Staking actions that spend no inputs can be replayed

**Severity: Medium**

`has_inputs_and_outputs` exempts staking transactions from the input requirement. A transaction with no inputs spends no nullifier and no UTXO, so nothing in the chain marks it as used. Take a bond that retargets A→B, then later B→A. The original A→B transaction becomes valid again, as long as it hasn't expired. A transaction with expiry height 0 never expires.

Mempool fee policy narrows who can do this. A staking action counts as one ZIP-317 action, so the mempool requires a fee, and a transaction with no inputs pays none. The wallet funds that fee from spent notes, so wallet-built retargets and unbonds do spend inputs and can't be replayed. A withdrawal can pay its fee out of the bond instead. What remains is a consensus gap: an input-less action is valid in a block, so a miner can include or replay one directly, and fee policy isn't binding on miners.

Unbond and withdraw are protected by the bond's status, which never returns to an earlier state. Creation is protected because an existing bond key can't be created again; that holds only while bond records, including withdrawn ones, are never pruned. Retarget has no such protection.

**Fix.** Either make consensus require staking transactions to spend an input or carry a minimum fee, or add a per-bond sequence number (see the design notes below). The sequence number is the more complete fix: it doesn't depend on how the transaction is funded.

## S5. The mempool is first-seen per bond: no replacement and no sequencing

**Severity: Low (design)**

The mempool holds at most one staking action per bond; a second is rejected. This correctly keeps templates free of pairs that can't both be valid, but:

- **No replacement.** A bond holder who changes their mind can't replace a pending action. A low-fee action pins the holder's own bond until it's mined or expires.
- **Rebroadcast pinning.** Once an action has been broadcast, anyone holding a copy can keep submitting it to nodes that don't have it. At those nodes it blocks a later action from the same holder.
- **No sequencing.** A holder can't queue "retarget, then unbond". The second action is only valid once the first has been applied, and the mempool validates against the tip.

Mempool contents differ per node, so which of two conflicting actions is mined is always up to the miners. This is inherent to every blockchain: replacement policies (Bitcoin's replace-by-fee, Ethereum's same-nonce replacement) raise the odds for the intended version but don't guarantee it. Ordering *within* a chain of dependent actions is solvable, and is already partly enforced: a retarget's `from` must match the previous retarget's `to`, so two chained retargets in one block validate in only one order.

**Fix.** See the design notes: a sequence number in consensus, plus fee-bump replacement keyed on (bond, sequence) in the mempool.

## S6. Every pending staking transaction is fully re-verified on every new block

**Severity: Low**

Whether a staking action is valid depends on its bond's state at the tip, so on every tip growth the mempool removes all staking transactions and sends them back through the full verifier, proofs included. Mempool size bounds the cost, but it grows with the number of pending staking transactions times the number of blocks they wait.

**Fix.** Split re-verification: keep the cached proof and signature results, and re-run only the contextual bond-state check. The same check is needed anyway for S1.

## S7. The staking day window has hardcoded exception heights

**Severity: Low**

`check_staking_day_window` exempts a fixed list of block heights from the window rule. These are leftovers from an earlier deployment. Any other network with blocks at those heights inherits the exemption.

**Fix.** Remove the list. If existing chain history needs it, key it on the specific network.

## S8. ZIP-317 fees for VCrosslink transactions ignore everything but the staking action

**Severity: Medium**

`conventional_actions` returns early for VCrosslink transactions: 1 logical action if the transaction has a staking action, 0 otherwise. Its transparent inputs and outputs and its Orchard and Ironwood actions aren't counted, and the grace minimum doesn't apply.

**Impact.**
- **Underpriced block space.** A VCrosslink staking transaction can carry many shielded actions for the price of one, 5,000 zats.
- **Near-free spam at top priority.** A VCrosslink transaction without a staking action has a conventional fee of 0. It passes the unpaid-actions rule with any fee, and the only floor left is the size-based minimum of 100 to 1,000 zats. Block template weighting divides the fee by the conventional fee. With a conventional fee of 0 that ratio is infinite and is capped at the maximum, so these transactions get the highest template priority. Wallets don't build such transactions, but anyone can.
- **Wallet and node disagree.** The wallet's fee rule counts every action plus the staking action, so honest wallets overpay compared with what the node requires.

**Fix.** Compute the standard ZIP-317 count for VCrosslink transactions and add the staking action on top, the way the wallet's fee rule does. Decide whether a staking action should cost more than one action: it adds a bond record kept permanently, its bond state is checked again on every new block, and slashing reads it.

---

## Design notes: per-bond sequence numbers

Proposal: every action that changes a bond carries a `u32` sequence number. The bond record stores the next expected number. It starts at 0 when the bond is created, each applied action must carry exactly that number, and applying the action increments it. This is validated the same way `from_finalizer` is: against the in-block overlay first, then the chain.

### Does it prevent replay?

Yes, completely, for every action kind. After an action is applied, its sequence number is consumed forever, so the A→B→A cycle in S4 can't bring the old transaction back. This works whether the transaction spends inputs or not, and whatever its expiry.

### Does it enable replacement?

It provides the conflict identity: two actions with the same (bond, sequence) are mutually exclusive, like two Ethereum transactions with the same nonce. The replacement rule is still mempool policy, and **"prefer the newest seen" is not a safe rule**:

- **Arrival order is per node.** An older version that arrives at a node after the newer one would replace it there. That includes a copy an attacker keeps rebroadcasting (S5), so the stale version wins wherever it lands last.
- **It never settles.** Nodes disagree about which version is newest, so the network gossips versions back and forth.
- **Replacement is free.** Each replacement makes every node re-verify and re-gossip. A key holder can generate a steady stream of them at no cost.

Ethereum avoids this by requiring the replacement to raise the fee by a minimum step. That ordering is monotone, the author controls it, and it prices the churn. The same rule works here: a replacement for (bond, sequence) must pay more than the transaction it replaces, by at least its own conventional fee. Mempool policy already requires staking transactions to pay a fee, so this needs no consensus change beyond the sequence number itself.

### Does it fix S3 (unbond, then retarget in the same block)?

Not on its own. *Unbond at sequence n, retarget at n+1* is correctly ordered; the retarget is invalid because of the bond's status, not its position. It helps indirectly: in-block validation must track each bond's current sequence number, and the natural place for that is the single per-bond overlay proposed in S3. Once status, target and sequence number live in one record, the retarget sees the unbonding status and is rejected.

It doesn't fix S1 either. An action can carry the right sequence number and a wrong `from_finalizer`, so the mempool still needs the target check.

### Should `from_finalizer` stay?

Yes. Slashing finds bonds that left a slashed finalizer inside the window by reading the `from` of retargets in the window blocks, with no index. A sequence number doesn't name a finalizer, so it can't replace `from`.

### Sequence numbers or replace-by-bond?

They work at different layers and fit together:

| | Replay protection | Replacement | Queued sequences |
|---|---|---|---|
| Replace-by-bond (mempool only) | No | Yes, one pending action per bond | No |
| Sequence number (consensus only) | Yes | First-seen only | Possible |
| Both: replace by (bond, sequence) with fee bump | Yes | Yes | Possible |

Costs of the sequence number:
- A consensus change.
- A new field in the bond record: a database format change, reverted on reorg.
- Wallets must learn a bond's current sequence number from the chain (a restored wallet can recover it from there).
- A stuck action at sequence n blocks every later action on that bond until it is mined, replaced or expires.

**Recommendation.**
1. Add the sequence number in consensus.
2. Make the mempool accept only the sequence number the tip expects (still one pending action per bond), with fee-bump replacement keyed on (bond, sequence).
3. Later, if wallets need multi-step flows such as "retarget, then unbond", accept future sequence numbers. That needs a per-bond pending overlay in the mempool, template ordering by sequence number, and evicting every later action when an earlier one is replaced.

Queuing matters less than it sounds. Most sequences collapse to one action: A→B then B→C is just A→C. The exceptions are sequences whose intermediate steps have side effects, such as slashing liability or the staking action delay.

---

## Open questions

- Retarget is exempt from both the staking day window and the staking action delay, so a bond can move to a different finalizer in every block. Slashing tracks these moves through `from`, but is unrestricted hopping intended? It lets stake follow whichever finalizer looks safest at the last moment.
- A retarget doesn't reset the unbond delay, which is measured from bond creation. So *retarget, then unbond* is valid in a single block. Intended?
- Should withdrawn bond records be retained permanently? Protection against replaying a bond's creation depends on it.
- Should staking actions have a consensus minimum fee? Zcash has none; fees are mempool policy. But staking actions add permanent state, and miners can include fee-free ones directly.
