//! System metrics collector.
//!
//! Uses the `sysinfo` crate for cross-platform CPU, memory, disk,
//! and network metrics. Works on both Linux and macOS without
//! platform-specific code.

use std::collections::BTreeMap;

use sysinfo::{Disks, Networks, Pid, System};

use super::types::MetricKey;

/// Collected metric: a key + value pair ready for insertion into MayoStore.
pub struct CollectedMetric {
    pub key: MetricKey,
    pub value: f64,
}

/// One running workload instance whose process the collector samples.
///
/// Borrows its strings from the agent's status list for the length of
/// one collection tick; the `'a` lifetime says it can't outlive them.
#[derive(Debug, Clone, Copy)]
pub struct InstanceProcess<'a> {
    /// Host PID, when the runtime reports one.
    pub pid: Option<u32>,
    /// Namespace the app lives in.
    pub namespace: &'a str,
    /// App name.
    pub app: &'a str,
    /// Instance id, e.g. `default__web-0`.
    pub instance: &'a str,
}

/// Collects system and per-process metrics via sysinfo.
pub struct SystemCollector {
    system: System,
    networks: Networks,
    disks: Disks,
}

impl SystemCollector {
    /// Create a new collector. Performs an initial refresh to establish
    /// baselines (CPU usage needs two measurements to compute deltas).
    pub fn new() -> Self {
        // sysinfo keeps each tracked process's /proc stat file open to save
        // syscalls, bounded only by RLIMIT_NOFILE, which Bun raises to about
        // a million. Open and close them per refresh instead.
        sysinfo::set_open_files_limit(0);
        let mut system = System::new_all();
        system.refresh_all();
        let networks = Networks::new_with_refreshed_list();
        let disks = Disks::new_with_refreshed_list();
        Self {
            system,
            networks,
            disks,
        }
    }

    /// Refresh all system data. Call this before collecting metrics.
    pub fn refresh(&mut self) {
        // `refresh_all` never forgets an exited process. On a node that
        // starts short-lived processes all day, that grew Bun past 950 MB
        // and 1,500 open files within half an hour in the V02 soak.
        self.system.refresh_memory();
        self.system.refresh_cpu_all();
        self.system
            .refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        self.networks.refresh(true);
        self.disks.refresh(true);
    }

    /// Collect node-level metrics (CPU, memory, disk, network).
    pub fn collect_node_metrics(&self) -> Vec<CollectedMetric> {
        let mut metrics = Vec::new();

        // CPU usage (global average across all cores)
        metrics.push(CollectedMetric {
            key: MetricKey::simple("node_cpu_usage_percent"),
            value: self.system.global_cpu_usage() as f64,
        });

        // Memory
        metrics.push(CollectedMetric {
            key: MetricKey::simple("node_memory_used_bytes"),
            value: self.system.used_memory() as f64,
        });
        metrics.push(CollectedMetric {
            key: MetricKey::simple("node_memory_total_bytes"),
            value: self.system.total_memory() as f64,
        });

        // Disk (sum across all disks)
        let mut disk_used: u64 = 0;
        let mut disk_total: u64 = 0;
        for disk in self.disks.list() {
            disk_total += disk.total_space();
            disk_used += disk.total_space() - disk.available_space();
        }
        metrics.push(CollectedMetric {
            key: MetricKey::simple("node_disk_used_bytes"),
            value: disk_used as f64,
        });
        metrics.push(CollectedMetric {
            key: MetricKey::simple("node_disk_total_bytes"),
            value: disk_total as f64,
        });

        // Network (sum across all interfaces)
        let mut rx_bytes: u64 = 0;
        let mut tx_bytes: u64 = 0;
        let mut rx_packets: u64 = 0;
        let mut tx_packets: u64 = 0;
        for data in self.networks.values() {
            rx_bytes += data.total_received();
            tx_bytes += data.total_transmitted();
            rx_packets += data.total_packets_received();
            tx_packets += data.total_packets_transmitted();
        }
        metrics.push(CollectedMetric {
            key: MetricKey::simple("node_network_rx_bytes"),
            value: rx_bytes as f64,
        });
        metrics.push(CollectedMetric {
            key: MetricKey::simple("node_network_tx_bytes"),
            value: tx_bytes as f64,
        });
        metrics.push(CollectedMetric {
            key: MetricKey::simple("node_network_rx_packets"),
            value: rx_packets as f64,
        });
        metrics.push(CollectedMetric {
            key: MetricKey::simple("node_network_tx_packets"),
            value: tx_packets as f64,
        });

        metrics
    }

    /// Collect per-app process metrics for a batch of running instances.
    ///
    /// Instances without a PID are skipped. Metrics are labelled
    /// `namespace/app` under the `app` key, the shape the per-app query
    /// endpoint and autoscaler filter on, plus `namespace`, `instance` and
    /// `node`, the same labels scraped app metrics carry, so a chart can
    /// draw one line per instance. Extracted from the collection loop in the
    /// `bun` binary so the labelling logic is testable rather than buried in
    /// un-reachable glue (OBS3).
    pub fn collect_instance_metrics(
        &self,
        instances: &[InstanceProcess<'_>],
        node: &str,
    ) -> Vec<CollectedMetric> {
        let mut metrics = Vec::new();
        for instance in instances {
            let Some(pid) = instance.pid else { continue };
            let app_label = format!("{}/{}", instance.namespace, instance.app);
            for mut metric in self.collect_process_metrics(pid, &app_label) {
                let labels = &mut metric.key.labels;
                labels.insert("namespace".to_string(), instance.namespace.to_string());
                labels.insert("instance".to_string(), instance.instance.to_string());
                labels.insert("node".to_string(), node.to_string());
                metrics.push(metric);
            }
        }
        metrics
    }

    /// Ask the agent which instances it runs and collect their per-process
    /// metrics (see [`Self::collect_instance_metrics`]).
    ///
    /// This is the per-app half of Bun's collection loop, kept here so the
    /// cluster tests drive the same code that feeds the autoscaler in
    /// production. Returns an empty vec when the agent doesn't answer.
    pub async fn collect_agent_instance_metrics(
        &self,
        agent: &tokio::sync::mpsc::Sender<crate::bun::agent::AgentCommand>,
        node: &str,
    ) -> Vec<CollectedMetric> {
        let (response, statuses) = tokio::sync::oneshot::channel();
        if agent
            .send(crate::bun::agent::AgentCommand::Status { response })
            .await
            .is_err()
        {
            return Vec::new();
        }
        let Ok(statuses) = statuses.await else {
            return Vec::new();
        };
        let instances: Vec<InstanceProcess<'_>> = statuses
            .iter()
            .map(|s| InstanceProcess {
                pid: s.pid,
                namespace: &s.namespace,
                app: &s.app_name,
                instance: &s.id,
            })
            .collect();
        self.collect_instance_metrics(&instances, node)
    }

    /// Collect per-process metrics for a given PID.
    ///
    /// Returns an empty vec if the process doesn't exist.
    pub fn collect_process_metrics(&self, pid: u32, app_label: &str) -> Vec<CollectedMetric> {
        let sysinfo_pid = Pid::from_u32(pid);
        let Some(process) = self.system.process(sysinfo_pid) else {
            return Vec::new();
        };

        let mut labels = BTreeMap::new();
        labels.insert("app".to_string(), app_label.to_string());
        labels.insert("pid".to_string(), pid.to_string());

        vec![
            CollectedMetric {
                key: MetricKey::with_labels("process_cpu_percent", labels.clone()),
                value: process.cpu_usage() as f64,
            },
            CollectedMetric {
                key: MetricKey::with_labels("process_memory_bytes", labels),
                value: process.memory() as f64,
            },
        ]
    }
}

impl Default for SystemCollector {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn exited_processes_are_forgotten_on_refresh() {
        let mut collector = SystemCollector::new();
        let mut children: Vec<_> = (0..20)
            .map(|_| {
                std::process::Command::new("sleep")
                    .arg("30")
                    .spawn()
                    .unwrap()
            })
            .collect();
        let pids: Vec<_> = children
            .iter()
            .map(|child| sysinfo::Pid::from_u32(child.id()))
            .collect();
        collector.refresh();
        assert!(
            pids.iter()
                .all(|pid| collector.system.process(*pid).is_some())
        );
        for child in &mut children {
            child.kill().unwrap();
            child.wait().unwrap();
        }
        collector.refresh();
        let remembered: Vec<_> = pids
            .iter()
            .filter(|pid| collector.system.process(**pid).is_some())
            .collect();
        assert!(remembered.is_empty(), "still tracking {remembered:?}");
    }

    use super::*;

    #[test]
    fn collector_creates_without_panic() {
        let _collector = SystemCollector::new();
    }

    #[test]
    fn node_metrics_include_cpu() {
        let collector = SystemCollector::new();
        let metrics = collector.collect_node_metrics();
        assert!(
            metrics
                .iter()
                .any(|m| m.key.name.as_str() == "node_cpu_usage_percent")
        );
    }

    #[test]
    fn node_metrics_include_memory() {
        let collector = SystemCollector::new();
        let metrics = collector.collect_node_metrics();
        assert!(
            metrics
                .iter()
                .any(|m| m.key.name.as_str() == "node_memory_used_bytes")
        );
        assert!(
            metrics
                .iter()
                .any(|m| m.key.name.as_str() == "node_memory_total_bytes")
        );
    }

    #[test]
    fn node_metrics_include_disk() {
        let collector = SystemCollector::new();
        let metrics = collector.collect_node_metrics();
        assert!(
            metrics
                .iter()
                .any(|m| m.key.name.as_str() == "node_disk_used_bytes")
        );
        assert!(
            metrics
                .iter()
                .any(|m| m.key.name.as_str() == "node_disk_total_bytes")
        );
    }

    #[test]
    fn node_metrics_include_network() {
        let collector = SystemCollector::new();
        let metrics = collector.collect_node_metrics();
        assert!(
            metrics
                .iter()
                .any(|m| m.key.name.as_str() == "node_network_rx_bytes")
        );
        assert!(
            metrics
                .iter()
                .any(|m| m.key.name.as_str() == "node_network_tx_bytes")
        );
        assert!(
            metrics
                .iter()
                .any(|m| m.key.name.as_str() == "node_network_rx_packets")
        );
        assert!(
            metrics
                .iter()
                .any(|m| m.key.name.as_str() == "node_network_tx_packets")
        );
    }

    #[test]
    fn node_metrics_count() {
        let collector = SystemCollector::new();
        let metrics = collector.collect_node_metrics();
        // cpu(1) + memory(2) + disk(2) + network(4) = 9
        assert_eq!(metrics.len(), 9);
    }

    #[test]
    fn node_metrics_follow_naming_convention() {
        let collector = SystemCollector::new();
        let metrics = collector.collect_node_metrics();
        for m in &metrics {
            assert!(
                m.key.name.as_str().starts_with("node_"),
                "metric {} doesn't start with node_",
                m.key.name
            );
        }
    }

    #[test]
    fn memory_total_is_positive() {
        let collector = SystemCollector::new();
        let metrics = collector.collect_node_metrics();
        let total = metrics
            .iter()
            .find(|m| m.key.name.as_str() == "node_memory_total_bytes")
            .unwrap();
        assert!(total.value > 0.0);
    }

    #[test]
    fn process_metrics_for_current_pid() {
        let collector = SystemCollector::new();
        let pid = std::process::id();
        let metrics = collector.collect_process_metrics(pid, "self");
        assert_eq!(metrics.len(), 2);
        assert!(
            metrics
                .iter()
                .any(|m| m.key.name.as_str() == "process_cpu_percent")
        );
        assert!(
            metrics
                .iter()
                .any(|m| m.key.name.as_str() == "process_memory_bytes")
        );
    }

    #[test]
    fn process_metrics_have_app_label() {
        let collector = SystemCollector::new();
        let pid = std::process::id();
        let metrics = collector.collect_process_metrics(pid, "myapp");
        for m in &metrics {
            assert_eq!(m.key.labels.get("app").unwrap(), "myapp");
        }
    }

    #[test]
    fn process_metrics_for_unknown_pid_returns_empty() {
        let collector = SystemCollector::new();
        let metrics = collector.collect_process_metrics(999_999_999, "ghost");
        assert!(metrics.is_empty());
    }

    #[test]
    fn instance_metrics_label_namespace_slash_app() {
        // OBS3: the collection loop labels per-app metrics `namespace/app`.
        let collector = SystemCollector::new();
        let pid = std::process::id();
        let instances = [process(Some(pid), "web")];
        let metrics = collector.collect_instance_metrics(&instances, "node-a");
        assert_eq!(metrics.len(), 2, "cpu + memory for the one live instance");
        for m in &metrics {
            assert_eq!(m.key.labels.get("app").unwrap(), "prod/web");
            assert_eq!(m.key.labels.get("namespace").unwrap(), "prod");
            assert_eq!(m.key.labels.get("instance").unwrap(), "web-0");
            assert_eq!(m.key.labels.get("node").unwrap(), "node-a");
        }
    }

    #[test]
    fn instance_metrics_skip_instances_without_a_pid() {
        let collector = SystemCollector::new();
        let pid = std::process::id();
        let instances = [
            process(None, "no-pid"),             // skipped
            process(Some(pid), "live"),          // collected
            process(Some(999_999_999), "ghost"), // no such process → empty
        ];
        let metrics = collector.collect_instance_metrics(&instances, "node-a");
        // Only the live instance contributes (2 metrics).
        assert_eq!(metrics.len(), 2);
        assert!(
            metrics
                .iter()
                .all(|m| m.key.labels.get("app").unwrap() == "prod/live")
        );
    }

    #[test]
    fn instance_metrics_empty_input_is_empty() {
        let collector = SystemCollector::new();
        assert!(collector.collect_instance_metrics(&[], "node-a").is_empty());
    }

    fn process(pid: Option<u32>, app: &str) -> InstanceProcess<'_> {
        InstanceProcess {
            pid,
            namespace: "prod",
            app,
            instance: "web-0",
        }
    }

    #[test]
    fn process_metrics_follow_naming_convention() {
        let collector = SystemCollector::new();
        let pid = std::process::id();
        let metrics = collector.collect_process_metrics(pid, "test");
        for m in &metrics {
            assert!(
                m.key.name.as_str().starts_with("process_"),
                "metric {} doesn't start with process_",
                m.key.name
            );
        }
    }

    #[test]
    fn refresh_does_not_panic() {
        let mut collector = SystemCollector::new();
        collector.refresh();
        let metrics = collector.collect_node_metrics();
        assert!(!metrics.is_empty());
    }

    #[test]
    fn network_metrics_are_non_negative() {
        let collector = SystemCollector::new();
        let metrics = collector.collect_node_metrics();
        for m in metrics
            .iter()
            .filter(|m| m.key.name.as_str().contains("network"))
        {
            assert!(m.value >= 0.0, "{} is negative: {}", m.key.name, m.value);
        }
    }
}
