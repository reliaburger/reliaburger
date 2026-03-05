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
    // TODO(Phase 5): images section
    // TODO(Phase 6): logs, metrics sections
    // TODO(Phase 3): ingress section
    // TODO(Phase 8): process_workloads section
    // TODO(Phase 9): upgrades section
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
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClusterSection {
    /// Addresses of existing cluster members to join. Empty for the first node.
    pub join: Vec<String>,
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
}
