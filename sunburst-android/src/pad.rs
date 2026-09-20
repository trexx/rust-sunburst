// SPDX-License-Identifier: GPL-2.0-or-later

//! Server→client pad output on the client: applying an inbound rumble or
//! rich pad-output frame to the physical controller.
//!
//! [`PadSink`] is the seam the GIP bridge implements in Stage B — writing motor
//! and effect reports back to an Xbox pad over the wireless adapter or wired USB.
//! It is a plain trait so the routing above it is host-testable with a recording
//! fake, which is why this module is not `#[cfg(target_os = "android")]`.
//!
//! [`PadOutputRouter`] sits in front of the sink and enforces the
//! unreliable-channel contract the server relies on (see
//! [`sunburst_core::proto::rumble`]): a reordered stale frame is dropped, and a
//! motor that has gone unheard past [`RUMBLE_TIMEOUT_MS`](sunburst_core::proto::rumble::RUMBLE_TIMEOUT_MS)
//! is stopped — so a lost final zero-level packet cannot leave a controller
//! buzzing in someone's hands.

use sunburst_core::proto::input::MAX_PADS;
use sunburst_core::proto::padoutput::PadOutput;
use sunburst_core::proto::rumble::{Rumble, RumbleTracker};

/// Applies decoded pad output to the physical controller.
pub trait PadSink {
    /// Set a pad's motor levels (low, high, and the Xbox trigger motors).
    fn rumble(&mut self, r: Rumble);
    /// Apply a rich output frame (motors + adaptive triggers + LED).
    fn pad_output(&mut self, o: PadOutput);
}

/// Deduplicates and times out server→client pad output before it reaches a
/// [`PadSink`].
pub struct PadOutputRouter<S: PadSink> {
    sink: S,
    tracker: RumbleTracker,
    /// The last applied rich-output sequence per pad, for latest-wins dedup of
    /// [`PadOutput`] (whose `seq` is separate from rumble's).
    out_seq: [Option<u8>; MAX_PADS as usize],
}

impl<S: PadSink> PadOutputRouter<S> {
    pub fn new(sink: S) -> PadOutputRouter<S> {
        PadOutputRouter {
            sink,
            tracker: RumbleTracker::new(),
            out_seq: [None; MAX_PADS as usize],
        }
    }

    /// A rumble frame arrived: apply it unless a newer one already has.
    pub fn on_rumble(&mut self, r: Rumble, now_ms: u32) {
        if let Some(level) = self.tracker.apply(r, now_ms) {
            self.sink.rumble(level);
        }
    }

    /// A rich pad-output frame arrived: apply it unless it is a reordered stale
    /// frame (modular compare on its own `seq`, like the rumble tracker).
    pub fn on_pad_output(&mut self, o: PadOutput) {
        let Some(slot) = self.out_seq.get_mut(o.pad_index as usize) else {
            return;
        };
        if let Some(prev) = *slot {
            let forward = o.seq.wrapping_sub(prev);
            if forward == 0 || forward >= 0x80 {
                return;
            }
        }
        *slot = Some(o.seq);
        self.sink.pad_output(o);
    }

    /// Periodic: stop any motor that has gone unheard past the timeout.
    pub fn tick(&mut self, now_ms: u32) {
        for pad in self.tracker.expired(now_ms) {
            self.sink.rumble(Rumble {
                pad_index: pad,
                ..Default::default()
            });
        }
    }

    /// The underlying sink, for the bridge to reach its hardware handle.
    pub fn sink_mut(&mut self) -> &mut S {
        &mut self.sink
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sunburst_core::proto::rumble::RUMBLE_TIMEOUT_MS;

    #[derive(Default)]
    struct Recorder {
        rumbles: Vec<Rumble>,
        outputs: Vec<PadOutput>,
    }
    impl PadSink for Recorder {
        fn rumble(&mut self, r: Rumble) {
            self.rumbles.push(r);
        }
        fn pad_output(&mut self, o: PadOutput) {
            self.outputs.push(o);
        }
    }

    fn rumble(pad: u8, level: u16, seq: u8) -> Rumble {
        Rumble {
            pad_index: pad,
            motor_low: level,
            seq,
            ..Default::default()
        }
    }

    #[test]
    fn stale_rumble_is_dropped_and_a_motor_is_stopped_on_timeout() {
        let mut r = PadOutputRouter::new(Recorder::default());
        r.on_rumble(rumble(0, 100, 1), 0);
        r.on_rumble(rumble(0, 200, 3), 10);
        r.on_rumble(rumble(0, 999, 2), 20); // reordered, stale — dropped
        assert_eq!(r.sink_mut().rumbles.len(), 2, "the stale frame was applied");

        // Nothing more arrives; the timeout must stop the motor exactly once.
        r.tick(RUMBLE_TIMEOUT_MS + 10);
        let last = r.sink_mut().rumbles.last().copied().expect("a stop");
        assert!(last.is_silent(), "the motor was not stopped");
        assert_eq!(last.pad_index, 0);
        let n = r.sink_mut().rumbles.len();
        r.tick(RUMBLE_TIMEOUT_MS + 5000);
        assert_eq!(r.sink_mut().rumbles.len(), n, "stopped more than once");
    }

    #[test]
    fn reordered_pad_output_is_dropped_per_pad() {
        let mut r = PadOutputRouter::new(Recorder::default());
        let out = |pad: u8, seq: u8| PadOutput {
            pad_index: pad,
            seq,
            ..Default::default()
        };
        r.on_pad_output(out(1, 5));
        r.on_pad_output(out(1, 4)); // stale — dropped
        r.on_pad_output(out(1, 6));
        // A different pad is tracked independently.
        r.on_pad_output(out(2, 1));
        let outs = &r.sink_mut().outputs;
        assert_eq!(outs.len(), 3);
        assert_eq!(outs[0].seq, 5);
        assert_eq!(outs[1].seq, 6);
        assert_eq!(outs[2].pad_index, 2);
    }
}
