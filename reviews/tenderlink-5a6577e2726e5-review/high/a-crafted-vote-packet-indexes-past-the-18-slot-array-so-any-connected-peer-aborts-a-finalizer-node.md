# `PacketVotes::read_from` indexes past its 18-slot array on an attacker-chosen vote count, so any connected peer aborts a finalizer node with one packet

**Severity**: High
**Validation Status**: Confirmed
**Location**: `tenderlink/src/lib.rs:3053-3067` (`PacketVotes::read_from`, the `for i in 0..(no_votes_n + yes_votes_n)` loop writing `packet.votes[i]`), `tenderlink/src/lib.rs:3031` (`votes: [PubKeySig; 18]`, the fixed 18-slot array), `tenderlink/src/lib.rs:2501-2513` (receive loop calls `read_from` then re-reads the two counts, before any signature or roster check), `tenderlink/src/lib.rs:1564-1572` (`make_vote_sign_datas`, the `round + 0x8000_0000 * is_precommit` add on an unverified `round`)
**Found by agent:** raised as an unconfirmed read-only lead in the first review (item 2a of the brief); validated by the second adversarial review subagent, 2026-10-12, at `dev` 5a6577e2726e5
**In scope of audit?** Yes. The two commits under review (6705b3ca, 5a6577e2) touch only the proposal-chunk receive path and the vote tallies; they do not touch `PacketVotes`, its reader, or the vote branch of the receive loop. The defect predates both commits and is present on the deployed engine. The release profile that builds `zebrad` (`zebra-crosslink/Cargo.toml:280-283`, `panic = "abort"`) is affected.

## Description

A `PACKET_TYPE_PREVOTE_SIGNATURES` or `PACKET_TYPE_PRECOMMIT_SIGNATURES` datagram carries a `PacketVotes` whose first two bytes are `no_votes_n` and `yes_votes_n`, each a `u8`. The receive loop at `tenderlink/src/lib.rs:2501` calls `PacketVotes::read_from`, which loops `for i in 0..(no_votes_n + yes_votes_n)` and writes `packet.votes[i]` into a fixed array of 18 elements (`tenderlink/src/lib.rs:3031`). Nothing bounds the two counts against 18 before the loop, and nothing verifies the packet first: the roster-membership check and the per-vote signature check both happen later, inside `check_and_incorporate_msg`, which is only reached after `read_from` returns.

The code assumes a peer sends at most 18 votes in a packet, because the sender in `send_round_data_to_peer` flushes the packet whenever `no_votes_n + yes_votes_n` reaches `votes.len()` (`tenderlink/src/lib.rs:2110`). That is a property of the honest sender, not of the bytes on the wire. A peer that writes any `no_votes_n + yes_votes_n` in the range 19 to 255 makes the loop index `packet.votes[18]` and beyond.

Two distinct arithmetic hazards sit on these unverified fields, and they bite in different build profiles:

1. **The array index is out of bounds for any sum from 19 to 255.** Rust's array bounds check is always present, in every profile. This is the serious one: it fires in the shipping release build.
2. **`no_votes_n + yes_votes_n` is a `u8` add that overflows for sums above 255**, and `make_vote_sign_datas` computes `round + 0x8000_0000` on an unverified `u32 round` (`tenderlink/src/lib.rs:2508` passes `packet.round`). Both overflow only when `overflow-checks` is on, which is the `tenderlink` dev profile and the test build, not the release profile that builds `zebrad` (`overflow-checks` defaults off in release).

**This is a remotely triggerable crash, not a safety or key-compromise bug: the node aborts and can restart, and no wrong value is ever accepted.** What bounds it is that the attacker must first complete the STP/Noise handshake to be a connected peer; what does not bound it is roster membership or any valid signature, neither of which is checked before the panic.

## Attack Scenario and Steps

An adversary that can open an encrypted STP connection to a finalizer (the listener's static key is public configuration) triggers this.

1. The adversary completes the STP/Noise handshake with the target finalizer. A `Peer` entry now exists for the connection (`tenderlink/src/lib.rs:2402`). No ID-Hello verification or roster membership is required for the vote branch.
2. The adversary sends one datagram whose header type is `PACKET_TYPE_PREVOTE_SIGNATURES`, with body `no_votes_n = 10`, `yes_votes_n = 10` (sum 20, no `u8` overflow), followed by enough bytes for 20 vote entries.
3. The receive loop reaches the vote branch (`tenderlink/src/lib.rs:2501`) and calls `PacketVotes::read_from`.
4. `read_from` loops to `i = 18` and executes `packet.votes[18].roster_i = ...` (`tenderlink/src/lib.rs:3063`). The array has 18 elements. The index is out of bounds, the thread panics, and with `panic = "abort"` the whole `zebrad` process aborts. The failure is observable as an immediate process exit; nothing is logged about the sender being at fault, because the panic is before the signature check.

**Attack Requirements and Assumptions:**

- Any party that can establish an STP/Noise connection to the node. The listener static key is public, so this is not gated on being a configured peer in the general case; it is certainly open to any configured peer.
- No valid signature, no roster membership, and no correct height or round are needed: the panic is upstream of every such check.
- Repeatable at will, so one adversary can hold a finalizer down continuously, and several finalizers down at once.

## Impact on Users

Node operators and, through them, the whole finalizer set bear the loss.

- **A single malformed packet aborts the process.** With `panic = "abort"` in the release profile, there is no unwinding and no recovery within the process; the node exits.
- **Consensus can be stalled.** An adversary that can reach enough finalizers to drop the active set below a quorum keeps BFT from deciding for as long as the attack runs, since each targeted node dies on the first such packet after each restart.

**Detectability, stated precisely.** An operator sees the process exit (and any supervisor restart loop); the crash site names `src/lib.rs` line 3063, an index-out-of-bounds. There is no application-level log naming the sending peer, because the panic precedes the fault-attribution code, so from logs alone the operator cannot tell which connected peer sent the packet.

## Technical Details / Code Analysis

**The unbounded loop over attacker counts, writing a fixed array** (`tenderlink/src/lib.rs:3053-3067`):

```rust
pub fn read_from(buf: &mut &[u8]) -> Option<Self> {
    let mut packet = PacketVotes {
        no_votes_n:   u8::read_from(buf)?,
        yes_votes_n:  u8::read_from(buf)?,
        round:       u32::read_from(buf)?,
        height:      u64::read_from(buf)?,
        value_id:    ValueId(SliceRead::read_from(buf)?),
        ..Default::default()
    };
    for i in 0..(packet.no_votes_n + packet.yes_votes_n) as usize {
        packet.votes[i].roster_i = u16::read_from(buf)?;
        packet.votes[i].sig.0    = SliceRead::read_from(buf)?;
    }
    Some(packet)
}
```

`packet.votes` is `[PubKeySig; 18]` (`tenderlink/src/lib.rs:3031`). For any `no_votes_n + yes_votes_n` in 19..=255 the write at `i = 18` is out of bounds.

**Nothing is verified before the loop runs** (`tenderlink/src/lib.rs:2500-2513`):

```rust
else if packet_type == PACKET_TYPE_PREVOTE_SIGNATURES || packet_type == PACKET_TYPE_PRECOMMIT_SIGNATURES {
    if let Some(packet) = PacketVotes::read_from(&mut &msg[read_o..]) {
        ...
        for vote_i in 0..(packet.no_votes_n + packet.yes_votes_n) as usize {
            if let Some(roster_member) = roster.get(packet.votes[vote_i].roster_i as usize) {
                let sign_datas = make_vote_sign_datas(roster_member.pub_key, is_precommit != 0, packet.height, packet.round, packet.value_id);
                ...
```

`read_from` is called first, so the panic is inside it. The later `roster.get(...)` is a safe lookup, and `check_and_incorporate_msg` (which does the signature check) is reached only after. `make_vote_sign_datas` computes `round + 0x8000_0000 * is_precommit` on `packet.round` (`tenderlink/src/lib.rs:1568`), a second hazard that overflows only under `overflow-checks`.

## Recommendations

1. **Reject the packet when `no_votes_n as usize + yes_votes_n as usize > votes.len()` before the loop.** In `PacketVotes::read_from`, read the two counts, widen each to `usize`, sum, and `return None` if the sum exceeds `self.votes.len()` (18). This turns a crash into a dropped packet, matching how the proposal-chunk path now rejects an oversized header (`tenderlink/src/lib.rs:2480`, added by 5a6577e2). Mirror the same `usize` widening in the receive loop's own `for vote_i in 0..(no_votes_n + yes_votes_n)` so it cannot overflow either.
2. **Compute the signing round without overflow.** In `make_vote_sign_datas` use `round | 0x8000_0000` for the precommit case rather than `round + 0x8000_0000`, matching the canonical encoding in `librustzcash/zcash_primitives/src/bft.rs:1199-1203` (`merged_round_val | 0x8000_0000`). The add and the or agree when the round's top bit is clear, and the or cannot overflow; this removes the dev/test-only panic and makes the field robust if `overflow-checks` is ever enabled in a shipping profile.
3. **Alternatives considered.** Capping the loop at `votes.len()` and silently ignoring extra entries was rejected: a well-formed packet never exceeds 18, so any excess is a malformed or hostile packet and should be dropped whole, not partially accepted. Raising `votes` to a `Vec` was rejected: the fixed 18-slot array is a deliberate bound on per-packet work, and a `Vec` sized from the wire would reintroduce an allocation-from-untrusted-length hazard of the kind 5a6577e2 just closed for proposal chunks.
4. **Tests.** Add a unit test that feeds `PacketVotes::read_from` a buffer with `no_votes_n = 10, yes_votes_n = 10` and asserts it returns `None` rather than panicking; a second with `no_votes_n = 255, yes_votes_n = 255` for the overflow path. A standalone reproduction of the current reader (struct and `read_from` copied verbatim) was built and run during validation and panics at the `votes[18]` write in both an optimized build with overflow checks off and a build with overflow checks on.
5. **Rollout.** No consensus or validity rule changes: this only hardens packet parsing. It should land before any public exposure of a finalizer's STP listener. A deployed finalizer branch wants the backport, since the crash is reachable by any peer that can connect.

## Validation Information

**Verdict: CONFIRMED. Severity: High.**

Confirmed by reading the engine at `dev` 5a6577e2726e5, and by building and running a standalone reproduction of `PacketVotes::read_from` (the struct and reader copied verbatim) outside the crate; the live `tenderlink` crate's own test suite was also run in place (27 passed, 1 known-unrelated failure).

| Claim | Verified at |
|-|-|
| `votes` is a fixed 18-element array | `tenderlink/src/lib.rs:3031` |
| `read_from` loops over `no_votes_n + yes_votes_n` and writes `votes[i]` | `tenderlink/src/lib.rs:3062-3064` |
| A sum of 20 (no `u8` overflow) indexes `votes[18]` and panics in release and debug | standalone reproduction, run; `-O` (overflow-checks off) and `-C overflow-checks=on` both abort at the `votes[18]` write |
| `read_from` is called before any signature or roster check | `tenderlink/src/lib.rs:2501`, then `make_vote_sign_datas`/`check_and_incorporate_msg` at `:2508-2511` |
| The vote branch needs only a connected peer, not roster membership | `tenderlink/src/lib.rs:2446-2513`; a `Peer` entry exists for every connection, `:2402` |
| Release profile that builds `zebrad` aborts on panic | `zebra-crosslink/Cargo.toml:280-283` (`panic = "abort"`) |
| `make_vote_sign_datas` adds `round + 0x8000_0000`; overflow only under overflow-checks | `tenderlink/src/lib.rs:1568`; release profile does not set `overflow-checks` (inferred from `zebra-crosslink/Cargo.toml:201-283`; not run against a release `zebrad`) |
| The defect is not introduced by 6705b3ca or 5a6577e2 | `git diff e78c6ca6 5a6577e2` does not touch `PacketVotes`, `read_from`, or the vote receive branch |

**Severity justification.**

*Why not Critical:* it is a crash and denial of service, not a consensus-safety break or a key compromise; the node can be restarted, and no incorrect value is accepted. The attacker must first complete the STP/Noise handshake.

*Why not Medium:* it aborts the entire process in the shipping release profile (`panic = "abort"`), is reachable by any connected peer with a single packet and no valid signature, and is repeatable, so one adversary can keep a finalizer, or several finalizers, offline for the duration of the attack and thereby stall consensus.

**Corrections made during validation.**

1. The lead bundled two mechanisms. The `round + 0x8000_0000` and `u8`-sum overflows panic only under `overflow-checks` (dev and test), so they do not crash the release `zebrad`. The out-of-bounds array write, by contrast, panics in every profile because the array bounds check is unconditional; that is the mechanism that reaches the release node, and a sum as small as 19 is enough without any overflow.

**What this issue does not claim.** It does not claim any value is accepted, any signature is forged, or the chain forks. It does not claim the `round + 0x8000_0000` overflow crashes the release node; that part is refuted for release and holds only under overflow-checks. It does not claim the attacker need not connect: an STP/Noise handshake is required.

**Cross-references.** Independent of all other findings in this review. It can land on its own and should land before K6, since a node that is being crashed cannot exhibit any consensus behaviour.

DO NOT DEVIATE FROM THIS TEMPLATE (`reviews/SECURITY-ISSUE-TEMPLATE.md`).
LEAVE THIS MESSAGE IN PLACE SO THAT ALL AGENTS KNOW NOT TO DEVIATE FROM THIS TEMPLATE (`reviews/SECURITY-ISSUE-TEMPLATE.md`).
