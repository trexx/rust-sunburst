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

use sunburst_core::proto::pairing::{NONCE_LEN, TAG_LEN};
use sunburst_core::proto::{AppListing, Hello, InputEvent, PairRequest, Rumble, ServerControl, SessionKey};

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
}

/// Where authenticated input goes once it has been verified.
///
/// Separate from [`ControlHandler`] because it is the one seam that is genuinely
/// platform-bound: `sunburst-input` implements it against `SendInput` on
/// Windows. It lives here rather than in `sunburst-web` so that the crate doing
/// the injecting does not have to depend on `hyper` to reach the trait.
pub trait InputSink: Send {
    fn inject(&mut self, client: u32, event: InputEvent);
}

/// Drops input. What the server uses until an injector is attached.
pub struct NoInput;

impl InputSink for NoInput {
    fn inject(&mut self, _client: u32, _event: InputEvent) {}
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
    /// Re-read rather than cached by the endpoint, because a revoke has to take
    /// effect immediately: a client removed in the web UI must stop being able
    /// to send input, not stop at the next restart.
    fn client_keys(&self) -> Vec<(u32, SessionKey)>;

    fn on_hello(&mut self, client: u32, hello: Hello);
    fn on_app_list(&mut self) -> Vec<AppListing>;
    fn on_launch(&mut self, app_id: u32) -> Result<(), String>;

    /// Where `sunburst-input` attaches on Windows.
    fn on_input(&mut self, client: u32, event: InputEvent);

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
    pub apps: Vec<AppListing>,
    pub server_nonce: [u8; NONCE_LEN],

    pub pair_requests: Vec<PairRequest>,
    pub pair_confirms: Vec<(u32, [u8; TAG_LEN])>,
    pub hellos: Vec<(u32, Hello)>,
    pub inputs: Vec<(u32, InputEvent)>,
    pub launches: Vec<u32>,
    pub app_list_calls: usize,
    pub byes: Vec<u32>,
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

    fn on_hello(&mut self, client: u32, hello: Hello) {
        self.lock().expect("not poisoned").on_hello(client, hello);
    }

    fn on_app_list(&mut self) -> Vec<AppListing> {
        self.lock().expect("not poisoned").on_app_list()
    }

    fn on_launch(&mut self, app_id: u32) -> Result<(), String> {
        self.lock().expect("not poisoned").on_launch(app_id)
    }

    fn on_input(&mut self, client: u32, event: InputEvent) {
        self.lock().expect("not poisoned").on_input(client, event);
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

    fn on_hello(&mut self, client: u32, hello: Hello) {
        self.hellos.push((client, hello));
    }

    fn on_app_list(&mut self) -> Vec<AppListing> {
        self.app_list_calls += 1;
        self.apps.clone()
    }

    fn on_launch(&mut self, app_id: u32) -> Result<(), String> {
        self.launches.push(app_id);
        Ok(())
    }

    fn on_input(&mut self, client: u32, event: InputEvent) {
        self.inputs.push((client, event));
    }

    fn on_bye(&mut self, client: u32) {
        self.byes.push(client);
    }

    fn drain_outbound(&mut self) -> Vec<Outbound> {
        std::mem::take(&mut self.outbound)
    }
}
