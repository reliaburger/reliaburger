//! What a test run produced.
//!
//! These types are the `relish test --output json` contract, so they carry a
//! `schema_version` and are pinned by snapshot tests. Renaming a field here
//! breaks somebody's CI, which makes this an API rather than an
//! implementation detail.

use serde::{Deserialize, Serialize};

/// The schema version of [`TestReport`]. Bump on any incompatible change.
pub const REPORT_SCHEMA_VERSION: u32 = 1;

/// Which family a test case belongs to. `--filter` selects by these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TestGroup {
    Scheduling,
    ServiceDiscovery,
    Deployments,
    HealthChecks,
    SecretsConfig,
    Firewall,
    WorkloadIdentity,
    Ingress,
    Volumes,
    ProcessWorkloads,
    Jobs,
    ImageRegistry,
    ClusterCoordination,
    /// The chaos scenarios, selected by `--chaos` rather than by name.
    Chaos,
}

impl TestGroup {
    /// Every group, in the order a report renders them.
    pub const ALL: &'static [TestGroup] = &[
        TestGroup::Scheduling,
        TestGroup::ServiceDiscovery,
        TestGroup::Deployments,
        TestGroup::HealthChecks,
        TestGroup::SecretsConfig,
        TestGroup::Firewall,
        TestGroup::WorkloadIdentity,
        TestGroup::Ingress,
        TestGroup::Volumes,
        TestGroup::ProcessWorkloads,
        TestGroup::Jobs,
        TestGroup::ImageRegistry,
        TestGroup::ClusterCoordination,
        TestGroup::Chaos,
    ];

    /// The kebab-case name used on the command line and the wire.
    pub fn as_str(&self) -> &'static str {
        match self {
            TestGroup::Scheduling => "scheduling",
            TestGroup::ServiceDiscovery => "service-discovery",
            TestGroup::Deployments => "deployments",
            TestGroup::HealthChecks => "health-checks",
            TestGroup::SecretsConfig => "secrets-config",
            TestGroup::Firewall => "firewall",
            TestGroup::WorkloadIdentity => "workload-identity",
            TestGroup::Ingress => "ingress",
            TestGroup::Volumes => "volumes",
            TestGroup::ProcessWorkloads => "process-workloads",
            TestGroup::Jobs => "jobs",
            TestGroup::ImageRegistry => "image-registry",
            TestGroup::ClusterCoordination => "cluster-coordination",
            TestGroup::Chaos => "chaos",
        }
    }
}

impl std::fmt::Display for TestGroup {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl std::str::FromStr for TestGroup {
    type Err = String;

    /// Parse a group name, listing every valid one on failure.
    ///
    /// A typo in `--filter` is the most likely way someone meets this type,
    /// and "unknown group" without the alternatives means a trip to the
    /// docs. The error carries them.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        TestGroup::ALL
            .iter()
            .copied()
            .find(|group| group.as_str() == s)
            .ok_or_else(|| {
                let valid: Vec<&str> = TestGroup::ALL.iter().map(|g| g.as_str()).collect();
                format!(
                    "unknown test group {s:?}; valid groups: {}",
                    valid.join(", ")
                )
            })
    }
}

/// How one case finished.
///
/// Four outcomes, not two. `Skipped` is the one that earns its keep: a case
/// whose capability is absent has neither passed nor failed, and folding it
/// into either is a lie — a green that didn't run, or a red that was never
/// going to run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum TestOutcome {
    Passed,
    Failed { message: String },
    Skipped { reason: String },
    TimedOut,
}

impl TestOutcome {
    /// Whether this outcome should fail the run (and the process exit code).
    ///
    /// A skip must never fail a run: the whole point is that an unwired
    /// subsystem is reported, not punished.
    pub fn is_failure(&self) -> bool {
        matches!(self, TestOutcome::Failed { .. } | TestOutcome::TimedOut)
    }
}

/// One case's result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TestCaseResult {
    pub name: String,
    pub group: TestGroup,
    pub outcome: TestOutcome,
    pub duration_ms: u64,
}

/// The whole run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TestReport {
    pub schema_version: u32,
    /// RFC 3339, so a report is self-describing without a filename.
    pub started_at: String,
    pub duration_ms: u64,
    pub cluster_nodes: u32,
    pub chaos: bool,
    pub total: u32,
    pub passed: u32,
    pub failed: u32,
    pub skipped: u32,
    pub results: Vec<TestCaseResult>,
}

impl TestReport {
    /// Aggregate results into a report.
    ///
    /// The counters are derived here rather than incremented as tests finish,
    /// so they cannot disagree with `results` — a summary line that
    /// contradicts the list below it destroys trust in both.
    pub fn from_results(
        results: Vec<TestCaseResult>,
        started_at: String,
        duration_ms: u64,
        cluster_nodes: u32,
        chaos: bool,
    ) -> Self {
        let passed = results
            .iter()
            .filter(|r| r.outcome == TestOutcome::Passed)
            .count() as u32;
        let failed = results.iter().filter(|r| r.outcome.is_failure()).count() as u32;
        let skipped = results
            .iter()
            .filter(|r| matches!(r.outcome, TestOutcome::Skipped { .. }))
            .count() as u32;
        Self {
            schema_version: REPORT_SCHEMA_VERSION,
            started_at,
            duration_ms,
            cluster_nodes,
            chaos,
            total: results.len() as u32,
            passed,
            failed,
            skipped,
            results,
        }
    }

    /// Whether the run should exit non-zero.
    pub fn failed_any(&self) -> bool {
        self.failed > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn result(name: &str, group: TestGroup, outcome: TestOutcome) -> TestCaseResult {
        TestCaseResult {
            name: name.to_string(),
            group,
            outcome,
            duration_ms: 10,
        }
    }

    #[test]
    fn group_names_round_trip_through_their_kebab_case_form() {
        for group in TestGroup::ALL {
            let name = group.as_str();
            assert_eq!(TestGroup::from_str(name).unwrap(), *group, "{name}");
            // The wire form and the CLI form must be the same string, or a
            // JSON report and a --filter argument disagree about a name.
            let json = serde_json::to_string(group).unwrap();
            assert_eq!(json.trim_matches('"'), name);
        }
    }

    #[test]
    fn an_unknown_group_lists_the_valid_ones() {
        let error = TestGroup::from_str("scheduling-typo").unwrap_err();
        assert!(error.contains("scheduling-typo"), "{error}");
        assert!(error.contains("scheduling"), "{error}");
        assert!(error.contains("cluster-coordination"), "{error}");
    }

    /// A skip has neither passed nor failed. Folding it into either would be
    /// a green that didn't run or a red that was never going to.
    #[test]
    fn only_failures_and_timeouts_fail_a_run() {
        assert!(!TestOutcome::Passed.is_failure());
        assert!(
            !TestOutcome::Skipped {
                reason: "no ebpf".into()
            }
            .is_failure()
        );
        assert!(
            TestOutcome::Failed {
                message: "boom".into()
            }
            .is_failure()
        );
        assert!(TestOutcome::TimedOut.is_failure());
    }

    #[test]
    fn counters_are_derived_from_the_results() {
        let results = vec![
            result("a", TestGroup::Scheduling, TestOutcome::Passed),
            result("b", TestGroup::Scheduling, TestOutcome::Passed),
            result(
                "c",
                TestGroup::Ingress,
                TestOutcome::Skipped {
                    reason: "ingress not bound".into(),
                },
            ),
            result(
                "d",
                TestGroup::Jobs,
                TestOutcome::Failed {
                    message: "expected 3, saw 1".into(),
                },
            ),
            result("e", TestGroup::Jobs, TestOutcome::TimedOut),
        ];
        let report =
            TestReport::from_results(results, "2026-07-26T00:00:00Z".into(), 1234, 3, false);

        assert_eq!(report.total, 5);
        assert_eq!(report.passed, 2);
        assert_eq!(report.failed, 2, "a timeout is a failure");
        assert_eq!(report.skipped, 1);
        // The parts must equal the whole, or the summary contradicts the list.
        assert_eq!(report.passed + report.failed + report.skipped, report.total);
        assert!(report.failed_any());
    }

    #[test]
    fn a_run_of_only_skips_does_not_fail() {
        let results = vec![result(
            "a",
            TestGroup::Firewall,
            TestOutcome::Skipped {
                reason: "no ebpf".into(),
            },
        )];
        let report = TestReport::from_results(results, "2026-07-26T00:00:00Z".into(), 1, 1, false);
        assert_eq!(report.skipped, 1);
        assert!(
            !report.failed_any(),
            "an entirely-skipped run is not a failed run"
        );
    }

    #[test]
    fn the_report_json_shape_is_stable() {
        let report = TestReport::from_results(
            vec![
                result("passes", TestGroup::Scheduling, TestOutcome::Passed),
                result(
                    "skips",
                    TestGroup::Ingress,
                    TestOutcome::Skipped {
                        reason: "capability ingress unavailable".into(),
                    },
                ),
                result(
                    "fails",
                    TestGroup::Jobs,
                    TestOutcome::Failed {
                        message: "expected Succeeded, saw Failed".into(),
                    },
                ),
            ],
            "2026-07-26T12:00:00Z".into(),
            4321,
            3,
            false,
        );
        insta::assert_json_snapshot!(report);
    }
}
