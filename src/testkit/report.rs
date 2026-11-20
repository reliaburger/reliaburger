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

/// Most steps a failure capture keeps, newest last.
pub const MAX_CAPTURED_STEPS: usize = 16;
/// Most applications a failure capture asks about.
pub const MAX_CAPTURED_APPS: usize = 8;
/// Most instances a failure capture keeps.
pub const MAX_CAPTURED_INSTANCES: usize = 32;
/// Most events a failure capture keeps, newest last.
pub const MAX_CAPTURED_EVENTS: usize = 20;
/// Most collection errors a failure capture keeps.
pub const MAX_CAPTURED_ERRORS: usize = 8;
/// Longest free-text field a failure capture keeps, in characters.
pub const MAX_CAPTURED_TEXT: usize = 300;

/// One named step of a case: staging an image, an apply, a wait, an exec.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaseStep {
    pub name: String,
    /// When the step began, counted from the start of the case.
    pub started_after_ms: u64,
    /// How long the step took. `None` means it never finished: the case
    /// failed, or was cancelled at its deadline, inside it.
    pub duration_ms: Option<u64>,
}

/// One instance of a failing case's applications, as a node reported it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapturedInstance {
    pub node: String,
    pub id: String,
    pub app: String,
    pub state: String,
    pub restart_count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub age_seconds: Option<u64>,
}

/// One recent cluster event about a failing case's applications.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapturedEvent {
    /// Unix seconds.
    pub timestamp: u64,
    pub severity: crate::bun::events::EventSeverity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    pub message: String,
}

/// What the cluster showed of a failing case's resources at the moment it
/// failed or timed out, taken before the runner releases its lease (#2131).
///
/// Cleanup removes the very instances a failure is about, so evidence taken
/// afterwards can't say whether an app never started or hung once running.
/// Every list is bounded (see [`FailureState::bound`]), so a capture can't
/// swell a report.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureState {
    pub captured_at: String,
    /// The step the case was in: the newest one still open, or else the last
    /// one it finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    /// The case's named steps, oldest first.
    pub steps: Vec<CaseStep>,
    /// The applications the case applied or that run in its namespace.
    pub apps: Vec<String>,
    /// Their instances on every reachable node.
    pub instances: Vec<CapturedInstance>,
    /// The newest deployment of each application, as the cluster records it.
    pub deployments: Vec<String>,
    /// What the server said of the case's lease.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease: Option<String>,
    /// Recent events about the applications, oldest first.
    pub events: Vec<CapturedEvent>,
    /// Reads that failed, so a missing list is never mistaken for an empty one.
    pub errors: Vec<String>,
    /// Whether any list or text was cut to its bound.
    pub truncated: bool,
}

impl FailureState {
    /// Cut every list and free-text field to its bound, keeping the newest
    /// steps and events, and record whether anything was cut.
    pub fn bound(&mut self) {
        let mut cut = false;
        cut |= keep_newest(&mut self.steps, MAX_CAPTURED_STEPS);
        cut |= keep_oldest(&mut self.apps, MAX_CAPTURED_APPS);
        cut |= keep_oldest(&mut self.instances, MAX_CAPTURED_INSTANCES);
        cut |= keep_oldest(&mut self.deployments, MAX_CAPTURED_APPS);
        cut |= keep_newest(&mut self.events, MAX_CAPTURED_EVENTS);
        cut |= keep_oldest(&mut self.errors, MAX_CAPTURED_ERRORS);
        for step in &mut self.steps {
            cut |= bound_text(&mut step.name);
        }
        if let Some(step) = &mut self.step {
            cut |= bound_text(step);
        }
        if let Some(lease) = &mut self.lease {
            cut |= bound_text(lease);
        }
        for instance in &mut self.instances {
            if let Some(reason) = &mut instance.reason {
                cut |= bound_text(reason);
            }
        }
        for text in self.deployments.iter_mut().chain(self.errors.iter_mut()) {
            cut |= bound_text(text);
        }
        for event in &mut self.events {
            cut |= bound_text(&mut event.message);
        }
        self.truncated |= cut;
    }
}

fn keep_newest<T>(items: &mut Vec<T>, limit: usize) -> bool {
    let excess = items.len().saturating_sub(limit);
    items.drain(..excess);
    excess > 0
}

fn keep_oldest<T>(items: &mut Vec<T>, limit: usize) -> bool {
    let cut = items.len() > limit;
    items.truncate(limit);
    cut
}

/// Cut `text` to [`MAX_CAPTURED_TEXT`] characters, marking the cut with `…`.
fn bound_text(text: &mut String) -> bool {
    match text.char_indices().nth(MAX_CAPTURED_TEXT) {
        Some((end, _)) => {
            text.truncate(end);
            text.push('…');
            true
        }
        None => false,
    }
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
    /// What the cluster showed of the case's resources when it failed or
    /// timed out, captured before cleanup. Absent for a pass or a skip.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_state: Option<FailureState>,
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
            failure_state: None,
        }
    }

    fn captured_event(message: &str) -> CapturedEvent {
        CapturedEvent {
            timestamp: 1_795_148_235,
            severity: crate::bun::events::EventSeverity::Info,
            node: Some("node-1".to_string()),
            app: Some("registry-web".to_string()),
            message: message.to_string(),
        }
    }

    #[test]
    fn a_failure_state_keeps_the_newest_steps_and_events_within_its_bounds() {
        let mut state = FailureState {
            steps: (0..40)
                .map(|index| CaseStep {
                    name: format!("step {index}"),
                    started_after_ms: index,
                    duration_ms: Some(1),
                })
                .collect(),
            events: (0..50)
                .map(|index| captured_event(&format!("event {index}")))
                .collect(),
            instances: (0..100)
                .map(|index| CapturedInstance {
                    node: "node-1".to_string(),
                    id: format!("web-{index}"),
                    app: "web".to_string(),
                    state: "pending".to_string(),
                    restart_count: 0,
                    exit_code: None,
                    reason: Some("x".repeat(5_000)),
                    age_seconds: None,
                })
                .collect(),
            ..FailureState::default()
        };

        state.bound();

        assert!(state.truncated);
        assert_eq!(state.steps.len(), MAX_CAPTURED_STEPS);
        assert_eq!(state.steps.last().unwrap().name, "step 39");
        assert_eq!(state.events.len(), MAX_CAPTURED_EVENTS);
        assert_eq!(state.events.last().unwrap().message, "event 49");
        assert_eq!(state.instances.len(), MAX_CAPTURED_INSTANCES);
        let reason = state.instances[0].reason.as_deref().unwrap();
        assert_eq!(reason.chars().count(), MAX_CAPTURED_TEXT + 1, "{reason}");
        assert!(reason.ends_with('…'));
        // The whole capture stays small enough to ride in every pulse snapshot.
        let size = serde_json::to_string(&state).unwrap().len();
        assert!(size < 32 * 1024, "{size} bytes");
    }

    #[test]
    fn a_small_failure_state_is_kept_whole() {
        let mut state = FailureState {
            step: Some("exec in registry-web".to_string()),
            events: vec![captured_event("pulled image")],
            ..FailureState::default()
        };
        let before = state.clone();
        state.bound();
        assert_eq!(state, before);
        assert!(!state.truncated);
    }

    #[test]
    fn the_failure_state_json_shape_is_stable() {
        let state = FailureState {
            captured_at: "2026-11-20T04:22:58Z".to_string(),
            step: Some("exec in registry-web".to_string()),
            steps: vec![
                CaseStep {
                    name: "stage pinned runnable image".to_string(),
                    started_after_ms: 12,
                    duration_ms: Some(3_100),
                },
                CaseStep {
                    name: "exec in registry-web".to_string(),
                    started_after_ms: 9_800,
                    duration_ms: None,
                },
            ],
            apps: vec!["registry-web".to_string()],
            instances: vec![CapturedInstance {
                node: "node-1".to_string(),
                id: "registry-web-0".to_string(),
                app: "registry-web".to_string(),
                state: "running".to_string(),
                restart_count: 0,
                exit_code: None,
                reason: None,
                age_seconds: Some(290),
            }],
            deployments: vec![
                "deployment op-3 is Finished with outcome Some(Completed)".to_string(),
            ],
            lease: Some("lease lease-1 is active".to_string()),
            events: vec![captured_event("registry-web started")],
            errors: Vec::new(),
            truncated: false,
        };
        insta::assert_json_snapshot!(state);
    }

    #[test]
    fn a_passing_result_carries_no_failure_state_field() {
        let passing = result("passes", TestOutcome::Pass, true, CleanupOutcome::Confirmed);
        let json = serde_json::to_value(&passing).unwrap();
        assert!(json.get("failure_state").is_none(), "{json}");
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
