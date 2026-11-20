use ratatui::text::Line;

use crate::relish::nodes_cmd::{self, NodeRow};
use crate::relish::tui::app::{TuiApp, View};

use super::widgets;

pub fn lines(app: &TuiApp) -> Vec<Line<'static>> {
    match app.view() {
        View::NodeDetail { node } => detail(app, node),
        _ => list(app),
    }
}

/// `zone=a,rack=b`, or `-` for a node with no labels.
fn labels_cell(row: &NodeRow) -> String {
    if row.status.labels.is_empty() {
        return "-".to_string();
    }
    row.status
        .labels
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// The `relish nodes` table, built from the same rows and cells so the two
/// can't drift. Each column is as wide as its widest cell rather than
/// `relish nodes`' fixed widths, so the table fits a 120-column terminal.
/// INC stays on the node's own page; LABELS goes last, and only when some
/// node has labels.
fn list(app: &TuiApp) -> Vec<Line<'static>> {
    let labelled = app
        .data
        .nodes
        .iter()
        .any(|row| !row.status.labels.is_empty());
    let mut table: Vec<Vec<String>> = vec![
        nodes_cmd::COLUMNS
            .iter()
            .map(|column| column.to_string())
            .chain(labelled.then(|| "LABELS".to_string()))
            .collect(),
    ];
    for row in &app.data.nodes {
        table.push(
            nodes_cmd::row_cells(row)
                .into_iter()
                .chain(labelled.then(|| labels_cell(row)))
                .collect(),
        );
    }
    let widths: Vec<usize> = (0..table[0].len())
        .map(|column| {
            table
                .iter()
                .map(|cells| cells[column].chars().count())
                .max()
                .unwrap_or_default()
        })
        .collect();
    let mut rendered = table.iter().map(|cells| {
        let mut line = String::new();
        for (column, (cell, width)) in cells.iter().zip(&widths).enumerate() {
            if column > 0 {
                line.push_str("  ");
            }
            // CPU, MEM and DISK are numbers: right-aligned, as `relish nodes`.
            if (6..=8).contains(&column) {
                line.push_str(&format!("{cell:>width$}"));
            } else {
                line.push_str(&format!("{cell:<width$}"));
            }
        }
        line.trim_end().to_string()
    });
    let mut lines = vec![widgets::heading(rendered.next().unwrap_or_default())];
    if app.data.nodes.is_empty() {
        lines.push(Line::raw("single-node mode — no gossip peers"));
    }
    for (index, line) in rendered.enumerate() {
        lines.push(widgets::row(line, index == app.nodes_cursor));
    }
    lines
}

fn detail(app: &TuiApp, name: &str) -> Vec<Line<'static>> {
    let Some(row) = app.data.nodes.iter().find(|row| row.status.node_id == name) else {
        return vec![Line::raw("node not found")];
    };
    let node = &row.status;
    let [.., version, cpu, memory, disk, allocated] = nodes_cmd::row_cells(row);
    let mut lines = vec![
        widgets::heading(&node.node_id),
        Line::raw(format!("address          {}", node.address)),
        Line::raw(format!("state            {}", node.state)),
        Line::raw(format!("incarnation      {}", node.incarnation)),
        Line::raw(format!("council          {}", node.is_council)),
        Line::raw(format!("leader           {}", node.is_leader)),
        Line::raw(format!("labels           {}", labels_cell(row))),
        Line::raw(format!("version          {version}")),
        Line::raw(format!("cpu              {cpu}")),
        Line::raw(format!("memory           {memory}")),
        Line::raw(format!("disk             {disk}")),
        Line::raw(format!("allocated        {allocated}")),
    ];
    if let Some(why) = &row.unanswered {
        lines.push(Line::raw(format!("didn't answer    {why}")));
    }
    if let Some(council) = &app.data.council {
        lines.push(Line::raw(format!("council term     {}", council.term)));
        lines.push(Line::raw(format!(
            "last applied     {:?}",
            council.last_applied_log
        )));
    }
    lines
}

#[cfg(test)]
mod tests {
    use crate::relish::tui::app::{TuiApp, View};
    use crate::relish::tui::fixtures::{TestScenario, render_to_string};

    fn nodes_screen() -> String {
        let mut app = TuiApp::with_test_data(TestScenario::HealthyCluster);
        app.view_stack.push(View::Nodes);
        render_to_string(&app, 160, 40)
    }

    fn line_for<'a>(screen: &'a str, node: &str) -> &'a str {
        screen
            .lines()
            .find(|line| line.contains(&format!("{node} ")))
            .unwrap_or_else(|| panic!("no row for {node}:\n{screen}"))
    }

    // The TUI's nodes view shows what `relish nodes` shows (#2168).
    #[test]
    fn nodes_view_has_the_relish_nodes_columns() {
        let screen = nodes_screen();
        for column in ["VERSION", "CPU", "MEM", "DISK", "ALLOCATED"] {
            assert!(screen.contains(column), "{column} missing:\n{screen}");
        }
    }

    #[test]
    fn a_node_with_usage_shows_its_cpu_and_memory() {
        let screen = nodes_screen();
        let row = line_for(&screen, "node-2");
        assert!(row.contains("0.3.1"), "{row}");
        assert!(row.contains("48%"), "cpu: {row}");
        assert!(row.contains("25%"), "memory: {row}");
        assert!(row.contains("30%"), "disk: {row}");
        assert!(row.contains("cpu 50% mem 25%"), "allocated: {row}");
    }

    #[test]
    fn a_node_without_usage_shows_dashes() {
        let screen = nodes_screen();
        let row = line_for(&screen, "node-3");
        // Version, CPU, memory and disk are unknown; the leader still
        // knows what it allocated there.
        let cells: Vec<&str> = row.split_whitespace().collect();
        assert_eq!(
            cells.iter().filter(|cell| **cell == "-").count(),
            5,
            "LEADER, VERSION, CPU, MEM and DISK: {row}"
        );
        assert!(row.contains("cpu 75% mem 25%"), "allocated: {row}");
    }
}
