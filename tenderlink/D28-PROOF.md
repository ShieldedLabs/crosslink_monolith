# D28 baseline: proof boundary and lossy-STP termination

## Status

The executable reference is `src/d28.rs` and `src/d28/`. It follows D28's
one-height rules, with an explicit extension for trusted configuration changes
at higher heights. It is not yet connected to Zebra's runtime or the shared STP
bridge. The tests exercise the Rust engine; they are not a formal refinement
proof, a transport memory audit, or a termination proof for arbitrary STP loss.

The reference specification is
`/home/sam/dev/tendermint_reduced_proof/designs/D28-weightedpacketjump/design.md`
and its companion `proof.md` (source line references below were checked on
2026-11-20). The original termination argument assumes every correct-to-correct
application SEND after stabilization is delivered and processed within `D_m`;
it expressly forbids such SEND drops (`proof.md:930–944`). STP itself does not
provide that contract.

## Safety and configuration changes

At a fixed height, the source agreement argument uses strict weighted quorum
intersection, non-equivocation, and locking; it does not require timely delivery
(`proof.md:32–176`). Dropping a message must not authorize a vote or decision.
The implementation verifies signatures and complete groups before their use,
binds signatures to the configuration hash/domain and height, and retains only
bounded current signing contents rather than a history of votes.

A composition argument across changing rosters requires the following host
contract, not merely a successful call to `Engine::init`:

1. There is exactly one agreed immutable configuration C_h for each height h,
   including ordered public keys, weights, and signing domain. Byzantine stake
   is strictly below one third in each authorized configuration. Signatures
   cannot be forged, and correct signing authority cannot be rolled back.
2. The unique authorized chain prefix determines the next configuration AND
   its parent/application state. Proposal validity enforces extension of that
   prefix. A skipped height is crossed using authenticated chain/certificate
   verification, not an untrusted announcement of a newer roster.
3. Every height transition retires signing at all earlier heights. Recovery
   restores the complete previously committed authority or the complete newer
   authority, never a mixture. The physical fixed-block adapter must implement
   this contract before deployment.

Conditional composition proof: assume an agreed genesis/checkpoint prefix.
Inductively, an agreed prefix authorizes a unique C_h and parent state. Apply
fixed-height agreement under C_h; host validity requires the decided value to
extend that parent. Therefore the extended prefix and its authorized successor
configuration are unique. Repeat at the next height. No adjacent-roster overlap
condition is needed for this argument, but the authorization/validity premises
are essential. The Rust core does not itself implement this host induction.

Historical verification also needs enduring protection against retired-key
forgery: for example an appropriate key-trust assumption, forward-secure key
retirement, or trusted checkpoints. A contemporaneous stake-fault bound alone
is not a defense against later fabrication of old certificates.

## Why "blackouts are not permanent" is insufficient

A schedule can drop all proposal-bearing messages during each proposal's finite
round lifetime, while delivering ENTER, nil prevotes and nil precommits. Correct
nodes can communicate and advance rounds forever without deciding. Every link
can have infinitely many successful deliveries. Even fair loss for a message
repeated infinitely often does not protect proposals abandoned after finitely
many attempts. Timing and service of useful evidence matter, not just link
activity or elapsed time since a blackout.

## A stronger sufficient assumption with a conditional termination proof

A finite clean-service window gives a precise fallback. This is NOT claimed to
be the weakest realistic assumption, and its existence is NOT guaranteed by
STP or by the simulation tests.

Choose a time s when every correct participant is online at the same height h
and configuration, its start delay has expired, and no higher INIT will occur
during the window. Keep the source assumptions for local execution, clocks,
validity, signing/recovery, proposal acquisition and fault weight. In particular,
this fallback retains the source's idealized NB/Z/ZG local-execution premises;
it is not a new proof for arbitrary real execution delays.

Let m be the maximum correct round at s. Use the source's timeout functions
TP, TPV, TPC, with TPC(-1)=0, and choose the least K such that for all r >= K:

    TP(r)  > 2 D_m + d + TPC(r-1)
    TPV(r) > 4 D_m + d
    TPC(r) > TPV(r) + 2 D_m + d

Define, exactly as in source O5 (`proof.md:439–475`):

    R = max(m, K-1) + 2N
    W(r) = TP(r) + TPV(r) + TPC(r) + 2 rho + 3 D_m
    b = rho + D_m + TPC(m-1) + sum[r=m..R] W(r)
    L = b + rho + D_m + d

Require source NET's timely service for EVERY correct-to-correct application
SEND issued during [s,s+L]: each is admitted and processed by its send time plus
D_m (even if that deadline extends beyond the window). The proof below only
needs receipts by s+L. This is a traffic-wide service condition, not an axiom
that selects messages according to whether a decision eventually occurs. The
contract includes admission, verification and dispatch, not merely STP ACKs.
Individual transmissions outside this window may be dropped without limit;
there is no requirement for permanently loss-free communication afterwards.

Conditional proof: re-anchor the source one-height argument at s. Its state
premise permits arbitrary reachable locks/rounds and restored timers. O5 gives
a first decision no later than s+b, or already gives all decisions by then.
L8' then supplies all remaining correct decisions within rho+D_m+d of that
first decision. Thus all correct participants decide by s+L. Only service up
to completion is used. If someone decided before s, retained DEC replay and
L8' give the shorter adoption bound directly. This is a finite-prefix use of
the existing source proof, not a separate verification of that proof or of its
refinement by the Rust implementation.

For the Rust finite-integer implementation, all rounds/durations/arithmetic
needed by the argument must fit its checked types. Exhaustion fails closed;
no unbounded mathematical liveness theorem is asserted past that limit.

## What remains to prove under weaker repeated-send service

The desired weaker model should permit some whole application messages to be
dropped even during useful service opportunities. A bounded-success axiom for
repeatedly offered evidence is a candidate, but a substitution of
`D_m := rho + delivery_delay` is not a proof.

The missing bridge-and-evidence handover lemma must show that ENTER/TIMEOUT/JUMP
bridges, proposal authorization, quorum rows and nonnil precommit bundles reach
receivers before the required state is discarded, OR that a specified successor
frame discharges the same obligation. It must cover dropped one-shot entry
replies (rule E) and quorum handover snapshots (35'), as well as rebuilt replay
attachments and changes of round. Relevant source dependencies include PK(3),
L10, L11, A2's early-exit bridge and A5.0 (`proof.md:255,308–436,532–542,596–607`).
The present investigation has not established this lemma or a minimal realistic
loss axiom. Extending the theorem to only a timely participating stake quorum,
rather than every correct node, is also not established here.

After a decision, repeatedly retained complete DEC packages permit a weaker
catch-up service premise. Once higher INIT retires that package, historical
catch-up instead depends on the host archive; the bounded one-height core is
not an archive of earlier decisions.

## Persistence and memory are separate contracts

`AtomicBlockStore` is the agreed fixed-size atomic replacement API. Its bounded
image codec, adapter and physical durable backend are deliberately deferred.
`MemoryStore` supports deterministic logical publication/crash tests and is not
durable storage. No production signing should use it as such.

Core roles are bounded independently of the number of elapsed rounds. Actual
transport queues, receive/reassembly buffers, serialization copies and host
workspaces still require accounting in the shared bridge integration. Memory
saturation can cause drops without compromising agreement, but termination
requires service of useful correct traffic; bounded capacity alone does not
prove freedom from starvation. Aggregate replay work is also not bounded by
the source's memory bound (`proof.md:994–1014`).
