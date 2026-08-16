// SPDX-License-Identifier: GPL-2.0-or-later

//! Wrapping sequence numbers and the input replay window.
//!
//! `frame_id` is 16 bits and wraps roughly every 18 minutes at 60 fps.
//! PROTOCOL.md says to compare modularly rather than with `<`; [`Seq16`] makes
//! the wrong comparison unrepresentable instead of relying on everyone
//! remembering, because the failure is invisible for 18 minutes and then
//! reorders an entire jitter buffer at once.

use core::cmp::Ordering;

/// A 16-bit wrapping sequence number.
///
/// Deliberately not `Ord`: there is no total order on a wrapping sequence, only
/// a local one. Use [`Seq16::is_newer_than`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, Default)]
pub struct Seq16(pub u16);

impl Seq16 {
    /// Whether `self` is ahead of `other`, treating the space as circular.
    ///
    /// Ahead means the forward distance is less than half the space. Exactly
    /// half is ambiguous and reported as not-newer, which is the safe answer for
    /// a jitter buffer: a frame that far away is not usable either way.
    pub fn is_newer_than(self, other: Seq16) -> bool {
        let forward = self.0.wrapping_sub(other.0);
        forward != 0 && forward < 0x8000
    }

    /// Signed distance from `other` to `self`, in the range −32768..=32767.
    pub fn distance_from(self, other: Seq16) -> i32 {
        (self.0.wrapping_sub(other.0) as i16) as i32
    }

    /// Ordering relative to `other`, or `None` at the antipode where the two are
    /// equidistant in both directions.
    pub fn partial_order(self, other: Seq16) -> Option<Ordering> {
        match self.0.wrapping_sub(other.0) {
            0 => Some(Ordering::Equal),
            0x8000 => None,
            d if d < 0x8000 => Some(Ordering::Greater),
            _ => Some(Ordering::Less),
        }
    }

    pub fn next(self) -> Seq16 {
        Seq16(self.0.wrapping_add(1))
    }
}

/// Entries in the input replay window. PROTOCOL.md specifies 256.
pub const REPLAY_WINDOW: u32 = 256;

/// Sliding anti-replay window over the 32-bit `input_seq`.
///
/// The MAC proves a packet came from the peer; this proves it has not been seen
/// before. Both halves are needed — a MAC is deterministic, so a captured packet
/// re-sent verbatim verifies perfectly.
#[derive(Debug)]
pub struct ReplayWindow {
    highest: u32,
    /// Bit *n* marks `highest - n` as seen. Bit 0 is `highest` itself.
    seen: [u64; 4],
    started: bool,
}

impl Default for ReplayWindow {
    fn default() -> Self {
        Self::new()
    }
}

impl ReplayWindow {
    pub fn new() -> ReplayWindow {
        ReplayWindow {
            highest: 0,
            seen: [0; 4],
            started: false,
        }
    }

    /// Accept `seq` if it is fresh, marking it seen. Returns false for a replay
    /// or for a packet older than the window.
    ///
    /// Call this only after the MAC has verified. Checking the sequence first
    /// looks like a cheap filter but lets unauthenticated traffic move the
    /// window, which is the one thing it must not be able to do.
    pub fn accept(&mut self, seq: u32) -> bool {
        if !self.started {
            self.started = true;
            self.highest = seq;
            self.seen = [0; 4];
            self.set(0);
            return true;
        }

        let forward = seq.wrapping_sub(self.highest);
        if forward == 0 {
            return false; // exactly the newest packet, seen by definition
        }

        if forward < 0x8000_0000 {
            // Newer. Slide the window up and mark the new top.
            self.shift(forward);
            self.highest = seq;
            self.set(0);
            return true;
        }

        // Older. `age` is how far behind the top it sits.
        let age = self.highest.wrapping_sub(seq);
        if age >= REPLAY_WINDOW {
            return false; // fell off the back
        }
        if self.get(age) {
            return false; // replay
        }
        self.set(age);
        true
    }

    fn shift(&mut self, by: u32) {
        if by >= REPLAY_WINDOW {
            // Everything currently recorded is now off the back of the window.
            self.seen = [0; 4];
            return;
        }
        let by = by as usize;
        let words = by / 64;
        let bits = by % 64;
        // Bit n means `highest - n`, so raising `highest` shifts bits upward.
        for i in (0..4).rev() {
            let mut v = if i >= words { self.seen[i - words] } else { 0 };
            if bits > 0 {
                v <<= bits;
                if i > words {
                    v |= self.seen[i - words - 1] >> (64 - bits);
                }
            }
            self.seen[i] = v;
        }
    }

    fn set(&mut self, age: u32) {
        let age = age as usize;
        self.seen[age / 64] |= 1u64 << (age % 64);
    }

    fn get(&self, age: u32) -> bool {
        let age = age as usize;
        self.seen[age / 64] & (1u64 << (age % 64)) != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_is_newer_in_the_ordinary_case() {
        assert!(Seq16(5).is_newer_than(Seq16(4)));
        assert!(!Seq16(4).is_newer_than(Seq16(5)));
        assert!(!Seq16(4).is_newer_than(Seq16(4)));
    }

    #[test]
    fn newer_survives_the_wrap() {
        // The whole reason this type exists: `0 < 65535` is true and wrong.
        assert!(Seq16(0).is_newer_than(Seq16(65535)));
        assert!(Seq16(2).is_newer_than(Seq16(65534)));
        assert!(!Seq16(65535).is_newer_than(Seq16(0)));
        assert!(!Seq16(65534).is_newer_than(Seq16(2)));
    }

    #[test]
    fn the_antipode_is_ordered_neither_way() {
        // Exactly half the space apart: no honest answer exists.
        assert!(!Seq16(0).is_newer_than(Seq16(0x8000)));
        assert!(!Seq16(0x8000).is_newer_than(Seq16(0)));
        assert_eq!(Seq16(0).partial_order(Seq16(0x8000)), None);
    }

    #[test]
    fn distance_is_signed_and_wraps() {
        assert_eq!(Seq16(5).distance_from(Seq16(3)), 2);
        assert_eq!(Seq16(3).distance_from(Seq16(5)), -2);
        assert_eq!(Seq16(1).distance_from(Seq16(65535)), 2);
        assert_eq!(Seq16(65535).distance_from(Seq16(1)), -2);
    }

    #[test]
    fn a_full_lap_stays_consistent() {
        // Walk the entire 16-bit space and confirm each step reads as newer.
        let mut prev = Seq16(0);
        for _ in 0..=u16::MAX {
            let next = prev.next();
            assert!(next.is_newer_than(prev), "{next:?} should follow {prev:?}");
            prev = next;
        }
        assert_eq!(prev, Seq16(0), "a full lap should return to the start");
    }

    #[test]
    fn replay_window_accepts_a_fresh_run() {
        let mut w = ReplayWindow::new();
        for seq in 0..1000 {
            assert!(w.accept(seq), "{seq} should be fresh");
        }
    }

    #[test]
    fn replay_window_rejects_repeats() {
        let mut w = ReplayWindow::new();
        assert!(w.accept(100));
        assert!(!w.accept(100), "the same packet twice is a replay");
        assert!(w.accept(101));
        assert!(!w.accept(101));
    }

    #[test]
    fn replay_window_accepts_reordering_inside_the_window() {
        // Packets 10 and 11 swapped on the wire. Both must be accepted.
        let mut w = ReplayWindow::new();
        assert!(w.accept(9));
        assert!(w.accept(11));
        assert!(w.accept(10), "a reordered packet is not a replay");
        assert!(!w.accept(10), "but only once");
    }

    #[test]
    fn replay_window_rejects_what_fell_off_the_back() {
        let mut w = ReplayWindow::new();
        assert!(w.accept(0));
        assert!(w.accept(REPLAY_WINDOW + 10));
        // 0 is now well outside the window and cannot be distinguished from a
        // replay, so it must be refused.
        assert!(!w.accept(0));
        // The oldest still inside is accepted.
        assert!(w.accept(REPLAY_WINDOW + 10 - (REPLAY_WINDOW - 1)));
    }

    #[test]
    fn replay_window_shifts_by_more_than_a_word() {
        // Exercises the multi-word path in `shift`, where an off-by-one is
        // invisible in the single-word case.
        let mut w = ReplayWindow::new();
        assert!(w.accept(0));
        assert!(w.accept(70)); // > 64, crosses a word boundary
        assert!(!w.accept(70));
        assert!(w.accept(69));
        assert!(w.accept(5));
        assert!(!w.accept(5));
        assert!(!w.accept(0), "0 is still marked, 70 places back");
    }

    #[test]
    fn replay_window_survives_a_u32_wrap() {
        // 49 days of continuous input at 1000 packets/sec. A session should be
        // re-keyed long before this, but wrapping arithmetic costs nothing and
        // an unsigned comparison here would reject every packet forever.
        let mut w = ReplayWindow::new();
        assert!(w.accept(u32::MAX - 2));
        assert!(w.accept(u32::MAX - 1));
        assert!(w.accept(u32::MAX));
        assert!(
            w.accept(0),
            "the wrap is a newer packet, not an ancient one"
        );
        assert!(w.accept(1));
        assert!(!w.accept(0), "and it is still only accepted once");
        assert!(!w.accept(u32::MAX));
    }

    #[test]
    fn replay_window_starts_wherever_the_first_packet_does() {
        // The peer's counter does not start at zero after a re-key.
        let mut w = ReplayWindow::new();
        assert!(w.accept(4_000_000_000));
        assert!(!w.accept(4_000_000_000));
        assert!(w.accept(4_000_000_001));
    }
}
