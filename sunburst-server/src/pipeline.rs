// SPDX-License-Identifier: GPL-2.0-or-later

//! The frame path: capture → convert → encode → packetize → send.
//!
//! Two dedicated OS threads, both MMCSS "Games" / `TIME_CRITICAL` (see
//! [`crate::realtime`]), joined by a lock-free SPSC packet ring
//! ([`sunburst_net::packet_ring`]):
//!
//! - **GPU thread** — the serial half. D3D11's immediate context is
//!   single-threaded and capture, the colour-convert compute shader and the
//!   NVENC submit all run on the one device, so they share one thread. It grabs
//!   a frame, converts scRGB→P010, submits to NVENC, and drains the encoder
//!   slice-by-slice; each slice/tile is packetized and pushed to the ring the
//!   instant it exists (subframe readback — CLAUDE.md: mandatory).
//! - **Send thread** — drains the ring and puts packets on the wire, overlapping
//!   transmit with the next slice's encode.
//!
//! Every stage records `(stage_id, frame_id, qpc)` into the instrumentation ring
//! ([`sunburst_core::instr::record`]); no locks, no logging, no allocation on the
//! per-frame path. The GPU thread builds the converter/encoder lazily on the
//! first frame (the device and resolution come from the frame), and re-emits a
//! keyframe + fresh sequence headers whenever capture is rebuilt after
//! [`CaptureError::AccessLost`](sunburst_capture::CaptureError::AccessLost).
//!
//! # Scope
//!
//! This wires the **D3D11** backends (DDA/WGC → [`Frame::Texture`]). The NvFBC
//! CUDA-native path ([`Frame::Cuda`]) encodes through a CUDA kernel and an
//! NVENC-CUDA session — a separate spine that needs the CUDA context plumbed off
//! the concrete backend — and is a follow-up; a `Cuda` frame here ends the loop
//! with a clear error rather than being silently mishandled.

use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::{JoinHandle, Thread};
use std::time::Duration;

use sunburst_capture::{CaptureError, Frame, select};
use sunburst_core::instr::{self, Stage};
use sunburst_core::proto::{Header, Seq16};
use sunburst_encode::convert::Converter;
use sunburst_encode::encoder::{Codec, Encoder, EncoderConfig, PicRequest};
use sunburst_encode::nvenc::Nvenc;
use sunburst_net::{Consumer, Packetizer, Producer, packet_ring};
use windows::Win32::Graphics::Direct3D11::ID3D11Device;
use windows::core::Interface;

use crate::realtime::RealtimeThread;

/// Packets the GPU→send ring holds. Sized to absorb a keyframe burst (a 4K IDR
/// is on the order of a thousand packets) without the encoder ever stalling on a
/// briefly-behind sender; a full ring drops, and the drop is recovered by the
/// next keyframe, not by back-pressuring the frame path.
const RING_CAPACITY: usize = 4096;

/// How the pipeline should encode.
#[derive(Clone, Copy, Debug)]
pub struct PipelineConfig {
    pub codec: Codec,
    /// Prefer an HDR (scRGB FP16 / BT.2020 PQ) capture and carry mastering
    /// metadata into the encoder.
    pub hdr: bool,
    /// HEVC slices, or AV1 tiles **per axis** (`2` = a 2×2 grid). `>1` turns on
    /// subframe readback.
    pub slices: u32,
}

impl PipelineConfig {
    /// The default subdivision for `codec`: 4 HEVC slices, or a 2×2 AV1 tile grid
    /// (CLAUDE.md: "Start at 2×2 for 4K").
    pub fn new(codec: Codec, hdr: bool) -> PipelineConfig {
        let slices = match codec {
            Codec::Hevc => 4,
            Codec::Av1 => 2,
        };
        PipelineConfig { codec, hdr, slices }
    }
}

/// The codec's out-of-band configuration for the client — HEVC VPS/SPS/PPS or the
/// AV1 `av1C` record (`csd-0`). Emitted once when the encoder is built; the
/// client needs it before any frame, so it travels the reliable control channel.
#[derive(Clone, Debug)]
pub struct CodecHeaders {
    pub codec: Codec,
    pub sequence: Vec<u8>,
}

/// A running stream. Dropping it (or calling [`stop`](Self::stop)) tears the
/// threads down and joins them.
pub struct Pipeline {
    stop: Arc<AtomicBool>,
    gpu: Option<JoinHandle<()>>,
    send: Option<JoinHandle<()>>,
}

impl Pipeline {
    /// Start streaming to `client` over `socket` (a clone of the endpoint's UDP
    /// socket, so video leaves the one shared port). `headers` receives the
    /// codec's sequence headers once, when the encoder is built.
    pub fn spawn(
        socket: UdpSocket,
        client: SocketAddr,
        cfg: PipelineConfig,
        headers: mpsc::Sender<CodecHeaders>,
    ) -> io::Result<Pipeline> {
        let stop = Arc::new(AtomicBool::new(false));
        let (producer, consumer) = packet_ring(RING_CAPACITY);

        let send_stop = Arc::clone(&stop);
        let send = std::thread::Builder::new()
            .name("sunburst-send".into())
            .spawn(move || send_loop(&send_stop, &consumer, &socket, client))?;
        let send_thread = send.thread().clone();

        let gpu_stop = Arc::clone(&stop);
        let gpu = std::thread::Builder::new()
            .name("sunburst-gpu".into())
            .spawn(move || {
                if let Err(e) = gpu_loop(&gpu_stop, producer, cfg, &headers, &send_thread) {
                    // Setup or fatal encode failure — off the per-frame path, so a
                    // one-time diagnostic is fine. AccessLost never lands here.
                    eprintln!("sunburst pipeline stopped: {e}");
                }
            })?;

        Ok(Pipeline {
            stop,
            gpu: Some(gpu),
            send: Some(send),
        })
    }

    /// Signal both threads and join them.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Wake the send thread if it is parked, so it observes the stop promptly.
        if let Some(s) = &self.send {
            s.thread().unpark();
        }
        if let Some(g) = self.gpu.take() {
            let _ = g.join();
        }
        if let Some(s) = self.send.take() {
            let _ = s.join();
        }
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// The serial GPU half. Returns `Err` only on setup or a fatal encode failure;
/// `AccessLost` is handled in-loop by rebuilding.
fn gpu_loop(
    stop: &AtomicBool,
    producer: Producer,
    cfg: PipelineConfig,
    headers: &mpsc::Sender<CodecHeaders>,
    send_thread: &Thread,
) -> Result<(), String> {
    let _rt = RealtimeThread::register();
    instr::register_thread("gpu");

    // NvFBC off: this pipeline wires the D3D11 backends (see the module docs).
    let mut capture = select::build(false, cfg.hdr).map_err(|e| e.to_string())?;
    let nvenc = Nvenc::load()?;
    let mut converter: Option<Converter> = None;
    let mut encoder: Option<Encoder> = None;
    let mut packetizer = Packetizer::new();
    let mut frame_id = Seq16(0);
    // The first frame after start or a rebuild must be a keyframe so the client
    // can begin decoding.
    let mut need_keyframe = true;

    while !stop.load(Ordering::Relaxed) {
        let frame = match capture.acquire(Duration::from_millis(100)) {
            Ok(Some(f)) => f,
            Ok(None) => continue, // idle desktop presented nothing
            Err(CaptureError::AccessLost) => {
                // Mode change / fullscreen transition / desktop switch: the device
                // may be gone, so drop everything bound to it and rebuild.
                capture = select::build(false, cfg.hdr).map_err(|e| e.to_string())?;
                converter = None;
                encoder = None;
                need_keyframe = true;
                continue;
            }
            Err(CaptureError::Unavailable) => {
                // Secure desktop / DRM. A placeholder frame is a later refinement;
                // for now wait it out and force a keyframe on return.
                std::thread::sleep(Duration::from_millis(50));
                need_keyframe = true;
                continue;
            }
            Err(CaptureError::Backend(m)) => return Err(m),
        };

        let Frame::Texture(tf) = frame else {
            return Err("pipeline is wired for the D3D11 backends; a CUDA (NvFBC) \
                        frame arrived — the CUDA-native path is a follow-up"
                .into());
        };

        let fid = frame_id;
        instr::record(Stage::CaptureAcquire, fid.0 as u32);
        let (w, h) = (tf.meta.width, tf.meta.height);
        let qpc = tf.meta.present_qpc as u32;

        // scRGB FP16 → P010, on the texture's own device.
        let conv = match &mut converter {
            Some(c) => c,
            None => converter.insert(Converter::new(&tf.texture)?),
        };
        let p010 = conv.convert(&tf.texture, w, h)?;
        let p010_raw = p010.as_raw();
        instr::record(Stage::ColorConvert, fid.0 as u32);

        // Build the encoder on the first frame; hand its sequence headers out.
        let enc = match &mut encoder {
            Some(e) => e,
            None => {
                // SAFETY: a captured texture always has a live device.
                let device: ID3D11Device =
                    unsafe { tf.texture.GetDevice() }.map_err(|e| e.to_string())?;
                let hdr_meta = capture.caps().hdr_metadata;
                let mut ecfg = EncoderConfig::new(cfg.codec, w, h);
                ecfg.slices = cfg.slices;
                ecfg.hdr = hdr_meta;
                let built = Encoder::new(&nvenc, device.as_raw(), &ecfg)?;
                let e = encoder.insert(built);
                let sequence = match cfg.codec {
                    Codec::Hevc => e.sequence_header()?,
                    Codec::Av1 => e.av1c()?,
                };
                // Control-plane, once per session; the channel decouples us from
                // however the endpoint delivers it.
                let _ = headers.send(CodecHeaders {
                    codec: cfg.codec,
                    sequence,
                });
                e
            }
        };

        instr::record(Stage::EncodeSubmit, fid.0 as u32);
        let keyframe = need_keyframe;
        need_keyframe = false;
        packetizer.begin_frame(fid, qpc, keyframe);
        // Each slice/tile: packetize and push to the send thread as it completes.
        let req = PicRequest {
            timestamp: fid.0 as u64,
            force_idr: keyframe,
        };
        enc.encode_slices(p010_raw, req, |unit| {
            instr::record(Stage::EncodeUnitOut, fid.0 as u32);
            packetizer.push_unit(unit, |pkt| {
                producer.push(pkt);
            });
            instr::record(Stage::Packetize, fid.0 as u32);
            send_thread.unpark();
        })?;
        packetizer.finish_frame(|pkt| {
            producer.push(pkt);
        });
        send_thread.unpark();

        frame_id = frame_id.next();
    }
    Ok(())
}

/// The network half. Drains the ring onto the wire, recording `Send` per packet,
/// and parks briefly when the ring is empty.
fn send_loop(stop: &AtomicBool, consumer: &Consumer, socket: &UdpSocket, client: SocketAddr) {
    let _rt = RealtimeThread::register();
    instr::register_thread("send");

    while !stop.load(Ordering::Relaxed) {
        let mut drained = false;
        while consumer.pop_with(|pkt| {
            let _ = socket.send_to(pkt, client);
            if let Some(h) = Header::decode(pkt) {
                instr::record(Stage::Send, h.frame_id.0 as u32);
            }
        }) {
            drained = true;
        }
        if !drained {
            // Woken by the GPU thread's unpark after each unit; the timeout is a
            // backstop so a missed wake cannot wedge the sender.
            std::thread::park_timeout(Duration::from_micros(250));
        }
    }

    // Flush whatever is queued so a clean stop still puts the last frame out.
    while consumer.pop_with(|pkt| {
        let _ = socket.send_to(pkt, client);
    }) {}
}
