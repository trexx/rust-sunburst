// SPDX-License-Identifier: GPL-2.0-or-later

//! Moving the cursor overlay from the client's own mouse — pure, host-tested.
//!
//! The overlay used to move only when the server reported the pointer: ~100 ms
//! of throttle plus a round trip behind the hand, which is the one latency a
//! mouse user notices first. The client knows its own mouse, and the server
//! tells it the gain (`SessionConfig.pointer_gain_milli`: server pixels per
//! count), so it can move the overlay the instant a move is sent.
//!
//! The server stays the truth. Each `CursorPosition` carries the newest mouse
//! `input_seq` it had applied, so the client knows which of its moves the
//! position already includes. Truth is that position plus every move sent
//! since. The prediction is compared against *that*, not against the raw
//! position, so a report that is a few moves behind does not drag the overlay
//! backwards. It is corrected only when it has really diverged: a game warped
//! the pointer, or the pointer hit an edge the client did not know about. While
//! the mouse is moving, "really" means [`MOVING_SNAP_PX`]; once it has stopped,
//! half a pixel, so the resting position is always exactly the server's.
//!
//! [`SubPixel`] is the other half: Android reports fractional deltas, and
//! truncating each one threw away slow motion entirely.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Server pixels of disagreement tolerated while the mouse is moving. The
/// server's report lags by the time Windows takes to apply the moves, which at
/// speed is tens of pixels; correcting inside that would be jitter.
pub const MOVING_SNAP_PX: f32 = 48.0;
/// Once still, the prediction must match the server to within this.
pub const IDLE_SNAP_PX: f32 = 0.5;
/// How recently a move counts as "moving", in ms.
pub const MOVING_WINDOW_MS: u64 = 60;
/// Moves remembered while unacknowledged. At a 1 kHz mouse that is a quarter
/// second, several times the server's report interval.
pub const RING: usize = 256;

/// Carries the fraction of each fractional delta into the next, so slow motion
/// is sent rather than truncated away.
#[derive(Clone, Copy, Debug, Default)]
pub struct SubPixel {
    rx: f32,
    ry: f32,
}

impl SubPixel {
    /// The whole counts to send for this delta (toward zero, saturating to
    /// `i16`), keeping the remainder.
    pub fn take(&mut self, dx: f32, dy: f32) -> (i16, i16) {
        (take_axis(&mut self.rx, dx), take_axis(&mut self.ry, dy))
    }
}

fn take_axis(rem: &mut f32, d: f32) -> i16 {
    let total = *rem + d;
    let whole = total
        .trunc()
        .clamp(f32::from(i16::MIN), f32::from(i16::MAX));
    *rem = total - whole;
    whole as i16
}

/// One sent move, kept until the server acknowledges it.
#[derive(Clone, Copy, Debug, Default)]
struct Move {
    seq: u32,
    dx: i16,
    dy: i16,
}

/// The overlay's position, predicted from local moves and reconciled against
/// the server's reports.
#[derive(Debug)]
pub struct CursorPredictor {
    /// The captured output in server pixels; zero until known.
    width: u16,
    height: u16,
    /// Server pixels per count; zero means do not predict.
    gain: f32,
    x: f32,
    y: f32,
    visible: bool,
    /// Whether a server report has anchored the prediction yet.
    anchored: bool,
    ring: [Move; RING],
    head: usize,
    len: usize,
    /// Moves were dropped from a full ring, so the unacknowledged sum is not
    /// known exactly; reconcile only once the mouse is still.
    overflowed: bool,
    last_move_ms: u64,
}

impl Default for CursorPredictor {
    fn default() -> CursorPredictor {
        CursorPredictor {
            width: 0,
            height: 0,
            gain: 0.0,
            x: 0.0,
            y: 0.0,
            visible: false,
            anchored: false,
            ring: [Move::default(); RING],
            head: 0,
            len: 0,
            overflowed: false,
            last_move_ms: 0,
        }
    }
}

/// Whether `seq` is at or before `acked`, allowing for the wrap. Sequences
/// start at 1 each session, so in practice this is `seq <= acked`.
fn acked(seq: u32, acked: u32) -> bool {
    (seq.wrapping_sub(acked) as i32) <= 0
}

impl CursorPredictor {
    pub fn new() -> CursorPredictor {
        CursorPredictor::default()
    }

    /// The captured output's size in server pixels (from `CodecPrivate`) and
    /// the gain × 1000 (from `SessionConfig`). Cheap enough to call every time.
    pub fn set_geometry(&mut self, width: u16, height: u16, gain_milli: u32) {
        if (width, height) != (self.width, self.height) {
            // A new output size: the old prediction means nothing in it, so
            // wait for the server to anchor it again.
            self.anchored = false;
        }
        self.width = width;
        self.height = height;
        self.gain = gain_milli as f32 / 1000.0;
    }

    /// Whether the overlay is being predicted at all.
    pub fn predicting(&self) -> bool {
        self.gain > 0.0 && self.width > 0 && self.height > 0
    }

    /// Keep the prediction on the output. The range is `0..=extent`, the same
    /// one the server's normalisation maps onto 0..=65535.
    fn clamp(&mut self) {
        self.x = self.x.clamp(0.0, f32::from(self.width));
        self.y = self.y.clamp(0.0, f32::from(self.height));
    }

    /// A move the client just sent as `seq`.
    pub fn on_local_move(&mut self, seq: u32, dx: i16, dy: i16, now_ms: u64) {
        if !self.predicting() {
            return;
        }
        self.x += self.gain * f32::from(dx);
        self.y += self.gain * f32::from(dy);
        self.clamp();
        if self.len == RING {
            self.head = (self.head + 1) % RING;
            self.len -= 1;
            self.overflowed = true;
        }
        self.ring[(self.head + self.len) % RING] = Move { seq, dx, dy };
        self.len += 1;
        self.last_move_ms = now_ms;
    }

    /// A server report: the pointer at `(x, y)` (0..=65535 of the captured
    /// output), `visible`, having applied moves up to `acked_seq`.
    pub fn on_server(&mut self, x: u16, y: u16, visible: bool, acked_seq: u32, now_ms: u64) {
        self.visible = visible;
        // Forget what the report already includes.
        while self.len > 0 && acked(self.ring[self.head].seq, acked_seq) {
            self.head = (self.head + 1) % RING;
            self.len -= 1;
        }
        if !self.predicting() {
            return;
        }
        let scale = |v: u16, extent: u16| f32::from(v) * f32::from(extent) / 65_535.0;
        let (mut tx, mut ty) = (scale(x, self.width), scale(y, self.height));
        for i in 0..self.len {
            let m = self.ring[(self.head + i) % RING];
            tx += self.gain * f32::from(m.dx);
            ty += self.gain * f32::from(m.dy);
        }
        let moving = now_ms.saturating_sub(self.last_move_ms) <= MOVING_WINDOW_MS;
        if moving && self.overflowed {
            // The sum is incomplete; a correction now would be a guess.
            return;
        }
        if !moving {
            self.overflowed = false;
        }
        let threshold = if moving { MOVING_SNAP_PX } else { IDLE_SNAP_PX };
        let error = ((tx - self.x).powi(2) + (ty - self.y).powi(2)).sqrt();
        if !self.anchored || error > threshold {
            self.x = tx;
            self.y = ty;
            self.clamp();
            self.anchored = true;
        }
    }

    /// The overlay position as 0..=65535 of the output, and visibility, once
    /// predicting and anchored.
    pub fn position(&self) -> Option<(u16, u16, bool)> {
        if !self.predicting() || !self.anchored {
            return None;
        }
        let norm = |v: f32, extent: u16| {
            (v / f32::from(extent) * 65_535.0)
                .round()
                .clamp(0.0, 65_535.0) as u16
        };
        Some((
            norm(self.x, self.width),
            norm(self.y, self.height),
            self.visible,
        ))
    }
}

/// A position for the JNI boundary: `visible << 32 | x << 16 | y`, or `-1`.
pub fn pack(position: Option<(u16, u16, bool)>) -> i64 {
    match position {
        Some((x, y, visible)) => i64::from(visible) << 32 | i64::from(x) << 16 | i64::from(y),
        None => -1,
    }
}

/// A server report crossing from the client thread to the UI thread in one
/// atomic word: `x:16 | y:16 | visible:1 | present:1 | seq:30`, and `0` for
/// none (the present bit is what keeps a real report from ever reading as
/// empty). The sequence is masked to 30 bits; a session numbers input from 1
/// and does not reach 2³⁰ in days of continuous mousing.
pub mod mailbox {
    pub const EMPTY: u64 = 0;
    const PRESENT: u64 = 1 << 30;
    const SEQ_MASK: u32 = 0x3FFF_FFFF;

    pub fn encode(x: u16, y: u16, visible: bool, seq: u32) -> u64 {
        u64::from(x) << 48
            | u64::from(y) << 32
            | u64::from(visible) << 31
            | PRESENT
            | u64::from(seq & SEQ_MASK)
    }

    pub fn decode(word: u64) -> Option<(u16, u16, bool, u32)> {
        (word & PRESENT != 0).then_some((
            (word >> 48) as u16,
            (word >> 32) as u16,
            word >> 31 & 1 != 0,
            word as u32 & SEQ_MASK,
        ))
    }
}

/// What the client thread and the UI thread share, all atomics: the client
/// thread learns the geometry and the server's reports, the UI thread predicts
/// from them, and both hand out input sequence numbers. Nothing here is a lock,
/// so the client (frame) thread never waits on the UI.
#[derive(Debug)]
pub struct CursorShared {
    next_seq: AtomicU32,
    /// `width << 16 | height` of the captured output; 0 until known.
    space: AtomicU32,
    gain_milli: AtomicU32,
    /// The latest server report, [`mailbox`]-encoded.
    report: AtomicU64,
}

impl Default for CursorShared {
    fn default() -> CursorShared {
        CursorShared {
            // A session numbers input from 1 (0 means "none" in reports).
            next_seq: AtomicU32::new(1),
            space: AtomicU32::new(0),
            gain_milli: AtomicU32::new(0),
            report: AtomicU64::new(mailbox::EMPTY),
        }
    }
}

impl CursorShared {
    /// The next `InputPacket.input_seq`, for any input from either thread.
    pub fn next_seq(&self) -> u32 {
        self.next_seq.fetch_add(1, Ordering::Relaxed)
    }

    pub fn set_space(&self, width: u16, height: u16) {
        self.space.store(
            u32::from(width) << 16 | u32::from(height),
            Ordering::Release,
        );
    }

    pub fn set_gain_milli(&self, gain: u16) {
        self.gain_milli.store(u32::from(gain), Ordering::Release);
    }

    /// A server report, replacing any the UI thread has not taken yet (only
    /// the newest matters).
    pub fn publish(&self, x: u16, y: u16, visible: bool, seq: u32) {
        self.report
            .store(mailbox::encode(x, y, visible, seq), Ordering::Release);
    }

    /// Bring `predictor` up to date with the geometry and any new report.
    pub fn sync(&self, predictor: &mut CursorPredictor, now_ms: u64) {
        let space = self.space.load(Ordering::Acquire);
        predictor.set_geometry(
            (space >> 16) as u16,
            space as u16,
            self.gain_milli.load(Ordering::Acquire),
        );
        if let Some((x, y, visible, seq)) =
            mailbox::decode(self.report.swap(mailbox::EMPTY, Ordering::AcqRel))
        {
            predictor.on_server(x, y, visible, seq, now_ms);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: u16 = 3840;
    const H: u16 = 2160;

    /// A predictor at 1:1 anchored at the centre, idle.
    fn anchored() -> CursorPredictor {
        let mut p = CursorPredictor::new();
        p.set_geometry(W, H, 1000);
        p.on_server(32_767, 32_767, true, 0, 0);
        p
    }

    fn px(p: &CursorPredictor) -> (f32, f32) {
        (p.x, p.y)
    }

    #[test]
    fn slow_fractional_motion_is_sent_not_truncated() {
        let mut s = SubPixel::default();
        let mut sum = 0i32;
        for _ in 0..100 {
            sum += i32::from(s.take(0.3, 0.0).0);
        }
        assert_eq!(sum, 30, "0.3 x 100 is 30 counts, not 0");
        let mut s = SubPixel::default();
        assert_eq!(s.take(-0.6, 0.0), (0, 0));
        assert_eq!(s.take(-0.6, 0.0), (-1, 0), "negatives carry too");
        assert_eq!(s.take(40_000.0, 0.0).0, i16::MAX, "a flick saturates");
    }

    #[test]
    fn nothing_is_predicted_until_the_server_anchors_it() {
        let mut p = CursorPredictor::new();
        p.set_geometry(W, H, 1000);
        p.on_local_move(1, 10, 0, 0);
        assert_eq!(p.position(), None);
        let mut off = CursorPredictor::new();
        off.set_geometry(W, H, 0);
        off.on_server(0, 0, true, 0, 0);
        assert_eq!(
            off.position(),
            None,
            "gain 0 means EPP is on: do not predict"
        );
    }

    #[test]
    fn a_local_move_moves_the_overlay_at_once() {
        let mut p = anchored();
        let (x0, y0) = px(&p);
        p.on_local_move(1, 10, -5, 1_000);
        assert_eq!(px(&p), (x0 + 10.0, y0 - 5.0));
        let mut fast = CursorPredictor::new();
        fast.set_geometry(W, H, 1500);
        fast.on_server(0, 0, true, 0, 0);
        fast.on_local_move(1, 10, 0, 1_000);
        assert_eq!(px(&fast).0, 15.0, "the gain scales it");
    }

    #[test]
    fn a_lagging_report_does_not_drag_the_overlay_back() {
        let mut p = anchored();
        for seq in 1..=10 {
            p.on_local_move(seq, 5, 0, 1_000);
        }
        let predicted = px(&p);
        // The server has applied moves 1..=4: its position is 20 px along, and
        // 6 moves (30 px) are still in flight.
        let (sx, _) = px(&anchored());
        let reported = ((sx + 20.0) / f32::from(W) * 65_535.0).round() as u16;
        p.on_server(reported, 32_767, true, 4, 1_010);
        let (x, _) = px(&p);
        assert!((x - predicted.0).abs() < 1.0, "{x} vs {}", predicted.0);
    }

    #[test]
    fn a_warp_is_corrected_even_while_moving() {
        let mut p = anchored();
        p.on_local_move(1, 5, 0, 1_000);
        // A game recentred the pointer at the top-left.
        p.on_server(0, 0, true, 1, 1_010);
        assert_eq!(px(&p), (0.0, 0.0));
    }

    #[test]
    fn small_disagreement_waits_for_the_mouse_to_stop() {
        let mut p = anchored();
        p.on_local_move(1, 5, 0, 1_000);
        let predicted = px(&p);
        // The server is 3 px short of the prediction (a rounding, an edge).
        let truth_x = predicted.0 - 3.0;
        let norm = (truth_x / f32::from(W) * 65_535.0).round() as u16;
        p.on_server(norm, 32_767, true, 1, 1_010);
        assert_eq!(
            px(&p),
            predicted,
            "moving: within tolerance, keep predicting"
        );
        p.on_server(norm, 32_767, true, 1, 2_000);
        assert!(
            (px(&p).0 - truth_x).abs() < 0.1,
            "still: the server's position, exactly"
        );
    }

    #[test]
    fn a_full_ring_reconciles_only_once_still() {
        let mut p = anchored();
        for seq in 1..=(RING as u32 + 10) {
            p.on_local_move(seq, 1, 0, 1_000);
        }
        let predicted = px(&p);
        p.on_server(0, 0, true, 0, 1_010);
        assert_eq!(px(&p), predicted, "the unacked sum is incomplete: no guess");
        p.on_server(0, 0, true, RING as u32 + 10, 2_000);
        assert_eq!(px(&p), (0.0, 0.0));
    }

    #[test]
    fn the_overlay_stays_on_the_output() {
        let mut p = anchored();
        p.on_local_move(1, i16::MAX, i16::MIN, 0);
        assert_eq!(px(&p), (f32::from(W), 0.0));
    }

    #[test]
    fn a_new_output_size_waits_for_a_fresh_anchor() {
        let mut p = anchored();
        p.set_geometry(2560, 1440, 1000);
        assert_eq!(p.position(), None);
        p.on_server(0, 65_535, true, 0, 0);
        assert_eq!(p.position(), Some((0, 65_535, true)));
    }

    #[test]
    fn acknowledgement_survives_the_wrap() {
        assert!(acked(u32::MAX, 2));
        assert!(!acked(3, u32::MAX));
        assert!(acked(5, 5));
    }

    #[test]
    fn the_shared_state_hands_over_geometry_and_only_the_newest_report() {
        let shared = CursorShared::default();
        assert_eq!(shared.next_seq(), 1, "input numbers from 1");
        assert_eq!(shared.next_seq(), 2);
        shared.set_space(W, H);
        shared.set_gain_milli(1000);
        shared.publish(0, 0, true, 1);
        shared.publish(65_535, 65_535, true, 2);
        let mut p = CursorPredictor::new();
        shared.sync(&mut p, 0);
        assert_eq!(
            p.position(),
            Some((65_535, 65_535, true)),
            "the newer report wins"
        );
        // Taken: a second sync leaves the prediction alone.
        p.on_local_move(3, -100, 0, 10);
        shared.sync(&mut p, 20);
        assert_eq!(px(&p), (f32::from(W) - 100.0, f32::from(H)));
    }

    #[test]
    fn packing_round_trips() {
        assert_eq!(pack(None), -1);
        let p = pack(Some((0xABCD, 0x1234, true)));
        assert_eq!(
            (p >> 32, (p >> 16) & 0xFFFF, p & 0xFFFF),
            (1, 0xABCD, 0x1234)
        );
        assert!(pack(Some((65_535, 65_535, true))) >= 0);
        let w = mailbox::encode(1, 2, true, 0x3FFF_FFFF);
        assert_eq!(mailbox::decode(w), Some((1, 2, true, 0x3FFF_FFFF)));
        assert_eq!(mailbox::decode(mailbox::EMPTY), None);
        // No report, however extreme, reads as empty.
        for (x, y, v, seq) in [(0, 0, false, 0), (65_535, 65_535, true, u32::MAX)] {
            assert!(mailbox::decode(mailbox::encode(x, y, v, seq)).is_some());
        }
    }
}
