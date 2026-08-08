//! The versioned JSON contract produced by `relish bench`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::bun::capabilities::{BuildFingerprint, RuntimeFingerprint};

/// Current `relish bench --output json` schema.
pub const BENCH_SCHEMA_VERSION: u32 = 2;

/// One node's reproducibility identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchNodeFingerprint {
    pub node_id: String,
    pub build: BuildFingerprint,
    pub runtime: RuntimeFingerprint,
}

/// Cluster shape which materially changes benchmark results.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TopologyFingerprint {
    pub node_count: u32,
    pub council_members: u32,
}

/// Whether the run happened in an environment known to add scheduling noise.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostedFingerprint {
    pub detected: bool,
    pub provider: Option<String>,
}

/// Reproduction data captured alongside every metric.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchEnvironment {
    pub cluster_name: String,
    pub cluster_id: Option<String>,
    pub topology: TopologyFingerprint,
    pub nodes: Vec<BenchNodeFingerprint>,
    /// Expected nodes which returned no trustworthy fingerprint.
    pub unknown_nodes: Vec<String>,
    pub hosted: HostedFingerprint,
}

/// One aggregate benchmark measurement.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchMetric {
    pub name: String,
    pub value: f64,
    pub unit: String,
    pub higher_is_better: bool,
    pub samples: u32,
    /// Workload size and method, used to prevent unlike comparisons.
    pub parameters: BTreeMap<String, String>,
}

/// A suite which could not produce a measurement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SkippedSuite {
    pub name: String,
    pub reason: String,
}

/// A suite which started but did not produce trustworthy evidence.
///
/// Unlike [`SkippedSuite`], this is a failed run: timeouts, API errors and
/// unconfirmed cleanup belong here and must make `relish bench` exit non-zero.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FailedSuite {
    /// Stable suite name, matching the metric it would have produced.
    pub name: String,
    /// Execution or cleanup failure which made a metric untrustworthy.
    pub reason: String,
}

/// Complete benchmark result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchReport {
    pub schema_version: u32,
    pub started_at: String,
    pub duration_ms: u64,
    pub quick: bool,
    pub capacity: bool,
    pub environment: BenchEnvironment,
    pub metrics: Vec<BenchMetric>,
    pub skipped: Vec<SkippedSuite>,
    pub failed: Vec<FailedSuite>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report() -> BenchReport {
        BenchReport {
            schema_version: BENCH_SCHEMA_VERSION,
            started_at: "2026-07-29T12:00:00Z".to_string(),
            duration_ms: 250,
            quick: true,
            capacity: false,
            environment: BenchEnvironment {
                cluster_name: "bench".to_string(),
                cluster_id: Some("cluster-1".to_string()),
                topology: TopologyFingerprint {
                    node_count: 1,
                    council_members: 1,
                },
                nodes: Vec::new(),
                unknown_nodes: vec!["node-a".to_string()],
                hosted: HostedFingerprint {
                    detected: true,
                    provider: Some("github-actions".to_string()),
                },
            },
            metrics: vec![BenchMetric {
                name: "discovery_latency_p99".to_string(),
                value: 1.25,
                unit: "ms".to_string(),
                higher_is_better: false,
                samples: 50,
                parameters: BTreeMap::from([("requests".to_string(), "50".to_string())]),
            }],
            skipped: vec![SkippedSuite {
                name: "cluster_capacity".to_string(),
                reason: "requires --capacity".to_string(),
            }],
            failed: vec![FailedSuite {
                name: "reconstruction_time".to_string(),
                reason: "timed out after 60s".to_string(),
            }],
        }
    }

    #[test]
    fn json_contract_round_trips_without_losing_fingerprints() {
        let report = report();
        let json = serde_json::to_value(&report).unwrap();

        assert_eq!(json["schema_version"], BENCH_SCHEMA_VERSION);
        assert_eq!(json["environment"]["topology"]["node_count"], 1);
        assert_eq!(json["environment"]["hosted"]["provider"], "github-actions");
        assert_eq!(
            json["metrics"][0]["parameters"]["requests"],
            serde_json::json!("50")
        );
        assert_eq!(serde_json::from_value::<BenchReport>(json).unwrap(), report);
    }

    #[test]
    fn failed_suites_are_distinct_from_preflight_skips() {
        let json = serde_json::to_value(report()).unwrap();

        assert_eq!(json["skipped"][0]["name"], "cluster_capacity");
        assert_eq!(json["failed"][0]["name"], "reconstruction_time");
    }

    #[test]
    fn same_schema_with_an_unknown_field_is_rejected() {
        let mut json = serde_json::to_value(report()).unwrap();
        json.as_object_mut()
            .unwrap()
            .insert("future_field".to_string(), serde_json::json!(true));

        let error = serde_json::from_value::<BenchReport>(json).unwrap_err();

        assert!(error.to_string().contains("future_field"), "{error}");
    }
}
