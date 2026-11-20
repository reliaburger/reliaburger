use std::collections::{BTreeMap, BTreeSet};

use ratatui::text::{Line, Span};

use crate::bun::agent::ClusterInstanceStatus;
use crate::mayo::rollup::MetricsQueryRow;

use crate::relish::output::sparkline;
use crate::relish::tui::app::{DetailTab, TuiApp, View};
use crate::relish::tui::matching;

use super::{logs, widgets};

/// How to switch tabs, and the keys the tab itself takes, shown under the
/// tab bar.
fn tab_hint(tab: DetailTab) -> &'static str {
    match tab {
        DetailTab::Logs => "←/→ or Tab: switch tabs · /: search · f: follow",
        DetailTab::Metrics => "←/→ or Tab: switch tabs · /: search",
        DetailTab::Instances => "←/→ or Tab: switch tabs · ↑/↓: select · k: kill",
        _ => "←/→ or Tab: switch tabs",
    }
}

/// Width of the Metrics tab's series column, before the last value.
const SERIES_WIDTH: usize = 48;

/// A series' name as Prometheus writes it, `name{label="value"}`: the name
/// alone without labels, and the labels as they came if they aren't the
/// JSON object Mayo stores.
fn series_name(name: &str, labels: &str) -> String {
    match serde_json::from_str::<BTreeMap<String, String>>(labels) {
        Ok(labels) if labels.is_empty() => name.to_string(),
        Ok(labels) => {
            let labels = labels
                .iter()
                .map(|(key, value)| format!("{key}={value:?}"))
                .collect::<Vec<_>>()
                .join(",");
            format!("{name}{{{labels}}}")
        }
        Err(_) if labels.is_empty() => name.to_string(),
        Err(_) => format!("{name}{labels}"),
    }
}

/// Every series in a metrics answer, by name, each with its values oldest
/// first.
fn metric_series(rows: &[MetricsQueryRow]) -> BTreeMap<String, Vec<f64>> {
    let mut rows: Vec<_> = rows.iter().collect();
    rows.sort_by_key(|row| row.timestamp);
    let mut series: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for row in rows {
        series
            .entry(series_name(&row.metric_name, &row.labels))
            .or_default()
            .push(row.value);
    }
    series
}

/// The image an app's instances run, as the Overview tab shows it: one
/// reference, every distinct one mid-rollout, or `unknown` when no instance
/// reports one (a process workload, or an older node).
fn overview_image(instances: &[&ClusterInstanceStatus]) -> String {
    let images: BTreeSet<&str> = instances
        .iter()
        .filter_map(|instance| instance.instance.image.as_deref())
        .collect();
    if images.is_empty() {
        return "unknown".into();
    }
    images.into_iter().collect::<Vec<_>>().join(", ")
}

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
        // The tabs looked stuck without it (#2169).
        Line::raw(tab_hint(*tab)),
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
                Line::raw(format!("image          {}", overview_image(&instances))),
            ]);
        }
        DetailTab::Instances => {
            lines.push(widgets::heading(
                "NODE         ID                       STATE         RESTARTS  HOST PORT  PID      IMAGE",
            ));
            if instances.is_empty() {
                lines.push(Line::raw("no instances"));
            }
            let selected = app.instances_cursor.min(instances.len().saturating_sub(1));
            for (index, instance) in instances.into_iter().enumerate() {
                lines.push(widgets::row(
                    format!(
                        "{:<12} {:<24} {:<13} {:>8}  {:>9}  {:<8} {}",
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
                            .unwrap_or_else(|| "-".into()),
                        instance.instance.image.as_deref().unwrap_or("-")
                    ),
                    index == selected,
                ));
            }
        }
        DetailTab::Logs => logs::push_log_body(app, &mut lines, height),
        DetailTab::Metrics => {
            let Some(metrics) = &app.data.app_metrics else {
                lines.push(Line::raw("no metrics recorded"));
                return lines;
            };
            let series = metric_series(&metrics.data);
            let needle = app.filter.as_deref().unwrap_or_default();
            let shown: Vec<_> = series
                .iter()
                .filter(|(name, _)| matching::contains(name, needle))
                .collect();
            lines.extend(widgets::filter_status(
                app,
                shown.len(),
                series.len(),
                "metrics",
            ));
            if series.is_empty() {
                lines.push(Line::raw("no metrics recorded"));
            } else if shown.is_empty() {
                lines.push(Line::raw("no metrics match"));
            } else {
                lines.push(widgets::heading(format!(
                    "{:<SERIES_WIDTH$} {:>12}  TREND",
                    "METRIC", "LAST"
                )));
            }
            for (name, values) in shown {
                let mut spans = matching::highlight(name, needle);
                let padding = SERIES_WIDTH.saturating_sub(name.chars().count());
                spans.push(Span::raw(format!(
                    "{:padding$} {:>12.2}  {}",
                    "",
                    values.last().copied().unwrap_or_default(),
                    sparkline(values)
                )));
                lines.push(Line::from(spans));
            }
            for warning in &metrics.warnings {
                lines.push(Line::raw(format!("warning: {warning:?}")));
            }
        }
        DetailTab::Deploys => {
            lines.push(Line::raw("deployment history from every node"));
            let history =
                app.data
                    .deploy_history
                    .get(&crate::relish::tui::state::deploy_history_key(
                        name, namespace,
                    ));
            let Some(history) = history else {
                lines.push(Line::raw("no deploy history"));
                return lines;
            };
            lines.extend(widgets::partial_warnings(&history.warnings));
            if history.history.is_empty() {
                lines.push(Line::raw("no deploy history"));
            }
            for entry in &history.history {
                lines.push(Line::raw(format!(
                    "{:<8} {:<12} {:<24} {:<10?} {}/{}",
                    entry.row.id.0,
                    entry.node,
                    entry.row.image,
                    entry.row.result,
                    entry.row.steps_completed,
                    entry.row.steps_total,
                )));
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
