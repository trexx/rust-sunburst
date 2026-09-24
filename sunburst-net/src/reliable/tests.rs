// SPDX-License-Identifier: GPL-2.0-or-later

use super::*;

const MTU: usize = 1100;
const T0: u64 = 1_000;

fn pair() -> (Reliable, Reliable) {
    (Reliable::new(MTU), Reliable::new(MTU))
}

/// Deliver `frame` and assert exactly one message came out of it.
fn deliver_one(to: &mut Reliable, frame: &[u8]) -> Vec<u8> {
    let mut got = to.on_frame(frame);
    assert_eq!(got.len(), 1, "expected exactly one message");
    got.pop().expect("checked")
}

#[test]
fn a_message_arrives_and_is_acknowledged() {
    let (mut a, mut b) = pair();

    let frame = a.send(b"hello", T0).expect("send");
    assert_eq!(deliver_one(&mut b, &frame), b"hello");
    assert_eq!(a.outstanding(), 1, "not acked yet");

    // B has nothing to send, so its ack goes out on its own.
    let acks = b.tick(T0).expect("tick");
    assert_eq!(acks.len(), 1, "a bare ack was owed");
    a.on_frame(&acks[0]);
    assert!(a.is_idle(), "the ack should have retired it");
}

#[test]
fn an_ack_rides_along_with_a_reply() {
    // The common case: no bare ack needed, because there was something to say.
    let (mut a, mut b) = pair();
    let frame = a.send(b"request", T0).expect("send");
    b.on_frame(&frame);

    let reply = b.send(b"response", T0).expect("send");
    assert_eq!(deliver_one(&mut a, &reply), b"response");
    assert!(a.is_idle(), "the reply carried the ack");

    // And nothing bare is emitted afterwards.
    assert!(b.tick(T0).expect("tick").is_empty());
}

#[test]
fn a_lost_message_is_retransmitted_and_then_delivered() {
    let (mut a, mut b) = pair();
    let _lost = a.send(b"hello", T0).expect("send");

    assert!(a.tick(T0).expect("tick").is_empty(), "too early to resend");

    let resent = a.tick(T0 + RETRANSMIT_MS).expect("tick");
    assert_eq!(resent.len(), 1);
    assert_eq!(deliver_one(&mut b, &resent[0]), b"hello");
}

#[test]
fn a_lost_ack_causes_a_resend_that_is_not_delivered_twice() {
    // The duplicate this protocol will actually see: the message arrived, the
    // ack did not, so the sender tries again. Delivering it twice would run a
    // pairing step or a launch a second time.
    let (mut a, mut b) = pair();
    let frame = a.send(b"launch", T0).expect("send");
    assert_eq!(deliver_one(&mut b, &frame), b"launch");

    let _lost_ack = b.tick(T0).expect("tick");
    let resent = a.tick(T0 + RETRANSMIT_MS).expect("tick");
    assert_eq!(resent.len(), 1);

    assert!(
        b.on_frame(&resent[0]).is_empty(),
        "a duplicate must not be delivered again"
    );
    // But it is acked again, or the sender never stops.
    assert_eq!(b.tick(T0 + RETRANSMIT_MS).expect("tick").len(), 1);
}

#[test]
fn reordering_is_held_until_the_gap_fills() {
    // Why in-order matters here: a PairConfirm overtaking its PairRequest would
    // reach a handler with no pending request to attach to.
    let (mut a, mut b) = pair();
    let first = a.send(b"request", T0).expect("send");
    let second = a.send(b"confirm", T0).expect("send");

    assert!(
        b.on_frame(&second).is_empty(),
        "the second message must wait"
    );

    let delivered = b.on_frame(&first);
    assert_eq!(delivered.len(), 2, "both should arrive once the gap fills");
    assert_eq!(delivered[0], b"request");
    assert_eq!(delivered[1], b"confirm");
}

#[test]
fn a_run_of_messages_is_retired_by_one_cumulative_ack() {
    let (mut a, mut b) = pair();
    let mut frames = Vec::new();
    for i in 0..5u8 {
        frames.push(a.send(&[i], T0).expect("send"));
    }
    assert_eq!(a.outstanding(), 5);

    for frame in &frames {
        b.on_frame(frame);
    }
    let ack = b.tick(T0).expect("tick");
    a.on_frame(&ack[0]);
    assert!(a.is_idle(), "one cumulative ack should clear all five");
}

#[test]
fn the_send_window_bounds_what_is_in_flight() {
    let (mut a, _b) = pair();
    for i in 0..WINDOW {
        a.send(&[i as u8], T0).expect("within window");
    }
    assert_eq!(a.send(b"one too many", T0), Err(ReliableError::WouldBlock));
}

#[test]
fn the_window_reopens_once_something_is_acked() {
    let (mut a, mut b) = pair();
    let mut frames = Vec::new();
    for i in 0..WINDOW {
        frames.push(a.send(&[i as u8], T0).expect("send"));
    }
    assert!(a.send(b"blocked", T0).is_err());

    for frame in &frames {
        b.on_frame(frame);
    }
    a.on_frame(&b.tick(T0).expect("tick")[0]);
    a.send(b"now fits", T0).expect("window reopened");
}

#[test]
fn a_peer_far_ahead_of_the_window_is_refused() {
    // Otherwise a peer could hold sequence 0 back and stream far-future ones,
    // and the holding area would grow without bound.
    let (mut a, mut b) = pair();
    // Burn sequences without delivering any of them.
    for i in 0..WINDOW {
        a.send(&[i as u8], T0).expect("send");
    }
    let far = a.send(b"far", T0);
    assert!(far.is_err(), "sender is window-bound too");

    // Hand-build something well beyond the window.
    let mut frame = Vec::new();
    frame.extend_from_slice(&1000u16.to_le_bytes());
    frame.extend_from_slice(&u16::MAX.to_le_bytes());
    frame.push(FLAG_HAS_PAYLOAD);
    frame.extend_from_slice(b"way ahead");

    assert!(b.on_frame(&frame).is_empty());
    assert!(b.is_idle(), "nothing should have been retained");
}

#[test]
fn a_silent_peer_is_eventually_declared_gone() {
    let (mut a, _b) = pair();
    a.send(b"anyone there", T0).expect("send");

    let mut now = T0;
    for _ in 0..MAX_ATTEMPTS - 1 {
        now += RETRANSMIT_MS;
        a.tick(now).expect("still trying");
    }
    now += RETRANSMIT_MS;
    assert_eq!(a.tick(now), Err(ReliableError::PeerGone));
}

#[test]
fn a_retransmission_carries_the_current_ack() {
    // A resend rebuilt with a stale ack would make the peer resend things we
    // already have, and the two ends would chase each other.
    let (mut a, mut b) = pair();
    let from_a = a.send(b"one", T0).expect("send");
    b.on_frame(&from_a);

    // B sends something that A receives, so A's ack advances.
    let from_b = b.send(b"two", T0).expect("send");
    a.on_frame(&from_b);

    let resent = a.tick(T0 + RETRANSMIT_MS).expect("tick");
    assert_eq!(resent.len(), 1);
    let ack = u16::from_le_bytes([resent[0][2], resent[0][3]]);
    assert_eq!(ack, 0, "the resend should advertise B's sequence 0");
}

#[test]
fn sequences_wrap_without_stalling() {
    // 65536 control messages is a long session, but the comparison has to be
    // modular or delivery stops dead at the wrap rather than degrading.
    let (mut a, mut b) = pair();
    a.next_seq = Seq16(u16::MAX - 1);
    b.ack = Some(Seq16(u16::MAX - 2));

    for expected in [
        b"before".as_slice(),
        b"wrap".as_slice(),
        b"after".as_slice(),
    ] {
        let frame = a.send(expected, T0).expect("send");
        assert_eq!(deliver_one(&mut b, &frame), expected);
        a.on_frame(&b.tick(T0).expect("tick")[0]);
    }
    assert!(a.is_idle());
}

#[test]
fn an_oversized_message_is_refused_rather_than_fragmented() {
    // Fragmenting control messages is not in this layer. A message that will not
    // fit is a bug in the caller, and silently splitting it would hide that.
    let (mut a, _b) = pair();
    let big = vec![0u8; MTU + 1];
    assert_eq!(a.send(&big, T0), Err(ReliableError::TooLarge(MTU + 1)));
    a.send(&vec![0u8; MTU], T0).expect("exactly the limit fits");
}

#[test]
fn a_runt_frame_is_ignored_rather_than_panicking() {
    let (_a, mut b) = pair();
    for len in 0..FRAME_HEADER_LEN {
        assert!(b.on_frame(&vec![0u8; len]).is_empty(), "len {len}");
    }
}

#[test]
fn an_empty_payload_frame_delivers_nothing() {
    // A bare ack carries a sequence that has not been allocated yet. The flag is
    // what decides, so the receiver must not treat it as a message.
    let (mut a, mut b) = pair();
    a.send(b"first", T0).expect("send");
    let bare = a.tick(T0).expect("tick");
    assert!(bare.is_empty(), "nothing owed yet");

    b.on_frame(&a.send(b"second", T0).expect("send"));
    let ack = b.tick(T0).expect("tick");
    assert_eq!(ack.len(), 1);
    assert!(
        a.on_frame(&ack[0]).is_empty(),
        "a bare ack is not a message"
    );
}

#[test]
fn a_full_exchange_survives_loss_in_both_directions() {
    // Roughly the pairing sequence, with the first attempt in each direction
    // dropped on the floor.
    let (mut client, mut server) = pair();
    let mut now = T0;

    let request = client.send(b"PairRequest", now).expect("send");
    drop(request); // lost

    now += RETRANSMIT_MS;
    let retry = client.tick(now).expect("tick");
    assert_eq!(deliver_one(&mut server, &retry[0]), b"PairRequest");

    let challenge = server.send(b"PairChallenge", now).expect("send");
    drop(challenge); // also lost

    now += RETRANSMIT_MS;
    let retry = server.tick(now).expect("tick");
    assert_eq!(deliver_one(&mut client, &retry[0]), b"PairChallenge");

    let confirm = client.send(b"PairConfirm", now).expect("send");
    assert_eq!(deliver_one(&mut server, &confirm), b"PairConfirm");

    // Both ends settle.
    for _ in 0..3 {
        for frame in server.tick(now).expect("tick") {
            client.on_frame(&frame);
        }
        for frame in client.tick(now).expect("tick") {
            server.on_frame(&frame);
        }
    }
    assert!(
        client.is_idle(),
        "client still has {} in flight",
        client.outstanding()
    );
    assert!(
        server.is_idle(),
        "server still has {} in flight",
        server.outstanding()
    );
}

#[test]
fn epochs_increase_across_constructions() {
    // Two channels built back to back, in the same millisecond more often than
    // not, still order: that is what lets a peer tell a restart from a replay.
    let first = Reliable::new(MTU).epoch();
    let second = Reliable::new(MTU).epoch();
    assert!(second > first, "{second} is not after {first}");
}

#[test]
fn a_restarted_peer_is_newer_and_its_old_self_is_stale() {
    let mut server = Reliable::with_epoch(MTU, 1);
    let mut old = Reliable::with_epoch(MTU, 10);
    let mut new = Reliable::with_epoch(MTU, 20);

    let hello = old.send(b"hello", T0).expect("send");
    assert_eq!(server.incarnation(&hello), Some(Incarnation::First));
    assert_eq!(deliver_one(&mut server, &hello), b"hello");
    assert_eq!(server.incarnation(&hello), Some(Incarnation::Same));

    let again = new.send(b"hello again", T0).expect("send");
    assert_eq!(server.incarnation(&again), Some(Incarnation::Newer(20)));
    assert!(server.incarnation(&[0; 4]).is_none(), "a runt has no epoch");

    server.restart(20);
    assert_eq!(server.incarnation(&hello), Some(Incarnation::Stale));
    assert_eq!(server.incarnation(&again), Some(Incarnation::Same));
}

#[test]
fn without_a_restart_a_new_incarnation_reads_as_a_duplicate() {
    // The bug the epoch exists for. A fresh peer numbers from zero, the old
    // ack says zero was delivered, so the new peer's first message is acked and
    // never handed up — and the ack retires it on the peer, so it is never
    // resent either.
    let mut server = Reliable::with_epoch(MTU, 1);
    let mut old = Reliable::with_epoch(MTU, 10);
    deliver_one(&mut server, &old.send(b"hello", T0).expect("send"));

    let mut new = Reliable::with_epoch(MTU, 20);
    let lost = new.send(b"hello again", T0).expect("send");
    assert!(server.on_frame(&lost).is_empty());
}

#[test]
fn a_restart_delivers_the_new_incarnation_from_zero_both_ways() {
    let mut server = Reliable::with_epoch(MTU, 1);
    let mut old = Reliable::with_epoch(MTU, 10);
    for i in 0..3u8 {
        deliver_one(&mut server, &old.send(&[i], T0).expect("send"));
    }
    // The server had something in flight to the old incarnation, too.
    server.send(b"for the old one", T0).expect("send");

    let mut new = Reliable::with_epoch(MTU, 20);
    let hello = new.send(b"hello again", T0).expect("send");
    let Some(Incarnation::Newer(epoch)) = server.incarnation(&hello) else {
        panic!("not newer");
    };
    server.restart(epoch);
    assert_eq!(deliver_one(&mut server, &hello), b"hello again");
    assert_eq!(
        server.outstanding(),
        0,
        "the old incarnation's frame is forgotten"
    );

    // And the server's replies start at zero, which is what the new peer wants.
    let reply = server.send(b"welcome", T0).expect("send");
    assert_eq!(deliver_one(&mut new, &reply), b"welcome");
    assert!(new.is_idle(), "the reply carried the ack");
}

#[test]
fn a_stale_frame_is_dropped_without_an_ack() {
    let mut server = Reliable::with_epoch(MTU, 1);
    let mut old = Reliable::with_epoch(MTU, 10);
    let captured = old.send(b"hello", T0).expect("send");
    deliver_one(&mut server, &captured);
    server.tick(T0).expect("tick");

    let mut new = Reliable::with_epoch(MTU, 20);
    let hello = new.send(b"hello again", T0).expect("send");
    server.restart(20);
    deliver_one(&mut server, &hello);
    server.tick(T0).expect("tick");

    // The capture, replayed after the restart, is neither delivered nor acked.
    assert!(server.on_frame(&captured).is_empty());
    assert!(
        server.tick(T0).expect("tick").is_empty(),
        "no ack for a stale frame"
    );
}

#[test]
fn take_ack_hands_over_the_owed_ack_once() {
    let (mut a, mut b) = pair();
    assert!(b.take_ack().is_none(), "nothing arrived, nothing owed");
    b.on_frame(&a.send(b"bye", T0).expect("send"));

    let ack = b.take_ack().expect("an ack is owed");
    a.on_frame(&ack);
    assert!(a.is_idle(), "the ack retired the message");
    assert!(b.take_ack().is_none(), "and it is not owed twice");
    assert!(b.tick(T0).expect("tick").is_empty());
}
