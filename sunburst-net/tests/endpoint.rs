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
    InputEvent::Gamepad(GamepadState {
        pad_index: 0,
        buttons: sunburst_core::proto::input::buttons::A,
        ..Default::default()
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
