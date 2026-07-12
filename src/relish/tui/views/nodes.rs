use ratatui::text::Line;

use crate::relish::tui::app::{TuiApp, View};

use super::widgets;

pub fn lines(app: &TuiApp) -> Vec<Line<'static>> {
    match app.view() {
        View::NodeDetail { node } => detail(app, node),
        _ => list(app),
    }
}

fn list(app: &TuiApp) -> Vec<Line<'static>> {
    let mut lines = vec![widgets::heading(
        "ID                 ADDRESS                 STATE      INC  COUNCIL  LEADER  LABELS",
    )];
    if app.data.nodes.is_empty() {
        lines.push(Line::raw("single-node mode — no gossip peers"));
    }
    for (index, node) in app.data.nodes.iter().enumerate() {
        lines.push(widgets::row(
            format!(
                "{:<18} {:<23} {:<10} {:>3}  {:<7}  {:<6}  {:?}",
                node.node_id,
                node.address,
                node.state,
                node.incarnation,
                if node.is_council { "yes" } else { "no" },
                if node.is_leader { "★" } else { "" },
                node.labels
            ),
            index == app.nodes_cursor,
        ));
    }
    lines
}

fn detail(app: &TuiApp, name: &str) -> Vec<Line<'static>> {
    let Some(node) = app.data.nodes.iter().find(|node| node.node_id == name) else {
        return vec![Line::raw("node not found")];
    };
    let mut lines = vec![
        widgets::heading(&node.node_id),
        Line::raw(format!("address          {}", node.address)),
        Line::raw(format!("state            {}", node.state)),
        Line::raw(format!("incarnation      {}", node.incarnation)),
        Line::raw(format!("council          {}", node.is_council)),
        Line::raw(format!("leader           {}", node.is_leader)),
        Line::raw(format!("labels           {:?}", node.labels)),
    ];
    if let Some(council) = &app.data.council {
        lines.push(Line::raw(format!("council term     {}", council.term)));
        lines.push(Line::raw(format!(
            "last applied     {:?}",
            council.last_applied_log
        )));
    }
    lines
}
