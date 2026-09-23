// SPDX-License-Identifier: GPL-2.0-or-later

//! A thread that finds the registry full must not allocate on every sample.
//!
//! It used to: with no slot free, `record` retried registration each call —
//! allocating a 64 KB ring, failing to place it, freeing it — on the frame path,
//! for every sample. That broke CLAUDE.md's zero-allocation rule on every stage
//! thread once enough sessions had come and gone.
//!
//! Its own test binary: the counting allocator is process-wide, and so is the
//! ring registry.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, mpsc};

use sunburst_core::instr::{self, MAX_THREADS, Stage};

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

struct Counting;

// SAFETY: every method forwards to `System` unchanged; the only addition is a
// relaxed counter increment, which cannot affect allocation behaviour.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: `layout` is forwarded unchanged.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: as in `alloc`.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: `ptr` came from `System` with this `layout`.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: every pointer we hand back came from `System`.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

#[test]
fn a_full_registry_costs_the_frame_path_nothing() {
    // Fill every slot with a live thread that holds it until released.
    let registered = Arc::new(Barrier::new(MAX_THREADS + 1));
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let release_rx = Arc::new(std::sync::Mutex::new(release_rx));
    let holders: Vec<_> = (0..MAX_THREADS)
        .map(|_| {
            let registered = Arc::clone(&registered);
            let release_rx = Arc::clone(&release_rx);
            std::thread::spawn(move || {
                instr::register_thread("holder");
                registered.wait();
                let _ = release_rx.lock().expect("release lock").recv();
            })
        })
        .collect();
    registered.wait();

    // One more thread. Its first `record` finds no slot; neither that attempt
    // nor any of the thousands after it may allocate.
    let allocations = std::thread::spawn(|| {
        let before = ALLOCATIONS.load(Ordering::Relaxed);
        for frame_id in 0..10_000u32 {
            instr::record(Stage::CaptureAcquire, frame_id);
            instr::record(Stage::ColorConvert, frame_id);
        }
        ALLOCATIONS.load(Ordering::Relaxed) - before
    })
    .join()
    .expect("recording thread panicked");

    assert_eq!(
        allocations, 0,
        "an unregistered thread allocated {allocations} times recording samples"
    );
    assert_eq!(
        instr::Collector::new().report().unregistered_threads,
        1,
        "the failed thread should be counted once, not once per sample"
    );

    for _ in 0..MAX_THREADS {
        let _ = release_tx.send(());
    }
    for h in holders {
        h.join().expect("holder panicked");
    }
}
