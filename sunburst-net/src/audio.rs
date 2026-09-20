// SPDX-License-Identifier: GPL-2.0-or-later

//! Audio on the wire.
//!
//! An audio packet is far simpler than a video frame: one Opus packet fits in a
//! single datagram under [`MAX_PAYLOAD`], so there is no fragmentation, no
//! terminator, and no reassembler. It is just the common 12-byte [`Header`]
//! followed by the Opus payload, which is why this whole module is two pure
//! functions rather than the `Packetizer`/`Reassembler` pair video needs.
//!
//! Field conventions on the header:
//! - `frame_id` carries the **audio** sequence number, its own monotonic
//!   counter unrelated to video frame ids.
//! - `qpc_timestamp` is the low 32 bits of the capture-time tick counter of the
//!   frame's first sample, in the same clock domain video uses, so the client
//!   can line audio up against video with the `qpc_freq_hz` it already has.
//! - `pkt_idx` is 0 and `pkt_count` is 1: one packet is the whole frame.
//! - `flags` is [`Flags::EMPTY`]; audio defines no flag bits today.
//!
//! Audio is unauthenticated (LAN-only, like video), so there is no MAC to add
//! here and none to verify on receipt.

use sunburst_core::proto::{Flags, HEADER_LEN, Header, MAX_PAYLOAD, PacketType, Seq16};

/// A fully-formed audio packet is at most the header plus one MTU-sized payload.
pub const MAX_AUDIO_PACKET: usize = HEADER_LEN + MAX_PAYLOAD;

/// Write one audio packet (header + Opus payload) into `buf`, returning the
/// number of bytes written.
///
/// Returns `None` if the Opus payload is larger than [`MAX_PAYLOAD`] or `buf`
/// cannot hold the whole packet — the caller is expected to size `buf` at
/// [`MAX_AUDIO_PACKET`], so `None` means a mis-sized Opus frame, not a routine
/// short buffer.
pub fn encode_audio_packet(seq: Seq16, qpc: u32, opus: &[u8], buf: &mut [u8]) -> Option<usize> {
    let total = HEADER_LEN + opus.len();
    if opus.len() > MAX_PAYLOAD || buf.len() < total {
        return None;
    }
    let header = Header {
        packet_type: PacketType::Audio,
        flags: Flags::EMPTY,
        frame_id: seq,
        qpc_timestamp: qpc,
        pkt_idx: 0,
        pkt_count: 1,
    };
    let head: &mut [u8; HEADER_LEN] = (&mut buf[..HEADER_LEN]).try_into().unwrap();
    header.encode(head);
    buf[HEADER_LEN..total].copy_from_slice(opus);
    Some(total)
}

/// Decode one audio datagram into its header and Opus payload slice.
///
/// Returns `None` unless the datagram has a well-formed header whose type is
/// [`PacketType::Audio`]. The payload may be empty (a silence frame can encode
/// very small, but never negative), so an empty slice is a valid result.
pub fn parse_audio_packet(datagram: &[u8]) -> Option<(Header, &[u8])> {
    let header = Header::decode(datagram)?;
    if header.packet_type != PacketType::Audio {
        return None;
    }
    Some((header, &datagram[HEADER_LEN..]))
}

/// Write one **client→server** audio-in packet — a paired pad's headset mic —
/// into `buf`. Identical framing to [`encode_audio_packet`], with two
/// differences: the type is [`PacketType::AudioIn`], and the header's `pkt_idx`
/// carries the **pad index** (the stream id), since one session can carry more
/// than one headset. `pkt_count` stays 1: one Opus packet is the whole frame.
pub fn encode_audio_in_packet(
    pad_index: u8,
    seq: Seq16,
    qpc: u32,
    opus: &[u8],
    buf: &mut [u8],
) -> Option<usize> {
    let total = HEADER_LEN + opus.len();
    if opus.len() > MAX_PAYLOAD || buf.len() < total {
        return None;
    }
    let header = Header {
        packet_type: PacketType::AudioIn,
        flags: Flags::EMPTY,
        frame_id: seq,
        qpc_timestamp: qpc,
        pkt_idx: pad_index as u16,
        pkt_count: 1,
    };
    let head: &mut [u8; HEADER_LEN] = (&mut buf[..HEADER_LEN]).try_into().unwrap();
    header.encode(head);
    buf[HEADER_LEN..total].copy_from_slice(opus);
    Some(total)
}

/// Decode one audio-in datagram into `(pad_index, header, Opus payload)`.
///
/// Returns `None` unless the datagram is a well-formed [`PacketType::AudioIn`]
/// packet. As with [`parse_audio_packet`] an empty payload is valid.
pub fn parse_audio_in_packet(datagram: &[u8]) -> Option<(u8, Header, &[u8])> {
    let header = Header::decode(datagram)?;
    if header.packet_type != PacketType::AudioIn {
        return None;
    }
    let pad_index = header.pkt_idx as u8;
    Some((pad_index, header, &datagram[HEADER_LEN..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_a_typical_frame() {
        let opus = [0x11u8; 320]; // ~a 5 ms Opus packet
        let mut buf = [0u8; MAX_AUDIO_PACKET];
        let n = encode_audio_packet(Seq16(1234), 0xABCD_1234, &opus, &mut buf).expect("encode");
        assert_eq!(n, HEADER_LEN + opus.len());

        let (header, payload) = parse_audio_packet(&buf[..n]).expect("parse");
        assert_eq!(header.packet_type, PacketType::Audio);
        assert_eq!(header.frame_id, Seq16(1234));
        assert_eq!(header.qpc_timestamp, 0xABCD_1234);
        assert_eq!(header.pkt_idx, 0);
        assert_eq!(header.pkt_count, 1);
        assert_eq!(header.flags, Flags::EMPTY);
        assert_eq!(payload, &opus);
    }

    #[test]
    fn audio_in_round_trips_with_its_pad_index() {
        let opus = [0x22u8; 160]; // ~a 5 ms mono headset frame
        let mut buf = [0u8; MAX_AUDIO_PACKET];
        let n = encode_audio_in_packet(1, Seq16(77), 0x0BAD_F00D, &opus, &mut buf).expect("encode");
        assert_eq!(n, HEADER_LEN + opus.len());

        let (pad, header, payload) = parse_audio_in_packet(&buf[..n]).expect("parse");
        assert_eq!(pad, 1, "the pad index rides in pkt_idx");
        assert_eq!(header.packet_type, PacketType::AudioIn);
        assert_eq!(header.frame_id, Seq16(77));
        assert_eq!(payload, &opus);

        // The two directions must not be confused for each other.
        assert!(parse_audio_packet(&buf[..n]).is_none());
        let mut out = [0u8; MAX_AUDIO_PACKET];
        let m = encode_audio_packet(Seq16(0), 0, &opus, &mut out).expect("encode audio");
        assert!(parse_audio_in_packet(&out[..m]).is_none());
    }

    #[test]
    fn a_zero_length_payload_is_valid() {
        let mut buf = [0u8; MAX_AUDIO_PACKET];
        let n = encode_audio_packet(Seq16(0), 0, &[], &mut buf).expect("encode");
        assert_eq!(n, HEADER_LEN);
        let (_, payload) = parse_audio_packet(&buf[..n]).expect("parse");
        assert!(payload.is_empty());
    }

    #[test]
    fn an_oversized_opus_frame_is_refused() {
        let opus = [0u8; MAX_PAYLOAD + 1];
        let mut buf = [0u8; MAX_PAYLOAD * 2];
        assert_eq!(encode_audio_packet(Seq16(0), 0, &opus, &mut buf), None);
    }

    #[test]
    fn a_short_output_buffer_is_refused() {
        let opus = [0u8; 100];
        let mut buf = [0u8; HEADER_LEN + 50];
        assert_eq!(encode_audio_packet(Seq16(0), 0, &opus, &mut buf), None);
    }

    #[test]
    fn a_non_audio_packet_is_rejected() {
        // A video header must not parse as audio.
        let header = Header {
            packet_type: PacketType::Video,
            flags: Flags::EMPTY,
            frame_id: Seq16(1),
            qpc_timestamp: 0,
            pkt_idx: 0,
            pkt_count: 1,
        };
        let mut buf = [0u8; HEADER_LEN];
        header.encode(&mut buf);
        assert!(parse_audio_packet(&buf).is_none());
    }

    #[test]
    fn a_short_datagram_is_rejected() {
        assert!(parse_audio_packet(&[0u8; HEADER_LEN - 1]).is_none());
    }
}
