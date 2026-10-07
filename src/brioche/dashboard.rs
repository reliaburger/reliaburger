//! Dashboard rendering.
//!
//! Produces a complete HTML page showing cluster overview: apps, nodes,
//! and alerts. Uses HTMX for automatic partial-page refreshes instead
//! of full-page reloads. Charts are initialised client-side by uPlot
//! via `data-chart-config` attributes.

use serde::{Deserialize, Serialize};

use super::fragments::{
    render_alerts_table_fragment, render_apps_table_fragment, render_nodes_table_fragment,
};

/// Data backing the dashboard.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DashboardData {
    pub cluster_name: String,
    pub node_count: usize,
    pub app_count: usize,
    pub alert_count: usize,
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

    html.push_str("<div class=\"summary\">\n");
    html.push_str(&format!(
        "<div class=\"stat\"><span class=\"num\">{}</span><span class=\"label\">Nodes</span></div>\n",
        data.node_count
    ));
    html.push_str(&format!(
        "<div class=\"stat\"><span class=\"num\">{}</span><span class=\"label\">Apps</span></div>\n",
        data.app_count
    ));

    let alert_class = if data.alert_count > 0 {
        "num alert"
    } else {
        "num"
    };
    html.push_str(&format!(
        "<div class=\"stat\"><span class=\"{alert_class}\">{}</span><span class=\"label\">Alerts</span></div>\n",
        data.alert_count
    ));
    html.push_str("</div>\n");

    // Apps table (HTMX-polled)
    html.push_str("<section>\n<h2>Apps</h2>\n");
    html.push_str(
        "<div hx-get=\"/ui/fragment/apps\" hx-trigger=\"every 5s\" hx-swap=\"innerHTML\">\n",
    );
    html.push_str(&render_apps_table_fragment(&data.apps));
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

/// Return a coloured dot span based on state.
pub fn status_dot(state: &str) -> &'static str {
    match state.to_lowercase().as_str() {
        "running" | "alive" | "healthy" => "<span class=\"dot-green\">●</span>",
        "pending" | "preparing" => "<span class=\"dot-amber\">●</span>",
        "failed" | "unhealthy" | "dead" | "blocked" => "<span class=\"dot-red\">●</span>",
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
            html.push_str(&format!("<tr><td>{name}</td><td>{ns}</td><td>{status}</td><td colspan=\"5\">UTC schedule: {expression}; {} tasks per occurrence</td></tr>", row["total"]));
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
    fn render_with_apps() {
        let data = DashboardData {
            app_count: 2,
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
            node_count: 2,
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
            alert_count: 1,
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
            app_count: 1,
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
            node_count: 1,
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
            node_count: 3,
            app_count: 5,
            ..Default::default()
        };
        let json = serde_json::to_string(&data).unwrap();
        assert!(json.contains("\"cluster_name\":\"prod\""));
        assert!(json.contains("\"node_count\":3"));
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
