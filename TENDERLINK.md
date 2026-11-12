# Tenderlink and STP: open work

Everything outstanding on the BFT engine (`tenderlink/src/lib.rs`) and its transport
(`tenderlink/src/stp.rs`). Code is named by symbol, because line numbers drift.

| # | topic | state |
|---|---|---|
| 1 | Out-of-range proposal chunk index crashes the receiver | fixed (`5a6577e2`) |
| 2 | Gossip by HAVE bitmaps under a per-peer byte budget, replacing the resend storm | committed; fuzz tests pass; not measured on a network |
| 3 | Receive-side overload and backpressure | hard cap and duplicate skip shipped; the rest waits on a measurement |
| 4 | Line 28 checks the cited round | fixed and deployed (`b150bec0`); expert review not recorded |

## 1. An out-of-range proposal chunk index crashes the receiver

A chunk whose `chunk_i` lay past the end of its proposal underflowed a subtraction on receive and
indexed past `proposal_sigs` in `check_and_incorporate_msg`; either could kill a node. Both places
now refuse a chunk unless `chunk_i < proposal_size.div_ceil(PROPOSAL_CHUNK_DATA_SIZE)`.

## 2. Gossip

### The problem it replaces

The old sender re-sent every round at the current height to every peer on every tick, with no
memory of what it had sent and no rate limit. Rounds at a height are pruned only on commit, and the
round timeout backs off linearly, so a height stalled for time `t` holds about `√t` rounds: roughly
44 after an hour, 235 after a day, 630 after a week. In the September 2026 featurenet stall this
measured about 12.3 MB/s of inbound BFT traffic per node (8,864 packets/s) against 0.1 MB/s of
block sync, and with an unbounded receive queue it killed nodes repeatedly. It also dropped votes:
a vote packet carried one value, so a vote for anything but the proposal a node held was counted
and never passed on.

### What Tendermint needs from it

The proofs in the Tendermint paper (Buchman, Kwon, Milosevic, *The latest gossip on BFT
consensus*) assume its Gossip property: a message a correct process sends or receives by time `t`
reaches every correct process by `max(t, GST) + Δ`. So gossip may drop nothing a node holds for its
height for good: not a vote for another value, not a faulty finalizer's second vote. It may only
delay, and only by a bounded amount once the network is synchronous. Two things narrow what it must
carry:

- Once a height is decided, its proposal and 2f+1 precommits are all a node still at that height
  needs (line 49), so a node ahead serves a node behind only those.
- Line 28 reads round `vr`, so the prevotes of the round a proposal cites must reach everyone; the
  repair rotation below carries them, and they are also near the front of the priority order.

Lemma 5 needs each timeout to exceed a small multiple of `Δ`, which timeouts that grow with the
round eventually do; this change leaves them as they are. The budget adds to `Δ` at most the time
one tick's budget takes to drain to a peer, per hop: 0.16 s at 1 Mbit/s.

### How it works

All of it is in the gossip section of `tenderlink/src/lib.rs`, from `GOSSIP_BYTES_PER_PEER_TICK` to
`take_consensus_packet`.

- **HAVE** (packet type 10) says which chunks and votes a node has validated and stored, round by
  round, for its height: the proposal id, a chunk bitmap, and a bitmap of finalizers per step for
  nil and for each value voted. A node sends each peer a HAVE in a tick when a round's holdings
  changed since it last told that peer, and every `HAVE_REFRESH_TICKS` (4) ticks a rotation over
  all its rounds, as many as fit one datagram, in case one was lost. A HAVE carries a sequence
  number taken from the clock, and a peer drops one no newer than the last it took.
- **What is sent.** Each tick, after the HAVE, a node sends a peer every chunk and vote its newest
  HAVE does not list and that was not sent to it, or received from it, in the last retry interval.
  The interval is `GOSSIP_RETRY_MIN_TICKS` (4) ticks per round and doubles, up to
  `GOSSIP_RETRY_MAX_TICKS` (16), while the peer reports nothing new. Votes for any value go,
  including both values of a finalizer that signed two.
- **The budget.** A peer gets at most `GOSSIP_BYTES_PER_PEER_TICK` (20 KB) a tick, less the bytes
  the transport still has queued for it on the BFT channel, less its HAVE. A slow peer therefore gets
  less, and what waits for it never grows much past one tick's budget. Three quarters go in priority
  order and the rest, plus whatever that left, to a rotation over every round of the height.
- **Priority:** precommits of rounds holding precommits for a value, newest first; precommits then
  prevotes of our round and of the peer's; prevotes of the rounds their proposals cite; chunks of
  those two rounds; chunks of rounds holding value precommits. The pass stops at the first step that
  does not fit, so nothing later overtakes it.
- **A peer at another height.** One behind gets the precommits and chunks of the round that decided
  its height, from `recent_commit_round_cache`. One ahead gets only HAVEs, from which it catches this
  node up. A peer outside the active roster at our height gets only HAVEs, as before.
- **Packets.** A vote packet holds at most `VOTES_PER_PACKET` (16) votes, so it fits one datagram,
  and votes for one value plus nil votes. Every consensus packet keeps its status header, with the
  need-ranges zeroed, since peers read heights and rounds from it. A peer that sends no HAVE within
  `GOSSIP_FIRST_HAVE_WAIT_TICKS` (8) ticks is served from its status as if it held nothing.

`tenderlink::status_packet_needing_everything`, which the test harness sends its synthetic peers,
is now a HAVE listing no rounds.

### Against the requirements of 2026-09-23

1. **A hard byte budget per peer per tick:** yes, less what the transport has queued. HAVEs, at most
   one datagram per peer per tick, come on top.
2. **Every old round stays reachable:** the rotation covers every round on at least a quarter of the
   budget, and rounds holding value precommits come early in the priority order.
3. **Decisive votes and status before chunks and repair:** the priority order.
4. **Back off per item:** HAVE bitmaps and the sent marks are per item. The retry timer is per peer
   and round.
5. **Need-ranges are hints:** nothing reads them, and the statuses nodes send now carry none.
6. **A transport ACK is not application receipt:** nothing uses ACKs.

### Tests

- `gossip_have_rounds_read_back_as_written` fuzzes the HAVE encoding, its truncation, and the
  reader against cut, bent and random bytes.
- `gossip_reaches_every_node_within_budget_and_then_goes_quiet` runs real engines over simulated
  links that queue at a byte rate, delay, and lose packets before GST, over partial meshes, slow
  links, and a finalizer voting for two values in every step. It asserts that a precommit made at
  GST reaches every node within a bound of a few ticks plus queue drain plus latency per hop, that
  every node ends up holding everything any node held, that no tick's data to a peer exceeds its
  budget less what is queued to it, and that once all hold everything only HAVEs are sent.
- `gossip_brings_a_node_behind_the_decision_and_then_goes_quiet` does the same for a node a height
  behind.
- `gossip_lets_the_state_machine_decide_heights` runs the real state machine (`bft_update`) on
  each node over the same links, its step timeouts on the sim's ticks, with finalizers that never
  run, partial meshes, slow links, heavy loss before GST, or a partition before GST with no quorum
  on either side. It asserts that every finalizer that runs decides three more heights within
  `LIVE_TICKS_PER_HEIGHT` ticks each after GST, that all decide the same value at every height,
  and the same budget.

`CROSSLINK_FUZZ_SEED` replays a run and `CROSSLINK_FUZZ_CASES` sets its size (default 8). Run them
with `phest.bat tenderlink Debug Win64 gossip_`. For the whole suite, add `-- --skip single_rt`:
that test never ends, and its nodes all bind port 23485, so they crash the test process.

### Known limits

- A finalizer that signs three or more values in one step: a node keeps two of them, the newest
  replacing the other, so nodes can hold different pairs and resend each other the missing one
  every retry interval. That traffic is bounded and never settles; the votes prove a fault anyway.
- A round's HAVE entry fits about ten values at 256 finalizers. Votes for the values left out look
  missing to peers, which resend them every retry interval.
- A node holding one proposal for a round is never sent another proposal's chunks for it. A decided
  one reaches it once its precommits make the node take that proposal (`adopt_if_certified`); a
  proposal with a quorum of prevotes comes back re-proposed in a later round, which line 28 accepts.
- Gossip tracks at most `VOTE_BITS_MAX` (256) active finalizers; past that it prints an error and
  the rest's votes are not gossiped.
- Receive-side CPU is still unbounded (section 3).
- Rounds at a stalled height still accumulate (`prune_unnecessary_data` is `todo!()`). The budget
  holds regardless, but the HAVE rotation takes longer to cover them.
- Nothing has been measured on a network.

### Still to do

- Measure on a stalled height: bytes per second per peer and the `receive loop processed …
  messages/s` line the node prints every 10 s. `PRINT_SEND_CS` reports per-peer sends each tick.

## 3. Receive-side overload and backpressure

### What is in the tree

- **A hard cap.** `MAX_RECV_QUEUE_BYTES` (64 MiB) bounds the application-facing receive queue. Past
  it messages are dropped and counted, and the node prints the queue's peak and drop counts
  (`45ac515f`). The cap drops *after* STP has ACKed, so the sender never learns it is overrunning
  the receiver.
- **Exact duplicates skip the signature check** (`25b12d36`). A vote or chunk byte-identical to one
  already verified and stored costs almost nothing. During a stall almost everything inbound is a
  duplicate, so this may be most of the fix by itself; that is unmeasured.

### Decided

1. Transport receipt is not application acceptance. Do not delay an ACK until the application has
   consumed the message: the ACK bit is set before fragments are parsed, one datagram carries
   several messages, the ACK bitmap is also the replay check, there is no durable per-message
   handle, and an ACK later than the loss timeout is ignored, so under load the sender would never
   see "acked" and would keep resending. The mechanism fails exactly when it is needed.
2. **Withholding ACKs does not stop the sender.** STP has no retransmission, and its loss loop
   retires in-flight packets without an ACK, so the send window reopens by itself. A peer that sees
   no ACK for 15 seconds drops the connection.
3. The order of work is: duplicate skip (done), the scheduler of section 2, and only then, **if a
   mixed-version measurement still shows overload**, admission drops before the ACK. Old senders
   never get the new scheduler, so pre-ACK drops are the only step that feeds congestion back to
   them, as loss their CUBIC already understands, with no peer cooperation and no wire change.
4. Pre-ACK admission drops are deliberate loss, not flow control. They report application overload
   as path congestion and can slow fresh votes along with duplicates. If built:
   - a fair share per connection, with a minimum accepted rate so ACKs and RTT samples keep flowing;
   - deliberate drops reported to the application and counted apart from path loss, on both sides;
   - the 64 MiB cap stays as the backstop for a peer that does not cooperate.
5. Packet type 10 is now the `HAVE` of section 2, sent to every peer without negotiation, and it
   means validated and incorporated. `WANT` and receive credit are not built. Do not change
   `MAGIC2_APP_CROSSLINK` or renumber the vote types. The magic2 negotiation is a list, so offering
   `[V2, V1]` is the backwards-compatible route for a future STP-level change.
6. Later and optional: STP reports per-message ACK and loss to the sender as a faster "maybe
   delivered" signal.
7. ECN echo on queue pressure is at most a hint, counted apart from real congestion. NewNet needs
   no new transport mechanism; it uses its existing request path.

### Constraints on anything built here

- **Pressure is global and the remedy must be per peer.** The receive queue is one queue for all
  connections. Throttling every peer because one floods punishes the honest ones, so per-peer byte
  accounting comes first.
- **The measure underestimates.** A batch leaves STP's byte count when the application takes it, so
  a per-connection measure sees only what arrives while the application is busy. Size thresholds
  knowing it can be off by about 2×.
- **Measure standing queue, not instantaneous length.** A long queue that drains is fine and a short
  one that never empties is not. The send side already reasons this way:
  `UnreliableSendBuffer::current_byte_capacity` is in microseconds of data at the current rate.
- **Throttling consensus traffic can entrench the stall that caused it.** The flood happens because
  BFT is stuck. Dropping the vote that would resolve the height turns a crash into a hang. Keep a
  floor below which a peer is never throttled, and let votes outrank proposal chunks.
- **Layering.** The network thread owns the queue and the consumer owns the drain rate. Whatever is
  measured has to be readable from the network thread without a lock on the consumer's path; the
  exchange is a `mem::swap` under an atomic state word and it is hot.
- **Judge any design against the send side.** `UnreliableSendBuffer` is already a bounded queue with
  explicit byte accounting, capacity sized to a latency target, and `cut_to_size` dropping from the
  front. STP also already has CUBIC rate control, a BDP-derived in-flight window, RTT estimation, ACK
  pacing, ECN receive and echo, app-limited detection, and path-MTU probing. The receive side never
  got the same treatment.

### A byte budget is not a CPU budget

Not fixed, and not covered by anything above. The receive loop accepts vote packets from any peer
past the Noise handshake, and checks each vote against the roster key the vote names, not against
who sent it. A vote with a bad signature is never stored, so it is verified in full every time it
arrives (one Ed25519 check, roughly 50 to 200 µs). Nothing counts the failures or disconnects the
sender. A peer sending current-height votes with junk signatures, 18 to a 1,240-byte packet, costs
about a millisecond of receive-loop time per packet; about 1,000 packets/s, roughly 1.2 MB/s,
saturates the loop. Honest traffic backs up behind it and is dropped at the cap. Nothing has to
stall for this to work, and byte accounting will not flag it.

The remedy charges failed signature checks to the sending peer and disconnects or throttles it past
a limit, or accepts votes only from identified validator peers.

### Open

- The mixed-version stall test that decides point 3. It has to cover useful evidence, multi-fragment
  messages, idle and active peers, and throttling at both ends at once.
- Whether tenderlink's STP instance and `new_network`'s want different thresholds: BFT traffic is
  small and latency-critical, block download is bulk and loss-tolerant.
- Whether the reliable and unreliable channels share one pressure signal.
- Who sets `target_queue_size_us`, `min_queue_size_bytes` and `max_queue_size_bytes`, and to what;
  the receive side should probably borrow the same tuning.

## 4. Line 28

Tendermint Algorithm 1 line 28 lets a node prevote a re-proposed value `v`, whose proposal cites an
earlier round `vr`, when **round `vr`** holds prevotes for `v` from at least `big_threshold` stake.
The code tested the current round's `yes_prevotes` instead. `b150bec0`, which is deployed, counts round `vr`'s prevotes
for the exact value, on round `vr`'s roster, whether or not `vr`'s proposal was received
(`has_prevote_certificate`), and adds eleven `line28_` tests: nine of the counting and two through
`bft_update`.

### Still owed

- Expert review of the diff. It is a change to when a validator prevotes, and a bug in it is shared
  by every upgraded node. None is recorded.
- The deployed nodes run it without the scheduler of section 2, so round `vr`'s prevotes reach a
  peer only by the old resend loop.

### What the change is and is not

- **Wire format:** unchanged.
- **Finality certificate:** unchanged. The fat pointer holds precommit signatures over
  `hash || height || round|0x8000_0000`, and `FatPointerToBftBlock::validate_signatures` accepts any
  round. The fix can change *which round* a height commits in, not what a valid certificate is.
- **Nothing checks why a peer prevoted.** Votes do not carry `vr`, and the `valid_round` check in
  `check_and_incorporate_msg` is a TODO. Old and new nodes accept the same messages.
- **What changes:** from the same evidence, a fixed node can prevote yes where an old node prevotes
  nil.
- **No double votes.** Lines 22 and 28 are mutually exclusive (`vr == -1` against `vr >= 0`), and
  `step == Propose` allows one prevote per round.
- **Not a hard fork, as far as was found.** The Crosslink verifier did not get a full upgrade audit.

Mixed networks:

- **Safety.** Under the paper's assumptions, an old node's nil vote and a new node's yes vote do not
  by themselves give a path to conflicting decisions. Restart and persistence, roster changes and
  proposal equivocation were not audited.
- **Liveness while partly upgraded.** No path was found where a mixed network does worse, but timing
  differs: a round may wait for the prevote timeout instead of precommitting nil at once.
- **When the benefit starts.** In the plain bootstrap case, once at least 2/3 of stake runs the fix.
  Byzantine or other current-round yes votes can move that.

### What the bug cost

A liveness risk, not a proof of a stuck height. The wrong-round check keeps a valid-round
re-proposal from unlocking honest validators. Where locks are incompatible and no old-round commit
certificate is already signed, later proposals can keep timing out. A fresh proposal (`vr = -1`)
helps only validators whose locks permit its value, so locks split across different values can
leave every fresh value short. Restart is not the only way out.

It was probably not the cause of the September 2026 featurenet stall, which looks like finalizers
not taking part. The evidence neither confirms nor excludes it. Logging `proposal_valid_round`,
whether the cited certificate is present, line-28 entries, timeouts and lock state would settle it.

### Why the bug looks like liveness and not safety

A bounded argument, not a proof.

- **Different value.** Once a round's proposal is known, votes for any other value are rejected, and
  votes stored before it was known are purged when the first chunk arrives. The branch also needs
  the full proposal. So when it ran, `yes_prevotes` counted only votes for that proposal's value.
- **Same value.** The wrong condition needed 2f+1 current-round prevotes for `v`. With `vr ≥ 0` no
  honest node can cast the first one: line 22 needs `vr == -1`, and line 28 needed the quorum to
  exist already. That leaves at most f Byzantine votes.
- **Equivocating proposer.** The proposer sends `vr = -1` to some nodes and `vr ≥ 0` to others.
  Honest nodes prevote `v` by line 22 only if unlocked or locked on `v`. If some `w ≠ v` was decided
  earlier, at least f+1 honest nodes are locked on `w`. That leaves at most f unlocked honest nodes
  plus f Byzantine ones: 2f, short of 2f+1.

Lines 36 and 49 are value-bound once a full valid proposal is present: the first chunk purges
other-value votes, later ones are rejected, and a second proposal value is a fault. A restart that
forgets a node's lock and signed votes breaks the locked-honest-node assumption for every Tendermint
rule, not only this one; the amnesiac-proposer flush is that known limitation.
