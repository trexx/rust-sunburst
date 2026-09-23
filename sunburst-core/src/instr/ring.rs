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
//! Rings live in a fixed global table of [`MAX_THREADS`] slots. A thread's ring
//! is **retired when the thread exits** (a thread-local guard marks it) and the
//! drain reclaims it — collecting whatever the thread left behind first — so the
//! slot is free for the next thread. Every session spawns fresh stage threads,
//! so without reclamation the table filled after a handful of sessions and
//! instrumentation silently went dark.
//!
//! A thread that finds the table full is marked failed and never tries again:
//! retrying on every [`record`] would allocate (and free) a ring per sample,
//! which is the exact hot-path cost this module exists to avoid.

use core::cell::{Cell, UnsafeCell};
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering};

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
    /// Set (Release) by the owning thread as it exits, after its last push. A
    /// consumer that observes it (Acquire) may drain the ring one final time and
    /// free it.
    retired: AtomicBool,
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
            retired: AtomicBool::new(false),
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

    /// Consumer side: the oldest unread sample, without consuming it.
    fn peek(&self) -> Option<Sample> {
        let tail = self.tail.0.load(Ordering::Relaxed);
        let head = self.head.0.load(Ordering::Acquire);
        if tail == head {
            return None;
        }
        let idx = (tail & self.mask) as usize;
        // SAFETY: as in `pop` — the slot is below the published `head`.
        Some(unsafe { self.buf.get_unchecked(idx).get().read() })
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
/// dropped samples, for the same reason. Counted once per thread.
static UNREGISTERED: AtomicU32 = AtomicU32::new(0);

/// Samples dropped by rings that have since been reclaimed, so a thread's
/// overruns stay visible in the report after the thread has gone.
static RETIRED_DROPPED: AtomicU32 = AtomicU32::new(0);

/// Excludes consumers from each other. Reclaiming frees a ring, so a second
/// consumer walking the registry at the same moment would read freed memory.
/// Only the drain side takes it — the frame path never does.
static CONSUMER: AtomicBool = AtomicBool::new(false);

/// This thread's relationship with the registry.
const STATE_NEW: u8 = 0;
const STATE_REGISTERED: u8 = 1;
/// The registry was full. Terminal: see the module docs.
const STATE_FAILED: u8 = 2;
/// The thread is exiting; its ring (if any) is retired.
const STATE_EXITED: u8 = 3;

/// Retires this thread's ring when the thread exits.
struct RetireGuard;

impl Drop for RetireGuard {
    fn drop(&mut self) {
        // `LOCAL` and `STATE` are const and have no destructor, so they stay
        // accessible while other thread-locals are being torn down. Null the
        // pointer first, so a `record` from a later destructor is a no-op
        // rather than a push into a ring the drain may already be freeing.
        let _ = STATE.try_with(|s| s.set(STATE_EXITED));
        let _ = LOCAL.try_with(|local| {
            let p = local.replace(ptr::null());
            if !p.is_null() {
                // SAFETY: a registered ring is only freed after it is retired,
                // and it is being retired right here, by its only producer.
                unsafe { (*p).retired.store(true, Ordering::Release) };
            }
        });
    }
}

thread_local! {
    static LOCAL: Cell<*const Ring> = const { Cell::new(ptr::null()) };
    static STATE: Cell<u8> = const { Cell::new(STATE_NEW) };
    static GUARD: RetireGuard = const { RetireGuard };
}

/// Claim a registry slot for a fresh ring, or `null` if the table is full.
fn register(name: &'static str) -> *const Ring {
    // Check for room first, so a full table costs no allocation at all.
    if REGISTRY
        .iter()
        .all(|slot| !slot.load(Ordering::Acquire).is_null())
    {
        return ptr::null();
    }
    let ptr = Box::into_raw(Box::new(Ring::new(name)));
    for slot in &REGISTRY {
        if slot
            .compare_exchange(ptr::null_mut(), ptr, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            return ptr;
        }
    }
    // Lost a race for the last slot.
    // SAFETY: `ptr` came from `Box::into_raw` and no registry slot took
    // ownership of it, so this thread is still the only owner.
    drop(unsafe { Box::from_raw(ptr) });
    ptr::null()
}

/// Register this thread (once), returning its ring or `null`. Called only when
/// `LOCAL` is null, so this is off the fast path.
fn register_local(local: &Cell<*const Ring>, name: &'static str) -> *const Ring {
    if STATE.with(Cell::get) != STATE_NEW {
        // Failed or exiting: never retry, never allocate.
        return ptr::null();
    }
    // Arm the retire guard before taking a slot. If this thread is already
    // tearing down its thread-locals, the guard is gone and so is the thread.
    if GUARD.try_with(|_| ()).is_err() {
        STATE.with(|s| s.set(STATE_EXITED));
        return ptr::null();
    }
    let p = register(name);
    if p.is_null() {
        STATE.with(|s| s.set(STATE_FAILED));
        UNREGISTERED.fetch_add(1, Ordering::Relaxed);
    } else {
        STATE.with(|s| s.set(STATE_REGISTERED));
        local.set(p);
    }
    p
}

/// Give this thread's ring a name for the readout.
///
/// Optional — [`record`] registers lazily on first use — but a table of stage
/// timings against `unnamed` is much less use when something is wrong. Call it
/// once at thread start, before the frame path begins.
pub fn register_thread(name: &'static str) {
    LOCAL.with(|local| {
        if local.get().is_null() {
            register_local(local, name);
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
            // First call on this thread: allocates once, during warmup. A thread
            // that could not register returns here without allocating.
            p = register_local(local, "unnamed");
            if p.is_null() {
                return;
            }
        }
        // SAFETY: a ring is freed only after its owner retired it, and `LOCAL`
        // is nulled before retirement, so a non-null `LOCAL` is still live.
        unsafe { (*p).push(Sample::new(stage, frame_id, ticks)) };
    });
}

/// Holds the consumer lock for its lifetime.
struct ConsumerGuard;

impl ConsumerGuard {
    fn acquire() -> ConsumerGuard {
        while CONSUMER
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            std::thread::yield_now();
        }
        ConsumerGuard
    }
}

impl Drop for ConsumerGuard {
    fn drop(&mut self) {
        CONSUMER.store(false, Ordering::Release);
    }
}

/// Pop samples from `rings` in timestamp order, up to and including `until`.
///
/// Each ring is already in time order (one producer thread, monotonic clock),
/// so a repeated minimum over the ring heads is a k-way merge — no buffer, no
/// sort, no allocation. Samples newer than `until` stay for the next call, so a
/// sample stamped during this merge cannot jump ahead of one from another
/// thread that is still being published.
fn merge_rings(rings: &[Option<&Ring>], until: u64, f: &mut impl FnMut(Sample)) {
    loop {
        let mut best: Option<(usize, u64)> = None;
        for (i, ring) in rings.iter().enumerate() {
            let Some(ring) = ring else { continue };
            if let Some(s) = ring.peek()
                && s.ticks <= until
                && best.is_none_or(|(_, t)| s.ticks < t)
            {
                best = Some((i, s.ticks));
            }
        }
        let Some((i, _)) = best else { break };
        if let Some(s) = rings[i].and_then(Ring::pop) {
            f(s);
        }
    }
}

/// Drain every registered ring in timestamp order, passing each sample stamped
/// at or before `until` to `f`, then reclaim the rings of exited threads.
///
/// Time order matters: pairing a stage with the one before it is only
/// meaningful if the earlier sample is seen first, and the stages of one frame
/// are recorded on different threads (encode vs send).
pub fn drain_merged(until: u64, mut f: impl FnMut(Sample)) {
    let _consumer = ConsumerGuard::acquire();

    let mut rings: [Option<&Ring>; MAX_THREADS] = [None; MAX_THREADS];
    let mut retired = [false; MAX_THREADS];
    for (i, slot) in REGISTRY.iter().enumerate() {
        let p = slot.load(Ordering::Acquire);
        if !p.is_null() {
            // SAFETY: only a consumer frees a ring, and we hold the consumer
            // lock, so the pointer stays valid for this call.
            let ring = unsafe { &*p };
            // Read the flag before draining: a ring retired *now* has had its
            // last push, so the final drain below sees everything.
            retired[i] = ring.retired.load(Ordering::Acquire);
            rings[i] = Some(ring);
        }
    }

    merge_rings(&rings, until, &mut f);

    for (i, slot) in REGISTRY.iter().enumerate() {
        let Some(ring) = rings[i] else { continue };
        if !retired[i] {
            continue;
        }
        // The owner is gone: take what it left, newer than `until` or not.
        while let Some(s) = ring.pop() {
            f(s);
        }
        let p = ring as *const Ring as *mut Ring;
        if slot
            .compare_exchange(p, ptr::null_mut(), Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            RETIRED_DROPPED.fetch_add(ring.dropped(), Ordering::Relaxed);
            // SAFETY: the slot no longer points at the ring, no producer
            // touches a retired ring, and the consumer lock excludes every other
            // reader — so this is the last reference.
            drop(unsafe { Box::from_raw(p) });
        }
    }
}

/// Visit every registered ring's name and drop count.
///
/// The name is why this is per-ring rather than a single total: when samples
/// start disappearing, the useful question is *which* stage thread is
/// overrunning its ring, and a lone number cannot answer it.
pub fn for_each_ring(mut f: impl FnMut(&'static str, u32)) {
    let _consumer = ConsumerGuard::acquire();
    for slot in &REGISTRY {
        let p = slot.load(Ordering::Acquire);
        if p.is_null() {
            continue;
        }
        // SAFETY: the consumer lock keeps reclamation out while we read.
        let ring = unsafe { &*p };
        f(ring.name(), ring.dropped());
    }
}

/// Threads that asked for a ring after the registry was full.
pub fn unregistered_threads() -> u32 {
    UNREGISTERED.load(Ordering::Relaxed)
}

/// Samples dropped by rings that have since been reclaimed.
pub fn retired_dropped() -> u32 {
    RETIRED_DROPPED.load(Ordering::Relaxed)
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
    fn merge_pops_across_rings_in_time_order_and_leaves_the_future() {
        let a = Ring::new("a");
        let b = Ring::new("b");
        for t in [1u64, 4, 6] {
            a.push(Sample::new(Stage::Packetize, 1, t));
        }
        for t in [2u64, 3, 7] {
            b.push(Sample::new(Stage::Send, 1, t));
        }
        let mut order = Vec::new();
        merge_rings(&[Some(&a), None, Some(&b)], 6, &mut |s| order.push(s.ticks));
        assert_eq!(
            order,
            [1, 2, 3, 4, 6],
            "k-way merge by tick, bounded by `until`"
        );
        // The sample newer than `until` waits for the next pass.
        assert_eq!(b.pop().map(|s| s.ticks), Some(7));
        assert!(a.pop().is_none());
    }

    #[test]
    fn peek_does_not_consume() {
        let r = Ring::new("test");
        r.push(Sample::new(Stage::Send, 9, 5));
        assert_eq!(r.peek().map(|s| s.frame_id), Some(9));
        assert_eq!(r.pop().map(|s| s.frame_id), Some(9));
        assert!(r.peek().is_none());
    }

    #[test]
    fn record_registers_lazily_and_is_drainable() {
        // Runs on its own thread so it cannot disturb other tests' rings.
        std::thread::spawn(|| {
            register_thread("test-named");
            record(Stage::CaptureAcquire, 7);
            record(Stage::ColorConvert, 7);

            let mut got = Vec::new();
            drain_merged(u64::MAX, |s| got.push(s));
            let ours: Vec<_> = got.iter().filter(|s| s.frame_id == 7).collect();
            assert_eq!(ours.len(), 2, "both samples should survive the drain");
            assert!(ours[0].ticks <= ours[1].ticks, "clock went backwards");
        })
        .join()
        .expect("test thread panicked");
    }
}
