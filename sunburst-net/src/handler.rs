// SPDX-License-Identifier: GPL-2.0-or-later

//! What the endpoint does with what arrives.
//!
//! A trait rather than a direct call into `sunburst-web`, and the reason is the
//! single socket: PROTOCOL.md puts video and control on one port, so the socket
//! belongs to the crate that will also carry video. `sunburst-net` cannot depend
//! on `sunburst-web` without dragging `hyper` onto the frame path, so the
//! dependency inverts.
//!
//! It buys the same thing `sunburst_web::host::Host` bought: [`on_input`] is
//! where `sunburst-input` attaches on Windows, and [`Recording`] makes the whole
//! path testable here.
//!
//! [`on_input`]: ControlHandler::on_input

use std::net::SocketAddr;
use std::sync::Arc;

use sunburst_core::proto::pairing::{NONCE_LEN, TAG_LEN};
use sunburst_core::proto::{
    AppListing, ArtRef, DecoderQuirks, Feedback, Hello, InputEvent, PadOutput, PairRequest, Rumble,
    Seq16, ServerControl, SessionConfig, SessionKey, StreamCodec,
};

/// Per-session stream settings the handler resolves before a session starts —
/// the whole effective stream config (the global defaults with the running app's
/// overrides merged in, `Config::effective`), so the manager reads every knob
/// live per session rather than from a value frozen at startup.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionSettings {
    /// `None` = auto (the manager negotiates AV1 when offered, else HEVC).
    pub codec: Option<StreamCodec>,
    pub bitrate_kbps: u32,
    /// Rate-controller floor/ceiling; `max = 0` means "use `bitrate_kbps`".
    pub min_bitrate_kbps: u32,
    pub max_bitrate_kbps: u32,
    pub hdr: bool,
    /// NVENC preset P1–P4 (1..=4, clamped by the encoder).
    pub preset: u8,
    /// Variable-bitrate rate control (else CBR).
    pub vbr: bool,
    /// Slice/tile override; `0` = the codec default.
    pub slices: u8,
    /// Forced IDR period in frames; `0` = infinite GOP.
    pub idr_period: u32,
    pub dpb_depth: u8,
    pub capture_backend: CaptureBackend,
    /// DXGI output index to capture; `None` = primary.
    pub capture_output: Option<u32>,
    /// Match the client's resolution on the physical display.
    pub match_resolution: bool,
    /// Capture the virtual display (MTT VDD) instead of the physical one.
    pub virtual_display: bool,
    /// Cap the encode frame rate; `0` = follow the client.
    pub fps_cap: u32,
    pub audio: bool,
    pub audio_device: Option<String>,
    /// Render endpoint the pads' headset mic is played into (a consumed virtual
    /// microphone such as "Steam Streaming Microphone"), matched by name.
    /// `None`/empty disables the pad-mic path — no render sink is opened.
    pub mic_device: Option<String>,
    pub audio_bitrate_kbps: u32,
    pub audio_frame_us: u32,
    pub audio_fec: bool,
    pub audio_complexity: u8,
    /// Turn Enhanced Pointer Precision off (server-side) for the session.
    pub disable_epp: bool,
    /// The injector's mouse sensitivity × 1000, so the manager can tell the
    /// client the pointer gain. Milli, not `f32`, so the settings stay `Eq`.
    pub mouse_sensitivity_milli: u32,
}

/// Which capture backend the session should use (mirrors the web config, kept
/// here so this trait crate needs no dependency on `sunburst-web`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CaptureBackend {
    /// The OS default: WGC on Win11, DDA on Win10.
    #[default]
    Auto,
    Wgc,
    Dda,
    /// Opt-in resilience only (CLAUDE.md); never automatic.
    Nvfbc,
}

/// A server → client message queued by a producer for the endpoint to send.
///
/// The seam every server-originated message crosses. Only the socket-owning
/// endpoint may transmit, and producers run on other threads — the injector's
/// rumble callback now, and in Phase 3 the `SessionConfig`, `CursorShape`,
/// `CursorPosition` and `SecureDesktop` messages. So a producer enqueues an
/// `Outbound` and the endpoint drains it each [`ControlHandler::drain_outbound`]
/// without the producer ever touching the socket.
///
/// Shaped for both reliabilities deliberately, because its users split on that:
/// control is retransmitted, rumble is not.
#[derive(Clone, Debug)]
pub enum Outbound {
    /// Reliable control, framed and retransmitted like any [`ServerControl`].
    Control { client: u32, message: ServerControl },
    /// Unreliable, latest-wins rumble (packet type 6). A superseded level is
    /// worthless, so it is never retransmitted — the point of not using the
    /// reliable channel.
    Rumble { client: u32, rumble: Rumble },
    /// Unreliable, latest-wins rich pad output (packet type 7): motors, adaptive
    /// triggers, LED. Same reasoning as [`Outbound::Rumble`], for rich pads.
    PadOutput { client: u32, output: PadOutput },
}

/// Where authenticated input goes once it has been verified.
///
/// Separate from [`ControlHandler`] because it is the one seam that is genuinely
/// platform-bound: `sunburst-input` implements it against `SendInput` on
/// Windows. It lives here rather than in `sunburst-web` so that the crate doing
/// the injecting does not have to depend on `hyper` to reach the trait.
/// Input tuning the injector applies live: mouse-delta scaling and an extra
/// stick deadzone. Sent by the handler when a session starts.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InputSettings {
    pub mouse_sensitivity: f32,
    pub gamepad_deadzone: f32,
}

impl Default for InputSettings {
    fn default() -> Self {
        InputSettings {
            mouse_sensitivity: 1.0,
            gamepad_deadzone: 0.0,
        }
    }
}

pub trait InputSink: Send {
    /// One verified, fresh input event. `seq` is its `InputPacket.input_seq`,
    /// which a sink that applies mouse motion reports back (see
    /// `ServerControl::CursorPosition::input_seq`).
    fn inject(&mut self, client: u32, seq: u32, event: InputEvent);

    /// Apply input tuning (mouse sensitivity, deadzone). Default no-op — tests
    /// and `NoInput` ignore it.
    fn configure(&mut self, _settings: InputSettings) {}

    /// A client announced a pad. The injector plugs a virtual controller of the
    /// declared type and readies its encoding session. Default no-op — most sinks
    /// (tests, `NoInput`) do not manage pads.
    fn pad_connected(&mut self, _client: u32, _pad_index: u8, _pad_type: u8, _capabilities: u16) {}

    /// A client removed a pad; the injector unplugs it.
    fn pad_disconnected(&mut self, _client: u32, _pad_index: u8) {}

    /// Server → client messages this sink has produced since the last call — the
    /// output effects (rumble / adaptive triggers / LED) a game wrote to the pads.
    /// Drained each tick and handed to the endpoint. Default none.
    fn drain_outbound(&mut self) -> Vec<Outbound> {
        Vec::new()
    }
}

/// Drops input. What the server uses until an injector is attached.
pub struct NoInput;

impl InputSink for NoInput {
    fn inject(&mut self, _client: u32, _seq: u32, _event: InputEvent) {}
}

/// The video-session half of what the endpoint drives, kept separate from
/// [`InputSink`] because it is a different concern with a different lifetime: a
/// session begins at `Hello` and ends at `Bye`, and produces the server→client
/// video-control messages (`SessionConfig`, then `CodecPrivate`, cursor updates,
/// `SecureDesktop`).
///
/// A `Hello` from a paired client returns a [`SessionConfig`]; the endpoint
/// signs it with the pairing key, sends it reliably, and derives the per-session
/// input key from its nonce. Everything after — codec headers, cursor, secure
/// desktop — arrives asynchronously from the pipeline and cursor threads through
/// [`drain_outbound`](StreamControl::drain_outbound), so this trait is the seam
/// the Windows `SessionManager` implements and [`NoStream`] fills for tests.
pub trait StreamControl: Send {
    /// A paired client said `Hello`. Return the session it should get, or `None`
    /// to decline (already streaming to someone, or an unsatisfiable codec).
    fn session_start(
        &mut self,
        client: u32,
        from: SocketAddr,
        hello: &Hello,
        quirks: DecoderQuirks,
        settings: SessionSettings,
    ) -> Option<SessionConfig>;

    /// The client's session ended (`Bye`, idle timeout, or a UI disconnect).
    fn session_stop(&mut self, client: u32);

    /// An updated decoder-quirks report; the encoder adapts.
    fn on_quirks(&mut self, _client: u32, _quirks: DecoderQuirks) {}

    /// A last-resort keyframe request. Prefer NACK-driven recovery.
    fn on_request_idr(&mut self, _client: u32) {}

    /// The client changed resolution or refresh rate.
    fn on_resize(&mut self, _client: u32, _width: u32, _height: u32, _refresh_mhz: u32) {}

    /// A NACK body (MAC already stripped) for `frame_id`: a list of missing
    /// packet indices to retransmit, or empty to abandon the frame.
    fn on_nack(&mut self, _client: u32, _frame_id: Seq16, _body: &[u8]) {}

    /// A periodic client feedback report; drives rate control.
    fn on_feedback(&mut self, _client: u32, _feedback: Feedback) {}

    /// A pad's headset-mic Opus frame (client → server). Default no-op — the
    /// Windows `SessionManager` decodes it and renders it to the configured
    /// virtual microphone for the active session.
    fn on_audio_in(&mut self, _client: u32, _pad_index: u8, _seq: Seq16, _payload: &[u8]) {}

    /// Server→client video-control messages produced since the last call —
    /// `CodecPrivate`, cursor updates, `SecureDesktop`. Drained each tick.
    fn drain_outbound(&mut self) -> Vec<Outbound> {
        Vec::new()
    }
}

/// Starts no sessions. What the server uses until a `SessionManager` is attached,
/// and what the pairing-only and input-only tests use.
pub struct NoStream;

impl StreamControl for NoStream {
    fn session_start(
        &mut self,
        _client: u32,
        _from: SocketAddr,
        _hello: &Hello,
        _quirks: DecoderQuirks,
        _settings: SessionSettings,
    ) -> Option<SessionConfig> {
        None
    }

    fn session_stop(&mut self, _client: u32) {}
}

pub trait ControlHandler: Send {
    /// Whether unauthenticated pairing messages may be processed at all.
    ///
    /// The endpoint asks before touching a packet with no MAC. Unarmed means the
    /// packet is dropped silently — an unsolicited pair request is the normal
    /// state of the world, not an incident.
    fn pairing_armed(&self, now: u64) -> bool;

    /// Returns the request id and the server nonce to challenge with.
    fn on_pair_request(&mut self, request: PairRequest, now: u64)
    -> Option<(u32, [u8; NONCE_LEN])>;

    fn on_pair_confirm(&mut self, request_id: u32, tag: [u8; TAG_LEN], now: u64);

    /// Every paired client's key, for selecting which one verifies a packet.
    ///
    /// These are the long-lived pairing keys: they verify `Hello` and the
    /// reliable control channel, and identify a client on its first packet from
    /// a new address. Re-read rather than cached, because a revoke has to take
    /// effect immediately: a client removed in the web UI must stop being able
    /// to send input, not stop at the next restart.
    fn client_keys(&self) -> Vec<(u32, SessionKey)>;

    /// A paired client's raw pairing secret, for deriving its per-session input
    /// key once `Hello` has supplied both nonces. `None` if it is not paired.
    fn client_secret(&self, client: u32) -> Option<[u8; 32]>;

    /// The client announced itself. Returns the [`SessionConfig`] to answer
    /// with — the endpoint signs it with the pairing key, sends it reliably, and
    /// installs the derived session key — or `None` to start no video session.
    fn on_hello(&mut self, client: u32, from: SocketAddr, hello: Hello) -> Option<SessionConfig>;

    fn on_app_list(&mut self) -> Vec<AppListing>;
    fn on_launch(&mut self, app_id: u32) -> Result<(), String>;

    /// An app's box art: what the listing says about it and the bytes. Default
    /// none. Answered from memory — the endpoint thread also receives input,
    /// and must not wait on a disk.
    fn on_art(&mut self, _app_id: u32) -> Option<(ArtRef, Arc<[u8]>)> {
        None
    }

    /// An updated decoder-quirks report.
    fn on_quirks(&mut self, _client: u32, _quirks: DecoderQuirks) {}

    /// A last-resort keyframe request.
    fn on_request_idr(&mut self, _client: u32) {}

    /// A client resolution or refresh change.
    fn on_resize(&mut self, _client: u32, _width: u32, _height: u32, _refresh_mhz: u32) {}

    /// A verified NACK body (MAC stripped) for `frame_id`.
    fn on_nack(&mut self, _client: u32, _frame_id: Seq16, _body: &[u8]) {}

    /// A verified periodic feedback report.
    fn on_feedback(&mut self, _client: u32, _feedback: Feedback) {}

    /// A session ended. Called from the same places as [`on_bye`](Self::on_bye)
    /// plus idle timeout, so a handler managing a pipeline can tear it down.
    fn session_stop(&mut self, _client: u32) {}

    /// Where `sunburst-input` attaches on Windows.
    fn on_input(&mut self, client: u32, seq: u32, event: InputEvent);

    /// A client connected / disconnected a pad. Default no-op — a handler that
    /// manages virtual controllers (the Windows one) forwards these to its
    /// [`InputSink`]. Reliable, so a plug or unplug is never lost.
    fn on_pad_connected(
        &mut self,
        _client: u32,
        _pad_index: u8,
        _pad_type: u8,
        _capabilities: u16,
    ) {
    }
    fn on_pad_disconnected(&mut self, _client: u32, _pad_index: u8) {}

    /// A pad's headset-mic Opus frame (client → server), tagged with the pad it
    /// came from. Default no-op — the Windows handler renders it to the virtual
    /// microphone. Unauthenticated, like the outbound audio: media rides the LAN
    /// without a MAC (see [`crate::endpoint`]), so `client` is attributed by
    /// source address and may be any paired client the endpoint has seen.
    fn on_audio_in(&mut self, _client: u32, _pad_index: u8, _seq: Seq16, _payload: &[u8]) {}

    fn on_bye(&mut self, client: u32);

    /// Server → client messages produced since the last call, for the endpoint
    /// to send. Called each tick; the default is none, so a handler that never
    /// originates anything (most tests, the pairing path) ignores it.
    fn drain_outbound(&mut self) -> Vec<Outbound> {
        Vec::new()
    }
}

/// A handler that records what it was told, for tests.
///
/// Shipped rather than hidden behind a feature, for the same reason
/// `sunburst_web::host::Fake` is: a test double that is awkward to reach does
/// not get used.
#[derive(Default)]
pub struct Recording {
    pub armed: bool,
    pub keys: Vec<(u32, SessionKey)>,
    /// Raw pairing secrets, for [`ControlHandler::client_secret`]. A test that
    /// exercises the session-key switch registers one with [`Recording::with_secret`].
    pub secrets: Vec<(u32, [u8; 32])>,
    pub apps: Vec<AppListing>,
    pub server_nonce: [u8; NONCE_LEN],
    /// What [`ControlHandler::on_hello`] returns. `None` starts no session, which
    /// is what the pairing-only and input-only tests want.
    pub session_config: Option<SessionConfig>,

    pub pair_requests: Vec<PairRequest>,
    pub pair_confirms: Vec<(u32, [u8; TAG_LEN])>,
    pub hellos: Vec<(u32, SocketAddr, Hello)>,
    pub inputs: Vec<(u32, InputEvent)>,
    /// The `input_seq` of each entry in `inputs`.
    pub input_seqs: Vec<u32>,
    pub nacks: Vec<(u32, Seq16, Vec<u8>)>,
    pub feedbacks: Vec<(u32, Feedback)>,
    pub quirks: Vec<(u32, DecoderQuirks)>,
    pub idr_requests: Vec<u32>,
    pub resizes: Vec<(u32, u32, u32, u32)>,
    pub pad_connects: Vec<(u32, u8, u8, u16)>,
    pub pad_disconnects: Vec<(u32, u8)>,
    /// `(client, pad_index, seq, opus payload)` for each pad-mic frame received.
    pub audio_ins: Vec<(u32, u8, Seq16, Vec<u8>)>,
    pub launches: Vec<u32>,
    /// What `on_launch` fails with, if set.
    pub launch_error: Option<String>,
    /// Box art by app id, for `on_art`.
    pub art: Vec<(u32, Arc<[u8]>)>,
    pub art_requests: Vec<u32>,
    pub app_list_calls: usize,
    pub byes: Vec<u32>,
    pub stops: Vec<u32>,
    /// Messages a test wants the endpoint to send back out. Drained each tick.
    pub outbound: Vec<Outbound>,
    next_request_id: u32,
}

impl Recording {
    pub fn new() -> Recording {
        Recording::default()
    }

    #[must_use]
    pub fn armed(mut self) -> Recording {
        self.armed = true;
        self
    }

    #[must_use]
    pub fn with_key(mut self, client: u32, key: SessionKey) -> Recording {
        self.keys.push((client, key));
        self
    }

    #[must_use]
    pub fn with_apps(mut self, apps: Vec<AppListing>) -> Recording {
        self.apps = apps;
        self
    }

    /// Box art `on_art` answers with, by app id.
    pub fn with_art(mut self, app_id: u32, bytes: Vec<u8>) -> Recording {
        self.art.push((app_id, bytes.into()));
        self
    }

    /// Make every launch fail with `message`.
    pub fn with_launch_error(mut self, message: &str) -> Recording {
        self.launch_error = Some(message.into());
        self
    }

    /// Register a client by its raw secret: both the pairing key (for the scan)
    /// and the secret (for session-key derivation). For the switch tests.
    #[must_use]
    pub fn with_secret(mut self, client: u32, secret: [u8; 32]) -> Recording {
        self.keys.push((client, SessionKey::from_bytes(secret)));
        self.secrets.push((client, secret));
        self
    }

    /// The `SessionConfig` `on_hello` will answer with.
    #[must_use]
    pub fn with_session_config(mut self, config: SessionConfig) -> Recording {
        self.session_config = Some(config);
        self
    }
}

/// Lets a test keep a handle on the handler the endpoint has taken ownership of.
///
/// The endpoint runs on its own thread and owns its handler, so without this a
/// test could set one up but never read what it recorded. Permitted because the
/// trait is local, even though `Arc` and `Mutex` are not.
impl<H: ControlHandler> ControlHandler for std::sync::Arc<std::sync::Mutex<H>> {
    fn pairing_armed(&self, now: u64) -> bool {
        self.lock().expect("not poisoned").pairing_armed(now)
    }

    fn on_pair_request(
        &mut self,
        request: PairRequest,
        now: u64,
    ) -> Option<(u32, [u8; NONCE_LEN])> {
        self.lock()
            .expect("not poisoned")
            .on_pair_request(request, now)
    }

    fn on_pair_confirm(&mut self, request_id: u32, tag: [u8; TAG_LEN], now: u64) {
        self.lock()
            .expect("not poisoned")
            .on_pair_confirm(request_id, tag, now);
    }

    fn client_keys(&self) -> Vec<(u32, SessionKey)> {
        self.lock().expect("not poisoned").client_keys()
    }

    fn client_secret(&self, client: u32) -> Option<[u8; 32]> {
        self.lock().expect("not poisoned").client_secret(client)
    }

    fn on_hello(&mut self, client: u32, from: SocketAddr, hello: Hello) -> Option<SessionConfig> {
        self.lock()
            .expect("not poisoned")
            .on_hello(client, from, hello)
    }

    fn on_app_list(&mut self) -> Vec<AppListing> {
        self.lock().expect("not poisoned").on_app_list()
    }

    fn on_launch(&mut self, app_id: u32) -> Result<(), String> {
        self.lock().expect("not poisoned").on_launch(app_id)
    }

    fn on_art(&mut self, app_id: u32) -> Option<(ArtRef, Arc<[u8]>)> {
        self.lock().expect("not poisoned").on_art(app_id)
    }

    fn on_input(&mut self, client: u32, seq: u32, event: InputEvent) {
        self.lock()
            .expect("not poisoned")
            .on_input(client, seq, event);
    }

    fn on_quirks(&mut self, client: u32, quirks: DecoderQuirks) {
        self.lock().expect("not poisoned").on_quirks(client, quirks);
    }

    fn on_request_idr(&mut self, client: u32) {
        self.lock().expect("not poisoned").on_request_idr(client);
    }

    fn on_resize(&mut self, client: u32, width: u32, height: u32, refresh_mhz: u32) {
        self.lock()
            .expect("not poisoned")
            .on_resize(client, width, height, refresh_mhz);
    }

    fn on_nack(&mut self, client: u32, frame_id: Seq16, body: &[u8]) {
        self.lock()
            .expect("not poisoned")
            .on_nack(client, frame_id, body);
    }

    fn on_feedback(&mut self, client: u32, feedback: Feedback) {
        self.lock()
            .expect("not poisoned")
            .on_feedback(client, feedback);
    }

    fn session_stop(&mut self, client: u32) {
        self.lock().expect("not poisoned").session_stop(client);
    }

    fn on_pad_connected(&mut self, client: u32, pad_index: u8, pad_type: u8, capabilities: u16) {
        self.lock().expect("not poisoned").on_pad_connected(
            client,
            pad_index,
            pad_type,
            capabilities,
        );
    }

    fn on_pad_disconnected(&mut self, client: u32, pad_index: u8) {
        self.lock()
            .expect("not poisoned")
            .on_pad_disconnected(client, pad_index);
    }

    fn on_audio_in(&mut self, client: u32, pad_index: u8, seq: Seq16, payload: &[u8]) {
        self.lock()
            .expect("not poisoned")
            .on_audio_in(client, pad_index, seq, payload);
    }

    fn on_bye(&mut self, client: u32) {
        self.lock().expect("not poisoned").on_bye(client);
    }

    fn drain_outbound(&mut self) -> Vec<Outbound> {
        self.lock().expect("not poisoned").drain_outbound()
    }
}

impl ControlHandler for Recording {
    fn pairing_armed(&self, _now: u64) -> bool {
        self.armed
    }

    fn on_pair_request(
        &mut self,
        request: PairRequest,
        _now: u64,
    ) -> Option<(u32, [u8; NONCE_LEN])> {
        if !self.armed {
            return None;
        }
        self.pair_requests.push(request);
        let id = self.next_request_id;
        self.next_request_id += 1;
        Some((id, self.server_nonce))
    }

    fn on_pair_confirm(&mut self, request_id: u32, tag: [u8; TAG_LEN], _now: u64) {
        self.pair_confirms.push((request_id, tag));
    }

    fn client_keys(&self) -> Vec<(u32, SessionKey)> {
        self.keys.clone()
    }

    fn client_secret(&self, client: u32) -> Option<[u8; 32]> {
        self.secrets
            .iter()
            .find(|(c, _)| *c == client)
            .map(|(_, s)| *s)
    }

    fn on_hello(&mut self, client: u32, from: SocketAddr, hello: Hello) -> Option<SessionConfig> {
        self.hellos.push((client, from, hello));
        self.session_config.clone()
    }

    fn on_app_list(&mut self) -> Vec<AppListing> {
        self.app_list_calls += 1;
        self.apps.clone()
    }

    fn on_launch(&mut self, app_id: u32) -> Result<(), String> {
        self.launches.push(app_id);
        match &self.launch_error {
            Some(e) => Err(e.clone()),
            None => Ok(()),
        }
    }

    fn on_art(&mut self, app_id: u32) -> Option<(ArtRef, Arc<[u8]>)> {
        self.art_requests.push(app_id);
        self.art
            .iter()
            .find(|(id, _)| *id == app_id)
            .and_then(|(_, bytes)| ArtRef::of(bytes).map(|r| (r, Arc::clone(bytes))))
    }

    fn on_input(&mut self, client: u32, seq: u32, event: InputEvent) {
        self.inputs.push((client, event));
        self.input_seqs.push(seq);
    }

    fn on_quirks(&mut self, client: u32, quirks: DecoderQuirks) {
        self.quirks.push((client, quirks));
    }

    fn on_request_idr(&mut self, client: u32) {
        self.idr_requests.push(client);
    }

    fn on_resize(&mut self, client: u32, width: u32, height: u32, refresh_mhz: u32) {
        self.resizes.push((client, width, height, refresh_mhz));
    }

    fn on_nack(&mut self, client: u32, frame_id: Seq16, body: &[u8]) {
        self.nacks.push((client, frame_id, body.to_vec()));
    }

    fn on_feedback(&mut self, client: u32, feedback: Feedback) {
        self.feedbacks.push((client, feedback));
    }

    fn on_pad_connected(&mut self, client: u32, pad_index: u8, pad_type: u8, capabilities: u16) {
        self.pad_connects
            .push((client, pad_index, pad_type, capabilities));
    }

    fn on_pad_disconnected(&mut self, client: u32, pad_index: u8) {
        self.pad_disconnects.push((client, pad_index));
    }

    fn on_audio_in(&mut self, client: u32, pad_index: u8, seq: Seq16, payload: &[u8]) {
        self.audio_ins
            .push((client, pad_index, seq, payload.to_vec()));
    }

    fn on_bye(&mut self, client: u32) {
        self.byes.push(client);
    }

    fn session_stop(&mut self, client: u32) {
        self.stops.push(client);
    }

    fn drain_outbound(&mut self) -> Vec<Outbound> {
        std::mem::take(&mut self.outbound)
    }
}
