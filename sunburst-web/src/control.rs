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

use std::sync::Arc;

use sunburst_core::proto::pairing::{NONCE_LEN, TAG_LEN};
use sunburst_core::proto::{AppListing, Hello, InputEvent, PairRequest, SessionKey};
// Re-exported so callers of this crate do not need to reach past it for the
// seam its own handler is generic over.
use sunburst_net::ControlHandler;
pub use sunburst_net::{InputSink, NoInput};

use crate::api::AppState;
use crate::client::QuirksRecord;

pub struct WebHandler<S: InputSink> {
    state: Arc<AppState>,
    input: S,
}

impl<S: InputSink> WebHandler<S> {
    pub fn new(state: Arc<AppState>, input: S) -> WebHandler<S> {
        WebHandler { state, input }
    }
}

impl<S: InputSink> ControlHandler for WebHandler<S> {
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

    fn on_hello(&mut self, client: u32, _hello: Hello) {
        self.state.touch_last_seen(client, unix_now());
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

    fn on_bye(&mut self, client: u32) {
        self.state.touch_last_seen(client, unix_now());
    }
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
