# A Visual Tour of Zcash Crosslink

## Zcash PoW

Currently the Zcash Mainnet uses consensus rules defined by Network Upgrade 6.1. This relies on a Bitcoin-like PoW consensus mechanism, which enables partition-tolerant high availability at the cost of forks / rollbacks. Here's a conceptual diagram of PoW blocks pointing to their parents (via `prevhash` header fields) which shows two objectively-verifiable histories leading back from PoW blocks _B₃_ and _B₂'_, with _B₃_ being the longer:

```mermaid
graph TD
    B3([B₃]):::pow --> B2([B₂]):::pow
    B2 --> B1([B₁]):::pow
    B1 --> B0([B₀]):::pow

    %% A split history:
    B2b([B₂']):::powAlt --> B1b([B₁']):::powAlt
    B1b --> B0

    %% Styles defined in mermaid-common-styles.js and mermaid-styles.md
    %% Define PoW block style with dark blue border and oval shape
    classDef pow fill:#fff,stroke:#01579b,stroke-width:3px,color:#000
    %% Define alternative PoW block style with orange fill for caution
    classDef powAlt fill:#ffcc80,stroke:#01579b,stroke-width:3px,color:#000
```
