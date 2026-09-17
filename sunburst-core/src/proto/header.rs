// SPDX-License-Identifier: GPL-2.0-or-later

//! The 12-byte common header every packet carries.
//!
//! Encoded and decoded through borrowed byte slices: no serde, no allocation,
//! and every integer written little-endian explicitly rather than by transmuting
//! a struct, because the client is `armeabi-v7a` and a layout assumption that
//! happens to hold on x86-64 is not a wire format.

use super::seq::Seq16;

/// Bytes in the common header.
pub const HEADER_LEN: usize = 12;

/// Largest payload after the header, to stay under path MTU on a 1500-byte link.
pub const MAX_PAYLOAD: usize = 1200;

/// Packet discriminant, from PROTOCOL.md.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum PacketType {
    Video = 0,
    Audio = 1,
    Input = 2,
    Control = 3,
    Nack = 4,
    Feedback = 5,
    Rumble = 6,
    /// Server→client rich pad output — motors, adaptive triggers, LED (see
    /// [`super::padoutput`]). Unreliable, latest-wins, like [`PacketType::Rumble`].
    PadOutput = 7,
}

impl PacketType {
    pub const fn from_u8(v: u8) -> Option<PacketType> {
        Some(match v {
            0 => PacketType::Video,
            1 => PacketType::Audio,
            2 => PacketType::Input,
            3 => PacketType::Control,
            4 => PacketType::Nack,
            5 => PacketType::Feedback,
            6 => PacketType::Rumble,
            7 => PacketType::PadOutput,
            _ => return None,
        })
    }

    /// Whether this type must carry a MAC.
    ///
    /// Input and control are the packets that can make the server act, so they
    /// are authenticated; rumble and pad output are included because they drive
    /// hardware on the client. Video and audio are not, per CLAUDE.md — LAN-only,
    /// and the threat model does not justify the key management.
    pub const fn is_authenticated(self) -> bool {
        matches!(
            self,
            PacketType::Input | PacketType::Control | PacketType::Rumble | PacketType::PadOutput
        )
    }
}

/// Header flag bits.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Flags(pub u8);

impl Flags {
    pub const EMPTY: Flags = Flags(0);
    /// Keyframe or IDR.
    pub const KEYFRAME: Flags = Flags(1 << 0);
    /// Last packet of this frame.
    pub const LAST_PACKET: Flags = Flags(1 << 1);
    /// First fragment of a NAL (HEVC) or OBU (AV1).
    pub const UNIT_BOUNDARY: Flags = Flags(1 << 2);
    /// Intra-refresh is active for this frame.
    pub const INTRA_REFRESH: Flags = Flags(1 << 3);

    pub const fn contains(self, other: Flags) -> bool {
        self.0 & other.0 == other.0
    }

    #[must_use]
    pub const fn with(self, other: Flags) -> Flags {
        Flags(self.0 | other.0)
    }
}

impl core::ops::BitOr for Flags {
    type Output = Flags;
    fn bitor(self, rhs: Flags) -> Flags {
        Flags(self.0 | rhs.0)
    }
}

/// The common header.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Header {
    pub packet_type: PacketType,
    pub flags: Flags,
    pub frame_id: Seq16,
    /// Low 32 bits of the *sender's* capture-time tick counter.
    ///
    /// There is no shared epoch: the receiver cannot subtract this from its own
    /// clock without `clock_offset_ns` from the handshake. It is a correlation
    /// tag, not a timestamp both ends can reason about.
    pub qpc_timestamp: u32,
    pub pkt_idx: u16,
    pub pkt_count: u16,
}

impl Header {
    /// Write into the first [`HEADER_LEN`] bytes of `buf`.
    pub fn encode(&self, buf: &mut [u8; HEADER_LEN]) {
        buf[0] = self.packet_type as u8;
        buf[1] = self.flags.0;
        buf[2..4].copy_from_slice(&self.frame_id.0.to_le_bytes());
        buf[4..8].copy_from_slice(&self.qpc_timestamp.to_le_bytes());
        buf[8..10].copy_from_slice(&self.pkt_idx.to_le_bytes());
        buf[10..12].copy_from_slice(&self.pkt_count.to_le_bytes());
    }

    /// Read from the front of `buf`. Returns `None` on a short buffer or an
    /// unknown packet type.
    pub fn decode(buf: &[u8]) -> Option<Header> {
        if buf.len() < HEADER_LEN {
            return None;
        }
        Some(Header {
            packet_type: PacketType::from_u8(buf[0])?,
            flags: Flags(buf[1]),
            frame_id: Seq16(u16::from_le_bytes([buf[2], buf[3]])),
            qpc_timestamp: u32::from_le_bytes([buf[4], buf[5], buf[6], buf[7]]),
            pkt_idx: u16::from_le_bytes([buf[8], buf[9]]),
            pkt_count: u16::from_le_bytes([buf[10], buf[11]]),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Header {
        Header {
            packet_type: PacketType::Video,
            flags: Flags::KEYFRAME | Flags::UNIT_BOUNDARY,
            frame_id: Seq16(0xBEEF),
            qpc_timestamp: 0xDEAD_1234,
            pkt_idx: 7,
            pkt_count: 42,
        }
    }

    #[test]
    fn round_trips() {
        let mut buf = [0u8; HEADER_LEN];
        sample().encode(&mut buf);
        assert_eq!(Header::decode(&buf), Some(sample()));
    }

    #[test]
    fn byte_layout_matches_the_specification() {
        // Pinned against PROTOCOL.md byte by byte. If this changes, the client
        // and server disagree silently, so it is worth asserting rather than
        // trusting the round trip above — which would pass just as happily with
        // both sides wrong in the same way.
        let mut buf = [0u8; HEADER_LEN];
        sample().encode(&mut buf);
        assert_eq!(
            buf,
            [
                0x00, // type = Video
                0x05, // flags = keyframe | unit-boundary
                0xEF, 0xBE, // frame_id, little-endian
                0x34, 0x12, 0xAD, 0xDE, // qpc_timestamp, little-endian
                0x07, 0x00, // pkt_idx
                0x2A, 0x00, // pkt_count
            ]
        );
    }

    #[test]
    fn header_is_twelve_bytes() {
        assert_eq!(HEADER_LEN, 12);
    }

    #[test]
    fn rejects_a_short_buffer() {
        let buf = [0u8; HEADER_LEN - 1];
        assert_eq!(Header::decode(&buf), None);
    }

    #[test]
    fn rejects_an_unknown_packet_type() {
        let mut buf = [0u8; HEADER_LEN];
        buf[0] = 8; // one past PadOutput
        assert_eq!(Header::decode(&buf), None);
        assert_eq!(PacketType::from_u8(255), None);
    }

    #[test]
    fn decode_ignores_trailing_payload() {
        let mut buf = [0u8; HEADER_LEN + MAX_PAYLOAD];
        sample().encode((&mut buf[..HEADER_LEN]).try_into().unwrap());
        assert_eq!(Header::decode(&buf), Some(sample()));
    }

    #[test]
    fn reserved_flag_bits_survive_a_round_trip() {
        // Bits 4-7 are reserved. A future sender setting one must not make an
        // older receiver reject the packet or silently lose the bit.
        let mut h = sample();
        h.flags = Flags(0xF0);
        let mut buf = [0u8; HEADER_LEN];
        h.encode(&mut buf);
        assert_eq!(Header::decode(&buf).unwrap().flags, Flags(0xF0));
    }

    #[test]
    fn only_actionable_packet_types_are_authenticated() {
        // Video and audio are unauthenticated by design; anything that makes the
        // far end act is not.
        assert!(PacketType::Input.is_authenticated());
        assert!(PacketType::Control.is_authenticated());
        assert!(PacketType::Rumble.is_authenticated());
        assert!(PacketType::PadOutput.is_authenticated());
        assert!(!PacketType::Video.is_authenticated());
        assert!(!PacketType::Audio.is_authenticated());
    }

    #[test]
    fn flags_compose_and_test() {
        let f = Flags::KEYFRAME | Flags::LAST_PACKET;
        assert!(f.contains(Flags::KEYFRAME));
        assert!(f.contains(Flags::LAST_PACKET));
        assert!(!f.contains(Flags::INTRA_REFRESH));
        assert!(f.contains(Flags::EMPTY), "every set contains the empty set");
    }
}
