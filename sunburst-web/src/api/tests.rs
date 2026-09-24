// SPDX-License-Identifier: GPL-2.0-or-later

//! API tests, driven entirely through [`dispatch`] against a [`Fake`] host.
//!
//! None of this needs a socket or Windows, which is the point of the `Host`
//! seam: the whole management surface is exercised on the development machine.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use super::*;
use crate::client::QuirksRecord;
use crate::config::{CaptureBackend, CodecPreference, ConfigError, RateControl};
use crate::host::Fake;
use crate::pairing::PairRequest;
use sunburst_core::proto::pairing::{confirm_tag, derive_secret};

const NOW: u64 = 1_700_000_000;

/// A scratch directory, removed on drop.
struct Temp(PathBuf);

impl Temp {
    fn new(tag: &str) -> Temp {
        let mut p = std::env::temp_dir();
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        p.push(format!("sunburst-api-{tag}-{unique}"));
        fs::create_dir_all(&p).expect("create temp dir");
        Temp(p)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Harness {
    dir: Temp,
    state: AppState,
    host: Arc<Fake>,
    token: String,
}

impl Harness {
    fn new(tag: &str) -> Harness {
        let dir = Temp::new(tag);
        let host = Arc::new(Fake::new());
        let state = AppState::load(Store::at(&dir.0), Arc::clone(&host) as Arc<dyn Host>)
            .expect("load state");
        let token = state.token();
        Harness {
            dir,
            state,
            host,
            token,
        }
    }

    /// Dispatch with the correct token already attached.
    fn send(&self, req: ApiRequest) -> ApiResponse {
        dispatch(&self.state, &req.with_token(&self.token), NOW)
    }

    fn send_at(&self, req: ApiRequest, now: u64) -> ApiResponse {
        dispatch(&self.state, &req.with_token(&self.token), now)
    }

    /// Dispatch with no credentials at all.
    fn send_anonymous(&self, req: ApiRequest) -> ApiResponse {
        dispatch(&self.state, &req, NOW)
    }

    fn add_app(&self, name: &str) -> AppEntry {
        let body = AppEntry {
            name: name.into(),
            exe: "game.exe".into(),
            ..Default::default()
        };
        let response = self.send(ApiRequest::post("/api/apps", &body));
        assert_eq!(response.status, 201, "{}", body_text(&response));
        response.parse().expect("app entry")
    }
}

fn body_text(r: &ApiResponse) -> String {
    String::from_utf8_lossy(&r.body).into_owned()
}

// ---------------------------------------------------------------- auth

#[test]
fn a_request_without_a_token_is_refused() {
    let h = Harness::new("noauth");
    let r = h.send_anonymous(ApiRequest::get("/api/status"));
    assert_eq!(r.status, 401);
}

#[test]
fn a_wrong_token_is_refused() {
    let h = Harness::new("wrongauth");
    let r = dispatch(
        &h.state,
        &ApiRequest::get("/api/status").with_token("nope"),
        NOW,
    );
    assert_eq!(r.status, 401);
}

#[test]
fn a_token_of_the_wrong_length_is_refused() {
    // Separate from the wrong-token case because a length check that returns
    // early is itself a leak — see `auth::verify`.
    let h = Harness::new("lenauth");
    for wrong in [&h.token[..8], &format!("{}extra", h.token)[..]] {
        let r = dispatch(
            &h.state,
            &ApiRequest::get("/api/status").with_token(wrong),
            NOW,
        );
        assert_eq!(r.status, 401, "accepted a token of length {}", wrong.len());
    }
}

#[test]
fn auth_is_checked_before_anything_is_done() {
    // An unauthorised delete must not reach the store. If auth were checked per
    // handler, one forgotten check would be a silent hole.
    let h = Harness::new("authfirst");
    let app = h.add_app("Game");
    assert_eq!(
        h.send_anonymous(ApiRequest::delete("/api/apps/0")).status,
        401
    );
    let apps: Vec<AppEntry> = h.send(ApiRequest::get("/api/apps")).parse().expect("apps");
    assert_eq!(apps.len(), 1);
    assert_eq!(apps[0].id, app.id);
}

#[test]
fn a_token_is_minted_on_first_run_and_persisted() {
    let h = Harness::new("token");
    assert_eq!(h.token.len(), 32);

    // A second load must find the same token rather than mint a new one, or
    // every restart would invalidate whatever the UI had stored.
    let again = AppState::load(Store::at(&h.dir.0), Arc::new(Fake::new()) as Arc<dyn Host>)
        .expect("reload");
    assert_eq!(again.token(), h.token);
}

// ---------------------------------------------------------------- routing

#[test]
fn an_unknown_endpoint_is_a_404_not_a_panic() {
    let h = Harness::new("404");
    assert_eq!(h.send(ApiRequest::get("/api/nope")).status, 404);
    assert_eq!(h.send(ApiRequest::get("/api/apps/1/nope")).status, 404);
    assert_eq!(h.send(ApiRequest::delete("/api/status")).status, 404);
}

#[test]
fn a_non_numeric_id_is_rejected_rather_than_ignored() {
    let h = Harness::new("badid");
    for path in ["/api/apps/abc", "/api/clients/abc", "/api/sessions/abc"] {
        assert_eq!(h.send(ApiRequest::delete(path)).status, 400, "{path}");
    }
}

// ---------------------------------------------------------------- settings

#[test]
fn settings_round_trip() {
    let h = Harness::new("settings");
    let mut settings: SettingsBody = h
        .send(ApiRequest::get("/api/config"))
        .parse()
        .expect("settings");
    settings.stream.bitrate_kbps = 90_000;
    settings.stream.codec = CodecPreference::Av1;
    // The advanced knobs and the input group must survive the round trip too,
    // not just the two headline fields.
    settings.stream.hdr = false;
    settings.stream.preset = 4;
    settings.stream.rate_control = RateControl::Vbr;
    settings.stream.capture_backend = CaptureBackend::Nvfbc;
    settings.stream.slices = 4;
    settings.stream.max_bitrate_kbps = 130_000;
    settings.stream.audio_frame_us = 2_500;
    settings.stream.audio_fec = false;
    settings.stream.mic_device = Some("Steam Streaming Microphone".into());
    settings.input.mouse_sensitivity = 1.5;
    settings.input.gamepad_deadzone = 0.1;
    settings.input.disable_epp = true;

    let r = h.send(ApiRequest::put("/api/config", &settings));
    assert_eq!(r.status, 200, "{}", body_text(&r));

    let reloaded: SettingsBody = h
        .send(ApiRequest::get("/api/config"))
        .parse()
        .expect("settings");
    assert_eq!(reloaded.stream.bitrate_kbps, 90_000);
    assert_eq!(reloaded.stream.codec, CodecPreference::Av1);
    assert!(!reloaded.stream.hdr);
    assert_eq!(reloaded.stream.preset, 4);
    assert_eq!(reloaded.stream.rate_control, RateControl::Vbr);
    assert_eq!(reloaded.stream.capture_backend, CaptureBackend::Nvfbc);
    assert_eq!(reloaded.stream.slices, 4);
    assert_eq!(reloaded.stream.max_bitrate_kbps, 130_000);
    assert_eq!(reloaded.stream.audio_frame_us, 2_500);
    assert!(!reloaded.stream.audio_fec);
    assert_eq!(
        reloaded.stream.mic_device.as_deref(),
        Some("Steam Streaming Microphone")
    );
    assert_eq!(reloaded.input.mouse_sensitivity, 1.5);
    assert_eq!(reloaded.input.gamepad_deadzone, 0.1);
    assert!(reloaded.input.disable_epp);
}

#[test]
fn audio_devices_lists_the_hosts_endpoints() {
    // The settings device picker reads this; the Fake stands in for WASAPI
    // enumeration. Assert the wiring reaches the host and the names round-trip,
    // including Valve's sink, which is the host-silencing choice.
    let h = Harness::new("audio-devices");
    let r = h.send(ApiRequest::get("/api/audio-devices"));
    assert_eq!(r.status, 200, "{}", body_text(&r));
    let body: serde_json::Value = r.parse().expect("audio devices");
    let devices: Vec<String> = body["devices"]
        .as_array()
        .expect("devices array")
        .iter()
        .map(|v| v.as_str().expect("string").to_string())
        .collect();
    assert_eq!(devices, h.host.audio_devices());
    assert!(devices.iter().any(|d| d.contains("Steam Streaming")));
}

#[test]
fn saving_settings_does_not_wipe_the_app_catalogue() {
    // The failure this guards: the UI PUTs only the settings half, and the
    // catalogue disappears because the body did not mention it.
    let h = Harness::new("preserve");
    h.add_app("Big Picture");

    let settings: SettingsBody = h
        .send(ApiRequest::get("/api/config"))
        .parse()
        .expect("settings");
    h.send(ApiRequest::put("/api/config", &settings));

    let apps: Vec<AppEntry> = h.send(ApiRequest::get("/api/apps")).parse().expect("apps");
    assert_eq!(apps.len(), 1, "settings save destroyed the catalogue");
}

#[test]
fn settings_that_would_expose_an_unauthenticated_api_are_refused() {
    let h = Harness::new("expose");
    let mut settings: SettingsBody = h
        .send(ApiRequest::get("/api/config"))
        .parse()
        .expect("settings");
    settings.web.bind = "192.168.0.10".parse().expect("addr");
    settings.web.token = String::new();

    let r = h.send(ApiRequest::put("/api/config", &settings));
    assert_eq!(r.status, 400, "{}", body_text(&r));

    // And the running config is untouched, so the token still works.
    assert_eq!(h.send(ApiRequest::get("/api/status")).status, 200);
}

#[test]
fn changing_the_token_takes_effect_immediately() {
    let h = Harness::new("retoken");
    let mut settings: SettingsBody = h
        .send(ApiRequest::get("/api/config"))
        .parse()
        .expect("settings");
    settings.web.token = "a-new-token".into();
    assert_eq!(
        h.send(ApiRequest::put("/api/config", &settings)).status,
        200
    );

    // The old token stops working without a restart.
    assert_eq!(h.send(ApiRequest::get("/api/status")).status, 401);
    let r = dispatch(
        &h.state,
        &ApiRequest::get("/api/status").with_token("a-new-token"),
        NOW,
    );
    assert_eq!(r.status, 200);
}

#[test]
fn a_malformed_settings_body_is_a_400() {
    let h = Harness::new("badbody");
    let mut req = ApiRequest::put("/api/config", ());
    req.body = b"not json".to_vec();
    assert_eq!(
        dispatch(&h.state, &req.with_token(&h.token), NOW).status,
        400
    );
}

// ---------------------------------------------------------------- apps

#[test]
fn app_crud_round_trips() {
    let h = Harness::new("appcrud");
    let mut app = h.add_app("Big Picture");
    assert_eq!(app.name, "Big Picture");

    app.name = "Steam".into();
    app.overrides.bitrate_kbps = Some(70_000);
    let r = h.send(ApiRequest::put(&format!("/api/apps/{}", app.id), &app));
    assert_eq!(r.status, 200, "{}", body_text(&r));

    let apps: Vec<AppEntry> = h.send(ApiRequest::get("/api/apps")).parse().expect("apps");
    assert_eq!(apps[0].name, "Steam");
    assert_eq!(apps[0].overrides.bitrate_kbps, Some(70_000));

    assert_eq!(
        h.send(ApiRequest::delete(&format!("/api/apps/{}", app.id)))
            .status,
        204
    );
    let apps: Vec<AppEntry> = h.send(ApiRequest::get("/api/apps")).parse().expect("apps");
    assert!(apps.is_empty());
}

#[test]
fn app_ids_are_assigned_by_the_server_not_the_body() {
    // Otherwise a client could claim an existing id and overwrite an entry.
    let h = Harness::new("appid");
    let first = h.add_app("One");

    let colliding = AppEntry {
        id: first.id,
        name: "Two".into(),
        exe: "two.exe".into(),
        ..Default::default()
    };
    let created: AppEntry = h
        .send(ApiRequest::post("/api/apps", &colliding))
        .parse()
        .expect("entry");
    assert_ne!(created.id, first.id, "the body's id was honoured");

    let apps: Vec<AppEntry> = h.send(ApiRequest::get("/api/apps")).parse().expect("apps");
    assert_eq!(apps.len(), 2);
}

#[test]
fn app_ids_are_not_reused_after_deletion() {
    // A client caches the catalogue. Reusing an id would silently point a saved
    // shortcut at a different program.
    let h = Harness::new("appreuse");
    let first = h.add_app("One");
    h.send(ApiRequest::delete(&format!("/api/apps/{}", first.id)));
    let second = h.add_app("Two");
    assert_ne!(second.id, first.id);
}

#[test]
fn an_app_whose_exe_does_not_exist_is_still_storable() {
    // Drives get remapped and games get moved. Refusing to save an entry because
    // its path is currently wrong is worse than saving it and failing at launch.
    let h = Harness::new("missingexe");
    let body = AppEntry {
        name: "Moved".into(),
        exe: r"D:\gone\nothere.exe".into(),
        ..Default::default()
    };
    assert_eq!(h.send(ApiRequest::post("/api/apps", &body)).status, 201);
}

#[test]
fn an_app_with_no_name_is_refused() {
    let h = Harness::new("noname");
    let body = AppEntry {
        name: "  ".into(),
        exe: "game.exe".into(),
        ..Default::default()
    };
    let r = h.send(ApiRequest::post("/api/apps", &body));
    assert_eq!(r.status, 400, "{}", body_text(&r));
    assert!(body_text(&r).contains("name"), "{}", body_text(&r));
}

#[test]
fn updating_or_deleting_an_unknown_app_is_a_404() {
    let h = Harness::new("noapp");
    let entry = AppEntry {
        name: "x".into(),
        exe: "x".into(),
        ..Default::default()
    };
    assert_eq!(h.send(ApiRequest::put("/api/apps/42", &entry)).status, 404);
    assert_eq!(h.send(ApiRequest::delete("/api/apps/42")).status, 404);
}

#[test]
fn apps_survive_a_reload() {
    let h = Harness::new("appreload");
    h.add_app("Big Picture");

    let again = AppState::load(Store::at(&h.dir.0), Arc::new(Fake::new()) as Arc<dyn Host>)
        .expect("reload");
    let apps: Vec<AppEntry> = dispatch(
        &again,
        &ApiRequest::get("/api/apps").with_token(&again.token()),
        NOW,
    )
    .parse()
    .expect("apps");
    assert_eq!(apps.len(), 1);
}

// ---------------------------------------------------------------- launching

#[test]
fn launching_reports_the_running_app() {
    let h = Harness::new("launch");
    let app = h.add_app("Game");
    let r = h.send(ApiRequest::post(
        &format!("/api/apps/{}/launch", app.id),
        (),
    ));
    assert_eq!(r.status, 200, "{}", body_text(&r));

    let running: RunningApp = r.parse().expect("running app");
    assert_eq!(running.app_id, app.id);
    assert_eq!(h.host.running_app().map(|r| r.app_id), Some(app.id));
}

#[test]
fn effective_stream_config_applies_the_running_apps_overrides() {
    use crate::config::{CodecPreference, SessionOverrides};

    let h = Harness::new("effective");
    // No app running: the global defaults.
    let global = h.state.stream_config();
    assert_eq!(h.state.effective_stream_config(), global);

    // An app with a bitrate + codec profile, launched.
    let body = crate::config::AppEntry {
        name: "Profiled".into(),
        exe: "game.exe".into(),
        overrides: SessionOverrides {
            bitrate_kbps: Some(55_000),
            codec: Some(CodecPreference::Av1),
            ..Default::default()
        },
        ..Default::default()
    };
    let app: crate::config::AppEntry = h
        .send(ApiRequest::post("/api/apps", &body))
        .parse()
        .expect("app entry");
    h.send(ApiRequest::post(
        &format!("/api/apps/{}/launch", app.id),
        (),
    ));

    let effective = h.state.effective_stream_config();
    assert_eq!(effective.bitrate_kbps, 55_000);
    assert_eq!(effective.codec, CodecPreference::Av1);
}

#[test]
fn launching_a_second_app_is_a_conflict_not_a_silent_swap() {
    let h = Harness::new("relaunch");
    let a = h.add_app("A");
    let b = h.add_app("B");
    h.send(ApiRequest::post(&format!("/api/apps/{}/launch", a.id), ()));
    let r = h.send(ApiRequest::post(&format!("/api/apps/{}/launch", b.id), ()));
    assert_eq!(r.status, 409, "{}", body_text(&r));
}

#[test]
fn big_picture_by_uri_does_not_block_the_next_launch() {
    // A URI launch has no process to follow, so nothing ever reaps it. Before
    // tracking, every launch after Big Picture was a 409 until a restart.
    let h = Harness::new("untracked");
    let body = AppEntry {
        name: "Big Picture".into(),
        exe: "steam://open/bigpicture".into(),
        ..Default::default()
    };
    let bp: AppEntry = h
        .send(ApiRequest::post("/api/apps", &body))
        .parse()
        .expect("app");
    let game = h.add_app("Game");
    let r = h.send(ApiRequest::post(&format!("/api/apps/{}/launch", bp.id), ()));
    let running: RunningApp = r.parse().expect("running");
    assert_eq!(running.tracking, crate::apptrack::TrackingKind::Untracked);
    let r = h.send(ApiRequest::post(
        &format!("/api/apps/{}/launch", game.id),
        (),
    ));
    assert_eq!(r.status, 200, "{}", body_text(&r));
    assert_eq!(h.host.running_app().map(|r| r.app_id), Some(game.id));
}

#[test]
fn an_app_that_exits_on_its_own_frees_the_next_launch() {
    let h = Harness::new("reaped");
    let a = h.add_app("A");
    let b = h.add_app("B");
    h.send(ApiRequest::post(&format!("/api/apps/{}/launch", a.id), ()));
    h.host.simulate_exit();
    let r = h.send(ApiRequest::post(&format!("/api/apps/{}/launch", b.id), ()));
    assert_eq!(r.status, 200, "{}", body_text(&r));
}

#[test]
fn a_bad_wait_process_is_refused_at_save() {
    let h = Harness::new("waitproc");
    let body = AppEntry {
        name: "Game".into(),
        exe: "steam://rungameid/1".into(),
        wait_process: Some(r"C:\Games\Game.exe".into()),
        ..Default::default()
    };
    let r = h.send(ApiRequest::post("/api/apps", &body));
    assert_eq!(r.status, 400, "{}", body_text(&r));
}

#[test]
fn launching_an_unknown_app_is_a_404() {
    let h = Harness::new("launch404");
    assert_eq!(
        h.send(ApiRequest::post("/api/apps/42/launch", ())).status,
        404
    );
}

#[test]
fn terminating_clears_the_running_app() {
    let h = Harness::new("terminate");
    let app = h.add_app("Game");
    h.send(ApiRequest::post(
        &format!("/api/apps/{}/launch", app.id),
        (),
    ));
    assert_eq!(
        h.send(ApiRequest::post("/api/apps/terminate", ())).status,
        204
    );
    assert!(h.host.running_app().is_none());
}

#[test]
fn a_host_failure_becomes_a_500_not_a_panic() {
    let h = Harness::new("hostfail");
    let app = h.add_app("Game");
    h.host.set_failing(true);
    let r = h.send(ApiRequest::post(
        &format!("/api/apps/{}/launch", app.id),
        (),
    ));
    assert_eq!(r.status, 500, "{}", body_text(&r));
}

// ---------------------------------------------------------------- pairing

/// Everything the client does, given the PIN it displays on the TV.
fn client_pairs(h: &Harness, pin: &str, nonce: [u8; 16], name: &str) -> u32 {
    let request = PairRequest {
        name: name.into(),
        model: "SHIELD Android TV".into(),
        abi: "arm64-v8a".into(),
        quirks: QuirksRecord::default(),
        client_nonce: nonce,
    };
    let mut inner = h.state.inner.lock().expect("not poisoned");
    let (id, server_nonce) = inner
        .pairing
        .receive_request(request, NOW)
        .expect("request accepted");
    let tag = confirm_tag(&derive_secret(pin, &nonce, &server_nonce));
    inner
        .pairing
        .receive_confirm(id, tag, NOW)
        .expect("confirm");
    id
}

#[test]
fn a_full_pairing_produces_a_listed_client() {
    let h = Harness::new("pair");
    assert_eq!(h.send(ApiRequest::post("/api/pair/arm", ())).status, 200);

    let id = client_pairs(&h, "12345678", [7; 16], "Living room");

    let pending: Vec<serde_json::Value> = h
        .send(ApiRequest::get("/api/pair/pending"))
        .parse()
        .expect("pending");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0]["awaiting_client"], false);

    let r = h.send(ApiRequest::post(
        "/api/pair/confirm",
        serde_json::json!({"request_id": id, "pin": "12345678"}),
    ));
    assert_eq!(r.status, 201, "{}", body_text(&r));

    let clients: Vec<PublicClient> = h
        .send(ApiRequest::get("/api/clients"))
        .parse()
        .expect("clients");
    assert_eq!(clients.len(), 1);
    assert_eq!(clients[0].name, "Living room");
}

#[test]
fn a_client_can_be_renamed_at_confirm_time() {
    let h = Harness::new("pairname");
    h.send(ApiRequest::post("/api/pair/arm", ()));
    let id = client_pairs(&h, "12345678", [7; 16], "SHIELD Android TV");
    h.send(ApiRequest::post(
        "/api/pair/confirm",
        serde_json::json!({"request_id": id, "pin": "12345678", "name": "Bedroom"}),
    ));

    let clients: Vec<PublicClient> = h
        .send(ApiRequest::get("/api/clients"))
        .parse()
        .expect("clients");
    assert_eq!(clients[0].name, "Bedroom");
}

#[test]
fn a_wrong_pin_is_a_403_and_leaves_the_arming_usable() {
    let h = Harness::new("pairwrong");
    h.send(ApiRequest::post("/api/pair/arm", ()));
    let id = client_pairs(&h, "12345678", [7; 16], "Shield");

    let r = h.send(ApiRequest::post(
        "/api/pair/confirm",
        serde_json::json!({"request_id": id, "pin": "00000000"}),
    ));
    assert_eq!(r.status, 403, "{}", body_text(&r));

    // The retype still works, which is the point of not consuming the arming.
    let r = h.send(ApiRequest::post(
        "/api/pair/confirm",
        serde_json::json!({"request_id": id, "pin": "12345678"}),
    ));
    assert_eq!(r.status, 201, "{}", body_text(&r));
}

#[test]
fn pairing_is_impossible_without_arming() {
    let h = Harness::new("pairunarmed");
    let r = h.send(ApiRequest::post(
        "/api/pair/confirm",
        serde_json::json!({"request_id": 0, "pin": "12345678"}),
    ));
    assert_eq!(r.status, 409, "{}", body_text(&r));
    let clients: Vec<PublicClient> = h
        .send(ApiRequest::get("/api/clients"))
        .parse()
        .expect("clients");
    assert!(clients.is_empty());
}

#[test]
fn the_arming_window_closes_on_its_own() {
    let h = Harness::new("pairexpire");
    h.send(ApiRequest::post("/api/pair/arm", ()));
    let id = client_pairs(&h, "12345678", [7; 16], "Shield");

    let late = NOW + crate::pairing::WINDOW_SECS;
    let pending: Vec<serde_json::Value> = h
        .send_at(ApiRequest::get("/api/pair/pending"), late)
        .parse()
        .expect("pending");
    assert!(pending.is_empty());

    let r = h.send_at(
        ApiRequest::post(
            "/api/pair/confirm",
            serde_json::json!({"request_id": id, "pin": "12345678"}),
        ),
        late,
    );
    assert_eq!(r.status, 409);
}

#[test]
fn a_paired_secret_is_never_in_an_api_response() {
    let h = Harness::new("pairsecret");
    h.send(ApiRequest::post("/api/pair/arm", ()));
    let id = client_pairs(&h, "12345678", [7; 16], "Shield");
    let created = h.send(ApiRequest::post(
        "/api/pair/confirm",
        serde_json::json!({"request_id": id, "pin": "12345678"}),
    ));

    let secret = h.state.client_secret(1).expect("secret stored");
    let hex: String = secret.iter().map(|b| format!("{b:02x}")).collect();

    for response in [&created, &h.send(ApiRequest::get("/api/clients"))] {
        assert!(
            !body_text(response).contains(&hex),
            "the pairing secret reached a response body"
        );
    }
}

#[test]
fn revoking_a_client_removes_it_from_disk_too() {
    let h = Harness::new("revoke");
    h.send(ApiRequest::post("/api/pair/arm", ()));
    let id = client_pairs(&h, "12345678", [7; 16], "Shield");
    h.send(ApiRequest::post(
        "/api/pair/confirm",
        serde_json::json!({"request_id": id, "pin": "12345678"}),
    ));

    assert_eq!(h.send(ApiRequest::delete("/api/clients/1")).status, 204);
    assert!(h.state.client_secret(1).is_none());

    // And the revoke survives a restart, which is what "revoked" has to mean.
    let again = AppState::load(Store::at(&h.dir.0), Arc::new(Fake::new()) as Arc<dyn Host>)
        .expect("reload");
    assert!(again.client_secret(1).is_none());
}

#[test]
fn revoking_an_unknown_client_is_a_404() {
    let h = Harness::new("revoke404");
    assert_eq!(h.send(ApiRequest::delete("/api/clients/9")).status, 404);
}

// ---------------------------------------------------------------- sessions

#[test]
fn sessions_are_listed_and_can_be_disconnected() {
    let dir = Temp::new("sessions");
    let host = Arc::new(Fake::with_sessions(vec![SessionSummary {
        id: 5,
        client_id: 1,
        client_name: "Living room".into(),
        codec: "hevc".into(),
        width: 3840,
        height: 2160,
        fps: 60,
        bitrate_kbps: 120_000,
        started_at: NOW,
        app_id: None,
    }]));
    let state =
        AppState::load(Store::at(&dir.0), Arc::clone(&host) as Arc<dyn Host>).expect("load");
    let token = state.token();

    let listed: Vec<SessionSummary> = dispatch(
        &state,
        &ApiRequest::get("/api/sessions").with_token(&token),
        NOW,
    )
    .parse()
    .expect("sessions");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].client_name, "Living room");

    let r = dispatch(
        &state,
        &ApiRequest::delete("/api/sessions/5").with_token(&token),
        NOW,
    );
    assert_eq!(r.status, 204);
    assert!(host.sessions().is_empty());
}

#[test]
fn disconnecting_an_unknown_session_is_a_404() {
    let h = Harness::new("sess404");
    assert_eq!(h.send(ApiRequest::delete("/api/sessions/9")).status, 404);
}

// ---------------------------------------------------------------- metrics

#[test]
fn metrics_report_losses_rather_than_hiding_them() {
    use sunburst_core::instr::{Report, Stage, StageStats};

    let h = Harness::new("metrics");
    h.host.set_metrics(Some(Report {
        stages: vec![StageStats {
            stage: Stage::EncodeUnitOut,
            count: 100,
            p50_ns: 6_000_000,
            p95_ns: 8_000_000,
            p99_ns: 9_000_000,
            max_ns: 15_000_000,
        }],
        dropped_samples: 12,
        dropped_by_thread: vec![("encode", 12)],
        unregistered_threads: 0,
    }));

    let record: MetricsRecord = h
        .send(ApiRequest::get("/api/metrics"))
        .parse()
        .expect("metrics");
    assert!(record.lossy, "the UI must be told these cannot be quoted");
    assert_eq!(record.dropped_samples, 12);
}

#[test]
fn metrics_without_a_drain_are_a_503_not_an_empty_table() {
    // An empty table would read as "everything is fast", which is the most
    // misleading answer available.
    let h = Harness::new("nometrics");
    assert_eq!(h.send(ApiRequest::get("/api/metrics")).status, 503);
}

// ---------------------------------------------------------------- autostart

#[test]
fn autostart_round_trips_and_is_reported_in_status() {
    let h = Harness::new("autostart");
    let status: serde_json::Value = h
        .send(ApiRequest::get("/api/status"))
        .parse()
        .expect("status");
    assert_eq!(status["autostart"], false);

    let r = h.send(ApiRequest::post(
        "/api/autostart",
        serde_json::json!({"enabled": true}),
    ));
    assert_eq!(r.status, 204, "{}", body_text(&r));
    assert!(h.host.autostart().expect("read"));

    let status: serde_json::Value = h
        .send(ApiRequest::get("/api/status"))
        .parse()
        .expect("status");
    assert_eq!(status["autostart"], true);
}

#[test]
fn an_unreadable_autostart_is_unknown_rather_than_disabled() {
    // Reporting "off" when the answer could not be read would invite turning on
    // something that is already on.
    let h = Harness::new("autostartfail");
    h.host.set_failing(true);
    let status: serde_json::Value = h
        .send(ApiRequest::get("/api/status"))
        .parse()
        .expect("status");
    assert!(status["autostart"].is_null(), "{status}");
}

#[test]
fn restart_answers_before_it_happens() {
    let h = Harness::new("restart");
    let r = h.send(ApiRequest::post("/api/restart", ()));
    assert_eq!(
        r.status, 202,
        "the UI needs an answer before the process goes"
    );
    assert_eq!(h.host.restarts(), 1);
}

// ---------------------------------------------------------------- app listing

#[test]
fn the_client_facing_listing_is_names_and_ids_only() {
    // What goes over the control channel. Paths and prep commands are the
    // server's business and have no reason to reach a TV.
    let h = Harness::new("listing");
    let app = h.add_app("Big Picture");

    let listing = h.state.app_list();
    assert_eq!(listing.len(), 1);
    assert_eq!(listing[0].id, app.id);
    assert_eq!(listing[0].name, "Big Picture");

    let json = serde_json::to_string(&listing).expect("serialise");
    assert!(
        !json.contains("game.exe"),
        "the exe path reached the client"
    );
}

// ---------------------------------------------------------------- store errors

#[test]
fn a_corrupt_clients_file_stops_startup_rather_than_unpairing_everything() {
    // Reading a corrupt file as "nothing is paired" would silently revoke every
    // device, which is indistinguishable from an attacker having cleared it.
    let dir = Temp::new("corrupt");
    fs::write(dir.0.join("clients.json"), "[{ truncated").expect("write");
    let result = AppState::load(Store::at(&dir.0), Arc::new(Fake::new()) as Arc<dyn Host>);
    assert!(matches!(result, Err(StoreError::Corrupt { .. })));
}

#[test]
fn a_config_that_would_expose_the_api_stops_startup() {
    let dir = Temp::new("badconfig");
    let bad = serde_json::json!({
        "web": {"bind": "0.0.0.0", "port": 47810, "token": "", "assets_dir": "web"}
    });
    fs::write(
        dir.0.join("config.json"),
        serde_json::to_string(&bad).expect("serialise"),
    )
    .expect("write");

    let result = AppState::load(Store::at(&dir.0), Arc::new(Fake::new()) as Arc<dyn Host>);
    match result {
        Err(StoreError::Invalid(ConfigError::LanWithoutToken(_))) => {}
        Err(other) => panic!("expected LanWithoutToken, got {other:?}"),
        Ok(_) => panic!("an unauthenticated LAN bind was accepted"),
    }
}
