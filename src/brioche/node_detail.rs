//! Node detail page rendering.
//!
//! Produces a complete HTML page for a single cluster node, showing
//! running apps, resource charts, and gossip peer status.

use super::app_detail::{render_head, render_nav};
use super::dashboard::{escape_html, status_dot};
use super::types::NodeDetailData;

/// Render the node detail page as a complete HTML page.
pub fn render_node_detail(data: &NodeDetailData) -> String {
    let mut html = String::with_capacity(4096);

    html.push_str(&render_head(&data.name));
    html.push_str(render_nav());

    // Header
    html.push_str("<div class=\"detail-header\">\n");
    html.push_str(&format!("<h1>{}</h1>\n", escape_html(&data.name)));
    let dot = status_dot(&data.state);
    html.push_str(&format!(
        "<div class=\"detail-meta\">\
         <span>State: {dot} <strong>{}</strong></span>\
         <span>Apps: <strong>{}</strong></span>\
         </div>\n",
        escape_html(&data.state),
        data.app_count,
    ));
    html.push_str("</div>\n");

    // Charts
    if !data.charts.is_empty() {
        html.push_str("<section>\n<h2>Resource Usage</h2>\n<div class=\"charts-row\">\n");
        for chart in &data.charts {
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
        html.push_str("</div>\n</section>\n");
    }

    // Running apps table
    html.push_str("<section>\n<h2>Running Apps</h2>\n");
    if data.apps.is_empty() {
        html.push_str("<p class=\"empty\">no apps on this node</p>\n");
    } else {
        html.push_str(
            "<table>\n<tr><th>App</th><th>Instance</th><th>State</th><th>Restarts</th></tr>\n",
        );
        for inst in &data.apps {
            let dot = status_dot(&inst.state);
            html.push_str(&format!(
                "<tr><td><a class=\"row-link\" href=\"/ui/app/{}/{}\">{}</a></td>\
                 <td>{}</td><td>{dot} {}</td><td>{}</td></tr>\n",
                escape_html(&inst.app_name),
                escape_html(&inst.namespace),
                escape_html(&inst.app_name),
                escape_html(&inst.id),
                escape_html(&inst.state),
                inst.restart_count,
            ));
        }
        html.push_str("</table>\n");
    }
    html.push_str("</section>\n");

    html.push_str("</body>\n</html>\n");
    html
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::brioche::types::ChartConfig;
    use crate::bun::agent::InstanceStatus;

    fn sample_data() -> NodeDetailData {
        NodeDetailData {
            name: "node-01".to_string(),
            state: "alive".to_string(),
            app_count: 2,
            apps: vec![
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
                    id: "api-1".to_string(),
                    app_name: "api".to_string(),
                    namespace: "prod".to_string(),
                    state: "running".to_string(),
                    restart_count: 1,
                    host_port: Some(9090),
                    pid: Some(5678),
                },
            ],
            charts: vec![ChartConfig {
                endpoint: "/v1/metrics?name=node_cpu_usage_percent".to_string(),
                title: "CPU Usage".to_string(),
                y_label: "%".to_string(),
                refresh_secs: 10,
                range_secs: 3600,
            }],
        }
    }

    #[test]
    fn render_node_detail_with_apps() {
        let data = sample_data();
        let html = render_node_detail(&data);
        assert!(html.contains("web"));
        assert!(html.contains("api"));
        assert!(html.contains("node-01"));
        assert!(html.contains("alive"));
    }

    #[test]
    fn render_node_detail_escapes_html() {
        let mut data = sample_data();
        data.name = "<script>xss</script>".to_string();
        let html = render_node_detail(&data);
        assert!(!html.contains("<script>xss</script>"));
        assert!(html.contains("&lt;script&gt;"));
    }

    #[test]
    fn render_node_detail_has_chart_config() {
        let data = sample_data();
        let html = render_node_detail(&data);
        assert!(html.contains("data-chart-config="));
        assert!(html.contains("node_cpu_usage_percent"));
    }

    #[test]
    fn render_node_detail_links_to_app_detail() {
        let data = sample_data();
        let html = render_node_detail(&data);
        assert!(html.contains("/ui/app/web/default"));
        assert!(html.contains("/ui/app/api/prod"));
    }

    #[test]
    fn render_node_detail_empty_apps() {
        let data = NodeDetailData {
            name: "node-02".to_string(),
            state: "alive".to_string(),
            app_count: 0,
            apps: vec![],
            charts: vec![],
        };
        let html = render_node_detail(&data);
        assert!(html.contains("no apps on this node"));
    }

    #[test]
    fn render_node_detail_includes_scripts() {
        let data = sample_data();
        let html = render_node_detail(&data);
        assert!(html.contains("/ui/static/htmx.min.js"));
        assert!(html.contains("/ui/static/uplot.min.js"));
        assert!(html.contains("/ui/static/brioche.js"));
    }
}
