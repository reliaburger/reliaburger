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
    AttachSignature, DeleteTag, GcReport, ManifestCatalog, ManifestCommit, UpdateLayerLocations,
};
use crate::sesame::types::SecurityState;

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
    /// Attach a cryptographic signature to an image manifest.
    AttachSignature(AttachSignature),
    /// Set the initial SecurityState during cluster bootstrap.
    SecurityStateInit(Box<SecurityState>),
    /// Add a new join token to the security state.
    CreateJoinToken(crate::sesame::types::JoinToken),
    /// Mark a join token as consumed (identified by hash).
    ConsumeJoinToken { token_hash: [u8; 32] },
    /// Add a new API token.
    CreateApiToken(crate::sesame::types::ApiToken),
    /// Revoke (remove) an API token by name.
    RevokeApiToken { name: String },
    /// Allocate the next certificate serial number.
    AllocateSerial,
    /// Start a secret key rotation: add a new age keypair, mark the
    /// current one as read-only.
    RotateSecretKey {
        scope: crate::sesame::types::AgeKeyScope,
        new_keypair: crate::sesame::types::AgeKeypair,
    },
    /// Finalise a secret key rotation: remove old read-only keypairs.
    FinalizeSecretRotation {
        scope: crate::sesame::types::AgeKeyScope,
    },
    /// Revoke a certificate by adding it to the CRL.
    RevokeCertificate(crate::sesame::types::CrlEntry),
    /// No-op entry (used for leader commit on election).
    Noop,
    // NOTE: the log is bincode-encoded (variant index on the wire), so new
    // variants are APPENDED here — never inserted or reordered — or every
    // existing log entry decodes as the wrong variant.
    /// Start or advance a cluster-wide rolling binary upgrade (Phase 14).
    UpgradeUpdate {
        state: Box<crate::upgrade::types::ClusterUpgradeState>,
    },
    /// Finish a cluster upgrade: archive it to history and clear the
    /// active slot.
    UpgradeClear { upgrade_id: String },
    /// Register a batch, allocating its id from the durable counter
    /// (12b.2 JOB4). The response carries the assigned id.
    BatchRegister {
        batch: crate::meat::batch_tracker::BatchRecord,
    },
    /// Record a job's state transition within a tracked batch. An
    /// unknown batch/job or an illegal transition is `Refused`.
    BatchJobUpdate {
        batch_id: u64,
        job_name: String,
        status: crate::meat::batch_tracker::JobStatus,
    },
    /// Register a build, allocating its id from the durable counter
    /// (12b.2 JOB4). The response carries the assigned id.
    BuildRegister {
        build: crate::bun::build_runner::BuildRecord,
    },
    /// Record a build's state transition. An unknown build or an
    /// illegal transition is `Refused`.
    BuildUpdate {
        build_id: u64,
        state: crate::bun::build_runner::BuildState,
    },
    /// Register or update a namespace's resource-quota specification
    /// (12b.2 T6). Namespaces are declarative desired state: the leader's
    /// scheduling pass builds its quota ledger from them.
    NamespaceSpec {
        name: String,
        spec: Box<crate::config::NamespaceSpec>,
    },
    /// Remove a namespace's quota spec.
    NamespaceDelete { name: String },
    /// Register or update a named permission grant (12b.2 T6).
    PermissionSpec {
        name: String,
        spec: Box<crate::config::PermissionSpec>,
    },
    /// Remove a named permission grant.
    PermissionDelete { name: String },
    /// Atomically consume a join token and allocate a certificate serial in
    /// one committed entry (PKI5). Marking the token consumed and allocating
    /// the serial were two separate Raft writes, so two racing joiners
    /// presenting the same token could each pass the consumed check before
    /// either write landed and double-issue. Applied against the replicated
    /// log, exactly one of the racers gets `JoinTokenConsumed`; the other,
    /// and every retry, gets `Refused`. The issuer signs only after this
    /// commit returns a serial.
    ConsumeJoinTokenForIssue { token_hash: [u8; 32] },
    /// Publish the cluster-wide service endpoint catalogue (12b.4). The
    /// leader rebuilds it from every node's health reports and replicates
    /// the whole catalogue as one entry, so every node's DNS and ingress can
    /// resolve services whose backends live on other nodes. Wholesale
    /// replacement (not a delta) keeps the apply idempotent and the leader
    /// authoritative — a follower never merges partial views.
    PublishEndpoints(Box<crate::onion::catalog::EndpointCatalog>),
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
    /// An `AllocateSerial` entry was applied; carries the serial assigned to
    /// *this* entry so the caller doesn't have to race to read it back from
    /// shared state (which returns duplicates under concurrent allocation).
    SerialAllocated { serial: u64 },
    /// A `GcReport` entry was applied; carries the digests the state
    /// machine approved for physical deletion. Nominations that would
    /// have removed a layer's last holder are absent — the proposing
    /// node must keep those blobs.
    GcApproved {
        approved: Vec<crate::pickle::types::Digest>,
    },
    // NOTE: like `RaftRequest`, responses ride the bincode-encoded wire,
    // so new variants are appended, never inserted or reordered.
    /// The entry was applied but the state machine refused to act on it —
    /// e.g. finalising a secret rotation while stored secrets are still
    /// sealed under an old generation (PKI8). The state is unchanged;
    /// `reason` tells the proposer why.
    Refused { reason: String },
    /// A `BatchRegister` entry was applied; carries the batch id the
    /// durable counter assigned to *this* entry (12b.2 JOB4).
    BatchRegistered { batch_id: u64 },
    /// A `BuildRegister` entry was applied; carries the assigned build id.
    BuildRegistered { build_id: u64 },
    /// A `ConsumeJoinTokenForIssue` entry was applied and this proposer won
    /// the token: it carries the serial allocated for the node certificate
    /// (PKI5). A racer that finds the token already consumed gets `Refused`.
    JoinTokenConsumed { serial: u64 },
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
    /// Cluster security state: CAs, tokens, age keypairs, OIDC config.
    /// Contains wrapped (encrypted) private keys — safe to replicate.
    #[serde(default)]
    pub security_state: SecurityState,
    /// The rolling binary upgrade in progress, if any (at most one).
    #[serde(default)]
    pub active_upgrade: Option<crate::upgrade::types::ClusterUpgradeState>,
    /// Completed/abandoned cluster upgrades, newest last (bounded to 20).
    #[serde(default)]
    pub upgrade_history: Vec<crate::upgrade::types::ClusterUpgradeState>,
    /// Durable batch tracker: monotonic id counter plus in-flight and
    /// recently terminal batches (12b.2 JOB4). Defaults empty so
    /// pre-12b.2 snapshots load cleanly.
    #[serde(default)]
    pub batch_state: crate::meat::batch_tracker::BatchDurableState,
    /// Durable build tracker, same shape and rationale as `batch_state`.
    #[serde(default)]
    pub build_state: crate::bun::build_runner::BuildDurableState,
    /// Monotonic disaster-recovery epoch (12b.2 D21/CP12). Zero on a cluster
    /// that has never been recovered; the recovery path stamps a strictly
    /// higher value into the restored state so anything issued before the
    /// loss (stale reports, tokens tied to the old term line) is
    /// distinguishable from post-recovery state. Defaults to zero so
    /// pre-12b.2 snapshots load cleanly.
    #[serde(default)]
    pub recovery_epoch: u64,
    /// Declared namespaces keyed by name (12b.2 T6). Their resource
    /// budgets feed the scheduler's quota ledger. Uses a `BTreeMap` so
    /// the JSON snapshot is deterministic. Defaults empty so pre-T6
    /// snapshots load cleanly.
    #[serde(default)]
    pub namespaces: std::collections::BTreeMap<String, crate::config::NamespaceSpec>,
    /// Declared permission grants keyed by name (12b.2 T6). Defaults
    /// empty so pre-T6 snapshots load cleanly.
    #[serde(default)]
    pub permissions: std::collections::BTreeMap<String, crate::config::PermissionSpec>,
    /// Cluster-wide service endpoint catalogue (12b.4): every namespace's
    /// services, their VIPs and healthy backends across all nodes. Built by
    /// the leader from health reports and replicated so any node resolves
    /// any service. Distinct from `manifest_catalog` (Pickle images).
    /// Defaults empty so pre-12b.4 snapshots load cleanly.
    #[serde(default)]
    pub endpoint_catalog: crate::onion::catalog::EndpointCatalog,
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
                    signature: None,
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
            RaftRequest::AttachSignature(AttachSignature {
                manifest_digest: crate::pickle::types::Digest::from_sha256_hex(
                    "0000000000000000000000000000000000000000000000000000000000000005",
                ),
                signature: crate::pickle::types::ImageSignature {
                    method: crate::pickle::types::SigningMethod::Keyless {
                        issuer: "https://test.reliaburger.dev".to_string(),
                        identity: "spiffe://test/ns/ci/job/build".to_string(),
                    },
                    signature: "MEUCIQD...".to_string(),
                    verification_material:
                        crate::pickle::types::VerificationMaterial::CertificateChain(vec![vec![
                            1, 2, 3,
                        ]]),
                    signed_at: std::time::SystemTime::UNIX_EPOCH,
                },
            }),
            RaftRequest::SecurityStateInit(Box::default()),
            RaftRequest::CreateJoinToken(crate::sesame::types::JoinToken {
                token_hash: [0xAB; 32],
                expires_at: std::time::SystemTime::UNIX_EPOCH,
                consumed: false,
                attestation_mode: crate::sesame::types::AttestationMode::None,
                node_id: "node-02".to_string(),
            }),
            RaftRequest::ConsumeJoinToken {
                token_hash: [0xAB; 32],
            },
            RaftRequest::CreateApiToken(crate::sesame::types::ApiToken {
                name: "ci-deploy".to_string(),
                token_hash: vec![1, 2, 3],
                token_salt: vec![4, 5, 6],
                role: crate::sesame::types::ApiRole::Deployer,
                scope: crate::sesame::types::TokenScope::default(),
                expires_at: None,
                created_at: std::time::SystemTime::UNIX_EPOCH,
            }),
            RaftRequest::RevokeApiToken {
                name: "ci-deploy".to_string(),
            },
            RaftRequest::AllocateSerial,
        ];

        for req in &requests {
            let json = serde_json::to_string(req).unwrap();
            let decoded: RaftRequest = serde_json::from_str(&json).unwrap();
            assert_eq!(*req, decoded);
        }
    }

    #[test]
    fn namespace_and_permission_raft_requests_round_trip() {
        let requests = vec![
            RaftRequest::NamespaceSpec {
                name: "team-backend".to_string(),
                spec: Box::new(crate::config::NamespaceSpec {
                    cpu: Some("8000m".to_string()),
                    memory: Some("16Gi".to_string()),
                    gpu: Some(2),
                    max_apps: Some(50),
                    max_replicas: Some(200),
                }),
            },
            RaftRequest::NamespaceDelete {
                name: "team-backend".to_string(),
            },
            RaftRequest::PermissionSpec {
                name: "deployer".to_string(),
                spec: Box::new(crate::config::PermissionSpec {
                    actions: vec!["deploy".to_string(), "scale".to_string()],
                    apps: vec!["web".to_string()],
                    namespaces: Some(vec!["production".to_string()]),
                }),
            },
            RaftRequest::PermissionDelete {
                name: "deployer".to_string(),
            },
        ];
        for req in &requests {
            let json = serde_json::to_string(req).unwrap();
            let decoded: RaftRequest = serde_json::from_str(&json).unwrap();
            assert_eq!(*req, decoded);
        }
    }

    #[test]
    fn pre_theme_snapshot_without_namespaces_loads_cleanly() {
        // A snapshot serialised before T6 added `namespaces`/`permissions`
        // has neither key. The `#[serde(default)]` on both must fill them
        // with empty maps rather than fail to deserialise (the #83 loader
        // is strict, so a missing-field error here would brick startup).
        let legacy = serde_json::json!({
            "apps": [],
            "scheduling": [],
            "config": {},
            "last_applied_log": null,
            "last_membership": { "log_id": null, "membership": { "configs": [], "nodes": {} } }
        });
        let state: DesiredState = serde_json::from_value(legacy).unwrap();
        assert!(state.namespaces.is_empty());
        assert!(state.permissions.is_empty());
        assert!(state.apps.is_empty());
        // 12b.4: the endpoint catalogue is serde-default too, so a snapshot
        // that predates it loads with an empty catalogue rather than failing.
        assert!(state.endpoint_catalog.is_empty());
    }

    #[test]
    fn pre_theme_snapshot_without_endpoint_catalog_loads_cleanly() {
        // A snapshot serialised after T6 but before 12b.4 has `namespaces`
        // and `permissions` but no `endpoint_catalog`. The `#[serde(default)]`
        // must fill it with an empty catalogue.
        let legacy = serde_json::json!({
            "apps": [],
            "scheduling": [],
            "config": {},
            "namespaces": {},
            "permissions": {},
            "last_applied_log": null,
            "last_membership": { "log_id": null, "membership": { "configs": [], "nodes": {} } }
        });
        let state: DesiredState = serde_json::from_value(legacy).unwrap();
        assert!(state.endpoint_catalog.is_empty());
    }

    #[test]
    fn publish_endpoints_raft_request_round_trips() {
        use crate::onion::catalog::{CatalogBackend, EndpointCatalog};
        use crate::onion::service_id::ServiceId;

        let catalog = EndpointCatalog::rebuild([(
            ServiceId::new("payments", "api"),
            3000,
            vec![CatalogBackend {
                node_id: "node-b".to_string(),
                node_ip: std::net::Ipv4Addr::new(10, 0, 0, 2),
                host_port: 30002,
                healthy: true,
            }],
        )]);
        let req = RaftRequest::PublishEndpoints(Box::new(catalog));
        let json = serde_json::to_string(&req).unwrap();
        let decoded: RaftRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(req, decoded);
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
