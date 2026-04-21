/// Node configuration (node.toml).
///
/// Each Reliaburger node is configured via a single TOML file.
/// A minimal node can join a cluster with just `[cluster] join = [...]`.
/// All other sections have sensible defaults.
use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Top-level node configuration.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NodeConfig {
    pub node: NodeSection,
    pub cluster: ClusterSection,
    pub storage: StorageSection,
    pub resources: ResourcesSection,
    pub network: NetworkSection,
    pub reporting_tree: ReportingTreeSection,
    pub reconstruction: ReconstructionSection,
    pub images: ImagesSection,
    pub metrics: MetricsSection,
    pub logs: LogsSection,
    pub process_workloads: super::process_workloads::ProcessWorkloadsConfig,
    /// GitOps configuration (optional — only needed if GitOps is enabled).
    #[serde(default)]
    pub gitops: Option<crate::lettuce::types::GitOpsConfig>,
    // TODO(Phase 3): ingress section
    // TODO(Phase 14): upgrades section
}

impl NodeConfig {
    /// Parse node configuration from a TOML string.
    pub fn parse(toml: &str) -> Result<Self, super::error::ConfigError> {
        Ok(toml::from_str(toml)?)
    }

    /// Parse node configuration from a file.
    pub fn from_file(path: &std::path::Path) -> Result<Self, super::error::ConfigError> {
        let content = std::fs::read_to_string(path).map_err(|source| {
            super::error::ConfigError::ReadFile {
                path: path.to_path_buf(),
                source,
            }
        })?;
        Self::parse(&content)
    }
}

/// Node identity and labels.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NodeSection {
    /// Node name. Defaults to hostname if omitted.
    pub name: Option<String>,
    /// Arbitrary key-value labels for placement constraints.
    pub labels: BTreeMap<String, String>,
}

/// Cluster membership configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClusterSection {
    /// Addresses of existing cluster members to join. Empty for the first node.
    pub join: Vec<String>,
    /// Port for Mustard SWIM gossip protocol.
    pub gossip_port: u16,
    /// Port for Raft council consensus RPCs.
    pub raft_port: u16,
    /// Port for reporting tree state reports.
    pub reporting_port: u16,
}

impl Default for ClusterSection {
    fn default() -> Self {
        Self {
            join: Vec::new(),
            gossip_port: 9443,
            raft_port: 9444,
            reporting_port: 9445,
        }
    }
}

/// Storage paths for data, images, logs, metrics, and volumes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StorageSection {
    pub data: PathBuf,
    pub images: PathBuf,
    pub logs: PathBuf,
    pub metrics: PathBuf,
    pub volumes: PathBuf,
}

impl Default for StorageSection {
    fn default() -> Self {
        Self {
            data: PathBuf::from("/var/lib/reliaburger/data"),
            images: PathBuf::from("/var/lib/reliaburger/images"),
            logs: PathBuf::from("/var/lib/reliaburger/logs"),
            metrics: PathBuf::from("/var/lib/reliaburger/metrics"),
            volumes: PathBuf::from("/var/lib/reliaburger/volumes"),
        }
    }
}

/// Reserved resources and GPU detection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ResourcesSection {
    /// CPU reserved for system and Bun, e.g. "500m".
    pub reserved_cpu: String,
    /// Memory reserved for system and Bun, e.g. "512Mi".
    pub reserved_memory: String,
    /// Enable GPU auto-detection via NVML.
    pub gpu_enabled: bool,
}

impl Default for ResourcesSection {
    fn default() -> Self {
        Self {
            reserved_cpu: "500m".to_string(),
            reserved_memory: "512Mi".to_string(),
            gpu_enabled: true,
        }
    }
}

/// Network configuration.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NetworkSection {
    /// IP address advertised to the cluster. Auto-detected if omitted.
    pub advertise_address: Option<String>,
    /// Ephemeral port range for container port mapping.
    pub port_range: PortRange,
}

/// Port range for container port mapping.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PortRange {
    pub start: u16,
    pub end: u16,
}

impl Default for PortRange {
    fn default() -> Self {
        Self {
            start: 10000,
            end: 60000,
        }
    }
}

/// Reporting tree configuration.
///
/// Controls how often worker nodes send state reports to their assigned
/// council member, and when reports are considered stale.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReportingTreeSection {
    /// How often worker nodes send StateReports (seconds).
    pub report_interval_secs: u64,
    /// Maximum events in a single report's event log.
    pub max_events_per_report: usize,
    /// Time after which a report is considered stale (seconds).
    pub stale_report_timeout_secs: u64,
}

impl Default for ReportingTreeSection {
    fn default() -> Self {
        Self {
            report_interval_secs: 5,
            max_events_per_report: 100,
            stale_report_timeout_secs: 30,
        }
    }
}

/// State reconstruction configuration.
///
/// Controls the learning period after a new Raft leader is elected.
/// The leader collects StateReports before reconciling desired vs actual state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReconstructionSection {
    /// Percentage of alive nodes that must report before the learning period ends.
    pub report_threshold_percent: u8,
    /// Maximum duration of the learning period (seconds).
    pub learning_period_timeout_secs: u64,
    /// Extended timeout for large clusters (seconds).
    pub large_cluster_timeout_secs: u64,
    /// Node count threshold for switching to the large cluster timeout.
    pub large_cluster_node_count: usize,
}

impl Default for ReconstructionSection {
    fn default() -> Self {
        Self {
            report_threshold_percent: 95,
            learning_period_timeout_secs: 15,
            large_cluster_timeout_secs: 30,
            large_cluster_node_count: 5000,
        }
    }
}

/// Pickle image registry configuration.
///
/// Controls replication, garbage collection, and storage limits
/// for the built-in OCI image registry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ImagesSection {
    /// Maximum storage for image blobs (e.g. "50Gi"). 0 means unlimited.
    pub max_storage: String,
    /// Number of replicas for each layer (including the original pusher).
    pub redundancy: u32,
    /// Days to retain unreferenced images before GC.
    pub gc_retain_days: u32,
    /// Hours between GC sweeps.
    pub gc_interval_hours: u32,
    /// Port for the OCI Distribution API (Pickle registry).
    pub registry_port: u16,
    /// Image trust policy (signature requirements).
    pub trust_policy: TrustPolicySection,
}

/// Image trust policy controlling signature requirements.
///
/// When `require_signatures` is `true`, the scheduler refuses to
/// schedule Pickle-hosted images that have no attached signature.
/// Images from external registries are not checked.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TrustPolicySection {
    /// Require all Pickle-hosted images to be signed before scheduling.
    pub require_signatures: bool,
    /// Base64-encoded ECDSA P-256 public keys trusted for external signing.
    pub keys: Vec<String>,
}

impl Default for ImagesSection {
    fn default() -> Self {
        Self {
            max_storage: "0".to_string(),
            redundancy: 2,
            gc_retain_days: 7,
            gc_interval_hours: 1,
            registry_port: 5050,
            trust_policy: TrustPolicySection::default(),
        }
    }
}

/// Metrics collection and storage configuration (Mayo).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MetricsSection {
    /// How often to collect system and process metrics (seconds).
    pub collection_interval_secs: u64,
    /// Days to retain metric data before pruning.
    pub retention_days: u32,
    /// How often to scrape Prometheus /metrics endpoints (seconds).
    pub scrape_interval_secs: u64,
    /// Enable built-in alert evaluation.
    pub alerts_enabled: bool,
    /// Object store URL for metric persistence. Empty = local filesystem.
    /// Set to `s3://bucket/prefix` for S3-backed storage.
    pub object_store_url: String,
    /// How often to push rollup aggregates to the council (seconds).
    pub rollup_interval_secs: u64,
    /// How long to retain rollup data on council members (hours).
    pub rollup_retention_hours: u32,
}

impl Default for MetricsSection {
    fn default() -> Self {
        Self {
            collection_interval_secs: 10,
            retention_days: 7,
            scrape_interval_secs: 30,
            alerts_enabled: true,
            object_store_url: String::new(),
            rollup_interval_secs: 60,
            rollup_retention_hours: 24,
        }
    }
}

/// Log collection configuration (Ketchup).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LogsSection {
    /// Days to retain log files before deletion.
    pub retention_days: u32,
    /// Maximum size of a single log file in MB before rotation.
    pub max_file_size_mb: u64,
    /// Optional destination for Parquet log export. When set, log
    /// files are periodically copied to this path. Can be a local
    /// path (`/mnt/backup/logs/`) or an object store URL
    /// (`s3://bucket/logs/`).
    pub export_path: Option<String>,
    /// How often to export logs (seconds). Default: 3600 (1 hour).
    pub export_interval_secs: u64,
}

impl Default for LogsSection {
    fn default() -> Self {
        Self {
            retention_days: 7,
            max_file_size_mb: 100,
            export_path: None,
            export_interval_secs: 3600,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_node_config_all_defaults() {
        let nc = NodeConfig::parse("").unwrap();
        assert_eq!(nc, NodeConfig::default());
        assert_eq!(nc.storage.data, PathBuf::from("/var/lib/reliaburger/data"));
        assert_eq!(nc.resources.reserved_cpu, "500m");
        assert_eq!(
            nc.network.port_range,
            PortRange {
                start: 10000,
                end: 60000
            }
        );
        assert!(nc.cluster.join.is_empty());
    }

    #[test]
    fn parse_node_config_all_fields() {
        let toml_str = r#"
            [node]
            name = "node-1"
            labels = { region = "us-east", ssd = "true" }

            [cluster]
            join = ["10.0.1.5:9443", "10.0.1.6:9443"]

            [storage]
            data = "/mnt/fast/data"
            images = "/mnt/fast/images"
            logs = "/mnt/slow/logs"
            metrics = "/mnt/slow/metrics"
            volumes = "/mnt/fast/volumes"

            [resources]
            reserved_cpu = "1000m"
            reserved_memory = "1Gi"
            gpu_enabled = false

            [network]
            advertise_address = "10.0.1.5"
            port_range = { start = 20000, end = 50000 }
        "#;
        let nc = NodeConfig::parse(toml_str).unwrap();
        assert_eq!(nc.node.name.as_deref(), Some("node-1"));
        assert_eq!(nc.node.labels.get("region").unwrap(), "us-east");
        assert_eq!(nc.cluster.join.len(), 2);
        assert_eq!(nc.storage.data, PathBuf::from("/mnt/fast/data"));
        assert_eq!(nc.resources.reserved_cpu, "1000m");
        assert!(!nc.resources.gpu_enabled);
        assert_eq!(nc.network.advertise_address.as_deref(), Some("10.0.1.5"));
        assert_eq!(nc.network.port_range.start, 20000);
    }

    #[test]
    fn parse_node_config_minimal_join() {
        let toml_str = r#"
            [cluster]
            join = ["10.0.1.5:9443"]
        "#;
        let nc = NodeConfig::parse(toml_str).unwrap();
        assert_eq!(nc.cluster.join, vec!["10.0.1.5:9443"]);
        // All other sections have defaults
        assert_eq!(nc.storage, StorageSection::default());
    }

    #[test]
    fn parse_node_config_with_labels() {
        let toml_str = r#"
            [node]
            labels = { gpu_model = "a100", rack = "r42" }
        "#;
        let nc = NodeConfig::parse(toml_str).unwrap();
        assert_eq!(nc.node.labels.get("gpu_model").unwrap(), "a100");
        assert_eq!(nc.node.labels.get("rack").unwrap(), "r42");
        assert_eq!(nc.node.labels.len(), 2);
    }

    #[test]
    fn parse_node_config_custom_storage_paths() {
        let toml_str = r#"
            [storage]
            data = "/opt/reliaburger/data"
            images = "/opt/reliaburger/images"
            logs = "/opt/reliaburger/logs"
            metrics = "/opt/reliaburger/metrics"
            volumes = "/opt/reliaburger/volumes"
        "#;
        let nc = NodeConfig::parse(toml_str).unwrap();
        assert_eq!(nc.storage.data, PathBuf::from("/opt/reliaburger/data"));
        assert_eq!(nc.storage.images, PathBuf::from("/opt/reliaburger/images"));
    }

    #[test]
    fn parse_metrics_section_defaults() {
        let nc = NodeConfig::parse("").unwrap();
        assert_eq!(nc.metrics.collection_interval_secs, 10);
        assert_eq!(nc.metrics.retention_days, 7);
        assert_eq!(nc.metrics.scrape_interval_secs, 30);
        assert!(nc.metrics.alerts_enabled);
        assert!(nc.metrics.object_store_url.is_empty());
    }

    #[test]
    fn parse_metrics_section_custom() {
        let toml_str = r#"
            [metrics]
            collection_interval_secs = 5
            retention_days = 30
            scrape_interval_secs = 15
            alerts_enabled = false
            object_store_url = "s3://my-bucket/metrics"
        "#;
        let nc = NodeConfig::parse(toml_str).unwrap();
        assert_eq!(nc.metrics.collection_interval_secs, 5);
        assert_eq!(nc.metrics.retention_days, 30);
        assert_eq!(nc.metrics.object_store_url, "s3://my-bucket/metrics");
        assert!(!nc.metrics.alerts_enabled);
    }

    #[test]
    fn parse_logs_section_defaults() {
        let nc = NodeConfig::parse("").unwrap();
        assert_eq!(nc.logs.retention_days, 7);
        assert_eq!(nc.logs.max_file_size_mb, 100);
    }

    #[test]
    fn parse_logs_section_custom() {
        let toml_str = r#"
            [logs]
            retention_days = 14
            max_file_size_mb = 500
        "#;
        let nc = NodeConfig::parse(toml_str).unwrap();
        assert_eq!(nc.logs.retention_days, 14);
        assert_eq!(nc.logs.max_file_size_mb, 500);
    }

    #[test]
    fn parse_trust_policy_defaults() {
        let nc = NodeConfig::parse("").unwrap();
        assert!(!nc.images.trust_policy.require_signatures);
        assert!(nc.images.trust_policy.keys.is_empty());
    }

    #[test]
    fn parse_trust_policy_enabled_with_keys() {
        let toml_str = r#"
            [images.trust_policy]
            require_signatures = true
            keys = ["MFkwEwYHKoZIzj0CAQ...", "MFkwEwYHKoZIzj0CAR..."]
        "#;
        let nc = NodeConfig::parse(toml_str).unwrap();
        assert!(nc.images.trust_policy.require_signatures);
        assert_eq!(nc.images.trust_policy.keys.len(), 2);
    }
}
