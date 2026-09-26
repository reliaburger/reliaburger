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

/// Most bytes of an init container's captured stderr carried into its failure.
/// Runc prints why it refused to start (an occupied cgroup, a missing binary)
/// in its last line or two, so a short tail says why without flooding logs.
const INIT_FAILURE_STDERR_BYTES: u64 = 400;

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
#[derive(Debug, Clone, PartialEq)]
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

/// The longest one confirmed stop can take with the production grace, given
/// the runtime's `stop_confirmation_timeout`: a drain of up to one grace, the
/// stop request, the grace itself, then the force-kill and its exit check.
/// Callers that wait for a stop or retirement size their deadline from this.
pub fn stop_completion_bound(confirmation_timeout: std::time::Duration) -> std::time::Duration {
    std::time::Duration::from_secs(STOP_GRACE_SECS) * 2 + confirmation_timeout * 3
}

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

/// Drain then stop a single instance using cloneable handles, so the wait can
/// run on a spawned deploy task instead of the command loop (M7): start the
/// drain (new traffic is routed away; the Wrapper proxy shares this drain
/// tracker, so the wait reflects real in-flight HTTP/WebSocket traffic), let
/// in-flight requests finish (up to `drain_timeout`), then stop and wait for
/// exit, killing if it overruns the grace (DEP5). Every deploy retire — the
/// per-step rolling retire, the rolling scale-down surplus, and the blue-green
/// bulk cut-over — funnels through here on the worker.
async fn drain_and_stop_instance<G: Grill>(
    drains: &crate::wrapper::draining::SharedDrains,
    grill: &G,
    id: &InstanceId,
    drain_timeout: std::time::Duration,
    confirmation_timeout: std::time::Duration,
) -> Result<(), BunError> {
    let cmd = crate::wrapper::draining::DrainCommand {
        app_name: String::new(),
        instance_id: id.0.clone(),
        timeout: drain_timeout,
    };
    drains.start_drain(&cmd).await;
    drains.wait_drained(&id.0).await;

    stop_runtime_instance(grill, id, drain_timeout, confirmation_timeout).await
}

/// Stop one instance, requiring observed exit even after force-kill.
///
/// `confirmation_timeout` (`[runtime] stop_confirmation_timeout_secs`) bounds
/// the runtime's own work: accepting the stop request, accepting a kill, and
/// reporting exit after it. `grace` is the workload's time to exit.
async fn stop_runtime_instance<G: Grill>(
    grill: &G,
    id: &InstanceId,
    grace: std::time::Duration,
    confirmation_timeout: std::time::Duration,
) -> Result<(), BunError> {
    tokio::time::timeout(confirmation_timeout, grill.stop(id))
        .await
        .map_err(|_| BunError::StopUnconfirmed {
            instance_id: id.clone(),
            reason: "graceful stop request timed out",
        })??;
    if observe_runtime_exit(grill, id, grace).await? {
        return Ok(());
    }
    kill_runtime_instance(grill, id, confirmation_timeout).await
}

/// Preserve ownership until both force-kill and observed runtime exit succeed.
async fn kill_runtime_instance<G: Grill>(
    grill: &G,
    id: &InstanceId,
    confirmation_timeout: std::time::Duration,
) -> Result<(), BunError> {
    tokio::time::timeout(confirmation_timeout, grill.kill(id))
        .await
        .map_err(|_| BunError::StopUnconfirmed {
            instance_id: id.clone(),
            reason: "force-kill request timed out",
        })??;
    if observe_runtime_exit(grill, id, confirmation_timeout).await? {
        return Ok(());
    }
    Err(BunError::StopUnconfirmed {
        instance_id: id.clone(),
        reason: "runtime did not confirm exit after force-kill",
    })
}

/// Bound the whole observation loop, including a stalled runtime query.
async fn observe_runtime_exit<G: Grill>(
    grill: &G,
    id: &InstanceId,
    wait: std::time::Duration,
) -> Result<bool, BunError> {
    let observation = async {
        loop {
            if grill.state(id).await? == ContainerState::Stopped {
                return Ok::<(), BunError>(());
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    };
    match tokio::time::timeout(wait, observation).await {
        Ok(result) => result.map(|()| true),
        Err(_) => Ok(false),
    }
}

/// Wait for a replacement instance to become healthy, on the deploy worker
/// (M5): first for the runtime to report `Running`, then — when the app
/// declares a health check — for the HTTP probe itself to pass
/// `threshold_healthy` consecutive times.
///
/// The old wait stopped at `Running`, the runtime's "process alive" view. A
/// version that started but failed its probe was announced healthy, published
/// as a routable backend, and allowed to replace instances that were
/// genuinely serving; its first real probe only ran after the deploy
/// finalised. Apps without a health check keep the `Running`-only wait.
///
/// Probes use the same config as steady-state monitoring afterwards
/// (`HealthCheckConfig::from_spec` on the spec's container port, probed at
/// `probe_host`), honouring `initial_delay` and re-probing at `interval`
/// capped to 500 ms — a deploy gate wants responsiveness, not the
/// steady-state cadence — all bounded by the deploy's `health_timeout`
/// deadline. Returns the failure message for the deploy's error event.
async fn wait_instance_healthy<G: Grill>(
    grill: &G,
    id: &InstanceId,
    spec: &AppSpec,
    container_ip: Option<std::net::Ipv4Addr>,
    wait: std::time::Duration,
) -> Result<(), String> {
    let deadline = std::time::Instant::now() + wait;
    let mut state = grill.state(id).await;
    while std::time::Instant::now() < deadline
        && !matches!(state, Ok(crate::grill::state::ContainerState::Running))
    {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        state = grill.state(id).await;
    }
    match state {
        Ok(crate::grill::state::ContainerState::Running) => {}
        Ok(state) => {
            return Err(format!(
                "{} not healthy (state: {state}), rolling back",
                id.0
            ));
        }
        Err(_) => return Err(format!("{} state unknown, rolling back", id.0)),
    }

    let Some(config) = spec
        .health
        .as_ref()
        .zip(spec.port)
        .map(|(hs, port)| crate::bun::health::HealthCheckConfig::from_spec(hs, port))
    else {
        return Ok(());
    };

    let host = probe_host(container_ip);
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    tokio::time::sleep(config.initial_delay.min(remaining)).await;
    let mut consecutive = 0u32;
    let mut last_status;
    // At least one probe runs even if `initial_delay` consumed the deadline,
    // so a tight `health_timeout` degrades to a single-shot check rather than
    // failing without ever asking the app.
    loop {
        last_status = crate::bun::probe::probe_health(&config, &host)
            .await
            .map_err(|error| format!("{}: {error}", id.0))?;
        if last_status == crate::bun::health::HealthStatus::Healthy {
            consecutive += 1;
            if consecutive >= config.threshold_healthy {
                return Ok(());
            }
        } else {
            consecutive = 0;
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!(
                "{} failed its health check ({last_status:?} at {}:{}{}), rolling back",
                id.0, host, config.port, config.path
            ));
        }
        tokio::time::sleep(
            config
                .interval
                .min(deadline.saturating_duration_since(std::time::Instant::now()))
                .min(std::time::Duration::from_millis(500)),
        )
        .await;
    }
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

/// The outcome of clearing one fault on this node.
#[derive(Debug)]
pub struct FaultClearance {
    /// Human-readable result for the API response.
    pub message: String,
    /// The committed node-fault reservation the fault held, until the leader
    /// has fenced it. The council releases that reservation asynchronously,
    /// so the API waits for it before reporting the clear as complete.
    pub reservation: Option<u64>,
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
    /// Explicit operator authorisation to rerun unknown node-local jobs.
    RerunJobs {
        config: Config,
        events: mpsc::Sender<ApplyEvent>,
    },
    /// Stop all instances of an app in a namespace.
    Stop {
        app_name: String,
        namespace: String,
        response: oneshot::Sender<Result<(), BunError>>,
    },
    /// Stop and retire an app removed from desired state or a resource lease.
    /// Successful retirement also releases its status and port ownership.
    Retire {
        app_name: String,
        namespace: String,
        response: oneshot::Sender<Result<(), BunError>>,
    },
    /// Retire a cleaning lease's runtime and its disposable managed storage.
    RetireTestResources {
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
    /// Get the metrics endpoint of every live local instance whose app
    /// declares `metrics`, for the node's scrape loop.
    ScrapeTargets {
        response: oneshot::Sender<Vec<crate::mayo::scrape::AppScrapeTarget>>,
    },
    /// Get the currently deployed resources in plan format ("app.{name}",
    /// "job.{name}") with their images, for `relish --dry-run` diffing.
    CurrentResources {
        response: oneshot::Sender<Vec<CurrentResourceStatus>>,
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
    /// Request cancellation of an operation owned by this node.
    CancelDeploy {
        operation_id: crate::bun::deploy_operations::DeployOperationId,
        response: oneshot::Sender<Option<crate::bun::deploy_operations::DeployOperation>>,
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
        /// `Some(node)` prefixes every line with `[node instance]`, so lines
        /// from several nodes stay attributable once they're merged.
        label: Option<String>,
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
        generation: u64,
        catalog: Box<crate::onion::catalog::EndpointCatalog>,
        ingress: Vec<crate::cluster::orchestrate::IngressAssignment>,
        response: oneshot::Sender<Result<(), BunError>>,
    },
    /// Reconcile enrolled durable consumer views and exact withdrawal obligations.
    SyncClusterConsumer {
        generation: u64,
        catalog: Box<crate::onion::catalog::EndpointCatalog>,
        ingress: Vec<crate::cluster::orchestrate::IngressAssignment>,
        withdrawals: Vec<crate::onion::withdrawal::EndpointWithdrawalInstruction>,
        /// When the placement request that carried this answer was sent, on
        /// [`crate::onion::lease::boot_clock_ns`]. The view lease runs from here.
        requested_at_ns: u64,
        response: oneshot::Sender<Result<ConsumerUpdate, BunError>>,
    },
    /// Confirm the leader acknowledged one original, locally proven receipt.
    ConfirmConsumerReceipt {
        generation: u64,
        response: oneshot::Sender<Result<(), BunError>>,
    },
    /// List all ingress routes.
    Routes {
        response: oneshot::Sender<Vec<crate::wrapper::types::RouteInfo>>,
    },
    /// Prepare the canonical request and identify this target process.
    PrepareNodeFault {
        request: crate::smoker::types::FaultRequest,
        response: oneshot::Sender<Result<(String, crate::smoker::types::FaultRequest), BunError>>,
    },
    /// Fence delayed activation and confirm reversal before releasing capacity.
    FenceNodeFault {
        only_if_finished: bool,
        reservation: crate::smoker::reservation::NodeFaultReservation,
        response: oneshot::Sender<Result<(), BunError>>,
    },
    /// Apply a workload fault, or a node fault carrying a committed grant.
    InjectFault {
        /// Boxed: a reservation embeds a whole fault request, and keeping it
        /// inline would make every other command as large as this one.
        reservation: Option<Box<crate::smoker::reservation::NodeFaultReservation>>,
        request: crate::smoker::types::FaultRequest,
        /// Cluster-wide replica counts for a workload fault, gathered by the
        /// API from every node. `None` falls back to this node's own view.
        replica_evidence: Option<crate::smoker::types::ReplicaEvidence>,
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
        response: oneshot::Sender<Result<FaultClearance, BunError>>,
    },
    /// Clear all active faults.
    ClearAllFaults {
        response: oneshot::Sender<Result<String, BunError>>,
    },
    /// Clear every active fault targeting a given service. `namespace`
    /// confines the clear to one tenant (`None` clears the service in every
    /// namespace, which the API allows only for unscoped tokens).
    ClearFaultsByService {
        service: String,
        namespace: Option<String>,
        response: oneshot::Sender<Result<String, BunError>>,
    },
    /// List all active faults.
    ListFaults {
        response: oneshot::Sender<Vec<crate::smoker::types::FaultSummary>>,
    },
    /// Verify an operator's detached image signature (made by `relish sign`
    /// with a key the cluster never sees) and attach it via Raft.
    SignImage {
        submission: crate::pickle::signing::SignatureSubmission,
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
        rejoin: Result<(), String>,
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
    /// A prerequisite's observed success must be durable before its dependent app runs.
    ConfirmJobSuccess {
        instance_id: InstanceId,
        reply: oneshot::Sender<Result<(), BunError>>,
    },
    /// A bounded probe completes off-loop; only the agent mutates health state.
    HealthProbeResult {
        instance_id: InstanceId,
        created_at: Instant,
        status: Result<super::health::HealthStatus, super::probe::ProbeError>,
    },
    /// Enforce the image trust policy; returns the digest-pinned image, if any.
    EnforceImageSignature {
        spec: Box<AppSpec>,
        reply: oneshot::Sender<Result<Option<String>, String>>,
    },
    /// Admit the app kind before recording the deployed spec for the Brioche UI.
    StoreDeployedSpec {
        app_name: String,
        namespace: String,
        spec: Box<AppSpec>,
        reply: oneshot::Sender<Result<(), BunError>>,
    },
    /// Every owned instance id, including terminal instances awaiting cleanup.
    ListExistingOwned {
        app_name: String,
        namespace: String,
        reply: oneshot::Sender<Vec<InstanceId>>,
    },
    /// Reserve and return the next rolling-redeploy generation counter.
    NextDeployGen {
        app_name: String,
        reply: oneshot::Sender<Result<u64, BunError>>,
    },
    /// Create supervisor-tracked instances for a fresh app deploy.
    SupervisorDeployApp {
        app_name: String,
        namespace: String,
        spec: Box<AppSpec>,
        reply: oneshot::Sender<Result<Vec<InstanceId>, BunError>>,
    },
    /// Create supervisor-tracked instances for a job deploy.
    SupervisorDeployJob {
        rerun_unknown: bool,
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
        reply: oneshot::Sender<Result<(), BunError>>,
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
    /// Reserve an auxiliary identity before its runtime can be created.
    RegisterInitialiser {
        instance_id: InstanceId,
        index: usize,
        reply: oneshot::Sender<Result<InstanceId, BunError>>,
    },
    /// Release an initialiser only after confirmed runtime retirement.
    ForgetInitialiser {
        instance_id: InstanceId,
        initialiser: InstanceId,
        reply: oneshot::Sender<Result<(), BunError>>,
    },
    /// Program source and egress policy before create → program → start. On
    /// failure the caller stops the created container and fails the deploy.
    ApplyNetworkPreStart {
        instance_id: InstanceId,
        app_name: String,
        spec: Option<Box<AppSpec>>,
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
    /// Claim replacement ownership before allocating identity or runtime resources.
    ReserveRollingInstance {
        instance_id: InstanceId,
        app_name: String,
        namespace: String,
        spec: Box<AppSpec>,
        reply: oneshot::Sender<Result<Option<u16>, BunError>>,
    },
    /// Fast pre-create bookkeeping for a rolling-redeploy instance: fail closed
    /// on undecryptable secrets, prepare its identity dir, build the OCI spec.
    PrepareRollingInstance {
        instance_id: InstanceId,
        app_name: String,
        namespace: String,
        spec: Box<AppSpec>,
        host_port: Option<u16>,
        reply: oneshot::Sender<Result<crate::grill::oci::OciSpec, BunError>>,
    },
    /// Persist a started replacement before health wait or traffic publication.
    RegisterRollingInstance {
        instance: Box<RollingInstance>,
        reply: oneshot::Sender<Result<(), BunError>>,
    },
    /// Keep a started replacement reachable through ordinary Stop after a failed cut-over.
    RetainRollingInstance {
        instance: Box<RollingInstance>,
        reply: oneshot::Sender<Result<(), BunError>>,
    },
    /// Forget the already-stopped old instances and register the healthy new
    /// ones: service map, health config, backends, kernel networking, ingress,
    /// history. Bookkeeping only — the deploy worker drains and stops the old
    /// instances off the command loop before sending this (M7).
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
        reply: oneshot::Sender<Result<(), BunError>>,
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
        reply: oneshot::Sender<Result<(), BunError>>,
    },
    /// Finish retiring one old instance: the fast `&mut self` bookkeeping
    /// (lift egress, clean identity, drop the record + supervisor entry) after
    /// the deploy worker has already drained and stopped it off the command
    /// loop (M7). The drain/stop wait used to run here on the loop, stalling
    /// every command for its duration per retired instance.
    FinishRetire {
        old_id: InstanceId,
        reply: oneshot::Sender<Result<(), BunError>>,
    },
    /// Fence restarts before the worker starts draining or signalling an old instance.
    BeginRetire {
        old_id: InstanceId,
        reply: oneshot::Sender<Result<(), BunError>>,
    },
    /// Hand a stopped old instance whose addresses still await remote
    /// withdrawal confirmations to the agent loop, so the rollout can finish.
    DeferRetire {
        old_id: InstanceId,
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

/// Launch data owned by the deploy worker before supervisor registration.
struct RollingInstance {
    instance_id: InstanceId,
    app_name: String,
    namespace: String,
    spec: AppSpec,
    oci_spec: crate::grill::oci::OciSpec,
    host_port: Option<u16>,
}

/// The fast pre-create outputs the loop hands back for a fresh instance.
struct PreparedInstance {
    oci_spec: crate::grill::oci::OciSpec,
    cgroup_path: PathBuf,
    has_init: bool,
}

/// How long a deploy worker keeps asking the leader to release a retired
/// instance's addresses before it hands the release to the agent loop and
/// carries on. Consumers confirm withdrawals on their placement poll, every
/// couple of seconds, so a healthy cluster answers well within it; a lost
/// node holds it up until the leader discharges it (`onion::lease`).
const PRODUCER_RELEASE_PATIENCE: std::time::Duration = std::time::Duration::from_secs(5);

/// Pause between two producer release attempts.
const PRODUCER_RELEASE_RETRY: std::time::Duration = std::time::Duration::from_secs(1);

/// Run `attempt` until it stops reporting a pending producer release, or
/// until `patience` runs out; returns the last outcome either way.
async fn retry_while_release_pending<F, Fut>(
    patience: std::time::Duration,
    interval: std::time::Duration,
    mut attempt: F,
) -> Result<(), BunError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), BunError>>,
{
    let deadline = tokio::time::Instant::now() + patience;
    loop {
        match attempt().await {
            Err(BunError::ProducerReleasePending { .. })
                if tokio::time::Instant::now() + interval < deadline =>
            {
                tokio::time::sleep(interval).await;
            }
            outcome => return outcome,
        }
    }
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

    async fn store_deployed_spec(
        &self,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
    ) -> Result<(), BunError> {
        self.call(
            |reply| DeployOp::StoreDeployedSpec {
                app_name: app_name.to_string(),
                namespace: namespace.to_string(),
                spec: Box::new(spec.clone()),
                reply,
            },
            Err(BunError::DeployFailed {
                app_name: app_name.into(),
                reason: "agent shutting down".into(),
            }),
        )
        .await
    }

    async fn list_existing_owned(&self, app_name: &str, namespace: &str) -> Vec<InstanceId> {
        self.call(
            |reply| DeployOp::ListExistingOwned {
                app_name: app_name.to_string(),
                namespace: namespace.to_string(),
                reply,
            },
            Vec::new(),
        )
        .await
    }

    async fn next_deploy_gen(&self, app_name: &str) -> Result<u64, BunError> {
        self.call(
            |reply| DeployOp::NextDeployGen {
                app_name: app_name.into(),
                reply,
            },
            Err(BunError::DeployFailed {
                app_name: app_name.into(),
                reason: "agent loop closed before reserving rollout identity".into(),
            }),
        )
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

    async fn confirm_job_success(&self, instance_id: &InstanceId) -> Result<(), BunError> {
        self.call(
            |reply| DeployOp::ConfirmJobSuccess {
                instance_id: instance_id.clone(),
                reply,
            },
            Err(BunError::JobState(
                "agent unavailable before job success was persisted".into(),
            )),
        )
        .await
    }

    async fn supervisor_deploy_job(
        &self,
        job_name: &str,
        namespace: &str,
        spec: &JobSpec,
        rerun_unknown: bool,
    ) -> Result<Vec<InstanceId>, BunError> {
        self.call(
            |reply| DeployOp::SupervisorDeployJob {
                rerun_unknown,
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
    ) -> Result<(), BunError> {
        self.call(
            |reply| DeployOp::RegisterServiceApp {
                app_name: app_name.to_string(),
                namespace: namespace.to_string(),
                port,
                firewall,
                reply,
            },
            Err(BunError::BackendPublication {
                service: crate::onion::service_id::ServiceId::new(namespace, app_name),
                reason: "agent loop closed before service registration".into(),
            }),
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

    async fn register_initialiser(
        &self,
        instance_id: &InstanceId,
        index: usize,
    ) -> Result<InstanceId, BunError> {
        self.call(
            |reply| DeployOp::RegisterInitialiser {
                instance_id: instance_id.clone(),
                index,
                reply,
            },
            Err(BunError::InstanceNotFound {
                instance_id: instance_id.clone(),
            }),
        )
        .await
    }

    async fn forget_initialiser(
        &self,
        instance_id: &InstanceId,
        initialiser: &InstanceId,
    ) -> Result<(), BunError> {
        self.call(
            |reply| DeployOp::ForgetInitialiser {
                instance_id: instance_id.clone(),
                initialiser: initialiser.clone(),
                reply,
            },
            Err(BunError::InstanceNotFound {
                instance_id: instance_id.clone(),
            }),
        )
        .await
    }

    async fn apply_network_pre_start(
        &self,
        instance_id: &InstanceId,
        app_name: &str,
        spec: Option<&AppSpec>,
        cgroup_path: &std::path::Path,
    ) -> Result<(), BunError> {
        self.call(
            |reply| DeployOp::ApplyNetworkPreStart {
                instance_id: instance_id.clone(),
                app_name: app_name.to_string(),
                spec: spec.cloned().map(Box::new),
                cgroup_path: cgroup_path.to_path_buf(),
                reply,
            },
            Err(BunError::InstanceNotFound {
                instance_id: instance_id.clone(),
            }),
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
    async fn reserve_rolling_instance(
        &self,
        instance_id: &InstanceId,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
    ) -> Result<Option<u16>, BunError> {
        self.call(
            |reply| DeployOp::ReserveRollingInstance {
                instance_id: instance_id.clone(),
                app_name: app_name.into(),
                namespace: namespace.into(),
                spec: Box::new(spec.clone()),
                reply,
            },
            Err(BunError::InstanceNotFound {
                instance_id: instance_id.clone(),
            }),
        )
        .await
    }

    async fn prepare_rolling_instance(
        &self,
        instance_id: &InstanceId,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
        host_port: Option<u16>,
    ) -> Result<crate::grill::oci::OciSpec, BunError> {
        self.call(
            |reply| DeployOp::PrepareRollingInstance {
                instance_id: instance_id.clone(),
                app_name: app_name.to_string(),
                namespace: namespace.to_string(),
                spec: Box::new(spec.clone()),
                host_port,
                reply,
            },
            Err(BunError::InstanceNotFound {
                instance_id: instance_id.clone(),
            }),
        )
        .await
    }

    async fn register_rolling_instance(&self, instance: RollingInstance) -> Result<(), BunError> {
        let missing = BunError::InstanceNotFound {
            instance_id: instance.instance_id.clone(),
        };
        self.call(
            |reply| DeployOp::RegisterRollingInstance {
                instance: Box::new(instance),
                reply,
            },
            Err(missing),
        )
        .await
    }

    async fn retain_rolling_instance(&self, instance: RollingInstance) -> Result<(), BunError> {
        let missing = BunError::InstanceNotFound {
            instance_id: instance.instance_id.clone(),
        };
        self.call(
            |reply| DeployOp::RetainRollingInstance {
                instance: Box::new(instance),
                reply,
            },
            Err(missing),
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
    ) -> Result<(), BunError> {
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
            Err(BunError::DeployFailed {
                app_name: app_name.into(),
                reason: "agent loop closed before finalisation".into(),
            }),
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
    ) -> Result<(), BunError> {
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
            Err(BunError::BackendPublication {
                service: crate::onion::service_id::ServiceId::new(namespace, app_name),
                reason: "agent loop closed before backend publication".into(),
            }),
        )
        .await
    }

    async fn begin_retire(&self, old_id: &InstanceId) -> Result<(), BunError> {
        self.call(
            |reply| DeployOp::BeginRetire {
                old_id: old_id.clone(),
                reply,
            },
            Err(BunError::RetirementState {
                instance_id: old_id.clone(),
                reason: "agent loop closed before retirement began".into(),
            }),
        )
        .await
    }

    /// Bookkeeping-only op sent after the worker has already drained+stopped
    /// the instance off the loop (M7).
    ///
    /// On a multi-node cluster the leader answers the first producer release
    /// with "pending" until every node confirms the old endpoint's
    /// withdrawal, which takes a placement poll or two. That's the normal
    /// case, not a failure, so the worker asks again for a while instead of
    /// failing the deploy (which would start yet another generation of
    /// replacements). The loop stays free between attempts, so this node can
    /// deliver its own receipt meanwhile.
    async fn finish_retire(&self, old_id: &InstanceId) -> Result<(), BunError> {
        retry_while_release_pending(PRODUCER_RELEASE_PATIENCE, PRODUCER_RELEASE_RETRY, || {
            self.call(
                |reply| DeployOp::FinishRetire {
                    old_id: old_id.clone(),
                    reply,
                },
                Err(BunError::RetirementState {
                    instance_id: old_id.clone(),
                    reason: "agent loop closed before retirement".into(),
                }),
            )
        })
        .await
    }

    /// Let the agent loop finish releasing a stopped old instance's addresses
    /// once every node has confirmed the withdrawal.
    async fn defer_retire(&self, old_id: &InstanceId) {
        self.call(
            |reply| DeployOp::DeferRetire {
                old_id: old_id.clone(),
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

/// Result of a deploy operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApplyResult {
    /// Number of instances created.
    pub created: usize,
    /// Instance IDs that were created.
    pub instances: Vec<String>,
}

/// One currently deployed resource in the CLI plan's identifier format,
/// served by `GET /v1/apps` for `relish apply --dry-run` diffing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CurrentResourceStatus {
    /// Plan-format identifier: "app.{name}", "job.{name}",
    /// "namespace.{name}" or "permission.{name}".
    pub resource: String,
    /// Image currently deployed, when the resource kind has one.
    pub image: Option<String>,
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

/// A workload status with the node that supplied it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterInstanceStatus {
    /// Node name, or `local` for a standalone agent.
    pub node: String,
    /// Node-local workload evidence.
    #[serde(flatten)]
    pub instance: InstanceStatus,
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
    /// Agent API endpoint supplied by the cluster's resolved peer directory.
    /// Missing evidence must not be replaced with a guessed port by clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_address: Option<std::net::SocketAddr>,
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
    /// Original node identity, available before council membership is established.
    pub local_node_id: crate::meat::NodeId,
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

#[cfg(all(feature = "ebpf", target_os = "linux"))]
use super::egress_owners::{EgressBinding, PolicyPhase};
mod app_stop;
mod consumer;
mod startup_recovery;
pub use consumer::ConsumerUpdate;
mod discovery_ownership;
mod discovery_recovery;
mod egress_ownership;
mod producer_release;
mod runtime_inventory;
use app_stop::{AppStop, PendingStops, StopPurpose};
use discovery_ownership::DiscoveryOwnership;
use runtime_inventory::{LOOP_RUNTIME_INVENTORY_TIMEOUT, RUNTIME_INVENTORY_TIMEOUT};

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
    /// Active faults that act on this source's calls to the destination.
    faults: Vec<crate::onion::trace::PathFault>,
    /// TCP connects to make.
    count: u32,
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    onion_ebpf: Option<std::sync::Arc<tokio::sync::Mutex<crate::onion::ebpf::loader::OnionEbpf>>>,
}

/// What this node has installed for its active network faults.
///
/// Network faults are reconciled rather than written once: every change to the
/// fault set or the local instances recomputes the desired state and applies
/// only the difference against what is recorded here.
#[derive(Debug, Default)]
struct InstalledNetworkFaults {
    /// `fault_connect_map` entries this node wrote.
    connect: std::collections::BTreeMap<
        crate::smoker::network::ConnectFaultKey,
        crate::smoker::network::ConnectFaultEntry,
    >,
    /// Proven workload cgroup per caller instance, with the restart count it
    /// was read at, so a restarted container is looked up again.
    caller_cgroups: std::collections::HashMap<InstanceId, (u32, u64)>,
    /// netem delay bands installed per caller instance id, with the restart
    /// count they were installed at.
    delays: std::collections::HashMap<String, (u32, Vec<crate::smoker::network::DelayBand>)>,
    /// Whether this Bun has swept delay trees a previous Bun left behind.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    delays_swept: bool,
}

/// The Bun agent. Generic over `G: Grill` so tests can inject mocks.
pub struct BunAgent<G: Grill> {
    supervisor: WorkloadSupervisor<G>,
    command_rx: mpsc::Receiver<AgentCommand>,
    shutdown: CancellationToken,
    /// Process-wide long-lived-task evidence shared with the API and reporter.
    readiness: Option<crate::bun::readiness::ReadinessTracker>,
    #[cfg(test)]
    egress_observation_count: std::sync::atomic::AtomicUsize,
    /// Hard per-node concurrency bound for workload connectivity traces.
    trace_slots: std::sync::Arc<tokio::sync::Semaphore>,
    volumes_dir: PathBuf,
    cluster: Option<ClusterHandle>,
    /// Immutable cluster identity used as every workload SPIFFE trust domain.
    trust_domain: String,
    /// Smoker fault registry — active faults on this node.
    fault_registry: crate::smoker::registry::FaultRegistry,
    /// Kernel state this node has installed for its active network faults.
    network_faults: InstalledNetworkFaults,
    /// Smoker duration limits (`[smoker]`): default + maximum fault lifetime.
    smoker_config: crate::smoker::config::SmokerConfig,
    /// Node leaf lifetime this member signs joining nodes' certificates with
    /// (`[security] leaf_lifetime_override_secs`, else the one-year default).
    node_leaf_lifetime: std::time::Duration,
    /// Reference-counted node drains, independent from binary-upgrade drains.
    node_fault_fence: crate::smoker::reservation::NodeFaultFence,
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
    /// Block policy mutations until restart resolves an uncertain checkpoint write.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    egress_store_uncertain: bool,
    /// Workloads fenced by the current live-enforcement incident. Kept after
    /// stop so the next capability report records what happened; cleared only
    /// after every required hook and pre-start guarantee recovers.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    egress_affected_workloads: std::collections::BTreeSet<(String, String)>,
    /// Ticks since the last egress re-resolution (the event loop runs at 1s).
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    egress_reresolve_ticks: u32,
    /// Kernel-truth sweep interval in seconds (`[ebpf] sweep_interval_secs`,
    /// 0 disables). The sweep reconciles external egress against original
    /// owners. Namespace retirement requires explicit source ownership.
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
    /// Exclusive publication checkpoint, or a fence after an uncertain write.
    discovery_ownership: DiscoveryOwnership,
    /// Enrolled transport used by the opt-in durable producer retirement gate.
    producer_release_client: Option<crate::cluster::producer::ProducerReleaseClient>,
    /// Forwards workload CSRs to the leader when this node isn't it.
    workload_csr_client: Option<crate::cluster::workload_identity::WorkloadCsrClient>,
    /// Producer release confirmations still waiting for the leader, keyed by
    /// the execution they would release. Dropping one aborts its request.
    producer_releases: std::collections::HashMap<
        crate::grill::RuntimeExecution,
        tokio_util::task::AbortOnDropHandle<
            Result<crate::cluster::producer::ProducerRelease, String>,
        >,
    >,
    /// Cluster-wide endpoint catalogue (12b.4), replicated from the leader.
    /// Overlaid onto the local `service_map` when publishing the DNS/routing
    /// snapshot so this node resolves services whose backends live elsewhere.
    /// Empty on a single node — the local map is then the whole picture.
    cluster_catalog: crate::onion::catalog::EndpointCatalog,
    /// Last confirmed catalogue generation; restart recovery must restore its durable fence.
    cluster_catalog_generation: Option<u64>,
    /// Publisher for service-map snapshots (DNS responder subscribes).
    service_map_tx: tokio::sync::watch::Sender<crate::onion::service_map::ServiceMap>,
    /// Publisher for the set of services under a Smoker `DnsNxdomain` fault.
    ///
    /// DNS lives in the userspace responder now (the in-kernel DNS eBPF
    /// object was never loaded), so the fault does too: we republish this on
    /// every apply/clear/expire and the responder returns NXDOMAIN for any
    /// service in the set. See [`crate::onion::dns::DnsFaultState`].
    dns_faults_tx: tokio::sync::watch::Sender<crate::onion::dns::DnsFaultState>,
    /// Wrapper routing table (shared with the proxy via `Arc<RwLock<_>>`).
    routing_table: std::sync::Arc<tokio::sync::RwLock<crate::wrapper::routing::RoutingTable>>,
    /// Ingress configs for deployed apps (app_name → IngressSpec).
    /// Ingress specs keyed by `(namespace, app_name)` so same-named apps
    /// in different namespaces route independently (D3/codex-M1).
    ingress_configs: std::collections::HashMap<(String, String), crate::config::app::IngressSpec>,
    cluster_ingress_configs:
        std::collections::HashMap<(String, String), crate::config::app::IngressSpec>,
    /// A local change awaits in-place republication of the consumer view.
    consumer_view_stale: bool,
    /// While the view lease has lapsed, the local-only view installed in
    /// place of the last publication: this node's own backends and nothing
    /// else. `None` while the published view is the whole cluster's.
    lapsed_view: Option<Vec<crate::onion::types::ServiceEntry>>,
    /// Stopped instances retired by a finished rollout whose addresses still
    /// wait for other nodes to confirm the withdrawal. The loop releases them.
    deferred_retirements: std::collections::HashSet<InstanceId>,
    /// How long this node may keep routing with its published cluster view
    /// (shared with Wrapper, mirrored into the kernel's `view_lease_map`).
    view_lease: std::sync::Arc<crate::onion::lease::ViewLease>,
    /// Journal to reopen after a discovery write whose outcome is unknown,
    /// and whether it had been recovered from an earlier process.
    discovery_reopen: Option<(std::path::PathBuf, bool)>,
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
    /// Initialisers whose runtime must retire before parent policy and records.
    initialisers: std::collections::HashMap<InstanceId, std::collections::HashSet<InstanceId>>,
    /// Captured before startup; a recovered runtime hold needs original discovery
    /// reconciliation rather than an empty in-memory map authorising release.
    /// Original executions awaiting cluster cleanup after the API becomes available.
    startup_retirements: std::collections::VecDeque<crate::grill::RuntimeLaunch>,
    /// Keep admission fenced until empty-allocation retirement also succeeds.
    startup_cleanup_pending: bool,
    network_references:
        std::collections::HashMap<InstanceId, crate::grill::runc_intent::NetworkReference>,
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
    /// same second; reservations advance beyond both this counter and all restored owners.
    next_deploy_gen: u64,
    /// Jobs carrying a `schedule`, registered on apply and fired by the cron
    /// tick. Keyed by (name, namespace) so a re-apply replaces the entry.
    scheduled_jobs: std::collections::HashMap<(String, String), ScheduledJob>,
    /// A failed or cancelled write must be resolved by reloading at startup.
    scheduled_jobs_store_uncertain: bool,
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
    recorded_jobs: BTreeMap<String, super::jobs::RecordedJob>,
    job_store_uncertain: bool,
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
    /// At most one outstanding health probe per instance identity.
    health_inflight: std::collections::HashSet<InstanceId>,
    /// Shared drain tracker (DEP5). Handed to the Wrapper proxy so in-flight
    /// requests to a retiring backend are counted; the retire path starts a
    /// drain and waits for it to finish (or time out) before killing the
    /// old container.
    drains: crate::wrapper::draining::SharedDrains,
    /// Per-step deadline for the runtime to confirm a stop or force-kill
    /// (`[runtime] stop_confirmation_timeout_secs`).
    stop_confirmation_timeout: std::time::Duration,
    /// How long an ordinary stop waits after SIGTERM before SIGKILL.
    /// `STOP_GRACE_SECS` unless a test shortens it with `set_stop_grace`.
    stop_grace: std::time::Duration,
    /// The same wait for node shutdown: `SHUTDOWN_GRACE_SECS` by default.
    shutdown_grace: std::time::Duration,
    /// Operator stops and retirements whose exit is still being awaited.
    pending_stops: PendingStops,
    /// Their exit waits, off the command loop so a workload that ignores
    /// SIGTERM can't stall every other command for its grace.
    stop_waits: tokio::task::JoinSet<Result<(), BunError>>,
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
            #[cfg(test)]
            egress_observation_count: std::sync::atomic::AtomicUsize::new(0),
            trace_slots: std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_TRACES)),
            volumes_dir: crate::config::node::StorageSection::default().volumes,
            cluster: None,
            trust_domain: "default".to_string(),
            fault_registry: crate::smoker::registry::FaultRegistry::new(),
            network_faults: InstalledNetworkFaults::default(),
            smoker_config: crate::smoker::config::SmokerConfig::default(),
            node_leaf_lifetime: crate::sesame::ca::NODE_LEAF_LIFETIME,
            stop_confirmation_timeout: crate::config::node::RuntimeSection::default()
                .stop_confirmation_timeout(),
            node_fault_fence: crate::smoker::reservation::NodeFaultFence::default(),
            node_drain_gate: crate::smoker::node_fault::NodeDrainGate::new(),
            node_pressure: crate::smoker::node_pressure::NodePressureController::default(),
            #[cfg(all(feature = "ebpf", target_os = "linux"))]
            onion_ebpf: None,
            #[cfg(all(feature = "ebpf", target_os = "linux"))]
            egress_bindings: std::collections::HashMap::new(),
            #[cfg(all(feature = "ebpf", target_os = "linux"))]
            egress_store_uncertain: false,
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
            discovery_ownership: DiscoveryOwnership::default(),
            producer_release_client: None,
            workload_csr_client: None,
            producer_releases: std::collections::HashMap::new(),
            cluster_catalog: crate::onion::catalog::EndpointCatalog::new(),
            cluster_catalog_generation: None,
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
            cluster_ingress_configs: std::collections::HashMap::new(),
            consumer_view_stale: false,
            lapsed_view: None,
            deferred_retirements: Default::default(),
            view_lease: Default::default(),
            discovery_reopen: None,
            // Single-node mode: no nftables needed (no cluster ports to protect)
            perimeter_config: crate::firewall::rules::PerimeterConfig {
                enabled: false,
                ..Default::default()
            },
            last_firewall_nodes: None,
            deploy_history: Arc::new(tokio::sync::RwLock::new(Vec::new())),
            deploy_operations: crate::bun::deploy_operations::DeployOperationTracker::default(),
            initialisers: std::collections::HashMap::new(),
            startup_retirements: Default::default(),
            startup_cleanup_pending: false,
            network_references: std::collections::HashMap::new(),
            netns_paths: std::collections::HashMap::new(),
            deployed_specs: std::collections::HashMap::new(),
            next_deploy_gen: 1,
            scheduled_jobs: std::collections::HashMap::new(),
            scheduled_jobs_store_uncertain: false,
            log_tx: None,
            events: None,
            capacity_cpu_millicores: 0,
            capacity_memory_mb: 0,
            trust_policy: crate::config::node::TrustPolicySection::default(),
            records_dir: None,
            recorded_jobs: BTreeMap::new(),
            job_store_uncertain: false,
            upgrade: None,
            draining: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            identity_retry_ticks: 0,
            deploy_ops_tx,
            deploy_ops_rx,
            health_inflight: std::collections::HashSet::new(),
            drains: new_shared_drains(),
            stop_grace: std::time::Duration::from_secs(STOP_GRACE_SECS),
            shutdown_grace: std::time::Duration::from_secs(SHUTDOWN_GRACE_SECS),
            pending_stops: PendingStops::new(),
            stop_waits: tokio::task::JoinSet::new(),
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
            #[cfg(test)]
            egress_observation_count: std::sync::atomic::AtomicUsize::new(0),
            trace_slots: std::sync::Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_TRACES)),
            volumes_dir: crate::config::node::StorageSection::default().volumes,
            cluster: Some(cluster),
            trust_domain,
            fault_registry: crate::smoker::registry::FaultRegistry::new(),
            network_faults: InstalledNetworkFaults::default(),
            smoker_config: crate::smoker::config::SmokerConfig::default(),
            node_leaf_lifetime: crate::sesame::ca::NODE_LEAF_LIFETIME,
            stop_confirmation_timeout: crate::config::node::RuntimeSection::default()
                .stop_confirmation_timeout(),
            node_fault_fence: crate::smoker::reservation::NodeFaultFence::default(),
            node_drain_gate: crate::smoker::node_fault::NodeDrainGate::new(),
            node_pressure: crate::smoker::node_pressure::NodePressureController::default(),
            #[cfg(all(feature = "ebpf", target_os = "linux"))]
            onion_ebpf: None,
            #[cfg(all(feature = "ebpf", target_os = "linux"))]
            egress_bindings: std::collections::HashMap::new(),
            #[cfg(all(feature = "ebpf", target_os = "linux"))]
            egress_store_uncertain: false,
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
            discovery_ownership: DiscoveryOwnership::default(),
            producer_release_client: None,
            workload_csr_client: None,
            producer_releases: std::collections::HashMap::new(),
            cluster_catalog: crate::onion::catalog::EndpointCatalog::new(),
            cluster_catalog_generation: None,
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
            cluster_ingress_configs: std::collections::HashMap::new(),
            consumer_view_stale: false,
            lapsed_view: None,
            deferred_retirements: Default::default(),
            view_lease: Default::default(),
            discovery_reopen: None,
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
            initialisers: std::collections::HashMap::new(),
            startup_retirements: Default::default(),
            startup_cleanup_pending: false,
            network_references: std::collections::HashMap::new(),
            netns_paths: std::collections::HashMap::new(),
            deployed_specs: std::collections::HashMap::new(),
            next_deploy_gen: 1,
            scheduled_jobs: std::collections::HashMap::new(),
            scheduled_jobs_store_uncertain: false,
            log_tx: None,
            events: None,
            capacity_cpu_millicores: 0,
            capacity_memory_mb: 0,
            trust_policy: crate::config::node::TrustPolicySection::default(),
            records_dir: None,
            recorded_jobs: BTreeMap::new(),
            job_store_uncertain: false,
            upgrade: None,
            draining: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            identity_retry_ticks: 0,
            deploy_ops_tx,
            deploy_ops_rx,
            health_inflight: std::collections::HashSet::new(),
            drains: new_shared_drains(),
            stop_grace: std::time::Duration::from_secs(STOP_GRACE_SECS),
            shutdown_grace: std::time::Duration::from_secs(SHUTDOWN_GRACE_SECS),
            pending_stops: PendingStops::new(),
            stop_waits: tokio::task::JoinSet::new(),
        }
    }

    /// A shared drain handle for the Wrapper proxy, so it counts in-flight
    /// requests to backends this agent is retiring (DEP5). The completion
    /// channel's receiver is dropped: the retire path waits via
    /// `wait_drained`, not the notification stream.
    pub fn drains_handle(&self) -> crate::wrapper::draining::SharedDrains {
        self.drains.clone()
    }

    /// The lease Wrapper checks before routing a cluster request.
    pub fn view_lease_handle(&self) -> std::sync::Arc<crate::onion::lease::ViewLease> {
        self.view_lease.clone()
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
    /// apply, clear, or expiry. Namespace-qualified identities prevent an
    /// authorised fault in one tenant from affecting another tenant's service.
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
            .filter_map(|rule| {
                Some((
                    crate::onion::service_id::ServiceId::new(
                        rule.namespace.as_ref()?,
                        &rule.target_service,
                    ),
                    rule.expires_at_ns,
                ))
            });
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

    /// Set the node leaf lifetime this member signs joining nodes with.
    pub fn set_node_leaf_lifetime(&mut self, lifetime: std::time::Duration) {
        self.node_leaf_lifetime = lifetime;
    }

    /// Thread `[runtime] stop_confirmation_timeout_secs` in: how long each
    /// step of a stop or force-kill may wait for the runtime to confirm it.
    pub fn set_stop_confirmation_timeout(&mut self, timeout: std::time::Duration) {
        self.stop_confirmation_timeout = timeout;
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
    /// Create volume snapshots. Free of `&self` (takes `volumes_dir`) so it can
    /// run on `spawn_blocking` — btrfs subprocess + fs walks must not run on the
    /// agent command loop (M7).
    fn snapshot_create(
        volumes_dir: &std::path::Path,
        namespace: &str,
        app_name: &str,
        volume: Option<String>,
        name: Option<String>,
    ) -> Result<Vec<crate::grill::snapshot::SnapshotMeta>, BunError> {
        let volumes = match volume {
            Some(v) => vec![v],
            None => {
                let found = crate::grill::volume::VolumeManager::new(volumes_dir)
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

        let manager = crate::grill::snapshot::SnapshotManager::new(volumes_dir);
        let now = std::time::SystemTime::now();
        let mut metas = Vec::with_capacity(volumes.len());
        for volume_path in &volumes {
            metas.push(manager.create(namespace, app_name, volume_path, name.as_deref(), now)?);
        }
        Ok(metas)
    }

    /// Configure the actual protected listener ports and explicit enrolment peers.
    /// This grants network reachability only; protocol authentication still applies.
    pub fn configure_perimeter(
        &mut self,
        cluster_ports: Vec<u16>,
        management_port: u16,
        bootstrap_peers: Vec<std::net::IpAddr>,
    ) {
        self.perimeter_config.cluster_ports = cluster_ports;
        self.perimeter_config.management_port = management_port;
        self.perimeter_config.bootstrap_peers = bootstrap_peers;
        self.last_firewall_nodes = None;
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

    /// Require successful kernel publication before acknowledging deployment.
    async fn publish_backend_ebpf(
        &mut self,
        id: &crate::onion::service_id::ServiceId,
    ) -> Result<(), BunError> {
        let services = self.service_map.clone();
        self.publish_backend_snapshot(id, &services).await
    }

    /// Journal attempted routing before acknowledging its kernel publication.
    async fn publish_backend_snapshot(
        &mut self,
        id: &crate::onion::service_id::ServiceId,
        services: &crate::onion::service_map::ServiceMap,
    ) -> Result<(), BunError> {
        self.persist_discovery_publication(id, services).await?;
        if self.consumer_controls_views() {
            return self.mark_consumer_view_stale();
        }
        self.publish_backend_kernel(id, services).await
    }

    /// Publish a validated candidate before exposing it to userspace readers.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    async fn publish_backend_kernel(
        &self,
        id: &crate::onion::service_id::ServiceId,
        services: &crate::onion::service_map::ServiceMap,
    ) -> Result<(), BunError> {
        let Some(handle) = self.onion_ebpf.as_ref() else {
            return Ok(());
        };
        let Some(entry) = services.resolve(id).cloned() else {
            return Ok(());
        };
        let bpf = crate::onion::ebpf::maps::BpfServiceMap::new();
        let mut ebpf = handle.lock().await;
        bpf.update_backends_bpf(&mut ebpf, entry.vip, entry.port, &entry)
            .map_err(|error| BunError::BackendPublication {
                service: id.clone(),
                reason: error.to_string(),
            })
    }

    #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
    async fn publish_backend_kernel(
        &self,
        _id: &crate::onion::service_id::ServiceId,
        _services: &crate::onion::service_map::ServiceMap,
    ) -> Result<(), BunError> {
        Ok(())
    }

    /// Withdraw a service's backend and destination grants before releasing
    /// its allocated VIP. A failed removal retains the original service entry.
    /// A no-op without the eBPF data path loaded.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    async fn withdraw_service_ebpf(
        &self,
        id: &crate::onion::service_id::ServiceId,
    ) -> Result<(), BunError> {
        // Read the VIP + port straight from the live entry: the VIP is
        // whatever the map allocated (which may have probed off the natural
        // hash on a collision), so we must not re-derive it here.
        let Some(entry) = self.service_map.resolve(id) else {
            return Ok(());
        };
        self.withdraw_discovery_entry(entry).await
    }

    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    async fn withdraw_discovery_entry(
        &self,
        entry: &crate::onion::types::ServiceEntry,
    ) -> Result<(), BunError> {
        let Some(handle) = self.onion_ebpf.as_ref() else {
            return Ok(());
        };
        let id = crate::onion::service_id::ServiceId::new(&entry.namespace, &entry.app_name);
        let (vip, port, destination) = (entry.vip, entry.port, entry.app_id);
        let bpf = crate::onion::ebpf::maps::BpfServiceMap::new();
        let mut ebpf = handle.lock().await;
        bpf.remove_backends_bpf(&mut ebpf, vip, port)
            .map_err(|error| BunError::BackendRetirement {
                service: id.clone(),
                reason: error.to_string(),
            })?;
        crate::sesame::firewall::delete_destination_firewall_state(&mut ebpf.bpf, destination)
            .map_err(|error| BunError::DestinationRetirement {
                service: id.clone(),
                reason: error.to_string(),
            })
    }

    #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
    async fn withdraw_service_ebpf(
        &self,
        _id: &crate::onion::service_id::ServiceId,
    ) -> Result<(), BunError> {
        Ok(())
    }

    #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
    async fn withdraw_discovery_entry(
        &self,
        _entry: &crate::onion::types::ServiceEntry,
    ) -> Result<(), BunError> {
        Ok(())
    }

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
        if self.egress_store_uncertain {
            return;
        }
        let Some(handle) = self.onion_ebpf.clone() else {
            return;
        };

        // cgroup id(s) per (namespace, app), from currently-running instances.
        // Keying by the namespace-qualified identity — not the bare app name —
        // is what stops same-named apps in different namespaces from sharing a
        // firewall rule or a namespace mapping (H9). Collect the pairs first so
        // the `list_instances` borrow is released before the async workload-identity lookups.
        let pairs: Vec<((String, String), InstanceId, bool)> = self
            .supervisor
            .list_instances()
            .into_iter()
            .map(|i| {
                (
                    (i.namespace.clone(), i.app_name.clone()),
                    i.id.clone(),
                    i.is_being_created(),
                )
            })
            .collect();
        let mut cgroup_ids: std::collections::HashMap<(String, String), Vec<u64>> =
            std::collections::HashMap::new();
        for (key, id, being_created) in pairs {
            if let Some(owner) = self.egress_bindings.get(&id)
                && owner.phase == PolicyPhase::Owned
                && owner.source_namespace.is_some()
            {
                cgroup_ids.entry(key).or_default().push(owner.cgroup_id);
                continue;
            }
            // No cgroup exists yet, and asking the runtime would hold the
            // agent loop until the instance's image pull finishes (Z6.7).
            if being_created {
                continue;
            }
            match self.supervisor.grill().workload_cgroup(&id).await {
                Ok(Some(cgroup)) => cgroup_ids.entry(key).or_default().push(cgroup),
                Ok(None) => {}
                Err(error) => {
                    // Unavailable source evidence cannot authorise erasing
                    // previously installed namespace/firewall bindings.
                    eprintln!("sesame: source identity for {id} is unavailable: {error}");
                    return;
                }
            }
        }

        let services: Vec<crate::onion::types::ServiceEntry> = self
            .merged_service_map()
            .resolve_all()
            .into_iter()
            .cloned()
            .collect();
        let ns_entries = crate::sesame::firewall::resolve_cgroup_namespace_entries(&cgroup_ids);
        let fw_entries = crate::sesame::firewall::rules_to_bpf_entries(
            &crate::sesame::firewall::resolve_firewall_rules(&services, &cgroup_ids),
        );

        let mut ebpf = handle.lock().await;
        if let Err(error) = crate::sesame::firewall::reconcile_firewall_maps(
            &mut ebpf.bpf,
            &ns_entries,
            &fw_entries,
            &mut self.cgroup_ns_bpf_keys,
            &mut self.firewall_bpf_keys,
        ) {
            eprintln!("sesame: firewall reconciliation failed: {error}");
        }
    }

    #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
    async fn sync_firewall_ebpf(&mut self) {}

    /// Enable on-disk instance records under `dir` ({data_dir}/instances).
    /// Call before deploying anything; also enables `adopt_recorded_instances`.
    pub fn set_records_dir(&mut self, dir: PathBuf) {
        self.records_dir = Some(dir);
    }

    /// Override how long an ordinary stop waits after SIGTERM before it
    /// escalates to SIGKILL. Production keeps `STOP_GRACE_SECS`; tests
    /// whose runtime ignores SIGTERM on purpose use a short grace instead
    /// of waiting the full ten seconds.
    pub fn set_stop_grace(&mut self, grace: std::time::Duration) {
        self.stop_grace = grace;
    }

    /// Override how long node shutdown waits after SIGTERM before SIGKILL.
    /// Production keeps `SHUTDOWN_GRACE_SECS`, as with `set_stop_grace`.
    pub fn set_shutdown_grace(&mut self, grace: std::time::Duration) {
        self.shutdown_grace = grace;
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
            let id = InstanceId(item.full_id.clone());
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

    /// Keep job evidence durable before runtime mutation or retry admission.
    async fn commit_jobs(
        &mut self,
        next: BTreeMap<String, super::jobs::RecordedJob>,
    ) -> Result<(), BunError> {
        if self.job_store_uncertain {
            return Err(BunError::JobState(
                "a previous write is uncertain; restart Bun to reload it".into(),
            ));
        }
        if next == self.recorded_jobs {
            return Ok(());
        }
        if let Some(directory) = self.records_dir.clone() {
            self.job_store_uncertain = true;
            for (id, job) in &next {
                self.recorded_jobs
                    .entry(id.clone())
                    .or_insert_with(|| job.clone());
            }
            let records = next.clone();
            tokio::task::spawn_blocking(move || super::jobs::persist(&directory, records))
                .await
                .map_err(|error| BunError::JobState(error.to_string()))?
                .map_err(|error| BunError::JobState(error.to_string()))?;
        }
        self.recorded_jobs = next;
        self.job_store_uncertain = false;
        Ok(())
    }

    async fn record_observed_job_exit(
        &mut self,
        id: &InstanceId,
        phase: super::jobs::JobPhase,
    ) -> Result<(), BunError> {
        let mut next = self.recorded_jobs.clone();
        let job = next
            .get_mut(&id.0)
            .ok_or_else(|| BunError::JobState(format!("missing attempt for {id}")))?;
        job.phase = phase;
        // A short process can exit before a PID adoption record is available.
        // Persist its positive exit observation with the outcome, rather than
        // asking a replacement ProcessGrill to signal an unadoptable handle.
        // OCI runtimes retain named container resources after process exit.
        if job.runtime == crate::grill::records::RuntimeKind::Process {
            job.runtime_absent = true;
        }
        self.commit_jobs(next).await
    }

    async fn record_job_runtime_absent(&mut self, id: &InstanceId) -> Result<(), BunError> {
        let mut jobs = self.recorded_jobs.clone();
        if let Some(job) = jobs.get_mut(&id.0) {
            job.runtime_absent = true;
        }
        self.commit_jobs(jobs).await
    }

    /// A new run may replace terminal evidence only after old runtime cleanup.
    async fn prepare_job_run(
        &mut self,
        name: &str,
        namespace: &str,
        spec: &JobSpec,
        rerun_unknown: bool,
    ) -> Result<Vec<InstanceId>, BunError> {
        use super::jobs::{JobPhase, RecordedJob};
        let id = crate::grill::InstanceIdentity::new(namespace, name, 0).instance_id();
        let refuse = |reason: &str| BunError::JobState(format!("{namespace}/{name}: {reason}"));
        if self.job_store_uncertain {
            return Err(refuse("checkpoint is uncertain; restart Bun"));
        }
        if let Some(instance) = self.supervisor.get_instance(&id) {
            if !instance.is_job || instance.app_name != name || instance.namespace != namespace {
                return Err(refuse("instance id belongs to another workload"));
            }
            if !rerun_unknown
                && !matches!(
                    instance.state,
                    ContainerState::Stopped | ContainerState::Failed
                )
            {
                return Err(refuse(
                    "previous job still owns its runtime; stop it before applying again",
                ));
            }
        }
        let previous = self.recorded_jobs.get(&id.0).cloned();
        let next_cron_occurrence = self
            .scheduled_jobs
            .contains_key(&(name.into(), namespace.into()))
            && previous.as_ref().is_some_and(|job| job.runtime_absent);
        if previous.as_ref().is_some_and(|job| {
            matches!(
                job.phase,
                JobPhase::Unknown | JobPhase::Preparing | JobPhase::Launching
            )
        }) && !rerun_unknown
            && !next_cron_occurrence
        {
            return Err(refuse(
                "previous outcome is unknown; use apply --rerun-jobs for an explicit rerun",
            ));
        }
        let generation = match &previous {
            Some(job) => job
                .generation
                .checked_add(1)
                .ok_or_else(|| refuse("job generation exhausted"))?,
            None => 1,
        };
        if let Some(job) = &previous {
            if !job.runtime_absent {
                self.kill_and_wait_for_exit(&id).await?;
            }
            self.record_job_runtime_absent(&id).await?;
            self.retire_instance_artifacts(&id).await?;
            self.supervisor.retire_instance(&id).await;
        }
        let ids = self
            .supervisor
            .deploy_job(name, namespace, spec, Instant::now())
            .await?;
        let mut next = self.recorded_jobs.clone();
        next.insert(
            id.0.clone(),
            RecordedJob {
                name: name.into(),
                namespace: namespace.into(),
                spec: spec.clone(),
                runtime: self.supervisor.grill().runtime_kind(),
                generation,
                restart_count: 0,
                phase: JobPhase::Preparing,
                runtime_absent: false,
            },
        );
        self.commit_jobs(next).await?;
        Ok(ids)
    }

    /// Retrying spends the budget before create/start, after retiring the old record.
    async fn claim_job_retry(&mut self, id: &InstanceId) -> Result<(), BunError> {
        use super::jobs::{JobPhase, MAX_RETRIES};
        let count = self
            .supervisor
            .get_instance(id)
            .ok_or_else(|| BunError::InstanceNotFound {
                instance_id: id.clone(),
            })?
            .restart_count;
        let mut next = self.recorded_jobs.clone();
        let job = next
            .get_mut(&id.0)
            .ok_or_else(|| BunError::JobState(format!("missing attempt for {id}")))?;
        if count > MAX_RETRIES
            || count < job.restart_count
            || matches!(
                job.phase,
                JobPhase::Unknown
                    | JobPhase::Stopping
                    | JobPhase::Stopped
                    | JobPhase::Exited { code: 0 }
            )
        {
            return Err(BunError::JobState(format!(
                "automatic retry refused for {id}"
            )));
        }
        job.restart_count = count;
        job.phase = JobPhase::Preparing;
        job.runtime_absent = false;
        self.commit_jobs(next).await
    }

    /// Commit permission to execute only after the runtime has prepared this attempt.
    async fn transition_deploy_state(
        &mut self,
        id: &InstanceId,
        to: ContainerState,
    ) -> Result<(), BunError> {
        let instance =
            self.supervisor
                .get_instance(id)
                .ok_or_else(|| BunError::InstanceNotFound {
                    instance_id: id.clone(),
                })?;
        let state = instance.state.transition_to(to)?;
        if instance.is_job && to == ContainerState::Starting {
            if self.job_store_uncertain {
                return Err(BunError::JobState(
                    "checkpoint is uncertain; restart Bun".into(),
                ));
            }
            let mut jobs = self.recorded_jobs.clone();
            let job = jobs
                .get_mut(&id.0)
                .ok_or_else(|| BunError::JobState(format!("missing attempt for {id}")))?;
            if job.phase != super::jobs::JobPhase::Preparing || job.runtime_absent {
                return Err(BunError::JobState(format!(
                    "job {id} has no prepared attempt"
                )));
            }
            job.phase = super::jobs::JobPhase::Launching;
            self.commit_jobs(jobs).await?;
        }
        let instance =
            self.supervisor
                .get_instance_mut(id)
                .ok_or_else(|| BunError::InstanceNotFound {
                    instance_id: id.clone(),
                })?;
        instance.state = state;
        Ok(())
    }

    fn job_state_label(&self, instance: &WorkloadInstance) -> String {
        if (instance.is_job && self.job_store_uncertain)
            || self
                .recorded_jobs
                .get(&instance.id.0)
                .is_some_and(|job| job.phase == super::jobs::JobPhase::Unknown)
        {
            "unknown".into()
        } else {
            instance.state.to_string()
        }
    }

    /// Write (or refresh) the instance record used for adoption after a bun
    /// restart or self-upgrade exec. Application acknowledgement requires this
    /// metadata; short jobs recover through their separate attempt record.
    async fn persist_instance_record(&self, instance_id: &InstanceId) -> Result<(), BunError> {
        let Some(dir) = self.records_dir.clone() else {
            return Ok(());
        };
        let fail = |reason: &str| {
            BunError::AdoptionState(format!("cannot record {instance_id}: {reason}"))
        };
        let instance = self
            .supervisor
            .get_instance(instance_id)
            .ok_or_else(|| fail("instance is missing"))?;
        let runtime = self.supervisor.grill().runtime_kind();
        // Apple workloads live in VMs. Record the launcher for provenance;
        // Apple adoption checks the named container, never this host PID.
        let pid = if runtime == crate::grill::records::RuntimeKind::Apple {
            Some(std::process::id())
        } else {
            self.supervisor.grill().pid(instance_id).await
        };
        let Some(pid) = pid else {
            return if instance.is_job {
                Ok(())
            } else {
                Err(fail("runtime process identity is unavailable"))
            };
        };
        let Some(pid_started_at) = crate::grill::records::process_start_time(pid) else {
            return if instance.is_job {
                Ok(())
            } else {
                Err(fail("runtime process identity could not be observed"))
            };
        };
        let oci_spec = instance
            .oci_spec
            .clone()
            .ok_or_else(|| fail("runtime specification is missing"))?;

        let replica_index = crate::grill::InstanceIdentity::parse(&instance_id.0)
            .map(|ident| ident.ordinal)
            .unwrap_or(0);
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
        tokio::task::spawn_blocking(move || crate::grill::records::write_record(&dir, &record))
            .await
            .map_err(|error| fail(&error.to_string()))?
            .map_err(|error| fail(&error.to_string()))
    }

    /// Persist launch evidence while the replacement is still owned by its
    /// rolling worker, before health wait or traffic publication.
    async fn persist_rolling_instance(&self, instance: &RollingInstance) -> Result<(), BunError> {
        let Some(directory) = self.records_dir.clone() else {
            return Ok(());
        };
        let fail = |reason: String| BunError::DeployFailed {
            app_name: instance.app_name.clone(),
            reason,
        };
        let grill = self.supervisor.grill();
        let runtime = grill.runtime_kind();
        let pid = if runtime == crate::grill::records::RuntimeKind::Apple {
            Some(std::process::id())
        } else {
            grill.pid(&instance.instance_id).await
        }
        .ok_or_else(|| {
            fail(
                "runtime did not expose a process identity for durable replacement adoption".into(),
            )
        })?;
        let pid_started_at = crate::grill::records::process_start_time(pid).ok_or_else(|| {
            fail("replacement process exited before its identity could be recorded".into())
        })?;
        let identity = crate::grill::InstanceIdentity::parse(&instance.instance_id.0)
            .ok_or_else(|| fail("replacement has an invalid instance identity".into()))?;
        let record = crate::grill::records::InstanceRecord {
            schema: 2,
            instance_id: instance.instance_id.0.clone(),
            namespace: instance.namespace.clone(),
            app_name: instance.app_name.clone(),
            replica_index: identity.ordinal,
            is_job: false,
            image: instance.spec.image.clone().unwrap_or_default(),
            runtime,
            pid,
            pid_started_at,
            runc_container_id: matches!(runtime, crate::grill::records::RuntimeKind::Runc)
                .then(|| instance.instance_id.0.clone()),
            log_stem: grill.log_stem(&instance.instance_id).await,
            host_port: instance.host_port,
            app_spec: Some(instance.spec.clone()),
            oci_spec: instance.oci_spec.clone(),
            rootless_network: grill.rootless_network_record(&instance.instance_id).await,
        };
        tokio::task::spawn_blocking(move || {
            crate::grill::records::write_record(&directory, &record)
        })
        .await
        .map_err(|error| fail(format!("persist replacement record: {error}")))?
        .map_err(|error| fail(format!("persist replacement record: {error}")))
    }

    /// Reconcile launches that reached the runtime before agent adoption was durable.
    async fn reconcile_runtime_launches(
        &mut self,
        records: &[crate::grill::records::InstanceRecord],
        jobs: &mut std::collections::BTreeMap<String, super::jobs::RecordedJob>,
        launches: &[crate::grill::RuntimeLaunch],
    ) -> Result<(), BunError> {
        use super::jobs::JobPhase;
        let inventory: std::collections::HashMap<_, _> = launches
            .iter()
            .map(|launch| (launch.instance_id.0.as_str(), launch))
            .collect();
        if inventory.len() != launches.len() {
            return Err(BunError::AdoptionState(
                "duplicate runtime launch identity".into(),
            ));
        }
        let recorded: std::collections::HashSet<_> = records
            .iter()
            .map(|record| record.instance_id.as_str())
            .collect();
        // Validate all cross-record relationships before retiring any owner.
        for record in records {
            let launch = inventory.get(record.instance_id.as_str()).ok_or_else(|| {
                BunError::AdoptionState(format!(
                    "instance {} has no runtime launch intent",
                    record.instance_id
                ))
            })?;
            if launch.spec != record.oci_spec
                || jobs
                    .get(&record.instance_id)
                    .is_some_and(|job| job.phase == JobPhase::Preparing)
            {
                return Err(BunError::AdoptionState(format!(
                    "instance {} conflicts with runtime preparation",
                    record.instance_id
                )));
            }
        }
        for (id, job) in jobs.iter() {
            if !inventory.contains_key(id.as_str())
                && !job.runtime_absent
                && job.phase != JobPhase::Preparing
            {
                return Err(BunError::AdoptionState(format!(
                    "job {id} has no runtime launch intent"
                )));
            }
        }
        let mut retired = Vec::new();
        for launch in launches {
            let id = &launch.instance_id;
            if recorded.contains(id.0.as_str()) {
                continue;
            }
            let state = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                self.supervisor.grill().state(id),
            )
            .await
            .map_err(|_| {
                BunError::AdoptionState(format!("runtime inspection timed out for {id}"))
            })??;
            if state != ContainerState::Stopped
                && jobs.get(&id.0).is_some_and(|job| job.runtime_absent)
            {
                return Err(BunError::AdoptionState(format!(
                    "job {id} has conflicting live and terminal evidence"
                )));
            }
            // Without the agent's acknowledgement record an active launch has
            // an uncertain outcome. Fence it before ordinary desired-state
            // reconciliation can authorise any replacement.
            if state != ContainerState::Stopped {
                kill_runtime_instance(self.supervisor.grill(), id, self.stop_confirmation_timeout)
                    .await?;
            }
            if let Some(job) = jobs.get_mut(&id.0) {
                job.runtime_absent = true;
                job.phase = match job.phase {
                    JobPhase::Launching if state == ContainerState::Stopped => {
                        match self.supervisor.grill().exit_code(id).await {
                            Some(code) => JobPhase::Exited { code },
                            None => JobPhase::Unknown,
                        }
                    }
                    JobPhase::Preparing | JobPhase::Launching => JobPhase::Unknown,
                    JobPhase::Stopping => JobPhase::Stopped,
                    ref phase => phase.clone(),
                };
                self.commit_jobs(jobs.clone()).await?;
            }
            retired.push(id.clone());
        }
        // An unacknowledged init can share its parent's cgroup. Retiring
        // parent artifacts first would lift policy while that init still runs.
        for id in retired {
            if !self.defer_startup_retirement(&id).await? {
                self.retire_instance_artifacts(&id).await?;
            }
        }
        for (id, job) in jobs.iter_mut() {
            if !inventory.contains_key(id.as_str()) && job.phase == JobPhase::Preparing {
                // A complete mandatory intent inventory plus the pre-execution
                // phase proves no runtime was activated for this preparation.
                job.phase = JobPhase::Unknown;
                job.runtime_absent = true;
            }
        }
        self.commit_jobs(jobs.clone()).await
    }

    /// Adopt still-running workloads recorded by a previous bun process.
    ///
    /// Called once at startup, BEFORE any reconciliation: adopted instances
    /// are seeded into the supervisor as Running so they don't get
    /// double-started. Records whose process is gone are deleted (the
    /// instance reschedules through the normal path). Returns the number
    /// of instances adopted. Any uncertain observation refuses startup and
    /// preserves durable records and identity material for recovery.
    ///
    /// Jobs restore durable retry budgets and retain unknown outcomes. App
    /// backoff starts fresh; normal reconciliation rebuilds cluster routing.
    pub async fn adopt_recorded_instances(&mut self) -> Result<usize, BunError> {
        let Some(dir) = self.records_dir.clone() else {
            return Ok(0);
        };
        let now = Instant::now();
        let mut adopted_count = 0;

        let records_dir = dir.clone();
        let (records, schedules, jobs) = tokio::task::spawn_blocking(move || {
            let records = crate::grill::records::load_records(&records_dir)?;
            let schedules = super::schedules::load(&records_dir)?;
            let jobs = super::jobs::load(&records_dir)?;
            Ok::<_, std::io::Error>((records, schedules, jobs))
        })
        .await
        .map_err(|error| BunError::AdoptionState(error.to_string()))?
        .map_err(|error| BunError::AdoptionState(error.to_string()))?;
        self.require_discovery_recovery(!records.is_empty(), false)?;
        for job in jobs.values() {
            if job.runtime != self.supervisor.grill().runtime_kind() {
                return Err(BunError::AdoptionState(
                    "job attempt belongs to another runtime".into(),
                ));
            }
        }
        // Validate the entire inventory before adopting or deleting any owner.
        for record in &records {
            if record.is_job && !jobs.contains_key(&record.instance_id) {
                return Err(BunError::AdoptionState(format!(
                    "job {} has no durable attempt",
                    record.instance_id
                )));
            }
            if let Some(job) = jobs.get(&record.instance_id)
                && (!record.is_job
                    || record.namespace != job.namespace
                    || record.app_name != job.name
                    || record.image != job.spec.image.clone().unwrap_or_default())
            {
                return Err(BunError::AdoptionState(
                    "job record conflicts with attempt ownership".into(),
                ));
            }
            let base = crate::grill::InstanceIdentity::new(
                &record.namespace,
                &record.app_name,
                record.replica_index,
            );
            let generation = record
                .instance_id
                .strip_prefix(&format!("{}__{}-g", record.namespace, record.app_name))
                .and_then(|suffix| suffix.strip_suffix(&format!("-{}", record.replica_index)))
                .and_then(|value| value.parse::<u64>().ok());
            let matches_generation = generation.is_some_and(|generation| {
                crate::grill::InstanceIdentity::canary(
                    &record.namespace,
                    &record.app_name,
                    generation,
                    record.replica_index,
                )
                .instance_id()
                .0 == record.instance_id
            });
            if !crate::config::valid_workload_label(&record.namespace)
                || !crate::config::valid_workload_label(&record.app_name)
                || (base.instance_id().0 != record.instance_id && !matches_generation)
                || record
                    .app_spec
                    .as_ref()
                    .and_then(|spec| spec.namespace.as_ref())
                    .is_some_and(|namespace| namespace != &record.namespace)
            {
                return Err(BunError::AdoptionState(format!(
                    "unsupported or inconsistent workload identity in record {:?}; the record and runtime are preserved",
                    record.instance_id,
                )));
            }
            if record.runtime != self.supervisor.grill().runtime_kind() {
                return Err(BunError::AdoptionState(format!(
                    "instance {} belongs to {:?}, but the selected runtime is {:?}",
                    record.instance_id,
                    record.runtime,
                    self.supervisor.grill().runtime_kind(),
                )));
            }
        }
        let mut restored = std::collections::HashMap::new();
        for stored in schedules {
            let namespace = stored.spec.namespace.as_deref().unwrap_or("default");
            if namespace != stored.namespace
                || stored.name.is_empty()
                || stored.last_fired_minute.is_some_and(|minute| minute < 0)
            {
                return Err(BunError::AdoptionState(
                    "invalid scheduled-job identity or firing stamp".into(),
                ));
            }
            let mut config = Config::default();
            config.job.insert(stored.name.clone(), stored.spec.clone());
            config
                .validate()
                .map_err(|error| BunError::AdoptionState(error.to_string()))?;
            let expression = stored.spec.schedule.as_deref().ok_or_else(|| {
                BunError::AdoptionState("recorded cron job has no schedule".into())
            })?;
            let schedule = crate::meat::cron::CronSchedule::parse(expression)
                .map_err(|error| BunError::AdoptionState(error.to_string()))?;
            let key = (stored.name.clone(), stored.namespace.clone());
            let job = ScheduledJob {
                name: stored.name,
                namespace: stored.namespace,
                spec: stored.spec,
                schedule,
                last_fired_minute: stored.last_fired_minute,
            };
            if restored.insert(key, job).is_some() {
                return Err(BunError::AdoptionState(
                    "duplicate scheduled-job identity".into(),
                ));
            }
        }
        self.scheduled_jobs = restored;
        self.scheduled_jobs_store_uncertain = false;
        self.recorded_jobs = jobs.clone();
        self.job_store_uncertain = false;
        let mut recovered_jobs = jobs;
        let launch_inventory = self
            .runtime_inventory(RUNTIME_INVENTORY_TIMEOUT, |reason| {
                BunError::AdoptionState(format!("startup adoption {reason}"))
            })
            .await?;
        self.require_discovery_recovery(
            !records.is_empty(),
            launch_inventory
                .as_ref()
                .is_some_and(|launches| !launches.is_empty()),
        )?;
        self.validate_recovered_discovery(&records, launch_inventory.as_deref())?;
        self.restore_egress_owners(&records, launch_inventory.as_deref())
            .await?;
        self.replay_discovery_releases().await?;
        if let Some(launches) = &launch_inventory {
            self.reconcile_runtime_launches(&records, &mut recovered_jobs, launches)
                .await?;
        }
        let mut adopted_jobs = std::collections::HashSet::new();
        for record in records {
            // Startup preflight proved that runtime, record and supervisor
            // share the same identity. Never invent an alias for an old owner.
            let runtime_id = InstanceId(record.instance_id.clone());
            let instance_id = runtime_id.clone();
            // Never clobber an instance the current process already tracks.
            if self.supervisor.get_instance(&instance_id).is_some() {
                if record.is_job {
                    adopted_jobs.insert(instance_id.0.clone());
                }
                continue;
            }

            let adopted = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                self.supervisor.grill().adopt(&runtime_id, &record),
            )
            .await
            .map_err(|_| {
                BunError::AdoptionState(format!("runtime adoption timed out for {runtime_id}"))
            })??;
            if !adopted {
                if let Some(job) = recovered_jobs.get_mut(&runtime_id.0) {
                    job.runtime_absent = true;
                    if matches!(
                        job.phase,
                        super::jobs::JobPhase::Preparing | super::jobs::JobPhase::Launching
                    ) {
                        job.phase = if launch_inventory.is_some()
                            && job.phase == super::jobs::JobPhase::Launching
                        {
                            match self.supervisor.grill().exit_code(&runtime_id).await {
                                Some(code) => super::jobs::JobPhase::Exited { code },
                                None => super::jobs::JobPhase::Unknown,
                            }
                        } else {
                            super::jobs::JobPhase::Unknown
                        };
                    }
                    // Preserve the positive observation before deleting the
                    // only record that let this runtime prove absence.
                    self.commit_jobs(recovered_jobs.clone()).await?;
                }
                if !self.defer_startup_retirement(&runtime_id).await? {
                    self.retire_instance_artifacts(&runtime_id).await?;
                }
                continue;
            }

            if let Err(error) = self.restore_live_egress(&runtime_id, &record).await {
                // Adoption has proved this is our surviving runtime. Do not
                // publish it as Running without confirmed policy ownership.
                kill_runtime_instance(
                    self.supervisor.grill(),
                    &runtime_id,
                    self.stop_confirmation_timeout,
                )
                .await?;
                return Err(error);
            }

            let recorded_job = recovered_jobs.get(&runtime_id.0);
            if let Some(job) = recorded_job {
                if matches!(job.phase, super::jobs::JobPhase::Exited { .. }) || job.runtime_absent {
                    return Err(BunError::AdoptionState(format!(
                        "job {runtime_id} has conflicting live and terminal evidence"
                    )));
                }
                adopted_jobs.insert(runtime_id.0.clone());
            }
            // The surviving instance still holds its port.
            if let Some(port) = record.host_port {
                self.supervisor.port_allocator.reserve(port).await?;
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
            // runtime id, which is also the supervisor key. An
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
                state: if recorded_job.is_some_and(|job| {
                    matches!(
                        job.phase,
                        super::jobs::JobPhase::Stopping | super::jobs::JobPhase::Stopped
                    )
                }) {
                    ContainerState::Stopping
                } else {
                    ContainerState::Running
                },
                health_counters: super::health::HealthCounters::new(),
                restart_count: recorded_job.map_or(0, |job| job.restart_count),
                last_restart: None,
                host_port: record.host_port,
                container_ip: None,
                created_at: now,
                restart_policy: if record.is_job {
                    super::restart::RestartPolicy::for_job(super::jobs::MAX_RETRIES)
                } else {
                    super::restart::RestartPolicy::default()
                },
                health_config,
                is_job: record.is_job,
                retry_pending: false,
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

        for (id, job) in &mut recovered_jobs {
            if !adopted_jobs.contains(id)
                && matches!(
                    job.phase,
                    super::jobs::JobPhase::Preparing | super::jobs::JobPhase::Launching
                )
            {
                job.phase = super::jobs::JobPhase::Unknown;
            }
        }
        self.commit_jobs(recovered_jobs).await?;
        // Keep terminal/unknown evidence visible even after its runtime is gone.
        for (id, job) in self.recorded_jobs.clone() {
            let instance_id = InstanceId(id);
            if self.supervisor.get_instance(&instance_id).is_some() {
                continue;
            }
            self.supervisor
                .deploy_job(&job.name, &job.namespace, &job.spec, now)
                .await?;
            let cgroup = crate::grill::cgroup::instance_cgroup_path(
                &job.namespace,
                &job.name,
                &instance_id,
            )?;
            let spec = generate_job_oci_spec(
                &job.name,
                &job.namespace,
                &job.spec,
                &cgroup.to_string_lossy(),
                None,
            );
            if let Some(instance) = self.supervisor.get_instance_mut(&instance_id) {
                instance.restart_count = job.restart_count;
                instance.restart_policy =
                    super::restart::RestartPolicy::for_job(super::jobs::MAX_RETRIES);
                instance.state = match job.phase {
                    super::jobs::JobPhase::Unknown => ContainerState::Failed,
                    super::jobs::JobPhase::Exited { code }
                        if code != 0 && job.restart_count >= super::jobs::MAX_RETRIES =>
                    {
                        ContainerState::Failed
                    }
                    super::jobs::JobPhase::Stopping => ContainerState::Stopping,
                    _ => ContainerState::Stopped,
                };
                instance.retry_pending = matches!(job.phase, super::jobs::JobPhase::Exited { code } if code != 0)
                    && job.restart_count < super::jobs::MAX_RETRIES;
                instance.oci_spec = Some(spec);
                instance.last_restart = Some(now);
            }
        }

        if adopted_count > 0 {
            println!("bun: adopted {adopted_count} running instance(s) from a previous process");
        }

        // Identity dirs of instances that died while bun was down have no
        // live owner, so they are stale key material.
        self.finish_discovery_recovery().await?;
        self.sweep_orphaned_identity_dirs().await;

        Ok(adopted_count)
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

        let (line_tx, mut line_rx) = mpsc::channel::<crate::ketchup::types::CapturedLine>(256);
        // Producer: the runtime streams complete lines, from the start of the
        // instance's output, into line_tx. The log store drops the ones it
        // already holds, so an adopted instance isn't ingested twice.
        let follow_grill = grill;
        let follow_id = id.clone();
        tokio::spawn(async move {
            follow_grill.follow_logs(&follow_id, line_tx).await;
        });
        // Consumer: tag each line and forward it to the log sink.
        tokio::spawn(async move {
            while let Some(captured) = line_rx.recv().await {
                let record = crate::ketchup::types::LogRecord {
                    app: app.clone(),
                    namespace: namespace.clone(),
                    instance: id.0.clone(),
                    stream: captured.stream,
                    line: captured.line,
                    position: captured.position,
                };
                if log_tx.send(record).await.is_err() {
                    break;
                }
            }
        });
    }

    /// Run the agent event loop until shutdown is requested.
    pub async fn run(&mut self) {
        self.run_loop(None).await;
    }

    /// Run the agent, acknowledging readiness after initial capability collection.
    pub async fn run_with_readiness(&mut self, ready: super::readiness::ReadySignal) {
        self.run_loop(Some(ready)).await;
    }

    async fn run_loop(&mut self, ready: Option<super::readiness::ReadySignal>) {
        let mut health_interval = tokio::time::interval(std::time::Duration::from_secs(1));

        if let Some(readiness) = self.readiness.clone() {
            let (capabilities, _) = self.live_egress_report_state().await;
            readiness.set_capabilities(capabilities).await;
        }

        if let Some(ready) = ready {
            ready.ready();
        }

        loop {
            tokio::select! {
                _ = self.shutdown.cancelled() => {
                    self.abandon_pending_stops();
                    self.shutdown_all().await;
                    break;
                }
                Some(cmd) = self.command_rx.recv() => {
                    self.handle_command(cmd).await;
                }
                Some(outcome) = self.stop_waits.join_next_with_id(),
                    if !self.stop_waits.is_empty() => {
                    self.complete_app_stop(outcome).await;
                }
                Some(op) = self.deploy_ops_rx.recv() => {
                    self.handle_deploy_op(op).await;
                }
                Some(req) = Self::recv_snapshot(&mut self.cluster) => {
                    self.handle_snapshot_request(req).await;
                }
                _ = health_interval.tick() => {
                    self.reopen_uncertain_discovery().await;
                    if let Err(error) = self.fence_lapsed_view().await {
                        eprintln!("bun: withdrawing the lapsed cluster view awaits retry: {error}");
                    }
                    self.drive_startup_retirements().await;
                    self.drive_deferred_retirements().await;
                    self.refresh_egress_readiness().await;
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
            // Local changes only mark the consumer view stale, so a burst of
            // them costs one republication, and the old view serves meanwhile.
            if let Err(error) = self.refresh_consumer_view().await {
                eprintln!("bun: consumer view refresh awaits retry: {error}");
            }
        }
    }

    /// Enforce the current kernel boundary and publish this tick's capabilities.
    async fn refresh_egress_readiness(&mut self) {
        let egress = self.enforce_live_egress_or_stop().await;
        if let Some(readiness) = self.readiness.clone() {
            readiness
                .set_capabilities(crate::meat::cluster_state::NodeCapabilities {
                    egress,
                    dns: self.supervisor.dns_capability(),
                })
                .await;
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

        // The report deadline is two seconds. Bound the evidence read without
        // hiding capacity when inventory is unavailable or internally ambiguous.
        let launches = match self
            .runtime_inventory(LOOP_RUNTIME_INVENTORY_TIMEOUT, BunError::AdoptionState)
            .await
        {
            Ok(Some(launches)) => {
                let count = launches.len();
                let by_instance: std::collections::HashMap<_, _> = launches
                    .into_iter()
                    .map(|launch| (launch.instance_id.clone(), launch))
                    .collect();
                (by_instance.len() == count).then_some(by_instance)
            }
            _ => None,
        };
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
                        execution: launches
                            .as_ref()
                            .and_then(|known| known.get(&inst.id))
                            .filter(|launch| inst.oci_spec.as_ref() == Some(&launch.spec))
                            .map(|launch| crate::grill::RuntimeExecution {
                                instance_id: launch.instance_id.clone(),
                                generation: launch.generation.clone(),
                            }),
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
        #[cfg(test)]
        self.egress_observation_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
            .filter(|(_, binding)| {
                binding.phase == PolicyPhase::Owned && enforced_cgroups.contains(&binding.cgroup_id)
            })
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
        #[cfg(test)]
        self.egress_observation_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
                    api_address: None,
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

    fn validate_deploy_names(&self, config: &Config) -> Result<(), String> {
        use crate::bun::deploy_operations::DeployTargetKind;
        config
            .validate_workload_names()
            .map_err(|error| error.to_string())?;
        for (name, spec) in &config.app {
            let namespace = spec.namespace.as_deref().unwrap_or("default");
            if self
                .scheduled_jobs
                .contains_key(&(name.clone(), namespace.to_string()))
            {
                return Err(format!(
                    "workload {namespace}/{name} belongs to a registered cron job; stop it before deploying an app with that name"
                ));
            }
            self.supervisor
                .admit_workload_kind(
                    name,
                    spec.namespace.as_deref().unwrap_or("default"),
                    DeployTargetKind::App,
                )
                .map_err(|error| error.to_string())?;
        }
        for (name, spec) in &config.job {
            self.supervisor
                .admit_workload_kind(
                    name,
                    spec.namespace.as_deref().unwrap_or("default"),
                    DeployTargetKind::Job,
                )
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    /// Admit and track either an operator apply or one cron firing through
    /// worker completion, including rollback and cancellation.
    async fn begin_deploy(
        &mut self,
        config: Config,
        events: mpsc::Sender<ApplyEvent>,
        register_schedule: bool,
        rerun_unknown_jobs: bool,
    ) {
        if self.startup_cleanup_pending {
            let _ = events
                .send(ApplyEvent::Error {
                    message: "startup cleanup still owns runtime allocations; retry after recovery"
                        .into(),
                })
                .await;
            return;
        }
        if rerun_unknown_jobs && let Err(message) = super::jobs::validate_rerun(&config) {
            let _ = events
                .send(ApplyEvent::Error {
                    message: message.into(),
                })
                .await;
            return;
        }
        if let Err(message) = self.validate_deploy_names(&config) {
            let _ = events.send(ApplyEvent::Error { message }).await;
            return;
        }
        // A stopping workload still owns its instances until their exit is
        // confirmed; a deploy must not replace them underneath the stop.
        if let Some(target) = self.stopping_target(&config) {
            let message = format!(
                "workload {}/{} is still stopping; retry once its exit is confirmed",
                target.namespace, target.name
            );
            let _ = events.send(ApplyEvent::Error { message }).await;
            return;
        }
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
            let message = "node is draining for a binary upgrade; retry shortly".to_string();
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
        if register_schedule && let Err(error) = self.register_scheduled_jobs(&config).await {
            let message = error.to_string();
            operation
                .finish(
                    crate::bun::deploy_operations::DeployOperationOutcome::Failed,
                    message.clone(),
                )
                .await;
            let _ = events.send(ApplyEvent::Error { message }).await;
            return;
        }

        // Forward deploy events to the caller, mirroring errors into the
        // event store. The deploy itself runs on its own task so a slow
        // pull or a rolling health wait can't wedge this loop
        // (DEP4/codex-M3); it drives its authoritative steps back
        // through `deploy_ops_tx`.
        let (forward_tx, mut forward_rx) = mpsc::channel(64);
        let event_store = self.events.clone();
        let observed_operation = operation.clone();
        let worker = DeployWorker {
            rerun_unknown_jobs,
            grill: self.supervisor.grill().clone(),
            ops: DeployOps {
                tx: self.deploy_ops_tx.clone(),
            },
            drains: self.drains.clone(),
            operation: Some(operation),
            stop_confirmation_timeout: self.stop_confirmation_timeout,
        };
        let worker_task = tokio::spawn(async move {
            worker.run_deploy(config, forward_tx).await;
        });
        tokio::spawn(async move {
            use crate::bun::deploy_operations::DeployOperationOutcome;
            let mut outcome = DeployOperationOutcome::Unknown;
            let mut message = "deploy worker ended without a terminal event".to_string();
            let mut completion = None;
            let mut events = Some(events);
            while let Some(event) = forward_rx.recv().await {
                match &event {
                    ApplyEvent::Complete { created, .. } => {
                        if outcome != DeployOperationOutcome::Failed {
                            outcome = DeployOperationOutcome::Completed;
                            message = format!("deploy completed ({created} instances)");
                            // Success becomes visible only after all trailing
                            // bookkeeping and the worker itself have finished.
                            completion = Some(event);
                        }
                        continue;
                    }
                    ApplyEvent::Error { message: error } => {
                        outcome = DeployOperationOutcome::Failed;
                        message = error.clone();
                        completion = None;
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
                                error.clone(),
                            );
                        }
                    }
                    _ => {}
                }
                // A stalled or disconnected observer cannot hold the
                // worker's outcome hostage. Close a full stream; its
                // client sees an incomplete stream and can query the ID.
                if let Some(sender) = &events
                    && sender.try_send(event).is_err()
                {
                    events = None;
                }
            }
            if let Err(error) = worker_task.await {
                outcome = DeployOperationOutcome::Unknown;
                message = format!("deploy worker ended unexpectedly: {error}");
                completion = None;
            } else if observed_operation.cancellation_observed() {
                outcome = DeployOperationOutcome::Cancelled;
                message = "deploy cancelled; in-flight work has finished".into();
                completion = None;
            }
            // Error events can precede rollback. Release target ownership
            // only after the worker has completed every mutation.
            observed_operation.finish(outcome, message.clone()).await;
            if let Some(sender) = events {
                if let Some(event) = completion {
                    let _ = sender.try_send(event);
                } else if outcome == DeployOperationOutcome::Unknown {
                    let _ = sender.try_send(ApplyEvent::Error { message });
                }
            }
        });
    }

    /// Handle a single command.
    async fn handle_command(&mut self, cmd: AgentCommand) {
        match cmd {
            AgentCommand::Deploy { config, events } => {
                self.begin_deploy(config, events, true, false).await;
            }
            AgentCommand::RerunJobs { config, events } => {
                self.begin_deploy(config, events, true, true).await;
            }
            AgentCommand::Stop {
                app_name,
                namespace,
                response,
            } => {
                self.request_app_stop(app_name, namespace, StopPurpose::Stop, response)
                    .await;
            }
            AgentCommand::Retire {
                app_name,
                namespace,
                response,
            } => {
                self.request_app_stop(app_name, namespace, StopPurpose::Retire, response)
                    .await;
            }
            AgentCommand::RetireTestResources {
                app_name,
                namespace,
                response,
            } => {
                if let Err(error) = Self::require_test_namespace(&app_name, &namespace) {
                    let _ = response.send(Err(error));
                } else {
                    self.request_app_stop(
                        app_name,
                        namespace,
                        StopPurpose::RetireTestResources,
                        response,
                    )
                    .await;
                }
            }
            AgentCommand::Status { response } => {
                let statuses = self.get_status().await;
                let _ = response.send(statuses);
            }
            AgentCommand::ScrapeTargets { response } => {
                let targets = self
                    .supervisor
                    .list_instances()
                    .iter()
                    .filter(|instance| {
                        !instance.is_job
                            && matches!(
                                instance.state,
                                ContainerState::HealthWait
                                    | ContainerState::Running
                                    | ContainerState::Unhealthy
                            )
                    })
                    .filter_map(|instance| {
                        let spec = self
                            .deployed_specs
                            .get(&(instance.app_name.clone(), instance.namespace.clone()))?;
                        crate::mayo::scrape::AppScrapeTarget::for_instance(
                            &instance.id.0,
                            &instance.app_name,
                            &instance.namespace,
                            instance.container_ip,
                            spec,
                        )
                    })
                    .collect();
                let _ = response.send(targets);
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
                            placements: Default::default(),
                            service_port: spec.port,
                        },
                    )
                    .collect::<Vec<_>>();
                apps.sort_by(|left, right| {
                    (&left.namespace, &left.app).cmp(&(&right.namespace, &right.app))
                });
                let _ = response.send(apps);
            }
            AgentCommand::CurrentResources { response } => {
                let mut resources: Vec<CurrentResourceStatus> = self
                    .deployed_specs
                    .iter()
                    .map(|((app, _namespace), spec)| CurrentResourceStatus {
                        resource: format!("app.{app}"),
                        image: spec.image.clone(),
                    })
                    .collect();
                for job in self.get_job_status() {
                    resources.push(CurrentResourceStatus {
                        resource: format!("job.{}", job.name),
                        image: Some(job.image),
                    });
                }
                resources.sort_by(|a, b| a.resource.cmp(&b.resource));
                resources.dedup_by(|a, b| a.resource == b.resource);
                let _ = response.send(resources);
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
            AgentCommand::CancelDeploy {
                operation_id,
                response,
            } => {
                let operation = self
                    .deploy_operations
                    .request_cancellation(&operation_id)
                    .await;
                let _ = response.send(operation);
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
                label,
                lines,
            } => {
                self.follow_app_logs(&app_name, &namespace, tail, label.as_deref(), lines)
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
            AgentCommand::SnapshotCreate {
                namespace,
                app_name,
                volume,
                name,
                response,
            } => {
                // btrfs subprocess + fs walks off the command loop (M7).
                let volumes_dir = self.volumes_dir.clone();
                tokio::task::spawn_blocking(move || {
                    let result =
                        Self::snapshot_create(&volumes_dir, &namespace, &app_name, volume, name);
                    let _ = response.send(result);
                });
            }
            AgentCommand::SnapshotList {
                namespace,
                app_name,
                response,
            } => {
                let volumes_dir = self.volumes_dir.clone();
                tokio::task::spawn_blocking(move || {
                    let manager = crate::grill::snapshot::SnapshotManager::new(&volumes_dir);
                    let _ =
                        response.send(manager.list(&namespace, &app_name).map_err(BunError::from));
                });
            }
            AgentCommand::SnapshotRestore {
                namespace,
                app_name,
                name,
                response,
            } => {
                // The running-instance check needs supervisor state, so it stays
                // on the loop; the btrfs restore itself runs off it (M7).
                let running = self.supervisor.list_instances().into_iter().any(|i| {
                    i.app_name == app_name
                        && i.namespace == namespace
                        && !matches!(i.state, ContainerState::Stopped | ContainerState::Failed)
                });
                if running {
                    let _ = response.send(Err(crate::grill::snapshot::SnapshotError::AppRunning {
                        namespace: namespace.clone(),
                        app: app_name.clone(),
                    }
                    .into()));
                } else {
                    let volumes_dir = self.volumes_dir.clone();
                    tokio::task::spawn_blocking(move || {
                        let result = crate::grill::snapshot::SnapshotManager::new(&volumes_dir)
                            .restore(&namespace, &app_name, &name)
                            .map_err(BunError::from);
                        let _ = response.send(result);
                    });
                }
            }
            AgentCommand::SnapshotDelete {
                namespace,
                app_name,
                name,
                response,
            } => {
                let volumes_dir = self.volumes_dir.clone();
                tokio::task::spawn_blocking(move || {
                    let manager = crate::grill::snapshot::SnapshotManager::new(&volumes_dir);
                    let _ = response.send(
                        manager
                            .delete(&namespace, &app_name, &name)
                            .map_err(BunError::from),
                    );
                });
            }
            AgentCommand::PrepareNodeFault {
                mut request,
                response,
            } => {
                let result = crate::smoker::config::effective_duration(
                    request.duration,
                    false,
                    &self.smoker_config,
                )
                .map(|duration| {
                    request.duration = duration;
                    (self.node_fault_fence.boot_id.clone(), request)
                })
                .map_err(|reason| BunError::FaultRejected { reason });
                let _ = response.send(result);
            }
            AgentCommand::FenceNodeFault {
                only_if_finished,
                reservation,
                response,
            } => {
                let result = self
                    .fence_node_fault(&reservation, only_if_finished)
                    .await
                    .map_err(|reason| BunError::FaultRejected { reason });
                let _ = response.send(result);
            }
            AgentCommand::InjectFault {
                reservation,
                mut request,
                replica_evidence,
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

                if !request.fault_type.is_node_targeted() && request.namespace.is_none() {
                    let _ = response.send(Err(BunError::FaultRejected {
                        reason: "workload faults require a namespace".into(),
                    }));
                    return;
                }

                if request.fault_type.is_node_targeted() {
                    let result = reservation
                        .as_deref()
                        .ok_or_else(|| {
                            "node faults require a committed cluster reservation".to_string()
                        })
                        .and_then(|grant| self.node_fault_fence.activate(grant, &request));
                    if let Err(reason) = result {
                        let _ = response.send(Err(BunError::FaultRejected { reason }));
                        return;
                    }
                }

                // Safety rails next (L14): reject faults that risk
                // quorum, kill a service's last replica, target the
                // leader, or exceed the node-percentage cap — unless
                // explicitly overridden. The context is built even with no
                // cluster handle so the replica-minimum rail still runs (M1).
                let context = self.build_safety_context(&request, replica_evidence).await;
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
                if let Some(grant) = &reservation {
                    self.node_fault_fence.active = Some((grant.sequence, rule.id));
                }
                match self.apply_fault(&rule).await {
                    Ok(()) => {
                        let summary = crate::smoker::types::FaultSummary::from(&rule);
                        let _ = response.send(Ok(summary));
                    }
                    Err(reason) => {
                        self.fault_registry.remove(rule.id);
                        // Take back anything a partial network install wrote.
                        self.reconcile_network_faults().await;
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
                // The fence keeps the grant after the effect is reversed, until
                // the leader fences it, so a retried clear still reports it.
                let reservation = self
                    .node_fault_fence
                    .active
                    .and_then(|(sequence, id)| (id == fault_id).then_some(sequence));
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
                        // Network faults are converged from the registry, so
                        // reconciling without the rule takes its kernel state
                        // back. A DnsNxdomain fault lives in the published set,
                        // so republish so the responder stops faulting the
                        // target.
                        self.reconcile_network_faults().await;
                        self.publish_dns_faults();
                        format!("cleared fault {} ({})", rule.id, rule.fault_type)
                    }
                    None => format!("fault {} not found", fault_id.0),
                };
                let _ = response.send(Ok(FaultClearance {
                    message: msg,
                    reservation,
                }));
            }
            AgentCommand::ClearAllFaults { response } => {
                let removed = self.fault_registry.clear_workload_faults();
                for rule in &removed {
                    self.reverse_fault(rule).await;
                }
                self.reconcile_network_faults().await;
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
                    self.reverse_fault(rule).await;
                }
                self.reconcile_network_faults().await;
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
                let merged = self.merged_service_map();
                let result = merged
                    .resolve_by_name(&app_name)
                    .map(|e| e.to_resolve_response());
                let _ = response.send(result);
            }
            AgentCommand::ResolveAll { response } => {
                let merged = self.merged_service_map();
                let results = merged
                    .resolve_all()
                    .iter()
                    .map(|e| e.to_resolve_response())
                    .collect();
                let _ = response.send(results);
            }
            AgentCommand::SyncClusterCatalog {
                generation,
                catalog,
                ingress,
                response,
            } => {
                let result = self
                    .publish_cluster_catalogue(generation, *catalog, ingress)
                    .await;
                let _ = response.send(result);
            }
            AgentCommand::SyncClusterConsumer {
                generation,
                catalog,
                ingress,
                withdrawals,
                requested_at_ns,
                response,
            } => {
                // An answer after a lapse replaces the view in place. The
                // kernel and Wrapper route only locally until the lease is
                // renewed below, and the answer is the current catalogue, so
                // every remote address it names is live.
                let result = self
                    .synchronise_consumer(generation, *catalog, ingress, withdrawals)
                    .await;
                if matches!(&result, Ok(update) if update.published) {
                    self.renew_view_lease(requested_at_ns).await;
                }
                let result = match result {
                    Err(error) => {
                        let retry = self.consumer_update(false);
                        if retry.receipts.is_empty() {
                            Err(error)
                        } else {
                            // Capacity or candidate refusal must not starve
                            // already-proven receipts needed to free capacity.
                            eprintln!("bun: consumer publication awaits retry: {error}");
                            Ok(retry)
                        }
                    }
                    success => success,
                };
                let _ = response.send(result);
            }
            AgentCommand::ConfirmConsumerReceipt {
                generation,
                response,
            } => {
                let result = self.confirm_consumer_receipt(generation).await;
                let _ = response.send(result);
            }
            AgentCommand::Routes { response } => {
                let table = self.routing_table.read().await;
                let _ = response.send(table.list_routes());
            }
            AgentCommand::SignImage {
                submission,
                response,
            } => {
                let result = self.handle_sign_image(submission).await;
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
            AgentCommand::UpgradeVerify {
                marker,
                rejoin,
                response,
            } => {
                self.handle_upgrade_verify(marker, rejoin, response).await;
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
        let prepared = match manager.prepare_rollback(version, inventory).await {
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
        rejoin: Result<(), String>,
        response: oneshot::Sender<Result<bool, BunError>>,
    ) {
        let Some(manager) = self.upgrade.clone() else {
            let _ = response.send(Err(BunError::UpgradesUnavailable));
            return;
        };

        if let Err(reason) = rejoin {
            match manager.mark_revert_pending(&marker, &reason) {
                Ok(()) => {
                    let _ = response.send(Ok(false));
                    eprintln!("bun: {reason}; restarting into the previous binary");
                    std::process::exit(1);
                }
                Err(error) => {
                    let _ = response.send(Err(BunError::Upgrade(error)));
                }
            }
            return;
        }

        // In cluster mode, workload placement is the cluster's decision:
        // the scheduler may legitimately move an app off this node while it
        // bounces, so a missing pre-upgrade instance is NOT an upgrade
        // failure. Boot grace and fresh gossip acknowledgement provide
        // separate local and cluster liveness proofs; boot failures are
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
    ///
    /// `replica_evidence`, when the API supplies it, replaces the local
    /// replica counts with cluster-wide ones, so a routed kill of the one
    /// replica this node holds is judged against the whole service.
    async fn build_safety_context(
        &self,
        request: &crate::smoker::types::FaultRequest,
        replica_evidence: Option<crate::smoker::types::ReplicaEvidence>,
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
                    && request.namespace.as_deref() == Some(i.namespace.as_str())
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
                        | crate::smoker::types::FaultType::CouncilPartition { .. }
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

        let (target_service_replicas, target_service_faulted_replicas) = match replica_evidence {
            Some(evidence) => (evidence.replicas, evidence.faulted_replicas),
            None => (target_service_replicas, target_service_faulted_replicas),
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
                if rule.namespace.as_deref().is_none_or(str::is_empty) {
                    return Err("DNS faults require an explicit namespace".into());
                }
                if rule.target_instance.is_some() {
                    return Err("DNS faults target a namespace-qualified service, not an individual instance".into());
                }
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
                // eBPF implementation. The rule is already in the registry,
                // so reconciling installs it; a failure here makes the caller
                // remove the rule and reconcile again, which takes back any
                // key this attempt wrote.
                #[cfg(all(feature = "ebpf", target_os = "linux"))]
                {
                    if self.onion_ebpf.is_some() {
                        self.check_connect_fault(rule).await?;
                        return self.reconcile_connect_faults().await;
                    }
                }
                Err(format!(
                    "{} requires the eBPF data path, which is not loaded on this node",
                    rule.fault_type
                ))
            }
            FaultType::Delay { .. } => {
                // The connect hook decides whether a connection may start; it
                // can't hold packets back. A netem qdisc on the caller's own
                // interface can, for new and open connections alike.
                #[cfg(target_os = "linux")]
                {
                    self.apply_delay_fault(rule).await
                }
                #[cfg(not(target_os = "linux"))]
                {
                    Err("delay faults need Linux traffic control (tc netem) in each caller's network namespace".to_string())
                }
            }
            FaultType::Bandwidth { .. } => Err(
                "bandwidth faults are not implemented yet; delay traffic with `relish fault delay` instead"
                    .to_string(),
            ),
            FaultType::MemoryPressure { percentage } => {
                // Squeeze the TARGET instance's `memory.high` toward its hard
                // limit so the kernel forces reclaim/allocation stalls on the
                // workload (CHAOS1 — this used to be a genuine no-op that
                // reported success).
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
            FaultType::CouncilPartition { peers } => {
                // Block both the gossip and Raft transports to each named
                // peer, and record exactly which peers so clear and expiry
                // unblock these and leave any other partition in force.
                self.apply_partition(peers).await;
                self.record_reversal(
                    rule.id,
                    crate::smoker::types::FaultReversal::Partition {
                        peers: peers.clone(),
                    },
                );
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
                    && rule.matches_namespace(&i.namespace)
                    && rule.target_instance.as_ref().is_none_or(|t| &i.id.0 == t)
                    && !i.is_being_created()
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

    /// Original `(instance id, cgroup path)` pairs for matching workloads.
    /// Rollout generations must never share their predecessor's target path.
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
            .filter_map(|instance| {
                Some((
                    instance.id.clone(),
                    instance.oci_spec.as_ref()?.linux.host_cgroup_path()?,
                ))
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

    /// Serialise the fence with activation and retain ownership if cleanup fails.
    async fn fence_node_fault(
        &mut self,
        grant: &crate::smoker::reservation::NodeFaultReservation,
        only_if_finished: bool,
    ) -> Result<(), String> {
        if only_if_finished
            && (!self.node_fault_fence.consumed(grant)
                || (grant.boot_id == self.node_fault_fence.boot_id
                    && self.node_fault_fence.active.is_some_and(|(sequence, id)| {
                        sequence == grant.sequence && self.fault_registry.get(id).is_some()
                    })))
        {
            return Err("node fault activation or reversal is still pending".into());
        }
        if let Some(id) = self.node_fault_fence.fence(grant) {
            if matches!(
                grant.request.fault_type,
                crate::smoker::types::FaultType::CouncilPartition { .. }
            ) {
                // The single node-experiment slot owns these transport lists.
                // Peer addresses may have changed since activation; removing
                // only today's addresses cannot prove the old entries are gone.
                self.clear_partition().await;
            }
            if matches!(
                grant.request.fault_type,
                crate::smoker::types::FaultType::NodePressure { .. }
            ) {
                self.node_pressure.clear(id).await?;
            }
            if let Some(rule) = self.fault_registry.get(id).cloned() {
                if !matches!(
                    rule.fault_type,
                    crate::smoker::types::FaultType::NodePressure { .. }
                ) {
                    self.reverse_fault(&rule).await;
                }
                self.fault_registry.remove(id);
            }
            // A failed apply or expiry may have removed its registry entry;
            // inspect pressure helpers independently before acknowledging.
        }
        if matches!(
            grant.request.fault_type,
            crate::smoker::types::FaultType::NodePressure { .. }
        ) {
            self.node_pressure.confirm_no_helpers().await?;
        }
        if self
            .node_fault_fence
            .active
            .is_some_and(|(sequence, _)| sequence <= grant.sequence)
            && self.node_fault_fence.boot_id == grant.boot_id
        {
            self.node_fault_fence.active = None;
        }
        Ok(())
    }

    /// Reverse a cleared or expired fault's persistent effect.
    ///
    /// Network faults are undone by `reconcile_network_faults`; this handles
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
                for (_id, cgroup, value) in self.rejoin_cgroups(rule, saved) {
                    if let Err(e) = crate::smoker::resource::restore_cpu_max(&cgroup, &value) {
                        eprintln!(
                            "smoker: restore cpu.max on {} failed: {e}",
                            cgroup.display()
                        );
                    }
                }
            }
            FaultReversal::MemoryHigh(saved) => {
                for (_id, cgroup, value) in self.rejoin_cgroups(rule, saved) {
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
                    eprintln!(
                        "smoker: reversed node fault {} on {:?}; transports quiesced={}",
                        rule.id,
                        rule.target_node,
                        cluster.partition_blocklists.node_gate.is_quiesced()
                    );
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
    /// The cgroup path comes from the current instance's original OCI specification so
    /// reversal writes to the same directory the fault wrote to. An instance
    /// that has since gone away is dropped (nothing to restore).
    fn rejoin_cgroups(
        &self,
        rule: &crate::smoker::types::FaultRule,
        saved: &[(String, String)],
    ) -> Vec<(String, std::path::PathBuf, String)> {
        saved
            .iter()
            .filter_map(|(id, value)| {
                let instance = self.supervisor.get_instance(&InstanceId(id.clone()))?;
                if instance.app_name != rule.target_service
                    || !rule.matches_namespace(&instance.namespace)
                {
                    return None;
                }
                let path = instance.oci_spec.as_ref()?.linux.host_cgroup_path()?;
                // A specific instance target still restores only its own cgroup.
                if rule.target_instance.as_ref().is_some_and(|t| t != id) {
                    return None;
                }
                Some((id.clone(), path, value.clone()))
            })
            .collect()
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
        // Converge network faults every tick, not only on expiry: a source
        // instance that started or restarted since the last tick needs the
        // faults already active against its targets.
        self.reconcile_network_faults().await;
        // Retry any node-pressure cgroup whose directory lingered after its
        // helper was killed, so a transient removal failure doesn't leave the
        // controller permanently refusing new pressure faults.
        self.node_pressure.retry_pending_cleanup().await;
    }

    /// Local instances that may call a faulted service.
    ///
    /// Only instances of an app some active fault names as its source need a
    /// cgroup id (the connect hook keys source-scoped faults by cgroup), and
    /// those are cached per restart, so the reconcile that runs on every
    /// health tick doesn't ask the runtime again for an unchanged container.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    async fn local_callers(&mut self) -> Vec<crate::smoker::network::LocalCaller> {
        use crate::smoker::network::{LocalCaller, applies_to_caller};

        let live: Vec<(InstanceId, String, String, u32)> = self
            .supervisor
            .list_instances()
            .into_iter()
            .filter(|instance| {
                matches!(
                    instance.state,
                    ContainerState::Starting
                        | ContainerState::HealthWait
                        | ContainerState::Running
                        | ContainerState::Unhealthy
                )
            })
            .map(|instance| {
                (
                    instance.id.clone(),
                    instance.app_name.clone(),
                    instance.namespace.clone(),
                    instance.restart_count,
                )
            })
            .collect();
        self.network_faults
            .caller_cgroups
            .retain(|id, _| live.iter().any(|(live_id, ..)| live_id == id));

        let mut callers = Vec::with_capacity(live.len());
        for (id, app, namespace, restarts) in live {
            let named_as_source = self.fault_registry.iter().any(|rule| {
                rule.fault_type.source_app().is_some() && applies_to_caller(rule, &app, &namespace)
            });
            let cgroup_id = match self.network_faults.caller_cgroups.get(&id) {
                _ if !named_as_source => None,
                Some((seen_at, cgroup)) if *seen_at == restarts => Some(*cgroup),
                _ => match self.supervisor.grill().workload_cgroup(&id).await {
                    Ok(Some(cgroup)) => {
                        self.network_faults
                            .caller_cgroups
                            .insert(id.clone(), (restarts, cgroup));
                        Some(cgroup)
                    }
                    Ok(None) => None,
                    Err(error) => {
                        eprintln!("smoker: caller {id} has no provable cgroup: {error}");
                        None
                    }
                },
            };
            callers.push(LocalCaller {
                instance_id: id.0,
                app,
                namespace,
                cgroup_id,
            });
        }
        callers
    }

    /// Bring every network fault's kernel state on this node in line with
    /// the active faults and the instances running now.
    ///
    /// Called after a fault is injected, cleared or expires, when a local
    /// instance starts, and on every health tick while a network fault is
    /// active, so a source replica that restarts or is scheduled here picks
    /// the fault up. Failures are logged; the next tick retries.
    async fn reconcile_network_faults(&mut self) {
        #[cfg(target_os = "linux")]
        self.sweep_stale_delays().await;
        let active = self
            .fault_registry
            .iter()
            .any(|rule| rule.fault_type.acts_on_callers());
        if !active
            && self.network_faults.connect.is_empty()
            && self.network_faults.delays.is_empty()
        {
            return;
        }
        if let Err(error) = self.reconcile_connect_faults().await {
            eprintln!("smoker: network fault reconcile: {error}");
        }
        #[cfg(target_os = "linux")]
        for (instance, error) in self.reconcile_delays().await {
            eprintln!("smoker: delay on {instance}: {error}");
        }
    }
    /// Check and install a delay fault on this node (Linux only).
    ///
    /// A delay is a netem qdisc on each caller container's own `eth0`, so it
    /// needs runc's per-container network namespaces, a target with backends
    /// to steer towards, and (for `--from`) a local instance of the source.
    /// The rule is already in the registry: reconciling installs it, and any
    /// caller that couldn't be shaped fails the injection.
    #[cfg(target_os = "linux")]
    async fn apply_delay_fault(
        &mut self,
        rule: &crate::smoker::types::FaultRule,
    ) -> Result<(), String> {
        let runtime = self.supervisor.grill().runtime_kind();
        if runtime != crate::grill::records::RuntimeKind::Runc {
            return Err(format!(
                "delay faults shape each caller container's own network interface, which needs the runc runtime; this node runs {runtime:?}"
            ));
        }
        let services = self.merged_service_map();
        if fault_backend_addresses(&services, rule).is_empty() {
            return Err(format!(
                "{}/{} has no backends to delay traffic to",
                rule.namespace.as_deref().unwrap_or("default"),
                rule.target_service
            ));
        }
        let callers: Vec<String> = self
            .local_callers()
            .await
            .into_iter()
            .filter(|caller| {
                crate::smoker::network::applies_to_caller(rule, &caller.app, &caller.namespace)
            })
            .map(|caller| caller.instance_id)
            .collect();
        if let Some(source) = rule.fault_type.source_app()
            && callers.is_empty()
        {
            return Err(format!(
                "no running instance of source app {source} runs on this node"
            ));
        }
        let failures: Vec<String> = self
            .reconcile_delays()
            .await
            .into_iter()
            .filter(|(instance, _)| callers.contains(instance))
            .map(|(_, error)| error)
            .collect();
        match failures.first() {
            None => Ok(()),
            Some(error) => Err(format!("cannot delay traffic: {error}")),
        }
    }

    /// Remove any delay tree a previous Bun left on this node's containers.
    ///
    /// Faults don't survive a restart, but a netem qdisc lives in the
    /// container's network namespace, not in Bun, so a crashed Bun would
    /// leave its callers slowed forever. Runs once, on the first reconcile.
    #[cfg(target_os = "linux")]
    async fn sweep_stale_delays(&mut self) {
        if self.network_faults.delays_swept
            || self.supervisor.grill().runtime_kind() != crate::grill::records::RuntimeKind::Runc
        {
            return;
        }
        self.network_faults.delays_swept = true;
        let instances: Vec<String> = self
            .supervisor
            .list_instances()
            .into_iter()
            .map(|instance| instance.id.0.clone())
            .collect();
        for instance in instances {
            match remove_delay_tree(&instance).await {
                Ok(true) => eprintln!("smoker: removed a stale delay from {instance}"),
                Ok(false) | Err(crate::smoker::network::NetnsCommandError::NoNamespace { .. }) => {}
                Err(error) => eprintln!("smoker: stale delay sweep: {error}"),
            }
        }
    }

    /// Converge every local caller's netem delays on what the active delay
    /// faults ask for. Returns `(instance, error)` for each caller whose
    /// interface couldn't be programmed; those are retried next tick.
    #[cfg(target_os = "linux")]
    async fn reconcile_delays(&mut self) -> Vec<(String, String)> {
        use crate::smoker::network::{NetnsCommandError, desired_delays};

        let delaying = self.fault_registry.iter().any(|rule| {
            matches!(
                rule.fault_type,
                crate::smoker::types::FaultType::Delay { .. }
            )
        });
        if !delaying && self.network_faults.delays.is_empty() {
            return Vec::new();
        }
        let callers = self.local_callers().await;
        let services = self.merged_service_map();
        let desired = desired_delays(
            self.fault_registry.iter(),
            |rule| fault_backend_addresses(&services, rule),
            &callers,
        );
        let restarts: std::collections::HashMap<String, u32> = self
            .supervisor
            .list_instances()
            .into_iter()
            .map(|instance| (instance.id.0.clone(), instance.restart_count))
            .collect();
        // A caller that has gone took its network namespace, and its qdisc,
        // with it.
        self.network_faults
            .delays
            .retain(|id, _| restarts.contains_key(id));

        let mut instances: std::collections::BTreeSet<String> = desired.keys().cloned().collect();
        instances.extend(self.network_faults.delays.keys().cloned());
        let mut failures = Vec::new();
        for instance in instances {
            let wanted = desired.get(&instance);
            let restart = restarts.get(&instance).copied().unwrap_or_default();
            let unchanged = match (wanted, self.network_faults.delays.get(&instance)) {
                (Some(wanted), Some((seen_at, installed))) => {
                    *seen_at == restart && installed == wanted
                }
                (None, None) => true,
                _ => false,
            };
            if unchanged {
                continue;
            }
            let bands = wanted.map(Vec::as_slice).unwrap_or_default();
            match program_delay_tree(&instance, bands).await {
                Ok(()) => match wanted {
                    Some(wanted) => {
                        self.network_faults
                            .delays
                            .insert(instance, (restart, wanted.clone()));
                    }
                    None => {
                        self.network_faults.delays.remove(&instance);
                    }
                },
                // A caller without its own namespace (host networking)
                // can't be shaped; remember that so we don't retry every
                // tick, and report it once.
                Err(error @ NetnsCommandError::NoNamespace { .. }) => {
                    if let Some(wanted) = wanted {
                        self.network_faults
                            .delays
                            .insert(instance.clone(), (restart, wanted.clone()));
                        failures.push((instance, error.to_string()));
                    } else {
                        self.network_faults.delays.remove(&instance);
                    }
                }
                Err(error) => {
                    self.network_faults.delays.remove(&instance);
                    failures.push((instance, delay_error_hint(&error)));
                }
            }
        }
        failures
    }

    /// Check that a drop or partition can take effect here before reporting
    /// it installed: the target's VIP is known, and a source-scoped fault has
    /// at least one local source instance with a provable cgroup.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    async fn check_connect_fault(
        &mut self,
        rule: &crate::smoker::types::FaultRule,
    ) -> Result<(), String> {
        let services = self.merged_service_map();
        if fault_vip_port(&services, rule).is_none() {
            return Err(format!(
                "no service VIP exists for {}/{}",
                rule.namespace.as_deref().unwrap_or("default"),
                rule.target_service
            ));
        }
        let Some(source) = rule.fault_type.source_app() else {
            return Ok(());
        };
        let callers = self.local_callers().await;
        let proven = callers.iter().any(|caller| {
            caller.cgroup_id.is_some()
                && crate::smoker::network::applies_to_caller(rule, &caller.app, &caller.namespace)
        });
        if proven {
            Ok(())
        } else {
            Err(format!(
                "no running instance of source app {source} on this node has a verified workload cgroup"
            ))
        }
    }

    /// Converge the eBPF `fault_connect_map` on what the active drop and
    /// partition faults ask for (see `smoker::network`).
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    async fn reconcile_connect_faults(&mut self) -> Result<(), String> {
        use crate::smoker::bpf_maps;
        use crate::smoker::bpf_types::{
            BpfConnectFaultValue, FAULT_ACTION_DROP, FAULT_ACTION_PARTITION, partition_fault_key,
        };
        use crate::smoker::network::{
            ConnectFaultAction, connect_fault_changes, connections_to_cut, desired_connect_faults,
            lands,
        };

        let Some(handle) = self.onion_ebpf.clone() else {
            return Ok(());
        };
        let callers = self.local_callers().await;
        let services = self.merged_service_map();
        let desired = desired_connect_faults(
            self.fault_registry.iter(),
            |rule| fault_vip_port(&services, rule),
            &callers,
        );
        let changes = connect_fault_changes(&self.network_faults.connect, &desired);
        if changes.write.is_empty() && changes.delete.is_empty() {
            return Ok(());
        }

        let mut failures = Vec::new();
        let mut landed = Vec::new();
        let mut ebpf = handle.lock().await;
        for key in changes.delete {
            let bpf_key = partition_fault_key(key.virtual_ip, key.port, key.source_cgroup_id);
            match bpf_maps::delete_connect_fault(&mut ebpf.bpf, &bpf_key) {
                Ok(()) => {
                    self.network_faults.connect.remove(&key);
                }
                Err(error) => failures.push(format!("delete {key:?}: {error}")),
            }
        }
        for (key, entry) in changes.write {
            let (action, probability) = match entry.action {
                ConnectFaultAction::Drop { probability } => (FAULT_ACTION_DROP, probability),
                ConnectFaultAction::Partition => (FAULT_ACTION_PARTITION, 100),
            };
            let value = BpfConnectFaultValue {
                action,
                probability,
                _pad: [0; 6],
                delay_ns: 0,
                jitter_ns: 0,
                expires_ns: entry.expires_ns,
            };
            let bpf_key = partition_fault_key(key.virtual_ip, key.port, key.source_cgroup_id);
            match bpf_maps::write_connect_fault(&mut ebpf.bpf, bpf_key, value) {
                Ok(()) => {
                    if lands(self.network_faults.connect.get(&key), &entry) {
                        landed.push(key);
                    }
                    self.network_faults.connect.insert(key, entry);
                }
                Err(error) => failures.push(format!("write {key:?}: {error}")),
            }
        }
        drop(ebpf);

        // The hook only refuses new connections, so cut the ones already
        // open: a pooled client reconnects straight into the fault.
        let cuts = connections_to_cut(&landed, &callers, |virtual_ip, port| {
            backend_addresses(&services, virtual_ip, port)
        });
        for cut in cuts {
            let args = crate::smoker::network::socket_destroy_args(&cut.backends);
            match crate::smoker::network::run_in_instance_netns(&cut.instance_id, "ss", &args).await
            {
                // Process and host-network workloads have no namespace of
                // their own; their sockets live in the host's, among every
                // other caller's, so they are left alone.
                Ok(_) | Err(crate::smoker::network::NetnsCommandError::NoNamespace { .. }) => {}
                Err(error) => eprintln!("smoker: cutting open connections: {error}"),
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "failed to program fault_connect_map: {}",
                failures.join("; ")
            ))
        }
    }

    /// Without the eBPF data path there is no connect map to converge.
    #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
    async fn reconcile_connect_faults(&mut self) -> Result<(), String> {
        Ok(())
    }

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
    ) -> Result<crate::grill::oci::OciSpec, BunError> {
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
        // A decryption failure fails the deploy closed (M4): the container must
        // not start with a broken secret injected as `DECRYPT_ERROR:...`.
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
        .map_err(|reason| BunError::DeployFailed {
            app_name: app_name.to_string(),
            reason,
        })
    }

    /// Program a freshly-started instance's kernel networking: mirror its
    /// backend into `backend_map` (L8) and reconcile namespace-firewall maps
    /// (NET5). Egress is deliberately absent here: it must already have been
    /// programmed before `start`, never repaired in post-start bookkeeping.
    async fn finish_instance_networking(
        &mut self,
        app_name: &str,
        namespace: &str,
    ) -> Result<(), BunError> {
        let service_id = crate::onion::service_id::ServiceId::new(namespace, app_name);
        self.publish_backend_ebpf(&service_id).await?;
        self.sync_firewall_ebpf().await;
        // A new caller must meet the network faults already active against
        // the services it calls.
        self.reconcile_network_faults().await;
        Ok(())
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

        let cgroup_path =
            crate::grill::cgroup::instance_cgroup_path(namespace, app_name, instance_id)?;
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
        // Claim test storage and provision every bind source before launch.
        self.prepare_storage(app_name, namespace, spec).await?;

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
        )?;

        Ok(PreparedInstance {
            oci_spec,
            cgroup_path,
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
        self.persist_instance_record(instance_id).await?;

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
            let service_id = crate::onion::service_id::ServiceId::new(namespace, app_name);
            let backend = self.local_backend(
                instance_id,
                &service_id,
                instance.container_ip,
                host_port,
                instance.state == ContainerState::Running,
            );
            self.service_map
                .add_backend(&service_id, backend)
                .map_err(|error| BunError::BackendPublication {
                    service: service_id,
                    reason: error.to_string(),
                })?;
        }

        self.finish_instance_networking(app_name, namespace).await?;
        Ok(())
    }

    async fn reserve_rolling_instance(
        &mut self,
        id: &InstanceId,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
    ) -> Result<Option<u16>, BunError> {
        if let Some(owner) = self.supervisor.get_instance(id) {
            return Err(BunError::DeployFailed {
                app_name: app_name.into(),
                reason: format!(
                    "instance {id} is still owned by {}/{}",
                    owner.namespace, owner.app_name
                ),
            });
        }
        let host_port = if spec.port.is_some() {
            Some(self.supervisor.port_allocator.allocate().await?)
        } else {
            None
        };
        self.supervisor.instances.insert(
            id.clone(),
            super::supervisor::WorkloadInstance {
                id: id.clone(),
                app_name: app_name.into(),
                namespace: namespace.into(),
                state: ContainerState::Preparing,
                health_counters: Default::default(),
                restart_count: 0,
                last_restart: None,
                host_port,
                container_ip: None,
                created_at: Instant::now(),
                restart_policy: Default::default(),
                health_config: None,
                is_job: false,
                retry_pending: false,
                image: spec.image.clone().unwrap_or_default(),
                oci_spec: None,
                identity: None,
                identity_mount: None,
            },
        );
        self.supervisor
            .app_instances
            .entry((app_name.into(), namespace.into()))
            .or_default()
            .push(id.clone());
        Ok(host_port)
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
        self.prepare_storage(app_name, namespace, spec).await?;
        if let Err(e) = self.prepare_instance_identity(instance_id) {
            eprintln!("bun: warning: {e}");
        }
        let cgroup_path =
            crate::grill::cgroup::instance_cgroup_path(namespace, app_name, instance_id)?;
        let oci_spec = Self::oci_spec_with_secrets(
            app_name,
            namespace,
            spec,
            &instance_id.0,
            host_port,
            &cgroup_path.to_string_lossy(),
            Some(&self.volumes_dir),
            None,
            identities,
        )?;
        let owner = self
            .supervisor
            .get_instance_mut(instance_id)
            .ok_or_else(|| BunError::InstanceNotFound {
                instance_id: instance_id.clone(),
            })?;
        owner.oci_spec = Some(oci_spec.clone());
        Ok(oci_spec)
    }

    /// Forget the old instances and register the healthy new ones after a
    /// redeploy: rebuild the service map and health config, register backends,
    /// finish kernel networking, store ingress, record history.
    ///
    /// Bookkeeping only (M7): the deploy worker has already drained and
    /// stopped every instance in `existing` off the command loop via
    /// `drain_and_stop_instance` — no waiting happens here.
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
    ) -> Result<(), BunError> {
        // DEP5: the worker routed traffic to the fresh instances (published
        // backends) before draining and stopping the old ones, so by the time
        // this op runs the cut-over has already happened. What's left is to
        // tear the old bookkeeping down and install the new.
        // A failed first cleanup must not leave another exited old instance
        // eligible for the crash-restart driver.
        for old_id in existing {
            self.retain_stopped_instance(old_id);
        }
        let service_id = crate::onion::service_id::ServiceId::new(namespace, app_name);
        // M7: publishing backends and retiring old instances now happen
        // incrementally as the rollout steps, so by the time we get here both
        // are usually already done. These loops are idempotent catch-ups for
        // anything the stepped path didn't cover (a zero-port app, or an
        // instance the planner retired before this call).
        if spec.port.is_some() {
            for new_id in new_ids {
                if let Some(host_port) = new_ports.get(new_id).copied().flatten() {
                    let backend = self.local_backend(
                        new_id,
                        &service_id,
                        new_ips.get(new_id).copied().flatten(),
                        host_port,
                        true,
                    );
                    self.service_map
                        .add_backend(&service_id, backend)
                        .map_err(|error| BunError::BackendPublication {
                            service: service_id.clone(),
                            reason: error.to_string(),
                        })?;
                }
            }
            self.rebuild_routing_table().await;
        }

        for old_id in existing {
            match self.finish_retire_bookkeeping(old_id).await {
                Err(BunError::ProducerReleasePending { .. }) => self.defer_retirement(old_id),
                result => result?,
            }
        }
        self.withdraw_service_ebpf(&service_id).await?;
        // Re-registration can be refused: a stop that withdrew the council's
        // allocation mid-rollout leaves nothing to register against. The
        // retained replacements then retire by proving withdrawal against
        // this local reservation, so a refusal must put it back.
        let reserved = self.service_map.clone();
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
            let (identity, identity_mount) = self
                .supervisor
                .get_instance_mut(new_id)
                .map(|owner| (owner.identity.take(), owner.identity_mount.take()))
                .unwrap_or_default();
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
                    retry_pending: false,
                    image: spec.image.clone().unwrap_or_default(),
                    oci_spec: new_specs.remove(new_id),
                    identity,
                    identity_mount,
                },
            );
        }
        let key = (app_name.to_string(), namespace.to_string());
        self.supervisor.app_instances.insert(key, new_ids.to_vec());

        if let Some(port) = spec.port
            && let Err(error) = self.register_replacement_service(
                &service_id,
                port,
                spec,
                new_ids,
                new_ports,
                new_ips,
            )
        {
            self.service_map = reserved;
            return Err(error);
        }

        self.finish_instance_networking(app_name, namespace).await?;
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
        Ok(())
    }

    /// Register a rolled-out app's service and its replacement backends. The
    /// caller restores the previous reservation if this refuses.
    fn register_replacement_service(
        &mut self,
        service_id: &crate::onion::service_id::ServiceId,
        port: u16,
        spec: &AppSpec,
        new_ids: &[InstanceId],
        new_ports: &std::collections::HashMap<InstanceId, Option<u16>>,
        new_ips: &std::collections::HashMap<InstanceId, Option<std::net::Ipv4Addr>>,
    ) -> Result<(), BunError> {
        let firewall = spec
            .firewall
            .as_ref()
            .filter(|firewall| !firewall.allow_from.is_empty())
            .map(|firewall| firewall.allow_from.clone());
        self.register_local_service(service_id, port, firewall)?;
        for new_id in new_ids {
            let Some(host_port) = new_ports.get(new_id).copied().flatten() else {
                continue;
            };
            let backend = self.local_backend(
                new_id,
                service_id,
                new_ips.get(new_id).copied().flatten(),
                host_port,
                true,
            );
            self.service_map
                .add_backend(service_id, backend)
                .map_err(|error| BunError::BackendPublication {
                    service: service_id.clone(),
                    reason: error.to_string(),
                })?;
        }
        Ok(())
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
        self.spawn_log_forwarder(instance_id, job_name, namespace);
        self.persist_instance_record(instance_id).await?;
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
    async fn apply_network_pre_start(
        &mut self,
        instance_id: &InstanceId,
        app_name: &str,
        spec: Option<&AppSpec>,
        cgroup_path: &std::path::Path,
    ) -> Result<(), BunError> {
        self.retain_network_reference(instance_id, spec).await?;
        use crate::sesame::egress::{self, PreStartEgress};

        let has_allowlist = spec
            .and_then(|spec| spec.egress.as_ref())
            .is_some_and(|e| !e.allow.is_empty());
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
        let cgroup_id = if capability.can_enforce_allowlist() {
            let _ = tokio::fs::create_dir_all(cgroup_path).await;
            egress::cgroup_id_of_path(cgroup_path)
        } else {
            None
        };

        let require_source =
            self.onion_ebpf.is_some() && self.supervisor.grill().honours_cgroup_path();
        match egress::plan_pre_start_egress(has_allowlist, capability, cgroup_id) {
            PreStartEgress::NoPolicy if require_source => {
                let cgroup_id = cgroup_id.ok_or_else(|| BunError::DeployFailed {
                    app_name: app_name.into(),
                    reason: "source namespace cgroup could not be prepared".into(),
                })?;
                self.program_egress_pre_start(instance_id, app_name, spec, cgroup_id)
                    .await
            }
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

    async fn retain_network_reference(
        &mut self,
        id: &InstanceId,
        spec: Option<&AppSpec>,
    ) -> Result<(), BunError> {
        if spec.is_none_or(|spec| spec.port.is_none()) {
            return Ok(());
        }
        if let Some(reference) = self.supervisor.grill().retain_network_reference(id).await? {
            if reference.instance_id != *id {
                return Err(BunError::RetirementState {
                    instance_id: id.clone(),
                    reason: "runtime returned another instance's network reference".into(),
                });
            }
            if self
                .network_references
                .get(id)
                .is_some_and(|original| original != &reference)
            {
                return Err(BunError::RetirementState {
                    instance_id: id.clone(),
                    reason: "original network reference still belongs to another generation".into(),
                });
            }
            self.persist_discovery_reference(&reference).await?;
            self.network_references.insert(id.clone(), reference);
        }
        Ok(())
    }

    async fn release_network_reference(
        &mut self,
        id: &InstanceId,
        remote: Option<&crate::onion::producer::ProducerReleaseConfirmation>,
    ) -> Result<(), BunError> {
        let Some(reference) = self.network_references.get(id).cloned() else {
            if self
                .supervisor
                .grill()
                .network_reference(id)
                .await?
                .is_some()
            {
                return Err(BunError::RetirementState {
                    instance_id: id.clone(),
                    reason: "retained network reference requires original discovery reconciliation"
                        .into(),
                });
            }
            return Ok(());
        };
        self.authorise_local_discovery_release(&reference, remote)
            .await?;
        self.require_discovery_release_permission(&reference)?;
        self.supervisor
            .grill()
            .release_network_reference(&reference)
            .await?;
        self.forget_released_discovery_reference(&reference).await?;
        self.network_references.remove(id);
        Ok(())
    }

    /// A build without the eBPF data path cannot enforce an allowlist.
    #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
    async fn apply_network_pre_start(
        &mut self,
        instance_id: &InstanceId,
        app_name: &str,
        spec: Option<&AppSpec>,
        _cgroup_path: &std::path::Path,
    ) -> Result<(), BunError> {
        self.retain_network_reference(instance_id, spec).await?;
        if spec
            .and_then(|spec| spec.egress.as_ref())
            .is_some_and(|e| !e.allow.is_empty())
        {
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
        spec: Option<&AppSpec>,
        cgroup_id: u64,
    ) -> Result<(), BunError> {
        let allow = spec
            .and_then(|spec| spec.egress.as_ref())
            .map(|policy| policy.allow.as_slice())
            .unwrap_or_default();
        self.clear_egress(instance_id).await?;
        let resolved = Self::resolve_owned_egress(allow).await;
        let union: Vec<_> = self
            .egress_bindings
            .values()
            .filter(|binding| binding.phase == PolicyPhase::Owned && binding.cgroup_id == cgroup_id)
            .flat_map(|binding| binding.resolved.iter().copied())
            .chain(resolved.iter().copied())
            .collect();
        crate::sesame::egress::merge_cidr_ports(&union).map_err(|error| {
            BunError::DeployFailed {
                app_name: app_name.into(),
                reason: error.to_string(),
            }
        })?;
        let original_spec = self
            .supervisor
            .get_instance(instance_id)
            .and_then(|instance| instance.oci_spec.clone())
            .ok_or_else(|| {
                BunError::AdoptionState(format!(
                    "egress owner {instance_id} has no original runtime input"
                ))
            })?;
        let source_identity = self
            .supervisor
            .get_instance(instance_id)
            .map(|instance| (instance.namespace.clone(), instance.app_name.clone()))
            .ok_or_else(|| BunError::InstanceNotFound {
                instance_id: instance_id.clone(),
            })?;
        let source_namespace = crate::onion::vip::name_to_id(&source_identity.0);
        let boot_id = tokio::task::spawn_blocking(super::egress_owners::boot_id)
            .await
            .map_err(|error| BunError::AdoptionState(error.to_string()))?
            .map_err(|error| BunError::AdoptionState(error.to_string()))?;
        self.egress_bindings.insert(
            instance_id.clone(),
            EgressBinding {
                phase: PolicyPhase::Owned,
                cgroup_id,
                source_namespace: Some(source_namespace),
                allow: allow.to_vec(),
                resolved,
                original_spec,
                runtime: self.supervisor.grill().runtime_kind(),
                boot_id,
            },
        );
        self.persist_egress_owners(self.egress_bindings.clone())
            .await?;
        let handle = self
            .onion_ebpf
            .clone()
            .ok_or_else(|| BunError::DeployFailed {
                app_name: app_name.into(),
                reason: "kernel source policy is unavailable".into(),
            })?;
        self.cgroup_ns_bpf_keys.insert(cgroup_id);
        crate::sesame::firewall::write_cgroup_namespace_entry(
            &mut handle.lock().await.bpf,
            cgroup_id,
            source_namespace,
        )
        .map_err(|error| BunError::DeployFailed {
            app_name: app_name.into(),
            reason: error.to_string(),
        })?;
        let services = self
            .service_map
            .resolve_all()
            .into_iter()
            .cloned()
            .collect::<Vec<_>>();
        let sources = std::collections::HashMap::from([(source_identity, vec![cgroup_id])]);
        let entries = crate::sesame::firewall::rules_to_bpf_entries(
            &crate::sesame::firewall::resolve_firewall_rules(&services, &sources),
        );
        for (key, value) in entries {
            // Remember partial publication before attempting the write. The
            // durable source owner retains every grant until confirmed cleanup.
            self.firewall_bpf_keys.insert(key);
            crate::sesame::firewall::write_firewall_entry(&mut handle.lock().await.bpf, key, value)
                .map_err(|error| BunError::DeployFailed {
                    app_name: app_name.into(),
                    reason: error.to_string(),
                })?;
        }
        if allow.is_empty() {
            return Ok(());
        }
        // Keep the enable flag while rebuilding. During a rollout the old and
        // new instances may share a cgroup, so removing it would open a gap.
        self.reprogram_cgroup_egress(cgroup_id, None)
            .await
            .map_err(|error| BunError::DeployFailed {
                app_name: app_name.into(),
                reason: format!("egress map programming failed for {instance_id}: {error}"),
            })
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
    async fn clear_egress(&mut self, instance_id: &InstanceId) -> Result<(), BunError> {
        if self.egress_store_uncertain {
            return Err(BunError::AdoptionState(
                "egress ownership persistence is uncertain; restart to recover the checkpoint"
                    .into(),
            ));
        }
        let Some(binding) = self.egress_bindings.get(instance_id).cloned() else {
            return Ok(());
        };
        if binding.phase == PolicyPhase::Retired {
            return Ok(());
        }
        let boot_id = tokio::task::spawn_blocking(super::egress_owners::boot_id)
            .await
            .map_err(|error| BunError::AdoptionState(error.to_string()))?
            .map_err(|error| BunError::AdoptionState(error.to_string()))?;
        if binding.boot_id == boot_id {
            self.reprogram_cgroup_egress(binding.cgroup_id, Some(instance_id))
                .await
                .map_err(|error| BunError::RetirementState {
                    instance_id: instance_id.clone(),
                    reason: error.to_string(),
                })?;
        }
        if binding.boot_id == boot_id
            && binding.source_namespace.is_some()
            && !self.egress_bindings.iter().any(|(id, owner)| {
                id != instance_id
                    && owner.phase == PolicyPhase::Owned
                    && owner.cgroup_id == binding.cgroup_id
            })
        {
            let handle = self
                .onion_ebpf
                .clone()
                .ok_or_else(|| BunError::RetirementState {
                    instance_id: instance_id.clone(),
                    reason: "kernel source policy is unavailable".into(),
                })?;
            crate::sesame::firewall::delete_cgroup_firewall_state(
                &mut handle.lock().await.bpf,
                binding.cgroup_id,
            )
            .map_err(|error| BunError::RetirementState {
                instance_id: instance_id.clone(),
                reason: error.to_string(),
            })?;
            self.cgroup_ns_bpf_keys.remove(&binding.cgroup_id);
            self.firewall_bpf_keys
                .retain(|key| key.src_cgroup_id != binding.cgroup_id);
        }
        // The caller has retired the previous workload. A different boot proves the
        // old kernel maps are gone; never delete a recycled current-boot key.
        let mut confirmed = binding;
        confirmed.phase = PolicyPhase::Retired;
        confirmed.resolved.clear();
        let mut owners = self.egress_bindings.clone();
        owners.insert(instance_id.clone(), confirmed.clone());
        self.persist_egress_owners(owners).await?;
        self.egress_bindings.insert(instance_id.clone(), confirmed);
        Ok(())
    }

    /// No-op without the eBPF data path.
    #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
    async fn clear_egress(&mut self, _instance_id: &InstanceId) -> Result<(), BunError> {
        Ok(())
    }

    /// Rebuild one cgroup's policy, excluding a retiring instance only from the
    /// proposed kernel state. Its binding remains owned until every write succeeds.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    async fn reprogram_cgroup_egress(
        &mut self,
        cgroup_id: u64,
        excluding: Option<&InstanceId>,
    ) -> Result<(), crate::sesame::egress::EgressMapError> {
        use crate::sesame::egress;
        if self.egress_store_uncertain {
            return Err(egress::EgressMapError::Unavailable);
        }
        let handle = self
            .onion_ebpf
            .clone()
            .ok_or(egress::EgressMapError::Unavailable)?;
        let survivors: Vec<_> = self
            .egress_bindings
            .iter()
            .filter(|(id, binding)| {
                binding.phase == PolicyPhase::Owned
                    && !binding.allow.is_empty()
                    && Some(*id) != excluding
                    && binding.cgroup_id == cgroup_id
            })
            .map(|(_, binding)| binding)
            .collect();
        let mut ebpf = handle.lock().await;
        if survivors.is_empty() {
            return egress::delete_cgroup_egress_state(&mut ebpf.bpf, cgroup_id);
        }
        let union: Vec<_> = survivors
            .iter()
            .flat_map(|binding| binding.resolved.iter().copied())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let merged = egress::merge_cidr_ports(&union)?;
        egress::set_egress_enforced(&mut ebpf.bpf, cgroup_id)?;
        egress::delete_cgroup_egress_entries(&mut ebpf.bpf, cgroup_id)?;
        egress::write_egress_destinations(&mut ebpf.bpf, cgroup_id, &union, &merged)
    }

    /// Stop every workload affected by an unconfirmed policy rewrite.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    async fn handle_egress_rewrite_failure(
        &mut self,
        cgroup_id: u64,
        error: crate::sesame::egress::EgressMapError,
    ) {
        eprintln!("sesame: egress rewrite failed for cgroup {cgroup_id}: {error}");
        let affected = self
            .egress_bindings
            .iter()
            .filter(|(_, binding)| {
                binding.phase == PolicyPhase::Owned && binding.cgroup_id == cgroup_id
            })
            .map(|(id, _)| id.clone())
            .collect();
        self.stop_instances_after_egress_loss(affected).await;
    }

    /// Fence executing workloads whose original namespace identity is unavailable.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    async fn enforce_live_source_or_stop(&mut self) {
        if !self.supervisor.grill().honours_cgroup_path() {
            return;
        }
        let Some(handle) = self.onion_ebpf.clone() else {
            return;
        };
        let instances: Vec<_> = self
            .supervisor
            .list_instances()
            .into_iter()
            .filter(|instance| {
                !matches!(
                    instance.state,
                    ContainerState::Pending
                        | ContainerState::Preparing
                        | ContainerState::Stopped
                        | ContainerState::Failed
                )
            })
            .map(|instance| instance.id.clone())
            .collect();
        let mut failed = std::collections::HashSet::new();
        let mut handle = handle.lock().await;
        let hooks = handle.is_attached()
            && handle.connect6_attached()
            && handle.sendmsg4_attached()
            && handle.sendmsg6_attached();
        for id in instances {
            let original = self
                .egress_bindings
                .get(&id)
                .filter(|owner| owner.phase == PolicyPhase::Owned)
                .and_then(|owner| {
                    owner
                        .source_namespace
                        .map(|namespace| (owner.cgroup_id, namespace))
                });
            let valid = if let Some((cgroup, namespace)) = original {
                hooks
                    && crate::sesame::firewall::read_firewall_state(&mut handle.bpf, cgroup, 0)
                        .is_ok_and(|state| state.source_namespace_id == Some(namespace))
            } else {
                false
            };
            if !valid {
                failed.insert(id);
            }
        }
        drop(handle);
        self.stop_instances_after_egress_loss(failed).await;
    }

    /// Verify the security boundary on every event-loop tick. Map drift gets
    /// one immediate repair attempt. If any required hook is gone, the map can't be
    /// read, or a repaired enforcement flag is still absent, stop every
    /// affected workload. Keeping it running would turn its allowlist into a
    /// label rather than a control.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    async fn enforce_live_egress_or_stop(
        &mut self,
    ) -> crate::sesame::egress::EgressEnforcementCapability {
        #[cfg(test)]
        self.egress_observation_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        use crate::sesame::egress;

        self.enforce_live_source_or_stop().await;

        // Pending/preparing work cannot execute yet. The deployment driver
        // installs policy before entering Initialising or Starting; monitoring
        // must not race that installation while an image is still being pulled.
        let unbound: std::collections::HashSet<InstanceId> = self
            .supervisor
            .list_instances()
            .iter()
            .filter(|instance| {
                !matches!(
                    instance.state,
                    ContainerState::Pending
                        | ContainerState::Preparing
                        | ContainerState::Stopped
                        | ContainerState::Failed
                )
            })
            .filter(|instance| {
                self.egress_bindings
                    .get(&instance.id)
                    .is_none_or(|binding| binding.phase != PolicyPhase::Owned)
            })
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
            let mut affected: std::collections::HashSet<InstanceId> = self
                .egress_bindings
                .iter()
                .filter(|(_, binding)| binding.phase == PolicyPhase::Owned)
                .map(|(id, _)| id.clone())
                .collect();
            affected.extend(unbound);
            self.stop_instances_after_egress_loss(affected).await;
            return Default::default();
        };
        let expected: std::collections::HashSet<u64> = self
            .egress_bindings
            .values()
            .filter(|binding| binding.phase == PolicyPhase::Owned && !binding.allow.is_empty())
            .map(|b| b.cgroup_id)
            .collect();
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
            return capability;
        }

        let plan = egress::plan_live_egress_health(capability, &expected, &kernel_enforced);
        for cgroup_id in &plan.repair {
            eprintln!("sesame: live check restoring egress enforcement for cgroup {cgroup_id}");
            if let Err(error) = self.reprogram_cgroup_egress(*cgroup_id, None).await {
                self.handle_egress_rewrite_failure(*cgroup_id, error).await;
            }
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
            return capability;
        }

        let mut affected_ids: std::collections::HashSet<InstanceId> = self
            .egress_bindings
            .iter()
            .filter(|(_, binding)| {
                binding.phase == PolicyPhase::Owned && fence.contains(&binding.cgroup_id)
            })
            .map(|(id, _)| id.clone())
            .collect();
        affected_ids.extend(unbound);
        self.stop_instances_after_egress_loss(affected_ids).await;
        capability
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
            eprintln!("sesame: stopping {namespace}/{app_name}: live kernel policy was lost");
            // The stop waits out its grace off the loop; if it fails, its
            // completion fences execution (`fence_after_failed_stop`).
            if let Err(error) = self.stop_app_unattended(&app_name, &namespace).await {
                eprintln!(
                    "sesame: failed to stop {namespace}/{app_name} after egress loss: {error}"
                );
                self.fence_after_failed_stop(&app_name, &namespace).await;
            }
        }
    }

    /// Force-kill an app whose graceful stop failed, keeping every
    /// allocation it still owns.
    async fn fence_after_failed_stop(&mut self, app_name: &str, namespace: &str) {
        #[cfg(all(feature = "ebpf", target_os = "linux"))]
        if let Err(error) = self.fence_app_execution(app_name, namespace).await {
            eprintln!(
                "sesame: execution fencing remains unconfirmed for {namespace}/{app_name}: {error}"
            );
        }
        // Only the egress fence asks for this, and it exists only with eBPF.
        #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
        let _ = (app_name, namespace);
    }

    /// Stop unsafe execution while preserving refused discovery and policy cleanup.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    async fn fence_app_execution(
        &mut self,
        app_name: &str,
        namespace: &str,
    ) -> Result<(), BunError> {
        let instances: Vec<_> = self
            .supervisor
            .list_instances()
            .iter()
            .filter(|instance| {
                instance.app_name == app_name
                    && instance.namespace == namespace
                    && instance.state != ContainerState::Stopped
            })
            .map(|instance| {
                (
                    instance.id.clone(),
                    instance.container_ip.is_some() && instance.host_port.is_some(),
                )
            })
            .collect();
        self.supervisor.stop_app(app_name, namespace).await?;
        let mut first_error = None;
        for (id, publishes_address) in instances {
            let result = async {
                if publishes_address {
                    let reference = self.supervisor.grill().network_reference(&id).await?;
                    if reference.is_none()
                        || self
                            .network_references
                            .get(&id)
                            .is_some_and(|original| reference.as_ref() != Some(original))
                    {
                        return Err(BunError::RetirementState {
                            instance_id: id.clone(),
                            reason: "execution fencing requires the original retained address"
                                .into(),
                        });
                    }
                }
                self.retire_initialisers(&id).await?;
                self.kill_and_wait_for_exit(&id).await
            }
            .await;
            if let Err(error) = result {
                first_error.get_or_insert(error);
                continue;
            }
            if let Some(instance) = self.supervisor.get_instance_mut(&id)
                && instance.state.can_transition_to(ContainerState::Stopped)
            {
                instance.state = ContainerState::Stopped;
            }
        }
        // Address holds, service keys, grants and adoption records remain owned.
        // An execution stop is not an acknowledgement of their retirement.
        first_error.map_or(Ok(()), Err)
    }

    /// Portable builds cannot have live egress bindings.
    #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
    async fn enforce_live_egress_or_stop(
        &mut self,
    ) -> crate::sesame::egress::EgressEnforcementCapability {
        #[cfg(test)]
        self.egress_observation_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Default::default()
    }

    /// Periodically re-resolve DNS-based egress allowlists and reprogram the
    /// eBPF egress maps when an app's destination IPs change (L16). Rate-
    /// limited to roughly once every five minutes; a no-op while nothing
    /// enforces egress.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    async fn reresolve_egress(&mut self) {
        if self.egress_store_uncertain {
            return;
        }
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
            .filter(|(_, binding)| binding.phase == PolicyPhase::Owned)
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
            if let Err(error) = self.reprogram_cgroup_egress(binding.cgroup_id, None).await {
                self.handle_egress_rewrite_failure(binding.cgroup_id, error)
                    .await;
            }
        }
    }

    /// No-op without the eBPF data path.
    #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
    async fn reresolve_egress(&mut self) {}

    /// Reconcile kernel truth against live instances (the sweep half of the
    /// network-policy theme): scrub egress state whose cgroup no longer maps
    /// to a live instance, rewrite every live binding (idempotent repairs),
    /// while retaining unknown namespace keys. The one-second live check
    /// fences adopted policy-bearing workloads with no trustworthy binding;
    /// the sweep never installs their policy after they have already run.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    async fn sweep_kernel_networking(&mut self) {
        use crate::sesame::egress;

        if self.egress_store_uncertain
            || self.ebpf_sweep_interval_secs == 0
            || self.onion_ebpf.is_none()
        {
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
                    crate::grill::state::ContainerState::Pending
                        | crate::grill::state::ContainerState::Preparing
                        | crate::grill::state::ContainerState::Stopped
                        | crate::grill::state::ContainerState::Failed
                )
            })
            .filter(|i| {
                self.egress_bindings
                    .get(&i.id)
                    .is_none_or(|binding| binding.phase != PolicyPhase::Owned)
            })
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
        let expected: std::collections::HashSet<u64> = self
            .egress_bindings
            .values()
            .filter(|binding| binding.phase == PolicyPhase::Owned && !binding.allow.is_empty())
            .map(|b| b.cgroup_id)
            .collect();
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
            if let Err(error) = self.reprogram_cgroup_egress(cgroup_id, None).await {
                self.handle_egress_rewrite_failure(cgroup_id, error).await;
            }
        }

        // Unknown kernel keys are not proof of abandoned ownership. Retained
        // source owners authorise individual retirement; reconciliation retries
        // only the keys it already owns.
        self.sync_firewall_ebpf().await;
    }

    /// No-op without the eBPF data path.
    #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
    async fn sweep_kernel_networking(&mut self) {}

    /// Run any due health checks.
    async fn run_health_checks(&mut self) {
        let now = Instant::now();
        let mut due = Vec::new();
        while let Some(check) = self.supervisor.health_checker_mut().pop_due(now) {
            due.push(check);
        }
        for (instance_id, config) in due {
            let Some(instance) = self.supervisor.get_instance(&instance_id) else {
                continue;
            };
            if !matches!(
                instance.state,
                ContainerState::HealthWait | ContainerState::Running | ContainerState::Unhealthy
            ) || !self.health_inflight.insert(instance_id.clone())
            {
                self.supervisor
                    .health_checker_mut()
                    .schedule_next(instance_id, now);
                continue;
            }
            let host = probe_host(instance.container_ip);
            let created_at = instance.created_at;
            let results = self.deploy_ops_tx.clone();
            let shutdown = self.shutdown.clone();
            tokio::spawn(async move {
                let status = tokio::select! {
                    _ = shutdown.cancelled() => return,
                    status = probe_health(&config, &host) => status,
                };
                let result = DeployOp::HealthProbeResult {
                    instance_id,
                    created_at,
                    status,
                };
                tokio::select! {
                    _ = shutdown.cancelled() => {},
                    _ = results.send(result) => {},
                }
            });
        }
    }

    async fn complete_health_probe(
        &mut self,
        instance_id: InstanceId,
        created_at: Instant,
        status: Result<super::health::HealthStatus, super::probe::ProbeError>,
    ) {
        self.health_inflight.remove(&instance_id);
        let now = Instant::now();
        let Some(instance) = self.supervisor.get_instance(&instance_id) else {
            return;
        };
        // A newer registration owns the cadence of a replaced instance.
        if instance.created_at != created_at {
            return;
        }
        if !matches!(
            instance.state,
            ContainerState::HealthWait | ContainerState::Running | ContainerState::Unhealthy
        ) {
            // The instance left the probed states while this probe was in
            // flight (killed, restarting). Discard the result but keep its
            // cadence, as `run_health_checks` does for a skipped check: a
            // restart reuses this registration, so dropping it here would
            // leave the restarted instance in HealthWait with no probes.
            self.supervisor
                .health_checker_mut()
                .schedule_next(instance_id, now);
            return;
        }
        let status = match status {
            Ok(status) => status,
            Err(error) => {
                eprintln!("bun: {}: {error}", instance_id.0);
                self.supervisor
                    .health_checker_mut()
                    .schedule_next(instance_id, now);
                return;
            }
        };
        let transition = self.supervisor.process_health_result(&instance_id, status);

        if let Ok(Some(ContainerState::Unhealthy)) = transition
            && let Some(instance) = self.supervisor.get_instance(&instance_id)
        {
            self.record_event(
                crate::bun::events::EventKind::Health,
                crate::bun::events::EventSeverity::Warning,
                Some(instance.app_name.clone()),
                Some(instance.namespace.clone()),
                format!("instance {} became unhealthy", instance_id.0),
            )
            .await;
        }
        // Retry publication even when health state already changed on an earlier
        // probe. A refused withdrawal must not advance the restart state machine.
        if let Err(error) = self.publish_instance_health(&instance_id).await {
            eprintln!("bun: {error}");
            self.supervisor
                .health_checker_mut()
                .schedule_next(instance_id, now);
            return;
        }

        // A later probe can complete publication that the transition probe failed.
        if self
            .supervisor
            .get_instance(&instance_id)
            .is_some_and(|instance| instance.state == ContainerState::Unhealthy)
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

        self.supervisor
            .health_checker_mut()
            .schedule_next(instance_id, now);
    }

    /// Confirm health publication before routing changes or automatic restart.
    async fn publish_instance_health(&mut self, id: &InstanceId) -> Result<(), BunError> {
        let instance =
            self.supervisor
                .get_instance(id)
                .ok_or_else(|| BunError::InstanceNotFound {
                    instance_id: id.clone(),
                })?;
        if instance.host_port.is_none() {
            return Ok(());
        }
        let service =
            crate::onion::service_id::ServiceId::new(&instance.namespace, &instance.app_name);
        let healthy = instance.state == ContainerState::Running;
        // `service_map` changes only after a successful publication, so a
        // failed attempt still differs here and the next probe retries it.
        let published = self
            .service_map
            .resolve(&service)
            .and_then(|entry| {
                entry
                    .backends
                    .iter()
                    .find(|backend| backend.instance_id == id.0)
            })
            .is_some_and(|backend| backend.healthy == healthy);
        if published {
            return Ok(());
        }
        let mut candidate = self.service_map.clone();
        candidate
            .set_backend_health(&service, &id.0, healthy)
            .map_err(|error| BunError::BackendPublication {
                service: service.clone(),
                reason: error.to_string(),
            })?;
        self.publish_backend_snapshot(&service, &candidate).await?;
        self.service_map = candidate;
        self.rebuild_routing_table().await;
        Ok(())
    }

    /// Replace the schedule inventory only after its checkpoint is durable.
    async fn commit_scheduled_jobs(
        &mut self,
        next: std::collections::HashMap<(String, String), ScheduledJob>,
    ) -> Result<(), BunError> {
        if self.scheduled_jobs_store_uncertain {
            return Err(BunError::ScheduleState(
                "a previous write is uncertain; restart Bun to reload the checkpoint".into(),
            ));
        }
        if next == self.scheduled_jobs {
            return Ok(());
        }
        if let Some(directory) = self.records_dir.clone() {
            let records = next
                .values()
                .map(|job| super::schedules::RecordedSchedule {
                    name: job.name.clone(),
                    namespace: job.namespace.clone(),
                    spec: job.spec.clone(),
                    last_fired_minute: job.last_fired_minute,
                })
                .collect();
            // spawn_blocking can finish after its caller is cancelled. Fence
            // scheduling before the await until memory and disk agree again.
            self.scheduled_jobs_store_uncertain = true;
            // Keep both old and proposed owners reachable if writing fails or
            // is cancelled. Retirement must not mistake either set for absent.
            for (key, job) in &next {
                self.scheduled_jobs
                    .entry(key.clone())
                    .or_insert_with(|| job.clone());
            }
            tokio::task::spawn_blocking(move || super::schedules::persist(&directory, records))
                .await
                .map_err(|error| BunError::ScheduleState(error.to_string()))?
                .map_err(|error| BunError::ScheduleState(error.to_string()))?;
        }
        self.scheduled_jobs = next;
        self.scheduled_jobs_store_uncertain = false;
        Ok(())
    }

    /// Persist registrations and retire schedules removed by an explicit apply.
    async fn register_scheduled_jobs(&mut self, config: &Config) -> Result<(), BunError> {
        if config.job.is_empty() {
            return Ok(());
        }
        let mut next = self.scheduled_jobs.clone();
        for (name, spec) in &config.job {
            let namespace = spec
                .namespace
                .clone()
                .unwrap_or_else(|| "default".to_string());
            let key = (name.clone(), namespace.clone());
            let Some(expression) = spec.schedule.as_deref() else {
                next.remove(&key);
                continue;
            };
            let schedule = crate::meat::cron::CronSchedule::parse(expression)
                .map_err(|error| BunError::ScheduleState(error.to_string()))?;
            let last_fired_minute = next
                .get(&key)
                .and_then(|existing| existing.last_fired_minute);
            next.insert(
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
        self.commit_scheduled_jobs(next).await
    }

    /// Fire every scheduled job whose cron matches the current UTC minute.
    ///
    /// Called on the 1s event-loop tick, but a schedule only resolves to the
    /// minute, so each job fires at most once per matching minute (guarded by
    /// its epoch-minute stamp). Firing reuses the normal job deploy path with
    /// the `schedule` cleared, so the job actually runs this time.
    async fn fire_due_jobs(&mut self) {
        if self.scheduled_jobs.is_empty() || self.scheduled_jobs_store_uncertain {
            return;
        }
        let now = time::OffsetDateTime::now_utc();
        let minute_stamp = now.unix_timestamp().div_euclid(60);

        let mut due: Vec<(String, String, JobSpec)> = Vec::new();
        let mut next = self.scheduled_jobs.clone();
        let active = self.deploy_operations.snapshot().await.active_deploys;
        for job in next.values_mut() {
            if active.iter().any(|operation| {
                operation
                    .targets
                    .iter()
                    .any(|target| target.name == job.name && target.namespace == job.namespace)
            }) {
                continue;
            }
            if job
                .last_fired_minute
                .is_some_and(|previous| previous >= minute_stamp)
            {
                continue;
            }
            if job.schedule.matches(now) {
                job.last_fired_minute = Some(minute_stamp);
                let mut spec = job.spec.clone();
                spec.schedule = None;
                due.push((job.name.clone(), job.namespace.clone(), spec));
            }
        }

        if due.is_empty() {
            return;
        }
        if let Err(error) = self.commit_scheduled_jobs(next).await {
            eprintln!("cron: firing refused: {error}");
            return;
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
            self.spawn_scheduled_job_deploy(config).await;
        }
    }

    /// Admit a cron firing without changing the registered schedule.
    async fn spawn_scheduled_job_deploy(&mut self, config: Config) {
        let (events_tx, mut events_rx) = mpsc::channel::<ApplyEvent>(64);
        tokio::spawn(async move { while events_rx.recv().await.is_some() {} });
        self.begin_deploy(config, events_tx, false, false).await;
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
            .filter(|i| {
                i.is_job
                    && i.state == ContainerState::Running
                    && self
                        .recorded_jobs
                        .get(&i.id.0)
                        .is_some_and(|job| job.phase == super::jobs::JobPhase::Launching)
            })
            .map(|i| i.id.clone())
            .collect();

        for id in running_jobs {
            let grill_state = match self.supervisor.grill().state(&id).await {
                Ok(s) => s,
                Err(_) => continue,
            };

            if grill_state == ContainerState::Stopped {
                let exit_code = self.supervisor.grill().exit_code(&id).await;
                let phase = match exit_code {
                    Some(code) => super::jobs::JobPhase::Exited { code },
                    None => super::jobs::JobPhase::Unknown,
                };
                if let Err(error) = self.record_observed_job_exit(&id, phase).await {
                    eprintln!("bun: job outcome retained as uncertain for {id}: {error}");
                    continue;
                }

                // Transition Running → Stopping → Stopped
                if let Some(instance) = self.supervisor.get_instance_mut(&id) {
                    instance.retry_pending = exit_code.is_some_and(|code| code != 0);
                    if let Ok(s) = instance.state.transition_to(ContainerState::Stopping) {
                        instance.state = s;
                    }
                    if let Ok(s) = instance.state.transition_to(ContainerState::Stopped) {
                        instance.state = s;
                    }
                }

                if exit_code.is_none() {
                    self.record_event(
                        crate::bun::events::EventKind::JobFailed,
                        crate::bun::events::EventSeverity::Warning,
                        None,
                        None,
                        format!("job {id} outcome unknown; explicit rerun required"),
                    )
                    .await;
                    continue;
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
                            instance.retry_pending = false;
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
                instance.retry_pending = true;
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
        // Partial startup can have changed the runtime even when its call
        // failed. Keep ownership until cleanup is observed; then apply the
        // same budget and backoff as any other failed execution.
        let retrying: Vec<_> = self
            .supervisor
            .list_instances()
            .iter()
            .filter(|instance| {
                instance.retry_pending
                    && (!instance.is_job || !self.job_store_uncertain)
                    && matches!(
                        instance.state,
                        ContainerState::Stopping | ContainerState::Stopped
                    )
            })
            .map(|instance| (instance.id.clone(), instance.state))
            .collect();
        for (id, state) in retrying {
            if state == ContainerState::Stopping {
                match self.poll_instance_withdrawal(&id, self.stop_grace).await {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(error) => {
                        eprintln!(
                            "bun: failed restart of {id} awaits discovery withdrawal: {error}"
                        );
                        continue;
                    }
                }
                if let Err(error) = self.kill_and_wait_for_exit(&id).await {
                    eprintln!("bun: failed restart of {id} awaits runtime cleanup: {error}");
                    continue;
                }
                if let Some(instance) = self.supervisor.get_instance_mut(&id) {
                    let Ok(stopped) = instance.state.transition_to(ContainerState::Stopped) else {
                        continue;
                    };
                    instance.state = stopped;
                }
            }
            match self.supervisor.maybe_restart(&id, Instant::now()).await {
                Ok(true) => {
                    if let Some(instance) = self.supervisor.get_instance(&id) {
                        self.record_event(
                            crate::bun::events::EventKind::Restart,
                            crate::bun::events::EventSeverity::Warning,
                            Some(instance.app_name.clone()),
                            Some(instance.namespace.clone()),
                            format!(
                                "instance {id} restarted (attempt {})",
                                instance.restart_count
                            ),
                        )
                        .await;
                    }
                }
                Ok(false) => {}
                Err(BunError::RestartLimitExceeded { .. }) => {
                    if let Some(instance) = self.supervisor.get_instance_mut(&id)
                        && let Ok(failed) = instance.state.transition_to(ContainerState::Failed)
                    {
                        instance.state = failed;
                        instance.retry_pending = false;
                    }
                    if let Some(instance) = self.supervisor.get_instance(&id) {
                        self.record_event(
                            crate::bun::events::EventKind::JobFailed,
                            crate::bun::events::EventSeverity::Warning,
                            Some(instance.app_name.clone()),
                            Some(instance.namespace.clone()),
                            format!(
                                "workload {} exhausted its restart budget",
                                instance.app_name
                            ),
                        )
                        .await;
                    }
                }
                Err(error) => eprintln!("bun: cannot retry {id}: {error}"),
            }
        }
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
            .filter(|i| {
                i.state == ContainerState::Pending
                    && i.restart_count > 0
                    && (!i.is_job || !self.job_store_uncertain)
            })
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
            match self.poll_instance_withdrawal(&id, self.stop_grace).await {
                Ok(true) => {}
                Ok(false) => continue,
                Err(error) => {
                    eprintln!("bun: restart of {id} awaits discovery withdrawal: {error}");
                    continue;
                }
            }
            // Tear down the old container first. Without this, the same-id
            // create is rejected (ProcessGrill: stale-Running entry) or fails
            // (runc/apple: container still exists), leaving the instance wedged
            // in Preparing and the old process leaked.
            if let Err(error) = self.kill_and_wait_for_exit(&id).await {
                eprintln!("bun: restart of {id} awaits runtime cleanup: {error}");
                continue;
            }

            if self
                .supervisor
                .get_instance(&id)
                .is_some_and(|instance| instance.is_job)
            {
                if let Err(error) = self.record_job_runtime_absent(&id).await {
                    eprintln!("bun: job retry cannot persist runtime absence for {id}: {error}");
                    continue;
                }
                if let Err(error) = self.retire_instance_artifacts(&id).await {
                    eprintln!("bun: job retry retains artifacts for {id}: {error}");
                    continue;
                }
                if let Err(error) = self.claim_job_retry(&id).await {
                    eprintln!("bun: job retry refused for {id}: {error}");
                    continue;
                }
            } else if let Err(error) = self.retire_restart_artifacts(&id).await {
                eprintln!(
                    "bun: application restart retains predecessor artifacts for {id}: {error}"
                );
                continue;
            }

            // Pending → Preparing
            if let Some(instance) = self.supervisor.get_instance_mut(&id) {
                match instance.state.transition_to(ContainerState::Preparing) {
                    Ok(s) => instance.state = s,
                    Err(_) => continue,
                }
            }

            if let Err(error) = self.supervisor.grill().create(&id, &oci_spec).await {
                self.record_failed_restart(&id, &error.to_string()).await;
                continue;
            }

            // Close the restart window too: the recreated cgroup gets its
            // egress programmed before start (the crash gave the instance a
            // fresh cgroup id). The AppSpec comes from the stored deploy
            // record, the cgroup path from the stored OCI spec. On failure
            // the created container is removed and the restart refused —
            // fail closed, same as a fresh deploy.
            let restart_spec = self
                .deployed_specs
                .get(&(app_name.clone(), namespace.clone()))
                .cloned();
            let restart_egress = match oci_spec.linux.host_cgroup_path() {
                Some(cgroup_path) => {
                    self.apply_network_pre_start(
                        &id,
                        &app_name,
                        restart_spec.as_ref(),
                        &cgroup_path,
                    )
                    .await
                }
                None if self.supervisor.grill().honours_cgroup_path()
                    || restart_spec
                        .as_ref()
                        .and_then(|spec| spec.egress.as_ref())
                        .is_some_and(|policy| !policy.allow.is_empty()) =>
                {
                    Err(BunError::DeployFailed {
                        app_name: app_name.clone(),
                        reason: "restart has no original cgroup path for network preparation"
                            .into(),
                    })
                }
                None => Ok(()),
            };
            if let Err(e) = restart_egress {
                eprintln!("bun: restart of {} refused: {e}", id.0);
                if let Err(error) = self.supervisor.grill().stop(&id).await {
                    // The replacement is created but not stopped. Keep the
                    // cleanup owed instead of abandoning it as Failed.
                    self.record_failed_restart(
                        &id,
                        &format!("refused restart could not stop its created container: {error}"),
                    )
                    .await;
                    continue;
                }
                if let Some(instance) = self.supervisor.get_instance_mut(&id)
                    && let Ok(state) = instance.state.transition_to(ContainerState::Failed)
                {
                    instance.state = state;
                }
                continue;
            }

            // The durable job permit must precede every retry's start too.
            if let Err(error) = self
                .transition_deploy_state(&id, ContainerState::Starting)
                .await
            {
                self.record_failed_restart(&id, &error.to_string()).await;
                continue;
            }

            if let Err(error) = self.supervisor.grill().start(&id).await {
                self.record_failed_restart(&id, &error.to_string()).await;
                continue;
            }
            // Re-wire the restarted instance: stream its logs and keep it routable.
            self.spawn_log_forwarder(&id, &app_name, &namespace);
            if let Err(error) = self.persist_instance_record(&id).await {
                self.record_failed_restart(&id, &error.to_string()).await;
                continue;
            }
            // A re-created container may get a fresh IP; refresh it before
            // registering the backend so routing points at the live address.
            let container_ip = self.supervisor.grill().container_ip(&id).await;
            if let Some(instance) = self.supervisor.get_instance_mut(&id) {
                instance.container_ip = container_ip;
            }
            let service_id = crate::onion::service_id::ServiceId::new(&namespace, &app_name);
            let mut candidate = self.service_map.clone();
            if let Some(port) = host_port {
                let healthy = self
                    .supervisor
                    .get_instance(&id)
                    .is_some_and(|instance| instance.health_config.is_none());
                let backend = self.local_backend(&id, &service_id, container_ip, port, healthy);
                if let Err(error) = candidate.add_backend(&service_id, backend) {
                    self.record_failed_restart(&id, &error.to_string()).await;
                    continue;
                }
            }
            // Keep every reader on the confirmed view. A runtime restart can
            // change its address, but does not establish application health.
            if let Err(error) = self.publish_backend_snapshot(&service_id, &candidate).await {
                self.record_failed_restart(&id, &error.to_string()).await;
                continue;
            }
            self.service_map = candidate;
            self.sync_firewall_ebpf().await;
            self.rebuild_routing_table().await;

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

    /// Retain a partially created runtime for observed cleanup and bounded retry.
    async fn record_failed_restart(&mut self, id: &InstanceId, reason: &str) {
        if let Some(instance) = self.supervisor.get_instance_mut(id)
            && let Ok(stopping) = instance.state.transition_to(ContainerState::Stopping)
        {
            instance.state = stopping;
            instance.retry_pending = true;
        }
        if let Some(instance) = self.supervisor.get_instance(id) {
            self.record_event(
                crate::bun::events::EventKind::Restart,
                crate::bun::events::EventSeverity::Warning,
                Some(instance.app_name.clone()),
                Some(instance.namespace.clone()),
                format!("restart of {id} failed and awaits cleanup: {reason}"),
            )
            .await;
        }
    }

    /// Refuse user/cleanup stops while a deploy can still mutate the target.
    async fn refuse_while_deploying(
        &self,
        app_name: &str,
        namespace: &str,
    ) -> Result<(), BunError> {
        // Worker completion releases ownership only after its last runtime
        // mutation. Refuse before retiring a schedule or claiming a stop.
        // Both command admission and cron firing run on this same event loop.
        if let Some(operation) = self
            .deploy_operations
            .snapshot()
            .await
            .active_deploys
            .into_iter()
            .find(|operation| {
                operation
                    .targets
                    .iter()
                    .any(|target| target.name == app_name && target.namespace == namespace)
            })
        {
            return Err(BunError::WorkloadBusy {
                app_name: app_name.to_owned(),
                namespace: namespace.to_owned(),
                operation_id: operation.id,
            });
        }
        Ok(())
    }

    /// Retire a workload inline: the same steps a `Retire` command takes, for
    /// tests that drive the agent without running its loop.
    #[cfg(test)]
    async fn retire_workload(&mut self, app_name: &str, namespace: &str) -> Result<(), BunError> {
        self.refuse_while_deploying(app_name, namespace).await?;
        match self.stop_app(app_name, namespace).await {
            Ok(()) | Err(BunError::AppNotFound { .. }) => {}
            Err(error) => return Err(error),
        }
        self.release_retired_workload(app_name, namespace).await
    }

    /// Forget a workload's ownership once its stop has confirmed every exit.
    async fn release_retired_workload(
        &mut self,
        app_name: &str,
        namespace: &str,
    ) -> Result<(), BunError> {
        let instances: Vec<_> = self
            .supervisor
            .list_instances()
            .into_iter()
            .filter(|instance| instance.app_name == app_name && instance.namespace == namespace)
            .map(|instance| instance.id.clone())
            .collect();
        for id in instances {
            self.supervisor.retire_instance(&id).await;
        }
        self.deployed_specs
            .remove(&(app_name.to_string(), namespace.to_string()));
        let mut jobs = self.recorded_jobs.clone();
        jobs.retain(|_, job| job.name != app_name || job.namespace != namespace);
        if jobs.len() != self.recorded_jobs.len() {
            self.commit_jobs(jobs).await?;
        }
        Ok(())
    }

    /// Managed storage retirement only ever touches an owned test namespace.
    fn require_test_namespace(app_name: &str, namespace: &str) -> Result<(), BunError> {
        if crate::testkit::lease::valid_test_namespace(namespace) {
            return Ok(());
        }
        Err(BunError::RetirementState {
            instance_id: InstanceId(format!("{namespace}/{app_name}")),
            reason: "managed storage retirement requires an owned test namespace".into(),
        })
    }

    /// Remove a retired lease's disposable managed storage.
    async fn retire_test_storage(&self, app_name: &str, namespace: &str) -> Result<(), BunError> {
        let manager = crate::grill::volume::VolumeManager::new(self.volumes_dir.clone());
        let namespace = namespace.to_string();
        let app = app_name.to_string();
        tokio::task::spawn_blocking(move || manager.retire_test_storage(&namespace, &app))
            .await
            .map_err(|error| BunError::DeployFailed {
                app_name: app_name.into(),
                reason: error.to_string(),
            })?
            .map_err(|error| BunError::DeployFailed {
                app_name: app_name.into(),
                reason: error.to_string(),
            })
    }

    async fn prepare_storage(
        &self,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
    ) -> Result<(), BunError> {
        let manager = crate::grill::volume::VolumeManager::new(self.volumes_dir.clone());
        let namespace = namespace.to_string();
        let app = app_name.to_string();
        let spec = spec.clone();
        tokio::task::spawn_blocking(move || {
            if crate::testkit::lease::valid_test_namespace(&namespace) {
                manager.prepare_test_storage(&namespace, &app, &spec)?;
            } else {
                for volume in spec.volumes.iter().filter(|volume| volume.source.is_none()) {
                    manager.create_managed_volume(
                        &namespace,
                        &app,
                        &volume.path,
                        volume.size.as_deref(),
                    )?;
                }
            }
            Ok::<(), crate::grill::volume::VolumeError>(())
        })
        .await
        .map_err(|error| BunError::DeployFailed {
            app_name: app_name.into(),
            reason: error.to_string(),
        })?
        .map_err(|error| BunError::DeployFailed {
            app_name: app_name.into(),
            reason: error.to_string(),
        })
    }

    /// Stop an app's instances, waiting for their exit inline.
    ///
    /// Operator stops, retirements and the egress fence all await the exit
    /// off the command loop instead (`request_app_stop`,
    /// `stop_app_unattended`). This inline form lets tests drive a whole stop
    /// without running the loop.
    #[cfg(test)]
    async fn stop_app(&mut self, app_name: &str, namespace: &str) -> Result<(), BunError> {
        let stop = self.begin_app_stop(app_name, namespace).await?;
        self.app_exit_wait(&stop).await?;
        self.finish_app_stop(app_name, namespace, stop).await
    }

    /// Withdraw an app's routing and move its instances to Stopping.
    ///
    /// Nothing is signalled yet: `app_exit_wait` sends SIGTERM, waits out
    /// the grace and escalates, and `finish_app_stop` releases ownership only
    /// after that wait has confirmed every exit.
    async fn begin_app_stop(
        &mut self,
        app_name: &str,
        namespace: &str,
    ) -> Result<AppStop, BunError> {
        // A schedule exists before its first instance. Retire future firings
        // even when there is no running process (or runtime cleanup fails).
        let mut next = self.scheduled_jobs.clone();
        let had_schedule = next
            .remove(&(app_name.to_string(), namespace.to_string()))
            .is_some();
        if had_schedule {
            self.commit_scheduled_jobs(next).await?;
        }
        // Get instance IDs for this app
        let instances: Vec<InstanceId> = self
            .supervisor
            .list_instances()
            .iter()
            .filter(|i| i.app_name == app_name && i.namespace == namespace)
            .map(|i| i.id.clone())
            .collect();

        if instances.is_empty() && !had_schedule {
            return Err(BunError::AppNotFound {
                app_name: app_name.to_string(),
                namespace: namespace.to_string(),
            });
        }

        let owns_job = instances
            .iter()
            .any(|id| self.recorded_jobs.contains_key(&id.0));
        let mut jobs = self.recorded_jobs.clone();
        for id in &instances {
            if let Some(job) = jobs.get_mut(&id.0)
                && job.phase != super::jobs::JobPhase::Unknown
            {
                job.phase = super::jobs::JobPhase::Stopping;
            }
        }
        if owns_job {
            self.commit_jobs(jobs).await?;
        }

        // Runtime retirement can release a reusable container address. Refuse
        // before that happens if an old VIP can still route to the address.
        let service_id = crate::onion::service_id::ServiceId::new(namespace, app_name);
        self.withdraw_service_ebpf(&service_id).await?;
        for id in &instances {
            let _ = self.service_map.remove_backend(&service_id, &id.0);
        }
        self.rebuild_routing_table().await;

        // Stop via supervisor (moves the tracked state to Stopping).
        if !instances.is_empty() {
            self.supervisor.stop_app(app_name, namespace).await?;
        }

        Ok(AppStop {
            instances,
            owns_job,
        })
    }

    /// The exit wait for a begun stop, detached from `self` so it can run on
    /// a spawned task while the command loop keeps serving.
    ///
    /// DEP6: SIGTERM, wait for the runtime to confirm exit, escalate to
    /// SIGKILL on timeout. Only then may the caller record Stopped. Recording
    /// it before the process exits let container and supervisor state
    /// diverge — a "stopped" app whose process was still serving traffic.
    /// Every replica waits at once, so a stop costs one grace, not one each.
    fn app_exit_wait(
        &self,
        stop: &AppStop,
    ) -> impl std::future::Future<Output = Result<(), BunError>> + Send + 'static {
        let ids: Vec<InstanceId> = stop
            .instances
            .iter()
            .filter(|id| {
                !self
                    .recorded_jobs
                    .get(&id.0)
                    .is_some_and(|job| job.runtime_absent)
            })
            .cloned()
            .collect();
        let grill = self.supervisor.grill().clone();
        let drains = self.drains.clone();
        let grace = self.stop_grace;
        let confirmation_timeout = self.stop_confirmation_timeout;
        async move {
            let waits = ids.iter().map(|id| {
                drain_and_stop_instance(&drains, &grill, id, grace, confirmation_timeout)
            });
            // Try every replica, but report the first failure: ownership and
            // enforcement stay until all exits are confirmed, and a later stop
            // can retry the incomplete cleanup.
            futures_util::future::join_all(waits)
                .await
                .into_iter()
                .find_map(Result::err)
                .map_or(Ok(()), Err)
        }
    }

    /// Record a stop whose exits are confirmed and release what it owned.
    async fn finish_app_stop(
        &mut self,
        app_name: &str,
        namespace: &str,
        stop: AppStop,
    ) -> Result<(), BunError> {
        let AppStop {
            instances,
            owns_job,
        } = stop;
        let service_id = crate::onion::service_id::ServiceId::new(namespace, app_name);

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

        let mut jobs = self.recorded_jobs.clone();
        for id in &instances {
            if let Some(job) = jobs.get_mut(&id.0) {
                if job.phase != super::jobs::JobPhase::Unknown {
                    job.phase = super::jobs::JobPhase::Stopped;
                }
                job.runtime_absent = true;
            }
        }
        if owns_job {
            self.commit_jobs(jobs).await?;
        }

        // A failed artifact cleanup retains the empty service's key for retry.
        for id in &instances {
            self.retire_instance_artifacts(id).await?;
        }

        self.retire_discovery_service(&service_id).await?;
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

    /// Prepare every view before replacing any confirmed cluster publication.
    async fn publish_cluster_catalogue(
        &mut self,
        generation: u64,
        catalog: crate::onion::catalog::EndpointCatalog,
        ingress: Vec<crate::cluster::orchestrate::IngressAssignment>,
    ) -> Result<(), BunError> {
        if self.consumer_controls_views() {
            return Err(BunError::ClusterPublication(
                "durable consumer publication requires withdrawal instructions".into(),
            ));
        }
        if (generation == 0 && !catalog.is_empty())
            || self.cluster_catalog_generation.is_some_and(|confirmed| {
                generation < confirmed
                    || (generation == confirmed && catalog != self.cluster_catalog)
            })
        {
            return Err(BunError::ClusterPublication(
                "catalogue generation is stale or conflicts with confirmed publication".into(),
            ));
        }
        catalog
            .validate_allocations()
            .map_err(|error| BunError::ClusterPublication(error.to_string()))?;
        let cluster_ingress: std::collections::HashMap<_, _> = ingress
            .into_iter()
            .map(|route| ((route.namespace, route.name), route.config))
            .collect();
        let local_name = self
            .cluster
            .as_ref()
            .map(|cluster| cluster.local_node_id.0.as_str());
        let merged = self
            .service_map
            .with_cluster_catalog_excluding_node(&catalog, local_name);
        let entries: Vec<_> = merged.resolve_all().into_iter().cloned().collect();
        crate::onion::service_map::ServiceMap::from_snapshot(&entries)
            .map_err(|error| BunError::ClusterPublication(error.to_string()))?;
        if self.cluster_catalog == catalog && self.cluster_ingress_configs == cluster_ingress {
            // Even an identical catalogue can advance after an intermediate
            // publication. Delayed replies must not regress that confirmation.
            self.cluster_catalog_generation = Some(generation);
            return Ok(());
        }
        let mut ingress = self.ingress_configs.clone();
        ingress.extend(cluster_ingress.clone());
        let mut candidate = crate::wrapper::routing::RoutingTable::new();
        candidate
            .rebuild(&merged, &ingress)
            .map_err(|error| BunError::ClusterPublication(error.to_string()))?;

        // Readers may retain old request guards. This commits the new views,
        // but is not evidence that those older requests have drained.
        let mut table = self.routing_table.write().await;
        *table = candidate;
        self.cluster_catalog = catalog;
        self.cluster_catalog_generation = Some(generation);
        self.cluster_ingress_configs = cluster_ingress;
        self.service_map_tx.send_replace(merged);
        Ok(())
    }

    fn merged_service_map(&self) -> crate::onion::service_map::ServiceMap {
        if self.consumer_controls_views() {
            return self.service_map_tx.borrow().clone();
        }
        // Membership can lag or omit a non-voter. Local retirement must not
        // depend on the council having already learned this node's identity.
        let local_name = self
            .cluster
            .as_ref()
            .map(|cluster| cluster.local_node_id.0.as_str());
        self.service_map
            .with_cluster_catalog_excluding_node(&self.cluster_catalog, local_name)
    }

    /// Use the container port for direct netns traffic, and the published port
    /// when the runtime shares the host network.
    fn local_backend(
        &self,
        instance_id: &InstanceId,
        service: &crate::onion::service_id::ServiceId,
        container_ip: Option<std::net::Ipv4Addr>,
        host_port: u16,
        healthy: bool,
    ) -> crate::onion::types::BackendInstance {
        let port = if container_ip.is_some() {
            self.deployed_specs
                .get(&(service.name.clone(), service.namespace.clone()))
                .and_then(|spec| spec.port)
                .unwrap_or(host_port)
        } else {
            host_port
        };
        crate::onion::types::BackendInstance {
            instance_id: instance_id.0.clone(),
            node_ip: container_ip.unwrap_or(std::net::Ipv4Addr::LOCALHOST),
            host_port: port,
            healthy,
            local: true,
        }
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
        if self.consumer_controls_views() {
            return;
        }
        let merged = self.merged_service_map();

        let mut table = self.routing_table.write().await;
        // Invalid ingress configs (unsupported TLS mode, zero/overflow rate)
        // are rejected here: their routes are skipped rather than installed,
        // so a bad app can't serve TLS traffic in plaintext or divide by zero.
        let mut ingress = self.ingress_configs.clone();
        ingress.extend(self.cluster_ingress_configs.clone());
        if let Err(e) = table.rebuild(&merged, &ingress) {
            eprintln!("wrapper: ingress routing rebuild rejected some routes: {e}");
        }
        drop(table);

        // Retain the latest view even before the first DNS subscriber attaches.
        self.service_map_tx.send_replace(merged);
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
    /// process can read its owner-only key. Only when we're root and can
    /// actually chown: in rootless mode the files stay owned by the bun
    /// user, the same user namespace the workload runs in.
    ///
    /// Runc hands the directory to the container's (user-namespaced) host
    /// uid when it creates the container, so files follow the directory's
    /// owner. A directory still owned by root belongs to a runtime without
    /// that step, whose workloads run as nobody (65534).
    fn workload_identity_owner(dir: &std::path::Path) -> Option<(u32, u32)> {
        use std::os::unix::fs::MetadataExt;
        if !nix::unistd::geteuid().is_root() {
            return None;
        }
        match std::fs::metadata(dir) {
            Ok(metadata) if metadata.uid() != 0 => Some((metadata.uid(), metadata.gid())),
            _ => Some((65534, 65534)),
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

    /// Remove predecessor execution/policy evidence before an automatic restart.
    /// Runtime retirement must already be confirmed. The same logical workload
    /// keeps its identity bundle and mount; final retirement removes those too.
    async fn retire_restart_artifacts(&mut self, instance_id: &InstanceId) -> Result<(), BunError> {
        let remote = self.confirm_producer_release(instance_id).await?;
        self.clear_egress(instance_id).await?;
        self.release_network_reference(instance_id, remote.as_ref())
            .await?;
        if let Some(directory) = self.records_dir.clone() {
            let id = instance_id.0.clone();
            tokio::task::spawn_blocking(move || {
                crate::grill::records::remove_record(&directory, &id)
            })
            .await
            .map_err(|error| BunError::RetirementState {
                instance_id: instance_id.clone(),
                reason: error.to_string(),
            })?
            .map_err(|error| BunError::RetirementState {
                instance_id: instance_id.clone(),
                reason: error.to_string(),
            })?;
        }
        self.forget_retired_egress_owner(instance_id).await?;
        // Validate the mount source before a new runtime can consume it. The
        // preparation is idempotent and preserves this workload's credentials.
        self.prepare_instance_identity(instance_id)
    }

    async fn retire_initialisers(&mut self, parent: &InstanceId) -> Result<(), BunError> {
        let children = self.initialisers.get(parent).cloned().unwrap_or_default();
        for child in children {
            kill_runtime_instance(
                self.supervisor.grill(),
                &child,
                self.stop_confirmation_timeout,
            )
            .await?;
            if let Some(remaining) = self.initialisers.get_mut(parent) {
                remaining.remove(&child);
            }
        }
        self.initialisers.remove(parent);
        Ok(())
    }

    /// Retire durable artifacts before allowing the caller to forget an owner.
    async fn retire_instance_artifacts(
        &mut self,
        instance_id: &InstanceId,
    ) -> Result<(), BunError> {
        self.retire_initialisers(instance_id).await?;
        if !self
            .poll_instance_withdrawal(instance_id, std::time::Duration::ZERO)
            .await?
        {
            return Err(BunError::RetirementState {
                instance_id: instance_id.clone(),
                reason: "captured ingress requests still require confirmed release".into(),
            });
        }
        let remote = self.confirm_producer_release(instance_id).await?;
        self.clear_egress(instance_id).await?;
        self.release_network_reference(instance_id, remote.as_ref())
            .await?;
        let identity_dir = self.instance_identity_dir(instance_id);
        let records_dir = self.records_dir.clone();
        let id = instance_id.0.clone();
        let cleanup = tokio::task::spawn_blocking(move || {
            crate::sesame::identity::cleanup_identity_dir(&identity_dir)?;
            if let Some(directory) = records_dir {
                crate::grill::records::remove_record(&directory, &id)?;
            }
            Ok::<(), std::io::Error>(())
        })
        .await
        .map_err(|error| BunError::RetirementState {
            instance_id: instance_id.clone(),
            reason: error.to_string(),
        })?;
        cleanup.map_err(|error| BunError::RetirementState {
            instance_id: instance_id.clone(),
            reason: error.to_string(),
        })?;
        self.forget_retired_egress_owner(instance_id).await?;
        if let Some(instance) = self.supervisor.get_instance_mut(instance_id) {
            instance.identity = None;
            instance.identity_mount = None;
        }
        Ok(())
    }

    /// Remove identity directories that don't belong to any tracked
    /// instance. Runs once after adoption, so the key material of instances
    /// that died while bun was down never lingers (PKI7).
    async fn sweep_orphaned_identity_dirs(&self) {
        let root = self.volumes_dir.join(".identity");
        // Decide what to keep here, then leave the directory walk and file
        // removal to a blocking worker.
        let keep: std::collections::HashSet<String> = self
            .supervisor
            .list_instances()
            .iter()
            .map(|instance| instance.id.0.clone())
            .chain(
                self.startup_retirements
                    .iter()
                    .map(|pending| pending.instance_id.0.clone()),
            )
            .collect();
        let swept = tokio::task::spawn_blocking(move || {
            let Ok(entries) = std::fs::read_dir(&root) else {
                return;
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if keep.contains(&name) {
                    continue;
                }
                if let Err(e) = crate::sesame::identity::cleanup_identity_dir(&entry.path()) {
                    eprintln!("bun: warning: failed to sweep stale identity dir {name}: {e}");
                }
            }
        })
        .await;
        if let Err(error) = swept {
            eprintln!("bun: warning: identity sweep worker failed: {error}");
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

        // Only the leader can sign. A follower used to call its own council,
        // fail, and start the container with no identity at all.
        let result = if council.is_leader().await {
            council
                .sign_workload_csr(
                    &csr_der,
                    &spiffe_uri,
                    crate::sesame::identity::CertUsage::Mtls,
                    &self.trust_domain,
                    "local",
                    &instance_id.0,
                )
                .await
                .map(|signed| crate::cluster::workload_identity::SignedWorkload {
                    cert_der: signed.cert_der,
                    workload_ca_cert_der: signed.workload_ca_cert_der,
                    root_ca_cert_der: signed.root_ca_cert_der,
                    jwt_token: signed.jwt_token,
                })
                .map_err(|error| error.to_string())
        } else {
            match &self.workload_csr_client {
                Some(client) => client
                    .sign(&instance_id.0, workload_type, &csr_der)
                    .await
                    .map_err(|error| error.to_string()),
                None => Err("no leader transport for workload signing".to_string()),
            }
        };

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
                    Self::workload_identity_owner(&identity_dir),
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
                        message: format!("identity: CSR signing failed: {e}"),
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

        // Confirm the identity still has authority after consuming the token.
        let security_state = council
            .security_state_linearizable()
            .await
            .map_err(|error| BunError::SecurityError {
                reason: error.to_string(),
            })?;
        let join_result = crate::sesame::join::sign_join_csr(
            csr_der,
            node_id,
            serial,
            self.node_leaf_lifetime,
            &security_state,
            ikm,
        )
        .map_err(|e| BunError::SecurityError {
            reason: format!("join signing failed: {e}"),
        })?;

        Ok(crate::sesame::join::JoinBundle::from_result(&join_result))
    }

    /// Handle a SignImage command: verify an operator's detached signature
    /// and attach it to the manifest via Raft.
    ///
    /// The node never holds the signing key, so it can't mint trust: it only
    /// checks that the signature verifies under the public key it came with.
    /// Whether that key is trusted is decided at deploy time against
    /// `[images.trust_policy] keys`. The reply warns when this node's policy
    /// doesn't list the key, because deploys here would still refuse it.
    async fn handle_sign_image(
        &self,
        submission: crate::pickle::signing::SignatureSubmission,
    ) -> Result<String, BunError> {
        let council = self
            .cluster
            .as_ref()
            .and_then(|cluster| cluster.council.as_ref())
            .ok_or_else(|| BunError::SecurityError {
                reason: "image signatures live in the cluster catalogue; this node has no council"
                    .to_string(),
            })?;

        let public_key = submission.public_key.clone();
        let (digest, signature) =
            submission
                .into_verified()
                .map_err(|e| BunError::SecurityError {
                    reason: format!("signature rejected: {e}"),
                })?;
        let fingerprint = match &signature.method {
            crate::pickle::types::SigningMethod::ExternalKey { key_id } => key_id.clone(),
            crate::pickle::types::SigningMethod::Keyless { identity, .. } => identity.clone(),
        };

        let attach = crate::pickle::types::AttachSignature {
            manifest_digest: digest.clone(),
            signature,
        };
        let response = council
            .write(crate::council::RaftRequest::AttachSignature(attach))
            .await
            .map_err(|e| BunError::SecurityError {
                reason: format!("failed to attach signature: {e}"),
            })?;
        // An unknown digest comes back as a refusal, not an error; reporting
        // success there would claim a signature that attached to nothing.
        if let crate::council::types::CouncilResponse::Refused { reason } = response {
            return Err(BunError::SecurityError {
                reason: format!("signature attach refused: {reason}"),
            });
        }

        let mut message = format!("signed {} with key {fingerprint}", digest.as_str());
        if !self.trust_policy.keys.contains(&public_key) {
            message.push_str(
                "\nwarning: this node's [images.trust_policy] keys does not list this key, so deploys here will refuse the image until it does",
            );
        }
        Ok(message)
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
            // An instance still being created has neither, and asking would
            // hold the agent loop until its image pull finishes (Z6.7).
            let creating = instance.is_being_created();
            let pid = if creating {
                None
            } else {
                self.supervisor.grill().pid(&instance.id).await
            };
            let exit_code = match self.recorded_jobs.get(&instance.id.0).map(|job| &job.phase) {
                Some(super::jobs::JobPhase::Exited { code }) => Some(*code),
                Some(super::jobs::JobPhase::Unknown) => None,
                _ if creating => None,
                _ => self.supervisor.grill().exit_code(&instance.id).await,
            };
            statuses.push(InstanceStatus {
                id: instance.id.0.clone(),
                app_name: instance.app_name.clone(),
                namespace: instance.namespace.clone(),
                state: self.job_state_label(instance),
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
                state: self.job_state_label(instance),
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
        label: Option<&str>,
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

        let prefix = |id: &InstanceId| label.map(|node| format!("[{node} {}] ", id.0));

        // Send initial tail lines if requested
        if let Some(n) = tail {
            for id in &instance_ids {
                let logs = self.supervisor.grill().logs(id).await.unwrap_or_default();
                let tailed = tail_lines(&logs, n);
                let prefix = prefix(id).unwrap_or_default();
                for line in tailed.lines() {
                    if lines.send(format!("{prefix}{line}")).await.is_err() {
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
            // Each instance streams through its own channel; a labelled
            // follow stamps each line with its node and instance on the way.
            let prefix = prefix(&id).unwrap_or_default();
            let (instance_tx, mut instance_rx) =
                mpsc::channel::<crate::ketchup::types::CapturedLine>(64);
            tokio::spawn(async move {
                grill.follow_logs(&id, instance_tx).await;
            });
            let tx = lines.clone();
            tokio::spawn(async move {
                while let Some(captured) = instance_rx.recv().await {
                    if tx.send(format!("{prefix}{}", captured.line)).await.is_err() {
                        return;
                    }
                }
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
                reason: "path destination port must be between 1 and 65535".to_string(),
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
        let merged_services = self.merged_service_map();
        let service = internal_destination
            .then(|| merged_services.resolve(&service_id).cloned())
            .flatten();
        let destination_port = request
            .port
            .or_else(|| service.as_ref().map(|entry| entry.port))
            .ok_or_else(|| BunError::SecurityError {
                reason: "external path destination requires an explicit port".to_string(),
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
        let count = request.count.unwrap_or(1);
        if count == 0 || count > crate::onion::trace::MAX_TRACE_CONNECTS {
            return Err(BunError::SecurityError {
                reason: format!(
                    "path probe count must be between 1 and {}",
                    crate::onion::trace::MAX_TRACE_CONNECTS
                ),
            });
        }
        let faults = if internal_destination {
            self.path_faults(&request)
        } else {
            Vec::new()
        };
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
            faults,
            count,
            #[cfg(all(feature = "ebpf", target_os = "linux"))]
            onion_ebpf: self.onion_ebpf.clone(),
        })
    }

    /// The active network faults on this node that act on calls from the
    /// trace's source to its destination: faults on the destination (in its
    /// namespace) that either name this source or apply to every caller.
    fn path_faults(
        &self,
        request: &crate::onion::trace::TraceRequest,
    ) -> Vec<crate::onion::trace::PathFault> {
        use crate::onion::trace::{PathFault, PathFaultKind};
        use crate::smoker::types::FaultType;

        let mut faults: Vec<PathFault> = self
            .fault_registry
            .iter()
            .filter(|rule| rule.fault_type.acts_on_callers())
            .filter(|rule| {
                rule.target_service == request.destination
                    && rule.matches_namespace(&request.destination_namespace)
            })
            .filter(|rule| {
                crate::smoker::network::applies_to_caller(
                    rule,
                    &request.source,
                    &request.source_namespace,
                )
            })
            .map(|rule| PathFault {
                id: rule.id.0,
                kind: match rule.fault_type {
                    FaultType::Partition { .. } => PathFaultKind::Partition,
                    FaultType::Drop { probability } => PathFaultKind::Drop { probability },
                    FaultType::Delay { .. } => PathFaultKind::Delay,
                    FaultType::DnsNxdomain => PathFaultKind::DnsNxdomain,
                    _ => PathFaultKind::Other,
                },
                description: rule.fault_type.to_string(),
                remaining_secs: rule.remaining().as_secs(),
            })
            .collect();
        faults.sort_by_key(|fault| fault.id);
        faults
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
                std::time::Duration::from_secs(8),
            )
            .await;
        let dns_step = trace_dns_step(&self.dns_name, dns_probe, self.expected_vip.as_deref());

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
        let faults_step = crate::onion::trace::path_faults_step(
            4,
            &self.faults,
            self.trace_fault_evidence().await,
        );

        let connect_host = self
            .expected_vip
            .as_deref()
            .unwrap_or(self.request.destination.as_str());
        // One connect keeps the old three-second patience; a series waits
        // two seconds per connect so the whole trace stays inside the API's
        // deadline even when every connect hangs.
        let wait_secs = if self.count > 1 { 2 } else { 3 };
        let tcp_probe = self
            .run_workload_trace_probe(
                &self.source_instance,
                trace_tcp_command(connect_host, self.destination_port, self.count, wait_secs),
                "__RB_TRACE_TCP_STATUS__",
                std::time::Duration::from_secs(u64::from(self.count * (wait_secs + 1)) + 5),
            )
            .await;
        let tcp_step = crate::onion::trace::tcp_probe_step(
            5,
            &format!("{connect_host}:{}", self.destination_port),
            tcp_probe.clone(),
        );
        let connects = tcp_probe
            .ok()
            .and_then(|probe| crate::onion::trace::summarise_connects(&probe.attempts));
        let latency_ms = connects.as_ref().and_then(|summary| summary.median_ms);

        let steps = vec![dns_step, service_step, firewall_step, faults_step, tcp_step];
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
            connects,
        })
    }

    /// Live evidence for the faults on this path: the `fault_connect_map`
    /// entries the connect hook would find for this source (its own cgroup
    /// first, then every caller), and the netem delays on its interface.
    async fn trace_fault_evidence(&self) -> Vec<String> {
        if self.faults.is_empty() {
            return Vec::new();
        }
        #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
        let mut evidence = Vec::new();
        #[cfg(all(feature = "ebpf", target_os = "linux"))]
        if let (Some(handle), Some(service)) = (&self.onion_ebpf, &self.service) {
            let cgroup = self
                .grill
                .workload_cgroup(&self.source_instance)
                .await
                .ok()
                .flatten();
            let virtual_ip = service.vip.to_network_byte_order();
            let port = service.port.to_be();
            let mut ebpf = handle.lock().await;
            for (label, source_cgroup_id) in
                [("this source's cgroup", cgroup), ("every caller", Some(0))]
            {
                let Some(source_cgroup_id) = source_cgroup_id else {
                    continue;
                };
                let key = crate::smoker::bpf_types::partition_fault_key(
                    virtual_ip,
                    port,
                    source_cgroup_id,
                );
                match crate::smoker::bpf_maps::read_connect_fault(&mut ebpf.bpf, &key) {
                    Ok(Some(value)) => evidence.push(format!(
                        "live fault_connect_map entry for {label}: {}",
                        describe_connect_fault(&value)
                    )),
                    Ok(None) => {}
                    Err(error) => {
                        evidence.push(format!("fault_connect_map could not be read: {error}"))
                    }
                }
            }
        }
        #[cfg(target_os = "linux")]
        if self
            .faults
            .iter()
            .any(|fault| fault.kind == crate::onion::trace::PathFaultKind::Delay)
            && let Ok(shown) = crate::smoker::network::run_in_instance_netns(
                &self.source_instance.0,
                "tc",
                &crate::smoker::network::delay_show_args(),
            )
            .await
        {
            evidence.extend(
                crate::smoker::network::installed_delays(&shown)
                    .into_iter()
                    .map(|delay| format!("live netem on the source's eth0: {delay}")),
            );
        }
        evidence
    }

    async fn run_workload_trace_probe(
        &self,
        source_instance: &InstanceId,
        command: Vec<String>,
        marker: &str,
        timeout: std::time::Duration,
    ) -> Result<crate::onion::trace::ProbeOutput, String> {
        let future = self.grill.exec(source_instance, &command);
        let result = tokio::select! {
            _ = self.shutdown.cancelled() => {
                return Err("workload probe cancelled because the agent is shutting down".to_string());
            }
            result = tokio::time::timeout(timeout, future) => result,
        };
        match result {
            Ok(Ok(output)) => crate::onion::trace::parse_probe_output(&output, marker)
                .ok_or_else(|| "source image lacks a usable POSIX shell or probe tool".to_string()),
            Ok(Err(error)) => Err(format!("workload probe could not start: {error}")),
            Err(_) => Err(format!(
                "workload probe timed out after {} seconds",
                timeout.as_secs()
            )),
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
        details.extend(describe_backends(service));
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
                    details.extend(value.backends.iter().take(value.count.min(5) as usize).map(
                        |backend| {
                            format!(
                                "  kernel backend {}:{} ({})",
                                std::net::Ipv4Addr::from(u32::from_be(backend.host_ip)),
                                u16::from_be(backend.host_port),
                                if backend.healthy == 1 {
                                    "healthy"
                                } else {
                                    "unhealthy"
                                }
                            )
                        },
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
            let cgroup_id = match self.grill.workload_cgroup(source_instance).await {
                Ok(Some(cgroup_id)) => cgroup_id,
                Ok(None) => {
                    return unknown("runtime does not expose a verified workload cgroup".into());
                }
                Err(error) => {
                    return unknown(format!("source workload identity is unavailable: {error}"));
                }
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
    /// Withdraw local routing and poll request release without blocking the agent loop.
    async fn poll_instance_withdrawal(
        &mut self,
        id: &InstanceId,
        timeout: std::time::Duration,
    ) -> Result<bool, BunError> {
        self.withdraw_instance_backend(id).await?;
        Ok(self
            .drains
            .drain_all(&[crate::wrapper::draining::DrainCommand {
                app_name: String::new(),
                instance_id: id.0.clone(),
                timeout,
            }])
            .await)
    }

    /// Preserve ownership until both force-kill and observed runtime exit succeed.
    async fn kill_and_wait_for_exit(&self, id: &InstanceId) -> Result<(), BunError> {
        if self
            .recorded_jobs
            .get(&id.0)
            .is_some_and(|job| job.runtime_absent)
        {
            return Ok(());
        }
        kill_runtime_instance(self.supervisor.grill(), id, self.stop_confirmation_timeout).await
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
    ) -> Result<(), BunError> {
        if !has_port {
            return Ok(());
        }
        let Some(host_port) = host_port else {
            return Err(BunError::BackendPublication {
                service: crate::onion::service_id::ServiceId::new(namespace, app_name),
                reason: "replacement has no allocated port".into(),
            });
        };
        let service_id = crate::onion::service_id::ServiceId::new(namespace, app_name);
        let backend = self.local_backend(new_id, &service_id, container_ip, host_port, true);
        let mut candidate = self.service_map.clone();
        candidate
            .add_backend(&service_id, backend)
            .map_err(|error| BunError::BackendPublication {
                service: service_id.clone(),
                reason: error.to_string(),
            })?;
        self.publish_backend_snapshot(&service_id, &candidate)
            .await?;
        self.service_map = candidate;
        self.rebuild_routing_table().await;
        Ok(())
    }

    /// Confirm one backend's withdrawal before runtime cleanup can reuse its address.
    async fn withdraw_instance_backend(&mut self, id: &InstanceId) -> Result<(), BunError> {
        let Some(owner) = self.supervisor.get_instance(id) else {
            return Ok(());
        };
        let service = crate::onion::service_id::ServiceId::new(&owner.namespace, &owner.app_name);
        let Some(mut entry) = self.service_map.resolve(&service).cloned() else {
            return Ok(());
        };
        let had_backend = entry
            .backends
            .iter()
            .any(|backend| backend.instance_id == id.0);
        entry.backends.retain(|backend| backend.instance_id != id.0);
        if self.consumer_controls_views() {
            self.mark_consumer_view_stale()?;
            // A prior attempt may have removed the local backend before remote
            // consumers confirmed. Remote receipts, not this return, prove release.
            if had_backend {
                self.service_map
                    .remove_backend(&service, &id.0)
                    .map_err(|error| BunError::BackendRetirement {
                        service,
                        reason: error.to_string(),
                    })?;
            }
            return Ok(());
        }
        // Keep the original userspace owner on refusal. A retry must still know
        // the exact allocated key and the backend whose removal is outstanding.
        #[cfg(all(feature = "ebpf", target_os = "linux"))]
        if let Some(handle) = self.onion_ebpf.as_ref() {
            let mut ebpf = handle.lock().await;
            let map = crate::onion::ebpf::maps::BpfServiceMap::new();
            let failure =
                |error: crate::onion::ebpf::maps::BpfMapError| BunError::BackendRetirement {
                    service: service.clone(),
                    reason: error.to_string(),
                };
            // A missing userspace backend is not evidence that an earlier kernel
            // rewrite succeeded. Conversely, never recreate a confirmed absent key.
            if map
                .read_backends(&mut ebpf, entry.vip, entry.port)
                .map_err(failure)?
                .is_some()
            {
                map.update_backends_bpf(&mut ebpf, entry.vip, entry.port, &entry)
                    .map_err(failure)?;
            }
        }
        if had_backend {
            self.service_map
                .remove_backend(&service, &id.0)
                .map_err(|error| BunError::BackendRetirement {
                    service,
                    reason: error.to_string(),
                })?;
        }
        self.rebuild_routing_table().await;
        Ok(())
    }

    /// Withdraw traffic before fencing supervision and permitting an off-loop stop.
    async fn begin_instance_retirement(&mut self, id: &InstanceId) -> Result<(), BunError> {
        if self.supervisor.get_instance(id).is_none() {
            return Err(BunError::InstanceNotFound {
                instance_id: id.clone(),
            });
        }
        self.withdraw_instance_backend(id).await?;
        if let Some(instance) = self.supervisor.get_instance_mut(id) {
            instance.retry_pending = false;
            if instance.state.can_transition_to(ContainerState::Stopping) {
                instance.state = ContainerState::Stopping;
            }
        }
        self.supervisor.health_checker_mut().unregister(id);
        Ok(())
    }

    /// Drain, stop and forget one old instance (M7).
    ///
    /// The fast `&mut self` bookkeeping half of retiring one old instance: the
    /// worker has already drained and stopped it off the command loop (M7), so
    /// this only lifts egress, cleans identity, and drops the record and
    /// supervisor entry. Interleaving it with replacement is what gives
    /// `max_surge` and `max_unavailable` their meaning.
    async fn finish_retire_bookkeeping(&mut self, old_id: &InstanceId) -> Result<(), BunError> {
        // The worker already observed exit. Preserve a stopped cleanup owner,
        // so a filesystem failure cannot make the restart driver revive it.
        self.retain_stopped_instance(old_id);
        self.withdraw_instance_backend(old_id).await?;
        self.retire_instance_artifacts(old_id).await?;
        self.supervisor.retire_instance(old_id).await;
        self.sync_firewall_ebpf().await;
        self.rebuild_routing_table().await;
        Ok(())
    }

    /// Retain cleanup ownership after observed runtime exit without restarting it.
    fn retain_stopped_instance(&mut self, old_id: &InstanceId) {
        if let Some(instance) = self.supervisor.get_instance_mut(old_id) {
            instance.state = ContainerState::Stopped;
            instance.retry_pending = false;
        }
        self.supervisor.health_checker_mut().unregister(old_id);
    }

    /// Gracefully stop all instances.
    async fn shutdown_all(&mut self) {
        // Reverse every owned fault before the process goes away. The
        // node-pressure helper also has PR_SET_PDEATHSIG and startup sweeping
        // for crash recovery, but graceful shutdown should leave no helper or
        // cgroup behind in the first place.
        let faults = self.fault_registry.clear();
        for rule in &faults {
            self.reverse_fault(rule).await;
        }
        self.reconcile_network_faults().await;
        self.publish_dns_faults();

        let mut ids: Vec<InstanceId> = self
            .supervisor
            .list_instances()
            .iter()
            .map(|i| i.id.clone())
            .collect();

        ids.extend(
            self.initialisers
                .values()
                .flat_map(|children| children.iter().cloned()),
        );

        // Ask everything to stop (SIGTERM), wait (up to a grace period, but no
        // longer than needed) for it to exit, then force-kill (SIGKILL) whatever
        // is still running so nothing is orphaned.
        for id in &ids {
            let _ = self.supervisor.grill().stop(id).await;
        }
        let deadline = Instant::now() + self.shutdown_grace;
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
            DeployOp::HealthProbeResult {
                instance_id,
                created_at,
                status,
            } => {
                self.complete_health_probe(instance_id, created_at, status)
                    .await;
            }
            DeployOp::EnforceImageSignature { spec, reply } => {
                let _ = reply.send(self.enforce_image_signature(&spec).await);
            }
            DeployOp::StoreDeployedSpec {
                app_name,
                namespace,
                spec,
                reply,
            } => {
                let result = self.supervisor.admit_workload_kind(
                    &app_name,
                    &namespace,
                    crate::bun::deploy_operations::DeployTargetKind::App,
                );
                if result.is_ok() {
                    self.deployed_specs.insert((app_name, namespace), *spec);
                }
                let _ = reply.send(result);
            }
            DeployOp::ListExistingOwned {
                app_name,
                namespace,
                reply,
            } => {
                let ids = self
                    .supervisor
                    .list_instances()
                    .iter()
                    .filter(|i| !i.is_job && i.app_name == app_name && i.namespace == namespace)
                    // Retired by an earlier rollout; only its release remains.
                    .filter(|i| !self.deferred_retirements.contains(&i.id))
                    .map(|i| i.id.clone())
                    .collect();
                let _ = reply.send(ids);
            }
            DeployOp::NextDeployGen { app_name, reply } => {
                // Adoption restores owners, not the previous process's counter.
                // Use the structured app name to distinguish an ordinary app
                // named `worker-g9` from generation 9 of an app named `worker`.
                let highest_owned = self
                    .supervisor
                    .list_instances()
                    .iter()
                    .filter_map(|instance| {
                        let prefix = format!("{}__{}-g", instance.namespace, instance.app_name);
                        let suffix = instance.id.0.strip_prefix(&prefix)?;
                        let (generation, _) = suffix.split_once('-')?;
                        generation.parse::<u64>().ok()
                    })
                    .max()
                    .unwrap_or(0);
                let next = highest_owned
                    .checked_add(1)
                    .and_then(|after_owned| self.next_deploy_gen.max(after_owned).checked_add(1));
                let result = match next {
                    Some(next) => {
                        self.next_deploy_gen = next;
                        Ok(next - 1)
                    }
                    None => Err(BunError::DeployFailed {
                        app_name,
                        reason: "rollout generation exhausted; ownership preserved".into(),
                    }),
                };
                let _ = reply.send(result);
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
            DeployOp::ConfirmJobSuccess { instance_id, reply } => {
                let result = self
                    .record_observed_job_exit(
                        &instance_id,
                        super::jobs::JobPhase::Exited { code: 0 },
                    )
                    .await;
                if result.is_ok()
                    && let Some(instance) = self.supervisor.get_instance_mut(&instance_id)
                {
                    instance.retry_pending = false;
                    if instance.state.can_transition_to(ContainerState::Stopping) {
                        instance.state = ContainerState::Stopping;
                    }
                    if instance.state.can_transition_to(ContainerState::Stopped) {
                        instance.state = ContainerState::Stopped;
                    }
                }
                let _ = reply.send(result);
            }
            DeployOp::SupervisorDeployJob {
                rerun_unknown,
                job_name,
                namespace,
                spec,
                reply,
            } => {
                let result = self
                    .prepare_job_run(&job_name, &namespace, &spec, rerun_unknown)
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
                let result = async {
                    self.register_local_service(&service_id, port, firewall)?;
                    self.publish_backend_ebpf(&service_id).await?;
                    self.sync_firewall_ebpf().await;
                    Ok(())
                }
                .await;
                let _ = reply.send(result);
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
            DeployOp::RegisterInitialiser {
                instance_id,
                index,
                reply,
            } => {
                let result = match self.supervisor.get_instance(&instance_id) {
                    Some(instance) if instance.state == ContainerState::Initialising => {
                        // DNS workload labels cannot contain this auxiliary separator.
                        let initialiser = InstanceId(format!("{}__init-{index}", instance_id.0));
                        if self
                            .initialisers
                            .entry(instance_id.clone())
                            .or_default()
                            .insert(initialiser.clone())
                        {
                            Ok(initialiser)
                        } else {
                            Err(BunError::RetirementState {
                                instance_id,
                                reason: "initialiser still owns its previous runtime".into(),
                            })
                        }
                    }
                    _ => Err(BunError::InstanceNotFound { instance_id }),
                };
                let _ = reply.send(result);
            }
            DeployOp::ForgetInitialiser {
                instance_id,
                initialiser,
                reply,
            } => {
                let result = if self
                    .initialisers
                    .get_mut(&instance_id)
                    .is_some_and(|children| children.remove(&initialiser))
                {
                    if self
                        .initialisers
                        .get(&instance_id)
                        .is_some_and(|children| children.is_empty())
                    {
                        self.initialisers.remove(&instance_id);
                    }
                    Ok(())
                } else {
                    Err(BunError::RetirementState {
                        instance_id,
                        reason: "initialiser ownership changed before confirmation".into(),
                    })
                };
                let _ = reply.send(result);
            }
            DeployOp::ApplyNetworkPreStart {
                instance_id,
                app_name,
                spec,
                cgroup_path,
                reply,
            } => {
                let result = self
                    .apply_network_pre_start(&instance_id, &app_name, spec.as_deref(), &cgroup_path)
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
                let result = self.transition_deploy_state(&instance_id, to).await;
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
            DeployOp::ReserveRollingInstance {
                instance_id,
                app_name,
                namespace,
                spec,
                reply,
            } => {
                let result = self
                    .reserve_rolling_instance(&instance_id, &app_name, &namespace, &spec)
                    .await;
                let _ = reply.send(result);
            }
            DeployOp::PrepareRollingInstance {
                instance_id,
                app_name,
                namespace,
                spec,
                host_port,
                reply,
            } => {
                let result = self
                    .prepare_rolling_instance(&instance_id, &app_name, &namespace, &spec, host_port)
                    .await;
                let _ = reply.send(result);
            }
            DeployOp::RegisterRollingInstance { instance, reply } => {
                let result = self.persist_rolling_instance(&instance).await;
                if result.is_ok() {
                    self.spawn_log_forwarder(
                        &instance.instance_id,
                        &instance.app_name,
                        &instance.namespace,
                    );
                }
                let _ = reply.send(result);
            }
            DeployOp::RetainRollingInstance { instance, reply } => {
                let id = instance.instance_id.clone();
                let container_ip = self.supervisor.grill().container_ip(&id).await;
                let result = match self.supervisor.get_instance_mut(&id) {
                    Some(owner)
                        if owner.app_name == instance.app_name
                            && owner.namespace == instance.namespace =>
                    {
                        owner.state = ContainerState::Running;
                        owner.container_ip = container_ip;
                        owner.retry_pending = false;
                        owner.oci_spec = Some(instance.oci_spec);
                        let health_config =
                            instance.spec.health.as_ref().zip(instance.spec.port).map(
                                |(health, port)| {
                                    crate::bun::health::HealthCheckConfig::from_spec(health, port)
                                },
                            );
                        owner.health_config = health_config.clone();
                        if let Some(config) = health_config {
                            self.supervisor
                                .register_health(id.clone(), config, Instant::now());
                        }
                        Ok(())
                    }
                    _ => Err(BunError::InstanceNotFound { instance_id: id }),
                };
                let _ = reply.send(result);
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
                let result = self
                    .finalise_rolling_deploy(
                        &app_name, &namespace, &spec, &existing, &new_ids, &new_ports, &new_ips,
                        new_specs, now,
                    )
                    .await;
                let _ = reply.send(result);
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
                let result = self
                    .publish_new_backend(
                        &app_name,
                        &namespace,
                        &new_id,
                        host_port,
                        container_ip,
                        has_port,
                    )
                    .await;
                let _ = reply.send(result);
            }
            DeployOp::BeginRetire { old_id, reply } => {
                let result = self.begin_instance_retirement(&old_id).await;
                let _ = reply.send(result);
            }
            DeployOp::FinishRetire { old_id, reply } => {
                let result = self.finish_retire_bookkeeping(&old_id).await;
                let _ = reply.send(result);
            }
            DeployOp::DeferRetire { old_id, reply } => {
                self.defer_retirement(&old_id);
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
    rerun_unknown_jobs: bool,
    grill: G,
    ops: DeployOps,
    /// Shared drain tracker, so the worker can drain-and-stop a retiring
    /// instance off the command loop (M7) rather than sending the whole wait
    /// to the loop as an op.
    drains: crate::wrapper::draining::SharedDrains,
    operation: Option<crate::bun::deploy_operations::DeployOperationHandle>,
    /// The agent's `[runtime] stop_confirmation_timeout_secs`.
    stop_confirmation_timeout: std::time::Duration,
}

/// The last few hundred bytes of a runtime's captured stderr (`{stem}.stderr`),
/// on one line. `None` when nothing was captured or the file can't be read:
/// the caller still has the exit status to report.
async fn captured_stderr_tail(stem: &std::path::Path) -> Option<String> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};
    let mut file = tokio::fs::File::open(stem.with_extension("stderr"))
        .await
        .ok()?;
    let length = file.metadata().await.ok()?.len();
    file.seek(std::io::SeekFrom::Start(
        length.saturating_sub(INIT_FAILURE_STDERR_BYTES),
    ))
    .await
    .ok()?;
    let mut bytes = Vec::new();
    file.take(INIT_FAILURE_STDERR_BYTES)
        .read_to_end(&mut bytes)
        .await
        .ok()?;
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    (!lines.is_empty()).then(|| lines.join("; "))
}

impl<G: Grill + Clone + 'static> DeployWorker<G> {
    async fn report_cancellation(&self, events: &mpsc::Sender<ApplyEvent>) -> bool {
        if self
            .operation
            .as_ref()
            .is_some_and(|operation| operation.cancellation_requested())
        {
            let _ = events
                .send(ApplyEvent::Error {
                    message: "deploy cancellation requested; finishing owned cleanup".into(),
                })
                .await;
            return true;
        }
        false
    }

    async fn wait_for_deploy_health(
        &self,
        id: &InstanceId,
        spec: &AppSpec,
        container_ip: Option<std::net::Ipv4Addr>,
        wait: std::time::Duration,
    ) -> Result<(), String> {
        let health = wait_instance_healthy(&self.grill, id, spec, container_ip, wait);
        if let Some(operation) = &self.operation {
            tokio::select! {
                biased;
                _ = operation.cancelled() => Err("deploy cancellation requested; finishing owned cleanup".into()),
                result = health => result,
            }
        } else {
            health.await
        }
    }

    /// Deploy all apps and jobs from a config, streaming progress events. The
    /// mirror of the former `BunAgent::deploy`, but off the command loop.
    async fn run_deploy(self, config: Config, events: mpsc::Sender<ApplyEvent>) {
        if self.report_cancellation(&events).await {
            return;
        }
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
            if self.report_cancellation(&events).await {
                return;
            }
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
            if self.report_cancellation(&events).await {
                return;
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

            if let Err(error) = self
                .ops
                .store_deployed_spec(app_name, namespace, spec)
                .await
            {
                let _ = events
                    .send(ApplyEvent::Error {
                        message: error.to_string(),
                    })
                    .await;
                return;
            }

            let existing = self.ops.list_existing_owned(app_name, namespace).await;

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
                        .list_existing_owned(app_name, namespace)
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
                if let Err(error) = self
                    .ops
                    .register_service_app(app_name, namespace, port, firewall)
                    .await
                {
                    let _ = events
                        .send(ApplyEvent::Error {
                            message: error.to_string(),
                        })
                        .await;
                    return;
                }
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
            if self.report_cancellation(&events).await {
                return;
            }
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
                .supervisor_deploy_job(job_name, namespace, spec, self.rerun_unknown_jobs)
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

        if self.report_cancellation(&events).await {
            return;
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

    /// Run the same owned init chain for fresh and rolling replacements.
    async fn drive_initialisers(
        &self,
        instance_id: &InstanceId,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
        cgroup_path: &std::path::Path,
    ) -> Result<(), BunError> {
        if spec.init.is_empty() {
            return Ok(());
        }
        self.ops
            .transition_state(instance_id, ContainerState::Initialising)
            .await?;
        for (i, init_spec) in spec.init.iter().enumerate() {
            let init_id = self.ops.register_initialiser(instance_id, i).await?;
            let init_oci = crate::grill::oci::generate_init_oci_spec(
                &init_spec.command,
                namespace,
                app_name,
                spec.image.as_deref(),
                &cgroup_path.to_string_lossy(),
                None,
            );
            self.grill.create(&init_id, &init_oci).await?;
            self.grill.start(&init_id).await?;

            // Bounded wait: a hung init can't wedge the deploy forever (and
            // no longer wedges the loop at all — this poll is off it).
            let deadline =
                std::time::Instant::now() + std::time::Duration::from_secs(INIT_TIMEOUT_SECS);
            let failure = loop {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                let state = self.grill.state(&init_id).await?;
                if state == ContainerState::Stopped {
                    break match self.grill.exit_code(&init_id).await {
                        Some(0) => None,
                        Some(code) => Some(format!("exited with code {code}")),
                        None => Some("stopped without an exit code".to_string()),
                    };
                }
                if std::time::Instant::now() >= deadline {
                    let _ = self.grill.kill(&init_id).await;
                    break Some(format!("did not finish within {INIT_TIMEOUT_SECS}s"));
                }
            };

            if let Some(failure) = failure {
                let _ = self
                    .ops
                    .transition_state(instance_id, ContainerState::Failed)
                    .await;
                let reason = match self.grill.log_stem(&init_id).await {
                    Some(stem) => match captured_stderr_tail(&stem).await {
                        Some(stderr) => format!("{failure}: {stderr}"),
                        None => failure,
                    },
                    None => failure,
                };
                return Err(BunError::InitContainerFailed {
                    instance_id: instance_id.clone(),
                    init_index: i,
                    reason,
                });
            }
            kill_runtime_instance(&self.grill, &init_id, self.stop_confirmation_timeout).await?;
            self.ops.forget_initialiser(instance_id, &init_id).await?;
            // Runc can remove the shared cgroup when an init exits. Its
            // successor must receive policy for the new kernel identity
            // before either another init or the main workload executes.
            self.ops
                .apply_network_pre_start(instance_id, app_name, Some(spec), cgroup_path)
                .await?;
        }
        Ok(())
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
            .apply_network_pre_start(instance_id, app_name, Some(spec), &prepared.cgroup_path)
            .await?;

        if prepared.has_init {
            self.drive_initialisers(
                instance_id,
                app_name,
                namespace,
                spec,
                &prepared.cgroup_path,
            )
            .await?;
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

    /// Drive a job through create → source policy → start → Running.
    /// Jobs have no external allowlist or health checks.
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

        let cgroup_path =
            crate::grill::cgroup::instance_cgroup_path(namespace, job_name, instance_id)?;
        let cgroup_str = cgroup_path.to_string_lossy();
        let oci_spec = generate_job_oci_spec(job_name, namespace, spec, &cgroup_str, None);

        self.grill.create(instance_id, &oci_spec).await?;
        self.ops.store_oci_spec(instance_id, oci_spec.clone()).await;
        self.ops
            .apply_network_pre_start(instance_id, job_name, None, &cgroup_path)
            .await?;
        self.ops
            .transition_state(instance_id, ContainerState::Starting)
            .await?;
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
            .supervisor_deploy_job(job_name, namespace, spec, self.rerun_unknown_jobs)
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
                        self.ops.confirm_job_success(id).await?;
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

        let deploy_gen = match self.ops.next_deploy_gen(app_name).await {
            Ok(generation) => generation,
            Err(error) => {
                let _ = events
                    .send(ApplyEvent::Error {
                        message: error.to_string(),
                    })
                    .await;
                return std::ops::ControlFlow::Break(());
            }
        };
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
        let mut runtime_attempted = std::collections::HashSet::new();
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
            if self.report_cancellation(events).await {
                new_failed = true;
                break;
            }
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
                    // Drain and stop the old instance on this spawned deploy
                    // task (M7), then send only the fast bookkeeping to the
                    // command loop — the wait no longer stalls every command.
                    if let Err(error) = self
                        .retire_old_instance(&old_id, deploy_config.drain_timeout)
                        .await
                    {
                        let retention = self
                            .retain_started_replacements(
                                app_name, namespace, spec, &new_ids, &new_ports, &new_specs,
                            )
                            .await;
                        let _ = events
                            .send(ApplyEvent::Error {
                                message: format!(
                                    "old instance retirement unconfirmed: {error}; {}",
                                    match retention {
                                        Ok(()) =>
                                            "started replacements retained for cleanup".to_string(),
                                        Err(error) => format!(
                                            "could not retain replacement ownership: {error}"
                                        ),
                                    }
                                ),
                            })
                            .await;
                        return std::ops::ControlFlow::Break(());
                    }
                    match self.ops.finish_retire(&old_id).await {
                        // Stopped, drained and withdrawn locally; only other
                        // nodes' confirmations are outstanding. That can take
                        // as long as a lost node's view lease, and starting
                        // another generation wouldn't make it any shorter.
                        Err(BunError::ProducerReleasePending { .. }) => {
                            self.ops.defer_retire(&old_id).await;
                        }
                        Ok(()) => {}
                        Err(error) => {
                            let retention = self
                                .retain_started_replacements(
                                    app_name, namespace, spec, &new_ids, &new_ports, &new_specs,
                                )
                                .await;
                            let detail = match retention {
                                Ok(()) => "started replacements retained for cleanup".into(),
                                Err(error) => {
                                    format!("could not retain replacement ownership: {error}")
                                }
                            };
                            let _ = events
                                .send(ApplyEvent::Error {
                                    message: format!(
                                        "old instance artifact retirement failed: {error}; {detail}"
                                    ),
                                })
                                .await;
                            return std::ops::ControlFlow::Break(());
                        }
                    }
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

            let host_port = match self
                .ops
                .reserve_rolling_instance(&new_id, app_name, namespace, spec)
                .await
            {
                Ok(port) => port,
                Err(error) => {
                    let _ = events
                        .send(ApplyEvent::Error {
                            message: error.to_string(),
                        })
                        .await;
                    new_failed = true;
                    break;
                }
            };

            new_ports.insert(new_id.clone(), host_port);
            new_prepared.push(new_id.clone());
            let oci_spec = match self
                .ops
                .prepare_rolling_instance(&new_id, app_name, namespace, spec, host_port)
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
            let Some(cgroup_path) = oci_spec.linux.host_cgroup_path() else {
                let _ = events
                    .send(ApplyEvent::Error {
                        message: format!(
                            "replacement {} has no valid original cgroup path",
                            new_id.0
                        ),
                    })
                    .await;
                new_failed = true;
                break;
            };

            runtime_attempted.insert(new_id.clone());
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
                .apply_network_pre_start(&new_id, app_name, Some(spec), &cgroup_path)
                .await
            {
                let _ = events
                    .send(ApplyEvent::Error {
                        message: format!("failed to program egress for {}: {e}", new_id.0),
                    })
                    .await;
                new_failed = true;
                break;
            }
            let initialised = async {
                self.drive_initialisers(&new_id, app_name, namespace, spec, &cgroup_path)
                    .await?;
                self.ops
                    .transition_state(&new_id, ContainerState::Starting)
                    .await
            }
            .await;
            if let Err(error) = initialised {
                let _ = events
                    .send(ApplyEvent::Error {
                        message: format!("failed to initialise {}: {error}", new_id.0),
                    })
                    .await;
                new_failed = true;
                break;
            }
            if let Err(e) = self.grill.start(&new_id).await {
                let _ = events
                    .send(ApplyEvent::Error {
                        message: format!("failed to start {}: {e}", new_id.0),
                    })
                    .await;
                new_failed = true;
                break;
            }
            if let Err(error) = self
                .ops
                .register_rolling_instance(RollingInstance {
                    instance_id: new_id.clone(),
                    app_name: app_name.to_string(),
                    namespace: namespace.to_string(),
                    spec: spec.clone(),
                    oci_spec: oci_spec.clone(),
                    host_port,
                })
                .await
            {
                let _ = events
                    .send(ApplyEvent::Error {
                        message: error.to_string(),
                    })
                    .await;
                new_failed = true;
                break;
            }

            let container_ip = self.grill.container_ip(&new_id).await;

            // Health wait, off the command loop (this runs on the spawned
            // per-deploy task, so the full configured `health_timeout` is
            // honoured — M7). Waits for Running, then for the app's own HTTP
            // probe to pass (M5): a replacement is only announced healthy —
            // and only published as a backend below — once it answers the
            // health check the operator configured, not merely because its
            // process came up.
            let wait = effective_health_wait(&deploy_config);
            match self
                .wait_for_deploy_health(&new_id, spec, container_ip, wait)
                .await
            {
                Ok(()) => {
                    let _ = events
                        .send(ApplyEvent::Progress {
                            message: format!("{} healthy ✓", new_id.0),
                        })
                        .await;
                    self.ops
                        .provision_identity(app_name, namespace, &new_id, false)
                        .await;
                }
                Err(message) => {
                    let _ = events.send(ApplyEvent::Error { message }).await;
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
            if let Err(error) = self
                .ops
                .publish_new_backend(
                    app_name,
                    namespace,
                    &new_id,
                    host_port,
                    container_ip,
                    spec.port.is_some(),
                )
                .await
            {
                let _ = events
                    .send(ApplyEvent::Error {
                        message: error.to_string(),
                    })
                    .await;
                new_failed = true;
                break;
            }
            new_ids.push(new_id);
        }

        if new_failed {
            self.abort_rollout(
                app_name,
                namespace,
                spec,
                &new_ids,
                &new_prepared,
                &runtime_attempted,
                &new_ports,
                &new_specs,
                deploy_config.auto_rollback,
                retired,
                replica_count,
                events,
            )
            .await;
            return std::ops::ControlFlow::Break(());
        }

        // Anything the planner didn't reach (it stops once every replacement is
        // healthy, and a scale-down leaves surplus old instances) is retired
        // here. On a default rollout this is empty — the stop-progress lines
        // were already emitted per step above. The drain+stop wait runs on
        // this spawned task (M7); finalise only does the fast bookkeeping.
        let outstanding: Vec<InstanceId> = existing[retired..].to_vec();
        for old_id in &outstanding {
            let _ = events
                .send(ApplyEvent::Progress {
                    message: format!("stopping old instance {}", old_id.0),
                })
                .await;
            if let Err(error) = self
                .retire_old_instance(old_id, deploy_config.drain_timeout)
                .await
            {
                let retention = self
                    .retain_started_replacements(
                        app_name, namespace, spec, &new_ids, &new_ports, &new_specs,
                    )
                    .await;
                let _ = events
                    .send(ApplyEvent::Error {
                        message: format!(
                            "old instance retirement unconfirmed: {error}; {}",
                            match retention {
                                Ok(()) => "started replacements retained for cleanup".to_string(),
                                Err(error) =>
                                    format!("could not retain replacement ownership: {error}"),
                            }
                        ),
                    })
                    .await;
                return std::ops::ControlFlow::Break(());
            }
        }

        if let Err(error) = self
            .ops
            .finalise_rolling_deploy(
                app_name,
                namespace,
                spec,
                outstanding,
                new_ids.clone(),
                new_ports.clone(),
                new_ips,
                new_specs.clone(),
                now,
            )
            .await
        {
            let retention = self
                .retain_started_replacements(
                    app_name, namespace, spec, &new_ids, &new_ports, &new_specs,
                )
                .await;
            let detail = match retention {
                Ok(()) => "started replacements retained for cleanup".into(),
                Err(error) => format!("could not retain replacement ownership: {error}"),
            };
            let _ = events
                .send(ApplyEvent::Error {
                    message: format!("rollout finalisation failed: {error}; {detail}"),
                })
                .await;
            return std::ops::ControlFlow::Break(());
        }

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

    #[allow(clippy::too_many_arguments)]
    async fn abort_rollout(
        &self,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
        healthy: &[InstanceId],
        prepared: &[InstanceId],
        runtime_attempted: &std::collections::HashSet<InstanceId>,
        ports: &std::collections::HashMap<InstanceId, Option<u16>>,
        specs: &std::collections::HashMap<InstanceId, crate::grill::oci::OciSpec>,
        auto_rollback: bool,
        retired: usize,
        replica_count: u32,
        events: &mpsc::Sender<ApplyEvent>,
    ) {
        let mut errors = Vec::new();
        if !auto_rollback
            && let Err(error) = self
                .retain_started_replacements(app_name, namespace, spec, healthy, ports, specs)
                .await
        {
            errors.push(error.to_string());
        }
        for id in prepared {
            if !auto_rollback && healthy.contains(id) {
                continue;
            }
            let cleanup = async {
                self.ops.begin_retire(id).await?;
                // A failed create may already own runtime resources. Only a
                // reservation that never attempted create proves their absence.
                if runtime_attempted.contains(id) {
                    kill_runtime_instance(&self.grill, id, self.stop_confirmation_timeout).await?;
                }
                self.ops.finish_retire(id).await
            }
            .await;
            if let Err(error) = cleanup {
                errors.push(format!("{id}: {error}"));
            }
        }
        let (result, message) = if !errors.is_empty() {
            (
                crate::meat::deploy_types::DeployResult::Failed,
                format!(
                    "rollout cleanup incomplete; remaining owners retained: {}",
                    errors.join("; ")
                ),
            )
        } else if auto_rollback && retired == 0 {
            (
                crate::meat::deploy_types::DeployResult::RolledBack,
                "rolled back — old instances preserved".to_string(),
            )
        } else {
            (
                crate::meat::deploy_types::DeployResult::Halted,
                format!(
                    "deploy halted: {} healthy new instance(s) left running; {retired} old instance(s) already retired",
                    if auto_rollback { 0 } else { healthy.len() }
                ),
            )
        };
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
                result,
                created_at: SystemTime::now(),
                completed_at: SystemTime::now(),
                steps_completed: healthy.len(),
                steps_total: replica_count as usize,
                spec: Some(Box::new(spec.clone())),
            })
            .await;
        let _ = events.send(ApplyEvent::Error { message }).await;
    }

    /// Publish retirement intent before runtime exit can trigger the restart driver.
    async fn retire_old_instance(
        &self,
        id: &InstanceId,
        drain_timeout: std::time::Duration,
    ) -> Result<(), BunError> {
        self.ops.begin_retire(id).await?;
        drain_and_stop_instance(
            &self.drains,
            &self.grill,
            id,
            drain_timeout,
            self.stop_confirmation_timeout,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn retain_started_replacements(
        &self,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
        ids: &[InstanceId],
        ports: &std::collections::HashMap<InstanceId, Option<u16>>,
        specs: &std::collections::HashMap<InstanceId, crate::grill::oci::OciSpec>,
    ) -> Result<(), BunError> {
        for id in ids {
            let oci_spec = specs.get(id).ok_or_else(|| BunError::DeployFailed {
                app_name: app_name.into(),
                reason: format!("missing launch ownership for {id}"),
            })?;
            self.ops
                .retain_rolling_instance(RollingInstance {
                    instance_id: id.clone(),
                    app_name: app_name.into(),
                    namespace: namespace.into(),
                    spec: spec.clone(),
                    oci_spec: oci_spec.clone(),
                    host_port: ports.get(id).copied().flatten(),
                })
                .await?;
        }
        Ok(())
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
        let deploy_gen = match self.ops.next_deploy_gen(app_name).await {
            Ok(generation) => generation,
            Err(error) => {
                let _ = events
                    .send(ApplyEvent::Error {
                        message: error.to_string(),
                    })
                    .await;
                return std::ops::ControlFlow::Break(());
            }
        };
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
        let mut runtime_attempted = std::collections::HashSet::new();
        let mut new_failed = false;

        // Start and health check the entire green fleet before touching blue.
        // Unlike the rolling planner, nothing retires here and nothing is
        // published to routing yet: green comes up dark, alongside blue.
        for i in 0..replica_count {
            if self.report_cancellation(events).await {
                new_failed = true;
                break;
            }
            let new_id = crate::grill::InstanceIdentity::canary(namespace, app_name, deploy_gen, i)
                .instance_id();
            let _ = events
                .send(ApplyEvent::Progress {
                    message: format!("starting green instance {}", new_id.0),
                })
                .await;

            let host_port = match self
                .ops
                .reserve_rolling_instance(&new_id, app_name, namespace, spec)
                .await
            {
                Ok(port) => port,
                Err(error) => {
                    let _ = events
                        .send(ApplyEvent::Error {
                            message: error.to_string(),
                        })
                        .await;
                    new_failed = true;
                    break;
                }
            };

            new_ports.insert(new_id.clone(), host_port);
            new_prepared.push(new_id.clone());
            let oci_spec = match self
                .ops
                .prepare_rolling_instance(&new_id, app_name, namespace, spec, host_port)
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
            let Some(cgroup_path) = oci_spec.linux.host_cgroup_path() else {
                let _ = events
                    .send(ApplyEvent::Error {
                        message: format!(
                            "replacement {} has no valid original cgroup path",
                            new_id.0
                        ),
                    })
                    .await;
                new_failed = true;
                break;
            };

            runtime_attempted.insert(new_id.clone());
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
                .apply_network_pre_start(&new_id, app_name, Some(spec), &cgroup_path)
                .await
            {
                let _ = events
                    .send(ApplyEvent::Error {
                        message: format!("failed to program egress for {}: {e}", new_id.0),
                    })
                    .await;
                new_failed = true;
                break;
            }
            if let Err(e) = self.grill.start(&new_id).await {
                let _ = events
                    .send(ApplyEvent::Error {
                        message: format!("failed to start {}: {e}", new_id.0),
                    })
                    .await;
                new_failed = true;
                break;
            }
            if let Err(error) = self
                .ops
                .register_rolling_instance(RollingInstance {
                    instance_id: new_id.clone(),
                    app_name: app_name.to_string(),
                    namespace: namespace.to_string(),
                    spec: spec.clone(),
                    oci_spec: oci_spec.clone(),
                    host_port,
                })
                .await
            {
                let _ = events
                    .send(ApplyEvent::Error {
                        message: error.to_string(),
                    })
                    .await;
                new_failed = true;
                break;
            }

            let container_ip = self.grill.container_ip(&new_id).await;

            // Same M5 gate as the rolling path: green only counts as healthy
            // once its configured HTTP probe passes, not merely on Running —
            // otherwise a green fleet that starts but can't serve replaces a
            // blue fleet that can.
            let wait = effective_health_wait(&deploy_config);
            match self
                .wait_for_deploy_health(&new_id, spec, container_ip, wait)
                .await
            {
                Ok(()) => {
                    let _ = events
                        .send(ApplyEvent::Progress {
                            message: format!("{} healthy ✓", new_id.0),
                        })
                        .await;
                    self.ops
                        .provision_identity(app_name, namespace, &new_id, false)
                        .await;
                }
                Err(message) => {
                    let _ = events.send(ApplyEvent::Error { message }).await;
                    new_failed = true;
                    break;
                }
            }

            new_ports.insert(new_id.clone(), host_port);
            new_specs.insert(new_id.clone(), oci_spec);
            new_ips.insert(new_id.clone(), container_ip);
            new_ids.push(new_id);
        }

        if !new_failed && self.report_cancellation(events).await {
            new_failed = true;
        }
        if new_failed {
            self.abort_rollout(
                app_name,
                namespace,
                spec,
                &new_ids,
                &new_prepared,
                &runtime_attempted,
                &new_ports,
                &new_specs,
                deploy_config.auto_rollback,
                0,
                replica_count,
                events,
            )
            .await;
            return std::ops::ControlFlow::Break(());
        }

        // The whole green fleet is healthy. Cut over: publish every green
        // backend so routing picks them up while blue still serves, then
        // drain and stop blue on this spawned task (M7 — the bulk drain used
        // to run inside finalise on the command loop, freezing every agent
        // command for up to fleet-size × drain_timeout), and finally send the
        // fast bookkeeping to the loop.
        for new_id in &new_ids {
            if let Err(error) = self
                .ops
                .publish_new_backend(
                    app_name,
                    namespace,
                    new_id,
                    new_ports.get(new_id).copied().flatten(),
                    new_ips.get(new_id).copied().flatten(),
                    spec.port.is_some(),
                )
                .await
            {
                let _ = events
                    .send(ApplyEvent::Error {
                        message: error.to_string(),
                    })
                    .await;
                self.abort_rollout(
                    app_name,
                    namespace,
                    spec,
                    &new_ids,
                    &new_prepared,
                    &runtime_attempted,
                    &new_ports,
                    &new_specs,
                    deploy_config.auto_rollback,
                    0,
                    replica_count,
                    events,
                )
                .await;
                return std::ops::ControlFlow::Break(());
            }
        }
        for old_id in &existing {
            if let Err(error) = self
                .retire_old_instance(old_id, deploy_config.drain_timeout)
                .await
            {
                let retention = self
                    .retain_started_replacements(
                        app_name, namespace, spec, &new_ids, &new_ports, &new_specs,
                    )
                    .await;
                let _ = events
                    .send(ApplyEvent::Error {
                        message: format!(
                            "old instance retirement unconfirmed: {error}; {}",
                            match retention {
                                Ok(()) => "started replacements retained for cleanup".to_string(),
                                Err(error) =>
                                    format!("could not retain replacement ownership: {error}"),
                            }
                        ),
                    })
                    .await;
                return std::ops::ControlFlow::Break(());
            }
        }
        if let Err(error) = self
            .ops
            .finalise_rolling_deploy(
                app_name,
                namespace,
                spec,
                existing,
                new_ids.clone(),
                new_ports.clone(),
                new_ips,
                new_specs.clone(),
                now,
            )
            .await
        {
            let retention = self
                .retain_started_replacements(
                    app_name, namespace, spec, &new_ids, &new_ports, &new_specs,
                )
                .await;
            let detail = match retention {
                Ok(()) => "started replacements retained for cleanup".into(),
                Err(error) => format!("could not retain replacement ownership: {error}"),
            };
            let _ = events
                .send(ApplyEvent::Error {
                    message: format!("rollout finalisation failed: {error}; {detail}"),
                })
                .await;
            return std::ops::ControlFlow::Break(());
        }

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

/// The network-byte-order VIP and port of a fault's target service, if this
/// node knows it. Resolved against the exact namespace-qualified identity, so
/// a fault on `web` in `team-a` never picks up `team-b`'s `web` VIP.
#[cfg(all(feature = "ebpf", target_os = "linux"))]
fn fault_vip_port(
    services: &crate::onion::service_map::ServiceMap,
    rule: &crate::smoker::types::FaultRule,
) -> Option<(u32, u16)> {
    let entry = services.resolve(&crate::onion::service_id::ServiceId::new(
        rule.namespace.as_deref()?,
        rule.target_service.as_str(),
    ))?;
    Some((entry.vip.to_network_byte_order(), entry.port.to_be()))
}

/// The post-rewrite backend addresses of a fault's target service, as this
/// node's merged service map knows them.
#[cfg(target_os = "linux")]
fn fault_backend_addresses(
    services: &crate::onion::service_map::ServiceMap,
    rule: &crate::smoker::types::FaultRule,
) -> Vec<std::net::SocketAddrV4> {
    let Some(namespace) = rule.namespace.as_deref() else {
        return Vec::new();
    };
    services
        .resolve(&crate::onion::service_id::ServiceId::new(
            namespace,
            rule.target_service.as_str(),
        ))
        .map(|entry| {
            entry
                .backends
                .iter()
                .map(|backend| std::net::SocketAddrV4::new(backend.node_ip, backend.host_port))
                .collect()
        })
        .unwrap_or_default()
}

/// Remove Smoker's delay tree from an instance's interface if it has one,
/// restoring the default qdisc. Returns whether there was one.
#[cfg(target_os = "linux")]
async fn remove_delay_tree(
    instance: &str,
) -> Result<bool, crate::smoker::network::NetnsCommandError> {
    use crate::smoker::network::{
        delay_remove_args, delay_show_args, has_delay_root, run_in_instance_netns,
    };
    let shown = run_in_instance_netns(instance, "tc", &delay_show_args()).await?;
    if !has_delay_root(&shown) {
        return Ok(false);
    }
    run_in_instance_netns(instance, "tc", &delay_remove_args()).await?;
    Ok(true)
}

/// Replace an instance's delay tree with `bands` (none: just remove it).
///
/// Rebuilding the whole tree keeps this simple and idempotent: a qdisc that
/// someone else added at the root makes the `add` fail rather than be
/// overwritten, and a failure half-way takes our partial tree back out.
#[cfg(target_os = "linux")]
async fn program_delay_tree(
    instance: &str,
    bands: &[crate::smoker::network::DelayBand],
) -> Result<(), crate::smoker::network::NetnsCommandError> {
    use crate::smoker::network::{delay_install_args, run_in_instance_netns};
    remove_delay_tree(instance).await?;
    if bands.is_empty() {
        return Ok(());
    }
    for args in delay_install_args(bands) {
        if let Err(error) = run_in_instance_netns(instance, "tc", &args).await {
            let _ = remove_delay_tree(instance).await;
            return Err(error);
        }
    }
    Ok(())
}

/// Say what to do when the kernel has no netem, rather than echo tc.
#[cfg(target_os = "linux")]
fn delay_error_hint(error: &crate::smoker::network::NetnsCommandError) -> String {
    let text = error.to_string();
    if text.contains("netem") && (text.contains("Unknown") || text.contains("not found")) {
        format!(
            "{text} (the kernel has no sch_netem module; install the linux-modules package for this kernel)"
        )
    } else {
        text
    }
}

/// The post-rewrite backend addresses behind a service's (virtual IP, port),
/// both in network byte order: what a caller's sockets are connected to once
/// the connect hook has picked a backend.
#[cfg(all(feature = "ebpf", target_os = "linux"))]
fn backend_addresses(
    services: &crate::onion::service_map::ServiceMap,
    virtual_ip: u32,
    port: u16,
) -> Vec<std::net::SocketAddrV4> {
    services
        .resolve_all()
        .into_iter()
        .filter(|entry| {
            entry.vip.to_network_byte_order() == virtual_ip && entry.port.to_be() == port
        })
        .flat_map(|entry| entry.backends.iter())
        .map(|backend| std::net::SocketAddrV4::new(backend.node_ip, backend.host_port))
        .collect()
}

const DNS_TRACE_SCRIPT: &str = r#"
output=$(nslookup "$1" 2>&1)
status=$?
printf '%s\n' "$output"
printf '__RB_TRACE_DNS_STATUS__=%s\n' "$status"
"#;

// Each connect is timed inside the container, so the figure excludes the
// cost of exec'ing the probe. `date +%s%N` gives nanoseconds where the image's
// `date` supports `%N`; BusyBox often doesn't, so `/proc/uptime` (10 ms) is
// read too, and the parser uses whichever is plausible. nc's own chatter (the
// OpenBSD "Connection ... succeeded!" line) is dropped on success.
const TCP_TRACE_SCRIPT: &str = r#"
count=$3
i=0
status=1
while [ "$i" -lt "$count" ]; do
  up_start=
  up_end=
  read -r up_start _ < /proc/uptime 2>/dev/null
  start=$(date +%s%N 2>/dev/null)
  output=$(nc -z -w "$4" "$1" "$2" 2>&1)
  status=$?
  end=$(date +%s%N 2>/dev/null)
  read -r up_end _ < /proc/uptime 2>/dev/null
  [ "$status" -ne 0 ] && [ -n "$output" ] && printf '%s\n' "$output"
  printf '__RB_TRACE_TCP_ATTEMPT__=%s %s %s %s %s\n' "$status" "$start" "$end" "$up_start" "$up_end"
  i=$((i + 1))
done
printf '__RB_TRACE_TCP_STATUS__=%s\n' "$status"
"#;

fn trace_dns_command(name: &str) -> Vec<String> {
    vec![
        "sh".to_string(),
        "-c".to_string(),
        DNS_TRACE_SCRIPT.to_string(),
        "reliaburger-path".to_string(),
        name.to_string(),
    ]
}

fn trace_tcp_command(host: &str, port: u16, count: u32, wait_secs: u32) -> Vec<String> {
    vec![
        "sh".to_string(),
        "-c".to_string(),
        TCP_TRACE_SCRIPT.to_string(),
        "reliaburger-path".to_string(),
        host.to_string(),
        port.to_string(),
        count.to_string(),
        wait_secs.to_string(),
    ]
}

/// Name a service's backends, and which one the VIP picks, for the trace.
fn describe_backends(service: &crate::onion::types::ServiceEntry) -> Vec<String> {
    let mut details: Vec<String> = service
        .backends
        .iter()
        .take(5)
        .map(|backend| {
            format!(
                "  backend {} at {}:{} ({})",
                backend.instance_id,
                backend.node_ip,
                backend.host_port,
                if backend.healthy {
                    "healthy"
                } else {
                    "unhealthy"
                }
            )
        })
        .collect();
    let healthy: Vec<_> = service
        .backends
        .iter()
        .filter(|backend| backend.healthy)
        .collect();
    match healthy.as_slice() {
        [] => {}
        [only] => details.push(format!(
            "the VIP sends every connect to {} at {}:{}",
            only.instance_id, only.node_ip, only.host_port
        )),
        several => details.push(format!(
            "the VIP spreads connects round-robin over {} healthy backends",
            several.len()
        )),
    }
    details
}

/// Describe a live `fault_connect_map` value.
#[cfg(all(feature = "ebpf", target_os = "linux"))]
fn describe_connect_fault(value: &crate::smoker::bpf_types::BpfConnectFaultValue) -> String {
    let action = match value.action {
        crate::smoker::bpf_types::FAULT_ACTION_PARTITION => "partition".to_string(),
        crate::smoker::bpf_types::FAULT_ACTION_DROP => format!("drop {}%", value.probability),
        other => format!("action {other}"),
    };
    let now = crate::smoker::types::monotonic_now_ns();
    let left = value.expires_ns.saturating_sub(now) / 1_000_000_000;
    format!("{action}, expires in {left}s")
}

fn trace_dns_step(
    name: &str,
    probe: Result<crate::onion::trace::ProbeOutput, String>,
    expected_value: Option<&str>,
) -> crate::onion::trace::TraceStep {
    use crate::onion::trace::{TraceEvidence, TraceStep, TraceVerdict};
    let step_name = "DNS query".to_string();
    match probe {
        Ok(probe) => {
            let expected_answer = expected_value.is_none_or(|expected| {
                expected
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|address| probe.dns_answers().contains(&address))
            });
            let details = crate::onion::trace::dns_details(name, &probe);
            let verdict = if probe.status == 0 {
                if let Some(expected) = expected_value
                    && !expected_answer
                {
                    TraceVerdict::Fail {
                        reason: format!(
                            "probe succeeded but its DNS answers did not include exact address {expected}"
                        ),
                    }
                } else {
                    TraceVerdict::Pass
                }
            } else if probe.status == 126 || probe.status == 127 {
                TraceVerdict::Unknown {
                    reason: "source image does not provide the fixed DNS query probe tool"
                        .to_string(),
                }
            } else {
                TraceVerdict::Fail {
                    reason: format!("DNS query exited with status {}", probe.status),
                }
            };
            TraceStep {
                step_number: 1,
                name: step_name,
                evidence: TraceEvidence::Observed,
                details,
                verdict,
            }
        }
        Err(reason) => TraceStep {
            step_number: 1,
            name: step_name,
            evidence: TraceEvidence::Unavailable,
            details: vec![format!("query {name}")],
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
        let tcp = trace_tcp_command(hostile, 443, 3, 2);
        assert!(!dns[2].contains(hostile));
        assert_eq!(dns[4], hostile);
        assert!(!tcp[2].contains(hostile));
        assert_eq!(tcp[4], hostile);
        assert_eq!(tcp[5], "443");
        assert_eq!(tcp[6], "3");
    }

    #[test]
    fn missing_workload_probe_tool_is_unknown_not_a_network_failure() {
        let step = trace_dns_step(
            "api.internal",
            Ok(crate::onion::trace::ProbeOutput {
                status: 127,
                lines: vec!["nslookup: not found".to_string()],
                attempts: Vec::new(),
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
            "__RB_TRACE_TCP_ATTEMPT__=0 1727000000000000000 1727000000002500000\n__RB_TRACE_TCP_STATUS__=0\n"
                .to_string(),
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
                count: None,
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

        assert_eq!(result.steps.len(), 5);
        assert_eq!(
            result.steps[0].verdict,
            crate::onion::trace::TraceVerdict::Pass
        );
        assert_eq!(
            result.steps[1].evidence,
            crate::onion::trace::TraceEvidence::Inferred
        );
        assert_eq!(
            result.steps[4].verdict,
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
        assert_eq!(result.latency_ms, Some(2.5));

        shutdown.cancel();
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn a_trace_lists_only_the_faults_that_act_on_its_own_path() {
        use crate::smoker::types::{FaultRequest, FaultType};
        let (mut agent, _tx, _shutdown) = test_agent();
        let mut inject = |fault_type: FaultType, service: &str, namespace: &str| {
            agent
                .fault_registry
                .insert(&FaultRequest {
                    fault_type,
                    target_service: service.to_string(),
                    namespace: Some(namespace.to_string()),
                    target_instance: None,
                    target_node: None,
                    duration: std::time::Duration::from_secs(60),
                    injected_by: "test".to_string(),
                    reason: None,
                    include_leader: false,
                    override_safety: false,
                    acknowledged: true,
                })
                .id
                .0
        };
        let partition = inject(
            FaultType::Partition {
                source_app: Some("frontend".to_string()),
            },
            "redis",
            "default",
        );
        let delay = inject(
            FaultType::Delay {
                delay_ns: 300_000_000,
                jitter_ns: 0,
                source_app: None,
            },
            "redis",
            "default",
        );
        inject(
            FaultType::Partition {
                source_app: Some("backend".to_string()),
            },
            "redis",
            "default",
        );
        inject(FaultType::Drop { probability: 50 }, "redis", "team-b");
        inject(FaultType::DnsNxdomain, "backend", "default");
        inject(FaultType::Pause, "redis", "default");

        let faults = agent.path_faults(&crate::onion::trace::TraceRequest {
            source: "frontend".to_string(),
            source_namespace: "default".to_string(),
            destination: "redis".to_string(),
            destination_namespace: "default".to_string(),
            port: None,
            count: None,
        });
        let listed: Vec<(u64, &str)> = faults
            .iter()
            .map(|fault| (fault.id, fault.description.as_str()))
            .collect();
        assert_eq!(
            listed,
            vec![
                (partition, "partition from frontend"),
                (delay, "delay 300ms"),
            ]
        );
    }

    #[tokio::test]
    async fn trace_requires_an_exact_dns_answer_not_server_or_name_text() {
        let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
        let handle = tokio::spawn(async move { agent.run().await });
        expect_complete(
            &send_deploy(
                &tx,
                Config::parse(
                    r#"
            [app.source]
            image = "source:v1"
            [app.destination]
            image = "destination:v1"
            port = 8080
        "#,
                )
                .unwrap(),
            )
            .await,
        );
        let vip = crate::onion::vip::VirtualIP::from_qualified("default__destination");
        let mut verdicts = Vec::new();
        for answer in [
            format!(
                "Server: resolver\nAddress: {vip}#53\nName: destination.default.internal\nAddress: 127.0.0.1"
            ),
            format!("Name: {vip}.invalid\nAddress: {vip}0"),
        ] {
            grill.set_exec_outputs([
                format!("{answer}\n__RB_TRACE_DNS_STATUS__=0\n"),
                "__RB_TRACE_TCP_STATUS__=0\n".into(),
            ]);
            let (response, receiver) = oneshot::channel();
            tx.send(AgentCommand::Trace {
                request: crate::onion::trace::TraceRequest {
                    source: "source".into(),
                    source_namespace: "default".into(),
                    destination: "destination".into(),
                    destination_namespace: "default".into(),
                    port: None,
                    count: None,
                },
                internal_destination: true,
                source_node: "node-a".into(),
                response,
            })
            .await
            .unwrap();
            verdicts.push(receiver.await.unwrap().unwrap().steps[0].verdict.clone());
        }
        shutdown.cancel();
        handle.await.unwrap();
        assert!(
            verdicts
                .iter()
                .all(|verdict| matches!(verdict, crate::onion::trace::TraceVerdict::Fail { .. })),
            "{verdicts:?}"
        );
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
            count: None,
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
                count: None,
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

    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    #[tokio::test]
    async fn egress_health_waits_for_preparation_but_fences_unbound_execution() {
        for stage in [
            ContainerState::Pending,
            ContainerState::Preparing,
            ContainerState::Initialising,
            ContainerState::Starting,
            ContainerState::HealthWait,
            ContainerState::Running,
            ContainerState::Unhealthy,
            ContainerState::Stopping,
        ] {
            let (mut agent, _tx, _shutdown) = test_agent();
            let mut spec = Config::parse("[app.web]\nimage = 'mock:image'\n")
                .unwrap()
                .app
                .remove("web")
                .unwrap();
            let ids = agent
                .supervisor
                .deploy_app("web", "default", &spec, Instant::now())
                .await
                .unwrap();
            spec.egress = Config::parse(
                "[app.web]\nimage = 'mock:image'\n[app.web.egress]\nallow = ['203.0.113.9:443']\n",
            )
            .unwrap()
            .app
            .remove("web")
            .unwrap()
            .egress;
            agent
                .deployed_specs
                .insert(("web".into(), "default".into()), spec);
            agent.supervisor.get_instance_mut(&ids[0]).unwrap().state = stage;
            // Missing kernel ownership is expected before pre-start programming,
            // but must remain a fail-closed condition from init/start onwards.
            agent.enforce_live_egress_or_stop().await;
            let actual = agent.supervisor.get_instance(&ids[0]).unwrap().state;
            if matches!(stage, ContainerState::Pending | ContainerState::Preparing) {
                assert_eq!(
                    actual, stage,
                    "preparation was stopped before policy installation"
                );
                assert!(agent.egress_affected_workloads.is_empty());
            } else {
                assert!(
                    matches!(actual, ContainerState::Stopping | ContainerState::Stopped),
                    "{stage:?} remained {actual:?}"
                );
                assert!(
                    agent
                        .egress_affected_workloads
                        .contains(&("web".into(), "default".into()))
                );
            }
        }
    }

    #[tokio::test]
    async fn health_tick_reuses_egress_evidence_but_later_reports_refresh_it() {
        use std::sync::atomic::Ordering;
        let (mut agent, _tx, _shutdown) = test_agent();
        let readiness = crate::bun::readiness::ReadinessTracker::new();
        agent.set_readiness_tracker(readiness.clone());
        readiness
            .set_capabilities(crate::meat::cluster_state::NodeCapabilities {
                egress: crate::sesame::egress::EgressEnforcementCapability {
                    connect_ipv4: true,
                    connect_ipv6: true,
                    udp_ipv4: true,
                    udp_ipv6: true,
                    pre_start: true,
                },
                ..Default::default()
            })
            .await;
        agent.refresh_egress_readiness().await;
        assert!(
            !readiness
                .capability_snapshot()
                .await
                .egress
                .can_enforce_allowlist()
        );
        assert_eq!(agent.egress_observation_count.load(Ordering::Relaxed), 1);
        agent.refresh_egress_readiness().await;
        assert_eq!(agent.egress_observation_count.load(Ordering::Relaxed), 2);
        let (capabilities, _) = agent.live_egress_report_state().await;
        assert!(!capabilities.egress.can_enforce_allowlist());
        assert_eq!(agent.egress_observation_count.load(Ordering::Relaxed), 3);
    }

    struct TestAgent {
        agent: BunAgent<MockGrill>,
        // Fields drop in declaration order: keep filesystem ownership through
        // agent teardown, including when the fixture moves into a spawned task.
        _volumes: tempfile::TempDir,
    }

    impl std::ops::Deref for TestAgent {
        type Target = BunAgent<MockGrill>;

        fn deref(&self) -> &Self::Target {
            &self.agent
        }
    }

    impl std::ops::DerefMut for TestAgent {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.agent
        }
    }

    fn test_agent() -> (TestAgent, mpsc::Sender<AgentCommand>, CancellationToken) {
        let (agent, tx, shutdown, _grill) = test_agent_with_grill();
        (agent, tx, shutdown)
    }

    #[tokio::test]
    async fn service_registration_refuses_a_closed_agent_channel() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let result = DeployOps { tx }
            .register_service_app("api", "default", 8080, None)
            .await;
        assert!(matches!(result, Err(BunError::BackendPublication { .. })));
    }

    #[tokio::test]
    async fn reporting_binds_execution_to_the_original_runtime_specification() {
        let (mut agent, _, _, grill) = test_agent_with_grill();
        expect_complete(&drain_deploy(&mut agent, basic_config()).await);
        let id = InstanceId("default__web-0".into());
        let spec = agent
            .supervisor
            .get_instance(&id)
            .unwrap()
            .oci_spec
            .clone()
            .unwrap();
        for token in [
            "first-original-generation",
            "replacement-original-generation",
        ] {
            let launch = crate::grill::RuntimeLaunch {
                instance_id: id.clone(),
                spec: spec.clone(),
                generation: crate::grill::RuntimeGeneration::process(token),
                network_reference: None,
            };
            let expected = crate::grill::RuntimeExecution {
                instance_id: id.clone(),
                generation: launch.generation.clone(),
            };
            grill.set_launch_inventory(vec![launch.clone()]).await;
            let (tx, rx) = oneshot::channel();
            agent
                .handle_snapshot_request(CollectSnapshotRequest { response: tx })
                .await;
            assert_eq!(rx.await.unwrap().instances[0].execution, Some(expected));
            let mut wrong_spec = launch.clone();
            wrong_spec
                .spec
                .process
                .args
                .push("different-runtime-spec".into());
            for invalid in [vec![wrong_spec], vec![launch.clone(), launch], vec![]] {
                grill.set_launch_inventory(invalid).await;
                let (tx, rx) = oneshot::channel();
                agent
                    .handle_snapshot_request(CollectSnapshotRequest { response: tx })
                    .await;
                let report = rx.await.unwrap();
                assert_eq!(
                    report.instances.len(),
                    1,
                    "missing identity must not hide resource commitments"
                );
                assert!(report.instances[0].execution.is_none());
            }
        }
    }

    #[tokio::test]
    async fn late_discovery_subscribers_receive_the_latest_service_snapshot() {
        let (mut agent, _commands, _shutdown) = test_agent();
        expect_complete(&drain_deploy(&mut agent, basic_config()).await);
        let view = agent.service_map_watch();
        let snapshot = view.borrow();
        let service = crate::onion::service_id::ServiceId::new("default", "web");
        let entry = snapshot
            .resolve(&service)
            .expect("late subscriber lost the completed deployment");
        assert_eq!(entry.backends.len(), 1);
        assert_eq!(entry.backends[0].instance_id, "default__web-0");
        assert!(entry.backends[0].healthy);
    }

    #[tokio::test]
    async fn fresh_discovery_refuses_adoption_without_original_inventory() {
        let (mut original, _, _, grill) = test_agent_with_grill();
        let records = tempfile::tempdir().unwrap();
        original.set_records_dir(records.path().to_owned());
        grill.set_pid(std::process::id());
        expect_complete(&drain_deploy(&mut original, basic_config()).await);
        let (mut replacement, _, _, recovered) = test_agent_with_grill();
        replacement.set_records_dir(records.path().to_owned());
        replacement
            .enable_fresh_discovery_ownership(&records.path().join("discovery"))
            .await
            .unwrap();
        let result = replacement.adopt_recorded_instances().await;
        assert!(
            matches!(result, Err(BunError::AdoptionState(_))),
            "missing discovery recovery was accepted: {result:?}"
        );
        assert!(
            !recovered
                .calls()
                .iter()
                .any(|(operation, _)| operation == "adopt" || operation == "kill"),
            "runtime recovery ran before original discovery reconciliation"
        );
        assert!(crate::grill::records::record_path(records.path(), "default__web-0").exists());
    }

    #[tokio::test]
    async fn fresh_discovery_enablement_refuses_existing_consumer_obligations() {
        let (mut agent, _, _) = test_agent();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("discovery");
        let journal = crate::bun::discovery_owners::DiscoveryJournal::open_async(&path)
            .await
            .unwrap();
        let inventory = serde_json::from_value(serde_json::json!({
            "services": [], "references": [], "consumer": {
                "identity": {"node_id": "reader", "cluster_identity": vec![42_u8; 32]},
                "publications": []
            }
        }))
        .unwrap();
        drop(journal.persist(inventory).await.unwrap());
        assert!(agent.enable_fresh_discovery_ownership(&path).await.is_err());
        assert!(matches!(
            agent.discovery_ownership,
            discovery_ownership::DiscoveryOwnership::Uncertain
        ));
    }

    #[tokio::test]
    async fn producer_agent_retains_process_port_and_owner_without_remote_confirmation() {
        for has_inventory in [true, false] {
            let (mut agent, _, _, grill) = test_agent_with_grill();
            grill.set_pid(std::process::id());
            let root = tempfile::tempdir().unwrap();
            agent.set_volumes_dir(root.path().join("volumes"));
            agent.set_records_dir(root.path().join("records"));
            agent
                .enable_fresh_discovery_ownership(&root.path().join("discovery"))
                .await
                .unwrap();
            expect_complete(&drain_deploy(&mut agent, basic_config()).await);
            let id = InstanceId("default__web-0".into());
            let original = agent.supervisor.get_instance(&id).unwrap();
            let original_port = original.host_port;
            let original_spec = original.oci_spec.clone().unwrap();
            if has_inventory {
                grill
                    .set_launch_inventory(vec![crate::grill::RuntimeLaunch {
                        instance_id: id.clone(),
                        generation: crate::grill::RuntimeGeneration::process("original"),
                        spec: original_spec,
                        network_reference: None,
                    }])
                    .await;
            }
            let (mut clustered, _, _) = test_cluster_fault_agent().await;
            agent.cluster = clustered.cluster.take();
            agent.kill_and_wait_for_exit(&id).await.unwrap();
            assert!(
                agent.finish_retire_bookkeeping(&id).await.is_err(),
                "unconfirmed producer released its host port"
            );
            assert_eq!(
                agent.supervisor.get_instance(&id).unwrap().host_port,
                original_port
            );
            assert!(
                crate::grill::records::record_path(&root.path().join("records"), &id.0).exists()
            );
        }
    }

    /// Z6.7: with a node stopped, the old instance's release waited for that
    /// node's receipt. The rollout failed, the orchestrator retried it, and
    /// every retry took the retained replacements for "existing" instances
    /// and stopped a healthy one. A rollout now finishes and leaves the
    /// release to the agent loop.
    #[tokio::test]
    async fn a_rollout_finishes_while_the_old_instance_waits_for_remote_release() {
        let grill = MockGrill::new();
        grill.set_pid(std::process::id());
        let allocator = PortAllocator::new(30000, 30010);
        let (_, receiver) = mpsc::channel(8);
        let mut agent = BunAgent::new(
            grill.clone(),
            allocator.clone(),
            receiver,
            CancellationToken::new(),
        );
        let root = tempfile::tempdir().unwrap();
        agent.set_volumes_dir(root.path().join("volumes"));
        agent.set_records_dir(root.path().join("records"));
        agent
            .enable_fresh_discovery_ownership(&root.path().join("discovery"))
            .await
            .unwrap();
        expect_complete(&drain_deploy(&mut agent, basic_config()).await);
        let old = InstanceId("default__web-0".into());
        let original = agent.supervisor.get_instance(&old).unwrap();
        let old_port = original.host_port.unwrap();
        let execution = crate::grill::RuntimeExecution {
            instance_id: old.clone(),
            generation: crate::grill::RuntimeGeneration::process("original"),
        };
        grill
            .set_launch_inventory(vec![crate::grill::RuntimeLaunch {
                instance_id: old.clone(),
                generation: execution.generation.clone(),
                spec: original.oci_spec.clone().unwrap(),
                network_reference: None,
            }])
            .await;
        let (mut clustered, _, _) = test_cluster_fault_agent().await;
        agent.cluster = clustered.cluster.take();
        // A stopped node never sends its receipt: the leader answers 202.
        let (client, pending) =
            crate::cluster::producer::test_fixture(axum::http::StatusCode::ACCEPTED, String::new())
                .await;
        agent.set_producer_release_client(client);

        let replacement = Config::parse("[app.web]\nimage = 'web:v2'\nport = 8080\n").unwrap();
        expect_complete(&drain_deploy(&mut agent, replacement).await);

        assert!(agent.deferred_retirements.contains(&old));
        assert_eq!(
            agent.supervisor.get_instance(&old).unwrap().state,
            ContainerState::Stopped
        );
        assert!(allocator.is_allocated(old_port).await, "released too early");
        let (reply, existing) = oneshot::channel();
        agent
            .handle_deploy_op(DeployOp::ListExistingOwned {
                app_name: "web".into(),
                namespace: "default".into(),
                reply,
            })
            .await;
        let existing = existing.await.unwrap();
        assert!(
            !existing.contains(&old),
            "a later rollout must not retire the old instance again"
        );
        assert_eq!(existing.len(), 1, "{existing:?}");

        // Still pending: the agent loop keeps waiting, nothing else happens.
        agent.drive_deferred_retirements().await;
        assert!(agent.deferred_retirements.contains(&old));
        pending.abort();
        let _ = pending.await;

        // The leader confirms; the next tick releases the address.
        let confirmation =
            serde_json::json!({"node_id": "test", "execution": execution}).to_string();
        let (client, confirmed) =
            crate::cluster::producer::test_fixture(axum::http::StatusCode::OK, confirmation).await;
        agent.set_producer_release_client(client);
        agent.drive_deferred_retirements().await;
        assert!(agent.deferred_retirements.is_empty());
        assert!(agent.supervisor.get_instance(&old).is_none());
        assert!(!allocator.is_allocated(old_port).await);
        confirmed.abort();
        let _ = confirmed.await;
    }

    #[tokio::test]
    async fn slow_producer_release_does_not_stall_the_agent_loop() {
        let grill = MockGrill::new();
        grill.set_pid(std::process::id());
        let allocator = PortAllocator::new(30000, 30001);
        let (_, receiver) = mpsc::channel(8);
        let mut agent = BunAgent::new(
            grill.clone(),
            allocator.clone(),
            receiver,
            CancellationToken::new(),
        );
        let root = tempfile::tempdir().unwrap();
        agent.set_volumes_dir(root.path().join("volumes"));
        agent.set_records_dir(root.path().join("records"));
        agent
            .enable_fresh_discovery_ownership(&root.path().join("discovery"))
            .await
            .unwrap();
        let reference = original_test_network_reference();
        grill.set_network_reference(reference.clone()).await;
        expect_complete(&drain_deploy(&mut agent, basic_config()).await);
        let id = reference.instance_id.clone();
        let original = agent.supervisor.get_instance(&id).unwrap();
        let host_port = original.host_port.unwrap();
        let execution = crate::grill::RuntimeExecution {
            instance_id: id.clone(),
            generation: crate::grill::RuntimeGeneration::runc(reference.generation.as_str()),
        };
        grill
            .set_launch_inventory(vec![crate::grill::RuntimeLaunch {
                instance_id: id.clone(),
                generation: execution.generation.clone(),
                spec: original.oci_spec.clone().unwrap(),
                network_reference: Some(crate::grill::runc_intent::NetworkReferenceState::Held(
                    reference.clone(),
                )),
            }])
            .await;
        let (mut clustered, _, _) = test_cluster_fault_agent().await;
        agent.cluster = clustered.cluster.take();
        agent.kill_and_wait_for_exit(&id).await.unwrap();
        let confirmation =
            serde_json::json!({"node_id": "test", "execution": execution}).to_string();
        // An overloaded or partitioned leader answers slowly.
        let (client, task) = crate::cluster::producer::test_delayed_fixture(
            axum::http::StatusCode::OK,
            confirmation,
            std::time::Duration::from_secs(3),
        )
        .await;
        agent.set_producer_release_client(client);
        let started = std::time::Instant::now();
        assert!(agent.finish_retire_bookkeeping(&id).await.is_err());
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "one retirement held the agent loop for {:?}",
            started.elapsed()
        );
        let retry = std::time::Instant::now();
        assert!(agent.finish_retire_bookkeeping(&id).await.is_err());
        assert!(
            retry.elapsed() < std::time::Duration::from_millis(500),
            "a retry waited for the leader again"
        );
        assert!(allocator.is_allocated(host_port).await);
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        // The confirmation that arrived in the background completes retirement.
        agent.finish_retire_bookkeeping(&id).await.unwrap();
        assert!(!allocator.is_allocated(host_port).await);
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn producer_agent_releases_process_ports_and_exact_runc_addresses_only_after_confirmation()
     {
        for runc in [false, true] {
            let grill = MockGrill::new();
            grill.set_pid(std::process::id());
            let allocator = PortAllocator::new(30000, 30001);
            let (_, receiver) = mpsc::channel(8);
            let mut agent = BunAgent::new(
                grill.clone(),
                allocator.clone(),
                receiver,
                CancellationToken::new(),
            );
            let root = tempfile::tempdir().unwrap();
            agent.set_volumes_dir(root.path().join("volumes"));
            agent.set_records_dir(root.path().join("records"));
            agent
                .enable_fresh_discovery_ownership(&root.path().join("discovery"))
                .await
                .unwrap();
            let reference = original_test_network_reference();
            if runc {
                grill.set_network_reference(reference.clone()).await;
            }
            expect_complete(&drain_deploy(&mut agent, basic_config()).await);
            let id = reference.instance_id.clone();
            let original = agent.supervisor.get_instance(&id).unwrap();
            let host_port = original.host_port.unwrap();
            let execution = crate::grill::RuntimeExecution {
                instance_id: id.clone(),
                generation: if runc {
                    crate::grill::RuntimeGeneration::runc(reference.generation.as_str())
                } else {
                    crate::grill::RuntimeGeneration::process("original")
                },
            };
            grill
                .set_launch_inventory(vec![crate::grill::RuntimeLaunch {
                    instance_id: id.clone(),
                    generation: execution.generation.clone(),
                    spec: original.oci_spec.clone().unwrap(),
                    network_reference: runc.then(|| {
                        crate::grill::runc_intent::NetworkReferenceState::Held(reference.clone())
                    }),
                }])
                .await;
            let (mut clustered, _, _) = test_cluster_fault_agent().await;
            agent.cluster = clustered.cluster.take();
            agent.kill_and_wait_for_exit(&id).await.unwrap();
            let (client, task) = crate::cluster::producer::test_fixture(
                axum::http::StatusCode::ACCEPTED,
                String::new(),
            )
            .await;
            agent.set_producer_release_client(client);
            assert!(agent.finish_retire_bookkeeping(&id).await.is_err());
            assert!(allocator.is_allocated(host_port).await);
            assert!(agent.supervisor.get_instance(&id).is_some());
            assert!(
                !grill
                    .calls()
                    .iter()
                    .any(|(operation, _)| operation == "release_network_reference")
            );
            task.abort();
            let _ = task.await;
            let confirmation =
                serde_json::json!({"node_id": "test", "execution": execution}).to_string();
            let (client, delayed_task) = crate::cluster::producer::test_delayed_fixture(
                axum::http::StatusCode::OK,
                confirmation.clone(),
                std::time::Duration::from_secs(1),
            )
            .await;
            agent.set_producer_release_client(client);
            assert!(
                tokio::time::timeout(
                    std::time::Duration::from_millis(50),
                    agent.finish_retire_bookkeeping(&id)
                )
                .await
                .is_err()
            );
            assert!(allocator.is_allocated(host_port).await);
            assert!(agent.supervisor.get_instance(&id).is_some());
            assert!(
                !grill
                    .calls()
                    .iter()
                    .any(|(operation, _)| operation == "release_network_reference")
            );
            delayed_task.abort();
            let _ = delayed_task.await;
            let (client, task) =
                crate::cluster::producer::test_fixture(axum::http::StatusCode::OK, confirmation)
                    .await;
            agent.set_producer_release_client(client);
            agent.finish_retire_bookkeeping(&id).await.unwrap();
            assert!(!allocator.is_allocated(host_port).await);
            assert!(agent.supervisor.get_instance(&id).is_none());
            assert!(
                !crate::grill::records::record_path(&root.path().join("records"), &id.0).exists()
            );
            assert_eq!(
                grill
                    .calls()
                    .iter()
                    .any(|(operation, _)| operation == "release_network_reference"),
                runc
            );
            task.abort();
            let _ = task.await;
        }
    }

    fn pending_release() -> BunError {
        BunError::ProducerReleasePending {
            instance_id: InstanceId("default__web-0".into()),
            reason: "other nodes have not yet confirmed the endpoint's withdrawal",
        }
    }

    /// Z6.7: a rolling deploy on a three-node laptop cluster failed every
    /// retirement on the leader's first "pending" answer and started a new
    /// generation of replacements, forever.
    #[tokio::test(start_paused = true)]
    async fn a_pending_producer_release_is_asked_again_until_confirmed() {
        let attempts = std::sync::atomic::AtomicU32::new(0);
        let outcome = retry_while_release_pending(
            std::time::Duration::from_secs(30),
            std::time::Duration::from_secs(1),
            || async {
                if attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) < 3 {
                    Err(pending_release())
                } else {
                    Ok(())
                }
            },
        )
        .await;
        assert!(outcome.is_ok(), "{outcome:?}");
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn a_producer_release_still_pending_after_the_patience_fails_the_retirement() {
        let attempts = std::sync::atomic::AtomicU32::new(0);
        let started = tokio::time::Instant::now();
        let outcome = retry_while_release_pending(
            std::time::Duration::from_secs(30),
            std::time::Duration::from_secs(1),
            || async {
                attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Err(pending_release())
            },
        )
        .await;
        assert!(matches!(
            outcome,
            Err(BunError::ProducerReleasePending { .. })
        ));
        assert!(started.elapsed() <= std::time::Duration::from_secs(30));
        assert!(attempts.load(std::sync::atomic::Ordering::SeqCst) >= 29);
    }

    #[tokio::test(start_paused = true)]
    async fn a_refused_producer_release_is_not_retried() {
        let attempts = std::sync::atomic::AtomicU32::new(0);
        let outcome = retry_while_release_pending(
            std::time::Duration::from_secs(30),
            std::time::Duration::from_secs(1),
            || async {
                attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Err(BunError::RetirementState {
                    instance_id: InstanceId("default__web-0".into()),
                    reason: "producer release is unconfirmed (409 Conflict)".into(),
                })
            },
        )
        .await;
        assert!(matches!(outcome, Err(BunError::RetirementState { .. })));
        assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    fn original_test_network_reference() -> crate::grill::runc_intent::NetworkReference {
        serde_json::from_value(serde_json::json!({
            "instance_id": "default__web-0", "generation": "1234567890abcdef1234567890abcdef", "container_index": 7
        })).unwrap()
    }

    #[tokio::test]
    async fn failed_release_permission_checkpoint_preserves_the_runtime_hold() {
        let (mut agent, _, _, grill) = test_agent_with_grill();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("discovery");
        agent.enable_fresh_discovery_ownership(&path).await.unwrap();
        let reference = original_test_network_reference();
        grill.set_network_reference(reference.clone()).await;
        expect_complete(&drain_deploy(&mut agent, basic_config()).await);
        let checkpoint = path.join("discovery.json");
        std::fs::remove_file(&checkpoint).unwrap();
        std::fs::create_dir(&checkpoint).unwrap();
        assert!(agent.stop_app("web", "default").await.is_err());
        assert!(
            !grill
                .calls()
                .iter()
                .any(|(operation, _)| operation == "release_network_reference")
        );
        assert_eq!(
            grill
                .network_reference(&reference.instance_id)
                .await
                .unwrap(),
            Some(reference)
        );
    }

    #[tokio::test]
    async fn standalone_release_persists_permission_before_runtime_acknowledgement() {
        let (mut agent, _, _, grill) = test_agent_with_grill();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("discovery");
        agent.enable_fresh_discovery_ownership(&path).await.unwrap();
        let reference = original_test_network_reference();
        grill.set_network_reference(reference.clone()).await;
        expect_complete(&drain_deploy(&mut agent, basic_config()).await);
        grill.block_network_releases();
        let mut task = tokio::spawn(async move {
            let result = agent.stop_app("web", "default").await;
            (agent, result)
        });
        tokio::select! {
            result = &mut task => panic!("standalone retirement returned before authorised release: {:?}", result.unwrap().1),
            result = tokio::time::timeout(std::time::Duration::from_secs(2), grill.wait_for_network_release()) => result.unwrap(),
        }
        let checkpoint: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path.join("discovery.json")).unwrap()).unwrap();
        let inventory: crate::bun::discovery_owners::DiscoveryInventory =
            serde_json::from_value(checkpoint["inventory"].clone()).unwrap();
        assert_eq!(inventory.references[0].reference, reference);
        assert_eq!(
            inventory.references[0].phase,
            crate::bun::discovery_owners::ReferencePhase::ReleaseAuthorised
        );
        assert!(
            !inventory.services[0]
                .entry
                .backends
                .iter()
                .any(|backend| backend.instance_id == reference.instance_id.0)
        );
        assert_eq!(
            grill
                .network_reference(&reference.instance_id)
                .await
                .unwrap(),
            Some(reference.clone())
        );
        grill.resume_network_release();
        let (agent, result) = tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        result.unwrap();
        drop(agent);
        let journal = crate::bun::discovery_owners::DiscoveryJournal::open(&path).unwrap();
        assert!(journal.inventory().references.is_empty());
        assert!(
            grill
                .network_reference(&reference.instance_id)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn clustered_discovery_requires_remote_proof_before_release_permission() {
        let (mut agent, _, _, grill) = test_agent_with_grill();
        let directory = tempfile::tempdir().unwrap();
        agent
            .enable_fresh_discovery_ownership(&directory.path().join("discovery"))
            .await
            .unwrap();
        let reference = original_test_network_reference();
        grill.set_network_reference(reference.clone()).await;
        expect_complete(&drain_deploy(&mut agent, basic_config()).await);
        grill
            .set_launch_inventory(vec![crate::grill::RuntimeLaunch {
                instance_id: reference.instance_id.clone(),
                generation: crate::grill::RuntimeGeneration::runc(reference.generation.as_str()),
                spec: agent
                    .supervisor
                    .get_instance(&reference.instance_id)
                    .unwrap()
                    .oci_spec
                    .clone()
                    .unwrap(),
                network_reference: Some(crate::grill::runc_intent::NetworkReferenceState::Held(
                    reference.clone(),
                )),
            }])
            .await;
        let (mut cluster_agent, _, _) = test_cluster_fault_agent().await;
        agent.cluster = cluster_agent.cluster.take();
        let result = agent.stop_app("web", "default").await;
        assert!(
            matches!(result, Err(BunError::RetirementState { ref reason, .. }) if reason.contains("producer release transport")),
            "held reference was not fenced by its durable owner: {result:?}"
        );
        assert_eq!(
            grill
                .network_reference(&reference.instance_id)
                .await
                .unwrap(),
            Some(reference)
        );
    }

    #[tokio::test]
    async fn durable_discovery_captures_the_original_runtime_reference_before_launch() {
        let (mut agent, _, _, grill) = test_agent_with_grill();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("discovery");
        agent.enable_fresh_discovery_ownership(&path).await.unwrap();
        let reference = original_test_network_reference();
        grill.set_network_reference(reference.clone()).await;
        expect_complete(&drain_deploy(&mut agent, basic_config()).await);
        drop(agent);
        let journal = crate::bun::discovery_owners::DiscoveryJournal::open(&path).unwrap();
        let references = &journal.inventory().references;
        assert_eq!(
            references.len(),
            1,
            "runtime started without a durable original reference"
        );
        assert_eq!(references[0].reference, reference);
        assert_eq!(
            references[0].service,
            crate::onion::service_id::ServiceId::new("default", "web")
        );
        assert_eq!(
            references[0].phase,
            crate::bun::discovery_owners::ReferencePhase::Held
        );
    }

    #[tokio::test]
    async fn failed_runtime_reference_checkpoint_prevents_start_and_retains_the_address() {
        let (mut agent, _, _, grill) = test_agent_with_grill();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("discovery");
        agent.enable_fresh_discovery_ownership(&path).await.unwrap();
        let reference = original_test_network_reference();
        grill.set_network_reference(reference.clone()).await;
        grill.block_creates();
        let task = tokio::spawn(async move {
            let events = drain_deploy(&mut agent, basic_config()).await;
            (agent, events)
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), grill.wait_for_creates(1))
            .await
            .unwrap();
        let checkpoint = path.join("discovery.json");
        std::fs::remove_file(&checkpoint).unwrap();
        std::fs::create_dir(&checkpoint).unwrap();
        grill.release_creates(1);
        let (_agent, events) = tokio::time::timeout(std::time::Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ApplyEvent::Error { .. }))
        );
        assert!(
            !grill
                .calls()
                .iter()
                .any(|(operation, _)| operation == "start"),
            "runtime started after original-reference persistence failed"
        );
        assert_eq!(
            grill
                .network_reference(&reference.instance_id)
                .await
                .unwrap(),
            Some(reference)
        );
    }

    #[tokio::test]
    async fn durable_publication_records_the_exact_service_before_acknowledgement() {
        let (mut agent, _commands, _shutdown) = test_agent();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("discovery");
        agent.enable_fresh_discovery_ownership(&path).await.unwrap();
        expect_complete(&drain_deploy(&mut agent, basic_config()).await);
        let service = crate::onion::service_id::ServiceId::new("default", "web");
        let expected = agent.service_map.resolve(&service).unwrap().clone();
        drop(agent);
        let journal = crate::bun::discovery_owners::DiscoveryJournal::open(&path).unwrap();
        let entries = &journal.inventory().services;
        assert_eq!(
            entries.len(),
            1,
            "acknowledged routing has no durable service owner"
        );
        assert_eq!(
            serde_json::to_value(&entries[0].entry).unwrap(),
            serde_json::to_value(&expected).unwrap()
        );
        assert_eq!(
            entries[0].phase,
            crate::bun::discovery_owners::ServicePhase::Owned
        );
    }

    async fn discovery_recovery_fixture() -> (
        BunAgent<MockGrill>,
        MockGrill,
        tempfile::TempDir,
        crate::grill::runc_intent::NetworkReference,
    ) {
        let (mut original, _, _, grill) = test_agent_with_grill();
        let root = tempfile::tempdir().unwrap();
        original.set_records_dir(root.path().join("records"));
        original.set_volumes_dir(root.path().join("volumes"));
        original
            .enable_fresh_discovery_ownership(&root.path().join("discovery"))
            .await
            .unwrap();
        let reference = original_test_network_reference();
        grill.set_pid(std::process::id());
        grill.set_container_ip("10.0.2.5".parse().unwrap());
        grill.set_network_reference(reference.clone()).await;
        expect_complete(&drain_deploy(&mut original, basic_config()).await);
        let spec = original
            .supervisor
            .get_instance(&reference.instance_id)
            .unwrap()
            .oci_spec
            .clone()
            .unwrap();
        grill
            .set_launch_inventory(vec![crate::grill::RuntimeLaunch {
                generation: crate::grill::RuntimeGeneration::runc(reference.generation.as_str()),
                instance_id: reference.instance_id.clone(),
                spec,
                network_reference: Some(crate::grill::runc_intent::NetworkReferenceState::Held(
                    reference.clone(),
                )),
            }])
            .await;
        drop(original);
        let (_, receiver) = mpsc::channel(32);
        let mut recovered = BunAgent::new(
            grill.clone(),
            PortAllocator::new(30000, 31000),
            receiver,
            CancellationToken::new(),
        );
        recovered.set_records_dir(root.path().join("records"));
        recovered.set_volumes_dir(root.path().join("volumes"));
        (recovered, grill, root, reference)
    }

    #[tokio::test]
    async fn discovery_recovery_gives_up_on_a_wedged_runtime_inventory() {
        let (mut agent, grill, root, _) = discovery_recovery_fixture().await;
        grill.set_inventory_delay(Some(std::time::Duration::from_secs(300)));
        let started = std::time::Instant::now();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            agent.recover_discovery_ownership(&root.path().join("discovery")),
        )
        .await
        .expect("discovery recovery hung on the runtime inventory");
        assert!(result.is_err(), "recovery proceeded without an inventory");
        assert!(started.elapsed() < std::time::Duration::from_secs(20));
    }

    #[tokio::test]
    async fn discovery_recovery_reserves_original_vip_before_republishing_adopted_runtime() {
        let (mut agent, grill, root, reference) = discovery_recovery_fixture().await;
        let service = crate::onion::service_id::ServiceId::new("default", "web");
        let saved =
            crate::bun::discovery_owners::DiscoveryJournal::open(&root.path().join("discovery"))
                .unwrap();
        let original_vip = saved.inventory().services[0].entry.vip;
        drop(saved);
        grill.set_adopt_result(&reference.instance_id, true);
        agent
            .recover_discovery_ownership(&root.path().join("discovery"))
            .await
            .unwrap();
        assert_eq!(
            agent.service_map.resolve(&service).unwrap().vip,
            original_vip
        );
        assert!(
            agent
                .service_map
                .resolve(&service)
                .unwrap()
                .backends
                .is_empty()
        );
        assert!(
            agent
                .service_map_watch()
                .borrow()
                .resolve(&service)
                .is_none()
        );
        assert_eq!(agent.adopt_recorded_instances().await.unwrap(), 1);
        let view = agent.service_map_watch();
        let map = view.borrow();
        let live = map.resolve(&service).unwrap();
        assert_eq!(live.vip, original_vip);
        assert_eq!(live.backends[0].node_ip.to_string(), "10.0.2.5");
        assert!(live.backends[0].healthy);
        drop(map);
        agent.retire_workload("web", "default").await.unwrap();
    }

    #[tokio::test]
    async fn discovery_recovery_does_not_publish_historical_health() {
        let (mut agent, grill, root, reference) = discovery_recovery_fixture().await;
        let records = root.path().join("records");
        let mut record = crate::grill::records::load_records(&records)
            .unwrap()
            .remove(0);
        record.app_spec.as_mut().unwrap().health = config_with_health().app["web"].health.clone();
        crate::grill::records::write_record(&records, &record).unwrap();
        grill.set_adopt_result(&reference.instance_id, true);
        agent
            .recover_discovery_ownership(&root.path().join("discovery"))
            .await
            .unwrap();
        assert_eq!(agent.adopt_recorded_instances().await.unwrap(), 1);
        let service = crate::onion::service_id::ServiceId::new("default", "web");
        assert!(
            !agent
                .service_map_watch()
                .borrow()
                .resolve(&service)
                .unwrap()
                .backends[0]
                .healthy
        );
        assert_eq!(
            agent
                .supervisor
                .get_instance(&reference.instance_id)
                .unwrap()
                .state,
            ContainerState::HealthWait
        );
        agent.retire_workload("web", "default").await.unwrap();
    }

    #[tokio::test]
    async fn discovery_recovery_retires_unrecorded_original_runtime_and_allocation() {
        let (mut agent, grill, root, reference) = discovery_recovery_fixture().await;
        crate::grill::records::remove_record(
            &root.path().join("records"),
            &reference.instance_id.0,
        )
        .unwrap();
        agent
            .recover_discovery_ownership(&root.path().join("discovery"))
            .await
            .unwrap();
        assert_eq!(agent.adopt_recorded_instances().await.unwrap(), 0);
        assert!(
            grill
                .network_reference(&reference.instance_id)
                .await
                .unwrap()
                .is_none()
        );
        drop(agent);
        let journal =
            crate::bun::discovery_owners::DiscoveryJournal::open(&root.path().join("discovery"))
                .unwrap();
        assert!(journal.inventory().references.is_empty());
        assert!(journal.inventory().services.is_empty());
    }

    #[tokio::test]
    async fn discovery_recovery_replays_original_permission_without_a_new_launch() {
        let (mut agent, grill, root, reference) = discovery_recovery_fixture().await;
        grill.set_state(&reference.instance_id, ContainerState::Stopped);
        let journal =
            crate::bun::discovery_owners::DiscoveryJournal::open(&root.path().join("discovery"))
                .unwrap();
        let mut inventory = journal.inventory().clone();
        inventory.references[0].phase =
            crate::bun::discovery_owners::ReferencePhase::ReleaseAuthorised;
        inventory.services[0].entry.backends.clear();
        drop(journal.persist(inventory).await.unwrap());
        let before = grill.calls().len();
        agent
            .recover_discovery_ownership(&root.path().join("discovery"))
            .await
            .unwrap();
        assert_eq!(agent.adopt_recorded_instances().await.unwrap(), 0);
        assert!(
            grill
                .network_reference(&reference.instance_id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            !grill.calls()[before..]
                .iter()
                .any(|(op, _)| op == "create" || op == "start")
        );
    }

    #[tokio::test]
    async fn clustered_recovery_replays_durable_release_without_contacting_leader() {
        let (mut agent, grill, root, reference) = discovery_recovery_fixture().await;
        grill.set_state(&reference.instance_id, ContainerState::Stopped);
        let journal =
            crate::bun::discovery_owners::DiscoveryJournal::open(&root.path().join("discovery"))
                .unwrap();
        let mut inventory = journal.inventory().clone();
        inventory.references[0].phase =
            crate::bun::discovery_owners::ReferencePhase::ReleaseAuthorised;
        inventory.services[0].entry.backends.clear();
        let identity = crate::bun::consumer_owners::ConsumerIdentity {
            node_id: crate::meat::NodeId::new("test"),
            cluster_identity: [42; 32],
        };
        inventory.consumer = Some(crate::bun::consumer_owners::ConsumerOwnership {
            identity: identity.clone(),
            publications: vec![],
            phase: crate::bun::consumer_owners::ConsumerPhase::Withdrawn,
            receipts: Default::default(),
        });
        drop(journal.persist(inventory).await.unwrap());
        let (mut clustered, _, _) = test_cluster_fault_agent().await;
        agent.cluster = clustered.cluster.take();
        agent
            .recover_consumer_ownership(&root.path().join("discovery"), identity)
            .await
            .unwrap();
        agent.replay_discovery_releases().await.unwrap();
        assert!(
            grill
                .network_reference(&reference.instance_id)
                .await
                .unwrap()
                .is_none()
        );
        assert!(agent.network_references.is_empty());
    }

    #[tokio::test]
    async fn discovery_recovery_refuses_changed_runtime_before_adoption_or_cleanup() {
        let (mut agent, grill, root, reference) = discovery_recovery_fixture().await;
        let mut launches = grill.launch_inventory().await.unwrap().unwrap();
        let mut changed = reference.clone();
        changed.container_index += 1;
        launches[0].network_reference = Some(
            crate::grill::runc_intent::NetworkReferenceState::Held(changed),
        );
        grill.set_launch_inventory(launches).await;
        let before = grill.calls().len();
        assert!(
            agent
                .recover_discovery_ownership(&root.path().join("discovery"))
                .await
                .is_err()
        );
        assert!(agent.adopt_recorded_instances().await.is_err());
        assert_eq!(grill.calls().len(), before);
        assert_eq!(
            grill
                .network_reference(&reference.instance_id)
                .await
                .unwrap(),
            Some(reference)
        );
        assert!(agent.service_map_watch().borrow().resolve_all().is_empty());
    }

    #[tokio::test]
    async fn confirmed_stop_forgets_durable_service_allocation() {
        let (mut agent, _, _) = test_agent();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("discovery");
        agent.enable_fresh_discovery_ownership(&path).await.unwrap();
        expect_complete(&drain_deploy(&mut agent, basic_config()).await);
        agent.stop_app("web", "default").await.unwrap();
        drop(agent);
        let journal = crate::bun::discovery_owners::DiscoveryJournal::open(&path).unwrap();
        assert!(
            journal.inventory().services.is_empty(),
            "confirmed stop retained its allocation"
        );
    }

    #[tokio::test]
    async fn failed_service_retirement_checkpoint_keeps_the_allocated_vip() {
        let (mut agent, _, _) = test_agent();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("discovery");
        agent.enable_fresh_discovery_ownership(&path).await.unwrap();
        expect_complete(&drain_deploy(&mut agent, basic_config()).await);
        let service = crate::onion::service_id::ServiceId::new("default", "web");
        let original = agent.service_map.resolve(&service).unwrap().vip;
        let checkpoint = path.join("discovery.json");
        std::fs::remove_file(&checkpoint).unwrap();
        std::fs::create_dir(&checkpoint).unwrap();
        assert!(
            agent.stop_app("web", "default").await.is_err(),
            "stop acknowledged failed service retirement"
        );
        assert_eq!(agent.service_map.resolve(&service).unwrap().vip, original);
    }

    #[tokio::test]
    async fn clustered_service_retirement_requires_remote_proof_without_runtime_references() {
        let (mut agent, _, _) = test_agent();
        let directory = tempfile::tempdir().unwrap();
        agent
            .enable_fresh_discovery_ownership(&directory.path().join("discovery"))
            .await
            .unwrap();
        expect_complete(&drain_deploy(&mut agent, basic_config()).await);
        let (mut cluster_agent, _, _) = test_cluster_fault_agent().await;
        agent.cluster = cluster_agent.cluster.take();
        assert!(
            agent.stop_app("web", "default").await.is_err(),
            "clustered stop forgot an unconfirmed allocation"
        );
        let service = crate::onion::service_id::ServiceId::new("default", "web");
        assert!(agent.service_map.resolve(&service).is_some());
    }

    #[tokio::test]
    async fn later_publication_preserves_unretired_discovery_allocations() {
        let (mut agent, _commands, _shutdown) = test_agent();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("discovery");
        agent.enable_fresh_discovery_ownership(&path).await.unwrap();
        expect_complete(&drain_deploy(&mut agent, basic_config()).await);
        let service = crate::onion::service_id::ServiceId::new("default", "web");
        let original = agent.service_map.resolve(&service).unwrap().clone();
        // Simulate lost private metadata without confirmed retirement.
        agent.service_map.unregister(&service).unwrap();
        assert!(agent.service_map.resolve(&service).is_none());
        let config = Config::parse("[app.other]\nimage = 'mock:image'\nport = 8081\n").unwrap();
        expect_complete(&drain_deploy(&mut agent, config).await);
        drop(agent);
        let journal = crate::bun::discovery_owners::DiscoveryJournal::open(&path).unwrap();
        assert_eq!(journal.inventory().services.len(), 2);
        let retained = journal
            .inventory()
            .services
            .iter()
            .find(|owner| owner.entry.app_name == "web")
            .unwrap();
        assert_eq!(retained.entry.vip, original.vip);
        assert_eq!(
            retained.phase,
            crate::bun::discovery_owners::ServicePhase::Owned
        );
    }

    #[tokio::test]
    async fn failed_discovery_checkpoint_refuses_launch_and_fences_later_publication() {
        let (mut agent, _commands, _shutdown, grill) = test_agent_with_grill();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("discovery");
        agent.enable_fresh_discovery_ownership(&path).await.unwrap();
        let checkpoint = path.join("discovery.json");
        std::fs::remove_file(&checkpoint).unwrap();
        std::fs::create_dir(&checkpoint).unwrap();
        let events = drain_deploy(&mut agent, basic_config()).await;
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ApplyEvent::Error { .. })),
            "deployment acknowledged a failed discovery checkpoint"
        );
        assert!(
            !grill
                .calls()
                .iter()
                .any(|(operation, _)| operation == "create" || operation == "start")
        );
        std::fs::remove_dir(&checkpoint).unwrap();
        // Repairing the path does not establish what an interrupted write published.
        let service = crate::onion::service_id::ServiceId::new("default", "web");
        assert!(agent.publish_backend_ebpf(&service).await.is_err());
    }

    #[tokio::test]
    async fn stopped_runtime_keeps_artifacts_until_captured_ingress_releases() {
        let (mut agent, _commands, _shutdown, grill) = test_agent_with_grill();
        let records = tempfile::tempdir().unwrap();
        agent.set_records_dir(records.path().to_owned());
        grill.set_pid(std::process::id());
        expect_complete(&drain_deploy(&mut agent, basic_config()).await);
        let id = InstanceId("default__web-0".into());
        let record = crate::grill::records::record_path(records.path(), &id.0);
        let identity = agent.instance_identity_dir(&id);
        assert!(record.exists() && identity.exists());
        let drains = agent.drains.clone();
        let tokens = drains
            .capture_requests(std::slice::from_ref(&id.0), false)
            .await
            .unwrap();
        grill.set_state(&id, ContainerState::Stopped);
        let result = agent.retire_instance_artifacts(&id).await;
        assert!(
            matches!(result, Err(BunError::RetirementState { .. })),
            "stopped-runtime cleanup discarded ownership before request release: {result:?}"
        );
        assert!(record.exists() && identity.exists());
        assert!(
            tokens[0].is_cancelled(),
            "an absent runtime's captured requests were not cancelled"
        );
        drains.decrement_connections(&id.0).await;
        agent.retire_instance_artifacts(&id).await.unwrap();
        assert!(!record.exists() && !identity.exists());
        agent.retire_workload("web", "default").await.unwrap();
    }

    #[tokio::test]
    async fn explicit_stop_waits_for_captured_ingress_before_runtime_retirement() {
        let (mut agent, _commands, _shutdown, grill) = test_agent_with_grill();
        expect_complete(&drain_deploy(&mut agent, basic_config()).await);
        let id = InstanceId("default__web-0".into());
        let drains = agent.drains.clone();
        let _tokens = drains
            .capture_requests(std::slice::from_ref(&id.0), false)
            .await
            .unwrap();
        let mut task = tokio::spawn(async move {
            let result = agent.stop_app("web", "default").await;
            (agent, result)
        });
        tokio::select! {
            result = &mut task => panic!("stop returned before captured request release: {:?}", result.unwrap().1),
            result = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while !drains.is_draining(&id.0).await { tokio::task::yield_now().await; }
            }) => result.expect("stop did not start an ingress drain"),
        }
        assert!(
            !grill
                .calls()
                .iter()
                .any(|(operation, instance)| instance == &id
                    && matches!(operation.as_str(), "stop" | "kill")),
            "runtime retired before ingress request release"
        );
        drains.decrement_connections(&id.0).await;
        let (_agent, result) = tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        result.unwrap();
    }

    #[tokio::test]
    async fn automatic_restart_defers_runtime_retirement_until_captured_ingress_releases() {
        let (mut agent, grill, id, _directory) = failed_restart_fixture().await;
        let drains = agent.drains.clone();
        let view = agent.service_map_watch();
        let _tokens = drains
            .capture_requests(std::slice::from_ref(&id.0), false)
            .await
            .unwrap();
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            agent.drive_pending_restarts(),
        )
        .await
        .expect("waiting for ingress blocked the agent loop");
        assert!(
            !grill
                .calls()
                .iter()
                .any(|(operation, instance)| instance == &id && operation == "kill"),
            "restart retired its predecessor before request release"
        );
        assert_eq!(
            agent.supervisor.get_instance(&id).unwrap().state,
            ContainerState::Pending
        );
        let service = crate::onion::service_id::ServiceId::new("default", "retry");
        assert!(view.borrow().resolve(&service).unwrap().backends.is_empty());
        assert!(drains.is_draining(&id.0).await);
        drains.decrement_connections(&id.0).await;
        agent.drive_pending_restarts().await;
        assert_eq!(
            agent.supervisor.get_instance(&id).unwrap().state,
            ContainerState::Running
        );
        agent.stop_app("retry", "default").await.unwrap();
    }

    #[tokio::test]
    async fn refused_restart_keeps_cleanup_owed_when_stop_fails() {
        let (mut agent, _, _, grill) = test_agent_with_grill();
        expect_complete(&drain_deploy(&mut agent, basic_config()).await);
        let id = agent.supervisor.list_instances()[0].id.clone();
        // Without its original cgroup path, egress preparation refuses the
        // restart after the replacement container has been created.
        grill.set_honours_cgroup_path(true);
        agent
            .supervisor
            .get_instance_mut(&id)
            .unwrap()
            .oci_spec
            .as_mut()
            .unwrap()
            .linux
            .cgroups_path = None;
        grill.set_state(&id, ContainerState::Stopped);
        agent.check_apps().await;
        grill.set_fail_stop(true);
        agent.drive_pending_restarts().await;
        let instance = agent.supervisor.get_instance(&id).unwrap();
        assert_ne!(
            instance.state,
            ContainerState::Failed,
            "a refused restart abandoned its created container"
        );
        assert!(
            instance.retry_pending,
            "cleanup of the created container is no longer owed"
        );
    }

    #[tokio::test]
    async fn automatic_restart_releases_original_address_before_successor_creation() {
        let (mut agent, _, _, grill) = test_agent_with_grill();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("discovery");
        agent.enable_fresh_discovery_ownership(&path).await.unwrap();
        let original = original_test_network_reference();
        grill.set_network_reference(original.clone()).await;
        expect_complete(&drain_deploy(&mut agent, basic_config()).await);
        grill.set_state(&original.instance_id, ContainerState::Stopped);
        agent.check_apps().await;
        agent.drive_pending_restarts().await;
        let calls = grill.calls();
        let release = calls.iter().position(|(operation, id)| {
            operation == "release_network_reference" && id == &original.instance_id
        });
        let successor = calls
            .iter()
            .rposition(|(operation, id)| operation == "create" && id == &original.instance_id)
            .unwrap();
        assert!(
            release.is_some_and(|release| release < successor),
            "successor creation preceded original address release: {calls:?}"
        );
        assert!(!agent.network_references.contains_key(&original.instance_id));
        assert_eq!(
            agent
                .supervisor
                .get_instance(&original.instance_id)
                .unwrap()
                .state,
            ContainerState::Running
        );
        agent.retire_workload("web", "default").await.unwrap();
    }

    #[tokio::test]
    async fn automatic_restart_retains_original_address_when_release_permission_fails() {
        let (mut agent, _, _, grill) = test_agent_with_grill();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("discovery");
        agent.enable_fresh_discovery_ownership(&path).await.unwrap();
        let original = original_test_network_reference();
        grill.set_network_reference(original.clone()).await;
        expect_complete(&drain_deploy(&mut agent, basic_config()).await);
        grill.set_state(&original.instance_id, ContainerState::Stopped);
        agent.check_apps().await;
        let checkpoint = path.join("discovery.json");
        std::fs::remove_file(&checkpoint).unwrap();
        std::fs::create_dir(&checkpoint).unwrap();
        agent.drive_pending_restarts().await;
        assert_eq!(
            grill
                .calls()
                .iter()
                .filter(|(operation, id)| operation == "create" && id == &original.instance_id)
                .count(),
            1,
            "successor created without release permission"
        );
        assert_eq!(
            grill
                .network_reference(&original.instance_id)
                .await
                .unwrap(),
            Some(original.clone())
        );
        assert_eq!(
            agent
                .supervisor
                .get_instance(&original.instance_id)
                .unwrap()
                .state,
            ContainerState::Pending
        );
    }

    #[tokio::test]
    async fn automatic_restart_publishes_the_replacement_address() {
        let (mut agent, _commands, _shutdown, grill) = test_agent_with_grill();
        let old_ip = std::net::Ipv4Addr::new(10, 0, 2, 5);
        let new_ip = std::net::Ipv4Addr::new(10, 0, 2, 6);
        grill.set_container_ip(old_ip);
        expect_complete(&drain_deploy(&mut agent, basic_config()).await);
        let view = agent.service_map_watch();
        let id = InstanceId("default__web-0".into());
        let service = crate::onion::service_id::ServiceId::new("default", "web");
        grill.set_state(&id, ContainerState::Stopped);
        agent.check_apps().await;
        grill.set_container_ip(new_ip);
        agent.drive_pending_restarts().await;
        assert_eq!(
            agent.supervisor.get_instance(&id).unwrap().state,
            ContainerState::Running
        );
        assert_eq!(
            view.borrow().resolve(&service).unwrap().backends[0].node_ip,
            new_ip
        );
        assert!(view.borrow().resolve(&service).unwrap().backends[0].healthy);
        agent.retire_workload("web", "default").await.unwrap();
    }

    #[tokio::test]
    async fn automatic_restart_waits_for_health_before_publishing_a_healthy_backend() {
        let (mut agent, _commands, _shutdown) = test_agent();
        expect_complete(&drain_deploy(&mut agent, basic_config()).await);
        let view = agent.service_map_watch();
        let id = InstanceId("default__web-0".into());
        let service = crate::onion::service_id::ServiceId::new("default", "web");
        let mut health = super::super::health::HealthCheckConfig::from_spec(
            config_with_health().app["web"].health.as_ref().unwrap(),
            8080,
        );
        health.threshold_unhealthy = 1;
        health.threshold_healthy = 1;
        let instance = agent.supervisor.get_instance_mut(&id).unwrap();
        instance.health_config = Some(health);
        let created_at = instance.created_at;
        agent
            .complete_health_probe(
                id.clone(),
                created_at,
                Ok(super::super::health::HealthStatus::Unhealthy),
            )
            .await;
        agent.drive_pending_restarts().await;
        assert_eq!(
            agent.supervisor.get_instance(&id).unwrap().state,
            ContainerState::HealthWait
        );
        assert!(
            !agent.service_map.resolve(&service).unwrap().backends[0].healthy,
            "restart published a healthy backend before its first successful probe"
        );
        assert!(!view.borrow().resolve(&service).unwrap().backends[0].healthy);
        let created_at = agent.supervisor.get_instance(&id).unwrap().created_at;
        agent
            .complete_health_probe(
                id,
                created_at,
                Ok(super::super::health::HealthStatus::Healthy),
            )
            .await;
        assert!(view.borrow().resolve(&service).unwrap().backends[0].healthy);
        agent.retire_workload("web", "default").await.unwrap();
    }

    #[tokio::test]
    async fn a_probe_that_lands_after_a_kill_keeps_the_restarted_instance_probed() {
        let (mut agent, _commands, _shutdown) = test_agent();
        expect_complete(&drain_deploy(&mut agent, basic_config()).await);
        let id = InstanceId("default__web-0".into());
        let health = super::super::health::HealthCheckConfig::from_spec(
            config_with_health().app["web"].health.as_ref().unwrap(),
            8080,
        );
        let now = Instant::now();
        agent.supervisor.register_health(id.clone(), health, now);
        // The check is taken off the queue for a probe, as run_health_checks does.
        let far = now + std::time::Duration::from_secs(3600);
        while agent.supervisor.health_checker_mut().pop_due(far).is_some() {}
        // The process is killed while the probe is in flight.
        let instance = agent.supervisor.get_instance_mut(&id).unwrap();
        instance.state = ContainerState::Pending;
        let created_at = instance.created_at;
        agent
            .complete_health_probe(
                id.clone(),
                created_at,
                Ok(super::super::health::HealthStatus::Unhealthy),
            )
            .await;
        assert_eq!(
            agent
                .supervisor
                .health_checker_mut()
                .pop_due(far)
                .map(|(due, _)| due),
            Some(id.clone()),
            "the late probe dropped the check, so the restart would never be probed"
        );
        agent.retire_workload("web", "default").await.unwrap();
    }

    #[tokio::test]
    async fn deployment_refuses_backend_overflow_without_losing_runtime_owners() {
        let (mut agent, _commands, _shutdown, grill) = test_agent_with_grill();
        let mut config = basic_config();
        config.app.get_mut("web").unwrap().replicas =
            crate::config::Replicas::Fixed(crate::onion::types::MAX_BACKENDS as u32 + 1);
        let events = drain_deploy(&mut agent, config).await;
        let owners: std::collections::HashSet<_> = agent
            .supervisor
            .list_instances()
            .iter()
            .map(|instance| instance.id.clone())
            .collect();
        let unowned: Vec<_> = grill
            .calls()
            .into_iter()
            .filter(|(call, id)| call == "create" && !owners.contains(id))
            .collect();
        agent.retire_workload("web", "default").await.unwrap();
        assert!(
            unowned.is_empty(),
            "created runtimes lost their cleanup owner: {unowned:?}"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, ApplyEvent::Complete { .. })),
            "deployment completed despite refusing an endpoint: {events:?}"
        );
        assert!(events.iter().any(|event| matches!(event, ApplyEvent::Error { message } if message.contains("cannot publish backend"))));
    }

    #[tokio::test]
    async fn health_transitions_publish_the_confirmed_userspace_view() {
        let (mut agent, _commands, _shutdown) = test_agent();
        let view = agent.service_map_watch();
        expect_complete(&drain_deploy(&mut agent, basic_config()).await);
        let id = InstanceId("default__web-0".into());
        let service = crate::onion::service_id::ServiceId::new("default", "web");
        let mut health = super::super::health::HealthCheckConfig::from_spec(
            config_with_health().app["web"].health.as_ref().unwrap(),
            8080,
        );
        health.threshold_unhealthy = 1;
        let instance = agent.supervisor.get_instance_mut(&id).unwrap();
        instance.health_config = Some(health);
        instance.restart_policy.max_restarts = Some(0);
        let created_at = instance.created_at;
        agent
            .complete_health_probe(
                id.clone(),
                created_at,
                Ok(super::super::health::HealthStatus::Unhealthy),
            )
            .await;
        assert!(
            !view.borrow().resolve(&service).unwrap().backends[0].healthy,
            "DNS/ingress retained a backend after its health withdrawal"
        );
        agent
            .complete_health_probe(
                id,
                created_at,
                Ok(super::super::health::HealthStatus::Healthy),
            )
            .await;
        assert!(view.borrow().resolve(&service).unwrap().backends[0].healthy);
    }

    #[tokio::test]
    async fn a_later_probe_retries_publication_before_starting_restart() {
        let (mut agent, _commands, _shutdown) = test_agent();
        let view = agent.service_map_watch();
        expect_complete(&drain_deploy(&mut agent, basic_config()).await);
        let id = InstanceId("default__web-0".into());
        let service = crate::onion::service_id::ServiceId::new("default", "web");
        let mut health = super::super::health::HealthCheckConfig::from_spec(
            config_with_health().app["web"].health.as_ref().unwrap(),
            8080,
        );
        health.threshold_unhealthy = 1;
        let instance = agent.supervisor.get_instance_mut(&id).unwrap();
        instance.health_config = Some(health);
        let created_at = instance.created_at;
        let original = agent.service_map.clone();
        // Missing original allocation must refuse publication. Restore the same
        // evidence before retrying, rather than inventing a replacement VIP.
        agent.service_map = crate::onion::service_map::ServiceMap::new();
        agent
            .complete_health_probe(
                id.clone(),
                created_at,
                Ok(super::super::health::HealthStatus::Unhealthy),
            )
            .await;
        let instance = agent.supervisor.get_instance(&id).unwrap();
        assert_eq!(instance.state, ContainerState::Unhealthy);
        assert_eq!(instance.restart_count, 0);
        assert!(view.borrow().resolve(&service).unwrap().backends[0].healthy);
        agent.service_map = original;
        agent
            .complete_health_probe(
                id.clone(),
                created_at,
                Ok(super::super::health::HealthStatus::Unhealthy),
            )
            .await;
        let instance = agent.supervisor.get_instance(&id).unwrap();
        assert_eq!(instance.state, ContainerState::Pending);
        assert_eq!(instance.restart_count, 1);
        assert!(!view.borrow().resolve(&service).unwrap().backends[0].healthy);
    }

    #[tokio::test]
    async fn replacement_publication_refuses_a_closed_agent_channel() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let ops = DeployOps { tx };
        let result = ops
            .publish_new_backend(
                "api",
                "default",
                &InstanceId("default__api-g1-0".into()),
                Some(8080),
                None,
                true,
            )
            .await;
        assert!(matches!(result, Err(BunError::BackendPublication { .. })));
    }

    #[tokio::test]
    async fn replacement_publication_refuses_missing_service_or_port() {
        let (mut agent, _commands, _shutdown) = test_agent();
        let id = InstanceId("default__api-g1-0".into());
        let service = crate::onion::service_id::ServiceId::new("default", "api");
        let view = agent.service_map_watch();
        assert!(
            agent
                .publish_new_backend("api", "default", &id, Some(8080), None, true)
                .await
                .is_err()
        );
        agent.service_map.register(&service, 8080, None).unwrap();
        assert!(
            agent
                .publish_new_backend("api", "default", &id, None, None, true)
                .await
                .is_err()
        );
        assert!(
            agent
                .service_map
                .resolve(&service)
                .unwrap()
                .backends
                .is_empty()
        );
        assert!(view.borrow().resolve(&service).is_none());
    }

    /// The stop-confirmation deadline test agents use: the pre-configuration
    /// constant, well under the timeouts the stall tests assert against.
    const TEST_STOP_CONFIRMATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

    fn test_agent_with_grill() -> (
        TestAgent,
        mpsc::Sender<AgentCommand>,
        CancellationToken,
        MockGrill,
    ) {
        let (tx, rx) = mpsc::channel(32);
        let shutdown = CancellationToken::new();
        let grill = MockGrill::new();
        let grill_handle = grill.clone();
        let port_allocator = PortAllocator::new(30000, 31000);
        let mut agent = BunAgent::new(grill, port_allocator, rx, shutdown.clone());
        let volumes = tempfile::tempdir().unwrap();
        agent.set_volumes_dir(volumes.path().to_path_buf());
        // MockGrill answers instantly unless a test stalls it, so a short
        // deadline keeps injected stalls fast without changing any outcome.
        agent.set_stop_confirmation_timeout(TEST_STOP_CONFIRMATION_TIMEOUT);
        let agent = TestAgent {
            agent,
            _volumes: volumes,
        };
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
            local_node_id: crate::meat::NodeId::new("test"),
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
            self.deploy_with_rerun(config, events, false).await;
        }

        async fn deploy_with_rerun(
            &mut self,
            config: Config,
            events: &mpsc::Sender<ApplyEvent>,
            rerun_unknown_jobs: bool,
        ) {
            let worker = DeployWorker {
                rerun_unknown_jobs,
                grill: self.supervisor.grill().clone(),
                ops: DeployOps {
                    tx: self.deploy_ops_tx.clone(),
                },
                drains: self.drains.clone(),
                operation: None,
                stop_confirmation_timeout: self.stop_confirmation_timeout,
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

    #[tokio::test]
    async fn lease_storage_waits_for_confirmed_runtime_retirement() {
        use crate::testkit::lease::{
            LeasedResource, LocalLeaseStore, TestLease, cleanup_local_lease,
        };
        let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
        let volumes = tempfile::tempdir().unwrap();
        agent.set_volumes_dir(volumes.path().to_path_buf());
        // The runtime ignores SIGTERM on purpose; a short grace reaches the
        // unconfirmed kill without waiting out the production ten seconds.
        agent.set_stop_grace(std::time::Duration::from_millis(200));
        let task = tokio::spawn(async move { agent.run().await });
        let config = Config::parse("[app.web]\nimage = 'test:v1'\nnamespace = 'rbtest-cleanup'\n[app.web.deploy]\ndrain_timeout = '0s'\n[[app.web.volumes]]\npath = '/data'\n").unwrap();
        expect_complete(&send_deploy(&tx, config).await);
        let marker = volumes.path().join("rbtest-cleanup/web/data/marker");
        std::fs::write(&marker, "live").unwrap();
        let store = LocalLeaseStore::in_memory();
        let mut lease = TestLease::new(
            "cleanup".into(),
            "owner".into(),
            "owner".into(),
            "rbtest-cleanup".into(),
            1,
            2,
        )
        .unwrap();
        lease.resources.insert(LeasedResource::App {
            app_id: crate::meat::AppId::new("web", "rbtest-cleanup"),
        });
        store.create(lease).await.unwrap();
        grill.set_ignore_stop(true);
        grill.set_ignore_kill(true);
        let refused = cleanup_local_lease(&store, &tx, "cleanup", Some("owner")).await;
        assert!(refused.is_err());
        assert!(store.get("cleanup").await.is_some());
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "live");
        assert!(
            volumes
                .path()
                .join(".test-storage/rbtest-cleanup__web.checkpoint")
                .exists()
        );
        grill.set_ignore_stop(false);
        grill.set_ignore_kill(false);
        cleanup_local_lease(&store, &tx, "cleanup", Some("owner"))
            .await
            .unwrap();
        assert!(!marker.exists());
        assert!(store.get("cleanup").await.is_none());
        shutdown.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn lease_retirement_removes_only_owned_test_volumes_after_stop_preserves_them() {
        use crate::testkit::lease::{
            LeasedResource, LocalLeaseStore, TestLease, cleanup_local_lease,
        };
        let (mut agent, tx, shutdown) = test_agent();
        let volumes = tempfile::tempdir().unwrap();
        agent.set_volumes_dir(volumes.path().to_path_buf());
        let task = tokio::spawn(async move { agent.run().await });
        for namespace in ["rbtest-cleanup", "default"] {
            let config = Config::parse(&format!(
                "[app.web]\nimage = 'test:v1'\nnamespace = '{namespace}'\n[[app.web.volumes]]\npath = '/data'\n"
            )).unwrap();
            expect_complete(&send_deploy(&tx, config).await);
            std::fs::write(
                volumes.path().join(namespace).join("web/data/marker"),
                namespace,
            )
            .unwrap();
        }
        let (response, stopped) = oneshot::channel();
        tx.send(AgentCommand::Stop {
            app_name: "web".into(),
            namespace: "rbtest-cleanup".into(),
            response,
        })
        .await
        .unwrap();
        stopped.await.unwrap().unwrap();
        assert!(
            volumes
                .path()
                .join("rbtest-cleanup/web/data/marker")
                .is_file()
        );
        let store = LocalLeaseStore::in_memory();
        let mut lease = TestLease::new(
            "cleanup".into(),
            "owner".into(),
            "owner".into(),
            "rbtest-cleanup".into(),
            1,
            2,
        )
        .unwrap();
        lease.resources.insert(LeasedResource::App {
            app_id: crate::meat::AppId::new("web", "rbtest-cleanup"),
        });
        store.create(lease).await.unwrap();
        let cleanup = cleanup_local_lease(&store, &tx, "cleanup", Some("owner")).await;
        shutdown.cancel();
        task.await.unwrap();
        cleanup.unwrap();
        assert!(!volumes.path().join("rbtest-cleanup/web/data").exists());
        assert_eq!(
            std::fs::read_to_string(volumes.path().join("default/web/data/marker")).unwrap(),
            "default"
        );
        assert!(store.get("cleanup").await.is_none());
    }

    #[tokio::test]
    async fn lease_cleanup_retires_owned_instances_without_erasing_another_namespace() {
        use crate::testkit::lease::{
            LeasedResource, LocalLeaseStore, TestLease, cleanup_local_lease,
        };
        let (mut agent, tx, shutdown) = test_agent();
        let volumes = tempfile::tempdir().unwrap();
        agent.set_volumes_dir(volumes.path().to_path_buf());
        let task = tokio::spawn(async move { agent.run().await });
        for namespace in ["rbtest-cleanup", "rbtest-keep"] {
            let config = Config::parse(&format!(
                "[app.web]\nimage = 'test:v1'\nnamespace = '{namespace}'\n"
            ))
            .unwrap();
            expect_complete(&send_deploy(&tx, config).await);
        }
        let store = LocalLeaseStore::in_memory();
        let mut lease = TestLease::new(
            "cleanup".into(),
            "owner".into(),
            "owner".into(),
            "rbtest-cleanup".into(),
            1,
            2,
        )
        .unwrap();
        lease.resources.insert(LeasedResource::App {
            app_id: crate::meat::AppId::new("web", "rbtest-cleanup"),
        });
        store.create(lease).await.unwrap();
        cleanup_local_lease(&store, &tx, "cleanup", Some("owner"))
            .await
            .unwrap();
        let (response, result) = oneshot::channel();
        tx.send(AgentCommand::Status { response }).await.unwrap();
        let instances = result.await.unwrap();
        shutdown.cancel();
        task.await.unwrap();
        assert!(store.get("cleanup").await.is_none());
        assert_eq!(
            instances.len(),
            1,
            "cleanup must retire its status record too"
        );
        assert_eq!(instances[0].namespace, "rbtest-keep");
        assert_eq!(instances[0].state, "running");
    }

    #[tokio::test]
    async fn cron_registration_and_stop_survive_agent_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let records = directory.path().join("instances");
        let (mut agent, tx, _shutdown) = test_agent();
        agent.set_records_dir(records.clone());
        agent.set_volumes_dir(directory.path().join("volumes"));
        let task = tokio::spawn(async move { agent.run().await });
        for namespace in ["red", "blue"] {
            let config = Config::parse(&format!(
                "[job.backup]\nimage = 'test:v1'\nschedule = '0 0 30 2 *'\nnamespace = '{namespace}'\n"
            )).unwrap();
            expect_complete(&send_deploy(&tx, config).await);
        }
        let (response, stopped) = oneshot::channel();
        tx.send(AgentCommand::Stop {
            app_name: "backup".into(),
            namespace: "red".into(),
            response,
        })
        .await
        .unwrap();
        stopped.await.unwrap().unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());

        let (mut replacement, tx, shutdown) = test_agent();
        replacement.set_records_dir(records);
        replacement.set_volumes_dir(directory.path().join("volumes"));
        replacement.adopt_recorded_instances().await.unwrap();
        let task = tokio::spawn(async move { replacement.run().await });
        let conflicting =
            Config::parse("[app.backup]\nimage = 'test:v1'\nnamespace = 'blue'\n").unwrap();
        let events = send_deploy(&tx, conflicting).await;
        assert!(events.iter().any(|event| matches!(event, ApplyEvent::Error { message } if message.contains("registered cron job"))));
        for (namespace, exists) in [("red", false), ("blue", true)] {
            let (response, stopped) = oneshot::channel();
            tx.send(AgentCommand::Stop {
                app_name: "backup".into(),
                namespace: namespace.into(),
                response,
            })
            .await
            .unwrap();
            let result = stopped.await.unwrap();
            assert_eq!(result.is_ok(), exists, "{namespace}: {result:?}");
        }
        shutdown.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn cron_claim_is_durable_before_launch_and_is_not_repeated_after_crash() {
        let seconds = time::OffsetDateTime::now_utc().second();
        if seconds >= 50 {
            tokio::time::sleep(std::time::Duration::from_secs(u64::from(61 - seconds))).await;
        }
        let directory = tempfile::tempdir().unwrap();
        let records = directory.path().join("instances");
        let (mut agent, tx, _shutdown, grill) = test_agent_with_grill();
        agent.set_records_dir(records.clone());
        agent.set_volumes_dir(directory.path().join("volumes"));
        grill.block_creates();
        let task = tokio::spawn(async move { agent.run().await });
        let config =
            Config::parse("[job.once]\nimage = 'test:v1'\nschedule = '* * * * *'\n").unwrap();
        expect_complete(&send_deploy(&tx, config).await);
        tokio::time::timeout(std::time::Duration::from_secs(3), grill.wait_for_creates(1))
            .await
            .unwrap();
        let checkpoint: serde_json::Value = serde_json::from_slice(
            &std::fs::read(records.join("scheduled-jobs.checkpoint")).unwrap(),
        )
        .unwrap();
        let claimed = checkpoint["jobs"][0]["last_fired_minute"].as_i64().unwrap();
        assert_eq!(
            claimed,
            time::OffsetDateTime::now_utc()
                .unix_timestamp()
                .div_euclid(60)
        );
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        grill.release_creates(1);

        let (mut replacement, _tx, shutdown, runtime) = test_agent_with_grill();
        replacement.set_records_dir(records);
        replacement.set_volumes_dir(directory.path().join("volumes"));
        replacement.adopt_recorded_instances().await.unwrap();
        let task = tokio::spawn(async move { replacement.run().await });
        tokio::time::sleep(std::time::Duration::from_millis(2200)).await;
        shutdown.cancel();
        task.await.unwrap();
        assert_eq!(
            claimed,
            time::OffsetDateTime::now_utc()
                .unix_timestamp()
                .div_euclid(60),
            "fixture crossed the minute boundary"
        );
        assert!(
            !runtime
                .calls()
                .iter()
                .any(|(operation, _)| operation == "create"),
            "recovery repeated a claimed firing"
        );
    }

    #[tokio::test]
    async fn failed_cron_stop_retains_checkpoint_and_fences_later_changes() {
        let directory = tempfile::tempdir().unwrap();
        let records = directory.path().join("instances");
        let (mut agent, tx, shutdown) = test_agent();
        agent.set_records_dir(records.clone());
        agent.set_volumes_dir(directory.path().join("volumes"));
        let task = tokio::spawn(async move { agent.run().await });
        let config =
            Config::parse("[job.backup]\nimage = 'test:v1'\nschedule = '0 0 30 2 *'\n").unwrap();
        expect_complete(&send_deploy(&tx, config.clone()).await);
        let saved = directory.path().join("saved");
        std::fs::rename(&records, &saved).unwrap();
        std::fs::write(&records, "blocked").unwrap();
        let (response, stopped) = oneshot::channel();
        tx.send(AgentCommand::Stop {
            app_name: "backup".into(),
            namespace: "default".into(),
            response,
        })
        .await
        .unwrap();
        assert!(stopped.await.unwrap().is_err());
        std::fs::remove_file(&records).unwrap();
        std::fs::rename(&saved, &records).unwrap();
        let events = send_deploy(&tx, config).await;
        assert!(events.iter().any(|event| matches!(event, ApplyEvent::Error { message } if message.contains("previous write is uncertain"))));
        shutdown.cancel();
        task.await.unwrap();
        let (mut replacement, _tx, _shutdown) = test_agent();
        replacement.set_records_dir(records);
        replacement.set_volumes_dir(directory.path().join("volumes"));
        replacement.adopt_recorded_instances().await.unwrap();
        assert!(
            replacement
                .scheduled_jobs
                .contains_key(&("backup".into(), "default".into()))
        );
    }

    #[tokio::test]
    async fn cron_checkpoint_corruption_refuses_startup() {
        let job = serde_json::json!({"name":"backup", "namespace":"default", "spec":{"image":"test:v1", "schedule":"* * * * *"}, "last_fired_minute":null});
        let mut wrong_namespace = job.clone();
        wrong_namespace["namespace"] = "other".into();
        let mut invalid_schedule = job.clone();
        invalid_schedule["spec"]["schedule"] = "bad".into();
        let mut invalid_stamp = job.clone();
        invalid_stamp["last_fired_minute"] = (-1).into();
        for contents in [
            "{broken".to_string(),
            serde_json::json!({"schema":999,"jobs":[]}).to_string(),
            serde_json::json!({"schema":1,"jobs":[job.clone(),job]}).to_string(),
            serde_json::json!({"schema":1,"jobs":[wrong_namespace]}).to_string(),
            serde_json::json!({"schema":1,"jobs":[invalid_schedule]}).to_string(),
            serde_json::json!({"schema":1,"jobs":[invalid_stamp]}).to_string(),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("scheduled-jobs.checkpoint");
            std::fs::write(&path, &contents).unwrap();
            let (mut agent, _tx, _shutdown) = test_agent();
            agent.set_records_dir(directory.path().to_path_buf());
            agent.set_volumes_dir(directory.path().join("volumes"));
            assert!(
                agent.adopt_recorded_instances().await.is_err(),
                "invalid checkpoint was accepted: {contents}"
            );
            assert_eq!(std::fs::read_to_string(path).unwrap(), contents);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cron_checkpoint_refuses_symlinks_and_nonregular_files() {
        let directory = tempfile::tempdir().unwrap();
        let checkpoint = directory.path().join("scheduled-jobs.checkpoint");
        let source = directory.path().join("source");
        std::fs::write(&source, r#"{"schema":1,"jobs":[]}"#).unwrap();
        for kind in 0..3 {
            match kind {
                0 => std::os::unix::fs::symlink(&source, &checkpoint).unwrap(),
                1 => nix::unistd::mkfifo(&checkpoint, nix::sys::stat::Mode::S_IRUSR).unwrap(),
                _ => std::fs::create_dir(&checkpoint).unwrap(),
            }
            let (mut agent, _tx, _shutdown) = test_agent();
            agent.set_records_dir(directory.path().to_path_buf());
            agent.set_volumes_dir(directory.path().join("volumes"));
            assert!(
                tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    agent.adopt_recorded_instances()
                )
                .await
                .unwrap()
                .is_err()
            );
            if kind == 2 {
                std::fs::remove_dir(&checkpoint).unwrap();
            } else {
                std::fs::remove_file(&checkpoint).unwrap();
            }
        }
    }

    #[tokio::test]
    async fn cron_registration_refuses_when_ownership_cannot_be_persisted() {
        let directory = tempfile::tempdir().unwrap();
        let records = directory.path().join("instances");
        std::fs::write(&records, "not a directory").unwrap();
        let (mut agent, tx, shutdown) = test_agent();
        agent.set_records_dir(records);
        agent.set_volumes_dir(directory.path().join("volumes"));
        let task = tokio::spawn(async move { agent.run().await });
        let config =
            Config::parse("[job.backup]\nimage = 'test:v1'\nschedule = '0 0 30 2 *'\n").unwrap();
        let events = send_deploy(&tx, config).await;
        let (response, stopped) = oneshot::channel();
        tx.send(AgentCommand::Stop {
            app_name: "backup".into(),
            namespace: "default".into(),
            response,
        })
        .await
        .unwrap();
        assert!(
            matches!(stopped.await.unwrap(), Err(BunError::ScheduleState(_))),
            "an uncertain new registration must retain ownership"
        );
        shutdown.cancel();
        task.await.unwrap();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ApplyEvent::Error { .. })),
            "schedule was acknowledged without durable ownership: {events:?}"
        );
    }

    #[tokio::test]
    async fn reapplying_a_job_without_schedule_retires_only_its_previous_cron() {
        let (mut agent, tx, shutdown) = test_agent();
        let task = tokio::spawn(async move {
            agent.run().await;
            agent
        });
        for namespace in ["red", "blue"] {
            let config = Config::parse(&format!(
                "[job.backup]\nimage = 'test:v1'\nschedule = '0 0 30 2 *'\nnamespace = '{namespace}'\n"
            )).unwrap();
            expect_complete(&send_deploy(&tx, config).await);
        }
        let config = Config::parse("[job.backup]\nimage = 'test:v2'\nnamespace = 'red'\n").unwrap();
        expect_complete(&send_deploy(&tx, config).await);
        shutdown.cancel();
        let agent = task.await.unwrap();
        assert!(
            !agent
                .scheduled_jobs
                .contains_key(&("backup".into(), "red".into())),
            "removing schedule from the applied job must retire its old cron"
        );
        assert!(
            agent
                .scheduled_jobs
                .contains_key(&("backup".into(), "blue".into()))
        );
    }

    #[tokio::test]
    async fn stopping_a_scheduled_job_before_its_first_run_retires_only_its_namespace() {
        let (tx, rx) = mpsc::channel(32);
        let shutdown = CancellationToken::new();
        let mut agent = BunAgent::new(
            crate::grill::mock::MockGrill::new(),
            PortAllocator::new(30000, 31000),
            rx,
            shutdown.clone(),
        );
        let task = tokio::spawn(async move {
            agent.run().await;
            agent
        });
        for namespace in ["red", "blue"] {
            // February 30 never matches, so this tests the pre-first-run path
            // without depending on which minute CI happens to execute it.
            let config = Config::parse(&format!(
                "[job.backup]\nimage = \"busybox:latest\"\ncommand = [\"true\"]\nschedule = \"0 0 30 2 *\"\nnamespace = \"{namespace}\"\n"
            )).unwrap();
            let events = send_deploy(&tx, config).await;
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, ApplyEvent::Complete { .. })),
                "{events:?}"
            );
        }
        let (response, result) = oneshot::channel();
        tx.send(AgentCommand::Stop {
            app_name: "backup".into(),
            namespace: "red".into(),
            response,
        })
        .await
        .unwrap();
        let stopped = result.await.unwrap();
        shutdown.cancel();
        let agent = task.await.unwrap();
        assert!(
            stopped.is_ok(),
            "stopping a registered schedule failed: {stopped:?}"
        );
        assert!(
            !agent
                .scheduled_jobs
                .contains_key(&("backup".into(), "red".into()))
        );
        assert!(
            agent
                .scheduled_jobs
                .contains_key(&("backup".into(), "blue".into()))
        );
        assert!(agent.supervisor.list_instances().is_empty());
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

    // --- relish sign, end to end (operator key → attach → deploy gate) ---

    /// A single-node leader council, enough to hold a manifest catalogue.
    async fn catalogue_council(raft_port: u16) -> Arc<CouncilNode> {
        use crate::council::log_store::MemLogStore;
        use crate::council::network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter};
        use crate::council::state_machine::CouncilStateMachine;
        use crate::council::types::CouncilConfig;

        let router = InMemoryRaftRouter::new();
        let network = InMemoryRaftNetworkFactory::new(1, router.clone());
        let node = CouncilNode::new(
            1,
            CouncilConfig::default(),
            network,
            MemLogStore::new(),
            CouncilStateMachine::new(),
            None,
        )
        .await
        .unwrap();
        router.register(1, node.raft().clone()).await;
        let address = std::net::SocketAddr::from(([127, 0, 0, 1], raft_port));
        node.initialize(std::collections::BTreeMap::from([(
            1u64,
            CouncilNodeInfo::new(address, "node-1".to_string()),
        )]))
        .await
        .unwrap();
        for _ in 0..40 {
            if node.is_leader().await {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        Arc::new(node)
    }

    /// Push (commit) an unsigned manifest `repository:tag` with `digest_hex`.
    async fn push_manifest(council: &CouncilNode, repository: &str, tag: &str, digest_hex: &str) {
        use crate::pickle::types::{Digest, ImageManifest, LayerDescriptor, ManifestCommit};
        let commit = ManifestCommit {
            observed_gc_generation: 0,
            manifest: ImageManifest {
                digest: Digest::from_sha256_hex(digest_hex),
                config: LayerDescriptor {
                    digest: Digest::from_sha256_hex(&"c".repeat(64)),
                    size: 100,
                    media_type: "application/vnd.oci.image.config.v1+json".to_string(),
                },
                layers: vec![],
                repository: repository.to_string(),
                tags: std::collections::BTreeSet::new(),
                total_size: 100,
                pushed_at: std::time::SystemTime::UNIX_EPOCH,
                pushed_by: 1,
                signature: None,
            },
            tag: tag.to_string(),
            holder_nodes: std::collections::BTreeSet::from([1]),
        };
        let response = council
            .write(crate::council::RaftRequest::ManifestCommit(commit))
            .await
            .unwrap();
        assert!(
            matches!(
                response,
                crate::council::types::CouncilResponse::Applied { .. }
            ),
            "manifest commit: {response:?}"
        );
    }

    fn agent_with_council(council: Arc<CouncilNode>) -> BunAgent<MockGrill> {
        let (_membership_tx, membership_rx) = tokio::sync::watch::channel(Vec::new());
        let (_snapshot_tx, snapshot_rx) = mpsc::channel(1);
        let (_command_tx, command_rx) = mpsc::channel(8);
        let cluster = ClusterHandle {
            local_node_id: crate::meat::NodeId::new("node-1"),
            membership_rx,
            raft_metrics_rx: None,
            council: Some(council),
            snapshot_rx,
            wrapping_ikm: None,
            partition_blocklists: PartitionBlocklists::default(),
            crl_handle: Default::default(),
        };
        BunAgent::with_cluster(
            MockGrill::new(),
            PortAllocator::new(30000, 31000),
            command_rx,
            CancellationToken::new(),
            cluster,
            "test".to_string(),
        )
    }

    /// Do what `relish sign IMAGE --key KEY` does against this council:
    /// resolve the reference through the image listing, sign the digest
    /// locally, and hand the submission to the node.
    async fn relish_sign(
        agent: &BunAgent<MockGrill>,
        council: &CouncilNode,
        image: &str,
        key: &crate::pickle::signing::SigningKey,
    ) -> Result<String, BunError> {
        let images = council.manifest_catalog().await.images();
        let digest = crate::relish::commands::resolve_image_digest(image, &images).unwrap();
        agent.handle_sign_image(key.sign(&digest).unwrap()).await
    }

    fn app(image: &str) -> AppSpec {
        toml::from_str(&format!("image = {image:?}")).unwrap()
    }

    #[tokio::test]
    async fn relish_signed_image_is_admitted_only_under_a_policy_trusting_its_key() {
        let council = catalogue_council(9301).await;
        let signed = "1".repeat(64);
        push_manifest(&council, "myapp", "v1", &signed).await;
        push_manifest(&council, "unsigned", "v1", &"2".repeat(64)).await;
        push_manifest(&council, "stranger", "v1", &"3".repeat(64)).await;
        let mut agent = agent_with_council(council.clone());

        let operator = crate::pickle::signing::SigningKey::generate().unwrap();
        let stranger = crate::pickle::signing::SigningKey::generate().unwrap();
        let message = relish_sign(&agent, &council, "myapp:v1", &operator)
            .await
            .unwrap();
        assert!(message.contains(&format!("sha256:{signed}")), "{message}");
        assert!(
            message.contains("does not list this key"),
            "an untrusted key must be called out: {message}"
        );
        relish_sign(&agent, &council, "stranger:v1", &stranger)
            .await
            .unwrap();

        agent.set_trust_policy(crate::config::node::TrustPolicySection {
            require_signatures: true,
            keys: vec![operator.public_key_base64()],
        });

        // Signed with the trusted key: admitted, pinned to the signed digest.
        let pinned = agent.enforce_image_signature(&app("myapp:v1")).await;
        assert_eq!(pinned, Ok(Some(format!("myapp@sha256:{signed}"))));
        // Never signed: refused.
        let unsigned = agent.enforce_image_signature(&app("unsigned:v1")).await;
        assert!(unsigned.is_err(), "unsigned image admitted: {unsigned:?}");
        // Signed, but by a key the policy doesn't list: refused.
        let untrusted = agent.enforce_image_signature(&app("stranger:v1")).await;
        assert!(
            untrusted
                .as_ref()
                .is_err_and(|reason| reason.contains("not in trust policy")),
            "other-key image admitted: {untrusted:?}"
        );

        // With the trusted key listed, signing reports no warning.
        let message = relish_sign(&agent, &council, "myapp:v1", &operator)
            .await
            .unwrap();
        assert!(!message.contains("warning"), "{message}");
    }

    #[tokio::test]
    async fn moving_a_tag_after_signing_leaves_the_new_digest_unsigned() {
        let council = catalogue_council(9302).await;
        push_manifest(&council, "myapp", "v1", &"1".repeat(64)).await;
        let mut agent = agent_with_council(council.clone());
        let operator = crate::pickle::signing::SigningKey::generate().unwrap();
        relish_sign(&agent, &council, "myapp:v1", &operator)
            .await
            .unwrap();
        agent.set_trust_policy(crate::config::node::TrustPolicySection {
            require_signatures: true,
            keys: vec![operator.public_key_base64()],
        });

        // Someone re-pushes v1 with different bytes: the signature covered
        // the old digest, not the tag, so the new content is refused.
        push_manifest(&council, "myapp", "v1", &"4".repeat(64)).await;
        let result = agent.enforce_image_signature(&app("myapp:v1")).await;
        assert!(result.is_err(), "re-tagged content admitted: {result:?}");
    }

    #[tokio::test]
    async fn signing_a_digest_the_catalogue_does_not_hold_is_refused() {
        let council = catalogue_council(9303).await;
        let agent = agent_with_council(council);
        let operator = crate::pickle::signing::SigningKey::generate().unwrap();
        let digest = crate::pickle::types::Digest::from_sha256_hex(&"5".repeat(64));
        let result = agent
            .handle_sign_image(operator.sign(&digest).unwrap())
            .await;
        assert!(
            matches!(&result, Err(BunError::SecurityError { reason }) if reason.contains("refused")),
            "got: {result:?}"
        );
    }

    #[tokio::test]
    async fn shutdown_escalates_to_kill_when_stop_is_ignored() {
        let (_tx, rx) = mpsc::channel(8);
        let shutdown = CancellationToken::new();
        let grill = MockGrill::new();
        let grill_handle = grill.clone();
        let port_allocator = PortAllocator::new(30000, 31000);
        let mut agent = BunAgent::new(grill, port_allocator, rx, shutdown);
        // Escalation is under test, not the length of the production grace.
        agent.set_shutdown_grace(std::time::Duration::from_millis(200));

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
    async fn slow_health_probe_does_not_block_status_or_shutdown() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (_socket, _) = listener.accept().await.unwrap();
            let _ = started_tx.send(());
            std::future::pending::<()>().await;
        });
        let (mut agent, tx, shutdown) = test_agent();
        let volumes = tempfile::tempdir().unwrap();
        agent.set_volumes_dir(volumes.path().to_path_buf());
        let task = tokio::spawn(async move { agent.run().await });
        let config = Config::parse(&format!(
            r#"[app.web]
image = "test:v1"
port = {port}
[app.web.health]
path = "/health"
timeout = 3
interval = 1
"#
        ))
        .unwrap();
        send_deploy(&tx, config).await;
        tokio::time::timeout(std::time::Duration::from_secs(3), started_rx)
            .await
            .unwrap()
            .unwrap();
        let (response, received) = tokio::sync::oneshot::channel();
        tx.send(AgentCommand::Status { response }).await.unwrap();
        let status = tokio::time::timeout(std::time::Duration::from_millis(500), received).await;
        shutdown.cancel();
        let mut task = task;
        let stopped = tokio::time::timeout(std::time::Duration::from_millis(500), &mut task).await;
        task.abort();
        server.abort();
        assert!(
            status.is_ok(),
            "a slow health probe blocked the command loop"
        );
        assert!(stopped.is_ok(), "a slow health probe blocked shutdown");
    }

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
    async fn cancellation_waits_for_in_flight_create_before_releasing_ownership() {
        let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
        let task = tokio::spawn(async move { agent.run().await });
        grill.block_creates();
        let (events, mut stream) = mpsc::channel(64);
        tx.send(AgentCommand::Deploy {
            config: basic_config(),
            events,
        })
        .await
        .unwrap();
        let ApplyEvent::Accepted { operation_id } = stream.recv().await.unwrap() else {
            panic!("no ID")
        };
        tokio::time::timeout(std::time::Duration::from_secs(2), grill.wait_for_creates(1))
            .await
            .unwrap();
        let (response, result) = oneshot::channel();
        tx.send(AgentCommand::CancelDeploy {
            operation_id: operation_id.clone().into(),
            response,
        })
        .await
        .unwrap();
        let receipt = result.await.unwrap().unwrap();
        assert!(receipt.cancellation_requested_at.is_some());
        assert!(receipt.outcome.is_none());
        let conflict = send_deploy(&tx, basic_config()).await;
        assert!(conflict.iter().any(|event| matches!(event, ApplyEvent::Error { message } if message.contains(&operation_id))));
        grill.release_creates(1);
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while stream.recv().await.is_some() {}
        })
        .await
        .unwrap();
        let (response, result) = oneshot::channel();
        tx.send(AgentCommand::DeployOperations { response })
            .await
            .unwrap();
        assert_eq!(
            result.await.unwrap().history[0].outcome,
            Some(crate::bun::deploy_operations::DeployOperationOutcome::Cancelled)
        );
        expect_complete(&send_deploy(&tx, basic_config()).await);
        shutdown.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn cancellation_interrupts_health_wait_and_holds_ownership_through_rollback() {
        for strategy in ["rolling", "blue-green"] {
            let port = spawn_health_responder(500).await;
            let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
            let task = tokio::spawn(async move { agent.run().await });
            expect_complete(&send_deploy(&tx, no_health_config(port)).await);
            let mut config = health_gated_config(port, strategy);
            config
                .app
                .get_mut("web")
                .unwrap()
                .deploy
                .as_mut()
                .unwrap()
                .health_timeout = Some("30s".into());
            let (events, mut stream) = mpsc::channel(64);
            tx.send(AgentCommand::Deploy { config, events })
                .await
                .unwrap();
            let ApplyEvent::Accepted { operation_id } = stream.recv().await.unwrap() else {
                panic!("no ID")
            };
            let canary =
                crate::grill::InstanceIdentity::canary("default", "web", 1, 0).instance_id();
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while !grill
                    .calls()
                    .iter()
                    .any(|(call, id)| call == "start" && id == &canary)
                {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            grill.block_kills();
            let (response, result) = oneshot::channel();
            tx.send(AgentCommand::CancelDeploy {
                operation_id: operation_id.clone().into(),
                response,
            })
            .await
            .unwrap();
            assert!(
                result
                    .await
                    .unwrap()
                    .unwrap()
                    .cancellation_requested_at
                    .is_some()
            );
            tokio::time::timeout(std::time::Duration::from_secs(2), grill.wait_for_kills(1))
                .await
                .expect("cancellation did not interrupt the 30-second health wait");
            let (response, result) = oneshot::channel();
            tx.send(AgentCommand::DeployOperations { response })
                .await
                .unwrap();
            let snapshot = result.await.unwrap();
            grill.release_kills(1);
            assert!(
                snapshot
                    .active_deploys
                    .iter()
                    .any(|op| op.id.as_str() == operation_id)
            );
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while stream.recv().await.is_some() {}
            })
            .await
            .unwrap();
            let (response, result) = oneshot::channel();
            tx.send(AgentCommand::DeployOperations { response })
                .await
                .unwrap();
            assert_eq!(
                result.await.unwrap().history[0].outcome,
                Some(crate::bun::deploy_operations::DeployOperationOutcome::Cancelled)
            );
            expect_complete(&send_deploy(&tx, no_health_config(port)).await);
            shutdown.cancel();
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn app_deploy_does_not_roll_over_an_existing_job() {
        let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
        let task = tokio::spawn(async move { agent.run().await });
        let job = Config::parse("[job.web]\nimage = 'job:v1'\n").unwrap();
        expect_complete(&send_deploy(&tx, job).await);
        let events = send_deploy(&tx, basic_config()).await;
        let calls_before_shutdown = grill.calls();
        shutdown.cancel();
        task.await.unwrap();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ApplyEvent::Error { .. })),
            "an app rollout accepted a live job as its previous generation"
        );
        assert!(
            !calls_before_shutdown
                .iter()
                .any(|(call, _)| call == "stop" || call == "kill"),
            "the conflicting deploy changed the existing job"
        );
    }

    #[tokio::test]
    async fn failed_deploy_keeps_target_ownership_until_rollback_finishes() {
        for strategy in ["rolling", "blue-green"] {
            let port = spawn_health_responder(500).await;
            let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
            let task = tokio::spawn(async move { agent.run().await });
            expect_complete(&send_deploy(&tx, no_health_config(port)).await);
            grill.block_kills();
            let (events, mut event_rx) = mpsc::channel(64);
            tx.send(AgentCommand::Deploy {
                config: health_gated_config(port, strategy),
                events,
            })
            .await
            .unwrap();
            let operation_id = match event_rx.recv().await.unwrap() {
                ApplyEvent::Accepted { operation_id } => operation_id,
                event => panic!("expected acceptance, got {event:?}"),
            };
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                while let Some(event) = event_rx.recv().await {
                    if matches!(event, ApplyEvent::Error { .. }) {
                        break;
                    }
                }
                grill.wait_for_kills(1).await;
            })
            .await
            .expect("failed replacement did not enter cleanup");
            let (response, result) = oneshot::channel();
            tx.send(AgentCommand::DeployOperations { response })
                .await
                .unwrap();
            let snapshot = result.await.unwrap();
            // Release the fixture even when the assertion below fails.
            grill.release_kills(1);
            assert!(
                snapshot
                    .active_deploys
                    .iter()
                    .any(|op| op.id.as_str() == operation_id),
                "{strategy} released target ownership while rollback still owned its runtime mutation"
            );
            assert!(
                !snapshot
                    .history
                    .iter()
                    .any(|op| op.id.as_str() == operation_id)
            );
            while event_rx.recv().await.is_some() {}
            let (response, result) = oneshot::channel();
            tx.send(AgentCommand::DeployOperations { response })
                .await
                .unwrap();
            let snapshot = result.await.unwrap();
            assert_eq!(
                snapshot
                    .history
                    .iter()
                    .find(|op| op.id.as_str() == operation_id)
                    .unwrap()
                    .outcome,
                Some(crate::bun::deploy_operations::DeployOperationOutcome::Failed)
            );
            expect_complete(&send_deploy(&tx, no_health_config(port)).await);
            shutdown.cancel();
            task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn stalled_event_consumer_cannot_pin_deployment_ownership() {
        let (mut agent, tx, shutdown, _) = test_agent_with_grill();
        let task = tokio::spawn(async move { agent.run().await });
        // Acceptance fills this queue. Keep its receiver alive without reading.
        let (events, _event_rx) = mpsc::channel(1);
        tx.send(AgentCommand::Deploy {
            config: basic_config(),
            events,
        })
        .await
        .unwrap();
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                let (response, result) = oneshot::channel();
                tx.send(AgentCommand::DeployOperations { response })
                    .await
                    .unwrap();
                let snapshot = result.await.unwrap();
                if let Some(operation) = snapshot.history.first() {
                    break operation.outcome;
                }
                tokio::task::yield_now().await;
            }
        })
        .await;
        shutdown.cancel();
        task.await.unwrap();
        assert_eq!(
            outcome.expect("client backpressure prevented terminal history"),
            Some(crate::bun::deploy_operations::DeployOperationOutcome::Completed)
        );
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
        // The rolling gate (M5) probes the app's health endpoint before the
        // redeploy may complete, so give it one that answers 200.
        let port = spawn_health_responder(200).await;
        let (_tx, rx) = mpsc::channel(32);
        let shutdown = CancellationToken::new();
        let grill = crate::grill::process::ProcessGrill::new();
        let port_allocator = PortAllocator::new(30000, 31000);
        let volumes = tempfile::tempdir().unwrap();
        let mut agent = BunAgent::new(grill, port_allocator, rx, shutdown);
        agent.set_volumes_dir(volumes.path().to_path_buf());

        let config = Config::parse(&format!(
            "[app.web]\nimage = \"proc-grill:ignored\"\ncommand = [\"sleep\", \"60\"]\nport = {port}\n\n[app.web.health]\npath = \"/healthz\"\n\n[app.web.deploy]\nhealth_timeout = \"5s\"\n",
        ))
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

    async fn restart_preserves_uncertain_cleanup(inject: fn(&MockGrill)) {
        let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
        grill.set_pid(std::process::id());
        let directory = tempfile::tempdir().unwrap();
        agent.set_volumes_dir(directory.path().join("volumes"));
        let records = directory.path().join("instances");
        agent.set_records_dir(records.clone());
        let config = Config::parse("[app.restart]\nimage = \"mock:image\"\nport = 8080\n").unwrap();
        let (events, _received) = mpsc::channel(256);
        agent.deploy(config, &events).await;
        assert_eq!(
            agent.supervisor.list_instances()[0].state,
            ContainerState::Running
        );
        let id = agent.supervisor.list_instances()[0].id.clone();
        let port = agent.supervisor.get_instance(&id).unwrap().host_port;
        assert!(port.is_some());
        std::fs::create_dir_all(&records).unwrap();
        let record = crate::grill::records::record_path(&records, &id.0);
        std::fs::write(&record, "retained ownership").unwrap();
        agent.supervisor.get_instance_mut(&id).unwrap().state = ContainerState::Unhealthy;
        assert!(
            agent
                .supervisor
                .maybe_restart(&id, Instant::now())
                .await
                .unwrap()
        );
        let before = grill.calls().len();
        inject(&grill);
        tokio::time::timeout(
            std::time::Duration::from_secs(6),
            agent.drive_pending_restarts(),
        )
        .await
        .expect("restart cleanup stalled the agent");
        assert_eq!(
            agent.supervisor.get_instance(&id).unwrap().state,
            ContainerState::Pending
        );
        assert_eq!(agent.supervisor.get_instance(&id).unwrap().host_port, port);
        assert_eq!(
            std::fs::read_to_string(&record).unwrap(),
            "retained ownership"
        );
        assert!(
            !grill.calls()[before..]
                .iter()
                .any(|(operation, _)| operation == "create" || operation == "start")
        );

        grill.set_fail_kill(false);
        grill.set_ignore_kill(false);
        grill.set_fail_state(false);
        grill.release_kills(1);
        agent.drive_pending_restarts().await;
        assert_eq!(
            agent.supervisor.get_instance(&id).unwrap().state,
            ContainerState::Running
        );
        assert_eq!(agent.supervisor.get_instance(&id).unwrap().restart_count, 1);
        agent.stop_app("restart", "default").await.unwrap();
    }

    #[tokio::test]
    async fn restart_retains_owner_after_failed_kill() {
        restart_preserves_uncertain_cleanup(|grill| grill.set_fail_kill(true)).await;
    }

    #[tokio::test]
    async fn restart_retains_owner_after_unconfirmed_kill() {
        restart_preserves_uncertain_cleanup(|grill| grill.set_ignore_kill(true)).await;
    }

    #[tokio::test]
    async fn restart_retains_owner_after_failed_observation() {
        restart_preserves_uncertain_cleanup(|grill| grill.set_fail_state(true)).await;
    }

    #[tokio::test]
    async fn restart_retains_owner_after_stalled_kill() {
        restart_preserves_uncertain_cleanup(MockGrill::block_kills).await;
    }

    async fn failed_restart_fixture() -> (TestAgent, MockGrill, InstanceId, tempfile::TempDir) {
        let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
        let directory = tempfile::tempdir().unwrap();
        agent.set_volumes_dir(directory.path().join("volumes"));
        let config = Config::parse("[app.retry]\nimage = \"mock:image\"\nport = 8080\n").unwrap();
        let (events, _received) = mpsc::channel(256);
        agent.deploy(config, &events).await;
        let id = agent.supervisor.list_instances()[0].id.clone();
        agent.supervisor.get_instance_mut(&id).unwrap().state = ContainerState::Unhealthy;
        assert!(
            agent
                .supervisor
                .maybe_restart(&id, Instant::now())
                .await
                .unwrap()
        );
        (agent, grill, id, directory)
    }

    async fn failed_restart_recovers(inject: fn(&MockGrill)) {
        let (mut agent, grill, id, _directory) = failed_restart_fixture().await;
        let port = agent.supervisor.get_instance(&id).unwrap().host_port;
        inject(&grill);
        agent.drive_pending_restarts().await;
        assert_eq!(
            agent.supervisor.get_instance(&id).unwrap().state,
            ContainerState::Stopping
        );
        assert_eq!(agent.supervisor.get_instance(&id).unwrap().host_port, port);
        let creates = grill
            .calls()
            .iter()
            .filter(|(op, _)| op == "create")
            .count();
        grill.set_fail_kill(true);
        agent.drive_pending_restarts().await;
        assert_eq!(
            agent.supervisor.get_instance(&id).unwrap().state,
            ContainerState::Stopping
        );
        assert_eq!(
            grill
                .calls()
                .iter()
                .filter(|(op, _)| op == "create")
                .count(),
            creates
        );
        grill.set_fail_kill(false);
        agent.drive_pending_restarts().await;
        assert_eq!(
            agent.supervisor.get_instance(&id).unwrap().state,
            ContainerState::Stopped
        );
        assert_eq!(agent.supervisor.get_instance(&id).unwrap().restart_count, 1);
        assert_eq!(
            grill
                .calls()
                .iter()
                .filter(|(op, _)| op == "create")
                .count(),
            creates
        );
        grill.set_fail_create(false);
        grill.set_fail_start(false);
        agent.supervisor.get_instance_mut(&id).unwrap().last_restart =
            Some(Instant::now() - std::time::Duration::from_secs(600));
        agent.drive_pending_restarts().await;
        assert_eq!(
            agent.supervisor.get_instance(&id).unwrap().state,
            ContainerState::Running
        );
        assert_eq!(agent.supervisor.get_instance(&id).unwrap().restart_count, 2);
        agent.stop_app("retry", "default").await.unwrap();
    }

    #[tokio::test]
    async fn restart_create_failure_recovers_after_cleanup_and_backoff() {
        failed_restart_recovers(|grill| grill.set_fail_create(true)).await;
    }

    #[tokio::test]
    async fn restart_start_failure_recovers_after_cleanup_and_backoff() {
        failed_restart_recovers(|grill| grill.set_fail_start(true)).await;
    }

    #[tokio::test]
    async fn restart_start_failures_exhaust_job_budget() {
        let (mut agent, _, _, grill) = test_agent_with_grill();
        let directory = tempfile::tempdir().unwrap();
        agent.set_records_dir(directory.path().to_path_buf());
        grill.set_pid(std::process::id());
        expect_complete(
            &drain_deploy(
                &mut agent,
                Config::parse("[job.retry]\nimage = 'test:v1'\n").unwrap(),
            )
            .await,
        );
        let id = InstanceId("default__retry-0".into());
        grill.set_state(&id, ContainerState::Stopped);
        grill.set_exit_code(&id, Some(1));
        agent.check_jobs().await;
        grill.set_fail_start(true);
        for _ in 0..4 {
            agent.supervisor.get_instance_mut(&id).unwrap().last_restart =
                Some(Instant::now() - std::time::Duration::from_secs(600));
            agent.drive_pending_restarts().await;
        }
        let instance = agent.supervisor.get_instance(&id).unwrap();
        assert_eq!(instance.state, ContainerState::Failed);
        assert_eq!(instance.restart_count, 3);
        assert_eq!(
            grill.calls().iter().filter(|(op, _)| op == "start").count(),
            4
        );
    }

    #[tokio::test]
    async fn explicit_stop_cancels_failed_restart_recovery() {
        let (mut agent, grill, id, _directory) = failed_restart_fixture().await;
        grill.set_fail_start(true);
        agent.drive_pending_restarts().await;
        agent.stop_app("retry", "default").await.unwrap();
        assert_eq!(
            agent.supervisor.get_instance(&id).unwrap().state,
            ContainerState::Stopped
        );
        let calls = grill.calls().len();
        agent.drive_pending_restarts().await;
        assert_eq!(grill.calls().len(), calls);
    }

    #[tokio::test]
    async fn explicit_stop_cancels_pending_restart_before_creation() {
        let (mut agent, grill, id, _directory) = failed_restart_fixture().await;
        agent.stop_app("retry", "default").await.unwrap();
        assert_eq!(
            agent.supervisor.get_instance(&id).unwrap().state,
            ContainerState::Stopped
        );
        let calls = grill.calls().len();
        agent.drive_pending_restarts().await;
        assert_eq!(grill.calls().len(), calls);
    }

    #[tokio::test]
    async fn real_process_restart_recovers_when_executable_returns() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let program = directory.path().join("worker");
        let install = || {
            std::fs::write(&program, "#!/bin/sh\nexec sleep 60\n").unwrap();
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
        };
        install();
        let (_tx, rx) = mpsc::channel(32);
        let grill = crate::grill::process::ProcessGrill::new();
        let mut agent = BunAgent::new(
            grill.clone(),
            PortAllocator::new(30000, 31000),
            rx,
            CancellationToken::new(),
        );
        agent.set_volumes_dir(directory.path().join("volumes"));
        let config = Config::parse(&format!(
            "[app.retry]\nimage = 'proc-grill:ignored'\ncommand = [{:?}]\n",
            program.to_str().unwrap()
        ))
        .unwrap();
        let (events, _received) = mpsc::channel(256);
        agent.deploy(config, &events).await;
        let id = agent.supervisor.list_instances()[0].id.clone();
        grill.kill(&id).await.unwrap();
        std::fs::remove_file(&program).unwrap();
        agent.check_apps().await;
        agent.drive_pending_restarts().await;
        assert_eq!(
            agent.supervisor.get_instance(&id).unwrap().state,
            ContainerState::Stopping
        );
        agent.drive_pending_restarts().await;
        assert_eq!(
            agent.supervisor.get_instance(&id).unwrap().state,
            ContainerState::Stopped
        );
        install();
        agent.supervisor.get_instance_mut(&id).unwrap().last_restart =
            Some(Instant::now() - std::time::Duration::from_secs(600));
        agent.drive_pending_restarts().await;
        assert_eq!(grill.state(&id).await.unwrap(), ContainerState::Running);
        assert_eq!(agent.supervisor.get_instance(&id).unwrap().restart_count, 2);
        // A second crash during backoff must stay eligible for a later tick.
        grill.kill(&id).await.unwrap();
        agent.check_apps().await;
        assert_eq!(
            agent.supervisor.get_instance(&id).unwrap().state,
            ContainerState::Stopped
        );
        agent.supervisor.get_instance_mut(&id).unwrap().last_restart =
            Some(Instant::now() - std::time::Duration::from_secs(600));
        agent.drive_pending_restarts().await;
        assert_eq!(grill.state(&id).await.unwrap(), ContainerState::Running);
        assert_eq!(agent.supervisor.get_instance(&id).unwrap().restart_count, 3);
        agent.stop_app("retry", "default").await.unwrap();
    }

    #[tokio::test]
    async fn redeployed_instance_restarts_after_a_crash() {
        let (_tx, rx) = mpsc::channel(32);
        let shutdown = CancellationToken::new();
        let grill = crate::grill::process::ProcessGrill::new();
        let port_allocator = PortAllocator::new(30000, 31000);
        let volumes = tempfile::tempdir().unwrap();
        let mut agent = BunAgent::new(grill, port_allocator, rx, shutdown);
        agent.set_volumes_dir(volumes.path().to_path_buf());

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

    fn cluster_publication_fixture() -> (
        crate::onion::catalog::EndpointCatalog,
        Vec<crate::cluster::orchestrate::IngressAssignment>,
    ) {
        let config: crate::config::app::IngressSpec =
            toml::from_str("host = \"remote.local\"\ntls = \"disabled\"").unwrap();
        let catalog = crate::onion::catalog::EndpointCatalog::rebuild([(
            crate::onion::service_id::ServiceId::new("default", "remote"),
            8080,
            vec![crate::onion::catalog::CatalogBackend {
                execution: None,
                node_id: "other-node".into(),
                node_ip: "192.168.1.2".parse().unwrap(),
                host_port: 30001,
                healthy: true,
            }],
        )])
        .unwrap();
        (
            catalog,
            vec![crate::cluster::orchestrate::IngressAssignment {
                namespace: "default".into(),
                name: "remote".into(),
                config,
            }],
        )
    }

    #[tokio::test]
    async fn cluster_consumer_refuses_stale_and_rewritten_generations_without_changing_views() {
        let (mut agent, _, _) = test_agent();
        let (original, ingress) = cluster_publication_fixture();
        agent
            .publish_cluster_catalogue(4, original.clone(), ingress.clone())
            .await
            .unwrap();
        let mut changed = original.clone();
        changed
            .services
            .get_mut("default__remote")
            .unwrap()
            .backends[0]
            .host_port = 30002;
        for (generation, catalog) in [
            (3, original.clone()),
            (3, changed.clone()),
            (4, changed.clone()),
        ] {
            assert!(
                agent
                    .publish_cluster_catalogue(generation, catalog, ingress.clone())
                    .await
                    .is_err(),
                "accepted stale or rewritten generation {generation}"
            );
            assert_eq!(agent.cluster_catalog, original);
            assert_eq!(
                agent
                    .routing_table
                    .read()
                    .await
                    .lookup("remote.local", "/")
                    .unwrap()
                    .backends[0]
                    .addr
                    .port(),
                30001
            );
            assert_eq!(
                agent.service_map_tx.borrow().resolve_all()[0].backends[0].host_port,
                30001
            );
        }
        agent
            .publish_cluster_catalogue(5, changed.clone(), ingress)
            .await
            .unwrap();
        assert_eq!(agent.cluster_catalog, changed);
    }

    #[tokio::test]
    async fn cluster_consumer_advances_identical_generations_and_does_not_consume_refused_updates()
    {
        let (mut agent, _, _) = test_agent();
        let (original, ingress) = cluster_publication_fixture();
        agent
            .publish_cluster_catalogue(1, original.clone(), ingress.clone())
            .await
            .unwrap();
        agent
            .publish_cluster_catalogue(4, original.clone(), ingress.clone())
            .await
            .unwrap();
        assert!(
            agent
                .publish_cluster_catalogue(3, original.clone(), ingress.clone())
                .await
                .is_err()
        );
        let mut changed = original.clone();
        changed
            .services
            .get_mut("default__remote")
            .unwrap()
            .backends[0]
            .host_port = 30002;
        let mut invalid = ingress.clone();
        invalid[0].config.rate_limit_rps = Some(0);
        assert!(
            agent
                .publish_cluster_catalogue(6, changed.clone(), invalid)
                .await
                .is_err()
        );
        agent
            .publish_cluster_catalogue(5, changed.clone(), ingress.clone())
            .await
            .unwrap();
        agent
            .publish_cluster_catalogue(5, changed.clone(), vec![])
            .await
            .unwrap();
        assert!(
            agent
                .routing_table
                .read()
                .await
                .lookup("remote.local", "/")
                .is_none()
        );
        agent
            .publish_cluster_catalogue(5, changed, ingress)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn cluster_consumer_zero_generation_requires_an_empty_catalogue() {
        let (mut agent, _, _) = test_agent();
        let (catalog, ingress) = cluster_publication_fixture();
        assert!(
            agent
                .publish_cluster_catalogue(0, catalog, ingress)
                .await
                .is_err()
        );
        assert!(agent.cluster_catalog.is_empty());
        agent
            .publish_cluster_catalogue(0, Default::default(), vec![])
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn cluster_consumer_refuses_collisions_in_the_effective_local_and_remote_view() {
        let (mut agent, _, _) = test_agent();
        let local = crate::onion::service_id::ServiceId::new("default", "local");
        let vip = agent.service_map.register(&local, 8080, None).unwrap();
        let (mut catalog, ingress) = cluster_publication_fixture();
        catalog.services.get_mut("default__remote").unwrap().vip = vip;
        catalog.validate_allocations().unwrap();
        assert!(
            agent
                .publish_cluster_catalogue(1, catalog, ingress)
                .await
                .is_err()
        );
        assert!(agent.cluster_catalog.is_empty());
        assert!(agent.service_map_tx.borrow().resolve_all().is_empty());
        assert!(
            agent
                .routing_table
                .read()
                .await
                .lookup("remote.local", "/")
                .is_none()
        );
    }

    #[tokio::test]
    async fn cluster_publication_refusal_preserves_the_confirmed_catalogue_dns_and_ingress() {
        let (mut agent, _, _) = test_agent();
        let (original, ingress) = cluster_publication_fixture();
        let (response, reply) = oneshot::channel();
        agent
            .handle_command(AgentCommand::SyncClusterCatalog {
                generation: 1,
                response,
                catalog: Box::new(original.clone()),
                ingress: ingress.clone(),
            })
            .await;
        reply.await.unwrap().unwrap();
        let mut changed = original.clone();
        changed
            .services
            .get_mut("default__remote")
            .unwrap()
            .backends[0]
            .host_port = 30002;
        let mut invalid = ingress.clone();
        invalid[0].config.rate_limit_rps = Some(0);
        let (response, reply) = oneshot::channel();
        agent
            .handle_command(AgentCommand::SyncClusterCatalog {
                generation: 2,
                response,
                catalog: Box::new(changed.clone()),
                ingress: invalid,
            })
            .await;
        assert!(reply.await.unwrap().is_err());
        assert_eq!(
            agent.cluster_catalog, original,
            "a refused route changed the installed catalogue"
        );
        assert_eq!(
            agent
                .service_map_tx
                .borrow()
                .resolve(&crate::onion::service_id::ServiceId::new(
                    "default", "remote"
                ))
                .unwrap()
                .backends[0]
                .host_port,
            30001
        );
        let table = agent.routing_table.read().await;
        let route = table
            .lookup("remote.local", "/")
            .expect("last confirmed ingress was lost");
        assert_eq!(route.backends[0].addr.port(), 30001);
        assert!(route.rate_limit.is_none());
        drop(table);
        let (response, reply) = oneshot::channel();
        agent
            .handle_command(AgentCommand::SyncClusterCatalog {
                generation: 2,
                catalog: Box::new(changed.clone()),
                ingress,
                response,
            })
            .await;
        reply.await.unwrap().unwrap();
        assert_eq!(agent.cluster_catalog, changed);
        assert_eq!(
            agent
                .service_map_tx
                .borrow()
                .resolve(&crate::onion::service_id::ServiceId::new(
                    "default", "remote"
                ))
                .unwrap()
                .backends[0]
                .host_port,
            30002
        );
        assert_eq!(
            agent
                .routing_table
                .read()
                .await
                .lookup("remote.local", "/")
                .unwrap()
                .backends[0]
                .addr
                .port(),
            30002
        );
    }

    #[tokio::test]
    async fn cluster_publication_refuses_invalid_allocations_before_any_view_changes() {
        let (mut agent, _, _) = test_agent();
        let (mut invalid, ingress) = cluster_publication_fixture();
        invalid.services.get_mut("default__remote").unwrap().vip.0 = std::net::Ipv4Addr::LOCALHOST;
        let (response, reply) = oneshot::channel();
        agent
            .handle_command(AgentCommand::SyncClusterCatalog {
                generation: 1,
                response,
                catalog: Box::new(invalid),
                ingress,
            })
            .await;
        assert!(reply.await.unwrap().is_err());
        assert!(agent.cluster_catalog.is_empty());
        assert!(agent.service_map_tx.borrow().resolve_all().is_empty());
        assert!(
            agent
                .routing_table
                .read()
                .await
                .lookup("remote.local", "/")
                .is_none()
        );
    }

    #[tokio::test]
    async fn ingress_routes_are_installed_without_a_local_replica_and_removed_on_update() {
        let (mut agent, _tx, _shutdown) = test_agent();
        let config = Config::parse(
            r#"[app.remote]
image = "example:v1"
port = 8080
[app.remote.ingress]
host = "remote.local"
"#,
        )
        .unwrap();
        let catalog = crate::onion::catalog::EndpointCatalog::rebuild([(
            crate::onion::service_id::ServiceId::new("default", "remote"),
            8080,
            vec![crate::onion::catalog::CatalogBackend {
                execution: None,
                node_id: "other-node".into(),
                node_ip: "192.168.1.2".parse().unwrap(),
                host_port: 30001,
                healthy: true,
            }],
        )])
        .unwrap();
        let (response, reply) = oneshot::channel();
        agent
            .handle_command(AgentCommand::SyncClusterCatalog {
                generation: 1,
                response,
                catalog: Box::new(catalog.clone()),
                ingress: vec![crate::cluster::orchestrate::IngressAssignment {
                    name: "remote".into(),
                    namespace: "default".into(),
                    config: config.app["remote"].ingress.clone().unwrap(),
                }],
            })
            .await;
        reply.await.unwrap().unwrap();
        assert!(
            agent
                .routing_table
                .read()
                .await
                .lookup("remote.local", "/")
                .is_some()
        );
        assert!(agent.supervisor.list_instances().is_empty());
        let (response, reply) = oneshot::channel();
        agent
            .handle_command(AgentCommand::SyncClusterCatalog {
                generation: 1,
                response,
                catalog: Box::new(catalog),
                ingress: Vec::new(),
            })
            .await;
        reply.await.unwrap().unwrap();
        assert!(
            agent
                .routing_table
                .read()
                .await
                .lookup("remote.local", "/")
                .is_none()
        );
    }

    #[tokio::test]
    async fn local_probe_failure_does_not_restart_a_healthy_workload() {
        let (mut agent, _tx, _shutdown, _grill) = test_agent_with_grill();
        let (events, _receiver) = mpsc::channel(64);
        agent.deploy(config_with_health(), &events).await;
        let instance = agent.supervisor.list_instances()[0];
        let id = instance.id.clone();
        let created_at = instance.created_at;
        for _ in 0..3 {
            agent
                .complete_health_probe(
                    id.clone(),
                    created_at,
                    Ok(super::super::health::HealthStatus::Healthy),
                )
                .await;
        }
        assert_eq!(
            agent.supervisor.get_instance(&id).unwrap().state,
            ContainerState::Running
        );
        for _ in 0..3 {
            agent
                .complete_health_probe(
                    id.clone(),
                    created_at,
                    Err(super::super::probe::ProbeError::Client(
                        "local TLS setup failed".into(),
                    )),
                )
                .await;
        }
        let instance = agent.supervisor.get_instance(&id).unwrap();
        assert_eq!(instance.state, ContainerState::Running);
        assert_eq!(instance.health_counters.consecutive_unhealthy, 0);
        assert_eq!(instance.restart_count, 0);
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
        assert!(
            entry
                .backends
                .iter()
                .all(|backend| backend.host_port == 8080),
            "a container IP must use its declared port, not the allocated host port"
        );
    }

    #[tokio::test]
    async fn scrape_targets_name_each_running_instance_of_apps_with_metrics() {
        let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
        grill.set_container_ip(std::net::Ipv4Addr::new(10, 0, 2, 5));
        let config = Config::parse(
            r#"
            [app.web]
            image = "myapp:v1"
            port = 8080
            metrics = { port = 9797 }

            [app.quiet]
            image = "myapp:v1"
            port = 8081
            "#,
        )
        .unwrap();
        let (ev_tx, mut ev_rx) = mpsc::channel(64);
        agent.deploy(config, &ev_tx).await;
        drop(ev_tx);
        while ev_rx.recv().await.is_some() {}

        let (response, receiver) = oneshot::channel();
        agent
            .handle_command(AgentCommand::ScrapeTargets { response })
            .await;
        let targets = receiver.await.unwrap();
        assert_eq!(
            targets,
            vec![crate::mayo::scrape::AppScrapeTarget {
                app: "web".to_string(),
                namespace: "default".to_string(),
                instance: "default__web-0".to_string(),
                url: "http://10.0.2.5:9797/metrics".to_string(),
            }]
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
            label: None,
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

    // ---------------------------------------------------------------------
    // Stops whose SIGTERM is ignored wait off the command loop.
    // ---------------------------------------------------------------------

    /// The stop grace the stubborn-workload tests use: long enough that a
    /// loop blocked on it is unmistakable, short enough to keep them quick.
    const STUBBORN_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

    /// A process-runtime workload whose whole process group ignores SIGTERM,
    /// as busybox `sleep` and many shells do as PID 1. It touches
    /// `<dir>/<name>.trapped` once the trap is in place.
    fn stubborn_app(name: &str, dir: &std::path::Path) -> String {
        let trapped = dir.join(format!("{name}.trapped"));
        format!(
            "[app.{name}]\nimage = \"proc-grill:ignored\"\ncommand = [\"sh\", \"-c\", \"trap '' TERM; touch '{}'; sleep 60\"]\n",
            trapped.display()
        )
    }

    /// Deploy stubborn apps and wait until each has installed its trap, so
    /// a SIGTERM can't land before the shell gets to ignore it.
    async fn deploy_stubborn(
        tx: &mpsc::Sender<AgentCommand>,
        dir: &std::path::Path,
        names: &[&str],
    ) {
        let config: String = names.iter().map(|name| stubborn_app(name, dir)).collect();
        expect_complete(&send_deploy(tx, Config::parse(&config).unwrap()).await);
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !names
                .iter()
                .all(|name| dir.join(format!("{name}.trapped")).exists())
            {
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("stubborn workloads never installed their trap");
    }

    /// Run a process-runtime agent with `STUBBORN_GRACE`, keeping a grill
    /// handle so tests can see whether the process is really gone.
    fn stubborn_agent() -> (
        mpsc::Sender<AgentCommand>,
        CancellationToken,
        tokio::task::JoinHandle<()>,
        crate::grill::process::ProcessGrill,
        tempfile::TempDir,
    ) {
        let (tx, rx) = mpsc::channel(32);
        let shutdown = CancellationToken::new();
        let grill = crate::grill::process::ProcessGrill::new();
        let grill_handle = grill.clone();
        let port_allocator = PortAllocator::new(30000, 31000);
        let mut agent = BunAgent::new(grill, port_allocator, rx, shutdown.clone());
        let volumes = tempfile::tempdir().unwrap();
        agent.set_volumes_dir(volumes.path().to_path_buf());
        agent.set_stop_grace(STUBBORN_GRACE);
        agent.set_shutdown_grace(std::time::Duration::from_millis(200));
        let handle = tokio::spawn(async move { agent.run().await });
        (tx, shutdown, handle, grill_handle, volumes)
    }

    /// Ask for status and return it with how long the loop took to answer.
    async fn timed_status(
        tx: &mpsc::Sender<AgentCommand>,
    ) -> (Vec<InstanceStatus>, std::time::Duration) {
        let started = Instant::now();
        let (response, reply) = oneshot::channel();
        tx.send(AgentCommand::Status { response }).await.unwrap();
        let status = tokio::time::timeout(std::time::Duration::from_secs(10), reply)
            .await
            .expect("status never answered")
            .unwrap();
        (status, started.elapsed())
    }

    fn send_stop(
        tx: &mpsc::Sender<AgentCommand>,
        app_name: &str,
    ) -> oneshot::Receiver<Result<(), BunError>> {
        let (response, reply) = oneshot::channel();
        tx.try_send(AgentCommand::Stop {
            app_name: app_name.into(),
            namespace: "default".into(),
            response,
        })
        .unwrap();
        reply
    }

    /// The V02 soak's stall: a workload that ignores SIGTERM made the agent
    /// wait its whole grace inside the command loop, so `/v1/status` and the
    /// report worker timed out behind it. Status must answer at once while
    /// the stop is still waiting, and the stop must still end in SIGKILL.
    #[tokio::test]
    async fn status_answers_promptly_while_a_sigterm_ignoring_stop_waits() {
        let (tx, shutdown, handle, grill, volumes) = stubborn_agent();
        deploy_stubborn(&tx, volumes.path(), &["stubborn"]).await;
        let id = InstanceId("default__stubborn-0".into());

        let started = Instant::now();
        let stopped = send_stop(&tx, "stubborn");
        let (status, answered_in) = timed_status(&tx).await;

        assert!(
            answered_in < std::time::Duration::from_secs(1),
            "status waited {answered_in:?} behind the stop"
        );
        let instance = status.iter().find(|i| i.id == id.0).unwrap();
        assert_eq!(instance.state, ContainerState::Stopping.to_string());
        assert_eq!(grill.state(&id).await.unwrap(), ContainerState::Stopping);

        tokio::time::timeout(std::time::Duration::from_secs(15), stopped)
            .await
            .expect("stop never finished")
            .unwrap()
            .unwrap();
        assert!(
            started.elapsed() >= STUBBORN_GRACE,
            "the workload must get its full grace before SIGKILL"
        );
        assert_eq!(grill.state(&id).await.unwrap(), ContainerState::Stopped);
        let (status, _) = timed_status(&tx).await;
        let instance = status.iter().find(|i| i.id == id.0).unwrap();
        assert_eq!(instance.state, ContainerState::Stopped.to_string());

        shutdown.cancel();
        handle.await.unwrap();
    }

    /// A retirement keeps ownership (status, port) while the process lives,
    /// and releases it only once the runtime has confirmed the exit.
    #[tokio::test]
    async fn retirement_releases_ownership_only_after_the_process_exits() {
        let (tx, shutdown, handle, grill, volumes) = stubborn_agent();
        deploy_stubborn(&tx, volumes.path(), &["stubborn"]).await;
        let id = InstanceId("default__stubborn-0".into());

        let (response, retired) = oneshot::channel();
        tx.send(AgentCommand::Retire {
            app_name: "stubborn".into(),
            namespace: "default".into(),
            response,
        })
        .await
        .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        let (status, answered_in) = timed_status(&tx).await;
        assert!(answered_in < std::time::Duration::from_secs(1));
        assert!(
            status.iter().any(|i| i.id == id.0),
            "ownership was released before the process exited"
        );
        assert_eq!(grill.state(&id).await.unwrap(), ContainerState::Stopping);

        tokio::time::timeout(std::time::Duration::from_secs(15), retired)
            .await
            .expect("retirement never finished")
            .unwrap()
            .unwrap();
        assert_eq!(grill.state(&id).await.unwrap(), ContainerState::Stopped);
        let (status, _) = timed_status(&tx).await;
        assert!(status.is_empty(), "retirement must release ownership");

        shutdown.cancel();
        handle.await.unwrap();
    }

    /// Two stubborn stops overlap: together they cost one grace, not two.
    /// Serial stops take at least two graces; the 1.8 bound leaves a
    /// loaded runner room without admitting them.
    #[tokio::test]
    async fn concurrent_sigterm_ignoring_stops_overlap() {
        let (tx, shutdown, handle, grill, volumes) = stubborn_agent();
        deploy_stubborn(&tx, volumes.path(), &["first", "second"]).await;

        let started = Instant::now();
        let first = send_stop(&tx, "first");
        let second = send_stop(&tx, "second");
        for stopped in [first, second] {
            tokio::time::timeout(std::time::Duration::from_secs(15), stopped)
                .await
                .expect("stop never finished")
                .unwrap()
                .unwrap();
        }
        let elapsed = started.elapsed();

        assert!(
            elapsed < STUBBORN_GRACE * 9 / 5,
            "stops serialised: {elapsed:?} for two {STUBBORN_GRACE:?} graces"
        );
        for app in ["first", "second"] {
            let id = InstanceId(format!("default__{app}-0"));
            assert_eq!(grill.state(&id).await.unwrap(), ContainerState::Stopped);
        }

        shutdown.cancel();
        handle.await.unwrap();
    }

    /// A second stop of a workload that is already stopping joins the first
    /// rather than signalling again, and both callers learn the outcome.
    #[tokio::test]
    async fn a_second_stop_joins_the_pending_one() {
        let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
        agent.set_stop_grace(std::time::Duration::from_millis(500));
        expect_complete(&drain_deploy(&mut agent, basic_config()).await);
        grill.set_ignore_stop(true);
        grill.set_state(
            &InstanceId("default__web-0".into()),
            ContainerState::Running,
        );
        let handle = tokio::spawn(async move { agent.run().await });

        let first = send_stop(&tx, "web");
        let second = send_stop(&tx, "web");
        first.await.unwrap().unwrap();
        second.await.unwrap().unwrap();

        let id = InstanceId("default__web-0".into());
        let stops = grill
            .calls()
            .iter()
            .filter(|(op, i)| op == "stop" && i == &id)
            .count();
        assert_eq!(stops, 1, "a joined stop must not signal again");

        shutdown.cancel();
        handle.await.unwrap();
    }

    /// A retirement that arrives while an operator stop is pending joins it,
    /// then forgets ownership once the shared stop confirms the exit.
    #[tokio::test]
    async fn a_retire_joining_a_pending_stop_releases_ownership() {
        let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
        agent.set_stop_grace(std::time::Duration::from_millis(500));
        expect_complete(&drain_deploy(&mut agent, basic_config()).await);
        let id = InstanceId("default__web-0".into());
        grill.set_ignore_stop(true);
        grill.set_state(&id, ContainerState::Running);
        let handle = tokio::spawn(async move { agent.run().await });

        let stopped = send_stop(&tx, "web");
        let (response, retired) = oneshot::channel();
        tx.send(AgentCommand::Retire {
            app_name: "web".into(),
            namespace: "default".into(),
            response,
        })
        .await
        .unwrap();
        stopped.await.unwrap().unwrap();
        retired.await.unwrap().unwrap();

        let (status, _) = timed_status(&tx).await;
        assert!(
            status.is_empty(),
            "the joined retirement must release ownership"
        );
        let stops = grill
            .calls()
            .iter()
            .filter(|(op, i)| op == "stop" && i == &id)
            .count();
        assert_eq!(stops, 1, "the retirement must not signal again");

        shutdown.cancel();
        handle.await.unwrap();
    }

    /// The egress fence's stop returns before any grace passes and leaves the
    /// wait to `stop_waits`, which still ends in SIGKILL and Stopped.
    #[tokio::test]
    async fn an_unattended_stop_returns_before_its_grace() {
        let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
        agent.set_stop_grace(std::time::Duration::from_millis(500));
        expect_complete(&drain_deploy(&mut agent, basic_config()).await);
        let id = InstanceId("default__web-0".into());
        grill.set_ignore_stop(true);
        grill.set_state(&id, ContainerState::Running);

        let started = Instant::now();
        agent.stop_app_unattended("web", "default").await.unwrap();
        assert!(started.elapsed() < std::time::Duration::from_millis(400));
        assert_eq!(
            agent.supervisor.get_instance(&id).map(|i| i.state),
            Some(ContainerState::Stopping)
        );
        // A second fence joins the pending stop rather than starting another.
        agent.stop_app_unattended("web", "default").await.unwrap();
        assert_eq!(agent.stop_waits.len(), 1);

        let outcome = agent.stop_waits.join_next_with_id().await.unwrap();
        agent.complete_app_stop(outcome).await;
        assert_eq!(
            agent.supervisor.get_instance(&id).map(|i| i.state),
            Some(ContainerState::Stopped)
        );
        assert!(
            grill.calls().iter().any(|(op, i)| op == "kill" && i == &id),
            "a stubborn workload must still be force-killed"
        );
    }

    /// When a stop the egress fence relied on fails, its completion fences
    /// execution at once instead of leaving it to a later tick.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    #[tokio::test]
    async fn a_failed_stop_the_egress_fence_relies_on_fences_at_once() {
        let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
        agent.set_stop_grace(std::time::Duration::from_millis(200));
        let config = Config::parse("[app.web]\nimage = \"myapp:v1\"\n").unwrap();
        expect_complete(&drain_deploy(&mut agent, config).await);
        let id = InstanceId("default__web-0".into());
        grill.set_ignore_stop(true);
        grill.set_ignore_kill(true);
        grill.set_state(&id, ContainerState::Running);

        // An operator stop is pending when the egress fence arrives.
        let (response, stopped) = oneshot::channel();
        agent
            .request_app_stop("web".into(), "default".into(), StopPurpose::Stop, response)
            .await;
        agent.stop_app_unattended("web", "default").await.unwrap();

        let outcome = agent.stop_waits.join_next_with_id().await.unwrap();
        agent.complete_app_stop(outcome).await;

        assert!(
            stopped.await.unwrap().is_err(),
            "the stop must report its failure"
        );
        let kills = grill
            .calls()
            .iter()
            .filter(|(op, i)| op == "kill" && i == &id)
            .count();
        assert_eq!(
            kills, 2,
            "the fence must force-kill again after the failed stop"
        );
    }

    /// A deploy must not replace instances a pending stop still owns.
    #[tokio::test]
    async fn deploy_is_refused_while_the_workload_is_stopping() {
        let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
        agent.set_stop_grace(std::time::Duration::from_secs(1));
        expect_complete(&drain_deploy(&mut agent, basic_config()).await);
        grill.set_ignore_stop(true);
        grill.set_state(
            &InstanceId("default__web-0".into()),
            ContainerState::Running,
        );
        let handle = tokio::spawn(async move { agent.run().await });

        let stopped = send_stop(&tx, "web");
        let events = send_deploy(&tx, basic_config()).await;
        match events.last() {
            Some(ApplyEvent::Error { message }) => {
                assert!(message.contains("still stopping"), "{message}")
            }
            other => panic!("deploy over a pending stop was not refused: {other:?}"),
        }
        stopped.await.unwrap().unwrap();

        shutdown.cancel();
        handle.await.unwrap();
    }

    /// Shutdown doesn't wait out a pending stop's grace; the caller learns
    /// the stop is unconfirmed and keeps what it owns.
    #[tokio::test]
    async fn shutdown_reports_pending_stops_unconfirmed() {
        let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
        agent.set_stop_grace(std::time::Duration::from_secs(60));
        agent.set_shutdown_grace(std::time::Duration::from_millis(200));
        expect_complete(&drain_deploy(&mut agent, basic_config()).await);
        grill.set_ignore_stop(true);
        grill.set_state(
            &InstanceId("default__web-0".into()),
            ContainerState::Running,
        );
        let handle = tokio::spawn(async move { agent.run().await });

        let stopped = send_stop(&tx, "web");
        let _ = timed_status(&tx).await;
        shutdown.cancel();

        let result = tokio::time::timeout(std::time::Duration::from_secs(5), stopped)
            .await
            .expect("shutdown waited out the stop grace")
            .unwrap();
        assert!(
            matches!(result, Err(BunError::StopIncomplete { .. })),
            "{result:?}"
        );
        handle.await.unwrap();
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
        let volumes = tempfile::tempdir().unwrap();
        agent.set_volumes_dir(volumes.path().to_path_buf());

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
    async fn retirement_retains_ownership_until_durable_artifacts_are_removed() {
        for block_identity in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let records = root.path().join("records");
            std::fs::create_dir(&records).unwrap();
            let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
            grill.set_pid(std::process::id());
            agent.set_records_dir(records.clone());
            agent.set_volumes_dir(root.path().join("volumes"));
            let id = InstanceId("default__web-0".into());
            let identity = agent.instance_identity_dir(&id);
            let task = tokio::spawn(async move {
                agent.run().await;
                agent
            });
            expect_complete(&send_deploy(&tx, basic_config()).await);
            let record = crate::grill::records::record_path(&records, &id.0);
            if block_identity {
                crate::sesame::identity::cleanup_identity_dir(&identity).unwrap();
                std::fs::write(&identity, "blocked identity cleanup").unwrap();
                std::fs::write(&record, "owned until cleanup succeeds").unwrap();
            } else {
                std::fs::remove_file(&record).unwrap();
                std::fs::create_dir(&record).unwrap();
            }
            let (response, result) = oneshot::channel();
            tx.send(AgentCommand::Retire {
                app_name: "web".into(),
                namespace: "default".into(),
                response,
            })
            .await
            .unwrap();
            let outcome = result.await.unwrap();
            let durable_owner_retained = record.exists();
            let (response, status) = oneshot::channel();
            tx.send(AgentCommand::Status { response }).await.unwrap();
            let retained = status.await.unwrap();
            // Restore the injected filesystem fault before stopping the fixture.
            if block_identity {
                std::fs::remove_file(&identity).unwrap();
            } else {
                std::fs::remove_dir(&record).unwrap();
            }
            let (response, result) = oneshot::channel();
            tx.send(AgentCommand::Retire {
                app_name: "web".into(),
                namespace: "default".into(),
                response,
            })
            .await
            .unwrap();
            let retry = result.await.unwrap();
            shutdown.cancel();
            let agent = task.await.unwrap();
            assert!(
                outcome.is_err(),
                "retirement succeeded despite failed artifact removal"
            );
            assert!(
                durable_owner_retained,
                "failed artifact cleanup discarded the adoption record"
            );
            assert_eq!(
                retained.len(),
                1,
                "uncertain cleanup lost runtime ownership"
            );
            assert!(retry.is_ok(), "{retry:?}");
            assert!(agent.supervisor.list_instances().is_empty());
            assert!(!record.exists());
        }
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
    async fn halted_rollout_keeps_healthy_replacements_in_ordinary_supervision() {
        for strategy in ["rolling", "blue-green"] {
            let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
            let records = tempfile::tempdir().unwrap();
            agent.set_records_dir(records.path().to_path_buf());
            grill.set_pid(std::process::id());
            let config =
                Config::parse("[app.web]\nimage = 'web:v1'\nport = 8080\nreplicas = 2\n").unwrap();
            expect_complete(&drain_deploy(&mut agent, config).await);
            let failed = InstanceId("default__web-g1-1".into());
            grill.set_state(&failed, ContainerState::Failed);
            let replacement = Config::parse(&format!("[app.web]\nimage = 'web:v2'\nport = 8080\nreplicas = 2\n[app.web.deploy]\nstrategy = '{strategy}'\nauto_rollback = false\nhealth_timeout = '1s'\n")).unwrap();
            let outcome = drain_deploy(&mut agent, replacement).await;
            assert!(
                matches!(outcome.last(), Some(ApplyEvent::Error { message }) if message.contains("halted")),
                "{outcome:?}"
            );
            let healthy = InstanceId("default__web-g1-0".into());
            let owner = agent.supervisor.get_instance(&healthy).unwrap();
            assert_eq!(owner.state, ContainerState::Running);
            assert!(owner.oci_spec.is_some());
            assert!(agent.supervisor.get_instance(&failed).is_none());
            assert!(crate::grill::records::record_path(records.path(), &healthy.0).exists());
            agent.retire_workload("web", "default").await.unwrap();
            assert!(agent.supervisor.instances.is_empty());
            assert_eq!(agent.supervisor.port_allocator.allocated_count().await, 0);
            assert!(
                crate::grill::records::load_records(records.path())
                    .unwrap()
                    .is_empty()
            );
        }
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
    async fn failed_rollout_retains_every_owner_until_cleanup_is_confirmed() {
        for strategy in ["rolling", "blue-green"] {
            for auto_rollback in [true, false] {
                for fault in [
                    "kill error",
                    "kill ignored",
                    "kill stalled",
                    "inspection",
                    "record",
                    "identity",
                ] {
                    let root = tempfile::tempdir().unwrap();
                    let records = root.path().join("records");
                    let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
                    agent.set_volumes_dir(root.path().join("volumes"));
                    agent.set_records_dir(records.clone());
                    grill.set_pid(std::process::id());
                    let new_id = InstanceId("default__web-g1-0".into());
                    let record = crate::grill::records::record_path(&records, &new_id.0);
                    let identity = agent.instance_identity_dir(&new_id);
                    let history = agent.deploy_history_handle();
                    let task = tokio::spawn(async move {
                        agent.run().await;
                        agent
                    });
                    expect_complete(&send_deploy(&tx, basic_config()).await);
                    grill.set_state(&new_id, ContainerState::Failed);
                    let config = Config::parse(&format!("[app.web]\nimage = 'web:v2'\nport = 8080\n[app.web.deploy]\nstrategy = '{strategy}'\nauto_rollback = {auto_rollback}\nhealth_timeout = '30s'\n")).unwrap();
                    let (events, mut stream) = mpsc::channel(64);
                    tx.send(AgentCommand::Deploy { config, events })
                        .await
                        .unwrap();
                    let ApplyEvent::Accepted { operation_id } = stream.recv().await.unwrap() else {
                        panic!("missing operation id")
                    };
                    tokio::time::timeout(std::time::Duration::from_secs(2), async {
                        while !record.exists() {
                            tokio::task::yield_now().await;
                        }
                    })
                    .await
                    .unwrap();
                    let original_record = std::fs::read(&record).unwrap();
                    match fault {
                        "kill error" => grill.set_fail_kill(true),
                        "kill ignored" => grill.set_ignore_kill(true),
                        "kill stalled" => grill.block_kills(),
                        "inspection" => grill.set_instance_inspection_failure(&new_id, true),
                        "record" => {
                            std::fs::remove_file(&record).unwrap();
                            std::fs::create_dir(&record).unwrap();
                        }
                        _ => {
                            crate::sesame::identity::cleanup_identity_dir(&identity).unwrap();
                            std::fs::write(&identity, "blocked").unwrap();
                        }
                    }
                    let (response, cancelled) = oneshot::channel();
                    tx.send(AgentCommand::CancelDeploy {
                        operation_id: operation_id.into(),
                        response,
                    })
                    .await
                    .unwrap();
                    cancelled.await.unwrap().unwrap();
                    let outcome = tokio::time::timeout(std::time::Duration::from_secs(6), async {
                        let mut events = Vec::new();
                        while let Some(event) = stream.recv().await {
                            events.push(event);
                        }
                        events
                    })
                    .await;
                    let (response, status) = oneshot::channel();
                    tx.send(AgentCommand::Status { response }).await.unwrap();
                    let retained =
                        tokio::time::timeout(std::time::Duration::from_secs(1), status).await;
                    let record_retained = record.exists();
                    let claimed_rollback = history.read().await.iter().any(|entry| {
                        entry.result == crate::meat::deploy_types::DeployResult::RolledBack
                    });
                    grill.set_fail_kill(false);
                    grill.set_ignore_kill(false);
                    grill.set_instance_inspection_failure(&new_id, false);
                    grill.release_kills(4);
                    if fault == "record" {
                        std::fs::remove_dir(&record).unwrap();
                        std::fs::write(&record, original_record).unwrap();
                    }
                    if fault == "identity" {
                        std::fs::remove_file(&identity).unwrap();
                    }
                    grill.kill(&new_id).await.unwrap();
                    let (response, retired) = oneshot::channel();
                    tx.send(AgentCommand::Retire {
                        app_name: "web".into(),
                        namespace: "default".into(),
                        response,
                    })
                    .await
                    .unwrap();
                    let recovery = retired.await.unwrap();
                    shutdown.cancel();
                    let agent = task.await.unwrap();
                    let outcome = outcome.expect("rollback runtime cleanup must be bounded");
                    let retained = retained
                        .expect("rollback must leave the agent responsive")
                        .unwrap();
                    assert!(
                        outcome
                            .iter()
                            .any(|event| matches!(event, ApplyEvent::Error { .. }))
                    );
                    assert_eq!(
                        retained.len(),
                        2,
                        "{strategy}/{auto_rollback}/{fault} discarded a cleanup owner: {outcome:?}"
                    );
                    assert!(record_retained, "{fault} discarded adoption ownership");
                    assert!(
                        !claimed_rollback,
                        "unconfirmed cleanup was recorded as rolled back"
                    );
                    assert!(recovery.is_ok(), "{recovery:?}");
                    assert!(agent.supervisor.list_instances().is_empty());
                    assert_eq!(agent.supervisor.port_allocator.allocated_count().await, 0);
                    assert!(
                        crate::grill::records::load_records(&records)
                            .unwrap()
                            .is_empty()
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn rollout_retirement_fences_the_crash_restart_driver() {
        for (strategy, replicas) in [("rolling", 1), ("rolling", 2), ("blue-green", 2)] {
            let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
            let mut initial = basic_config();
            initial.app.get_mut("web").unwrap().replicas = crate::config::Replicas::Fixed(replicas);
            expect_complete(&drain_deploy(&mut agent, initial).await);
            grill.set_ignore_stop(true);
            grill.block_kills();
            let config = Config::parse(&format!("[app.web]\nimage = 'web:v2'\nport = 8080\nreplicas = {replicas}\n[app.web.deploy]\nstrategy = '{strategy}'\ndrain_timeout = '0s'\n")).unwrap();
            let (events, mut stream) = mpsc::channel(64);
            agent.begin_deploy(config, events, true, false).await;
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    tokio::select! {
                        Some(op) = agent.deploy_ops_rx.recv() => agent.handle_deploy_op(op).await,
                        () = grill.wait_for_kills(1) => break,
                    }
                }
            })
            .await
            .unwrap();
            let old_id = grill
                .calls()
                .into_iter()
                .rev()
                .find(|(call, _)| call == "kill")
                .unwrap()
                .1;
            // Runtime exit can become observable before its request completes.
            // Run the actual periodic restart driver at that exact boundary.
            grill.set_state(&old_id, ContainerState::Stopped);
            agent.check_apps().await;
            let old = agent.supervisor.get_instance(&old_id).unwrap();
            let observed = (old.state, old.restart_count, old.retry_pending);
            grill.release_kills(1);
            let outcome = tokio::time::timeout(std::time::Duration::from_secs(3), async {
                let mut events = Vec::new();
                loop {
                    tokio::select! {
                        Some(op) = agent.deploy_ops_rx.recv() => agent.handle_deploy_op(op).await,
                        event = stream.recv() => match event {
                            Some(event) => events.push(event),
                            None => break events,
                        }
                    }
                }
            })
            .await
            .unwrap();
            grill.set_ignore_stop(false);
            agent.stop_app("web", "default").await.unwrap();
            expect_complete(&outcome);
            assert_eq!(
                observed,
                (ContainerState::Stopping, 0, false),
                "{strategy} restarted a retiring instance"
            );
        }
    }

    #[tokio::test]
    async fn rollout_retains_old_owner_when_runtime_retirement_is_unconfirmed() {
        for strategy in ["rolling", "blue-green"] {
            for fault in [
                "kill error",
                "kill ignored",
                "inspection error",
                "kill stalled",
            ] {
                let volumes = tempfile::tempdir().unwrap();
                let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
                agent.set_volumes_dir(volumes.path().to_path_buf());
                let task = tokio::spawn(async move {
                    agent.run().await;
                    agent
                });
                expect_complete(&send_deploy(&tx, basic_config()).await);
                let old_id = InstanceId("default__web-0".into());
                grill.set_ignore_stop(true);
                grill.set_state(&old_id, ContainerState::Running);
                match fault {
                    "kill error" => grill.set_fail_kill(true),
                    "kill ignored" => grill.set_ignore_kill(true),
                    "kill stalled" => grill.block_kills(),
                    _ => grill.set_instance_inspection_failure(&old_id, true),
                }
                let config = Config::parse(&format!("[app.web]\nimage = 'web:v2'\nport = 8080\n[app.web.deploy]\nstrategy = '{strategy}'\ndrain_timeout = '0s'\nhealth_timeout = '100ms'\n")).unwrap();
                let events = tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    send_deploy(&tx, config),
                )
                .await
                .expect("runtime retirement must have a deadline");
                let (response, result) = oneshot::channel();
                tx.send(AgentCommand::Status { response }).await.unwrap();
                let retained = result.await.unwrap();
                grill.set_fail_kill(false);
                grill.set_ignore_kill(false);
                grill.set_instance_inspection_failure(&old_id, false);
                grill.set_ignore_stop(false);
                grill.release_kills(1);
                grill.kill(&old_id).await.unwrap();
                let (response, retired) = oneshot::channel();
                tx.send(AgentCommand::Retire {
                    app_name: "web".into(),
                    namespace: "default".into(),
                    response,
                })
                .await
                .unwrap();
                assert!(retired.await.unwrap().is_ok());
                let (response, status) = oneshot::channel();
                tx.send(AgentCommand::Status { response }).await.unwrap();
                assert!(
                    status.await.unwrap().is_empty(),
                    "recovery left a replacement unowned"
                );
                shutdown.cancel();
                task.await.unwrap();
                assert!(
                    !events
                        .iter()
                        .any(|event| matches!(event, ApplyEvent::Complete { .. })),
                    "{strategy} accepted {fault}: {events:?}"
                );
                assert!(
                    events
                        .iter()
                        .any(|event| matches!(event, ApplyEvent::Error { .. })),
                    "missing retirement failure"
                );
                assert_eq!(
                    retained.len(),
                    2,
                    "{strategy} must retain both the old owner and the started replacement after {fault}"
                );
                assert!(
                    retained.iter().any(|instance| instance.id == old_id.0),
                    "{strategy} forgot the unconfirmed owner after {fault}"
                );
            }
        }
    }

    #[tokio::test]
    async fn rollout_retains_owners_when_artifact_retirement_fails() {
        for strategy in ["rolling", "blue-green"] {
            for block_identity in [false, true] {
                let root = tempfile::tempdir().unwrap();
                let records = root.path().join("records");
                let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
                agent.set_volumes_dir(root.path().join("volumes"));
                agent.set_records_dir(records.clone());
                grill.set_pid(std::process::id());
                let artifacts: Vec<_> = (0..2)
                    .map(|index| {
                        let id = InstanceId(format!("default__web-{index}"));
                        (
                            agent.instance_identity_dir(&id),
                            crate::grill::records::record_path(&records, &id.0),
                        )
                    })
                    .collect();
                let task = tokio::spawn(async move {
                    agent.run().await;
                    agent
                });
                let mut initial = basic_config();
                initial.app.get_mut("web").unwrap().replicas = crate::config::Replicas::Fixed(2);
                expect_complete(&send_deploy(&tx, initial).await);
                let mut original_records = Vec::new();
                // Block either possible first owner; HashMap iteration order
                // must not decide whether this exercises the whole old fleet.
                for (identity, record) in &artifacts {
                    original_records.push(std::fs::read(record).unwrap());
                    if block_identity {
                        crate::sesame::identity::cleanup_identity_dir(identity).unwrap();
                        std::fs::write(identity, "blocked identity cleanup").unwrap();
                    } else {
                        std::fs::remove_file(record).unwrap();
                        std::fs::create_dir(record).unwrap();
                    }
                }
                let config = Config::parse(&format!("[app.web]\nimage = 'web:v2'\nport = 8080\nreplicas = 2\n[app.web.deploy]\nstrategy = '{strategy}'\ndrain_timeout = '0s'\n")).unwrap();
                let events = send_deploy(&tx, config).await;
                let durable_owner_retained = artifacts.iter().all(|(_, record)| record.exists());
                let (response, status) = oneshot::channel();
                tx.send(AgentCommand::Status { response }).await.unwrap();
                let retained = status.await.unwrap();
                for ((identity, record), original_record) in artifacts.iter().zip(original_records)
                {
                    if block_identity {
                        std::fs::remove_file(identity).unwrap();
                    } else {
                        std::fs::remove_dir(record).unwrap();
                        std::fs::write(record, original_record).unwrap();
                    }
                }
                let (response, retired) = oneshot::channel();
                tx.send(AgentCommand::Retire {
                    app_name: "web".into(),
                    namespace: "default".into(),
                    response,
                })
                .await
                .unwrap();
                let recovery = retired.await.unwrap();
                shutdown.cancel();
                let agent = task.await.unwrap();
                assert!(
                    !events
                        .iter()
                        .any(|event| matches!(event, ApplyEvent::Complete { .. })),
                    "{strategy} ignored artifact failure: {events:?}"
                );
                assert!(durable_owner_retained);
                assert_eq!(
                    retained.len(),
                    if strategy == "rolling" { 3 } else { 4 },
                    "both generations need a cleanup owner"
                );
                if strategy == "blue-green" {
                    assert!(
                        retained
                            .iter()
                            .filter(|instance| !instance.id.contains("-g"))
                            .all(|instance| instance.state == "stopped"),
                        "every retired blue instance must be stopped: {retained:?}"
                    );
                }
                assert!(
                    retained
                        .iter()
                        .any(|instance| !instance.id.contains("-g") && instance.state == "stopped"),
                    "the old instance must not be eligible for crash restart"
                );
                assert!(recovery.is_ok(), "{recovery:?}");
                assert!(agent.supervisor.list_instances().is_empty());
                assert!(
                    crate::grill::records::load_records(&records)
                        .unwrap()
                        .is_empty()
                );
            }
        }
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

    /// M7 ordering guarantee: the blue-green cut-over publishes the green
    /// backends *before* draining and stopping the blue fleet. An in-flight
    /// request holds the blue drain open; while it does, the green backend
    /// must already be routable and blue must still be unstopped — and the
    /// whole wait runs on the deploy worker, so the command loop keeps
    /// answering (asserted via a Status round-trip mid-drain).
    #[tokio::test]
    async fn blue_green_publishes_green_before_stopping_blue() {
        let (agent, tx, shutdown, grill) = test_agent_with_grill();
        let drains = agent.drains_handle();
        let mut service_maps = agent.service_map_watch();
        let mut agent = agent;
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

        expect_complete(&send_deploy(&tx, bg_config()).await);
        let blue_id = InstanceId("default__web-0".to_string());

        // Simulate the proxy holding an in-flight request on blue: pre-start
        // its drain and bump the connection count. The worker's own
        // `start_drain` is a no-op on an already-draining instance, so the
        // cut-over blocks on this connection.
        drains
            .start_drain(&crate::wrapper::draining::DrainCommand {
                app_name: "web".to_string(),
                instance_id: blue_id.0.clone(),
                timeout: std::time::Duration::from_secs(30),
            })
            .await;
        drains.increment_connections(&blue_id.0).await;

        let redeploy_tx = tx.clone();
        let redeploy = tokio::spawn(async move { send_deploy(&redeploy_tx, bg_config()).await });

        // Wait until the green backend is published (the service-map watch
        // publishes on every rebuild).
        let green_id = crate::grill::InstanceIdentity::canary("default", "web", 1, 0).instance_id();
        let published = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let map = service_maps.borrow_and_update().clone();
                let has_green = map
                    .resolve_by_name("web")
                    .is_some_and(|e| e.backends.iter().any(|b| b.instance_id == green_id.0));
                if has_green {
                    return;
                }
                if service_maps.changed().await.is_err() {
                    panic!("service map channel closed before green was published");
                }
            }
        })
        .await;
        assert!(published.is_ok(), "green backend was never published");

        // Green is routable, blue's drain is still held open: blue must not
        // have been stopped or killed yet.
        assert!(
            !grill
                .calls()
                .iter()
                .any(|(op, i)| (op == "stop" || op == "kill") && i == &blue_id),
            "blue was stopped before its in-flight request drained"
        );

        // The wait runs on the deploy worker, not the command loop: the agent
        // still answers commands mid-drain.
        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(AgentCommand::Status { response: resp_tx })
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), resp_rx)
            .await
            .expect("command loop was blocked during the blue drain")
            .unwrap();

        // The request finishes; the cut-over completes and blue is stopped.
        drains.decrement_connections(&blue_id.0).await;
        let events = tokio::time::timeout(std::time::Duration::from_secs(5), redeploy)
            .await
            .expect("redeploy did not complete after the drain released")
            .unwrap();
        expect_complete(&events);
        assert!(
            grill
                .calls()
                .iter()
                .any(|(op, i)| op == "stop" && i == &blue_id),
            "blue must be stopped after the drain"
        );

        shutdown.cancel();
        agent_handle.await.unwrap();
    }

    /// A minimal HTTP responder that answers every connection with `status`,
    /// standing in for the app's health endpoint. Returns the bound port.
    /// MockGrill reports no container IP, so the deploy gate probes
    /// `127.0.0.1:{spec.port}` — binding the responder there and pointing
    /// `port` at it exercises the real `probe_health` path end to end.
    async fn spawn_health_responder(status: u16) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0u8; 1024];
                let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buf).await;
                let response = format!("HTTP/1.1 {status} X\r\nContent-Length: 0\r\n\r\n");
                let _ = tokio::io::AsyncWriteExt::write_all(&mut socket, response.as_bytes()).await;
            }
        });
        port
    }

    fn no_health_config(port: u16) -> Config {
        Config::parse(&format!(
            r#"
            [app.web]
            image = "myapp:v1"
            port = {port}
        "#
        ))
        .unwrap()
    }

    fn health_gated_config(port: u16, strategy: &str) -> Config {
        Config::parse(&format!(
            r#"
            [app.web]
            image = "myapp:v2"
            port = {port}

            [app.web.health]
            path = "/healthz"

            [app.web.deploy]
            strategy = "{strategy}"
            health_timeout = "1s"
        "#
        ))
        .unwrap()
    }

    /// M5: a replacement whose container runs but whose HTTP health check
    /// fails must NOT replace the old instance — the deploy rolls back and
    /// the old instance keeps serving. Before the gate, the rolling wait
    /// only polled `grill.state == Running` (which MockGrill always
    /// satisfies), so this exact scenario replaced a healthy v1 with a
    /// broken v2.
    #[tokio::test]
    async fn rolling_redeploy_rolls_back_when_the_probe_fails() {
        let port = spawn_health_responder(500).await;
        let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
        let agent_handle = tokio::spawn(async move {
            agent.run().await;
        });

        expect_complete(&send_deploy(&tx, no_health_config(port)).await);

        let events = send_deploy(&tx, health_gated_config(port, "rolling")).await;
        let error = events
            .iter()
            .find_map(|e| match e {
                ApplyEvent::Error { message } => Some(message.clone()),
                _ => None,
            })
            .expect("a probe-failing redeploy must produce an error event");
        assert!(
            error.contains("failed its health check"),
            "error should name the failed probe, got: {error}"
        );

        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(AgentCommand::Status { response: resp_tx })
            .await
            .unwrap();
        let ids: Vec<String> = resp_rx.await.unwrap().into_iter().map(|s| s.id).collect();
        assert_eq!(
            ids,
            vec!["default__web-0".to_string()],
            "the old instance must keep serving after the rollback"
        );
        let old_id = InstanceId("default__web-0".to_string());
        assert!(
            !grill
                .calls()
                .iter()
                .any(|(op, i)| (op == "stop" || op == "kill") && i == &old_id),
            "the old instance must never be stopped when the replacement fails its probe"
        );
        let canary = crate::grill::InstanceIdentity::canary("default", "web", 1, 0).instance_id();
        assert!(
            grill
                .calls()
                .iter()
                .any(|(op, i)| op == "kill" && i == &canary),
            "the probe-failing replacement must be torn down"
        );

        shutdown.cancel();
        agent_handle.await.unwrap();
    }

    /// The gate must not break healthy deploys: with the responder answering
    /// 200, the rolling redeploy completes through the real probe path and
    /// the replacement takes over.
    #[tokio::test]
    async fn rolling_redeploy_completes_when_the_probe_passes() {
        let port = spawn_health_responder(200).await;
        let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
        let agent_handle = tokio::spawn(async move {
            agent.run().await;
        });

        expect_complete(&send_deploy(&tx, no_health_config(port)).await);
        expect_complete(&send_deploy(&tx, health_gated_config(port, "rolling")).await);

        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(AgentCommand::Status { response: resp_tx })
            .await
            .unwrap();
        let ids: Vec<String> = resp_rx.await.unwrap().into_iter().map(|s| s.id).collect();
        let canary = crate::grill::InstanceIdentity::canary("default", "web", 1, 0).instance_id();
        assert_eq!(
            ids,
            vec![canary.0.clone()],
            "the probe-passing replacement must take over"
        );
        let old_id = InstanceId("default__web-0".to_string());
        assert!(
            grill
                .calls()
                .iter()
                .any(|(op, i)| op == "stop" && i == &old_id),
            "the old instance must be retired after the healthy replacement"
        );

        shutdown.cancel();
        agent_handle.await.unwrap();
    }

    /// M5 on the blue-green path: a green fleet that starts but fails its
    /// probe must leave blue serving untouched.
    #[tokio::test]
    async fn blue_green_rolls_back_when_green_fails_its_probe() {
        let port = spawn_health_responder(500).await;
        let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
        let agent_handle = tokio::spawn(async move {
            agent.run().await;
        });

        expect_complete(&send_deploy(&tx, no_health_config(port)).await);

        let events = send_deploy(&tx, health_gated_config(port, "blue-green")).await;
        assert!(
            matches!(events.last(), Some(ApplyEvent::Error { .. })),
            "a probe-failing green fleet must fail the deploy, got: {events:?}"
        );

        let (resp_tx, resp_rx) = oneshot::channel();
        tx.send(AgentCommand::Status { response: resp_tx })
            .await
            .unwrap();
        let ids: Vec<String> = resp_rx.await.unwrap().into_iter().map(|s| s.id).collect();
        assert_eq!(
            ids,
            vec!["default__web-0".to_string()],
            "blue must keep serving when green fails its probe"
        );
        let old_id = InstanceId("default__web-0".to_string());
        assert!(
            !grill
                .calls()
                .iter()
                .any(|(op, i)| (op == "stop" || op == "kill") && i == &old_id),
            "blue must never be stopped when green fails its probe"
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
    async fn uncertain_job_checkpoint_does_not_block_unrelated_app_retirement() {
        let records = tempfile::tempdir().unwrap();
        let (mut agent, _, _, grill) = test_agent_with_grill();
        agent.set_records_dir(records.path().to_path_buf());
        grill.set_pid(std::process::id());
        expect_complete(&drain_deploy(&mut agent, basic_config()).await);
        std::fs::create_dir(records.path().join(super::super::jobs::CHECKPOINT_FILE)).unwrap();
        let events = drain_deploy(
            &mut agent,
            Config::parse("[job.work]\nimage = 'test:v1'\n").unwrap(),
        )
        .await;
        assert!(matches!(events.last(), Some(ApplyEvent::Error { .. })));
        agent.retire_workload("web", "default").await.unwrap();
        assert!(
            agent
                .supervisor
                .get_instance(&InstanceId("default__web-0".into()))
                .is_none()
        );
        assert!(agent.job_store_uncertain);
        assert_eq!(agent.get_job_status()[0].state, "unknown");
    }

    #[tokio::test]
    async fn job_launch_refuses_uncertain_checkpoint_without_runtime_mutation() {
        let records = tempfile::tempdir().unwrap();
        std::fs::create_dir(records.path().join(super::super::jobs::CHECKPOINT_FILE)).unwrap();
        let (mut agent, _, _, grill) = test_agent_with_grill();
        agent.set_records_dir(records.path().to_path_buf());
        let config = Config::parse("[job.work]\nimage = 'test:v1'\n").unwrap();
        let events = drain_deploy(&mut agent, config.clone()).await;
        assert!(matches!(events.last(), Some(ApplyEvent::Error { .. })));
        assert!(
            !grill
                .calls()
                .iter()
                .any(|(op, _)| op == "create" || op == "start")
        );
        std::fs::remove_dir(records.path().join(super::super::jobs::CHECKPOINT_FILE)).unwrap();
        let events = drain_deploy(&mut agent, config).await;
        assert!(
            matches!(events.last(), Some(ApplyEvent::Error { message }) if message.contains("uncertain"))
        );
        assert!(!grill.calls().iter().any(|(op, _)| op == "start"));
    }

    #[tokio::test]
    async fn application_deploy_refuses_failed_adoption_record_write() {
        let records = tempfile::tempdir().unwrap();
        let (mut agent, _, _, grill) = test_agent_with_grill();
        agent.set_records_dir(records.path().to_path_buf());
        grill.set_pid(std::process::id());
        std::fs::create_dir(records.path().join("default__web-0.json")).unwrap();
        let events = drain_deploy(
            &mut agent,
            Config::parse("[app.web]\nimage = 'test:v1'\n").unwrap(),
        )
        .await;
        assert!(
            matches!(events.last(), Some(ApplyEvent::Error { .. })),
            "{events:?}"
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, ApplyEvent::Complete { .. }))
        );
    }

    #[tokio::test]
    async fn application_deploy_refuses_missing_runtime_identity_for_adoption() {
        let records = tempfile::tempdir().unwrap();
        let (mut agent, _, _, _) = test_agent_with_grill();
        agent.set_records_dir(records.path().to_path_buf());
        let events = drain_deploy(
            &mut agent,
            Config::parse("[app.web]\nimage = 'test:v1'\n").unwrap(),
        )
        .await;
        assert!(
            matches!(events.last(), Some(ApplyEvent::Error { .. })),
            "{events:?}"
        );
    }

    #[tokio::test]
    async fn job_checkpoint_failure_after_create_refuses_execution() {
        let records = tempfile::tempdir().unwrap();
        let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
        agent.set_records_dir(records.path().to_path_buf());
        grill.block_creates();
        let task = tokio::spawn(async move { agent.run().await });
        let (events, mut results) = mpsc::channel(32);
        tx.send(AgentCommand::Deploy {
            config: Config::parse("[job.work]\nimage = 'test:v1'\n").unwrap(),
            events,
        })
        .await
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), grill.wait_for_creates(1))
            .await
            .unwrap();
        let checkpoint = records.path().join(super::super::jobs::CHECKPOINT_FILE);
        std::fs::remove_file(&checkpoint).unwrap();
        std::fs::create_dir(&checkpoint).unwrap();
        grill.release_creates(1);
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let event = results.recv().await.unwrap();
                if matches!(
                    event,
                    ApplyEvent::Complete { .. } | ApplyEvent::Error { .. }
                ) {
                    break event;
                }
            }
        })
        .await
        .unwrap();
        shutdown.cancel();
        task.await.unwrap();
        assert!(matches!(event, ApplyEvent::Error { .. }), "{event:?}");
        assert!(!grill.calls().iter().any(|(op, _)| op == "start"));
    }

    #[tokio::test]
    async fn job_attempt_precedes_create_and_missing_record_stays_unknown() {
        let records = tempfile::tempdir().unwrap();
        let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
        agent.set_records_dir(records.path().to_path_buf());
        grill.block_creates();
        let task = tokio::spawn(async move { agent.run().await });
        let (events, mut results) = mpsc::channel(32);
        tx.send(AgentCommand::Deploy {
            config: Config::parse("[job.work]\nimage = 'test:v1'\n").unwrap(),
            events,
        })
        .await
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), grill.wait_for_creates(1))
            .await
            .unwrap();
        let checkpoint = super::super::jobs::load(records.path()).unwrap();
        assert_eq!(
            checkpoint["default__work-0"].phase,
            super::super::jobs::JobPhase::Preparing
        );
        assert!(
            crate::grill::records::load_records(records.path())
                .unwrap()
                .is_empty()
        );
        // A separate directory captures exactly this physical crash window.
        let crashed = tempfile::tempdir().unwrap();
        super::super::jobs::persist(crashed.path(), checkpoint).unwrap();
        for _ in 0..2 {
            let (mut replacement, _, _, runtime) = test_agent_with_grill();
            replacement.set_records_dir(crashed.path().to_path_buf());
            replacement.adopt_recorded_instances().await.unwrap();
            assert_eq!(replacement.get_job_status()[0].state, "unknown");
            replacement.drive_pending_restarts().await;
            assert!(runtime.calls().is_empty());
        }
        grill.release_creates(1);
        while let Some(event) = results.recv().await {
            if matches!(
                event,
                ApplyEvent::Complete { .. } | ApplyEvent::Error { .. }
            ) {
                break;
            }
        }
        shutdown.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn short_job_exit_without_an_adoption_record_keeps_absence_evidence() {
        for code in [Some(0), Some(1), None] {
            let records = tempfile::tempdir().unwrap();
            let (mut agent, _, _, grill) = test_agent_with_grill();
            agent.set_records_dir(records.path().to_path_buf());
            expect_complete(
                &drain_deploy(
                    &mut agent,
                    Config::parse("[job.work]\nimage = 'test:v1'\n").unwrap(),
                )
                .await,
            );
            assert!(
                crate::grill::records::load_records(records.path())
                    .unwrap()
                    .is_empty()
            );
            let id = InstanceId("default__work-0".into());
            grill.set_state(&id, ContainerState::Stopped);
            grill.set_exit_code(&id, code);
            agent.check_jobs().await;
            assert!(super::super::jobs::load(records.path()).unwrap()[&id.0].runtime_absent);
            let (mut replacement, _, _, runtime) = test_agent_with_grill();
            replacement.set_records_dir(records.path().to_path_buf());
            replacement.adopt_recorded_instances().await.unwrap();
            runtime.set_fail_state(true);
            replacement
                .retire_workload("work", "default")
                .await
                .unwrap();
            assert!(
                runtime.calls().is_empty(),
                "positive absence must not require an unadoptable runtime handle"
            );
            assert!(super::super::jobs::load(records.path()).unwrap().is_empty());
        }
    }

    /// Z6.7: runc holds an instance's lifecycle lock for its whole create,
    /// image pull included, so asking it for a creating instance's PID held
    /// the agent loop for the length of the pull. Status timed out, reports
    /// went stale, and the leader moved the node's workloads elsewhere.
    #[tokio::test]
    async fn status_does_not_ask_the_runtime_about_an_instance_being_created() {
        let (mut agent, _, _, grill) = test_agent_with_grill();
        grill.set_pid(4242);
        let mut config = basic_config();
        config.app.get_mut("web").unwrap().replicas = crate::config::types::Replicas::Fixed(2);
        expect_complete(&drain_deploy(&mut agent, config).await);
        let creating = InstanceId("default__web-1".into());
        agent.supervisor.get_instance_mut(&creating).unwrap().state = ContainerState::Preparing;

        let statuses = agent.get_status().await;
        let pid_of = |id: &str| {
            statuses
                .iter()
                .find(|status| status.id == id)
                .map(|status| status.pid)
        };
        assert_eq!(pid_of("default__web-0"), Some(Some(4242)));
        assert_eq!(pid_of("default__web-1"), Some(None));
    }

    #[tokio::test]
    async fn job_observed_exit_and_stop_survive_replacement() {
        for code in [0, 1] {
            let records = tempfile::tempdir().unwrap();
            let (mut agent, _, _, grill) = test_agent_with_grill();
            agent.set_records_dir(records.path().to_path_buf());
            grill.set_pid(std::process::id());
            expect_complete(
                &drain_deploy(
                    &mut agent,
                    Config::parse("[job.work]\nimage = 'test:v1'\n").unwrap(),
                )
                .await,
            );
            let id = InstanceId("default__work-0".into());
            grill.set_state(&id, ContainerState::Stopped);
            grill.set_exit_code(&id, Some(code));
            agent.check_jobs().await;
            let (mut replacement, _, _, runtime) = test_agent_with_grill();
            replacement.set_records_dir(records.path().to_path_buf());
            replacement.adopt_recorded_instances().await.unwrap();
            assert_eq!(replacement.get_status().await[0].exit_code, Some(code));
            assert_eq!(
                replacement
                    .supervisor
                    .get_instance(&id)
                    .unwrap()
                    .retry_pending,
                code != 0
            );
            replacement.stop_app("work", "default").await.unwrap();
            let (mut stopped, _, _, _) = test_agent_with_grill();
            stopped.set_records_dir(records.path().to_path_buf());
            stopped.adopt_recorded_instances().await.unwrap();
            assert!(!stopped.supervisor.get_instance(&id).unwrap().retry_pending);
            assert_eq!(stopped.get_job_status()[0].state, "stopped");
            assert!(!runtime.calls().iter().any(|(op, _)| op == "start"));
        }
    }

    #[tokio::test]
    async fn job_explicit_rerun_retires_old_owner_and_claims_a_new_generation() {
        let records = tempfile::tempdir().unwrap();
        let (mut agent, _, _, grill) = test_agent_with_grill();
        agent.set_records_dir(records.path().to_path_buf());
        grill.set_pid(std::process::id());
        let config = Config::parse("[job.work]\nimage = 'test:v1'\n").unwrap();
        expect_complete(&drain_deploy(&mut agent, config.clone()).await);
        let id = InstanceId("default__work-0".into());
        grill.set_state(&id, ContainerState::Stopped);
        grill.set_exit_code(&id, None);
        agent.check_jobs().await;
        let (mut replacement, _, _, runtime) = test_agent_with_grill();
        replacement.set_records_dir(records.path().to_path_buf());
        replacement.adopt_recorded_instances().await.unwrap();
        let (events, mut results) = mpsc::channel(32);
        replacement.deploy_with_rerun(config, &events, true).await;
        drop(events);
        let mut all = Vec::new();
        while let Some(event) = results.recv().await {
            all.push(event);
        }
        expect_complete(&all);
        let job = &super::super::jobs::load(records.path()).unwrap()[&id.0];
        assert_eq!(job.generation, 2);
        assert_eq!(job.restart_count, 0);
        assert_eq!(job.phase, super::super::jobs::JobPhase::Launching);
        assert_eq!(
            runtime
                .calls()
                .iter()
                .filter(|(op, _)| op == "start")
                .count(),
            1
        );
        replacement
            .retire_workload("work", "default")
            .await
            .unwrap();
        assert!(super::super::jobs::load(records.path()).unwrap().is_empty());
    }

    #[tokio::test]
    async fn job_adoption_persists_absence_before_retiring_runtime_evidence() {
        let records = tempfile::tempdir().unwrap();
        let volumes = tempfile::tempdir().unwrap();
        let (mut agent, _, _, grill) = test_agent_with_grill();
        agent.set_records_dir(records.path().to_path_buf());
        grill.set_pid(std::process::id());
        expect_complete(&drain_deploy(&mut agent, job_config()).await);
        let id = "default__migrate-0";
        let identity = crate::sesame::identity::instance_identity_dir(volumes.path(), id);
        std::fs::create_dir_all(identity.parent().unwrap()).unwrap();
        std::fs::write(&identity, b"blocked retirement").unwrap();
        let (mut replacement, _, _, _) = test_agent_with_grill();
        replacement.set_records_dir(records.path().to_path_buf());
        replacement.set_volumes_dir(volumes.path().to_path_buf());
        assert!(replacement.adopt_recorded_instances().await.is_err());
        let checkpoint = super::super::jobs::load(records.path()).unwrap();
        assert!(checkpoint[id].runtime_absent);
        assert_eq!(checkpoint[id].phase, super::super::jobs::JobPhase::Unknown);
        assert!(crate::grill::records::record_path(records.path(), id).exists());
        std::fs::remove_file(identity).unwrap();
        replacement.adopt_recorded_instances().await.unwrap();
        assert_eq!(replacement.get_job_status()[0].state, "unknown");
    }

    #[tokio::test]
    async fn job_checkpoint_rejects_corruption_before_adopting_any_runtime() {
        for contents in ["{", "{\"schema\":99,\"jobs\":[]}"] {
            let records = tempfile::tempdir().unwrap();
            std::fs::write(
                records.path().join(super::super::jobs::CHECKPOINT_FILE),
                contents,
            )
            .unwrap();
            let (mut agent, _, _, grill) = test_agent_with_grill();
            agent.set_records_dir(records.path().to_path_buf());
            assert!(agent.adopt_recorded_instances().await.is_err());
            assert!(grill.calls().is_empty());
        }
    }

    #[tokio::test]
    async fn job_checkpoint_rejects_unsafe_and_ambiguous_files() {
        for fault in ["duplicate", "budget", "symlink", "oversized"] {
            let records = tempfile::tempdir().unwrap();
            let (mut agent, _, _, grill) = test_agent_with_grill();
            agent.set_records_dir(records.path().to_path_buf());
            grill.set_pid(std::process::id());
            expect_complete(&drain_deploy(&mut agent, job_config()).await);
            let path = records.path().join(super::super::jobs::CHECKPOINT_FILE);
            let original = std::fs::read(&path).unwrap();
            match fault {
                "duplicate" | "budget" => {
                    let mut value: serde_json::Value = serde_json::from_slice(&original).unwrap();
                    if fault == "duplicate" {
                        let duplicate = value["jobs"][0].clone();
                        value["jobs"].as_array_mut().unwrap().push(duplicate);
                    } else {
                        value["jobs"][0]["restart_count"] = serde_json::json!(4);
                    }
                    std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
                }
                "symlink" => {
                    let target = records.path().join("foreign.checkpoint");
                    std::fs::rename(&path, &target).unwrap();
                    std::os::unix::fs::symlink(target, &path).unwrap();
                }
                "oversized" => std::fs::OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_len(17 * 1024 * 1024)
                    .unwrap(),
                _ => unreachable!(),
            }
            let (mut replacement, _, _, runtime) = test_agent_with_grill();
            replacement.set_records_dir(records.path().to_path_buf());
            assert!(
                replacement.adopt_recorded_instances().await.is_err(),
                "{fault}"
            );
            assert!(runtime.calls().is_empty(), "{fault}");
            assert_eq!(
                crate::grill::records::load_records(records.path())
                    .unwrap()
                    .len(),
                1
            );
        }
    }

    #[tokio::test]
    async fn job_retry_budget_survives_adoption() {
        let records = tempfile::tempdir().unwrap();
        let (mut agent, _, _, grill) = test_agent_with_grill();
        agent.set_records_dir(records.path().to_path_buf());
        grill.set_pid(std::process::id());
        let config = Config::parse("[job.work]\nimage = 'test:v1'\n").unwrap();
        expect_complete(&drain_deploy(&mut agent, config).await);
        let id = InstanceId("default__work-0".into());
        // Exercise the real retry driver so durable state must precede launch.
        agent.supervisor.get_instance_mut(&id).unwrap().state = ContainerState::Pending;
        agent
            .supervisor
            .get_instance_mut(&id)
            .unwrap()
            .restart_count = 3;
        agent.drive_pending_restarts().await;
        assert_eq!(
            agent.supervisor.get_instance(&id).unwrap().state,
            ContainerState::Running
        );
        let (mut replacement, _, _, runtime) = test_agent_with_grill();
        replacement.set_records_dir(records.path().to_path_buf());
        runtime.set_adopt_result(&id, true);
        replacement.adopt_recorded_instances().await.unwrap();
        let restored = replacement.supervisor.get_instance(&id).unwrap();
        assert_eq!(restored.restart_count, 3);
        assert_eq!(restored.restart_policy.max_restarts, Some(3));
        runtime.set_state(&id, ContainerState::Stopped);
        runtime.set_exit_code(&id, Some(1));
        replacement.check_jobs().await;
        replacement.drive_pending_restarts().await;
        assert!(!runtime.calls().iter().any(|(op, _)| op == "start"));
    }

    #[tokio::test]
    async fn job_unknown_exit_requires_explicit_rerun() {
        let records = tempfile::tempdir().unwrap();
        let (mut agent, _, _, grill) = test_agent_with_grill();
        agent.set_records_dir(records.path().to_path_buf());
        grill.set_pid(std::process::id());
        expect_complete(
            &drain_deploy(
                &mut agent,
                Config::parse("[job.work]\nimage = 'test:v1'\n").unwrap(),
            )
            .await,
        );
        let id = InstanceId("default__work-0".into());
        grill.set_state(&id, ContainerState::Stopped);
        grill.set_exit_code(&id, None);
        agent.check_jobs().await;
        assert_eq!(agent.get_job_status()[0].state, "unknown");
        assert!(!agent.supervisor.get_instance(&id).unwrap().retry_pending);
        let (mut replacement, _, _, _) = test_agent_with_grill();
        replacement.set_records_dir(records.path().to_path_buf());
        replacement.adopt_recorded_instances().await.unwrap();
        assert_eq!(replacement.get_job_status()[0].state, "unknown");
        let events = drain_deploy(
            &mut replacement,
            Config::parse("[job.work]\nimage = 'test:v1'\n").unwrap(),
        )
        .await;
        assert!(
            matches!(events.last(), Some(ApplyEvent::Error { message }) if message.contains("rerun"))
        );
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
    async fn fresh_workloads_cannot_replace_another_apps_generation_owner() {
        for kind in ["app", "job"] {
            for state in [ContainerState::Running, ContainerState::Stopped] {
                let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
                let records = tempfile::tempdir().unwrap();
                agent.set_records_dir(records.path().to_path_buf());
                grill.set_pid(std::process::id());
                for image in ["worker:v1", "worker:v2"] {
                    let config =
                        Config::parse(&format!("[app.worker]\nimage = '{image}'\nport = 8080\n"))
                            .unwrap();
                    expect_complete(&drain_deploy(&mut agent, config).await);
                }
                let id = InstanceId("default__worker-g1-0".into());
                agent.supervisor.get_instance_mut(&id).unwrap().state = state;
                grill.set_state(&id, state);
                let original_port = agent.supervisor.get_instance(&id).unwrap().host_port;
                let original_records = crate::grill::records::load_records(records.path()).unwrap();
                let original_ports = agent.supervisor.port_allocator.allocated_count().await;
                let calls_before = grill.calls().len();
                let collision =
                    Config::parse(&format!("[{kind}.worker-g1]\nimage = 'intruder:v1'\n")).unwrap();
                let events = drain_deploy(&mut agent, collision).await;
                assert!(
                    matches!(events.last(), Some(ApplyEvent::Error { .. })),
                    "{kind}/{state:?}: {events:?}"
                );
                let owner = agent.supervisor.get_instance(&id).unwrap();
                assert_eq!(owner.app_name, "worker");
                assert_eq!(owner.image, "worker:v2");
                assert_eq!(owner.host_port, original_port);
                assert_eq!(
                    agent.supervisor.port_allocator.allocated_count().await,
                    original_ports
                );
                assert_eq!(
                    crate::grill::records::load_records(records.path()).unwrap(),
                    original_records
                );
                assert!(
                    !grill.calls()[calls_before..]
                        .iter()
                        .any(|(operation, _)| matches!(
                            operation.as_str(),
                            "create" | "start" | "stop" | "kill"
                        ))
                );
            }
        }
    }

    #[tokio::test]
    async fn workload_labels_are_refused_before_runtime_mutation() {
        for (name, namespace) in [("Bad", "default"), ("web", "bad__namespace")] {
            let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
            let mut config = basic_config();
            let mut spec = config.app.remove("web").unwrap();
            spec.namespace = Some(namespace.into());
            config.app.insert(name.into(), spec);
            let handle = tokio::spawn(async move { agent.run().await });
            let events = send_deploy(&tx, config).await;
            shutdown.cancel();
            handle.await.unwrap();
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, ApplyEvent::Error { .. })),
                "invalid label was deployed: {events:?}"
            );
            assert!(
                !grill
                    .calls()
                    .iter()
                    .any(|(operation, _)| operation == "create")
            );
        }
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
    async fn initialiser_identity_cannot_replace_an_ordinary_application() {
        let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
        let task = tokio::spawn(async move { agent.run().await });
        let foreign = InstanceId("default__web-0-init-0".into());
        let reserved = InstanceId("default__web-0__init-0".into());
        let config =
            Config::parse("[app.web-0-init]\nimage = 'foreign:image'\ncommand = ['sleep', '60']\n")
                .unwrap();
        expect_complete(&send_deploy(&tx, config).await);
        grill.set_state(&reserved, ContainerState::Stopped);
        grill.set_exit_code(&reserved, Some(0));
        let events = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            send_deploy(&tx, config_with_init_container()),
        )
        .await;
        let foreign_creates = grill
            .calls()
            .iter()
            .filter(|(operation, id)| operation == "create" && id == &foreign)
            .count();
        shutdown.cancel();
        task.await.unwrap();
        expect_complete(&events.expect("initialiser reused the running application's identity"));
        assert_eq!(
            foreign_creates, 1,
            "initialiser reused an ordinary workload identity"
        );
    }

    #[tokio::test]
    async fn uncertain_initialiser_keeps_parent_retirement_pending() {
        let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
        let old = InstanceId("default__web-0-init-0".into());
        let reserved = InstanceId("default__web-0__init-0".into());
        grill.set_instance_inspection_failure(&old, true);
        grill.set_instance_inspection_failure(&reserved, true);
        let task = tokio::spawn(async move { agent.run().await });
        let events = send_deploy(&tx, config_with_init_container()).await;
        assert!(
            events
                .iter()
                .any(|event| matches!(event, ApplyEvent::Error { .. }))
        );
        let initialiser = grill
            .calls()
            .into_iter()
            .find_map(|(operation, id)| {
                (operation == "create" && id.0 != "default__web-0").then_some(id)
            })
            .unwrap();
        let (response, result) = oneshot::channel();
        tx.send(AgentCommand::Retire {
            app_name: "web".into(),
            namespace: "default".into(),
            response,
        })
        .await
        .unwrap();
        let first = result.await.unwrap();
        grill.set_instance_inspection_failure(&old, false);
        grill.set_instance_inspection_failure(&reserved, false);
        let (response, result) = oneshot::channel();
        tx.send(AgentCommand::Retire {
            app_name: "web".into(),
            namespace: "default".into(),
            response,
        })
        .await
        .unwrap();
        let second = result.await.unwrap();
        let stopped = grill.state(&initialiser).await.unwrap() == ContainerState::Stopped;
        shutdown.cancel();
        task.await.unwrap();
        assert!(
            first.is_err(),
            "parent retired without observing its initialiser"
        );
        assert!(second.is_ok(), "confirmed retry failed: {second:?}");
        assert!(
            stopped,
            "initialiser still owns execution after parent retirement"
        );
    }

    #[tokio::test]
    async fn rolling_replacement_runs_initialisers_and_refuses_main_after_init_failure() {
        for exit_code in [0, 7] {
            let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
            let task = tokio::spawn(async move { agent.run().await });
            let mut fresh = config_with_init_container();
            fresh.app.get_mut("web").unwrap().init.clear();
            expect_complete(&send_deploy(&tx, fresh).await);
            let main = InstanceId("default__web-g1-0".into());
            let init = InstanceId(format!("{}__init-0", main.0));
            grill.set_state(&init, ContainerState::Stopped);
            grill.set_exit_code(&init, Some(exit_code));
            let before = grill.calls().len();
            let events = send_deploy(&tx, config_with_init_container()).await;
            let calls = grill.calls()[before..].to_vec();
            let init_started = calls
                .iter()
                .position(|(op, id)| op == "start" && id == &init);
            let main_started = calls
                .iter()
                .position(|(op, id)| op == "start" && id == &main);
            shutdown.cancel();
            task.await.unwrap();
            assert!(
                init_started.is_some(),
                "rolling replacement skipped its initialiser"
            );
            if exit_code == 0 {
                expect_complete(&events);
                assert!(main_started.is_some() && init_started < main_started);
            } else {
                assert!(
                    events
                        .iter()
                        .any(|event| matches!(event, ApplyEvent::Error { .. }))
                );
                assert!(
                    main_started.is_none(),
                    "failed init allowed the main payload to start"
                );
                assert!(
                    !calls
                        .iter()
                        .any(|(op, id)| (op == "stop" || op == "kill") && id.0 == "default__web-0"),
                    "failed initialiser retired the original serving workload"
                );
            }
        }
    }

    #[tokio::test]
    async fn deploy_with_init_container_succeeds() {
        let (mut agent, tx, shutdown, grill) = test_agent_with_grill();

        // Pre-configure: init container exits successfully
        let init_id = InstanceId("default__web-0__init-0".to_string());
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
        let init_id = InstanceId("default__web-0__init-0".to_string());
        grill.set_state(&init_id, ContainerState::Stopped);
        grill.set_exit_code(&init_id, Some(1));

        let agent_handle = tokio::spawn(async move {
            agent.run().await;
        });

        let events = send_deploy(&tx, config_with_init_container()).await;
        let last = events.last().expect("no events");
        assert!(
            matches!(last, ApplyEvent::Error { message } if message.contains("exited with code 1")),
            "expected an Error event naming the exit code, got {last:?}"
        );

        shutdown.cancel();
        agent_handle.await.unwrap();
    }

    #[tokio::test]
    async fn failing_init_container_reports_the_runtimes_stderr() {
        let (mut agent, tx, shutdown, grill) = test_agent_with_grill();
        let init_id = InstanceId("default__web-0__init-0".to_string());
        grill.set_state(&init_id, ContainerState::Stopped);
        grill.set_exit_code(&init_id, Some(1));
        let owner = tempfile::tempdir().unwrap();
        let stem = owner.path().join("output");
        let reason = "runc run failed: container's cgroup is not empty: 1 process(es) found";
        // Enough earlier noise that only a bounded tail can reach the error.
        let noise = "EARLY-NOISE ".repeat(1_000);
        std::fs::write(
            stem.with_extension("stderr"),
            format!("{noise}\n{reason}\n"),
        )
        .unwrap();
        grill.set_log_stem(&init_id, stem);

        let agent_handle = tokio::spawn(async move {
            agent.run().await;
        });
        let events = send_deploy(&tx, config_with_init_container()).await;
        shutdown.cancel();
        agent_handle.await.unwrap();

        let message = events
            .iter()
            .find_map(|event| match event {
                ApplyEvent::Error { message } => Some(message.clone()),
                _ => None,
            })
            .expect("the failed initialiser produced no Error event");
        assert!(
            message.contains(reason),
            "the runtime's reason is missing: {message}"
        );
        assert!(
            message.contains("exited with code 1"),
            "the exit code is missing: {message}"
        );
        assert!(
            message.len() < 1_024,
            "the stderr tail is unbounded ({} bytes)",
            message.len()
        );
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
            api_address: None,
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
            schema: 2,
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
                    capabilities: None,
                    overrides: None,
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
    async fn portless_redeploy_after_adoption_never_reuses_an_owned_generation() {
        for (runtime_id, generation) in [("default__web-g1-0", 1), ("default__web-g17-0", 17)] {
            let records = tempfile::tempdir().unwrap();
            let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
            agent.set_records_dir(records.path().to_path_buf());
            let mut record = adoption_record(runtime_id, "web", false);
            record.host_port = None;
            crate::grill::records::write_record(records.path(), &record).unwrap();
            grill.set_adopt_result(&InstanceId(runtime_id.into()), true);
            grill.set_pid(std::process::id());
            assert_eq!(agent.adopt_recorded_instances().await.unwrap(), 1);
            // Generation continuity needs no service recovery or guessed VIP.
            let mut config = basic_config();
            config.app.get_mut("web").unwrap().port = None;
            let events = drain_deploy(&mut agent, config).await;
            let expected = format!("default__web-g{}-0", generation + 1);
            let created: Vec<_> = grill
                .calls()
                .into_iter()
                .filter(|(call, _)| call == "create")
                .map(|(_, id)| id.0)
                .collect();
            let persisted = crate::grill::records::load_records(records.path()).unwrap();
            agent.stop_app("web", "default").await.unwrap();
            expect_complete(&events);
            assert_eq!(
                created,
                vec![expected.clone()],
                "adoption must advance rollout identity"
            );
            assert_eq!(persisted.len(), 1);
            assert_eq!(persisted[0].instance_id, expected);
        }
    }

    #[tokio::test]
    async fn exhausted_rollout_generation_refuses_without_mutating_the_old_instance() {
        let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
        expect_complete(&drain_deploy(&mut agent, basic_config()).await);
        agent.next_deploy_gen = u64::MAX;
        let before = grill.calls().len();
        let events = drain_deploy(&mut agent, basic_config()).await;
        let calls = grill.calls();
        agent.stop_app("web", "default").await.unwrap();
        assert!(events.iter().any(|event| matches!(event, ApplyEvent::Error { message } if message.contains("generation exhausted"))), "{events:?}");
        assert_eq!(calls.len(), before);
    }

    #[tokio::test]
    async fn redeploy_does_not_overwrite_a_stopped_or_failed_cleanup_owner() {
        for state in [ContainerState::Stopped, ContainerState::Failed] {
            let records = tempfile::tempdir().unwrap();
            let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
            agent.set_records_dir(records.path().to_path_buf());
            grill.set_pid(std::process::id());
            expect_complete(&drain_deploy(&mut agent, basic_config()).await);
            let old = InstanceId("default__web-0".into());
            let path = crate::grill::records::record_path(records.path(), &old.0);
            let original = std::fs::read(&path).unwrap();
            std::fs::remove_file(&path).unwrap();
            std::fs::create_dir(&path).unwrap();
            assert!(agent.stop_app("web", "default").await.is_err());
            agent.supervisor.get_instance_mut(&old).unwrap().state = state;
            let events = drain_deploy(&mut agent, basic_config()).await;
            let owned = agent.supervisor.list_instances().len();
            let old_creates = grill
                .calls()
                .iter()
                .filter(|(call, id)| call == "create" && id == &old)
                .count();
            std::fs::remove_dir(&path).unwrap();
            std::fs::write(&path, original).unwrap();
            agent.retire_workload("web", "default").await.unwrap();
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, ApplyEvent::Complete { .. })),
                "{state}: {events:?}"
            );
            assert_eq!(owned, 2, "must retain both owners after cleanup fails");
            assert_eq!(old_creates, 1, "the original identity was created twice");
            assert_eq!(agent.supervisor.port_allocator.allocated_count().await, 0);
        }
    }

    #[tokio::test]
    async fn rolling_replacement_persists_its_launch_spec_for_adoption() {
        let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
        let records = tempfile::tempdir().unwrap();
        let volumes = tempfile::tempdir().unwrap();
        agent.set_records_dir(records.path().to_path_buf());
        agent.set_volumes_dir(volumes.path().to_path_buf());
        grill.set_pid(std::process::id());
        let (events, _receiver) = mpsc::channel(128);
        agent.deploy(basic_config(), &events).await;
        let mut replacement = basic_config();
        replacement.app.get_mut("web").unwrap().image = Some("web:v2".to_string());
        let (rolling_events, mut rolling_rx) = mpsc::channel(1);
        let mut deployment = Box::pin(agent.deploy(replacement, &rolling_events));
        let mut observed_during_rollout = false;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                tokio::select! {
                    Some(event) = rolling_rx.recv() => {
                        if let ApplyEvent::Progress { message } = event
                            && message.contains("healthy")
                        {
                            assert!(crate::grill::records::load_records(records.path()).unwrap().iter()
                                .any(|record| record.image == "web:v2"));
                            observed_during_rollout = true;
                        }
                    }
                    _ = &mut deployment => break,
                }
            }
        })
        .await
        .unwrap();
        drop(deployment);
        assert!(observed_during_rollout);
        let persisted = crate::grill::records::load_records(records.path()).unwrap();
        assert_eq!(
            persisted.len(),
            1,
            "the replacement must retain an adoption record"
        );
        assert_eq!(persisted[0].image, "web:v2");
        assert_eq!(
            persisted[0].app_spec.as_ref().unwrap().image.as_deref(),
            Some("web:v2")
        );
        let (mut restarted, _tx, _shutdown, runtime) = test_agent_with_grill();
        restarted.set_records_dir(records.path().to_path_buf());
        restarted.set_volumes_dir(volumes.path().to_path_buf());
        let id = InstanceId(persisted[0].instance_id.clone());
        runtime.set_adopt_result(&id, true);
        assert_eq!(restarted.adopt_recorded_instances().await.unwrap(), 1);
        assert!(restarted.supervisor.get_instance(&id).is_some());
        assert!(
            !runtime
                .calls()
                .iter()
                .any(|(operation, _)| operation == "create")
        );
    }

    #[tokio::test]
    async fn rolling_record_failure_retains_cleanup_ownership_until_directory_recovery() {
        for strategy in ["rolling", "blue-green"] {
            let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
            let records = tempfile::tempdir().unwrap();
            let volumes = tempfile::tempdir().unwrap();
            agent.set_records_dir(records.path().to_path_buf());
            agent.set_volumes_dir(volumes.path().to_path_buf());
            grill.set_pid(std::process::id());
            let initial = drain_deploy(&mut agent, basic_config()).await;
            assert!(matches!(initial.last(), Some(ApplyEvent::Complete { .. })));
            let allocated_before = agent.supervisor.port_allocator.allocated_count().await;
            let blocked = tempfile::NamedTempFile::new().unwrap();
            agent.set_records_dir(blocked.path().to_path_buf());
            let replacement = Config::parse(&format!(
                "[app.web]\nimage = \"web:v2\"\nport = 8080\n[app.web.deploy]\nstrategy = \"{strategy}\"\n"
            )).unwrap();
            let outcome = drain_deploy(&mut agent, replacement).await;
            assert!(outcome.iter().any(|event| matches!(event, ApplyEvent::Error { message } if message.contains("persist replacement record"))), "{outcome:?}");
            assert!(
                agent
                    .supervisor
                    .get_instance(&InstanceId("default__web-0".into()))
                    .is_some()
            );
            assert_eq!(
                agent.supervisor.port_allocator.allocated_count().await,
                allocated_before + 1
            );
            let created: Vec<_> = grill
                .calls()
                .into_iter()
                .filter(|(op, id)| op == "create" && id.0 != "default__web-0")
                .collect();
            assert_eq!(created.len(), 1);
            let owner = agent.supervisor.get_instance(&created[0].1).unwrap();
            assert_eq!(owner.state, ContainerState::Stopped);
            agent.set_records_dir(records.path().to_path_buf());
            agent
                .finish_retire_bookkeeping(&created[0].1)
                .await
                .unwrap();
            assert_eq!(
                agent.supervisor.port_allocator.allocated_count().await,
                allocated_before
            );
            assert!(agent.supervisor.get_instance(&created[0].1).is_none());
            assert!(
                agent
                    .supervisor
                    .get_instance(&InstanceId("default__web-0".into()))
                    .is_some()
            );

            assert!(
                grill
                    .calls()
                    .contains(&("kill".to_string(), created[0].1.clone()))
            );
        }
    }

    #[tokio::test]
    async fn apple_launches_persist_records_without_a_host_workload_pid() {
        let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
        let records = tempfile::tempdir().unwrap();
        let volumes = tempfile::tempdir().unwrap();
        agent.set_records_dir(records.path().to_path_buf());
        agent.set_volumes_dir(volumes.path().to_path_buf());
        grill.set_runtime_kind(crate::grill::records::RuntimeKind::Apple);
        let (events, _receiver) = mpsc::channel(128);
        agent.deploy(basic_config(), &events).await;
        assert_eq!(
            crate::grill::records::load_records(records.path())
                .unwrap()
                .len(),
            1
        );
        agent.deploy(basic_config(), &events).await;
        let saved = crate::grill::records::load_records(records.path()).unwrap();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].runtime, crate::grill::records::RuntimeKind::Apple);
        assert_eq!(saved[0].pid, std::process::id());
        assert!(saved[0].instance_id.contains("-g"));
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

        let persisted = crate::grill::records::load_records(records.path()).unwrap();
        assert_eq!(persisted.len(), 1);
        assert_eq!(persisted[0].schema, 2);
        assert_eq!(persisted[0].rootless_network, Some(rootless_network));
    }

    #[tokio::test]
    async fn adoption_refuses_conflicting_port_ownership() {
        let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
        let records = tempfile::tempdir().unwrap();
        agent.set_records_dir(records.path().to_path_buf());
        for app in ["web", "api"] {
            let record = adoption_record(&format!("default__{app}-0"), app, false);
            crate::grill::records::write_record(records.path(), &record).unwrap();
            grill.set_adopt_result(&InstanceId(record.instance_id), true);
        }
        assert!(agent.adopt_recorded_instances().await.is_err());
        assert_eq!(agent.supervisor.list_instances().len(), 1);
        assert_eq!(
            crate::grill::records::load_records(records.path())
                .unwrap()
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn adoption_refuses_a_different_runtime_without_touching_the_record() {
        let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
        let records = tempfile::tempdir().unwrap();
        let mut record = adoption_record("default__web-0", "web", false);
        record.runtime = crate::grill::records::RuntimeKind::Runc;
        crate::grill::records::write_record(records.path(), &record).unwrap();
        agent.set_records_dir(records.path().to_path_buf());
        assert!(agent.adopt_recorded_instances().await.is_err());
        assert!(grill.calls().is_empty());
        assert_eq!(
            crate::grill::records::load_records(records.path()).unwrap(),
            vec![record]
        );
    }

    #[tokio::test]
    async fn adoption_retains_dead_owner_record_until_identity_cleanup_succeeds() {
        let (mut agent, _tx, _shutdown, _grill) = test_agent_with_grill();
        let records = tempfile::tempdir().unwrap();
        let volumes = tempfile::tempdir().unwrap();
        let record = adoption_record("default__web-0", "web", false);
        crate::grill::records::write_record(records.path(), &record).unwrap();
        agent.set_records_dir(records.path().to_path_buf());
        agent.set_volumes_dir(volumes.path().to_path_buf());
        let identity =
            crate::sesame::identity::instance_identity_dir(volumes.path(), &record.instance_id);
        std::fs::create_dir_all(identity.parent().unwrap()).unwrap();
        std::fs::write(&identity, b"not a directory").unwrap();
        assert!(agent.adopt_recorded_instances().await.is_err());
        assert!(crate::grill::records::record_path(records.path(), &record.instance_id).exists());
        std::fs::remove_file(identity).unwrap();
        assert_eq!(agent.adopt_recorded_instances().await.unwrap(), 0);
        assert!(
            crate::grill::records::load_records(records.path())
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn uncertain_adoption_preserves_the_record_and_identity_for_retry() {
        let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
        let records = tempfile::tempdir().unwrap();
        let volumes = tempfile::tempdir().unwrap();
        let record = adoption_record("default__web-0", "web", false);
        crate::grill::records::write_record(records.path(), &record).unwrap();
        write_test_identity(volumes.path(), &record.instance_id);
        agent.set_records_dir(records.path().to_path_buf());
        agent.set_volumes_dir(volumes.path().to_path_buf());
        grill.set_fail_state(true);
        assert!(agent.adopt_recorded_instances().await.is_err());
        assert!(
            crate::grill::records::record_path(records.path(), &record.instance_id).exists(),
            "uncertain runtime inspection discarded durable ownership"
        );
        let identity =
            crate::sesame::identity::instance_identity_dir(volumes.path(), &record.instance_id);
        assert!(
            crate::sesame::identity::load_identity(&identity)
                .unwrap()
                .is_some(),
            "uncertain runtime inspection swept a surviving owner's identity"
        );
        grill.set_fail_state(false);
        grill.set_adopt_result(&InstanceId(record.instance_id.clone()), true);
        agent.adopt_recorded_instances().await.unwrap();
        assert!(
            agent
                .supervisor
                .get_instance(&InstanceId(record.instance_id))
                .is_some()
        );
    }

    #[tokio::test]
    async fn corrupt_adoption_record_never_sweeps_its_workload_identity() {
        let (mut agent, _tx, _shutdown, _grill) = test_agent_with_grill();
        let records = tempfile::tempdir().unwrap();
        let volumes = tempfile::tempdir().unwrap();
        let instance = "default__web-0";
        std::fs::write(
            records.path().join(format!("{instance}.json")),
            b"{incomplete",
        )
        .unwrap();
        write_test_identity(volumes.path(), instance);
        agent.set_records_dir(records.path().to_path_buf());
        agent.set_volumes_dir(volumes.path().to_path_buf());
        assert!(agent.adopt_recorded_instances().await.is_err());
        let identity = crate::sesame::identity::instance_identity_dir(volumes.path(), instance);
        assert!(
            crate::sesame::identity::load_identity(&identity)
                .unwrap()
                .is_some(),
            "unreadable ownership was incorrectly treated as absence"
        );
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

        assert_eq!(agent.adopt_recorded_instances().await.unwrap(), 1);

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
    async fn adoption_refuses_unsupported_or_inconsistent_identities_before_mutation() {
        for (instance, app, namespace, ordinal) in [
            ("web-0", "web", "default", 0),
            ("web-g9-0", "web", "default", 0),
            ("default__web-0", "other", "default", 0),
            ("default__web-0", "web", "other", 0),
            ("default__web-0", "web", "default", 1),
            ("default__Bad-0", "Bad", "default", 0),
            ("bad__namespace__web-0", "web", "bad__namespace", 0),
            ("default__web-g01-0", "web", "default", 0),
            ("payments__web-0", "web", "payments", 0),
        ] {
            let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
            let dir = tempfile::tempdir().unwrap();
            let mut record = adoption_record(instance, app, false);
            record.namespace = namespace.into();
            record.replica_index = ordinal;
            if namespace == "payments" {
                record.app_spec.as_mut().unwrap().namespace = Some("other".into());
            }
            crate::grill::records::write_record(dir.path(), &record).unwrap();
            agent.set_records_dir(dir.path().to_path_buf());
            let runtime_id = InstanceId(instance.into());
            grill.set_adopt_result(&runtime_id, true);
            let result = agent.adopt_recorded_instances().await;
            assert!(result.is_err(), "accepted {record:?}: {result:?}");
            assert!(
                grill.calls().is_empty(),
                "invalid ownership reached the runtime"
            );
            assert!(agent.supervisor.instances.is_empty());
            assert_eq!(
                crate::grill::records::load_records(dir.path()).unwrap(),
                vec![record]
            );
        }
    }

    #[tokio::test]
    async fn adoption_uses_structured_names_to_validate_generation_like_suffixes() {
        for instance in ["default__worker-g9-0", "default__worker-g9-g17-0"] {
            let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
            let dir = tempfile::tempdir().unwrap();
            let record = adoption_record(instance, "worker-g9", false);
            crate::grill::records::write_record(dir.path(), &record).unwrap();
            agent.set_records_dir(dir.path().to_path_buf());
            let id = InstanceId(instance.into());
            grill.set_adopt_result(&id, true);
            assert_eq!(agent.adopt_recorded_instances().await.unwrap(), 1);
            let owner = agent.supervisor.get_instance(&id).unwrap();
            assert_eq!(owner.app_name, "worker-g9");
            assert_eq!(owner.namespace, "default");
            assert_eq!(
                crate::grill::records::load_records(dir.path()).unwrap(),
                vec![record]
            );
        }
    }

    #[tokio::test]
    async fn startup_deletes_stale_records_and_reschedules() {
        let (mut agent, _tx, _shutdown, _grill) = test_agent_with_grill();
        let dir = tempfile::tempdir().unwrap();
        // MockGrill declines adoption by default (dead process).
        let record = adoption_record("default__web-0", "web", false);
        crate::grill::records::write_record(dir.path(), &record).unwrap();
        agent.set_records_dir(dir.path().to_path_buf());
        agent.set_volumes_dir(dir.path().join("volumes"));

        assert_eq!(agent.adopt_recorded_instances().await.unwrap(), 0);

        // The stale record is gone and nothing was seeded: the normal
        // reconcile path is free to reschedule the instance.
        assert!(
            crate::grill::records::load_records(dir.path())
                .unwrap()
                .is_empty()
        );
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

        assert_eq!(agent.adopt_recorded_instances().await.unwrap(), 1);

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

    /// PKI7: identity directories with no live owner (an instance that died
    /// while bun was down) are swept at adoption, so stale key material
    /// never lingers.
    #[tokio::test]
    async fn adoption_sweeps_orphaned_identity_dirs() {
        let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
        let records = tempfile::tempdir().unwrap();
        let volumes = tempfile::tempdir().unwrap();
        agent.set_volumes_dir(volumes.path().to_path_buf());
        agent.set_records_dir(records.path().to_path_buf());

        // A live instance's dir and a dead instance's leftovers.
        write_test_identity(volumes.path(), "default__web-0");
        let dead = volumes.path().join(".identity/old-app-0");
        std::fs::create_dir_all(&dead).unwrap();
        std::fs::write(dead.join("key.pem"), b"dead key").unwrap();

        let record = adoption_record("default__web-0", "web", false);
        crate::grill::records::write_record(records.path(), &record).unwrap();
        let id = InstanceId("default__web-0".to_string());
        grill.set_adopt_result(&id, true);

        assert_eq!(agent.adopt_recorded_instances().await.unwrap(), 1);

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
        assert_eq!(agent.adopt_recorded_instances().await.unwrap(), 1);

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
        assert_eq!(agent.adopt_recorded_instances().await.unwrap(), 1);

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

        assert_eq!(agent.adopt_recorded_instances().await.unwrap(), 0);
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
        let volumes = tempfile::tempdir().unwrap();
        agent.set_volumes_dir(volumes.path().to_path_buf());
        // Escalation, not the length of the grace, is under test here;
        // `stop_reports_stopped_after_exit_without_kill` keeps the default.
        agent.set_stop_grace(std::time::Duration::from_millis(200));

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

    /// `runc kill` on a loaded host can take seconds to answer. The default
    /// confirmation deadline must wait that out rather than report an
    /// unconfirmed stop and leave the workload owned for another retry.
    #[tokio::test(start_paused = true)]
    async fn force_kill_waits_out_a_slow_runtime_within_the_default_deadline() {
        let grill = MockGrill::new();
        let id = InstanceId("default__web-0".to_string());
        grill.set_state(&id, ContainerState::Running);
        grill.set_kill_delay(Some(std::time::Duration::from_secs(5)));

        let deadline = crate::config::node::RuntimeSection::default().stop_confirmation_timeout();
        kill_runtime_instance(&grill, &id, deadline).await.unwrap();

        assert_eq!(grill.state(&id).await.unwrap(), ContainerState::Stopped);
    }

    /// A runtime slower than the configured deadline still yields an
    /// unconfirmed stop, so ownership is kept rather than guessed away.
    #[tokio::test(start_paused = true)]
    async fn force_kill_is_unconfirmed_when_the_runtime_outlasts_the_deadline() {
        let grill = MockGrill::new();
        let id = InstanceId("default__web-0".to_string());
        grill.set_state(&id, ContainerState::Running);
        grill.set_kill_delay(Some(std::time::Duration::from_secs(5)));

        let error = kill_runtime_instance(&grill, &id, std::time::Duration::from_secs(2))
            .await
            .unwrap_err();

        assert!(
            matches!(
                error,
                BunError::StopUnconfirmed {
                    reason: "force-kill request timed out",
                    ..
                }
            ),
            "expected an unconfirmed force-kill, got {error:?}"
        );
    }

    /// The agent's kill path uses the configured deadline, not a constant:
    /// a kill that outlasts a short configured deadline is unconfirmed.
    #[tokio::test]
    async fn kill_uses_the_configured_confirmation_deadline() {
        let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
        let id = InstanceId("default__web-0".to_string());
        grill.set_state(&id, ContainerState::Running);
        grill.set_kill_delay(Some(std::time::Duration::from_millis(500)));
        agent.set_stop_confirmation_timeout(std::time::Duration::from_millis(50));

        let error = agent.kill_and_wait_for_exit(&id).await.unwrap_err();

        assert!(
            error.to_string().contains("force-kill request timed out"),
            "expected the configured deadline to expire, got {error}"
        );
    }

    /// A cooperative stop reports Stopped once the runtime confirms exit, and
    /// does not needlessly escalate to kill.
    #[tokio::test]
    async fn stop_reports_stopped_after_exit_without_kill() {
        let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
        let volumes = tempfile::tempdir().unwrap();
        agent.set_volumes_dir(volumes.path().to_path_buf());

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
        let retire = drain_and_stop_instance(
            &drains,
            &grill,
            &id,
            std::time::Duration::from_secs(30),
            std::time::Duration::from_secs(10),
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
            .expect("retire did not complete after the request drained")
            .unwrap();
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

        let retire = drain_and_stop_instance(
            &drains,
            &grill,
            &id,
            std::time::Duration::from_secs(30),
            std::time::Duration::from_secs(10),
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
            .expect("retire did not complete after the WebSocket closed")
            .unwrap();
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
    async fn dns_fault_refuses_unknown_namespace_or_instance_scope_without_recording_it() {
        let (mut agent, _tx, _shutdown) = test_agent();
        for (namespace, target_instance) in
            [(None, None), (Some("red".into()), Some("redis-0".into()))]
        {
            let (response, result) = oneshot::channel();
            agent
                .handle_command(AgentCommand::InjectFault {
                    reservation: None,
                    replica_evidence: None,
                    request: crate::smoker::types::FaultRequest {
                        fault_type: crate::smoker::types::FaultType::DnsNxdomain,
                        target_service: "redis".into(),
                        namespace,
                        target_instance,
                        target_node: None,
                        duration: std::time::Duration::from_secs(60),
                        injected_by: "test".into(),
                        reason: None,
                        include_leader: false,
                        override_safety: false,
                        acknowledged: true,
                    },
                    response,
                })
                .await;
            assert!(result.await.unwrap().is_err());
            assert_eq!(agent.fault_registry.iter().count(), 0);
        }
    }

    #[tokio::test]
    async fn workload_fault_without_a_namespace_is_refused_before_recording() {
        let (mut agent, _tx, _shutdown) = test_agent();
        let (response, result) = oneshot::channel();
        agent
            .handle_command(AgentCommand::InjectFault {
                reservation: None,
                replica_evidence: None,
                request: crate::smoker::types::FaultRequest {
                    fault_type: crate::smoker::types::FaultType::Pause,
                    target_service: "web".into(),
                    namespace: None,
                    target_instance: None,
                    target_node: None,
                    duration: std::time::Duration::from_secs(60),
                    injected_by: "test".into(),
                    reason: None,
                    include_leader: false,
                    override_safety: false,
                    acknowledged: true,
                },
                response,
            })
            .await;
        let error = result.await.unwrap().unwrap_err().to_string();
        assert!(error.contains("require a namespace"), "{error}");
        assert_eq!(agent.fault_registry.iter().count(), 0);
    }

    #[tokio::test]
    async fn dns_fault_keeps_its_namespace_and_each_owner_until_clear() {
        use crate::onion::dns::{BoundDnsResponder, DnsConfig};
        let (mut agent, _tx, shutdown) = test_agent();
        let mut map = crate::onion::service_map::ServiceMap::new();
        map.register_app("redis", "red", 6379, None).unwrap();
        map.register_app("redis", "blue", 6379, None).unwrap();
        let (_map_tx, map_rx) = tokio::sync::watch::channel(map);
        let responder = BoundDnsResponder::bind(DnsConfig {
            listen_addr: "127.0.0.1:0".parse().unwrap(),
            ..Default::default()
        })
        .await
        .unwrap();
        let address = responder.local_addr().unwrap();
        let task = tokio::spawn(responder.run(map_rx, agent.dns_faults_watch(), shutdown.clone()));
        async fn query(address: std::net::SocketAddr, name: &str) -> u8 {
            let mut packet = vec![0x12, 0x34, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0];
            for label in name.split('.') {
                packet.push(label.len() as u8);
                packet.extend_from_slice(label.as_bytes());
            }
            packet.extend_from_slice(&[0, 0, 1, 0, 1]);
            let socket = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
            socket.send_to(&packet, address).await.unwrap();
            let mut answer = [0; 1500];
            tokio::time::timeout(
                std::time::Duration::from_secs(2),
                socket.recv_from(&mut answer),
            )
            .await
            .unwrap()
            .unwrap();
            answer[3] & 0xf
        }
        let mut owners = Vec::new();
        for _ in 0..2 {
            let (response, result) = oneshot::channel();
            agent
                .handle_command(AgentCommand::InjectFault {
                    reservation: None,
                    replica_evidence: None,
                    request: crate::smoker::types::FaultRequest {
                        fault_type: crate::smoker::types::FaultType::DnsNxdomain,
                        target_service: "redis".into(),
                        namespace: Some("red".into()),
                        target_instance: None,
                        target_node: None,
                        duration: std::time::Duration::from_secs(60),
                        injected_by: "test".into(),
                        reason: None,
                        include_leader: false,
                        override_safety: false,
                        acknowledged: true,
                    },
                    response,
                })
                .await;
            owners.push(result.await.unwrap().unwrap().id);
        }
        assert_eq!(query(address, "redis.red.internal").await, 3);
        assert_eq!(query(address, "redis.blue.internal").await, 0);
        for (index, id) in owners.into_iter().enumerate() {
            let (response, result) = oneshot::channel();
            agent
                .handle_command(AgentCommand::ClearFault {
                    fault_id: id,
                    allow_workload_fault: true,
                    allow_node_fault: false,
                    allow_node_pressure: false,
                    response,
                })
                .await;
            result.await.unwrap().unwrap();
            assert_eq!(
                query(address, "redis.red.internal").await,
                if index == 0 { 3 } else { 0 }
            );
            assert_eq!(query(address, "redis.blue.internal").await, 0);
        }
        shutdown.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn node_fault_fence_reverses_before_acknowledging_and_blocks_late_activation() {
        use crate::smoker::{
            reservation::NodeFaultReservation,
            types::{FaultRequest, FaultType},
        };
        let (mut agent, gate, _) = test_cluster_fault_agent().await;
        let request = FaultRequest {
            fault_type: FaultType::NodeKill {
                kill_containers: false,
            },
            target_service: String::new(),
            namespace: None,
            target_instance: None,
            target_node: Some("node-a".into()),
            duration: std::time::Duration::from_secs(30),
            injected_by: "operator".into(),
            reason: None,
            include_leader: true,
            override_safety: true,
            acknowledged: true,
        };
        let mut grant = NodeFaultReservation {
            sequence: 1,
            boot_id: agent.node_fault_fence.boot_id.clone(),
            cleanup_after_unix_ms: 30_000,
            request: request.clone(),
        };
        assert!(agent.fence_node_fault(&grant, true).await.is_err());
        let (response, result) = oneshot::channel();
        agent
            .handle_command(AgentCommand::InjectFault {
                reservation: Some(Box::new(grant.clone())),
                replica_evidence: None,
                request: request.clone(),
                response,
            })
            .await;
        result.await.unwrap().unwrap();
        assert!(gate.is_quiesced());
        assert!(agent.fence_node_fault(&grant, true).await.is_err());
        agent.fence_node_fault(&grant, false).await.unwrap();
        assert!(!gate.is_quiesced());
        assert!(agent.fault_registry.iter().next().is_none());
        let (response, result) = oneshot::channel();
        agent
            .handle_command(AgentCommand::InjectFault {
                reservation: Some(Box::new(grant.clone())),
                replica_evidence: None,
                request: request.clone(),
                response,
            })
            .await;
        assert!(result.await.unwrap().is_err());
        assert!(!gate.is_quiesced());
        let old = grant.clone();
        grant.sequence = 2;
        let (response, result) = oneshot::channel();
        agent
            .handle_command(AgentCommand::InjectFault {
                reservation: Some(Box::new(grant.clone())),
                replica_evidence: None,
                request,
                response,
            })
            .await;
        result.await.unwrap().unwrap();
        agent.fence_node_fault(&old, false).await.unwrap();
        assert!(
            gate.is_quiesced(),
            "an old fence must not reverse a later operation"
        );
        agent.fence_node_fault(&grant, false).await.unwrap();
        assert!(!gate.is_quiesced());
    }

    #[tokio::test]
    async fn clearing_a_reserved_node_fault_reports_its_reservation_until_fenced() {
        use crate::smoker::{
            reservation::NodeFaultReservation,
            types::{FaultRequest, FaultType},
        };
        let (mut agent, gate, _) = test_cluster_fault_agent().await;
        let request = FaultRequest {
            fault_type: FaultType::NodeKill {
                kill_containers: false,
            },
            target_service: String::new(),
            namespace: None,
            target_instance: None,
            target_node: Some("node-a".into()),
            duration: std::time::Duration::from_secs(30),
            injected_by: "operator".into(),
            reason: None,
            include_leader: true,
            override_safety: true,
            acknowledged: true,
        };
        let grant = NodeFaultReservation {
            sequence: 7,
            boot_id: agent.node_fault_fence.boot_id.clone(),
            cleanup_after_unix_ms: 30_000,
            request: request.clone(),
        };
        let (response, result) = oneshot::channel();
        agent
            .handle_command(AgentCommand::InjectFault {
                reservation: Some(Box::new(grant.clone())),
                replica_evidence: None,
                request,
                response,
            })
            .await;
        let fault_id = result.await.unwrap().unwrap().id;
        let clear = async |agent: &mut BunAgent<MockGrill>| {
            let (response, result) = oneshot::channel();
            agent
                .handle_command(AgentCommand::ClearFault {
                    fault_id,
                    allow_workload_fault: false,
                    allow_node_fault: true,
                    allow_node_pressure: false,
                    response,
                })
                .await;
            result.await.unwrap().unwrap()
        };

        // The API waits on this sequence, so "cleared" can mean the cluster
        // has released the slot, not just that this node reopened its gate.
        assert_eq!(clear(&mut agent).await.reservation, Some(7));
        assert!(!gate.is_quiesced());
        assert_eq!(
            clear(&mut agent).await.reservation,
            Some(7),
            "a retried clear must keep waiting until the leader fences the grant"
        );
        agent.fence_node_fault(&grant, true).await.unwrap();
        assert_eq!(clear(&mut agent).await.reservation, None);
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
    async fn node_fault_expiry_restores_the_transport_gate() {
        let (mut agent, gate, _) = test_cluster_fault_agent().await;
        let rule = register_fault(
            &mut agent,
            crate::smoker::types::FaultType::NodeKill {
                kill_containers: false,
            },
            std::time::Duration::from_millis(1),
        );
        agent.apply_fault(&rule).await.unwrap();
        assert!(gate.is_quiesced());
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        agent.expire_faults().await;
        assert!(agent.fault_registry.is_empty());
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
    async fn service_partition_without_ebpf_is_refused_not_recorded_as_success() {
        let (mut agent, _tx, _shutdown) = test_agent();
        let rule = fault_rule(crate::smoker::types::FaultType::Partition {
            source_app: Some("web".to_string()),
        });
        let error = agent
            .apply_fault(&rule)
            .await
            .expect_err("partition must not claim success without a loaded eBPF path");
        assert!(error.contains("eBPF data path"), "{error}");
    }

    #[tokio::test]
    async fn delay_without_runc_namespaces_and_bandwidth_are_refused_honestly() {
        let (mut agent, _tx, _shutdown) = test_agent();
        let delay = fault_rule(crate::smoker::types::FaultType::Delay {
            delay_ns: 10_000_000,
            jitter_ns: 0,
            source_app: None,
        });
        let error = agent.apply_fault(&delay).await.unwrap_err();
        assert!(
            error.contains("runc runtime") || error.contains("Linux traffic control"),
            "{error}"
        );

        let bandwidth = fault_rule(crate::smoker::types::FaultType::Bandwidth {
            bytes_per_sec: 125_000,
        });
        let error = agent.apply_fault(&bandwidth).await.unwrap_err();
        assert!(error.contains("not implemented"), "{error}");
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
            namespace: Some("default".into()),
            target_instance: None,
            target_node: None,
            duration: std::time::Duration::from_secs(30),
            injected_by: "test".into(),
            reason: None,
            include_leader: false,
            override_safety: false,
            acknowledged: false,
        };
        let context = agent.build_safety_context(&request, None).await;
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

    /// Z2.1: a routed kill of the only replica this node holds is judged
    /// against the cluster-wide count the API gathered, not the local one.
    #[tokio::test]
    async fn cluster_replica_evidence_replaces_the_local_count() {
        let (mut agent, _tx, _shutdown, _grill) = test_agent_with_grill();
        let config =
            Config::parse("[app.web]\nimage = \"web:v1\"\nport = 8080\nreplicas = 1\n").unwrap();
        let (ev_tx, mut ev_rx) = mpsc::channel(64);
        agent.deploy(config, &ev_tx).await;
        drop(ev_tx);
        while ev_rx.recv().await.is_some() {}

        let request = crate::smoker::types::FaultRequest {
            fault_type: crate::smoker::types::FaultType::Kill { count: 1 },
            target_service: "web".into(),
            namespace: Some("default".into()),
            target_instance: None,
            target_node: None,
            duration: std::time::Duration::from_secs(0),
            injected_by: "test".into(),
            reason: None,
            include_leader: false,
            override_safety: false,
            acknowledged: true,
        };
        let local = agent.build_safety_context(&request, None).await;
        assert!(!crate::smoker::safety::evaluate_safety(&request, &local).approved);

        let evidence = crate::smoker::types::ReplicaEvidence {
            replicas: 3,
            faulted_replicas: 0,
        };
        let cluster = agent.build_safety_context(&request, Some(evidence)).await;
        assert_eq!(cluster.target_service_replicas, 3);
        assert!(crate::smoker::safety::evaluate_safety(&request, &cluster).approved);
    }

    #[tokio::test]
    async fn application_restart_retires_predecessor_artifacts_before_successor_create() {
        let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
        let records = tempfile::tempdir().unwrap();
        let volumes = tempfile::tempdir().unwrap();
        agent.set_records_dir(records.path().to_path_buf());
        agent.set_volumes_dir(volumes.path().to_path_buf());
        grill.set_pid(std::process::id());
        expect_complete(&drain_deploy(&mut agent, basic_config()).await);
        let id = InstanceId("default__web-0".into());
        let record = crate::grill::records::record_path(records.path(), &id.0);
        let identity = agent.instance_identity_dir(&id);
        std::fs::write(identity.join("old-generation"), b"old identity material").unwrap();
        let instance = agent.supervisor.get_instance_mut(&id).unwrap();
        instance.state = ContainerState::Pending;
        instance.restart_count = 1;
        grill.block_creates();
        let task = tokio::spawn(async move {
            agent.drive_pending_restarts().await;
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), grill.wait_for_creates(1))
            .await
            .unwrap();
        let predecessor_record_retired = !record.exists();
        let logical_identity_retained = identity.join("old-generation").exists();
        let successor_identity_prepared = identity.is_dir();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        crate::sesame::identity::cleanup_identity_dir(&identity).unwrap();
        assert!(
            predecessor_record_retired,
            "successor creation retained the predecessor adoption record"
        );
        assert!(
            logical_identity_retained,
            "automatic restart discarded the logical workload identity"
        );
        assert!(
            successor_identity_prepared,
            "successor creation has no identity mount source"
        );
    }

    #[tokio::test]
    async fn application_restart_refuses_successor_creation_until_artifact_cleanup_succeeds() {
        let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
        let records = tempfile::tempdir().unwrap();
        let volumes = tempfile::tempdir().unwrap();
        agent.set_records_dir(records.path().to_path_buf());
        agent.set_volumes_dir(volumes.path().to_path_buf());
        grill.set_pid(std::process::id());
        expect_complete(&drain_deploy(&mut agent, basic_config()).await);
        let id = InstanceId("default__web-0".into());
        let record = crate::grill::records::record_path(records.path(), &id.0);
        let identity = agent.instance_identity_dir(&id);
        let original = std::fs::read(&record).unwrap();
        std::fs::remove_file(&record).unwrap();
        std::fs::create_dir(&record).unwrap();
        let instance = agent.supervisor.get_instance_mut(&id).unwrap();
        instance.state = ContainerState::Pending;
        instance.restart_count = 1;
        agent.drive_pending_restarts().await;
        let creates = grill
            .calls()
            .iter()
            .filter(|(operation, instance)| operation == "create" && instance == &id)
            .count();
        let predecessor_retained = record.exists();
        let pending = agent.supervisor.get_instance(&id).unwrap().state == ContainerState::Pending;
        std::fs::remove_dir(&record).unwrap();
        std::fs::write(&record, original).unwrap();
        assert_eq!(
            creates, 1,
            "a successor was created despite failed artifact cleanup"
        );
        assert!(
            predecessor_retained && pending,
            "restart lost the predecessor cleanup obligation"
        );
        agent.drive_pending_restarts().await;
        assert_eq!(
            agent.supervisor.get_instance(&id).unwrap().state,
            ContainerState::Running
        );
        assert!(record.exists() && identity.is_dir());
        agent.retire_workload("web", "default").await.unwrap();
    }

    #[tokio::test]
    async fn rollout_generations_have_independent_cgroup_paths() {
        for strategy in ["rolling", "blue-green"] {
            let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
            let volumes = tempfile::tempdir().unwrap();
            agent.set_volumes_dir(volumes.path().to_path_buf());
            grill.set_pid(std::process::id());
            expect_complete(&drain_deploy(&mut agent, basic_config()).await);
            let previous = agent.supervisor.list_instances()[0]
                .oci_spec
                .as_ref()
                .unwrap()
                .linux
                .cgroups_path
                .clone();
            let replacement = Config::parse(&format!(
                "[app.web]\nimage = 'web:v2'\nport = 8080\n[app.web.deploy]\nstrategy = '{strategy}'\n"
            )).unwrap();
            expect_complete(&drain_deploy(&mut agent, replacement).await);
            let current = agent.supervisor.list_instances()[0]
                .oci_spec
                .as_ref()
                .unwrap()
                .linux
                .cgroups_path
                .clone();
            agent.retire_workload("web", "default").await.unwrap();
            assert!(previous.is_some() && current.is_some());
            assert_ne!(
                previous, current,
                "{strategy} reused its predecessor's cgroup"
            );
        }
    }
    /// Z6.7: the leader may stop waiting for a node that has been silent past
    /// its view lease. That's only safe if the node has stopped routing to
    /// other nodes by then. Its own backends are different: only this agent
    /// can release their addresses, so they keep serving through a lapse, and
    /// the agent refuses to release one while any view it published names it.
    #[tokio::test]
    async fn a_lapsed_view_lease_keeps_local_backends_until_the_leader_answers() {
        use crate::bun::consumer_owners::{ConsumerIdentity, ConsumerPhase};
        use crate::onion::service_id::ServiceId;
        let root = tempfile::tempdir().unwrap();
        let identity = ConsumerIdentity {
            node_id: crate::meat::NodeId::new("test"),
            cluster_identity: [42; 32],
        };
        let (mut agent, _, _) = test_cluster_fault_agent().await;
        agent.set_records_dir(root.path().to_owned());
        let local = InstanceId("default__web-0".into());
        let execution = crate::grill::RuntimeExecution {
            instance_id: local.clone(),
            generation: crate::grill::RuntimeGeneration::process("original"),
        };
        let spec: crate::grill::OciSpec = serde_json::from_value(serde_json::json!({
            "root": {"path": "/fixture", "readonly": true},
            "process": {"args": ["/app"], "env": [], "cwd": "/", "user": {"uid": 0, "gid": 0}},
            "mounts": [], "linux": {"namespaces": []},
        }))
        .unwrap();
        agent
            .supervisor
            .grill()
            .set_launch_inventory(vec![crate::grill::RuntimeLaunch {
                instance_id: local.clone(),
                generation: execution.generation.clone(),
                spec,
                network_reference: None,
            }])
            .await;
        let (mut catalog, ingress) = cluster_publication_fixture();
        let web = ServiceId::new("default", "web");
        let with_web = crate::onion::catalog::EndpointCatalog::rebuild(
            catalog
                .services
                .iter()
                .map(|(qualified, service)| {
                    (
                        ServiceId::parse(qualified).unwrap(),
                        service.port,
                        service.backends.clone(),
                    )
                })
                .chain([(
                    web.clone(),
                    8080,
                    vec![crate::onion::catalog::CatalogBackend {
                        execution: Some(execution),
                        node_id: "test".into(),
                        node_ip: "192.168.1.1".parse().unwrap(),
                        host_port: 30002,
                        healthy: true,
                    }],
                )]),
        )
        .unwrap();
        catalog = with_web;
        let vip = catalog.resolve(&web).unwrap().vip;
        let lease = agent.view_lease_handle();
        assert!(lease.is_valid(), "a standalone view never lapses");
        agent
            .recover_consumer_ownership(&root.path().join("discovery"), identity)
            .await
            .unwrap();
        assert!(!lease.is_valid(), "nothing routes before the first answer");
        // The instance this node runs, as adoption would register it.
        let own = agent.local_backend(&local, &web, Some("10.0.2.2".parse().unwrap()), 30002, true);
        agent.service_map = crate::onion::service_map::ServiceMap::from_snapshot(&[
            crate::onion::types::ServiceEntry {
                app_name: "web".into(),
                namespace: "default".into(),
                namespace_id: crate::onion::vip::name_to_id("default"),
                app_id: u32::from(vip.0),
                vip,
                port: 8080,
                backends: vec![own.clone()],
                firewall_allow_from: None,
            },
        ])
        .unwrap();

        let answer = |generation, response| AgentCommand::SyncClusterConsumer {
            generation,
            catalog: Box::new(catalog.clone()),
            ingress: ingress.clone(),
            withdrawals: vec![],
            requested_at_ns: crate::onion::lease::boot_clock_ns(),
            response,
        };
        let backends = |agent: &BunAgent<MockGrill>, app: &str| {
            agent
                .service_map_tx
                .borrow()
                .resolve(&ServiceId::new("default", app))
                .map(|entry| entry.backends.clone())
        };
        let (response, reply) = oneshot::channel();
        agent.handle_command(answer(1, response)).await;
        assert!(reply.await.unwrap().unwrap().published);
        assert!(lease.is_valid(), "publishing the leader's answer renews it");
        assert_eq!(backends(&agent, "web"), Some(vec![own.clone()]));
        assert_eq!(backends(&agent, "remote").unwrap().len(), 1);

        // The leader stops answering for longer than the lease.
        lease.expire();
        agent.fence_lapsed_view().await.unwrap();
        assert_eq!(
            backends(&agent, "web"),
            Some(vec![own.clone()]),
            "this node's own backend keeps serving"
        );
        assert_eq!(
            backends(&agent, "remote"),
            Some(vec![]),
            "another node's backend stops"
        );
        assert_eq!(agent.consumer_owner().unwrap().phase, ConsumerPhase::Active);
        let routes = agent.routing_table.read().await.list_routes();
        assert!(routes.iter().all(|route| route.healthy_backends == 0));
        assert!(
            matches!(
                agent.confirm_producer_release(&local).await,
                Err(BunError::ProducerReleasePending { .. })
            ),
            "a routed local address must not be released"
        );

        // A local change still reaches the local view, but never remote ones.
        agent.service_map.remove_backend(&web, &local.0).unwrap();
        agent.consumer_view_stale = true;
        agent.refresh_consumer_view().await.unwrap();
        assert_eq!(backends(&agent, "web"), Some(vec![]));
        assert_eq!(backends(&agent, "remote"), Some(vec![]));

        // The next answer, even for the same catalogue, restores the rest.
        let (response, reply) = oneshot::channel();
        agent.handle_command(answer(1, response)).await;
        assert!(reply.await.unwrap().unwrap().published);
        assert!(lease.is_valid());
        assert_eq!(agent.consumer_owner().unwrap().phase, ConsumerPhase::Active);
        assert_eq!(backends(&agent, "remote").unwrap().len(), 1);
        assert_eq!(backends(&agent, "web"), Some(vec![]));
    }

    #[tokio::test]
    async fn durable_consumer_waits_for_http_and_websocket_release_then_recovers_receipt_retry() {
        use crate::bun::consumer_owners::ConsumerIdentity;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("discovery");
        let identity = ConsumerIdentity {
            node_id: crate::meat::NodeId::new("test"),
            cluster_identity: [42; 32],
        };
        let (mut agent, _, _) = test_cluster_fault_agent().await;
        agent.set_records_dir(root.path().to_owned());
        agent.supervisor.grill().set_launch_inventory(vec![]).await;
        agent
            .recover_consumer_ownership(&path, identity.clone())
            .await
            .unwrap();
        let (catalog, ingress) = cluster_publication_fixture();
        let result = agent
            .synchronise_consumer(1, catalog.clone(), ingress.clone(), vec![])
            .await
            .unwrap();
        assert!(result.published && result.receipts.is_empty());
        let backend = agent.service_map_tx.borrow().resolve_all()[0].backends[0]
            .instance_id
            .clone();
        let http = agent
            .drains
            .capture_requests(std::slice::from_ref(&backend), false)
            .await
            .unwrap();
        let websocket = agent
            .drains
            .capture_requests(std::slice::from_ref(&backend), true)
            .await
            .unwrap();
        let instruction = crate::onion::withdrawal::EndpointWithdrawalInstruction {
            generation: 1,
            services: catalog
                .services
                .iter()
                .map(|(id, service)| {
                    (
                        id.clone(),
                        crate::onion::withdrawal::ServiceWithdrawal {
                            service: service.clone(),
                            retire_vip: true,
                        },
                    )
                })
                .collect(),
        };
        let result = agent
            .synchronise_consumer(2, Default::default(), vec![], vec![instruction.clone()])
            .await
            .unwrap();
        // The new view publishes at once; the withdrawn backend drains gracefully.
        assert!(result.published && result.receipts.is_empty());
        assert!(
            http.iter()
                .chain(&websocket)
                .all(|token| !token.is_cancelled()),
            "withdrawal cancelled requests before their drain deadline"
        );
        assert!(agent.service_map_tx.borrow().resolve_all().is_empty());
        agent.drains.decrement_connections(&backend).await;
        let result = agent
            .synchronise_consumer(2, Default::default(), vec![], vec![instruction.clone()])
            .await
            .unwrap();
        assert!(result.published && result.receipts.is_empty());
        agent.drains.decrement_connections(&backend).await;
        agent.drains.decrement_websocket(&backend).await;
        let result = agent
            .synchronise_consumer(2, Default::default(), vec![], vec![instruction.clone()])
            .await
            .unwrap();
        assert!(result.published);
        assert_eq!(result.receipts, vec![1]);
        drop(agent);
        let (mut recovered, _, _) = test_cluster_fault_agent().await;
        recovered.set_records_dir(root.path().to_owned());
        recovered
            .supervisor
            .grill()
            .set_launch_inventory(vec![])
            .await;
        recovered
            .recover_consumer_ownership(&path, identity)
            .await
            .unwrap();
        assert!(recovered.service_map_tx.borrow().resolve_all().is_empty());
        assert!(
            recovered
                .synchronise_consumer(1, catalog, ingress, vec![])
                .await
                .is_err()
        );
        let result = recovered
            .synchronise_consumer(2, Default::default(), vec![], vec![instruction])
            .await
            .unwrap();
        assert_eq!(result.receipts, vec![1]);
        let (response, reply) = oneshot::channel();
        recovered
            .handle_command(AgentCommand::SyncClusterConsumer {
                generation: 0,
                catalog: Box::default(),
                ingress: vec![],
                withdrawals: vec![],
                requested_at_ns: crate::onion::lease::boot_clock_ns(),
                response,
            })
            .await;
        let retry = reply.await.unwrap().unwrap();
        assert!(!retry.published);
        assert_eq!(retry.receipts, vec![1]);
        recovered.confirm_consumer_receipt(1).await.unwrap();
        recovered.confirm_consumer_receipt(1).await.unwrap();
        drop(recovered);
        let journal = crate::bun::discovery_owners::DiscoveryJournal::open(&path).unwrap();
        let consumer = journal.inventory().consumer.as_ref().unwrap();
        assert!(consumer.receipts.is_empty());
        assert_eq!(consumer.publications.len(), 1);
        assert_eq!(consumer.publications[0].generation, 2);
    }

    async fn fresh_discovery_agent() -> (TestAgent, tempfile::TempDir) {
        let (mut agent, _, _, _) = test_agent_with_grill();
        let root = tempfile::tempdir().unwrap();
        agent.set_records_dir(root.path().join("records"));
        agent
            .enable_fresh_discovery_ownership(&root.path().join("discovery"))
            .await
            .unwrap();
        (agent, root)
    }

    async fn discovery_journal_state(
        readiness: &crate::bun::readiness::ReadinessTracker,
    ) -> Option<crate::bun::readiness::SubsystemState> {
        readiness
            .snapshot()
            .await
            .subsystems
            .into_iter()
            .find(|subsystem| subsystem.name == "discovery:journal")
            .map(|subsystem| subsystem.state)
    }

    #[tokio::test]
    async fn failed_discovery_write_recovers_by_reopening_the_journal() {
        let (mut agent, _root) = fresh_discovery_agent().await;
        let readiness = crate::bun::readiness::ReadinessTracker::new();
        agent.set_readiness_tracker(readiness.clone());
        let service = crate::onion::service_id::ServiceId::new("default", "web");
        let DiscoveryOwnership::Ready(journal) = &mut agent.discovery_ownership else {
            panic!("fresh discovery ownership is not ready");
        };
        journal.fail_next_write();
        let map = crate::onion::service_map::ServiceMap::new();
        assert!(
            agent
                .persist_discovery_publication(&service, &map)
                .await
                .is_err()
        );
        assert_eq!(
            discovery_journal_state(&readiness).await,
            Some(crate::bun::readiness::SubsystemState::Degraded),
            "a fenced journal must be visible"
        );
        // A transient ENOSPC or EIO must not fence discovery until restart.
        agent
            .persist_discovery_publication(&service, &map)
            .await
            .unwrap();
        assert!(matches!(
            agent.discovery_ownership,
            DiscoveryOwnership::Ready(_)
        ));
        assert_eq!(
            discovery_journal_state(&readiness).await,
            Some(crate::bun::readiness::SubsystemState::Ready)
        );
    }

    #[tokio::test]
    async fn refused_discovery_update_does_not_fence_the_journal() {
        let (mut agent, _root) = fresh_discovery_agent().await;
        let service = crate::onion::service_id::ServiceId::new("system", "discovery");
        // An invalid consumer identity fails validation before any disk write.
        let refused = agent
            .update_discovery_inventory(&service, |next| {
                next.consumer = Some(crate::bun::consumer_owners::ConsumerOwnership {
                    identity: crate::bun::consumer_owners::ConsumerIdentity {
                        node_id: crate::meat::NodeId::new(""),
                        cluster_identity: [0; 32],
                    },
                    publications: vec![],
                    phase: crate::bun::consumer_owners::ConsumerPhase::Withdrawn,
                    receipts: Default::default(),
                })
            })
            .await;
        assert!(refused.is_err());
        assert!(
            matches!(agent.discovery_ownership, DiscoveryOwnership::Ready(_)),
            "a refusal that never reached disk fenced discovery"
        );
    }

    #[tokio::test]
    async fn durable_consumer_catalogue_change_keeps_captured_requests_and_view() {
        let (mut agent, _root, catalog) = clustered_allocation_fixture().await;
        let (_, ingress) = cluster_publication_fixture();
        let backend = agent.service_map_tx.borrow().resolve_all()[0].backends[0]
            .instance_id
            .clone();
        let captured = agent
            .drains
            .capture_requests(std::slice::from_ref(&backend), false)
            .await
            .unwrap();
        // A deploy anywhere in the cluster commits a new generation. This
        // node's backends are unchanged, so its requests must not notice.
        let result = agent
            .synchronise_consumer(2, catalog, ingress, vec![])
            .await
            .unwrap();
        assert!(result.published);
        assert!(
            captured.iter().all(|token| !token.is_cancelled()),
            "a catalogue change cancelled requests to an unchanged backend"
        );
        assert!(!agent.drains.is_draining(&backend).await);
        assert_eq!(
            agent.service_map_tx.borrow().resolve_all()[0].backends[0].instance_id,
            backend
        );
        agent.drains.decrement_connections(&backend).await;
    }

    #[tokio::test]
    async fn durable_consumer_history_compacts_once_a_long_capture_releases() {
        let (mut agent, _root, catalog) = clustered_allocation_fixture().await;
        let (_, ingress) = cluster_publication_fixture();
        let backend = agent.service_map_tx.borrow().resolve_all()[0].backends[0]
            .instance_id
            .clone();
        agent
            .drains
            .capture_requests(std::slice::from_ref(&backend), false)
            .await
            .unwrap();
        // The backend leaves the catalogue while a request still holds it, and
        // the cluster keeps publishing. Every change stays retained...
        let empty = crate::onion::catalog::EndpointCatalog::default();
        for generation in 2..=40 {
            let next = if generation % 2 == 0 {
                &empty
            } else {
                &catalog
            };
            let _ = agent
                .synchronise_consumer(generation, next.clone(), ingress.clone(), vec![])
                .await;
        }
        let retained = agent.consumer_owner().unwrap().publications.len();
        assert!(
            retained > 1,
            "views were forgotten while a request held one"
        );
        // ...until the request releases, and then one pass compacts them all.
        agent.drains.decrement_connections(&backend).await;
        agent
            .synchronise_consumer(41, empty, ingress, vec![])
            .await
            .unwrap();
        assert_eq!(agent.consumer_owner().unwrap().publications.len(), 1);
    }

    #[tokio::test]
    async fn durable_consumer_local_change_keeps_the_published_view() {
        let (mut agent, _root, _catalog) = clustered_allocation_fixture().await;
        let service = crate::onion::service_id::ServiceId::new("default", "remote");
        let published = agent.service_map_tx.borrow().resolve_all().len();
        assert_eq!(published, 1);
        // Health probes, restarts and replacements all publish through here.
        agent
            .publish_backend_snapshot(&service, &agent.service_map.clone())
            .await
            .unwrap();
        assert_eq!(
            agent.service_map_tx.borrow().resolve_all().len(),
            published,
            "a local change blanked DNS and ingress"
        );
        assert!(!agent.routing_table.read().await.list_routes().is_empty());
    }

    #[tokio::test]
    async fn unchanged_health_probe_does_not_rewrite_discovery_ownership() {
        use std::os::unix::fs::MetadataExt;
        let (mut agent, _, _, grill) = test_agent_with_grill();
        let root = tempfile::tempdir().unwrap();
        agent.set_records_dir(root.path().join("records"));
        agent.set_volumes_dir(root.path().join("volumes"));
        agent
            .enable_fresh_discovery_ownership(&root.path().join("discovery"))
            .await
            .unwrap();
        grill.set_pid(std::process::id());
        grill.set_container_ip("10.0.2.5".parse().unwrap());
        grill
            .set_network_reference(original_test_network_reference())
            .await;
        expect_complete(&drain_deploy(&mut agent, basic_config()).await);
        let id = agent.supervisor.list_instances()[0].id.clone();
        let journal = root.path().join("discovery").join("discovery.json");
        agent.publish_instance_health(&id).await.unwrap();
        let before = std::fs::metadata(&journal).unwrap().ino();
        agent.publish_instance_health(&id).await.unwrap();
        assert_eq!(
            std::fs::metadata(&journal).unwrap().ino(),
            before,
            "an unchanged probe result rewrote the discovery journal"
        );
    }

    #[tokio::test]
    async fn durable_consumer_refuses_changed_enrolment_before_recovery() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("discovery");
        let identity = crate::bun::consumer_owners::ConsumerIdentity {
            node_id: crate::meat::NodeId::new("test"),
            cluster_identity: [42; 32],
        };
        let (mut agent, _, _) = test_cluster_fault_agent().await;
        agent.set_records_dir(root.path().to_owned());
        agent.supervisor.grill().set_launch_inventory(vec![]).await;
        agent
            .recover_consumer_ownership(&path, identity.clone())
            .await
            .unwrap();
        let (catalog, ingress) = cluster_publication_fixture();
        agent
            .synchronise_consumer(3, catalog, ingress, vec![])
            .await
            .unwrap();
        drop(agent);
        let original = std::fs::read(path.join("discovery.json")).unwrap();
        let (mut replacement, _, _) = test_cluster_fault_agent().await;
        replacement.set_records_dir(root.path().to_owned());
        replacement
            .supervisor
            .grill()
            .set_launch_inventory(vec![])
            .await;
        let mut changed = identity;
        changed.cluster_identity = [43; 32];
        assert!(
            replacement
                .recover_consumer_ownership(&path, changed)
                .await
                .is_err()
        );
        assert_eq!(
            std::fs::read(path.join("discovery.json")).unwrap(),
            original
        );
        assert!(replacement.service_map_tx.borrow().resolve_all().is_empty());
    }
    async fn clustered_allocation_fixture() -> (
        BunAgent<MockGrill>,
        tempfile::TempDir,
        crate::onion::catalog::EndpointCatalog,
    ) {
        let (mut agent, _, _) = test_cluster_fault_agent().await;
        let root = tempfile::tempdir().unwrap();
        agent.set_records_dir(root.path().join("records"));
        agent.supervisor.grill().set_launch_inventory(vec![]).await;
        agent
            .recover_consumer_ownership(
                &root.path().join("discovery"),
                crate::bun::consumer_owners::ConsumerIdentity {
                    node_id: crate::meat::NodeId::new("test"),
                    cluster_identity: [42; 32],
                },
            )
            .await
            .unwrap();
        let (mut catalog, ingress) = cluster_publication_fixture();
        catalog.services.get_mut("default__remote").unwrap().vip =
            crate::onion::vip::VirtualIP("127.128.43.42".parse().unwrap());
        agent
            .synchronise_consumer(1, catalog.clone(), ingress, vec![])
            .await
            .unwrap();
        // As if the leader had just answered: a lapsed lease would shrink
        // the view to local backends on the next refresh.
        agent
            .renew_view_lease(crate::onion::lease::boot_clock_ns())
            .await;
        (agent, root, catalog)
    }

    #[tokio::test]
    async fn clustered_local_registration_uses_the_committed_vip() {
        let (mut agent, _root, catalog) = clustered_allocation_fixture().await;
        let (reply, result) = oneshot::channel();
        agent
            .handle_deploy_op(DeployOp::RegisterServiceApp {
                app_name: "remote".into(),
                namespace: "default".into(),
                port: 8080,
                firewall: None,
                reply,
            })
            .await;
        result.await.unwrap().unwrap();
        let service = crate::onion::service_id::ServiceId::new("default", "remote");
        assert_eq!(
            agent.service_map.resolve(&service).unwrap().vip,
            catalog.resolve(&service).unwrap().vip
        );
        let (reply, result) = oneshot::channel();
        agent
            .handle_deploy_op(DeployOp::RegisterServiceApp {
                app_name: "uncommitted".into(),
                namespace: "default".into(),
                port: 8080,
                firewall: None,
                reply,
            })
            .await;
        assert!(
            result.await.unwrap().is_err(),
            "invented an uncommitted cluster allocation"
        );
    }

    /// A `relish stop` can land mid-rollout: the council withdraws the app's
    /// allocation, the next consumer poll drops it from the committed
    /// catalogue, and only then does the rollout try to finalise. The failed
    /// finalisation must leave the local reservation in place, because the
    /// retained replacement's retirement proves withdrawal against it. Losing
    /// it made every retry fail with "original service withdrawal is unproven".
    #[tokio::test]
    async fn failed_rollout_finalisation_keeps_the_reservation_retirement_needs() {
        let (mut agent, _root, _catalog) = clustered_allocation_fixture().await;
        let service = crate::onion::service_id::ServiceId::new("default", "remote");
        let (reply, result) = oneshot::channel();
        agent
            .handle_deploy_op(DeployOp::RegisterServiceApp {
                app_name: "remote".into(),
                namespace: "default".into(),
                port: 8080,
                firewall: None,
                reply,
            })
            .await;
        result.await.unwrap().unwrap();
        // The rollout published its replacement before retiring the old one.
        let replacement = InstanceId("default__remote-g1-0".into());
        let backend = agent.local_backend(&replacement, &service, None, 30002, true);
        agent.service_map.add_backend(&service, backend).unwrap();
        agent
            .persist_discovery_publication(&service, &agent.service_map.clone())
            .await
            .unwrap();
        let reserved = agent.service_map.resolve(&service).unwrap().clone();
        agent
            .synchronise_consumer(2, Default::default(), vec![], vec![])
            .await
            .unwrap();

        let spec = Config::parse("[app.remote]\nimage = 'test:v1'\nport = 8080\n")
            .unwrap()
            .app
            .remove("remote")
            .unwrap();
        let finalised = agent
            .finalise_rolling_deploy(
                "remote",
                "default",
                &spec,
                &[],
                std::slice::from_ref(&replacement),
                &[(replacement.clone(), Some(30002))].into_iter().collect(),
                &[(replacement.clone(), None)].into_iter().collect(),
                Default::default(),
                Instant::now(),
            )
            .await;
        assert!(
            finalised.is_err(),
            "finalised against a withdrawn allocation"
        );

        let retained = agent.service_map.resolve(&service).cloned();
        assert_eq!(retained.as_ref().map(|entry| entry.vip), Some(reserved.vip));
        // Stop withdraws the replacement's backend, then retirement proves it.
        agent
            .service_map
            .remove_backend(&service, &replacement.0)
            .unwrap();
        agent.retire_discovery_service(&service).await.unwrap();
    }

    #[tokio::test]
    async fn clustered_local_allocation_retires_without_cancelling_remote_replica_requests() {
        let (mut agent, root, catalog) = clustered_allocation_fixture().await;
        // Restore the exact local reservation; the public view also has a remote replica.
        let mut entry = agent.service_map_tx.borrow().resolve_all()[0].clone();
        let backend = entry.backends[0].instance_id.clone();
        entry.backends.clear();
        agent.service_map = crate::onion::service_map::ServiceMap::from_snapshot(&[entry]).unwrap();
        let service = crate::onion::service_id::ServiceId::new("default", "remote");
        agent
            .persist_discovery_publication(&service, &agent.service_map.clone())
            .await
            .unwrap();
        let guards = agent
            .drains
            .capture_requests(std::slice::from_ref(&backend), false)
            .await
            .unwrap();
        // Only this node's reservation retires. The remote replica keeps
        // serving, so a request it captured must not notice.
        agent.retire_discovery_service(&service).await.unwrap();
        assert!(
            !guards[0].is_cancelled(),
            "retiring a local allocation cancelled a remote replica's request"
        );
        agent.drains.decrement_connections(&backend).await;
        agent.service_map.unregister(&service).unwrap();
        assert_eq!(
            agent
                .service_map_tx
                .borrow()
                .resolve(&service)
                .unwrap()
                .backends[0]
                .instance_id,
            backend
        );
        assert!(
            agent
                .synchronise_consumer(1, catalog.clone(), vec![], vec![])
                .await
                .unwrap()
                .published
        );
        assert_eq!(
            agent.service_map_tx.borrow().resolve(&service).unwrap().vip,
            catalog.resolve(&service).unwrap().vip
        );
        drop(agent);
        let journal =
            crate::bun::discovery_owners::DiscoveryJournal::open(&root.path().join("discovery"))
                .unwrap();
        assert!(journal.inventory().services.is_empty());
        assert!(journal.inventory().consumer.is_some());
    }
    #[tokio::test]
    async fn clustered_startup_retains_orphan_ports_until_api_driven_cleanup_can_finish() {
        let (mut agent, grill, root, reference) = discovery_recovery_fixture().await;
        crate::grill::records::remove_record(
            &root.path().join("records"),
            &reference.instance_id.0,
        )
        .unwrap();
        let journal =
            crate::bun::discovery_owners::DiscoveryJournal::open(&root.path().join("discovery"))
                .unwrap();
        let mut inventory = journal.inventory().clone();
        let identity = crate::bun::consumer_owners::ConsumerIdentity {
            node_id: crate::meat::NodeId::new("test"),
            cluster_identity: [42; 32],
        };
        inventory.consumer = Some(crate::bun::consumer_owners::ConsumerOwnership {
            identity: identity.clone(),
            publications: vec![],
            phase: crate::bun::consumer_owners::ConsumerPhase::Withdrawn,
            receipts: Default::default(),
        });
        drop(journal.persist(inventory).await.unwrap());
        let (mut clustered, _, _) = test_cluster_fault_agent().await;
        agent.cluster = clustered.cluster.take();
        agent
            .recover_consumer_ownership(&root.path().join("discovery"), identity)
            .await
            .unwrap();
        assert_eq!(agent.adopt_recorded_instances().await.unwrap(), 0);
        let launch = grill.launch_inventory().await.unwrap().unwrap().remove(0);
        assert!(
            agent
                .supervisor
                .port_allocator
                .is_allocated(launch.spec.port_mapping.unwrap().host_port)
                .await
        );
        assert!(
            grill
                .network_reference(&reference.instance_id)
                .await
                .unwrap()
                .is_some()
        );
        let (events, mut received) = mpsc::channel(8);
        agent
            .begin_deploy(basic_config(), events, true, false)
            .await;
        assert!(matches!(
            received.recv().await,
            Some(ApplyEvent::Error { .. })
        ));
        agent.drive_startup_retirements().await;
        assert!(agent.startup_cleanup_pending);
        assert!(
            agent
                .supervisor
                .port_allocator
                .is_allocated(launch.spec.port_mapping.unwrap().host_port)
                .await
        );
        let confirmation = serde_json::json!({"node_id": "test", "execution": {"instance_id": reference.instance_id, "generation": launch.generation}}).to_string();
        let (client, server) =
            crate::cluster::producer::test_fixture(reqwest::StatusCode::OK, confirmation).await;
        agent.set_producer_release_client(client);
        agent.drive_startup_retirements().await;
        assert!(!agent.startup_cleanup_pending);
        assert!(
            !agent
                .supervisor
                .port_allocator
                .is_allocated(launch.spec.port_mapping.unwrap().host_port)
                .await
        );
        assert!(agent.network_references.is_empty());
        assert!(agent.service_map.resolve_all().is_empty());
        server.abort();
    }

    #[tokio::test]
    async fn rootless_discovery_adoption_restores_owned_host_forward_without_container_ip() {
        let (mut agent, grill, root, reference) = discovery_recovery_fixture().await;
        let mut launches = grill.launch_inventory().await.unwrap().unwrap();
        launches[0].network_reference = None;
        grill.release_network_reference(&reference).await.unwrap();
        grill.set_launch_inventory(launches.clone()).await;
        grill.set_adopt_result(&reference.instance_id, true);
        grill.clear_container_ip();
        grill.set_rootless_network(crate::grill::records::RootlessNetworkRecord {
            api_socket: root.path().join("slirp.sock"),
            owner_pid: std::process::id(),
            owner_pid_started_at: 1,
            container_pid: std::process::id(),
            port_mapping: launches[0].spec.port_mapping,
        });
        let journal =
            crate::bun::discovery_owners::DiscoveryJournal::open(&root.path().join("discovery"))
                .unwrap();
        let mut inventory = journal.inventory().clone();
        inventory.references.clear();
        let backend = &mut inventory.services[0].entry.backends[0];
        backend.node_ip = std::net::Ipv4Addr::LOCALHOST;
        backend.host_port = launches[0].spec.port_mapping.unwrap().host_port;
        inventory.services[0].executions.insert(
            reference.instance_id.0.clone(),
            launches[0].generation.clone(),
        );
        drop(journal);
        std::fs::write(
            root.path().join("discovery/discovery.json"),
            serde_json::to_vec(&serde_json::json!({"schema": 4, "inventory": inventory})).unwrap(),
        )
        .unwrap();
        agent
            .recover_discovery_ownership(&root.path().join("discovery"))
            .await
            .unwrap();
        assert_eq!(agent.adopt_recorded_instances().await.unwrap(), 1);
        let service = crate::onion::service_id::ServiceId::new("default", "web");
        let entry = agent
            .service_map_tx
            .borrow()
            .resolve(&service)
            .unwrap()
            .clone();
        assert_eq!(entry.backends[0].node_ip, std::net::Ipv4Addr::LOCALHOST);
        assert_eq!(
            entry.backends[0].host_port,
            launches[0].spec.port_mapping.unwrap().host_port
        );
    }
}
