use ratatui::text::{Line, Span};

use crate::relish::tui::app::{TuiApp, View};
use crate::relish::tui::matching;

use super::{apps, widgets};

/// The keys the logs view takes, under its heading.
const LOGS_HINT: &str = "/: search · f: follow · PgUp/PgDn: scroll";

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
    let mut lines = vec![
        widgets::heading(format!(
            "{name} · {namespace} · follow {}",
            if app.log_follow { "on" } else { "off" }
        )),
        Line::raw(LOGS_HINT),
    ];
    push_log_body(app, &mut lines, height);
    lines
}

/// The live log lines, shared by the logs view and the app detail's Logs
/// tab: the stream's state, any `/` search with its count, then the newest
/// lines that fit under what `lines` already holds, each match highlighted.
pub fn push_log_body(app: &TuiApp, lines: &mut Vec<Line<'static>>, height: usize) {
    lines.push(Line::raw("live logs from every node running the app"));
    if let Some(error) = &app.log_stream_down {
        lines.push(Line::raw(format!(
            "stream disconnected — reconnecting: {error}"
        )));
    }
    let visible = app.visible_log_lines();
    lines.extend(widgets::filter_status(
        app,
        visible.len(),
        app.log_lines.len(),
        "lines",
    ));
    if app.log_lines.is_empty() {
        lines.push(Line::raw("waiting for logs…"));
    } else if visible.is_empty() {
        lines.push(Line::raw("no lines match"));
    }
    let needle = app.filter.as_deref().unwrap_or_default();
    let available = height.saturating_sub(lines.len());
    let scroll = app.log_scroll.min(visible.len().saturating_sub(available));
    let newest = visible.len().saturating_sub(scroll);
    let oldest = newest.saturating_sub(available);
    for line in &visible[oldest..newest] {
        let mut spans = vec![Span::raw(format!("{} │ ", line.instance))];
        spans.extend(matching::highlight(&line.line, needle));
        lines.push(Line::from(spans));
    }
}
