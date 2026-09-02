# Crosslink Chain Semantics Reference

Four chain-semantics questions that come up repeatedly, each pinned back to the code.

Line numbers are against `s1_dev` at the time of writing and will drift. Symbol names are given alongside every citation so the reference survives that drift.

## Finalizer pub_key byte order

`PubKeyID` reverses its stored bytes for display. `Display` and `Debug` both go through the reverse-byte helpers `fmt_byte_str_rev` / `fmt_prefixed_byte_str_rev` (`librustzcash/zcash_primitives/src/bft.rs:504-526`), and `Debug` prints only the leading two bytes as `Pub{..}`. The `PubKeyID` serde impls reverse as well (`librustzcash/zcash_primitives/src/bft.rs:528-559`).

Tenderlink log and debug context strings format finalizer IDs through `PubKeyID`, so they carry the reversed form: `ctx_str` and `name_str_other` (`tenderlink/src/lib.rs:892-907`).

The RPC form is the opposite. `get_tfl_roster_zats` returns `zcash_primitives::transaction::RosterMember` (`zebra-crosslink/zebra-rpc/src/methods.rs:317-318` for the trait, `:1951-1966` for the impl). `RosterMember.pub_key` is `#[serde(with = "hex")]` over the raw `[u8; 32]`, so it is hex-encoded in storage order with no reversal, and the raw `write_to_vec` / `read_from` pair uses storage order too (`librustzcash/zcash_primitives/src/transaction/mod.rs:1415-1442`). The wallet decodes in storage order as well (`zebra-crosslink/wallet/src/lib.rs:3486-3549`).

The GUI "copy" path reverses `member.pub_key` before hex-encoding, matching the human-facing form (`zebra-gui/src/ui.rs:3665-3671`).

Convert between the RPC form and the log/display form with:

```python
log_hex = bytes.fromhex(rpc_hex)[::-1].hex()
rpc_hex = bytes.fromhex(log_hex)[::-1].hex()
```

## Staking cycle

Consensus uses a 150-block staking period with a 70-block window. `STAKING_PERIOD` and `STAKING_DAY_WINDOW` are defined in `librustzcash/zcash_primitives/src/transaction/mod.rs:1551` and `:1555`, and imported by consensus (`zebra-crosslink/zebra-consensus/src/transaction.rs:27`). The check rejects a staking action once `position_in_period >= STAKING_DAY_WINDOW` (`zebra-crosslink/zebra-consensus/src/transaction.rs:944-980`, specifically `:969-976`; error variant at `zebra-crosslink/zebra-consensus/src/error.rs:242-251`).

Two exceptions to the window rule:

- `RetargetDelegationBond` is exempt (`zebra-crosslink/zebra-consensus/src/transaction.rs:956-958`).
- A hardcoded height exception list `[1120, 2320, 2620, 2621, 3224]` bypasses the check (`zebra-crosslink/zebra-consensus/src/transaction.rs:962-966`).

At the current 30-second-per-PoW-block estimate a 70-block window is about 35 minutes.

The GUI "staking day" indicator keys off the **unfinalized** PoW tip, not the finalized one: `let is_staking_day = viz.bc_tip_height % UI_COPY_STAKING_PERIOD < UI_COPY_STAKING_DAY_WINDOW` (`zebra-gui/src/ui.rs:1575`, constants at `zebra-gui/src/lib.rs:81-82`, gating its consumers at `zebra-gui/src/ui.rs:2162` and `:2178`). Consensus instead checks the height of the block the transaction actually lands in, so the indicator and the consensus verdict can disagree near a window boundary or across a reorg. `bc_finalized_tip_height` is still populated from `internal.latest_final_block` (`zebra-crosslink/zebra-crosslink/src/viz2.rs:174-179`), but the only remaining consumer combining it with the period math is the sound cue (`zebra-gui/src/lib.rs:1589-1596`), which compares with `>` rather than `<`.

## Fat pointer signer semantics

Each PoW block header carries a fat pointer to the BFT chain tip in `fat_pointer_to_bft_block` (`zebra-crosslink/zebra-chain/src/block/header.rs:110-111`). The RPC entry point is `get_tfl_fat_pointer_to_bft_chain_tip` (`zebra-crosslink/zebra-rpc/src/methods.rs:321-322` for the trait, `:1968-1983` for the impl).

`FatPointerToBftBlock` is a 44-byte `vote_for_block_without_finalizer_public_key` payload plus a `signatures` vector (`librustzcash/zcash_primitives/src/bft.rs:641-645`). `from_parts` writes those 44 bytes as (`librustzcash/zcash_primitives/src/bft.rs:711-720`):

- 32 bytes: BFT block hash
- 8 bytes: PoS height as `u64` little-endian
- 4 bytes: `round | 0x80000000` as `u32` little-endian

Reinflating a vote is two steps. `get_vote_template` builds a 76-byte vote whose leading 32-byte finalizer pubkey field is left **zeroed**, copying the stored 44 bytes into `[32..76]` (`librustzcash/zcash_primitives/src/bft.rs:729-733`). `inflate` then clones that template once per signature and fills in `vote.validator_address = s.pub_key` (`librustzcash/zcash_primitives/src/bft.rs:735-745`). The vote builder uses the matching layout: 32-byte finalizer pubkey, 32-byte value ID, 8-byte height little-endian, 4 trailing bytes for round plus precommit flag (`make_vote_sign_datas`, `tenderlink/src/lib.rs:1246-1254`).

Tenderlink does not put the whole roster into the fat pointer. It keeps only the precommit slot of signatures that are non-`NIL` and match the decided proposal: `if *value_id == round_data.proposal_id && *commit_signature != TMSig::NIL` (`tenderlink/src/lib.rs:177-203`, filter at `:189-201`).

The threshold is stake-weighted `2f+1`, where `f_from_n(n) = (n - 1) / 3` is called with `total_active_stake` (`tenderlink/src/lib.rs:638-640`, called at `:918`; `big_threshold = 2*f+1` at `:925`; decide rule at `:1058-1077`). Note the degenerate case: when `f == 0` both thresholds collapse to `total_active_stake`, so a small roster requires unanimity (`tenderlink/src/lib.rs:921-923`).

In practice a fat pointer's signer list is a quorum certificate covering at least roughly two thirds of active stake, not a roster dump. A finalizer missing from one certificate is therefore not evidence that it missed a vote.

## Bond privacy

The bond format is per-bond, not aggregate. `StakingActionRequest::CreateNewDelegationBond` and the corresponding action carry one `amount_zats`, one `unique_pubkey` bond key, and one `target_finalizer`, alongside a `challenge` and `signature` that the wallet currently zero-fills (`librustzcash/zcash_primitives/src/transaction/mod.rs:1445-1452`).

The wallet's `stake_orchard_to_finalizer` builds exactly one such action per call, with a freshly generated random `unique_pubkey` (`zebra-crosslink/wallet/src/lib.rs:1966-1993`, key generation at `:1976-1987`). Wallet and GUI state track bonded positions as separate `(bond_key, target_finalizer, amount)` tuples (`zebra-crosslink/wallet/src/lib.rs:972-973`, `zebra-gui/src/ui.rs:1579-1580`, derivation loop at `zebra-crosslink/wallet/src/lib.rs:4253-4295`).

Keeping each Orchard-side bond as its own position is deliberate. Consolidating several bonds into a single on-chain restake would link those positions and weaken privacy, so the cost of repeated per-bond restaking inside a 70-block window is a tooling gap rather than a hidden alternative encoding in the bond format.
