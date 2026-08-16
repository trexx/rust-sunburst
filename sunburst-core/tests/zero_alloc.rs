// SPDX-License-Identifier: GPL-2.0-or-later

//! Phase 1's zero-allocation acceptance criterion, enforced by a counting
//! allocator rather than by reading the code.
//!
//! CLAUDE.md's first hot-path rule is zero allocation after warmup. This is the
//! test that keeps it true — a `Vec` that can grow or a stray `format!` added to
//! the frame path three phases from now fails here rather than showing up as
//! jitter on a TV.
//!
//! Its own test binary for two reasons: the `#[global_allocator]` below applies
//! to every thread in the process, and the ring registry is process-wide.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use sunburst_core::instr::{self, Collector, Stage};

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

struct Counting;

// SAFETY: every method forwards to `System` unchanged, with the same contract
// and the same pointers. The only addition is a relaxed counter increment, which
// cannot affect allocation behaviour.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: `layout` is forwarded unchanged from our caller, who owes
        // `System` exactly the contract they owed us.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: as in `alloc`.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: `ptr` was returned by our `alloc` — which is `System`'s — with
        // this `layout`, and is forwarded unchanged.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: as in `realloc`; every pointer we hand back came from
        // `System`, so `System` is the right allocator to return it to.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

const PIPELINE: [Stage; 5] = [
    Stage::CaptureAcquire,
    Stage::ColorConvert,
    Stage::EncodeSubmit,
    Stage::EncodeUnitOut,
    Stage::Packetize,
];

#[test]
fn frame_path_allocates_nothing_after_warmup() {
    // Warmup. The first `record` on a thread allocates its ring, once, and the
    // collector allocates its frame table and histograms up front. Both are
    // startup costs by design; the claim is about steady state.
    instr::register_thread("zero-alloc");
    let mut collector = Collector::new();
    for frame_id in 0..1_000u32 {
        for stage in PIPELINE {
            instr::record(stage, frame_id);
        }
        // Drain as we go here too: 1000 frames of five stages is 5000 samples
        // against a 4096-slot ring, so a single drain at the end would overflow
        // it and start the measured section already lossy.
        if frame_id % 512 == 0 {
            collector.poll();
        }
    }
    collector.poll();

    // Steady state.
    let before = ALLOCATIONS.load(Ordering::Relaxed);

    for frame_id in 0..20_000u32 {
        for stage in PIPELINE {
            instr::record(stage, frame_id);
        }
        // Drain as we go, so the ring never fills and starts dropping — which
        // would make this pass for the wrong reason.
        if frame_id % 512 == 0 {
            collector.poll();
        }
    }
    collector.poll();
    collector.rotate();

    let allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;
    assert_eq!(
        allocations, 0,
        "the frame path allocated {allocations} times after warmup"
    );

    // Prove the run was real: 100k records, none dropped. Without this the test
    // would pass just as happily against a `record` that did nothing at all.
    let report = collector.report();
    assert!(
        !report.is_lossy(),
        "samples were dropped, so the zero-allocation result is not meaningful"
    );
    let convert = report
        .stage(Stage::ColorConvert)
        .expect("no timings collected, so nothing was actually exercised");
    assert!(
        convert.count >= 20_000,
        "expected at least one duration per frame, got {}",
        convert.count
    );
}
