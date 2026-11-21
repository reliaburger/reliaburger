//! `relish wtf` collection, rendering and exit-code orchestration.

use std::fmt::Write as _;
use std::io::{IsTerminal, Write as _};
use std::time::Duration;

use super::client::BunClient;
use super::wtf::checks::{CheckKind, check_for};
use super::wtf::findings::{Unit, finding};
use super::wtf::{WtfFinding, WtfOk, WtfReport, WtfUnknown, collect, diagnose};
use super::{CommandOutcome, OutputFormat, RelishError};

/// Arguments accepted by the diagnostic command body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WtfArgs {
    /// Optional application name to diagnose.
    pub app: Option<String>,
    /// Re-run collection every `interval` until interrupted.
    pub watch: bool,
    /// Pause between `watch` collections (30 seconds from the CLI).
    pub interval: Duration,
    /// Human, JSON or YAML output.
    pub output: OutputFormat,
    /// How much the human report says.
    pub view: WtfView,
}

/// How much the human report says. JSON and YAML always carry everything.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WtfView {
    /// One row per check with its status, and a summary for the ones that
    /// aren't OK.
    #[default]
    Table,
    /// The counts, then only what isn't OK, each with its next step.
    Short,
    /// Every check, with what it tests and where the manual covers it.
    Explain,
}

/// Run `relish wtf` against the configured Bun endpoint.
pub async fn run(args: WtfArgs) -> Result<CommandOutcome, RelishError> {
    run_with_client(args, &BunClient::default_local()).await
}

/// Run `relish wtf` with an explicit client for tests and embedded callers.
pub async fn run_with_client(
    args: WtfArgs,
    client: &BunClient,
) -> Result<CommandOutcome, RelishError> {
    validate_args(&args)?;
    if args.watch {
        return run_watch(&args, client).await;
    }
    let report = diagnose(&collect(client, args.app.as_deref()).await?);
    println!("{}", render_report(&report, args.output, args.view)?);
    Ok(report_outcome(&report))
}

fn validate_args(args: &WtfArgs) -> Result<(), RelishError> {
    if args.watch && args.output != OutputFormat::Human {
        return Err(RelishError::InvalidFlag {
            flag: "watch".to_string(),
            reason: "continuous output supports only --output human".to_string(),
        });
    }
    let flag = match args.view {
        WtfView::Table => return Ok(()),
        WtfView::Short => "short",
        WtfView::Explain => "explain",
    };
    if args.output != OutputFormat::Human {
        return Err(RelishError::InvalidFlag {
            flag: flag.to_string(),
            reason: "it changes only the human report; --output json and yaml always carry \
                     every finding"
                .to_string(),
        });
    }
    Ok(())
}

async fn run_watch(args: &WtfArgs, client: &BunClient) -> Result<CommandOutcome, RelishError> {
    let mut first = true;
    let mut last_outcome = CommandOutcome::Warnings;
    let interrupt = tokio::signal::ctrl_c();
    tokio::pin!(interrupt);
    loop {
        let collected = tokio::select! {
            biased;
            signal = &mut interrupt => {
                signal?;
                return Ok(last_outcome);
            }
            collected = collect(client, args.app.as_deref()) => collected,
        };
        let report = match collected {
            Ok(inputs) => {
                let report = diagnose(&inputs);
                last_outcome = report_outcome(&report);
                report
            }
            Err(error) => {
                // Lost evidence cannot turn previously observed problems clean.
                if last_outcome != CommandOutcome::Problems {
                    last_outcome = CommandOutcome::Warnings;
                }
                collection_failure_report(&error)
            }
        };
        if !first && std::io::stdout().is_terminal() {
            print!("\x1b[2J\x1b[H");
        }
        first = false;
        println!(
            "{}",
            render_report(&report, OutputFormat::Human, args.view)?
        );
        std::io::stdout().flush()?;
        tokio::select! {
            signal = &mut interrupt => {
                signal?;
                return Ok(last_outcome);
            }
            _ = tokio::time::sleep(args.interval) => {}
        }
    }
}

fn collection_failure_report(error: &RelishError) -> WtfReport {
    WtfReport {
        schema_version: super::wtf::WTF_SCHEMA_VERSION,
        cluster_name: "unknown".into(),
        collected_at: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
        node_count: 0,
        critical: Vec::new(),
        warnings: Vec::new(),
        unknown: vec![super::wtf::WtfUnknown {
            source: "collection".into(),
            reason: error.to_string(),
            affected_resource: "cluster".into(),
        }],
        ok: Vec::new(),
        summary: super::wtf::WtfSummary {
            critical_count: 0,
            warning_count: 0,
            unknown_count: 1,
            ok_count: 0,
        },
    }
}

fn report_outcome(report: &WtfReport) -> CommandOutcome {
    if !report.critical.is_empty() {
        CommandOutcome::Problems
    } else if !report.warnings.is_empty() || !report.unknown.is_empty() {
        CommandOutcome::Warnings
    } else {
        CommandOutcome::Clean
    }
}

fn render_report(
    report: &WtfReport,
    output: OutputFormat,
    view: WtfView,
) -> Result<String, RelishError> {
    match output {
        OutputFormat::Human => Ok(match view {
            WtfView::Table => render_table(report),
            WtfView::Short => render_short(report),
            WtfView::Explain => render_explain(report),
        }),
        OutputFormat::Json => {
            serde_json::to_string_pretty(report).map_err(RelishError::SerialiseJson)
        }
        OutputFormat::Yaml => yaml_serde::to_string(report).map_err(RelishError::SerialiseYaml),
    }
}

/// A check's verdict: the worst thing it reported. The derived `Ord`
/// follows declaration order, so the variants run from best to worst.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Status {
    Ok,
    Unknown,
    Warning,
    Critical,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::Unknown => "unknown",
            Status::Warning => "warning",
            Status::Critical => "critical",
        }
    }
}

/// Everything one check reported, gathered from the report's four lists.
struct CheckRow<'a> {
    name: &'a str,
    kind: Option<&'static CheckKind>,
    order: usize,
    critical: Vec<&'a WtfFinding>,
    warnings: Vec<&'a WtfFinding>,
    unknown: Vec<&'a WtfUnknown>,
    ok: Vec<&'a WtfOk>,
}

impl CheckRow<'_> {
    fn status(&self) -> Status {
        if !self.critical.is_empty() {
            Status::Critical
        } else if !self.warnings.is_empty() {
            Status::Warning
        } else if !self.unknown.is_empty() {
            Status::Unknown
        } else {
            Status::Ok
        }
    }

    /// The first problem in one line, and how many more there are.
    fn summary(&self) -> Option<String> {
        let first = self
            .critical
            .iter()
            .chain(&self.warnings)
            .map(|finding| finding.title.clone())
            .chain(self.unknown.iter().map(|unknown| unknown.reason.clone()))
            .next()?;
        let more = self.critical.len() + self.warnings.len() + self.unknown.len() - 1;
        Some(if more == 0 {
            first
        } else {
            format!("{first} (+{more} more)")
        })
    }
}

/// One row per check, worst first, then in the order the checks run. A
/// result no check claims gets a row of its own rather than vanishing.
fn check_rows(report: &WtfReport) -> Vec<CheckRow<'_>> {
    let mut rows: Vec<CheckRow<'_>> = Vec::new();
    fn row<'a, 'r>(rows: &'r mut Vec<CheckRow<'a>>, id: &'a str) -> &'r mut CheckRow<'a> {
        let kind = check_for(id);
        let name = kind.map_or(id, |kind| kind.id);
        let index = match rows.iter().position(|row| row.name == name) {
            Some(index) => index,
            None => {
                let order = kind
                    .and_then(|kind| {
                        super::wtf::checks::CHECKS
                            .iter()
                            .position(|check| check.id == kind.id)
                    })
                    .unwrap_or(usize::MAX);
                rows.push(CheckRow {
                    name,
                    kind,
                    order,
                    critical: Vec::new(),
                    warnings: Vec::new(),
                    unknown: Vec::new(),
                    ok: Vec::new(),
                });
                rows.len() - 1
            }
        };
        &mut rows[index]
    }
    for finding in &report.critical {
        row(&mut rows, &finding.id).critical.push(finding);
    }
    for finding in &report.warnings {
        row(&mut rows, &finding.id).warnings.push(finding);
    }
    for unknown in &report.unknown {
        row(&mut rows, &unknown.source).unknown.push(unknown);
    }
    for ok in &report.ok {
        row(&mut rows, &ok.id).ok.push(ok);
    }
    rows.sort_by(|left, right| {
        right
            .status()
            .cmp(&left.status())
            .then(left.order.cmp(&right.order))
            .then(left.name.cmp(right.name))
    });
    rows
}

fn render_heading(output: &mut String, report: &WtfReport) {
    let _ = writeln!(
        output,
        "Reliaburger diagnosis: {} ({} node{})",
        report.cluster_name,
        report.node_count,
        if report.node_count == 1 { "" } else { "s" }
    );
    let _ = writeln!(output, "collected at unix {}", report.collected_at);
}

fn summary_line(report: &WtfReport) -> String {
    format!(
        "Summary: {} critical, {} warning, {} unknown, {} OK",
        report.summary.critical_count,
        report.summary.warning_count,
        report.summary.unknown_count,
        report.summary.ok_count
    )
}

/// The default view: a table of checks, a one-line summary on each that
/// isn't OK, then the counts.
fn render_table(report: &WtfReport) -> String {
    let mut output = String::new();
    render_heading(&mut output, report);
    let rows = check_rows(report);
    let width = rows
        .iter()
        .map(|row| row.name.len())
        .chain(["CHECK".len()])
        .max()
        .unwrap_or(0);
    let mut lines = vec![format!("{:<width$}  {:<8}  SUMMARY", "CHECK", "STATUS")];
    for row in &rows {
        let summary = match row.status() {
            Status::Ok => String::new(),
            _ => row.summary().unwrap_or_default(),
        };
        lines.push(format!(
            "{:<width$}  {:<8}  {summary}",
            row.name,
            row.status().label()
        ));
    }
    let _ = writeln!(output);
    for line in lines {
        let _ = writeln!(output, "{}", line.trim_end());
    }
    let _ = write!(output, "\n{}", summary_line(report));
    output
}

/// `--short`: the counts, then one line for each thing that isn't OK, with
/// its next step. Nothing about the checks that passed.
fn render_short(report: &WtfReport) -> String {
    let mut output = summary_line(report);
    for (label, findings) in [
        ("CRITICAL", &report.critical),
        ("WARNING", &report.warnings),
    ] {
        for finding in findings {
            let _ = write!(
                output,
                "\n{label} [{}] {} ({})\n  next: {}",
                finding.id, finding.title, finding.affected_resource, finding.suggestion
            );
        }
    }
    for unknown in &report.unknown {
        let _ = write!(
            output,
            "\nUNKNOWN [{}] {}: {}",
            unknown.source, unknown.affected_resource, unknown.reason
        );
    }
    output
}

/// `--explain`: every check, what it found, what exactly it tests, the
/// numbers that decide it, and where the manual covers it.
fn render_explain(report: &WtfReport) -> String {
    let mut output = String::new();
    render_heading(&mut output, report);
    for row in check_rows(report) {
        let _ = writeln!(output, "\n{}: {}", row.name, row.status().label());
        for (label, findings) in [("CRITICAL", &row.critical), ("WARNING", &row.warnings)] {
            for finding in findings {
                render_finding(&mut output, label, finding);
            }
        }
        for unknown in &row.unknown {
            let _ = writeln!(
                output,
                "  UNKNOWN [{}] {}: {}",
                unknown.source, unknown.affected_resource, unknown.reason
            );
        }
        for ok in &row.ok {
            let _ = writeln!(output, "  OK: {}", ok.description);
        }
        let Some(kind) = row.kind else {
            continue;
        };
        wrap_into(&mut output, "  tests: ", kind.tests);
        for threshold in kind
            .findings
            .iter()
            .filter_map(|id| finding(id))
            .flat_map(|kind| kind.thresholds)
        {
            let _ = writeln!(
                output,
                "  threshold: {} ({}): {}",
                threshold_value(threshold.value, threshold.unit),
                threshold.constant,
                threshold.meaning
            );
        }
        let _ = writeln!(output, "  docs: {}", kind.docs_url());
        let _ = writeln!(output, "  manual: relish manual {}", kind.manual);
    }
    let _ = write!(output, "\n{}", summary_line(report));
    output
}

fn render_finding(output: &mut String, label: &str, finding: &WtfFinding) {
    let _ = writeln!(
        output,
        "  {label} [{}] {} ({})",
        finding.id, finding.title, finding.affected_resource
    );
    for detail in &finding.details {
        let _ = writeln!(output, "    {detail}");
    }
    for event in &finding.correlated_events {
        let _ = writeln!(
            output,
            "    related {} at {}: {}",
            event.kind, event.timestamp, event.message
        );
    }
    let _ = writeln!(output, "    next: {}", finding.suggestion);
}

/// A threshold as a reader says it: `15 minutes`, not `900`.
fn threshold_value(value: u64, unit: Unit) -> String {
    let plural = |count: u64, word: &str| {
        if count == 1 {
            format!("1 {word}")
        } else {
            format!("{count} {word}s")
        }
    };
    match unit {
        Unit::Count => value.to_string(),
        Unit::Percent => format!("{value}%"),
        Unit::Seconds if value >= 86_400 && value.is_multiple_of(86_400) => {
            plural(value / 86_400, "day")
        }
        Unit::Seconds if value >= 3_600 && value.is_multiple_of(3_600) => {
            plural(value / 3_600, "hour")
        }
        Unit::Seconds if value >= 60 && value.is_multiple_of(60) => plural(value / 60, "minute"),
        Unit::Seconds => plural(value, "second"),
    }
}

/// Word-wrap `text` after `prefix`, indenting continuation lines to match.
fn wrap_into(output: &mut String, prefix: &str, text: &str) {
    const WIDTH: usize = 80;
    let indent = " ".repeat(prefix.len());
    let mut line = prefix.to_string();
    let mut empty = true;
    for word in text.split_whitespace() {
        if !empty && line.len() + 1 + word.len() > WIDTH {
            let _ = writeln!(output, "{line}");
            line = indent.clone();
            empty = true;
        }
        if !empty {
            line.push(' ');
        }
        line.push_str(word);
        empty = false;
    }
    let _ = writeln!(output, "{line}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relish::wtf::{WTF_SCHEMA_VERSION, WtfOk, WtfSummary, WtfUnknown};

    fn report() -> WtfReport {
        WtfReport {
            schema_version: WTF_SCHEMA_VERSION,
            cluster_name: "test".to_string(),
            collected_at: 10,
            node_count: 2,
            critical: Vec::new(),
            warnings: Vec::new(),
            unknown: Vec::new(),
            ok: vec![WtfOk {
                id: "nodes".to_string(),
                description: "all 2 nodes alive".to_string(),
            }],
            summary: WtfSummary {
                critical_count: 0,
                warning_count: 0,
                unknown_count: 0,
                ok_count: 1,
            },
        }
    }

    #[test]
    fn unknown_evidence_maps_to_warning_exit_and_is_rendered() {
        let mut report = report();
        report.unknown.push(WtfUnknown {
            source: "events".to_string(),
            reason: "node ring restarted".to_string(),
            affected_resource: "cluster".to_string(),
        });
        report.summary.unknown_count = 1;

        assert_eq!(report_outcome(&report), CommandOutcome::Warnings);
        let output = render_report(&report, OutputFormat::Human, WtfView::Table).unwrap();
        assert!(output.contains("unknown"));
        assert!(output.contains("node ring restarted"));
    }

    #[test]
    fn critical_evidence_takes_exit_one_over_warnings() {
        let mut report = report();
        report.critical.push(WtfFinding {
            id: "no-leader".to_string(),
            title: "no leader".to_string(),
            details: Vec::new(),
            suggestion: "inspect council".to_string(),
            correlated_events: Vec::new(),
            affected_resource: "council".to_string(),
        });
        report.unknown.push(WtfUnknown {
            source: "events".to_string(),
            reason: "unavailable".to_string(),
            affected_resource: "cluster".to_string(),
        });

        assert_eq!(report_outcome(&report), CommandOutcome::Problems);
    }

    #[test]
    fn json_and_yaml_preserve_first_class_unknown_list() {
        let mut report = report();
        report.unknown.push(WtfUnknown {
            source: "events".to_string(),
            reason: "unavailable".to_string(),
            affected_resource: "cluster".to_string(),
        });

        let json = render_report(&report, OutputFormat::Json, WtfView::Table).unwrap();
        let yaml = render_report(&report, OutputFormat::Yaml, WtfView::Table).unwrap();

        assert!(json.contains("\"unknown\""));
        assert!(yaml.contains("unknown:"));
    }

    #[test]
    fn watch_rejects_machine_streams() {
        for output in [OutputFormat::Json, OutputFormat::Yaml] {
            let error = validate_args(&WtfArgs {
                app: None,
                watch: true,
                interval: Duration::from_secs(30),
                output,
                view: WtfView::Table,
            })
            .unwrap_err();
            assert!(error.to_string().contains("only --output human"));
        }
    }

    #[test]
    fn short_and_explain_refuse_machine_output_and_work_with_watch() {
        for view in [WtfView::Short, WtfView::Explain] {
            for output in [OutputFormat::Json, OutputFormat::Yaml] {
                let args = WtfArgs {
                    app: None,
                    watch: false,
                    interval: Duration::from_secs(30),
                    output,
                    view,
                };
                let error = validate_args(&args).unwrap_err();
                assert!(
                    error.to_string().contains("only the human report"),
                    "{error}"
                );
            }
            let watched = WtfArgs {
                app: Some("api".to_string()),
                watch: true,
                interval: Duration::from_secs(30),
                output: OutputFormat::Human,
                view,
            };
            assert!(validate_args(&watched).is_ok());
        }
    }

    /// One check of each status: `crashloops` critical, `disks` warning,
    /// `alerts` unknown, `nodes` and `council` OK.
    fn mixed_report() -> WtfReport {
        let mut report = report();
        report.critical.push(WtfFinding {
            id: "crashloop".to_string(),
            title: "app api/default restarted repeatedly".to_string(),
            details: vec!["3 timestamped restarts in the last 15 minutes".to_string()],
            suggestion: "app api is crashlooping; inspect its recent logs and run \
                         `relish rollback api` if this started after a deploy"
                .to_string(),
            correlated_events: vec![crate::relish::wtf::CorrelatedEvent {
                timestamp: 9,
                kind: "restart".to_string(),
                message: "exit code 1".to_string(),
            }],
            affected_resource: "app.api/default".to_string(),
        });
        for (node, percent) in [("node-1", "88.0"), ("node-2", "90.5")] {
            report.warnings.push(WtfFinding {
                id: "disk-high".to_string(),
                title: format!("node {node} filesystem for images is {percent}% full"),
                details: Vec::new(),
                suggestion: format!(
                    "node {node} disk is at {percent}%; prune images (`relish pickle gc`) or logs"
                ),
                correlated_events: Vec::new(),
                affected_resource: format!("node.{node}"),
            });
        }
        report.unknown.push(WtfUnknown {
            source: "alerts".to_string(),
            reason: "node node-2: alerts: timed out after 5s".to_string(),
            affected_resource: "cluster".to_string(),
        });
        report.ok.push(WtfOk {
            id: "council".to_string(),
            description: "council quorum healthy (2/2)".to_string(),
        });
        report.summary = WtfSummary {
            critical_count: 1,
            warning_count: 2,
            unknown_count: 1,
            ok_count: 2,
        };
        report
    }

    fn human(report: &WtfReport, view: WtfView) -> String {
        render_report(report, OutputFormat::Human, view).unwrap()
    }

    #[test]
    fn the_table_shows_one_row_per_check_worst_first() {
        insta::assert_snapshot!(human(&mixed_report(), WtfView::Table));
    }

    #[test]
    fn short_lists_only_what_is_not_ok_with_its_next_step() {
        insta::assert_snapshot!(human(&mixed_report(), WtfView::Short));
    }

    #[test]
    fn short_is_just_the_counts_when_everything_is_ok() {
        assert_eq!(
            human(&report(), WtfView::Short),
            "Summary: 0 critical, 0 warning, 0 unknown, 1 OK"
        );
    }

    #[test]
    fn explain_says_what_every_check_tests_and_where_to_read_more() {
        insta::assert_snapshot!(human(&mixed_report(), WtfView::Explain));
    }

    #[test]
    fn a_result_no_check_claims_still_gets_a_row() {
        let mut report = report();
        report.unknown.push(WtfUnknown {
            source: "something-new".to_string(),
            reason: "not read".to_string(),
            affected_resource: "cluster".to_string(),
        });
        let table = human(&report, WtfView::Table);
        assert!(
            table.contains("something-new  unknown   not read"),
            "{table}"
        );
        let explain = human(&report, WtfView::Explain);
        assert!(explain.contains("something-new: unknown"), "{explain}");
    }

    #[test]
    fn thresholds_read_as_durations() {
        assert_eq!(threshold_value(900, Unit::Seconds), "15 minutes");
        assert_eq!(threshold_value(14 * 86_400, Unit::Seconds), "14 days");
        assert_eq!(threshold_value(3_600, Unit::Seconds), "1 hour");
        assert_eq!(threshold_value(90, Unit::Seconds), "90 seconds");
        assert_eq!(threshold_value(85, Unit::Percent), "85%");
        assert_eq!(threshold_value(3, Unit::Count), "3");
    }
}
