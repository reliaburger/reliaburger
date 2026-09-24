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
<a href="/ui/gitops">GitOps</a>
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
        data.desired_instances,
    ));
    html.push_str("</div>\n");

    // Charts
    if !data.charts.is_empty() {
        html.push_str("<section>\n<h2>Metrics</h2>\n<div class=\"charts-row\">\n");
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

/// How far back the app page's charts reach, in seconds.
const APP_CHART_RANGE_SECS: u64 = 15 * 60;

/// Counters that belong to the runtime or client library rather than the
/// app's own work, so they never stand in for "requests".
const RUNTIME_METRIC_PREFIXES: [&str; 3] = ["go_", "process_", "promhttp_"];

/// The charts on an app's page.
///
/// CPU and memory always, one line per instance. When the app's own
/// metrics were scraped (`scraped_names`), also a requests-per-second chart
/// from its request counter and a latency chart from its duration
/// histogram, when it has them.
pub fn app_charts(app: &str, namespace: &str, scraped_names: &[String]) -> Vec<ChartConfig> {
    let chart = |name: &str, kind: &str, title: &str, y_label: &str| ChartConfig {
        endpoint: format!("/v1/metrics/app/{app}/{namespace}/chart?name={name}&kind={kind}"),
        title: title.to_string(),
        y_label: y_label.to_string(),
        refresh_secs: 10,
        range_secs: APP_CHART_RANGE_SECS,
    };
    let mut charts = vec![
        chart("process_cpu_percent", "gauge", "CPU Usage", "%"),
        chart("process_memory_bytes", "gauge", "Memory Usage", "bytes"),
    ];
    if let Some(counter) = request_counter(scraped_names) {
        charts.push(chart(counter, "rate", "Requests/s", "req/s"));
    }
    if let Some(histogram) = latency_histogram(scraped_names) {
        charts.push(chart(histogram, "mean", "Mean Latency", "seconds"));
    }
    charts
}

/// The counter that best reads as "requests served": `http_requests_total`
/// if the app has it, then any `*_requests_total`, then the first other
/// counter of the app's own.
fn request_counter(names: &[String]) -> Option<&str> {
    let own_counters = || {
        names.iter().map(String::as_str).filter(|name| {
            name.ends_with("_total")
                && !RUNTIME_METRIC_PREFIXES
                    .iter()
                    .any(|prefix| name.starts_with(prefix))
        })
    };
    own_counters()
        .find(|name| *name == "http_requests_total")
        .or_else(|| own_counters().find(|name| name.ends_with("_requests_total")))
        .or_else(|| own_counters().next())
}

/// The histogram that best reads as latency: `http_request_duration_seconds`
/// if present, else the first one whose name says duration or latency.
/// Returned by its base name; its `_sum` and `_count` must both exist.
fn latency_histogram(names: &[String]) -> Option<&str> {
    let histograms = || {
        names.iter().filter_map(|name| {
            let base = name.strip_suffix("_count")?;
            let own = !RUNTIME_METRIC_PREFIXES
                .iter()
                .any(|prefix| base.starts_with(prefix));
            let has_sum = names
                .iter()
                .any(|other| other.strip_suffix("_sum") == Some(base));
            (own && has_sum).then_some(base)
        })
    };
    histograms()
        .find(|base| *base == "http_request_duration_seconds")
        .or_else(|| histograms().find(|base| base.contains("duration") || base.contains("latency")))
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
            desired_instances: 2,
            app_name: "web".to_string(),
            namespace: "default".to_string(),
            state: "running".to_string(),
            instances: vec![
                InstanceStatus {
                    exit_code: None,
                    id: "web-1".to_string(),
                    app_name: "web".to_string(),
                    namespace: "default".to_string(),
                    state: "running".to_string(),
                    restart_count: 0,
                    host_port: Some(8080),
                    pid: Some(1234),
                },
                InstanceStatus {
                    exit_code: None,
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
    fn hostile_chart_title_cannot_break_out_of_the_single_quoted_attribute() {
        // AUTH8: a chart title carrying an apostrophe lands in a single-quoted
        // `data-chart-config='...'` attribute. It must be escaped so it can't
        // close the attribute and inject an event handler.
        let mut data = sample_data();
        data.charts[0].title = "x' onload='alert(1)".to_string();
        let html = render_app_detail(&data);
        assert!(
            !html.contains("onload='alert"),
            "attribute break-out survived: {html}"
        );
        assert!(html.contains("&#39;"));
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

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|name| name.to_string()).collect()
    }

    #[test]
    fn every_app_gets_cpu_and_memory_charts_per_instance() {
        let charts = app_charts("web", "default", &[]);
        let endpoints: Vec<&str> = charts.iter().map(|c| c.endpoint.as_str()).collect();
        assert_eq!(
            endpoints,
            vec![
                "/v1/metrics/app/web/default/chart?name=process_cpu_percent&kind=gauge",
                "/v1/metrics/app/web/default/chart?name=process_memory_bytes&kind=gauge",
            ]
        );
    }

    #[test]
    fn scraped_apps_add_requests_and_latency_charts() {
        let charts = app_charts(
            "web",
            "default",
            &names(&[
                "go_gc_duration_seconds_count",
                "go_gc_duration_seconds_sum",
                "http_request_duration_seconds_bucket",
                "http_request_duration_seconds_count",
                "http_request_duration_seconds_sum",
                "http_requests_total",
                "process_cpu_seconds_total",
                "up",
            ]),
        );
        let titles: Vec<&str> = charts.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(
            titles,
            vec!["CPU Usage", "Memory Usage", "Requests/s", "Mean Latency"]
        );
        assert!(
            charts[2]
                .endpoint
                .ends_with("name=http_requests_total&kind=rate")
        );
        assert!(
            charts[3]
                .endpoint
                .ends_with("name=http_request_duration_seconds&kind=mean")
        );
    }

    #[test]
    fn the_request_chart_falls_back_to_the_apps_own_counter() {
        let charts = app_charts(
            "worker",
            "default",
            &names(&[
                "go_goroutines",
                "process_cpu_seconds_total",
                "jobs_done_total",
            ]),
        );
        assert!(
            charts[2]
                .endpoint
                .contains("name=jobs_done_total&kind=rate")
        );
        assert_eq!(charts.len(), 3, "no histogram, no latency chart");
    }

    #[test]
    fn runtime_counters_alone_draw_no_request_chart() {
        let charts = app_charts(
            "web",
            "default",
            &names(&["go_goroutines", "process_cpu_seconds_total", "up"]),
        );
        assert_eq!(charts.len(), 2);
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
