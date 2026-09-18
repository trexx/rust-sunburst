// SPDX-License-Identifier: GPL-2.0-or-later

//! Client → server NACK (packet type 4).
//!
//! The header's `frame_id` names the frame; the body lists the packet indices
//! the client is still missing. The server answers a non-empty list by
//! **retransmitting** those packets from its cache — at LAN round-trip times the
//! resend lands well inside the jitter-buffer deadline, so the frame still
//! displays on time and the encoder is never involved.
//!
//! An **empty** list means the client has given up on the frame: it was stepped
//! over by the jitter buffer or evicted before completing. That is the signal
//! for reference invalidation — the server marks the frame and everything
//! encoded since as unusable for prediction, and the next frame references an
//! older, good one instead of costing a keyframe. See PROTOCOL.md.
//!
//! Authenticated, like every packet that can make the server do something: an
//! unauthenticated abandon would let anyone on the network force IDRs at will.
//! Freshness comes from the header's `frame_id` — a NACK for a frame older than
//! the cache is simply unanswerable — so there is no sequence number.

use super::auth::MAC_LEN;
use super::header::MAX_PAYLOAD;

/// Most indices one NACK carries: the payload less the count and the MAC.
pub const NACK_MAX_MISSING: usize = (MAX_PAYLOAD - 2 - MAC_LEN) / 2;

/// A decoded NACK body, borrowing the packet it came from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Nack<'a> {
    count: u16,
    /// `count` little-endian `u16`s.
    indices: &'a [u8],
}

impl<'a> Nack<'a> {
    /// Encode a request for `missing` (empty = abandon) into `out`, returning
    /// the body length. `None` if there are too many indices or `out` is short.
    pub fn encode(missing: &[u16], out: &mut [u8]) -> Option<usize> {
        if missing.len() > NACK_MAX_MISSING {
            return None;
        }
        let len = 2 + missing.len() * 2;
        if out.len() < len {
            return None;
        }
        out[..2].copy_from_slice(&(missing.len() as u16).to_le_bytes());
        for (i, idx) in missing.iter().enumerate() {
            out[2 + i * 2..4 + i * 2].copy_from_slice(&idx.to_le_bytes());
        }
        Some(len)
    }

    /// Decode a body (MAC already stripped). Refuses a count the buffer cannot
    /// hold rather than reading short.
    pub fn decode(body: &'a [u8]) -> Option<Nack<'a>> {
        if body.len() < 2 {
            return None;
        }
        let count = u16::from_le_bytes([body[0], body[1]]);
        let need = count as usize * 2;
        let indices = body.get(2..2 + need)?;
        Some(Nack { count, indices })
    }

    pub const fn count(&self) -> u16 {
        self.count
    }

    /// The client has given up on this frame: invalidate rather than resend.
    pub const fn is_abandon(&self) -> bool {
        self.count == 0
    }

    /// The missing packet indices, in the order the client listed them.
    pub fn missing(&self) -> impl Iterator<Item = u16> + 'a {
        self.indices
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_list_round_trips() {
        let missing = [3u16, 7, 1000, 65535];
        let mut buf = [0u8; 64];
        let n = Nack::encode(&missing, &mut buf).unwrap();
        assert_eq!(n, 2 + 8);
        let nack = Nack::decode(&buf[..n]).unwrap();
        assert_eq!(nack.count(), 4);
        assert!(!nack.is_abandon());
        assert_eq!(nack.missing().collect::<Vec<_>>(), missing);
    }

    #[test]
    fn an_empty_list_is_an_abandon() {
        let mut buf = [0u8; 8];
        let n = Nack::encode(&[], &mut buf).unwrap();
        assert_eq!(n, 2);
        let nack = Nack::decode(&buf[..n]).unwrap();
        assert!(nack.is_abandon());
        assert_eq!(nack.missing().count(), 0);
    }

    #[test]
    fn the_largest_list_fits_one_packet_and_one_more_does_not() {
        let max: Vec<u16> = (0..NACK_MAX_MISSING as u16).collect();
        let mut buf = [0u8; MAX_PAYLOAD];
        let n = Nack::encode(&max, &mut buf).unwrap();
        assert!(n + MAC_LEN <= MAX_PAYLOAD, "with its MAC it still fits");
        assert_eq!(
            Nack::decode(&buf[..n]).unwrap().count() as usize,
            NACK_MAX_MISSING
        );

        let too_many: Vec<u16> = (0..=NACK_MAX_MISSING as u16).collect();
        assert_eq!(Nack::encode(&too_many, &mut buf), None);
    }

    #[test]
    fn a_count_the_body_cannot_hold_is_refused() {
        // Claims two indices, carries one: reading short would index past the
        // packet on a hostile sender.
        let body = [2u8, 0, 5, 0];
        assert_eq!(Nack::decode(&body), None);
        assert_eq!(Nack::decode(&[]), None);
        assert_eq!(Nack::decode(&[0]), None);
    }

    #[test]
    fn a_short_output_buffer_is_refused() {
        let mut buf = [0u8; 3];
        assert_eq!(Nack::encode(&[1], &mut buf), None);
    }
}
