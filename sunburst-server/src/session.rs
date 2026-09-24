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

use sunburst_capture::OutputSelect;
use sunburst_core::proto::{
    AudioParams, CodecPrivate, DecoderQuirks, Feedback, Hello, Nack, Seq16, ServerControl,
    SessionConfig, StreamCodec, negotiate_codec,
};
use sunburst_encode::encoder::Codec;
use sunburst_net::{
    BitrateAsk, CaptureBackend as SessionBackend, Outbound, RateController, SessionSettings,
    StreamControl, client_interval_ns, encoder_fps, session_bitrate,
};
use sunburst_web::host::SessionSummary;
use windows::Win32::System::Performance::QueryPerformanceFrequency;

use crate::audio_pipeline::{self, AudioPipeline};
use crate::cursor::CursorPoller;
use crate::display::{self, DisplayGuard, EppGuard, VirtualDisplay};
use crate::mic_pipeline::MicPipeline;
use crate::pipeline::{CodecHeaders, Pipeline, PipelineParams, StreamShared, retransmit_ring};

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
    /// The audio pipeline, when the session streams sound. Stopped with the
    /// video pipeline in `stop_active`.
    audio: Option<AudioPipeline>,
    /// The pad-mic receive pipeline, when a virtual-mic endpoint is configured.
    /// Renders the pads' headset mic into a consumed virtual microphone; dropped
    /// (which joins its thread) when the session ends.
    mic: Option<MicPipeline>,
    /// Restores the display (HDR, and resolution if matched) when dropped, so a
    /// disconnect — or an abnormal teardown — puts the desktop back as found.
    _display: DisplayGuard,
    /// Holds the virtual display enabled for this session, disabling it on drop.
    /// `None` when not opted in or the VDD is absent (physical-display fallback).
    _vdd: Option<VirtualDisplay>,
    /// Holds the EPP override for the session, restoring it on drop; `None`
    /// unless `disable_epp` is set.
    _epp: Option<EppGuard>,
    shared: Arc<StreamShared>,
    rate: RateController,
    headers_rx: mpsc::Receiver<CodecHeaders>,
    retransmit: sunburst_net::Producer,
    headers_sent: bool,
    secure_sent: bool,
    cursor: CursorPoller,
    last_cursor_ms: u64,
}

pub struct SessionManager {
    socket: UdpSocket,
    sessions: Arc<Sessions>,
    active: Option<Active>,
    next_session_id: u32,
}

impl SessionManager {
    /// `socket` is a clone of the endpoint's UDP socket, so video leaves the one
    /// shared port. `sessions` is shared with the `Host` for the UI. All stream
    /// config now arrives per session via `session_start`'s [`SessionSettings`]
    /// (the handler resolves it live from the effective config).
    pub fn new(socket: UdpSocket, sessions: Arc<Sessions>) -> SessionManager {
        SessionManager {
            socket,
            sessions,
            active: None,
            next_session_id: 1,
        }
    }

    fn stop_active(&mut self) {
        if let Some(active) = self.active.take() {
            active.pipeline.stop();
            if let Some(audio) = active.audio {
                audio.stop();
            }
            self.sessions.set_active(None);
        }
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
        settings: SessionSettings,
    ) -> Option<SessionConfig> {
        // One stream at a time.
        if self.active.is_some() {
            return None;
        }
        // Codec: the client's request (from the TV settings) wins over the
        // server/per-app preference, but only among codecs it can decode.
        let prefer = hello.prefer_codec.or(settings.codec);
        let codec = negotiate_codec(prefer, hello.codecs)?;
        let enc_codec = match codec {
            StreamCodec::Hevc => Codec::Hevc,
            StreamCodec::Av1 => Codec::Av1,
            StreamCodec::H264 => Codec::H264,
        };

        // Bitrate: the configured target and the rate controller's range, both
        // bounded by the codec ceiling, the decoder's own hint and any
        // client-requested ceiling (the client can only lower it), with a floor.
        let bounds = session_bitrate(&BitrateAsk {
            codec,
            target_kbps: settings.bitrate_kbps,
            min_kbps: settings.min_bitrate_kbps,
            max_kbps: settings.max_bitrate_kbps,
            decoder_hint_bps: quirks.max_bitrate_hint,
            client_max_kbps: hello.max_bitrate_kbps,
        });
        let bitrate_kbps = bounds.initial_kbps;
        let ref_invalidation = quirks.ref_invalidation;
        let intra_refresh = quirks.intra_refresh.then_some((240, 30));
        // Subframe units per frame (HEVC/H.264 slices, AV1 tiles): the config
        // override, else four — 4 slices, or a 2×2 AV1 tile grid.
        let slices = if settings.slices > 0 {
            u32::from(settings.slices)
        } else {
            4
        };
        // Frame rate: the client's refresh, capped if the config asks. Whole
        // frames for the encoder (rounded, so a 59.94 Hz TV is 60), but the
        // capture governor keeps the exact interval.
        let fps = encoder_fps(hello.refresh_mhz, settings.fps_cap);
        let interval_ns = client_interval_ns(hello.refresh_mhz, settings.fps_cap);
        // H.264 is SDR (tonemapped): never enable HDR on the desktop for it.
        let want_hdr = settings.hdr && codec != StreamCodec::H264;

        // Set the display up BEFORE spawning the pipeline, so capture starts on the
        // final desktop (correct HDR state + resolution) instead of the old one.
        // Otherwise the pipeline captures the pre-toggle desktop, encodes a burst of
        // SDR frames with the wrong colour signalling, then takes an AccessLost when
        // the mode changes. Bring the virtual display up first (if opted in), so it
        // is an active output before HDR is toggled; absent, fall back to physical.
        let vdd = if settings.virtual_display {
            let enabled = VirtualDisplay::enable();
            if enabled.is_none() {
                eprintln!(
                    "display: virtual_display requested but no VDD is installed; \
                     streaming the physical display"
                );
            }
            enabled
        } else {
            None
        };
        // Resolution matching only touches the physical display; when the virtual
        // display is active it provides the resolution instead (Phase 7 commit 4).
        let resolution = (settings.match_resolution && !settings.virtual_display).then(|| {
            (
                hello.width,
                hello.height,
                display::refresh_hz(hello.refresh_mhz),
            )
        });
        let display_guard = DisplayGuard::apply(want_hdr, resolution);
        let epp_guard = settings.disable_epp.then(EppGuard::disable);

        let output = match settings.capture_output {
            Some(i) => OutputSelect::Index(i),
            None => OutputSelect::Primary,
        };
        // What the captured output reports now that the display is set up: the
        // mastering the handshake announces. It is the server's expectation, not
        // a promise. The desktop's real state decides the colour path per frame,
        // and DXGI can lag a toggle it has only just been told about, so a
        // client configures from what each encoder build reports, not from this.
        let hdr_mastering = if want_hdr {
            sunburst_capture::output::output_info(output)
                .ok()
                .filter(|o| o.hdr)
                .and_then(|o| o.hdr_metadata)
                .map(|m| m.mastering())
        } else {
            None
        };

        let socket = self.socket.try_clone().ok()?;
        let shared = StreamShared::new(bitrate_kbps);
        let (headers_tx, headers_rx) = mpsc::channel();
        let (retransmit_tx, retransmit_rx) = retransmit_ring();

        let params = PipelineParams {
            codec: enc_codec,
            width: hello.width,
            height: hello.height,
            fps,
            interval_ns,
            bitrate_kbps,
            slices,
            hdr: want_hdr,
            intra_refresh,
            dpb_depth: settings.dpb_depth as u32,
            ref_invalidation,
            nvfbc: matches!(settings.capture_backend, SessionBackend::Nvfbc),
            force_backend: match settings.capture_backend {
                SessionBackend::Wgc => Some(sunburst_capture::Backend::Wgc),
                SessionBackend::Dda => Some(sunburst_capture::Backend::Dda),
                SessionBackend::Auto | SessionBackend::Nvfbc => None,
            },
            output,
            preset: settings.preset,
            vbr: settings.vbr,
            idr_period: settings.idr_period,
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

        // Audio rides the same shared socket to the same client. Spawn failure
        // (only the socket clone can fail here) leaves the session video-only.
        let audio = if settings.audio {
            match self.socket.try_clone() {
                Ok(audio_socket) => Some(AudioPipeline::spawn(
                    audio_socket,
                    from,
                    audio_pipeline::AudioParams {
                        device: settings.audio_device.clone(),
                        bitrate_kbps: settings.audio_bitrate_kbps,
                        frame_us: settings.audio_frame_us,
                        fec: settings.audio_fec,
                        complexity: settings.audio_complexity,
                    },
                )),
                Err(e) => {
                    eprintln!(
                        "audio: could not clone the stream socket, streaming without sound: {e}"
                    );
                    None
                }
            }
        } else {
            None
        };
        let audio_on = audio.is_some();

        // The pad-mic path: open a render sink to the configured virtual mic when
        // one is named. Independent of the game-audio direction above — a headset
        // can carry the mic even when TV audio is routed elsewhere. The endpoint
        // routes `AudioIn` frames here through `on_audio_in`.
        let mic = settings
            .mic_device
            .as_deref()
            .filter(|d| !d.is_empty())
            .map(|d| MicPipeline::spawn(d.to_string()));

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
            audio,
            mic,
            _display: display_guard,
            _vdd: vdd,
            _epp: epp_guard,
            shared,
            rate: RateController::new(bounds),
            headers_rx,
            retransmit: retransmit_tx,
            headers_sent: false,
            secure_sent: false,
            cursor: CursorPoller::new(),
            last_cursor_ms: 0,
        });

        // The clock facts let the client attribute one-way delay.
        Some(SessionConfig {
            session_id,
            codec,
            width: hello.width as u16,
            height: hello.height as u16,
            fps_mhz: hello.refresh_mhz,
            bitrate_kbps,
            hdr: hdr_mastering,
            audio: audio_on.then_some(AudioParams {
                sample_rate: audio_pipeline::SAMPLE_RATE,
                channels: audio_pipeline::CHANNELS,
                frame_samples: audio_pipeline::frame_samples(settings.audio_frame_us),
            }),
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
            a.pipeline.wake_send();
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

    fn on_audio_in(&mut self, client: u32, pad_index: u8, _seq: Seq16, payload: &[u8]) {
        // Only the active session's client feeds the virtual mic; a frame from
        // anyone else (media is unauthenticated) is ignored. `None` mic means no
        // endpoint was configured, so the frame is dropped.
        if let Some(a) = &self.active
            && a.client == client
            && let Some(mic) = &a.mic
        {
            mic.push(pad_index, payload);
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
                message: ServerControl::CodecPrivate(CodecPrivate {
                    codec: a.codec,
                    data: headers.sequence,
                    width: headers.width as u16,
                    height: headers.height as u16,
                    first_frame: headers.first_frame,
                    color: headers.color,
                    hdr: headers.hdr,
                }),
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

        // Cursor: the shape (reliably) whenever it changes, the position
        // throttled to ~10/s. The client renders it, so it never rides the
        // video and never re-encodes.
        let update = a.cursor.poll();
        for chunk in update.shape {
            out.push(Outbound::Control {
                client: a.client,
                message: ServerControl::CursorShape(chunk),
            });
        }
        let now = now_ms();
        if now.saturating_sub(a.last_cursor_ms) >= 100 {
            a.last_cursor_ms = now;
            let (x, y, visible) = update.position;
            out.push(Outbound::Control {
                client: a.client,
                message: ServerControl::CursorPosition { x, y, visible },
            });
        }

        out
    }
}
