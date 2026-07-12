use std::collections::BTreeMap;

use ratatui::text::Line;

use crate::relish::tui::app::TuiApp;

use super::widgets;

type AppAggregate = (usize, usize, String, u32, Vec<String>);

pub fn aggregate(app: &TuiApp) -> Vec<(String, String, usize, usize, String, u32, String)> {
    let mut rows: BTreeMap<(String, String), AppAggregate> = BTreeMap::new();
    for instance in &app.data.instances {
        let entry = rows
            .entry((instance.app_name.clone(), instance.namespace.clone()))
            .or_insert((0, 0, "running".into(), 0, Vec::new()));
        entry.1 += 1;
        if instance.state == "running" {
            entry.0 += 1;
        } else {
            entry.2 = instance.state.clone();
        }
        entry.3 += instance.restart_count;
        if let Some(port) = instance.host_port {
            entry.4.push(port.to_string());
        }
    }
    rows.into_iter()
        .filter(|((name, namespace), _)| {
            app.filter.as_ref().is_none_or(|filter| {
                let filter = filter.to_lowercase();
                name.to_lowercase().contains(&filter) || namespace.to_lowercase().contains(&filter)
            })
        })
        .map(
            |((name, namespace), (ready, total, state, restarts, ports))| {
                (
                    name,
                    namespace,
                    ready,
                    total,
                    state,
                    restarts,
                    ports.join(","),
                )
            },
        )
        .collect()
}

pub fn lines(app: &TuiApp) -> Vec<Line<'static>> {
    let rows = aggregate(app);
    let mut lines = vec![widgets::heading(
        "NAME                 NS           READY  STATUS       RESTARTS  PORTS",
    )];
    if rows.is_empty() {
        lines.push(Line::raw("no apps"));
        return lines;
    }
    for (index, (name, namespace, ready, total, state, restarts, ports)) in
        rows.into_iter().enumerate()
    {
        lines.push(widgets::row(
            format!(
                "{name:<20} {namespace:<12} {ready:>2}/{total:<2}  {state:<12} {restarts:>8}  {ports}"
            ),
            index == app.apps_cursor,
        ));
    }
    lines
}
