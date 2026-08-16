// SPDX-License-Identifier: GPL-2.0-or-later

//! The background drain thread, end to end.
//!
//! `Collector` is unit-tested directly; this covers the part that only exists
//! once a thread is involved — that it actually collects, answers a readout
//! request, and shuts down cleanly when the handle drops.
//!
//! Its own test binary, because the ring registry is process-wide.

use std::time::{Duration, Instant};

use sunburst_core::instr::{self, Stage};

#[test]
fn drain_thread_collects_and_stops() {
    let handle = instr::spawn(Duration::from_millis(2), Duration::from_secs(30));

    instr::register_thread("producer");
    for frame_id in 0..300u32 {
        instr::record(Stage::CaptureAcquire, frame_id);
        instr::record(Stage::ColorConvert, frame_id);
        instr::record(Stage::EncodeSubmit, frame_id);
    }

    // Poll until the drain thread has picked the samples up, rather than
    // sleeping a fixed interval and hoping. Bounded so a genuine hang fails the
    // test instead of wedging CI.
    let deadline = Instant::now() + Duration::from_secs(5);
    let report = loop {
        let report = handle
            .report()
            .expect("drain thread should still be running");
        if report
            .stage(Stage::ColorConvert)
            .is_some_and(|s| s.count >= 300)
        {
            break report;
        }
        assert!(
            Instant::now() < deadline,
            "drain thread did not collect 300 samples within 5s; got {:?}",
            report.stage(Stage::ColorConvert).map(|s| s.count)
        );
        std::thread::sleep(Duration::from_millis(5));
    };

    assert!(
        !report.is_lossy(),
        "{} samples dropped across {} threads",
        report.dropped_samples,
        report.dropped_by_thread.len()
    );

    let submit = report.stage(Stage::EncodeSubmit).expect("submit missing");
    assert_eq!(submit.count, 300);

    // Dropping the handle stops and joins the thread. If it did not, this test
    // would hang here rather than pass, which is the behaviour worth having.
    drop(handle);
}
