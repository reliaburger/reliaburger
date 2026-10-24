//! How `relish policy test`, `relish policy audit` and `relish apply
//! --dry-run` show what the policies said.

use crate::config::PolicyAction;

use super::audit::PolicyAudit;
use super::{Evaluation, Filled, Finding, Verdict};

/// The fields the defaults filled in and the findings in `verdict`, as an
/// aligned table, one line each: fills first, then denials, warnings and
/// audit findings. `None` when there's nothing to show.
pub fn findings_table(filled: &[Filled], verdict: &Verdict) -> Option<String> {
    let findings: Vec<&Finding> = verdict
        .denials
        .iter()
        .chain(&verdict.warnings)
        .chain(&verdict.audits)
        .collect();
    if findings.is_empty() && filled.is_empty() {
        return None;
    }
    let fill_rows = filled.iter().map(|fill| {
        [
            "FILL".to_string(),
            format!("app {}/{}", fill.app.namespace, fill.app.name),
            fill.policy.clone(),
            format!("{} = {}", fill.field, fill.value),
        ]
    });
    let finding_rows = findings.iter().map(|finding| {
        [
            match finding.action {
                PolicyAction::Deny => "DENY".to_string(),
                PolicyAction::Warn => "WARN".to_string(),
                PolicyAction::Audit => "AUDIT".to_string(),
            },
            format!(
                "{} {}/{}",
                finding.kind.as_str(),
                finding.workload.namespace,
                finding.workload.name
            ),
            finding.policy.clone(),
            finding.message.clone(),
        ]
    });
    let rows: Vec<[String; 4]> = fill_rows.chain(finding_rows).collect();
    let width = |column: usize| rows.iter().map(|row| row[column].len()).max().unwrap_or(0);
    let (action_width, workload_width, policy_width) = (width(0), width(1), width(2));
    let mut table = String::new();
    for [action, workload, policy, message] in rows {
        table.push_str(&format!(
            "  {action:action_width$}  {workload:workload_width$}  {policy:policy_width$}  {message}\n"
        ));
    }
    Some(table)
}

/// What was checked: `N app(s)`, with `and M job(s)` when there were any.
fn checked(apps: usize, jobs: usize) -> String {
    if jobs == 0 {
        format!("{apps} app(s)")
    } else {
        format!("{apps} app(s) and {jobs} job(s)")
    }
}

/// The counts at the end of a report: `N denial(s), N warning(s)`, with
/// the audit findings when there were any.
fn counts(verdict: &Verdict) -> String {
    let mut counts = format!(
        "{} denial(s), {} warning(s)",
        verdict.denials.len(),
        verdict.warnings.len()
    );
    if !verdict.audits.is_empty() {
        counts.push_str(&format!(", {} audit finding(s)", verdict.audits.len()));
    }
    counts
}

/// The report `relish policy test` prints: how many apps and jobs were
/// checked against how many policies, every filled field and finding, and
/// whether an apply would go through.
pub fn render_test(evaluation: &Evaluation, apps: usize, jobs: usize, policies: usize) -> String {
    let verdict = &evaluation.verdict;
    let mut report = format!(
        "Checked {} against {policies} policy(ies).\n",
        checked(apps, jobs)
    );
    if let Some(table) = findings_table(&evaluation.filled, verdict) {
        report.push('\n');
        report.push_str(&table);
    }
    report.push('\n');
    let outcome = if verdict.is_allowed() {
        "an apply would be accepted"
    } else {
        "an apply would be refused"
    };
    if !evaluation.filled.is_empty() {
        report.push_str(&format!(
            "{} field(s) filled by defaults, ",
            evaluation.filled.len()
        ));
    }
    report.push_str(&format!("{}: {outcome}.", counts(verdict)));
    report
}

/// The report `relish policy audit` prints (#1119): every committed app
/// and job a policy would now refuse, warn about or record, and a reminder
/// that none of them was touched.
pub fn render_audit(audit: &PolicyAudit) -> String {
    if audit.policies == 0 {
        return format!(
            "No policies (generation {}): nothing to audit.\n",
            audit.generation
        );
    }
    let mut report = format!(
        "Checked {} against {} policy(ies) at generation {}.\n",
        checked(audit.apps, audit.jobs),
        audit.policies,
        audit.generation
    );
    if let Some(broken) = &audit.broken {
        report.push_str(&format!("\n{broken}\n"));
        return report;
    }
    match findings_table(&[], &audit.verdict) {
        Some(table) => {
            report.push('\n');
            report.push_str(&table);
            report.push('\n');
            report.push_str(&format!(
                "{}. Nothing was changed: each runs as it is until it's next applied.\n",
                counts(&audit.verdict)
            ));
        }
        None => report.push_str("\nEvery committed app and job passes.\n"),
    }
    report
}
