//! `relish status`: is the cluster all right?
//!
//! One screen, the same size on three nodes or three hundred. The first
//! line is the verdict (`HEALTHY`, `DEGRADED` or `DOWN`) with the number of
//! problems behind it; then one line per section of the cluster summary
//! (`GET /v1/cluster/summary`), each ending in the command that lists the
//! detail when something there needs looking at. The lists themselves live
//! in `relish apps` and `relish nodes`.
//!
//! The rendering is pure: [`assess`] turns a summary and the outcome of
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
use crate::bun::summary::{ClusterSummary, Section, Source};

/// How long `relish status` waits for `wtf`'s checks before it shows "not
/// checked in time" (decision D1 in the plan).
pub const WTF_BUDGET: Duration = Duration::from_secs(2);

/// Metrics older than this mean the pipeline that collects them stalled.
const STALE_METRICS_MS: u64 = 120_000;

/// A certificate expiring sooner than this many days is a problem.
const CERT_WARNING_DAYS: u64 = 30;

/// The commands a line can name as its next step, as `relish` parses them;
/// `every_status_hint_is_a_real_command` in `src/bin/relish.rs` holds each
/// one to the CLI.
const NODES_HINT: &str = "relish nodes --unhealthy";
const COUNCIL_HINT: &str = "relish council status";
const UPGRADE_HINT: &str = "relish upgrade status";
const APPS_HINT: &str = "relish apps --unhealthy";
const APP_LIST_HINT: &str = "relish apps";
const JOBS_HINT: &str = "relish jobs";
const FAULTS_HINT: &str = "relish fault list";
const GITOPS_HINT: &str = "relish gitops status";
const WTF_HINT: &str = "relish wtf";

/// Every command `relish status` can suggest, on a line or as a `next`
/// step.
pub const HINTS: &[&str] = &[
    NODES_HINT,
    COUNCIL_HINT,
    UPGRADE_HINT,
    APPS_HINT,
    APP_LIST_HINT,
    JOBS_HINT,
    FAULTS_HINT,
    GITOPS_HINT,
    WTF_HINT,
];

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

/// The cluster's verdict, worst last.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Nothing needs looking at.
    Healthy,
    /// Something needs looking at, and the cluster still serves.
    Degraded,
    /// The council lost its quorum or its leader: nothing can change.
    Down,
}

impl Verdict {
    /// The word the verdict line prints.
    pub fn word(self) -> &'static str {
        match self {
            Verdict::Healthy => "HEALTHY",
            Verdict::Degraded => "DEGRADED",
            Verdict::Down => "DOWN",
        }
    }

    /// `relish status --check`'s exit code: 0, 1 or 2.
    pub fn check_exit_code(self) -> u8 {
        match self {
            Verdict::Healthy => 0,
            Verdict::Degraded => 1,
            Verdict::Down => 2,
        }
    }
}

/// How `wtf`'s checks went, when `relish status` ran them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "outcome")]
pub enum WtfCheck {
    /// `--quick` skipped them.
    Skipped,
    /// They didn't finish inside the budget.
    Late {
        /// The budget, in seconds.
        budget_seconds: u64,
    },
    /// They couldn't run.
    Failed {
        /// Why not.
        reason: String,
    },
    /// They ran.
    Checked {
        /// Findings that need action now.
        critical: usize,
        /// Findings that need attention.
        warning: usize,
        /// Checks that couldn't reach a verdict.
        unknown: usize,
        /// The first critical finding's title, if any.
        first_critical: Option<String>,
    },
}

/// How much one line of the screen needs attention.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Mark {
    /// All is well.
    Quiet,
    /// Worth knowing, not a problem: an upgrade running, a fault injected
    /// on purpose.
    Note,
    /// A problem: the cluster is degraded.
    Problem,
    /// The cluster is down.
    Down,
}

/// One line of the screen: a section of the summary, or `wtf`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Line {
    /// The section's name, as the first column prints it.
    pub section: &'static str,
    /// What the line says.
    pub text: String,
    /// How much it needs attention.
    pub mark: Mark,
    /// The command that shows the detail, on a line that needs attention.
    pub next: Option<String>,
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
        Ok(Ok(inputs)) => {
            let report = super::wtf::diagnose(&inputs);
            WtfCheck::Checked {
                critical: report.summary.critical_count,
                warning: report.summary.warning_count,
                unknown: report.summary.unknown_count,
                first_critical: report.critical.first().map(|finding| finding.title.clone()),
            }
        }
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
            run: APP_LIST_HINT.to_string(),
            why: "list every app with its state".to_string(),
            read_only: true,
        });
    }
    steps
}

/// The verdict and the lines for `summary` and `wtf`.
pub fn status_data(summary: ClusterSummary, wtf: WtfCheck) -> StatusData {
    let lines = assess(&summary, &wtf);
    let worst = lines
        .iter()
        .map(|line| line.mark)
        .max()
        .unwrap_or(Mark::Quiet);
    let verdict = match worst {
        Mark::Down => Verdict::Down,
        Mark::Problem => Verdict::Degraded,
        Mark::Quiet | Mark::Note => Verdict::Healthy,
    };
    StatusData {
        verdict,
        problems: lines
            .iter()
            .filter(|line| line.mark >= Mark::Problem)
            .count(),
        lines,
        wtf,
        summary,
    }
}

/// One line per section of `summary`, then `wtf`'s.
pub fn assess(summary: &ClusterSummary, wtf: &WtfCheck) -> Vec<Line> {
    let now = summary.generated_at_unix_ms;
    let standalone = summary
        .council
        .value
        .as_ref()
        .is_some_and(|council| council.voters == 0);
    // A standalone node runs no gossip: it has no members, and that's no
    // problem.
    let mut lines = if standalone {
        vec![
            quiet("nodes", "1: a standalone node"),
            quiet("council", "none: a standalone node"),
            quiet("gossip", "none: a standalone node"),
        ]
    } else {
        vec![
            section_line("nodes", &summary.nodes, summary.scoped, nodes_line),
            section_line("council", &summary.council, summary.scoped, council_line),
            section_line("gossip", &summary.gossip, summary.scoped, |gossip| {
                gossip_line(gossip, now)
            }),
        ]
    };
    lines.push(section_line(
        "versions",
        &summary.versions,
        summary.scoped,
        versions_line,
    ));
    if standalone && summary.capacity.source == Source::Missing {
        lines.push(quiet("capacity", "not tracked on a standalone node"));
    } else {
        lines.push(section_line(
            "capacity",
            &summary.capacity,
            summary.scoped,
            capacity_line,
        ));
    }
    lines.extend([
        section_line("usage", &summary.usage, summary.scoped, |usage| {
            usage_line(usage, now)
        }),
        section_line("apps", &summary.apps, summary.scoped, apps_line),
        section_line("instances", &summary.instances, summary.scoped, |i| {
            quiet(
                "instances",
                format!(
                    "{} of {} running",
                    grouped(i.running.into()),
                    grouped(i.desired.into())
                ),
            )
        }),
        section_line("jobs", &summary.jobs, summary.scoped, jobs_line),
        section_line("logs", &summary.logs, summary.scoped, |logs| {
            logs_line(logs, now)
        }),
        section_line("faults", &summary.faults, summary.scoped, faults_line),
        section_line("alerts", &summary.alerts, summary.scoped, alerts_line),
        section_line("certs", &summary.certs, summary.scoped, |certs| {
            certs_line(certs, now / 1000)
        }),
    ]);
    // GitOps gets a line only where it's configured.
    let gitops_configured = summary
        .gitops
        .value
        .as_ref()
        .is_none_or(|gitops| gitops.configured);
    if gitops_configured {
        lines.push(section_line(
            "gitops",
            &summary.gitops,
            summary.scoped,
            |gitops| gitops_line(gitops, now / 1000),
        ));
    }
    // TODO(#1682): an `agents` line (leases held, proposals waiting for
    // approval) once the lease and proposal client APIs exist.
    lines.push(wtf_line(wtf));
    lines
}

fn quiet(section: &'static str, text: impl Into<String>) -> Line {
    Line {
        section,
        text: text.into(),
        mark: Mark::Quiet,
        next: None,
    }
}

fn flagged(section: &'static str, text: String, mark: Mark, next: Option<&str>) -> Line {
    Line {
        section,
        text,
        mark,
        next: next.map(str::to_string),
    }
}

/// A section's line: `describe` for what it saw, with a partial section
/// saying how much it saw and a missing one saying why. A scoped token
/// sees less by design, so what it can't see is no problem.
fn section_line<T>(
    section: &'static str,
    value: &Section<T>,
    scoped: bool,
    describe: impl Fn(&T) -> Line,
) -> Line {
    let Some(seen) = &value.value else {
        if scoped {
            return quiet(section, "not visible to a scoped token");
        }
        let reason = value.reason.as_deref().unwrap_or("no answer");
        return flagged(
            section,
            format!("unknown: {reason}"),
            Mark::Problem,
            Some(WTF_HINT),
        );
    };
    let mut line = describe(seen);
    line.section = section;
    if value.source == Source::Partial {
        let coverage = value
            .coverage
            .map(|c| format!("{} of {} nodes", c.answered, c.asked))
            .or_else(|| value.reason.clone())
            .unwrap_or_else(|| "some sources didn't answer".to_string());
        line.text = format!("{}; partial ({coverage})", line.text);
        if line.mark < Mark::Problem {
            line.mark = Mark::Problem;
            line.next = Some(NODES_HINT.to_string());
        }
    }
    line
}

fn nodes_line(nodes: &crate::bun::summary::NodesSection) -> Line {
    let mut text = format!(
        "{} alive, {} suspect, {} dead",
        nodes.alive, nodes.suspect, nodes.dead
    );
    for (count, what) in [
        (nodes.left, "left"),
        (nodes.cordoned, "cordoned"),
        (nodes.disk_pressure, "under disk pressure"),
    ] {
        if count > 0 {
            let _ = write!(text, ", {count} {what}");
        }
    }
    let mark = if nodes.alive == 0 {
        Mark::Down
    } else if nodes.suspect > 0 || nodes.dead > 0 || nodes.disk_pressure > 0 {
        Mark::Problem
    } else {
        Mark::Quiet
    };
    let next = (mark > Mark::Quiet).then_some(NODES_HINT);
    flagged("nodes", text, mark, next)
}

fn council_line(council: &crate::bun::summary::CouncilSection) -> Line {
    let leader = match &council.leader {
        Some(leader) if !council.other_leaders.is_empty() => format!(
            "SPLIT: leader {leader} AND {} (term {})",
            council.other_leaders.join(" AND "),
            council.term
        ),
        Some(leader) => format!("leader {leader} (term {})", council.term),
        None => "NO LEADER".to_string(),
    };
    let quorum = if council.quorum_ok {
        "quorum ok"
    } else {
        "QUORUM LOST"
    };
    let text = format!(
        "{leader}, {}/{} voters, {quorum}, {} lagging",
        council.voters_answering, council.voters, council.lagging
    );
    let mark = if council.leader.is_none() || !council.quorum_ok {
        Mark::Down
    } else if council.voters_answering < council.voters
        || council.lagging > 0
        || !council.other_leaders.is_empty()
    {
        Mark::Problem
    } else {
        Mark::Quiet
    };
    let next = (mark > Mark::Quiet).then_some(COUNCIL_HINT);
    flagged("council", text, mark, next)
}

fn gossip_line(gossip: &crate::bun::summary::GossipSection, now_ms: u64) -> Line {
    let change = match gossip.last_change_unix_ms {
        Some(at) => format!("last change {} ago", span(now_ms.saturating_sub(at))),
        None => "no change since this node started".to_string(),
    };
    quiet(
        "gossip",
        format!(
            "{} members, {change}, {} updates received",
            gossip.members,
            grouped(gossip.updates_received)
        ),
    )
}

fn versions_line(versions: &crate::bun::summary::VersionsSection) -> Line {
    let mut parts = versions.versions.iter();
    let mut text = match parts.next() {
        Some(first) => format!("bun {} on {}", first.version, nodes_word(first.nodes)),
        None => "no node reported its version".to_string(),
    };
    for other in parts {
        let _ = write!(text, ", {} on {}", other.version, other.nodes);
    }
    let mixed = versions.versions.len() > 1;
    if versions.upgrade_in_progress {
        text.push_str(": upgrade in progress");
        flagged("versions", text, Mark::Note, Some(UPGRADE_HINT))
    } else if mixed {
        text.push_str(": mixed, and no upgrade runs");
        flagged("versions", text, Mark::Problem, Some(UPGRADE_HINT))
    } else {
        quiet("versions", text)
    }
}

fn nodes_word(count: u32) -> String {
    if count == 1 {
        "1 node".to_string()
    } else {
        format!("{count} nodes")
    }
}

fn capacity_line(capacity: &crate::bun::summary::CapacitySection) -> Line {
    let mut text = format!(
        "cpu {} allocated ({} of {} cores), memory {}",
        share(
            capacity.cpu_allocated_millicores,
            capacity.cpu_total_millicores
        ),
        cores(capacity.cpu_allocated_millicores),
        cores(capacity.cpu_total_millicores),
        share(capacity.memory_allocated_mb, capacity.memory_total_mb),
    );
    if capacity.gpu_total > 0 {
        let _ = write!(
            text,
            ", gpu {} of {}",
            capacity.gpu_allocated, capacity.gpu_total
        );
    }
    quiet("capacity", text)
}

fn usage_line(usage: &crate::bun::summary::UsageSection, now_ms: u64) -> Line {
    let mut text = format!("cpu {}", percent_cell(usage.cpu_percent));
    if let Some(busiest) = usage.busiest.first() {
        let _ = write!(text, " (busiest {} {:.0}%)", busiest.node, busiest.percent);
    }
    let _ = write!(
        text,
        ", memory {}, disk {}",
        percent_cell(usage.memory_percent),
        percent_cell(usage.disk_percent)
    );
    match usage.newest_sample_unix_ms {
        Some(at) => {
            let age = now_ms.saturating_sub(at);
            let _ = write!(text, "; newest sample {} old", span(age));
            if age > STALE_METRICS_MS {
                return flagged("usage", text, Mark::Problem, Some(WTF_HINT));
            }
            quiet("usage", text)
        }
        None => {
            text.push_str("; no samples yet");
            quiet("usage", text)
        }
    }
}

fn apps_line(apps: &crate::bun::summary::AppsSection) -> Line {
    if apps.total == 0 {
        return quiet("apps", "none deployed");
    }
    // Each app is in exactly one of these, so they add up to the total.
    let mut text = format!(
        "{}: {} running",
        grouped(apps.total.into()),
        grouped(apps.running.into())
    );
    for (count, what) in [
        (apps.degraded, "degraded"),
        (apps.down, "down"),
        (apps.unknown, "unknown"),
        (apps.stopped, "stopped"),
    ] {
        if count > 0 {
            let _ = write!(text, ", {} {what}", grouped(count.into()));
        }
    }
    // An unschedulable app is already degraded or down (or unknown), so
    // it's an aside in parentheses, not another bucket.
    if apps.unschedulable > 0 {
        let _ = write!(text, " ({} unschedulable)", apps.unschedulable);
    }
    let troubled = apps.degraded + apps.down + apps.unknown + apps.unschedulable;
    if troubled > 0 {
        flagged("apps", text, Mark::Problem, Some(APPS_HINT))
    } else {
        quiet("apps", text)
    }
}

fn jobs_line(jobs: &crate::bun::summary::JobsSection) -> Line {
    let mut parts = Vec::new();
    if jobs.running > 0 {
        parts.push(format!(
            "{} running ({} tasks)",
            grouped(jobs.running.into()),
            grouped(jobs.tasks_running)
        ));
    }
    if jobs.tasks_queued > 0 {
        parts.push(format!("{} tasks queued", grouped(jobs.tasks_queued)));
    }
    let mut hour = Vec::new();
    if jobs.failed_last_hour > 0 {
        hour.push(format!("{} failed", grouped(jobs.failed_last_hour.into())));
    }
    if jobs.succeeded_last_hour > 0 {
        hour.push(format!(
            "{} succeeded",
            grouped(jobs.succeeded_last_hour.into())
        ));
    }
    if !hour.is_empty() {
        parts.push(format!("{} in the last hour", hour.join(" and ")));
    }
    if jobs.scheduled > 0 {
        parts.push(format!("{} scheduled", jobs.scheduled));
    }
    if parts.is_empty() {
        return quiet("jobs", "none running or finished in the last hour");
    }
    let text = parts.join(", ");
    if jobs.failed_last_hour > 0 {
        flagged("jobs", text, Mark::Problem, Some(JOBS_HINT))
    } else {
        quiet("jobs", text)
    }
}

fn logs_line(logs: &crate::bun::summary::LogsSection, now_ms: u64) -> Line {
    let Some(newest) = logs.newest_unix_ms else {
        return quiet("logs", "no lines stored yet");
    };
    let mut text = format!("newest line {} old", span(now_ms.saturating_sub(newest)));
    if let Some(slowest) = logs.slowest.as_ref().filter(|s| s.unix_ms != newest) {
        let _ = write!(
            text,
            ", slowest node {} ({})",
            span(now_ms.saturating_sub(slowest.unix_ms)),
            slowest.node
        );
    }
    quiet("logs", text)
}

fn faults_line(faults: &crate::bun::summary::FaultsSection) -> Line {
    let Some(first) = faults.soonest.first() else {
        return quiet("faults", "none active");
    };
    let target = match &first.namespace {
        Some(namespace) => format!("{} ({namespace})", first.target),
        None => first.target.clone(),
    };
    let mut text = format!(
        "{} active: {} on {target}, {} left",
        faults.active,
        first.kind,
        span(first.remaining_seconds * 1000)
    );
    if faults.active > 1 {
        let _ = write!(text, ", and {} more", faults.active - 1);
    }
    flagged("faults", text, Mark::Note, Some(FAULTS_HINT))
}

fn alerts_line(alerts: &crate::bun::summary::AlertsSection) -> Line {
    let Some(first) = alerts.first.first() else {
        return quiet("alerts", "none firing");
    };
    let mut text = format!("{} firing: {first}", alerts.firing);
    if alerts.firing > 1 {
        let _ = write!(text, ", and {} more", alerts.firing - 1);
    }
    flagged("alerts", text, Mark::Problem, Some(WTF_HINT))
}

fn certs_line(certs: &crate::bun::summary::CertsSection, now_seconds: u64) -> Line {
    let Some(soonest) = &certs.soonest else {
        return quiet("certs", "none tracked");
    };
    if soonest.not_after_unix_seconds <= now_seconds {
        return flagged(
            "certs",
            format!("EXPIRED: {}", soonest.subject),
            Mark::Problem,
            Some(WTF_HINT),
        );
    }
    let days = (soonest.not_after_unix_seconds - now_seconds) / 86_400;
    let text = format!("soonest expiry {days} days ({})", soonest.subject);
    if days < CERT_WARNING_DAYS {
        flagged("certs", text, Mark::Problem, Some(WTF_HINT))
    } else {
        quiet("certs", text)
    }
}

fn gitops_line(gitops: &crate::bun::summary::GitOpsSection, now_seconds: u64) -> Line {
    if let Some(error) = &gitops.error {
        return flagged(
            "gitops",
            format!("failing: {error}"),
            Mark::Problem,
            Some(GITOPS_HINT),
        );
    }
    let text = match (&gitops.commit, gitops.synced_at_unix_seconds) {
        (Some(commit), Some(at)) => format!(
            "synced at {}, {} ago",
            short_commit(commit),
            span(now_seconds.saturating_sub(at) * 1000)
        ),
        (Some(commit), None) => format!("synced at {}", short_commit(commit)),
        (None, _) => "configured, not synced yet".to_string(),
    };
    quiet("gitops", text)
}

fn short_commit(commit: &str) -> &str {
    commit.get(..7).unwrap_or(commit)
}

fn wtf_line(wtf: &WtfCheck) -> Line {
    match wtf {
        WtfCheck::Skipped => quiet("wtf", "skipped (--quick)"),
        WtfCheck::Late { budget_seconds } => flagged(
            "wtf",
            format!("not checked in time ({budget_seconds} s budget)"),
            Mark::Note,
            Some(WTF_HINT),
        ),
        WtfCheck::Failed { reason } => flagged(
            "wtf",
            format!("couldn't run: {reason}"),
            Mark::Note,
            Some(WTF_HINT),
        ),
        WtfCheck::Checked {
            critical,
            warning,
            unknown,
            first_critical,
        } => {
            let mut text = format!(
                "{critical} critical, {warning} {}",
                if *warning == 1 { "warning" } else { "warnings" }
            );
            if *unknown > 0 {
                let _ = write!(text, ", {unknown} unknown");
            }
            if let Some(title) = first_critical {
                let _ = write!(text, ": {title}");
            }
            let mark = if *critical > 0 {
                Mark::Problem
            } else if *warning > 0 {
                Mark::Note
            } else {
                Mark::Quiet
            };
            let next = (mark > Mark::Quiet).then_some(WTF_HINT);
            flagged("wtf", text, mark, next)
        }
    }
}

/// `used` as a whole percentage of `total`, or `-` with no total.
fn share(used: u64, total: u64) -> String {
    if total == 0 {
        return "-".to_string();
    }
    format!("{}%", (used as f64 / total as f64 * 100.0).round())
}

fn percent_cell(percent: Option<f64>) -> String {
    match percent {
        Some(percent) => format!("{percent:.0}%"),
        None => "-".to_string(),
    }
}

/// Millicores as whole cores, rounded down.
fn cores(millicores: u64) -> u64 {
    millicores / 1000
}

/// A span of milliseconds in its largest whole unit: `6 s`, `4 m`, `3 h`,
/// `2 d`.
pub fn span(ms: u64) -> String {
    let seconds = ms / 1000;
    match seconds {
        s if s >= 86_400 => format!("{} d", s / 86_400),
        s if s >= 3_600 => format!("{} h", s / 3_600),
        s if s >= 60 => format!("{} m", s / 60),
        s => format!("{s} s"),
    }
}

/// `value` with a comma between each group of three digits.
pub fn grouped(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
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

/// The widest a line's text gets beside the command column. Longer text
/// wraps onto the rows below, so every command starts in the same column;
/// with the section names and the longest command, the screen fits 100
/// columns.
const TEXT_COLUMN_MAX: usize = 56;

/// The section-name column, with the space after it.
const SECTION_WIDTH: usize = 11;

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
        .max()
        .unwrap_or(0)
        .min(TEXT_COLUMN_MAX);
    for line in &data.lines {
        let rows = match &line.next {
            Some(next) => {
                let mut rows = wrap(&line.text, column);
                rows[0] = format!("{:<column$}  {next}", rows[0]);
                rows
            }
            None => vec![line.text.clone()],
        };
        for (index, row) in rows.into_iter().enumerate() {
            let section = if index == 0 { line.section } else { "" };
            let row = match colour_of(line.mark) {
                paint if colour && !paint.is_empty() => format!("{paint}{row}{RESET}"),
                _ => row,
            };
            let _ = writeln!(out, "{section:<SECTION_WIDTH$}{row}");
        }
    }
    out
}

/// `text` in rows of at most `width` characters, so a long line's command
/// keeps its column. It breaks between clauses (after `, ` or `; `) where
/// it can, and between words inside a clause too wide for a row; a word
/// wider than `width` gets a row of its own. Never empty.
fn wrap(text: &str, width: usize) -> Vec<String> {
    // Each clause keeps its trailing separator, without the space.
    let mut clauses = Vec::new();
    let mut rest = text;
    while let Some(at) = rest.find(", ").into_iter().chain(rest.find("; ")).min() {
        clauses.push(&rest[..=at]);
        rest = &rest[at + 2..];
    }
    clauses.push(rest);

    let mut rows = vec![String::new()];
    for clause in clauses {
        let pieces: Vec<&str> = if clause.chars().count() > width {
            clause.split(' ').collect()
        } else {
            vec![clause]
        };
        for piece in pieces {
            let Some(row) = rows.last_mut() else {
                break;
            };
            if !row.is_empty() && row.chars().count() + 1 + piece.chars().count() > width {
                rows.push(piece.to_string());
            } else {
                if !row.is_empty() {
                    row.push(' ');
                }
                row.push_str(piece);
            }
        }
    }
    rows
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
        summary.jobs = Section::complete(JobsSection {
            running: 12,
            tasks_running: 40,
            tasks_queued: 340,
            succeeded_last_hour: 9,
            failed_last_hour: 3,
            scheduled: 0,
        });
        let data = status_data(summary, checked(1, 1));
        assert_eq!(data.verdict, Verdict::Degraded);
        assert_eq!(data.problems, 5, "nodes, apps, jobs, alerts and wtf");
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
        assert_eq!(council.next.as_deref(), Some("relish council status"));
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
    fn the_status_data_has_a_schema_though_agent_schema_does_not_offer_it() {
        let schema = crate::relish::agent::data_schema(CommandName::Status).unwrap();
        assert!(schema["properties"]["verdict"].is_object(), "{schema}");
        assert!(!CommandName::ANSWERING.contains(&CommandName::Status));
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
        assert_eq!(runs, ["relish council status", "relish wtf"]);
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

    /// The numbers the apps line shows outside its parentheses add up to
    /// the total: an unschedulable app is already down or degraded, so it's
    /// an aside, not another bucket.
    #[test]
    fn the_apps_line_parts_add_up_to_the_total() {
        let cases = [
            AppsSection {
                total: 4,
                running: 2,
                degraded: 1,
                down: 1,
                unschedulable: 1,
                ..AppsSection::default()
            },
            AppsSection {
                total: 9,
                running: 3,
                degraded: 2,
                down: 1,
                unknown: 1,
                stopped: 2,
                unschedulable: 3,
                ..AppsSection::default()
            },
            AppsSection {
                total: 1000,
                running: 1000,
                ..AppsSection::default()
            },
        ];
        for apps in cases {
            let text = apps_line(&apps).text;
            let (total, parts) = text.split_once(": ").unwrap();
            let outside = parts.split(" (").next().unwrap();
            let sum: u32 = outside
                .split(", ")
                .map(|part| {
                    let count = part.split(' ').next().unwrap().replace(',', "");
                    count.parse::<u32>().unwrap()
                })
                .sum();
            assert_eq!(
                total.replace(',', "").parse::<u32>().unwrap(),
                sum,
                "{text}"
            );
            assert_eq!(sum, apps.total, "{text}");
        }
        let text = apps_line(&AppsSection {
            total: 4,
            running: 2,
            degraded: 1,
            down: 1,
            unschedulable: 1,
            ..AppsSection::default()
        })
        .text;
        assert_eq!(text, "4: 2 running, 1 degraded, 1 down (1 unschedulable)");
    }

    /// The jobs line shows what runs, what waits and what failed, whenever
    /// any of it isn't zero.
    #[test]
    fn the_jobs_line_shows_running_queued_and_failed() {
        let line = jobs_line(&JobsSection {
            running: 12,
            tasks_running: 40,
            tasks_queued: 340,
            succeeded_last_hour: 9,
            failed_last_hour: 3,
            scheduled: 2,
        });
        assert_eq!(
            line.text,
            "12 running (40 tasks), 340 tasks queued, 3 failed and 9 succeeded in the last hour, 2 scheduled"
        );
        assert_eq!(line.mark, Mark::Problem);
        let quiet_queue = jobs_line(&JobsSection {
            running: 1,
            tasks_running: 4,
            ..JobsSection::default()
        });
        assert_eq!(quiet_queue.text, "1 running (4 tasks)");
        let failed_only = jobs_line(&JobsSection {
            failed_last_hour: 1,
            ..JobsSection::default()
        });
        assert_eq!(failed_only.text, "1 failed in the last hour");
    }

    /// Every next command starts in the same column, however long a line's
    /// text, and the screen fits 100 columns.
    #[test]
    fn hints_line_up_in_one_column_within_a_hundred_columns() {
        for data in fixtures() {
            let screen = render(&data, false);
            let columns: Vec<usize> = screen
                .lines()
                .filter_map(|line| {
                    HINTS
                        .iter()
                        .filter_map(|hint| line.find(&format!("  {hint}")))
                        .min()
                        .map(|at| at + 2)
                })
                .collect();
            assert!(
                columns.windows(2).all(|pair| pair[0] == pair[1]),
                "{columns:?}\n{screen}"
            );
            for line in screen.lines() {
                assert!(line.chars().count() <= 100, "{} wide: {line}", line.len());
            }
        }
    }

    #[test]
    fn long_text_wraps_between_clauses_then_words() {
        assert_eq!(
            wrap(
                "leader node-1 (term 41), 2/3 voters, quorum ok, 0 lagging; partial (2 of 3 nodes)",
                56
            ),
            [
                "leader node-1 (term 41), 2/3 voters, quorum ok,",
                "0 lagging; partial (2 of 3 nodes)"
            ]
        );
        assert_eq!(
            wrap("one clause that is far too long for its row", 20),
            ["one clause that is", "far too long for its", "row"]
        );
        assert_eq!(wrap("short", 56), ["short"]);
        assert_eq!(wrap("", 56), [""]);
    }

    /// Every command a line names is one `relish` has; the CLI test in
    /// `src/bin/relish.rs` parses each of [`HINTS`].
    #[test]
    fn every_next_command_is_a_listed_hint() {
        for data in fixtures() {
            for line in &data.lines {
                if let Some(next) = &line.next {
                    assert!(HINTS.contains(&next.as_str()), "{next}");
                }
            }
            for step in next_steps(&data) {
                assert!(HINTS.contains(&step.run.as_str()), "{}", step.run);
            }
        }
    }

    /// One summary per kind of line that names a command, for the layout
    /// and hint tests.
    fn fixtures() -> Vec<StatusData> {
        let mut problems = healthy();
        let partial = Coverage {
            answered: 2,
            asked: 3,
        };
        let reasons = ["node-3: timed out".to_string()];
        problems.nodes = Section::from_nodes(
            NodesSection {
                alive: 2,
                suspect: 1,
                disk_pressure: 1,
                ..NodesSection::default()
            },
            partial,
            &reasons,
        );
        problems.council = Section::from_nodes(
            CouncilSection {
                leader: Some("node-1".to_string()),
                term: 41,
                voters: 3,
                voters_answering: 2,
                quorum_ok: true,
                lagging: 1,
                other_leaders: vec!["node-3".to_string()],
            },
            partial,
            &reasons,
        );
        problems.versions = Section::complete(VersionsSection {
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
        problems.apps = Section::from_nodes(
            AppsSection {
                total: 104,
                running: 99,
                degraded: 3,
                down: 1,
                unknown: 1,
                unschedulable: 1,
                ..AppsSection::default()
            },
            partial,
            &reasons,
        );
        problems.jobs = Section::complete(JobsSection {
            running: 12,
            tasks_running: 40,
            tasks_queued: 340,
            succeeded_last_hour: 9,
            failed_last_hour: 3,
            scheduled: 2,
        });
        problems.faults = Section::complete(FaultsSection {
            active: 2,
            soonest: vec![FaultBrief {
                kind: "latency".to_string(),
                target: "api".to_string(),
                namespace: Some("default".to_string()),
                remaining_seconds: 360,
            }],
        });
        problems.alerts = Section::complete(AlertsSection {
            firing: 2,
            first: vec!["disk_high on node-3".to_string()],
        });
        problems.gitops = Section::complete(GitOpsSection {
            configured: true,
            commit: None,
            synced_at_unix_seconds: None,
            error: Some("fetch failed: connection refused".to_string()),
        });
        let mut stale = healthy();
        stale.usage.value.as_mut().unwrap().newest_sample_unix_ms = Some(NOW_MS - 600_000);
        stale.certs.value.as_mut().unwrap().soonest = Some(CertBrief {
            subject: "node-2".to_string(),
            not_after_unix_seconds: NOW_MS / 1000 + 86_400,
        });
        let mut down = healthy();
        down.council = Section::complete(CouncilSection {
            leader: None,
            term: 42,
            voters: 3,
            voters_answering: 1,
            quorum_ok: false,
            lagging: 0,
            other_leaders: Vec::new(),
        });
        vec![
            status_data(healthy(), checked(0, 0)),
            status_data(problems, checked(1, 1)),
            status_data(stale, WtfCheck::Late { budget_seconds: 2 }),
            status_data(
                down,
                WtfCheck::Failed {
                    reason: "no answer".to_string(),
                },
            ),
        ]
    }
}
