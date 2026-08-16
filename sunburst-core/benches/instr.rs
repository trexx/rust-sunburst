// SPDX-License-Identifier: GPL-2.0-or-later

//! Cost of the one function the frame path calls.
//!
//! Phase 1's budget is under 50 ns per `record`, **including its own clock
//! read** — measuring the store alone and quoting that would be measuring the
//! cheap half. `clock_only` is here so the split is visible: on current hardware
//! the clock read is most of the total, and that is the number to attack if the
//! budget is ever missed.

use std::time::Duration;

use criterion::{Criterion, criterion_group, criterion_main};
use sunburst_core::instr::{self, Collector, Stage};

/// Keep a collector draining in the background for the duration of the run.
///
/// Without this the rings fill, `push` starts taking its drop branch, and the
/// benchmark reports a number that is *better* than reality — the failure mode
/// where the measurement flatters exactly what it is meant to police.
struct Drainer {
    _handle: instr::DrainHandle,
}

impl Drainer {
    fn start() -> Drainer {
        Drainer {
            _handle: instr::spawn(Duration::from_millis(1), Duration::from_secs(1)),
        }
    }
}

fn bench_record(c: &mut Criterion) {
    let _drainer = Drainer::start();
    instr::register_thread("bench");

    let mut group = c.benchmark_group("instr");

    group.bench_function("record", |b| {
        let mut frame_id = 0u32;
        b.iter(|| {
            instr::record(Stage::CaptureAcquire, frame_id);
            frame_id = frame_id.wrapping_add(1);
        });
    });

    group.bench_function("clock_only", |b| {
        b.iter(|| std::hint::black_box(instr::clock::now()));
    });

    group.finish();
}

/// The drain side, for completeness: it runs off the frame path, but a collector
/// that cannot keep up with 60 fps across a dozen threads would show up as
/// dropped samples rather than as latency, and dropped samples make every other
/// number suspect.
fn bench_collector(c: &mut Criterion) {
    c.bench_function("collector/poll_1000_samples", |b| {
        let mut collector = Collector::new();
        b.iter(|| {
            for frame_id in 0..200u32 {
                instr::record(Stage::CaptureAcquire, frame_id);
                instr::record(Stage::ColorConvert, frame_id);
                instr::record(Stage::EncodeSubmit, frame_id);
                instr::record(Stage::EncodeUnitOut, frame_id);
                instr::record(Stage::Packetize, frame_id);
            }
            collector.poll();
        });
    });
}

criterion_group!(benches, bench_record, bench_collector);
criterion_main!(benches);
