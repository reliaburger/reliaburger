use ratatui::text::Line;

use crate::relish::tui::app::TuiApp;

use super::widgets;

pub fn lines(app: &TuiApp) -> Vec<Line<'static>> {
    let mut lines = vec![
        widgets::heading(format!("> {}", app.search.input)),
        Line::raw(""),
        widgets::heading("KIND       NAME                         CONTEXT"),
    ];
    if app.search.input.is_empty() {
        lines.push(Line::raw("type to search"));
    } else if app.search.results.is_empty() {
        lines.push(Line::raw("no matches"));
    }
    for (index, hit) in app.search.results.iter().enumerate() {
        lines.push(widgets::row(
            format!("{:<10} {:<28} {}", hit.kind, hit.name, hit.context),
            index == app.search.cursor,
        ));
    }
    lines
}
