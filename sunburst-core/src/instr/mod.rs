// SPDX-License-Identifier: GPL-2.0-or-later

//! Lock-free pipeline instrumentation.
//!
//! Built first, before anything it measures, because retrofitting timestamps
//! through code that already works is a thing nobody does.
//!
//! The frame path calls exactly one function, [`record`], which costs one clock
//! read and one store into a thread-local ring. Everything expensive — pairing
//! samples into durations, bucketing them, computing percentiles — happens on a
//! low-priority drain thread that the frame path never waits for.
//!
//! ```no_run
//! use sunburst_core::instr::{self, Stage};
//! # let frame_id = 0u32;
//!
//! // Once, at thread start.
//! instr::register_thread("capture");
//!
//! // Per frame. No allocation, no formatting, no locks, no logging.
//! instr::record(Stage::CaptureAcquire, frame_id);
//! ```
//!
//! # Reading the numbers honestly
//!
//! [`Report::is_lossy`] is not decoration. If the rings dropped samples or a
//! thread never got one, the percentiles below are computed over whatever
//! survived, and a thinned histogram is the kind of metric that is worse than
//! no metric. Check it before quoting a p99 anywhere.

pub mod clock;
mod drain;
pub mod format;
mod hist;
mod ring;
mod stage;

pub use drain::{Collector, DrainHandle, Report, StageStats, spawn};
pub use hist::Histogram;
pub use ring::{MAX_THREADS, RING_CAPACITY, Sample, record, register_thread};
pub use stage::{STAGE_COUNT, Stage};

// The end-to-end tests live in `tests/` rather than here, and deliberately.
// `record` and `drain_all` share one process-wide ring registry, so two tests
// draining in parallel would steal each other's samples — a separate test
// binary per scenario is a separate process, and the isolation is free.
