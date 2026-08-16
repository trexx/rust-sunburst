// SPDX-License-Identifier: GPL-2.0-or-later

//! Lock-free SPSC sample ring, one per producer thread.
//!
//! One ring per thread rather than one shared MPSC ring. Capture, encode and
//! net each push from their own thread, and a shared tail would bounce a cache
//! line between them on every frame — which is instrumentation perturbing the
//! thing it is supposed to be measuring. Per-thread rings mean a push touches
//! only lines this thread already owns, except for the one `tail` read that
//! bounds the ring, and the drain thread writes `tail` about once a second.
//!
//! Rings are registered into a fixed global table and **never freed**, even
//! after their thread exits, so the drain thread can collect whatever was left
//! behind. That is fine for the long-lived stage threads this project runs and
//! would be a leak under a thread-per-frame design, which nothing here does.

use core::cell::{Cell, UnsafeCell};
use core::ptr;
use core::sync::atomic::{AtomicPtr, AtomicU32, Ordering};

use super::clock;
use super::stage::Stage;

/// Samples per thread ring. At 60 fps across six server stages a thread
/// produces roughly 360 samples/sec, so this is about 11 seconds of slack
/// against a drain that runs every second.
pub const RING_CAPACITY: usize = 4096;

/// Maximum simultaneously registered producer threads.
pub const MAX_THREADS: usize = 16;

/// One timestamped pipeline event. 16 bytes, so a power-of-two ring never
/// straddles a cache line.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Sample {
    /// Raw monotonic ticks; see [`clock`]. Not nanoseconds on Windows.
    pub ticks: u64,
    pub frame_id: u32,
    pub stage_id: u8,
    _pad: [u8; 3],
}

impl Sample {
    #[inline(always)]
    pub(super) fn new(stage: Stage, frame_id: u32, ticks: u64) -> Self {
        Sample {
            ticks,
            frame_id,
            stage_id: stage as u8,
            _pad: [0; 3],
        }
    }

    /// Construct with an arbitrary stage id, to simulate a sample written by a
    /// build that knows a stage this one does not.
    #[cfg(test)]
    pub(super) fn raw(stage_id: u8, frame_id: u32, ticks: u64) -> Self {
        Sample {
            ticks,
            frame_id,
            stage_id,
            _pad: [0; 3],
        }
    }
}

/// Keeps the two cursors off each other's cache line. Without this, the
/// producer's `head` store invalidates the line the consumer reads `tail` from
/// and vice versa, which is the entire cost this design exists to avoid.
#[repr(align(64))]
struct Padded<T>(T);

pub struct Ring {
    /// Producer-owned write cursor. Free-running; wraps at `u32` and is masked
    /// on use, so wrapping is not a special case.
    head: Padded<AtomicU32>,
    /// Consumer-owned read cursor.
    tail: Padded<AtomicU32>,
    /// Samples discarded because the ring was full.
    ///
    /// Surfaced in the readout rather than swallowed: a histogram that has been
    /// silently thinned is exactly the metric that can be wrong, and those are
    /// worse than no metric at all.
    dropped: Padded<AtomicU32>,
    buf: Box<[UnsafeCell<Sample>]>,
    mask: u32,
    name: &'static str,
}

// SAFETY: the head/tail protocol below is single-producer, single-consumer. The
// producer only ever writes the slot at `head` before publishing it with a
// release store; the consumer only ever reads slots strictly below `head` after
// an acquire load. So no slot is ever touched by both at once.
unsafe impl Sync for Ring {}
// SAFETY: as above — a Ring is only reachable through the registry, which hands
// out shared references.
unsafe impl Send for Ring {}

impl Ring {
    fn new(name: &'static str) -> Ring {
        assert!(
            RING_CAPACITY.is_power_of_two(),
            "RING_CAPACITY must be a power of two for the index mask"
        );
        let buf = (0..RING_CAPACITY)
            .map(|_| UnsafeCell::new(Sample::new(Stage::CaptureAcquire, 0, 0)))
            .collect::<Vec<_>>()
            .into_boxed_slice();
        Ring {
            head: Padded(AtomicU32::new(0)),
            tail: Padded(AtomicU32::new(0)),
            dropped: Padded(AtomicU32::new(0)),
            buf,
            mask: (RING_CAPACITY - 1) as u32,
            name,
        }
    }

    /// Producer side. Never blocks, never allocates, never grows.
    ///
    /// Returns whether the sample was accepted. [`record`] ignores it — there is
    /// nothing useful a frame path can do about a full ring, and the drop is
    /// already counted — but it is not free information, and without it a caller
    /// cannot distinguish "stored" from "silently discarded".
    #[inline(always)]
    fn push(&self, s: Sample) -> bool {
        let head = self.head.0.load(Ordering::Relaxed);
        let tail = self.tail.0.load(Ordering::Acquire);
        if head.wrapping_sub(tail) >= RING_CAPACITY as u32 {
            self.dropped.0.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        let idx = (head & self.mask) as usize;
        // SAFETY: `idx` is masked into range, and the slot is below `head`, which
        // has not been published yet — so the consumer cannot be reading it. The
        // unchecked index is deliberate: LLVM cannot prove `mask + 1 == len`
        // through a boxed slice, and this is the hot path.
        unsafe { self.buf.get_unchecked(idx).get().write(s) };
        self.head.0.store(head.wrapping_add(1), Ordering::Release);
        true
    }

    /// Consumer side. Returns `None` when drained.
    fn pop(&self) -> Option<Sample> {
        let tail = self.tail.0.load(Ordering::Relaxed);
        let head = self.head.0.load(Ordering::Acquire);
        if tail == head {
            return None;
        }
        let idx = (tail & self.mask) as usize;
        // SAFETY: `idx` is masked into range, and `tail != head` means this slot
        // was published by the producer's release store, which the acquire load
        // above synchronises with.
        let s = unsafe { self.buf.get_unchecked(idx).get().read() };
        self.tail.0.store(tail.wrapping_add(1), Ordering::Release);
        Some(s)
    }

    /// Total samples this ring has discarded for want of space.
    pub fn dropped(&self) -> u32 {
        self.dropped.0.load(Ordering::Relaxed)
    }

    pub fn name(&self) -> &'static str {
        self.name
    }
}

static REGISTRY: [AtomicPtr<Ring>; MAX_THREADS] =
    [const { AtomicPtr::new(ptr::null_mut()) }; MAX_THREADS];

/// Threads that wanted a ring after the registry filled up. Reported alongside
/// dropped samples, for the same reason.
static UNREGISTERED: AtomicU32 = AtomicU32::new(0);

thread_local! {
    static LOCAL: Cell<*const Ring> = const { Cell::new(ptr::null()) };
}

fn register(name: &'static str) -> *const Ring {
    let ptr = Box::into_raw(Box::new(Ring::new(name)));
    for slot in &REGISTRY {
        if slot
            .compare_exchange(ptr::null_mut(), ptr, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            return ptr;
        }
    }
    UNREGISTERED.fetch_add(1, Ordering::Relaxed);
    // SAFETY: `ptr` came from `Box::into_raw` and no registry slot took
    // ownership of it, so this thread is still the only owner.
    drop(unsafe { Box::from_raw(ptr) });
    ptr::null()
}

/// Give this thread's ring a name for the readout.
///
/// Optional — [`record`] registers lazily on first use — but a table of stage
/// timings against `unnamed` is much less use when something is wrong. Call it
/// once at thread start, before the frame path begins.
pub fn register_thread(name: &'static str) {
    LOCAL.with(|local| {
        if local.get().is_null() {
            local.set(register(name));
        }
    });
}

/// Record a stage completing for a frame.
///
/// The whole point of this crate: no allocation, no formatting, no locks, and
/// no logging. The only work is one clock read and one store.
#[inline(always)]
pub fn record(stage: Stage, frame_id: u32) {
    let ticks = clock::now();
    LOCAL.with(|local| {
        let mut p = local.get();
        if p.is_null() {
            // First call on this thread. Allocates once, during warmup, and
            // never again.
            p = register("unnamed");
            local.set(p);
            if p.is_null() {
                return;
            }
        }
        // SAFETY: registry entries are leaked for the lifetime of the process,
        // so a non-null ring pointer stays valid once observed.
        unsafe { (*p).push(Sample::new(stage, frame_id, ticks)) };
    });
}

/// Drain every registered ring, passing each sample to `f`.
///
/// Called by the drain thread. Visits rings in registration order, which does
/// not interleave samples by time — the frame table in [`super::drain`] sorts
/// that out by keying on `frame_id`.
pub fn drain_all(mut f: impl FnMut(Sample)) {
    for slot in &REGISTRY {
        let p = slot.load(Ordering::Acquire);
        if p.is_null() {
            continue;
        }
        // SAFETY: registry entries are leaked and never cleared, so a non-null
        // pointer remains valid.
        let ring = unsafe { &*p };
        while let Some(s) = ring.pop() {
            f(s);
        }
    }
}

/// Visit every registered ring's name and drop count.
///
/// The name is why this is per-ring rather than a single total: when samples
/// start disappearing, the useful question is *which* stage thread is
/// overrunning its ring, and a lone number cannot answer it.
pub fn for_each_ring(mut f: impl FnMut(&'static str, u32)) {
    for slot in &REGISTRY {
        let p = slot.load(Ordering::Acquire);
        if p.is_null() {
            continue;
        }
        // SAFETY: as in `drain_all`.
        let ring = unsafe { &*p };
        f(ring.name(), ring.dropped());
    }
}

/// Threads that asked for a ring after the registry was full.
pub fn unregistered_threads() -> u32 {
    UNREGISTERED.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sample_is_sixteen_bytes() {
        // The ring's freedom from straddling cache lines depends on this.
        assert_eq!(size_of::<Sample>(), 16);
        assert_eq!(align_of::<Sample>(), 8);
    }

    #[test]
    fn cursors_are_on_separate_cache_lines() {
        let r = Ring::new("test");
        let head = &r.head as *const _ as usize;
        let tail = &r.tail as *const _ as usize;
        assert!(
            head.abs_diff(tail) >= 64,
            "head and tail share a cache line, which defeats the design"
        );
    }

    #[test]
    fn push_then_pop_round_trips_in_order() {
        let r = Ring::new("test");
        for i in 0..100 {
            r.push(Sample::new(Stage::Send, i, u64::from(i) * 10));
        }
        for i in 0..100 {
            let s = r.pop().expect("sample should be present");
            assert_eq!(s.frame_id, i);
            assert_eq!(s.ticks, u64::from(i) * 10);
            assert_eq!(s.stage_id, Stage::Send as u8);
        }
        assert!(r.pop().is_none());
    }

    #[test]
    fn overflow_drops_and_counts_rather_than_growing() {
        let r = Ring::new("test");
        for i in 0..(RING_CAPACITY as u32 + 50) {
            r.push(Sample::new(Stage::Send, i, 0));
        }
        assert_eq!(r.dropped(), 50, "excess must be counted, not absorbed");

        // The retained samples are the oldest, and they are intact — dropping
        // the newest keeps the ring readable rather than shredding it.
        for i in 0..RING_CAPACITY as u32 {
            assert_eq!(r.pop().expect("sample").frame_id, i);
        }
        assert!(r.pop().is_none());
    }

    #[test]
    fn cursors_wrap_without_a_special_case() {
        let r = Ring::new("test");
        // Drive head and tail past the u32 boundary in lockstep.
        r.head.0.store(u32::MAX - 2, Ordering::Relaxed);
        r.tail.0.store(u32::MAX - 2, Ordering::Relaxed);
        for i in 0..10 {
            r.push(Sample::new(Stage::Recv, i, 0));
        }
        for i in 0..10 {
            assert_eq!(r.pop().expect("sample").frame_id, i);
        }
    }

    #[test]
    fn spsc_across_real_threads() {
        // The orderings only matter under real concurrency, so exercise them.
        let ring: &'static Ring = Box::leak(Box::new(Ring::new("test")));
        const N: u32 = 200_000;

        let producer = std::thread::spawn(move || {
            let mut sent = 0u32;
            while sent < N {
                // Retry rather than advance on a full ring, so every one of the
                // N samples really is published and the consumer below can wait
                // for all of them. `record` deliberately does not do this — the
                // frame path drops instead of blocking.
                if ring.push(Sample::new(Stage::Send, sent, u64::from(sent))) {
                    sent += 1;
                }
            }
        });

        let mut seen = 0u32;
        while seen < N {
            if let Some(s) = ring.pop() {
                // In order, and never torn: `frame_id` and `ticks` are written
                // as one sample and must still agree on the way out.
                assert_eq!(u64::from(s.frame_id), s.ticks, "torn or reordered sample");
                assert_eq!(s.frame_id, seen, "samples arrived out of order");
                seen += 1;
            }
        }
        producer.join().expect("producer panicked");
        assert_eq!(seen, N, "every published sample should have been consumed");
        // `dropped` is deliberately not asserted here. It counts failed *push
        // attempts*, which is exactly right for `record` — there, one failure is
        // one lost sample — but the retry loop above turns backpressure into
        // repeated attempts, so a non-zero count here means the ring filled, not
        // that anything was lost.
    }

    #[test]
    fn record_registers_lazily_and_is_drainable() {
        // Runs on its own thread so it cannot disturb other tests' rings.
        std::thread::spawn(|| {
            register_thread("test-named");
            record(Stage::CaptureAcquire, 7);
            record(Stage::ColorConvert, 7);

            let mut got = Vec::new();
            drain_all(|s| got.push(s));
            let ours: Vec<_> = got.iter().filter(|s| s.frame_id == 7).collect();
            assert_eq!(ours.len(), 2, "both samples should survive the drain");
            assert!(ours[0].ticks <= ours[1].ticks, "clock went backwards");
        })
        .join()
        .expect("test thread panicked");
    }
}
