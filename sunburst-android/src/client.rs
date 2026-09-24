// SPDX-License-Identifier: GPL-2.0-or-later
#![cfg(target_os = "android")]

//! The client frame path: connect, negotiate, and run receive → decode →
//! present. Mirrors `tools/fakeclient/src/stream.rs`, but feeds the reassembled
//! frames to a real [`Decoder`] on the `SurfaceView` instead of a file.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use jni::JavaVM;
use jni::objects::{GlobalRef, JValue};
use ndk::native_window::NativeWindow;
use sunburst_core::instr::{self, Stage};
use sunburst_core::proto::{
    CursorChunk, Feedback, Hello, InputPacket, Seq16, ServerControl, StreamCodec,
    pairing::NONCE_LEN,
};
use sunburst_net::{
    Accept, ClientEndpoint, Inbound, JitterBuffer, OwdGradient, Reassembler, TickUnwrap,
};

use crate::decode::Decoder;
use crate::input_map::{ClientInput, InputAccumulator};
use crate::pad::{PadOutputRouter, PadSink};
use sunburst_core::proto::input::GamepadState;
use sunburst_core::proto::padoutput::PadOutput;
use sunburst_core::proto::rumble::Rumble;
use sunburst_gip_bridge::PadEvent;

/// Where decoded server→client pad output goes: the physical controller, via the
/// currently-attached GIP bridge. The bridge lives in a process global set by the
/// USB attach ([`crate::usb`]), read here each call so it works whatever order the
/// attach and the stream start happened in. A rumble with no pad attached is
/// dropped rather than queued.
struct BridgePadSink;

impl PadSink for BridgePadSink {
    fn rumble(&mut self, r: Rumble) {
        if let Some(bridge) = crate::usb::current_bridge() {
            bridge.rumble(r);
        }
    }
    fn pad_output(&mut self, o: PadOutput) {
        if let Some(bridge) = crate::usb::current_bridge() {
            bridge.pad_output(o);
        }
    }
}

/// CLOCK_MONOTONIC nanoseconds — the base `MediaCodec` release timestamps and
/// `System.nanoTime()` share.
// The `as i64` casts are redundant on arm64 (64-bit fields) but needed on
// armv7, where `timespec`'s fields are narrower; keep them for both ABIs.
#[allow(clippy::unnecessary_cast)]
fn mono_ns() -> i64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `ts` is a valid, writable out-param for clock_gettime.
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    ts.tv_sec as i64 * 1_000_000_000 + ts.tv_nsec as i64
}

/// Upcalls into the Kotlin activity from the client thread (attached to the JVM
/// per call — cursor events are infrequent). The activity draws the cursor
/// overlay, so the pointer never rides the video.
pub struct Callbacks {
    pub vm: JavaVM,
    pub activity: GlobalRef,
}

impl Callbacks {
    fn cursor_shape(&self, bgra: &[u8], w: i32, h: i32, hx: i32, hy: i32) {
        if let Ok(mut env) = self.vm.attach_current_thread()
            && let Ok(arr) = env.byte_array_from_slice(bgra)
        {
            let _ = env.call_method(
                &self.activity,
                "onCursorShape",
                "([BIIII)V",
                &[
                    JValue::Object(&arr),
                    JValue::Int(w),
                    JValue::Int(h),
                    JValue::Int(hx),
                    JValue::Int(hy),
                ],
            );
        }
    }

    fn cursor_position(&self, x: i32, y: i32, visible: bool) {
        if let Ok(mut env) = self.vm.attach_current_thread() {
            let _ = env.call_method(
                &self.activity,
                "onCursorPosition",
                "(IIZ)V",
                &[JValue::Int(x), JValue::Int(y), JValue::Bool(visible as u8)],
            );
        }
    }
}

/// Reassembles the chunked cursor bitmap; the reliable channel delivers chunks
/// in order, so this appends. A `width == 0` chunk means the cursor is hidden.
#[derive(Default)]
struct CursorReassembler {
    shape_id: u32,
    buf: Vec<u8>,
    got: usize,
    w: i32,
    h: i32,
    hx: i32,
    hy: i32,
}

impl CursorReassembler {
    /// Feed a chunk; returns `(bgra, w, h, hotspot_x, hotspot_y)` when a shape
    /// completes (a hidden cursor completes immediately with `w == 0`).
    fn push(&mut self, c: &CursorChunk) -> Option<(Vec<u8>, i32, i32, i32, i32)> {
        if c.shape_id != self.shape_id || c.offset == 0 {
            self.shape_id = c.shape_id;
            self.buf = vec![0u8; c.total_len as usize];
            self.got = 0;
            self.w = c.width as i32;
            self.h = c.height as i32;
            self.hx = c.hotspot_x as i32;
            self.hy = c.hotspot_y as i32;
        }
        if c.width == 0 {
            return Some((Vec::new(), 0, 0, 0, 0));
        }
        let end = (c.offset as usize + c.data.len()).min(self.buf.len());
        let start = (c.offset as usize).min(end);
        self.buf[start..end].copy_from_slice(&c.data[..end - start]);
        self.got += end - start;
        (self.got >= self.buf.len() && !self.buf.is_empty()).then(|| {
            (
                std::mem::take(&mut self.buf),
                self.w,
                self.h,
                self.hx,
                self.hy,
            )
        })
    }
}

/// Client-side stream preferences, set on the TV's settings screen and threaded
/// down from `nativeStart`. All are requests the server is free to clamp:
/// `prefer_codec` and `max_bitrate_kbps` ride in the `Hello`, and `jitter_min_ms`
/// is a purely local floor on the adaptive jitter buffer.
#[derive(Clone, Copy)]
pub struct StreamPrefs {
    /// Codec to ask the server for, or `None` to leave the choice to the server.
    pub prefer_codec: Option<StreamCodec>,
    /// Client-side bitrate ceiling in kbps; `0` means no client-imposed cap.
    pub max_bitrate_kbps: u32,
    /// Jitter-buffer minimum depth in milliseconds (smoothness vs. latency).
    pub jitter_min_ms: u32,
    /// Where decoded audio plays: `0` TV only, `1` pad headset only, `2` both.
    pub audio_route: u8,
    /// Pad headset volume, `0..=100`.
    pub pad_volume: u8,
}

/// Run the client until `stop` is set. `codecs` is the bitmask the device can
/// decode (`sunburst_core::proto::codecs`); the server negotiates one of them.
#[allow(clippy::too_many_arguments)]
pub fn run(
    server: SocketAddr,
    secret: [u8; 32],
    codecs: u8,
    prefs: StreamPrefs,
    window: NativeWindow,
    stop: Arc<AtomicBool>,
    input_rx: Receiver<ClientInput>,
    client_tid: Arc<AtomicI32>,
    callbacks: Callbacks,
) {
    // Publish our tid so the Java PerformanceHintManager can target this thread.
    // SAFETY: gettid takes no arguments and cannot fail.
    let tid = unsafe { libc::gettid() };
    client_tid.store(tid, Ordering::Relaxed);
    if let Err(e) = run_inner(
        server, secret, codecs, prefs, &window, &stop, &input_rx, &callbacks,
    ) {
        log::error!("client stopped: {e}");
    }
}

#[allow(clippy::too_many_arguments)]
fn run_inner(
    server: SocketAddr,
    secret: [u8; 32],
    codecs: u8,
    prefs: StreamPrefs,
    window: &NativeWindow,
    stop: &AtomicBool,
    input_rx: &Receiver<ClientInput>,
    callbacks: &Callbacks,
) -> Result<(), String> {
    let mut client = ClientEndpoint::connect_paired(server, secret).map_err(|e| e.to_string())?;

    let mut client_nonce = [0u8; NONCE_LEN];
    getrandom::fill(&mut client_nonce).map_err(|e| e.to_string())?;
    client
        .send_hello(Hello {
            client_id: 1,
            name: "sunburst-android".into(),
            abi: std::env::consts::ARCH.into(),
            width: 3840,
            height: 2160,
            refresh_mhz: 60_000,
            client_nonce,
            clock_offset_ns: 0,
            codecs,
            // The client's requests from the TV settings screen; the server
            // honours `prefer_codec` when the device can decode it and clamps
            // `max_bitrate_kbps` to its own ceiling.
            prefer_codec: prefs.prefer_codec,
            max_bitrate_kbps: prefs.max_bitrate_kbps,
        })
        .map_err(|e| e.to_string())?;

    // Await the negotiation: SessionConfig then CodecPrivate.
    let mut config = None;
    let mut csd: Option<(StreamCodec, Vec<u8>)> = None;
    let deadline = Instant::now() + Duration::from_secs(5);
    while (config.is_none() || csd.is_none()) && !stop.load(Ordering::Relaxed) {
        if Instant::now() >= deadline {
            return Err("negotiation timed out; is the server streaming?".into());
        }
        match client.recv().map_err(|e| e.to_string())? {
            Some(Inbound::Control(ServerControl::SessionConfig(c))) => {
                log::info!(
                    "session {:?} {}x{} @ {} kbps",
                    c.codec,
                    c.width,
                    c.height,
                    c.bitrate_kbps
                );
                config = Some(c);
            }
            Some(Inbound::Control(ServerControl::CodecPrivate { codec, data })) => {
                csd = Some((codec, data));
            }
            Some(_) => {}
            None => client.tick().map_err(|e| e.to_string())?,
        }
    }
    let config = config.ok_or("no SessionConfig")?;
    let (_, csd0) = csd.ok_or("no CodecPrivate")?;
    let fps = (config.fps_mhz.max(1000) / 1000) as i32;

    let decoder = Decoder::new(
        config.codec,
        config.width as i32,
        config.height as i32,
        fps,
        &csd0,
        config.hdr,
        window,
    )?;

    // Audio routing: 0 TV only, 1 pad headset only, 2 both.
    let route_tv = prefs.audio_route != 1;
    let route_pad = prefs.audio_route != 0;

    // Start audio playback if the session carries it. Failure is non-fatal —
    // video plays on without sound.
    let mut audio_player = config.audio.and_then(|a| {
        match crate::audio::AudioPlayer::new(a.sample_rate, a.channels, a.frame_samples, route_tv) {
            Ok(p) => Some(p),
            Err(e) => {
                log::warn!("audio playback unavailable: {e}");
                None
            }
        }
    });
    // The pads' headsets, both directions behind one shared ≤2 cap: server audio
    // forked to the headphones (when the route includes the pad) and the headset
    // mic captured and sent. `capture` enables a headset itself, so the mic works
    // on every audio route — including TV-only.
    let mut headsets = crate::audio::PadHeadsets::new(prefs.pad_volume);

    let mut reassembler = Reassembler::new();
    let mut jitter = JitterBuffer::new();
    let frame_interval_ns = 1_000_000_000u64 / fps.max(1) as u64;
    jitter.set_frame_interval_ns(frame_interval_ns);
    // The TV setting is a floor in ms; default 2 ms reproduces the prior fixed
    // value. The buffer still adapts upward from here under loss/jitter.
    jitter.set_min_depth_ns(prefs.jitter_min_ms as u64 * 1_000_000);
    let mut owd = OwdGradient::new(100_000_000);
    let mut ticks = TickUnwrap::new(config.qpc_freq_hz);

    let mut out = vec![0u8; 8 * 1024 * 1024];
    let mut targets = [0u16; 128];
    let mut abandoned = [Seq16(0); 16];
    let mut pts_us = 0u64;
    let frame_interval_us = 1_000_000u64 / fps.max(1) as u64;
    let mut newest_seen: Option<Seq16> = None;
    let mut received = 0u32;
    let mut dropped = 0u32;
    let mut last_feedback = Instant::now();
    let mut input_acc = InputAccumulator::new();
    let mut input_seq: u32 = 1;
    let mut cursor = CursorReassembler::default();
    // Server→client rumble/pad-output, deduplicated and timed out before it
    // reaches the pad. The sink is a stub until the GIP bridge lands (Stage B).
    let mut pad_out = PadOutputRouter::new(BridgePadSink);

    while !stop.load(Ordering::Relaxed) {
        match client.recv().map_err(|e| e.to_string())? {
            Some(Inbound::Video(pkt)) => {
                let fid = sunburst_core::proto::Header::decode(&pkt).map(|h| h.frame_id);
                if let Some(fid) = fid {
                    newest_seen = Some(match newest_seen {
                        Some(n) if n.is_newer_than(fid) => n,
                        _ => fid,
                    });
                }
                match reassembler.push(&pkt) {
                    Accept::Complete(frame) => {
                        received += 1;
                        instr::record(Stage::Recv, frame.frame_id.0 as u32);
                        owd.push(ticks.to_ns(frame.qpc_timestamp), mono_ns());
                        if let Err(f) = jitter.push(frame, mono_ns() as u64) {
                            reassembler.release(f);
                        }
                    }
                    Accept::Buffered => {
                        if let Some(fid) = fid {
                            let newer = newest_seen.is_some_and(|n| n.is_newer_than(fid));
                            if newer || reassembler.expected_count(fid).is_some() {
                                let n = reassembler.nack_targets(fid, newer, &mut targets);
                                if n > 0 {
                                    let _ = client.send_nack(fid, &targets[..n]);
                                }
                            }
                        }
                    }
                    Accept::Ignored => {}
                }
            }
            Some(Inbound::Control(ServerControl::CursorShape(c))) => {
                if let Some((bgra, w, h, hx, hy)) = cursor.push(&c) {
                    callbacks.cursor_shape(&bgra, w, h, hx, hy);
                }
            }
            Some(Inbound::Control(ServerControl::CursorPosition { x, y, visible })) => {
                callbacks.cursor_position(x as i32, y as i32, visible);
            }
            Some(Inbound::Audio(pkt)) => {
                if let Some(player) = audio_player.as_mut()
                    && let Some((header, payload)) = sunburst_net::parse_audio_packet(&pkt)
                {
                    let id = header.frame_id.0 as u32;
                    instr::record(Stage::AudioRecv, id);
                    if let Some(pcm) = player.feed(payload, id)
                        && route_pad
                    {
                        headsets.play(pcm);
                    }
                }
            }
            Some(Inbound::Rumble(r)) => pad_out.on_rumble(r, (mono_ns() / 1_000_000) as u32),
            Some(Inbound::PadOutput(o)) => pad_out.on_pad_output(o),
            Some(_) => {}
            None => client.tick().map_err(|e| e.to_string())?,
        }

        // Release due frames into the decoder.
        while let Some(rel) = jitter.pop(mono_ns() as u64) {
            instr::record(Stage::JitterOut, rel.frame.frame_id.0 as u32);
            if let Some(from) = rel.stepped_over {
                dropped += 1;
                let _ = client.send_nack(from, &[]);
                let mut id = from;
                while id != rel.frame.frame_id {
                    reassembler.discard(id);
                    id = id.next();
                }
            }
            let fid = rel.frame.frame_id.0 as u32;
            if let Some(n) = reassembler.copy_into(&rel.frame, &mut out) {
                match decoder.feed(&out[..n], pts_us, fid) {
                    Ok(true) => pts_us += frame_interval_us,
                    Ok(false) => log::warn!("decoder input full; dropped frame {fid}"),
                    Err(e) => log::error!("feed: {e}"),
                }
            }
            reassembler.release(rel.frame);
        }

        // Present whatever the decoder has finished, at the next vsync.
        if let Err(e) = decoder.drain(mono_ns(), 0) {
            log::error!("drain: {e}");
        }

        let a = reassembler.drain_abandoned(&mut abandoned);
        for id in &abandoned[..a] {
            let _ = client.send_nack(*id, &[]);
        }

        // Drain queued input and send it, mapped to wire events. The session
        // key is installed, so the sequence starts at 1 each session.
        while let Ok(raw) = input_rx.try_recv() {
            if let Some(event) = input_acc.apply(raw)
                && client.send_input(&InputPacket { input_seq, event }).is_ok()
            {
                input_seq = input_seq.wrapping_add(1);
            }
        }

        // Drain any Xbox pads on the GIP bridge into the same input stream, so the
        // server creates and drives their emulated pads exactly as it does for a
        // TV-native controller. `GamepadState` carries battery along for free.
        if let Some(bridge) = crate::usb::current_bridge() {
            while let Some(ev) = bridge.poll() {
                let (index, state) = match ev {
                    PadEvent::Input(state) => (state.pad_index, state),
                    // A neutral state so the server releases held buttons/sticks.
                    PadEvent::Disconnected { index } => (
                        index,
                        GamepadState {
                            pad_index: index,
                            ..Default::default()
                        },
                    ),
                    PadEvent::Connected { index } => {
                        log::info!("gip: pad {index} connected");
                        continue;
                    }
                };
                if let Some(event) = input_acc.apply(ClientInput::Pad { index, state })
                    && client.send_input(&InputPacket { input_seq, event }).is_ok()
                {
                    input_seq = input_seq.wrapping_add(1);
                }
            }
        }

        // Drain any headset microphones and send them to the server. The mic
        // timestamp is the client monotonic clock (the server renders mic audio
        // immediately and does not use it for sync yet).
        headsets.capture(|pad, seq, opus| {
            let _ = client.send_audio_in(pad, seq, mono_ns() as u32, opus);
        });

        // Stop any motor that has gone unheard past the timeout (a lost final
        // zero-level packet), on the same 100 ms cadence as feedback.
        if last_feedback.elapsed() >= Duration::from_millis(100) {
            pad_out.tick((mono_ns() / 1_000_000) as u32);
            last_feedback = Instant::now();
            let _ = client.send_feedback(&Feedback {
                recv_timestamp: mono_ns() as u32,
                frames_received: received,
                frames_dropped: dropped,
                jitter_buffer_ms: (jitter.target_depth_ns() / 1_000_000) as u16,
                decode_p99_us: 0,
                owd_gradient: owd.slope_us_per_s(),
            });
        }
        client.tick().map_err(|e| e.to_string())?;
    }

    // Tell the server now, so the stream stops at once and a reconnect is not
    // left waiting on the old one's timeout.
    client.bye();
    decoder.stop();
    Ok(())
}
