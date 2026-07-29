//! `relish test` — run the built-in integration suite against the cluster.
//!
//! This is the operator-facing front door to [`crate::testkit`]. It asks the
//! node what it can do, selects the cases that suit, runs them through the
//! harness, and renders the report — human-readable by default, JSON for CI.
//!
//! The exit code is the point. A run with a failure exits non-zero so a
//! pipeline can gate on it, and that "found problems" signal is distinct from
//! "the tool couldn't run at all" (a `RelishError`, exit 1 via the usual
//! path). A development profile may accept a typed optional skip; a full
//! profile rejects required skips, unknown evidence, timeouts and unconfirmed
//! cleanup.

use crate::testkit::{self, RunConfig, TestGroup, TestOutcome, TestProfile, TestReport};

use super::client::BunClient;
use super::fault::parse_duration;
use super::output::OutputFormat;
use super::{CommandOutcome, RelishError};

/// Everything `relish test` takes from the command line.
pub struct TestArgs {
    /// `--filter scheduling,firewall`; `None` runs every group.
    pub filter: Option<String>,
    /// `--parallel`: maximum cases running at once.
    pub parallel: usize,
    /// `--timeout`: per-case budget, e.g. `"120s"`.
    pub timeout: String,
    /// `--chaos`: run the chaos suite instead of the integration suite.
    pub chaos: bool,
    /// `--yes`: acknowledge real fault injection without expanding authority.
    pub yes: bool,
    /// Acceptance policy for required cases and known skips.
    pub profile: String,
    /// `--namespace`: readable base for each case's isolated namespace.
    pub namespace: Option<String>,
    /// `--output`: how to render the report.
    pub output: OutputFormat,
}

/// Run the suite against the local agent (honouring the global endpoint
/// override).
pub async fn run(args: TestArgs) -> Result<CommandOutcome, RelishError> {
    run_with_client(args, &BunClient::default_local()).await
}

async fn run_with_client(
    args: TestArgs,
    client: &BunClient,
) -> Result<CommandOutcome, RelishError> {
    let timeout = parse_duration(&args.timeout)?;
    if timeout.is_zero() {
        return Err(RelishError::InvalidFlag {
            flag: "timeout".to_string(),
            reason: "must be greater than zero".to_string(),
        });
    }
    let profile: TestProfile = args
        .profile
        .parse()
        .map_err(|reason| RelishError::InvalidFlag {
            flag: "profile".to_string(),
            reason,
        })?;
    if let Some(namespace) = &args.namespace
        && (!testkit::TestContext::is_test_namespace(namespace)
            || !testkit::lease::valid_test_namespace(&format!("{namespace}-00")))
    {
        return Err(RelishError::InvalidFlag {
            flag: "namespace".to_string(),
            reason:
                "must be a DNS-label prefix beginning rbtest- and leave room for a per-case suffix"
                    .to_string(),
        });
    }

    // The whole harness keys off capabilities, so a node we can't ask is a
    // hard error, not an empty run that looks like a pass.
    let capabilities = client
        .capabilities()
        .await
        .map_err(|error| RelishError::ApiError {
            status: 0,
            body: format!("could not read cluster capabilities (is the agent running?): {error}"),
        })?;

    if args.chaos {
        confirm_chaos(&capabilities, args.yes)?;
    }

    let cases = if args.chaos {
        testkit::chaos_cases()
    } else {
        let groups = testkit::parse_filter(args.filter.as_deref().unwrap_or(""))
            .map_err(|body| RelishError::ApiError { status: 0, body })?;
        testkit::select(testkit::all_cases(), &groups)
    };

    let report = testkit::run(
        cases,
        RunConfig {
            client: client.clone(),
            capabilities,
            run_id: generate_run_id(),
            // Two individually safe node faults can be unsafe together.
            parallel: if args.chaos { 1 } else { args.parallel },
            timeout,
            chaos: args.chaos,
            profile,
            fixed_namespace: args.namespace.clone(),
            lease_ownership: testkit::runner::LeaseOwnership::Required,
        },
    )
    .await;

    render(&report, args.output)?;

    Ok(if report.failed_any() {
        CommandOutcome::Problems
    } else {
        CommandOutcome::Clean
    })
}

fn confirm_chaos(
    capabilities: &crate::bun::capabilities::ClusterCapabilities,
    yes: bool,
) -> Result<(), RelishError> {
    use std::io::{IsTerminal, Write};

    let is_tty = std::io::stdin().is_terminal();
    match testkit::chaos::chaos_preflight(capabilities, testkit::chaos::ChaosFlags { yes }, is_tty)
    {
        Ok(()) => Ok(()),
        Err(testkit::chaos::RefusalReason::InteractiveConfirmation) => {
            eprint!(
                "This will inject real faults into cluster '{}'. Type 'yes' to continue: ",
                capabilities.cluster_name
            );
            std::io::stderr().flush().map_err(RelishError::Io)?;
            let mut answer = String::new();
            std::io::stdin()
                .read_line(&mut answer)
                .map_err(RelishError::Io)?;
            if answer.trim() == "yes" {
                Ok(())
            } else {
                Err(RelishError::ApiError {
                    status: 0,
                    body: "chaos cancelled; confirmation was not exactly 'yes'".to_string(),
                })
            }
        }
        Err(reason) => Err(RelishError::ApiError {
            status: 0,
            body: reason.to_string(),
        }),
    }
}

/// A short lowercase-hex tag, unique enough per invocation to keep two
/// concurrent runs' namespaces apart.
fn generate_run_id() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    // The low 32 bits are eight hex digits — plenty of spread for the minutes
    // between two `relish test` calls, and short enough to read in a
    // namespace name.
    format!("{:x}", seconds & 0xffff_ffff)
}

fn render(report: &TestReport, output: OutputFormat) -> Result<(), RelishError> {
    match output {
        OutputFormat::Json => {
            let json = serde_json::to_string_pretty(report).map_err(RelishError::SerialiseJson)?;
            println!("{json}");
        }
        OutputFormat::Yaml => {
            let yaml = serde_yaml::to_string(report).map_err(RelishError::SerialiseYaml)?;
            print!("{yaml}");
        }
        OutputFormat::Human => print!("{}", render_human(report)),
    }
    Ok(())
}

/// Render a report as aligned plain text.
///
/// Kept a pure `&TestReport -> String` so it snapshot-tests without a cluster.
/// No colour and no TTY detection: the labels carry the meaning, and a report
/// that reads the same in a terminal and in a CI log is one fewer thing to
/// reason about.
fn render_human(report: &TestReport) -> String {
    use std::fmt::Write;

    let suite = if report.chaos {
        "chaos scenarios"
    } else {
        "tests"
    };
    let mut out = String::new();
    let _ = writeln!(
        out,
        "running {} {suite} against a {}-node cluster\n",
        report.total, report.cluster_nodes
    );

    let mut last_group: Option<TestGroup> = None;
    for result in &report.results {
        if last_group != Some(result.group) {
            let _ = writeln!(out, "{}", result.group);
            last_group = Some(result.group);
        }
        let (mark, detail) = match &result.outcome {
            TestOutcome::Pass => ("PASS", String::new()),
            TestOutcome::Fail { reason } => ("FAIL", format!("  {reason}")),
            TestOutcome::Skipped { reason, .. } => ("SKIP", format!("  {reason}")),
            TestOutcome::Unknown { reason, .. } => ("UNKN", format!("  {reason}")),
        };
        let _ = writeln!(
            out,
            "  {mark}  {}  ({} ms){detail}",
            result.name, result.duration_ms
        );
    }

    let seconds = report.duration_ms as f64 / 1000.0;
    let _ = writeln!(
        out,
        "\n{} {suite}: {} passed, {} failed, {} skipped, {} unknown  ({:.1}s)",
        report.total, report.passed, report.failed, report.skipped, report.unknown, seconds
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::report::{
        CleanupOutcome, EvidenceKind, TestCaseResult, TestEvidence, TestGroup, TestOutcome,
        UnknownKind,
    };
    use axum::routing::get;
    use axum::{Json, Router};

    fn result(
        name: &str,
        group: TestGroup,
        outcome: TestOutcome,
        duration_ms: u64,
    ) -> TestCaseResult {
        let evidence = matches!(outcome, TestOutcome::Pass)
            .then(|| TestEvidence {
                kind: EvidenceKind::Observed,
                source: name.to_string(),
                observed_at: "2026-07-28T12:00:00Z".to_string(),
                detail: "assertion observed".to_string(),
            })
            .into_iter()
            .collect();
        TestCaseResult {
            name: name.to_string(),
            group,
            required: false,
            started_at: "2026-07-28T12:00:00Z".to_string(),
            finished_at: "2026-07-28T12:00:01Z".to_string(),
            deadline_at: "2026-07-28T12:02:00Z".to_string(),
            outcome,
            duration_ms,
            evidence,
            cleanup: CleanupOutcome::NotRequired,
        }
    }

    fn sample_report() -> TestReport {
        TestReport::from_results(
            vec![
                result(
                    "schedule_fixed_replicas_across_nodes",
                    TestGroup::Scheduling,
                    TestOutcome::Pass,
                    120,
                ),
                result(
                    "schedule_respects_required_placement_label",
                    TestGroup::Scheduling,
                    TestOutcome::Skipped {
                        capability: crate::bun::capabilities::Capability::MultiNode,
                        reason: "requires multi_node".to_string(),
                    },
                    0,
                ),
                result(
                    "resolve_returns_vip_and_healthy_backends",
                    TestGroup::ServiceDiscovery,
                    TestOutcome::Fail {
                        reason: "expected 2 backends, saw 1".to_string(),
                    },
                    80,
                ),
                result(
                    "hanging_health_check_marks_instance_unhealthy",
                    TestGroup::HealthChecks,
                    TestOutcome::Unknown {
                        kind: UnknownKind::TimedOut,
                        reason: "case exceeded its 120000 ms deadline".to_string(),
                    },
                    120_000,
                ),
            ],
            "2026-07-26T12:00:00Z".to_string(),
            4321,
            3,
            false,
            TestProfile::Development,
        )
    }

    #[test]
    fn a_run_id_is_short_lowercase_hex() {
        let id = generate_run_id();
        assert!(!id.is_empty());
        assert!(id.len() <= 8, "{id}");
        assert!(
            id.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "{id}"
        );
    }

    #[test]
    fn the_human_report_groups_and_marks_each_outcome() {
        let text = render_human(&sample_report());
        insta::assert_snapshot!(text);
    }

    #[test]
    fn a_failure_maps_to_problems_a_clean_run_to_clean() {
        assert!(sample_report().failed_any());
        // Clean report: only a pass and a skip.
        let clean = TestReport::from_results(
            vec![
                result("passes", TestGroup::Scheduling, TestOutcome::Pass, 10),
                result(
                    "skips",
                    TestGroup::Ingress,
                    TestOutcome::Skipped {
                        capability: crate::bun::capabilities::Capability::Ingress,
                        reason: "no ingress".to_string(),
                    },
                    0,
                ),
            ],
            "2026-07-26T12:00:00Z".to_string(),
            10,
            1,
            false,
            TestProfile::Development,
        );
        assert!(!clean.failed_any());
    }

    #[test]
    fn command_outcome_exit_codes() {
        assert_eq!(CommandOutcome::Clean.exit_code(), 0);
        assert_eq!(CommandOutcome::Problems.exit_code(), 1);
        assert_eq!(CommandOutcome::Warnings.exit_code(), 2);
    }

    #[tokio::test]
    async fn chaos_against_a_one_node_harness_refuses_before_running_cases() {
        let capabilities = crate::bun::capabilities::ClusterCapabilities {
            node_count: 1,
            cluster_name: "one-node".to_string(),
            ..crate::bun::capabilities::ClusterCapabilities::default()
        };
        let app = Router::new().route(
            "/v1/capabilities",
            get(move || {
                let capabilities = capabilities.clone();
                async move { Json(capabilities) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let error = run_with_client(
            TestArgs {
                filter: None,
                parallel: 4,
                timeout: "120s".to_string(),
                chaos: true,
                yes: true,
                profile: "development".to_string(),
                namespace: None,
                output: OutputFormat::Human,
            },
            &BunClient::new(&format!("http://{address}")),
        )
        .await
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("chaos suite requires at least 3 nodes (found 1)"),
            "{error}"
        );
    }
}
