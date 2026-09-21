# The Zcash Supply Policy

Zcash, like other cryptocurrency protocols, maintains a ledger codifying the distribution and rules of a scarce, permissionless, asset called `ZEC`. In essence, the entire purpose of the Zcash protocol is to ensure the supply integrity of `ZEC` while enabling permissionless storage and transfer of the asset between users.

The characteristics of a cryptocurrency asset, the consensus protocol embodying that asset, and the community of users of the asset form a mutually-dependent system:

- The asset is only valuable when it has the right set of characteristics protected by a protocol and a user community which judiciously adopts or rejects newly proposed protocol changes.
- Meanwhile, the protocol requires a set of operational users to maintain the network infrastructure, and it relies on the asset to have the right characteristics in order to have the capability to incentivize permissionless network operation with the asset.
- Finally, the users rely on the asset to store value or to be exchangeable with goods and services, and they use the protocol to achieve this. They also must decide when to adopt or reject potential changes to the protocol.

## The Characteristics of `ZEC` are Protected by the Users

It's a common misconception to believe "a cryptocurrency protocol" (defined as the rules encoded into production software) is what provides and preserves the asset's characteristics.

This is only true on small time-scales, but it is actually the users' collective beliefs and actions in the longer run which preserve the asset's characteristics, since any given set of protocol rules must evolve through different versions over time.

For this reason it is especially important to distinguish the idealized characteristics one believes are essential to the asset itself, versus those characteristics which are incidental, due to the current protocol's implementation details.

### Zcash vs Bitcoin Culture on Protocol Change

Bitcoin, the original and precedent-setting precursor to Zcash, developed a development community notion that "the implementation is the spec", thereby explicitly erasing this distinction between idealized asset characteristics versus implementation quirks. In principle, this extreme means every incidental and accidental behavior becomes enshrined as a characteristic detail of `BTC`, the asset.

By contrast Zcash culture has distinguished between "the current protocol rules" vs "the intended abstract ideal", which has enabled, for example, lowering blocktimes and adjusting the issuance rate per block to approximately the original wall-clock issuance rate as Bitcoin (offset in time to account for the relative start dates), the deprecation of `sprout`, changes to transaction fees, changes to transaction formats, discretionary transfer of new issuance to explicit community governed recipients, and many other divergences from earlier revisions.

However, in Zcash development norms, there is still a more restrictive Overton window for protocol changes compared to many cryptocurrencies, especially around the top-line supply characteristics of `ZEC`.

## The Top-Line `ZEC` Characteristics

We posit that the following characteristics of `ZEC` are held by the majority of the current and past user communities as not mere incidents of the protocol implementation, but rather intentional explicitly desired characteristics:

- `Max Supply Cap`: There must always be less than 21,000,000 `ZEC` into the foreseeable future, even as the protocol changes.
- `Limited Issuance Rate`: Newly issued ZEC should approximately asymptotically approaches 0, with a four year half-life, which is not only necessary to preserve the Max Supply Cap, but also limiting the issuance rate in this manner is a desirable characteristic in itself to help ensure a predictable distribution of the supply over time.
- The supply must be storable and transferrable in a permissionless manner with strong privacy protections.

### Uncertain Precedent Characteristics

There are some characteristics of Bitcoin and/or the current and previous Zcash protocol versions which have less obvious agreement across the community, which we phrase here as "Should `ZEC` ...?" questions:

- Should `ZEC` issuance be a constant rate over approximately a four year span, with that rate halving after each four year span? (This is what `BTC` and `ZEC` have done so far, but there seems to be disagreement in the community if this is a key desireable characteristic of `ZEC` or an incidental property.)
- Should initial issuance and transfers between of `ZEC` between shielded pools always be transparent? (This ensures the total transparent / top-line supply integrity as a bit of cost to "migration privacy". An example of the opposite trade-off would be enabling fully private migration between pools or across bridges.)

## Protocol Rewards Distribution

xxx

