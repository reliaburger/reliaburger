use ratatui::text::Line;

use crate::relish::tui::app::{TuiApp, View};

use super::{apps, widgets};

pub fn lines(app: &TuiApp, height: usize) -> Vec<Line<'static>> {
    let View::Logs { app: selected } = app.view() else {
        return Vec::new();
    };
    let Some((name, namespace)) = selected.as_ref() else {
        let mut lines = vec![widgets::heading("Choose an app to stream")];
        lines.extend(apps::lines(app));
        if app.data.instances.is_empty() {
            lines.push(Line::raw("no apps to stream"));
        }
        return lines;
    };
    let mut lines = vec![widgets::heading(format!(
        "{name} · {namespace} · follow {}",
        if app.log_follow { "on" } else { "off" }
    ))];
    lines.push(Line::raw("live logs from the connected node"));
    if let Some(error) = &app.log_stream_down {
        lines.push(Line::raw(format!(
            "stream disconnected — reconnecting: {error}"
        )));
    }
    let needle = app.filter.as_deref().unwrap_or_default().to_lowercase();
    let filtered: Vec<_> = app
        .log_lines
        .iter()
        .filter(|line| needle.is_empty() || line.line.to_lowercase().contains(&needle))
        .collect();
    if filtered.is_empty() {
        lines.push(Line::raw("waiting for logs…"));
    }
    let available = height.saturating_sub(lines.len());
    let scroll = app.log_scroll.min(filtered.len().saturating_sub(available));
    for line in filtered
        .into_iter()
        .rev()
        .skip(scroll)
        .take(available)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        lines.push(Line::raw(format!("{} │ {}", line.instance, line.line)));
    }
    lines
}
