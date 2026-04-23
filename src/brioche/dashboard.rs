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
        "failed" | "unhealthy" | "dead" => "<span class=\"dot-red\">●</span>",
        _ => "<span class=\"dot-grey\">●</span>",
    }
}

/// Escape HTML special characters.
pub fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

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
                name: "cpu_throttle".to_string(),
                severity: "Critical".to_string(),
                description: "CPU above 90%".to_string(),
            }],
            ..Default::default()
        };
        let html = render_dashboard(&data);
        assert!(html.contains("cpu_throttle"));
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
    fn status_dot_colours() {
        assert!(status_dot("running").contains("dot-green"));
        assert!(status_dot("pending").contains("dot-amber"));
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
