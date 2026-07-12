use ratatui::text::Line;

use crate::relish::tui::app::TuiApp;

use super::{apps, widgets};

pub fn lines(app: &TuiApp) -> Vec<Line<'static>> {
    if app.data.instances.is_empty() {
        return vec![
            widgets::heading("Apps"),
            Line::raw("no apps deployed — relish apply <path>"),
            Line::raw(""),
            widgets::heading("Nodes"),
            Line::raw(if app.data.nodes.is_empty() {
                "single-node mode — no gossip peers"
            } else {
                "cluster connected"
            }),
        ];
    }
    let mut output = vec![widgets::heading("Apps")];
    output.extend(apps::lines(app));
    output.push(Line::raw(""));
    output.push(widgets::heading("Nodes"));
    for node in &app.data.nodes {
        output.push(Line::raw(format!(
            "{:<18} {:<10} {}{}",
            node.node_id,
            node.state,
            if node.is_council { "COUNCIL " } else { "" },
            if node.is_leader { "★ LEADER" } else { "" }
        )));
    }
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
    if !app.data.alerts.is_empty() {
        output.push(Line::raw(""));
        output.push(widgets::heading(format!(
            "Alerts: {}",
            app.data.alerts.len()
        )));
    }
    output
}
