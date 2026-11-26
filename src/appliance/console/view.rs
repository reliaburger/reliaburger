//! Drawing the console: a header, the tab bar, the current tab and a key
//! line, from a [`ConsoleState`] and nothing else.
//!
//! The Linux console (tty1) has 16 colours and the kernel's font, so
//! everything here is ASCII plus box drawing and the full block: no
//! braille, no eighth-blocks, no emoji, no RGB colours. Each screen still
//! reads in monochrome: the current tab is in reverse video and brackets,
//! and every state that has a colour has a word too.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::bun::summary::ClusterSummary;
use crate::bun::summary::verdict::{self, Mark, Verdict, WtfCheck};

use super::bigtext;
use super::local::{LocalStatus, Sample, clock};
use super::state::{ConsoleState, Fetched, Tab};

const GOOD: Color = Color::Green;
const WARNING: Color = Color::Yellow;
const BAD: Color = Color::Red;
const MUTED: Color = Color::DarkGray;

/// The label column's width on the Status, Metrics and Versions tabs.
const LABEL: usize = 11;

fn title_style() -> Style {
    Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD)
}

/// Draw the whole screen.
pub fn render(frame: &mut Frame<'_>, state: &ConsoleState) {
    let [header, tabs, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    render_header(frame, header, &state.local);
    frame.render_widget(Paragraph::new(tab_bar(state.tab)), tabs);

    let block = Block::default().borders(Borders::ALL).title(format!(
        " {} {} ",
        state.tab.number(),
        state.tab.title()
    ));
    let inner = block.inner(body);
    frame.render_widget(block, body);
    let width = usize::from(inner.width);
    let height = usize::from(inner.height);
    let lines = match state.tab {
        Tab::Status => status_lines(state, width),
        Tab::Metrics => metrics_lines(state, width),
        Tab::Workloads => workload_lines(state, height),
        Tab::Cluster => cluster_lines(state, height),
        Tab::Versions => version_lines(state),
        Tab::Logs => log_lines(state, height),
        Tab::Invaders => invader_lines(state, height),
    };
    frame.render_widget(Paragraph::new(lines), inner);

    let keys = if state.keyless {
        " read-only; this OS image's console takes no keys"
    } else if state.tab == Tab::Invaders {
        " h l or <- -> aim   space fire   r new wave   Tab leave"
    } else if state.can_quit {
        " 1-6 or <- -> tabs   up/down scroll   q quit   read-only"
    } else {
        " 1-6 or <- -> tabs   up/down scroll   read-only: no login here"
    };
    frame.render_widget(Paragraph::new(keys), footer);
    frame.render_widget(
        Paragraph::new(format!("{} UTC ", clock(state.now))).alignment(Alignment::Right),
        footer,
    );
}

fn render_header(frame: &mut Frame<'_>, area: Rect, local: &LocalStatus) {
    let mut title = vec![Span::styled(" Reliaburger OS", title_style())];
    if let Some(version) = &local.os_version {
        title.push(Span::styled(format!(" {version}"), title_style()));
    }
    if let Some(node) = &local.node {
        title.push(Span::raw(format!(" | {node}")));
    }
    if let Some(cluster) = &local.cluster {
        title.push(Span::raw(format!(" | cluster {cluster}")));
    }
    frame.render_widget(Paragraph::new(Line::from(title)), area);
    let (word, colour) = bun_word(local);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw("bun "),
            Span::styled(word, Style::default().fg(colour)),
            Span::raw(" "),
        ]))
        .alignment(Alignment::Right),
        area,
    );
}

fn bun_word(local: &LocalStatus) -> (&'static str, Color) {
    match local.healthy {
        Some(true) => ("running", GOOD),
        Some(false) => ("not healthy", WARNING),
        None => ("not answering", BAD),
    }
}

/// Every tab by number and name, the current one in reverse video and
/// brackets.
fn tab_bar(current: Tab) -> Line<'static> {
    let mut spans = vec![Span::raw(" ")];
    for tab in Tab::SHOWN {
        let label = format!("{} {}", tab.number(), tab.title());
        if tab == current {
            spans.push(Span::styled(
                format!("[{label}]"),
                Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD),
            ));
        } else {
            spans.push(Span::raw(format!(" {label} ")));
        }
        spans.push(Span::raw(" "));
    }
    Line::from(spans)
}

fn labelled(label: &str, value: impl Into<String>) -> Line<'static> {
    Line::raw(format!("{label:<LABEL$}{}", value.into()))
}

fn muted(text: impl Into<String>) -> Line<'static> {
    Line::styled(text.into(), Style::default().fg(MUTED))
}

fn warning(text: impl Into<String>) -> Line<'static> {
    Line::styled(text.into(), Style::default().fg(WARNING))
}

/// The line that says a panel's newest refresh failed, and how old what
/// it still shows is.
fn stale_line<T>(fetched: &Fetched<T>, what: &str) -> Option<Line<'static>> {
    let error = fetched.error.as_ref()?;
    Some(match fetched.at {
        Some(at) => warning(format!(
            "can't refresh {what}: {error} (showing {})",
            clock(at)
        )),
        None => warning(format!("can't read {what}: {error}")),
    })
}

// --- 1 Status ---

/// Who the machine is, then the cluster's `relish status` lines. While bun
/// doesn't answer, or the machine has no cluster yet, it's the screen the
/// console showed before it had tabs: that's the one a boot, a fallback
/// and a claim need, and it can't depend on the API it reports on.
fn status_lines(state: &ConsoleState, width: usize) -> Vec<Line<'static>> {
    let local = &state.local;
    let mut lines = identity_lines(local);
    if !local.bun_up() || local.node.is_none() {
        lines.extend(fallback_lines(local, width));
        return lines;
    }
    lines.push(Line::raw(""));
    let Some(summary) = &state.summary.value else {
        lines.push(match &state.summary.error {
            Some(error) => warning(format!("can't read the cluster summary: {error}")),
            None => muted("asking bun for the cluster summary..."),
        });
        return lines;
    };
    lines.extend(stale_line(&state.summary, "the cluster summary"));
    lines.extend(summary_lines(summary, state.wtf.as_ref()));
    lines
}

fn identity_lines(local: &LocalStatus) -> Vec<Line<'static>> {
    let (word, colour) = bun_word(local);
    let mut lines = vec![
        labelled(
            "Node",
            local
                .node
                .clone()
                .unwrap_or_else(|| "not seeded yet".into()),
        ),
        labelled(
            "Cluster",
            local.cluster.clone().unwrap_or_else(|| "-".into()),
        ),
        labelled(
            "Address",
            local
                .address
                .clone()
                .unwrap_or_else(|| "waiting for DHCP".into()),
        ),
        labelled(
            "MAC",
            if local.macs.is_empty() {
                "-".to_string()
            } else {
                local.macs.join(" ")
            },
        ),
        Line::from(vec![
            Span::raw(format!("{:<LABEL$}", "bun")),
            Span::styled(word, Style::default().fg(colour)),
            Span::raw(format!(
                " ({})",
                local.bun_version.as_deref().unwrap_or("-")
            )),
        ]),
    ];
    if local.waiting_for_claim() {
        let key = local.claim_key.clone().unwrap_or_default();
        lines.push(Line::styled(
            format!("{:<LABEL$}{key}", "Claim key"),
            title_style(),
        ));
    }
    lines
}

/// The rest of the screen bun's absence leaves: prepare's notes, what to
/// do next, and the claim key in large letters.
fn fallback_lines(local: &LocalStatus, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if !local.notes.is_empty() {
        lines.push(Line::raw(""));
        lines.extend(local.notes.iter().map(|note| Line::raw(note.clone())));
    }
    if local.node.is_some() && !local.bun_up() {
        lines.push(Line::raw(""));
        lines.push(Line::raw(
            "bun isn't answering. 2 Metrics, 5 Versions and 6 Logs read this machine.",
        ));
    }
    if local.node.is_none() {
        let hint = if local.claim_key.is_some() {
            "Claim this machine with `relish machines claim`, comparing the claim key."
        } else {
            "Seed this machine: plug in its RBSEED stick and power-cycle it."
        };
        lines.push(Line::raw(""));
        lines.push(Line::raw(hint));
    }
    if let Some(key) = local
        .claim_key
        .as_deref()
        .filter(|_| local.waiting_for_claim())
        && bigtext::width(key) <= width
        && let Some(rows) = bigtext::render(key)
    {
        lines.push(Line::raw(""));
        lines.extend(rows.into_iter().map(|row| Line::styled(row, title_style())));
    }
    lines
}

/// `relish status`'s verdict and lines, the ones that need attention
/// first, so an 80 by 25 screen shows them before the quiet ones.
fn summary_lines(summary: &ClusterSummary, wtf: Option<&WtfCheck>) -> Vec<Line<'static>> {
    let mut assessed = verdict::section_lines(summary);
    let wtf_line = match wtf {
        Some(check) => verdict::assess(summary, check).pop(),
        None => Some(verdict::Line {
            section: "wtf",
            text: "running its checks...".to_string(),
            mark: Mark::Quiet,
            next: None,
        }),
    };
    assessed.extend(wtf_line);
    let overall = verdict::verdict(&assessed);
    let problems = match verdict::problems(&assessed) {
        0 => "no problems".to_string(),
        1 => "1 problem".to_string(),
        n => format!("{n} problems"),
    };
    let colour = match overall {
        Verdict::Healthy => GOOD,
        Verdict::Degraded => WARNING,
        Verdict::Down => BAD,
    };
    let mut lines = vec![Line::from(vec![
        Span::styled("Cluster ", title_style()),
        Span::styled(
            overall.word(),
            Style::default().fg(colour).add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            ", {problems} ({} answered at {})",
            summary.answered_by,
            clock(summary.generated_at_unix_ms / 1000)
        )),
    ])];
    // `wtf` first, then the worst sections; a stable sort keeps the
    // summary's own order among equals.
    assessed.sort_by_key(|line| (line.section != "wtf", std::cmp::Reverse(line.mark)));
    for line in assessed {
        let style = match line.mark {
            Mark::Quiet => Style::default(),
            Mark::Note | Mark::Problem => Style::default().fg(WARNING),
            Mark::Down => Style::default().fg(BAD),
        };
        lines.push(Line::from(vec![
            Span::raw(format!("{:<LABEL$}", line.section)),
            Span::styled(line.text, style),
        ]));
    }
    lines
}

// --- 2 Metrics ---

/// From quiet to busy, one ASCII character per sample.
const LEVELS: &[u8] = b" .:-=+*#";

/// One character per value, scaled so `top` is the busiest.
fn history(values: impl Iterator<Item = f64>, top: f64) -> String {
    values
        .map(|value| {
            let share = if top > 0.0 {
                (value / top).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let index = (share * (LEVELS.len() - 1) as f64).round() as usize;
            char::from(LEVELS[index])
        })
        .collect()
}

fn gauge(percent: f64) -> String {
    const CELLS: usize = 30;
    let filled = ((percent / 100.0).clamp(0.0, 1.0) * CELLS as f64).round() as usize;
    format!(
        "[{}{}] {:>3.0}%",
        "#".repeat(filled),
        ".".repeat(CELLS - filled),
        percent.clamp(0.0, 100.0)
    )
}

fn metrics_lines(state: &ConsoleState, width: usize) -> Vec<Line<'static>> {
    let Some(latest) = state.samples.back() else {
        return vec![muted("measuring: one sample every 2 s...")];
    };
    let columns = width.saturating_sub(LABEL);
    let recent = || {
        let skip = state.samples.len().saturating_sub(columns);
        state.samples.iter().skip(skip)
    };
    let percent = |used: u64, total: u64| {
        if total == 0 {
            0.0
        } else {
            used as f64 * 100.0 / total as f64
        }
    };
    let history_line = |values: String| muted(format!("{:<LABEL$}{values}", ""));
    let mut lines = vec![
        labelled("CPU", gauge(f64::from(latest.cpu_percent))),
        history_line(history(
            recent().map(|sample| f64::from(sample.cpu_percent)),
            100.0,
        )),
        Line::raw(""),
        labelled(
            "Memory",
            format!(
                "{}  {} of {}",
                gauge(percent(latest.memory_used_bytes, latest.memory_total_bytes)),
                bytes(latest.memory_used_bytes),
                bytes(latest.memory_total_bytes)
            ),
        ),
        history_line(history(
            recent().map(|s| percent(s.memory_used_bytes, s.memory_total_bytes)),
            100.0,
        )),
        Line::raw(""),
    ];
    match latest.disk {
        Some((used, total)) => {
            lines.push(labelled(
                "Disk",
                format!(
                    "{}  {} of {}",
                    gauge(percent(used, total)),
                    bytes(used),
                    bytes(total)
                ),
            ));
            lines.push(history_line(history(
                recent().map(|s| s.disk.map_or(0.0, |(used, total)| percent(used, total))),
                100.0,
            )));
        }
        None => lines.push(labelled("Disk", "can't measure the data partition")),
    }
    lines.push(Line::raw(""));
    lines.push(labelled(
        "Network",
        format!(
            "in {}/s, out {}/s",
            bytes(latest.receive_bytes_per_second),
            bytes(latest.transmit_bytes_per_second)
        ),
    ));
    let busiest = |pick: fn(&Sample) -> u64| recent().map(pick).max().unwrap_or(0) as f64;
    let receive = |sample: &Sample| sample.receive_bytes_per_second;
    let transmit = |sample: &Sample| sample.transmit_bytes_per_second;
    lines.push(muted(format!(
        "{:<LABEL$}{}",
        "  in",
        history(recent().map(|s| receive(s) as f64), busiest(receive))
    )));
    lines.push(muted(format!(
        "{:<LABEL$}{}",
        "  out",
        history(recent().map(|s| transmit(s) as f64), busiest(transmit))
    )));
    lines.push(Line::raw(""));
    lines.push(muted(format!(
        "history: the last {} s, oldest on the left",
        recent().count() as u64 * super::local::SAMPLE_EVERY.as_secs()
    )));
    lines
}

/// `bytes` in the largest binary unit that keeps it at or above 1.
pub fn bytes(count: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = count as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{count} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

// --- 3 Workloads ---

fn bun_down_line(what: &str) -> Line<'static> {
    warning(format!(
        "bun isn't answering on this node, so there's no {what}."
    ))
}

fn workload_lines(state: &ConsoleState, height: usize) -> Vec<Line<'static>> {
    if !state.local.bun_up() {
        return vec![bun_down_line("list of workloads")];
    }
    let mut lines: Vec<Line<'static>> = stale_line(&state.workloads, "the workloads")
        .into_iter()
        .collect();
    let Some(rows) = &state.workloads.value else {
        if lines.is_empty() {
            lines.push(muted("asking bun for this node's workloads..."));
        }
        return lines;
    };
    if rows.is_empty() {
        lines.push(Line::raw("No workloads run on this node."));
        return lines;
    }
    lines.push(Line::styled(
        format!(
            "{:<26}{:<14}{:<11}{:>8}{:>7}{:>11}",
            "INSTANCE", "NAMESPACE", "STATE", "RESTARTS", "CPU", "MEMORY"
        ),
        title_style(),
    ));
    let room = height.saturating_sub(lines.len());
    for row in rows.iter().skip(state.scroll).take(room) {
        let instance = &row.instance;
        let style = match instance.state.as_str() {
            "running" => Style::default(),
            "pending" | "preparing" | "starting" => Style::default().fg(WARNING),
            _ => Style::default().fg(BAD),
        };
        let cpu = row
            .cpu_percent
            .map_or("-".to_string(), |cpu| format!("{cpu:.1}%"));
        let memory = row.memory_bytes.map_or("-".to_string(), bytes);
        lines.push(Line::styled(
            format!(
                "{:<26}{:<14}{:<11}{:>8}{:>7}{:>11}",
                fit(&instance.id, 25),
                fit(&instance.namespace, 13),
                fit(&instance.state, 10),
                instance.restart_count,
                cpu,
                memory
            ),
            style,
        ));
    }
    lines
}

/// `text` cut to `width` characters, the last one a `~` when it was cut.
fn fit(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        text.to_string()
    } else {
        text.chars()
            .take(width.saturating_sub(1))
            .chain(['~'])
            .collect()
    }
}

/// `text` cut to `width` columns from the left, marked with `~`, so the end
/// survives: an invader's label ends with what tells it apart (`web-0`).
fn fit_tail(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count <= width {
        return text.to_string();
    }
    std::iter::once('~')
        .chain(text.chars().skip(count + 1 - width))
        .collect()
}

// --- 4 Cluster ---

fn cluster_lines(state: &ConsoleState, height: usize) -> Vec<Line<'static>> {
    if state.local.node.is_none() {
        return vec![Line::raw(
            "This machine isn't part of a cluster yet: see 1 Status.",
        )];
    }
    if !state.local.bun_up() {
        return vec![bun_down_line("view of the cluster")];
    }
    let mut lines = Vec::new();
    lines.extend(stale_line(&state.summary, "the cluster summary"));
    match &state.summary.value {
        Some(summary) => lines.extend(gossip_and_raft(summary, state.now)),
        None if state.summary.error.is_none() => {
            lines.push(muted("asking bun for the cluster summary..."));
        }
        None => {}
    }
    lines.push(Line::raw(""));
    lines.extend(stale_line(&state.council, "the council"));
    let Some(observations) = &state.council.value else {
        if state.council.error.is_none() {
            lines.push(muted("asking every node about the council..."));
        }
        return lines;
    };
    lines.push(Line::styled(
        format!(
            "{:<20}{:<12}{:>6}{:>10}{:>10}{:>8}",
            "NODE", "ROLE", "TERM", "APPLIED", "LOG", "LAG"
        ),
        title_style(),
    ));
    let furthest = observations
        .iter()
        .filter(|node| node.error.is_none())
        .filter_map(|node| node.last_applied)
        .max();
    let room = height.saturating_sub(lines.len());
    for node in observations.iter().skip(state.scroll).take(room) {
        if let Some(error) = &node.error {
            lines.push(Line::styled(
                format!("{:<20}unreachable: {error}", fit(&node.node_id, 19)),
                Style::default().fg(BAD),
            ));
            continue;
        }
        let number = |value: Option<u64>| value.map_or("-".to_string(), |v| v.to_string());
        let lag = match (furthest, node.last_applied) {
            (Some(furthest), Some(applied)) => Some(furthest.saturating_sub(applied)),
            _ => None,
        };
        let style = if lag.is_some_and(|lag| lag > crate::relish::council_view::VOTER_LAG_WARNING) {
            Style::default().fg(WARNING)
        } else {
            Style::default()
        };
        lines.push(Line::styled(
            format!(
                "{:<20}{:<12}{:>6}{:>10}{:>10}{:>8}",
                fit(&node.node_id, 19),
                node.role.as_str(),
                node.term,
                number(node.last_applied),
                number(node.last_log_index),
                number(lag)
            ),
            style,
        ));
    }
    lines
}

fn gossip_and_raft(summary: &ClusterSummary, now: u64) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    match (&summary.nodes.value, &summary.gossip.value) {
        (Some(nodes), Some(gossip)) => {
            lines.push(labelled(
                "Gossip",
                format!(
                    "{} members: {} alive, {} suspect, {} dead",
                    gossip.members, nodes.alive, nodes.suspect, nodes.dead
                ),
            ));
            let change = gossip.last_change_unix_ms.map_or_else(
                || "no change since bun started".to_string(),
                |at| {
                    format!(
                        "last change {} ago",
                        verdict::span((now * 1000).saturating_sub(at))
                    )
                },
            );
            lines.push(labelled(
                "",
                format!(
                    "{} updates received, {change}",
                    verdict::grouped(gossip.updates_received)
                ),
            ));
        }
        _ => lines.push(labelled("Gossip", "unknown: the summary has no gossip")),
    }
    match &summary.council.value {
        Some(council) => {
            let leader = council.leader.as_deref().unwrap_or("none");
            let quorum = if council.quorum_ok {
                "quorum ok"
            } else {
                "NO QUORUM"
            };
            lines.push(labelled(
                "Raft",
                format!(
                    "leader {leader}, term {}, {quorum} ({} of {} voters answer)",
                    council.term, council.voters_answering, council.voters
                ),
            ));
        }
        None => lines.push(labelled("Raft", "unknown: the summary has no council")),
    }
    lines
}

// --- 5 Versions ---

fn version_lines(state: &ConsoleState) -> Vec<Line<'static>> {
    let Some(versions) = &state.versions else {
        return vec![muted("reading versions...")];
    };
    let known = |value: &Option<String>| value.clone().unwrap_or_else(|| "unknown".into());
    let running = state
        .summary
        .value
        .as_ref()
        .filter(|summary| Some(&summary.answered_by) == state.local.node.as_ref())
        .map(|summary| summary.version.clone());
    let bun = match (&versions.bun, running) {
        (Some(disk), Some(running)) if *disk != running => {
            format!("{disk} on disk, {running} running")
        }
        (installed, _) => known(installed),
    };
    vec![
        labelled("OS image", known(&versions.os_image)),
        labelled("Kernel", known(&versions.kernel)),
        labelled("bun", bun),
        labelled("runc", known(&versions.runc)),
        labelled("systemd", known(&versions.systemd)),
        labelled(
            "Buildah",
            versions
                .buildah
                .clone()
                .unwrap_or_else(|| "not installed".into()),
        ),
    ]
}

// --- 6 Logs ---

fn log_lines(state: &ConsoleState, height: usize) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> =
        stale_line(&state.logs, "the journal").into_iter().collect();
    let Some(entries) = &state.logs.value else {
        if lines.is_empty() {
            lines.push(muted("reading the journal..."));
        }
        return lines;
    };
    if entries.is_empty() {
        lines.push(Line::raw(format!(
            "No journal lines yet for {}.",
            super::local::LOG_UNITS.join(" or ")
        )));
        return lines;
    }
    let room = height.saturating_sub(lines.len());
    let end = entries.len().saturating_sub(state.scroll);
    let start = end.saturating_sub(room);
    lines.extend(entries[start..end].iter().map(|entry| {
        let style = if entry.contains("not healthy") || entry.contains("error") {
            Style::default().fg(BAD)
        } else {
            Style::default()
        };
        Line::styled(entry.clone(), style)
    }));
    lines
}

// --- 7 Invaders ---

/// The invader sprite, two lines of six columns.
const SPRITE: [&str; 2] = ["_/OO\\_", "\\/  \\/"];

/// The game: whether kill mode is on, the formation, the cannon and what
/// the last shot did.
fn invader_lines(state: &ConsoleState, height: usize) -> Vec<Line<'static>> {
    use super::invaders::{CELL_WIDTH, FIELD_WIDTH, MAX_ROWS, PER_ROW};

    let mut lines = vec![kill_mode_line(state), Line::raw("")];
    if !state.local.bun_up() {
        lines.push(bun_down_line("list of workloads to play with"));
        return lines;
    }
    let Some(rows) = &state.workloads.value else {
        lines.push(muted("asking bun for this node's workloads..."));
        return lines;
    };
    let invaders = state.invaders();
    let colour = if state.kill_mode() { BAD } else { GOOD };
    let label_width = CELL_WIDTH - 1;
    let rows_used = invaders.len().div_ceil(PER_ROW).min(MAX_ROWS);
    for row in 0..rows_used {
        let mut field = [
            vec![' '; FIELD_WIDTH],
            vec![' '; FIELD_WIDTH],
            vec![' '; FIELD_WIDTH],
        ];
        for (index, invader) in invaders.iter().enumerate() {
            let Some((at_row, column)) = state.game.position(index) else {
                break;
            };
            if at_row != row {
                continue;
            }
            let sprite_at = column + (label_width - SPRITE[0].len()) / 2;
            for (line, sprite) in SPRITE.iter().enumerate() {
                paint(&mut field[line], sprite_at, sprite);
            }
            let label = fit_tail(&invader.label, label_width);
            let label_at = column + (label_width - label.chars().count()) / 2;
            paint(&mut field[2], label_at, &label);
        }
        for line in field {
            lines.push(Line::styled(
                line.into_iter().collect::<String>().trim_end().to_string(),
                Style::default().fg(colour),
            ));
        }
        lines.push(Line::raw(""));
    }
    // The cannon sits on the last line but one; the laser climbs to it.
    let filler = height.saturating_sub(lines.len() + 2);
    let laser = state
        .game
        .laser
        .map(|column| format!("{}|", " ".repeat(column)));
    for _ in 0..filler {
        lines.push(Line::raw(laser.clone().unwrap_or_default()));
    }
    let mut cannon = vec![' '; FIELD_WIDTH];
    paint(&mut cannon, state.game.cannon.saturating_sub(1), "/A\\");
    lines.push(Line::styled(
        cannon
            .into_iter()
            .collect::<String>()
            .trim_end()
            .to_string(),
        title_style(),
    ));
    let waiting = invaders.len().saturating_sub(PER_ROW * MAX_ROWS);
    lines.push(shot_line(
        state,
        rows.is_empty(),
        invaders.is_empty(),
        waiting,
    ));
    lines
}

/// Write `text` into `line` from column `at`, as far as the line goes.
fn paint(line: &mut [char], at: usize, text: &str) {
    for (cell, character) in line.iter_mut().skip(at).zip(text.chars()) {
        *cell = character;
    }
}

fn kill_mode_line(state: &ConsoleState) -> Line<'static> {
    if state.kill_mode() {
        return Line::styled(
            "KILL MODE ON: a hit kills that instance, as an audited Smoker fault.",
            Style::default().fg(BAD).add_modifier(Modifier::BOLD),
        );
    }
    match &state.settings.error {
        Some(error) if state.settings.value.is_none() => muted(format!(
            "Kill mode off (can't read the cluster's setting: {error}). A hit only pops."
        )),
        _ => muted("Kill mode off: a hit pops the invader and touches nothing."),
    }
}

/// What the last shot did, or what there is to shoot at.
fn shot_line(
    state: &ConsoleState,
    no_workloads: bool,
    cleared: bool,
    waiting: usize,
) -> Line<'static> {
    use super::invaders::Shot;
    let waiting_on_bun = matches!(
        state.game.last,
        Some(Shot::Sending { .. } | Shot::Refused { .. } | Shot::Unknown { .. })
    );
    if cleared && !no_workloads && !waiting_on_bun {
        return Line::raw("Wave cleared. r brings every invader back.");
    }
    match &state.game.last {
        Some(Shot::Missed) => muted("Missed."),
        Some(Shot::Popped { instance }) => {
            Line::raw(format!("Pop! {instance} is gone until the next wave."))
        }
        Some(Shot::Sending { instance }) => {
            warning(format!("Hit {instance}: asking Smoker to kill it..."))
        }
        Some(Shot::Killed { instance, fault_id }) => Line::styled(
            format!("Smoker killed {instance} (fault {fault_id}; see relish fault list)."),
            Style::default().fg(BAD),
        ),
        Some(Shot::Refused { instance, reason }) => {
            warning(format!("{instance} survived: {reason}"))
        }
        Some(Shot::Unknown { instance, reason }) => warning(format!(
            "{instance}: no clear answer ({reason}); holding fire until the list shows it"
        )),
        Some(Shot::Gone { instance }) => Line::styled(
            format!("Smoker killed {instance} (see relish fault list)."),
            Style::default().fg(BAD),
        ),
        None if no_workloads => Line::raw("No workloads run on this node: nothing to shoot."),
        None if waiting > 0 => muted(format!("{waiting} more waiting off the field.")),
        None => muted("One invader for each instance on this node."),
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;
    use crate::bun::agent::{CouncilRole, InstanceStatus};
    use crate::bun::top::TopRow;
    use crate::relish::council_view::CouncilNodeObservation;
    use crate::relish::tui::fixtures::{FIXTURE_NOW, TestScenario};

    use super::super::local::Versions;
    use super::super::state::Msg;

    fn screen(state: &ConsoleState, width: u16, height: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| render(frame, state)).unwrap();
        let buffer = terminal.backend().buffer();
        let mut output = String::new();
        for y in 0..buffer.area.height {
            let mut row = String::new();
            for x in 0..buffer.area.width {
                row.push_str(buffer[(x, y)].symbol());
            }
            output.push_str(row.trim_end());
            output.push('\n');
        }
        output
    }

    /// The Linux console's own size.
    fn console(state: &ConsoleState) -> String {
        screen(state, 80, 25)
    }

    fn running_node() -> LocalStatus {
        LocalStatus {
            os_version: Some("2026.47.228".into()),
            bun_version: Some("0.3.3".into()),
            node: Some("node-1".into()),
            cluster: Some("home".into()),
            address: Some("192.168.1.52".into()),
            macs: vec!["d8:9e:f3:12:34:56".into()],
            healthy: Some(true),
            claim_key: None,
            notes: vec![],
        }
    }

    fn healthy_state(tab: Tab) -> ConsoleState {
        let mut state = ConsoleState {
            tab,
            local: running_node(),
            now: FIXTURE_NOW,
            ..ConsoleState::default()
        };
        let summary = TestScenario::HealthyCluster.data().summary.unwrap();
        state.update(Msg::Summary(Box::new(Ok(summary))));
        // What a real console gets: `wtf` as `__console`, whose token list
        // is refused, counted the console's way.
        state.update(Msg::Wtf(super::super::remote::console_check(
            &crate::relish::wtf::WtfReport {
                schema_version: 1,
                cluster_name: "home".into(),
                collected_at: FIXTURE_NOW,
                node_count: 3,
                critical: vec![],
                warnings: vec![],
                unknown: vec![crate::relish::wtf::WtfUnknown {
                    source: "tokens".into(),
                    reason: "token lifetimes need an unscoped admin credential".into(),
                    affected_resource: "cluster".into(),
                }],
                ok: vec![],
                summary: crate::relish::wtf::WtfSummary {
                    critical_count: 0,
                    warning_count: 0,
                    unknown_count: 1,
                    ok_count: 30,
                },
            },
        )));
        state.update(Msg::Workloads(Ok(workloads())));
        state.update(Msg::Council(Ok(council())));
        state.update(Msg::Versions(Versions {
            os_image: Some("2026.47.228".into()),
            kernel: Some("6.8.0-48-generic".into()),
            bun: Some("0.3.3".into()),
            runc: Some("1.1.12".into()),
            systemd: Some("255 (255.4-1ubuntu8)".into()),
            buildah: Some("1.33.7".into()),
        }));
        for index in 0..50u64 {
            state.update(Msg::Sample(Sample {
                cpu_percent: (index * 2) as f32,
                memory_used_bytes: 1_200_000_000 + index * 10_000_000,
                memory_total_bytes: 4_000_000_000,
                disk: Some((4_500_000_000, 15_000_000_000)),
                receive_bytes_per_second: (index % 10) * 1_000,
                transmit_bytes_per_second: 800,
            }));
        }
        state.update(Msg::Logs(Ok((0..40)
            .map(|index| format!("{} bun: line {index}", clock(FIXTURE_NOW - 40 + index)))
            .collect())));
        state
    }

    /// Instance `label` (`web-0`) of an app in `default`, with the id bun
    /// gives it (`default__web-0`).
    fn instance(label: &str, state: &str, restarts: u32) -> InstanceStatus {
        InstanceStatus {
            id: format!("default__{label}"),
            app_name: label.split('-').next().unwrap_or(label).into(),
            namespace: "default".into(),
            state: state.into(),
            restart_count: restarts,
            host_port: None,
            exit_code: None,
            pid: None,
            runtime_unknown: false,
            status_age_ms: None,
            age_seconds: None,
            image: None,
            data_on_node: false,
            reason: None,
            sidecars: Vec::new(),
        }
    }

    fn workloads() -> Vec<TopRow> {
        vec![
            TopRow {
                node: "node-1".into(),
                instance: instance("web-0", "running", 0),
                cpu_percent: Some(1.5),
                memory_bytes: Some(48 * 1024 * 1024),
                tasks: None,
            },
            TopRow {
                node: "node-1".into(),
                instance: instance("worker-1", "unhealthy", 4),
                cpu_percent: Some(92.0),
                memory_bytes: Some(700 * 1024 * 1024),
                tasks: None,
            },
            TopRow {
                node: "node-1".into(),
                instance: instance("burger-0", "pending", 0),
                cpu_percent: None,
                memory_bytes: None,
                tasks: None,
            },
        ]
    }

    fn observation(node: &str, role: CouncilRole, applied: u64) -> CouncilNodeObservation {
        let status = crate::bun::agent::CouncilStatus {
            members: vec![],
            leader: Some("node-1".into()),
            term: 4,
            last_applied_log: Some(applied),
            app_count: 3,
            role,
            recovery_epoch: None,
            fenced_by: None,
            recovery_id: None,
            split_with: None,
            fence_persist_error: None,
            last_log_index: Some(1_350),
            council_size: None,
        };
        CouncilNodeObservation::answered(node, &status)
    }

    fn council() -> Vec<CouncilNodeObservation> {
        vec![
            observation("node-1", CouncilRole::Leader, 1_350),
            observation("node-2", CouncilRole::Follower, 1_349),
            observation("node-3", CouncilRole::Follower, 1_100),
        ]
    }

    #[test]
    fn status_on_a_healthy_node() {
        insta::assert_snapshot!(console(&healthy_state(Tab::Status)));
    }

    #[test]
    fn status_before_wtf_finishes() {
        let mut state = healthy_state(Tab::Status);
        state.wtf = None;
        insta::assert_snapshot!(console(&state));
    }

    #[test]
    fn status_on_a_large_screen() {
        // A Wyse's framebuffer console at 1920 by 1080 with an 8 by 16 font.
        insta::assert_snapshot!(screen(&healthy_state(Tab::Status), 240, 67));
    }

    #[test]
    fn status_falls_back_to_the_plain_screen_when_bun_is_down() {
        let mut state = healthy_state(Tab::Status);
        state.local.healthy = None;
        let output = console(&state);
        assert!(!output.contains("Cluster HEALTHY"), "{output}");
        insta::assert_snapshot!(output);
    }

    #[test]
    fn status_of_an_unclaimed_machine_shows_its_key_large() {
        let state = ConsoleState {
            local: LocalStatus {
                os_version: Some("2026.47.228".into()),
                bun_version: Some("0.3.3".into()),
                address: Some("192.168.1.60".into()),
                macs: vec!["d8:9e:f3:12:34:57".into()],
                claim_key: Some("3f9a-12bc-77de-0a41".into()),
                ..LocalStatus::default()
            },
            now: FIXTURE_NOW,
            ..ConsoleState::default()
        };
        let output = console(&state);
        // What image/tests/claimed-pair.sh looks for on tty1, spaces squeezed.
        let squeezed = output.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            squeezed.contains("Claim key 3f9a-12bc-77de-0a41"),
            "{output}"
        );
        insta::assert_snapshot!(output);
    }

    #[test]
    fn status_of_an_unseeded_machine_says_how_to_seed_it() {
        let state = ConsoleState {
            now: FIXTURE_NOW,
            ..ConsoleState::default()
        };
        insta::assert_snapshot!(console(&state));
    }

    #[test]
    fn status_when_the_summary_fails() {
        let mut state = healthy_state(Tab::Status);
        state.summary = Fetched::default();
        state.update(Msg::Summary(Box::new(Err("no answer within 10 s".into()))));
        insta::assert_snapshot!(console(&state));
    }

    #[test]
    fn status_with_a_node_missing_from_the_summary() {
        let mut state = healthy_state(Tab::Status);
        let summary = TestScenario::NodeMissing.data().summary.unwrap();
        state.update(Msg::Summary(Box::new(Ok(summary))));
        insta::assert_snapshot!(console(&state));
    }

    #[test]
    fn a_session_over_ssh_says_q_quits() {
        let mut state = healthy_state(Tab::Versions);
        state.can_quit = true;
        assert!(console(&state).contains("q quit"));
    }

    #[test]
    fn an_older_images_console_says_it_takes_no_keys() {
        let mut state = healthy_state(Tab::Status);
        state.keyless = true;
        assert!(console(&state).contains("takes no keys"));
    }

    #[test]
    fn metrics_with_a_history() {
        insta::assert_snapshot!(console(&healthy_state(Tab::Metrics)));
    }

    #[test]
    fn metrics_before_the_first_sample() {
        let mut state = healthy_state(Tab::Metrics);
        state.samples.clear();
        insta::assert_snapshot!(console(&state));
    }

    #[test]
    fn workloads_on_this_node() {
        insta::assert_snapshot!(console(&healthy_state(Tab::Workloads)));
    }

    #[test]
    fn workloads_when_bun_is_down() {
        let mut state = healthy_state(Tab::Workloads);
        state.local.healthy = None;
        insta::assert_snapshot!(console(&state));
    }

    #[test]
    fn workloads_that_failed_to_refresh_keep_the_last_list() {
        let mut state = healthy_state(Tab::Workloads);
        state.update(Msg::Tick(FIXTURE_NOW + 30));
        state.update(Msg::Workloads(Err("no answer within 10 s".into())));
        insta::assert_snapshot!(console(&state));
    }

    /// The workloads the game tests shoot at: four running instances, two
    /// of them `web-0` in different namespaces, and the fixture's unhealthy
    /// and pending ones, which get no invader.
    fn invader_workloads() -> Vec<TopRow> {
        let mut rows = workloads();
        for label in ["web-1", "api-0"] {
            rows.push(TopRow {
                node: "node-1".into(),
                instance: instance(label, "running", 0),
                cpu_percent: None,
                memory_bytes: None,
                tasks: None,
            });
        }
        let mut other = instance("web-0", "running", 0);
        other.id = "team-b__web-0".into();
        other.namespace = "team-b".into();
        rows.push(TopRow {
            node: "node-1".into(),
            instance: other,
            cpu_percent: None,
            memory_bytes: None,
            tasks: None,
        });
        rows
    }

    #[test]
    fn invaders_with_kill_mode_off() {
        let mut state = healthy_state(Tab::Invaders);
        state.update(Msg::Workloads(Ok(invader_workloads())));
        state.update(Msg::Settings(Ok(Default::default())));
        insta::assert_snapshot!(console(&state));
    }

    /// Kill mode on, after bun refused a kill: the invader is back, and the
    /// last line says why.
    #[test]
    fn invaders_in_kill_mode_after_a_refused_kill() {
        let mut state = healthy_state(Tab::Invaders);
        state.update(Msg::Workloads(Ok(invader_workloads())));
        state.update(Msg::Settings(Ok(
            crate::appliance::settings::ApplianceSettings {
                console_chaos: true,
            },
        )));
        state.game.cannon = 3;
        state.update(Msg::Key(KeyEvent::new(
            KeyCode::Char(' '),
            KeyModifiers::NONE,
        )));
        let kill = state.take_kill().expect("kill mode queues a kill");
        state.update(Msg::KillAnswered {
            instance: kill.instance,
            answer: super::super::invaders::KillAnswer::Refused {
                reason: "safety check failed: web would have no replica left".into(),
            },
        });
        insta::assert_snapshot!(console(&state));
    }

    #[test]
    fn cluster_with_a_lagging_voter() {
        insta::assert_snapshot!(console(&healthy_state(Tab::Cluster)));
    }

    #[test]
    fn cluster_with_a_node_unreachable() {
        let mut state = healthy_state(Tab::Cluster);
        let mut observations = council();
        observations[2] = CouncilNodeObservation::unanswered("node-3", "gossip reports it dead");
        state.update(Msg::Council(Ok(observations)));
        // The summary agrees: gossip has node-3 dead, and two voters answer.
        let mut summary = state.summary.value.clone().unwrap();
        if let Some(nodes) = &mut summary.nodes.value {
            nodes.alive = 2;
            nodes.dead = 1;
        }
        if let Some(council) = &mut summary.council.value {
            council.voters_answering = 2;
        }
        state.update(Msg::Summary(Box::new(Ok(summary))));
        insta::assert_snapshot!(console(&state));
    }

    #[test]
    fn cluster_before_the_machine_has_one() {
        let state = ConsoleState {
            tab: Tab::Cluster,
            now: FIXTURE_NOW,
            ..ConsoleState::default()
        };
        insta::assert_snapshot!(console(&state));
    }

    #[test]
    fn versions_of_everything() {
        insta::assert_snapshot!(console(&healthy_state(Tab::Versions)));
    }

    #[test]
    fn versions_without_buildah_and_a_newer_bun_on_disk() {
        let mut state = healthy_state(Tab::Versions);
        if let Some(versions) = &mut state.versions {
            versions.buildah = None;
            versions.bun = Some("0.3.4".into());
        }
        if let Some(summary) = &mut state.summary.value {
            summary.answered_by = "node-1".into();
            summary.version = "0.3.3".into();
        }
        insta::assert_snapshot!(console(&state));
    }

    #[test]
    fn logs_newest_at_the_bottom() {
        insta::assert_snapshot!(console(&healthy_state(Tab::Logs)));
    }

    #[test]
    fn logs_scrolled_back() {
        let mut state = healthy_state(Tab::Logs);
        for _ in 0..2 {
            state.update(Msg::Key(KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE)));
        }
        let output = console(&state);
        assert!(output.contains("line 19"), "{output}");
        assert!(!output.contains("line 39"), "{output}");
    }

    #[test]
    fn logs_when_journalctl_fails() {
        let mut state = healthy_state(Tab::Logs);
        state.logs = Fetched::default();
        state.update(Msg::Logs(Err("can't run journalctl: not found".into())));
        insta::assert_snapshot!(console(&state));
    }

    #[test]
    fn every_screen_is_ascii_box_drawing_or_full_blocks() {
        let allowed =
            |c: char| c.is_ascii() || ('\u{2500}'..='\u{257f}').contains(&c) || c == '\u{2588}';
        let mut states: Vec<ConsoleState> =
            Tab::ALL.iter().map(|tab| healthy_state(*tab)).collect();
        states.push(ConsoleState {
            local: LocalStatus {
                claim_key: Some("3f9a-12bc-77de-0a41".into()),
                ..LocalStatus::default()
            },
            ..ConsoleState::default()
        });
        for state in states {
            let output = console(&state);
            let odd: Vec<char> = output.chars().filter(|c| !allowed(*c)).collect();
            assert!(odd.is_empty(), "{odd:?} on\n{output}");
        }
    }

    #[test]
    fn history_scales_to_its_top() {
        assert_eq!(history([0.0, 50.0, 100.0].into_iter(), 100.0), " =#");
        assert_eq!(history([5.0, 10.0].into_iter(), 0.0), "  ");
    }

    #[test]
    fn bytes_read_in_binary_units() {
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(48 * 1024 * 1024), "48.0 MiB");
        assert_eq!(bytes(4_000_000_000), "3.7 GiB");
    }
}
