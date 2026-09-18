// SPDX-License-Identifier: GPL-2.0-or-later

//! Client → server feedback (packet type 5), every 100 ms.
//!
//! Drives rate control. The load-bearing field is `owd_gradient`: the slope of
//! one-way delay over the last window, in microseconds per second. A queue
//! forming somewhere on the path shows up here as a positive slope *before* any
//! packet is dropped, which is why the controller keys on it rather than on
//! loss. The gradient needs no shared clock epoch — a constant offset between
//! the two machines' clocks differentiates to zero.
//!
//! Authenticated: a forged feedback could drive the bitrate to the floor.

use super::header::MAX_PAYLOAD;

/// Encoded body length, before the MAC.
pub const FEEDBACK_BODY_LEN: usize = 4 + 4 + 4 + 2 + 2 + 4;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Feedback {
    /// Client clock, nanoseconds, low 32 bits — an ordering tag, not an epoch.
    pub recv_timestamp: u32,
    pub frames_received: u32,
    pub frames_dropped: u32,
    pub jitter_buffer_ms: u16,
    pub decode_p99_us: u16,
    /// One-way delay gradient, µs/s. Positive means queues are building.
    pub owd_gradient: i32,
}

impl Feedback {
    pub fn encode(&self, out: &mut [u8]) -> Option<usize> {
        if out.len() < FEEDBACK_BODY_LEN {
            return None;
        }
        out[0..4].copy_from_slice(&self.recv_timestamp.to_le_bytes());
        out[4..8].copy_from_slice(&self.frames_received.to_le_bytes());
        out[8..12].copy_from_slice(&self.frames_dropped.to_le_bytes());
        out[12..14].copy_from_slice(&self.jitter_buffer_ms.to_le_bytes());
        out[14..16].copy_from_slice(&self.decode_p99_us.to_le_bytes());
        out[16..20].copy_from_slice(&self.owd_gradient.to_le_bytes());
        Some(FEEDBACK_BODY_LEN)
    }

    /// Decode a body (MAC already stripped).
    pub fn decode(body: &[u8]) -> Option<Feedback> {
        if body.len() < FEEDBACK_BODY_LEN {
            return None;
        }
        let u32_at =
            |i: usize| u32::from_le_bytes([body[i], body[i + 1], body[i + 2], body[i + 3]]);
        let u16_at = |i: usize| u16::from_le_bytes([body[i], body[i + 1]]);
        Some(Feedback {
            recv_timestamp: u32_at(0),
            frames_received: u32_at(4),
            frames_dropped: u32_at(8),
            jitter_buffer_ms: u16_at(12),
            decode_p99_us: u16_at(14),
            owd_gradient: u32_at(16) as i32,
        })
    }
}

const _: () = assert!(FEEDBACK_BODY_LEN + super::auth::MAC_LEN <= MAX_PAYLOAD);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_including_a_negative_gradient() {
        let fb = Feedback {
            recv_timestamp: 0xDEAD_BEEF,
            frames_received: 3600,
            frames_dropped: 2,
            jitter_buffer_ms: 6,
            decode_p99_us: 11_500,
            owd_gradient: -4_321,
        };
        let mut buf = [0u8; FEEDBACK_BODY_LEN];
        assert_eq!(fb.encode(&mut buf), Some(FEEDBACK_BODY_LEN));
        assert_eq!(Feedback::decode(&buf), Some(fb));
    }

    #[test]
    fn a_short_body_is_refused() {
        for len in 0..FEEDBACK_BODY_LEN {
            assert_eq!(Feedback::decode(&[0u8; FEEDBACK_BODY_LEN][..len]), None);
        }
        let mut short = [0u8; FEEDBACK_BODY_LEN - 1];
        assert_eq!(Feedback::default().encode(&mut short), None);
    }
}
