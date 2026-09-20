// SPDX-License-Identifier: GPL-2.0-or-later

//! Delay-gradient rate control.
//!
//! The one-way delay gradient the client reports (`Feedback::owd_gradient`,
//! µs/s) is the queue-growth ratio of the path: at a sending rate `R` over a
//! bottleneck `C`, delay grows at `(R − C) / C` seconds per second. That is why
//! it reacts before loss does, and it is also a direct capacity estimate —
//! `C ≈ R / (1 + g)` — so a decrease can land on the answer in one step
//! rather than backing off in fixed fractions until the symptom goes away.
//!
//! Increases are additive and stop short of the last learned ceiling; a slow
//! probe above it is what discovers recovered bandwidth. That is what keeps
//! the controller from sawtoothing around the bottleneck: it converges and
//! then holds, rather than repeatedly re-discovering the same limit.
//!
//! Pure logic, clock as an argument. Runs on the control plane (feedback is
//! 10/s); the frame path only ever reads the resulting target.

use sunburst_core::proto::Feedback;

/// Bitrate limits, kbps. `max` is the decoder's hint, the codec's ceiling and
/// the configured target, whichever is lowest; `min` is where quality stops
/// being worth streaming at all.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Bounds {
    pub min_kbps: u32,
    pub max_kbps: u32,
    pub initial_kbps: u32,
}

/// Gradient above which the path is judged to be queueing (µs/s: 0.5 % overload).
pub const GRADIENT_DOWN_US_PER_S: i32 = 5_000;
/// Gradient below which the path is judged flat enough to try more (µs/s).
pub const GRADIENT_FLAT_US_PER_S: i32 = 1_000;
/// After a decrease, no increase for this long.
pub const HOLD_AFTER_DECREASE_MS: u64 = 1_000;
/// Between additive increases.
pub const INCREASE_INTERVAL_MS: u64 = 500;
/// Additive increase, as a fraction of the current target.
pub const INCREASE_FRACTION: f64 = 0.05;
/// Stay this far under the learned ceiling.
pub const CEILING_MARGIN: f64 = 0.95;
/// How often to probe above the ceiling once holding under it.
pub const PROBE_INTERVAL_MS: u64 = 5_000;
/// Decrease on loss with no usable gradient.
pub const LOSS_DECREASE: f64 = 0.85;
/// Safety factor under the capacity estimate.
pub const ESTIMATE_MARGIN: f64 = 0.9;

pub struct RateController {
    bounds: Bounds,
    target_kbps: u32,
    /// The rate at which queueing was last observed; increases stop short of
    /// it until a probe succeeds.
    ceiling_kbps: Option<u32>,
    last_decrease_ms: Option<u64>,
    last_increase_ms: Option<u64>,
    last_probe_ms: Option<u64>,
    last_dropped: Option<u32>,
    last_loss_ms: Option<u64>,
    decreases: u32,
    increases: u32,
}

impl RateController {
    pub fn new(bounds: Bounds) -> RateController {
        let target = bounds
            .initial_kbps
            .clamp(bounds.min_kbps.min(bounds.max_kbps), bounds.max_kbps);
        RateController {
            bounds,
            target_kbps: target,
            ceiling_kbps: None,
            last_decrease_ms: None,
            last_increase_ms: None,
            last_probe_ms: None,
            last_dropped: None,
            last_loss_ms: None,
            decreases: 0,
            increases: 0,
        }
    }

    pub fn target_kbps(&self) -> u32 {
        self.target_kbps
    }

    /// `(decreases, increases)` so far — for the metrics readout.
    pub fn stats(&self) -> (u32, u32) {
        (self.decreases, self.increases)
    }

    /// Feed one feedback report. Returns the new target when it changed.
    pub fn on_feedback(&mut self, fb: &Feedback, now_ms: u64) -> Option<u32> {
        let before = self.target_kbps;
        if self.last_increase_ms.is_none() {
            // The first report starts the clock: one flat reading is not a
            // reason to push, a flat interval is.
            self.last_increase_ms = Some(now_ms);
        }

        let lost = match self.last_dropped {
            Some(prev) => fb.frames_dropped > prev,
            None => false,
        };
        self.last_dropped = Some(fb.frames_dropped);
        if lost {
            self.last_loss_ms = Some(now_ms);
        }

        if fb.owd_gradient > GRADIENT_DOWN_US_PER_S {
            // Queueing: the gradient says by how much. Land under the capacity
            // it implies, and remember where the limit was.
            let g = fb.owd_gradient as f64 / 1_000_000.0;
            let estimate = self.target_kbps as f64 / (1.0 + g);
            self.ceiling_kbps = Some(self.target_kbps.min(estimate as u32));
            self.set(estimate * ESTIMATE_MARGIN, now_ms, true);
        } else if lost {
            // Loss without a gradient to explain it: back off a fixed step.
            self.ceiling_kbps = Some(self.target_kbps);
            self.set(self.target_kbps as f64 * LOSS_DECREASE, now_ms, true);
        } else if fb.owd_gradient < GRADIENT_FLAT_US_PER_S {
            self.maybe_increase(now_ms);
        }

        (self.target_kbps != before).then_some(self.target_kbps)
    }

    fn maybe_increase(&mut self, now_ms: u64) {
        let held = self
            .last_decrease_ms
            .is_some_and(|t| now_ms.saturating_sub(t) < HOLD_AFTER_DECREASE_MS);
        let lossy = self
            .last_loss_ms
            .is_some_and(|t| now_ms.saturating_sub(t) < HOLD_AFTER_DECREASE_MS);
        let too_soon = self
            .last_increase_ms
            .is_some_and(|t| now_ms.saturating_sub(t) < INCREASE_INTERVAL_MS);
        if held || lossy || too_soon {
            return;
        }

        let step = self.target_kbps as f64 * (1.0 + INCREASE_FRACTION);
        let limit = match self.ceiling_kbps {
            Some(c) => {
                let under = c as f64 * CEILING_MARGIN;
                let probe_due = self
                    .last_probe_ms
                    .is_none_or(|t| now_ms.saturating_sub(t) >= PROBE_INTERVAL_MS);
                if step <= under {
                    under
                } else if probe_due {
                    // One step above the ceiling, then wait to see the
                    // gradient's verdict before another.
                    self.last_probe_ms = Some(now_ms);
                    self.ceiling_kbps = None;
                    f64::MAX
                } else {
                    return; // holding under the ceiling
                }
            }
            None => f64::MAX,
        };
        let next = step.min(limit);
        if next as u32 > self.target_kbps {
            self.set(next, now_ms, false);
        }
    }

    fn set(&mut self, kbps: f64, now_ms: u64, decrease: bool) {
        let clamped = (kbps as u32).clamp(self.bounds.min_kbps, self.bounds.max_kbps);
        if clamped == self.target_kbps {
            return;
        }
        self.target_kbps = clamped;
        if decrease {
            self.last_decrease_ms = Some(now_ms);
            self.decreases += 1;
        } else {
            self.last_increase_ms = Some(now_ms);
            self.increases += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bottleneck of `capacity_kbps` with a queue: the gradient is the
    /// overload ratio while the sender exceeds it, and the queue drains
    /// (negative gradient) once it does not. Drops start once the queue has
    /// grown past a frame's worth for a while.
    struct Link {
        capacity_kbps: u32,
        queue_kbit: f64,
        dropped: u32,
    }

    impl Link {
        fn feedback(&mut self, rate_kbps: u32, dt_ms: u64) -> Feedback {
            let dt = dt_ms as f64 / 1000.0;
            let excess = rate_kbps as f64 - self.capacity_kbps as f64;
            // Growing while over capacity, draining (negative) while under it
            // with a backlog, flat otherwise.
            let gradient = if excess > 0.0 || self.queue_kbit > 0.0 {
                excess / self.capacity_kbps as f64
            } else {
                0.0
            };
            self.queue_kbit = (self.queue_kbit + excess * dt).max(0.0);
            if self.queue_kbit > self.capacity_kbps as f64 * 0.05 {
                self.dropped += 1;
            }
            Feedback {
                owd_gradient: (gradient * 1_000_000.0) as i32,
                frames_dropped: self.dropped,
                ..Default::default()
            }
        }
    }

    fn bounds() -> Bounds {
        Bounds {
            min_kbps: 10_000,
            max_kbps: 150_000,
            initial_kbps: 120_000,
        }
    }

    /// Run `secs` of 100 ms feedbacks, returning the target after each.
    fn run(rc: &mut RateController, link: &mut Link, start_ms: u64, secs: u64) -> Vec<u32> {
        let mut out = Vec::new();
        for i in 0..secs * 10 {
            let now = start_ms + i * 100;
            let fb = link.feedback(rc.target_kbps(), 100);
            rc.on_feedback(&fb, now);
            out.push(rc.target_kbps());
        }
        out
    }

    #[test]
    fn converges_under_a_bottleneck_within_two_seconds() {
        let mut rc = RateController::new(bounds());
        let mut link = Link {
            capacity_kbps: 80_000,
            queue_kbit: 0.0,
            dropped: 0,
        };
        let trace = run(&mut rc, &mut link, 0, 2);
        let under = trace
            .iter()
            .position(|t| *t <= 80_000)
            .expect("never got under");
        assert!(under < 20, "took {under} feedbacks");
        assert!(
            trace[19] >= 60_000,
            "landed far below capacity: {}",
            trace[19]
        );
    }

    #[test]
    fn holds_under_the_bottleneck_without_sawtoothing() {
        let mut rc = RateController::new(bounds());
        let mut link = Link {
            capacity_kbps: 80_000,
            queue_kbit: 0.0,
            dropped: 0,
        };
        run(&mut rc, &mut link, 0, 3);
        let (dec_before, _) = rc.stats();
        let trace = run(&mut rc, &mut link, 3_000, 20);
        let (dec_after, _) = rc.stats();
        assert!(
            dec_after - dec_before <= 4,
            "{} decreases in 20 s of steady bottleneck",
            dec_after - dec_before
        );
        let lo = *trace.iter().min().unwrap();
        let hi = *trace.iter().max().unwrap();
        assert!(lo >= 60_000, "dipped to {lo}");
        assert!(hi <= 84_000, "overshot to {hi}");
        assert!(
            trace.last().unwrap() * 100 >= 80_000 * 85,
            "settled too low: {}",
            trace.last().unwrap()
        );
    }

    #[test]
    fn climbs_back_when_the_bottleneck_lifts() {
        let mut rc = RateController::new(bounds());
        let mut link = Link {
            capacity_kbps: 80_000,
            queue_kbit: 0.0,
            dropped: 0,
        };
        run(&mut rc, &mut link, 0, 5);
        link.capacity_kbps = 1_000_000;
        let trace = run(&mut rc, &mut link, 5_000, 30);
        assert!(
            *trace.last().unwrap() >= 140_000,
            "did not recover: {}",
            trace.last().unwrap()
        );
        assert!(trace.iter().all(|t| *t <= 150_000), "exceeded the bound");
    }

    #[test]
    fn a_clear_path_rises_to_the_bound_and_stops() {
        let mut rc = RateController::new(Bounds {
            initial_kbps: 50_000,
            ..bounds()
        });
        let mut link = Link {
            capacity_kbps: 1_000_000,
            queue_kbit: 0.0,
            dropped: 0,
        };
        let trace = run(&mut rc, &mut link, 0, 30);
        assert_eq!(*trace.last().unwrap(), 150_000);
        assert!(trace.windows(2).all(|w| w[1] >= w[0]), "never decreased");
    }

    #[test]
    fn loss_without_a_gradient_backs_off_a_fixed_step() {
        let mut rc = RateController::new(bounds());
        let flat = Feedback::default();
        assert_eq!(rc.on_feedback(&flat, 0), None);
        let lossy = Feedback {
            frames_dropped: 3,
            ..Default::default()
        };
        assert_eq!(rc.on_feedback(&lossy, 100), Some(102_000));
        // The same count again is not new loss.
        assert_eq!(rc.on_feedback(&lossy, 200), None);
    }

    #[test]
    fn never_leaves_the_bounds() {
        let mut rc = RateController::new(bounds());
        let bad = Feedback {
            owd_gradient: 50_000_000, // 50× overload: estimate ≈ 2 Mbps
            ..Default::default()
        };
        rc.on_feedback(&bad, 0);
        assert_eq!(rc.target_kbps(), 10_000);
        let mut link = Link {
            capacity_kbps: 1_000_000,
            queue_kbit: 0.0,
            dropped: 0,
        };
        let trace = run(&mut rc, &mut link, 1_000, 120);
        assert!(trace.iter().all(|t| (10_000..=150_000).contains(t)));
    }
}
