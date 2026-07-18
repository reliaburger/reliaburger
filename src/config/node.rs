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
    pub alerts: AlertsSection,
    pub process_workloads: super::process_workloads::ProcessWorkloadsConfig,
    pub security: SecuritySection,
    /// GitOps configuration (optional — only needed if GitOps is enabled).
    #[serde(default)]
    pub gitops: Option<crate::lettuce::types::GitOpsConfig>,
    pub ingress: IngressSection,
    pub dns: DnsSection,
    pub ebpf: EbpfSection,
    pub upgrades: UpgradeSection,
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

/// DNS responder configuration.
///
/// Disabled by default: the standard runc listen address is `0.0.0.0:53`
/// and binding port 53 needs root, so it must be an explicit choice.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DnsSection {
    /// Start the `.internal` DNS responder on this node.
    pub enabled: bool,
    /// Listen address, e.g. "0.0.0.0:53" for rootful runc.
    pub listen: String,
    /// Upstream resolver for non-`.internal` names.
    pub upstream: String,
    /// Namespace a bare `<app>.internal` query resolves within. Fully
    /// qualified `<app>.<namespace>.internal` queries ignore it.
    pub default_namespace: String,
    /// Restrict the `.internal` zone to container-reachable (loopback /
    /// private-range) sources. On by default so the internal topology
    /// isn't exposed to public clients; disable only for debugging.
    pub restrict_sources: bool,
}

impl Default for DnsSection {
    fn default() -> Self {
        Self {
            enabled: false,
            listen: "0.0.0.0:53".to_string(),
            upstream: "8.8.8.8:53".to_string(),
            default_namespace: "default".to_string(),
            restrict_sources: true,
        }
    }
}

impl DnsSection {
    /// Convert into the Onion DNS responder's config type.
    ///
    /// Fails when the listen or upstream addresses don't parse.
    pub fn to_dns_config(&self) -> Result<crate::onion::dns::DnsConfig, super::error::ConfigError> {
        let listen_addr =
            self.listen
                .parse()
                .map_err(|_| super::error::ConfigError::InvalidValue {
                    field: "dns.listen".to_string(),
                    value: self.listen.clone(),
                })?;
        let upstream =
            self.upstream
                .parse()
                .map_err(|_| super::error::ConfigError::InvalidValue {
                    field: "dns.upstream".to_string(),
                    value: self.upstream.clone(),
                })?;
        Ok(crate::onion::dns::DnsConfig {
            listen_addr,
            upstream,
            upstream_timeout: std::time::Duration::from_secs(2),
            default_namespace: self.default_namespace.clone(),
            source_acl: crate::onion::dns::SourceAcl {
                restrict_to_private: self.restrict_sources,
            },
        })
    }
}

/// eBPF data-path configuration (Onion service resolution, Smoker
/// network faults, Sesame egress allowlists).
///
/// Disabled by default: loading eBPF programs needs root, a 5.7+ kernel
/// and cgroup v2, and it is Linux-only. `program_dir` defaults to the
/// directory the build compiled the `.bpf.o` objects into (baked in at
/// build time), so a dev/Lima build "just works" with `enabled = true`;
/// a packaged install points it at the installed objects.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EbpfSection {
    /// Load and attach the eBPF programs on this node at startup.
    pub enabled: bool,
    /// Directory holding the compiled `.bpf.o` objects. When unset, the
    /// build-time output directory is used.
    pub program_dir: Option<PathBuf>,
    /// Root cgroup v2 path to attach the connect hook to.
    pub cgroup_path: PathBuf,
    /// How often (seconds) to reconcile the kernel egress/firewall maps
    /// against live instances — deleting entries whose cgroup no longer
    /// maps to a running workload and reinstalling entries a live
    /// instance lost (e.g. across a Bun restart). `0` disables the sweep.
    pub sweep_interval_secs: u64,
}

impl Default for EbpfSection {
    fn default() -> Self {
        Self {
            enabled: false,
            program_dir: None,
            cgroup_path: PathBuf::from("/sys/fs/cgroup"),
            sweep_interval_secs: 60,
        }
    }
}

impl EbpfSection {
    /// Resolve the directory to load `.bpf.o` objects from: the explicit
    /// config value, or the build-time output directory baked in by
    /// `build.rs`. Returns `None` if neither is available.
    pub fn resolve_program_dir(&self) -> Option<PathBuf> {
        self.program_dir
            .clone()
            .or_else(|| option_env!("RELIABURGER_BPF_DIR").map(PathBuf::from))
    }
}

/// Wrapper ingress proxy configuration.
///
/// Disabled by default: enabling binds real listeners on the node, so
/// it must be an explicit choice. Ports default to the standard 80/443
/// (bun typically runs as root in cluster VMs); tests override with
/// port 0 to get ephemeral assignments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct IngressSection {
    /// Bind the ingress listeners on this node.
    pub enabled: bool,
    /// HTTP listen port.
    pub http_port: u16,
    /// HTTPS listen port.
    pub https_port: u16,
    /// TLS certificate PEM path. Self-signed at startup if unset.
    pub tls_cert: Option<PathBuf>,
    /// TLS private key PEM path.
    pub tls_key: Option<PathBuf>,
    /// Maximum concurrent proxied connections.
    pub max_connections: usize,
}

impl Default for IngressSection {
    fn default() -> Self {
        Self {
            enabled: false,
            http_port: 80,
            https_port: 443,
            tls_cert: None,
            tls_key: None,
            max_connections: 10_000,
        }
    }
}

impl IngressSection {
    /// Convert into the Wrapper subsystem's config type.
    pub fn to_wrapper_config(&self) -> crate::wrapper::types::WrapperConfig {
        crate::wrapper::types::WrapperConfig {
            http_port: self.http_port,
            https_port: self.https_port,
            max_connections: self.max_connections,
            worker_threads: 4,
            tls_cert_path: self.tls_cert.clone(),
            tls_key_path: self.tls_key.clone(),
            ..crate::wrapper::types::WrapperConfig::default()
        }
    }
}

/// Security material paths.
///
/// Points a clustered node at the secrets `relish init` produces. Both are
/// optional so single-node and pre-security configs keep parsing. Every
/// clustered node needs `master_key_path` (to build its wrapping IKM); only the
/// bootstrap node needs `bootstrap_path` (it seeds the initial `SecurityState`
/// into Raft, which then replicates to everyone else).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SecuritySection {
    /// Path to the hex-encoded 32-byte cluster master key (`{cluster}-master.key`).
    pub master_key_path: Option<PathBuf>,
    /// Path to the initial security bootstrap (`{cluster}-security-bootstrap.json`).
    pub bootstrap_path: Option<PathBuf>,
    /// Directory holding this node's identity (certificate, private key and
    /// CA chain). Defaults to `{storage.data}/identity`. Written by
    /// `relish init` on the bootstrap node and by `relish join` on joiners.
    pub identity_dir: Option<PathBuf>,
    /// Require mTLS on the internal listeners (Raft RPC, agent API,
    /// reporting). With this set a node refuses to bootstrap without an
    /// identity on disk, and a joiner starts in enrollment mode until
    /// `relish join` installs one.
    pub require_mtls: bool,
}

/// Self-upgrade configuration (Phase 14).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UpgradeSection {
    /// External Ed25519 signing key (`ed25519:` + base64 of 32 bytes).
    /// Required for network upgrades; air-gapped (`--binary`) upgrades
    /// work without it.
    pub external_signing_key: Option<String>,
    /// Number of previous binary versions to retain on disk for rollback.
    pub retain_versions: u32,
    /// Release metadata endpoint URL for `relish upgrade check`.
    pub release_url: String,
    /// Directory holding the versioned binaries and the entry symlink.
    /// Defaults to the directory of the resolved current executable.
    pub binary_dir: Option<PathBuf>,
    /// Seconds the new binary must survive after an upgrade before the
    /// swap counts as verified.
    pub boot_grace_secs: u64,
    /// Seconds the new binary has to rejoin gossip (cluster mode only).
    pub gossip_rejoin_secs: u64,
    /// Boot attempts on the new version before automatic revert.
    pub max_boot_attempts: u32,
    /// Debug builds only: replaces the compiled-in release key set so
    /// integration tests can sign with throwaway keys. Ignored (with a
    /// warning) in release builds.
    pub release_keys_override: Option<Vec<String>>,
}

impl Default for UpgradeSection {
    fn default() -> Self {
        Self {
            external_signing_key: None,
            retain_versions: 3,
            release_url: "https://releases.reliaburger.dev/metadata.json".to_string(),
            binary_dir: None,
            boot_grace_secs: 30,
            gossip_rejoin_secs: 60,
            max_boot_attempts: 2,
            release_keys_override: None,
        }
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
    /// Encrypted external council backup (`[cluster.backup]`, 12b.2
    /// D21/CP12). Off by default; set `url` to enable.
    pub backup: crate::council::backup::BackupConfig,
}

impl Default for ClusterSection {
    fn default() -> Self {
        Self {
            join: Vec::new(),
            gossip_port: 9443,
            raft_port: 9444,
            reporting_port: 9445,
            backup: crate::council::backup::BackupConfig::default(),
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
    /// Scheduled volume snapshots (`[storage.snapshots]`).
    pub snapshots: SnapshotsSection,
}

impl Default for StorageSection {
    fn default() -> Self {
        Self {
            data: PathBuf::from("/var/lib/reliaburger/data"),
            images: PathBuf::from("/var/lib/reliaburger/images"),
            logs: PathBuf::from("/var/lib/reliaburger/logs"),
            metrics: PathBuf::from("/var/lib/reliaburger/metrics"),
            volumes: PathBuf::from("/var/lib/reliaburger/volumes"),
            snapshots: SnapshotsSection::default(),
        }
    }
}

/// Scheduled volume snapshots and their object-store upload.
///
/// Interval-based rather than cron expressions — a scheduled loop
/// satisfies "snapshots run on a schedule" without dragging in a cron
/// parser and chrono for a feature nobody asked for by name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SnapshotsSection {
    /// Seconds between snapshot sweeps. `0` disables the loop.
    pub interval_secs: u64,
    /// Snapshots retained per volume; older ones are pruned.
    pub retain: usize,
    /// Optional object-store destination for snapshot archives
    /// (`file://`, `s3://`, `gs://`). Credentials come from the
    /// standard environment variables of each backend.
    pub upload_url: Option<String>,
}

impl Default for SnapshotsSection {
    fn default() -> Self {
        Self {
            interval_secs: 0,
            retain: 7,
            upload_url: None,
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

impl ReconstructionSection {
    /// Validate the reconstruction thresholds.
    ///
    /// The recovery path (12b.2 D21/CP12) and the ordinary leadership edge
    /// both gate on these, so a nonsensical value would silently break the
    /// learning period. Coverage must sit in `1..=100` (0% would end the
    /// period before any report, 100% needs everyone) and every timeout must
    /// be positive (a zero timeout would fire the moment learning starts).
    pub fn validate(&self) -> Result<(), super::error::ConfigError> {
        let bad = |field: &str, reason: &str| super::error::ConfigError::Validation {
            field: field.to_string(),
            context: "reconstruction".to_string(),
            reason: reason.to_string(),
        };
        if self.report_threshold_percent == 0 || self.report_threshold_percent > 100 {
            return Err(bad(
                "coverage_percent",
                "must be between 1 and 100 inclusive",
            ));
        }
        if self.learning_period_timeout_secs == 0 {
            return Err(bad("timeout_secs", "must be greater than zero"));
        }
        if self.large_cluster_timeout_secs == 0 {
            return Err(bad(
                "large_cluster_timeout_secs",
                "must be greater than zero",
            ));
        }
        Ok(())
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
    /// Interface the registry binds to. Defaults to `127.0.0.1` (loopback) so
    /// the registry is not exposed unauthenticated on all interfaces. Set to
    /// `0.0.0.0` once cross-node replication (and auth) are wired.
    pub registry_bind: String,
    /// Parallel layer fetches per P2P image pull.
    pub p2p_concurrency: usize,
    /// Serve external images through the Pickle pull-through cache:
    /// first pull fetches from upstream and commits under
    /// `cache/<host>/<repo>`; later pulls anywhere in the cluster are
    /// served peer-to-peer.
    pub pull_through: bool,
    /// Seconds a cached upstream image is served without rechecking
    /// whether its (mutable) tag moved upstream.
    pub cache_recheck_secs: u64,
    /// Credentials for upstream registries the pull-through cache may
    /// authenticate to. Hosts not listed are accessed anonymously.
    pub external_registries: Vec<ExternalRegistrySection>,
    /// Ceiling (seconds) per buildah stage of an image build.
    pub build_timeout_secs: u64,
    /// Ceiling (bytes) on an extracted build context. The build runner
    /// streams the context download to disk and aborts past this cap,
    /// so a giant or sparse-bomb context cannot fill the build node.
    pub max_context_bytes: u64,
    /// Image trust policy (signature requirements).
    pub trust_policy: TrustPolicySection,
}

/// Credentials for one upstream registry host.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExternalRegistrySection {
    /// Registry host, e.g. `ghcr.io`.
    pub host: String,
    /// Username for basic auth; anonymous when omitted.
    pub username: Option<String>,
    /// Name of the environment variable holding the password/token,
    /// resolved once at startup. An unset variable falls back to
    /// anonymous access. (Env, not a cluster secret: registry
    /// credentials are needed before the secret machinery is up, and
    /// env-injected credentials are the registry-auth convention.)
    pub password_secret: Option<String>,
}

/// Image trust policy controlling signature requirements.
///
/// When `require_signatures` is `true`, the scheduler refuses to
/// schedule Pickle-hosted images that have no attached signature, and a
/// node that can't reach the cluster trust state to verify one refuses the
/// deploy rather than skipping the check (fail-closed, IMG2). Images from
/// external registries are not checked.
///
/// There is deliberately no `build_signer` key path to configure: the
/// build signer is a persistent code-signing identity the council mints
/// from the cluster's own Workload CA (one per namespace,
/// `spiffe://…/job/build-signer`). Tying it to the cluster CA means the
/// trust policy accepts it by construction, and there's no key material for
/// an operator to provision or distribute.
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
            registry_bind: "127.0.0.1".to_string(),
            p2p_concurrency: 4,
            pull_through: true,
            cache_recheck_secs: 3600,
            external_registries: Vec::new(),
            build_timeout_secs: 900,
            max_context_bytes: 256 * 1024 * 1024,
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
    /// Maximum local Parquet storage for metrics (MB). 0 means unlimited.
    /// When exceeded, exported files are pruned oldest-first.
    pub max_storage_mb: u64,
    /// Optional export destination for metrics Parquet files.
    pub export_path: Option<String>,
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
            max_storage_mb: 0,
            export_path: None,
        }
    }
}

/// Log collection configuration (Ketchup).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LogsSection {
    /// Days to retain log files before deletion.
    pub retention_days: u32,
    /// Optional destination for Parquet log export. When set, un-exported
    /// log files are periodically shipped here via `object_store`. Can be a
    /// local path (`/mnt/backup/logs/`), a `file://…` URL, or an object
    /// store URL (`s3://bucket/logs/`, `gs://bucket/logs/`).
    pub export_path: Option<String>,
    /// How often to export logs (seconds). Default: 3600 (1 hour).
    pub export_interval_secs: u64,
    /// Maximum local Parquet storage for logs (MB). 0 means unlimited.
    /// When exceeded, exported files are pruned oldest-first.
    pub max_storage_mb: u64,
}

impl Default for LogsSection {
    fn default() -> Self {
        Self {
            retention_days: 7,
            export_path: None,
            export_interval_secs: 3600,
            max_storage_mb: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Alert notification configuration
// ---------------------------------------------------------------------------

/// Alert evaluation and notification configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AlertsSection {
    /// How often to evaluate alert rules (seconds).
    pub evaluation_interval_secs: u64,
    /// Webhook destinations for alert notifications.
    pub destinations: Vec<AlertDestination>,
}

impl Default for AlertsSection {
    fn default() -> Self {
        Self {
            evaluation_interval_secs: 30,
            destinations: vec![],
        }
    }
}

impl AlertsSection {
    /// Reject a nonsensical config before anything is built on it (OBS4).
    ///
    /// A zero evaluation interval would make `tokio::time::interval` panic at
    /// startup, so we fail loudly here with a clear message instead.
    pub fn validate(&self) -> Result<(), super::error::ConfigError> {
        if self.evaluation_interval_secs == 0 {
            return Err(super::error::ConfigError::Validation {
                field: "evaluation_interval_secs".to_string(),
                context: "alerts".to_string(),
                reason: "must be greater than zero".to_string(),
            });
        }
        Ok(())
    }
}

/// A single webhook destination for alert notifications.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AlertDestination {
    /// Destination type (currently only `"webhook"`).
    #[serde(rename = "type")]
    pub dest_type: String,
    /// Webhook URL.
    pub url: String,
    /// Which severities to send. Empty means all severities.
    #[serde(default)]
    pub severity: Vec<String>,
    /// Optional HMAC-SHA256 shared secret for payload signing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
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

    /// L7: enabling ingress binds real listeners, so it must be opt-in.
    #[test]
    fn ingress_disabled_by_default() {
        let nc = NodeConfig::parse("").unwrap();
        assert!(!nc.ingress.enabled);
        assert_eq!(nc.ingress.http_port, 80);
        assert_eq!(nc.ingress.https_port, 443);
    }

    #[test]
    fn ingress_section_parses_and_converts() {
        let nc = NodeConfig::parse(
            r#"
            [ingress]
            enabled = true
            http_port = 8080
            https_port = 8443
            max_connections = 500
        "#,
        )
        .unwrap();
        assert!(nc.ingress.enabled);
        let wc = nc.ingress.to_wrapper_config();
        assert_eq!(wc.http_port, 8080);
        assert_eq!(wc.https_port, 8443);
        assert_eq!(wc.max_connections, 500);
        assert!(wc.tls_cert_path.is_none());
    }

    /// L8: loading eBPF needs root + a recent kernel, so it is opt-in.
    #[test]
    fn ebpf_disabled_by_default() {
        let nc = NodeConfig::parse("").unwrap();
        assert!(!nc.ebpf.enabled);
        assert_eq!(nc.ebpf.cgroup_path, PathBuf::from("/sys/fs/cgroup"));
        assert!(nc.ebpf.program_dir.is_none());
        // The kernel-truth sweep defaults on with a sane interval.
        assert_eq!(nc.ebpf.sweep_interval_secs, 60);
    }

    #[test]
    fn ebpf_sweep_interval_parses_and_zero_disables() {
        let nc = NodeConfig::parse(
            r#"
            [ebpf]
            sweep_interval_secs = 0
        "#,
        )
        .unwrap();
        assert_eq!(nc.ebpf.sweep_interval_secs, 0);
    }

    #[test]
    fn ebpf_section_parses_explicit_program_dir() {
        let nc = NodeConfig::parse(
            r#"
            [ebpf]
            enabled = true
            program_dir = "/opt/reliaburger/bpf"
            cgroup_path = "/sys/fs/cgroup"
        "#,
        )
        .unwrap();
        assert!(nc.ebpf.enabled);
        // An explicit program_dir wins over the build-time default.
        assert_eq!(
            nc.ebpf.resolve_program_dir(),
            Some(PathBuf::from("/opt/reliaburger/bpf"))
        );
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
    fn alerts_default_interval_validates() {
        assert!(AlertsSection::default().validate().is_ok());
    }

    #[test]
    fn alerts_zero_interval_is_rejected() {
        // OBS4: a zero interval would panic tokio's interval timer at startup.
        let section = AlertsSection {
            evaluation_interval_secs: 0,
            destinations: vec![],
        };
        assert!(section.validate().is_err());
    }

    #[test]
    fn parse_logs_section_defaults() {
        let nc = NodeConfig::parse("").unwrap();
        assert_eq!(nc.logs.retention_days, 7);
        assert_eq!(nc.logs.export_interval_secs, 3600);
        assert!(nc.logs.export_path.is_none());
    }

    #[test]
    fn parse_logs_section_custom() {
        let toml_str = r#"
            [logs]
            retention_days = 14
            export_path = "s3://bucket/logs"
            export_interval_secs = 600
        "#;
        let nc = NodeConfig::parse(toml_str).unwrap();
        assert_eq!(nc.logs.retention_days, 14);
        assert_eq!(nc.logs.export_path.as_deref(), Some("s3://bucket/logs"));
        assert_eq!(nc.logs.export_interval_secs, 600);
    }

    /// OBS8: the removed `logs.max_file_size_mb` field must not break existing
    /// configs that still set it — serde ignores unknown keys here.
    #[test]
    fn parse_logs_section_ignores_removed_max_file_size() {
        let nc = NodeConfig::parse("[logs]\nmax_file_size_mb = 500\n").unwrap();
        assert_eq!(nc.logs.retention_days, 7);
    }

    #[test]
    fn parse_trust_policy_defaults() {
        let nc = NodeConfig::parse("").unwrap();
        assert!(!nc.images.trust_policy.require_signatures);
        assert!(nc.images.trust_policy.keys.is_empty());
    }

    #[test]
    fn registry_binds_to_loopback_by_default() {
        let nc = NodeConfig::parse("").unwrap();
        assert_eq!(nc.images.registry_bind, "127.0.0.1");
        assert_eq!(nc.images.registry_port, 5050);
    }

    #[test]
    fn max_context_bytes_defaults_to_256_mib() {
        let nc = NodeConfig::parse("").unwrap();
        assert_eq!(nc.images.max_context_bytes, 256 * 1024 * 1024);
    }

    #[test]
    fn max_context_bytes_is_configurable() {
        let nc = NodeConfig::parse("[images]\nmax_context_bytes = 1048576\n").unwrap();
        assert_eq!(nc.images.max_context_bytes, 1024 * 1024);
    }

    #[test]
    fn registry_bind_is_configurable() {
        let toml_str = r#"
            [images]
            registry_bind = "0.0.0.0"
        "#;
        let nc = NodeConfig::parse(toml_str).unwrap();
        assert_eq!(nc.images.registry_bind, "0.0.0.0");
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

    #[test]
    fn parse_alerts_section_defaults() {
        let nc = NodeConfig::parse("").unwrap();
        assert_eq!(nc.alerts.evaluation_interval_secs, 30);
        assert!(nc.alerts.destinations.is_empty());
    }

    #[test]
    fn parse_alerts_section_with_destinations() {
        let toml_str = r#"
            [[alerts.destinations]]
            type = "webhook"
            url = "https://hooks.slack.com/services/T/B/xxx"
            severity = ["critical", "warning"]

            [[alerts.destinations]]
            type = "webhook"
            url = "https://events.pagerduty.com/v2/enqueue"
            severity = ["critical"]
        "#;
        let nc = NodeConfig::parse(toml_str).unwrap();
        assert_eq!(nc.alerts.destinations.len(), 2);
        assert_eq!(nc.alerts.destinations[0].dest_type, "webhook");
        assert_eq!(
            nc.alerts.destinations[0].url,
            "https://hooks.slack.com/services/T/B/xxx"
        );
        assert_eq!(
            nc.alerts.destinations[0].severity,
            vec!["critical", "warning"]
        );
        assert!(nc.alerts.destinations[0].secret.is_none());
        assert_eq!(nc.alerts.destinations[1].severity, vec!["critical"]);
    }

    #[test]
    fn security_section_defaults_to_none() {
        let nc = NodeConfig::parse("").unwrap();
        assert!(nc.security.master_key_path.is_none());
        assert!(nc.security.bootstrap_path.is_none());
        assert!(nc.security.identity_dir.is_none());
        assert!(!nc.security.require_mtls);
    }

    #[test]
    fn parse_security_section_paths() {
        let toml_str = r#"
            [security]
            master_key_path = "/etc/reliaburger/master.key"
            bootstrap_path = "/etc/reliaburger/security-bootstrap.json"
        "#;
        let nc = NodeConfig::parse(toml_str).unwrap();
        assert_eq!(
            nc.security.master_key_path,
            Some(PathBuf::from("/etc/reliaburger/master.key"))
        );
        assert_eq!(
            nc.security.bootstrap_path,
            Some(PathBuf::from("/etc/reliaburger/security-bootstrap.json"))
        );
    }

    #[test]
    fn parse_security_section_identity_and_mtls() {
        let toml_str = r#"
            [security]
            identity_dir = "/var/lib/reliaburger/data/identity"
            require_mtls = true
        "#;
        let nc = NodeConfig::parse(toml_str).unwrap();
        assert_eq!(
            nc.security.identity_dir,
            Some(PathBuf::from("/var/lib/reliaburger/data/identity"))
        );
        assert!(nc.security.require_mtls);
    }

    #[test]
    fn parse_alerts_section_with_secret() {
        let toml_str = r#"
            [alerts]
            evaluation_interval_secs = 15

            [[alerts.destinations]]
            type = "webhook"
            url = "https://example.com/hook"
            secret = "my-shared-secret"
        "#;
        let nc = NodeConfig::parse(toml_str).unwrap();
        assert_eq!(nc.alerts.evaluation_interval_secs, 15);
        assert_eq!(nc.alerts.destinations.len(), 1);
        assert_eq!(
            nc.alerts.destinations[0].secret.as_deref(),
            Some("my-shared-secret")
        );
    }

    // -- reconstruction thresholds (12b.2 D21/CP12) -----------------------

    #[test]
    fn reconstruction_defaults_validate() {
        assert!(ReconstructionSection::default().validate().is_ok());
    }

    #[test]
    fn reconstruction_rejects_zero_coverage() {
        let section = ReconstructionSection {
            report_threshold_percent: 0,
            ..ReconstructionSection::default()
        };
        assert!(section.validate().is_err());
    }

    #[test]
    fn reconstruction_rejects_coverage_above_100() {
        let section = ReconstructionSection {
            report_threshold_percent: 101,
            ..ReconstructionSection::default()
        };
        assert!(section.validate().is_err());
    }

    #[test]
    fn reconstruction_accepts_full_coverage() {
        let section = ReconstructionSection {
            report_threshold_percent: 100,
            ..ReconstructionSection::default()
        };
        assert!(section.validate().is_ok());
    }

    #[test]
    fn reconstruction_rejects_zero_timeout() {
        let section = ReconstructionSection {
            learning_period_timeout_secs: 0,
            ..ReconstructionSection::default()
        };
        assert!(section.validate().is_err());
    }

    #[test]
    fn reconstruction_section_parses_overrides() {
        let nc = NodeConfig::parse(
            r#"
            [reconstruction]
            report_threshold_percent = 80
            learning_period_timeout_secs = 20
        "#,
        )
        .unwrap();
        assert_eq!(nc.reconstruction.report_threshold_percent, 80);
        assert_eq!(nc.reconstruction.learning_period_timeout_secs, 20);
        assert!(nc.reconstruction.validate().is_ok());
    }

    // -- cluster backup (12b.2 D21/CP12) ----------------------------------

    #[test]
    fn cluster_backup_disabled_by_default() {
        let nc = NodeConfig::parse("").unwrap();
        assert!(nc.cluster.backup.url.is_none());
        assert!(!nc.cluster.backup.enabled());
        assert!(nc.cluster.backup.validate().is_ok());
    }

    #[test]
    fn cluster_backup_section_parses() {
        let nc = NodeConfig::parse(
            r#"
            [cluster.backup]
            url = "file:///var/backups/council"
            interval_secs = 60
            retain = 10
        "#,
        )
        .unwrap();
        assert!(nc.cluster.backup.enabled());
        assert_eq!(nc.cluster.backup.interval_secs, 60);
        assert_eq!(nc.cluster.backup.retain, 10);
        assert!(nc.cluster.backup.validate().is_ok());
    }

    #[test]
    fn cluster_backup_rejects_zero_retain_when_enabled() {
        let nc = NodeConfig::parse(
            r#"
            [cluster.backup]
            url = "file:///var/backups/council"
            retain = 0
        "#,
        )
        .unwrap();
        assert!(nc.cluster.backup.validate().is_err());
    }
}
