// SPDX-License-Identifier: GPL-2.0-or-later

//! Sample collection and aggregation, off the frame path.
//!
//! The rings hold timestamps; durations only exist once someone pairs a stage
//! with the one before it. That pairing happens here, on a low-priority thread,
//! so the frame path never pays for it.

use std::time::Duration;

use super::clock;
use super::hist::Histogram;
use super::ring::{self, Sample};
use super::stage::{STAGE_COUNT, Stage};

/// Frames tracked concurrently. `frame_id` is a `u16` on the wire, so masking
/// into 256 slots means a frame is only evicted once 256 newer ones have
/// started — by which point it is complete or lost.
const FRAME_SLOTS: usize = 256;

/// Per-interval histograms retained. With a one-second interval this is an
/// eight-second rolling window.
const WINDOW_INTERVALS: usize = 8;

/// Timings for one frame, in flight.
struct FrameSlot {
    frame_id: u32,
    /// Bit per stage, so "have I seen this one" costs no sentinel value.
    present: u16,
    in_use: bool,
    ticks: [u64; STAGE_COUNT],
}

impl FrameSlot {
    fn empty() -> FrameSlot {
        FrameSlot {
            frame_id: 0,
            present: 0,
            in_use: false,
            ticks: [0; STAGE_COUNT],
        }
    }

    #[inline]
    fn has(&self, stage_idx: usize) -> bool {
        self.present & (1 << stage_idx) != 0
    }
}

/// Per-stage statistics over the rolling window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StageStats {
    pub stage: Stage,
    pub count: u64,
    pub p50_ns: u64,
    pub p95_ns: u64,
    pub p99_ns: u64,
    pub max_ns: u64,
}

/// A readout of the whole pipeline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Report {
    pub stages: Vec<StageStats>,
    /// Samples the rings discarded. Non-zero means the numbers above are
    /// thinned, and should be read as suspect rather than merely approximate.
    pub dropped_samples: u64,
    /// Per-thread breakdown of the above, for threads that dropped anything.
    ///
    /// Which thread is overrunning its ring is the actionable half; a total on
    /// its own only tells you to distrust the table.
    pub dropped_by_thread: Vec<(&'static str, u32)>,
    /// Threads that wanted a ring after the registry filled.
    pub unregistered_threads: u32,
}

impl Report {
    /// Stats for one stage, if it was observed at all.
    pub fn stage(&self, stage: Stage) -> Option<&StageStats> {
        self.stages.iter().find(|s| s.stage == stage)
    }

    /// Whether anything about this run makes the numbers untrustworthy.
    pub fn is_lossy(&self) -> bool {
        self.dropped_samples > 0 || self.unregistered_threads > 0
    }
}

/// Pairs samples into durations and aggregates them.
///
/// Single-threaded by construction: one collector drains every ring.
pub struct Collector {
    frames: Box<[FrameSlot]>,
    /// `[stage][interval]`, flattened.
    ///
    /// A ring of per-interval histograms rather than differences between
    /// cumulative snapshots. Same window, but the maximum is windowable too —
    /// a cumulative maximum cannot be subtracted back out, so the snapshot
    /// formulation would have had to report a lifetime max beside a windowed
    /// p99 and hope nobody compared them.
    hist: Vec<Histogram>,
    current: usize,
}

impl Default for Collector {
    fn default() -> Self {
        Self::new()
    }
}

impl Collector {
    pub fn new() -> Collector {
        Collector {
            frames: (0..FRAME_SLOTS)
                .map(|_| FrameSlot::empty())
                .collect::<Vec<_>>()
                .into_boxed_slice(),
            hist: (0..STAGE_COUNT * WINDOW_INTERVALS)
                .map(|_| Histogram::new())
                .collect(),
            current: 0,
        }
    }

    /// Drain every registered ring in timestamp order. Allocation-free.
    pub fn poll(&mut self) {
        // Borrow the fields separately so the closure does not hold `&mut self`
        // while `ingest` needs the same fields. The merge hands samples over one
        // at a time, so nothing is buffered and nothing is allocated.
        let frames = &mut *self.frames;
        let hist = &mut self.hist[..];
        let current = self.current;
        // Bound the merge at "now": a sample stamped after this point may still
        // be mid-publication on another thread, and taking it would let it jump
        // ahead of an earlier one.
        let until = clock::now();
        ring::drain_merged(until, |s| Self::ingest(frames, hist, current, s));
    }

    /// Place one sample and emit any duration it completes.
    ///
    /// Free function over the fields rather than `&mut self`, because the
    /// closure passed to `drain_all` already borrows `self`.
    fn ingest(frames: &mut [FrameSlot], hist: &mut [Histogram], current: usize, s: Sample) {
        let Some(stage) = Stage::from_u8(s.stage_id) else {
            // An id from a newer build than this one. Ignore rather than
            // panic — the drain thread outliving a rolling upgrade is not worth
            // taking the process down for.
            return;
        };
        let idx = stage as usize;
        let slot = &mut frames[(s.frame_id as usize) & (FRAME_SLOTS - 1)];

        if !slot.in_use || slot.frame_id != s.frame_id {
            // A newer frame has claimed this slot. Whatever the old one had is
            // either already aggregated or was never going to complete.
            //
            // This relies on `frame_id` increasing monotonically, which
            // PROTOCOL.md guarantees: it wraps at 16 bits, so an id repeats only
            // after 65536 frames, by which point these 256 slots have turned
            // over 256 times and the comparison above has already evicted it.
            //
            // Re-recording the *same* stage for the same frame is therefore not
            // treated as a new frame, and must not be: subframe readback emits
            // several `EncodeUnitOut` per frame, one per slice or tile, and each
            // is a real measurement against the same `EncodeSubmit`.
            slot.frame_id = s.frame_id;
            slot.present = 0;
            slot.in_use = true;
        }

        slot.ticks[idx] = s.ticks;
        slot.present |= 1 << idx;

        // A duration is emitted when the *second* of a pair arrives, whichever
        // that turns out to be. Rings are merged in time order, but a sample can
        // still straddle a poll (stamped before one, published after), so a
        // successor can occasionally be ingested first.
        //
        // A negative pair is dropped, never recorded as zero. It is not a
        // measurement: it is the next unit of a subframe frame meeting the
        // previous unit's later stage (`EncodeUnitOut` #2 after `Packetize` #1),
        // or two threads' clocks disagreeing by a hair. Recording those as 0 ns
        // filled the packetize and send rows with fake zeros and pulled their
        // p50 toward nothing.
        if !stage.starts_chain()
            && slot.has(idx - 1)
            && let Some(d) = s.ticks.checked_sub(slot.ticks[idx - 1])
        {
            Self::emit(hist, current, stage, d);
        }
        if idx + 1 < STAGE_COUNT {
            let next = Stage::ALL[idx + 1];
            if !next.starts_chain()
                && slot.has(idx + 1)
                && let Some(d) = slot.ticks[idx + 1].checked_sub(s.ticks)
            {
                Self::emit(hist, current, next, d);
            }
        }
    }

    fn emit(hist: &mut [Histogram], current: usize, stage: Stage, ticks: u64) {
        hist[stage as usize * WINDOW_INTERVALS + current].record(clock::ticks_to_ns(ticks));
    }

    /// Close the current interval and begin the next, discarding the oldest.
    pub fn rotate(&mut self) {
        self.current = (self.current + 1) % WINDOW_INTERVALS;
        for stage_idx in 0..STAGE_COUNT {
            self.hist[stage_idx * WINDOW_INTERVALS + self.current].clear();
        }
    }

    /// Summarise the rolling window.
    ///
    /// Allocates, deliberately and off the frame path — this runs when a human
    /// or the PR gate asks, not per frame.
    pub fn report(&self) -> Report {
        let mut stages = Vec::with_capacity(STAGE_COUNT);
        let mut merged = Histogram::new();
        for stage in Stage::ALL {
            merged.clear();
            for interval in 0..WINDOW_INTERVALS {
                merged.merge(&self.hist[stage as usize * WINDOW_INTERVALS + interval]);
            }
            if merged.is_empty() {
                // A stage nobody instrumented is absent from the table rather
                // than present as a row of zeros. Zeros invite the reader to
                // believe the stage is free.
                continue;
            }
            stages.push(StageStats {
                stage,
                count: merged.count(),
                p50_ns: merged.percentile(50.0),
                p95_ns: merged.percentile(95.0),
                p99_ns: merged.percentile(99.0),
                max_ns: merged.max(),
            });
        }
        let mut dropped_by_thread = Vec::new();
        ring::for_each_ring(|name, dropped| {
            if dropped > 0 {
                dropped_by_thread.push((name, dropped));
            }
        });
        // Threads that have exited and had their rings reclaimed still count:
        // a session that overran its ring should not look clean afterwards.
        let retired = ring::retired_dropped();
        if retired > 0 {
            dropped_by_thread.push(("retired", retired));
        }

        Report {
            stages,
            dropped_samples: dropped_by_thread.iter().map(|(_, d)| u64::from(*d)).sum(),
            dropped_by_thread,
            unregistered_threads: ring::unregistered_threads(),
        }
    }
}

/// Run a collector on a background thread until the handle is dropped.
///
/// The thread runs at low priority: it must never preempt a stage thread, and
/// it has an entire interval to do a few milliseconds of work.
pub fn spawn(poll_every: Duration, rotate_every: Duration) -> DrainHandle {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;

    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel::<mpsc::Sender<Report>>();

    let thread_stop = Arc::clone(&stop);
    let handle = std::thread::Builder::new()
        .name("sunburst-instr-drain".into())
        .spawn(move || {
            lower_priority();
            let mut collector = Collector::new();
            let mut since_rotate = Duration::ZERO;
            while !thread_stop.load(Ordering::Relaxed) {
                std::thread::sleep(poll_every);
                collector.poll();
                since_rotate += poll_every;
                if since_rotate >= rotate_every {
                    collector.rotate();
                    since_rotate = Duration::ZERO;
                }
                // Answer any pending readout requests.
                while let Ok(reply) = rx.try_recv() {
                    let _ = reply.send(collector.report());
                }
            }
            collector.poll();
            while let Ok(reply) = rx.try_recv() {
                let _ = reply.send(collector.report());
            }
        })
        .expect("failed to spawn instrumentation drain thread");

    DrainHandle {
        stop,
        request: tx,
        thread: Some(handle),
    }
}

/// Owns the drain thread; stops it on drop.
pub struct DrainHandle {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    request: std::sync::mpsc::Sender<std::sync::mpsc::Sender<Report>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl DrainHandle {
    /// Ask the drain thread for the current window. Returns `None` if it has
    /// already stopped.
    pub fn report(&self) -> Option<Report> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.request.send(tx).ok()?;
        rx.recv().ok()
    }
}

impl Drop for DrainHandle {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(windows)]
fn lower_priority() {
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn GetCurrentThread() -> isize;
        fn SetThreadPriority(thread: isize, priority: i32) -> i32;
    }
    const THREAD_PRIORITY_LOWEST: i32 = -2;
    // SAFETY: GetCurrentThread returns a pseudo-handle that needs no closing,
    // and SetThreadPriority only reads it.
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_LOWEST) };
}

#[cfg(unix)]
fn lower_priority() {
    // SAFETY: setpriority on the calling thread (`who` = 0) cannot fail in a way
    // that matters here — without privilege it simply does not raise priority,
    // and we are only ever lowering.
    unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, 10) };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ingest directly, bypassing the rings, so the pairing logic can be tested
    /// without threads or timing.
    fn feed(c: &mut Collector, stage: Stage, frame_id: u32, ticks: u64) {
        Collector::ingest(
            &mut c.frames,
            &mut c.hist,
            c.current,
            Sample::new(stage, frame_id, ticks),
        );
    }

    /// A timestamp fed to the collector, in ticks. One tick is one nanosecond
    /// only on Linux; the ingest path converts ticks to ns with the platform
    /// clock (QPC on Windows), so value assertions compare against [`dur_ns`]
    /// rather than the raw tick count — otherwise they fail on Windows CI.
    fn ns(n: u64) -> u64 {
        n
    }

    /// The nanoseconds the collector records for a duration of `n` ticks —
    /// identity on Linux, QPC-scaled on Windows — mirroring the
    /// `clock::ticks_to_ns` the ingest path applies. Value assertions below run
    /// their expected value and tolerance through this so they hold on both.
    fn dur_ns(n: u64) -> u64 {
        clock::ticks_to_ns(n)
    }

    #[test]
    fn pairs_adjacent_stages_into_durations() {
        let mut c = Collector::new();
        feed(&mut c, Stage::CaptureAcquire, 1, ns(0));
        feed(&mut c, Stage::ColorConvert, 1, ns(1_000_000));

        let r = c.report();
        let convert = r.stage(Stage::ColorConvert).expect("convert missing");
        assert_eq!(convert.count, 1);
        assert!(
            convert.p50_ns.abs_diff(dur_ns(1_000_000)) < dur_ns(20_000),
            "expected ~1ms, got {}ns",
            convert.p50_ns
        );

        // The chain-opening stage has no predecessor, so it contributes no
        // duration of its own.
        assert!(r.stage(Stage::CaptureAcquire).is_none());
    }

    #[test]
    fn pairs_regardless_of_arrival_order() {
        // A sample stamped before one poll but published after it is ingested
        // after later samples from other threads, so a successor really can be
        // ingested before its predecessor.
        let mut c = Collector::new();
        feed(&mut c, Stage::ColorConvert, 1, ns(1_000_000));
        feed(&mut c, Stage::CaptureAcquire, 1, ns(0));

        let convert = c
            .report()
            .stage(Stage::ColorConvert)
            .cloned()
            .expect("missing");
        assert_eq!(
            convert.count, 1,
            "out-of-order pair should still be counted"
        );
        assert!(convert.p50_ns.abs_diff(dur_ns(1_000_000)) < dur_ns(20_000));
    }

    #[test]
    fn a_pair_is_counted_exactly_once() {
        let mut c = Collector::new();
        for frame in 0..10 {
            feed(&mut c, Stage::CaptureAcquire, frame, ns(0));
            feed(&mut c, Stage::ColorConvert, frame, ns(1_000_000));
        }
        assert_eq!(c.report().stage(Stage::ColorConvert).unwrap().count, 10);
    }

    #[test]
    fn does_not_bridge_the_two_machines() {
        // Send -> Recv spans clocks with no shared epoch. Subtracting them would
        // produce a confident, meaningless number.
        let mut c = Collector::new();
        feed(&mut c, Stage::Send, 1, ns(5_000_000));
        feed(&mut c, Stage::Recv, 1, ns(9_999_999_999));

        assert!(
            c.report().stage(Stage::Recv).is_none(),
            "wire time must not be derived by subtraction"
        );
    }

    #[test]
    fn client_chain_measures_normally() {
        let mut c = Collector::new();
        feed(&mut c, Stage::Recv, 1, ns(0));
        feed(&mut c, Stage::JitterOut, 1, ns(2_000_000));
        feed(&mut c, Stage::DecodeSubmit, 1, ns(2_100_000));

        let r = c.report();
        assert!(r.stage(Stage::JitterOut).is_some());
        assert!(r.stage(Stage::DecodeSubmit).is_some());
    }

    #[test]
    fn frame_slots_recycle_without_mixing_frames() {
        let mut c = Collector::new();
        // Two frames exactly FRAME_SLOTS apart share a slot.
        feed(&mut c, Stage::CaptureAcquire, 1, ns(0));
        feed(
            &mut c,
            Stage::CaptureAcquire,
            1 + FRAME_SLOTS as u32,
            ns(500_000_000),
        );
        // The second frame's convert must pair with the second capture, not the
        // first — otherwise this reads as a half-second stage.
        feed(
            &mut c,
            Stage::ColorConvert,
            1 + FRAME_SLOTS as u32,
            ns(501_000_000),
        );

        let convert = c
            .report()
            .stage(Stage::ColorConvert)
            .cloned()
            .expect("missing");
        assert_eq!(convert.count, 1);
        assert!(
            convert.p50_ns.abs_diff(dur_ns(1_000_000)) < dur_ns(20_000),
            "slot recycling mixed two frames: got {}ns",
            convert.p50_ns
        );
    }

    #[test]
    fn repeated_stage_within_a_frame_measures_each_unit() {
        // Subframe readback emits one `EncodeUnitOut` per slice or tile — four
        // for a 2x2 AV1 tiling. Each is a real measurement against the same
        // submit, and treating the repeat as a new frame would throw three of
        // them away.
        let mut c = Collector::new();
        feed(&mut c, Stage::EncodeSubmit, 1, ns(0));
        for unit in 1..=4u64 {
            feed(&mut c, Stage::EncodeUnitOut, 1, ns(unit * 2_000_000));
        }

        let units = c
            .report()
            .stage(Stage::EncodeUnitOut)
            .cloned()
            .expect("missing");
        assert_eq!(units.count, 4, "every emitted unit should be measured");
        assert!(
            units.max_ns.abs_diff(dur_ns(8_000_000)) < dur_ns(200_000),
            "the last unit should read ~8ms, got {}ns",
            units.max_ns
        );

        // The pipeline continues from the frame's submit, not from a reset slot.
        feed(&mut c, Stage::Packetize, 1, ns(9_000_000));
        assert!(c.report().stage(Stage::Packetize).is_some());
    }

    #[test]
    fn frame_id_wrapping_evicts_the_previous_occupant() {
        // Production behaviour: `frame_id` is a u16 on the wire, so it wraps
        // through zero. The slot for frame 0 was last used by frame 65280.
        let mut c = Collector::new();
        feed(&mut c, Stage::CaptureAcquire, 65280, ns(0));
        feed(&mut c, Stage::ColorConvert, 65280, ns(1_000_000));

        feed(&mut c, Stage::CaptureAcquire, 0, ns(900_000_000));
        feed(&mut c, Stage::ColorConvert, 0, ns(901_000_000));

        let convert = c
            .report()
            .stage(Stage::ColorConvert)
            .cloned()
            .expect("missing");
        assert_eq!(convert.count, 2);
        assert!(
            convert.max_ns < dur_ns(2_000_000),
            "the wrap paired two different frames: max {}ns",
            convert.max_ns
        );
    }

    #[test]
    fn incomplete_frames_contribute_nothing() {
        let mut c = Collector::new();
        for frame in 0..50 {
            feed(&mut c, Stage::CaptureAcquire, frame, ns(0));
            // ColorConvert never arrives.
        }
        assert!(c.report().stages.is_empty());
    }

    #[test]
    fn rotation_ages_data_out_of_the_window() {
        let mut c = Collector::new();
        feed(&mut c, Stage::CaptureAcquire, 1, ns(0));
        feed(&mut c, Stage::ColorConvert, 1, ns(1_000_000));
        assert_eq!(c.report().stage(Stage::ColorConvert).unwrap().count, 1);

        // One full lap of the interval ring clears it.
        for _ in 0..WINDOW_INTERVALS {
            c.rotate();
        }
        assert!(
            c.report().stage(Stage::ColorConvert).is_none(),
            "the window should have aged this out"
        );
    }

    #[test]
    fn unknown_stage_ids_are_ignored_not_fatal() {
        let mut c = Collector::new();
        Collector::ingest(
            &mut c.frames,
            &mut c.hist,
            c.current,
            Sample::raw(200, 1, 0),
        );
        assert!(c.report().stages.is_empty());
    }

    #[test]
    fn clock_going_backwards_drops_the_pair_rather_than_wrapping() {
        // Two threads reading QPC can observe a tiny inversion. Wrapping would
        // turn that into a ~584-year duration in the p99 column, and clamping to
        // zero would plant a fake 0 ns sample — neither is a measurement.
        let mut c = Collector::new();
        feed(&mut c, Stage::CaptureAcquire, 1, ns(1_000_000));
        feed(&mut c, Stage::ColorConvert, 1, ns(999_000));

        assert!(
            c.report().stage(Stage::ColorConvert).is_none(),
            "an inverted pair must be dropped, not recorded"
        );
    }

    #[test]
    fn subframe_units_do_not_plant_zero_samples() {
        // The shape the pipeline really produces: each slice/tile is emitted and
        // packetized before the next one completes. Unit #2's EncodeUnitOut meets
        // unit #1's Packetize, which is *later* in stage order but *earlier* in
        // time. That used to be recorded as a 0 ns packetize sample, three per
        // four-unit frame, which halved the packetize row's p50.
        let mut c = Collector::new();
        feed(&mut c, Stage::EncodeSubmit, 1, ns(0));
        for unit in 1..=4u64 {
            feed(&mut c, Stage::EncodeUnitOut, 1, ns(unit * 2_000_000));
            feed(&mut c, Stage::Packetize, 1, ns(unit * 2_000_000 + 100_000));
        }

        let r = c.report();
        let pk = r.stage(Stage::Packetize).expect("packetize missing");
        assert_eq!(pk.count, 4, "one packetize duration per unit, no extras");
        assert!(
            pk.p50_ns.abs_diff(dur_ns(100_000)) < dur_ns(20_000),
            "every packetize sample is a real ~100us, got p50 {}ns",
            pk.p50_ns
        );
        assert_eq!(r.stage(Stage::EncodeUnitOut).expect("units").count, 4);
    }
}
