// SPDX-License-Identifier: GPL-2.0-or-later

//! Pairing and input, end to end, against the real state and the real store.
//!
//! The two halves of the server meet here: a client pairs over a real UDP
//! socket, the PIN is typed through the HTTP API the way a person would, and the
//! client then authenticates with the secret both ends derived independently.
//!
//! In-process rather than spawning the example binary — the same coverage
//! without a shell script juggling background processes, and it runs in the
//! ordinary `cargo test`.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sunburst_core::proto::pairing::{NONCE_LEN, confirm_tag, derive_secret};
use sunburst_core::proto::{
    ClientControl, GamepadState, InputEvent, InputPacket, PairRequest, ServerControl, SessionKey,
};
use sunburst_net::{ClientEndpoint, Endpoint};
use sunburst_web::api::{ApiRequest, AppState, dispatch};
use sunburst_web::client::PublicClient;
use sunburst_web::config::AppEntry;
use sunburst_web::host::{Fake, Host};
use sunburst_web::{InputSink, Store, WebHandler};

const PIN: &str = "13572468";

/// Real wall-clock seconds.
///
/// A fixed timestamp will not do: the endpoint checks the pairing window against
/// the real clock, so arming at an invented time leaves it already expired.
fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs()
}

struct Temp(PathBuf);

impl Temp {
    fn new(tag: &str) -> Temp {
        let mut p = std::env::temp_dir();
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        p.push(format!("sunburst-e2e-{tag}-{unique}"));
        fs::create_dir_all(&p).expect("temp dir");
        Temp(p)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Stands in for `sunburst-input`, which does not exist yet.
#[derive(Clone, Default)]
struct RecordedInput(Arc<Mutex<Vec<(u32, InputEvent)>>>);

impl InputSink for RecordedInput {
    fn inject(&mut self, client: u32, _seq: u32, event: InputEvent) {
        self.0.lock().expect("not poisoned").push((client, event));
    }
}

struct Harness {
    _dir: Temp,
    state: Arc<AppState>,
    token: String,
    udp: std::net::SocketAddr,
    injected: RecordedInput,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Harness {
    fn new(tag: &str) -> Harness {
        let dir = Temp::new(tag);
        let host = Arc::new(Fake::new()) as Arc<dyn Host>;
        let state = Arc::new(AppState::load(Store::at(&dir.0), host).expect("state"));
        let token = state.token();

        let injected = RecordedInput::default();
        let mut endpoint = Endpoint::bind(
            "127.0.0.1:0".parse().expect("literal"),
            WebHandler::new(Arc::clone(&state), injected.clone(), sunburst_net::NoStream),
        )
        .expect("bind");
        let udp = endpoint.local_addr().expect("addr");

        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread = std::thread::spawn(move || {
            let _ = endpoint.run(&thread_stop);
        });

        Harness {
            _dir: dir,
            state,
            token,
            udp,
            injected,
            stop,
            thread: Some(thread),
        }
    }

    fn api(&self, req: ApiRequest) -> sunburst_web::ApiResponse {
        dispatch(&self.state, &req.with_token(&self.token), now())
    }

    fn wait<T>(&self, what: &str, mut f: impl FnMut() -> Option<T>) -> T {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(value) = f() {
                return value;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Everything a client does to pair, returning the secret it derived.
fn pair(h: &Harness, client: &mut ClientEndpoint) -> [u8; 32] {
    let client_nonce = [11u8; NONCE_LEN];
    client
        .send_control(&ClientControl::PairRequest(PairRequest {
            name: "fakeclient".into(),
            model: "development".into(),
            abi: "x86_64".into(),
            quirks: Default::default(),
            client_nonce,
        }))
        .expect("send request");

    let (request_id, server_nonce) = h.wait("the pair challenge", || {
        match client.recv_control().expect("recv") {
            Some(ServerControl::PairChallenge {
                request_id,
                server_nonce,
            }) => Some((request_id, server_nonce)),
            _ => None,
        }
    });

    let secret = derive_secret(PIN, &client_nonce, &server_nonce);
    client
        .send_control(&ClientControl::PairConfirm {
            request_id,
            tag: confirm_tag(&secret),
        })
        .expect("send confirm");

    // The server records the tag off the wire; nothing is paired until the PIN
    // is typed.
    h.wait("the confirmation to be recorded", || {
        let pending: Vec<serde_json::Value> = h
            .api(ApiRequest::get("/api/pair/pending"))
            .parse()
            .expect("pending");
        pending
            .first()
            .filter(|p| p["awaiting_client"] == false)
            .map(|_| ())
    });
    secret
}

#[test]
fn a_client_pairs_over_udp_and_is_then_trusted() {
    let h = Harness::new("pair");
    assert_eq!(h.api(ApiRequest::post("/api/pair/arm", ())).status, 200);

    let mut client = ClientEndpoint::connect(h.udp, None).expect("connect");
    let secret = pair(&h, &mut client);

    // The PIN is typed into the web UI. It never went near the wire — the server
    // is deriving from what a person entered, and the tags have to agree.
    let confirmed = h.api(ApiRequest::post(
        "/api/pair/confirm",
        serde_json::json!({"request_id": 0, "pin": PIN, "name": "Living room"}),
    ));
    assert_eq!(
        confirmed.status,
        201,
        "{}",
        String::from_utf8_lossy(&confirmed.body)
    );

    let clients: Vec<PublicClient> = h
        .api(ApiRequest::get("/api/clients"))
        .parse()
        .expect("clients");
    assert_eq!(clients.len(), 1);
    assert_eq!(clients[0].name, "Living room");

    // The two ends agree on the secret, which is the whole point.
    assert_eq!(
        h.state.client_secret(clients[0].id),
        Some(secret),
        "the derived secrets differ"
    );
}

#[test]
fn a_wrong_pin_leaves_the_client_unpaired_and_the_arming_alive() {
    let h = Harness::new("wrongpin");
    h.api(ApiRequest::post("/api/pair/arm", ()));
    let mut client = ClientEndpoint::connect(h.udp, None).expect("connect");
    pair(&h, &mut client);

    let refused = h.api(ApiRequest::post(
        "/api/pair/confirm",
        serde_json::json!({"request_id": 0, "pin": "00000000"}),
    ));
    assert_eq!(refused.status, 403);

    let clients: Vec<PublicClient> = h
        .api(ApiRequest::get("/api/clients"))
        .parse()
        .expect("clients");
    assert!(clients.is_empty());

    // A typo costs an attempt, not the walk back to the TV.
    assert_eq!(
        h.api(ApiRequest::post(
            "/api/pair/confirm",
            serde_json::json!({"request_id": 0, "pin": PIN}),
        ))
        .status,
        201
    );
}

#[test]
fn a_paired_client_lists_apps_and_sends_input() {
    let h = Harness::new("session");
    h.api(ApiRequest::post("/api/pair/arm", ()));

    let mut pairing_client = ClientEndpoint::connect(h.udp, None).expect("connect");
    let secret = pair(&h, &mut pairing_client);
    h.api(ApiRequest::post(
        "/api/pair/confirm",
        serde_json::json!({"request_id": 0, "pin": PIN}),
    ));

    // An app added through the web UI has to be visible over the control
    // channel, or the two halves have diverged on what exists.
    let created = h.api(ApiRequest::post(
        "/api/apps",
        AppEntry {
            name: "Big Picture".into(),
            exe: "steam://open/bigpicture".into(),
            ..Default::default()
        },
    ));
    assert_eq!(created.status, 201);

    let mut client =
        ClientEndpoint::connect(h.udp, Some(SessionKey::from_bytes(secret))).expect("connect");
    client.send_control(&ClientControl::ListApps).expect("send");

    let apps = h.wait("the app list", || {
        match client.recv_control().expect("recv") {
            Some(ServerControl::AppList(page)) => Some(page.apps),
            _ => None,
        }
    });
    assert_eq!(apps.len(), 1);
    assert_eq!(apps[0].name, "Big Picture");

    // Launching over the control channel goes through the same path as the web
    // UI's button.
    client
        .send_control(&ClientControl::LaunchApp { app_id: apps[0].id })
        .expect("send");
    h.wait("the launch", || h.state.host().running_app().map(|_| ()));

    // And input reaches the sink `sunburst-input` will replace — a rich,
    // DualSense-shaped pad, so the whole IMU/touchpad/battery body makes it
    // through pairing, transport and attribution intact.
    let event = InputEvent::Gamepad(GamepadState {
        pad_index: 0,
        buttons: sunburst_core::proto::input::buttons::A
            | sunburst_core::proto::input::buttons::SHARE,
        lx: 1234,
        ly: -5678,
        lt: 40,
        rt: 200,
        imu: Some(sunburst_core::proto::Imu {
            gyro_pitch: 1000,
            gyro_yaw: -2000,
            gyro_roll: 300,
            accel_x: 4096,
            accel_y: -8192,
            accel_z: 512,
            sensor_timestamp: 0xDEAD_BEEF,
        }),
        touchpad: Some(sunburst_core::proto::Touchpad {
            finger0: sunburst_core::proto::Finger {
                active: true,
                x: 960,
                y: 540,
                id: 3,
            },
            finger1: sunburst_core::proto::Finger::default(),
        }),
        battery: Some(sunburst_core::proto::Battery {
            level: 8,
            charging: true,
            full: false,
            mic_muted: true,
            headphones: true,
        }),
        ..Default::default()
    });
    for seq in 1..=4u32 {
        client
            .send_input(&InputPacket {
                input_seq: seq,
                event,
            })
            .expect("send input");
    }
    h.wait("four input events", || {
        let seen = h.injected.0.lock().expect("not poisoned").len();
        (seen == 4).then_some(())
    });

    let injected = h.injected.0.lock().expect("not poisoned");
    assert!(injected.iter().all(|(_, e)| *e == event));
    assert!(
        injected.iter().all(|(c, _)| *c == 1),
        "input attributed to the wrong client"
    );
}

#[test]
fn a_revoked_client_stops_being_able_to_send_input() {
    // The other free mitigation for PIN-derived pairing: revoking has to take
    // effect at once, not at the next restart.
    let h = Harness::new("revoke");
    h.api(ApiRequest::post("/api/pair/arm", ()));

    let mut pairing_client = ClientEndpoint::connect(h.udp, None).expect("connect");
    let secret = pair(&h, &mut pairing_client);
    h.api(ApiRequest::post(
        "/api/pair/confirm",
        serde_json::json!({"request_id": 0, "pin": PIN}),
    ));

    let mut client =
        ClientEndpoint::connect(h.udp, Some(SessionKey::from_bytes(secret))).expect("connect");
    let event = InputEvent::Gamepad(GamepadState::default());
    client
        .send_input(&InputPacket {
            input_seq: 1,
            event,
        })
        .expect("send");
    h.wait("the first event", || {
        (h.injected.0.lock().expect("not poisoned").len() == 1).then_some(())
    });

    assert_eq!(h.api(ApiRequest::delete("/api/clients/1")).status, 204);

    // A new socket, so the endpoint has to re-scan the keys — and there are none
    // left that match.
    let mut after =
        ClientEndpoint::connect(h.udp, Some(SessionKey::from_bytes(secret))).expect("connect");
    after
        .send_input(&InputPacket {
            input_seq: 99,
            event,
        })
        .expect("send");
    std::thread::sleep(Duration::from_millis(300));

    assert_eq!(
        h.injected.0.lock().expect("not poisoned").len(),
        1,
        "a revoked client could still inject input"
    );
}
