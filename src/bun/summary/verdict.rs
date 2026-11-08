//! The verdict on a cluster summary: one line per section, each marked
//! quiet, a note, a problem or down, and the worst mark decides whether
//! the cluster is healthy.
//!
//! `relish status`, the TUI's dashboard and Brioche's all print these same
//! lines, so a section can't read as fine in one and partial in another.
//! It's pure: a [`ClusterSummary`] and the outcome of `wtf`'s checks in,
//! lines out.

use std::fmt::Write as _;

use schemars::JsonSchema;
use serde::Serialize;

use super::{ClusterSummary, Section, Source};
/// Metrics older than this mean the pipeline that collects them stalled.
const STALE_METRICS_MS: u64 = 120_000;

/// A certificate expiring sooner than this many days is a problem.
const CERT_WARNING_DAYS: u64 = 30;

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

/// One line per section of `summary`, then `wtf`'s.
pub fn assess(summary: &ClusterSummary, wtf: &WtfCheck) -> Vec<Line> {
    let mut lines = section_lines(summary);
    lines.push(wtf_line(wtf));
    lines
}

/// One line per section of `summary`, for a reader that doesn't run
/// `wtf`'s checks (the TUI, Brioche): no `wtf` line at all, rather than one
/// that says it was skipped.
pub fn section_lines(summary: &ClusterSummary) -> Vec<Line> {
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
            Some("relish wtf"),
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
            line.next = Some("relish nodes --unhealthy".to_string());
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
    let next = (mark > Mark::Quiet).then_some("relish nodes --unhealthy");
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
    let next = (mark > Mark::Quiet).then_some("relish council");
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
        flagged("versions", text, Mark::Note, Some("relish upgrade status"))
    } else if mixed {
        text.push_str(": mixed, and no upgrade runs");
        flagged(
            "versions",
            text,
            Mark::Problem,
            Some("relish upgrade status"),
        )
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
                return flagged("usage", text, Mark::Problem, Some("relish wtf"));
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
    let mut text = format!("{}: {} running", grouped(apps.total.into()), apps.running);
    for (count, what) in [
        (apps.degraded, "degraded"),
        (apps.down, "down"),
        (apps.unknown, "unknown"),
        (apps.stopped, "stopped"),
        (apps.unschedulable, "unschedulable"),
    ] {
        if count > 0 {
            let _ = write!(text, ", {count} {what}");
        }
    }
    let troubled = apps.degraded + apps.down + apps.unknown + apps.unschedulable;
    if troubled > 0 {
        flagged("apps", text, Mark::Problem, Some("relish apps --unhealthy"))
    } else {
        quiet("apps", text)
    }
}

fn jobs_line(jobs: &crate::bun::summary::JobsSection) -> Line {
    let mut parts = Vec::new();
    if jobs.running > 0 {
        parts.push(format!(
            "{} running ({} tasks running, {} queued)",
            jobs.running,
            grouped(jobs.tasks_running),
            grouped(jobs.tasks_queued)
        ));
    }
    if jobs.scheduled > 0 {
        parts.push(format!("{} scheduled", jobs.scheduled));
    }
    if jobs.succeeded_last_hour > 0 {
        parts.push(format!(
            "{} succeeded in the last hour",
            jobs.succeeded_last_hour
        ));
    }
    if jobs.failed_last_hour > 0 {
        parts.push(format!("{} failed in the last hour", jobs.failed_last_hour));
    }
    if parts.is_empty() {
        return quiet("jobs", "none running or finished in the last hour");
    }
    let text = parts.join(", ");
    if jobs.failed_last_hour > 0 {
        flagged("jobs", text, Mark::Problem, Some("relish jobs"))
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
    flagged("faults", text, Mark::Note, Some("relish fault list"))
}

fn alerts_line(alerts: &crate::bun::summary::AlertsSection) -> Line {
    let Some(first) = alerts.first.first() else {
        return quiet("alerts", "none firing");
    };
    let mut text = format!("{} firing: {first}", alerts.firing);
    if alerts.firing > 1 {
        let _ = write!(text, ", and {} more", alerts.firing - 1);
    }
    flagged("alerts", text, Mark::Problem, Some("relish wtf"))
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
            Some("relish wtf"),
        );
    }
    let days = (soonest.not_after_unix_seconds - now_seconds) / 86_400;
    let text = format!("soonest expiry {days} days ({})", soonest.subject);
    if days < CERT_WARNING_DAYS {
        flagged("certs", text, Mark::Problem, Some("relish wtf"))
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
            Some("relish gitops status"),
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
            Some("relish wtf"),
        ),
        WtfCheck::Failed { reason } => flagged(
            "wtf",
            format!("couldn't run: {reason}"),
            Mark::Note,
            Some("relish wtf"),
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
            let next = (mark > Mark::Quiet).then_some("relish wtf");
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

/// The worst mark on `lines` as a verdict: `DOWN` when the council can't
/// change anything, `DEGRADED` for any other problem, else `HEALTHY`.
pub fn verdict(lines: &[Line]) -> Verdict {
    match lines
        .iter()
        .map(|line| line.mark)
        .max()
        .unwrap_or(Mark::Quiet)
    {
        Mark::Down => Verdict::Down,
        Mark::Problem => Verdict::Degraded,
        Mark::Quiet | Mark::Note => Verdict::Healthy,
    }
}

/// How many of `lines` are problems.
pub fn problems(lines: &[Line]) -> usize {
    lines
        .iter()
        .filter(|line| line.mark >= Mark::Problem)
        .count()
}
