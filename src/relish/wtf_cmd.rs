//! `relish wtf` collection, rendering and exit-code orchestration.

use std::fmt::Write as _;
use std::io::{IsTerminal, Write as _};
use std::time::Duration;

use super::client::BunClient;
use super::wtf::{WtfFinding, WtfReport, collect, diagnose};
use super::{CommandOutcome, OutputFormat, RelishError};

/// Arguments accepted by the diagnostic command body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WtfArgs {
    /// Optional application name to diagnose.
    pub app: Option<String>,
    /// Re-run collection every 30 seconds until interrupted.
    pub watch: bool,
    /// Human, JSON or YAML output.
    pub output: OutputFormat,
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
    println!("{}", render_report(&report, args.output)?);
    Ok(report_outcome(&report))
}

fn validate_args(args: &WtfArgs) -> Result<(), RelishError> {
    if args.watch && args.output != OutputFormat::Human {
        return Err(RelishError::InvalidFlag {
            flag: "watch".to_string(),
            reason: "continuous output supports only --output human".to_string(),
        });
    }
    Ok(())
}

async fn run_watch(args: &WtfArgs, client: &BunClient) -> Result<CommandOutcome, RelishError> {
    let mut first = true;
    loop {
        let report = diagnose(&collect(client, args.app.as_deref()).await?);
        if !first && std::io::stdout().is_terminal() {
            print!("\x1b[2J\x1b[H");
        }
        first = false;
        println!("{}", render_report(&report, OutputFormat::Human)?);
        std::io::stdout().flush()?;
        tokio::select! {
            signal = tokio::signal::ctrl_c() => {
                signal?;
                return Ok(CommandOutcome::Clean);
            }
            _ = tokio::time::sleep(Duration::from_secs(30)) => {}
        }
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

fn render_report(report: &WtfReport, output: OutputFormat) -> Result<String, RelishError> {
    match output {
        OutputFormat::Human => Ok(render_human(report)),
        OutputFormat::Json => {
            serde_json::to_string_pretty(report).map_err(RelishError::SerialiseJson)
        }
        OutputFormat::Yaml => serde_yaml::to_string(report).map_err(RelishError::SerialiseYaml),
    }
}

fn render_human(report: &WtfReport) -> String {
    let mut output = String::new();
    let _ = writeln!(
        output,
        "Reliaburger diagnosis: {} ({} node{})",
        report.cluster_name,
        report.node_count,
        if report.node_count == 1 { "" } else { "s" }
    );
    let _ = writeln!(output, "collected at unix {}", report.collected_at);

    render_findings(&mut output, "CRITICAL", &report.critical);
    render_findings(&mut output, "WARNING", &report.warnings);

    if !report.unknown.is_empty() {
        let _ = writeln!(output, "\nUNKNOWN ({})", report.unknown.len());
        for unknown in &report.unknown {
            let _ = writeln!(
                output,
                "  [{}] {}: {}",
                unknown.source, unknown.affected_resource, unknown.reason
            );
        }
    }
    if !report.ok.is_empty() {
        let _ = writeln!(output, "\nOK ({})", report.ok.len());
        for ok in &report.ok {
            let _ = writeln!(output, "  [{}] {}", ok.id, ok.description);
        }
    }
    let _ = write!(
        output,
        "\nSummary: {} critical, {} warning, {} unknown, {} OK",
        report.summary.critical_count,
        report.summary.warning_count,
        report.summary.unknown_count,
        report.summary.ok_count
    );
    output
}

fn render_findings(output: &mut String, heading: &str, findings: &[WtfFinding]) {
    if findings.is_empty() {
        return;
    }
    let _ = writeln!(output, "\n{heading} ({})", findings.len());
    for finding in findings {
        let _ = writeln!(
            output,
            "  [{}] {} ({})",
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
        let output = render_report(&report, OutputFormat::Human).unwrap();
        assert!(output.contains("UNKNOWN (1)"));
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

        let json = render_report(&report, OutputFormat::Json).unwrap();
        let yaml = render_report(&report, OutputFormat::Yaml).unwrap();

        assert!(json.contains("\"unknown\""));
        assert!(yaml.contains("unknown:"));
    }

    #[test]
    fn watch_rejects_machine_streams() {
        for output in [OutputFormat::Json, OutputFormat::Yaml] {
            let error = validate_args(&WtfArgs {
                app: None,
                watch: true,
                output,
            })
            .unwrap_err();
            assert!(error.to_string().contains("only --output human"));
        }
    }
}
