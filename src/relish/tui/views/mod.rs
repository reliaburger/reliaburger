//! Ratatui renderers. No renderer owns state.

mod app_detail;
mod apps;
mod dashboard;
mod events;
mod help;
mod jobs;
mod logs;
mod nodes;
mod routes;
mod search;
mod widgets;

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use super::app::{TuiApp, View};
use super::theme;

/// Render the current view plus global chrome.
pub fn render(frame: &mut Frame<'_>, app: &TuiApp) {
    if frame.area().width < 80 || frame.area().height < 24 {
        widgets::too_small(frame);
        return;
    }
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(2),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .split(frame.area());
    widgets::header(frame, chunks[0], app);
    let lines: Vec<Line<'static>> = match app.view() {
        View::Dashboard => dashboard::lines(app),
        View::Apps => apps::lines(app),
        View::AppDetail { .. } => app_detail::lines(app),
        View::Nodes | View::NodeDetail { .. } => nodes::lines(app),
        View::Jobs | View::JobDetail { .. } => jobs::lines(app),
        View::Events => events::lines(app),
        View::Logs { .. } => logs::lines(app),
        View::Routes | View::RouteDetail { .. } => routes::lines(app),
        View::Search => search::lines(app),
        View::Help => help::lines(app),
    };
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(widgets::view_title(app.view())),
            )
            .wrap(Wrap { trim: false }),
        chunks[1],
    );
    widgets::status_bar(frame, chunks[2], app);
    if let Some(palette) = &app.palette {
        let area = ratatui::layout::Rect::new(
            1,
            frame.area().height.saturating_sub(3),
            frame.area().width.saturating_sub(2),
            3,
        );
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(format!(":{}", palette.input))
                .style(theme::title())
                .block(Block::default().borders(Borders::ALL).title("command")),
            area,
        );
    }
}
