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

/// Deadline for an `exec` run off the command loop (H3). Bounds an orphaned
/// task if the caller disconnects; the exec no longer blocks the loop, so this
/// is generous — it only stops a truly runaway command from lingering forever.
const EXEC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Maximum time an init container may run before the deploy fails. Bounds the
/// init wait so a hung init can't wedge the agent event loop indefinitely.
const INIT_TIMEOUT_SECS: u64 = 300;

/// Maximum time a `run_before` prerequisite job may run before the gated
/// deploy is aborted. Migrations are the classic case; a hung one must not
/// wedge the deploy forever.
const RUN_BEFORE_TIMEOUT_SECS: u64 = 600;

/// A job registered to run on a cron schedule rather than at deploy time.
///
/// `last_fired_minute` is the epoch-minute stamp of the most recent firing. The
/// cron tick runs every second but a schedule matches to minute resolution, so
/// we only fire when the stamp changes — otherwise a `* * * * *` job would fire
/// sixty times a minute.
struct ScheduledJob {
    name: String,
    namespace: String,
    schedule: crate::meat::cron::CronSchedule,
    spec: JobSpec,
    last_fired_minute: Option<i64>,
}

/// How many event-loop ticks (~1s each) between attempts to provision an
/// identity for a running instance that has none — frequent enough to heal
/// promptly, infrequent enough not to hammer an unreachable council.
const IDENTITY_RETRY_TICKS: u32 = 30;

/// Grace period between SIGTERM and SIGKILL during shutdown.
const SHUTDOWN_GRACE_SECS: u64 = 5;

/// How long an ordinary stop waits for a container to exit after SIGTERM
/// before it escalates to SIGKILL (DEP6).
const STOP_GRACE_SECS: u64 = 10;

/// A trace starts processes inside a workload and may remain in flight for two
/// eight-second probe bounds. Refuse excess work instead of building an
/// unbounded queue of authenticated diagnostic tasks.
const MAX_CONCURRENT_TRACES: usize = 8;

/// Build a shared drain tracker for a new agent. The completion channel's
/// receiver is dropped because the retire path polls `wait_drained` rather
/// than consuming the notification stream.
fn new_shared_drains() -> crate::wrapper::draining::SharedDrains {
    let (complete_tx, _complete_rx) = mpsc::channel(64);
    crate::wrapper::draining::SharedDrains::new(crate::wrapper::draining::DrainTracker::new(
        complete_tx,
    ))
}

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
    /// The agent accepted the deploy and assigned its queryable operation ID.
    Accepted { operation_id: String },
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
    /// Get the local desired application specs for standalone diagnostics.
    DesiredApps {
        response: oneshot::Sender<Vec<crate::bun::diagnostics::DesiredAppEvidence>>,
    },
    /// Get status of run-to-completion workload instances.
    JobStatus {
        response: oneshot::Sender<Vec<JobStatus>>,
    },
    /// Get the image references of all current instances (for GC
    /// protection: actively deployed images must not be collected).
    ActiveImages {
        response: oneshot::Sender<std::collections::HashSet<String>>,
    },
    /// Snapshot active and recent real deploy operations.
    DeployOperations {
        response: oneshot::Sender<crate::bun::deploy_operations::DeployOperationSnapshot>,
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
    /// Run the fixed Phase 15 connectivity probe from a local workload.
    Trace {
        request: crate::onion::trace::TraceRequest,
        internal_destination: bool,
        source_node: String,
        response: oneshot::Sender<Result<crate::onion::trace::TraceResult, BunError>>,
    },
    /// Get cluster node membership from the gossip layer.
    Nodes {
        response: oneshot::Sender<Vec<NodeStatus>>,
    },
    /// Get council (Raft) status.
    Council {
        response: oneshot::Sender<CouncilStatus>,
    },
    /// Issue a node certificate for a joining node (issuer side).
    ///
    /// An existing cluster member receives this when a new node presents a
    /// join token. It validates the token against the replicated security
    /// state, consumes it via Raft, and returns the certificate bundle for
    /// the joiner to persist. `node_id` is supplied by the joiner.
    JoinIssue {
        token: String,
        node_id: String,
        /// DER PKCS#10 CSR the joiner generated (PKI4). The joiner keeps its
        /// private key; the issuer only signs this request.
        csr_der: Vec<u8>,
        response: oneshot::Sender<Result<crate::sesame::join::JoinBundle, BunError>>,
    },
    /// Inject a network partition (chaos testing).
    InjectPartition {
        peers: Vec<String>,
        duration_secs: u64,
        injected_by: String,
        response: oneshot::Sender<Result<(String, crate::smoker::types::FaultSummary), BunError>>,
    },
    /// Remove all network partitions (chaos testing).
    HealPartition {
        response: oneshot::Sender<Result<String, BunError>>,
    },
    /// Query active chaos state.
    ChaosStatus {
        response: oneshot::Sender<ChaosState>,
    },
    /// Snapshot an app's managed volumes (one volume, or all of them).
    SnapshotCreate {
        namespace: String,
        app_name: String,
        /// Container mount path to snapshot; `None` = every
        /// provisioned volume of the app.
        volume: Option<String>,
        name: Option<String>,
        response: oneshot::Sender<Result<Vec<crate::grill::snapshot::SnapshotMeta>, BunError>>,
    },
    /// List an app's snapshots, newest first.
    SnapshotList {
        namespace: String,
        app_name: String,
        response: oneshot::Sender<Result<Vec<crate::grill::snapshot::SnapshotMeta>, BunError>>,
    },
    /// Restore a snapshot over its live volume. Refused while the app
    /// has running instances.
    SnapshotRestore {
        namespace: String,
        app_name: String,
        name: String,
        response: oneshot::Sender<Result<(), BunError>>,
    },
    /// Delete a snapshot.
    SnapshotDelete {
        namespace: String,
        app_name: String,
        name: String,
        response: oneshot::Sender<Result<(), BunError>>,
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
    /// Install the latest cluster-wide endpoint catalogue (12b.4), replicated
    /// from the leader. The agent overlays it onto its local service map so
    /// DNS and ingress resolve services running on other nodes.
    SyncClusterCatalog {
        catalog: Box<crate::onion::catalog::EndpointCatalog>,
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
        /// Whether the authenticated API caller may reverse a workload fault.
        allow_workload_fault: bool,
        /// Whether the authenticated API caller may reverse node state.
        allow_node_fault: bool,
        /// Whether the authenticated API caller may remove node pressure.
        allow_node_pressure: bool,
        response: oneshot::Sender<Result<String, BunError>>,
    },
    /// Clear all active faults.
    ClearAllFaults {
        response: oneshot::Sender<Result<String, BunError>>,
    },
    /// Clear every active fault targeting a given service. `namespace`
    /// confines the clear to one tenant (`None` clears the service in every
    /// namespace, for legacy/admin callers).
    ClearFaultsByService {
        service: String,
        namespace: Option<String>,
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

/// The fast, `&mut self` steps a deploy needs the command loop to perform on
/// its behalf.
///
/// A deploy runs on its own spawned task so a slow image pull or a rolling
/// health wait can't wedge the command loop (DEP4/codex-M3). The task owns the
/// blocking grill I/O (create, start, init and health polling), but the
/// supervisor state machine stays authoritative on the loop: every state
/// transition and every mutation of supervisor/service-map/networking travels
/// back as one of these ops. Each carries a `oneshot` the loop replies on, so
/// the task drives the sequence while the loop applies it.
enum DeployOp {
    /// Enforce the image trust policy; returns the digest-pinned image, if any.
    EnforceImageSignature {
        spec: Box<AppSpec>,
        reply: oneshot::Sender<Result<Option<String>, String>>,
    },
    /// Record the deployed spec for the Brioche UI.
    StoreDeployedSpec {
        app_name: String,
        namespace: String,
        spec: Box<AppSpec>,
        reply: oneshot::Sender<()>,
    },
    /// Active (non-terminal) instance ids for an app in a namespace.
    ListExistingActive {
        app_name: String,
        namespace: String,
        reply: oneshot::Sender<Vec<InstanceId>>,
    },
    /// Reserve and return the next rolling-redeploy generation counter.
    NextDeployGen { reply: oneshot::Sender<u64> },
    /// Create supervisor-tracked instances for a fresh app deploy.
    SupervisorDeployApp {
        app_name: String,
        namespace: String,
        spec: Box<AppSpec>,
        reply: oneshot::Sender<Result<Vec<InstanceId>, BunError>>,
    },
    /// Create supervisor-tracked instances for a job deploy.
    SupervisorDeployJob {
        job_name: String,
        namespace: String,
        spec: Box<JobSpec>,
        reply: oneshot::Sender<Result<Vec<InstanceId>, BunError>>,
    },
    /// Register an app + firewall in the service map and sync its eBPF maps.
    RegisterServiceApp {
        app_name: String,
        namespace: String,
        port: u16,
        firewall: Option<Vec<String>>,
        reply: oneshot::Sender<()>,
    },
    /// Store an app's ingress config for the routing table.
    StoreIngress {
        app_name: String,
        namespace: String,
        ingress: Box<crate::config::app::IngressSpec>,
        reply: oneshot::Sender<()>,
    },
    /// Do the fast pre-create bookkeeping for a fresh instance: transition to
    /// Preparing, prepare its identity dir, its managed volumes, and build the
    /// OCI spec (fail closed on undecryptable secrets). The task then calls
    /// `grill.create` itself, off the loop.
    PrepareFreshInstance {
        instance_id: InstanceId,
        app_name: String,
        namespace: String,
        spec: Box<AppSpec>,
        reply: oneshot::Sender<Result<PreparedInstance, BunError>>,
    },
    /// Store the built OCI spec on the tracked instance (for restart re-drive).
    StoreOciSpec {
        instance_id: InstanceId,
        oci_spec: Box<crate::grill::oci::OciSpec>,
        reply: oneshot::Sender<()>,
    },
    /// Program egress before the workload runs (create → program → start). On
    /// failure the caller stops the created container and fails the deploy.
    ApplyEgressPreStart {
        instance_id: InstanceId,
        app_name: String,
        spec: Box<AppSpec>,
        cgroup_path: PathBuf,
        reply: oneshot::Sender<Result<(), BunError>>,
    },
    /// Transition an instance to a new lifecycle state through the supervisor.
    TransitionState {
        instance_id: InstanceId,
        to: ContainerState,
        reply: oneshot::Sender<Result<(), BunError>>,
    },
    /// Post-start bookkeeping for a fresh instance: log forwarder, on-disk
    /// record, container IP, HealthWait(→Running), service-map backend and
    /// kernel networking.
    FinishFreshInstance {
        instance_id: InstanceId,
        app_name: String,
        namespace: String,
        container_ip: Option<std::net::Ipv4Addr>,
        reply: oneshot::Sender<Result<(), BunError>>,
    },
    /// Provision a workload identity (SPIFFE cert + OIDC JWT).
    ProvisionIdentity {
        app_name: String,
        namespace: String,
        instance_id: InstanceId,
        is_job: bool,
        reply: oneshot::Sender<()>,
    },
    /// Fast pre-create bookkeeping for a rolling-redeploy instance: fail closed
    /// on undecryptable secrets, prepare its identity dir, build the OCI spec.
    PrepareRollingInstance {
        instance_id: InstanceId,
        app_name: String,
        namespace: String,
        spec: Box<AppSpec>,
        host_port: Option<u16>,
        index: u32,
        reply: oneshot::Sender<Result<crate::grill::oci::OciSpec, BunError>>,
    },
    /// Lift an instance's egress enforcement.
    ClearEgress {
        instance_id: InstanceId,
        reply: oneshot::Sender<()>,
    },
    /// Post-start bookkeeping for a healthy rolling instance: log forwarder,
    /// on-disk record, provision identity.
    RegisterRollingInstance {
        instance_id: InstanceId,
        app_name: String,
        namespace: String,
        reply: oneshot::Sender<()>,
    },
    /// Roll a failed rolling redeploy back: kill and clean up the new
    /// instances, release their ports, drop their identity dirs, record it.
    RollbackRollingDeploy {
        app_name: String,
        namespace: String,
        spec: Box<AppSpec>,
        new_ids: Vec<InstanceId>,
        new_prepared: Vec<InstanceId>,
        new_ports: std::collections::HashMap<InstanceId, Option<u16>>,
        replica_count: u32,
        reply: oneshot::Sender<()>,
    },
    /// Halt a failed rolling deploy without reverting (`auto_rollback = false`):
    /// keep the healthy new + surviving old instances, tear down only the
    /// incomplete one, record a `Halted` result.
    HaltRollingDeploy {
        app_name: String,
        namespace: String,
        spec: Box<AppSpec>,
        new_ids: Vec<InstanceId>,
        new_prepared: Vec<InstanceId>,
        new_ports: std::collections::HashMap<InstanceId, Option<u16>>,
        replica_count: u32,
        reply: oneshot::Sender<()>,
    },
    /// Retire the old instances and register the healthy new ones: service
    /// map, health config, backends, kernel networking, ingress, history.
    FinaliseRollingDeploy {
        app_name: String,
        namespace: String,
        spec: Box<AppSpec>,
        existing: Vec<InstanceId>,
        new_ids: Vec<InstanceId>,
        new_ports: std::collections::HashMap<InstanceId, Option<u16>>,
        new_ips: std::collections::HashMap<InstanceId, Option<std::net::Ipv4Addr>>,
        new_specs: std::collections::HashMap<InstanceId, crate::grill::oci::OciSpec>,
        now: Instant,
        reply: oneshot::Sender<()>,
    },
    /// Publish one freshly-healthy replacement as a routable backend (M7).
    ///
    /// Split out of `FinaliseRollingDeploy` so a rolling deploy can move
    /// traffic onto a replacement *before* retiring an old instance, which is
    /// what makes `max_unavailable = 0` mean anything.
    PublishNewBackend {
        app_name: String,
        namespace: String,
        new_id: InstanceId,
        host_port: Option<u16>,
        container_ip: Option<std::net::Ipv4Addr>,
        has_port: bool,
        reply: oneshot::Sender<()>,
    },
    /// Drain, stop and forget one old instance (M7).
    ///
    /// Also split out of `FinaliseRollingDeploy`, so retirement can be
    /// interleaved with replacement one instance at a time instead of
    /// happening all at once at the end.
    RetireOldInstance {
        old_id: InstanceId,
        drain_timeout: std::time::Duration,
        reply: oneshot::Sender<()>,
    },
    /// Append an entry to the deploy history.
    PushDeployHistory {
        entry: Box<crate::meat::deploy_types::DeployHistoryEntry>,
        reply: oneshot::Sender<()>,
    },
    /// Post-start bookkeeping for a job instance: log forwarder, on-disk
    /// record, transitions to Running.
    FinishJobInstance {
        instance_id: InstanceId,
        job_name: String,
        namespace: String,
        oci_spec: Box<crate::grill::oci::OciSpec>,
        reply: oneshot::Sender<Result<(), BunError>>,
    },
    /// Rebuild the Wrapper routing table after all instances started.
    RebuildRoutingTable { reply: oneshot::Sender<()> },
    /// Record a per-app "deployed" lifecycle event.
    RecordDeployedEvent {
        app_name: String,
        namespace: String,
        reply: oneshot::Sender<()>,
    },
}

/// The fast pre-create outputs the loop hands back for a fresh instance.
struct PreparedInstance {
    oci_spec: crate::grill::oci::OciSpec,
    cgroup_path: PathBuf,
    cgroup_str: String,
    has_init: bool,
}

/// A handle a deploy task uses to ask the command loop to perform its
/// authoritative `&mut self` steps. Each method sends a `DeployOp` and awaits
/// the reply, so the loop stays the single owner of supervisor state.
#[derive(Clone)]
struct DeployOps {
    tx: mpsc::Sender<DeployOp>,
}

impl DeployOps {
    /// Send an op built by `make` (given the reply sender) and await its
    /// reply, falling back to `on_gone` if the loop has shut down (the task is
    /// tearing down anyway, so the value is never observed).
    async fn call<T, F>(&self, make: F, on_gone: T) -> T
    where
        F: FnOnce(oneshot::Sender<T>) -> DeployOp,
    {
        let (reply, rx) = oneshot::channel();
        if self.tx.send(make(reply)).await.is_err() {
            return on_gone;
        }
        rx.await.unwrap_or(on_gone)
    }

    async fn enforce_image_signature(&self, spec: &AppSpec) -> Result<Option<String>, String> {
        self.call(
            |reply| DeployOp::EnforceImageSignature {
                spec: Box::new(spec.clone()),
                reply,
            },
            Err("agent shutting down".to_string()),
        )
        .await
    }

    async fn store_deployed_spec(&self, app_name: &str, namespace: &str, spec: &AppSpec) {
        self.call(
            |reply| DeployOp::StoreDeployedSpec {
                app_name: app_name.to_string(),
                namespace: namespace.to_string(),
                spec: Box::new(spec.clone()),
                reply,
            },
            (),
        )
        .await
    }

    async fn list_existing_active(&self, app_name: &str, namespace: &str) -> Vec<InstanceId> {
        self.call(
            |reply| DeployOp::ListExistingActive {
                app_name: app_name.to_string(),
                namespace: namespace.to_string(),
                reply,
            },
            Vec::new(),
        )
        .await
    }

    async fn next_deploy_gen(&self) -> u64 {
        self.call(|reply| DeployOp::NextDeployGen { reply }, 0)
            .await
    }

    async fn supervisor_deploy_app(
        &self,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
    ) -> Result<Vec<InstanceId>, BunError> {
        self.call(
            |reply| DeployOp::SupervisorDeployApp {
                app_name: app_name.to_string(),
                namespace: namespace.to_string(),
                spec: Box::new(spec.clone()),
                reply,
            },
            Ok(Vec::new()),
        )
        .await
    }

    async fn supervisor_deploy_job(
        &self,
        job_name: &str,
        namespace: &str,
        spec: &JobSpec,
    ) -> Result<Vec<InstanceId>, BunError> {
        self.call(
            |reply| DeployOp::SupervisorDeployJob {
                job_name: job_name.to_string(),
                namespace: namespace.to_string(),
                spec: Box::new(spec.clone()),
                reply,
            },
            Ok(Vec::new()),
        )
        .await
    }

    async fn register_service_app(
        &self,
        app_name: &str,
        namespace: &str,
        port: u16,
        firewall: Option<Vec<String>>,
    ) {
        self.call(
            |reply| DeployOp::RegisterServiceApp {
                app_name: app_name.to_string(),
                namespace: namespace.to_string(),
                port,
                firewall,
                reply,
            },
            (),
        )
        .await
    }

    async fn store_ingress(
        &self,
        app_name: &str,
        namespace: &str,
        ingress: &crate::config::app::IngressSpec,
    ) {
        self.call(
            |reply| DeployOp::StoreIngress {
                app_name: app_name.to_string(),
                namespace: namespace.to_string(),
                ingress: Box::new(ingress.clone()),
                reply,
            },
            (),
        )
        .await
    }

    async fn prepare_fresh_instance(
        &self,
        instance_id: &InstanceId,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
    ) -> Result<PreparedInstance, BunError> {
        self.call(
            |reply| DeployOp::PrepareFreshInstance {
                instance_id: instance_id.clone(),
                app_name: app_name.to_string(),
                namespace: namespace.to_string(),
                spec: Box::new(spec.clone()),
                reply,
            },
            Err(BunError::InstanceNotFound {
                instance_id: instance_id.clone(),
            }),
        )
        .await
    }

    async fn store_oci_spec(&self, instance_id: &InstanceId, oci_spec: crate::grill::oci::OciSpec) {
        self.call(
            |reply| DeployOp::StoreOciSpec {
                instance_id: instance_id.clone(),
                oci_spec: Box::new(oci_spec),
                reply,
            },
            (),
        )
        .await
    }

    async fn apply_egress_pre_start(
        &self,
        instance_id: &InstanceId,
        app_name: &str,
        spec: &AppSpec,
        cgroup_path: &std::path::Path,
    ) -> Result<(), BunError> {
        self.call(
            |reply| DeployOp::ApplyEgressPreStart {
                instance_id: instance_id.clone(),
                app_name: app_name.to_string(),
                spec: Box::new(spec.clone()),
                cgroup_path: cgroup_path.to_path_buf(),
                reply,
            },
            Ok(()),
        )
        .await
    }

    async fn transition_state(
        &self,
        instance_id: &InstanceId,
        to: ContainerState,
    ) -> Result<(), BunError> {
        self.call(
            |reply| DeployOp::TransitionState {
                instance_id: instance_id.clone(),
                to,
                reply,
            },
            Ok(()),
        )
        .await
    }

    async fn finish_fresh_instance(
        &self,
        instance_id: &InstanceId,
        app_name: &str,
        namespace: &str,
        container_ip: Option<std::net::Ipv4Addr>,
    ) -> Result<(), BunError> {
        self.call(
            |reply| DeployOp::FinishFreshInstance {
                instance_id: instance_id.clone(),
                app_name: app_name.to_string(),
                namespace: namespace.to_string(),
                container_ip,
                reply,
            },
            Ok(()),
        )
        .await
    }

    async fn provision_identity(
        &self,
        app_name: &str,
        namespace: &str,
        instance_id: &InstanceId,
        is_job: bool,
    ) {
        self.call(
            |reply| DeployOp::ProvisionIdentity {
                app_name: app_name.to_string(),
                namespace: namespace.to_string(),
                instance_id: instance_id.clone(),
                is_job,
                reply,
            },
            (),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn prepare_rolling_instance(
        &self,
        instance_id: &InstanceId,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
        host_port: Option<u16>,
        index: u32,
    ) -> Result<crate::grill::oci::OciSpec, BunError> {
        self.call(
            |reply| DeployOp::PrepareRollingInstance {
                instance_id: instance_id.clone(),
                app_name: app_name.to_string(),
                namespace: namespace.to_string(),
                spec: Box::new(spec.clone()),
                host_port,
                index,
                reply,
            },
            Err(BunError::InstanceNotFound {
                instance_id: instance_id.clone(),
            }),
        )
        .await
    }

    async fn clear_egress(&self, instance_id: &InstanceId) {
        self.call(
            |reply| DeployOp::ClearEgress {
                instance_id: instance_id.clone(),
                reply,
            },
            (),
        )
        .await
    }

    async fn register_rolling_instance(
        &self,
        instance_id: &InstanceId,
        app_name: &str,
        namespace: &str,
    ) {
        self.call(
            |reply| DeployOp::RegisterRollingInstance {
                instance_id: instance_id.clone(),
                app_name: app_name.to_string(),
                namespace: namespace.to_string(),
                reply,
            },
            (),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn rollback_rolling_deploy(
        &self,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
        new_ids: Vec<InstanceId>,
        new_prepared: Vec<InstanceId>,
        new_ports: std::collections::HashMap<InstanceId, Option<u16>>,
        replica_count: u32,
    ) {
        self.call(
            |reply| DeployOp::RollbackRollingDeploy {
                app_name: app_name.to_string(),
                namespace: namespace.to_string(),
                spec: Box::new(spec.clone()),
                new_ids,
                new_prepared,
                new_ports,
                replica_count,
                reply,
            },
            (),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn halt_rolling_deploy(
        &self,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
        new_ids: Vec<InstanceId>,
        new_prepared: Vec<InstanceId>,
        new_ports: std::collections::HashMap<InstanceId, Option<u16>>,
        replica_count: u32,
    ) {
        self.call(
            |reply| DeployOp::HaltRollingDeploy {
                app_name: app_name.to_string(),
                namespace: namespace.to_string(),
                spec: Box::new(spec.clone()),
                new_ids,
                new_prepared,
                new_ports,
                replica_count,
                reply,
            },
            (),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn finalise_rolling_deploy(
        &self,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
        existing: Vec<InstanceId>,
        new_ids: Vec<InstanceId>,
        new_ports: std::collections::HashMap<InstanceId, Option<u16>>,
        new_ips: std::collections::HashMap<InstanceId, Option<std::net::Ipv4Addr>>,
        new_specs: std::collections::HashMap<InstanceId, crate::grill::oci::OciSpec>,
        now: Instant,
    ) {
        self.call(
            |reply| DeployOp::FinaliseRollingDeploy {
                app_name: app_name.to_string(),
                namespace: namespace.to_string(),
                spec: Box::new(spec.clone()),
                existing,
                new_ids,
                new_ports,
                new_ips,
                new_specs,
                now,
                reply,
            },
            (),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn publish_new_backend(
        &self,
        app_name: &str,
        namespace: &str,
        new_id: &InstanceId,
        host_port: Option<u16>,
        container_ip: Option<std::net::Ipv4Addr>,
        has_port: bool,
    ) {
        self.call(
            |reply| DeployOp::PublishNewBackend {
                app_name: app_name.to_string(),
                namespace: namespace.to_string(),
                new_id: new_id.clone(),
                host_port,
                container_ip,
                has_port,
                reply,
            },
            (),
        )
        .await
    }

    async fn retire_old_instance(&self, old_id: &InstanceId, drain_timeout: std::time::Duration) {
        self.call(
            |reply| DeployOp::RetireOldInstance {
                old_id: old_id.clone(),
                drain_timeout,
                reply,
            },
            (),
        )
        .await
    }

    async fn push_deploy_history(&self, entry: crate::meat::deploy_types::DeployHistoryEntry) {
        self.call(
            |reply| DeployOp::PushDeployHistory {
                entry: Box::new(entry),
                reply,
            },
            (),
        )
        .await
    }

    async fn finish_job_instance(
        &self,
        instance_id: &InstanceId,
        job_name: &str,
        namespace: &str,
        oci_spec: crate::grill::oci::OciSpec,
    ) -> Result<(), BunError> {
        self.call(
            |reply| DeployOp::FinishJobInstance {
                instance_id: instance_id.clone(),
                job_name: job_name.to_string(),
                namespace: namespace.to_string(),
                oci_spec: Box::new(oci_spec),
                reply,
            },
            Ok(()),
        )
        .await
    }

    async fn rebuild_routing_table(&self) {
        self.call(|reply| DeployOp::RebuildRoutingTable { reply }, ())
            .await
    }

    async fn record_deployed_event(&self, app_name: &str, namespace: &str) {
        self.call(
            |reply| DeployOp::RecordDeployedEvent {
                app_name: app_name.to_string(),
                namespace: namespace.to_string(),
                reply,
            },
            (),
        )
        .await
    }
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
    /// Exit code of a stopped instance, when the runtime tracks it.
    /// `stopped` alone is ambiguous for jobs — a failing job passes
    /// through `stopped` between retries — so batch watchers (F1) need
    /// this to tell success from failure-in-backoff.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    /// OS process ID, if available.
    pub pid: Option<u32>,
}

/// Status of a run-to-completion job instance, as returned by `/v1/jobs`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobStatus {
    pub name: String,
    pub namespace: String,
    pub instance_id: String,
    pub image: String,
    pub state: String,
    pub restart_count: u32,
    pub age_seconds: u64,
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
    /// Shared CRL used by the internal mTLS verifiers. bun's security refresh
    /// ticker updates it as `RevokeCertificate` entries replicate, so a
    /// revoked peer is refused on its next handshake without a restart.
    pub crl_handle: crate::sesame::mtls::CrlHandle,
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
    /// Shared all-transport gate used by reversible node-failure faults.
    pub node_gate: crate::smoker::node_fault::NodeTransportGate,
}

/// Egress enforcement bound to a running instance's cgroup (L16).
#[cfg(all(feature = "ebpf", target_os = "linux"))]
#[derive(Clone)]
struct EgressBinding {
    /// The instance's cgroup id (key into the egress maps).
    cgroup_id: u64,
    /// The raw `[egress] allow` list, re-resolved periodically.
    allow: Vec<String>,
    /// The destinations currently programmed into the egress maps
    /// (exact v4/v6 and CIDR).
    resolved: Vec<crate::sesame::egress::EgressDestination>,
}

/// An immutable, owned connectivity trace that can run outside the agent
/// command loop. Workload probes have explicit timeouts, but even a bounded
/// probe must not delay status, shutdown or another control-plane command.
struct PreparedTrace<G> {
    _permit: tokio::sync::OwnedSemaphorePermit,
    shutdown: CancellationToken,
    grill: G,
    source_instance: InstanceId,
    request: crate::onion::trace::TraceRequest,
    internal_destination: bool,
    source_node: String,
    service: Option<crate::onion::types::ServiceEntry>,
    destination_port: u16,
    dns_name: String,
    expected_vip: Option<String>,
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    onion_ebpf: Option<std::sync::Arc<tokio::sync::Mutex<crate::onion::ebpf::loader::OnionEbpf>>>,
}

/// The Bun agent. Generic over `G: Grill` so tests can inject mocks.
pub struct BunAgent<G: Grill> {
    supervisor: WorkloadSupervisor<G>,
    command_rx: mpsc::Receiver<AgentCommand>,
    shutdown: CancellationToken,
    /// Process-wide long-lived-task evidence shared with the API and reporter.
    readiness: Option<crate::bun::readiness::ReadinessTracker>,
    /// Hard per-node concurrency bound for workload connectivity traces.
    trace_slots: std::sync::Arc<tokio::sync::Semaphore>,
    volumes_dir: PathBuf,
    cluster: Option<ClusterHandle>,
    /// Immutable cluster identity used as every workload SPIFFE trust domain.
    trust_domain: String,
    /// Smoker fault registry — active faults on this node.
    fault_registry: crate::smoker::registry::FaultRegistry,
    /// Smoker duration limits (`[smoker]`): default + maximum fault lifetime.
    smoker_config: crate::smoker::config::SmokerConfig,
    /// Reference-counted node drains, independent from binary-upgrade drains.
    node_drain_gate: crate::smoker::node_fault::NodeDrainGate,
    /// Owned helper processes and cgroups for node-scoped capacity pressure.
    node_pressure: crate::smoker::node_pressure::NodePressureController,
    /// eBPF program handle for writing fault maps (Linux + ebpf feature only).
    /// `None` on macOS or when eBPF is not loaded.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    onion_ebpf: Option<std::sync::Arc<tokio::sync::Mutex<crate::onion::ebpf::loader::OnionEbpf>>>,
    /// Egress enforcement state per instance with an allowlist: its cgroup
    /// id, the raw allow list, and the last-resolved destinations — so
    /// enforcement can be lifted on stop and the allowlist re-resolved as
    /// DNS changes (L16).
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    egress_bindings: std::collections::HashMap<InstanceId, EgressBinding>,
    /// Workloads fenced by the current live-enforcement incident. Kept after
    /// stop so the next capability report records what happened; cleared only
    /// after every required hook and pre-start guarantee recovers.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    egress_affected_workloads: std::collections::BTreeSet<(String, String)>,
    /// Ticks since the last egress re-resolution (the event loop runs at 1s).
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    egress_reresolve_ticks: u32,
    /// Kernel-truth sweep interval in seconds (`[ebpf] sweep_interval_secs`,
    /// 0 disables). The sweep deletes egress/namespace map entries whose
    /// cgroup no longer maps to a live instance and reinstalls entries a
    /// live instance lost.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    ebpf_sweep_interval_secs: u64,
    /// Ticks since the last kernel-truth sweep.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    ebpf_sweep_ticks: u64,
    /// `firewall_map` keys last written to the kernel, so the next reconcile
    /// deletes entries for departed cgroups (NET5). eBPF only.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    firewall_bpf_keys: std::collections::HashSet<crate::onion::types::FirewallKey>,
    /// `cgroup_namespace_map` keys (cgroup ids) last written, for the same
    /// reconcile-and-prune reason (NET5). eBPF only.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    cgroup_ns_bpf_keys: std::collections::HashSet<u64>,
    /// Onion service map: app names → VIPs + backends.
    service_map: crate::onion::service_map::ServiceMap,
    /// Cluster-wide endpoint catalogue (12b.4), replicated from the leader.
    /// Overlaid onto the local `service_map` when publishing the DNS/routing
    /// snapshot so this node resolves services whose backends live elsewhere.
    /// Empty on a single node — the local map is then the whole picture.
    cluster_catalog: crate::onion::catalog::EndpointCatalog,
    /// Publisher for service-map snapshots (DNS responder subscribes).
    service_map_tx: tokio::sync::watch::Sender<crate::onion::service_map::ServiceMap>,
    /// Publisher for the set of services under a Smoker `DnsNxdomain` fault.
    ///
    /// DNS lives in the userspace responder now (the in-kernel DNS eBPF
    /// object was never loaded), so the fault does too: we republish this on
    /// every apply/clear/expire and the responder returns NXDOMAIN for any
    /// service in the set. See [`crate::onion::dns::DnsFaultState`].
    dns_faults_tx: tokio::sync::watch::Sender<crate::onion::dns::DnsFaultState>,
    /// Wrapper routing table (shared with the proxy via Arc<RwLock>).
    routing_table: std::sync::Arc<tokio::sync::RwLock<crate::wrapper::routing::RoutingTable>>,
    /// Ingress configs for deployed apps (app_name → IngressSpec).
    /// Ingress specs keyed by `(namespace, app_name)` so same-named apps
    /// in different namespaces route independently (D3/codex-M1).
    ingress_configs: std::collections::HashMap<(String, String), crate::config::app::IngressSpec>,
    /// Perimeter firewall config. Disabled in rootless mode.
    perimeter_config: crate::firewall::rules::PerimeterConfig,
    /// Last applied cluster-node set for firewall reconciliation. `None`
    /// until the first apply, so a standalone node (empty set) still gets
    /// the firewall; comparing the set (not a count) catches node swaps (M18).
    last_firewall_nodes: Option<crate::firewall::rules::ClusterNodes>,
    /// Deploy history (shared with API for query access).
    pub(crate) deploy_history:
        Arc<tokio::sync::RwLock<Vec<crate::meat::deploy_types::DeployHistoryEntry>>>,
    /// Real apply-worker activity and bounded terminal outcomes for the API.
    deploy_operations: crate::bun::deploy_operations::DeployOperationTracker,
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
    /// Jobs carrying a `schedule`, registered on apply and fired by the cron
    /// tick. Keyed by (name, namespace) so a re-apply replaces the entry.
    scheduled_jobs: std::collections::HashMap<(String, String), ScheduledJob>,
    /// Sink for container log lines. When set, each started instance spawns a
    /// forwarder that streams its output here (drained into the LogStore).
    log_tx: Option<mpsc::Sender<crate::ketchup::types::LogRecord>>,
    /// Bounded lifecycle event history shared with the API.
    events: Option<Arc<tokio::sync::RwLock<crate::bun::events::EventStore>>>,
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
    /// Ticks since the last attempt to provision identities for running
    /// instances that have none (see `IDENTITY_RETRY_TICKS`).
    identity_retry_ticks: u32,
    /// Sender cloned into each spawned deploy task so it can ask the loop to
    /// perform its authoritative `&mut self` steps (DEP4/codex-M3).
    deploy_ops_tx: mpsc::Sender<DeployOp>,
    /// Receiver the command loop drains to apply those deploy ops. Paired with
    /// `deploy_ops_tx`; kept here so `run` can `select!` on it.
    deploy_ops_rx: mpsc::Receiver<DeployOp>,
    /// Shared drain tracker (DEP5). Handed to the Wrapper proxy so in-flight
    /// requests to a retiring backend are counted; the retire path starts a
    /// drain and waits for it to finish (or time out) before killing the
    /// old container.
    drains: crate::wrapper::draining::SharedDrains,
}

impl<G: Grill + Clone + 'static> BunAgent<G> {
    async fn record_event(
        &self,
        kind: crate::bun::events::EventKind,
        severity: crate::bun::events::EventSeverity,
        app: Option<String>,
        namespace: Option<String>,
        message: String,
    ) {
        let Some(events) = &self.events else { return };
        let timestamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        events
            .write()
            .await
            .record(timestamp, kind, severity, app, namespace, None, message);
    }

    /// Create a new agent in single-node mode (no cluster).
    pub fn new(
        grill: G,
        port_allocator: PortAllocator,
        command_rx: mpsc::Receiver<AgentCommand>,
        shutdown: CancellationToken,
    ) -> Self {
        // Deploy tasks drive their authoritative steps back through this
        // channel; the loop drains it in `run` (DEP4/codex-M3).
        let (deploy_ops_tx, deploy_ops_rx) = mpsc::channel(256);
        Self {
            supervisor: WorkloadSupervisor::new(grill, port_allocator),
            command_rx,
            shutdown,
            readiness: None,
            trace_slots: std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_TRACES)),
            volumes_dir: crate::config::node::StorageSection::default().volumes,
            cluster: None,
            trust_domain: "default".to_string(),
            fault_registry: crate::smoker::registry::FaultRegistry::new(),
            smoker_config: crate::smoker::config::SmokerConfig::default(),
            node_drain_gate: crate::smoker::node_fault::NodeDrainGate::new(),
            node_pressure: crate::smoker::node_pressure::NodePressureController::default(),
            #[cfg(all(feature = "ebpf", target_os = "linux"))]
            onion_ebpf: None,
            #[cfg(all(feature = "ebpf", target_os = "linux"))]
            egress_bindings: std::collections::HashMap::new(),
            #[cfg(all(feature = "ebpf", target_os = "linux"))]
            egress_affected_workloads: std::collections::BTreeSet::new(),
            #[cfg(all(feature = "ebpf", target_os = "linux"))]
            egress_reresolve_ticks: 0,
            #[cfg(all(feature = "ebpf", target_os = "linux"))]
            ebpf_sweep_interval_secs: 60,
            #[cfg(all(feature = "ebpf", target_os = "linux"))]
            ebpf_sweep_ticks: 0,
            #[cfg(all(feature = "ebpf", target_os = "linux"))]
            firewall_bpf_keys: std::collections::HashSet::new(),
            #[cfg(all(feature = "ebpf", target_os = "linux"))]
            cgroup_ns_bpf_keys: std::collections::HashSet::new(),
            service_map: crate::onion::service_map::ServiceMap::new(),
            cluster_catalog: crate::onion::catalog::EndpointCatalog::new(),
            service_map_tx: tokio::sync::watch::channel(
                crate::onion::service_map::ServiceMap::new(),
            )
            .0,
            dns_faults_tx: tokio::sync::watch::channel(crate::onion::dns::DnsFaultState::default())
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
            deploy_operations: crate::bun::deploy_operations::DeployOperationTracker::default(),
            netns_paths: std::collections::HashMap::new(),
            deployed_specs: std::collections::HashMap::new(),
            next_deploy_gen: 1,
            scheduled_jobs: std::collections::HashMap::new(),
            log_tx: None,
            events: None,
            capacity_cpu_millicores: 0,
            capacity_memory_mb: 0,
            trust_policy: crate::config::node::TrustPolicySection::default(),
            records_dir: None,
            upgrade: None,
            draining: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            identity_retry_ticks: 0,
            deploy_ops_tx,
            deploy_ops_rx,
            drains: new_shared_drains(),
        }
    }

    /// Create a new agent with cluster subsystem handles.
    pub fn with_cluster(
        grill: G,
        port_allocator: PortAllocator,
        command_rx: mpsc::Receiver<AgentCommand>,
        shutdown: CancellationToken,
        cluster: ClusterHandle,
        trust_domain: String,
    ) -> Self {
        let (deploy_ops_tx, deploy_ops_rx) = mpsc::channel(256);
        // Capture the allocator's port range before it moves into the
        // supervisor, so the perimeter firewall drops exactly the host ports
        // Bun actually hands out (not a hardcoded 30000-31000 guess).
        let host_port_range = port_allocator.range();
        Self {
            supervisor: WorkloadSupervisor::new(grill, port_allocator),
            command_rx,
            shutdown,
            readiness: None,
            trace_slots: std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_TRACES)),
            volumes_dir: crate::config::node::StorageSection::default().volumes,
            cluster: Some(cluster),
            trust_domain,
            fault_registry: crate::smoker::registry::FaultRegistry::new(),
            smoker_config: crate::smoker::config::SmokerConfig::default(),
            node_drain_gate: crate::smoker::node_fault::NodeDrainGate::new(),
            node_pressure: crate::smoker::node_pressure::NodePressureController::default(),
            #[cfg(all(feature = "ebpf", target_os = "linux"))]
            onion_ebpf: None,
            #[cfg(all(feature = "ebpf", target_os = "linux"))]
            egress_bindings: std::collections::HashMap::new(),
            #[cfg(all(feature = "ebpf", target_os = "linux"))]
            egress_affected_workloads: std::collections::BTreeSet::new(),
            #[cfg(all(feature = "ebpf", target_os = "linux"))]
            egress_reresolve_ticks: 0,
            #[cfg(all(feature = "ebpf", target_os = "linux"))]
            ebpf_sweep_interval_secs: 60,
            #[cfg(all(feature = "ebpf", target_os = "linux"))]
            ebpf_sweep_ticks: 0,
            #[cfg(all(feature = "ebpf", target_os = "linux"))]
            firewall_bpf_keys: std::collections::HashSet::new(),
            #[cfg(all(feature = "ebpf", target_os = "linux"))]
            cgroup_ns_bpf_keys: std::collections::HashSet::new(),
            service_map: crate::onion::service_map::ServiceMap::new(),
            cluster_catalog: crate::onion::catalog::EndpointCatalog::new(),
            service_map_tx: tokio::sync::watch::channel(
                crate::onion::service_map::ServiceMap::new(),
            )
            .0,
            dns_faults_tx: tokio::sync::watch::channel(crate::onion::dns::DnsFaultState::default())
                .0,
            routing_table: std::sync::Arc::new(tokio::sync::RwLock::new(
                crate::wrapper::routing::RoutingTable::new(),
            )),
            ingress_configs: std::collections::HashMap::new(),
            #[cfg(target_os = "linux")]
            perimeter_config: {
                let mut cfg = if crate::grill::rootless::is_rootless() {
                    crate::firewall::rules::PerimeterConfig::for_rootless()
                } else {
                    crate::firewall::rules::PerimeterConfig::default()
                };
                cfg.host_port_range = host_port_range;
                cfg
            },
            #[cfg(not(target_os = "linux"))]
            perimeter_config: crate::firewall::rules::PerimeterConfig {
                enabled: false,
                host_port_range,
                ..Default::default()
            },
            last_firewall_nodes: None,
            deploy_history: Arc::new(tokio::sync::RwLock::new(Vec::new())),
            deploy_operations: crate::bun::deploy_operations::DeployOperationTracker::default(),
            netns_paths: std::collections::HashMap::new(),
            deployed_specs: std::collections::HashMap::new(),
            next_deploy_gen: 1,
            scheduled_jobs: std::collections::HashMap::new(),
            log_tx: None,
            events: None,
            capacity_cpu_millicores: 0,
            capacity_memory_mb: 0,
            trust_policy: crate::config::node::TrustPolicySection::default(),
            records_dir: None,
            upgrade: None,
            draining: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            identity_retry_ticks: 0,
            deploy_ops_tx,
            deploy_ops_rx,
            drains: new_shared_drains(),
        }
    }

    /// A shared drain handle for the Wrapper proxy, so it counts in-flight
    /// requests to backends this agent is retiring (DEP5). The completion
    /// channel's receiver is dropped: the retire path waits via
    /// `wait_drained`, not the notification stream.
    pub fn drains_handle(&self) -> crate::wrapper::draining::SharedDrains {
        self.drains.clone()
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

    /// Attach process-wide readiness and capability evidence.
    pub fn set_readiness_tracker(&mut self, readiness: crate::bun::readiness::ReadinessTracker) {
        self.readiness = Some(readiness);
    }

    /// Attach the cluster event store used by the TUI and events API.
    pub fn set_event_store(
        &mut self,
        events: Arc<tokio::sync::RwLock<crate::bun::events::EventStore>>,
    ) {
        self.events = Some(events);
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

    /// Subscribe to the set of services under an active `DnsNxdomain` fault.
    ///
    /// The DNS responder reads this alongside the service map: a service in
    /// the set is answered with NXDOMAIN even if it resolves. The agent
    /// republishes on every fault apply/clear/expire.
    pub fn dns_faults_watch(
        &self,
    ) -> tokio::sync::watch::Receiver<crate::onion::dns::DnsFaultState> {
        self.dns_faults_tx.subscribe()
    }

    /// Republish the current `DnsNxdomain` fault set to the DNS responder.
    ///
    /// Rebuilt from the fault registry so it always reflects reality after an
    /// apply, clear, or expiry. Keyed by bare service name (`target_service`),
    /// which is how the resolver checks it. Cheap: the set is tiny and this
    /// only runs when a fault changes.
    fn publish_dns_faults(&self) {
        let faults = self
            .fault_registry
            .iter()
            .filter(|rule| {
                matches!(
                    rule.fault_type,
                    crate::smoker::types::FaultType::DnsNxdomain
                )
            })
            .map(|rule| (rule.target_service.clone(), rule.expires_at_ns));
        let _ = self
            .dns_faults_tx
            .send(crate::onion::dns::DnsFaultState::from_faults(faults));
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

    /// Thread the parsed `[process_workloads]` policy into the supervisor
    /// (D17/H8). Without this the supervisor keeps its deny-by-default
    /// constructor policy, so an operator's allowlist would be ignored.
    pub fn set_process_config(
        &mut self,
        config: crate::config::process_workloads::ProcessWorkloadsConfig,
    ) {
        self.supervisor.set_process_config(config);
    }

    /// Thread the parsed `[smoker]` duration limits in, so faults are bounded
    /// by config rather than only the hardcoded 24h backstop.
    pub fn set_smoker_config(&mut self, config: crate::smoker::config::SmokerConfig) {
        self.smoker_config = config;
    }

    /// Configure the opt-in node-pressure helper and clean owned crash
    /// leftovers. The result feeds capability evidence.
    pub fn configure_node_pressure(
        &mut self,
        limits: crate::smoker::node_pressure::NodePressureLimits,
        executable: std::path::PathBuf,
    ) -> bool {
        self.node_pressure.configure(limits, executable)
    }

    /// Record detected platform capabilities (GPUs, rootless mode) so the
    /// supervisor refuses workloads this node can't honour (D15/M22).
    pub fn set_platform_capabilities(
        &mut self,
        capabilities: crate::bun::supervisor::PlatformCapabilities,
    ) {
        self.supervisor.set_capabilities(capabilities);
    }

    /// Set the base directory for managed volumes (`[storage] volumes`).
    /// The constructors default it; the binary overrides from config.
    pub fn set_volumes_dir(&mut self, dir: std::path::PathBuf) {
        self.volumes_dir = dir;
    }

    /// Snapshot one volume of an app — or, with `volume: None`, every
    /// provisioned volume (discovered from sidecars, so this works for
    /// stopped apps too). Multi-volume snapshots share one timestamp.
    fn snapshot_create(
        &self,
        namespace: &str,
        app_name: &str,
        volume: Option<String>,
        name: Option<String>,
    ) -> Result<Vec<crate::grill::snapshot::SnapshotMeta>, BunError> {
        let volumes = match volume {
            Some(v) => vec![v],
            None => {
                let found = crate::grill::volume::VolumeManager::new(&self.volumes_dir)
                    .provisioned_volumes(namespace, app_name);
                if found.is_empty() {
                    return Err(crate::grill::snapshot::SnapshotError::NoVolumes {
                        namespace: namespace.to_string(),
                        app: app_name.to_string(),
                    }
                    .into());
                }
                found
            }
        };

        let manager = crate::grill::snapshot::SnapshotManager::new(&self.volumes_dir);
        let now = std::time::SystemTime::now();
        let mut metas = Vec::with_capacity(volumes.len());
        for volume_path in &volumes {
            metas.push(manager.create(namespace, app_name, volume_path, name.as_deref(), now)?);
        }
        Ok(metas)
    }

    /// Restore a snapshot — refused while the app has instances that
    /// aren't terminal, because swapping a subvolume under a live
    /// workload corrupts both the workload's view and the data.
    fn snapshot_restore(
        &self,
        namespace: &str,
        app_name: &str,
        name: &str,
    ) -> Result<(), BunError> {
        let running = self.supervisor.list_instances().into_iter().any(|i| {
            i.app_name == app_name
                && i.namespace == namespace
                && !matches!(i.state, ContainerState::Stopped | ContainerState::Failed)
        });
        if running {
            return Err(crate::grill::snapshot::SnapshotError::AppRunning {
                namespace: namespace.to_string(),
                app: app_name.to_string(),
            }
            .into());
        }

        crate::grill::snapshot::SnapshotManager::new(&self.volumes_dir)
            .restore(namespace, app_name, name)
            .map_err(BunError::from)
    }

    /// Enable or disable the perimeter firewall. In-process multi-node tests
    /// run several agents on one host and must not spawn `nft` against the
    /// shared host firewall (`with_cluster` enables it by default on Linux).
    pub fn set_perimeter_enabled(&mut self, enabled: bool) {
        self.perimeter_config.enabled = enabled;
    }

    /// Attach a loaded eBPF handle so the agent can write fault and
    /// egress map entries (L8). Only present with the `ebpf` feature;
    /// `bun` calls this at startup when `[ebpf] enabled`.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    pub async fn set_onion_ebpf(
        &mut self,
        ebpf: std::sync::Arc<tokio::sync::Mutex<crate::onion::ebpf::loader::OnionEbpf>>,
    ) {
        let capability = {
            let handle = ebpf.lock().await;
            crate::sesame::egress::EgressEnforcementCapability {
                connect_ipv4: handle.is_attached(),
                connect_ipv6: handle.connect6_attached(),
                udp_ipv4: handle.sendmsg4_attached(),
                udp_ipv6: handle.sendmsg6_attached(),
                pre_start: self.supervisor.grill().honours_cgroup_path(),
            }
        };
        self.supervisor.set_egress_capability(capability);
        self.onion_ebpf = Some(ebpf);
    }

    /// Configure the kernel-truth sweep interval (`[ebpf]
    /// sweep_interval_secs`); 0 disables the sweep.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    pub fn set_ebpf_sweep_interval(&mut self, secs: u64) {
        self.ebpf_sweep_interval_secs = secs;
    }

    /// No-op without the eBPF data path.
    #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
    pub fn set_ebpf_sweep_interval(&mut self, _secs: u64) {}

    /// Mirror an app's current service-map entry into the kernel
    /// `backend_map` so the eBPF connect hook rewrites its VIP to live
    /// backends (L8 completeness). Called after every service-map add /
    /// health change. A no-op without the eBPF data path loaded.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    async fn sync_backend_ebpf(&self, id: &crate::onion::service_id::ServiceId) {
        let Some(handle) = self.onion_ebpf.as_ref() else {
            return;
        };
        let Some(entry) = self.service_map.resolve(id).cloned() else {
            return;
        };
        let bpf = crate::onion::ebpf::maps::BpfServiceMap::new();
        let mut ebpf = handle.lock().await;
        if let Err(e) = bpf.update_backends_bpf(&mut ebpf, entry.vip, entry.port, &entry) {
            eprintln!("onion: backend map sync failed for {id}: {e}");
        }
    }

    #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
    async fn sync_backend_ebpf(&self, _id: &crate::onion::service_id::ServiceId) {}

    /// Drop an app's `backend_map` entry. Must be called *before* the app
    /// is unregistered from the service map, while its VIP/port are still
    /// known. A no-op without the eBPF data path loaded.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    async fn remove_backend_ebpf(&self, id: &crate::onion::service_id::ServiceId) {
        let Some(handle) = self.onion_ebpf.as_ref() else {
            return;
        };
        // Read the VIP + port straight from the live entry: the VIP is
        // whatever the map allocated (which may have probed off the natural
        // hash on a collision), so we must not re-derive it here.
        let Some((vip, port)) = self.service_map.resolve(id).map(|e| (e.vip, e.port)) else {
            return;
        };
        let bpf = crate::onion::ebpf::maps::BpfServiceMap::new();
        let mut ebpf = handle.lock().await;
        if let Err(e) = bpf.remove_backends_bpf(&mut ebpf, vip, port) {
            eprintln!("onion: backend map removal failed for {id}: {e}");
        }
    }

    #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
    async fn remove_backend_ebpf(&self, _id: &crate::onion::service_id::ServiceId) {}

    /// Reconcile the namespace-firewall eBPF maps against current state (NET5).
    ///
    /// Writes `cgroup_namespace_map` (cgroup → namespace) for every running
    /// instance — which is what makes the connect hook enforce cross-namespace
    /// isolation at all: with the source's namespace unknown the hook lets
    /// every connection through. Writes `firewall_map` for each explicit
    /// cross-namespace `allow_from` rule. Both maps are rebuilt from scratch
    /// each call (a new instance of app A changes rules wherever A is a
    /// *source*), deleting keys no longer desired. A no-op without eBPF.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    async fn sync_firewall_ebpf(&mut self) {
        let Some(handle) = self.onion_ebpf.clone() else {
            return;
        };

        // cgroup id(s) per app, from currently-running instances. Collect the
        // (app, id) pairs first so the `list_instances` borrow is released
        // before the async `pid` lookups.
        let pairs: Vec<(String, InstanceId)> = self
            .supervisor
            .list_instances()
            .into_iter()
            .map(|i| (i.app_name.clone(), i.id.clone()))
            .collect();
        let mut cgroup_ids: std::collections::HashMap<String, Vec<u64>> =
            std::collections::HashMap::new();
        for (app, id) in pairs {
            if let Some(pid) = self.supervisor.grill().pid(&id).await
                && let Some(cg) = crate::sesame::egress::cgroup_id_of_pid(pid)
            {
                cgroup_ids.entry(app).or_default().push(cg);
            }
        }

        let services: Vec<crate::onion::types::ServiceEntry> = self
            .service_map
            .resolve_all()
            .into_iter()
            .cloned()
            .collect();
        let ns_entries =
            crate::sesame::firewall::resolve_cgroup_namespace_entries(&services, &cgroup_ids);
        let fw_entries = crate::sesame::firewall::rules_to_bpf_entries(
            &crate::sesame::firewall::resolve_firewall_rules(&services, &cgroup_ids),
        );

        let desired_ns_keys: std::collections::HashSet<u64> =
            ns_entries.iter().map(|e| e.cgroup_id).collect();
        let desired_fw_keys: std::collections::HashSet<crate::onion::types::FirewallKey> =
            fw_entries.iter().map(|(k, _)| *k).collect();
        let ns_to_delete =
            crate::sesame::firewall::keys_to_delete(&self.cgroup_ns_bpf_keys, &desired_ns_keys);
        let fw_to_delete =
            crate::sesame::firewall::keys_to_delete(&self.firewall_bpf_keys, &desired_fw_keys);

        {
            let mut ebpf = handle.lock().await;
            for entry in &ns_entries {
                if let Err(e) = crate::sesame::firewall::write_cgroup_namespace_entry(
                    &mut ebpf.bpf,
                    entry.cgroup_id,
                    entry.namespace_id,
                ) {
                    eprintln!("sesame: cgroup-namespace map write failed: {e}");
                }
            }
            for (key, value) in &fw_entries {
                if let Err(e) =
                    crate::sesame::firewall::write_firewall_entry(&mut ebpf.bpf, *key, *value)
                {
                    eprintln!("sesame: firewall map write failed: {e}");
                }
            }
            for cg in &ns_to_delete {
                let _ = crate::sesame::firewall::delete_cgroup_namespace_entry(&mut ebpf.bpf, *cg);
            }
            for key in &fw_to_delete {
                let _ = crate::sesame::firewall::delete_firewall_entry(&mut ebpf.bpf, *key);
            }
        }

        self.cgroup_ns_bpf_keys = desired_ns_keys;
        self.firewall_bpf_keys = desired_fw_keys;
    }

    #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
    async fn sync_firewall_ebpf(&mut self) {}

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
            let replica_index = crate::grill::InstanceIdentity::parse(&instance.id.0)
                .map(|ident| ident.ordinal)
                .unwrap_or(0);
            inventory.push(crate::upgrade::marker::InstanceInventory {
                namespace: instance.namespace.clone(),
                app_name: instance.app_name.clone(),
                instance_id: replica_index,
                pid,
                full_id: instance.id.0.clone(),
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
            // Prefer the exact canonical id; fall back to the legacy
            // `{app}-{ordinal}` form for a marker written before this theme.
            let id = if item.full_id.is_empty() {
                InstanceId(format!("{}-{}", item.app_name, item.instance_id))
            } else {
                InstanceId(item.full_id.clone())
            };
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

        let replica_index = crate::grill::InstanceIdentity::parse(&instance_id.0)
            .map(|ident| ident.ordinal)
            .unwrap_or(0);
        let runtime = self.supervisor.grill().runtime_kind();
        let rootless_network = self
            .supervisor
            .grill()
            .rootless_network_record(instance_id)
            .await;
        let record = crate::grill::records::InstanceRecord {
            schema: 2,
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
            rootless_network,
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
            // The runtime knows the still-running container by the id it was
            // started under, which is what the record stores. The supervisor
            // and everything downstream key on the canonical id: for a fresh
            // record they're the same; for a legacy record (this node
            // upgraded across the identity change with workloads running) the
            // canonical id is rebuilt from the record's structured fields so
            // the adopted instance lands under a namespace-safe key.
            let runtime_id = InstanceId(record.instance_id.clone());
            let instance_id =
                if crate::grill::InstanceIdentity::parse(&record.instance_id).is_some() {
                    runtime_id.clone()
                } else {
                    let mut ident = crate::grill::InstanceIdentity::new(
                        &record.namespace,
                        &record.app_name,
                        record.replica_index,
                    );
                    // Preserve a legacy canary generation if the id carried one.
                    if let Some(legacy) = crate::grill::InstanceIdentity::parse_legacy(
                        &record.instance_id,
                        &record.namespace,
                    ) && legacy.app == record.app_name
                    {
                        ident.generation = legacy.generation;
                    }
                    ident.instance_id()
                };
            // Never clobber an instance the current process already tracks.
            if self.supervisor.get_instance(&instance_id).is_some() {
                continue;
            }

            let adopted = matches!(
                self.supervisor.grill().adopt(&runtime_id, &record).await,
                Ok(true)
            );
            if !adopted {
                let _ = crate::grill::records::remove_record(&dir, &record.instance_id);
                // The instance is gone for good — its key material goes
                // with it (PKI7). The identity dir was created under the id
                // the container ran as (the runtime id), which for a legacy
                // record differs from the canonical supervisor key.
                let identity_dir = self.instance_identity_dir(&runtime_id);
                if let Err(e) = crate::sesame::identity::cleanup_identity_dir(&identity_dir) {
                    eprintln!("bun: warning: failed to remove identity dir for {runtime_id}: {e}");
                }
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

            // Rebuild the workload's identity and rotation schedule from
            // its per-instance directory, so an adopted instance keeps
            // rotating on time instead of coming back with
            // `identity: None` (D9). The directory was created under the
            // runtime id, so a legacy record still finds its keys. An
            // unprovisioned directory loads as `None` and the rotation loop
            // provisions afresh.
            let identity_dir = self.instance_identity_dir(&runtime_id);
            let identity = match crate::sesame::identity::load_identity(&identity_dir) {
                Ok(identity) => identity,
                Err(e) => {
                    eprintln!("bun: warning: could not restore identity for {runtime_id}: {e}");
                    None
                }
            };
            let identity_mount = identity.is_some().then(|| identity_dir.clone());

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
                identity,
                identity_mount,
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
            // Logs are captured under the runtime id (the container's name).
            self.spawn_log_forwarder(&runtime_id, &record.app_name, &record.namespace);
            adopted_count += 1;
        }

        if adopted_count > 0 {
            println!("bun: adopted {adopted_count} running instance(s) from a previous process");
        }

        // Identity dirs with no live owner — legacy app-scoped layouts and
        // instances that died while bun was down — are stale key material.
        self.sweep_orphaned_identity_dirs();

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

        if let Some(readiness) = self.readiness.clone() {
            let (capabilities, _) = self.live_egress_report_state().await;
            readiness.set_capabilities(capabilities).await;
        }

        loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => {
                    self.shutdown_all().await;
                    break;
                }
                Some(cmd) = self.command_rx.recv() => {
                    self.handle_command(cmd).await;
                }
                Some(op) = self.deploy_ops_rx.recv() => {
                    self.handle_deploy_op(op).await;
                }
                Some(req) = Self::recv_snapshot(&mut self.cluster) => {
                    self.handle_snapshot_request(req).await;
                }
                _ = health_interval.tick() => {
                    self.enforce_live_egress_or_stop().await;
                    if let Some(readiness) = self.readiness.clone() {
                        let (capabilities, _) = self.live_egress_report_state().await;
                        readiness.set_capabilities(capabilities).await;
                    }
                    self.run_health_checks().await;
                    self.check_jobs().await;
                    self.fire_due_jobs().await;
                    self.check_apps().await;
                    self.drive_pending_restarts().await;
                    self.expire_faults().await;
                    self.reconcile_firewall().await;
                    self.reresolve_egress().await;
                    self.sweep_kernel_networking().await;
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
    async fn handle_snapshot_request(&self, req: CollectSnapshotRequest) {
        use crate::reporting::worker::{AgentSnapshot, InstanceSnapshot};

        let (capabilities, enforced_instances) = self.live_egress_report_state().await;
        #[cfg(all(feature = "ebpf", target_os = "linux"))]
        let egress_affected_workloads: Vec<
            crate::reporting::types::EgressAffectedWorkload,
        > = self
            .egress_affected_workloads
            .iter()
            .map(
                |(app_name, namespace)| crate::reporting::types::EgressAffectedWorkload {
                    app_name: app_name.clone(),
                    namespace: namespace.clone(),
                },
            )
            .collect();
        #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
        let egress_affected_workloads = Vec::new();

        let instances = self.supervisor.list_instances();
        let snapshot = AgentSnapshot {
            instances: instances
                .iter()
                .map(|inst| {
                    // The report carries the replica ordinal, recovered from
                    // the canonical id (e.g. "default__web-0" → 0).
                    let instance_id = crate::grill::InstanceIdentity::parse(&inst.id.0)
                        .map(|ident| ident.ordinal)
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
                    let has_egress = spec
                        .and_then(|s| s.egress.as_ref())
                        .is_some_and(|e| !e.allow.is_empty() || !e.allow_franchise.is_empty());
                    let egress_enforcement = if !has_egress {
                        crate::reporting::types::EgressEnforcementStatus::NotRequested
                    } else if capabilities.egress.can_enforce_allowlist()
                        && enforced_instances.contains(&inst.id)
                    {
                        crate::reporting::types::EgressEnforcementStatus::Enforced
                    } else {
                        crate::reporting::types::EgressEnforcementStatus::Unenforced
                    };

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
                        egress_enforcement,
                    }
                })
                .collect(),
            // Terminal instances no longer hold their ports (CP6) — the
            // worker also filters them from running/capacity.
            allocated_ports: instances
                .iter()
                .filter(|i| {
                    !matches!(
                        i.state,
                        crate::grill::state::ContainerState::Stopped
                            | crate::grill::state::ContainerState::Failed
                    )
                })
                .filter_map(|i| i.host_port)
                .collect(),
            capacity_cpu_millicores: self.capacity_cpu_millicores,
            capacity_memory_mb: self.capacity_memory_mb,
            capabilities,
            readiness: match &self.readiness {
                Some(readiness) => Some(readiness.snapshot().await),
                None => None,
            },
            egress_degraded: !egress_affected_workloads.is_empty(),
            egress_affected_workloads,
        };
        let _ = req.response.send(snapshot);
    }

    /// Read the hooks and enforcement map as kernel truth for reporting.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    async fn live_egress_report_state(
        &self,
    ) -> (
        crate::meat::cluster_state::NodeCapabilities,
        std::collections::HashSet<InstanceId>,
    ) {
        let Some(handle) = self.onion_ebpf.as_ref() else {
            return (
                crate::meat::cluster_state::NodeCapabilities {
                    dns: self.supervisor.dns_capability(),
                    ..Default::default()
                },
                Default::default(),
            );
        };
        let mut ebpf = handle.lock().await;
        let capabilities = crate::meat::cluster_state::NodeCapabilities {
            egress: crate::sesame::egress::EgressEnforcementCapability {
                connect_ipv4: ebpf.is_attached(),
                connect_ipv6: ebpf.connect6_attached(),
                udp_ipv4: ebpf.sendmsg4_attached(),
                udp_ipv6: ebpf.sendmsg6_attached(),
                pre_start: self.supervisor.grill().honours_cgroup_path(),
            },
            dns: self.supervisor.dns_capability(),
        };
        let enforced_cgroups =
            crate::sesame::egress::list_enforced_cgroups(&mut ebpf.bpf).unwrap_or_default();
        let enforced = self
            .egress_bindings
            .iter()
            .filter(|(_, binding)| enforced_cgroups.contains(&binding.cgroup_id))
            .map(|(instance_id, _)| instance_id.clone())
            .collect();
        (capabilities, enforced)
    }

    /// A portable build has no kernel enforcement to report.
    #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
    async fn live_egress_report_state(
        &self,
    ) -> (
        crate::meat::cluster_state::NodeCapabilities,
        std::collections::HashSet<InstanceId>,
    ) {
        (
            crate::meat::cluster_state::NodeCapabilities {
                dns: self.supervisor.dns_capability(),
                ..Default::default()
            },
            Default::default(),
        )
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
                let operation = match self.deploy_operations.start(&config).await {
                    Ok(operation) => operation,
                    Err(error) => {
                        let message = format!("deploy refused: {error}");
                        self.record_event(
                            crate::bun::events::EventKind::Deploy,
                            crate::bun::events::EventSeverity::Critical,
                            None,
                            None,
                            message.clone(),
                        )
                        .await;
                        let _ = events.send(ApplyEvent::Error { message }).await;
                        return;
                    }
                };
                let _ = events
                    .send(ApplyEvent::Accepted {
                        operation_id: operation.id().to_string(),
                    })
                    .await;
                if self.draining.load(std::sync::atomic::Ordering::Relaxed) {
                    let message =
                        "node is draining for a binary upgrade; retry shortly".to_string();
                    self.record_event(
                        crate::bun::events::EventKind::Deploy,
                        crate::bun::events::EventSeverity::Critical,
                        None,
                        None,
                        "deploy refused while node is draining".to_string(),
                    )
                    .await;
                    operation
                        .finish(
                            crate::bun::deploy_operations::DeployOperationOutcome::Failed,
                            message.clone(),
                        )
                        .await;
                    let _ = events.send(ApplyEvent::Error { message }).await;
                    return;
                }
                // Register any cron-scheduled jobs so the event loop fires them
                // on their schedule rather than at deploy time (E).
                self.register_scheduled_jobs(&config);

                // Forward deploy events to the caller, mirroring errors into the
                // event store. The deploy itself runs on its own task so a slow
                // pull or a rolling health wait can't wedge this loop
                // (DEP4/codex-M3); it drives its authoritative steps back
                // through `deploy_ops_tx`.
                let (forward_tx, mut forward_rx) = mpsc::channel(64);
                let event_store = self.events.clone();
                let observed_operation = operation.clone();
                tokio::spawn(async move {
                    while let Some(event) = forward_rx.recv().await {
                        match &event {
                            ApplyEvent::Complete { created, .. } => {
                                observed_operation
                                    .finish(
                                        crate::bun::deploy_operations::DeployOperationOutcome::Completed,
                                        format!("deploy completed ({created} instances)"),
                                    )
                                    .await;
                            }
                            ApplyEvent::Error { message } => {
                                observed_operation
                                    .finish(
                                        crate::bun::deploy_operations::DeployOperationOutcome::Failed,
                                        message.clone(),
                                    )
                                    .await;
                                if let Some(store) = &event_store {
                                    let timestamp = SystemTime::now()
                                        .duration_since(SystemTime::UNIX_EPOCH)
                                        .unwrap_or_default()
                                        .as_secs();
                                    store.write().await.record(
                                        timestamp,
                                        crate::bun::events::EventKind::Deploy,
                                        crate::bun::events::EventSeverity::Critical,
                                        None,
                                        None,
                                        None,
                                        message.clone(),
                                    );
                                }
                            }
                            _ => {}
                        }
                        // A disconnected SSE client doesn't cancel the deploy or
                        // its evidence. Keep draining the worker to a terminal
                        // outcome even when this send fails.
                        let _ = events.send(event).await;
                    }
                    observed_operation
                        .finish(
                            crate::bun::deploy_operations::DeployOperationOutcome::Unknown,
                            "deploy worker ended without a terminal event",
                        )
                        .await;
                });
                let worker = DeployWorker {
                    grill: self.supervisor.grill().clone(),
                    port_allocator: self.supervisor.port_allocator(),
                    ops: DeployOps {
                        tx: self.deploy_ops_tx.clone(),
                    },
                    operation: Some(operation),
                };
                tokio::spawn(async move {
                    worker.run_deploy(config, forward_tx).await;
                });
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
            AgentCommand::DesiredApps { response } => {
                let mut apps = self
                    .deployed_specs
                    .iter()
                    .map(
                        |((app, namespace), spec)| crate::bun::diagnostics::DesiredAppEvidence {
                            app: app.clone(),
                            namespace: namespace.clone(),
                            desired_replicas: crate::bun::diagnostics::desired_replica_count(
                                spec.replicas,
                                1,
                            ),
                            scheduled_replicas: self
                                .supervisor
                                .list_instances()
                                .iter()
                                .filter(|instance| {
                                    instance.app_name == *app && instance.namespace == *namespace
                                })
                                .count()
                                .try_into()
                                .unwrap_or(u32::MAX),
                            service_port: spec.port,
                        },
                    )
                    .collect::<Vec<_>>();
                apps.sort_by(|left, right| {
                    (&left.namespace, &left.app).cmp(&(&right.namespace, &right.app))
                });
                let _ = response.send(apps);
            }
            AgentCommand::JobStatus { response } => {
                let statuses = self.get_job_status();
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
            AgentCommand::DeployOperations { response } => {
                let _ = response.send(self.deploy_operations.snapshot().await);
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
                // Resolve the target instance on the loop (cheap), then run the
                // exec off-loop under a deadline (H3). Running it inline let a
                // long command (`relish exec app -- sleep 3600`) stall health
                // checks, restarts and every other command — the exact reason
                // Trace was moved off the loop.
                match self.resolve_running_instance(&app_name, &namespace) {
                    Ok(instance_id) => {
                        let grill = self.supervisor.grill().clone();
                        tokio::spawn(async move {
                            let result = match tokio::time::timeout(
                                EXEC_TIMEOUT,
                                grill.exec(&instance_id, &command),
                            )
                            .await
                            {
                                Ok(inner) => inner.map_err(BunError::from),
                                Err(_) => Err(BunError::ExecTimeout {
                                    seconds: EXEC_TIMEOUT.as_secs(),
                                }),
                            };
                            let _ = response.send(result);
                        });
                    }
                    Err(error) => {
                        let _ = response.send(Err(error));
                    }
                }
            }
            AgentCommand::Trace {
                request,
                internal_destination,
                source_node,
                response,
            } => match self.prepare_trace(request, internal_destination, source_node) {
                Ok(trace) => {
                    tokio::spawn(async move {
                        let _ = response.send(trace.run().await);
                    });
                }
                Err(error) => {
                    let _ = response.send(Err(error));
                }
            },
            AgentCommand::Nodes { response } => {
                let nodes = self.get_cluster_nodes();
                let _ = response.send(nodes);
            }
            AgentCommand::Council { response } => {
                let status = self.get_council_status().await;
                let _ = response.send(status);
            }
            AgentCommand::JoinIssue {
                token,
                node_id,
                csr_der,
                response,
            } => {
                let result = self.handle_join_issue(&token, &node_id, &csr_der).await;
                let _ = response.send(result);
            }
            AgentCommand::InjectPartition {
                peers,
                duration_secs,
                injected_by,
                response,
            } => {
                // Legacy chaos API — create a partition fault in the registry
                let request = crate::smoker::types::FaultRequest {
                    fault_type: crate::smoker::types::FaultType::CouncilPartition,
                    target_service: peers.join(","),
                    namespace: None,
                    target_instance: None,
                    target_node: None,
                    duration: std::time::Duration::from_secs(duration_secs),
                    injected_by,
                    reason: Some("legacy chaos partition".into()),
                    include_leader: false,
                    override_safety: false,
                    acknowledged: false,
                };
                // Safety rails apply to the legacy path too (M1): a partition
                // that would strand quorum must be refused, not waved through
                // just because it came in on the old chaos API.
                let context = self.build_safety_context(&request).await;
                let check = crate::smoker::safety::evaluate_safety(&request, &context);
                if !check.approved {
                    let reason = check
                        .violation
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "safety check failed".into());
                    let _ = response.send(Err(BunError::FaultRejected { reason }));
                    return;
                }
                let rule = self.fault_registry.insert(&request);
                // L15: actually partition. Resolve each peer (by name)
                // to its gossip address from membership, then block both
                // the gossip and Raft transports to it — the old code
                // only recorded a registry entry and dropped nothing.
                let blocked = self.apply_partition(&peers).await;
                // Record the reversal so heal and TTL-expiry unblock exactly
                // these peers (M1): without it a Ctrl-C'd partition stayed in
                // force forever with no record it existed.
                self.record_reversal(
                    rule.id,
                    crate::smoker::types::FaultReversal::Partition {
                        peers: peers.clone(),
                    },
                );
                let msg =
                    format!("partition injected: blocking {blocked} peer(s) for {duration_secs}s");
                let summary = crate::smoker::types::FaultSummary::from(&rule);
                let _ = response.send(Ok((msg, summary)));
            }
            AgentCommand::HealPartition { response } => {
                // Legacy chaos API — clear all faults, reversing each so no
                // persistent effect (a SIGSTOPped workload, a cgroup cap, a
                // transport blocklist) outlives the heal (M1). The old code
                // cleared the registry and the blocklists but never ran
                // `reverse_fault`, so a frozen process stayed frozen.
                let removed = self.fault_registry.clear();
                for rule in &removed {
                    self.delete_fault_bpf_entry(rule).await;
                    self.reverse_fault(rule).await;
                }
                // Belt and braces: drop any blocklist entries no fault owned.
                self.clear_partition().await;
                self.publish_dns_faults();
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
            AgentCommand::SnapshotCreate {
                namespace,
                app_name,
                volume,
                name,
                response,
            } => {
                let _ = response.send(self.snapshot_create(&namespace, &app_name, volume, name));
            }
            AgentCommand::SnapshotList {
                namespace,
                app_name,
                response,
            } => {
                let manager = crate::grill::snapshot::SnapshotManager::new(&self.volumes_dir);
                let _ = response.send(manager.list(&namespace, &app_name).map_err(BunError::from));
            }
            AgentCommand::SnapshotRestore {
                namespace,
                app_name,
                name,
                response,
            } => {
                let _ = response.send(self.snapshot_restore(&namespace, &app_name, &name));
            }
            AgentCommand::SnapshotDelete {
                namespace,
                app_name,
                name,
                response,
            } => {
                let manager = crate::grill::snapshot::SnapshotManager::new(&self.volumes_dir);
                let _ = response.send(
                    manager
                        .delete(&namespace, &app_name, &name)
                        .map_err(BunError::from),
                );
            }
            AgentCommand::InjectFault {
                mut request,
                response,
            } => {
                // Duration bounds first (server-side, so a direct API call
                // can't slip past the CLI's defaulting): apply the configured
                // default when none was given, reject anything over the max.
                match crate::smoker::config::effective_duration(
                    request.duration,
                    request.fault_type.is_instantaneous(),
                    &self.smoker_config,
                ) {
                    Ok(effective) => request.duration = effective,
                    Err(reason) => {
                        let _ = response.send(Err(BunError::FaultRejected { reason }));
                        return;
                    }
                }

                // Safety rails next (L14): reject faults that risk
                // quorum, kill a service's last replica, target the
                // leader, or exceed the node-percentage cap — unless
                // explicitly overridden. The context is built even with no
                // cluster handle so the replica-minimum rail still runs (M1).
                let context = self.build_safety_context(&request).await;
                let check = crate::smoker::safety::evaluate_safety(&request, &context);
                if !check.approved {
                    let reason = check
                        .violation
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "safety check failed".into());
                    let _ = response.send(Err(BunError::FaultRejected { reason }));
                    return;
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
            AgentCommand::ClearFault {
                fault_id,
                allow_workload_fault,
                allow_node_fault,
                allow_node_pressure,
                response,
            } => {
                let fault_id = crate::smoker::types::FaultId(fault_id);
                if let Some(rule) = self.fault_registry.get(fault_id) {
                    let denied = if rule.fault_type.is_node_operation() {
                        (!allow_node_fault).then_some(
                            "node fault reversal requires alter_node_state authorisation",
                        )
                    } else if matches!(
                        rule.fault_type,
                        crate::smoker::types::FaultType::NodePressure { .. }
                    ) {
                        (!allow_node_pressure).then_some(
                            "node pressure reversal requires saturate_capacity authorisation",
                        )
                    } else {
                        (!allow_workload_fault).then_some(
                            "workload fault reversal requires inject_workload_faults authorisation",
                        )
                    };
                    if let Some(reason) = denied {
                        let _ = response.send(Err(BunError::FaultRejected {
                            reason: reason.to_string(),
                        }));
                        return;
                    }
                }
                let msg = match self.fault_registry.get(fault_id).cloned() {
                    Some(rule) => {
                        self.delete_fault_bpf_entry(&rule).await;
                        let node_pressure = matches!(
                            &rule.fault_type,
                            crate::smoker::types::FaultType::NodePressure { .. }
                        );
                        if node_pressure {
                            if let Err(reason) = self.node_pressure.clear(rule.id).await {
                                let _ = response.send(Err(BunError::FaultRejected { reason }));
                                return;
                            }
                        } else {
                            self.reverse_fault(&rule).await;
                        }
                        self.fault_registry.remove(fault_id);
                        // A DnsNxdomain fault lives in the published set, not a
                        // BPF map, so republish so the responder stops faulting
                        // the target.
                        self.publish_dns_faults();
                        format!("cleared fault {} ({})", rule.id, rule.fault_type)
                    }
                    None => format!("fault {} not found", fault_id.0),
                };
                let _ = response.send(Ok(msg));
            }
            AgentCommand::ClearAllFaults { response } => {
                let removed = self.fault_registry.clear_workload_faults();
                for rule in &removed {
                    self.delete_fault_bpf_entry(rule).await;
                    self.reverse_fault(rule).await;
                }
                // Republish the (now empty) DnsNxdomain set for the responder.
                self.publish_dns_faults();
                let msg = format!("cleared {} fault(s)", removed.len());
                let _ = response.send(Ok(msg));
            }
            AgentCommand::ClearFaultsByService {
                service,
                namespace,
                response,
            } => {
                let removed = self
                    .fault_registry
                    .clear_by_service(&service, namespace.as_deref());
                for rule in &removed {
                    self.delete_fault_bpf_entry(rule).await;
                    self.reverse_fault(rule).await;
                }
                self.publish_dns_faults();
                let msg = format!("cleared {} fault(s) for {service}", removed.len());
                let _ = response.send(Ok(msg));
            }
            AgentCommand::ListFaults { response } => {
                let summaries = self.fault_registry.list();
                let _ = response.send(summaries);
            }
            AgentCommand::Resolve { app_name, response } => {
                // The CLI targets a service by bare name; resolve the first
                // match in any namespace, against the merged cluster view so a
                // service running only on other nodes still resolves (12b.4).
                let merged = self.service_map.with_cluster_catalog(&self.cluster_catalog);
                let result = merged
                    .resolve_by_name(&app_name)
                    .map(|e| e.to_resolve_response());
                let _ = response.send(result);
            }
            AgentCommand::ResolveAll { response } => {
                let merged = self.service_map.with_cluster_catalog(&self.cluster_catalog);
                let results = merged
                    .resolve_all()
                    .iter()
                    .map(|e| e.to_resolve_response())
                    .collect();
                let _ = response.send(results);
            }
            AgentCommand::SyncClusterCatalog { catalog } => {
                // Only rebuild + republish when the catalogue actually moved;
                // the reconciler pushes on every tick.
                if self.cluster_catalog != *catalog {
                    self.cluster_catalog = *catalog;
                    self.rebuild_routing_table().await;
                }
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

    /// Unblock a specific set of peers on both transports — the reversal of
    /// [`apply_partition`]. Only the addresses this fault added are removed, so
    /// healing one partition fault leaves any others still in force.
    async fn remove_partition(&self, peers: &[String]) {
        let Some(handle) = &self.cluster else {
            return;
        };
        let blocklists = &handle.partition_blocklists;
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
        if let Some(gossip) = &blocklists.gossip {
            let mut set = gossip.write().await;
            for addr in &targets {
                set.remove(addr);
            }
        }
        if let Some(raft) = &blocklists.raft {
            let mut set = raft.write().await;
            for addr in &targets {
                let raft_addr = std::net::SocketAddr::new(
                    addr.ip(),
                    (addr.port() as i32 + blocklists.raft_port_offset) as u16,
                );
                set.remove(&raft_addr);
            }
        }
    }

    /// Build the safety context for a fault request from live cluster state.
    ///
    /// Always returns a context (M1): when there's no council — standalone
    /// mode, or a node that hasn't joined — the quorum, leader, and
    /// node-percentage rails have nothing to act on and neutralise themselves
    /// via zeroed fields, but the **replica-minimum** rail still fires from the
    /// locally-known replica count. That rail is what stops `fault kill
    /// --count 0` from taking out a service's last replica, so it must run even
    /// with no cluster handle; the old code returned `None` there and skipped
    /// safety entirely.
    async fn build_safety_context(
        &self,
        request: &crate::smoker::types::FaultRequest,
    ) -> crate::smoker::types::SafetyContext {
        // Replicas of the target service running locally (an approximation —
        // the leader has the cluster-wide count, but this node protects at
        // least its own replicas). Available with or without a cluster.
        let target_service_replicas = self
            .supervisor
            .list_instances()
            .iter()
            .filter(|i| {
                i.app_name == request.target_service
                    && request
                        .namespace
                        .as_deref()
                        .is_none_or(|ns| ns == i.namespace)
            })
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
                        | crate::smoker::types::FaultType::NodePressure { .. }
                        | crate::smoker::types::FaultType::CouncilPartition
                )
            })
            .count() as u32;

        let target_service_faulted_replicas =
            self.fault_registry
                .count_by_service(&request.target_service) as u32;

        // Cluster-derived fields, or zeros when this node has no council. A
        // zero `council_size`/`total_nodes` makes the quorum, leader, and
        // node-percentage rails self-skip (see `smoker::safety`).
        let (council_size, leader_node_id, total_nodes) = match self
            .cluster
            .as_ref()
            .and_then(|handle| handle.raft_metrics_rx.as_ref().map(|rx| (handle, rx)))
        {
            Some((handle, metrics_rx)) => {
                let metrics = metrics_rx.borrow().clone();
                let council_size =
                    metrics.membership_config.membership().voter_ids().count() as u32;
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
                (council_size, leader_node_id, total_nodes)
            }
            None => (0, String::new(), 0),
        };

        crate::smoker::types::SafetyContext {
            council_size,
            council_nodes_with_active_faults: active_node_faults,
            leader_node_id,
            total_nodes,
            nodes_with_active_faults: active_node_faults,
            target_service_replicas,
            target_service_faulted_replicas,
        }
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
                // Remember which PIDs we froze so clear/expiry can SIGCONT
                // them. Without this a paused workload stayed frozen forever
                // once the fault expired (CHAOS1); Resume was a separate
                // manual fault the operator had to remember to send.
                let mut paused = Vec::new();
                for pid in pids {
                    if let Err(e) = crate::smoker::process::pause_process(pid as i32) {
                        eprintln!("smoker: pause {pid} failed: {e}");
                    } else {
                        paused.push(pid as i32);
                    }
                }
                self.record_reversal(rule.id, crate::smoker::types::FaultReversal::Pause(paused));
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
                // Cap the TARGET instance's `cpu.max` quota instead of
                // burning cycles in Bun's own cgroup (CHAOS1). The old code
                // spun blocking tasks that competed for whatever CPU the Bun
                // process could get, which starved Bun — not the workload —
                // and could not be lifted before the deadline. Now the
                // workload keeps only `100 - percentage` of a core, and clear
                // /expiry restores its original quota.
                self.apply_cgroup_fault(
                    rule,
                    |cgroup| {
                        let saved = crate::smoker::resource::read_cpu_max(cgroup)
                            .map_err(|e| e.to_string())?;
                        // O17: `cores` used to be parsed and thrown away while
                        // the quota maths assumed one core, so on a 4-core node
                        // "80% stress" actually took 95%.
                        crate::smoker::resource::apply_cpu_stress(cgroup, *percentage, *cores)
                            .map_err(|e| e.to_string())?;
                        Ok(saved)
                    },
                    |cgroup, saved| {
                        if let Err(e) = crate::smoker::resource::restore_cpu_max(cgroup, saved) {
                            eprintln!(
                                "smoker: rollback cpu.max on {} failed: {e}",
                                cgroup.display()
                            );
                        }
                    },
                )
                .await
                .map(|saved| {
                    self.record_reversal(
                        rule.id,
                        crate::smoker::types::FaultReversal::CpuMax(saved),
                    );
                })
            }
            FaultType::DnsNxdomain => {
                // DNS resolution lives in the userspace responder
                // (src/onion/dns.rs), so this fault does too. Republish the
                // faulted-service set and the responder starts returning
                // NXDOMAIN for the target. This used to write an eBPF
                // `fault_dns_map` entry into an object that was never loaded,
                // so the fault did nothing on any configuration (12b.6 gate).
                self.publish_dns_faults();
                Ok(())
            }
            FaultType::Drop { .. } | FaultType::Partition { .. } => {
                // Connect-time drop and partition faults have a real cgroup
                // eBPF implementation. Record the exact keys only after every
                // requested map write succeeds.
                #[cfg(all(feature = "ebpf", target_os = "linux"))]
                {
                    if self.onion_ebpf.is_some() {
                        let reversal = self.write_fault_bpf_entry(rule).await?;
                        self.record_reversal(rule.id, reversal);
                        return Ok(());
                    }
                }
                Err(format!(
                    "{} requires the eBPF data path, which is not loaded on this node",
                    rule.fault_type
                ))
            }
            FaultType::Delay { .. } => Err(
                "delay faults need a TC packet hook; the current cgroup connect hook cannot delay packets"
                    .to_string(),
            ),
            FaultType::Bandwidth { .. } => Err(
                "bandwidth faults need a TC packet hook; no bandwidth program is attached"
                    .to_string(),
            ),
            FaultType::MemoryPressure { percentage, oom } => {
                // Squeeze the TARGET instance's `memory.high` toward its hard
                // limit so the kernel forces reclaim/allocation stalls on the
                // workload (CHAOS1 — this used to be a genuine no-op that
                // reported success). `oom` isn't a reversible cgroup edit —
                // it would lower `memory.max` to trigger the kill — so we
                // reject it here rather than pretend; the supervisor's normal
                // OOM/restart path is the honest way to test that.
                if *oom {
                    return Err(
                        "memory oom is not a reversible fault; use a Kill fault to crash an instance"
                            .to_string(),
                    );
                }
                self.apply_cgroup_fault(
                    rule,
                    |cgroup| {
                        let saved = crate::smoker::resource::read_memory_high(cgroup)
                            .map_err(|e| e.to_string())?;
                        crate::smoker::resource::apply_memory_pressure(cgroup, *percentage)
                            .map_err(|e| e.to_string())?;
                        Ok(saved)
                    },
                    |cgroup, saved| {
                        if let Err(e) = crate::smoker::resource::restore_memory_high(cgroup, saved)
                        {
                            eprintln!(
                                "smoker: rollback memory.high on {} failed: {e}",
                                cgroup.display()
                            );
                        }
                    },
                )
                .await
                .map(|saved| {
                    self.record_reversal(
                        rule.id,
                        crate::smoker::types::FaultReversal::MemoryHigh(saved),
                    );
                })
            }
            FaultType::DiskIoThrottle {
                bytes_per_sec,
                write_only,
            } => {
                // Throttle the TARGET instance's block-I/O via `io.max`
                // (CHAOS1). The device major:minor is read from the workload's
                // volumes dir so the throttle lands on the disk the workload
                // actually writes to; clear/expiry lifts it.
                let device = self.io_device_major_minor();
                let dev_for_reverse = device.clone();
                let dev_for_rollback = device.clone();
                self.apply_cgroup_fault(
                    rule,
                    |cgroup| {
                        crate::smoker::resource::apply_disk_io_throttle(
                            cgroup,
                            *bytes_per_sec,
                            *write_only,
                            &device,
                        )
                        .map_err(|e| e.to_string())?;
                        Ok(cgroup.to_string_lossy().into_owned())
                    },
                    |cgroup, _saved| {
                        if let Err(e) = crate::smoker::resource::remove_disk_io_throttle(
                            cgroup,
                            &dev_for_rollback,
                        ) {
                            eprintln!(
                                "smoker: rollback io.max on {} failed: {e}",
                                cgroup.display()
                            );
                        }
                    },
                )
                .await
                .map(|paths| {
                    let instances = paths
                        .into_iter()
                        .map(|(_, path)| (path, dev_for_reverse.clone()))
                        .collect();
                    self.record_reversal(
                        rule.id,
                        crate::smoker::types::FaultReversal::DiskIo { instances },
                    );
                })
            }
            FaultType::NodeDrain => {
                if rule.duration_ns == 0 {
                    return Err("node faults require a non-zero duration".to_string());
                }
                if self.cluster.is_none() {
                    return Err("node drain requires an active cluster runtime".to_string());
                }
                let Some(readiness) = self.readiness.clone() else {
                    return Err(
                        "node drain requires live readiness evidence for scheduler fencing"
                            .to_string(),
                    );
                };

                if self.node_drain_gate.begin() {
                    readiness.register("node:chaos-drain", true).await;
                }
                readiness
                    .degraded("node:chaos-drain", "node drain fault is active")
                    .await;
                self.record_reversal(rule.id, crate::smoker::types::FaultReversal::NodeDrain);
                Ok(())
            }
            FaultType::NodeKill { kill_containers } => {
                if rule.duration_ns == 0 {
                    return Err("node faults require a non-zero duration".to_string());
                }
                let Some(cluster) = &self.cluster else {
                    return Err("node kill requires an active cluster runtime".to_string());
                };

                cluster.partition_blocklists.node_gate.quiesce();
                if *kill_containers {
                    let ids: Vec<_> = self
                        .supervisor
                        .list_instances()
                        .iter()
                        .map(|instance| instance.id.clone())
                        .collect();
                    for id in ids {
                        if let Err(error) = self.supervisor.grill().kill(&id).await {
                            eprintln!("smoker: node-kill container {} failed: {error}", id.0);
                        }
                    }
                }
                self.record_reversal(rule.id, crate::smoker::types::FaultReversal::NodeQuiesce);
                Ok(())
            }
            FaultType::NodePressure {
                cpu_percentage,
                memory_percentage,
            } => {
                if rule.duration_ns == 0 {
                    return Err("node pressure requires a non-zero duration".to_string());
                }
                self.node_pressure
                    .apply(rule.id, *cpu_percentage, *memory_percentage)
                    .await?;
                self.record_reversal(rule.id, crate::smoker::types::FaultReversal::NodePressure);
                Ok(())
            }
            FaultType::CouncilPartition => Err(
                "council partitions must use the authenticated /v1/chaos/partition operation"
                    .to_string(),
            ),
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
                    && rule.matches_namespace(&i.namespace)
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

    /// The `(instance id, cgroup path)` pairs a resource fault should target.
    ///
    /// A resource fault names a service (and optionally one instance); the
    /// cgroup a limit must be written to is `cgroup_path(namespace, app,
    /// ordinal)` for each matching running instance. We take namespace and app
    /// straight from the `WorkloadInstance`, and recover the ordinal from the
    /// canonical instance id (falling back to `0` for a legacy id). This is
    /// what threads instance metadata into `apply_fault` (CHAOS1): the fault
    /// arrives with only a service name, and we turn it into concrete cgroups.
    #[cfg(target_os = "linux")]
    fn target_instance_cgroups(
        &self,
        rule: &crate::smoker::types::FaultRule,
    ) -> Vec<(InstanceId, std::path::PathBuf)> {
        self.supervisor
            .list_instances()
            .iter()
            .filter(|i| {
                i.app_name == rule.target_service
                    && rule.matches_namespace(&i.namespace)
                    && rule.target_instance.as_ref().is_none_or(|t| &i.id.0 == t)
            })
            .map(|i| {
                let ordinal = crate::grill::InstanceIdentity::parse(&i.id.0)
                    .map(|ident| ident.ordinal)
                    .unwrap_or(0);
                let path = crate::grill::cgroup::cgroup_path(&i.namespace, &i.app_name, ordinal);
                (i.id.clone(), path)
            })
            .collect()
    }

    /// Apply a cgroup-writing fault to every target instance and collect the
    /// per-instance saved state the `apply` closure returns (for later
    /// reversal).
    ///
    /// Returns an honest error when there are no running instances to target,
    /// or on any platform without cgroup v2. The `apply` closure runs once per
    /// target cgroup. If one fails partway through, the instances already
    /// modified are rolled back with `restore` before the error is surfaced
    /// (M1) — without that, an earlier replica stayed throttled while the
    /// caller, seeing the error, dropped the registry entry that would have
    /// let a later clear undo it.
    #[cfg(target_os = "linux")]
    async fn apply_cgroup_fault<F, R>(
        &self,
        rule: &crate::smoker::types::FaultRule,
        mut apply: F,
        restore: R,
    ) -> Result<Vec<(String, String)>, String>
    where
        F: FnMut(&std::path::Path) -> Result<String, String>,
        R: Fn(&std::path::Path, &str),
    {
        let targets = self.target_instance_cgroups(rule);
        if targets.is_empty() {
            return Err(format!("no running instances of {}", rule.target_service));
        }
        let mut saved = Vec::with_capacity(targets.len());
        let mut applied: Vec<(std::path::PathBuf, String)> = Vec::new();
        for (id, cgroup) in targets {
            match apply(&cgroup) {
                Ok(value) => {
                    applied.push((cgroup.clone(), value.clone()));
                    saved.push((id.0, value));
                }
                Err(e) => {
                    // Roll back the instances already modified, newest first,
                    // so a partial application never leaks a limit.
                    for (cgroup, value) in applied.iter().rev() {
                        restore(cgroup, value);
                    }
                    return Err(e);
                }
            }
        }
        Ok(saved)
    }

    #[cfg(not(target_os = "linux"))]
    async fn apply_cgroup_fault<F, R>(
        &self,
        rule: &crate::smoker::types::FaultRule,
        _apply: F,
        _restore: R,
    ) -> Result<Vec<(String, String)>, String>
    where
        F: FnMut(&std::path::Path) -> Result<String, String>,
        R: Fn(&std::path::Path, &str),
    {
        Err(format!("{} requires Linux cgroups", rule.fault_type))
    }

    /// Record a fault's reversal state in the registry after it was applied,
    /// so a later clear/expiry can undo the persistent effect.
    fn record_reversal(
        &mut self,
        id: crate::smoker::types::FaultId,
        reversal: crate::smoker::types::FaultReversal,
    ) {
        if let Some(rule) = self.fault_registry.get_mut(id) {
            rule.reversal = reversal;
        }
    }

    /// The block device (`major:minor`) backing this node's workload storage,
    /// used to key an `io.max` throttle. cgroup v2 `io.max` is per-device, so
    /// a throttle must name one. We resolve the device under the volumes dir
    /// where workloads write; if it can't be determined we fall back to the
    /// common `8:0` (first SCSI/SATA disk), which the operator can override by
    /// running on a host whose data disk is `8:0`.
    fn io_device_major_minor(&self) -> String {
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::MetadataExt;
            if let Ok(meta) = std::fs::metadata(&self.volumes_dir) {
                let dev = meta.dev();
                // Linux encodes major:minor in st_dev; unpack per libc rules.
                let major = (dev >> 8) & 0xfff;
                let minor = (dev & 0xff) | ((dev >> 12) & 0xfff00);
                return format!("{major}:{minor}");
            }
        }
        "8:0".to_string()
    }

    /// Reverse a cleared or expired fault's persistent effect.
    ///
    /// eBPF network faults are undone by `delete_fault_bpf_entry`; this handles
    /// everything else that leaves a durable change — a paused process (SIGCONT
    /// it), a capped `cpu.max`, a squeezed `memory.high` or an `io.max`
    /// throttle (restore the saved value). Best-effort: an instance that has
    /// since exited simply has nothing left to restore.
    async fn reverse_fault(&mut self, rule: &crate::smoker::types::FaultRule) {
        use crate::smoker::types::FaultReversal;
        match &rule.reversal {
            FaultReversal::None => {}
            FaultReversal::Pause(pids) => {
                for pid in pids {
                    if let Err(e) = crate::smoker::process::resume_process(*pid) {
                        // A process that exited while paused is fine; anything
                        // else is worth a line so a stuck workload is visible.
                        eprintln!("smoker: resume (auto) pid {pid} failed: {e}");
                    }
                }
            }
            FaultReversal::CpuMax(saved) => {
                for (_id, cgroup, value) in Self::rejoin_cgroups(rule, saved) {
                    if let Err(e) = crate::smoker::resource::restore_cpu_max(&cgroup, &value) {
                        eprintln!(
                            "smoker: restore cpu.max on {} failed: {e}",
                            cgroup.display()
                        );
                    }
                }
            }
            FaultReversal::MemoryHigh(saved) => {
                for (_id, cgroup, value) in Self::rejoin_cgroups(rule, saved) {
                    if let Err(e) = crate::smoker::resource::restore_memory_high(&cgroup, &value) {
                        eprintln!(
                            "smoker: restore memory.high on {} failed: {e}",
                            cgroup.display()
                        );
                    }
                }
            }
            FaultReversal::DiskIo { instances } => {
                for (path, device) in instances {
                    let cgroup = std::path::PathBuf::from(path);
                    if let Err(e) =
                        crate::smoker::resource::remove_disk_io_throttle(&cgroup, device)
                    {
                        eprintln!("smoker: lift io.max on {path} failed: {e}");
                    }
                }
            }
            FaultReversal::Partition { peers } => {
                self.remove_partition(peers).await;
            }
            FaultReversal::BpfConnectKeys(_) => {
                // `delete_fault_bpf_entry` owns map cleanup before this
                // generic non-eBPF reversal path runs.
            }
            FaultReversal::NodeDrain => {
                if self.node_drain_gate.finish()
                    && let Some(readiness) = self.readiness.clone()
                {
                    readiness.ready("node:chaos-drain").await;
                }
            }
            FaultReversal::NodeQuiesce => {
                if let Some(cluster) = &self.cluster {
                    cluster.partition_blocklists.node_gate.restore();
                }
            }
            FaultReversal::NodePressure => {
                if let Err(error) = self.node_pressure.clear(rule.id).await {
                    eprintln!(
                        "smoker: clear node pressure for {} failed: {error}",
                        rule.id
                    );
                }
            }
        }
    }

    /// Pair each saved `(instance id, value)` with the instance's cgroup path.
    ///
    /// The cgroup path is recomputed from the *current* target-instance list so
    /// reversal writes to the same directory the fault wrote to. An instance
    /// that has since gone away is dropped (nothing to restore).
    fn rejoin_cgroups(
        rule: &crate::smoker::types::FaultRule,
        saved: &[(String, String)],
    ) -> Vec<(String, std::path::PathBuf, String)> {
        saved
            .iter()
            .filter_map(|(id, value)| {
                let ident = crate::grill::InstanceIdentity::parse(id)?;
                let path =
                    crate::grill::cgroup::cgroup_path(&ident.namespace, &ident.app, ident.ordinal);
                // A specific instance target still restores only its own cgroup.
                if rule.target_instance.as_ref().is_some_and(|t| t != id) {
                    return None;
                }
                Some((id.clone(), path, value.clone()))
            })
            .collect()
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
                    crate::smoker::types::FaultType::CouncilPartition
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
        let mut expired_dns = false;
        for rule in &expired {
            if !rule.target_service.is_empty() {
                eprintln!(
                    "smoker: fault {} expired ({}), cleaning up",
                    rule.id, rule.fault_type
                );
            }
            self.delete_fault_bpf_entry(rule).await;
            // Undo persistent non-eBPF effects too: SIGCONT a paused
            // workload, lift a cgroup cap. Without this an expired Pause left
            // the process frozen and an expired resource fault left its cap in
            // place (CHAOS1).
            self.reverse_fault(rule).await;
            expired_dns |= matches!(
                rule.fault_type,
                crate::smoker::types::FaultType::DnsNxdomain
            );
        }
        // Republish the DnsNxdomain set only if one actually expired, so the
        // responder drops the name (the resolver also self-corrects on
        // expiry, but publishing keeps the set honest).
        if expired_dns {
            self.publish_dns_faults();
        }
        // Retry any node-pressure cgroup whose directory lingered after its
        // helper was killed, so a transient removal failure doesn't leave the
        // controller permanently refusing new pressure faults.
        self.node_pressure.retry_pending_cleanup().await;
    }

    /// The network-byte-order VIP + port for a fault's target service, if
    /// it is registered. VIP is deterministic from the app name; the port
    /// comes from the service entry. Connect/bandwidth fault keys need both.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    fn fault_vip_port(&self, rule: &crate::smoker::types::FaultRule) -> Option<(u32, u16)> {
        // A namespace-qualified fault resolves the exact service identity, so a
        // network fault on `web` in `team-a` never picks up `team-b`'s `web`
        // VIP. A legacy fault with no namespace falls back to the first entry
        // in any namespace.
        let entry = match rule.namespace.as_deref() {
            Some(namespace) => self
                .service_map
                .resolve(&crate::onion::service_id::ServiceId::new(
                    namespace,
                    rule.target_service.as_str(),
                )),
            None => self.service_map.resolve_by_name(&rule.target_service),
        }?;
        Some((entry.vip.to_network_byte_order(), entry.port.to_be()))
    }

    /// Resolve every running instance of a partition's source app to the
    /// cgroup id observed by `bpf_get_current_cgroup_id()`.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    async fn partition_source_cgroup_ids(
        &self,
        source_app: Option<&str>,
        client_supplied_id: u64,
    ) -> Result<Vec<u64>, String> {
        if client_supplied_id != 0 {
            return Err(
                "source_cgroup_id is server-resolved and must be zero in fault requests"
                    .to_string(),
            );
        }
        let Some(source_app) = source_app else {
            return Ok(vec![0]);
        };
        let instances: Vec<_> = self
            .supervisor
            .list_instances()
            .iter()
            .filter(|instance| instance.app_name == source_app)
            .map(|instance| instance.id.clone())
            .collect();
        if instances.is_empty() {
            return Err(format!("no running instances of source app {source_app}"));
        }

        let mut cgroup_ids = Vec::with_capacity(instances.len());
        for instance in instances {
            let pid = self
                .supervisor
                .grill()
                .pid(&instance)
                .await
                .ok_or_else(|| format!("source instance {} has no running PID", instance.0))?;
            let cgroup_id = crate::sesame::egress::cgroup_id_of_pid(pid).ok_or_else(|| {
                format!(
                    "could not resolve the cgroup id for source instance {}",
                    instance.0
                )
            })?;
            cgroup_ids.push(cgroup_id);
        }
        cgroup_ids.sort_unstable();
        cgroup_ids.dedup();
        Ok(cgroup_ids)
    }

    /// Write the eBPF map entry for a newly injected network fault (P2).
    ///
    /// Only reachable with the `ebpf` feature: without it, `apply_fault`
    /// rejects network faults before we get here. `expires_ns` comes from
    /// the rule, which now uses CLOCK_MONOTONIC (P0) to match the kernel's
    /// `bpf_ktime_get_ns()`.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    async fn write_fault_bpf_entry(
        &self,
        rule: &crate::smoker::types::FaultRule,
    ) -> Result<crate::smoker::types::FaultReversal, String> {
        use crate::smoker::bpf_maps;
        use crate::smoker::bpf_types::*;
        use crate::smoker::types::{FaultReversal, FaultType};

        let source_cgroup_ids = match &rule.fault_type {
            FaultType::Partition {
                source_app,
                source_cgroup_id,
            } => {
                self.partition_source_cgroup_ids(source_app.as_deref(), *source_cgroup_id)
                    .await?
            }
            _ => vec![0],
        };
        let (vip, port) = self
            .fault_vip_port(rule)
            .ok_or_else(|| format!("no service VIP exists for {}", rule.target_service))?;
        let Some(handle) = self.onion_ebpf.as_ref() else {
            return Err("the eBPF data path is not loaded on this node".to_string());
        };
        let expires = rule.expires_at_ns;
        let mut ebpf = handle.lock().await;

        match &rule.fault_type {
            FaultType::Drop { probability } => {
                let key = connect_fault_key(vip, port);
                let value = BpfConnectFaultValue {
                    action: FAULT_ACTION_DROP,
                    probability: *probability,
                    _pad: [0; 6],
                    delay_ns: 0,
                    jitter_ns: 0,
                    expires_ns: expires,
                };
                bpf_maps::write_connect_fault(&mut ebpf.bpf, key, value)
                    .map_err(|error| format!("failed to install drop fault: {error}"))?;
                Ok(FaultReversal::BpfConnectKeys(vec![(
                    key.virtual_ip,
                    key.port,
                    key.source_cgroup_id,
                )]))
            }
            FaultType::Partition {
                source_app: _,
                source_cgroup_id: _,
            } => {
                let value = BpfConnectFaultValue {
                    action: FAULT_ACTION_PARTITION,
                    probability: 100,
                    _pad: [0; 6],
                    delay_ns: 0,
                    jitter_ns: 0,
                    expires_ns: expires,
                };
                let mut installed = Vec::with_capacity(source_cgroup_ids.len());
                for source_cgroup_id in source_cgroup_ids {
                    let key = partition_fault_key(vip, port, source_cgroup_id);
                    if let Err(error) = bpf_maps::write_connect_fault(&mut ebpf.bpf, key, value) {
                        for installed_key in installed.iter().rev() {
                            let _ = bpf_maps::delete_connect_fault(&mut ebpf.bpf, installed_key);
                        }
                        return Err(format!("failed to install partition fault: {error}"));
                    }
                    installed.push(key);
                }
                Ok(FaultReversal::BpfConnectKeys(
                    installed
                        .iter()
                        .map(|key| (key.virtual_ip, key.port, key.source_cgroup_id))
                        .collect(),
                ))
            }
            _ => Err(format!(
                "{} has no connect-map implementation",
                rule.fault_type
            )),
        }
    }

    /// Delete the eBPF map entry for a cleared or expired fault (P2).
    ///
    /// Best-effort: VIP is deterministic from the app name, but the port
    /// comes from the service entry — if the service is already gone we
    /// skip, since the kernel ignores the entry past its `expires_ns`.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
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
        let mut ebpf = handle.lock().await;

        if let crate::smoker::types::FaultReversal::BpfConnectKeys(keys) = &rule.reversal {
            for (vip, port, source_cgroup_id) in keys {
                if let Err(error) = bpf_maps::delete_connect_fault(
                    &mut ebpf.bpf,
                    &partition_fault_key(*vip, *port, *source_cgroup_id),
                ) {
                    eprintln!(
                        "smoker: delete connect fault key for {} failed: {error}",
                        rule.id
                    );
                }
            }
            return;
        }

        // Compatibility fallback for a rule created before exact key
        // ownership was recorded.
        if let Some((vip, port)) = self.fault_vip_port(rule)
            && matches!(rule.fault_type, FaultType::Drop { .. })
            && let Err(error) =
                bpf_maps::delete_connect_fault(&mut ebpf.bpf, &connect_fault_key(vip, port))
        {
            eprintln!(
                "smoker: delete legacy connect fault key for {} failed: {error}",
                rule.id
            );
        }
    }

    /// Delete is a no-op without the eBPF data path (nothing was written).
    #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
    async fn delete_fault_bpf_entry(&self, _rule: &crate::smoker::types::FaultRule) {}

    /// Enforce the image trust policy for a workload before deploying it.
    ///
    /// Returns `Err(reason)` to reject the deploy. It's a no-op (`Ok(None)`)
    /// when the policy doesn't require signatures or for a process workload
    /// (no image to verify).
    ///
    /// When `require_signatures` is set and this node has no council handle,
    /// it can't reach the manifest catalogue or the cluster root CA — the
    /// verification material simply isn't here. That used to skip the check
    /// (a fail-OPEN: an unsigned image sailed through on any worker or
    /// standalone node). Now it fails CLOSED: an image deploy is refused
    /// because the node can't prove the image is signed (IMG2). Cluster nodes
    /// all run a `CouncilNode` that replicates this state, so only a genuine
    /// standalone node hits this refusal.
    ///
    /// For a Pickle-hosted image it verifies the signature against the cluster
    /// root CA and returns the digest-pinned reference (`repo@sha256:…`) the
    /// deploy must use, so the runtime pulls exactly the verified bytes — a
    /// tag can move between verify and pull (IMG1).
    async fn enforce_image_signature(&self, spec: &AppSpec) -> Result<Option<String>, String> {
        if !self.trust_policy.require_signatures {
            return Ok(None);
        }
        // A process workload has no image; nothing to verify.
        if spec.image.is_none() {
            return Ok(None);
        }
        let Some(council) = self.cluster.as_ref().and_then(|c| c.council.as_ref()) else {
            return Err(format!(
                "image {} requires a signature but this node has no cluster trust state to verify it against (require_signatures is enabled); run in cluster mode or disable require_signatures",
                spec.image.as_deref().unwrap_or("<none>")
            ));
        };
        let catalog = council.manifest_catalog().await;
        let security_state = council.security_state().await;
        let root_ca = security_state
            .get_ca(crate::sesame::types::CaRole::Root)
            .map(|ca| ca.certificate_der.clone());
        let verified = crate::meat::scheduler::verify_image_signature(
            spec.image.as_deref(),
            &catalog,
            &self.trust_policy,
            root_ca.as_deref(),
            Some(&security_state.crl),
        )
        .map_err(|e| e.to_string())?;
        Ok(match (spec.image.as_deref(), verified) {
            (Some(image), Some(digest)) => {
                Some(crate::meat::scheduler::pin_image_reference(image, &digest))
            }
            _ => None,
        })
    }

    /// Every age identity that could decrypt this namespace's secrets, newest
    /// generation first: the namespace-scoped keys then the cluster-wide keys.
    ///
    /// Returning all live generations (not just the active one) is what makes a
    /// secret survive a rotation window — a value encrypted under the retiring
    /// key still decrypts until it is retired, and a value re-encrypted under
    /// the new key decrypts immediately (PKI8).
    async fn decrypt_identities(&self, namespace: &str) -> Vec<age::x25519::Identity> {
        let Some(cluster) = self.cluster.as_ref() else {
            return Vec::new();
        };
        let Some(ikm) = cluster.wrapping_ikm else {
            return Vec::new();
        };
        let Some(council) = cluster.council.as_ref() else {
            return Vec::new();
        };
        let security_state = council.security_state().await;

        let ns_scope = crate::sesame::types::AgeKeyScope::Namespace(namespace.to_string());
        security_state
            .age_keypairs_for_scope(&ns_scope)
            .into_iter()
            .chain(
                security_state
                    .age_keypairs_for_scope(&crate::sesame::types::AgeKeyScope::ClusterWide),
            )
            .filter_map(|kp| crate::sesame::secret::unwrap_age_identity(kp, &ikm).ok())
            .collect()
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
        instance_id: &str,
        host_port: Option<u16>,
        cgroup_str: &str,
        volumes_dir: Option<&std::path::Path>,
        netns_path: Option<&str>,
        identities: Vec<age::x25519::Identity>,
    ) -> crate::grill::oci::OciSpec {
        // Try each live generation's identity until one decrypts the value, so
        // a secret encrypted under any still-present key is readable across a
        // rotation window (PKI8).
        let decryptor: Option<crate::grill::oci::SecretDecryptor> = if identities.is_empty() {
            None
        } else {
            Some(Box::new(move |encrypted: &str| {
                let mut last_err = String::from("no age identity could decrypt the value");
                for id in &identities {
                    match crate::sesame::secret::decrypt_secret(encrypted, id) {
                        Ok(plain) => return Ok(plain),
                        Err(e) => last_err = e.to_string(),
                    }
                }
                Err(last_err)
            }) as crate::grill::oci::SecretDecryptor)
        };
        crate::grill::oci::generate_oci_spec_with_decryptor(
            app_name,
            namespace,
            spec,
            instance_id,
            host_port,
            cgroup_str,
            volumes_dir,
            netns_path,
            decryptor.as_ref(),
        )
    }

    /// Program a freshly-started instance's kernel networking: mirror its
    /// backend into `backend_map` (L8) and reconcile namespace-firewall maps
    /// (NET5). Egress is deliberately absent here: it must already have been
    /// programmed before `start`, never repaired in post-start bookkeeping.
    async fn finish_instance_networking(&mut self, app_name: &str, namespace: &str) {
        let service_id = crate::onion::service_id::ServiceId::new(namespace, app_name);
        self.sync_backend_ebpf(&service_id).await;
        self.sync_firewall_ebpf().await;
    }

    /// Fast pre-create bookkeeping for a fresh instance (the loop side of the
    /// former `drive_instance_startup`): transition to Preparing, prepare
    /// managed volumes and the identity dir, and build the OCI spec. The
    /// spawned deploy task calls `grill.create` with the returned spec off the
    /// loop, so the image pull no longer blocks health checks (DEP4).
    async fn prepare_fresh_instance(
        &mut self,
        instance_id: &InstanceId,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
    ) -> Result<PreparedInstance, BunError> {
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

        let host_port = self
            .supervisor
            .get_instance(instance_id)
            .and_then(|i| i.host_port);

        // Extract the replica index from the canonical id's ordinal.
        let instance_index: u32 = instance_id
            .0
            .rsplit('-')
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let cgroup_path = crate::grill::cgroup::cgroup_path(namespace, app_name, instance_index);
        let cgroup_str = cgroup_path.to_string_lossy().into_owned();
        let netns_path = self
            .netns_paths
            .get(instance_id)
            .map(|p| p.to_string_lossy().into_owned());
        let identities = self.decrypt_identities(namespace).await;
        if identities.is_empty() && spec.env.values().any(|v| v.is_encrypted()) {
            return Err(BunError::DeployFailed {
                app_name: app_name.to_string(),
                reason: "encrypted secrets require cluster security state (unavailable here)"
                    .to_string(),
            });
        }
        // Managed volumes must exist before the bind mounts reference them
        // (runc fails create on a missing bind source, review M21).
        let managed: Vec<crate::config::types::VolumeSpec> = spec
            .volumes
            .iter()
            .filter(|v| v.source.is_none())
            .cloned()
            .collect();
        if !managed.is_empty() {
            let manager = crate::grill::volume::VolumeManager::new(self.volumes_dir.clone());
            let volume_ns = namespace.to_string();
            let volume_app = app_name.to_string();
            tokio::task::spawn_blocking(move || {
                for vol in &managed {
                    manager.create_managed_volume(
                        &volume_ns,
                        &volume_app,
                        &vol.path,
                        vol.size.as_deref(),
                    )?;
                }
                Ok::<(), crate::grill::volume::VolumeError>(())
            })
            .await
            .map_err(|e| BunError::DeployFailed {
                app_name: app_name.to_string(),
                reason: format!("volume preparation task failed: {e}"),
            })?
            .map_err(|e| BunError::DeployFailed {
                app_name: app_name.to_string(),
                reason: format!("managed volume: {e}"),
            })?;
        }

        // The per-instance identity dir must exist before create (PKI7).
        if let Err(e) = self.prepare_instance_identity(instance_id) {
            eprintln!("bun: warning: {e}");
        }

        let oci_spec = Self::oci_spec_with_secrets(
            app_name,
            namespace,
            spec,
            &instance_id.0,
            host_port,
            &cgroup_str,
            Some(&self.volumes_dir),
            netns_path.as_deref(),
            identities,
        );

        Ok(PreparedInstance {
            oci_spec,
            cgroup_path,
            cgroup_str,
            has_init: !spec.init.is_empty(),
        })
    }

    /// Post-start bookkeeping for a fresh instance (the loop side of the tail
    /// of `drive_instance_startup`): record the container IP, transition to
    /// HealthWait (→Running if no health checks), register its service-map
    /// backend and finish kernel networking.
    async fn finish_fresh_instance(
        &mut self,
        instance_id: &InstanceId,
        app_name: &str,
        namespace: &str,
        container_ip: Option<std::net::Ipv4Addr>,
    ) -> Result<(), BunError> {
        self.spawn_log_forwarder(instance_id, app_name, namespace);
        self.persist_instance_record(instance_id).await;

        if let Some(instance) = self.supervisor.get_instance_mut(instance_id) {
            instance.container_ip = container_ip;
        }

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
            if let Err(e) = self.service_map.add_backend(
                &crate::onion::service_id::ServiceId::new(namespace, app_name),
                backend,
            ) {
                eprintln!("onion: backend not registered for {namespace}/{app_name}: {e}");
            }
        }

        self.finish_instance_networking(app_name, namespace).await;
        Ok(())
    }

    /// Fast pre-create bookkeeping for a rolling-redeploy instance: fail closed
    /// on undecryptable secrets, prepare its identity dir, build the OCI spec.
    /// The spawned task then creates and starts it off the loop.
    async fn prepare_rolling_instance(
        &mut self,
        instance_id: &InstanceId,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
        host_port: Option<u16>,
        index: u32,
    ) -> Result<crate::grill::oci::OciSpec, BunError> {
        let identities = self.decrypt_identities(namespace).await;
        if identities.is_empty() && spec.env.values().any(|v| v.is_encrypted()) {
            return Err(BunError::DeployFailed {
                app_name: app_name.to_string(),
                reason: format!(
                    "cannot start {}: encrypted secrets require cluster security state",
                    instance_id.0
                ),
            });
        }
        if let Err(e) = self.prepare_instance_identity(instance_id) {
            eprintln!("bun: warning: {e}");
        }
        let cgroup_path = crate::grill::cgroup::cgroup_path(namespace, app_name, index);
        Ok(Self::oci_spec_with_secrets(
            app_name,
            namespace,
            spec,
            &instance_id.0,
            host_port,
            &cgroup_path.to_string_lossy(),
            Some(&self.volumes_dir),
            None,
            identities,
        ))
    }

    /// Roll a failed rolling redeploy back: kill and clean up the new
    /// instances (grill-created but never supervisor-tracked), release their
    /// ports and identity dirs, and record the rollback in history.
    #[allow(clippy::too_many_arguments)]
    async fn rollback_rolling_deploy(
        &mut self,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
        new_ids: &[InstanceId],
        new_prepared: &[InstanceId],
        new_ports: &std::collections::HashMap<InstanceId, Option<u16>>,
        replica_count: u32,
    ) {
        for new_id in new_ids {
            let _ = self.supervisor.grill().kill(new_id).await;
            self.clear_egress(new_id).await;
            if let Some(port) = new_ports.get(new_id).copied().flatten() {
                let _ = self.supervisor.port_allocator.release(port).await;
            }
        }
        for new_id in new_prepared {
            self.cleanup_instance_identity(new_id);
        }
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
    }

    /// Halt a failed rolling deploy without reverting (`auto_rollback = false`).
    ///
    /// Unlike [`rollback_rolling_deploy`], the healthy new instances that were
    /// already published stay in service alongside the surviving old ones — the
    /// operator inspects the mixed state and decides. Only the incomplete
    /// instance (prepared but never made healthy, so not in `new_ids`) is torn
    /// down, so a failed replacement can't leak its container, port or identity
    /// dir.
    #[allow(clippy::too_many_arguments)]
    async fn halt_rolling_deploy(
        &mut self,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
        new_ids: &[InstanceId],
        new_prepared: &[InstanceId],
        new_ports: &std::collections::HashMap<InstanceId, Option<u16>>,
        replica_count: u32,
    ) {
        for prepared in new_prepared {
            if new_ids.contains(prepared) {
                continue; // healthy and serving — leave it running
            }
            let _ = self.supervisor.grill().kill(prepared).await;
            self.clear_egress(prepared).await;
            if let Some(port) = new_ports.get(prepared).copied().flatten() {
                let _ = self.supervisor.port_allocator.release(port).await;
            }
            self.cleanup_instance_identity(prepared);
        }
        let entry = crate::meat::deploy_types::DeployHistoryEntry {
            id: crate::meat::deploy_types::DeployId(
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            ),
            app_id: crate::meat::types::AppId::new(app_name, namespace),
            image: spec.image.clone().unwrap_or_default(),
            result: crate::meat::deploy_types::DeployResult::Halted,
            created_at: SystemTime::now(),
            completed_at: SystemTime::now(),
            steps_completed: new_ids.len(),
            steps_total: replica_count as usize,
            spec: Some(Box::new(spec.clone())),
        };
        self.deploy_history.write().await.push(entry);
    }

    /// Retire the old instances and register the healthy new ones after a
    /// rolling redeploy: kill old, rebuild the service map and health config,
    /// register backends, finish kernel networking, store ingress, record it.
    #[allow(clippy::too_many_arguments)]
    async fn finalise_rolling_deploy(
        &mut self,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
        existing: &[InstanceId],
        new_ids: &[InstanceId],
        new_ports: &std::collections::HashMap<InstanceId, Option<u16>>,
        new_ips: &std::collections::HashMap<InstanceId, Option<std::net::Ipv4Addr>>,
        mut new_specs: std::collections::HashMap<InstanceId, crate::grill::oci::OciSpec>,
        now: Instant,
    ) {
        // DEP5: route new traffic to the fresh instances before we retire the
        // old ones. We add the new backends to the service map alongside the
        // old ones and rebuild the routing table, so the proxy starts picking
        // the fresh instances. Then we drain the old instances (let in-flight
        // requests finish, up to drain_timeout) and stop-and-wait-for-exit.
        // Only after that do we tear their bookkeeping down.
        let drain_timeout = spec
            .deploy
            .as_ref()
            .map(crate::meat::deploy_types::DeployConfig::from_spec)
            .unwrap_or_default()
            .drain_timeout;
        let service_id = crate::onion::service_id::ServiceId::new(namespace, app_name);
        // M7: publishing backends and retiring old instances now happen
        // incrementally as the rollout steps, so by the time we get here both
        // are usually already done. These loops are idempotent catch-ups for
        // anything the stepped path didn't cover (a zero-port app, or an
        // instance the planner retired before this call).
        if spec.port.is_some() {
            for new_id in new_ids {
                if let Some(host_port) = new_ports.get(new_id).copied().flatten() {
                    let backend = crate::onion::types::BackendInstance {
                        instance_id: new_id.0.clone(),
                        node_ip: new_ips
                            .get(new_id)
                            .copied()
                            .flatten()
                            .unwrap_or(std::net::Ipv4Addr::LOCALHOST),
                        host_port,
                        healthy: true,
                    };
                    if let Err(e) = self.service_map.add_backend(&service_id, backend) {
                        eprintln!("onion: backend not registered for {service_id:?}: {e}");
                    }
                }
            }
            self.rebuild_routing_table().await;
        }
        self.retire_with_drain(existing, drain_timeout).await;

        for old_id in existing {
            // NET6: lift the retiring instance's egress enforcement.
            self.clear_egress(old_id).await;
            self.cleanup_instance_identity(old_id);
        }
        self.supervisor.remove_app(app_name, namespace).await;
        self.remove_backend_ebpf(&service_id).await;
        for old_id in existing {
            self.remove_instance_record(old_id);
        }
        let _ = self.service_map.unregister(&service_id);

        for new_id in new_ids {
            let host_port = new_ports.get(new_id).copied().flatten();
            let health_config = spec
                .health
                .as_ref()
                .zip(spec.port)
                .map(|(hs, port)| crate::bun::health::HealthCheckConfig::from_spec(hs, port));
            if let Some(ref cfg) = health_config {
                self.supervisor
                    .register_health(new_id.clone(), cfg.clone(), now);
            }
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
                    container_ip: new_ips.get(new_id).copied().flatten(),
                    created_at: now,
                    restart_policy: crate::bun::restart::RestartPolicy::default(),
                    health_config,
                    is_job: false,
                    image: spec.image.clone().unwrap_or_default(),
                    oci_spec: new_specs.remove(new_id),
                    identity: None,
                    identity_mount: None,
                },
            );
        }
        let key = (app_name.to_string(), namespace.to_string());
        self.supervisor.app_instances.insert(key, new_ids.to_vec());

        if let Some(port) = spec.port {
            let firewall = spec.firewall.as_ref().and_then(|f| {
                if f.allow_from.is_empty() {
                    None
                } else {
                    Some(f.allow_from.clone())
                }
            });
            let _ = self.service_map.register(&service_id, port, firewall);

            for new_id in new_ids {
                if let Some(host_port) = new_ports.get(new_id).copied().flatten() {
                    let backend = crate::onion::types::BackendInstance {
                        instance_id: new_id.0.clone(),
                        node_ip: new_ips
                            .get(new_id)
                            .copied()
                            .flatten()
                            .unwrap_or(std::net::Ipv4Addr::LOCALHOST),
                        host_port,
                        healthy: true,
                    };
                    if let Err(e) = self.service_map.add_backend(&service_id, backend) {
                        eprintln!("onion: backend not registered for {service_id:?}: {e}");
                    }
                }
            }
        }

        self.finish_instance_networking(app_name, namespace).await;
        if let Some(ref ingress) = spec.ingress {
            self.ingress_configs.insert(
                (namespace.to_string(), app_name.to_string()),
                ingress.clone(),
            );
        }

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
    }

    /// Post-start bookkeeping for a job instance (the loop side of the former
    /// `drive_job_startup`): store the OCI spec, log forwarder, on-disk
    /// record, and transitions to Running.
    async fn finish_job_instance(
        &mut self,
        instance_id: &InstanceId,
        job_name: &str,
        namespace: &str,
        oci_spec: crate::grill::oci::OciSpec,
    ) -> Result<(), BunError> {
        if let Some(instance) = self.supervisor.get_instance_mut(instance_id) {
            instance.oci_spec = Some(oci_spec);
        }
        {
            let instance = self
                .supervisor
                .get_instance_mut(instance_id)
                .ok_or_else(|| BunError::InstanceNotFound {
                    instance_id: instance_id.clone(),
                })?;
            instance.state = instance.state.transition_to(ContainerState::Starting)?;
        }
        self.spawn_log_forwarder(instance_id, job_name, namespace);
        self.persist_instance_record(instance_id).await;
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

    /// Program an instance's egress *before* its process starts, closing
    /// the window during which a fresh workload could connect anywhere
    /// (the connect hook allows everything for a cgroup with no
    /// `egress_enforced` flag). Only possible when the runtime honours
    /// the OCI `cgroupsPath` (root-mode runc): the agent creates the
    /// cgroup directory itself, programs the maps against its inode, and
    /// only then lets the runtime start the workload into it.
    ///
    /// Returns an error — failing the deploy closed — when enforcement is
    /// required but cannot be guaranteed (connect6 missing, cgroup id
    /// unresolvable, map programming failed).
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    async fn apply_egress_pre_start(
        &mut self,
        instance_id: &InstanceId,
        app_name: &str,
        spec: &AppSpec,
        cgroup_path: &std::path::Path,
    ) -> Result<(), BunError> {
        use crate::sesame::egress::{self, PreStartEgress};

        let has_allowlist = spec.egress.as_ref().is_some_and(|e| !e.allow.is_empty());
        let capability = match self.onion_ebpf.as_ref() {
            Some(handle) => {
                let handle = handle.lock().await;
                egress::EgressEnforcementCapability {
                    connect_ipv4: handle.is_attached(),
                    connect_ipv6: handle.connect6_attached(),
                    udp_ipv4: handle.sendmsg4_attached(),
                    udp_ipv6: handle.sendmsg6_attached(),
                    pre_start: self.supervisor.grill().honours_cgroup_path(),
                }
            }
            None => Default::default(),
        };

        // Create the cgroup directory before the runtime does, so its
        // inode — the id `bpf_get_current_cgroup_id()` will report — is
        // known before the process exists. runc joins an existing
        // `cgroupsPath` directory untouched, keeping the inode stable.
        let cgroup_id = if has_allowlist && capability.can_enforce_allowlist() {
            let _ = tokio::fs::create_dir_all(cgroup_path).await;
            egress::cgroup_id_of_path(cgroup_path)
        } else {
            None
        };

        match egress::plan_pre_start_egress(has_allowlist, capability, cgroup_id) {
            PreStartEgress::NoPolicy => Ok(()),
            PreStartEgress::Refuse { reason } => Err(BunError::DeployFailed {
                app_name: app_name.to_string(),
                reason: format!("egress enforcement for {}: {reason}", instance_id.0),
            }),
            PreStartEgress::Program { cgroup_id } => {
                self.program_egress_pre_start(instance_id, app_name, spec, cgroup_id)
                    .await
            }
        }
    }

    /// A build without the eBPF data path cannot enforce an allowlist.
    #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
    async fn apply_egress_pre_start(
        &mut self,
        _instance_id: &InstanceId,
        app_name: &str,
        spec: &AppSpec,
        _cgroup_path: &std::path::Path,
    ) -> Result<(), BunError> {
        if spec.egress.as_ref().is_some_and(|e| !e.allow.is_empty()) {
            return Err(BunError::DeployFailed {
                app_name: app_name.to_string(),
                reason: "egress allowlist requires an eBPF-enabled binary".to_string(),
            });
        }
        Ok(())
    }

    /// The programming half of the pre-start path. Deploy-failure
    /// semantics: a transient DNS failure denies all egress and lets the
    /// instance start (the re-resolve loop fills the allowlist in later),
    /// but a programming or representation error fails the deploy — a
    /// workload must never start ahead of a policy we could not install.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    async fn program_egress_pre_start(
        &mut self,
        instance_id: &InstanceId,
        app_name: &str,
        spec: &AppSpec,
        cgroup_id: u64,
    ) -> Result<(), BunError> {
        use crate::sesame::egress;

        let Some(handle) = self.onion_ebpf.clone() else {
            return Err(BunError::DeployFailed {
                app_name: app_name.to_string(),
                reason: "eBPF disappeared before pre-start egress programming".to_string(),
            });
        };
        let Some(egress_spec) = spec.egress.as_ref().filter(|e| !e.allow.is_empty()) else {
            return Ok(());
        };

        // A previous life of this instance (crash restart) programmed a
        // different cgroup id — the directory was recreated. Scrub it.
        self.clear_egress(instance_id).await;

        let allow = egress_spec.allow.clone();
        let allow_for_resolve = allow.clone();
        let resolved = match tokio::task::spawn_blocking(move || {
            egress::resolve_egress_entries(&allow_for_resolve)
        })
        .await
        {
            Ok(Ok(entries)) => entries,
            Ok(Err(e)) => {
                eprintln!(
                    "sesame: egress resolution failed for {}; starting deny-all: {e}",
                    instance_id.0
                );
                return self
                    .deny_all_pre_start(&handle, instance_id, app_name, cgroup_id, allow)
                    .await;
            }
            Err(_) => {
                return self
                    .deny_all_pre_start(&handle, instance_id, app_name, cgroup_id, allow)
                    .await;
            }
        };

        // Representation errors (too many ports on one CIDR) are permanent
        // config problems, not transient failures: fail the deploy.
        let merged = match egress::merge_cidr_ports(&resolved) {
            Ok(merged) => merged,
            Err(e) => {
                return Err(BunError::DeployFailed {
                    app_name: app_name.to_string(),
                    reason: format!(
                        "egress allowlist for {} cannot be programmed: {e}",
                        instance_id.0
                    ),
                });
            }
        };

        let mut ebpf = handle.lock().await;
        // Cgroup ids are kernel inode numbers and can be recycled. Scrub every
        // old exact/CIDR entry before enabling this instance, otherwise a
        // stale allow from an unclean predecessor could survive into the new
        // policy (or into a DNS-failure deny-all start).
        if let Err(e) = egress::delete_cgroup_egress_state(&mut ebpf.bpf, cgroup_id) {
            return Err(BunError::DeployFailed {
                app_name: app_name.to_string(),
                reason: format!(
                    "could not scrub recycled cgroup state for {}: {e}",
                    instance_id.0
                ),
            });
        }
        if let Err(e) = egress::set_egress_enforced(&mut ebpf.bpf, cgroup_id) {
            return Err(BunError::DeployFailed {
                app_name: app_name.to_string(),
                reason: format!(
                    "could not enable egress enforcement for {}: {e}",
                    instance_id.0
                ),
            });
        }
        if let Err(e) =
            egress::write_egress_destinations(&mut ebpf.bpf, cgroup_id, &resolved, &merged)
        {
            // Fail the deploy and leave nothing half-programmed behind.
            let _ = egress::delete_cgroup_egress_state(&mut ebpf.bpf, cgroup_id);
            return Err(BunError::DeployFailed {
                app_name: app_name.to_string(),
                reason: format!("egress map programming failed for {}: {e}", instance_id.0),
            });
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
        Ok(())
    }

    /// Pre-start deny-all: enforcement on, no allow entries. Unlike the
    /// post-start variant, a failure here fails the deploy — the process
    /// has not started yet, so refusing is still possible.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    async fn deny_all_pre_start(
        &mut self,
        handle: &std::sync::Arc<tokio::sync::Mutex<crate::onion::ebpf::loader::OnionEbpf>>,
        instance_id: &InstanceId,
        app_name: &str,
        cgroup_id: u64,
        allow: Vec<String>,
    ) -> Result<(), BunError> {
        {
            let mut ebpf = handle.lock().await;
            if let Err(e) =
                crate::sesame::egress::delete_cgroup_egress_state(&mut ebpf.bpf, cgroup_id)
            {
                return Err(BunError::DeployFailed {
                    app_name: app_name.to_string(),
                    reason: format!(
                        "could not scrub recycled cgroup state for {}: {e}",
                        instance_id.0
                    ),
                });
            }
            if let Err(e) = crate::sesame::egress::set_egress_enforced(&mut ebpf.bpf, cgroup_id) {
                return Err(BunError::DeployFailed {
                    app_name: app_name.to_string(),
                    reason: format!(
                        "could not enable egress enforcement for {}: {e}",
                        instance_id.0
                    ),
                });
            }
        }
        self.egress_bindings.insert(
            instance_id.clone(),
            EgressBinding {
                cgroup_id,
                allow,
                resolved: Vec::new(),
            },
        );
        Ok(())
    }

    /// Lift egress enforcement for a stopped instance's cgroup (L16).
    ///
    /// Deletes the allow entries as well as the enable flag: cgroup ids are
    /// recycled by the kernel, and a stale allowlist left behind could open
    /// destinations for whatever workload next lands on that cgroup id (NET6).
    /// Goes through `reprogram_cgroup_egress` because instances can share a
    /// cgroup path — deleting one instance's entries directly would wipe a
    /// co-tenant's policy.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    async fn clear_egress(&mut self, instance_id: &InstanceId) {
        let Some(binding) = self.egress_bindings.remove(instance_id) else {
            return;
        };
        self.reprogram_cgroup_egress(binding.cgroup_id).await;
    }

    /// No-op without the eBPF data path.
    #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
    async fn clear_egress(&mut self, _instance_id: &InstanceId) {}

    /// Rebuild the kernel egress state for one cgroup id from the current
    /// bindings: delete every entry for the cgroup, then write the union
    /// of what the surviving bindings allow (and the enforcement flag).
    /// With no surviving binding the cgroup is scrubbed completely.
    /// Failures leave the cgroup denying more than intended, never less.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    async fn reprogram_cgroup_egress(&mut self, cgroup_id: u64) {
        use crate::sesame::egress;

        let Some(handle) = self.onion_ebpf.clone() else {
            return;
        };
        let union: Vec<egress::EgressDestination> = {
            let mut set = std::collections::BTreeSet::new();
            for binding in self
                .egress_bindings
                .values()
                .filter(|b| b.cgroup_id == cgroup_id)
            {
                set.extend(binding.resolved.iter().copied());
            }
            set.into_iter().collect()
        };
        let survivors = self
            .egress_bindings
            .values()
            .any(|b| b.cgroup_id == cgroup_id);

        let mut ebpf = handle.lock().await;
        if !survivors {
            if let Err(e) = egress::delete_cgroup_egress_state(&mut ebpf.bpf, cgroup_id) {
                eprintln!("sesame: could not scrub egress state for cgroup {cgroup_id}: {e}");
            }
            return;
        }

        if let Err(e) = egress::delete_cgroup_egress_entries(&mut ebpf.bpf, cgroup_id) {
            eprintln!("sesame: could not clear egress entries for cgroup {cgroup_id}: {e}");
        }
        match egress::merge_cidr_ports(&union) {
            Ok(merged) => {
                if let Err(e) =
                    egress::write_egress_destinations(&mut ebpf.bpf, cgroup_id, &union, &merged)
                {
                    eprintln!(
                        "sesame: egress rewrite failed for cgroup {cgroup_id} \
                         (unwritten destinations stay denied): {e}"
                    );
                }
            }
            Err(e) => {
                eprintln!("sesame: egress CIDR merge failed for cgroup {cgroup_id}: {e}");
            }
        }
        if let Err(e) = egress::set_egress_enforced(&mut ebpf.bpf, cgroup_id) {
            eprintln!("sesame: could not re-enable egress enforcement for cgroup {cgroup_id}: {e}");
        }
    }

    /// Verify the security boundary on every event-loop tick. Map drift gets
    /// one immediate repair attempt. If any required hook is gone, the map can't be
    /// read, or a repaired enforcement flag is still absent, stop every
    /// affected workload. Keeping it running would turn its allowlist into a
    /// label rather than a control.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    async fn enforce_live_egress_or_stop(&mut self) {
        use crate::sesame::egress;

        let unbound: std::collections::HashSet<InstanceId> = self
            .supervisor
            .list_instances()
            .iter()
            .filter(|instance| {
                !matches!(
                    instance.state,
                    ContainerState::Stopped | ContainerState::Failed
                )
            })
            .filter(|instance| !self.egress_bindings.contains_key(&instance.id))
            .filter(|instance| {
                self.deployed_specs
                    .get(&(instance.app_name.clone(), instance.namespace.clone()))
                    .and_then(|spec| spec.egress.as_ref())
                    .is_some_and(|policy| {
                        !policy.allow.is_empty() || !policy.allow_franchise.is_empty()
                    })
            })
            .map(|instance| instance.id.clone())
            .collect();
        let Some(handle) = self.onion_ebpf.clone() else {
            self.supervisor.set_egress_capability(Default::default());
            let mut affected: std::collections::HashSet<InstanceId> =
                self.egress_bindings.keys().cloned().collect();
            affected.extend(unbound);
            self.stop_instances_after_egress_loss(affected).await;
            return;
        };
        let expected: std::collections::HashSet<u64> =
            self.egress_bindings.values().map(|b| b.cgroup_id).collect();
        let (capability, kernel_enforced) = {
            let mut ebpf = handle.lock().await;
            let capability = egress::EgressEnforcementCapability {
                connect_ipv4: ebpf.is_attached(),
                connect_ipv6: ebpf.connect6_attached(),
                udp_ipv4: ebpf.sendmsg4_attached(),
                udp_ipv6: ebpf.sendmsg6_attached(),
                pre_start: self.supervisor.grill().honours_cgroup_path(),
            };
            let enforced = egress::list_enforced_cgroups(&mut ebpf.bpf).unwrap_or_default();
            (capability, enforced)
        };
        self.supervisor.set_egress_capability(capability);
        if expected.is_empty() && unbound.is_empty() {
            if capability.can_enforce_allowlist() {
                self.egress_affected_workloads.clear();
            }
            return;
        }

        let plan = egress::plan_live_egress_health(capability, &expected, &kernel_enforced);
        for cgroup_id in &plan.repair {
            eprintln!("sesame: live check restoring egress enforcement for cgroup {cgroup_id}");
            self.reprogram_cgroup_egress(*cgroup_id).await;
        }

        let mut fence: std::collections::HashSet<u64> = plan.fence.into_iter().collect();
        if capability.can_enforce_allowlist() && !plan.repair.is_empty() {
            let verified = {
                let mut ebpf = handle.lock().await;
                egress::list_enforced_cgroups(&mut ebpf.bpf).unwrap_or_default()
            };
            fence.extend(expected.difference(&verified).copied());
        }
        if fence.is_empty() && unbound.is_empty() {
            self.egress_affected_workloads.clear();
            return;
        }

        let mut affected_ids: std::collections::HashSet<InstanceId> = self
            .egress_bindings
            .iter()
            .filter(|(_, binding)| fence.contains(&binding.cgroup_id))
            .map(|(id, _)| id.clone())
            .collect();
        affected_ids.extend(unbound);
        self.stop_instances_after_egress_loss(affected_ids).await;
    }

    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    async fn stop_instances_after_egress_loss(
        &mut self,
        affected_ids: std::collections::HashSet<InstanceId>,
    ) {
        if affected_ids.is_empty() {
            return;
        }
        let affected_apps: std::collections::HashSet<(String, String)> = self
            .supervisor
            .list_instances()
            .iter()
            .filter(|instance| affected_ids.contains(&instance.id))
            .map(|instance| (instance.app_name.clone(), instance.namespace.clone()))
            .collect();
        self.egress_affected_workloads
            .extend(affected_apps.iter().cloned());
        for (app_name, namespace) in affected_apps {
            eprintln!("sesame: stopping {namespace}/{app_name}: live egress enforcement was lost");
            if let Err(error) = self.stop_app(&app_name, &namespace).await {
                eprintln!(
                    "sesame: failed to stop {namespace}/{app_name} after egress loss: {error}"
                );
            }
        }
    }

    /// Portable builds cannot have live egress bindings.
    #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
    async fn enforce_live_egress_or_stop(&mut self) {}

    /// Periodically re-resolve DNS-based egress allowlists and reprogram the
    /// eBPF egress maps when an app's destination IPs change (L16). Rate-
    /// limited to roughly once every five minutes; a no-op while nothing
    /// enforces egress.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    async fn reresolve_egress(&mut self) {
        // ~5 minutes at the 1s event-loop tick.
        const RERESOLVE_EVERY_TICKS: u32 = 300;
        self.egress_reresolve_ticks += 1;
        if self.egress_reresolve_ticks < RERESOLVE_EVERY_TICKS || self.egress_bindings.is_empty() {
            return;
        }
        self.egress_reresolve_ticks = 0;

        if self.onion_ebpf.is_none() {
            return;
        }
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

            // Record the new set, then rebuild the cgroup's kernel state
            // from all bindings — CIDR values are merged per cgroup, so a
            // delta write can't be applied entry by entry.
            if let Some(b) = self.egress_bindings.get_mut(&instance_id) {
                b.resolved = new_resolved;
            }
            self.reprogram_cgroup_egress(binding.cgroup_id).await;
        }
    }

    /// No-op without the eBPF data path.
    #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
    async fn reresolve_egress(&mut self) {}

    /// Reconcile kernel truth against live instances (the sweep half of the
    /// network-policy theme): scrub egress state whose cgroup no longer maps
    /// to a live instance, rewrite every live binding (idempotent repairs),
    /// and prune stale `cgroup_namespace_map` keys. The one-second live check
    /// fences adopted policy-bearing workloads with no trustworthy binding;
    /// the sweep never installs their policy after they have already run.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    async fn sweep_kernel_networking(&mut self) {
        use crate::sesame::egress;

        if self.ebpf_sweep_interval_secs == 0 || self.onion_ebpf.is_none() {
            return;
        }
        self.ebpf_sweep_ticks += 1;
        if self.ebpf_sweep_ticks < self.ebpf_sweep_interval_secs {
            return;
        }
        self.ebpf_sweep_ticks = 0;
        let Some(handle) = self.onion_ebpf.clone() else {
            return;
        };

        // 1. Live instances with an allowlist but no binding are an invariant
        //    violation, not a repair opportunity after process start. The
        //    one-second live check stops them; repeat the check here as
        //    defence in depth instead of installing a late policy.
        let missing: std::collections::HashSet<InstanceId> = self
            .supervisor
            .list_instances()
            .iter()
            .filter(|i| {
                !matches!(
                    i.state,
                    crate::grill::state::ContainerState::Stopped
                        | crate::grill::state::ContainerState::Failed
                )
            })
            .filter(|i| !self.egress_bindings.contains_key(&i.id))
            .filter_map(|i| {
                self.deployed_specs
                    .get(&(i.app_name.clone(), i.namespace.clone()))
                    .filter(|s| s.egress.as_ref().is_some_and(|e| !e.allow.is_empty()))
                    .map(|_| i.id.clone())
            })
            .collect();
        for id in &missing {
            eprintln!(
                "sesame: sweep found unbound egress policy for {}; fencing",
                id.0
            );
        }
        self.stop_instances_after_egress_loss(missing).await;

        // 2. Kernel truth vs expected cgroups.
        let expected: std::collections::HashSet<u64> =
            self.egress_bindings.values().map(|b| b.cgroup_id).collect();
        let (kernel_enforced, kernel_entries) = {
            let mut ebpf = handle.lock().await;
            let enforced = match egress::list_enforced_cgroups(&mut ebpf.bpf) {
                Ok(set) => set,
                Err(e) => {
                    eprintln!("sesame: sweep could not list enforced cgroups: {e}");
                    return;
                }
            };
            let entries = match egress::list_egress_entry_cgroups(&mut ebpf.bpf) {
                Ok(set) => set,
                Err(e) => {
                    eprintln!("sesame: sweep could not list egress entries: {e}");
                    return;
                }
            };
            (enforced, entries)
        };
        let plan = egress::plan_egress_sweep(&expected, &kernel_enforced, &kernel_entries);
        if !plan.stale.is_empty() {
            let mut ebpf = handle.lock().await;
            for cgroup_id in &plan.stale {
                eprintln!(
                    "sesame: sweep deleting kernel egress state for departed cgroup {cgroup_id}"
                );
                if let Err(e) = egress::delete_cgroup_egress_state(&mut ebpf.bpf, *cgroup_id) {
                    eprintln!("sesame: sweep scrub failed for cgroup {cgroup_id}: {e}");
                }
            }
        }
        for cgroup_id in &plan.repair {
            eprintln!("sesame: sweep restoring egress enforcement for cgroup {cgroup_id}");
        }
        // Rewrite every live cgroup's entries: idempotent inserts, and the
        // only way lost entries (as opposed to a lost flag) come back.
        let live_cgroups: std::collections::HashSet<u64> = expected;
        for cgroup_id in live_cgroups {
            self.reprogram_cgroup_egress(cgroup_id).await;
        }

        // 3. Namespace map: delete kernel keys no reconcile pass wrote,
        //    then rebuild the desired state.
        let desired_ns = self.cgroup_ns_bpf_keys.clone();
        {
            let mut ebpf = handle.lock().await;
            match crate::sesame::firewall::list_cgroup_namespace_keys(&mut ebpf.bpf) {
                Ok(kernel_ns) => {
                    for cgroup_id in kernel_ns.difference(&desired_ns) {
                        eprintln!(
                            "sesame: sweep deleting stale cgroup-namespace entry {cgroup_id}"
                        );
                        let _ = crate::sesame::firewall::delete_cgroup_namespace_entry(
                            &mut ebpf.bpf,
                            *cgroup_id,
                        );
                    }
                }
                Err(e) => {
                    eprintln!("sesame: sweep could not list cgroup-namespace keys: {e}");
                }
            }
        }
        self.sync_firewall_ebpf().await;
    }

    /// No-op without the eBPF data path.
    #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
    async fn sweep_kernel_networking(&mut self) {}

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
                // service so we can re-sync its eBPF backend_map entry afterwards.
                let mut health_changed_service: Option<crate::onion::service_id::ServiceId> = None;
                match &transition {
                    Ok(Some(ContainerState::Running)) => {
                        if let Some(inst) = self.supervisor.get_instance(&instance_id) {
                            let service_id = crate::onion::service_id::ServiceId::new(
                                inst.namespace.clone(),
                                inst.app_name.clone(),
                            );
                            let _ = self.service_map.set_backend_health(
                                &service_id,
                                &instance_id.0,
                                true,
                            );
                            health_changed_service = Some(service_id);
                        }
                    }
                    Ok(Some(ContainerState::Unhealthy)) => {
                        if let Some(inst) = self.supervisor.get_instance(&instance_id) {
                            let app = inst.app_name.clone();
                            let namespace = inst.namespace.clone();
                            let service_id = crate::onion::service_id::ServiceId::new(
                                namespace.clone(),
                                app.clone(),
                            );
                            let _ = self.service_map.set_backend_health(
                                &service_id,
                                &instance_id.0,
                                false,
                            );
                            self.record_event(
                                crate::bun::events::EventKind::Health,
                                crate::bun::events::EventSeverity::Warning,
                                Some(app),
                                Some(namespace),
                                format!("instance {} became unhealthy", instance_id.0),
                            )
                            .await;
                            health_changed_service = Some(service_id);
                        }
                    }
                    _ => {}
                }
                if let Some(service_id) = health_changed_service {
                    self.sync_backend_ebpf(&service_id).await;
                }

                // Handle restart if unhealthy
                if let Ok(Some(ContainerState::Unhealthy)) = transition
                    && self
                        .supervisor
                        .maybe_restart(&instance_id, now)
                        .await
                        .unwrap_or(false)
                    && let Some(instance) = self.supervisor.get_instance(&instance_id)
                {
                    self.record_event(
                        crate::bun::events::EventKind::Restart,
                        crate::bun::events::EventSeverity::Warning,
                        Some(instance.app_name.clone()),
                        Some(instance.namespace.clone()),
                        format!(
                            "instance {} restarted (attempt {})",
                            instance_id.0, instance.restart_count
                        ),
                    )
                    .await;
                }
            }

            // Schedule the next check
            self.supervisor
                .health_checker_mut()
                .schedule_next(instance_id, now);
        }
    }

    /// Register (or refresh) the cron-scheduled jobs from an applied config.
    ///
    /// A job with a `schedule` is not run at deploy time; it's parked here and
    /// fired by [`fire_due_jobs`](Self::fire_due_jobs) when its cron matches.
    /// Re-applying an unchanged schedule preserves its last-fired stamp so the
    /// same minute doesn't fire twice; a parse failure is logged and skipped.
    fn register_scheduled_jobs(&mut self, config: &Config) {
        for (name, spec) in &config.job {
            let Some(expression) = spec.schedule.as_deref() else {
                continue;
            };
            let namespace = spec
                .namespace
                .clone()
                .unwrap_or_else(|| "default".to_string());
            match crate::meat::cron::CronSchedule::parse(expression) {
                Ok(schedule) => {
                    let key = (name.clone(), namespace.clone());
                    let last_fired_minute = self
                        .scheduled_jobs
                        .get(&key)
                        .filter(|existing| existing.schedule == schedule)
                        .and_then(|existing| existing.last_fired_minute);
                    self.scheduled_jobs.insert(
                        key,
                        ScheduledJob {
                            name: name.clone(),
                            namespace,
                            schedule,
                            spec: spec.clone(),
                            last_fired_minute,
                        },
                    );
                }
                Err(error) => {
                    eprintln!("cron: job {name} has invalid schedule {expression:?}: {error}");
                }
            }
        }
    }

    /// Fire every scheduled job whose cron matches the current UTC minute.
    ///
    /// Called on the 1s event-loop tick, but a schedule only resolves to the
    /// minute, so each job fires at most once per matching minute (guarded by
    /// its epoch-minute stamp). Firing reuses the normal job deploy path with
    /// the `schedule` cleared, so the job actually runs this time.
    async fn fire_due_jobs(&mut self) {
        if self.scheduled_jobs.is_empty() {
            return;
        }
        let now = time::OffsetDateTime::now_utc();
        let minute_stamp = now.unix_timestamp().div_euclid(60);

        let mut due: Vec<(String, String, JobSpec)> = Vec::new();
        for job in self.scheduled_jobs.values_mut() {
            if job.last_fired_minute == Some(minute_stamp) {
                continue;
            }
            if job.schedule.matches(now) {
                job.last_fired_minute = Some(minute_stamp);
                let mut spec = job.spec.clone();
                spec.schedule = None;
                due.push((job.name.clone(), job.namespace.clone(), spec));
            }
        }

        for (name, namespace, spec) in due {
            self.record_event(
                crate::bun::events::EventKind::Deploy,
                crate::bun::events::EventSeverity::Info,
                Some(name.clone()),
                Some(namespace.clone()),
                format!("firing scheduled job {namespace}/{name}"),
            )
            .await;

            let mut config = Config::default();
            config.job.insert(name, spec);
            self.spawn_scheduled_job_deploy(config);
        }
    }

    /// Spawn a one-off deploy of a cron-fired job on its own task, draining the
    /// event stream. Mirrors the worker construction in the deploy command path.
    fn spawn_scheduled_job_deploy(&self, config: Config) {
        let (events_tx, mut events_rx) = mpsc::channel::<ApplyEvent>(64);
        tokio::spawn(async move { while events_rx.recv().await.is_some() {} });

        let worker = DeployWorker {
            grill: self.supervisor.grill().clone(),
            port_allocator: self.supervisor.port_allocator(),
            ops: DeployOps {
                tx: self.deploy_ops_tx.clone(),
            },
            operation: None,
        };
        tokio::spawn(async move {
            worker.run_deploy(config, events_tx).await;
        });
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
                    if let Some(instance) = self.supervisor.get_instance(&id) {
                        self.record_event(
                            crate::bun::events::EventKind::JobCompleted,
                            crate::bun::events::EventSeverity::Info,
                            Some(instance.app_name.clone()),
                            Some(instance.namespace.clone()),
                            format!("job {} completed", instance.app_name),
                        )
                        .await;
                    }
                    continue;
                }

                // Job failed — attempt restart
                match self.supervisor.maybe_restart(&id, now).await {
                    Ok(true) => {
                        // Now in Pending — drive_pending_restarts will handle it
                        if let Some(instance) = self.supervisor.get_instance(&id) {
                            self.record_event(
                                crate::bun::events::EventKind::Restart,
                                crate::bun::events::EventSeverity::Warning,
                                Some(instance.app_name.clone()),
                                Some(instance.namespace.clone()),
                                format!(
                                    "instance {} restarted (attempt {})",
                                    id.0, instance.restart_count
                                ),
                            )
                            .await;
                        }
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
                        if let Some(instance) = self.supervisor.get_instance(&id) {
                            self.record_event(
                                crate::bun::events::EventKind::JobFailed,
                                crate::bun::events::EventSeverity::Warning,
                                Some(instance.app_name.clone()),
                                Some(instance.namespace.clone()),
                                format!("job {} failed", instance.app_name),
                            )
                            .await;
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
                    if let Some(instance) = self.supervisor.get_instance(&id) {
                        self.record_event(
                            crate::bun::events::EventKind::Restart,
                            crate::bun::events::EventSeverity::Warning,
                            Some(instance.app_name.clone()),
                            Some(instance.namespace.clone()),
                            format!(
                                "instance {} restarted (attempt {})",
                                id.0, instance.restart_count
                            ),
                        )
                        .await;
                    }
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
                    if let Some(instance) = self.supervisor.get_instance(&id) {
                        self.record_event(
                            crate::bun::events::EventKind::JobFailed,
                            crate::bun::events::EventSeverity::Warning,
                            Some(instance.app_name.clone()),
                            Some(instance.namespace.clone()),
                            format!("job {} failed", instance.app_name),
                        )
                        .await;
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

            // Close the restart window too: the recreated cgroup gets its
            // egress programmed before start (the crash gave the instance a
            // fresh cgroup id). The AppSpec comes from the stored deploy
            // record, the cgroup path from the stored OCI spec. On failure
            // the created container is removed and the restart refused —
            // fail closed, same as a fresh deploy.
            let restart_egress = if let Some(spec) = self
                .deployed_specs
                .get(&(app_name.clone(), namespace.clone()))
                .cloned()
            {
                match oci_spec.linux.cgroups_path.as_deref() {
                    Some(cgroup_path) => {
                        self.apply_egress_pre_start(
                            &id,
                            &app_name,
                            &spec,
                            std::path::Path::new(cgroup_path),
                        )
                        .await
                    }
                    None if spec.egress.as_ref().is_some_and(|e| !e.allow.is_empty()) => {
                        Err(BunError::DeployFailed {
                            app_name: app_name.clone(),
                            reason: "restart has no cgroup path for pre-start egress programming"
                                .to_string(),
                        })
                    }
                    None => Ok(()),
                }
            } else {
                Ok(())
            };
            if let Err(e) = restart_egress {
                eprintln!("bun: restart of {} refused: {e}", id.0);
                let _ = self.supervisor.grill().stop(&id).await;
                if let Some(instance) = self.supervisor.get_instance_mut(&id)
                    && let Ok(state) = instance.state.transition_to(ContainerState::Failed)
                {
                    instance.state = state;
                }
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
            // A re-created container may get a fresh IP; refresh it before
            // registering the backend so routing points at the live address.
            let container_ip = self.supervisor.grill().container_ip(&id).await;
            if let Some(instance) = self.supervisor.get_instance_mut(&id) {
                instance.container_ip = container_ip;
            }
            let service_id = crate::onion::service_id::ServiceId::new(&namespace, &app_name);
            if let Some(port) = host_port {
                let backend = crate::onion::types::BackendInstance {
                    instance_id: id.0.clone(),
                    node_ip: container_ip.unwrap_or(std::net::Ipv4Addr::LOCALHOST),
                    host_port: port,
                    healthy: true,
                };
                if let Err(e) = self.service_map.add_backend(&service_id, backend) {
                    eprintln!("onion: backend not registered for {service_id:?}: {e}");
                }
            }
            // Egress was applied before `start` above. Post-start networking
            // only refreshes the service and namespace-firewall maps.
            self.finish_instance_networking(&app_name, &namespace).await;

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

        // Stop via supervisor (moves the tracked state to Stopping).
        self.supervisor.stop_app(app_name, namespace).await?;

        // DEP6: SIGTERM, wait for the runtime to confirm exit, escalate to
        // SIGKILL on timeout. Only then do we record Stopped. Recording it
        // before the process exits let container and supervisor state
        // diverge — a "stopped" app whose process was still serving traffic.
        for id in &instances {
            self.stop_and_wait_for_exit(id, std::time::Duration::from_secs(STOP_GRACE_SECS))
                .await;
        }

        // Transition Stopping → Stopped now the exit is confirmed.
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
        let service_id = crate::onion::service_id::ServiceId::new(namespace, app_name);
        for id in &instances {
            let _ = self.service_map.remove_backend(&service_id, &id.0);
            self.clear_egress(id).await;
            // Key material must not outlive the instance (PKI7): remove
            // the identity dir and unmount its tmpfs backing.
            self.cleanup_instance_identity(id);
        }
        self.remove_backend_ebpf(&service_id).await;
        let _ = self.service_map.unregister(&service_id);
        // NET5: prune this app's cgroup-namespace + firewall entries now it's
        // gone, so a reused cgroup inode can't inherit its isolation identity.
        self.sync_firewall_ebpf().await;
        self.ingress_configs
            .remove(&(namespace.to_string(), app_name.to_string()));
        self.rebuild_routing_table().await;

        self.record_event(
            crate::bun::events::EventKind::Stop,
            crate::bun::events::EventSeverity::Info,
            Some(app_name.to_string()),
            Some(namespace.to_string()),
            format!("stopped app {app_name}"),
        )
        .await;

        Ok(())
    }

    /// Rebuild the Wrapper routing table from the current service map
    /// and ingress configs.
    ///
    /// Resolution uses the *merged* view: the local service map overlaid
    /// with the replicated cluster catalogue (12b.4), so both DNS and the
    /// ingress routing table can reach services whose backends live on other
    /// nodes. The local map alone still drives eBPF backend-map syncing —
    /// this merge only affects what DNS/ingress resolve.
    async fn rebuild_routing_table(&self) {
        let merged = self.service_map.with_cluster_catalog(&self.cluster_catalog);

        let mut table = self.routing_table.write().await;
        // Invalid ingress configs (unsupported TLS mode, zero/overflow rate)
        // are rejected here: their routes are skipped rather than installed,
        // so a bad app can't serve TLS traffic in plaintext or divide by zero.
        if let Err(e) = table.rebuild(&merged, &self.ingress_configs) {
            eprintln!("wrapper: ingress routing rebuild rejected some routes: {e}");
        }
        drop(table);

        // Publish the merged service-map snapshot for out-of-loop readers (DNS).
        // send() only errs when no receiver exists, which is fine.
        let _ = self.service_map_tx.send(merged);
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

        let ruleset = match crate::firewall::rules::generate_ruleset(
            &self.perimeter_config,
            &cluster_nodes,
        ) {
            Ok(ruleset) => ruleset,
            Err(e) => {
                // A malformed admin CIDR never reaches nft (NET8); the
                // previous ruleset stays in force.
                eprintln!("warning: firewall ruleset generation failed: {e}");
                return;
            }
        };

        if let Err(e) = crate::firewall::rules::apply_ruleset(&ruleset).await {
            eprintln!("warning: firewall reconciliation failed: {e}");
        } else {
            self.last_firewall_nodes = Some(cluster_nodes);
        }
    }

    /// The per-instance identity directory (PKI7): keyed by instance id so
    /// replicas never share (or clobber) key material.
    fn instance_identity_dir(&self, instance_id: &InstanceId) -> std::path::PathBuf {
        crate::sesame::identity::instance_identity_dir(&self.volumes_dir, &instance_id.0)
    }

    /// The uid/gid identity files should be owned by, so the container
    /// process can read its owner-only key: the OCI runtime user (65534),
    /// but only when we're root and can actually chown. In rootless mode
    /// the files stay owned by the bun user — the same user namespace the
    /// workload runs in.
    fn workload_identity_owner() -> Option<(u32, u32)> {
        #[cfg(unix)]
        {
            nix::unistd::geteuid().is_root().then_some((65534, 65534))
        }
        #[cfg(not(unix))]
        {
            None
        }
    }

    /// Prepare an instance's identity directory before its container is
    /// created — the bind-mount source must exist, and on Linux root mode
    /// this is where the backing tmpfs gets mounted (PKI7).
    fn prepare_instance_identity(&self, instance_id: &InstanceId) -> Result<(), BunError> {
        let dir = self.instance_identity_dir(instance_id);
        crate::sesame::identity::prepare_identity_dir(&dir).map_err(|e| BunError::SecurityError {
            reason: format!("failed to prepare identity dir for {instance_id}: {e}"),
        })
    }

    /// Remove an instance's identity directory (and drop the in-memory
    /// identity), so key material never outlives the instance (PKI7).
    fn cleanup_instance_identity(&mut self, instance_id: &InstanceId) {
        let dir = self.instance_identity_dir(instance_id);
        if let Err(e) = crate::sesame::identity::cleanup_identity_dir(&dir) {
            eprintln!("bun: warning: failed to remove identity dir for {instance_id}: {e}");
        }
        if let Some(inst) = self.supervisor.get_instance_mut(instance_id) {
            inst.identity = None;
            inst.identity_mount = None;
        }
    }

    /// Remove identity directories that don't belong to any tracked
    /// instance. Runs once after adoption: legacy app-scoped directories
    /// and instances that died while bun was down both get swept, so
    /// stale key material never lingers (PKI7).
    fn sweep_orphaned_identity_dirs(&self) {
        let root = self.volumes_dir.join(".identity");
        let Ok(entries) = std::fs::read_dir(&root) else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let tracked = self
                .supervisor
                .get_instance(&InstanceId(name.clone()))
                .is_some();
            if tracked {
                continue;
            }
            if let Err(e) = crate::sesame::identity::cleanup_identity_dir(&entry.path()) {
                eprintln!("bun: warning: failed to sweep stale identity dir {name}: {e}");
            }
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

        let spiffe_uri =
            workload_spiffe_uri(&self.trust_domain, namespace, app_name, workload_type);

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
            .sign_workload_csr(
                &csr_der,
                &spiffe_uri,
                crate::sesame::identity::CertUsage::Mtls,
                &self.trust_domain,
                "local",
                &instance_id.0,
            )
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

                // Write to the instance's own identity mount (PKI7). The
                // dir was prepared before the container was created; a
                // rotation for an adopted instance may find it missing, so
                // prepare (idempotently) here too.
                let identity_dir = self.instance_identity_dir(instance_id);
                if let Err(e) = crate::sesame::identity::prepare_identity_dir(&identity_dir) {
                    let _ = events
                        .send(ApplyEvent::Progress {
                            message: format!("identity: failed to prepare directory: {e}"),
                        })
                        .await;
                    return;
                }
                if let Err(e) = crate::sesame::identity::write_identity_files(
                    &identity,
                    &identity_dir,
                    Self::workload_identity_owner(),
                ) {
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

    /// Issue a certificate bundle for a joining node.
    ///
    /// Runs on an existing cluster member. Validates the token against the
    /// replicated security state, consumes it via Raft, and returns the
    /// bundle (certificate, private key, CA chain) for the joiner to persist.
    /// The joiner supplies its own `node_id`.
    async fn handle_join_issue(
        &self,
        token: &str,
        node_id: &str,
        csr_der: &[u8],
    ) -> Result<crate::sesame::join::JoinBundle, BunError> {
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

        // Fast-fail check against the replicated state (unknown/expired/already
        // consumed token). The authoritative consume happens atomically below.
        let security_state = council.security_state().await;
        let token_hash = crate::sesame::join::check_join_token(token, node_id, &security_state)
            .map_err(|e| BunError::SecurityError {
                reason: format!("join validation failed: {e}"),
            })?;

        // Atomically consume the token and allocate a serial in one committed
        // Raft entry (PKI5). Two racing joiners with the same token: exactly one
        // gets a serial here; the loser is refused, so a token issues one cert.
        let serial = match council
            .write(crate::council::RaftRequest::ConsumeJoinTokenForIssue { token_hash })
            .await
            .map_err(|e| BunError::SecurityError {
                reason: format!("failed to consume join token: {e}"),
            })? {
            crate::council::CouncilResponse::JoinTokenConsumed { serial } => {
                crate::sesame::types::SerialNumber(serial)
            }
            crate::council::CouncilResponse::Refused { reason } => {
                return Err(BunError::SecurityError {
                    reason: format!("join refused: {reason}"),
                });
            }
            other => {
                return Err(BunError::SecurityError {
                    reason: format!("unexpected council response to join: {other:?}"),
                });
            }
        };

        // Re-read the state after the commit so the CA material reflects the
        // committed serial counter, then sign the joiner's CSR.
        let security_state = council.security_state().await;
        let join_result =
            crate::sesame::join::sign_join_csr(csr_der, node_id, serial, &security_state, ikm)
                .map_err(|e| BunError::SecurityError {
                    reason: format!("join signing failed: {e}"),
                })?;

        Ok(crate::sesame::join::JoinBundle::from_result(&join_result))
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

    /// Check identity rotation for all instances, and (rate-limited)
    /// provision identities for running instances that don't have one —
    /// a failed CSR at deploy time, or an adopted instance whose
    /// directory predates the per-instance layout, heals here (D9).
    async fn check_identity_rotation(&mut self) {
        let now = std::time::SystemTime::now();
        let mut needs_rotation = Vec::new();

        self.identity_retry_ticks += 1;
        let retry_missing = self.identity_retry_ticks >= IDENTITY_RETRY_TICKS;
        if retry_missing {
            self.identity_retry_ticks = 0;
        }

        for inst in self.supervisor.list_instances() {
            let Some(ref identity) = inst.identity else {
                // Apps only: job containers don't mount an identity dir.
                if retry_missing
                    && !inst.is_job
                    && inst.state == crate::grill::state::ContainerState::Running
                {
                    needs_rotation.push((
                        inst.id.clone(),
                        inst.app_name.clone(),
                        inst.namespace.clone(),
                        inst.is_job,
                    ));
                }
                continue;
            };
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

        // Re-provision identities that need rotation. `provision_identity` emits
        // best-effort progress events, but a background rotation tick has no SSE
        // consumer for them. The old code held a capacity-1 receiver it never
        // read, so the *second* send inside the *first* provision blocked the
        // agent loop forever (H2). Drop the receiver instead: each `send` now
        // fails fast (channel closed) and is swallowed, while the actual
        // CSR-signing and file writes proceed unchanged.
        let (dummy_tx, dummy_rx) = mpsc::channel(1);
        drop(dummy_rx);
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
            let exit_code = self.supervisor.grill().exit_code(&instance.id).await;
            statuses.push(InstanceStatus {
                id: instance.id.0.clone(),
                app_name: instance.app_name.clone(),
                namespace: instance.namespace.clone(),
                state: instance.state.to_string(),
                restart_count: instance.restart_count,
                host_port: instance.host_port,
                exit_code,
                pid,
            });
        }
        statuses
    }

    fn get_job_status(&self) -> Vec<JobStatus> {
        self.supervisor
            .list_instances()
            .into_iter()
            .filter(|instance| instance.is_job)
            .map(|instance| JobStatus {
                name: instance.app_name.clone(),
                namespace: instance.namespace.clone(),
                instance_id: instance.id.0.clone(),
                image: instance.image.clone(),
                state: instance.state.to_string(),
                restart_count: instance.restart_count,
                age_seconds: instance.created_at.elapsed().as_secs(),
            })
            .collect()
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
    /// Resolve the id of a running instance of `app_name` in `namespace`, or
    /// `AppNotFound`. Cheap and synchronous, so it runs on the command loop
    /// before the actual exec is spawned off it (H3).
    fn resolve_running_instance(
        &self,
        app_name: &str,
        namespace: &str,
    ) -> Result<InstanceId, BunError> {
        self.supervisor
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
            })
    }

    /// Capture the immutable state needed by a connectivity trace. The slow
    /// workload and kernel observations run later on a spawned task.
    fn prepare_trace(
        &self,
        request: crate::onion::trace::TraceRequest,
        internal_destination: bool,
        source_node: String,
    ) -> Result<PreparedTrace<G>, BunError> {
        if request.port == Some(0) {
            return Err(BunError::SecurityError {
                reason: "trace destination port must be between 1 and 65535".to_string(),
            });
        }
        let source_instance = self
            .supervisor
            .list_instances()
            .into_iter()
            .find(|instance| {
                instance.app_name == request.source
                    && instance.namespace == request.source_namespace
                    && instance.state == ContainerState::Running
            })
            .map(|instance| instance.id.clone())
            .ok_or_else(|| BunError::AppNotFound {
                app_name: request.source.clone(),
                namespace: request.source_namespace.clone(),
            })?;

        let service_id = crate::onion::service_id::ServiceId::new(
            &request.destination_namespace,
            &request.destination,
        );
        let merged_services = self.service_map.with_cluster_catalog(&self.cluster_catalog);
        let service = internal_destination
            .then(|| merged_services.resolve(&service_id).cloned())
            .flatten();
        let destination_port = request
            .port
            .or_else(|| service.as_ref().map(|entry| entry.port))
            .ok_or_else(|| BunError::SecurityError {
                reason: "external trace destination requires an explicit port".to_string(),
            })?;
        let dns_name = if internal_destination {
            format!(
                "{}.{}.internal",
                request.destination, request.destination_namespace
            )
        } else {
            request.destination.clone()
        };
        let expected_vip = service.as_ref().map(|entry| entry.vip.to_string());
        let permit = self
            .trace_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| BunError::TraceBusy)?;

        Ok(PreparedTrace {
            _permit: permit,
            shutdown: self.shutdown.clone(),
            grill: self.supervisor.grill().clone(),
            source_instance,
            request,
            internal_destination,
            source_node,
            service,
            destination_port,
            dns_name,
            expected_vip,
            #[cfg(all(feature = "ebpf", target_os = "linux"))]
            onion_ebpf: self.onion_ebpf.clone(),
        })
    }
}

impl<G: Grill + Clone + 'static> PreparedTrace<G> {
    /// Trace DNS, live service/firewall state and a TCP connection from one
    /// running workload. The command strings are fixed; request values are
    /// positional shell arguments and can never become shell syntax.
    async fn run(self) -> Result<crate::onion::trace::TraceResult, BunError> {
        use crate::onion::trace::TraceResult;

        let dns_probe = self
            .run_workload_trace_probe(
                &self.source_instance,
                trace_dns_command(&self.dns_name),
                "__RB_TRACE_DNS_STATUS__",
            )
            .await;
        let dns_step = trace_probe_step(
            1,
            "DNS query",
            &self.dns_name,
            dns_probe,
            self.expected_vip.as_deref(),
        );

        let service_step = self
            .trace_service_state(self.service.as_ref(), self.internal_destination)
            .await;
        let firewall_step = self
            .trace_firewall_state(
                &self.source_instance,
                self.service.as_ref(),
                self.internal_destination,
            )
            .await;

        let connect_host = self
            .expected_vip
            .as_deref()
            .unwrap_or(self.request.destination.as_str());
        let started = std::time::Instant::now();
        let tcp_probe = self
            .run_workload_trace_probe(
                &self.source_instance,
                trace_tcp_command(connect_host, self.destination_port),
                "__RB_TRACE_TCP_STATUS__",
            )
            .await;
        let tcp_succeeded = tcp_probe.as_ref().is_ok_and(|probe| probe.status == 0);
        let tcp_step = trace_probe_step(
            4,
            "TCP probe",
            &format!("{connect_host}:{}", self.destination_port),
            tcp_probe,
            None,
        );
        let latency_ms = tcp_succeeded.then(|| started.elapsed().as_secs_f64() * 1000.0);

        let steps = vec![dns_step, service_step, firewall_step, tcp_step];
        let overall_result = crate::onion::trace::overall_verdict(&steps);
        Ok(TraceResult {
            schema_version: crate::onion::trace::TRACE_SCHEMA_VERSION,
            source: format!("{}/{}", self.request.source_namespace, self.request.source),
            destination: if self.internal_destination {
                format!(
                    "{}/{}",
                    self.request.destination_namespace, self.request.destination
                )
            } else {
                self.request.destination.clone()
            },
            destination_port: self.destination_port,
            source_node: self.source_node,
            steps,
            overall_result,
            latency_ms,
        })
    }

    async fn run_workload_trace_probe(
        &self,
        source_instance: &InstanceId,
        command: Vec<String>,
        marker: &str,
    ) -> Result<crate::onion::trace::ProbeOutput, String> {
        let future = self.grill.exec(source_instance, &command);
        let result = tokio::select! {
            _ = self.shutdown.cancelled() => {
                return Err("workload probe cancelled because the agent is shutting down".to_string());
            }
            result = tokio::time::timeout(std::time::Duration::from_secs(8), future) => result,
        };
        match result {
            Ok(Ok(output)) => crate::onion::trace::parse_probe_output(&output, marker)
                .ok_or_else(|| "source image lacks a usable POSIX shell or probe tool".to_string()),
            Ok(Err(error)) => Err(format!("workload probe could not start: {error}")),
            Err(_) => Err("workload probe timed out after 8 seconds".to_string()),
        }
    }

    async fn trace_service_state(
        &self,
        service: Option<&crate::onion::types::ServiceEntry>,
        internal_destination: bool,
    ) -> crate::onion::trace::TraceStep {
        use crate::onion::trace::{TraceEvidence, TraceStep, TraceVerdict};
        if !internal_destination {
            return TraceStep {
                step_number: 2,
                name: "Service and eBPF state".to_string(),
                evidence: TraceEvidence::Inferred,
                details: vec![
                    "external destinations bypass the internal service and backend maps"
                        .to_string(),
                ],
                verdict: TraceVerdict::Pass,
            };
        }
        let Some(service) = service else {
            return TraceStep {
                step_number: 2,
                name: "Service and eBPF state".to_string(),
                evidence: TraceEvidence::Observed,
                details: Vec::new(),
                verdict: TraceVerdict::Fail {
                    reason: "destination is absent from the live userspace service map".to_string(),
                },
            };
        };
        let healthy = service
            .backends
            .iter()
            .filter(|backend| backend.healthy)
            .count();
        let mut details = vec![format!(
            "userspace service map: VIP {}, {} of {} backends healthy",
            service.vip,
            healthy,
            service.backends.len()
        )];
        if healthy == 0 {
            return TraceStep {
                step_number: 2,
                name: "Service and eBPF state".to_string(),
                evidence: TraceEvidence::Observed,
                details,
                verdict: TraceVerdict::Fail {
                    reason: "live service state has no healthy backend".to_string(),
                },
            };
        }

        #[cfg(all(feature = "ebpf", target_os = "linux"))]
        if let Some(handle) = &self.onion_ebpf {
            let bpf_map = crate::onion::ebpf::maps::BpfServiceMap::new();
            let mut ebpf = handle.lock().await;
            return match bpf_map.read_backends(&mut ebpf, service.vip, service.port) {
                Ok(Some(value)) => {
                    let kernel_healthy = value
                        .backends
                        .iter()
                        .take(value.count as usize)
                        .filter(|backend| backend.healthy == 1)
                        .count();
                    details.push(format!(
                        "live backend_map: {} entries, {kernel_healthy} healthy",
                        value.count
                    ));
                    let verdict = if value.count == 0 || kernel_healthy == 0 {
                        TraceVerdict::Fail {
                            reason: "live eBPF backend map has no healthy backend".to_string(),
                        }
                    } else {
                        TraceVerdict::Pass
                    };
                    TraceStep {
                        step_number: 2,
                        name: "Service and eBPF state".to_string(),
                        evidence: TraceEvidence::Observed,
                        details,
                        verdict,
                    }
                }
                Ok(None) => TraceStep {
                    step_number: 2,
                    name: "Service and eBPF state".to_string(),
                    evidence: TraceEvidence::Observed,
                    details,
                    verdict: TraceVerdict::Fail {
                        reason: "service exists in userspace but is absent from live backend_map"
                            .to_string(),
                    },
                },
                Err(error) => TraceStep {
                    step_number: 2,
                    name: "Service and eBPF state".to_string(),
                    evidence: TraceEvidence::Unavailable,
                    details,
                    verdict: TraceVerdict::Unknown {
                        reason: format!("live backend_map could not be read: {error}"),
                    },
                },
            };
        }

        details.push(
            "no live eBPF backend map is attached; this step is inferred from userspace state"
                .to_string(),
        );
        TraceStep {
            step_number: 2,
            name: "Service and eBPF state".to_string(),
            evidence: TraceEvidence::Inferred,
            details,
            verdict: TraceVerdict::Pass,
        }
    }

    async fn trace_firewall_state(
        &self,
        source_instance: &InstanceId,
        service: Option<&crate::onion::types::ServiceEntry>,
        internal_destination: bool,
    ) -> crate::onion::trace::TraceStep {
        use crate::onion::trace::{TraceEvidence, TraceStep, TraceVerdict};
        let unknown = |reason: String| TraceStep {
            step_number: 3,
            name: "Firewall state".to_string(),
            evidence: TraceEvidence::Unavailable,
            details: Vec::new(),
            verdict: TraceVerdict::Unknown { reason },
        };

        #[cfg(all(feature = "ebpf", target_os = "linux"))]
        if let Some(handle) = &self.onion_ebpf {
            let Some(pid) = self.grill.pid(source_instance).await else {
                return unknown("runtime does not expose the source workload PID".to_string());
            };
            let Some(cgroup_id) = crate::sesame::egress::cgroup_id_of_pid(pid) else {
                return unknown("source workload cgroup id could not be resolved".to_string());
            };
            let mut ebpf = handle.lock().await;
            if !internal_destination {
                return match crate::sesame::egress::egress_enforced(&mut ebpf.bpf, cgroup_id) {
                    Ok(false) => TraceStep {
                        step_number: 3,
                        name: "Firewall state".to_string(),
                        evidence: TraceEvidence::Observed,
                        details: vec![
                            "live egress_enabled_map has no policy for the source cgroup; external traffic passes through".to_string(),
                        ],
                        verdict: TraceVerdict::Pass,
                    },
                    Ok(true) => unknown(
                        "live egress enforcement is active; the exact hostname decision is observed by the TCP probe but cannot yet be attributed to one exact/CIDR map entry"
                            .to_string(),
                    ),
                    Err(error) => unknown(format!("live egress map could not be read: {error}")),
                };
            }
            let Some(service) = service else {
                return unknown("destination service state is unavailable".to_string());
            };
            return match crate::sesame::firewall::read_firewall_state(
                &mut ebpf.bpf,
                cgroup_id,
                service.app_id,
            ) {
                Ok(state) => {
                    let verdict = crate::onion::trace::evaluate_firewall(
                        state.source_namespace_id,
                        service.namespace_id,
                        state.action,
                    );
                    TraceStep {
                        step_number: 3,
                        name: "Firewall state".to_string(),
                        evidence: TraceEvidence::Observed,
                        details: vec![format!(
                            "live maps: source cgroup {cgroup_id}, source namespace {:?}, destination namespace {}, action {:?}",
                            state.source_namespace_id, service.namespace_id, state.action
                        )],
                        verdict,
                    }
                }
                Err(error) => unknown(format!("live firewall maps could not be read: {error}")),
            };
        }

        let _ = (source_instance, service, internal_destination);
        unknown("no live eBPF firewall maps are attached on this node".to_string())
    }
}

impl<G: Grill + Clone + 'static> BunAgent<G> {
    /// Stop one instance and wait for it to actually exit (DEP6).
    ///
    /// SIGTERM first, then poll the runtime until the container reports
    /// `Stopped` or `grace` elapses, then SIGKILL whatever is still running.
    /// Returns only once the runtime confirms the exit (or the kill lands),
    /// so the supervisor never records `Stopped` for a process that is still
    /// alive — container and supervisor state stay in step.
    async fn stop_and_wait_for_exit(&self, id: &InstanceId, grace: std::time::Duration) {
        let _ = self.supervisor.grill().stop(id).await;
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            if matches!(
                self.supervisor.grill().state(id).await,
                Ok(ContainerState::Stopped)
            ) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        if !matches!(
            self.supervisor.grill().state(id).await,
            Ok(ContainerState::Stopped)
        ) {
            let _ = self.supervisor.grill().kill(id).await;
        }
    }

    /// Add one freshly-healthy replacement to the service map and rebuild the
    /// routing table, so traffic moves onto it before anything old retires (M7).
    async fn publish_new_backend(
        &mut self,
        app_name: &str,
        namespace: &str,
        new_id: &InstanceId,
        host_port: Option<u16>,
        container_ip: Option<std::net::Ipv4Addr>,
        has_port: bool,
    ) {
        if !has_port {
            return;
        }
        let Some(host_port) = host_port else {
            return;
        };
        let service_id = crate::onion::service_id::ServiceId::new(namespace, app_name);
        let backend = crate::onion::types::BackendInstance {
            instance_id: new_id.0.clone(),
            node_ip: container_ip.unwrap_or(std::net::Ipv4Addr::LOCALHOST),
            host_port,
            healthy: true,
        };
        if let Err(e) = self.service_map.add_backend(&service_id, backend) {
            eprintln!("onion: backend not registered for {service_id:?}: {e}");
        }
        self.rebuild_routing_table().await;
    }

    /// Drain, stop and forget one old instance (M7).
    ///
    /// The per-instance half of what `finalise_rolling_deploy` used to do to
    /// every old instance at once. Interleaving it with replacement is what
    /// gives `max_surge` and `max_unavailable` their meaning.
    async fn retire_one_old_instance(
        &mut self,
        old_id: &InstanceId,
        drain_timeout: std::time::Duration,
    ) {
        self.retire_with_drain(std::slice::from_ref(old_id), drain_timeout)
            .await;
        // NET6: lift the retiring instance's egress enforcement.
        self.clear_egress(old_id).await;
        self.cleanup_instance_identity(old_id);
        self.remove_instance_record(old_id);
        // Drop it from the supervisor in this same turn. A stopped instance
        // still listed there looks exactly like a crashed one to the restart
        // driver, which gets a chance to run between two retirement ops.
        self.supervisor.retire_instance(old_id).await;
    }

    /// Retire a set of old instances with connection draining (DEP5).
    ///
    /// New traffic is already routed away (the caller rebuilds the routing
    /// table before this). For each instance we start a drain, let in-flight
    /// requests finish (up to `drain_timeout`), then stop-and-wait-for-exit.
    /// The Wrapper proxy shares this agent's drain tracker, so the wait
    /// reflects real in-flight HTTP/WebSocket traffic.
    async fn retire_with_drain(&self, ids: &[InstanceId], drain_timeout: std::time::Duration) {
        for id in ids {
            let cmd = crate::wrapper::draining::DrainCommand {
                app_name: String::new(),
                instance_id: id.0.clone(),
                timeout: drain_timeout,
            };
            self.drains.start_drain(&cmd).await;
        }
        for id in ids {
            self.drains.wait_drained(&id.0).await;
            self.stop_and_wait_for_exit(id, drain_timeout).await;
        }
    }

    /// Gracefully stop all instances.
    async fn shutdown_all(&mut self) {
        // Reverse every owned fault before the process goes away. The
        // node-pressure helper also has PR_SET_PDEATHSIG and startup sweeping
        // for crash recovery, but graceful shutdown should leave no helper or
        // cgroup behind in the first place.
        let faults = self.fault_registry.clear();
        for rule in &faults {
            self.delete_fault_bpf_entry(rule).await;
            self.reverse_fault(rule).await;
        }
        self.publish_dns_faults();

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

    /// Apply one deploy op from a spawned deploy task. This is where the
    /// supervisor state machine stays authoritative: the task owns the
    /// blocking grill I/O, but every state transition and every mutation of
    /// supervisor / service-map / networking state happens here, on the loop
    /// (DEP4/codex-M3).
    async fn handle_deploy_op(&mut self, op: DeployOp) {
        match op {
            DeployOp::EnforceImageSignature { spec, reply } => {
                let _ = reply.send(self.enforce_image_signature(&spec).await);
            }
            DeployOp::StoreDeployedSpec {
                app_name,
                namespace,
                spec,
                reply,
            } => {
                self.deployed_specs.insert((app_name, namespace), *spec);
                let _ = reply.send(());
            }
            DeployOp::ListExistingActive {
                app_name,
                namespace,
                reply,
            } => {
                let ids = self
                    .supervisor
                    .list_instances()
                    .iter()
                    .filter(|i| i.app_name == app_name && i.namespace == namespace)
                    .filter(|i| {
                        !matches!(
                            i.state,
                            crate::grill::state::ContainerState::Stopped
                                | crate::grill::state::ContainerState::Failed
                        )
                    })
                    .map(|i| i.id.clone())
                    .collect();
                let _ = reply.send(ids);
            }
            DeployOp::NextDeployGen { reply } => {
                let deploy_gen = self.next_deploy_gen;
                self.next_deploy_gen += 1;
                let _ = reply.send(deploy_gen);
            }
            DeployOp::SupervisorDeployApp {
                app_name,
                namespace,
                spec,
                reply,
            } => {
                let now = Instant::now();
                let result = self
                    .supervisor
                    .deploy_app(&app_name, &namespace, &spec, now)
                    .await;
                let _ = reply.send(result);
            }
            DeployOp::SupervisorDeployJob {
                job_name,
                namespace,
                spec,
                reply,
            } => {
                let now = Instant::now();
                let result = self
                    .supervisor
                    .deploy_job(&job_name, &namespace, &spec, now)
                    .await;
                let _ = reply.send(result);
            }
            DeployOp::RegisterServiceApp {
                app_name,
                namespace,
                port,
                firewall,
                reply,
            } => {
                let service_id = crate::onion::service_id::ServiceId::new(&namespace, &app_name);
                let _ = self.service_map.register(&service_id, port, firewall);
                self.sync_backend_ebpf(&service_id).await;
                self.sync_firewall_ebpf().await;
                let _ = reply.send(());
            }
            DeployOp::StoreIngress {
                app_name,
                namespace,
                ingress,
                reply,
            } => {
                self.ingress_configs.insert((namespace, app_name), *ingress);
                let _ = reply.send(());
            }
            DeployOp::PrepareFreshInstance {
                instance_id,
                app_name,
                namespace,
                spec,
                reply,
            } => {
                let result = self
                    .prepare_fresh_instance(&instance_id, &app_name, &namespace, &spec)
                    .await;
                let _ = reply.send(result);
            }
            DeployOp::StoreOciSpec {
                instance_id,
                oci_spec,
                reply,
            } => {
                if let Some(instance) = self.supervisor.get_instance_mut(&instance_id) {
                    instance.oci_spec = Some(*oci_spec);
                }
                let _ = reply.send(());
            }
            DeployOp::ApplyEgressPreStart {
                instance_id,
                app_name,
                spec,
                cgroup_path,
                reply,
            } => {
                let result = self
                    .apply_egress_pre_start(&instance_id, &app_name, &spec, &cgroup_path)
                    .await;
                // On failure, mirror the fresh path's clean-up: mark Failed and
                // stop the created container so no half-started workload lingers.
                if result.is_err() {
                    if let Some(instance) = self.supervisor.get_instance_mut(&instance_id)
                        && let Ok(state) = instance.state.transition_to(ContainerState::Failed)
                    {
                        instance.state = state;
                    }
                    let _ = self.supervisor.grill().stop(&instance_id).await;
                }
                let _ = reply.send(result);
            }
            DeployOp::TransitionState {
                instance_id,
                to,
                reply,
            } => {
                let result = match self.supervisor.get_instance_mut(&instance_id) {
                    Some(instance) => match instance.state.transition_to(to) {
                        Ok(state) => {
                            instance.state = state;
                            Ok(())
                        }
                        Err(e) => Err(BunError::from(e)),
                    },
                    None => Err(BunError::InstanceNotFound { instance_id }),
                };
                let _ = reply.send(result);
            }
            DeployOp::FinishFreshInstance {
                instance_id,
                app_name,
                namespace,
                container_ip,
                reply,
            } => {
                let result = self
                    .finish_fresh_instance(&instance_id, &app_name, &namespace, container_ip)
                    .await;
                let _ = reply.send(result);
            }
            DeployOp::ProvisionIdentity {
                app_name,
                namespace,
                instance_id,
                is_job,
                reply,
            } => {
                // A no-op in standalone mode; a failure here is retried by the
                // rotation loop rather than failing the deploy. The progress
                // events it emits are dropped: the deploy already completed by
                // the time identity provisioning runs. The sink is buffered
                // wide enough (and provision emits only a handful of events),
                // so provisioning never blocks on it; the drain then discards
                // whatever it wrote.
                let (sink, mut drain) = mpsc::channel(64);
                self.provision_identity(&app_name, &namespace, &instance_id, is_job, &sink)
                    .await;
                drop(sink);
                while drain.recv().await.is_some() {}
                let _ = reply.send(());
            }
            DeployOp::PrepareRollingInstance {
                instance_id,
                app_name,
                namespace,
                spec,
                host_port,
                index,
                reply,
            } => {
                let result = self
                    .prepare_rolling_instance(
                        &instance_id,
                        &app_name,
                        &namespace,
                        &spec,
                        host_port,
                        index,
                    )
                    .await;
                let _ = reply.send(result);
            }
            DeployOp::ClearEgress { instance_id, reply } => {
                self.clear_egress(&instance_id).await;
                let _ = reply.send(());
            }
            DeployOp::RegisterRollingInstance {
                instance_id,
                app_name,
                namespace,
                reply,
            } => {
                self.spawn_log_forwarder(&instance_id, &app_name, &namespace);
                self.persist_instance_record(&instance_id).await;
                let _ = reply.send(());
            }
            DeployOp::RollbackRollingDeploy {
                app_name,
                namespace,
                spec,
                new_ids,
                new_prepared,
                new_ports,
                replica_count,
                reply,
            } => {
                self.rollback_rolling_deploy(
                    &app_name,
                    &namespace,
                    &spec,
                    &new_ids,
                    &new_prepared,
                    &new_ports,
                    replica_count,
                )
                .await;
                let _ = reply.send(());
            }
            DeployOp::HaltRollingDeploy {
                app_name,
                namespace,
                spec,
                new_ids,
                new_prepared,
                new_ports,
                replica_count,
                reply,
            } => {
                self.halt_rolling_deploy(
                    &app_name,
                    &namespace,
                    &spec,
                    &new_ids,
                    &new_prepared,
                    &new_ports,
                    replica_count,
                )
                .await;
                let _ = reply.send(());
            }
            DeployOp::FinaliseRollingDeploy {
                app_name,
                namespace,
                spec,
                existing,
                new_ids,
                new_ports,
                new_ips,
                new_specs,
                now,
                reply,
            } => {
                self.finalise_rolling_deploy(
                    &app_name, &namespace, &spec, &existing, &new_ids, &new_ports, &new_ips,
                    new_specs, now,
                )
                .await;
                let _ = reply.send(());
            }
            DeployOp::PublishNewBackend {
                app_name,
                namespace,
                new_id,
                host_port,
                container_ip,
                has_port,
                reply,
            } => {
                self.publish_new_backend(
                    &app_name,
                    &namespace,
                    &new_id,
                    host_port,
                    container_ip,
                    has_port,
                )
                .await;
                let _ = reply.send(());
            }
            DeployOp::RetireOldInstance {
                old_id,
                drain_timeout,
                reply,
            } => {
                self.retire_one_old_instance(&old_id, drain_timeout).await;
                let _ = reply.send(());
            }
            DeployOp::PushDeployHistory { entry, reply } => {
                self.deploy_history.write().await.push(*entry);
                let _ = reply.send(());
            }
            DeployOp::FinishJobInstance {
                instance_id,
                job_name,
                namespace,
                oci_spec,
                reply,
            } => {
                let result = self
                    .finish_job_instance(&instance_id, &job_name, &namespace, *oci_spec)
                    .await;
                let _ = reply.send(result);
            }
            DeployOp::RebuildRoutingTable { reply } => {
                self.rebuild_routing_table().await;
                let _ = reply.send(());
            }
            DeployOp::RecordDeployedEvent {
                app_name,
                namespace,
                reply,
            } => {
                let count = self
                    .supervisor
                    .list_instances()
                    .iter()
                    .filter(|i| i.app_name == app_name && i.namespace == namespace)
                    .count();
                self.record_event(
                    crate::bun::events::EventKind::Deploy,
                    crate::bun::events::EventSeverity::Info,
                    Some(app_name.clone()),
                    Some(namespace),
                    format!("deployed app {app_name} ({count} instances)"),
                )
                .await;
                let _ = reply.send(());
            }
        }
    }
}

/// Runs one deploy on its own spawned task so the command loop keeps
/// servicing health checks, restarts and other commands while an image pulls
/// or a rolling deploy waits on health (DEP4/codex-M3).
///
/// The worker owns the blocking grill I/O — create (the image pull), start,
/// init-container polling, and the rolling health wait — but not the
/// supervisor state machine. Every authoritative mutation travels back to the
/// loop as a `DeployOp` through `ops`, so the loop stays the single owner of
/// supervisor / service-map / networking state.
struct DeployWorker<G: Grill> {
    grill: G,
    port_allocator: PortAllocator,
    ops: DeployOps,
    operation: Option<crate::bun::deploy_operations::DeployOperationHandle>,
}

impl<G: Grill + Clone + 'static> DeployWorker<G> {
    /// Deploy all apps and jobs from a config, streaming progress events. The
    /// mirror of the former `BunAgent::deploy`, but off the command loop.
    async fn run_deploy(self, config: Config, events: mpsc::Sender<ApplyEvent>) {
        let now = Instant::now();
        let mut all_ids: Vec<String> = Vec::new();
        // Jobs already run as `run_before` prerequisites, so the regular jobs
        // loop below doesn't run them a second time.
        let mut ran_prereqs: std::collections::HashSet<String> = std::collections::HashSet::new();
        let deployed_apps: Vec<(String, String)> = config
            .app
            .iter()
            .map(|(name, spec)| {
                (
                    name.clone(),
                    spec.namespace
                        .clone()
                        .unwrap_or_else(|| "default".to_string()),
                )
            })
            .collect();

        if !config.app.is_empty()
            && let Some(operation) = &self.operation
        {
            operation
                .advance(
                    crate::bun::deploy_operations::DeployOperationPhase::DeployingApps,
                    None,
                    format!("deploying {} app(s)", config.app.len()),
                )
                .await;
        }

        for (app_name, spec) in &config.app {
            let namespace = spec.namespace.as_deref().unwrap_or("default");

            // run_before (E): jobs declaring `run_before = ["app.<name>"]` must
            // run to completion before this app's deploy begins — migrations are
            // the classic case. A prerequisite failure aborts the whole deploy.
            let target = format!("app.{app_name}");
            for (job_name, job_spec) in &config.job {
                // Cron-scheduled jobs fire on their schedule, never as a
                // deploy-time prerequisite.
                if ran_prereqs.contains(job_name)
                    || job_spec.schedule.is_some()
                    || !job_spec.run_before.contains(&target)
                {
                    continue;
                }
                let job_ns = job_spec.namespace.as_deref().unwrap_or("default");
                let _ = events
                    .send(ApplyEvent::Progress {
                        message: format!(
                            "running prerequisite job {job_name} before app {app_name}"
                        ),
                    })
                    .await;
                if let Err(e) = self.run_prerequisite_job(job_name, job_ns, job_spec).await {
                    let _ = events
                        .send(ApplyEvent::Error {
                            message: e.to_string(),
                        })
                        .await;
                    return;
                }
                ran_prereqs.insert(job_name.clone());
            }

            if let Some(operation) = &self.operation {
                operation
                    .advance(
                        crate::bun::deploy_operations::DeployOperationPhase::DeployingApps,
                        Some(crate::bun::deploy_operations::DeployTarget {
                            kind: crate::bun::deploy_operations::DeployTargetKind::App,
                            name: app_name.clone(),
                            namespace: namespace.to_string(),
                        }),
                        format!("deploying app {namespace}/{app_name}"),
                    )
                    .await;
            }

            // Gate on image signature first (IMG1). A verified image comes back
            // pinned to its manifest digest; the pinned spec shadows the
            // original for the rest of this iteration.
            let pinned_spec;
            let spec = match self.ops.enforce_image_signature(spec).await {
                Ok(None) => spec,
                Ok(Some(pinned_image)) => {
                    let mut with_pin = spec.clone();
                    with_pin.image = Some(pinned_image);
                    pinned_spec = with_pin;
                    &pinned_spec
                }
                Err(reason) => {
                    let _ = events.send(ApplyEvent::Error { message: reason }).await;
                    return;
                }
            };

            self.ops
                .store_deployed_spec(app_name, namespace, spec)
                .await;

            let existing = self.ops.list_existing_active(app_name, namespace).await;

            if !existing.is_empty() {
                // Dispatch on deploy strategy (E): blue-green stands up the
                // whole new fleet before swapping; rolling replaces one at a
                // time. Everything else about the deploy is identical.
                let strategy = spec
                    .deploy
                    .as_ref()
                    .map(crate::meat::deploy_types::DeployConfig::from_spec)
                    .unwrap_or_default()
                    .strategy;
                let outcome = match strategy {
                    crate::meat::deploy_types::DeployStrategy::BlueGreen => {
                        self.blue_green_redeploy(app_name, namespace, spec, existing, &events, now)
                            .await
                    }
                    crate::meat::deploy_types::DeployStrategy::Rolling => {
                        self.rolling_redeploy(app_name, namespace, spec, existing, &events, now)
                            .await
                    }
                };
                if outcome.is_break() {
                    return;
                }
                all_ids.extend(
                    self.ops
                        .list_existing_active(app_name, namespace)
                        .await
                        .iter()
                        .map(|id| id.0.clone()),
                );
                continue;
            }

            // Fresh deploy: no existing instances.
            let _ = events
                .send(ApplyEvent::Progress {
                    message: format!("deploying app {app_name} (replicas: {})", spec.replicas),
                })
                .await;

            let ids = match self
                .ops
                .supervisor_deploy_app(app_name, namespace, spec)
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

            if let Some(port) = spec.port {
                let firewall = spec.firewall.as_ref().and_then(|f| {
                    if f.allow_from.is_empty() {
                        None
                    } else {
                        Some(f.allow_from.clone())
                    }
                });
                self.ops
                    .register_service_app(app_name, namespace, port, firewall)
                    .await;
            }

            if let Some(ref ingress) = spec.ingress {
                self.ops.store_ingress(app_name, namespace, ingress).await;
            }

            for id in &ids {
                let _ = events
                    .send(ApplyEvent::Progress {
                        message: format!("creating instance {}", id.0),
                    })
                    .await;

                if let Err(e) = self
                    .drive_fresh_instance(id, app_name, namespace, spec)
                    .await
                {
                    let _ = events
                        .send(ApplyEvent::Error {
                            message: e.to_string(),
                        })
                        .await;
                    return;
                }

                self.ops
                    .provision_identity(app_name, namespace, id, false)
                    .await;

                let _ = events
                    .send(ApplyEvent::InstanceCreated {
                        id: id.0.clone(),
                        app: app_name.to_string(),
                    })
                    .await;
            }

            self.ops
                .push_deploy_history(crate::meat::deploy_types::DeployHistoryEntry {
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
                })
                .await;

            all_ids.extend(ids.iter().map(|id| id.0.clone()));
        }

        if !config.job.is_empty()
            && let Some(operation) = &self.operation
        {
            operation
                .advance(
                    crate::bun::deploy_operations::DeployOperationPhase::DeployingJobs,
                    None,
                    format!("deploying {} job(s)", config.job.len()),
                )
                .await;
        }

        for (job_name, spec) in &config.job {
            // Already run to completion as a run_before prerequisite above, or a
            // cron-scheduled job that fires on its schedule rather than now.
            if ran_prereqs.contains(job_name) || spec.schedule.is_some() {
                continue;
            }
            let namespace = spec.namespace.as_deref().unwrap_or("default");
            if let Some(operation) = &self.operation {
                operation
                    .advance(
                        crate::bun::deploy_operations::DeployOperationPhase::DeployingJobs,
                        Some(crate::bun::deploy_operations::DeployTarget {
                            kind: crate::bun::deploy_operations::DeployTargetKind::Job,
                            name: job_name.clone(),
                            namespace: namespace.to_string(),
                        }),
                        format!("deploying job {namespace}/{job_name}"),
                    )
                    .await;
            }
            let _ = events
                .send(ApplyEvent::Progress {
                    message: format!("deploying job {job_name}"),
                })
                .await;

            let ids = match self
                .ops
                .supervisor_deploy_job(job_name, namespace, spec)
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

                if let Err(e) = self.drive_job(id, job_name, namespace, spec).await {
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

        if let Some(operation) = &self.operation {
            operation
                .advance(
                    crate::bun::deploy_operations::DeployOperationPhase::RebuildingRoutes,
                    None,
                    "rebuilding service and ingress routes",
                )
                .await;
        }
        self.ops.rebuild_routing_table().await;

        let _ = events
            .send(ApplyEvent::Complete {
                created: all_ids.len(),
                instances: all_ids,
            })
            .await;
        for (app, namespace) in deployed_apps {
            self.ops.record_deployed_event(&app, &namespace).await;
        }
    }

    /// Drive a fresh instance through create → egress → init → start →
    /// HealthWait. The blocking grill calls (create/init/start) run here on
    /// the task; the loop applies the state transitions and bookkeeping.
    async fn drive_fresh_instance(
        &self,
        instance_id: &InstanceId,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
    ) -> Result<(), BunError> {
        let prepared = self
            .ops
            .prepare_fresh_instance(instance_id, app_name, namespace, spec)
            .await?;

        // The image pull happens here, off the loop.
        self.grill.create(instance_id, &prepared.oci_spec).await?;
        self.ops
            .store_oci_spec(instance_id, prepared.oci_spec)
            .await;

        // create → program → start: the workload never runs ahead of its
        // egress policy (#86). On failure the loop stops the container.
        self.ops
            .apply_egress_pre_start(instance_id, app_name, spec, &prepared.cgroup_path)
            .await?;

        if prepared.has_init {
            self.ops
                .transition_state(instance_id, ContainerState::Initialising)
                .await?;
            for (i, init_spec) in spec.init.iter().enumerate() {
                let init_id = InstanceId(format!("{}-init-{i}", instance_id.0));
                let init_oci = crate::grill::oci::generate_init_oci_spec(
                    &init_spec.command,
                    namespace,
                    app_name,
                    spec.image.as_deref(),
                    &prepared.cgroup_str,
                    None,
                );
                self.grill.create(&init_id, &init_oci).await?;
                self.grill.start(&init_id).await?;

                // Bounded wait: a hung init can't wedge the deploy forever (and
                // no longer wedges the loop at all — this poll is off it).
                let deadline =
                    std::time::Instant::now() + std::time::Duration::from_secs(INIT_TIMEOUT_SECS);
                let failed = loop {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    let state = self.grill.state(&init_id).await?;
                    if state == ContainerState::Stopped {
                        let exit_code = self.grill.exit_code(&init_id).await;
                        break exit_code != Some(0);
                    }
                    if std::time::Instant::now() >= deadline {
                        let _ = self.grill.kill(&init_id).await;
                        break true;
                    }
                };

                if failed {
                    let _ = self
                        .ops
                        .transition_state(instance_id, ContainerState::Failed)
                        .await;
                    return Err(BunError::InitContainerFailed {
                        instance_id: instance_id.clone(),
                        init_index: i,
                    });
                }
            }
        }

        self.ops
            .transition_state(instance_id, ContainerState::Starting)
            .await?;
        self.grill.start(instance_id).await?;

        let container_ip = self.grill.container_ip(instance_id).await;
        self.ops
            .finish_fresh_instance(instance_id, app_name, namespace, container_ip)
            .await
    }

    /// Drive a job instance through create → start → Running. Jobs skip
    /// health checks and egress programming (the former `drive_job_startup`).
    async fn drive_job(
        &self,
        instance_id: &InstanceId,
        job_name: &str,
        namespace: &str,
        spec: &JobSpec,
    ) -> Result<(), BunError> {
        self.ops
            .transition_state(instance_id, ContainerState::Preparing)
            .await?;

        let instance_index: u32 = instance_id
            .0
            .rsplit('-')
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let cgroup_path = crate::grill::cgroup::cgroup_path(namespace, job_name, instance_index);
        let cgroup_str = cgroup_path.to_string_lossy();
        let oci_spec = generate_job_oci_spec(job_name, namespace, spec, &cgroup_str, None);

        self.grill.create(instance_id, &oci_spec).await?;
        self.grill.start(instance_id).await?;
        self.ops
            .finish_job_instance(instance_id, job_name, namespace, oci_spec)
            .await
    }

    /// Run a `run_before` prerequisite job to completion for dependency
    /// ordering. Deploys the job, then polls the runtime until every instance
    /// exits. Returns `Ok(())` only when all instances exit cleanly (code 0);
    /// a non-zero exit or a timeout is an error that aborts the gated deploy.
    async fn run_prerequisite_job(
        &self,
        job_name: &str,
        namespace: &str,
        spec: &JobSpec,
    ) -> Result<(), BunError> {
        let ids = self
            .ops
            .supervisor_deploy_job(job_name, namespace, spec)
            .await?;
        for id in &ids {
            self.drive_job(id, job_name, namespace, spec).await?;

            let deadline =
                std::time::Instant::now() + std::time::Duration::from_secs(RUN_BEFORE_TIMEOUT_SECS);
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                let state = self.grill.state(id).await?;
                if state == ContainerState::Stopped {
                    let exit_code = self.grill.exit_code(id).await;
                    if exit_code == Some(0) {
                        break;
                    }
                    return Err(BunError::DeployFailed {
                        app_name: job_name.to_string(),
                        reason: format!(
                            "run_before job exited with {}",
                            exit_code
                                .map(|c| c.to_string())
                                .unwrap_or_else(|| "unknown status".to_string())
                        ),
                    });
                }
                if std::time::Instant::now() >= deadline {
                    let _ = self.grill.kill(id).await;
                    return Err(BunError::DeployFailed {
                        app_name: job_name.to_string(),
                        reason: format!(
                            "run_before job timed out after {RUN_BEFORE_TIMEOUT_SECS}s"
                        ),
                    });
                }
            }
        }
        Ok(())
    }

    /// Rolling redeploy: start generation-tagged new instances, health check
    /// them off the loop, then retire the old ones. Returns `Break` when the
    /// caller must stop the whole deploy. On new-instance failure it keeps the
    /// old instances and returns `Continue`.
    async fn rolling_redeploy(
        &self,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
        existing: Vec<InstanceId>,
        events: &mpsc::Sender<ApplyEvent>,
        now: Instant,
    ) -> std::ops::ControlFlow<()> {
        let _ = events
            .send(ApplyEvent::Progress {
                message: format!(
                    "rolling redeploy {app_name} ({} existing instance(s))",
                    existing.len()
                ),
            })
            .await;

        let deploy_config = spec
            .deploy
            .as_ref()
            .map(crate::meat::deploy_types::DeployConfig::from_spec)
            .unwrap_or_default();

        let deploy_gen = self.ops.next_deploy_gen().await;
        let replica_count = match spec.replicas {
            crate::config::types::Replicas::Fixed(n) => n,
            crate::config::types::Replicas::DaemonSet => 1,
        };

        let mut new_ids: Vec<InstanceId> = Vec::new();
        let mut new_ports: std::collections::HashMap<InstanceId, Option<u16>> =
            std::collections::HashMap::new();
        let mut new_specs: std::collections::HashMap<InstanceId, crate::grill::oci::OciSpec> =
            std::collections::HashMap::new();
        let mut new_ips: std::collections::HashMap<InstanceId, Option<std::net::Ipv4Addr>> =
            std::collections::HashMap::new();
        let mut new_prepared: Vec<InstanceId> = Vec::new();
        let mut new_failed = false;

        // M7: drive the rollout through `plan_rolling_step` rather than
        // "start everything, then retire everything". The planner decides
        // whether the next move is a replacement or a retirement based on
        // `max_surge` (how far above the target we may go) and
        // `max_unavailable` (how far below), which previously parsed,
        // validated and changed nothing.
        //
        // `retired` tracks how many of `existing` are gone; `finalise_rolling_deploy`
        // is given only what's left, and its own retire loop is an idempotent
        // catch-up for anything the planner didn't reach.
        let mut retired: usize = 0;
        let mut next_replica_index: u32 = 0;
        loop {
            let step = crate::meat::deploy_types::plan_rolling_step(
                replica_count,
                new_ids.len() as u32,
                0, // the start path health-waits inline, so nothing is ever pending here
                (existing.len() - retired) as u32,
                deploy_config.max_surge,
                deploy_config.max_unavailable,
            );
            match step {
                crate::meat::deploy_types::RollingStep::Done => break,
                crate::meat::deploy_types::RollingStep::Wait => break,
                crate::meat::deploy_types::RollingStep::Stuck => {
                    // Config validation rejects the only combination that can
                    // produce this, so reaching it means the bounds came from
                    // somewhere that skipped validation. Fail loudly rather
                    // than spin.
                    let _ = events
                        .send(ApplyEvent::Error {
                            message: format!(
                                "rolling deploy cannot progress with max_surge={} and \
                                 max_unavailable={}",
                                deploy_config.max_surge, deploy_config.max_unavailable
                            ),
                        })
                        .await;
                    new_failed = true;
                    break;
                }
                crate::meat::deploy_types::RollingStep::RetireOld => {
                    let old_id = existing[retired].clone();
                    let _ = events
                        .send(ApplyEvent::Progress {
                            message: format!("stopping old instance {}", old_id.0),
                        })
                        .await;
                    self.ops
                        .retire_old_instance(&old_id, deploy_config.drain_timeout)
                        .await;
                    retired += 1;
                    continue;
                }
                crate::meat::deploy_types::RollingStep::StartNew => {}
            }

            let i = next_replica_index;
            next_replica_index += 1;
            let new_id = crate::grill::InstanceIdentity::canary(namespace, app_name, deploy_gen, i)
                .instance_id();
            let _ = events
                .send(ApplyEvent::Progress {
                    message: format!("starting new instance {}", new_id.0),
                })
                .await;

            let host_port = if spec.port.is_some() {
                match self.port_allocator.allocate().await {
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

            new_prepared.push(new_id.clone());
            let oci_spec = match self
                .ops
                .prepare_rolling_instance(&new_id, app_name, namespace, spec, host_port, i)
                .await
            {
                Ok(oci_spec) => oci_spec,
                Err(e) => {
                    let _ = events
                        .send(ApplyEvent::Error {
                            message: e.to_string(),
                        })
                        .await;
                    new_failed = true;
                    break;
                }
            };
            let cgroup_path = crate::grill::cgroup::cgroup_path(namespace, app_name, i);

            if let Err(e) = self.grill.create(&new_id, &oci_spec).await {
                let _ = events
                    .send(ApplyEvent::Error {
                        message: format!("failed to create {}: {e}", new_id.0),
                    })
                    .await;
                new_failed = true;
                break;
            }
            // Same create → program → start ordering as the fresh path (#86).
            if let Err(e) = self
                .ops
                .apply_egress_pre_start(&new_id, app_name, spec, &cgroup_path)
                .await
            {
                let _ = events
                    .send(ApplyEvent::Error {
                        message: format!("failed to program egress for {}: {e}", new_id.0),
                    })
                    .await;
                let _ = self.grill.stop(&new_id).await;
                new_failed = true;
                break;
            }
            if let Err(e) = self.grill.start(&new_id).await {
                let _ = events
                    .send(ApplyEvent::Error {
                        message: format!("failed to start {}: {e}", new_id.0),
                    })
                    .await;
                self.ops.clear_egress(&new_id).await;
                new_failed = true;
                break;
            }
            self.ops
                .register_rolling_instance(&new_id, app_name, namespace)
                .await;

            let container_ip = self.grill.container_ip(&new_id).await;

            // Health wait: poll until Running, off the command loop. This runs
            // on a spawned per-deploy task, not the command loop, so the full
            // configured `health_timeout` is honoured — the old `.min(5s)` cap
            // silently failed a container that legitimately took longer than 5s
            // to become healthy and rolled the deploy back (M7).
            let wait = effective_health_wait(&deploy_config);
            let deadline = std::time::Instant::now() + wait;
            let mut probe = self.grill.state(&new_id).await;
            while std::time::Instant::now() < deadline
                && !matches!(probe, Ok(crate::grill::state::ContainerState::Running))
            {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                probe = self.grill.state(&new_id).await;
            }
            match probe {
                Ok(crate::grill::state::ContainerState::Running) => {
                    let _ = events
                        .send(ApplyEvent::Progress {
                            message: format!("{} healthy ✓", new_id.0),
                        })
                        .await;
                    self.ops
                        .provision_identity(app_name, namespace, &new_id, false)
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
                    let _ = self.grill.kill(&new_id).await;
                    self.ops.clear_egress(&new_id).await;
                    new_failed = true;
                    break;
                }
                Err(_) => {
                    let _ = events
                        .send(ApplyEvent::Error {
                            message: format!("{} state unknown, rolling back", new_id.0),
                        })
                        .await;
                    let _ = self.grill.kill(&new_id).await;
                    self.ops.clear_egress(&new_id).await;
                    new_failed = true;
                    break;
                }
            }

            new_ports.insert(new_id.clone(), host_port);
            new_specs.insert(new_id.clone(), oci_spec);
            new_ips.insert(new_id.clone(), container_ip);
            // DEP5/M7: route traffic onto the replacement the moment it's
            // healthy, before the planner is allowed to retire anything. With
            // `max_unavailable = 0` this is what makes the guarantee real —
            // retiring first and publishing later would leave a gap however
            // carefully the counts were tracked.
            self.ops
                .publish_new_backend(
                    app_name,
                    namespace,
                    &new_id,
                    host_port,
                    container_ip,
                    spec.port.is_some(),
                )
                .await;
            new_ids.push(new_id);
        }

        if new_failed {
            if deploy_config.auto_rollback {
                self.ops
                    .rollback_rolling_deploy(
                        app_name,
                        namespace,
                        spec,
                        new_ids,
                        new_prepared,
                        new_ports,
                        replica_count,
                    )
                    .await;
                let _ = events
                    .send(ApplyEvent::Error {
                        message: "rolled back — old instances preserved".to_string(),
                    })
                    .await;
            } else {
                // auto_rollback = false: halt without reverting. Keep the
                // healthy new instances and the surviving old ones in place for
                // the operator to inspect; tear down only the incomplete one.
                let new_live = new_ids.len();
                let old_live = existing.len().saturating_sub(retired);
                self.ops
                    .halt_rolling_deploy(
                        app_name,
                        namespace,
                        spec,
                        new_ids,
                        new_prepared,
                        new_ports,
                        replica_count,
                    )
                    .await;
                let _ = events
                    .send(ApplyEvent::Error {
                        message: format!(
                            "deploy halted (auto_rollback = false): {new_live} new and \
                             {old_live} old instance(s) left running for inspection"
                        ),
                    })
                    .await;
            }
            return std::ops::ControlFlow::Break(());
        }

        // Anything the planner didn't reach (it stops once every replacement is
        // healthy, and a scale-down leaves surplus old instances) is retired
        // here. On a default rollout this is empty — the stop-progress lines
        // were already emitted per step above.
        let outstanding: Vec<InstanceId> = existing[retired..].to_vec();
        for old_id in &outstanding {
            let _ = events
                .send(ApplyEvent::Progress {
                    message: format!("stopping old instance {}", old_id.0),
                })
                .await;
        }

        self.ops
            .finalise_rolling_deploy(
                app_name,
                namespace,
                spec,
                outstanding,
                new_ids.clone(),
                new_ports,
                new_ips,
                new_specs,
                now,
            )
            .await;

        for new_id in &new_ids {
            let _ = events
                .send(ApplyEvent::InstanceCreated {
                    id: new_id.0.clone(),
                    app: app_name.to_string(),
                })
                .await;
        }

        std::ops::ControlFlow::Continue(())
    }

    /// Blue-green redeploy: start the whole new ("green") fleet in parallel to
    /// the old ("blue") one, health check every green instance, and only then
    /// swap routing over and retire all of blue at once. Blue keeps serving the
    /// entire time green is coming up, so a failure anywhere in green tears the
    /// green fleet down and leaves blue untouched. Returns `Break` when the
    /// caller must stop the whole deploy.
    async fn blue_green_redeploy(
        &self,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
        existing: Vec<InstanceId>,
        events: &mpsc::Sender<ApplyEvent>,
        now: Instant,
    ) -> std::ops::ControlFlow<()> {
        let _ = events
            .send(ApplyEvent::Progress {
                message: format!(
                    "blue-green redeploy {app_name} ({} blue instance(s))",
                    existing.len()
                ),
            })
            .await;

        let deploy_config = spec
            .deploy
            .as_ref()
            .map(crate::meat::deploy_types::DeployConfig::from_spec)
            .unwrap_or_default();
        let deploy_gen = self.ops.next_deploy_gen().await;
        let replica_count = match spec.replicas {
            crate::config::types::Replicas::Fixed(n) => n,
            crate::config::types::Replicas::DaemonSet => 1,
        };

        let mut new_ids: Vec<InstanceId> = Vec::new();
        let mut new_ports: std::collections::HashMap<InstanceId, Option<u16>> =
            std::collections::HashMap::new();
        let mut new_specs: std::collections::HashMap<InstanceId, crate::grill::oci::OciSpec> =
            std::collections::HashMap::new();
        let mut new_ips: std::collections::HashMap<InstanceId, Option<std::net::Ipv4Addr>> =
            std::collections::HashMap::new();
        let mut new_prepared: Vec<InstanceId> = Vec::new();
        let mut new_failed = false;

        // Start and health check the entire green fleet before touching blue.
        // Unlike the rolling planner, nothing retires here and nothing is
        // published to routing yet: green comes up dark, alongside blue.
        for i in 0..replica_count {
            let new_id = crate::grill::InstanceIdentity::canary(namespace, app_name, deploy_gen, i)
                .instance_id();
            let _ = events
                .send(ApplyEvent::Progress {
                    message: format!("starting green instance {}", new_id.0),
                })
                .await;

            let host_port = if spec.port.is_some() {
                match self.port_allocator.allocate().await {
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

            new_prepared.push(new_id.clone());
            let oci_spec = match self
                .ops
                .prepare_rolling_instance(&new_id, app_name, namespace, spec, host_port, i)
                .await
            {
                Ok(oci_spec) => oci_spec,
                Err(e) => {
                    let _ = events
                        .send(ApplyEvent::Error {
                            message: e.to_string(),
                        })
                        .await;
                    new_failed = true;
                    break;
                }
            };
            let cgroup_path = crate::grill::cgroup::cgroup_path(namespace, app_name, i);

            if let Err(e) = self.grill.create(&new_id, &oci_spec).await {
                let _ = events
                    .send(ApplyEvent::Error {
                        message: format!("failed to create {}: {e}", new_id.0),
                    })
                    .await;
                new_failed = true;
                break;
            }
            if let Err(e) = self
                .ops
                .apply_egress_pre_start(&new_id, app_name, spec, &cgroup_path)
                .await
            {
                let _ = events
                    .send(ApplyEvent::Error {
                        message: format!("failed to program egress for {}: {e}", new_id.0),
                    })
                    .await;
                let _ = self.grill.stop(&new_id).await;
                new_failed = true;
                break;
            }
            if let Err(e) = self.grill.start(&new_id).await {
                let _ = events
                    .send(ApplyEvent::Error {
                        message: format!("failed to start {}: {e}", new_id.0),
                    })
                    .await;
                self.ops.clear_egress(&new_id).await;
                new_failed = true;
                break;
            }
            self.ops
                .register_rolling_instance(&new_id, app_name, namespace)
                .await;

            let container_ip = self.grill.container_ip(&new_id).await;

            let wait = effective_health_wait(&deploy_config);
            let deadline = std::time::Instant::now() + wait;
            let mut probe = self.grill.state(&new_id).await;
            while std::time::Instant::now() < deadline
                && !matches!(probe, Ok(crate::grill::state::ContainerState::Running))
            {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                probe = self.grill.state(&new_id).await;
            }
            match probe {
                Ok(crate::grill::state::ContainerState::Running) => {
                    let _ = events
                        .send(ApplyEvent::Progress {
                            message: format!("{} healthy ✓", new_id.0),
                        })
                        .await;
                    self.ops
                        .provision_identity(app_name, namespace, &new_id, false)
                        .await;
                }
                other => {
                    let message = match other {
                        Ok(state) => {
                            format!("{} not healthy (state: {state}), rolling back", new_id.0)
                        }
                        Err(_) => format!("{} state unknown, rolling back", new_id.0),
                    };
                    let _ = events.send(ApplyEvent::Error { message }).await;
                    let _ = self.grill.kill(&new_id).await;
                    self.ops.clear_egress(&new_id).await;
                    new_failed = true;
                    break;
                }
            }

            new_ports.insert(new_id.clone(), host_port);
            new_specs.insert(new_id.clone(), oci_spec);
            new_ips.insert(new_id.clone(), container_ip);
            new_ids.push(new_id);
        }

        if new_failed {
            // Green never took over routing, so blue is still live regardless of
            // auto_rollback. Rollback tears green down; halt leaves it up for
            // inspection. Either way blue keeps serving.
            if deploy_config.auto_rollback {
                self.ops
                    .rollback_rolling_deploy(
                        app_name,
                        namespace,
                        spec,
                        new_ids,
                        new_prepared,
                        new_ports,
                        replica_count,
                    )
                    .await;
                let _ = events
                    .send(ApplyEvent::Error {
                        message: "rolled back — blue fleet preserved".to_string(),
                    })
                    .await;
            } else {
                self.ops
                    .halt_rolling_deploy(
                        app_name,
                        namespace,
                        spec,
                        new_ids,
                        new_prepared,
                        new_ports,
                        replica_count,
                    )
                    .await;
                let _ = events
                    .send(ApplyEvent::Error {
                        message: "deploy halted (auto_rollback = false): green fleet left running \
                                  for inspection"
                            .to_string(),
                    })
                    .await;
            }
            return std::ops::ControlFlow::Break(());
        }

        // The whole green fleet is healthy. Swap: `finalise_rolling_deploy`
        // publishes every green backend, rebuilds routing, then drains and
        // retires all of blue. This is the atomic cut-over.
        self.ops
            .finalise_rolling_deploy(
                app_name,
                namespace,
                spec,
                existing,
                new_ids.clone(),
                new_ports,
                new_ips,
                new_specs,
                now,
            )
            .await;

        for new_id in &new_ids {
            let _ = events
                .send(ApplyEvent::InstanceCreated {
                    id: new_id.0.clone(),
                    app: app_name.to_string(),
                })
                .await;
        }

        std::ops::ControlFlow::Continue(())
    }
}

const DNS_TRACE_SCRIPT: &str = r#"
output=$(nslookup "$1" 2>&1)
status=$?
printf '%s\n' "$output"
printf '__RB_TRACE_DNS_STATUS__=%s\n' "$status"
"#;

const TCP_TRACE_SCRIPT: &str = r#"
output=$(nc -z -w 3 "$1" "$2" 2>&1)
status=$?
printf '%s\n' "$output"
printf '__RB_TRACE_TCP_STATUS__=%s\n' "$status"
"#;

fn trace_dns_command(name: &str) -> Vec<String> {
    vec![
        "sh".to_string(),
        "-c".to_string(),
        DNS_TRACE_SCRIPT.to_string(),
        "reliaburger-trace".to_string(),
        name.to_string(),
    ]
}

fn trace_tcp_command(host: &str, port: u16) -> Vec<String> {
    vec![
        "sh".to_string(),
        "-c".to_string(),
        TCP_TRACE_SCRIPT.to_string(),
        "reliaburger-trace".to_string(),
        host.to_string(),
        port.to_string(),
    ]
}

fn trace_probe_step(
    step_number: u32,
    name: &str,
    target: &str,
    probe: Result<crate::onion::trace::ProbeOutput, String>,
    expected_value: Option<&str>,
) -> crate::onion::trace::TraceStep {
    use crate::onion::trace::{TraceEvidence, TraceStep, TraceVerdict};
    match probe {
        Ok(probe) => {
            let mut details = vec![format!("fixed workload probe target: {target}")];
            details.extend(probe.lines);
            let verdict = if probe.status == 0 {
                if let Some(expected) = expected_value
                    && !details.iter().any(|line| line.contains(expected))
                {
                    TraceVerdict::Fail {
                        reason: format!(
                            "probe succeeded but its answer did not contain expected value {expected}"
                        ),
                    }
                } else {
                    TraceVerdict::Pass
                }
            } else if probe.status == 126 || probe.status == 127 {
                TraceVerdict::Unknown {
                    reason: format!("source image does not provide the fixed {name} probe tool"),
                }
            } else {
                TraceVerdict::Fail {
                    reason: format!("{name} exited with status {}", probe.status),
                }
            };
            TraceStep {
                step_number,
                name: name.to_string(),
                evidence: TraceEvidence::Observed,
                details,
                verdict,
            }
        }
        Err(reason) => TraceStep {
            step_number,
            name: name.to_string(),
            evidence: TraceEvidence::Unavailable,
            details: vec![format!("fixed workload probe target: {target}")],
            verdict: TraceVerdict::Unknown { reason },
        },
    }
}

/// The health-wait deadline for a rolling redeploy: the configured
/// `health_timeout`, uncapped (M7).
///
/// The rolling redeploy runs on a spawned per-deploy task, so a long wait
/// doesn't stall the command loop; the previous `.min(5s)` cap silently
/// clamped a configured 60s timeout to 5s and rolled back any container slower
/// than that to become healthy.
fn effective_health_wait(config: &crate::meat::deploy_types::DeployConfig) -> std::time::Duration {
    config.health_timeout
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

/// Construct the identity the agent requests for an app or job.
///
/// Keeping this in one function prevents certificate SANs and JWT claims from
/// drifting onto different trust domains.
pub fn workload_spiffe_uri(
    trust_domain: &str,
    namespace: &str,
    name: &str,
    workload_type: crate::sesame::types::WorkloadType,
) -> crate::sesame::types::SpiffeUri {
    crate::sesame::types::SpiffeUri {
        trust_domain: trust_domain.to_string(),
        namespace: namespace.to_string(),
        workload_type,
        name: name.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grill::mock::MockGrill;

    #[test]
    fn trace_targets_are_positional_arguments_not_shell_source() {
        let hostile = "api; touch /tmp/never";
        let dns = trace_dns_command(hostile);
        let tcp = trace_tcp_command(hostile, 443);
        assert!(!dns[2].contains(hostile));
        assert_eq!(dns[4], hostile);
        assert!(!tcp[2].contains(hostile));
        assert_eq!(tcp[4], hostile);
        assert_eq!(tcp[5], "443");
    }

    #[test]
    fn missing_workload_probe_tool_is_unknown_not_a_network_failure() {
        let step = trace_probe_step(
            1,
            "DNS query",
            "api.internal",
            Ok(crate::onion::trace::ProbeOutput {
                status: 127,
                lines: vec!["nslookup: not found".to_string()],
            }),
            None,
        );
        assert!(matches!(
            step.verdict,
            crate::onion::trace::TraceVerdict::Unknown { .. }
        ));
    }

    #[tokio::test]
    async fn trace_runs_fixed_dns_and_tcp_probes_from_the_source_workload() {
        let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
        let handle = tokio::spawn(async move { agent.run().await });
        let config = Config::parse(
            r#"
            [app.source]
            image = "source:v1"

            [app.destination]
            image = "destination:v1"
            port = 8080
            "#,
        )
        .unwrap();
        expect_complete(&send_deploy(&tx, config).await);

        let vip = crate::onion::vip::VirtualIP::from_qualified("default__destination");
        grill.set_exec_outputs([
            format!(
                "Name: destination.default.internal\nAddress: {vip}\n__RB_TRACE_DNS_STATUS__=0\n"
            ),
            "__RB_TRACE_TCP_STATUS__=0\n".to_string(),
        ]);
        grill.block_execs();
        let (response, receiver) = oneshot::channel();
        tx.send(AgentCommand::Trace {
            request: crate::onion::trace::TraceRequest {
                source: "source".to_string(),
                source_namespace: "default".to_string(),
                destination: "destination".to_string(),
                destination_namespace: "default".to_string(),
                port: None,
            },
            internal_destination: true,
            source_node: "node-a".to_string(),
            response,
        })
        .await
        .unwrap();

        grill.wait_for_execs(1).await;
        let (status_response, status_receiver) = oneshot::channel();
        tx.send(AgentCommand::Status {
            response: status_response,
        })
        .await
        .unwrap();
        let statuses = tokio::time::timeout(std::time::Duration::from_secs(1), status_receiver)
            .await
            .expect("a workload trace must not block the agent command loop")
            .unwrap();
        assert_eq!(statuses.len(), 2);
        grill.release_execs(1);

        let result = receiver.await.unwrap().unwrap();

        assert_eq!(result.steps.len(), 4);
        assert_eq!(
            result.steps[0].verdict,
            crate::onion::trace::TraceVerdict::Pass
        );
        assert_eq!(
            result.steps[1].evidence,
            crate::onion::trace::TraceEvidence::Inferred
        );
        assert_eq!(
            result.steps[3].verdict,
            crate::onion::trace::TraceVerdict::Pass
        );
        assert!(matches!(
            result.steps[2].verdict,
            crate::onion::trace::TraceVerdict::Unknown { .. }
        ));
        assert!(matches!(
            result.overall_result,
            crate::onion::trace::TraceVerdict::Unknown { .. }
        ));
        assert!(result.latency_ms.is_some());

        shutdown.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn trace_concurrency_is_bounded_without_queueing_more_workload_processes() {
        let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
        let handle = tokio::spawn(async move { agent.run().await });
        let config = Config::parse(
            r#"
            [app.source]
            image = "source:v1"

            [app.destination]
            image = "destination:v1"
            port = 8080
            "#,
        )
        .unwrap();
        expect_complete(&send_deploy(&tx, config).await);

        grill.set_exec_outputs(
            (0..16).map(|_| "__RB_TRACE_DNS_STATUS__=0\n__RB_TRACE_TCP_STATUS__=0\n".to_string()),
        );
        grill.block_execs();
        let request = crate::onion::trace::TraceRequest {
            source: "source".to_string(),
            source_namespace: "default".to_string(),
            destination: "destination".to_string(),
            destination_namespace: "default".to_string(),
            port: None,
        };
        let mut active_receivers = Vec::new();
        for _ in 0..MAX_CONCURRENT_TRACES {
            let (response, receiver) = oneshot::channel();
            tx.send(AgentCommand::Trace {
                request: request.clone(),
                internal_destination: true,
                source_node: "node-a".to_string(),
                response,
            })
            .await
            .unwrap();
            active_receivers.push(receiver);
        }
        grill
            .wait_for_execs(MAX_CONCURRENT_TRACES.try_into().unwrap())
            .await;

        let (response, receiver) = oneshot::channel();
        tx.send(AgentCommand::Trace {
            request,
            internal_destination: true,
            source_node: "node-a".to_string(),
            response,
        })
        .await
        .unwrap();
        let error = tokio::time::timeout(std::time::Duration::from_secs(1), receiver)
            .await
            .expect("the excess trace must be refused without joining a queue")
            .unwrap()
            .unwrap_err();
        assert!(matches!(error, BunError::TraceBusy));

        grill.release_execs(MAX_CONCURRENT_TRACES);
        for receiver in active_receivers {
            let _ = receiver.await;
        }
        shutdown.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_cancels_an_in_flight_workload_trace() {
        let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
        let handle = tokio::spawn(async move { agent.run().await });
        let config = Config::parse(
            r#"
            [app.source]
            image = "source:v1"

            [app.destination]
            image = "destination:v1"
            port = 8080
            "#,
        )
        .unwrap();
        expect_complete(&send_deploy(&tx, config).await);

        grill.block_execs();
        let (response, receiver) = oneshot::channel();
        tx.send(AgentCommand::Trace {
            request: crate::onion::trace::TraceRequest {
                source: "source".to_string(),
                source_namespace: "default".to_string(),
                destination: "destination".to_string(),
                destination_namespace: "default".to_string(),
                port: None,
            },
            internal_destination: true,
            source_node: "node-a".to_string(),
            response,
        })
        .await
        .unwrap();
        grill.wait_for_execs(1).await;

        shutdown.cancel();
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), receiver)
            .await
            .expect("shutdown must cancel the trace probe")
            .unwrap()
            .unwrap();
        assert!(matches!(
            result.overall_result,
            crate::onion::trace::TraceVerdict::Unknown { .. }
        ));
        handle.await.unwrap();
    }

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

    async fn test_cluster_fault_agent() -> (
        BunAgent<MockGrill>,
        crate::smoker::node_fault::NodeTransportGate,
        crate::bun::readiness::ReadinessTracker,
    ) {
        let (_membership_tx, membership_rx) = tokio::sync::watch::channel(Vec::new());
        let (_snapshot_tx, snapshot_rx) = mpsc::channel(1);
        let (_command_tx, command_rx) = mpsc::channel(8);
        let node_gate = crate::smoker::node_fault::NodeTransportGate::new();
        let cluster = ClusterHandle {
            membership_rx,
            raft_metrics_rx: None,
            council: None,
            snapshot_rx,
            wrapping_ikm: None,
            partition_blocklists: PartitionBlocklists {
                node_gate: node_gate.clone(),
                ..PartitionBlocklists::default()
            },
            crl_handle: Default::default(),
        };
        let mut agent = BunAgent::with_cluster(
            MockGrill::new(),
            PortAllocator::new(30000, 31000),
            command_rx,
            CancellationToken::new(),
            cluster,
            "test".to_string(),
        );
        let readiness = crate::bun::readiness::ReadinessTracker::new();
        readiness.register("agent:test", true).await;
        readiness.ready("agent:test").await;
        agent.set_readiness_tracker(readiness.clone());
        (agent, node_gate, readiness)
    }

    impl<G: Grill + Clone + 'static> BunAgent<G> {
        /// Test-only: run a deploy to completion against an agent that is not
        /// yet on its `run` loop. Deploys now execute on a spawned task that
        /// drives `&mut self` steps back through `deploy_ops_rx`, so this pumps
        /// those ops inline until the deploy's events channel closes. Keeps the
        /// direct-`deploy` unit tests working without standing up a full loop.
        async fn deploy(&mut self, config: Config, events: &mpsc::Sender<ApplyEvent>) {
            let worker = DeployWorker {
                grill: self.supervisor.grill().clone(),
                port_allocator: self.supervisor.port_allocator(),
                ops: DeployOps {
                    tx: self.deploy_ops_tx.clone(),
                },
                operation: None,
            };
            let events = events.clone();
            let mut task = tokio::spawn(async move { worker.run_deploy(config, events).await });
            loop {
                tokio::select! {
                    Some(op) = self.deploy_ops_rx.recv() => {
                        self.handle_deploy_op(op).await;
                    }
                    result = &mut task => {
                        let _ = result;
                        // Drain any ops queued right before the task finished.
                        while let Ok(op) = self.deploy_ops_rx.try_recv() {
                            self.handle_deploy_op(op).await;
                        }
                        break;
                    }
                }
            }
        }
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

    /// Phase 12 E0 (review M21): deploying an app with a managed
    /// volume creates the host directory before the container starts —
    /// runc fails create on a bind mount whose source doesn't exist.
    #[tokio::test]
    async fn deploy_creates_managed_volume_directories() {
        let volumes_dir = tempfile::tempdir().unwrap();
        let (mut agent, tx, shutdown) = test_agent();
        agent.set_volumes_dir(volumes_dir.path().to_path_buf());
        let handle = tokio::spawn(async move {
            agent.run().await;
        });

        let config = Config::parse(
            r#"
            [app.web]
            image = "myapp:v1"

            [[app.web.volumes]]
            path = "/data"
        "#,
        )
        .unwrap();
        let events = send_deploy(&tx, config).await;
        let (created, _) = expect_complete(&events);
        assert_eq!(created, 1);

        assert!(
            volumes_dir
                .path()
                .join("default")
                .join("web")
                .join("data")
                .is_dir(),
            "managed volume host directory must exist after deploy"
        );

        shutdown.cancel();
        let _ = handle.await;
    }

    /// Host-path volumes are the operator's responsibility — deploys
    /// must not create anything under the managed volumes directory.
    #[tokio::test]
    async fn deploy_leaves_hostpath_volumes_alone() {
        let volumes_dir = tempfile::tempdir().unwrap();
        let source_dir = tempfile::tempdir().unwrap();
        let (mut agent, tx, shutdown) = test_agent();
        agent.set_volumes_dir(volumes_dir.path().to_path_buf());
        let handle = tokio::spawn(async move {
            agent.run().await;
        });

        let toml = format!(
            r#"
            [app.web]
            image = "myapp:v1"

            [[app.web.volumes]]
            source = "{}"
            path = "/data"
        "#,
            source_dir.path().display()
        );
        let events = send_deploy(&tx, Config::parse(&toml).unwrap()).await;
        expect_complete(&events);

        assert!(
            !volumes_dir.path().join("default").exists(),
            "host-path volumes must not create managed directories"
        );

        shutdown.cancel();
        let _ = handle.await;
    }

    /// Phase 12 E2: restoring a snapshot under a running app is
    /// refused — the guard fires before any filesystem checks, so this
    /// tests on every platform.
    #[tokio::test]
    async fn snapshot_restore_refused_while_app_runs() {
        let volumes_dir = tempfile::tempdir().unwrap();
        let (mut agent, tx, shutdown) = test_agent();
        agent.set_volumes_dir(volumes_dir.path().to_path_buf());
        let handle = tokio::spawn(async move {
            agent.run().await;
        });

        let events = send_deploy(&tx, basic_config()).await;
        expect_complete(&events);

        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(AgentCommand::SnapshotRestore {
            namespace: "default".to_string(),
            app_name: "web".to_string(),
            name: "whatever".to_string(),
            response: resp_tx,
        })
        .await
        .unwrap();
        let result = resp_rx.await.unwrap();
        assert!(
            matches!(
                result,
                Err(BunError::Snapshot(
                    crate::grill::snapshot::SnapshotError::AppRunning { .. }
                ))
            ),
            "expected AppRunning, got {result:?}"
        );

        shutdown.cancel();
        let _ = handle.await;
    }

    /// Phase 12 E2: snapshotting an app with no provisioned volumes is
    /// an honest NoVolumes error, not an empty success.
    #[tokio::test]
    async fn snapshot_create_without_volumes_errors() {
        let volumes_dir = tempfile::tempdir().unwrap();
        let (mut agent, tx, shutdown) = test_agent();
        agent.set_volumes_dir(volumes_dir.path().to_path_buf());
        let handle = tokio::spawn(async move {
            agent.run().await;
        });

        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(AgentCommand::SnapshotCreate {
            namespace: "default".to_string(),
            app_name: "ghost".to_string(),
            volume: None,
            name: None,
            response: resp_tx,
        })
        .await
        .unwrap();
        let result = resp_rx.await.unwrap();
        assert!(matches!(
            result,
            Err(BunError::Snapshot(
                crate::grill::snapshot::SnapshotError::NoVolumes { .. }
            ))
        ));

        shutdown.cancel();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn single_node_image_deploy_is_refused_without_trust_state() {
        // require_signatures is on and there's no council to consult, so a
        // standalone image deploy can't be verified. It fails CLOSED: no
        // instances come up (IMG2). The old behaviour let it through.
        let (mut agent, tx, shutdown) = test_agent();
        agent.set_trust_policy(require_signatures_policy());
        let handle = tokio::spawn(async move {
            agent.run().await;
        });

        let events = send_deploy(&tx, basic_config()).await;
        assert!(
            events.iter().any(|e| matches!(e, ApplyEvent::Error { .. })),
            "an unverifiable image deploy must be refused, got: {events:?}"
        );
        let created = events.iter().find_map(|e| match e {
            ApplyEvent::Complete { created, .. } => Some(*created),
            _ => None,
        });
        assert_ne!(created, Some(1), "no instance should be created");
        let (snapshot_tx, snapshot_rx) = oneshot::channel();
        tx.send(AgentCommand::DeployOperations {
            response: snapshot_tx,
        })
        .await
        .unwrap();
        let snapshot = snapshot_rx.await.unwrap();
        assert!(snapshot.active_deploys.is_empty());
        assert_eq!(snapshot.history.len(), 1);
        assert_eq!(
            snapshot.history[0].outcome,
            Some(crate::bun::deploy_operations::DeployOperationOutcome::Failed)
        );

        shutdown.cancel();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn enforce_image_signature_fails_closed_without_council() {
        let (mut agent, _tx, _shutdown) = test_agent();
        agent.set_trust_policy(require_signatures_policy());
        let spec: AppSpec = toml::from_str(r#"image = "myapp:v1""#).unwrap();
        // No cluster/council → the gate can't obtain verification material, so
        // it must refuse rather than skip (IMG2 fail-closed).
        let result = agent.enforce_image_signature(&spec).await;
        assert!(result.is_err(), "expected refusal, got {result:?}");
        assert!(
            result.unwrap_err().contains("requires a signature"),
            "the refusal should name the missing verification"
        );
    }

    #[tokio::test]
    async fn enforce_image_signature_allows_a_process_workload_without_council() {
        let (mut agent, _tx, _shutdown) = test_agent();
        agent.set_trust_policy(require_signatures_policy());
        // A process workload has no image — nothing to verify, so it passes
        // even with require_signatures on and no council.
        let spec: AppSpec = toml::from_str(r#"command = ["echo", "hi"]"#).unwrap();
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
        let id = InstanceId("default__web-0".to_string());
        grill_handle.set_state(&id, ContainerState::Running);

        agent.shutdown_all().await;

        let calls = grill_handle.calls();
        assert!(
            calls
                .iter()
                .any(|(op, i)| op == "stop" && i.0 == "default__web-0"),
            "shutdown should SIGTERM first"
        );
        assert!(
            calls
                .iter()
                .any(|(op, i)| op == "kill" && i.0 == "default__web-0"),
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
        assert_eq!(instances, &["default__web-0"]);

        shutdown.cancel();
        agent_handle.await.unwrap();
    }

    /// DEP4/codex-M3: a deploy that blocks on a slow image pull must not
    /// wedge the command loop. While one deploy is stuck inside `create`,
    /// a `Status` command on the running loop still answers promptly. With
    /// the old serial deploy (awaited inline in the command arm) this
    /// `Status` could not be serviced until the pull finished.
    #[tokio::test]
    async fn slow_deploy_does_not_block_the_command_loop() {
        let (tx, rx) = mpsc::channel(32);
        let shutdown = CancellationToken::new();
        let grill = MockGrill::new();
        let grill_handle = grill.clone();
        let port_allocator = PortAllocator::new(30000, 31000);
        let mut agent = BunAgent::new(grill, port_allocator, rx, shutdown.clone());
        let volumes = tempfile::tempdir().unwrap();
        agent.set_volumes_dir(volumes.path().to_path_buf());
        let handle = tokio::spawn(async move { agent.run().await });

        // Hold create() at a deterministic barrier, simulating a slow image
        // pull without making the test wait for wall-clock time.
        grill_handle.block_creates();
        let (ev_tx, _ev_rx) = mpsc::channel(64);
        tx.send(AgentCommand::Deploy {
            config: basic_config(),
            events: ev_tx,
        })
        .await
        .unwrap();

        tokio::time::timeout(
            std::time::Duration::from_millis(500),
            grill_handle.wait_for_creates(1),
        )
        .await
        .expect("deploy never entered create");

        // A Status command must round-trip well before the 3s pull finishes.
        // If the loop were blocked inside create() this would not be answered
        // until the pull completed, blowing the 500ms timeout.
        let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
        tx.send(AgentCommand::Status { response: resp_tx })
            .await
            .unwrap();
        let answered = tokio::time::timeout(std::time::Duration::from_millis(500), resp_rx).await;
        assert!(
            answered.is_ok(),
            "status was not answered while a slow deploy was in flight — the deploy blocked the loop"
        );

        grill_handle.release_creates(1);
        shutdown.cancel();
        let _ = handle.await;
    }

    #[tokio::test]
    async fn deploy_operations_track_live_phase_conflicts_and_disconnected_clients() {
        let (tx, rx) = mpsc::channel(32);
        let shutdown = CancellationToken::new();
        let grill = MockGrill::new();
        let grill_handle = grill.clone();
        let port_allocator = PortAllocator::new(30000, 31000);
        let mut agent = BunAgent::new(grill, port_allocator, rx, shutdown.clone());
        let volumes = tempfile::tempdir().unwrap();
        agent.set_volumes_dir(volumes.path().to_path_buf());
        let handle = tokio::spawn(async move { agent.run().await });

        grill_handle.block_creates();
        let (events, mut event_rx) = mpsc::channel(64);
        tx.send(AgentCommand::Deploy {
            config: basic_config(),
            events,
        })
        .await
        .unwrap();
        let operation_id = match event_rx.recv().await.unwrap() {
            ApplyEvent::Accepted { operation_id } => operation_id,
            event => panic!("first event was not acceptance: {event:?}"),
        };
        tokio::time::timeout(
            std::time::Duration::from_millis(500),
            grill_handle.wait_for_creates(1),
        )
        .await
        .expect("deploy never entered create");

        let (snapshot_tx, snapshot_rx) = oneshot::channel();
        tx.send(AgentCommand::DeployOperations {
            response: snapshot_tx,
        })
        .await
        .unwrap();
        let snapshot = snapshot_rx.await.unwrap();
        assert_eq!(snapshot.active_deploys.len(), 1);
        let active = &snapshot.active_deploys[0];
        assert_eq!(active.id.as_str(), operation_id);
        assert_eq!(
            active.phase,
            crate::bun::deploy_operations::DeployOperationPhase::DeployingApps
        );
        assert_eq!(
            active
                .current_target
                .as_ref()
                .map(|target| target.name.as_str()),
            Some("web")
        );

        let (conflict_events, mut conflict_rx) = mpsc::channel(8);
        tx.send(AgentCommand::Deploy {
            config: basic_config(),
            events: conflict_events,
        })
        .await
        .unwrap();
        match conflict_rx.recv().await.unwrap() {
            ApplyEvent::Error { message } => {
                assert!(message.contains(&operation_id));
                assert!(message.contains("already being changed"));
            }
            event => panic!("overlapping deploy was not refused: {event:?}"),
        }

        // Losing the SSE consumer must not lose the operation outcome.
        drop(event_rx);
        grill_handle.release_creates(1);
        let terminal = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let (snapshot_tx, snapshot_rx) = oneshot::channel();
                tx.send(AgentCommand::DeployOperations {
                    response: snapshot_tx,
                })
                .await
                .unwrap();
                let snapshot = snapshot_rx.await.unwrap();
                if let Some(operation) = snapshot
                    .history
                    .into_iter()
                    .find(|operation| operation.id.as_str() == operation_id)
                {
                    break operation;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("deploy never reached terminal operation history");
        assert_eq!(
            terminal.outcome,
            Some(crate::bun::deploy_operations::DeployOperationOutcome::Completed)
        );
        assert!(terminal.finished_at.is_some());
        assert!(terminal.finished_at.unwrap() >= terminal.started_at);

        shutdown.cancel();
        let _ = handle.await;
    }

    /// DEP4/codex-M3: two concurrent deploys interleave rather than
    /// serialise. With both apps' create() sleeping, the second deploy's
    /// first grill call happens before the first deploy's create returns —
    /// impossible if deploys ran one-after-another on the command loop.
    #[tokio::test]
    async fn concurrent_deploys_interleave() {
        let (tx, rx) = mpsc::channel(32);
        let shutdown = CancellationToken::new();
        let grill = MockGrill::new();
        let grill_handle = grill.clone();
        let port_allocator = PortAllocator::new(30000, 31000);
        let mut agent = BunAgent::new(grill, port_allocator, rx, shutdown.clone());
        let handle = tokio::spawn(async move { agent.run().await });

        grill_handle.block_creates();

        let config_a = Config::parse("[app.alpha]\nimage = \"a:v1\"\n").unwrap();
        let config_b = Config::parse("[app.beta]\nimage = \"b:v1\"\n").unwrap();

        let (ev_a, _ra) = mpsc::channel(64);
        let (ev_b, _rb) = mpsc::channel(64);
        tx.send(AgentCommand::Deploy {
            config: config_a,
            events: ev_a,
        })
        .await
        .unwrap();
        tx.send(AgentCommand::Deploy {
            config: config_b,
            events: ev_b,
        })
        .await
        .unwrap();

        // If deploys were serial, the first blocked create would prevent the
        // second from reaching this barrier.
        tokio::time::timeout(
            std::time::Duration::from_millis(500),
            grill_handle.wait_for_creates(2),
        )
        .await
        .expect("both deploys did not enter create concurrently");

        let created: std::collections::HashSet<String> = grill_handle
            .calls()
            .into_iter()
            .filter(|(op, _)| op == "create")
            .map(|(_, id)| id.0)
            .collect();
        assert!(
            created.contains("default__alpha-0") && created.contains("default__beta-0"),
            "both deploys should be in flight together, got: {created:?}"
        );

        grill_handle.release_creates(2);
        shutdown.cancel();
        let _ = handle.await;
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

        let crasher = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                tx.send(AgentCommand::Status { response: resp_tx })
                    .await
                    .unwrap();
                if let Some(crasher) = resp_rx
                    .await
                    .unwrap()
                    .into_iter()
                    .find(|status| status.app_name == "crasher" && status.restart_count > 0)
                {
                    return crasher;
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("crashed app was not restarted");
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
            .resolve(&crate::onion::service_id::ServiceId::new("default", "web"))
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

        agent.stop_app("web", "default").await.unwrap();
    }

    #[tokio::test]
    async fn redeployed_instance_restarts_after_a_crash() {
        let (_tx, rx) = mpsc::channel(32);
        let shutdown = CancellationToken::new();
        let grill = crate::grill::process::ProcessGrill::new();
        let port_allocator = PortAllocator::new(30000, 31000);
        let mut agent = BunAgent::new(grill, port_allocator, rx, shutdown);

        let config = Config::parse(
            "[app.web]\nimage = \"proc-grill:ignored\"\ncommand = [\"sleep\", \"60\"]\n",
        )
        .unwrap();
        let (ev_tx, mut ev_rx) = mpsc::channel(256);

        // Fresh deploy, then redeploy (existing instances → rolling path).
        agent.deploy(config.clone(), &ev_tx).await;
        agent.deploy(config, &ev_tx).await;
        drop(ev_tx);
        while ev_rx.recv().await.is_some() {}

        let id = agent
            .supervisor
            .list_instances()
            .into_iter()
            .find(|i| i.app_name == "web")
            .expect("no web instance after redeploy")
            .id
            .clone();

        // The redeploy must have stored the OCI spec — without it the
        // crash-restart driver silently skips the instance (it filters on
        // `oci_spec.is_some()`), wedging it in Pending forever.
        assert!(
            agent
                .supervisor
                .get_instance(&id)
                .unwrap()
                .oci_spec
                .is_some(),
            "redeploy left the instance with no OCI spec, so it can never restart"
        );

        // Simulate a crash and drive one restart cycle.
        let now = std::time::Instant::now();
        agent.supervisor.get_instance_mut(&id).unwrap().state =
            crate::grill::state::ContainerState::Stopped;
        let _ = agent.supervisor.maybe_restart(&id, now).await;
        agent.drive_pending_restarts().await;

        let state = agent.supervisor.get_instance(&id).unwrap().state;
        assert_ne!(
            state,
            crate::grill::state::ContainerState::Pending,
            "redeployed instance stayed wedged in Pending instead of re-creating"
        );

        agent.stop_app("web", "default").await.unwrap();
    }

    #[tokio::test]
    async fn deploy_records_the_grills_container_ip_on_instance_and_backend() {
        let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
        let ip = std::net::Ipv4Addr::new(10, 0, 2, 5);
        grill.set_container_ip(ip);

        let (ev_tx, mut ev_rx) = mpsc::channel(64);
        agent.deploy(basic_config(), &ev_tx).await;
        drop(ev_tx);
        while ev_rx.recv().await.is_some() {}

        let inst = agent
            .supervisor
            .list_instances()
            .into_iter()
            .find(|i| i.app_name == "web")
            .expect("no web instance after deploy");
        assert_eq!(
            inst.container_ip,
            Some(ip),
            "the runtime's container IP was not recorded on the instance"
        );

        let entry = agent
            .service_map
            .resolve(&crate::onion::service_id::ServiceId::new("default", "web"))
            .expect("web not in map");
        assert!(
            entry.backends.iter().any(|b| b.node_ip == ip),
            "backend registered with loopback instead of the container IP"
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
            .any(|e| matches!(e, ApplyEvent::InstanceCreated { id, .. } if id == "default__web-0"));
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

    // ---- per-instance workload identity lifecycle (PKI7/D9) ----

    /// Names of the entries under `{volumes}/.identity`, sorted.
    fn identity_dir_names(volumes: &std::path::Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(volumes.join(".identity"))
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }

    /// PKI7: deploying two replicas prepares one identity directory per
    /// instance, and stopping the app removes them (key material never
    /// outlives the instance).
    #[tokio::test]
    async fn deploy_prepares_and_stop_removes_per_instance_identity_dirs() {
        let (mut agent, tx, shutdown) = test_agent();
        let volumes = tempfile::tempdir().unwrap();
        agent.set_volumes_dir(volumes.path().to_path_buf());

        let agent_handle = tokio::spawn(async move {
            agent.run().await;
        });

        let config = Config::parse(
            r#"
            [app.web]
            image = "myapp:v1"
            replicas = 2
        "#,
        )
        .unwrap();
        let events = send_deploy(&tx, config).await;
        expect_complete(&events);

        assert_eq!(
            identity_dir_names(volumes.path()),
            vec!["default__web-0".to_string(), "default__web-1".to_string()],
            "one identity dir per instance"
        );

        // Simulate provisioned key material so the stop has something to
        // scrub (single-node mode never reaches the council).
        std::fs::write(
            volumes.path().join(".identity/default__web-0/key.pem"),
            b"PRIVATE KEY",
        )
        .unwrap();

        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(AgentCommand::Stop {
            app_name: "web".to_string(),
            namespace: "default".to_string(),
            response: resp_tx,
        })
        .await
        .unwrap();
        resp_rx.await.unwrap().unwrap();

        assert!(
            identity_dir_names(volumes.path()).is_empty(),
            "stop removes every instance identity dir"
        );

        shutdown.cancel();
        agent_handle.await.unwrap();
    }

    /// PKI7: a rolling redeploy leaves exactly the live (new) instances'
    /// identity dirs — the retired generation's key material is gone.
    #[tokio::test]
    async fn rolling_redeploy_leaves_only_live_instances_identity_dirs() {
        let (mut agent, tx, shutdown) = test_agent();
        let volumes = tempfile::tempdir().unwrap();
        agent.set_volumes_dir(volumes.path().to_path_buf());

        let agent_handle = tokio::spawn(async move {
            agent.run().await;
        });

        let events = send_deploy(&tx, basic_config()).await;
        expect_complete(&events);
        assert_eq!(
            identity_dir_names(volumes.path()),
            vec!["default__web-0".to_string()]
        );

        // Redeploy: the rolling path replaces web-0 with web-g1-0.
        let events = send_deploy(&tx, basic_config()).await;
        let (_, instances) = expect_complete(&events);
        let mut expected: Vec<String> = instances.to_vec();
        expected.sort();

        assert_eq!(
            identity_dir_names(volumes.path()),
            expected,
            "exactly the live instances' dirs survive the redeploy"
        );

        shutdown.cancel();
        agent_handle.await.unwrap();
    }

    #[tokio::test]
    async fn rolling_redeploy_halts_without_reverting_when_auto_rollback_is_false() {
        let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
        let history = agent.deploy_history_handle();
        let agent_handle = tokio::spawn(async move {
            agent.run().await;
        });

        fn halt_config() -> Config {
            Config::parse(
                r#"
                [app.web]
                image = "myapp:v1"
                port = 8080

                [app.web.deploy]
                auto_rollback = false
                health_timeout = "1s"
            "#,
            )
            .unwrap()
        }

        // First deploy: web-0 comes up healthy (MockGrill defaults to Running).
        expect_complete(&send_deploy(&tx, halt_config()).await);

        // The next rolling redeploy's new instance (generation 1) never becomes
        // healthy, so the rollout fails after the 1s health wait.
        let new_id = crate::grill::InstanceIdentity::canary("default", "web", 1, 0).instance_id();
        grill.set_state(&new_id, crate::grill::state::ContainerState::Failed);

        let events = send_deploy(&tx, halt_config()).await;
        match events.last().expect("no events received") {
            ApplyEvent::Error { message } => {
                assert!(
                    message.contains("halted"),
                    "expected a halt, got: {message}"
                );
                assert!(
                    !message.contains("rolled back"),
                    "must not revert: {message}"
                );
            }
            other => panic!("expected an Error (halt) event, got {other:?}"),
        }

        // Halt keeps the old instance and tears down only the failed new one.
        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(AgentCommand::Status { response: resp_tx })
            .await
            .unwrap();
        let ids: Vec<String> = resp_rx.await.unwrap().into_iter().map(|s| s.id).collect();
        assert!(
            ids.iter().any(|id| id == "default__web-0"),
            "the old instance survives a halt, got {ids:?}"
        );
        assert!(
            !ids.iter().any(|id| id.contains("web-g1")),
            "the failed new instance was torn down, got {ids:?}"
        );

        // The deploy is recorded as Halted, not RolledBack.
        let hist = history.read().await;
        assert!(
            hist.iter()
                .any(|e| e.result == crate::meat::deploy_types::DeployResult::Halted),
            "a Halted deploy-history entry was recorded, got {:?}",
            hist.iter().map(|e| &e.result).collect::<Vec<_>>()
        );

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

    fn run_before_config() -> Config {
        Config::parse(
            r#"
            [app.web]
            image = "myapp:v1"
            port = 8080

            [job.migrate]
            image = "myapp:v1"
            command = ["echo", "migrating"]
            run_before = ["app.web"]
        "#,
        )
        .unwrap()
    }

    async fn drain_deploy(agent: &mut BunAgent<MockGrill>, config: Config) -> Vec<ApplyEvent> {
        let (ev_tx, mut ev_rx) = mpsc::channel(256);
        agent.deploy(config, &ev_tx).await;
        drop(ev_tx);
        let mut events = Vec::new();
        while let Some(e) = ev_rx.recv().await {
            events.push(e);
        }
        events
    }

    #[tokio::test]
    async fn run_before_runs_the_job_to_completion_before_the_app() {
        let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();

        // The prerequisite job exits cleanly, so the gate lets the app through.
        let job_id = InstanceId("default__migrate-0".to_string());
        grill.set_state(&job_id, ContainerState::Stopped);
        grill.set_exit_code(&job_id, Some(0));

        let events = drain_deploy(&mut agent, run_before_config()).await;
        expect_complete(&events);

        let calls = grill.calls();
        let migrate_at = calls
            .iter()
            .position(|(op, id)| op == "create" && id.0.contains("migrate"))
            .expect("prerequisite job was never created");
        let web_at = calls
            .iter()
            .position(|(op, id)| op == "create" && id.0.contains("web"))
            .expect("app was never created");
        assert!(
            migrate_at < web_at,
            "the run_before job must be created before the app: {calls:?}"
        );

        let migrate_creates = calls
            .iter()
            .filter(|(op, id)| op == "create" && id.0.contains("migrate"))
            .count();
        assert_eq!(
            migrate_creates, 1,
            "the run_before job must not also run in the regular jobs loop: {calls:?}"
        );
    }

    #[tokio::test]
    async fn run_before_failure_aborts_the_deploy() {
        let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();

        // The prerequisite job exits non-zero, so the whole deploy is aborted.
        let job_id = InstanceId("default__migrate-0".to_string());
        grill.set_state(&job_id, ContainerState::Stopped);
        grill.set_exit_code(&job_id, Some(1));

        let events = drain_deploy(&mut agent, run_before_config()).await;
        match events.last().expect("no events received") {
            ApplyEvent::Error { message } => assert!(
                message.contains("migrate"),
                "expected a prerequisite failure, got: {message}"
            ),
            other => panic!("expected an Error event, got {other:?}"),
        }

        let calls = grill.calls();
        assert!(
            !calls
                .iter()
                .any(|(op, id)| op == "create" && id.0.contains("web")),
            "the app must not deploy once a prerequisite fails: {calls:?}"
        );
    }

    #[tokio::test]
    async fn scheduled_job_is_not_run_at_deploy_time() {
        let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
        let config = Config::parse(
            r#"
            [job.nightly]
            image = "myapp:v1"
            command = ["echo", "hi"]
            schedule = "0 3 * * *"
        "#,
        )
        .unwrap();

        let events = drain_deploy(&mut agent, config).await;
        let (created, _) = expect_complete(&events);
        assert_eq!(created, 0, "a scheduled job must not run at deploy time");
        assert!(
            !grill.calls().iter().any(|(op, _)| op == "create"),
            "no container should be created for a scheduled job at deploy time"
        );
    }

    #[tokio::test]
    async fn blue_green_redeploy_swaps_to_the_green_fleet() {
        let (mut agent, tx, shutdown, _grill) = test_agent_with_grill();
        let agent_handle = tokio::spawn(async move {
            agent.run().await;
        });

        fn bg_config() -> Config {
            Config::parse(
                r#"
                [app.web]
                image = "myapp:v1"
                port = 8080

                [app.web.deploy]
                strategy = "blue-green"
                health_timeout = "1s"
            "#,
            )
            .unwrap()
        }

        // First deploy: no existing instances, so the fresh path brings up the
        // blue fleet (web-0). MockGrill defaults every container to Running.
        expect_complete(&send_deploy(&tx, bg_config()).await);

        // Redeploy: existing instances present, so the strategy dispatch routes
        // to blue-green — the green fleet (generation 1) comes up and swaps.
        expect_complete(&send_deploy(&tx, bg_config()).await);

        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(AgentCommand::Status { response: resp_tx })
            .await
            .unwrap();
        let ids: Vec<String> = resp_rx.await.unwrap().into_iter().map(|s| s.id).collect();
        assert_eq!(
            ids.len(),
            1,
            "exactly one green instance is live, got {ids:?}"
        );
        assert!(
            !ids.iter().any(|id| id == "default__web-0"),
            "the blue instance must be retired after the swap, got {ids:?}"
        );

        shutdown.cancel();
        agent_handle.await.unwrap();
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
        assert_eq!(instances, &["default__migrate-0"]);

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
        let init_id = InstanceId("default__web-0-init-0".to_string());
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
        let init_id = InstanceId("default__web-0-init-0".to_string());
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
    fn rolling_health_wait_honours_the_configured_timeout_not_a_5s_cap() {
        // M7: a configured 60s health_timeout must be used in full, not
        // clamped to 5s (which would fail a slow-starting container).
        let config = crate::meat::deploy_types::DeployConfig {
            health_timeout: std::time::Duration::from_secs(60),
            ..Default::default()
        };
        assert_eq!(
            super::effective_health_wait(&config),
            std::time::Duration::from_secs(60)
        );

        let short = crate::meat::deploy_types::DeployConfig {
            health_timeout: std::time::Duration::from_secs(2),
            ..Default::default()
        };
        assert_eq!(
            super::effective_health_wait(&short),
            std::time::Duration::from_secs(2),
            "a short timeout is still honoured exactly"
        );
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
                port_mapping: None,
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
            rootless_network: None,
        }
    }

    #[tokio::test]
    async fn started_rootless_instance_persists_network_recreation_state() {
        let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
        let records = tempfile::tempdir().unwrap();
        let volumes = tempfile::tempdir().unwrap();
        agent.set_records_dir(records.path().to_path_buf());
        agent.set_volumes_dir(volumes.path().to_path_buf());
        grill.set_runtime_kind(crate::grill::records::RuntimeKind::Runc);
        grill.set_pid(std::process::id());
        let rootless_network = crate::grill::records::RootlessNetworkRecord {
            api_socket: records.path().join("slirp4netns.sock"),
            owner_pid: 4243,
            owner_pid_started_at: 1001,
            container_pid: 4244,
            port_mapping: Some(crate::grill::oci::PortMapping {
                host_port: 30000,
                container_port: 8080,
            }),
        };
        grill.set_rootless_network(rootless_network.clone());

        let (events, mut event_rx) = mpsc::channel(64);
        agent.deploy(basic_config(), &events).await;
        drop(events);
        while event_rx.recv().await.is_some() {}

        let persisted = crate::grill::records::load_records(records.path());
        assert_eq!(persisted.len(), 1);
        assert_eq!(persisted[0].schema, 2);
        assert_eq!(persisted[0].rootless_network, Some(rootless_network));
    }

    #[tokio::test]
    async fn startup_adopts_recorded_instances_instead_of_restarting() {
        let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
        let dir = tempfile::tempdir().unwrap();
        let record = adoption_record("default__web-0", "web", false);
        crate::grill::records::write_record(dir.path(), &record).unwrap();
        agent.set_records_dir(dir.path().to_path_buf());

        let id = InstanceId("default__web-0".to_string());
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
    async fn legacy_record_adopts_under_a_canonical_key() {
        // In-place upgrade across the identity change: an old bun left a
        // record whose instance_id has no namespace prefix (`web-0`). The
        // runtime still knows the container by that legacy id, but the
        // supervisor must key the adopted instance canonically so it can't
        // collide with a same-name app in another namespace (DEP1).
        let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
        let dir = tempfile::tempdir().unwrap();
        let record = adoption_record("web-0", "web", false); // legacy, bare id
        crate::grill::records::write_record(dir.path(), &record).unwrap();
        agent.set_records_dir(dir.path().to_path_buf());

        // The runtime adopts by the id it ran under: the legacy one.
        let runtime_id = InstanceId("web-0".to_string());
        grill.set_adopt_result(&runtime_id, true);

        assert_eq!(agent.adopt_recorded_instances().await, 1);

        // But the supervisor keys it canonically.
        let canonical = InstanceId("default__web-0".to_string());
        let instance = agent
            .supervisor
            .get_instance(&canonical)
            .expect("adopted under the canonical key");
        assert_eq!(instance.namespace, "default");
        assert_eq!(instance.app_name, "web");
        // The legacy key resolves to nothing.
        assert!(agent.supervisor.get_instance(&runtime_id).is_none());
        // The runtime was asked to adopt the legacy container id, not to
        // create or start a fresh one.
        let calls = grill.calls();
        assert!(calls.contains(&("adopt".to_string(), runtime_id.clone())));
        assert!(!calls.contains(&("create".to_string(), runtime_id.clone())));
        assert!(!calls.contains(&("start".to_string(), runtime_id)));
    }

    #[tokio::test]
    async fn startup_deletes_stale_records_and_reschedules() {
        let (mut agent, _tx, _shutdown, _grill) = test_agent_with_grill();
        let dir = tempfile::tempdir().unwrap();
        // MockGrill declines adoption by default (dead process).
        let record = adoption_record("default__web-0", "web", false);
        crate::grill::records::write_record(dir.path(), &record).unwrap();
        agent.set_records_dir(dir.path().to_path_buf());

        assert_eq!(agent.adopt_recorded_instances().await, 0);

        // The stale record is gone and nothing was seeded: the normal
        // reconcile path is free to reschedule the instance.
        assert!(crate::grill::records::load_records(dir.path()).is_empty());
        assert!(
            agent
                .supervisor
                .get_instance(&InstanceId("default__web-0".to_string()))
                .is_none()
        );
    }

    /// Write a real identity bundle into `volumes/.identity/{instance}`
    /// and return it, so adoption tests have on-disk state to restore.
    fn write_test_identity(
        volumes: &std::path::Path,
        instance: &str,
    ) -> crate::sesame::types::WorkloadIdentity {
        let uri = crate::sesame::types::SpiffeUri {
            trust_domain: "default".to_string(),
            namespace: "default".to_string(),
            workload_type: crate::sesame::types::WorkloadType::App,
            name: "web".to_string(),
        };
        let hierarchy =
            crate::sesame::ca::generate_ca_hierarchy("default", b"test-ikm-32-bytes!").unwrap();
        let (csr_der, private_key_der) =
            crate::sesame::identity::create_workload_csr(&uri).unwrap();
        let cert_der = crate::sesame::identity::validate_and_sign_csr(
            &csr_der,
            &uri,
            crate::sesame::types::SerialNumber(42),
            crate::sesame::identity::CertUsage::Mtls,
            &hierarchy.workload.signing_keypair,
            &hierarchy.workload.certificate_params,
            SystemTime::now(),
        )
        .unwrap();
        let identity = crate::sesame::identity::build_identity_bundle(
            uri,
            cert_der,
            private_key_der,
            &hierarchy.workload.ca.certificate_der,
            &hierarchy.root.ca.certificate_der,
            "adopted-jwt".to_string(),
        );
        let dir = crate::sesame::identity::instance_identity_dir(volumes, instance);
        crate::sesame::identity::write_identity_files(&identity, &dir, None).unwrap();
        identity
    }

    /// D9: adoption rebuilds the identity and its rotation schedule from
    /// the per-instance directory — no `identity: None`, no fresh CSR.
    /// The restored schedule means the next rotation fires exactly when
    /// the pre-restart one would have.
    #[tokio::test]
    async fn adoption_restores_identity_and_rotation_schedule_from_disk() {
        let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
        let records = tempfile::tempdir().unwrap();
        let volumes = tempfile::tempdir().unwrap();
        agent.set_volumes_dir(volumes.path().to_path_buf());
        agent.set_records_dir(records.path().to_path_buf());

        let written = write_test_identity(volumes.path(), "default__web-0");
        let record = adoption_record("default__web-0", "web", false);
        crate::grill::records::write_record(records.path(), &record).unwrap();
        let id = InstanceId("default__web-0".to_string());
        grill.set_adopt_result(&id, true);

        assert_eq!(agent.adopt_recorded_instances().await, 1);

        let instance = agent.supervisor.get_instance(&id).unwrap();
        let restored = instance
            .identity
            .as_ref()
            .expect("adopted instance keeps its identity");
        assert_eq!(restored.spiffe_uri, written.spiffe_uri);
        assert_eq!(restored.private_key_der, written.private_key_der);
        assert_eq!(
            restored.next_rotation, written.next_rotation,
            "the rotation schedule is the disk one, not a fresh clock"
        );
        assert_eq!(
            instance.identity_mount.as_deref(),
            Some(
                crate::sesame::identity::instance_identity_dir(volumes.path(), "default__web-0")
                    .as_path()
            )
        );

        // The rotation loop fires on the restored schedule: fresh now,
        // then due once the recorded next_rotation passes.
        assert_eq!(
            crate::sesame::identity::rotation_state(restored, written.issued_at),
            crate::sesame::identity::RotationState::Valid
        );
        assert_eq!(
            crate::sesame::identity::rotation_state(
                restored,
                written.next_rotation + std::time::Duration::from_secs(1)
            ),
            crate::sesame::identity::RotationState::NeedsRotation
        );
    }

    /// PKI7: identity directories with no live owner — a legacy
    /// app-scoped layout, or an instance that died while bun was down —
    /// are swept at adoption, so stale key material never lingers.
    #[tokio::test]
    async fn adoption_sweeps_orphaned_identity_dirs() {
        let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
        let records = tempfile::tempdir().unwrap();
        let volumes = tempfile::tempdir().unwrap();
        agent.set_volumes_dir(volumes.path().to_path_buf());
        agent.set_records_dir(records.path().to_path_buf());

        // A live instance's dir, a legacy app-scoped dir, and a dead
        // instance's leftovers.
        write_test_identity(volumes.path(), "default__web-0");
        let legacy = volumes.path().join(".identity/default");
        std::fs::create_dir_all(legacy.join("web")).unwrap();
        std::fs::write(legacy.join("web/key.pem"), b"legacy key").unwrap();
        let dead = volumes.path().join(".identity/old-app-0");
        std::fs::create_dir_all(&dead).unwrap();
        std::fs::write(dead.join("key.pem"), b"dead key").unwrap();

        let record = adoption_record("default__web-0", "web", false);
        crate::grill::records::write_record(records.path(), &record).unwrap();
        let id = InstanceId("default__web-0".to_string());
        grill.set_adopt_result(&id, true);

        assert_eq!(agent.adopt_recorded_instances().await, 1);

        assert_eq!(
            identity_dir_names(volumes.path()),
            vec!["default__web-0".to_string()],
            "only the adopted instance's identity dir survives"
        );
    }

    #[tokio::test]
    async fn adopted_instances_resume_health_checks() {
        let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
        let dir = tempfile::tempdir().unwrap();
        let record = adoption_record("default__web-0", "web", true);
        crate::grill::records::write_record(dir.path(), &record).unwrap();
        agent.set_records_dir(dir.path().to_path_buf());

        let id = InstanceId("default__web-0".to_string());
        grill.set_adopt_result(&id, true);
        assert_eq!(agent.adopt_recorded_instances().await, 1);

        let instance = agent.supervisor.get_instance(&id).unwrap();
        assert!(instance.health_config.is_some());
    }

    #[tokio::test]
    async fn adopted_instance_port_is_reserved() {
        let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
        let dir = tempfile::tempdir().unwrap();
        let record = adoption_record("default__web-0", "web", false);
        crate::grill::records::write_record(dir.path(), &record).unwrap();
        agent.set_records_dir(dir.path().to_path_buf());

        let id = InstanceId("default__web-0".to_string());
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

        let id = InstanceId("default__web-0".to_string());
        assert!(agent.supervisor.get_instance(&id).is_some());
        let record = adoption_record("default__web-0", "web", false);
        crate::grill::records::write_record(dir.path(), &record).unwrap();
        grill.set_adopt_result(&id, true);

        assert_eq!(agent.adopt_recorded_instances().await, 0);
    }

    // ---------------------------------------------------------------------
    // DEP6: exit-aware stop.
    // ---------------------------------------------------------------------

    /// A stop must SIGTERM, wait for the runtime to confirm exit, and record
    /// Stopped only then. If the process ignores SIGTERM the stop escalates
    /// to SIGKILL rather than lying that the app is down.
    #[tokio::test]
    async fn stop_escalates_to_kill_when_process_ignores_sigterm() {
        let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();

        let (ev_tx, mut ev_rx) = mpsc::channel(64);
        agent.deploy(basic_config(), &ev_tx).await;
        drop(ev_tx);
        while ev_rx.recv().await.is_some() {}

        // The workload refuses SIGTERM: stop() records the call but leaves the
        // state Running. The exit-aware stop must therefore kill().
        grill.set_ignore_stop(true);
        let id = InstanceId("default__web-0".to_string());
        grill.set_state(&id, ContainerState::Running);

        agent.stop_app("web", "default").await.unwrap();

        let calls = grill.calls();
        assert!(
            calls.iter().any(|(op, i)| op == "stop" && i == &id),
            "stop must SIGTERM first"
        );
        assert!(
            calls.iter().any(|(op, i)| op == "kill" && i == &id),
            "stop must escalate to SIGKILL when the process ignores SIGTERM"
        );
    }

    /// A cooperative stop reports Stopped once the runtime confirms exit, and
    /// does not needlessly escalate to kill.
    #[tokio::test]
    async fn stop_reports_stopped_after_exit_without_kill() {
        let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();

        let (ev_tx, mut ev_rx) = mpsc::channel(64);
        agent.deploy(basic_config(), &ev_tx).await;
        drop(ev_tx);
        while ev_rx.recv().await.is_some() {}

        agent.stop_app("web", "default").await.unwrap();

        // The instance is recorded Stopped, and stop did not need to
        // force-kill a cooperative process.
        let id = InstanceId("default__web-0".to_string());
        assert_eq!(
            agent.supervisor.get_instance(&id).map(|i| i.state),
            Some(ContainerState::Stopped),
            "stopped instance should be recorded Stopped after exit"
        );
        let calls = grill.calls();
        assert!(
            calls.iter().any(|(op, i)| op == "stop" && i == &id),
            "stop must SIGTERM"
        );
        assert!(
            !calls.iter().any(|(op, i)| op == "kill" && i == &id),
            "a cooperative stop must not escalate to SIGKILL"
        );
    }

    // ---------------------------------------------------------------------
    // DEP5: drain / surge / max_unavailable.
    // ---------------------------------------------------------------------

    /// A retire waits for an in-flight request (tracked through the shared
    /// drain tracker, as the live Wrapper proxy would report it) to finish
    /// before the old container is killed.
    #[tokio::test]
    async fn retire_waits_for_in_flight_request_before_kill() {
        let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();

        let (ev_tx, mut ev_rx) = mpsc::channel(64);
        agent.deploy(config_with_health(), &ev_tx).await;
        drop(ev_tx);
        while ev_rx.recv().await.is_some() {}

        let id = InstanceId("default__web-0".to_string());
        let drains = agent.drains_handle();

        // Simulate the proxy holding an in-flight request open on the backend
        // that is about to be retired: start the drain and bump its count.
        drains
            .start_drain(&crate::wrapper::draining::DrainCommand {
                app_name: "web".to_string(),
                instance_id: id.0.clone(),
                timeout: std::time::Duration::from_secs(30),
            })
            .await;
        drains.increment_connections(&id.0).await;

        // Kick off the retire on a task; it must block on the drain.
        let agent_ref = &agent;
        let retire = agent_ref.retire_with_drain(
            std::slice::from_ref(&id),
            std::time::Duration::from_secs(30),
        );
        tokio::pin!(retire);

        // While the request is in flight, the retire has not killed anything.
        let early = tokio::time::timeout(std::time::Duration::from_millis(200), &mut retire).await;
        assert!(
            early.is_err(),
            "retire finished before the in-flight request drained"
        );
        assert!(
            !grill.calls().iter().any(|(op, i)| op == "kill" && i == &id),
            "old instance killed while a request was still in flight"
        );

        // The request finishes: the drain completes and the retire proceeds.
        drains.decrement_connections(&id.0).await;
        tokio::time::timeout(std::time::Duration::from_secs(2), &mut retire)
            .await
            .expect("retire did not complete after the request drained");
        let calls = grill.calls();
        assert!(
            calls.iter().any(|(op, i)| op == "stop" && i == &id),
            "retire must stop the drained instance"
        );
    }

    /// ING4: a retire waits for an in-flight *WebSocket* splice, not just a
    /// plain HTTP request. The WebSocket bumps both counters; the HTTP part of
    /// the splice finishes first, but the live WebSocket must keep the drain
    /// open until it closes, so the old container isn't killed mid-splice.
    #[tokio::test]
    async fn retire_waits_for_in_flight_websocket_before_kill() {
        let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();

        let (ev_tx, mut ev_rx) = mpsc::channel(64);
        agent.deploy(config_with_health(), &ev_tx).await;
        drop(ev_tx);
        while ev_rx.recv().await.is_some() {}

        let id = InstanceId("default__web-0".to_string());
        let drains = agent.drains_handle();

        // The proxy would bump both counters at the 101 for a WebSocket.
        drains
            .start_drain(&crate::wrapper::draining::DrainCommand {
                app_name: "web".to_string(),
                instance_id: id.0.clone(),
                timeout: std::time::Duration::from_secs(30),
            })
            .await;
        drains.increment_connections(&id.0).await;
        drains.increment_websocket(&id.0).await;

        let agent_ref = &agent;
        let retire = agent_ref.retire_with_drain(
            std::slice::from_ref(&id),
            std::time::Duration::from_secs(30),
        );
        tokio::pin!(retire);

        // The HTTP half of the splice completes, but the WebSocket is still
        // open, so the retire must not proceed.
        drains.decrement_connections(&id.0).await;
        let early = tokio::time::timeout(std::time::Duration::from_millis(200), &mut retire).await;
        assert!(
            early.is_err(),
            "retire finished while a WebSocket splice was still open"
        );
        assert!(
            !grill.calls().iter().any(|(op, i)| op == "kill" && i == &id),
            "old instance killed while a WebSocket was still spliced"
        );

        // The WebSocket closes: the drain completes and the retire proceeds.
        drains.decrement_websocket(&id.0).await;
        tokio::time::timeout(std::time::Duration::from_secs(2), &mut retire)
            .await
            .expect("retire did not complete after the WebSocket closed");
        assert!(
            grill.calls().iter().any(|(op, i)| op == "stop" && i == &id),
            "retire must stop the drained instance once the WebSocket closed"
        );
    }

    /// A rolling redeploy with `max_unavailable = 1` surges the new instances
    /// up before retiring the old, so the serving-instance count never drops
    /// below `replicas - max_unavailable`. With surge-first, the old instance
    /// is only stopped after the new one is healthy.
    #[tokio::test]
    async fn rolling_redeploy_never_drops_below_target_availability() {
        let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();

        let config = Config::parse(
            "[app.web]\nimage = \"web:v1\"\nport = 8080\nreplicas = 1\n\n[app.web.deploy]\nmax_unavailable = 1\ndrain_timeout = \"1s\"\n",
        )
        .unwrap();

        // Fresh deploy.
        let (ev_tx, mut ev_rx) = mpsc::channel(256);
        agent.deploy(config.clone(), &ev_tx).await;
        drop(ev_tx);
        while ev_rx.recv().await.is_some() {}

        let calls_before = grill.calls().len();

        // Redeploy: rolling path. The new instance is created and started
        // before the old one is stopped/killed.
        let (ev_tx, mut ev_rx) = mpsc::channel(256);
        agent.deploy(config, &ev_tx).await;
        drop(ev_tx);
        while ev_rx.recv().await.is_some() {}

        let calls: Vec<(String, InstanceId)> = grill.calls().split_off(calls_before);

        // The first "start" of a new (gen-tagged) instance must come before the
        // first "stop"/"kill" of the old default__web-0 — surge-first ordering.
        let first_new_start = calls.iter().position(|(op, i)| {
            op == "start" && i.0.contains("-g") && i.0.starts_with("default__web")
        });
        let first_old_retire = calls
            .iter()
            .position(|(op, i)| (op == "stop" || op == "kill") && i.0 == "default__web-0");
        assert!(
            first_new_start.is_some(),
            "rolling redeploy never started a new instance"
        );
        assert!(
            first_old_retire.is_some(),
            "rolling redeploy never retired the old instance"
        );
        assert!(
            first_new_start < first_old_retire,
            "old instance was retired before the new one started — availability dropped below target"
        );
    }

    /// M7: `max_surge` bounds how many containers exist at once during a
    /// rollout. It used to parse, validate and change nothing — the rollout
    /// started every replacement and only then retired every old instance, so
    /// a 3-replica app peaked at 6 containers whatever the config said.
    ///
    /// Replay the grill's call log to reconstruct how many instances were live
    /// at each moment, and assert the peak.
    #[tokio::test]
    async fn rolling_redeploy_honours_max_surge() {
        let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();

        // 3 replicas, default max_surge = 1, max_unavailable = 0.
        let config = Config::parse(
            "[app.web]\nimage = \"web:v1\"\nport = 8080\nreplicas = 3\n\n[app.web.deploy]\nmax_surge = 1\nmax_unavailable = 0\ndrain_timeout = \"0s\"\n",
        )
        .unwrap();

        let (ev_tx, mut ev_rx) = mpsc::channel(256);
        agent.deploy(config.clone(), &ev_tx).await;
        drop(ev_tx);
        while ev_rx.recv().await.is_some() {}

        let calls_before = grill.calls().len();
        let (ev_tx, mut ev_rx) = mpsc::channel(256);
        agent.deploy(config, &ev_tx).await;
        drop(ev_tx);
        while ev_rx.recv().await.is_some() {}
        let calls: Vec<(String, InstanceId)> = grill.calls().split_off(calls_before);

        // Replay: a `start` adds a live instance, a `stop`/`kill` removes one.
        // Three old instances are live when the rollout begins.
        let mut live: std::collections::HashSet<String> =
            (0..3).map(|i| format!("default__web-{i}")).collect();
        let mut peak = live.len();
        for (op, id) in &calls {
            match op.as_str() {
                "start" => {
                    live.insert(id.0.clone());
                    peak = peak.max(live.len());
                }
                "stop" | "kill" => {
                    live.remove(&id.0);
                }
                _ => {}
            }
        }

        assert_eq!(
            peak, 4,
            "peaked at {peak} live instances; max_surge = 1 on 3 replicas allows 4 \
             (the old behaviour peaked at 6)"
        );
    }

    /// The mirror: `max_surge = 0` with `max_unavailable = 1` must never
    /// exceed the replica target, retiring before replacing.
    #[tokio::test]
    async fn rolling_redeploy_honours_zero_surge() {
        let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();

        let config = Config::parse(
            "[app.web]\nimage = \"web:v1\"\nport = 8080\nreplicas = 2\n\n[app.web.deploy]\nmax_surge = 0\nmax_unavailable = 1\ndrain_timeout = \"0s\"\n",
        )
        .unwrap();

        let (ev_tx, mut ev_rx) = mpsc::channel(256);
        agent.deploy(config.clone(), &ev_tx).await;
        drop(ev_tx);
        while ev_rx.recv().await.is_some() {}

        let calls_before = grill.calls().len();
        let (ev_tx, mut ev_rx) = mpsc::channel(256);
        agent.deploy(config, &ev_tx).await;
        drop(ev_tx);
        while ev_rx.recv().await.is_some() {}
        let calls: Vec<(String, InstanceId)> = grill.calls().split_off(calls_before);

        let mut live: std::collections::HashSet<String> =
            (0..2).map(|i| format!("default__web-{i}")).collect();
        let mut peak = live.len();
        for (op, id) in &calls {
            match op.as_str() {
                "start" => {
                    live.insert(id.0.clone());
                    peak = peak.max(live.len());
                }
                "stop" | "kill" => {
                    live.remove(&id.0);
                }
                _ => {}
            }
        }

        assert_eq!(
            peak, 2,
            "peaked at {peak}; max_surge = 0 must never exceed the 2-replica target"
        );
    }

    /// A deploy config with no room to move in either direction is refused at
    /// validation rather than wedging a live rollout (M7).
    #[test]
    fn both_deploy_bounds_zero_is_rejected_at_validation() {
        let config = Config::parse(
            "[app.web]\nimage = \"web:v1\"\nreplicas = 2\n\n[app.web.deploy]\nmax_surge = 0\nmax_unavailable = 0\n",
        )
        .unwrap();
        let error = config
            .validate()
            .expect_err("both bounds at zero must not validate");
        let message = error.to_string();
        assert!(
            message.contains("max_surge") && message.contains("max_unavailable"),
            "unhelpful error: {message}"
        );
    }

    // -- Smoker effects and cleanup (CHAOS1) ----------------------------------

    fn fault_rule(fault_type: crate::smoker::types::FaultType) -> crate::smoker::types::FaultRule {
        crate::smoker::types::FaultRule::new(
            crate::smoker::types::FaultId(1),
            fault_type,
            "web".into(),
            std::time::Duration::from_secs(30),
            "test".into(),
        )
    }

    fn register_fault(
        agent: &mut BunAgent<MockGrill>,
        fault_type: crate::smoker::types::FaultType,
        duration: std::time::Duration,
    ) -> crate::smoker::types::FaultRule {
        agent
            .fault_registry
            .insert(&crate::smoker::types::FaultRequest {
                fault_type,
                target_service: String::new(),
                namespace: None,
                target_instance: None,
                target_node: Some("node-a".to_string()),
                duration,
                injected_by: "test".to_string(),
                reason: Some("node fault test".to_string()),
                include_leader: false,
                override_safety: false,
                acknowledged: true,
            })
    }

    #[tokio::test]
    async fn node_drain_stops_scheduling_but_keeps_cluster_transports() {
        let (mut agent, gate, readiness) = test_cluster_fault_agent().await;
        let rule = register_fault(
            &mut agent,
            crate::smoker::types::FaultType::NodeDrain,
            std::time::Duration::from_secs(30),
        );

        agent.apply_fault(&rule).await.unwrap();
        assert!(!gate.is_quiesced(), "drain must keep gossip and Raft alive");
        assert!(
            !readiness.snapshot().await.ready,
            "drain must withdraw scheduler readiness"
        );

        let stored = agent.fault_registry.get(rule.id).cloned().unwrap();
        agent.reverse_fault(&stored).await;
        assert!(readiness.snapshot().await.ready);
    }

    #[tokio::test]
    async fn node_kill_quiesces_all_cluster_transports_and_restores() {
        let (mut agent, gate, _readiness) = test_cluster_fault_agent().await;
        let rule = register_fault(
            &mut agent,
            crate::smoker::types::FaultType::NodeKill {
                kill_containers: false,
            },
            std::time::Duration::from_secs(30),
        );

        agent.apply_fault(&rule).await.unwrap();
        assert!(gate.is_quiesced());

        let stored = agent.fault_registry.get(rule.id).cloned().unwrap();
        agent.reverse_fault(&stored).await;
        assert!(!gate.is_quiesced());
    }

    #[tokio::test]
    async fn node_fault_refuses_without_a_duration() {
        let (mut agent, _gate, _readiness) = test_cluster_fault_agent().await;
        let rule = register_fault(
            &mut agent,
            crate::smoker::types::FaultType::NodeKill {
                kill_containers: false,
            },
            std::time::Duration::ZERO,
        );

        let error = agent
            .apply_fault(&rule)
            .await
            .expect_err("node faults must always be reversible by a deadline");
        assert!(error.contains("duration"));
    }

    #[tokio::test]
    async fn node_pressure_refuses_when_server_limits_are_disabled() {
        let (mut agent, _tx, _shutdown) = test_agent();
        let rule = register_fault(
            &mut agent,
            crate::smoker::types::FaultType::NodePressure {
                cpu_percentage: 80,
                memory_percentage: 90,
            },
            std::time::Duration::from_secs(30),
        );
        let error = agent
            .apply_fault(&rule)
            .await
            .expect_err("pressure must not claim success while server limits are zero");
        assert!(error.contains("configured maximum of 0%"), "{error}");
    }

    #[tokio::test]
    async fn node_fault_clear_needs_explicit_node_authorisation() {
        let (mut agent, gate, _readiness) = test_cluster_fault_agent().await;
        let rule = register_fault(
            &mut agent,
            crate::smoker::types::FaultType::NodeKill {
                kill_containers: false,
            },
            std::time::Duration::from_secs(30),
        );
        agent.apply_fault(&rule).await.unwrap();

        let (response, result) = oneshot::channel();
        agent
            .handle_command(AgentCommand::ClearFault {
                fault_id: rule.id.0,
                allow_workload_fault: false,
                allow_node_fault: false,
                allow_node_pressure: false,
                response,
            })
            .await;
        assert!(result.await.unwrap().is_err());
        assert!(agent.fault_registry.get(rule.id).is_some());
        assert!(gate.is_quiesced());

        let (response, result) = oneshot::channel();
        agent
            .handle_command(AgentCommand::ClearFault {
                fault_id: rule.id.0,
                allow_workload_fault: false,
                allow_node_fault: true,
                allow_node_pressure: false,
                response,
            })
            .await;
        assert!(result.await.unwrap().is_ok());
        assert!(agent.fault_registry.get(rule.id).is_none());
        assert!(!gate.is_quiesced());
    }

    #[tokio::test]
    async fn memory_oom_is_rejected_as_irreversible() {
        // An OOM squeeze isn't a reversible cgroup edit, so we refuse it and
        // point the operator at a Kill fault instead of pretending.
        let (mut agent, _tx, _shutdown) = test_agent();
        let rule = fault_rule(crate::smoker::types::FaultType::MemoryPressure {
            percentage: 100,
            oom: true,
        });
        let err = agent
            .apply_fault(&rule)
            .await
            .expect_err("memory oom must be rejected");
        assert!(err.contains("reversible"), "unexpected reason: {err}");
    }

    #[tokio::test]
    async fn service_partition_without_ebpf_is_refused_not_recorded_as_success() {
        let (mut agent, _tx, _shutdown) = test_agent();
        let rule = fault_rule(crate::smoker::types::FaultType::Partition {
            source_app: Some("web".to_string()),
            source_cgroup_id: 0,
        });
        let error = agent
            .apply_fault(&rule)
            .await
            .expect_err("partition must not claim success without a loaded eBPF path");
        assert!(error.contains("eBPF data path"), "{error}");
    }

    #[tokio::test]
    async fn unimplemented_packet_faults_are_refused_even_if_the_cli_can_describe_them() {
        let (mut agent, _tx, _shutdown) = test_agent();
        let delay = fault_rule(crate::smoker::types::FaultType::Delay {
            delay_ns: 10_000_000,
            jitter_ns: 0,
        });
        let error = agent.apply_fault(&delay).await.unwrap_err();
        assert!(error.contains("TC packet hook"), "{error}");

        let bandwidth = fault_rule(crate::smoker::types::FaultType::Bandwidth {
            bytes_per_sec: 125_000,
        });
        let error = agent.apply_fault(&bandwidth).await.unwrap_err();
        assert!(error.contains("no bandwidth program"), "{error}");
    }

    #[cfg(not(target_os = "linux"))]
    #[tokio::test]
    async fn resource_faults_reject_off_linux() {
        // Off Linux there are no cgroups, so a resource fault reports an
        // honest error instead of recording a fake success.
        let (mut agent, _tx, _shutdown) = test_agent();
        let rule = fault_rule(crate::smoker::types::FaultType::CpuStress {
            percentage: 80,
            cores: None,
        });
        let err = agent
            .apply_fault(&rule)
            .await
            .expect_err("cpu stress must reject without cgroups");
        assert!(err.contains("Linux cgroups"), "unexpected reason: {err}");
    }

    /// Reversing a Pause fault SIGCONTs the frozen process.
    ///
    /// We freeze a real child with SIGSTOP, then drive `reverse_fault` with
    /// the same `Pause` reversal the apply path records. If reversal resumes
    /// the process it exits and `waitpid` reaps it; if it doesn't, the child
    /// stays stopped and the bounded wait never sees an exit — the test fails
    /// on the assertion, not a sleep.
    #[cfg(unix)]
    #[tokio::test]
    async fn clearing_a_pause_resumes_the_process() {
        use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
        use nix::unistd::Pid;

        // A child that exits immediately once it's allowed to run. We reap it
        // via `waitpid` below rather than `Child::wait`, so drop the handle's
        // reaping responsibility to avoid the double-wait clippy flags.
        let child = std::process::Command::new("sh")
            .arg("-c")
            .arg("exit 0")
            .spawn()
            .expect("spawn child");
        let pid = child.id() as i32;
        std::mem::forget(child);
        let nix_pid = Pid::from_raw(pid);

        // Freeze it before it can finish.
        crate::smoker::process::pause_process(pid).expect("pause");

        let mut rule = fault_rule(crate::smoker::types::FaultType::Pause);
        rule.reversal = crate::smoker::types::FaultReversal::Pause(vec![pid]);

        let (mut agent, _tx, _shutdown) = test_agent();
        agent.reverse_fault(&rule).await;

        // Bounded observable wait: poll waitpid until the resumed child exits.
        let mut exited = false;
        for _ in 0..200 {
            match waitpid(nix_pid, Some(WaitPidFlag::WNOHANG)) {
                Ok(WaitStatus::Exited(_, _)) | Ok(WaitStatus::Signaled(_, _, _)) => {
                    exited = true;
                    break;
                }
                Ok(WaitStatus::StillAlive) | Ok(_) => {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
                Err(_) => break,
            }
        }
        assert!(
            exited,
            "paused process was never resumed by reverse_fault — it stayed frozen"
        );
    }

    /// A Pause fault with no reversal recorded (e.g. cleared before it ever
    /// applied) is a no-op, not a panic.
    #[cfg(unix)]
    #[tokio::test]
    async fn reversing_a_pause_without_state_is_a_noop() {
        let (mut agent, _tx, _shutdown) = test_agent();
        let rule = fault_rule(crate::smoker::types::FaultType::Pause);
        // reversal defaults to None; must not panic or error.
        agent.reverse_fault(&rule).await;
    }

    /// M1: the replica-minimum rail must run even with no cluster handle. The
    /// old `build_safety_context` returned `None` there, so `InjectFault`
    /// skipped safety entirely and `fault kill --count 0` could take out a
    /// service's last replica. With a locally-known replica count the rail
    /// fires and the fault is rejected.
    #[tokio::test]
    async fn kill_all_is_refused_for_the_last_replica_without_a_cluster() {
        let (mut agent, _tx, _shutdown, _grill) = test_agent_with_grill();

        // One running replica of "web".
        let config =
            Config::parse("[app.web]\nimage = \"web:v1\"\nport = 8080\nreplicas = 1\n").unwrap();
        let (ev_tx, mut ev_rx) = mpsc::channel(64);
        agent.deploy(config, &ev_tx).await;
        drop(ev_tx);
        while ev_rx.recv().await.is_some() {}

        // `--count 0` means "all replicas"; killing all of a single-replica
        // service leaves zero survivors.
        let request = crate::smoker::types::FaultRequest {
            fault_type: crate::smoker::types::FaultType::Kill { count: 0 },
            target_service: "web".into(),
            namespace: None,
            target_instance: None,
            target_node: None,
            duration: std::time::Duration::from_secs(30),
            injected_by: "test".into(),
            reason: None,
            include_leader: false,
            override_safety: false,
            acknowledged: false,
        };
        let context = agent.build_safety_context(&request).await;
        let check = crate::smoker::safety::evaluate_safety(&request, &context);
        assert!(
            !check.approved,
            "killing the last replica must be refused even with no cluster handle"
        );
        assert!(matches!(
            check.violation,
            Some(crate::smoker::types::SafetyViolation::ReplicaMinimum { .. })
        ));
    }
}
