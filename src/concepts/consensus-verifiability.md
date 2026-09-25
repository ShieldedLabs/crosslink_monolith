# Consensus Verifiability

The primary purpose of cryptographic consensus protocols is to enable people to interact with strangers all over the world where a person's locally controlled software can provide safe, timely guarantees that some globally available state was updated through an interaction.

To do this, a person's software needs to be able to _verify_ that a particular interaction resulted in a change to some globally available state and the resuling state is _valid_ according to a set of pre-defined rules.

Unfortunately, "global state" is a useful fiction: 

### Objectivity and Subjectivity in Consensus

#### Objective Verifiability

Verifying nodes run software which _verifies_ whether a particular ledge history is valid according to consensus rules, which all nodes must agree on to arrive at the same view of the network's ledger. In order to ensure any two nodes would arrive at the same decision as to whether a given history is valid or not, their decision must be _objectively verifiable_. Precisely:

> An **objectively verifiable** property of a ledger history can be computed using _only_ that ledger history as input.

Objectively verifiable properties include all of the _ledger state_ that users care about: balances, when transfers occurred, what authorization criteria were fulfilled to enable a transfer, the current total supply, the supply totals for different shielded pools, how changes to balances and ownership were bundled into (objectively verifiable) transactions, and so forth.

By relying _only_ on the ledger history, and not any other auxillary, node-specific information, a node ensures that a property it verifies is the same value any other node would verify from that same history. If nodes relied on non-objective input to their verification process, they would lose the guarantee that other nodes would arrive at the same conclusion for a given history.

The top-line objectively verifiable property of a ledger history is a boolean value: _consensus-valid_. By accurately _excluding_ any ledger history which is not objectively verifiable as consensus-valid, nodes protect their users from large categories of malice and accident with respect to the ledger state.

#### Intersubjectivity

Nodes, like people, are immersed in subjectivity. Local data and code can compute values locally, which nodes can then rely on innately through the magic of computation, but there's no a priori guarantee any other entity has access to the the same input or calculated data. Furthermore, if presented with arbitrary data along with a claim that it is a conclusion or result of some kind of verification, a node can only rely on this claim by either performing their own verification locally, or relying on the third party as an authority (or anyone who can foil authentication attempts of that authority).

When performing local verification, a wonderful advancement Zcash in particular has leveraged are proofs-of-integrity which allow verifying a claim objectively without needing to reproduce all of the original input data and direct calculation steps.

However, none of this can 


