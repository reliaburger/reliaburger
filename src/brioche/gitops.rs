//! GitOps status page rendering.
//!
//! Produces a complete HTML page for the Lettuce GitOps engine: current
//! sync status, the last applied commit, the coordinator node, and a
//! timeline of recent syncs. The data comes straight from the Raft-replicated
//! [`SyncState`], so any council member can serve an identical view.

use super::app_detail::{render_head, render_nav};
use super::dashboard::{escape_html, status_dot};
use crate::lettuce::types::{SyncPhase, SyncResult, SyncState};

/// Render the GitOps status page as a complete HTML page.
///
/// The page polls nothing itself — GitOps syncs are minutes apart, not
/// seconds, so a plain navigation (or the browser's refresh) is enough.
pub fn render_gitops(state: &SyncState) -> String {
    let mut html = String::with_capacity(8192);

    html.push_str(&render_head("GitOps"));
    html.push_str(render_nav());

    // Header: title, current phase, coordinator.
    html.push_str("<div class=\"detail-header\">\n");
    html.push_str("<h1>GitOps</h1>\n");
    let phase_dot = status_dot(phase_state(state.phase));
    let coordinator = state
        .coordinator_node_id
        .as_deref()
        .unwrap_or("(none elected)");
    html.push_str(&format!(
        "<div class=\"detail-meta\">\
         <span>Phase: {phase_dot} <strong>{}</strong></span>\
         <span>Coordinator: <strong>{}</strong></span>\
         <span>Failures: <strong>{}</strong></span>\
         </div>\n",
        escape_html(phase_label(state.phase)),
        escape_html(coordinator),
        state.consecutive_failures,
    ));
    html.push_str("</div>\n");

    // A durable error, if the last attempt failed.
    if let Some(error) = &state.last_error {
        html.push_str(&format!(
            "<section>\n<p class=\"alert\">Last error: {}</p>\n</section>\n",
            escape_html(error),
        ));
    }

    // Last applied commit.
    html.push_str("<section>\n<h2>Last Applied Commit</h2>\n");
    match &state.last_applied_commit {
        Some(commit) => {
            html.push_str("<table>\n");
            html.push_str(&format!(
                "<tr><th>Commit</th><td>{}</td></tr>\n",
                escape_html(&short_sha(&commit.sha)),
            ));
            html.push_str(&format!(
                "<tr><th>Message</th><td>{}</td></tr>\n",
                escape_html(&commit.message),
            ));
            html.push_str(&format!(
                "<tr><th>Author</th><td>{}</td></tr>\n",
                escape_html(&commit.author),
            ));
            html.push_str(&format!(
                "<tr><th>Signature</th><td>{}</td></tr>\n",
                escape_html(&format!("{:?}", commit.signature)),
            ));
            if let Some(at) = state.last_sync_at {
                html.push_str(&format!(
                    "<tr><th>Applied at</th><td>{at} (Unix ms), took {} ms</td></tr>\n",
                    state.last_sync_duration_ms,
                ));
            }
            html.push_str("</table>\n");

            if let Some(diff) = &state.last_diff_summary {
                html.push_str(&format!(
                    "<p class=\"empty\">Changes: {} added, {} modified, {} removed</p>\n",
                    diff.added, diff.modified, diff.removed,
                ));
            }
        }
        None => {
            html.push_str("<p class=\"empty\">no commit applied yet</p>\n");
        }
    }
    html.push_str("</section>\n");

    // Sync history (newest first).
    html.push_str("<section>\n<h2>Sync History</h2>\n");
    if state.history.is_empty() {
        html.push_str("<p class=\"empty\">no syncs recorded yet</p>\n");
    } else {
        html.push_str(
            "<table>\n<tr><th>When (Unix ms)</th><th>Commit</th><th>Result</th>\
             <th>Duration</th><th>Changes</th></tr>\n",
        );
        for entry in state.history.iter().rev() {
            let (dot, label) = result_display(&entry.result);
            let changes = entry
                .diff_summary
                .as_ref()
                .map(|d| format!("+{} ~{} -{}", d.added, d.modified, d.removed))
                .unwrap_or_else(|| "—".to_string());
            html.push_str(&format!(
                "<tr><td>{}</td><td>{}</td><td>{dot} {}</td><td>{} ms</td><td>{}</td></tr>\n",
                entry.timestamp,
                escape_html(&short_sha(&entry.commit.sha)),
                escape_html(label),
                entry.duration_ms,
                escape_html(&changes),
            ));
        }
        html.push_str("</table>\n");
    }
    html.push_str("</section>\n");

    html.push_str("</body>\n</html>\n");
    html
}

/// The first 8 characters of a SHA, or the whole thing if shorter.
fn short_sha(sha: &str) -> String {
    sha.chars().take(8).collect()
}

/// A human label for a sync phase.
fn phase_label(phase: SyncPhase) -> &'static str {
    match phase {
        SyncPhase::Idle => "Idle",
        SyncPhase::Fetching => "Fetching",
        SyncPhase::Verifying => "Verifying",
        SyncPhase::Parsing => "Parsing",
        SyncPhase::Diffing => "Diffing",
        SyncPhase::Applying => "Applying",
        SyncPhase::Error => "Error",
    }
}

/// Map a phase to a state string `status_dot` understands (green/amber/red).
fn phase_state(phase: SyncPhase) -> &'static str {
    match phase {
        SyncPhase::Error => "failed",
        SyncPhase::Idle => "running",
        _ => "pending",
    }
}

/// A coloured dot and label for a sync outcome.
fn result_display(result: &SyncResult) -> (&'static str, &'static str) {
    match result {
        SyncResult::Success => (status_dot("running"), "Success"),
        SyncResult::PartialSuccess { .. } => (status_dot("pending"), "Partial"),
        SyncResult::Failure { .. } => (status_dot("failed"), "Failure"),
        SyncResult::Skipped { .. } => (status_dot("unknown"), "Skipped"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lettuce::types::{CommitInfo, DiffSummary, SignatureStatus, SyncHistoryEntry};

    fn sample_commit(sha: &str, message: &str) -> CommitInfo {
        CommitInfo {
            sha: sha.to_string(),
            message: message.to_string(),
            author: "dev".to_string(),
            timestamp: 1700000000000,
            signature: SignatureStatus::Verified,
        }
    }

    fn populated_state() -> SyncState {
        let mut state = SyncState {
            last_applied_commit: Some(sample_commit("abcdef1234567890", "deploy web v2")),
            last_fetched_commit: Some(sample_commit("abcdef1234567890", "deploy web v2")),
            phase: SyncPhase::Idle,
            last_sync_at: Some(1700000000000),
            last_attempt_at: Some(1700000000000),
            last_sync_duration_ms: 42,
            consecutive_failures: 0,
            last_error: None,
            coordinator_node_id: Some("node-01".to_string()),
            last_diff_summary: Some(DiffSummary {
                added: 2,
                modified: 1,
                removed: 0,
            }),
            ..Default::default()
        };
        state.history.push_back(SyncHistoryEntry {
            commit: sample_commit("abcdef1234567890", "deploy web v2"),
            timestamp: 1700000000000,
            duration_ms: 42,
            result: SyncResult::Success,
            diff_summary: Some(DiffSummary {
                added: 2,
                modified: 1,
                removed: 0,
            }),
        });
        state
    }

    #[test]
    fn renders_empty_state() {
        let html = render_gitops(&SyncState::default());
        assert!(html.contains("<title>GitOps — Reliaburger</title>"));
        assert!(html.contains("no commit applied yet"));
        assert!(html.contains("no syncs recorded yet"));
        assert!(html.contains("(none elected)"));
    }

    #[test]
    fn renders_populated_state() {
        let html = render_gitops(&populated_state());
        // Coordinator and commit surface.
        assert!(html.contains("node-01"));
        assert!(html.contains("abcdef12")); // short sha
        assert!(html.contains("deploy web v2"));
        // Diff summary and history row.
        assert!(html.contains("2 added, 1 modified, 0 removed"));
        assert!(html.contains("Success"));
        assert!(html.contains("42 ms"));
    }

    #[test]
    fn shows_the_last_error() {
        let mut state = SyncState {
            phase: SyncPhase::Error,
            last_error: Some("repo access failed: host unreachable".to_string()),
            consecutive_failures: 3,
            ..Default::default()
        };
        state.coordinator_node_id = Some("node-02".to_string());
        let html = render_gitops(&state);
        assert!(html.contains("Last error: repo access failed"));
        assert!(html.contains("node-02"));
    }

    #[test]
    fn escapes_hostile_commit_message() {
        let state = SyncState {
            last_applied_commit: Some(sample_commit("deadbeef", "<script>alert(1)</script>")),
            ..SyncState::default()
        };
        let html = render_gitops(&state);
        assert!(!html.contains("<script>alert(1)</script>"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn history_is_newest_first() {
        let mut state = SyncState::default();
        state.history.push_back(SyncHistoryEntry {
            commit: sample_commit("1111111111", "old"),
            timestamp: 1000,
            duration_ms: 1,
            result: SyncResult::Success,
            diff_summary: None,
        });
        state.history.push_back(SyncHistoryEntry {
            commit: sample_commit("2222222222", "new"),
            timestamp: 2000,
            duration_ms: 1,
            result: SyncResult::Failure {
                error: "boom".to_string(),
            },
            diff_summary: None,
        });
        let html = render_gitops(&state);
        // Newest (22222222) must render before oldest (11111111).
        let pos_new = html.find("22222222").unwrap();
        let pos_old = html.find("11111111").unwrap();
        assert!(pos_new < pos_old, "history should be newest-first");
    }
}
