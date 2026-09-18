// SPDX-License-Identifier: GPL-2.0-or-later

//! The management API.
//!
//! [`dispatch`] is deliberately synchronous and takes plain types rather than
//! hyper's. Routing an admin API is a `match` on method and path — that does not
//! justify a framework — and keeping it off the async types means the whole
//! surface is testable without opening a socket. [`crate::http`] is the only
//! part that knows about hyper.
//!
//! Blocking file I/O happens inline. The files are a few kilobytes and this is
//! the control plane; a `spawn_blocking` round trip would cost more than the
//! write.

use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::auth::Auth;
use crate::client::{PairedClient, PublicClient};
use crate::config::{AppEntry, Config, StreamConfig, WebConfig};
use crate::host::{Host, HostError, HostStatus, RunningApp, SessionSummary};
use crate::metrics::MetricsRecord;
use crate::pairing::{Pairing, PairingError};
use crate::random;
use crate::store::{Store, StoreError};
use sunburst_core::proto::SessionKey;

/// Request, reduced to what routing actually needs.
#[derive(Clone, Debug, Default)]
pub struct ApiRequest {
    pub method: String,
    pub path: String,
    pub authorization: Option<String>,
    pub body: Vec<u8>,
}

impl ApiRequest {
    pub fn get(path: &str) -> ApiRequest {
        ApiRequest {
            method: "GET".into(),
            path: path.into(),
            ..Default::default()
        }
    }

    pub fn post(path: &str, body: impl Serialize) -> ApiRequest {
        ApiRequest {
            method: "POST".into(),
            path: path.into(),
            body: serde_json::to_vec(&body).expect("test body serialises"),
            ..Default::default()
        }
    }

    pub fn put(path: &str, body: impl Serialize) -> ApiRequest {
        ApiRequest {
            method: "PUT".into(),
            path: path.into(),
            body: serde_json::to_vec(&body).expect("test body serialises"),
            ..Default::default()
        }
    }

    pub fn delete(path: &str) -> ApiRequest {
        ApiRequest {
            method: "DELETE".into(),
            path: path.into(),
            ..Default::default()
        }
    }

    #[must_use]
    pub fn with_token(mut self, token: &str) -> ApiRequest {
        self.authorization = Some(format!("Bearer {token}"));
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiResponse {
    pub status: u16,
    pub content_type: &'static str,
    pub body: Vec<u8>,
}

impl ApiResponse {
    fn json(status: u16, value: &impl Serialize) -> ApiResponse {
        ApiResponse {
            status,
            content_type: "application/json",
            body: serde_json::to_vec(value).unwrap_or_else(|e| {
                format!(r#"{{"error":"could not serialise response: {e}"}}"#).into_bytes()
            }),
        }
    }

    fn ok(value: &impl Serialize) -> ApiResponse {
        ApiResponse::json(200, value)
    }

    fn error(status: u16, message: impl std::fmt::Display) -> ApiResponse {
        #[derive(Serialize)]
        struct ErrorBody {
            error: String,
        }
        ApiResponse::json(
            status,
            &ErrorBody {
                error: message.to_string(),
            },
        )
    }

    fn empty(status: u16) -> ApiResponse {
        ApiResponse {
            status,
            content_type: "application/json",
            body: Vec::new(),
        }
    }

    pub fn parse<T: for<'de> Deserialize<'de>>(&self) -> Result<T, serde_json::Error> {
        serde_json::from_slice(&self.body)
    }
}

struct Inner {
    config: Config,
    clients: Vec<PairedClient>,
    auth: Auth,
    next_client_id: u32,
    pairing: Pairing,
}

/// Everything the API operates on.
pub struct AppState {
    store: Store,
    host: Arc<dyn Host>,
    inner: Mutex<Inner>,
}

impl AppState {
    /// Load from disk, generating a token on first run.
    ///
    /// A token is minted even for a loopback bind, so that changing the bind
    /// address later does not silently produce an unauthenticated LAN service
    /// in the window before someone remembers to set one.
    pub fn load(store: Store, host: Arc<dyn Host>) -> Result<AppState, StoreError> {
        let mut config = store.load_config()?;
        let clients = store.load_clients()?;

        if config.web.token.is_empty() {
            config.web.token = random::token()?;
            store.save_config(&config)?;
        }

        let next_client_id = clients.iter().map(|c| c.id).max().map_or(1, |m| m + 1);
        let auth = Auth::from_config(&config.web);

        Ok(AppState {
            store,
            host,
            inner: Mutex::new(Inner {
                config,
                clients,
                auth,
                next_client_id,
                pairing: Pairing::new(),
            }),
        })
    }

    /// The token to print at startup, so it can be found without opening a file.
    pub fn token(&self) -> String {
        self.inner
            .lock()
            .expect("not poisoned")
            .config
            .web
            .token
            .clone()
    }

    /// The UDP port the control channel and video share.
    pub fn stream_port(&self) -> u16 {
        self.inner.lock().expect("not poisoned").config.stream.port
    }

    pub fn web_config(&self) -> WebConfig {
        self.inner.lock().expect("not poisoned").config.web.clone()
    }

    pub fn host(&self) -> &Arc<dyn Host> {
        &self.host
    }

    /// Look up a paired client's secret, for the control-plane handshake.
    pub fn client_secret(&self, client_id: u32) -> Option<[u8; 32]> {
        self.inner
            .lock()
            .expect("not poisoned")
            .clients
            .iter()
            .find(|c| c.id == client_id)
            .map(|c| c.secret)
    }

    /// The decoder quirks a client reported at pairing, for seeding a session
    /// before any fresh `DecoderQuirks` message arrives.
    pub fn client_quirks(&self, client_id: u32) -> Option<sunburst_core::proto::DecoderQuirks> {
        self.inner
            .lock()
            .expect("not poisoned")
            .clients
            .iter()
            .find(|c| c.id == client_id)
            .map(|c| c.quirks.into())
    }

    /// Whether pairing is open. Consulted before any unauthenticated packet is
    /// looked at.
    pub fn pairing_armed(&self, now: u64) -> bool {
        self.inner
            .lock()
            .expect("not poisoned")
            .pairing
            .is_armed(now)
    }

    /// Take a pair request from the control channel.
    pub fn pair_request(
        &self,
        request: crate::pairing::PairRequest,
        now: u64,
    ) -> Option<(u32, [u8; 16])> {
        self.inner
            .lock()
            .expect("not poisoned")
            .pairing
            .receive_request(request, now)
            .ok()
    }

    /// Take the client's confirmation tag. The PIN is typed in the web UI, so
    /// this only records; nothing is paired until `/api/pair/confirm`.
    pub fn pair_confirm(&self, request_id: u32, tag: [u8; 8], now: u64) {
        let _ = self
            .inner
            .lock()
            .expect("not poisoned")
            .pairing
            .receive_confirm(request_id, tag, now);
    }

    /// Every paired client's session key.
    ///
    /// Derived per call rather than cached, so a revoke takes effect at once.
    pub fn client_keys(&self) -> Vec<(u32, SessionKey)> {
        self.inner
            .lock()
            .expect("not poisoned")
            .clients
            .iter()
            .map(|c| (c.id, SessionKey::from_bytes(c.secret)))
            .collect()
    }

    /// Record that a client was heard from, for the UI's "last seen" column.
    pub fn touch_last_seen(&self, client_id: u32, now: u64) {
        let mut inner = self.inner.lock().expect("not poisoned");
        if let Some(client) = inner.clients.iter_mut().find(|c| c.id == client_id) {
            client.last_seen = Some(now);
        }
    }

    /// Launch an app by id, the way the control channel asks for it.
    ///
    /// Shares the path the web UI uses, so the two cannot diverge on which
    /// entries exist or what "already running" means.
    pub fn launch(&self, app_id: u32) -> Result<(), String> {
        let app = {
            let inner = self.inner.lock().expect("not poisoned");
            inner.config.app(app_id).cloned()
        };
        let app = app.ok_or_else(|| format!("no such app: {app_id}"))?;
        self.host
            .launch(&app)
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// The catalogue as the client sees it: names and ids, nothing else.
    pub fn app_list(&self) -> Vec<AppListing> {
        self.inner
            .lock()
            .expect("not poisoned")
            .config
            .apps
            .iter()
            .map(|a| AppListing {
                id: a.id,
                name: a.name.clone(),
            })
            .collect()
    }
}

/// What the client is told about an app. Names and ids only; box art is a later
/// phase and the control channel is the wrong carrier for it anyway.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AppListing {
    pub id: u32,
    pub name: String,
}

#[derive(Serialize)]
struct StatusBody {
    #[serde(flatten)]
    host: HostStatus,
    autostart: Option<bool>,
    running_app: Option<RunningApp>,
    paired_clients: usize,
    sessions: usize,
    lan_exposed: bool,
}

#[derive(Serialize, Deserialize)]
pub struct SettingsBody {
    pub web: WebConfig,
    pub stream: StreamConfig,
}

#[derive(Serialize)]
struct ArmedBody {
    expires_at: u64,
}

#[derive(Serialize)]
struct PendingBody {
    id: u32,
    name: String,
    model: String,
    abi: String,
    received_at: u64,
    awaiting_client: bool,
}

#[derive(Deserialize)]
struct ConfirmBody {
    request_id: u32,
    pin: String,
    /// Optional rename, so a device can be called "Living room" rather than
    /// whatever model string it reported.
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
struct AutostartBody {
    enabled: bool,
}

/// Route and handle. `now` is passed in so pairing expiry is testable.
pub fn dispatch(state: &AppState, req: &ApiRequest, now: u64) -> ApiResponse {
    let path = req.path.as_str();

    // Auth first, before anything is read or written. The one exception is
    // nothing: every /api route requires it.
    {
        let inner = state.inner.lock().expect("not poisoned");
        if !inner.auth.authorize(req.authorization.as_deref()) {
            return ApiResponse::error(401, "unauthorized");
        }
    }

    let segments: Vec<&str> = path
        .trim_start_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();

    match (req.method.as_str(), segments.as_slice()) {
        ("GET", ["api", "status"]) => status(state),
        ("GET", ["api", "metrics"]) => metrics(state),

        ("GET", ["api", "config"]) => settings(state),
        ("PUT", ["api", "config"]) => update_settings(state, req),

        ("GET", ["api", "clients"]) => list_clients(state),
        ("DELETE", ["api", "clients", id]) => revoke_client(state, id),

        ("POST", ["api", "pair", "arm"]) => arm(state, now),
        ("POST", ["api", "pair", "disarm"]) => disarm(state),
        ("GET", ["api", "pair", "pending"]) => pending(state, now),
        ("POST", ["api", "pair", "confirm"]) => confirm(state, req, now),

        ("GET", ["api", "apps"]) => list_apps(state),
        ("POST", ["api", "apps"]) => create_app(state, req),
        ("PUT", ["api", "apps", id]) => update_app(state, req, id),
        ("DELETE", ["api", "apps", id]) => delete_app(state, id),
        ("POST", ["api", "apps", id, "launch"]) => launch_app(state, id),
        ("POST", ["api", "apps", "terminate"]) => terminate_app(state),

        ("GET", ["api", "sessions"]) => sessions(state),
        ("DELETE", ["api", "sessions", id]) => disconnect(state, id),

        ("POST", ["api", "autostart"]) => set_autostart(state, req),
        ("POST", ["api", "restart"]) => restart(state),

        (_, ["api", ..]) => ApiResponse::error(404, "no such endpoint"),
        _ => ApiResponse::error(404, "not found"),
    }
}

fn status(state: &AppState) -> ApiResponse {
    let inner = state.inner.lock().expect("not poisoned");
    ApiResponse::ok(&StatusBody {
        host: state.host.status(),
        // A host that cannot answer is reported as unknown rather than as
        // "disabled", which would be a confident wrong answer.
        autostart: state.host.autostart().ok(),
        running_app: state.host.running_app(),
        paired_clients: inner.clients.len(),
        sessions: state.host.sessions().len(),
        lan_exposed: inner.config.web.is_lan_exposed(),
    })
}

fn metrics(state: &AppState) -> ApiResponse {
    match state.host.metrics() {
        Some(report) => ApiResponse::ok(&MetricsRecord::from(&report)),
        None => ApiResponse::error(503, "no instrumentation drain is running"),
    }
}

fn settings(state: &AppState) -> ApiResponse {
    let inner = state.inner.lock().expect("not poisoned");
    ApiResponse::ok(&SettingsBody {
        web: inner.config.web.clone(),
        stream: inner.config.stream.clone(),
    })
}

fn update_settings(state: &AppState, req: &ApiRequest) -> ApiResponse {
    let update: SettingsBody = match serde_json::from_slice(&req.body) {
        Ok(u) => u,
        Err(e) => return ApiResponse::error(400, e),
    };

    let mut inner = state.inner.lock().expect("not poisoned");
    // Apps are managed through /api/apps. Taking them from this body would let a
    // settings save wipe the catalogue, which is a bad way to learn that the UI
    // sent a partial object.
    let mut candidate = inner.config.clone();
    candidate.web = update.web;
    candidate.stream = update.stream;

    if let Err(e) = state.store.save_config(&candidate) {
        return ApiResponse::error(400, e);
    }
    inner.auth = Auth::from_config(&candidate.web);
    inner.config = candidate;

    ApiResponse::ok(&SettingsBody {
        web: inner.config.web.clone(),
        stream: inner.config.stream.clone(),
    })
}

fn list_clients(state: &AppState) -> ApiResponse {
    let inner = state.inner.lock().expect("not poisoned");
    let public: Vec<PublicClient> = inner.clients.iter().map(PairedClient::public).collect();
    ApiResponse::ok(&public)
}

fn revoke_client(state: &AppState, id: &str) -> ApiResponse {
    let Some(id) = parse_id(id) else {
        return ApiResponse::error(400, "client id must be a number");
    };

    let mut inner = state.inner.lock().expect("not poisoned");
    let before = inner.clients.len();
    inner.clients.retain(|c| c.id != id);
    if inner.clients.len() == before {
        return ApiResponse::error(404, "no such client");
    }

    // Persist immediately. A revoke that lives only in memory would come back
    // at the next restart, which is the opposite of what was asked for.
    if let Err(e) = state.store.save_clients(&inner.clients) {
        return ApiResponse::error(500, e);
    }
    ApiResponse::empty(204)
}

fn arm(state: &AppState, now: u64) -> ApiResponse {
    let mut inner = state.inner.lock().expect("not poisoned");
    match inner.pairing.arm(now) {
        Ok(expires_at) => ApiResponse::ok(&ArmedBody { expires_at }),
        Err(e) => ApiResponse::error(500, e),
    }
}

fn disarm(state: &AppState) -> ApiResponse {
    state.inner.lock().expect("not poisoned").pairing.disarm();
    ApiResponse::empty(204)
}

fn pending(state: &AppState, now: u64) -> ApiResponse {
    let mut inner = state.inner.lock().expect("not poisoned");
    let body: Vec<PendingBody> = inner
        .pairing
        .pending(now)
        .iter()
        .map(|p| PendingBody {
            id: p.id,
            name: p.name.clone(),
            model: p.model.clone(),
            abi: p.abi.clone(),
            received_at: p.received_at,
            // The UI needs to know whether typing a PIN can work yet.
            awaiting_client: p.tag.is_none(),
        })
        .collect();
    ApiResponse::ok(&body)
}

fn confirm(state: &AppState, req: &ApiRequest, now: u64) -> ApiResponse {
    let body: ConfirmBody = match serde_json::from_slice(&req.body) {
        Ok(b) => b,
        Err(e) => return ApiResponse::error(400, e),
    };

    let mut inner = state.inner.lock().expect("not poisoned");
    let client_id = inner.next_client_id;

    let mut paired = match inner
        .pairing
        .confirm(body.request_id, &body.pin, client_id, now)
    {
        Ok(c) => c,
        Err(e) => {
            let status = match e {
                PairingError::WrongPin { .. } | PairingError::TooManyAttempts => 403,
                PairingError::UnknownRequest(_) => 404,
                PairingError::NotArmed | PairingError::NotConfirmed => 409,
                PairingError::Random(_) => 500,
            };
            return ApiResponse::error(status, e);
        }
    };

    if let Some(name) = body.name.filter(|n| !n.trim().is_empty()) {
        paired.name = name;
    }

    let public = paired.public();
    inner.next_client_id += 1;
    inner.clients.push(paired);

    if let Err(e) = state.store.save_clients(&inner.clients) {
        // Roll back rather than report a pairing that will not survive a
        // restart. A device that appears paired and is not is worse than one
        // that visibly failed to pair.
        inner.clients.pop();
        inner.next_client_id -= 1;
        return ApiResponse::error(500, e);
    }
    ApiResponse::json(201, &public)
}

fn list_apps(state: &AppState) -> ApiResponse {
    let inner = state.inner.lock().expect("not poisoned");
    ApiResponse::ok(&inner.config.apps)
}

fn create_app(state: &AppState, req: &ApiRequest) -> ApiResponse {
    let mut entry: AppEntry = match serde_json::from_slice(&req.body) {
        Ok(e) => e,
        Err(e) => return ApiResponse::error(400, e),
    };

    let mut inner = state.inner.lock().expect("not poisoned");
    let mut candidate = inner.config.clone();
    // The id is the server's to assign; whatever the body claimed is ignored so
    // a client cannot collide with an existing entry.
    entry.id = candidate.allocate_app_id();
    candidate.apps.push(entry.clone());

    if let Err(e) = state.store.save_config(&candidate) {
        return ApiResponse::error(400, e);
    }
    inner.config = candidate;
    ApiResponse::json(201, &entry)
}

fn update_app(state: &AppState, req: &ApiRequest, id: &str) -> ApiResponse {
    let Some(id) = parse_id(id) else {
        return ApiResponse::error(400, "app id must be a number");
    };
    let mut entry: AppEntry = match serde_json::from_slice(&req.body) {
        Ok(e) => e,
        Err(e) => return ApiResponse::error(400, e),
    };
    entry.id = id;

    let mut inner = state.inner.lock().expect("not poisoned");
    let mut candidate = inner.config.clone();
    let Some(slot) = candidate.app_mut(id) else {
        return ApiResponse::error(404, "no such app");
    };
    *slot = entry.clone();

    if let Err(e) = state.store.save_config(&candidate) {
        return ApiResponse::error(400, e);
    }
    inner.config = candidate;
    ApiResponse::ok(&entry)
}

fn delete_app(state: &AppState, id: &str) -> ApiResponse {
    let Some(id) = parse_id(id) else {
        return ApiResponse::error(400, "app id must be a number");
    };

    let mut inner = state.inner.lock().expect("not poisoned");
    let mut candidate = inner.config.clone();
    let before = candidate.apps.len();
    candidate.apps.retain(|a| a.id != id);
    if candidate.apps.len() == before {
        return ApiResponse::error(404, "no such app");
    }

    if let Err(e) = state.store.save_config(&candidate) {
        return ApiResponse::error(400, e);
    }
    inner.config = candidate;
    ApiResponse::empty(204)
}

fn launch_app(state: &AppState, id: &str) -> ApiResponse {
    let Some(id) = parse_id(id) else {
        return ApiResponse::error(400, "app id must be a number");
    };

    let app = {
        let inner = state.inner.lock().expect("not poisoned");
        match inner.config.app(id) {
            Some(a) => a.clone(),
            None => return ApiResponse::error(404, "no such app"),
        }
    };

    match state.host.launch(&app) {
        Ok(running) => ApiResponse::ok(&running),
        Err(e) => ApiResponse::error(host_status(&e), e),
    }
}

fn terminate_app(state: &AppState) -> ApiResponse {
    match state.host.terminate() {
        Ok(()) => ApiResponse::empty(204),
        Err(e) => ApiResponse::error(host_status(&e), e),
    }
}

fn sessions(state: &AppState) -> ApiResponse {
    let sessions: Vec<SessionSummary> = state.host.sessions();
    ApiResponse::ok(&sessions)
}

fn disconnect(state: &AppState, id: &str) -> ApiResponse {
    let Some(id) = parse_id(id) else {
        return ApiResponse::error(400, "session id must be a number");
    };
    match state.host.disconnect(id) {
        Ok(()) => ApiResponse::empty(204),
        Err(e) => ApiResponse::error(host_status(&e), e),
    }
}

fn set_autostart(state: &AppState, req: &ApiRequest) -> ApiResponse {
    let body: AutostartBody = match serde_json::from_slice(&req.body) {
        Ok(b) => b,
        Err(e) => return ApiResponse::error(400, e),
    };
    match state.host.set_autostart(body.enabled) {
        Ok(()) => ApiResponse::empty(204),
        Err(e) => ApiResponse::error(host_status(&e), e),
    }
}

fn restart(state: &AppState) -> ApiResponse {
    match state.host.restart_self() {
        // Answered before the process goes away, so the UI can say what is
        // happening rather than showing a dropped connection.
        Ok(()) => ApiResponse::empty(202),
        Err(e) => ApiResponse::error(host_status(&e), e),
    }
}

fn host_status(e: &HostError) -> u16 {
    match e {
        HostError::UnknownApp(_) | HostError::UnknownSession(_) => 404,
        HostError::AlreadyRunning(_) => 409,
        HostError::Unsupported => 501,
        HostError::Failed(_) => 500,
    }
}

fn parse_id(raw: &str) -> Option<u32> {
    raw.parse().ok()
}

#[cfg(test)]
mod tests;
