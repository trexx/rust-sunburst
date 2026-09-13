// SPDX-License-Identifier: GPL-2.0-or-later

//! Present → capture latency, and what happens when the presenter outruns the
//! panel.
//!
//! # The number this exists for
//!
//! CLAUDE.md budgets **~16.7ms for DWM composition** — the largest single line in
//! the latency table, larger than encode — and nothing has ever measured it. It
//! is the entire justification for Phase 7's swapchain hook, and it is why NvFBC
//! was pursued in the first place.
//!
//! Every capture measurement before this one was *throughput*, which is not what
//! this project optimises, and which answered none of that. This measures the
//! interval that contains composition: an application submits a frame, and some
//! time later a capture backend has it. Both ends are QPC timestamps taken in
//! this process.
//!
//! # How to read the result
//!
//! DDA and WGC are both post-composition, so **their figures should land in the
//! same region**. If they do not, the harness is measuring itself rather than the
//! path, and nothing else here should be believed.
//!
//! A DDA figure near 16.7ms confirms the budget. A figure of a few milliseconds
//! says the largest line in the table is wrong — and takes Phase 7's hook with
//! it, since that item exists only to beat composition.

use sunburst_core::instr::clock;

use crate::presenter::{Mode, Presenter, READ_X, READ_Y, Signal};
use crate::readback::SAMPLE_BYTES;
use crate::watch::Watcher;

/// Long enough for ~80 flips at the presenter's 100ms cadence.
const LATENCY_SECS: u64 = 8;
/// Shorter: the stress figure is a rate, and it stabilises quickly.
const STRESS_SECS: u64 = 4;

struct Latency {
    samples: Vec<u64>,
    frames: u64,
    polls: u64,
    errors: u32,
}

impl Latency {
    fn percentile(&self, pct: usize) -> u64 {
        if self.samples.is_empty() {
            return 0;
        }
        let index = (self.samples.len() * pct / 100).min(self.samples.len() - 1);
        self.samples[index]
    }
}

/// Watch for the signal changing, and time each change against the `Present`
/// that caused it.
fn measure(signal: &Signal, watcher: &mut dyn Watcher, secs: u64) -> Latency {
    let mut result = Latency {
        samples: Vec::new(),
        frames: 0,
        polls: 0,
        errors: 0,
    };
    let mut previous = [0u8; SAMPLE_BYTES];
    let mut baseline = false;
    // The last flip already turned into a sample, so one flip cannot produce
    // several.
    let mut attributed = signal.flip_seq.load(std::sync::atomic::Ordering::Acquire);

    let deadline = clock::now() + clock::ticks_per_sec() * secs;
    while clock::now() < deadline {
        let mut current = [0u8; SAMPLE_BYTES];
        result.polls += 1;
        match watcher.poll(&mut current) {
            Ok(false) => continue,
            Err(_) => {
                result.errors += 1;
                if result.errors > 64 {
                    break;
                }
                continue;
            }
            Ok(true) => {}
        }
        let seen_at = clock::now();
        result.frames += 1;

        // The first frame establishes what "unchanged" looks like.
        if !baseline {
            previous = current;
            baseline = true;
            continue;
        }
        if current == previous {
            continue;
        }
        previous = current;

        // Seqlock around the presenter's pair: a flip landing between the two
        // reads would otherwise pair a new timestamp with an old sequence and
        // report a latency that is too small.
        use std::sync::atomic::Ordering::Acquire;
        let before = signal.flip_seq.load(Acquire);
        let flip_qpc = signal.flip_qpc.load(Acquire);
        let after = signal.flip_seq.load(Acquire);
        if before != after || before == attributed {
            continue;
        }
        attributed = before;
        if seen_at > flip_qpc {
            result.samples.push(clock::ticks_to_ns(seen_at - flip_qpc));
        }
    }

    result.samples.sort_unstable();
    result
}

/// Count how many distinct frames a backend actually sees while the presenter
/// changes on every frame.
fn stress(signal: &Signal, watcher: &mut dyn Watcher, secs: u64) -> (u64, u64, f64) {
    use std::sync::atomic::Ordering::Relaxed;
    let mut previous = [0u8; SAMPLE_BYTES];
    let mut baseline = false;
    let mut transitions = 0u64;

    let presents_before = signal.presents.load(Relaxed);
    let started = clock::now();
    let deadline = started + clock::ticks_per_sec() * secs;
    while clock::now() < deadline {
        let mut current = [0u8; SAMPLE_BYTES];
        if !matches!(watcher.poll(&mut current), Ok(true)) {
            continue;
        }
        if !baseline {
            previous = current;
            baseline = true;
            continue;
        }
        if current != previous {
            previous = current;
            transitions += 1;
        }
    }
    let elapsed = clock::ticks_to_ns(clock::now() - started).max(1);
    let presents = signal.presents.load(Relaxed) - presents_before;
    (transitions, presents, elapsed as f64 / 1e9)
}

fn report(name: &str, l: &Latency) {
    if l.samples.is_empty() {
        println!(
            "  {name}: NO SAMPLES -- {} frames from {} polls, {} errors",
            l.frames, l.polls, l.errors
        );
        println!("          The signal never changed. Either the presenter window is");
        println!("          covered, or this backend is not capturing the monitor it is on.");
        return;
    }
    println!(
        "  {name}: p50 {:.2}ms  p99 {:.2}ms  max {:.2}ms   ({} samples, {} frames)",
        l.percentile(50) as f64 / 1e6,
        l.percentile(99) as f64 / 1e6,
        l.samples[l.samples.len() - 1] as f64 / 1e6,
        l.samples.len(),
        l.frames,
    );
    if l.errors > 0 {
        println!("          {} poll errors along the way", l.errors);
    }
}

/// Build each backend in turn. Never two at once: concurrent capture sessions
/// against one desktop measure contention between them, not latency.
fn with_each<F: FnMut(&mut dyn Watcher)>(cuda: Option<&crate::cuda::Cuda>, mut f: F) {
    match crate::watch::Dda::open(READ_X, READ_Y) {
        Ok(mut dda) => f(&mut dda),
        Err(e) => println!("  DDA   : unavailable -- {e}"),
    }
    match crate::watch::Wgc::open(READ_X, READ_Y) {
        Ok(mut wgc) => f(&mut wgc),
        Err(e) => println!("  WGC   : unavailable -- {e}"),
    }
    if let Some(cuda) = cuda {
        // ARGB10 in both colour modes: it worked in each during Phase 0.1, and
        // the 8-bit ToSys path froze silently on an HDR desktop, which is not a
        // failure worth re-inviting.
        match crate::tocuda::Watch::open(cuda, READ_X, READ_Y, true, true) {
            Some(mut nvfbc) => f(&mut nvfbc),
            None => println!("  NvFBC : unavailable -- no keyed ToCuda session"),
        }
    }
}

pub fn run() {
    println!("== Present -> capture latency ==");
    println!("  Reading desktop pixel ({READ_X},{READ_Y}) -- the centre of a topmost");
    println!("  window, inset from the origin so WGC's capture border cannot sit on it.");
    println!("  This interval contains DWM composition, which CLAUDE.md budgets at");
    println!("  ~16.7ms and has never measured.");
    println!();

    let cuda = match crate::cuda::Cuda::load() {
        Ok(cuda) if cuda.init() == crate::cuda::CUDA_SUCCESS => Some(cuda),
        Ok(_) => {
            println!("  (cuInit failed; NvFBC will be skipped)");
            None
        }
        Err(e) => {
            println!("  ({e}; NvFBC will be skipped)");
            None
        }
    };

    match Presenter::start(Mode::Latency) {
        Ok(presenter) => {
            println!("  presenting at vsync, flipping every 100ms\n");
            with_each(cuda.as_ref(), |w| {
                let name = w.name();
                let measured = measure(&presenter.signal, w, LATENCY_SECS);
                report(name, &measured);
            });
        }
        Err(e) => println!("  presenter failed: {e}"),
    }

    println!();
    println!("== Stress: presenter uncapped, changing every frame ==");
    match Presenter::start(Mode::Stress) {
        Ok(presenter) => {
            let tearing = presenter
                .signal
                .tearing
                .load(std::sync::atomic::Ordering::Relaxed);
            if tearing {
                println!("  tearing available, so Present is not refresh-bound\n");
            } else {
                println!("  NO TEARING SUPPORT -- Present stays refresh-bound, so this");
                println!("  cannot show a backend exceeding the panel. Read it as a");
                println!("  keep-up test only.\n");
            }
            with_each(cuda.as_ref(), |w| {
                let name = w.name();
                let (transitions, presents, secs) = stress(&presenter.signal, w, STRESS_SECS);
                println!(
                    "  {name}: {:.1} distinct frames/sec against {:.1} presents/sec ({:.0}%)",
                    transitions as f64 / secs,
                    presents as f64 / secs,
                    if presents > 0 {
                        transitions as f64 * 100.0 / presents as f64
                    } else {
                        0.0
                    },
                );
            });
        }
        Err(e) => println!("  presenter failed: {e}"),
    }

    println!();
    println!("  -> Sanity check first: DDA and WGC are both post-composition, so their");
    println!("     p50s should be in the same region. If they are not, the harness is");
    println!("     measuring itself and none of the rest of this means anything.");
}
