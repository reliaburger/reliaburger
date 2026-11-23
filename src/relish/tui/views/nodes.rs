use ratatui::style::Style;
use ratatui::text::Line;

use crate::relish::nodes_cmd::{self, NodeRow};
use crate::relish::tui::app::{NodeTab, TuiApp, View};
use crate::relish::tui::impact::{AppRisk, CouncilImpact, node_impact};
use crate::relish::tui::theme;

use super::widgets;

pub fn lines(app: &TuiApp) -> Vec<Line<'static>> {
    match app.view() {
        View::NodeDetail { node, tab } => detail(app, node, *tab),
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

/// Lay out a table, heading first: each column as wide as its widest cell,
/// two spaces apart, the `right` columns right-aligned.
fn table_lines(table: &[Vec<String>], right: &[usize]) -> Vec<String> {
    let columns = table.first().map(Vec::len).unwrap_or_default();
    let widths: Vec<usize> = (0..columns)
        .map(|column| {
            table
                .iter()
                .map(|cells| cells[column].chars().count())
                .max()
                .unwrap_or_default()
        })
        .collect();
    table
        .iter()
        .map(|cells| {
            let mut line = String::new();
            for (column, (cell, width)) in cells.iter().zip(&widths).enumerate() {
                if column > 0 {
                    line.push_str("  ");
                }
                if right.contains(&column) {
                    line.push_str(&format!("{cell:>width$}"));
                } else {
                    line.push_str(&format!("{cell:<width$}"));
                }
            }
            line.trim_end().to_string()
        })
        .collect()
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
    // CPU, MEM and DISK are numbers: right-aligned, as `relish nodes`.
    let mut rendered = table_lines(&table, &[6, 7, 8]).into_iter();
    let mut lines = vec![widgets::heading(rendered.next().unwrap_or_default())];
    if app.data.nodes.is_empty() {
        lines.push(Line::raw("single-node mode — no gossip peers"));
    }
    for (index, line) in rendered.enumerate() {
        lines.push(widgets::row(line, index == app.nodes_cursor));
    }
    lines
}

/// How to switch tabs, and the keys the tab itself takes, shown under the
/// tab bar.
fn tab_hint(tab: NodeTab) -> &'static str {
    match tab {
        NodeTab::Workloads => "←/→ or Tab: switch tabs · ↑/↓: select · Enter: open the app",
        _ => "←/→ or Tab: switch tabs",
    }
}

fn detail(app: &TuiApp, name: &str, tab: NodeTab) -> Vec<Line<'static>> {
    let Some(row) = app.data.nodes.iter().find(|row| row.status.node_id == name) else {
        return vec![Line::raw("node not found")];
    };
    let mut lines = vec![
        widgets::heading(&row.status.node_id),
        widgets::tab_bar(&NodeTab::ALL, &tab),
        Line::raw(tab_hint(tab)),
        Line::raw(""),
    ];
    match tab {
        NodeTab::Overview => overview(app, row, &mut lines),
        NodeTab::Workloads => workloads(app, name, &mut lines),
        NodeTab::Maintenance => maintenance(app, row, &mut lines),
    }
    lines
}

fn overview(app: &TuiApp, row: &NodeRow, lines: &mut Vec<Line<'static>>) {
    let node = &row.status;
    let [.., state, _, _, version, cpu, memory, disk, allocated] = nodes_cmd::row_cells(row);
    let instances = app.node_instances(&node.node_id);
    let running = instances
        .iter()
        .filter(|row| row.instance.state == "running")
        .count();
    lines.extend([
        Line::raw(format!("address          {}", node.address)),
        Line::raw(format!("state            {state}")),
        Line::raw(format!("incarnation      {}", node.incarnation)),
        Line::raw(format!("council          {}", node.is_council)),
        Line::raw(format!("leader           {}", node.is_leader)),
        Line::raw(format!("labels           {}", labels_cell(row))),
        Line::raw(format!("version          {version}")),
        Line::raw(format!("cpu              {cpu}")),
        Line::raw(format!("memory           {memory}")),
        Line::raw(format!("disk             {disk}")),
        Line::raw(format!("allocated        {allocated}")),
        Line::raw(format!(
            "instances        {} ({running} running)",
            instances.len()
        )),
    ]);
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
}

/// Every instance on the node, from the cluster status the TUI already
/// polls: the same rows as the app detail's Instances tab, filtered by
/// node rather than by app.
fn workloads(app: &TuiApp, name: &str, lines: &mut Vec<Line<'static>>) {
    let instances = app.node_instances(name);
    let mut table = vec![
        ["APP", "NAMESPACE", "INSTANCE", "STATE", "RESTARTS", "IMAGE"]
            .map(String::from)
            .to_vec(),
    ];
    for row in &instances {
        let instance = &row.instance;
        table.push(vec![
            instance.app_name.clone(),
            instance.namespace.clone(),
            instance.id.clone(),
            instance.state.clone(),
            instance.restart_count.to_string(),
            instance.image.clone().unwrap_or_else(|| "-".into()),
        ]);
    }
    let mut rendered = table_lines(&table, &[4]).into_iter();
    lines.push(widgets::heading(rendered.next().unwrap_or_default()));
    if instances.is_empty() {
        lines.push(Line::raw("no instances on this node"));
    }
    let selected = app.instances_cursor.min(instances.len().saturating_sub(1));
    for (index, line) in rendered.enumerate() {
        lines.push(widgets::row(line, index == selected));
    }
}

/// What would stop, or lose its spare, if the node went away.
fn maintenance(app: &TuiApp, row: &NodeRow, lines: &mut Vec<Line<'static>>) {
    let section = app
        .data
        .summary
        .as_ref()
        .and_then(|summary| summary.council.value.as_ref());
    let impact = node_impact(
        row,
        &app.data.nodes,
        &app.data.instances,
        app.data.council.as_ref(),
        section,
    );
    lines.push(Line::raw(format!(
        "If {} went away (a reboot, an OS update, a decommission):",
        row.status.node_id
    )));
    lines.push(Line::raw(""));
    lines.push(council_line(impact.council));
    lines.push(Line::raw(""));
    if impact.apps.is_empty() {
        lines.push(Line::raw("no app instances on this node"));
        return;
    }
    let mut table = vec![
        [
            "APP",
            "NAMESPACE",
            "HERE",
            "READY ELSEWHERE",
            "IMPACT",
            "DATA",
        ]
        .map(String::from)
        .to_vec(),
    ];
    for entry in &impact.apps {
        table.push(vec![
            entry.app.clone(),
            entry.namespace.clone(),
            entry.here.to_string(),
            entry.ready_elsewhere.to_string(),
            match entry.risk {
                AppRisk::GoesDown => "goes down",
                AppRisk::LosesRedundancy => "loses redundancy",
                AppRisk::Keeps => "keeps serving",
            }
            .to_string(),
            if entry.data_on_node { "here" } else { "-" }.to_string(),
        ]);
    }
    let mut rendered = table_lines(&table, &[2, 3]).into_iter();
    lines.push(widgets::heading(rendered.next().unwrap_or_default()));
    for (line, entry) in rendered.zip(&impact.apps) {
        lines.push(match entry.risk {
            AppRisk::GoesDown => Line::styled(line, Style::default().fg(theme::BAD)),
            AppRisk::LosesRedundancy => Line::styled(line, Style::default().fg(theme::WARNING)),
            AppRisk::Keeps => Line::raw(line),
        });
    }
    if impact.apps.iter().any(|entry| entry.data_on_node) {
        lines.push(Line::raw(""));
        lines.push(Line::raw(
            "DATA here: a managed volume or host path on this node. That replica waits for",
        ));
        lines.push(Line::raw("the node to come back rather than moving."));
    }
}

fn council_line(council: CouncilImpact) -> Line<'static> {
    match council {
        CouncilImpact::NotAVoter => Line::raw("council   not a voter: quorum is unaffected"),
        CouncilImpact::AlreadyDown => {
            Line::raw("council   a voter, already counted as gone: no further change")
        }
        CouncilImpact::KeepsQuorum {
            voters,
            answering_after,
        } => Line::raw(format!(
            "council   a voter: {answering_after} of {voters} voters would still answer, enough for quorum"
        )),
        CouncilImpact::LosesQuorum {
            voters,
            answering_after,
        } => Line::styled(
            format!(
                "council   a voter: the council loses quorum, only {answering_after} of {voters} \
                 voters would answer"
            ),
            Style::default().fg(theme::BAD),
        ),
        CouncilImpact::Unknown => {
            Line::raw("council   a voter: quorum unknown, the summary hasn't said who answers")
        }
    }
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
