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
//! # What is not implemented yet
//!
//! `SessionConfig`, `CodecPrivate`, `CursorShape`, `CursorPosition` and
//! `SecureDesktop` carry no payload here. Their fields depend on decisions
//! Phases 3 and 5 have not made — `CodecPrivate` in particular is an av1C record
//! whose construction PROTOCOL.md flags as a silent-failure point — and
//! inventing their encodings now would mean inventing them twice. Their
//! discriminants are reserved below so the numbering cannot shift under them,
//! and they decode to [`ClientControl::Unhandled`] / [`ServerControl::Unhandled`]
//! in the meantime.

use super::pairing::{NONCE_LEN, TAG_LEN};

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
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AppListing {
    pub id: u32,
    pub name: String,
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
    AppList(Vec<AppListing>),
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
            }
            ClientControl::Quirks(q) => {
                b.push(q.flags());
                b.extend_from_slice(&q.max_bitrate_hint.to_le_bytes());
            }
            ClientControl::LaunchApp { app_id } => b.extend_from_slice(&app_id.to_le_bytes()),
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
            }),
            ClientMessage::DecoderQuirks => {
                ClientControl::Quirks(DecoderQuirks::from_flags(r.u8()?, r.u32()?))
            }
            ClientMessage::LaunchApp => ClientControl::LaunchApp { app_id: r.u32()? },
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
            ServerControl::AppList(apps) => {
                let count = u16::try_from(apps.len()).unwrap_or(u16::MAX);
                b.extend_from_slice(&count.to_le_bytes());
                for app in apps.iter().take(count as usize) {
                    b.extend_from_slice(&app.id.to_le_bytes());
                    if err.is_ok() {
                        err = put_str(b, &app.name);
                    }
                }
            }
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
                let count = r.u16()?;
                let mut apps = Vec::with_capacity(count as usize);
                for _ in 0..count {
                    apps.push(AppListing {
                        id: r.u32()?,
                        name: r.string()?,
                    });
                }
                ServerControl::AppList(apps)
            }
            ServerMessage::Bye => ServerControl::Bye,
            // Reserved: known to exist, not decoded until its phase.
            ServerMessage::SessionConfig
            | ServerMessage::CodecPrivate
            | ServerMessage::CursorShape
            | ServerMessage::CursorPosition
            | ServerMessage::SecureDesktop => ServerControl::Unhandled(kind),
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
