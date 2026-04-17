# Crosslink Chain Semantics Reference

This note collects four chain-semantics questions that came up repeatedly in the workshop channel and points back to the code for each answer.

## Finalizer pub_key byte order

Node-facing finalizer formatting reverses the stored bytes before printing them. `PubKeyID`'s `Display` and `Debug` impls both call reverse-byte helpers, and Tenderlink log/debug context strings format finalizer IDs through `PubKeyID` ([`librustzcash/zcash_primitives/src/bft.rs#L335-L356`](librustzcash/zcash_primitives/src/bft.rs#L335-L356), [`tenderlink/src/lib.rs#L825-L839`](tenderlink/src/lib.rs#L825-L839), [`tenderlink/src/lib.rs#L594-L597`](tenderlink/src/lib.rs#L594-L597)).

`get_tfl_roster_zats` returns `zcash_primitives::transaction::RosterMember` ([`zebra-crosslink/zebra-rpc/src/methods.rs#L1941-L1955`](zebra-crosslink/zebra-rpc/src/methods.rs#L1941-L1955)). `RosterMember.pub_key` is hex-serialized directly from the raw `[u8; 32]` storage order, and the same raw order is written to bytes and read back into wallet state without reversal ([`librustzcash/zcash_primitives/src/transaction/mod.rs#L1415-L1438`](librustzcash/zcash_primitives/src/transaction/mod.rs#L1415-L1438), [`zebra-crosslink/wallet/src/lib.rs#L3431-L3487`](zebra-crosslink/wallet/src/lib.rs#L3431-L3487)).

The GUI's "copy" path reverses `member.pub_key` before hex-encoding it, which matches the human-facing finalizer form used elsewhere ([`zebra-gui/src/ui.rs#L2847-L2850`](zebra-gui/src/ui.rs#L2847-L2850)).

Convert between the RPC form and the log/display form with:

```python
log_hex = bytes.fromhex(rpc_hex)[::-1].hex()
rpc_hex = bytes.fromhex(log_hex)[::-1].hex()
```

## Staking cycle

Consensus uses a 150-block staking period with a 70-block staking window. Staking actions are valid only when `block_height % 150 < 70`; transactions outside that window are rejected ([`zebra-crosslink/zebra-consensus/src/transaction.rs#L863-L870`](zebra-crosslink/zebra-consensus/src/transaction.rs#L863-L870), [`zebra-crosslink/zebra-consensus/src/transaction.rs#L936-L967`](zebra-crosslink/zebra-consensus/src/transaction.rs#L936-L967), [`zebra-crosslink/zebra-consensus/src/error.rs#L239-L247`](zebra-crosslink/zebra-consensus/src/error.rs#L239-L247)).

At the current community 30-second-per-PoW-block estimate, a 70-block window is about 35 minutes.

For operators using the shipped GUI, "staking day" is keyed off the finalized PoW tip, not the unfinalized tip. `bc_finalized_tip_height` is sourced from `internal.latest_final_block`, and the UI applies the same 150/70 window check to that finalized height ([`zebra-crosslink/zebra-crosslink/src/viz2.rs#L155-L160`](zebra-crosslink/zebra-crosslink/src/viz2.rs#L155-L160), [`zebra-gui/src/lib.rs#L66-L68`](zebra-gui/src/lib.rs#L66-L68), [`zebra-gui/src/ui.rs#L1142`](zebra-gui/src/ui.rs#L1142)).

## Fat pointer signer semantics

Each PoW block header carries a fat pointer to the BFT chain tip in `fat_pointer_to_bft_block` ([`zebra-crosslink/zebra-chain/src/block/header.rs#L110-L111`](zebra-crosslink/zebra-chain/src/block/header.rs#L110-L111)). The RPC entry point is `get_tfl_fat_pointer_to_bft_chain_tip` ([`zebra-crosslink/zebra-rpc/src/methods.rs#L1958-L1972`](zebra-crosslink/zebra-rpc/src/methods.rs#L1958-L1972)).

`FatPointerToBftBlock` is defined as a 44-byte `vote_for_block_without_finalizer_public_key` payload plus a `signatures` vector ([`librustzcash/zcash_primitives/src/bft.rs#L423-L425`](librustzcash/zcash_primitives/src/bft.rs#L423-L425)). `FatPointerToBftBlock::from_parts` writes those 44 bytes as:

- 32 bytes: BFT block hash
- 8 bytes: PoS height as `u64` little-endian
- 4 bytes: round and precommit overhead as `u32` little-endian

See [`librustzcash/zcash_primitives/src/bft.rs#L493-L500`](librustzcash/zcash_primitives/src/bft.rs#L493-L500). `get_vote_template` reconstructs the full 76-byte vote by adding the finalizer pubkey back in later, per signature entry ([`librustzcash/zcash_primitives/src/bft.rs#L512-L519`](librustzcash/zcash_primitives/src/bft.rs#L512-L519)). The underlying vote builder uses the same layout: 32-byte finalizer pubkey, 32-byte block hash/value ID, 8-byte height in little-endian, and 4 trailing bytes for round and precommit flag ([`tenderlink/src/lib.rs#L1142-L1148`](tenderlink/src/lib.rs#L1142-L1148)).

Tenderlink does not put the whole roster into the fat pointer. It filters down to non-`NIL` precommit signatures for the decided proposal only ([`tenderlink/src/lib.rs#L132-L154`](tenderlink/src/lib.rs#L132-L154)). Decision threshold is stake-weighted `2f+1`, where `f = (total_active_stake - 1) / 3`; the block is decided once `yes_precommits` reaches that threshold ([`tenderlink/src/lib.rs#L576`](tenderlink/src/lib.rs#L576), [`tenderlink/src/lib.rs#L844-L856`](tenderlink/src/lib.rs#L844-L856), [`tenderlink/src/lib.rs#L985-L991`](tenderlink/src/lib.rs#L985-L991)).

In practice, the signer list in a fat pointer is a quorum certificate for at least 67% of active stake, not a roster dump. A finalizer missing from one certificate is therefore not enough evidence to conclude it missed a vote.

## Bond privacy

The current bond format is per-bond, not aggregate. `StakingAction_CreateNewDelegationBond` holds one `amount_zats`, one `unique_pubkey` bond key, and one `target_finalizer` ([`librustzcash/zcash_primitives/src/transaction/mod.rs#L1445-L1463`](librustzcash/zcash_primitives/src/transaction/mod.rs#L1445-L1463)). The wallet's `stake_orchard_to_finalizer` flow creates one such action per call ([`zebra-crosslink/wallet/src/lib.rs#L1951-L1969`](zebra-crosslink/wallet/src/lib.rs#L1951-L1969)), and wallet state plus GUI state track bonded positions as separate `(bond_key, target_finalizer, amount)` tuples ([`zebra-crosslink/wallet/src/lib.rs#L935-L936`](zebra-crosslink/wallet/src/lib.rs#L935-L936), [`zebra-crosslink/wallet/src/lib.rs#L4288-L4300`](zebra-crosslink/wallet/src/lib.rs#L4288-L4300), [`zebra-gui/src/ui.rs#L1144-L1149`](zebra-gui/src/ui.rs#L1144-L1149)).

Channel consensus from 2026-04-17: each Orchard-side stake bond is intentionally kept as its own private position. Consolidating multiple bonds into one on-chain restake would link those positions and weaken privacy. The current pain around multi-bond restaking is therefore a tooling gap, not a hidden alternative encoding in the current bond format. For a workflow helper built on the same per-bond model, see [`stake_orchard_to_finalizer_batch` PR #18](https://github.com/ShieldedLabs/crosslink_monolith/pull/18).
