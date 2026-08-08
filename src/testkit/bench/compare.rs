//! Direction-aware benchmark comparison.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::report::{BENCH_SCHEMA_VERSION, BenchEnvironment, BenchMetric, BenchReport};

/// One material change between a baseline and current metric.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetricChange {
    pub name: String,
    pub baseline: f64,
    pub current: f64,
    /// `None` when the zero baseline makes a finite percentage undefined.
    pub change_percent: Option<f64>,
}

/// Compatible report comparison.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BenchComparison {
    pub threshold_percent: f64,
    pub informational: bool,
    pub informational_reasons: Vec<String>,
    pub regressions: Vec<MetricChange>,
    pub improvements: Vec<MetricChange>,
    pub missing_in_current: Vec<String>,
    pub missing_in_baseline: Vec<String>,
}

/// Why two reports cannot produce a meaningful regression verdict.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum BenchComparisonError {
    #[error("comparison threshold must be finite and greater than zero")]
    InvalidThreshold,
    #[error("{which} benchmark schema {found} is incompatible with {expected}")]
    Schema {
        which: &'static str,
        found: u32,
        expected: u32,
    },
    #[error("{which} benchmark report is invalid: {reason}")]
    InvalidReport { which: &'static str, reason: String },
    #[error("benchmark reports are incompatible: {reasons}")]
    Incompatible { reasons: String },
}

/// Compare two benchmark reports.
pub fn compare(
    baseline: &BenchReport,
    current: &BenchReport,
    threshold: f64,
) -> Result<BenchComparison, BenchComparisonError> {
    if !threshold.is_finite() || threshold <= 0.0 {
        return Err(BenchComparisonError::InvalidThreshold);
    }
    validate_report("baseline", baseline)?;
    validate_report("current", current)?;

    let baseline_metrics = metrics_by_name(baseline);
    let current_metrics = metrics_by_name(current);
    let mut incompatibilities = environment_incompatibilities(
        &baseline.environment,
        &current.environment,
        baseline.quick,
        current.quick,
        baseline.capacity,
        current.capacity,
    );
    for (name, baseline_metric) in &baseline_metrics {
        let Some(current_metric) = current_metrics.get(name) else {
            continue;
        };
        if baseline_metric.unit != current_metric.unit {
            incompatibilities.push(format!(
                "metric {name} unit differs ({} vs {})",
                baseline_metric.unit, current_metric.unit
            ));
        }
        if baseline_metric.higher_is_better != current_metric.higher_is_better {
            incompatibilities.push(format!("metric {name} direction differs"));
        }
        if baseline_metric.parameters != current_metric.parameters {
            incompatibilities.push(format!("metric {name} parameters differ"));
        }
    }
    if !incompatibilities.is_empty() {
        return Err(BenchComparisonError::Incompatible {
            reasons: incompatibilities.join("; "),
        });
    }

    let mut regressions = Vec::new();
    let mut improvements = Vec::new();
    for (name, baseline_metric) in &baseline_metrics {
        let Some(current_metric) = current_metrics.get(name) else {
            continue;
        };
        let magnitude = (current_metric.value - baseline_metric.value).abs();
        if magnitude <= threshold * baseline_metric.value.abs() {
            continue;
        }
        let change = MetricChange {
            name: name.clone(),
            baseline: baseline_metric.value,
            current: current_metric.value,
            change_percent: finite_change_percent(baseline_metric.value, current_metric.value),
        };
        let improved = if baseline_metric.higher_is_better {
            current_metric.value > baseline_metric.value
        } else {
            current_metric.value < baseline_metric.value
        };
        if improved {
            improvements.push(change);
        } else {
            regressions.push(change);
        }
    }

    let baseline_names: BTreeSet<_> = baseline_metrics.keys().cloned().collect();
    let current_names: BTreeSet<_> = current_metrics.keys().cloned().collect();
    let hosted = baseline.environment.hosted.detected || current.environment.hosted.detected;
    let informational_reasons = hosted
        .then(|| {
            format!(
                "hosted benchmark environment detected (baseline: {}, current: {}); regression verdict is informational",
                hosted_name(&baseline.environment),
                hosted_name(&current.environment)
            )
        })
        .into_iter()
        .collect();

    Ok(BenchComparison {
        threshold_percent: threshold * 100.0,
        informational: hosted,
        informational_reasons,
        regressions,
        improvements,
        missing_in_current: baseline_names.difference(&current_names).cloned().collect(),
        missing_in_baseline: current_names.difference(&baseline_names).cloned().collect(),
    })
}

fn validate_report(which: &'static str, report: &BenchReport) -> Result<(), BenchComparisonError> {
    if report.schema_version != BENCH_SCHEMA_VERSION {
        return Err(BenchComparisonError::Schema {
            which,
            found: report.schema_version,
            expected: BENCH_SCHEMA_VERSION,
        });
    }
    if report.environment.topology.node_count == 0 {
        return invalid(which, "topology has zero nodes");
    }
    if !report.failed.is_empty() {
        return invalid(which, "one or more benchmark suites failed");
    }
    let mut names = BTreeSet::new();
    for metric in &report.metrics {
        if metric.name.is_empty() {
            return invalid(which, "metric name is empty");
        }
        if !names.insert(metric.name.as_str()) {
            return invalid(which, &format!("duplicate metric {}", metric.name));
        }
        if metric.unit.is_empty() {
            return invalid(which, &format!("metric {} has no unit", metric.name));
        }
        if !metric.value.is_finite() || metric.value < 0.0 {
            return invalid(
                which,
                &format!("metric {} has an invalid value", metric.name),
            );
        }
        if metric.samples == 0 {
            return invalid(which, &format!("metric {} has zero samples", metric.name));
        }
    }
    Ok(())
}

fn invalid<T>(which: &'static str, reason: &str) -> Result<T, BenchComparisonError> {
    Err(BenchComparisonError::InvalidReport {
        which,
        reason: reason.to_string(),
    })
}

fn metrics_by_name(report: &BenchReport) -> BTreeMap<String, &BenchMetric> {
    report
        .metrics
        .iter()
        .map(|metric| (metric.name.clone(), metric))
        .collect()
}

fn environment_incompatibilities(
    baseline: &BenchEnvironment,
    current: &BenchEnvironment,
    baseline_quick: bool,
    current_quick: bool,
    baseline_capacity: bool,
    current_capacity: bool,
) -> Vec<String> {
    let mut reasons = Vec::new();
    if baseline_quick != current_quick {
        reasons.push(format!(
            "quick mode differs ({baseline_quick} vs {current_quick})"
        ));
    }
    if baseline_capacity != current_capacity {
        reasons.push(format!(
            "capacity mode differs ({baseline_capacity} vs {current_capacity})"
        ));
    }
    if baseline.topology != current.topology {
        reasons.push(format!(
            "topology differs ({:?} vs {:?})",
            baseline.topology, current.topology
        ));
    }
    if !baseline.unknown_nodes.is_empty() || !current.unknown_nodes.is_empty() {
        reasons.push(format!(
            "unknown node fingerprints remain (baseline: {:?}, current: {:?})",
            baseline.unknown_nodes, current.unknown_nodes
        ));
    }
    let baseline_platforms = platform_fingerprints(baseline);
    let current_platforms = platform_fingerprints(current);
    if baseline_platforms != current_platforms {
        reasons.push(format!(
            "node build target/profile and runtime/version/rootless/kernel/architecture fingerprints differ ({baseline_platforms:?} vs {current_platforms:?})"
        ));
    }
    reasons
}

fn platform_fingerprints(environment: &BenchEnvironment) -> Vec<String> {
    let mut fingerprints: Vec<_> = environment
        .nodes
        .iter()
        .map(|node| {
            format!(
                "target={};profile={};runtime={};runtime_version={};rootless={};kernel={};architecture={}",
                node.build.target,
                node.build.profile,
                node.runtime.name,
                node.runtime.version.as_deref().unwrap_or("<unknown>"),
                node.runtime.rootless,
                node.runtime.kernel,
                node.runtime.architecture,
            )
        })
        .collect();
    fingerprints.sort();
    fingerprints
}

fn finite_change_percent(baseline: f64, current: f64) -> Option<f64> {
    if baseline == 0.0 {
        None
    } else {
        Some((current - baseline) / baseline.abs() * 100.0)
    }
}

fn hosted_name(environment: &BenchEnvironment) -> &str {
    if !environment.hosted.detected {
        "no"
    } else {
        environment
            .hosted
            .provider
            .as_deref()
            .unwrap_or("yes (provider unknown)")
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::bun::capabilities::{BuildFingerprint, RuntimeFingerprint};
    use crate::testkit::bench::report::{
        BENCH_SCHEMA_VERSION, BenchEnvironment, BenchMetric, BenchNodeFingerprint, BenchReport,
        HostedFingerprint, TopologyFingerprint,
    };

    fn metric(name: &str, value: f64, higher_is_better: bool) -> BenchMetric {
        BenchMetric {
            name: name.to_string(),
            value,
            unit: "ms".to_string(),
            higher_is_better,
            samples: 3,
            parameters: BTreeMap::from([("requests".to_string(), "100".to_string())]),
        }
    }

    fn node(version: &str, git_sha: &str) -> BenchNodeFingerprint {
        BenchNodeFingerprint {
            node_id: "node-a".to_string(),
            build: BuildFingerprint {
                version: version.to_string(),
                git_sha: Some(git_sha.to_string()),
                target: "aarch64-unknown-linux-gnu".to_string(),
                profile: "release".to_string(),
            },
            runtime: RuntimeFingerprint {
                node_id: "node-a".to_string(),
                name: "runc".to_string(),
                version: Some("1.4.0".to_string()),
                rootless: false,
                kernel: "Linux 6.8.0".to_string(),
                architecture: "aarch64".to_string(),
            },
        }
    }

    fn report(metrics: Vec<BenchMetric>) -> BenchReport {
        BenchReport {
            schema_version: BENCH_SCHEMA_VERSION,
            started_at: "2026-07-29T12:00:00Z".to_string(),
            duration_ms: 1_000,
            quick: false,
            capacity: false,
            environment: BenchEnvironment {
                cluster_name: "bench-a".to_string(),
                cluster_id: Some("cluster-a".to_string()),
                topology: TopologyFingerprint {
                    node_count: 1,
                    council_members: 1,
                },
                nodes: vec![node("0.1.0", "old")],
                unknown_nodes: Vec::new(),
                hosted: HostedFingerprint::default(),
            },
            metrics,
            skipped: Vec::new(),
            failed: Vec::new(),
        }
    }

    #[test]
    fn comparison_is_direction_aware_and_reports_missing_metrics() {
        let baseline = report(vec![
            metric("latency", 100.0, false),
            metric("throughput", 100.0, true),
            metric("removed", 1.0, true),
        ]);
        let current = report(vec![
            metric("latency", 111.0, false),
            metric("throughput", 111.0, true),
            metric("added", 1.0, true),
        ]);

        let comparison = compare(&baseline, &current, 0.10).unwrap();

        assert_eq!(
            comparison
                .regressions
                .iter()
                .map(|change| change.name.as_str())
                .collect::<Vec<_>>(),
            vec!["latency"]
        );
        assert_eq!(
            comparison
                .improvements
                .iter()
                .map(|change| change.name.as_str())
                .collect::<Vec<_>>(),
            vec!["throughput"]
        );
        assert_eq!(comparison.missing_in_current, vec!["removed"]);
        assert_eq!(comparison.missing_in_baseline, vec!["added"]);
    }

    #[test]
    fn exactly_the_threshold_is_not_a_regression() {
        let baseline = report(vec![metric("latency", 100.0, false)]);
        let current = report(vec![metric("latency", 110.0, false)]);

        let comparison = compare(&baseline, &current, 0.10).unwrap();

        assert!(comparison.regressions.is_empty());
    }

    #[test]
    fn build_identity_may_change_but_platform_must_not() {
        let baseline = report(vec![metric("latency", 100.0, false)]);
        let mut current = report(vec![metric("latency", 111.0, false)]);
        current.environment.nodes[0] = node("0.2.0", "new");
        assert!(compare(&baseline, &current, 0.10).is_ok());

        current.environment.nodes[0].runtime.kernel = "Linux 6.12.0".to_string();
        let error = compare(&baseline, &current, 0.10).unwrap_err();
        assert!(error.to_string().contains("kernel"), "{error}");
    }

    #[test]
    fn topology_quick_mode_and_metric_method_are_compatibility_checks() {
        let baseline = report(vec![metric("latency", 100.0, false)]);
        let mut current = report(vec![metric("latency", 111.0, false)]);
        current.quick = true;
        current.environment.topology.node_count = 3;
        current.metrics[0]
            .parameters
            .insert("requests".to_string(), "50".to_string());

        let error = compare(&baseline, &current, 0.10).unwrap_err();
        let message = error.to_string();
        assert!(message.contains("quick"), "{message}");
        assert!(message.contains("topology"), "{message}");
        assert!(message.contains("parameters"), "{message}");
    }

    #[test]
    fn hosted_runs_are_comparable_but_informational() {
        let baseline = report(vec![metric("latency", 100.0, false)]);
        let mut current = report(vec![metric("latency", 111.0, false)]);
        current.environment.hosted = HostedFingerprint {
            detected: true,
            provider: Some("github-actions".to_string()),
        };

        let comparison = compare(&baseline, &current, 0.10).unwrap();

        assert!(comparison.informational);
        assert_eq!(comparison.regressions.len(), 1);
        assert!(
            comparison.informational_reasons[0].contains("hosted"),
            "{comparison:?}"
        );
    }

    #[test]
    fn zero_baseline_never_emits_non_finite_json() {
        let baseline = report(vec![metric("latency", 0.0, false)]);
        let current = report(vec![metric("latency", 1.0, false)]);

        let comparison = compare(&baseline, &current, 0.10).unwrap();

        assert_eq!(comparison.regressions[0].change_percent, None);
        serde_json::to_string(&comparison).unwrap();
    }

    #[test]
    fn wrong_schema_duplicate_names_and_unknown_nodes_are_rejected() {
        let baseline = report(vec![metric("latency", 100.0, false)]);
        let mut wrong_schema = report(vec![metric("latency", 100.0, false)]);
        wrong_schema.schema_version = 99;
        assert!(matches!(
            compare(&wrong_schema, &baseline, 0.10),
            Err(BenchComparisonError::Schema {
                which: "baseline",
                ..
            })
        ));

        let duplicate = report(vec![
            metric("latency", 100.0, false),
            metric("latency", 101.0, false),
        ]);
        assert!(matches!(
            compare(&duplicate, &baseline, 0.10),
            Err(BenchComparisonError::InvalidReport {
                which: "baseline",
                ..
            })
        ));

        let mut unknown = report(vec![metric("latency", 100.0, false)]);
        unknown.environment.unknown_nodes.push("node-b".to_string());
        let error = compare(&baseline, &unknown, 0.10).unwrap_err();
        assert!(error.to_string().contains("unknown node"), "{error}");

        let mut failed = report(vec![metric("latency", 100.0, false)]);
        failed.failed.push(crate::testkit::bench::FailedSuite {
            name: "network_throughput".to_string(),
            reason: "timed out".to_string(),
        });
        let error = compare(&failed, &baseline, 0.10).unwrap_err();
        assert!(error.to_string().contains("suites failed"), "{error}");
    }
}
