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
        self.system.refresh_all();
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
        for (_name, data) in self.networks.iter() {
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
