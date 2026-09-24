// SPDX-License-Identifier: GPL-2.0-or-later

//! Control-channel messages.
//!
//! The reliable channel is control plane, so allocation is permitted here and
//! only here.
//!
//! # The envelope
//!
//! Every message is `kind: u8`, `len: u16`, then `len` bytes of payload. The
//! length is what lets a receiver **skip a kind it does not decode** rather than
//! desynchronising, which is what allows a client from a later build to talk to
//! an older server without a version negotiation.
//!
//! # Session negotiation
//!
//! `Hello` (carrying the client's nonce and the codecs it can decode) is
//! answered by [`SessionConfig`], which pins everything the client needs before
//! the first frame: codec, geometry, rate, HDR mastering metadata, the server's
//! nonce for the session key, and the clock facts that let a video header's
//! `qpc_timestamp` be read on the other machine. [`ServerControl::CodecPrivate`]
//! follows with the decoder configuration (VPS/SPS/PPS, or an av1C record) and
//! is sent again whenever the encoder is rebuilt.
//!
//! A cursor bitmap is larger than one reliable message, so [`CursorChunk`]
//! carries it in pieces. Every chunk repeats the shape's head, and the channel
//! delivers in order, so the receiver appends and never has to reorder.

pub use super::art::{ART_CHUNK_MAX, AppPage, ArtChunk, ArtFormat, ArtRef};
pub use super::color::{ColorInfo, HdrMastering};
use super::pairing::{NONCE_LEN, TAG_LEN};
use super::seq::Seq16;

/// Bytes of envelope before the payload.
pub const ENVELOPE_LEN: usize = 3;

/// Longest string this format carries. Names, models and ABIs are all short, and
/// a single length byte makes the encoding one case rather than two.
pub const MAX_STRING: usize = 255;

/// Why a control message could not be encoded or decoded.
///
/// `Display` is written out rather than derived: `thiserror` would put a
/// proc-macro dependency on the crate the frame path builds against, and
/// CLAUDE.md keeps `sunburst-core` to `blake3`, `subtle` and `libc`. Five
/// variants is not worth widening that.
#[derive(Debug, PartialEq, Eq)]
pub enum ControlError {
    Truncated,
    BadLength(usize),
    StringTooLong(usize),
    BadUtf8,
    OutOfRange(&'static str),
}

impl core::fmt::Display for ControlError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ControlError::Truncated => write!(f, "buffer ended mid-message"),
            ControlError::BadLength(n) => {
                write!(f, "declared payload length {n} exceeds the buffer")
            }
            ControlError::StringTooLong(n) => {
                write!(f, "string of {n} bytes exceeds the {MAX_STRING} byte limit")
            }
            ControlError::BadUtf8 => write!(f, "string is not valid UTF-8"),
            ControlError::OutOfRange(what) => write!(f, "value out of range for {what}"),
        }
    }
}

impl std::error::Error for ControlError {}

/// Client → server discriminants. Complete, including kinds not yet decoded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum ClientMessage {
    Hello = 0,
    DecoderQuirks = 1,
    RequestIdr = 2,
    Resize = 3,
    PadConnected = 4,
    PadDisconnected = 5,
    Bye = 6,
    PairRequest = 7,
    PairConfirm = 8,
    ListApps = 9,
    LaunchApp = 10,
    ArtRequest = 11,
}

/// Server → client discriminants. Complete, including kinds not yet decoded.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum ServerMessage {
    SessionConfig = 0,
    CodecPrivate = 1,
    CursorShape = 2,
    CursorPosition = 3,
    SecureDesktop = 4,
    Bye = 5,
    PairChallenge = 6,
    AppList = 7,
    ArtChunk = 8,
    LaunchResult = 9,
}

impl ClientMessage {
    pub const fn from_u8(v: u8) -> Option<ClientMessage> {
        Some(match v {
            0 => ClientMessage::Hello,
            1 => ClientMessage::DecoderQuirks,
            2 => ClientMessage::RequestIdr,
            3 => ClientMessage::Resize,
            4 => ClientMessage::PadConnected,
            5 => ClientMessage::PadDisconnected,
            6 => ClientMessage::Bye,
            7 => ClientMessage::PairRequest,
            8 => ClientMessage::PairConfirm,
            9 => ClientMessage::ListApps,
            10 => ClientMessage::LaunchApp,
            11 => ClientMessage::ArtRequest,
            _ => return None,
        })
    }

    /// Whether this kind may arrive before the peer has a key.
    ///
    /// Only the pairing exchange. Everything else must be authenticated, which
    /// is the rule the endpoint enforces — this is the allow-list it consults.
    pub const fn is_pre_pairing(self) -> bool {
        matches!(
            self,
            ClientMessage::PairRequest | ClientMessage::PairConfirm
        )
    }
}

impl ServerMessage {
    pub const fn from_u8(v: u8) -> Option<ServerMessage> {
        Some(match v {
            0 => ServerMessage::SessionConfig,
            1 => ServerMessage::CodecPrivate,
            2 => ServerMessage::CursorShape,
            3 => ServerMessage::CursorPosition,
            4 => ServerMessage::SecureDesktop,
            5 => ServerMessage::Bye,
            6 => ServerMessage::PairChallenge,
            7 => ServerMessage::AppList,
            8 => ServerMessage::ArtChunk,
            9 => ServerMessage::LaunchResult,
            _ => return None,
        })
    }

    pub const fn is_pre_pairing(self) -> bool {
        matches!(self, ServerMessage::PairChallenge)
    }
}

/// Decoder behaviour the client reports at handshake.
///
/// Exists from day one rather than being retrofitted, per CLAUDE.md. Every field
/// is a measurement from the Phase 0.2 enumeration, not an assumption.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DecoderQuirks {
    /// Amlogic decoders are known to mishandle this; when false the server
    /// falls back to `RequestIdr` instead of `NvEncInvalidateRefFrames`.
    pub ref_invalidation: bool,
    pub intra_refresh: bool,
    pub slice_output: bool,
    pub needs_annexb_startcodes: bool,
    /// The Shield's decoder caps out before 1GbE does; ~150 Mbps is its
    /// practical ceiling.
    pub max_bitrate_hint: u32,
}

impl Default for DecoderQuirks {
    /// The conservative decoder: nothing clever, periodic IDR, modest bitrate.
    ///
    /// Defaults matter here because they are what an unknown device gets. A
    /// permissive default would mean a new decoder's first experience of
    /// Sunburst is corruption, and the safe answer costs only latency.
    fn default() -> Self {
        DecoderQuirks {
            ref_invalidation: false,
            intra_refresh: false,
            slice_output: false,
            needs_annexb_startcodes: true,
            max_bitrate_hint: 50_000_000,
        }
    }
}

impl DecoderQuirks {
    fn flags(&self) -> u8 {
        u8::from(self.ref_invalidation)
            | u8::from(self.intra_refresh) << 1
            | u8::from(self.slice_output) << 2
            | u8::from(self.needs_annexb_startcodes) << 3
    }

    fn from_flags(bits: u8, max_bitrate_hint: u32) -> DecoderQuirks {
        DecoderQuirks {
            ref_invalidation: bits & 1 != 0,
            intra_refresh: bits & 2 != 0,
            slice_output: bits & 4 != 0,
            needs_annexb_startcodes: bits & 8 != 0,
            max_bitrate_hint,
        }
    }
}

/// A client asking to pair. Carries no PIN, deliberately — see
/// [`super::pairing`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairRequest {
    pub name: String,
    pub model: String,
    pub abi: String,
    pub quirks: DecoderQuirks,
    pub client_nonce: [u8; NONCE_LEN],
}

/// A client announcing itself on an already-paired link.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hello {
    /// Which paired client this claims to be.
    ///
    /// **A hint, not a credential.** It only decides which key the endpoint
    /// tries first; the MAC is what proves identity, and a wrong id here costs
    /// one extra verification attempt.
    pub client_id: u32,
    pub name: String,
    pub abi: String,
    pub width: u32,
    pub height: u32,
    /// Millihertz — 59940 for 59.94 Hz. Whole hertz would round a fractional
    /// mode to the wrong pacing target, which is a bug moonlight-trexx already
    /// had to fix once.
    pub refresh_mhz: u32,
    pub client_nonce: [u8; NONCE_LEN],
    pub clock_offset_ns: i64,
    /// Bitmask of the codecs the client can decode — [`codecs::HEVC_MAIN10`],
    /// [`codecs::AV1_MAIN10`] — from its `MediaCodecList` enumeration. The
    /// server chooses among these; a client never receives a codec it did not
    /// offer.
    pub codecs: u8,
    /// The codec the client *requests*, if the user picked one on the device.
    /// `None` = no preference (the server decides). The server honours it when
    /// the client can also decode it, taking precedence over the server's
    /// global default; it can never yield a codec absent from `codecs`.
    pub prefer_codec: Option<StreamCodec>,
    /// A client-requested bitrate ceiling in kbps, or `0` for no client limit.
    /// The server only ever lowers its target by this, never raises it.
    pub max_bitrate_kbps: u32,
}

/// Bits of [`Hello::codecs`].
pub mod codecs {
    pub const HEVC_MAIN10: u8 = 1 << 0;
    pub const AV1_MAIN10: u8 = 1 << 1;
    /// H.264 High profile, 8-bit. SDR only (NVENC H.264 has no 10-bit), so it is
    /// a low-latency fallback, never an HDR path.
    pub const H264: u8 = 1 << 2;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppListing {
    pub id: u32,
    pub name: String,
    /// Its box art, if it has any: fetch with `ArtRequest` unless an image with
    /// this digest is already cached.
    pub art: Option<ArtRef>,
}

/// The codec a session streams. The wire value of `SessionConfig.codec` and
/// `CodecPrivate.codec`; the encoder crate maps its own `Codec` onto this.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum StreamCodec {
    /// HEVC Main10, subdivided into slices. The Shield's codec.
    Hevc = 0,
    /// AV1 Main10, subdivided into tiles. The Homatics' codec.
    Av1 = 1,
    /// H.264 High profile, 8-bit SDR. A low-latency, opt-in fallback — HDR stays
    /// on HEVC/AV1. Subdivided into slices, like HEVC.
    H264 = 2,
}

impl StreamCodec {
    pub const fn from_u8(v: u8) -> Option<StreamCodec> {
        match v {
            0 => Some(StreamCodec::Hevc),
            1 => Some(StreamCodec::Av1),
            2 => Some(StreamCodec::H264),
            _ => None,
        }
    }

    /// The [`Hello::codecs`] bit that advertises this codec.
    pub const fn hello_bit(self) -> u8 {
        match self {
            StreamCodec::Hevc => codecs::HEVC_MAIN10,
            StreamCodec::Av1 => codecs::AV1_MAIN10,
            StreamCodec::H264 => codecs::H264,
        }
    }
}

/// Pick the codec to stream, given the server's `prefer` (`None` = "auto") and
/// the bitmask of codecs the client advertised in [`Hello::codecs`].
///
/// A pinned preference the client cannot decode yields `None` — the server
/// declines rather than sending a stream that will not play. Auto takes AV1 when
/// offered (it is the more efficient of the two), else HEVC, and only falls to
/// H.264 when the client offers nothing else — auto never chooses H.264 over an
/// HDR-capable codec, because H.264 here is 8-bit SDR.
pub fn negotiate_codec(prefer: Option<StreamCodec>, client_codecs: u8) -> Option<StreamCodec> {
    let has = |c: StreamCodec| client_codecs & c.hello_bit() != 0;
    match prefer {
        Some(c) => has(c).then_some(c),
        None if has(StreamCodec::Av1) => Some(StreamCodec::Av1),
        None if has(StreamCodec::Hevc) => Some(StreamCodec::Hevc),
        None if has(StreamCodec::H264) => Some(StreamCodec::H264),
        None => None,
    }
}

/// What one encoder build produces, for the decoder to be configured from.
///
/// Sent when the encoder is built and **again after every rebuild** (an
/// `AccessLost`, an HDR↔SDR flip), and authoritative for the frames from
/// `first_frame` on. `SessionConfig` says what the server expects at the start
/// of a session; this says what each build actually produces. The colour path
/// follows the captured desktop, and the desktop can change under a session, so
/// this is the carrier that stays true.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CodecPrivate {
    pub codec: StreamCodec,
    /// HEVC VPS/SPS/PPS or H.264 SPS/PPS (Annex-B), or the AV1 av1C record
    /// (`csd-0`). PROTOCOL.md flags av1C as a silent-failure point — a wrong
    /// record configures fine and outputs nothing — so the bytes are the
    /// encoder's own, never re-derived here.
    pub data: Vec<u8>,
    /// The encoded picture, in pixels: the captured output's size, which is not
    /// necessarily the size the client asked for in `Hello`.
    pub width: u16,
    pub height: u16,
    /// The frame id of the IDR that opens this build. Anything older belongs to
    /// the previous build and must not reach a decoder configured for this one.
    pub first_frame: Seq16,
    pub color: ColorInfo,
    /// ST 2086 mastering, when the build is PQ and the display reported it.
    pub hdr: Option<HdrMastering>,
}

/// Opus audio parameters for the session, present when the server streams sound.
///
/// Opus needs no per-stream codec-private blob — the decoder is configured from
/// the sample rate and channel count alone — so there is no audio equivalent of
/// [`ServerControl::CodecPrivate`]. `frame_samples` is the samples per channel
/// in one packet (240 = 5 ms at 48 kHz), which fixes the packet cadence both
/// ends pace against.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct AudioParams {
    /// Sample rate in Hz. 48 000 in practice; see the 48 kHz endpoint note in
    /// CLAUDE.md.
    pub sample_rate: u32,
    /// Channels: 2 (stereo) today.
    pub channels: u8,
    /// Samples per channel in one Opus packet.
    pub frame_samples: u16,
}

/// Everything the client needs before the first frame arrives. Sent reliably,
/// once per session, signed with the pairing key because it carries the nonce
/// the session key is derived from.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SessionConfig {
    pub session_id: u32,
    pub codec: StreamCodec,
    pub width: u16,
    pub height: u16,
    /// Millihertz, the same unit as [`Hello::refresh_mhz`].
    pub fps_mhz: u32,
    /// The initial target; the rate controller moves it afterwards.
    pub bitrate_kbps: u32,
    /// Present when the stream is HDR (scRGB capture → BT.2020 PQ).
    pub hdr: Option<HdrMastering>,
    /// Present when the server streams audio; `None` disables it for the session.
    pub audio: Option<AudioParams>,
    /// What the server will actually do, given the quirks the client reported
    /// and what the encoder supports.
    pub intra_refresh: bool,
    pub ref_invalidation: bool,
    /// Subframe units per frame: HEVC/H.264 slices, or AV1 tiles (the server
    /// arranges them as the nearest power-of-two grid the level allows).
    pub slices: u8,
    pub server_nonce: [u8; NONCE_LEN],
    /// Ticks per second of the clock behind every video header's
    /// `qpc_timestamp`, so the client can turn ticks into nanoseconds.
    pub qpc_freq_hz: u64,
    /// The server's clock, in nanoseconds, when this message was queued.
    pub server_ns: i64,
    /// Server time between receiving `Hello` and queuing this, so the client can
    /// subtract it from its measured round trip: `offset ≈ server_ns −
    /// t_hello_sent − (rtt − hello_delay_ns) / 2`.
    pub hello_delay_ns: u32,
    /// How far the server's pointer moves per relative mouse count the client
    /// sends, in server pixels × 1000: the mouse sensitivity times Windows'
    /// pointer-speed multiplier. What a client needs to move its own cursor
    /// overlay from its own input. `0` means do not predict — Enhanced Pointer
    /// Precision is on, and its curve is not something to guess at.
    pub pointer_gain_milli: u16,
}

/// Pixel format of a [`CursorChunk`]. One value today; a byte on the wire so a
/// second one never needs a new message kind.
pub const CURSOR_FORMAT_BGRA32: u8 = 0;

/// Largest `data` a [`CursorChunk`] carries. Well under the reliable channel's
/// per-message payload once the envelope and the chunk head are counted;
/// `sunburst-net` asserts that at compile time against its own frame overhead.
pub const CURSOR_CHUNK_MAX: usize = 1024;

/// One piece of a cursor bitmap. Every chunk repeats the shape's head so a
/// receiver can start from any of them; the reliable channel's in-order
/// delivery is what makes appending in arrival order correct.
///
/// A hidden cursor is a shape with `width == 0` and no data.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CursorChunk {
    /// Increments per new shape; a chunk for a shape the receiver has already
    /// completed is ignored.
    pub shape_id: u32,
    pub width: u16,
    pub height: u16,
    pub hotspot_x: u16,
    pub hotspot_y: u16,
    pub format: u8,
    /// Pixel bytes for the whole shape.
    pub total_len: u32,
    /// This chunk's byte offset into the shape.
    pub offset: u32,
    pub data: Vec<u8>,
}

/// A decoded client → server message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientControl {
    PairRequest(PairRequest),
    PairConfirm {
        request_id: u32,
        tag: [u8; TAG_LEN],
    },
    Hello(Hello),
    Quirks(DecoderQuirks),
    ListApps,
    LaunchApp {
        app_id: u32,
    },
    /// Send this app's box art (see `proto::art`).
    ArtRequest {
        app_id: u32,
        digest: [u8; 16],
    },
    RequestIdr,
    Resize {
        width: u32,
        height: u32,
        refresh_mhz: u32,
    },
    PadConnected {
        pad_index: u8,
        pad_type: u8,
        capabilities: u16,
    },
    PadDisconnected {
        pad_index: u8,
    },
    Bye,
    /// A kind this build does not decode. Skipped, never fatal.
    Unhandled(u8),
}

/// A decoded server → client message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ServerControl {
    PairChallenge {
        request_id: u32,
        server_nonce: [u8; NONCE_LEN],
    },
    /// One page of the app list; see [`app_list_pages`](super::art::app_list_pages).
    AppList(AppPage),
    ArtChunk(ArtChunk),
    /// The answer to `LaunchApp`: whether it started, and why not.
    LaunchResult {
        app_id: u32,
        ok: bool,
        message: String,
    },
    SessionConfig(SessionConfig),
    /// One encoder build's configuration. Sent when the encoder is built and
    /// again after every rebuild; see [`CodecPrivate`].
    CodecPrivate(CodecPrivate),
    CursorShape(CursorChunk),
    /// The server-observed pointer. Sent when it changes: visibility at once,
    /// motion at most every 100 ms, nothing while it is still.
    CursorPosition {
        /// 0–65535, normalised to the captured output's rectangle.
        x: u16,
        y: u16,
        visible: bool,
        /// The newest mouse input (`InputPacket.input_seq`) the server had
        /// applied when it sampled the pointer, or 0 for none. A client that
        /// predicts its cursor from its own input replays what came after this
        /// on top of the position.
        input_seq: u32,
    },
    /// Capture is unavailable (UAC prompt, lock screen, DRM). The client shows
    /// a placeholder rather than the last frame; `active: false` ends it.
    SecureDesktop {
        active: bool,
    },
    Bye,
    Unhandled(u8),
}

// ---------------------------------------------------------------- encoding

/// Append `kind`, a length, and whatever `body` writes.
fn envelope(out: &mut Vec<u8>, kind: u8, body: impl FnOnce(&mut Vec<u8>)) {
    out.push(kind);
    let len_at = out.len();
    out.extend_from_slice(&[0, 0]);
    let start = out.len();
    body(out);
    let len = u16::try_from(out.len() - start).expect("control payloads are far below 64 KiB");
    out[len_at..len_at + 2].copy_from_slice(&len.to_le_bytes());
}

/// Bytes [`put_str`] writes for `s`.
pub(crate) fn put_str_len(s: &str) -> usize {
    1 + s.len()
}

fn put_str(out: &mut Vec<u8>, s: &str) -> Result<(), ControlError> {
    let bytes = s.as_bytes();
    if bytes.len() > MAX_STRING {
        return Err(ControlError::StringTooLong(bytes.len()));
    }
    out.push(bytes.len() as u8);
    out.extend_from_slice(bytes);
    Ok(())
}

impl ClientControl {
    pub const fn kind(&self) -> Option<ClientMessage> {
        Some(match self {
            ClientControl::Hello(_) => ClientMessage::Hello,
            ClientControl::Quirks(_) => ClientMessage::DecoderQuirks,
            ClientControl::RequestIdr => ClientMessage::RequestIdr,
            ClientControl::Resize { .. } => ClientMessage::Resize,
            ClientControl::PadConnected { .. } => ClientMessage::PadConnected,
            ClientControl::PadDisconnected { .. } => ClientMessage::PadDisconnected,
            ClientControl::Bye => ClientMessage::Bye,
            ClientControl::PairRequest(_) => ClientMessage::PairRequest,
            ClientControl::PairConfirm { .. } => ClientMessage::PairConfirm,
            ClientControl::ListApps => ClientMessage::ListApps,
            ClientControl::LaunchApp { .. } => ClientMessage::LaunchApp,
            ClientControl::ArtRequest { .. } => ClientMessage::ArtRequest,
            ClientControl::Unhandled(_) => return None,
        })
    }

    pub fn encode(&self) -> Result<Vec<u8>, ControlError> {
        let kind = self.kind().ok_or(ControlError::OutOfRange(
            "cannot encode an unhandled message",
        ))? as u8;
        let mut out = Vec::with_capacity(64);
        let mut err = Ok(());

        envelope(&mut out, kind, |b| match self {
            ClientControl::PairRequest(r) => {
                err = put_str(b, &r.name)
                    .and_then(|()| put_str(b, &r.model))
                    .and_then(|()| put_str(b, &r.abi));
                b.push(r.quirks.flags());
                b.extend_from_slice(&r.quirks.max_bitrate_hint.to_le_bytes());
                b.extend_from_slice(&r.client_nonce);
            }
            ClientControl::PairConfirm { request_id, tag } => {
                b.extend_from_slice(&request_id.to_le_bytes());
                b.extend_from_slice(tag);
            }
            ClientControl::Hello(h) => {
                b.extend_from_slice(&h.client_id.to_le_bytes());
                err = put_str(b, &h.name).and_then(|()| put_str(b, &h.abi));
                b.extend_from_slice(&h.width.to_le_bytes());
                b.extend_from_slice(&h.height.to_le_bytes());
                b.extend_from_slice(&h.refresh_mhz.to_le_bytes());
                b.extend_from_slice(&h.client_nonce);
                b.extend_from_slice(&h.clock_offset_ns.to_le_bytes());
                b.push(h.codecs);
                // 0xFF = no codec preference; else the StreamCodec wire value.
                b.push(h.prefer_codec.map_or(0xFF, |c| c as u8));
                b.extend_from_slice(&h.max_bitrate_kbps.to_le_bytes());
            }
            ClientControl::Quirks(q) => {
                b.push(q.flags());
                b.extend_from_slice(&q.max_bitrate_hint.to_le_bytes());
            }
            ClientControl::LaunchApp { app_id } => b.extend_from_slice(&app_id.to_le_bytes()),
            ClientControl::ArtRequest { app_id, digest } => {
                b.extend_from_slice(&app_id.to_le_bytes());
                b.extend_from_slice(digest);
            }
            ClientControl::Resize {
                width,
                height,
                refresh_mhz,
            } => {
                b.extend_from_slice(&width.to_le_bytes());
                b.extend_from_slice(&height.to_le_bytes());
                b.extend_from_slice(&refresh_mhz.to_le_bytes());
            }
            ClientControl::PadConnected {
                pad_index,
                pad_type,
                capabilities,
            } => {
                b.push(*pad_index);
                b.push(*pad_type);
                b.extend_from_slice(&capabilities.to_le_bytes());
            }
            ClientControl::PadDisconnected { pad_index } => b.push(*pad_index),
            ClientControl::ListApps
            | ClientControl::RequestIdr
            | ClientControl::Bye
            | ClientControl::Unhandled(_) => {}
        });

        err?;
        Ok(out)
    }

    /// Decode one message, returning it and the bytes consumed.
    pub fn decode(buf: &[u8]) -> Result<(ClientControl, usize), ControlError> {
        let (kind, payload, consumed) = split_envelope(buf)?;
        let Some(known) = ClientMessage::from_u8(kind) else {
            return Ok((ClientControl::Unhandled(kind), consumed));
        };

        let mut r = Reader::new(payload);
        let message = match known {
            ClientMessage::PairRequest => ClientControl::PairRequest(PairRequest {
                name: r.string()?,
                model: r.string()?,
                abi: r.string()?,
                quirks: DecoderQuirks::from_flags(r.u8()?, r.u32()?),
                client_nonce: r.array::<NONCE_LEN>()?,
            }),
            ClientMessage::PairConfirm => ClientControl::PairConfirm {
                request_id: r.u32()?,
                tag: r.array::<TAG_LEN>()?,
            },
            ClientMessage::Hello => ClientControl::Hello(Hello {
                client_id: r.u32()?,
                name: r.string()?,
                abi: r.string()?,
                width: r.u32()?,
                height: r.u32()?,
                refresh_mhz: r.u32()?,
                client_nonce: r.array::<NONCE_LEN>()?,
                clock_offset_ns: r.u64()? as i64,
                codecs: r.u8()?,
                prefer_codec: match r.u8()? {
                    0xFF => None,
                    v => StreamCodec::from_u8(v),
                },
                max_bitrate_kbps: r.u32()?,
            }),
            ClientMessage::DecoderQuirks => {
                ClientControl::Quirks(DecoderQuirks::from_flags(r.u8()?, r.u32()?))
            }
            ClientMessage::LaunchApp => ClientControl::LaunchApp { app_id: r.u32()? },
            ClientMessage::ArtRequest => ClientControl::ArtRequest {
                app_id: r.u32()?,
                digest: r.array::<16>()?,
            },
            ClientMessage::Resize => ClientControl::Resize {
                width: r.u32()?,
                height: r.u32()?,
                refresh_mhz: r.u32()?,
            },
            ClientMessage::PadConnected => ClientControl::PadConnected {
                pad_index: r.u8()?,
                pad_type: r.u8()?,
                capabilities: r.u16()?,
            },
            ClientMessage::PadDisconnected => ClientControl::PadDisconnected { pad_index: r.u8()? },
            ClientMessage::ListApps => ClientControl::ListApps,
            ClientMessage::RequestIdr => ClientControl::RequestIdr,
            ClientMessage::Bye => ClientControl::Bye,
        };
        Ok((message, consumed))
    }
}

impl ServerControl {
    pub const fn kind(&self) -> Option<ServerMessage> {
        Some(match self {
            ServerControl::PairChallenge { .. } => ServerMessage::PairChallenge,
            ServerControl::AppList(_) => ServerMessage::AppList,
            ServerControl::ArtChunk(_) => ServerMessage::ArtChunk,
            ServerControl::LaunchResult { .. } => ServerMessage::LaunchResult,
            ServerControl::SessionConfig(_) => ServerMessage::SessionConfig,
            ServerControl::CodecPrivate(_) => ServerMessage::CodecPrivate,
            ServerControl::CursorShape(_) => ServerMessage::CursorShape,
            ServerControl::CursorPosition { .. } => ServerMessage::CursorPosition,
            ServerControl::SecureDesktop { .. } => ServerMessage::SecureDesktop,
            ServerControl::Bye => ServerMessage::Bye,
            ServerControl::Unhandled(_) => return None,
        })
    }

    pub fn encode(&self) -> Result<Vec<u8>, ControlError> {
        let kind = self.kind().ok_or(ControlError::OutOfRange(
            "cannot encode an unhandled message",
        ))? as u8;
        let mut out = Vec::with_capacity(64);
        let mut err = Ok(());

        envelope(&mut out, kind, |b| match self {
            ServerControl::PairChallenge {
                request_id,
                server_nonce,
            } => {
                b.extend_from_slice(&request_id.to_le_bytes());
                b.extend_from_slice(server_nonce);
            }
            ServerControl::AppList(page) => {
                let count = u16::try_from(page.apps.len()).unwrap_or(u16::MAX);
                b.extend_from_slice(&page.total.to_le_bytes());
                b.extend_from_slice(&page.start.to_le_bytes());
                b.extend_from_slice(&count.to_le_bytes());
                for app in page.apps.iter().take(count as usize) {
                    b.extend_from_slice(&app.id.to_le_bytes());
                    if err.is_ok() {
                        err = put_str(b, &app.name);
                    }
                    b.push(u8::from(app.art.is_some()));
                    if let Some(art) = &app.art {
                        b.extend_from_slice(&art.digest);
                        b.extend_from_slice(&art.len.to_le_bytes());
                        b.push(art.format as u8);
                    }
                }
            }
            ServerControl::ArtChunk(c) => {
                b.extend_from_slice(&c.app_id.to_le_bytes());
                b.extend_from_slice(&c.digest);
                b.push(c.format as u8);
                b.extend_from_slice(&c.total_len.to_le_bytes());
                b.extend_from_slice(&c.offset.to_le_bytes());
                if c.data.len() > ART_CHUNK_MAX {
                    err = Err(ControlError::OutOfRange("art chunk data"));
                } else {
                    b.extend_from_slice(&(c.data.len() as u16).to_le_bytes());
                    b.extend_from_slice(&c.data);
                }
            }
            ServerControl::LaunchResult {
                app_id,
                ok,
                message,
            } => {
                b.extend_from_slice(&app_id.to_le_bytes());
                b.push(u8::from(*ok));
                // A message too long for the wire is cut, not refused: the
                // result matters more than the whole explanation.
                let mut end = message.len().min(MAX_STRING);
                while !message.is_char_boundary(end) {
                    end -= 1;
                }
                err = put_str(b, &message[..end]);
            }
            ServerControl::SessionConfig(c) => {
                b.extend_from_slice(&c.session_id.to_le_bytes());
                b.push(c.codec as u8);
                b.extend_from_slice(&c.width.to_le_bytes());
                b.extend_from_slice(&c.height.to_le_bytes());
                b.extend_from_slice(&c.fps_mhz.to_le_bytes());
                b.extend_from_slice(&c.bitrate_kbps.to_le_bytes());
                let flags = u8::from(c.hdr.is_some())
                    | u8::from(c.intra_refresh) << 1
                    | u8::from(c.ref_invalidation) << 2
                    | u8::from(c.audio.is_some()) << 3;
                b.push(flags);
                b.push(c.slices);
                b.extend_from_slice(&c.server_nonce);
                b.extend_from_slice(&c.qpc_freq_hz.to_le_bytes());
                b.extend_from_slice(&c.server_ns.to_le_bytes());
                b.extend_from_slice(&c.hello_delay_ns.to_le_bytes());
                if let Some(h) = &c.hdr {
                    put_mastering(b, h);
                }
                if let Some(a) = &c.audio {
                    b.extend_from_slice(&a.sample_rate.to_le_bytes());
                    b.push(a.channels);
                    b.extend_from_slice(&a.frame_samples.to_le_bytes());
                }
                b.extend_from_slice(&c.pointer_gain_milli.to_le_bytes());
            }
            ServerControl::CodecPrivate(c) => {
                b.push(c.codec as u8);
                match u16::try_from(c.data.len()) {
                    Ok(len) => {
                        b.extend_from_slice(&len.to_le_bytes());
                        b.extend_from_slice(&c.data);
                    }
                    Err(_) => err = Err(ControlError::OutOfRange("codec private data")),
                }
                b.extend_from_slice(&c.width.to_le_bytes());
                b.extend_from_slice(&c.height.to_le_bytes());
                b.extend_from_slice(&c.first_frame.0.to_le_bytes());
                b.push(c.color.primaries);
                b.push(c.color.transfer);
                b.push(c.color.matrix);
                b.push(u8::from(c.color.full_range) | u8::from(c.hdr.is_some()) << 1);
                b.push(c.color.bit_depth);
                if let Some(h) = &c.hdr {
                    put_mastering(b, h);
                }
            }
            ServerControl::CursorShape(c) => {
                b.extend_from_slice(&c.shape_id.to_le_bytes());
                b.extend_from_slice(&c.width.to_le_bytes());
                b.extend_from_slice(&c.height.to_le_bytes());
                b.extend_from_slice(&c.hotspot_x.to_le_bytes());
                b.extend_from_slice(&c.hotspot_y.to_le_bytes());
                b.push(c.format);
                b.extend_from_slice(&c.total_len.to_le_bytes());
                b.extend_from_slice(&c.offset.to_le_bytes());
                if c.data.len() > CURSOR_CHUNK_MAX {
                    err = Err(ControlError::OutOfRange("cursor chunk"));
                } else {
                    b.extend_from_slice(&(c.data.len() as u16).to_le_bytes());
                    b.extend_from_slice(&c.data);
                }
            }
            ServerControl::CursorPosition {
                x,
                y,
                visible,
                input_seq,
            } => {
                b.extend_from_slice(&x.to_le_bytes());
                b.extend_from_slice(&y.to_le_bytes());
                b.push(u8::from(*visible));
                b.extend_from_slice(&input_seq.to_le_bytes());
            }
            ServerControl::SecureDesktop { active } => b.push(u8::from(*active)),
            ServerControl::Bye | ServerControl::Unhandled(_) => {}
        });

        err?;
        Ok(out)
    }

    pub fn decode(buf: &[u8]) -> Result<(ServerControl, usize), ControlError> {
        let (kind, payload, consumed) = split_envelope(buf)?;
        let Some(known) = ServerMessage::from_u8(kind) else {
            return Ok((ServerControl::Unhandled(kind), consumed));
        };

        let mut r = Reader::new(payload);
        let message = match known {
            ServerMessage::PairChallenge => ServerControl::PairChallenge {
                request_id: r.u32()?,
                server_nonce: r.array::<NONCE_LEN>()?,
            },
            ServerMessage::AppList => {
                let total = r.u16()?;
                let start = r.u16()?;
                let count = r.u16()?;
                let mut apps = Vec::with_capacity(count.min(256) as usize);
                for _ in 0..count {
                    let id = r.u32()?;
                    let name = r.string()?;
                    let art = if r.u8()? & 1 != 0 {
                        Some(ArtRef {
                            digest: r.array::<16>()?,
                            len: r.u32()?,
                            format: ArtFormat::from_u8(r.u8()?)
                                .ok_or(ControlError::OutOfRange("art format"))?,
                        })
                    } else {
                        None
                    };
                    apps.push(AppListing { id, name, art });
                }
                ServerControl::AppList(AppPage { total, start, apps })
            }
            ServerMessage::ArtChunk => {
                let app_id = r.u32()?;
                let digest = r.array::<16>()?;
                let format =
                    ArtFormat::from_u8(r.u8()?).ok_or(ControlError::OutOfRange("art format"))?;
                let total_len = r.u32()?;
                let offset = r.u32()?;
                let len = r.u16()? as usize;
                if len > ART_CHUNK_MAX {
                    return Err(ControlError::OutOfRange("art chunk data"));
                }
                ServerControl::ArtChunk(ArtChunk {
                    app_id,
                    digest,
                    format,
                    total_len,
                    offset,
                    data: r.take(len)?.to_vec(),
                })
            }
            ServerMessage::LaunchResult => ServerControl::LaunchResult {
                app_id: r.u32()?,
                ok: r.u8()? != 0,
                message: r.string()?,
            },
            ServerMessage::Bye => ServerControl::Bye,
            ServerMessage::SessionConfig => {
                let session_id = r.u32()?;
                let codec =
                    StreamCodec::from_u8(r.u8()?).ok_or(ControlError::OutOfRange("codec"))?;
                let width = r.u16()?;
                let height = r.u16()?;
                let fps_mhz = r.u32()?;
                let bitrate_kbps = r.u32()?;
                let flags = r.u8()?;
                let slices = r.u8()?;
                let server_nonce = r.array::<NONCE_LEN>()?;
                let qpc_freq_hz = r.u64()?;
                let server_ns = r.u64()? as i64;
                let hello_delay_ns = r.u32()?;
                let hdr = if flags & 1 != 0 {
                    Some(read_mastering(&mut r)?)
                } else {
                    None
                };
                let audio = if flags & 8 != 0 {
                    Some(AudioParams {
                        sample_rate: r.u32()?,
                        channels: r.u8()?,
                        frame_samples: r.u16()?,
                    })
                } else {
                    None
                };
                let pointer_gain_milli = r.u16()?;
                ServerControl::SessionConfig(SessionConfig {
                    session_id,
                    codec,
                    width,
                    height,
                    fps_mhz,
                    bitrate_kbps,
                    hdr,
                    audio,
                    intra_refresh: flags & 2 != 0,
                    ref_invalidation: flags & 4 != 0,
                    slices,
                    server_nonce,
                    qpc_freq_hz,
                    server_ns,
                    hello_delay_ns,
                    pointer_gain_milli,
                })
            }
            ServerMessage::CodecPrivate => {
                let codec =
                    StreamCodec::from_u8(r.u8()?).ok_or(ControlError::OutOfRange("codec"))?;
                let len = r.u16()? as usize;
                let data = r.take(len)?.to_vec();
                let width = r.u16()?;
                let height = r.u16()?;
                let first_frame = Seq16(r.u16()?);
                let primaries = r.u8()?;
                let transfer = r.u8()?;
                let matrix = r.u8()?;
                let flags = r.u8()?;
                let bit_depth = r.u8()?;
                let hdr = if flags & 2 != 0 {
                    Some(read_mastering(&mut r)?)
                } else {
                    None
                };
                ServerControl::CodecPrivate(CodecPrivate {
                    codec,
                    data,
                    width,
                    height,
                    first_frame,
                    color: ColorInfo {
                        primaries,
                        transfer,
                        matrix,
                        full_range: flags & 1 != 0,
                        bit_depth,
                    },
                    hdr,
                })
            }
            ServerMessage::CursorShape => {
                let shape_id = r.u32()?;
                let width = r.u16()?;
                let height = r.u16()?;
                let hotspot_x = r.u16()?;
                let hotspot_y = r.u16()?;
                let format = r.u8()?;
                let total_len = r.u32()?;
                let offset = r.u32()?;
                let len = r.u16()? as usize;
                if len > CURSOR_CHUNK_MAX {
                    return Err(ControlError::OutOfRange("cursor chunk"));
                }
                ServerControl::CursorShape(CursorChunk {
                    shape_id,
                    width,
                    height,
                    hotspot_x,
                    hotspot_y,
                    format,
                    total_len,
                    offset,
                    data: r.take(len)?.to_vec(),
                })
            }
            ServerMessage::CursorPosition => ServerControl::CursorPosition {
                x: r.u16()?,
                y: r.u16()?,
                visible: r.u8()? != 0,
                input_seq: r.u32()?,
            },
            ServerMessage::SecureDesktop => ServerControl::SecureDesktop {
                active: r.u8()? != 0,
            },
        };
        Ok((message, consumed))
    }
}

/// Split the envelope, returning the kind, the payload, and total bytes used.
fn split_envelope(buf: &[u8]) -> Result<(u8, &[u8], usize), ControlError> {
    if buf.len() < ENVELOPE_LEN {
        return Err(ControlError::Truncated);
    }
    let kind = buf[0];
    let len = u16::from_le_bytes([buf[1], buf[2]]) as usize;
    let end = ENVELOPE_LEN + len;
    if buf.len() < end {
        return Err(ControlError::BadLength(len));
    }
    Ok((kind, &buf[ENVELOPE_LEN..end], end))
}

/// Bounds-checked forward reader, so every payload is not its own set of index
/// arithmetic waiting to panic on a hostile packet.
/// The 28-byte ST 2086 block, as `SessionConfig` and `CodecPrivate` both carry
/// it: R, G, B, white `[x, y]` as u16, then max and min luminance as u32, then
/// MaxCLL and MaxFALL as u16. One writer and one reader, so the two messages
/// cannot drift apart.
fn put_mastering(b: &mut Vec<u8>, h: &HdrMastering) {
    for [x, y] in h.primaries.iter().chain(core::iter::once(&h.white)) {
        b.extend_from_slice(&x.to_le_bytes());
        b.extend_from_slice(&y.to_le_bytes());
    }
    b.extend_from_slice(&h.max_luminance.to_le_bytes());
    b.extend_from_slice(&h.min_luminance.to_le_bytes());
    b.extend_from_slice(&h.max_cll.to_le_bytes());
    b.extend_from_slice(&h.max_fall.to_le_bytes());
}

fn read_mastering(r: &mut Reader<'_>) -> Result<HdrMastering, ControlError> {
    let mut xy = [[0u16; 2]; 4];
    for pair in &mut xy {
        pair[0] = r.u16()?;
        pair[1] = r.u16()?;
    }
    Ok(HdrMastering {
        primaries: [xy[0], xy[1], xy[2]],
        white: xy[3],
        max_luminance: r.u32()?,
        min_luminance: r.u32()?,
        max_cll: r.u16()?,
        max_fall: r.u16()?,
    })
}

struct Reader<'a> {
    buf: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Reader<'a> {
        Reader { buf, at: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], ControlError> {
        let end = self.at.checked_add(n).ok_or(ControlError::Truncated)?;
        let slice = self.buf.get(self.at..end).ok_or(ControlError::Truncated)?;
        self.at = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, ControlError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, ControlError> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32, ControlError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self) -> Result<u64, ControlError> {
        let b = self.take(8)?;
        Ok(u64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], ControlError> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.take(N)?);
        Ok(out)
    }

    fn string(&mut self) -> Result<String, ControlError> {
        let len = self.u8()? as usize;
        let bytes = self.take(len)?;
        core::str::from_utf8(bytes)
            .map(str::to_owned)
            .map_err(|_| ControlError::BadUtf8)
    }
}

#[cfg(test)]
mod tests;
