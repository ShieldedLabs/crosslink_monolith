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
candidate. Keep weight-blind proposer rotation, so each member is proposer once
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
to the protocol, such that for each correct ordered pair (p,q), each of the four
lanes, and each interval [u,u+H] wholly inside the service interval: if that lane
is available throughout the interval, its periodic offers yield at least one
frame OFFERED after the start boundary u and processed by q no later than u+H.
An offer at the same logical timestamp counts only if it follows that boundary
in execution order; an earlier same-time queued copy does not count. For the
three round-relative lanes the interval must lie wholly within that availability
context r. For Progress the frame version and sender round may change during
the interval. Endpoint transitions may be assigned to either adjacent version;
all comparisons protecting round-local evidence below are strict.

The arbitration key is (authenticated sender, destination, lane), NOT a round,
ENTRY region, certificate digest, proposal-authorization form or frame hash.
Each broadcast has this contract separately for each correct destination.
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
and validity premises only for members of C_fast, and BPS only for ordered
pairs within C_fast. Members outside C_fast need not be online or timely and
may keep arbitrary stale locks. Retain the full roster's N, weights,
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
current proof does not certify such an optimization or its memory bound.

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
