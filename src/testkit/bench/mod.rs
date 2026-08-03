//! Stable reports and comparison rules for `relish bench`.

pub mod compare;
pub mod report;

pub use compare::{BenchComparison, BenchComparisonError, MetricChange, compare};
pub use report::{
    BENCH_SCHEMA_VERSION, BenchEnvironment, BenchMetric, BenchNodeFingerprint, BenchReport,
    HostedFingerprint, SkippedSuite, TopologyFingerprint,
};
