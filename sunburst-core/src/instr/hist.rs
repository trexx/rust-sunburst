// SPDX-License-Identifier: GPL-2.0-or-later

//! Fixed-bucket log-linear histogram, HDR-style.
//!
//! Percentiles without retaining samples and without allocating after
//! construction. Buckets are laid out as a fixed number of linear sub-buckets
//! per power of two, which gives constant *relative* error across the whole
//! range — the property that matters when the same table has to show a 200 ns
//! packetize step and a 9 ms encode.
//!
//! Sized so that 1.5% error is comfortably finer than the differences worth
//! arguing about: 0.3 ms on a 9 ms encode is 3%, and the PR gate needs to see
//! that.

/// Linear sub-buckets per octave. 64 gives at most 1/64 ≈ 1.6% relative error.
const SUB_BITS: u32 = 6;
const SUB_COUNT: usize = 1 << SUB_BITS;

/// Smallest resolved value, 2^6 = 64 ns. Anything faster is two reads of the
/// same clock tick and is clamped.
const MIN_OCTAVE: u32 = 6;
/// Largest resolved value, 2^30 ≈ 1.07 s. Anything slower is a stall, and its
/// exact size is not the interesting part.
const MAX_OCTAVE: u32 = 30;

const OCTAVES: usize = (MAX_OCTAVE - MIN_OCTAVE + 1) as usize;

/// Total buckets: 25 octaves × 64 = 1600, or 6.4 KiB per histogram.
pub const BUCKETS: usize = OCTAVES * SUB_COUNT;

/// Duration histogram in nanoseconds.
#[derive(Clone)]
pub struct Histogram {
    buckets: Box<[u32]>,
    count: u64,
    max_ns: u64,
    min_ns: u64,
}

impl Default for Histogram {
    fn default() -> Self {
        Self::new()
    }
}

impl Histogram {
    pub fn new() -> Histogram {
        Histogram {
            buckets: vec![0u32; BUCKETS].into_boxed_slice(),
            count: 0,
            max_ns: 0,
            min_ns: u64::MAX,
        }
    }

    /// Add one observation. Allocation-free.
    pub fn record(&mut self, ns: u64) {
        let idx = bucket_of(ns);
        self.buckets[idx] = self.buckets[idx].saturating_add(1);
        self.count += 1;
        // Tracked exactly rather than read back out of the buckets, so the
        // extremes are never blurred by quantisation.
        self.max_ns = self.max_ns.max(ns);
        self.min_ns = self.min_ns.min(ns);
    }

    pub fn clear(&mut self) {
        self.buckets.fill(0);
        self.count = 0;
        self.max_ns = 0;
        self.min_ns = u64::MAX;
    }

    /// Fold `other` into `self`. Used to sum the per-interval histograms that
    /// make up a rolling window.
    pub fn merge(&mut self, other: &Histogram) {
        for (a, b) in self.buckets.iter_mut().zip(other.buckets.iter()) {
            *a = a.saturating_add(*b);
        }
        self.count += other.count;
        self.max_ns = self.max_ns.max(other.max_ns);
        self.min_ns = self.min_ns.min(other.min_ns);
    }

    pub fn count(&self) -> u64 {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn max(&self) -> u64 {
        self.max_ns
    }

    pub fn min(&self) -> u64 {
        if self.count == 0 { 0 } else { self.min_ns }
    }

    /// Percentile in nanoseconds, `p` in 0..=100.
    ///
    /// Returns each bucket's **upper** bound, so the answer never understates
    /// the latency — the bias a latency budget wants. Clamped to the largest
    /// value actually seen, because a p99 above the observed maximum reads as a
    /// bug even when it is only quantisation.
    pub fn percentile(&self, p: f64) -> u64 {
        if self.count == 0 {
            return 0;
        }
        let target = ((p / 100.0) * self.count as f64).ceil() as u64;
        let target = target.clamp(1, self.count);

        let mut cumulative = 0u64;
        for (i, &c) in self.buckets.iter().enumerate() {
            cumulative += u64::from(c);
            if cumulative >= target {
                return bucket_upper(i).min(self.max_ns);
            }
        }
        self.max_ns
    }
}

/// Bucket index for a duration.
fn bucket_of(ns: u64) -> usize {
    let floor = 1u64 << MIN_OCTAVE;
    if ns < floor {
        return 0;
    }
    let octave = 63 - ns.leading_zeros();
    if octave > MAX_OCTAVE {
        return BUCKETS - 1;
    }
    // Drop the implicit leading 1 and keep the next SUB_BITS bits. The shift is
    // safe because `octave >= MIN_OCTAVE == SUB_BITS`.
    let sub = ((ns >> (octave - SUB_BITS)) & (SUB_COUNT as u64 - 1)) as usize;
    (octave - MIN_OCTAVE) as usize * SUB_COUNT + sub
}

/// Smallest value that lands in `idx`.
fn bucket_lower(idx: usize) -> u64 {
    let octave = MIN_OCTAVE + (idx / SUB_COUNT) as u32;
    let sub = (idx % SUB_COUNT) as u64;
    (1u64 << octave) | (sub << (octave - SUB_BITS))
}

/// Largest value that lands in `idx`.
fn bucket_upper(idx: usize) -> u64 {
    let octave = MIN_OCTAVE + (idx / SUB_COUNT) as u32;
    let width = 1u64 << (octave - SUB_BITS);
    bucket_lower(idx) + width - 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_bounds_are_consistent() {
        for idx in 0..BUCKETS {
            let lo = bucket_lower(idx);
            let hi = bucket_upper(idx);
            assert!(lo <= hi, "bucket {idx} is inverted: {lo}..{hi}");
            assert_eq!(bucket_of(lo), idx, "lower bound of {idx} maps elsewhere");
            assert_eq!(bucket_of(hi), idx, "upper bound of {idx} maps elsewhere");
            if idx + 1 < BUCKETS {
                assert_eq!(
                    hi + 1,
                    bucket_lower(idx + 1),
                    "gap or overlap after bucket {idx}"
                );
            }
        }
    }

    #[test]
    fn relative_error_stays_under_two_percent() {
        // The claim the whole layout rests on. Checked across the full range
        // rather than at a couple of convenient points.
        let mut ns = 1u64 << MIN_OCTAVE;
        while ns < 1u64 << MAX_OCTAVE {
            let idx = bucket_of(ns);
            let err = (bucket_upper(idx) - bucket_lower(idx)) as f64 / ns as f64;
            assert!(
                err < 0.02,
                "{ns} ns lands in a bucket {:.3}% wide",
                err * 100.0
            );
            ns = ns + (ns / 37).max(1); // irregular stride, to avoid only hitting boundaries
        }
    }

    #[test]
    fn out_of_range_values_clamp_rather_than_panic() {
        assert_eq!(bucket_of(0), 0);
        assert_eq!(bucket_of(1), 0);
        assert_eq!(bucket_of(u64::MAX), BUCKETS - 1);
    }

    #[test]
    fn percentiles_track_a_known_distribution() {
        let mut h = Histogram::new();
        // 1..=1000 µs, so the answers are known by construction.
        for i in 1..=1000u64 {
            h.record(i * 1_000);
        }
        assert_eq!(h.count(), 1000);

        // Within one bucket width of the true value at each point.
        for (p, expect_us) in [(50.0, 500u64), (95.0, 950), (99.0, 990)] {
            let got_us = h.percentile(p) / 1_000;
            let err = got_us.abs_diff(expect_us) as f64 / expect_us as f64;
            assert!(err < 0.02, "p{p} was {got_us}µs, expected ~{expect_us}µs");
        }
    }

    #[test]
    fn min_and_max_are_exact_not_quantised() {
        let mut h = Histogram::new();
        h.record(1_234_567);
        h.record(7_654_321);
        h.record(3_000_000);
        assert_eq!(h.min(), 1_234_567);
        assert_eq!(h.max(), 7_654_321);
    }

    #[test]
    fn percentile_never_exceeds_the_observed_max() {
        // Quantisation pushes bucket upper bounds past the real maximum; a p99
        // above anything ever measured reads as a bug, so it is clamped.
        let mut h = Histogram::new();
        for _ in 0..100 {
            h.record(9_000_001);
        }
        assert_eq!(h.percentile(99.0), 9_000_001);
        assert_eq!(h.percentile(100.0), 9_000_001);
    }

    #[test]
    fn empty_histogram_reports_zero_rather_than_panicking() {
        let h = Histogram::new();
        assert!(h.is_empty());
        assert_eq!(h.percentile(99.0), 0);
        assert_eq!(h.max(), 0);
        assert_eq!(h.min(), 0);
    }

    #[test]
    fn merge_sums_counts_and_widens_extremes() {
        let mut a = Histogram::new();
        let mut b = Histogram::new();
        for i in 1..=100u64 {
            a.record(i * 1_000);
        }
        for i in 101..=200u64 {
            b.record(i * 1_000);
        }
        a.merge(&b);
        assert_eq!(a.count(), 200);
        assert_eq!(a.min(), 1_000);
        assert_eq!(a.max(), 200_000);
    }
}
