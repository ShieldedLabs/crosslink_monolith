# Proposal: decided BFT blocks with their proposal signatures in the test format

Status: proposal. Written for #241 (chain prefix export/import); meant to be implemented
by whoever owns `test_format.rs`, in a way that also serves ordinary tests.

## The gap

A decided BFT height is stored as three rows (`zebra_db/bft.rs`): the block, the fat
pointer to it, and the proposer's per-chunk proposal signatures (`proposal_sigs`).
`.zeccltf` can carry only the first two: `LOAD_POS` is a `BftBlockAndFatPointerToIt`, and
its handler force-feeds the block with `Vec::new()` for the signatures
(`new_network/bft.rs`, `BftRequest::ForceFeed` → `decide(.., Vec::new(), ..)`).

So a node rebuilt from a file is not the node the file was taken from:

- `restore()` turns each stored decision into tenderlink `RoundData` via
  `decided_round_data`, including `proposal_sigs`.
- Tenderlink uses those to re-serve the decided proposal's chunks to a peer still on that
  height (`tenderlink/src/lib.rs`, `write_signed_proposal_chunk` and the
  `proposal_sigs_n > 0` send loop) and to judge incoming chunks against a decided value
  (`is_prev_seen_round && proposal_sigs_n > 0`).
- With no signatures, the rebuilt node cannot help a lagging finalizer through that height,
  and takes the `proposal_sigs_n == 0` branches where the original took the others.

Nothing else differs. Validation, storage, rosters, staking and `fin` never read
`proposal_sigs`. That is why the chain prefix work does not block on this, but a replay
meant to reproduce network behaviour ecologically needs it.

## A related gap already in the database

The signed bytes of a proposal chunk include `valid_round` (`PacketProposalChunkHeader`).
The database does not store it, and `decided_round_data` leaves
`round_data.proposal_valid_round` at its `-1` default. For a decision whose proposal was
re-proposed (`valid_round != -1`), the restored signatures fail tenderlink's own debug
self-check in `write_signed_proposal_chunk`, which returns `None`. In release builds the
node sends chunks that its peers reject. This happens on a plain restart, before any test
file is involved.

A new instruction should therefore carry `valid_round` too. Storing it in the database
(next to `proposal_sigs`) and restoring it in `decided_round_data` is the matching fix
there. That needs a DB format bump, so it can land separately, but the instruction should
not repeat the omission.

## Requirements

1. **Lossless decided height.** One instruction carries everything `StoredDecision` holds:
   the block, its fat pointer, and `proposal_sigs` in chunk order. It also carries
   `valid_round` (see above). The round is already in the fat pointer's vote template.
2. **Same admission path as `LOAD_POS`.** It goes through `force_feed_bft_block`, which
   validates the block as a proposal and then calls `decide`. The only difference is that
   the signatures reach `decide`, and through `finish_decision` and `write_bft_decision`
   they land in the database. A later `restore()` then rebuilds identical `RoundData`.
3. **Signatures are optional and may be empty.** Decisions made before signatures were
   stored, and hand-built test blocks, have none. An empty list must behave exactly like
   today's `LOAD_POS`.
4. **Old files still read.** Existing `LOAD_POS` files and the stored `.zeccltf` fixtures
   keep their meaning. Whether this is a new kind (`LOAD_POS_DECIDED`, say) or an optional
   trailing payload on `LOAD_POS` is the implementer's call. A new kind is clearer for
   `string_from_instr`, the fuzzer and old readers, which then fail loudly rather than
   silently drop data.
5. **Optional verification, off by default when replaying an export.** For general testing
   it is useful to be able to flag a load with bad signatures as `SHOULD_FAIL`. That means
   checking each chunk signature against the proposer for (height, round). The proposer comes
   from the roster at that height (`TMState::proposer_from_height_round`). The namespace is
   `namespace_for_bft_height`. An export of a real chain carries the real signatures, so
   verification should pass there too. But the replay must not depend on the verifier
   to admit a block the fat pointer already certifies.
6. **Bounded parsing.** The signature count is derived from the block's serialized size
   (`chunks_n` = `ceil(len / PROPOSAL_CHUNK_DATA_SIZE)`) and checked against the payload.
   It is never trusted as an allocation size, since fuzzed files reach this parser.
7. **Shown everywhere `LOAD_POS` is.** That covers `str_from_kind`, `string_from_instr`, the
   viz file viewer (`VizScene::from_tf`), the GUI's "Load into zebra", and the fuzzer's
   instruction tables (`TFInstr::COUNT` const-asserts in `test_format.rs`).

## What the chain prefix exporter needs from it

The exporter has one call site, `push_decision` in the chain prefix TF builder. Today it
writes `LOAD_POS`:

```rust
tf.push_instr_load_pos(&BftBlockAndFatPointerToItWrap(BftBlockAndFatPointerToIt {
    block: decision.block.clone(),
    fat_ptr: decision.fat_pointer.clone(),
}), 0);
```

It needs a pusher that also takes `&[TMSig]`, plus `valid_round` once that is stored,
with replay semantics identical to `LOAD_POS` otherwise. For example:

```rust
tf.push_instr_load_pos_decided(&block, &fat_pointer, &proposal_sigs, valid_round, flags);
```

The import side needs nothing else. Replay already goes through `read_instrs`, so the new
kind gets handled there.
