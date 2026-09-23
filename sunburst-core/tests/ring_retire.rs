// SPDX-License-Identifier: GPL-2.0-or-later

//! Rings are reclaimed when their thread exits, so the registry never fills.
//!
//! Every session spawns fresh stage threads. Before reclamation the sixteen
//! registry slots were gone after a handful of sessions and every later thread
//! went uninstrumented — the PR gate's p99 table quietly lost its rows.
//!
//! Its own test binary, because the ring registry is process-wide.

use sunburst_core::instr::{self, Collector, MAX_THREADS, Stage};

#[test]
fn exited_threads_give_their_slots_back() {
    let mut collector = Collector::new();
    let threads = MAX_THREADS as u32 * 2 + 8;

    for i in 0..threads {
        std::thread::spawn(move || {
            instr::register_thread("short-lived");
            instr::record(Stage::CaptureAcquire, i);
            instr::record(Stage::ColorConvert, i);
        })
        .join()
        .expect("producer panicked");
        // The thread has fully exited (join waits for its thread-local
        // destructors), so its ring is retired: this poll drains and frees it.
        collector.poll();
    }

    let report = collector.report();
    assert_eq!(
        report.unregistered_threads, 0,
        "a thread went unregistered, so slots were not being reclaimed"
    );
    assert!(!report.is_lossy(), "{report:?}");
    let convert = report
        .stage(Stage::ColorConvert)
        .expect("no durations collected");
    assert_eq!(
        convert.count,
        u64::from(threads),
        "every exited thread's samples should have been collected"
    );
}
