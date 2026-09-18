// SPDX-License-Identifier: GPL-2.0-or-later

//! The client's one-way-delay gradient estimator, and the tick unwrapper that
//! feeds it.
//!
//! A video header carries the low 32 bits of the sender's tick counter at
//! capture time. The client turns that into nanoseconds on the sender's clock
//! (`SessionConfig.qpc_freq_hz` says how), subtracts it from its own receive
//! time, and gets a one-way delay offset by an unknown constant — the two
//! machines share no epoch. The *slope* of that quantity over time is what the
//! rate controller wants, and the constant differentiates away.
//!
//! Least squares over a short window rather than a two-point difference, so a
//! single late packet does not read as a trend.

/// Samples kept for the fit. At one per frame that is a second at 60 fps; the
/// window is shorter than that in time anyway.
const SAMPLES: usize = 64;

/// Turns the header's 32-bit tick into monotonic nanoseconds.
///
/// Consecutive samples are assumed to be less than 2³¹ ticks apart — at a
/// 10 MHz counter that is 3½ minutes, against frames 16 ms apart.
pub struct TickUnwrap {
    freq_hz: u64,
    last: Option<u32>,
    high: u64,
}

impl TickUnwrap {
    pub fn new(freq_hz: u64) -> TickUnwrap {
        TickUnwrap {
            freq_hz: freq_hz.max(1),
            last: None,
            high: 0,
        }
    }

    /// The tick, unwrapped and converted, in nanoseconds of the sender's clock.
    pub fn to_ns(&mut self, tick: u32) -> i64 {
        if let Some(last) = self.last
            && tick < last
            && last - tick > u32::MAX / 2
        {
            self.high += 1 << 32;
        }
        self.last = Some(tick);
        let ticks = self.high + tick as u64;
        // Split to keep the multiply from overflowing at high tick counts.
        let secs = ticks / self.freq_hz;
        let rem = ticks % self.freq_hz;
        (secs * 1_000_000_000 + rem * 1_000_000_000 / self.freq_hz) as i64
    }
}

/// Least-squares slope of one-way delay against receive time.
pub struct OwdGradient {
    /// `(recv_ns, owd_ns)`.
    samples: [(i64, i64); SAMPLES],
    next: usize,
    len: usize,
    window_ns: i64,
}

impl OwdGradient {
    /// `window_ns` is how far back the fit looks.
    pub fn new(window_ns: u64) -> OwdGradient {
        OwdGradient {
            samples: [(0, 0); SAMPLES],
            next: 0,
            len: 0,
            window_ns: window_ns.max(1) as i64,
        }
    }

    /// A frame sent at `send_ns` (sender clock) arrived at `recv_ns` (client
    /// clock). The clocks' offset is irrelevant; only its change matters.
    pub fn push(&mut self, send_ns: i64, recv_ns: i64) {
        self.samples[self.next] = (recv_ns, recv_ns.wrapping_sub(send_ns));
        self.next = (self.next + 1) % SAMPLES;
        self.len = (self.len + 1).min(SAMPLES);
    }

    /// The gradient in microseconds of delay per second of time, over the
    /// samples inside the window. Zero until there are two of them.
    pub fn slope_us_per_s(&self) -> i32 {
        let newest = if self.len == 0 {
            return 0;
        } else {
            self.samples[(self.next + SAMPLES - 1) % SAMPLES].0
        };
        let cutoff = newest - self.window_ns;

        // Two passes in f64, relative to the newest sample so the sums stay
        // small. Sixty-four points at 10/s is nothing.
        let (mut n, mut sx, mut sy) = (0.0f64, 0.0f64, 0.0f64);
        for &(t, d) in self.samples[..self.len].iter() {
            if t < cutoff {
                continue;
            }
            n += 1.0;
            sx += (t - newest) as f64;
            sy += d as f64;
        }
        if n < 2.0 {
            return 0;
        }
        let (mx, my) = (sx / n, sy / n);
        let (mut sxx, mut sxy) = (0.0f64, 0.0f64);
        for &(t, d) in self.samples[..self.len].iter() {
            if t < cutoff {
                continue;
            }
            let dx = (t - newest) as f64 - mx;
            sxx += dx * dx;
            sxy += dx * (d as f64 - my);
        }
        if sxx <= 0.0 {
            return 0;
        }
        // ns per ns → µs per s is ×1e6 / 1e3.
        let slope = sxy / sxx * 1_000_000.0;
        slope.clamp(i32::MIN as f64, i32::MAX as f64) as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: i64 = 1_000_000;

    #[test]
    fn a_constant_offset_has_no_slope() {
        let mut g = OwdGradient::new(100 * MS as u64);
        for k in 0..20 {
            let t = k * 16 * MS;
            g.push(t - 5_000 * MS, t); // clocks five seconds apart, steady
        }
        assert_eq!(g.slope_us_per_s(), 0);
    }

    #[test]
    fn a_growing_queue_reads_as_its_growth_rate() {
        // Delay grows 1 ms every 100 ms: 10 000 µs/s.
        let mut g = OwdGradient::new(200 * MS as u64);
        for k in 0..20i64 {
            let t = k * 16 * MS;
            let delay = t / 100; // 1 % of elapsed time
            g.push(t - delay, t);
        }
        let s = g.slope_us_per_s();
        assert!((9_900..=10_100).contains(&s), "slope {s}");
    }

    #[test]
    fn a_draining_queue_is_negative_and_one_outlier_is_not_a_trend() {
        let mut g = OwdGradient::new(200 * MS as u64);
        for k in 0..20i64 {
            let t = k * 16 * MS;
            g.push(t + t / 200, t); // delay shrinking 0.5 %
        }
        assert!(g.slope_us_per_s() < -4_500);

        let mut g = OwdGradient::new(200 * MS as u64);
        for k in 0..12i64 {
            let t = k * 16 * MS;
            let late = if k == 6 { 8 * MS } else { 0 };
            g.push(t - late, t);
        }
        assert!(g.slope_us_per_s().abs() < 5_000, "one late frame is noise");
    }

    #[test]
    fn samples_outside_the_window_are_ignored() {
        let mut g = OwdGradient::new(50 * MS as u64);
        // Old history with a steep slope...
        for k in 0..10i64 {
            let t = k * 16 * MS;
            g.push(t - t / 10, t);
        }
        // ...then a flat recent stretch a long time later.
        for k in 0..4i64 {
            let t = 10_000 * MS + k * 16 * MS;
            g.push(t - 1000 * MS, t);
        }
        assert_eq!(g.slope_us_per_s(), 0);
    }

    #[test]
    fn ticks_unwrap_across_the_32_bit_boundary() {
        let mut u = TickUnwrap::new(10_000_000); // 100 ns per tick
        assert_eq!(u.to_ns(0), 0);
        assert_eq!(u.to_ns(10), 1_000);
        let before = u.to_ns(u32::MAX - 5);
        let after = u.to_ns(4); // wrapped: 10 ticks later
        assert_eq!(after - before, 1_000);
        // A small backwards step (reordering) is not a wrap.
        let back = u.to_ns(2);
        assert_eq!(after - back, 200);
    }
}
