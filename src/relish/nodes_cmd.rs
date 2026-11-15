//! `relish nodes`: one row per node, problems first.
//!
//! Gossip's view of each node (its state, whether it votes or leads), what
//! the node says about itself (`/v1/cluster/summary/node`: the version it
//! runs, its CPU, memory and fullest data disk) and what the scheduler has
//! committed on it (`/v1/cluster/capacity`).

use std::fmt::Write as _;

use serde::Serialize;

use super::apps_cmd::ask_each_node;
use super::client::BunClient;
use super::{OutputFormat, RelishError};
use crate::bun::agent::NodeStatus;
use crate::bun::summary::{ClusterCapacity, DISK_PRESSURE_PERCENT, NodeCapacity, NodeSummary};

/// One node, as `relish nodes` lists it.
#[derive(Debug, Clone, Serialize)]
pub struct NodeRow {
    /// Gossip's view: name, addresses, state, council seat, labels.
    #[serde(flatten)]
    pub status: NodeStatus,
    /// The version the node runs, when it answered.
    pub version: Option<String>,
    /// The node's CPU use, percent.
    pub cpu_percent: Option<f64>,
    /// The node's memory use, percent.
    pub memory_percent: Option<f64>,
    /// Its fullest data path, percent.
    pub disk_percent: Option<f64>,
    /// What the scheduler has committed on it, when the leader said.
    pub allocated: Option<NodeCapacity>,
    /// Whether an upgrade has cordoned it.
    pub cordoned: bool,
    /// Whether a data path is at or above the disk-pressure threshold.
    pub disk_pressure: bool,
    /// Why the node's own answer is missing, when it is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unanswered: Option<String>,
}

impl NodeRow {
    /// Whether the row needs looking at: not alive, cordoned, under disk
    /// pressure, or silent.
    pub fn is_unhealthy(&self) -> bool {
        self.status.state != "alive"
            || self.cordoned
            || self.disk_pressure
            || self.unanswered.is_some()
    }
}

/// Run `relish nodes` against the configured endpoint.
pub async fn run(unhealthy: bool, output: OutputFormat) -> Result<(), RelishError> {
    run_with_client(unhealthy, output, &BunClient::default_local()).await
}

/// Run `relish nodes` with an explicit client.
pub async fn run_with_client(
    unhealthy: bool,
    output: OutputFormat,
    client: &BunClient,
) -> Result<(), RelishError> {
    let nodes = client.nodes().await?;
    if nodes.is_empty() {
        println!("no cluster nodes (single-node mode)");
        return Ok(());
    }
    let (capacity, (shares, unanswered)) = tokio::join!(
        client.cluster_capacity(),
        ask_each_node(
            client,
            &nodes,
            |node| async move { node.node_summary().await }
        ),
    );
    let capacity = match capacity {
        Ok(capacity) => Some(capacity),
        Err(error) => {
            eprintln!("warning: allocations unavailable: {error}");
            None
        }
    };
    let shares: Vec<NodeSummary> = shares.into_iter().map(|(_, share)| share).collect();
    let mut rows = node_rows(nodes, &shares, &unanswered, capacity.as_ref());
    if unhealthy {
        rows.retain(NodeRow::is_unhealthy);
    }
    match output {
        OutputFormat::Human => print!("{}", render_nodes(&rows)),
        OutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(&rows).map_err(RelishError::SerialiseJson)?
        ),
        OutputFormat::Yaml => print!(
            "{}",
            yaml_serde::to_string(&rows).map_err(RelishError::SerialiseYaml)?
        ),
    }
    Ok(())
}

/// One row per node, problems first, then by name.
pub fn node_rows(
    nodes: Vec<NodeStatus>,
    shares: &[NodeSummary],
    unanswered: &[(String, String)],
    capacity: Option<&ClusterCapacity>,
) -> Vec<NodeRow> {
    let mut rows: Vec<NodeRow> = nodes
        .into_iter()
        .map(|status| {
            let share = shares.iter().find(|s| s.node == status.node_id);
            let allocated = capacity
                .and_then(|c| c.nodes.iter().find(|n| n.node == status.node_id))
                .cloned();
            let usage = share.map(|s| &s.usage);
            let disk_percent = share.and_then(|s| s.data_disk_percent);
            let unanswered = unanswered
                .iter()
                .find(|(node, _)| *node == status.node_id)
                .map(|(_, why)| why.clone());
            NodeRow {
                version: share.map(|s| s.version.clone()),
                cpu_percent: usage.and_then(|u| u.cpu_percent),
                memory_percent: usage
                    .and_then(|u| ratio(u.memory_used_bytes, u.memory_total_bytes)),
                disk_percent,
                cordoned: allocated.as_ref().is_some_and(|a| a.cordoned),
                disk_pressure: disk_percent.is_some_and(|p| p >= DISK_PRESSURE_PERCENT),
                allocated,
                unanswered,
                status,
            }
        })
        .collect();
    rows.sort_by(|a, b| {
        b.is_unhealthy()
            .cmp(&a.is_unhealthy())
            .then_with(|| a.status.node_id.cmp(&b.status.node_id))
    });
    rows
}

fn ratio(used: Option<f64>, total: Option<f64>) -> Option<f64> {
    match (used, total) {
        (Some(used), Some(total)) if total > 0.0 => Some(used / total * 100.0),
        _ => None,
    }
}

fn percent(value: Option<f64>) -> String {
    match value {
        Some(value) => format!("{value:.0}%"),
        None => "-".to_string(),
    }
}

fn allocated_cell(allocated: Option<&NodeCapacity>) -> String {
    let Some(a) = allocated else {
        return "-".to_string();
    };
    let share = |used: u64, total: u64| {
        if total == 0 {
            "-".to_string()
        } else {
            format!("{:.0}%", used as f64 / total as f64 * 100.0)
        }
    };
    format!(
        "cpu {} mem {}",
        share(a.cpu_allocated_millicores, a.cpu_total_millicores),
        share(a.memory_allocated_mb, a.memory_total_mb)
    )
}

/// The state column: gossip's word, with a cordon or disk pressure after it.
fn state_cell(row: &NodeRow) -> String {
    let mut state = row.status.state.clone();
    if row.cordoned {
        state.push_str(",cordoned");
    }
    if row.disk_pressure {
        state.push_str(",disk");
    }
    state
}

/// The `relish nodes` table, then a line for each node that didn't answer.
pub fn render_nodes(rows: &[NodeRow]) -> String {
    if rows.is_empty() {
        return "no nodes match\n".to_string();
    }
    let mut out = format!(
        "{:<20} {:<22} {:<16} {:<8} {:<7} {:<10} {:>5} {:>5} {:>5}  {}\n",
        "NODE",
        "ADDRESS",
        "STATE",
        "COUNCIL",
        "LEADER",
        "VERSION",
        "CPU",
        "MEM",
        "DISK",
        "ALLOCATED"
    );
    for row in rows {
        let _ = writeln!(
            out,
            "{:<20} {:<22} {:<16} {:<8} {:<7} {:<10} {:>5} {:>5} {:>5}  {}",
            row.status.node_id,
            row.status.address,
            state_cell(row),
            if row.status.is_council { "yes" } else { "-" },
            if row.status.is_leader { "yes" } else { "-" },
            row.version.as_deref().unwrap_or("-"),
            percent(row.cpu_percent),
            percent(row.memory_percent),
            percent(row.disk_percent),
            allocated_cell(row.allocated.as_ref()),
        );
    }
    let silent: Vec<&NodeRow> = rows.iter().filter(|r| r.unanswered.is_some()).collect();
    if !silent.is_empty() {
        out.push('\n');
        for row in silent {
            let _ = writeln!(
                out,
                "{} didn't answer: {}",
                row.status.node_id,
                row.unanswered.as_deref().unwrap_or_default()
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bun::summary::{NodeCouncilView, NodeUsage};

    fn member(name: &str, state: &str, leader: bool) -> NodeStatus {
        NodeStatus {
            node_id: name.to_string(),
            address: format!("10.0.0.{}:7946", name.trim_start_matches("node-")),
            api_address: None,
            state: state.to_string(),
            incarnation: 1,
            is_council: true,
            is_leader: leader,
            labels: Default::default(),
        }
    }

    fn share(name: &str, cpu: f64, disk: f64) -> NodeSummary {
        NodeSummary {
            node: name.to_string(),
            version: "0.2.8".to_string(),
            council: NodeCouncilView::default(),
            apps: Vec::new(),
            newest_log_unix_ms: None,
            usage: NodeUsage {
                newest_sample_unix_ms: Some(1),
                cpu_percent: Some(cpu),
                memory_used_bytes: Some(4.0),
                memory_total_bytes: Some(16.0),
                disk_used_bytes: None,
                disk_total_bytes: None,
            },
            data_disk_percent: Some(disk),
            faults: Vec::new(),
            alerts: Vec::new(),
            certificate_not_after: None,
        }
    }

    fn capacity(name: &str, cordoned: bool) -> NodeCapacity {
        NodeCapacity {
            node: name.to_string(),
            cpu_allocated_millicores: 3_000,
            cpu_total_millicores: 4_000,
            memory_allocated_mb: 2_048,
            memory_total_mb: 8_192,
            gpu_allocated: 0,
            gpu_total: 0,
            cordoned,
        }
    }

    fn fixture() -> Vec<NodeRow> {
        node_rows(
            vec![
                member("node-1", "alive", true),
                member("node-2", "alive", false),
                member("node-3", "suspect", false),
                member("node-4", "alive", false),
            ],
            &[
                share("node-1", 34.0, 40.0),
                share("node-2", 81.0, 91.0),
                share("node-4", 12.0, 20.0),
            ],
            &[("node-3".to_string(), "no answer in 3 s".to_string())],
            Some(&ClusterCapacity {
                nodes: vec![
                    capacity("node-1", false),
                    capacity("node-2", false),
                    capacity("node-4", true),
                ],
                not_reporting: vec!["node-3".to_string()],
            }),
        )
    }

    #[test]
    fn nodes_show_version_usage_and_allocation_with_problems_first() {
        let rows = fixture();
        let order: Vec<&str> = rows.iter().map(|r| r.status.node_id.as_str()).collect();
        assert_eq!(order, ["node-2", "node-3", "node-4", "node-1"]);
        insta::assert_snapshot!(render_nodes(&rows));
    }

    #[test]
    fn unhealthy_means_not_alive_cordoned_under_disk_pressure_or_silent() {
        let rows: Vec<NodeRow> = fixture()
            .into_iter()
            .filter(NodeRow::is_unhealthy)
            .collect();
        let names: Vec<&str> = rows.iter().map(|r| r.status.node_id.as_str()).collect();
        assert_eq!(names, ["node-2", "node-3", "node-4"]);
    }

    #[test]
    fn json_keeps_gossips_fields_and_adds_the_new_ones() {
        let rows = fixture();
        let value = serde_json::to_value(&rows).unwrap();
        assert_eq!(value[0]["node_id"], "node-2");
        assert_eq!(value[0]["state"], "alive");
        assert_eq!(value[0]["version"], "0.2.8");
        assert_eq!(value[0]["disk_pressure"], true);
        assert_eq!(value[0]["memory_percent"], 25.0);
        assert_eq!(value[1]["unanswered"], "no answer in 3 s");
    }

    #[tokio::test]
    async fn nodes_returns_agent_unreachable() {
        let err = run_with_client(
            false,
            OutputFormat::Human,
            &BunClient::new("http://127.0.0.1:1"),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, RelishError::AgentUnreachable { .. }),
            "got: {err:?}"
        );
    }
}
