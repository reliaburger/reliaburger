/// Type definitions for the Raft council.
///
/// Defines the openraft type configuration, request/response envelopes,
/// the desired-state model that the state machine maintains, and
/// configuration knobs for tuning Raft timers.
use std::collections::HashMap;
use std::fmt;
use std::hash::Hash;
use std::io::Cursor;
use std::net::SocketAddr;

use openraft::StoredMembership;
use openraft::storage::LogState;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::config::app::AppSpec;
use crate::meat::deploy_types::{DeployHistoryEntry, DeployState};
use crate::meat::types::{AppId, Placement, SchedulingDecision};
use crate::pickle::types::{
    DeleteTag, GcReport, ManifestCatalog, ManifestCommit, UpdateLayerLocations,
};

// ---------------------------------------------------------------------------
// openraft type configuration
// ---------------------------------------------------------------------------

openraft::declare_raft_types!(
    /// Raft type configuration for the council.
    ///
    /// Uses `u64` node IDs (openraft requires `Copy`), carries
    /// application-level node info in `CouncilNodeInfo`, and stores
    /// snapshots as in-memory byte buffers.
    pub TypeConfig:
        D            = RaftRequest,
        R            = CouncilResponse,
        NodeId       = u64,
        Node         = CouncilNodeInfo,
        Entry        = openraft::Entry<TypeConfig>,
        SnapshotData = Cursor<Vec<u8>>,
);

// ---------------------------------------------------------------------------
// CouncilNodeInfo
// ---------------------------------------------------------------------------

/// Application-level data attached to each Raft node.
///
/// openraft requires `NodeId` to be `Copy`, so we use `u64` internally.
/// The human-readable name (mapping to our `meat::NodeId(String)`)
/// lives here, alongside the Raft RPC address.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CouncilNodeInfo {
    /// Raft RPC address.
    pub addr: SocketAddr,
    /// Human-readable name, maps to `meat::NodeId`.
    pub name: String,
}

impl Default for CouncilNodeInfo {
    fn default() -> Self {
        Self {
            addr: SocketAddr::from(([0, 0, 0, 0], 0)),
            name: String::new(),
        }
    }
}

impl CouncilNodeInfo {
    /// Create a new `CouncilNodeInfo`.
    pub fn new(addr: SocketAddr, name: impl Into<String>) -> Self {
        Self {
            addr,
            name: name.into(),
        }
    }
}

impl fmt::Display for CouncilNodeInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}({})", self.name, self.addr)
    }
}

// openraft's Node trait is auto-implemented for types satisfying
// NodeEssential + Serialize + Deserialize, which CouncilNodeInfo does.

// ---------------------------------------------------------------------------
// RaftRequest (log entry payload)
// ---------------------------------------------------------------------------

/// Payload written to the Raft log.
///
/// Each variant represents a mutation to the cluster's desired state.
/// The state machine applies these in order to build its in-memory view.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum RaftRequest {
    /// Register or update an application specification.
    AppSpec { app_id: AppId, spec: Box<AppSpec> },
    /// Remove an application.
    AppDelete { app_id: AppId },
    /// Record where replicas of an app should run.
    SchedulingDecision(SchedulingDecision),
    /// Set a cluster-wide configuration key.
    ConfigSet { key: String, value: String },
    /// Commit an image manifest to the Pickle registry catalog.
    ManifestCommit(ManifestCommit),
    /// Update which nodes hold copies of specific layers.
    UpdateLayerLocations(UpdateLayerLocations),
    /// Report that a node deleted layers during garbage collection.
    GcReport(GcReport),
    /// Delete a tag from the Pickle manifest catalog.
    DeleteTag(DeleteTag),
    /// Start or update a deploy operation.
    DeployUpdate {
        app_id: AppId,
        state: Box<DeployState>,
    },
    /// Record a completed deploy in history.
    DeployComplete {
        app_id: AppId,
        entry: DeployHistoryEntry,
    },
    /// Set an autoscale replica override for an app.
    AutoscaleOverride {
        app_id: AppId,
        replicas: u32,
        reason: String,
    },
    /// Elect a GitOps coordinator.
    GitOpsCoordinatorElection(crate::lettuce::types::CoordinatorElection),
    /// Update GitOps sync state.
    GitOpsSyncUpdate(Box<crate::lettuce::types::SyncState>),
    /// No-op entry (used for leader commit on election).
    Noop,
}

// ---------------------------------------------------------------------------
// CouncilResponse
// ---------------------------------------------------------------------------

/// Response returned after a Raft log entry is applied.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum CouncilResponse {
    /// Generic success.
    Ok,
    /// Success with the log index at which the entry was applied.
    Applied { log_index: u64 },
}

// ---------------------------------------------------------------------------
// DesiredState
// ---------------------------------------------------------------------------

/// The state machine's in-memory view of desired cluster state.
///
/// Built by applying `RaftRequest` entries in log order. Snapshotted
/// to JSON for transfer to followers that fall behind.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DesiredState {
    /// Registered application specifications, keyed by app identity.
    #[serde(
        serialize_with = "map_as_vec::serialize",
        deserialize_with = "map_as_vec::deserialize"
    )]
    pub apps: HashMap<AppId, AppSpec>,
    /// Scheduling placements per app.
    #[serde(
        serialize_with = "map_as_vec::serialize",
        deserialize_with = "map_as_vec::deserialize"
    )]
    pub scheduling: HashMap<AppId, Vec<Placement>>,
    /// Cluster-wide configuration key-value pairs.
    pub config: HashMap<String, String>,
    /// Pickle image registry manifest catalog.
    #[serde(default)]
    pub manifest_catalog: ManifestCatalog,
    /// Active deploys (one per app at most).
    #[serde(default)]
    pub active_deploys: Vec<(String, DeployState)>,
    /// Deploy history (last 50 per app).
    #[serde(default)]
    pub deploy_history: Vec<(String, Vec<DeployHistoryEntry>)>,
    /// Autoscale replica overrides (runtime adjustments above/below baseline).
    #[serde(default)]
    pub autoscale_overrides: Vec<(String, u32)>,
    /// GitOps sync state.
    #[serde(default)]
    pub gitops_sync_state: Option<crate::lettuce::types::SyncState>,
    /// GitOps coordinator election.
    #[serde(default)]
    pub gitops_coordinator: Option<crate::lettuce::types::CoordinatorElection>,
    /// Log position of the last applied entry.
    pub last_applied_log: Option<openraft::LogId<u64>>,
    /// Last known membership configuration.
    pub last_membership: StoredMembership<u64, CouncilNodeInfo>,
}

/// Serialises a `HashMap<K, V>` as a `Vec<(K, V)>`.
///
/// JSON requires string keys, but `AppId` is a struct. We serialise
/// these maps as arrays of key-value pairs instead.
mod map_as_vec {
    use super::*;

    pub fn serialize<K, V, S>(map: &HashMap<K, V>, serializer: S) -> Result<S::Ok, S::Error>
    where
        K: Serialize + Eq + Hash,
        V: Serialize,
        S: Serializer,
    {
        let vec: Vec<(&K, &V)> = map.iter().collect();
        vec.serialize(serializer)
    }

    pub fn deserialize<'de, K, V, D>(deserializer: D) -> Result<HashMap<K, V>, D::Error>
    where
        K: Deserialize<'de> + Eq + Hash,
        V: Deserialize<'de>,
        D: Deserializer<'de>,
    {
        let vec: Vec<(K, V)> = Vec::deserialize(deserializer)?;
        Ok(vec.into_iter().collect())
    }
}

// ---------------------------------------------------------------------------
// CouncilConfig
// ---------------------------------------------------------------------------

/// Tuning knobs for Raft timers and thresholds.
///
/// Mapped to `openraft::Config` when creating a Raft instance.
#[derive(Debug, Clone)]
pub struct CouncilConfig {
    /// Interval between leader heartbeats (ms).
    pub heartbeat_interval_ms: u64,
    /// Minimum election timeout (ms).
    pub election_timeout_min_ms: u64,
    /// Maximum election timeout (ms).
    pub election_timeout_max_ms: u64,
    /// Number of applied entries before triggering a snapshot.
    pub snapshot_threshold: u64,
    /// Maximum log entries to keep after a snapshot.
    pub max_in_snapshot_log_to_keep: u64,
}

impl Default for CouncilConfig {
    fn default() -> Self {
        Self {
            heartbeat_interval_ms: 150,
            election_timeout_min_ms: 1000,
            election_timeout_max_ms: 2000,
            snapshot_threshold: 10_000,
            max_in_snapshot_log_to_keep: 1000,
        }
    }
}

impl CouncilConfig {
    /// Convert to an `openraft::Config`.
    pub fn to_openraft_config(&self) -> openraft::Config {
        openraft::Config {
            heartbeat_interval: self.heartbeat_interval_ms,
            election_timeout_min: self.election_timeout_min_ms,
            election_timeout_max: self.election_timeout_max_ms,
            snapshot_policy: openraft::SnapshotPolicy::LogsSinceLast(self.snapshot_threshold),
            max_in_snapshot_log_to_keep: self.max_in_snapshot_log_to_keep,
            ..Default::default()
        }
    }
}

/// Type alias for the log state of our Raft configuration.
pub type CouncilLogState = LogState<TypeConfig>;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raft_request_serialisation_round_trip() {
        let requests = vec![
            RaftRequest::AppSpec {
                app_id: AppId::new("web", "production"),
                spec: Box::new(AppSpec {
                    image: Some("myapp:v1".to_string()),
                    ..default_spec()
                }),
            },
            RaftRequest::AppDelete {
                app_id: AppId::new("old-app", "default"),
            },
            RaftRequest::SchedulingDecision(SchedulingDecision {
                app_id: AppId::new("web", "production"),
                placements: vec![Placement {
                    node_id: crate::meat::types::NodeId::new("node-1"),
                    resources: crate::meat::types::Resources::new(500, 256 * 1024 * 1024, 0),
                }],
            }),
            RaftRequest::ConfigSet {
                key: "max_apps".to_string(),
                value: "100".to_string(),
            },
            RaftRequest::Noop,
            RaftRequest::ManifestCommit(ManifestCommit {
                manifest: crate::pickle::types::ImageManifest {
                    digest: crate::pickle::types::Digest::from_sha256_hex(
                        "0000000000000000000000000000000000000000000000000000000000000001",
                    ),
                    config: crate::pickle::types::LayerDescriptor {
                        digest: crate::pickle::types::Digest::from_sha256_hex(
                            "0000000000000000000000000000000000000000000000000000000000000002",
                        ),
                        size: 1024,
                        media_type: "application/vnd.oci.image.config.v1+json".to_string(),
                    },
                    layers: vec![],
                    repository: "myapp".to_string(),
                    tags: std::collections::BTreeSet::new(),
                    total_size: 1024,
                    pushed_at: std::time::SystemTime::UNIX_EPOCH,
                    pushed_by: 1,
                },
                tag: "latest".to_string(),
                holder_nodes: std::collections::BTreeSet::from([1, 2]),
            }),
            RaftRequest::UpdateLayerLocations(UpdateLayerLocations {
                updates: vec![(
                    crate::pickle::types::Digest::from_sha256_hex(
                        "0000000000000000000000000000000000000000000000000000000000000003",
                    ),
                    std::collections::BTreeSet::from([1, 2, 3]),
                )],
            }),
            RaftRequest::GcReport(GcReport {
                node_id: 2,
                deleted_layers: vec![crate::pickle::types::Digest::from_sha256_hex(
                    "0000000000000000000000000000000000000000000000000000000000000004",
                )],
            }),
            RaftRequest::DeleteTag(DeleteTag {
                repository: "myapp".to_string(),
                tag: "old".to_string(),
            }),
        ];

        for req in &requests {
            let json = serde_json::to_string(req).unwrap();
            let decoded: RaftRequest = serde_json::from_str(&json).unwrap();
            assert_eq!(*req, decoded);
        }
    }

    #[test]
    fn council_config_default_values() {
        let cfg = CouncilConfig::default();
        assert_eq!(cfg.heartbeat_interval_ms, 150);
        assert_eq!(cfg.election_timeout_min_ms, 1000);
        assert_eq!(cfg.election_timeout_max_ms, 2000);
        assert_eq!(cfg.snapshot_threshold, 10_000);
        assert_eq!(cfg.max_in_snapshot_log_to_keep, 1000);
    }

    #[test]
    fn council_node_info_display() {
        let info = CouncilNodeInfo::new("127.0.0.1:9000".parse().unwrap(), "node-1");
        let s = info.to_string();
        assert!(s.contains("node-1"));
        assert!(s.contains("127.0.0.1:9000"));
    }

    #[test]
    fn desired_state_default_is_empty() {
        let state = DesiredState::default();
        assert!(state.apps.is_empty());
        assert!(state.scheduling.is_empty());
        assert!(state.config.is_empty());
        assert!(state.last_applied_log.is_none());
    }

    #[test]
    fn raft_request_variants_are_distinct() {
        let app_spec = RaftRequest::AppSpec {
            app_id: AppId::new("web", "default"),
            spec: Box::new(AppSpec {
                image: Some("img:v1".to_string()),
                ..default_spec()
            }),
        };
        let app_delete = RaftRequest::AppDelete {
            app_id: AppId::new("web", "default"),
        };
        let noop = RaftRequest::Noop;

        assert_ne!(app_spec, app_delete);
        assert_ne!(app_spec, noop);
        assert_ne!(app_delete, noop);
    }

    /// Helper to create a minimal AppSpec for tests.
    fn default_spec() -> AppSpec {
        toml::from_str(r#"image = "test:v1""#).unwrap()
    }
}
