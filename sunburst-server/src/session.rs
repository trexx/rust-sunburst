// SPDX-License-Identifier: GPL-2.0-or-later

//! The video-session manager: what turns a client `Hello` into a running stream.
//!
//! Implements [`StreamControl`], so it lives behind the endpoint's handler seam
//! and never touches the socket itself. On `Hello` it picks the codec, sizes the
//! rate controller, spawns a [`Pipeline`], and answers with the `SessionConfig`
//! the endpoint signs and derives the session key from. Afterwards it routes the
//! back-channel: NACKs to the pipeline (retransmit through the ring, or abandon
//! through the shared atomic), feedback into the rate controller, and it drains
//! the codec headers, cursor and secure-desktop messages the pipeline produces.
//!
//! One stream at a time — a two-device household, and the codec is negotiated
//! once per session. A second client's `Hello` is declined until the first ends.

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use sunburst_core::proto::{
    DecoderQuirks, Feedback, Hello, Nack, Seq16, ServerControl, SessionConfig, StreamCodec,
    negotiate_codec,
};
use sunburst_encode::encoder::Codec;
use sunburst_net::{Bounds, Outbound, RateController, StreamControl};
use sunburst_web::host::SessionSummary;
use windows::Win32::System::Performance::QueryPerformanceFrequency;

use crate::pipeline::{CodecHeaders, Pipeline, PipelineParams, StreamShared, retransmit_ring};

/// The configured stream defaults the manager applies to every session.
#[derive(Clone, Debug)]
pub struct StreamSettings {
    /// `None` = auto (AV1 when the client offers it, else HEVC).
    pub codec: Option<StreamCodec>,
    pub bitrate_kbps: u32,
    pub hdr: bool,
    /// Opt into the NvFBC CUDA-native backend (resilience, never the default).
    pub nvfbc: bool,
}

/// The live-session view the web UI reads and the disconnect it can request.
/// Shared between the manager (on the endpoint thread) and the `Host` (on the
/// web thread); a mutex is fine here — this is the control plane.
#[derive(Default)]
pub struct Sessions {
    inner: Mutex<SessionsInner>,
}

#[derive(Default)]
struct SessionsInner {
    active: Option<SessionSummary>,
    disconnect: Option<u32>,
}

impl Sessions {
    pub fn new() -> Arc<Sessions> {
        Arc::new(Sessions::default())
    }

    /// The active session, for `GET /api/sessions`.
    pub fn list(&self) -> Vec<SessionSummary> {
        self.inner
            .lock()
            .expect("not poisoned")
            .active
            .iter()
            .cloned()
            .collect()
    }

    /// Ask the manager to drop `id`. Returns whether it is the active session.
    pub fn request_disconnect(&self, id: u32) -> bool {
        let mut inner = self.inner.lock().expect("not poisoned");
        if inner.active.as_ref().is_some_and(|s| s.id == id) {
            inner.disconnect = Some(id);
            true
        } else {
            false
        }
    }

    fn set_active(&self, summary: Option<SessionSummary>) {
        self.inner.lock().expect("not poisoned").active = summary;
    }

    fn take_disconnect(&self) -> Option<u32> {
        self.inner.lock().expect("not poisoned").disconnect.take()
    }
}

struct Active {
    client: u32,
    session_id: u32,
    codec: StreamCodec,
    pipeline: Pipeline,
    shared: Arc<StreamShared>,
    rate: RateController,
    headers_rx: mpsc::Receiver<CodecHeaders>,
    retransmit: sunburst_net::Producer,
    headers_sent: bool,
    secure_sent: bool,
}

pub struct SessionManager {
    socket: UdpSocket,
    sessions: Arc<Sessions>,
    settings: StreamSettings,
    active: Option<Active>,
    next_session_id: u32,
}

impl SessionManager {
    /// `socket` is a clone of the endpoint's UDP socket, so video leaves the one
    /// shared port. `sessions` is shared with the `Host` for the UI.
    pub fn new(
        socket: UdpSocket,
        sessions: Arc<Sessions>,
        settings: StreamSettings,
    ) -> SessionManager {
        SessionManager {
            socket,
            sessions,
            settings,
            active: None,
            next_session_id: 1,
        }
    }

    fn stop_active(&mut self) {
        if let Some(active) = self.active.take() {
            active.pipeline.stop();
            self.sessions.set_active(None);
        }
    }
}

/// HEVC 150 Mbps / AV1 100 Mbps — the decoder-agnostic ceilings from CLAUDE.md.
fn codec_ceiling_kbps(codec: StreamCodec) -> u32 {
    match codec {
        StreamCodec::Hevc => 150_000,
        StreamCodec::Av1 => 100_000,
    }
}

fn qpc_freq_hz() -> u64 {
    let mut freq = 0i64;
    // SAFETY: QueryPerformanceFrequency writes the counter frequency and always
    // succeeds on the supported OS floor.
    unsafe {
        let _ = QueryPerformanceFrequency(&mut freq);
    }
    freq.max(1) as u64
}

fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

impl StreamControl for SessionManager {
    fn session_start(
        &mut self,
        client: u32,
        from: SocketAddr,
        hello: &Hello,
        quirks: DecoderQuirks,
    ) -> Option<SessionConfig> {
        // One stream at a time.
        if self.active.is_some() {
            return None;
        }
        let codec = negotiate_codec(self.settings.codec, hello.codecs)?;
        let enc_codec = match codec {
            StreamCodec::Hevc => Codec::Hevc,
            StreamCodec::Av1 => Codec::Av1,
        };

        // Bitrate: the configured target, bounded by the codec ceiling and the
        // decoder's own hint.
        let ceiling = codec_ceiling_kbps(codec).min(quirks.max_bitrate_hint / 1000);
        let bitrate_kbps = self.settings.bitrate_kbps.min(ceiling).max(10_000);
        let ref_invalidation = quirks.ref_invalidation;
        let intra_refresh = quirks.intra_refresh.then_some((240, 30));
        let slices = match enc_codec {
            Codec::Hevc => 4,
            Codec::Av1 => 2,
        };
        let fps = (hello.refresh_mhz / 1000).max(1);

        let socket = self.socket.try_clone().ok()?;
        let shared = StreamShared::new(bitrate_kbps);
        let (headers_tx, headers_rx) = mpsc::channel();
        let (retransmit_tx, retransmit_rx) = retransmit_ring();

        let params = PipelineParams {
            codec: enc_codec,
            width: hello.width,
            height: hello.height,
            fps,
            bitrate_kbps,
            slices,
            hdr: self.settings.hdr,
            intra_refresh,
            dpb_depth: 8,
            ref_invalidation,
            nvfbc: self.settings.nvfbc,
        };
        let pipeline = Pipeline::spawn(
            socket,
            from,
            params,
            Arc::clone(&shared),
            headers_tx,
            retransmit_rx,
        )
        .ok()?;

        let session_id = self.next_session_id;
        self.next_session_id += 1;

        let mut server_nonce = [0u8; 16];
        if sunburst_web::random::fill(&mut server_nonce).is_err() {
            pipeline.stop();
            return None;
        }

        let bounds = Bounds {
            min_kbps: 10_000,
            max_kbps: bitrate_kbps,
            initial_kbps: bitrate_kbps,
        };

        self.sessions.set_active(Some(SessionSummary {
            id: session_id,
            client_id: client,
            client_name: hello.name.clone(),
            codec: format!("{codec:?}"),
            width: hello.width,
            height: hello.height,
            fps,
            bitrate_kbps,
            started_at: now_ms() / 1000,
            app_id: None,
        }));

        self.active = Some(Active {
            client,
            session_id,
            codec,
            pipeline,
            shared,
            rate: RateController::new(bounds),
            headers_rx,
            retransmit: retransmit_tx,
            headers_sent: false,
            secure_sent: false,
        });

        // HDR mastering rides in the bitstream (the encoder's output flags), so
        // it is not duplicated here; the client reads it from the stream. The
        // clock facts let the client attribute one-way delay.
        Some(SessionConfig {
            session_id,
            codec,
            width: hello.width as u16,
            height: hello.height as u16,
            fps_mhz: hello.refresh_mhz,
            bitrate_kbps,
            hdr: None,
            intra_refresh: intra_refresh.is_some(),
            ref_invalidation,
            slices: slices as u8,
            server_nonce,
            qpc_freq_hz: qpc_freq_hz(),
            server_ns: now_ns(),
            hello_delay_ns: 0,
        })
    }

    fn session_stop(&mut self, client: u32) {
        if self.active.as_ref().is_some_and(|a| a.client == client) {
            self.stop_active();
        }
    }

    fn on_request_idr(&mut self, client: u32) {
        if let Some(a) = &self.active
            && a.client == client
        {
            a.shared.request_idr.store(true, Ordering::Relaxed);
        }
    }

    fn on_nack(&mut self, client: u32, frame_id: Seq16, body: &[u8]) {
        let Some(a) = &self.active else { return };
        if a.client != client {
            return;
        }
        let Some(nack) = Nack::decode(body) else {
            return;
        };
        if nack.is_abandon() {
            a.shared.note_abandon(frame_id);
        } else {
            // Forward the request to the send thread: [frame_id][nack body].
            let mut req = Vec::with_capacity(2 + body.len());
            req.extend_from_slice(&frame_id.0.to_le_bytes());
            req.extend_from_slice(body);
            a.retransmit.push(&req);
        }
    }

    fn on_feedback(&mut self, client: u32, feedback: Feedback) {
        if let Some(a) = &mut self.active
            && a.client == client
            && let Some(new_kbps) = a.rate.on_feedback(&feedback, now_ms())
        {
            a.shared.target_kbps.store(new_kbps, Ordering::Relaxed);
        }
    }

    fn drain_outbound(&mut self) -> Vec<Outbound> {
        // A UI-requested disconnect ends the session first.
        if let Some(id) = self.sessions.take_disconnect()
            && self.active.as_ref().is_some_and(|a| a.session_id == id)
        {
            self.stop_active();
            return Vec::new();
        }

        let Some(a) = &mut self.active else {
            return Vec::new();
        };
        let mut out = Vec::new();

        // Codec headers, once the encoder has produced them (and again after a
        // rebuild, which sends a fresh set down the same channel).
        while let Ok(headers) = a.headers_rx.try_recv() {
            a.headers_sent = true;
            out.push(Outbound::Control {
                client: a.client,
                message: ServerControl::CodecPrivate {
                    codec: a.codec,
                    data: headers.sequence,
                },
            });
        }

        // Secure-desktop transitions.
        let secure = a.shared.secure.load(Ordering::Relaxed);
        if secure != a.secure_sent {
            a.secure_sent = secure;
            out.push(Outbound::Control {
                client: a.client,
                message: ServerControl::SecureDesktop { active: secure },
            });
        }

        out
    }
}
