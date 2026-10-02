// SPDX-License-Identifier: GPL-2.0-or-later

//! When the AAudio callback may play from the PCM ring — pure, host-tested.
//!
//! The ring used to play from the first sample: the callback popped whatever
//! was there and zero-filled the rest. AAudio pulls a whole burst at once
//! (384 frames, 8 ms, on the Homatics) while packets bring 2.5–5 ms each, so
//! the ring sat near empty, and any Wi-Fi jitter came out as a short run of
//! silence, a click. Measured on the box: the ring touched empty in nearly
//! every two-second window.
//!
//! So playback waits until the ring holds a cushion: one burst plus two frames
//! to start with. If it runs dry anyway, it waits to refill (rebuffers) and the
//! cushion grows by a frame, up to a cap, so a link that needs more slack gets
//! it once rather than clicking repeatedly. The producer trims to the cushion
//! (plus a little slack) rather than to a fixed watermark, so latency follows
//! the cushion too. All sizes are interleaved samples, as the ring counts them.

/// Frames of slack the cushion starts with above one burst.
const START_FRAMES: usize = 2;
/// The most frames of slack it grows to.
const MAX_FRAMES: usize = 8;
/// Frames above the cushion the producer lets the ring hold before dropping.
const TRIM_SLACK_FRAMES: usize = 4;

/// The callback side's state.
#[derive(Debug)]
pub struct Playout {
    burst: usize,
    frame: usize,
    target: usize,
    primed: bool,
}

impl Playout {
    /// `burst` is AAudio's callback size and `frame` one decoded packet, both
    /// in interleaved samples.
    pub fn new(burst: usize, frame: usize) -> Playout {
        Playout {
            burst,
            frame,
            target: burst + START_FRAMES * frame,
            primed: false,
        }
    }

    /// One callback, with `available` samples in the ring: whether to play
    /// from it. Waiting plays silence and leaves the ring to fill.
    pub fn should_play(&mut self, available: usize) -> bool {
        if !self.primed && available >= self.target {
            self.primed = true;
        }
        self.primed
    }

    /// The callback popped fewer samples than it needed: refill before playing
    /// again, and keep a bigger cushion from now on.
    pub fn underrun(&mut self) {
        self.primed = false;
        self.target = (self.target + self.frame).min(self.burst + MAX_FRAMES * self.frame);
    }

    /// The cushion, in samples.
    pub fn target(&self) -> usize {
        self.target
    }

    /// Past this many samples in the ring, the producer drops a decoded frame
    /// rather than queueing it: the client is running slow, or a burst arrived
    /// after a stall, and holding it all would only add latency.
    pub fn trim_above(target: usize, frame: usize) -> usize {
        target + TRIM_SLACK_FRAMES * frame
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BURST: usize = 768; // 384 stereo frames
    const FRAME: usize = 240; // 2.5 ms stereo at 48 kHz

    #[test]
    fn playback_waits_for_the_cushion() {
        let mut p = Playout::new(BURST, FRAME);
        assert_eq!(p.target(), BURST + 2 * FRAME);
        assert!(!p.should_play(0));
        assert!(!p.should_play(BURST + 2 * FRAME - 1));
        assert!(p.should_play(BURST + 2 * FRAME));
        // Once primed it plays down to empty, without re-waiting.
        assert!(p.should_play(10));
    }

    #[test]
    fn an_underrun_rebuffers_and_grows_the_cushion() {
        let mut p = Playout::new(BURST, FRAME);
        assert!(p.should_play(BURST + 2 * FRAME));
        p.underrun();
        assert_eq!(p.target(), BURST + 3 * FRAME);
        assert!(
            !p.should_play(BURST + 2 * FRAME),
            "waits for the bigger cushion"
        );
        assert!(p.should_play(BURST + 3 * FRAME));
    }

    #[test]
    fn the_cushion_is_capped() {
        let mut p = Playout::new(BURST, FRAME);
        for _ in 0..50 {
            p.underrun();
        }
        assert_eq!(p.target(), BURST + MAX_FRAMES * FRAME);
    }

    #[test]
    fn the_trim_sits_a_few_frames_above_the_cushion() {
        assert_eq!(Playout::trim_above(1248, FRAME), 1248 + 4 * FRAME);
    }
}
