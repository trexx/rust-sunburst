// SPDX-License-Identifier: GPL-2.0-or-later

//! The server side of the unreliable output channel: sequence + repeat.
//!
//! Rumble and rich pad output are sent latest-wins and unreliably, so a lost
//! final "stop" packet would leave a motor running until the battery dies. The
//! client's [`RumbleTracker`](sunburst_core::proto::rumble::RumbleTracker) stops a
//! motor it has not heard about within a timeout; this is the matching server
//! half — it stamps each effect with a sequence and **repeats a non-neutral effect
//! every [`RUMBLE_REPEAT_MS`]** with the same sequence, so a dropped packet
//! self-heals before the client's timeout fires.
//!
//! Pure and cross-platform: time is passed in, so it is tested on the host and the
//! Windows injector drives it with a real clock.

use sunburst_core::proto::rumble::RUMBLE_REPEAT_MS;

/// Per-pad sequence + repeat bookkeeping for one output stream.
#[derive(Debug, Default)]
pub struct RepeatPolicy {
    seq: u8,
    last_sent_ms: u32,
    active: bool,
    started: bool,
}

impl RepeatPolicy {
    pub fn new() -> RepeatPolicy {
        RepeatPolicy::default()
    }

    /// Stamp a freshly-decoded effect for sending: bump the sequence, record
    /// whether it is `active` (non-neutral — commands a motor / trigger / LED) and
    /// when it went out. Returns the sequence to send with.
    pub fn on_send(&mut self, active: bool, now_ms: u32) -> u8 {
        self.seq = self.seq.wrapping_add(1);
        self.active = active;
        self.last_sent_ms = now_ms;
        self.started = true;
        self.seq
    }

    /// If an active effect has gone [`RUMBLE_REPEAT_MS`] without being resent,
    /// returns the sequence to **repeat** it with (unchanged, so the client counts
    /// it as still-heard, not a new level) and records the resend time. A neutral
    /// or never-sent stream returns `None`.
    pub fn repeat_due(&mut self, now_ms: u32) -> Option<u8> {
        if self.started && self.active && now_ms.wrapping_sub(self.last_sent_ms) >= RUMBLE_REPEAT_MS
        {
            self.last_sent_ms = now_ms;
            Some(self.seq)
        } else {
            None
        }
    }

    /// The current sequence.
    pub fn seq(&self) -> u8 {
        self.seq
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_new_effect_advances_the_sequence() {
        let mut p = RepeatPolicy::new();
        assert_eq!(p.on_send(true, 0), 1);
        assert_eq!(p.on_send(true, 10), 2);
        assert_eq!(p.on_send(false, 20), 3);
    }

    #[test]
    fn a_non_neutral_effect_repeats_with_the_same_sequence() {
        let mut p = RepeatPolicy::new();
        let seq = p.on_send(true, 0);
        assert_eq!(p.repeat_due(RUMBLE_REPEAT_MS - 1), None, "too soon");
        assert_eq!(
            p.repeat_due(RUMBLE_REPEAT_MS),
            Some(seq),
            "repeat keeps the seq"
        );
        // And it keeps repeating on the same cadence.
        assert_eq!(p.repeat_due(2 * RUMBLE_REPEAT_MS), Some(seq));
    }

    #[test]
    fn a_neutral_effect_does_not_repeat() {
        // The stop packet: once sent, nothing is resent — the client's own
        // timeout is not needed because there is no motor left running.
        let mut p = RepeatPolicy::new();
        p.on_send(true, 0);
        p.on_send(false, 10); // motors to zero
        assert_eq!(p.repeat_due(10 + RUMBLE_REPEAT_MS * 5), None);
    }

    #[test]
    fn an_untouched_policy_never_repeats() {
        let mut p = RepeatPolicy::new();
        assert_eq!(p.repeat_due(RUMBLE_REPEAT_MS * 10), None);
    }
}
