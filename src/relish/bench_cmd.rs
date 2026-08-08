//! Benchmark command orchestration and rendering.

use std::fmt::Write;
use std::path::PathBuf;

use serde::Serialize;

use crate::testkit::bench::{BenchComparison, BenchReport, BenchRunOptions, compare};

use super::client::BunClient;
use super::output::OutputFormat;
use super::{CommandOutcome, RelishError};

/// Values parsed from the bench subcommand.
pub struct BenchArgs {
    /// Use smaller workloads and sample counts.
    pub quick: bool,
    /// Strict JSON baseline to compare with the new report.
    pub compare: Option<PathBuf>,
    /// Include the opt-in cluster saturation probe.
    pub capacity: bool,
    /// Include the real leader-failure probe.
    pub disruptive: bool,
    /// Explicit acknowledgement for saturating or disruptive work.
    pub yes: bool,
    /// Human, JSON or YAML rendering selected globally.
    pub output: OutputFormat,
}

/// Run benchmarks against the configured cluster endpoint.
pub async fn run(args: BenchArgs) -> Result<CommandOutcome, RelishError> {
    run_with_client(args, BunClient::default_local()).await
}

async fn run_with_client(
    args: BenchArgs,
    client: BunClient,
) -> Result<CommandOutcome, RelishError> {
    validate_risk_flags(&args)?;
    let report = crate::testkit::bench::run(
        client,
        BenchRunOptions {
            quick: args.quick,
            capacity: args.capacity,
            disruptive: args.disruptive,
            yes: args.yes,
        },
    )
    .await
    .map_err(|error| RelishError::ApiError {
        status: 0,
        body: error.to_string(),
    })?;

    let comparison = match args.compare {
        Some(path) => {
            let bytes = tokio::fs::read(&path)
                .await
                .map_err(|source| RelishError::ApiError {
                    status: 0,
                    body: format!(
                        "could not read benchmark baseline {}: {source}",
                        path.display()
                    ),
                })?;
            let baseline: BenchReport =
                serde_json::from_slice(&bytes).map_err(|error| RelishError::ApiError {
                    status: 0,
                    body: format!(
                        "benchmark baseline {} is not strict report JSON: {error}",
                        path.display()
                    ),
                })?;
            Some(
                compare(&baseline, &report, 0.10).map_err(|error| RelishError::ApiError {
                    status: 0,
                    body: error.to_string(),
                })?,
            )
        }
        None => None,
    };

    render(&report, comparison.as_ref(), args.output)?;

    if !report.failed.is_empty()
        || comparison
            .as_ref()
            .is_some_and(|result| !result.informational && !result.regressions.is_empty())
    {
        Ok(CommandOutcome::Problems)
    } else {
        Ok(CommandOutcome::Clean)
    }
}

fn validate_risk_flags(args: &BenchArgs) -> Result<(), RelishError> {
    if args.capacity && !args.yes {
        return Err(RelishError::InvalidFlag {
            flag: "capacity".to_string(),
            reason: "cluster saturation also requires --yes".to_string(),
        });
    }
    if args.disruptive && !args.yes {
        return Err(RelishError::InvalidFlag {
            flag: "disruptive".to_string(),
            reason: "leader failure also requires --yes".to_string(),
        });
    }
    Ok(())
}

#[derive(Serialize)]
struct ComparedOutput<'a> {
    report: &'a BenchReport,
    comparison: &'a BenchComparison,
}

fn render(
    report: &BenchReport,
    comparison: Option<&BenchComparison>,
    output: OutputFormat,
) -> Result<(), RelishError> {
    let rendered = match (output, comparison) {
        (OutputFormat::Human, comparison) => render_human(report, comparison),
        (OutputFormat::Json, Some(comparison)) => {
            serde_json::to_string_pretty(&ComparedOutput { report, comparison })
                .map_err(RelishError::SerialiseJson)?
        }
        (OutputFormat::Yaml, Some(comparison)) => {
            serde_yaml::to_string(&ComparedOutput { report, comparison })
                .map_err(RelishError::SerialiseYaml)?
        }
        (OutputFormat::Json, None) => {
            serde_json::to_string_pretty(report).map_err(RelishError::SerialiseJson)?
        }
        (OutputFormat::Yaml, None) => {
            serde_yaml::to_string(report).map_err(RelishError::SerialiseYaml)?
        }
    };
    println!("{rendered}");
    Ok(())
}

fn render_human(report: &BenchReport, comparison: Option<&BenchComparison>) -> String {
    let mut output = String::new();
    let _ = writeln!(
        output,
        "benchmark: {} node(s), {} ms{}",
        report.environment.topology.node_count,
        report.duration_ms,
        if report.quick { " (quick)" } else { "" }
    );
    for metric in &report.metrics {
        let _ = writeln!(
            output,
            "  PASS  {:28} {:>10.3} {} ({} sample(s))",
            metric.name, metric.value, metric.unit, metric.samples
        );
    }
    for suite in &report.skipped {
        let _ = writeln!(output, "  SKIP  {:28} {}", suite.name, suite.reason);
    }
    for suite in &report.failed {
        let _ = writeln!(output, "  FAIL  {:28} {}", suite.name, suite.reason);
    }
    if !report.environment.unknown_nodes.is_empty() {
        let _ = writeln!(
            output,
            "  evidence unknown for: {}",
            report.environment.unknown_nodes.join(", ")
        );
    }
    if let Some(comparison) = comparison {
        let verdict = if comparison.informational {
            "INFORMATIONAL"
        } else if comparison.regressions.is_empty() {
            "PASS"
        } else {
            "FAIL"
        };
        let _ = writeln!(output, "\ncomparison: {verdict}");
        for change in &comparison.regressions {
            let _ = writeln!(
                output,
                "  REGRESSION  {:24} {:.3} -> {:.3}{}",
                change.name,
                change.baseline,
                change.current,
                percent(change.change_percent)
            );
        }
        for change in &comparison.improvements {
            let _ = writeln!(
                output,
                "  IMPROVEMENT {:24} {:.3} -> {:.3}{}",
                change.name,
                change.baseline,
                change.current,
                percent(change.change_percent)
            );
        }
        for reason in &comparison.informational_reasons {
            let _ = writeln!(output, "  {reason}");
        }
    }
    output
}

fn percent(value: Option<f64>) -> String {
    value
        .map(|value| format!(" ({value:+.1}%)"))
        .unwrap_or_else(|| " (percentage undefined from zero baseline)".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::bench::{
        BENCH_SCHEMA_VERSION, BenchEnvironment, BenchMetric, FailedSuite, HostedFingerprint,
        SkippedSuite, TopologyFingerprint,
    };
    use std::collections::BTreeMap;

    fn report() -> BenchReport {
        BenchReport {
            schema_version: BENCH_SCHEMA_VERSION,
            started_at: "2026-08-03T10:00:00Z".to_string(),
            duration_ms: 42,
            quick: true,
            capacity: false,
            environment: BenchEnvironment {
                cluster_name: "dev".to_string(),
                topology: TopologyFingerprint {
                    node_count: 3,
                    council_members: 3,
                },
                hosted: HostedFingerprint::default(),
                ..BenchEnvironment::default()
            },
            metrics: vec![BenchMetric {
                name: "network_throughput".to_string(),
                value: 12.5,
                unit: "MiB/s".to_string(),
                higher_is_better: true,
                samples: 1,
                parameters: BTreeMap::new(),
            }],
            skipped: vec![SkippedSuite {
                name: "cluster_capacity".to_string(),
                reason: "requires --capacity".to_string(),
            }],
            failed: vec![FailedSuite {
                name: "deploy_speed".to_string(),
                reason: "timed out".to_string(),
            }],
        }
    }

    #[test]
    fn human_report_distinguishes_pass_skip_and_failure() {
        let rendered = render_human(&report(), None);
        assert!(rendered.contains("PASS  network_throughput"), "{rendered}");
        assert!(rendered.contains("SKIP  cluster_capacity"), "{rendered}");
        assert!(rendered.contains("FAIL  deploy_speed"), "{rendered}");
    }

    #[test]
    fn destructive_flags_without_acknowledgement_are_refused() {
        let args = BenchArgs {
            quick: false,
            compare: None,
            capacity: true,
            disruptive: false,
            yes: false,
            output: OutputFormat::Human,
        };
        assert!(matches!(
            validate_risk_flags(&args),
            Err(RelishError::InvalidFlag { ref flag, .. }) if flag == "capacity"
        ));
    }
}
