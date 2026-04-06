//! Dashboard rendering.
//!
//! Produces a complete HTML page showing cluster overview: apps, nodes,
//! and alerts. No templates — just format strings. Total output is
//! under 10KB including embedded CSS.

use serde::{Deserialize, Serialize};

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
pub fn render_dashboard(data: &DashboardData) -> String {
    let mut html = String::with_capacity(8192);

    html.push_str(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta http-equiv="refresh" content="5">
<title>Reliaburger</title>
<style>
"#,
    );
    html.push_str(DASHBOARD_CSS);
    html.push_str(
        r#"</style>
</head>
<body>
<header>
<h1>Reliaburger Dashboard</h1>
"#,
    );

    if !data.cluster_name.is_empty() {
        html.push_str(&format!(
            "<span class=\"cluster\">{}</span>\n",
            escape_html(&data.cluster_name)
        ));
    }

    html.push_str("</header>\n<div class=\"summary\">\n");
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

    // Apps table
    html.push_str("<section>\n<h2>Apps</h2>\n");
    if data.apps.is_empty() {
        html.push_str("<p class=\"empty\">no workloads running</p>\n");
    } else {
        html.push_str(
            "<table>\n<tr><th>Name</th><th>Namespace</th><th>Status</th><th>Instances</th></tr>\n",
        );
        for app in &data.apps {
            let dot = status_dot(&app.state);
            html.push_str(&format!(
                "<tr><td>{}</td><td>{}</td><td>{dot} {}</td><td>{}/{}</td></tr>\n",
                escape_html(&app.name),
                escape_html(&app.namespace),
                escape_html(&app.state),
                app.instances_running,
                app.instances_desired,
            ));
        }
        html.push_str("</table>\n");
    }
    html.push_str("</section>\n");

    // Nodes table
    html.push_str("<section>\n<h2>Nodes</h2>\n");
    if data.nodes.is_empty() {
        html.push_str("<p class=\"empty\">single-node mode</p>\n");
    } else {
        html.push_str("<table>\n<tr><th>Name</th><th>State</th><th>Apps</th></tr>\n");
        for node in &data.nodes {
            let dot = status_dot(&node.state);
            html.push_str(&format!(
                "<tr><td>{}</td><td>{dot} {}</td><td>{}</td></tr>\n",
                escape_html(&node.name),
                escape_html(&node.state),
                node.app_count,
            ));
        }
        html.push_str("</table>\n");
    }
    html.push_str("</section>\n");

    // Alerts section
    html.push_str("<section>\n<h2>Alerts</h2>\n");
    if data.alerts.is_empty() {
        html.push_str("<p class=\"empty\">none active</p>\n");
    } else {
        html.push_str("<table>\n<tr><th>Name</th><th>Severity</th><th>Description</th></tr>\n");
        for alert in &data.alerts {
            html.push_str(&format!(
                "<tr class=\"alert-{}\"><td>{}</td><td>{}</td><td>{}</td></tr>\n",
                escape_html(&alert.severity.to_lowercase()),
                escape_html(&alert.name),
                escape_html(&alert.severity),
                escape_html(&alert.description),
            ));
        }
        html.push_str("</table>\n");
    }
    html.push_str("</section>\n");

    html.push_str("</body>\n</html>\n");
    html
}

/// Minimal CSS for the dashboard (dark theme).
const DASHBOARD_CSS: &str = r#"
* { margin: 0; padding: 0; box-sizing: border-box; }
body { background: #1a1a2e; color: #e0e0e0; font-family: -apple-system, 'Segoe UI', Roboto, monospace; padding: 1rem; }
header { display: flex; align-items: center; gap: 1rem; margin-bottom: 1.5rem; }
h1 { font-size: 1.4rem; color: #fff; }
.cluster { background: #16213e; padding: 0.2rem 0.6rem; border-radius: 4px; font-size: 0.85rem; }
.summary { display: flex; gap: 2rem; margin-bottom: 1.5rem; }
.stat { text-align: center; }
.num { display: block; font-size: 2rem; font-weight: bold; color: #4ecca3; }
.num.alert { color: #e74c3c; }
.label { font-size: 0.8rem; color: #888; text-transform: uppercase; }
section { margin-bottom: 1.5rem; }
h2 { font-size: 1.1rem; color: #ccc; margin-bottom: 0.5rem; border-bottom: 1px solid #333; padding-bottom: 0.3rem; }
table { width: 100%; border-collapse: collapse; font-size: 0.9rem; }
th { text-align: left; padding: 0.4rem 0.6rem; color: #888; font-weight: normal; text-transform: uppercase; font-size: 0.75rem; }
td { padding: 0.4rem 0.6rem; border-top: 1px solid #2a2a3e; }
tr:hover td { background: #16213e; }
.empty { color: #666; font-style: italic; padding: 0.5rem 0; }
.dot-green { color: #4ecca3; }
.dot-amber { color: #f39c12; }
.dot-red { color: #e74c3c; }
.dot-grey { color: #666; }
.alert-critical td { color: #e74c3c; }
.alert-warning td { color: #f39c12; }
"#;

/// Return a coloured dot span based on state.
fn status_dot(state: &str) -> &'static str {
    match state.to_lowercase().as_str() {
        "running" | "alive" | "healthy" => "<span class=\"dot-green\">●</span>",
        "pending" | "preparing" => "<span class=\"dot-amber\">●</span>",
        "failed" | "unhealthy" | "dead" => "<span class=\"dot-red\">●</span>",
        _ => "<span class=\"dot-grey\">●</span>",
    }
}

/// Escape HTML special characters.
fn escape_html(s: &str) -> String {
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
    fn dashboard_has_auto_refresh() {
        let html = render_dashboard(&DashboardData::default());
        assert!(html.contains("http-equiv=\"refresh\""));
        assert!(html.contains("content=\"5\""));
    }

    #[test]
    fn dashboard_has_css() {
        let html = render_dashboard(&DashboardData::default());
        assert!(html.contains("<style>"));
        assert!(html.contains("background:"));
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
