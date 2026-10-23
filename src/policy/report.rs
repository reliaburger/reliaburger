//! How `relish policy test` and `relish apply --dry-run` show what the
//! policies said.

use crate::config::PolicyAction;

use super::{Finding, Verdict};

/// The findings in `verdict` as an aligned table, one line each, denials
/// first, or `None` when there are none.
pub fn findings_table(verdict: &Verdict) -> Option<String> {
    let findings: Vec<&Finding> = verdict
        .denials
        .iter()
        .chain(verdict.warnings.iter())
        .collect();
    if findings.is_empty() {
        return None;
    }
    let rows: Vec<[String; 4]> = findings
        .iter()
        .map(|finding| {
            [
                match finding.action {
                    PolicyAction::Deny => "DENY".to_string(),
                    PolicyAction::Warn => "WARN".to_string(),
                },
                format!("{}/{}", finding.app.namespace, finding.app.name),
                finding.policy.clone(),
                finding.message.clone(),
            ]
        })
        .collect();
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
/// against how many policies, every finding, and whether an apply would
/// go through.
pub fn render_test(verdict: &Verdict, apps: usize, policies: usize) -> String {
    let mut report = format!("Checked {apps} app(s) against {policies} policy(ies).\n");
    if let Some(table) = findings_table(verdict) {
        report.push('\n');
        report.push_str(&table);
    }
    report.push('\n');
    let outcome = if verdict.is_allowed() {
        "an apply would be accepted"
    } else {
        "an apply would be refused"
    };
    report.push_str(&format!(
        "{} denial(s), {} warning(s): {outcome}.",
        verdict.denials.len(),
        verdict.warnings.len()
    ));
    report
}
