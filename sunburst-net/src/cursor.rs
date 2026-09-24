// SPDX-License-Identifier: GPL-2.0-or-later

//! Where the server says the pointer is, and when — pure, host-tested.
//!
//! The client draws the cursor itself (CLAUDE.md: the biggest single
//! responsiveness win), so the server reports the pointer's position. Two
//! decisions that were easy to get wrong live here instead of in the Win32
//! poller:
//!
//! - [`normalise`] maps a virtual-desktop point into the **captured output's**
//!   0..=65535 space. It used to use `SM_CXSCREEN` with no origin, which is the
//!   primary monitor's size at (0, 0) — wrong the moment the capture is a
//!   virtual display or a secondary output. A pointer that has left the captured
//!   output is reported hidden, so the overlay does not pin itself to an edge
//!   the viewer is not looking at.
//! - [`PositionThrottle`] sends on change only. It used to send every 100 ms
//!   whether or not anything moved, although the protocol describes the message
//!   as an override of the client's own idea of the position.

/// A pointer at `(x, y)` in virtual-desktop pixels, against the captured output
/// `rect` (`[left, top, right, bottom]`). Returns the 0..=65535 position within
/// it and whether the pointer is on it at all.
pub fn normalise(x: i32, y: i32, rect: [i32; 4]) -> (u16, u16, bool) {
    let [left, top, right, bottom] = rect;
    let w = (right - left).max(1);
    let h = (bottom - top).max(1);
    let inside = (left..right).contains(&x) && (top..bottom).contains(&y);
    let scale =
        |v: i32, extent: i32| (i64::from(v.clamp(0, extent)) * 65_535 / i64::from(extent)) as u16;
    (scale(x - left, w), scale(y - top, h), inside)
}

/// How often motion may be reported. A change of visibility is reported at
/// once.
pub const MOTION_INTERVAL_MS: u64 = 100;

/// Decides when a polled position goes on the wire.
///
/// Compared against the last position *sent*, not the last one polled, so a
/// pointer that stops between sends still has its resting place reported
/// within [`MOTION_INTERVAL_MS`], and a still pointer costs nothing.
#[derive(Debug, Default)]
pub struct PositionThrottle {
    last_sent: Option<(u16, u16, bool)>,
    last_sent_ms: u64,
}

impl PositionThrottle {
    pub fn new() -> PositionThrottle {
        PositionThrottle::default()
    }

    /// The position to send now, if any.
    pub fn offer(&mut self, pos: (u16, u16, bool), now_ms: u64) -> Option<(u16, u16, bool)> {
        let due = match self.last_sent {
            None => true,
            Some(last) if last == pos => false,
            Some(last) if last.2 != pos.2 => true,
            Some(_) => now_ms.saturating_sub(self.last_sent_ms) >= MOTION_INTERVAL_MS,
        };
        if !due {
            return None;
        }
        self.last_sent = Some(pos);
        self.last_sent_ms = now_ms;
        Some(pos)
    }

    /// Forget what was sent, so the next poll goes out whatever it says: after
    /// the output changed under the same pointer, say.
    pub fn reset(&mut self) {
        self.last_sent = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRIMARY: [i32; 4] = [0, 0, 3840, 2160];
    /// A second monitor to the left of the primary, as Windows lays it out:
    /// negative virtual-desktop coordinates.
    const LEFT: [i32; 4] = [-2560, 0, 0, 1440];

    #[test]
    fn corners_of_the_primary() {
        assert_eq!(normalise(0, 0, PRIMARY), (0, 0, true));
        assert_eq!(normalise(1920, 1080, PRIMARY), (32_767, 32_767, true));
        // The last pixel is inside; the edge itself is the next output's.
        assert!(normalise(3839, 2159, PRIMARY).2);
        assert!(!normalise(3840, 0, PRIMARY).2);
    }

    #[test]
    fn an_output_off_the_origin_is_measured_from_its_own_corner() {
        assert_eq!(normalise(-2560, 0, LEFT), (0, 0, true));
        assert_eq!(normalise(-1280, 720, LEFT), (32_767, 32_767, true));
        // The same point, against the primary: off it, and pinned to the edge.
        assert_eq!(normalise(-1280, 720, PRIMARY), (0, 21_845, false));
    }

    #[test]
    fn a_degenerate_rect_does_not_divide_by_zero() {
        assert_eq!(normalise(5, 5, [0, 0, 0, 0]), (65_535, 65_535, false));
    }

    #[test]
    fn a_still_pointer_is_sent_once() {
        let mut t = PositionThrottle::new();
        assert!(
            t.offer((10, 10, true), 0).is_some(),
            "the first report goes"
        );
        for now in (0..2_000).step_by(50) {
            assert!(
                t.offer((10, 10, true), now).is_none(),
                "nothing moved at {now}"
            );
        }
    }

    #[test]
    fn motion_is_limited_and_the_resting_place_always_lands() {
        let mut t = PositionThrottle::new();
        t.offer((0, 0, true), 0);
        assert!(t.offer((5, 0, true), 50).is_none(), "too soon");
        assert_eq!(t.offer((9, 0, true), 100), Some((9, 0, true)));
        // Motion stops at 12 between sends...
        assert!(t.offer((12, 0, true), 150).is_none());
        // ...and the next poll after the interval reports it.
        assert_eq!(t.offer((12, 0, true), 200), Some((12, 0, true)));
        assert!(t.offer((12, 0, true), 250).is_none());
    }

    #[test]
    fn visibility_goes_out_at_once() {
        let mut t = PositionThrottle::new();
        t.offer((0, 0, true), 0);
        assert_eq!(t.offer((0, 0, false), 10), Some((0, 0, false)));
        assert_eq!(t.offer((0, 0, true), 20), Some((0, 0, true)));
    }

    #[test]
    fn reset_resends() {
        let mut t = PositionThrottle::new();
        t.offer((1, 1, true), 0);
        t.reset();
        assert!(t.offer((1, 1, true), 1).is_some());
    }
}
