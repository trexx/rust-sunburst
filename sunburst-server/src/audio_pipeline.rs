// SPDX-License-Identifier: GPL-2.0-or-later

//! The audio path: WASAPI loopback → Opus → 5 ms send, on its own thread.
//!
//! One dedicated MMCSS "Pro Audio" thread, spawned per session alongside the
//! video [`Pipeline`](crate::pipeline::Pipeline) and torn down with it. It
//! clones the same shared UDP socket the video path uses and sends to the same
//! client address; audio is unauthenticated (LAN, like video) and tiny
//! (~128 kbps), so it needs no NACK, no rate control, and no pacer — one
//! datagram per Opus frame.
//!
//! Cadence and silence: loopback delivers nothing while the endpoint is idle, so
//! each poll emits every captured frame, or a single silence frame if none — the
//! client always gets a steady stream to keep A/V sync anchored. Every frame is
//! stamped with the raw performance counter, re-read each poll, so audio shares
//! the video header's clock domain and cannot drift away from it over a long
//! stream.

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use sunburst_audio::LoopbackCapture;
use sunburst_audio::codec::{Application, OpusEncoder};
use sunburst_audio::pcm::FrameAccumulator;
use sunburst_core::instr::{self, Stage};
use sunburst_core::proto::{MAX_PAYLOAD, Seq16};
use sunburst_net::send::{PlainSender, Sender};
use sunburst_net::{MAX_AUDIO_PACKET, encode_audio_packet};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};

use crate::realtime::RealtimeThread;

/// The Opus stream shape mirrored into `SessionConfig.audio`. Rate and channels
/// are fixed; the frame size is derived from the configured frame duration.
pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u8 = 2;

/// Samples per channel for a frame of `frame_us` microseconds at 48 kHz.
pub fn frame_samples(frame_us: u32) -> u16 {
    ((SAMPLE_RATE as u64 * frame_us as u64) / 1_000_000) as u16
}

/// Per-session audio parameters.
pub struct AudioParams {
    /// Capture-endpoint name substring; `None`/empty selects the default endpoint.
    pub device: Option<String>,
    pub bitrate_kbps: u32,
    /// Opus frame duration in microseconds (2500/5000/10000/20000).
    pub frame_us: u32,
    /// Opus in-band FEC.
    pub fec: bool,
    /// Opus complexity 0..=10.
    pub complexity: u8,
}

/// A running audio pipeline: the stop flag its thread polls, and its join handle.
pub struct AudioPipeline {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl AudioPipeline {
    /// Spawn the audio thread. `socket` is a clone of the shared stream socket;
    /// `client` is where video is going too.
    pub fn spawn(socket: UdpSocket, client: SocketAddr, params: AudioParams) -> AudioPipeline {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("sunburst-audio".into())
            .spawn(move || audio_loop(socket, client, params, thread_stop))
            .ok();
        AudioPipeline { stop, thread }
    }

    /// Signal the thread and join it.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for AudioPipeline {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn qpc_now() -> u64 {
    let mut v = 0i64;
    // SAFETY: writes the current performance counter value.
    unsafe {
        let _ = QueryPerformanceCounter(&mut v);
    }
    v as u64
}

fn qpc_freq() -> u64 {
    let mut v = 0i64;
    // SAFETY: writes the performance-counter frequency.
    unsafe {
        let _ = QueryPerformanceFrequency(&mut v);
    }
    (v as u64).max(1)
}

fn make_encoder(
    bitrate_kbps: u32,
    fec: bool,
    complexity: u8,
) -> Result<OpusEncoder, sunburst_audio::codec::OpusError> {
    let mut enc = OpusEncoder::new(SAMPLE_RATE, CHANNELS, Application::LowDelay)?;
    enc.set_bitrate(bitrate_kbps.saturating_mul(1000))?;
    // FEC + an expected loss lets the client recover a single dropped packet
    // from the next one, which is why audio needs no NACK.
    enc.set_inband_fec(fec)?;
    enc.set_packet_loss_perc(if fec { 5 } else { 0 })?;
    // Audio is cheap; spend cycles for quality without touching the frame path.
    enc.set_complexity(complexity)?;
    Ok(enc)
}

fn audio_loop(socket: UdpSocket, client: SocketAddr, params: AudioParams, stop: Arc<AtomicBool>) {
    let _rt = RealtimeThread::register_audio();
    instr::register_thread("audio");

    // Capture unavailable (no endpoint, or WASAPI refused) is not fatal to the
    // session: video streams on without sound.
    let mut capture = match LoopbackCapture::open(params.device.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("audio: loopback capture unavailable, streaming without sound: {e}");
            return;
        }
    };
    let mut encoder = match make_encoder(params.bitrate_kbps, params.fec, params.complexity) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("audio: Opus encoder init failed: {e}");
            return;
        }
    };

    let samples = frame_samples(params.frame_us).max(1) as usize;
    let frame_len = samples * CHANNELS as usize;
    let mut acc = FrameAccumulator::new(samples, CHANNELS as usize);
    let mut sender = PlainSender::new(&socket);

    // Ticks between successive frames' timestamps: freq * frame_samples / rate.
    let ticks_per_frame = qpc_freq() * samples as u64 / SAMPLE_RATE as u64;
    let frame = &mut vec![0i16; frame_len][..];
    let silence = vec![0i16; frame_len];
    let mut opus = [0u8; MAX_PAYLOAD];
    let mut pkt = [0u8; MAX_AUDIO_PACKET];
    let mut seq = Seq16(0);

    // Poll at the frame interval; the WASAPI buffer (200 ms) absorbs the rest.
    let poll = Duration::from_micros(5_000);

    while !stop.load(Ordering::Relaxed) {
        if capture.drain_into(&mut acc).is_err() {
            // A transient WASAPI error: back off briefly and retry.
            std::thread::sleep(poll);
            continue;
        }

        // Re-anchor to real time each poll so audio timestamps track the video
        // clock; space frames drained together by one frame each.
        let base = qpc_now();
        let mut i = 0u64;
        while acc.pop_frame(frame) {
            let qpc = base.wrapping_add(i.wrapping_mul(ticks_per_frame));
            emit(
                &mut encoder,
                frame,
                qpc,
                &mut seq,
                &mut opus,
                &mut pkt,
                &mut sender,
                client,
            );
            i += 1;
        }

        if i == 0 {
            // Idle: hold the cadence with one silence frame.
            emit(
                &mut encoder,
                &silence,
                qpc_now(),
                &mut seq,
                &mut opus,
                &mut pkt,
                &mut sender,
                client,
            );
        }

        std::thread::sleep(poll);
    }
}

/// Encode one PCM frame and send it as one audio packet, recording the three
/// server-side audio stages against the frame's sequence number.
#[allow(clippy::too_many_arguments)]
fn emit(
    encoder: &mut OpusEncoder,
    pcm: &[i16],
    qpc: u64,
    seq: &mut Seq16,
    opus: &mut [u8],
    pkt: &mut [u8],
    sender: &mut PlainSender,
    client: SocketAddr,
) {
    let id = seq.0 as u32;
    instr::record(Stage::AudioCapture, id);

    // A dropped frame here (encode or send failure) is recovered by the client's
    // Opus PLC/FEC, so the path stays silent rather than logging per frame.
    if let Ok(n) = encoder.encode(pcm, opus) {
        instr::record(Stage::AudioEncode, id);
        if let Some(len) = encode_audio_packet(*seq, qpc as u32, &opus[..n], pkt) {
            let _ = sender.send_batch(&pkt[..len], len, client);
            instr::record(Stage::AudioSend, id);
        }
    }
    seq.0 = seq.0.wrapping_add(1);
}
