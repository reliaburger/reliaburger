//! App detail page rendering.
//!
//! Produces a complete HTML page for a single application, showing
//! instance table, resource charts, streaming logs, deploy history,
//! and environment variables (with encrypted values masked).

use super::dashboard::escape_html;
use super::fragments::render_instance_table_fragment;
use super::types::{AppDetailData, ChartConfig, SafeEnvValue};

/// Shared HTML head included by all Brioche pages.
pub(crate) fn render_head(title: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title} — Reliaburger</title>
<link rel="stylesheet" href="/ui/static/uplot.min.css">
<link rel="stylesheet" href="/ui/static/brioche.css">
<script src="/ui/static/htmx.min.js"></script>
<script src="/ui/static/uplot.min.js"></script>
<script src="/ui/static/brioche.js"></script>
</head>
<body>
"#,
        title = escape_html(title)
    )
}

/// Render the nav bar.
pub(crate) fn render_nav() -> &'static str {
    r#"<nav>
<span class="brand">Reliaburger</span>
<a href="/">Dashboard</a>
</nav>
"#
}

/// Render the app detail page as a complete HTML page.
pub fn render_app_detail(data: &AppDetailData) -> String {
    let mut html = String::with_capacity(8192);

    html.push_str(&render_head(&data.app_name));
    html.push_str(render_nav());

    // Header
    html.push_str("<div class=\"detail-header\">\n");
    html.push_str(&format!("<h1>{}</h1>\n", escape_html(&data.app_name)));
    html.push_str(&format!(
        "<div class=\"detail-meta\">\
         <span>Namespace: <strong>{}</strong></span>\
         <span>Status: <strong>{}</strong></span>\
         <span>Instances: <strong>{}/{}</strong></span>\
         </div>\n",
        escape_html(&data.namespace),
        escape_html(&data.state),
        data.instances
            .iter()
            .filter(|i| i.state == "running")
            .count(),
        data.instances.len(),
    ));
    html.push_str("</div>\n");

    // Charts
    if !data.charts.is_empty() {
        html.push_str("<section>\n<h2>Resource Usage</h2>\n<div class=\"charts-row\">\n");
        for chart in &data.charts {
            render_chart_container(&mut html, chart);
        }
        html.push_str("</div>\n</section>\n");
    }

    // Instance table (HTMX-polled)
    html.push_str("<section>\n<h2>Instances</h2>\n");
    html.push_str(&format!(
        "<div hx-get=\"/ui/fragment/app/{}/{}/instances\" hx-trigger=\"every 5s\" hx-swap=\"innerHTML\">\n",
        escape_html(&data.app_name),
        escape_html(&data.namespace),
    ));
    html.push_str(&render_instance_table_fragment(&data.instances));
    html.push_str("</div>\n</section>\n");

    // Streaming logs
    html.push_str("<section>\n<h2>Logs</h2>\n");
    html.push_str(&format!(
        "<div class=\"log-viewer\" data-log-stream=\"/v1/logs/{}/{}?follow=true\"></div>\n",
        escape_html(&data.app_name),
        escape_html(&data.namespace),
    ));
    html.push_str("</section>\n");

    // Deploy history
    html.push_str("<section>\n<h2>Deploy History</h2>\n");
    if data.deploy_history.is_empty() {
        html.push_str("<p class=\"empty\">no deploys</p>\n");
    } else {
        html.push_str("<table>\n<tr><th>Image</th><th>Result</th><th>Steps</th></tr>\n");
        for entry in &data.deploy_history {
            html.push_str(&format!(
                "<tr><td>{}</td><td>{:?}</td><td>{}/{}</td></tr>\n",
                escape_html(&entry.image),
                entry.result,
                entry.steps_completed,
                entry.steps_total,
            ));
        }
        html.push_str("</table>\n");
    }
    html.push_str("</section>\n");

    // Environment variables
    html.push_str("<section>\n<h2>Environment</h2>\n");
    render_env_table(&mut html, &data.env);
    html.push_str("</section>\n");

    html.push_str("</body>\n</html>\n");
    html
}

/// Render a chart container with `data-chart-config` for client-side init.
fn render_chart_container(html: &mut String, chart: &ChartConfig) {
    let json = serde_json::to_string(chart).unwrap_or_default();
    html.push_str(&format!(
        "<div class=\"chart-container\">\
         <h3>{}</h3>\
         <div data-chart-config='{}'></div>\
         </div>\n",
        escape_html(&chart.title),
        escape_html(&json),
    ));
}

/// Render the environment variables table with encrypted values masked.
fn render_env_table(html: &mut String, env: &[SafeEnvValue]) {
    if env.is_empty() {
        html.push_str("<p class=\"empty\">no environment variables</p>\n");
        return;
    }
    html.push_str("<table>\n<tr><th>Variable</th><th>Value</th></tr>\n");
    for entry in env {
        let cls = if entry.encrypted {
            " class=\"env-encrypted\""
        } else {
            ""
        };
        html.push_str(&format!(
            "<tr><td>{}</td><td{}>{}</td></tr>\n",
            escape_html(&entry.key),
            cls,
            escape_html(&entry.value),
        ));
    }
    html.push_str("</table>\n");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bun::agent::InstanceStatus;

    fn sample_data() -> AppDetailData {
        AppDetailData {
            app_name: "web".to_string(),
            namespace: "default".to_string(),
            state: "running".to_string(),
            instances: vec![
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
                    restart_count: 0,
                    host_port: Some(8081),
                    pid: Some(1235),
                },
            ],
            env: vec![
                SafeEnvValue {
                    key: "NODE_ENV".to_string(),
                    value: "production".to_string(),
                    encrypted: false,
                },
                SafeEnvValue {
                    key: "DB_URL".to_string(),
                    value: "[encrypted]".to_string(),
                    encrypted: true,
                },
            ],
            deploy_history: vec![],
            charts: vec![ChartConfig {
                endpoint: "/v1/metrics/app/web/default?name=process_cpu_percent".to_string(),
                title: "CPU Usage".to_string(),
                y_label: "%".to_string(),
                refresh_secs: 10,
                range_secs: 3600,
            }],
        }
    }

    #[test]
    fn render_app_detail_with_instances() {
        let data = sample_data();
        let html = render_app_detail(&data);
        assert!(html.contains("web-1"));
        assert!(html.contains("web-2"));
        // Two instance rows + 1 header row
        assert_eq!(html.matches("<tr><td>web-").count(), 2);
    }

    #[test]
    fn render_app_detail_escapes_html() {
        let mut data = sample_data();
        data.app_name = "<script>alert(1)</script>".to_string();
        let html = render_app_detail(&data);
        assert!(!html.contains("<script>alert(1)</script>"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn render_app_detail_masks_encrypted_env() {
        let data = sample_data();
        let html = render_app_detail(&data);
        assert!(html.contains("[encrypted]"));
        assert!(!html.contains("ENC[AGE:"));
        assert!(html.contains("production"));
    }

    #[test]
    fn render_app_detail_has_htmx_polling() {
        let data = sample_data();
        let html = render_app_detail(&data);
        assert!(html.contains("hx-get="));
        assert!(html.contains("hx-trigger=\"every 5s\""));
    }

    #[test]
    fn render_app_detail_has_chart_config() {
        let data = sample_data();
        let html = render_app_detail(&data);
        assert!(html.contains("data-chart-config="));
        assert!(html.contains("CPU Usage"));
    }

    #[test]
    fn render_app_detail_has_log_stream() {
        let data = sample_data();
        let html = render_app_detail(&data);
        assert!(html.contains("data-log-stream="));
        assert!(html.contains("/v1/logs/web/default"));
    }

    #[test]
    fn render_app_detail_includes_scripts() {
        let data = sample_data();
        let html = render_app_detail(&data);
        assert!(html.contains("/ui/static/htmx.min.js"));
        assert!(html.contains("/ui/static/uplot.min.js"));
        assert!(html.contains("/ui/static/brioche.js"));
    }
}
