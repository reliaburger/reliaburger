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
        DetailTab::Instances => "←/→ or Tab: switch tabs · ↑/↓: select · k: kill · +/-: scale",
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
    let mut lines = vec![
        widgets::heading(format!("{name} · {namespace}")),
        widgets::tab_bar(&DetailTab::ALL, tab),
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
            match &app.scale_prompt {
                Some(prompt) => lines.push(widgets::heading(prompt.question())),
                None => lines.push(Line::raw(match app.desired_replicas(name, namespace) {
                    Some(count) => format!("desired {count} · [+] scale up  [-] scale down"),
                    None => "desired unknown".to_string(),
                })),
            }
            lines.push(Line::raw(""));
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
        DetailTab::Config => push_config(app, name, namespace, &mut lines),
    }
    lines
}

/// The Config tab: the spec the cluster runs for the app and, when it
/// kept one apart, the spec as declared, each with its secrets left out
/// by the API (#1322).
fn push_config(app: &TuiApp, name: &str, namespace: &str, lines: &mut Vec<Line<'static>>) {
    let view = app
        .data
        .app_spec
        .as_ref()
        .filter(|view| view.app == name && view.namespace == namespace);
    let Some(view) = view else {
        let error = app
            .data
            .app_spec_error
            .as_ref()
            .filter(|error| error.app == name && error.namespace == namespace);
        lines.push(Line::raw(match error {
            Some(error) => format!("spec unavailable: {}", error.message),
            None => "loading spec…".to_string(),
        }));
        return;
    };
    lines.push(Line::raw(format!(
        "owner          {}",
        view.owner.as_deref().unwrap_or("-")
    )));
    lines.push(Line::raw(format!(
        "gitops         {}",
        if view.gitops_owned { "owns it" } else { "no" }
    )));
    if !view.redacted.is_empty() {
        lines.push(Line::raw(format!(
            "redacted       {} value(s): env values, config file contents and secrets in commands",
            view.redacted.len()
        )));
    }
    lines.push(Line::raw(""));
    lines.push(widgets::heading("RESOLVED SPEC"));
    push_spec(&view.spec, lines);
    if let Some(declared) = &view.declared {
        lines.push(Line::raw(""));
        lines.push(widgets::heading("DECLARED SPEC"));
        push_spec(declared, lines);
    }
}

/// One spec as TOML, the way a manifest would write it.
fn push_spec(spec: &crate::config::AppSpec, lines: &mut Vec<Line<'static>>) {
    let text = toml::to_string_pretty(spec)
        .or_else(|_| serde_json::to_string_pretty(spec))
        .unwrap_or_else(|error| format!("unreadable spec: {error}"));
    lines.extend(text.lines().map(|line| Line::raw(line.to_string())));
}

#[cfg(test)]
mod tests {
    use crate::relish::tui::app::{DetailTab, TuiApp, View};
    use crate::relish::tui::data::ProviderError;
    use crate::relish::tui::fixtures::{TestScenario, render_to_string};
    use crate::relish::tui::msg::{DataUpdate, Msg};
    use crate::relish::tui::state::Effect;

    fn arrow(code: crossterm::event::KeyCode) -> Msg {
        Msg::Key(crossterm::event::KeyEvent::new(
            code,
            crossterm::event::KeyModifiers::NONE,
        ))
    }

    fn right() -> Msg {
        arrow(crossterm::event::KeyCode::Right)
    }

    fn left() -> Msg {
        arrow(crossterm::event::KeyCode::Left)
    }

    fn on_config_tab() -> TuiApp {
        let mut app = TuiApp::with_test_data(TestScenario::HealthyCluster);
        app.view_stack.push(View::AppDetail {
            app: "web".into(),
            namespace: "default".into(),
            tab: DetailTab::Deploys,
        });
        // Config follows Deploys.
        app.update(right());
        app
    }

    fn web_spec() -> crate::bun::spec_read::AppSpecView {
        let config = crate::config::Config::parse(
            "[app.web]\nimage = 'web:v2'\nreplicas = 3\nport = 8080\n\
             [app.web.env]\nDB_PASSWORD = 'hunter2'\n",
        )
        .unwrap();
        let id = crate::meat::AppId::new("web", "default");
        let mut desired = crate::council::types::DesiredState::default();
        desired.apps.insert(id.clone(), config.app["web"].clone());
        crate::bun::spec_read::app_spec_view(&desired, &id).unwrap()
    }

    #[test]
    fn opening_the_config_tab_fetches_the_apps_spec() {
        let app = on_config_tab();
        assert_eq!(
            app.pending.last(),
            Some(&Effect::FetchAppSpec {
                app: "web".into(),
                namespace: "default".into(),
            })
        );
    }

    #[test]
    fn the_config_tab_shows_the_resolved_spec_without_its_secrets() {
        let mut app = on_config_tab();
        app.data.app_spec = Some(web_spec());
        let screen = render_to_string(&app, 120, 40);
        assert!(screen.contains("RESOLVED SPEC"), "{screen}");
        assert!(screen.contains(r#"image = "web:v2""#), "{screen}");
        assert!(screen.contains("replicas = 3"), "{screen}");
        assert!(screen.contains(r#"DB_PASSWORD = "[redacted]""#), "{screen}");
        assert!(!screen.contains("hunter2"), "{screen}");
        assert!(screen.contains("redacted       1 value(s)"), "{screen}");
        assert!(!screen.contains("not exposed by the API"), "{screen}");
    }

    #[test]
    fn the_config_tab_says_why_it_has_no_spec() {
        let mut app = on_config_tab();
        app.data.app_spec = None;
        assert!(render_to_string(&app, 120, 40).contains("loading spec"));
        app.update(spec_answer(
            "web",
            ProviderError::Api {
                status: 403,
                body: "token scope does not allow web".into(),
            },
        ));
        let screen = render_to_string(&app, 120, 40);
        assert!(
            screen.contains("spec unavailable: 403: token scope does not allow web"),
            "{screen}"
        );
        // Another app's spec is never shown as this one's.
        let mut other = web_spec();
        other.app = "api".into();
        app.data.app_spec = Some(other);
        assert!(render_to_string(&app, 120, 40).contains("spec unavailable"));
    }

    fn spec_answer(app: &str, error: ProviderError) -> Msg {
        Msg::Data(DataUpdate::AppSpec {
            app: app.into(),
            namespace: "default".into(),
            result: Err(error),
        })
    }

    #[test]
    fn another_apps_refusal_is_not_shown_on_this_apps_config_tab() {
        let mut app = on_config_tab();
        app.data.app_spec = None;
        app.update(spec_answer(
            "api",
            ProviderError::Api {
                status: 403,
                body: "token scope does not allow api".into(),
            },
        ));
        let screen = render_to_string(&app, 120, 40);
        assert!(screen.contains("loading spec"), "{screen}");
        assert!(!screen.contains("does not allow api"), "{screen}");
    }

    #[test]
    fn opening_the_config_tab_again_forgets_the_last_refusal() {
        let mut app = on_config_tab();
        app.data.app_spec = None;
        app.update(spec_answer(
            "web",
            ProviderError::Api {
                status: 503,
                body: "no leader".into(),
            },
        ));
        assert!(app.data.app_spec_error.is_some());
        app.update(right());
        app.update(left());
        assert!(app.data.app_spec_error.is_none());
        assert!(render_to_string(&app, 120, 40).contains("loading spec"));
    }

    #[test]
    fn a_followers_404_shows_the_reason_its_body_carries() {
        let mut app = on_config_tab();
        app.data.app_spec = None;
        app.update(spec_answer(
            "web",
            ProviderError::Api {
                status: 404,
                body: r#"{"error":"the leader doesn't serve spec reads; it may run an older release"}"#
                    .into(),
            },
        ));
        let screen = render_to_string(&app, 120, 40);
        assert!(
            screen.contains(
                "spec unavailable: 404: the leader doesn't serve spec reads; it may run an older release"
            ),
            "{screen}"
        );
    }

    #[test]
    fn an_empty_404_says_the_node_does_not_serve_spec_reads() {
        let mut app = on_config_tab();
        app.data.app_spec = None;
        app.update(spec_answer(
            "web",
            ProviderError::Api {
                status: 404,
                body: String::new(),
            },
        ));
        let screen = render_to_string(&app, 120, 40);
        assert!(
            screen.contains("spec unavailable: this node's release doesn't serve spec reads"),
            "{screen}"
        );
    }
}
