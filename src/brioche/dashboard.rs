//! Dashboard rendering.
//!
//! Produces a complete HTML page showing cluster overview: `relish
//! status`'s lines from the cluster summary, then apps, nodes and alerts.
//! Uses HTMX for automatic partial-page refreshes instead of full-page
//! reloads. Charts are initialised client-side by uPlot via
//! `data-chart-config` attributes.

use serde::{Deserialize, Serialize};

use crate::bun::summary::ClusterSummary;
use crate::bun::summary::verdict::{Mark, Verdict, problems, section_lines, verdict};

use super::fragments::{
    render_alerts_table_fragment, render_apps_table_fragment, render_nodes_table_fragment,
};

/// Data backing the dashboard.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DashboardData {
    pub cluster_name: String,
    /// The cluster at a glance, as `GET /v1/cluster/summary` answers it.
    /// The counts at the top of the page come from here, not from the
    /// lists below, so a node that didn't answer shows as partial.
    pub summary: Option<ClusterSummary>,
    /// Why the app list couldn't be read whole (a node didn't answer).
    /// The page still renders, with the summary saying how much it saw.
    pub apps_error: Option<String>,
    pub apps: Vec<DashboardApp>,
    pub nodes: Vec<DashboardNode>,
    pub alerts: Vec<DashboardAlert>,
}

/// An app row in the dashboard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardApp {
    pub name: String,
    pub namespace: String,
    pub instances_running: usize,
    pub instances_desired: usize,
    pub state: String,
}

/// A node row in the dashboard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardNode {
    pub name: String,
    pub state: String,
    pub app_count: usize,
}

/// An alert row in the dashboard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DashboardAlert {
    /// Labels identifying the firing series.
    #[serde(default)]
    pub labels: std::collections::BTreeMap<String, String>,
    pub name: String,
    pub severity: String,
    pub description: String,
}

/// Render the dashboard as a complete HTML page.
///
/// Uses HTMX for automatic polling of each section independently.
/// The apps, nodes, and alerts sections refresh every 5s / 3s via
/// `hx-get` attributes, replacing the old `<meta http-equiv="refresh">`
/// approach that reloaded the entire page.
pub fn render_dashboard(data: &DashboardData) -> String {
    let mut html = String::with_capacity(8192);

    html.push_str(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Reliaburger</title>
<link rel="stylesheet" href="/ui/static/uplot.min.css">
<link rel="stylesheet" href="/ui/static/brioche.css">
<script src="/ui/static/htmx.min.js"></script>
<script src="/ui/static/uplot.min.js"></script>
<script src="/ui/static/brioche.js"></script>
</head>
<body>
<nav>
<span class="brand">Reliaburger Dashboard</span>
</nav>
"#,
    );

    if !data.cluster_name.is_empty() {
        html.push_str(&format!(
            "<span class=\"cluster\">{}</span>\n",
            escape_html(&data.cluster_name)
        ));
    }

    // The summary (HTMX-polled): the same lines `relish status` prints.
    html.push_str("<section>\n<h2>Cluster</h2>\n");
    html.push_str(
        "<div hx-get=\"/ui/fragment/summary\" hx-trigger=\"every 5s\" hx-swap=\"innerHTML\">\n",
    );
    html.push_str(&render_summary_fragment(data.summary.as_ref()));
    html.push_str("</div>\n</section>\n");

    // Apps table (HTMX-polled)
    html.push_str("<section>\n<h2>Apps</h2>\n");
    html.push_str(
        "<div hx-get=\"/ui/fragment/apps\" hx-trigger=\"every 5s\" hx-swap=\"innerHTML\">\n",
    );
    match &data.apps_error {
        Some(error) => html.push_str(&format!(
            "<p class=\"empty\">Couldn't list every app: {}</p>\n",
            escape_html(error)
        )),
        None => html.push_str(&render_apps_table_fragment(&data.apps)),
    }
    html.push_str("</div>\n</section>\n");

    html.push_str("<section><h2>Job runs and schedules</h2><div hx-get=\"/ui/fragment/batches\" hx-trigger=\"load, every 3s\" hx-swap=\"innerHTML\">Loading summaries…</div></section>\n");

    // Nodes table (HTMX-polled)
    html.push_str("<section>\n<h2>Nodes</h2>\n");
    html.push_str(
        "<div hx-get=\"/ui/fragment/nodes\" hx-trigger=\"every 5s\" hx-swap=\"innerHTML\">\n",
    );
    html.push_str(&render_nodes_table_fragment(&data.nodes));
    html.push_str("</div>\n</section>\n");

    // Alerts section (HTMX-polled, more frequently)
    html.push_str("<section>\n<h2>Alerts</h2>\n");
    html.push_str(
        "<div hx-get=\"/ui/fragment/alerts\" hx-trigger=\"every 3s\" hx-swap=\"innerHTML\">\n",
    );
    html.push_str(&render_alerts_table_fragment(&data.alerts));
    html.push_str("</div>\n</section>\n");

    html.push_str("</body>\n</html>\n");
    html
}

/// The cluster summary as `relish status` reads it: the verdict, then one
/// row per section, each that needs attention coloured and naming the
/// command that shows the detail. A section that saw only some nodes says
/// `partial (n of m nodes)` in its own text, from the shared `assess`.
/// Brioche doesn't run `wtf`'s checks, so there's no `wtf` row.
pub fn render_summary_fragment(summary: Option<&ClusterSummary>) -> String {
    let Some(summary) = summary else {
        return "<p class=\"empty\">The cluster summary couldn't be read.</p>\n".to_string();
    };
    let lines = section_lines(summary);
    let badge = match verdict(&lines) {
        Verdict::Healthy => "badge-green",
        Verdict::Degraded => "badge-amber",
        Verdict::Down => "badge-red",
    };
    let problems = match problems(&lines) {
        0 => "no problems".to_string(),
        1 => "1 problem".to_string(),
        n => format!("{n} problems"),
    };
    let mut html = format!(
        "<p><span class=\"badge {badge}\">{}</span> {problems} ({} answered)</p>\n",
        verdict(&lines).word(),
        escape_html(&summary.answered_by)
    );
    html.push_str("<table><tbody>\n");
    for line in &lines {
        let class = match line.mark {
            Mark::Quiet => "",
            Mark::Note | Mark::Problem => " class=\"alert-warning\"",
            Mark::Down => " class=\"alert-critical\"",
        };
        let next = line
            .next
            .as_deref()
            .map(|next| format!("<code>{}</code>", escape_html(next)))
            .unwrap_or_default();
        // The agents line opens the Proposals page, where a person reads
        // and decides what agents asked for.
        let text = if line.section == "agents" {
            format!(
                "<a class=\"row-link\" href=\"/ui/proposals\">{}</a>",
                escape_html(&line.text)
            )
        } else {
            escape_html(&line.text)
        };
        html.push_str(&format!(
            "<tr{class}><td>{}</td><td>{text}</td><td>{next}</td></tr>\n",
            line.section,
        ));
    }
    html.push_str("</tbody></table>\n");
    html
}

/// Return a coloured dot span based on state.
pub fn status_dot(state: &str) -> &'static str {
    match state.to_lowercase().as_str() {
        "running" | "alive" | "healthy" => "<span class=\"dot-green\">●</span>",
        "pending" | "preparing" | "degraded" | "troubled" => "<span class=\"dot-amber\">●</span>",
        "failed" | "unhealthy" | "dead" | "blocked" | "down" => "<span class=\"dot-red\">●</span>",
        _ => "<span class=\"dot-grey\">●</span>",
    }
}

/// Escape HTML special characters, including both quote styles.
///
/// We escape `'` as well as `"` (AUTH8) because chart config JSON is embedded
/// in a single-quoted attribute (`data-chart-config='...'`). Without escaping
/// the apostrophe, an app or label value containing `'` closes the attribute
/// early and can inject markup. Escaping both quote characters makes the
/// output safe in either attribute context.
pub fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Render aggregate counters only. Names remain escaped like other dashboard input.
/// A schedule's last few outcomes, newest first, from its durable history:
/// what's left after the runs' own details were pruned.
fn recent_outcomes(history: &serde_json::Value) -> String {
    let outcomes: Vec<String> = history
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .rev()
        .take(5)
        .map(|outcome| {
            format!(
                "run {}: {} ({} succeeded, {} failed)",
                outcome["run"],
                escape_html(outcome["status"].as_str().unwrap_or("?")),
                outcome["succeeded"],
                outcome["failed"]
            )
        })
        .collect();
    if outcomes.is_empty() {
        String::new()
    } else {
        format!("<br>Last runs: {}", outcomes.join("; "))
    }
}

pub fn render_batches(rows: &[serde_json::Value]) -> String {
    let mut html = String::from(
        "<table><thead><tr><th>Run</th><th>Namespace</th><th>State</th><th>Succeeded / total</th><th>Failed</th><th>Queued</th><th>Accepted successes/s</th><th>Profiles</th></tr></thead><tbody>",
    );
    for row in rows {
        let name = escape_html(row["name"].as_str().unwrap_or("?"));
        let ns = escape_html(row["namespace"].as_str().unwrap_or("default"));
        let status = escape_html(row["status"].as_str().unwrap_or("unknown"));
        if row["kind"] == "schedule" {
            let expression = escape_html(row["cron"]["expression"].as_str().unwrap_or("?"));
            html.push_str(&format!("<tr><td>{name}</td><td>{ns}</td><td>{status}</td><td colspan=\"5\">UTC schedule: {expression}; {} tasks per occurrence{}</td></tr>", row["total"], recent_outcomes(&row["history"])));
            continue;
        }
        let rate = row["rates"]["successes_per_second"]
            .as_f64()
            .map_or("unknown".into(), |r| format!("{r:.1}"));
        html.push_str(&format!("<tr><td>{} {name}</td><td>{ns}</td><td>{status}</td><td>{} / {}</td><td>{}</td><td>{}</td><td>{rate}</td><td>{}</td></tr>", row["batch_id"].as_u64().unwrap_or(0), row["succeeded"].as_u64().unwrap_or(0), row["total"].as_u64().unwrap_or(0), row["failed"].as_u64().unwrap_or(0), row["queued"].as_u64().unwrap_or(0), row["cohorts"].as_array().map_or(1, Vec::len)));
        if row["status"] == "Unknown" {
            html.push_str("<tr><td colspan=\"8\">Outcome unknown: keep ownership or choose acknowledged replay after checking side effects. Inspect the scoped run summary for the exact owner and grant digest; use <code>relish batch replay</code>.</td></tr>");
        }
    }
    if rows.is_empty() {
        html.push_str("<tr><td colspan=\"8\">No retained batches</td></tr>");
    }
    html.push_str("</tbody></table>");
    html
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scheduled_definitions_show_a_schedule_without_a_fabricated_run_zero() {
        let html = render_batches(&[
            serde_json::json!({"kind":"schedule","name":"nightly","namespace":"default","status":"Scheduled","cron":{"expression":"0 3 * * *"},"total":1}),
        ]);
        assert!(html.contains("0 3 * * *"));
        assert!(!html.contains("0 nightly"));
    }

    #[test]
    fn a_schedule_shows_its_last_outcomes_newest_first() {
        let html = render_batches(&[serde_json::json!({
            "kind":"schedule","name":"nightly","namespace":"ops","status":"Scheduled",
            "cron":{"expression":"0 3 * * *"},"total":1,
            "history":[
                {"run":40,"status":"Succeeded","succeeded":1,"failed":0},
                {"run":41,"status":"CompletedWithFailures","succeeded":0,"failed":1},
            ],
        })]);
        let newest = html.find("run 41: CompletedWithFailures (0 succeeded, 1 failed)");
        let older = html.find("run 40: Succeeded");
        assert!(newest.is_some() && older.is_some(), "{html}");
        assert!(newest < older, "newest first: {html}");
    }

    #[test]
    fn unknown_run_summaries_explain_the_required_operator_decision() {
        let html = render_batches(&[
            serde_json::json!({"batch_id":1,"name":"migrate","status":"Unknown","unknown_owners":[{"node":"lost","grant_digest":"digest"}],"total":1}),
        ]);
        assert!(html.contains("acknowledged replay"));
        assert!(html.contains("relish batch replay"));
    }

    #[test]
    fn render_empty_dashboard() {
        let data = DashboardData::default();
        let html = render_dashboard(&data);
        assert!(html.contains("<title>Reliaburger</title>"));
        assert!(html.contains("no workloads running"));
        assert!(html.contains("none active"));
    }

    #[test]
    fn the_summary_says_partial_rather_than_a_smaller_number() {
        let mut summary = summary_fixture();
        summary.nodes = crate::bun::summary::Section::from_nodes(
            crate::bun::summary::NodesSection {
                alive: 2,
                ..Default::default()
            },
            crate::bun::summary::Coverage {
                answered: 2,
                asked: 3,
            },
            &["node-3: timed out".to_string()],
        );
        let html = render_summary_fragment(Some(&summary));
        assert!(html.contains("badge-amber\">DEGRADED"), "{html}");
        assert!(
            html.contains("<tr class=\"alert-warning\"><td>nodes</td><td>2 alive, 0 suspect, 0 dead; partial (2 of 3 nodes)</td><td><code>relish nodes --unhealthy</code></td></tr>"),
            "{html}"
        );
        assert!(
            !html.contains("<td>wtf</td>"),
            "Brioche runs no checks: {html}"
        );
    }

    #[test]
    fn a_healthy_summary_is_quiet_and_text_is_escaped() {
        let mut summary = summary_fixture();
        summary.answered_by = "node<1>".to_string();
        let html = render_summary_fragment(Some(&summary));
        assert!(
            html.contains("badge-green\">HEALTHY</span> no problems"),
            "{html}"
        );
        assert!(html.contains("node&lt;1&gt;"), "{html}");
        assert!(!html.contains("alert-warning"), "{html}");
    }

    /// A proposal waiting for a person is worth a look, not a problem; one
    /// that can't apply any more is.
    #[test]
    fn the_cluster_card_shows_proposals_waiting_and_stale_ones_degrade() {
        use crate::bun::summary::{AgentsSection, ProposalBrief, Section};
        let brief = |stale: Option<&str>| ProposalBrief {
            id: 17,
            kind: "scale".to_string(),
            author: "holmes".to_string(),
            namespace: "shop".to_string(),
            apps: vec!["web".to_string()],
            created_at_unix_ms: 1_800_000_000_000,
            stale: stale.map(str::to_string),
        };
        let mut summary = summary_fixture();
        summary.agents = Section::complete(AgentsSection {
            waiting: 1,
            first_proposals: vec![brief(None)],
            ..AgentsSection::default()
        });
        let html = render_summary_fragment(Some(&summary));
        assert!(html.contains("badge-green\">HEALTHY"), "{html}");
        assert!(
            html.contains("<tr class=\"alert-warning\"><td>agents</td><td><a class=\"row-link\" href=\"/ui/proposals\">1 proposal waiting for approval (#17 by holmes)</a></td><td><code>relish proposal list</code></td></tr>"),
            "{html}"
        );
        summary.agents = Section::complete(AgentsSection {
            waiting: 1,
            stale: 1,
            first_proposals: vec![brief(Some("shop/web changed after it was made"))],
            ..AgentsSection::default()
        });
        let html = render_summary_fragment(Some(&summary));
        assert!(html.contains("badge-amber\">DEGRADED"), "{html}");
    }

    #[test]
    fn an_app_list_that_could_not_be_read_says_why() {
        let html = render_dashboard(&DashboardData {
            apps_error: Some("node-3 did not answer".to_string()),
            ..Default::default()
        });
        assert!(
            html.contains("Couldn't list every app: node-3 did not answer"),
            "{html}"
        );
    }

    #[test]
    fn no_summary_says_so_rather_than_showing_zeros() {
        let html = render_dashboard(&DashboardData::default());
        assert!(html.contains("couldn't be read"), "{html}");
    }

    /// A healthy three-node summary.
    fn summary_fixture() -> ClusterSummary {
        use crate::bun::summary::*;
        ClusterSummary {
            generated_at_unix_ms: 1_800_000_000_000,
            answered_by: "node-1".to_string(),
            version: "0.2.8".to_string(),
            scoped: false,
            nodes: Section::complete(NodesSection {
                alive: 3,
                ..Default::default()
            }),
            council: Section::complete(CouncilSection {
                leader: Some("node-1".to_string()),
                term: 2,
                voters: 3,
                voters_answering: 3,
                quorum_ok: true,
                lagging: 0,
                other_leaders: Vec::new(),
            }),
            gossip: Section::complete(GossipSection {
                members: 3,
                updates_received: 10,
                last_change_unix_ms: None,
            }),
            versions: Section::complete(VersionsSection {
                versions: vec![VersionCount {
                    version: "0.2.8".to_string(),
                    nodes: 3,
                }],
                upgrade_in_progress: false,
            }),
            capacity: Section::complete(Default::default()),
            usage: Section::complete(UsageSection {
                cpu_percent: None,
                memory_percent: None,
                disk_percent: None,
                busiest: Vec::new(),
                newest_sample_unix_ms: None,
            }),
            apps: Section::complete(Default::default()),
            instances: Section::complete(Default::default()),
            jobs: Section::complete(Default::default()),
            logs: Section::complete(LogsSection {
                newest_unix_ms: None,
                slowest: None,
            }),
            faults: Section::complete(Default::default()),
            alerts: Section::complete(Default::default()),
            certs: Section::complete(CertsSection {
                count: 0,
                soonest: None,
            }),
            gitops: Section::complete(GitOpsSection {
                configured: false,
                commit: None,
                synced_at_unix_seconds: None,
                error: None,
            }),
            agents: Section::complete(AgentsSection::default()),
        }
    }

    #[test]
    fn render_with_apps() {
        let data = DashboardData {
            apps: vec![
                DashboardApp {
                    name: "web".to_string(),
                    namespace: "default".to_string(),
                    instances_running: 3,
                    instances_desired: 3,
                    state: "running".to_string(),
                },
                DashboardApp {
                    name: "api".to_string(),
                    namespace: "prod".to_string(),
                    instances_running: 1,
                    instances_desired: 2,
                    state: "pending".to_string(),
                },
            ],
            ..Default::default()
        };
        let html = render_dashboard(&data);
        assert!(html.contains("web"));
        assert!(html.contains("api"));
        assert!(html.contains("3/3"));
        assert!(html.contains("1/2"));
    }

    #[test]
    fn render_with_nodes() {
        let data = DashboardData {
            nodes: vec![DashboardNode {
                name: "node-01".to_string(),
                state: "alive".to_string(),
                app_count: 4,
            }],
            ..Default::default()
        };
        let html = render_dashboard(&data);
        assert!(html.contains("node-01"));
        assert!(html.contains("alive"));
    }

    #[test]
    fn render_with_alerts() {
        let data = DashboardData {
            alerts: vec![DashboardAlert {
                labels: std::collections::BTreeMap::from([("node".into(), "hot-<a>".into())]),
                name: "cpu_throttle".to_string(),
                severity: "Critical".to_string(),
                description: "CPU above 90%".to_string(),
            }],
            ..Default::default()
        };
        let html = render_dashboard(&data);
        assert!(html.contains("cpu_throttle"));
        assert!(html.contains("hot-&lt;a&gt;"));
        assert!(!html.contains("hot-<a>"));
        assert!(html.contains("Critical"));
        assert!(!html.contains("none active"));
    }

    #[test]
    fn dashboard_has_htmx_polling() {
        let html = render_dashboard(&DashboardData::default());
        assert!(html.contains("hx-get="));
        assert!(html.contains("hx-trigger=\"every 5s\""));
        assert!(html.contains("hx-trigger=\"every 3s\""));
    }

    #[test]
    fn dashboard_includes_scripts() {
        let html = render_dashboard(&DashboardData::default());
        assert!(html.contains("/ui/static/htmx.min.js"));
        assert!(html.contains("/ui/static/uplot.min.js"));
        assert!(html.contains("/ui/static/brioche.js"));
        assert!(html.contains("/ui/static/brioche.css"));
    }

    #[test]
    fn dashboard_apps_link_to_detail() {
        let data = DashboardData {
            apps: vec![DashboardApp {
                name: "web".to_string(),
                namespace: "default".to_string(),
                instances_running: 1,
                instances_desired: 1,
                state: "running".to_string(),
            }],
            ..Default::default()
        };
        let html = render_dashboard(&data);
        assert!(html.contains("/ui/app/web/default"));
    }

    #[test]
    fn dashboard_nodes_link_to_detail() {
        let data = DashboardData {
            nodes: vec![DashboardNode {
                name: "node-01".to_string(),
                state: "alive".to_string(),
                app_count: 3,
            }],
            ..Default::default()
        };
        let html = render_dashboard(&data);
        assert!(html.contains("/ui/node/node-01"));
    }

    #[test]
    fn escape_html_works() {
        assert_eq!(escape_html("<script>"), "&lt;script&gt;");
        assert_eq!(escape_html("a&b"), "a&amp;b");
    }

    #[test]
    fn escape_html_escapes_the_apostrophe_for_single_quoted_attributes() {
        // AUTH8: a value carrying `'` must not break out of a single-quoted
        // attribute like `data-chart-config='...'`.
        let hostile = "x' onload='alert(1)";
        let escaped = escape_html(hostile);
        assert!(
            !escaped.contains('\''),
            "raw apostrophe survived: {escaped}"
        );
        assert!(escaped.contains("&#39;"));
    }

    #[test]
    fn escape_html_leaves_ordinary_values_readable() {
        // A normal value round-trips unchanged (no over-escaping).
        assert_eq!(escape_html("web-frontend"), "web-frontend");
        assert_eq!(escape_html("prod_namespace"), "prod_namespace");
    }

    #[test]
    fn app_states_get_the_dots_their_words_deserve() {
        assert!(status_dot("down").contains("dot-red"));
        assert!(status_dot("degraded").contains("dot-amber"));
        assert!(status_dot("troubled").contains("dot-amber"));
        assert!(status_dot("stopped").contains("dot-grey"));
        assert!(status_dot("unknown").contains("dot-grey"));
    }

    #[test]
    fn status_dot_colours() {
        assert!(status_dot("running").contains("dot-green"));
        assert!(status_dot("pending").contains("dot-amber"));
        assert!(status_dot("blocked").contains("dot-red"));
        assert!(status_dot("failed").contains("dot-red"));
        assert!(status_dot("unknown").contains("dot-grey"));
    }

    #[test]
    fn dashboard_data_serialises() {
        let data = DashboardData {
            cluster_name: "prod".to_string(),
            ..Default::default()
        };
        let json = serde_json::to_string(&data).unwrap();
        assert!(json.contains("\"cluster_name\":\"prod\""));
        assert!(json.contains("\"summary\":null"));
    }

    #[test]
    fn render_with_cluster_name() {
        let data = DashboardData {
            cluster_name: "production".to_string(),
            ..Default::default()
        };
        let html = render_dashboard(&data);
        assert!(html.contains("production"));
    }
}
