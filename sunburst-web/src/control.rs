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
    SessionKey,
};
// Re-exported so callers of this crate do not need to reach past it for the
// seams its own handler is generic over.
use sunburst_net::{ControlHandler, Outbound};
pub use sunburst_net::{InputSink, NoInput, NoStream, StreamControl};

use crate::api::AppState;
use crate::client::QuirksRecord;

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
        self.stream.session_start(client, from, &hello, quirks)
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

    fn on_input(&mut self, client: u32, event: InputEvent) {
        self.state.touch_last_seen(client, unix_now());
        self.input.inject(client, event);
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
