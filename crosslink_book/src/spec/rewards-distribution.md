# Rewards Distribution

Here we give an overview of how the Zcash protocol with Crosslink enabled distributes protocol rewards.

## The ZEC Supply Policy

We conceptualize all abstract rules about the `ZEC` asset's ownership and accounting as the *ZEC Supply Policy*. This policy is not an emergent property of (any version of) the Zcash consensus protocol, and is instead an explicit part of the conceptual design which protocol implementations and changes are evaluated against.

We propose the following hold for _existing_ Zcash mainnet (NU 6.3) as well as in this Crosslink design:

1. `ZEC` must be scarce, with every unit accounted for by the consensus protocol, all adding up to the *Max Supply Cap* of 21,000,000.
2. The consensus protocol constrains `ZEC` transfers, either between users, or between the protocol and users. All such transfers maintain *Supply Integrity* by ensuring the same number of units transferred away from a set of sources are distributed to a set of recipients.
3. `ZEC` is divided between *Unissued Supply* versus *Active Supply*, with the total of these two categories being the Max Supply Cap.
4. The Active Supply is all `ZEC` that is under a user's discretionary control, given any protocol constraints on those funds. For example, much of the active supply is `ZEC` which cannot be transferred without possession of a spending authority, but another example relevant to Crosslink is that staking bonds have restrictions on transfers, yet end users still have discretion within those constraints, and so both examples are considered part of the Active Supply.
5. The consensus protocol may *issue* or *unissue* `ZEC` by accounting for those units being removed from the Unissues Supply in a transfer to the Active Supply (or vice versa for unissuance), with the following further constraints on issuance:

   The consensus protocol regularly *issues* `ZEC` as part of the network operation in order to incentivize successful operation of the network and establish a valuable network effect of `ZEC` scarcity among users.

   This new `ZEC` is issued by the protocol at a predictable constrained rate, the *Issuance Schedule*, which asymptotically approaches 0 over time with an approximate 4 year half-life.

   <details>
   <summary>Issuance Rate Detail</summary>
   
   - Details:
     - The rate is time-approximate by relying on per-block accounting on the assumption that the block production rate is approximately constant.
     - The rate currently follows the Bitcoin-like "four year halving" schedule, although it may be adjusted by the *Network Sustainability Mechanism* or other changes which alter this "fine-grained" detail of the issuance rate.
   </details>

## Consensus Rewards Distribution

Given the concept of the ZEC Supply Policy, we can envision any version of a given consensus protocol as a component which can take newly issued `ZEC` and distribute it such that all of the consensus guarantees hold, including all of the ZEC Supply Policy.

In other words, one way to conceptualize the consensus protocol designers job is in terms of an operational budget:

> Given that the ZEC Supply Policy provides you with $X$ new `ZEC` to distribute in each time period to protocol participants, design and deploy a self-funding protocol that supports all of the ZEC Supply Policy goals.

FIXME: What about transaction fees?

FIXME: What about "dev fund" / "human-governed funding" that is not direct "consensus operation funding"?

## Crosslink Rewards Distribution Rules

With that framework in mind, here are the concrete rewards distribution rules Crosslink follows:

1. The consensus protocol distributes block rewards (= new issuance + transaction fees) in each block. _Crosslink does not alter the distribution of transaction fees in any way, and their distribution is orthogonal to Crosslink_.
2. A chunk may come out for dev fund or other discretionary governed funding (FIXME: this is part of the policy that's out-of-scope for the consensus protocol; move this section).
3. The remainder is called the *Operational Consensus Rewards* (aka *OCR*).
4. 50% (rounding up) of the OCR is distributed as *Mining Rewards* to the current block miner in the same manner/mechanism as in Zcash NU 6.3. (Note: transaction fees are not altered by Crosslink, and thus accrue to the miner and/or NSM or any other current txn fee design when Crosslink activates.)
5. 50% (rounding down) of the OCR is distributed as *PoS Rewards* as follows:

   a. 90% (rounding down) of PoS Rewards are distributed as *Staking Returns*. Staking returns are divided proportionally among _all_ *Bonds* present in a block.
   b. 10% (rounding up) are distributed as *Finalizer Comission Fees* which are divided propotionally to the *total stake weight* of each *active finalizer*.

     FIXME: Should it be $F^{active}_i / \Sum F^{active}$ or $F^{active}_i / \Sum F^{total}$ ?

### Staking Mechanics

The rewards distribution rules above depend on these staking mechanics:

1. Users can modify their *bonds* with Crosslink-specific transaction fields. All interactions with bonds require that there are no txn inputs or outputs aside from: the bond actions, latest/greatest shielded pool actions, and transaction fee payments.
2. Txns with bond actions may only be included in blocks at *Staking Day* height ranges (FIXME: define these heights).
3. Bonds have a stateful lifecycle:

   - From any Shielded Pool `ZEC`: `create` action
   - From `active` state: `redelegate` or `withdraw` actions
   - From `withdrawing w/ sufficient delay state`: `transfer` action

      - FIXME: define "Sufficient delay", but it should be relatively simple and based on the height of the withdraw action (or maaaaybe an associated staking day on/off height?)

   Actions:

   - The `create` and `withdraw` actions may only occur during Staking Day heights.
   - The `redelegate` and `transfer` actions are not constrained by Staking Day height ranges.
   - The `transfer` action is akin to any other transfer transaction with the additional restriction that ther are no inputs/outputs/actions except for the transfer and the latest/greatest shielded pool actions, plus a transaction fee.

FIXME: consistent rigorous rounding for every apportioning of `ZEC` of entire accounting design.
