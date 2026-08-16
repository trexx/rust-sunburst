// SPDX-License-Identifier: GPL-2.0-or-later

//! Serde mirror of the Phase 1 instrumentation report.
//!
//! Mirrored for the same reason as [`crate::client::QuirksRecord`]: `serde` stays
//! out of `sunburst-core`, which the frame path depends on.
//!
//! The report is reused wholesale rather than measured again here. A second
//! measurement path would eventually disagree with the one the PR gate uses, and
//! two numbers for the same thing is worse than one.

use serde::{Deserialize, Serialize};
use sunburst_core::instr::Report;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct StageRecord {
    pub stage: String,
    pub count: u64,
    pub p50_ns: u64,
    pub p95_ns: u64,
    pub p99_ns: u64,
    pub max_ns: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct MetricsRecord {
    pub stages: Vec<StageRecord>,
    pub dropped_samples: u64,
    pub dropped_by_thread: Vec<(String, u32)>,
    pub unregistered_threads: u32,
    /// Whether these numbers can be quoted at all.
    ///
    /// Carried explicitly rather than left for the UI to derive from the two
    /// fields above. `format_table` already refuses to be quoted when this is
    /// set, and the web UI must not be the place that quietly drops the warning:
    /// a percentile table that has been silently thinned is exactly the metric
    /// that misleads.
    pub lossy: bool,
}

impl From<&Report> for MetricsRecord {
    fn from(r: &Report) -> Self {
        MetricsRecord {
            stages: r
                .stages
                .iter()
                .map(|s| StageRecord {
                    stage: s.stage.name().to_string(),
                    count: s.count,
                    p50_ns: s.p50_ns,
                    p95_ns: s.p95_ns,
                    p99_ns: s.p99_ns,
                    max_ns: s.max_ns,
                })
                .collect(),
            dropped_samples: r.dropped_samples,
            dropped_by_thread: r
                .dropped_by_thread
                .iter()
                .map(|(name, n)| ((*name).to_string(), *n))
                .collect(),
            unregistered_threads: r.unregistered_threads,
            lossy: r.is_lossy(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sunburst_core::instr::{Stage, StageStats};

    fn report() -> Report {
        Report {
            stages: vec![StageStats {
                stage: Stage::EncodeUnitOut,
                count: 3600,
                p50_ns: 6_100_000,
                p95_ns: 8_400_000,
                p99_ns: 9_100_000,
                max_ns: 15_000_000,
            }],
            dropped_samples: 0,
            dropped_by_thread: Vec::new(),
            unregistered_threads: 0,
        }
    }

    #[test]
    fn a_clean_report_converts_and_is_not_lossy() {
        let record = MetricsRecord::from(&report());
        assert!(!record.lossy);
        assert_eq!(record.stages.len(), 1);
        assert_eq!(record.stages[0].stage, "enc-unit");
        assert_eq!(record.stages[0].p99_ns, 9_100_000);
    }

    #[test]
    fn losses_are_carried_through_rather_than_dropped() {
        let mut r = report();
        r.dropped_samples = 17;
        r.dropped_by_thread = vec![("encode", 17)];
        r.unregistered_threads = 1;

        let record = MetricsRecord::from(&r);
        assert!(record.lossy, "the UI must be told these cannot be quoted");
        assert_eq!(record.dropped_samples, 17);
        assert_eq!(record.dropped_by_thread, vec![("encode".to_string(), 17)]);
        assert_eq!(record.unregistered_threads, 1);
    }

    #[test]
    fn lossy_agrees_with_the_core_definition() {
        // If `is_lossy` ever grows a third condition, this catches the mirror
        // falling behind it rather than the UI quietly under-reporting.
        let mut r = report();
        assert_eq!(MetricsRecord::from(&r).lossy, r.is_lossy());
        r.unregistered_threads = 2;
        assert_eq!(MetricsRecord::from(&r).lossy, r.is_lossy());
        r.unregistered_threads = 0;
        r.dropped_samples = 1;
        assert_eq!(MetricsRecord::from(&r).lossy, r.is_lossy());
    }

    #[test]
    fn round_trips_through_json() {
        let record = MetricsRecord::from(&report());
        let json = serde_json::to_string(&record).expect("serialise");
        assert_eq!(
            serde_json::from_str::<MetricsRecord>(&json).expect("load"),
            record
        );
    }

    #[test]
    fn an_empty_report_is_representable() {
        let empty = Report {
            stages: Vec::new(),
            dropped_samples: 0,
            dropped_by_thread: Vec::new(),
            unregistered_threads: 0,
        };
        let record = MetricsRecord::from(&empty);
        assert!(record.stages.is_empty());
        assert!(!record.lossy);
    }
}
