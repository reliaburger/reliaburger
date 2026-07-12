use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::relish::tui::app::{Connection, Level, TuiApp, View};
use crate::relish::tui::theme;

pub fn too_small(frame: &mut Frame<'_>) {
    frame.render_widget(
        Paragraph::new("terminal too small — resize to at least 80x24")
            .alignment(Alignment::Center)
            .block(Block::default().borders(Borders::ALL).title("relish")),
        frame.area(),
    );
}

pub fn header(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let leader = app
        .data
        .council
        .as_ref()
        .and_then(|council| council.leader.as_deref())
        .unwrap_or("-");
    let apps = app.all_apps().len();
    let connection = match app.connection {
        Connection::Connected => Span::styled("CONNECTED", Style::default().fg(theme::GOOD)),
        Connection::Disconnected { .. } => {
            Span::styled("DISCONNECTED", Style::default().fg(theme::BAD))
        }
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" RELISH ", theme::title()),
            Span::raw(format!(
                " {} nodes · {apps} apps · leader {leader} · ",
                app.data.nodes.len()
            )),
            connection,
        ])),
        area,
    );
}

pub fn status_bar(frame: &mut Frame<'_>, area: Rect, app: &TuiApp) {
    let mut spans = vec![Span::raw(
        " [a]pps [n]odes [j]obs [e]vents [l]ogs [r]outes [s]earch [?]help [:]cmd [q]uit ",
    )];
    if let Some(filter) = &app.filter {
        spans.push(Span::styled(
            format!(" /{filter}"),
            Style::default().fg(theme::ACCENT),
        ));
    }
    if let Some((message, level)) = &app.status_message {
        let colour = match level {
            Level::Info => Color::Cyan,
            Level::Warning => theme::WARNING,
            Level::Error => theme::BAD,
        };
        spans.push(Span::styled(
            format!(" {message}"),
            Style::default().fg(colour),
        ));
    }
    if let Connection::Disconnected { .. } = &app.connection {
        let time = app
            .data
            .last_updated
            .map(format_time)
            .unwrap_or_else(|| "never".into());
        spans.push(Span::styled(
            format!(" DISCONNECTED (retrying) — data as of {time}"),
            Style::default().fg(theme::BAD),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

pub fn view_title(view: &View) -> &'static str {
    match view {
        View::Dashboard => "dashboard",
        View::Apps => "apps",
        View::AppDetail { .. } => "app detail",
        View::Nodes => "nodes",
        View::NodeDetail { .. } => "node detail",
        View::Jobs => "jobs",
        View::JobDetail { .. } => "job detail",
        View::Events => "events",
        View::Logs { .. } => "logs",
        View::Routes => "routes",
        View::RouteDetail { .. } => "route detail",
        View::Search => "search",
        View::Help => "help",
    }
}

pub fn row(text: impl Into<String>, selected: bool) -> Line<'static> {
    let text = text.into();
    if selected {
        Line::styled(text, theme::selected())
    } else {
        Line::raw(text)
    }
}

pub fn heading(text: impl Into<String>) -> Line<'static> {
    Line::styled(text.into(), theme::title())
}

pub fn format_time(timestamp: u64) -> String {
    let seconds = timestamp % 86_400;
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds % 3600 / 60,
        seconds % 60
    )
}
