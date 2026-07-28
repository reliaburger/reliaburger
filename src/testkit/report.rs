//! What a test run produced.
//!
//! These types are the `relish test --output json` contract, so they carry a
//! `schema_version` and are pinned by snapshot tests. Renaming a field here
//! breaks somebody's CI, which makes this an API rather than an
//! implementation detail.

use serde::{Deserialize, Serialize};

/// The schema version of [`TestReport`]. Bump on any incompatible change.
///
/// Version two replaces the ambiguous `timed_out` fifth outcome with
/// `unknown`, records cleanup independently, and adds acceptance profiles.
pub const REPORT_SCHEMA_VERSION: u32 = 2;

/// Acceptance profile selected by the operator.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TestProfile {
    /// Local iteration. A capability known to be absent may skip.
    #[default]
    Development,
    /// Complete rootful-runc acceptance.
    FullRunc,
    /// Complete Apple Container acceptance.
    FullApple,
    /// Host-process behaviour, kept separate from OCI acceptance.
    ProcessGrill,
}

impl std::str::FromStr for TestProfile {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "development" => Ok(Self::Development),
            "full-runc" => Ok(Self::FullRunc),
            "full-apple" => Ok(Self::FullApple),
            "process-grill" => Ok(Self::ProcessGrill),
            other => Err(format!(
                "unknown test profile {other:?}; valid profiles: development, full-runc, full-apple, process-grill"
            )),
        }
    }
}

impl std::fmt::Display for TestProfile {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Development => "development",
            Self::FullRunc => "full-runc",
            Self::FullApple => "full-apple",
            Self::ProcessGrill => "process-grill",
        })
    }
}

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
/// Four outcomes, not two. `Skipped` requires a known absent capability;
/// timeouts and missing evidence are `Unknown`, never green.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum TestOutcome {
    Pass,
    Fail {
        reason: String,
    },
    Skipped {
        /// Known capability which was absent.
        capability: crate::bun::capabilities::Capability,
        reason: String,
    },
    Unknown {
        kind: UnknownKind,
        reason: String,
    },
}

impl TestOutcome {
    /// Construct the only valid timeout outcome.
    pub fn timed_out(operation: &str, timeout_ms: u64) -> Self {
        Self::Unknown {
            kind: UnknownKind::TimedOut,
            reason: format!("{operation} exceeded its {timeout_ms} ms deadline"),
        }
    }
}

/// Why a case could not establish pass or fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnknownKind {
    TimedOut,
    CollectorFailed,
    MissingEvidence,
    AmbiguousEvidence,
    Panicked,
}

/// How directly one evidence item supports its statement.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    Observed,
    Inferred,
    Unknown,
}

/// One auditable fact used by a case verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestEvidence {
    pub kind: EvidenceKind,
    pub source: String,
    pub observed_at: String,
    pub detail: String,
}

/// Result of cleaning resources independently of the case verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum CleanupOutcome {
    Confirmed,
    NotRequired,
    Failed { reason: String },
    Unknown { reason: String },
}

/// One case's result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TestCaseResult {
    pub name: String,
    pub group: TestGroup,
    /// Whether this profile requires the case. A required skip rejects a run.
    pub required: bool,
    pub started_at: String,
    pub finished_at: String,
    pub deadline_at: String,
    pub outcome: TestOutcome,
    pub duration_ms: u64,
    pub evidence: Vec<TestEvidence>,
    pub cleanup: CleanupOutcome,
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
    pub profile: TestProfile,
    pub total: u32,
    pub passed: u32,
    pub failed: u32,
    pub skipped: u32,
    pub unknown: u32,
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
        profile: TestProfile,
    ) -> Self {
        let passed = results
            .iter()
            .filter(|r| r.outcome == TestOutcome::Pass)
            .count() as u32;
        let failed = results
            .iter()
            .filter(|result| matches!(result.outcome, TestOutcome::Fail { .. }))
            .count() as u32;
        let skipped = results
            .iter()
            .filter(|r| matches!(r.outcome, TestOutcome::Skipped { .. }))
            .count() as u32;
        let unknown = results
            .iter()
            .filter(|result| matches!(result.outcome, TestOutcome::Unknown { .. }))
            .count() as u32;
        Self {
            schema_version: REPORT_SCHEMA_VERSION,
            started_at,
            duration_ms,
            cluster_nodes,
            chaos,
            profile,
            total: results.len() as u32,
            passed,
            failed,
            skipped,
            unknown,
            results,
        }
    }

    /// Whether the run should exit non-zero.
    pub fn failed_any(&self) -> bool {
        self.results.iter().any(|result| {
            matches!(
                result.outcome,
                TestOutcome::Fail { .. } | TestOutcome::Unknown { .. }
            ) || (result.required && matches!(result.outcome, TestOutcome::Skipped { .. }))
                || matches!(
                    result.cleanup,
                    CleanupOutcome::Failed { .. } | CleanupOutcome::Unknown { .. }
                )
                || (matches!(result.outcome, TestOutcome::Pass)
                    && !result
                        .evidence
                        .iter()
                        .any(|evidence| evidence.kind == EvidenceKind::Observed))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn result(
        name: &str,
        outcome: TestOutcome,
        required: bool,
        cleanup: CleanupOutcome,
    ) -> TestCaseResult {
        let evidence = matches!(outcome, TestOutcome::Pass)
            .then(|| TestEvidence {
                kind: EvidenceKind::Observed,
                source: "case".to_string(),
                observed_at: "2026-07-28T10:00:00Z".to_string(),
                detail: "live assertion completed".to_string(),
            })
            .into_iter()
            .collect();
        TestCaseResult {
            name: name.to_string(),
            group: TestGroup::Scheduling,
            required,
            started_at: "2026-07-28T10:00:00Z".to_string(),
            finished_at: "2026-07-28T10:00:01Z".to_string(),
            deadline_at: "2026-07-28T10:02:00Z".to_string(),
            outcome,
            duration_ms: 10,
            evidence,
            cleanup,
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

    #[test]
    fn outcome_json_has_exactly_four_states() {
        let outcomes = [
            TestOutcome::Pass,
            TestOutcome::Fail {
                reason: "wrong state".to_string(),
            },
            TestOutcome::Skipped {
                capability: crate::bun::capabilities::Capability::Ingress,
                reason: "ingress unavailable".to_string(),
            },
            TestOutcome::timed_out("case", 1_000),
        ];
        let states: Vec<_> = outcomes
            .into_iter()
            .map(|outcome| serde_json::to_value(outcome).unwrap()["status"].clone())
            .collect();
        assert_eq!(states, ["pass", "fail", "skipped", "unknown"]);
    }

    #[test]
    fn counters_never_merge_failed_skipped_and_unknown() {
        let results = vec![
            result("pass", TestOutcome::Pass, true, CleanupOutcome::Confirmed),
            result(
                "skip",
                TestOutcome::Skipped {
                    capability: crate::bun::capabilities::Capability::Ingress,
                    reason: "ingress not bound".into(),
                },
                false,
                CleanupOutcome::NotRequired,
            ),
            result(
                "fail",
                TestOutcome::Fail {
                    reason: "expected 3, saw 1".into(),
                },
                true,
                CleanupOutcome::Confirmed,
            ),
            result(
                "unknown",
                TestOutcome::timed_out("case", 1_000),
                true,
                CleanupOutcome::Unknown {
                    reason: "lease unreachable".to_string(),
                },
            ),
        ];
        let report = TestReport::from_results(
            results,
            "2026-07-28T10:00:00Z".into(),
            1234,
            3,
            false,
            TestProfile::FullRunc,
        );

        assert_eq!(report.total, 4);
        assert_eq!(report.passed, 1);
        assert_eq!(report.failed, 1);
        assert_eq!(report.skipped, 1);
        assert_eq!(report.unknown, 1);
        assert_eq!(
            report.passed + report.failed + report.skipped + report.unknown,
            report.total
        );
        assert!(report.failed_any());
    }

    #[test]
    fn full_profile_rejects_required_skip_unknown_evidence_and_cleanup() {
        let required_skip = result(
            "required",
            TestOutcome::Skipped {
                capability: crate::bun::capabilities::Capability::Ebpf,
                reason: "eBPF absent".to_string(),
            },
            true,
            CleanupOutcome::NotRequired,
        );
        let mut no_observation = result(
            "unproven",
            TestOutcome::Pass,
            true,
            CleanupOutcome::Confirmed,
        );
        no_observation.evidence.clear();
        for case in [required_skip, no_observation] {
            let report = TestReport::from_results(
                vec![case],
                "2026-07-28T10:00:00Z".into(),
                1,
                3,
                false,
                TestProfile::FullRunc,
            );
            assert!(report.failed_any());
        }
    }

    #[test]
    fn development_allows_only_a_known_optional_skip() {
        let report = TestReport::from_results(
            vec![result(
                "optional",
                TestOutcome::Skipped {
                    capability: crate::bun::capabilities::Capability::Ingress,
                    reason: "ingress absent".to_string(),
                },
                false,
                CleanupOutcome::NotRequired,
            )],
            "2026-07-28T10:00:00Z".into(),
            1,
            1,
            false,
            TestProfile::Development,
        );
        assert!(!report.failed_any());
    }

    #[test]
    fn the_report_json_shape_is_stable() {
        let report = TestReport::from_results(
            vec![
                result("passes", TestOutcome::Pass, true, CleanupOutcome::Confirmed),
                result(
                    "skips",
                    TestOutcome::Skipped {
                        capability: crate::bun::capabilities::Capability::Ingress,
                        reason: "capability ingress unavailable".into(),
                    },
                    false,
                    CleanupOutcome::NotRequired,
                ),
                result(
                    "fails",
                    TestOutcome::Fail {
                        reason: "expected Succeeded, saw Failed".into(),
                    },
                    true,
                    CleanupOutcome::Confirmed,
                ),
            ],
            "2026-07-28T12:00:00Z".into(),
            4321,
            3,
            false,
            TestProfile::FullRunc,
        );
        insta::assert_json_snapshot!(report);
    }
}
