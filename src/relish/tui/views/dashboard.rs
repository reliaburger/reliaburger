//! The dashboard: `relish status`'s screen, then the newest events.
//!
//! It reads the cluster summary (`GET /v1/cluster/summary`) and prints the
//! same lines `relish status` does, from the same `assess`, so a section
//! that saw only some nodes says `partial (2 of 3 nodes)` here too, never a
//! smaller number. The lists live in their own views.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::bun::summary::verdict::{Mark, Verdict, problems, section_lines, verdict};
use crate::relish::tui::app::TuiApp;
use crate::relish::tui::theme;

use super::widgets;

/// The section column's width, as `relish status` prints it.
const SECTION_WIDTH: usize = 10;

pub fn lines(app: &TuiApp) -> Vec<Line<'static>> {
    let mut output = match &app.data.summary {
        Some(summary) => status_lines(summary),
        None => vec![Line::raw("waiting for the cluster summary…")],
    };
    output.push(Line::raw(""));
    output.push(widgets::heading("Recent events"));
    for event in app.data.events.iter().rev().take(8).rev() {
        output.push(Line::raw(format!(
            "{}  {:?}  {}",
            widgets::format_time(event.timestamp),
            event.kind,
            event.message
        )));
    }
    output.extend(widgets::partial_warnings(&app.data.event_warnings));
    output
}

/// The verdict and one line per section. The TUI doesn't run `wtf`'s
/// checks, so there's no `wtf` line.
fn status_lines(summary: &crate::bun::summary::ClusterSummary) -> Vec<Line<'static>> {
    let lines = section_lines(summary);
    let verdict = verdict(&lines);
    let problems = match problems(&lines) {
        0 => "no problems".to_string(),
        1 => "1 problem".to_string(),
        n => format!("{n} problems"),
    };
    let colour = match verdict {
        Verdict::Healthy => theme::GOOD,
        Verdict::Degraded => theme::WARNING,
        Verdict::Down => theme::BAD,
    };
    let mut output = vec![
        Line::from(vec![
            Span::styled("Cluster ", theme::title()),
            Span::styled(
                verdict.word(),
                Style::default().fg(colour).add_modifier(Modifier::BOLD),
            ),
            Span::raw(format!(
                ", {problems} ({} answered at {})",
                summary.answered_by,
                widgets::format_time(summary.generated_at_unix_ms / 1000)
            )),
        ]),
        Line::raw(""),
    ];
    for line in lines {
        let style = match line.mark {
            Mark::Quiet => Style::default(),
            Mark::Note | Mark::Problem => Style::default().fg(theme::WARNING),
            Mark::Down => Style::default().fg(theme::BAD),
        };
        let mut spans = vec![
            Span::raw(format!("{:<SECTION_WIDTH$} ", line.section)),
            Span::styled(line.text, style),
        ];
        if let Some(next) = line.next {
            spans.push(Span::styled(
                format!("  {next}"),
                Style::default().fg(theme::MUTED),
            ));
        }
        output.push(Line::from(spans));
    }
    output
}
