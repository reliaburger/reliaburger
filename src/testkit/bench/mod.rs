//! Stable reports and comparison rules for `relish bench`.

pub mod compare;
pub mod environment;
pub mod report;
pub mod runner;
pub mod suites;

pub use compare::{BenchComparison, BenchComparisonError, MetricChange, compare};
pub use environment::{build_environment, detect_hosted_environment};
pub use report::{
    BENCH_SCHEMA_VERSION, BenchEnvironment, BenchMetric, BenchNodeFingerprint, BenchReport,
    FailedSuite, HostedFingerprint, SkippedSuite, TopologyFingerprint,
};
pub use runner::{BenchRunOptions, BenchRunnerError, run};
