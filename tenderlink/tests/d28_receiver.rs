//! Receiver-selected evidence scheduling through the public API and real engines.
//! Logical requests and host-owned complete frames are accounted independently.
use ed25519_zebra::{SigningKey, VerificationKeyBytes};
use tenderlink::d28::{gossip::receiver::*, gossip::*, *};

const BODY: &[u8] = b"receiver-selected complete evidence";

fn key(i: usize) -> SigningKey {
    SigningKey::from([i as u8 + 1; 32])
}

fn config() -> Config {
    Config::new(
        [42; 32],
        (0..4)
            .map(|i| Member {
                public_key: VerificationKeyBytes::from(&key(i)).into(),
                weight: 1,
            })
            .collect(),
        1024,
        3,
        Timeouts {
            propose: 30,
            prevote: 15,
            precommit: 25,
            inc_propose: 5,
            inc_prevote: 1,
            inc_precommit: 3,
        },
    )
    .unwrap()
}

// Host peer handles must never be interpreted as member indices.
fn peer(i: usize) -> u32 {
    1009 + 17 * i as u32
}

#[derive(Clone, Debug)]
struct Packet {
    target: Target,
    message: Message,
    outer: Option<Vote>,
    proposal: Option<(Header, Vec<u8>)>,
    groups: Vec<(CertKind, Vec<Vote>)>,
}

impl Packet {
    fn deliver(
        &self,
        engine: &mut Engine<MemoryStore>,
        validity: Status,
        out: &mut Recorder,
    ) -> Result<()> {
        let groups: Vec<_> = self
            .groups
            .iter()
            .map(|(kind, votes)| Group {
                kind: *kind,
                votes: Votes::Dense(votes),
            })
            .collect();
        engine.receive(
            Frame {
                message: self.message,
                outer: self.outer.as_ref(),
                proposal: self
                    .proposal
                    .as_ref()
                    .map(|(header, body)| Proposal { header, body }),
                groups: &groups,
            },
            validity,
            out,
        )
    }
}

#[derive(Default)]
struct Recorder {
    packets: Vec<Packet>,
    decisions: Vec<(u64, Id)>,
}

impl Sink for Recorder {
    fn send(&mut self, target: Target, frame: Frame<'_>) {
        self.packets.push(Packet {
            target,
            message: frame.message,
            outer: frame.outer.copied(),
            proposal: frame.proposal.map(|p| (*p.header, p.body.to_vec())),
            groups: frame
                .groups
                .iter()
                .map(|g| (g.kind, g.votes.iter().copied().collect()))
                .collect(),
        });
    }

    fn decided(&mut self, height: u64, proposal: Proposal<'_>, certificate: Group<'_>) {
        assert_eq!(
            certificate.kind,
            CertKind::Decision {
                round: proposal.header.content.round,
                id: proposal.header.content.id,
            }
        );
        self.decisions.push((height, proposal.header.content.id));
    }
}

fn engine(c: &Config, member: usize) -> (Engine<MemoryStore>, Recorder) {
    let mut engine = Engine::boot(MemoryStore::default(), key(member), c.clone()).unwrap();
    engine.set_emission(Emission::Guided);
    let mut out = Recorder::default();
    assert!(engine.init(c, 0, 0, &mut out).unwrap());
    out.packets.clear();
    (engine, out)
}

fn decision_packet(c: &Config) -> Packet {
    let id = body_id(BODY);
    let signer = c.proposer(0, 0);
    let header = Header::sign(
        c,
        &key(signer),
        ProposalContent {
            height: 0,
            round: 0,
            valid_round: None,
            id,
            signer: signer as u32,
        },
    );
    let votes = (0..3)
        .map(|signer| {
            Vote::sign(
                c,
                &key(signer),
                VoteContent {
                    height: 0,
                    round: 0,
                    kind: VoteKind::Precommit,
                    id: Some(id),
                    signer: signer as u32,
                },
            )
        })
        .collect();
    Packet {
        target: Target::Peer(peer(3)),
        message: Message::Dec,
        outer: None,
        proposal: Some((header, BODY.to_vec())),
        groups: vec![(CertKind::Decision { round: 0, id }, votes)],
    }
}

fn decided(c: &Config, member: usize) -> (Engine<MemoryStore>, Recorder) {
    let (mut engine, mut out) = engine(c, member);
    decision_packet(c)
        .deliver(&mut engine, Status::Valid, &mut out)
        .unwrap();
    assert_eq!(out.decisions, [(0, body_id(BODY))]);
    out.packets.clear();
    (engine, out)
}

fn update(peer: u32, generation: u64, sequence: u64, status: StatusSnapshot) -> StatusUpdate {
    StatusUpdate {
        peer,
        generation,
        sequence,
        status,
    }
}

#[test]
fn complete_evidence_still_needs_core_validation_and_external_validity() {
    let c = config();
    let (mut source, mut sent) = decided(&c, 0);
    let (receiver, _) = engine(&c, 3);
    assert_eq!(
        source
            .serve(peer(3), &receiver.status().unwrap(), &mut sent)
            .unwrap(),
        Served::Sent(Gap::Decision)
    );
    assert_eq!(sent.packets.len(), 1);
    let complete = sent.packets.pop().unwrap();
    assert_eq!(complete.target, Target::Peer(peer(3)));

    for mutation in 0..8 {
        let (mut receiver, mut out) = engine(&c, 3);
        let before = receiver.view().unwrap().clone();
        let mut packet = complete.clone();
        match mutation {
            0 => packet.groups[0].1[0].signature[0] ^= 1,
            1 => packet.groups[0].1[1] = packet.groups[0].1[0],
            2 => {
                packet.groups[0].1.pop(); // Two of four is not a quorum.
            }
            3 => {
                packet.groups[0].0 = CertKind::Decision {
                    round: 0,
                    id: [7; 32],
                };
            }
            4 => packet.proposal.as_mut().unwrap().1[0] ^= 1,
            5 => packet.proposal.as_mut().unwrap().0.signature[0] ^= 1,
            6 => packet
                .proposal
                .as_mut()
                .unwrap()
                .1
                .resize(c.max_body() + 1, 0),
            7 => {
                let vote = packet.groups[0].1[0];
                packet.groups[0].1.resize(5, vote); // N+1 physical group rows.
            }
            _ => unreachable!(),
        }
        assert_eq!(
            packet.deliver(&mut receiver, Status::Valid, &mut out),
            Err(Error::Invalid),
            "mutation={mutation}"
        );
        assert_eq!(receiver.view(), Some(&before), "mutation={mutation}");
        assert!(receiver.prevotes().iter().all(Option::is_none));
        assert!(receiver.precommits().iter().all(Option::is_none));
        assert!(out.decisions.is_empty());
        assert!(out.packets.is_empty());
    }

    let (mut receiver, mut out) = engine(&c, 3);
    complete
        .deliver(&mut receiver, Status::Indeterminate, &mut out)
        .unwrap();
    assert!(out.decisions.is_empty());
    assert_ne!(receiver.status().unwrap().step, Step::Decided);
    let pending = receiver.view().unwrap().pending.unwrap();
    let tag = receiver.body_tag(pending.proposal);
    receiver.validity(tag, Status::Valid, &mut out).unwrap();
    assert_eq!(out.decisions, [(0, body_id(BODY))]);
    assert_eq!(receiver.status().unwrap().step, Step::Decided);
    assert!(out.packets.iter().all(|p| {
        p.proposal.is_none() && p.groups.is_empty() && p.message == Message::Ordinary
    }));
}

fn receiver_config(n: usize, k: usize) -> ReceiverConfig {
    ReceiverConfig {
        max_peers: n,
        max_requests: k,
        request_timeout: 10,
        status_ttl: 1000,
    }
}

fn sender_config(n: usize, k: usize) -> SenderConfig {
    SenderConfig {
        max_peers: n,
        max_outgoing: k,
        request_timeout: 10,
        status_ttl: 1000,
    }
}

fn request_status(
    peer: u32,
    generation: u64,
    sequence: u64,
    request: Option<u64>,
    status: StatusSnapshot,
) -> RequestStatus {
    RequestStatus {
        update: update(peer, generation, sequence, status),
        request,
    }
}

struct OwnedReply {
    key: RequestKey,
    packet: Packet,
    _permit: Permit,
}

#[derive(Default)]
struct Replies {
    calls: Vec<RequestKey>,
    owned: Vec<OwnedReply>,
    refuse: bool,
}

impl ReplySink for Replies {
    fn send(
        &mut self,
        key: RequestKey,
        frame: Frame<'_>,
        permit: Permit,
    ) -> std::result::Result<(), Permit> {
        self.calls.push(key);
        if self.refuse {
            return Err(permit);
        }
        // A host copies only after obtaining the physical TX ownership permit.
        let mut recorder = Recorder::default();
        recorder.send(Target::Peer(key.peer), frame);
        self.owned.push(OwnedReply {
            key,
            packet: recorder.packets.pop().unwrap(),
            _permit: permit,
        });
        Ok(())
    }
}

#[test]
fn older_closed_origin_cannot_reopen_a_newer_already_offered_request() {
    let c = config();
    let (mut source, _) = decided(&c, 0);
    let (receiver, _) = engine(&c, 3);
    let status = receiver.status().unwrap();
    let mut responder = Responder::try_new(sender_config(2, 1)).unwrap();
    let mut replies = Replies::default();
    let put = |sender: &mut Responder, sequence, request, now| {
        sender
            .update(request_status(peer(3), 1, sequence, request, status), now)
            .unwrap();
    };

    put(&mut responder, 10, None, 0);
    put(&mut responder, 20, Some(20), 0);
    let offered = responder.poll(&mut source, 0, &mut replies).unwrap();
    assert_eq!(offered.len(), 1);
    assert_eq!(offered[0].0.sequence, 20);
    assert_eq!(replies.calls.len(), 1);
    assert_eq!(responder.outstanding(), 1);

    // This newer status is authoritative control even though its origin is old.
    // Retiring token 20 must tombstone 20, not only the already-closed token 9.
    put(&mut responder, 21, Some(9), 1);
    assert_eq!(responder.active_requests(), 0);
    assert_eq!(
        responder.outstanding(),
        1,
        "cancellation is not TX retirement"
    );
    put(&mut responder, 22, Some(20), 2);
    assert!(
        responder
            .poll(&mut source, 2, &mut replies)
            .unwrap()
            .is_empty()
    );
    replies.owned.clear();
    assert_eq!(responder.outstanding(), 0);
    // Remove the capacity obstruction: a duplicate must still not be offered.
    put(&mut responder, 23, Some(20), 3);
    assert!(
        responder
            .poll(&mut source, 3, &mut replies)
            .unwrap()
            .is_empty()
    );
    assert_eq!(replies.calls.len(), 1, "token 20 was offered twice");
    assert_eq!(responder.active_requests(), 0);

    put(&mut responder, 24, Some(24), 4);
    let fresh = responder.poll(&mut source, 4, &mut replies).unwrap();
    assert_eq!(fresh.len(), 1);
    assert_eq!(fresh[0].0.sequence, 24);
    assert_eq!(replies.calls.len(), 2);
}

#[test]
fn many_holders_send_only_to_fixed_receiver_nominations() {
    let c = config();
    let (mut source, _) = decided(&c, 0);
    let (receiver, _) = engine(&c, 3);
    let local = receiver.status().unwrap();
    for k in [1, 2] {
        let mut chooser = Receiver::try_new(receiver_config(16, k)).unwrap();
        for i in 10..26 {
            chooser
                .update(update(peer(i), 1, 1, source.status().unwrap()), 0)
                .unwrap();
        }
        assert_eq!(
            chooser.update(update(peer(26), 1, 1, source.status().unwrap()), 0),
            Err(UpdateError::Full)
        );
        let selections = chooser.poll(local, 1, 0).unwrap();
        assert_eq!(selections.len(), k);
        assert_eq!(chooser.active_requests(), k);
        assert_eq!(chooser.requests().count(), k);
        let mut replies = Replies::default();
        for i in 10..26 {
            let mut sender = Responder::try_new(sender_config(1, 1)).unwrap();
            let request = chooser.request_for(peer(i));
            sender
                .update(request_status(peer(3), 1, 1, request, local), 0)
                .unwrap();
            let sent = sender.poll(&mut source, 0, &mut replies).unwrap();
            assert_eq!(sent.len(), usize::from(request.is_some()));
            assert_eq!(sender.outstanding(), usize::from(request.is_some()));
        }
        assert_eq!(replies.calls.len(), k, "not N independent complete offers");
        assert_eq!(replies.owned.len(), k);
        assert!(replies.owned.iter().all(|p| {
            p.key.peer == peer(3)
                && p.packet.target == Target::Peer(peer(3))
                && p.packet.message == Message::Dec
        }));
        assert_eq!(
            replies
                .owned
                .iter()
                .map(|p| p.packet.proposal.as_ref().unwrap().1.len())
                .sum::<usize>(),
            k * BODY.len()
        );
        let budget = AdmissionBudget::new(k);
        let held: Vec<_> = (0..k).map(|_| budget.try_acquire().unwrap()).collect();
        assert!(budget.try_acquire().is_none());
        assert_eq!(
            (budget.capacity(), budget.in_use(), budget.high_water()),
            (k, k, k)
        );
        drop(held);
        assert_eq!(budget.in_use(), 0);
    }
}

#[test]
fn receiver_heartbeats_keep_origin_and_deadline_with_backwards_clock() {
    let c = config();
    let (source, _) = decided(&c, 0);
    let (receiver, _) = engine(&c, 3);
    let local = receiver.status().unwrap();
    let mut chooser = Receiver::try_new(receiver_config(1, 1)).unwrap();
    chooser
        .update(update(peer(0), 7, 1, source.status().unwrap()), 5)
        .unwrap();
    let old = chooser.poll(local, 10, 5).unwrap()[0];
    assert_eq!(old.sequence, 10);
    assert_eq!(old.generation, 7);
    for (sequence, now) in [(11, 0), (12, 5), (13, 14)] {
        assert!(chooser.poll(local, sequence, now).unwrap().is_empty());
        assert_eq!(chooser.request_for(old.peer), Some(10));
        assert!(chooser.is_current(old, now));
    }
    // A fresh peer status does not renew the local request's original t=15 end.
    chooser
        .update(update(peer(0), 7, 2, source.status().unwrap()), 14)
        .unwrap();
    assert_eq!(
        chooser.update(update(peer(0), 7, 1, local), 14),
        Err(UpdateError::Stale)
    );
    assert!(chooser.poll(local, 14, 15).unwrap().is_empty());
    assert!(!chooser.is_current(old, 15));
    assert_eq!(chooser.request_for(old.peer), None);
    assert!(chooser.poll(local, 15, 0).unwrap().is_empty());
    assert_eq!(chooser.poll(local, 15, 0), Err(Error::Stale));
    assert_eq!(chooser.poll(local, 14, 0), Err(Error::Stale));
    assert!(
        !chooser.is_current(old, 0),
        "backwards host time cannot revive an expired key"
    );
}

#[test]
fn responder_heartbeats_use_latest_status_without_reset_or_duplicate_offer() {
    let c = config();
    let (mut source, _) = decided(&c, 0);
    let (receiver, _) = engine(&c, 3);
    let local = receiver.status().unwrap();
    let mut sender = Responder::try_new(sender_config(1, 1)).unwrap();
    let mut replies = Replies::default();
    // No response at first: this requester claims the decision already.
    sender
        .update(
            request_status(peer(3), 1, 10, Some(10), source.status().unwrap()),
            0,
        )
        .unwrap();
    assert!(
        sender
            .poll(&mut source, 0, &mut replies)
            .unwrap()
            .is_empty()
    );
    // Same origin, but a newer semantic snapshot exposes the real gap.
    sender
        .update(request_status(peer(3), 1, 11, Some(10), local), 1)
        .unwrap();
    assert_eq!(sender.poll(&mut source, 1, &mut replies).unwrap().len(), 1);
    assert_eq!(replies.owned[0].key.sequence, 10);
    replies.owned.clear();
    for sequence in 12..20 {
        sender
            .update(request_status(peer(3), 1, sequence, Some(10), local), 9)
            .unwrap();
        assert!(
            sender
                .poll(&mut source, 9, &mut replies)
                .unwrap()
                .is_empty()
        );
    }
    assert_eq!(
        replies.calls.len(),
        1,
        "same-token heartbeat is not another offer"
    );
    assert!(
        sender
            .poll(&mut source, 10, &mut replies)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        sender.active_requests(),
        0,
        "heartbeats cannot extend origin deadline"
    );
    sender
        .update(request_status(peer(3), 1, 20, Some(10), local), 10)
        .unwrap();
    assert!(
        sender
            .poll(&mut source, 10, &mut replies)
            .unwrap()
            .is_empty()
    );
    sender
        .update(request_status(peer(3), 1, 21, Some(21), local), 10)
        .unwrap();
    assert_eq!(sender.poll(&mut source, 10, &mut replies).unwrap().len(), 1);
    assert_eq!(
        replies.owned[0].key.sequence, 21,
        "loss requires a fresh attempt"
    );
}

#[test]
fn cancelled_receive_ownership_survives_reconnect_and_late_evidence_is_admissible() {
    let c = config();
    let (source, _) = decided(&c, 0);
    let (mut engine, mut out) = engine(&c, 3);
    let local = engine.status().unwrap();
    let mut chooser = Receiver::try_new(receiver_config(1, 1)).unwrap();
    chooser
        .update(update(peer(0), 1, 1, source.status().unwrap()), 0)
        .unwrap();
    let old = chooser.poll(local, 1, 0).unwrap()[0];
    let budget = AdmissionBudget::new(1);
    let other_lane = budget.clone();
    // Allocation happens after admission, with the permit retained by its owner.
    let permit = budget.try_acquire().unwrap();
    let old_owned = (old, decision_packet(&c), permit);
    chooser.remove(peer(0));
    chooser
        .update(update(peer(0), 2, 0, source.status().unwrap()), 1)
        .unwrap();
    let fresh = chooser.poll(local, 2, 1).unwrap()[0];
    assert_ne!(old, fresh);
    assert!(!chooser.is_current(old, 1));
    assert!(chooser.is_current(fresh, 1));
    assert!(!chooser.is_current(
        RequestKey {
            peer: peer(99),
            ..fresh
        },
        1
    ));
    assert_eq!(
        chooser.update(update(peer(0), 1, 99, local), 1),
        Err(UpdateError::Stale)
    );
    assert_eq!(
        (budget.capacity(), budget.in_use(), budget.high_water()),
        (1, 1, 1)
    );
    assert!(
        other_lane.try_acquire().is_none(),
        "logical replacement cannot recredit RX ownership"
    );
    drop(old_owned);
    assert_eq!(budget.in_use(), 0);
    assert_eq!(chooser.request_for(fresh.peer), Some(fresh.sequence));

    // Host session authentication is mandatory and separate from request tokens.
    let current_session =
        |key: RequestKey| (key.peer, key.generation) == (fresh.peer, fresh.generation);
    assert!(
        !current_session(old),
        "old transport-session traffic is discarded"
    );
    // On the current authenticated session, an old echoed request origin is not
    // an evidence ban. Rebind the echo to that session, not its retired one.
    let late_key = RequestKey {
        sequence: old.sequence,
        ..fresh
    };
    assert!(current_session(late_key));
    let late_permit = other_lane.try_acquire().unwrap();
    let late = (late_key, decision_packet(&c), late_permit);
    assert!(!chooser.is_current(late.0, 1));
    if current_session(late.0) {
        late.1
            .deliver(&mut engine, Status::Valid, &mut out)
            .unwrap();
    }
    assert_eq!(out.decisions, [(0, body_id(BODY))]);
    assert_eq!(
        chooser.request_for(fresh.peer),
        Some(fresh.sequence),
        "receive has no request-ACK side effect"
    );
    chooser.poll(engine.status().unwrap(), 3, 1).unwrap();
    assert_eq!(
        chooser.active_requests(),
        0,
        "only the new local semantic fact retires the request"
    );
    drop(chooser);
    let replacement = Receiver::try_new(receiver_config(1, 1)).unwrap();
    assert_eq!(replacement.active_requests(), 0);
    assert_eq!(
        budget.in_use(),
        1,
        "scheduler replacement cannot release a live frame"
    );
    assert!(budget.try_acquire().is_none());
    drop(late);
    assert_eq!((budget.in_use(), budget.high_water()), (0, 1));
}

#[test]
fn cancelled_transmit_ownership_blocks_refill_across_fresh_generation() {
    let c = config();
    let (mut source, _) = decided(&c, 0);
    let (receiver, _) = engine(&c, 3);
    let local = receiver.status().unwrap();
    let mut sender = Responder::try_new(sender_config(1, 1)).unwrap();
    let mut replies = Replies::default();
    sender
        .update(request_status(peer(3), 1, 1, Some(1), local), 0)
        .unwrap();
    assert_eq!(sender.poll(&mut source, 0, &mut replies).unwrap().len(), 1);
    let old_key = replies.owned[0].key;
    sender
        .update(request_status(peer(3), 1, 2, None, local), 1)
        .unwrap();
    assert_eq!(sender.active_requests(), 0);
    assert_eq!(sender.outstanding(), 1);
    assert_eq!(
        sender.update(request_status(peer(3), 1, 1, Some(1), local), 1),
        Err(RequestUpdateError::Stale)
    );
    sender
        .update(request_status(peer(3), 1, 3, Some(3), local), 1)
        .unwrap();
    assert!(
        sender
            .poll(&mut source, 1, &mut replies)
            .unwrap()
            .is_empty()
    );
    sender.remove(peer(3));
    assert_eq!(sender.peers(), 0);
    assert_eq!(sender.outstanding(), 1);
    sender
        .update(request_status(peer(3), 2, 0, Some(0), local), 1)
        .unwrap();
    assert_eq!(sender.active_requests(), 1);
    assert_eq!(
        sender.update(request_status(peer(3), 1, 99, None, local), 1),
        Err(RequestUpdateError::Stale)
    );
    assert!(
        sender
            .poll(&mut source, 1, &mut replies)
            .unwrap()
            .is_empty()
    );
    assert_eq!(replies.calls, [old_key]);
    replies.owned.clear();
    assert_eq!(sender.outstanding(), 0);
    assert_eq!(sender.poll(&mut source, 1, &mut replies).unwrap().len(), 1);
    assert_eq!(sender.outstanding(), 1);
    let new_key = replies.owned[0].key;
    assert_eq!(
        (new_key.peer, new_key.generation, new_key.sequence),
        (peer(3), 2, 0)
    );
    assert_ne!(old_key, new_key);
    // Old generation control cannot retire the new logical or physical owner.
    assert_eq!(
        sender.update(request_status(peer(3), 1, 100, None, local), 1),
        Err(RequestUpdateError::Stale)
    );
    assert_eq!((sender.active_requests(), sender.outstanding()), (1, 1));
    replies.owned.clear();
    assert_eq!(sender.outstanding(), 0);
}

#[test]
fn latest_request_snapshot_serves_current_polka_without_asserting_body_validity() {
    let c = config();
    let id = body_id(BODY);
    let (mut source, mut source_out) = engine(&c, 3);
    let mut proposal = decision_packet(&c);
    proposal.message = Message::Ordinary;
    proposal.groups.clear();
    proposal
        .deliver(&mut source, Status::Valid, &mut source_out)
        .unwrap();
    // A quorum of OTHER members: the receiving member (2) has not voted yet.
    let votes = [0, 1, 3]
        .into_iter()
        .map(|signer| {
            Vote::sign(
                &c,
                &key(signer),
                VoteContent {
                    height: 0,
                    round: 0,
                    kind: VoteKind::Prevote,
                    id: Some(id),
                    signer: signer as u32,
                },
            )
        })
        .collect();
    let polka = Packet {
        target: Target::Peer(peer(3)),
        message: Message::Ordinary,
        outer: None,
        proposal: None,
        groups: vec![(CertKind::Polka { round: 0, id }, votes)],
    };
    polka
        .deliver(&mut source, Status::Valid, &mut source_out)
        .unwrap();
    assert_eq!(source.status().unwrap().polka, Some((0, id)));

    let (mut receiver, mut out) = engine(&c, 2);
    let mut sender = Responder::try_new(sender_config(1, 1)).unwrap();
    let mut replies = Replies::default();
    // The origin's snapshot needs nothing. A newer snapshot on the SAME token
    // exposes a missing polka: the sender must not keep using the origin state.
    sender
        .update(
            request_status(peer(2), 1, 10, Some(10), source.status().unwrap()),
            0,
        )
        .unwrap();
    assert!(
        sender
            .poll(&mut source, 0, &mut replies)
            .unwrap()
            .is_empty()
    );
    sender
        .update(
            request_status(peer(2), 1, 11, Some(10), receiver.status().unwrap()),
            1,
        )
        .unwrap();
    let sent = sender.poll(&mut source, 1, &mut replies).unwrap();
    assert_eq!(sent.len(), 1);
    assert_eq!((sent[0].0.sequence, sent[0].1), (10, Gap::Polka));
    assert_eq!(replies.owned.len(), 1);
    let packet = &replies.owned[0].packet;
    assert!(packet.proposal.is_some());
    assert!(
        packet
            .groups
            .iter()
            .any(|(kind, _)| *kind == CertKind::Polka { round: 0, id })
    );

    let rx = AdmissionBudget::new(1);
    let permit = rx.try_acquire().unwrap();
    packet
        .deliver(&mut receiver, Status::Indeterminate, &mut out)
        .unwrap();
    assert!(out.decisions.is_empty());
    assert!(out.packets.iter().all(|p| {
        !p.outer
            .is_some_and(|v| v.content.kind == VoteKind::Precommit && v.content.id.is_some())
    }));
    let slot = receiver
        .view()
        .unwrap()
        .slot
        .expect("retained pending body");
    assert_eq!(
        receiver.view().unwrap().statuses[slot.index],
        Status::Indeterminate
    );
    let tag = receiver.body_tag(slot);
    receiver.validity(tag, Status::Valid, &mut out).unwrap();
    assert_eq!(receiver.view().unwrap().locked_id, Some(id));
    assert!(out.packets.iter().any(|p| {
        p.outer
            .is_some_and(|v| v.content.kind == VoteKind::Precommit && v.content.id == Some(id))
    }));
    assert!(
        out.decisions.is_empty(),
        "a polka is not a decision certificate"
    );
    drop(permit);
    assert_eq!((rx.in_use(), rx.high_water()), (0, 1));
    replies.owned.clear();
    assert_eq!(
        sender.outstanding(),
        0,
        "TX capacity must not mask token replay"
    );
    assert!(
        sender
            .poll(&mut source, 2, &mut replies)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        replies.calls.len(),
        1,
        "no second accepted handoff on one origin"
    );
}
