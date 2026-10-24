//! How `relish policy test` and `relish apply --dry-run` show what the
//! policies said.

use crate::config::PolicyAction;

use super::{Evaluation, Filled, Finding, Verdict};

/// The fields the defaults filled in and the findings in `verdict`, as an
/// aligned table, one line each: fills first, then denials, then
/// warnings. `None` when there's nothing to show.
pub fn findings_table(filled: &[Filled], verdict: &Verdict) -> Option<String> {
    let findings: Vec<&Finding> = verdict
        .denials
        .iter()
        .chain(verdict.warnings.iter())
        .collect();
    if findings.is_empty() && filled.is_empty() {
        return None;
    }
    let fill_rows = filled.iter().map(|fill| {
        [
            "FILL".to_string(),
            format!("{}/{}", fill.app.namespace, fill.app.name),
            fill.policy.clone(),
            format!("{} = {}", fill.field, fill.value),
        ]
    });
    let finding_rows = findings.iter().map(|finding| {
        [
            match finding.action {
                PolicyAction::Deny => "DENY".to_string(),
                PolicyAction::Warn => "WARN".to_string(),
            },
            format!("{}/{}", finding.app.namespace, finding.app.name),
            finding.policy.clone(),
            finding.message.clone(),
        ]
    });
    let rows: Vec<[String; 4]> = fill_rows.chain(finding_rows).collect();
    let width = |column: usize| rows.iter().map(|row| row[column].len()).max().unwrap_or(0);
    let (app_width, policy_width) = (width(1), width(2));
    let mut table = String::new();
    for [action, app, policy, message] in rows {
        table.push_str(&format!(
            "  {action:4}  {app:app_width$}  {policy:policy_width$}  {message}\n"
        ));
    }
    Some(table)
}

/// The report `relish policy test` prints: how many apps were checked
/// against how many policies, every filled field and finding, and whether
/// an apply would go through.
pub fn render_test(evaluation: &Evaluation, apps: usize, policies: usize) -> String {
    let verdict = &evaluation.verdict;
    let mut report = format!("Checked {apps} app(s) against {policies} policy(ies).\n");
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
    report.push_str(&format!(
        "{} denial(s), {} warning(s): {outcome}.",
        verdict.denials.len(),
        verdict.warnings.len()
    ));
    report
}
