use ratatui::text::Line;

use crate::relish::tui::app::TuiApp;

use super::widgets;

pub fn lines(app: &TuiApp) -> Vec<Line<'static>> {
    let mut lines = vec![widgets::heading(
        "TIME      SEV       KIND            APP              MESSAGE",
    )];
    if let Some(error) = &app.event_stream_down {
        lines.push(Line::raw(format!(
            "stream disconnected — reconnecting: {error}"
        )));
    }
    let needle = app.filter.as_deref().unwrap_or_default().to_lowercase();
    let events: Vec<_> = app
        .data
        .events
        .iter()
        .filter(|event| {
            needle.is_empty()
                || event.message.to_lowercase().contains(&needle)
                || event
                    .app
                    .as_deref()
                    .is_some_and(|name| name.to_lowercase().contains(&needle))
        })
        .collect();
    if events.is_empty() {
        lines.push(Line::raw("no events yet"));
    }
    for event in events
        .into_iter()
        .rev()
        .skip(app.events_scroll)
        .take(30)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        lines.push(Line::raw(format!(
            "{}  {:<9?} {:<15?} {:<16} {}",
            widgets::format_time(event.timestamp),
            event.severity,
            event.kind,
            event.app.as_deref().unwrap_or("-"),
            event.message
        )));
    }
    lines
}
