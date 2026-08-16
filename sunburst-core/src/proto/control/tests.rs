// SPDX-License-Identifier: GPL-2.0-or-later

use super::*;

fn quirks() -> DecoderQuirks {
    DecoderQuirks {
        ref_invalidation: true,
        intra_refresh: false,
        slice_output: true,
        needs_annexb_startcodes: false,
        max_bitrate_hint: 150_000_000,
    }
}

fn pair_request() -> ClientControl {
    ClientControl::PairRequest(PairRequest {
        name: "Living room".into(),
        model: "SHIELD Android TV".into(),
        abi: "arm64-v8a".into(),
        quirks: quirks(),
        client_nonce: [7; NONCE_LEN],
    })
}

fn hello() -> ClientControl {
    ClientControl::Hello(Hello {
        client_id: 3,
        name: "Living room".into(),
        abi: "arm64-v8a".into(),
        width: 3840,
        height: 2160,
        refresh_mhz: 59_940,
        client_nonce: [9; NONCE_LEN],
        clock_offset_ns: -1_234_567,
    })
}

fn round_trip_client(message: ClientControl) {
    let encoded = message.encode().expect("encode");
    let (decoded, used) = ClientControl::decode(&encoded).expect("decode");
    assert_eq!(decoded, message);
    assert_eq!(
        used,
        encoded.len(),
        "envelope length disagreed with the body"
    );
}

fn round_trip_server(message: ServerControl) {
    let encoded = message.encode().expect("encode");
    let (decoded, used) = ServerControl::decode(&encoded).expect("decode");
    assert_eq!(decoded, message);
    assert_eq!(
        used,
        encoded.len(),
        "envelope length disagreed with the body"
    );
}

#[test]
fn every_implemented_client_message_round_trips() {
    round_trip_client(pair_request());
    round_trip_client(ClientControl::PairConfirm {
        request_id: 42,
        tag: [0xAB; TAG_LEN],
    });
    round_trip_client(hello());
    round_trip_client(ClientControl::Quirks(quirks()));
    round_trip_client(ClientControl::ListApps);
    round_trip_client(ClientControl::LaunchApp { app_id: 7 });
    round_trip_client(ClientControl::RequestIdr);
    round_trip_client(ClientControl::Resize {
        width: 1920,
        height: 1080,
        refresh_mhz: 120_000,
    });
    round_trip_client(ClientControl::PadConnected {
        pad_index: 3,
        pad_type: 1,
        capabilities: 0xBEEF,
    });
    round_trip_client(ClientControl::PadDisconnected { pad_index: 2 });
    round_trip_client(ClientControl::Bye);
}

#[test]
fn every_implemented_server_message_round_trips() {
    round_trip_server(ServerControl::PairChallenge {
        request_id: 5,
        server_nonce: [3; NONCE_LEN],
    });
    round_trip_server(ServerControl::AppList(vec![
        AppListing {
            id: 0,
            name: "Big Picture".into(),
        },
        AppListing {
            id: 4,
            name: "Cyberpunk 2077".into(),
        },
    ]));
    round_trip_server(ServerControl::Bye);
    round_trip_server(ServerControl::AppList(Vec::new()));
}

#[test]
fn a_negative_clock_offset_survives() {
    // Encoded through u64; a sign-losing round trip would put the client's clock
    // permanently ahead of the server's.
    let ClientControl::Hello(h) = hello() else {
        unreachable!()
    };
    let encoded = ClientControl::Hello(h.clone()).encode().expect("encode");
    let (decoded, _) = ClientControl::decode(&encoded).expect("decode");
    let ClientControl::Hello(back) = decoded else {
        panic!("wrong variant")
    };
    assert_eq!(back.clock_offset_ns, -1_234_567);
}

#[test]
fn quirks_flags_survive_every_combination() {
    // Four bits packed into one byte; an off-by-one shift would silently swap
    // two capabilities and produce corruption on one device only.
    for bits in 0..16u8 {
        let q = DecoderQuirks {
            ref_invalidation: bits & 1 != 0,
            intra_refresh: bits & 2 != 0,
            slice_output: bits & 4 != 0,
            needs_annexb_startcodes: bits & 8 != 0,
            max_bitrate_hint: 1234,
        };
        let encoded = ClientControl::Quirks(q).encode().expect("encode");
        let (decoded, _) = ClientControl::decode(&encoded).expect("decode");
        assert_eq!(decoded, ClientControl::Quirks(q), "bits {bits:04b}");
    }
}

#[test]
fn an_unknown_kind_is_skipped_with_its_length() {
    // The property the envelope exists for: a message from a newer build is
    // stepped over rather than desynchronising everything after it.
    let mut stream = Vec::new();
    stream.push(200u8); // a kind this build has never heard of
    stream.extend_from_slice(&11u16.to_le_bytes());
    stream.extend_from_slice(b"hello world");
    let follow = ClientControl::Bye.encode().expect("encode");
    stream.extend_from_slice(&follow);

    let (first, used) = ClientControl::decode(&stream).expect("decode unknown");
    assert_eq!(first, ClientControl::Unhandled(200));
    assert_eq!(used, 3 + 11);

    let (second, _) = ClientControl::decode(&stream[used..]).expect("decode next");
    assert_eq!(second, ClientControl::Bye, "the stream desynchronised");
}

#[test]
fn a_reserved_server_kind_decodes_as_unhandled() {
    // SessionConfig and friends are known but not decoded yet. They must skip
    // cleanly rather than being mistaken for something else.
    for kind in [
        ServerMessage::SessionConfig,
        ServerMessage::CodecPrivate,
        ServerMessage::CursorShape,
        ServerMessage::CursorPosition,
        ServerMessage::SecureDesktop,
    ] {
        let mut stream = vec![kind as u8];
        stream.extend_from_slice(&4u16.to_le_bytes());
        stream.extend_from_slice(&[1, 2, 3, 4]);

        let (decoded, used) = ServerControl::decode(&stream).expect("decode");
        assert_eq!(decoded, ServerControl::Unhandled(kind as u8));
        assert_eq!(used, 7);
    }
}

#[test]
fn discriminants_are_pinned() {
    // The numbering is on the wire. Reordering the enum would silently turn a
    // Bye into a Resize between two builds.
    assert_eq!(ClientMessage::Hello as u8, 0);
    assert_eq!(ClientMessage::Bye as u8, 6);
    assert_eq!(ClientMessage::PairRequest as u8, 7);
    assert_eq!(ClientMessage::LaunchApp as u8, 10);
    assert_eq!(ServerMessage::SessionConfig as u8, 0);
    assert_eq!(ServerMessage::PairChallenge as u8, 6);
    assert_eq!(ServerMessage::AppList as u8, 7);
}

#[test]
fn only_the_pairing_exchange_may_arrive_unauthenticated() {
    // The allow-list the endpoint consults. Anything else arriving without a MAC
    // is an attempt to skip authentication.
    for kind in [ClientMessage::PairRequest, ClientMessage::PairConfirm] {
        assert!(kind.is_pre_pairing(), "{kind:?}");
    }
    for kind in [
        ClientMessage::Hello,
        ClientMessage::LaunchApp,
        ClientMessage::ListApps,
        ClientMessage::Bye,
        ClientMessage::PadConnected,
        ClientMessage::RequestIdr,
        ClientMessage::Resize,
        ClientMessage::DecoderQuirks,
        ClientMessage::PadDisconnected,
    ] {
        assert!(!kind.is_pre_pairing(), "{kind:?} must require a MAC");
    }
    assert!(ServerMessage::PairChallenge.is_pre_pairing());
    assert!(!ServerMessage::AppList.is_pre_pairing());
}

#[test]
fn truncation_is_refused_rather_than_panicking() {
    // Every prefix of a valid message, since that is the class of bug that
    // reaches a panic on a hostile packet.
    let encoded = pair_request().encode().expect("encode");
    for len in 0..encoded.len() {
        assert!(
            ClientControl::decode(&encoded[..len]).is_err(),
            "a {len}-byte prefix decoded"
        );
    }
    assert!(ClientControl::decode(&encoded).is_ok());
}

#[test]
fn a_length_that_overruns_the_buffer_is_refused() {
    let stream = [ClientMessage::Bye as u8, 0xFF, 0xFF];
    assert_eq!(
        ClientControl::decode(&stream),
        Err(ControlError::BadLength(0xFFFF))
    );
}

#[test]
fn a_payload_shorter_than_its_fields_is_refused() {
    // The length is honest but the body is too small — a truncated sender rather
    // than a truncated buffer, which the outer length check would not catch.
    let mut stream = vec![ClientMessage::PairConfirm as u8];
    stream.extend_from_slice(&2u16.to_le_bytes());
    stream.extend_from_slice(&[1, 2]);
    assert_eq!(ClientControl::decode(&stream), Err(ControlError::Truncated));
}

#[test]
fn a_non_utf8_string_is_refused() {
    let mut stream = vec![ClientMessage::PairRequest as u8];
    let payload = [1u8, 0xFF]; // one byte of name, and it is not UTF-8
    stream.extend_from_slice(&(payload.len() as u16).to_le_bytes());
    stream.extend_from_slice(&payload);
    assert_eq!(ClientControl::decode(&stream), Err(ControlError::BadUtf8));
}

#[test]
fn an_over_long_string_is_refused_at_encode() {
    let long = "x".repeat(MAX_STRING + 1);
    let message = ClientControl::PairRequest(PairRequest {
        name: long.clone(),
        model: "m".into(),
        abi: "a".into(),
        quirks: quirks(),
        client_nonce: [0; NONCE_LEN],
    });
    assert_eq!(
        message.encode(),
        Err(ControlError::StringTooLong(MAX_STRING + 1))
    );
}

#[test]
fn strings_at_the_limit_still_encode() {
    let name = "x".repeat(MAX_STRING);
    round_trip_client(ClientControl::PairRequest(PairRequest {
        name,
        model: String::new(),
        abi: String::new(),
        quirks: quirks(),
        client_nonce: [0; NONCE_LEN],
    }));
}

#[test]
fn non_ascii_names_survive() {
    // Device names come from whatever the user typed on a TV.
    round_trip_client(ClientControl::PairRequest(PairRequest {
        name: "Wohnzimmer — 4K".into(),
        model: "SHIELD".into(),
        abi: "arm64-v8a".into(),
        quirks: quirks(),
        client_nonce: [0; NONCE_LEN],
    }));
}

#[test]
fn an_unhandled_message_cannot_be_encoded() {
    // It carries no payload to re-emit, so forwarding one would silently drop
    // its body. Better to refuse than to corrupt.
    assert!(ClientControl::Unhandled(200).encode().is_err());
    assert!(ServerControl::Unhandled(200).encode().is_err());
}

#[test]
fn several_messages_decode_back_to_back() {
    let mut stream = Vec::new();
    let messages = [
        ClientControl::ListApps,
        ClientControl::LaunchApp { app_id: 2 },
        hello(),
        ClientControl::Bye,
    ];
    for m in &messages {
        stream.extend_from_slice(&m.encode().expect("encode"));
    }

    let mut at = 0;
    for expected in &messages {
        let (decoded, used) = ClientControl::decode(&stream[at..]).expect("decode");
        assert_eq!(&decoded, expected);
        at += used;
    }
    assert_eq!(at, stream.len(), "left bytes unconsumed");
}

#[test]
fn quirks_default_is_the_conservative_decoder() {
    let q = DecoderQuirks::default();
    assert!(!q.ref_invalidation);
    assert!(!q.intra_refresh);
    assert!(!q.slice_output);
    assert!(q.needs_annexb_startcodes);
}
