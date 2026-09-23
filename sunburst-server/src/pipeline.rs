// SPDX-License-Identifier: GPL-2.0-or-later

//! The frame path: capture → convert → encode → packetize → paced send, with
//! loss recovery and rate control driven from the control thread.
//!
//! Two dedicated OS threads, both MMCSS "Games" / `TIME_CRITICAL`
//! ([`crate::realtime`]), joined by a lock-free SPSC packet ring:
//!
//! - **GPU thread** — the serial half. D3D11's immediate context is
//!   single-threaded, so capture, the colour convert and the NVENC submit share
//!   one thread. It grabs the freshest frame within the client's frame interval
//!   (the governor), converts to P010, applies any pending recovery, submits to
//!   NVENC and drains it slice-by-slice; each slice is packetized and pushed the
//!   instant it exists (subframe readback — CLAUDE.md: mandatory).
//! - **Send thread** — services NACK retransmits from a cache first, then paces
//!   the main stream onto the wire, batching equal-size datagrams for USO.
//!
//! Both NvFBC (CUDA) and DDA/WGC (D3D11) are handled: the [`Spine`] is built
//! lazily from whichever surface the first frame carries, each taking its
//! shortest path to NVENC.
//!
//! The control thread never touches the socket or the encoder. It signals the
//! GPU thread through [`StreamShared`] (an IDR request, the oldest abandoned
//! frame, the target bitrate) and the send thread through the retransmit ring.

use std::ffi::c_void;
use std::io;
use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc;
use std::thread::{JoinHandle, Thread};
use std::time::{Duration, Instant};

use sunburst_capture::cuda::{CuContext, CuDevicePtr};
use sunburst_capture::{CaptureError, Frame, OutputSelect, TextureFormat, select};
use sunburst_core::instr::{self, Stage};
use sunburst_core::proto::{Header, Nack, Seq16};
use sunburst_encode::convert::{ConvertOutput, Converter};
use sunburst_encode::cuda_convert::CudaConverter;
use sunburst_encode::encoder::{Codec, ColorSpace, Encoder, EncoderConfig, PicRequest};
use sunburst_encode::nvenc::Nvenc;
use sunburst_net::send::Sender;
use sunburst_net::send::windows::WsaSender;
use sunburst_net::{
    Admit, Av1RefState, Batch, Consumer, FrameGovernor, H264RefState, HevcRefState, Pacer,
    Packetizer, Producer, Recovery, RefState, RetransmitCache, packet_ring, video_pace_bps,
};
use windows::Win32::Graphics::Direct3D11::ID3D11Device;
use windows::core::Interface;

/// Packets the GPU→send ring holds. Sized to absorb a keyframe burst (a 4K IDR
/// is on the order of a thousand packets) without the encoder stalling on a
/// briefly-behind sender; a full ring drops, recovered by NACK, not by
/// back-pressuring the frame path.
const RING_CAPACITY: usize = 4096;
/// Retransmit requests the control thread may queue for the send thread.
const RETRANSMIT_CAPACITY: usize = 256;
/// Sentinel for [`StreamShared::abandon`]: no frame is currently abandoned.
pub const NO_ABANDON: u32 = u32::MAX;

/// How the pipeline should encode a session.
#[derive(Clone, Debug)]
pub struct PipelineParams {
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    /// Whole frames per second, for the encoder's rate header and VBV sizing.
    pub fps: u32,
    /// The client's exact frame interval, for the capture governor. Not
    /// `1e9 / fps`: a 59.94 Hz client must not be governed at 60 or 59.
    pub interval_ns: u64,
    pub bitrate_kbps: u32,
    pub slices: u32,
    pub hdr: bool,
    pub intra_refresh: Option<(u32, u32)>,
    pub dpb_depth: u32,
    pub ref_invalidation: bool,
    /// Use the NvFBC CUDA-native backend (opt-in resilience), else DDA/WGC.
    pub nvfbc: bool,
    /// Force a specific D3D11 backend (WGC/DDA); `None` = the OS default.
    pub force_backend: Option<sunburst_capture::Backend>,
    /// Which monitor to capture — the primary, or a virtual display's output.
    pub output: OutputSelect,
    /// NVENC preset P1–P4 (1..=4), within the ULL tuning.
    pub preset: u8,
    /// Variable-bitrate rate control (else CBR).
    pub vbr: bool,
    /// Forced IDR period in frames; `0` = infinite GOP.
    pub idr_period: u32,
}

/// The lock-free signals the control thread raises for the frame path. Read and
/// cleared on the GPU thread; never a lock on the hot path.
pub struct StreamShared {
    /// A client `RequestIdr`, or recovery that found nothing to reference.
    pub request_idr: AtomicBool,
    /// The target bitrate the rate controller last set. The GPU thread
    /// reconfigures the encoder when it changes.
    pub target_kbps: AtomicU32,
    /// The oldest frame the client has abandoned, or [`NO_ABANDON`]. The GPU
    /// thread swaps it out and runs reference invalidation from it; coalescing
    /// to the oldest is correct, since invalidating it covers everything since.
    pub abandon: AtomicU32,
    /// The GPU thread sets this on `Unavailable` (secure desktop / DRM) and
    /// clears it on return; the session manager turns transitions into
    /// `SecureDesktop` messages.
    pub secure: AtomicBool,
}

impl StreamShared {
    pub fn new(initial_kbps: u32) -> Arc<StreamShared> {
        Arc::new(StreamShared {
            request_idr: AtomicBool::new(false),
            target_kbps: AtomicU32::new(initial_kbps),
            abandon: AtomicU32::new(NO_ABANDON),
            secure: AtomicBool::new(false),
        })
    }

    /// Record `frame_id` as abandoned, keeping the modularly-oldest of any
    /// already pending, so the GPU thread invalidates from the earliest.
    pub fn note_abandon(&self, frame_id: Seq16) {
        let mut cur = self.abandon.load(Ordering::Relaxed);
        loop {
            let keep = cur == NO_ABANDON || Seq16(cur as u16).is_newer_than(frame_id);
            if !keep {
                return;
            }
            match self.abandon.compare_exchange_weak(
                cur,
                frame_id.0 as u32,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => return,
                Err(observed) => cur = observed,
            }
        }
    }
}

/// The codec's out-of-band configuration for the client — HEVC VPS/SPS/PPS or an
/// AV1 `av1C` record. Emitted once when the encoder is built (and again after a
/// rebuild); the client needs it before any frame, so it travels the reliable
/// control channel.
#[derive(Clone, Debug)]
pub struct CodecHeaders {
    pub codec: Codec,
    pub sequence: Vec<u8>,
}

/// A running stream. Dropping it (or [`stop`](Self::stop)) tears the threads down.
pub struct Pipeline {
    stop: Arc<AtomicBool>,
    gpu: Option<JoinHandle<()>>,
    send: Option<JoinHandle<()>>,
}

impl Pipeline {
    /// Start streaming to `client` over `socket`. `shared` carries control-thread
    /// signals, `headers` receives the codec config when the encoder is built,
    /// and `retransmit` delivers NACK retransmit requests to the send thread.
    pub fn spawn(
        socket: UdpSocket,
        client: SocketAddr,
        params: PipelineParams,
        shared: Arc<StreamShared>,
        headers: mpsc::Sender<CodecHeaders>,
        retransmit: Consumer,
    ) -> io::Result<Pipeline> {
        let stop = Arc::new(AtomicBool::new(false));
        let (producer, consumer) = packet_ring(RING_CAPACITY);

        let send_stop = Arc::clone(&stop);
        let send_shared = Arc::clone(&shared);
        let send_initial = params.bitrate_kbps;
        let send = std::thread::Builder::new()
            .name("sunburst-send".into())
            .spawn(move || {
                send_loop(
                    &send_stop,
                    &consumer,
                    &retransmit,
                    &socket,
                    client,
                    &send_shared,
                    send_initial,
                )
            })?;
        let send_thread = send.thread().clone();

        let gpu_stop = Arc::clone(&stop);
        let gpu_shared = Arc::clone(&shared);
        let gpu = std::thread::Builder::new()
            .name("sunburst-gpu".into())
            .spawn(move || {
                if let Err(e) = gpu_loop(
                    &gpu_stop,
                    producer,
                    &params,
                    &gpu_shared,
                    &headers,
                    &send_thread,
                ) {
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

    pub fn stop(mut self) {
        self.shutdown();
    }

    /// Wake the send thread now. The control thread calls this after queueing a
    /// retransmit: the client is waiting on that packet, and without a wake it
    /// sits in the ring until the send thread's idle park times out.
    pub fn wake_send(&self) {
        if let Some(s) = &self.send {
            s.thread().unpark();
        }
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
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

/// The convert+encode spine, built from whichever surface the first frame
/// carries. DDA/WGC take the D3D11 path; NvFBC stays CUDA-native.
enum Spine<'a> {
    D3d11 {
        converter: Converter,
        encoder: Encoder<'a>,
    },
    Cuda {
        converter: CudaConverter,
        encoder: Encoder<'a>,
    },
}

impl<'a> Spine<'a> {
    fn encoder(&mut self) -> &mut Encoder<'a> {
        match self {
            Spine::D3d11 { encoder, .. } => encoder,
            Spine::Cuda { encoder, .. } => encoder,
        }
    }
}

/// How long `acquire` may block when no frame is held: long enough to be idle,
/// short enough that a stop request or a keyframe owed to a client on a still
/// screen is noticed promptly.
const IDLE_WAIT: Duration = Duration::from_millis(100);

/// A converted frame: the converter's output surface (valid until the next
/// convert or spine rebuild) and the present timestamp of its content.
#[derive(Clone, Copy)]
struct Surface {
    input: *mut c_void,
    qpc: u32,
}

/// The encode half's state: everything that advances once per encoded frame.
///
/// One encode path for every reason a frame is encoded — admitted on arrival,
/// flushed after the governor held it, or re-encoded on a still screen because
/// the client is owed a keyframe — so control signals (IDR requests, reference
/// invalidation, bitrate changes) are applied identically to all of them.
struct Encoding {
    packetizer: Packetizer,
    producer: Producer,
    refs: Box<dyn RefState>,
    frame_id: Seq16,
    frame_ts: u64,
    need_keyframe: bool,
    last_kbps: u32,
}

impl Encoding {
    /// Whether the client is owed a frame even if the screen never changes: a
    /// keyframe request, recovery from an abandoned frame, or a fresh encoder.
    fn owed(&self, shared: &StreamShared) -> bool {
        self.need_keyframe
            || shared.request_idr.load(Ordering::Relaxed)
            || shared.abandon.load(Ordering::Relaxed) != NO_ABANDON
    }

    /// Encode `surface`, packetize it, and hand the packets to the send thread.
    fn encode(
        &mut self,
        enc: &mut Encoder<'_>,
        surface: Surface,
        params: &PipelineParams,
        shared: &StreamShared,
        send_thread: &Thread,
    ) -> Result<(), String> {
        let fid = self.frame_id;
        let qpc = surface.qpc;

        // Apply control-thread signals before encoding this frame.
        let mut force_idr = self.need_keyframe || shared.request_idr.swap(false, Ordering::AcqRel);
        let abandoned = shared.abandon.swap(NO_ABANDON, Ordering::AcqRel);
        if abandoned != NO_ABANDON && params.ref_invalidation {
            match self.refs.on_abandoned(Seq16(abandoned as u16)) {
                Recovery::Invalidate { from, to } => {
                    let mut id = from;
                    loop {
                        if let Some(ts) = self.refs.timestamp_of(id) {
                            enc.invalidate_ref_frames(ts)?;
                        }
                        if id == to {
                            break;
                        }
                        id = id.next();
                    }
                }
                Recovery::ForceIdr => force_idr = true,
                Recovery::Nothing => {}
            }
        } else if abandoned != NO_ABANDON {
            // The decoder cannot use reference invalidation: a keyframe instead.
            force_idr = true;
        }
        let target = shared.target_kbps.load(Ordering::Relaxed);
        if target != self.last_kbps && target != 0 {
            enc.reconfigure_bitrate(target)?;
            self.last_kbps = target;
        }

        instr::record(Stage::EncodeSubmit, fid.0 as u32);
        self.need_keyframe = false;
        let req = PicRequest {
            timestamp: self.frame_ts,
            force_idr,
        };
        // The wire keyframe flag comes from NVENC's actual encoded picture type,
        // not the request, so an auto-inserted periodic IDR (finite idr_period) is
        // flagged too. begin_frame is deferred to the first emitted unit, when the
        // type is known.
        let packetizer = &mut self.packetizer;
        let producer = &self.producer;
        let mut begun = false;
        let is_idr = enc.encode_slices(surface.input, req, |unit, unit_is_idr| {
            instr::record(Stage::EncodeUnitOut, fid.0 as u32);
            if !begun {
                packetizer.begin_frame(fid, qpc, unit_is_idr);
                begun = true;
            }
            packetizer.push_unit(unit, |pkt| {
                producer.push(pkt);
            });
            instr::record(Stage::Packetize, fid.0 as u32);
            send_thread.unpark();
        })?;
        if !begun {
            // No units emitted (abnormal); open an empty frame so finish_frame has
            // valid state.
            packetizer.begin_frame(fid, qpc, is_idr);
        }
        packetizer.finish_frame(|pkt| {
            producer.push(pkt);
        });
        send_thread.unpark();

        self.refs.on_encoded(fid, self.frame_ts, is_idr);
        self.frame_id = fid.next();
        self.frame_ts += 1;
        Ok(())
    }
}

/// The serial GPU half. Returns `Err` only on setup or a fatal encode failure;
/// `AccessLost` is handled in-loop by rebuilding.
fn gpu_loop(
    stop: &AtomicBool,
    producer: Producer,
    params: &PipelineParams,
    shared: &StreamShared,
    headers: &mpsc::Sender<CodecHeaders>,
    send_thread: &Thread,
) -> Result<(), String> {
    let _rt = RealtimeThread::register();
    instr::register_thread("gpu");

    let mut capture = select::build(
        params.nvfbc,
        params.force_backend,
        params.hdr,
        params.output,
    )
    .map_err(|e| e.to_string())?;
    let nvenc = Nvenc::load()?;
    let mut spine: Option<Spine> = None;
    let mut st = Encoding {
        packetizer: Packetizer::new(),
        producer,
        refs: match params.codec {
            Codec::Hevc => Box::new(HevcRefState::new(params.dpb_depth as u16)),
            Codec::H264 => Box::new(H264RefState::new(params.dpb_depth as u16)),
            Codec::Av1 => Box::new(Av1RefState::new()),
        },
        frame_id: Seq16(0),
        frame_ts: 0,
        need_keyframe: true,
        last_kbps: params.target_kbps_seed(),
    };
    let mut current_color: Option<ConvertOutput> = None;

    // The frame-rate governor: at most one encode per client frame interval
    // (a 144 Hz desktop would otherwise feed NVENC far past what a 60 Hz TV
    // shows), holding an early frame instead of discarding it so the last frame
    // of a motion is never lost. See `sunburst_net::governor`.
    let origin = Instant::now();
    let now_ns = || origin.elapsed().as_nanos() as u64;
    let mut governor = FrameGovernor::new(params.interval_ns, capture.honors_timeout());
    // The frame the governor is holding, if any.
    let mut held: Option<Surface> = None;
    // The most recent converted frame — still intact in the converter's output
    // until the next convert — for a keyframe owed on a still screen.
    let mut last: Option<Surface> = None;

    while !stop.load(Ordering::Relaxed) {
        // Motion stopped with a frame still held: encode it now.
        if let Some(h) = held
            && governor.flush_deadline().is_some_and(|dl| now_ns() >= dl)
        {
            governor.on_flush(now_ns());
            held = None;
            let enc = spine
                .as_mut()
                .ok_or("held a frame without a spine")?
                .encoder();
            st.encode(enc, h, params, shared, send_thread)?;
            continue;
        }
        let timeout = match (held, governor.flush_deadline()) {
            // Wake at the flush deadline (the backends round up to whole ms).
            (Some(_), Some(dl)) => {
                Duration::from_nanos(dl.saturating_sub(now_ns())).max(Duration::from_millis(1))
            }
            _ => IDLE_WAIT,
        };

        let frame = match capture.acquire(timeout) {
            Ok(Some(f)) => f,
            Ok(None) => {
                // A still screen: nothing new to encode. But a client owed a
                // keyframe (its request, recovery from a lost frame) must not
                // wait for the screen to change — re-encode the last frame.
                if held.is_none()
                    && let Some(surface) = last
                    && st.owed(shared)
                    && let Some(spine) = spine.as_mut()
                {
                    st.encode(spine.encoder(), surface, params, shared, send_thread)?;
                }
                continue;
            }
            Err(CaptureError::AccessLost) => {
                capture = select::build(
                    params.nvfbc,
                    params.force_backend,
                    params.hdr,
                    params.output,
                )
                .map_err(|e| e.to_string())?;
                // The spine (and every surface it produced) goes with it.
                spine = None;
                held = None;
                last = None;
                st.need_keyframe = true;
                governor = FrameGovernor::new(params.interval_ns, capture.honors_timeout());
                continue;
            }
            Err(CaptureError::Unavailable) => {
                shared.secure.store(true, Ordering::Relaxed);
                // Never a frozen frame (CLAUDE.md): nothing held or kept over
                // from before the secure desktop is sent after it.
                held = None;
                last = None;
                governor.reset();
                std::thread::sleep(Duration::from_millis(50));
                st.need_keyframe = true;
                continue;
            }
            Err(CaptureError::Backend(m)) => return Err(m),
        };
        shared.secure.store(false, Ordering::Relaxed);

        let admit = governor.on_frame(now_ns());
        if admit == Admit::Skip {
            continue;
        }

        let fid = st.frame_id;
        instr::record(Stage::CaptureAcquire, fid.0 as u32);
        let qpc = frame.meta().present_qpc as u32;

        // Convert to P010 on the surface's own path, building the spine and the
        // encoder (and emitting the codec headers) on the first frame.
        // A (re)build starts at the rate controller's live target.
        let live_kbps = match shared.target_kbps.load(Ordering::Relaxed) {
            0 => params.bitrate_kbps,
            t => t,
        };
        let (input, first_build) = build_and_convert(
            &mut spine,
            &nvenc,
            params,
            capture.as_ref(),
            frame,
            &mut current_color,
            live_kbps,
        )?;
        instr::record(Stage::ColorConvert, fid.0 as u32);
        if first_build {
            // The encoder now runs at `live_kbps`; without this, an unchanged
            // target would never be re-applied and `reconfigure_bitrate`'s
            // equal-value early return would hide the mismatch.
            st.last_kbps = live_kbps;
            // A freshly (re)built encoder must open on an IDR.
            st.need_keyframe = true;
            let enc = spine.as_mut().expect("just built").encoder();
            let sequence = match params.codec {
                // HEVC and H.264 send their Annex-B parameter sets (VPS/SPS/PPS,
                // or SPS/PPS); AV1 sends the av1C record.
                Codec::Hevc | Codec::H264 => enc.sequence_header()?,
                Codec::Av1 => enc.av1c()?,
            };
            let _ = headers.send(CodecHeaders {
                codec: params.codec,
                sequence,
            });
        }
        let surface = Surface { input, qpc };
        last = Some(surface);

        match admit {
            Admit::Encode => {
                held = None;
                let enc = spine.as_mut().expect("built above").encoder();
                st.encode(enc, surface, params, shared, send_thread)?;
            }
            Admit::Hold => {
                // The next `acquire` releases the capture surface this convert
                // reads; make sure the GPU is done with it first. (An encoded
                // frame needs no wait: NVENC consumes the convert's output.)
                if let Some(Spine::D3d11 { converter, .. }) = &spine {
                    converter.wait_idle()?;
                }
                held = Some(surface);
            }
            Admit::Skip => unreachable!("skipped above"),
        }
    }
    Ok(())
}

/// Build the spine on the first frame and convert this frame to a P010 input
/// pointer. Returns `(input, first_build)`.
fn build_and_convert<'a>(
    spine: &mut Option<Spine<'a>>,
    nvenc: &'a Nvenc,
    params: &PipelineParams,
    capture: &dyn sunburst_capture::Capture,
    frame: Frame,
    current_color: &mut Option<ConvertOutput>,
    bitrate_kbps: u32,
) -> Result<(*mut c_void, bool), String> {
    let mut ecfg = EncoderConfig::new(params.codec, params.width, params.height);
    ecfg.fps = params.fps;
    // The rate controller's current target, not the session's starting one: a
    // rebuild (AccessLost, HDR<->SDR) must not snap a throttled stream back up.
    ecfg.bitrate_kbps = bitrate_kbps;
    ecfg.slices = params.slices;
    ecfg.dpb_depth = params.dpb_depth;
    ecfg.intra_refresh = params.intra_refresh;
    ecfg.preset = params.preset;
    ecfg.vbr = params.vbr;
    ecfg.idr_period = params.idr_period;
    // The color path follows the *actual* captured desktop state (per frame),
    // not the codec: HEVC/AV1 encode BT.2020 PQ (P010) only when the source is
    // genuinely HDR, else BT.709 (P010Sdr); H.264 is always BT.709 NV12. So a
    // live HDR<->SDR flip changes `output` and rebuilds the spine below.
    let frame_hdr = frame.meta().hdr;
    let hdr_source = frame_hdr;
    let (output, color) = match params.codec {
        Codec::H264 => (ConvertOutput::Nv12, ColorSpace::Bt709),
        Codec::Hevc | Codec::Av1 if frame_hdr => (ConvertOutput::P010, ColorSpace::Bt2020Pq),
        Codec::Hevc | Codec::Av1 => (ConvertOutput::P010Sdr, ColorSpace::Bt709),
    };
    ecfg.color = color;
    // Mastering-display / MaxCLL SEI only when the source is genuinely HDR.
    ecfg.hdr = if frame_hdr {
        capture.caps().hdr_metadata
    } else {
        None
    };
    // A color-path change with no AccessLost (e.g. Win11 per-app HDR) drops the
    // spine so it rebuilds with the new shader + VUI; the caller forces an IDR.
    if *current_color != Some(output) {
        *spine = None;
        *current_color = Some(output);
    }

    match frame {
        Frame::Texture(tf) => {
            let (w, h) = (tf.meta.width, tf.meta.height);
            let first = !matches!(spine, Some(Spine::D3d11 { .. }));
            if first {
                // SAFETY: a captured texture always has a live device.
                let device: ID3D11Device =
                    unsafe { tf.texture.GetDevice() }.map_err(|e| e.to_string())?;
                ecfg.width = w;
                ecfg.height = h;
                let srgb_input = matches!(tf.format, TextureFormat::Bgra8);
                let converter = Converter::new(&tf.texture, output, hdr_source, srgb_input)?;
                let encoder = Encoder::new(nvenc, device.as_raw(), &ecfg)?;
                *spine = Some(Spine::D3d11 { converter, encoder });
            }
            let Some(Spine::D3d11 { converter, .. }) = spine.as_mut() else {
                return Err("spine is not D3D11".into());
            };
            let surface = converter.convert(&tf.texture, w, h)?;
            Ok((surface.as_raw(), first))
        }
        Frame::Cuda(cf) => {
            let (w, h) = (cf.meta.width, cf.meta.height);
            let ctx = capture
                .cuda_context()
                .ok_or("a CUDA frame arrived but the backend exposes no context")?
                as CuContext;
            let first = !matches!(spine, Some(Spine::Cuda { .. }));
            if first {
                ecfg.width = w;
                ecfg.height = h;
                let converter = CudaConverter::new(ctx, w, h, output, hdr_source)?;
                let pitch = converter.pitch();
                let encoder = Encoder::new_cuda(nvenc, ctx, &ecfg, pitch)?;
                *spine = Some(Spine::Cuda { converter, encoder });
            }
            let Some(Spine::Cuda { converter, .. }) = spine.as_mut() else {
                return Err("spine is not CUDA".into());
            };
            let surface: CuDevicePtr = converter.convert(cf.device_ptr, cf.pitch as u32)?;
            Ok((surface as *mut c_void, first))
        }
    }
}

/// The network half. Services retransmits first (urgent, unpaced), then paces
/// the main stream onto the wire in USO batches, caching each packet so a NACK
/// can be answered without the encoder.
fn send_loop(
    stop: &AtomicBool,
    consumer: &Consumer,
    retransmit: &Consumer,
    socket: &UdpSocket,
    client: SocketAddr,
    shared: &StreamShared,
    initial_kbps: u32,
) {
    let _rt = RealtimeThread::register();
    instr::register_thread("send");

    let no_uso = std::env::var_os("SUNBURST_NO_USO").is_some();
    let mut sender = if no_uso {
        WsaSender::without_offload(socket)
    } else {
        WsaSender::new(socket)
    };
    let mut cache = RetransmitCache::default();
    // Pace at twice the rate so a frame's bytes leave over ~half its interval,
    // with a small burst so the sender is not throttled after an idle gap. The
    // rate follows the live target up but never below the session's start —
    // see `video_pace_bps` for the feedback loop that pacing below the encoder's
    // real output caused.
    let mut paced_kbps = initial_kbps;
    let mut pacer = Pacer::new(video_pace_bps(initial_kbps, initial_kbps), 500_000, 0);
    let mut batch = Batch::new();
    let origin = Instant::now();
    let now_ns = || origin.elapsed().as_nanos() as u64;

    while !stop.load(Ordering::Relaxed) {
        let target = shared.target_kbps.load(Ordering::Relaxed);
        if target != paced_kbps {
            pacer.set_rate(video_pace_bps(initial_kbps, target));
            paced_kbps = target;
        }

        // Retransmits first: they are closing a gap the client already noticed.
        while retransmit.pop_with(|req| service_retransmit(req, &mut cache, socket, client)) {}

        let mut drained = false;
        while consumer.pop_with(|pkt| {
            cache.store(pkt);
            if let Some(h) = Header::decode(pkt) {
                instr::record(Stage::Send, h.frame_id.0 as u32);
            }
            if !batch.try_add(pkt) {
                flush(&mut batch, &mut sender, &mut pacer, client, now_ns);
                batch.try_add(pkt);
            }
        }) {
            drained = true;
        }
        flush(&mut batch, &mut sender, &mut pacer, client, now_ns);

        if !drained {
            std::thread::park_timeout(Duration::from_micros(250));
        }
    }
    // A clean stop still flushes the last frame.
    while consumer.pop_with(|pkt| {
        if !batch.try_add(pkt) {
            flush(&mut batch, &mut sender, &mut pacer, client, now_ns);
            batch.try_add(pkt);
        }
    }) {}
    flush(&mut batch, &mut sender, &mut pacer, client, now_ns);
}

/// Wait for the pace, then hand the batch to the sender.
fn flush(
    batch: &mut Batch,
    sender: &mut WsaSender<'_>,
    pacer: &mut Pacer,
    client: SocketAddr,
    now_ns: impl Fn() -> u64,
) {
    if batch.is_empty() {
        return;
    }
    let (bytes, seg) = batch.bytes();
    let wait = pacer.wait_ns(now_ns());
    if wait > 0 {
        std::thread::sleep(Duration::from_nanos(wait));
    }
    let _ = sender.send_batch(bytes, seg, client);
    pacer.record(bytes.len(), now_ns());
    batch.clear();
}

/// Answer one retransmit request: `[frame_id: u16][Nack body]`. Each requested
/// packet still in the cache goes back out; a miss (aged out) is skipped.
fn service_retransmit(
    req: &[u8],
    cache: &mut RetransmitCache,
    socket: &UdpSocket,
    client: SocketAddr,
) {
    if req.len() < 2 {
        return;
    }
    let frame_id = Seq16(u16::from_le_bytes([req[0], req[1]]));
    let Some(nack) = Nack::decode(&req[2..]) else {
        return;
    };
    for idx in nack.missing() {
        if let Some(pkt) = cache.get(frame_id, idx) {
            let _ = socket.send_to(pkt, client);
        }
    }
}

impl PipelineParams {
    fn target_kbps_seed(&self) -> u32 {
        self.bitrate_kbps
    }
}

use crate::realtime::RealtimeThread;

/// Build a retransmit-ring producer/consumer pair sized for this pipeline.
pub fn retransmit_ring() -> (Producer, Consumer) {
    packet_ring(RETRANSMIT_CAPACITY)
}
