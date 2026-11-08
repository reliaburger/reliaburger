//! `relish status`: is the cluster all right?
//!
//! One screen, the same size on three nodes or three hundred. The first
//! line is the verdict (`HEALTHY`, `DEGRADED` or `DOWN`) with the number of
//! problems behind it; then one line per section of the cluster summary
//! (`GET /v1/cluster/summary`), each ending in the command that lists the
//! detail when something there needs looking at. The lists themselves live
//! in `relish apps` and `relish nodes`.
//!
//! The rendering is pure: [`assess`] (in [`crate::bun::summary::verdict`],
//! which the TUI and Brioche share) turns a summary and the outcome of
//! `wtf`'s checks into lines and a verdict, and [`render`] lays them out, so
//! the snapshot tests run it over fixtures with no cluster.

use std::fmt::Write as _;
use std::io::IsTerminal as _;
use std::time::Duration;

use schemars::JsonSchema;
use serde::Serialize;

use super::agent::envelope::{AgentError, BuildInfo, CommandName, Envelope, NextStep};
use super::client::BunClient;
use super::{CommandOutcome, OutputFormat, RelishError};
use crate::bun::summary::ClusterSummary;
pub use crate::bun::summary::verdict::{
    Line, Mark, Verdict, WtfCheck, assess, grouped, problems, span, verdict,
};

/// How long `relish status` waits for `wtf`'s checks before it shows "not
/// checked in time" (decision D1 in the plan).
pub const WTF_BUDGET: Duration = Duration::from_secs(2);

/// The flags `relish status` takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusArgs {
    /// Skip `wtf`'s checks.
    pub quick: bool,
    /// Exit 1 on `DEGRADED` and 2 on `DOWN`, for scripts and monitoring.
    pub check: bool,
    /// Human, JSON or YAML.
    pub output: OutputFormat,
}

/// `data` for `relish status -o json`.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct StatusData {
    /// `healthy`, `degraded` or `down`.
    pub verdict: Verdict,
    /// How many lines are problems.
    pub problems: usize,
    /// Every line of the screen, in order.
    pub lines: Vec<Line>,
    /// How `wtf`'s checks went.
    pub wtf: WtfCheck,
    /// The cluster summary the lines were drawn from.
    pub summary: ClusterSummary,
}

/// Run `relish status` against the configured endpoint.
pub async fn run(args: StatusArgs) -> Result<CommandOutcome, RelishError> {
    run_with_client(args, &BunClient::default_local()).await
}

/// Run `relish status` with an explicit client.
pub async fn run_with_client(
    args: StatusArgs,
    client: &BunClient,
) -> Result<CommandOutcome, RelishError> {
    let (summary, wtf) = tokio::join!(client.cluster_summary(), async {
        if args.quick {
            WtfCheck::Skipped
        } else {
            run_wtf(client, WTF_BUDGET).await
        }
    });
    let summary = match summary {
        Ok(summary) => summary,
        Err(error) if args.output == OutputFormat::Json => {
            // An agent reads the envelope whether or not the cluster
            // answered; the exit code is the envelope's own.
            let envelope: Envelope<()> = Envelope::failed(
                CommandName::Status,
                BuildInfo::current(),
                AgentError::from(&error),
            );
            let exit = envelope.exit_status().code();
            println!(
                "{}",
                serde_json::to_string_pretty(&envelope).map_err(RelishError::SerialiseJson)?
            );
            return Ok(CommandOutcome::Exited(exit));
        }
        Err(error) => return Err(error),
    };
    let data = status_data(summary, wtf);
    match args.output {
        OutputFormat::Human => print!("{}", render(&data, colour_wanted())),
        OutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(&envelope(&data)).map_err(RelishError::SerialiseJson)?
        ),
        OutputFormat::Yaml => print!(
            "{}",
            yaml_serde::to_string(&envelope(&data)).map_err(RelishError::SerialiseYaml)?
        ),
    }
    Ok(if args.check {
        CommandOutcome::Exited(data.verdict.check_exit_code())
    } else {
        CommandOutcome::Clean
    })
}

/// Run `wtf`'s checks within `budget`.
async fn run_wtf(client: &BunClient, budget: Duration) -> WtfCheck {
    match tokio::time::timeout(budget, super::wtf::collect(client, None)).await {
        Err(_) => WtfCheck::Late {
            budget_seconds: budget.as_secs(),
        },
        Ok(Err(error)) => WtfCheck::Failed {
            reason: error.to_string(),
        },
        Ok(Ok(inputs)) => wtf_check(&super::wtf::diagnose(&inputs)),
    }
}

/// What `wtf`'s `report` adds to the screen: its counts and the first
/// critical finding.
pub fn wtf_check(report: &super::wtf::WtfReport) -> WtfCheck {
    WtfCheck::Checked {
        critical: report.summary.critical_count,
        warning: report.summary.warning_count,
        unknown: report.summary.unknown_count,
        first_critical: report.critical.first().map(|finding| finding.title.clone()),
    }
}

/// Colour only on a terminal, and never when `NO_COLOR` is set.
fn colour_wanted() -> bool {
    std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

/// The answer `relish status -o json` prints: the agent envelope, with the
/// commands on the lines that need attention as `next` steps.
pub fn envelope(data: &StatusData) -> Envelope<&StatusData> {
    let mut envelope = Envelope::answered(CommandName::Status, BuildInfo::current(), data);
    for step in next_steps(data) {
        envelope = envelope.with_next(step);
    }
    envelope
}

/// The commands worth running next: each line's own, worst first, once
/// each; on a healthy cluster, the app list.
pub fn next_steps(data: &StatusData) -> Vec<NextStep> {
    let mut lines: Vec<&Line> = data
        .lines
        .iter()
        .filter(|line| line.mark > Mark::Quiet)
        .collect();
    lines.sort_by_key(|line| std::cmp::Reverse(line.mark));
    let mut steps: Vec<NextStep> = Vec::new();
    for line in lines {
        let Some(run) = &line.next else { continue };
        if steps.iter().any(|step| &step.run == run) {
            continue;
        }
        steps.push(NextStep {
            run: run.clone(),
            why: format!("{}: {}", line.section, line.text),
            read_only: true,
        });
    }
    if steps.is_empty() {
        steps.push(NextStep {
            run: "relish apps".to_string(),
            why: "list every app with its state".to_string(),
            read_only: true,
        });
    }
    steps
}

/// The verdict and the lines for `summary` and `wtf`.
pub fn status_data(summary: ClusterSummary, wtf: WtfCheck) -> StatusData {
    let lines = assess(&summary, &wtf);
    StatusData {
        verdict: verdict(&lines),
        problems: problems(&lines),
        lines,
        wtf,
        summary,
    }
}

/// The wall-clock time `unix_ms` names, as `HH:MM:SS UTC`.
fn clock(unix_ms: u64) -> String {
    let seconds = i64::try_from(unix_ms / 1000).unwrap_or(0);
    match time::OffsetDateTime::from_unix_timestamp(seconds) {
        Ok(at) => format!("{:02}:{:02}:{:02} UTC", at.hour(), at.minute(), at.second()),
        Err(_) => "an unknown time".to_string(),
    }
}

const RED: &str = "\x1b[31m";
const YELLOW: &str = "\x1b[33m";
const BOLD: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

/// The text of `mark`'s colour, or nothing for a quiet line.
fn colour_of(mark: Mark) -> &'static str {
    match mark {
        Mark::Quiet => "",
        Mark::Note | Mark::Problem => YELLOW,
        Mark::Down => RED,
    }
}

/// The widest a line's text gets before the command column; longer text
/// pushes its own command further right rather than every line's.
const TEXT_COLUMN_MAX: usize = 60;

/// The one-screen view. `colour` adds ANSI colour to the verdict and to
/// each line that needs attention.
pub fn render(data: &StatusData, colour: bool) -> String {
    let mut out = String::new();
    let problems = match data.problems {
        0 => "no problems".to_string(),
        1 => "1 problem".to_string(),
        n => format!("{n} problems"),
    };
    let verdict_colour = match data.verdict {
        Verdict::Healthy => "",
        Verdict::Degraded => YELLOW,
        Verdict::Down => RED,
    };
    let verdict = if colour {
        format!("{BOLD}{verdict_colour}{}{RESET}", data.verdict.word())
    } else {
        data.verdict.word().to_string()
    };
    let _ = writeln!(
        out,
        "cluster: {verdict}, {problems} ({} answered at {})",
        data.summary.answered_by,
        clock(data.summary.generated_at_unix_ms)
    );
    out.push('\n');
    let column = data
        .lines
        .iter()
        .filter(|line| line.next.is_some())
        .map(|line| line.text.chars().count())
        .filter(|width| *width <= TEXT_COLUMN_MAX)
        .max()
        .unwrap_or(0);
    for line in &data.lines {
        let text = match &line.next {
            Some(next) => format!("{:<column$}  {next}", line.text),
            None => line.text.clone(),
        };
        let painted = match colour_of(line.mark) {
            paint if colour && !paint.is_empty() => format!("{paint}{text}{RESET}"),
            _ => text,
        };
        let _ = writeln!(out, "{:<10} {painted}", line.section);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bun::summary::*;

    /// 8 November 2026, 14:02:11 UTC.
    const NOW_MS: u64 = 1_794_146_531_000;

    fn healthy() -> ClusterSummary {
        ClusterSummary {
            generated_at_unix_ms: NOW_MS,
            answered_by: "node-1".to_string(),
            version: "0.2.8".to_string(),
            scoped: false,
            nodes: Section::from_nodes(
                NodesSection {
                    alive: 3,
                    ..NodesSection::default()
                },
                Coverage {
                    answered: 3,
                    asked: 3,
                },
                &[],
            ),
            council: Section::complete(CouncilSection {
                leader: Some("node-1".to_string()),
                term: 41,
                voters: 3,
                voters_answering: 3,
                quorum_ok: true,
                lagging: 0,
                other_leaders: Vec::new(),
            }),
            gossip: Section::complete(GossipSection {
                members: 3,
                updates_received: 18_442,
                last_change_unix_ms: Some(NOW_MS - 240_000),
            }),
            versions: Section::complete(VersionsSection {
                versions: vec![VersionCount {
                    version: "0.2.8".to_string(),
                    nodes: 3,
                }],
                upgrade_in_progress: false,
            }),
            capacity: Section::complete(CapacitySection {
                nodes_reporting: 3,
                cpu_allocated_millicores: 14_700,
                cpu_total_millicores: 24_000,
                memory_allocated_mb: 54_000,
                memory_total_mb: 100_000,
                gpu_allocated: 0,
                gpu_total: 0,
            }),
            usage: Section::complete(UsageSection {
                cpu_percent: Some(34.0),
                memory_percent: Some(47.2),
                disk_percent: Some(38.0),
                busiest: vec![NodePercent {
                    node: "node-2".to_string(),
                    percent: 81.0,
                }],
                newest_sample_unix_ms: Some(NOW_MS - 6_000),
            }),
            apps: Section::complete(AppsSection {
                total: 4,
                running: 4,
                ..AppsSection::default()
            }),
            instances: Section::complete(InstancesSection {
                running: 9,
                desired: 9,
                not_running: 0,
            }),
            jobs: Section::complete(JobsSection {
                succeeded_last_hour: 12,
                ..JobsSection::default()
            }),
            logs: Section::complete(LogsSection {
                newest_unix_ms: Some(NOW_MS - 2_000),
                slowest: Some(NodeTime {
                    node: "node-3".to_string(),
                    unix_ms: NOW_MS - 9_000,
                }),
            }),
            faults: Section::complete(FaultsSection::default()),
            alerts: Section::complete(AlertsSection::default()),
            certs: Section::complete(CertsSection {
                count: 3,
                soonest: Some(CertBrief {
                    subject: "node-2".to_string(),
                    not_after_unix_seconds: NOW_MS / 1000 + 61 * 86_400,
                }),
            }),
            gitops: Section::complete(GitOpsSection {
                configured: false,
                commit: None,
                synced_at_unix_seconds: None,
                error: None,
            }),
        }
    }

    fn checked(critical: usize, warning: usize) -> WtfCheck {
        WtfCheck::Checked {
            critical,
            warning,
            unknown: 0,
            first_critical: (critical > 0).then(|| "app web is down".to_string()),
        }
    }

    #[test]
    fn a_healthy_cluster_says_so_on_one_screen() {
        let data = status_data(healthy(), checked(0, 0));
        assert_eq!(data.verdict, Verdict::Healthy);
        assert_eq!(data.problems, 0);
        insta::assert_snapshot!(render(&data, false));
    }

    #[test]
    fn a_degraded_cluster_counts_its_problems_and_names_the_next_command() {
        let mut summary = healthy();
        let nodes = summary.nodes.value.as_mut().unwrap();
        nodes.alive = 2;
        nodes.suspect = 1;
        summary.apps = Section::complete(AppsSection {
            total: 4,
            running: 2,
            degraded: 1,
            down: 1,
            unschedulable: 1,
            ..AppsSection::default()
        });
        summary.faults = Section::complete(FaultsSection {
            active: 1,
            soonest: vec![FaultBrief {
                kind: "latency".to_string(),
                target: "api".to_string(),
                namespace: Some("default".to_string()),
                remaining_seconds: 360,
            }],
        });
        summary.alerts = Section::complete(AlertsSection {
            firing: 1,
            first: vec!["disk_high on node-3".to_string()],
        });
        let data = status_data(summary, checked(1, 1));
        assert_eq!(data.verdict, Verdict::Degraded);
        assert_eq!(data.problems, 4, "nodes, apps, alerts and wtf");
        insta::assert_snapshot!(render(&data, false));
    }

    #[test]
    fn an_upgrade_in_progress_is_a_note_not_a_problem() {
        let mut summary = healthy();
        summary.versions = Section::complete(VersionsSection {
            versions: vec![
                VersionCount {
                    version: "0.2.8".to_string(),
                    nodes: 2,
                },
                VersionCount {
                    version: "0.2.7".to_string(),
                    nodes: 1,
                },
            ],
            upgrade_in_progress: true,
        });
        summary.nodes.value.as_mut().unwrap().cordoned = 1;
        let data = status_data(summary, checked(0, 0));
        assert_eq!(data.verdict, Verdict::Healthy);
        insta::assert_snapshot!(render(&data, false));
    }

    #[test]
    fn a_node_that_did_not_answer_makes_its_sections_partial() {
        let mut summary = healthy();
        let partial = Coverage {
            answered: 2,
            asked: 3,
        };
        let reasons = ["node-3: timed out".to_string()];
        summary.nodes = Section::from_nodes(
            NodesSection {
                alive: 2,
                suspect: 1,
                ..NodesSection::default()
            },
            partial,
            &reasons,
        );
        summary.council = Section::from_nodes(
            CouncilSection {
                leader: Some("node-1".to_string()),
                term: 41,
                voters: 3,
                voters_answering: 2,
                quorum_ok: true,
                lagging: 0,
                other_leaders: Vec::new(),
            },
            partial,
            &reasons,
        );
        summary.apps = Section::from_nodes(
            AppsSection {
                total: 4,
                running: 3,
                unknown: 1,
                ..AppsSection::default()
            },
            partial,
            &reasons,
        );
        let data = status_data(summary, WtfCheck::Late { budget_seconds: 2 });
        assert_eq!(data.verdict, Verdict::Degraded);
        insta::assert_snapshot!(render(&data, false));
    }

    #[test]
    fn a_council_without_quorum_is_down() {
        let mut summary = healthy();
        summary.council = Section::complete(CouncilSection {
            leader: None,
            term: 42,
            voters: 3,
            voters_answering: 1,
            quorum_ok: false,
            lagging: 0,
            other_leaders: Vec::new(),
        });
        let data = status_data(summary, WtfCheck::Skipped);
        assert_eq!(data.verdict, Verdict::Down);
        assert_eq!(data.verdict.check_exit_code(), 2);
        let council = data.lines.iter().find(|l| l.section == "council").unwrap();
        assert_eq!(
            council.text,
            "NO LEADER, 1/3 voters, QUORUM LOST, 0 lagging"
        );
    }

    /// #424: a split brain must not hide behind a healthy-looking screen.
    #[test]
    fn a_split_council_is_a_problem_that_names_both_leaders() {
        let mut summary = healthy();
        summary.council.value.as_mut().unwrap().other_leaders = vec!["node-3".to_string()];
        let data = status_data(summary, WtfCheck::Skipped);
        assert_eq!(data.verdict, Verdict::Degraded);
        let council = data.lines.iter().find(|l| l.section == "council").unwrap();
        assert!(
            council
                .text
                .starts_with("SPLIT: leader node-1 AND node-3 (term 41)"),
            "{}",
            council.text
        );
        assert_eq!(council.next.as_deref(), Some("relish council"));
    }

    #[test]
    fn check_exit_codes_follow_the_verdict() {
        assert_eq!(Verdict::Healthy.check_exit_code(), 0);
        assert_eq!(Verdict::Degraded.check_exit_code(), 1);
        assert_eq!(Verdict::Down.check_exit_code(), 2);
    }

    #[test]
    fn a_scoped_token_sees_less_without_a_problem() {
        let mut summary = healthy();
        summary.scoped = true;
        summary.capacity = Section::not_visible("capacity");
        summary.usage = Section::not_visible("usage");
        let data = status_data(summary, WtfCheck::Skipped);
        assert_eq!(data.verdict, Verdict::Healthy);
        let usage = data.lines.iter().find(|l| l.section == "usage").unwrap();
        assert_eq!(usage.text, "not visible to a scoped token");
    }

    #[test]
    fn a_section_that_could_not_be_read_is_a_problem_that_says_why() {
        let mut summary = healthy();
        summary.jobs = Section::missing("jobs comes from the leader: timed out");
        let data = status_data(summary, WtfCheck::Skipped);
        assert_eq!(data.verdict, Verdict::Degraded);
        let jobs = data.lines.iter().find(|l| l.section == "jobs").unwrap();
        assert_eq!(jobs.text, "unknown: jobs comes from the leader: timed out");
    }

    #[test]
    fn a_standalone_node_has_no_council_and_no_capacity_problem() {
        let mut summary = healthy();
        summary.council = Section::complete(CouncilSection {
            leader: None,
            term: 0,
            voters: 0,
            voters_answering: 0,
            quorum_ok: true,
            lagging: 0,
            other_leaders: Vec::new(),
        });
        summary.capacity = Section::missing("a standalone node keeps no state reports");
        // A standalone node runs no gossip, so it has no members at all.
        summary.nodes = Section::complete(NodesSection::default());
        summary.gossip = Section::missing("this node runs no gossip");
        let data = status_data(summary, WtfCheck::Skipped);
        assert_eq!(data.verdict, Verdict::Healthy, "{:?}", data.lines);
        let text = |section: &str| {
            data.lines
                .iter()
                .find(|l| l.section == section)
                .map(|l| l.text.clone())
                .unwrap()
        };
        assert_eq!(text("nodes"), "1: a standalone node");
        assert_eq!(text("gossip"), "none: a standalone node");
    }

    #[test]
    fn one_nodes_logs_name_no_slowest_node() {
        let mut summary = healthy();
        summary.logs = Section::complete(LogsSection {
            newest_unix_ms: Some(NOW_MS - 2_000),
            slowest: Some(NodeTime {
                node: "node-1".to_string(),
                unix_ms: NOW_MS - 2_000,
            }),
        });
        let data = status_data(summary, WtfCheck::Skipped);
        let logs = data.lines.iter().find(|l| l.section == "logs").unwrap();
        assert_eq!(logs.text, "newest line 2 s old");
    }

    #[test]
    fn gitops_gets_a_line_only_where_it_is_configured() {
        let data = status_data(healthy(), WtfCheck::Skipped);
        assert!(!data.lines.iter().any(|l| l.section == "gitops"));
        let mut summary = healthy();
        summary.gitops = Section::complete(GitOpsSection {
            configured: true,
            commit: Some("a1b2c3d4e5f6".to_string()),
            synced_at_unix_seconds: Some(NOW_MS / 1000 - 180),
            error: None,
        });
        let data = status_data(summary, WtfCheck::Skipped);
        let gitops = data.lines.iter().find(|l| l.section == "gitops").unwrap();
        assert_eq!(gitops.text, "synced at a1b2c3d, 3 m ago");
    }

    #[test]
    fn the_screen_is_the_same_height_however_many_apps_there_are() {
        let small = render(&status_data(healthy(), WtfCheck::Skipped), false);
        let mut big = healthy();
        big.apps = Section::complete(AppsSection {
            total: 1000,
            running: 1000,
            ..AppsSection::default()
        });
        let big = render(&status_data(big, WtfCheck::Skipped), false);
        assert_eq!(small.lines().count(), big.lines().count());
    }

    #[test]
    fn colour_marks_only_what_needs_attention() {
        let plain = render(&status_data(healthy(), checked(0, 0)), true);
        assert!(!plain.contains(YELLOW) && !plain.contains(RED));
        let mut summary = healthy();
        summary.alerts = Section::complete(AlertsSection {
            firing: 1,
            first: vec!["disk_high on node-3".to_string()],
        });
        let coloured = render(&status_data(summary, checked(0, 0)), true);
        assert!(
            coloured.contains(&format!("{YELLOW}1 firing")),
            "{coloured}"
        );
        assert!(!render(&status_data(healthy(), checked(0, 0)), false).contains('\x1b'));
    }

    #[test]
    fn json_carries_the_verdict_and_next_steps_in_the_agent_envelope() {
        let mut summary = healthy();
        summary.apps = Section::complete(AppsSection {
            total: 4,
            running: 3,
            down: 1,
            ..AppsSection::default()
        });
        let data = status_data(summary, WtfCheck::Skipped);
        let value = serde_json::to_value(envelope(&data)).unwrap();
        assert_eq!(value["schema"], "relish.agent/v1");
        assert_eq!(value["command"], "status");
        assert_eq!(value["ok"], true);
        assert_eq!(value["data"]["verdict"], "degraded");
        assert_eq!(value["data"]["problems"], 1);
        assert_eq!(value["data"]["summary"]["answered_by"], "node-1");
        assert_eq!(value["next"][0]["run"], "relish apps --unhealthy");
        assert_eq!(value["next"][0]["read_only"], true);
    }

    #[test]
    fn agent_schema_offers_the_status_data() {
        // The summary's fourteen sections share one definition, so the
        // whole schema fits `relish agent schema`'s default budget (the
        // agent module's tests hold every command to it).
        let schema = crate::relish::agent::data_schema(CommandName::Status).unwrap();
        assert!(schema["properties"]["verdict"].is_object(), "{schema}");
        assert!(CommandName::ANSWERING.contains(&CommandName::Status));
    }

    #[test]
    fn a_healthy_cluster_points_at_the_app_list() {
        let data = status_data(healthy(), checked(0, 0));
        let steps = next_steps(&data);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].run, "relish apps");
    }

    #[test]
    fn next_steps_put_the_worst_first_and_name_each_command_once() {
        let mut summary = healthy();
        summary.alerts = Section::complete(AlertsSection {
            firing: 1,
            first: vec!["x".to_string()],
        });
        summary.council = Section::complete(CouncilSection {
            leader: None,
            term: 1,
            voters: 3,
            voters_answering: 1,
            quorum_ok: false,
            lagging: 0,
            other_leaders: Vec::new(),
        });
        let data = status_data(summary, checked(1, 0));
        let runs: Vec<String> = next_steps(&data).into_iter().map(|s| s.run).collect();
        assert_eq!(runs, ["relish council", "relish wtf"]);
    }

    #[test]
    fn numbers_and_spans_read_naturally() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(18_442), "18,442");
        assert_eq!(grouped(1_000_000), "1,000,000");
        assert_eq!(span(6_000), "6 s");
        assert_eq!(span(240_000), "4 m");
        assert_eq!(span(3 * 3_600_000), "3 h");
        assert_eq!(span(2 * 86_400_000), "2 d");
    }
}
