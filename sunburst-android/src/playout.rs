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
//! by a frame. All sizes are interleaved samples, as the ring counts them.

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

/// The producer side: the measured jitter, and the cushion and trim from it.
#[derive(Debug)]
pub struct Cushion {
    burst: usize,
    frame: usize,
    /// Peak gap between arrivals, in samples of playback it covers.
    peak: f32,
}

impl Cushion {
    /// `burst` is AAudio's callback size and `frame` one decoded packet, both
    /// in interleaved samples.
    pub fn new(burst: usize, frame: usize) -> Cushion {
        Cushion {
            burst,
            frame,
            peak: frame as f32,
        }
    }

    /// A packet arrived `gap` samples' worth of time after the previous one.
    pub fn on_arrival(&mut self, gap: usize) {
        self.peak = (self.peak * (1.0 - PEAK_DECAY)).max(gap as f32);
        self.clamp();
    }

    /// The ring ran dry anyway: the link has more jitter than measured.
    pub fn on_underrun(&mut self) {
        self.peak += self.frame as f32;
        self.clamp();
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
        let mut c = Cushion::new(BURST, FRAME);
        for _ in 0..1000 {
            c.on_arrival(FRAME);
        }
        assert_eq!(c.target(), BURST + FRAME + FRAME);
    }

    #[test]
    fn the_cushion_covers_the_worst_gap_seen() {
        let mut c = Cushion::new(BURST, FRAME);
        c.on_arrival(GAP_24MS);
        // After the burst a callback pulls, the ring still holds the gap.
        assert!(c.target() - BURST >= GAP_24MS);
    }

    #[test]
    fn a_post_gap_burst_fits_under_the_trim() {
        let mut c = Cushion::new(BURST, FRAME);
        c.on_arrival(GAP_24MS);
        // The gap's worth arrives at once on top of a full cushion.
        assert!(c.target() + GAP_24MS <= c.trim_above());
    }

    #[test]
    fn the_peak_decays_after_a_bad_patch() {
        let mut c = Cushion::new(BURST, FRAME);
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
        let mut c = Cushion::new(BURST, FRAME);
        let before = c.target();
        c.on_underrun();
        assert_eq!(c.target(), before + FRAME);
        for _ in 0..100 {
            c.on_underrun();
        }
        assert_eq!(c.target(), BURST + MAX_PEAK_FRAMES * FRAME + FRAME);
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
