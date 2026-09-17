// SPDX-License-Identifier: GPL-2.0-or-later

//! The endpoint over a real UDP socket.
//!
//! `Reliable` is tested directly elsewhere; what this covers is everything the
//! endpoint adds — key selection, the unauthenticated pairing exception, the
//! replay window, and the framing that both ends have to agree on.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sunburst_core::proto::pairing::{NONCE_LEN, confirm_tag, derive_secret};
use sunburst_core::proto::{
    AppListing, ClientControl, GamepadState, Hello, InputEvent, InputPacket, PairRequest,
    ServerControl, SessionKey,
};
use sunburst_net::endpoint::ClientEndpoint;
use sunburst_net::{Endpoint, Recording};

const SECRET: &[u8] = b"pairing secret";

fn key(seed: u8) -> SessionKey {
    SessionKey::derive(SECRET, &[seed; NONCE_LEN], &[seed; NONCE_LEN])
}

struct Server {
    addr: SocketAddr,
    recording: Arc<Mutex<Recording>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn start(recording: Recording) -> Server {
        let shared = Arc::new(Mutex::new(recording));
        let mut endpoint =
            Endpoint::bind("127.0.0.1:0".parse().expect("literal"), Arc::clone(&shared))
                .expect("bind");
        let addr = endpoint.local_addr().expect("addr");

        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            let _ = endpoint.run(&thread_stop);
        });

        Server {
            addr,
            recording: shared,
            stop,
            thread: Some(thread),
        }
    }

    /// Poll the recording until `check` passes, or fail rather than hang.
    fn wait_for(&self, what: &str, check: impl Fn(&Recording) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if check(&self.recording.lock().expect("not poisoned")) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn recording<T>(&self, f: impl FnOnce(&Recording) -> T) -> T {
        f(&self.recording.lock().expect("not poisoned"))
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn pair_request() -> ClientControl {
    ClientControl::PairRequest(PairRequest {
        name: "Living room".into(),
        model: "SHIELD Android TV".into(),
        abi: "arm64-v8a".into(),
        quirks: Default::default(),
        client_nonce: [7; NONCE_LEN],
    })
}

fn press() -> InputEvent {
    // A rich, DualSense-shaped pad, so the full IMU/touchpad/battery body is
    // exercised over the real socket, not just the X360 core.
    use sunburst_core::proto::input::buttons;
    use sunburst_core::proto::{Battery, Finger, Imu, Touchpad};
    InputEvent::Gamepad(GamepadState {
        pad_index: 0,
        buttons: buttons::A | buttons::SHARE,
        lx: 1234,
        ly: -5678,
        rx: -1,
        ry: 32767,
        lt: 40,
        rt: 200,
        imu: Some(Imu {
            gyro_pitch: 1000,
            gyro_yaw: -2000,
            gyro_roll: 300,
            accel_x: 4096,
            accel_y: -8192,
            accel_z: 512,
            sensor_timestamp: 0xDEAD_BEEF,
        }),
        touchpad: Some(Touchpad {
            finger0: Finger {
                active: true,
                x: 960,
                y: 540,
                id: 3,
            },
            finger1: Finger {
                active: false,
                x: 100,
                y: 200,
                id: 7,
            },
        }),
        battery: Some(Battery {
            level: 8,
            charging: true,
            full: false,
            mic_muted: true,
            headphones: true,
        }),
    })
}

#[test]
fn a_full_pairing_exchange_completes_over_the_wire() {
    let server = Server::start(Recording::new().armed());
    let mut client = ClientEndpoint::connect(server.addr, None).expect("connect");

    client.send_control(&pair_request()).expect("send");
    server.wait_for("the pair request", |r| !r.pair_requests.is_empty());

    // The challenge comes back on the same socket.
    let deadline = Instant::now() + Duration::from_secs(3);
    let (request_id, server_nonce) = loop {
        if let Some(ServerControl::PairChallenge {
            request_id,
            server_nonce,
        }) = client.recv_control().expect("recv")
        {
            break (request_id, server_nonce);
        }
        assert!(Instant::now() < deadline, "no challenge arrived");
    };

    // The client derives from the PIN it is displaying. The PIN itself never
    // goes near the wire.
    let secret = derive_secret("12345678", &[7; NONCE_LEN], &server_nonce);
    client
        .send_control(&ClientControl::PairConfirm {
            request_id,
            tag: confirm_tag(&secret),
        })
        .expect("send");

    server.wait_for("the confirmation", |r| !r.pair_confirms.is_empty());
    server.recording(|r| {
        assert_eq!(r.pair_requests[0].name, "Living room");
        assert_eq!(r.pair_confirms[0].0, request_id);
        assert_eq!(r.pair_confirms[0].1, confirm_tag(&secret));
    });
}

#[test]
fn an_unarmed_pair_request_is_dropped() {
    // The normal state of the world. Nothing unsolicited should be queued for
    // approval later.
    let server = Server::start(Recording::new());
    let mut client = ClientEndpoint::connect(server.addr, None).expect("connect");

    client.send_control(&pair_request()).expect("send");
    std::thread::sleep(Duration::from_millis(200));

    server.recording(|r| assert!(r.pair_requests.is_empty()));
    assert!(
        client.recv_control().expect("recv").is_none(),
        "an unarmed server should answer nothing at all"
    );
}

#[test]
fn an_unauthenticated_non_pairing_message_is_refused() {
    // The allow-list. Arriving without a MAC while pairing happens to be armed
    // must not be a way to launch a game.
    let server = Server::start(Recording::new().armed());
    let mut client = ClientEndpoint::connect(server.addr, None).expect("connect");

    client
        .send_control(&ClientControl::LaunchApp { app_id: 1 })
        .expect("send");
    std::thread::sleep(Duration::from_millis(200));

    server.recording(|r| assert!(r.launches.is_empty(), "an unsigned launch was honoured"));
}

#[test]
fn an_authenticated_client_is_recognised_by_its_key_alone() {
    // No client id on the wire: the endpoint scans the paired keys and the MAC
    // is what identifies the peer.
    let server = Server::start(Recording::new().with_key(9, key(1)));
    let mut client = ClientEndpoint::connect(server.addr, Some(key(1))).expect("connect");

    client
        .send_control(&ClientControl::Hello(Hello {
            client_id: 9,
            name: "Living room".into(),
            abi: "arm64-v8a".into(),
            width: 3840,
            height: 2160,
            refresh_mhz: 59_940,
            client_nonce: [1; NONCE_LEN],
            clock_offset_ns: 0,
        }))
        .expect("send");

    server.wait_for("the hello", |r| !r.hellos.is_empty());
    server.recording(|r| assert_eq!(r.hellos[0].0, 9, "attributed to the wrong client"));
}

#[test]
fn pad_connect_and_disconnect_reach_the_handler() {
    // Pad lifecycle rides the reliable control channel, so the injector learns
    // which controller to plug and never misses a plug/unplug.
    let server = Server::start(Recording::new().with_key(5, key(1)));
    let mut client = ClientEndpoint::connect(server.addr, Some(key(1))).expect("connect");

    client
        .send_control(&ClientControl::PadConnected {
            pad_index: 0,
            pad_type: 2,
            capabilities: 0x1234,
        })
        .expect("pad connected");
    client
        .send_control(&ClientControl::PadDisconnected { pad_index: 0 })
        .expect("pad disconnected");

    server.wait_for("the pad lifecycle", |r| {
        !r.pad_connects.is_empty() && !r.pad_disconnects.is_empty()
    });
    server.recording(|r| {
        assert_eq!(r.pad_connects[0], (5, 0, 2, 0x1234));
        assert_eq!(r.pad_disconnects[0], (5, 0));
    });
}

#[test]
fn a_packet_signed_with_another_clients_key_is_refused() {
    let server = Server::start(Recording::new().with_key(9, key(1)));
    let mut impostor = ClientEndpoint::connect(server.addr, Some(key(2))).expect("connect");

    impostor
        .send_control(&ClientControl::LaunchApp { app_id: 1 })
        .expect("send");
    std::thread::sleep(Duration::from_millis(200));
    server.recording(|r| assert!(r.launches.is_empty()));

    // And the genuine client still works afterwards — the failed scan must not
    // have poisoned anything.
    let mut genuine = ClientEndpoint::connect(server.addr, Some(key(1))).expect("connect");
    genuine
        .send_control(&ClientControl::LaunchApp { app_id: 4 })
        .expect("send");
    server.wait_for("the genuine launch", |r| !r.launches.is_empty());
    server.recording(|r| assert_eq!(r.launches, vec![4]));
}

#[test]
fn input_reaches_the_handler_and_a_replay_does_not() {
    let server = Server::start(Recording::new().with_key(3, key(1)));
    let mut client = ClientEndpoint::connect(server.addr, Some(key(1))).expect("connect");

    for seq in 1..=3u32 {
        client
            .send_input(&InputPacket {
                input_seq: seq,
                event: press(),
            })
            .expect("send");
    }
    server.wait_for("three input events", |r| r.inputs.len() == 3);

    // The same packet again. The MAC still verifies — it is deterministic — so
    // only the replay window can refuse it.
    client
        .send_input(&InputPacket {
            input_seq: 2,
            event: press(),
        })
        .expect("send");
    std::thread::sleep(Duration::from_millis(200));

    server.recording(|r| {
        assert_eq!(r.inputs.len(), 3, "a replayed packet was delivered again");
        assert!(r.inputs.iter().all(|(client, _)| *client == 3));
    });
}

#[test]
fn unsigned_input_is_refused() {
    // The hole CLAUDE.md is emphatic about: an unauthenticated port that reaches
    // SendInput is remote input injection.
    let server = Server::start(Recording::new().with_key(3, key(1)).armed());
    let mut client = ClientEndpoint::connect(server.addr, None).expect("connect");

    client
        .send_input(&InputPacket {
            input_seq: 1,
            event: press(),
        })
        .expect("send");
    std::thread::sleep(Duration::from_millis(200));

    server.recording(|r| assert!(r.inputs.is_empty(), "unsigned input was injected"));
}

#[test]
fn the_app_list_round_trips() {
    let apps = vec![
        AppListing {
            id: 0,
            name: "Big Picture".into(),
        },
        AppListing {
            id: 4,
            name: "Cyberpunk 2077".into(),
        },
    ];
    let server = Server::start(Recording::new().with_key(1, key(1)).with_apps(apps.clone()));
    let mut client = ClientEndpoint::connect(server.addr, Some(key(1))).expect("connect");

    client.send_control(&ClientControl::ListApps).expect("send");

    let deadline = Instant::now() + Duration::from_secs(3);
    let received = loop {
        if let Some(ServerControl::AppList(list)) = client.recv_control().expect("recv") {
            break list;
        }
        assert!(Instant::now() < deadline, "no app list arrived");
    };
    assert_eq!(received, apps);
}

#[test]
fn a_bye_forgets_the_peer() {
    let server = Server::start(Recording::new().with_key(5, key(1)));
    let mut client = ClientEndpoint::connect(server.addr, Some(key(1))).expect("connect");

    client
        .send_control(&ClientControl::LaunchApp { app_id: 1 })
        .expect("send");
    server.wait_for("the launch", |r| !r.launches.is_empty());

    client.send_control(&ClientControl::Bye).expect("send");
    server.wait_for("the bye", |r| !r.byes.is_empty());
    server.recording(|r| assert_eq!(r.byes, vec![5]));
}

#[test]
fn a_truncated_datagram_does_not_take_the_endpoint_down() {
    // A hostile or corrupt packet must not stop the server answering anyone.
    let server = Server::start(Recording::new().with_key(1, key(1)));
    let raw = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind");

    for len in 0..12 {
        raw.send_to(&vec![0u8; len], server.addr).expect("send");
    }
    raw.send_to(&[255u8; 40], server.addr).expect("send");

    // Still serving.
    let mut client = ClientEndpoint::connect(server.addr, Some(key(1))).expect("connect");
    client
        .send_control(&ClientControl::LaunchApp { app_id: 7 })
        .expect("send");
    server.wait_for("the launch after the garbage", |r| !r.launches.is_empty());
}

#[test]
fn a_replay_from_a_different_source_port_is_still_refused() {
    // The hole a per-address replay window leaves. A MAC is deterministic, so a
    // captured input packet verifies wherever it is resent from; if the window
    // were keyed by address, a new source port would get a fresh one and the
    // replay would be accepted. The window follows the client instead.
    let server = Server::start(Recording::new().with_key(3, key(1)));

    let mut first = ClientEndpoint::connect(server.addr, Some(key(1))).expect("connect");
    for seq in 1..=3u32 {
        first
            .send_input(&InputPacket {
                input_seq: seq,
                event: press(),
            })
            .expect("send");
    }
    server.wait_for("the first three events", |r| r.inputs.len() == 3);

    // A different socket, so a different source port — exactly what an attacker
    // replaying a capture would look like.
    let mut replayer = ClientEndpoint::connect(server.addr, Some(key(1))).expect("connect");
    for seq in 1..=3u32 {
        replayer
            .send_input(&InputPacket {
                input_seq: seq,
                event: press(),
            })
            .expect("send");
    }
    std::thread::sleep(Duration::from_millis(300));

    server.recording(|r| {
        assert_eq!(
            r.inputs.len(),
            3,
            "replayed input was accepted from a new source port"
        );
    });
}

#[test]
fn a_client_that_reconnects_from_a_new_port_keeps_working() {
    // The other side of the same coin: the window following the client must not
    // lock out a client whose app restarted and got a new ephemeral port.
    let server = Server::start(Recording::new().with_key(3, key(1)));

    let mut first = ClientEndpoint::connect(server.addr, Some(key(1))).expect("connect");
    first
        .send_input(&InputPacket {
            input_seq: 1,
            event: press(),
        })
        .expect("send");
    server.wait_for("the first event", |r| r.inputs.len() == 1);

    // New socket, continuing the sequence, as a reconnecting client would.
    let mut again = ClientEndpoint::connect(server.addr, Some(key(1))).expect("connect");
    again
        .send_input(&InputPacket {
            input_seq: 2,
            event: press(),
        })
        .expect("send");
    server.wait_for("the event after reconnecting", |r| r.inputs.len() == 2);
}

#[test]
fn queued_rumble_reaches_the_client_signed_and_unreliable() {
    // The outbound seam's unreliable user. A producer (the injector, on Windows)
    // queues a Rumble; the endpoint sends it as a signed type=6 packet, outside
    // the reliable channel.
    use sunburst_core::proto::rumble::RUMBLE_BODY_LEN;
    use sunburst_core::proto::{HEADER_LEN, Header, PacketType, Rumble};
    use sunburst_net::Outbound;

    let server = Server::start(Recording::new().with_key(4, key(1)));
    let mut client = ClientEndpoint::connect(server.addr, Some(key(1))).expect("connect");

    // A session only exists once the client has authenticated once, since that
    // is what teaches the endpoint the return address.
    client
        .send_input(&InputPacket {
            input_seq: 1,
            event: press(),
        })
        .expect("send");
    server.wait_for("the session to exist", |r| r.inputs.len() == 1);

    let rumble = Rumble {
        pad_index: 0,
        motor_low: 0xBEEF,
        motor_high: 0x1234,
        seq: 7,
    };
    server
        .recording
        .lock()
        .expect("not poisoned")
        .outbound
        .push(Outbound::Rumble { client: 4, rumble });

    // Grab the raw datagram: recv_control would drop it, because it is not a
    // ServerControl.
    client
        .socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .expect("timeout");
    let mut buf = [0u8; 256];
    let len = client.socket.recv(&mut buf).expect("a rumble packet");
    let datagram = &buf[..len];

    let header = Header::decode(datagram).expect("a header");
    assert_eq!(
        header.packet_type,
        PacketType::Rumble,
        "rumble must be type 6, not the reliable control type"
    );

    let key = key(1);
    let verified = key
        .verify_packet(datagram)
        .expect("rumble is authenticated and must verify");
    let body = &verified[HEADER_LEN..];
    assert_eq!(body.len(), RUMBLE_BODY_LEN);
    let decoded = Rumble::decode(body).expect("a rumble body");
    assert_eq!(decoded, rumble, "the level did not survive the round trip");
}

#[test]
fn queued_pad_output_reaches_the_client_signed_and_unreliable() {
    // The rich sibling of rumble: the injector queues a PadOutput; the endpoint
    // sends it as a signed type=7 packet, unreliable like rumble.
    use sunburst_core::proto::{HEADER_LEN, Header, PacketType, PadOutput, TriggerEffect};
    use sunburst_net::Outbound;

    let server = Server::start(Recording::new().with_key(4, key(1)));
    let mut client = ClientEndpoint::connect(server.addr, Some(key(1))).expect("connect");
    client
        .send_input(&InputPacket {
            input_seq: 1,
            event: press(),
        })
        .expect("send");
    server.wait_for("the session to exist", |r| r.inputs.len() == 1);

    let output = PadOutput {
        pad_index: 0,
        seq: 9,
        motor_low: 0xBEEF,
        motor_high: 0x1234,
        led: [0x10, 0x20, 0x30],
        player_led: 0b0000_0101,
        flags: 1,
        left_trigger: TriggerEffect::from_slice(&[0x02, 0x90, 0xA0]),
        right_trigger: TriggerEffect::default(),
    };
    server
        .recording
        .lock()
        .expect("not poisoned")
        .outbound
        .push(Outbound::PadOutput { client: 4, output });

    client
        .socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .expect("timeout");
    let mut buf = [0u8; 256];
    let len = client.socket.recv(&mut buf).expect("a pad-output packet");
    let datagram = &buf[..len];

    let header = Header::decode(datagram).expect("a header");
    assert_eq!(
        header.packet_type,
        PacketType::PadOutput,
        "pad output must be type 7"
    );

    let key = key(1);
    let verified = key
        .verify_packet(datagram)
        .expect("pad output is authenticated and must verify");
    let decoded = PadOutput::decode(&verified[HEADER_LEN..]).expect("a pad-output body");
    assert_eq!(
        decoded, output,
        "the effects did not survive the round trip"
    );
}

#[test]
fn a_forged_rumble_does_not_verify() {
    // The client must reject a rumble signed with the wrong key, the same as any
    // other authenticated packet.
    use sunburst_core::proto::{Header, PacketType, Rumble};
    use sunburst_net::Outbound;

    let server = Server::start(Recording::new().with_key(4, key(1)));
    let mut client = ClientEndpoint::connect(server.addr, Some(key(1))).expect("connect");
    client
        .send_input(&InputPacket {
            input_seq: 1,
            event: press(),
        })
        .expect("send");
    server.wait_for("the session", |r| r.inputs.len() == 1);

    server
        .recording
        .lock()
        .expect("not poisoned")
        .outbound
        .push(Outbound::Rumble {
            client: 4,
            rumble: Rumble {
                pad_index: 0,
                motor_low: 1,
                motor_high: 1,
                seq: 1,
            },
        });

    client
        .socket
        .set_read_timeout(Some(Duration::from_secs(3)))
        .expect("timeout");
    let mut buf = [0u8; 256];
    let len = client.socket.recv(&mut buf).expect("a packet");
    let datagram = &buf[..len];
    assert_eq!(
        Header::decode(datagram).expect("header").packet_type,
        PacketType::Rumble
    );
    // A different key must not verify the server's signature.
    assert!(
        key(2).verify_packet(datagram).is_none(),
        "a rumble packet verified under the wrong key"
    );
}
