// SPDX-License-Identifier: GPL-2.0-or-later

//! How much audio the PCM ring keeps, and when the AAudio callback may play
//! from it — pure, host-tested.
//!
//! The ring first played from the first sample, then from a fixed cushion (one
//! burst plus a couple of frames), and trimmed anything past a fixed margin
//! above it. On the Homatics over Wi-Fi neither fit: audio packets arrived
//! with gaps of up to ~24 ms, while AAudio pulls a whole burst (8 ms) per
//! callback. The fixed cushion left ~20 ms of margin, so it underran every
//! couple of seconds, and the packets that arrive together after a gap
//! overflowed the fixed trim and were dropped. Both were audible as pops.
//!
//! So both are sized from what actually arrives. [`Cushion`] tracks the peak gap
//! between packet arrivals, decaying slowly so the buffer shrinks again after a
//! bad patch. The callback plays once the ring holds a burst plus that peak plus
//! a frame. The producer trims only past the cushion plus another peak gap plus
//! a few frames, so a normal post-gap burst fits and only sustained clock drift
//! is trimmed. An underrun anyway (a gap past anything seen yet) raises the peak
//! by a frame.
//!
//! That cushion is what playback *waits* for. Once running, the ring tends to
//! settle above it: on the Homatics every callback found at least ~32 ms
//! buffered against an 8 ms pull, ~24 ms that no gap ever used. Nothing pulled
//! it back down, since the trim only catches bursts. So the callback also
//! reports the lowest level it saw, and once the lowest over the last
//! [`MARGIN_WINDOWS`] windows still sits above the cushion, that surplus is
//! drained by slipping one sample frame from every other packet: a 0.4% speed-up
//! (about 7 cents), too small to hear, removing ~4 ms a second. All sizes are
//! interleaved samples, as the ring counts them.

/// Per-arrival decay of the peak gap: ~5% a second at 400 packets/s, so a
/// burst of jitter is forgotten over tens of seconds, not instantly.
const PEAK_DECAY: f32 = 1.0 / 8192.0;
/// Frames of slack on top of burst + peak, so the cushion is never below a
/// burst plus a frame even on a perfect link.
const SLACK_FRAMES: usize = 1;
/// The most jitter (beyond one burst) the cushion grows to absorb, in frames:
/// past this, latency costs more than an occasional glitch.
const MAX_PEAK_FRAMES: usize = 24;
/// Frames past cushion + peak before the producer drops a decoded frame.
const TRIM_SLACK_FRAMES: usize = 4;
/// How many margin windows (2 s each, in `audio.rs`) the lowest level must
/// stay high across before any of it is drained: long enough to have seen the
/// link's worse gaps, short enough to act within seconds.
pub const MARGIN_WINDOWS: usize = 5;

/// The producer side: the measured jitter, and the cushion and trim from it.
#[derive(Debug)]
pub struct Cushion {
    burst: usize,
    frame: usize,
    /// Samples in one sample frame (the channel count): the unit a slip drops.
    channels: usize,
    /// Peak gap between arrivals, in samples of playback it covers.
    peak: f32,
    /// The lowest level the callback saw in each recent window.
    margins: [usize; MARGIN_WINDOWS],
    /// How many entries of `margins` are real since the last reset.
    margins_seen: usize,
    next_margin: usize,
    /// Standing surplus still to drain, in samples.
    drain: usize,
    /// Slip on alternate packets only.
    slip_due: bool,
}

impl Cushion {
    /// `burst` is AAudio's callback size and `frame` one decoded packet, both
    /// in interleaved samples.
    /// `channels` is the samples per sample frame.
    pub fn new(burst: usize, frame: usize, channels: usize) -> Cushion {
        Cushion {
            burst,
            frame,
            channels: channels.max(1),
            peak: frame as f32,
            margins: [0; MARGIN_WINDOWS],
            margins_seen: 0,
            next_margin: 0,
            drain: 0,
            slip_due: false,
        }
    }

    /// A packet arrived `gap` samples' worth of time after the previous one.
    pub fn on_arrival(&mut self, gap: usize) {
        self.peak = (self.peak * (1.0 - PEAK_DECAY)).max(gap as f32);
        self.clamp();
    }

    /// The ring ran dry anyway: the link has more jitter than measured. Stop
    /// draining, and forget the margins that suggested it was safe.
    pub fn on_underrun(&mut self) {
        self.peak += self.frame as f32;
        self.clamp();
        self.drain = 0;
        self.margins_seen = 0;
    }

    /// One window ended; the callback's lowest level in it was `lowest`. Once
    /// [`MARGIN_WINDOWS`] windows all stayed above the cushion by more than a
    /// frame, drain the smallest of those surpluses.
    pub fn on_margin(&mut self, lowest: usize) {
        self.margins[self.next_margin] = lowest;
        self.next_margin = (self.next_margin + 1) % MARGIN_WINDOWS;
        self.margins_seen = (self.margins_seen + 1).min(MARGIN_WINDOWS);
        if self.margins_seen < MARGIN_WINDOWS {
            return;
        }
        let floor = self.margins.iter().copied().min().unwrap_or(0);
        let surplus = floor.saturating_sub(self.target());
        self.drain = if surplus > self.frame { surplus } else { 0 };
    }

    /// For one decoded packet about to be queued: whether to drop one sample
    /// frame from it to drain standing surplus.
    pub fn slip(&mut self) -> bool {
        if self.drain < self.channels {
            return false;
        }
        self.slip_due = !self.slip_due;
        if self.slip_due {
            self.drain -= self.channels;
        }
        self.slip_due
    }

    fn clamp(&mut self) {
        self.peak = self
            .peak
            .clamp(self.frame as f32, (MAX_PEAK_FRAMES * self.frame) as f32);
    }

    /// What the callback waits for before playing, in samples.
    pub fn target(&self) -> usize {
        self.burst + self.peak as usize + SLACK_FRAMES * self.frame
    }

    /// Past this many samples in the ring, the producer drops a decoded frame:
    /// more than a post-gap burst could explain, so the client is running slow.
    pub fn trim_above(&self) -> usize {
        self.target() + self.peak as usize + TRIM_SLACK_FRAMES * self.frame
    }
}

/// The callback side: whether to play from the ring now.
#[derive(Debug, Default)]
pub struct Gate {
    primed: bool,
}

impl Gate {
    /// One callback, with `available` samples in the ring and the producer's
    /// current `target`: whether to play. Waiting plays silence and leaves the
    /// ring to fill.
    pub fn should_play(&mut self, available: usize, target: usize) -> bool {
        if !self.primed && available >= target {
            self.primed = true;
        }
        self.primed
    }

    /// The callback popped fewer samples than it needed: refill to the target
    /// before playing again.
    pub fn underrun(&mut self) {
        self.primed = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BURST: usize = 768; // 384 stereo frames, 8 ms
    const FRAME: usize = 240; // 2.5 ms stereo at 48 kHz
    /// 24 ms of stereo samples: the worst gap measured on the Homatics.
    const GAP_24MS: usize = 2304;

    #[test]
    fn a_steady_link_keeps_a_small_cushion() {
        let mut c = Cushion::new(BURST, FRAME, 2);
        for _ in 0..1000 {
            c.on_arrival(FRAME);
        }
        assert_eq!(c.target(), BURST + FRAME + FRAME);
    }

    #[test]
    fn the_cushion_covers_the_worst_gap_seen() {
        let mut c = Cushion::new(BURST, FRAME, 2);
        c.on_arrival(GAP_24MS);
        // After the burst a callback pulls, the ring still holds the gap.
        assert!(c.target() - BURST >= GAP_24MS);
    }

    #[test]
    fn a_post_gap_burst_fits_under_the_trim() {
        let mut c = Cushion::new(BURST, FRAME, 2);
        c.on_arrival(GAP_24MS);
        // The gap's worth arrives at once on top of a full cushion.
        assert!(c.target() + GAP_24MS <= c.trim_above());
    }

    #[test]
    fn the_peak_decays_after_a_bad_patch() {
        let mut c = Cushion::new(BURST, FRAME, 2);
        c.on_arrival(GAP_24MS);
        let high = c.target();
        // A minute of steady 400 packets/s.
        for _ in 0..24_000 {
            c.on_arrival(FRAME);
        }
        assert!(c.target() < high);
        assert_eq!(c.target(), BURST + FRAME + FRAME, "back to the floor");
    }

    #[test]
    fn an_underrun_raises_the_peak_and_it_is_capped() {
        let mut c = Cushion::new(BURST, FRAME, 2);
        let before = c.target();
        c.on_underrun();
        assert_eq!(c.target(), before + FRAME);
        for _ in 0..100 {
            c.on_underrun();
        }
        assert_eq!(c.target(), BURST + MAX_PEAK_FRAMES * FRAME + FRAME);
    }

    /// Feed `windows` margin windows whose lowest level was `lowest`.
    fn margins(c: &mut Cushion, lowest: usize, windows: usize) {
        for _ in 0..windows {
            c.on_margin(lowest);
        }
    }

    /// How many sample frames `packets` packets would slip.
    fn slips(c: &mut Cushion, packets: usize) -> usize {
        (0..packets).filter(|_| c.slip()).count()
    }

    #[test]
    fn a_standing_surplus_is_drained_on_alternate_packets() {
        let mut c = Cushion::new(BURST, FRAME, 2);
        let t = c.target();
        let surplus = 10 * FRAME;
        margins(&mut c, t + surplus, MARGIN_WINDOWS);
        // One sample frame (2 samples) on every other packet, until it is gone.
        assert_eq!(slips(&mut c, 4), 2);
        assert_eq!(slips(&mut c, 10_000), surplus / 2 - 2);
        assert_eq!(slips(&mut c, 10), 0, "drained");
    }

    #[test]
    fn nothing_drains_until_every_window_agrees() {
        let mut c = Cushion::new(BURST, FRAME, 2);
        let t = c.target();
        margins(&mut c, t + 10 * FRAME, MARGIN_WINDOWS - 1);
        assert_eq!(slips(&mut c, 100), 0, "not enough windows yet");
        // One low window among high ones caps the drain at its surplus.
        let mut c = Cushion::new(BURST, FRAME, 2);
        let t = c.target();
        margins(&mut c, t + 10 * FRAME, MARGIN_WINDOWS - 1);
        c.on_margin(t + 2 * FRAME);
        assert_eq!(slips(&mut c, 10_000), 2 * FRAME / 2);
    }

    #[test]
    fn a_surplus_within_a_frame_is_left_alone() {
        let mut c = Cushion::new(BURST, FRAME, 2);
        let t = c.target();
        margins(&mut c, t + FRAME, MARGIN_WINDOWS);
        assert_eq!(slips(&mut c, 100), 0);
    }

    #[test]
    fn an_underrun_stops_the_drain_and_restarts_the_count() {
        let mut c = Cushion::new(BURST, FRAME, 2);
        let t = c.target();
        margins(&mut c, t + 10 * FRAME, MARGIN_WINDOWS);
        c.on_underrun();
        assert_eq!(slips(&mut c, 100), 0);
        margins(&mut c, t + 10 * FRAME, MARGIN_WINDOWS - 1);
        assert_eq!(slips(&mut c, 100), 0, "needs a full set of windows again");
    }

    #[test]
    fn the_gate_waits_for_the_target_then_plays_down_to_empty() {
        let mut g = Gate::default();
        assert!(!g.should_play(1000, 1248));
        assert!(g.should_play(1248, 1248));
        assert!(g.should_play(10, 1248), "primed: plays down to empty");
        g.underrun();
        assert!(!g.should_play(1000, 1248), "refills before playing again");
    }
}
