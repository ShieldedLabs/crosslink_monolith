# D28 baseline: proof boundary and lossy-STP termination

## Status

The executable reference is `src/d28.rs` and `src/d28/`. It follows D28's
one-height rules, with an explicit extension for trusted configuration changes
at higher heights. `src/d28/runtime.rs` connects the guided receiver-selected
engine to Zebra's existing host callbacks and shared STP bridge; `chain.rs`
preserves the chain's BFT block and fat-pointer contracts. The finite core,
codec, durability and runtime tests are not a formal refinement proof, a
whole-node memory audit, or a termination proof for arbitrary STP loss.

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
and retains only bounded current signing contents rather than a history of votes.
Reference-mode signatures bind the configuration hash/domain and height. In the
chain-compatible mode, prevote/precommit signing bytes instead preserve the existing
public-key/value/height/round and namespace contract; they do NOT additionally bind
D28's configuration hash or signer index. Enter and proposal headers remain
configuration-bound. Thus the chain mode relies on the host's unique immutable
height/namespace-authorized roster below, not a cryptographic distinction between
two different rosters carrying the same keys and namespace.

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

## Weaker sufficient assumption: mutable periodic-lane service

This section gives a conditional one-height termination proof permitting whole
application-frame drops DURING the useful service interval. It replaces the
source NET uses in PK(3), L10, L11 and A1-A6 by the replay lemmas below; it is not
an assertion that substituting a resend delay for `D_m` proves termination.
Neither STP nor the existing tests establish the service premise. No weakest
or minimal loss axiom or Rust formal refinement is claimed. A fixed actually
correct >2/3-weight cohort is covered by the explicit corollary below; arbitrary
time-varying timely quorums are not.

### Model and local-execution restrictions

Fix one agreed height h, immutable configuration C_h and roster of N members,
with Byzantine weight strictly below one third, unforgeable signatures and
correct non-equivocating signing/recovery. Keep the source's validity premises:
trimodal validity and determinate latches, the strong M9c bound d for a received
copy of a body already T at a correct holder, and M9d for a correct fresh
candidate. Specifically, after acquisition at t, a held copy is determinate by
max(t,s)+d, where s replaces the source's stabilization time T*; soundness
makes that result T when a correct holder already has T. Keep weight-blind proposer rotation, so each member is proposer once
in each N-round cycle. Timers do not depend on validity completion.

The following theorem retains source NB/Z/ZG: ready local guards are executed
without logical-time delay, each specified local multi-write/sign/send
transition takes zero logical time, and correct proposal acquisition returns a
complete locally-T candidate in that entry transition (or uses retained VC).
Clocks measure exact online durations. There are no correct crashes or offline
periods during the service interval. At its start every correct participant is
at h with its height-start delay expired; arbitrary reachable earlier rounds,
locks, rows and recovered timer remainders are allowed. No higher INIT occurs
through completion, including at a node that has already decided.

These are explicit restrictions, not realistic CPU guarantees inferred from
STP. The service bound below includes bounded whole-frame admission,
verification and receive/guard processing; it does not by itself bound unrelated
local timer dispatch or fresh acquisition. A wall-clock extension replacing
NB/Z/ZG needs bounded scheduler/acquisition latency and clock-rate assumptions,
and a new timing derivation. The Rust host must fairly call `elapse`, `receive`,
`supply` and `validity`; the present source-model proof is not that refinement.

### Generic, version-oblivious service contract

A periodic OFFER is a core's output opportunity before transport/application
admission. An offer may be dropped at either stage. It is not an STP ACK, and a
completed service means the correct receiver has admitted, verified and
processed the complete offered frame, including the ordinary receive guards.
Classify the FULL frames on each correct sender p's persistent rho grid into:

| Lane | Available frame reconstructed from retained state |
|---|---|
| Progress | ENTER+ENTRY while active; complete retained DEC after decision |
| Proposal | p's current proposal with current VC form A/B, when p is current proposer and has published that proposal |
| Prevote | current own signed PREVOTE with the current PV row and prescribed best-effort proposal |
| Precommit | current own signed PRECOMMIT with its prescribed PV row, or matching body and current-round polka |

Proposal/Prevote/Precommit availability is relative to one fixed (h,r): the
relevant own item has been published and remains current. It does not require
step=propose or step=prevote: existing own votes continue to replay after later
step transitions in the same round. A fresh round has no recovery to clear its
rows. Progress availability is continuous after height-start expiry, INCLUDING
across round changes and the active-to-decided transition. Pre-round ENTER is
not an available Progress offer for this theorem.

**BPS(H), bounded periodic-lane service.** There is some finite H>=rho, unknown
to the protocol, such that for each distinct correct ordered pair (p,q), each
of the four lanes, and each interval [u,u+H] wholly inside the service interval:
if that lane is available throughout, its periodic offers yield at least one
frame OFFERED after the start boundary u and processed by q no later than u+H.
Use one global total order of events extending each process's local order to
resolve equal logical timestamps. An offer at the same logical timestamp counts
only if it follows that boundary in this order; an earlier same-time queued
copy does not count. For the
three round-relative lanes the interval must lie wholly within that availability
context r. For Progress the frame version and sender round may change during
the interval. Endpoint transitions may be assigned to either adjacent version;
all comparisons protecting round-local evidence below are strict.

The arbitration key is (authenticated sender, destination, lane), NOT a round,
ENTRY region, certificate digest, proposal-authorization form or frame hash.
Each broadcast has this contract separately for each other correct destination.
Self-insertion and local publication need no network self-delivery. If the
designated correct set consists of a single member, necessarily of quorum
weight, its pairwise service requirements are vacuous; the own-vote and
local-proposal cases below still apply.
An old queued frame offered before u, arbitrary replay traffic, or success on
another lane does not fulfill it. A frame offered inside the interval may be
superseded before its processing, but the receiver must still process that
actual complete frame. Arbitrary individual offers, including the initial
broadcast, E reply and 35' resend, may be dropped. It is their periodic streams,
not those particular actions, that receive the guarantee.

This is a generic class scheduler/service premise, not a rule selecting frames
by eventual decision, usefulness to a particular lock, or certificate contents.
Mutating a Byzantine row column cannot reset its deadline. For example, a
version-oblivious guarantee that one of every k consecutive eligible periodic
offers completes within delta of its offer implies H=k*rho+delta, with
admission/CPU processing included in delta. This is an example of sufficient
bounded service, not a property asserted of the current bridge. Bounded memory
alone, eventual connectivity, or successful STP transmission does not imply it.

The proof uses ONLY these FULL periodic lanes. It therefore also applies when
an opt-in CompactLive mode makes initial ENTER/PREVOTE/PRECOMMIT and 35'
transition sends bare, provided the existing full periodic reconstructions and
all consensus guards/retention rules remain unchanged. Reference mode retains
the source initial-send formats. Neither mode may replace the periodic full
packages with unproved deltas or hash references.

### R1: retained replay evidence and stage of admission

Until leaving a round, PV signer occupancy never decreases. A Byzantine column
may be replaced under the source's supported-group rule, but a correct column
cannot change to conflicting signed content, and the process's own signed
PREVOTE remains self-inserted. Thus a PV row that once has quorum signer weight
continues to have it, although its serialized bytes need not stay identical.
The current own PREVOTE is replayed with that row even after precommit. A fresh
round's nil PRECOMMIT was enabled by a nil PV quorum or an expired quorum-armed
prevote timer, so its replayed PV row has quorum weight. A nonnil PRECOMMIT
retains the matching proposal and current-round VC polka until leaving: a
distinct-id quorum cannot displace them by weighted quorum intersection. These
periodic frames are self-carrying for their round, exactly as source PK(2).

A received self-carrying frame moves a lower active receiver to its round by
55' and THEN merges the current-round row/proposal from that same frame. It
requires no combining of future fragments. Below-witness future votes and
still-future headers are discarded; lower-round rows are not resurrected. A
proposal received before its recipient enters r is not assumed retained.
For a correct proposer, its unique current header/body survives throughout r;
its old-vr form-A authorization may change to the same value's current-round
form B after VC advancement. Every claim below respects that change. These are
the source's retained-state/receive facts, not new packet storage.

In Rust these facts are visible in `core.rs` row insertion and own-column
protection, `periodic`, and `d28.rs` complete-vote/proposal emitters (the
bandwidth extension names the full helper `emit_vote_complete`): rows and VC
attachments are reconstructed on every replay. Their inspection supports this
proof boundary, but is not a machine-checked correspondence proof.

### R2: successor bridge lemma (including transient first bridges)

Assume no correct decision yet. Let correct p be in round M at u, and q be
below M. Every later Progress offer from p is an ENTER at L>=M, until a terminal
DEC replaces it. Consider any such ENTER received while q is still below M.

* If L=M and ENTRY is TIMEOUT(M-1), its quorum PCs take q to M-1, insert that PC
  quorum and arm its value-independent precommit timer. A heavy outer ENTER
  can instead take q directly to M. In either case q reaches >=M within at
  most TPC(M-1) of receipt, unless it advances sooner.
* If L=M and ENTRY is JUMP(M), its retained witness votes of rounds >=M take q
  to >=M at once. The mixed higher votes are handled by the same one-frame
  greatest-cut selection as source PK(2'').
* If L>M and ENTRY is TIMEOUT(L-1), predecessor votes have rounds L-1>=M and
  quorum weight; q reaches >=M at receipt. For JUMP(L), witness votes have
  rounds >=L>M and yield the same lower bound.
* Empty ENTRY occurs only at round zero, where the target is already reached.

A q below the relevant target has never signed a vote at or above that target,
so excluding q from the jump scan removes none of the counted weight. Other
receipts either preserve these facts or already move q past the target. If q
starts a timer in M-1, its new/unexpired/recovered remaining duration is at
most TPC(M-1); an already-expired timer advances once the actual PC quorum is
present. No missing quorum is fabricated.

Progress remains available under supersession, so BPS on [u,u+H] supplies one
of these versions. Therefore by u+H+TPC(M-1), every q is in >=M, unless an
earlier correct decision has invoked R6 below. In particular, if e is the
first correct entry into r+1, that entrant's subsequent Progress stream puts
everyone in >=r+1 by e+H+TPC(r). Its original ENTRY and original SEND need not
survive H. This is the replacement for A2's one-shot early-exit bridge, not an
assumption that this particular bridge is delivered.

### R3: periodic quorum-row handover (replacement for PK(3)/35')

In a fresh round r, let s1 be the first correct quorum-in-prevote instant,
including the case where 36 preempts 34. Let a_c be correct c's PREVOTE signing
time. Suppose no correct process leaves r through max(a_c,s1+H)+H.

The first quorum holder has its own PREVOTE, and R1 keeps its quorum PV row.
Its periodic Prevote lane is self-carrying throughout [s1,s1+H], even if 36
immediately changes its step. BPS and the jump-then-merge receive order give
every correct process quorum PV signers in r by s1+H. Let g_c<=s1+H be c's
first such acquisition. At and after max(a_c,g_c), c's current own PREVOTE
replays with a quorum PV row; BPS gives everyone c's PREVOTE by

    max(a_c, s1+H) + H.

Correct row columns persist through this handover. Byzantine id/signature
changes do not lower signer occupancy or change the lane key. Neither a
byte-identical 35' snapshot nor a successful transition send is used. The
source's no-44 premise is still available where L10 uses its nil-weight
argument; periodic transport of these rows itself does not require that
premise. All service intervals here lie before the assumed first departure.

### R4: periodic proposal delivery and L10's first-stopping-event argument

Take source L10's fresh round r>0, first entrant p at t, correct proposer q,
and entry-lock domination hypothesis. Put the first correct nil PRECOMMIT or
first correct exit from r as the stopping event; an earlier decision uses R6.
All delivery claims in this paragraph are conditional prefix claims, not an
assumption that senders remain forever.

By R2, entries satisfy e_c<=t1=t+H+TPC(r-1), unless the stopping event has
already occurred. Whenever both c and q have entered and the required service
interval precedes the event, q's full Proposal lane gives a post-entry,
q-origin copy, with T, by

    b_c <= max(e_c,e_q) + H + d
        <= max(e_c,e_q) + 2H + d.

For q itself publication is local and zero-time. For c!=q this copy is offered
AFTER c entered; there is no retention assumption for an earlier future header.
The unique correct proposer header makes SLOT empty or that same proposal.
A form-A arrival sets/preserves the justification bit even if a relay's form B
arrived first. A later VC update may make the selected q-origin offer form B;
inside this no-nil-PC prefix it implies q locked and published its nonnil PC
and polka, making the FULL Precommit lane available no later than that offer.
A relay-origin form B alone does not imply a q lock.
Thus E is not used, even if its replied bit was consumed by a dropped reply.

Use the following strict inequalities:

    TP(r)  > 2H + d + TPC(r-1)
    TPV(r) > 4H + d
    TPC(r) > TPV(r) + 2H + d.

The source's earliest-prevote argument still applies: a first correct prevote
cannot come from 34' without an earlier correct prevote, from another header,
or from TP before q's bounded entry. Thus no correct prevote precedes e_q.
The displayed proposal bound precedes a propose deadline e_c+TP(r): if e_q<=e_c,
use b_c<=e_c+H+d; otherwise use q's replay after e_q and
b_c<=t+2H+TPC(r-1)+d. A 34' arming y is at least max(e_c,e_q), so the bound
also precedes y+TPV(r). These comparisons place the entire needed service
interval and T determination before the deadline being tested. They do not
apply BPS across an already-departed sender or assume the conclusion.

Consequently any correct nil prevote in the prefix already had its designated
q-origin delivery. Form A would have enabled the value prevote under entry-lock
domination, so its delivery must be q-origin form B. That gives a value polka
and q's prior nonnil PC. Nil prevoters are outside that polka's signer set;
its intersection with any alleged nil quorum contains a correct signer.
Therefore 44 cannot be the first correct nil PRECOMMIT.

For a first rule-63 nil PRECOMMIT at n, the first quorum-in-prevote instant s1
satisfies n>=s1+TPV(r). R3's first row replay places everybody in r with a
quorum by s1+H. The conservative designated-copy bound gives all T acquisitions
by s1+3H+d, strictly before n. If any designated q-origin delivery is form B,
q's retained nonnil PC lane supplies everyone its polka/body with T by
s1+3H+d: the q-origin offer occurred by s1+2H, and its nonnil PC was already
available. If all designated deliveries are form A, everyone prevotes the
value by s1+3H+d; R3 hands over each correct vote by s1+4H+d. Either way every
correct process holds the value polka and T strictly before s1+TPV(r)<=n, so
36 preempts the alleged 63. All required replay intervals finish before n.

For a first exit at L, let t2 be the first correct precommit-timer arming;
L>=t2+TPC(r). Its PC quorum has a correct nonnil member because no correct nil
PC precedes L. That member's full PC replay puts everyone in r with polka and
body by t2+H, with T by t2+H+d. A still-proposing node reaches prevote via 34'
within TPV(r); it then precommits the value. All correct PC lanes, a quorum
of weight, are received by

    t2 + 2H + d + TPV(r) < t2 + TPC(r) <= L,

and decide before the alleged first exit. If neither stopping event occurs,
the value-oblivious progress bound R7 supplies finite progress or an earlier
decision; L10 is not being used to prove its own rendezvous. This reproduces
L10 with periodic carriers rather than reliable entry broadcasts, E, 35' or
initial PC sends. The bound H already includes periodic grid wait.

### R5: L11 lock propagation and A5.0's frontier bound

For L11, retain its fresh-round and first-exit notation. Let locked p have
T body and polka, and let L be the first exit, whose timer was armed at y on
PC quorum S. All round-local lanes persist until L.

If a correct member of S nonnil-precommitted the locked value at x<=y, its
periodic PC gives everyone the body/polka by x+H and T by x+H+d. Rule 34'
adds at most TPV(r) for a still-proposing node; the valid update is before L
under TPC(r)>TPV(r)+2H+d. Distinct nonnil ids cannot have polkas.

Otherwise all correct members of S precommitted nil via 63; 44 is impossible
because a nil polka would intersect p's polka in a correct signer. Writing s1
for the first quorum-in-prevote instant gives

    L >= s1 + TPV(r) + TPC(r).

R3's first periodic quorum row reaches everyone by s1+H. Each reaches step
>=prevote by s1+H+TPV(r). If p has not yet locked, its own 34 timeout allows at
most one further TPV(r); hence its lock time t_lock obeys

    t_lock <= s1 + H + 2TPV(r).

p's retained full nonnil PC supplies T body/polka everywhere by

    t_lock + H + d <= s1 + 2H + 2TPV(r) + d
                    < s1 + TPV(r) + TPC(r) <= L.

All valid updates therefore precede the first exit. This includes a receiver
initially below r: that self-carrying frame first jumps it into r and then
merges its polka/body. There is no retention of future proposals across a
round entry, and no delivery after a discarded round is assumed.

For A5.0 let t2 be the first relevant correct arming of TPC(r0+1) leading to
the first entry into r0+2. The PC quorum has a correct member, whose current
full PC lane is self-carrying by R1 and remains available until the first
exit. Since TPC(r0+1)>H, its replay reaches all correct processes by t2+H,
putting any old-round laggard into r0+1. Thus the last old-round departure S1
satisfies S1<=t2+H before the first exit. No accepted one-shot frame is needed
for this frontier argument.

### R6: terminal decision dissemination

If correct p has decided at x, its retained PEND body/decision certificate
replaces ENTER on the same Progress lane. For a service interval starting at
u>=x, BPS gives every correct receiver a complete DEC by u+H; strong validity
and retained T give adoption by u+H+d, in any step. If p switches to DEC during
a previously started Progress interval, that interval still supplies a genuine
Progress version; alternatively start at x to get the stated terminal bound.
This is the replacement for source L8'/T3' post-decision reliable SEND.

### R7: value-oblivious rendezvous and a quantitative theorem

For general timeout functions, ASSUME a finite K exists for which all three
R4 inequalities hold for every r>=K. The source's nondecreasing affine
functions with incPropose>incPrecommit>incPrevote>0 imply this for every finite
unknown H,d in unbounded arithmetic. Accepting an arbitrary Rust `Timeouts`
value is not itself a liveness proof: this growth/inequality premise and all
finite-domain bounds remain necessary.

At time s all correct participants satisfy the model above. Let m be their
maximum round, TPC(-1)=0, and define

    R = max(m, K-1) + 2N
    W'(r) = TP(r) + TPV(r) + TPC(r) + 3H
    B' = H + TPC(m-1) + sum[r=m..R] W'(r).

All following progress claims stop at an earlier first correct decision, when
R6 replaces them. A1's monotone rounds and first-frontier-entry provenance use
only signatures and weight, not delivery: the first correct entry into a new
frontier r>m is 67 from r-1. Its initial ENTER can now be bare or lost.

R2 applied to a correct node initially at m gives every correct process round
>=m by A_m=s+H+TPC(m-1). Inductively suppose all are in >=r at A_r. Set

    F_r = A_r + TP(r) + TPV(r) + 2H.

If some correct node first enters r+1 at e<=F_r, R2 on its Progress stream
gives all nodes >=r+1 by e+H+TPC(r)<=A_r+W'(r). Otherwise everyone stays
exactly in r through F_r. All correct PREVOTEs exist by A_r+TP(r), since a
restored or newly armed propose timer has at most TP(r) remaining. Their
periodic Prevote lanes supply a correct-weight quorum everywhere by
A_r+TP(r)+H. Value-independent guards then produce all correct PRECOMMITs by
A_r+TP(r)+H+TPV(r); their periodic lanes supply all correct PCs by F_r.
Every precommit timer has at most TPC(r) remaining, or an expired recovered
timer can now use an actual quorum, so all enter r+1 by A_r+W'(r). No timeout
inequality is required below K. If an earlier exit interrupts a stay-case
service interval, it belongs to the first, successor-bridge case instead.

This proves the A2 induction with W'. A3 co-residence follows from R2: from
first entry E into a fresh r until first exit, every correct entry is by
E+H+TPC(r-1), whenever that interval is nonempty. A4's old-lock domination is
unchanged: sample p* with greatest validRound only at the LAST departure S1
from r0=max(m,K-1), thereby including any hidden older polka delivered before
that departure. R5 propagates every later fresh-round lock before its first
exit, and also bounds the frontier at S1 by r0+1. Choose p*'s first proposer
round r1 strictly above r0+1; rotation gives r0+2<=r1<=r0+N+1 and its proposal
is signed after S1. If there is no correct lock in (r0,r1), p*'s actual entry
validRound dominates every entry lock, including hidden old locks, and R4
applies in r1. Otherwise choose the least lock round a in (r0,r1). R5 gives
every correct process validRound>=a before entry a+1. For every subsequent
round, that bound persists if no new lock forms, or R5 propagates that round's
lock before the next entry. Thus each subsequent proposer's actual entry
validRound dominates all entry locks. The first correct proposer round r2>a
satisfies r2<=a+N<=r0+2N, and R4 applies there. These are source A5's two cases;
a current-round lock on the unique correct proposal does not defeat the entry
hypothesis. If no decision had occurred by A_rdec+W'(rdec), A2 would force an
exit from the deciding round rdec, contradicting R4. Therefore the A2 sum
through R bounds a first decision by s+B'. No post-decision A2 induction is
used. This is the source A4-A6 argument with its communication dependencies
explicitly replaced above. The fast-cohort corollary below is a separate
conservative reclassification, not a change to the all-correct quantifiers.

**Conditional termination theorem.** Under these local/validity/configuration
premises and BPS(H) for the required intervals, a correct participant decides
by s+B', unless one decided earlier. In either case R6 gives all correct
participants a complete DEC and decision by

    s + B' + H + d.

If a correct participant already decided before s, the shorter bound s+H+d
applies. If no stopping event occurred in R4, R7's independent value-oblivious
progress supplies the finite arming/exit needed there; it never assumed a
successful proposal, validity, or L10 to prove that progress. This avoids a
circular liveness premise.

**Finite-service-window corollary.** Put L=B'+H+d. It suffices that all the
model premises hold through [s,s+L] and that BPS(H) holds ONLY for H-long
intervals wholly inside that window. R7 uses service finishing by the first
decision deadline s+B'; a first decision at x<=s+B' leaves the full terminal
interval [max(s,x),max(s,x)+H] and at most d for adoption before s+L. Every
correct participant therefore decides within the window. All transmissions
outside it may be dropped forever. Inside it individual offers may still be
dropped: unlike the preceding clean-window fallback, this corollary requires
no loss-free subinterval and no reliable particular SEND. The existence of
such a generic service window is sufficient, not claimed necessary or minimal.

### Fixed actually-correct >2/3-weight timely-cohort corollary

Let C_fast be one FIXED set of actually correct roster members with
w(C_fast)>2TW/3. It must remain the same set throughout the service window.
Require the preceding online, height/configuration, NB/Z/ZG, clock, proposal
and bounded-validity (M9c/M9d) premises only for members of C_fast, and BPS only
for ordered pairs within C_fast. M9a soundness remains a global safety premise
for every actually correct participant, including slow outsiders. Members
outside C_fast need not be online or timely and may keep arbitrary stale locks. Retain the full roster's N, weights,
configuration and proposer rotation; do not renormalize to the fast members.
Compute m as the maximum round within C_fast and use the same R, W', B', L.

Proof: reclassify EVERY member outside C_fast as Byzantine for this application
of the source-model theorem, including slow members that are actually correct.
Their total weight is TW-w(C_fast)<TW/3. Actual correctness of C_fast implies
each source correctness/signing invariant for this smaller designated correct
set, including its pre-window history; allowing the complement arbitrary
behavior only enlarges the executions considered. A witness still contains a
C_fast signer and a quorum still contains >TW/3 weight from C_fast, so A1's
frontier provenance, quorum intersection and all R1-R7 arguments apply with
correct=C_fast. Outside high-round votes alone cannot manufacture a witness;
outside old locks impose no new obligation on this conservatively Byzantine
set. Outside proposers are allowed Byzantine proposer rounds, just as in the
original rotation argument. Hence C_fast decides by s+L, with a first fast
decision by s+B'. A decision previously made only by an outsider need not be
used as a source-model correct decision; any certificates it supplies are
handled as arbitrary authenticated input instead.

This proves timely-cohort decision, NOT timely decision by every actually
correct slow outsider. Agreement for the actual fault set separately ensures
the decided value is unique, using the global safety assumptions earlier in
this document. A slow actual-correct receiver can later adopt through one
complete DEC package plus its validity service; if the sending fast member
still retains PEND and a later fast-to-slow Progress service interval of length
H_q is available, adoption is by that interval's start+H_q+d_q. If higher INIT
has retired PEND, the host's authenticated archive must supply the package.
No arbitrary time-varying or rotating >2/3 timely-weight assumption follows
from this fixed-set reclassification: without a fixed C_fast it does not give
one <1/3 complement fault set for the argument.

### What this does not establish; counterexamples and implementation boundary

* Infinite successful deliveries per link can service ENTER and nil votes while
  every finite-lived proposal is lost. Fair loss only for an identical message
  repeated infinitely often is likewise insufficient when every round ends.
* A bound on successful messages per sender without lane separation can starve
  all proposals. A bound that resets on each round, PV-row digest or Byzantine
  column replacement can indefinitely postpone the evolving evidence streams.
* Admission drops and CPU starvation must be covered by BPS and the local model;
  STP ACKs or a finite memory cap do not supply these guarantees.
* A complete future self-carrying frame is essential: separate sub-witness
  fragments or bare live votes are not retained/combined as a future-vote bank.
  CompactLive is covered only because its full periodic repair is unchanged.
* The main theorem assumes all its designated correct participants are timely.
  The fixed actually-correct >2/3-weight corollary relaxes this only by the
  explicit complement reclassification; arbitrary rotating timely weight or a
  cohort containing Byzantine stake is not covered.
* Finite slots do not refute BPS: unknown H may scale with N, maximum body size
  and available throughput. Fair admission of complete bounded frames could
  support such a bound. Budgets and retries alone do not prove it, especially
  under Byzantine flooding; no current STP-bridge BPS proof or queue/slot
  implementation is supplied here. A finite simulation prefix dropping all
  proposal-bearing frames while control flows can illustrate the insufficient
  blackout premise, but is not itself a proof of an infinite execution.
* Higher INIT may retire PEND. Subsequent historical catch-up then needs the
  host's authenticated archive; this bounded one-height core is not an archive.
* Rust arithmetic, rounds and timers must remain within checked integer bounds
  throughout this finite argument. Exhaustion fails closed, and the source's
  unbounded arithmetic cannot prove execution past that point.

No extra durable snapshot or wire message is required for this theorem. An
adapter can expose or infer the four lane classes from the existing frames;
Progress must share one key across ENTER round supersession and terminal DEC.
If it defers serialization, it must keep bounded replay REQUEST descriptors and
regenerate current frames, not retain borrowed `Sink::send` data. A transport
queue retaining owned frames instead needs its own byte/work accounting.
Application-processing acknowledgement, not an STP ACK alone, is needed before
an optimization may suppress periodic repair to a particular receiver. The
periodic-lane theorem does not certify such an optimization or its memory
bound. The separate guided-only theorem below has different service premises.

## Guided-only evidence: a separate conditional theorem

This section does NOT infer guided-only termination from the preceding BPS
premise. BPS services full broadcast lanes; the protocol here has NO broadcast
body/certificate fallback. It supplies replacement lemmas, using actual latest
peer status and bare own-vote service. The bound is conservative and is not a
claim about an STP implementation or a Rust wall-clock refinement.

### Emission/API and information contract

`Emission::Guided` has the following boundary. Automatic transition, proposer,
reactive-E, periodic and recovery output never sends a proposal body or group,
whether its target would have been `All` or `Peer`. Automatic vote output is
only the node's own signed ENTER/PREVOTE/PRECOMMIT without attachments. The rho
grid continues to offer its current own PREVOTE after precommit, and its current
own PRECOMMIT after signing it, until departure. It does not stop the PV stream
at a phase change. A decided node advertises status but broadcasts no DEC.
Explicit `serve(peer, latest_status, sink)` is the ONLY evidence emission path;
it emits at most one complete, already-retained frame, to that peer only.
`recover_with_emission` must select Guided BEFORE recovery can emit anything;
selecting a volatile policy after a reference-mode recovery is too late.
Reference and CompactLive retain their separate earlier contracts.

The Guided runtime uses `elapse_coalesced`: between consensus/height-delay
expiries it applies the full timer decrement and exact normalized rho phase
once, rather than processing every missed rho tick separately. It retains an
old-state replay when an earlier tick was crossed, and a post-drive replay when
the endpoint coincides with a tick; both may occur. With eager storage,
successful final descriptors, signed records, rows and the runtime's reduced
events match reference elapsed processing; intermediate per-tick
callback/write/crash-cut traces intentionally differ. Public `Engine::elapse`
keeps its reference tick processing. This runtime coalescing is not a wall-clock
refinement of the source theorem above.

The runtime now explicitly opens buffered storage. Signing and publication
barriers remain synchronous, but countdown/rho-only updates can remain in RAM
until the next fixed-age one-second checkpoint (plus scheduler/storage stalls),
not a sliding debounce restarted by traffic. Recovery resumes the last committed
remaining durations and rho phase, without charging offline time. This can
repeat elapsed/replay work after a crash; it cannot forget an externally used
signing choice or lock. See the separate persistence contract below.

The host advertises `Engine::status()` periodically, also after useful state
changes, including throughout round changes and after decision. A snapshot
contains configuration, height, round, step, current proposal id, its
`justified` bit, retained polka `(round,id)`, and `prevote_quorum`. The last bit
means quorum signer WEIGHT in the current PV row, not a matching-value quorum.
These are constant-size hints, never votes or proofs. Bind a snapshot to its
authenticated source and a fresh transport generation plus sender sequence;
reject older sequence/generation pairs. The host must admit and retain the
correct roster peers' records; merely allocating N table slots does not ensure
that Byzantine identities cannot exclude a correct peer. No per-peer signer
inventory is required. No status may fabricate weight, external T validity,
proposal authorization, a lock, or a decision.

The directed planner must have these dominance properties, including when the
recipient reports `Step::Propose`:

1. An undecided peer is offered retained complete DEC once the sender decides.
2. For a peer in a lower round, prefer a retained CURRENT self-carrying package
   when available: matching body/current polka (with own nonnil PC if held), or
   own PV plus a quorum-weight current PV row, even a mixed/nil row. Otherwise
   use full ENTER+ENTRY. A TIMEOUT(r-1) ENTRY alone may take another TPC(r-1)
   to enter r; it is NOT a substitute for the current self-carrier in this rule.
3. For a peer in the same round, a missing matching current polka/body is
   repaired by its complete package. If the local PV row has quorum weight and
   the peer lacks a PV quorum, repair that quorum BEFORE a proposal-only gap.
   Such a row frame may carry the prescribed best-effort proposal, but need not
   carry one when the sender prevoted nil.
4. Remaining proposal gaps include a missing body AND a held old-vr proposal
   whose exact-vr authorization is missing. Serve the same current header/body
   with its retained exact-vr polka (form A), or its matching CURRENT-round
   polka (form B) when the old authorization was superseded. A bare repeated
   header does not close an authorization gap. Form B does not set `justified`;
   it repairs the current PV row and uses the existing 34'/36 guards.

A form-A-to-form-B change cannot reset service. Current polka repair must be
possible from retained VC even if there is no matching own nonnil PC, since a
late VC update after a nil PC need not create a new lock. Proposal-only repair
must not repeatedly outrank a quorum repair against a polluted cached body.
Step-difference repair without a quorum may remain a best-effort extra, but is
not used in the proof. Individual missing PV/PC votes are supplied by their
signers' bare streams, NOT inferred from step or `prevote_quorum`. A PC-quorum
bit, pending-DEC bit or timer flags can reduce redundant traffic, but are not
needed for this theorem. An indeterminate retained body relies on the validity
service below, not on status being treated as T.

### Non-oracular service premises

Use all the preceding one-height local-execution, signature, validity,
NB/Z/ZG, clock, no-crash and no-higher-INIT restrictions for the designated
correct set. In particular retain STRONG M9c: once a correct holder has T at x,
every correct copy still held or newly acquired is determinate by the
applicable max(x,s)+d boundary (a later acquisition adds at most d); soundness
makes it T. A previously held copy need not be fetched again merely to gain
this bound. Correct fresh acquisition is still the source's zero-time locally-T
operation. Timers do not await validity.

There are finite unknown S,A,V>0, including rho-grid waits where applicable,
with the following contracts for every distinct ordered pair (p,q) of designated
correct members. Intervals are wholly inside the useful service window. Use
the earlier global event order for equal timestamps; "after u" excludes an
old queued copy, even at the same logical timestamp.

* **STATUS(S).** For every boundary u, by u+S, p has accepted a status actually
  generated by q AFTER u. Status continues across rounds, phase changes and
  decision. Sender sequences prevent a later receipt from rolling the table
  back. Thus an old snapshot cannot indefinitely hide a newly created gap.
* **REQUEST(A).** If the SEMANTIC response obligation
  `gap_for(latest_admissible_status).is_some()` persists throughout [u,u+A],
  p's explicit guided response stream to q yields at least one actual complete
  frame OFFERED after u and admitted, verified and processed by q by u+A.
  Eligibility here ignores outstanding grants, retry backoff and temporarily
  occupied K slots: these may delay service WITHIN A, not make the antecedent
  vacuous. The key is authenticated `(p,q,guided)`, not the gap enum, round,
  status sequence, certificate hash, evidence version or proposal form.
* **BARE(V).** In any V-long interval during which p's own PV (respectively PC)
  remains current in one fixed round, its periodic bare stream has a vote
  offered after the start and admitted, verified and processed by q by the end.
  PV and PC have distinct keys, independent of their ids and p's current step.
  Processing a vote while q is below its round may discard it; this premise
  does NOT promise retention or combining of future bare votes. Self-insertion
  needs no network self-delivery. Bare ENTER service is optional for the proof.

All three are observable class/transport contracts: they refer to generated
statuses, the local public planner result, actual offers and actual receive
processing. None selects a message by a future decision, global lock knowledge,
or whether it will ultimately be useful. REQUEST concerns ANY outstanding
local response stream, not a globally chosen "needed certificate". It remains
one stream across ENTER supersession and active-to-DEC. Individual status,
vote, initial send, directed attempt and entire frames may be lost. The
contracts do not require all live SENDs to succeed. Availability gaps caused
by genuine completion need no service; the lemmas below prove when that
completion already supplies the relevant local fact.

The director/host must offer continuations often enough to realize REQUEST:
K>=1 is fixed independently of N, grant timeout is positive and bounded, capped
backoff is bounded, host polling is bounded, and status TTL must not expire a
correct peer between STATUS updates. Latest statuses must not cancel/restart a
still-open semantic obligation's deadline. Fair service must cover every
registered correct peer despite Byzantine requests. The guided director uses
strict monotone service ordinals (checked for exhaustion BEFORE sending) for
oldest-unserved arbitration, alternating those opportunities with gap priority.
This preserves ordering even when multiple polls share a host timestamp, with
O(1) words per peer. Gap priority alone would be insufficient. With capped backoff, finite N, bounded grants/polling
and strict oldest-ticket opportunities, request OFFER opportunities have a
finite bound that may grow with N/K. Successful whole-frame admission and CPU
processing still need their own bounded loss/service premise to obtain A.
Neither finite K, a fair selection order, an STP ACK, nor existing tests proves
REQUEST. No lane-level inventory/scheduler is needed for this proof once the
planner dominance rules above hold; without them ANY-gap service is not enough.

### Derived repair lemmas and timing

Put E=S+A and choose one deliberately loose constant

    H_g = 4(S + A + V).

A repair wave takes at most E: wait for a post-boundary truthful status, then
apply REQUEST to the still-open local obligation. If that obligation ceases
before its service deadline, a NEWER truthful status or monotone sender
advancement must already imply the target fact (or an earlier decision/exit).
If it does not cease, the actually selected complete frame has the following
properties by the planner rules. This is a derived alternative, not an extra
service axiom. In a fixed-round no-exit prefix, row occupancy, correct vote
columns, matching correct-proposer body/justification and held current polkas
are persistent. Byzantine row-column replacements do not decrease signer
occupancy. For Progress the sender's round is monotone and its DEC is terminal.

**G1 (successor bridge).** A correct p at M followed by later versions of its
Progress response puts any q initially below M into >=M within
E+TPC(M-1), or supplies complete DEC. If q's latest status already reports
>=M, the target is already reached. Otherwise its lower-round obligation stays
open; the actual response is a current self-carrier, or ENTER+ENTRY at L>=M.
The TIMEOUT/JUMP cases are exactly R2, including L>M and heavy signers. Thus R2
holds with H_g in place of H, including a transient first entrant whose ENTRY
is superseded before service. No particular first ENTER must be delivered.

**G2 (quorum handover).** At the first correct quorum-in-prevote instant s1 in
a fresh r, its holder can serve a current self-carrier. In the no-exit prefix,
within E every correct q is in r and holds PV quorum signer weight, or already
has that fact: lower-round responses prefer the self-carrier, same-round
missing-quorum responses carry the row or a stronger current polka. A package
jumps BEFORE merging its own current-round evidence. An already complete peer
needs no duplicate row. Start bare c-to-q PV service AFTER q's entry, not at
c's earlier signing time a_c. Everyone gets each correct c's vote by

    max(a_c, s1 + E) + V <= max(a_c, s1 + H_g) + H_g.

This proves the R3 handover bound without transmitting every c's quorum row
and without retaining any below-witness future fragment. Phase changes do not
remove the bare PV lane. A first departure truncates this prefix; no claim
uses service after a discarded round.

**G3 (proposal and polka).** Once c and correct proposer q have entered r,
q's response to missing proposal/authorization supplies its unique header/body
and either exact-vr authorization or matching current polka within at most
TWO repair waves. The first wave may instead supply a quorum PV row without
the body when q's own PV is nil, or WITH the body but WITHOUT its old-vr
certificate. After a fresh status reports that quorum, the next wave cannot
be starved by that same row gap: its missing body or held-but-unjustified
proposal requests the exact-vr package. This is why 2(S+A), rather than one
wave, is budgeted. A received current-polka package already supplies both. If status already reports body plus justification (or current polka),
the relevant evidence is already held; strong M9c supplies T within d once q's
locally-T publication exists. For a correct proposer no conflicting signed
current header exists. Consequently the R4 designated-copy bound is replaced
by the following evidence-or-absorption bound:

    b_c <= max(e_c,e_q) + 2E + d
        <= max(e_c,e_q) + H_g + d.

Form A already held from a relay is sufficient authorization. A received form B
alone is NOT a servable retained VC: a receiver still in Propose may hold its
verified PV row and even a T body without installing VC or signing a PV until
34'. In particular, such a receiver is not an additional one-wave polka source.
Use the CORRECT PROPOSER q's following dichotomy instead. While q retains the
exact-vr VC, a peer lacking authorization keeps q's form-A obligation open,
even if that peer already received a form B from someone else. After at most
the quorum-row wave and the authorization wave, q supplies form A. If q
supersedes that exact-vr VC, its replacement in this fixed-round prefix is the
matching CURRENT-round VC; q already has the T body and can itself serve form B.
The current-polka priority then repairs every still-deficient peer in one wave,
with T at most d later. A status reporting the matching current polka denotes
an already installed VC, not merely a received row; this closes the obligation
only when there is a genuine retained-VC holder. This replaces R4's reliance
on q's nonnil-PC lane without inferring a lock or immediate relay ability from
a received form B. A nil prevoter is outside the polka's correct signer set
regardless of the relay's lock. Exact-vr-to-current-polka supersession and phase
changes therefore preserve the obligation and cannot reset a deadline.

**G4 (first stopping events and frontier).** Apply G1-G3 only to prefixes
ending at the first correct nil PC, first exit, or earlier decision. Use

    TP(r)  > 2H_g + d + TPC(r-1)
    TPV(r) > 4H_g + d
    TPC(r) > TPV(r) + 2H_g + d.

The earliest-prevote and form-A lock-domination arguments of R4 are unchanged.
At a purported first rule-63 nil PC n, G2 aligns everyone by s1+E. Put
tau=s1+3E. By G3 either q has installed its matching current VC by tau, or
every correct receiver has received form-A authorization and body by tau.
In the first case q already has T and a servable VC; current-polka repair
gives everyone body/polka by s1+4E, with T by s1+4E+d. Otherwise all correct
processes have value-prevoted by s1+3E+d, and BARE supplies those votes
everywhere by s1+3E+d+V. Both bounds are at most s1+4H_g+d, strictly before n,
so 36 preempts 63. Supersession AFTER tau cannot invalidate the second branch:
its required value prevotes have already been enabled by the delivered form A.
A nil quorum cannot intersect a value polka in a correct signer, excluding 44. At a purported first exit L with first PC-timer arming t2, the
PC quorum has a correct nonnil member; its guided body/polka aligns everyone
by t2+H_g, and bare PCs complete decision by

    t2 + 2H_g + d + TPV(r) < t2 + TPC(r) <= L.

Thus the deciding-round contradiction survives without q-origin full
broadcasts or any delivered E reply. R5's L11 propagation uses G2 for the
first quorum and complete current-polka repair from a correct LOCKED holder.
Such a holder has a T body and installed current VC already, so its service
needs no Propose-step bootstrap or extra validity delay at the source. The
same bounds s1+H_g+2TPV(r) and s1+2H_g+2TPV(r)+d precede the first exit.
For A5.0, a correct member of the first PC quorum has either a nonnil PC with
polka or a nil PC with retained quorum PV row. Its CURRENT self-carrier reaches
old-round laggards by t2+H_g, before that round's first exit. This is why merely
serving ENTER+TIMEOUT(r-1) is insufficient here: its extra predecessor timeout
would invalidate the stated eventual affine inequalities.

**G5 (terminal repair and value-oblivious progress).** A decided correct
holder serves complete DEC against every truthful not-decided status, regardless
of the receiver's round. One wave plus validity yields decision by x+H_g+d
from a first decision x (or s+H_g+d if already decided at s). A status reporting
decision is already the terminal fact. For the no-exit rendezvous case all
correct nodes already in r sign PV by A_r+TP(r), bare service yields a quorum
by another V, and after TPV(r) bare PCs yield a quorum by one further V.
Hence R7's value-oblivious induction uses the same conservative

    W_g(r) = TP(r) + TPV(r) + TPC(r) + 3H_g.

Its early-exit case instead uses G1. This does not assume a successful
proposal, T validity or an indefinitely resident sender to establish progress.

### Guided-only conditional termination theorem

Let m be the greatest designated-correct round at s. Assume a finite K_round
such that all three G4 inequalities hold for every r>=K_round; this is a timeout
threshold, NOT the director's interaction cap K. The source affine growth
incPropose>incPrecommit>incPrevote>0 supplies it for finite unknown H_g,d in
unbounded arithmetic. Retain all finite Rust integer/exhaustion constraints.
Define

    R_g = max(m, K_round-1) + 2N
    B_g = H_g + TPC(m-1) + sum[r=m..R_g] W_g(r),  TPC(-1)=0
    L_g = B_g + H_g + d.

Under the model, API/planner and STATUS/REQUEST/BARE premises above, a first
correct decision occurs by s+B_g and every designated-correct participant
decides by s+L_g. Proof: G1 supplies A2/A3, G4 supplies L10/L11 and A5.0,
while A1's signature/weight frontier provenance and A4's last-departure hidden-
lock sampling are unchanged. The R7/A5 two-proposer-cycle argument therefore
applies with H_g; G5 supplies terminal repair and closes the no-stopping-event
case without circularity. It suffices that these premises hold only through
[s,s+L_g], with each service premise applied only to its full subintervals
inside the window. All transmissions outside that window may be lost forever;
individual transmissions inside it may also be lost. No full evidence is
broadcast in this argument.

The SAME fixed actually-correct >2/3-weight cohort corollary applies by the
earlier complement reclassification, with S,A,V/local timeliness required only
within that fixed cohort. Keep the original roster, weights and N; do not
renormalize. Soundness, non-equivocation and agreement remain global safety
requirements for all actually correct members. Only the fixed timely cohort's
decision latency is bounded. A slow outsider needs a later status/directed DEC
service interval and its own bounded validity, or an authenticated archive if
higher INIT has retired PEND. A rotating timely quorum or a cohort containing
Byzantine members is not covered.

### Scope and missing implementation evidence

This theorem adds no durable region, future-vote bank or per-peer vote
inventory. The core remains O(N+B), and director metadata is O(N) total/O(1)
per peer with at most fixed K granted interactions. Complete-frame ownership,
serialization/reassembly, admission queues and CPU work are separate transport
budgets; counting K grants does not bound arbitrary transport-held copies.
STATUS authentication/admission, continuous periodic status, fair host polling,
bounded grant lifetime/backoff, non-vacuous semantic request continuation,
whole-frame loss/admission service and validity completion are still explicit
host/transport obligations. Simulations can verify emission exclusion, planner
choices, bounded metadata and executions of a chosen service schedule; they do
not establish these generic premises for STP. Ordinary fair loss, infinitely
many successful small votes, or finite memory alone still permits every
finite-lived proposal response to be lost. No physical WAL/backend, deployment
memory bound, end-to-end Byzantine-flood scheduler proof, or stronger wall-clock
termination claim is supplied by this section.

## Receiver-selected evidence: a separate conditional theorem

This section concerns the opt-in `gossip::receiver` prototype, not the original
`Director`. It does NOT deduce receiver-selected service from REQUEST(A).
Previously every correct holder independently served its peer's semantic gap;
now the receiver chooses which sources may use its directed interactions. A
selected source need not be correct, hold the needed evidence, or reply at all.
The unchanged `Emission::Guided` exclusion and `Engine::serve` dominance rules
above remain essential. This is another conditional source-model theorem, not
an STP guarantee or a Rust wall-clock refinement.

### Requests, cancellation, and physical admission

`RequestStatus` pairs the authenticated `StatusUpdate` with a peer-specific
`request: Option<u64>`. `Some(j)` nominates that destination using the status
sequence at which this request originated; the SAME j survives subsequent
status heartbeats. `None` withdraws the nomination. Bind the request to the
authenticated peer/connection at each endpoint, its destination, and its origin
sequence. Peer handles and generation counters are ENDPOINT-LOCAL: the wire
echoes the origin sequence, not an assertion that those local handles/counters
agree. The host assigns generations; a peer may not claim an arbitrary newer
one. Reject stale status updates and impossible future origin sequences. Wrong-
generation, withdrawn or already-consumed tokens must not authorize new work.
A newer heartbeat is not a fresh request, a renewed lease, a transport
acknowledgement, or evidence of progress.

There are THREE independent fixed positive bounds, each independent of N:
K_sel source nominations at a receiver, K_rx owned receive slots, and K_tx owned
outgoing slots at a responder. They may be configured equally, but a logical
nomination does not reserve physical receive capacity. K_tx bounds owned output,
not the number of peer request descriptors. Each token permits at most one
ACCEPTED `ReplySink` ownership handoff of a complete frame from `Engine::serve`.
A rejecting sink must return the permit and retain no payload; that declined
borrow may be retried under the same token and is not an accepted response. The
frame is chosen from the source's CURRENT retained state and the actual bound
receiver snapshot. A request does not authorize a caller-selected certificate,
force a particular gap enum, or bypass whole-frame verification. A request that
has no actual planner gap emits no complete frame. Fair exploration may send
control requests to every compatible roster source, including unhelpful ones;
this is NOT an unguided sweep of bodies/certificates or a broadcast fallback.

Latest status, source priority and connection/token churn must not reset service
age for a continuously registered identity. A request has a hard finite lease;
heartbeats do not extend it. Expiry, cancellation and bounded backoff release
logical opportunities for other sources. Keep replay/withdrawal high-water marks
within each authenticated generation so that a delayed old positive request
cannot resurrect a withdrawn nomination. Trusted roster admission must preserve
correct peers' records; repeated attacker removal/reinsertion must not create an
unlimited supply of never-served identities. The stake bound is weighted: it
places no useful cardinality bound on the number of faulty low-weight sources.
A search bound must therefore retain the full finite N, not an assumed f+1
number of sources or an assumption that any chosen source is honest.

Cancellation on an actual local semantic improvement can avoid redundant work.
It is NOT itself evidence that every remaining obligation closed: for example,
advancing from Propose to Prevote or acquiring a mixed PV quorum need not repair
a missing body or its exact-vr authorization. Neither a request, successful
send, transport ACK, repeated status, nor `Engine::receive` returning success
without the relevant state change proves a repair. The lemmas below establish
specific facts from actual planner evaluations and complete-frame processing.

Logical nomination limits do not count transport-held copies. Acquire a shared
physical receive-admission permit BEFORE allocating/decoding a complete incoming
evidence package, and retain its accounting while any owned copy survives.
Charge additional owned copies separately. Cancellation, expiry or reconnection
must not return that capacity while old work still owns it. Apply the same
bounded admission to unsolicited, late and canceled-request evidence from ANY
source; nomination is scheduling, not trust. Such a package may still help if
full verification and receive guards accept it. It cannot revive an expired
logical request or manufacture an acknowledgement. Sender serialization, queued
output, partial reassembly and CPU admission need their own bounded ownership/
work contracts. In this API `Responder::try_new` creates a new outgoing budget;
drain prior outgoing payload owners before replacing a responder. Reconnection
on the same instance preserves its pool, but replacement does not inherit old
permits. A slot-count cap is not a byte bound: the host must bound body size AND
aggregate decoded backing storage, including sparse rows. The core's logical
signer-position bound does not enforce that preallocation byte limit; N+1 groups
each with N backing positions can occupy O(N^2) storage. Bounded completion,
abort and final release under stalled or Byzantine transfers are separate
service obligations, not consequences of RAII or a fixed K_rx.

### Observable useful-service premise, distinct from source fairness

Retain the guided theorem's fixed-height/configuration, designated-correct-set,
non-equivocation/recovery, strong M9c/M9d validity, NB/Z/ZG, clock, no-crash and
no-higher-INIT premises. Keep STATUS(S) for candidate discovery and BARE(V) for
the separate current PV and PC streams. Correct peers remain admitted to the
fixed roster table, and status TTL/admission must not indefinitely exclude them
between STATUS updates. Use the same global event order for equal timestamps.

**PULL(C), bounded fresh request evaluation.** There is a finite C>0 such that
for every distinct ordered correct pair (source p, receiver q), and every
interval [u,u+C] wholly in the service window, if q remains undecided throughout
that interval, the following occurs by u+C:

1. p evaluates its actual retained-state planner against a truthful q snapshot
   GENERATED AFTER u, carried by an authenticated positive request to p. That
   request is admissible in its bound generation/token at evaluation time. A
   newer truthful snapshot on the same still-active token may be used; an old
   queued snapshot or evaluation before u does not qualify.
2. If the planner has a gap, p's explicit response is the complete frame selected
   by `Engine::serve`, actually offered after that evaluation and admitted,
   verified and processed by q by u+C. It is not enough to receive a header, an
   STP ACK, an offer count, or a grant. An eventual late/canceled-token package
   can count only if it actually reaches this complete processing boundary under
   the same physical admission budget.
3. Otherwise the actual pure planner evaluation reports no gap. This is a local
   evaluation result, NOT a claim of semantic progress and not a required
   negative-acknowledgement wire message. Expiry, cancellation, lack of a request,
   no free outgoing/receive slot, backoff, or skipping the peer is NOT this case.
   `gap_for` returning None because the engine is invalid/sealed/exhausted is
   likewise excluded by the model.

This is deliberately stronger than the minimal service a particular execution
may need. It requests fair fresh EVALUATIONS even for sources that turn out not
to help; complete evidence is still emitted only for an actual gap. Its key is
(authenticated source, receiver, pull), not gap, round, token, status sequence,
connection attempt, certificate hash or proposal form. No key reset may avoid
the bound. Source selection does not need to know which p is correct or which
certificate a later deciding round needs. Both the evaluator and any emitted
frame are observable local events. Usefulness is derived below for the correct
witnesses supplied by the consensus proof, not assumed for arbitrary requests.
A q that decides during the interval already supplies the terminal alternative.

One sufficient decomposition makes the obligations explicit. After at most S
for fresh candidate discovery, suppose each registered correct source receives
a fresh positive request opportunity from q within every F time units while q
remains undecided, with all polling, occupied nominations, expiry, backoff and
source rotation included in F. Each opportunity GENERATES its truthful snapshot
at that opportunity, so the first opportunity after u really carries a
post-u snapshot. It must permit a new evaluation/response under the token rules;
a heartbeat on an already-offered token is not a new opportunity. Suppose also
that among any k consecutive such opportunities for a correct ordered pair, at
least one results in the planner evaluation above and, when nonempty, complete
response processing within delta of its opportunity. Count those k opportunities
ACROSS successive tokens, rounds, proposal forms and connection generations;
none of those changes restarts the count. The completion bound INCLUDES request
delivery, the responder's fair bounded-output arbitration, the request's being
admissible when evaluated, whole-frame loss/reassembly/admission, verification
and receive dispatch. Then the conservative bound

    C = S + k F + delta

satisfies PULL for intervals containing those opportunities and their completion.
This is a generic bounded-loss/transaction-service premise, not a consequence
of source rotation. Source and responder fairness must both survive Byzantine
requests, misleading priority hints and identity-preserving token/generation
churn. Status hints are not a complete retained-evidence inventory: a source
can hold a needed exact-vr certificate without looking highest priority, and
received form B does not imply an installed, servable VC. A fair exploration
lane must therefore not permanently exclude a compatible source solely on those
hints. Bounded F may grow with N, frame size, the separate interaction/admission
caps and competing load; no N-independent latency follows from N-independent
interaction counts.

The successful opportunity must keep the request admissible for the actual
source evaluation and permit complete processing by the service deadline.
Processing may be later than logical cancellation, as allowed above. A
sufficient implementation contract can bound request delivery, sender polling/
output admission and transfer/verification by known times and choose lease/
release schedules with adequate margin. Without such a contract, the bounded-
success assumption above remains an explicit premise, even if its constants
are unknown to the protocol. In particular:

* STATUS(S) alone can deliver only `None` between finite positive-request leases,
  or reorder every `Some(j)` behind a withdrawal. Even a lossless ordered link
  and bounded polling can place each positive-request window entirely between
  two responder polls. Late-frame acceptance cannot help if no frame was made.
* Independently fair receiver and sender schedulers need not provide overlapping
  live request/output opportunities. PULL requires the actual rendezvous.
* Peer heartbeats must not restart leases; nor may repeated local cancellations
  be counted as successful repairs. Cancellation caused by a partial semantic
  gain still leaves PULL's fresh useful-service obligation in force while q is
  undecided. Keeping a fair ordinal alone does not prove that obligation.
* Successful useless requests, small-vote delivery, or ordinary fair loss for an
  infinitely retransmitted message can coexist with losing every finite-lived
  proposal response. No blanket fair-loss argument replaces PULL.
* Fixed physical slots can all remain pinned by stalled transfers, or be repeatedly
  seized by unsolicited traffic. Physical admission/release and verification
  scheduling must themselves provide the service inside C. Rejecting unsolicited
  evidence solely because its source was not nominated is not required here.

### Pull repair lemmas

A **pull wave** is a C-long interval beginning at the relevant proof boundary.
For a correct source p and receiver q, PULL supplies an actual post-boundary
snapshot/evaluation and its nonempty response, or q decides. A no-gap evaluation
closes a proof obligation ONLY when the following retained-state and planner
arguments show that its truthful snapshot already has the required fact. An
unrelated status gain, successful request, or no-gap evaluation on an older
snapshot is never substituted for that argument.

**P1 (progress and quorum).** G1 holds with C replacing E=S+A: if p is at M,
its monotone current round/terminal DEC and a truthful q snapshot below M force
a current self-carrier or ENTER+ENTRY, unless q already reached M or decided.
Complete processing reaches >=M within C+TPC(M-1), or supplies DEC. Superseding
the source's round does not reset service. Likewise G2's first fresh-round
quorum holder retains a current self-carrier until the first departure. A
post-boundary lower-round or missing-quorum snapshot forces that package, a
quorum row, or a stronger current-polka package. No gap then means the quorum
fact is already present, not merely that q was polled. Every q is therefore in
r with quorum PV signer weight by s1+C, unless the prefix has already stopped.
Start each signer's BARE service AFTER q's entry; its PV arrives by

    max(a_c, s1+C) + V.

**P2 (proposal and authorization).** G3 holds with at most TWO pull waves,
not just one successful request to some source. Apply PULL specifically to the
correct proposer p: source fairness must reach it even if other selected
sources are Byzantine, unhelpful, or only advertise better hints. In a no-exit
prefix p retains its unique proposal and either exact-vr authorization or its
matching current VC. The first response may repair only a PV quorum. Start the
second wave after that response was processed, so its evaluated snapshot is
fresh enough that the same row gap cannot starve proposal/authorization repair.
If an evaluation has no gap, that actual snapshot already holds the authorized
body or matching current polka under G3's uniqueness/persistence arguments.
Use the same correct-proposer form-A/current-VC dichotomy; a receiver still in
Propose that merely received form B is NOT a new one-wave VC source. Thus

    b_c <= max(e_c,e_p) + 2C + d.

Strong M9c applies to an already-held matching body as well as a newly fetched
copy. Source nomination, a body digest or a request result does not assert T.

**P3 (stopping events, terminal repair and progress).** Put

    H_rx = 4(C + V).

The G4 first-nil-PC, first-exit, L11 and A5.0 arguments now apply with C in every
place they used E and H_rx in every place they used H_g. They still use only
service intervals finishing before the stopping event. In particular the
first-PC-quorum witness must serve a CURRENT self-carrier, not merely
ENTER+TIMEOUT with an extra predecessor timeout. Bare PV/PC lanes remain
separate and are not suppressed by source selection. G5 terminal repair uses
a fresh query to a correct decided holder: either q already decided or the
actual planner emits complete DEC, giving decision within C+d<=H_rx+d. The
no-exit rendezvous still uses bare votes, not a successful proposal; the
early-exit case uses P1. These facts reproduce G4/G5 without assuming that a
nomination or a generic completed exchange is a consensus transition.

### Receiver-selected conditional termination theorem and scope

Let m be the maximum designated-correct round at s. Assume a finite K_round
such that for all r>=K_round,

    TP(r)  > 2H_rx + d + TPC(r-1)
    TPV(r) > 4H_rx + d
    TPC(r) > TPV(r) + 2H_rx + d.

K_round is a timeout threshold, unrelated to either interaction cap. Define

    R_rx = max(m, K_round-1) + 2N
    W_rx(r) = TP(r) + TPV(r) + TPC(r) + 3H_rx
    B_rx = H_rx + TPC(m-1) + sum[r=m..R_rx] W_rx(r),  TPC(-1)=0
    L_rx = B_rx + H_rx + d.

Under the unchanged consensus/model/planner premises and STATUS, PULL(C) and
BARE(V) on the needed intervals, P1-P3 and the guided theorem's unchanged
signature/frontier and last-departure-lock argument give a first correct
decision by s+B_rx and every designated-correct decision by s+L_rx. If a correct
holder already decided before s, s+H_rx+d suffices. The premises need hold only
through [s,s+L_rx]; outside it all traffic may be lost, and individual requests
and responses inside it may also be lost. All quantities and the execution's
rounds, timers, status sequences, request tokens, service ordinals and clock
arithmetic must fit the implementation's checked domains. Exhaustion fails
closed and is outside this finite termination conclusion.

The fixed actually-correct >2/3-weight cohort corollary still works by the SAME
complement reclassification. Require PULL/STATUS/BARE and local timeliness only
within that fixed cohort, but retain the full roster, weights and N. Source and
responder arbitration must not let outsiders exclude its members. No rotating
timely quorum, honest-selected-source assumption, or timely decision by slow
outsiders follows. A later outsider needs directed DEC/validity service or an
authenticated archive after higher INIT retires PEND.

This extension adds no consensus vote rule, persistent region, future-vote bank,
per-peer signer inventory or broadcast evidence fallback. It separates O(N)
control metadata, fixed logical interactions, and bounded physical frame
ownership. The prototype API and finite tests can exercise these contracts and
the chosen simulated service schedules; they do not establish PULL, a shared
STP-bridge scheduler/admission guarantee, a deployment memory bound, a physical
WAL/backend, or a stronger wall-clock theorem. The old sender-selected Guided
and Reference/CompactLive results remain separate and unchanged.

### Outlined payloads and the complete-processing boundary

A body reference is not complete evidence. Splitting an `Engine::serve` frame
into signed-header/certificate metadata and a separately fetched body does not
change the PULL(C) boundary above: the receiver must reconstruct, verify and
process the complete logical frame. Discovery, retries, source retirement,
assembly and body validation all count toward that service obligation. A
reference, possession bitmap, Merkle proof or payload acknowledgement alone
cannot establish it.

The existing proposal header, keyed chain body ID, certificates and host-validity
checks remain authoritative. A transport-only Merkle root is not the chain body
ID; even a signed root claim requires final reconstruction and verification of
the existing body ID and canonical bytes before normal core/host processing.
No transport acknowledgement grants `Status::Valid`.

A valid proposal-header signature alone does not authenticate a claimed DEC or
its priority. Before outlined metadata can replace a pending record or select a
cached-body completion, the runtime checks vote signatures, certificate kinds,
signer distinctness, weights and admission cardinality using the core's existing
certificate predicate. A DEC must contain a decision certificate matching that
exact header's round and body ID. These checks prevent malformed replacement
metadata from displacing retained valid evidence; they neither validate the
missing body nor bypass complete-frame and host validation after reconstruction.

Reference metadata needs its own global fixed-slot bound; retaining a dense
certificate per peer would reintroduce O(N^2) backing storage. Payload attempts
and outgoing ownership are separate from the one-off evidence request tokens.
Retired height/configuration/session state must not be revived by late bytes.
Useful current-scope late data may satisfy an already bounded interest; a token
is a scheduling correlation, not a substitute for content verification. Bytes
already handed to STP retain STP's independent ownership bounds and cannot be
recalled merely by dropping an application lease. The complete-frame theorem
therefore does not, by itself, prove either outlined or chunked acquisition live.

The unordered Merkle transport owns **additional** K=`max_requests` body slots,
shared by partial assemblies and completed caches. These are not the core's
three body slots. Each owns at most Bmax bytes plus bounded tree/retry metadata;
per-peer inventories and send histories contain bitmaps, not body copies.
Current-height content can survive metadata expiry and round/header changes,
but a finite cache cannot retain every distinct body ever observed. Eviction,
source availability and scheduler service still belong in the acquisition
assumptions, not in a claim that randomness guarantees delivery.

Every shard carries its indexed Merkle proof and the original proposer's
BFT-only signature over the exact-header/context-bound root and geometry. The
index is authenticated by the leaf hash, not by arrival order. Proposer-only
initial push and randomized missing-leaf repair use no shard ACK or batch
completion barrier. Neither these signatures nor possession advertisements
replace the complete-body and host-validity checks above.

The Merkle sender admits new encoded work against a geometry-independent byte
ceiling after charging observed active-channel backlog and already-staged
frames, and also limits per-call messages and local outbox slots. If preexisting
work exceeds that ceiling, it adds nothing. Other runtime control producers
retain their existing policies; this is not a hard whole-runtime memory bound.
Transport snapshots are observations, not reservations against concurrent Base
traffic. Native STP queue, FEC/rescue and receive ownership remain separate;
planned-send backlog retirement is not proof of delivery or memory reclamation.

## Persistence and memory are separate contracts

`AtomicBlockStore` is the unchanged fixed-size atomic replacement API. Its
bounded codec contains two configuration banks, three bodies, four certificate
regions and fixed signing/publication records. The image size remains
`4096 + 3*Bmax + 556*Nmax` bytes; the byte format and old-or-new backend contract
are unchanged. `DurableStore::open` retains eager per-mutation durability and
its reference crash cuts. Explicit `open_buffered`, used by the runtime, stages
logical writes in the same bounded workspace and commits at barriers instead.
It does not add an append-only WAL, another image, or fragment history.

The engine commits the full body/evidence/intent state before invoking either
consensus signer, and commits publication state before callbacks, explicit
service, completed proposals and successful INIT. Runtime host scheduling,
access snapshots and network/Merkle attestations have corresponding publication
barriers. Already-published output can replay without flushing countdown-only
dirt. Only the explicit progress writer may defer countdown/rho changes; it
compares the entire descriptor after normalizing those fields and rejects any
authority, reference, activation, context or increasing countdown change.
Completed-body hash/canonical checks and Host validity remain separate.

A failed mutation, encoding or replacement poisons the adapter; a failed barrier
halts the engine before further output. Dirty flags clear only on successful
replacement. Drop does not flush: recovery discards the working copy and reloads
the backend, checking limits, identity, checksum and semantic publication
invariants before signing. Zebra's existing backend still adds its 40-byte
signer-identity envelope and commits via synced RocksDB separately from decided
chain rows. `MemoryStore` remains a logical test fixture, not durable storage;
physical power-loss guarantees depend on RocksDB and the filesystem/device.

Core roles are bounded independently of the number of elapsed rounds. The runtime
separates bounded application frame permits, per-tick receive work, queued host
requests and STP's message/reassembly/inbox/outbox limits. Moving a buffer into
STP transfers ownership; it is not an end-to-end delivery acknowledgement. These
component bounds are not a whole-node RSS bound or a bound on historical chain
storage. Memory saturation can cause drops without compromising agreement, but
termination still requires service of useful correct traffic; bounded capacity
alone does not prove freedom from starvation. Aggregate replay work is also not
bounded by the source's memory bound (`proof.md:994–1014`).
