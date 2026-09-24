//! `relish top`: every workload in the cluster with its latest CPU and memory.
//!
//! Each node already samples `process_cpu_percent` and `process_memory_bytes`
//! for its own instances into its own Mayo store, labelled with the app and
//! the host PID. PIDs only mean something on the node that owns them, so each
//! node joins its own statuses to its own samples, and the node the CLI talks
//! to merges the finished rows. That keeps a PID from one VM ever being
//! matched against a sample from another.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use super::agent::InstanceStatus;

/// Metric holding a process's CPU use, in percent of one core.
pub const CPU_METRIC: &str = "process_cpu_percent";
/// Metric holding a process's resident memory, in bytes.
pub const MEMORY_METRIC: &str = "process_memory_bytes";

/// How far back a node looks for a workload's latest sample.
pub const USAGE_WINDOW_SECS: u64 = 120;

/// One workload instance with its node and latest resource use.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopRow {
    /// Node that runs the instance.
    pub node: String,
    /// The instance's status on that node.
    #[serde(flatten)]
    pub instance: InstanceStatus,
    /// Latest CPU use, in percent of one core; `None` before the first sample.
    pub cpu_percent: Option<f64>,
    /// Latest resident memory in bytes; `None` before the first sample.
    pub memory_bytes: Option<u64>,
}

/// Every node's rows, plus one message per node that didn't answer.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClusterTop {
    /// Rows sorted by node, namespace and instance id.
    pub rows: Vec<TopRow>,
    /// Nodes whose rows are missing, and why.
    pub warnings: Vec<String>,
}

/// Latest CPU and memory sample for one process.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Usage {
    /// Latest `process_cpu_percent`.
    pub cpu_percent: Option<f64>,
    /// Latest `process_memory_bytes`.
    pub memory_bytes: Option<u64>,
}

/// Reduce `(timestamp, metric, labels JSON, value)` rows to the latest sample
/// of each metric per `(app label, pid)`.
pub fn latest_usage(rows: &[(u64, String, String, f64)]) -> HashMap<(String, u32), Usage> {
    let mut latest: HashMap<(String, u32), (u64, u64, Usage)> = HashMap::new();
    for (timestamp, metric, labels, value) in rows {
        let Ok(labels) = serde_json::from_str::<BTreeMap<String, String>>(labels) else {
            continue;
        };
        let (Some(app), Some(pid)) = (
            labels.get("app"),
            labels.get("pid").and_then(|pid| pid.parse::<u32>().ok()),
        ) else {
            continue;
        };
        let entry = latest.entry((app.clone(), pid)).or_default();
        let (cpu_at, memory_at, usage) = entry;
        match metric.as_str() {
            CPU_METRIC if *timestamp >= *cpu_at => {
                *cpu_at = *timestamp;
                usage.cpu_percent = Some(*value);
            }
            MEMORY_METRIC if *timestamp >= *memory_at => {
                *memory_at = *timestamp;
                usage.memory_bytes = Some(value.max(0.0) as u64);
            }
            _ => {}
        }
    }
    latest
        .into_iter()
        .map(|(key, (_, _, usage))| (key, usage))
        .collect()
}

/// Join one node's statuses to that node's usage samples.
pub fn node_rows(
    node: &str,
    statuses: Vec<InstanceStatus>,
    usage: &HashMap<(String, u32), Usage>,
) -> Vec<TopRow> {
    statuses
        .into_iter()
        .map(|instance| {
            let sample = instance
                .pid
                .and_then(|pid| {
                    usage.get(&(format!("{}/{}", instance.namespace, instance.app_name), pid))
                })
                .copied()
                .unwrap_or_default();
            TopRow {
                node: node.to_string(),
                instance,
                cpu_percent: sample.cpu_percent,
                memory_bytes: sample.memory_bytes,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(app: &str, pid: u32) -> String {
        format!(r#"{{"app":"{app}","pid":"{pid}"}}"#)
    }

    fn status(id: &str, pid: Option<u32>) -> InstanceStatus {
        InstanceStatus {
            id: id.to_string(),
            app_name: "web".to_string(),
            namespace: "default".to_string(),
            state: "running".to_string(),
            restart_count: 0,
            host_port: None,
            exit_code: None,
            pid,
        }
    }

    #[test]
    fn the_newest_sample_of_each_metric_wins() {
        let rows = vec![
            (20, CPU_METRIC.to_string(), labels("default/web", 7), 40.0),
            (10, CPU_METRIC.to_string(), labels("default/web", 7), 90.0),
            (
                15,
                MEMORY_METRIC.to_string(),
                labels("default/web", 7),
                2048.0,
            ),
            (
                15,
                "node_cpu_usage_percent".to_string(),
                "{}".to_string(),
                5.0,
            ),
        ];
        let usage = latest_usage(&rows);
        assert_eq!(
            usage[&("default/web".to_string(), 7)],
            Usage {
                cpu_percent: Some(40.0),
                memory_bytes: Some(2048),
            }
        );
        assert_eq!(usage.len(), 1);
    }

    #[test]
    fn rows_match_samples_by_app_and_pid_and_leave_gaps_empty() {
        let usage = latest_usage(&[
            (1, CPU_METRIC.to_string(), labels("default/web", 7), 12.5),
            (1, CPU_METRIC.to_string(), labels("other/web", 8), 99.0),
        ]);
        let rows = node_rows(
            "node-2",
            vec![
                status("default__web-0", Some(7)),
                status("default__web-1", Some(8)),
                status("default__web-2", None),
            ],
            &usage,
        );
        assert_eq!(rows[0].node, "node-2");
        assert_eq!(rows[0].cpu_percent, Some(12.5));
        // Same PID, different namespace: not this instance's sample.
        assert_eq!(rows[1].cpu_percent, None);
        assert_eq!(rows[2].cpu_percent, None);
        assert_eq!(rows[2].memory_bytes, None);
    }
}
