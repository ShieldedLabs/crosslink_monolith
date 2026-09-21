# Rewards Distribution

Here we give an overview of how the Zcash protocol with Crosslink enabled distributes protocol rewards.

## The Supply Distribution Rules

We conceptualize all abstract rules about the `ZEC` asset's ownership and accounting as the *Supply Distribution Rules*. These are an abstraction which is codified and implemented by a more concrete consensus protocol, and even more concrete running software.

### Top-Line Rules

- `ZEC` must be scarce, with every unit accounted for by the consensus protocol, all adding up to the *Max Supply Cap* of 21,000,000.
- The consensus protocol constrains `ZEC` transfers, either between users, or between the protocol and users. All such transfers maintain *Supply Integrity* by ensuring the same number of units transferred away from a set of sources are distributed to a set of recipients.
- `ZEC` is divided between *Unissued Supply* versus *Active Supply*, with the total of these two categories being the Max Supply Cap.
- The consensus protocol *issues* new `ZEC` units by transferring them out of the Unissued Supply to any user or protocol-managed destination.
- `ZEC` is *issued* by the protocol at a predictable constrained rate, the *Issuance Schedule*, which asymptotically approaches 0 over time.
  - Details:
    - The rate is time-approximate by relying on per-block accounting on the assumption that the block production rate is approximately constant.
    - The rate currently follows the Bitcoin-like "four year halving" schedule, although it may be adjusted by the *Network Sustainability Mechanism* or other changes which alter this "fine-grained" detail of the issuance rate.
  - The protocol *issues* `ZEC` by subtracting a number of units defined by the Issuance Schedule from the Unissued Supply and adding that into the Active Supply as part of the *Protocol Rewards*.
- The consensus protocol enables and constrains the `ZEC` supply to change

#### Consensus Rewards Distribution

## Operational Consensus Rewards
