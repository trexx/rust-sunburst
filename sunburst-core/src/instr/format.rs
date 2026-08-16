// SPDX-License-Identifier: GPL-2.0-or-later

//! Rendering and parsing of [`Report`]s.
//!
//! The machine format is line-oriented text rather than JSON, for two reasons.
//! It is readable as-is, which matters because the PR gate asks for numbers
//! pasted into a description and a reviewer should not have to run a tool to
//! read them. And it is small enough to write and parse by hand, which keeps a
//! serialisation framework out of the one crate that both ends of the link
//! depend on.

use core::fmt::Write as _;

use super::drain::{Report, StageStats};
use super::stage::Stage;

/// Bumped if the field set changes, so `diff` refuses mismatched files rather
/// than comparing columns that no longer mean the same thing.
const FORMAT_VERSION: u32 = 1;

/// Render a report as the machine-readable format.
pub fn write_report(r: &Report) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "sunburst-instr {FORMAT_VERSION}");
    let _ = writeln!(out, "dropped {}", r.dropped_samples);
    let _ = writeln!(out, "unregistered {}", r.unregistered_threads);
    for (name, dropped) in &r.dropped_by_thread {
        let _ = writeln!(out, "thread-dropped {name} {dropped}");
    }
    for s in &r.stages {
        let _ = writeln!(
            out,
            "stage {} count={} p50={} p95={} p99={} max={}",
            s.stage.name(),
            s.count,
            s.p50_ns,
            s.p95_ns,
            s.p99_ns,
            s.max_ns
        );
    }
    out
}

/// What went wrong reading a report file.
#[derive(Debug, PartialEq, Eq)]
pub enum ParseError {
    MissingHeader,
    /// Written by a build whose columns may not mean the same thing.
    VersionMismatch {
        found: u32,
    },
    BadLine(String),
    UnknownStage(String),
}

impl core::fmt::Display for ParseError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            ParseError::MissingHeader => write!(f, "not a sunburst-instr report"),
            ParseError::VersionMismatch { found } => write!(
                f,
                "report is format version {found}, this build writes {FORMAT_VERSION}"
            ),
            ParseError::BadLine(l) => write!(f, "malformed line: {l}"),
            ParseError::UnknownStage(s) => write!(f, "unknown stage: {s}"),
        }
    }
}

impl std::error::Error for ParseError {}

/// Read back what [`write_report`] produced.
pub fn parse_report(text: &str) -> Result<Report, ParseError> {
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());

    let header = lines.next().ok_or(ParseError::MissingHeader)?;
    let version = header
        .strip_prefix("sunburst-instr ")
        .ok_or(ParseError::MissingHeader)?
        .trim()
        .parse::<u32>()
        .map_err(|_| ParseError::MissingHeader)?;
    if version != FORMAT_VERSION {
        return Err(ParseError::VersionMismatch { found: version });
    }

    let mut report = Report {
        stages: Vec::new(),
        dropped_samples: 0,
        dropped_by_thread: Vec::new(),
        unregistered_threads: 0,
    };

    for line in lines {
        let bad = || ParseError::BadLine(line.to_string());
        let mut parts = line.split_whitespace();
        match parts.next() {
            Some("dropped") => {
                report.dropped_samples =
                    parts.next().and_then(|v| v.parse().ok()).ok_or_else(bad)?;
            }
            Some("unregistered") => {
                report.unregistered_threads =
                    parts.next().and_then(|v| v.parse().ok()).ok_or_else(bad)?;
            }
            Some("thread-dropped") => {
                // Thread names are `&'static str` in the live report; on the way
                // back in they are leaked, which is fine for a short-lived tool
                // and keeps `Report` free of lifetimes on the hot side.
                let name: &'static str =
                    Box::leak(parts.next().ok_or_else(bad)?.to_string().into_boxed_str());
                let count = parts.next().and_then(|v| v.parse().ok()).ok_or_else(bad)?;
                report.dropped_by_thread.push((name, count));
            }
            Some("stage") => {
                let name = parts.next().ok_or_else(bad)?;
                let stage = Stage::ALL
                    .into_iter()
                    .find(|s| s.name() == name)
                    .ok_or_else(|| ParseError::UnknownStage(name.to_string()))?;
                let mut fields = [None::<u64>; 5];
                for kv in parts {
                    let (k, v) = kv.split_once('=').ok_or_else(bad)?;
                    let v: u64 = v.parse().map_err(|_| bad())?;
                    match k {
                        "count" => fields[0] = Some(v),
                        "p50" => fields[1] = Some(v),
                        "p95" => fields[2] = Some(v),
                        "p99" => fields[3] = Some(v),
                        "max" => fields[4] = Some(v),
                        _ => return Err(bad()),
                    }
                }
                let [count, p50, p95, p99, max] = fields;
                report.stages.push(StageStats {
                    stage,
                    count: count.ok_or_else(bad)?,
                    p50_ns: p50.ok_or_else(bad)?,
                    p95_ns: p95.ok_or_else(bad)?,
                    p99_ns: p99.ok_or_else(bad)?,
                    max_ns: max.ok_or_else(bad)?,
                });
            }
            _ => return Err(bad()),
        }
    }

    Ok(report)
}

/// Human-readable per-stage table.
pub fn format_table(r: &Report) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:<12} {:>9} {:>10} {:>10} {:>10} {:>10}",
        "stage", "count", "p50", "p95", "p99", "max"
    );
    let _ = writeln!(out, "{}", "-".repeat(65));
    for s in &r.stages {
        let _ = writeln!(
            out,
            "{:<12} {:>9} {:>10} {:>10} {:>10} {:>10}",
            s.stage.name(),
            s.count,
            human_ns(s.p50_ns),
            human_ns(s.p95_ns),
            human_ns(s.p99_ns),
            human_ns(s.max_ns),
        );
    }
    if r.stages.is_empty() {
        let _ = writeln!(out, "(no stages recorded)");
    }
    // Loss is reported loudly rather than as a footnote: it decides whether the
    // table above can be quoted at all.
    if r.is_lossy() {
        let _ = writeln!(
            out,
            "\nWARNING: {} samples dropped, {} threads unregistered.",
            r.dropped_samples, r.unregistered_threads
        );
        for (name, dropped) in &r.dropped_by_thread {
            let _ = writeln!(out, "  {name}: {dropped} dropped");
        }
        let _ = writeln!(
            out,
            "These percentiles are computed over what survived. Do not quote them."
        );
    }
    out
}

/// Before/after p99 comparison, for pasting into a PR description.
pub fn format_diff(before: &Report, after: &Report) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:<12} {:>12} {:>12} {:>10}",
        "stage", "p99 before", "p99 after", "delta"
    );
    let _ = writeln!(out, "{}", "-".repeat(50));

    for stage in Stage::ALL {
        let b = before.stage(stage);
        let a = after.stage(stage);
        let (b, a) = match (b, a) {
            (Some(b), Some(a)) => (b, a),
            // A stage present on only one side is shown rather than skipped —
            // instrumentation appearing or disappearing is itself a change
            // worth seeing in the diff.
            (Some(b), None) => {
                let _ = writeln!(
                    out,
                    "{:<12} {:>12} {:>12} {:>10}",
                    stage.name(),
                    human_ns(b.p99_ns),
                    "-",
                    "gone"
                );
                continue;
            }
            (None, Some(a)) => {
                let _ = writeln!(
                    out,
                    "{:<12} {:>12} {:>12} {:>10}",
                    stage.name(),
                    "-",
                    human_ns(a.p99_ns),
                    "new"
                );
                continue;
            }
            (None, None) => continue,
        };

        let delta = if b.p99_ns == 0 {
            "-".to_string()
        } else {
            let pct = (a.p99_ns as f64 - b.p99_ns as f64) / b.p99_ns as f64 * 100.0;
            format!("{pct:+.1}%")
        };
        let _ = writeln!(
            out,
            "{:<12} {:>12} {:>12} {:>10}",
            stage.name(),
            human_ns(b.p99_ns),
            human_ns(a.p99_ns),
            delta
        );
    }

    if before.is_lossy() || after.is_lossy() {
        let _ = writeln!(
            out,
            "\nWARNING: one or both runs dropped samples. This comparison is not sound."
        );
    }
    out
}

/// Nanoseconds at a scale a human reads without counting zeroes.
fn human_ns(ns: u64) -> String {
    if ns >= 1_000_000 {
        format!("{:.2} ms", ns as f64 / 1_000_000.0)
    } else if ns >= 1_000 {
        format!("{:.1} us", ns as f64 / 1_000.0)
    } else {
        format!("{ns} ns")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_report() -> Report {
        Report {
            stages: vec![
                StageStats {
                    stage: Stage::ColorConvert,
                    count: 3600,
                    p50_ns: 812_000,
                    p95_ns: 1_400_000,
                    p99_ns: 1_900_000,
                    max_ns: 4_200_000,
                },
                StageStats {
                    stage: Stage::EncodeUnitOut,
                    count: 14400,
                    p50_ns: 6_100_000,
                    p95_ns: 8_400_000,
                    p99_ns: 9_100_000,
                    max_ns: 15_000_000,
                },
            ],
            dropped_samples: 0,
            dropped_by_thread: Vec::new(),
            unregistered_threads: 0,
        }
    }

    #[test]
    fn round_trips_through_the_machine_format() {
        let r = sample_report();
        let parsed = parse_report(&write_report(&r)).expect("should parse");
        assert_eq!(parsed, r);
    }

    #[test]
    fn round_trips_with_losses_recorded() {
        let mut r = sample_report();
        r.dropped_samples = 17;
        r.dropped_by_thread = vec![("encode", 17)];
        r.unregistered_threads = 2;
        let parsed = parse_report(&write_report(&r)).expect("should parse");
        assert_eq!(parsed, r);
        assert!(parsed.is_lossy());
    }

    #[test]
    fn rejects_a_foreign_or_future_format() {
        assert_eq!(
            parse_report("something else\n"),
            Err(ParseError::MissingHeader)
        );
        assert_eq!(
            parse_report("sunburst-instr 99\n"),
            Err(ParseError::VersionMismatch { found: 99 })
        );
    }

    #[test]
    fn rejects_a_stage_it_does_not_know() {
        let text = "sunburst-instr 1\nstage teleport count=1 p50=1 p95=1 p99=1 max=1\n";
        assert!(matches!(
            parse_report(text),
            Err(ParseError::UnknownStage(_))
        ));
    }

    #[test]
    fn rejects_a_stage_line_missing_a_field() {
        let text = "sunburst-instr 1\nstage convert count=1 p50=1 p95=1\n";
        assert!(matches!(parse_report(text), Err(ParseError::BadLine(_))));
    }

    #[test]
    fn diff_reports_direction_and_magnitude() {
        let before = sample_report();
        let mut after = sample_report();
        after.stages[1].p99_ns = 10_010_000; // 9.10ms -> 10.01ms, +10%

        let d = format_diff(&before, &after);
        assert!(d.contains("+10.0%"), "expected a +10% row:\n{d}");
        assert!(
            d.contains("+0.0%"),
            "unchanged stage should read as +0.0%:\n{d}"
        );
    }

    #[test]
    fn diff_shows_stages_that_appear_or_vanish() {
        let before = sample_report();
        let mut after = sample_report();
        after.stages.pop();
        let d = format_diff(&before, &after);
        assert!(
            d.contains("gone"),
            "a stage losing instrumentation should show:\n{d}"
        );

        let d = format_diff(&after, &before);
        assert!(
            d.contains("new"),
            "a newly instrumented stage should show:\n{d}"
        );
    }

    #[test]
    fn diff_refuses_to_be_quoted_when_a_run_was_lossy() {
        let before = sample_report();
        let mut after = sample_report();
        after.dropped_samples = 5;
        after.dropped_by_thread = vec![("encode", 5)];
        assert!(format_diff(&before, &after).contains("not sound"));
    }

    #[test]
    fn table_warns_loudly_rather_than_footnoting_loss() {
        let mut r = sample_report();
        r.dropped_samples = 9;
        r.dropped_by_thread = vec![("capture", 9)];
        let t = format_table(&r);
        assert!(t.contains("WARNING"));
        assert!(t.contains("Do not quote them"));
        assert!(t.contains("capture: 9 dropped"));
    }

    #[test]
    fn human_ns_switches_scale() {
        assert_eq!(human_ns(900), "900 ns");
        assert_eq!(human_ns(1_500), "1.5 us");
        assert_eq!(human_ns(9_100_000), "9.10 ms");
    }
}
