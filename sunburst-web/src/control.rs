// SPDX-License-Identifier: GPL-2.0-or-later

//! The control channel wired to [`AppState`].
//!
//! Lives here rather than in `sunburst-server` for one practical reason: this
//! crate is cross-platform, so the whole pairing path — over a real socket,
//! against the real state and the real store — is exercisable on the development
//! machine. In `sunburst-server` it would be Windows-only and untestable here.
//!
//! Input is the exception. It is forwarded to a sink the server supplies,
//! because injecting it is the one genuinely platform-bound part.

use std::net::SocketAddr;
use std::sync::Arc;

use sunburst_core::proto::pairing::{NONCE_LEN, TAG_LEN};
use sunburst_core::proto::{
    AppListing, DecoderQuirks, Feedback, Hello, InputEvent, PairRequest, Seq16, SessionConfig,
    SessionKey, StreamCodec,
};
// Re-exported so callers of this crate do not need to reach past it for the
// seams its own handler is generic over.
use sunburst_net::{
    CaptureBackend as NetBackend, ControlHandler, InputSettings, Outbound, SessionSettings,
};
pub use sunburst_net::{InputSink, NoInput, NoStream, StreamControl};

use crate::api::AppState;
use crate::client::QuirksRecord;
use crate::config::{CaptureBackend, CodecPreference, RateControl};

/// Wires the control channel to [`AppState`] (`ControlHandler`), the input
/// injector (`InputSink`), and the video-session manager (`StreamControl`). The
/// last is generic so the Windows `SessionManager` and the test `NoStream` both
/// slot in without this crate depending on either.
pub struct WebHandler<S: InputSink, T: StreamControl> {
    state: Arc<AppState>,
    input: S,
    stream: T,
}

impl<S: InputSink, T: StreamControl> WebHandler<S, T> {
    pub fn new(state: Arc<AppState>, input: S, stream: T) -> WebHandler<S, T> {
        WebHandler {
            state,
            input,
            stream,
        }
    }
}

impl<S: InputSink, T: StreamControl> ControlHandler for WebHandler<S, T> {
    fn pairing_armed(&self, now: u64) -> bool {
        self.state.pairing_armed(now)
    }

    fn on_pair_request(
        &mut self,
        request: PairRequest,
        now: u64,
    ) -> Option<(u32, [u8; NONCE_LEN])> {
        // Core carries the quirks as the plain struct; the store keeps a serde
        // mirror so `serde` stays out of the frame path's crate.
        self.state.pair_request(
            crate::pairing::PairRequest {
                name: request.name,
                model: request.model,
                abi: request.abi,
                quirks: QuirksRecord::from(request.quirks),
                client_nonce: request.client_nonce,
            },
            now,
        )
    }

    fn on_pair_confirm(&mut self, request_id: u32, tag: [u8; TAG_LEN], now: u64) {
        self.state.pair_confirm(request_id, tag, now);
    }

    fn client_keys(&self) -> Vec<(u32, SessionKey)> {
        self.state.client_keys()
    }

    fn client_secret(&self, client: u32) -> Option<[u8; 32]> {
        self.state.client_secret(client)
    }

    fn on_hello(&mut self, client: u32, from: SocketAddr, hello: Hello) -> Option<SessionConfig> {
        self.state.touch_last_seen(client, unix_now());
        // Seed the session with the quirks recorded at pairing; a fresh
        // `DecoderQuirks` message can refine them afterwards.
        let quirks = self.state.client_quirks(client).unwrap_or_default();
        // Resolve the whole effective stream config for this session (global
        // defaults + the running app's overrides) so every knob is live.
        let e = self.state.effective_stream_config();
        let settings = SessionSettings {
            codec: match e.codec {
                CodecPreference::Auto => None,
                CodecPreference::Hevc => Some(StreamCodec::Hevc),
                CodecPreference::Av1 => Some(StreamCodec::Av1),
                CodecPreference::H264 => Some(StreamCodec::H264),
            },
            bitrate_kbps: e.bitrate_kbps,
            min_bitrate_kbps: e.min_bitrate_kbps,
            max_bitrate_kbps: e.max_bitrate_kbps,
            hdr: e.hdr,
            preset: e.preset,
            vbr: matches!(e.rate_control, RateControl::Vbr),
            slices: e.slices,
            idr_period: e.idr_period,
            dpb_depth: e.dpb_depth,
            capture_backend: match e.capture_backend {
                CaptureBackend::Auto => NetBackend::Auto,
                CaptureBackend::Wgc => NetBackend::Wgc,
                CaptureBackend::Dda => NetBackend::Dda,
                CaptureBackend::Nvfbc => NetBackend::Nvfbc,
            },
            capture_output: e.capture_output,
            match_resolution: e.match_resolution,
            virtual_display: e.virtual_display,
            fps_cap: e.fps_cap,
            audio: e.audio,
            audio_device: e.audio_device.clone(),
            mic_device: e.mic_device.clone(),
            audio_bitrate_kbps: e.audio_bitrate_kbps,
            audio_frame_us: e.audio_frame_us,
            audio_fec: e.audio_fec,
            audio_complexity: e.audio_complexity,
            disable_epp: self.state.input_config().disable_epp,
            mouse_sensitivity_milli: (self.state.input_config().mouse_sensitivity * 1000.0)
                .round()
                .max(0.0) as u32,
        };
        // Push the input tuning to the injector (mouse sensitivity / deadzone are
        // applied live; EPP is handled by the session guard via `settings`).
        let input = self.state.input_config();
        self.input.configure(InputSettings {
            mouse_sensitivity: input.mouse_sensitivity,
            gamepad_deadzone: input.gamepad_deadzone,
        });
        self.stream
            .session_start(client, from, &hello, quirks, settings)
    }

    fn on_quirks(&mut self, client: u32, quirks: DecoderQuirks) {
        self.state.touch_last_seen(client, unix_now());
        self.stream.on_quirks(client, quirks);
    }

    fn on_request_idr(&mut self, client: u32) {
        self.stream.on_request_idr(client);
    }

    fn on_resize(&mut self, client: u32, width: u32, height: u32, refresh_mhz: u32) {
        self.stream.on_resize(client, width, height, refresh_mhz);
    }

    fn on_nack(&mut self, client: u32, frame_id: Seq16, body: &[u8]) {
        self.stream.on_nack(client, frame_id, body);
    }

    fn on_feedback(&mut self, client: u32, feedback: Feedback) {
        self.stream.on_feedback(client, feedback);
    }

    fn session_stop(&mut self, client: u32) {
        self.stream.session_stop(client);
    }

    fn on_app_list(&mut self) -> Vec<AppListing> {
        self.state
            .app_list()
            .into_iter()
            .map(|a| AppListing {
                id: a.id,
                name: a.name,
            })
            .collect()
    }

    fn on_launch(&mut self, app_id: u32) -> Result<(), String> {
        self.state.launch(app_id)
    }

    fn on_input(&mut self, client: u32, seq: u32, event: InputEvent) {
        self.state.touch_last_seen(client, unix_now());
        self.input.inject(client, seq, event);
    }

    fn on_pad_connected(&mut self, client: u32, pad_index: u8, pad_type: u8, capabilities: u16) {
        self.state.touch_last_seen(client, unix_now());
        self.input
            .pad_connected(client, pad_index, pad_type, capabilities);
    }

    fn on_pad_disconnected(&mut self, client: u32, pad_index: u8) {
        self.state.touch_last_seen(client, unix_now());
        self.input.pad_disconnected(client, pad_index);
    }

    fn on_bye(&mut self, client: u32) {
        self.state.touch_last_seen(client, unix_now());
        self.stream.session_stop(client);
    }

    /// Everything the server originates since the last tick: the injector's
    /// rumble and pad output, plus the session manager's `CodecPrivate`, cursor
    /// updates and `SecureDesktop`.
    fn drain_outbound(&mut self) -> Vec<Outbound> {
        let mut out = self.input.drain_outbound();
        out.append(&mut self.stream.drain_outbound());
        out
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
