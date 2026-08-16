// SPDX-License-Identifier: GPL-2.0-or-later

//! Synthetic five-stage pipeline, end to end.
//!
//! Phase 1's acceptance criterion: a pipeline that exists only to be measured
//! reports stable p99s. This drives the real path — `record` into the real
//! thread-local rings, drained by a real `Collector` — rather than feeding the
//! collector directly, which is what the unit tests in `instr::drain` already do.
//!
//! Its own test binary, because the ring registry is process-wide and a test
//! draining in parallel would steal these samples.

use std::time::{Duration, Instant};

use sunburst_core::instr::{self, Collector, Stage};

/// The five stages of the server chain that a capture-to-wire pipeline walks.
const PIPELINE: [Stage; 5] = [
    Stage::CaptureAcquire,
    Stage::ColorConvert,
    Stage::EncodeSubmit,
    Stage::EncodeUnitOut,
    Stage::Packetize,
];

/// Roughly how long each simulated stage takes.
const STAGE_WORK: Duration = Duration::from_micros(200);

const FRAMES: u32 = 200;

/// Burn time without sleeping. `thread::sleep` has millisecond-scale granularity
/// on some platforms, which would swamp the durations being measured.
fn spin(d: Duration) {
    let start = Instant::now();
    while start.elapsed() < d {
        std::hint::spin_loop();
    }
}

/// Run `frames` frames starting at `first_id`.
///
/// Ids continue across calls rather than restarting. The collector's frame
/// table keys on `frame_id`, and PROTOCOL.md guarantees the id increases
/// monotonically (wrapping at 16 bits), so replaying the same ids would be the
/// test violating the contract rather than an interesting case.
fn run_pipeline(first_id: u32, frames: u32) {
    instr::register_thread("synthetic");
    for frame_id in first_id..first_id + frames {
        for stage in PIPELINE {
            instr::record(stage, frame_id);
            spin(STAGE_WORK);
        }
    }
}

#[test]
fn synthetic_pipeline_reports_stable_percentiles() {
    let mut collector = Collector::new();

    // First window.
    run_pipeline(0, FRAMES);
    collector.poll();
    let first = collector.report();

    // Second window, same work, continuing the id sequence. Rotating between
    // them is not needed: the interval ring only ages data out, and both runs
    // belong to one window.
    run_pipeline(FRAMES, FRAMES);
    collector.poll();
    let second = collector.report();

    assert!(
        !first.is_lossy(),
        "rings dropped samples, so these numbers are thinned: {} dropped, {} unregistered threads",
        first.dropped_samples,
        first.unregistered_threads
    );

    // Every stage but the chain opener yields a duration; the opener has no
    // predecessor to measure against.
    for stage in PIPELINE.into_iter().filter(|s| !s.starts_chain()) {
        let a = first
            .stage(stage)
            .unwrap_or_else(|| panic!("{} missing from the first report", stage.name()));
        let b = second
            .stage(stage)
            .unwrap_or_else(|| panic!("{} missing from the second report", stage.name()));

        assert_eq!(
            a.count,
            FRAMES as u64,
            "{}: expected one duration per frame",
            stage.name()
        );
        assert_eq!(
            b.count,
            2 * FRAMES as u64,
            "{}: second window should include both runs",
            stage.name()
        );

        // Ordering within a stage is arithmetic, not luck: if this fails the
        // percentile walk is wrong, not the timing.
        assert!(
            a.p50_ns <= a.p95_ns && a.p95_ns <= a.p99_ns && a.p99_ns <= a.max_ns,
            "{}: percentiles out of order: p50={} p95={} p99={} max={}",
            stage.name(),
            a.p50_ns,
            a.p95_ns,
            a.p99_ns,
            a.max_ns
        );

        // The band is deliberately wide. This asserts the plumbing carries a
        // plausible number, not that a spin loop on a shared CI runner hits
        // 200µs — a tight bound here would be a flaky test pretending to be a
        // latency measurement.
        assert!(
            a.p50_ns > STAGE_WORK.as_nanos() as u64 / 4,
            "{}: p50 of {}ns is far below the {}µs of work injected",
            stage.name(),
            a.p50_ns,
            STAGE_WORK.as_micros()
        );
        assert!(
            a.p50_ns < 50_000_000,
            "{}: p50 of {}ns is not a sub-frame stage",
            stage.name(),
            a.p50_ns
        );

        // Stability: the same work twice should not move the median much. p99
        // is deliberately not asserted this way — it is a tail, and one
        // scheduler preemption legitimately moves it.
        let ratio = a.p50_ns.max(b.p50_ns) as f64 / a.p50_ns.min(b.p50_ns) as f64;
        assert!(
            ratio < 3.0,
            "{}: median moved {ratio:.1}x between identical runs ({} then {})",
            stage.name(),
            a.p50_ns,
            b.p50_ns
        );
    }

    // The chain opener is recorded but yields no duration, having no
    // predecessor to measure against.
    assert!(
        first.stage(Stage::CaptureAcquire).is_none(),
        "the first stage in a chain has nothing to subtract from"
    );
}

// Deliberately the only test in this binary. `record` and `drain_all` share one
// process-wide registry, so a second test draining in parallel would consume
// these samples and both would fail confusingly.
