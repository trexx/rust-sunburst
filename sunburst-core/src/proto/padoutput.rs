// SPDX-License-Identifier: GPL-2.0-or-later

//! Server → client rich pad output.
//!
//! A native pad's output report sets more than a motor level: a DualSense frame
//! carries both rumble motors, the two adaptive-trigger effects, the lightbar
//! colour and the player LEDs at once. The server decodes the game's output report
//! (via the HIDMaestro codec) and ships the union here, so the client can apply
//! whatever its hardware supports.
//!
//! Carried **unreliably, latest-wins**, for the same reason as [`super::rumble`]: a
//! superseded effect is worthless, so `seq` lets the client drop a reordered stale
//! frame rather than retransmitting one the game has moved past. Simple X360-style
//! pads keep using [`super::rumble`] (motor-only); this is the carrier for rich
//! pads, and the two are never sent for the same controller.
//!
//! The adaptive-trigger effects are carried as **opaque length-prefixed blobs** —
//! the server does not model their semantics and the client forwards or interprets
//! them per family; a DualSense effect is 11 bytes.

use super::input::MAX_PADS;

/// Largest adaptive-trigger effect blob (DualSense: 11 bytes).
pub const MAX_TRIGGER_EFFECT: usize = 11;

/// Largest encoded body: the fixed head plus two full trigger blobs.
pub const PAD_OUTPUT_MAX_BODY: usize = 11 + 2 * (1 + MAX_TRIGGER_EFFECT);

/// Flag bits.
pub mod flags {
    /// The pad's mic-mute LED should be lit.
    pub const MIC_MUTED: u8 = 1 << 0;
}

/// One adaptive-trigger effect: an opaque, family-specific blob.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TriggerEffect {
    pub len: u8,
    pub data: [u8; MAX_TRIGGER_EFFECT],
}

impl Default for TriggerEffect {
    fn default() -> Self {
        TriggerEffect {
            len: 0,
            data: [0; MAX_TRIGGER_EFFECT],
        }
    }
}

impl TriggerEffect {
    /// Build from a slice, truncating to [`MAX_TRIGGER_EFFECT`].
    pub fn from_slice(bytes: &[u8]) -> TriggerEffect {
        let mut e = TriggerEffect::default();
        let n = bytes.len().min(MAX_TRIGGER_EFFECT);
        e.data[..n].copy_from_slice(&bytes[..n]);
        e.len = n as u8;
        e
    }

    /// The meaningful bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.data[..self.len.min(MAX_TRIGGER_EFFECT as u8) as usize]
    }
}

/// One pad's full output-effect frame.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct PadOutput {
    pub pad_index: u8,
    /// Wraps; compared modularly so a reordered frame cannot strand an effect.
    pub seq: u8,
    pub motor_low: u16,
    pub motor_high: u16,
    /// Lightbar colour.
    pub led: [u8; 3],
    /// Player-indicator LEDs (family-specific pattern byte).
    pub player_led: u8,
    /// See [`flags`].
    pub flags: u8,
    pub left_trigger: TriggerEffect,
    pub right_trigger: TriggerEffect,
}

impl PadOutput {
    /// Encode into `out`, returning the number of bytes written. `None` if `out`
    /// is smaller than [`PAD_OUTPUT_MAX_BODY`].
    pub fn encode(&self, out: &mut [u8]) -> Option<usize> {
        if out.len() < PAD_OUTPUT_MAX_BODY {
            return None;
        }
        out[0] = self.pad_index;
        out[1] = self.seq;
        out[2..4].copy_from_slice(&self.motor_low.to_le_bytes());
        out[4..6].copy_from_slice(&self.motor_high.to_le_bytes());
        out[6..9].copy_from_slice(&self.led);
        out[9] = self.player_led;
        out[10] = self.flags;

        let mut at = 11;
        for effect in [&self.left_trigger, &self.right_trigger] {
            let bytes = effect.bytes();
            out[at] = bytes.len() as u8;
            out[at + 1..at + 1 + bytes.len()].copy_from_slice(bytes);
            at += 1 + bytes.len();
        }
        Some(at)
    }

    /// Decode a body. `None` on anything malformed — a short buffer, an
    /// out-of-range pad, or a trigger length past what remains.
    pub fn decode(buf: &[u8]) -> Option<PadOutput> {
        if buf.len() < 11 {
            return None;
        }
        let pad_index = buf[0];
        if pad_index >= MAX_PADS {
            return None;
        }
        let mut out = PadOutput {
            pad_index,
            seq: buf[1],
            motor_low: u16::from_le_bytes([buf[2], buf[3]]),
            motor_high: u16::from_le_bytes([buf[4], buf[5]]),
            led: [buf[6], buf[7], buf[8]],
            player_led: buf[9],
            flags: buf[10],
            ..Default::default()
        };

        let mut at = 11;
        for effect in [&mut out.left_trigger, &mut out.right_trigger] {
            let len = *buf.get(at)? as usize;
            if len > MAX_TRIGGER_EFFECT {
                return None;
            }
            let start = at + 1;
            let bytes = buf.get(start..start + len)?;
            effect.data[..len].copy_from_slice(bytes);
            effect.len = len as u8;
            at = start + len;
        }
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> PadOutput {
        PadOutput {
            pad_index: 2,
            seq: 200,
            motor_low: 40000,
            motor_high: 12345,
            led: [0x10, 0x20, 0x30],
            player_led: 0b0001_0101,
            flags: flags::MIC_MUTED,
            left_trigger: TriggerEffect::from_slice(&[0x02, 0x90, 0xA0, 0xFF, 0x00]),
            right_trigger: TriggerEffect::from_slice(&[0x26, 0x01, 0x02, 0x03]),
        }
    }

    #[test]
    fn round_trips() {
        let p = sample();
        let mut buf = [0u8; PAD_OUTPUT_MAX_BODY];
        let n = p.encode(&mut buf).unwrap();
        assert_eq!(PadOutput::decode(&buf[..n]), Some(p));
    }

    #[test]
    fn a_no_effect_frame_is_compact() {
        let p = PadOutput {
            pad_index: 0,
            seq: 1,
            motor_low: 100,
            ..Default::default()
        };
        let mut buf = [0u8; PAD_OUTPUT_MAX_BODY];
        let n = p.encode(&mut buf).unwrap();
        // 11 head + two zero-length effects (1 byte each).
        assert_eq!(n, 13);
        assert_eq!(PadOutput::decode(&buf[..n]), Some(p));
    }

    #[test]
    fn refuses_a_pad_the_client_does_not_have() {
        let mut buf = [0u8; PAD_OUTPUT_MAX_BODY];
        sample().encode(&mut buf).unwrap();
        buf[0] = MAX_PADS;
        assert_eq!(PadOutput::decode(&buf), None);
    }

    #[test]
    fn a_trigger_length_past_the_buffer_is_refused() {
        let mut buf = [0u8; PAD_OUTPUT_MAX_BODY];
        let n = sample().encode(&mut buf).unwrap();
        // Corrupt the left-trigger length to claim more than remains.
        buf[11] = (MAX_TRIGGER_EFFECT + 1) as u8;
        assert_eq!(PadOutput::decode(&buf[..n]), None);
    }

    #[test]
    fn truncated_bodies_are_refused_rather_than_panicking() {
        let mut buf = [0u8; PAD_OUTPUT_MAX_BODY];
        let n = sample().encode(&mut buf).unwrap();
        for len in 0..n {
            assert_eq!(PadOutput::decode(&buf[..len]), None, "{len}-byte body");
        }
    }

    #[test]
    fn encode_refuses_a_buffer_it_could_overrun() {
        let mut small = [0u8; PAD_OUTPUT_MAX_BODY - 1];
        assert_eq!(sample().encode(&mut small), None);
    }
}
