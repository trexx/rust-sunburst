// SPDX-License-Identifier: GPL-2.0-or-later

//! Control-channel message discriminants.
//!
//! The reliable channel is control plane, so allocation is permitted here and
//! only here.
//!
//! **Discriminants only for now.** The payloads are pinned in PROTOCOL.md but
//! several of them — `Hello`'s capability set, `CodecPrivate`'s av1C record,
//! `CursorShape`'s bitmap — depend on decisions that belong to the phases that
//! consume them, and inventing an encoding now would mean inventing it twice.
//! The wire values are fixed here so both ends agree on the envelope while the
//! contents are still being settled.

/// Client → server control messages.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum ClientMessage {
    /// Capabilities, ABI, display info, `client_nonce`, `clock_offset_ns`.
    Hello = 0,
    /// The `DecoderQuirks` struct; the server adapts its encoder configuration.
    DecoderQuirks = 1,
    /// Last resort. Prefer NACK and reference invalidation — an IDR is a
    /// bitrate spike and a visible hitch.
    RequestIdr = 2,
    Resize = 3,
    /// A pad appeared. The server plugs a ViGEm target for it.
    PadConnected = 4,
    /// A pad went away. The server unplugs it.
    PadDisconnected = 5,
    Bye = 6,
}

/// Server → client control messages.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum ServerMessage {
    /// Codec, resolution, fps, bitrate, HDR metadata, `server_nonce`,
    /// `clock_offset_ns`.
    SessionConfig = 0,
    /// VPS/SPS/PPS, or the av1C record.
    ///
    /// The client cannot configure MediaCodec without this, and for AV1 it is
    /// the known silent-failure point: a wrong av1C configures cleanly and
    /// outputs nothing at all.
    CodecPrivate = 1,
    /// Bitmap and hotspot, for client-side cursor rendering.
    CursorShape = 2,
    /// Sent on absolute-mode changes only.
    CursorPosition = 3,
    /// Capture is unavailable — UAC, lock screen. The client shows a
    /// placeholder; never a frozen frame.
    SecureDesktop = 4,
    Bye = 5,
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
            _ => return None,
        })
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
            _ => return None,
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_discriminants_round_trip() {
        for (v, m) in [
            (0, ClientMessage::Hello),
            (1, ClientMessage::DecoderQuirks),
            (2, ClientMessage::RequestIdr),
            (3, ClientMessage::Resize),
            (4, ClientMessage::PadConnected),
            (5, ClientMessage::PadDisconnected),
            (6, ClientMessage::Bye),
        ] {
            assert_eq!(m as u8, v);
            assert_eq!(ClientMessage::from_u8(v), Some(m));
        }
        assert_eq!(ClientMessage::from_u8(7), None);
    }

    #[test]
    fn server_discriminants_round_trip() {
        for (v, m) in [
            (0, ServerMessage::SessionConfig),
            (1, ServerMessage::CodecPrivate),
            (2, ServerMessage::CursorShape),
            (3, ServerMessage::CursorPosition),
            (4, ServerMessage::SecureDesktop),
            (5, ServerMessage::Bye),
        ] {
            assert_eq!(m as u8, v);
            assert_eq!(ServerMessage::from_u8(v), Some(m));
        }
        assert_eq!(ServerMessage::from_u8(6), None);
    }

    #[test]
    fn unknown_quirks_default_to_the_conservative_decoder() {
        // An unknown device must not be assumed capable. Corruption on first
        // contact is a far worse failure than a few milliseconds of latency.
        let q = DecoderQuirks::default();
        assert!(!q.ref_invalidation);
        assert!(!q.intra_refresh);
        assert!(!q.slice_output);
        assert!(q.needs_annexb_startcodes);
    }
}
