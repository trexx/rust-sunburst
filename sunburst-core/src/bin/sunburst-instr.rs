// SPDX-License-Identifier: GPL-2.0-or-later

//! Readout and before/after comparison for the instrumentation ring.
//!
//! The server and client emit reports with `instr::format::write_report`; this
//! tool renders and compares them. `diff` is what satisfies the PR gate in
//! CLAUDE.md — a change touching a hot path ships a before/after p99, and
//! without a command that produces one the gate is a request nobody can
//! conveniently meet.
//!
//! ```text
//! sunburst-instr table <report>
//! sunburst-instr diff <before> <after>
//! sunburst-instr selftest
//! ```

use std::process::ExitCode;

use sunburst_core::instr::{self, Collector, Stage, format};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["table", path] => table(path),
        ["diff", before, after] => diff(before, after),
        ["selftest"] => selftest(),
        _ => {
            eprintln!("{}", USAGE);
            return ExitCode::from(2);
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sunburst-instr: {e}");
            ExitCode::FAILURE
        }
    }
}

const USAGE: &str = "\
usage:
  sunburst-instr table <report>          render one report
  sunburst-instr diff <before> <after>   compare p99s, for a PR description
  sunburst-instr selftest                run a synthetic pipeline and report

Reports are written by the server or client through
`sunburst_core::instr::format::write_report`.";

fn table(path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let text = std::fs::read_to_string(path)?;
    let report = format::parse_report(&text)?;
    print!("{}", format::format_table(&report));
    Ok(())
}

fn diff(before: &str, after: &str) -> Result<(), Box<dyn std::error::Error>> {
    let before = format::parse_report(&std::fs::read_to_string(before)?)?;
    let after = format::parse_report(&std::fs::read_to_string(after)?)?;
    print!("{}", format::format_diff(&before, &after));
    Ok(())
}

/// Drive the real recording path so the instrumentation can be proved working
/// on a machine before there is anything to instrument.
///
/// Worth having on the Windows box specifically: the sub-50ns claim for
/// `record` is measured against `clock_gettime` on the development machine, and
/// `QueryPerformanceCounter` is a different cost.
fn selftest() -> Result<(), Box<dyn std::error::Error>> {
    const STAGES: [Stage; 5] = [
        Stage::CaptureAcquire,
        Stage::ColorConvert,
        Stage::EncodeSubmit,
        Stage::EncodeUnitOut,
        Stage::Packetize,
    ];

    instr::register_thread("selftest");
    let mut collector = Collector::new();

    eprintln!("running a synthetic 5-stage pipeline for 600 frames...");
    for frame_id in 0..600u32 {
        for stage in STAGES {
            instr::record(stage, frame_id);
            // Roughly a stage's worth of work. Spun rather than slept: sleep
            // granularity is coarser than the durations being measured.
            let until = instr::clock::now() + instr::clock::ticks_per_sec() / 5_000;
            while instr::clock::now() < until {
                std::hint::spin_loop();
            }
        }
        if frame_id % 512 == 0 {
            collector.poll();
        }
    }
    collector.poll();

    let report = collector.report();
    eprint!("{}", format::format_table(&report));
    eprintln!("\n--- machine-readable, redirect stdout to save ---");
    print!("{}", format::write_report(&report));

    // A clock that cannot resolve a stage is worth saying out loud rather than
    // leaving as a table of zeroes.
    let resolution = instr::clock::ticks_per_sec();
    if resolution < 1_000_000 {
        eprintln!(
            "\nWARNING: clock resolution is {resolution} Hz; sub-millisecond stages are noise."
        );
    }
    Ok(())
}
