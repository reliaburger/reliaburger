/// Bun agent event loop.
///
/// Ties the supervisor, health checker, and container runtime together
/// into a single async event loop. Commands arrive over an `mpsc` channel;
/// health checks fire on a timer; shutdown is coordinated via a
/// `CancellationToken`.
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Instant, SystemTime};

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::config::app::AppSpec;
use crate::config::job::JobSpec;
use crate::council::node::CouncilNode;
use crate::council::types::CouncilNodeInfo;
use crate::grill::oci::generate_job_oci_spec;
use crate::grill::port::PortAllocator;
use crate::grill::state::ContainerState;
use crate::grill::{Grill, InstanceId};
use crate::mustard::membership::MembershipSnapshot;
use crate::reporting::worker::CollectSnapshotRequest;

use super::BunError;
use super::probe::probe_health;
use super::supervisor::{WorkloadInstance, WorkloadSupervisor};

/// Maximum time an init container may run before the deploy fails. Bounds the
/// init wait so a hung init can't wedge the agent event loop indefinitely.
const INIT_TIMEOUT_SECS: u64 = 300;

/// Grace period between SIGTERM and SIGKILL during shutdown.
const SHUTDOWN_GRACE_SECS: u64 = 5;

/// The address to probe an instance's health check at.
///
/// A container with its own IP (runc/apple per-container netns) is probed at
/// that IP; ProcessGrill shares the host network, so it falls back to loopback.
/// Previously hardcoded to loopback, which flapped every runc app unhealthy.
fn probe_host(container_ip: Option<std::net::Ipv4Addr>) -> String {
    container_ip
        .map(|ip| ip.to_string())
        .unwrap_or_else(|| "127.0.0.1".to_string())
}

/// A progress event emitted during a deploy operation.
///
/// Sent over an `mpsc` channel so the API layer can stream events
/// to the client via SSE. The client displays `Progress` messages
/// in real time and collects the final `Complete` or `Error` event.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ApplyEvent {
    /// Informational progress update.
    Progress { message: String },
    /// A single instance was created and started.
    InstanceCreated { id: String, app: String },
    /// The deploy finished successfully.
    Complete {
        created: usize,
        instances: Vec<String>,
    },
    /// The deploy failed.
    Error { message: String },
}

/// Commands sent to the agent over the command channel.
pub enum AgentCommand {
    /// Deploy workloads from a parsed Config.
    ///
    /// Progress events are streamed over the `events` channel so the
    /// API can relay them to the client in real time.
    Deploy {
        config: Config,
        events: mpsc::Sender<ApplyEvent>,
    },
    /// Stop all instances of an app in a namespace.
    Stop {
        app_name: String,
        namespace: String,
        response: oneshot::Sender<Result<(), BunError>>,
    },
    /// Get status of all instances.
    Status {
        response: oneshot::Sender<Vec<InstanceStatus>>,
    },
    /// Get the image references of all current instances (for GC
    /// protection: actively deployed images must not be collected).
    ActiveImages {
        response: oneshot::Sender<std::collections::HashSet<String>>,
    },
    /// Get logs for an app.
    Logs {
        app_name: String,
        namespace: String,
        tail: Option<usize>,
        response: oneshot::Sender<Result<String, BunError>>,
    },
    /// Follow logs for an app (streaming).
    FollowLogs {
        app_name: String,
        namespace: String,
        tail: Option<usize>,
        lines: mpsc::Sender<String>,
    },
    /// Execute a command inside a running instance.
    Exec {
        app_name: String,
        namespace: String,
        command: Vec<String>,
        response: oneshot::Sender<Result<String, BunError>>,
    },
    /// Get cluster node membership from the gossip layer.
    Nodes {
        response: oneshot::Sender<Vec<NodeStatus>>,
    },
    /// Get council (Raft) status.
    Council {
        response: oneshot::Sender<CouncilStatus>,
    },
    /// Join an existing cluster.
    Join {
        token: String,
        addr: String,
        response: oneshot::Sender<Result<String, BunError>>,
    },
    /// Inject a network partition (chaos testing).
    InjectPartition {
        peers: Vec<String>,
        duration_secs: u64,
        response: oneshot::Sender<Result<String, BunError>>,
    },
    /// Remove all network partitions (chaos testing).
    HealPartition {
        response: oneshot::Sender<Result<String, BunError>>,
    },
    /// Query active chaos state.
    ChaosStatus {
        response: oneshot::Sender<ChaosState>,
    },
    /// Resolve a service name to its VIP and backends.
    Resolve {
        app_name: String,
        response: oneshot::Sender<Option<crate::onion::types::ResolveResponse>>,
    },
    /// List all registered services.
    ResolveAll {
        response: oneshot::Sender<Vec<crate::onion::types::ResolveResponse>>,
    },
    /// List all ingress routes.
    Routes {
        response: oneshot::Sender<Vec<crate::wrapper::types::RouteInfo>>,
    },
    /// Inject a fault (Smoker).
    InjectFault {
        request: crate::smoker::types::FaultRequest,
        response: oneshot::Sender<Result<crate::smoker::types::FaultSummary, BunError>>,
    },
    /// Clear a specific fault by ID.
    ClearFault {
        fault_id: u64,
        response: oneshot::Sender<Result<String, BunError>>,
    },
    /// Clear all active faults.
    ClearAllFaults {
        response: oneshot::Sender<Result<String, BunError>>,
    },
    /// List all active faults.
    ListFaults {
        response: oneshot::Sender<Vec<crate::smoker::types::FaultSummary>>,
    },
    /// Sign an image manifest digest and attach the signature via Raft.
    SignImage {
        manifest_digest: String,
        response: oneshot::Sender<Result<String, BunError>>,
    },
    /// Get the deployed AppSpec for a specific app (for safe env display).
    AppConfig {
        app_name: String,
        namespace: String,
        response: oneshot::Sender<Option<AppSpec>>,
    },
    /// Apply a node-level upgrade directive (Phase 14). Responds Ok once
    /// the upgrade is verified + staged; the exec happens just after.
    UpgradeApply {
        directive: crate::upgrade::types::UpgradeDirective,
        response: oneshot::Sender<Result<(), BunError>>,
    },
    /// Node-level upgrade status.
    UpgradeStatus {
        response: oneshot::Sender<Result<crate::upgrade::types::NodeUpgradeStatus, BunError>>,
    },
    /// Revert this node to a previous binary version.
    UpgradeRollback {
        version: Option<crate::upgrade::BinaryVersion>,
        response: oneshot::Sender<Result<(), BunError>>,
    },
    /// Post-boot self-verification of a freshly swapped-in version.
    /// Commits on success; flags revert and exits on failure.
    UpgradeVerify {
        marker: crate::upgrade::marker::UpgradeMarker,
        response: oneshot::Sender<Result<bool, BunError>>,
    },
}

/// Active chaos fault injection state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChaosState {
    /// Currently active partition, if any.
    pub active_partition: Option<PartitionInfo>,
}

/// Details of an active partition injection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PartitionInfo {
    /// Addresses being blocked.
    pub peers: Vec<String>,
    /// When the partition was injected (seconds since UNIX epoch).
    pub injected_at_epoch: u64,
    /// Duration in seconds before auto-heal.
    pub duration_secs: u64,
    /// Seconds remaining before auto-heal.
    pub remaining_secs: u64,
}

/// Result of a deploy operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyResult {
    /// Number of instances created.
    pub created: usize,
    /// Instance IDs that were created.
    pub instances: Vec<String>,
}

/// Status of a single workload instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstanceStatus {
    /// Instance ID.
    pub id: String,
    /// App name.
    pub app_name: String,
    /// Namespace.
    pub namespace: String,
    /// Current lifecycle state.
    pub state: String,
    /// Number of restarts.
    pub restart_count: u32,
    /// Allocated host port, if any.
    pub host_port: Option<u16>,
    /// OS process ID, if available.
    pub pid: Option<u32>,
}

/// Status of a single cluster node, as returned by the nodes API.
///
/// Flat, wire-friendly representation of `NodeMembership`. Uses strings
/// instead of newtypes and omits `Instant` fields (not serialisable).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeStatus {
    /// Node identifier.
    pub node_id: String,
    /// Node address (gossip endpoint).
    pub address: String,
    /// Current SWIM state: "alive", "suspect", "dead", or "left".
    pub state: String,
    /// SWIM incarnation number.
    pub incarnation: u64,
    /// Whether this node is a council (Raft voter) member.
    pub is_council: bool,
    /// Whether this node is the current Raft leader.
    pub is_leader: bool,
    /// Node labels (zone, region, etc.).
    pub labels: BTreeMap<String, String>,
}

/// Info about a single council member, as returned by the council API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CouncilMemberInfo {
    /// Raft numeric node ID.
    pub raft_id: u64,
    /// Human-readable node name (maps to `NodeId`).
    pub name: String,
    /// Raft RPC address.
    pub address: String,
}

/// Status of the Raft council, as returned by the council API.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CouncilStatus {
    /// Council member nodes.
    pub members: Vec<CouncilMemberInfo>,
    /// Current leader node name, if known.
    pub leader: Option<String>,
    /// Current Raft term.
    pub term: u64,
    /// Last applied log index.
    pub last_applied_log: Option<u64>,
    /// Number of registered apps in desired state.
    pub app_count: usize,
}

/// Optional cluster subsystem references.
///
/// Holds communication channels to gossip, Raft, and reporting subsystems.
/// `None` when running in single-node mode (no cluster config).
pub struct ClusterHandle {
    /// Membership snapshots from the gossip layer.
    pub membership_rx: watch::Receiver<Vec<MembershipSnapshot>>,
    /// Raft metrics (if this node is a council member).
    pub raft_metrics_rx: Option<watch::Receiver<openraft::RaftMetrics<u64, CouncilNodeInfo>>>,
    /// Council node handle (if this node is a council member).
    pub council: Option<Arc<CouncilNode>>,
    /// Channel for receiving snapshot requests from the reporting worker.
    pub snapshot_rx: mpsc::Receiver<CollectSnapshotRequest>,
    /// Master secret for unwrapping CA private keys during join/CSR operations.
    pub wrapping_ikm: Option<[u8; 32]>,
    /// Gossip + Raft transport blocklists (chaos partitions populate
    /// these to drop traffic to specific peers). Empty in tests that
    /// don't exercise partitions.
    pub partition_blocklists: PartitionBlocklists,
}

/// The transport blocklists a chaos partition manipulates, plus the
/// gossip→raft port offset needed to derive a peer's Raft address from
/// its gossip address.
#[derive(Clone, Default)]
pub struct PartitionBlocklists {
    pub gossip: Option<Arc<tokio::sync::RwLock<std::collections::HashSet<std::net::SocketAddr>>>>,
    pub raft: Option<Arc<tokio::sync::RwLock<std::collections::HashSet<std::net::SocketAddr>>>>,
    /// raft_port - gossip_port, to map a peer's gossip addr → raft addr.
    pub raft_port_offset: i32,
}

/// Egress enforcement bound to a running instance's cgroup (L16).
#[cfg(feature = "ebpf")]
#[derive(Clone)]
struct EgressBinding {
    /// The instance's cgroup id (key into the egress maps).
    cgroup_id: u64,
    /// The raw `[egress] allow` list, re-resolved periodically.
    allow: Vec<String>,
    /// The destinations currently programmed into the egress map.
    resolved: Vec<(std::net::Ipv4Addr, u16)>,
}

/// The Bun agent. Generic over `G: Grill` so tests can inject mocks.
pub struct BunAgent<G: Grill> {
    supervisor: WorkloadSupervisor<G>,
    command_rx: mpsc::Receiver<AgentCommand>,
    shutdown: CancellationToken,
    volumes_dir: PathBuf,
    cluster: Option<ClusterHandle>,
    /// Smoker fault registry — active faults on this node.
    fault_registry: crate::smoker::registry::FaultRegistry,
    /// eBPF program handle for writing fault maps (Linux + ebpf feature only).
    /// `None` on macOS or when eBPF is not loaded.
    #[cfg(feature = "ebpf")]
    onion_ebpf: Option<std::sync::Arc<tokio::sync::Mutex<crate::onion::ebpf::loader::OnionEbpf>>>,
    /// Egress enforcement state per instance with an allowlist: its cgroup
    /// id, the raw allow list, and the last-resolved destinations — so
    /// enforcement can be lifted on stop and the allowlist re-resolved as
    /// DNS changes (L16).
    #[cfg(feature = "ebpf")]
    egress_bindings: std::collections::HashMap<InstanceId, EgressBinding>,
    /// Ticks since the last egress re-resolution (the event loop runs at 1s).
    #[cfg(feature = "ebpf")]
    egress_reresolve_ticks: u32,
    /// Onion service map: app names → VIPs + backends.
    service_map: crate::onion::service_map::ServiceMap,
    /// Publisher for service-map snapshots (DNS responder subscribes).
    service_map_tx: tokio::sync::watch::Sender<crate::onion::service_map::ServiceMap>,
    /// Wrapper routing table (shared with the proxy via Arc<RwLock>).
    routing_table: std::sync::Arc<tokio::sync::RwLock<crate::wrapper::routing::RoutingTable>>,
    /// Ingress configs for deployed apps (app_name → IngressSpec).
    ingress_configs: std::collections::HashMap<String, crate::config::app::IngressSpec>,
    /// Perimeter firewall config. Disabled in rootless mode.
    perimeter_config: crate::firewall::rules::PerimeterConfig,
    /// Last applied cluster-node set for firewall reconciliation. `None`
    /// until the first apply, so a standalone node (empty set) still gets
    /// the firewall; comparing the set (not a count) catches node swaps (M18).
    last_firewall_nodes: Option<crate::firewall::rules::ClusterNodes>,
    /// Deploy history (shared with API for query access).
    pub(crate) deploy_history:
        Arc<tokio::sync::RwLock<Vec<crate::meat::deploy_types::DeployHistoryEntry>>>,
    /// Pre-created network namespace paths for instances (Linux + runc only).
    /// When present, the namespace path is passed to `generate_oci_spec` so
    /// the container joins the pre-created namespace instead of creating one.
    netns_paths: std::collections::HashMap<InstanceId, std::path::PathBuf>,
    /// Deployed app specs, keyed by (app_name, namespace). Stored so the
    /// Brioche UI can display environment variables with encrypted values
    /// masked as `[encrypted]`.
    deployed_specs: std::collections::HashMap<(String, String), AppSpec>,
    /// Monotonic counter tagging each rolling-redeploy's new instance IDs.
    /// A wall-clock generation collided when two redeploys landed in the
    /// same second; this never repeats within a process.
    next_deploy_gen: u64,
    /// Sink for container log lines. When set, each started instance spawns a
    /// forwarder that streams its output here (drained into the LogStore).
    log_tx: Option<mpsc::Sender<crate::ketchup::types::LogRecord>>,
    /// Schedulable CPU capacity (system total minus `[resources]`
    /// reserved), reported to the cluster. Zero until the binary sets it.
    capacity_cpu_millicores: u32,
    /// Schedulable memory capacity, reported to the cluster.
    capacity_memory_mb: u32,
    /// Image trust policy. When `require_signatures` is set, deploys of
    /// Pickle-hosted images are gated on a valid signature. Defaults to
    /// permissive so single-node / untrusted setups are unaffected.
    trust_policy: crate::config::node::TrustPolicySection,
    /// Directory for on-disk instance records ({data_dir}/instances).
    /// When set, started instances are recorded so a future bun (after a
    /// crash restart or a self-upgrade exec) can adopt them instead of
    /// restarting them. `None` disables recording and adoption.
    records_dir: Option<PathBuf>,
    /// Self-upgrade manager. `None` when upgrades are not configured
    /// (upgrade commands then answer with an error).
    upgrade: Option<crate::upgrade::manager::UpgradeManager>,
    /// Set while an upgrade is staged/executing: new deploys are refused,
    /// running workloads are untouched.
    draining: Arc<std::sync::atomic::AtomicBool>,
}

impl<G: Grill + Clone + 'static> BunAgent<G> {
    /// Create a new agent in single-node mode (no cluster).
    pub fn new(
        grill: G,
        port_allocator: PortAllocator,
        command_rx: mpsc::Receiver<AgentCommand>,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            supervisor: WorkloadSupervisor::new(grill, port_allocator),
            command_rx,
            shutdown,
            volumes_dir: crate::config::node::StorageSection::default().volumes,
            cluster: None,
            fault_registry: crate::smoker::registry::FaultRegistry::new(),
            #[cfg(feature = "ebpf")]
            onion_ebpf: None,
            #[cfg(feature = "ebpf")]
            egress_bindings: std::collections::HashMap::new(),
            #[cfg(feature = "ebpf")]
            egress_reresolve_ticks: 0,
            service_map: crate::onion::service_map::ServiceMap::new(),
            service_map_tx: tokio::sync::watch::channel(
                crate::onion::service_map::ServiceMap::new(),
            )
            .0,
            routing_table: std::sync::Arc::new(tokio::sync::RwLock::new(
                crate::wrapper::routing::RoutingTable::new(),
            )),
            ingress_configs: std::collections::HashMap::new(),
            // Single-node mode: no nftables needed (no cluster ports to protect)
            perimeter_config: crate::firewall::rules::PerimeterConfig {
                enabled: false,
                ..Default::default()
            },
            last_firewall_nodes: None,
            deploy_history: Arc::new(tokio::sync::RwLock::new(Vec::new())),
            netns_paths: std::collections::HashMap::new(),
            deployed_specs: std::collections::HashMap::new(),
            next_deploy_gen: 1,
            log_tx: None,
            capacity_cpu_millicores: 0,
            capacity_memory_mb: 0,
            trust_policy: crate::config::node::TrustPolicySection::default(),
            records_dir: None,
            upgrade: None,
            draining: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Create a new agent with cluster subsystem handles.
    pub fn with_cluster(
        grill: G,
        port_allocator: PortAllocator,
        command_rx: mpsc::Receiver<AgentCommand>,
        shutdown: CancellationToken,
        cluster: ClusterHandle,
    ) -> Self {
        Self {
            supervisor: WorkloadSupervisor::new(grill, port_allocator),
            command_rx,
            shutdown,
            volumes_dir: crate::config::node::StorageSection::default().volumes,
            cluster: Some(cluster),
            fault_registry: crate::smoker::registry::FaultRegistry::new(),
            #[cfg(feature = "ebpf")]
            onion_ebpf: None,
            #[cfg(feature = "ebpf")]
            egress_bindings: std::collections::HashMap::new(),
            #[cfg(feature = "ebpf")]
            egress_reresolve_ticks: 0,
            service_map: crate::onion::service_map::ServiceMap::new(),
            service_map_tx: tokio::sync::watch::channel(
                crate::onion::service_map::ServiceMap::new(),
            )
            .0,
            routing_table: std::sync::Arc::new(tokio::sync::RwLock::new(
                crate::wrapper::routing::RoutingTable::new(),
            )),
            ingress_configs: std::collections::HashMap::new(),
            #[cfg(target_os = "linux")]
            perimeter_config: if crate::grill::rootless::is_rootless() {
                crate::firewall::rules::PerimeterConfig::for_rootless()
            } else {
                crate::firewall::rules::PerimeterConfig::default()
            },
            #[cfg(not(target_os = "linux"))]
            perimeter_config: crate::firewall::rules::PerimeterConfig {
                enabled: false,
                ..Default::default()
            },
            last_firewall_nodes: None,
            deploy_history: Arc::new(tokio::sync::RwLock::new(Vec::new())),
            netns_paths: std::collections::HashMap::new(),
            deployed_specs: std::collections::HashMap::new(),
            next_deploy_gen: 1,
            log_tx: None,
            capacity_cpu_millicores: 0,
            capacity_memory_mb: 0,
            trust_policy: crate::config::node::TrustPolicySection::default(),
            records_dir: None,
            upgrade: None,
            draining: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    /// Get a shared handle to the deploy history for the API.
    pub fn deploy_history_handle(
        &self,
    ) -> Arc<tokio::sync::RwLock<Vec<crate::meat::deploy_types::DeployHistoryEntry>>> {
        Arc::clone(&self.deploy_history)
    }

    /// Set the sink that container log lines are forwarded to.
    ///
    /// The binary drains this into the LogStore so container output is
    /// queryable. Without it, container output is only reachable live via
    /// `relish logs` (which asks the runtime directly).
    pub fn set_log_sink(&mut self, log_tx: mpsc::Sender<crate::ketchup::types::LogRecord>) {
        self.log_tx = Some(log_tx);
    }

    /// Get a shared handle to the ingress routing table.
    ///
    /// The Wrapper proxy reads routes from this table; the agent
    /// rebuilds it on every deploy, stop, and health change.
    pub fn routing_table_handle(
        &self,
    ) -> Arc<tokio::sync::RwLock<crate::wrapper::routing::RoutingTable>> {
        Arc::clone(&self.routing_table)
    }

    /// Subscribe to service-map snapshots.
    ///
    /// The agent publishes a snapshot whenever the map changes (same
    /// cadence as routing-table rebuilds). The DNS responder resolves
    /// `.internal` names from these snapshots.
    pub fn service_map_watch(
        &self,
    ) -> tokio::sync::watch::Receiver<crate::onion::service_map::ServiceMap> {
        self.service_map_tx.subscribe()
    }

    /// Set the image trust policy (from node config). When it requires
    /// signatures, deploys verify Pickle-hosted images before creating them.
    pub fn set_trust_policy(&mut self, trust_policy: crate::config::node::TrustPolicySection) {
        self.trust_policy = trust_policy;
    }

    /// Set the node's schedulable capacity (system totals minus the
    /// `[resources]` reservation). Reported in every StateReport.
    pub fn set_node_capacity(&mut self, cpu_millicores: u32, memory_mb: u32) {
        self.capacity_cpu_millicores = cpu_millicores;
        self.capacity_memory_mb = memory_mb;
    }

    /// Attach a loaded eBPF handle so the agent can write fault and
    /// egress map entries (L8). Only present with the `ebpf` feature;
    /// `bun` calls this at startup when `[ebpf] enabled`.
    #[cfg(feature = "ebpf")]
    pub fn set_onion_ebpf(
        &mut self,
        ebpf: std::sync::Arc<tokio::sync::Mutex<crate::onion::ebpf::loader::OnionEbpf>>,
    ) {
        self.onion_ebpf = Some(ebpf);
    }

    /// Mirror an app's current service-map entry into the kernel
    /// `backend_map` so the eBPF connect hook rewrites its VIP to live
    /// backends (L8 completeness). Called after every service-map add /
    /// health change. A no-op without the eBPF data path loaded.
    #[cfg(feature = "ebpf")]
    async fn sync_backend_ebpf(&self, app_name: &str) {
        let Some(handle) = self.onion_ebpf.as_ref() else {
            return;
        };
        let Some(entry) = self.service_map.resolve(app_name).cloned() else {
            return;
        };
        let bpf = crate::onion::ebpf::maps::BpfServiceMap::new();
        let mut ebpf = handle.lock().await;
        bpf.update_backends_bpf(&mut ebpf, entry.vip, entry.port, &entry);
    }

    #[cfg(not(feature = "ebpf"))]
    async fn sync_backend_ebpf(&self, _app_name: &str) {}

    /// Drop an app's `backend_map` entry. Must be called *before* the app
    /// is unregistered from the service map, while its VIP/port are still
    /// known. A no-op without the eBPF data path loaded.
    #[cfg(feature = "ebpf")]
    async fn remove_backend_ebpf(&self, app_name: &str) {
        let Some(handle) = self.onion_ebpf.as_ref() else {
            return;
        };
        let Some(port) = self.service_map.resolve(app_name).map(|e| e.port) else {
            return;
        };
        let vip = crate::onion::vip::VirtualIP::from_app_name(app_name);
        let bpf = crate::onion::ebpf::maps::BpfServiceMap::new();
        let mut ebpf = handle.lock().await;
        bpf.remove_backends_bpf(&mut ebpf, vip, port);
    }

    #[cfg(not(feature = "ebpf"))]
    async fn remove_backend_ebpf(&self, _app_name: &str) {}

    /// Enable on-disk instance records under `dir` ({data_dir}/instances).
    /// Call before deploying anything; also enables `adopt_recorded_instances`.
    pub fn set_records_dir(&mut self, dir: PathBuf) {
        self.records_dir = Some(dir);
    }

    /// Attach the self-upgrade manager (enables the upgrade commands).
    pub fn set_upgrade_manager(&mut self, manager: crate::upgrade::manager::UpgradeManager) {
        self.upgrade = Some(manager);
    }

    /// Snapshot of running (non-job) workloads for the upgrade marker:
    /// these must all still be alive after the swap for it to commit.
    async fn upgrade_inventory(&self) -> Vec<crate::upgrade::marker::InstanceInventory> {
        let mut inventory = Vec::new();
        for instance in self.supervisor.list_instances() {
            if instance.is_job || instance.state != ContainerState::Running {
                continue;
            }
            let Some(pid) = self.supervisor.grill().pid(&instance.id).await else {
                continue;
            };
            let replica_index: u32 = instance
                .id
                .0
                .rsplit('-')
                .next()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            inventory.push(crate::upgrade::marker::InstanceInventory {
                namespace: instance.namespace.clone(),
                app_name: instance.app_name.clone(),
                instance_id: replica_index,
                pid,
            });
        }
        inventory
    }

    /// Check that every pre-upgrade workload survived the swap.
    async fn verify_upgrade_inventory(
        &self,
        marker: &crate::upgrade::marker::UpgradeMarker,
    ) -> Result<(), String> {
        for item in &marker.pre_upgrade_instances {
            let id = InstanceId(format!("{}-{}", item.app_name, item.instance_id));
            match self.supervisor.get_instance(&id) {
                Some(instance) if instance.state == ContainerState::Running => {}
                Some(instance) => {
                    return Err(format!(
                        "instance {id} is {} (was running before the upgrade)",
                        instance.state
                    ));
                }
                None => {
                    return Err(format!("instance {id} was not adopted after the upgrade"));
                }
            }
        }
        Ok(())
    }

    /// Write (or refresh) the instance record used for adoption after a bun
    /// restart or self-upgrade exec. Best-effort: a failed record write must
    /// never fail a deploy, but it is logged.
    async fn persist_instance_record(&self, instance_id: &InstanceId) {
        let Some(dir) = &self.records_dir else { return };
        let Some(instance) = self.supervisor.get_instance(instance_id) else {
            return;
        };
        let Some(pid) = self.supervisor.grill().pid(instance_id).await else {
            return;
        };
        let Some(pid_started_at) = crate::grill::records::process_start_time(pid) else {
            return;
        };
        let Some(oci_spec) = instance.oci_spec.clone() else {
            return;
        };

        let replica_index: u32 = instance_id
            .0
            .rsplit('-')
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let runtime = self.supervisor.grill().runtime_kind();
        let record = crate::grill::records::InstanceRecord {
            schema: 1,
            instance_id: instance_id.0.clone(),
            namespace: instance.namespace.clone(),
            app_name: instance.app_name.clone(),
            replica_index,
            is_job: instance.is_job,
            image: instance.image.clone(),
            runtime,
            pid,
            pid_started_at,
            // RunC uses the instance id as the container id (see runc.rs).
            runc_container_id: matches!(runtime, crate::grill::records::RuntimeKind::Runc)
                .then(|| instance_id.0.clone()),
            log_stem: self.supervisor.grill().log_stem(instance_id).await,
            host_port: instance.host_port,
            app_spec: self
                .deployed_specs
                .get(&(instance.app_name.clone(), instance.namespace.clone()))
                .cloned(),
            oci_spec,
        };
        if let Err(e) = crate::grill::records::write_record(dir, &record) {
            eprintln!("bun: warning: failed to write instance record for {instance_id}: {e}");
        }
    }

    /// Remove an instance's adoption record (instance stopped for good).
    fn remove_instance_record(&self, instance_id: &InstanceId) {
        if let Some(dir) = &self.records_dir
            && let Err(e) = crate::grill::records::remove_record(dir, &instance_id.0)
        {
            eprintln!("bun: warning: failed to remove instance record for {instance_id}: {e}");
        }
    }

    /// Adopt still-running workloads recorded by a previous bun process.
    ///
    /// Called once at startup, BEFORE any reconciliation: adopted instances
    /// are seeded into the supervisor as Running so they don't get
    /// double-started. Records whose process is gone are deleted (the
    /// instance reschedules through the normal path). Returns the number
    /// of instances adopted.
    ///
    /// Restart backoff counters start fresh for adopted instances, and
    /// cluster routing is rebuilt by the normal reconcile paths.
    pub async fn adopt_recorded_instances(&mut self) -> usize {
        let Some(dir) = self.records_dir.clone() else {
            return 0;
        };
        let now = Instant::now();
        let mut adopted_count = 0;

        for record in crate::grill::records::load_records(&dir) {
            let instance_id = InstanceId(record.instance_id.clone());
            // Never clobber an instance the current process already tracks.
            if self.supervisor.get_instance(&instance_id).is_some() {
                continue;
            }

            let adopted = matches!(
                self.supervisor.grill().adopt(&instance_id, &record).await,
                Ok(true)
            );
            if !adopted {
                let _ = crate::grill::records::remove_record(&dir, &record.instance_id);
                continue;
            }

            // The surviving instance still holds its port.
            if let Some(port) = record.host_port {
                let _ = self.supervisor.port_allocator.reserve(port).await;
            }

            // Rebuild the health check from the recorded app spec.
            let health_config = record.app_spec.as_ref().and_then(|spec| {
                let health = spec.health.as_ref()?;
                let port = spec.port?;
                Some(super::health::HealthCheckConfig::from_spec(health, port))
            });
            if let Some(config) = &health_config {
                self.supervisor
                    .register_health(instance_id.clone(), config.clone(), now);
            }

            let key = (record.app_name.clone(), record.namespace.clone());
            let instance = WorkloadInstance {
                id: instance_id.clone(),
                app_name: record.app_name.clone(),
                namespace: record.namespace.clone(),
                state: ContainerState::Running,
                health_counters: super::health::HealthCounters::new(),
                restart_count: 0,
                last_restart: None,
                host_port: record.host_port,
                container_ip: None,
                created_at: now,
                restart_policy: super::restart::RestartPolicy::default(),
                health_config,
                is_job: record.is_job,
                image: record.image.clone(),
                oci_spec: Some(record.oci_spec.clone()),
                identity: None,
                identity_mount: None,
            };
            self.supervisor
                .instances
                .insert(instance_id.clone(), instance);
            self.supervisor
                .app_instances
                .entry(key.clone())
                .or_default()
                .push(instance_id.clone());
            if let Some(spec) = record.app_spec {
                self.deployed_specs.insert(key, spec);
            }
            // Keep the adopted instance's output flowing into the log store.
            self.spawn_log_forwarder(&instance_id, &record.app_name, &record.namespace);
            adopted_count += 1;
        }

        if adopted_count > 0 {
            println!("bun: adopted {adopted_count} running instance(s) from a previous process");
        }
        adopted_count
    }

    /// Spawn a background forwarder that streams a started instance's log lines
    /// into the configured log sink. No-op if no sink is set.
    ///
    /// Runs off the event loop (the grill handle is cloned into the task), so
    /// following logs never blocks the agent.
    fn spawn_log_forwarder(&self, instance_id: &InstanceId, app_name: &str, namespace: &str) {
        let Some(log_tx) = self.log_tx.clone() else {
            return;
        };
        let grill = self.supervisor.grill().clone();
        let id = instance_id.clone();
        let app = app_name.to_string();
        let namespace = namespace.to_string();

        let (line_tx, mut line_rx) = mpsc::channel::<String>(256);
        // Producer: the runtime streams complete stdout lines into line_tx.
        let follow_grill = grill;
        let follow_id = id.clone();
        tokio::spawn(async move {
            follow_grill.follow_logs(&follow_id, line_tx).await;
        });
        // Consumer: tag each line and forward it to the log sink.
        tokio::spawn(async move {
            while let Some(line) = line_rx.recv().await {
                let record = crate::ketchup::types::LogRecord {
                    app: app.clone(),
                    namespace: namespace.clone(),
                    stream: crate::ketchup::types::LogStream::Stdout,
                    line,
                };
                if log_tx.send(record).await.is_err() {
                    break;
                }
            }
        });
    }

    /// Run the agent event loop until shutdown is requested.
    pub async fn run(&mut self) {
        let mut health_interval = tokio::time::interval(std::time::Duration::from_secs(1));

        loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => {
                    self.shutdown_all().await;
                    break;
                }
                Some(cmd) = self.command_rx.recv() => {
                    self.handle_command(cmd).await;
                }
                Some(req) = Self::recv_snapshot(&mut self.cluster) => {
                    self.handle_snapshot_request(req);
                }
                _ = health_interval.tick() => {
                    self.run_health_checks().await;
                    self.check_jobs().await;
                    self.check_apps().await;
                    self.drive_pending_restarts().await;
                    self.expire_faults().await;
                    self.reconcile_firewall().await;
                    self.reresolve_egress().await;
                    self.check_identity_rotation().await;
                }
            }
        }
    }

    /// Receive a snapshot request from the cluster handle, or pend forever.
    async fn recv_snapshot(cluster: &mut Option<ClusterHandle>) -> Option<CollectSnapshotRequest> {
        match cluster {
            Some(handle) => handle.snapshot_rx.recv().await,
            None => std::future::pending().await,
        }
    }

    /// Handle a snapshot request from the reporting worker.
    fn handle_snapshot_request(&self, req: CollectSnapshotRequest) {
        use crate::reporting::worker::{AgentSnapshot, InstanceSnapshot};

        let instances = self.supervisor.list_instances();
        let snapshot = AgentSnapshot {
            instances: instances
                .iter()
                .map(|inst| {
                    // Parse instance index from ID (e.g. "web-0" → 0)
                    let instance_id = inst
                        .id
                        .0
                        .rsplit_once('-')
                        .and_then(|(_, n)| n.parse::<u32>().ok())
                        .unwrap_or(0);

                    // Requested resources from the deployed spec: these
                    // are the commitments the scheduler must respect.
                    let spec = self
                        .deployed_specs
                        .get(&(inst.app_name.clone(), inst.namespace.clone()));
                    let cpu_request_millicores = spec
                        .and_then(|s| s.cpu.as_ref())
                        .map(|r| r.request as u32)
                        .unwrap_or(0);
                    let memory_request_mb = spec
                        .and_then(|s| s.memory.as_ref())
                        .map(|r| (r.request / (1024 * 1024)) as u32)
                        .unwrap_or(0);

                    InstanceSnapshot {
                        app_name: inst.app_name.clone(),
                        namespace: inst.namespace.clone(),
                        instance_id,
                        image: inst.image.clone(),
                        port: inst.host_port,
                        container_state: inst.state,
                        consecutive_unhealthy: inst.health_counters.consecutive_unhealthy,
                        uptime: inst.created_at.elapsed(),
                        cpu_request_millicores,
                        memory_request_mb,
                    }
                })
                .collect(),
            allocated_ports: instances.iter().filter_map(|i| i.host_port).collect(),
            capacity_cpu_millicores: self.capacity_cpu_millicores,
            capacity_memory_mb: self.capacity_memory_mb,
        };
        let _ = req.response.send(snapshot);
    }

    /// Get cluster node membership from gossip, or empty if single-node.
    fn get_cluster_nodes(&self) -> Vec<NodeStatus> {
        let Some(handle) = &self.cluster else {
            return Vec::new();
        };

        // Cross-reference the Raft council so the COUNCIL / LEADER columns
        // reflect actual consensus state. The gossip-level `is_council` /
        // `is_leader` flags on the membership snapshot are never set by this
        // runtime — council membership and leadership live in the Raft metrics.
        // A node is a council member if it's a current voter, and the leader if
        // it's the current Raft leader.
        let mut council_names: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut leader_name: Option<String> = None;
        if let Some(metrics_rx) = &handle.raft_metrics_rx {
            let metrics = metrics_rx.borrow();
            let membership = metrics.membership_config.membership();
            council_names = membership
                .voter_ids()
                .filter_map(|id| membership.get_node(&id).map(|n| n.name.clone()))
                .collect();
            leader_name = metrics
                .current_leader
                .and_then(|id| membership.get_node(&id).map(|n| n.name.clone()));
        }

        let have_metrics = handle.raft_metrics_rx.is_some();
        let membership = handle.membership_rx.borrow();
        membership
            .iter()
            .map(|m| {
                let name = m.node_id.to_string();
                // Raft metrics are authoritative when the council is wired;
                // otherwise fall back to whatever the gossip snapshot reports.
                let (is_council, is_leader) = if have_metrics {
                    (
                        council_names.contains(&name),
                        leader_name.as_deref() == Some(name.as_str()),
                    )
                } else {
                    (m.is_council, m.is_leader)
                };
                NodeStatus {
                    node_id: name,
                    address: m.address.to_string(),
                    state: m.state.to_string(),
                    incarnation: m.incarnation,
                    is_council,
                    is_leader,
                    labels: m.labels.clone(),
                }
            })
            .collect()
    }

    /// Get Raft council status, or default if single-node/non-council.
    async fn get_council_status(&self) -> CouncilStatus {
        let Some(handle) = &self.cluster else {
            return CouncilStatus::default();
        };
        let Some(council) = &handle.council else {
            return CouncilStatus::default();
        };
        let Some(metrics_rx) = &handle.raft_metrics_rx else {
            return CouncilStatus::default();
        };

        let metrics = metrics_rx.borrow().clone();
        let desired = council.desired_state().await;

        let leader_name = metrics.current_leader.and_then(|leader_id| {
            metrics
                .membership_config
                .membership()
                .get_joint_config()
                .iter()
                .flat_map(|ids| ids.iter())
                .find(|&&id| id == leader_id)
                .and_then(|_| {
                    metrics
                        .membership_config
                        .membership()
                        .get_node(&leader_id)
                        .map(|info| info.name.clone())
                })
        });

        let members = metrics
            .membership_config
            .membership()
            .nodes()
            .map(|(id, info)| CouncilMemberInfo {
                raft_id: *id,
                name: info.name.clone(),
                address: info.addr.to_string(),
            })
            .collect();

        CouncilStatus {
            members,
            leader: leader_name,
            term: metrics.current_term,
            last_applied_log: metrics.last_applied.map(|l| l.index),
            app_count: desired.apps.len(),
        }
    }

    /// Handle a single command.
    async fn handle_command(&mut self, cmd: AgentCommand) {
        match cmd {
            AgentCommand::Deploy { config, events } => {
                if self.draining.load(std::sync::atomic::Ordering::Relaxed) {
                    let _ = events
                        .send(ApplyEvent::Error {
                            message: "node is draining for a binary upgrade; retry shortly"
                                .to_string(),
                        })
                        .await;
                    return;
                }
                self.deploy(config, &events).await;
            }
            AgentCommand::Stop {
                app_name,
                namespace,
                response,
            } => {
                let result = self.stop_app(&app_name, &namespace).await;
                let _ = response.send(result);
            }
            AgentCommand::Status { response } => {
                let statuses = self.get_status().await;
                let _ = response.send(statuses);
            }
            AgentCommand::ActiveImages { response } => {
                let images: std::collections::HashSet<String> = self
                    .supervisor
                    .list_instances()
                    .iter()
                    .map(|i| i.image.clone())
                    .filter(|image| !image.is_empty())
                    .collect();
                let _ = response.send(images);
            }
            AgentCommand::Logs {
                app_name,
                namespace,
                tail,
                response,
            } => {
                let result = self.get_logs(&app_name, &namespace).await;
                let result = result.map(|logs| match tail {
                    Some(n) => tail_lines(&logs, n),
                    None => logs,
                });
                let _ = response.send(result);
            }
            AgentCommand::FollowLogs {
                app_name,
                namespace,
                tail,
                lines,
            } => {
                self.follow_app_logs(&app_name, &namespace, tail, lines)
                    .await;
            }
            AgentCommand::Exec {
                app_name,
                namespace,
                command,
                response,
            } => {
                let result = self.exec_app(&app_name, &namespace, &command).await;
                let _ = response.send(result);
            }
            AgentCommand::Nodes { response } => {
                let nodes = self.get_cluster_nodes();
                let _ = response.send(nodes);
            }
            AgentCommand::Council { response } => {
                let status = self.get_council_status().await;
                let _ = response.send(status);
            }
            AgentCommand::Join {
                token,
                addr,
                response,
            } => {
                let result = self.handle_join(&token, &addr).await;
                let _ = response.send(result);
            }
            AgentCommand::InjectPartition {
                peers,
                duration_secs,
                response,
            } => {
                // Legacy chaos API — create a partition fault in the registry
                let request = crate::smoker::types::FaultRequest {
                    fault_type: crate::smoker::types::FaultType::Partition {
                        source_app: None,
                        source_cgroup_id: 0,
                    },
                    target_service: peers.join(","),
                    target_instance: None,
                    target_node: None,
                    duration: std::time::Duration::from_secs(duration_secs),
                    injected_by: "relish chaos".into(),
                    reason: Some("legacy chaos partition".into()),
                    include_leader: false,
                    override_safety: false,
                };
                self.fault_registry.insert(&request);
                // L15: actually partition. Resolve each peer (by name)
                // to its gossip address from membership, then block both
                // the gossip and Raft transports to it — the old code
                // only recorded a registry entry and dropped nothing.
                let blocked = self.apply_partition(&peers).await;
                let msg =
                    format!("partition injected: blocking {blocked} peer(s) for {duration_secs}s");
                let _ = response.send(Ok(msg));
            }
            AgentCommand::HealPartition { response } => {
                // Legacy chaos API — clear all faults and blocklists.
                let removed = self.fault_registry.clear();
                self.clear_partition().await;
                let msg = if removed.is_empty() {
                    "partition healed".to_string()
                } else {
                    format!("cleared {} fault(s); partition healed", removed.len())
                };
                let _ = response.send(Ok(msg));
            }
            AgentCommand::ChaosStatus { response } => {
                let state = self.get_chaos_state();
                let _ = response.send(state);
            }
            AgentCommand::InjectFault { request, response } => {
                // Safety rails first (L14): reject faults that risk
                // quorum, kill a service's last replica, target the
                // leader, or exceed the node-percentage cap — unless
                // explicitly overridden.
                if let Some(context) = self.build_safety_context(&request).await {
                    let check = crate::smoker::safety::evaluate_safety(&request, &context);
                    if !check.approved {
                        let reason = check
                            .violation
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "safety check failed".into());
                        let _ = response.send(Err(BunError::FaultRejected { reason }));
                        return;
                    }
                }

                // Actually apply the fault. Only record it in the
                // registry if injection succeeded — a fault that can't
                // be applied must not report success (the old code
                // recorded everything, injecting nothing).
                let rule = self.fault_registry.insert(&request);
                match self.apply_fault(&rule).await {
                    Ok(()) => {
                        let summary = crate::smoker::types::FaultSummary::from(&rule);
                        let _ = response.send(Ok(summary));
                    }
                    Err(reason) => {
                        self.fault_registry.remove(rule.id);
                        let _ = response.send(Err(BunError::FaultRejected { reason }));
                    }
                }
            }
            AgentCommand::ClearFault { fault_id, response } => {
                let msg = match self
                    .fault_registry
                    .remove(crate::smoker::types::FaultId(fault_id))
                {
                    Some(rule) => {
                        self.delete_fault_bpf_entry(&rule).await;
                        format!("cleared fault {} ({})", rule.id, rule.fault_type)
                    }
                    None => format!("fault {fault_id} not found"),
                };
                let _ = response.send(Ok(msg));
            }
            AgentCommand::ClearAllFaults { response } => {
                let removed = self.fault_registry.clear();
                for rule in &removed {
                    self.delete_fault_bpf_entry(rule).await;
                }
                let msg = format!("cleared {} fault(s)", removed.len());
                let _ = response.send(Ok(msg));
            }
            AgentCommand::ListFaults { response } => {
                let summaries = self.fault_registry.list();
                let _ = response.send(summaries);
            }
            AgentCommand::Resolve { app_name, response } => {
                let result = self
                    .service_map
                    .resolve(&app_name)
                    .map(|e| e.to_resolve_response());
                let _ = response.send(result);
            }
            AgentCommand::ResolveAll { response } => {
                let results = self
                    .service_map
                    .resolve_all()
                    .iter()
                    .map(|e| e.to_resolve_response())
                    .collect();
                let _ = response.send(results);
            }
            AgentCommand::Routes { response } => {
                let table = self.routing_table.read().await;
                let _ = response.send(table.list_routes());
            }
            AgentCommand::SignImage {
                manifest_digest,
                response,
            } => {
                let result = self.handle_sign_image(&manifest_digest).await;
                let _ = response.send(result);
            }
            AgentCommand::AppConfig {
                app_name,
                namespace,
                response,
            } => {
                let spec = self.deployed_specs.get(&(app_name, namespace)).cloned();
                let _ = response.send(spec);
            }
            AgentCommand::UpgradeApply {
                directive,
                response,
            } => {
                self.handle_upgrade_apply(directive, response).await;
            }
            AgentCommand::UpgradeStatus { response } => {
                let result = match &self.upgrade {
                    Some(manager) => Ok(manager.status()),
                    None => Err(BunError::UpgradesUnavailable),
                };
                let _ = response.send(result);
            }
            AgentCommand::UpgradeRollback { version, response } => {
                self.handle_upgrade_rollback(version, response).await;
            }
            AgentCommand::UpgradeVerify { marker, response } => {
                self.handle_upgrade_verify(marker, response).await;
            }
        }
    }

    /// Node-level upgrade: verify + stage, respond, then exec. On any
    /// failure the node keeps running the current version, undrained.
    async fn handle_upgrade_apply(
        &mut self,
        directive: crate::upgrade::types::UpgradeDirective,
        response: oneshot::Sender<Result<(), BunError>>,
    ) {
        let Some(manager) = self.upgrade.clone() else {
            let _ = response.send(Err(BunError::UpgradesUnavailable));
            return;
        };

        // Stop taking new work while the swap is in progress. Running
        // workloads are untouched (and survive the exec — see grill).
        self.draining
            .store(true, std::sync::atomic::Ordering::Relaxed);

        let inventory = self.upgrade_inventory().await;
        let prepared = match manager.prepare(&directive, inventory).await {
            Ok(Some(prepared)) => prepared,
            Ok(None) => {
                // Same upgrade already in flight: idempotent OK.
                let _ = response.send(Ok(()));
                return;
            }
            Err(e) => {
                self.draining
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                let _ = response.send(Err(BunError::Upgrade(e)));
                return;
            }
        };

        println!(
            "bun: upgrading to {} (upgrade {})",
            prepared.target_version(),
            directive.upgrade_id
        );
        // Respond before the point of no return, and give the HTTP layer a
        // moment to flush the response — exec closes every socket.
        let _ = response.send(Ok(()));
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Only returns on failure (the symlink is already reverted then).
        let error = manager.execute(prepared);
        self.draining
            .store(false, std::sync::atomic::Ordering::Relaxed);
        eprintln!(
            "bun: upgrade exec failed, still on {}: {error}",
            manager.running_version()
        );
    }

    /// Node-level rollback: same swap machinery, no download or re-verify.
    async fn handle_upgrade_rollback(
        &mut self,
        version: Option<crate::upgrade::BinaryVersion>,
        response: oneshot::Sender<Result<(), BunError>>,
    ) {
        let Some(manager) = self.upgrade.clone() else {
            let _ = response.send(Err(BunError::UpgradesUnavailable));
            return;
        };

        self.draining
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let inventory = self.upgrade_inventory().await;
        let prepared = match manager.prepare_rollback(version, inventory) {
            Ok(prepared) => prepared,
            Err(e) => {
                self.draining
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                let _ = response.send(Err(BunError::Upgrade(e)));
                return;
            }
        };

        println!("bun: rolling back to {}", prepared.target_version());
        let _ = response.send(Ok(()));
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let error = manager.execute(prepared);
        self.draining
            .store(false, std::sync::atomic::Ordering::Relaxed);
        eprintln!(
            "bun: rollback exec failed, still on {}: {error}",
            manager.running_version()
        );
    }

    /// Post-boot verification of a freshly swapped-in version: all
    /// pre-upgrade workloads must have been adopted and still be Running.
    /// Commit on success; flag revert and exit on failure (the supervisor
    /// restarts us, and startup recovery swaps the old binary back).
    async fn handle_upgrade_verify(
        &mut self,
        marker: crate::upgrade::marker::UpgradeMarker,
        response: oneshot::Sender<Result<bool, BunError>>,
    ) {
        let Some(manager) = self.upgrade.clone() else {
            let _ = response.send(Err(BunError::UpgradesUnavailable));
            return;
        };

        // In cluster mode, workload placement is the cluster's decision:
        // the scheduler may legitimately move an app off this node while it
        // bounces, so a missing pre-upgrade instance is NOT an upgrade
        // failure. We surviving the boot-grace period (this command runs on
        // the new binary) is the liveness proof; genuine boot failures are
        // caught by the crash-loop budget, which reverts before we ever get
        // here. Single-node keeps the strict check as a local safety net —
        // there is no cluster to reschedule, so a vanished workload really
        // is a failed swap.
        if self.cluster.is_some() {
            if let Err(reason) = self.verify_upgrade_inventory(&marker).await {
                eprintln!("bun: note: {reason} — not reverting (cluster reschedules placements)");
            }
            match manager.commit(&marker) {
                Ok(()) => {
                    println!(
                        "bun: upgrade to {} verified and committed",
                        marker.target_version
                    );
                    let _ = response.send(Ok(true));
                }
                Err(e) => {
                    let _ = response.send(Err(BunError::Upgrade(e)));
                }
            }
            return;
        }

        match self.verify_upgrade_inventory(&marker).await {
            Ok(()) => match manager.commit(&marker) {
                Ok(()) => {
                    println!(
                        "bun: upgrade to {} verified and committed",
                        marker.target_version
                    );
                    let _ = response.send(Ok(true));
                }
                Err(e) => {
                    let _ = response.send(Err(BunError::Upgrade(e)));
                }
            },
            Err(reason) => {
                let _ = manager.mark_revert_pending(&marker, &reason);
                let _ = response.send(Ok(false));
                eprintln!("bun: exiting so the supervisor can restart into the revert");
                std::process::exit(1);
            }
        }
    }

    /// Populate the gossip + Raft blocklists to partition this node
    /// from the named peers. Returns how many addresses were blocked.
    ///
    /// A peer is identified by gossip node name; its gossip address
    /// comes from membership and its Raft address is derived by the
    /// fixed port offset. Both must be blocked, or SWIM keeps half the
    /// path alive and the partition doesn't take.
    async fn apply_partition(&self, peers: &[String]) -> usize {
        let Some(handle) = &self.cluster else {
            return 0;
        };
        let blocklists = &handle.partition_blocklists;

        // Resolve peer names → gossip SocketAddrs.
        let targets: Vec<std::net::SocketAddr> = {
            let membership = handle.membership_rx.borrow();
            peers
                .iter()
                .filter_map(|name| {
                    membership
                        .iter()
                        .find(|m| &m.node_id.0 == name)
                        .map(|m| m.address)
                })
                .collect()
        };

        let mut blocked = 0;
        if let Some(gossip) = &blocklists.gossip {
            let mut set = gossip.write().await;
            for addr in &targets {
                if set.insert(*addr) {
                    blocked += 1;
                }
            }
        }
        if let Some(raft) = &blocklists.raft {
            let mut set = raft.write().await;
            for addr in &targets {
                let raft_addr = std::net::SocketAddr::new(
                    addr.ip(),
                    (addr.port() as i32 + blocklists.raft_port_offset) as u16,
                );
                set.insert(raft_addr);
            }
        }
        blocked
    }

    /// Clear both transport blocklists (heal all partitions).
    async fn clear_partition(&self) {
        let Some(handle) = &self.cluster else {
            return;
        };
        if let Some(gossip) = &handle.partition_blocklists.gossip {
            gossip.write().await.clear();
        }
        if let Some(raft) = &handle.partition_blocklists.raft {
            raft.write().await.clear();
        }
    }

    /// Build the safety context for a fault request from live cluster
    /// state, or `None` when there's no council (single-node mode has
    /// nothing to protect quorum-wise; replica/leader rails don't apply).
    async fn build_safety_context(
        &self,
        request: &crate::smoker::types::FaultRequest,
    ) -> Option<crate::smoker::types::SafetyContext> {
        let handle = self.cluster.as_ref()?;
        let metrics = handle.raft_metrics_rx.as_ref()?.borrow().clone();
        let council_size = metrics.membership_config.membership().voter_ids().count() as u32;
        let leader_node_id = metrics
            .current_leader
            .and_then(|id| {
                metrics
                    .membership_config
                    .membership()
                    .get_node(&id)
                    .map(|info| info.name.clone())
            })
            .unwrap_or_default();
        let total_nodes = handle
            .membership_rx
            .borrow()
            .iter()
            .filter(|m| m.state == crate::mustard::state::NodeState::Alive)
            .count()
            .max(1) as u32;

        // Replicas of the target service running locally (an
        // approximation — the leader has the cluster-wide count, but
        // this node protects at least its own replicas).
        let target_service_replicas = self
            .supervisor
            .list_instances()
            .iter()
            .filter(|i| i.app_name == request.target_service)
            .count() as u32;

        // Node-level faults already active. We count NodeKill/NodeDrain/
        // Partition and, conservatively, treat each as if it could touch a
        // council member — protecting quorum against the worst case rather
        // than assuming the best.
        let active_node_faults = self
            .fault_registry
            .iter()
            .filter(|f| {
                matches!(
                    f.fault_type,
                    crate::smoker::types::FaultType::NodeKill { .. }
                        | crate::smoker::types::FaultType::NodeDrain
                        | crate::smoker::types::FaultType::Partition { .. }
                )
            })
            .count() as u32;

        let target_service_faulted_replicas =
            self.fault_registry
                .count_by_service(&request.target_service) as u32;

        Some(crate::smoker::types::SafetyContext {
            council_size,
            council_nodes_with_active_faults: active_node_faults,
            leader_node_id,
            total_nodes,
            nodes_with_active_faults: active_node_faults,
            target_service_replicas,
            target_service_faulted_replicas,
        })
    }

    /// Apply a fault for real (L14). Process faults (kill/pause/resume)
    /// and CPU stress work on every platform; network faults need eBPF
    /// and are rejected honestly when it isn't loaded, rather than
    /// recorded as active while injecting nothing.
    async fn apply_fault(&mut self, rule: &crate::smoker::types::FaultRule) -> Result<(), String> {
        use crate::smoker::types::FaultType;

        match &rule.fault_type {
            FaultType::Kill { count } => {
                let pids = self.target_pids(rule, *count).await;
                if pids.is_empty() {
                    return Err(format!("no running instances of {}", rule.target_service));
                }
                for pid in pids {
                    if let Err(e) = crate::smoker::process::kill_process(pid as i32) {
                        eprintln!("smoker: kill {pid} failed: {e}");
                    }
                }
                Ok(())
            }
            FaultType::Pause => {
                let pids = self.target_pids(rule, 0).await;
                if pids.is_empty() {
                    return Err(format!("no running instances of {}", rule.target_service));
                }
                for pid in pids {
                    if let Err(e) = crate::smoker::process::pause_process(pid as i32) {
                        eprintln!("smoker: pause {pid} failed: {e}");
                    }
                }
                Ok(())
            }
            FaultType::Resume => {
                let pids = self.target_pids(rule, 0).await;
                for pid in pids {
                    if let Err(e) = crate::smoker::process::resume_process(pid as i32) {
                        eprintln!("smoker: resume {pid} failed: {e}");
                    }
                }
                Ok(())
            }
            FaultType::CpuStress { percentage, cores } => {
                // Burn CPU in spawned blocking tasks for the fault's
                // lifetime. Bounded by the rule's remaining duration.
                let config = crate::smoker::resource::CpuBurnConfig::new(*percentage, *cores);
                let duration = rule.remaining().max(std::time::Duration::from_millis(1));
                let core_count = cores.unwrap_or(1).max(1);
                for _ in 0..core_count {
                    let burn_us = config.burn_duration_us();
                    let sleep_us = config.sleep_duration_us();
                    tokio::task::spawn_blocking(move || {
                        let deadline = std::time::Instant::now() + duration;
                        while std::time::Instant::now() < deadline {
                            let spin_until = std::time::Instant::now()
                                + std::time::Duration::from_micros(burn_us);
                            while std::time::Instant::now() < spin_until {
                                std::hint::spin_loop();
                            }
                            std::thread::sleep(std::time::Duration::from_micros(sleep_us));
                        }
                    });
                }
                Ok(())
            }
            FaultType::Delay { .. }
            | FaultType::Drop { .. }
            | FaultType::DnsNxdomain
            | FaultType::Bandwidth { .. } => {
                // Packet-level network faults genuinely need the eBPF
                // data path. Apply via BPF maps when it's loaded;
                // otherwise reject honestly rather than record fake
                // success (the old stub recorded everything).
                #[cfg(feature = "ebpf")]
                {
                    if self.onion_ebpf.is_some() {
                        self.write_fault_bpf_entry(rule).await;
                        return Ok(());
                    }
                }
                Err(format!(
                    "{} requires the eBPF data path, which is not loaded on this node",
                    rule.fault_type
                ))
            }
            FaultType::MemoryPressure { .. } | FaultType::DiskIoThrottle { .. } => {
                // cgroup-based; Linux only. Honest error elsewhere.
                #[cfg(target_os = "linux")]
                {
                    Ok(()) // TODO(Phase 10): apply cgroup limits
                }
                #[cfg(not(target_os = "linux"))]
                {
                    Err(format!("{} requires Linux cgroups", rule.fault_type))
                }
            }
            FaultType::NodeDrain | FaultType::NodeKill { .. } => {
                // Node-level faults are orchestrated by the chaos
                // controller, not a per-instance action here.
                Ok(())
            }
            FaultType::Partition { .. } => {
                // Handled by InjectPartition via transport blocklists.
                Ok(())
            }
        }
    }

    /// PIDs of running instances matching a fault's target (service, or
    /// a specific instance). `count` limits how many (0 = all).
    async fn target_pids(&self, rule: &crate::smoker::types::FaultRule, count: u32) -> Vec<u32> {
        let ids: Vec<InstanceId> = self
            .supervisor
            .list_instances()
            .iter()
            .filter(|i| {
                i.app_name == rule.target_service
                    && rule.target_instance.as_ref().is_none_or(|t| &i.id.0 == t)
            })
            .map(|i| i.id.clone())
            .collect();

        let mut pids = Vec::new();
        for id in ids {
            if let Some(pid) = self.supervisor.grill().pid(&id).await {
                pids.push(pid);
                if count > 0 && pids.len() as u32 >= count {
                    break;
                }
            }
        }
        pids
    }

    /// Build the current chaos state for the API (legacy format).
    fn get_chaos_state(&self) -> ChaosState {
        // Find the first partition-type fault for backward compatibility
        let active_partition = self
            .fault_registry
            .iter()
            .find(|f| {
                matches!(
                    f.fault_type,
                    crate::smoker::types::FaultType::Partition { .. }
                )
            })
            .map(|f| {
                let remaining = f.remaining();
                let epoch = SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                PartitionInfo {
                    peers: vec![f.target_service.clone()],
                    injected_at_epoch: epoch.saturating_sub(
                        (f.duration_ns / 1_000_000_000).saturating_sub(remaining.as_secs()),
                    ),
                    duration_secs: f.duration_ns / 1_000_000_000,
                    remaining_secs: remaining.as_secs(),
                }
            });
        ChaosState { active_partition }
    }

    /// Drain expired faults from the registry. Called on every health tick.
    ///
    /// When a fault expires, its BPF map entry must be deleted so the
    /// kernel stops applying it. The eBPF programs also check expiry
    /// independently (defense in depth), but userspace cleanup frees
    /// map slots and kills resource fault helper processes.
    async fn expire_faults(&mut self) {
        let now = crate::smoker::types::monotonic_now_ns();
        let expired = self.fault_registry.drain_expired(now);
        for rule in &expired {
            if !rule.target_service.is_empty() {
                eprintln!(
                    "smoker: fault {} expired ({}), cleaning up",
                    rule.id, rule.fault_type
                );
            }
            self.delete_fault_bpf_entry(rule).await;
        }
    }

    /// The network-byte-order VIP + port for a fault's target service, if
    /// it is registered. VIP is deterministic from the app name; the port
    /// comes from the service entry. Connect/bandwidth fault keys need both.
    #[cfg(feature = "ebpf")]
    fn fault_vip_port(&self, target_service: &str) -> Option<(u32, u16)> {
        self.service_map
            .resolve(target_service)
            .map(|e| (e.vip.to_network_byte_order(), e.port.to_be()))
    }

    /// Write the eBPF map entry for a newly injected network fault (P2).
    ///
    /// Only reachable with the `ebpf` feature: without it, `apply_fault`
    /// rejects network faults before we get here. `expires_ns` comes from
    /// the rule, which now uses CLOCK_MONOTONIC (P0) to match the kernel's
    /// `bpf_ktime_get_ns()`.
    #[cfg(feature = "ebpf")]
    async fn write_fault_bpf_entry(&self, rule: &crate::smoker::types::FaultRule) {
        use crate::smoker::bpf_maps;
        use crate::smoker::bpf_types::*;
        use crate::smoker::types::FaultType;

        let Some(handle) = self.onion_ebpf.as_ref() else {
            return;
        };
        let vip_port = self.fault_vip_port(&rule.target_service);
        let expires = rule.expires_at_ns;
        let mut ebpf = handle.lock().await;

        // Connect/bandwidth faults need the target VIP; DNS is keyed by name.
        let require_vip = || match vip_port {
            Some(vp) => Some(vp),
            None => {
                eprintln!(
                    "smoker: no VIP for {} — {} fault not applied",
                    rule.target_service, rule.fault_type
                );
                None
            }
        };

        match &rule.fault_type {
            FaultType::Drop { probability } => {
                let Some((vip, port)) = require_vip() else {
                    return;
                };
                let value = BpfConnectFaultValue {
                    action: FAULT_ACTION_DROP,
                    probability: *probability,
                    _pad: [0; 6],
                    delay_ns: 0,
                    jitter_ns: 0,
                    expires_ns: expires,
                };
                let _ = bpf_maps::write_connect_fault(
                    &mut ebpf.bpf,
                    connect_fault_key(vip, port),
                    value,
                );
            }
            FaultType::Delay {
                delay_ns,
                jitter_ns,
            } => {
                let Some((vip, port)) = require_vip() else {
                    return;
                };
                let value = BpfConnectFaultValue {
                    action: FAULT_ACTION_DELAY,
                    probability: 100,
                    _pad: [0; 6],
                    delay_ns: *delay_ns,
                    jitter_ns: *jitter_ns,
                    expires_ns: expires,
                };
                let _ = bpf_maps::write_connect_fault(
                    &mut ebpf.bpf,
                    connect_fault_key(vip, port),
                    value,
                );
            }
            FaultType::Partition {
                source_cgroup_id, ..
            } => {
                let Some((vip, port)) = require_vip() else {
                    return;
                };
                let value = BpfConnectFaultValue {
                    action: FAULT_ACTION_PARTITION,
                    probability: 100,
                    _pad: [0; 6],
                    delay_ns: 0,
                    jitter_ns: 0,
                    expires_ns: expires,
                };
                let _ = bpf_maps::write_connect_fault(
                    &mut ebpf.bpf,
                    partition_fault_key(vip, port, *source_cgroup_id),
                    value,
                );
            }
            FaultType::DnsNxdomain => {
                let value = BpfDnsFaultValue {
                    action: DNS_FAULT_NXDOMAIN,
                    probability: 100,
                    _pad: [0; 6],
                    delay_ns: 0,
                    expires_ns: expires,
                };
                let _ = bpf_maps::write_dns_fault(
                    &mut ebpf.bpf,
                    dns_fault_key(&rule.target_service),
                    value,
                );
            }
            FaultType::Bandwidth { bytes_per_sec } => {
                let Some((vip, port)) = require_vip() else {
                    return;
                };
                let value = BpfBandwidthFaultValue {
                    rate_bytes_per_sec: *bytes_per_sec,
                    tokens: *bytes_per_sec, // start with a full bucket
                    last_refill_ns: crate::smoker::types::monotonic_now_ns(),
                    expires_ns: expires,
                };
                let _ =
                    bpf_maps::write_bw_fault(&mut ebpf.bpf, bandwidth_fault_key(vip, port), value);
            }
            // Non-network faults never reach here (apply_fault handles them).
            _ => {}
        }
    }

    /// Delete the eBPF map entry for a cleared or expired fault (P2).
    ///
    /// Best-effort: VIP is deterministic from the app name, but the port
    /// comes from the service entry — if the service is already gone we
    /// skip, since the kernel ignores the entry past its `expires_ns`.
    #[cfg(feature = "ebpf")]
    async fn delete_fault_bpf_entry(&self, rule: &crate::smoker::types::FaultRule) {
        use crate::smoker::bpf_maps;
        use crate::smoker::bpf_types::*;
        use crate::smoker::types::FaultType;

        if !rule.fault_type.requires_ebpf() {
            return;
        }
        let Some(handle) = self.onion_ebpf.as_ref() else {
            return;
        };
        let vip_port = self.fault_vip_port(&rule.target_service);
        let mut ebpf = handle.lock().await;

        match &rule.fault_type {
            FaultType::Drop { .. } | FaultType::Delay { .. } => {
                if let Some((vip, port)) = vip_port {
                    let _ = bpf_maps::delete_connect_fault(
                        &mut ebpf.bpf,
                        &connect_fault_key(vip, port),
                    );
                }
            }
            FaultType::Partition {
                source_cgroup_id, ..
            } => {
                if let Some((vip, port)) = vip_port {
                    let _ = bpf_maps::delete_connect_fault(
                        &mut ebpf.bpf,
                        &partition_fault_key(vip, port, *source_cgroup_id),
                    );
                }
            }
            FaultType::DnsNxdomain => {
                let _ =
                    bpf_maps::delete_dns_fault(&mut ebpf.bpf, &dns_fault_key(&rule.target_service));
            }
            FaultType::Bandwidth { .. } => {
                if let Some((vip, port)) = vip_port {
                    let _ =
                        bpf_maps::delete_bw_fault(&mut ebpf.bpf, &bandwidth_fault_key(vip, port));
                }
            }
            _ => {}
        }
    }

    /// Delete is a no-op without the eBPF data path (nothing was written).
    #[cfg(not(feature = "ebpf"))]
    async fn delete_fault_bpf_entry(&self, _rule: &crate::smoker::types::FaultRule) {}

    /// Deploy all apps and jobs from a config, streaming progress events.
    async fn deploy(&mut self, config: Config, events: &mpsc::Sender<ApplyEvent>) {
        let now = Instant::now();
        let mut all_ids = Vec::new();

        for (app_name, spec) in &config.app {
            let namespace = spec.namespace.as_deref().unwrap_or("default");

            // Gate on image signature before doing anything: an unsigned or
            // invalidly-signed Pickle image is refused, but other apps in the
            // same config still deploy.
            if let Err(reason) = self.enforce_image_signature(spec).await {
                let _ = events.send(ApplyEvent::Error { message: reason }).await;
                continue;
            }

            // Store the spec so the Brioche UI can display env vars safely.
            self.deployed_specs
                .insert((app_name.clone(), namespace.to_string()), spec.clone());

            // Check if this app already has running instances → rolling deploy
            let existing: Vec<_> = self
                .supervisor
                .list_instances()
                .iter()
                .filter(|i| i.app_name == *app_name && i.namespace == namespace)
                .filter(|i| {
                    !matches!(
                        i.state,
                        crate::grill::state::ContainerState::Stopped
                            | crate::grill::state::ContainerState::Failed
                    )
                })
                .map(|i| i.id.clone())
                .collect();

            if !existing.is_empty() {
                // Rolling redeploy: start new instances first, health check,
                // then kill old ones. If new instances fail, keep the old ones.
                let _ = events
                    .send(ApplyEvent::Progress {
                        message: format!(
                            "rolling redeploy {app_name} ({} existing instance(s))",
                            existing.len()
                        ),
                    })
                    .await;

                // Read deploy config from app spec
                let deploy_config = spec
                    .deploy
                    .as_ref()
                    .map(crate::meat::deploy_types::DeployConfig::from_spec)
                    .unwrap_or_default();
                let health_wait = deploy_config.health_timeout;

                // Generate new instance IDs that don't collide with existing
                // ones. A monotonic counter, not wall-clock seconds: two
                // redeploys in the same second used to produce identical IDs
                // and the second create failed "instance already exists".
                let deploy_gen = self.next_deploy_gen;
                self.next_deploy_gen += 1;
                let replica_count = match spec.replicas {
                    crate::config::types::Replicas::Fixed(n) => n,
                    crate::config::types::Replicas::DaemonSet => 1,
                };

                // Start new instances with generation-tagged IDs
                let mut new_ids = Vec::new();
                // Track the host port allocated for each new instance so it can
                // be recorded on the instance and registered as a backend.
                let mut new_ports: std::collections::HashMap<InstanceId, Option<u16>> =
                    std::collections::HashMap::new();
                let mut new_failed = false;
                for i in 0..replica_count {
                    let new_id = crate::grill::InstanceId(format!("{app_name}-g{deploy_gen}-{i}"));
                    let _ = events
                        .send(ApplyEvent::Progress {
                            message: format!("starting new instance {}", new_id.0),
                        })
                        .await;

                    // Create and start the new instance via the Grill directly
                    let host_port = if spec.port.is_some() {
                        match self.supervisor.port_allocator.allocate().await {
                            Ok(p) => Some(p),
                            Err(e) => {
                                let _ = events
                                    .send(ApplyEvent::Error {
                                        message: format!("port allocation failed: {e}"),
                                    })
                                    .await;
                                new_failed = true;
                                break;
                            }
                        }
                    } else {
                        None
                    };

                    // Fail closed on encrypted secrets we can't decrypt (see
                    // drive_instance_startup).
                    let identity = self.decrypt_identity(namespace).await;
                    if identity.is_none() && spec.env.values().any(|v| v.is_encrypted()) {
                        let _ = events
                            .send(ApplyEvent::Error {
                                message: format!(
                                    "cannot start {}: encrypted secrets require cluster security state",
                                    new_id.0
                                ),
                            })
                            .await;
                        new_failed = true;
                        break;
                    }
                    let oci_spec = Self::oci_spec_with_secrets(
                        app_name,
                        namespace,
                        spec,
                        host_port,
                        &crate::grill::cgroup::cgroup_path(namespace, app_name, i)
                            .to_string_lossy(),
                        None,
                        None,
                        identity,
                    );

                    if let Err(e) = self.supervisor.grill.create(&new_id, &oci_spec).await {
                        let _ = events
                            .send(ApplyEvent::Error {
                                message: format!("failed to create {}: {e}", new_id.0),
                            })
                            .await;
                        new_failed = true;
                        break;
                    }
                    if let Err(e) = self.supervisor.grill.start(&new_id).await {
                        let _ = events
                            .send(ApplyEvent::Error {
                                message: format!("failed to start {}: {e}", new_id.0),
                            })
                            .await;
                        new_failed = true;
                        break;
                    }
                    self.spawn_log_forwarder(&new_id, app_name, namespace);
                    self.persist_instance_record(&new_id).await;

                    // Health check: poll until the process is alive, returning
                    // as soon as it's Running instead of always sleeping the
                    // full window. Bounded by health_wait (capped at 5s).
                    // TODO(Stage 2+): move rolling deploys off the event loop
                    // entirely so even this bounded wait doesn't block the loop.
                    let wait = health_wait.min(std::time::Duration::from_secs(5));
                    let deadline = std::time::Instant::now() + wait;
                    let mut probe = self.supervisor.grill.state(&new_id).await;
                    while std::time::Instant::now() < deadline
                        && !matches!(probe, Ok(crate::grill::state::ContainerState::Running))
                    {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        probe = self.supervisor.grill.state(&new_id).await;
                    }
                    match probe {
                        Ok(crate::grill::state::ContainerState::Running) => {
                            let _ = events
                                .send(ApplyEvent::Progress {
                                    message: format!("{} healthy ✓", new_id.0),
                                })
                                .await;

                            // Provision workload identity (SPIFFE cert + OIDC JWT)
                            self.provision_identity(app_name, namespace, &new_id, false, events)
                                .await;
                        }
                        Ok(state) => {
                            let _ = events
                                .send(ApplyEvent::Error {
                                    message: format!(
                                        "{} not healthy (state: {state}), rolling back",
                                        new_id.0
                                    ),
                                })
                                .await;
                            // Kill the failed new instance
                            let _ = self.supervisor.grill.kill(&new_id).await;
                            new_failed = true;
                            break;
                        }
                        Err(_) => {
                            let _ = events
                                .send(ApplyEvent::Error {
                                    message: format!("{} state unknown, rolling back", new_id.0),
                                })
                                .await;
                            let _ = self.supervisor.grill.kill(&new_id).await;
                            new_failed = true;
                            break;
                        }
                    }

                    new_ports.insert(new_id.clone(), host_port);
                    new_ids.push(new_id);
                }

                if new_failed {
                    // Rollback: kill any new instances we started, keep old ones.
                    // These were grill-created but never inserted into supervisor
                    // tracking, so release their ports here directly.
                    for new_id in &new_ids {
                        let _ = self.supervisor.grill.kill(new_id).await;
                        if let Some(port) = new_ports.get(new_id).copied().flatten() {
                            let _ = self.supervisor.port_allocator.release(port).await;
                        }
                    }
                    // Record failed deploy in history
                    let entry = crate::meat::deploy_types::DeployHistoryEntry {
                        id: crate::meat::deploy_types::DeployId(
                            SystemTime::now()
                                .duration_since(SystemTime::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs(),
                        ),
                        app_id: crate::meat::types::AppId::new(app_name, namespace),
                        image: spec.image.clone().unwrap_or_default(),
                        result: crate::meat::deploy_types::DeployResult::RolledBack,
                        created_at: SystemTime::now(),
                        completed_at: SystemTime::now(),
                        steps_completed: 0,
                        steps_total: replica_count as usize,
                        spec: Some(Box::new(spec.clone())),
                    };
                    self.deploy_history.write().await.push(entry);
                    let _ = events
                        .send(ApplyEvent::Error {
                            message: "rolled back — old instances preserved".to_string(),
                        })
                        .await;
                    return;
                }

                // New instances are healthy — kill old ones
                for old_id in &existing {
                    let _ = events
                        .send(ApplyEvent::Progress {
                            message: format!("stopping old instance {}", old_id.0),
                        })
                        .await;
                }
                self.supervisor.remove_app(app_name, namespace).await;
                self.remove_backend_ebpf(app_name).await;
                for old_id in &existing {
                    self.remove_instance_record(old_id);
                }
                let _ = self.service_map.unregister_app(app_name);

                // Register new instances in supervisor tracking
                for new_id in &new_ids {
                    let host_port = new_ports.get(new_id).copied().flatten();
                    // Re-probe the app after a redeploy: build and register its
                    // health config (previously dropped, so a redeployed app was
                    // never health-checked again).
                    let health_config = spec.health.as_ref().zip(spec.port).map(|(hs, port)| {
                        crate::bun::health::HealthCheckConfig::from_spec(hs, port)
                    });
                    if let Some(ref cfg) = health_config {
                        self.supervisor
                            .register_health(new_id.clone(), cfg.clone(), now);
                    }
                    // Add to supervisor's instance tracking
                    self.supervisor.instances.insert(
                        new_id.clone(),
                        super::supervisor::WorkloadInstance {
                            id: new_id.clone(),
                            app_name: app_name.to_string(),
                            namespace: namespace.to_string(),
                            state: crate::grill::state::ContainerState::Running,
                            health_counters: crate::bun::health::HealthCounters::new(),
                            restart_count: 0,
                            last_restart: None,
                            host_port,
                            container_ip: None,
                            created_at: now,
                            restart_policy: crate::bun::restart::RestartPolicy::default(),
                            health_config,
                            is_job: false,
                            image: spec.image.clone().unwrap_or_default(),
                            oci_spec: None,
                            identity: None,
                            identity_mount: None,
                        },
                    );
                    let _ = events
                        .send(ApplyEvent::InstanceCreated {
                            id: new_id.0.clone(),
                            app: app_name.to_string(),
                        })
                        .await;
                }
                let key = (app_name.to_string(), namespace.to_string());
                self.supervisor.app_instances.insert(key, new_ids.clone());

                // Re-register in service map + rebuild routing (same as fresh deploy)
                if let Some(port) = spec.port {
                    let firewall = spec.firewall.as_ref().and_then(|f| {
                        if f.allow_from.is_empty() {
                            None
                        } else {
                            Some(f.allow_from.clone())
                        }
                    });
                    let _ = self
                        .service_map
                        .register_app(app_name, namespace, port, firewall);

                    // Register each new instance as a backend so the service has
                    // endpoints after the redeploy — previously it was left with
                    // zero backends (`relish resolve` empty, ingress 502).
                    for new_id in &new_ids {
                        if let Some(host_port) = new_ports.get(new_id).copied().flatten() {
                            let backend = crate::onion::types::BackendInstance {
                                instance_id: new_id.0.clone(),
                                node_ip: std::net::Ipv4Addr::LOCALHOST,
                                host_port,
                                healthy: true,
                            };
                            let _ = self.service_map.add_backend(app_name, backend);
                        }
                    }
                    self.sync_backend_ebpf(app_name).await;
                }
                if let Some(ref ingress) = spec.ingress {
                    self.ingress_configs
                        .insert(app_name.to_string(), ingress.clone());
                }

                // Record deploy in history
                let entry = crate::meat::deploy_types::DeployHistoryEntry {
                    id: crate::meat::deploy_types::DeployId(
                        SystemTime::now()
                            .duration_since(SystemTime::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs(),
                    ),
                    app_id: crate::meat::types::AppId::new(app_name, namespace),
                    image: spec.image.clone().unwrap_or_default(),
                    result: crate::meat::deploy_types::DeployResult::Completed,
                    created_at: SystemTime::now(),
                    completed_at: SystemTime::now(),
                    steps_completed: new_ids.len(),
                    steps_total: new_ids.len(),
                    spec: Some(Box::new(spec.clone())),
                };
                self.deploy_history.write().await.push(entry);

                all_ids.extend(new_ids.iter().map(|id| id.0.clone()));
                continue;
            }

            // Fresh deploy: no existing instances
            let _ = events
                .send(ApplyEvent::Progress {
                    message: format!("deploying app {app_name} (replicas: {})", spec.replicas),
                })
                .await;

            let ids = match self
                .supervisor
                .deploy_app(app_name, namespace, spec, now)
                .await
            {
                Ok(ids) => ids,
                Err(e) => {
                    let _ = events
                        .send(ApplyEvent::Error {
                            message: e.to_string(),
                        })
                        .await;
                    return;
                }
            };

            // Register the app in the service map if it declares a port
            if let Some(port) = spec.port {
                let firewall = spec.firewall.as_ref().and_then(|f| {
                    if f.allow_from.is_empty() {
                        None
                    } else {
                        Some(f.allow_from.clone())
                    }
                });
                let _ = self
                    .service_map
                    .register_app(app_name, namespace, port, firewall);
                self.sync_backend_ebpf(app_name).await;
            }

            // Store ingress config for routing table
            if let Some(ref ingress) = spec.ingress {
                self.ingress_configs
                    .insert(app_name.to_string(), ingress.clone());
            }

            // Drive each instance through Pending → Preparing → Starting → HealthWait
            for id in &ids {
                let _ = events
                    .send(ApplyEvent::Progress {
                        message: format!("creating instance {}", id.0),
                    })
                    .await;

                if let Err(e) = self
                    .drive_instance_startup(id, app_name, namespace, spec)
                    .await
                {
                    let _ = events
                        .send(ApplyEvent::Error {
                            message: e.to_string(),
                        })
                        .await;
                    return;
                }

                let _ = events
                    .send(ApplyEvent::InstanceCreated {
                        id: id.0.clone(),
                        app: app_name.to_string(),
                    })
                    .await;
            }

            // Record the fresh deploy in history so `relish rollback`
            // has a previous version to return to (this path recorded
            // nothing before, so the first deploy was invisible).
            let entry = crate::meat::deploy_types::DeployHistoryEntry {
                id: crate::meat::deploy_types::DeployId(
                    SystemTime::now()
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                ),
                app_id: crate::meat::types::AppId::new(app_name, namespace),
                image: spec.image.clone().unwrap_or_default(),
                result: crate::meat::deploy_types::DeployResult::Completed,
                created_at: SystemTime::now(),
                completed_at: SystemTime::now(),
                steps_completed: ids.len(),
                steps_total: ids.len(),
                spec: Some(Box::new(spec.clone())),
            };
            self.deploy_history.write().await.push(entry);

            all_ids.extend(ids.iter().map(|id| id.0.clone()));
        }

        for (job_name, spec) in &config.job {
            let namespace = spec.namespace.as_deref().unwrap_or("default");
            let _ = events
                .send(ApplyEvent::Progress {
                    message: format!("deploying job {job_name}"),
                })
                .await;

            let ids = match self
                .supervisor
                .deploy_job(job_name, namespace, spec, now)
                .await
            {
                Ok(ids) => ids,
                Err(e) => {
                    let _ = events
                        .send(ApplyEvent::Error {
                            message: e.to_string(),
                        })
                        .await;
                    return;
                }
            };

            for id in &ids {
                let _ = events
                    .send(ApplyEvent::Progress {
                        message: format!("creating instance {}", id.0),
                    })
                    .await;

                if let Err(e) = self.drive_job_startup(id, job_name, namespace, spec).await {
                    let _ = events
                        .send(ApplyEvent::Error {
                            message: e.to_string(),
                        })
                        .await;
                    return;
                }

                let _ = events
                    .send(ApplyEvent::InstanceCreated {
                        id: id.0.clone(),
                        app: job_name.to_string(),
                    })
                    .await;
            }

            all_ids.extend(ids.iter().map(|id| id.0.clone()));
        }

        // Rebuild the routing table now that all instances are started
        self.rebuild_routing_table().await;

        let _ = events
            .send(ApplyEvent::Complete {
                created: all_ids.len(),
                instances: all_ids,
            })
            .await;
    }

    /// Drive a newly created instance through the startup state machine.
    /// Resolve the age identity that decrypts `namespace`'s secrets, if the
    /// cluster has the key material to unwrap it.
    ///
    /// Returns `None` in single-node mode, or before the wrapping IKM and age
    /// keypair are loaded into the council security state. Callers must then
    /// refuse to start any workload carrying encrypted secrets rather than leak
    /// the ciphertext into the container environment.
    /// Enforce the image trust policy for a workload before deploying it.
    ///
    /// Returns `Err(reason)` to reject the deploy. It's a no-op (`Ok`) when the
    /// policy doesn't require signatures, in single-node mode (no council), or
    /// for external-registry images not in the Pickle catalogue. For a
    /// Pickle-hosted image it verifies the signature against the cluster root
    /// CA.
    async fn enforce_image_signature(&self, spec: &AppSpec) -> Result<(), String> {
        if !self.trust_policy.require_signatures {
            return Ok(());
        }
        let Some(council) = self.cluster.as_ref().and_then(|c| c.council.as_ref()) else {
            return Ok(());
        };
        let catalog = council.manifest_catalog().await;
        let security_state = council.security_state().await;
        let root_ca = security_state
            .get_ca(crate::sesame::types::CaRole::Root)
            .map(|ca| ca.certificate_der.clone());
        crate::meat::scheduler::verify_image_signature(
            spec.image.as_deref(),
            &catalog,
            &self.trust_policy,
            root_ca.as_deref(),
        )
        .map_err(|e| e.to_string())
    }

    async fn decrypt_identity(&self, namespace: &str) -> Option<age::x25519::Identity> {
        let cluster = self.cluster.as_ref()?;
        let ikm = cluster.wrapping_ikm?;
        let council = cluster.council.as_ref()?;
        let security_state = council.security_state().await;
        let keypair = security_state
            .namespace_age_keypair(namespace)
            .or_else(|| security_state.cluster_age_keypair())?;
        crate::sesame::secret::unwrap_age_identity(keypair, &ikm).ok()
    }

    /// Build an OCI spec, decrypting `ENC[AGE:...]` env values with `identity`.
    ///
    /// This is synchronous on purpose: the `SecretDecryptor` closure is `!Send`
    /// and must never be held across an `.await` in the (spawned) agent task, so
    /// it is created and consumed entirely within this call.
    #[allow(clippy::too_many_arguments)]
    fn oci_spec_with_secrets(
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
        host_port: Option<u16>,
        cgroup_str: &str,
        volumes_dir: Option<&std::path::Path>,
        netns_path: Option<&str>,
        identity: Option<age::x25519::Identity>,
    ) -> crate::grill::oci::OciSpec {
        let decryptor: Option<crate::grill::oci::SecretDecryptor> = identity.map(|id| {
            Box::new(move |encrypted: &str| {
                crate::sesame::secret::decrypt_secret(encrypted, &id).map_err(|e| e.to_string())
            }) as crate::grill::oci::SecretDecryptor
        });
        crate::grill::oci::generate_oci_spec_with_decryptor(
            app_name,
            namespace,
            spec,
            host_port,
            cgroup_str,
            volumes_dir,
            netns_path,
            decryptor.as_ref(),
        )
    }

    async fn drive_instance_startup(
        &mut self,
        instance_id: &InstanceId,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
    ) -> Result<(), BunError> {
        // Pending → Preparing
        {
            let instance = self
                .supervisor
                .get_instance_mut(instance_id)
                .ok_or_else(|| BunError::InstanceNotFound {
                    instance_id: instance_id.clone(),
                })?;
            instance.state = instance.state.transition_to(ContainerState::Preparing)?;
        }

        // Generate OCI spec and call grill.create()
        let host_port = self
            .supervisor
            .get_instance(instance_id)
            .and_then(|i| i.host_port);

        // Extract the replica index from "app_name-N" format
        let instance_index: u32 = instance_id
            .0
            .rsplit('-')
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let cgroup_path = crate::grill::cgroup::cgroup_path(namespace, app_name, instance_index);
        let cgroup_str = cgroup_path.to_string_lossy();
        // Pass the pre-created network namespace path if the instance has
        // one (Linux + runc only). Otherwise each container gets its own
        // namespace via the OCI spec.
        let netns_path = self
            .netns_paths
            .get(instance_id)
            .map(|p| p.to_string_lossy().into_owned());
        // Decrypt `ENC[AGE:...]` secrets if we have the key material. If the
        // spec carries encrypted secrets but we can't decrypt them, fail closed
        // rather than pass ciphertext into the container environment.
        let identity = self.decrypt_identity(namespace).await;
        if identity.is_none() && spec.env.values().any(|v| v.is_encrypted()) {
            return Err(BunError::DeployFailed {
                app_name: app_name.to_string(),
                reason: "encrypted secrets require cluster security state (unavailable here)"
                    .to_string(),
            });
        }
        let oci_spec = Self::oci_spec_with_secrets(
            app_name,
            namespace,
            spec,
            host_port,
            &cgroup_str,
            Some(&self.volumes_dir),
            netns_path.as_deref(),
            identity,
        );

        self.supervisor
            .grill()
            .create(instance_id, &oci_spec)
            .await?;

        // Store OCI spec for restart re-drive
        if let Some(instance) = self.supervisor.get_instance_mut(instance_id) {
            instance.oci_spec = Some(oci_spec);
        }

        // Run init containers if any
        if !spec.init.is_empty() {
            // Preparing → Initialising
            {
                let instance = self
                    .supervisor
                    .get_instance_mut(instance_id)
                    .ok_or_else(|| BunError::InstanceNotFound {
                        instance_id: instance_id.clone(),
                    })?;
                instance.state = instance.state.transition_to(ContainerState::Initialising)?;
            }

            for (i, init_spec) in spec.init.iter().enumerate() {
                let init_id = InstanceId(format!("{}-init-{i}", instance_id.0));
                let init_oci = crate::grill::oci::generate_init_oci_spec(
                    &init_spec.command,
                    namespace,
                    app_name,
                    spec.image.as_deref(),
                    &cgroup_str,
                    None,
                );

                self.supervisor.grill().create(&init_id, &init_oci).await?;
                self.supervisor.grill().start(&init_id).await?;

                // Wait for the init container to complete, bounded by a timeout
                // so a hung init can't wedge the agent event loop forever.
                let deadline =
                    std::time::Instant::now() + std::time::Duration::from_secs(INIT_TIMEOUT_SECS);
                let failed = loop {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    let state = self.supervisor.grill().state(&init_id).await?;
                    if state == ContainerState::Stopped {
                        let exit_code = self.supervisor.grill().exit_code(&init_id).await;
                        break exit_code != Some(0);
                    }
                    if std::time::Instant::now() >= deadline {
                        // Timed out — kill the init container and fail the deploy.
                        let _ = self.supervisor.grill().kill(&init_id).await;
                        break true;
                    }
                };

                if failed {
                    if let Some(instance) = self.supervisor.get_instance_mut(instance_id) {
                        instance.state = instance.state.transition_to(ContainerState::Failed)?;
                    }
                    return Err(BunError::InitContainerFailed {
                        instance_id: instance_id.clone(),
                        init_index: i,
                    });
                }
            }
        }

        // Preparing/Initialising → Starting
        {
            let instance = self
                .supervisor
                .get_instance_mut(instance_id)
                .ok_or_else(|| BunError::InstanceNotFound {
                    instance_id: instance_id.clone(),
                })?;
            instance.state = instance.state.transition_to(ContainerState::Starting)?;
        }

        // Call grill.start()
        self.supervisor.grill().start(instance_id).await?;
        self.spawn_log_forwarder(instance_id, app_name, namespace);
        self.persist_instance_record(instance_id).await;

        // Starting → HealthWait, then immediately to Running if no health checks
        {
            let instance = self
                .supervisor
                .get_instance_mut(instance_id)
                .ok_or_else(|| BunError::InstanceNotFound {
                    instance_id: instance_id.clone(),
                })?;
            instance.state = instance.state.transition_to(ContainerState::HealthWait)?;
            if instance.health_config.is_none() {
                instance.state = instance.state.transition_to(ContainerState::Running)?;
            }
        }

        // Register as a backend in the service map if the instance has a port
        if let Some(instance) = self.supervisor.get_instance(instance_id)
            && let Some(host_port) = instance.host_port
        {
            let node_ip = instance
                .container_ip
                .unwrap_or(std::net::Ipv4Addr::LOCALHOST);
            let backend = crate::onion::types::BackendInstance {
                instance_id: instance_id.0.clone(),
                node_ip,
                host_port,
                healthy: instance.state == ContainerState::Running,
            };
            let _ = self.service_map.add_backend(app_name, backend);
        }

        // L8: mirror the freshly-registered backend into the eBPF backend_map.
        self.sync_backend_ebpf(app_name).await;

        // L16: program the egress allowlist for this instance's cgroup.
        self.apply_egress(instance_id, spec).await;

        Ok(())
    }

    /// Program an instance's egress allowlist into the eBPF maps (L16).
    ///
    /// With an `[egress] allow` list, only the listed destinations are
    /// permitted from the instance's cgroup; everything else is denied at
    /// the kernel. A no-op without an allowlist (all egress permitted) or
    /// when the eBPF data path isn't loaded — in which case it warns, per
    /// the default-deny-is-unenforced contract, rather than pretending.
    #[cfg(feature = "ebpf")]
    async fn apply_egress(&mut self, instance_id: &InstanceId, spec: &AppSpec) {
        let Some(egress) = spec.egress.as_ref().filter(|e| !e.allow.is_empty()) else {
            return;
        };

        let Some(pid) = self.supervisor.grill().pid(instance_id).await else {
            return;
        };
        let Some(cgroup_id) = crate::sesame::egress::cgroup_id_of_pid(pid) else {
            eprintln!(
                "sesame: could not resolve cgroup for {}; egress unenforced",
                instance_id.0
            );
            return;
        };

        // DNS resolution can block — do it off the event loop.
        let allow = egress.allow.clone();
        let allow_for_resolve = allow.clone();
        let resolved = match tokio::task::spawn_blocking(move || {
            crate::sesame::egress::resolve_egress_entries(&allow_for_resolve)
        })
        .await
        {
            Ok(Ok(entries)) => entries,
            Ok(Err(e)) => {
                eprintln!(
                    "sesame: egress resolution failed for {}: {e}",
                    instance_id.0
                );
                return;
            }
            Err(_) => return,
        };

        let Some(handle) = self.onion_ebpf.as_ref() else {
            eprintln!(
                "sesame: [egress] set for {} but eBPF is not loaded; egress is NOT enforced",
                instance_id.0
            );
            return;
        };

        let entries = crate::sesame::egress::egress_to_bpf_entries(&[cgroup_id], &resolved);
        let mut ebpf = handle.lock().await;
        for (key, value) in entries {
            if let Err(e) = crate::sesame::egress::write_egress_entry(&mut ebpf.bpf, key, value) {
                eprintln!("sesame: egress map write failed for {}: {e}", instance_id.0);
                return;
            }
        }
        if let Err(e) = crate::sesame::egress::set_egress_enforced(&mut ebpf.bpf, cgroup_id) {
            eprintln!(
                "sesame: enabling egress enforcement failed for {}: {e}",
                instance_id.0
            );
            return;
        }
        drop(ebpf);
        self.egress_bindings.insert(
            instance_id.clone(),
            EgressBinding {
                cgroup_id,
                allow,
                resolved,
            },
        );
    }

    /// Egress enforcement is a no-op without the eBPF data path.
    #[cfg(not(feature = "ebpf"))]
    async fn apply_egress(&mut self, _instance_id: &InstanceId, spec: &AppSpec) {
        if spec.egress.as_ref().is_some_and(|e| !e.allow.is_empty()) {
            eprintln!(
                "sesame: [egress] configured but this build has no eBPF support; egress is NOT enforced"
            );
        }
    }

    /// Lift egress enforcement for a stopped instance's cgroup (L16).
    ///
    /// Enforcement is keyed by the cgroup id, which is unique per
    /// instance, so clearing the enable flag is enough; stale allow
    /// entries become inert once nothing enforces against them.
    #[cfg(feature = "ebpf")]
    async fn clear_egress(&mut self, instance_id: &InstanceId) {
        let Some(binding) = self.egress_bindings.remove(instance_id) else {
            return;
        };
        if let Some(handle) = self.onion_ebpf.as_ref() {
            let mut ebpf = handle.lock().await;
            let _ = crate::sesame::egress::clear_egress_enforced(&mut ebpf.bpf, binding.cgroup_id);
        }
    }

    /// No-op without the eBPF data path.
    #[cfg(not(feature = "ebpf"))]
    async fn clear_egress(&mut self, _instance_id: &InstanceId) {}

    /// Periodically re-resolve DNS-based egress allowlists and reprogram the
    /// eBPF egress map when an app's destination IPs change (L16). Rate-
    /// limited to roughly once every five minutes; a no-op while nothing
    /// enforces egress.
    #[cfg(feature = "ebpf")]
    async fn reresolve_egress(&mut self) {
        // ~5 minutes at the 1s event-loop tick.
        const RERESOLVE_EVERY_TICKS: u32 = 300;
        self.egress_reresolve_ticks += 1;
        if self.egress_reresolve_ticks < RERESOLVE_EVERY_TICKS || self.egress_bindings.is_empty() {
            return;
        }
        self.egress_reresolve_ticks = 0;

        let Some(handle) = self.onion_ebpf.clone() else {
            return;
        };
        // Snapshot so we don't hold a borrow of self across the DNS awaits.
        let bindings: Vec<(InstanceId, EgressBinding)> = self
            .egress_bindings
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();

        for (instance_id, binding) in bindings {
            let new_resolved =
                match crate::sesame::egress::re_resolve_egress_async(&binding.allow).await {
                    Ok(r) => r,
                    Err(e) => {
                        eprintln!(
                            "sesame: egress re-resolve failed for {}: {e}",
                            instance_id.0
                        );
                        continue;
                    }
                };
            let (to_add, to_remove) =
                crate::sesame::egress::egress_diff(&binding.resolved, &new_resolved);
            if to_add.is_empty() && to_remove.is_empty() {
                continue;
            }

            let adds = crate::sesame::egress::egress_to_bpf_entries(&[binding.cgroup_id], &to_add);
            let removes =
                crate::sesame::egress::egress_to_bpf_entries(&[binding.cgroup_id], &to_remove);
            {
                let mut ebpf = handle.lock().await;
                for (key, value) in adds {
                    let _ = crate::sesame::egress::write_egress_entry(&mut ebpf.bpf, key, value);
                }
                for (key, _) in removes {
                    let _ = crate::sesame::egress::delete_egress_entry(&mut ebpf.bpf, key);
                }
            }
            // Record the new set so the next diff is against reality.
            if let Some(b) = self.egress_bindings.get_mut(&instance_id) {
                b.resolved = new_resolved;
            }
        }
    }

    /// No-op without the eBPF data path.
    #[cfg(not(feature = "ebpf"))]
    async fn reresolve_egress(&mut self) {}

    /// Drive a job instance through startup: Pending → Preparing → Starting → Running.
    ///
    /// Jobs skip health checks and go straight to Running.
    async fn drive_job_startup(
        &mut self,
        instance_id: &InstanceId,
        job_name: &str,
        namespace: &str,
        spec: &JobSpec,
    ) -> Result<(), BunError> {
        // Pending → Preparing
        {
            let instance = self
                .supervisor
                .get_instance_mut(instance_id)
                .ok_or_else(|| BunError::InstanceNotFound {
                    instance_id: instance_id.clone(),
                })?;
            instance.state = instance.state.transition_to(ContainerState::Preparing)?;
        }

        let instance_index: u32 = instance_id
            .0
            .rsplit('-')
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let cgroup_path = crate::grill::cgroup::cgroup_path(namespace, job_name, instance_index);
        let cgroup_str = cgroup_path.to_string_lossy();
        let oci_spec = generate_job_oci_spec(job_name, namespace, spec, &cgroup_str, None);

        self.supervisor
            .grill()
            .create(instance_id, &oci_spec)
            .await?;

        // Store OCI spec for restart re-drive
        if let Some(instance) = self.supervisor.get_instance_mut(instance_id) {
            instance.oci_spec = Some(oci_spec);
        }

        // Preparing → Starting
        {
            let instance = self
                .supervisor
                .get_instance_mut(instance_id)
                .ok_or_else(|| BunError::InstanceNotFound {
                    instance_id: instance_id.clone(),
                })?;
            instance.state = instance.state.transition_to(ContainerState::Starting)?;
        }

        self.supervisor.grill().start(instance_id).await?;
        self.spawn_log_forwarder(instance_id, job_name, namespace);
        self.persist_instance_record(instance_id).await;

        // Starting → HealthWait → Running (no health checks for jobs)
        {
            let instance = self
                .supervisor
                .get_instance_mut(instance_id)
                .ok_or_else(|| BunError::InstanceNotFound {
                    instance_id: instance_id.clone(),
                })?;
            instance.state = instance.state.transition_to(ContainerState::HealthWait)?;
            instance.state = instance.state.transition_to(ContainerState::Running)?;
        }

        Ok(())
    }

    /// Run any due health checks.
    async fn run_health_checks(&mut self) {
        let now = Instant::now();

        // Collect all due checks
        let mut due_checks = Vec::new();
        while let Some((instance_id, config)) = self.supervisor.health_checker_mut().pop_due(now) {
            due_checks.push((instance_id, config));
        }

        for (instance_id, config) in due_checks {
            // Only probe instances in a probeable state
            let (state, probe_host) = match self.supervisor.get_instance(&instance_id) {
                Some(i) => (Some(i.state), probe_host(i.container_ip)),
                None => (None, "127.0.0.1".to_string()),
            };

            let should_probe = matches!(
                state,
                Some(ContainerState::HealthWait)
                    | Some(ContainerState::Running)
                    | Some(ContainerState::Unhealthy)
            );

            if should_probe {
                let status = probe_health(&config, &probe_host).await;

                let transition = self.supervisor.process_health_result(&instance_id, status);

                // Propagate health transitions to the service map, noting the
                // app so we can re-sync its eBPF backend_map entry afterwards.
                let mut health_changed_app: Option<String> = None;
                match &transition {
                    Ok(Some(ContainerState::Running)) => {
                        if let Some(inst) = self.supervisor.get_instance(&instance_id) {
                            let app = inst.app_name.clone();
                            let _ = self
                                .service_map
                                .set_backend_health(&app, &instance_id.0, true);
                            health_changed_app = Some(app);
                        }
                    }
                    Ok(Some(ContainerState::Unhealthy)) => {
                        if let Some(inst) = self.supervisor.get_instance(&instance_id) {
                            let app = inst.app_name.clone();
                            let _ =
                                self.service_map
                                    .set_backend_health(&app, &instance_id.0, false);
                            health_changed_app = Some(app);
                        }
                    }
                    _ => {}
                }
                if let Some(app) = health_changed_app {
                    self.sync_backend_ebpf(&app).await;
                }

                // Handle restart if unhealthy
                if let Ok(Some(ContainerState::Unhealthy)) = transition {
                    let _ = self.supervisor.maybe_restart(&instance_id, now).await;
                }
            }

            // Schedule the next check
            self.supervisor
                .health_checker_mut()
                .schedule_next(instance_id, now);
        }
    }

    /// Monitor running job instances for process exit.
    ///
    /// For each running job, polls the runtime to see if the process has
    /// exited. On success (exit code 0), transitions to Stopped. On
    /// failure, attempts a restart or marks as Failed if the retry limit
    /// is exhausted.
    async fn check_jobs(&mut self) {
        let now = Instant::now();

        // Check running job instances for process exit
        let running_jobs: Vec<InstanceId> = self
            .supervisor
            .list_instances()
            .iter()
            .filter(|i| i.is_job && i.state == ContainerState::Running)
            .map(|i| i.id.clone())
            .collect();

        for id in running_jobs {
            let grill_state = match self.supervisor.grill().state(&id).await {
                Ok(s) => s,
                Err(_) => continue,
            };

            if grill_state == ContainerState::Stopped {
                let exit_code = self.supervisor.grill().exit_code(&id).await;

                // Transition Running → Stopping → Stopped
                if let Some(instance) = self.supervisor.get_instance_mut(&id) {
                    if let Ok(s) = instance.state.transition_to(ContainerState::Stopping) {
                        instance.state = s;
                    }
                    if let Ok(s) = instance.state.transition_to(ContainerState::Stopped) {
                        instance.state = s;
                    }
                }

                if exit_code == Some(0) {
                    // Job completed successfully — stays in Stopped
                    continue;
                }

                // Job failed — attempt restart
                match self.supervisor.maybe_restart(&id, now).await {
                    Ok(true) => {
                        // Now in Pending — drive_pending_restarts will handle it
                    }
                    Ok(false) => {
                        // Backoff not elapsed — will retry on next tick
                    }
                    Err(_) => {
                        // Exceeded restart limit — mark as Failed
                        if let Some(instance) = self.supervisor.get_instance_mut(&id)
                            && let Ok(s) = instance.state.transition_to(ContainerState::Failed)
                        {
                            instance.state = s;
                        }
                    }
                }
            }
        }

        // Retry stopped failed jobs waiting for backoff
        let stopped_jobs: Vec<InstanceId> = self
            .supervisor
            .list_instances()
            .iter()
            .filter(|i| i.is_job && i.state == ContainerState::Stopped && i.restart_count > 0)
            .map(|i| i.id.clone())
            .collect();

        for id in stopped_jobs {
            match self.supervisor.maybe_restart(&id, now).await {
                Ok(true) => {
                    // Now in Pending — drive_pending_restarts will handle it
                }
                Ok(false) => {
                    // Still in backoff
                }
                Err(_) => {
                    if let Some(instance) = self.supervisor.get_instance_mut(&id)
                        && let Ok(s) = instance.state.transition_to(ContainerState::Failed)
                    {
                        instance.state = s;
                    }
                }
            }
        }
    }

    /// Detect crashed app instances and restart them.
    ///
    /// Health checks catch an app that fails its probe, but an app *without* a
    /// health check that crashes was previously reported Running forever —
    /// nothing polled the runtime. This polls non-job Running apps and, when the
    /// container has exited, routes them through the restart path.
    async fn check_apps(&mut self) {
        let now = Instant::now();
        let running_apps: Vec<InstanceId> = self
            .supervisor
            .list_instances()
            .iter()
            .filter(|i| !i.is_job && i.state == ContainerState::Running)
            .map(|i| i.id.clone())
            .collect();

        for id in running_apps {
            let grill_state = match self.supervisor.grill().state(&id).await {
                Ok(s) => s,
                Err(_) => continue,
            };
            if grill_state != ContainerState::Stopped {
                continue;
            }

            // The process exited unexpectedly. Mark it Stopped, then restart.
            if let Some(instance) = self.supervisor.get_instance_mut(&id) {
                if let Ok(s) = instance.state.transition_to(ContainerState::Stopping) {
                    instance.state = s;
                }
                if let Ok(s) = instance.state.transition_to(ContainerState::Stopped) {
                    instance.state = s;
                }
            }
            if let Err(BunError::RestartLimitExceeded { .. }) =
                self.supervisor.maybe_restart(&id, now).await
                && let Some(instance) = self.supervisor.get_instance_mut(&id)
                && let Ok(s) = instance.state.transition_to(ContainerState::Failed)
            {
                instance.state = s;
            }
        }
    }

    /// Re-drive instances that are in Pending state after a restart.
    ///
    /// When `maybe_restart` transitions an instance back to Pending,
    /// this method picks it up and drives it through the startup
    /// sequence again using the stored OCI spec.
    async fn drive_pending_restarts(&mut self) {
        #[allow(clippy::type_complexity)]
        let pending_restarts: Vec<(
            InstanceId,
            crate::grill::oci::OciSpec,
            String,
            String,
            Option<u16>,
        )> = self
            .supervisor
            .list_instances()
            .iter()
            .filter(|i| i.state == ContainerState::Pending && i.restart_count > 0)
            .filter_map(|i| {
                i.oci_spec.as_ref().map(|spec| {
                    (
                        i.id.clone(),
                        spec.clone(),
                        i.app_name.clone(),
                        i.namespace.clone(),
                        i.host_port,
                    )
                })
            })
            .collect();

        for (id, oci_spec, app_name, namespace, host_port) in pending_restarts {
            // Tear down the old container first. Without this, the same-id
            // create is rejected (ProcessGrill: stale-Running entry) or fails
            // (runc/apple: container still exists), leaving the instance wedged
            // in Preparing and the old process leaked.
            let _ = self.supervisor.grill().kill(&id).await;

            // Pending → Preparing
            if let Some(instance) = self.supervisor.get_instance_mut(&id) {
                match instance.state.transition_to(ContainerState::Preparing) {
                    Ok(s) => instance.state = s,
                    Err(_) => continue,
                }
            }

            if self
                .supervisor
                .grill()
                .create(&id, &oci_spec)
                .await
                .is_err()
            {
                continue;
            }

            // Preparing → Starting
            if let Some(instance) = self.supervisor.get_instance_mut(&id) {
                match instance.state.transition_to(ContainerState::Starting) {
                    Ok(s) => instance.state = s,
                    Err(_) => continue,
                }
            }

            if self.supervisor.grill().start(&id).await.is_err() {
                continue;
            }
            // Re-wire the restarted instance: stream its logs and keep it routable.
            self.spawn_log_forwarder(&id, &app_name, &namespace);
            self.persist_instance_record(&id).await;
            if let Some(port) = host_port {
                let backend = crate::onion::types::BackendInstance {
                    instance_id: id.0.clone(),
                    node_ip: std::net::Ipv4Addr::LOCALHOST,
                    host_port: port,
                    healthy: true,
                };
                let _ = self.service_map.add_backend(&app_name, backend);
            }
            self.sync_backend_ebpf(&app_name).await;

            // Starting → HealthWait, then Running if no health checks
            if let Some(instance) = self.supervisor.get_instance_mut(&id) {
                if let Ok(s) = instance.state.transition_to(ContainerState::HealthWait) {
                    instance.state = s;
                }
                if instance.health_config.is_none()
                    && let Ok(s) = instance.state.transition_to(ContainerState::Running)
                {
                    instance.state = s;
                }
            }
        }
    }

    /// Stop an app's instances.
    async fn stop_app(&mut self, app_name: &str, namespace: &str) -> Result<(), BunError> {
        // Get instance IDs for this app
        let instances: Vec<InstanceId> = self
            .supervisor
            .list_instances()
            .iter()
            .filter(|i| i.app_name == app_name && i.namespace == namespace)
            .map(|i| i.id.clone())
            .collect();

        if instances.is_empty() {
            return Err(BunError::AppNotFound {
                app_name: app_name.to_string(),
                namespace: namespace.to_string(),
            });
        }

        // Stop via supervisor
        self.supervisor.stop_app(app_name, namespace).await?;

        // Send stop to runtime
        for id in &instances {
            let _ = self.supervisor.grill().stop(id).await;
        }

        // Transition Stopping → Stopped
        for id in &instances {
            if let Some(instance) = self.supervisor.get_instance_mut(id)
                && instance.state == ContainerState::Stopping
            {
                let _ = instance
                    .state
                    .transition_to(ContainerState::Stopped)
                    .map(|s| {
                        instance.state = s;
                    });
            }
        }

        // Stopped for good: nothing left to adopt after a restart.
        for id in &instances {
            self.remove_instance_record(id);
        }

        // Remove backends and unregister from the service map
        for id in &instances {
            let _ = self.service_map.remove_backend(app_name, &id.0);
            self.clear_egress(id).await;
        }
        self.remove_backend_ebpf(app_name).await;
        let _ = self.service_map.unregister_app(app_name);
        self.ingress_configs.remove(app_name);
        self.rebuild_routing_table().await;

        Ok(())
    }

    /// Rebuild the Wrapper routing table from the current service map
    /// and ingress configs.
    async fn rebuild_routing_table(&self) {
        let mut table = self.routing_table.write().await;
        table.rebuild(&self.service_map, &self.ingress_configs);
        drop(table);

        // Publish a service-map snapshot for out-of-loop readers (DNS).
        // send() only errs when no receiver exists, which is fine.
        let _ = self.service_map_tx.send(self.service_map.clone());
    }

    /// Reconcile the perimeter firewall if cluster membership changed.
    async fn reconcile_firewall(&mut self) {
        if !self.perimeter_config.enabled {
            return;
        }

        // Collect cluster node IPs from gossip membership. Reconcile when
        // the *set* changes — a node swap keeps the count constant (M18) —
        // and always on the first pass (`None`), so a standalone node with
        // no peers still gets the firewall applied.
        let cluster_nodes = self.collect_cluster_node_ips();
        if self.last_firewall_nodes.as_ref() == Some(&cluster_nodes) {
            return;
        }

        let ruleset =
            crate::firewall::rules::generate_ruleset(&self.perimeter_config, &cluster_nodes);

        if let Err(e) = crate::firewall::rules::apply_ruleset(&ruleset).await {
            eprintln!("warning: firewall reconciliation failed: {e}");
        } else {
            self.last_firewall_nodes = Some(cluster_nodes);
        }
    }

    /// Provision workload identity for an instance after it passes health check.
    ///
    /// Generates a SPIFFE CSR, submits it to the council for signing,
    /// builds the identity bundle, and writes cert/key/JWT to the
    /// instance's identity mount. No-op in standalone mode.
    async fn provision_identity(
        &mut self,
        app_name: &str,
        namespace: &str,
        instance_id: &crate::grill::InstanceId,
        is_job: bool,
        events: &mpsc::Sender<ApplyEvent>,
    ) {
        let Some(ref cluster) = self.cluster else {
            return; // standalone mode — no council to sign CSRs
        };
        let Some(ref council) = cluster.council else {
            return;
        };

        let workload_type = if is_job {
            crate::sesame::types::WorkloadType::Job
        } else {
            crate::sesame::types::WorkloadType::App
        };

        // TODO: get cluster_name from config rather than hardcoding
        let cluster_name = "default";
        let spiffe_uri = crate::sesame::types::SpiffeUri {
            trust_domain: cluster_name.to_string(),
            namespace: namespace.to_string(),
            workload_type,
            name: app_name.to_string(),
        };

        // Generate CSR (keypair stays local)
        let (csr_der, private_key_der) =
            match crate::sesame::identity::create_workload_csr(&spiffe_uri) {
                Ok(pair) => pair,
                Err(e) => {
                    let _ = events
                        .send(ApplyEvent::Progress {
                            message: format!("identity: CSR generation failed: {e}"),
                        })
                        .await;
                    return;
                }
            };

        // Submit CSR to council
        let result = council
            .sign_workload_csr(&csr_der, &spiffe_uri, cluster_name, "local", &instance_id.0)
            .await;

        match result {
            Ok(csr_result) => {
                let jwt = csr_result.jwt_token.unwrap_or_default();
                let identity = crate::sesame::identity::build_identity_bundle(
                    spiffe_uri,
                    csr_result.cert_der,
                    private_key_der,
                    &csr_result.workload_ca_cert_der,
                    &csr_result.root_ca_cert_der,
                    jwt,
                );

                // Write to the identity mount
                let identity_dir = self
                    .volumes_dir
                    .join(".identity")
                    .join(namespace)
                    .join(app_name);
                if let Err(e) =
                    crate::sesame::identity::write_identity_to_tmpfs(&identity, &identity_dir)
                {
                    let _ = events
                        .send(ApplyEvent::Progress {
                            message: format!("identity: failed to write files: {e}"),
                        })
                        .await;
                    return;
                }

                // Store in supervisor
                if let Some(inst) = self.supervisor.get_instance_mut(instance_id) {
                    inst.identity = Some(identity);
                    inst.identity_mount = Some(identity_dir);
                }

                let _ = events
                    .send(ApplyEvent::Progress {
                        message: format!("{} identity provisioned ✓", instance_id.0),
                    })
                    .await;
            }
            Err(e) => {
                let _ = events
                    .send(ApplyEvent::Progress {
                        message: format!("identity: council CSR signing failed: {e}"),
                    })
                    .await;
            }
        }
    }

    /// Handle a join request: validate the token, issue a node certificate.
    async fn handle_join(&self, token: &str, addr: &str) -> Result<String, BunError> {
        let cluster = self
            .cluster
            .as_ref()
            .ok_or_else(|| BunError::SecurityError {
                reason: "no cluster available for join validation".to_string(),
            })?;
        let council = cluster
            .council
            .as_ref()
            .ok_or_else(|| BunError::SecurityError {
                reason: "no council available for join validation".to_string(),
            })?;
        let ikm = cluster
            .wrapping_ikm
            .as_ref()
            .ok_or_else(|| BunError::SecurityError {
                reason: "no wrapping IKM available".to_string(),
            })?;

        // Read security state and validate token
        let mut security_state = council.security_state().await;
        let node_id = format!("node-{addr}");

        let join_result =
            crate::sesame::join::validate_and_issue(token, &node_id, &mut security_state, ikm)
                .map_err(|e| BunError::SecurityError {
                    reason: format!("join validation failed: {e}"),
                })?;

        // Find the consumed token's hash for the Raft write
        let token_hash = security_state
            .join_tokens
            .iter()
            .find(|jt| jt.consumed && crate::sesame::ca::verify_join_token(token, &jt.token_hash))
            .map(|jt| jt.token_hash)
            .ok_or_else(|| BunError::SecurityError {
                reason: "could not find consumed token hash".to_string(),
            })?;

        // Persist token consumption to Raft
        council
            .write(crate::council::RaftRequest::ConsumeJoinToken { token_hash })
            .await
            .map_err(|e| BunError::SecurityError {
                reason: format!("failed to persist token consumption: {e}"),
            })?;

        Ok(format!(
            "join accepted: node {} issued cert serial {}",
            join_result.node_certificate.node_id, join_result.node_certificate.serial
        ))
    }

    /// Handle a SignImage command: sign a manifest digest and attach via Raft.
    async fn handle_sign_image(&self, manifest_digest: &str) -> Result<String, BunError> {
        let cluster = self
            .cluster
            .as_ref()
            .ok_or_else(|| BunError::SecurityError {
                reason: "no cluster available for signing".to_string(),
            })?;
        let council = cluster
            .council
            .as_ref()
            .ok_or_else(|| BunError::SecurityError {
                reason: "no council available for signing".to_string(),
            })?;

        let digest = crate::pickle::types::Digest::new(manifest_digest).map_err(|e| {
            BunError::SecurityError {
                reason: format!("invalid digest: {e}"),
            }
        })?;

        // Generate an ephemeral signing keypair
        let rng = ring::rand::SystemRandom::new();
        let pkcs8 = ring::signature::EcdsaKeyPair::generate_pkcs8(
            &ring::signature::ECDSA_P256_SHA256_ASN1_SIGNING,
            &rng,
        )
        .map_err(|_| BunError::SecurityError {
            reason: "failed to generate signing keypair".to_string(),
        })?;

        let sig = crate::pickle::signing::create_external_key_signature(
            &digest,
            pkcs8.as_ref(),
            "local-agent",
        )
        .map_err(|e| BunError::SecurityError {
            reason: format!("signing failed: {e}"),
        })?;

        let attach = crate::pickle::types::AttachSignature {
            manifest_digest: digest,
            signature: sig,
        };
        council
            .write(crate::council::RaftRequest::AttachSignature(attach))
            .await
            .map_err(|e| BunError::SecurityError {
                reason: format!("failed to attach signature: {e}"),
            })?;

        Ok(format!("signature attached to {manifest_digest}"))
    }

    /// Check identity rotation for all instances.
    async fn check_identity_rotation(&mut self) {
        let now = std::time::SystemTime::now();
        let mut needs_rotation = Vec::new();

        for inst in self.supervisor.list_instances() {
            if let Some(ref identity) = inst.identity {
                let state = crate::sesame::identity::rotation_state(identity, now);
                match state {
                    crate::sesame::identity::RotationState::NeedsRotation => {
                        needs_rotation.push((
                            inst.id.clone(),
                            inst.app_name.clone(),
                            inst.namespace.clone(),
                            inst.is_job,
                        ));
                    }
                    crate::sesame::identity::RotationState::Expired => {
                        eprintln!(
                            "warning: identity expired for {} ({})",
                            inst.id.0, inst.app_name
                        );
                    }
                    crate::sesame::identity::RotationState::GracePeriod => {
                        eprintln!(
                            "warning: identity in grace period for {} ({})",
                            inst.id.0, inst.app_name
                        );
                    }
                    crate::sesame::identity::RotationState::Valid => {}
                }
            }
        }

        // Re-provision identities that need rotation
        let (dummy_tx, _dummy_rx) = mpsc::channel(1);
        for (id, app, ns, is_job) in needs_rotation {
            self.provision_identity(&app, &ns, &id, is_job, &dummy_tx)
                .await;
        }
    }

    /// Collect cluster node IPs from the gossip membership table.
    fn collect_cluster_node_ips(&self) -> crate::firewall::rules::ClusterNodes {
        let mut nodes = crate::firewall::rules::ClusterNodes::new();

        if let Some(ref cluster) = self.cluster {
            let membership = cluster.membership_rx.borrow();
            for snapshot in membership.iter() {
                nodes.insert(snapshot.address.ip());
            }
        }

        nodes
    }

    /// Get status of all instances.
    async fn get_status(&self) -> Vec<InstanceStatus> {
        let mut statuses = Vec::new();
        for instance in self.supervisor.list_instances() {
            let pid = self.supervisor.grill().pid(&instance.id).await;
            statuses.push(InstanceStatus {
                id: instance.id.0.clone(),
                app_name: instance.app_name.clone(),
                namespace: instance.namespace.clone(),
                state: instance.state.to_string(),
                restart_count: instance.restart_count,
                host_port: instance.host_port,
                pid,
            });
        }
        statuses
    }

    /// Get logs for all instances of an app in a namespace.
    async fn get_logs(&self, app_name: &str, namespace: &str) -> Result<String, BunError> {
        let instance_ids: Vec<InstanceId> = self
            .supervisor
            .list_instances()
            .into_iter()
            .filter(|i| i.app_name == app_name && i.namespace == namespace)
            .map(|i| i.id.clone())
            .collect();

        if instance_ids.is_empty() {
            return Err(BunError::AppNotFound {
                app_name: app_name.to_string(),
                namespace: namespace.to_string(),
            });
        }

        let mut all_logs = String::new();
        for id in &instance_ids {
            let logs = self.supervisor.grill().logs(id).await.unwrap_or_default();
            if !logs.is_empty() {
                if instance_ids.len() > 1 {
                    all_logs.push_str(&format!("==> {id} <==\n"));
                }
                all_logs.push_str(&logs);
                if !logs.ends_with('\n') {
                    all_logs.push('\n');
                }
            }
        }
        Ok(all_logs)
    }

    /// Start streaming logs for all instances of an app.
    ///
    /// If `tail` is set, sends the last N lines of existing output first,
    /// then starts following. Spawns a background task per instance so
    /// the agent event loop isn't blocked.
    async fn follow_app_logs(
        &self,
        app_name: &str,
        namespace: &str,
        tail: Option<usize>,
        lines: mpsc::Sender<String>,
    ) {
        let instance_ids: Vec<InstanceId> = self
            .supervisor
            .list_instances()
            .into_iter()
            .filter(|i| i.app_name == app_name && i.namespace == namespace)
            .map(|i| i.id.clone())
            .collect();

        if instance_ids.is_empty() {
            return;
        }

        // Send initial tail lines if requested
        if let Some(n) = tail {
            for id in &instance_ids {
                let logs = self.supervisor.grill().logs(id).await.unwrap_or_default();
                let tailed = tail_lines(&logs, n);
                for line in tailed.lines() {
                    if lines.send(line.to_string()).await.is_err() {
                        return;
                    }
                }
            }
        }

        // Spawn a follow task per instance. The grill is Arc-backed and Clone,
        // so each task owns a handle and streams concurrently — the agent event
        // loop is never blocked waiting for a client to disconnect.
        for id in instance_ids {
            let grill = self.supervisor.grill().clone();
            let tx = lines.clone();
            tokio::spawn(async move {
                grill.follow_logs(&id, tx).await;
            });
        }
    }

    /// Execute a command inside a running instance of an app.
    ///
    /// Finds the first running instance of the app in the given namespace
    /// and delegates to `grill.exec()`. In Phase 1 (ProcessGrill), this
    /// just spawns the command directly. Phase 3+ will add namespace entry.
    async fn exec_app(
        &self,
        app_name: &str,
        namespace: &str,
        command: &[String],
    ) -> Result<String, BunError> {
        let instance_id = self
            .supervisor
            .list_instances()
            .into_iter()
            .find(|i| {
                i.app_name == app_name
                    && i.namespace == namespace
                    && i.state == ContainerState::Running
            })
            .map(|i| i.id.clone())
            .ok_or_else(|| BunError::AppNotFound {
                app_name: app_name.to_string(),
                namespace: namespace.to_string(),
            })?;

        let output = self.supervisor.grill().exec(&instance_id, command).await?;
        Ok(output)
    }

    /// Gracefully stop all instances.
    async fn shutdown_all(&mut self) {
        let ids: Vec<InstanceId> = self
            .supervisor
            .list_instances()
            .iter()
            .map(|i| i.id.clone())
            .collect();

        // Ask everything to stop (SIGTERM), wait (up to a grace period, but no
        // longer than needed) for it to exit, then force-kill (SIGKILL) whatever
        // is still running so nothing is orphaned.
        for id in &ids {
            let _ = self.supervisor.grill().stop(id).await;
        }
        let deadline = Instant::now() + std::time::Duration::from_secs(SHUTDOWN_GRACE_SECS);
        loop {
            let mut all_stopped = true;
            for id in &ids {
                if !matches!(
                    self.supervisor.grill().state(id).await,
                    Ok(ContainerState::Stopped)
                ) {
                    all_stopped = false;
                    break;
                }
            }
            if all_stopped || Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        for id in &ids {
            if !matches!(
                self.supervisor.grill().state(id).await,
                Ok(ContainerState::Stopped)
            ) {
                let _ = self.supervisor.grill().kill(id).await;
            }
        }
    }
}

/// Return the last `n` lines of a string.
///
/// If the string has fewer than `n` lines, the whole string is returned.
/// Preserves a trailing newline if present.
pub fn tail_lines(s: &str, n: usize) -> String {
    if n == 0 {
        return String::new();
    }
    let lines: Vec<&str> = s.lines().collect();
    let start = lines.len().saturating_sub(n);
    let result = lines[start..].join("\n");
    if s.ends_with('\n') && !result.is_empty() {
        format!("{result}\n")
    } else {
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grill::mock::MockGrill;

    fn test_agent() -> (
        BunAgent<MockGrill>,
        mpsc::Sender<AgentCommand>,
        CancellationToken,
    ) {
        let (agent, tx, shutdown, _grill) = test_agent_with_grill();
        (agent, tx, shutdown)
    }

    fn test_agent_with_grill() -> (
        BunAgent<MockGrill>,
        mpsc::Sender<AgentCommand>,
        CancellationToken,
        MockGrill,
    ) {
        let (tx, rx) = mpsc::channel(32);
        let shutdown = CancellationToken::new();
        let grill = MockGrill::new();
        let grill_handle = grill.clone();
        let port_allocator = PortAllocator::new(30000, 31000);
        let agent = BunAgent::new(grill, port_allocator, rx, shutdown.clone());
        (agent, tx, shutdown, grill_handle)
    }

    /// Send a Deploy command and collect all events. Returns the list
    /// of events (the last one should be Complete or Error).
    async fn send_deploy(tx: &mpsc::Sender<AgentCommand>, config: Config) -> Vec<ApplyEvent> {
        let (event_tx, mut event_rx) = mpsc::channel(64);
        tx.send(AgentCommand::Deploy {
            config,
            events: event_tx,
        })
        .await
        .unwrap();

        let mut events = Vec::new();
        while let Some(e) = event_rx.recv().await {
            events.push(e);
        }
        events
    }

    /// Extract the Complete event from a list of deploy events.
    /// Panics if the last event is an Error or if there are no events.
    fn expect_complete(events: &[ApplyEvent]) -> (usize, &[String]) {
        match events.last().expect("no events received") {
            ApplyEvent::Complete { created, instances } => (*created, instances),
            ApplyEvent::Error { message } => panic!("deploy failed: {message}"),
            other => panic!("unexpected final event: {other:?}"),
        }
    }

    fn basic_config() -> Config {
        let toml_str = r#"
            [app.web]
            image = "myapp:v1"
            port = 8080
        "#;
        Config::parse(toml_str).unwrap()
    }

    fn config_with_health() -> Config {
        let toml_str = r#"
            [app.web]
            image = "myapp:v1"
            port = 8080

            [app.web.health]
            path = "/healthz"
        "#;
        Config::parse(toml_str).unwrap()
    }

    fn require_signatures_policy() -> crate::config::node::TrustPolicySection {
        crate::config::node::TrustPolicySection {
            require_signatures: true,
            keys: vec![],
        }
    }

    #[tokio::test]
    async fn single_node_deploy_ignores_the_trust_policy() {
        // require_signatures is on, but there's no council to consult, so a
        // single-node deploy is not gated — instances still come up.
        let (mut agent, tx, shutdown) = test_agent();
        agent.set_trust_policy(require_signatures_policy());
        let handle = tokio::spawn(async move {
            agent.run().await;
        });

        let events = send_deploy(&tx, basic_config()).await;
        let (created, _) = expect_complete(&events);
        assert_eq!(created, 1);

        shutdown.cancel();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn enforce_image_signature_allows_when_no_council() {
        let (mut agent, _tx, _shutdown) = test_agent();
        agent.set_trust_policy(require_signatures_policy());
        let spec: AppSpec = toml::from_str(r#"image = "myapp:v1""#).unwrap();
        // No cluster/council → the gate can't (and shouldn't) enforce.
        assert!(agent.enforce_image_signature(&spec).await.is_ok());
    }

    #[tokio::test]
    async fn shutdown_escalates_to_kill_when_stop_is_ignored() {
        let (_tx, rx) = mpsc::channel(8);
        let shutdown = CancellationToken::new();
        let grill = MockGrill::new();
        let grill_handle = grill.clone();
        let port_allocator = PortAllocator::new(30000, 31000);
        let mut agent = BunAgent::new(grill, port_allocator, rx, shutdown);

        let (ev_tx, mut ev_rx) = mpsc::channel(64);
        agent.deploy(basic_config(), &ev_tx).await;
        drop(ev_tx);
        while ev_rx.recv().await.is_some() {}

        // Pin the instance to Running so stop() is effectively ignored (the
        // process refuses SIGTERM). shutdown_all must escalate to SIGKILL.
        let id = InstanceId("web-0".to_string());
        grill_handle.set_state(&id, ContainerState::Running);

        agent.shutdown_all().await;

        let calls = grill_handle.calls();
        assert!(
            calls.iter().any(|(op, i)| op == "stop" && i.0 == "web-0"),
            "shutdown should SIGTERM first"
        );
        assert!(
            calls.iter().any(|(op, i)| op == "kill" && i.0 == "web-0"),
            "shutdown should escalate to SIGKILL when the process ignores stop"
        );
    }

    #[tokio::test]
    async fn deploy_command_creates_instances() {
        let (mut agent, tx, shutdown) = test_agent();

        let agent_handle = tokio::spawn(async move {
            agent.run().await;
        });

        let events = send_deploy(&tx, basic_config()).await;
        let (created, instances) = expect_complete(&events);
        assert_eq!(created, 1);
        assert_eq!(instances, &["web-0"]);

        shutdown.cancel();
        agent_handle.await.unwrap();
    }

    #[test]
    fn probe_host_prefers_container_ip() {
        assert_eq!(probe_host(None), "127.0.0.1");
        assert_eq!(
            probe_host(Some(std::net::Ipv4Addr::new(10, 0, 2, 2))),
            "10.0.2.2"
        );
    }

    #[tokio::test]
    async fn container_logs_forwarded_to_log_sink() {
        // Use a real ProcessGrill so follow_logs actually streams output.
        let (tx, rx) = mpsc::channel(32);
        let shutdown = CancellationToken::new();
        let grill = crate::grill::process::ProcessGrill::new();
        let port_allocator = PortAllocator::new(30000, 31000);
        let mut agent = BunAgent::new(grill, port_allocator, rx, shutdown.clone());

        let (log_tx, mut log_rx) = mpsc::channel(64);
        agent.set_log_sink(log_tx);
        let handle = tokio::spawn(async move { agent.run().await });

        let config = Config::parse(
            "[app.printer]\nimage = \"proc-grill:ignored\"\ncommand = [\"echo\", \"hello-logs\"]\n",
        )
        .unwrap();
        let _ = send_deploy(&tx, config).await;

        // The per-instance forwarder should stream the echoed line into the sink.
        let record = tokio::time::timeout(std::time::Duration::from_secs(5), log_rx.recv())
            .await
            .expect("timed out waiting for a forwarded log record")
            .expect("log channel closed");
        assert_eq!(record.app, "printer");
        assert_eq!(record.line, "hello-logs");

        shutdown.cancel();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn crashed_app_without_health_check_is_restarted() {
        let (tx, rx) = mpsc::channel(32);
        let shutdown = CancellationToken::new();
        let grill = crate::grill::process::ProcessGrill::new();
        let port_allocator = PortAllocator::new(30000, 31000);
        let mut agent = BunAgent::new(grill, port_allocator, rx, shutdown.clone());
        let handle = tokio::spawn(async move { agent.run().await });

        // An app with no health check whose process exits immediately. Nothing
        // probes it, so only crash detection can notice and restart it.
        let config =
            Config::parse("[app.crasher]\nimage = \"proc-grill:ignored\"\ncommand = [\"true\"]\n")
                .unwrap();
        let _ = send_deploy(&tx, config).await;

        // Give the health tick (1s) a few cycles to detect the exit and restart.
        tokio::time::sleep(std::time::Duration::from_secs(4)).await;

        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        tx.send(AgentCommand::Status { response: resp_tx })
            .await
            .unwrap();
        let status = resp_rx.await.unwrap();
        let crasher = status
            .iter()
            .find(|s| s.app_name == "crasher")
            .expect("crasher not found");
        assert!(
            crasher.restart_count > 0,
            "crashed app was never restarted (state: {})",
            crasher.state
        );

        shutdown.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(3), handle).await;
    }

    #[tokio::test]
    async fn redeploy_registers_backends_health_and_port() {
        let (_tx, rx) = mpsc::channel(32);
        let shutdown = CancellationToken::new();
        let grill = crate::grill::process::ProcessGrill::new();
        let port_allocator = PortAllocator::new(30000, 31000);
        let mut agent = BunAgent::new(grill, port_allocator, rx, shutdown);

        let config = Config::parse(
            "[app.web]\nimage = \"proc-grill:ignored\"\ncommand = [\"sleep\", \"60\"]\nport = 8080\n\n[app.web.health]\npath = \"/healthz\"\n",
        )
        .unwrap();
        let (ev_tx, mut ev_rx) = mpsc::channel(256);

        // Fresh deploy, then redeploy (existing instances → rolling path).
        agent.deploy(config.clone(), &ev_tx).await;
        agent.deploy(config, &ev_tx).await;
        drop(ev_tx);
        while ev_rx.recv().await.is_some() {}

        // The service must have backends after the redeploy (was left empty).
        let entry = agent
            .service_map
            .resolve("web")
            .expect("web missing from service map");
        assert!(
            !entry.backends.is_empty(),
            "redeploy left the service with zero backends"
        );

        // A redeployed instance keeps its port and health-check registration.
        let inst = agent
            .supervisor
            .list_instances()
            .into_iter()
            .find(|i| i.app_name == "web")
            .expect("no web instance after redeploy");
        assert!(inst.host_port.is_some(), "redeploy dropped the host port");
        assert!(
            inst.health_config.is_some(),
            "redeploy dropped the health check"
        );
    }

    #[tokio::test]
    async fn follow_logs_does_not_block_the_event_loop() {
        let (tx, rx) = mpsc::channel(32);
        let shutdown = CancellationToken::new();
        let grill = crate::grill::process::ProcessGrill::new();
        let port_allocator = PortAllocator::new(30000, 31000);
        let mut agent = BunAgent::new(grill, port_allocator, rx, shutdown.clone());
        let handle = tokio::spawn(async move { agent.run().await });

        // A long-running process whose follow would block the loop for 60s if
        // handled inline.
        let config = Config::parse(
            "[app.sleeper]\nimage = \"proc-grill:ignored\"\ncommand = [\"sleep\", \"60\"]\n",
        )
        .unwrap();
        let _ = send_deploy(&tx, config).await;

        // Start following logs; never drain them.
        let (line_tx, _line_rx) = mpsc::channel(16);
        tx.send(AgentCommand::FollowLogs {
            app_name: "sleeper".into(),
            namespace: "default".into(),
            tail: None,
            lines: line_tx,
        })
        .await
        .unwrap();

        // A subsequent command must still be answered promptly.
        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        tx.send(AgentCommand::Status { response: resp_tx })
            .await
            .unwrap();
        let status = tokio::time::timeout(std::time::Duration::from_secs(3), resp_rx)
            .await
            .expect("event loop blocked by FollowLogs")
            .unwrap();
        assert!(!status.is_empty(), "sleeper should be listed");

        shutdown.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(3), handle).await;
    }

    #[tokio::test]
    async fn deploy_fails_closed_on_encrypted_secret_without_key() {
        // Single-node agent has no cluster security state, so it cannot decrypt
        // ENC[AGE:...] secrets. It must refuse to start the workload rather than
        // pass ciphertext into the container environment.
        let (mut agent, tx, shutdown) = test_agent();
        let agent_handle = tokio::spawn(async move {
            agent.run().await;
        });

        let config = Config::parse(
            r#"
            [app.web]
            image = "myapp:v1"
            [app.web.env]
            SECRET = "ENC[AGE:abc123]"
        "#,
        )
        .unwrap();
        let events = send_deploy(&tx, config).await;

        match events.last().expect("no events received") {
            ApplyEvent::Error { message } => {
                assert!(
                    message.contains("encrypted secrets"),
                    "unexpected error: {message}"
                );
            }
            other => panic!("expected fail-closed Error, got {other:?}"),
        }

        shutdown.cancel();
        agent_handle.await.unwrap();
    }

    #[tokio::test]
    async fn deploy_streams_progress_events() {
        let (mut agent, tx, shutdown) = test_agent();

        let agent_handle = tokio::spawn(async move {
            agent.run().await;
        });

        let events = send_deploy(&tx, basic_config()).await;

        // Should have progress events before the final Complete
        let progress_count = events
            .iter()
            .filter(|e| matches!(e, ApplyEvent::Progress { .. }))
            .count();
        assert!(progress_count >= 1, "expected progress events");

        let instance_created = events
            .iter()
            .any(|e| matches!(e, ApplyEvent::InstanceCreated { id, .. } if id == "web-0"));
        assert!(instance_created, "expected InstanceCreated for web-0");

        expect_complete(&events);

        shutdown.cancel();
        agent_handle.await.unwrap();
    }

    #[tokio::test]
    async fn status_returns_all_instances() {
        let (mut agent, tx, shutdown) = test_agent();

        let agent_handle = tokio::spawn(async move {
            agent.run().await;
        });

        // Deploy first
        let events = send_deploy(&tx, basic_config()).await;
        expect_complete(&events);

        // Then get status
        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(AgentCommand::Status { response: resp_tx })
            .await
            .unwrap();

        let statuses = resp_rx.await.unwrap();
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].app_name, "web");
        // Without health checks, goes straight to Running
        assert_eq!(statuses[0].state, "running");

        shutdown.cancel();
        agent_handle.await.unwrap();
    }

    #[tokio::test]
    async fn stop_command_stops_instances() {
        let (mut agent, tx, shutdown) = test_agent();

        let agent_handle = tokio::spawn(async move {
            agent.run().await;
        });

        // Deploy
        let events = send_deploy(&tx, basic_config()).await;
        expect_complete(&events);

        // Stop
        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(AgentCommand::Stop {
            app_name: "web".to_string(),
            namespace: "default".to_string(),
            response: resp_tx,
        })
        .await
        .unwrap();
        resp_rx.await.unwrap().unwrap();

        // Verify stopped
        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(AgentCommand::Status { response: resp_tx })
            .await
            .unwrap();
        let statuses = resp_rx.await.unwrap();
        assert_eq!(statuses[0].state, "stopped");

        shutdown.cancel();
        agent_handle.await.unwrap();
    }

    #[tokio::test]
    async fn deploy_with_health_check_starts_in_health_wait() {
        let (mut agent, tx, shutdown) = test_agent();

        let agent_handle = tokio::spawn(async move {
            agent.run().await;
        });

        let events = send_deploy(&tx, config_with_health()).await;
        expect_complete(&events);

        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(AgentCommand::Status { response: resp_tx })
            .await
            .unwrap();
        let statuses = resp_rx.await.unwrap();
        // The instance should be in health-wait (awaiting first health check)
        // or running (if the mock health check resolved before we queried status).
        // Both are correct — it's a race between the status query and the
        // health check timer.
        let state = &statuses[0].state;
        assert!(
            state == "health-wait" || state == "running",
            "expected health-wait or running, got {state}"
        );

        shutdown.cancel();
        agent_handle.await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_stops_all_instances() {
        let (mut agent, tx, shutdown) = test_agent();

        let agent_handle = tokio::spawn(async move {
            agent.run().await;
        });

        let events = send_deploy(&tx, basic_config()).await;
        expect_complete(&events);

        shutdown.cancel();
        agent_handle.await.unwrap();
        // Agent ran shutdown_all — grill.stop() was called
    }

    #[tokio::test]
    async fn logs_returns_result_for_deployed_app() {
        let (mut agent, tx, shutdown) = test_agent();

        let agent_handle = tokio::spawn(async move {
            agent.run().await;
        });

        let events = send_deploy(&tx, basic_config()).await;
        expect_complete(&events);

        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(AgentCommand::Logs {
            app_name: "web".to_string(),
            namespace: "default".to_string(),
            tail: None,
            response: resp_tx,
        })
        .await
        .unwrap();
        let result = resp_rx.await.unwrap();
        // MockGrill returns empty logs, but the call should succeed
        assert!(result.is_ok());

        shutdown.cancel();
        agent_handle.await.unwrap();
    }

    #[tokio::test]
    async fn logs_for_unknown_app_errors() {
        let (mut agent, tx, shutdown) = test_agent();

        let agent_handle = tokio::spawn(async move {
            agent.run().await;
        });

        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(AgentCommand::Logs {
            app_name: "nope".to_string(),
            namespace: "default".to_string(),
            tail: None,
            response: resp_tx,
        })
        .await
        .unwrap();
        let result = resp_rx.await.unwrap();
        assert!(result.is_err());

        shutdown.cancel();
        agent_handle.await.unwrap();
    }

    #[tokio::test]
    async fn stop_unknown_app_errors() {
        let (mut agent, tx, shutdown) = test_agent();

        let agent_handle = tokio::spawn(async move {
            agent.run().await;
        });

        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(AgentCommand::Stop {
            app_name: "nope".to_string(),
            namespace: "default".to_string(),
            response: resp_tx,
        })
        .await
        .unwrap();
        let result = resp_rx.await.unwrap();
        assert!(result.is_err());

        shutdown.cancel();
        agent_handle.await.unwrap();
    }

    fn job_config() -> Config {
        let toml_str = r#"
            [job.migrate]
            image = "myapp:v1"
            command = ["echo", "done"]
        "#;
        Config::parse(toml_str).unwrap()
    }

    fn mixed_config() -> Config {
        let toml_str = r#"
            [app.web]
            image = "myapp:v1"
            port = 8080

            [job.migrate]
            image = "myapp:v1"
            command = ["echo", "done"]
        "#;
        Config::parse(toml_str).unwrap()
    }

    #[tokio::test]
    async fn deploy_job_creates_instance() {
        let (mut agent, tx, shutdown) = test_agent();

        let agent_handle = tokio::spawn(async move {
            agent.run().await;
        });

        let events = send_deploy(&tx, job_config()).await;
        let (created, instances) = expect_complete(&events);
        assert_eq!(created, 1);
        assert_eq!(instances, &["migrate-0"]);

        shutdown.cancel();
        agent_handle.await.unwrap();
    }

    #[tokio::test]
    async fn deploy_job_starts_in_running() {
        let (mut agent, tx, shutdown) = test_agent();

        let agent_handle = tokio::spawn(async move {
            agent.run().await;
        });

        let events = send_deploy(&tx, job_config()).await;
        expect_complete(&events);

        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(AgentCommand::Status { response: resp_tx })
            .await
            .unwrap();

        let statuses = resp_rx.await.unwrap();
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].app_name, "migrate");
        assert_eq!(statuses[0].state, "running");

        shutdown.cancel();
        agent_handle.await.unwrap();
    }

    #[tokio::test]
    async fn deploy_mixed_apps_and_jobs() {
        let (mut agent, tx, shutdown) = test_agent();

        let agent_handle = tokio::spawn(async move {
            agent.run().await;
        });

        let events = send_deploy(&tx, mixed_config()).await;
        let (created, _instances) = expect_complete(&events);
        assert_eq!(created, 2);

        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(AgentCommand::Status { response: resp_tx })
            .await
            .unwrap();

        let statuses = resp_rx.await.unwrap();
        assert_eq!(statuses.len(), 2);

        shutdown.cancel();
        agent_handle.await.unwrap();
    }

    fn config_with_init_container() -> Config {
        let toml_str = r#"
            [app.web]
            image = "myapp:v1"
            port = 8080

            [[app.web.init]]
            command = ["echo", "init"]
        "#;
        Config::parse(toml_str).unwrap()
    }

    #[tokio::test]
    async fn deploy_with_init_container_succeeds() {
        let (mut agent, tx, shutdown, grill) = test_agent_with_grill();

        // Pre-configure: init container exits successfully
        let init_id = InstanceId("web-0-init-0".to_string());
        grill.set_state(&init_id, ContainerState::Stopped);
        grill.set_exit_code(&init_id, Some(0));

        let agent_handle = tokio::spawn(async move {
            agent.run().await;
        });

        let events = send_deploy(&tx, config_with_init_container()).await;
        let (created, _instances) = expect_complete(&events);
        assert_eq!(created, 1);

        // App should reach running after successful init
        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(AgentCommand::Status { response: resp_tx })
            .await
            .unwrap();
        let statuses = resp_rx.await.unwrap();
        assert_eq!(statuses[0].state, "running");

        shutdown.cancel();
        agent_handle.await.unwrap();
    }

    #[tokio::test]
    async fn deploy_with_failing_init_container_fails() {
        let (mut agent, tx, shutdown, grill) = test_agent_with_grill();

        // Pre-configure: init container exits with failure
        let init_id = InstanceId("web-0-init-0".to_string());
        grill.set_state(&init_id, ContainerState::Stopped);
        grill.set_exit_code(&init_id, Some(1));

        let agent_handle = tokio::spawn(async move {
            agent.run().await;
        });

        let events = send_deploy(&tx, config_with_init_container()).await;
        let last = events.last().expect("no events");
        assert!(
            matches!(last, ApplyEvent::Error { .. }),
            "expected Error event, got {last:?}"
        );

        shutdown.cancel();
        agent_handle.await.unwrap();
    }

    #[test]
    fn tail_lines_empty_string() {
        assert_eq!(super::tail_lines("", 5), "");
    }

    #[test]
    fn tail_lines_fewer_than_n() {
        assert_eq!(super::tail_lines("a\nb\n", 5), "a\nb\n");
    }

    #[test]
    fn tail_lines_exactly_n() {
        assert_eq!(super::tail_lines("a\nb\nc\n", 3), "a\nb\nc\n");
    }

    #[test]
    fn tail_lines_more_than_n() {
        assert_eq!(super::tail_lines("a\nb\nc\nd\n", 2), "c\nd\n");
    }

    #[test]
    fn tail_lines_zero_returns_empty() {
        assert_eq!(super::tail_lines("a\nb\nc\n", 0), "");
    }

    #[test]
    fn tail_lines_no_trailing_newline() {
        assert_eq!(super::tail_lines("a\nb\nc", 2), "b\nc");
    }

    #[test]
    fn node_status_serialisation_round_trip() {
        let status = NodeStatus {
            node_id: "node-1".to_string(),
            address: "192.168.1.1:9116".to_string(),
            state: "alive".to_string(),
            incarnation: 42,
            is_council: true,
            is_leader: false,
            labels: BTreeMap::from([("zone".to_string(), "us-east-1a".to_string())]),
        };
        let json = serde_json::to_string(&status).unwrap();
        let decoded: NodeStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.node_id, "node-1");
        assert_eq!(decoded.incarnation, 42);
        assert!(decoded.is_council);
    }

    #[test]
    fn council_status_serialisation_round_trip() {
        let status = CouncilStatus {
            members: vec![CouncilMemberInfo {
                raft_id: 1,
                name: "node-1".to_string(),
                address: "192.168.1.1:9200".to_string(),
            }],
            leader: Some("node-1".to_string()),
            term: 5,
            last_applied_log: Some(42),
            app_count: 3,
        };
        let json = serde_json::to_string(&status).unwrap();
        let decoded: CouncilStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.term, 5);
        assert_eq!(decoded.leader, Some("node-1".to_string()));
        assert_eq!(decoded.members.len(), 1);
    }

    #[tokio::test]
    async fn logs_with_tail_truncates_output() {
        let (mut agent, tx, shutdown) = test_agent();

        let agent_handle = tokio::spawn(async move {
            agent.run().await;
        });

        let events = send_deploy(&tx, basic_config()).await;
        expect_complete(&events);

        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(AgentCommand::Logs {
            app_name: "web".to_string(),
            namespace: "default".to_string(),
            tail: Some(1),
            response: resp_tx,
        })
        .await
        .unwrap();
        let result = resp_rx.await.unwrap();
        // MockGrill returns empty logs, so tail of empty is still ok
        assert!(result.is_ok());

        shutdown.cancel();
        agent_handle.await.unwrap();
    }

    // ---- workload adoption (Phase 14) ----

    fn adoption_record(
        instance: &str,
        app: &str,
        with_health: bool,
    ) -> crate::grill::records::InstanceRecord {
        let spec_toml = if with_health {
            "image = \"myapp:v1\"\nport = 8080\n[health]\npath = \"/health\"\n"
        } else {
            "image = \"myapp:v1\"\n"
        };
        let app_spec: AppSpec = toml::from_str(spec_toml).unwrap();
        crate::grill::records::InstanceRecord {
            schema: 1,
            instance_id: instance.to_string(),
            namespace: "default".to_string(),
            app_name: app.to_string(),
            replica_index: 0,
            is_job: false,
            image: "myapp:v1".to_string(),
            runtime: crate::grill::records::RuntimeKind::Process,
            pid: 4242,
            pid_started_at: 1000,
            runc_container_id: None,
            log_stem: None,
            host_port: Some(30123),
            app_spec: Some(app_spec),
            oci_spec: crate::grill::oci::OciSpec {
                root: crate::grill::oci::OciRoot {
                    path: "/tmp/test".to_string(),
                    readonly: false,
                },
                process: crate::grill::oci::OciProcess {
                    args: vec!["sleep".to_string(), "60".to_string()],
                    env: vec![],
                    cwd: "/".to_string(),
                    user: crate::grill::oci::OciUser { uid: 0, gid: 0 },
                },
                mounts: vec![],
                linux: crate::grill::oci::OciLinux {
                    namespaces: vec![],
                    resources: None,
                    cgroups_path: None,
                    uid_mappings: None,
                    gid_mappings: None,
                },
            },
        }
    }

    #[tokio::test]
    async fn startup_adopts_recorded_instances_instead_of_restarting() {
        let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
        let dir = tempfile::tempdir().unwrap();
        let record = adoption_record("web-0", "web", false);
        crate::grill::records::write_record(dir.path(), &record).unwrap();
        agent.set_records_dir(dir.path().to_path_buf());

        let id = InstanceId("web-0".to_string());
        grill.set_adopt_result(&id, true);

        assert_eq!(agent.adopt_recorded_instances().await, 1);

        let instance = agent.supervisor.get_instance(&id).unwrap();
        assert_eq!(instance.state, ContainerState::Running);
        assert_eq!(instance.app_name, "web");
        // Adopted, never created or started by this process.
        let calls = grill.calls();
        assert!(calls.contains(&("adopt".to_string(), id.clone())));
        assert!(!calls.contains(&("create".to_string(), id.clone())));
        assert!(!calls.contains(&("start".to_string(), id)));
    }

    #[tokio::test]
    async fn startup_deletes_stale_records_and_reschedules() {
        let (mut agent, _tx, _shutdown, _grill) = test_agent_with_grill();
        let dir = tempfile::tempdir().unwrap();
        // MockGrill declines adoption by default (dead process).
        let record = adoption_record("web-0", "web", false);
        crate::grill::records::write_record(dir.path(), &record).unwrap();
        agent.set_records_dir(dir.path().to_path_buf());

        assert_eq!(agent.adopt_recorded_instances().await, 0);

        // The stale record is gone and nothing was seeded: the normal
        // reconcile path is free to reschedule the instance.
        assert!(crate::grill::records::load_records(dir.path()).is_empty());
        assert!(
            agent
                .supervisor
                .get_instance(&InstanceId("web-0".to_string()))
                .is_none()
        );
    }

    #[tokio::test]
    async fn adopted_instances_resume_health_checks() {
        let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
        let dir = tempfile::tempdir().unwrap();
        let record = adoption_record("web-0", "web", true);
        crate::grill::records::write_record(dir.path(), &record).unwrap();
        agent.set_records_dir(dir.path().to_path_buf());

        let id = InstanceId("web-0".to_string());
        grill.set_adopt_result(&id, true);
        assert_eq!(agent.adopt_recorded_instances().await, 1);

        let instance = agent.supervisor.get_instance(&id).unwrap();
        assert!(instance.health_config.is_some());
    }

    #[tokio::test]
    async fn adopted_instance_port_is_reserved() {
        let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
        let dir = tempfile::tempdir().unwrap();
        let record = adoption_record("web-0", "web", false);
        crate::grill::records::write_record(dir.path(), &record).unwrap();
        agent.set_records_dir(dir.path().to_path_buf());

        let id = InstanceId("web-0".to_string());
        grill.set_adopt_result(&id, true);
        assert_eq!(agent.adopt_recorded_instances().await, 1);

        // The adopted instance's port must not be handed out again.
        assert!(agent.supervisor.port_allocator.is_allocated(30123).await);
    }

    #[tokio::test]
    async fn adoption_never_clobbers_a_tracked_instance() {
        let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
        let dir = tempfile::tempdir().unwrap();
        agent.set_records_dir(dir.path().to_path_buf());

        // Deploy an instance in THIS process, then drop a record for the
        // same id (as if left behind) — adoption must skip it.
        let (ev_tx, mut ev_rx) = mpsc::channel(64);
        agent.deploy(basic_config(), &ev_tx).await;
        drop(ev_tx);
        while ev_rx.recv().await.is_some() {}

        let id = InstanceId("web-0".to_string());
        assert!(agent.supervisor.get_instance(&id).is_some());
        let record = adoption_record("web-0", "web", false);
        crate::grill::records::write_record(dir.path(), &record).unwrap();
        grill.set_adopt_result(&id, true);

        assert_eq!(agent.adopt_recorded_instances().await, 0);
    }
}
