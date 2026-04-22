//! HTMX fragment renderers.
//!
//! These functions return bare HTML (no `<html>`, `<head>`, or `<body>`
//! wrapper) suitable for HTMX `innerHTML` swaps. The cluster overview
//! page uses them internally and they are also served directly by the
//! fragment API endpoints.

use crate::bun::agent::InstanceStatus;

use super::dashboard::{DashboardAlert, DashboardApp, DashboardNode, escape_html, status_dot};

/// Render the apps table as an HTML fragment.
pub fn render_apps_table_fragment(apps: &[DashboardApp]) -> String {
    if apps.is_empty() {
        return "<p class=\"empty\">no workloads running</p>\n".to_string();
    }

    let mut html = String::with_capacity(1024);
    html.push_str(
        "<table>\n<tr><th>Name</th><th>Namespace</th><th>Status</th><th>Instances</th></tr>\n",
    );
    for app in apps {
        let dot = status_dot(&app.state);
        html.push_str(&format!(
            "<tr><td><a class=\"row-link\" href=\"/ui/app/{}/{}\">{}</a></td>\
             <td>{}</td><td>{dot} {}</td><td>{}/{}</td></tr>\n",
            escape_html(&app.name),
            escape_html(&app.namespace),
            escape_html(&app.name),
            escape_html(&app.namespace),
            escape_html(&app.state),
            app.instances_running,
            app.instances_desired,
        ));
    }
    html.push_str("</table>\n");
    html
}

/// Render the nodes table as an HTML fragment.
pub fn render_nodes_table_fragment(nodes: &[DashboardNode]) -> String {
    if nodes.is_empty() {
        return "<p class=\"empty\">single-node mode</p>\n".to_string();
    }

    let mut html = String::with_capacity(512);
    html.push_str("<table>\n<tr><th>Name</th><th>State</th><th>Apps</th></tr>\n");
    for node in nodes {
        let dot = status_dot(&node.state);
        html.push_str(&format!(
            "<tr><td><a class=\"row-link\" href=\"/ui/node/{}\">{}</a></td>\
             <td>{dot} {}</td><td>{}</td></tr>\n",
            escape_html(&node.name),
            escape_html(&node.name),
            escape_html(&node.state),
            node.app_count,
        ));
    }
    html.push_str("</table>\n");
    html
}

/// Render the alerts table as an HTML fragment.
pub fn render_alerts_table_fragment(alerts: &[DashboardAlert]) -> String {
    if alerts.is_empty() {
        return "<p class=\"empty\">none active</p>\n".to_string();
    }

    let mut html = String::with_capacity(512);
    html.push_str("<table>\n<tr><th>Name</th><th>Severity</th><th>Description</th></tr>\n");
    for alert in alerts {
        html.push_str(&format!(
            "<tr class=\"alert-{}\"><td>{}</td><td>{}</td><td>{}</td></tr>\n",
            escape_html(&alert.severity.to_lowercase()),
            escape_html(&alert.name),
            escape_html(&alert.severity),
            escape_html(&alert.description),
        ));
    }
    html.push_str("</table>\n");
    html
}

/// Render the instance table (for app detail) as an HTML fragment.
pub fn render_instance_table_fragment(instances: &[InstanceStatus]) -> String {
    if instances.is_empty() {
        return "<p class=\"empty\">no instances</p>\n".to_string();
    }

    let mut html = String::with_capacity(512);
    html.push_str(
        "<table>\n<tr><th>ID</th><th>State</th><th>Restarts</th><th>Port</th><th>PID</th></tr>\n",
    );
    for inst in instances {
        let dot = status_dot(&inst.state);
        let port = inst
            .host_port
            .map(|p| p.to_string())
            .unwrap_or_else(|| "-".to_string());
        let pid = inst
            .pid
            .map(|p| p.to_string())
            .unwrap_or_else(|| "-".to_string());
        html.push_str(&format!(
            "<tr><td>{}</td><td>{dot} {}</td><td>{}</td><td>{}</td><td>{}</td></tr>\n",
            escape_html(&inst.id),
            escape_html(&inst.state),
            inst.restart_count,
            port,
            pid,
        ));
    }
    html.push_str("</table>\n");
    html
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fragment_apps_table_no_wrapper() {
        let apps = vec![DashboardApp {
            name: "web".to_string(),
            namespace: "default".to_string(),
            instances_running: 2,
            instances_desired: 3,
            state: "running".to_string(),
        }];
        let html = render_apps_table_fragment(&apps);
        assert!(html.starts_with("<table"));
        assert!(!html.contains("<!DOCTYPE"));
        assert!(!html.contains("<html"));
    }

    #[test]
    fn fragment_apps_table_links_to_detail() {
        let apps = vec![DashboardApp {
            name: "api".to_string(),
            namespace: "prod".to_string(),
            instances_running: 1,
            instances_desired: 1,
            state: "running".to_string(),
        }];
        let html = render_apps_table_fragment(&apps);
        assert!(html.contains("/ui/app/api/prod"));
    }

    #[test]
    fn fragment_apps_table_empty() {
        let html = render_apps_table_fragment(&[]);
        assert!(html.contains("no workloads running"));
    }

    #[test]
    fn fragment_nodes_table_links_to_detail() {
        let nodes = vec![DashboardNode {
            name: "node-01".to_string(),
            state: "alive".to_string(),
            app_count: 3,
        }];
        let html = render_nodes_table_fragment(&nodes);
        assert!(html.contains("/ui/node/node-01"));
    }

    #[test]
    fn fragment_instances_renders_rows() {
        let instances = vec![
            InstanceStatus {
                id: "web-1".to_string(),
                app_name: "web".to_string(),
                namespace: "default".to_string(),
                state: "running".to_string(),
                restart_count: 0,
                host_port: Some(8080),
                pid: Some(1234),
            },
            InstanceStatus {
                id: "web-2".to_string(),
                app_name: "web".to_string(),
                namespace: "default".to_string(),
                state: "running".to_string(),
                restart_count: 1,
                host_port: None,
                pid: None,
            },
        ];
        let html = render_instance_table_fragment(&instances);
        assert!(html.contains("web-1"));
        assert!(html.contains("web-2"));
        assert!(html.contains("8080"));
        assert!(html.contains("1234"));
        // Two data rows + 1 header row = 3 <tr> tags
        assert_eq!(html.matches("<tr>").count(), 3);
    }

    #[test]
    fn fragment_instances_empty() {
        let html = render_instance_table_fragment(&[]);
        assert!(html.contains("no instances"));
    }
}
