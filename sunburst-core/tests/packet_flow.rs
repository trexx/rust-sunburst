// SPDX-License-Identifier: GPL-2.0-or-later

//! The authenticated receive path, end to end.
//!
//! Each piece is unit-tested in its own module; what this covers is the
//! *ordering*, which is where the subtlety lives. Verify the MAC, then the
//! replay window, then act — in that order, because checking the sequence first
//! lets unauthenticated traffic move the window.

use sunburst_core::proto::{
    Flags, GamepadState, Header, InputEvent, InputPacket, PacketType, ReplayWindow, Seq16,
    SessionKey,
    auth::NONCE_LEN,
    input::{MAX_INPUT_BODY, MAX_PADS},
};

const SECRET: &[u8] = b"secret established at pairing";

fn session() -> SessionKey {
    SessionKey::derive(SECRET, &[7; NONCE_LEN], &[9; NONCE_LEN])
}

/// Build a complete, signed input packet the way a client would.
fn send(key: &SessionKey, input_seq: u32, event: InputEvent) -> Vec<u8> {
    let header = Header {
        packet_type: PacketType::Input,
        flags: Flags::EMPTY,
        frame_id: Seq16(0),
        qpc_timestamp: 0,
        pkt_idx: 0,
        pkt_count: 1,
    };

    let mut packet = vec![0u8; sunburst_core::proto::HEADER_LEN];
    header.encode((&mut packet[..]).try_into().unwrap());

    let mut body = [0u8; MAX_INPUT_BODY];
    let n = InputPacket { input_seq, event }
        .encode(&mut body)
        .expect("body should encode");
    packet.extend_from_slice(&body[..n]);

    key.sign_packet(&mut packet);
    packet
}

/// Receive the way a server would, returning the event only if every check
/// passes.
fn receive(key: &SessionKey, window: &mut ReplayWindow, packet: &[u8]) -> Option<InputEvent> {
    let header = Header::decode(packet)?;
    if !header.packet_type.is_authenticated() {
        return None;
    }
    let body = key.verify_packet(packet)?;
    let input = InputPacket::decode(&body[sunburst_core::proto::HEADER_LEN..])?;
    if !window.accept(input.input_seq) {
        return None;
    }
    Some(input.event)
}

fn press(pad_index: u8) -> InputEvent {
    InputEvent::Gamepad(GamepadState {
        pad_index,
        buttons: sunburst_core::proto::input::buttons::A,
        ..Default::default()
    })
}

#[test]
fn a_well_formed_packet_is_accepted_once() {
    let key = session();
    let mut window = ReplayWindow::new();
    let packet = send(&key, 1, press(0));

    assert_eq!(receive(&key, &mut window, &packet), Some(press(0)));
    assert_eq!(
        receive(&key, &mut window, &packet),
        None,
        "the identical packet replayed must be refused"
    );
}

#[test]
fn a_packet_from_another_session_is_refused() {
    // The hole per-session keys close: `input_seq` restarts at zero every
    // session, so without re-keying a captured packet would land ahead of a
    // fresh window and verify perfectly.
    let yesterday = SessionKey::derive(SECRET, &[1; NONCE_LEN], &[2; NONCE_LEN]);
    let today = session();
    let captured = send(&yesterday, 1, press(0));

    let mut window = ReplayWindow::new();
    assert_eq!(receive(&today, &mut window, &captured), None);
}

#[test]
fn tampering_anywhere_in_the_packet_is_refused() {
    let key = session();
    let packet = send(&key, 1, press(0));

    for byte in 0..packet.len() {
        let mut tampered = packet.clone();
        tampered[byte] ^= 0x01;
        let mut window = ReplayWindow::new();
        assert_eq!(
            receive(&key, &mut window, &tampered),
            None,
            "flipping a bit in byte {byte} should have been caught"
        );
    }
}

#[test]
fn an_unauthenticated_packet_cannot_move_the_replay_window() {
    // The reason the MAC is verified before the sequence. If the window moved
    // first, anyone on the network could push it forward and make the real
    // client's next packets look like replays — a denial of service needing no
    // key at all.
    let key = session();
    let mut window = ReplayWindow::new();

    let forged = send(&SessionKey::from_bytes([0xAB; 32]), 10_000, press(0));
    assert_eq!(receive(&key, &mut window, &forged), None);

    // The genuine client, still at a low sequence, is unaffected.
    for seq in 1..5 {
        assert_eq!(
            receive(&key, &mut window, &send(&key, seq, press(0))),
            Some(press(0)),
            "sequence {seq} should still be accepted"
        );
    }
}

#[test]
fn reordered_input_is_delivered_but_not_duplicated() {
    let key = session();
    let mut window = ReplayWindow::new();

    let first = send(&key, 1, press(0));
    let second = send(&key, 2, press(1));

    assert_eq!(receive(&key, &mut window, &second), Some(press(1)));
    assert_eq!(
        receive(&key, &mut window, &first),
        Some(press(0)),
        "a late packet is not a replay"
    );
    assert_eq!(receive(&key, &mut window, &first), None, "but only once");
}

#[test]
fn a_truncated_packet_is_refused_rather_than_panicking() {
    let key = session();
    let packet = send(&key, 1, press(0));

    for len in 0..packet.len() {
        let mut window = ReplayWindow::new();
        assert_eq!(
            receive(&key, &mut window, &packet[..len]),
            None,
            "a {len}-byte packet should be refused"
        );
    }
}

#[test]
fn every_pad_the_server_can_plug_survives_the_round_trip() {
    let key = session();
    let mut window = ReplayWindow::new();
    for pad in 0..MAX_PADS {
        let packet = send(&key, u32::from(pad) + 1, press(pad));
        assert_eq!(receive(&key, &mut window, &packet), Some(press(pad)));
    }
}
