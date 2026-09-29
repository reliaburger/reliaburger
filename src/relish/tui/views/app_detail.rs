use ratatui::text::Line;

use crate::relish::output::sparkline;
use crate::relish::tui::app::{DetailTab, TuiApp, View};

use super::widgets;

pub fn lines(app: &TuiApp, height: usize) -> Vec<Line<'static>> {
    let View::AppDetail {
        app: name,
        namespace,
        tab,
    } = app.view()
    else {
        return Vec::new();
    };
    let tabs = DetailTab::ALL
        .iter()
        .map(|item| {
            if item == tab {
                format!("[{item:?}]")
            } else {
                format!(" {item:?} ")
            }
        })
        .collect::<Vec<_>>()
        .join(" │ ");
    let mut lines = vec![
        widgets::heading(format!("{name} · {namespace}")),
        Line::raw(tabs),
        Line::raw(""),
    ];
    let instances: Vec<_> = app
        .data
        .instances
        .iter()
        .filter(|instance| {
            &instance.instance.app_name == name && &instance.instance.namespace == namespace
        })
        .collect();
    match tab {
        DetailTab::Overview => {
            let ready = instances
                .iter()
                .filter(|instance| instance.instance.state == "running")
                .count();
            let restarts: u32 = instances
                .iter()
                .map(|instance| instance.instance.restart_count)
                .sum();
            lines.extend([
                Line::raw(format!("ready          {ready}/{}", instances.len())),
                Line::raw(format!("restarts       {restarts}")),
                Line::raw("image          unknown"),
            ]);
        }
        DetailTab::Instances => {
            lines.push(widgets::heading(
                "NODE         ID                       STATE         RESTARTS  HOST PORT  PID",
            ));
            if instances.is_empty() {
                lines.push(Line::raw("no instances"));
            }
            for instance in instances {
                lines.push(Line::raw(format!(
                    "{:<12} {:<24} {:<13} {:>8}  {:>9}  {}",
                    instance.node,
                    instance.instance.id,
                    instance.instance.state,
                    instance.instance.restart_count,
                    instance
                        .instance
                        .host_port
                        .map(|port| port.to_string())
                        .unwrap_or_else(|| "-".into()),
                    instance
                        .instance
                        .pid
                        .map(|pid| pid.to_string())
                        .unwrap_or_else(|| "-".into())
                )));
            }
        }
        DetailTab::Logs => {
            lines.push(Line::raw("live logs from the connected node"));
            if let Some(error) = &app.log_stream_down {
                lines.push(Line::raw(format!(
                    "stream disconnected — reconnecting: {error}"
                )));
            }
            if app.log_lines.is_empty() {
                lines.push(Line::raw("waiting for logs…"));
            }
            let available = height.saturating_sub(lines.len());
            let scroll = app
                .log_scroll
                .min(app.log_lines.len().saturating_sub(available));
            for line in app
                .log_lines
                .iter()
                .rev()
                .skip(scroll)
                .take(available)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
            {
                lines.push(Line::raw(format!("{} │ {}", line.instance, line.line)));
            }
        }
        DetailTab::Metrics => {
            let Some(metrics) = &app.data.app_metrics else {
                lines.push(Line::raw("no metrics recorded"));
                return lines;
            };
            if metrics.data.is_empty() {
                lines.push(Line::raw("no metrics recorded"));
            }
            let cpu: Vec<f64> = metrics
                .data
                .iter()
                .filter(|row| row.metric_name.to_lowercase().contains("cpu"))
                .map(|row| row.value)
                .collect();
            let memory: Vec<f64> = metrics
                .data
                .iter()
                .filter(|row| row.metric_name.to_lowercase().contains("memory"))
                .map(|row| row.value)
                .collect();
            if !cpu.is_empty() {
                lines.push(Line::raw(format!("CPU     {}", sparkline(&cpu))));
            }
            if !memory.is_empty() {
                lines.push(Line::raw(format!("Memory  {}", sparkline(&memory))));
            }
            for row in metrics.data.iter().rev().take(20).rev() {
                lines.push(Line::raw(format!(
                    "{} {:<32} {:>10.2}",
                    widgets::format_time(row.timestamp),
                    row.metric_name,
                    row.value
                )));
            }
            for warning in &metrics.warnings {
                lines.push(Line::raw(format!("warning: {warning:?}")));
            }
        }
        DetailTab::Deploys => {
            lines.push(Line::raw("deployment history from the connected node"));
            let history =
                app.data
                    .deploy_history
                    .get(&crate::relish::tui::state::deploy_history_key(
                        name, namespace,
                    ));
            if history.is_none_or(Vec::is_empty) {
                lines.push(Line::raw("no deploy history"));
            } else if let Some(history) = history {
                for entry in history {
                    lines.push(Line::raw(entry.to_string()));
                }
            }
        }
        DetailTab::Config => {
            lines.push(Line::raw("full resolved config is not exposed by the API"));
            lines.push(Line::raw(format!("instances      {}", instances.len())));
            let ports: Vec<_> = instances
                .iter()
                .filter_map(|instance| instance.instance.host_port)
                .collect();
            lines.push(Line::raw(format!("host ports     {ports:?}")));
        }
    }
    lines
}
